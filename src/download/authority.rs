use super::{
    COPY_BUFFER_BYTES, CollisionPolicy, DownloadCancel, FileIdentity, MAX_DOWNLOAD_BYTES,
    TransferCheckpoint,
};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::File,
    io::{self, Read, Seek, SeekFrom, Write},
    path::{Component, Path, PathBuf},
    sync::{Arc, Mutex},
};

mod managed_native;
mod producer_file;
mod producer_part;
pub(crate) use managed_native::{
    DownloadNativeFile, DownloadNativeStatus, prepare_retry_native_file,
};
use producer_file::ProducerFile;
pub use producer_part::{CreationJournalRequired, run_download_producer};
pub(crate) use producer_part::{
    DownloadProducerObject, ProducerWorkScope, close_read_task_handles_after_join,
    has_retained_download_objects, retained_download_profile_objects,
    retained_read_task_objects_after_join, retire_read_task_pins_for_recovery,
};

struct GrantInner {
    path: PathBuf,
    pins: Vec<File>,
    identities: Vec<FileIdentity>,
    epoch: u64,
    selected_identity: FileIdentity,
    parent: Option<DestinationGrant>,
    revoked: Arc<Mutex<bool>>,
}
#[derive(Clone)]
pub struct DestinationGrant(Arc<GrantInner>);
impl DestinationGrant {
    pub(crate) fn same_native_owner(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
    pub(crate) fn retained_parent(&self) -> Option<&DestinationGrant> {
        self.0.parent.as_ref()
    }
    pub(crate) fn close_retained_leaf(
        &mut self,
        parent_receiver: Option<&DestinationGrant>,
    ) -> Result<()> {
        producer_file::close_retained_leaf_grant(self, parent_receiver)
    }
    /// Closes sole-owned native pins with a checked close. Outstanding strong
    /// borrowers or a failed native close leave this owner available for retry.
    pub fn close_retained_unique(&mut self) -> Result<()> {
        producer_file::close_retained_unique_grant(self)
    }
    pub fn verify_owned_artifact(
        &self,
        basename: &str,
        expected_identity: &FileIdentity,
        expected_bytes: u64,
        expected_sha256: &str,
        cancel: &DownloadCancel,
    ) -> Result<VerifiedOwnedArtifact> {
        validate_basename(basename)?;
        ensure!(
            expected_bytes <= MAX_DOWNLOAD_BYTES
                && expected_sha256.len() == 64
                && expected_sha256.bytes().all(|byte| byte.is_ascii_hexdigit()),
            "Invalid owned artifact recovery expectation"
        );
        cancel.check()?;
        self.verify()?;
        let mut file = platform::open_relative_readonly(self.parent(), basename)?;
        platform::validate(&file, false)?;
        ensure!(
            platform::identity(&file)? == *expected_identity
                && platform::link_count(&file)? == 1
                && file.metadata()?.len() == expected_bytes,
            "Owned artifact recovery identity, link count or length differs"
        );
        let stamp = platform::stamp(&file)?;
        let mut hasher = Sha256::new();
        let mut bytes = 0u64;
        let mut buffer = [0u8; COPY_BUFFER_BYTES];
        loop {
            cancel.check()?;
            self.verify()?;
            ensure!(
                platform::stamp(&file)? == stamp,
                "Owned artifact changed during recovery verification"
            );
            let read = file.read(&mut buffer)?;
            self.verify()?;
            cancel.check()?;
            ensure!(
                platform::stamp(&file)? == stamp,
                "Owned artifact changed during recovery read"
            );
            if read == 0 {
                break;
            }
            bytes += read as u64;
            ensure!(
                bytes <= expected_bytes && bytes <= MAX_DOWNLOAD_BYTES,
                "Owned artifact recovery length exceeded its expectation"
            );
            hasher.update(&buffer[..read]);
        }
        let sha256 = format!("{:x}", hasher.finalize());
        ensure!(
            bytes == expected_bytes && sha256.eq_ignore_ascii_case(expected_sha256),
            "Owned artifact recovery digest differs"
        );
        platform::verify_file_name(self.parent(), basename, expected_identity)?;
        ensure!(
            platform::identity(&file)? == *expected_identity
                && platform::stamp(&file)? == stamp
                && platform::link_count(&file)? == 1,
            "Owned artifact recovery final physical proof changed"
        );
        cancel.check()?;
        self.verify()?;
        Ok(VerifiedOwnedArtifact {
            basename: basename.into(),
            identity: expected_identity.clone(),
            bytes,
            sha256,
        })
    }
    /// This call represents an explicit user selection, independent of library read grants.
    pub fn open_selected(path: impl AsRef<Path>, epoch: u64) -> Result<Self> {
        ensure!(
            epoch != 0 && path.as_ref().is_absolute(),
            "Destination selection requires an absolute path and nonzero epoch"
        );
        ensure!(
            path.as_ref().components().count() <= 64
                && path
                    .as_ref()
                    .as_os_str()
                    .to_string_lossy()
                    .encode_utf16()
                    .count()
                    <= 32000,
            "Destination path or ancestor depth exceeds its bound"
        );
        ensure!(
            path.as_ref()
                .components()
                .all(|c| !matches!(c, Component::ParentDir | Component::CurDir)),
            "Destination selection contains traversal"
        );
        let (path, pins) = platform::pin_directory(path.as_ref())?;
        let identities = pins
            .iter()
            .map(platform::identity)
            .collect::<Result<Vec<_>>>()?;
        let selected_identity = identities
            .last()
            .context("Selected destination identity missing")?
            .clone();
        Ok(Self(Arc::new(GrantInner {
            path,
            pins,
            identities,
            epoch,
            selected_identity,
            parent: None,
            revoked: Arc::new(Mutex::new(false)),
        })))
    }
    pub fn path(&self) -> &Path {
        &self.0.path
    }
    pub fn epoch(&self) -> u64 {
        self.0.epoch
    }
    pub fn identity(&self) -> &FileIdentity {
        &self.0.selected_identity
    }
    pub fn working_directory_identity(&self) -> &FileIdentity {
        self.0.identities.last().expect("Selected directory pin")
    }
    /// A single chosen group component inherits only its selected parent's write scope.
    pub fn derive_group(&self, group_component: &str) -> Result<Self> {
        self.open_group(group_component, true)
    }
    /// Read-only recovery never creates a missing group directory.
    pub fn open_existing_group(&self, group_component: &str) -> Result<Self> {
        self.open_group(group_component, false)
    }
    fn open_group(&self, group_component: &str, create_allowed: bool) -> Result<Self> {
        validate_basename(group_component)?;
        ensure!(
            self.0.parent.is_none(),
            "Download grouping permits one relative directory component"
        );
        self.verify()?;
        let child = match platform::open_relative(self.parent(), group_component, true, false) {
            Ok(file) => file,
            Err(error) if create_allowed && is_not_found(&error) => {
                platform::create_child_directory(self.parent(), group_component)?
            }
            Err(error) => return Err(error),
        };
        let path = self.path().join(group_component);
        ensure!(
            path.as_os_str().to_string_lossy().encode_utf16().count() <= 32000,
            "Download grouped path exceeds its bound"
        );
        let child_identity = platform::identity(&child)?;
        platform::verify_child_directory(self.parent(), group_component, &child_identity)?;
        self.verify()?;
        Ok(Self(Arc::new(GrantInner {
            path,
            pins: vec![child],
            identities: vec![child_identity],
            epoch: self.epoch(),
            selected_identity: self.identity().clone(),
            parent: Some(self.clone()),
            revoked: self.0.revoked.clone(),
        })))
    }
    pub fn revoke(&self) {
        *self.0.revoked.lock().unwrap_or_else(|e| e.into_inner()) = true;
    }
    pub fn is_revoked(&self) -> bool {
        *self.0.revoked.lock().unwrap_or_else(|e| e.into_inner())
    }
    fn check_locked(&self) -> Result<()> {
        ensure!(!self.is_revoked(), "Download destination grant revoked");
        ensure!(
            !self.0.pins.is_empty() && self.0.pins.len() == self.0.identities.len(),
            "Download destination has no complete retained native pins"
        );
        for (pin, expected) in self.0.pins.iter().zip(&self.0.identities) {
            platform::validate(pin, true)?;
            ensure!(
                &platform::identity(pin)? == expected,
                "Download destination identity changed"
            );
        }
        if let Some(parent) = &self.0.parent {
            parent.verify()?;
            platform::verify_child_directory(
                parent.parent(),
                self.0
                    .path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .context("Grouped destination component is invalid")?,
                self.working_directory_identity(),
            )?;
        } else {
            platform::verify_directory_path(&self.0.path, &self.0.pins)?;
        }
        Ok(())
    }
    pub fn verify(&self) -> Result<()> {
        self.check_locked()
    }
    #[cfg(windows)]
    fn verify_pinned_io(&self) -> Result<()> {
        ensure!(!self.is_revoked(), "Download destination grant revoked");
        ensure!(
            !self.0.pins.is_empty() && self.0.pins.len() == self.0.identities.len(),
            "Download destination has no complete retained native pins"
        );
        if let Some(parent) = &self.0.parent {
            parent.verify_pinned_io()?;
        }
        // These named directory handles exclude delete sharing, pinning every
        // ancestor and the original child namespace for the borrow's lifetime.
        // Reparse and case flags remain mutable, so inspect the actual FDs.
        for directory in &self.0.pins {
            platform::validate(directory, true)?;
        }
        Ok(())
    }
    pub fn plan(&self, basename: &str, policy: CollisionPolicy) -> Result<DownloadPlan> {
        validate_basename(basename)?;
        self.verify()?;
        let path = self.0.path.join(basename);
        ensure!(
            path.as_os_str().to_string_lossy().encode_utf16().count() <= 32000,
            "Download path exceeds its bound"
        );
        let existing = platform::open_relative(self.parent(), basename, false, false);
        match (&policy, existing) {
            (CollisionPolicy::Skip, Err(error)) if is_not_found(&error) => {}
            (CollisionPolicy::Skip, Ok(_)) => bail!("Download destination already exists"),
            (CollisionPolicy::Overwrite { observed }, Ok(file)) => {
                ensure!(
                    &platform::identity(&file)? == observed,
                    "Overwrite destination identity changed"
                );
                bail!("UnsupportedCapability: transactional overwrite is unavailable");
            }
            (_, Err(error)) => return Err(error).context("Cannot inspect download destination"),
        }
        Ok(DownloadPlan {
            grant: self.clone(),
            basename: basename.into(),
            policy,
            root_identity: self.identity().clone(),
            epoch: self.epoch(),
        })
    }
    fn parent(&self) -> &File {
        self.0.pins.last().expect("Selected directory pin")
    }
}
fn is_not_found(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<io::Error>()
        .is_some_and(|e| e.kind() == io::ErrorKind::NotFound)
}

pub fn validate_basename(value: &str) -> Result<()> {
    ensure!(
        !value.is_empty() && value.encode_utf16().count() <= 240 && value != "." && value != "..",
        "Invalid download basename"
    );
    ensure!(
        !value.ends_with(['.', ' '])
            && !value
                .chars()
                .any(|c| c.is_control() || "\\/:*?#\"<>|".contains(c)),
        "Unsafe download basename"
    );
    let stem = value
        .split('.')
        .next()
        .unwrap_or("")
        .trim_end()
        .to_ascii_uppercase();
    ensure!(
        !matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
            && !(stem.len() == 4
                && (stem.starts_with("COM") || stem.starts_with("LPT"))
                && matches!(stem.as_bytes()[3], b'1'..=b'9')),
        "Reserved download basename"
    );
    Ok(())
}

#[derive(Clone)]
pub struct DownloadPlan {
    grant: DestinationGrant,
    basename: String,
    policy: CollisionPolicy,
    root_identity: FileIdentity,
    epoch: u64,
}
impl DownloadPlan {
    pub fn lyric_sidecar_plan(&self, collision: CollisionPolicy) -> Result<DownloadPlan> {
        self.verify()?;
        let stem = Path::new(&self.basename)
            .file_stem()
            .and_then(|stem| stem.to_str())
            .context("Download audio stem is invalid")?;
        self.grant.plan(&format!("{stem}.lrc"), collision)
    }
    pub fn path(&self) -> PathBuf {
        self.grant.path().join(&self.basename)
    }
    pub fn basename(&self) -> &str {
        &self.basename
    }
    pub fn grant(&self) -> &DestinationGrant {
        &self.grant
    }
    pub fn verify(&self) -> Result<()> {
        self.grant.verify()?;
        ensure!(
            self.grant.epoch() == self.epoch && self.grant.identity() == &self.root_identity,
            "Download plan grant changed"
        );
        ensure!(
            matches!(self.policy, CollisionPolicy::Skip),
            "UnsupportedCapability: transactional overwrite is unavailable"
        );
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct OwnedPartRecord {
    pub basename: String,
    pub nonce: String,
    pub identity: FileIdentity,
    pub destination_identity: FileIdentity,
    pub destination_epoch: u64,
    pub final_basename: String,
}
#[derive(Clone, Debug, Eq, PartialEq)]
struct FileStamp {
    changed: i128,
    written: i128,
    length: u64,
}
struct Integrity {
    hasher: Sha256,
    bytes: u64,
    stamp: FileStamp,
}
#[derive(Clone)]
struct ExecutionGuard {
    lease: super::DownloadExecutionLease,
    context: super::DownloadContext,
    cancel: DownloadCancel,
}
impl ExecutionGuard {
    fn check(&self) -> Result<()> {
        self.cancel.check()?;
        self.lease.check(&self.context)
    }
}
pub struct OwnedPart {
    plan: DownloadPlan,
    record: OwnedPartRecord,
    file: ProducerFile,
    quota: Option<super::quota::PartialQuota>,
    integrity: Option<Integrity>,
    execution: Option<ExecutionGuard>,
}
impl OwnedPart {
    pub fn record(&self) -> &OwnedPartRecord {
        &self.record
    }
    pub(crate) fn attach_quota(&mut self, quota: super::quota::PartialQuota) -> Result<()> {
        quota.update(&self.record.identity, self.len()?)?;
        self.file.attach_quota(quota.clone())?;
        self.quota = Some(quota);
        Ok(())
    }
    pub(crate) fn bind_execution(
        &mut self,
        lease: super::DownloadExecutionLease,
        context: super::DownloadContext,
        cancel: DownloadCancel,
    ) {
        self.execution = Some(ExecutionGuard {
            lease,
            context,
            cancel,
        });
    }
    fn check_execution(&self) -> Result<()> {
        if let Some(execution) = &self.execution {
            execution.check()?;
        }
        Ok(())
    }
    pub fn path(&self) -> PathBuf {
        self.plan.grant.path().join(&self.record.basename)
    }
    pub fn len(&self) -> Result<u64> {
        self.verify()?;
        Ok(self.file.metadata()?.len())
    }
    pub fn is_empty(&self) -> Result<bool> {
        Ok(self.len()? == 0)
    }
    fn verify_borrowed_io(&self) -> Result<()> {
        #[cfg(windows)]
        {
            self.plan.grant.verify_pinned_io()?;
            // The original named handle excludes write/delete sharing. Its
            // actual object and exact namespace were verified before borrowing;
            // their identity cannot be replaced while these handles stay open.
            Ok(())
        }
        #[cfg(not(windows))]
        {
            self.verify()
        }
    }
    pub fn verify(&self) -> Result<()> {
        self.plan.verify()?;
        self.file.inspect(|file| platform::validate(file, false))?;
        ensure!(
            self.file.inspect(|file| platform::link_count(file))? == 1,
            "Owned download partial has an unapproved hard link"
        );
        ensure!(
            self.file.inspect(|file| platform::identity(file))? == self.record.identity,
            "Owned download partial identity changed"
        );
        platform::verify_file_name(
            self.plan.grant.parent(),
            &self.record.basename,
            &self.record.identity,
        )?;
        Ok(())
    }
    pub(crate) fn append(
        &mut self,
        bytes: &[u8],
        cancel: &DownloadCancel,
        limit: u64,
    ) -> Result<()> {
        cancel.check()?;
        self.check_execution()?;
        self.verify()?;
        let integrity = self
            .integrity
            .as_ref()
            .context("Download partial integrity is uninitialized")?;
        ensure!(
            self.file.inspect(|file| platform::stamp(file))? == integrity.stamp
                && self.file.metadata()?.len() == integrity.bytes,
            "Owned partial changed outside its authorized writer"
        );
        let previous = integrity.bytes;
        ensure!(
            bytes.len() <= COPY_BUFFER_BYTES
                && self
                    .file
                    .metadata()?
                    .len()
                    .checked_add(bytes.len() as u64)
                    .is_some_and(|n| n <= limit),
            "Download byte limit exceeded"
        );
        if let Some(quota) = &self.quota {
            quota.update(
                &self.record.identity,
                self.file.metadata()?.len() + bytes.len() as u64,
            )?;
        }
        self.file.seek(SeekFrom::End(0))?;
        self.file.write_all(bytes)?;
        ensure!(
            self.file.metadata()?.len() == previous + bytes.len() as u64,
            "Owned partial length changed outside its authorized writer"
        );
        let stamp = self.file.inspect(|file| platform::stamp(file))?;
        let integrity = self
            .integrity
            .as_mut()
            .expect("Initialized download writer integrity");
        integrity.hasher.update(bytes);
        integrity.bytes += bytes.len() as u64;
        integrity.stamp = stamp;
        self.verify()?;
        self.check_execution()?;
        cancel.check()?;
        Ok(())
    }
    pub(crate) fn reset(&mut self, cancel: &DownloadCancel) -> Result<()> {
        cancel.check()?;
        self.check_execution()?;
        self.verify()?;
        self.file.set_len(0)?;
        self.file.seek(SeekFrom::Start(0))?;
        self.file.sync_all()?;
        self.verify()?;
        if let Some(quota) = &self.quota {
            quota.update(&self.record.identity, 0)?;
        }
        self.integrity = Some(Integrity {
            hasher: Sha256::new(),
            bytes: 0,
            stamp: self.file.inspect(|file| platform::stamp(file))?,
        });
        self.check_execution()?;
        Ok(())
    }
    /// Discards only this physically verified own prefix before rebinding its representation.
    pub fn restart_for_representation(
        &mut self,
        cancel: &DownloadCancel,
    ) -> Result<TransferCheckpoint> {
        self.reset(cancel)?;
        self.checkpoint(cancel, None, None, false)
    }
    pub fn acknowledge_artifact_allocation(
        &self,
        kind: super::artifact::OwnedArtifactKind,
        context: &super::DownloadContext,
        reserved_max_bytes: u64,
        journal: &dyn super::artifact::OwnedArtifactJournal,
        cancel: &DownloadCancel,
    ) -> Result<()> {
        cancel.check()?;
        self.check_execution()?;
        self.verify()?;
        ensure!(
            self.len()? == 0,
            "Artifact ownership must be durable before its first body write"
        );
        let group = self
            .plan
            .grant
            .0
            .parent
            .as_ref()
            .map(|_| {
                self.plan
                    .grant
                    .path()
                    .file_name()
                    .and_then(|name| name.to_str())
                    .context("Artifact group component missing")
                    .map(str::to_owned)
            })
            .transpose()?;
        let allocation = super::artifact::OwnedArtifactAllocation {
            kind,
            record: self.record.clone(),
            working_directory_path: self.plan.grant.path().to_owned(),
            working_directory_identity: self.plan.grant.working_directory_identity().clone(),
            selected_root_identity: self.plan.root_identity.clone(),
            relative_group: group,
            reserved_max_bytes,
            context: context.clone(),
        };
        allocation.validate()?;
        self.file
            .acknowledge_created(|file, plan, basename, nonce| {
                ensure!(
                    basename == allocation.record.basename && nonce == allocation.record.nonce,
                    "Creation journal differs from its admitted actual object"
                );
                platform::validate(file, false)?;
                ensure!(
                    platform::identity(file)? == allocation.record.identity
                        && platform::link_count(file)? == 1
                        && file.metadata()?.len() == 0,
                    "Creation journal no longer holds its original zero file"
                );
                platform::verify_file_name(
                    plan.grant.parent(),
                    basename,
                    &allocation.record.identity,
                )?;
                journal.record_created(&allocation)?;
                cancel.check()?;
                self.check_execution()?;
                plan.verify()?;
                ensure!(
                    platform::identity(file)? == allocation.record.identity
                        && platform::link_count(file)? == 1
                        && file.metadata()?.len() == 0,
                    "Actual zero file changed during its creation acknowledgement"
                );
                Ok(allocation.record.clone())
            })?;
        cancel.check()?;
        self.check_execution()?;
        self.verify()?;
        ensure!(
            self.len()? == 0,
            "Artifact changed while its ownership was being recorded"
        );
        Ok(())
    }
    pub fn sync(&mut self) -> Result<()> {
        self.verify()?;
        self.file.sync_all()?;
        self.verify()
    }
    pub fn checkpoint(
        &mut self,
        cancel: &DownloadCancel,
        strong_etag: Option<String>,
        remote_total: Option<u64>,
        completed_body: bool,
    ) -> Result<TransferCheckpoint> {
        self.sync()?;
        let (bytes, digest) = self.hash(cancel)?;
        Ok(TransferCheckpoint {
            bytes,
            sha256: digest,
            strong_etag,
            remote_total,
            completed_body,
            representation: None,
        })
    }
    pub(crate) fn checkpoint_if_initialized(
        &mut self,
        strong_etag: Option<String>,
        remote_total: Option<u64>,
        completed_body: bool,
    ) -> Result<TransferCheckpoint> {
        ensure!(
            self.integrity.is_some(),
            "Owned partial requires verification before a fresh checkpoint"
        );
        self.checkpoint(
            &DownloadCancel::default(),
            strong_etag,
            remote_total,
            completed_body,
        )
    }
    pub(crate) fn hash(&mut self, cancel: &DownloadCancel) -> Result<(u64, String)> {
        cancel.check()?;
        self.verify()?;
        if let Some(integrity) = &self.integrity {
            ensure!(
                self.file.inspect(|file| platform::stamp(file))? == integrity.stamp
                    && self.file.metadata()?.len() == integrity.bytes,
                "Owned partial integrity stamp changed"
            );
            return Ok((
                integrity.bytes,
                format!("{:x}", integrity.hasher.clone().finalize()),
            ));
        }
        let initial_stamp = self.file.inspect(|file| platform::stamp(file))?;
        self.check_execution()?;
        self.file.seek(SeekFrom::Start(0))?;
        let mut hasher = Sha256::new();
        let mut bytes = 0;
        let mut buffer = [0u8; COPY_BUFFER_BYTES];
        loop {
            cancel.check()?;
            self.check_execution()?;
            self.verify()?;
            let n = self.file.read(&mut buffer)?;
            self.verify()?;
            if n == 0 {
                break;
            }
            bytes += n as u64;
            ensure!(
                bytes <= MAX_DOWNLOAD_BYTES,
                "Owned partial exceeds byte limit"
            );
            hasher.update(&buffer[..n]);
        }
        let final_stamp = self.file.inspect(|file| platform::stamp(file))?;
        self.check_execution()?;
        ensure!(
            initial_stamp == final_stamp && bytes == final_stamp.length,
            "Owned partial changed during its integrity verification"
        );
        let digest = format!("{:x}", hasher.clone().finalize());
        self.integrity = Some(Integrity {
            hasher,
            bytes,
            stamp: final_stamp,
        });
        Ok((bytes, digest))
    }
    pub(crate) fn tail(&mut self, length: usize, cancel: &DownloadCancel) -> Result<Vec<u8>> {
        ensure!(length <= 10, "Download overlap exceeds its bound");
        cancel.check()?;
        self.verify()?;
        let len = self.file.metadata()?.len();
        ensure!(
            length as u64 <= len,
            "Owned partial overlap exceeds its length"
        );
        self.file.seek(SeekFrom::Start(len - length as u64))?;
        let mut tail = vec![0; length];
        self.file.read_exact(&mut tail)?;
        self.verify()?;
        Ok(tail)
    }
    pub(crate) fn reader(&self, cancel: DownloadCancel) -> Result<GuardedReader> {
        self.check_execution()?;
        self.verify()?;
        let mut file = self.file.shared_borrow();
        file.seek(SeekFrom::Start(0))?;
        Ok(GuardedReader {
            file,
            plan: self.plan.clone(),
            identity: self.record.identity.clone(),
            basename: self.record.basename.clone(),
            cancel,
            execution: self.execution.clone(),
        })
    }
    /// A separate owned copy preserves the good audio if rewriting is stopped.
    pub fn copy_for_postprocess(&self, _cancel: &DownloadCancel) -> Result<Self> {
        Err(CreationJournalRequired.into())
    }
    pub fn copy_for_postprocess_journaled(
        &self,
        cancel: &DownloadCancel,
        journal: &dyn super::artifact::OwnedArtifactJournal,
    ) -> Result<Self> {
        cancel.check()?;
        self.check_execution()?;
        self.verify()?;
        let context = self
            .execution
            .as_ref()
            .context("Postprocess ownership requires its original execution context")?
            .context
            .clone();
        let mut copy = Self::create_journaled(
            self.plan.clone(),
            super::artifact::OwnedArtifactKind::TagCopy,
            &context,
            MAX_DOWNLOAD_BYTES,
            journal,
            cancel,
        )?;
        if let Some(quota) = &self.quota {
            copy.attach_quota(quota.clone())?;
        }
        copy.execution = self.execution.clone();
        let mut reader = self.reader(cancel.clone())?;
        let mut buffer = [0u8; COPY_BUFFER_BYTES];
        loop {
            let length = reader.read(&mut buffer)?;
            if length == 0 {
                break;
            }
            copy.append(&buffer[..length], cancel, MAX_DOWNLOAD_BYTES)?;
        }
        copy.sync()?;
        cancel.check()?;
        self.check_execution()?;
        Ok(copy)
    }
    /// The writer is restricted to this task's exact owned temporary object.
    pub fn rewrite_io(&mut self, cancel: DownloadCancel) -> Result<OwnedFileIo<'_>> {
        cancel.check()?;
        self.check_execution()?;
        self.verify()?;
        let stamp = self.file.inspect(|file| platform::stamp(file))?;
        self.integrity = None;
        self.file.seek(SeekFrom::Start(0))?;
        Ok(OwnedFileIo {
            part: self,
            cancel,
            stamp,
        })
    }
    pub fn remove_owned(self) -> Result<()> {
        self.remove_owned_after_task_commit().map(|_| ())
    }
    pub fn remove_owned_after_task_commit(self) -> Result<OwnedPartRecord> {
        self.verify()?;
        ensure!(
            self.file.producers_joined()?,
            "Actual producer has not joined before removal"
        );
        self.file.inspect(|file| {
            platform::remove_owned(
                file,
                self.plan.grant.parent(),
                &self.record.basename,
                &self.record.identity,
            )
        })?;
        self.file.close_after_join()?;
        let OwnedPart {
            plan,
            record,
            file,
            quota,
            integrity: _,
            execution: _,
        } = self;
        drop(plan);
        let receipt = file.release_removed()?;
        ensure!(
            receipt == record,
            "Actual removal receipt changed its original object"
        );
        if let Some(quota) = quota {
            quota.remove(&record.identity);
        }
        Ok(receipt)
    }
    pub(crate) fn commit_sidecar(
        mut self,
        expected_bytes: u64,
        expected_sha256: &str,
        cancel: &DownloadCancel,
    ) -> Result<CommittedSidecar> {
        cancel.check()?;
        self.check_execution()?;
        self.sync()?;
        ensure!(
            self.plan.basename.ends_with(".lrc"),
            "Only a planned lyric sidecar can use artifact commit"
        );
        let (bytes, sha256) = self.hash(cancel)?;
        ensure!(
            bytes == expected_bytes
                && sha256 == expected_sha256
                && bytes <= super::lyrics::MAX_DOWNLOAD_LYRIC_OUTPUT_BYTES as u64 + 3,
            "Lyric sidecar digest differs from its prepared content"
        );
        self.file.inspect(|file| {
            platform::rename_no_replace(
                file,
                self.plan.grant.parent(),
                &self.record.basename,
                &self.plan.basename,
                &self.record.identity,
            )
        })?;
        self.plan.grant.verify()?;
        self.check_execution()?;
        platform::verify_file_name(
            self.plan.grant.parent(),
            &self.plan.basename,
            &self.record.identity,
        )?;
        let retained_part = None;
        Ok(CommittedSidecar {
            path: self.plan.path(),
            identity: self.record.identity,
            bytes,
            sha256,
            retained_part,
        })
    }
    pub(crate) fn into_verified(
        mut self,
        checkpoint: TransferCheckpoint,
        verification: super::AudioVerification,
        execution_lease: super::DownloadExecutionLease,
        context: super::DownloadContext,
        cancel: DownloadCancel,
    ) -> Result<VerifiedOwnedAudio> {
        self.sync()?;
        self.verify()?;
        ensure!(
            checkpoint.completed_body && checkpoint.bytes == self.len()? && checkpoint.bytes > 0,
            "Cannot commit incomplete download audio"
        );
        Ok(VerifiedOwnedAudio {
            part: self,
            checkpoint,
            verification,
            execution_lease,
            context,
            cancel,
        })
    }
}

pub struct VerifiedOwnedAudio {
    part: OwnedPart,
    checkpoint: TransferCheckpoint,
    verification: super::AudioVerification,
    execution_lease: super::DownloadExecutionLease,
    context: super::DownloadContext,
    cancel: DownloadCancel,
}
impl VerifiedOwnedAudio {
    pub(crate) fn producer_object(&self) -> DownloadProducerObject {
        self.part.producer_object()
    }
    pub(crate) fn create_sidecar_journaled(
        &self,
        plan: DownloadPlan,
        maximum_bytes: u64,
        journal: &dyn super::artifact::OwnedArtifactJournal,
        cancel: &DownloadCancel,
    ) -> Result<OwnedPart> {
        self.execution_lease.check(&self.context)?;
        let mut part = OwnedPart::create_journaled(
            plan,
            super::artifact::OwnedArtifactKind::LyricSidecar,
            &self.context,
            maximum_bytes,
            journal,
            cancel,
        )?;
        self.bind_sidecar_execution(&mut part)?;
        Ok(part)
    }
    /// Cleanup is called only after the controller acknowledges its task commit.
    pub fn discard_original_after_task_commit(self) -> Result<OwnedPartRecord> {
        self.part.remove_owned_after_task_commit()
    }
    pub fn checkpoint(&self) -> &TransferCheckpoint {
        &self.checkpoint
    }
    pub fn verification(&self) -> &super::AudioVerification {
        &self.verification
    }
    pub fn owned_record(&self) -> &OwnedPartRecord {
        self.part.record()
    }
    pub fn copy_for_postprocess(&self, cancel: &DownloadCancel) -> Result<OwnedPart> {
        self.cancel.check()?;
        self.execution_lease.check(&self.context)?;
        self.part.copy_for_postprocess(cancel)
    }
    pub fn copy_for_postprocess_journaled(
        &self,
        cancel: &DownloadCancel,
        journal: &dyn super::artifact::OwnedArtifactJournal,
    ) -> Result<OwnedPart> {
        self.cancel.check()?;
        self.execution_lease.check(&self.context)?;
        self.part.copy_for_postprocess_journaled(cancel, journal)
    }
    pub(crate) fn plan(&self) -> &DownloadPlan {
        &self.part.plan
    }
    pub(crate) fn bind_sidecar_execution(&self, part: &mut OwnedPart) -> Result<()> {
        self.cancel.check()?;
        self.execution_lease.check(&self.context)?;
        part.bind_execution(
            self.execution_lease.clone(),
            self.context.clone(),
            self.cancel.clone(),
        );
        if let Some(quota) = &self.part.quota {
            part.attach_quota(quota.clone())?;
        }
        Ok(())
    }
    pub(crate) fn seal_postprocessed_copy(
        &self,
        mut copy: OwnedPart,
        extension: &str,
        cancel: &DownloadCancel,
    ) -> Result<VerifiedOwnedAudio> {
        self.cancel.check()?;
        self.execution_lease.check(&self.context)?;
        cancel.check()?;
        ensure!(
            copy.plan.epoch == self.part.plan.epoch
                && copy.plan.root_identity == self.part.plan.root_identity
                && copy.plan.basename == self.part.plan.basename
                && copy.plan.path() == self.part.plan.path()
                && copy.record.identity != self.part.record.identity,
            "Postprocess copy does not belong to its original selected output"
        );
        let verification =
            super::AudioVerifier::verify(&super::SymphoniaVerifier, &copy, extension, cancel)?;
        ensure!(
            verification == self.verification,
            "Postprocess copy changed the original codec, precision or canonical audio samples"
        );
        let checkpoint = copy.checkpoint(cancel, None, None, true)?;
        copy.into_verified(
            checkpoint,
            verification,
            self.execution_lease.clone(),
            self.context.clone(),
            cancel.clone(),
        )
    }
    pub fn commit(mut self) -> Result<CommittedAudio> {
        self.cancel.check()?;
        self.execution_lease.check(&self.context)?;
        self.part.check_execution()?;
        self.part.sync()?;
        self.part.verify()?;
        let (bytes, digest) = self.part.hash(&self.cancel)?;
        ensure!(
            bytes == self.checkpoint.bytes && digest == self.checkpoint.sha256,
            "Verified audio changed before atomic commit"
        );
        self.part.file.inspect(|file| {
            platform::rename_no_replace(
                file,
                self.part.plan.grant.parent(),
                &self.part.record.basename,
                &self.part.plan.basename,
                &self.part.record.identity,
            )
        })?;
        self.part.plan.grant.verify()?;
        self.execution_lease.check(&self.context)?;
        self.part.check_execution()?;
        platform::verify_file_name(
            self.part.plan.grant.parent(),
            &self.part.plan.basename,
            &self.part.record.identity,
        )?;
        let path = self.part.plan.path();
        let retained_part = None;
        Ok(CommittedAudio {
            path,
            identity: self.part.record.identity,
            checkpoint: self.checkpoint,
            verification: self.verification,
            retained_part,
        })
    }
}
#[derive(Clone, Debug, Serialize)]
pub struct CommittedAudio {
    pub path: PathBuf,
    pub identity: FileIdentity,
    pub checkpoint: TransferCheckpoint,
    pub verification: super::AudioVerification,
    pub retained_part: Option<OwnedPartRecord>,
}
#[derive(Clone, Debug, Serialize)]
pub struct CommittedSidecar {
    pub path: PathBuf,
    pub identity: FileIdentity,
    pub bytes: u64,
    pub sha256: String,
    pub retained_part: Option<OwnedPartRecord>,
}
#[derive(Clone, Debug, Serialize)]
pub struct VerifiedOwnedArtifact {
    basename: String,
    identity: FileIdentity,
    bytes: u64,
    sha256: String,
}

/// A live selected destination and its actual write-open published object.
/// Stored identity and hash expectations cannot construct this owner.
pub(crate) struct PublishedAudioOwner {
    grant: DestinationGrant,
    file: File,
    proof: super::task_model::DownloadArtifactProof,
}
pub(crate) struct RetryArtifactRetirement {
    record: OwnedPartRecord,
}
pub(crate) struct RetryArtifactRemovalPending {
    grant: DestinationGrant,
    record: OwnedPartRecord,
    calibration: NativeVolumeCalibration,
    message: String,
    file: ProducerFile,
}
impl std::fmt::Debug for RetryArtifactRemovalPending {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RetryArtifactRemovalPending")
            .field("record", &self.record)
            .finish()
    }
}
impl std::fmt::Display for RetryArtifactRemovalPending {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Download retry physical retirement is pending: {}",
            self.message
        )
    }
}
impl std::error::Error for RetryArtifactRemovalPending {}
impl RetryArtifactRemovalPending {
    pub(crate) fn finish(&self) -> Result<RetryArtifactRetirement> {
        prove_retry_absent(&self.grant, &self.record, &self.calibration)?;
        self.file.release_removed()?;
        Ok(RetryArtifactRetirement {
            record: self.record.clone(),
        })
    }
}
fn prove_retry_absent(
    grant: &DestinationGrant,
    record: &OwnedPartRecord,
    calibration: &NativeVolumeCalibration,
) -> Result<()> {
    grant.verify()?;
    #[cfg(windows)]
    {
        platform::prove_attested_cleanup_absent(
            grant.parent(),
            &record.identity,
            &calibration.volume_guid,
        )?;
        grant.verify()?;
        Ok(())
    }
    #[cfg(unix)]
    {
        ensure!(
            calibration.unlinked.as_ref() == Some(&record.identity)
                && calibration.filesystem == "POSIX-UNLINK"
                && calibration.volume_guid == platform::cleanup_volume_guid(grant.parent())?,
            "Unix retry retirement has no live same-inode zero-link witness"
        );
        grant.verify()
    }
}
pub(crate) fn recover_retry_retirement(
    grant: &DestinationGrant,
    record: &OwnedPartRecord,
    attestation: &super::postprocess_retry::RetryDeletionAttestation,
) -> Result<RetryArtifactRetirement> {
    let calibration = grant.calibrate_cleanup_volume()?;
    ensure!(
        calibration.volume_guid == attestation.volume_guid
            && calibration.filesystem == attestation.filesystem,
        "Prepared retry deletion has a different actual calibrated volume"
    );
    prove_retry_absent(grant, record, &calibration)?;
    Ok(RetryArtifactRetirement {
        record: record.clone(),
    })
}
pub(crate) struct RetrySidecarPreparationFailure {
    part: OwnedPart,
    context: super::DownloadContext,
    message: String,
}
impl std::fmt::Debug for RetrySidecarPreparationFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RetrySidecarPreparationFailure")
            .field("record", self.part.record())
            .finish()
    }
}
impl std::fmt::Display for RetrySidecarPreparationFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Download lyric preparation retains its actual file: {}",
            self.message
        )
    }
}
impl std::error::Error for RetrySidecarPreparationFailure {}
impl RetrySidecarPreparationFailure {
    pub(crate) fn acknowledge(
        &mut self,
        journal: &dyn super::artifact::OwnedArtifactJournal,
        cancel: &DownloadCancel,
    ) -> Result<()> {
        ensure!(
            self.part.len()? == 0,
            "Unacknowledged download lyric file is no longer empty"
        );
        self.part.bind_execution(
            super::DownloadExecutionLease::from_approved_context(self.context.clone())?,
            self.context.clone(),
            cancel.clone(),
        );
        self.part.acknowledge_artifact_allocation(
            super::artifact::OwnedArtifactKind::LyricSidecar,
            &self.context,
            super::lyrics::MAX_DOWNLOAD_LYRIC_OUTPUT_BYTES as u64 + 3,
            journal,
            cancel,
        )
    }
    pub(crate) fn retire(
        self,
        grant: &DestinationGrant,
        allocation: &super::artifact::OwnedArtifactAllocation,
        prepare: &dyn Fn(
            &OwnedPartRecord,
            &super::postprocess_retry::RetryDeletionAttestation,
        ) -> Result<()>,
    ) -> Result<RetryArtifactRetirement> {
        ensure!(
            self.part.record() == &allocation.record,
            "Retained lyric retirement differs from its actual original file"
        );
        let OwnedPart { file, .. } = self.part;
        retire_retry_file(grant, allocation, file, prepare)
    }
}
impl RetryArtifactRetirement {
    pub(crate) fn record(&self) -> &OwnedPartRecord {
        &self.record
    }
}
pub(crate) struct PreparedRetryArtifactRetirement {
    grant: Option<DestinationGrant>,
    allocation: super::artifact::OwnedArtifactAllocation,
    file: ProducerFile,
    calibration: NativeVolumeCalibration,
}
impl PreparedRetryArtifactRetirement {
    pub(crate) fn record(&self) -> &OwnedPartRecord {
        &self.allocation.record
    }
    pub(crate) fn finish(&mut self) -> Result<RetryArtifactRetirement> {
        ensure!(
            self.file.producers_joined()?,
            "Retry physical retirement loan has not actually joined"
        );
        if self.file.metadata().is_ok() {
            self.file.request_owned_removal()?;
            #[cfg(unix)]
            {
                ensure!(
                    self.file.inspect(platform::link_count)? == 0
                        && self.file.inspect(platform::identity)?
                            == self.allocation.record.identity,
                    "Retry unlink changed its actual inode"
                );
                self.calibration.unlinked = Some(self.allocation.record.identity.clone());
            }
            self.file.close_after_join()?;
        }
        self.file.inspect_namespace(|grant| {
            prove_retry_absent(grant, &self.allocation.record, &self.calibration)
        })?;
        // Return this controller's strong namespace borrow before the keeper
        // attempts the last actual pin close.
        self.grant.take();
        self.file.release_removed()?;
        Ok(RetryArtifactRetirement {
            record: self.allocation.record.clone(),
        })
    }
}
pub(crate) fn prepare_retry_artifact_retirement(
    grant: &DestinationGrant,
    allocation: &super::artifact::OwnedArtifactAllocation,
    prepare: &dyn Fn(
        &OwnedPartRecord,
        &super::postprocess_retry::RetryDeletionAttestation,
    ) -> Result<()>,
) -> Result<PreparedRetryArtifactRetirement> {
    allocation.validate()?;
    grant.verify()?;
    ensure!(
        grant.working_directory_identity() == &allocation.working_directory_identity
            && grant.identity() == &allocation.selected_root_identity
            && crate::domain::music::normalize_local_path(grant.path())
                == crate::domain::music::normalize_local_path(&allocation.working_directory_path),
        "Retry artifact retirement scope changed"
    );
    let plan = DownloadPlan {
        grant: grant.clone(),
        basename: allocation.record.final_basename.clone(),
        policy: CollisionPolicy::Skip,
        root_identity: grant.identity().clone(),
        epoch: grant.epoch(),
    };
    let file = if let Some(file) = ProducerFile::retain_original(&plan, &allocation.record)? {
        if file.metadata().is_err() {
            file.reopen_closed(&plan, &allocation.record)?;
        }
        file
    } else {
        let file = ProducerFile::reserve(
            plan,
            allocation.record.nonce.clone(),
            allocation.kind,
            Some(allocation.context.clone()),
            allocation.reserved_max_bytes,
        )?;
        file.reopen()?;
        file.remember_record(allocation.record.clone())?;
        file.reconcile_bytes()?;
        file
    };
    let calibration = grant.calibrate_cleanup_volume()?;
    file.inspect(|native| {
        platform::validate(native, false)?;
        ensure!(
            platform::identity(native)? == allocation.record.identity
                && platform::link_count(native)? == 1,
            "Retry preparation actual object changed"
        );
        platform::verify_file_name(
            grant.parent(),
            &allocation.record.basename,
            &allocation.record.identity,
        )
    })?;
    #[cfg(windows)]
    ensure!(
        file.inspect(platform::cleanup_volume_guid)? == calibration.volume_guid
            && file.inspect(platform::cleanup_filesystem)? == "NTFS"
            && allocation.record.identity.object.len() == 32
            && allocation.record.identity.object[16..]
                .bytes()
                .all(|byte| byte == b'0'),
        "UnsupportedCapability: prepared retry artifact has unsupported actual FileID or volume semantics"
    );
    prepare(
        &allocation.record,
        &super::postprocess_retry::RetryDeletionAttestation {
            volume_guid: calibration.volume_guid.clone(),
            filesystem: calibration.filesystem.clone(),
        },
    )?;
    Ok(PreparedRetryArtifactRetirement {
        grant: Some(grant.clone()),
        allocation: allocation.clone(),
        file,
        calibration,
    })
}
fn retire_retry_file(
    grant: &DestinationGrant,
    allocation: &super::artifact::OwnedArtifactAllocation,
    file: ProducerFile,
    prepare: &dyn Fn(
        &OwnedPartRecord,
        &super::postprocess_retry::RetryDeletionAttestation,
    ) -> Result<()>,
) -> Result<RetryArtifactRetirement> {
    grant.verify()?;
    allocation.validate()?;
    let record = &allocation.record;
    ensure!(
        file.inspect(platform::identity)? == record.identity
            && file.inspect(platform::link_count)? == 1,
        "Retry artifact retirement object changed"
    );
    ensure!(
        grant.working_directory_identity() == &allocation.working_directory_identity
            && grant.identity() == &allocation.selected_root_identity,
        "Retry actual file namespace owner changed"
    );
    let mut calibration = grant.calibrate_cleanup_volume()?;
    #[cfg(windows)]
    ensure!(
        file.inspect(platform::cleanup_volume_guid)? == calibration.volume_guid
            && file.inspect(platform::cleanup_filesystem)? == "NTFS"
            && record.identity.object.len() == 32
            && record.identity.object[16..].bytes().all(|b| b == b'0'),
        "UnsupportedCapability: retry artifact has unsupported actual FileID or volume semantics"
    );
    prepare(
        record,
        &super::postprocess_retry::RetryDeletionAttestation {
            volume_guid: calibration.volume_guid.clone(),
            filesystem: calibration.filesystem.clone(),
        },
    )?;
    ensure!(
        file.producers_joined()?,
        "Retry artifact producer has not actually joined before retirement"
    );
    file.inspect(|native| {
        platform::remove_owned(native, grant.parent(), &record.basename, &record.identity)
    })?;
    #[cfg(unix)]
    {
        ensure!(
            file.inspect(platform::link_count)? == 0
                && file.inspect(platform::identity)? == record.identity,
            "Retry unlink changed its actual inode"
        );
        calibration.unlinked = Some(record.identity.clone());
    }
    file.close_after_join()?;
    grant.verify()?;
    match prove_retry_absent(grant, record, &calibration) {
        Ok(()) => {
            file.release_removed()?;
            Ok(RetryArtifactRetirement {
                record: record.clone(),
            })
        }
        Err(cause) => Err(RetryArtifactRemovalPending {
            grant: grant.clone(),
            record: record.clone(),
            calibration,
            message: format!("{cause:#}").chars().take(1024).collect(),
            file,
        }
        .into()),
    }
}
impl PublishedAudioOwner {
    pub(crate) fn open(
        grant: DestinationGrant,
        proof: super::task_model::DownloadArtifactProof,
        cancel: &DownloadCancel,
        recovering: bool,
    ) -> Result<Self> {
        proof.validate()?;
        cancel.check()?;
        grant.verify()?;
        let file = platform::open_relative(grant.parent(), &proof.basename, false, false)?;
        platform::validate(&file, false)?;
        ensure!(
            platform::identity(&file)? == proof.identity && platform::link_count(&file)? == 1,
            "Published download object or hardlink count changed"
        );
        platform::verify_file_name(grant.parent(), &proof.basename, &proof.identity)?;
        let mut owner = Self { grant, file, proof };
        if !recovering {
            owner.verify_body(cancel)?;
        }
        Ok(owner)
    }
    pub(crate) fn grant(&self) -> &DestinationGrant {
        &self.grant
    }
    pub(crate) fn verify_body(&mut self, cancel: &DownloadCancel) -> Result<()> {
        cancel.check()?;
        self.grant.verify()?;
        platform::validate(&self.file, false)?;
        let stamp = platform::stamp(&self.file)?;
        ensure!(
            platform::identity(&self.file)? == self.proof.identity
                && platform::link_count(&self.file)? == 1
                && stamp.length == self.proof.bytes,
            "Published download physical output changed"
        );
        self.file.rewind()?;
        let mut hash = Sha256::new();
        let mut bytes = 0u64;
        let mut buffer = [0; COPY_BUFFER_BYTES];
        loop {
            cancel.check()?;
            self.grant.verify()?;
            ensure!(
                platform::stamp(&self.file)? == stamp,
                "Published download changed during verification"
            );
            let read = self.file.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            bytes += read as u64;
            ensure!(
                bytes <= self.proof.bytes,
                "Published download grew during verification"
            );
            hash.update(&buffer[..read]);
        }
        ensure!(
            bytes == self.proof.bytes
                && format!("{:x}", hash.finalize()) == self.proof.sha256
                && platform::stamp(&self.file)? == stamp,
            "Published download body differs from its acknowledged output"
        );
        platform::verify_file_name(
            self.grant.parent(),
            &self.proof.basename,
            &self.proof.identity,
        )?;
        cancel.check()?;
        Ok(())
    }
    pub(crate) fn tag_writer(
        &self,
        selection: crate::library::tag_install::TagWriteSelection,
        context: &crate::library::scan::ScanContext,
        coordinator: &dyn crate::library::tag_install::TagInstallCoordinator,
    ) -> Result<crate::library::tag_install::NativeWriterToken> {
        self.grant.verify()?;
        ensure!(
            crate::domain::music::normalize_local_path(&selection.path)
                == crate::domain::music::normalize_local_path(
                    &self.grant.path().join(&self.proof.basename)
                )
                && selection.expected_identity
                    == crate::domain::library::FileIdentity::from_native_parts(
                        self.proof.identity.device,
                        &self.proof.identity.object,
                    )?,
            "Published writer selection differs from its actual owner"
        );
        let grant = self.grant.clone();
        crate::library::tag_install::NativeWriterToken::from_owned_file(
            selection,
            self.file.try_clone()?,
            Box::new(move || grant.verify()),
            context,
            coordinator,
        )
    }
    pub(crate) fn accept_installed(
        &mut self,
        proof: super::task_model::DownloadArtifactProof,
        cancel: &DownloadCancel,
    ) -> Result<()> {
        ensure!(
            proof.basename == self.proof.basename && proof.identity == self.proof.identity,
            "Installed output changed its published object"
        );
        self.proof = proof;
        self.verify_body(cancel)
    }
    pub(crate) fn prepare_sidecar(
        &self,
        artifact: &super::lyrics::LyricArtifact,
        nonce: String,
        context: super::DownloadContext,
        cancel: &DownloadCancel,
        journal: &dyn super::artifact::OwnedArtifactJournal,
    ) -> Result<OwnedPart> {
        cancel.check()?;
        self.grant.verify()?;
        ensure!(
            artifact.has_ordinary_lyrics
                && !artifact.bytes.is_empty()
                && artifact.bytes.len() <= super::lyrics::MAX_DOWNLOAD_LYRIC_OUTPUT_BYTES + 3,
            "Invalid selected download lyric artifact"
        );
        let audio_plan = DownloadPlan {
            grant: self.grant.clone(),
            basename: self.proof.basename.clone(),
            policy: CollisionPolicy::Skip,
            root_identity: self.grant.identity().clone(),
            epoch: self.grant.epoch(),
        };
        let plan = audio_plan.lyric_sidecar_plan(CollisionPolicy::Skip)?;
        let mut part = OwnedPart::create_with_nonce_journaled(
            plan,
            nonce,
            super::artifact::OwnedArtifactKind::LyricSidecar,
            &context,
            super::lyrics::MAX_DOWNLOAD_LYRIC_OUTPUT_BYTES as u64 + 3,
            journal,
            cancel,
        )?;
        let lease = super::DownloadExecutionLease::from_approved_context(context.clone())?;
        part.bind_execution(lease, context.clone(), cancel.clone());
        let result = (|| -> Result<()> {
            part.acknowledge_artifact_allocation(
                super::artifact::OwnedArtifactKind::LyricSidecar,
                &context,
                super::lyrics::MAX_DOWNLOAD_LYRIC_OUTPUT_BYTES as u64 + 3,
                journal,
                cancel,
            )?;
            for bytes in artifact.bytes.chunks(COPY_BUFFER_BYTES) {
                part.append(
                    bytes,
                    cancel,
                    super::lyrics::MAX_DOWNLOAD_LYRIC_OUTPUT_BYTES as u64 + 3,
                )?;
            }
            part.sync()?;
            Ok(())
        })();
        match result {
            Ok(()) => Ok(part),
            Err(cause) => Err(RetrySidecarPreparationFailure {
                part,
                context,
                message: format!("{cause:#}").chars().take(1024).collect(),
            }
            .into()),
        }
    }
}
impl VerifiedOwnedArtifact {
    pub fn basename(&self) -> &str {
        &self.basename
    }
    pub fn identity(&self) -> &FileIdentity {
        &self.identity
    }
    pub fn bytes(&self) -> u64 {
        self.bytes
    }
    pub fn sha256(&self) -> &str {
        &self.sha256
    }
}

pub(crate) struct GuardedReader {
    file: ProducerFile,
    plan: DownloadPlan,
    identity: FileIdentity,
    basename: String,
    cancel: DownloadCancel,
    execution: Option<ExecutionGuard>,
}
impl GuardedReader {
    fn check(&self) -> io::Result<()> {
        self.cancel
            .check()
            .and_then(|_| {
                if let Some(execution) = &self.execution {
                    execution.check()?;
                }
                Ok(())
            })
            .and_then(|_| self.plan.verify())
            .and_then(|_| {
                platform::verify_file_name(self.plan.grant.parent(), &self.basename, &self.identity)
            })
            .map_err(|_| {
                io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "Download read authority changed",
                )
            })
    }
}
impl Read for GuardedReader {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.check()?;
        let limit = buffer.len().min(COPY_BUFFER_BYTES);
        let n = self.file.read(&mut buffer[..limit])?;
        if let Err(error) = self.check() {
            buffer[..n].fill(0);
            return Err(error);
        }
        Ok(n)
    }
}
impl Seek for GuardedReader {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        self.check()?;
        let value = self.file.seek(from)?;
        self.check()?;
        Ok(value)
    }
}
impl symphonia::core::io::MediaSource for GuardedReader {
    fn is_seekable(&self) -> bool {
        true
    }
    fn byte_len(&self) -> Option<u64> {
        self.file.metadata().ok().map(|m| m.len())
    }
}

pub struct OwnedFileIo<'a> {
    part: &'a mut OwnedPart,
    cancel: DownloadCancel,
    stamp: FileStamp,
}
impl OwnedFileIo<'_> {
    fn check(&self) -> io::Result<()> {
        self.cancel
            .check()
            .and_then(|_| self.part.check_execution())
            .and_then(|_| self.part.verify_borrowed_io())
            .and_then(|_| {
                ensure!(
                    self.part.file.inspect(|file| platform::stamp(file))? == self.stamp,
                    "Owned metadata copy changed outside its authorized writer"
                );
                Ok(())
            })
            .map_err(|_| {
                io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "Owned metadata copy authority or identity changed",
                )
            })
    }
    fn refresh(&mut self) -> io::Result<()> {
        self.stamp = self
            .part
            .file
            .inspect(|file| platform::stamp(file))
            .map_err(|_| {
                io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "Cannot inspect owned metadata copy stamp",
                )
            })?;
        self.check()
    }
    fn reserve_length(&self, bytes: u64) -> io::Result<()> {
        if bytes > MAX_DOWNLOAD_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Owned metadata copy exceeds byte bound",
            ));
        }
        if let Some(quota) = &self.part.quota {
            quota
                .update(&self.part.record.identity, bytes)
                .map_err(|_| {
                    io::Error::new(
                        io::ErrorKind::StorageFull,
                        "Owned metadata copy quota exhausted",
                    )
                })?;
        }
        Ok(())
    }
    pub fn sync_data(&mut self) -> io::Result<()> {
        self.check()?;
        self.part.verify().map_err(|_| {
            io::Error::new(
                io::ErrorKind::PermissionDenied,
                "Owned metadata copy final namespace proof failed",
            )
        })?;
        self.part.file.sync_all()?;
        self.part.verify().map_err(|_| {
            io::Error::new(
                io::ErrorKind::PermissionDenied,
                "Owned metadata copy namespace changed during synchronization",
            )
        })?;
        self.refresh()
    }
}
impl Read for OwnedFileIo<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.check()?;
        let limit = buffer.len().min(COPY_BUFFER_BYTES);
        let length = self.part.file.read(&mut buffer[..limit])?;
        if let Err(error) = self.check() {
            buffer[..length].fill(0);
            return Err(error);
        }
        Ok(length)
    }
}
impl Write for OwnedFileIo<'_> {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.check()?;
        let limit = buffer.len().min(COPY_BUFFER_BYTES);
        let position = self.part.file.stream_position()?;
        let final_length = position
            .checked_add(limit as u64)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "Owned metadata copy position overflow",
                )
            })?
            .max(self.part.file.metadata()?.len());
        self.reserve_length(final_length)?;
        let length = self.part.file.write(&buffer[..limit])?;
        self.refresh()?;
        if let Some(quota) = &self.part.quota {
            quota
                .update(&self.part.record.identity, self.part.file.metadata()?.len())
                .map_err(|_| {
                    io::Error::new(
                        io::ErrorKind::StorageFull,
                        "Owned metadata copy quota exhausted",
                    )
                })?;
        }
        Ok(length)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.sync_data()
    }
}
impl Seek for OwnedFileIo<'_> {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        self.check()?;
        let requested = match from {
            SeekFrom::Start(position) => position as i128,
            SeekFrom::End(offset) => self.part.file.metadata()?.len() as i128 + offset as i128,
            SeekFrom::Current(offset) => self.part.file.stream_position()? as i128 + offset as i128,
        };
        if !(0..=MAX_DOWNLOAD_BYTES as i128).contains(&requested) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Owned metadata copy seek exceeds its bound",
            ));
        }
        let position = self.part.file.seek(SeekFrom::Start(requested as u64))?;
        self.check()?;
        Ok(position)
    }
}
impl lofty::io::Length for OwnedFileIo<'_> {
    fn len(&self) -> io::Result<u64> {
        self.check()?;
        Ok(self.part.file.metadata()?.len())
    }
}
impl lofty::io::Truncate for OwnedFileIo<'_> {
    fn truncate(&mut self, length: u64) -> io::Result<()> {
        self.check()?;
        self.reserve_length(length)?;
        self.part.file.set_len(length)?;
        self.refresh()
    }
}
impl crate::library::tag_write::EditableTagFile for OwnedFileIo<'_> {
    fn check_write_authority(&self) -> io::Result<()> {
        self.check()
    }
    fn sync_data(&mut self) -> io::Result<()> {
        OwnedFileIo::sync_data(self)
    }
}

#[derive(Clone)]
pub(crate) struct CleanupDirectory {
    grant: DestinationGrant,
}

pub(crate) struct CleanupJournalFile {
    file: File,
    directory: CleanupDirectory,
    name: String,
    identity: FileIdentity,
}

impl CleanupDirectory {
    pub(crate) fn open_profile(path: &Path) -> Result<Self> {
        Ok(Self {
            grant: DestinationGrant::open_selected(path, 1)?,
        })
    }

    pub(crate) fn identity(&self) -> &FileIdentity {
        self.grant.identity()
    }
    pub(crate) fn path(&self) -> &Path {
        self.grant.path()
    }
    pub(crate) fn verify(&self) -> Result<()> {
        self.grant.verify()
    }

    pub(crate) fn entries(&self) -> Result<Vec<String>> {
        self.verify()?;
        let entries = std::fs::read_dir(self.path())?
            .map(|entry| {
                entry?.file_name().into_string().map_err(|_| {
                    anyhow::anyhow!("Cleanup profile contains a non-UTF-8 journal name")
                })
            })
            .collect::<Result<Vec<_>>>()?;
        self.verify()?;
        Ok(entries)
    }

    pub(crate) fn journal_length(&self, name: &str) -> Result<u64> {
        self.verify()?;
        #[cfg(any(windows, unix))]
        {
            let file = platform::open_cleanup_journal(self.grant.parent(), name, false, false)?;
            ensure!(
                platform::link_count(&file)? == 1,
                "Cleanup journal has an unapproved hard link"
            );
            let length = file.metadata()?.len();
            self.verify()?;
            Ok(length)
        }
        #[cfg(not(any(windows, unix)))]
        {
            let _ = name;
            bail!("UnsupportedCapability: durable cleanup journal is unavailable")
        }
    }

    pub(crate) fn open_journal(
        &self,
        name: &str,
        create: bool,
        writable: bool,
    ) -> Result<CleanupJournalFile> {
        self.verify()?;
        #[cfg(any(windows, unix))]
        {
            let file = platform::open_cleanup_journal(self.grant.parent(), name, create, writable)?;
            ensure!(
                platform::link_count(&file)? == 1,
                "Cleanup journal has an unapproved hard link"
            );
            let identity = platform::identity(&file)?;
            let journal = CleanupJournalFile {
                file,
                directory: self.clone(),
                name: name.into(),
                identity,
            };
            journal.verify()?;
            Ok(journal)
        }
        #[cfg(not(any(windows, unix)))]
        {
            let _ = (name, create, writable);
            bail!("UnsupportedCapability: durable cleanup journal is unavailable")
        }
    }
}

impl CleanupJournalFile {
    pub(crate) fn recorded_prepare(
        &self,
        facts: &CleanupPreparedFacts,
        disposition: Option<&CleanupDispositionFacts>,
        profile: &FileIdentity,
        journal: &FileIdentity,
    ) -> Result<NativePreparedAttestation> {
        self.verify()?;
        ensure!(
            self.directory.identity() == profile && &self.identity == journal,
            "Prepared attestation does not belong to its actual private profile and journal objects"
        );
        Ok(NativePreparedAttestation {
            facts: facts.clone(),
            live: false,
            disposition: disposition.cloned(),
        })
    }
    pub(crate) fn identity(&self) -> &FileIdentity {
        &self.identity
    }
    pub(crate) fn length(&self) -> Result<u64> {
        self.verify()?;
        Ok(self.file.metadata()?.len())
    }
    pub(crate) fn verify(&self) -> Result<()> {
        self.directory.verify()?;
        platform::validate(&self.file, false)?;
        ensure!(
            platform::identity(&self.file)? == self.identity
                && platform::link_count(&self.file)? == 1,
            "Cleanup journal physical identity changed"
        );
        platform::verify_file_name(self.directory.grant.parent(), &self.name, &self.identity)?;
        Ok(())
    }

    pub(crate) fn read_bounded(&mut self, limit: usize) -> Result<Vec<u8>> {
        self.verify()?;
        ensure!(
            self.file.metadata()?.len() <= limit as u64,
            "Cleanup journal exceeds its input bound"
        );
        self.file.seek(SeekFrom::Start(0))?;
        let mut bytes = Vec::new();
        (&mut self.file)
            .take(limit as u64 + 1)
            .read_to_end(&mut bytes)?;
        ensure!(
            bytes.len() <= limit,
            "Cleanup journal exceeds its input bound"
        );
        self.verify()?;
        Ok(bytes)
    }

    pub(crate) fn truncate(&mut self, length: u64) -> Result<()> {
        self.verify()?;
        ensure!(
            length <= self.file.metadata()?.len(),
            "Cleanup journal repair cannot grow a file"
        );
        self.file.set_len(length)?;
        self.file.sync_all()?;
        self.verify()
    }

    pub(crate) fn append(&mut self, bytes: &[u8]) -> Result<()> {
        self.verify()?;
        self.file.seek(SeekFrom::End(0))?;
        self.file.write_all(bytes)?;
        self.file.sync_all()?;
        self.verify()
    }

    pub(crate) fn flush(&self) -> Result<()> {
        self.verify()?;
        self.file.sync_all()?;
        self.verify()
    }
}

pub(crate) struct CleanupNativeAbsence {
    record: OwnedPartRecord,
    volume_guid: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CleanupPreparedFacts {
    pub volume_guid: String,
    pub filesystem: String,
    pub raw_record: OwnedPartRecord,
    pub raw_bytes: u64,
    pub raw_sha256: String,
    pub final_output: super::task_model::DownloadArtifactProof,
}

#[derive(Clone)]
pub(crate) struct NativePreparedAttestation {
    facts: CleanupPreparedFacts,
    live: bool,
    // A durable same-handle zero-link witness is required for Unix cold recovery.
    // A missing pathname alone never establishes that its inode was unlinked.
    disposition: Option<CleanupDispositionFacts>,
}

impl NativePreparedAttestation {
    pub(crate) fn facts(&self) -> &CleanupPreparedFacts {
        &self.facts
    }
    pub(crate) fn is_live(&self) -> bool {
        self.live
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CleanupDispositionFacts {
    pub record: OwnedPartRecord,
    pub volume_guid: String,
    pub delete_pending: bool,
    pub number_of_links: u64,
}

pub(crate) struct NativeDispositionWitness {
    facts: CleanupDispositionFacts,
}
impl NativeDispositionWitness {
    pub(crate) fn facts(&self) -> &CleanupDispositionFacts {
        &self.facts
    }
}

#[derive(Clone)]
pub(crate) struct NativeVolumeCalibration {
    volume_guid: String,
    filesystem: String,
    #[cfg(unix)]
    unlinked: Option<FileIdentity>,
}
impl CleanupNativeAbsence {
    pub(crate) fn record(&self) -> &OwnedPartRecord {
        &self.record
    }
    pub(crate) fn volume_guid(&self) -> &str {
        &self.volume_guid
    }
}

pub(crate) enum CleanupOriginalLookup {
    Present(Box<CleanupOriginal>),
    Absent(CleanupAbsentOriginal),
}
pub(crate) struct CleanupAbsentOriginal {
    absence: Option<CleanupNativeAbsence>,
    retained: ProducerFile,
}
impl CleanupAbsentOriginal {
    pub(crate) fn producer_object(&self) -> DownloadProducerObject {
        DownloadProducerObject::from_native_file(self.retained.shared_borrow())
    }
    pub(crate) fn finish_after_join(&mut self) -> Result<CleanupNativeAbsence> {
        let absence = self
            .absence
            .as_ref()
            .context("Cleanup absence was already handed over")?;
        self.retained.close_after_join()?;
        self.retained.release_native_absence(absence)?;
        Ok(self.absence.take().expect("Verified native absence"))
    }
}

pub(crate) struct CleanupOriginal {
    part: Option<OwnedPart>,
    kept_original: Option<ProducerFile>,
    grant: DestinationGrant,
    record: OwnedPartRecord,
    verified_stamp: Option<FileStamp>,
    quota: Option<super::quota::PartialQuota>,
    disposition: Option<CleanupDispositionFacts>,
}

pub(crate) struct CleanupFinalGuard {
    file: CleanupReadFile,
    grant: DestinationGrant,
    proof: super::task_model::DownloadArtifactProof,
    stamp: FileStamp,
}
enum CleanupReadFile {
    Native(File),
    Authorized(crate::library::authority::AuthorizedFile),
}
impl CleanupReadFile {
    fn inspect<R>(&self, inspect: impl FnOnce(&File) -> Result<R>) -> Result<R> {
        match self {
            Self::Native(file) => inspect(file),
            Self::Authorized(file) => file.inspect_native(inspect),
        }
    }
}
impl Read for CleanupReadFile {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        match self {
            Self::Native(file) => file.read(bytes),
            Self::Authorized(file) => file.read(bytes),
        }
    }
}
impl Seek for CleanupReadFile {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        match self {
            Self::Native(file) => file.seek(position),
            Self::Authorized(file) => file.seek(position),
        }
    }
}

impl DestinationGrant {
    pub(crate) fn calibrate_cleanup_volume(&self) -> Result<NativeVolumeCalibration> {
        self.verify()?;
        #[cfg(windows)]
        {
            let volume_guid = platform::calibrate_cleanup_volume(self.parent())?;
            self.verify()?;
            Ok(NativeVolumeCalibration {
                volume_guid,
                filesystem: "NTFS".into(),
            })
        }
        #[cfg(unix)]
        {
            let volume_guid = platform::cleanup_volume_guid(self.parent())?;
            self.verify()?;
            Ok(NativeVolumeCalibration {
                volume_guid,
                filesystem: "POSIX-UNLINK".into(),
                unlinked: None,
            })
        }
    }

    pub(crate) fn cleanup_attested_absence(
        &self,
        record: &OwnedPartRecord,
        attestation: &NativePreparedAttestation,
        calibration: &NativeVolumeCalibration,
    ) -> Result<CleanupNativeAbsence> {
        self.verify()?;
        ensure!(
            attestation.facts.raw_record == *record
                && calibration.volume_guid == attestation.facts.volume_guid
                && calibration.filesystem == attestation.facts.filesystem,
            "Cleanup absence has no native prepared identity and same-volume calibration"
        );
        #[cfg(windows)]
        {
            platform::prove_attested_cleanup_absent(
                self.parent(),
                &record.identity,
                &calibration.volume_guid,
            )?;
            self.verify()?;
            Ok(CleanupNativeAbsence {
                record: record.clone(),
                volume_guid: calibration.volume_guid.clone(),
            })
        }
        #[cfg(unix)]
        {
            let disposition = attestation.disposition.as_ref().context(
                "Unix cleanup has no durable zero-link witness; a missing or moved pathname is insufficient",
            )?;
            ensure!(
                calibration.filesystem == "POSIX-UNLINK"
                    && calibration.volume_guid == platform::cleanup_volume_guid(self.parent())?
                    && disposition.record == *record
                    && disposition.volume_guid == calibration.volume_guid
                    && disposition.delete_pending
                    && disposition.number_of_links == 0,
                "Unix cleanup disposition differs from its original inode or mounted device"
            );
            // The journal is bound to its actual profile and file identities. Its
            // fsynced zero-link witness survives a process crash; merely prepared
            // metadata does not. Reused nonce names are never opened for deletion.
            self.verify()?;
            Ok(CleanupNativeAbsence {
                record: record.clone(),
                volume_guid: calibration.volume_guid.clone(),
            })
        }
    }
    pub(crate) fn cleanup_scope(
        &self,
        allocation: &super::artifact::OwnedArtifactAllocation,
    ) -> Result<Self> {
        self.verify()?;
        ensure!(
            self.0.parent.is_none()
                && self.identity() == &allocation.selected_root_identity
                && self.epoch() == allocation.record.destination_epoch,
            "Cleanup recovery requires its current explicitly selected root"
        );
        let scope = if let Some(group) = &allocation.relative_group {
            self.open_existing_group(group)?
        } else {
            self.clone()
        };
        scope.cleanup_matches_allocation(allocation)?;
        Ok(scope)
    }

    pub(crate) fn cleanup_matches_allocation(
        &self,
        allocation: &super::artifact::OwnedArtifactAllocation,
    ) -> Result<()> {
        allocation.validate()?;
        self.verify()?;
        let group = self.0.parent.as_ref().and_then(|_| {
            self.path()
                .file_name()
                .and_then(|name| name.to_str())
                .map(str::to_owned)
        });
        ensure!(
            self.identity() == &allocation.selected_root_identity
                && self.epoch() == allocation.record.destination_epoch
                && self.working_directory_identity() == &allocation.working_directory_identity
                && group == allocation.relative_group
                && crate::domain::music::normalize_local_path(self.path())
                    == crate::domain::music::normalize_local_path(
                        &allocation.working_directory_path
                    ),
            "Cleanup allocation differs from its actual selected namespace"
        );
        Ok(())
    }

    pub(crate) fn bind_cleanup_final(
        &self,
        proof: &super::task_model::DownloadArtifactProof,
        cancel: &DownloadCancel,
    ) -> Result<CleanupFinalGuard> {
        proof.validate()?;
        cancel.check()?;
        self.verify()?;
        let mut file = platform::open_relative_readonly(self.parent(), &proof.basename)?;
        platform::validate(&file, false)?;
        ensure!(
            platform::identity(&file)? == proof.identity
                && platform::link_count(&file)? == 1
                && file.metadata()?.len() == proof.bytes,
            "Cleanup final-file physical proof differs"
        );
        let stamp = platform::stamp(&file)?;
        let (bytes, sha256) = cleanup_whole_hash(&mut file, self, &stamp, cancel)?;
        ensure!(
            bytes == proof.bytes && sha256 == proof.sha256,
            "Cleanup final-file whole digest differs"
        );
        let guard = CleanupFinalGuard {
            file: CleanupReadFile::Native(file),
            grant: self.clone(),
            proof: proof.clone(),
            stamp,
        };
        guard.verify(cancel)?;
        Ok(guard)
    }
    pub(crate) fn bind_cleanup_final_authorized(
        &self,
        proof: &super::task_model::DownloadArtifactProof,
        authorization: crate::library::authority::FileAuthorization,
        cancel: &DownloadCancel,
    ) -> Result<CleanupFinalGuard> {
        proof.validate()?;
        cancel.check()?;
        self.verify()?;
        let mut file = CleanupReadFile::Authorized(authorization.open()?);
        let stamp = file.inspect(|native| {
            platform::validate(native, false)?;
            ensure!(
                platform::identity(native)? == proof.identity
                    && platform::link_count(native)? == 1
                    && native.metadata()?.len() == proof.bytes,
                "Cleanup authorized final-file actual identity or length differs"
            );
            platform::verify_file_name(self.parent(), &proof.basename, &proof.identity)?;
            platform::stamp(native)
        })?;
        let (bytes, sha256) = cleanup_whole_hash(&mut file, self, &stamp, cancel)?;
        ensure!(
            bytes == proof.bytes && sha256 == proof.sha256,
            "Cleanup authorized final-file whole digest differs"
        );
        let guard = CleanupFinalGuard {
            file,
            grant: self.clone(),
            proof: proof.clone(),
            stamp,
        };
        guard.verify(cancel)?;
        Ok(guard)
    }
}

impl CleanupFinalGuard {
    pub(crate) fn verify(&self, cancel: &DownloadCancel) -> Result<()> {
        cancel.check()?;
        self.grant.verify()?;
        self.file.inspect(|native| {
            platform::validate(native, false)?;
            ensure!(
                platform::identity(native)? == self.proof.identity
                    && platform::link_count(native)? == 1
                    && platform::stamp(native)? == self.stamp,
                "Cleanup final-file fence changed"
            );
            Ok(())
        })?;
        platform::verify_file_name(
            self.grant.parent(),
            &self.proof.basename,
            &self.proof.identity,
        )?;
        Ok(())
    }
}

impl CleanupOriginal {
    pub(crate) fn producer_object(&self) -> Option<DownloadProducerObject> {
        self.kept_original
            .as_ref()
            .map(|file| DownloadProducerObject::from_native_file(file.shared_borrow()))
    }
    pub(crate) fn from_live(audio: VerifiedOwnedAudio) -> Self {
        let part = audio.part;
        Self {
            grant: part.plan.grant.clone(),
            record: part.record.clone(),
            quota: part.quota.clone(),
            kept_original: Some(part.file.shared_borrow()),
            part: Some(part),
            verified_stamp: None,
            disposition: None,
        }
    }

    pub(crate) fn grant(&self) -> &DestinationGrant {
        &self.grant
    }
    pub(crate) fn record(&self) -> &OwnedPartRecord {
        &self.record
    }
    pub(crate) fn is_closed(&self) -> bool {
        self.part.is_none()
    }
    pub(crate) fn disposition_requested(&self) -> bool {
        self.disposition.is_some()
    }

    pub(crate) fn reopen(
        grant: &DestinationGrant,
        record: &OwnedPartRecord,
        attestation: Option<&NativePreparedAttestation>,
        calibration: Option<&NativeVolumeCalibration>,
    ) -> Result<CleanupOriginalLookup> {
        grant.verify()?;
        ensure!(
            record.destination_identity == *grant.identity()
                && record.destination_epoch == grant.epoch(),
            "Cleanup original differs from its actual selected root"
        );
        validate_basename(&record.basename)?;
        validate_basename(&record.final_basename)?;
        ensure!(
            record.nonce.len() == 32
                && record.nonce.bytes().all(|byte| byte.is_ascii_hexdigit())
                && record.basename == format!(".lx-download-{}.part", record.nonce),
            "Invalid cleanup original nonce"
        );
        let plan = DownloadPlan {
            grant: grant.clone(),
            basename: record.final_basename.clone(),
            policy: CollisionPolicy::Skip,
            root_identity: grant.identity().clone(),
            epoch: grant.epoch(),
        };
        let file = if let Some(file) = ProducerFile::retain_original(&plan, record)? {
            if file.metadata().is_err() {
                if let Err(error) = file.reopen_closed(&plan, record) {
                    if !is_not_found(&error) {
                        return Err(error)
                            .context("Cannot exclusively reopen original cleanup object");
                    }
                    let absence = grant.cleanup_attested_absence(
                        record,
                        attestation.context(
                            "Missing original has no durable native prepared ID evidence",
                        )?,
                        calibration
                            .context("Missing original has no fresh actual-volume calibration")?,
                    )?;
                    return Ok(CleanupOriginalLookup::Absent(CleanupAbsentOriginal {
                        absence: Some(absence),
                        retained: file,
                    }));
                }
            }
            file
        } else {
            let file = ProducerFile::reserve(
                plan.clone(),
                record.nonce.clone(),
                super::artifact::OwnedArtifactKind::Transfer,
                None,
                MAX_DOWNLOAD_BYTES,
            )?;
            if let Err(error) = file.reopen() {
                if !is_not_found(&error) {
                    return Err(error).context("Cannot exclusively reopen original cleanup object");
                }
                let absence = grant.cleanup_attested_absence(
                    record,
                    attestation
                        .context("Missing original has no durable native prepared ID evidence")?,
                    calibration
                        .context("Missing original has no fresh actual-volume calibration")?,
                )?;
                return Ok(CleanupOriginalLookup::Absent(CleanupAbsentOriginal {
                    absence: Some(absence),
                    retained: file,
                }));
            }
            file
        };
        if file.inspect(platform::identity)? != record.identity {
            let absence = grant.cleanup_attested_absence(
                record,
                attestation
                    .context("Foreign nonce occupant has no durable native prepared ID evidence")?,
                calibration
                    .context("Foreign nonce occupant has no fresh actual-volume calibration")?,
            )?;
            return Ok(CleanupOriginalLookup::Absent(CleanupAbsentOriginal {
                absence: Some(absence),
                retained: file,
            }));
        }
        #[cfg(unix)]
        if file.inspect(platform::link_count)? == 0 {
            let absence = grant.cleanup_attested_absence(
                record,
                attestation.context("Unlinked original has no durable preparation")?,
                calibration.context("Unlinked original has no device calibration")?,
            )?;
            return Ok(CleanupOriginalLookup::Absent(CleanupAbsentOriginal {
                absence: Some(absence),
                retained: file,
            }));
        }
        file.remember_record(record.clone())?;
        file.reconcile_bytes()?;
        let part = OwnedPart {
            plan,
            record: record.clone(),
            file,
            quota: None,
            integrity: None,
            execution: None,
        };
        Ok(CleanupOriginalLookup::Present(Box::new(Self {
            kept_original: Some(part.file.shared_borrow()),
            part: Some(part),
            grant: grant.clone(),
            record: record.clone(),
            verified_stamp: None,
            quota: None,
            disposition: None,
        })))
    }

    pub(crate) fn verify_raw(
        &mut self,
        expected_bytes: u64,
        expected_sha256: &str,
        cancel: &DownloadCancel,
    ) -> Result<()> {
        ensure!(
            expected_bytes > 0 && expected_bytes <= MAX_DOWNLOAD_BYTES,
            "Cleanup original byte bound is invalid"
        );
        cancel.check()?;
        let part = self
            .part
            .as_mut()
            .context("Cleanup original has already been closed")?;
        part.verify()?;
        ensure!(
            part.record == self.record && part.file.metadata()?.len() == expected_bytes,
            "Cleanup original identity or length differs"
        );
        let stamp = part.file.inspect(|file| platform::stamp(file))?;
        let (bytes, sha256) = cleanup_whole_hash(&mut part.file, &self.grant, &stamp, cancel)?;
        ensure!(
            bytes == expected_bytes && sha256 == expected_sha256,
            "Cleanup original whole digest differs"
        );
        part.verify()?;
        self.verified_stamp = Some(stamp);
        Ok(())
    }

    pub(crate) fn attest_prepared(
        &self,
        final_guard: &CleanupFinalGuard,
        raw_bytes: u64,
        raw_sha256: &str,
        cancel: &DownloadCancel,
    ) -> Result<NativePreparedAttestation> {
        cancel.check()?;
        final_guard.verify(cancel)?;
        self.grant.verify()?;
        let part = self
            .part
            .as_ref()
            .context("Native preparation has no held original")?;
        part.verify()?;
        ensure!(
            self.verified_stamp.as_ref() == Some(&part.file.inspect(|file| platform::stamp(file))?)
                && part.record == self.record
                && part.file.metadata()?.len() == raw_bytes
                && final_guard.proof.identity != self.record.identity,
            "Native preparation has no stable actual raw and different final-file proof"
        );
        #[cfg(windows)]
        {
            let volume_guid = platform::cleanup_volume_guid(self.grant.parent())?;
            ensure!(
                platform::cleanup_filesystem(self.grant.parent())? == "NTFS",
                "UnsupportedCapability: original cleanup requires calibrated NTFS semantics"
            );
            Ok(NativePreparedAttestation {
                facts: CleanupPreparedFacts {
                    volume_guid,
                    filesystem: "NTFS".into(),
                    raw_record: self.record.clone(),
                    raw_bytes,
                    raw_sha256: raw_sha256.into(),
                    final_output: final_guard.proof.clone(),
                },
                live: true,
                disposition: None,
            })
        }
        #[cfg(unix)]
        {
            Ok(NativePreparedAttestation {
                facts: CleanupPreparedFacts {
                    volume_guid: platform::cleanup_volume_guid(self.grant.parent())?,
                    filesystem: "POSIX-UNLINK".into(),
                    raw_record: self.record.clone(),
                    raw_bytes,
                    raw_sha256: raw_sha256.into(),
                    final_output: final_guard.proof.clone(),
                },
                live: true,
                disposition: None,
            })
        }
    }

    pub(crate) fn request_cleanup_disposition(
        &mut self,
        attestation: &NativePreparedAttestation,
        cancel: &DownloadCancel,
    ) -> Result<NativeDispositionWitness> {
        cancel.check()?;
        self.grant.verify()?;
        ensure!(
            attestation.facts.raw_record == self.record,
            "Cleanup disposition attestation differs from its original"
        );
        let part = self
            .part
            .as_ref()
            .context("Cleanup disposition has no actual held file")?;
        if self.disposition.is_none() {
            part.verify()?;
            ensure!(
                self.verified_stamp.as_ref()
                    == Some(&part.file.inspect(|file| platform::stamp(file))?),
                "Original changed before native disposition"
            );
            part.file.inspect(|file| {
                platform::remove_owned(
                    file,
                    self.grant.parent(),
                    &self.record.basename,
                    &self.record.identity,
                )
            })?;
            // Preserve this state before any subsequent fallible inspection;
            // the original must never be re-opened or hashed by name afterward.
            self.disposition = Some(CleanupDispositionFacts {
                record: self.record.clone(),
                volume_guid: attestation.facts.volume_guid.clone(),
                delete_pending: true,
                number_of_links: 0,
            });
        }
        #[cfg(windows)]
        {
            let (pending, links) = part
                .file
                .inspect(|file| platform::cleanup_disposition_state(file))?;
            ensure!(
                pending
                    && links == 0
                    && part.file.inspect(|file| platform::identity(file))? == self.record.identity,
                "Original disposition has not produced a same-handle delete-pending zero-link witness"
            );
            let facts = CleanupDispositionFacts {
                record: self.record.clone(),
                volume_guid: attestation.facts.volume_guid.clone(),
                delete_pending: pending,
                number_of_links: links,
            };
            self.disposition = Some(facts.clone());
            Ok(NativeDispositionWitness { facts })
        }
        #[cfg(unix)]
        {
            let links = part.file.inspect(platform::link_count)?;
            ensure!(
                links == 0 && part.file.inspect(platform::identity)? == self.record.identity,
                "Original unlink has no same-descriptor zero-link inode witness"
            );
            let facts = CleanupDispositionFacts {
                record: self.record.clone(),
                volume_guid: attestation.facts.volume_guid.clone(),
                delete_pending: true,
                number_of_links: links,
            };
            self.disposition = Some(facts.clone());
            Ok(NativeDispositionWitness { facts })
        }
    }

    pub(crate) fn close_disposition(&mut self, witness: &NativeDispositionWitness) -> Result<()> {
        ensure!(
            self.disposition.as_ref() == Some(&witness.facts)
                && witness.facts.record == self.record,
            "Original close has no matching actual native disposition witness"
        );
        if let Some(part) = self.part.as_ref() {
            part.file.close_after_join()?;
        }
        self.part.take();
        Ok(())
    }

    pub(crate) fn finish_calibrated_absence(
        &mut self,
        attestation: &NativePreparedAttestation,
        calibration: &NativeVolumeCalibration,
    ) -> Result<CleanupNativeAbsence> {
        ensure!(
            self.part.is_none(),
            "Original must actually close before calibrated absence"
        );
        let mut closed_attestation = attestation.clone();
        closed_attestation.disposition = self.disposition.clone();
        let absence =
            self.grant
                .cleanup_attested_absence(&self.record, &closed_attestation, calibration)?;
        if let Some(file) = self.kept_original.as_ref() {
            file.release_native_absence(&absence)?;
        }
        self.kept_original.take();
        if let Some(quota) = self.quota.take() {
            quota.remove(&self.record.identity);
        }
        Ok(absence)
    }
}

trait CleanupHashFile: Read + Seek {
    fn native_stamp(&self) -> Result<FileStamp>;
}
impl CleanupHashFile for File {
    fn native_stamp(&self) -> Result<FileStamp> {
        platform::stamp(self)
    }
}
impl CleanupHashFile for ProducerFile {
    fn native_stamp(&self) -> Result<FileStamp> {
        self.inspect(platform::stamp)
    }
}
impl CleanupHashFile for CleanupReadFile {
    fn native_stamp(&self) -> Result<FileStamp> {
        self.inspect(platform::stamp)
    }
}
fn cleanup_whole_hash<F: CleanupHashFile>(
    file: &mut F,
    grant: &DestinationGrant,
    stamp: &FileStamp,
    cancel: &DownloadCancel,
) -> Result<(u64, String)> {
    file.seek(SeekFrom::Start(0))?;
    let mut hasher = Sha256::new();
    let mut count = 0u64;
    let mut buffer = [0u8; COPY_BUFFER_BYTES];
    loop {
        cancel.check()?;
        grant.verify()?;
        ensure!(
            &file.native_stamp()? == stamp,
            "Cleanup object changed before its actual read"
        );
        let read = file.read(&mut buffer)?;
        cancel.check()?;
        grant.verify()?;
        ensure!(
            &file.native_stamp()? == stamp,
            "Cleanup object changed during its actual read"
        );
        if read == 0 {
            break;
        }
        count = count
            .checked_add(read as u64)
            .context("Cleanup whole-file byte count overflow")?;
        ensure!(
            count <= MAX_DOWNLOAD_BYTES && count <= stamp.length,
            "Cleanup object exceeds its whole-file bound"
        );
        hasher.update(&buffer[..read]);
    }
    ensure!(
        count == stamp.length,
        "Cleanup object ended before its actual length"
    );
    Ok((count, format!("{:x}", hasher.finalize())))
}

#[cfg(windows)]
mod platform {
    use super::*;
    use std::os::windows::{
        ffi::OsStrExt,
        io::{AsRawHandle, FromRawHandle},
    };
    use windows_sys::{
        Wdk::{
            Foundation::OBJECT_ATTRIBUTES,
            Storage::FileSystem::{
                FILE_CREATE, FILE_DIRECTORY_FILE, FILE_NON_DIRECTORY_FILE, FILE_OPEN,
                FILE_OPEN_REPARSE_POINT, FILE_SYNCHRONOUS_IO_NONALERT, FileRenameInformation,
                NtCreateFile, NtSetInformationFile,
            },
        },
        Win32::{
            Foundation::{
                GENERIC_READ, GENERIC_WRITE, INVALID_HANDLE_VALUE, OBJ_CASE_INSENSITIVE,
                RtlNtStatusToDosError, UNICODE_STRING,
            },
            Storage::FileSystem::*,
            System::IO::IO_STATUS_BLOCK,
        },
    };
    pub(super) fn open_cleanup_journal(
        parent: &File,
        name: &str,
        create: bool,
        writable: bool,
    ) -> Result<File> {
        let mut text: Vec<u16> = name.encode_utf16().collect();
        ensure!(
            !text.is_empty() && text.len() <= 240 && !name.contains(['\\', '/', ':']),
            "Invalid private cleanup journal basename"
        );
        let name = UNICODE_STRING {
            Length: (text.len() * 2) as u16,
            MaximumLength: (text.len() * 2) as u16,
            Buffer: text.as_mut_ptr(),
        };
        let attributes = OBJECT_ATTRIBUTES {
            Length: std::mem::size_of::<OBJECT_ATTRIBUTES>() as u32,
            RootDirectory: parent.as_raw_handle(),
            ObjectName: &name,
            Attributes: OBJ_CASE_INSENSITIVE,
            SecurityDescriptor: std::ptr::null(),
            SecurityQualityOfService: std::ptr::null(),
        };
        let mut handle = std::ptr::null_mut();
        let mut status_block = IO_STATUS_BLOCK::default();
        let status = unsafe {
            NtCreateFile(
                &mut handle,
                GENERIC_READ | SYNCHRONIZE | if writable { GENERIC_WRITE } else { 0 },
                &attributes,
                &mut status_block,
                std::ptr::null(),
                0,
                if writable {
                    FILE_SHARE_READ
                } else {
                    FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE
                },
                if create { FILE_CREATE } else { FILE_OPEN },
                FILE_OPEN_REPARSE_POINT | FILE_SYNCHRONOUS_IO_NONALERT | FILE_NON_DIRECTORY_FILE,
                std::ptr::null(),
                0,
            )
        };
        if status < 0 {
            return Err(io::Error::from_raw_os_error(
                unsafe { RtlNtStatusToDosError(status) } as i32
            )
            .into());
        }
        ensure!(
            !handle.is_null() && handle != INVALID_HANDLE_VALUE,
            "Invalid private cleanup journal handle"
        );
        let file = unsafe { File::from_raw_handle(handle) };
        validate(&file, false)?;
        Ok(file)
    }

    pub(super) fn cleanup_volume_guid(file: &File) -> Result<String> {
        let mut buffer = vec![0u16; 32768];
        let count = unsafe {
            GetFinalPathNameByHandleW(
                file.as_raw_handle(),
                buffer.as_mut_ptr(),
                buffer.len() as u32,
                FILE_NAME_NORMALIZED | VOLUME_NAME_GUID,
            )
        };
        ensure!(
            count > 0 && (count as usize) < buffer.len(),
            "UnsupportedCapability: cannot inspect actual cleanup volume GUID"
        );
        let path = String::from_utf16(&buffer[..count as usize])?;
        ensure!(
            path.starts_with("\\\\?\\Volume{"),
            "UnsupportedCapability: cleanup volume does not expose a native GUID path"
        );
        let end = path
            .find("}\\")
            .context("Invalid actual cleanup volume GUID")?
            + 2;
        Ok(path[..end].to_ascii_lowercase())
    }

    pub(super) fn cleanup_filesystem(hint: &File) -> Result<String> {
        let mut filesystem = [0u16; 256];
        let mut serial = 0;
        let mut maximum = 0;
        let mut flags = 0;
        ensure!(
            unsafe {
                GetVolumeInformationByHandleW(
                    hint.as_raw_handle(),
                    std::ptr::null_mut(),
                    0,
                    &mut serial,
                    &mut maximum,
                    &mut flags,
                    filesystem.as_mut_ptr(),
                    filesystem.len() as u32,
                )
            } != 0,
            "UnsupportedCapability: cannot identify the actual cleanup filesystem"
        );
        let length = filesystem
            .iter()
            .position(|unit| *unit == 0)
            .context("Unbounded cleanup filesystem name")?;
        Ok(String::from_utf16(&filesystem[..length])?)
    }

    pub(super) fn cleanup_disposition_state(file: &File) -> Result<(bool, u64)> {
        let mut info = FILE_STANDARD_INFO::default();
        ensure!(
            unsafe {
                GetFileInformationByHandleEx(
                    file.as_raw_handle(),
                    FileStandardInfo,
                    &mut info as *mut _ as *mut _,
                    std::mem::size_of_val(&info) as u32,
                )
            } != 0,
            "Cannot inspect actual cleanup disposition state"
        );
        Ok((info.DeletePending, info.NumberOfLinks as u64))
    }

    fn cleanup_nt_lookup(
        hint: &File,
        identifier: [u8; 16],
        length: usize,
    ) -> std::result::Result<File, i32> {
        let mut buffer = [0u16; 8];
        for (index, unit) in buffer.iter_mut().enumerate() {
            *unit = u16::from_le_bytes([identifier[index * 2], identifier[index * 2 + 1]]);
        }
        let name = UNICODE_STRING {
            Length: length as u16,
            MaximumLength: length as u16,
            Buffer: buffer.as_mut_ptr(),
        };
        let attributes = OBJECT_ATTRIBUTES {
            Length: std::mem::size_of::<OBJECT_ATTRIBUTES>() as u32,
            RootDirectory: hint.as_raw_handle(),
            ObjectName: &name,
            Attributes: OBJ_CASE_INSENSITIVE,
            SecurityDescriptor: std::ptr::null(),
            SecurityQualityOfService: std::ptr::null(),
        };
        let mut handle = std::ptr::null_mut();
        let mut block = IO_STATUS_BLOCK::default();
        let status = unsafe {
            NtCreateFile(
                &mut handle,
                FILE_READ_ATTRIBUTES | SYNCHRONIZE,
                &attributes,
                &mut block,
                std::ptr::null(),
                0,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                FILE_OPEN,
                windows_sys::Wdk::Storage::FileSystem::FILE_OPEN_BY_FILE_ID
                    | FILE_OPEN_REPARSE_POINT
                    | FILE_SYNCHRONOUS_IO_NONALERT,
                std::ptr::null(),
                0,
            )
        };
        if status < 0 {
            return Err(status);
        }
        if handle.is_null() || handle == INVALID_HANDLE_VALUE {
            return Err(0xc0000008u32 as i32);
        }
        Ok(unsafe { File::from_raw_handle(handle) })
    }

    fn cleanup_check_present(
        hint: &File,
        identifier: [u8; 16],
        length: usize,
        expected: &FileIdentity,
        directory: bool,
        volume: &str,
    ) -> Result<()> {
        let file = cleanup_nt_lookup(hint, identifier, length).map_err(|status| {
            io::Error::from_raw_os_error(unsafe { RtlNtStatusToDosError(status) } as i32)
        })?;
        validate(&file, directory)?;
        ensure!(
            identity(&file)? == *expected && cleanup_volume_guid(&file)? == volume,
            "Cleanup native present control differs from its full actual ID or volume"
        );
        Ok(())
    }

    fn cleanup_create_calibration(parent: &File, name: &str) -> Result<File> {
        let mut buffer: Vec<u16> = name.encode_utf16().collect();
        let name = UNICODE_STRING {
            Length: (buffer.len() * 2) as u16,
            MaximumLength: (buffer.len() * 2) as u16,
            Buffer: buffer.as_mut_ptr(),
        };
        let attributes = OBJECT_ATTRIBUTES {
            Length: std::mem::size_of::<OBJECT_ATTRIBUTES>() as u32,
            RootDirectory: parent.as_raw_handle(),
            ObjectName: &name,
            Attributes: OBJ_CASE_INSENSITIVE,
            SecurityDescriptor: std::ptr::null(),
            SecurityQualityOfService: std::ptr::null(),
        };
        let mut handle = std::ptr::null_mut();
        let mut block = IO_STATUS_BLOCK::default();
        let status = unsafe {
            NtCreateFile(
                &mut handle,
                GENERIC_READ | GENERIC_WRITE | DELETE | SYNCHRONIZE,
                &attributes,
                &mut block,
                std::ptr::null(),
                0,
                0,
                FILE_CREATE,
                FILE_OPEN_REPARSE_POINT | FILE_SYNCHRONOUS_IO_NONALERT | FILE_NON_DIRECTORY_FILE,
                std::ptr::null(),
                0,
            )
        };
        if status < 0 {
            return Err(io::Error::from_raw_os_error(
                unsafe { RtlNtStatusToDosError(status) } as i32
            )
            .into());
        }
        ensure!(
            !handle.is_null() && handle != INVALID_HANDLE_VALUE,
            "Invalid exclusive calibration handle"
        );
        let file = unsafe { File::from_raw_handle(handle) };
        validate(&file, false)?;
        Ok(file)
    }

    pub(super) fn calibrate_cleanup_volume(parent: &File) -> Result<String> {
        // Serialized calibration has one exclusive zero-byte probe and at most
        // one temporary metadata handle, below the four-handle allowance.
        static CALIBRATION_GATE: Mutex<()> = Mutex::new(());
        let _gate = CALIBRATION_GATE
            .lock()
            .map_err(|_| anyhow::anyhow!("Cleanup calibration gate poisoned"))?;
        validate(parent, true)?;
        ensure!(
            cleanup_filesystem(parent)? == "NTFS",
            "UnsupportedCapability: cleanup absence calibration requires actual NTFS"
        );
        let volume = cleanup_volume_guid(parent)?;
        let known = identity(parent)?;
        let known_id = cleanup_identifier(&known)?;
        ensure!(
            known_id[8..].iter().all(|byte| *byte == 0),
            "UnsupportedCapability: cleanup calibrated ID layout differs"
        );
        for length in [8, 16] {
            cleanup_check_present(parent, known_id, length, &known, true, &volume)?;
        }
        let mut nonce = [0u8; 16];
        getrandom::fill(&mut nonce)
            .map_err(|_| anyhow::anyhow!("Cannot allocate calibration identity"))?;
        let nonce: String = nonce.iter().map(|byte| format!("{byte:02x}")).collect();
        let name = format!(".lx-cleanup-probe-{nonce}.tmp");
        let mut probe = Some(cleanup_create_calibration(parent, &name)?);
        let expected = identity(probe.as_ref().unwrap())?;
        let identifier = cleanup_identifier(&expected)?;
        let result = (|| -> Result<()> {
            ensure!(
                probe.as_ref().unwrap().metadata()?.len() == 0
                    && link_count(probe.as_ref().unwrap())? == 1
                    && expected.device == known.device
                    && identifier[8..].iter().all(|byte| *byte == 0),
                "Calibration probe is not its own exclusive zero-byte object"
            );
            for length in [8, 16] {
                cleanup_check_present(parent, identifier, length, &expected, false, &volume)?;
            }
            let reader = cleanup_nt_lookup(parent, identifier, 16).map_err(|status| {
                io::Error::from_raw_os_error(unsafe { RtlNtStatusToDosError(status) } as i32)
            })?;
            ensure!(
                identity(&reader)? == expected && cleanup_volume_guid(&reader)? == volume,
                "Calibration delayed-reader control has a different actual object"
            );
            remove_owned(probe.as_ref().unwrap(), parent, &name, &expected)?;
            let (pending, links) = cleanup_disposition_state(probe.as_ref().unwrap())?;
            ensure!(
                pending && links == 0 && identity(probe.as_ref().unwrap())? == expected,
                "Calibration disposition witness differs"
            );
            for length in [8, 16] {
                ensure!(
                    matches!(cleanup_nt_lookup(parent, identifier, length), Err(status) if status == 0xc0000056u32 as i32),
                    "UnsupportedCapability: calibration delete-pending signature differs"
                );
            }
            drop(probe.take());
            let (pending, links) = cleanup_disposition_state(&reader)?;
            ensure!(
                pending && links == 0 && identity(&reader)? == expected,
                "Calibration extra-reader lost its actual delete-pending object"
            );
            for length in [8, 16] {
                ensure!(
                    matches!(cleanup_nt_lookup(parent, identifier, length), Err(status) if status == 0xc0000056u32 as i32),
                    "UnsupportedCapability: calibration released an object before its last reader close"
                );
            }
            drop(reader);
            for length in [8, 16] {
                ensure!(
                    matches!(cleanup_nt_lookup(parent, identifier, length), Err(status) if status == 0xc000000du32 as i32),
                    "UnsupportedCapability: calibration actual-close signature differs"
                );
            }
            Ok(())
        })();
        if result.is_err()
            && let Some(file) = probe.take()
        {
            let _ = remove_owned(&file, parent, &name, &expected);
            drop(file);
        }
        result?;
        ensure!(
            cleanup_volume_guid(parent)? == volume && identity(parent)? == known,
            "Cleanup volume changed during calibration"
        );
        Ok(volume)
    }

    pub(super) fn prove_attested_cleanup_absent(
        parent: &File,
        expected: &FileIdentity,
        volume: &str,
    ) -> Result<()> {
        validate(parent, true)?;
        ensure!(
            cleanup_filesystem(parent)? == "NTFS"
                && cleanup_volume_guid(parent)? == volume
                && identity(parent)?.device == expected.device,
            "Attested cleanup absence is on a different actual volume or filesystem"
        );
        let identifier = cleanup_identifier(expected)?;
        ensure!(
            identifier[8..].iter().all(|byte| *byte == 0),
            "UnsupportedCapability: attested original has unsupported high ID bits"
        );
        let known = identity(parent)?;
        let known_id = cleanup_identifier(&known)?;
        for length in [8, 16] {
            cleanup_check_present(parent, known_id, length, &known, true, volume)?;
            match cleanup_nt_lookup(parent, identifier, length) {
                Ok(file) => {
                    validate(&file, false)?;
                    ensure!(
                        identity(&file)? == *expected && cleanup_volume_guid(&file)? == volume,
                        "Original metadata lookup differs from its attested full ID"
                    );
                    bail!(
                        "Original cleanup FileID still exists, including renamed objects or aliases"
                    );
                }
                Err(status) if status == 0xc000000du32 as i32 => {}
                Err(status) => {
                    return Err(io::Error::from_raw_os_error(
                        unsafe { RtlNtStatusToDosError(status) } as i32,
                    ))
                    .context(format!(
                        "Attested original has no calibrated actual-close absence signature (NTSTATUS 0x{:08x})",
                        status as u32
                    ));
                }
            }
        }
        ensure!(
            cleanup_volume_guid(parent)? == volume && identity(parent)? == known,
            "Attested cleanup selected volume changed"
        );
        Ok(())
    }

    fn cleanup_identifier(identity: &FileIdentity) -> Result<[u8; 16]> {
        ensure!(
            identity.object.len() == 32
                && identity.object.bytes().all(|byte| byte.is_ascii_hexdigit()),
            "Cleanup object requires its full actual 128-bit identity"
        );
        let mut identifier = [0u8; 16];
        for (index, byte) in identifier.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&identity.object[index * 2..index * 2 + 2], 16)?;
        }
        Ok(identifier)
    }

    pub(super) fn pin_directory(path: &Path) -> Result<(PathBuf, Vec<File>)> {
        let mut components = path.components();
        let prefix = components.next().context("Destination prefix missing")?;
        ensure!(
            matches!(prefix, Component::Prefix(_))
                && matches!(components.next(), Some(Component::RootDir)),
            "Destination must use an absolute native path"
        );
        let mut current = PathBuf::from(prefix.as_os_str());
        current.push("\\");
        let text: Vec<u16> = current.as_os_str().encode_wide().chain(Some(0)).collect();
        let handle = unsafe {
            CreateFileW(
                text.as_ptr(),
                FILE_READ_ATTRIBUTES | FILE_LIST_DIRECTORY,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                std::ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
                std::ptr::null_mut(),
            )
        };
        ensure!(
            handle != INVALID_HANDLE_VALUE,
            "Cannot pin selected destination volume: {}",
            io::Error::last_os_error()
        );
        let mut pins = vec![unsafe { File::from_raw_handle(handle) }];
        validate(&pins[0], true)?;
        for component in components {
            let Component::Normal(name) = component else {
                bail!("Invalid destination component");
            };
            let name = name
                .to_str()
                .context("Destination component is not UTF-8")?;
            let file = open_relative(pins.last().unwrap(), name, true, false)?;
            current.push(name);
            pins.push(file);
        }
        let actual = current_path(pins.last().unwrap())?;
        ensure!(
            normalize(&actual) == normalize(path),
            "Selected destination namespace differs from its path"
        );
        Ok((actual, pins))
    }
    pub(super) fn create_child_directory(parent: &File, name: &str) -> Result<File> {
        match open_relative(parent, name, true, true) {
            Ok(file) => Ok(file),
            Err(error)
                if error
                    .downcast_ref::<io::Error>()
                    .is_some_and(|error| error.kind() == io::ErrorKind::AlreadyExists) =>
            {
                open_relative(parent, name, true, false)
            }
            Err(error) => Err(error),
        }
    }
    pub(super) fn open_relative_readonly(parent: &File, name: &str) -> Result<File> {
        let mut text: Vec<u16> = name.encode_utf16().collect();
        ensure!(
            !text.is_empty() && text.len() <= 32767 && !name.contains(['\\', '/', ':']),
            "Invalid recovery artifact basename"
        );
        let name = UNICODE_STRING {
            Length: (text.len() * 2) as u16,
            MaximumLength: (text.len() * 2) as u16,
            Buffer: text.as_mut_ptr(),
        };
        let attributes = OBJECT_ATTRIBUTES {
            Length: std::mem::size_of::<OBJECT_ATTRIBUTES>() as u32,
            RootDirectory: parent.as_raw_handle(),
            ObjectName: &name,
            Attributes: OBJ_CASE_INSENSITIVE,
            SecurityDescriptor: std::ptr::null(),
            SecurityQualityOfService: std::ptr::null(),
        };
        let mut handle = std::ptr::null_mut();
        let mut status_block = IO_STATUS_BLOCK::default();
        let status = unsafe {
            NtCreateFile(
                &mut handle,
                GENERIC_READ | SYNCHRONIZE,
                &attributes,
                &mut status_block,
                std::ptr::null(),
                0,
                FILE_SHARE_READ,
                FILE_OPEN,
                FILE_OPEN_REPARSE_POINT | FILE_SYNCHRONOUS_IO_NONALERT | FILE_NON_DIRECTORY_FILE,
                std::ptr::null(),
                0,
            )
        };
        if status < 0 {
            return Err(io::Error::from_raw_os_error(
                unsafe { RtlNtStatusToDosError(status) } as i32
            )
            .into());
        }
        ensure!(
            !handle.is_null() && handle != INVALID_HANDLE_VALUE,
            "Invalid readonly recovery handle"
        );
        let file = unsafe { File::from_raw_handle(handle) };
        validate(&file, false)?;
        Ok(file)
    }
    pub(super) fn verify_removed_owned(
        parent: &File,
        name: &str,
        expected: &FileIdentity,
    ) -> Result<()> {
        match open_relative_readonly(parent, name) {
            Err(error)
                if error
                    .downcast_ref::<io::Error>()
                    .is_some_and(|error| error.kind() == io::ErrorKind::NotFound) =>
            {
                Ok(())
            }
            Ok(file) => {
                ensure!(
                    identity(&file)? != *expected,
                    "Owned deletion is still pending its actual file close"
                );
                Ok(())
            }
            Err(error) => Err(error).context("Cannot confirm actual owned artifact removal"),
        }
    }
    pub(super) fn verify_child_directory(
        parent: &File,
        name: &str,
        expected: &FileIdentity,
    ) -> Result<()> {
        let child = open_relative(parent, name, true, false)?;
        ensure!(
            &identity(&child)? == expected
                && normalize(&current_path(&child)?)
                    == normalize(&current_path(parent)?.join(name)),
            "Grouped download directory identity or namespace changed"
        );
        Ok(())
    }
    fn normalize(path: &Path) -> String {
        let text = path.to_string_lossy();
        let text = text
            .strip_prefix("\\\\?\\UNC\\")
            .map(|s| format!("\\\\{s}"))
            .unwrap_or_else(|| text.strip_prefix("\\\\?\\").unwrap_or(&text).to_owned());
        text.trim_end_matches('\\').to_lowercase()
    }
    fn current_path(file: &File) -> Result<PathBuf> {
        let mut initial = [0u16; 256];
        let mut n = unsafe {
            GetFinalPathNameByHandleW(
                file.as_raw_handle(),
                initial.as_mut_ptr(),
                initial.len() as u32,
                FILE_NAME_NORMALIZED | VOLUME_NAME_DOS,
            )
        };
        if n > 0 && n < initial.len() as u32 {
            return Ok(PathBuf::from(String::from_utf16(&initial[..n as usize])?));
        }
        ensure!(
            n > 0 && n <= 32767,
            "Cannot inspect bounded destination handle namespace"
        );
        let mut bytes = vec![0u16; n as usize + 1];
        n = unsafe {
            GetFinalPathNameByHandleW(
                file.as_raw_handle(),
                bytes.as_mut_ptr(),
                bytes.len() as u32,
                FILE_NAME_NORMALIZED | VOLUME_NAME_DOS,
            )
        };
        ensure!(
            n > 0 && n < bytes.len() as u32,
            "Cannot verify destination handle namespace"
        );
        bytes.truncate(n as usize);
        Ok(PathBuf::from(String::from_utf16(&bytes)?))
    }
    pub(super) fn validate(file: &File, directory: bool) -> Result<()> {
        let mut info = FILE_ATTRIBUTE_TAG_INFO::default();
        ensure!(
            unsafe {
                GetFileInformationByHandleEx(
                    file.as_raw_handle(),
                    FileAttributeTagInfo,
                    &mut info as *mut _ as *mut _,
                    std::mem::size_of_val(&info) as u32,
                )
            } != 0,
            "Cannot inspect destination object"
        );
        ensure!(
            info.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT == 0
                && (info.FileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0) == directory,
            "Destination traversal or object type refused"
        );
        if directory {
            let mut case = FILE_CASE_SENSITIVE_INFO::default();
            ensure!(
                unsafe {
                    GetFileInformationByHandleEx(
                        file.as_raw_handle(),
                        FileCaseSensitiveInfo,
                        &mut case as *mut _ as *mut _,
                        std::mem::size_of_val(&case) as u32,
                    )
                } != 0,
                "UnsupportedCapability: cannot verify destination directory case policy"
            );
            ensure!(
                case.Flags & 1 == 0,
                "UnsupportedCapability: case-sensitive destination directories require exact namespace support"
            );
        }
        Ok(())
    }
    pub(super) fn identity(file: &File) -> Result<FileIdentity> {
        let mut info = FILE_ID_INFO::default();
        ensure!(
            unsafe {
                GetFileInformationByHandleEx(
                    file.as_raw_handle(),
                    FileIdInfo,
                    &mut info as *mut _ as *mut _,
                    std::mem::size_of_val(&info) as u32,
                )
            } != 0,
            "Cannot identify destination object"
        );
        Ok(FileIdentity {
            device: info.VolumeSerialNumber,
            object: info
                .FileId
                .Identifier
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect(),
        })
    }
    pub(super) fn stamp(file: &File) -> Result<FileStamp> {
        let mut info = FILE_BASIC_INFO::default();
        ensure!(
            unsafe {
                GetFileInformationByHandleEx(
                    file.as_raw_handle(),
                    FileBasicInfo,
                    &mut info as *mut _ as *mut _,
                    std::mem::size_of_val(&info) as u32,
                )
            } != 0,
            "Cannot inspect owned download change stamp"
        );
        Ok(FileStamp {
            changed: info.ChangeTime as i128,
            written: info.LastWriteTime as i128,
            length: file.metadata()?.len(),
        })
    }
    pub(super) fn link_count(file: &File) -> Result<u64> {
        let mut info = FILE_STANDARD_INFO::default();
        ensure!(
            unsafe {
                GetFileInformationByHandleEx(
                    file.as_raw_handle(),
                    FileStandardInfo,
                    &mut info as *mut _ as *mut _,
                    std::mem::size_of_val(&info) as u32,
                )
            } != 0,
            "Cannot inspect owned download link count"
        );
        Ok(info.NumberOfLinks as u64)
    }
    pub(super) fn open_relative(
        parent: &File,
        name: &str,
        directory: bool,
        create: bool,
    ) -> Result<File> {
        let mut text: Vec<u16> = name.encode_utf16().collect();
        ensure!(
            !text.is_empty() && text.len() <= 32767 && !name.contains(['\\', '/', ':']),
            "Invalid relative destination name"
        );
        let name = UNICODE_STRING {
            Length: (text.len() * 2) as u16,
            MaximumLength: (text.len() * 2) as u16,
            Buffer: text.as_mut_ptr(),
        };
        let attributes = OBJECT_ATTRIBUTES {
            Length: std::mem::size_of::<OBJECT_ATTRIBUTES>() as u32,
            RootDirectory: parent.as_raw_handle(),
            ObjectName: &name,
            Attributes: OBJ_CASE_INSENSITIVE,
            SecurityDescriptor: std::ptr::null(),
            SecurityQualityOfService: std::ptr::null(),
        };
        let mut handle = std::ptr::null_mut();
        let mut status_block = IO_STATUS_BLOCK::default();
        let status = unsafe {
            NtCreateFile(
                &mut handle,
                (if directory {
                    FILE_READ_ATTRIBUTES | FILE_LIST_DIRECTORY
                } else {
                    GENERIC_READ | GENERIC_WRITE | DELETE
                }) | SYNCHRONIZE,
                &attributes,
                &mut status_block,
                std::ptr::null(),
                0,
                if directory {
                    FILE_SHARE_READ | FILE_SHARE_WRITE
                } else {
                    FILE_SHARE_READ
                },
                if create { FILE_CREATE } else { FILE_OPEN },
                FILE_OPEN_REPARSE_POINT
                    | FILE_SYNCHRONOUS_IO_NONALERT
                    | if directory {
                        FILE_DIRECTORY_FILE
                    } else {
                        FILE_NON_DIRECTORY_FILE
                    },
                std::ptr::null(),
                0,
            )
        };
        if status < 0 {
            return Err(io::Error::from_raw_os_error(
                unsafe { RtlNtStatusToDosError(status) } as i32
            )
            .into());
        }
        ensure!(
            !handle.is_null() && handle != INVALID_HANDLE_VALUE,
            "Invalid destination handle"
        );
        let file = unsafe { File::from_raw_handle(handle) };
        validate(&file, directory)?;
        Ok(file)
    }
    pub(super) fn verify_directory_path(path: &Path, pins: &[File]) -> Result<()> {
        ensure!(
            normalize(&current_path(pins.last().unwrap())?) == normalize(path),
            "Selected destination moved"
        );
        Ok(())
    }
    pub(super) fn verify_file_name(
        parent: &File,
        name: &str,
        expected: &FileIdentity,
    ) -> Result<()> {
        // File handles exclude write/delete sharing; namespace replacement cannot
        // occur while the owner holds its original handle.
        let expected_path = current_path(parent)?.join(name);
        let metadata = std::fs::symlink_metadata(&expected_path)?;
        ensure!(
            metadata.is_file() && !metadata.file_type().is_symlink(),
            "Owned partial namespace changed"
        );
        let text: Vec<u16> = expected_path
            .as_os_str()
            .encode_wide()
            .chain(Some(0))
            .collect();
        let handle = unsafe {
            CreateFileW(
                text.as_ptr(),
                FILE_READ_ATTRIBUTES,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                std::ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_OPEN_REPARSE_POINT,
                std::ptr::null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error().into());
        }
        let proof = unsafe { File::from_raw_handle(handle) };
        validate(&proof, false)?;
        ensure!(
            &identity(&proof)? == expected,
            "Owned partial namespace identity changed"
        );
        Ok(())
    }
    pub(super) fn rename_no_replace(
        file: &File,
        parent: &File,
        _old: &str,
        new: &str,
        expected: &FileIdentity,
    ) -> Result<()> {
        ensure!(
            &identity(file)? == expected,
            "Owned partial changed before commit"
        );
        let text: Vec<u16> = new.encode_utf16().collect();
        let offset = std::mem::offset_of!(FILE_RENAME_INFO, FileName);
        let size = offset + text.len() * 2;
        let mut storage = vec![0u64; size.div_ceil(8)];
        let info = storage.as_mut_ptr() as *mut FILE_RENAME_INFO;
        unsafe {
            (*info).Anonymous.ReplaceIfExists = false;
            (*info).RootDirectory = parent.as_raw_handle();
            (*info).FileNameLength = (text.len() * 2) as u32;
            std::ptr::copy_nonoverlapping(
                text.as_ptr(),
                (info as *mut u8).add(offset) as *mut u16,
                text.len(),
            );
        }
        let mut status_block = IO_STATUS_BLOCK::default();
        let status = unsafe {
            NtSetInformationFile(
                file.as_raw_handle(),
                &mut status_block,
                info as *const _,
                size as u32,
                FileRenameInformation,
            )
        };
        if status < 0 {
            return Err(io::Error::from_raw_os_error(
                unsafe { RtlNtStatusToDosError(status) } as i32,
            ))
            .context("Cannot atomically commit download without overwrite");
        }
        file.sync_all()?;
        Ok(())
    }
    pub(super) fn remove_owned(
        file: &File,
        parent: &File,
        name: &str,
        expected: &FileIdentity,
    ) -> Result<()> {
        verify_file_name(parent, name, expected)?;
        let info = FILE_DISPOSITION_INFO { DeleteFile: true };
        ensure!(
            unsafe {
                SetFileInformationByHandle(
                    file.as_raw_handle(),
                    FileDispositionInfo,
                    &info as *const _ as *const _,
                    std::mem::size_of_val(&info) as u32,
                )
            } != 0,
            "Cannot remove owned partial: {}",
            io::Error::last_os_error()
        );
        Ok(())
    }
}

#[cfg(unix)]
mod platform {
    use super::*;
    use std::{
        ffi::CString,
        os::unix::{
            ffi::OsStrExt,
            fs::MetadataExt,
            io::{AsRawFd, FromRawFd},
        },
    };
    pub(super) fn cleanup_volume_guid(file: &File) -> Result<String> {
        Ok(format!("posix-dev-{:016x}", file.metadata()?.dev()))
    }
    pub(super) fn open_cleanup_journal(
        parent: &File,
        name: &str,
        create: bool,
        writable: bool,
    ) -> Result<File> {
        validate_basename(name)?;
        let name = CString::new(name)?;
        let flags = libc::O_CLOEXEC
            | libc::O_NOFOLLOW
            | libc::O_NONBLOCK
            | if writable {
                libc::O_RDWR
            } else {
                libc::O_RDONLY
            }
            | if create {
                libc::O_CREAT | libc::O_EXCL
            } else {
                0
            };
        let fd = unsafe { libc::openat(parent.as_raw_fd(), name.as_ptr(), flags, 0o600) };
        if fd < 0 {
            return Err(io::Error::last_os_error().into());
        }
        let file = unsafe { File::from_raw_fd(fd) };
        validate(&file, false)?;
        let metadata = file.metadata()?;
        ensure!(
            metadata.uid() == unsafe { libc::geteuid() } && metadata.mode() & 0o077 == 0,
            "Cleanup journal must be private to the current account"
        );
        if create {
            parent.sync_all()?;
        }
        Ok(file)
    }
    pub(super) fn pin_directory(path: &Path) -> Result<(PathBuf, Vec<File>)> {
        let fd = unsafe {
            libc::open(
                c"/".as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
            )
        };
        ensure!(fd >= 0, "Cannot pin destination filesystem");
        let mut pins = vec![unsafe { File::from_raw_fd(fd) }];
        for component in path.components() {
            if let Component::Normal(name) = component {
                pins.push(open_relative(
                    pins.last().unwrap(),
                    name.to_str().context("Destination name is not UTF-8")?,
                    true,
                    false,
                )?);
            }
        }
        Ok((path.to_owned(), pins))
    }
    pub(super) fn open_relative(
        parent: &File,
        name: &str,
        directory: bool,
        create: bool,
    ) -> Result<File> {
        let name = CString::new(name)?;
        let flags = libc::O_CLOEXEC
            | libc::O_NOFOLLOW
            | if directory {
                libc::O_RDONLY | libc::O_DIRECTORY
            } else {
                libc::O_RDWR
            }
            | if create {
                libc::O_CREAT | libc::O_EXCL
            } else {
                0
            };
        let fd = unsafe { libc::openat(parent.as_raw_fd(), name.as_ptr(), flags, 0o600) };
        if fd < 0 {
            return Err(io::Error::last_os_error().into());
        }
        let file = unsafe { File::from_raw_fd(fd) };
        validate(&file, directory)?;
        Ok(file)
    }
    pub(super) fn open_relative_readonly(parent: &File, name: &str) -> Result<File> {
        let name = CString::new(name)?;
        let fd = unsafe {
            libc::openat(
                parent.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error().into());
        }
        let file = unsafe { File::from_raw_fd(fd) };
        validate(&file, false)?;
        Ok(file)
    }
    pub(super) fn verify_removed_owned(
        parent: &File,
        name: &str,
        _expected: &FileIdentity,
    ) -> Result<()> {
        let name = CString::new(name)?;
        let mut result = std::mem::MaybeUninit::<libc::stat>::uninit();
        if unsafe {
            libc::fstatat(
                parent.as_raw_fd(),
                name.as_ptr(),
                result.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        } == 0
        {
            bail!("Download cleanup leaf is still present");
        }
        let error = io::Error::last_os_error();
        ensure!(
            error.kind() == io::ErrorKind::NotFound,
            "Cannot verify download cleanup: {error}"
        );
        Ok(())
    }
    pub(super) fn create_child_directory(parent: &File, name: &str) -> Result<File> {
        let component = CString::new(name)?;
        if unsafe { libc::mkdirat(parent.as_raw_fd(), component.as_ptr(), 0o700) } != 0
            && io::Error::last_os_error().kind() != io::ErrorKind::AlreadyExists
        {
            return Err(io::Error::last_os_error().into());
        }
        open_relative(parent, name, true, false)
    }
    pub(super) fn verify_child_directory(
        parent: &File,
        name: &str,
        expected: &FileIdentity,
    ) -> Result<()> {
        let child = open_relative(parent, name, true, false)?;
        ensure!(
            &identity(&child)? == expected,
            "Grouped download directory identity changed"
        );
        Ok(())
    }
    pub(super) fn validate(file: &File, directory: bool) -> Result<()> {
        let metadata = file.metadata()?;
        ensure!(
            if directory {
                metadata.is_dir()
            } else {
                metadata.is_file()
            },
            "Invalid destination object type"
        );
        Ok(())
    }
    pub(super) fn identity(file: &File) -> Result<FileIdentity> {
        let metadata = file.metadata()?;
        Ok(FileIdentity {
            device: metadata.dev(),
            object: metadata.ino().to_string(),
        })
    }
    pub(super) fn stamp(file: &File) -> Result<FileStamp> {
        let metadata = file.metadata()?;
        Ok(FileStamp {
            changed: metadata.ctime() as i128 * 1_000_000_000 + metadata.ctime_nsec() as i128,
            written: metadata.mtime() as i128 * 1_000_000_000 + metadata.mtime_nsec() as i128,
            length: metadata.len(),
        })
    }
    pub(super) fn link_count(file: &File) -> Result<u64> {
        Ok(file.metadata()?.nlink())
    }
    pub(super) fn verify_directory_path(path: &Path, pins: &[File]) -> Result<()> {
        let mut index = 1;
        for component in path.components() {
            if let Component::Normal(name) = component {
                let proof = open_relative(
                    &pins[index - 1],
                    name.to_str().context("Invalid destination name")?,
                    true,
                    false,
                )?;
                ensure!(
                    identity(&proof)? == identity(&pins[index])?,
                    "Selected destination moved or was replaced"
                );
                index += 1;
            }
        }
        Ok(())
    }
    pub(super) fn verify_file_name(
        parent: &File,
        name: &str,
        expected: &FileIdentity,
    ) -> Result<()> {
        let proof = open_relative(parent, name, false, false)?;
        ensure!(
            &identity(&proof)? == expected,
            "Owned partial namespace identity changed"
        );
        Ok(())
    }
    pub(super) fn rename_no_replace(
        file: &File,
        parent: &File,
        old: &str,
        new: &str,
        expected: &FileIdentity,
    ) -> Result<()> {
        verify_file_name(parent, old, expected)?;
        ensure!(
            &identity(file)? == expected && link_count(file)? == 1,
            "Download source changed before publication"
        );
        let old = CString::new(old)?;
        let new = CString::new(new)?;
        #[cfg(target_os = "linux")]
        let result = unsafe {
            libc::renameat2(
                parent.as_raw_fd(),
                old.as_ptr(),
                parent.as_raw_fd(),
                new.as_ptr(),
                libc::RENAME_NOREPLACE,
            )
        };
        #[cfg(target_os = "macos")]
        let result = unsafe {
            libc::renameatx_np(
                parent.as_raw_fd(),
                old.as_ptr(),
                parent.as_raw_fd(),
                new.as_ptr(),
                libc::RENAME_EXCL,
            )
        };
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        return Err(anyhow::anyhow!(
            "Atomic no-replace publication is unavailable on this Unix target"
        ));
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            if result != 0 {
                return Err(io::Error::last_os_error())
                    .context("Native no-replace download publication failed");
            }
            parent.sync_all()?;
            Ok(())
        }
    }
    pub(super) fn remove_owned(
        file: &File,
        parent: &File,
        name: &str,
        expected: &FileIdentity,
    ) -> Result<()> {
        verify_file_name(parent, name, expected)?;
        ensure!(
            &identity(file)? == expected && link_count(file)? == 1,
            "Download cleanup refuses changed or aliased temporary files"
        );
        let name = CString::new(name)?;
        if unsafe { libc::unlinkat(parent.as_raw_fd(), name.as_ptr(), 0) } != 0 {
            return Err(io::Error::last_os_error())
                .context("Cannot retire owned download temporary file");
        }
        ensure!(
            link_count(file)? == 0,
            "Download namespace changed during retirement"
        );
        parent.sync_all()?;
        Ok(())
    }
}

#[cfg(not(any(windows, unix)))]
compile_error!(
    "Download destination authority requires native handle-relative filesystem operations"
);

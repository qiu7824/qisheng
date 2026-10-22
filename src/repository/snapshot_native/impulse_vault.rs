//! Durable impulse representations inside one profile-owned native namespace.
//!
//! Serialized receipts describe exact objects. They never supply a pathname
//! grant: every open starts from a continuously pinned profile and rechecks the
//! namespace owner, exact file identity, single-link body, header and digest.
//! Failed imports remain journaled and consume the namespace's disk quota.
#[cfg(all(test, windows))]
mod abort_tests;
mod creation;
pub(crate) use creation::ImpulseVaultCreator;
pub use creation::{ImpulseVaultCreationFact, ImpulseVaultCreationPhase};

use super::{
    ActualFile, ActualFileMetadata, ColdCleanup, DurableCleanupFact, FileIdentity, NativeFailure,
    SnapshotContext, SnapshotDirectory, SnapshotLimiter, platform,
};
use crate::audio::{
    effects_factory::OwnedImpulse,
    impulse_decode::{
        ImpulseDecodeContext, ImpulseEncodedInput, MAX_IMPULSE_ENCODED_BYTES, decode_impulse,
    },
    impulse_resource::prepare_selected_impulse,
};
use crate::domain::audio_effects::ImpulseResourceId;
use crate::library::authority::AuthorizedFile;
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs::{self, File},
    io::{Read, Seek, SeekFrom, Write},
    sync::{Arc, Mutex},
};

const DIRECTORY: &str = ".lx-impulse-vault";
const OWNER: &str = "owner.lock";
const OWNER_MAGIC: &[u8; 16] = b"LXIMPULSEVAULT01";
const BODY_MAGIC: &[u8; 16] = b"LXIMPULSEBODY001";
const OWNER_BYTES: usize = 48;
const HEADER_BYTES: usize = 144;
pub const MAX_IMPULSE_VAULT_BYTES: u64 = 64 * 1024 * 1024;
pub const MAX_STORED_IMPULSES: usize = 32;
const MAX_NAMESPACE_ENTRIES: usize = 64;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImpulseFileIdentity {
    pub device: u64,
    pub object: String,
}
impl ImpulseFileIdentity {
    fn from_native(identity: &FileIdentity) -> Self {
        Self {
            device: identity.device,
            object: identity.object.clone(),
        }
    }
    fn matches(&self, identity: &FileIdentity) -> bool {
        self.device == identity.device && self.object == identity.object
    }
    fn validate(&self) -> Result<()> {
        ensure!(
            !self.object.is_empty() && self.object.len() <= 128 && !self.object.contains('\0'),
            "Invalid impulse physical identity"
        );
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImpulseVaultReceipt {
    pub namespace: String,
    pub profile_identity: ImpulseFileIdentity,
    pub directory_identity: ImpulseFileIdentity,
    pub owner_identity: ImpulseFileIdentity,
}
impl ImpulseVaultReceipt {
    pub fn validate(&self) -> Result<()> {
        ensure!(hex(&self.namespace, 32), "Invalid impulse vault namespace");
        self.profile_identity.validate()?;
        self.directory_identity.validate()?;
        self.owner_identity.validate()
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StoredImpulseResource {
    pub resource_id: ImpulseResourceId,
    pub namespace: String,
    pub body_identity: ImpulseFileIdentity,
    pub encoded_sha256: String,
    pub encoded_bytes: u64,
    pub extension: String,
    pub codec: String,
    pub decoded_frames: u64,
    pub sample_rate: u32,
    pub channels: u16,
}
impl StoredImpulseResource {
    pub fn validate(&self) -> Result<()> {
        self.resource_id.validate()?;
        ensure!(
            matches!(&self.resource_id, ImpulseResourceId::Imported(id) if hex(id, 32)),
            "Stored impulse must have an imported representation identity"
        );
        ensure!(
            hex(&self.namespace, 32) && hex(&self.encoded_sha256, 64),
            "Invalid stored impulse representation proof"
        );
        self.body_identity.validate()?;
        ensure!(
            self.encoded_bytes > 0
                && self.encoded_bytes <= MAX_IMPULSE_ENCODED_BYTES as u64
                && matches!(self.extension.as_str(), "wav" | "flac")
                && !self.codec.is_empty()
                && self.codec.len() <= 64
                && (8000..=384000).contains(&self.sample_rate)
                && matches!(self.channels, 1 | 2 | 4)
                && self.decoded_frames > 0
                && self.decoded_frames <= self.sample_rate as u64 * 4,
            "Invalid stored impulse admission"
        );
        Ok(())
    }
    fn token(&self) -> &str {
        match &self.resource_id {
            ImpulseResourceId::Imported(id) => id,
            ImpulseResourceId::Bundled(_) => unreachable!("validated imported resource"),
        }
    }
    fn basename(&self) -> String {
        impulse_basename(self.token())
    }
    pub fn storage_bytes(&self) -> u64 {
        self.encoded_bytes + HEADER_BYTES as u64
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImpulseImportPhase {
    Allocating,
    Writing,
    Sealed,
    Aborting,
    Removed,
}

/// Persisted before allocation and before the first body write. An allocation
/// without an attested FID is unresolved and can never authorize path deletion.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImpulseImportFact {
    pub namespace: String,
    pub token: String,
    pub body_identity: Option<ImpulseFileIdentity>,
    pub reserved_bytes: u64,
    pub expected_sha256: String,
    pub phase: ImpulseImportPhase,
}
impl ImpulseImportFact {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            hex(&self.namespace, 32)
                && hex(&self.token, 32)
                && hex(&self.expected_sha256, 64)
                && self.reserved_bytes > HEADER_BYTES as u64
                && self.reserved_bytes <= MAX_IMPULSE_ENCODED_BYTES as u64 + HEADER_BYTES as u64,
            "Invalid impulse import inventory"
        );
        if let Some(identity) = &self.body_identity {
            identity.validate()?;
        }
        ensure!(
            (self.phase == ImpulseImportPhase::Allocating) == self.body_identity.is_none(),
            "Impulse import phase does not match its physical identity attestation"
        );
        Ok(())
    }
}

pub(crate) struct ImportedImpulse {
    pub resource: StoredImpulseResource,
    pub impulse: Arc<OwnedImpulse>,
}
struct PendingImpulseFile {
    file: Option<File>,
    reserved: u64,
    producer: bool,
}

/// Only the cleanup engine can create a retirement witness. Deserializing one
/// cannot obtain a descriptor or ask the engine to delete a locator.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImpulseRetirementRecord {
    pub namespace: String,
    pub token: String,
    fact: DurableCleanupFact,
}
impl ImpulseRetirementRecord {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            hex(&self.namespace, 32) && hex(&self.token, 32),
            "Invalid impulse retirement identity"
        );
        ensure!(
            self.fact.file.nonce == self.token
                && self.fact.file.operation_id == format!("ir-retire-{}", self.token)
                && self.fact.file.temporary_name.as_deref()
                    == Some(impulse_basename(&self.token).as_str())
                && !self.fact.file.published
                && self.fact.file.final_basename.is_none()
                && self.fact.file.identity.is_some()
                && self.fact.file.bytes <= MAX_IMPULSE_ENCODED_BYTES as u64 + HEADER_BYTES as u64
                && hex(&self.fact.sha256, 64),
            "Impulse retirement witness identifies a different purpose or allocation"
        );
        Ok(())
    }
    pub fn removed(&self) -> bool {
        self.fact.removed
    }
    pub fn pending_zero_links(&self) -> bool {
        self.fact.pending_zero_links()
    }
    pub fn resource_id(&self) -> ImpulseResourceId {
        ImpulseResourceId::Imported(self.token.clone())
    }
    pub fn body_identity(&self) -> ImpulseFileIdentity {
        ImpulseFileIdentity::from_native(
            self.fact
                .file
                .identity
                .as_ref()
                .expect("validated retirement FID"),
        )
    }
}

/// NativeFailure retains the same actual FID and calibration objects until a
/// later attempt confirms their physical close and zero-link retirement.
pub(crate) struct ImpulseRetirementOwner {
    namespace: String,
    token: String,
    owner: Option<ActualFile>,
    failure: Option<NativeFailure>,
    removed: bool,
}
impl ImpulseRetirementOwner {
    pub(crate) fn attempt(
        &mut self,
        context: &SnapshotContext,
        record: &dyn Fn(&ImpulseRetirementRecord) -> Result<()>,
    ) -> Result<bool> {
        if self.removed {
            return Ok(true);
        }
        if self.owner.is_none() {
            self.owner = self
                .failure
                .as_mut()
                .and_then(|failure| failure.file.take())
                .map(|owner| *owner);
        }
        let owner = self
            .owner
            .take()
            .context("Impulse retirement lost its original native owner")?;
        match owner.cleanup_journaled(context, &|fact| {
            let witness = ImpulseRetirementRecord {
                namespace: self.namespace.clone(),
                token: self.token.clone(),
                fact: fact.clone(),
            };
            witness.validate()?;
            record(&witness)
        }) {
            Ok(_) => {
                self.removed = true;
                self.failure = None;
                Ok(true)
            }
            Err(failure) => {
                let message = failure.message.clone();
                self.failure = Some(failure);
                Err(anyhow::anyhow!(message))
            }
        }
    }
    pub(crate) fn can_release_for_recovery(&self) -> bool {
        self.removed
            || self
                .owner
                .as_ref()
                .is_some_and(ActualFile::can_release_for_recovery)
            || self
                .failure
                .as_ref()
                .and_then(|failure| failure.file.as_ref())
                .is_some_and(|owner| owner.can_release_for_recovery())
    }
    /// Explicitly closes the retained original; a failed physical close keeps
    /// the exact descriptor owned here. This releases no stored disk inventory.
    pub(crate) fn release_for_recovery(&mut self) -> Result<bool> {
        if !self.can_release_for_recovery() {
            return Ok(false);
        }
        let original = self.owner.as_mut().or_else(|| {
            self.failure
                .as_mut()
                .and_then(|failure| failure.file.as_mut())
                .map(|owner| owner.as_mut())
        });
        let Some(original) = original else {
            return Ok(self.removed);
        };
        if !original.can_release_for_recovery() {
            return Ok(false);
        }
        #[cfg(windows)]
        platform::close_retained(&mut original.file)?;
        #[cfg(not(windows))]
        drop(original.file.take());
        Ok(original.file.is_none() && original.can_release_for_recovery())
    }
}
pub(crate) struct ColdImpulseRetirement {
    namespace: String,
    token: String,
    owner: ColdCleanup,
}
impl ColdImpulseRetirement {
    pub(crate) fn can_release_for_recovery(&self) -> bool {
        self.owner.can_release_for_recovery()
    }
    pub(crate) fn confirm(&mut self, context: &SnapshotContext) -> Result<ImpulseRetirementRecord> {
        let fact = self.owner.confirm(context)?;
        let record = ImpulseRetirementRecord {
            namespace: self.namespace.clone(),
            token: self.token.clone(),
            fact,
        };
        record.validate()?;
        Ok(record)
    }
}

/// One producer owns the namespace lock. PCM and recipes receive no descriptor.
pub(crate) struct ImpulseVault {
    profile: SnapshotDirectory,
    directory: SnapshotDirectory,
    owner: File,
    receipt: ImpulseVaultReceipt,
    armed: bool,
    retirement_limiter: SnapshotLimiter,
    pending: Arc<Mutex<BTreeMap<String, PendingImpulseFile>>>,
    #[cfg(test)]
    cancel_after_write: Option<u64>,
}
impl ImpulseVault {
    /// New receipts must be persisted, then armed, before any import is allowed.
    #[cfg(all(test, windows))]
    pub(crate) fn create(profile: &SnapshotDirectory) -> Result<Self> {
        profile.verify()?;
        platform::ensure_private_directory(profile.parent(), DIRECTORY)?;
        profile.verify()?;
        let directory = SnapshotDirectory::open(
            &profile.path().join(DIRECTORY),
            profile.epoch(),
            super::DirectoryPurpose::Private,
        )?;
        platform::validate_private_directory(directory.parent())?;
        ensure!(
            fs::read_dir(directory.path())?.next().is_none(),
            "Impulse namespace without ownership proof is not empty"
        );
        directory.verify()?;
        let namespace = random_token()?;
        // Retain the allocation before inspecting its identity.
        let mut owner = directory.open_group_file(OWNER, true)?;
        let identity = directory.group_file_identity(&owner)?;
        owner
            .try_lock()
            .context("Impulse namespace has another producer")?;
        owner.write_all(&owner_header(&namespace))?;
        owner.sync_all()?;
        platform::sync_directory(directory.parent())?;
        let receipt = ImpulseVaultReceipt {
            namespace,
            profile_identity: ImpulseFileIdentity::from_native(profile.identity()),
            directory_identity: ImpulseFileIdentity::from_native(directory.identity()),
            owner_identity: ImpulseFileIdentity::from_native(&identity),
        };
        let vault = Self {
            profile: profile.clone(),
            directory,
            owner,
            receipt,
            armed: false,
            retirement_limiter: retirement_limiter()?,
            pending: Arc::new(Mutex::new(BTreeMap::new())),
            #[cfg(test)]
            cancel_after_write: None,
        };
        vault.verify()?;
        Ok(vault)
    }
    #[cfg(all(test, not(windows)))]
    pub(crate) fn create(_: &SnapshotDirectory) -> Result<Self> {
        anyhow::bail!("Durable impulse storage requires native Windows ownership")
    }

    #[cfg(windows)]
    pub(crate) fn reopen(
        profile: &SnapshotDirectory,
        receipt: &ImpulseVaultReceipt,
    ) -> Result<Self> {
        receipt.validate()?;
        profile.verify()?;
        ensure!(
            receipt.profile_identity.matches(profile.identity()),
            "Impulse receipt belongs to a different actual profile"
        );
        let directory = SnapshotDirectory::open(
            &profile.path().join(DIRECTORY),
            profile.epoch(),
            super::DirectoryPurpose::Private,
        )?;
        platform::validate_private_directory(directory.parent())?;
        ensure!(
            receipt.directory_identity.matches(directory.identity()),
            "Impulse receipt belongs to another actual namespace"
        );
        let owner = directory.open_group_file(OWNER, false)?;
        ensure!(
            receipt
                .owner_identity
                .matches(&directory.group_file_identity(&owner)?),
            "Impulse namespace owner was replaced"
        );
        owner
            .try_lock()
            .context("Impulse namespace has another producer")?;
        let vault = Self {
            profile: profile.clone(),
            directory,
            owner,
            receipt: receipt.clone(),
            armed: true,
            retirement_limiter: retirement_limiter()?,
            pending: Arc::new(Mutex::new(BTreeMap::new())),
            #[cfg(test)]
            cancel_after_write: None,
        };
        vault.verify()?;
        vault.disk_usage()?;
        Ok(vault)
    }
    #[cfg(not(windows))]
    pub(crate) fn reopen(_: &SnapshotDirectory, _: &ImpulseVaultReceipt) -> Result<Self> {
        anyhow::bail!("Durable impulse storage requires native Windows ownership")
    }
    #[cfg(test)]
    pub(crate) fn receipt(&self) -> &ImpulseVaultReceipt {
        &self.receipt
    }
    pub(crate) fn arm(&mut self, durable: &ImpulseVaultReceipt) -> Result<()> {
        ensure!(
            durable == &self.receipt,
            "Durable impulse namespace receipt differs"
        );
        self.verify()?;
        self.armed = true;
        Ok(())
    }

    fn verify(&self) -> Result<()> {
        self.profile.verify()?;
        self.directory.verify()?;
        ensure!(
            self.receipt
                .profile_identity
                .matches(self.profile.identity())
                && self
                    .receipt
                    .directory_identity
                    .matches(self.directory.identity())
                && self
                    .receipt
                    .owner_identity
                    .matches(&platform::identity(&self.owner)?),
            "Impulse namespace physical ownership changed"
        );
        platform::validate(&self.owner, false)?;
        ensure!(
            platform::links(&self.owner)? == 1
                && self.owner.metadata()?.len() == OWNER_BYTES as u64,
            "Impulse namespace owner is aliased or has changed length"
        );
        let mut header = [0; OWNER_BYTES];
        read_exact_at(&self.owner, 0, &mut header)?;
        ensure!(
            header == owner_header(&self.receipt.namespace),
            "Impulse namespace owner header differs"
        );
        platform::verify_name(
            self.directory.parent(),
            OWNER,
            &platform::identity(&self.owner)?,
        )?;
        self.directory.verify()
    }

    /// Includes incomplete and foreign regular files. Links and subdirectories
    /// make admission fail; inventory never follows or removes their targets.
    fn named_disk_usage(&self) -> Result<(usize, u64)> {
        self.verify()?;
        let mut count = 0usize;
        let mut bytes = 0u64;
        for entry in fs::read_dir(self.directory.path())? {
            count += 1;
            ensure!(
                count <= MAX_NAMESPACE_ENTRIES,
                "Impulse namespace entry quota exceeded"
            );
            let entry = entry?;
            let metadata = fs::symlink_metadata(entry.path())?;
            #[cfg(windows)]
            let linked = {
                use std::os::windows::fs::MetadataExt;
                metadata.file_attributes() & 0x400 != 0
            };
            #[cfg(not(windows))]
            let linked = metadata.file_type().is_symlink();
            ensure!(
                metadata.is_file() && !linked,
                "Impulse namespace contains an unmeasurable foreign object"
            );
            bytes = bytes
                .checked_add(metadata.len())
                .context("Impulse disk accounting overflow")?;
            ensure!(
                bytes <= MAX_IMPULSE_VAULT_BYTES,
                "Impulse namespace disk quota exceeded"
            );
        }
        self.verify()?;
        Ok((count, bytes))
    }
    pub(crate) fn disk_usage(&self) -> Result<(usize, u64)> {
        let (count, mut bytes) = self.named_disk_usage()?;
        let pending = self
            .pending
            .lock()
            .map_err(|_| anyhow::anyhow!("Impulse producer ownership was poisoned"))?;
        for allocation in pending.values() {
            let named = allocation
                .file
                .as_ref()
                .map(|file| file.metadata().map(|metadata| metadata.len()))
                .transpose()?
                .unwrap_or(0);
            ensure!(
                named <= allocation.reserved,
                "Impulse allocation exceeds its original reservation"
            );
            bytes = bytes
                .checked_add(allocation.reserved - named)
                .context("Impulse reservation accounting overflow")?;
        }
        ensure!(
            bytes <= MAX_IMPULSE_VAULT_BYTES,
            "Impulse pending allocation quota exceeded"
        );
        Ok((count, bytes))
    }
    /// Called after durable removed acknowledgement and the cleanup worker's
    /// actual join. No descriptor or reservation is released by cancellation.
    pub(crate) fn release_import_reservation(&mut self, token: &str) -> Result<()> {
        let mut pending = self
            .pending
            .lock()
            .map_err(|_| anyhow::anyhow!("Impulse producer ownership was poisoned"))?;
        if let Some(allocation) = pending.get(token) {
            ensure!(
                allocation.file.is_none(),
                "Original impulse producer descriptor is still owned"
            );
        }
        pending.remove(token);
        Ok(())
    }

    #[cfg(windows)]
    pub(crate) fn import_selected(
        &mut self,
        selected: &AuthorizedFile,
        extension: &str,
        context: &ImpulseDecodeContext,
        journal: &dyn Fn(&ImpulseImportFact) -> Result<()>,
    ) -> Result<ImportedImpulse> {
        ensure!(self.armed, "Impulse namespace receipt is not durable");
        context.check()?;
        self.verify()?;
        let original = selected.metadata_snapshot()?;
        let prepared = prepare_selected_impulse(selected, extension, context)?;
        let reserved = prepared.encoded_bytes + HEADER_BYTES as u64;
        let (count, disk) = self.disk_usage()?;
        ensure!(
            count < MAX_NAMESPACE_ENTRIES && reserved <= MAX_IMPULSE_VAULT_BYTES - disk,
            "Impulse namespace cannot reserve this representation"
        );
        let token = random_token()?;
        let basename = impulse_basename(&token);
        let mut fact = ImpulseImportFact {
            namespace: self.receipt.namespace.clone(),
            token: token.clone(),
            body_identity: None,
            reserved_bytes: reserved,
            expected_sha256: prepared.encoded_sha256.clone(),
            phase: ImpulseImportPhase::Allocating,
        };
        fact.validate()?;
        journal(&fact)?;
        context.check()?;
        self.verify()?;
        let retained = self.pending.clone();
        let mut retained = retained
            .lock()
            .map_err(|_| anyhow::anyhow!("Impulse producer ownership was poisoned"))?;
        ensure!(
            retained.len() < MAX_NAMESPACE_ENTRIES,
            "Impulse producer owner quota exceeded"
        );
        let body = self.directory.open_group_file(&basename, true)?;
        retained.insert(
            token.clone(),
            PendingImpulseFile {
                file: Some(body),
                reserved,
                producer: true,
            },
        );
        let body = retained.get_mut(&token).unwrap().file.as_mut().unwrap();
        let identity = self.directory.group_file_identity(body)?;
        fact.body_identity = Some(ImpulseFileIdentity::from_native(&identity));
        fact.phase = ImpulseImportPhase::Writing;
        // The caller durably attests the retained object before any body write.
        journal(&fact)?;
        context.check()?;
        let header = body_header(&fact.namespace, &fact.token, &fact.expected_sha256);
        body.write_all(&header)?;
        let mut source = selected
            .authorization()
            .with_cancel(context.cancel_flag())?
            .open()?;
        ensure!(
            source.identity() == selected.identity()
                && source.change_stamp()? == original.change_stamp,
            "Impulse selection changed after decode"
        );
        source.seek(SeekFrom::Start(0))?;
        let mut copied = 0u64;
        let mut hash = Sha256::new();
        let mut buffer = [0; 64 * 1024];
        loop {
            context.check()?;
            self.verify()?;
            ensure!(
                selected.change_stamp()? == original.change_stamp
                    && source.change_stamp()? == original.change_stamp,
                "Impulse selection changed during durable import"
            );
            let read = source.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            ensure!(
                read as u64 <= prepared.encoded_bytes - copied,
                "Impulse selection grew during durable import"
            );
            body.write_all(&buffer[..read])?;
            hash.update(&buffer[..read]);
            copied += read as u64;
            #[cfg(test)]
            if self.cancel_after_write.is_some_and(|limit| copied >= limit) {
                context.cancel();
            }
            ensure!(
                body.metadata()?.len() <= reserved,
                "Impulse body exceeded its disk reservation"
            );
        }
        ensure!(
            copied == prepared.encoded_bytes
                && format!("{:x}", hash.finalize()) == prepared.encoded_sha256
                && selected.change_stamp()? == original.change_stamp
                && source.change_stamp()? == original.change_stamp,
            "Durable impulse body differs from its decoded selected representation"
        );
        body.sync_all()?;
        platform::sync_directory(self.directory.parent())?;
        self.verify_body(body, &basename, &identity, reserved, &header)?;
        context.check()?;
        ensure!(
            hash_body(body, HEADER_BYTES as u64, prepared.encoded_bytes, context)?
                == prepared.encoded_sha256,
            "Durable impulse body failed final SHA verification"
        );
        let resource = StoredImpulseResource {
            resource_id: ImpulseResourceId::Imported(token),
            namespace: fact.namespace.clone(),
            body_identity: ImpulseFileIdentity::from_native(&identity),
            encoded_sha256: prepared.encoded_sha256.clone(),
            encoded_bytes: prepared.encoded_bytes,
            extension: extension.to_owned(),
            codec: prepared.decoded.codec,
            decoded_frames: prepared.decoded.decoded_frames,
            sample_rate: prepared.decoded.impulse.sample_rate(),
            channels: prepared.decoded.impulse.channels(),
        };
        resource.validate()?;
        let impulse = OwnedImpulse::from_validated(
            prepared.decoded.impulse,
            resource.resource_id.clone(),
            resource.encoded_sha256.clone(),
        )?;
        self.named_disk_usage()?;
        fact.phase = ImpulseImportPhase::Sealed;
        journal(&fact)?;
        context.check()?;
        self.verify_body(body, &basename, &identity, reserved, &header)?;
        // Closing the sole writer ends its allocation ownership. Subsequent
        // preparation uses a read-only handle with write/delete sharing denied.
        let pending = retained
            .remove(resource.token())
            .expect("original producer allocation");
        drop(pending);
        Ok(ImportedImpulse { resource, impulse })
    }
    #[cfg(not(windows))]
    pub(crate) fn import_selected(
        &mut self,
        _: &AuthorizedFile,
        _: &str,
        _: &ImpulseDecodeContext,
        _: &dyn Fn(&ImpulseImportFact) -> Result<()>,
    ) -> Result<ImportedImpulse> {
        anyhow::bail!("Durable impulse storage requires native Windows ownership")
    }

    fn verify_body(
        &self,
        body: &File,
        basename: &str,
        identity: &FileIdentity,
        bytes: u64,
        expected: &[u8; HEADER_BYTES],
    ) -> Result<()> {
        self.verify()?;
        platform::validate(body, false)?;
        ensure!(
            platform::identity(body)? == *identity
                && platform::links(body)? == 1
                && body.metadata()?.len() == bytes,
            "Impulse body identity, alias count or length changed"
        );
        platform::verify_name(self.directory.parent(), basename, identity)?;
        let mut header = [0; HEADER_BYTES];
        read_exact_at(body, 0, &mut header)?;
        ensure!(&header == expected, "Impulse body ownership header changed");
        self.verify()
    }

    #[cfg(windows)]
    pub(crate) fn prepare_owned(
        &self,
        resource: &StoredImpulseResource,
        context: &ImpulseDecodeContext,
    ) -> Result<Arc<OwnedImpulse>> {
        resource.validate()?;
        ensure!(
            self.armed && resource.namespace == self.receipt.namespace,
            "Impulse representation belongs to a different owned namespace"
        );
        context.check()?;
        self.verify()?;
        let name = resource.basename();
        let body = platform::open_immutable_source(self.directory.parent(), &name)?;
        let identity = platform::identity(&body)?;
        ensure!(
            resource.body_identity.matches(&identity),
            "Stored impulse file identity was replaced"
        );
        let header = body_header(
            &resource.namespace,
            resource.token(),
            &resource.encoded_sha256,
        );
        let length = resource.encoded_bytes + HEADER_BYTES as u64;
        self.verify_body(&body, &name, &identity, length, &header)?;
        let stamp = platform::stamp(&body)?;
        let mut input = ImpulseEncodedInput::prepare(resource.encoded_bytes as usize)?;
        let mut buffer = [0; 64 * 1024];
        let mut offset = 0u64;
        let mut hash = Sha256::new();
        while offset < resource.encoded_bytes {
            context.check()?;
            self.verify_body(&body, &name, &identity, length, &header)?;
            let count = buffer.len().min((resource.encoded_bytes - offset) as usize);
            read_exact_at(&body, HEADER_BYTES as u64 + offset, &mut buffer[..count])?;
            input.append(&buffer[..count])?;
            hash.update(&buffer[..count]);
            offset += count as u64;
            ensure!(
                platform::stamp(&body)? == stamp,
                "Stored impulse changed during preparation"
            );
        }
        ensure!(
            format!("{:x}", hash.finalize()) == resource.encoded_sha256,
            "Stored impulse body differs from its durable SHA"
        );
        let decoded = decode_impulse(input, &resource.extension, context)?;
        ensure!(
            decoded.codec == resource.codec
                && decoded.decoded_frames == resource.decoded_frames
                && decoded.impulse.sample_rate() == resource.sample_rate
                && decoded.impulse.channels() == resource.channels,
            "Stored impulse decode differs from its admission record"
        );
        context.check()?;
        self.verify_body(&body, &name, &identity, length, &header)?;
        ensure!(
            platform::stamp(&body)? == stamp,
            "Stored impulse changed before ownership transfer"
        );
        Ok(OwnedImpulse::from_validated(
            decoded.impulse,
            resource.resource_id.clone(),
            resource.encoded_sha256.clone(),
        )?)
    }
    #[cfg(not(windows))]
    pub(crate) fn prepare_owned(
        &self,
        _: &StoredImpulseResource,
        _: &ImpulseDecodeContext,
    ) -> Result<Arc<OwnedImpulse>> {
        anyhow::bail!("Durable impulse storage requires native Windows ownership")
    }

    /// Explicit recovery of a complete, journaled allocation. An intent without
    /// its actual FID cannot be adopted from a token, header or pathname.
    #[cfg(windows)]
    pub(crate) fn recover_import(
        &self,
        fact: &ImpulseImportFact,
        context: &ImpulseDecodeContext,
    ) -> Result<ImportedImpulse> {
        fact.validate()?;
        ensure!(
            self.armed && fact.namespace == self.receipt.namespace,
            "Impulse import does not belong to this actual namespace"
        );
        let expected = fact
            .body_identity
            .as_ref()
            .context("Impulse allocation has no attested physical identity")?;
        context.check()?;
        self.verify()?;
        let name = impulse_basename(&fact.token);
        let body = platform::open_immutable_source(self.directory.parent(), &name)?;
        let identity = platform::identity(&body)?;
        ensure!(
            expected.matches(&identity),
            "Interrupted impulse allocation was replaced"
        );
        let header = body_header(&fact.namespace, &fact.token, &fact.expected_sha256);
        self.verify_body(&body, &name, &identity, fact.reserved_bytes, &header)?;
        let stamp = platform::stamp(&body)?;
        let length = fact.reserved_bytes - HEADER_BYTES as u64;
        let mut signature = [0u8; 12];
        read_exact_at(
            &body,
            HEADER_BYTES as u64,
            &mut signature[..length.min(12) as usize],
        )?;
        let extension = if &signature[..4] == b"fLaC" {
            "flac"
        } else if &signature[..4] == b"RIFF" && &signature[8..] == b"WAVE" {
            "wav"
        } else {
            anyhow::bail!("Interrupted impulse has no admitted container")
        };
        let mut input = ImpulseEncodedInput::prepare(length as usize)?;
        let mut offset = 0u64;
        let mut hash = Sha256::new();
        let mut buffer = [0; 64 * 1024];
        while offset < length {
            context.check()?;
            self.verify_body(&body, &name, &identity, fact.reserved_bytes, &header)?;
            let count = buffer.len().min((length - offset) as usize);
            read_exact_at(&body, HEADER_BYTES as u64 + offset, &mut buffer[..count])?;
            input.append(&buffer[..count])?;
            hash.update(&buffer[..count]);
            offset += count as u64;
            ensure!(
                platform::stamp(&body)? == stamp,
                "Interrupted impulse changed during recovery"
            );
        }
        ensure!(
            format!("{:x}", hash.finalize()) == fact.expected_sha256,
            "Interrupted impulse differs from its durable selected-body SHA"
        );
        let decoded = decode_impulse(input, extension, context)?;
        context.check()?;
        self.verify_body(&body, &name, &identity, fact.reserved_bytes, &header)?;
        ensure!(
            platform::stamp(&body)? == stamp,
            "Interrupted impulse changed before ownership transfer"
        );
        let resource = StoredImpulseResource {
            resource_id: ImpulseResourceId::Imported(fact.token.clone()),
            namespace: fact.namespace.clone(),
            body_identity: expected.clone(),
            encoded_sha256: fact.expected_sha256.clone(),
            encoded_bytes: length,
            extension: extension.into(),
            codec: decoded.codec,
            decoded_frames: decoded.decoded_frames,
            sample_rate: decoded.impulse.sample_rate(),
            channels: decoded.impulse.channels(),
        };
        resource.validate()?;
        let impulse = OwnedImpulse::from_validated(
            decoded.impulse,
            resource.resource_id.clone(),
            resource.encoded_sha256.clone(),
        )?;
        Ok(ImportedImpulse { resource, impulse })
    }
    #[cfg(not(windows))]
    pub(crate) fn recover_import(
        &self,
        _: &ImpulseImportFact,
        _: &ImpulseDecodeContext,
    ) -> Result<ImportedImpulse> {
        anyhow::bail!("Durable impulse storage requires native Windows ownership")
    }

    /// Explicit retirement admits only a fully verified durable IR. The shared
    /// filename syntax satisfies the native engine's private-leaf validator;
    /// authority comes from this distinct vault receipt and original body FID.
    #[cfg(windows)]
    pub(crate) fn prepare_retirement(
        &self,
        resource: &StoredImpulseResource,
        context: &ImpulseDecodeContext,
        cleanup: &SnapshotContext,
    ) -> Result<ImpulseRetirementOwner> {
        resource.validate()?;
        ensure!(
            self.armed && resource.namespace == self.receipt.namespace,
            "Impulse retirement belongs to another owned namespace"
        );
        context.check()?;
        cleanup.check()?;
        self.verify()?;
        let name = resource.basename();
        let mut file = Some(self.directory.open_group_file(&name, false)?);
        let body = file.as_ref().unwrap();
        let identity = self.directory.group_file_identity(body)?;
        ensure!(
            resource.body_identity.matches(&identity),
            "Impulse retirement opened a foreign original"
        );
        let header = body_header(
            &resource.namespace,
            resource.token(),
            &resource.encoded_sha256,
        );
        self.verify_body(body, &name, &identity, resource.storage_bytes(), &header)?;
        ensure!(
            hash_body(body, HEADER_BYTES as u64, resource.encoded_bytes, context)?
                == resource.encoded_sha256,
            "Impulse retirement body differs from its durable selected representation"
        );
        let whole_sha = hash_body(body, 0, resource.storage_bytes(), context)?;
        self.verify_body(body, &name, &identity, resource.storage_bytes(), &header)?;
        let maximum = MAX_IMPULSE_ENCODED_BYTES as u64 + HEADER_BYTES as u64;
        let permit = self
            .retirement_limiter
            .reserve_job(maximum, retirement_workspace())?;
        let metadata = ActualFileMetadata {
            operation_id: format!("ir-retire-{}", resource.token()),
            nonce: resource.token().into(),
            temporary_name: Some(name),
            directory: self.directory.path().to_owned(),
            directory_identity: self.directory.identity().clone(),
            selection_epoch: self.directory.epoch(),
            identity: Some(identity),
            bytes: resource.storage_bytes(),
            published: false,
            final_basename: None,
            cleanup_probe: None,
        };
        let owner = ActualFile::adopt_group_file(
            &mut file,
            &self.directory,
            metadata,
            &whole_sha,
            cleanup,
            &permit,
        )?;
        Ok(ImpulseRetirementOwner {
            namespace: resource.namespace.clone(),
            token: resource.token().into(),
            owner: Some(owner),
            failure: None,
            removed: false,
        })
    }
    #[cfg(not(windows))]
    pub(crate) fn prepare_retirement(
        &self,
        _: &StoredImpulseResource,
        _: &ImpulseDecodeContext,
        _: &SnapshotContext,
    ) -> Result<ImpulseRetirementOwner> {
        anyhow::bail!("Durable impulse retirement requires native Windows ownership")
    }

    pub(crate) fn prepare_cold_retirement(
        &self,
        record: &ImpulseRetirementRecord,
    ) -> Result<ColdImpulseRetirement> {
        record.validate()?;
        self.verify()?;
        ensure!(
            self.armed
                && record.namespace == self.receipt.namespace
                && record.fact.file.directory == self.directory.path()
                && record.fact.file.directory_identity == *self.directory.identity()
                && record.pending_zero_links(),
            "Cold impulse retirement lacks the original namespace and pending zero-link witness"
        );
        let owner = ColdCleanup::prepare(self.directory.clone(), record.fact.clone())?;
        Ok(ColdImpulseRetirement {
            namespace: record.namespace.clone(),
            token: record.token.clone(),
            owner,
        })
    }

    /// Warm abort transfers the producer's actual descriptor. Cold abort needs
    /// an attested original FID and a fresh authorized selection of that exact
    /// object; an ID or locator cannot manufacture the missing producer owner.
    #[cfg(windows)]
    pub(crate) fn prepare_import_abort(
        &mut self,
        fact: &ImpulseImportFact,
        selected: Option<AuthorizedFile>,
        context: &ImpulseDecodeContext,
        cleanup: &SnapshotContext,
    ) -> Result<ImpulseRetirementOwner> {
        fact.validate()?;
        context.check()?;
        cleanup.check()?;
        self.verify()?;
        ensure!(
            self.armed && fact.namespace == self.receipt.namespace,
            "Impulse abort belongs to another owned namespace"
        );
        let selection = selected
            .map(|selected| AbortSelectionProof::capture(selected, fact, context))
            .transpose()?;
        let retained = self.pending.clone();
        let mut pending = retained
            .lock()
            .map_err(|_| anyhow::anyhow!("Impulse producer ownership was poisoned"))?;
        if !pending.contains_key(&fact.token) {
            ensure!(
                fact.body_identity.is_some() && selection.is_some(),
                "Cold impulse abort requires the original attested FID and a fresh approved selection"
            );
            let file = self
                .directory
                .open_group_file(&impulse_basename(&fact.token), false)?;
            // Retain this descriptor before any fallible verification.
            pending.insert(
                fact.token.clone(),
                PendingImpulseFile {
                    file: Some(file),
                    reserved: fact.reserved_bytes,
                    producer: false,
                },
            );
        }
        let allocation = pending.get_mut(&fact.token).unwrap();
        ensure!(
            allocation.producer || selection.is_some(),
            "Cold impulse descriptor has no fresh abort approval"
        );
        ensure!(
            allocation.reserved == fact.reserved_bytes,
            "Impulse reservation was replaced"
        );
        let file = allocation
            .file
            .as_ref()
            .context("Impulse producer descriptor was transferred to a retirement owner")?;
        let identity = self.directory.group_file_identity(file)?;
        if let Some(expected) = &fact.body_identity {
            ensure!(
                expected.matches(&identity),
                "Impulse abort opened a foreign allocation"
            );
        }
        if let Some(selection) = &selection {
            selection.verify(file, &identity, context)?;
        }
        self.directory
            .verify_group_name(&impulse_basename(&fact.token), &identity)?;
        let length = file.metadata()?.len();
        ensure!(
            length <= fact.reserved_bytes,
            "Impulse partial body exceeds its admitted reservation"
        );
        let header = body_header(&fact.namespace, &fact.token, &fact.expected_sha256);
        let mut prefix = [0; HEADER_BYTES];
        let count = (length as usize).min(HEADER_BYTES);
        read_exact_at(file, 0, &mut prefix[..count])?;
        ensure!(
            prefix[..count] == header[..count],
            "Impulse partial header is foreign to its original allocation"
        );
        let stamp = platform::stamp(file)?;
        let sha = hash_body(file, 0, length, context)?;
        ensure!(
            platform::stamp(file)? == stamp,
            "Impulse partial body changed during abort admission"
        );
        let maximum = MAX_IMPULSE_ENCODED_BYTES as u64 + HEADER_BYTES as u64;
        let permit = self
            .retirement_limiter
            .reserve_job(maximum, retirement_workspace())?;
        let metadata = ActualFileMetadata {
            operation_id: format!("ir-retire-{}", fact.token),
            nonce: fact.token.clone(),
            temporary_name: Some(impulse_basename(&fact.token)),
            directory: self.directory.path().to_owned(),
            directory_identity: self.directory.identity().clone(),
            selection_epoch: self.directory.epoch(),
            identity: Some(identity),
            bytes: length,
            published: false,
            final_basename: None,
            cleanup_probe: None,
        };
        let owner = ActualFile::adopt_group_file(
            &mut allocation.file,
            &self.directory,
            metadata,
            &sha,
            cleanup,
            &permit,
        )?;
        Ok(ImpulseRetirementOwner {
            namespace: fact.namespace.clone(),
            token: fact.token.clone(),
            owner: Some(owner),
            failure: None,
            removed: false,
        })
    }
    #[cfg(not(windows))]
    pub(crate) fn prepare_import_abort(
        &mut self,
        _: &ImpulseImportFact,
        _: Option<AuthorizedFile>,
        _: &ImpulseDecodeContext,
        _: &SnapshotContext,
    ) -> Result<ImpulseRetirementOwner> {
        anyhow::bail!("Impulse abort requires native Windows ownership")
    }
}

#[cfg(windows)]
struct AbortSelectionProof {
    identity: crate::domain::library::FileIdentity,
    changed: i64,
    length: u64,
    sha256: String,
    grant: crate::library::authority::FileAuthorization,
}
#[cfg(windows)]
impl AbortSelectionProof {
    fn capture(
        mut selected: AuthorizedFile,
        fact: &ImpulseImportFact,
        context: &ImpulseDecodeContext,
    ) -> Result<Self> {
        context.check()?;
        selected.check()?;
        let snapshot = selected.metadata_snapshot()?;
        ensure!(
            snapshot.metadata.len() <= fact.reserved_bytes,
            "Selected partial impulse exceeds its original reservation"
        );
        let expected = fact
            .body_identity
            .as_ref()
            .context("Unknown cold allocation cannot be claimed by a selection")?;
        ensure!(
            selected.identity().kind == "windows_file_id"
                && selected.identity().volume == format!("{:016x}", expected.device)
                && selected.identity().file_id == expected.object,
            "Selected partial impulse has a foreign physical identity"
        );
        let crate::library::authority::FileChangeStamp::Windows { change_time_100ns } =
            snapshot.change_stamp
        else {
            anyhow::bail!("Selected partial impulse has no native Windows stamp");
        };
        selected.seek(SeekFrom::Start(0))?;
        let mut hash = Sha256::new();
        let mut read = 0u64;
        let mut buffer = [0; 64 * 1024];
        loop {
            context.check()?;
            let count = selected.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            ensure!(
                count as u64 <= snapshot.metadata.len() - read,
                "Selected partial impulse grew while granting abort"
            );
            hash.update(&buffer[..count]);
            read += count as u64;
        }
        ensure!(
            read == snapshot.metadata.len() && selected.change_stamp()? == snapshot.change_stamp,
            "Selected partial impulse changed during its fresh abort approval"
        );
        let proof = Self {
            identity: selected.identity().clone(),
            changed: change_time_100ns,
            length: read,
            sha256: format!("{:x}", hash.finalize()),
            grant: selected.authorization(),
        };
        drop(selected);
        proof.grant.check()?;
        Ok(proof)
    }
    fn verify(
        &self,
        file: &File,
        identity: &FileIdentity,
        context: &ImpulseDecodeContext,
    ) -> Result<()> {
        ensure!(
            self.identity.volume == format!("{:016x}", identity.device)
                && self.identity.file_id == identity.object,
            "Writable partial impulse differs from the actual selected descriptor"
        );
        let stamp = platform::stamp(file)?;
        ensure!(
            stamp.length == self.length && stamp.changed == self.changed as i128,
            "Writable partial impulse changed after selection consumption"
        );
        ensure!(
            hash_body(file, 0, self.length, context)? == self.sha256
                && platform::stamp(file)? == stamp,
            "Writable partial impulse body differs from its fresh selected representation"
        );
        Ok(())
    }
}

fn impulse_basename(token: &str) -> String {
    format!(".lx-profile-{token}-impulse.bin")
}
fn retirement_workspace() -> u64 {
    3 * (MAX_IMPULSE_ENCODED_BYTES as u64 + HEADER_BYTES as u64) + 65 * 1024 * 1024
}
fn retirement_limiter() -> Result<SnapshotLimiter> {
    SnapshotLimiter::new(retirement_workspace() * 4)
}

fn random_token() -> Result<String> {
    let mut bytes = [0; 16];
    getrandom::fill(&mut bytes)
        .map_err(|_| anyhow::anyhow!("Impulse ownership entropy unavailable"))?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}
fn hex(value: &str, size: usize) -> bool {
    value.len() == size
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}
fn owner_header(namespace: &str) -> [u8; OWNER_BYTES] {
    let mut bytes = [0; OWNER_BYTES];
    bytes[..16].copy_from_slice(OWNER_MAGIC);
    bytes[16..].copy_from_slice(namespace.as_bytes());
    bytes
}
fn body_header(namespace: &str, token: &str, sha: &str) -> [u8; HEADER_BYTES] {
    let mut bytes = [0; HEADER_BYTES];
    bytes[..16].copy_from_slice(BODY_MAGIC);
    bytes[16..48].copy_from_slice(namespace.as_bytes());
    bytes[48..80].copy_from_slice(token.as_bytes());
    bytes[80..].copy_from_slice(sha.as_bytes());
    bytes
}
fn read_exact_at(file: &File, mut offset: u64, mut bytes: &mut [u8]) -> Result<()> {
    while !bytes.is_empty() {
        let count = platform::read_at(file, bytes, offset)?;
        ensure!(count > 0, "Impulse representation is truncated");
        offset += count as u64;
        bytes = &mut bytes[count..];
    }
    Ok(())
}
fn hash_body(
    file: &File,
    start: u64,
    length: u64,
    context: &ImpulseDecodeContext,
) -> Result<String> {
    let mut offset = 0u64;
    let mut buffer = [0; 64 * 1024];
    let mut hash = Sha256::new();
    while offset < length {
        context.check()?;
        let count = buffer.len().min((length - offset) as usize);
        read_exact_at(file, start + offset, &mut buffer[..count])?;
        hash.update(&buffer[..count]);
        offset += count as u64;
    }
    Ok(format!("{:x}", hash.finalize()))
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use crate::library::authority::AuthorityRegistry;
    use std::time::Duration;

    fn wav() -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"RIFF");
        bytes.extend_from_slice(&40u32.to_le_bytes());
        bytes.extend_from_slice(b"WAVEfmt ");
        bytes.extend_from_slice(&16u32.to_le_bytes());
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.extend_from_slice(&8000u32.to_le_bytes());
        bytes.extend_from_slice(&16000u32.to_le_bytes());
        bytes.extend_from_slice(&2u16.to_le_bytes());
        bytes.extend_from_slice(&16u16.to_le_bytes());
        bytes.extend_from_slice(b"data");
        bytes.extend_from_slice(&4u32.to_le_bytes());
        bytes.extend_from_slice(&16384i16.to_le_bytes());
        bytes.extend_from_slice(&0i16.to_le_bytes());
        bytes
    }
    fn imported(temp: &tempfile::TempDir) -> (ImpulseVault, StoredImpulseResource) {
        let selected_path = temp.path().join("selected.wav");
        fs::write(&selected_path, wav()).unwrap();
        let profile = SnapshotDirectory::pin_profile(temp.path(), 1).unwrap();
        let mut vault = ImpulseVault::create(&profile).unwrap();
        let receipt = vault.receipt().clone();
        vault.arm(&receipt).unwrap();
        let authority = AuthorityRegistry::default();
        let selected = authority.open_selection(&selected_path).unwrap();
        let prepared = vault
            .import_selected(
                &selected,
                "wav",
                &ImpulseDecodeContext::new(Duration::from_secs(10)).unwrap(),
                &|_| Ok(()),
            )
            .unwrap();
        assert_eq!(prepared.impulse.source_id(), &prepared.resource.resource_id);
        (vault, prepared.resource)
    }
    #[test]
    fn restart_reads_owned_bytes_after_original_selection_is_removed() {
        let temp = tempfile::tempdir().unwrap();
        let (vault, resource) = imported(&temp);
        let receipt = vault.receipt().clone();
        drop(vault);
        fs::remove_file(temp.path().join("selected.wav")).unwrap();
        let profile = SnapshotDirectory::pin_profile(temp.path(), 2).unwrap();
        let restarted = ImpulseVault::reopen(&profile, &receipt).unwrap();
        let impulse = restarted
            .prepare_owned(
                &resource,
                &ImpulseDecodeContext::new(Duration::from_secs(10)).unwrap(),
            )
            .unwrap();
        assert_eq!(impulse.validated().frames(), 2);
        assert_eq!(impulse.representation_sha256(), resource.encoded_sha256);
        assert!(
            !serde_json::to_string(&resource)
                .unwrap()
                .contains("selected.wav")
        );
    }
    #[test]
    fn foreign_owner_and_same_body_replacement_do_not_recreate_authority() {
        let temp = tempfile::tempdir().unwrap();
        let (vault, resource) = imported(&temp);
        let body = vault.directory.path().join(resource.basename());
        let mut replacement = fs::read(&body).unwrap();
        let receipt = vault.receipt().clone();
        drop(vault);
        fs::remove_file(&body).unwrap();
        fs::write(&body, &replacement).unwrap();
        let profile = SnapshotDirectory::pin_profile(temp.path(), 2).unwrap();
        let restarted = ImpulseVault::reopen(&profile, &receipt).unwrap();
        assert!(
            restarted
                .prepare_owned(
                    &resource,
                    &ImpulseDecodeContext::new(Duration::from_secs(10)).unwrap()
                )
                .is_err()
        );
        drop(restarted);
        replacement.fill(0);
        fs::write(
            temp.path().join(DIRECTORY).join(OWNER),
            &replacement[..OWNER_BYTES],
        )
        .unwrap();
        assert!(ImpulseVault::reopen(&profile, &receipt).is_err());
    }
    #[test]
    fn cancellation_before_first_body_write_leaves_exact_charged_inventory() {
        let temp = tempfile::tempdir().unwrap();
        let profile = SnapshotDirectory::pin_profile(temp.path(), 1).unwrap();
        let mut vault = ImpulseVault::create(&profile).unwrap();
        let receipt = vault.receipt().clone();
        vault.arm(&receipt).unwrap();
        let path = temp.path().join("selected.wav");
        fs::write(&path, wav()).unwrap();
        let selected = AuthorityRegistry::default().open_selection(&path).unwrap();
        let context = ImpulseDecodeContext::new(Duration::from_secs(10)).unwrap();
        let facts = std::sync::Mutex::new(Vec::new());
        let result = vault.import_selected(&selected, "wav", &context, &|fact| {
            facts.lock().unwrap().push(fact.clone());
            if fact.phase == ImpulseImportPhase::Writing {
                context.cancel();
            }
            Ok(())
        });
        assert!(result.is_err());
        let facts = facts.lock().unwrap();
        assert_eq!(facts.len(), 2);
        assert!(facts[1].body_identity.is_some());
        assert_eq!(vault.disk_usage().unwrap().0, 2);
        let pending = vault.pending.lock().unwrap();
        let body = pending.get(&facts[1].token).unwrap().file.as_ref().unwrap();
        assert!(
            facts[1]
                .body_identity
                .as_ref()
                .unwrap()
                .matches(&platform::identity(body).unwrap())
        );
        assert_eq!(body.metadata().unwrap().len(), 0);
    }
    #[test]
    fn complete_interrupted_import_recovers_only_with_its_durable_original_fid() {
        let temp = tempfile::tempdir().unwrap();
        let profile = SnapshotDirectory::pin_profile(temp.path(), 1).unwrap();
        let mut vault = ImpulseVault::create(&profile).unwrap();
        let receipt = vault.receipt().clone();
        vault.arm(&receipt).unwrap();
        let path = temp.path().join("selected.wav");
        fs::write(&path, wav()).unwrap();
        let selected = AuthorityRegistry::default().open_selection(&path).unwrap();
        let context = ImpulseDecodeContext::new(Duration::from_secs(10)).unwrap();
        let facts = std::sync::Mutex::new(Vec::new());
        assert!(
            vault
                .import_selected(&selected, "wav", &context, &|fact| {
                    facts.lock().unwrap().push(fact.clone());
                    if fact.phase == ImpulseImportPhase::Sealed {
                        context.cancel();
                    }
                    Ok(())
                })
                .is_err()
        );
        drop(selected);
        drop(vault);
        fs::remove_file(&path).unwrap();
        let restarted = ImpulseVault::reopen(&profile, &receipt).unwrap();
        let facts = facts.lock().unwrap();
        let recovered = restarted
            .recover_import(
                &facts[2],
                &ImpulseDecodeContext::new(Duration::from_secs(10)).unwrap(),
            )
            .unwrap();
        assert_eq!(recovered.resource.encoded_sha256, facts[2].expected_sha256);
        assert_eq!(
            recovered.resource.body_identity,
            *facts[2].body_identity.as_ref().unwrap()
        );
        assert!(
            restarted
                .recover_import(
                    &facts[0],
                    &ImpulseDecodeContext::new(Duration::from_secs(10)).unwrap()
                )
                .is_err()
        );
    }
    #[test]
    fn retirement_retains_original_on_journal_failure_then_confirms_exact_zero_links() {
        let temp = tempfile::tempdir().unwrap();
        let (vault, resource) = imported(&temp);
        let body = vault.directory.path().join(resource.basename());
        let context = ImpulseDecodeContext::new(Duration::from_secs(30)).unwrap();
        let cleanup = SnapshotContext::new(Duration::from_secs(30)).unwrap();
        let mut retirement = vault
            .prepare_retirement(&resource, &context, &cleanup)
            .unwrap();
        assert!(
            retirement
                .attempt(&cleanup, &|_| anyhow::bail!("Injected journal failure"))
                .is_err()
        );
        assert!(body.exists());
        assert_eq!(fs::read(temp.path().join("selected.wav")).unwrap(), wav());
        let records = std::sync::Mutex::new(Vec::new());
        assert!(
            retirement
                .attempt(
                    &SnapshotContext::new(Duration::from_secs(30)).unwrap(),
                    &|record| {
                        records.lock().unwrap().push(record.clone());
                        Ok(())
                    }
                )
                .unwrap()
        );
        assert!(!body.exists());
        assert_eq!(vault.disk_usage().unwrap(), (1, OWNER_BYTES as u64));
        assert!(records.lock().unwrap().last().unwrap().removed());
        assert!(retirement.can_release_for_recovery());
    }
    #[test]
    fn final_journal_failure_has_cold_zero_link_recovery_without_redeleting_by_name() {
        let temp = tempfile::tempdir().unwrap();
        let (vault, resource) = imported(&temp);
        let receipt = vault.receipt().clone();
        let records = std::sync::Mutex::new(Vec::new());
        let context = ImpulseDecodeContext::new(Duration::from_secs(30)).unwrap();
        let cleanup = SnapshotContext::new(Duration::from_secs(30)).unwrap();
        let mut retirement = vault
            .prepare_retirement(&resource, &context, &cleanup)
            .unwrap();
        assert!(
            retirement
                .attempt(&cleanup, &|record| {
                    if record.removed() {
                        anyhow::bail!("Injected final store failure");
                    }
                    records.lock().unwrap().push(record.clone());
                    Ok(())
                })
                .is_err()
        );
        assert!(retirement.can_release_for_recovery());
        let pending = records.lock().unwrap().last().unwrap().clone();
        assert!(pending.pending_zero_links());
        assert!(!pending.removed());
        drop(retirement);
        drop(vault);
        let profile = SnapshotDirectory::pin_profile(temp.path(), 2).unwrap();
        let restarted = ImpulseVault::reopen(&profile, &receipt).unwrap();
        let mut cold = restarted.prepare_cold_retirement(&pending).unwrap();
        assert!(
            cold.confirm(&SnapshotContext::new(Duration::from_secs(30)).unwrap())
                .unwrap()
                .removed()
        );
        assert_eq!(fs::read(temp.path().join("selected.wav")).unwrap(), wav());
    }
    #[test]
    fn aliased_body_and_foreign_directory_fail_closed_without_deletion() {
        let temp = tempfile::tempdir().unwrap();
        let (vault, resource) = imported(&temp);
        let body = vault.directory.path().join(resource.basename());
        fs::hard_link(&body, temp.path().join("foreign-alias.bin")).unwrap();
        assert!(
            vault
                .prepare_owned(
                    &resource,
                    &ImpulseDecodeContext::new(Duration::from_secs(10)).unwrap()
                )
                .is_err()
        );
        let foreign = vault.directory.path().join("foreign");
        fs::create_dir(&foreign).unwrap();
        assert!(vault.disk_usage().is_err());
        assert!(body.exists());
        assert!(foreign.exists());
    }
}

//! Native, nonserializable authority for bounded physical SQLite snapshots.
//!
//! Writable SQLite files remain inside the profile-owned staging directory until
//! the actor marks a compact, sanitized image ready for publication. Receipts and
//! phase records describe objects; they cannot recreate a descriptor or a grant.
//! Read-only source pins prove native objects and namespace continuity. SQLite
//! transactions and the default VFS retain responsibility for database/WAL locks.

use anyhow::{Context, Result, bail, ensure};
pub(crate) mod impulse_vault;
#[path = "snapshot_limits_recovery.rs"]
mod limits_recovery;
use sha2::{Digest, Sha256};
use std::{
    fs::File,
    io,
    path::{Component, Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

pub(crate) const MAX_SNAPSHOT_BYTES: u64 = 1024 * 1024 * 1024;
pub(crate) const MAX_MANIFEST_BYTES: u64 = 1024 * 1024;
pub(crate) const IO_CHUNK: usize = 64 * 1024;
const MAX_OWNERS: usize = 4;
const MAX_RECORDS: usize = 64;
const MAX_RECORD_BODY: usize = 16 * 1024 - 36;

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct FileIdentity {
    pub device: u64,
    pub object: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum NativeErrorKind {
    Cancelled,
    Deadline,
    Quota,
    Conflict,
    Unsupported,
    Changed,
    Io,
    PublishedUnconfirmed,
}
#[derive(Debug)]
struct NativeControlError(NativeErrorKind);
impl std::fmt::Display for NativeControlError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Physical snapshot operation: {:?}", self.0)
    }
}
impl std::error::Error for NativeControlError {}
fn require_unchanged(condition: bool, reason: &'static str) -> Result<()> {
    if condition {
        Ok(())
    } else {
        Err(NativeControlError(NativeErrorKind::Changed)).context(reason)
    }
}
fn unsupported(reason: &'static str) -> anyhow::Error {
    anyhow::Error::new(NativeControlError(NativeErrorKind::Unsupported)).context(reason)
}
pub(crate) fn enforce_wal_growth(bytes: u64, baseline: u64) -> Result<()> {
    if bytes > baseline.saturating_add(64 * 1024 * 1024) {
        return Err(NativeControlError(NativeErrorKind::Quota))
            .context("Snapshot WAL growth exceeded its 64 MiB budget");
    }
    Ok(())
}
pub(crate) fn error_kind(error: &anyhow::Error) -> NativeErrorKind {
    if let Some(kind) = error
        .chain()
        .find_map(|e| e.downcast_ref::<NativeControlError>())
    {
        return kind.0;
    }
    if error.chain().any(|e| {
        e.downcast_ref::<io::Error>()
            .is_some_and(|e| e.kind() == io::ErrorKind::AlreadyExists)
    }) {
        return NativeErrorKind::Conflict;
    }
    NativeErrorKind::Io
}
fn checked_callback<T>(operation: impl FnOnce() -> Result<T>) -> Result<T> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(operation))
        .unwrap_or_else(|_| Err(anyhow::anyhow!("Snapshot journal callback panicked")))
}

#[derive(Clone)]
pub(crate) struct SnapshotContext {
    cancel: Arc<AtomicBool>,
    deadline: Instant,
}
impl SnapshotContext {
    pub(crate) fn new(timeout: Duration) -> Result<Self> {
        ensure!(
            !timeout.is_zero() && timeout <= Duration::from_secs(120),
            "Snapshot timeout must be positive and at most 120 seconds"
        );
        Ok(Self {
            cancel: Arc::new(AtomicBool::new(false)),
            deadline: Instant::now() + timeout,
        })
    }
    pub(crate) fn cancel(&self) {
        self.cancel.store(true, Ordering::Release);
    }
    pub(crate) fn check(&self) -> Result<()> {
        if self.cancel.load(Ordering::Acquire) {
            return Err(NativeControlError(NativeErrorKind::Cancelled).into());
        }
        if Instant::now() >= self.deadline {
            return Err(NativeControlError(NativeErrorKind::Deadline).into());
        }
        Ok(())
    }
}

#[derive(Default)]
struct FlightState {
    active: usize,
    reserved: u64,
}
#[derive(Clone)]
pub(crate) struct SnapshotLimiter {
    state: Arc<Mutex<FlightState>>,
    aggregate: u64,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SnapshotFlightStats {
    pub active: usize,
    pub reserved_bytes: u64,
}
impl SnapshotLimiter {
    pub(crate) fn new(aggregate_maximum: u64) -> Result<Self> {
        ensure!(
            aggregate_maximum > 0 && aggregate_maximum <= MAX_SNAPSHOT_BYTES * MAX_OWNERS as u64,
            "Invalid snapshot aggregate reservation"
        );
        Ok(Self {
            state: Arc::new(Mutex::new(FlightState::default())),
            aggregate: aggregate_maximum,
        })
    }
    pub(crate) fn stats(&self) -> SnapshotFlightStats {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        SnapshotFlightStats {
            active: state.active,
            reserved_bytes: state.reserved,
        }
    }
    pub(crate) fn reserve_job(
        &self,
        max_image: u64,
        workspace_reserved: u64,
    ) -> Result<SnapshotPermit> {
        let required = max_image
            .checked_mul(3)
            .and_then(|n| n.checked_add(64 * 1024 * 1024 + MAX_MANIFEST_BYTES));
        if max_image == 0
            || max_image > MAX_SNAPSHOT_BYTES
            || required.is_none_or(|n| workspace_reserved < n)
        {
            return Err(NativeControlError(NativeErrorKind::Quota).into());
        }
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.active >= MAX_OWNERS
            || workspace_reserved > self.aggregate.saturating_sub(state.reserved)
        {
            return Err(NativeControlError(NativeErrorKind::Quota).into());
        }
        state.active += 1;
        state.reserved += workspace_reserved;
        Ok(SnapshotPermit(Arc::new(JobReservation {
            limiter: self.clone(),
            maximum: max_image,
            bytes: workspace_reserved,
            files: Mutex::new(JobFiles::default()),
        })))
    }
}
#[derive(Default)]
struct JobFiles {
    active: usize,
    bytes: u64,
}
struct JobReservation {
    limiter: SnapshotLimiter,
    maximum: u64,
    bytes: u64,
    files: Mutex<JobFiles>,
}
/// Clones share a single Root operation owner and one bounded workspace allowance.
#[derive(Clone)]
pub(crate) struct SnapshotPermit(Arc<JobReservation>);
impl Drop for JobReservation {
    fn drop(&mut self) {
        let mut state = self.limiter.state.lock().unwrap_or_else(|e| e.into_inner());
        state.active -= 1;
        state.reserved -= self.bytes;
    }
}
struct FileCharge {
    permit: SnapshotPermit,
    bytes: u64,
}
impl SnapshotPermit {
    fn subfile(&self, maximum: u64, manifest: bool) -> Result<FileCharge> {
        if maximum == 0
            || maximum
                > if manifest {
                    MAX_MANIFEST_BYTES
                } else {
                    self.0.maximum
                }
        {
            return Err(NativeControlError(NativeErrorKind::Quota).into());
        }
        let mut files = self.0.files.lock().unwrap_or_else(|e| e.into_inner());
        if files.active >= 16 {
            return Err(NativeControlError(NativeErrorKind::Quota).into());
        }
        files.active += 1;
        Ok(FileCharge {
            permit: self.clone(),
            bytes: 0,
        })
    }
}
impl FileCharge {
    fn resize(&mut self, bytes: u64) -> Result<()> {
        let mut files = self
            .permit
            .0
            .files
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let other = files.bytes - self.bytes;
        if bytes > self.permit.0.bytes.saturating_sub(other) {
            return Err(NativeControlError(NativeErrorKind::Quota).into());
        }
        files.bytes = other + bytes;
        self.bytes = bytes;
        Ok(())
    }
}
impl Drop for FileCharge {
    fn drop(&mut self) {
        let mut files = self
            .permit
            .0
            .files
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        files.active -= 1;
        files.bytes -= self.bytes;
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DirectoryPurpose {
    Private,
    Selected,
    ReadOnly,
}
struct DirectoryInner {
    path: PathBuf,
    pins: Vec<File>,
    identities: Vec<FileIdentity>,
    epoch: u64,
    purpose: DirectoryPurpose,
    revoked: AtomicBool,
}
#[derive(Clone)]
pub(crate) struct SnapshotDirectory(Arc<DirectoryInner>);
impl SnapshotDirectory {
    #[cfg(windows)]
    pub(crate) fn open_tag_staging(profile: &Self) -> Result<Self> {
        profile.verify()?;
        platform::ensure_private_directory(profile.parent(), "tag-edits")?;
        profile.verify()?;
        let staging = Self::open(
            &profile.path().join("tag-edits"),
            profile.epoch(),
            DirectoryPurpose::Private,
        )?;
        platform::validate_private_directory(staging.parent())?;
        staging.verify()?;
        profile.verify()?;
        Ok(staging)
    }
    #[cfg(windows)]
    pub(crate) fn open_group_file(&self, name: &str, create: bool) -> Result<File> {
        self.verify()?;
        validate_basename(name)?;
        // The coordinator retains the returned descriptor before fallible
        // inspection; allocation identity is attested through group_file_identity.
        platform::open_group_file(self.parent(), name, create)
    }
    #[cfg(windows)]
    pub(crate) fn rename_group_file(
        &self,
        file: &File,
        name: &str,
        identity: &FileIdentity,
    ) -> Result<()> {
        self.verify()?;
        validate_basename(name)?;
        platform::validate(file, false)?;
        ensure!(
            platform::links(file)? == 1,
            "Profile group file is aliased before rename"
        );
        platform::publish(file, self.parent(), name, identity)?;
        platform::sync_directory(self.parent())?;
        platform::verify_name(self.parent(), name, identity)?;
        self.verify()
    }
    #[cfg(windows)]
    pub(crate) fn group_file_identity(&self, file: &File) -> Result<FileIdentity> {
        self.verify()?;
        platform::validate(file, false)?;
        ensure!(
            platform::links(file)? == 1,
            "Profile group file identity is aliased"
        );
        platform::identity(file)
    }
    #[cfg(windows)]
    pub(crate) fn verify_group_name(&self, name: &str, identity: &FileIdentity) -> Result<()> {
        self.verify()?;
        platform::verify_name(self.parent(), name, identity)
    }
    #[cfg(windows)]
    pub(crate) fn require_group_absent(&self, name: &str) -> Result<()> {
        self.verify()?;
        validate_basename(name)?;
        platform::require_absent(self.parent(), name)?;
        self.verify()
    }
    pub(crate) fn pin_profile(path: impl AsRef<Path>, epoch: u64) -> Result<Self> {
        Self::open(path.as_ref(), epoch, DirectoryPurpose::ReadOnly)
    }
    /// Opens a profile-owned staging child through a continuously pinned profile.
    pub(crate) fn open_private(profile_root: impl AsRef<Path>, epoch: u64) -> Result<Self> {
        let profile = Self::open(profile_root.as_ref(), epoch, DirectoryPurpose::ReadOnly)?;
        profile.verify()?;
        platform::ensure_private_directory(profile.parent(), ".lx-snapshot-staging")?;
        profile.verify()?;
        let directory = Self::open(
            &profile.path().join(".lx-snapshot-staging"),
            epoch,
            DirectoryPurpose::Private,
        )?;
        platform::validate_private_directory(directory.parent())?;
        Ok(directory)
    }
    /// A fresh explicit write selection; no library read grant is accepted here.
    pub(crate) fn open_selected(path: impl AsRef<Path>, epoch: u64) -> Result<Self> {
        let directory = Self::open(path.as_ref(), epoch, DirectoryPurpose::Selected)?;
        #[cfg(windows)]
        platform::validate_private_directory(directory.parent())?;
        Ok(directory)
    }
    fn open(path: &Path, epoch: u64, purpose: DirectoryPurpose) -> Result<Self> {
        ensure!(
            epoch > 0
                && path.is_absolute()
                && path.components().count() <= 64
                && path.as_os_str().to_string_lossy().encode_utf16().count() <= 32000
                && path
                    .components()
                    .all(|c| !matches!(c, Component::CurDir | Component::ParentDir)),
            "Invalid snapshot directory"
        );
        let (path, pins) = platform::pin_directory(path)?;
        let identities = pins
            .iter()
            .map(platform::identity)
            .collect::<Result<Vec<_>>>()?;
        let directory = Self(Arc::new(DirectoryInner {
            path,
            pins,
            identities,
            epoch,
            purpose,
            revoked: AtomicBool::new(false),
        }));
        directory.verify()?;
        Ok(directory)
    }
    pub(crate) fn path(&self) -> &Path {
        &self.0.path
    }
    pub(crate) fn identity(&self) -> &FileIdentity {
        self.0.identities.last().expect("Pinned snapshot directory")
    }
    pub(crate) fn epoch(&self) -> u64 {
        self.0.epoch
    }
    pub(crate) fn revoke(&self) {
        self.0.revoked.store(true, Ordering::Release);
    }
    fn parent(&self) -> &File {
        self.0.pins.last().expect("Pinned snapshot directory")
    }
    pub(crate) fn verify(&self) -> Result<()> {
        require_unchanged(
            !self.0.revoked.load(Ordering::Acquire),
            "Snapshot actual-write scope was revoked",
        )?;
        for (pin, expected) in self.0.pins.iter().zip(&self.0.identities) {
            platform::validate(pin, true)?;
            require_unchanged(
                platform::identity(pin)? == *expected,
                "Snapshot directory identity changed",
            )?;
        }
        platform::verify_directory(self.path(), &self.0.pins)
    }
    fn require_write(&self, purpose: DirectoryPurpose) -> Result<()> {
        self.verify()?;
        ensure!(
            self.0.purpose == purpose,
            "Snapshot directory lacks the required actual-write scope"
        );
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct Stamp {
    changed: i128,
    written: i128,
    length: u64,
}
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub(crate) struct CalibrationMetadata {
    pub temporary_name: String,
    pub identity: Option<FileIdentity>,
    pub owner_handle_open: bool,
    pub reader_handle_open: bool,
    pub disposition_requested: bool,
    pub zero_link_witness: bool,
}
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub(crate) struct ActualFileMetadata {
    pub operation_id: String,
    pub nonce: String,
    pub temporary_name: Option<String>,
    pub directory: PathBuf,
    pub directory_identity: FileIdentity,
    pub selection_epoch: u64,
    pub identity: Option<FileIdentity>,
    pub bytes: u64,
    pub published: bool,
    pub final_basename: Option<String>,
    pub cleanup_probe: Option<CalibrationMetadata>,
}
#[derive(Clone, Debug)]
pub(crate) struct SnapshotReceipt {
    pub operation_id: String,
    pub path: PathBuf,
    pub identity: FileIdentity,
    pub bytes: u64,
    pub sha256: String,
    pub directory_identity: FileIdentity,
    pub selection_epoch: u64,
}
/// Retaining this value retains every private output/probe still needing action.
pub(crate) struct NativeFailure {
    pub kind: NativeErrorKind,
    pub message: String,
    pub file: Option<Box<ActualFile>>,
    pub manifest: Option<Box<DurableManifest>>,
    pub source: Option<Box<SourceFileProof>>,
}
impl std::fmt::Debug for NativeFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NativeFailure")
            .field("kind", &self.kind)
            .field("message", &self.message)
            .field("owns_file", &self.file.is_some())
            .field("owns_manifest", &self.manifest.is_some())
            .field("owns_source", &self.source.is_some())
            .finish()
    }
}
impl std::fmt::Display for NativeFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}
impl std::error::Error for NativeFailure {}
impl NativeFailure {
    fn new(error: anyhow::Error, file: Option<ActualFile>) -> Self {
        let kind = if file.as_ref().is_some_and(|f| f.metadata.published) {
            NativeErrorKind::PublishedUnconfirmed
        } else if let Some(e) = error
            .chain()
            .find_map(|e| e.downcast_ref::<NativeControlError>())
        {
            e.0
        } else if error.chain().any(|e| {
            e.downcast_ref::<io::Error>()
                .is_some_and(|e| e.kind() == io::ErrorKind::AlreadyExists)
        }) {
            NativeErrorKind::Conflict
        } else if error.to_string().contains("UnsupportedCapability") {
            NativeErrorKind::Unsupported
        } else {
            NativeErrorKind::Io
        };
        Self {
            kind,
            message: format!("{error:#}").chars().take(2048).collect(),
            file: file.map(Box::new),
            manifest: None,
            source: None,
        }
    }
}

/// Owns one exact native read/write descriptor and its Root reservation.
/// Drop closes handles; it never removes a path, foreign leaf, or published file.
pub(crate) struct ActualFile {
    file: Option<File>,
    directory: SnapshotDirectory,
    metadata: ActualFileMetadata,
    stamp: Option<Stamp>,
    maximum: u64,
    verified_sha: Option<String>,
    publication_ready: bool,
    inventory_attested: bool,
    deletion_requested: bool,
    #[cfg(windows)]
    cleanup_state: platform::CleanupState,
    charge: FileCharge,
}
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub(crate) struct DurableCleanupFact {
    pub file: ActualFileMetadata,
    stamp: Stamp,
    sha256: String,
    pending_zero_links: bool,
    pub removed: bool,
}
impl DurableCleanupFact {
    pub(crate) fn pending_zero_links(&self) -> bool {
        self.pending_zero_links
    }
}
pub(crate) struct ColdCleanup {
    directory: SnapshotDirectory,
    fact: DurableCleanupFact,
    #[cfg(windows)]
    state: platform::CleanupState,
}
impl ColdCleanup {
    pub(crate) fn can_release_for_recovery(&self) -> bool {
        #[cfg(windows)]
        {
            self.state.probe_metadata().is_none()
        }
        #[cfg(not(windows))]
        {
            true
        }
    }
    pub(crate) fn rebind(&mut self, fresh: &SnapshotDirectory) -> Result<()> {
        fresh.verify()?;
        require_unchanged(
            fresh.identity() == &self.fact.file.directory_identity,
            "Cold cleanup fresh scope changed actual root",
        )?;
        self.directory = fresh.clone();
        Ok(())
    }
    pub(crate) fn prepare(directory: SnapshotDirectory, fact: DurableCleanupFact) -> Result<Self> {
        directory.verify()?;
        require_unchanged(
            directory.identity() == &fact.file.directory_identity,
            "Cold cleanup authority identifies another actual directory",
        )?;
        ensure!(
            !fact.file.published
                && fact.pending_zero_links
                && fact.sha256.len() == 64
                && fact.sha256.bytes().all(|b| b.is_ascii_hexdigit()),
            "Cold cleanup has no durable original disposition witness"
        );
        Ok(Self {
            directory,
            fact,
            #[cfg(windows)]
            state: platform::CleanupState::default(),
        })
    }
    pub(crate) fn confirm(&mut self, context: &SnapshotContext) -> Result<DurableCleanupFact> {
        self.directory.verify()?;
        context.check()?;
        #[cfg(windows)]
        {
            platform::confirm_cold_cleanup(&mut self.state, &self.directory, &self.fact, context)?;
            let mut fact = self.fact.clone();
            fact.removed = true;
            Ok(fact)
        }
        #[cfg(not(windows))]
        {
            Err(unsupported("Cold cleanup attestation is Windows-only"))
        }
    }
}
/// Read-only inspection retains no write capability. A trusted coordinator must
/// authorize recovery before consuming this probe to acquire an exclusive FH.
pub(crate) struct RecoveryProbe {
    file: File,
    directory: SnapshotDirectory,
    metadata: ActualFileMetadata,
    maximum: u64,
    stamp: Stamp,
}
impl RecoveryProbe {
    pub(crate) fn read_manifest(&self, context: &SnapshotContext) -> Result<Vec<PhaseFact>> {
        ensure!(
            self.maximum == MAX_MANIFEST_BYTES && self.stamp.length <= MAX_MANIFEST_BYTES,
            "Recovery object is not a bounded manifest"
        );
        let mut bytes = vec![0; self.stamp.length as usize];
        let mut at = 0;
        while at < bytes.len() {
            context.check()?;
            self.directory.verify()?;
            require_unchanged(
                platform::stamp(&self.file)? == self.stamp,
                "Recovery manifest changed",
            )?;
            let end = (at + IO_CHUNK).min(bytes.len());
            let n = platform::read_at(&self.file, &mut bytes[at..end], at as u64)?;
            ensure!(n > 0, "Recovery manifest was truncated");
            at += n;
        }
        context.check()?;
        self.directory.verify()?;
        require_unchanged(
            platform::stamp(&self.file)? == self.stamp,
            "Recovery manifest changed",
        )?;
        parse_phase_facts(&bytes)
    }
}
impl ActualFile {
    /// Transfers an already attested profile-group FH into the same calibrated
    /// retirement engine. All fallible admission precedes taking the descriptor.
    #[cfg(windows)]
    pub(crate) fn adopt_group_file(
        file: &mut Option<File>,
        directory: &SnapshotDirectory,
        metadata: ActualFileMetadata,
        sha256: &str,
        context: &SnapshotContext,
        permit: &SnapshotPermit,
    ) -> Result<Self> {
        context.check()?;
        directory.verify()?;
        validate_operation(&metadata.operation_id)?;
        let name = metadata
            .temporary_name
            .as_deref()
            .context("Group retirement name is absent")?;
        validate_basename(name)?;
        ensure!(
            metadata.nonce.len() == 32
                && metadata.nonce.bytes().all(|b| b.is_ascii_hexdigit())
                && name.starts_with(&format!(".lx-profile-{}", metadata.nonce))
                && !metadata.published
                && metadata.final_basename.is_none()
                && metadata.directory_identity == *directory.identity()
                && metadata.directory == directory.path()
                && metadata.bytes <= permit.0.bytes
                && sha256.len() == 64
                && sha256.bytes().all(|b| b.is_ascii_hexdigit()),
            "Group retirement does not identify its admitted private object"
        );
        let native = file
            .as_ref()
            .context("Group retirement descriptor is absent")?;
        let identity = metadata
            .identity
            .as_ref()
            .context("Group retirement identity is absent")?;
        platform::validate(native, false)?;
        require_unchanged(
            platform::identity(native)? == *identity && platform::links(native)? == 1,
            "Group retirement actual object changed or is aliased",
        )?;
        directory.verify_group_name(name, identity)?;
        let stamp = platform::stamp(native)?;
        require_unchanged(
            stamp.length == metadata.bytes,
            "Group retirement actual length changed",
        )?;
        let mut at = 0u64;
        let mut buffer = [0u8; IO_CHUNK];
        let mut hash = Sha256::new();
        while at < stamp.length {
            context.check()?;
            let wanted = (stamp.length - at).min(IO_CHUNK as u64) as usize;
            let read = platform::read_at(native, &mut buffer[..wanted], at)?;
            ensure!(read > 0, "Group retirement body was truncated");
            hash.update(&buffer[..read]);
            at += read as u64;
        }
        require_unchanged(
            platform::stamp(native)? == stamp
                && format!("{:x}", hash.finalize()).eq_ignore_ascii_case(sha256),
            "Group retirement actual body differs from its approved attestation",
        )?;
        let mut charge = permit.subfile(metadata.bytes.min(permit.0.maximum).max(1), false)?;
        charge.resize(metadata.bytes)?;
        let maximum = metadata.bytes.max(1);
        Ok(Self {
            file: file.take(),
            directory: directory.clone(),
            metadata,
            stamp: Some(stamp),
            maximum,
            verified_sha: Some(sha256.to_owned()),
            publication_ready: false,
            inventory_attested: true,
            deletion_requested: false,
            cleanup_state: platform::CleanupState::default(),
            charge,
        })
    }
    pub(crate) fn can_release_for_recovery(&self) -> bool {
        if !self.inventory_attested {
            return false;
        }
        #[cfg(windows)]
        {
            self.cleanup_state.probe_metadata().is_none()
                && (!self.cleanup_state.disposition_started() || self.deletion_requested)
        }
        #[cfg(not(windows))]
        {
            true
        }
    }
    pub(crate) fn inspect_recovery(
        directory: &SnapshotDirectory,
        recorded: &ActualFileMetadata,
        maximum: u64,
        context: &SnapshotContext,
    ) -> Result<RecoveryProbe> {
        context.check()?;
        directory.verify()?;
        validate_operation(&recorded.operation_id)?;
        require_unchanged(
            *directory.identity() == recorded.directory_identity,
            "Recovery directory is a different actual object",
        )?;
        let identity = recorded
            .identity
            .as_ref()
            .context("Allocation has no durable actual FID; explicit recovery is required")?;
        ensure!(
            recorded.nonce.len() == 32 && recorded.nonce.bytes().all(|b| b.is_ascii_hexdigit()),
            "Invalid recovery nonce"
        );
        let name = if recorded.published {
            let name = recorded
                .final_basename
                .as_deref()
                .context("Recovery final name missing")?;
            validate_basename(name)?;
            name
        } else {
            let name = recorded
                .temporary_name
                .as_deref()
                .context("Anonymous staging cannot be recovered by path")?;
            ensure!(
                name == format!(".lx-snapshot-{}.part", recorded.nonce)
                    || name == format!(".lx-snapshot-{}.manifest", recorded.nonce),
                "Invalid recovery staging leaf"
            );
            name
        };
        let file = platform::open_source(directory.parent(), name)?;
        platform::validate(&file, false)?;
        require_unchanged(
            platform::identity(&file)? == *identity && platform::links(&file)? == 1,
            "Recovery path is foreign or aliased",
        )?;
        platform::verify_name(directory.parent(), name, identity)?;
        let stamp = platform::stamp(&file)?;
        ensure!(
            maximum > 0 && maximum <= MAX_SNAPSHOT_BYTES && stamp.length <= maximum,
            "Recovery object exceeds its reserved bound"
        );
        context.check()?;
        directory.verify()?;
        let mut metadata = recorded.clone();
        metadata.bytes = stamp.length;
        Ok(RecoveryProbe {
            file,
            directory: directory.clone(),
            metadata,
            maximum,
            stamp,
        })
    }
    pub(crate) fn reopen_recovery(
        probe: RecoveryProbe,
        context: &SnapshotContext,
        permit: &SnapshotPermit,
    ) -> std::result::Result<Self, NativeFailure> {
        let RecoveryProbe {
            file,
            directory,
            metadata,
            maximum,
            stamp,
        } = probe;
        let preparation = (|| -> Result<(FileCharge, File)> {
            context.check()?;
            directory.verify()?;
            require_unchanged(
                platform::stamp(&file)? == stamp,
                "Recovery object changed after authorization",
            )?;
            let mut charge = permit.subfile(
                maximum,
                maximum == MAX_MANIFEST_BYTES
                    && metadata
                        .temporary_name
                        .as_deref()
                        .is_some_and(|n| n.ends_with(".manifest")),
            )?;
            charge.resize(stamp.length)?;
            let name = metadata
                .final_basename
                .as_deref()
                .filter(|_| metadata.published)
                .or(metadata.temporary_name.as_deref())
                .context("Recovery leaf missing")?;
            // The exclusive reopen itself is checked before any writer call;
            // the read-only probe deliberately cannot be upgraded in place.
            drop(file);
            let opened = platform::open_recovery(directory.parent(), name)?;
            Ok((charge, opened))
        })();
        let (charge, file) = preparation.map_err(|e| NativeFailure::new(e, None))?;
        let mut owner = Self {
            file: Some(file),
            directory,
            metadata,
            stamp: None,
            maximum,
            verified_sha: None,
            publication_ready: false,
            inventory_attested: true,
            deletion_requested: false,
            #[cfg(windows)]
            cleanup_state: platform::CleanupState::default(),
            charge,
        };
        let checked = (|| -> Result<()> {
            require_unchanged(
                platform::identity(owner.file()?)? == *owner.identity()?,
                "Recovery writer opened a foreign object",
            )?;
            require_unchanged(
                platform::stamp(owner.file()?)? == stamp,
                "Recovery representation changed before writer open",
            )?;
            owner.stamp = Some(stamp);
            owner.verify(context)
        })();
        match checked {
            Ok(()) => Ok(owner),
            Err(e) => Err(NativeFailure::new(e, Some(owner))),
        }
    }
    pub(crate) fn restore_sealed(
        &mut self,
        expected: &str,
        context: &SnapshotContext,
    ) -> Result<()> {
        ensure!(
            expected.len() == 64 && expected.bytes().all(|b| b.is_ascii_hexdigit()),
            "Invalid compact digest"
        );
        require_unchanged(
            self.seal_sha256(context)? == expected,
            "Recovered compact image differs from its durable SHA",
        )?;
        self.verified_sha = Some(expected.into());
        self.publication_ready = true;
        Ok(())
    }
    /// Inventory is durable before creation and again before the first body
    /// write. An interrupted allocation without a recorded FID is unresolved;
    /// recovery never treats its nonce as permission to delete a path.
    pub(crate) fn create_journaled(
        directory: &SnapshotDirectory,
        operation_id: &str,
        maximum: u64,
        context: &SnapshotContext,
        permit: &SnapshotPermit,
        inventory: &dyn Fn(&ActualFileMetadata) -> Result<()>,
    ) -> std::result::Result<Self, NativeFailure> {
        Self::allocate_observed(
            directory,
            operation_id,
            maximum,
            false,
            context,
            permit,
            inventory,
        )
    }
    #[cfg(test)]
    pub(crate) fn create(
        directory: &SnapshotDirectory,
        operation_id: &str,
        maximum: u64,
        context: &SnapshotContext,
        permit: &SnapshotPermit,
    ) -> std::result::Result<Self, NativeFailure> {
        Self::allocate(directory, operation_id, maximum, false, context, permit)
    }
    fn allocate(
        directory: &SnapshotDirectory,
        operation_id: &str,
        maximum: u64,
        manifest: bool,
        context: &SnapshotContext,
        permit: &SnapshotPermit,
    ) -> std::result::Result<Self, NativeFailure> {
        Self::allocate_observed(
            directory,
            operation_id,
            maximum,
            manifest,
            context,
            permit,
            &|_| Ok(()),
        )
    }
    fn allocate_observed(
        directory: &SnapshotDirectory,
        operation_id: &str,
        maximum: u64,
        manifest: bool,
        context: &SnapshotContext,
        permit: &SnapshotPermit,
        inventory: &dyn Fn(&ActualFileMetadata) -> Result<()>,
    ) -> std::result::Result<Self, NativeFailure> {
        let observed = |metadata: &ActualFileMetadata| checked_callback(|| inventory(metadata));
        let preparation = (|| -> Result<(FileCharge, String, String, File)> {
            context.check()?;
            directory.require_write(DirectoryPurpose::Private)?;
            validate_operation(operation_id)?;
            let charge = permit.subfile(maximum, manifest)?;
            let nonce = nonce()?;
            let name = format!(
                ".lx-snapshot-{nonce}.{}",
                if manifest { "manifest" } else { "part" }
            );
            observed(&ActualFileMetadata {
                operation_id: operation_id.into(),
                nonce: nonce.clone(),
                temporary_name: Some(name.clone()),
                directory: directory.path().into(),
                directory_identity: directory.identity().clone(),
                selection_epoch: directory.epoch(),
                identity: None,
                bytes: 0,
                published: false,
                final_basename: None,
                cleanup_probe: None,
            })?;
            let file = if manifest {
                platform::create_manifest(directory.parent(), &name)?
            } else {
                platform::create_private(directory.parent(), Some(&name))?
            };
            Ok((charge, nonce, name, file))
        })();
        let (charge, nonce, name, file) = preparation.map_err(|e| NativeFailure::new(e, None))?;
        #[cfg(windows)]
        let temporary_name = Some(name);
        #[cfg(not(windows))]
        let temporary_name = if manifest { Some(name) } else { None };
        let mut owner = Self {
            file: Some(file),
            directory: directory.clone(),
            metadata: ActualFileMetadata {
                operation_id: operation_id.into(),
                nonce,
                temporary_name,
                directory: directory.path().to_path_buf(),
                directory_identity: directory.identity().clone(),
                selection_epoch: directory.epoch(),
                identity: None,
                bytes: 0,
                published: false,
                final_basename: None,
                cleanup_probe: None,
            },
            stamp: None,
            maximum,
            verified_sha: None,
            publication_ready: false,
            inventory_attested: false,
            deletion_requested: false,
            #[cfg(windows)]
            cleanup_state: platform::CleanupState::default(),
            charge,
        };
        let initialized = owner
            .initialize(context)
            .and_then(|()| observed(&owner.metadata()));
        match initialized {
            Ok(()) => {
                owner.inventory_attested = true;
                Ok(owner)
            }
            Err(e) => Err(NativeFailure::new(e, Some(owner))),
        }
    }
    fn file(&self) -> Result<&File> {
        self.file
            .as_ref()
            .context("Snapshot actual descriptor is closed")
    }
    fn identity(&self) -> Result<&FileIdentity> {
        self.metadata
            .identity
            .as_ref()
            .context("Snapshot native identity is not verified")
    }
    fn initialize(&mut self, context: &SnapshotContext) -> Result<()> {
        context.check()?;
        self.directory.verify()?;
        platform::validate(self.file()?, false)?;
        self.metadata.identity = Some(platform::identity(self.file()?)?);
        self.stamp = Some(platform::stamp(self.file()?)?);
        self.verify(context)
    }
    pub(crate) fn metadata(&self) -> ActualFileMetadata {
        #[cfg(windows)]
        {
            ActualFileMetadata {
                cleanup_probe: self.cleanup_state.probe_metadata(),
                ..self.metadata.clone()
            }
        }
        #[cfg(not(windows))]
        {
            self.metadata.clone()
        }
    }
    fn verify_object(&self, context: &SnapshotContext) -> Result<()> {
        context.check()?;
        self.directory.verify()?;
        ensure!(
            !self.deletion_requested,
            "Snapshot descriptor awaits deletion confirmation"
        );
        platform::validate(self.file()?, false)?;
        require_unchanged(
            platform::identity(self.file()?)? == *self.identity()?,
            "Snapshot actual file identity changed",
        )?;
        let expected_links =
            u64::from(self.metadata.temporary_name.is_some() || self.metadata.published);
        require_unchanged(
            platform::links(self.file()?)? == expected_links,
            "Snapshot actual descriptor has unexpected aliases",
        )?;
        if let Some(name) = self
            .metadata
            .final_basename
            .as_deref()
            .filter(|_| self.metadata.published)
        {
            platform::verify_name(self.directory.parent(), name, self.identity()?)?;
        } else if let Some(name) = &self.metadata.temporary_name {
            platform::verify_name(self.directory.parent(), name, self.identity()?)?;
        }
        context.check()
    }
    pub(crate) fn verify(&self, context: &SnapshotContext) -> Result<()> {
        self.verify_object(context)?;
        require_unchanged(
            Some(platform::stamp(self.file()?)?) == self.stamp,
            "Snapshot representation changed outside its checked descriptor",
        )?;
        context.check()
    }
    pub(crate) fn len(&self, context: &SnapshotContext) -> Result<u64> {
        self.verify(context)?;
        Ok(self.metadata.bytes)
    }
    pub(crate) fn read_at(
        &self,
        offset: u64,
        bytes: &mut [u8],
        context: &SnapshotContext,
    ) -> Result<usize> {
        ensure!(
            bytes.len() <= IO_CHUNK,
            "Snapshot native read exceeds 64 KiB"
        );
        self.verify(context)?;
        // SQLite may request a page beyond EOF and expects its VFS to zero-fill
        // a short read. Reads allocate no storage and remain bounded per call.
        ensure!(
            offset.checked_add(bytes.len() as u64).is_some(),
            "Snapshot read offset overflow"
        );
        let read = platform::read_at(self.file()?, bytes, offset)?;
        self.verify(context)?;
        Ok(read)
    }
    pub(crate) fn write_at(
        &mut self,
        offset: u64,
        bytes: &[u8],
        context: &SnapshotContext,
    ) -> Result<usize> {
        ensure!(
            bytes.len() <= IO_CHUNK,
            "Snapshot native write exceeds 64 KiB"
        );
        self.verify(context)?;
        ensure!(
            !self.metadata.published && !self.publication_ready,
            "Sealed snapshot cannot be modified"
        );
        if offset > self.maximum || bytes.len() as u64 > self.maximum.saturating_sub(offset) {
            return Err(NativeControlError(NativeErrorKind::Quota).into());
        }
        self.charge
            .resize(self.metadata.bytes.max(offset + bytes.len() as u64))?;
        let result = platform::write_at(self.file()?, bytes, offset);
        // Capture the resulting object even after an interrupted native write.
        self.stamp = Some(platform::stamp(self.file()?)?);
        self.metadata.bytes = self.stamp.as_ref().unwrap().length;
        self.charge.resize(self.metadata.bytes)?;
        self.verified_sha = None;
        let written = result?;
        self.verify(context)?;
        Ok(written)
    }
    pub(crate) fn set_len(&mut self, length: u64, context: &SnapshotContext) -> Result<()> {
        self.verify(context)?;
        ensure!(
            !self.metadata.published && !self.publication_ready,
            "Sealed snapshot cannot be truncated"
        );
        if length > self.maximum {
            return Err(NativeControlError(NativeErrorKind::Quota).into());
        }
        self.charge.resize(self.metadata.bytes.max(length))?;
        let result = self.file()?.set_len(length);
        self.stamp = Some(platform::stamp(self.file()?)?);
        self.metadata.bytes = self.stamp.as_ref().unwrap().length;
        self.charge.resize(self.metadata.bytes)?;
        self.verified_sha = None;
        result?;
        self.verify(context)
    }
    pub(crate) fn sync_all(&self, context: &SnapshotContext) -> Result<()> {
        self.verify(context)?;
        self.file()?.sync_all()?;
        self.verify(context)
    }
    pub(crate) fn seal_sha256(&mut self, context: &SnapshotContext) -> Result<String> {
        self.sync_all(context)?;
        let length = self.metadata.bytes;
        let mut digest = Sha256::new();
        let mut offset = 0;
        let mut buffer = [0u8; IO_CHUNK];
        while offset < length {
            let wanted = (length - offset).min(IO_CHUNK as u64) as usize;
            let read = self.read_at(offset, &mut buffer[..wanted], context)?;
            ensure!(
                read > 0,
                "Snapshot was truncated during whole-image verification"
            );
            digest.update(&buffer[..read]);
            offset += read as u64;
        }
        self.verify(context)?;
        let hash = format!("{:x}", digest.finalize());
        if !self.publication_ready {
            self.verified_sha = Some(hash.clone());
        }
        Ok(hash)
    }
    /// The actor calls this only after sanitization, compaction and SQLite checks.
    /// Sealing makes the compact descriptor immutable for every later VFS call.
    pub(crate) fn seal_for_publication(&mut self, context: &SnapshotContext) -> Result<String> {
        self.directory.require_write(DirectoryPurpose::Private)?;
        let hash = self.seal_sha256(context)?;
        self.publication_ready = true;
        Ok(hash)
    }
    /// Copies only a sealed image. Unsanitized staging descriptors cannot create
    /// any object inside the user's selected directory, including on another disk.
    pub(crate) fn copy_verified_to_selected(
        &self,
        selected: &SnapshotDirectory,
        operation_id: &str,
        context: &SnapshotContext,
        permit: &SnapshotPermit,
        inventory: &dyn Fn(&ActualFileMetadata) -> Result<()>,
    ) -> std::result::Result<Self, NativeFailure> {
        let observed = |metadata: &ActualFileMetadata| checked_callback(|| inventory(metadata));
        let preparation = (|| -> Result<(FileCharge, String, String, File)> {
            self.verify(context)?;
            ensure!(
                self.publication_ready && self.verified_sha.is_some(),
                "Snapshot compact image is not sealed for publication"
            );
            selected.require_write(DirectoryPurpose::Selected)?;
            validate_operation(operation_id)?;
            ensure!(
                operation_id == self.metadata.operation_id,
                "Snapshot publication copy belongs to another operation"
            );
            ensure!(
                Arc::ptr_eq(&permit.0, &self.charge.permit.0),
                "Snapshot publication copy belongs to another Root reservation"
            );
            let charge = permit.subfile(self.metadata.bytes.max(1), false)?;
            let nonce = nonce()?;
            let name = format!(".lx-snapshot-{nonce}.part");
            observed(&ActualFileMetadata {
                operation_id: operation_id.into(),
                nonce: nonce.clone(),
                temporary_name: Some(name.clone()),
                directory: selected.path().into(),
                directory_identity: selected.identity().clone(),
                selection_epoch: selected.epoch(),
                identity: None,
                bytes: 0,
                published: false,
                final_basename: None,
                cleanup_probe: None,
            })?;
            let file = platform::create_private(selected.parent(), Some(&name))?;
            Ok((charge, nonce, name, file))
        })();
        let (charge, nonce, name, file) = preparation.map_err(|e| NativeFailure::new(e, None))?;
        #[cfg(windows)]
        let temporary_name = Some(name);
        #[cfg(not(windows))]
        let temporary_name = {
            let _ = name;
            None
        };
        let mut target = Self {
            file: Some(file),
            directory: selected.clone(),
            metadata: ActualFileMetadata {
                operation_id: operation_id.into(),
                nonce,
                temporary_name,
                directory: selected.path().to_path_buf(),
                directory_identity: selected.identity().clone(),
                selection_epoch: selected.epoch(),
                identity: None,
                bytes: 0,
                published: false,
                final_basename: None,
                cleanup_probe: None,
            },
            stamp: None,
            maximum: self.metadata.bytes.max(1),
            verified_sha: None,
            publication_ready: false,
            inventory_attested: false,
            deletion_requested: false,
            #[cfg(windows)]
            cleanup_state: platform::CleanupState::default(),
            charge,
        };
        let copied = (|| -> Result<()> {
            target.initialize(context)?;
            observed(&target.metadata())?;
            target.inventory_attested = true;
            let mut offset = 0;
            let mut buffer = [0u8; IO_CHUNK];
            while offset < self.metadata.bytes {
                let wanted = (self.metadata.bytes - offset).min(IO_CHUNK as u64) as usize;
                let read = self.read_at(offset, &mut buffer[..wanted], context)?;
                ensure!(read > 0, "Sealed snapshot ended during publication copy");
                let mut copied = 0;
                while copied < read {
                    let written =
                        target.write_at(offset + copied as u64, &buffer[copied..read], context)?;
                    ensure!(written > 0, "Snapshot publication copy made no progress");
                    copied += written;
                }
                offset += read as u64;
            }
            let actual = target.seal_sha256(context)?;
            require_unchanged(
                Some(actual) == self.verified_sha,
                "Selected-directory copy differs from the sealed compact image",
            )?;
            target.publication_ready = true;
            self.verify(context)
        })();
        match copied {
            Ok(()) => Ok(target),
            Err(e) => Err(NativeFailure::new(e, Some(target))),
        }
    }
    pub(crate) fn publish(
        &mut self,
        basename: &str,
        context: &SnapshotContext,
    ) -> Result<SnapshotReceipt> {
        validate_basename(basename)?;
        self.directory.require_write(DirectoryPurpose::Selected)?;
        self.verify(context)?;
        ensure!(
            self.publication_ready && !self.metadata.published,
            "Snapshot publication requires a sealed unpublished selected-directory candidate"
        );
        let expected = self
            .verified_sha
            .clone()
            .context("Snapshot whole digest is missing")?;
        self.file()?.sync_all()?;
        self.verify(context)?;
        platform::publish(
            self.file()?,
            self.directory.parent(),
            basename,
            self.identity()?,
        )?;
        // Record the successful syscall before any later fallible inspection.
        self.metadata.published = true;
        self.metadata.temporary_name = None;
        self.metadata.final_basename = Some(basename.into());
        self.stamp = Some(platform::stamp(self.file()?)?);
        self.file()?.sync_all()?;
        platform::sync_directory(self.directory.parent())?;
        require_unchanged(
            self.seal_sha256(context)? == expected,
            "Published snapshot whole digest changed",
        )?;
        self.receipt(expected)
    }
    fn receipt(&self, sha256: String) -> Result<SnapshotReceipt> {
        Ok(SnapshotReceipt {
            operation_id: self.metadata.operation_id.clone(),
            path: self.directory.path().join(
                self.metadata
                    .final_basename
                    .as_deref()
                    .context("Snapshot final name is absent")?,
            ),
            identity: self.identity()?.clone(),
            bytes: self.metadata.bytes,
            sha256,
            directory_identity: self.directory.identity().clone(),
            selection_epoch: self.directory.epoch(),
        })
    }
    /// Rechecks the original published descriptor; it never publishes or deletes.
    pub(crate) fn verify_published(
        &mut self,
        context: &SnapshotContext,
    ) -> Result<SnapshotReceipt> {
        ensure!(self.metadata.published, "Snapshot was not published");
        let expected = self
            .verified_sha
            .clone()
            .context("Published snapshot digest is missing")?;
        self.verify_object(context)?;
        self.stamp = Some(platform::stamp(self.file()?)?);
        self.file()?.sync_all()?;
        platform::sync_directory(self.directory.parent())?;
        require_unchanged(
            self.seal_sha256(context)? == expected,
            "Published snapshot whole digest changed",
        )?;
        self.receipt(expected)
    }
    /// Deletes the unpublished object through the exact Windows descriptor, or
    /// closes an unlinked Linux inode. Published files are never cleanup targets.
    #[cfg(test)]
    pub(crate) fn cleanup(
        self,
        context: &SnapshotContext,
    ) -> std::result::Result<ActualFileMetadata, NativeFailure> {
        self.cleanup_journaled(context, &|_| Ok(()))
    }
    pub(crate) fn cleanup_journaled(
        mut self,
        context: &SnapshotContext,
        record: &dyn Fn(&DurableCleanupFact) -> Result<()>,
    ) -> std::result::Result<ActualFileMetadata, NativeFailure> {
        let observed = |fact: &DurableCleanupFact| checked_callback(|| record(fact));
        let result = (|| -> Result<ActualFileMetadata> {
            ensure!(
                !self.metadata.published,
                "Published snapshot cannot be removed as a temporary file"
            );
            context.check()?;
            self.directory.verify()?;
            if !self.deletion_requested {
                if self.metadata.identity.is_none() {
                    self.metadata.identity = Some(platform::identity(self.file()?)?);
                }
                #[cfg(windows)]
                {
                    if !self.cleanup_state.disposition_started() {
                        self.verify_object(context)?;
                        self.stamp = Some(platform::stamp(self.file()?)?);
                        self.metadata.bytes = self.stamp.as_ref().unwrap().length;
                        ensure!(
                            self.metadata.bytes <= self.maximum,
                            "Snapshot cleanup exceeds its reservation"
                        );
                        self.seal_sha256(context)?;
                    }
                    let expected = self.identity()?.clone();
                    let file = self
                        .file
                        .as_ref()
                        .context("Snapshot cleanup handle is missing")?;
                    platform::prepare_cleanup(
                        &mut self.cleanup_state,
                        file,
                        &expected,
                        &self.directory,
                        context,
                        self.stamp
                            .as_ref()
                            .context("Snapshot cleanup stamp is missing")?,
                        self.verified_sha
                            .as_deref()
                            .context("Snapshot cleanup digest is missing")?,
                    )?;
                    if !self.cleanup_state.disposition_started() {
                        observed(&DurableCleanupFact {
                            file: self.metadata(),
                            stamp: self.stamp.clone().unwrap(),
                            sha256: self.verified_sha.clone().unwrap(),
                            pending_zero_links: false,
                            removed: false,
                        })?;
                    }
                    platform::request_cleanup_disposition(
                        &mut self.cleanup_state,
                        file,
                        &expected,
                        &self.directory,
                        context,
                    )?;
                    observed(&DurableCleanupFact {
                        file: self.metadata(),
                        stamp: self.stamp.clone().unwrap(),
                        sha256: self.verified_sha.clone().unwrap(),
                        pending_zero_links: true,
                        removed: false,
                    })?;
                }
                #[cfg(not(windows))]
                {
                    self.verify_object(context)?;
                    self.stamp = Some(platform::stamp(self.file()?)?);
                    self.metadata.bytes = self.stamp.as_ref().unwrap().length;
                    self.seal_sha256(context)?;
                    platform::delete_private(self.file()?)?;
                }
                self.deletion_requested = true;
            }
            #[cfg(windows)]
            platform::close_retained(&mut self.file)?;
            #[cfg(not(windows))]
            drop(self.file.take());
            #[cfg(windows)]
            platform::confirm_cleanup(
                &self.cleanup_state,
                self.identity()?,
                &self.directory,
                context,
            )?;
            context.check()?;
            observed(&DurableCleanupFact {
                file: self.metadata(),
                stamp: self.stamp.clone().context("Cleanup stamp missing")?,
                sha256: self.verified_sha.clone().context("Cleanup SHA missing")?,
                pending_zero_links: true,
                removed: true,
            })?;
            Ok(self.metadata())
        })();
        match result {
            Ok(metadata) => Ok(metadata),
            Err(e) => Err(NativeFailure::new(e, Some(self))),
        }
    }
    pub(crate) fn rebind_cleanup_directory(&mut self, fresh: &SnapshotDirectory) -> Result<()> {
        fresh.verify()?;
        require_unchanged(
            fresh.identity() == &self.metadata.directory_identity,
            "Cleanup scope changed actual root",
        )?;
        ensure!(
            matches!(
                fresh.0.purpose,
                DirectoryPurpose::Private | DirectoryPurpose::Selected
            ),
            "Cleanup scope lacks actual write authority"
        );
        self.directory = fresh.clone();
        Ok(())
    }
}

fn nonce() -> Result<String> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes)
        .map_err(|e| anyhow::anyhow!("Cannot allocate snapshot nonce: {e}"))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}
fn validate_operation(operation: &str) -> Result<()> {
    ensure!(
        !operation.is_empty()
            && operation.len() <= 256
            && operation.chars().all(|c| !c.is_control()),
        "Invalid snapshot operation identifier"
    );
    Ok(())
}
pub(crate) fn validate_basename(name: &str) -> Result<()> {
    ensure!(
        !name.is_empty()
            && name.len() <= 180
            && !name.ends_with(['.', ' '])
            && !name.contains(['/', '\\', ':', '\0', '*', '?', '"', '<', '>', '|'])
            && name != "."
            && name != ".."
            && name.chars().all(|c| !c.is_control()),
        "Invalid snapshot final basename"
    );
    let stem = name.split('.').next().unwrap_or("").to_ascii_uppercase();
    ensure!(
        !matches!(
            stem.as_str(),
            "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
        ) && !(stem.chars().count() == 4
            && (stem.starts_with("COM") || stem.starts_with("LPT"))
            && matches!(stem.chars().nth(3), Some('1'..='9' | '¹' | '²' | '³'))),
        "Reserved snapshot final basename"
    );
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum SnapshotPhase {
    Created = 0,
    SourceCut = 1,
    Copied = 2,
    Sanitized = 3,
    CompactVerified = 4,
    PublishIntent = 5,
    Published = 6,
    Acknowledged = 7,
}
impl SnapshotPhase {
    fn decode(value: u8) -> Result<Self> {
        Ok(match value {
            0 => Self::Created,
            1 => Self::SourceCut,
            2 => Self::Copied,
            3 => Self::Sanitized,
            4 => Self::CompactVerified,
            5 => Self::PublishIntent,
            6 => Self::Published,
            7 => Self::Acknowledged,
            _ => bail!("Invalid snapshot phase record"),
        })
    }
}
#[derive(Clone, Debug)]
pub(crate) struct PhaseFact {
    pub phase: SnapshotPhase,
    pub checkpoint: bool,
    pub body: Vec<u8>,
}
/// Length/SHA envelopes make a torn final append detectable without treating
/// phase metadata as authority to reacquire, publish, or delete an object.
pub(crate) struct DurableManifest {
    owner: ActualFile,
    records: usize,
    phase: Option<SnapshotPhase>,
    valid_length: u64,
    failed_append: bool,
}
impl DurableManifest {
    pub(crate) fn can_release_for_recovery(&self) -> bool {
        self.owner.can_release_for_recovery()
    }
    pub(crate) fn from_recovered(
        owner: ActualFile,
        context: &SnapshotContext,
    ) -> std::result::Result<Self, NativeFailure> {
        let mut manifest = Self {
            owner,
            records: 0,
            phase: None,
            valid_length: 0,
            failed_append: true,
        };
        let result = (|| -> Result<()> {
            ensure!(
                manifest.owner.maximum == MAX_MANIFEST_BYTES && !manifest.owner.metadata.published,
                "Recovery manifest has the wrong role"
            );
            manifest.reconcile(context)
        })();
        match result {
            Ok(()) => Ok(manifest),
            Err(error) => Err(manifest.retain_failure(error)),
        }
    }
    /// Reconciles a failed append from the same actual object. Complete records
    /// remain immutable; only an incomplete final envelope can be truncated.
    pub(crate) fn reconcile(&mut self, context: &SnapshotContext) -> Result<()> {
        let facts = self.read_facts(context)?;
        let valid = facts
            .iter()
            .try_fold(0u64, |sum, fact| {
                sum.checked_add(37 + fact.body.len() as u64)
            })
            .context("Manifest length overflow")?;
        if self.owner.len(context)? != valid {
            self.owner.set_len(valid, context)?;
            self.owner.sync_all(context)?;
        }
        self.phase = facts.iter().rev().find(|f| !f.checkpoint).map(|f| f.phase);
        self.records = facts.len();
        self.valid_length = valid;
        self.failed_append = false;
        Ok(())
    }
    pub(crate) fn append_checkpoint(
        &mut self,
        fact: &[u8],
        context: &SnapshotContext,
    ) -> Result<()> {
        self.reconcile(context)?;
        let phase = self.phase.context("Manifest inventory requires Created")?;
        self.append_record(phase, true, fact, context)
    }
    pub(crate) fn append_once(
        &mut self,
        phase: SnapshotPhase,
        fact: &[u8],
        context: &SnapshotContext,
    ) -> Result<()> {
        self.reconcile(context)?;
        if self.phase.is_some_and(|p| p as u8 >= phase as u8) {
            let facts = self.read_facts(context)?;
            let existing = facts
                .iter()
                .find(|f| !f.checkpoint && f.phase == phase)
                .context("Manifest phase disappeared")?;
            require_unchanged(
                existing.body == fact,
                "Retry changed an existing manifest fact",
            )?;
            return Ok(());
        }
        self.append_record(phase, false, fact, context)
    }
    pub(crate) fn create(
        private: &SnapshotDirectory,
        operation: &str,
        context: &SnapshotContext,
        permit: &SnapshotPermit,
    ) -> std::result::Result<Self, NativeFailure> {
        let owner = ActualFile::allocate(
            private,
            operation,
            MAX_MANIFEST_BYTES,
            true,
            context,
            permit,
        )?;
        Ok(Self {
            owner,
            records: 0,
            phase: None,
            valid_length: 0,
            failed_append: false,
        })
    }
    pub(crate) fn metadata(&self) -> ActualFileMetadata {
        self.owner.metadata()
    }
    fn append_record(
        &mut self,
        phase: SnapshotPhase,
        checkpoint: bool,
        fact: &[u8],
        context: &SnapshotContext,
    ) -> Result<()> {
        ensure!(
            !self.failed_append,
            "Manifest has an unresolved interrupted append"
        );
        ensure!(
            self.records < MAX_RECORDS && fact.len() < MAX_RECORD_BODY,
            "Snapshot manifest exceeds its bounded record capacity"
        );
        let expected = if checkpoint {
            self.phase.context("Inventory before Created")? as u8
        } else {
            self.phase.map_or(0, |previous| previous as u8 + 1)
        };
        ensure!(
            phase as u8 == expected,
            "Snapshot phase fact is out of order"
        );
        let mut body = Vec::with_capacity(fact.len() + 1);
        body.push(phase as u8 | if checkpoint { 0x80 } else { 0 });
        body.extend_from_slice(fact);
        let length = 36u64 + body.len() as u64;
        ensure!(
            length <= MAX_MANIFEST_BYTES.saturating_sub(self.valid_length),
            "Snapshot manifest exceeds 1 MiB"
        );
        let mut record = Vec::with_capacity(length as usize);
        record.extend_from_slice(&(body.len() as u32).to_le_bytes());
        record.extend_from_slice(&Sha256::digest(&body));
        record.extend_from_slice(&body);
        self.failed_append = true;
        let mut written = 0;
        while written < record.len() {
            let count = self.owner.write_at(
                self.valid_length + written as u64,
                &record[written..record.len().min(written + IO_CHUNK)],
                context,
            )?;
            ensure!(count > 0, "Snapshot manifest append made no progress");
            written += count;
        }
        self.owner.sync_all(context)?;
        platform::sync_directory(self.owner.directory.parent())?;
        self.valid_length += length;
        self.records += 1;
        if !checkpoint {
            self.phase = Some(phase);
        }
        self.failed_append = false;
        Ok(())
    }
    pub(crate) fn read_facts(&self, context: &SnapshotContext) -> Result<Vec<PhaseFact>> {
        self.owner.verify(context)?;
        let length = self.owner.len(context)?;
        ensure!(
            length <= MAX_MANIFEST_BYTES,
            "Snapshot manifest exceeds its physical byte bound"
        );
        let mut bytes = vec![0; length as usize];
        let mut offset = 0;
        while offset < bytes.len() {
            let wanted = (bytes.len() - offset).min(IO_CHUNK);
            let count =
                self.owner
                    .read_at(offset as u64, &mut bytes[offset..offset + wanted], context)?;
            ensure!(
                count > 0,
                "Snapshot manifest was truncated during inspection"
            );
            offset += count;
        }
        parse_phase_facts(&bytes)
    }
    /// Moves a failed manifest into a retained recovery ticket without closing it.
    pub(crate) fn retain_failure(self, error: anyhow::Error) -> NativeFailure {
        let mut failure = NativeFailure::new(error, None);
        failure.manifest = Some(Box::new(self));
        failure
    }
    pub(crate) fn cleanup(
        self,
        context: &SnapshotContext,
        record: &dyn Fn(&DurableCleanupFact) -> Result<()>,
    ) -> std::result::Result<(), NativeFailure> {
        let Self {
            owner,
            records,
            phase,
            valid_length,
            failed_append,
        } = self;
        match owner.cleanup_journaled(context, record) {
            Ok(_) => Ok(()),
            Err(mut failure) => {
                if let Some(owner) = failure.file.take() {
                    failure.manifest = Some(Box::new(Self {
                        owner: *owner,
                        records,
                        phase,
                        valid_length,
                        failed_append,
                    }));
                }
                Err(failure)
            }
        }
    }
}
pub(crate) fn parse_phase_facts(bytes: &[u8]) -> Result<Vec<PhaseFact>> {
    ensure!(
        bytes.len() as u64 <= MAX_MANIFEST_BYTES,
        "Snapshot manifest exceeds 1 MiB"
    );
    let mut facts = Vec::new();
    let mut offset = 0;
    let mut previous: Option<SnapshotPhase> = None;
    while offset < bytes.len() {
        ensure!(
            facts.len() < MAX_RECORDS,
            "Snapshot manifest exceeds 64 records"
        );
        if bytes.len() - offset < 36 {
            break;
        }
        let length = u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap()) as usize;
        ensure!(
            length > 0 && length <= MAX_RECORD_BODY,
            "Invalid snapshot manifest length envelope"
        );
        if bytes.len() - offset - 36 < length {
            break;
        }
        let body = &bytes[offset + 36..offset + 36 + length];
        require_unchanged(
            Sha256::digest(body).as_slice() == &bytes[offset + 4..offset + 36],
            "Snapshot manifest checksum changed",
        )?;
        let checkpoint = body[0] & 0x80 != 0;
        let phase = SnapshotPhase::decode(body[0] & 0x7f)?;
        ensure!(
            if checkpoint {
                previous == Some(phase)
            } else {
                phase as u8 == previous.map_or(0, |p| p as u8 + 1)
            },
            "Snapshot manifest phase sequence changed"
        );
        if !checkpoint {
            previous = Some(phase);
        }
        facts.push(PhaseFact {
            phase,
            checkpoint,
            body: body[1..].to_vec(),
        });
        offset += 36 + length;
    }
    Ok(facts)
}

#[derive(Clone, Debug)]
pub(crate) struct SourceFileMetadata {
    pub path: PathBuf,
    pub identity: FileIdentity,
    pub volume: String,
}
#[derive(Clone, Debug)]
pub(crate) struct SourceCloseFingerprint {
    pub identity: FileIdentity,
    stamp: Stamp,
    sha256: String,
}
impl SourceCloseFingerprint {
    pub(crate) fn same_body(&self, other: &Self) -> bool {
        self.identity == other.identity
            && self.stamp.length == other.stamp.length
            && self.sha256 == other.sha256
    }
    pub(crate) fn verify_file(&self, file: &File, check: &dyn Fn() -> Result<()>) -> Result<()> {
        platform::validate(file, false)?;
        require_unchanged(
            platform::identity(file)? == self.identity && platform::links(file)? == 1,
            "Closed profile main actual identity changed",
        )?;
        require_unchanged(
            platform::stamp(file)? == self.stamp,
            "Closed profile main body changed after actual close",
        )?;
        let actual = fingerprint_source_file(file, check)?;
        require_unchanged(
            self.same_body(&actual) && self.stamp == actual.stamp,
            "Closed profile main body changed after actual close",
        )
    }
}
fn fingerprint_source_file(
    file: &File,
    check: &dyn Fn() -> Result<()>,
) -> Result<SourceCloseFingerprint> {
    check()?;
    platform::validate(file, false)?;
    ensure!(
        platform::links(file)? == 1,
        "Closed profile main is aliased"
    );
    let identity = platform::identity(file)?;
    let stamp = platform::stamp(file)?;
    let mut hash = Sha256::new();
    let mut buffer = [0u8; IO_CHUNK];
    let mut at = 0;
    while at < stamp.length {
        check()?;
        let end = (stamp.length - at).min(IO_CHUNK as u64) as usize;
        let count = platform::read_at(file, &mut buffer[..end], at)?;
        ensure!(count > 0, "Closed profile main was truncated while sealing");
        hash.update(&buffer[..count]);
        at += count as u64;
    }
    require_unchanged(
        platform::identity(file)? == identity
            && platform::stamp(file)? == stamp
            && platform::links(file)? == 1,
        "Closed profile main changed while sealing",
    )?;
    check()?;
    Ok(SourceCloseFingerprint {
        identity,
        stamp,
        sha256: format!("{:x}", hash.finalize()),
    })
}
/// A read-only native descriptor plus continuous ancestors and leaf-name proof.
/// Source sizes may change while SQLite's native lock protocol allows writes.
pub(crate) struct SourceFileProof {
    directory: SnapshotDirectory,
    file: File,
    name: String,
    metadata: Option<SourceFileMetadata>,
    sqlite_anchor: bool,
    immutable_stamp: Option<Stamp>,
}
impl SourceFileProof {
    pub(crate) fn fingerprint_close(
        &self,
        check: &dyn Fn() -> Result<()>,
    ) -> Result<SourceCloseFingerprint> {
        self.verify()?;
        let fingerprint = fingerprint_source_file(&self.file, check)?;
        self.verify()?;
        Ok(fingerprint)
    }
    pub(crate) fn close(self) -> std::result::Result<(), NativeFailure> {
        let Self {
            directory,
            file,
            name,
            metadata,
            sqlite_anchor,
            immutable_stamp,
        } = self;
        let mut file = Some(file);
        #[cfg(windows)]
        let result = platform::close_retained(&mut file);
        #[cfg(not(windows))]
        let result: Result<()> = {
            drop(file.take());
            Ok(())
        };
        match result {
            Ok(()) => Ok(()),
            Err(error) => {
                let mut failure = NativeFailure::new(error, None);
                failure.source = Some(Box::new(Self {
                    directory,
                    file: file.unwrap(),
                    name,
                    metadata,
                    sqlite_anchor,
                    immutable_stamp,
                }));
                Err(failure)
            }
        }
    }
    #[cfg(all(test, windows))]
    pub(crate) fn close_handle_for_test(&self) -> std::os::windows::io::RawHandle {
        use std::os::windows::io::AsRawHandle;
        self.file.as_raw_handle()
    }
    pub(crate) fn pin(path: impl AsRef<Path>) -> std::result::Result<Self, NativeFailure> {
        Self::pin_mode(path.as_ref(), false)
    }
    #[cfg(windows)]
    pub(crate) fn pin_immutable(
        path: impl AsRef<Path>,
    ) -> std::result::Result<Self, NativeFailure> {
        Self::pin_mode(path.as_ref(), true)
    }
    fn pin_mode(path: &Path, immutable: bool) -> std::result::Result<Self, NativeFailure> {
        let preparation = (|| -> Result<(SnapshotDirectory, String, File)> {
            let parent = path
                .parent()
                .context("Snapshot source has no parent directory")?;
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .context("Snapshot source basename is not UTF-8")?
                .to_string();
            validate_basename(&name)?;
            let directory = SnapshotDirectory::open(parent, 1, DirectoryPurpose::ReadOnly)?;
            let file = if immutable {
                #[cfg(windows)]
                {
                    platform::open_immutable_source(directory.parent(), &name)?
                }
                #[cfg(not(windows))]
                {
                    return Err(unsupported(
                        "Immutable selected images require native share exclusion",
                    ));
                }
            } else {
                platform::open_source(directory.parent(), &name)?
            };
            Ok((directory, name, file))
        })();
        let (directory, name, file) = preparation.map_err(|e| NativeFailure::new(e, None))?;
        let mut proof = Self {
            directory,
            file,
            name,
            metadata: None,
            sqlite_anchor: false,
            immutable_stamp: None,
        };
        let inspected = (|| -> Result<()> {
            platform::validate(&proof.file, false)?;
            let identity = platform::identity(&proof.file)?;
            let volume = platform::volume(&proof.file)?;
            proof.metadata = Some(SourceFileMetadata {
                path: proof.directory.path().join(&proof.name),
                identity,
                volume,
            });
            if immutable {
                proof.immutable_stamp = Some(platform::stamp(&proof.file)?);
            }
            proof.verify()
        })();
        match inspected {
            Ok(()) => Ok(proof),
            Err(e) => {
                let mut failure = NativeFailure::new(e, None);
                failure.source = Some(Box::new(proof));
                Err(failure)
            }
        }
    }
    pub(crate) fn metadata(&self) -> Result<&SourceFileMetadata> {
        self.metadata
            .as_ref()
            .context("Snapshot source native proof is incomplete")
    }
    pub(crate) fn len(&self) -> Result<u64> {
        self.verify()?;
        let length = self.file.metadata()?.len();
        self.verify()?;
        Ok(length)
    }
    #[cfg(windows)]
    pub(crate) fn read_immutable_at(
        &self,
        bytes: &mut [u8],
        offset: u64,
        context: &SnapshotContext,
    ) -> Result<usize> {
        ensure!(
            bytes.len() <= IO_CHUNK,
            "Selected-image read exceeds 64 KiB"
        );
        context.check()?;
        self.verify()?;
        let stamp = self
            .immutable_stamp
            .as_ref()
            .context("Source lacks immutable read ownership")?;
        require_unchanged(
            platform::stamp(&self.file)? == *stamp,
            "Selected image changed",
        )?;
        let count = platform::read_at(&self.file, bytes, offset)?;
        require_unchanged(
            platform::stamp(&self.file)? == *stamp,
            "Selected image changed during read",
        )?;
        self.verify()?;
        context.check()?;
        Ok(count)
    }
    #[cfg(windows)]
    pub(crate) fn require_standalone_image(&self) -> Result<()> {
        self.verify()?;
        ensure!(
            self.immutable_stamp.is_some(),
            "Image has no immutable ownership"
        );
        for suffix in ["-wal", "-shm", "-journal"] {
            platform::require_absent(self.directory.parent(), &format!("{}{suffix}", self.name))?;
        }
        self.verify()
    }
    /// Sidecars retain a leaf proof beneath the SQLite-anchored actual parent.
    /// This does not authorize a main/WAL source; those need file-control 29.
    pub(crate) fn pin_checked_sidecar(
        main: &Self,
        basename: &str,
    ) -> std::result::Result<Self, NativeFailure> {
        let checked = (|| -> Result<()> {
            main.verify()?;
            ensure!(
                main.sqlite_anchor,
                "Snapshot sidecar needs a live SQLite-anchored source parent"
            );
            ensure!(
                basename == format!("{}-shm", main.name)
                    || basename == format!("{}-wal", main.name),
                "Snapshot sidecar name is outside its anchored database"
            );
            Ok(())
        })();
        checked.map_err(|e| NativeFailure::new(e, None))?;
        let proof = Self::pin(main.directory.path().join(basename))?;
        let checked = (|| -> Result<()> {
            main.verify()?;
            require_unchanged(
                proof.directory.identity() == main.directory.identity(),
                "Snapshot sidecar opened another actual source parent",
            )?;
            proof.verify()
        })();
        match checked {
            Ok(()) => Ok(proof),
            Err(e) => {
                let mut failure = NativeFailure::new(e, None);
                failure.source = Some(Box::new(proof));
                Err(failure)
            }
        }
    }
    pub(crate) fn verify(&self) -> Result<()> {
        self.directory.verify()?;
        platform::validate(&self.file, false)?;
        let metadata = self.metadata()?;
        require_unchanged(
            platform::identity(&self.file)? == metadata.identity
                && platform::volume(&self.file)? == metadata.volume
                && platform::links(&self.file)? == 1,
            "Snapshot source actual object or link count changed",
        )?;
        platform::verify_name(self.directory.parent(), &self.name, &metadata.identity)
    }
    /// `db` must be an open, exclusively actor-owned connection for this call.
    /// The public file-pointer opcode and native-handle opcode avoid any private
    /// SQLite winFile/unixFile layout or dependency on amalgamation offsets.
    pub(crate) unsafe fn from_sqlite_main(
        db: *mut rusqlite::ffi::sqlite3,
        path: &Path,
    ) -> std::result::Result<Self, NativeFailure> {
        let mut file = std::ptr::null_mut::<rusqlite::ffi::sqlite3_file>();
        let rc = unsafe {
            rusqlite::ffi::sqlite3_file_control(
                db,
                c"main".as_ptr(),
                rusqlite::ffi::SQLITE_FCNTL_FILE_POINTER,
                &mut file as *mut _ as *mut _,
            )
        };
        if rc != rusqlite::ffi::SQLITE_OK || file.is_null() {
            return Err(NativeFailure::new(
                unsupported("SQLite main native file-pointer contract is unavailable"),
                None,
            ));
        }
        unsafe { Self::from_sqlite_file(file, path) }
    }
    /// `file` must remain live in the default native VFS for this entire call.
    pub(crate) unsafe fn from_sqlite_file(
        file: *mut rusqlite::ffi::sqlite3_file,
        path: &Path,
    ) -> std::result::Result<Self, NativeFailure> {
        let mut proof = Self::pin(path)?;
        let checked = unsafe { proof.verify_sqlite_file(file) };
        match checked {
            Ok(()) => {
                proof.sqlite_anchor = true;
                Ok(proof)
            }
            Err(e) => {
                let mut failure = NativeFailure::new(e, None);
                failure.source = Some(Box::new(proof));
                Err(failure)
            }
        }
    }
    /// The caller compares every real default-VFS xOpen result before it can be
    /// installed into a source proxy, while the original native pin stays live.
    pub(crate) unsafe fn verify_sqlite_file(
        &self,
        file: *mut rusqlite::ffi::sqlite3_file,
    ) -> Result<()> {
        self.verify()?;
        #[cfg(windows)]
        {
            ensure!(!file.is_null(), "SQLite native source file is null");
            let methods = unsafe { (*file).pMethods };
            ensure!(
                !methods.is_null(),
                "SQLite native source methods are absent"
            );
            let control = unsafe { (*methods).xFileControl }
                .context("SQLite native source file-control is absent")?;
            let mut handle: windows_sys::Win32::Foundation::HANDLE = std::ptr::null_mut();
            let rc = unsafe {
                control(
                    file,
                    rusqlite::ffi::SQLITE_FCNTL_WIN32_GET_HANDLE,
                    &mut handle as *mut _ as *mut _,
                )
            };
            if rc != rusqlite::ffi::SQLITE_OK {
                return Err(unsupported(
                    "SQLite VFS does not expose its documented Windows handle",
                ));
            }
            unsafe { self.verify_handle(handle) }
        }
        #[cfg(not(windows))]
        {
            let _ = file;
            Err(unsupported(
                "SQLite default Unix VFS has no supported native descriptor file-control contract",
            ))
        }
    }
    #[cfg(windows)]
    pub(crate) unsafe fn verify_handle(
        &self,
        handle: windows_sys::Win32::Foundation::HANDLE,
    ) -> Result<()> {
        use std::os::windows::io::FromRawHandle;
        ensure!(
            !handle.is_null() && handle != windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE,
            "SQLite returned an invalid native source handle"
        );
        self.verify()?;
        // The wrapper borrows SQLite's live HANDLE and must never close it.
        let borrowed = std::mem::ManuallyDrop::new(unsafe { File::from_raw_handle(handle) });
        platform::validate(&borrowed, false)?;
        let expected = self.metadata()?;
        require_unchanged(
            platform::identity(&borrowed)? == expected.identity
                && platform::volume(&borrowed)? == expected.volume
                && platform::links(&borrowed)? == 1,
            "Default SQLite VFS opened another native source object",
        )?;
        require_unchanged(
            crate::domain::music::normalize_local_path(&platform::current_path(&borrowed)?)
                == crate::domain::music::normalize_local_path(&expected.path),
            "SQLite native source handle has a different actual namespace",
        )?;
        self.verify()
    }
}

#[cfg(windows)]
mod platform {
    use super::*;
    use std::os::windows::{
        ffi::OsStrExt,
        io::{AsRawHandle, FromRawHandle, IntoRawHandle},
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
    const STATUS_DELETE_PENDING: i32 = 0xc0000056u32 as i32;
    const STATUS_INVALID_PARAMETER: i32 = 0xc000000du32 as i32;

    #[derive(Default)]
    pub(super) struct CleanupState {
        probe: Option<CalibrationProbe>,
        calibration: Option<CleanupCalibration>,
        prepared: Option<CleanupPrepared>,
        disposition_requested: bool,
        zero_link_witness: bool,
    }
    impl CleanupState {
        pub(super) fn disposition_started(&self) -> bool {
            self.disposition_requested
        }
        pub(super) fn probe_metadata(&self) -> Option<CalibrationMetadata> {
            self.probe.as_ref().map(|probe| CalibrationMetadata {
                temporary_name: probe.name.clone(),
                identity: probe.identity.clone(),
                owner_handle_open: probe.file.is_some(),
                reader_handle_open: probe.reader.is_some(),
                disposition_requested: probe.disposition_requested,
                zero_link_witness: probe.zero_link_witness,
            })
        }
    }
    struct CalibrationProbe {
        file: Option<File>,
        reader: Option<File>,
        name: String,
        identity: Option<FileIdentity>,
        positive_control: bool,
        disposition_requested: bool,
        zero_link_witness: bool,
    }
    struct CleanupCalibration {
        volume: String,
        directory: FileIdentity,
    }
    struct CleanupPrepared {
        identity: FileIdentity,
        stamp: Stamp,
        sha256: String,
    }
    fn normalize(path: &Path) -> PathBuf {
        crate::domain::music::normalize_local_path(path)
    }
    pub(super) fn pin_directory(path: &Path) -> Result<(PathBuf, Vec<File>)> {
        let mut components = path.components();
        let prefix = components
            .next()
            .context("Snapshot volume prefix missing")?;
        ensure!(
            matches!(prefix, Component::Prefix(_))
                && matches!(components.next(), Some(Component::RootDir)),
            "Snapshot must use an absolute native path"
        );
        let mut current = PathBuf::from(prefix.as_os_str());
        current.push("\\");
        let name = current
            .as_os_str()
            .encode_wide()
            .chain(Some(0))
            .collect::<Vec<_>>();
        let handle = unsafe {
            CreateFileW(
                name.as_ptr(),
                FILE_READ_ATTRIBUTES | FILE_LIST_DIRECTORY,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                std::ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
                std::ptr::null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error().into());
        }
        let mut pins = vec![unsafe { File::from_raw_handle(handle) }];
        validate(&pins[0], true)?;
        for component in components {
            let Component::Normal(name) = component else {
                bail!("Invalid selected snapshot component");
            };
            pins.push(open_relative(
                pins.last().unwrap(),
                name.to_str().context("Snapshot path is not UTF-8")?,
                true,
                false,
                false,
            )?);
        }
        let actual = current_path(pins.last().unwrap())?;
        ensure!(
            normalize(&actual) == normalize(path),
            "Selected snapshot namespace differs from its path"
        );
        Ok((normalize(&actual), pins))
    }
    fn open_relative(
        parent: &File,
        name: &str,
        directory: bool,
        create: bool,
        readonly: bool,
    ) -> Result<File> {
        open_relative_with_sharing(parent, name, directory, create, readonly, None)
    }
    pub(super) fn open_group_file(parent: &File, name: &str, create: bool) -> Result<File> {
        open_relative_retained(parent, name, false, create, false, None, false)
    }
    pub(super) fn close_retained(file: &mut Option<File>) -> Result<()> {
        if let Some(owner) = file.take() {
            let raw = owner.into_raw_handle();
            if unsafe { windows_sys::Win32::Foundation::CloseHandle(raw) } == 0 {
                let error = io::Error::last_os_error();
                *file = Some(unsafe { File::from_raw_handle(raw) });
                return Err(error.into());
            }
        }
        Ok(())
    }
    fn open_relative_with_sharing(
        parent: &File,
        name: &str,
        directory: bool,
        create: bool,
        readonly: bool,
        share_override: Option<u32>,
    ) -> Result<File> {
        open_relative_retained(
            parent,
            name,
            directory,
            create,
            readonly,
            share_override,
            true,
        )
    }
    fn open_relative_retained(
        parent: &File,
        name: &str,
        directory: bool,
        create: bool,
        readonly: bool,
        share_override: Option<u32>,
        validate_open: bool,
    ) -> Result<File> {
        let mut text = name.encode_utf16().collect::<Vec<_>>();
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
        let access = if directory {
            FILE_READ_ATTRIBUTES | FILE_LIST_DIRECTORY
        } else if readonly {
            FILE_READ_ATTRIBUTES
        } else {
            GENERIC_READ | GENERIC_WRITE | DELETE
        };
        let sharing = share_override.unwrap_or(if directory {
            FILE_SHARE_READ | FILE_SHARE_WRITE
        } else if readonly {
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE
        } else {
            FILE_SHARE_READ
        });
        let status = unsafe {
            NtCreateFile(
                &mut handle,
                access | SYNCHRONIZE,
                &attributes,
                &mut status_block,
                std::ptr::null(),
                0,
                sharing,
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
            "Invalid snapshot handle"
        );
        let file = unsafe { File::from_raw_handle(handle) };
        // A newly created handle must reach its retained owner before any
        // fallible inspection; otherwise an inspection failure strands a leaf.
        if !create && validate_open {
            validate(&file, directory)?;
        }
        Ok(file)
    }
    pub(super) fn create_private(parent: &File, name: Option<&str>) -> Result<File> {
        open_relative(
            parent,
            name.context("Private snapshot name is missing")?,
            false,
            true,
            false,
        )
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
            "Cannot inspect snapshot object"
        );
        ensure!(
            info.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT == 0
                && (info.FileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0) == directory,
            "Snapshot object type or reparse traversal refused"
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
                } != 0
                    && case.Flags & 1 == 0,
                "UnsupportedCapability: snapshot directory case policy cannot be supported"
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
            "Cannot identify snapshot object"
        );
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut object = String::with_capacity(32);
        for byte in info.FileId.Identifier {
            object.push(HEX[(byte >> 4) as usize] as char);
            object.push(HEX[(byte & 15) as usize] as char);
        }
        Ok(FileIdentity {
            device: info.VolumeSerialNumber,
            object,
        })
    }
    pub(super) fn stamp(file: &File) -> Result<Stamp> {
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
            "Cannot inspect snapshot change stamp"
        );
        Ok(Stamp {
            changed: info.ChangeTime as i128,
            written: info.LastWriteTime as i128,
            length: file.metadata()?.len(),
        })
    }
    pub(super) fn links(file: &File) -> Result<u64> {
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
            "Cannot inspect snapshot link count"
        );
        Ok(u64::from(info.NumberOfLinks))
    }
    pub(super) fn current_path(file: &File) -> Result<PathBuf> {
        let mut text = [0u16; 32768];
        let length = unsafe {
            GetFinalPathNameByHandleW(
                file.as_raw_handle(),
                text.as_mut_ptr(),
                text.len() as u32,
                FILE_NAME_NORMALIZED | VOLUME_NAME_DOS,
            )
        };
        ensure!(
            length > 0 && (length as usize) < text.len(),
            "Cannot inspect bounded snapshot namespace"
        );
        Ok(PathBuf::from(String::from_utf16(&text[..length as usize])?))
    }
    pub(super) fn verify_directory(path: &Path, pins: &[File]) -> Result<()> {
        ensure!(
            normalize(&current_path(
                pins.last().context("Snapshot root handle missing")?
            )?) == normalize(path),
            "Selected snapshot directory moved"
        );
        Ok(())
    }
    pub(super) fn verify_name(parent: &File, name: &str, expected: &FileIdentity) -> Result<()> {
        let proof = open_relative(parent, name, false, false, true)?;
        ensure!(
            identity(&proof)? == *expected,
            "Snapshot leaf now belongs to another object"
        );
        Ok(())
    }
    pub(super) fn publish(
        file: &File,
        parent: &File,
        name: &str,
        expected: &FileIdentity,
    ) -> Result<()> {
        ensure!(
            identity(file)? == *expected,
            "Snapshot object changed before publication"
        );
        let text = name.encode_utf16().collect::<Vec<_>>();
        let offset = std::mem::offset_of!(FILE_RENAME_INFO, FileName);
        let length = offset + text.len() * 2;
        let mut storage = vec![0u64; length.div_ceil(8)];
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
                length as u32,
                FileRenameInformation,
            )
        };
        if status < 0 {
            return Err(io::Error::from_raw_os_error(
                unsafe { RtlNtStatusToDosError(status) } as i32
            )
            .into());
        }
        Ok(())
    }
    pub(super) fn sync_directory(directory: &File) -> Result<()> {
        // Win32 exposes FlushFileBuffers for the owned writable file, used by
        // every caller before this namespace check. A read-only directory pin
        // is a namespace witness, not a substitute flush handle.
        validate(directory, true)?;
        cleanup_volume(directory)?;
        Ok(())
    }
    pub(super) fn delete_private(file: &File) -> Result<()> {
        let info = FILE_DISPOSITION_INFO { DeleteFile: true };
        if unsafe {
            SetFileInformationByHandle(
                file.as_raw_handle(),
                FileDispositionInfo,
                &info as *const _ as *const _,
                std::mem::size_of_val(&info) as u32,
            )
        } == 0
        {
            return Err(io::Error::last_os_error().into());
        }
        Ok(())
    }

    fn cleanup_scope(directory: &SnapshotDirectory, context: &SnapshotContext) -> Result<()> {
        context.check()?;
        directory.verify()?;
        Ok(())
    }
    fn cleanup_volume(file: &File) -> Result<String> {
        let mut filesystem = [0u16; 32];
        let (mut serial, mut maximum, mut flags) = (0, 0, 0);
        ensure!(
            unsafe {
                GetVolumeInformationByHandleW(
                    file.as_raw_handle(),
                    std::ptr::null_mut(),
                    0,
                    &mut serial,
                    &mut maximum,
                    &mut flags,
                    filesystem.as_mut_ptr(),
                    filesystem.len() as u32,
                )
            } != 0,
            "UnsupportedCapability: snapshot cleanup filesystem cannot be inspected"
        );
        let length = filesystem
            .iter()
            .position(|unit| *unit == 0)
            .context("Cleanup filesystem name is not bounded")?;
        ensure!(
            String::from_utf16(&filesystem[..length])? == "NTFS",
            "UnsupportedCapability: snapshot cleanup calibration requires NTFS"
        );
        let mut path = [0u16; 32768];
        let length = unsafe {
            GetFinalPathNameByHandleW(
                file.as_raw_handle(),
                path.as_mut_ptr(),
                path.len() as u32,
                FILE_NAME_NORMALIZED | VOLUME_NAME_GUID,
            )
        } as usize;
        ensure!(
            length > 0 && length < path.len(),
            "UnsupportedCapability: snapshot cleanup volume GUID is unavailable"
        );
        let path = String::from_utf16(&path[..length])?;
        ensure!(
            path.starts_with("\\\\?\\Volume{"),
            "UnsupportedCapability: snapshot cleanup volume GUID is not native"
        );
        let end = path
            .find("}\\")
            .context("Cleanup volume GUID is malformed")?
            + 2;
        Ok(path[..end].to_ascii_lowercase())
    }
    fn cleanup_identifier(expected: &FileIdentity) -> Result<[u8; 16]> {
        ensure!(
            expected.object.len() == 32
                && expected.object.bytes().all(|byte| byte.is_ascii_hexdigit()),
            "Cleanup requires the original actual 128-bit file identity"
        );
        let mut identifier = [0u8; 16];
        for (index, byte) in identifier.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&expected.object[index * 2..index * 2 + 2], 16)?;
        }
        ensure!(
            identifier[8..].iter().all(|byte| *byte == 0),
            "UnsupportedCapability: snapshot cleanup ID layout is not calibrated"
        );
        Ok(identifier)
    }
    fn cleanup_lookup(
        parent: &File,
        identifier: [u8; 16],
        length: u16,
    ) -> std::result::Result<File, i32> {
        let mut buffer = [0u16; 8];
        for (index, unit) in buffer.iter_mut().enumerate() {
            *unit = u16::from_le_bytes([identifier[index * 2], identifier[index * 2 + 1]]);
        }
        let name = UNICODE_STRING {
            Length: length,
            MaximumLength: length,
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
    fn cleanup_present(
        directory: &SnapshotDirectory,
        expected: &FileIdentity,
        is_directory: bool,
        volume: &str,
        context: &SnapshotContext,
    ) -> Result<()> {
        let identifier = cleanup_identifier(expected)?;
        for length in [8, 16] {
            cleanup_scope(directory, context)?;
            let file = cleanup_lookup(directory.parent(), identifier, length)
                .map_err(|status| {
                    io::Error::from_raw_os_error(unsafe { RtlNtStatusToDosError(status) } as i32)
                })
                .context(
                    "UnsupportedCapability: native snapshot cleanup positive control failed",
                )?;
            validate(&file, is_directory)?;
            require_unchanged(
                identity(&file)? == *expected && cleanup_volume(&file)? == volume,
                "Cleanup native positive control differs from the actual full file identity or volume",
            )?;
            cleanup_scope(directory, context)?;
        }
        Ok(())
    }
    fn cleanup_status(
        directory: &SnapshotDirectory,
        expected: &FileIdentity,
        status: i32,
        context: &SnapshotContext,
    ) -> Result<()> {
        let identifier = cleanup_identifier(expected)?;
        for length in [8, 16] {
            cleanup_scope(directory, context)?;
            match cleanup_lookup(directory.parent(), identifier, length) {
                Err(actual) if actual == status => {}
                Err(actual) => bail!(
                    "UnsupportedCapability: native snapshot cleanup signature differs: {actual:#010x}"
                ),
                Ok(file) => {
                    require_unchanged(
                        identity(&file)? == *expected,
                        "Cleanup native lookup returned an unexpected identity",
                    )?;
                    bail!(
                        "Original snapshot file identity still exists, including aliases or renamed objects"
                    );
                }
            }
            cleanup_scope(directory, context)?;
        }
        Ok(())
    }
    fn disposition_state(file: &File) -> Result<(bool, u32)> {
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
            "Cannot inspect snapshot cleanup disposition"
        );
        Ok((info.DeletePending, info.NumberOfLinks))
    }
    fn calibrate_cleanup(
        state: &mut CleanupState,
        directory: &SnapshotDirectory,
        context: &SnapshotContext,
    ) -> Result<()> {
        cleanup_scope(directory, context)?;
        let volume = cleanup_volume(directory.parent())?;
        let root = identity(directory.parent())?;
        cleanup_present(directory, &root, true, &volume, context)?;
        if let Some(calibration) = &state.calibration {
            require_unchanged(
                calibration.volume == volume && calibration.directory == root,
                "Cleanup calibration belongs to a different actual selected volume",
            )?;
            return Ok(());
        }
        if state.probe.is_none() {
            let mut nonce = [0u8; 16];
            getrandom::fill(&mut nonce).map_err(|error| {
                anyhow::anyhow!("Cannot allocate cleanup calibration nonce: {error}")
            })?;
            let nonce: String = nonce.iter().map(|byte| format!("{byte:02x}")).collect();
            let name = format!(".lx-snapshot-calibration-{nonce}.part");
            let file =
                open_relative_with_sharing(directory.parent(), &name, false, true, false, Some(0))?;
            // Retain the exclusive probe before its first fallible inspection.
            state.probe = Some(CalibrationProbe {
                file: Some(file),
                reader: None,
                name,
                identity: None,
                positive_control: false,
                disposition_requested: false,
                zero_link_witness: false,
            });
        }
        let probe = state.probe.as_mut().unwrap();
        if !probe.disposition_requested {
            let file = probe
                .file
                .as_ref()
                .context("Cleanup calibration probe handle is missing")?;
            validate(file, false)?;
            if probe.identity.is_none() {
                probe.identity = Some(identity(file)?);
            }
            let expected = probe.identity.as_ref().unwrap();
            require_unchanged(
                identity(file)? == *expected
                    && expected.device == root.device
                    && file.metadata()?.len() == 0
                    && links(file)? == 1,
                "Cleanup calibration probe is not its original zero-byte single-link object",
            )?;
            verify_name(directory.parent(), &probe.name, expected)?;
            cleanup_present(directory, expected, false, &volume, context)?;
            if probe.reader.is_none() {
                let reader =
                    cleanup_lookup(directory.parent(), cleanup_identifier(expected)?, 16).map_err(
                        |status| {
                            io::Error::from_raw_os_error(
                                unsafe { RtlNtStatusToDosError(status) } as i32
                            )
                        },
                    )?;
                require_unchanged(
                    identity(&reader)? == *expected && cleanup_volume(&reader)? == volume,
                    "Cleanup calibration reader does not identify the original probe",
                )?;
                probe.reader = Some(reader);
            }
            file.sync_all()?;
            probe.positive_control = true;
            cleanup_scope(directory, context)?;
            delete_private(file)?;
            probe.disposition_requested = true;
        }
        let expected = probe
            .identity
            .as_ref()
            .context("Cleanup calibration probe identity is missing")?;
        if let Some(file) = &probe.file {
            let (pending, count) = disposition_state(file)?;
            require_unchanged(
                probe.positive_control && pending && count == 0 && identity(file)? == *expected,
                "Cleanup calibration has no same-handle pending zero-link witness",
            )?;
            cleanup_status(directory, expected, STATUS_DELETE_PENDING, context)?;
            probe.zero_link_witness = true;
            close_retained(&mut probe.file)?;
        }
        ensure!(
            probe.zero_link_witness,
            "Cleanup calibration closed without its physical witness"
        );
        if let Some(reader) = &probe.reader {
            let (pending, count) = disposition_state(reader)?;
            require_unchanged(
                pending && count == 0 && identity(reader)? == *expected,
                "Cleanup delayed-reader control has no same-object pending zero-link witness",
            )?;
            // Closing the original owner is not sufficient while another real
            // handle still exists. Calibrate that distinct state explicitly.
            cleanup_status(directory, expected, STATUS_DELETE_PENDING, context)?;
            close_retained(&mut probe.reader)?;
        }
        cleanup_status(directory, expected, STATUS_INVALID_PARAMETER, context)?;
        cleanup_scope(directory, context)?;
        require_unchanged(
            cleanup_volume(directory.parent())? == volume && identity(directory.parent())? == root,
            "Actual snapshot cleanup volume changed during calibration",
        )?;
        state.calibration = Some(CleanupCalibration {
            volume,
            directory: root,
        });
        state.probe = None;
        Ok(())
    }
    pub(super) fn prepare_cleanup(
        state: &mut CleanupState,
        file: &File,
        expected: &FileIdentity,
        directory: &SnapshotDirectory,
        context: &SnapshotContext,
        captured: &Stamp,
        sha256: &str,
    ) -> Result<()> {
        cleanup_scope(directory, context)?;
        calibrate_cleanup(state, directory, context)?;
        if state.disposition_requested {
            return Ok(());
        }
        let calibration = state.calibration.as_ref().unwrap();
        require_unchanged(
            identity(file)? == *expected && stamp(file)? == *captured && links(file)? == 1,
            "Cleanup original changed after actual whole-body verification",
        )?;
        ensure!(
            sha256.len() == 64 && sha256.bytes().all(|byte| byte.is_ascii_hexdigit()),
            "Cleanup whole digest is missing"
        );
        cleanup_present(directory, expected, false, &calibration.volume, context)?;
        file.sync_all()?;
        require_unchanged(
            stamp(file)? == *captured,
            "Cleanup original changed during native preparation",
        )?;
        state.prepared = Some(CleanupPrepared {
            identity: expected.clone(),
            stamp: captured.clone(),
            sha256: sha256.into(),
        });
        cleanup_scope(directory, context)
    }
    pub(super) fn request_cleanup_disposition(
        state: &mut CleanupState,
        file: &File,
        expected: &FileIdentity,
        directory: &SnapshotDirectory,
        context: &SnapshotContext,
    ) -> Result<()> {
        cleanup_scope(directory, context)?;
        let prepared = state
            .prepared
            .as_ref()
            .context("Cleanup original has no native-valid preparation")?;
        require_unchanged(
            prepared.identity == *expected && identity(file)? == *expected,
            "Cleanup disposition does not identify the actual prepared original",
        )?;
        if !state.disposition_requested {
            require_unchanged(
                stamp(file)? == prepared.stamp && links(file)? == 1,
                "Cleanup original changed before native disposition",
            )?;
            delete_private(file)?;
            state.disposition_requested = true;
        }
        let (pending, count) = disposition_state(file)?;
        require_unchanged(
            pending && count == 0 && identity(file)? == *expected,
            "Cleanup original has no same-handle pending zero-link witness",
        )?;
        cleanup_status(directory, expected, STATUS_DELETE_PENDING, context)?;
        state.zero_link_witness = true;
        cleanup_scope(directory, context)
    }
    pub(super) fn confirm_cleanup(
        state: &CleanupState,
        expected: &FileIdentity,
        directory: &SnapshotDirectory,
        context: &SnapshotContext,
    ) -> Result<()> {
        cleanup_scope(directory, context)?;
        let prepared = state
            .prepared
            .as_ref()
            .context("Closed cleanup original has no native preparation")?;
        let calibration = state
            .calibration
            .as_ref()
            .context("Cleanup original has no native calibration")?;
        ensure!(
            state.disposition_requested
                && state.zero_link_witness
                && prepared.identity == *expected
                && prepared.sha256.len() == 64,
            "Cleanup original has no matching native disposition and body witness"
        );
        require_unchanged(
            cleanup_volume(directory.parent())? == calibration.volume
                && identity(directory.parent())? == calibration.directory,
            "Cleanup original is on a different actual selected volume",
        )?;
        cleanup_present(
            directory,
            &calibration.directory,
            true,
            &calibration.volume,
            context,
        )?;
        // Invalid-parameter is accepted only for this previously native-valid
        // exact ID and this fresh opaque pending/close calibration, never as a
        // generic OpenFileById failure or a private-name absence shortcut.
        cleanup_status(directory, expected, STATUS_INVALID_PARAMETER, context)?;
        require_unchanged(
            cleanup_volume(directory.parent())? == calibration.volume
                && identity(directory.parent())? == calibration.directory,
            "Cleanup selected volume changed during the exact absent-ID lookup",
        )?;
        cleanup_scope(directory, context)
    }
    pub(super) fn confirm_cold_cleanup(
        state: &mut CleanupState,
        directory: &SnapshotDirectory,
        fact: &DurableCleanupFact,
        context: &SnapshotContext,
    ) -> Result<()> {
        let expected = fact
            .file
            .identity
            .as_ref()
            .context("Cold cleanup original FID missing")?;
        calibrate_cleanup(state, directory, context)?;
        state.prepared = Some(CleanupPrepared {
            identity: expected.clone(),
            stamp: fact.stamp.clone(),
            sha256: fact.sha256.clone(),
        });
        state.disposition_requested = true;
        state.zero_link_witness = true;
        confirm_cleanup(state, expected, directory, context)
    }

    pub(super) fn read_at(file: &File, bytes: &mut [u8], offset: u64) -> Result<usize> {
        use std::os::windows::fs::FileExt;
        Ok(file.seek_read(bytes, offset)?)
    }
    pub(super) fn write_at(file: &File, bytes: &[u8], offset: u64) -> Result<usize> {
        use std::os::windows::fs::FileExt;
        Ok(file.seek_write(bytes, offset)?)
    }
    pub(super) fn open_source(parent: &File, name: &str) -> Result<File> {
        open_source_with_sharing(parent, name, FILE_SHARE_READ | FILE_SHARE_WRITE)
    }
    pub(super) fn open_immutable_source(parent: &File, name: &str) -> Result<File> {
        open_source_with_sharing(parent, name, FILE_SHARE_READ)
    }
    pub(super) fn require_absent(parent: &File, name: &str) -> Result<()> {
        match open_source(parent, name) {
            Ok(file) => {
                drop(file);
                bail!("Selected standalone image has a sidecar");
            }
            Err(error)
                if error.chain().any(|e| {
                    e.downcast_ref::<io::Error>()
                        .is_some_and(|e| e.raw_os_error() == Some(2))
                }) =>
            {
                Ok(())
            }
            Err(error) => Err(error).context("Cannot verify selected image sidecar absence"),
        }
    }
    fn open_source_with_sharing(parent: &File, name: &str, sharing: u32) -> Result<File> {
        // Pinned ancestors deny delete sharing before this absolute open. The
        // leaf is opened without traversal and retained before inspection.
        let path = current_path(parent)?.join(name);
        let text = path
            .as_os_str()
            .encode_wide()
            .chain(Some(0))
            .collect::<Vec<_>>();
        let handle = unsafe {
            CreateFileW(
                text.as_ptr(),
                GENERIC_READ,
                sharing,
                std::ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_OPEN_REPARSE_POINT,
                std::ptr::null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error().into());
        }
        Ok(unsafe { File::from_raw_handle(handle) })
    }
    pub(super) fn volume(file: &File) -> Result<String> {
        let mut text = [0u16; 32768];
        let length = unsafe {
            GetFinalPathNameByHandleW(
                file.as_raw_handle(),
                text.as_mut_ptr(),
                text.len() as u32,
                FILE_NAME_NORMALIZED | VOLUME_NAME_GUID,
            )
        } as usize;
        ensure!(
            length > 0 && length < text.len(),
            "UnsupportedCapability: native snapshot volume GUID is unavailable"
        );
        let path = String::from_utf16(&text[..length])?;
        ensure!(
            path.starts_with("\\\\?\\Volume{"),
            "UnsupportedCapability: snapshot volume has no native GUID"
        );
        let end = path
            .find("}\\")
            .context("Snapshot volume GUID is malformed")?
            + 2;
        Ok(path[..end].to_ascii_lowercase())
    }
    pub(super) fn create_private_directory(parent: &File, name: &str) -> Result<File> {
        open_relative(parent, name, true, true, false)
    }
    pub(super) fn ensure_private_directory(parent: &File, name: &str) -> Result<()> {
        let directory = match open_relative(parent, name, true, true, false) {
            Ok(file) => file,
            Err(error)
                if error.chain().any(|e| {
                    e.downcast_ref::<io::Error>()
                        .is_some_and(|e| e.kind() == io::ErrorKind::AlreadyExists)
                }) =>
            {
                open_relative(parent, name, true, false, false)?
            }
            Err(error) => return Err(error),
        };
        validate(&directory, true)
    }
    pub(super) fn validate_private_directory(directory: &File) -> Result<()> {
        validate(directory, true)?;
        // The exact-FID cleanup protocol is calibrated for native NTFS only.
        cleanup_volume(directory)?;
        Ok(())
    }
    pub(super) fn create_manifest(parent: &File, name: &str) -> Result<File> {
        open_relative(parent, name, false, true, false)
    }
    pub(super) fn open_recovery(parent: &File, name: &str) -> Result<File> {
        open_relative(parent, name, false, false, false)
    }
}
#[cfg(target_os = "linux")]
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
    pub(super) fn pin_directory(path: &Path) -> Result<(PathBuf, Vec<File>)> {
        let fd = unsafe {
            libc::open(
                c"/".as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error().into());
        }
        let mut pins = vec![unsafe { File::from_raw_fd(fd) }];
        for component in path.components() {
            if let Component::Normal(name) = component {
                let name = CString::new(name.as_bytes())?;
                let fd = unsafe {
                    libc::openat(
                        pins.last().unwrap().as_raw_fd(),
                        name.as_ptr(),
                        libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                    )
                };
                if fd < 0 {
                    return Err(io::Error::last_os_error().into());
                }
                let file = unsafe { File::from_raw_fd(fd) };
                validate(&file, true)?;
                pins.push(file);
            }
        }
        Ok((path.to_path_buf(), pins))
    }
    pub(super) fn validate(file: &File, directory: bool) -> Result<()> {
        let metadata = file.metadata()?;
        ensure!(
            if directory {
                metadata.is_dir()
            } else {
                metadata.is_file()
            },
            "Snapshot object type refused"
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
    pub(super) fn stamp(file: &File) -> Result<Stamp> {
        let metadata = file.metadata()?;
        Ok(Stamp {
            changed: metadata.ctime() as i128 * 1_000_000_000 + metadata.ctime_nsec() as i128,
            written: metadata.mtime() as i128 * 1_000_000_000 + metadata.mtime_nsec() as i128,
            length: metadata.len(),
        })
    }
    pub(super) fn links(file: &File) -> Result<u64> {
        Ok(file.metadata()?.nlink())
    }
    pub(super) fn verify_directory(path: &Path, pins: &[File]) -> Result<()> {
        let mut parent = &pins[0];
        let mut index = 1;
        for component in path.components() {
            if let Component::Normal(name) = component {
                let name = CString::new(name.as_bytes())?;
                let fd = unsafe {
                    libc::openat(
                        parent.as_raw_fd(),
                        name.as_ptr(),
                        libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                    )
                };
                if fd < 0 {
                    return Err(io::Error::last_os_error().into());
                }
                let observed = unsafe { File::from_raw_fd(fd) };
                ensure!(
                    identity(&observed)? == identity(&pins[index])?,
                    "Selected snapshot ancestor namespace changed"
                );
                parent = &pins[index];
                index += 1;
            }
        }
        Ok(())
    }
    pub(super) fn create_private(parent: &File, _name: Option<&str>) -> Result<File> {
        let fd = unsafe {
            libc::openat(
                parent.as_raw_fd(),
                c".".as_ptr(),
                libc::O_TMPFILE | libc::O_RDWR | libc::O_CLOEXEC,
                0o600,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error()).context(
                "UnsupportedCapability: filesystem cannot create an anonymous snapshot inode",
            );
        }
        Ok(unsafe { File::from_raw_fd(fd) })
    }
    pub(super) fn verify_name(parent: &File, name: &str, expected: &FileIdentity) -> Result<()> {
        let name = CString::new(name)?;
        let fd = unsafe {
            libc::openat(
                parent.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error().into());
        }
        let file = unsafe { File::from_raw_fd(fd) };
        validate(&file, false)?;
        ensure!(
            identity(&file)? == *expected,
            "Published snapshot leaf belongs to another object"
        );
        Ok(())
    }
    pub(super) fn publish(
        file: &File,
        parent: &File,
        name: &str,
        expected: &FileIdentity,
    ) -> Result<()> {
        ensure!(
            identity(file)? == *expected && links(file)? == 0,
            "Anonymous snapshot inode changed"
        );
        let name = CString::new(name)?;
        if unsafe {
            libc::linkat(
                file.as_raw_fd(),
                c"".as_ptr(),
                parent.as_raw_fd(),
                name.as_ptr(),
                libc::AT_EMPTY_PATH,
            )
        } != 0
        {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::AlreadyExists {
                return Err(error.into());
            }
            if !matches!(error.raw_os_error(), Some(libc::EPERM) | Some(libc::ENOENT)) {
                return Err(error.into());
            }
            let descriptor = CString::new(format!("/proc/self/fd/{}", file.as_raw_fd()))?;
            if unsafe {
                libc::linkat(
                    libc::AT_FDCWD,
                    descriptor.as_ptr(),
                    parent.as_raw_fd(),
                    name.as_ptr(),
                    libc::AT_SYMLINK_FOLLOW,
                )
            } != 0
            {
                return Err(io::Error::last_os_error())
                    .context("UnsupportedCapability: actual-FD no-replace snapshot link failed");
            }
        }
        Ok(())
    }
    pub(super) fn sync_directory(directory: &File) -> Result<()> {
        directory.sync_all()?;
        Ok(())
    }
    pub(super) fn delete_private(file: &File) -> Result<()> {
        if links(file)? != 0 {
            return Err(unsupported(
                "Named Linux snapshot has no verified exact-FD unlink primitive",
            ));
        }
        Ok(())
    }
    pub(super) fn verify_absent(
        _parent: &File,
        _name: &str,
        _expected: &FileIdentity,
    ) -> Result<()> {
        bail!("UnsupportedCapability: Linux anonymous snapshots have no private leaf")
    }

    pub(super) fn read_at(file: &File, bytes: &mut [u8], offset: u64) -> Result<usize> {
        use std::os::unix::fs::FileExt;
        Ok(file.read_at(bytes, offset)?)
    }
    pub(super) fn write_at(file: &File, bytes: &[u8], offset: u64) -> Result<usize> {
        use std::os::unix::fs::FileExt;
        Ok(file.write_at(bytes, offset)?)
    }
    pub(super) fn open_source(parent: &File, name: &str) -> Result<File> {
        let name = CString::new(name)?;
        let descriptor = unsafe {
            libc::openat(
                parent.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if descriptor < 0 {
            return Err(io::Error::last_os_error().into());
        }
        Ok(unsafe { File::from_raw_fd(descriptor) })
    }
    pub(super) fn volume(file: &File) -> Result<String> {
        Ok(format!("device:{}", file.metadata()?.dev()))
    }
    pub(super) fn ensure_private_directory(parent: &File, name: &str) -> Result<()> {
        let name = CString::new(name)?;
        if unsafe { libc::mkdirat(parent.as_raw_fd(), name.as_ptr(), 0o700) } != 0 {
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::AlreadyExists {
                return Err(error.into());
            }
        }
        let descriptor = unsafe {
            libc::openat(
                parent.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if descriptor < 0 {
            return Err(io::Error::last_os_error().into());
        }
        let directory = unsafe { File::from_raw_fd(descriptor) };
        validate_private_directory(&directory)?;
        parent.sync_all()?;
        Ok(())
    }
    pub(super) fn validate_private_directory(directory: &File) -> Result<()> {
        validate(directory, true)?;
        let metadata = directory.metadata()?;
        ensure!(
            metadata.uid() == unsafe { libc::geteuid() } && metadata.mode() & 0o077 == 0,
            "Private snapshot staging directory must be owned by the current user with mode 0700"
        );
        Ok(())
    }
    pub(super) fn create_manifest(parent: &File, name: &str) -> Result<File> {
        let name = CString::new(name)?;
        let descriptor = unsafe {
            libc::openat(
                parent.as_raw_fd(),
                name.as_ptr(),
                libc::O_CREAT | libc::O_EXCL | libc::O_RDWR | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0o600,
            )
        };
        if descriptor < 0 {
            return Err(io::Error::last_os_error().into());
        }
        // Retain the newly created named journal before fallible inspection.
        Ok(unsafe { File::from_raw_fd(descriptor) })
    }
    pub(super) fn open_recovery(_parent: &File, _name: &str) -> Result<File> {
        bail!("UnsupportedCapability: Linux cold snapshot recovery is not enabled")
    }
}
#[cfg(not(any(windows, target_os = "linux")))]
mod platform {
    use super::*;
    pub(super) fn pin_directory(_path: &Path) -> Result<(PathBuf, Vec<File>)> {
        bail!("UnsupportedCapability: no verified snapshot publication primitive for this platform")
    }
    pub(super) fn identity(_file: &File) -> Result<FileIdentity> {
        bail!("UnsupportedCapability")
    }
    pub(super) fn validate(_file: &File, _directory: bool) -> Result<()> {
        bail!("UnsupportedCapability")
    }
    pub(super) fn stamp(_file: &File) -> Result<Stamp> {
        bail!("UnsupportedCapability")
    }
    pub(super) fn links(_file: &File) -> Result<u64> {
        bail!("UnsupportedCapability")
    }
    pub(super) fn verify_directory(_path: &Path, _pins: &[File]) -> Result<()> {
        bail!("UnsupportedCapability")
    }
    pub(super) fn create_private(_parent: &File, _name: Option<&str>) -> Result<File> {
        bail!("UnsupportedCapability")
    }
    pub(super) fn verify_name(_parent: &File, _name: &str, _expected: &FileIdentity) -> Result<()> {
        bail!("UnsupportedCapability")
    }
    pub(super) fn publish(
        _file: &File,
        _parent: &File,
        _name: &str,
        _expected: &FileIdentity,
    ) -> Result<()> {
        bail!("UnsupportedCapability")
    }
    pub(super) fn sync_directory(_directory: &File) -> Result<()> {
        bail!("UnsupportedCapability")
    }
    pub(super) fn delete_private(_file: &File) -> Result<()> {
        bail!("UnsupportedCapability")
    }
    pub(super) fn verify_absent(
        _parent: &File,
        _name: &str,
        _expected: &FileIdentity,
    ) -> Result<()> {
        bail!("UnsupportedCapability")
    }

    pub(super) fn read_at(_file: &File, _bytes: &mut [u8], _offset: u64) -> Result<usize> {
        Err(unsupported(
            "No verified native snapshot descriptor on this platform",
        ))
    }
    pub(super) fn write_at(_file: &File, _bytes: &[u8], _offset: u64) -> Result<usize> {
        Err(unsupported(
            "No verified native snapshot descriptor on this platform",
        ))
    }
    pub(super) fn open_source(_parent: &File, _name: &str) -> Result<File> {
        Err(unsupported(
            "No verified native snapshot source descriptor on this platform",
        ))
    }
    pub(super) fn volume(_file: &File) -> Result<String> {
        Err(unsupported(
            "No verified native snapshot volume on this platform",
        ))
    }
    pub(super) fn ensure_private_directory(_parent: &File, _name: &str) -> Result<()> {
        Err(unsupported(
            "No verified private snapshot directory on this platform",
        ))
    }
    pub(super) fn validate_private_directory(_directory: &File) -> Result<()> {
        Err(unsupported(
            "No verified private snapshot directory on this platform",
        ))
    }
    pub(super) fn create_manifest(_parent: &File, _name: &str) -> Result<File> {
        Err(unsupported(
            "No verified native snapshot manifest on this platform",
        ))
    }
    pub(super) fn open_recovery(_parent: &File, _name: &str) -> Result<File> {
        Err(unsupported(
            "No verified native snapshot recovery descriptor on this platform",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_folder() -> tempfile::TempDir {
        tempfile::tempdir_in(std::env::temp_dir()).unwrap()
    }

    fn job(limiter: &SnapshotLimiter, image: u64) -> SnapshotPermit {
        limiter
            .reserve_job(image, image * 3 + 64 * 1024 * 1024 + MAX_MANIFEST_BYTES)
            .unwrap()
    }

    fn envelope(phase: SnapshotPhase, fact: &[u8]) -> Vec<u8> {
        let mut body = vec![phase as u8];
        body.extend_from_slice(fact);
        let mut record = (body.len() as u32).to_le_bytes().to_vec();
        record.extend_from_slice(&Sha256::digest(&body));
        record.extend_from_slice(&body);
        record
    }

    #[test]
    fn operation_permit_clones_share_one_owner_and_release_after_last_clone() {
        let limiter = SnapshotLimiter::new(MAX_SNAPSHOT_BYTES * 4).unwrap();
        let first = job(&limiter, 1024);
        let clone = first.clone();
        assert_eq!(limiter.stats().active, 1);
        let second = job(&limiter, 1024);
        let third = job(&limiter, 1024);
        let fourth = job(&limiter, 1024);
        assert!(limiter.reserve_job(1024, 65 * 1024 * 1024 + 3072).is_err());
        drop(first);
        assert_eq!(limiter.stats().active, 4);
        drop(clone);
        assert_eq!(limiter.stats().active, 3);
        drop((second, third, fourth));
        assert_eq!(limiter.stats().reserved_bytes, 0);
    }

    #[test]
    fn manifest_rejects_checksum_and_phase_changes_but_preserves_a_torn_tail() {
        let mut bytes = envelope(SnapshotPhase::Created, b"created");
        bytes.extend(envelope(SnapshotPhase::SourceCut, b"cut"));
        let complete = bytes.len();
        bytes.extend_from_slice(&[3, 0, 0]);
        let facts = parse_phase_facts(&bytes).unwrap();
        assert_eq!(facts.len(), 2);
        assert_eq!(facts[1].body, b"cut");
        bytes[complete - 1] ^= 1;
        assert!(parse_phase_facts(&bytes).is_err());
        let wrong_first = envelope(SnapshotPhase::Published, b"published");
        assert!(parse_phase_facts(&wrong_first).is_err());
        let mut repeated = envelope(SnapshotPhase::Created, b"created");
        repeated.extend(envelope(SnapshotPhase::Created, b"repeated"));
        assert!(parse_phase_facts(&repeated).is_err());
    }
    #[test]
    fn failed_actual_fid_inventory_keeps_unique_handle_until_journaled_cleanup() {
        let folder = test_folder();
        let directory = SnapshotDirectory::open_private(folder.path(), 1).unwrap();
        let limiter = SnapshotLimiter::new(1024 * 1024 * 1024).unwrap();
        let permit = job(&limiter, 1024 * 1024);
        let context = SnapshotContext::new(Duration::from_secs(30)).unwrap();
        let mut failure = ActualFile::create_journaled(
            &directory,
            "inventory-failure",
            1024 * 1024,
            &context,
            &permit,
            &|metadata| {
                if metadata.identity.is_some() {
                    Err(anyhow::anyhow!("Injected durable FID inventory failure"))
                } else {
                    Ok(())
                }
            },
        )
        .err()
        .expect("FID inventory failure must retain the created object");
        let owner = failure.file.take().unwrap();
        assert!(!owner.can_release_for_recovery());
        assert!(owner.metadata().identity.is_some());
        let witnessed = std::cell::Cell::new(false);
        owner
            .cleanup_journaled(&context, &|fact| {
                if fact.removed {
                    witnessed.set(true);
                }
                Ok(())
            })
            .unwrap();
        assert!(witnessed.get());
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
    }
    #[cfg(windows)]
    #[test]
    fn protected_actual_cleanup_close_retains_same_descriptor_and_retries_physical_close() {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Foundation::{
            HANDLE_FLAG_PROTECT_FROM_CLOSE, SetHandleInformation,
        };
        let folder = test_folder();
        let directory = SnapshotDirectory::open_private(folder.path(), 1).unwrap();
        let limiter = SnapshotLimiter::new(1024 * 1024 * 1024).unwrap();
        let permit = job(&limiter, 1024 * 1024);
        let context = SnapshotContext::new(Duration::from_secs(30)).unwrap();
        let mut owner = ActualFile::create_journaled(
            &directory,
            "protected-cleanup-close",
            1024 * 1024,
            &context,
            &permit,
            &|_| Ok(()),
        )
        .unwrap();
        owner
            .write_at(0, b"actual protected retirement", &context)
            .unwrap();
        owner.sync_all(&context).unwrap();
        let identity = owner.metadata().identity.unwrap();
        let raw = owner.file().unwrap().as_raw_handle();
        assert_ne!(
            unsafe {
                SetHandleInformation(
                    raw,
                    HANDLE_FLAG_PROTECT_FROM_CLOSE,
                    HANDLE_FLAG_PROTECT_FROM_CLOSE,
                )
            },
            0
        );
        let mut failure = owner.cleanup_journaled(&context, &|_| Ok(())).unwrap_err();
        let same = failure.file.take().unwrap();
        assert_eq!(same.file().unwrap().as_raw_handle(), raw);
        assert_eq!(same.metadata().identity.as_ref(), Some(&identity));
        assert!(same.deletion_requested && same.file.is_some());
        assert_eq!(limiter.stats().active, 1);
        assert_ne!(
            unsafe { SetHandleInformation(raw, HANDLE_FLAG_PROTECT_FROM_CLOSE, 0) },
            0
        );
        let completed = same.cleanup_journaled(&context, &|_| Ok(())).unwrap();
        assert_eq!(completed.identity.as_ref(), Some(&identity));
        drop(permit);
        assert_eq!(limiter.stats().active, 0);
    }

    #[cfg(any(windows, target_os = "linux"))]
    #[test]
    fn native_checked_file_enforces_length_chunk_and_subfile_limits() {
        let folder = test_folder();
        let private = SnapshotDirectory::open_private(folder.path(), 1).unwrap();
        let context = SnapshotContext::new(Duration::from_secs(30)).unwrap();
        let limiter = SnapshotLimiter::new(MAX_SNAPSHOT_BYTES * 4).unwrap();
        let permit = job(&limiter, 1024);
        let mut owner =
            ActualFile::create(&private, "request/含空格:metadata", 1024, &context, &permit)
                .unwrap();
        assert_eq!(owner.write_at(0, b"native", &context).unwrap(), 6);
        let mut bytes = [0u8; 6];
        assert_eq!(owner.read_at(0, &mut bytes, &context).unwrap(), 6);
        assert_eq!(&bytes, b"native");
        assert!(owner.write_at(1024, b"x", &context).is_err());
        assert!(owner.set_len(1025, &context).is_err());
        assert!(owner.write_at(0, &vec![0; IO_CHUNK + 1], &context).is_err());
        // A read beyond EOF does not allocate storage and supports SQLite's
        // required short-read/zero-fill behavior in the owning VFS.
        assert_eq!(owner.read_at(4096, &mut bytes, &context).unwrap(), 0);
        if let Some(name) = &owner.metadata().temporary_name {
            assert!(!name.contains("request"));
            assert!(!name.contains(':'));
        }
        let mut owners = vec![owner];
        for _ in 1..16 {
            owners.push(ActualFile::create(&private, "bounded", 1024, &context, &permit).unwrap());
        }
        let failure = match ActualFile::create(&private, "bounded", 1024, &context, &permit) {
            Ok(_) => panic!("Seventeenth native subfile was accepted"),
            Err(failure) => failure,
        };
        assert_eq!(failure.kind, NativeErrorKind::Quota);
        assert!(failure.file.is_none());
        assert_eq!(limiter.stats().active, 1);
    }

    #[cfg(any(windows, target_os = "linux"))]
    #[test]
    fn selected_copy_requires_seal_and_no_replace_preserves_foreign_final() {
        let folder = test_folder();
        let selected_path = folder.path().join("selected");
        std::fs::create_dir(&selected_path).unwrap();
        std::fs::write(selected_path.join("snapshot.sqlite3"), b"foreign").unwrap();
        let private = SnapshotDirectory::open_private(folder.path(), 1).unwrap();
        let selected = SnapshotDirectory::open_selected(&selected_path, 2).unwrap();
        let context = SnapshotContext::new(Duration::from_secs(30)).unwrap();
        let limiter = SnapshotLimiter::new(MAX_SNAPSHOT_BYTES * 4).unwrap();
        let permit = job(&limiter, 1024);
        let mut compact =
            ActualFile::create(&private, "snapshot", 1024, &context, &permit).unwrap();
        compact
            .write_at(0, b"sealed compact image", &context)
            .unwrap();
        assert!(
            compact
                .copy_verified_to_selected(&selected, "snapshot", &context, &permit, &|_| Ok(()))
                .is_err()
        );
        compact.seal_for_publication(&context).unwrap();
        assert!(compact.write_at(0, b"x", &context).is_err());
        let mut candidate = compact
            .copy_verified_to_selected(&selected, "snapshot", &context, &permit, &|_| Ok(()))
            .unwrap();
        assert!(candidate.publish("snapshot.sqlite3", &context).is_err());
        assert!(!candidate.metadata().published);
        assert_eq!(
            std::fs::read(selected_path.join("snapshot.sqlite3")).unwrap(),
            b"foreign"
        );
        let receipt = candidate.publish("available.sqlite3", &context).unwrap();
        assert_eq!(receipt.bytes, 20);
        assert_eq!(
            receipt.sha256,
            format!("{:x}", Sha256::digest(b"sealed compact image"))
        );
        assert_eq!(
            candidate.verify_published(&context).unwrap().identity,
            receipt.identity
        );
        let failure = candidate.cleanup(&context).unwrap_err();
        assert!(failure.file.as_ref().unwrap().metadata().published);
        assert_eq!(
            std::fs::read(selected_path.join("available.sqlite3")).unwrap(),
            b"sealed compact image"
        );
    }

    #[cfg(windows)]
    #[test]
    fn sqlite_handle_anchor_rejects_another_database_and_retains_failed_pin() {
        let folder = test_folder();
        let first_path = folder.path().join("first.sqlite3");
        let second_path = folder.path().join("second.sqlite3");
        let first = rusqlite::Connection::open(&first_path).unwrap();
        let second = rusqlite::Connection::open(&second_path).unwrap();
        first
            .execute_batch("CREATE TABLE native_proof(value INTEGER)")
            .unwrap();
        second
            .execute_batch("CREATE TABLE native_proof(value INTEGER)")
            .unwrap();
        let proof =
            unsafe { SourceFileProof::from_sqlite_main(first.handle(), &first_path) }.unwrap();
        proof.verify().unwrap();
        assert!(proof.len().unwrap() > 0);
        let failed =
            match unsafe { SourceFileProof::from_sqlite_main(first.handle(), &second_path) } {
                Ok(_) => panic!("SQLite source handle was accepted for another native object"),
                Err(failed) => failed,
            };
        assert_eq!(failed.kind, NativeErrorKind::Changed);
        assert!(failed.source.is_some());
        assert!(SourceFileProof::pin_checked_sidecar(&proof, "unrelated-shm").is_err());
    }
}

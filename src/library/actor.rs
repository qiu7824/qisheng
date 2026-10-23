//! Owned filesystem workers with one acknowledged batch per worker.
use super::authority::RelocationAdmissionError;
use super::authority::{
    AuthorityRegistry, AuthorizedFile, FileAuthorization, FileChangeStamp, LiveRelocationHint,
    PreparedAssetRelocation, RootAuthority, RootHealth, RootProof, RootScanGuard,
};
use super::scan::{
    DiscoveredFile, ScanContext, ScanError, ScanReport, ScanRequest, discover_authorized_stream,
};
use crate::audio::worker_owner::{TrackedWorker, WorkerKind, WorkerPermit};
use crate::catalog::{Catalog, PreparedLocalImport};
use crate::domain::library::*;
use crate::domain::library_watch::{LibraryAssetBinding, LibraryWatchToken, RelinkEvidence};
use crate::domain::music::{TrackRecord, normalize_local_path};
use anyhow::{Result, ensure};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, VecDeque};
use std::io::{Read, Seek, SeekFrom};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::PathBuf;
use std::sync::{
    Arc, Mutex, Weak,
    atomic::{AtomicBool, AtomicUsize, Ordering},
    mpsc::{self, Receiver, SyncSender, TryRecvError, TrySendError},
};
use std::thread;
use std::time::{Duration, Instant, UNIX_EPOCH};

const REQUEST_CAPACITY: usize = 8;
const WORKERS: usize = 2;
const COMPLETION_CAPACITY: usize = 2;
const BATCH_RESERVATION: usize = 8 * 1024 * 1024;
const CONTENT_HASH_BYTES: u64 = 1024 * 1024 * 1024;
const CONTENT_HASH_TIMEOUT: Duration = Duration::from_secs(30);
const METADATA_CACHE_BYTES: usize = 8 * 1024 * 1024;
const METADATA_CACHE_ITEMS: usize = 4096;
static PREPARING_BYTES: AtomicUsize = AtomicUsize::new(0);
#[derive(Debug)]
struct PreparationReservation;
impl PreparationReservation {
    fn new() -> Result<Self> {
        PREPARING_BYTES
            .try_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                current
                    .checked_add(BATCH_RESERVATION)
                    .filter(|total| *total <= 32 * 1024 * 1024)
            })
            .map_err(|_| LibraryError::budget("Library preparation budget exceeded"))?;
        Ok(Self)
    }
}
impl Drop for PreparationReservation {
    fn drop(&mut self) {
        PREPARING_BYTES.fetch_sub(BATCH_RESERVATION, Ordering::AcqRel);
    }
}
#[derive(Clone, Debug)]
pub struct LibraryBatchStorage {
    _reservation: Arc<PreparationReservation>,
    _files: Arc<Mutex<Vec<AuthorizedFile>>>,
    _scope: Arc<Mutex<Option<LibraryPrivateScope>>>,
}
struct LibraryPrivateScope {
    _guard: RootScanGuard,
}
impl std::fmt::Debug for LibraryPrivateScope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("LibraryPrivateScope")
    }
}

#[derive(Debug)]
pub struct LibraryAck {
    sender: SyncSender<std::result::Result<(), String>>,
    guards: Arc<Mutex<Vec<AuthorizedFile>>>,
    relocation: Arc<Mutex<Option<PreparedAssetRelocation>>>,
    scope: Arc<Mutex<Option<LibraryPrivateScope>>>,
}
impl LibraryAck {
    pub fn complete(self, result: std::result::Result<(), String>) -> bool {
        let result = result.map_err(|message| message.chars().take(1024).collect());
        let delivered = self.sender.try_send(result).is_ok();
        self.guards
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clear();
        self.relocation
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take();
        self.scope
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take();
        delivered
    }
}
impl Drop for LibraryAck {
    fn drop(&mut self) {
        self.guards
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clear();
        self.relocation
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take();
        self.scope
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take();
    }
}
#[derive(Debug)]
pub enum LibraryCompletion {
    RootPathPrepared {
        operation_id: String,
        watch_token: LibraryWatchToken,
        result: Result<Box<LibraryRootPathPreparation>>,
        ack: LibraryAck,
    },
    LiveRelocationHints {
        operation_id: String,
        root_id: String,
        watch_generation: u64,
        result: Result<Vec<LiveRelocationHint>>,
    },
    AutoRelinkPrepared {
        operation_id: String,
        watch_token: LibraryWatchToken,
        result: Result<LibraryAutoRelinkOutcome>,
        ack: LibraryAck,
    },
    RelinkPrepared {
        operation_id: String,
        result: Result<Box<LibraryRelinkPreparation>>,
        ack: LibraryAck,
    },
    RootValidated {
        operation_id: String,
        result: Result<LibraryRootDraft>,
        proof: Option<RootProof>,
    },
    RootVerified {
        root_id: String,
        grant_epoch: u64,
        health: RootHealth,
        error: Option<LibraryError>,
    },
    Batch {
        token: LibraryScanToken,
        seen: Vec<LibrarySeenFile>,
        prepared: Vec<(PreparedLocalImport, LibraryPreparedAsset)>,
        content_evidence: Vec<LibraryContentEvidence>,
        progress: LibraryOperationView,
        ack: LibraryAck,
        _storage: LibraryBatchStorage,
    },
    Finished {
        token: LibraryScanToken,
        outcome: LibraryOperationView,
        discovery_complete: bool,
    },
}
#[derive(Debug)]
pub struct LibraryRelinkPreparation {
    pub local: PreparedLocalImport,
    pub prepared: LibraryPreparedAsset,
    pub content_sha256: String,
    _storage: LibraryBatchStorage,
}
#[derive(Debug)]
pub struct LibraryRootPathPreparation {
    pub local: PreparedLocalImport,
    pub prepared: LibraryPreparedAsset,
    pub content_evidence: Option<LibraryContentEvidence>,
    stamp: ContentStamp,
    _storage: LibraryBatchStorage,
}
impl LibraryRootPathPreparation {
    pub fn check(&self) -> Result<()> {
        let files = self
            ._storage
            ._files
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let opened = files
            .first()
            .ok_or_else(|| LibraryError::conflict("Fresh root path proof was acknowledged"))?;
        ensure!(
            Some(opened.identity()) == self.prepared.file_identity.as_ref()
                && normalize_local_path(&opened.current_path()?)
                    == normalize_local_path(&self.prepared.asset.path),
            LibraryError::conflict("Fresh root path moved or changed after preparation")
        );
        self.stamp.check(opened)
    }
}
#[derive(Clone, Debug, serde::Serialize)]
pub struct LibraryContentEvidence {
    pub path: PathBuf,
    pub file_identity: FileIdentity,
    pub size_bytes: u64,
    pub modified_ms: Option<u64>,
    pub content_sha256: String,
    #[serde(skip)]
    stamp: ContentStamp,
    #[serde(skip)]
    files: Weak<Mutex<Vec<AuthorizedFile>>>,
}
impl LibraryContentEvidence {
    pub fn check(&self) -> Result<()> {
        let files = self
            .files
            .upgrade()
            .ok_or_else(|| LibraryError::conflict("Content evidence proof was released"))?;
        let files = files.lock().unwrap_or_else(|error| error.into_inner());
        let file = files
            .iter()
            .find(|file| {
                file.path() == self.path.as_path() && file.identity() == &self.file_identity
            })
            .ok_or_else(|| LibraryError::conflict("Content evidence proof was acknowledged"))?;
        self.stamp.check(file)
    }
}
#[derive(Debug)]
pub enum LibraryAutoRelinkOutcome {
    Prepared(Box<LibraryAutoRelinkPreparation>),
    NeedsSelection(LibraryAutoRelinkSelection),
}
#[derive(Debug)]
pub struct LibraryAutoRelinkSelection {
    pub asset_id: String,
    pub old_path: PathBuf,
    pub new_path: PathBuf,
    pub new_identity: FileIdentity,
    pub expected_binding_revision: u64,
    pub reason: LibraryError,
}
#[derive(Debug)]
pub struct LibraryAutoRelinkPreparation {
    pub local: PreparedLocalImport,
    pub prepared: LibraryPreparedAsset,
    pub evidence: RelinkEvidence,
    pub content_sha256: Option<String>,
    pub expected_binding_revision: u64,
    relocation: Arc<Mutex<Option<PreparedAssetRelocation>>>,
    _storage: LibraryBatchStorage,
}
impl LibraryAutoRelinkPreparation {
    pub fn check(&self) -> Result<()> {
        let relocation = self
            .relocation
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        relocation
            .as_ref()
            .ok_or_else(|| {
                LibraryError::conflict("Relocation proof was acknowledged or published")
            })?
            .check()
    }
    pub fn publish(
        &mut self,
        authority: &AuthorityRegistry,
        committed: &LocalAssetAuthority,
    ) -> Result<FileAuthorization> {
        let ticket = self
            .relocation
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take()
            .ok_or_else(|| {
                LibraryError::conflict("Relocation proof was acknowledged or published")
            })?;
        authority.publish_asset_relocation(ticket, committed)
    }
}
enum Request {
    RefreshRootPath {
        operation_id: String,
        path: PathBuf,
        root: LibraryRoot,
        watch_token: LibraryWatchToken,
        expected_identity: FileIdentity,
        context: ScanContext,
    },
    LiveRelocationHints {
        operation_id: String,
        root: LibraryRoot,
        watch_token: LibraryWatchToken,
        context: ScanContext,
    },
    AutoRelink {
        operation_id: String,
        path: PathBuf,
        original: Box<TrackRecord>,
        old_binding: Box<LibraryAssetBinding>,
        target_authority: LocalAssetAuthority,
        root: LibraryRoot,
        watch_token: LibraryWatchToken,
        context: ScanContext,
    },
    Relink {
        operation_id: String,
        path: PathBuf,
        original: Box<TrackRecord>,
        asset_id: String,
        context: ScanContext,
    },
    Verify {
        root: LibraryRoot,
        context: ScanContext,
    },
    Validate {
        operation_id: String,
        path: PathBuf,
        recursive: bool,
        context: ScanContext,
    },
    Scan {
        token: LibraryScanToken,
        root: LibraryRoot,
        options: LibraryScanOptions,
        context: ScanContext,
        content_evidence: bool,
    },
}
struct Job {
    root_id: Option<String>,
    cancel: Arc<AtomicBool>,
    verification: bool,
}
#[cfg(test)]
#[derive(Default)]
struct TestHooks {
    panic_validation: AtomicBool,
}
/// Shutdown retains every unfinished handle; a timeout is not a successful join.
pub struct LibraryActor {
    sender: SyncSender<Request>,
    completions: Receiver<LibraryCompletion>,
    jobs: Arc<Mutex<BTreeMap<String, Job>>>,
    closing: Arc<AtomicBool>,
    queued: Arc<AtomicUsize>,
    workers: Vec<TrackedWorker>,
    join_error: bool,
    authority: AuthorityRegistry,
    hashing: Arc<AtomicBool>,
    metadata_cache: Arc<Mutex<PreparedMetadataCache>>,
    #[cfg(test)]
    hooks: Arc<TestHooks>,
}
impl LibraryActor {
    pub fn new(catalog: Catalog) -> Result<Self> {
        Self::new_with_authority(catalog, AuthorityRegistry::default())
    }
    pub fn new_with_authority(catalog: Catalog, authority: AuthorityRegistry) -> Result<Self> {
        authority.prepare_retirement()?;
        let (sender, receiver) = mpsc::sync_channel(REQUEST_CAPACITY);
        let receiver = Arc::new(Mutex::new(receiver));
        let (complete, completions) = mpsc::sync_channel(COMPLETION_CAPACITY);
        let jobs = Arc::new(Mutex::new(BTreeMap::new()));
        let closing = Arc::new(AtomicBool::new(false));
        let mut actor = Self {
            sender,
            completions,
            jobs,
            closing,
            queued: Arc::new(AtomicUsize::new(0)),
            workers: Vec::with_capacity(WORKERS),
            join_error: false,
            authority,
            hashing: Arc::new(AtomicBool::new(false)),
            metadata_cache: Arc::default(),
            #[cfg(test)]
            hooks: Arc::new(TestHooks::default()),
        };
        for index in 0..WORKERS {
            let permit = WorkerPermit::reserve(WorkerKind::Library)?;
            let receiver = receiver.clone();
            let complete = complete.clone();
            let closing = actor.closing.clone();
            let catalog = catalog.clone();
            let queued = actor.queued.clone();
            let authority = actor.authority.clone();
            let hashing = actor.hashing.clone();
            let metadata_cache = actor.metadata_cache.clone();
            #[cfg(test)]
            let hooks = actor.hooks.clone();
            let handle = thread::Builder::new()
                .name(format!("lx-library-{index}"))
                .spawn(move || {
                    #[cfg(test)]
                    worker(
                        receiver,
                        complete,
                        catalog,
                        closing,
                        queued,
                        authority,
                        hashing,
                        metadata_cache,
                        hooks,
                    );
                    #[cfg(not(test))]
                    worker(
                        receiver,
                        complete,
                        catalog,
                        closing,
                        queued,
                        authority,
                        hashing,
                        metadata_cache,
                    );
                })?;
            actor.workers.push(permit.attach(handle));
        }
        Ok(actor)
    }
    pub fn can_submit_scan(&self, root_id: &str) -> Result<()> {
        validate_library_id(root_id)?;
        ensure!(
            !self.closing.load(Ordering::Acquire)
                && !self.join_error
                && !self.workers.iter().any(TrackedWorker::is_finished),
            LibraryError::new(
                LibraryErrorKind::Unavailable,
                "Library workers are unavailable"
            )
        );
        let jobs = self.jobs.lock().unwrap_or_else(|error| error.into_inner());
        ensure!(
            self.queued.load(Ordering::Acquire) < REQUEST_CAPACITY
                && jobs.len() < REQUEST_CAPACITY + WORKERS,
            LibraryError::new(LibraryErrorKind::Busy, "Library request capacity reached")
        );
        ensure!(
            !jobs
                .values()
                .any(|job| !job.verification && job.root_id.as_deref() == Some(root_id)),
            LibraryError::new(LibraryErrorKind::Busy, "Library root already has a scan")
        );
        Ok(())
    }
    pub fn submit_validate(
        &self,
        operation_id: String,
        path: PathBuf,
        recursive: bool,
        context: ScanContext,
    ) -> Result<()> {
        validate_library_id(&operation_id)?;
        validate_library_path(&path)?;
        self.submit(
            operation_id.clone(),
            None,
            context.cancel.clone(),
            false,
            Request::Validate {
                operation_id,
                path,
                recursive,
                context,
            },
        )
    }
    pub fn submit_relink(
        &self,
        operation_id: String,
        path: PathBuf,
        original: TrackRecord,
        asset_id: String,
        context: ScanContext,
    ) -> Result<()> {
        validate_library_id(&operation_id)?;
        validate_library_path(&path)?;
        validate_library_id(&asset_id)?;
        ensure!(
            original.platform.source == "local"
                && original.local_asset_id.as_deref() == Some(asset_id.as_str()),
            LibraryError::invalid("Relink track and asset identities disagree")
        );
        self.submit(
            operation_id.clone(),
            None,
            context.cancel.clone(),
            false,
            Request::Relink {
                operation_id,
                path,
                original: Box::new(original),
                asset_id,
                context,
            },
        )
    }
    pub fn submit_live_relocation_hints(
        &self,
        operation_id: String,
        root: LibraryRoot,
        watch_token: LibraryWatchToken,
        context: ScanContext,
    ) -> Result<()> {
        validate_library_id(&operation_id)?;
        validate_auto_watch(&root, &watch_token)?;
        self.submit(
            operation_id.clone(),
            Some(root.root_id.clone()),
            context.cancel.clone(),
            true,
            Request::LiveRelocationHints {
                operation_id,
                root,
                watch_token,
                context,
            },
        )
    }
    #[allow(clippy::too_many_arguments)]
    pub fn submit_refresh_root_path(
        &self,
        operation_id: String,
        path: PathBuf,
        root: LibraryRoot,
        watch_token: LibraryWatchToken,
        expected_path_identity: FileIdentity,
        context: ScanContext,
    ) -> Result<()> {
        validate_library_id(&operation_id)?;
        validate_library_path(&path)?;
        expected_path_identity.validate()?;
        validate_auto_watch(&root, &watch_token)?;
        self.submit(
            operation_id.clone(),
            Some(root.root_id.clone()),
            context.cancel.clone(),
            true,
            Request::RefreshRootPath {
                operation_id,
                path: normalize_local_path(&path),
                root,
                watch_token,
                expected_identity: expected_path_identity,
                context,
            },
        )
    }
    #[allow(clippy::too_many_arguments)]
    pub fn submit_auto_relink(
        &self,
        operation_id: String,
        path: PathBuf,
        original: TrackRecord,
        old_binding: LibraryAssetBinding,
        mut target_authority: LocalAssetAuthority,
        root: LibraryRoot,
        watch_token: LibraryWatchToken,
        context: ScanContext,
    ) -> Result<()> {
        validate_library_id(&operation_id)?;
        validate_library_path(&path)?;
        validate_library_path(&old_binding.path)?;
        validate_library_id(&old_binding.asset_id)?;
        validate_auto_watch(&root, &watch_token)?;
        ensure!(
            original.platform.source == "local",
            LibraryError::invalid("Automatic relocation original track is not local")
        );
        ensure!(
            original.local_asset_id.as_deref() == Some(old_binding.asset_id.as_str()),
            LibraryError::invalid(
                "Automatic relocation original track asset_id differs from binding asset_id"
            )
        );
        ensure!(
            target_authority.asset_id == old_binding.asset_id,
            LibraryError::invalid(
                "Automatic relocation target asset_id differs from original binding asset_id"
            )
        );
        ensure!(
            target_authority.track_id == original.id,
            LibraryError::invalid(
                "Automatic relocation target track_id differs from original track id"
            )
        );
        let path = normalize_local_path(&path);
        ensure!(
            normalize_local_path(&target_authority.path) == path,
            LibraryError::invalid(
                "Automatic relocation target authority path differs from candidate path"
            )
        );
        target_authority.path.clone_from(&path);
        ensure!(
            target_authority.explicit_file_epoch.is_none(),
            LibraryError::invalid("Automatic relocation target would renew an exact file grant")
        );
        ensure!(
            old_binding.binding_revision > 0 && old_binding.binding_revision <= i64::MAX as u64,
            LibraryError::invalid("Automatic relocation original binding revision is invalid")
        );
        let target_root = target_authority
            .roots
            .iter()
            .find(|target| target.root_id == root.root_id)
            .ok_or_else(|| {
                LibraryError::conflict("Automatic relocation target has no original root_id grant")
            })?;
        ensure!(
            target_root.grant_epoch == root.grant_epoch,
            LibraryError::conflict("Automatic relocation original root grant_epoch differs")
        );
        ensure!(
            target_root.anchor == root.anchor,
            LibraryError::conflict("Automatic relocation original root physical anchor differs")
        );
        ensure!(
            target_root.recursive == root.recursive,
            LibraryError::conflict("Automatic relocation original root recursive scope differs")
        );
        ensure!(
            target_root.path_policy == root.path_policy,
            LibraryError::conflict("Automatic relocation original root path_policy differs")
        );
        ensure!(
            normalize_local_path(&target_root.path) == normalize_local_path(&root.path),
            LibraryError::conflict("Automatic relocation original root locator path differs")
        );
        target_authority.permission_sha256()?;
        if let Some(identity) = &old_binding.file_identity {
            identity.validate()?;
        }
        ensure!(
            serde_json::to_vec(&(&original, &old_binding, &target_authority, &watch_token))?.len()
                <= MAX_LIBRARY_BATCH_BYTES,
            LibraryError::budget("Automatic relocation request exceeds 1 MiB")
        );
        self.submit(
            operation_id.clone(),
            Some(root.root_id.clone()),
            context.cancel.clone(),
            true,
            Request::AutoRelink {
                operation_id,
                path,
                original: Box::new(original),
                old_binding: Box::new(old_binding),
                target_authority,
                root,
                watch_token,
                context,
            },
        )
    }
    pub fn submit_verify(&self, root: LibraryRoot, context: ScanContext) -> Result<()> {
        root.validate()?;
        context.check()?;
        self.authority.install_root(&root)?;
        let operation_id = crate::domain::music::stable_id(
            "root-health",
            format!("{}\0{}", root.root_id, root.grant_epoch).as_bytes(),
        );
        self.submit(
            operation_id.clone(),
            Some(root.root_id.clone()),
            context.cancel.clone(),
            true,
            Request::Verify { root, context },
        )
    }
    pub fn submit_scan(
        &self,
        token: LibraryScanToken,
        root: LibraryRoot,
        options: LibraryScanOptions,
        context: ScanContext,
    ) -> Result<()> {
        self.submit_scan_inner(token, root, options, context, false)
    }
    /// Called only for committed watch intent that enables automatic relocation.
    pub fn submit_scan_with_content_evidence(
        &self,
        token: LibraryScanToken,
        root: LibraryRoot,
        options: LibraryScanOptions,
        context: ScanContext,
    ) -> Result<()> {
        self.submit_scan_inner(token, root, options, context, true)
    }
    fn submit_scan_inner(
        &self,
        token: LibraryScanToken,
        root: LibraryRoot,
        options: LibraryScanOptions,
        context: ScanContext,
        content_evidence: bool,
    ) -> Result<()> {
        token.validate()?;
        root.validate()?;
        options.validate()?;
        ensure!(
            token.root_id == root.root_id
                && token.grant_epoch == root.grant_epoch
                && token.generation == root.generation,
            LibraryError::conflict("Library scan token does not match root")
        );
        self.submit(
            token.operation_id.clone(),
            Some(token.root_id.clone()),
            context.cancel.clone(),
            false,
            Request::Scan {
                token,
                root,
                options,
                context,
                content_evidence,
            },
        )
    }
    fn submit(
        &self,
        operation_id: String,
        root_id: Option<String>,
        cancel: Arc<AtomicBool>,
        verification: bool,
        request: Request,
    ) -> Result<()> {
        ensure!(
            !self.closing.load(Ordering::Acquire),
            LibraryError::new(LibraryErrorKind::Unavailable, "Library actor is closing")
        );
        let mut jobs = self.jobs.lock().unwrap_or_else(|error| error.into_inner());
        ensure!(
            jobs.len() < REQUEST_CAPACITY + WORKERS && !jobs.contains_key(&operation_id),
            LibraryError::new(LibraryErrorKind::Busy, "Library request capacity reached")
        );
        ensure!(
            verification
                || root_id.is_none()
                || !jobs
                    .values()
                    .any(|job| !job.verification && job.root_id == root_id),
            LibraryError::new(LibraryErrorKind::Busy, "Library root already has a scan")
        );
        self.queued.fetch_add(1, Ordering::AcqRel);
        if self.sender.try_send(request).is_err() {
            self.queued.fetch_sub(1, Ordering::AcqRel);
            return Err(
                LibraryError::new(LibraryErrorKind::Busy, "Library request queue is full").into(),
            );
        }
        jobs.insert(
            operation_id,
            Job {
                root_id,
                cancel,
                verification,
            },
        );
        Ok(())
    }
    pub fn try_recv(&mut self) -> Option<LibraryCompletion> {
        let completion = self.completions.try_recv().ok()?;
        let terminal = match &completion {
            LibraryCompletion::RootPathPrepared { operation_id, .. } => Some(operation_id),
            LibraryCompletion::AutoRelinkPrepared { operation_id, .. }
            | LibraryCompletion::LiveRelocationHints { operation_id, .. } => Some(operation_id),
            LibraryCompletion::RelinkPrepared { operation_id, .. } => Some(operation_id),
            LibraryCompletion::RootValidated { operation_id, .. } => Some(operation_id),
            LibraryCompletion::Finished { token, .. } => Some(&token.operation_id),
            LibraryCompletion::RootVerified {
                root_id,
                grant_epoch,
                ..
            } => {
                let id = crate::domain::music::stable_id(
                    "root-health",
                    format!("{root_id}\0{grant_epoch}").as_bytes(),
                );
                self.jobs
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .remove(&id);
                None
            }
            LibraryCompletion::Batch { .. } => None,
        };
        if let Some(id) = terminal {
            self.jobs
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .remove(id);
        }
        Some(completion)
    }
    pub fn cancel(&self, operation_id: &str) -> bool {
        if let Some(job) = self
            .jobs
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get(operation_id)
        {
            job.cancel.store(true, Ordering::Release);
            true
        } else {
            false
        }
    }
    pub fn cancel_root(&self, root_id: &str) {
        for job in self
            .jobs
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .values()
        {
            if job.root_id.as_deref() == Some(root_id) {
                job.cancel.store(true, Ordering::Release);
            }
        }
    }
    pub fn active_jobs(&self) -> usize {
        self.jobs
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .len()
    }
    pub fn metadata_cache_stats(&self) -> LibraryPreparationCacheStats {
        let cache = self
            .metadata_cache
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        LibraryPreparationCacheStats {
            entries: cache.entries.len(),
            owned_bytes: cache.bytes,
            hits: cache.hits,
            misses: cache.misses,
        }
    }
    pub fn shutdown(&mut self, timeout: Duration) -> Result<bool> {
        self.authority.begin_shutdown();
        self.closing.store(true, Ordering::Release);
        for job in self
            .jobs
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .values()
        {
            job.cancel.store(true, Ordering::Release);
        }
        for worker in &self.workers {
            worker.unpark();
        }
        let deadline = Instant::now()
            .checked_add(timeout)
            .unwrap_or_else(Instant::now);
        loop {
            // Dropping pending acknowledgements lets workers release their file
            // guards on their own thread; no completion is committed here.
            while self.try_recv().is_some() {}
            let mut index = 0;
            while index < self.workers.len() {
                if self.workers[index].wait(Duration::ZERO) {
                    let worker = self.workers.swap_remove(index);
                    self.join_error |= worker.panicked();
                } else {
                    index += 1;
                }
            }
            if self.workers.is_empty() && self.authority.retirement_pending() == 0 {
                self.metadata_cache
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .clear();
                self.jobs
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .clear();
                ensure!(!self.join_error, "Library worker panicked during shutdown");
                return Ok(true);
            }
            if Instant::now() >= deadline {
                return Ok(false);
            }
            thread::sleep(Duration::from_millis(2));
        }
    }
}
impl Drop for LibraryActor {
    fn drop(&mut self) {
        self.authority.begin_shutdown();
        self.closing.store(true, Ordering::Release);
        for job in self
            .jobs
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .values()
        {
            job.cancel.store(true, Ordering::Release);
        }
        for worker in &self.workers {
            worker.unpark();
        }
    }
}
#[allow(clippy::too_many_arguments)]
fn worker(
    receiver: Arc<Mutex<Receiver<Request>>>,
    sender: SyncSender<LibraryCompletion>,
    catalog: Catalog,
    closing: Arc<AtomicBool>,
    queued: Arc<AtomicUsize>,
    authority: AuthorityRegistry,
    hashing: Arc<AtomicBool>,
    metadata_cache: Arc<Mutex<PreparedMetadataCache>>,
    #[cfg(test)] hooks: Arc<TestHooks>,
) {
    while !closing.load(Ordering::Acquire) {
        let request = receiver
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .recv_timeout(Duration::from_millis(10));
        let Ok(request) = request else {
            continue;
        };
        queued.fetch_sub(1, Ordering::AcqRel);
        match request {
            Request::RefreshRootPath {
                operation_id,
                path,
                root,
                watch_token,
                expected_identity,
                context,
            } => {
                let result = catch_unwind(AssertUnwindSafe(|| {
                    prepare_refresh_root_path(
                        &catalog,
                        &authority,
                        &hashing,
                        &metadata_cache,
                        &closing,
                        &path,
                        &root,
                        &watch_token,
                        &expected_identity,
                        &context,
                    )
                }))
                .unwrap_or_else(|_| {
                    Err(LibraryError::new(
                        LibraryErrorKind::Unavailable,
                        "Library root path preparation worker panicked",
                    )
                    .into())
                });
                let (ack, waiting) = mpsc::sync_channel(1);
                let (guards, scope) = result
                    .as_ref()
                    .map(|prepared| {
                        (
                            prepared._storage._files.clone(),
                            prepared._storage._scope.clone(),
                        )
                    })
                    .unwrap_or_default();
                if send_terminal(
                    &sender,
                    LibraryCompletion::RootPathPrepared {
                        operation_id,
                        watch_token,
                        result,
                        ack: LibraryAck {
                            sender: ack,
                            guards,
                            scope,
                            relocation: Arc::default(),
                        },
                    },
                    &closing,
                )
                .is_ok()
                {
                    wait_relink_ack(&waiting, &context, &closing);
                }
            }
            Request::LiveRelocationHints {
                operation_id,
                root,
                watch_token,
                context,
            } => {
                let result =
                    catch_unwind(AssertUnwindSafe(|| -> Result<Vec<LiveRelocationHint>> {
                        context.check()?;
                        validate_auto_watch(&root, &watch_token)?;
                        authority.watch_authority(&root)?.check()?;
                        let hints = authority.live_relocation_hints(
                            &root.root_id,
                            root.grant_epoch,
                            MAX_LIBRARY_BATCH_ITEMS,
                        )?;
                        context.check()?;
                        Ok(hints)
                    }))
                    .unwrap_or_else(|_| {
                        Err(LibraryError::new(
                            LibraryErrorKind::Unavailable,
                            "Library relocation hint worker panicked",
                        )
                        .into())
                    });
                let _ = send_terminal(
                    &sender,
                    LibraryCompletion::LiveRelocationHints {
                        operation_id,
                        root_id: root.root_id,
                        watch_generation: watch_token.watch_generation,
                        result,
                    },
                    &closing,
                );
            }
            Request::AutoRelink {
                operation_id,
                path,
                original,
                old_binding,
                target_authority,
                root,
                watch_token,
                context,
            } => {
                let result = catch_unwind(AssertUnwindSafe(|| {
                    prepare_auto_relink(
                        &catalog,
                        &authority,
                        &hashing,
                        &closing,
                        &path,
                        &original,
                        &old_binding,
                        &target_authority,
                        &root,
                        &watch_token,
                        &context,
                    )
                }))
                .unwrap_or_else(|_| {
                    Err(LibraryError::new(
                        LibraryErrorKind::Unavailable,
                        "Automatic library relocation worker panicked",
                    )
                    .into())
                });
                let (ack, waiting) = mpsc::sync_channel(1);
                let (guards, relocation, scope) = match &result {
                    Ok(LibraryAutoRelinkOutcome::Prepared(prepared)) => (
                        prepared._storage._files.clone(),
                        prepared.relocation.clone(),
                        prepared._storage._scope.clone(),
                    ),
                    _ => (Arc::default(), Arc::default(), Arc::default()),
                };
                if send_terminal(
                    &sender,
                    LibraryCompletion::AutoRelinkPrepared {
                        operation_id,
                        watch_token,
                        result,
                        ack: LibraryAck {
                            sender: ack,
                            guards,
                            relocation,
                            scope,
                        },
                    },
                    &closing,
                )
                .is_ok()
                {
                    wait_relink_ack(&waiting, &context, &closing);
                }
            }
            Request::Relink {
                operation_id,
                path,
                original,
                asset_id,
                context,
            } => {
                let mut opened_guard = None;
                let result = catch_unwind(AssertUnwindSafe(
                    || -> Result<Box<LibraryRelinkPreparation>> {
                        let mut storage = LibraryBatchStorage {
                            _reservation: Arc::new(PreparationReservation::new()?),
                            _files: Arc::new(Mutex::new(Vec::new())),
                            _scope: Arc::default(),
                        };
                        context.check()?;
                        let selected = authority.open_selection(&path)?;
                        let mut opened = selected
                            .authorization()
                            .with_cancel(context.cancel.clone())?
                            .open()?;
                        let before = opened.metadata()?;
                        let content = capture_content_evidence(
                            &mut opened,
                            &context,
                            &closing,
                            &hashing,
                            &mut ContentHashBudget::default(),
                        )?;
                        let mut local = catalog.prepare_authorized(opened.try_clone()?)?;
                        let mut prepared = local.library_asset();
                        prepared.asset.id = asset_id;
                        prepared.asset.track_id.clone_from(&original.id);
                        prepared.track.id.clone_from(&original.id);
                        prepared.track.lx_id.clone_from(&original.lx_id);
                        prepared.track.local_asset_id = Some(prepared.asset.id.clone());
                        local.bind_library_identity(&prepared)?;
                        if !local.tags_verified() {
                            local.preserve_metadata_from(&original)?;
                            prepared.track = local.library_asset().track;
                        }
                        ensure!(
                            local.owned_metadata_bytes() + serde_json::to_vec(&prepared)?.len()
                                <= MAX_LIBRARY_BATCH_BYTES,
                            LibraryError::budget("Relink prepared metadata exceeds 1 MiB")
                        );
                        let stable_after = Instant::now() + Duration::from_millis(100);
                        while Instant::now() < stable_after {
                            context.check()?;
                            thread::sleep(Duration::from_millis(10));
                        }
                        let after = opened.metadata()?;
                        ensure!(
                            before.len() == after.len()
                                && before.modified().ok() == after.modified().ok(),
                            LibraryError::conflict(
                                "Selected file changed during relink preparation"
                            )
                        );
                        content.stamp.check(&opened)?;
                        context.check()?;
                        storage._files = Arc::new(Mutex::new(vec![opened.try_clone()?]));
                        opened_guard = Some(opened);
                        Ok(Box::new(LibraryRelinkPreparation {
                            local,
                            prepared,
                            content_sha256: content.content_sha256,
                            _storage: storage,
                        }))
                    },
                ))
                .unwrap_or_else(|_| {
                    Err(LibraryError::new(
                        LibraryErrorKind::Unavailable,
                        "Library relocation worker panicked",
                    )
                    .into())
                });
                let (ack, waiting) = mpsc::sync_channel(1);
                let guards = result
                    .as_ref()
                    .map(|prepared| prepared._storage._files.clone())
                    .unwrap_or_default();
                let delivered = send_terminal(
                    &sender,
                    LibraryCompletion::RelinkPrepared {
                        operation_id,
                        result,
                        ack: LibraryAck {
                            sender: ack,
                            guards,
                            relocation: Arc::default(),
                            scope: Arc::default(),
                        },
                    },
                    &closing,
                );
                if delivered.is_ok() {
                    wait_relink_ack(&waiting, &context, &closing);
                }
                drop(opened_guard);
            }
            Request::Verify { root, context } => {
                let result =
                    catch_unwind(AssertUnwindSafe(|| authority.verify_root(&root, &context)))
                        .unwrap_or_else(|_| {
                            Err(LibraryError::new(
                                LibraryErrorKind::Unavailable,
                                "Library root verification worker panicked",
                            )
                            .into())
                        });
                let error = result.err().map(|error| {
                    LibraryError::new(
                        if context.cancel.load(Ordering::Acquire) {
                            LibraryErrorKind::Cancelled
                        } else if Instant::now() >= context.deadline {
                            LibraryErrorKind::Timeout
                        } else {
                            LibraryErrorKind::Unavailable
                        },
                        error.to_string().chars().take(2048).collect::<String>(),
                    )
                });
                let _ = send_terminal(
                    &sender,
                    LibraryCompletion::RootVerified {
                        root_id: root.root_id.clone(),
                        grant_epoch: root.grant_epoch,
                        health: authority.root_health(&root.root_id, root.grant_epoch),
                        error,
                    },
                    &closing,
                );
            }
            Request::Validate {
                operation_id,
                path,
                recursive,
                context,
            } => {
                let result = catch_unwind(AssertUnwindSafe(|| {
                    let verified =
                        authority.open_root_for_validation(&path, recursive, &context)?;
                    #[cfg(test)]
                    if hooks.panic_validation.swap(false, Ordering::AcqRel) {
                        panic!("injected library validation panic after real root open");
                    }
                    Ok((verified.draft(), verified.proof()?))
                }))
                .unwrap_or_else(|_| {
                    Err(LibraryError::new(
                        LibraryErrorKind::Unavailable,
                        "Library validation worker panicked",
                    )
                    .into())
                });
                let (result, proof) = match result {
                    Ok((draft, proof)) => (Ok(draft), Some(proof)),
                    Err(error) => (Err(error), None),
                };
                let _ = send_terminal(
                    &sender,
                    LibraryCompletion::RootValidated {
                        operation_id,
                        result,
                        proof,
                    },
                    &closing,
                );
            }
            Request::Scan {
                token,
                root,
                options,
                context,
                content_evidence,
            } => {
                let (outcome, discovery_complete) = catch_unwind(AssertUnwindSafe(|| {
                    let execution = ScanExecution {
                        sender: &sender,
                        token: &token,
                        catalog: &catalog,
                        context: &context,
                        closing: &closing,
                        hashing: &hashing,
                        content_evidence,
                        metadata_cache: &metadata_cache,
                    };
                    run_scan(&execution, &root, &options, &authority)
                }))
                .unwrap_or_else(|_| {
                    let mut outcome = LibraryOperationView::running(&token);
                    outcome.state = LibraryOperationState::Failed;
                    outcome.error = Some(LibraryError::new(
                        LibraryErrorKind::Unavailable,
                        "Library scan worker panicked",
                    ));
                    (outcome, false)
                });
                let _ = send_terminal(
                    &sender,
                    LibraryCompletion::Finished {
                        token,
                        outcome,
                        discovery_complete,
                    },
                    &closing,
                );
            }
        }
    }
}
fn validate_auto_watch(root: &LibraryRoot, token: &LibraryWatchToken) -> Result<()> {
    root.validate()?;
    token.validate()?;
    ensure!(
        token.enabled
            && token.options.auto_relink
            && token.root_id == root.root_id
            && token.grant_epoch == root.grant_epoch,
        LibraryError::conflict("Automatic relocation watch intent is inactive or stale")
    );
    Ok(())
}
fn wait_relink_ack(
    waiting: &Receiver<std::result::Result<(), String>>,
    context: &ScanContext,
    closing: &AtomicBool,
) {
    loop {
        if context.check().is_err() || closing.load(Ordering::Acquire) {
            break;
        }
        match waiting.recv_timeout(Duration::from_millis(10)) {
            Ok(_) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
    }
}
struct HashLease<'a>(&'a AtomicBool);
impl<'a> HashLease<'a> {
    fn acquire(
        hashing: &'a AtomicBool,
        context: &ScanContext,
        closing: &AtomicBool,
        deadline: Instant,
    ) -> Result<Self> {
        loop {
            context.check()?;
            ensure!(
                !closing.load(Ordering::Acquire),
                LibraryError::new(LibraryErrorKind::Cancelled, "Library actor is closing")
            );
            ensure!(
                Instant::now() < deadline,
                LibraryError::budget("Library content evidence deadline exceeded")
            );
            if hashing
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return Ok(Self(hashing));
            }
            thread::sleep(Duration::from_millis(2));
        }
    }
}
impl Drop for HashLease<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
struct ContentStamp {
    size: u64,
    modified: std::time::SystemTime,
    #[cfg(target_os = "linux")]
    changed: (i64, i64),
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LibraryPreparationCacheStats {
    pub entries: usize,
    pub owned_bytes: usize,
    pub hits: u64,
    pub misses: u64,
}
#[derive(Clone, Debug, PartialEq, Eq)]
struct PreparedMetadataKey {
    root_id: String,
    root_epoch: u64,
    root_anchor: FileIdentity,
    path: PathBuf,
    identity: FileIdentity,
    stamp: ContentStamp,
    change: FileChangeStamp,
}
impl PreparedMetadataKey {
    fn capture(root: &LibraryRoot, file: &AuthorizedFile) -> Result<Self> {
        let snapshot = file.metadata_snapshot()?;
        let stamp = ContentStamp::from_metadata(&snapshot.metadata)?;
        let change = snapshot.change_stamp;
        let path = normalize_local_path(&snapshot.path);
        Ok(Self {
            root_id: root.root_id.clone(),
            root_epoch: root.grant_epoch,
            root_anchor: root.anchor.clone(),
            path,
            identity: file.identity().clone(),
            stamp,
            change,
        })
    }
    fn digest(&self) -> [u8; 32] {
        Sha256::digest(format!("{self:?}").as_bytes()).into()
    }
    fn owned_bytes(&self) -> usize {
        std::mem::size_of::<Self>()
            + self.root_id.capacity()
            + self.path.capacity() * 2
            + [&self.root_anchor, &self.identity]
                .iter()
                .map(|identity| {
                    identity.kind.capacity()
                        + identity.volume.capacity()
                        + identity.file_id.capacity()
                })
                .sum::<usize>()
    }
}
struct CachedPreparedMetadata {
    key: PreparedMetadataKey,
    item: LibraryPreparedAsset,
    bytes: usize,
}
#[derive(Default)]
struct PreparedMetadataCache {
    entries: BTreeMap<[u8; 32], CachedPreparedMetadata>,
    order: VecDeque<[u8; 32]>,
    bytes: usize,
    hits: u64,
    misses: u64,
}
impl PreparedMetadataCache {
    fn get(&mut self, key: &PreparedMetadataKey) -> Option<LibraryPreparedAsset> {
        let digest = key.digest();
        if let Some(entry) = self.entries.get(&digest).filter(|entry| entry.key == *key) {
            self.hits = self.hits.saturating_add(1);
            Some(entry.item.clone())
        } else {
            self.misses = self.misses.saturating_add(1);
            None
        }
    }
    fn insert(&mut self, key: PreparedMetadataKey, item: LibraryPreparedAsset) -> Result<()> {
        let bytes = key
            .owned_bytes()
            .checked_add(serde_json::to_vec(&item)?.len().saturating_mul(2) + 2048)
            .ok_or_else(|| LibraryError::budget("Verified metadata cache size overflow"))?;
        if bytes > METADATA_CACHE_BYTES {
            return Ok(());
        }
        let digest = key.digest();
        self.remove(&digest);
        while self.entries.len() >= METADATA_CACHE_ITEMS
            || self.bytes.saturating_add(bytes) > METADATA_CACHE_BYTES
        {
            let Some(old) = self.order.pop_front() else {
                break;
            };
            if let Some(old) = self.entries.remove(&old) {
                self.bytes -= old.bytes;
            }
        }
        // Account for metadata before admitting its entry to the shared cache.
        ensure!(
            self.bytes.saturating_add(bytes) <= METADATA_CACHE_BYTES,
            LibraryError::budget("Verified metadata cache admission failed")
        );
        self.bytes += bytes;
        self.order.push_back(digest);
        self.entries
            .insert(digest, CachedPreparedMetadata { key, item, bytes });
        Ok(())
    }
    fn remove(&mut self, digest: &[u8; 32]) {
        if let Some(old) = self.entries.remove(digest) {
            self.bytes -= old.bytes;
        }
        self.order.retain(|entry| entry != digest);
    }
    fn retire_changed_root(&mut self, root: &LibraryRoot) {
        let expired = self
            .entries
            .iter()
            .filter(|(_, entry)| {
                entry.key.root_id == root.root_id
                    && (entry.key.root_epoch != root.grant_epoch
                        || entry.key.root_anchor != root.anchor)
            })
            .map(|(key, _)| *key)
            .collect::<Vec<_>>();
        for key in expired {
            self.remove(&key);
        }
    }
    fn clear(&mut self) {
        self.entries.clear();
        self.order.clear();
        self.bytes = 0;
    }
}
fn prepare_authorized_metadata(
    catalog: &Catalog,
    cache: &Mutex<PreparedMetadataCache>,
    root: &LibraryRoot,
    file: &AuthorizedFile,
    context: &ScanContext,
) -> Result<PreparedLocalImport> {
    context.check()?;
    let before = PreparedMetadataKey::capture(root, file)?;
    let cached = {
        let mut cache = cache.lock().unwrap_or_else(|error| error.into_inner());
        cache.get(&before)
    };
    let reused = cached.is_some();
    let item = if let Some(cached) = cached {
        catalog.prepare_authorized_from_verified(file.try_clone()?, &cached)?
    } else {
        catalog.prepare_authorized(file.try_clone()?)?
    };
    context.check()?;
    ensure!(
        PreparedMetadataKey::capture(root, file)? == before,
        LibraryError::conflict("File changed during verified metadata preparation")
    );
    if !reused && item.tags_verified() && item.warnings().is_empty() {
        let dto = item.library_asset();
        if normalize_local_path(&dto.asset.path) == before.path
            && dto.file_identity.as_ref() == Some(&before.identity)
            && dto.asset.size_bytes == before.stamp.size
        {
            cache
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .insert(before, dto)?;
        }
    }
    Ok(item)
}
impl ContentStamp {
    fn read(file: &AuthorizedFile) -> Result<Self> {
        let metadata = file.metadata()?;
        Self::from_metadata(&metadata)
    }
    fn from_metadata(metadata: &std::fs::Metadata) -> Result<Self> {
        #[cfg(target_os = "linux")]
        let changed = {
            use std::os::unix::fs::MetadataExt;
            (metadata.ctime(), metadata.ctime_nsec())
        };
        Ok(Self {
            size: metadata.len(),
            modified: metadata.modified()?,
            #[cfg(target_os = "linux")]
            changed,
        })
    }
    fn check(&self, file: &AuthorizedFile) -> Result<()> {
        ensure!(
            Self::read(file)? == *self,
            LibraryError::conflict("Content evidence representation changed")
        );
        file.check()
    }
}
struct ContentHashBudget {
    remaining: u64,
    deadline: Option<Instant>,
}
impl Default for ContentHashBudget {
    fn default() -> Self {
        Self {
            remaining: CONTENT_HASH_BYTES,
            deadline: None,
        }
    }
}
fn capture_content_evidence(
    file: &mut AuthorizedFile,
    context: &ScanContext,
    closing: &AtomicBool,
    hashing: &AtomicBool,
    budget: &mut ContentHashBudget,
) -> Result<LibraryContentEvidence> {
    context.check()?;
    let stamp = ContentStamp::read(file)?;
    ensure!(
        stamp.size <= budget.remaining,
        LibraryError::budget("Library content evidence exceeds the 1 GiB job budget")
    );
    let deadline = *budget
        .deadline
        .get_or_insert_with(|| Instant::now() + CONTENT_HASH_TIMEOUT);
    let _lease = HashLease::acquire(hashing, context, closing, deadline)?;
    budget.remaining -= stamp.size;
    file.seek(SeekFrom::Start(0))?;
    let mut digest = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    let mut read_bytes = 0u64;
    while read_bytes < stamp.size {
        context.check()?;
        ensure!(
            !closing.load(Ordering::Acquire),
            LibraryError::new(LibraryErrorKind::Cancelled, "Library actor is closing")
        );
        ensure!(
            Instant::now() < deadline,
            LibraryError::budget("Library content evidence deadline exceeded")
        );
        let allowance = usize::try_from((stamp.size - read_bytes).min(buffer.len() as u64))?;
        let count = file.read(&mut buffer[..allowance])?;
        if count == 0 {
            break;
        }
        read_bytes = read_bytes
            .checked_add(count as u64)
            .ok_or_else(|| LibraryError::budget("Library content evidence size overflow"))?;
        ensure!(
            read_bytes <= stamp.size,
            LibraryError::conflict("File changed during content evidence capture")
        );
        digest.update(&buffer[..count]);
    }
    ensure!(
        read_bytes == stamp.size,
        LibraryError::conflict("File length changed during content evidence capture")
    );
    stamp.check(file)?;
    let modified_ms = stamp
        .modified
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|time| u64::try_from(time.as_millis()).ok());
    let evidence = LibraryContentEvidence {
        path: file.current_path()?,
        file_identity: file.identity().clone(),
        size_bytes: stamp.size,
        modified_ms,
        content_sha256: format!("{:x}", digest.finalize()),
        stamp,
        files: Weak::new(),
    };
    file.seek(SeekFrom::Start(0))?;
    Ok(evidence)
}
#[allow(clippy::too_many_arguments)]
fn prepare_refresh_root_path(
    catalog: &Catalog,
    registry: &AuthorityRegistry,
    hashing: &AtomicBool,
    metadata_cache: &Mutex<PreparedMetadataCache>,
    closing: &AtomicBool,
    path: &PathBuf,
    root: &LibraryRoot,
    watch: &LibraryWatchToken,
    expected: &FileIdentity,
    context: &ScanContext,
) -> Result<Box<LibraryRootPathPreparation>> {
    context.check()?;
    let bounded = ScanContext {
        cancel: context.cancel.clone(),
        deadline: context.deadline.min(Instant::now() + CONTENT_HASH_TIMEOUT),
    };
    validate_auto_watch(root, watch)?;
    let storage = LibraryBatchStorage {
        _reservation: Arc::new(PreparationReservation::new()?),
        _files: Arc::default(),
        _scope: Arc::default(),
    };
    let authority = registry.scan_authority(root, &bounded)?;
    metadata_cache
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .retire_changed_root(root);
    *storage
        ._scope
        .lock()
        .unwrap_or_else(|error| error.into_inner()) = Some(LibraryPrivateScope {
        _guard: authority.scan_guard(),
    });
    let mut opened = authority.open_file(path)?;
    ensure!(
        opened.identity() == expected,
        LibraryError::conflict(
            "Fresh root path no longer identifies the observed replacement object"
        )
    );
    let stamp = ContentStamp::read(&opened)?;
    let local = prepare_authorized_metadata(catalog, metadata_cache, root, &opened, &bounded)?;
    let prepared = local.library_asset();
    let mut content_evidence = match capture_content_evidence(
        &mut opened,
        &bounded,
        closing,
        hashing,
        &mut ContentHashBudget::default(),
    ) {
        Ok(evidence) => Some(evidence),
        Err(error) => {
            bounded.check()?;
            if !error
                .downcast_ref::<LibraryError>()
                .is_some_and(|error| error.kind == LibraryErrorKind::BudgetExceeded)
            {
                return Err(error);
            }
            None
        }
    };
    stamp.check(&opened)?;
    bounded.check()?;
    ensure!(
        local.owned_metadata_bytes()
            + serde_json::to_vec(&prepared)?.len()
            + content_evidence
                .as_ref()
                .map(serde_json::to_vec)
                .transpose()?
                .map_or(0, |bytes| bytes.len())
            <= MAX_LIBRARY_BATCH_BYTES,
        LibraryError::budget("Fresh root path metadata exceeds 1 MiB")
    );
    storage
        ._files
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .push(opened);
    if let Some(evidence) = content_evidence.as_mut() {
        evidence.files = Arc::downgrade(&storage._files);
        evidence.check()?;
    }
    Ok(Box::new(LibraryRootPathPreparation {
        local,
        prepared,
        content_evidence,
        stamp,
        _storage: storage,
    }))
}
#[allow(clippy::too_many_arguments)]
fn prepare_auto_relink(
    catalog: &Catalog,
    authority: &AuthorityRegistry,
    hashing: &AtomicBool,
    closing: &AtomicBool,
    path: &PathBuf,
    original: &TrackRecord,
    old_binding: &LibraryAssetBinding,
    target_authority: &LocalAssetAuthority,
    root: &LibraryRoot,
    watch_token: &LibraryWatchToken,
    context: &ScanContext,
) -> Result<LibraryAutoRelinkOutcome> {
    context.check()?;
    let bounded_context = ScanContext {
        cancel: context.cancel.clone(),
        deadline: context.deadline.min(Instant::now() + CONTENT_HASH_TIMEOUT),
    };
    let context = &bounded_context;
    validate_auto_watch(root, watch_token)?;
    let storage = LibraryBatchStorage {
        _reservation: Arc::new(PreparationReservation::new()?),
        _files: Arc::new(Mutex::new(Vec::new())),
        _scope: Arc::default(),
    };
    // This existing physical root is the only admission source. The notification
    // path and watch token cannot create a selected-file grant.
    let approved_root = authority.scan_authority(root, context)?;
    *storage
        ._scope
        .lock()
        .unwrap_or_else(|error| error.into_inner()) = Some(LibraryPrivateScope {
        _guard: approved_root.scan_guard(),
    });
    let opened = approved_root.open_file(path)?;
    let identity = opened.identity().clone();
    let selection = |message: String| {
        LibraryAutoRelinkOutcome::NeedsSelection(LibraryAutoRelinkSelection {
            asset_id: old_binding.asset_id.clone(),
            old_path: old_binding.path.clone(),
            new_path: path.clone(),
            new_identity: identity.clone(),
            expected_binding_revision: old_binding.binding_revision,
            reason: LibraryError::conflict(message.chars().take(1024).collect::<String>()),
        })
    };
    if old_binding.file_identity.as_ref() != Some(&identity) {
        return Ok(selection(
            "Automatic relocation requires the original physical file identity".into(),
        ));
    }
    let ticket = {
        #[cfg(windows)]
        {
            match authority.prepare_asset_relocation(target_authority, &identity, &opened) {
                Ok(ticket) => Ok(ticket),
                Err(error)
                    if matches!(
                        error.downcast_ref::<RelocationAdmissionError>(),
                        Some(
                            RelocationAdmissionError::OriginalLiveProofUnavailable
                                | RelocationAdmissionError::OriginalRepresentationChanged
                        )
                    ) =>
                {
                    match HashLease::acquire(
                        hashing,
                        context,
                        closing,
                        Instant::now() + CONTENT_HASH_TIMEOUT,
                    ) {
                        Ok(_lease) => authority.prepare_asset_relocation_from_binding(
                            old_binding,
                            target_authority,
                            &opened,
                        ),
                        Err(error) => Err(error),
                    }
                }
                Err(error) => Err(error),
            }
        }
        #[cfg(not(windows))]
        {
            if let Some(old_sha) = &old_binding.content_sha256 {
                match HashLease::acquire(
                    hashing,
                    context,
                    closing,
                    Instant::now() + CONTENT_HASH_TIMEOUT,
                ) {
                    Ok(_lease) => {
                        match authority.prepare_asset_relocation_with_content(
                            target_authority,
                            &identity,
                            &opened,
                            old_sha,
                            old_sha,
                        ) {
                            Ok(ticket) => Ok(ticket),
                            Err(error)
                                if matches!(
                                error.downcast_ref::<RelocationAdmissionError>(),
                                Some(RelocationAdmissionError::OriginalLiveProofUnavailable
                                    | RelocationAdmissionError::OriginalRepresentationChanged)
                            ) =>
                            {
                                authority.prepare_asset_relocation_from_binding(
                                    old_binding,
                                    target_authority,
                                    &opened,
                                )
                            }
                            Err(error) => Err(error),
                        }
                    }
                    Err(error) => Err(error),
                }
            } else {
                Err(LibraryError::conflict(
                    "Automatic relocation requires trusted original content evidence",
                )
                .into())
            }
        }
    };
    context.check()?;
    let ticket = match ticket {
        Ok(ticket) => ticket,
        Err(error) => return Ok(selection(format!("{error:#}"))),
    };
    ticket.check()?;
    let evidence = ticket.evidence();
    let mut local = catalog.prepare_authorized(opened.try_clone()?)?;
    let mut prepared = local.library_asset();
    prepared.asset.id.clone_from(&old_binding.asset_id);
    prepared.asset.track_id.clone_from(&original.id);
    prepared.track.id.clone_from(&original.id);
    prepared.track.lx_id.clone_from(&original.lx_id);
    prepared.track.local_asset_id = Some(old_binding.asset_id.clone());
    local.bind_library_identity(&prepared)?;
    if !local.tags_verified() {
        local.preserve_metadata_from(original)?;
        prepared.track = local.library_asset().track;
    }
    ensure!(
        local.owned_metadata_bytes() + serde_json::to_vec(&prepared)?.len()
            <= MAX_LIBRARY_BATCH_BYTES,
        LibraryError::budget("Automatic relocation prepared metadata exceeds 1 MiB")
    );
    ticket.check()?;
    context.check()?;
    storage
        ._files
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .push(opened);
    Ok(LibraryAutoRelinkOutcome::Prepared(Box::new(
        LibraryAutoRelinkPreparation {
            local,
            prepared,
            evidence,
            content_sha256: if evidence == RelinkEvidence::SameFileAndContent {
                old_binding.content_sha256.clone()
            } else {
                None
            },
            expected_binding_revision: old_binding.binding_revision,
            relocation: Arc::new(Mutex::new(Some(ticket))),
            _storage: storage,
        },
    )))
}
fn send_terminal(
    sender: &SyncSender<LibraryCompletion>,
    mut completion: LibraryCompletion,
    closing: &AtomicBool,
) -> Result<()> {
    loop {
        ensure!(!closing.load(Ordering::Acquire), "Library actor is closing");
        match sender.try_send(completion) {
            Ok(()) => return Ok(()),
            Err(TrySendError::Full(returned)) => completion = returned,
            Err(TrySendError::Disconnected(_)) => {
                anyhow::bail!("Library completion receiver closed")
            }
        }
        thread::sleep(Duration::from_millis(2));
    }
}
fn send_batch(
    sender: &SyncSender<LibraryCompletion>,
    mut completion: LibraryCompletion,
    receiver: Receiver<std::result::Result<(), String>>,
    context: &ScanContext,
    closing: &AtomicBool,
) -> Result<()> {
    loop {
        context.check()?;
        ensure!(!closing.load(Ordering::Acquire), "Library actor is closing");
        match sender.try_send(completion) {
            Ok(()) => break,
            Err(TrySendError::Full(returned)) => completion = returned,
            Err(TrySendError::Disconnected(_)) => {
                anyhow::bail!("Library completion receiver closed")
            }
        }
        thread::sleep(Duration::from_millis(2));
    }
    loop {
        context.check()?;
        ensure!(!closing.load(Ordering::Acquire), "Library actor is closing");
        match receiver.try_recv() {
            Ok(Ok(())) => return Ok(()),
            Ok(Err(message)) => anyhow::bail!(message),
            Err(TryRecvError::Disconnected) => {
                anyhow::bail!("Library batch acknowledgement closed")
            }
            Err(TryRecvError::Empty) => thread::sleep(Duration::from_millis(2)),
        }
    }
}
struct ScanExecution<'a> {
    sender: &'a SyncSender<LibraryCompletion>,
    token: &'a LibraryScanToken,
    catalog: &'a Catalog,
    context: &'a ScanContext,
    closing: &'a AtomicBool,
    hashing: &'a AtomicBool,
    content_evidence: bool,
    metadata_cache: &'a Mutex<PreparedMetadataCache>,
}
fn batch(
    execution: &ScanExecution<'_>,
    seen: Vec<LibrarySeenFile>,
    prepared: Vec<(PreparedLocalImport, LibraryPreparedAsset)>,
    mut content_evidence: Vec<LibraryContentEvidence>,
    progress: &LibraryOperationView,
    mut storage: LibraryBatchStorage,
    guards: Vec<AuthorizedFile>,
) -> Result<()> {
    // The delivered packet owns the proof through the controller's actual
    // commit, even if the worker's deadline expires while awaiting its ACK.
    storage._files = Arc::new(Mutex::new(
        guards
            .iter()
            .map(AuthorizedFile::try_clone)
            .collect::<Result<Vec<_>>>()?,
    ));
    let protected = storage._files.clone();
    for evidence in &mut content_evidence {
        evidence.files = Arc::downgrade(&protected);
        evidence.check()?;
    }
    let (ack, receiver) = mpsc::sync_channel(1);
    send_batch(
        execution.sender,
        LibraryCompletion::Batch {
            token: execution.token.clone(),
            seen,
            prepared,
            content_evidence,
            progress: progress.clone(),
            ack: LibraryAck {
                sender: ack,
                guards: protected,
                relocation: Arc::default(),
                scope: Arc::default(),
            },
            _storage: storage,
        },
        receiver,
        execution.context,
        execution.closing,
    )
}
fn prepare_chunk(
    files: &mut Vec<DiscoveredFile>,
    authority: &RootAuthority,
    outcome: &mut LibraryOperationView,
    execution: &ScanExecution<'_>,
    hash_budget: &mut ContentHashBudget,
    root: &LibraryRoot,
) -> Result<()> {
    if files.is_empty() {
        return Ok(());
    }
    let reservation = LibraryBatchStorage {
        _reservation: Arc::new(PreparationReservation::new()?),
        _files: Arc::new(Mutex::new(Vec::new())),
        _scope: Arc::default(),
    };
    let seen: Vec<_> = files
        .iter()
        .map(|file| LibrarySeenFile {
            path: file.path.clone(),
            asset_id: None,
        })
        .collect();
    outcome.phase = LibraryPhase::Discovering;
    batch(
        execution,
        seen,
        Vec::new(),
        Vec::new(),
        outcome,
        reservation.clone(),
        Vec::new(),
    )?;
    let mut prepared = Vec::with_capacity(files.len());
    let mut prepared_seen = Vec::with_capacity(files.len());
    let mut guards: Vec<AuthorizedFile> = Vec::with_capacity(files.len());
    let mut content_evidence = Vec::with_capacity(files.len());
    let mut bytes = 8usize;
    outcome.phase = LibraryPhase::Preparing;
    for file in files.iter() {
        execution.context.check()?;
        let result = (|| -> Result<(AuthorizedFile, PreparedLocalImport)> {
            let opened = authority.open_file(&file.path)?;
            let prepared = prepare_authorized_metadata(
                execution.catalog,
                execution.metadata_cache,
                root,
                &opened,
                execution.context,
            )?;
            opened.check()?;
            Ok((opened, prepared))
        })();
        match result {
            Ok((mut opened, item)) => {
                let content = if execution.content_evidence {
                    match capture_content_evidence(
                        &mut opened,
                        execution.context,
                        execution.closing,
                        execution.hashing,
                        hash_budget,
                    ) {
                        Ok(evidence) => Some(evidence),
                        Err(error) => {
                            execution.context.check()?;
                            add_warning(outcome, file.path.clone(), &error.to_string())?;
                            None
                        }
                    }
                } else {
                    None
                };
                let asset = item.library_asset();
                let seen = LibrarySeenFile {
                    path: asset.asset.path.clone(),
                    asset_id: Some(asset.asset.id.clone()),
                };
                let additional = serde_json::to_vec(&asset)?
                    .len()
                    .checked_add(
                        serde_json::to_vec(&seen)?.len()
                            + 2
                            + content
                                .as_ref()
                                .map(serde_json::to_vec)
                                .transpose()?
                                .map_or(0, |bytes| bytes.len() + 1),
                    )
                    .ok_or_else(|| LibraryError::budget("Library preparation size overflow"))?;
                ensure!(
                    additional + 8 <= MAX_LIBRARY_BATCH_BYTES,
                    LibraryError::budget("One library prepared item exceeds 1 MiB")
                );
                if bytes + additional > MAX_LIBRARY_BATCH_BYTES {
                    outcome.phase = LibraryPhase::Committing;
                    batch(
                        execution,
                        std::mem::take(&mut prepared_seen),
                        std::mem::take(&mut prepared),
                        std::mem::take(&mut content_evidence),
                        outcome,
                        reservation.clone(),
                        std::mem::take(&mut guards),
                    )?;
                    guards.clear();
                    bytes = 8;
                    outcome.phase = LibraryPhase::Preparing;
                }
                bytes += additional;
                outcome.prepared_files += 1;
                guards.push(opened);
                if let Some(evidence) = content {
                    content_evidence.push(evidence);
                }
                prepared_seen.push(seen);
                prepared.push((item, asset));
            }
            Err(error) => {
                execution.context.check()?;
                outcome.skipped_files += 1;
                add_warning(outcome, file.path.clone(), &error.to_string())?;
            }
        }
    }
    outcome.phase = LibraryPhase::Committing;
    if !prepared.is_empty() {
        batch(
            execution,
            prepared_seen,
            prepared,
            content_evidence,
            outcome,
            reservation.clone(),
            std::mem::take(&mut guards),
        )?;
    }
    drop(guards);
    files.clear();
    drop(reservation);
    Ok(())
}
fn add_warning(outcome: &mut LibraryOperationView, path: PathBuf, reason: &str) -> Result<()> {
    if outcome.warnings.len() >= MAX_LIBRARY_WARNINGS {
        outcome.omitted_warnings += 1;
        return Ok(());
    }
    let warning = LibraryWarning {
        path,
        reason: reason.chars().take(512).collect(),
    };
    let bytes = serde_json::to_vec(&outcome.warnings)?.len() + serde_json::to_vec(&warning)?.len();
    if bytes > MAX_LIBRARY_WARNING_BYTES {
        outcome.omitted_warnings += 1;
    } else {
        outcome.warnings.push(warning);
    }
    Ok(())
}
fn run_scan(
    execution: &ScanExecution<'_>,
    root: &LibraryRoot,
    options: &LibraryScanOptions,
    registry: &AuthorityRegistry,
) -> (LibraryOperationView, bool) {
    let token = execution.token;
    let context = execution.context;
    let mut outcome = LibraryOperationView::running(token);
    let result = (|| -> Result<ScanReport> {
        let authority = registry.scan_authority(root, context)?;
        execution
            .metadata_cache
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .retire_changed_root(root);
        let _scope = authority.scan_guard();
        let request = ScanRequest {
            root: root.path.clone(),
            recursive: root.recursive,
            max_files: options.max_files,
            max_entries: options.max_entries,
            max_depth: options.max_depth,
        };
        let mut files = Vec::with_capacity(MAX_LIBRARY_BATCH_ITEMS);
        let mut bytes = 2usize;
        let mut hash_budget = ContentHashBudget::default();
        let report = discover_authorized_stream(&authority, &request, context, |file| {
            let additional = serde_json::to_vec(&LibrarySeenFile {
                path: file.path.clone(),
                asset_id: None,
            })?
            .len()
                + 1;
            if files.len() == MAX_LIBRARY_BATCH_ITEMS
                || bytes + additional > MAX_LIBRARY_BATCH_BYTES
            {
                prepare_chunk(
                    &mut files,
                    &authority,
                    &mut outcome,
                    execution,
                    &mut hash_budget,
                    root,
                )?;
                bytes = 2;
            }
            ensure!(
                additional <= MAX_LIBRARY_BATCH_BYTES,
                LibraryError::budget("Library path batch exceeds 1 MiB")
            );
            bytes += additional;
            outcome.discovered_files += 1;
            files.push(file);
            Ok(())
        })?;
        prepare_chunk(
            &mut files,
            &authority,
            &mut outcome,
            execution,
            &mut hash_budget,
            root,
        )?;
        authority.verify()?;
        Ok(report)
    })();
    match result {
        Ok(report) => {
            outcome.entries_examined = report.entries_examined;
            outcome.discovered_files = report.discovered_files;
            for warning in report.warnings {
                let _ = add_warning(&mut outcome, warning.path, &warning.reason);
            }
            outcome.omitted_warnings += report.omitted_warnings;
            outcome.phase = LibraryPhase::Finalizing;
            outcome.state = if report.complete
                && outcome.skipped_files == 0
                && (!execution.content_evidence || outcome.warnings.is_empty())
            {
                LibraryOperationState::Completed
            } else {
                LibraryOperationState::Partial
            };
            (outcome, report.complete)
        }
        Err(error) => {
            let kind = if context.cancel.load(Ordering::Acquire)
                || error.downcast_ref::<ScanError>() == Some(&ScanError::Cancelled)
            {
                LibraryErrorKind::Cancelled
            } else if Instant::now() >= context.deadline
                || error.downcast_ref::<ScanError>() == Some(&ScanError::DeadlineExceeded)
            {
                LibraryErrorKind::Timeout
            } else if error.downcast_ref::<ScanError>() == Some(&ScanError::BudgetExceeded) {
                LibraryErrorKind::BudgetExceeded
            } else {
                error
                    .downcast_ref::<LibraryError>()
                    .map_or(LibraryErrorKind::Unavailable, |error| error.kind)
            };
            outcome.state = if kind == LibraryErrorKind::Cancelled {
                LibraryOperationState::Cancelled
            } else {
                LibraryOperationState::Failed
            };
            outcome.error = Some(LibraryError::new(
                kind,
                error.to_string().chars().take(2048).collect::<String>(),
            ));
            (outcome, false)
        }
    }
}

#[cfg(all(test, any(windows, target_os = "linux")))]
mod tests {
    use super::*;
    static CASES: Mutex<()> = Mutex::new(());
    #[test]
    fn occupied_profile_hash_slot_honors_deadline_without_releasing_another_hash() {
        let hashing = AtomicBool::new(true);
        let closing = AtomicBool::new(false);
        let context = ScanContext::new(Duration::from_millis(20));
        let started = Instant::now();
        assert!(
            HashLease::acquire(
                &hashing,
                &context,
                &closing,
                Instant::now() + Duration::from_secs(1)
            )
            .is_err()
        );
        assert!(started.elapsed() < Duration::from_millis(200));
        assert!(hashing.load(Ordering::Acquire));
        context.cancel.store(true, Ordering::Release);
        assert!(
            HashLease::acquire(
                &hashing,
                &context,
                &closing,
                Instant::now() + Duration::from_secs(1)
            )
            .is_err()
        );
        assert!(hashing.load(Ordering::Acquire));
        hashing.store(false, Ordering::Release);
        let context = ScanContext::new(Duration::from_secs(1));
        let lease = HashLease::acquire(
            &hashing,
            &context,
            &closing,
            Instant::now() + Duration::from_secs(1),
        )
        .unwrap();
        assert!(hashing.load(Ordering::Acquire));
        drop(lease);
        assert!(!hashing.load(Ordering::Acquire));
    }
    #[test]
    fn content_hash_job_consumes_actual_length_and_rejects_oversized_next_object_before_read() {
        let _case = CASES.lock().unwrap();
        let temp = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
        let path = temp.path().join("body.wav");
        std::fs::write(&path, b"abcdefghijkl").unwrap();
        let context = ScanContext::new(Duration::from_secs(2));
        let authority = RootAuthority::open(temp.path(), true, &context).unwrap();
        let mut opened = authority.open_file(&path).unwrap();
        let hashing = AtomicBool::new(false);
        let closing = AtomicBool::new(false);
        let mut budget = ContentHashBudget {
            remaining: 16,
            deadline: None,
        };
        let evidence =
            capture_content_evidence(&mut opened, &context, &closing, &hashing, &mut budget)
                .unwrap();
        assert_eq!(evidence.size_bytes, 12);
        assert_eq!(
            evidence.content_sha256,
            format!("{:x}", Sha256::digest(b"abcdefghijkl"))
        );
        assert_eq!(budget.remaining, 4);
        assert!(!hashing.load(Ordering::Acquire));
        let error =
            capture_content_evidence(&mut opened, &context, &closing, &hashing, &mut budget)
                .unwrap_err();
        assert_eq!(
            error.downcast_ref::<LibraryError>().unwrap().kind,
            LibraryErrorKind::BudgetExceeded
        );
        assert_eq!(budget.remaining, 4);
        assert!(!hashing.load(Ordering::Acquire));
        let barrier = authority.retirement_barrier();
        drop(opened);
        drop(authority);
        assert!(barrier.wait(Duration::from_secs(3)));
    }
    fn next(actor: &mut LibraryActor) -> LibraryCompletion {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(completion) = actor.try_recv() {
                return completion;
            }
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(2));
        }
    }
    #[test]
    fn validation_panic_reports_terminal_failure_and_the_owned_worker_remains_usable() {
        let _case = CASES.lock().unwrap();
        let temp = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
        let path = temp.path().join("selected");
        std::fs::create_dir(&path).unwrap();
        let mut actor = LibraryActor::new(Catalog::new().unwrap()).unwrap();
        actor.hooks.panic_validation.store(true, Ordering::Release);
        actor
            .submit_validate(
                "validation:panic".into(),
                path.clone(),
                true,
                ScanContext::new(Duration::from_secs(10)),
            )
            .unwrap();
        let LibraryCompletion::RootValidated {
            operation_id,
            result,
            ..
        } = next(&mut actor)
        else {
            panic!("Missing failed validation terminal");
        };
        assert_eq!(operation_id, "validation:panic");
        assert!(result.unwrap_err().to_string().contains("panicked"));
        assert_eq!(actor.active_jobs(), 0);
        assert!(
            actor
                .authority
                .retirement_barrier()
                .wait(Duration::from_secs(3))
        );
        std::fs::rename(&path, temp.path().join("after-panic")).unwrap();
        actor
            .submit_validate(
                "validation:healthy".into(),
                temp.path().join("after-panic"),
                true,
                ScanContext::new(Duration::from_secs(10)),
            )
            .unwrap();
        let LibraryCompletion::RootValidated { result, .. } = next(&mut actor) else {
            panic!("Missing next validation terminal");
        };
        assert!(result.is_ok());
        assert!(actor.shutdown(Duration::from_secs(3)).unwrap());
    }
    #[test]
    fn shutdown_waits_for_all_registered_workers_before_reporting_a_join_panic() {
        let _case = CASES.lock().unwrap();
        let temp = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
        let path = temp.path().join("held.wav");
        std::fs::write(&path, b"owned").unwrap();
        let authority = RootAuthority::open(
            temp.path(),
            true,
            &ScanContext::new(Duration::from_secs(10)),
        )
        .unwrap();
        let opened = authority.open_file(&path).unwrap();
        let mut actor = LibraryActor::new(Catalog::new().unwrap()).unwrap();
        assert!(actor.shutdown(Duration::from_secs(3)).unwrap());
        let failed = WorkerPermit::reserve(WorkerKind::Library)
            .unwrap()
            .attach(thread::spawn(|| panic!("injected owner failure")));
        assert!(failed.wait(Duration::from_secs(3)));
        assert!(failed.panicked());
        let (entered, observed) = mpsc::sync_channel(1);
        let (release, wait) = mpsc::sync_channel(1);
        let blocked = WorkerPermit::reserve(WorkerKind::Library)
            .unwrap()
            .attach(thread::spawn(move || {
                let mut opened = opened;
                entered.send(()).unwrap();
                wait.recv().unwrap();
                use std::io::Read;
                let mut actual = [0; 5];
                opened.read_exact(&mut actual).unwrap();
                assert_eq!(&actual, b"owned");
            }));
        observed.recv_timeout(Duration::from_secs(3)).unwrap();
        actor.workers.extend([failed, blocked]);
        assert!(!actor.shutdown(Duration::from_millis(5)).unwrap());
        assert_eq!(actor.workers.len(), 1);
        #[cfg(windows)]
        assert!(std::fs::OpenOptions::new().write(true).open(&path).is_err());
        release.send(()).unwrap();
        assert!(actor.shutdown(Duration::from_secs(3)).is_err());
        assert!(actor.workers.is_empty());
        assert!(std::fs::OpenOptions::new().write(true).open(&path).is_ok());
    }
}

//! Sanitization applies to a private snapshot, never the live profile database.
#[cfg(test)]
use super::APPLICATION_ID;
use super::{
    Repository, SCHEMA_VERSION, schema,
    snapshot_native::{
        self, ActualFile, ActualFileMetadata, ColdCleanup, DurableCleanupFact, DurableManifest,
        RecoveryProbe, SnapshotContext, SnapshotDirectory, SnapshotLimiter, SnapshotPermit,
        SnapshotPhase, SourceFileProof,
    },
    snapshot_vfs::{OwnedSqlite, SourceAnchors, SourceSqlite},
};
use anyhow::{Context, Result, ensure};
use rusqlite::{
    Connection,
    backup::{Backup, StepResult},
    ffi,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    ptr,
    sync::{Arc, Mutex},
    time::Duration,
};

#[cfg(test)]
pub(super) fn sanitize_sync_snapshot(snapshot: &mut Connection) -> Result<()> {
    schema::validate_current(snapshot)?;
    // Clear both live cells and deleted-cell remnants before publication.
    snapshot.pragma_update(None, "secure_delete", true)?;
    snapshot.pragma_update(None, "journal_mode", "DELETE")?;
    super::snapshot_sanitize::purge_snapshot_authority(snapshot, &|| Ok(()))?;
    snapshot.execute_batch("VACUUM")?;
    schema::integrity_check(snapshot)?;
    let version: i64 = snapshot.pragma_query_value(None, "user_version", |row| row.get(0))?;
    let application: i32 = snapshot.pragma_query_value(None, "application_id", |row| row.get(0))?;
    ensure!(
        version == SCHEMA_VERSION as i64 && application == APPLICATION_ID,
        "Snapshot identity changed during sanitization"
    );
    Ok(())
}

pub const MAX_PHYSICAL_SNAPSHOT_BYTES: u64 = snapshot_native::MAX_SNAPSHOT_BYTES;
fn retain_on_panic<T>(operation: impl FnOnce() -> Result<T>) -> Result<T> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(operation)).unwrap_or_else(|_| {
        Err(anyhow::anyhow!(
            "Snapshot coordinator panicked; actual owners remain retained"
        ))
    })
}

struct SqliteProgress {
    database: *mut ffi::sqlite3,
    context: Box<SnapshotContext>,
}
impl SqliteProgress {
    fn install(connection: &Connection, context: SnapshotContext) -> Self {
        let mut guard = Self {
            database: unsafe { connection.handle() },
            context: Box::new(context),
        };
        unsafe {
            ffi::sqlite3_progress_handler(
                guard.database,
                1000,
                Some(snapshot_progress),
                (&mut *guard.context as *mut SnapshotContext).cast(),
            );
        }
        guard
    }
}
impl Drop for SqliteProgress {
    fn drop(&mut self) {
        unsafe { ffi::sqlite3_progress_handler(self.database, 0, None, ptr::null_mut()) };
    }
}
unsafe extern "C" fn snapshot_progress(context: *mut std::ffi::c_void) -> std::ffi::c_int {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        if context.is_null() {
            return 1;
        }
        std::ffi::c_int::from(
            unsafe { &*context.cast::<SnapshotContext>() }
                .check()
                .is_err(),
        )
    }))
    .unwrap_or(1)
}
/// Actual selected-directory write authority; uploaded metadata cannot create it.
pub struct PhysicalSnapshotSelection {
    directory: SnapshotDirectory,
}
impl PhysicalSnapshotSelection {
    pub fn open_selected(path: impl AsRef<Path>, epoch: u64) -> Result<Self> {
        Ok(Self {
            directory: SnapshotDirectory::open_selected(path, epoch)?,
        })
    }
    pub fn path(&self) -> &Path {
        self.directory.path()
    }
    pub fn revoke(&self) {
        self.directory.revoke();
    }
}
#[derive(Clone)]
pub struct PhysicalSnapshotContext {
    inner: SnapshotContext,
}
impl PhysicalSnapshotContext {
    pub fn new(timeout: Duration) -> Result<Self> {
        Ok(Self {
            inner: SnapshotContext::new(timeout)?,
        })
    }
    pub fn cancel(&self) {
        self.inner.cancel();
    }
}
#[derive(Clone)]
pub struct PhysicalSnapshotLimits {
    inner: SnapshotLimiter,
    retained: Arc<Mutex<BTreeMap<String, RetainedReservation>>>,
}
struct RetainedReservation {
    digest: String,
    profile_root: snapshot_native::FileIdentity,
    permit: SnapshotPermit,
}
impl PhysicalSnapshotLimits {
    pub fn new(aggregate_bytes: u64) -> Result<Self> {
        Ok(Self {
            inner: SnapshotLimiter::new(aggregate_bytes)?,
            retained: Arc::new(Mutex::new(BTreeMap::new())),
        })
    }
    pub fn stats(&self) -> (usize, u64) {
        let stats = self.inner.stats();
        (stats.active, stats.reserved_bytes)
    }
    fn retain(
        &self,
        request: &PhysicalSnapshotRequest,
        private: &SnapshotDirectory,
        recovering: bool,
    ) -> Result<SnapshotPermit> {
        let digest = format!("{:x}", Sha256::digest(serde_json::to_vec(request)?));
        let mut retained = self.retained.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(existing) = retained.get(&request.operation_id) {
            ensure!(
                recovering
                    && existing.digest == digest
                    && existing.profile_root == *private.identity(),
                "Snapshot operation is already allocated or changed its scope"
            );
            return Ok(existing.permit.clone());
        }
        ensure!(
            retained.len() < 4,
            "Physical snapshot operation history is full of pending owners"
        );
        let workspace = request
            .maximum_image_bytes
            .checked_mul(3)
            .and_then(|v| v.checked_add(65 * 1024 * 1024))
            .context("Snapshot reservation overflow")?;
        let permit = self
            .inner
            .reserve_job(request.maximum_image_bytes, workspace)?;
        retained.insert(
            request.operation_id.clone(),
            RetainedReservation {
                digest,
                profile_root: private.identity().clone(),
                permit: permit.clone(),
            },
        );
        Ok(permit)
    }
    fn settle(&self, operation: &str) {
        self.retained
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(operation);
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PhysicalSnapshotRequest {
    pub operation_id: String,
    pub expected_revision: u64,
    pub maximum_image_bytes: u64,
    pub basename: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PhysicalSnapshotReceipt {
    pub operation_id: String,
    pub path: PathBuf,
    pub bytes: u64,
    pub sha256: String,
    pub file_device: u64,
    pub file_object: String,
    pub root_device: u64,
    pub root_object: String,
    pub snapshot_revision: u64,
    pub schema_version: u32,
    pub selection_epoch: u64,
}
/// Recovery facts locate an object; they confer no write, publish or cleanup
/// permission. The application must persist this locator before body creation.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PhysicalSnapshotRecoveryLocator {
    pub operation_id: String,
    pub request_digest: String,
    request: PhysicalSnapshotRequest,
    manifest: ActualFileMetadata,
    selected_identity: snapshot_native::FileIdentity,
}
pub struct PhysicalSnapshotRecoveryPreview {
    pub operation_id: String,
    pub request_digest: String,
    pub snapshot_revision: u64,
    pub phase: String,
    pub artifact_count: usize,
    pub recorded_receipt: Option<PhysicalSnapshotReceipt>,
    locator: PhysicalSnapshotRecoveryLocator,
    private: SnapshotDirectory,
    selected: SnapshotDirectory,
    manifest_probe: Option<RecoveryProbe>,
    facts: Vec<snapshot_native::PhaseFact>,
    cleanup: Vec<PhysicalSnapshotCleanupFact>,
    trusted_publication: Option<PhysicalSnapshotReceipt>,
    sealed_receipt: Option<PhysicalSnapshotReceipt>,
}
impl std::fmt::Debug for PhysicalSnapshotRecoveryPreview {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PhysicalSnapshotRecoveryPreview")
            .field("operation_id", &self.operation_id)
            .field("phase", &self.phase)
            .field("artifact_count", &self.artifact_count)
            .finish()
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PhysicalSnapshotRecoveryAction {
    CleanupPrivate,
    ResumePublication,
}
fn recovery_inventory(
    facts: &[snapshot_native::PhaseFact],
) -> Result<BTreeMap<String, (String, ActualFileMetadata)>> {
    let mut inventory: BTreeMap<String, (String, ActualFileMetadata)> = BTreeMap::new();
    for fact in facts.iter().filter(|f| f.checkpoint) {
        let value: serde_json::Value = serde_json::from_slice(&fact.body)?;
        ensure!(
            value["kind"].as_str() == Some("allocation"),
            "Unknown snapshot inventory checkpoint"
        );
        let role = value["role"]
            .as_str()
            .context("Snapshot inventory role missing")?;
        ensure!(
            matches!(role, "main" | "auxiliary" | "candidate"),
            "Unknown snapshot object role"
        );
        let file: ActualFileMetadata = serde_json::from_value(value["file"].clone())?;
        if let Some((old_role, old)) = inventory.get(&file.nonce) {
            let old: &ActualFileMetadata = old;
            ensure!(
                old_role == role
                    && old.directory_identity == file.directory_identity
                    && old.operation_id == file.operation_id
                    && old.temporary_name == file.temporary_name
                    && (old.identity.is_none() || old.identity == file.identity),
                "Allocation checkpoint changed object identity"
            );
        }
        inventory.insert(file.nonce.clone(), (role.to_owned(), file));
        ensure!(
            inventory.len() <= 15,
            "Snapshot inventory exceeds its actual file bound"
        );
    }
    Ok(inventory)
}
fn validate_recovery_receipt(
    receipt: &PhysicalSnapshotReceipt,
    locator: &PhysicalSnapshotRecoveryLocator,
    selected: &SnapshotDirectory,
) -> Result<()> {
    ensure!(
        receipt.operation_id == locator.operation_id
            && receipt.schema_version == SCHEMA_VERSION
            && receipt.snapshot_revision == locator.request.expected_revision
            && receipt.bytes <= locator.request.maximum_image_bytes
            && receipt.sha256.len() == 64
            && receipt.sha256.bytes().all(|b| b.is_ascii_hexdigit())
            && receipt.root_device == selected.identity().device
            && receipt.root_object == selected.identity().object
            && crate::domain::music::normalize_local_path(&receipt.path)
                == crate::domain::music::normalize_local_path(
                    &selected.path().join(&locator.request.basename)
                ),
        "Recovery publication receipt is outside its sealed operation"
    );
    Ok(())
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PhysicalSnapshotCleanupFact {
    native: DurableCleanupFact,
}
impl PhysicalSnapshotCleanupFact {
    pub fn nonce(&self) -> &str {
        &self.native.file.nonce
    }
    pub fn removed(&self) -> bool {
        self.native.removed
    }
}
/// Application-owned durable operation storage. Each cleanup nonce replaces its
/// previous checkpoint monotonically; at most 16 latest facts and 1 MiB are
/// permitted. Finished operations are removed from pending recovery, rather
/// than accumulating unbounded private manifests or retained reservations.
pub trait PhysicalSnapshotRecoveryJournal {
    fn record_cleanup(
        &self,
        locator: &PhysicalSnapshotRecoveryLocator,
        fact: &PhysicalSnapshotCleanupFact,
    ) -> Result<()>;
    fn finished(&self, locator: &PhysicalSnapshotRecoveryLocator) -> Result<()>;
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PhysicalSnapshotRecoveryRecord {
    pub locator: PhysicalSnapshotRecoveryLocator,
    pub publication: Option<PhysicalSnapshotReceipt>,
    pub cleanup: Vec<PhysicalSnapshotCleanupFact>,
}
pub struct PhysicalSnapshotFailure {
    pub kind: PhysicalSnapshotFailureKind,
    pub message: String,
    pub owner: Option<Box<PreparedPhysicalSnapshot>>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PhysicalSnapshotFailureKind {
    Cancelled,
    Deadline,
    Quota,
    Conflict,
    Unsupported,
    Changed,
    Io,
    PublishedUnconfirmed,
}
impl From<snapshot_native::NativeErrorKind> for PhysicalSnapshotFailureKind {
    fn from(kind: snapshot_native::NativeErrorKind) -> Self {
        use snapshot_native::NativeErrorKind as K;
        match kind {
            K::Cancelled => Self::Cancelled,
            K::Deadline => Self::Deadline,
            K::Quota => Self::Quota,
            K::Conflict => Self::Conflict,
            K::Unsupported => Self::Unsupported,
            K::Changed => Self::Changed,
            K::Io => Self::Io,
            K::PublishedUnconfirmed => Self::PublishedUnconfirmed,
        }
    }
}
impl std::fmt::Debug for PhysicalSnapshotFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PhysicalSnapshotFailure")
            .field("message", &self.message)
            .field("has_owner", &self.owner.is_some())
            .finish()
    }
}
impl std::fmt::Display for PhysicalSnapshotFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}
impl std::error::Error for PhysicalSnapshotFailure {}
pub struct PreparedPhysicalSnapshot {
    request: PhysicalSnapshotRequest,
    revision: u64,
    selected: SnapshotDirectory,
    private: SnapshotDirectory,
    permit: SnapshotPermit,
    manifest: Option<Arc<Mutex<DurableManifest>>>,
    main: Option<ActualFile>,
    candidate: Option<ActualFile>,
    auxiliary: Vec<ActualFile>,
    snapshot_hash: Option<String>,
    published: Option<PhysicalSnapshotReceipt>,
    retained_registry: Option<std::sync::Arc<std::sync::Mutex<super::snapshot_vfs::OwnedFiles>>>,
    recovery_locator: Option<PhysicalSnapshotRecoveryLocator>,
    cleanup_only: bool,
    cold_cleanup: Vec<ColdCleanup>,
    acknowledged: bool,
    unresolved: Vec<ActualFileMetadata>,
    limits: PhysicalSnapshotLimits,
    recovery_blocked: bool,
    recovery_registered: bool,
}
impl std::fmt::Debug for PreparedPhysicalSnapshot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PreparedPhysicalSnapshot")
            .field("operation_id", &self.request.operation_id)
            .field("snapshot_revision", &self.revision)
            .field("has_main", &self.main.is_some())
            .field("has_candidate", &self.candidate.is_some())
            .field("published", &self.published.is_some())
            .finish()
    }
}
pub struct PublishedPhysicalSnapshot {
    owner: PreparedPhysicalSnapshot,
    receipt: PhysicalSnapshotReceipt,
}
impl std::fmt::Debug for PublishedPhysicalSnapshot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PublishedPhysicalSnapshot")
            .field("receipt", &self.receipt)
            .finish()
    }
}
impl PublishedPhysicalSnapshot {
    pub fn receipt(&self) -> &PhysicalSnapshotReceipt {
        &self.receipt
    }
    /// The trusted caller must persist the exact publication receipt before
    /// acknowledging. The callback is an application transaction, not JSON.
    pub fn acknowledge(
        mut self,
        context: &PhysicalSnapshotContext,
        commit: &dyn Fn(&PhysicalSnapshotReceipt) -> Result<()>,
        journal: &dyn PhysicalSnapshotRecoveryJournal,
    ) -> std::result::Result<PhysicalSnapshotReceipt, PhysicalSnapshotFailure> {
        let result = retain_on_panic(|| -> Result<PhysicalSnapshotReceipt> {
            context.inner.check()?;
            let verified = self
                .owner
                .candidate
                .as_mut()
                .context("Published snapshot descriptor is missing")?
                .verify_published(&context.inner)?;
            ensure!(
                verified.sha256 == self.receipt.sha256
                    && verified.bytes == self.receipt.bytes
                    && verified.identity.device == self.receipt.file_device
                    && verified.identity.object == self.receipt.file_object
                    && verified.directory_identity.device == self.receipt.root_device
                    && verified.directory_identity.object == self.receipt.root_object
                    && crate::domain::music::normalize_local_path(&verified.path)
                        == crate::domain::music::normalize_local_path(&self.receipt.path),
                "Published snapshot changed before acknowledgement"
            );
            commit(&self.receipt)?;
            if !self.owner.acknowledged {
                self.owner.fact(
                    SnapshotPhase::Acknowledged,
                    &serde_json::to_vec(&self.receipt)?,
                    &context.inner,
                )?;
                self.owner.acknowledged = true;
            }
            self.owner.cleanup_files(context, journal)?;
            self.owner.cleanup_manifest(&context.inner, journal)?;
            journal.finished(
                self.owner
                    .recovery_locator
                    .as_ref()
                    .context("Snapshot recovery locator missing")?,
            )?;
            self.owner.limits.settle(&self.owner.request.operation_id);
            Ok(self.receipt.clone())
        });
        match result {
            Ok(receipt) => Ok(receipt),
            Err(error) => Err(self.owner.failure(error)),
        }
    }
}
impl PreparedPhysicalSnapshot {
    pub fn ensure_recovery_registered(
        &mut self,
        register: &dyn Fn(&PhysicalSnapshotRecoveryLocator) -> Result<()>,
    ) -> Result<()> {
        if self.recovery_registered {
            return Ok(());
        }
        let locator = self
            .recovery_locator
            .as_ref()
            .context("Snapshot has no actual recovery locator")?;
        retain_on_panic(|| register(locator))?;
        self.recovery_registered = true;
        Ok(())
    }
    /// Closes this settled worker's descriptors for an explicitly scheduled
    /// cold retry. The shared limits continue charging the pending operation;
    /// closing an owner does not acknowledge deletion or free disk accounting.
    pub fn release_handles_for_recovery(
        self,
    ) -> std::result::Result<PhysicalSnapshotRecoveryLocator, PhysicalSnapshotFailure> {
        if !self.recovery_registered {
            return Err(self.failure(anyhow::anyhow!(
                "Snapshot recovery locator is not durably registered"
            )));
        }
        let Some(locator) = self.recovery_locator.clone() else {
            return Err(self.failure(anyhow::anyhow!(
                "Unregistered snapshot retains its live handles"
            )));
        };
        let can_release = self
            .main
            .as_ref()
            .is_none_or(ActualFile::can_release_for_recovery)
            && self
                .candidate
                .as_ref()
                .is_none_or(ActualFile::can_release_for_recovery)
            && self
                .auxiliary
                .iter()
                .all(ActualFile::can_release_for_recovery)
            && self
                .cold_cleanup
                .iter()
                .all(ColdCleanup::can_release_for_recovery)
            && self.manifest.as_ref().is_none_or(|m| {
                m.lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .can_release_for_recovery()
            });
        let can_release = can_release
            && self.retained_registry.as_ref().is_none_or(|registry| {
                registry
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .can_release_for_recovery()
            });
        if !can_release {
            return Err(self.failure(anyhow::anyhow!("Deletion has begun without a durable disposition checkpoint; retain and retry its live owner")));
        }
        Ok(locator)
    }
    fn refresh_selection(
        &mut self,
        fresh: &PhysicalSnapshotSelection,
        check: &dyn Fn(&PhysicalSnapshotRecoveryLocator) -> Result<()>,
    ) -> Result<()> {
        fresh.directory.verify()?;
        ensure!(
            fresh.directory.identity() == self.selected.identity(),
            "Fresh selection identifies another actual root"
        );
        check(
            self.recovery_locator
                .as_ref()
                .context("Snapshot locator missing")?,
        )?;
        if let Some(file) = self.candidate.as_mut() {
            file.rebind_cleanup_directory(&fresh.directory)?;
        }
        self.selected = fresh.directory.clone();
        // A cold deletion owner carries its own fresh scope; do not bypass a
        // revoked grant merely because its serialized path still exists.
        for cold in &mut self.cold_cleanup {
            // Private profile scopes are not selected-directory scopes. A
            // mismatching root leaves that owner on its original private pins.
            if cold.rebind(&self.selected).is_err() {
                cold.rebind(&self.private)?;
            }
        }
        Ok(())
    }
    pub fn cleanup_in(
        mut self,
        fresh: &PhysicalSnapshotSelection,
        context: &PhysicalSnapshotContext,
        journal: &dyn PhysicalSnapshotRecoveryJournal,
        check: &dyn Fn(&PhysicalSnapshotRecoveryLocator) -> Result<()>,
    ) -> std::result::Result<(), PhysicalSnapshotFailure> {
        if let Err(error) = retain_on_panic(|| self.refresh_selection(fresh, check)) {
            return Err(self.failure(error));
        }
        self.cleanup(context, journal)
    }
    pub fn publish_in(
        mut self,
        fresh: &PhysicalSnapshotSelection,
        context: &PhysicalSnapshotContext,
        journal: &dyn PhysicalSnapshotRecoveryJournal,
        check: &dyn Fn(&PhysicalSnapshotRecoveryLocator) -> Result<()>,
    ) -> std::result::Result<PublishedPhysicalSnapshot, PhysicalSnapshotFailure> {
        if let Err(error) = retain_on_panic(|| self.refresh_selection(fresh, check)) {
            return Err(self.failure(error));
        }
        self.publish(context, journal)
    }
    pub fn recovery_locator(&self) -> Option<&PhysicalSnapshotRecoveryLocator> {
        self.recovery_locator.as_ref()
    }
    fn fact(&self, phase: SnapshotPhase, bytes: &[u8], context: &SnapshotContext) -> Result<()> {
        self.manifest
            .as_ref()
            .context("Snapshot manifest missing")?
            .lock()
            .map_err(|_| anyhow::anyhow!("Snapshot manifest poisoned"))?
            .append_once(phase, bytes, context)
    }
    fn inventory(
        &self,
        role: &str,
        metadata: &ActualFileMetadata,
        context: &SnapshotContext,
    ) -> Result<()> {
        self.manifest
            .as_ref()
            .context("Snapshot manifest missing")?
            .lock()
            .map_err(|_| anyhow::anyhow!("Snapshot manifest poisoned"))?
            .append_checkpoint(
                &serde_json::to_vec(
                    &serde_json::json!({"kind":"allocation","role":role,"file":metadata}),
                )?,
                context,
            )
    }
    fn cleanup_manifest(
        &mut self,
        context: &SnapshotContext,
        journal: &dyn PhysicalSnapshotRecoveryJournal,
    ) -> Result<()> {
        let Some(manifest) = self.manifest.take() else {
            return Ok(());
        };
        let manifest = match Arc::try_unwrap(manifest) {
            Ok(mutex) => mutex.into_inner().unwrap_or_else(|e| e.into_inner()),
            Err(manifest) => {
                self.manifest = Some(manifest);
                anyhow::bail!("SQLite still retains the snapshot manifest");
            }
        };
        let locator = self
            .recovery_locator
            .as_ref()
            .context("Snapshot recovery locator missing")?;
        if let Err(mut failure) = manifest.cleanup(context, &|native| {
            journal.record_cleanup(
                locator,
                &PhysicalSnapshotCleanupFact {
                    native: native.clone(),
                },
            )
        }) {
            if let Some(manifest) = failure.manifest.take() {
                self.manifest = Some(Arc::new(Mutex::new(*manifest)));
            }
            if let Some(file) = failure.file.take() {
                self.auxiliary.push(*file);
            }
            anyhow::bail!("{}", failure.message);
        }
        Ok(())
    }
    fn cleanup_files(
        &mut self,
        context: &PhysicalSnapshotContext,
        journal: &dyn PhysicalSnapshotRecoveryJournal,
    ) -> Result<()> {
        ensure!(
            self.unresolved.is_empty() && !self.recovery_blocked,
            "Recovery contains unresolved actual objects; cleanup cannot release their accounting"
        );
        self.collect_registry()?;
        if let Some(file) = self.main.take() {
            self.auxiliary.push(file);
        }
        self.cleanup_auxiliary(context, journal)
    }
    fn cleanup_auxiliary(
        &mut self,
        context: &PhysicalSnapshotContext,
        journal: &dyn PhysicalSnapshotRecoveryJournal,
    ) -> Result<()> {
        let locator = self.recovery_locator.clone();
        while let Some(cold) = self.cold_cleanup.last_mut() {
            let native = cold.confirm(&context.inner)?;
            journal.record_cleanup(
                locator.as_ref().context("Cold cleanup locator missing")?,
                &PhysicalSnapshotCleanupFact { native },
            )?;
            self.cold_cleanup.pop();
        }
        while let Some(mut file) = self.auxiliary.pop() {
            let fresh = if file.metadata().directory_identity == *self.private.identity() {
                &self.private
            } else {
                &self.selected
            };
            if let Err(error) = file.rebind_cleanup_directory(fresh) {
                self.auxiliary.push(file);
                return Err(error);
            }
            if let Err(mut failure) = file.cleanup_journaled(&context.inner, &|native| {
                if let Some(locator) = &locator {
                    journal.record_cleanup(
                        locator,
                        &PhysicalSnapshotCleanupFact {
                            native: native.clone(),
                        },
                    )
                } else {
                    Ok(())
                }
            }) {
                if let Some(file) = failure.file.take() {
                    self.auxiliary.push(*file);
                }
                anyhow::bail!("{}", failure.message);
            }
        }
        Ok(())
    }
    fn failure(self, error: anyhow::Error) -> PhysicalSnapshotFailure {
        PhysicalSnapshotFailure {
            kind: snapshot_native::error_kind(&error).into(),
            message: format!("{error:#}").chars().take(4096).collect(),
            owner: Some(Box::new(self)),
        }
    }
    pub fn published_receipt(&self) -> Option<&PhysicalSnapshotReceipt> {
        self.published.as_ref()
    }
    pub fn acknowledge_published(
        self,
        context: &PhysicalSnapshotContext,
        commit: &dyn Fn(&PhysicalSnapshotReceipt) -> Result<()>,
        journal: &dyn PhysicalSnapshotRecoveryJournal,
    ) -> std::result::Result<PhysicalSnapshotReceipt, PhysicalSnapshotFailure> {
        let Some(receipt) = self.published.clone() else {
            return Err(self.failure(anyhow::anyhow!(
                "Snapshot has no settled publication receipt"
            )));
        };
        PublishedPhysicalSnapshot {
            owner: self,
            receipt,
        }
        .acknowledge(context, commit, journal)
    }
    pub fn acknowledge_published_in(
        mut self,
        fresh: &PhysicalSnapshotSelection,
        context: &PhysicalSnapshotContext,
        commit: &dyn Fn(&PhysicalSnapshotReceipt) -> Result<()>,
        journal: &dyn PhysicalSnapshotRecoveryJournal,
        check: &dyn Fn(&PhysicalSnapshotRecoveryLocator) -> Result<()>,
    ) -> std::result::Result<PhysicalSnapshotReceipt, PhysicalSnapshotFailure> {
        if let Err(error) = retain_on_panic(|| self.refresh_selection(fresh, check)) {
            return Err(self.failure(error));
        }
        self.acknowledge_published(context, commit, journal)
    }
    fn collect_registry(&mut self) -> Result<()> {
        if let Some(registry) = self.retained_registry.as_ref() {
            let files = registry
                .lock()
                .map_err(|_| anyhow::anyhow!("Snapshot file registry poisoned"))?
                .take_remaining()?;
            self.auxiliary.extend(files);
            self.retained_registry = None;
        }
        Ok(())
    }
    pub fn publish(
        mut self,
        context: &PhysicalSnapshotContext,
        journal: &dyn PhysicalSnapshotRecoveryJournal,
    ) -> std::result::Result<PublishedPhysicalSnapshot, PhysicalSnapshotFailure> {
        let result = retain_on_panic(|| -> Result<PhysicalSnapshotReceipt> {
            context.inner.check()?;
            ensure!(
                !self.cleanup_only && !self.recovery_blocked && self.unresolved.is_empty(),
                "Incomplete snapshot recovery permits cleanup only"
            );
            self.selected.verify()?;
            // SQLite VACUUM may retain a temporary image and bounded journal
            // headers. Remove those exact obsolete objects before
            // allocating a selected copy, keeping real disk usage within 3x.
            self.cleanup_auxiliary(context, journal)?;
            let expected = self
                .snapshot_hash
                .as_deref()
                .context("Snapshot has no verified compact image")?
                .to_owned();
            if let Some(main) = &self.main {
                let metadata = main.metadata();
                self.fact(
                    SnapshotPhase::CompactVerified,
                    &serde_json::to_vec(&serde_json::json!({"sha256":expected,
                    "bytes":metadata.bytes,"schema":SCHEMA_VERSION,"main":metadata}))?,
                    &context.inner,
                )?;
            }
            if self.candidate.is_none() {
                let copied = self
                    .main
                    .as_ref()
                    .context("Snapshot compact actual descriptor is missing")?
                    .copy_verified_to_selected(
                        &self.selected,
                        &self.request.operation_id,
                        &context.inner,
                        &self.permit,
                        &|metadata| self.inventory("candidate", metadata, &context.inner),
                    );
                match copied {
                    Ok(file) => self.candidate = Some(file),
                    Err(mut failure) => {
                        if let Some(file) = failure.file.take() {
                            self.auxiliary.push(*file);
                        }
                        return Err(anyhow::anyhow!(failure.message));
                    }
                }
            }
            let metadata = self.candidate.as_ref().unwrap().metadata();
            // A successful native rename is recorded on the owner before later
            // fsync/hash checks. Retrying that owner verifies its original FH;
            // it never executes the rename a second time.
            if !metadata.published {
                self.fact(SnapshotPhase::PublishIntent,
                    &serde_json::to_vec(&serde_json::json!({"operation":self.request.operation_id,"target":self.request.basename,"sha256":expected,
                        "candidate":metadata}))?,&context.inner)?;
            }
            let native = if metadata.published {
                self.candidate
                    .as_mut()
                    .unwrap()
                    .verify_published(&context.inner)?
            } else {
                self.candidate
                    .as_mut()
                    .unwrap()
                    .publish(&self.request.basename, &context.inner)?
            };
            ensure!(
                native.sha256 == expected,
                "Published snapshot digest differs from its compact image"
            );
            let receipt = PhysicalSnapshotReceipt {
                operation_id: native.operation_id,
                path: native.path,
                bytes: native.bytes,
                sha256: native.sha256,
                file_device: native.identity.device,
                file_object: native.identity.object,
                root_device: native.directory_identity.device,
                root_object: native.directory_identity.object,
                snapshot_revision: self.revision,
                schema_version: SCHEMA_VERSION,
                selection_epoch: self
                    .recovery_locator
                    .as_ref()
                    .map_or(native.selection_epoch, |l| l.manifest.selection_epoch),
            };
            if let Some(previous) = &self.published {
                ensure!(
                    serde_json::to_vec(previous)? == serde_json::to_vec(&receipt)?,
                    "Publication retry changed its durable receipt"
                );
            }
            self.published = Some(receipt.clone());
            if self.manifest.is_some() {
                self.fact(
                    SnapshotPhase::Published,
                    &serde_json::to_vec(&receipt)?,
                    &context.inner,
                )?;
            } else {
                ensure!(
                    self.acknowledged,
                    "Publication has no durable manifest or committed receipt"
                );
            }
            Ok(receipt)
        });
        match result {
            Ok(receipt) => Ok(PublishedPhysicalSnapshot {
                owner: self,
                receipt,
            }),
            Err(error) => Err(self.failure(error)),
        }
    }
    pub fn cleanup(
        mut self,
        context: &PhysicalSnapshotContext,
        journal: &dyn PhysicalSnapshotRecoveryJournal,
    ) -> std::result::Result<(), PhysicalSnapshotFailure> {
        let result = retain_on_panic(|| -> Result<()> {
            ensure!(
                self.published.is_none()
                    && self
                        .candidate
                        .as_ref()
                        .is_none_or(|file| !file.metadata().published),
                "Published snapshot cannot be deleted as a private failure"
            );
            if let Some(file) = self.candidate.take() {
                self.auxiliary.push(file);
            }
            self.cleanup_files(context, journal)?;
            self.cleanup_manifest(&context.inner, journal)?;
            if let Some(locator) = &self.recovery_locator {
                journal.finished(locator)?;
            }
            self.limits.settle(&self.request.operation_id);
            Ok(())
        });
        match result {
            Ok(()) => Ok(()),
            Err(error) => Err(self.failure(error)),
        }
    }
}
impl Repository {
    /// Restore the application's durable pending operation reservations before
    /// admitting new exports after restart. Metadata reserves quota only; it
    /// does not reopen snapshot writers or create selected-directory grants.
    pub fn restore_pending_physical_snapshot_reservations(
        &self,
        locators: &[PhysicalSnapshotRecoveryLocator],
        limits: &PhysicalSnapshotLimits,
        approve: &dyn Fn(&PhysicalSnapshotRecoveryLocator) -> Result<()>,
    ) -> Result<()> {
        ensure!(
            locators.len() <= 4 && serde_json::to_vec(locators)?.len() <= 1024 * 1024,
            "Pending snapshot registry exceeds its bounded capacity"
        );
        for locator in locators {
            approve(locator)?;
            ensure!(
                locator.operation_id == locator.request.operation_id
                    && locator.manifest.operation_id == locator.operation_id
                    && locator.request_digest
                        == format!(
                            "{:x}",
                            Sha256::digest(serde_json::to_vec(&locator.request)?)
                        ),
                "Pending snapshot request digest changed"
            );
            let private = SnapshotDirectory::open_private(
                self.path.parent().context("Profile directory missing")?,
                locator.manifest.selection_epoch,
            )?;
            ensure!(
                private.identity() == &locator.manifest.directory_identity,
                "Pending snapshot belongs to another actual profile scope"
            );
            limits.retain(&locator.request, &private, true)?;
        }
        Ok(())
    }
    /// Inspection performs only actual read opens. Uploaded locator/fact data
    /// cannot acquire a writer. The caller must compare its durable operation,
    /// profile epoch and phase-specific receipt/CAS in recover_physical_snapshot.
    pub fn inspect_physical_snapshot_recovery(
        &self,
        record: PhysicalSnapshotRecoveryRecord,
        selection: &PhysicalSnapshotSelection,
        context: &PhysicalSnapshotContext,
    ) -> Result<PhysicalSnapshotRecoveryPreview> {
        context.inner.check()?;
        selection.directory.verify()?;
        let locator = record.locator;
        ensure!(
            locator.operation_id == locator.request.operation_id
                && locator.manifest.operation_id == locator.operation_id
                && locator.request_digest
                    == format!(
                        "{:x}",
                        Sha256::digest(serde_json::to_vec(&locator.request)?)
                    ),
            "Recovery locator changed its sealed request"
        );
        ensure!(
            *selection.directory.identity() == locator.selected_identity,
            "Recovery selection identifies a different actual export root"
        );
        ensure!(
            record.cleanup.len() <= 16 && serde_json::to_vec(&record.cleanup)?.len() <= 1024 * 1024,
            "Recovery cleanup ledger exceeds its bounded capacity"
        );
        snapshot_native::validate_basename(&locator.request.basename)?;
        let private = SnapshotDirectory::open_private(
            self.path.parent().context("Profile directory missing")?,
            selection.directory.epoch(),
        )?;
        let probe = ActualFile::inspect_recovery(
            &private,
            &locator.manifest,
            snapshot_native::MAX_MANIFEST_BYTES,
            &context.inner,
        );
        let (manifest_probe, facts) = match probe {
            Ok(probe) => {
                let facts = probe.read_manifest(&context.inner)?;
                (Some(probe), facts)
            }
            Err(error) => {
                // A missing path alone proves nothing. Only a durable same-FH
                // disposition witness permits later native ID calibration;
                // that calibration occurs after coordinator authorization.
                let witness = record
                    .cleanup
                    .iter()
                    .find(|f| f.native.file.nonce == locator.manifest.nonce)
                    .context(
                        "Recovery manifest is unavailable without a recorded disposition witness",
                    )?;
                ensure!(
                    witness.native.file.identity == locator.manifest.identity
                        && witness.native.file.directory_identity
                            == locator.manifest.directory_identity,
                    "Manifest cleanup witness identifies a foreign object"
                );
                // Do not conceal an existing foreign/reused leaf behind an
                // absent-ID witness. Only an actual not-found probe proceeds.
                ensure!(
                    error.chain().any(|e| e
                        .downcast_ref::<std::io::Error>()
                        .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound)),
                    "Recovery manifest probe failed: {error:#}"
                );
                (None, Vec::new())
            }
        };
        if let Some(created) = facts
            .iter()
            .find(|f| !f.checkpoint && f.phase == SnapshotPhase::Created)
        {
            let value: serde_json::Value = serde_json::from_slice(&created.body)?;
            ensure!(
                value["request_digest"].as_str() == Some(locator.request_digest.as_str())
                    && serde_json::from_value::<PhysicalSnapshotRequest>(value["request"].clone())?
                        .operation_id
                        == locator.operation_id,
                "Manifest request and application locator disagree"
            );
            let root: snapshot_native::FileIdentity =
                serde_json::from_value(value["selected_root"].clone())?;
            ensure!(
                root == *selection.directory.identity(),
                "Recovery export selection is a different actual root"
            );
        }
        let phase = facts.iter().rev().find(|f| !f.checkpoint).map(|f| f.phase);
        let revision = facts
            .iter()
            .find(|f| !f.checkpoint && f.phase == SnapshotPhase::SourceCut)
            .map(|f| serde_json::from_slice::<serde_json::Value>(&f.body))
            .transpose()?
            .and_then(|v| v["revision"].as_u64())
            .unwrap_or(locator.request.expected_revision);
        let receipt = facts
            .iter()
            .find(|f| !f.checkpoint && f.phase == SnapshotPhase::Published)
            .map(|f| serde_json::from_slice::<PhysicalSnapshotReceipt>(&f.body))
            .transpose()?;
        if let Some(receipt) = &receipt {
            validate_recovery_receipt(receipt, &locator, &selection.directory)?;
        }
        if let Some(receipt) = &record.publication {
            validate_recovery_receipt(receipt, &locator, &selection.directory)?;
        }
        if let (Some(stored), Some(manifest)) = (&record.publication, &receipt) {
            ensure!(
                serde_json::to_vec(stored)? == serde_json::to_vec(manifest)?,
                "Application and manifest publication receipts disagree"
            );
        }
        let artifact_count = recovery_inventory(&facts)?.len();
        Ok(PhysicalSnapshotRecoveryPreview {
            operation_id: locator.operation_id.clone(),
            request_digest: locator.request_digest.clone(),
            snapshot_revision: revision,
            phase: phase.map_or_else(
                || {
                    if manifest_probe.is_none() {
                        if record.publication.is_some() {
                            "AcknowledgedCleanup".into()
                        } else {
                            "PrivateCleanup".into()
                        }
                    } else {
                        "Created".into()
                    }
                },
                |p| format!("{p:?}"),
            ),
            artifact_count,
            recorded_receipt: receipt.clone().or_else(|| record.publication.clone()),
            sealed_receipt: receipt.or_else(|| record.publication.clone()),
            locator,
            private,
            selected: selection.directory.clone(),
            manifest_probe,
            facts,
            cleanup: record.cleanup,
            trusted_publication: record.publication,
        })
    }
    /// The application authorization callback runs before every exclusive
    /// recovery open or calibration. Earlier phases can only clean their private
    /// objects; a compact image must pass its original whole SHA before publish.
    pub fn recover_physical_snapshot(
        &self,
        preview: PhysicalSnapshotRecoveryPreview,
        context: &PhysicalSnapshotContext,
        limits: &PhysicalSnapshotLimits,
        authorize: &dyn Fn(
            &PhysicalSnapshotRecoveryPreview,
        ) -> Result<PhysicalSnapshotRecoveryAction>,
    ) -> std::result::Result<PreparedPhysicalSnapshot, PhysicalSnapshotFailure> {
        let admitted = retain_on_panic(|| -> Result<PhysicalSnapshotRecoveryAction> {
            context.inner.check()?;
            preview.private.verify()?;
            preview.selected.verify()?;
            ensure!(
                preview.operation_id == preview.locator.operation_id
                    && preview.request_digest == preview.locator.request_digest
                    && preview.snapshot_revision == preview.locator.request.expected_revision,
                "Recovery preview identity was modified"
            );
            ensure!(
                serde_json::to_vec(&preview.recorded_receipt)?
                    == serde_json::to_vec(&preview.sealed_receipt)?,
                "Recovery preview publication receipt was modified"
            );
            authorize(&preview)
        });
        let action = match admitted {
            Ok(action) => action,
            Err(error) => {
                return Err(PhysicalSnapshotFailure {
                    kind: snapshot_native::error_kind(&error).into(),
                    message: format!("{error:#}"),
                    owner: None,
                });
            }
        };
        let permit = match limits.retain(&preview.locator.request, &preview.private, true) {
            Ok(permit) => permit,
            Err(error) => {
                return Err(PhysicalSnapshotFailure {
                    kind: snapshot_native::error_kind(&error).into(),
                    message: format!("{error:#}"),
                    owner: None,
                });
            }
        };
        let phase = preview
            .facts
            .iter()
            .rev()
            .find(|f| !f.checkpoint)
            .map(|f| f.phase);
        let mut owner = PreparedPhysicalSnapshot {
            request: preview.locator.request.clone(),
            revision: preview.locator.request.expected_revision,
            selected: preview.selected,
            private: preview.private,
            permit,
            manifest: None,
            main: None,
            candidate: None,
            auxiliary: Vec::new(),
            snapshot_hash: None,
            published: None,
            retained_registry: None,
            recovery_locator: Some(preview.locator.clone()),
            cleanup_only: action == PhysicalSnapshotRecoveryAction::CleanupPrivate,
            cold_cleanup: Vec::new(),
            acknowledged: phase == Some(SnapshotPhase::Acknowledged)
                || (preview.manifest_probe.is_none() && preview.recorded_receipt.is_some()),
            unresolved: Vec::new(),
            limits: limits.clone(),
            recovery_blocked: true,
            recovery_registered: true,
        };
        let result = retain_on_panic(|| -> Result<()> {
            if let Some(probe) = preview.manifest_probe {
                let native = ActualFile::reopen_recovery(probe, &context.inner, &owner.permit);
                let file = match native {
                    Ok(file) => file,
                    Err(mut failure) => {
                        if let Some(file) = failure.file.take() {
                            owner.auxiliary.push(*file);
                        }
                        anyhow::bail!("{}", failure.message);
                    }
                };
                let manifest = match DurableManifest::from_recovered(file, &context.inner) {
                    Ok(manifest) => manifest,
                    Err(mut failure) => {
                        if let Some(manifest) = failure.manifest.take() {
                            owner.manifest = Some(Arc::new(Mutex::new(*manifest)));
                        }
                        anyhow::bail!("{}", failure.message);
                    }
                };
                owner.manifest = Some(Arc::new(Mutex::new(manifest)));
            }
            let compact = preview
                .facts
                .iter()
                .find(|f| !f.checkpoint && f.phase == SnapshotPhase::CompactVerified)
                .map(|f| serde_json::from_slice::<serde_json::Value>(&f.body))
                .transpose()?;
            let mut inventory = recovery_inventory(&preview.facts)?;
            if let Some(compact) = &compact {
                let main: ActualFileMetadata = serde_json::from_value(compact["main"].clone())?;
                inventory.insert(main.nonce.clone(), ("main".into(), main));
                owner.snapshot_hash = Some(
                    compact["sha256"]
                        .as_str()
                        .context("Compact SHA missing")?
                        .into(),
                );
            }
            let intent = preview
                .facts
                .iter()
                .find(|f| !f.checkpoint && f.phase == SnapshotPhase::PublishIntent)
                .map(|f| serde_json::from_slice::<serde_json::Value>(&f.body))
                .transpose()?;
            if let Some(intent) = &intent {
                let candidate: ActualFileMetadata =
                    serde_json::from_value(intent["candidate"].clone())?;
                inventory.insert(candidate.nonce.clone(), ("candidate".into(), candidate));
            }
            let stored_receipt = preview.sealed_receipt.or(preview.trusted_publication);
            for fact in &preview.cleanup {
                ensure!(
                    fact.native.file.operation_id == owner.request.operation_id,
                    "Cleanup ledger belongs to another operation"
                );
                if fact.native.file.nonce == preview.locator.manifest.nonce
                    && owner.manifest.is_some()
                {
                    continue;
                }
                if fact.native.removed || fact.native.pending_zero_links() {
                    let directory =
                        if fact.native.file.directory_identity == *owner.private.identity() {
                            owner.private.clone()
                        } else {
                            owner.selected.clone()
                        };
                    owner
                        .cold_cleanup
                        .push(ColdCleanup::prepare(directory, fact.native.clone())?);
                    inventory.remove(&fact.native.file.nonce);
                }
            }
            if action == PhysicalSnapshotRecoveryAction::ResumePublication {
                ensure!(
                    compact.is_some() || (owner.acknowledged && stored_receipt.is_some()),
                    "Incomplete image cannot resume publication"
                );
            }
            owner.unresolved = inventory.values().map(|(_, meta)| meta.clone()).collect();
            let candidate_nonce = intent
                .as_ref()
                .and_then(|v| v["candidate"]["nonce"].as_str())
                .map(str::to_owned);
            for (_, (role, mut metadata)) in inventory {
                ensure!(
                    metadata.operation_id == owner.request.operation_id
                        && metadata.identity.is_some(),
                    "Unattested allocation is retained for explicit recovery; nonce is not file authority"
                );
                let directory = if metadata.directory_identity == *owner.private.identity() {
                    &owner.private
                } else {
                    &owner.selected
                };
                let mut probe = ActualFile::inspect_recovery(
                    directory,
                    &metadata,
                    owner.request.maximum_image_bytes,
                    &context.inner,
                );
                let active_candidate = role == "candidate"
                    && candidate_nonce.as_deref() == Some(metadata.nonce.as_str());
                if active_candidate
                    && probe.as_ref().is_err_and(|error| {
                        error.chain().any(|e| {
                            e.downcast_ref::<std::io::Error>()
                                .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound)
                        })
                    })
                {
                    metadata.published = true;
                    metadata.temporary_name = None;
                    metadata.final_basename = Some(owner.request.basename.clone());
                    probe = ActualFile::inspect_recovery(
                        directory,
                        &metadata,
                        owner.request.maximum_image_bytes,
                        &context.inner,
                    );
                }
                let file = ActualFile::reopen_recovery(probe?, &context.inner, &owner.permit);
                let file = match file {
                    Ok(file) => file,
                    Err(mut failure) => {
                        if let Some(file) = failure.file.take() {
                            owner.auxiliary.push(*file);
                        }
                        anyhow::bail!("{}", failure.message);
                    }
                };
                owner.unresolved.retain(|m| m.nonce != metadata.nonce);
                if role == "main" && owner.snapshot_hash.is_some() {
                    owner.main = Some(file);
                    owner
                        .main
                        .as_mut()
                        .unwrap()
                        .restore_sealed(owner.snapshot_hash.as_deref().unwrap(), &context.inner)?;
                } else if active_candidate {
                    owner.candidate = Some(file);
                    owner.candidate.as_mut().unwrap().restore_sealed(
                        owner
                            .snapshot_hash
                            .as_deref()
                            .context("Candidate compact SHA missing")?,
                        &context.inner,
                    )?;
                } else {
                    owner.auxiliary.push(file);
                }
            }
            if owner.candidate.is_none()
                && owner.acknowledged
                && action == PhysicalSnapshotRecoveryAction::ResumePublication
            {
                let receipt = stored_receipt
                    .as_ref()
                    .context("Acknowledged recovery needs its durable publication receipt")?;
                let metadata = ActualFileMetadata {
                    operation_id: owner.request.operation_id.clone(),
                    nonce: preview.locator.manifest.nonce.clone(),
                    temporary_name: None,
                    directory: owner.selected.path().into(),
                    directory_identity: owner.selected.identity().clone(),
                    selection_epoch: receipt.selection_epoch,
                    identity: Some(snapshot_native::FileIdentity {
                        device: receipt.file_device,
                        object: receipt.file_object.clone(),
                    }),
                    bytes: receipt.bytes,
                    published: true,
                    final_basename: Some(owner.request.basename.clone()),
                    cleanup_probe: None,
                };
                let probe = ActualFile::inspect_recovery(
                    &owner.selected,
                    &metadata,
                    owner.request.maximum_image_bytes,
                    &context.inner,
                )?;
                let file = ActualFile::reopen_recovery(probe, &context.inner, &owner.permit)
                    .map_err(|mut failure| {
                        if let Some(file) = failure.file.take() {
                            owner.auxiliary.push(*file);
                        }
                        anyhow::anyhow!(failure.message)
                    })?;
                owner.candidate = Some(file);
                owner
                    .candidate
                    .as_mut()
                    .unwrap()
                    .restore_sealed(&receipt.sha256, &context.inner)?;
                owner.snapshot_hash = Some(receipt.sha256.clone());
            }
            owner.published = stored_receipt;
            if owner
                .candidate
                .as_ref()
                .is_some_and(|f| f.metadata().published)
            {
                owner.cleanup_only = false;
            }
            owner.recovery_blocked = false;
            Ok(())
        });
        match result {
            Ok(()) => Ok(owner),
            Err(error) => Err(owner.failure(error)),
        }
    }
    pub fn prepare_physical_snapshot(
        &self,
        request: PhysicalSnapshotRequest,
        selection: &PhysicalSnapshotSelection,
        context: &PhysicalSnapshotContext,
        limits: &PhysicalSnapshotLimits,
        register: &dyn Fn(&PhysicalSnapshotRecoveryLocator) -> Result<()>,
    ) -> std::result::Result<PreparedPhysicalSnapshot, PhysicalSnapshotFailure> {
        self.prepare_physical_snapshot_observed(
            request,
            selection,
            context,
            limits,
            register,
            &|_| Ok(()),
        )
    }
    /// Reports a genuine fixed read cut before copying. The observer runs on
    /// this worker, must honor its own bounded waits, and grants no file access.
    pub fn prepare_physical_snapshot_observed(
        &self,
        request: PhysicalSnapshotRequest,
        selection: &PhysicalSnapshotSelection,
        context: &PhysicalSnapshotContext,
        limits: &PhysicalSnapshotLimits,
        register: &dyn Fn(&PhysicalSnapshotRecoveryLocator) -> Result<()>,
        after_read_cut: &dyn Fn(u64) -> Result<()>,
    ) -> std::result::Result<PreparedPhysicalSnapshot, PhysicalSnapshotFailure> {
        let preliminary = (|| -> Result<PreparedPhysicalSnapshot> {
            context.inner.check()?;
            selection.directory.verify()?;
            ensure!(
                !request.operation_id.is_empty()
                    && request.operation_id.len() <= 256
                    && !request.operation_id.chars().any(char::is_control),
                "Invalid snapshot operation ID"
            );
            snapshot_native::validate_basename(&request.basename)?;
            let private = SnapshotDirectory::open_private(
                self.path
                    .parent()
                    .context("Native profile directory missing")?,
                selection.directory.epoch(),
            )?;
            let permit = limits.retain(&request, &private, false)?;
            Ok(PreparedPhysicalSnapshot {
                request,
                revision: 0,
                selected: selection.directory.clone(),
                private,
                permit,
                manifest: None,
                main: None,
                candidate: None,
                auxiliary: Vec::new(),
                snapshot_hash: None,
                published: None,
                retained_registry: None,
                recovery_locator: None,
                cleanup_only: false,
                cold_cleanup: Vec::new(),
                acknowledged: false,
                unresolved: Vec::new(),
                limits: limits.clone(),
                recovery_blocked: false,
                recovery_registered: false,
            })
        })();
        let mut owner = match preliminary {
            Ok(owner) => owner,
            Err(error) => {
                return Err(PhysicalSnapshotFailure {
                    kind: snapshot_native::error_kind(&error).into(),
                    message: format!("{error:#}").chars().take(4096).collect(),
                    owner: None,
                });
            }
        };
        let result = retain_on_panic(|| -> Result<()> {
            let manifest = DurableManifest::create(
                &owner.private,
                &owner.request.operation_id,
                &context.inner,
                &owner.permit,
            );
            let manifest = match manifest {
                Ok(manifest) => manifest,
                Err(mut failure) => {
                    if let Some(file) = failure.file.take() {
                        owner.auxiliary.push(*file);
                    }
                    return Err(anyhow::anyhow!(failure.message));
                }
            };
            let locator = PhysicalSnapshotRecoveryLocator {
                operation_id: owner.request.operation_id.clone(),
                request_digest: format!(
                    "{:x}",
                    Sha256::digest(serde_json::to_vec(&owner.request)?)
                ),
                request: owner.request.clone(),
                manifest: manifest.metadata(),
                selected_identity: owner.selected.identity().clone(),
            };
            owner.manifest = Some(Arc::new(Mutex::new(manifest)));
            owner.recovery_locator = Some(locator.clone());
            register(&locator)?;
            owner.recovery_registered = true;
            owner.fact(SnapshotPhase::Created,&serde_json::to_vec(&serde_json::json!({"request":owner.request,
                "request_digest":locator.request_digest,"selected_root":owner.selected.identity()}))?,&context.inner)?;
            #[cfg(not(unix))]
            let anchors = {
                let connection = self
                    .connection
                    .lock()
                    .map_err(|_| anyhow::anyhow!("Repository lock poisoned"))?;
                schema::validate_current(&connection)?;
                let anchor_revision: i64 = connection.query_row(
                    "SELECT revision FROM repository_state WHERE singleton=1",
                    [],
                    |row| row.get::<_, i64>(0),
                )?;
                u64::try_from(anchor_revision)
                    .context("Snapshot source anchor revision is negative")?;
                let main =
                    unsafe { SourceFileProof::from_sqlite_main(connection.handle(), &self.path) }?;
                let mut journal: *mut ffi::sqlite3_file = ptr::null_mut();
                let code = unsafe {
                    ffi::sqlite3_file_control(
                        connection.handle(),
                        c"main".as_ptr(),
                        ffi::SQLITE_FCNTL_JOURNAL_POINTER,
                        (&mut journal as *mut *mut ffi::sqlite3_file).cast(),
                    )
                };
                ensure!(
                    code == ffi::SQLITE_OK && !journal.is_null(),
                    "Actual source WAL handle is unavailable"
                );
                let wal_path = self.path.with_file_name(format!(
                    "{}-wal",
                    self.path
                        .file_name()
                        .context("Native DB basename missing")?
                        .to_string_lossy()
                ));
                let wal = unsafe { SourceFileProof::from_sqlite_file(journal, &wal_path) }?;
                let shm_name = format!("{}-shm", self.path.file_name().unwrap().to_string_lossy());
                let shm = SourceFileProof::pin_checked_sidecar(&main, &shm_name)?;
                SourceAnchors { main, wal, shm }
            };
            #[cfg(not(unix))]
            let mut source = SourceSqlite::open(anchors, context.inner.clone())
                .context("Snapshot source reader opening failed")?;
            #[cfg(unix)]
            let mut source = {
                let connection = self
                    .connection
                    .lock()
                    .map_err(|_| anyhow::anyhow!("Repository lock poisoned"))?;
                schema::validate_current(&connection)?;
                SourceSqlite::open_unix(&connection, &self.path, context.inner.clone())
                    .context("Snapshot native Unix reader opening failed")?
            };
            source
                .connection()?
                .execute_batch("BEGIN DEFERRED")
                .context("Snapshot coherent read transaction failed")?;
            let revision: i64 = source.connection()?.query_row(
                "SELECT revision FROM repository_state WHERE singleton=1",
                [],
                |row| row.get::<_, i64>(0),
            )?;
            let revision =
                u64::try_from(revision).context("Snapshot coherent-cut revision is negative")?;
            ensure!(
                revision == owner.request.expected_revision,
                "Snapshot source revision changed before its coherent cut"
            );
            owner.revision = revision;
            schema::validate_current(source.connection()?)?;
            let pages: i64 =
                source
                    .connection()?
                    .pragma_query_value(None, "page_count", |row| row.get(0))?;
            let page_size: i64 =
                source
                    .connection()?
                    .pragma_query_value(None, "page_size", |row| row.get(0))?;
            ensure!(
                pages >= 0
                    && (512..=65536).contains(&page_size)
                    && pages
                        .checked_mul(page_size)
                        .is_some_and(|bytes| bytes as u64 <= owner.request.maximum_image_bytes),
                "Snapshot source exceeds its image budget"
            );
            owner.fact(
                SnapshotPhase::SourceCut,
                &serde_json::to_vec(
                    &serde_json::json!({"revision":revision,"pages":pages,"page_size":page_size}),
                )?,
                &context.inner,
            )?;
            after_read_cut(revision)?;
            context.inner.check()?;
            source.check()?;
            let mut destination = OwnedSqlite::allocate(
                owner.private.clone(),
                &owner.request.operation_id,
                owner.request.maximum_image_bytes,
                context.inner.clone(),
                owner.permit.clone(),
                owner.manifest.as_ref().unwrap().clone(),
            )?;
            let files = destination.files();
            owner.retained_registry = Some(files.clone());
            let copied = (|| -> Result<()> {
                destination
                    .open()
                    .context("Snapshot owned destination opening failed")?;
                let backup = Backup::new(source.connection()?, destination.connection_mut()?)
                    .context("Snapshot SQLite copy initialization failed")?;
                loop {
                    context.inner.check()?;
                    source.check()?;
                    match backup
                        .step((65536 / page_size) as i32)
                        .context("Snapshot SQLite page copy failed")?
                    {
                        StepResult::Done => break,
                        StepResult::More => {}
                        StepResult::Busy | StepResult::Locked => {
                            std::thread::sleep(Duration::from_millis(10));
                        }
                        _ => anyhow::bail!("Unexpected SQLite backup result"),
                    }
                    ensure!(
                        backup.progress().pagecount >= 0
                            && i64::from(backup.progress().pagecount)
                                .checked_mul(page_size)
                                .is_some_and(
                                    |bytes| bytes as u64 <= owner.request.maximum_image_bytes
                                ),
                        "Snapshot image grew beyond its cut budget"
                    );
                }
                drop(backup);
                source
                    .connection()?
                    .execute_batch("ROLLBACK")
                    .context("Snapshot source read transaction release failed")?;
                source
                    .close()
                    .context("Snapshot source SQLite close failed")?;
                destination
                    .normalize_copied_journal_mode()
                    .context("Snapshot standalone format normalization failed")?;
                destination
                    .connection()?
                    .execute_batch("PRAGMA journal_mode=OFF; PRAGMA synchronous=FULL;")
                    .context("Snapshot destination copy finalization failed")?;
                owner.fact(SnapshotPhase::Copied, b"{}", &context.inner)?;
                let progress =
                    SqliteProgress::install(destination.connection()?, context.inner.clone());
                super::snapshot_sanitize::purge_snapshot_authority(
                    destination.connection_mut()?,
                    &|| context.inner.check(),
                )
                .context("Snapshot image authority sanitization failed")?;
                owner.fact(SnapshotPhase::Sanitized, b"{}", &context.inner)?;
                destination
                    .connection()?
                    .execute_batch("VACUUM")
                    .context("Snapshot image compaction failed")?;
                schema::integrity_check(destination.connection()?)
                    .context("Snapshot compact image integrity verification failed")?;
                schema::validate_current(destination.connection()?)
                    .context("Snapshot compact image schema verification failed")?;
                drop(progress);
                context.inner.check()?;
                destination
                    .close()
                    .context("Snapshot compact SQLite close failed")?;
                Ok(())
            })()
            .map_err(|error| {
                let callbacks = [destination.take_last_error(), source.take_last_error()]
                    .into_iter()
                    .flatten()
                    .collect::<Vec<_>>();
                if callbacks.is_empty() {
                    error
                } else {
                    error.context(format!(
                        "Snapshot native VFS failure: {}",
                        callbacks.join("; ")
                    ))
                }
            });
            let close = destination
                .close()
                .context("Snapshot retained SQLite close failed");
            drop(destination);
            let mut files = files
                .lock()
                .map_err(|_| anyhow::anyhow!("Snapshot file registry poisoned"))?;
            if close.is_ok() {
                owner.main = Some(files.take_main()?);
                owner.auxiliary.extend(files.take_remaining()?);
                owner.retained_registry = None;
            }
            copied?;
            close?;
            let main = owner
                .main
                .as_mut()
                .context("Compact snapshot actual descriptor disappeared")?;
            let sha = main
                .seal_for_publication(&context.inner)
                .context("Snapshot compact descriptor sealing failed")?;
            let metadata = main.metadata();
            owner.snapshot_hash = Some(sha.clone());
            owner
                .fact(
                    SnapshotPhase::CompactVerified,
                    &serde_json::to_vec(&serde_json::json!({"sha256":sha,"bytes":metadata.bytes,
                "schema":SCHEMA_VERSION,"main":metadata}))?,
                    &context.inner,
                )
                .context("Snapshot compact verification checkpoint failed")?;
            Ok(())
        });
        match result {
            Ok(()) => Ok(owner),
            Err(error) => Err(owner.failure(error)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        repository::Repository,
        sync::{PeerCapabilities, SharedKey, network::PeerRecord},
    };
    use std::fs;

    #[test]
    fn private_snapshot_removes_key_cells_and_freelist_bytes_without_changing_the_live_peer() {
        let folder = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
        let repo = Repository::open(folder.path().join("profile")).unwrap();
        let state = repo.sync_profile_state().unwrap();
        repo.sync_configure_pairing(state.profile_epoch, state.pairing_epoch, true)
            .unwrap();
        let key = SharedKey::from_bytes([0x9d; 16]);
        let secret = key.expose_base64();
        let peer = PeerRecord {
            peer_id: "snapshot-peer".into(),
            client_id: "snapshot-client".into(),
            key,
            device_name: "Private snapshot fixture".into(),
            is_mobile: false,
            auth_epoch: 1,
            capabilities: PeerCapabilities {
                list: Some(false),
                dislike: Some(false),
            },
        };
        repo.sync_store_peer(state.pairing_epoch, &peer).unwrap();
        let revision = repo.revision().unwrap();
        let path = folder.path().join("private-snapshot.sqlite3");
        {
            let connection = repo.connection.lock().unwrap();
            connection.backup("main", &path, None).unwrap();
        }
        let mut snapshot = Connection::open(&path).unwrap();
        sanitize_sync_snapshot(&mut snapshot).unwrap();
        drop(snapshot);
        assert!(
            !fs::read(&path)
                .unwrap()
                .windows(secret.len())
                .any(|window| window == secret.as_bytes())
        );
        assert_eq!(repo.sync_peers().unwrap()[0].key.expose_base64(), secret);
        assert_eq!(repo.revision().unwrap(), revision);
        assert!(repo.sync_profile_state().unwrap().pairing_enabled);
        let snapshot = Connection::open(path).unwrap();
        assert_eq!(
            snapshot
                .query_row(
                    "SELECT count(*) FROM sync_peers WHERE private_key IS NOT NULL OR revoked=0",
                    [],
                    |row| row.get::<_, i64>(0)
                )
                .unwrap(),
            0
        );
        assert!(
            !snapshot
                .query_row(
                    "SELECT pairing_enabled FROM sync_profile_state",
                    [],
                    |row| row.get::<_, bool>(0)
                )
                .unwrap()
        );
    }
}

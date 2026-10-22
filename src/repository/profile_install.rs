//! Physical restore candidates retain native descriptors and strip execution
//! authority before installation can receive the continuously held writer guard.

use super::profile_guard::ProfileContinuityToken;
use super::snapshot_native::{
    ActualFile, ActualFileMetadata, DurableCleanupFact, DurableManifest, FileIdentity, IO_CHUNK,
    MAX_SNAPSHOT_BYTES, SnapshotContext, SnapshotDirectory, SnapshotLimiter, SnapshotPermit,
    SnapshotPhase, SourceFileProof,
};
use super::snapshot_vfs::{OwnedFiles, OwnedSqlite};
use super::{Repository, SCHEMA_VERSION, schema, snapshot_sanitize};
use anyhow::{Context, Result, ensure};
use rusqlite::ffi;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

pub(crate) mod authority;
mod group;
pub use authority::PhysicalRestoreAuthorityReport;
pub use group::{
    PhysicalProfileFileIdentity, PhysicalProfileInstallFailure, PhysicalProfileInstallation,
    PhysicalProfileRecoveryAdmission, PhysicalProfileRecoveryFailure, PhysicalProfileRecoveryFile,
    PhysicalProfileRecoveryOwner, PhysicalProfileRecoveryPreview, PhysicalProfileRecoverySelection,
    PhysicalProfileRecoverySelectionFailure, PhysicalProfileTerminal,
    PhysicalProfileTerminalAdmission, PhysicalProfileTerminalRecovery,
    PhysicalProfileTerminalRecoveryFailure, PhysicalProfileTerminalRecoveryOwner,
    PhysicalProfileTerminalSelection, PhysicalProfileTerminalSelectionFailure,
};

fn trace_sqlite_memory(connection: &rusqlite::Connection, phase: &str) {
    if std::env::var_os("LX_DEBUG").is_none() {
        return;
    }
    let (mut cache, mut schema_bytes, mut statements, mut unused) = (0, 0, 0, 0);
    let (mut allocated, mut peak, mut largest, mut unused64) = (0, 0, 0, 0);
    let codes = unsafe {
        [
            ffi::sqlite3_db_status(
                connection.handle(),
                ffi::SQLITE_DBSTATUS_CACHE_USED,
                &mut cache,
                &mut unused,
                0,
            ),
            ffi::sqlite3_db_status(
                connection.handle(),
                ffi::SQLITE_DBSTATUS_SCHEMA_USED,
                &mut schema_bytes,
                &mut unused,
                0,
            ),
            ffi::sqlite3_db_status(
                connection.handle(),
                ffi::SQLITE_DBSTATUS_STMT_USED,
                &mut statements,
                &mut unused,
                0,
            ),
            ffi::sqlite3_status64(ffi::SQLITE_STATUS_MEMORY_USED, &mut allocated, &mut peak, 0),
            ffi::sqlite3_status64(
                ffi::SQLITE_STATUS_MALLOC_SIZE,
                &mut unused64,
                &mut largest,
                0,
            ),
        ]
    };
    if codes.iter().all(|code| *code == ffi::SQLITE_OK) {
        let cache_pages: rusqlite::Result<i64> =
            connection.pragma_query_value(None, "cache_size", |row| row.get(0));
        let spill_pages: rusqlite::Result<i64> =
            connection.pragma_query_value(None, "cache_spill", |row| row.get(0));
        eprintln!(
            "Restore SQLite {phase}: cache_bytes={cache} schema_bytes={schema_bytes} statement_bytes={statements} global_bytes={allocated} global_peak_bytes={peak} largest_allocation={largest} cache_pages={cache_pages:?} spill_pages={spill_pages:?}"
        );
    }
}

pub struct PhysicalRestoreSelection {
    source: SourceFileProof,
}
pub struct PhysicalRestoreSelectionFailure {
    pub message: String,
    owner: Option<Box<SourceFileProof>>,
}
impl std::fmt::Debug for PhysicalRestoreSelectionFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PhysicalRestoreSelectionFailure")
            .field("message", &self.message)
            .field("has_owner", &self.owner.is_some())
            .finish()
    }
}
impl std::fmt::Display for PhysicalRestoreSelectionFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}
impl std::error::Error for PhysicalRestoreSelectionFailure {}
impl PhysicalRestoreSelection {
    pub fn open_selected(
        path: impl AsRef<Path>,
    ) -> std::result::Result<Self, PhysicalRestoreSelectionFailure> {
        SourceFileProof::pin_immutable(path)
            .map(|source| Self { source })
            .map_err(|mut failure| PhysicalRestoreSelectionFailure {
                message: failure.message,
                owner: failure.source.take(),
            })
    }
    pub fn path(&self) -> Result<&Path> {
        Ok(&self.source.metadata()?.path)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PhysicalRestoreRequest {
    pub operation_id: String,
    pub expected_revision: u64,
    pub maximum_image_bytes: u64,
    pub selection_epoch: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PhysicalRestoreImageView {
    pub operation_id: String,
    pub source_sha256: String,
    pub image_sha256: String,
    pub source_bytes: u64,
    pub image_bytes: u64,
    pub source_schema: u32,
    pub image_schema: u32,
    pub restored_revision: u64,
    #[serde(default)]
    pub authority: Option<PhysicalRestoreAuthorityReport>,
}
/// A locator describes an allocated journal. It grants no reopen or deletion.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PhysicalRestoreLocator {
    pub operation_id: String,
    pub journal_path: PathBuf,
    pub journal_device: u64,
    pub journal_object: String,
    pub target_device: u64,
    pub target_object: String,
    pub request_sha256: String,
}
struct Reservation {
    digest: String,
    root: FileIdentity,
    permit: SnapshotPermit,
}
#[derive(Clone)]
pub struct PhysicalRestoreLimits {
    limiter: SnapshotLimiter,
    retained: Arc<Mutex<BTreeMap<String, Reservation>>>,
}
impl PhysicalRestoreLimits {
    pub fn new(aggregate_bytes: u64) -> Result<Self> {
        Ok(Self {
            limiter: SnapshotLimiter::new(aggregate_bytes)?,
            retained: Arc::new(Mutex::new(BTreeMap::new())),
        })
    }
    pub fn stats(&self) -> (usize, u64) {
        let stats = self.limiter.stats();
        (stats.active, stats.reserved_bytes)
    }
    fn reserve(
        &self,
        request: &PhysicalRestoreRequest,
        root: &FileIdentity,
    ) -> Result<SnapshotPermit> {
        let digest = format!("{:x}", Sha256::digest(serde_json::to_vec(request)?));
        let mut retained = self.retained.lock().unwrap_or_else(|e| e.into_inner());
        ensure!(
            !retained.contains_key(&request.operation_id),
            "A physical restore already retains this operation's objects"
        );
        let bytes = request
            .maximum_image_bytes
            .checked_mul(3)
            .and_then(|n| n.checked_add(65 * 1024 * 1024))
            .context("Restore reservation overflow")?;
        let permit = self
            .limiter
            .reserve_job(request.maximum_image_bytes, bytes)?;
        retained.insert(
            request.operation_id.clone(),
            Reservation {
                digest,
                root: root.clone(),
                permit: permit.clone(),
            },
        );
        Ok(permit)
    }
    fn verify(&self, request: &PhysicalRestoreRequest, root: &FileIdentity) -> Result<()> {
        let digest = format!("{:x}", Sha256::digest(serde_json::to_vec(request)?));
        let retained = self.retained.lock().unwrap_or_else(|e| e.into_inner());
        let record = retained
            .get(&request.operation_id)
            .context("Restore reservation disappeared")?;
        ensure!(
            record.digest == digest && record.root == *root,
            "Restore reservation changed ownership"
        );
        // The charged permit stays with the retained registry until confirmed cleanup.
        let _ = &record.permit;
        Ok(())
    }
    fn reconstruct_retained(
        &self,
        request: &PhysicalRestoreRequest,
        root: &FileIdentity,
    ) -> Result<SnapshotPermit> {
        let digest = format!("{:x}", Sha256::digest(serde_json::to_vec(request)?));
        let mut retained = self
            .retained
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if let Some(record) = retained.get(&request.operation_id) {
            ensure!(
                record.digest == digest && record.root == *root,
                "Existing restore charge identifies another request or actual root"
            );
            return Ok(record.permit.clone());
        }
        let bytes = request
            .maximum_image_bytes
            .checked_mul(3)
            .and_then(|bytes| bytes.checked_add(65 * 1024 * 1024))
            .context("Recovered restore reservation overflow")?;
        let permit = self
            .limiter
            .restore_job(request.maximum_image_bytes, bytes)?;
        retained.insert(
            request.operation_id.clone(),
            Reservation {
                digest,
                root: root.clone(),
                permit: permit.clone(),
            },
        );
        Ok(permit)
    }
    fn release(&self, operation: &str) {
        self.retained
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(operation);
    }
}
pub struct PhysicalRestoreContext {
    inner: SnapshotContext,
}
impl PhysicalRestoreContext {
    pub fn new(timeout: Duration) -> Result<Self> {
        Ok(Self {
            inner: SnapshotContext::new(timeout)?,
        })
    }
    pub fn cancel(&self) {
        self.inner.cancel();
    }
}
struct SqliteProgress {
    database: *mut ffi::sqlite3,
    context: Box<SnapshotContext>,
}
impl SqliteProgress {
    fn install(connection: &rusqlite::Connection, context: SnapshotContext) -> Self {
        let mut progress = Self {
            database: unsafe { connection.handle() },
            context: Box::new(context),
        };
        unsafe {
            ffi::sqlite3_progress_handler(
                progress.database,
                1000,
                Some(restore_progress),
                (&mut *progress.context as *mut SnapshotContext).cast(),
            );
        }
        progress
    }
}
unsafe extern "C" fn restore_progress(context: *mut std::ffi::c_void) -> i32 {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        i32::from(
            context.is_null()
                || unsafe { &*context.cast::<SnapshotContext>() }
                    .check()
                    .is_err(),
        )
    }))
    .unwrap_or(1)
}
impl Drop for SqliteProgress {
    fn drop(&mut self) {
        unsafe {
            ffi::sqlite3_progress_handler(self.database, 0, None, std::ptr::null_mut());
        }
    }
}
pub struct PreparedPhysicalRestore {
    request: PhysicalRestoreRequest,
    continuity: ProfileContinuityToken,
    target: SnapshotDirectory,
    private: SnapshotDirectory,
    source: Option<PhysicalRestoreSelection>,
    manifest: Option<Arc<Mutex<DurableManifest>>>,
    sqlite: Option<OwnedSqlite>,
    registry: Option<Arc<Mutex<OwnedFiles>>>,
    main: Option<ActualFile>,
    auxiliary: Vec<ActualFile>,
    permit: SnapshotPermit,
    limits: PhysicalRestoreLimits,
    locator: Option<PhysicalRestoreLocator>,
    view: Option<PhysicalRestoreImageView>,
    authority: Option<authority::CurrentAuthority>,
}
impl std::fmt::Debug for PreparedPhysicalRestore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PreparedPhysicalRestore")
            .field("operation_id", &self.request.operation_id)
            .field("ready", &self.view.is_some())
            .field("retains_sqlite", &self.sqlite.is_some())
            .finish()
    }
}
pub struct PhysicalRestoreFailure {
    pub message: String,
    pub owner: Option<Box<PreparedPhysicalRestore>>,
}
impl std::fmt::Debug for PhysicalRestoreFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PhysicalRestoreFailure")
            .field("message", &self.message)
            .field("owner", &self.owner)
            .finish()
    }
}
impl std::fmt::Display for PhysicalRestoreFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}
impl std::error::Error for PhysicalRestoreFailure {}
impl PreparedPhysicalRestore {
    pub fn view(&self) -> Option<&PhysicalRestoreImageView> {
        self.view.as_ref()
    }
    pub fn locator(&self) -> Option<&PhysicalRestoreLocator> {
        self.locator.as_ref()
    }
    pub fn verify_guard(&self, guard: &super::ProfileGuard) -> Result<()> {
        guard.verify_continuity(&self.continuity)?;
        self.target.verify()?;
        self.limits.verify(&self.request, self.target.identity())
    }
    fn fact(&self, phase: SnapshotPhase, body: &[u8], context: &SnapshotContext) -> Result<()> {
        self.manifest
            .as_ref()
            .context("Restore journal missing")?
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .append_once(phase, body, context)
    }
    fn take_sqlite_files(&mut self) -> Result<()> {
        if let Some(sqlite) = self.sqlite.as_mut() {
            sqlite.close()?;
        }
        self.sqlite.take();
        if let Some(registry) = self.registry.as_ref() {
            let mut files = registry.lock().unwrap_or_else(|e| e.into_inner());
            if self.main.is_none() {
                self.main = Some(files.take_main()?);
            }
            self.auxiliary.extend(files.take_remaining()?);
        }
        self.registry.take();
        Ok(())
    }
    /// Abort is ownership-preserving: failed close/unlink returns this same owner.
    /// Cleanup ignores the cancelled preparation context and uses a fresh wait.
    pub fn abort(
        mut self,
        context: &PhysicalRestoreContext,
    ) -> std::result::Result<(), PhysicalRestoreFailure> {
        let result = self.cleanup_private(context);
        result.map_err(|error| self.failure(error))
    }
    fn cleanup_private(&mut self, context: &PhysicalRestoreContext) -> Result<()> {
        self.take_sqlite_files()?;
        self.source.take();
        if let Some(main) = self.main.take() {
            self.auxiliary.push(main);
        }
        while let Some(file) = self.auxiliary.pop() {
            let journal = self.manifest.clone();
            let record = |fact: &DurableCleanupFact| -> Result<()> {
                if let Some(journal) = &journal {
                    journal
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .append_checkpoint(
                            &serde_json::to_vec(
                                &serde_json::json!({"kind":"cleanup","fact":fact}),
                            )?,
                            &context.inner,
                        )?;
                    #[cfg(test)]
                    if fact.pending_zero_links() {
                        group::crash_point(if fact.removed {
                            "terminal-prepare-file-removed"
                        } else {
                            "terminal-prepare-file-pending"
                        });
                    }
                }
                Ok(())
            };
            if let Err(mut failure) = file.cleanup_journaled(&context.inner, &record) {
                if let Some(file) = failure.file.take() {
                    self.auxiliary.push(*file);
                }
                return Err(anyhow::anyhow!(failure.message));
            }
        }
        if let Some(manifest) = self.manifest.take() {
            let manifest = match Arc::try_unwrap(manifest) {
                Ok(lock) => lock.into_inner().unwrap_or_else(|e| e.into_inner()),
                Err(retained) => {
                    self.manifest = Some(retained);
                    anyhow::bail!("Restore journal still has owners");
                }
            };
            if let Err(mut failure) = manifest.cleanup(&context.inner, &|_| Ok(())) {
                if let Some(manifest) = failure.manifest.take() {
                    self.manifest = Some(Arc::new(Mutex::new(*manifest)));
                }
                return Err(anyhow::anyhow!(failure.message));
            }
        }
        self.limits.release(&self.request.operation_id);
        Ok(())
    }
    fn failure(self, error: anyhow::Error) -> PhysicalRestoreFailure {
        let native_error = self.sqlite.as_ref().and_then(|sqlite| {
            let files = sqlite.files();
            let files = files.lock().ok()?;
            files.last_error().map(str::to_owned)
        });
        let message = match native_error {
            Some(native) => format!("{error:#}; last native VFS error: {native}"),
            None => format!("{error:#}"),
        };
        PhysicalRestoreFailure {
            message: message.chars().take(2048).collect(),
            owner: Some(Box::new(self)),
        }
    }
}

impl Repository {
    /// Copies only an explicitly selected, immutable standalone image. The live
    /// target database and JSON are untouched; installation needs a later guard.
    pub fn prepare_physical_restore(
        &self,
        request: PhysicalRestoreRequest,
        selection: PhysicalRestoreSelection,
        context: &PhysicalRestoreContext,
        limits: &PhysicalRestoreLimits,
        register: &dyn Fn(&PhysicalRestoreLocator) -> Result<()>,
    ) -> std::result::Result<PreparedPhysicalRestore, PhysicalRestoreFailure> {
        let preliminary=(||->Result<(ProfileContinuityToken,SnapshotDirectory,SnapshotDirectory,SnapshotPermit)> {
            context.inner.check()?;
            ensure!(!request.operation_id.is_empty() && request.operation_id.len()<=128 && !request.operation_id.chars().any(char::is_control) && request.selection_epoch>0 && request.maximum_image_bytes>0 && request.maximum_image_bytes<=MAX_SNAPSHOT_BYTES,"Invalid physical restore request");
            self.profile_guard.verify()?;
            ensure!(self.revision()?==request.expected_revision,"Target profile revision changed");
            let continuity=self.profile_guard.continuity_token()?;
            let target=SnapshotDirectory::pin_profile(self.profile_guard.directory(),request.selection_epoch)?;
            let private=SnapshotDirectory::open_private(self.profile_guard.directory(),request.selection_epoch)?;
            let permit=limits.reserve(&request,target.identity())?;
            Ok((continuity,target,private,permit))
        })();
        let (continuity, target, private, permit) =
            preliminary.map_err(|error| PhysicalRestoreFailure {
                message: format!("{error:#}"),
                owner: None,
            })?;
        let mut owner = PreparedPhysicalRestore {
            request,
            continuity,
            target,
            private,
            source: Some(selection),
            manifest: None,
            sqlite: None,
            registry: None,
            main: None,
            auxiliary: Vec::new(),
            permit,
            limits: limits.clone(),
            locator: None,
            view: None,
            authority: None,
        };
        let prepared=std::panic::catch_unwind(std::panic::AssertUnwindSafe(||->Result<()> {
            let manifest=match DurableManifest::create(&owner.private,&owner.request.operation_id,&context.inner,&owner.permit){Ok(manifest)=>manifest,Err(mut failure)=>{if let Some(file)=failure.file.take(){owner.auxiliary.push(*file);}return Err(anyhow::anyhow!(failure.message));}};
            let metadata=manifest.metadata();
            owner.manifest=Some(Arc::new(Mutex::new(manifest)));
            let fid=metadata.identity.as_ref().context("Journal lacks actual identity")?;
            let locator=PhysicalRestoreLocator{operation_id:owner.request.operation_id.clone(),journal_path:metadata.directory.join(metadata.temporary_name.as_ref().context("Journal lacks retained name")?),journal_device:fid.device,journal_object:fid.object.clone(),target_device:owner.target.identity().device,target_object:owner.target.identity().object.clone(),request_sha256:format!("{:x}",Sha256::digest(serde_json::to_vec(&owner.request)?))};
            owner.locator=Some(locator.clone());
            register(&locator)?;
            owner.fact(SnapshotPhase::Created,&serde_json::to_vec(&serde_json::json!({"restore_request":owner.request,"locator":locator}))?,&context.inner)?;
            let source=&owner.source.as_ref().unwrap().source;
            source.require_standalone_image()?;
            let source_bytes=source.len()?;
            ensure!((100..=owner.request.maximum_image_bytes).contains(&source_bytes),"Selected image exceeds its bound");
            let mut header=[0u8;100];
            ensure!(source.read_immutable_at(&mut header,0,&context.inner)?==100 && &header[..16]==b"SQLite format 3\0" && header[18]==1 && header[19]==1,"Selected image is not a standalone SQLite image");
            let source_schema=u32::from_be_bytes(header[60..64].try_into().unwrap());
            ensure!((1..=SCHEMA_VERSION).contains(&source_schema),"Unsupported native image schema");
            owner.fact(SnapshotPhase::SourceCut,&serde_json::to_vec(&serde_json::json!({"source_path":source.metadata()?.path,"source_identity":source.metadata()?.identity,"source_bytes":source_bytes,"source_schema":source_schema}))?,&context.inner)?;
            let journal=owner.manifest.as_ref().unwrap().clone();
            let inventory=|file:&ActualFileMetadata|->Result<()> {journal.lock().unwrap_or_else(|e|e.into_inner()).append_checkpoint(&serde_json::to_vec(&serde_json::json!({"kind":"allocation","role":"restore-main","file":file}))?,&context.inner)};
            owner.main=match ActualFile::create_journaled(&owner.private,&owner.request.operation_id,owner.request.maximum_image_bytes,&context.inner,&owner.permit,&inventory){Ok(main)=>Some(main),Err(mut failure)=>{owner.main=failure.file.take().map(|file|*file);return Err(anyhow::anyhow!(failure.message));}};
            let mut buffer=vec![0u8;IO_CHUNK];
            let mut at=0u64;
            let mut source_sha=Sha256::new();
            while at<source_bytes {
                let wanted=(source_bytes-at).min(IO_CHUNK as u64) as usize;
                let count=source.read_immutable_at(&mut buffer[..wanted],at,&context.inner)?;
                ensure!(count>0,"Selected SQLite image was truncated");
                source_sha.update(&buffer[..count]);
                let mut written=0usize;
                while written<count {let n=owner.main.as_mut().unwrap().write_at(at+written as u64,&buffer[written..count],&context.inner)?;ensure!(n>0,"Restore native write stalled");written+=n;}
                at+=count as u64;
            }
            let source_sha256=format!("{:x}",source_sha.finalize());
            owner.main.as_mut().unwrap().sync_all(&context.inner)?;
            owner.fact(SnapshotPhase::Copied,&serde_json::to_vec(&serde_json::json!({"source_sha256":source_sha256}))?,&context.inner)?;
            let mut sqlite=OwnedSqlite::allocate(owner.private.clone(),&owner.request.operation_id,owner.request.maximum_image_bytes,context.inner.clone(),owner.permit.clone(),owner.manifest.as_ref().unwrap().clone())?;
            if let Err((error,main))=sqlite.seed_main(owner.main.take().unwrap()){owner.main=Some(*main);return Err(error);}
            owner.registry=Some(sqlite.files());
            owner.sqlite=Some(sqlite);
            let sqlite=owner.sqlite.as_mut().unwrap();
            sqlite.open_connection()?;
            // Configure the untrusted seed before its first prepared SQL can
            // load sqlite_schema. The 40 MiB row bound retains 32 MiB checkpoints.
            for (kind, maximum) in [
                (ffi::SQLITE_LIMIT_SQL_LENGTH, 64 * 1024),
                (ffi::SQLITE_LIMIT_LENGTH, 40 * 1024 * 1024),
                (ffi::SQLITE_LIMIT_COLUMN, 128),
                (ffi::SQLITE_LIMIT_EXPR_DEPTH, 64),
                (ffi::SQLITE_LIMIT_ATTACHED, 1),
                (ffi::SQLITE_LIMIT_VARIABLE_NUMBER, 256),
                (ffi::SQLITE_LIMIT_COMPOUND_SELECT, 16),
                (ffi::SQLITE_LIMIT_VDBE_OP, 1_000_000),
                (ffi::SQLITE_LIMIT_WORKER_THREADS, 0),
            ] {
                unsafe { ffi::sqlite3_limit(sqlite.connection()?.handle(), kind, maximum); }
            }
            let progress=SqliteProgress::install(sqlite.connection()?,context.inner.clone());
            sqlite.connection()?.execute_batch("PRAGMA trusted_schema=OFF; PRAGMA mmap_size=0; PRAGMA temp_store=FILE;")?;
            sqlite.initialize_private_pragmas()?;
            schema::validate_identity(sqlite.connection()?,source_schema)?;
            snapshot_sanitize::validate_schema_for_version(sqlite.connection()?,source_schema,&||context.inner.check())?;
            schema::integrity_check(sqlite.connection()?)?;
            schema::migrate(sqlite.connection_mut()?,source_schema)?;
            snapshot_sanitize::purge_snapshot_authority(sqlite.connection_mut()?,&||context.inner.check())?;
            sqlite.enable_restore_rollback_journal()?;
            let current = self.connection.lock().map_err(|_|anyhow::anyhow!("Repository lock was poisoned"))?;
            const RESTORE_PAGE_BYTES: i64 = 32 * 1024;
            const AUTHORITY_PAGER_BYTES: i64 = 32 * 1024 * 1024;
            const COMPACTION_PAGER_BYTES: i64 = 8 * 1024 * 1024;
            let page_size: i64 = sqlite.connection()?.pragma_query_value(None, "page_size", |row| row.get(0))?;
            ensure!(page_size > 0, "Restore page geometry is invalid");
            let mut overlay_page_size = page_size;
            let local_assets: i64 = current.query_row("SELECT count(*) FROM local_assets", [], |row| row.get(0))?;
            // Repage a small seed before a large authority overlay, so its
            // new rows do not incur native proofs for thousands of tiny pages.
            // Each page remains below the unchanged 64 KiB native I/O bound.
            if source_bytes <= 16 * 1024 * 1024 && local_assets >= 8192 && page_size < RESTORE_PAGE_BYTES {
                sqlite.connection()?.pragma_update(None, "cache_size", COMPACTION_PAGER_BYTES / RESTORE_PAGE_BYTES)?;
                sqlite.connection()?.pragma_update(None, "page_size", RESTORE_PAGE_BYTES)?;
                sqlite.connection()?.execute_batch("VACUUM").context("Restore seed page layout")?;
                overlay_page_size = RESTORE_PAGE_BYTES;
            }
            // Keep the bounded authority indexes resident during the streaming
            // overlay, rather than reopening verified native pages for each
            // random key lookup.
            sqlite.connection()?.pragma_update(None, "cache_size", (AUTHORITY_PAGER_BYTES / overlay_page_size).max(1))?;
            trace_sqlite_memory(sqlite.connection()?, "before overlay");
            let (authority,authority_report) = authority::CurrentAuthority::copy_into(&current,sqlite.connection_mut()?,&||context.inner.check()).context("Restore current authority overlay")?;
            owner.authority = Some(authority);
            drop(current);
            trace_sqlite_memory(sqlite.connection()?, "after overlay");
            let revision:i64=sqlite.connection()?.query_row("SELECT revision FROM repository_state WHERE singleton=1",[],|row|row.get(0))?;
            let restored_revision=u64::try_from(revision).context("Invalid restored revision")?;
            owner.fact(SnapshotPhase::Sanitized,b"{}",&context.inner)?;
            let connection = owner.sqlite.as_ref().unwrap().connection()?;
            let page_size: i64 = connection.pragma_query_value(None, "page_size", |row| row.get(0))?;
            let page_count: i64 = connection.pragma_query_value(None, "page_count", |row| row.get(0))?;
            ensure!(page_size > 0 && page_count > 0, "Restore page geometry is invalid");
            // Larger pages reduce native proof calls while keeping temporary
            // rollback journals within their original bounded allowance.
            let compact_page_size = if page_size < RESTORE_PAGE_BYTES
                && page_count.checked_mul(page_size).context("Restore image size overflow")? >= 32 * 1024 * 1024
            {
                connection.pragma_update(None, "page_size", RESTORE_PAGE_BYTES)?;
                RESTORE_PAGE_BYTES
            } else {
                page_size
            };
            // VACUUM copies cache_size before changing its temporary pager's
            // page size. A KiB-based setting can therefore grow with that page
            // size. Positive page counts cap each pager at 8 MiB throughout
            // the transition, including larger incoming page formats.
            connection.pragma_update(None, "cache_size", (COMPACTION_PAGER_BYTES / compact_page_size).max(1))?;
            connection.execute_batch("PRAGMA shrink_memory;")?;
            trace_sqlite_memory(connection, "before compaction");
            connection.execute_batch("VACUUM").context("Restore compaction")?;
            trace_sqlite_memory(connection, "after compaction");
            schema::integrity_check(connection).context("Compacted restore integrity")?;
            trace_sqlite_memory(connection, "after final integrity");
            connection.pragma_update(None, "cache_size", -2048)?;
            context.inner.check()?;
            drop(progress);
            owner.take_sqlite_files()?;
            let main=owner.main.as_mut().context("Restored image owner disappeared")?;
            let image_sha256=main.seal_for_publication(&context.inner)?;
            let image_bytes=main.metadata().bytes;
            self.profile_guard.verify_continuity(&owner.continuity)?;
            ensure!(self.revision()?==owner.request.expected_revision,"Target changed during image preparation");
            {
                let current = self.connection.lock().map_err(|_|anyhow::anyhow!("Repository lock was poisoned"))?;
                owner.authority.as_ref().unwrap().verify_current(&current,&||context.inner.check())?;
            }
            let view=PhysicalRestoreImageView{operation_id:owner.request.operation_id.clone(),source_sha256,image_sha256,source_bytes,image_bytes,source_schema,image_schema:SCHEMA_VERSION,restored_revision,authority:Some(authority_report)};
            owner.fact(SnapshotPhase::CompactVerified,&serde_json::to_vec(&view)?,&context.inner)?;
            owner.view=Some(view);
            Ok(())
        })).unwrap_or_else(|_|Err(anyhow::anyhow!("Physical restore preparation panicked; actual owner retained")));
        match prepared {
            Ok(()) => Ok(owner),
            Err(error) => Err(owner.failure(error)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync::{PeerCapabilities, SharedKey, network::PeerRecord};
    use std::fs;
    pub(super) fn image(folder: &Path) -> (PathBuf, String) {
        let source = Repository::open(folder.join("source")).unwrap();
        let state = source.sync_profile_state().unwrap();
        source
            .sync_configure_pairing(state.profile_epoch, state.pairing_epoch, true)
            .unwrap();
        let key = SharedKey::from_bytes([0x57; 16]);
        let secret = key.expose_base64();
        source
            .sync_store_peer(
                state.pairing_epoch,
                &PeerRecord {
                    peer_id: "restore-fixture-peer".into(),
                    client_id: "restore-fixture-client".into(),
                    key,
                    device_name: "Restore fixture".into(),
                    is_mobile: false,
                    auth_epoch: 1,
                    capabilities: PeerCapabilities {
                        list: Some(false),
                        dislike: Some(false),
                    },
                },
            )
            .unwrap();
        let path = folder.join("selected.sqlite3");
        source
            .connection
            .lock()
            .unwrap()
            .backup("main", &path, None)
            .unwrap();
        let standalone = rusqlite::Connection::open(&path).unwrap();
        standalone
            .pragma_update(None, "journal_mode", "DELETE")
            .unwrap();
        standalone.execute_batch("VACUUM").unwrap();
        drop(standalone);
        drop(source);
        (path, secret)
    }
    pub(super) fn context() -> PhysicalRestoreContext {
        PhysicalRestoreContext::new(Duration::from_secs(20)).unwrap()
    }
    pub(super) fn request(repo: &Repository) -> PhysicalRestoreRequest {
        PhysicalRestoreRequest {
            operation_id: "restore-candidate-real".into(),
            expected_revision: repo.revision().unwrap(),
            maximum_image_bytes: 8 * 1024 * 1024,
            selection_epoch: 3,
        }
    }
    #[test]
    fn actual_physical_install_preserves_current_approved_peer_and_never_imports_private_key() {
        let folder = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
        let (source, _) = image(folder.path());
        let source_bytes = fs::read(&source).unwrap();
        let target = Arc::new(Repository::open(folder.path().join("target")).unwrap());
        let peer = crate::sync::network::PeerRecord {
            peer_id: "current-approved-peer".into(),
            client_id: "current-approved-client".into(),
            key: SharedKey::from_bytes([0x19; 16]),
            device_name: "Current fixture".into(),
            is_mobile: false,
            auth_epoch: 3,
            capabilities: PeerCapabilities {
                list: Some(false),
                dislike: Some(false),
            },
        };
        target.sync_configure_pairing(1, 3, true).unwrap();
        target.sync_store_peer(3, &peer).unwrap();
        let old_sequence = target.sync_profile_state().unwrap().sequence;
        let limits = PhysicalRestoreLimits::new(128 * 1024 * 1024).unwrap();
        let context = context();
        let prepared = target
            .prepare_physical_restore(
                request(&target),
                PhysicalRestoreSelection::open_selected(&source).unwrap(),
                &context,
                &limits,
                &|_| Ok(()),
            )
            .unwrap();
        assert!(prepared.view().unwrap().authority.is_some());
        let guard = target.close_into_guard().unwrap();
        assert!(guard.closed_authority_digest().is_some());
        let repository = prepared
            .install_files(guard, &context)
            .unwrap()
            .reopen(&context)
            .unwrap()
            .commit(&context)
            .unwrap()
            .into_repository();
        let peers = repository.sync_peers().unwrap();
        assert_eq!(peers.len(), 1);
        assert_eq!(peers[0].peer_id, peer.peer_id);
        assert!(
            peers[0].key.expose_base64() == peer.key.expose_base64(),
            "Actual current credential was not preserved"
        );
        let sql = repository.connection.lock().unwrap();
        let imported_key: Option<String> = sql
            .query_row(
                "SELECT private_key FROM sync_peers WHERE peer_id='restore-fixture-peer'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(
            imported_key.is_none(),
            "Imported metadata must remain keyless"
        );
        drop(sql);
        let scope = repository.sync_profile_state().unwrap();
        assert_eq!(scope.sequence, old_sequence + 1);
        let ticket = crate::sync::network::SessionTicket::new(peer.peer_id, 3, 71);
        repository.sync_begin_session(&ticket).unwrap();
        repository.sync_snapshot(&ticket).unwrap();
        assert!(
            fs::read(&source).unwrap() == source_bytes,
            "Selected source image changed"
        );
        assert_eq!(limits.stats(), (0, 0));
    }
    #[test]
    fn current_credential_revocation_without_revision_change_prevents_physical_install() {
        let folder = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
        let (source, _) = image(folder.path());
        let target = Arc::new(Repository::open(folder.path().join("target")).unwrap());
        target.sync_configure_pairing(1, 3, true).unwrap();
        target
            .sync_store_peer(
                3,
                &crate::sync::network::PeerRecord {
                    peer_id: "revoked-after-prepare".into(),
                    client_id: "fixture-client".into(),
                    key: SharedKey::from_bytes([0x29; 16]),
                    device_name: "Fixture".into(),
                    is_mobile: false,
                    auth_epoch: 3,
                    capabilities: PeerCapabilities {
                        list: Some(false),
                        dislike: Some(false),
                    },
                },
            )
            .unwrap();
        let revision = target.revision().unwrap();
        let limits = PhysicalRestoreLimits::new(128 * 1024 * 1024).unwrap();
        let context = context();
        let prepared = target
            .prepare_physical_restore(
                request(&target),
                PhysicalRestoreSelection::open_selected(source).unwrap(),
                &context,
                &limits,
                &|_| Ok(()),
            )
            .unwrap();
        target.sync_revoke_peer("revoked-after-prepare").unwrap();
        assert_eq!(target.revision().unwrap(), revision);
        let guard = target.close_into_guard().unwrap();
        let main = folder
            .path()
            .join("target")
            .join(super::super::DATABASE_NAME);
        let actual = fs::read(&main).unwrap();
        let failure = prepared.install_files(guard, &context).unwrap_err();
        assert!(
            failure
                .message
                .contains("Current execution authority changed"),
            "{}",
            failure.message
        );
        assert!(
            fs::read(&main).unwrap() == actual,
            "Rejected installation changed the actual current main"
        );
        assert!(!failure.owner.journal_path().exists());
        assert_eq!(limits.stats().0, 1);
        assert!(super::super::ProfileGuard::acquire(folder.path().join("target")).is_err());
    }
    #[test]
    fn actual_same_fid_main_mutation_after_close_is_refused_before_original_movement() {
        let folder = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
        let (source, _) = image(folder.path());
        let target = Arc::new(Repository::open(folder.path().join("target")).unwrap());
        let limits = PhysicalRestoreLimits::new(128 * 1024 * 1024).unwrap();
        let context = context();
        let prepared = target
            .prepare_physical_restore(
                request(&target),
                PhysicalRestoreSelection::open_selected(source).unwrap(),
                &context,
                &limits,
                &|_| Ok(()),
            )
            .unwrap();
        let guard = target.close_into_guard().unwrap();
        let identity = guard.closed_main_identity().unwrap().clone();
        let main = folder
            .path()
            .join("target")
            .join(super::super::DATABASE_NAME);
        let mut altered = fs::read(&main).unwrap();
        altered[24..28].copy_from_slice(&23456u32.to_be_bytes());
        fs::write(&main, &altered).unwrap();
        let root = SnapshotDirectory::pin_profile(folder.path().join("target"), 901).unwrap();
        let mut inspection = Some(
            root.open_group_file(super::super::DATABASE_NAME, false)
                .unwrap(),
        );
        assert_eq!(
            root.group_file_identity(inspection.as_ref().unwrap())
                .unwrap(),
            identity
        );
        group::close_owned(&mut inspection).unwrap();
        let failure = prepared.install_files(guard, &context).unwrap_err();
        assert!(
            failure.message.contains("Closed profile main body changed"),
            "{}",
            failure.message
        );
        assert!(
            fs::read(&main).unwrap() == altered,
            "Rejected installation moved or replaced the modified main"
        );
        assert_eq!(limits.stats().0, 1);
        assert!(super::super::ProfileGuard::acquire(folder.path().join("target")).is_err());
    }
    fn bytes(file: &ActualFile, context: &SnapshotContext) -> Vec<u8> {
        let length = file.len(context).unwrap() as usize;
        let mut output = vec![0u8; length];
        let mut at = 0;
        while at < length {
            let end = (at + IO_CHUNK).min(length);
            let n = file
                .read_at(at as u64, &mut output[at..end], context)
                .unwrap();
            assert!(n > 0);
            at += n;
        }
        output
    }
    #[test]
    fn actual_private_restore_candidate_sanitizes_secrets_and_leaves_source_and_target_unchanged() {
        let folder = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
        let (path, secret) = image(folder.path());
        let original = fs::read(&path).unwrap();
        assert!(
            original
                .windows(secret.len())
                .any(|bytes| bytes == secret.as_bytes())
        );
        let target = Repository::open(folder.path().join("target")).unwrap();
        let request = request(&target);
        let revision = request.expected_revision;
        let limits = PhysicalRestoreLimits::new(128 * 1024 * 1024).unwrap();
        let context = context();
        let locator = Mutex::new(None);
        let prepared = target
            .prepare_physical_restore(
                request,
                PhysicalRestoreSelection::open_selected(&path).unwrap(),
                &context,
                &limits,
                &|value| {
                    *locator.lock().unwrap() = Some(value.clone());
                    Ok(())
                },
            )
            .unwrap();
        assert_eq!(target.revision().unwrap(), revision);
        assert_eq!(fs::read(&path).unwrap(), original);
        assert_eq!(
            prepared.view().unwrap().source_sha256,
            format!("{:x}", Sha256::digest(&original))
        );
        let actual = bytes(prepared.main.as_ref().unwrap(), &context.inner);
        assert!(
            !actual
                .windows(secret.len())
                .any(|bytes| bytes == secret.as_bytes())
        );
        assert_eq!(
            prepared.view().unwrap().image_sha256,
            format!("{:x}", Sha256::digest(&actual))
        );
        let journal_path = locator
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .journal_path
            .clone();
        assert!(journal_path.exists());
        assert_eq!(limits.stats().0, 1);
        prepared.abort(&context).unwrap();
        assert!(!journal_path.exists());
        assert_eq!(limits.stats(), (0, 0));
    }
    #[test]
    fn actual_immutable_selection_blocks_writer_and_rename_and_rejects_sidecars_without_effects() {
        let folder = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
        let (path, _) = image(folder.path());
        let original = fs::read(&path).unwrap();
        let selection = PhysicalRestoreSelection::open_selected(&path).unwrap();
        assert!(fs::OpenOptions::new().write(true).open(&path).is_err());
        assert!(fs::rename(&path, folder.path().join("moved.sqlite3")).is_err());
        fs::write(
            folder.path().join("selected.sqlite3-journal"),
            b"foreign hot journal",
        )
        .unwrap();
        let target = Repository::open(folder.path().join("target")).unwrap();
        let limits = PhysicalRestoreLimits::new(128 * 1024 * 1024).unwrap();
        let context = context();
        let failure = target
            .prepare_physical_restore(request(&target), selection, &context, &limits, &|_| Ok(()))
            .unwrap_err();
        assert!(failure.message.contains("sidecar"));
        assert_eq!(fs::read(&path).unwrap(), original);
        assert_eq!(
            fs::read(folder.path().join("selected.sqlite3-journal")).unwrap(),
            b"foreign hot journal"
        );
        failure.owner.unwrap().abort(&context).unwrap();
        assert_eq!(limits.stats(), (0, 0));
    }
    #[test]
    fn original_guard_survives_close_reopen_but_a_new_acquire_cannot_install_its_prepared_image() {
        let folder = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
        let (path, _) = image(folder.path());
        let target = Arc::new(Repository::open(folder.path().join("target")).unwrap());
        let limits = PhysicalRestoreLimits::new(128 * 1024 * 1024).unwrap();
        let context = context();
        let prepared = target
            .prepare_physical_restore(
                request(&target),
                PhysicalRestoreSelection::open_selected(&path).unwrap(),
                &context,
                &limits,
                &|_| Ok(()),
            )
            .unwrap();
        let guard = target.close_into_guard().unwrap();
        prepared.verify_guard(&guard).unwrap();
        let reopened = Arc::new(Repository::open_with_guard(guard).unwrap());
        prepared.verify_guard(&reopened.profile_guard).unwrap();
        let guard = reopened.close_into_guard().unwrap();
        drop(guard);
        let fresh = super::super::ProfileGuard::acquire(folder.path().join("target")).unwrap();
        assert!(prepared.verify_guard(&fresh).is_err());
        prepared.abort(&context).unwrap();
        drop(fresh);
        assert_eq!(limits.stats(), (0, 0));
    }
    #[test]
    fn failed_or_cancelled_preparation_retains_real_objects_and_budget_until_fresh_cleanup() {
        let folder = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
        let (path, _) = image(folder.path());
        let target = Repository::open(folder.path().join("target")).unwrap();
        let limits = PhysicalRestoreLimits::new(128 * 1024 * 1024).unwrap();
        let cancelled = context();
        let failure = target
            .prepare_physical_restore(
                request(&target),
                PhysicalRestoreSelection::open_selected(&path).unwrap(),
                &cancelled,
                &limits,
                &|_| {
                    cancelled.cancel();
                    Ok(())
                },
            )
            .unwrap_err();
        let owner = failure.owner.unwrap();
        let journal_path = owner.locator().unwrap().journal_path.clone();
        assert!(journal_path.exists());
        assert_eq!(limits.stats().0, 1);
        assert!(
            target
                .prepare_physical_restore(
                    request(&target),
                    PhysicalRestoreSelection::open_selected(&path).unwrap(),
                    &context(),
                    &limits,
                    &|_| Ok(())
                )
                .is_err()
        );
        owner.abort(&context()).unwrap();
        assert!(!journal_path.exists());
        assert_eq!(limits.stats(), (0, 0));
    }
    #[test]
    fn oversized_untrusted_schema_is_limited_before_its_first_sql_and_owner_remains_retryable() {
        let folder = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
        let (path, _) = image(folder.path());
        let source = rusqlite::Connection::open(&path).unwrap();
        source
            .execute_batch(&format!(
                "CREATE TABLE oversized_untrusted_schema(value TEXT DEFAULT '{}')",
                "x".repeat(70 * 1024)
            ))
            .unwrap();
        drop(source);
        let original = fs::read(&path).unwrap();
        let target = Repository::open(folder.path().join("target")).unwrap();
        let limits = PhysicalRestoreLimits::new(128 * 1024 * 1024).unwrap();
        let context = context();
        let failure = target
            .prepare_physical_restore(
                request(&target),
                PhysicalRestoreSelection::open_selected(&path).unwrap(),
                &context,
                &limits,
                &|_| Ok(()),
            )
            .unwrap_err();
        let owner = failure.owner.unwrap();
        assert!(owner.view().is_none());
        let connection = owner.sqlite.as_ref().unwrap().connection().unwrap();
        assert_eq!(
            unsafe { ffi::sqlite3_limit(connection.handle(), ffi::SQLITE_LIMIT_SQL_LENGTH, -1) },
            64 * 1024
        );
        assert_eq!(
            unsafe { ffi::sqlite3_limit(connection.handle(), ffi::SQLITE_LIMIT_LENGTH, -1) },
            40 * 1024 * 1024
        );
        assert_eq!(fs::read(&path).unwrap(), original);
        assert_eq!(limits.stats().0, 1);
        owner.abort(&context).unwrap();
        assert_eq!(limits.stats(), (0, 0));
    }

    #[test]
    fn actual_profile_group_install_reopens_sealed_database_preserves_json_and_rolls_back() {
        let folder = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
        let (path, _) = image(folder.path());
        let target = Arc::new(Repository::open(folder.path().join("target")).unwrap());
        target.record_profile_change(&"b".repeat(64)).unwrap();
        let old_revision = target.revision().unwrap();
        let json_path = target.path().parent().unwrap().join("player-state.json");
        let json = br#"{"version":1,"data":{"sources":[],"selected":null,"volume":0.25}}"#;
        fs::write(&json_path, json).unwrap();
        let limits = PhysicalRestoreLimits::new(128 * 1024 * 1024).unwrap();
        let context = context();
        let prepared = target
            .prepare_physical_restore(
                request(&target),
                PhysicalRestoreSelection::open_selected(&path).unwrap(),
                &context,
                &limits,
                &|_| Ok(()),
            )
            .unwrap();
        let restored_revision = prepared.view().unwrap().restored_revision;
        let guard = target.close_into_guard().unwrap();
        let mut installed = prepared.install_files(guard, &context).unwrap();
        assert_eq!(fs::read(&json_path).unwrap(), json);
        assert!(installed.journal_path().is_file());
        assert!(super::super::ProfileGuard::acquire(json_path.parent().unwrap()).is_err());
        installed = installed.reopen(&context).unwrap();
        assert_eq!(
            installed.repository().unwrap().revision().unwrap(),
            restored_revision
        );
        installed.repository().unwrap().integrity_check().unwrap();
        let installed = installed
            .rollback(&context)
            .unwrap()
            .reopen(&context)
            .unwrap();
        assert_eq!(
            installed.repository().unwrap().revision().unwrap(),
            old_revision
        );
        assert_eq!(fs::read(&json_path).unwrap(), json);
        assert_eq!(limits.stats().0, 1);
        drop(installed);
    }

    #[test]
    fn profile_install_refuses_revision_changed_after_candidate_seal_without_moving_live_files() {
        let folder = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
        let (path, _) = image(folder.path());
        let target = Arc::new(Repository::open(folder.path().join("target")).unwrap());
        let main_path = target.path().to_path_buf();
        let limits = PhysicalRestoreLimits::new(128 * 1024 * 1024).unwrap();
        let context = context();
        let prepared = target
            .prepare_physical_restore(
                request(&target),
                PhysicalRestoreSelection::open_selected(&path).unwrap(),
                &context,
                &limits,
                &|_| Ok(()),
            )
            .unwrap();
        target.record_profile_change(&"d".repeat(64)).unwrap();
        let changed_revision = target.revision().unwrap();
        let guard = target.close_into_guard().unwrap();
        let original = fs::read(&main_path).unwrap();
        let failure = prepared.install_files(guard, &context).unwrap_err();
        assert!(
            failure.message.contains("revision changed"),
            "{}",
            failure.message
        );
        assert_eq!(fs::read(&main_path).unwrap(), original);
        assert!(super::super::ProfileGuard::acquire(main_path.parent().unwrap()).is_err());
        assert_eq!(limits.stats().0, 1);
        drop(failure);
        assert_eq!(
            Repository::open(main_path.parent().unwrap())
                .unwrap()
                .revision()
                .unwrap(),
            changed_revision
        );
    }

    #[test]
    fn replacement_after_sqlite_close_cannot_be_adopted_as_original_profile_main() {
        let folder = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
        let (path, _) = image(folder.path());
        let target = Arc::new(Repository::open(folder.path().join("target")).unwrap());
        let main = target.path().to_path_buf();
        let retained = main.with_file_name("retained-closed-original.sqlite3");
        let limits = PhysicalRestoreLimits::new(128 * 1024 * 1024).unwrap();
        let context = context();
        let prepared = target
            .prepare_physical_restore(
                request(&target),
                PhysicalRestoreSelection::open_selected(&path).unwrap(),
                &context,
                &limits,
                &|_| Ok(()),
            )
            .unwrap();
        let guard = target.close_into_guard().unwrap();
        let original = fs::read(&main).unwrap();
        fs::rename(&main, &retained).unwrap();
        let foreign = b"foreign main after genuine SQLite close";
        fs::write(&main, foreign).unwrap();
        let failure = prepared.install_files(guard, &context).unwrap_err();
        assert!(
            failure.message.contains("main identity changed"),
            "{}",
            failure.message
        );
        assert_eq!(fs::read(&main).unwrap(), foreign);
        assert_eq!(fs::read(&retained).unwrap(), original);
        assert_eq!(limits.stats().0, 1);
        assert!(super::super::ProfileGuard::acquire(main.parent().unwrap()).is_err());
    }
}

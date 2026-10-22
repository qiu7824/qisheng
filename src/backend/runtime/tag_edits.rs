//! Explicit local-tag consent and bounded native installation ownership.
use super::*;
use crate::library::tag_write::{
    LocalTagPatch, MAX_TAG_COVER_BYTES, MAX_TAG_METADATA_BYTES, TagField, VerifiedTagCover,
};
use crate::library::{scan::ScanContext, tag_install::TagOwnedInstallation};
use crate::system_control::ManagedWorker;
use serde::{Deserialize, Serialize};

const MAX_TAG_WORKERS: usize = 4;
const MAX_TAG_TEXT_BYTES: usize = 64 * 1024;
const MAX_TAG_ARTISTS: usize = 64;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(tag = "action", content = "value", rename_all = "snake_case")]
pub enum TagEditField<T> {
    #[default]
    Keep,
    Set(T),
    Remove,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocalTagEditPatch {
    #[serde(default)]
    pub title: TagEditField<String>,
    #[serde(default)]
    pub artists: TagEditField<Vec<String>>,
    #[serde(default)]
    pub album: TagEditField<String>,
    #[serde(default)]
    pub lyrics: TagEditField<String>,
    #[serde(default)]
    pub cover: TagEditField<Vec<u8>>,
}
impl LocalTagEditPatch {
    pub fn validate(&self) -> Result<()> {
        let text = |value: &str, maximum: usize| -> Result<usize> {
            ensure!(
                value.len() <= maximum && !value.contains('\0'),
                "Tag text exceeds its bound or contains NUL"
            );
            Ok(value.len())
        };
        let mut metadata = 0usize;
        for field in [&self.title, &self.album] {
            if let TagEditField::Set(value) = field {
                metadata = metadata
                    .checked_add(text(value, MAX_TAG_TEXT_BYTES)?)
                    .context("Tag metadata size overflow")?;
            }
        }
        if let TagEditField::Set(artists) = &self.artists {
            ensure!(
                artists.len() <= MAX_TAG_ARTISTS,
                "Tag artist count exceeds its bound"
            );
            for artist in artists {
                metadata = metadata
                    .checked_add(text(artist, MAX_TAG_TEXT_BYTES)?)
                    .context("Tag metadata size overflow")?;
            }
        }
        if let TagEditField::Set(lyrics) = &self.lyrics {
            metadata = metadata
                .checked_add(text(lyrics, MAX_TAG_METADATA_BYTES)?)
                .context("Tag metadata size overflow")?;
        }
        ensure!(
            metadata <= MAX_TAG_METADATA_BYTES,
            "Tag metadata exceeds 16 MiB"
        );
        if let TagEditField::Set(bytes) = &self.cover {
            ensure!(
                !bytes.is_empty() && bytes.len() <= MAX_TAG_COVER_BYTES,
                "Tag cover exceeds its bound or is empty"
            );
        }
        Ok(())
    }
    pub fn digest(&self) -> Result<String> {
        self.validate()?;
        Ok(format!(
            "{:x}",
            Sha256::digest(serde_json::to_vec(&("local-tag-edit-v1", self))?)
        ))
    }
    fn native(&self, check: impl Fn() -> Result<()>) -> Result<LocalTagPatch> {
        self.validate()?;
        check()?;
        let map = |field: &TagEditField<String>| match field {
            TagEditField::Keep => TagField::Keep,
            TagEditField::Remove => TagField::Remove,
            TagEditField::Set(value) => TagField::Set(value.clone()),
        };
        Ok(LocalTagPatch {
            title: map(&self.title),
            artists: match &self.artists {
                TagEditField::Keep => TagField::Keep,
                TagEditField::Remove => TagField::Remove,
                TagEditField::Set(value) => TagField::Set(value.clone()),
            },
            album: map(&self.album),
            lyrics: map(&self.lyrics),
            cover: match &self.cover {
                TagEditField::Keep => TagField::Keep,
                TagEditField::Remove => TagField::Remove,
                TagEditField::Set(bytes) => {
                    TagField::Set(VerifiedTagCover::from_bytes_checked(bytes.clone(), check)?)
                }
            },
        })
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TagEditRecoveryAction {
    ResumePublication,
    RestoreOriginal,
    RetireArtifacts,
}

#[derive(Clone, Debug, Serialize)]
pub struct TagEditView {
    pub operation: crate::repository::TagEditOperation,
    pub running: bool,
    pub has_live_owner: bool,
    pub pending_recovery: bool,
    pub finished: bool,
    pub error: Option<BackendError>,
}
#[derive(Clone, Debug, Serialize)]
pub struct LocalTagReadView {
    pub selection: crate::library::tag_install::TagWriteSelection,
    pub size_bytes: u64,
    pub modified_ms: Option<u64>,
    pub title: Option<String>,
    pub artists: Vec<String>,
    pub album: Option<String>,
    pub lyrics: Option<String>,
    pub picture_count: usize,
    pub metadata_error: Option<BackendError>,
}

#[derive(Default)]
struct TagOwnerSlot {
    native: Mutex<Option<Box<TagOwnedInstallation>>>,
    operation: Mutex<Option<crate::repository::TagEditOperation>>,
    // Admission stays charged across worker failure until its actual owner is retired.
    charged: AtomicBool,
}
type NativeOwnerSlot = Arc<TagOwnerSlot>;
impl TagOwnerSlot {
    fn remember(&self, operation: &crate::repository::TagEditOperation) {
        *self.operation.lock().unwrap_or_else(|e| e.into_inner()) = Some(operation.clone());
    }
    fn known_operation(&self) -> Option<crate::repository::TagEditOperation> {
        self.operation
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
    // Called only after the associated worker has actually joined.
    fn release_if_retired(&self) {
        let native = self.native.lock().unwrap_or_else(|e| e.into_inner());
        if native
            .as_ref()
            .is_none_or(|owner| !owner.has_live_handles() && !owner.has_retirement_obligation())
        {
            self.charged.store(false, Ordering::Release);
        }
    }
}
struct TagEntry {
    owner: NativeOwnerSlot,
    error: Option<BackendError>,
}
enum TagJobResult {
    Read(Box<LocalTagReadView>),
    Approval(Box<crate::repository::TagWriteApproval>),
    Revoked(crate::repository::RepositoryChange),
    Edit {
        operation: Box<crate::repository::TagEditOperation>,
        error: Option<BackendError>,
        catalog: Option<Box<TagCatalogUpdate>>,
    },
    Failed(BackendError),
}
struct TagCatalogUpdate {
    record: TrackRecord,
    asset: crate::domain::music::LocalAsset,
    authorization: crate::library::authority::FileAuthorization,
}
struct TagWorkerContext {
    repository: Arc<Repository>,
    authority: crate::library::authority::AuthorityRegistry,
    context: ScanContext,
    slot: NativeOwnerSlot,
    paused: Arc<AtomicBool>,
}
struct TagInstallInput {
    approval: String,
    epoch: u64,
    budget: crate::repository::TagEditBudget,
    patch: LocalTagEditPatch,
}
struct TagRecoveryInput {
    operation: crate::repository::TagEditOperation,
    action: TagEditRecoveryAction,
    recovery: Option<(String, u64)>,
}
struct TagJob {
    target: Option<String>,
    context: ScanContext,
    worker: Option<ManagedWorker>,
    result: Arc<Mutex<Option<TagJobResult>>>,
    owner: NativeOwnerSlot,
    closing: bool,
}
pub(super) struct TagEditRuntime {
    entries: HashMap<String, TagEntry>,
    jobs: HashMap<String, TagJob>,
    closing: bool,
    reads: VecDeque<(String, LocalTagReadView)>,
}
impl TagEditRuntime {
    pub(super) fn restore(repository: &Arc<Repository>) -> Result<Self> {
        let mut value = Self {
            entries: HashMap::new(),
            jobs: HashMap::new(),
            closing: false,
            reads: VecDeque::new(),
        };
        for operation in repository.pending_tag_edits()? {
            ensure!(
                value.entries.len() < crate::repository::tag_edit::MAX_TAG_EDIT_OPERATIONS,
                "Tag recovery history exceeds its bound"
            );
            let owner = Arc::new(TagOwnerSlot::default());
            owner.remember(&operation);
            value.entries.insert(
                operation.token.operation_id.clone(),
                TagEntry { owner, error: None },
            );
        }
        Ok(value)
    }
    pub(super) fn job_count(&self) -> usize {
        self.jobs
            .values()
            .filter(|job| job.target.is_none())
            .count()
            + self
                .entries
                .values()
                .filter(|entry| entry.owner.charged.load(Ordering::Acquire))
                .count()
    }
    pub(super) fn read_view(&self, id: &str) -> Option<LocalTagReadView> {
        self.reads
            .iter()
            .find(|(operation, _)| operation == id)
            .map(|(_, view)| view.clone())
    }
    pub(super) fn owns_operation(&self, operation: &str) -> bool {
        self.jobs.contains_key(operation)
    }
    fn spawn(
        &mut self,
        command: String,
        target: Option<String>,
        owner: NativeOwnerSlot,
        retirement: bool,
        work: impl FnOnce(ScanContext, NativeOwnerSlot) -> TagJobResult + Send + 'static,
    ) -> Result<()> {
        ensure!(!self.closing || retirement, "Tag edits are closing");
        ensure!(
            self.job_count() - usize::from(owner.charged.load(Ordering::Acquire)) < MAX_TAG_WORKERS
                && self.jobs.len() < MAX_TAG_WORKERS
                && !self.jobs.contains_key(&command),
            BackendError::new(ErrorKind::Busy, "Local tag worker capacity is full")
        );
        if let Some(target) = &target {
            ensure!(
                !self
                    .jobs
                    .values()
                    .any(|job| job.target.as_ref() == Some(target)),
                BackendError::new(
                    ErrorKind::Busy,
                    "A native tag operation still owns this asset"
                )
            );
            ensure!(
                self.entries.contains_key(target)
                    || self.entries.len() < crate::repository::tag_edit::MAX_TAG_EDIT_OPERATIONS,
                BackendError::new(ErrorKind::Busy, "Tag ownership history is full")
            );
            // Register the actual slot before spawn/native admission. SQL read failures
            // can never determine whether the final reference to this owner survives.
            self.entries
                .entry(target.clone())
                .or_insert_with(|| TagEntry {
                    owner: owner.clone(),
                    error: None,
                });
        }
        let context = ScanContext::new(Duration::from_secs(120));
        let worker_context = context.clone();
        let output = Arc::new(Mutex::new(None));
        let worker_output = output.clone();
        let retained_owner = owner.clone();
        let was_charged = owner.charged.swap(target.is_some(), Ordering::AcqRel);
        let spawned = ManagedWorker::spawn("lx-local-tag-edit", move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                work(worker_context, retained_owner)
            }))
            .unwrap_or_else(|_| {
                TagJobResult::Failed(BackendError::new(
                    ErrorKind::OperationFailed,
                    "Native tag worker panicked; its actual owner remains retained",
                ))
            });
            *worker_output.lock().unwrap_or_else(|e| e.into_inner()) = Some(result);
        });
        let worker = match spawned {
            Ok(worker) => worker,
            Err(error) => {
                // No worker started, so restore the existing owner's admission state.
                owner.charged.store(was_charged, Ordering::Release);
                return Err(error);
            }
        };
        self.jobs.insert(
            command,
            TagJob {
                target,
                context,
                worker: Some(worker),
                result: output,
                owner,
                closing: retirement,
            },
        );
        Ok(())
    }
    pub(super) fn cancel(&self, operation: &str) {
        for (command, job) in &self.jobs {
            if command == operation || job.target.as_deref() == Some(operation) {
                job.context.cancel.store(true, Ordering::Release);
            }
        }
    }
    fn begin_shutdown(&mut self) {
        self.closing = true;
        for job in self.jobs.values_mut() {
            job.context.cancel.store(true, Ordering::Release);
            job.closing = true;
        }
    }
}

impl Runtime {
    fn tag_admission(&self) -> Result<()> {
        self.tag_admission_target(None)
    }
    fn tag_admission_target(&self, target: Option<&str>) -> Result<()> {
        self.system_control.require_store_mutation()?;
        let existing = target
            .and_then(|id| self.tag_edits.entries.get(id))
            .is_some_and(|entry| entry.owner.charged.load(Ordering::Acquire));
        ensure!(
            !self.tag_edits.closing
                && self.tasks.len() + self.tag_edits.job_count() - usize::from(existing)
                    < MAX_TAG_WORKERS,
            BackendError::new(ErrorKind::Busy, "Shared file worker capacity is full")
        );
        Ok(())
    }
    pub(super) fn start_tag_read(&mut self, asset: String, operation: String) -> Result<()> {
        self.tag_admission()?;
        let repository = self.repository.clone();
        let authority = self.library.authority.clone();
        self.tag_edits.spawn(
            self.request_id.clone(),
            None,
            Arc::new(TagOwnerSlot::default()),
            false,
            move |context, _| match read_tags(&repository, &authority, asset, operation, &context) {
                Ok(view) => TagJobResult::Read(Box::new(view)),
                Err(error) => TagJobResult::Failed(tag_error(error)),
            },
        )?;
        self.async_started = true;
        Ok(())
    }
    pub(super) fn start_tag_approval(
        &mut self,
        id: String,
        selection: crate::library::tag_install::TagWriteSelection,
        recovery: Option<String>,
    ) -> Result<()> {
        self.tag_admission()?;
        let repository = self.repository.clone();
        let paused = self.system_control.mutation_gate();
        self.tag_edits.spawn(
            self.request_id.clone(),
            None,
            Arc::new(TagOwnerSlot::default()),
            false,
            move |context, _| {
                let result = (|| -> Result<crate::repository::TagWriteApproval> {
                    let check = || work_check(&context, &paused);
                    match recovery {
                        Some(operation) => {
                            let token = repository
                                .tag_edit_operation(&operation)?
                                .context("Tag recovery operation is absent")?
                                .token;
                            repository.approve_tag_recovery(&id, &token, &selection, &check)
                        }
                        None => repository.approve_tag_write(&id, &selection, &check),
                    }
                })();
                match result {
                    Ok(approval) => TagJobResult::Approval(Box::new(approval)),
                    Err(error) => TagJobResult::Failed(tag_error(error)),
                }
            },
        )?;
        self.async_started = true;
        Ok(())
    }
    pub(super) fn start_tag_revoke(&mut self, id: String, epoch: u64) -> Result<()> {
        self.tag_admission()?;
        let repository = self.repository.clone();
        let paused = self.system_control.mutation_gate();
        self.tag_edits.spawn(
            self.request_id.clone(),
            None,
            Arc::new(TagOwnerSlot::default()),
            false,
            move |context, _| match repository
                .revoke_tag_write(&id, epoch, &|| work_check(&context, &paused))
            {
                Ok(change) => TagJobResult::Revoked(change),
                Err(error) => TagJobResult::Failed(tag_error(error)),
            },
        )?;
        self.async_started = true;
        Ok(())
    }
    fn require_tag_track_idle(
        &self,
        selection: &crate::library::tag_install::TagWriteSelection,
    ) -> Result<()> {
        let asset = self
            .repository
            .library_asset_at_path(&selection.path)?
            .context("Tag edit asset is absent")?;
        ensure!(
            asset.id == selection.asset_id,
            BackendError::new(ErrorKind::Conflict, "Tag edit asset binding changed")
        );
        let playback = self.audio.snapshot();
        let preparing = self
            .pending
            .as_ref()
            .is_some_and(|pending| pending.identity.track_id == asset.track_id)
            || self
                .pending_promotion
                .as_ref()
                .is_some_and(|pending| pending.entry.track_id == asset.track_id);
        let preload = self.preload.snapshot();
        let speculative = matches!(
            preload.state,
            PreloadState::Resolving
                | PreloadState::AwaitingApproval
                | PreloadState::Preparing
                | PreloadState::Ready
        ) && preload
            .candidate
            .as_ref()
            .is_some_and(|entry| entry.track_id == asset.track_id);
        ensure!(
            !preparing
                && !speculative
                && (playback.track_id.as_deref() != Some(asset.track_id.as_str())
                    || !matches!(
                        playback.status,
                        PlaybackStatus::Loading
                            | PlaybackStatus::Buffering
                            | PlaybackStatus::Playing
                            | PlaybackStatus::Paused
                    )),
            BackendError::new(
                ErrorKind::Busy,
                "Stop playback and preloading of this track before editing its tags"
            )
        );
        Ok(())
    }
    pub(super) fn start_tag_install(
        &mut self,
        approval: String,
        epoch: u64,
        budget: crate::repository::TagEditBudget,
        patch: LocalTagEditPatch,
    ) -> Result<()> {
        patch.validate()?;
        budget.validate()?;
        let selected = self
            .repository
            .tag_write_approval(&approval)?
            .context("Independent tag write approval is absent")?;
        if self
            .repository
            .tag_edit_operation(&selected.selection.operation_id)?
            .is_none_or(|operation| !operation.is_finished())
        {
            self.require_tag_track_idle(&selected.selection)?;
        }
        let target = selected.selection.operation_id;
        self.tag_admission_target(Some(&target))?;
        ensure!(
            !self
                .tag_edits
                .jobs
                .values()
                .any(|job| job.target.as_deref() == Some(target.as_str())),
            BackendError::new(ErrorKind::Busy, "Tag worker still owns this operation")
        );
        let owner = self.tag_edits.entries.get(&target).map_or_else(
            || Arc::new(TagOwnerSlot::default()),
            |entry| entry.owner.clone(),
        );
        ensure!(
            !owner
                .native
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .as_ref()
                .is_some_and(|owner| owner.has_retirement_obligation()),
            BackendError::new(
                ErrorKind::Busy,
                "A retained native tag installation requires explicit recovery"
            )
        );
        let repository = self.repository.clone();
        let authority = self.library.authority.clone();
        let paused = self.system_control.mutation_gate();
        self.tag_edits.spawn(
            self.request_id.clone(),
            Some(target),
            owner,
            false,
            move |context, owner| {
                run_install(
                    TagWorkerContext {
                        repository,
                        authority,
                        context,
                        slot: owner,
                        paused,
                    },
                    TagInstallInput {
                        approval,
                        epoch,
                        budget,
                        patch,
                    },
                )
            },
        )?;
        self.async_started = true;
        Ok(())
    }
    pub(super) fn start_tag_recovery(
        &mut self,
        id: String,
        action: TagEditRecoveryAction,
        approval: Option<String>,
        epoch: Option<u64>,
    ) -> Result<()> {
        self.tag_admission_target(Some(&id))?;
        let operation = self
            .repository
            .tag_edit_operation(&id)?
            .context("Tag recovery operation is absent")?;
        ensure!(
            approval.is_some() == epoch.is_some(),
            "Tag recovery approval requires both ID and epoch"
        );
        let recovery = approval.zip(epoch);
        let owner = self.tag_edits.entries.get(&id).map_or_else(
            || Arc::new(TagOwnerSlot::default()),
            |entry| entry.owner.clone(),
        );
        owner.remember(&operation);
        let repository = self.repository.clone();
        let authority = self.library.authority.clone();
        let paused = self.system_control.mutation_gate();
        self.tag_edits.spawn(
            self.request_id.clone(),
            Some(id),
            owner,
            false,
            move |context, owner| {
                run_recovery(
                    TagWorkerContext {
                        repository,
                        authority,
                        context,
                        slot: owner,
                        paused,
                    },
                    TagRecoveryInput {
                        operation,
                        action,
                        recovery,
                    },
                )
            },
        )?;
        self.async_started = true;
        Ok(())
    }
    pub(super) fn tag_view(&self, id: &str) -> Result<Option<TagEditView>> {
        let Some(operation) = self.repository.tag_edit_operation(id)? else {
            return Ok(None);
        };
        let running = self
            .tag_edits
            .jobs
            .values()
            .any(|job| job.target.as_deref() == Some(id));
        let (has_live_owner, has_obligation, error) = match self.tag_edits.entries.get(id) {
            Some(entry) if !running => {
                let owner = entry.owner.native.lock().unwrap_or_else(|e| e.into_inner());
                (
                    owner.as_ref().is_some_and(|owner| owner.has_live_handles()),
                    owner
                        .as_ref()
                        .is_some_and(|owner| owner.has_retirement_obligation()),
                    entry.error.clone(),
                )
            }
            Some(entry) => (true, true, entry.error.clone()),
            None => (running, running, None),
        };
        let finished = operation.is_finished() && !running && !has_obligation && !has_live_owner;
        Ok(Some(TagEditView {
            operation,
            running,
            has_live_owner,
            pending_recovery: !finished,
            finished,
            error,
        }))
    }
    pub(super) fn tag_views(&self) -> Result<Vec<TagEditView>> {
        let mut ids = self
            .tag_edits
            .entries
            .keys()
            .cloned()
            .collect::<HashSet<_>>();
        for operation in self.repository.pending_tag_edits()? {
            ids.insert(operation.token.operation_id);
        }
        let mut ids = ids.into_iter().collect::<Vec<_>>();
        ids.sort();
        ids.into_iter()
            .map(|id| self.tag_view(&id))
            .filter_map(|result| match result {
                Ok(None) => None,
                other => Some(other.map(Option::unwrap)),
            })
            .collect()
    }
    pub(super) fn poll_tag_edits(&mut self) {
        let done = self
            .tag_edits
            .jobs
            .iter()
            .filter(|(_, job)| job.worker.as_ref().is_none_or(ManagedWorker::is_finished))
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>();
        for command in done {
            let mut job = self
                .tag_edits
                .jobs
                .remove(&command)
                .expect("Finished tag job remains owned");
            let joined = job
                .worker
                .as_mut()
                .map_or(Ok(()), ManagedWorker::join_finished);
            let outcome = job
                .result
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take()
                .unwrap_or_else(|| {
                    TagJobResult::Failed(BackendError::new(
                        ErrorKind::OperationFailed,
                        "Tag worker exited without a retained outcome",
                    ))
                });
            let mut result = joined;
            match outcome {
                TagJobResult::Read(view) => {
                    self.tag_edits.reads.push_back((command.clone(), *view));
                    while self.tag_edits.reads.len() > 64 {
                        self.tag_edits.reads.pop_front();
                    }
                }
                TagJobResult::Approval(approval) => {
                    self.events.lock().unwrap().publish(
                        self.revision,
                        "local_tags",
                        Some(command.clone()),
                        BackendEvent::TagWriteApprovalChanged { approval },
                    );
                }
                TagJobResult::Revoked(change) => self.revision = change.revision,
                TagJobResult::Failed(error) => {
                    result = Err(error.clone().into());
                    if let Some(id) = &job.target {
                        if let Ok(Some(operation)) = self.repository.tag_edit_operation(id) {
                            job.owner.remember(&operation);
                        }
                        // Spawn pre-registered this bounded slot. Preserve it even when
                        // both the worker's final SQL read and this observation fail.
                        let entry = self
                            .tag_edits
                            .entries
                            .get_mut(id)
                            .expect("Admitted tag target retains its owner slot");
                        entry.error = Some(error);
                    }
                }
                TagJobResult::Edit {
                    operation,
                    error,
                    catalog,
                } => {
                    let id = operation.token.operation_id.clone();
                    if let Some(update) = catalog {
                        if let Err(error) = self.catalog.restore_local_record(
                            &update.record,
                            &update.asset,
                            update.authorization,
                        ) {
                            result = Err(error);
                        }
                        if self
                            .current_record
                            .as_ref()
                            .is_some_and(|current| current.id == update.record.id)
                        {
                            self.current_record = Some(Arc::new(update.record));
                        }
                    }
                    self.tag_edits.entries.insert(
                        id.clone(),
                        TagEntry {
                            owner: job.owner.clone(),
                            error: error.clone(),
                        },
                    );
                    job.owner.remember(&operation);
                    if let Some(error) = error {
                        result = Err(error.into());
                    }
                    if let Ok(Some(view)) = self.tag_view(&id) {
                        self.events.lock().unwrap().publish(
                            self.revision,
                            "local_tags",
                            Some(command.clone()),
                            BackendEvent::TagEditChanged {
                                view: Box::new(view),
                            },
                        );
                    }
                }
            }
            job.owner.release_if_retired();
            if let Some(target) = &job.target
                && !job.owner.charged.load(Ordering::Acquire)
                && job.owner.known_operation().is_none()
            {
                self.tag_edits.entries.remove(target);
            }
            if let Ok(revision) = self.repository.revision() {
                self.revision = revision;
            }
            if !job.closing {
                self.publish_finished(command, &result);
            }
        }
    }
    pub(super) fn begin_tag_shutdown(&mut self) {
        self.tag_edits.begin_shutdown();
    }
    pub(super) fn poll_tag_quiesce_step(&mut self) -> Result<bool> {
        ensure!(
            self.tag_edits.closing,
            "Tag quiescence requires closed admission"
        );
        self.poll_tag_edits();
        if !self.tag_edits.jobs.is_empty() {
            return Ok(false);
        }
        let next = self
            .tag_edits
            .entries
            .iter()
            .find(|(_, entry)| {
                entry
                    .owner
                    .native
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .as_ref()
                    .is_some_and(|owner| {
                        owner.has_retirement_obligation() || owner.has_live_handles()
                    })
            })
            .map(|(id, entry)| {
                (
                    id.clone(),
                    entry.owner.known_operation(),
                    entry.owner.clone(),
                )
            });
        if let Some((id, operation, owner)) = next {
            let operation =
                operation.context("Retained native tag owner lacks its admitted token")?;
            let repository = self.repository.clone();
            let target = id.clone();
            self.tag_edits.spawn(
                format!("tag-retire:{id}"),
                Some(target),
                owner,
                true,
                move |_, owner| {
                    let error = retire_native_owner(&repository, &operation, &owner, None)
                        .err()
                        .map(tag_error);
                    match repository.tag_edit_operation(&operation.token.operation_id) {
                        Ok(Some(operation)) => TagJobResult::Edit {
                            operation: Box::new(operation),
                            error,
                            catalog: None,
                        },
                        Ok(None) => TagJobResult::Failed(BackendError::new(
                            ErrorKind::Conflict,
                            "Retained tag operation disappeared",
                        )),
                        Err(error) => TagJobResult::Failed(tag_error(error)),
                    }
                },
            )?;
            return Ok(false);
        }
        Ok(true)
    }
}

fn tag_error(error: anyhow::Error) -> BackendError {
    library::backend_library_error_ref(&error)
        .or_else(|| error.downcast_ref::<BackendError>().cloned())
        .unwrap_or_else(|| {
            BackendError::new(
                ErrorKind::OperationFailed,
                error.to_string().chars().take(1024).collect::<String>(),
            )
        })
}
fn work_check(context: &ScanContext, paused: &AtomicBool) -> Result<()> {
    context.check()?;
    ensure!(
        !paused.load(Ordering::Acquire),
        BackendError::new(
            ErrorKind::Storage,
            "Durable settings recovery pauses local-tag mutation"
        )
    );
    Ok(())
}
fn coordinator(
    repository: Arc<Repository>,
    token: crate::repository::TagEditToken,
    owner_slot: NativeOwnerSlot,
    context: ScanContext,
    paused: Option<Arc<AtomicBool>>,
    recovery: Option<(String, u64)>,
) -> Result<crate::repository::RepositoryTagEditCoordinator> {
    let check: Arc<dyn Fn() -> Result<()> + Send + Sync> = Arc::new(move || {
        context.check()?;
        if let Some(paused) = &paused {
            ensure!(
                !paused.load(Ordering::Acquire),
                BackendError::new(
                    ErrorKind::Storage,
                    "Durable settings recovery pauses local-tag mutation"
                )
            );
        }
        Ok(())
    });
    let owner_token = serde_json::to_vec(&token)?;
    let owner: Arc<dyn Fn() -> Result<()> + Send + Sync> = Arc::new(move || {
        // Repository invokes this fence while holding its transaction mutex.
        // The bounded slot carries the actual admitted token; SQL require_token
        // and CAS run separately inside that same transaction.
        let admitted = owner_slot
            .known_operation()
            .context("Actual tag owner lost its admitted token")?;
        ensure!(
            serde_json::to_vec(&admitted.token)? == owner_token,
            "Tag ownership epoch or complete admitted token changed"
        );
        Ok(())
    });
    match recovery {
        Some((id, epoch)) => crate::repository::RepositoryTagEditCoordinator::for_recovery(
            repository, token, id, epoch, check, owner,
        ),
        None => {
            crate::repository::RepositoryTagEditCoordinator::new(repository, token, check, owner)
        }
    }
}
fn retain_native(
    result: Result<TagOwnedInstallation>,
    slot: &NativeOwnerSlot,
) -> Option<BackendError> {
    match result {
        Ok(owner) => {
            *slot.native.lock().unwrap_or_else(|e| e.into_inner()) = Some(Box::new(owner));
            None
        }
        Err(error) => match error.downcast::<crate::library::tag_install::TagInstallError>() {
            Ok(crate::library::tag_install::TagInstallError::Owned { message, owner }) => {
                let kind =
                    owner
                        .failure_kind()
                        .map_or(ErrorKind::OperationFailed, |kind| match kind {
                            crate::domain::library::LibraryErrorKind::Busy
                            | crate::domain::library::LibraryErrorKind::BudgetExceeded => {
                                ErrorKind::Busy
                            }
                            crate::domain::library::LibraryErrorKind::Conflict => {
                                ErrorKind::Conflict
                            }
                            crate::domain::library::LibraryErrorKind::Cancelled => {
                                ErrorKind::Cancelled
                            }
                            crate::domain::library::LibraryErrorKind::Timeout => ErrorKind::Timeout,
                            crate::domain::library::LibraryErrorKind::InvalidData => {
                                ErrorKind::InvalidData
                            }
                            crate::domain::library::LibraryErrorKind::Storage => ErrorKind::Storage,
                            crate::domain::library::LibraryErrorKind::Unavailable => {
                                ErrorKind::Unavailable
                            }
                        });
                *slot.native.lock().unwrap_or_else(|e| e.into_inner()) = Some(owner);
                Some(BackendError::new(kind, message))
            }
            Ok(error) => Some(tag_error(error.into())),
            Err(error) => Some(tag_error(error)),
        },
    }
}
fn retire_native_owner(
    repository: &Arc<Repository>,
    operation: &crate::repository::TagEditOperation,
    slot: &NativeOwnerSlot,
    action: Option<TagEditRecoveryAction>,
) -> Result<()> {
    let context = ScanContext::new(Duration::from_secs(120));
    let mut coordinator = coordinator(
        repository.clone(),
        operation.token.clone(),
        slot.clone(),
        context.clone(),
        None,
        None,
    )?;
    let mut guard = slot.native.lock().unwrap_or_else(|e| e.into_inner());
    let Some(owner) = guard.as_mut() else {
        return Ok(());
    };
    if owner.is_retired() && !owner.has_live_handles() {
        guard.take();
        return Ok(());
    }
    use crate::library::tag_install::TagInstallPhase;
    if !owner.is_retirement_only() {
        match action {
            Some(TagEditRecoveryAction::ResumePublication) => {
                owner.retry_publish(&context, &mut coordinator)?
            }
            Some(TagEditRecoveryAction::RestoreOriginal) => {
                owner.retry_restore(&context, &mut coordinator)?
            }
            Some(TagEditRecoveryAction::RetireArtifacts) => {}
            None => match owner.phase() {
                TagInstallPhase::Installing
                | TagInstallPhase::DiskVerified
                | TagInstallPhase::Restoring => {
                    if operation.commit.is_some() {
                        owner.retry_publish(&context, &mut coordinator)?;
                    } else {
                        owner.retry_restore(&context, &mut coordinator)?;
                    }
                }
                TagInstallPhase::RepoCommitted => {
                    owner.retry_publish(&context, &mut coordinator)?
                }
                _ => {}
            },
        }
    }
    owner.retire(&context, &mut coordinator)?;
    if owner.is_retired() && !owner.has_live_handles() {
        guard.take();
    }
    Ok(())
}
fn catalog_update(
    repository: &Arc<Repository>,
    authority: &crate::library::authority::AuthorityRegistry,
    operation: &crate::repository::TagEditOperation,
) -> Result<Option<Box<TagCatalogUpdate>>> {
    if operation.commit.is_none() && operation.restoration.is_none() {
        return Ok(None);
    }
    let asset = repository
        .library_asset_at_path(&operation.token.selection.path)?
        .context("Edited local asset is absent")?;
    ensure!(
        asset.id == operation.token.selection.asset_id,
        "Edited asset binding changed"
    );
    let record = repository
        .track(&asset.track_id)?
        .context("Edited track is absent")?;
    let authorization = library::file_authorization(repository, authority, &asset.id)?;
    authorization.check()?;
    Ok(Some(Box::new(TagCatalogUpdate {
        record,
        asset,
        authorization,
    })))
}
fn run_install(worker: TagWorkerContext, input: TagInstallInput) -> TagJobResult {
    let TagWorkerContext {
        repository,
        authority,
        context,
        slot,
        paused,
    } = worker;
    let TagInstallInput {
        approval,
        epoch,
        budget,
        patch,
    } = input;
    let result = (|| -> Result<TagJobResult> {
        let native = patch.native(|| work_check(&context, &paused))?;
        budget.validate()?;
        let selected = repository
            .tag_write_approval(&approval)?
            .context("Independent tag write approval is absent")?;
        ensure!(
            selected.approval_epoch == epoch
                && !selected.revoked
                && selected.purpose == crate::repository::tag_edit::TagWritePurpose::Install,
            BackendError::new(
                ErrorKind::Conflict,
                "Tag write approval changed or was revoked"
            )
        );
        work_check(&context, &paused)?;
        // This single profile directory is not a producer artifact. Keep the real
        // Guard-derived pin through SDK admission; directory failure precedes quota.
        #[cfg(windows)]
        let directory_pin = repository.ensure_tag_staging()?;
        #[cfg(windows)]
        let directory = directory_pin.path();
        #[cfg(not(windows))]
        let directory_path = repository
            .path()
            .parent()
            .context("Tag profile directory is absent")?
            .join("tag-edits");
        #[cfg(not(windows))]
        let directory = directory_path.as_path();
        let operation =
            repository.begin_tag_edit(&approval, epoch, budget, &patch.digest()?, &|| {
                work_check(&context, &paused)
            })?;
        slot.remember(&operation);
        if operation.is_finished() {
            let error = (operation.commit.is_none()
                || operation.phase != crate::repository::TagEditPhase::Published)
                .then(|| BackendError::new(
                    ErrorKind::Conflict,
                    "The retired tag intent did not complete installation; use a new operation ID",
                ));
            return Ok(TagJobResult::Edit {
                catalog: catalog_update(&repository, &authority, &operation)
                    .ok()
                    .flatten(),
                operation: Box::new(operation),
                error,
            });
        }
        {
            let mut native_owner = slot.native.lock().unwrap_or_else(|e| e.into_inner());
            ensure!(
                native_owner.is_none(),
                "Tag installation already retains an actual owner"
            );
            *native_owner = Some(Box::new(TagOwnedInstallation::unstarted(
                operation.token.selection.clone(),
            )));
        }
        let mut coordinator = coordinator(
            repository.clone(),
            operation.token.clone(),
            slot.clone(),
            context.clone(),
            Some(paused),
            None,
        )?;
        let unstarted = slot
            .native
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
            .context("Tag installation lost its unstarted obligation")?;
        let mut error = retain_native(
            unstarted.start_selected(directory, &native, &context, &mut coordinator),
            &slot,
        );
        let current = repository
            .tag_edit_operation(&operation.token.operation_id)?
            .context("Tag operation is absent")?;
        slot.remember(&current);
        if let Err(failure) = retire_native_owner(&repository, &current, &slot, None) {
            error.get_or_insert_with(|| tag_error(failure));
        }
        let operation = repository
            .tag_edit_operation(&operation.token.operation_id)?
            .context("Tag operation is absent")?;
        slot.remember(&operation);
        let catalog = if operation.is_finished() {
            catalog_update(&repository, &authority, &operation)
                .ok()
                .flatten()
        } else {
            None
        };
        Ok(TagJobResult::Edit {
            operation: Box::new(operation),
            error,
            catalog,
        })
    })();
    result.unwrap_or_else(|error| TagJobResult::Failed(tag_error(error)))
}
fn run_recovery(worker: TagWorkerContext, input: TagRecoveryInput) -> TagJobResult {
    let TagWorkerContext {
        repository,
        authority,
        context,
        slot,
        paused,
    } = worker;
    let TagRecoveryInput {
        operation,
        action,
        recovery,
    } = input;
    let result = (|| -> Result<TagJobResult> {
        slot.remember(&operation);
        let mut error = None;
        if slot
            .native
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_none()
            && !operation.is_finished()
        {
            let permission = recovery
                .context("Cold tag recovery requires a current explicit recovery approval")?;
            let mut coordinator = coordinator(
                repository.clone(),
                operation.token.clone(),
                slot.clone(),
                context.clone(),
                Some(paused),
                Some(permission),
            )?;
            let journal = crate::library::tag_install::journal_path(
                &repository
                    .path()
                    .parent()
                    .context("Tag profile directory is absent")?
                    .join("tag-edits"),
                &operation.token.operation_id,
            );
            error = retain_native(
                crate::library::tag_install::recover_selected_tags_owned(
                    &operation.token.selection,
                    &journal,
                    &context,
                    &mut coordinator,
                ),
                &slot,
            );
        }
        if let Err(failure) = retire_native_owner(&repository, &operation, &slot, Some(action)) {
            error.get_or_insert_with(|| tag_error(failure));
        }
        let operation = repository
            .tag_edit_operation(&operation.token.operation_id)?
            .context("Tag operation is absent")?;
        slot.remember(&operation);
        let catalog = if operation.is_finished() {
            catalog_update(&repository, &authority, &operation)
                .ok()
                .flatten()
        } else {
            None
        };
        Ok(TagJobResult::Edit {
            operation: Box::new(operation),
            error,
            catalog,
        })
    })();
    result.unwrap_or_else(|error| TagJobResult::Failed(tag_error(error)))
}
fn read_tags(
    repository: &Arc<Repository>,
    authority: &crate::library::authority::AuthorityRegistry,
    asset_id: String,
    operation_id: String,
    context: &ScanContext,
) -> Result<LocalTagReadView> {
    use std::io::{Read, Seek, SeekFrom};
    crate::domain::library::validate_library_id(&operation_id)?;
    let grant = repository
        .local_asset_authority(&asset_id)?
        .context("Local tag asset has no read authority")?;
    let binding = repository
        .library_asset_binding(&asset_id)?
        .context("Local tag binding is absent")?;
    let authorization = library::file_authorization(repository, authority, &asset_id)?
        .with_cancel(context.cancel.clone())?;
    let mut file = authorization.open()?;
    let before = file.metadata_snapshot()?;
    ensure!(
        before.metadata.len() <= crate::library::tag_write::MAX_TAG_FILE_BYTES,
        "Tag body exceeds 1 GiB"
    );
    let mut hash = Sha256::new();
    let mut bytes = [0u8; 64 * 1024];
    let mut count = 0u64;
    loop {
        context.check()?;
        let read = file.read(&mut bytes)?;
        if read == 0 {
            break;
        }
        count = count
            .checked_add(read as u64)
            .context("Tag body length overflow")?;
        ensure!(
            count <= before.metadata.len(),
            "Tag body grew while hashing"
        );
        hash.update(&bytes[..read]);
    }
    ensure!(
        count == before.metadata.len(),
        "Tag body changed while hashing"
    );
    file.seek(SeekFrom::Start(0))?;
    let metadata = crate::library::metadata::read_local_metadata_from_open_file(
        file.try_clone()?,
        before.path.clone(),
        count,
    );
    let after = file.metadata_snapshot()?;
    ensure!(
        before.change_stamp == after.change_stamp
            && before.metadata.len() == after.metadata.len()
            && before.path == after.path,
        "Tag body changed during inspection"
    );
    let (root_id, grant_epoch) = match grant.explicit_file_epoch {
        Some(epoch) => (None, epoch),
        None => {
            let root = grant
                .roots
                .first()
                .context("Local tags have no active read grant")?;
            (Some(root.root_id.clone()), root.grant_epoch)
        }
    };
    let selection = crate::library::tag_install::TagWriteSelection {
        operation_id,
        asset_id,
        path: after.path.clone(),
        expected_identity: file.identity().clone(),
        root_id,
        grant_epoch,
        expected_binding_revision: binding.binding_revision,
        expected_body_sha256: Some(format!("{:x}", hash.finalize())),
    };
    let mut result = LocalTagReadView {
        selection,
        size_bytes: count,
        modified_ms: after
            .metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|time| time.as_millis().min(u64::MAX as u128) as u64),
        title: None,
        artists: Vec::new(),
        album: None,
        lyrics: None,
        picture_count: 0,
        metadata_error: None,
    };
    match metadata {
        Ok(metadata) => {
            result.title = metadata.title;
            result.artists = metadata.artists;
            result.album = metadata.album;
            result.lyrics = metadata
                .embedded_lyrics
                .and_then(|bundle| bundle.lyric.text().map(str::to_owned));
            result.picture_count = metadata.pictures.len();
        }
        Err(error) => result.metadata_error = Some(tag_error(error)),
    }
    context.check()?;
    file.check()?;
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn request_digest_covers_field_actions_and_every_cover_byte() {
        let mut patch = LocalTagEditPatch {
            title: TagEditField::Set("title".into()),
            cover: TagEditField::Set(vec![1, 2, 3]),
            ..Default::default()
        };
        let original = patch.digest().unwrap();
        patch.cover = TagEditField::Set(vec![1, 2, 4]);
        assert_ne!(original, patch.digest().unwrap());
        patch.cover = TagEditField::Set(vec![1, 2, 3]);
        patch.title = TagEditField::Remove;
        assert_ne!(original, patch.digest().unwrap());
        patch.title = TagEditField::Keep;
        assert_ne!(original, patch.digest().unwrap());
    }
    #[test]
    fn unknown_patch_fields_and_bounded_text_are_rejected_before_native_work() {
        assert!(
            serde_json::from_value::<LocalTagEditPatch>(serde_json::json!({"autogrant":true}))
                .is_err()
        );
        let patch = LocalTagEditPatch {
            title: TagEditField::Set("bad\0title".into()),
            ..Default::default()
        };
        assert!(patch.validate().is_err());
        let patch = LocalTagEditPatch {
            artists: TagEditField::Set(vec!["artist".into(); MAX_TAG_ARTISTS + 1]),
            ..Default::default()
        };
        assert!(patch.validate().is_err());
    }
}

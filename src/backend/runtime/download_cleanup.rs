use super::*;
use crate::download::{
    DestinationGrant, DownloadCancel, StopReason, VerifiedOwnedAudio,
    cleanup::{
        CleanupBoundary, CleanupCoordinator, CleanupIntent, CleanupJournal, CleanupOwner,
        CleanupPhase, NativeRemovedReceipt,
    },
    task_model::{DownloadTask, StoredDestinationGrant},
};

pub(super) enum CleanupWork {
    Live {
        intent: CleanupIntent,
        audio: Option<Box<VerifiedOwnedAudio>>,
    },
    Recover {
        intent: CleanupIntent,
        destination: StoredDestinationGrant,
    },
    Owner(Box<CleanupOwner>),
}
impl CleanupWork {
    fn original_for_recovery_after_join(
        &mut self,
    ) -> Result<Option<crate::download::DownloadProducerObject>> {
        match self {
            Self::Live { audio, .. } => Ok(audio.as_ref().map(|audio| audio.producer_object())),
            Self::Recover { .. } => Ok(None),
            Self::Owner(owner) => owner.original_for_recovery_after_join(),
        }
    }
    fn return_recovery_borrows(&mut self) {
        if let Self::Owner(owner) = self {
            owner.return_recovery_borrows();
        }
        if let Self::Live { audio, .. } = self {
            audio.take();
        }
    }
    fn intent(&self) -> &CleanupIntent {
        match self {
            Self::Live { intent, .. } | Self::Recover { intent, .. } => intent,
            Self::Owner(owner) => owner.intent(),
        }
    }
    fn advance(
        &mut self,
        profile: &std::path::Path,
        coordinator: &RepositoryCleanup,
        cancel: &DownloadCancel,
        final_authorization: Option<crate::library::authority::FileAuthorization>,
    ) -> Result<()> {
        match self {
            Self::Live { intent, audio } => {
                let journal = CleanupJournal::open(profile)?;
                let owner = CleanupOwner::from_live_with_cancel(
                    journal,
                    intent.clone(),
                    *audio.take().context("Cleanup original is unavailable")?,
                    cancel.clone(),
                );
                *self = Self::Owner(Box::new(owner));
            }
            Self::Recover {
                intent,
                destination,
            } => {
                let grant = DestinationGrant::open_selected(&destination.path, destination.epoch)?;
                ensure!(
                    grant.identity() == &destination.identity,
                    "Cleanup selected directory identity changed"
                );
                let journal = CleanupJournal::open(profile)?;
                *self = Self::Owner(Box::new(CleanupOwner::recover_with_cancel(
                    journal,
                    intent.clone(),
                    grant,
                    cancel.clone(),
                )));
            }
            Self::Owner(_) => {}
        }
        let Self::Owner(owner) = self else {
            unreachable!()
        };
        if let Some(authorization) = final_authorization {
            owner.set_final_authorization(authorization);
        }
        ensure!(
            owner.advance(coordinator)? == CleanupPhase::Acknowledged,
            "Cleanup has not reached its acknowledged phase"
        );
        Ok(())
    }
}
struct PendingCleanup {
    work: Box<CleanupWork>,
    next_attempt: Instant,
    operation: Option<String>,
}
#[derive(Default)]
pub(super) struct CleanupRuntime {
    pending: HashMap<String, PendingCleanup>,
    active: HashSet<String>,
    cancels: HashMap<String, DownloadCancel>,
    closing: Arc<AtomicBool>,
    retirement: Vec<(Box<CleanupWork>, Option<String>)>,
    retirement_pins: HashMap<String, crate::library::authority::DownloadedPinReceipt>,
    operations: HashMap<String, String>,
    errors: HashMap<String, String>,
}
#[derive(Clone, Debug, serde::Serialize)]
pub struct DownloadCleanupView {
    pub task_id: String,
    pub active: bool,
    pub pending: bool,
    pub cancelled: bool,
    pub accounting_removed: bool,
    pub error: Option<String>,
}
struct RepositoryCleanup {
    repository: Arc<Repository>,
    cancel: DownloadCancel,
    closing: Arc<AtomicBool>,
}
impl RepositoryCleanup {
    fn check(&self) -> Result<()> {
        ensure!(
            !self.closing.load(Ordering::Acquire) && self.cancel.reason().is_none(),
            "Cleanup operation stopped"
        );
        Ok(())
    }
}
impl CleanupCoordinator for RepositoryCleanup {
    fn validate(&self, intent: &CleanupIntent, boundary: CleanupBoundary) -> Result<()> {
        self.repository.validate_download_cleanup_intent(
            intent,
            boundary == CleanupBoundary::Acknowledge,
            &|| self.check(),
        )
    }
    fn acknowledge(&self, receipt: &NativeRemovedReceipt) -> Result<()> {
        self.repository
            .acknowledge_removed_download_original_checked(
                &receipt.intent().allocation.context.task_id,
                receipt.intent().allocation.context.operation_epoch,
                receipt.removed_record(),
                &|| self.check(),
            )
    }
}
impl Runtime {
    pub(super) fn cancel_download_cleanup(&mut self, id: String, revision: u64) -> Result<()> {
        let task = self
            .repository
            .download_task(&id)?
            .context("Cleanup task is unavailable")?;
        ensure!(task.revision == revision, "Cleanup task revision conflict");
        let cancel = self
            .download_cleanup
            .cancels
            .get(&id)
            .context("Cleanup is not active or pending")?;
        cancel.stop(StopReason::Cancel);
        if let Some(pending) = self.download_cleanup.pending.remove(&id) {
            self.retire_cleanup(pending.work, pending.operation);
        }
        Ok(())
    }
    pub(super) fn start_recovered_cleanup(
        &mut self,
        task: &DownloadTask,
        operation: Option<String>,
    ) -> Result<bool> {
        let output = task
            .output_audio
            .as_ref()
            .context("Cleanup output is unavailable")?;
        let artifacts = self.repository.download_owned_artifacts(Some(&task.id))?;
        let Some(artifact) = artifacts.iter().find(|artifact| {
            artifact.allocation.kind == crate::download::artifact::OwnedArtifactKind::Transfer
                && artifact.allocation.record.identity != output.identity
                && artifact.phase == "verified"
        }) else {
            return Ok(false);
        };
        let intent = cleanup_intent(task, artifact)?;
        let destination = self
            .repository
            .download_destinations()?
            .into_iter()
            .find(|grant| grant.id == task.destination.grant_id)
            .context("Cleanup destination is unavailable")?;
        task.require_current_destination(&destination)?;
        self.submit_cleanup(
            task.id.clone(),
            Box::new(CleanupWork::Recover {
                intent,
                destination,
            }),
            operation,
        )?;
        Ok(true)
    }
    pub(super) fn start_original_cleanup(
        &mut self,
        task: &DownloadTask,
        artifact: &crate::repository::DownloadOwnedArtifact,
        audio: Box<VerifiedOwnedAudio>,
    ) -> Result<()> {
        let intent = cleanup_intent(task, artifact)?;
        self.submit_cleanup(
            task.id.clone(),
            Box::new(CleanupWork::Live {
                intent,
                audio: Some(audio),
            }),
            None,
        )
    }
    pub(super) fn recover_download_cleanup(
        &mut self,
        id: String,
        expected_revision: u64,
    ) -> Result<()> {
        let task = self
            .repository
            .download_task(&id)?
            .context("Cleanup task is unavailable")?;
        ensure!(
            task.revision == expected_revision
                && task.audio_available
                && task.commit_journal.is_none(),
            "Cleanup recovery intent changed"
        );
        ensure!(
            !self.download_cleanup.active.contains(&id),
            "Cleanup is already active"
        );
        if let Some(pending) = self.download_cleanup.pending.remove(&id) {
            return self.submit_cleanup(id, pending.work, Some(self.request_id.clone()));
        }
        let output = task
            .output_audio
            .as_ref()
            .context("Cleanup output is unavailable")?;
        let artifacts = self.repository.download_owned_artifacts(Some(&id))?;
        let artifact = artifacts
            .iter()
            .find(|artifact| {
                artifact.allocation.kind == crate::download::artifact::OwnedArtifactKind::Transfer
                    && artifact.allocation.record.identity != output.identity
                    && matches!(artifact.phase.as_str(), "verified" | "removed")
            })
            .context("Cleanup has no distinct retained original allocation")?;
        let intent = cleanup_intent(&task, artifact)?;
        let destination = self
            .repository
            .download_destinations()?
            .into_iter()
            .find(|grant| grant.id == task.destination.grant_id)
            .context("Cleanup destination is unavailable")?;
        task.require_current_destination(&destination)?;
        self.submit_cleanup(
            id,
            Box::new(CleanupWork::Recover {
                intent,
                destination,
            }),
            Some(self.request_id.clone()),
        )
    }
    fn submit_cleanup(
        &mut self,
        id: String,
        work: Box<CleanupWork>,
        operation: Option<String>,
    ) -> Result<()> {
        ensure!(
            !self.download_cleanup.closing.load(Ordering::Acquire),
            "Cleanup admission is closed"
        );
        ensure!(
            !self.download_cleanup.active.contains(&id)
                && self.download_cleanup.active.len() + self.download_cleanup.pending.len() < 4,
            "Cleanup capacity reached"
        );
        let cancel = self
            .download_cleanup
            .cancels
            .entry(id.clone())
            .or_default()
            .clone();
        if let Some(operation) = &operation {
            self.download_cleanup
                .operations
                .insert(operation.clone(), id.clone());
        }
        self.spawn_cleanup(id, work, cancel, operation)
    }
    fn spawn_cleanup(
        &mut self,
        id: String,
        work: Box<CleanupWork>,
        cancel: DownloadCancel,
        operation: Option<String>,
    ) -> Result<()> {
        if self.tasks.len() >= 8 {
            self.downloads.note_cleanup_start(id.clone());
            self.download_cleanup.pending.insert(
                id,
                PendingCleanup {
                    work,
                    next_attempt: Instant::now() + Duration::from_secs(5),
                    operation: None,
                },
            );
            anyhow::bail!("Too many pending cleanup workers");
        }
        let repository = self.repository.clone();
        let final_path = work
            .intent()
            .allocation
            .working_directory_path
            .join(&work.intent().final_output.basename);
        let final_authorization = self
            .repository
            .library_asset_at_path(&final_path)?
            .map(|asset| self.library.authority.download_authorization(&asset.id))
            .transpose()?
            .flatten();
        if final_authorization.is_none()
            && (self.download_reads.jobs.contains_key(&id)
                || self.download_reads.recovering.contains_key(&id))
        {
            self.downloads.note_cleanup_start(id.clone());
            self.download_cleanup.pending.insert(
                id,
                PendingCleanup {
                    work,
                    next_attempt: Instant::now() + Duration::from_millis(20),
                    operation,
                },
            );
            return Ok(());
        }
        let profile = repository
            .path()
            .parent()
            .context("Cleanup profile directory is unavailable")?
            .to_path_buf();
        let closing = self.download_cleanup.closing.clone();
        let tx = self.completed_tx.clone();
        let task_id = id.clone();
        let slot = Arc::new(Mutex::new(Some(work)));
        let worker_slot = slot.clone();
        let spawned = thread::Builder::new()
            .name("download-durable-cleanup".into())
            .spawn(move || {
                let mut work = worker_slot
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .take()
                    .expect("Owned cleanup task");
                let coordinator = RepositoryCleanup {
                    repository,
                    cancel: cancel.clone(),
                    closing,
                };
                let result = coordinator.check().and_then(|_| {
                    work.advance(&profile, &coordinator, &cancel, final_authorization)
                });
                let remaining = if result.is_ok() {
                    drop(work);
                    None
                } else {
                    Some(work)
                };
                let _ = tx.send(Completion::DownloadCleanupFinished {
                    task_id,
                    operation,
                    work: remaining,
                    result,
                });
            });
        let worker = match spawned {
            Ok(worker) => worker,
            Err(error) => {
                if let Some(work) = slot
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .take()
                {
                    self.downloads.note_cleanup_start(id.clone());
                    self.download_cleanup.pending.insert(
                        id,
                        PendingCleanup {
                            work,
                            next_attempt: Instant::now() + Duration::from_secs(5),
                            operation: None,
                        },
                    );
                }
                return Err(error.into());
            }
        };
        self.download_cleanup.active.insert(id.clone());
        self.downloads.note_cleanup_start(id);
        self.tasks.push(worker);
        Ok(())
    }
    pub(super) fn finish_cleanup(
        &mut self,
        id: String,
        operation: Option<String>,
        work: Option<Box<CleanupWork>>,
        result: Result<()>,
    ) {
        self.download_cleanup.active.remove(&id);
        let cancelled = self
            .download_cleanup
            .cancels
            .get(&id)
            .is_some_and(|cancel| cancel.reason().is_some());
        if let Some(work) = work {
            if self.download_cleanup.closing.load(Ordering::Acquire) || cancelled {
                self.retire_cleanup(work, operation);
                return;
            } else {
                self.download_cleanup.pending.insert(
                    id.clone(),
                    PendingCleanup {
                        work,
                        next_attempt: Instant::now() + Duration::from_secs(5),
                        operation: None,
                    },
                );
            }
        } else {
            self.downloads.finish_original_cleanup(&id);
            self.download_cleanup.cancels.remove(&id);
        }
        if let Some(operation) = operation {
            self.download_cleanup.operations.remove(&operation);
            self.publish_finished(operation, &result);
        }
        if let Err(error) = result {
            self.download_cleanup.errors.insert(
                id.clone(),
                format!("{error:#}").chars().take(1024).collect(),
            );
            self.error(format!("Download cleanup remains pending: {error:#}"));
        } else {
            self.download_cleanup.errors.remove(&id);
        }
    }
    pub(super) fn poll_cleanup(&mut self) {
        self.retry_cleanup_retirement();
        if self.download_cleanup.closing.load(Ordering::Acquire) || self.tasks.len() >= 4 {
            return;
        }
        let ready = self
            .download_cleanup
            .pending
            .iter()
            .find(|(_, pending)| pending.next_attempt <= Instant::now())
            .map(|(id, _)| id.clone());
        if let Some(id) = ready
            && let Some(pending) = self.download_cleanup.pending.remove(&id)
        {
            let cancel = self
                .download_cleanup
                .cancels
                .entry(id.clone())
                .or_default()
                .clone();
            if let Err(error) = self.spawn_cleanup(id, pending.work, cancel, pending.operation) {
                self.error(format!("Cleanup worker could not start: {error:#}"));
            }
        }
    }
    pub(super) fn begin_cleanup_shutdown(&mut self) {
        self.download_cleanup.closing.store(true, Ordering::Release);
        for cancel in self.download_cleanup.cancels.values() {
            cancel.stop(StopReason::Shutdown);
        }
        let pending = std::mem::take(&mut self.download_cleanup.pending);
        for (_, pending) in pending {
            self.retire_cleanup(pending.work, pending.operation);
        }
    }
    fn retire_cleanup(&mut self, mut work: Box<CleanupWork>, operation: Option<String>) {
        let task_id = work.intent().allocation.context.task_id.clone();
        self.download_cleanup.active.insert(task_id.clone());
        let retired = (|| -> Result<bool> {
            if let Some(original) = work.original_for_recovery_after_join()? {
                if !original.producers_joined()? {
                    return Ok(false);
                }
                if !self.download_cleanup.retirement_pins.contains_key(&task_id) {
                    let pins = self
                        .library
                        .authority
                        .stage_download_pin_retirement(&original)?;
                    self.download_cleanup
                        .retirement_pins
                        .insert(task_id.clone(), pins);
                }
                let pins = self
                    .download_cleanup
                    .retirement_pins
                    .get(&task_id)
                    .context("Cleanup actual original native pins receiver missing")?;
                if !original.close_for_cold_recovery_after_join(Some(pins))? {
                    return Ok(false);
                }
            }
            work.return_recovery_borrows();
            Ok(true)
        })();
        match retired {
            Ok(true) => {
                drop(work);
                self.download_cleanup.retirement_pins.remove(&task_id);
                self.finish_cleanup_retired(task_id, operation);
            }
            Ok(false) => self.download_cleanup.retirement.push((work, operation)),
            Err(error) => {
                self.download_cleanup.retirement.push((work, operation));
                self.error(format!(
                    "Cleanup recovery retirement retains its actual original: {error:#}"
                ));
            }
        }
    }
    pub(super) fn retry_cleanup_retirement(&mut self) -> bool {
        let pending = std::mem::take(&mut self.download_cleanup.retirement);
        for (work, operation) in pending {
            self.retire_cleanup(work, operation);
        }
        self.download_cleanup.retirement.is_empty()
    }
    pub(super) fn cancel_cleanup(&self, id: &str) {
        if let Some(cancel) = self.download_cleanup.cancels.get(id) {
            cancel.stop(StopReason::AuthorityRevoked);
        }
    }
    pub(super) fn cancel_cleanup_operation(&mut self, operation: &str) {
        let Some(id) = self.download_cleanup.operations.get(operation).cloned() else {
            return;
        };
        if let Some(cancel) = self.download_cleanup.cancels.get(&id) {
            cancel.stop(StopReason::Cancel);
        }
        if let Some(pending) = self.download_cleanup.pending.remove(&id) {
            self.retire_cleanup(pending.work, Some(operation.into()));
        }
    }
    pub(super) fn finish_cleanup_retired(&mut self, id: String, operation: Option<String>) {
        self.download_cleanup.active.remove(&id);
        self.download_cleanup.cancels.remove(&id);
        self.downloads.finish_original_cleanup(&id);
        if let Some(operation) = operation {
            self.download_cleanup.operations.remove(&operation);
            self.publish_finished(
                operation,
                &Err(BackendError::new(
                    ErrorKind::Cancelled,
                    "Cleanup cancelled; its recovery journal and accounting remain available",
                )
                .into()),
            );
        }
    }
    pub(super) fn cleanup_view(&self, id: String) -> Result<DownloadCleanupView> {
        ensure!(
            self.repository.download_task(&id)?.is_some(),
            "Cleanup task is unavailable"
        );
        let accounting_removed = self
            .repository
            .download_owned_artifacts(Some(&id))?
            .iter()
            .any(|artifact| {
                artifact.allocation.kind == crate::download::artifact::OwnedArtifactKind::Transfer
                    && artifact.phase == "removed"
            });
        Ok(DownloadCleanupView {
            active: self.download_cleanup.active.contains(&id),
            pending: self.download_cleanup.pending.contains_key(&id),
            cancelled: self
                .download_cleanup
                .cancels
                .get(&id)
                .is_some_and(|cancel| cancel.reason().is_some()),
            accounting_removed,
            error: self.download_cleanup.errors.get(&id).cloned(),
            task_id: id,
        })
    }
    pub(super) fn forget_cleanup_status(&mut self, id: &str) {
        self.download_cleanup.errors.remove(id);
    }
}
fn cleanup_intent(
    task: &DownloadTask,
    artifact: &crate::repository::DownloadOwnedArtifact,
) -> Result<CleanupIntent> {
    let checkpoint = task
        .original_transfer
        .as_ref()
        .context("Cleanup original checkpoint is unavailable")?;
    let intent = CleanupIntent {
        allocation: artifact.allocation.clone(),
        raw_bytes: checkpoint.bytes,
        raw_sha256: checkpoint.sha256.clone(),
        final_output: task
            .output_audio
            .clone()
            .context("Cleanup durable output is unavailable")?,
        task_revision: task.revision,
    };
    intent.validate()?;
    Ok(intent)
}

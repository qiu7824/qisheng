use super::*;
use crate::download::{
    CollisionPolicy, DestinationGrant, DownloadState,
    planning::{DownloadQuality, safe_filename},
    task_model::*,
};
use std::time::{SystemTime, UNIX_EPOCH};

mod pending_postprocess;
mod producer_workers;
mod quiesce;
use producer_workers::{DeferredProducerCompletion, ProducerKey};

pub(super) enum PreparedAudioOutput {
    Raw(Box<crate::download::VerifiedOwnedAudio>),
    Tagged(Box<crate::download::postprocess::PreparedTaggedOutput>),
}
impl PreparedAudioOutput {
    fn producer_object(&self) -> crate::download::DownloadProducerObject {
        match self {
            Self::Raw(audio) => audio.producer_object(),
            Self::Tagged(audio) => audio.producer_object(),
        }
    }
    fn record(&self) -> &crate::download::OwnedPartRecord {
        match self {
            Self::Raw(audio) => audio.owned_record(),
            Self::Tagged(audio) => audio.owned_record(),
        }
    }
    fn digest(&self) -> (u64, String) {
        match self {
            Self::Raw(audio) => (audio.checkpoint().bytes, audio.checkpoint().sha256.clone()),
            Self::Tagged(audio) => (
                audio.output_digest().bytes,
                audio.output_digest().sha256.clone(),
            ),
        }
    }
    fn commit(self) -> Result<crate::download::CommittedAudio> {
        match self {
            Self::Raw(audio) => (*audio).commit(),
            Self::Tagged(audio) => Ok((*audio).commit()?.output),
        }
    }
}
pub(super) struct PreparedDownloadOutput {
    audio: PreparedAudioOutput,
    sidecar: Option<crate::download::postprocess::PreparedLyricSidecar>,
    original_audio: Option<Box<crate::download::VerifiedOwnedAudio>>,
    original_transfer: crate::download::TransferCheckpoint,
    original_part: crate::download::OwnedPartRecord,
    stages: DownloadStageRecords,
}
impl PreparedDownloadOutput {
    fn producer_objects(&self) -> Vec<crate::download::DownloadProducerObject> {
        let mut objects = vec![self.audio.producer_object()];
        if let Some(sidecar) = &self.sidecar {
            objects.push(sidecar.producer_object());
        }
        if let Some(original) = &self.original_audio {
            objects.push(original.producer_object());
        }
        objects
    }
}
pub(super) struct RecoveredDownloadOutput {
    audio: DownloadArtifactProof,
    audio_facts: crate::download::CommittedAudio,
    sidecar: Option<DownloadArtifactProof>,
    sidecar_error: Option<crate::download::DownloadError>,
    owners: Vec<crate::download::DownloadProducerObject>,
}
struct StageControl {
    cancel: crate::download::DownloadCancel,
    metadata_cancel: Arc<AtomicBool>,
    deadline: Instant,
    source_bound: bool,
}
struct RetryJob {
    operation: String,
    worker: Option<crate::audio::worker_owner::TrackedWorker>,
    result: Arc<Mutex<Option<crate::download::postprocess_retry::RetryWorkResult>>>,
    retained: Option<crate::download::postprocess_retry::RetryWorkResult>,
}
struct PendingPostprocess {
    audio: Box<crate::download::VerifiedOwnedAudio>,
    task: DownloadTask,
    retirement_pins: Option<crate::library::authority::DownloadedPinReceipt>,
    error: Option<String>,
}
impl StageControl {
    fn stop(&self, reason: crate::download::StopReason) {
        self.cancel.stop(reason);
        self.metadata_cancel.store(true, Ordering::Release);
    }
}
struct DownloadArtifactJournal {
    repository: Arc<Repository>,
    cancel: crate::download::DownloadCancel,
    lease: crate::download::DownloadExecutionLease,
}
struct RebindDownloadArtifactJournal {
    journal: DownloadArtifactJournal,
    checkpoint: crate::download::TransferCheckpoint,
}
impl crate::download::artifact::OwnedArtifactJournal for RebindDownloadArtifactJournal {
    fn record_created(
        &self,
        allocation: &crate::download::artifact::OwnedArtifactAllocation,
    ) -> Result<()> {
        let check = || -> Result<()> {
            ensure!(
                self.journal.cancel.reason().is_none(),
                "Download artifact operation stopped"
            );
            self.journal.lease.check_context(&allocation.context)?;
            Ok(())
        };
        self.journal.repository.rebind_download_transfer_artifact(
            allocation,
            &self.checkpoint,
            &check,
        )
    }
}
impl crate::download::artifact::OwnedArtifactJournal for DownloadArtifactJournal {
    fn record_intent(&self, intent: &crate::download::artifact::OwnedArtifactIntent) -> Result<()> {
        self.repository
            .record_download_allocation_intent(intent, &|| {
                ensure!(
                    self.cancel.reason().is_none(),
                    "Download artifact operation stopped"
                );
                self.lease.check_context(&intent.context)
            })
    }
    fn record_reopened(
        &self,
        allocation: &crate::download::artifact::OwnedArtifactAllocation,
    ) -> Result<()> {
        self.repository
            .acknowledge_download_artifact_reopen(allocation, &|| {
                ensure!(
                    self.cancel.reason().is_none(),
                    "Download artifact operation stopped"
                );
                self.lease.check_context(&allocation.context)
            })
    }
    fn record_created(
        &self,
        allocation: &crate::download::artifact::OwnedArtifactAllocation,
    ) -> Result<()> {
        let check = || -> Result<()> {
            ensure!(
                self.cancel.reason().is_none(),
                "Download artifact operation stopped"
            );
            self.lease.check_context(&allocation.context)?;
            Ok(())
        };
        self.repository.record_download_artifact(allocation, &check)
    }
}

#[derive(Default)]
pub(super) struct DownloadRuntime {
    producer_workers: HashMap<ProducerKey, crate::audio::worker_owner::TrackedWorker>,
    producer_objects: HashMap<ProducerKey, Vec<crate::download::DownloadProducerObject>>,
    deferred_producers: VecDeque<DeferredProducerCompletion>,
    resolve_queue: VecDeque<String>,
    resolving: Option<(String, u64, Arc<AtomicBool>)>,
    pending: HashMap<String, crate::download::ApprovedDownloadUrl>,
    leases: HashMap<String, crate::download::DownloadExecutionLease>,
    pool: Option<crate::download::DownloadPool>,
    active: HashMap<String, crate::download::DownloadHandle>,
    preparing: HashSet<String>,
    staging: HashMap<String, StageControl>,
    postprocess_queue: VecDeque<PendingPostprocess>,
    auxiliary_tasks: HashSet<String>,
    checkpoint_at: HashMap<String, Instant>,
    cleaning: HashSet<String>,
    retry_jobs: HashMap<String, RetryJob>,
    quiesce_pins: Vec<crate::library::authority::DownloadedPinReceipt>,
    closing: bool,
}
impl DownloadRuntime {
    pub(super) fn note_cleanup_start(&mut self, task_id: String) {
        self.cleaning.insert(task_id);
    }
    pub(super) fn finish_original_cleanup(&mut self, task_id: &str) {
        self.cleaning.remove(task_id);
    }
    pub(super) fn require_no_cleanup(&self, task_id: &str) -> Result<()> {
        ensure!(
            !self.cleaning.contains(task_id),
            "Download original cleanup is still active"
        );
        Ok(())
    }
}

impl Runtime {
    pub(super) fn retry_download_postprocessing(&mut self, id: String) -> Result<()> {
        self.start_download_postprocessing_retry(id, self.request_id.clone())
    }
    pub(super) fn cancel_download_postprocessing(&mut self, operation: &str) -> bool {
        let task = self
            .downloads
            .retry_jobs
            .iter()
            .find(|(_, job)| job.operation == operation && job.worker.is_some())
            .map(|(id, _)| id.clone());
        if let Some(id) = task
            && let Some(control) = self.downloads.staging.get(&id)
        {
            control.stop(crate::download::StopReason::Cancel);
            return true;
        }
        false
    }
    fn start_download_postprocessing_retry(&mut self, id: String, operation: String) -> Result<()> {
        ensure!(!self.downloads.closing, "Download service is closing");
        self.downloads.require_no_cleanup(&id)?;
        ensure!(
            self.downloads.auxiliary_tasks.is_empty()
                && self.downloads.postprocess_queue.is_empty()
                && (self.downloads.retry_jobs.is_empty()
                    || self.downloads.retry_jobs.contains_key(&id)),
            BackendError::new(
                ErrorKind::Busy,
                "Download auxiliary native objects are still owned"
            )
        );
        ensure!(
            !self.downloads.preparing.contains(&id)
                && !self.downloads.active.contains_key(&id)
                && !self.downloads.staging.contains_key(&id),
            BackendError::new(ErrorKind::Busy, "Download still owns active work")
        );
        ensure!(
            self.downloads.active.len()
                + self.downloads.preparing.len()
                + self.downloads.staging.len()
                + self.downloads.cleaning.len()
                < 4,
            BackendError::new(ErrorKind::Busy, "Download postprocessing capacity reached")
        );
        ensure!(
            self.downloads.retry_jobs.len() < 4 || self.downloads.retry_jobs.contains_key(&id),
            BackendError::new(
                ErrorKind::Busy,
                "Download retry retained-owner capacity reached"
            )
        );
        let mut task = self
            .repository
            .download_task(&id)?
            .context("Download task is unavailable")?;
        ensure!(
            self.downloads.active.len()
                + self.downloads.preparing.len()
                + self.downloads.staging.len()
                + self.downloads.cleaning.len()
                < task.settings.max_concurrent,
            BackendError::new(
                ErrorKind::Busy,
                "Download configured concurrency capacity reached"
            )
        );
        ensure!(
            task.audio_available
                && task.output_audio.is_some()
                && matches!(task.state, DownloadState::Paused | DownloadState::Failed),
            "Choose a published download with unfinished postprocessing"
        );
        let stored = self
            .repository
            .download_destinations()?
            .into_iter()
            .find(|g| g.id == task.destination.grant_id)
            .context("Download destination is unavailable")?;
        task.require_current_destination(&stored)?;
        let old_revision = task.revision;
        let old_epoch = task.operation_epoch;
        let pending = task
            .commit_journal
            .as_ref()
            .and_then(|j| j.retry.as_ref())
            .is_some();
        ensure!(
            task.commit_journal.is_none() || pending,
            "Recover the pending download publication first"
        );
        task.operation_epoch = old_epoch
            .checked_add(1)
            .context("Download retry epoch overflow")?;
        task.revision = old_revision
            .checked_add(1)
            .context("Download retry revision overflow")?;
        if !pending {
            let intent =
                crate::download::postprocess_retry::DownloadPostprocessRetryIntent::new(&task)?;
            let output = task.output_audio.as_ref().unwrap();
            let published = self
                .repository
                .download_owned_artifacts(Some(&id))?
                .into_iter()
                .find(|a| {
                    a.phase == "published"
                        && a.allocation.record.identity == output.identity
                        && a.actual_bytes == output.bytes
                        && a.sha256.as_deref() == Some(output.sha256.as_str())
                })
                .context("Published download has no matching durable producer record")?;
            intent.selected.begin(&mut task.stages)?;
            task.commit_journal = Some(DownloadCommitJournal {
                task_revision: intent.task_revision,
                operation_epoch: intent.operation_epoch,
                grant_epoch: task.destination.grant_epoch,
                phase: DownloadCommitPhase::PostprocessRetry,
                original_part: published.allocation.record,
                candidate: output.clone(),
                observed_target: Some(output.clone()),
                rollback_original: None,
                sidecar: task.stages.lyrics.artifact.clone(),
                retry: Some(intent),
            });
        }
        task.state = DownloadState::PostProcessing;
        task.pause_reason = None;
        task.error = None;
        task.updated_at_ms = download_now()?;
        self.revision = self
            .repository
            .update_download_task(&task, old_revision, old_epoch, Some(self.revision), &|| {
                Ok(())
            })?
            .revision;
        let retained = self
            .downloads
            .retry_jobs
            .remove(&id)
            .and_then(|mut old| old.retained.take());
        let cancel = crate::download::DownloadCancel::default();
        let metadata_cancel = Arc::new(AtomicBool::new(false));
        let result = Arc::new(Mutex::new(None));
        let destination = result.clone();
        let repository = self.repository.clone();
        let providers = self.provider_registry.clone();
        let worker_cancel = cancel.clone();
        let worker_metadata = metadata_cancel.clone();
        let retry_epoch = task.operation_epoch;
        self.downloads.spawn_producer(
            "download-postprocess-retry",
            (id.clone(), retry_epoch, "retry"),
            vec![],
            move || {
                let outcome = crate::download::postprocess_retry::execute_retry(
                    repository,
                    providers,
                    task,
                    stored,
                    worker_cancel,
                    worker_metadata,
                    retained,
                );
                *destination.lock().unwrap_or_else(|e| e.into_inner()) = Some(outcome);
            },
        )?;
        let worker = self
            .downloads
            .producer_workers
            .get(&(id.clone(), retry_epoch, "retry"))
            .context("Actual retry producer worker disappeared")?
            .clone();
        self.downloads.staging.insert(
            id.clone(),
            StageControl {
                cancel,
                metadata_cancel,
                deadline: Instant::now() + Duration::from_secs(45),
                source_bound: false,
            },
        );
        self.downloads.retry_jobs.insert(
            id,
            RetryJob {
                operation,
                worker: Some(worker),
                result,
                retained: None,
            },
        );
        self.async_started = true;
        Ok(())
    }
    fn poll_download_postprocessing_retries(&mut self) {
        let completed = self
            .downloads
            .retry_jobs
            .iter()
            .filter(|(_, job)| job.worker.as_ref().is_some_and(|w| w.wait(Duration::ZERO)))
            .map(|(id, _)| id.clone())
            .take(4)
            .collect::<Vec<_>>();
        for id in completed {
            let Some(mut job) = self.downloads.retry_jobs.remove(&id) else {
                continue;
            };
            let worker = job.worker.take().unwrap();
            let mut outcome = job.result.lock().unwrap_or_else(|e| e.into_inner()).take();
            let finish = (|| -> Result<bool> {
                ensure!(
                    worker.wait(Duration::ZERO) && !worker.panicked(),
                    "Download postprocessing retry worker panicked or has not joined"
                );
                let result = outcome
                    .as_mut()
                    .context("Download postprocessing retry returned no outcome")?;
                if let Err(error) = result.finish_retirements_after_join(&self.repository) {
                    result.retirement_complete = false;
                    result.error = Some(error);
                }
                let mut task = self
                    .repository
                    .download_task(&id)?
                    .context("Download retry task disappeared")?;
                let Some(intent) = task
                    .commit_journal
                    .as_ref()
                    .and_then(|j| j.retry.as_ref())
                    .cloned()
                else {
                    anyhow::bail!("Download retry intent disappeared");
                };
                ensure!(
                    result
                        .task
                        .commit_journal
                        .as_ref()
                        .and_then(|j| j.retry.as_ref())
                        .is_some_and(|i| i.operation_id == intent.operation_id),
                    "Download retry result refers to a changed operation"
                );
                let old_revision = task.revision;
                let epoch = task.operation_epoch;
                task.stages = result.stages.clone();
                if let Some(sidecar) = &result.sidecar {
                    task.stages.lyrics.artifact = Some(sidecar.clone());
                }
                let paused = !task.stages.all_satisfied()
                    && (task.state == DownloadState::Pausing
                        || self.downloads.closing
                        || self
                            .downloads
                            .staging
                            .get(&id)
                            .is_some_and(|c| c.cancel.reason().is_some()));
                if paused {
                    task.state = DownloadState::Paused;
                    task.pause_reason = Some(DownloadPauseReason::User);
                } else if let Some(error) = &result.error {
                    task.state = DownloadState::Failed;
                    task.error = Some(crate::download::DownloadError {
                        kind: crate::download::DownloadErrorKind::Verification,
                        message: format!("{error:#}").chars().take(1024).collect(),
                        http_status: None,
                    });
                } else if task.stages.all_satisfied() {
                    task.state = DownloadState::Completed;
                    task.error = None;
                } else {
                    task.state = DownloadState::Failed;
                    task.error = [
                        &task.stages.metadata,
                        &task.stages.cover,
                        &task.stages.embedded_lyrics,
                        &task.stages.lyrics,
                    ]
                    .into_iter()
                    .find_map(|s| s.error.clone());
                }
                for stage in [
                    &mut task.stages.metadata,
                    &mut task.stages.cover,
                    &mut task.stages.embedded_lyrics,
                    &mut task.stages.lyrics,
                ] {
                    if stage.state == DownloadStageState::Running {
                        stage.state = DownloadStageState::Pending;
                    }
                }
                let safe = result.retirement_complete && result.owner.is_none();
                let restart = safe && result.restart_required && !paused;
                if safe {
                    task.commit_journal = None;
                }
                if restart {
                    for (selected, stage, original) in [
                        (
                            intent.selected.metadata,
                            &mut task.stages.metadata,
                            &intent.original_stages.metadata,
                        ),
                        (
                            intent.selected.cover,
                            &mut task.stages.cover,
                            &intent.original_stages.cover,
                        ),
                        (
                            intent.selected.embedded_lyrics,
                            &mut task.stages.embedded_lyrics,
                            &intent.original_stages.embedded_lyrics,
                        ),
                        (
                            intent.selected.lyrics,
                            &mut task.stages.lyrics,
                            &intent.original_stages.lyrics,
                        ),
                    ] {
                        if selected
                            && matches!(
                                stage.state,
                                DownloadStageState::Pending
                                    | DownloadStageState::Running
                                    | DownloadStageState::Failed
                            )
                        {
                            stage.attempts = original.attempts;
                        }
                    }
                    task.state = DownloadState::Paused;
                    task.pause_reason = Some(DownloadPauseReason::Restart);
                }
                task.revision = task
                    .revision
                    .checked_add(1)
                    .context("Download retry revision overflow")?;
                task.updated_at_ms = download_now()?;
                self.revision = self.repository.revision()?;
                self.revision = self
                    .repository
                    .update_download_task(&task, old_revision, epoch, Some(self.revision), &|| {
                        Ok(())
                    })?
                    .revision;
                Ok(restart)
            })();
            self.downloads.staging.remove(&id);
            match finish {
                Ok(true) => {
                    if let Err(error) =
                        self.start_download_postprocessing_retry(id.clone(), job.operation.clone())
                    {
                        self.publish_finished(job.operation, &Err(error));
                    }
                }
                Ok(false) => {
                    let result = self.repository.download_task(&id).and_then(|task| {
                        let task = task.context("Download retry task disappeared")?;
                        if task.state == DownloadState::Completed {
                            Ok(())
                        } else {
                            Err(anyhow::anyhow!(
                                "{}",
                                task.error
                                    .as_ref()
                                    .map(|e| e.message.as_str())
                                    .unwrap_or("Download postprocessing has not completed")
                            ))
                        }
                    });
                    self.publish_finished(job.operation.clone(), &result);
                    if outcome
                        .as_ref()
                        .is_some_and(|r| !r.retirement_complete || r.owner.is_some())
                    {
                        job.retained = outcome;
                        self.downloads.retry_jobs.insert(id, job);
                    }
                }
                Err(error) => {
                    self.publish_finished(job.operation.clone(), &Err(error));
                    job.retained = outcome;
                    self.downloads.retry_jobs.insert(id, job);
                }
            }
        }
    }
    pub(super) fn recover_download_commit(
        &mut self,
        id: String,
        expected_revision: u64,
    ) -> Result<()> {
        let task = self
            .repository
            .download_task(&id)?
            .context("Download task is unavailable")?;
        ensure!(
            task.revision == expected_revision
                && matches!(task.state, DownloadState::Paused | DownloadState::Failed),
            "Download recovery task changed"
        );
        ensure!(
            !self.downloads.preparing.contains(&id)
                && !self.downloads.active.contains_key(&id)
                && !self.downloads.staging.contains_key(&id),
            "Download task still owns live work"
        );
        let journal = task
            .commit_journal
            .clone()
            .context("Download task has no pending commit journal")?;
        let stored = self
            .repository
            .download_destinations()?
            .into_iter()
            .find(|grant| grant.id == task.destination.grant_id)
            .context("Download destination is unavailable")?;
        task.require_current_destination(&stored)?;
        ensure!(self.tasks.len() < 4, "Too many pending recovery operations");
        let completed = self.completed_tx.clone();
        let operation = self.request_id.clone();
        let recovery_epoch = task.operation_epoch;
        let allocations = self.repository.download_owned_artifacts(Some(&id))?;
        let repository = self.repository.clone();
        self.downloads.spawn_producer("download-commit-recovery",
            (id.clone(), recovery_epoch, "recovery"), vec![], move || {
                    let result = (|| -> Result<_> {
                        let mut grant =
                            DestinationGrant::open_selected(&stored.path, stored.epoch)?;
                        ensure!(
                            grant.identity() == &stored.identity,
                            "Download recovery root object changed"
                        );
                        if task.destination.relative_components.len() == 2 {
                            grant = grant
                                .open_existing_group(&task.destination.relative_components[0])?;
                        }
                        let leaf = task
                            .destination
                            .relative_components
                            .last()
                            .context("Download leaf missing")?;
                        let cancel = crate::download::DownloadCancel::default();
                        let audio = DownloadArtifactProof {
                            basename: leaf.clone(), identity: journal.candidate.identity.clone(),
                            bytes: journal.candidate.bytes, sha256: journal.candidate.sha256.clone(),
                        };
                        let allocation = allocations.iter().find(|artifact| artifact.allocation.record.identity == audio.identity)
                            .context("Recovery audio has no durable original allocation")?;
                        let mut owners = vec![grant.recover_owned_artifact_retained(&allocation.allocation, &audio, &cancel,
                            &|actual, proof| repository.acknowledge_download_artifact_recovery(actual, proof, &|| cancel.check()))?];
                        let extension = std::path::Path::new(&audio.basename).extension()
                            .and_then(|extension| extension.to_str()).context("Recovered audio extension missing")?;
                        let audio_facts = owners[0].verify_published_audio(&audio, extension, &cancel)?;
                        let sidecar_result = journal
                            .sidecar
                            .as_ref()
                            .map(|sidecar| {
                                let allocation = allocations.iter().find(|artifact| artifact.allocation.record.identity == sidecar.identity)
                                    .context("Recovery sidecar has no durable original allocation")?;
                                owners.push(grant.recover_owned_artifact_retained(&allocation.allocation, sidecar, &cancel,
                                    &|actual, proof| repository.acknowledge_download_artifact_recovery(actual, proof, &|| cancel.check()))?);
                                Ok::<_, anyhow::Error>(sidecar.clone())
                            })
                            .transpose();
                        let (sidecar, sidecar_error) = match sidecar_result {
                            Ok(sidecar) => (sidecar, None),
                            Err(_) => (None, Some(crate::download::DownloadError {
                                kind: crate::download::DownloadErrorKind::Disk,
                                message: "Lyric sidecar recovery proof failed; published audio was preserved".into(),
                                http_status: None,
                            })),
                        };
                        Ok(Box::new(RecoveredDownloadOutput { audio, audio_facts, sidecar, sidecar_error, owners }))
                    })();
                    let _ = completed.send(Completion::DownloadRecoveryVerified {
                        operation,
                        task_id: id,
                        expected_task_revision: expected_revision,
                        operation_epoch: task.operation_epoch,
                        result,
                    });
                })?;
        self.async_started = true;
        Ok(())
    }
    pub(super) fn finish_download_recovery(
        &mut self,
        operation: String,
        id: String,
        expected_revision: u64,
        epoch: u64,
        result: Result<Box<RecoveredDownloadOutput>>,
    ) {
        if !self.downloads.producer_joined(&id, epoch, "recovery") {
            self.downloads
                .deferred_producers
                .push_back(DeferredProducerCompletion::Recovery(
                    operation,
                    id,
                    expected_revision,
                    epoch,
                    result,
                ));
            return;
        }
        let outcome = (|| -> Result<bool> {
            let mut task = self
                .repository
                .download_task(&id)?
                .context("Download task is unavailable")?;
            ensure!(
                task.revision == expected_revision
                    && task.operation_epoch == epoch
                    && task.commit_journal.is_some(),
                "Download recovery intent changed"
            );
            let RecoveredDownloadOutput {
                audio,
                audio_facts,
                sidecar,
                sidecar_error,
                owners,
            } = *result?;
            ensure!(
                owners
                    .iter()
                    .all(|owner| owner.producers_joined().unwrap_or(false)),
                "Actual recovery descriptor loans have not joined"
            );
            self.downloads
                .register_recovery_borrows(&id, epoch, owners)?;
            task.output_audio = Some(audio);
            task.audio_available = true;
            task.owned_part = None;
            if let Some(sidecar) = sidecar {
                task.stages.lyrics.state = DownloadStageState::Succeeded;
                task.stages.lyrics.artifact = Some(sidecar);
            }
            if let Some(error) = sidecar_error {
                task.stages.lyrics.state = DownloadStageState::Failed;
                task.stages.lyrics.error = Some(error);
                task.stages.lyrics.artifact = None;
            }
            task.commit_journal = None;
            let failed = [
                &task.stages.metadata,
                &task.stages.cover,
                &task.stages.embedded_lyrics,
                &task.stages.lyrics,
            ]
            .into_iter()
            .find(|stage| stage.state == DownloadStageState::Failed);
            task.state = if let Some(stage) = failed {
                task.error = stage.error.clone();
                DownloadState::Failed
            } else if task.stages.all_satisfied() {
                DownloadState::Completed
            } else {
                DownloadState::Paused
            };
            task.pause_reason =
                (task.state == DownloadState::Paused).then_some(DownloadPauseReason::UrlReapproval);
            task.revision += 1;
            task.updated_at_ms = download_now()?;
            let artifacts = self.repository.download_owned_artifacts(Some(&id))?;
            let mut published_artifacts = Vec::new();
            for proof in [
                task.output_audio.as_ref(),
                task.stages.lyrics.artifact.as_ref(),
            ]
            .into_iter()
            .flatten()
            {
                let artifact = artifacts
                    .iter()
                    .find(|artifact| artifact.allocation.record.identity == proof.identity)
                    .context("Recovered output has no matching durable ownership record")?;
                published_artifacts.push((
                    artifact.allocation.record.nonce.clone(),
                    artifact.allocation.context.operation_epoch,
                    proof.bytes,
                    proof.sha256.clone(),
                ));
            }
            self.revision = self
                .repository
                .commit_download_publication(
                    &task,
                    expected_revision,
                    epoch,
                    Some(self.revision),
                    &published_artifacts,
                    &|| Ok(()),
                )?
                .revision;
            self.begin_download_read_adoption(task.clone(), audio_facts)?;
            self.start_recovered_cleanup(&task, Some(operation.clone()))
        })();
        match outcome {
            Ok(true) => {}
            Ok(false) => self.publish_finished(operation, &Ok(())),
            Err(error) => self.publish_finished(operation, &Err(error)),
        }
    }
    pub(super) fn begin_download_shutdown(&mut self) {
        self.begin_download_read_shutdown();
        self.downloads.closing = true;
        for control in self.downloads.staging.values() {
            control.stop(crate::download::StopReason::Shutdown);
        }
        if let Some(pool) = &self.downloads.pool {
            pool.begin_shutdown();
        }
    }
    pub(super) fn finish_download_shutdown(&mut self) -> Result<()> {
        loop {
            let state = self
                .downloads
                .pool
                .as_ref()
                .map(|pool| pool.shutdown(Duration::from_millis(20)));
            if matches!(
                state,
                Some(Err(crate::download::DownloadShutdownError::WorkerPanicked))
            ) {
                anyhow::bail!("Download worker panicked during shutdown");
            }
            if !matches!(
                state,
                Some(Err(crate::download::DownloadShutdownError::Timeout))
            ) {
                break;
            }
            self.poll_downloads();
            thread::sleep(Duration::from_millis(2));
        }
        self.poll_downloads();
        while self
            .downloads
            .retry_jobs
            .values()
            .any(|j| j.worker.is_some())
        {
            self.poll_downloads();
            thread::sleep(Duration::from_millis(2));
        }
        ensure!(
            self.downloads.retry_jobs.is_empty(),
            "Download retry retains an actual owner or unresolved publication"
        );
        Ok(())
    }
    pub(super) fn approve_download_url(
        &mut self,
        id: String,
        expected_revision: u64,
        operation_epoch: u64,
        url: String,
    ) -> Result<()> {
        let task = self
            .repository
            .download_task(&id)?
            .context("Download task is unavailable")?;
        ensure!(
            !self.downloads.preparing.contains(&id)
                && !self.downloads.active.contains_key(&id)
                && !self.downloads.staging.contains_key(&id),
            BackendError::new(
                ErrorKind::Busy,
                "Download task already owns live preparation or transfer work"
            )
        );
        ensure!(
            task.revision == expected_revision
                && task.operation_epoch == operation_epoch
                && task.state == DownloadState::AwaitingUrlApproval,
            "Download URL approval refers to a changed task"
        );
        let bookmark = task
            .url_bookmark
            .as_ref()
            .context("Download URL approval is unavailable")?;
        let context = &bookmark.context;
        ensure!(
            context.operation_epoch == operation_epoch
                && self.data.selected.as_deref() == Some(context.source_id.as_str())
                && self
                    .data
                    .sources
                    .iter()
                    .any(|source| source.sha256 == context.source_content_sha256
                        && source.approved
                        && u64::from(source.approval_policy_version) == context.policy_epoch),
            "Download source approval changed"
        );
        let approved = crate::download::ApprovedDownloadUrl::from_exact_approval(
            bookmark.exact_url_for_local_approval(),
            &url,
            context,
            context,
        )?;
        let lease =
            crate::download::DownloadExecutionLease::from_approved_context(context.clone())?;
        ensure!(
            self.downloads.active.len()
                + self.downloads.preparing.len()
                + self.downloads.staging.len()
                + self.downloads.cleaning.len()
                < task.settings.max_concurrent
                && self.tasks.len() < 4,
            BackendError::new(ErrorKind::Busy, "Download concurrency capacity reached")
        );
        if self.downloads.pool.is_none() {
            self.downloads.pool = Some(crate::download::DownloadPool::new(
                task.settings.max_concurrent,
            )?);
        }
        let stored = self
            .repository
            .download_destinations()?
            .into_iter()
            .find(|grant| grant.id == task.destination.grant_id)
            .context("Download destination is unavailable")?;
        task.require_current_destination(&stored)?;
        self.downloads.leases.insert(id.clone(), lease.clone());
        self.downloads.preparing.insert(id.clone());
        let tx = self.completed_tx.clone();
        let job_id = id.clone();
        let repository = self.repository.clone();
        let spawned = self.downloads.spawn_producer(
            "download-transfer-prepare",
            (id.clone(), operation_epoch, "prepare"),
            vec![],
            move || {
                let result = (|| -> Result<Box<crate::download::TransferJob>> {
                    let mut grant = DestinationGrant::open_selected(&stored.path, stored.epoch)?;
                    ensure!(
                        grant.identity() == &stored.identity,
                        "Download destination object changed"
                    );
                    task.require_current_destination(&stored)?;
                    if task.destination.relative_components.len() == 2 {
                        grant = grant.derive_group(&task.destination.relative_components[0])?;
                    }
                    let plan = grant.plan(
                        task.destination
                            .relative_components
                            .last()
                            .context("Download leaf missing")?,
                        task.destination.collision.clone(),
                    )?;
                    let cancel = crate::download::DownloadCancel::default();
                    let journal = DownloadArtifactJournal {
                        repository,
                        cancel: cancel.clone(),
                        lease: lease.clone(),
                    };
                    let mut part = if let Some(record) = task.owned_part.clone() {
                        crate::download::OwnedPart::reopen_journaled(
                            plan,
                            record,
                            approved.context(),
                            crate::download::MAX_DOWNLOAD_BYTES,
                            &journal,
                            &cancel,
                        )?
                    } else {
                        crate::download::OwnedPart::create_journaled(
                            plan,
                            crate::download::artifact::OwnedArtifactKind::Transfer,
                            approved.context(),
                            crate::download::MAX_DOWNLOAD_BYTES,
                            &journal,
                            &cancel,
                        )?
                    };
                    part.bind_execution(lease.clone(), approved.context().clone(), cancel.clone());
                    let digest = crate::domain::media::track_record_fingerprint(
                        task.resolved_record
                            .as_ref()
                            .unwrap_or(&task.original_record),
                    )?;
                    let representation =
                        crate::download::DownloadRepresentationIdentity::from_approved_url(
                            &approved,
                            digest,
                            task.actual_quality,
                        )?;
                    let checkpoint = if task
                        .original_transfer
                        .as_ref()
                        .and_then(|checkpoint| checkpoint.representation.as_ref())
                        != Some(&representation)
                    {
                        let mut checkpoint = part.restart_for_representation(&cancel)?;
                        checkpoint.representation = Some(representation.clone());
                        if task.owned_part.is_some() {
                            let artifacts = journal
                                .repository
                                .download_owned_artifacts(Some(&task.id))?;
                            let allocation = artifacts
                                .iter()
                                .find(|artifact| artifact.allocation.record == *part.record())
                                .context(
                                    "Resumed download has no matching durable ownership record",
                                )?;
                            if allocation.allocation.context != *approved.context() {
                                let rebound = RebindDownloadArtifactJournal {
                                    journal,
                                    checkpoint: checkpoint.clone(),
                                };
                                part.acknowledge_artifact_allocation(
                                    crate::download::artifact::OwnedArtifactKind::Transfer,
                                    approved.context(),
                                    crate::download::MAX_DOWNLOAD_BYTES,
                                    &rebound,
                                    &cancel,
                                )?;
                            }
                        }
                        Some(checkpoint)
                    } else {
                        task.original_transfer
                    };
                    Ok(Box::new(crate::download::TransferJob {
                        approved_url: approved,
                        execution_lease: lease,
                        representation,
                        part,
                        checkpoint,
                        extension: task.actual_quality.extension().into(),
                        redirect_count: 0,
                        options: Default::default(),
                        verifier: Arc::new(crate::download::SymphoniaVerifier),
                    }))
                })();
                let _ = tx.send(Completion::DownloadTransferPrepared {
                    task_id: job_id,
                    operation_epoch,
                    result,
                });
            },
        );
        match spawned {
            Ok(()) => {}
            Err(error) => {
                self.downloads.preparing.remove(&id);
                if let Some(lease) = self.downloads.leases.remove(&id) {
                    lease.revoke();
                }
                return Err(error.into());
            }
        }
        Ok(())
    }
    pub(super) fn finish_download_transfer_preparation(
        &mut self,
        id: String,
        epoch: u64,
        result: Result<Box<crate::download::TransferJob>>,
    ) {
        if !self.downloads.producer_joined(&id, epoch, "prepare") {
            self.downloads
                .deferred_producers
                .push_back(DeferredProducerCompletion::Transfer(id, epoch, result));
            return;
        }
        self.downloads.preparing.remove(&id);
        let outcome = (|| -> Result<()> {
            let mut task = self
                .repository
                .download_task(&id)?
                .context("Download task is unavailable")?;
            if task.operation_epoch != epoch || task.state != DownloadState::AwaitingUrlApproval {
                return Ok(());
            }
            let old_revision = task.revision;
            match result {
                Ok(job) => {
                    job.validate()?;
                    task.owned_part = Some(job.part.record().clone());
                    task.original_representation = Some(job.representation.clone());
                    task.original_transfer = job.checkpoint.clone();
                    task.state = DownloadState::Transferring;
                    task.pause_reason = None;
                    task.revision += 1;
                    task.updated_at_ms = download_now()?;
                    self.revision = self
                        .repository
                        .update_download_task(
                            &task,
                            old_revision,
                            epoch,
                            Some(self.revision),
                            &|| Ok(()),
                        )?
                        .revision;
                    let handle = self
                        .downloads
                        .pool
                        .as_ref()
                        .context("Download pool unavailable")?
                        .submit(*job)?;
                    self.downloads.active.insert(id.clone(), handle);
                }
                Err(error) => {
                    task.state = DownloadState::Failed;
                    task.revision += 1;
                    task.updated_at_ms = download_now()?;
                    task.error = Some(crate::download::DownloadError {
                        kind: crate::download::DownloadErrorKind::Permission,
                        message: format!("{error:#}").chars().take(1024).collect(),
                        http_status: None,
                    });
                    self.revision = self
                        .repository
                        .update_download_task(
                            &task,
                            old_revision,
                            epoch,
                            Some(self.revision),
                            &|| Ok(()),
                        )?
                        .revision;
                }
            }
            Ok(())
        })();
        if let Err(error) = outcome {
            self.error(format!("Download preparation failed: {error:#}"));
        }
    }
    pub(super) fn poll_downloads(&mut self) {
        self.poll_download_read_adoptions();
        self.poll_deferred_download_producers();
        self.poll_download_postprocessing_retries();
        self.poll_pending_download_postprocess();
        let now = Instant::now();
        for (id, control) in &self.downloads.staging {
            if !self
                .downloads
                .postprocess_queue
                .iter()
                .any(|pending| &pending.task.id == id)
                && now >= control.deadline
            {
                control.stop(crate::download::StopReason::Cancel);
            }
        }
        let checkpoints = self
            .downloads
            .active
            .iter()
            .filter_map(|(id, handle)| {
                if self
                    .downloads
                    .checkpoint_at
                    .get(id)
                    .is_some_and(|last| now.duration_since(*last) < Duration::from_secs(1))
                {
                    return None;
                }
                handle.durable_checkpoint().map(|checkpoint| {
                    (
                        id.clone(),
                        handle.progress().context.operation_epoch,
                        checkpoint,
                    )
                })
            })
            .take(4)
            .collect::<Vec<_>>();
        for (id, epoch, checkpoint) in checkpoints {
            let outcome = (|| -> Result<()> {
                let mut task = self
                    .repository
                    .download_task(&id)?
                    .context("Download task is unavailable")?;
                if task.operation_epoch != epoch
                    || !matches!(
                        task.state,
                        DownloadState::Transferring | DownloadState::Pausing
                    )
                    || task.original_transfer.as_ref() == Some(&checkpoint)
                {
                    return Ok(());
                }
                let revision = task.revision;
                task.revision += 1;
                task.updated_at_ms = download_now()?;
                task.original_representation = checkpoint.representation.clone();
                task.original_transfer = Some(checkpoint);
                self.revision = self
                    .repository
                    .update_download_task(&task, revision, epoch, Some(self.revision), &|| Ok(()))?
                    .revision;
                Ok(())
            })();
            if let Err(error) = outcome {
                self.error(format!("Download checkpoint failed: {error:#}"));
            }
            self.downloads.checkpoint_at.insert(id, now);
        }
        let finished = self
            .downloads
            .active
            .iter()
            .filter(|(_, handle)| handle.is_finished())
            .map(|(id, _)| id.clone())
            .take(4)
            .collect::<Vec<_>>();
        for id in finished {
            if let Some(handle) = self.downloads.active.remove(&id)
                && let Some(outcome) = handle.take_outcome()
            {
                self.downloads.checkpoint_at.remove(&id);
                self.finish_download_transfer(id, outcome);
            }
        }
    }
    fn finish_download_transfer(&mut self, id: String, outcome: crate::download::TransferOutcome) {
        use crate::download::TransferOutcome;
        let result = (|| -> Result<()> {
            let mut task = self
                .repository
                .download_task(&id)?
                .context("Download task is unavailable")?;
            let old_revision = task.revision;
            let epoch = task.operation_epoch;
            match outcome {
                TransferOutcome::Verified(audio) => {
                    let checkpoint = audio.checkpoint().clone();
                    if task.state == DownloadState::Pausing {
                        task.owned_part = Some(audio.owned_record().clone());
                        task.original_transfer = Some(checkpoint.clone());
                        task.original_representation = checkpoint.representation.clone();
                        task.operation_epoch += 1;
                        task.state = DownloadState::Paused;
                        task.pause_reason = Some(DownloadPauseReason::User);
                        task.revision += 1;
                        task.updated_at_ms = download_now()?;
                        self.revision = self
                            .repository
                            .update_download_task(
                                &task,
                                old_revision,
                                epoch,
                                Some(self.revision),
                                &|| Ok(()),
                            )?
                            .revision;
                        return Ok(());
                    }
                    if checkpoint
                        .representation
                        .as_ref()
                        .is_none_or(|representation| representation.task_id != task.id)
                    {
                        anyhow::bail!("Verified task identity changed");
                    }
                    task.original_transfer = Some(checkpoint.clone());
                    task.original_representation = checkpoint.representation.clone();
                    if let Some(artifact) = self
                        .repository
                        .download_owned_artifacts(Some(&id))?
                        .into_iter()
                        .find(|artifact| {
                            artifact.allocation.record.nonce == audio.owned_record().nonce
                        })
                    {
                        self.repository.verify_download_artifact(
                            &audio.owned_record().nonce,
                            artifact.allocation.context.operation_epoch,
                            checkpoint.bytes,
                            &checkpoint.sha256,
                            &|| Ok(()),
                        )?;
                    } else {
                        anyhow::bail!("Download partial has no durable ownership record");
                    }
                    task.state = DownloadState::PostProcessing;
                    task.revision += 1;
                    task.updated_at_ms = download_now()?;
                    self.revision = self
                        .repository
                        .update_download_task(
                            &task,
                            old_revision,
                            epoch,
                            Some(self.revision),
                            &|| Ok(()),
                        )?
                        .revision;
                    let cancel = crate::download::DownloadCancel::default();
                    let metadata_cancel = Arc::new(AtomicBool::new(false));
                    ensure!(
                        self.downloads.postprocess_queue.len()
                            < crate::download::MAX_DOWNLOAD_TASKS,
                        "Download postprocessing queue capacity reached"
                    );
                    self.downloads.staging.insert(
                        id.clone(),
                        StageControl {
                            cancel: cancel.clone(),
                            metadata_cancel: metadata_cancel.clone(),
                            deadline: Instant::now() + Duration::from_secs(45),
                            source_bound: true,
                        },
                    );
                    self.downloads
                        .postprocess_queue
                        .push_back(PendingPostprocess {
                            audio,
                            task,
                            retirement_pins: None,
                            error: None,
                        });
                    self.poll_pending_download_postprocess();
                    return Ok(());
                }
                TransferOutcome::Stopped {
                    reason: _,
                    part,
                    checkpoint,
                } => {
                    task.operation_epoch = task
                        .operation_epoch
                        .checked_add(1)
                        .context("Download epoch overflow")?;
                    task.owned_part = Some(part);
                    task.original_transfer = checkpoint;
                    task.original_representation = task
                        .original_transfer
                        .as_ref()
                        .and_then(|checkpoint| checkpoint.representation.clone());
                    task.state = DownloadState::Paused;
                    task.pause_reason = Some(DownloadPauseReason::User);
                }
                TransferOutcome::AwaitingRedirectApproval {
                    exact_url,
                    context,
                    part,
                    checkpoint,
                    ..
                } => {
                    task.owned_part = Some(part);
                    task.original_representation = checkpoint.representation.clone();
                    task.original_transfer = Some(checkpoint);
                    task.url_bookmark = Some(PrivateUrlBookmark::new(exact_url, context)?);
                    task.state = DownloadState::AwaitingUrlApproval;
                    task.pause_reason = Some(DownloadPauseReason::UrlReapproval);
                }
                TransferOutcome::RefreshRequired {
                    part, checkpoint, ..
                } => {
                    task.owned_part = Some(part);
                    task.original_representation = checkpoint.representation.clone();
                    task.original_transfer = Some(checkpoint);
                    task.state = DownloadState::Paused;
                    task.pause_reason = Some(DownloadPauseReason::SourceReapproval);
                }
                TransferOutcome::Failed {
                    error,
                    part,
                    checkpoint,
                } => {
                    task.error = Some(error);
                    task.owned_part = Some(part);
                    task.original_transfer = checkpoint;
                    task.original_representation = task
                        .original_transfer
                        .as_ref()
                        .and_then(|checkpoint| checkpoint.representation.clone());
                    task.state = DownloadState::Failed;
                }
            }
            task.revision += 1;
            task.updated_at_ms = download_now()?;
            self.revision = self
                .repository
                .update_download_task(&task, old_revision, epoch, Some(self.revision), &|| Ok(()))?
                .revision;
            self.downloads.leases.remove(&task.id);
            Ok(())
        })();
        if let Err(error) = result {
            self.error(format!("Download completion failed: {error:#}"));
        }
    }
    pub(super) fn finish_download_postprocess_preparation(
        &mut self,
        id: String,
        epoch: u64,
        result: Result<Box<PreparedDownloadOutput>>,
    ) {
        if !self.downloads.producer_joined(&id, epoch, "postprocess") {
            self.downloads
                .deferred_producers
                .push_back(DeferredProducerCompletion::Postprocess(id, epoch, result));
            return;
        }
        let outcome = (|| -> Result<()> {
            let mut task = self
                .repository
                .download_task(&id)?
                .context("Download task is unavailable")?;
            if task.operation_epoch == epoch && task.state == DownloadState::Pausing {
                let revision = task.revision;
                task.operation_epoch += 1;
                task.revision += 1;
                task.state = DownloadState::Paused;
                task.pause_reason = Some(DownloadPauseReason::User);
                task.updated_at_ms = download_now()?;
                self.revision = self
                    .repository
                    .update_download_task(&task, revision, epoch, Some(self.revision), &|| Ok(()))?
                    .revision;
                self.downloads.staging.remove(&id);
                self.downloads.leases.remove(&id);
                return Ok(());
            }
            if task.operation_epoch != epoch || task.state != DownloadState::PostProcessing {
                self.downloads.staging.remove(&id);
                return Ok(());
            }
            let old_revision = task.revision;
            let prepared = match result {
                Ok(prepared) => prepared,
                Err(_) => {
                    task.state = DownloadState::Failed;
                    task.error = Some(crate::download::DownloadError {
                        kind: crate::download::DownloadErrorKind::Verification,
                        message: "Download postprocessing failed before final publication".into(),
                        http_status: None,
                    });
                    task.revision += 1;
                    task.updated_at_ms = download_now()?;
                    self.revision = self
                        .repository
                        .update_download_task(
                            &task,
                            old_revision,
                            epoch,
                            Some(self.revision),
                            &|| Ok(()),
                        )?
                        .revision;
                    self.downloads.staging.remove(&id);
                    return Ok(());
                }
            };
            let (bytes, sha256) = prepared.audio.digest();
            self.repository.verify_download_artifact(
                &prepared.audio.record().nonce,
                epoch,
                bytes,
                &sha256,
                &|| Ok(()),
            )?;
            if let Some(sidecar) = &prepared.sidecar {
                self.repository.verify_download_artifact(
                    &sidecar.owned_record().nonce,
                    epoch,
                    sidecar.output_digest().bytes,
                    &sidecar.output_digest().sha256,
                    &|| Ok(()),
                )?;
            }
            let candidate = DownloadArtifactProof {
                basename: prepared.audio.record().basename.clone(),
                identity: prepared.audio.record().identity.clone(),
                bytes,
                sha256,
            };
            let sidecar = prepared
                .sidecar
                .as_ref()
                .map(|sidecar| DownloadArtifactProof {
                    basename: format!(
                        "{}.lrc",
                        task.destination
                            .relative_components
                            .last()
                            .unwrap()
                            .rsplit_once('.')
                            .map(|(stem, _)| stem)
                            .unwrap_or("")
                    ),
                    identity: sidecar.owned_record().identity.clone(),
                    bytes: sidecar.output_digest().bytes,
                    sha256: sidecar.output_digest().sha256.clone(),
                });
            task.commit_journal = Some(DownloadCommitJournal {
                task_revision: old_revision,
                operation_epoch: epoch,
                grant_epoch: task.destination.grant_epoch,
                phase: DownloadCommitPhase::Verified,
                original_part: prepared.original_part.clone(),
                candidate,
                observed_target: None,
                rollback_original: None,
                sidecar,
                retry: None,
            });
            task.stages = prepared.stages.clone();
            task.state = DownloadState::CommittingAudio;
            task.revision += 1;
            task.updated_at_ms = download_now()?;
            self.revision = self
                .repository
                .update_download_task(&task, old_revision, epoch, Some(self.revision), &|| Ok(()))?
                .revision;
            let completed = self.completed_tx.clone();
            let objects = prepared.producer_objects();
            self.downloads.spawn_producer("download-final-publication",
                (id.clone(), epoch, "publication"), objects, move || {
                let prepared = *prepared;
                let original = prepared.original_transfer;
                let mut stages = prepared.stages;
                let original_audio = prepared.original_audio;
                let result = (|| -> Result<(crate::download::CommittedAudio, Option<crate::download::CommittedSidecar>)> {
                    let audio = prepared.audio.commit()?;
                    let sidecar = if let Some(sidecar) = prepared.sidecar {
                        match sidecar.commit() {
                            Ok(sidecar) => { stages.lyrics.state = DownloadStageState::Succeeded; Some(sidecar) }
                            Err(_) => {
                                stages.lyrics.state = DownloadStageState::Failed; stages.lyrics.error = Some(crate::download::DownloadError {
                                    kind: crate::download::DownloadErrorKind::Disk, message: "Lyric sidecar could not be published".into(), http_status: None }); None
                            }
                        }
                    } else { None };
                    Ok((audio, sidecar))
                })();
                let _ = completed.send(Completion::DownloadAudioCommitted { task_id: id, operation_epoch: epoch, original, stages, original_audio, result: Box::new(result) });
            })?;
            Ok(())
        })();
        if let Err(error) = outcome {
            self.error(format!(
                "Download publication preparation failed: {error:#}"
            ));
        }
    }
    #[allow(clippy::too_many_arguments)]
    pub(super) fn finish_download_audio_commit(
        &mut self,
        id: String,
        epoch: u64,
        original: crate::download::TransferCheckpoint,
        stages: DownloadStageRecords,
        original_audio: Option<Box<crate::download::VerifiedOwnedAudio>>,
        result: Result<(
            crate::download::CommittedAudio,
            Option<crate::download::CommittedSidecar>,
        )>,
    ) {
        if !self.downloads.producer_joined(&id, epoch, "publication") {
            self.downloads
                .deferred_producers
                .push_back(DeferredProducerCompletion::Publication {
                    id,
                    epoch,
                    original,
                    stages,
                    original_audio,
                    result,
                });
            return;
        }
        let outcome = (|| -> Result<()> {
            let mut task = self
                .repository
                .download_task(&id)?
                .context("Download task is unavailable")?;
            if task.operation_epoch != epoch || task.state != DownloadState::CommittingAudio {
                return Ok(());
            }
            let revision = task.revision;
            let mut published_artifacts = Vec::new();
            let mut adopted_facts = None;
            match result {
                Ok((audio, sidecar)) => {
                    adopted_facts = Some(audio.clone());
                    let artifacts = self.repository.download_owned_artifacts(Some(&id))?;
                    let artifact = artifacts
                        .iter()
                        .find(|artifact| artifact.allocation.record.identity == audio.identity)
                        .context("Published download has no matching verified artifact")?;
                    published_artifacts.push((
                        artifact.allocation.record.nonce.clone(),
                        artifact.allocation.context.operation_epoch,
                        audio.checkpoint.bytes,
                        audio.checkpoint.sha256.clone(),
                    ));
                    task.output_audio = Some(DownloadArtifactProof {
                        basename: audio
                            .path
                            .file_name()
                            .context("Download output basename missing")?
                            .to_string_lossy()
                            .into_owned(),
                        identity: audio.identity,
                        bytes: audio.checkpoint.bytes,
                        sha256: audio.checkpoint.sha256,
                    });
                    task.audio_available = true;
                    task.owned_part = audio.retained_part;
                    task.original_transfer = Some(original);
                    task.stages = stages;
                    if let Some(sidecar) = sidecar {
                        let artifact = artifacts
                            .iter()
                            .find(|artifact| {
                                artifact.allocation.record.identity == sidecar.identity
                            })
                            .context("Published sidecar has no matching verified artifact")?;
                        published_artifacts.push((
                            artifact.allocation.record.nonce.clone(),
                            artifact.allocation.context.operation_epoch,
                            sidecar.bytes,
                            sidecar.sha256.clone(),
                        ));
                        task.stages.lyrics.artifact = Some(DownloadArtifactProof {
                            basename: sidecar
                                .path
                                .file_name()
                                .context("Sidecar basename missing")?
                                .to_string_lossy()
                                .into_owned(),
                            identity: sidecar.identity,
                            bytes: sidecar.bytes,
                            sha256: sidecar.sha256,
                        });
                    }
                    task.commit_journal = None;
                    let failed = [
                        &task.stages.metadata,
                        &task.stages.cover,
                        &task.stages.embedded_lyrics,
                        &task.stages.lyrics,
                    ]
                    .into_iter()
                    .find(|stage| stage.state == DownloadStageState::Failed);
                    if let Some(stage) = failed {
                        task.state = DownloadState::Failed;
                        task.error = stage.error.clone();
                    } else if task.stages.all_satisfied() {
                        task.state = DownloadState::Completed;
                    } else {
                        task.state = DownloadState::Paused;
                        task.pause_reason = Some(DownloadPauseReason::UrlReapproval);
                    }
                }
                Err(error) => {
                    task.state = DownloadState::Failed;
                    task.error = Some(crate::download::DownloadError {
                        kind: crate::download::DownloadErrorKind::Disk,
                        message: format!("{error:#}").chars().take(1024).collect(),
                        http_status: None,
                    });
                }
            }
            task.revision += 1;
            task.updated_at_ms = download_now()?;
            self.revision = if published_artifacts.is_empty() {
                self.repository.update_download_task(
                    &task,
                    revision,
                    epoch,
                    Some(self.revision),
                    &|| Ok(()),
                )?
            } else {
                self.repository.commit_download_publication(
                    &task,
                    revision,
                    epoch,
                    Some(self.revision),
                    &published_artifacts,
                    &|| Ok(()),
                )?
            }
            .revision;
            if let Some(facts) = adopted_facts {
                self.begin_download_read_adoption(task.clone(), facts)?;
            }
            if task.audio_available
                && let Some(original_audio) = original_audio
            {
                let artifacts = self.repository.download_owned_artifacts(Some(&id))?;
                let artifact = artifacts
                    .iter()
                    .find(|artifact| artifact.allocation.record == *original_audio.owned_record())
                    .context("Download original has no matching durable ownership record")?;
                self.start_original_cleanup(&task, artifact, original_audio)?;
            }
            Ok(())
        })();
        self.downloads.leases.remove(&id);
        self.downloads.staging.remove(&id);
        if let Err(error) = outcome {
            self.error(format!("Download audio commit failed: {error:#}"));
        }
    }
    pub(super) fn resume_download(&mut self, id: String, expected_revision: u64) -> Result<()> {
        let mut task = self
            .repository
            .download_task(&id)?
            .context("Download task is unavailable")?;
        ensure!(
            task.revision == expected_revision,
            "Download task revision conflict"
        );
        ensure!(
            !task.audio_available,
            "Published audio requires a postprocessing retry instead of another transfer"
        );
        ensure!(
            matches!(
                task.state,
                DownloadState::Waiting | DownloadState::Paused | DownloadState::Failed
            ),
            "Download task is already active"
        );
        let grant = self
            .repository
            .download_destinations()?
            .into_iter()
            .find(|grant| grant.id == task.destination.grant_id)
            .context("Download destination is unavailable")?;
        task.require_current_destination(&grant)?;
        let source = self
            .data
            .selected
            .as_ref()
            .and_then(|id| self.data.sources.iter().find(|source| &source.sha256 == id))
            .context("Choose an approved download source")?;
        source.require_approval()?;
        let epoch = task.operation_epoch;
        task.operation_epoch = epoch.checked_add(1).context("Download epoch overflow")?;
        task.revision += 1;
        task.state = DownloadState::Resolving;
        task.pause_reason = None;
        task.error = None;
        task.url_bookmark = None;
        task.updated_at_ms = download_now()?;
        self.revision = self
            .repository
            .update_download_task(
                &task,
                expected_revision,
                epoch,
                Some(self.revision),
                &|| Ok(()),
            )?
            .revision;
        if !self.downloads.resolve_queue.contains(&id) {
            self.downloads.resolve_queue.push_back(id);
        }
        Ok(())
    }
    pub(super) fn pause_download(&mut self, id: String, expected_revision: u64) -> Result<()> {
        let mut task = self
            .repository
            .download_task(&id)?
            .context("Download task is unavailable")?;
        ensure!(
            task.revision == expected_revision,
            "Download task revision conflict"
        );
        ensure!(
            !self.downloads.cleaning.contains(&id),
            "Download original cleanup is still active"
        );
        if task.state == DownloadState::PostProcessing
            && let Some(cancel) = self.downloads.staging.get(&id)
        {
            cancel.stop(crate::download::StopReason::Pause);
            let epoch = task.operation_epoch;
            task.revision += 1;
            task.state = DownloadState::Pausing;
            task.updated_at_ms = download_now()?;
            self.revision = self
                .repository
                .update_download_task(
                    &task,
                    expected_revision,
                    epoch,
                    Some(self.revision),
                    &|| Ok(()),
                )?
                .revision;
            return Ok(());
        }
        if let Some(handle) = self.downloads.active.get(&id) {
            handle.stop(crate::download::StopReason::Pause);
            let epoch = task.operation_epoch;
            task.revision += 1;
            task.state = DownloadState::Pausing;
            task.updated_at_ms = download_now()?;
            self.revision = self
                .repository
                .update_download_task(
                    &task,
                    expected_revision,
                    epoch,
                    Some(self.revision),
                    &|| Ok(()),
                )?
                .revision;
            return Ok(());
        }
        ensure!(
            task.state != DownloadState::CommittingAudio,
            BackendError::new(
                ErrorKind::Busy,
                "Download audio commit is already in progress"
            )
        );
        self.downloads.resolve_queue.retain(|queued| queued != &id);
        if let Some((active, _, cancel)) = &self.downloads.resolving
            && active == &id
        {
            cancel.store(true, Ordering::Release);
        }
        if let Some(lease) = self.downloads.leases.remove(&id) {
            lease.revoke();
        }
        if let Some(handle) = self.downloads.active.get(&id) {
            handle.stop(crate::download::StopReason::Pause);
        }
        self.downloads.pending.remove(&id);
        let old_epoch = task.operation_epoch;
        task.operation_epoch = old_epoch
            .checked_add(1)
            .context("Download epoch overflow")?;
        task.revision += 1;
        task.state = DownloadState::Paused;
        task.pause_reason = Some(DownloadPauseReason::User);
        task.updated_at_ms = download_now()?;
        self.revision = self
            .repository
            .update_download_task(
                &task,
                expected_revision,
                old_epoch,
                Some(self.revision),
                &|| Ok(()),
            )?
            .revision;
        Ok(())
    }
    pub(super) fn revoke_download_execution(&mut self) {
        self.downloads.resolve_queue.clear();
        if let Some((_, _, cancel)) = &self.downloads.resolving {
            cancel.store(true, Ordering::Release);
        }
        for lease in self.downloads.leases.values() {
            lease.revoke();
        }
        for handle in self.downloads.active.values() {
            handle.stop(crate::download::StopReason::AuthorityRevoked);
        }
        for cancel in self
            .downloads
            .staging
            .values()
            .filter(|control| control.source_bound)
        {
            cancel.stop(crate::download::StopReason::AuthorityRevoked);
        }
        self.downloads.leases.clear();
        self.downloads.pending.clear();
        if let Ok(tasks) = self.repository.download_tasks() {
            for mut task in tasks.into_iter().filter(|task| {
                matches!(
                    task.state,
                    DownloadState::Resolving | DownloadState::AwaitingUrlApproval
                )
            }) {
                let revision = task.revision;
                let epoch = task.operation_epoch;
                task.revision += 1;
                task.operation_epoch = task.operation_epoch.saturating_add(1);
                task.state = DownloadState::Paused;
                task.pause_reason = Some(DownloadPauseReason::SourceReapproval);
                if let Ok(now) = download_now() {
                    task.updated_at_ms = now;
                }
                if let Ok(change) =
                    self.repository
                        .update_download_task(&task, revision, epoch, None, &|| Ok(()))
                {
                    self.revision = change.revision;
                }
            }
        }
    }
    pub(super) fn pump_download_resolution(&mut self) -> Result<()> {
        if self.downloads.resolving.is_some()
            || self.source_inflight.is_some()
            || self.source_initializing
        {
            return Ok(());
        }
        let Some(id) = self.downloads.resolve_queue.pop_front() else {
            return Ok(());
        };
        let task = self
            .repository
            .download_task(&id)?
            .context("Download task is unavailable")?;
        if task.state != DownloadState::Resolving {
            return Ok(());
        }
        let source_id = self
            .data
            .selected
            .clone()
            .context("Approved download source is unavailable")?;
        let source = self
            .data
            .sources
            .iter()
            .find(|source| source.sha256 == source_id)
            .context("Download source is unavailable")?
            .clone();
        source.require_approval()?;
        let policy_epoch = u64::from(source.approval_policy_version);
        let mut worker = match self.runtime.take().filter(|worker| !worker.is_stopped()) {
            Some(worker) => worker,
            None => {
                self.source_epoch = self.source_epoch.wrapping_add(1);
                SourceWorker::prepare()?
            }
        };
        let source_epoch = self.source_epoch;
        let cancel = Arc::new(AtomicBool::new(false));
        self.cancellation = Some(worker.cancel_handle());
        self.source_inflight = Some(SourceFlight {
            download: true,
            source_epoch,
            cancel: cancel.clone(),
            preload_operation: None,
        });
        self.downloads.resolving = Some((id.clone(), task.operation_epoch, cancel.clone()));
        let tx = self.completed_tx.clone();
        let generation = self.generation;
        self.tasks.push(thread::Builder::new().name("lx-source-download".into()).spawn(move || {
            let result = (|| -> Result<(String, DownloadQuality)> {
                if !worker.is_initialized() { worker.initialize(&source.script)?; }
                ensure!(!cancel.load(Ordering::Acquire), crate::source::SourceRequestCancelled);
                let capabilities = worker.manifest().sources.get(&task.original_record.platform.source).context("Source does not support download platform")?;
                let track = crate::download::planning::track_quality_capabilities(&task.original_record);
                let plan = crate::download::planning::select_executable_quality(&task.original_record.platform.source, task.requested_quality, &track, capabilities)?;
                let request = crate::source::SourceRequest { source: task.original_record.platform.source.clone(), action: "musicUrl".into(),
                    info: serde_json::json!({"type":plan.actual.as_str(),"musicInfo":task.original_record.to_source_music_info()}) };
                let deadline = Instant::now() + Duration::from_secs(30);
                let url = worker.request_url_cancellable(&request, || cancel.load(Ordering::Acquire) || Instant::now() >= deadline)?;
                crate::domain::media::CachedMediaUrl::new(url.clone())?;
                Ok((url, plan.actual))
            })();
            let _ = tx.send(Completion::SourceEvents { generation, source_id: source_id.clone(), events: worker.take_host_events() });
            let _ = tx.send(Completion::DownloadResolved { task_id: id, operation_epoch: task.operation_epoch,
                source_epoch, source_id, policy_epoch, worker, result });
        })?);
        Ok(())
    }
    #[allow(clippy::too_many_arguments)]
    pub(super) fn finish_download_resolve(
        &mut self,
        id: String,
        epoch: u64,
        source_epoch: u64,
        source_id: String,
        policy_epoch: u64,
        worker: SourceWorker,
        result: Result<(String, DownloadQuality)>,
    ) {
        if self
            .source_inflight
            .as_ref()
            .is_some_and(|flight| flight.download && flight.source_epoch == source_epoch)
        {
            self.source_inflight = None;
            self.cancellation = None;
        }
        if self
            .downloads
            .resolving
            .as_ref()
            .is_some_and(|(active, active_epoch, _)| active == &id && *active_epoch == epoch)
        {
            self.downloads.resolving = None;
        }
        let selected = self.data.selected.as_deref() == Some(source_id.as_str())
            && self.source_epoch == source_epoch
            && self.data.sources.iter().any(|source| {
                source.sha256 == source_id
                    && source.approved
                    && u64::from(source.approval_policy_version) == policy_epoch
            });
        if selected && !worker.is_stopped() {
            self.source_caps = Some(worker.manifest().clone());
            self.runtime = Some(worker);
        }
        let outcome = (|| -> Result<()> {
            let mut task = self
                .repository
                .download_task(&id)?
                .context("Download task is unavailable")?;
            if task.operation_epoch != epoch || task.state != DownloadState::Resolving {
                return Ok(());
            }
            let previous = task.revision;
            ensure!(selected, "Download source execution context changed");
            match result {
                Ok((url, quality)) => {
                    if quality != task.actual_quality {
                        ensure!(
                            task.owned_part.is_none(),
                            "Existing partial requires explicit quality reset"
                        );
                        task.operation_epoch = task
                            .operation_epoch
                            .checked_add(1)
                            .context("Download epoch overflow")?;
                        task.original_transfer = None;
                        task.original_representation = None;
                        task.commit_journal = None;
                    }
                    task.actual_quality = quality;
                    let filename = safe_filename(
                        &task.original_record.title,
                        &task.original_record.artists.join("、"),
                        task.settings.filename_format,
                        quality,
                    )?;
                    *task
                        .destination
                        .relative_components
                        .last_mut()
                        .context("Download basename missing")? = filename.basename;
                    task.stages = DownloadStageRecords::for_settings(&task.settings, quality);
                    let context = crate::download::DownloadContext {
                        profile_id: task.profile_id.clone(),
                        task_id: id.clone(),
                        operation_epoch: task.operation_epoch,
                        source_id: source_id.clone(),
                        source_content_sha256: source_id.clone(),
                        policy_epoch,
                    };
                    task.url_bookmark =
                        Some(PrivateUrlBookmark::new(url, context).map_err(anyhow::Error::from)?);
                    task.state = DownloadState::AwaitingUrlApproval;
                    task.pause_reason = Some(DownloadPauseReason::UrlReapproval);
                }
                Err(_) => {
                    task.state = DownloadState::Failed;
                    task.error = Some(crate::download::DownloadError {
                        kind: crate::download::DownloadErrorKind::Network,
                        message: "Approved source could not resolve this download".into(),
                        http_status: None,
                    });
                }
            }
            task.revision += 1;
            task.updated_at_ms = download_now()?;
            self.revision = self
                .repository
                .update_download_task(&task, previous, epoch, Some(self.revision), &|| Ok(()))?
                .revision;
            Ok(())
        })();
        if let Err(error) = outcome {
            self.error(format!("Download resolution failed: {error:#}"));
        }
    }
    pub(super) fn download_pending_url(
        &self,
        id: &str,
    ) -> Result<Option<DownloadUrlApprovalRequest>> {
        let Some(task) = self.repository.download_task(id)? else {
            return Ok(None);
        };
        Ok(task
            .url_bookmark
            .as_ref()
            .filter(|_| task.state == DownloadState::AwaitingUrlApproval)
            .map(|bookmark| DownloadUrlApprovalRequest {
                task_id: task.id.clone(),
                task_revision: task.revision,
                operation_epoch: task.operation_epoch,
                source_content_sha256: bookmark.context.source_content_sha256.clone(),
                source_policy_epoch: bookmark.context.policy_epoch,
                url: bookmark.exact_url_for_local_approval().to_owned(),
            }))
    }
    pub(super) fn select_download_destination(&mut self, path: PathBuf) -> Result<()> {
        ensure!(
            path.is_absolute() && path.to_str().is_some(),
            "Download destination must be an absolute selected path"
        );
        ensure!(self.tasks.len() < 4, "Too many pending operations");
        let profile_id = crate::domain::music::stable_id(
            "download-profile",
            self.repository
                .path()
                .parent()
                .context("Profile directory missing")?
                .to_string_lossy()
                .as_bytes(),
        );
        let operation = self.request_id.clone();
        let cancel = Arc::new(AtomicBool::new(false));
        self.file_cancels.insert(operation.clone(), cancel.clone());
        let completed = self.completed_tx.clone();
        self.tasks.push(
            thread::Builder::new()
                .name("download-destination".into())
                .spawn(move || {
                    let result = (|| -> Result<StoredDestinationGrant> {
                        ensure!(
                            !cancel.load(Ordering::Acquire),
                            "Download destination selection cancelled"
                        );
                        let selected = DestinationGrant::open_selected(&path, 1)?;
                        ensure!(
                            !cancel.load(Ordering::Acquire),
                            "Download destination selection cancelled"
                        );
                        let id = crate::domain::music::stable_id(
                            "download-destination",
                            &serde_json::to_vec(&(selected.path(), selected.identity()))?,
                        );
                        Ok(StoredDestinationGrant {
                            id,
                            profile_id,
                            path: selected.path().to_path_buf(),
                            identity: selected.identity().clone(),
                            epoch: 1,
                            revision: 1,
                            revoked: false,
                        })
                    })();
                    let _ = completed.send(Completion::DownloadDestination { operation, result });
                })?,
        );
        self.async_started = true;
        Ok(())
    }
    pub(super) fn finish_download_destination(
        &mut self,
        operation: String,
        result: Result<StoredDestinationGrant>,
    ) {
        let cancel = self.file_cancels.remove(&operation);
        let result = result.and_then(|mut grant| {
            ensure!(
                !cancel
                    .as_ref()
                    .is_some_and(|cancel| cancel.load(Ordering::Acquire)),
                "Download destination selection cancelled"
            );
            let previous = self
                .repository
                .download_destinations()?
                .into_iter()
                .find(|old| old.id == grant.id);
            if let Some(previous) = previous {
                if !previous.revoked
                    && previous.path == grant.path
                    && previous.identity == grant.identity
                {
                    return Ok(());
                }
                grant.revision = previous
                    .revision
                    .checked_add(1)
                    .context("Destination revision overflow")?;
                grant.epoch = previous
                    .epoch
                    .checked_add(1)
                    .context("Destination epoch overflow")?;
                self.revision = self
                    .repository
                    .put_download_destination(
                        &grant,
                        Some(previous.revision),
                        Some(self.revision),
                        &|| Ok(()),
                    )?
                    .revision;
            } else {
                self.revision = self
                    .repository
                    .put_download_destination(&grant, None, Some(self.revision), &|| Ok(()))?
                    .revision;
            }
            Ok(())
        });
        self.publish_finished(operation, &result);
    }
    pub(super) fn revoke_download_destination(&mut self, id: String, expected: u64) -> Result<()> {
        let mut grant = self
            .repository
            .download_destinations()?
            .into_iter()
            .find(|grant| grant.id == id)
            .context("Download destination is unavailable")?;
        ensure!(
            grant.revision == expected,
            "Download destination revision conflict"
        );
        for task in self
            .repository
            .download_tasks()?
            .iter()
            .filter(|task| task.destination.grant_id == id)
        {
            self.cancel_cleanup(&task.id);
            if let Some(lease) = self.downloads.leases.get(&task.id) {
                lease.revoke();
            }
            if let Some(handle) = self.downloads.active.get(&task.id) {
                handle.stop(crate::download::StopReason::AuthorityRevoked);
            }
            if let Some(cancel) = self.downloads.staging.get(&task.id) {
                cancel.stop(crate::download::StopReason::AuthorityRevoked);
            }
            if let Some((active, _, cancel)) = &self.downloads.resolving
                && active == &task.id
            {
                cancel.store(true, Ordering::Release);
            }
            self.downloads
                .resolve_queue
                .retain(|queued| queued != &task.id);
        }
        grant.revoked = true;
        grant.epoch = grant
            .epoch
            .checked_add(1)
            .context("Destination epoch overflow")?;
        grant.revision = grant
            .revision
            .checked_add(1)
            .context("Destination revision overflow")?;
        self.revision = self
            .repository
            .put_download_destination(&grant, Some(expected), Some(self.revision), &|| Ok(()))?
            .revision;
        Ok(())
    }
    pub(super) fn add_download_tasks(
        &mut self,
        list_id: String,
        music_ids: Vec<String>,
        quality: DownloadQuality,
        top: bool,
    ) -> Result<()> {
        ensure!(
            !music_ids.is_empty() && music_ids.len() <= 100,
            "Choose 1 to 100 download tracks"
        );
        let settings = self.repository.download_settings()?;
        let destination_id = settings
            .destination_grant_id
            .as_ref()
            .context("Choose a download destination first")?;
        let destination = self
            .repository
            .download_destinations()?
            .into_iter()
            .find(|grant| &grant.id == destination_id && !grant.revoked)
            .context("Download destination requires reapproval")?;
        let existing = self.repository.download_tasks()?;
        let list_name = self
            .repository
            .list_infos()?
            .into_iter()
            .find(|list| list.id == list_id)
            .map(|list| list.name);
        let timestamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis() as u64;
        let mut rows = Vec::new();
        let mut seen = HashSet::new();
        for music_id in music_ids {
            let record = self
                .repository
                .list_track(&list_id, &music_id)?
                .context("Download track is not in its list")?;
            ensure!(
                matches!(
                    record.platform.source.as_str(),
                    "kw" | "kg" | "tx" | "wy" | "mg"
                ),
                "Download platform is unsupported"
            );
            let capabilities = crate::download::planning::track_quality_capabilities(&record);
            let actual = if capabilities.contains(&quality) {
                quality
            } else {
                crate::download::planning::QUALITY_ORDER
                    .into_iter()
                    .skip_while(|candidate| *candidate != quality)
                    .find(|candidate| capabilities.contains(candidate))
                    .unwrap_or(DownloadQuality::K128)
            };
            let id = crate::domain::music::stable_id(
                "download-task",
                format!(
                    "{}_{}_{}",
                    record.lx_id,
                    actual.as_str(),
                    actual.extension()
                )
                .as_bytes(),
            );
            if existing.iter().any(|task| task.id == id) || !seen.insert(id.clone()) {
                continue;
            }
            let filename = safe_filename(
                &record.title,
                &record.artists.join("、"),
                settings.filename_format,
                actual,
            )?;
            let mut components = Vec::new();
            if settings.group_by_list {
                components.push(
                    crate::download::planning::group_component(list_name.as_deref(), &list_id)?
                        .basename,
                );
            }
            components.push(filename.basename);
            ensure!(
                settings.skip_existing,
                "Download overwrite requires an explicit observed target selection"
            );
            rows.push(DownloadTask {
                id,
                profile_id: destination.profile_id.clone(),
                original_record: record,
                resolved_record: None,
                requested_quality: quality,
                actual_quality: actual,
                list_id: Some(list_id.clone()),
                list_name_snapshot: list_name.clone(),
                order: (existing.len() + rows.len()) as u32,
                revision: 1,
                operation_epoch: 1,
                settings: settings.clone(),
                destination: DownloadDestinationBinding {
                    grant_id: destination.id.clone(),
                    grant_epoch: destination.epoch,
                    root_identity: destination.identity.clone(),
                    relative_components: components,
                    collision: CollisionPolicy::Skip,
                },
                state: DownloadState::Waiting,
                pause_reason: None,
                owned_part: None,
                original_transfer: None,
                original_representation: None,
                url_bookmark: None,
                transfer_attempts: 0,
                retry_at_ms: None,
                error: None,
                output_audio: None,
                audio_available: false,
                stages: DownloadStageRecords::for_settings(&settings, actual),
                commit_journal: None,
                created_at_ms: timestamp,
                updated_at_ms: timestamp,
            });
        }
        if !rows.is_empty() {
            self.revision = self
                .repository
                .insert_download_tasks_at(&rows, top, Some(self.revision), &|| Ok(()))?
                .revision;
        }
        Ok(())
    }
}
fn download_now() -> Result<u64> {
    Ok(SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis() as u64)
}

fn prepare_download_output(
    audio: Box<crate::download::VerifiedOwnedAudio>,
    task: &DownloadTask,
    repository: &Repository,
    providers: &ProviderRegistry,
    cancel: &crate::download::DownloadCancel,
    metadata_cancel: Arc<AtomicBool>,
    journal: &dyn crate::download::artifact::OwnedArtifactJournal,
) -> Result<PreparedDownloadOutput> {
    use crate::download::postprocess::{
        prepare_lyric_sidecar_journaled, prepare_tagged_output_journaled,
    };
    use crate::library::tag_write::{LocalTagPatch, TagField, VerifiedTagCover};
    let original_transfer = audio.checkpoint().clone();
    let original_part = audio.owned_record().clone();
    let mut stages = task.stages.clone();
    let wants_lyrics = task.settings.download_lyrics || task.settings.embed_lyrics;
    let mut bundle = if wants_lyrics {
        repository
            .lyrics(&task.original_record.id, LyricOrigin::Edited)?
            .filter(|bundle| bundle.lyric.text().is_some())
            .or(repository.lyrics(&task.original_record.id, LyricOrigin::Raw)?)
    } else {
        None
    };
    if wants_lyrics && bundle.is_none() {
        let platform = match task.original_record.platform.source.as_str() {
            "kw" => Platform::Kw,
            "kg" => Platform::Kg,
            "tx" => Platform::Tx,
            "wy" => Platform::Wy,
            "mg" => Platform::Mg,
            _ => anyhow::bail!("Download lyric platform is unsupported"),
        };
        let context = ProviderContext {
            cancel: metadata_cancel,
            deadline: Instant::now() + Duration::from_secs(15),
        };
        match providers.execute(
            platform,
            ProviderRequest::Lyrics {
                track: task.original_record.clone(),
                word_by_word: true,
            },
            &context,
        ) {
            Ok(ProviderResponse::Lyrics(lyrics)) => {
                let field = |value: Option<String>| {
                    value
                        .map(crate::domain::lyric::LyricField::Text)
                        .unwrap_or(crate::domain::lyric::LyricField::Null)
                };
                bundle = Some(LyricBundle {
                    lyric: crate::domain::lyric::LyricField::Text(lyrics.lyric),
                    tlyric: field(lyrics.translated),
                    rlyric: field(lyrics.romanized),
                    lxlyric: field(lyrics.word_by_word),
                });
            }
            _ => {
                let error = crate::download::DownloadError {
                    kind: crate::download::DownloadErrorKind::Network,
                    message: "Download lyrics could not be fetched".into(),
                    http_status: None,
                };
                if task.settings.embed_lyrics {
                    stages.embedded_lyrics.state = DownloadStageState::Failed;
                    stages.embedded_lyrics.error = Some(error.clone());
                }
                if task.settings.download_lyrics {
                    stages.lyrics.state = DownloadStageState::Failed;
                    stages.lyrics.error = Some(error);
                }
            }
        }
    }
    if let Some(bundle) = &bundle {
        bundle.validate()?;
    }
    let ordinary_available = bundle.as_ref().is_some_and(|bundle| {
        bundle
            .lyric
            .text()
            .is_some_and(|text| !text.trim().is_empty())
    });
    if wants_lyrics && bundle.is_some() && !ordinary_available {
        if task.settings.embed_lyrics
            && stages.embedded_lyrics.state != DownloadStageState::SkippedUnsupportedBaseline
        {
            stages.embedded_lyrics.state = DownloadStageState::SkippedUnavailable;
        }
        if task.settings.download_lyrics {
            stages.lyrics.state = DownloadStageState::SkippedUnavailable;
        }
    }
    let text = |value: &str| {
        if value.is_empty() {
            TagField::Remove
        } else {
            TagField::Set(value.to_owned())
        }
    };
    let artists = task.original_record.artists.join("、").replace('、', ";");
    let mut patch = LocalTagPatch {
        title: text(&task.original_record.title),
        artists: if artists.is_empty() {
            TagField::Remove
        } else {
            TagField::Set(vec![artists])
        },
        album: text(&task.original_record.album),
        ..Default::default()
    };
    if task.settings.embed_lyrics
        && ordinary_available
        && stages.embedded_lyrics.state != DownloadStageState::SkippedUnsupportedBaseline
    {
        let lyrics = crate::download::lyrics::build_download_lyrics(
            bundle.as_ref().unwrap(),
            task.settings.embedded_lyrics,
        )?;
        patch.lyrics = TagField::Set(lyrics.text);
    }
    if task.settings.embed_cover
        && stages.cover.state != DownloadStageState::SkippedUnsupportedBaseline
    {
        let key = crate::domain::artwork::ArtworkKey::new(
            &task.original_record,
            None,
            crate::domain::artwork::ArtworkAuthority::Native,
        )?;
        if let crate::domain::artwork::ArtworkLookup::Hit(info) =
            repository.artwork_info(&key, download_now()?)?
            && let Some(blob) = &info.blob
            && let Some((_, bytes)) =
                repository.stored_artwork_blob(&blob.sha256, download_now()?)?
        {
            ensure!(
                format!("{:x}", Sha256::digest(&bytes)) == blob.sha256,
                "Cached download cover digest changed"
            );
            patch.cover = TagField::Set(VerifiedTagCover::from_bytes(bytes)?);
        }
    }
    let sidecar = if task.settings.download_lyrics && ordinary_available {
        let result = (|| -> Result<_> {
            let artifact = crate::download::lyrics::encode_lyric_artifact(
                bundle.as_ref().unwrap(),
                task.settings.sidecar_lyrics,
                task.settings.lyric_encoding,
                LyricOrigin::Raw,
                task.revision,
            )?;
            prepare_lyric_sidecar_journaled(
                &audio,
                &artifact,
                CollisionPolicy::Skip,
                cancel,
                journal,
            )
        })();
        match result {
            Ok(sidecar) => Some(sidecar),
            Err(_) => {
                stages.lyrics.state = DownloadStageState::Failed;
                stages.lyrics.error = Some(crate::download::DownloadError {
                    kind: crate::download::DownloadErrorKind::Disk,
                    message: "Lyric sidecar preparation failed; verified audio was preserved"
                        .into(),
                    http_status: None,
                });
                None
            }
        }
    } else {
        None
    };
    let tagged = if matches!(
        task.actual_quality,
        DownloadQuality::Wav | DownloadQuality::Ape
    ) {
        None
    } else {
        match prepare_tagged_output_journaled(
            &audio,
            &patch,
            cancel,
            Duration::from_secs(30),
            journal,
        ) {
            Ok(tagged) => Some(tagged),
            Err(_) => {
                stages.metadata.state = DownloadStageState::Failed;
                stages.metadata.error = Some(crate::download::DownloadError {
                    kind: crate::download::DownloadErrorKind::Verification,
                    message: "Tag writing failed; original verified audio was preserved".into(),
                    http_status: None,
                });
                if matches!(patch.cover, TagField::Set(_)) {
                    stages.cover = stages.metadata.clone();
                }
                if matches!(patch.lyrics, TagField::Set(_)) {
                    stages.embedded_lyrics = stages.metadata.clone();
                }
                None
            }
        }
    };
    if tagged.is_some() {
        stages.metadata.state = DownloadStageState::Succeeded;
        if matches!(patch.cover, TagField::Set(_)) {
            stages.cover.state = DownloadStageState::Succeeded;
        }
        if matches!(patch.lyrics, TagField::Set(_)) {
            stages.embedded_lyrics.state = DownloadStageState::Succeeded;
        }
    }
    let (audio, original_audio) = match tagged {
        Some(tagged) => (PreparedAudioOutput::Tagged(Box::new(tagged)), Some(audio)),
        None => (PreparedAudioOutput::Raw(audio), None),
    };
    Ok(PreparedDownloadOutput {
        audio,
        sidecar,
        original_audio,
        original_transfer,
        original_part,
        stages,
    })
}

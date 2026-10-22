use super::{
    DownloadCancel, DownloadContext, DownloadError, DownloadErrorKind, DownloadProgress,
    DownloadState, MAX_DOWNLOAD_TASKS, MAX_DOWNLOAD_WORKERS, OwnedPartRecord, StopReason,
    TransferCheckpoint, TransferJob, TransferOutcome,
    quota::PartialQuota,
    transfer::{TransferView, execute},
};
use crate::audio::worker_owner::{TrackedWorker, WorkerKind, WorkerPermit};
use anyhow::{Result, ensure};
use serde::Serialize;
use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

struct JobState {
    cancel: DownloadCancel,
    view: Arc<Mutex<TransferView>>,
    outcome: Mutex<Option<TransferOutcome>>,
    completed: AtomicBool,
    worker: Mutex<Option<TrackedWorker>>,
    not_started: AtomicBool,
    owner: super::DownloadProducerObject,
    original: OwnedPartRecord,
    initial_checkpoint: Option<TransferCheckpoint>,
}
impl JobState {
    fn joined(&self) -> bool {
        self.worker
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .as_ref()
            .is_some_and(|worker| worker.wait(Duration::ZERO))
            || self.not_started.load(Ordering::Acquire)
                && self.owner.producers_joined().unwrap_or(false)
    }
    fn fail(&self, message: &str) {
        *self
            .outcome
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(TransferOutcome::Failed {
            error: DownloadError::new(DownloadErrorKind::Internal, message),
            part: self.original.clone(),
            checkpoint: self.initial_checkpoint.clone(),
        });
        self.completed.store(true, Ordering::Release);
    }
}
struct Pending {
    job: TransferJob,
    state: Arc<JobState>,
}
struct PoolState {
    waiting: VecDeque<Pending>,
    jobs: Vec<(DownloadContext, Weak<JobState>)>,
    running: Vec<Arc<JobState>>,
    shutdown: bool,
    panicked: bool,
}
struct Shared {
    state: Mutex<PoolState>,
    quota: PartialQuota,
    concurrency: usize,
}
impl Shared {
    fn drive(&self) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let mut still_running = Vec::new();
        let running = std::mem::take(&mut state.running);
        for job in running {
            if job.joined() {
                let panicked = job
                    .worker
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .as_ref()
                    .is_some_and(TrackedWorker::panicked);
                if panicked && !job.completed.load(Ordering::Acquire) {
                    job.fail("Download worker operation panicked");
                }
                if panicked {
                    state.panicked = true;
                }
            } else {
                still_running.push(job);
            }
        }
        state.running = still_running;
        while state.running.len() < self.concurrency {
            let Some(pending) = state.waiting.pop_front() else {
                break;
            };
            if state.shutdown {
                *pending
                    .state
                    .outcome
                    .lock()
                    .unwrap_or_else(|error| error.into_inner()) = Some(TransferOutcome::Stopped {
                    reason: StopReason::Shutdown,
                    part: pending.state.original.clone(),
                    checkpoint: pending.state.initial_checkpoint.clone(),
                });
                pending.state.not_started.store(true, Ordering::Release);
                pending.state.completed.store(true, Ordering::Release);
                continue;
            }
            let permit = match WorkerPermit::reserve(WorkerKind::Download) {
                Ok(permit) => permit,
                Err(_) => {
                    pending.state.not_started.store(true, Ordering::Release);
                    pending.state.fail("Cannot admit actual download worker");
                    continue;
                }
            };
            let ticket = permit.tracker();
            let (start, ready) = std::sync::mpsc::sync_channel(1);
            let job_state = pending.state.clone();
            let worker_state = job_state.clone();
            let spawned = thread::Builder::new()
                .name("download-transfer".into())
                .spawn(move || {
                    if ready.recv().is_err() {
                        return;
                    }
                    let _scope = match super::ProducerWorkScope::enter(ticket) {
                        Ok(scope) => scope,
                        Err(_) => {
                            worker_state.fail("Download producer thread identity differs");
                            return;
                        }
                    };
                    let runtime = match tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                    {
                        Ok(runtime) => runtime,
                        Err(_) => {
                            worker_state.fail("Cannot initialize download worker runtime");
                            return;
                        }
                    };
                    let outcome = runtime.block_on(execute(
                        pending.job,
                        worker_state.cancel.clone(),
                        worker_state.view.clone(),
                    ));
                    let progress_state = match &outcome {
                        TransferOutcome::Verified(_) => DownloadState::CommittingAudio,
                        TransferOutcome::Stopped {
                            reason: StopReason::Cancel,
                            ..
                        } => DownloadState::Cancelled,
                        TransferOutcome::Stopped { .. } => DownloadState::Paused,
                        TransferOutcome::AwaitingRedirectApproval { .. }
                        | TransferOutcome::RefreshRequired { .. } => {
                            DownloadState::AwaitingUrlApproval
                        }
                        TransferOutcome::Failed { .. } => DownloadState::Failed,
                    };
                    worker_state
                        .view
                        .lock()
                        .unwrap_or_else(|error| error.into_inner())
                        .progress
                        .state = progress_state;
                    *worker_state
                        .outcome
                        .lock()
                        .unwrap_or_else(|error| error.into_inner()) = Some(outcome);
                    worker_state.completed.store(true, Ordering::Release);
                });
            let worker = match spawned {
                Ok(handle) => permit.attach(handle),
                Err(_) => {
                    job_state.not_started.store(true, Ordering::Release);
                    job_state.fail("Cannot start actual download worker");
                    continue;
                }
            };
            *job_state
                .worker
                .lock()
                .unwrap_or_else(|error| error.into_inner()) = Some(worker.clone());
            state.running.push(job_state.clone());
            if job_state.owner.bind_worker(worker).is_err() {
                job_state.fail("Cannot bind original native owner to actual download worker");
                continue;
            }
            if start.send(()).is_err() {
                job_state.fail("Actual download start gate failed");
            }
        }
    }
}

pub struct DownloadHandle {
    state: Arc<JobState>,
    pool: Weak<Shared>,
}
impl DownloadHandle {
    pub fn stop(&self, reason: StopReason) {
        self.state.cancel.stop(reason);
    }
    pub fn progress(&self) -> DownloadProgress {
        self.state
            .view
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .progress
            .clone()
    }
    pub fn durable_checkpoint(&self) -> Option<TransferCheckpoint> {
        self.state
            .view
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .checkpoint
            .clone()
    }
    pub fn is_finished(&self) -> bool {
        if let Some(pool) = self.pool.upgrade() {
            pool.drive();
        }
        self.state.completed.load(Ordering::Acquire) && self.state.joined()
    }
    pub fn take_outcome(&self) -> Option<TransferOutcome> {
        if !self.is_finished() {
            return None;
        }
        self.state
            .outcome
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take()
    }
}
impl Drop for DownloadHandle {
    fn drop(&mut self) {
        if !self.state.joined() {
            self.state.cancel.stop(StopReason::Cancel);
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct DownloadPoolStats {
    pub workers: usize,
    pub configured_workers: usize,
    pub active: usize,
    pub queued: usize,
    pub admitted: usize,
    pub known_partial_bytes: u64,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DownloadShutdownError {
    Timeout,
    WorkerPanicked,
}
pub struct DownloadPool {
    shared: Arc<Shared>,
}
impl DownloadPool {
    pub fn new(concurrency: usize) -> Result<Self> {
        ensure!(
            (1..=MAX_DOWNLOAD_WORKERS).contains(&concurrency),
            "Download concurrency must be between one and four"
        );
        Ok(Self {
            shared: Arc::new(Shared {
                state: Mutex::new(PoolState {
                    waiting: VecDeque::new(),
                    jobs: Vec::new(),
                    running: Vec::new(),
                    shutdown: false,
                    panicked: false,
                }),
                quota: PartialQuota::default(),
                concurrency,
            }),
        })
    }
    pub fn with_defaults() -> Result<Self> {
        Self::new(3)
    }
    pub fn account_owned_partial(&self, part: &mut super::OwnedPart) -> Result<()> {
        part.attach_quota(self.shared.quota.clone())
    }
    pub fn submit(&self, mut job: TransferJob) -> Result<DownloadHandle> {
        job.validate()?;
        let context = job.approved_url.context().clone();
        let bytes = job.part.len()?;
        job.part.attach_quota(self.shared.quota.clone())?;
        let state = Arc::new(JobState {
            cancel: DownloadCancel::default(),
            view: Arc::new(Mutex::new(TransferView {
                progress: DownloadProgress {
                    context: context.clone(),
                    state: DownloadState::Waiting,
                    bytes,
                    total: job.checkpoint.as_ref().and_then(|c| c.remote_total),
                    retry_count: 0,
                    representation_resets: 0,
                },
                checkpoint: job.checkpoint.clone(),
            })),
            outcome: Mutex::new(None),
            completed: AtomicBool::new(false),
            worker: Mutex::new(None),
            not_started: AtomicBool::new(false),
            owner: job.part.producer_object(),
            original: job.part.record().clone(),
            initial_checkpoint: job.checkpoint.clone(),
        });
        {
            let mut shared = self
                .shared
                .state
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            ensure!(!shared.shutdown, "Download pool is shutting down");
            shared.jobs.retain(|(_, weak)| weak.strong_count() > 0);
            shared.jobs.retain(|(existing, weak)| {
                !(existing.profile_id == context.profile_id
                    && existing.task_id == context.task_id
                    && weak
                        .upgrade()
                        .is_some_and(|job| job.completed.load(Ordering::Acquire) && job.joined()))
            });
            ensure!(
                shared.jobs.len() < MAX_DOWNLOAD_TASKS,
                "Download task admission bound reached"
            );
            ensure!(
                !shared.jobs.iter().any(|(existing, weak)| {
                    existing.profile_id == context.profile_id
                        && existing.task_id == context.task_id
                        && weak.upgrade().is_some_and(|job| {
                            !job.completed.load(Ordering::Acquire) || !job.joined()
                        })
                }),
                "Download task already owns a transfer slot"
            );
            shared.jobs.push((context, Arc::downgrade(&state)));
            shared.waiting.push_back(Pending {
                job,
                state: state.clone(),
            });
        }
        self.shared.drive();
        Ok(DownloadHandle {
            state,
            pool: Arc::downgrade(&self.shared),
        })
    }
    pub fn cancel_context(&self, source_id: &str, digest: &str, epoch: u64) {
        let shared = self
            .shared
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        for (context, job) in &shared.jobs {
            if context.source_id == source_id
                && context.source_content_sha256 == digest
                && context.policy_epoch == epoch
                && let Some(job) = job.upgrade()
            {
                job.cancel.stop(StopReason::AuthorityRevoked);
            }
        }
    }
    pub fn stats(&self) -> DownloadPoolStats {
        self.shared.drive();
        let state = self
            .shared
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        DownloadPoolStats {
            workers: state.running.len(),
            configured_workers: self.shared.concurrency,
            active: state.running.len(),
            queued: state.waiting.len(),
            admitted: state
                .jobs
                .iter()
                .filter(|(_, job)| job.strong_count() > 0)
                .count(),
            known_partial_bytes: self.shared.quota.bytes(),
        }
    }
    pub fn begin_shutdown(&self) {
        {
            let mut shared = self
                .shared
                .state
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            shared.shutdown = true;
            for (_, weak) in &shared.jobs {
                if let Some(job) = weak.upgrade() {
                    job.cancel.stop(StopReason::Shutdown);
                }
            }
        }
        self.shared.drive();
    }
    pub fn shutdown(&self, timeout: Duration) -> std::result::Result<(), DownloadShutdownError> {
        self.begin_shutdown();
        let started = Instant::now();
        loop {
            self.shared.drive();
            let shared = self
                .shared
                .state
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            if shared.running.is_empty() && shared.waiting.is_empty() {
                return if shared.panicked {
                    Err(DownloadShutdownError::WorkerPanicked)
                } else {
                    Ok(())
                };
            }
            drop(shared);
            if started.elapsed() >= timeout {
                return Err(DownloadShutdownError::Timeout);
            }
            thread::sleep(Duration::from_millis(1));
        }
    }
}
impl Drop for DownloadPool {
    fn drop(&mut self) {
        self.begin_shutdown();
    }
}

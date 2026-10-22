//! Continuous profile-lock ownership across an actual stopped actor and SQLite
//! close. Outcome notification carries no file owner or transferable authority.

use super::*;
use crate::backend::{
    BackendInner, external_control::RegisteredEndpoint, sync_service::RegisteredSyncEndpoint,
};
use crate::library::authority::RetirementBarrier;
use crate::repository::{ProfileGuard, RepositoryCloseOwner};
use std::sync::Condvar;

const MAX_WAIT: Duration = Duration::from_secs(120);
const COMPLETIONS_PER_STEP: usize = 32;

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct ProfileQuiesceScope {
    pub operation_id: String,
    pub expected_revision: u64,
}
pub(crate) struct ProfileQuiesceRequest {
    pub scope: ProfileQuiesceScope,
}
impl ProfileQuiesceRequest {
    pub(in crate::backend) fn new(operation_id: String, expected_revision: u64) -> Result<Self> {
        ensure!(
            !operation_id.is_empty()
                && operation_id.len() <= 128
                && !operation_id.chars().any(char::is_control),
            "Invalid profile handover operation"
        );
        ensure!(
            expected_revision <= i64::MAX as u64,
            "Invalid profile handover scope or observation deadline"
        );
        Ok(Self {
            scope: ProfileQuiesceScope {
                operation_id,
                expected_revision,
            },
        })
    }
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct ProfileQuiesceView {
    pub scope: Option<ProfileQuiesceScope>,
    pub phase: &'static str,
    pub actor_joined: bool,
    pub error: Option<BackendError>,
}
struct SlotState {
    view: ProfileQuiesceView,
    keeper: Option<Arc<Repository>>,
    fatal_error: Option<BackendError>,
    outcome: Option<OwnedQuiesceOutcome>,
    endpoint_scope: Option<ProfileQuiesceScope>,
    external_endpoints: Vec<RegisteredEndpoint>,
    sync_endpoints: Vec<RegisteredSyncEndpoint>,
}
/// BackendInner and the caller's ticket both retain this one bounded slot. A
/// closed receiver, timed-out observer or actor panic cannot drop its keeper.
pub(crate) struct ProfileQuiesceSlot {
    state: Mutex<SlotState>,
    changed: Condvar,
}
impl Default for ProfileQuiesceSlot {
    fn default() -> Self {
        Self {
            state: Mutex::new(SlotState {
                view: ProfileQuiesceView {
                    scope: None,
                    phase: "idle",
                    actor_joined: false,
                    error: None,
                },
                keeper: None,
                fatal_error: None,
                outcome: None,
                endpoint_scope: None,
                external_endpoints: Vec::new(),
                sync_endpoints: Vec::new(),
            }),
            changed: Condvar::new(),
        }
    }
}
pub(in crate::backend) enum OwnedQuiesceOutcome {
    Ready(Box<ProfileHandover>),
    FailedGuard {
        handover: Box<ProfileHandover>,
        error: BackendError,
    },
    Retained(RetainedRepositoryOwner),
}
impl ProfileQuiesceSlot {
    /// Admission must already be closed at BackendInner. The copied tokens
    /// retain actual worker join ownership without capturing BackendInner in
    /// its own actor and creating a shutdown-preventing reference cycle.
    pub(in crate::backend) fn capture_endpoints(
        &self,
        scope: &ProfileQuiesceScope,
        backend: &BackendInner,
    ) -> Result<()> {
        ensure!(
            backend.external_admission_closed.load(Ordering::Acquire)
                && backend.sync_admission_closed.load(Ordering::Acquire),
            "External admission remains open"
        );
        let external = backend
            .external_endpoints
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        let sync = backend
            .sync_endpoints
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        ensure!(
            external.len() <= 4 && sync.len() <= 4,
            "Endpoint handover owner capacity exceeded"
        );
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        ensure!(
            state.view.scope.is_none() && state.endpoint_scope.is_none(),
            "Profile endpoint owners already captured"
        );
        state.view.scope = Some(scope.clone());
        state.view.phase = "admitting";
        state.endpoint_scope = Some(scope.clone());
        state.external_endpoints = external;
        state.sync_endpoints = sync;
        Ok(())
    }
    /// Initial CAS rejection has not retired an endpoint or moved Runtime.
    /// Its descriptive capture can be released before mutation admission opens.
    pub(in crate::backend) fn reject_before_admission(
        &self,
        scope: &ProfileQuiesceScope,
        error: BackendError,
    ) -> Result<()> {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        ensure!(
            state.view.scope.as_ref() == Some(scope)
                && state.view.phase == "admitting"
                && state.keeper.is_none()
                && state.outcome.is_none()
                && state.endpoint_scope.as_ref() == Some(scope),
            "Accepted handover cannot be reset"
        );
        state.view.phase = "rejected";
        state.view.error = Some(error);
        self.changed.notify_all();
        state.endpoint_scope = None;
        state.external_endpoints.clear();
        state.sync_endpoints.clear();
        Ok(())
    }
    fn begin_endpoints_shutdown(&self, scope: &ProfileQuiesceScope) -> Result<()> {
        let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        ensure!(
            state.endpoint_scope.as_ref() == Some(scope),
            "Endpoint owners belong to another handover"
        );
        for endpoint in &state.external_endpoints {
            endpoint.begin_shutdown();
        }
        for endpoint in &state.sync_endpoints {
            endpoint.begin_shutdown();
        }
        Ok(())
    }
    pub(in crate::backend) fn endpoints_joined(&self, scope: &ProfileQuiesceScope) -> Result<bool> {
        let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        ensure!(
            state.endpoint_scope.as_ref() == Some(scope),
            "Endpoint handover scope changed"
        );
        let mut joined = true;
        for endpoint in &state.external_endpoints {
            joined &= endpoint.wait(Duration::ZERO).map_err(anyhow::Error::new)?;
        }
        for endpoint in &state.sync_endpoints {
            joined &= endpoint.wait(Duration::ZERO).map_err(anyhow::Error::new)?;
        }
        Ok(joined)
    }
    pub(in crate::backend) fn view(&self) -> ProfileQuiesceView {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .view
            .clone()
    }
    fn admit(&self, scope: &ProfileQuiesceScope, repository: Arc<Repository>) -> Result<()> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        ensure!(
            state.view.scope.as_ref() == Some(scope) && state.view.phase == "admitting",
            "A profile lifecycle operation already owns the slot"
        );
        state.view.scope = Some(scope.clone());
        state.view.phase = "joining";
        state.keeper = Some(repository);
        self.changed.notify_all();
        Ok(())
    }
    pub(in crate::backend) fn observe(
        &self,
        scope: &ProfileQuiesceScope,
        deadline: Instant,
    ) -> Result<ProfileQuiesceView> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        ensure!(
            state.view.scope.as_ref() == Some(scope),
            "Profile handover operation changed"
        );
        while state.view.phase != "ready"
            && state.view.phase != "rejected"
            && state.view.error.is_none()
        {
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            state = self
                .changed
                .wait_timeout(state, deadline.saturating_duration_since(now))
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
        // Timeout changes neither admission nor an actual owner.
        Ok(state.view.clone())
    }
    pub(in crate::backend) fn mark_actor_joined(&self) {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .view
            .actor_joined = true;
        self.changed.notify_all();
    }
    pub(in crate::backend) fn record_fatal(&self, error: BackendError) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.fatal_error.get_or_insert(error);
        state.view.error = state.fatal_error.clone();
        self.changed.notify_all();
    }
    pub(in crate::backend) fn record_error(&self, error: BackendError) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.view.error = state.fatal_error.clone().or(Some(error));
        self.changed.notify_all();
    }
    pub(in crate::backend) fn publish_retained(&self, owner: RetainedRepositoryOwner) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.view.phase = "close_pending";
        state.outcome = Some(OwnedQuiesceOutcome::Retained(owner));
        state.keeper = None;
        self.changed.notify_all();
    }
    pub(in crate::backend) fn retry_close(&self) -> bool {
        let outcome = {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            let Some(outcome) = state.outcome.take() else {
                return false;
            };
            outcome
        };
        let next = match outcome {
            OwnedQuiesceOutcome::Retained(owner) => owner.retry_close(),
            other => other,
        };
        let closed = matches!(
            next,
            OwnedQuiesceOutcome::Ready(_) | OwnedQuiesceOutcome::FailedGuard { .. }
        );
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.view.phase = match &next {
            OwnedQuiesceOutcome::Ready(_) => "ready",
            OwnedQuiesceOutcome::FailedGuard { .. } => "failed_guard",
            OwnedQuiesceOutcome::Retained(_) => "close_pending",
        };
        state.view.error = match &next {
            OwnedQuiesceOutcome::Retained(owner) => owner.error.clone(),
            OwnedQuiesceOutcome::FailedGuard { error, .. } => Some(error.clone()),
            _ => None,
        };
        if let Some(error) = state.fatal_error.clone() {
            state.view.error = Some(error);
        }
        state.outcome = Some(next);
        self.changed.notify_all();
        closed
    }
    pub(in crate::backend) fn take_after_actor_join(
        &self,
        scope: &ProfileQuiesceScope,
    ) -> Result<Option<OwnedQuiesceOutcome>> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        ensure!(
            state.view.scope.as_ref() == Some(scope),
            "Profile handover operation changed"
        );
        ensure!(
            state.view.actor_joined && state.view.error.is_none(),
            "Actual actor join is not confirmed"
        );
        Ok(state.outcome.take())
    }
    fn take_failed_guard_after_join(&self, scope: &ProfileQuiesceScope) -> Result<ProfileGuard> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        ensure!(
            state.view.scope.as_ref() == Some(scope)
                && state.view.actor_joined
                && state.view.phase == "failed_guard",
            "Failed profile guard is not actually closed and joined"
        );
        match state.outcome.take() {
            Some(OwnedQuiesceOutcome::FailedGuard { handover, .. }) => Ok(handover.guard),
            other => {
                state.outcome = other;
                anyhow::bail!("Failed profile guard is already taken");
            }
        }
    }
    /// Only after Runtime itself has released all service references. The
    /// supplied local Arc must outlive removal of this panic-protection keeper.
    fn release_keeper_for_close(
        &self,
        scope: &ProfileQuiesceScope,
        repository: &Arc<Repository>,
    ) -> Result<()> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        ensure!(
            state.view.scope.as_ref() == Some(scope)
                && state
                    .keeper
                    .as_ref()
                    .is_some_and(|keeper| Arc::ptr_eq(keeper, repository)),
            "Profile keeper differs from the stopped runtime"
        );
        state.keeper = None;
        state.view.phase = "closing";
        Ok(())
    }
    fn verify_keeper(
        &self,
        scope: &ProfileQuiesceScope,
        repository: &Arc<Repository>,
    ) -> Result<()> {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        ensure!(
            state.view.scope.as_ref() == Some(scope)
                && state
                    .keeper
                    .as_ref()
                    .is_some_and(|keeper| Arc::ptr_eq(keeper, repository)),
            "Profile handover is not owned by this stopped actor"
        );
        Ok(())
    }
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct ProfileCheckpointSeal {
    pub repository_revision: u64,
    pub store_digest: String,
    pub checkpoint_digest: String,
}
pub struct ProfileHandover {
    pub scope: ProfileQuiesceScope,
    pub checkpoint: ProfileCheckpointSeal,
    pub guard: ProfileGuard,
}
pub(in crate::backend) struct RetainedRepositoryOwner {
    pub scope: ProfileQuiesceScope,
    pub checkpoint: ProfileCheckpointSeal,
    pub owner: RepositoryCloseOwner,
    retirement: RetirementBarrier,
    fatal_error: Option<BackendError>,
    pub error: Option<BackendError>,
}
impl RetainedRepositoryOwner {
    /// This does not select another directory or reacquire a profile lock.
    pub(in crate::backend) fn retry_close(self) -> OwnedQuiesceOutcome {
        if !self.retirement.wait(Duration::ZERO) {
            return OwnedQuiesceOutcome::Retained(self);
        }
        let Self {
            scope,
            checkpoint,
            owner,
            retirement,
            fatal_error,
            ..
        } = self;
        let closed = match owner {
            RepositoryCloseOwner::Shared(repository) => repository.close_into_guard(),
            RepositoryCloseOwner::Exclusive(repository) => {
                Arc::<Repository>::from(repository).close_into_guard()
            }
            RepositoryCloseOwner::Guard(mut guard) => {
                return match guard.finish_close() {
                    Ok(()) => closed_outcome(
                        ProfileHandover {
                            scope,
                            checkpoint,
                            guard: *guard,
                        },
                        fatal_error,
                    ),
                    Err(error) => OwnedQuiesceOutcome::Retained(Self {
                        scope,
                        checkpoint,
                        owner: RepositoryCloseOwner::Guard(guard),
                        retirement,
                        fatal_error,
                        error: Some(handover_error(&error)),
                    }),
                };
            }
        };
        match closed {
            Ok(guard) => closed_outcome(
                ProfileHandover {
                    scope,
                    checkpoint,
                    guard,
                },
                fatal_error,
            ),
            Err(failure) => OwnedQuiesceOutcome::Retained(Self {
                scope,
                checkpoint,
                owner: failure.owner,
                retirement,
                fatal_error,
                error: Some(handover_error(&failure.error)),
            }),
        }
    }
}
fn closed_outcome(handover: ProfileHandover, error: Option<BackendError>) -> OwnedQuiesceOutcome {
    match error {
        Some(error) => OwnedQuiesceOutcome::FailedGuard {
            handover: Box::new(handover),
            error,
        },
        None => OwnedQuiesceOutcome::Ready(Box::new(handover)),
    }
}
pub(in crate::backend) struct ProfileQuiesceSeed {
    pub(in crate::backend) scope: ProfileQuiesceScope,
    checkpoint: PlaybackCheckpoint,
    begin_error: Option<BackendError>,
    pub(in crate::backend) fatal_error: Option<BackendError>,
}
pub(in crate::backend) struct RuntimeQuiesceFailure {
    pub error: BackendError,
    pub runtime: Box<Runtime>,
    pub seed: Box<ProfileQuiesceSeed>,
}

impl Runtime {
    /// The actor calls this after closing mutation admission at BackendInner.
    /// Capture presentation position before cancellation resets the audio clock.
    pub(in crate::backend) fn begin_profile_quiesce(
        &mut self,
        request: &ProfileQuiesceRequest,
        slot: &ProfileQuiesceSlot,
    ) -> Result<ProfileQuiesceSeed> {
        if self.repository.revision()? != request.scope.expected_revision
            || self.revision != request.scope.expected_revision
        {
            return Err(BackendError::new(
                ErrorKind::Conflict,
                "Profile handover revision changed",
            )
            .into());
        }
        let seed = ProfileQuiesceSeed {
            scope: request.scope.clone(),
            checkpoint: self.checkpoint(self.queue.clone(), self.playback_position()),
            begin_error: None,
            fatal_error: None,
        };
        slot.admit(&request.scope, self.repository.clone())?;
        Ok(seed)
    }
    pub(in crate::backend) fn begin_profile_quiesce_services(
        &mut self,
        seed: &mut ProfileQuiesceSeed,
        slot: &ProfileQuiesceSlot,
    ) -> Result<()> {
        slot.begin_endpoints_shutdown(&seed.scope)?;
        self.external_control.retire();
        if let Err(error) = self.retire_sync_service() {
            seed.begin_error = Some(error.clone());
            return Err(anyhow::Error::new(error));
        }
        self.deep_links.begin_shutdown();
        self.begin_effect_shutdown();
        self.begin_system_control_shutdown()?;
        self.begin_instance_relay_shutdown();
        self.begin_tag_shutdown();
        self.begin_download_shutdown();
        self.begin_cleanup_shutdown();
        self.cancel_runtime();
        for job in self.provider_jobs.values() {
            job.cancel.store(true, Ordering::Release);
        }
        for cancel in self.import_cancels.values() {
            cancel.terminate();
        }
        for cancel in self.file_cancels.values() {
            cancel.store(true, Ordering::Release);
        }
        self.cancel_all_picture_requests();
        self.backup_exports.begin_shutdown();
        self.physical_snapshots.begin_shutdown();
        self.pictures.begin_shutdown();
        self.shutdown_library_operations();
        seed.begin_error = None;
        Ok(())
    }
    /// Polls actual service joins without discarding incomplete owners.
    /// Every false result or error retains the Runtime for another observation.
    pub(in crate::backend) fn poll_profile_quiesce(
        &mut self,
        seed: &mut ProfileQuiesceSeed,
    ) -> Result<bool> {
        for _ in 0..COMPLETIONS_PER_STEP {
            let Ok(completion) = self.completed_rx.try_recv() else {
                break;
            };
            self.dispatch_completion(completion);
        }
        self.poll_downloads();
        self.poll_cleanup();
        self.poll_backup_exports();
        self.poll_physical_snapshots();
        if self.reap_shutdown_tasks() {
            seed.fatal_error.get_or_insert_with(|| {
                BackendError::new(
                    ErrorKind::OperationFailed,
                    "A profile handover worker panicked",
                )
            });
        }
        let mut ready = self.tasks.is_empty() && self.provider_tasks.is_empty();
        ready &= self.poll_download_quiesce_step()?;
        ready &= self.retry_cleanup_retirement();
        ready &= self.retire_physical_snapshot_owners()?;
        ready &= self.poll_system_control_quiesce_step()?;
        ready &= self.poll_instance_relay_quiesce_step()?;
        ready &= self.poll_tag_quiesce_step()?;
        ready &= self.poll_effect_quiesce()?;
        ready &= !self.backup_exports.has_running()
            && self.backup_exports.retained_count() == 0
            && !self.physical_snapshots.has_running()
            && self.physical_snapshots.retained_count() == 0;
        let audio = self.audio.shutdown(Duration::ZERO);
        match audio {
            Ok(()) => {}
            Err(error)
                if error
                    .downcast_ref::<crate::audio::AudioShutdownError>()
                    .is_some_and(|error| error.is_timeout()) =>
            {
                ready = false
            }
            Err(error) => return Err(error),
        }
        // Catalog closes its real server and releases asset readers before the
        // library/authority retirement barriers can become empty.
        ready &= self.catalog.shutdown(Duration::ZERO)?;
        ready &= self.pictures.shutdown(Duration::ZERO);
        ready &= self.library.shutdown(Duration::ZERO)?;
        Ok(ready)
    }
    /// Failure returns the whole Runtime, including every surviving service and
    /// file owner. Only successful actual joins permit consuming its services.
    pub(in crate::backend) fn into_handover_repository(
        mut self,
        scope: ProfileQuiesceScope,
        mut seed: ProfileQuiesceSeed,
        slot: &ProfileQuiesceSlot,
    ) -> std::result::Result<RetainedRepositoryOwner, RuntimeQuiesceFailure> {
        let sealed = (|| -> Result<ProfileCheckpointSeal> {
            ensure!(
                scope == seed.scope,
                "Profile handover seed belongs to another operation"
            );
            slot.verify_keeper(&scope, &self.repository)?;
            ensure!(
                slot.endpoints_joined(&scope)?,
                "External endpoint workers are still joining"
            );
            ensure!(
                self.poll_profile_quiesce(&mut seed)?,
                "Profile services are still joining"
            );
            ensure!(
                seed.begin_error.is_none(),
                "Initial profile retirement failed: {:?}",
                seed.begin_error
            );
            let saved = self.store.load()?;
            let store_digest = semantic_digest(&saved)?;
            ensure!(
                store_digest == semantic_digest(&self.data)?,
                "Actual player state differs from the stopped actor"
            );
            let revision = self.repository.revision()?;
            self.repository.save_playback_checkpoint(
                &seed.checkpoint,
                Some(revision),
                "profile_handover",
            )?;
            let reread = self
                .repository
                .playback_checkpoint()?
                .context("Final handover checkpoint is missing")?;
            let checkpoint_digest = format!("{:x}", Sha256::digest(serde_json::to_vec(&reread)?));
            ensure!(
                checkpoint_digest
                    == format!(
                        "{:x}",
                        Sha256::digest(serde_json::to_vec(&seed.checkpoint)?)
                    ),
                "Final handover checkpoint readback differs"
            );
            ensure!(
                semantic_digest(&self.store.load()?)? == store_digest,
                "Player state changed during handover sealing"
            );
            Ok(ProfileCheckpointSeal {
                repository_revision: self.repository.revision()?,
                store_digest,
                checkpoint_digest,
            })
        })();
        let checkpoint = match sealed {
            Ok(seal) => seal,
            Err(error) => {
                return Err(RuntimeQuiesceFailure {
                    error: handover_error(&error),
                    runtime: Box::new(self),
                    seed: Box::new(seed),
                });
            }
        };
        let repository = self.repository.clone();
        let retirement = self.library.authority.retirement_barrier();
        // This only drops services after actual joins. Repository remains alive
        // in this local owner and in the shared outcome slot throughout Drop.
        drop(self);
        if let Err(error) = slot.release_keeper_for_close(&scope, &repository) {
            return Ok(RetainedRepositoryOwner {
                scope,
                checkpoint,
                owner: RepositoryCloseOwner::Shared(repository),
                retirement,
                fatal_error: seed.fatal_error.clone(),
                error: Some(handover_error(&error)),
            });
        }
        Ok(RetainedRepositoryOwner {
            scope,
            checkpoint,
            owner: RepositoryCloseOwner::Shared(repository),
            retirement,
            fatal_error: seed.fatal_error,
            error: None,
        })
    }
}
fn semantic_digest(data: &StoreData) -> Result<String> {
    Ok(format!("{:x}", Sha256::digest(serde_json::to_vec(data)?)))
}
fn handover_error(error: &dyn std::fmt::Display) -> BackendError {
    BackendError::new(
        ErrorKind::OperationFailed,
        error.to_string().chars().take(1024).collect::<String>(),
    )
}

pub struct ProfileQuiesceTicket {
    scope: ProfileQuiesceScope,
    slot: Arc<ProfileQuiesceSlot>,
    lifecycle: startup_ownership::ActorLifecycle,
}
impl std::fmt::Debug for ProfileQuiesceTicket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProfileQuiesceTicket")
            .field("scope", &self.scope)
            .field("view", &self.slot.view())
            .finish()
    }
}
pub struct ProfileQuiesceFailure {
    pub error: BackendError,
    pub owner: ProfileQuiesceTicket,
}
impl std::fmt::Debug for ProfileQuiesceFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProfileQuiesceFailure")
            .field("error", &self.error)
            .field("owner", &self.owner)
            .finish()
    }
}
impl std::fmt::Display for ProfileQuiesceFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.error)
    }
}
impl std::error::Error for ProfileQuiesceFailure {}
impl ProfileQuiesceTicket {
    pub(in crate::backend) fn new(
        scope: ProfileQuiesceScope,
        slot: Arc<ProfileQuiesceSlot>,
        lifecycle: startup_ownership::ActorLifecycle,
    ) -> Self {
        Self {
            scope,
            slot,
            lifecycle,
        }
    }
    pub fn scope(&self) -> &ProfileQuiesceScope {
        &self.scope
    }
    pub fn view(&self) -> ProfileQuiesceView {
        self.slot.view()
    }
    pub fn observe(&self, wait: Duration) -> Result<ProfileQuiesceView, BackendError> {
        if wait > MAX_WAIT {
            return Err(BackendError::new(
                ErrorKind::InvalidData,
                "Profile observation exceeds its bound",
            ));
        }
        let deadline = Instant::now().checked_add(wait).ok_or_else(|| {
            BackendError::new(
                ErrorKind::InvalidData,
                "Profile observation deadline overflow",
            )
        })?;
        loop {
            let retired = self.lifecycle.poll().map_err(backend_error)?;
            let view = self.slot.view();
            if (view.phase == "ready" && retired)
                || view.phase == "rejected"
                || view.error.is_some()
                || Instant::now() >= deadline
            {
                return Ok(view);
            }
            if view.phase == "ready" {
                thread::sleep(Duration::from_millis(1));
            }
            self.slot
                .observe(
                    &self.scope,
                    (Instant::now() + Duration::from_millis(20)).min(deadline),
                )
                .map_err(backend_error)?;
        }
    }
    pub fn take_failed_guard(
        self,
        wait: Duration,
    ) -> std::result::Result<ProfileGuard, ProfileQuiesceFailure> {
        let result = (|| -> Result<ProfileGuard, BackendError> {
            if wait > MAX_WAIT {
                return Err(BackendError::new(
                    ErrorKind::InvalidData,
                    "Profile observation exceeds its bound",
                ));
            }
            let deadline = Instant::now().checked_add(wait).ok_or_else(|| {
                BackendError::new(
                    ErrorKind::InvalidData,
                    "Profile observation deadline overflow",
                )
            })?;
            loop {
                let retired = self.lifecycle.poll().map_err(backend_error)?;
                let view = self.slot.view();
                if retired && view.phase == "failed_guard" {
                    return self
                        .slot
                        .take_failed_guard_after_join(&self.scope)
                        .map_err(backend_error);
                }
                if view.phase == "ready" || view.phase == "rejected" {
                    return Err(view.error.unwrap_or_else(|| {
                        BackendError::new(ErrorKind::Conflict, "Profile scope has no failed guard")
                    }));
                }
                if Instant::now() >= deadline {
                    return Err(view.error.unwrap_or_else(|| {
                        BackendError::new(
                            ErrorKind::Timeout,
                            "Profile failure still retains its actual runtime",
                        )
                    }));
                }
                thread::sleep(Duration::from_millis(5));
            }
        })();
        result.map_err(|error| ProfileQuiesceFailure { error, owner: self })
    }
    pub fn take_handover(
        self,
        wait: Duration,
    ) -> std::result::Result<ProfileHandover, ProfileQuiesceFailure> {
        let result = (|| -> Result<ProfileHandover, BackendError> {
            let view = self.observe(wait)?;
            if view.phase != "ready" {
                return Err(view.error.unwrap_or_else(|| {
                    BackendError::new(
                        ErrorKind::Timeout,
                        "Profile services remain owned by this quiesce ticket",
                    )
                }));
            }
            let outcome = self
                .slot
                .take_after_actor_join(&self.scope)
                .map_err(backend_error)?;
            match outcome {
                Some(OwnedQuiesceOutcome::Ready(owner)) => Ok(*owner),
                _ => Err(BackendError::new(
                    ErrorKind::Conflict,
                    "Profile handover is already taken",
                )),
            }
        })();
        result.map_err(|error| ProfileQuiesceFailure { error, owner: self })
    }
}
impl std::fmt::Debug for ProfileHandover {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProfileHandover")
            .field("scope", &self.scope)
            .field("checkpoint", &self.checkpoint)
            .finish_non_exhaustive()
    }
}
#[cfg(all(test, windows))]
#[path = "profile_handover_close_tests.rs"]
mod close_tests;

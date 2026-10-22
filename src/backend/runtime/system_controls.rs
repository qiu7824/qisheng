use super::*;
#[cfg(windows)]
use crate::system_control::{ManagedWorker, SourceArtworkOwner};
use crate::system_control::{
    SourceArtworkBinding, SourceArtworkLease, SystemAction, SystemArtwork, SystemBackend,
    SystemControlView, SystemDispatcher, SystemInput, SystemInputFence, SystemPlaybackState,
    SystemPowerEvent, SystemProjection, SystemSettings,
};
#[cfg(windows)]
use crate::system_control::{
    SystemConfigurationAck, SystemConfigurationOutcome, WindowsSystemOwner,
};

const MAX_RETIRED_SYSTEM_OWNERS: usize = 4;
const SYSTEM_OBSERVATION_TIMEOUT: Duration = Duration::from_secs(6);

struct BackendBridge {
    commands: mpsc::SyncSender<RuntimeMessage>,
    shutdown: Arc<AtomicBool>,
}
struct InputCancellation(Arc<AtomicBool>);
impl Drop for InputCancellation {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}
impl SystemBackend for BackendBridge {
    fn dispatch_system_input(&self, input: SystemInput, fence: SystemInputFence) -> Result<()> {
        fence.validate(input)?;
        ensure!(
            !self.shutdown.load(Ordering::Acquire),
            "Player backend is stopping"
        );
        let cancellation = InputCancellation(Arc::new(AtomicBool::new(false)));
        let (reply, receiver) = mpsc::sync_channel(1);
        self.commands
            .try_send(RuntimeMessage::SystemInput {
                input,
                fence: fence.clone(),
                cancel: cancellation.0.clone(),
                reply,
            })
            .map_err(|error| match error {
                mpsc::TrySendError::Full(_) => {
                    BackendError::new(ErrorKind::Busy, "System command queue is full")
                }
                mpsc::TrySendError::Disconnected(_) => {
                    BackendError::new(ErrorKind::Closed, "Player backend stopped")
                }
            })?;
        let deadline = Instant::now() + SYSTEM_OBSERVATION_TIMEOUT;
        loop {
            fence.validate(input)?;
            ensure!(
                !self.shutdown.load(Ordering::Acquire),
                "Player backend is stopping"
            );
            match receiver.recv_timeout(Duration::from_millis(20)) {
                Ok(result) => return result.map_err(Into::into),
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    anyhow::bail!("Player stopped before acknowledging system input")
                }
                Err(mpsc::RecvTimeoutError::Timeout) => ensure!(
                    Instant::now() < deadline,
                    "System command acknowledgement timed out; query operation state"
                ),
            }
        }
    }
}

pub(super) struct SystemControlRuntime {
    bridge: Arc<BackendBridge>,
    closing: bool,
    store_frozen: bool,
    mutation_paused: Arc<AtomicBool>,
    internal_store_write: bool,
    persistence_error: Option<String>,
    native_error: Option<String>,
    last_view: Option<String>,
    navigation: Option<(String, bool, bool)>,
    store_recovery: Option<PendingStoreRecovery>,
    #[cfg(windows)]
    epoch: u64,
    #[cfg(windows)]
    active: Option<SystemHolder>,
    #[cfg(windows)]
    retired: Vec<RetiredSystemHolder>,
    #[cfg(windows)]
    pending: Option<PendingConfiguration>,
    #[cfg(windows)]
    artwork: Option<SourceArtworkLease>,
    #[cfg(windows)]
    retired_artwork: Vec<SourceArtworkLease>,
    #[cfg(windows)]
    artwork_checked: Option<(Instant, ArtworkContext)>,
    #[cfg(windows)]
    last_projection: Option<String>,
    #[cfg(windows)]
    artwork_job: Option<ArtworkGrantJob>,
}
impl SystemControlRuntime {
    pub(super) fn new(
        commands: mpsc::SyncSender<RuntimeMessage>,
        shutdown: Arc<AtomicBool>,
    ) -> Self {
        Self {
            bridge: Arc::new(BackendBridge { commands, shutdown }),
            closing: false,
            store_frozen: false,
            mutation_paused: Arc::new(AtomicBool::new(false)),
            internal_store_write: false,
            persistence_error: None,
            native_error: None,
            last_view: None,
            navigation: None,
            store_recovery: None,
            #[cfg(windows)]
            epoch: 0,
            #[cfg(windows)]
            active: None,
            #[cfg(windows)]
            retired: Vec::with_capacity(MAX_RETIRED_SYSTEM_OWNERS),
            #[cfg(windows)]
            pending: None,
            #[cfg(windows)]
            artwork: None,
            #[cfg(windows)]
            retired_artwork: Vec::with_capacity(MAX_RETIRED_SYSTEM_OWNERS),
            #[cfg(windows)]
            artwork_checked: None,
            #[cfg(windows)]
            last_projection: None,
            #[cfg(windows)]
            artwork_job: None,
        }
    }
    pub(super) fn require_store_mutation(&self) -> Result<()> {
        ensure!(
            !self.store_frozen || self.internal_store_write,
            BackendError::new(
                ErrorKind::Storage,
                "System configuration retains an unresolved durable settings transaction"
            )
        );
        Ok(())
    }
    fn set_store_frozen(&mut self, frozen: bool) {
        self.mutation_paused.store(frozen, Ordering::Release);
        self.store_frozen = frozen;
    }
    #[cfg(all(test, windows))]
    pub(super) fn test_set_store_frozen(&mut self, frozen: bool) {
        self.set_store_frozen(frozen);
    }
    pub(super) fn mutation_gate(&self) -> Arc<AtomicBool> {
        self.mutation_paused.clone()
    }
    pub(super) fn owns_operation(&self, operation: &str) -> bool {
        if self
            .store_recovery
            .as_ref()
            .is_some_and(|ticket| ticket.operation == operation)
        {
            return true;
        }
        #[cfg(windows)]
        if self
            .pending
            .as_ref()
            .is_some_and(|pending| pending.operation.as_deref() == Some(operation))
        {
            return true;
        }
        false
    }
}
struct PendingStoreRecovery {
    operation: String,
    old: SystemSettings,
    requested: SystemSettings,
    error: BackendError,
    next_attempt: Instant,
}

#[cfg(windows)]
struct SystemHolder {
    owner: WindowsSystemOwner,
    dispatcher: SystemDispatcher,
    fence: SystemInputFence,
    native_joined: bool,
    dispatcher_joined: bool,
    retirement_error: Option<String>,
}
#[cfg(windows)]
enum RetiredSystemHolder {
    Native(Box<SystemHolder>),
    Dispatcher {
        dispatcher: SystemDispatcher,
        error: Option<String>,
    },
    Artwork(Box<ArtworkGrantJob>),
}
#[cfg(windows)]
impl RetiredSystemHolder {
    fn begin_shutdown(&self) {
        match self {
            Self::Native(holder) => holder.begin_shutdown(),
            Self::Dispatcher { dispatcher, .. } => dispatcher.begin_shutdown(),
            Self::Artwork(job) => job.cancel.store(true, Ordering::Release),
        }
    }
    fn join_retired(&mut self) -> Result<bool> {
        match self {
            Self::Native(holder) => holder.join_retired(),
            Self::Dispatcher { dispatcher, error } => {
                ensure!(
                    error.is_none(),
                    "Native dispatcher retirement requires recovery after worker panic"
                );
                match dispatcher.shutdown(Duration::ZERO) {
                    Ok(joined) => Ok(joined),
                    Err(failure) => {
                        *error = Some(bounded_error(&failure));
                        Err(failure)
                    }
                }
            }
            Self::Artwork(job) => job.join_retired(),
        }
    }
}
#[cfg(windows)]
impl SystemHolder {
    fn begin_shutdown(&self) {
        self.dispatcher.begin_shutdown();
        self.owner.begin_shutdown();
    }
    fn join_retired(&mut self) -> Result<bool> {
        self.begin_shutdown();
        let mut failure = None;
        if !self.native_joined {
            match self.owner.join_retired() {
                Ok(joined) => self.native_joined = joined,
                Err(error) => failure = Some(error),
            }
        }
        if !self.dispatcher_joined {
            match self.dispatcher.shutdown(Duration::ZERO) {
                Ok(joined) => self.dispatcher_joined = joined,
                Err(error) => {
                    if failure.is_none() {
                        failure = Some(error);
                    }
                }
            }
        }
        if let Some(error) = failure {
            self.retirement_error = Some(bounded_error(&error));
            return Err(error);
        }
        ensure!(
            self.retirement_error.is_none(),
            "System owner requires recovery after its worker panicked"
        );
        Ok(self.native_joined && self.dispatcher_joined)
    }
}
#[cfg(windows)]
#[derive(Clone, Copy, PartialEq, Eq)]
enum PendingMode {
    Native,
    RepairStore,
    Rollback,
    RestartOld,
    RetireNewOwner,
}
#[cfg(windows)]
fn recovery_from_native_ack(ack: &SystemConfigurationAck) -> PendingMode {
    if ack.requested.enabled {
        if ack
            .previous
            .as_ref()
            .is_some_and(|previous| previous.enabled)
        {
            PendingMode::Rollback
        } else {
            PendingMode::RetireNewOwner
        }
    } else {
        PendingMode::RestartOld
    }
}
#[cfg(windows)]
struct PendingConfiguration {
    operation: Option<String>,
    old: SystemSettings,
    requested: SystemSettings,
    revision: u64,
    mode: PendingMode,
    failure: Option<BackendError>,
    durable_committed: bool,
    next_attempt: Instant,
}
#[cfg(windows)]
#[derive(Clone, PartialEq, Eq)]
struct ArtworkContext {
    generation: u64,
    track: Option<String>,
    source_epoch: u64,
    catalog_revision: u64,
    repository_revision: u64,
    pictures: (u64, u64),
}
#[cfg(windows)]
#[derive(Clone)]
struct CurrentSystemArtwork {
    binding: SourceArtworkBinding,
    sha256: String,
    bytes: Arc<[u8]>,
    physical_asset: Option<String>,
}
#[cfg(windows)]
impl CurrentSystemArtwork {
    fn same_authority(&self, other: &Self) -> bool {
        self.binding == other.binding
            && self.sha256 == other.sha256
            && self.physical_asset == other.physical_asset
    }
}
#[cfg(windows)]
struct PhysicalArtworkOwner {
    cancel: Arc<AtomicBool>,
    live: AtomicBool,
}
#[cfg(windows)]
impl SourceArtworkOwner for PhysicalArtworkOwner {
    fn is_current(&self) -> bool {
        self.live.load(Ordering::Acquire) && !self.cancel.load(Ordering::Acquire)
    }
    fn begin_retirement(&self) {
        self.cancel.store(true, Ordering::Release);
    }
}
#[cfg(windows)]
struct PhysicalArtworkLive(Arc<PhysicalArtworkOwner>);
#[cfg(windows)]
impl Drop for PhysicalArtworkLive {
    fn drop(&mut self) {
        self.0.live.store(false, Ordering::Release);
    }
}
#[cfg(windows)]
struct ArtworkGrantJob {
    candidate: CurrentSystemArtwork,
    context: ArtworkContext,
    cancel: Arc<AtomicBool>,
    receiver: mpsc::Receiver<Result<Arc<PhysicalArtworkOwner>>>,
    worker: Option<ManagedWorker>,
    accepted: bool,
    retirement_error: Option<String>,
}
#[cfg(windows)]
impl ArtworkGrantJob {
    fn start(
        candidate: CurrentSystemArtwork,
        context: ArtworkContext,
        repository: Arc<Repository>,
        authority: crate::library::authority::AuthorityRegistry,
    ) -> Result<Self> {
        let asset_id = candidate
            .physical_asset
            .clone()
            .context("Physical artwork proof has no authorized asset")?;
        let cancel = Arc::new(AtomicBool::new(false));
        let cancellation = cancel.clone();
        let (sender, receiver) = mpsc::sync_channel(1);
        let worker = ManagedWorker::spawn("lx-system-artwork-proof", move || {
            let acquired = (|| -> Result<_> {
                ensure!(
                    !cancellation.load(Ordering::Acquire),
                    "Physical artwork validation was cancelled"
                );
                let authorization =
                    library::file_authorization(&repository, &authority, &asset_id)?
                        .with_cancel(cancellation.clone())?;
                let file = authorization.open()?;
                file.check()?;
                ensure!(
                    !cancellation.load(Ordering::Acquire),
                    "Physical artwork validation was cancelled"
                );
                Ok((authorization, file))
            })();
            match acquired {
                Err(error) => {
                    let _ = sender.send(Err(error));
                }
                Ok((authorization, file)) => {
                    let control = Arc::new(PhysicalArtworkOwner {
                        cancel: cancellation.clone(),
                        live: AtomicBool::new(true),
                    });
                    let live = PhysicalArtworkLive(control.clone());
                    if sender.send(Ok(control.clone())).is_ok() {
                        // The physical FH stays on this worker while any lease,
                        // queued native projection or thumbnail pin owns its gate.
                        while Arc::strong_count(&control) > 2 {
                            thread::sleep(Duration::from_millis(10));
                        }
                    }
                    drop(file);
                    drop(authorization);
                    drop(live);
                }
            }
        })?;
        Ok(Self {
            candidate,
            context,
            cancel,
            receiver,
            worker: Some(worker),
            accepted: false,
            retirement_error: None,
        })
    }
    fn join_retired(&mut self) -> Result<bool> {
        self.cancel.store(true, Ordering::Release);
        while self.receiver.try_recv().is_ok() {}
        ensure!(
            self.retirement_error.is_none(),
            "Physical artwork worker retirement requires recovery"
        );
        if self
            .worker
            .as_ref()
            .is_some_and(|worker| !worker.is_finished())
        {
            return Ok(false);
        }
        if let Some(mut worker) = self.worker.take()
            && let Err(error) = worker.join_finished()
        {
            self.retirement_error = Some(bounded_error(&error));
            return Err(error);
        }
        Ok(true)
    }
}
#[cfg(windows)]
impl Drop for ArtworkGrantJob {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Release);
        while self.receiver.try_recv().is_ok() {}
        // ManagedWorker transfers a still-running actual handle to its bounded
        // supervisor; it never treats a dropped observer as a completed join.
    }
}

impl Runtime {
    pub(super) fn system_control_settings(&self) -> SystemSettings {
        self.data.system_control.clone()
    }
    pub(super) fn system_control_state(&self) -> SystemControlView {
        let mut view = SystemControlView {
            platform_supported: cfg!(windows),
            persisted_settings: self.data.system_control.clone(),
            native_error: self.system_control.native_error.clone(),
            persistence_error: self.system_control.persistence_error.clone(),
            store_recovery_pending: self.system_control.store_frozen
                || self.system_control.store_recovery.is_some(),
            store_recovery_operation: self
                .system_control
                .store_recovery
                .as_ref()
                .map(|ticket| ticket.operation.clone()),
            ..Default::default()
        };
        #[cfg(windows)]
        {
            view.retiring_owners = self.system_control.retired.len();
            if self
                .system_control
                .artwork_job
                .as_ref()
                .is_some_and(|job| job.cancel.load(Ordering::Acquire))
            {
                view.retiring_owners += 1;
            }
            if let Some(active) = &self.system_control.active {
                let state = active.owner.state();
                view.owner_epoch = Some(state.owner_epoch);
                view.configuration = state.configuration;
                view.native_ready =
                    state.ready && !active.fence.is_closing() && !active.fence.is_paused();
                view.owner_thread_exited = state.thread_exited;
                view.registered_hotkeys = state.registered_hotkeys;
                view.media_session_active = state.media_session_active;
                view.power_request_active = state.power_request_active;
                view.last_power_event = state.last_power_event;
                view.native_error = state.last_error.or(view.native_error);
                view.dispatcher_error = active.dispatcher.last_error();
                if active.fence.is_closing() {
                    view.retiring_owners += 1;
                }
            }
            if view.store_recovery_operation.is_none() && self.system_control.store_frozen {
                view.store_recovery_operation = self
                    .system_control
                    .pending
                    .as_ref()
                    .and_then(|pending| pending.operation.clone());
            }
        }
        view
    }
    pub(super) fn publish_system_control_state(&mut self) {
        let view = self.system_control_state();
        let Ok(signature) = serde_json::to_string(&view) else {
            return;
        };
        if self.system_control.last_view.as_ref() == Some(&signature) {
            return;
        }
        self.system_control.last_view = Some(signature);
        self.events.lock().unwrap().publish(
            self.revision,
            "system_controls",
            None,
            BackendEvent::SystemControlChanged {
                state: Box::new(view),
            },
        );
    }
    pub(super) fn start_system_controls_from_saved(&mut self) -> Result<()> {
        if !self.data.system_control.enabled {
            return Ok(());
        }
        #[cfg(windows)]
        {
            let requested = self.data.system_control.clone();
            if let Err(error) = self.start_native_system_holder(&requested) {
                self.system_control.native_error = Some(bounded_error(&error));
                return Err(error);
            }
            self.system_control.pending = Some(PendingConfiguration {
                operation: None,
                old: requested.clone(),
                requested,
                revision: 1,
                mode: PendingMode::Native,
                failure: None,
                durable_committed: true,
                next_attempt: Instant::now(),
            });
            Ok(())
        }
        #[cfg(not(windows))]
        anyhow::bail!(BackendError::new(
            ErrorKind::Unavailable,
            "Native system controls are not implemented on this platform"
        ))
    }
    pub(super) fn set_system_control_settings(
        &mut self,
        settings: SystemSettings,
    ) -> Result<ReceiptState> {
        settings
            .validate()
            .map_err(|error| BackendError::new(ErrorKind::InvalidData, error.to_string()))?;
        ensure!(
            !self.system_control.closing,
            BackendError::new(ErrorKind::Closed, "System controls are quiescing")
        );
        self.system_control.require_store_mutation()?;
        #[cfg(not(windows))]
        {
            ensure!(
                !settings.enabled,
                BackendError::new(
                    ErrorKind::Unavailable,
                    "Native system controls are not implemented on this platform"
                )
            );
            return self.commit_disabled_system_settings(settings);
        }
        #[cfg(windows)]
        {
            ensure!(
                self.system_control.pending.is_none(),
                BackendError::new(
                    ErrorKind::Busy,
                    "A system configuration or recovery acknowledgement is pending"
                )
            );
            ensure!(
                self.system_control.retired.is_empty(),
                BackendError::new(
                    ErrorKind::Busy,
                    "Previous native system owners are still retiring"
                )
            );
            let old = self.data.system_control.clone();
            if !settings.enabled && self.system_control.active.is_none() {
                return self.commit_disabled_system_settings(settings);
            }
            let revision = if let Some(active) = &self.system_control.active {
                active.owner.configure(settings.clone())?
            } else {
                self.start_native_system_holder(&settings)?;
                1
            };
            self.system_control.pending = Some(PendingConfiguration {
                operation: Some(self.request_id.clone()),
                old,
                requested: settings,
                revision,
                mode: PendingMode::Native,
                failure: None,
                durable_committed: false,
                next_attempt: Instant::now(),
            });
            self.async_started = true;
            self.publish_system_control_state();
            Ok(ReceiptState::Accepted)
        }
    }
    fn commit_system_control_settings(&mut self, settings: SystemSettings) -> Result<()> {
        let mut candidate = self.data.clone();
        candidate.system_control = settings;
        self.system_control.internal_store_write = true;
        let result = self.commit_data(candidate);
        self.system_control.internal_store_write = false;
        result
    }
    fn commit_disabled_system_settings(
        &mut self,
        settings: SystemSettings,
    ) -> Result<ReceiptState> {
        let old = self.data.system_control.clone();
        match self.commit_system_control_settings(settings.clone()) {
            Ok(()) => {
                self.publish_system_control_state();
                Ok(ReceiptState::Committed)
            }
            Err(error) => {
                let error = BackendError::new(ErrorKind::Storage, bounded_error(&error));
                self.system_control.set_store_frozen(true);
                self.system_control.persistence_error = Some(error.message.clone());
                self.system_control.store_recovery = Some(PendingStoreRecovery {
                    operation: self.request_id.clone(),
                    old,
                    requested: settings,
                    error,
                    next_attempt: Instant::now() + Duration::from_millis(250),
                });
                self.async_started = true;
                self.publish_system_control_state();
                Ok(ReceiptState::Accepted)
            }
        }
    }
    fn poll_system_store_recovery(&mut self) -> Result<bool> {
        let Some(mut ticket) = self.system_control.store_recovery.take() else {
            return Ok(true);
        };
        if Instant::now() < ticket.next_attempt && !self.system_control.closing {
            self.system_control.store_recovery = Some(ticket);
            return Ok(false);
        }
        if let Err(error) = self.repair_system_control_store() {
            self.system_control.persistence_error = Some(bounded_error(&error));
            ticket.next_attempt = Instant::now() + Duration::from_millis(250);
            self.system_control.store_recovery = Some(ticket);
            return Ok(false);
        }
        let result = if self.system_control.closing {
            Err(BackendError::new(
                ErrorKind::Cancelled,
                "System settings transaction cancelled by profile shutdown",
            )
            .into())
        } else if self.data.system_control == ticket.requested {
            Ok(())
        } else if self.data.system_control == ticket.old {
            Err(ticket.error.into())
        } else {
            self.system_control.set_store_frozen(true);
            self.system_control.store_recovery = Some(ticket);
            anyhow::bail!("System settings authority changed during durable recovery");
        };
        self.system_control.persistence_error = None;
        self.publish_finished(ticket.operation, &result);
        Ok(true)
    }
    fn repair_system_control_store(&mut self) -> Result<()> {
        self.system_control.internal_store_write = true;
        let result = self.commit_data(self.data.clone());
        self.system_control.internal_store_write = false;
        result?;
        let stored = self.store.load()?;
        ensure!(
            serde_json::to_vec(&stored)? == serde_json::to_vec(&self.data)?,
            "Durable system settings restoration differs from the current profile"
        );
        self.system_control.set_store_frozen(false);
        Ok(())
    }
    pub(super) fn retry_system_controls(&mut self) -> Result<ReceiptState> {
        ensure!(
            !self.system_control.closing,
            BackendError::new(ErrorKind::Closed, "System controls are quiescing")
        );
        if let Some(ticket) = &mut self.system_control.store_recovery {
            ticket.next_attempt = Instant::now();
            self.poll_system_controls();
            return Ok(ReceiptState::Applied);
        }
        #[cfg(windows)]
        if let Some(pending) = &mut self.system_control.pending {
            pending.next_attempt = Instant::now();
            self.poll_system_controls();
            return Ok(ReceiptState::Applied);
        }
        self.set_system_control_settings(self.data.system_control.clone())
    }
    pub(super) fn system_input(
        &mut self,
        input: SystemInput,
        fence: SystemInputFence,
        cancel: &AtomicBool,
    ) -> Result<(), BackendError> {
        self.check_system_input(input, &fence, cancel)?;
        let command = self.system_action_command(input.action)?;
        // Mapping collection/seek may inspect repository metadata. Recheck the
        // actual audio generation and revocable owned gate adjacent to effects.
        self.check_system_input(input, &fence, cancel)?;
        if let Some(command) = command {
            let envelope = CommandEnvelope {
                request_id: input.request_id(),
                origin: "system_controls".into(),
                expected_revision: None,
                command,
            };
            if envelope.request_id.len() > 128 || envelope.origin.len() > 128 {
                return Err(BackendError::new(
                    ErrorKind::InvalidData,
                    "System command identifiers exceed their bound",
                ));
            }
            self.execute(envelope)?;
        }
        if matches!(input.action, SystemAction::PowerEvent { .. }) {
            self.publish_system_control_state();
        }
        Ok(())
    }
    fn check_system_input(
        &self,
        input: SystemInput,
        fence: &SystemInputFence,
        cancel: &AtomicBool,
    ) -> Result<(), BackendError> {
        if cancel.load(Ordering::Acquire)
            || self.system_control.closing
            || self.system_control.bridge.shutdown.load(Ordering::Acquire)
        {
            return Err(BackendError::new(
                ErrorKind::Cancelled,
                "System input owner is stopping",
            ));
        }
        fence
            .validate(input)
            .map_err(|error| BackendError::new(ErrorKind::Cancelled, error.to_string()))?;
        #[cfg(windows)]
        if !self
            .system_control
            .active
            .as_ref()
            .is_some_and(|active| active.fence.same_owner(fence))
        {
            return Err(BackendError::new(
                ErrorKind::Cancelled,
                "System input native owner changed",
            ));
        }
        #[cfg(not(windows))]
        return Err(BackendError::new(
            ErrorKind::Unavailable,
            "Native system input is unavailable on this platform",
        ));
        #[cfg(windows)]
        {
            if input.action.requires_generation() {
                let audio = self.audio.snapshot();
                if input.generation != Some(audio.generation)
                    || !audio.has_presented_media
                    || !self
                        .current_record
                        .as_ref()
                        .is_some_and(|record| audio.track_id.as_deref() == Some(record.id.as_str()))
                {
                    return Err(BackendError::new(
                        ErrorKind::Cancelled,
                        "System input presentation generation changed",
                    ));
                }
            }
            Ok(())
        }
    }
    fn system_action_command(
        &self,
        action: SystemAction,
    ) -> Result<Option<BackendCommand>, BackendError> {
        use crate::open_api::ControlAction;
        let standard = match action {
            SystemAction::Play => Some(ControlAction::Play),
            SystemAction::Pause => Some(ControlAction::Pause),
            SystemAction::Toggle => Some(ControlAction::Toggle),
            SystemAction::Stop => Some(ControlAction::Stop),
            SystemAction::Previous => Some(ControlAction::Previous),
            SystemAction::Next => Some(ControlAction::Next),
            SystemAction::VolumeStep { steps } => {
                Some(ControlAction::VolumeRelative { percent: steps })
            }
            SystemAction::ToggleMute => Some(ControlAction::ToggleMute),
            SystemAction::Love => Some(ControlAction::CollectCurrent { collected: true }),
            SystemAction::Unlove => Some(ControlAction::CollectCurrent { collected: false }),
            SystemAction::Dislike => Some(ControlAction::DislikeCurrent),
            SystemAction::SeekAbsolute { position_ms } => {
                let audio = self.audio.snapshot();
                ensure_system_seek(&audio)?;
                Some(ControlAction::Seek {
                    seconds: (position_ms as f64 / 1000.0).min(audio.duration_seconds),
                })
            }
            SystemAction::SeekRelative { offset_ms } => {
                let audio = self.audio.snapshot();
                ensure_system_seek(&audio)?;
                Some(ControlAction::SeekRelative {
                    seconds: offset_ms as f64 / 1000.0,
                })
            }
            SystemAction::PlaybackRate { rate } => {
                return Ok(Some(BackendCommand::SetPlaybackRate { rate }));
            }
            SystemAction::PowerEvent {
                event: SystemPowerEvent::Suspend,
            } => matches!(
                self.audio.snapshot().status,
                PlaybackStatus::Playing | PlaybackStatus::Buffering | PlaybackStatus::Loading
            )
            .then_some(ControlAction::Pause),
            SystemAction::PowerEvent {
                event: SystemPowerEvent::ResumeAutomatic | SystemPowerEvent::ResumeUser,
            } => None,
        };
        standard
            .map(|action| self.control_action_command(action))
            .unwrap_or(Ok(None))
    }
    /// Reads the actual presentation. Queue navigation runs against clones and
    /// never changes the owner's pending plan, history or random sequence.
    pub(super) fn system_projection(&self) -> Result<SystemProjection> {
        let audio = self.audio.snapshot();
        let record = self.current_record.as_ref().filter(|record| {
            audio.generation > 0 && audio.track_id.as_deref() == Some(record.id.as_str())
        });
        let duration = if audio.duration_known && audio.duration_seconds.is_finite() {
            seconds_ms(audio.duration_seconds)
        } else {
            0
        };
        let position = if audio.position_seconds.is_finite() {
            seconds_ms(audio.position_seconds).min(duration)
        } else {
            0
        };
        let navigation_key = self.system_navigation_key();
        let (can_previous, can_next) = if let Some((key, previous, next)) =
            &self.system_control.navigation
            && key == &navigation_key
        {
            (*previous, *next)
        } else {
            let eligible = |entry: &QueueEntry| {
                Self::eligible_with(
                    &self.eligibility,
                    &self.dislike,
                    &self.failed_entries,
                    entry,
                )
            };
            (
                self.queue.clone().previous(eligible).is_some(),
                self.queue
                    .clone()
                    .preview_next(AdvanceReason::Manual, eligible)
                    .is_some(),
            )
        };
        let state = if record.is_none() {
            SystemPlaybackState::None
        } else {
            match audio.status {
                PlaybackStatus::Playing => SystemPlaybackState::Playing,
                PlaybackStatus::Paused => SystemPlaybackState::Paused,
                PlaybackStatus::Loading | PlaybackStatus::Buffering => {
                    SystemPlaybackState::Buffering
                }
                _ => SystemPlaybackState::Stopped,
            }
        };
        let value = SystemProjection {
            generation: audio.generation,
            media_id: record.map(|r| system_media_id(&r.id)),
            title: record.map(|r| system_text(&r.title)).unwrap_or_default(),
            artist: record
                .map(|r| system_text(&r.artists.join("、")))
                .unwrap_or_default(),
            album: record.map(|r| system_text(&r.album)).unwrap_or_default(),
            state,
            duration_ms: duration,
            position_ms: position,
            playback_rate: audio.rate,
            can_previous,
            can_next,
            can_seek: record.is_some()
                && audio.has_presented_media
                && audio.duration_known
                && duration > 0
                && !audio.seek_pending,
            artwork: None,
        };
        value.validate()?;
        Ok(value)
    }
    fn system_navigation_key(&self) -> String {
        let mut failed = self.failed_entries.iter().collect::<Vec<_>>();
        failed.sort_unstable();
        format!(
            "{}:{}:{}:{}:{}:{}:{failed:?}",
            self.queue.revision,
            self.revision,
            self.catalog.revision(),
            self.source_epoch,
            self.source_initializing,
            self.runtime.is_some()
        )
    }
}

#[cfg(windows)]
impl Runtime {
    fn start_native_system_holder(&mut self, settings: &SystemSettings) -> Result<()> {
        ensure!(settings.enabled, "Native system controls are disabled");
        ensure!(
            self.system_control.active.is_none() && self.system_control.retired.is_empty(),
            "Previous native system owners have not actually retired"
        );
        ensure!(
            self.system_control.retired.len() < MAX_RETIRED_SYSTEM_OWNERS,
            "System owner retention capacity exceeded"
        );
        let epoch = self
            .system_control
            .epoch
            .checked_add(1)
            .context("System owner epoch exhausted")?;
        let projection = self.system_projection()?;
        self.system_control.epoch = epoch;
        let dispatcher = SystemDispatcher::start(epoch, self.system_control.bridge.clone())
            .inspect_err(|error| {
                self.system_control.native_error = Some(bounded_error(error));
            })?;
        let sender = dispatcher.sender();
        let fence = sender.fence();
        let owner = match WindowsSystemOwner::start(settings.clone(), projection, sender) {
            Ok(owner) => owner,
            Err(error) => {
                self.system_control.native_error = Some(bounded_error(&error));
                dispatcher.begin_shutdown();
                self.system_control
                    .retired
                    .push(RetiredSystemHolder::Dispatcher {
                        dispatcher,
                        error: None,
                    });
                return Err(error);
            }
        };
        self.system_control.active = Some(SystemHolder {
            owner,
            dispatcher,
            fence,
            native_joined: false,
            dispatcher_joined: false,
            retirement_error: None,
        });
        self.system_control.last_projection = None;
        Ok(())
    }
    fn finish_system_configuration(&mut self, pending: PendingConfiguration, result: Result<()>) {
        if let Some(operation) = pending.operation {
            self.publish_finished(operation, &result);
        }
        self.publish_system_control_state();
    }
    fn retire_active_system_holder(&mut self) -> Result<()> {
        self.retire_system_artwork_lease()?;
        self.retire_active_artwork_job()?;
        ensure!(
            self.system_control.retired.len() < MAX_RETIRED_SYSTEM_OWNERS,
            "Native system retirement holder capacity exceeded"
        );
        if let Some(holder) = self.system_control.active.take() {
            holder.begin_shutdown();
            self.system_control
                .retired
                .push(RetiredSystemHolder::Native(Box::new(holder)));
        }
        self.system_control.last_projection = None;
        Ok(())
    }
    fn poll_retired_system_holders(&mut self) -> Result<bool> {
        let mut index = 0;
        let mut first_error = None;
        while index < self.system_control.retired.len() {
            match self.system_control.retired[index].join_retired() {
                Ok(true) => {
                    self.system_control.retired.swap_remove(index);
                }
                Ok(false) => index += 1,
                Err(error) => {
                    if first_error.is_none() {
                        first_error = Some(error);
                    }
                    index += 1;
                }
            }
        }
        if let Some(error) = first_error {
            return Err(error);
        }
        Ok(self.system_control.retired.is_empty())
    }
    fn poll_system_configuration(&mut self) {
        let Some(mut pending) = self.system_control.pending.take() else {
            return;
        };
        let result = self.step_system_configuration(&mut pending);
        match result {
            Ok(Some(outcome)) => self.finish_system_configuration(pending, outcome),
            Ok(None) => self.system_control.pending = Some(pending),
            Err(error) => {
                self.system_control.native_error = Some(bounded_error(&error));
                pending.next_attempt = Instant::now() + Duration::from_millis(250);
                self.system_control.pending = Some(pending);
            }
        }
    }
    fn step_system_configuration(
        &mut self,
        pending: &mut PendingConfiguration,
    ) -> Result<Option<Result<()>>> {
        if Instant::now() < pending.next_attempt {
            return Ok(None);
        }
        if pending.mode == PendingMode::RepairStore {
            self.repair_system_control_store()?;
            if self.data.system_control == pending.requested {
                pending.durable_committed = true;
                pending.mode = PendingMode::Native;
            } else {
                ensure!(
                    self.data.system_control == pending.old,
                    "System preferences changed during durable restoration"
                );
                let active = self
                    .system_control
                    .active
                    .as_mut()
                    .context("Native rollback owner is unavailable")?;
                let terminal = active
                    .owner
                    .state()
                    .configuration
                    .terminal
                    .context("Native rollback acknowledgement is unavailable")?;
                if recovery_from_native_ack(&terminal) == PendingMode::Rollback {
                    pending.revision = active.owner.rollback_configuration(pending.revision)?;
                    pending.mode = PendingMode::Rollback;
                } else if terminal.requested.enabled {
                    active.begin_shutdown();
                    pending.mode = PendingMode::RetireNewOwner;
                } else {
                    ensure!(
                        active.join_retired()?,
                        "Disabled native owner is still retiring"
                    );
                    active.owner.commit_configuration(pending.revision)?;
                    self.retire_active_system_holder()?;
                    pending.mode = PendingMode::RestartOld;
                }
            }
        }
        if pending.mode == PendingMode::RetireNewOwner {
            let active = self
                .system_control
                .active
                .as_mut()
                .context("Uncommitted system owner is unavailable")?;
            if !active.join_retired()? {
                return Ok(None);
            }
            // This owner had no prior native configuration. Its actual join
            // proves disposal, without claiming custom disabled preferences
            // were ever installed in a native session.
            self.retire_active_system_holder()?;
            pending.mode = PendingMode::RestartOld;
        }
        if pending.mode == PendingMode::RestartOld {
            if !self.poll_retired_system_holders()? {
                return Ok(None);
            }
            if !pending.old.enabled {
                return Ok(Some(Err(pending
                    .failure
                    .clone()
                    .unwrap_or_else(|| {
                        BackendError::new(
                            ErrorKind::OperationFailed,
                            "System configuration was rolled back",
                        )
                    })
                    .into())));
            }
            self.start_native_system_holder(&pending.old)?;
            pending.revision = 1;
            pending.mode = PendingMode::Rollback;
        }
        let active = self
            .system_control
            .active
            .as_mut()
            .context("Native system owner is unavailable")?;
        let state = active.owner.state();
        let Some(ack) = state
            .configuration
            .terminal
            .filter(|ack| ack.revision == pending.revision)
        else {
            return Ok(None);
        };
        match ack.outcome {
            SystemConfigurationOutcome::Rejected | SystemConfigurationOutcome::Cancelled => {
                let error = BackendError::new(
                    ErrorKind::OperationFailed,
                    ack.error
                        .clone()
                        .unwrap_or_else(|| "Native system configuration failed".into()),
                );
                self.system_control.native_error = Some(error.message.clone());
                if !ack.applied.as_ref().is_some_and(|s| s.enabled) && !active.join_retired()? {
                    return Ok(None);
                }
                active.owner.commit_configuration(pending.revision)?;
                if !ack.applied.as_ref().is_some_and(|s| s.enabled) {
                    self.retire_active_system_holder()?;
                    if pending.mode == PendingMode::Rollback && pending.old.enabled {
                        pending.mode = PendingMode::RestartOld;
                        pending.next_attempt = Instant::now() + Duration::from_secs(1);
                        return Ok(None);
                    }
                }
                Ok(Some(Err(pending.failure.clone().unwrap_or(error).into())))
            }
            SystemConfigurationOutcome::Applied => {
                let target = if pending.mode == PendingMode::Rollback {
                    &pending.old
                } else {
                    &pending.requested
                };
                ensure!(
                    &ack.requested == target && ack.applied.as_ref() == Some(target),
                    "Native system configuration acknowledgement changed"
                );
                if !target.enabled && !active.join_retired()? {
                    return Ok(None);
                }
                if !target.enabled {
                    self.retire_system_artwork_lease()?;
                    self.retire_active_artwork_job()?;
                    if !self.poll_retired_system_holders()? {
                        return Ok(None);
                    }
                }
                if pending.mode == PendingMode::Native && !pending.durable_committed {
                    if let Err(error) = self.commit_system_control_settings(target.clone()) {
                        let error = BackendError::new(ErrorKind::Storage, bounded_error(&error));
                        self.system_control.persistence_error = Some(error.message.clone());
                        self.system_control.set_store_frozen(true);
                        pending.failure = Some(error);
                        pending.mode = PendingMode::RepairStore;
                        pending.next_attempt = Instant::now() + Duration::from_millis(250);
                        return Ok(None);
                    }
                    pending.durable_committed = true;
                }
                self.system_control
                    .active
                    .as_mut()
                    .context("Native commit owner is unavailable")?
                    .owner
                    .commit_configuration(pending.revision)?;
                if !target.enabled {
                    self.retire_active_system_holder()?;
                }
                self.system_control.set_store_frozen(false);
                self.system_control.persistence_error = None;
                self.system_control.native_error = None;
                if pending.mode == PendingMode::Rollback {
                    Ok(Some(Err(pending
                        .failure
                        .clone()
                        .unwrap_or_else(|| {
                            BackendError::new(
                                ErrorKind::Storage,
                                "System settings transaction was rolled back",
                            )
                        })
                        .into())))
                } else {
                    Ok(Some(Ok(())))
                }
            }
        }
    }
    fn retire_active_artwork_job(&mut self) -> Result<()> {
        if let Some(job) = self.system_control.artwork_job.as_ref() {
            job.cancel.store(true, Ordering::Release);
            ensure!(
                self.system_control.retired.len() < MAX_RETIRED_SYSTEM_OWNERS,
                "Physical artwork worker retirement capacity reached"
            );
        }
        if let Some(job) = self.system_control.artwork_job.take() {
            self.system_control
                .retired
                .push(RetiredSystemHolder::Artwork(Box::new(job)));
        }
        Ok(())
    }
    fn refresh_system_artwork(&mut self, projection: &mut SystemProjection) -> Result<()> {
        self.system_control
            .retired_artwork
            .retain(|lease| !lease.is_drained());
        let context = ArtworkContext {
            generation: projection.generation,
            track: projection.media_id.clone(),
            source_epoch: self.source_epoch,
            catalog_revision: self.catalog.revision(),
            repository_revision: self.revision,
            pictures: self.open_api_picture_revision(),
        };
        let checked = self
            .system_control
            .artwork_checked
            .as_ref()
            .is_some_and(|(at, key)| key == &context && at.elapsed() < Duration::from_millis(100));
        if !checked
            || self
                .system_control
                .artwork
                .as_ref()
                .is_some_and(|lease| !lease.is_current())
            || self
                .system_control
                .artwork_job
                .as_ref()
                .is_some_and(|job| !job.accepted)
        {
            // This obtains only verified RAM and current SQL/consent fingerprints.
            // Every physical FH/namespace check runs on the retained worker.
            let candidate = self.current_system_artwork(projection.generation)?;
            let unchanged = match (&self.system_control.artwork, &candidate) {
                (None, None) => true,
                (Some(lease), Some(candidate)) => {
                    lease.is_current()
                        && lease.binding() == &candidate.binding
                        && SystemArtwork::from_source_lease(lease.clone())
                            .is_ok_and(|a| a.sha256() == candidate.sha256)
                }
                _ => false,
            };
            if !unchanged {
                self.retire_system_artwork_lease()?;
            }
            if self.system_control.artwork_job.as_ref().is_some_and(|job| {
                job.cancel.load(Ordering::Acquire)
                    || candidate
                        .as_ref()
                        .is_none_or(|current| !current.same_authority(&job.candidate))
            }) {
                self.retire_active_artwork_job()?;
            }
            if !unchanged && let Some(candidate) = candidate {
                ensure!(
                    self.system_control.retired_artwork.len() < MAX_RETIRED_SYSTEM_OWNERS,
                    "Native artwork retirement still owns writer pins"
                );
                if candidate.physical_asset.is_none() {
                    self.system_control.artwork = Some(SourceArtworkLease::from_verified_ram(
                        candidate.bytes,
                        candidate.sha256,
                        candidate.binding,
                    )?);
                } else {
                    if self.system_control.artwork_job.is_none() {
                        ensure!(
                            self.system_control.retired.len()
                                + usize::from(self.system_control.active.is_some())
                                < MAX_RETIRED_SYSTEM_OWNERS,
                            "Physical artwork validation capacity reached"
                        );
                        self.system_control.artwork_job = Some(ArtworkGrantJob::start(
                            candidate.clone(),
                            context.clone(),
                            self.repository.clone(),
                            self.library.authority.clone(),
                        )?);
                    }
                    let mut job = self
                        .system_control
                        .artwork_job
                        .take()
                        .expect("retained physical artwork validation");
                    let outcome = match job.receiver.try_recv() {
                        Ok(Ok(owner)) => (|| -> Result<()> {
                            ensure!(
                                owner.is_current()
                                    && !job.cancel.load(Ordering::Acquire)
                                    && job.context.generation == context.generation
                                    && job.context.track == context.track
                                    && job.context.pictures.0 == context.pictures.0
                                    && candidate.same_authority(&job.candidate),
                                "Physical artwork proof changed before RAM publication"
                            );
                            let lease = SourceArtworkLease::from_verified_ram(
                                candidate.bytes,
                                candidate.sha256,
                                candidate.binding,
                            )?
                            .retain_physical_owner(owner)?;
                            self.system_control.artwork = Some(lease);
                            job.accepted = true;
                            job.context = context.clone();
                            Ok(())
                        })(),
                        Ok(Err(error)) => {
                            job.cancel.store(true, Ordering::Release);
                            Err(error)
                        }
                        Err(mpsc::TryRecvError::Empty) => Ok(()),
                        Err(mpsc::TryRecvError::Disconnected) => Err(anyhow::anyhow!(
                            "Physical artwork validation ended before acknowledgement"
                        )),
                    };
                    self.system_control.artwork_job = Some(job);
                    outcome?;
                }
            }
            self.system_control.artwork_checked = Some((Instant::now(), context));
        }
        projection.artwork = self
            .system_control
            .artwork
            .clone()
            .map(SystemArtwork::from_source_lease)
            .transpose()?;
        Ok(())
    }
    fn current_system_artwork(&self, generation: u64) -> Result<Option<CurrentSystemArtwork>> {
        use crate::domain::artwork::{ArtworkAuthority, ArtworkLookup};
        let audio = self.audio.snapshot();
        let Some(record) = self.current_record.as_ref().filter(|record| {
            generation > 0
                && audio.generation == generation
                && audio.track_id.as_deref() == Some(record.id.as_str())
        }) else {
            return Ok(None);
        };
        let Some(entry) = self
            .queue
            .current
            .as_ref()
            .filter(|entry| entry.track_id == record.id)
        else {
            return Ok(None);
        };
        let ArtworkLookup::Hit(info) = self.picture_info(&entry.list_id, &record.id)? else {
            return Ok(None);
        };
        info.validate()?;
        ensure!(
            info.expire_at_ms > crate::pictures::service::now_ms(),
            "Native system artwork expired"
        );
        let source_epoch = if let ArtworkAuthority::Source {
            source_sha256,
            policy_version,
            ..
        } = &info.key.authority
        {
            let source = self
                .data
                .sources
                .iter()
                .find(|source| &source.sha256 == source_sha256)
                .context("System artwork source archive is unavailable")?;
            source.require_approval()?;
            ensure!(
                self.data.selected.as_ref() == Some(source_sha256)
                    && *policy_version == u64::from(source.approval_policy_version)
                    && self.runtime.is_some()
                    && !self.source_initializing
                    && self.source_epoch > 0,
                "System artwork source lifecycle permission changed"
            );
            Some(self.source_epoch)
        } else {
            None
        };
        let physical_asset = match &info.key.authority {
            ArtworkAuthority::LocalAsset { asset_id, .. } => Some(asset_id.clone()),
            ArtworkAuthority::Source {
                local_permission_sha256: Some(_),
                ..
            } => Some(if let Some(asset_id) = &record.local_asset_id {
                asset_id.clone()
            } else {
                self.repository
                    .local_asset_for_track(&record.id)?
                    .context("System artwork local asset permission disappeared")?
                    .id
            }),
            _ => record.local_asset_id.clone(),
        };
        let Some(verified) = self.current_open_api_artwork()? else {
            return Ok(None);
        };
        let Some(blob) = &info.blob else {
            return Ok(None);
        };
        ensure!(
            verified.sha256 == blob.sha256 && verified.bytes.len() as u64 == blob.encoded_bytes,
            "System artwork verified RAM differs from current metadata"
        );
        let scope = serde_json::to_vec(&(
            info.key.fingerprint()?,
            self.open_api_picture_revision().0,
            source_epoch,
            info.resolved_at_ms,
            info.expire_at_ms,
        ))?;
        let key_sha256 = format!("{:x}", Sha256::digest(scope));
        ensure!(
            key_sha256 == verified.key_sha256,
            "System artwork scope differs from its verified RAM projection"
        );
        Ok(Some(CurrentSystemArtwork {
            binding: SourceArtworkBinding {
                generation: Some(generation),
                source_epoch,
                key_sha256,
                expires_at_ms: info.expire_at_ms,
            },
            sha256: verified.sha256,
            bytes: verified.bytes,
            physical_asset,
        }))
    }
    fn retire_system_artwork_lease(&mut self) -> Result<()> {
        self.system_control
            .retired_artwork
            .retain(|lease| !lease.is_drained());
        if let Some(lease) = self.system_control.artwork.as_ref() {
            lease.retire();
            if !lease.is_drained() {
                ensure!(
                    self.system_control.retired_artwork.len() < MAX_RETIRED_SYSTEM_OWNERS,
                    "Native system artwork holder capacity reached"
                );
            }
        }
        if let Some(lease) = self.system_control.artwork.take()
            && !lease.is_drained()
        {
            self.system_control.retired_artwork.push(lease);
        }
        Ok(())
    }
    pub(super) fn invalidate_system_artwork(&mut self) -> Result<()> {
        self.retire_system_artwork_lease()?;
        self.retire_active_artwork_job()?;
        self.system_control.artwork_checked = None;
        self.system_control.last_projection = None;
        if let Some(active) = &self.system_control.active
            && !active.fence.is_closing()
        {
            active.owner.publish(self.system_projection()?)?;
        }
        Ok(())
    }
}
impl Runtime {
    pub(super) fn poll_system_controls(&mut self) {
        if self.system_control.closing {
            return;
        }
        if let Err(error) = self.poll_system_store_recovery() {
            self.system_control.persistence_error = Some(bounded_error(&error));
        }
        #[cfg(windows)]
        {
            let result = (|| -> Result<()> {
                self.poll_retired_system_holders()?;
                self.poll_system_configuration();
                if self
                    .system_control
                    .active
                    .as_ref()
                    .is_some_and(|holder| !holder.fence.is_closing())
                {
                    let mut projection = self.system_projection()?;
                    self.system_control.navigation = Some((
                        self.system_navigation_key(),
                        projection.can_previous,
                        projection.can_next,
                    ));
                    if let Err(error) = self.refresh_system_artwork(&mut projection) {
                        self.system_control.native_error = Some(bounded_error(&error));
                        self.retire_system_artwork_lease()?;
                        projection.artwork = None;
                    }
                    let signature = serde_json::to_string(&(
                        projection.generation,
                        &projection.media_id,
                        &projection.title,
                        &projection.artist,
                        &projection.album,
                        projection.state,
                        projection.duration_ms,
                        projection.position_ms,
                        projection.playback_rate,
                        projection.can_previous,
                        projection.can_next,
                        projection.can_seek,
                        projection
                            .artwork
                            .as_ref()
                            .map(|a| (&a.binding().key_sha256, a.sha256())),
                    ))?;
                    if self.system_control.last_projection.as_ref() != Some(&signature) {
                        self.system_control
                            .active
                            .as_ref()
                            .context("Native projection owner changed")?
                            .owner
                            .publish(projection)?;
                        self.system_control.last_projection = Some(signature);
                    }
                }
                Ok(())
            })();
            if let Err(error) = result {
                self.system_control.native_error = Some(bounded_error(&error));
            }
        }
        self.publish_system_control_state();
    }
    pub(super) fn begin_system_control_shutdown(&mut self) -> Result<()> {
        self.system_control.closing = true;
        #[cfg(windows)]
        {
            // Close command admission before any source/profile mutation.
            if let Some(active) = &self.system_control.active {
                active.begin_shutdown();
            }
            for holder in &self.system_control.retired {
                holder.begin_shutdown();
            }
            self.retire_system_artwork_lease()?;
            if let Some(job) = &self.system_control.artwork_job {
                job.cancel.store(true, Ordering::Release);
            }
        }
        Ok(())
    }
    pub(super) fn poll_system_control_quiesce_step(&mut self) -> Result<bool> {
        ensure!(
            self.system_control.closing,
            "System handover requires closed admission"
        );
        if !self.poll_system_store_recovery()? {
            return Ok(false);
        }
        if self.system_control.store_frozen {
            self.repair_system_control_store()?;
        }
        #[cfg(windows)]
        {
            let native_joined = match self.system_control.active.as_mut() {
                Some(active) => active.join_retired()?,
                None => true,
            };
            // Joined native holders can release their metadata mailboxes before
            // physical artwork workers wait for the last retained lease.
            if native_joined {
                self.system_control.active = None;
            }
            self.system_control
                .retired_artwork
                .retain(|lease| !lease.is_drained());
            if self
                .system_control
                .artwork
                .as_ref()
                .is_some_and(SourceArtworkLease::is_drained)
            {
                self.system_control.artwork = None;
            }
            let retired = self.poll_retired_system_holders();
            let mut joined = native_joined && retired.as_ref().is_ok_and(|value| *value);
            if let Some(job) = self.system_control.artwork_job.as_mut() {
                if job.join_retired()? {
                    self.system_control.artwork_job = None;
                } else {
                    joined = false;
                }
            }
            retired?;
            joined &= self
                .system_control
                .artwork
                .as_ref()
                .is_none_or(SourceArtworkLease::is_drained)
                && self.system_control.retired_artwork.is_empty();
            if !joined {
                return Ok(false);
            }
            self.system_control.active = None;
            self.system_control.artwork = None;
            if let Some(pending) = self.system_control.pending.take() {
                self.finish_system_configuration(
                    pending,
                    Err(BackendError::new(
                        ErrorKind::Cancelled,
                        "System configuration cancelled by profile shutdown",
                    )
                    .into()),
                );
            }
        }
        self.publish_system_control_state();
        Ok(true)
    }
    #[cfg(not(windows))]
    pub(super) fn invalidate_system_artwork(&mut self) -> Result<()> {
        Ok(())
    }
}
fn bounded_error(error: &dyn std::fmt::Display) -> String {
    format!("{error}").chars().take(1024).collect()
}
fn system_text(value: &str) -> String {
    let mut result = String::with_capacity(value.len().min(8 * 1024));
    for ch in value.chars().filter(|ch| *ch != '\0') {
        if result.len() + ch.len_utf8() > 8 * 1024 {
            break;
        }
        result.push(ch);
    }
    result
}
fn system_media_id(value: &str) -> String {
    if !value.is_empty() && value.len() <= 1024 && !value.contains('\0') {
        value.to_owned()
    } else {
        format!("track:{:x}", Sha256::digest(value.as_bytes()))
    }
}
fn seconds_ms(seconds: f64) -> u64 {
    if !seconds.is_finite() || seconds <= 0.0 {
        0
    } else {
        (seconds * 1000.0)
            .round()
            .min((i64::MAX as u64 / 10_000) as f64) as u64
    }
}
fn ensure_system_seek(audio: &crate::audio::PlaybackSnapshot) -> Result<(), BackendError> {
    if !audio.has_presented_media
        || !audio.duration_known
        || !audio.duration_seconds.is_finite()
        || audio.duration_seconds <= 0.0
        || audio.seek_pending
    {
        Err(BackendError::new(
            ErrorKind::Unavailable,
            "Actual playback timeline is not seekable",
        ))
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(windows)]
    #[test]
    fn actual_proof_worker_join_waits_for_real_lease_pins_and_the_queued_projection() {
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let cancel = Arc::new(AtomicBool::new(false));
        let cancellation = cancel.clone();
        let mut worker = ManagedWorker::spawn("lx-test-artwork-proof-lifetime", move || {
            let owner = Arc::new(PhysicalArtworkOwner {
                cancel: cancellation,
                live: AtomicBool::new(true),
            });
            let live = PhysicalArtworkLive(owner.clone());
            ready_tx.send(owner.clone()).unwrap();
            while Arc::strong_count(&owner) > 2 {
                thread::sleep(Duration::from_millis(1));
            }
            drop(live);
        })
        .unwrap();
        let owner = ready_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        let bytes: Arc<[u8]> = Arc::from(b"verified image ownership fixture".as_slice());
        let sha256 = format!("{:x}", Sha256::digest(&bytes));
        let lease = SourceArtworkLease::from_verified_ram(
            bytes,
            sha256.clone(),
            SourceArtworkBinding {
                generation: Some(2),
                source_epoch: None,
                key_sha256: sha256,
                expires_at_ms: u64::MAX,
            },
        )
        .unwrap()
        .retain_physical_owner(owner)
        .unwrap();
        let pin = lease.pin().unwrap();
        let mailbox = Mutex::new(Some(SystemProjection {
            artwork: Some(SystemArtwork::from_source_lease(lease.clone()).unwrap()),
            ..Default::default()
        }));
        lease.retire();
        assert!(cancel.load(Ordering::Acquire));
        drop(lease);
        assert!(!worker.is_finished());
        drop(pin);
        assert!(!worker.is_finished());
        mailbox.lock().unwrap().take();
        let deadline = Instant::now() + Duration::from_secs(2);
        while !worker.is_finished() {
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(1));
        }
        worker.join_finished().unwrap();
    }
    #[cfg(windows)]
    #[test]
    fn a_blocked_artwork_proof_worker_remains_owned_without_blocking_actor_observation() {
        let (release_tx, release_rx) = mpsc::sync_channel(1);
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let cancel = Arc::new(AtomicBool::new(false));
        let worker = ManagedWorker::spawn("lx-test-blocked-artwork-proof", move || {
            release_rx.recv().unwrap();
            let _ = ready_tx.send(Err(anyhow::anyhow!("Injected cancelled physical proof")));
        })
        .unwrap();
        let mut job = ArtworkGrantJob {
            candidate: CurrentSystemArtwork {
                binding: SourceArtworkBinding {
                    generation: Some(2),
                    source_epoch: None,
                    key_sha256: "a".repeat(64),
                    expires_at_ms: u64::MAX,
                },
                sha256: "b".repeat(64),
                bytes: Arc::from(b"verified RAM".as_slice()),
                physical_asset: Some("owned-fixture".into()),
            },
            context: ArtworkContext {
                generation: 2,
                track: Some("fixture-track".into()),
                source_epoch: 1,
                catalog_revision: 1,
                repository_revision: 1,
                pictures: (1, 1),
            },
            cancel: cancel.clone(),
            receiver: ready_rx,
            worker: Some(worker),
            accepted: false,
            retirement_error: None,
        };
        assert!(matches!(
            job.receiver.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));
        assert!(!job.join_retired().unwrap());
        assert!(cancel.load(Ordering::Acquire));
        assert!(job.worker.is_some());
        release_tx.send(()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while !job.join_retired().unwrap() {
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(1));
        }
        assert!(job.worker.is_none());
    }
    #[cfg(windows)]
    #[test]
    fn an_uncommitted_enable_retires_without_applying_a_default_over_custom_disabled_preferences() {
        let old = SystemSettings {
            media_session_enabled: false,
            power_blocker_enabled: false,
            hotkeys: Vec::new(),
            ..Default::default()
        };
        assert!(!old.enabled);
        assert_ne!(old, SystemSettings::default());
        let requested = SystemSettings {
            enabled: true,
            ..old.clone()
        };
        let ack = SystemConfigurationAck {
            revision: 1,
            requested: requested.clone(),
            previous: None,
            applied: Some(requested),
            outcome: SystemConfigurationOutcome::Applied,
            error: None,
        };
        assert!(recovery_from_native_ack(&ack) == PendingMode::RetireNewOwner);
        assert!(ack.previous.is_none());
        assert_ne!(ack.previous.unwrap_or_default(), old);
        // A durable failure keeps the caller's complete prior settings. There
        // is no equality comparison with a fabricated native Default result.
        let pending = PendingConfiguration {
            operation: Some("enable-failed-store".into()),
            old: old.clone(),
            requested: ack.requested,
            revision: 1,
            mode: PendingMode::RetireNewOwner,
            failure: Some(BackendError::new(
                ErrorKind::Storage,
                "Injected JSON commit failure",
            )),
            durable_committed: false,
            next_attempt: Instant::now(),
        };
        assert_eq!(pending.old, old);
        assert!(!pending.old.enabled);
        assert!(pending.failure.is_some());
    }
    #[cfg(windows)]
    #[test]
    fn same_owner_rollback_is_selected_only_for_an_acknowledged_prior_native_configuration() {
        let old = SystemSettings {
            enabled: true,
            ..Default::default()
        };
        let requested = SystemSettings {
            global_hotkeys_enabled: true,
            ..old.clone()
        };
        let ack = SystemConfigurationAck {
            revision: 2,
            requested: requested.clone(),
            previous: Some(old),
            applied: Some(requested),
            outcome: SystemConfigurationOutcome::Applied,
            error: None,
        };
        assert!(recovery_from_native_ack(&ack) == PendingMode::Rollback);
    }
    #[test]
    fn closing_a_dispatcher_cancels_the_actual_request_already_queued_to_the_actor() {
        let (commands, actor) = mpsc::sync_channel(2);
        let bridge = Arc::new(BackendBridge {
            commands,
            shutdown: Arc::new(AtomicBool::new(false)),
        });
        let mut dispatcher = SystemDispatcher::start(31, bridge).unwrap();
        dispatcher
            .sender()
            .try_submit(SystemAction::SeekAbsolute { position_ms: 1000 }, Some(7))
            .unwrap();
        let message = actor.recv_timeout(Duration::from_secs(2)).unwrap();
        let RuntimeMessage::SystemInput {
            input,
            fence,
            cancel,
            reply,
        } = message
        else {
            panic!("wrong actual bridge message")
        };
        assert_eq!(input.generation, Some(7));
        fence.validate(input).unwrap();
        assert!(!cancel.load(Ordering::Acquire));
        dispatcher.begin_shutdown();
        assert!(dispatcher.shutdown(Duration::from_secs(2)).unwrap());
        assert!(cancel.load(Ordering::Acquire));
        assert!(fence.validate(input).is_err());
        drop(reply);
    }
    #[test]
    fn native_seek_availability_uses_presented_duration_and_pending_seek_state() {
        let mut snapshot = crate::audio::PlaybackSnapshot {
            duration_seconds: 30.0,
            duration_known: true,
            ..Default::default()
        };
        assert!(ensure_system_seek(&snapshot).is_err());
        snapshot.has_presented_media = true;
        ensure_system_seek(&snapshot).unwrap();
        snapshot.seek_pending = true;
        assert!(ensure_system_seek(&snapshot).is_err());
        snapshot.seek_pending = false;
        snapshot.duration_seconds = f64::INFINITY;
        assert!(ensure_system_seek(&snapshot).is_err());
        assert_eq!(seconds_ms(f64::NAN), 0);
        assert_eq!(seconds_ms(-1.0), 0);
        assert_eq!(seconds_ms(1.001), 1001);
        assert!(seconds_ms(f64::MAX) <= i64::MAX as u64 / 10_000);
    }
}

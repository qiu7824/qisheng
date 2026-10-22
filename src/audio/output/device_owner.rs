//! One reserved owner performs all device I/O and retains callback ownership queues.
//!
//! CPAL's WASAPI implementation can ignore its build timeout and its private stream
//! thread is joined only when Terminate is sent successfully. Our deadline covers
//! the registered wrapper owner and PCM callback handback, not an unsupported
//! promise that every CPAL private thread has joined.

use super::*;
use crate::audio::worker_owner::{TrackedWorker, WorkerKind, WorkerPermit};
use crate::audio::{
    effect_pin::{EffectPinTicket, PinScope, PinStatus},
    effects_factory::{BankRequest, CountedHandoffReservation},
};
use crate::domain::audio_output::{
    AudioDeviceInfo, AudioOutputError, AudioOutputFormat, AudioOutputMode, AudioOutputState,
    AudioOutputStatus, MAX_AUDIO_DEVICE_NAME_BYTES, MAX_AUDIO_DEVICES,
};
use cpal::SampleFormat;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::time::Instant;

const OWNER_POLL: Duration = Duration::from_millis(2);
const TOPOLOGY_POLL: Duration = Duration::from_secs(1);

#[derive(Clone)]
struct Request {
    preferences: AudioPreferences,
    revision: u64,
    user_selection_revision: u64,
}

struct OwnerControl {
    request: Mutex<Request>,
    snapshot: Mutex<AudioOutputSnapshot>,
    shared: Arc<Shared>,
}

pub(super) struct DeviceHandle {
    control: Arc<OwnerControl>,
    worker: TrackedWorker,
}

impl DeviceHandle {
    pub(super) fn wake(&self) {
        self.worker.unpark();
    }
    pub(super) fn snapshot(&self) -> AudioOutputSnapshot {
        self.control
            .snapshot
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }

    pub(super) fn select(&self, selection: OutputSelection) -> Result<()> {
        let mut request = self
            .control
            .request
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if self.control.shared.shutdown.load(Ordering::Acquire) {
            bail!("audio output has closed");
        }
        if request.preferences.selection == selection {
            return Ok(());
        }
        request.revision = request
            .revision
            .checked_add(1)
            .context("device switch sequence exhausted")?;
        request.user_selection_revision = request.revision;
        request.preferences.selection = selection.clone();
        let mut snapshot = self
            .control
            .snapshot
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        snapshot.preferences.selection = selection.clone();
        snapshot.state.desired = selection;
        drop(snapshot);
        drop(request);
        self.worker.unpark();
        Ok(())
    }

    pub(super) fn refresh(&self) -> Result<()> {
        let mut request = self
            .control
            .request
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if self.control.shared.shutdown.load(Ordering::Acquire) {
            bail!("audio output has closed");
        }
        request.revision = request
            .revision
            .checked_add(1)
            .context("device switch sequence exhausted")?;
        drop(request);
        self.worker.unpark();
        Ok(())
    }

    pub(super) fn set_mute(&self, muted: bool) {
        self.control
            .request
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .preferences
            .muted = muted;
        self.control
            .snapshot
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .preferences
            .muted = muted;
    }

    pub(super) fn set_pause_policy(&self, pause: bool) {
        self.control
            .request
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .preferences
            .pause_on_device_change = pause;
        self.control
            .snapshot
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .preferences
            .pause_on_device_change = pause;
    }

    pub(super) fn shutdown(&self, timeout: Duration) -> bool {
        self.control.shared.shutdown.store(true, Ordering::Release);
        self.worker.unpark();
        self.worker.wait(timeout)
    }
}

impl Drop for DeviceHandle {
    fn drop(&mut self) {
        self.control.shared.shutdown.store(true, Ordering::Release);
        self.worker.unpark();
        // TrackedWorker remains registered until a real join, including blocked I/O.
    }
}

fn output_format(format: PcmFormat) -> AudioOutputFormat {
    AudioOutputFormat {
        sample_rate: format.sample_rate,
        channels: format.channels,
    }
}

pub(super) fn memory_snapshot(format: PcmFormat) -> AudioOutputSnapshot {
    AudioOutputSnapshot {
        devices: Vec::new(),
        preferences: AudioPreferences::default(),
        state: AudioOutputState {
            desired: OutputSelection::Default,
            active_device_id: None,
            active_device_name: None,
            format: Some(output_format(format)),
            status: AudioOutputStatus::NotApplicable,
            switch_sequence: 0,
            fallback: false,
            last_error: None,
            mode: AudioOutputMode::Memory,
        },
    }
}

pub(super) fn start(
    shared: Arc<Shared>,
    state: Box<RenderState>,
    retired: Consumer<Box<ConvertedSource>>,
    effect_reclaimer: EffectsReclaimer,
    preferences: AudioPreferences,
) -> Result<DeviceHandle> {
    spawn_owner(
        shared,
        state,
        retired,
        effect_reclaimer,
        preferences,
        CpalBackend::new,
    )
}

fn spawn_owner<B: DeviceBackend + 'static>(
    shared: Arc<Shared>,
    state: Box<RenderState>,
    retired: Consumer<Box<ConvertedSource>>,
    effect_reclaimer: EffectsReclaimer,
    preferences: AudioPreferences,
    backend: impl FnOnce() -> B + Send + 'static,
) -> Result<DeviceHandle> {
    let permit = WorkerPermit::reserve(WorkerKind::DeviceOwner)?;
    let control = Arc::new(OwnerControl {
        request: Mutex::new(Request {
            preferences: preferences.clone(),
            revision: 1,
            user_selection_revision: 0,
        }),
        snapshot: Mutex::new(AudioOutputSnapshot {
            devices: Vec::new(),
            state: AudioOutputState {
                desired: preferences.selection.clone(),
                active_device_id: None,
                active_device_name: None,
                format: None,
                status: AudioOutputStatus::Initializing,
                switch_sequence: 0,
                fallback: false,
                last_error: None,
                mode: AudioOutputMode::Device,
            },
            preferences,
        }),
        shared,
    });
    let owner_control = control.clone();
    let handle = thread::Builder::new()
        .name("lx-device-owner".into())
        .spawn(move || {
            run_owner(backend(), owner_control, state, retired, effect_reclaimer);
        })
        .context("cannot start audio device owner")?;
    Ok(DeviceHandle {
        control,
        worker: permit.attach(handle),
    })
}

struct Inventory {
    devices: Vec<AudioDeviceInfo>,
    default_id: Option<String>,
}

struct Opened<S> {
    stream: S,
    format: PcmFormat,
}

trait DeviceBackend {
    type Stream;
    fn inventory(&mut self) -> std::result::Result<Inventory, AudioOutputError>;
    fn lookup(&mut self, _id: &str) -> Option<AudioDeviceInfo> {
        None
    }
    fn open(
        &mut self,
        id: &str,
        kernel: RenderKernel,
        shared: Arc<Shared>,
        generation: u64,
    ) -> std::result::Result<Opened<Self::Stream>, AudioOutputError>;
}

struct Session<S> {
    stream: Option<S>,
    format: PcmFormat,
    device: AudioDeviceInfo,
    generation: u64,
    handoff: Producer<Box<RenderState>>,
    returned: Consumer<Box<RenderState>>,
    signals: Arc<KernelSignals>,
}

impl<S> Session<S> {
    fn candidate(device: AudioDeviceInfo, generation: u64) -> (Self, RenderKernel) {
        let (handoff, incoming) = RingBuffer::new(1);
        let (returning, returned) = RingBuffer::new(1);
        let signals = Arc::new(KernelSignals {
            acknowledged: AtomicBool::new(false),
            closed: AtomicBool::new(false),
            errors: AtomicU8::new(0),
            handback_serial: AtomicU64::new(0),
            handback_ack: AtomicU64::new(0),
        });
        let kernel = RenderKernel {
            state: None,
            handoff: Some(incoming),
            returned: returning,
            signals: Some(signals.clone()),
        };
        (
            Self {
                stream: None,
                format: MEMORY_FORMAT,
                device,
                generation,
                handoff,
                returned,
                signals,
            },
            kernel,
        )
    }

    fn close(&mut self) -> Option<Box<RenderState>> {
        drop(self.stream.take());
        // A CPAL Drop that returns before its callback releases the closure is not
        // enough. Keep both queue endpoints owned until the kernel confirms close.
        // `signals` is the kernel's last field. Its final Arc release proves the
        // handoff/return queue fields have also finished destruction, so their
        // buffers cannot become last-owned and be freed on the callback thread.
        while !self.signals.closed.load(Ordering::Acquire) || Arc::strong_count(&self.signals) != 1
        {
            thread::park_timeout(OWNER_POLL);
        }
        self.returned.pop().ok()
    }
}

/// A successful live replacement returns to this exact session. A fault returns
/// the actual parked state so the existing device recovery loop retains it.
fn service_live_session<S>(
    session: &mut Session<S>,
    shared: &Shared,
    reclaimer: &mut EffectsReclaimer,
) -> Option<Box<RenderState>> {
    let scope = shared.live_effects.claim()?;
    let pin = match reclaimer.pin_owner().request(scope) {
        Ok(pin) => pin,
        Err(_) => {
            shared.live_effects.finish_on_owner(true);
            return None;
        }
    };
    let serial = pin.identity().serial;
    session.signals.handback_ack.store(0, Ordering::Release);
    session
        .signals
        .handback_serial
        .store(serial, Ordering::Release);
    let deadline = Instant::now() + Duration::from_secs(1);
    while session.signals.handback_ack.load(Ordering::Acquire) != serial {
        if matches!(pin.status(), PinStatus::Rejected | PinStatus::Released) {
            session.signals.handback_serial.store(0, Ordering::Release);
            drop(pin);
            shared.live_effects.finish_on_owner(true);
            return None;
        }
        if shared.shutdown.load(Ordering::Acquire)
            || session.signals.closed.load(Ordering::Acquire)
            || signal_error(&session.signals).is_some()
            || Instant::now() >= deadline
        {
            session.signals.handback_serial.store(0, Ordering::Release);
            // Actual close settles a callback racing the last serial read.
            // The sole state is recovered even if it was handed back late.
            let state = session.close();
            drop(pin);
            shared.live_effects.finish_on_owner(true);
            return state;
        }
        reclaimer.reclaim();
        thread::park_timeout(OWNER_POLL);
    }
    let mut state = session.returned.pop().unwrap_or_else(|_| {
        panic!("The actual live handback acknowledgement follows its state push")
    });
    let mut deferred = None;
    let mut retry = false;
    let mut fault = false;
    match shared.live_effects.take_on_owner() {
        Some(prepared) => match prepared.start_on_owner(&mut state, &pin, &shared.shutdown) {
            Ok(live_effects::LiveEffectsBegin::Deferred(prepared)) => deferred = Some(prepared),
            Ok(live_effects::LiveEffectsBegin::Capturing(capture)) => loop {
                match capture.advance_on_owner(&mut state, &shared.shutdown) {
                    Ok(live_effects::LiveEffectsProgress::Ready) => break,
                    Ok(live_effects::LiveEffectsProgress::Pending) => {
                        reclaimer.reclaim();
                    }
                    Ok(live_effects::LiveEffectsProgress::Suspended) => {
                        retry = true;
                        break;
                    }
                    Err(_) => {
                        retry = true;
                        if state.presentation_fence(scope.source_epoch).0
                            || state.format.sample_rate != scope.sample_rate
                            || state.format.channels != scope.device_channels
                        {
                            state.effects.invalidate_scope_on_owner(0, 0, true);
                        } else {
                            fault = true;
                        }
                        break;
                    }
                }
            },
            Err(_) => retry = true,
        },
        None => retry = true,
    }
    session.signals.handback_serial.store(0, Ordering::Release);
    drop(pin);
    if shared.shutdown.load(Ordering::Acquire) {
        state.effects.invalidate_scope_on_owner(0, 0, true);
    }
    if fault {
        let abandoned = session.close();
        assert!(
            abandoned.is_none(),
            "The live owner retains the sole actual state"
        );
        shared.live_effects.finish_on_owner(true);
        return Some(state);
    }
    session.signals.acknowledged.store(false, Ordering::Release);
    assert!(
        session.handoff.push(state).is_ok(),
        "The same session handoff slot is empty"
    );
    let deadline = Instant::now() + Duration::from_secs(1);
    while !session.signals.acknowledged.load(Ordering::Acquire) {
        if shared.shutdown.load(Ordering::Acquire)
            || session.signals.closed.load(Ordering::Acquire)
            || signal_error(&session.signals).is_some()
            || Instant::now() >= deadline
        {
            let state = session.close();
            shared.live_effects.finish_on_owner(true);
            return state;
        }
        reclaimer.reclaim();
        thread::park_timeout(OWNER_POLL);
    }
    if let Some(prepared) = deferred {
        shared.live_effects.defer_on_owner(prepared);
    } else {
        shared.live_effects.finish_on_owner(retry);
    }
    None
}

fn request(control: &OwnerControl) -> Request {
    control
        .request
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .clone()
}

fn reap(retired: &mut Consumer<Box<ConvertedSource>>, shared: &Shared) {
    let mut count = 0;
    while let Ok(source) = retired.pop() {
        drop(source);
        count += 1;
    }
    if count != 0 {
        shared.reclaimed.fetch_add(count, Ordering::AcqRel);
    }
}

fn fail(control: &OwnerControl, error: AudioOutputError, has_active: bool) {
    let mut snapshot = control
        .snapshot
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    snapshot.state.last_error = Some(error);
    snapshot.state.status = if has_active {
        AudioOutputStatus::Ready
    } else {
        AudioOutputStatus::Unavailable
    };
    control.shared.ready.store(has_active, Ordering::Release);
}

fn signal_error(signals: &KernelSignals) -> Option<AudioOutputError> {
    let errors = signals.errors.load(Ordering::Acquire);
    if errors & 1 != 0 {
        Some(AudioOutputError::DeviceNotAvailable)
    } else if errors & 2 != 0 {
        Some(AudioOutputError::StreamInvalidated)
    } else if errors & 8 != 0 {
        Some(AudioOutputError::BackendSpecific)
    } else {
        None
    }
}

fn run_owner<B: DeviceBackend>(
    mut backend: B,
    control: Arc<OwnerControl>,
    state: Box<RenderState>,
    mut retired: Consumer<Box<ConvertedSource>>,
    mut effect_reclaimer: EffectsReclaimer,
) {
    let shared = &control.shared;
    let mut parked = Some(state);
    let mut current: Option<Session<B::Stream>> = None;
    let mut processed_revision = 0;
    let mut processed_user_selection_revision = 0;
    let mut generation = 0u64;
    let mut switch_sequence = 0u64;
    let mut last_active_device_id: Option<String> = None;
    let mut equalizer_published: Option<(u64, u32)> = None;
    let mut next_topology = Instant::now();
    'owner_loop: loop {
        reap(&mut retired, shared);
        effect_reclaimer.reclaim();
        if shared.shutdown.load(Ordering::Acquire) {
            break;
        }
        if let Some(session) = &mut current
            && let Some(state) = service_live_session(session, shared, &mut effect_reclaimer)
        {
            current = None;
            parked = Some(state);
            shared.ready.store(false, Ordering::Release);
            shared.switching.store(true, Ordering::Release);
            processed_revision = 0;
            continue;
        }
        if let Some(state) = &mut parked {
            let token = shared.token.load(Ordering::Acquire);
            let (epoch, hard) = state.active.as_ref().map_or((0, token != 0), |active| {
                let (epoch, unavailable) = active.source.control.analysis_state();
                (
                    epoch,
                    unavailable
                        || active.token != token
                        || active.epoch != shared.epoch.load(Ordering::Acquire),
                )
            });
            state.effects.invalidate_scope_on_owner(token, epoch, hard);
            match state.effects.advance_handoff_on_owner() {
                Ok(true) => {}
                Ok(false) => {
                    thread::park_timeout(OWNER_POLL);
                    continue;
                }
                Err(_) => {
                    fail(&control, AudioOutputError::ConfigurationUnavailable, false);
                    thread::park_timeout(OWNER_POLL);
                    continue;
                }
            }
        }
        if let Some(session) = &current {
            let requested = *shared
                .equalizer_requested
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            if equalizer_published != Some((requested.sequence, session.format.sample_rate))
                && let Ok(plan) = EqualizerPlan::prepare(
                    requested.settings,
                    session.format.sample_rate,
                    requested.sequence,
                )
                && shared.equalizer_plan.publish(plan)
            {
                equalizer_published = Some((requested.sequence, session.format.sample_rate));
            }
        }
        if shared.shutdown.load(Ordering::Acquire) {
            break;
        }
        let recovering = current.as_ref().is_some_and(|session| {
            session.signals.errors.load(Ordering::Acquire) & 3 != 0
                || session.signals.closed.load(Ordering::Acquire)
                || {
                    let error = shared.recovery_errors.load(Ordering::Acquire);
                    error >> 8 == session.generation && error & 3 != 0
                }
        });
        if recovering {
            shared.switching.store(true, Ordering::Release);
            shared.ready.store(false, Ordering::Release);
            let mut old = current.take().expect("recovering active stream");
            let error = signal_error(&old.signals).unwrap_or(AudioOutputError::DeviceNotAvailable);
            parked = old.close();
            {
                let mut snapshot = control
                    .snapshot
                    .lock()
                    .unwrap_or_else(|error| error.into_inner());
                snapshot.state.active_device_id = None;
                snapshot.state.active_device_name = None;
                snapshot.state.format = None;
            }
            fail(&control, error, false);
            processed_revision = 0;
        }
        if let Some(session) = &current {
            let diagnostic = shared.diagnostics.load(Ordering::Acquire);
            if (diagnostic >> 8 == session.generation && diagnostic & 4 != 0)
                || session.signals.errors.load(Ordering::Acquire) & 4 != 0
            {
                shared.diagnostics.fetch_and(!4, Ordering::AcqRel);
                session.signals.errors.fetch_and(!4, Ordering::AcqRel);
                control
                    .snapshot
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .state
                    .last_error = Some(AudioOutputError::BufferUnderrun);
            }
        }
        let requested = request(&control);
        if requested.revision == processed_revision && Instant::now() < next_topology {
            thread::park_timeout(OWNER_POLL);
            continue;
        }
        let user_selection = requested.user_selection_revision > processed_user_selection_revision;
        processed_user_selection_revision = requested.user_selection_revision;
        processed_revision = requested.revision;
        next_topology = Instant::now() + TOPOLOGY_POLL;
        let inventory = match backend.inventory() {
            Ok(inventory) => inventory,
            Err(error) => {
                fail(&control, error, current.is_some());
                continue;
            }
        };
        {
            control
                .snapshot
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .devices = inventory.devices.clone();
        }
        let selected_id = match &requested.preferences.selection {
            OutputSelection::Default => inventory.default_id.as_ref(),
            OutputSelection::Device { id } => Some(id),
        };
        let selected = selected_id
            .and_then(|id| inventory.devices.iter().find(|device| &device.id == id))
            .cloned()
            .or_else(|| match &requested.preferences.selection {
                OutputSelection::Device { id } => backend.lookup(id),
                OutputSelection::Default => None,
            });
        let fallback = selected.is_none()
            && matches!(
                requested.preferences.selection,
                OutputSelection::Device { .. }
            );
        let target = selected.or_else(|| {
            inventory.default_id.as_ref().and_then(|id| {
                inventory
                    .devices
                    .iter()
                    .find(|device| &device.id == id)
                    .cloned()
            })
        });
        let Some(target) = target else {
            fail(
                &control,
                AudioOutputError::DeviceNotAvailable,
                current.is_some(),
            );
            continue;
        };
        if current
            .as_ref()
            .is_some_and(|session| session.device.id == target.id)
        {
            let mut snapshot = control
                .snapshot
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            snapshot.state.active_device_name = Some(target.name.clone());
            snapshot.state.fallback = fallback;
            snapshot.state.last_error = fallback.then_some(AudioOutputError::DeviceNotAvailable);
            snapshot.state.status = AudioOutputStatus::Ready;
            shared.ready.store(true, Ordering::Release);
            continue;
        }
        generation = match generation.checked_add(1) {
            Some(value) => value,
            None => break,
        };
        switch_sequence = match switch_sequence.checked_add(1) {
            Some(value) => value,
            None => break,
        };
        {
            let mut snapshot = control
                .snapshot
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            snapshot.state.status = AudioOutputStatus::Switching;
            snapshot.state.switch_sequence = switch_sequence;
            snapshot.state.last_error = None;
        }
        let (mut candidate, kernel) = Session::candidate(target, generation);
        let opened = backend.open(
            &candidate.device.id,
            kernel,
            control.shared.clone(),
            generation,
        );
        match opened {
            Ok(opened) => {
                candidate.stream = Some(opened.stream);
                candidate.format = opened.format;
            }
            Err(error) => {
                candidate.close();
                fail(&control, error, current.is_some());
                continue;
            }
        }
        if let Some(error) = signal_error(&candidate.signals) {
            candidate.close();
            fail(&control, error, current.is_some());
            continue;
        }
        if shared.shutdown.load(Ordering::Acquire)
            || request(&control).revision != requested.revision
        {
            candidate.close();
            // Retry the latest request immediately. No old source was fenced.
            processed_revision = 0;
            continue;
        }
        let previous_format = current
            .as_ref()
            .map(|session| session.format)
            .or_else(|| parked.as_ref().map(|state| state.format));
        let mut effect_pin: Option<EffectPinTicket> = None;
        let mut effect_reservation = None;
        let mut effect_retarget = None;
        let mut effect_capture = None;
        let mut effect_bus = candidate.format;
        let record = if current.is_some() {
            let owner_id = effect_reclaimer.active_owner();
            if owner_id != 0 {
                let Some(record) = effect_reclaimer.record(owner_id) else {
                    candidate.close();
                    fail(&control, AudioOutputError::ConfigurationUnavailable, true);
                    continue;
                };
                let old_plan = record.bounds().preparation();
                let scope = PinScope {
                    bank_owner: owner_id,
                    media_token: old_plan.token,
                    source_epoch: old_plan.source_epoch,
                    sample_rate: old_plan.format.sample_rate,
                    device_channels: old_plan.device_channels,
                };
                let pin = match effect_reclaimer.pin_owner().request(scope) {
                    Ok(pin) => pin,
                    Err(_) => {
                        candidate.close();
                        fail(&control, AudioOutputError::ConfigurationUnavailable, true);
                        continue;
                    }
                };
                let deadline = Instant::now() + Duration::from_secs(1);
                while pin.status() == PinStatus::Requested
                    && Instant::now() < deadline
                    && !shared.shutdown.load(Ordering::Acquire)
                    && request(&control).revision == requested.revision
                {
                    effect_reclaimer.reclaim();
                    thread::park_timeout(OWNER_POLL);
                }
                if !pin.is_pinned_for(scope)
                    || shared.shutdown.load(Ordering::Acquire)
                    || request(&control).revision != requested.revision
                {
                    drop(pin);
                    candidate.close();
                    processed_revision = 0;
                    continue;
                }
                effect_reclaimer.reclaim();
                if effect_reclaimer.retained_handoff_active() {
                    drop(pin);
                    candidate.close();
                    fail(&control, AudioOutputError::ConfigurationUnavailable, true);
                    continue;
                }
                effect_pin = Some(pin);
                Some(record)
            } else {
                None
            }
        } else if let Some(state) = &mut parked {
            let record = state.effects.owned_record_on_owner();
            if record.is_some() {
                state.effects.retire_queued_on_owner();
            } else if state.effects.has_active_on_owner() {
                if state.format != candidate.format {
                    candidate.close();
                    fail(&control, AudioOutputError::ConfigurationUnavailable, false);
                    continue;
                }
                effect_bus.channels = state
                    .effects
                    .bus_channels()
                    .unwrap_or(candidate.format.channels);
            }
            record
        } else {
            None
        };
        if let Some(record) = record {
            let old_plan = record.bounds().preparation();
            effect_bus.channels = record.recipe().effect_channels();
            let same = effect_bus == old_plan.format
                && candidate.format.channels == old_plan.device_channels;
            let parked_handoff = parked
                .as_ref()
                .is_some_and(|state| state.effects.has_handoff());
            if parked_handoff
                && !same
                && !parked
                    .as_ref()
                    .unwrap()
                    .effects
                    .can_retarget_handoff_on_owner()
                && !parked
                    .as_ref()
                    .unwrap()
                    .effects
                    .can_capture_handoff_on_owner()
            {
                candidate.close();
                fail(&control, AudioOutputError::ConfigurationUnavailable, false);
                continue;
            }
            let prepared = if same {
                None
            } else {
                let spatial_target = shared
                    .spatial_latest_target
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .unwrap_or(old_plan.initial_spatial)
                    .for_sample_rate(effect_bus.sample_rate)
                    .expect("Admitted candidate rate is a supported effect bus");
                let holder = match effect_reclaimer.bank_pool().begin(
                    record.recipe().clone(),
                    BankRequest {
                        format: effect_bus,
                        device_channels: candidate.format.channels,
                        token: old_plan.token,
                        source_epoch: old_plan.source_epoch,
                        replay_capacity: old_plan.replay_capacity,
                        initial_spatial: Some(spatial_target),
                    },
                ) {
                    Ok(holder) => holder,
                    Err(_) => {
                        candidate.close();
                        fail(
                            &control,
                            AudioOutputError::ConfigurationUnavailable,
                            current.is_some(),
                        );
                        continue;
                    }
                };
                match holder.prepare(&shared.shutdown) {
                    Ok(bank) => Some(bank),
                    Err(failure) => {
                        drop(failure);
                        candidate.close();
                        fail(
                            &control,
                            AudioOutputError::ConfigurationUnavailable,
                            current.is_some(),
                        );
                        continue;
                    }
                }
            };
            if parked_handoff
                && !same
                && parked
                    .as_ref()
                    .unwrap()
                    .effects
                    .can_capture_handoff_on_owner()
                && !parked
                    .as_ref()
                    .unwrap()
                    .effects
                    .can_retarget_handoff_on_owner()
            {
                match CountedHandoffReservation::reserve(
                    record,
                    effect_bus,
                    candidate.format.channels,
                    prepared,
                    &shared.shutdown,
                ) {
                    Ok(reservation) => effect_capture = Some(reservation),
                    Err(failure) => {
                        let _reason = failure.retire_candidate_on_owner();
                        candidate.close();
                        fail(&control, AudioOutputError::ConfigurationUnavailable, false);
                        continue;
                    }
                }
            } else if parked_handoff {
                effect_retarget = prepared;
            } else {
                match CountedHandoffReservation::reserve(
                    record,
                    effect_bus,
                    candidate.format.channels,
                    prepared,
                    &AtomicBool::new(false),
                ) {
                    Ok(reservation) => effect_reservation = Some(reservation),
                    Err(failure) => {
                        let _reason = failure.retire_candidate_on_owner();
                        candidate.close();
                        fail(
                            &control,
                            AudioOutputError::ConfigurationUnavailable,
                            current.is_some(),
                        );
                        continue;
                    }
                }
            }
        }
        let prepared_pitch = if previous_format == Some(candidate.format) {
            None
        } else {
            let (bound, rate) = if current.is_some() {
                (
                    shared.pitch_capacity.load(Ordering::Acquire),
                    shared.pitch_rate.load(Ordering::Acquire),
                )
            } else {
                let state = parked.as_ref().expect("owner retains parked render state");
                (
                    state.pitch.capacity_bound() as u64,
                    state.pitch.sample_rate(),
                )
            };
            match PitchProcessor::prepare_retarget(effect_bus, bound, rate) {
                Ok(prepared) => Some(prepared),
                Err(_) => {
                    candidate.close();
                    fail(
                        &control,
                        AudioOutputError::ConfigurationUnavailable,
                        current.is_some(),
                    );
                    continue;
                }
            }
        };
        let prepared_analysis = match AnalysisTap::prepare(candidate.format) {
            Ok(prepared) => prepared,
            Err(_) => {
                candidate.close();
                fail(
                    &control,
                    AudioOutputError::ConfigurationUnavailable,
                    current.is_some(),
                );
                continue;
            }
        };
        shared.switching.store(true, Ordering::Release);
        shared.ready.store(false, Ordering::Release);
        if !user_selection
            && last_active_device_id
                .as_ref()
                .is_some_and(|id| id != &candidate.device.id)
            && requested.preferences.pause_on_device_change
            && shared.token.load(Ordering::Acquire) != 0
            && !shared.paused.swap(true, Ordering::AcqRel)
        {
            shared.device_change_pause.store(true, Ordering::Release);
        }
        if let Some(mut old) = current.take() {
            parked = old.close();
            let mut snapshot = control
                .snapshot
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            snapshot.state.active_device_id = None;
            snapshot.state.active_device_name = None;
            snapshot.state.format = None;
        }
        if shared.shutdown.load(Ordering::Acquire)
            || request(&control).revision != requested.revision
        {
            candidate.close();
            processed_revision = 0;
            continue;
        }
        let mut state = parked.take().expect("one owner retains one render state");
        if let Some(reservation) = effect_capture.take() {
            if state.effects.capture_handoff_on_owner(reservation).is_err() {
                parked = Some(state);
                candidate.close();
                fail(&control, AudioOutputError::ConfigurationUnavailable, false);
                continue;
            }
            state.retained_pitch_source_rate = state.pitch.sample_rate();
            state.retained_pitch_source_latency_frames = state.pitch.latency_frames();
            loop {
                if shared.shutdown.load(Ordering::Acquire)
                    || request(&control).revision != requested.revision
                    || state.effects.owned_record_on_owner().is_some_and(|record| {
                        let plan = record.bounds().preparation();
                        shared.token.load(Ordering::Acquire) != plan.token
                            || state.active.as_ref().is_none_or(|active| {
                                let (epoch, unavailable) = active.source.control.analysis_state();
                                unavailable
                                    || epoch != plan.source_epoch
                                    || active.epoch != shared.epoch.load(Ordering::Acquire)
                            })
                    })
                {
                    parked = Some(state);
                    candidate.close();
                    processed_revision = 0;
                    continue 'owner_loop;
                }
                match state.effects.advance_handoff_on_owner() {
                    Ok(true) => break,
                    Ok(false) => effect_reclaimer.reclaim(),
                    Err(_) => {
                        parked = Some(state);
                        candidate.close();
                        fail(&control, AudioOutputError::ConfigurationUnavailable, false);
                        continue 'owner_loop;
                    }
                };
            }
        }
        if let Some(bank) = effect_retarget.take()
            && state.effects.retarget_handoff_on_owner(bank).is_err()
        {
            parked = Some(state);
            candidate.close();
            fail(&control, AudioOutputError::ConfigurationUnavailable, false);
            continue;
        }
        if let Some(reservation) = effect_reservation.take() {
            let owned = if let Some(pin) = effect_pin.as_ref() {
                state.effects.take_counted_on_owner(pin)
            } else {
                state.effects.take_parked_counted_on_owner()
            };
            let old = match owned {
                Ok(old) => old,
                Err(_) => {
                    parked = Some(state);
                    candidate.close();
                    fail(&control, AudioOutputError::ConfigurationUnavailable, false);
                    continue;
                }
            };
            let old_pitch_rate = state.pitch.sample_rate();
            let old_pitch_latency = state.pitch.latency_frames() as u64;
            let mut captured = match reservation.capture(old, &AtomicBool::new(false)) {
                Ok(captured) => captured,
                Err(failure) => {
                    let (old, retired) = failure.recover_old_on_owner();
                    if let Some(id) = retired {
                        effect_reclaimer.forget_retired_record(id);
                    }
                    state.effects.restore_counted_on_owner(old);
                    parked = Some(state);
                    candidate.close();
                    fail(&control, AudioOutputError::ConfigurationUnavailable, false);
                    continue;
                }
            };
            state.retained_pitch_source_rate = old_pitch_rate;
            state.retained_pitch_source_latency_frames =
                old_pitch_latency.min(u32::MAX as u64) as u32;
            loop {
                if shared.shutdown.load(Ordering::Acquire)
                    || request(&control).revision != requested.revision
                    || shared.token.load(Ordering::Acquire)
                        != captured.record().bounds().preparation().token
                    || state.active.as_ref().is_none_or(|active| {
                        let (epoch, unavailable) = active.source.control.analysis_state();
                        unavailable
                            || epoch != captured.record().bounds().preparation().source_epoch
                            || active.token != shared.token.load(Ordering::Acquire)
                            || active.epoch != shared.epoch.load(Ordering::Acquire)
                    })
                {
                    state.effects.install_unfinished_handoff_on_owner(captured);
                    parked = Some(state);
                    candidate.close();
                    processed_revision = 0;
                    continue 'owner_loop;
                }
                match captured.advance_capture(&AtomicBool::new(false)) {
                    Ok(true) => break,
                    Ok(false) => {
                        effect_reclaimer.reclaim();
                    }
                    Err(_) => {
                        // Preserve the evolved old DSP and actual FIFO. A later
                        // owner retry, rather than source re-feeding, resumes it.
                        state.effects.install_unfinished_handoff_on_owner(captured);
                        parked = Some(state);
                        candidate.close();
                        fail(&control, AudioOutputError::ConfigurationUnavailable, false);
                        continue 'owner_loop;
                    }
                }
            }
            if let Some(id) = captured.retire_old_on_owner() {
                effect_reclaimer.forget_retired_record(id);
            }
            assert!(
                effect_reclaimer.register_record(captured.record().clone()),
                "Old actual bank was retired before new record registration"
            );
            state.effects.install_handoff_on_owner(captured);
        }
        drop(effect_pin.take());
        let equalizer_request = *shared
            .equalizer_requested
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let equalizer_plan = match EqualizerPlan::prepare(
            equalizer_request.settings,
            candidate.format.sample_rate,
            equalizer_request.sequence,
        ) {
            Ok(plan) => plan,
            Err(_) => {
                parked = Some(state);
                candidate.close();
                fail(&control, AudioOutputError::ConfigurationUnavailable, false);
                continue;
            }
        };
        if state
            .retarget(
                candidate.format,
                equalizer_plan,
                prepared_pitch,
                prepared_analysis,
            )
            .is_err()
        {
            parked = Some(state);
            candidate.close();
            fail(&control, AudioOutputError::ConfigurationUnavailable, false);
            continue;
        }
        shared.equalizer_plan.publish(equalizer_plan);
        equalizer_published = Some((equalizer_request.sequence, candidate.format.sample_rate));
        assert!(
            candidate.handoff.push(state).is_ok(),
            "new handoff slot is empty"
        );
        loop {
            reap(&mut retired, shared);
            if candidate.signals.acknowledged.load(Ordering::Acquire)
                || candidate.signals.closed.load(Ordering::Acquire)
                || signal_error(&candidate.signals).is_some()
                || shared.shutdown.load(Ordering::Acquire)
                || request(&control).revision != requested.revision
            {
                break;
            }
            thread::park_timeout(OWNER_POLL);
        }
        if !candidate.signals.acknowledged.load(Ordering::Acquire)
            || candidate.signals.closed.load(Ordering::Acquire)
            || signal_error(&candidate.signals).is_some()
            || shared.shutdown.load(Ordering::Acquire)
            || request(&control).revision != requested.revision
        {
            let canceled = shared.shutdown.load(Ordering::Acquire)
                || request(&control).revision != requested.revision;
            let error =
                signal_error(&candidate.signals).unwrap_or(AudioOutputError::DeviceNotAvailable);
            parked = candidate.close();
            if canceled {
                processed_revision = 0;
            } else {
                fail(&control, error, false);
            }
            continue;
        }
        // Callback acknowledgement precedes publication of the active endpoint.
        shared
            .format_rate
            .store(candidate.format.sample_rate, Ordering::Release);
        shared
            .format_channels
            .store(candidate.format.channels as u32, Ordering::Release);
        shared
            .errors
            .store(candidate.generation << 8, Ordering::Release);
        shared
            .recovery_errors
            .store(candidate.generation << 8, Ordering::Release);
        shared
            .diagnostics
            .store(candidate.generation << 8, Ordering::Release);
        shared
            .stream_generation
            .store(candidate.generation, Ordering::Release);
        // A fatal error arriving after the final candidate check but before
        // generation publication was retained locally even while globally fenced.
        shared.errors.fetch_or(
            (candidate.signals.errors.load(Ordering::Acquire) & 8) as u64,
            Ordering::AcqRel,
        );
        {
            let mut snapshot = control
                .snapshot
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            snapshot.state.active_device_id = Some(candidate.device.id.clone());
            snapshot.state.active_device_name = Some(candidate.device.name.clone());
            snapshot.state.format = Some(output_format(candidate.format));
            snapshot.state.status = AudioOutputStatus::Ready;
            snapshot.state.fallback = fallback;
            snapshot.state.last_error = fallback.then_some(AudioOutputError::DeviceNotAvailable);
        }
        last_active_device_id = Some(candidate.device.id.clone());
        current = Some(candidate);
        shared.ready.store(true, Ordering::Release);
        shared.switching.store(false, Ordering::Release);
    }
    shared.switching.store(true, Ordering::Release);
    shared.ready.store(false, Ordering::Release);
    if let Some(mut session) = current {
        parked = session.close();
    }
    if let Some(state) = &mut parked {
        state.effects.close_owned();
    }
    drop(parked);
    shared.live_effects.finish_on_owner(false);
    effect_reclaimer.reclaim();
    reap(&mut retired, shared);
    shared.callback_alive.store(false, Ordering::Release);
    let mut snapshot = control
        .snapshot
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    snapshot.state.status = AudioOutputStatus::Closed;
    snapshot.state.active_device_id = None;
    snapshot.state.active_device_name = None;
    snapshot.state.format = None;
}

struct CpalBackend {
    host: cpal::Host,
}

impl CpalBackend {
    fn new() -> Self {
        Self {
            host: cpal::default_host(),
        }
    }
}

fn device_info(device: &cpal::Device, is_default: bool) -> Option<AudioDeviceInfo> {
    if !device.supports_output() {
        return None;
    }
    let id = device.id().ok()?.to_string();
    crate::domain::audio_output::validate_device_id(&id).ok()?;
    let name = bounded_name(
        device
            .description()
            .map(|description| description.name().to_owned())
            .unwrap_or_default(),
    );
    Some(AudioDeviceInfo {
        id,
        name,
        is_default,
    })
}

fn bounded_name(mut value: String) -> String {
    if value.len() > MAX_AUDIO_DEVICE_NAME_BYTES {
        let mut end = MAX_AUDIO_DEVICE_NAME_BYTES;
        while !value.is_char_boundary(end) {
            end -= 1;
        }
        value.truncate(end);
    }
    value.retain(|character| !character.is_control());
    if value.is_empty() {
        value.push_str("Audio output");
    }
    value
}

impl DeviceBackend for CpalBackend {
    type Stream = cpal::Stream;

    fn inventory(&mut self) -> std::result::Result<Inventory, AudioOutputError> {
        let mut devices = Vec::new();
        if let Some(default) = self
            .host
            .default_output_device()
            .as_ref()
            .and_then(|device| device_info(device, true))
        {
            devices.push(default);
        }
        let default_id = devices.first().map(|device| device.id.clone());
        for device in self
            .host
            .output_devices()
            .map_err(|_| AudioOutputError::InventoryFailed)?
        {
            if devices.len() >= MAX_AUDIO_DEVICES {
                break;
            }
            let Some(info) = device_info(&device, false) else {
                continue;
            };
            if devices.iter().any(|device| device.id == info.id) {
                continue;
            }
            devices.push(info);
        }
        Ok(Inventory {
            devices,
            default_id,
        })
    }

    fn lookup(&mut self, id: &str) -> Option<AudioDeviceInfo> {
        let id = id.parse::<cpal::DeviceId>().ok()?;
        let device = self.host.device_by_id(&id)?;
        device_info(&device, false)
    }

    fn open(
        &mut self,
        id: &str,
        kernel: RenderKernel,
        shared: Arc<Shared>,
        generation: u64,
    ) -> std::result::Result<Opened<Self::Stream>, AudioOutputError> {
        let id = id
            .parse::<cpal::DeviceId>()
            .map_err(|_| AudioOutputError::InvalidSelection)?;
        let device = self
            .host
            .device_by_id(&id)
            .filter(DeviceTrait::supports_output)
            .ok_or(AudioOutputError::DeviceNotAvailable)?;
        let supported = device
            .default_output_config()
            .map_err(|_| AudioOutputError::ConfigurationUnavailable)?;
        let config = supported.config();
        let format = PcmFormat {
            sample_rate: config.sample_rate,
            channels: config.channels,
        };
        if format.sample_rate == 0
            || format.channels == 0
            || format.channels as usize > MAX_CHANNELS
        {
            return Err(AudioOutputError::ConfigurationUnavailable);
        }
        let stream = match supported.sample_format() {
            SampleFormat::I8 => build::<i8>(&device, &config, kernel, shared, generation),
            SampleFormat::I16 => build::<i16>(&device, &config, kernel, shared, generation),
            SampleFormat::I24 => build::<cpal::I24>(&device, &config, kernel, shared, generation),
            SampleFormat::I32 => build::<i32>(&device, &config, kernel, shared, generation),
            SampleFormat::I64 => build::<i64>(&device, &config, kernel, shared, generation),
            SampleFormat::U8 => build::<u8>(&device, &config, kernel, shared, generation),
            SampleFormat::U16 => build::<u16>(&device, &config, kernel, shared, generation),
            SampleFormat::U24 => build::<cpal::U24>(&device, &config, kernel, shared, generation),
            SampleFormat::U32 => build::<u32>(&device, &config, kernel, shared, generation),
            SampleFormat::U64 => build::<u64>(&device, &config, kernel, shared, generation),
            SampleFormat::F32 => build::<f32>(&device, &config, kernel, shared, generation),
            SampleFormat::F64 => build::<f64>(&device, &config, kernel, shared, generation),
            _ => return Err(AudioOutputError::ConfigurationUnavailable),
        }
        .map_err(|_| AudioOutputError::StreamBuildFailed)?;
        stream
            .play()
            .map_err(|_| AudioOutputError::StreamStartFailed)?;
        Ok(Opened { stream, format })
    }
}

fn build<T: SizedSample + FromSample<f32>>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    mut kernel: RenderKernel,
    shared: Arc<Shared>,
    generation: u64,
) -> std::result::Result<cpal::Stream, cpal::BuildStreamError> {
    let signals = kernel
        .signals
        .as_ref()
        .expect("device callback signals")
        .clone();
    device.build_output_stream(
        config,
        move |samples: &mut [T], _| kernel.render(samples),
        move |error| {
            let flag = match &error {
                cpal::StreamError::DeviceNotAvailable => 1,
                cpal::StreamError::StreamInvalidated => 2,
                cpal::StreamError::BufferUnderrun => 4,
                cpal::StreamError::BackendSpecific { .. } => 8,
            };
            signals.errors.fetch_or(flag, Ordering::Release);
            shared.report_error_for(generation, &error);
        },
        Some(Duration::from_millis(100)),
    )
}

#[cfg(test)]
mod tests {
    use super::super::tests::no_heap;
    use super::*;
    use crate::audio::pcm::PcmProducer;

    fn device_id(suffix: &str) -> String {
        // CPAL host syntax is parsed without creating a host or enumerating hardware.
        format!("{}:{suffix}", cpal::ALL_HOSTS[0])
    }

    #[derive(Clone)]
    struct Endpoint {
        id: &'static str,
        name: &'static str,
        format: PcmFormat,
    }

    struct Topology {
        devices: Vec<Endpoint>,
        default: &'static str,
    }

    #[derive(Default)]
    struct Gate {
        blocked: AtomicBool,
        entered: AtomicBool,
    }

    impl Gate {
        fn wait(&self) {
            self.entered.store(true, Ordering::Release);
            while self.blocked.load(Ordering::Acquire) {
                thread::park_timeout(OWNER_POLL);
            }
        }
        fn block(&self) {
            self.entered.store(false, Ordering::Release);
            self.blocked.store(true, Ordering::Release);
        }
        fn release(&self) {
            self.blocked.store(false, Ordering::Release);
        }
    }

    struct Callback {
        kernel: Option<RenderKernel>,
        shared: Arc<Shared>,
        generation: u64,
    }

    struct MockControl {
        topology: Mutex<Topology>,
        callbacks: Mutex<Vec<Callback>>,
        open_gate: Gate,
        drop_gate: Gate,
        fail_b: AtomicBool,
        hide_b: AtomicBool,
        defer_drop: AtomicBool,
        opens: AtomicUsize,
    }

    impl MockControl {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                topology: Mutex::new(Topology {
                    devices: vec![
                        Endpoint {
                            id: "endpoint-a",
                            name: "A",
                            format: PcmFormat {
                                sample_rate: 48_000,
                                channels: 1,
                            },
                        },
                        Endpoint {
                            id: "endpoint-b",
                            name: "B",
                            format: PcmFormat {
                                sample_rate: 8_000,
                                channels: 2,
                            },
                        },
                    ],
                    default: "endpoint-a",
                }),
                callbacks: Mutex::new(Vec::new()),
                open_gate: Gate::default(),
                drop_gate: Gate::default(),
                fail_b: AtomicBool::new(false),
                hide_b: AtomicBool::new(false),
                defer_drop: AtomicBool::new(false),
                opens: AtomicUsize::new(0),
            })
        }

        fn render(&self, index: usize, samples: &mut [f32]) {
            let mut callbacks = self.callbacks.lock().unwrap();
            if let Some(kernel) = callbacks
                .get_mut(index)
                .and_then(|callback| callback.kernel.as_mut())
            {
                no_heap(|| kernel.render(samples));
            }
        }

        fn close_callback(&self, index: usize) {
            let kernel = self.callbacks.lock().unwrap()[index].kernel.take();
            no_heap(|| drop(kernel));
        }

        fn latest(&self) -> Option<usize> {
            self.callbacks.lock().unwrap().len().checked_sub(1)
        }

        fn ack_latest(&self) {
            if let Some(index) = self.latest() {
                self.render(index, &mut []);
            }
        }

        fn error(&self, index: usize, error: cpal::StreamError) {
            let callbacks = self.callbacks.lock().unwrap();
            let callback = &callbacks[index];
            if let Some(kernel) = &callback.kernel
                && let Some(signals) = &kernel.signals
            {
                let flag = match &error {
                    cpal::StreamError::DeviceNotAvailable => 1,
                    cpal::StreamError::StreamInvalidated => 2,
                    cpal::StreamError::BufferUnderrun => 4,
                    cpal::StreamError::BackendSpecific { .. } => 8,
                };
                no_heap(|| {
                    signals.errors.fetch_or(flag, Ordering::Release);
                });
            }
            no_heap(|| {
                callback
                    .shared
                    .report_error_for(callback.generation, &error)
            });
        }
    }

    struct MockBackend {
        control: Arc<MockControl>,
    }
    struct MockStream {
        control: Arc<MockControl>,
        index: usize,
    }

    impl Drop for MockStream {
        fn drop(&mut self) {
            self.control.drop_gate.wait();
            if !self.control.defer_drop.load(Ordering::Acquire) {
                self.control.close_callback(self.index);
            }
        }
    }

    impl DeviceBackend for MockBackend {
        type Stream = MockStream;
        fn inventory(&mut self) -> std::result::Result<Inventory, AudioOutputError> {
            let topology = self.control.topology.lock().unwrap();
            Ok(Inventory {
                devices: topology
                    .devices
                    .iter()
                    .filter(|endpoint| {
                        !(endpoint.id == "endpoint-b"
                            && self.control.hide_b.load(Ordering::Acquire))
                    })
                    .map(|endpoint| AudioDeviceInfo {
                        id: device_id(endpoint.id),
                        name: endpoint.name.into(),
                        is_default: endpoint.id == topology.default,
                    })
                    .collect(),
                default_id: Some(device_id(topology.default)),
            })
        }

        fn lookup(&mut self, id: &str) -> Option<AudioDeviceInfo> {
            self.control
                .topology
                .lock()
                .unwrap()
                .devices
                .iter()
                .find(|endpoint| device_id(endpoint.id) == id)
                .map(|endpoint| AudioDeviceInfo {
                    id: device_id(endpoint.id),
                    name: endpoint.name.into(),
                    is_default: false,
                })
        }

        fn open(
            &mut self,
            id: &str,
            kernel: RenderKernel,
            shared: Arc<Shared>,
            generation: u64,
        ) -> std::result::Result<Opened<Self::Stream>, AudioOutputError> {
            self.control.opens.fetch_add(1, Ordering::AcqRel);
            self.control.open_gate.wait();
            if id == device_id("endpoint-b") && self.control.fail_b.load(Ordering::Acquire) {
                return Err(AudioOutputError::StreamBuildFailed);
            }
            let format = self
                .control
                .topology
                .lock()
                .unwrap()
                .devices
                .iter()
                .find(|endpoint| device_id(endpoint.id) == id)
                .unwrap()
                .format;
            let mut callbacks = self.control.callbacks.lock().unwrap();
            let index = callbacks.len();
            callbacks.push(Callback {
                kernel: Some(kernel),
                shared,
                generation,
            });
            Ok(Opened {
                stream: MockStream {
                    control: self.control.clone(),
                    index,
                },
                format,
            })
        }
    }

    fn output(control: &Arc<MockControl>, preferences: AudioPreferences) -> Output {
        let (mut output, mut kernel) = Output::pair(MEMORY_FORMAT).unwrap();
        output.shared.ready.store(false, Ordering::Release);
        output.shared.device_managed.store(true, Ordering::Release);
        let state = kernel.state.take().unwrap();
        output.returned.take();
        let retired = output.retired.take().unwrap();
        let effect_reclaimer = output.effects.take_reclaimer().unwrap();
        let mock = control.clone();
        output.backend = Some(Backend::Device(
            spawn_owner(
                output.shared.clone(),
                state,
                retired,
                effect_reclaimer,
                preferences,
                move || MockBackend { control: mock },
            )
            .unwrap(),
        ));
        output
    }

    fn until(mut condition: impl FnMut() -> bool) {
        let started = Instant::now();
        while !condition() {
            assert!(
                started.elapsed() < Duration::from_secs(3),
                "mock owner failed to advance"
            );
            thread::sleep(Duration::from_millis(1));
        }
    }

    fn ready(output: &Output, control: &MockControl, id: &str) {
        until(|| {
            control.ack_latest();
            output.is_ready()
                && output.snapshot().state.active_device_id.as_deref()
                    == Some(device_id(id).as_str())
        });
    }

    fn source() -> (PcmProducer, PcmSource, PcmControl) {
        let (mut producer, source, control) = PcmProducer::new(
            PcmFormat {
                sample_rate: 8_000,
                channels: 1,
            },
            16,
            0,
            None,
        )
        .unwrap();
        for index in 0..16 {
            assert!(producer.push_frame(index, &[index as f32 / 20.0]));
        }
        producer.finish();
        (producer, source, control)
    }

    #[test]
    fn silent_candidate_ack_then_hot_switch_preserves_box_token_prefetch_and_clock() {
        let mock = MockControl::new();
        let mut output = output(&mock, AudioPreferences::default());
        until(|| mock.latest().is_some());
        let mut silence = [9.0f32; 3];
        mock.render(0, &mut silence);
        assert_eq!(silence, [0.0; 3]);
        ready(&output, &mock, "endpoint-a");
        let (_producer, source, control) = source();
        output.attach(source).unwrap();
        let token = output.token();
        let mut first = [0.0; 3];
        mock.render(0, &mut first);
        assert_eq!(first[0], 0.0);
        assert!((first[2] - 0.05 / 3.0).abs() < 1e-6);
        let consumed = control.snapshot().consumed_frames;
        assert_eq!(consumed, 2);
        let box_address = mock.callbacks.lock().unwrap()[0]
            .kernel
            .as_ref()
            .unwrap()
            .state
            .as_ref()
            .map(|state| &**state as *const RenderState as usize)
            .unwrap();

        output
            .select(OutputSelection::Device {
                id: device_id("endpoint-b"),
            })
            .unwrap();
        until(|| mock.latest() == Some(1));
        assert_ne!(
            output.snapshot().state.active_device_id.as_deref(),
            Some(device_id("endpoint-b").as_str())
        );
        // An ack with no frames cannot consume media, and active ID is published later.
        mock.render(1, &mut []);
        ready(&output, &mock, "endpoint-b");
        assert_eq!(output.token(), token);
        assert_eq!(control.snapshot().consumed_frames, consumed);
        assert!(control.consumer_alive());
        let callbacks = mock.callbacks.lock().unwrap();
        let state = callbacks[1]
            .kernel
            .as_ref()
            .unwrap()
            .state
            .as_ref()
            .unwrap();
        assert_eq!(&**state as *const RenderState as usize, box_address);
        drop(callbacks);
        let mut switched = [0.0f32; 4];
        mock.render(1, &mut switched);
        assert_eq!(switched[0], switched[1]);
        assert!(
            (switched[0] - 0.025).abs() < 1e-6,
            "fractional phase/prefetch lost: {switched:?}"
        );
        assert!((switched[2] - 0.075).abs() < 1e-6);
        assert_eq!(control.snapshot().consumed_frames, 4);
        assert!(control.snapshot().position_seconds > consumed as f64 / 8_000.0);
        assert!(output.shutdown(Duration::from_secs(1)));
        assert!(!control.consumer_alive());
    }

    #[test]
    fn failed_candidate_keeps_old_source_and_old_generation_errors_are_isolated() {
        let mock = MockControl::new();
        let mut output = output(&mock, AudioPreferences::default());
        ready(&output, &mock, "endpoint-a");
        let (_producer, source, control) = source();
        output.attach(source).unwrap();
        mock.render(0, &mut [0.0f32; 1]);
        let token = output.token();
        mock.fail_b.store(true, Ordering::Release);
        output
            .select(OutputSelection::Device {
                id: device_id("endpoint-b"),
            })
            .unwrap();
        until(|| output.snapshot().state.last_error == Some(AudioOutputError::StreamBuildFailed));
        assert_eq!(
            output.snapshot().state.active_device_id.as_deref(),
            Some(device_id("endpoint-a").as_str())
        );
        assert_eq!(output.token(), token);
        assert!(control.consumer_alive());
        assert_eq!(output.poll_error(), None);
        mock.fail_b.store(false, Ordering::Release);
        output.refresh().unwrap();
        ready(&output, &mock, "endpoint-b");
        mock.error(0, cpal::StreamError::DeviceNotAvailable);
        assert_eq!(output.poll_error(), None);
        mock.error(1, cpal::StreamError::BufferUnderrun);
        assert_eq!(output.poll_error(), None);
        until(|| output.snapshot().state.last_error == Some(AudioOutputError::BufferUnderrun));
        assert!(output.shutdown(Duration::from_secs(1)));
    }

    #[test]
    fn blocked_open_is_owned_shutdown_uses_remaining_deadline_and_retry_joins() {
        let mock = MockControl::new();
        mock.open_gate.block();
        let mut output = output(&mock, AudioPreferences::default());
        until(|| mock.open_gate.entered.load(Ordering::Acquire));
        let started = Instant::now();
        assert!(!output.shutdown(Duration::from_millis(25)));
        assert!(started.elapsed() < Duration::from_millis(100));
        assert!(crate::audio::worker_owner::outstanding(WorkerKind::DeviceOwner) >= 1);
        mock.open_gate.release();
        assert!(output.shutdown(Duration::from_secs(1)));
        assert_eq!(output.snapshot().state.status, AudioOutputStatus::Closed);
    }

    #[test]
    fn blocked_old_drop_fences_pcm_and_retains_state_until_retry() {
        let mock = MockControl::new();
        let mut output = output(&mock, AudioPreferences::default());
        ready(&output, &mock, "endpoint-a");
        let (_producer, source, control) = source();
        output.attach(source).unwrap();
        mock.render(0, &mut [0.0f32; 1]);
        let frames = control.snapshot().consumed_frames;
        mock.drop_gate.block();
        output
            .select(OutputSelection::Device {
                id: device_id("endpoint-b"),
            })
            .unwrap();
        until(|| mock.drop_gate.entered.load(Ordering::Acquire));
        let mut samples = [1.0f32; 9];
        mock.render(0, &mut samples);
        assert_eq!(samples, [0.0; 9]);
        assert_eq!(control.snapshot().consumed_frames, frames);
        assert!(!output.shutdown(Duration::from_millis(10)));
        assert!(control.consumer_alive());
        mock.drop_gate.release();
        assert!(output.shutdown(Duration::from_secs(1)));
        assert!(!control.consumer_alive());
    }

    #[test]
    fn handle_drop_retains_return_queues_during_delayed_callback_close() {
        let mock = MockControl::new();
        let mut output = output(&mock, AudioPreferences::default());
        ready(&output, &mock, "endpoint-a");
        let (_producer, source, control) = source();
        output.attach(source).unwrap();
        mock.render(0, &mut [0.0f32; 1]);
        mock.defer_drop.store(true, Ordering::Release);
        let worker = match output.backend.as_ref().unwrap() {
            Backend::Device(handle) => handle.worker.clone(),
            _ => unreachable!(),
        };
        drop(output);
        until(|| mock.drop_gate.entered.load(Ordering::Acquire));
        assert!(control.consumer_alive());
        assert!(!worker.wait(Duration::from_millis(10)));
        mock.close_callback(0);
        assert!(worker.wait(Duration::from_secs(1)));
        assert!(!control.consumer_alive());
    }

    #[test]
    fn canceled_open_and_late_ack_cannot_publish_stale_endpoint() {
        let mock = MockControl::new();
        let mut output = output(&mock, AudioPreferences::default());
        ready(&output, &mock, "endpoint-a");
        mock.open_gate.block();
        output
            .select(OutputSelection::Device {
                id: device_id("endpoint-b"),
            })
            .unwrap();
        until(|| mock.open_gate.entered.load(Ordering::Acquire));
        output.select(OutputSelection::Default).unwrap();
        mock.open_gate.release();
        until(|| mock.latest() == Some(1));
        mock.ack_latest();
        ready(&output, &mock, "endpoint-a");
        assert_eq!(output.snapshot().state.desired, OutputSelection::Default);
        assert!(output.shutdown(Duration::from_secs(1)));
    }

    #[test]
    fn canceled_handoff_reuses_pcm_and_late_callback_ack_cannot_commit() {
        let mock = MockControl::new();
        let mut output = output(&mock, AudioPreferences::default());
        ready(&output, &mock, "endpoint-a");
        let (_producer, source, control) = source();
        output.attach(source).unwrap();
        mock.render(0, &mut [0.0f32; 3]);
        let frames = control.snapshot().consumed_frames;
        let token = output.token();
        output
            .select(OutputSelection::Device {
                id: device_id("endpoint-b"),
            })
            .unwrap();
        until(|| {
            mock.latest() == Some(1) && output.shared.switching.load(Ordering::Acquire) && {
                let callbacks = mock.callbacks.lock().unwrap();
                callbacks[0].kernel.is_none()
                    && callbacks[1].kernel.as_ref().is_some_and(|kernel| {
                        kernel
                            .handoff
                            .as_ref()
                            .is_some_and(|handoff| !handoff.is_empty())
                    })
            }
        });
        mock.defer_drop.store(true, Ordering::Release);
        output.select(OutputSelection::Default).unwrap();
        // A callback that acknowledges a superseded fence stays silent.
        let mut late = [9.0f32; 2];
        mock.render(1, &mut late);
        assert_eq!(late, [0.0; 2]);
        assert_ne!(
            output.snapshot().state.active_device_id.as_deref(),
            Some(device_id("endpoint-b").as_str())
        );
        mock.close_callback(1);
        mock.defer_drop.store(false, Ordering::Release);
        ready(&output, &mock, "endpoint-a");
        assert_eq!(output.token(), token);
        assert_eq!(control.snapshot().consumed_frames, frames);
        assert!(control.consumer_alive());
        assert_eq!(output.snapshot().state.desired, OutputSelection::Default);
        assert!(output.shutdown(Duration::from_secs(1)));
    }

    #[test]
    fn paused_eof_and_seek_epoch_survive_switch_without_clearing_media_token() {
        let mock = MockControl::new();
        let mut output = output(&mock, AudioPreferences::default());
        ready(&output, &mock, "endpoint-a");
        let (mut producer, source, control) = PcmProducer::new(
            PcmFormat {
                sample_rate: 8_000,
                channels: 1,
            },
            2,
            0,
            None,
        )
        .unwrap();
        assert!(producer.push_frame(0, &[0.4]));
        producer.finish();
        output.attach(source).unwrap();
        mock.render(0, &mut [0.0f32; 7]);
        assert!(output.ended());
        let token = output.token();
        output.set_paused(true);
        output
            .select(OutputSelection::Device {
                id: device_id("endpoint-b"),
            })
            .unwrap();
        ready(&output, &mock, "endpoint-b");
        assert!(output.ended());
        assert_eq!(output.token(), token);
        assert!(control.consumer_alive());
        assert!(!control.snapshot().cancelled);
        producer.begin_epoch().unwrap();
        assert!(producer.push_frame(800, &[-0.3]));
        producer.commit_epoch(800).unwrap();
        producer.finish();
        assert!(!output.ended());
        let mut samples = [1.0f32; 2];
        mock.render(1, &mut samples);
        assert_eq!(samples, [0.0; 2]);
        assert_eq!(control.snapshot().consumed_frames, 1);
        output.set_paused(false);
        mock.render(1, &mut samples);
        assert_eq!(samples, [-0.3; 2]);
        assert!((control.snapshot().position_seconds - 801.0 / 8_000.0).abs() < 1e-9);
        assert_eq!(output.token(), token);
        assert!(output.shutdown(Duration::from_secs(1)));
    }

    #[test]
    fn topology_policy_uses_actual_endpoint_changes_and_mute_keeps_clock() {
        let mock = MockControl::new();
        let mut output = output(
            &mock,
            AudioPreferences {
                pause_on_device_change: true,
                ..Default::default()
            },
        );
        ready(&output, &mock, "endpoint-a");
        let (_producer, source, control) = source();
        output.attach(source).unwrap();
        output.set_gain(0.4);
        output.set_mute(true);
        let mut samples = [9.0f32; 7];
        mock.render(0, &mut samples);
        assert_eq!(samples, [0.0; 7]);
        let frames = control.snapshot().consumed_frames;
        assert!(frames > 0);
        mock.topology.lock().unwrap().devices[1].name = "Renamed unused endpoint";
        output.refresh().unwrap();
        until(|| output.snapshot().devices[1].name == "Renamed unused endpoint");
        assert!(!output.take_device_change_pause());
        assert!(!output.shared.paused.load(Ordering::Acquire));
        mock.topology.lock().unwrap().default = "endpoint-b";
        output.refresh().unwrap();
        ready(&output, &mock, "endpoint-b");
        assert!(output.take_device_change_pause());
        assert!(output.shared.paused.load(Ordering::Acquire));
        mock.render(1, &mut samples);
        assert_eq!(control.snapshot().consumed_frames, frames);
        output.set_paused(false);
        output.set_mute(false);
        mock.render(1, &mut [0.0f32; 2]);
        assert!(control.snapshot().consumed_frames > frames);
        assert_eq!(
            f32::from_bits(output.shared.gain.load(Ordering::Acquire)),
            0.4
        );
        assert!(output.shutdown(Duration::from_secs(1)));
    }

    #[test]
    fn runtime_invalidation_rebuilds_same_endpoint_without_media_cancellation() {
        let mock = MockControl::new();
        let mut output = output(
            &mock,
            AudioPreferences {
                pause_on_device_change: true,
                ..Default::default()
            },
        );
        ready(&output, &mock, "endpoint-a");
        let (_producer, source, control) = source();
        output.attach(source).unwrap();
        mock.render(0, &mut [0.0f32; 3]);
        let frames = control.snapshot().consumed_frames;
        let token = output.token();
        let address = mock.callbacks.lock().unwrap()[0]
            .kernel
            .as_ref()
            .unwrap()
            .state
            .as_ref()
            .map(|state| &**state as *const RenderState as usize)
            .unwrap();
        mock.error(0, cpal::StreamError::StreamInvalidated);
        assert_eq!(output.poll_error(), None);
        until(|| mock.latest() == Some(1));
        assert!(!output.is_ready());
        assert!(control.consumer_alive());
        ready(&output, &mock, "endpoint-a");
        let callbacks = mock.callbacks.lock().unwrap();
        let state = callbacks[1]
            .kernel
            .as_ref()
            .unwrap()
            .state
            .as_ref()
            .unwrap();
        assert_eq!(&**state as *const RenderState as usize, address);
        drop(callbacks);
        assert_eq!(output.token(), token);
        assert_eq!(control.snapshot().consumed_frames, frames);
        assert!(!control.snapshot().cancelled);
        assert!(!output.take_device_change_pause());
        let opens = mock.opens.load(Ordering::Acquire);
        mock.error(1, cpal::StreamError::BufferUnderrun);
        assert_eq!(output.poll_error(), None);
        until(|| output.snapshot().state.last_error == Some(AudioOutputError::BufferUnderrun));
        assert_eq!(mock.opens.load(Ordering::Acquire), opens);
        assert!(output.is_ready());
        assert!(output.shutdown(Duration::from_secs(1)));
    }

    #[test]
    fn runtime_disconnect_retains_pcm_without_endpoint_and_reconnects() {
        let mock = MockControl::new();
        let mut output = output(&mock, AudioPreferences::default());
        ready(&output, &mock, "endpoint-a");
        let (_producer, source, control) = source();
        output.attach(source).unwrap();
        mock.render(0, &mut [0.0f32; 3]);
        let frames = control.snapshot().consumed_frames;
        let token = output.token();
        let devices = std::mem::take(&mut mock.topology.lock().unwrap().devices);
        mock.error(0, cpal::StreamError::DeviceNotAvailable);
        assert_eq!(output.poll_error(), None);
        until(|| {
            output.snapshot().state.status == AudioOutputStatus::Unavailable && !output.is_ready()
        });
        assert_eq!(output.snapshot().state.active_device_id, None);
        assert!(control.consumer_alive());
        assert!(!control.snapshot().cancelled);
        assert_eq!(control.snapshot().consumed_frames, frames);
        {
            let mut topology = mock.topology.lock().unwrap();
            topology.devices = devices;
            topology.default = "endpoint-b";
        }
        output.refresh().unwrap();
        ready(&output, &mock, "endpoint-b");
        assert_eq!(output.token(), token);
        assert_eq!(control.snapshot().consumed_frames, frames);
        let mut samples = [0.0f32; 2];
        mock.render(1, &mut samples);
        assert!((samples[0] - 0.025).abs() < 1e-6);
        assert_eq!(samples[0], samples[1]);
        assert!(output.shutdown(Duration::from_secs(1)));
    }

    #[test]
    fn failed_runtime_rebuild_is_unavailable_and_retry_is_throttled() {
        let mock = MockControl::new();
        let mut output = output(&mock, AudioPreferences::default());
        ready(&output, &mock, "endpoint-a");
        let (_producer, source, control) = source();
        output.attach(source).unwrap();
        mock.render(0, &mut [0.0f32; 3]);
        let token = output.token();
        mock.fail_b.store(true, Ordering::Release);
        mock.topology.lock().unwrap().default = "endpoint-b";
        mock.error(0, cpal::StreamError::DeviceNotAvailable);
        until(|| output.snapshot().state.last_error == Some(AudioOutputError::StreamBuildFailed));
        assert!(!output.is_ready());
        assert_eq!(
            output.snapshot().state.status,
            AudioOutputStatus::Unavailable
        );
        assert_eq!(output.snapshot().state.active_device_id, None);
        assert_eq!(output.token(), token);
        assert!(control.consumer_alive());
        let opens = mock.opens.load(Ordering::Acquire);
        thread::sleep(Duration::from_millis(40));
        assert_eq!(mock.opens.load(Ordering::Acquire), opens);
        mock.fail_b.store(false, Ordering::Release);
        output.refresh().unwrap();
        ready(&output, &mock, "endpoint-b");
        assert_eq!(output.token(), token);
        assert!(!control.snapshot().cancelled);
        assert!(output.shutdown(Duration::from_secs(1)));
    }

    #[test]
    fn historical_user_selection_does_not_hide_topology_fallback_pause() {
        let mock = MockControl::new();
        let mut output = output(
            &mock,
            AudioPreferences {
                pause_on_device_change: true,
                ..Default::default()
            },
        );
        ready(&output, &mock, "endpoint-a");
        output
            .select(OutputSelection::Device {
                id: device_id("endpoint-b"),
            })
            .unwrap();
        ready(&output, &mock, "endpoint-b");
        let (_producer, source, control) = source();
        output.attach(source).unwrap();
        mock.render(1, &mut [0.0f32; 2]);
        let frames = control.snapshot().consumed_frames;
        assert!(!output.take_device_change_pause());
        mock.topology
            .lock()
            .unwrap()
            .devices
            .retain(|device| device.id != "endpoint-b");
        mock.error(1, cpal::StreamError::DeviceNotAvailable);
        ready(&output, &mock, "endpoint-a");
        assert!(output.snapshot().state.fallback);
        assert_eq!(
            output.snapshot().state.desired,
            OutputSelection::Device {
                id: device_id("endpoint-b")
            }
        );
        assert!(output.take_device_change_pause());
        assert!(output.shared.paused.load(Ordering::Acquire));
        assert_eq!(control.snapshot().consumed_frames, frames);
        assert!(control.consumer_alive());
        assert!(output.shutdown(Duration::from_secs(1)));
    }

    #[test]
    fn lookup_resolves_selected_endpoint_outside_bounded_inventory() {
        let mock = MockControl::new();
        mock.hide_b.store(true, Ordering::Release);
        let mut output = output(&mock, AudioPreferences::default());
        ready(&output, &mock, "endpoint-a");
        assert_eq!(output.snapshot().devices.len(), 1);
        output
            .select(OutputSelection::Device {
                id: device_id("endpoint-b"),
            })
            .unwrap();
        ready(&output, &mock, "endpoint-b");
        assert!(!output.snapshot().state.fallback);
        assert_eq!(output.snapshot().devices.len(), 1);
        assert!(output.shutdown(Duration::from_secs(1)));
    }

    #[test]
    fn display_name_bounds_preserve_utf8_and_remove_control_characters() {
        let value = format!("\n{}\u{7f}", "音".repeat(512));
        let name = bounded_name(value);
        assert!(name.len() <= MAX_AUDIO_DEVICE_NAME_BYTES);
        assert!(!name.is_empty());
        assert!(!name.chars().any(char::is_control));
        crate::domain::audio_output::validate_device_name(&name).unwrap();
        assert_eq!(bounded_name("\n\t".into()), "Audio output");
    }

    #[test]
    fn equalizer_retargets_latest_pending_setting_on_same_pcm_box() {
        let mock = MockControl::new();
        let mut output = output(&mock, AudioPreferences::default());
        ready(&output, &mock, "endpoint-a");
        let (_producer, source, control) = source();
        output.attach(source).unwrap();
        mock.render(0, &mut [0.0f32; 1]);
        let token = output.token();
        let consumed = control.snapshot().consumed_frames;
        let mut settings = EqualizerSettings::default();
        settings.gains_db[9] = 9.0;
        output.set_equalizer(settings, 7).unwrap();
        until(|| {
            mock.render(0, &mut []);
            output.equalizer_snapshot().applied_sequence == 7
        });
        assert_eq!(output.equalizer_snapshot().sample_rate, Some(48_000));
        assert!(output.equalizer_snapshot().active_bands[9]);
        mock.open_gate.block();
        output
            .select(OutputSelection::Device {
                id: device_id("endpoint-b"),
            })
            .unwrap();
        until(|| mock.open_gate.entered.load(Ordering::Acquire));
        let mut latest = settings;
        latest.gains_db[5] = -6.0;
        output.set_equalizer(latest, 8).unwrap();
        mock.open_gate.release();
        ready(&output, &mock, "endpoint-b");
        mock.render(1, &mut []);
        let snapshot = output.equalizer_snapshot();
        assert_eq!(snapshot.applied_sequence, 8);
        assert_eq!(snapshot.applied, latest);
        assert_eq!(snapshot.sample_rate, Some(8_000));
        assert!(!snapshot.active_bands[9]);
        assert_eq!(snapshot.desired.gains_db[9], 9.0);
        assert_eq!(snapshot.transition_remaining_frames, 0);
        assert_eq!(output.token(), token);
        assert_eq!(control.snapshot().consumed_frames, consumed);
        assert!(output.shutdown(Duration::from_secs(1)));
    }

    #[test]
    fn equalizer_bank_transfer_preserves_single_consumer_and_silences_old_history_during_fence() {
        let mock = MockControl::new();
        let mut output = output(&mock, AudioPreferences::default());
        ready(&output, &mock, "endpoint-a");
        let (mut producer, source, control) = PcmProducer::new(
            PcmFormat {
                sample_rate: 48_000,
                channels: 1,
            },
            8,
            0,
            None,
        )
        .unwrap();
        for frame in 0..8 {
            assert!(producer.push_frame(frame, &[if frame == 0 { 0.001 } else { 0.0 }]));
        }
        producer.finish();
        output
            .set_equalizer(
                crate::domain::equalizer::EqualizerPreset::Rock.settings(),
                1,
            )
            .unwrap();
        until(|| {
            output
                .shared
                .equalizer_plan
                .sequence
                .load(Ordering::Acquire)
                == 1
        });
        output.attach(source).unwrap();
        let mut first = [0.0f32; 2];
        mock.render(0, &mut first);
        assert_ne!(first[1], 0.0);
        let consumed = control.snapshot().consumed_frames;
        mock.drop_gate.block();
        output
            .select(OutputSelection::Device {
                id: device_id("endpoint-b"),
            })
            .unwrap();
        until(|| mock.drop_gate.entered.load(Ordering::Acquire));
        let mut silent = [9.0f32; 2];
        mock.render(0, &mut silent);
        assert_eq!(silent, [0.0; 2]);
        assert_eq!(control.snapshot().consumed_frames, consumed);
        assert!(control.consumer_alive());
        mock.drop_gate.release();
        ready(&output, &mock, "endpoint-b");
        mock.render(1, &mut silent);
        assert_eq!(silent, [0.0; 2]);
        assert_eq!(output.equalizer_snapshot().sample_rate, Some(8_000));
        assert!(output.shutdown(Duration::from_secs(1)));
    }

    #[test]
    fn pitch_retarget_quota_rejection_keeps_the_old_endpoint_and_callback_running() {
        let mock = MockControl::new();
        {
            let mut topology = mock.topology.lock().unwrap();
            topology.devices[0].format = PcmFormat {
                sample_rate: 8_000,
                channels: 1,
            };
            topology.devices[1].format = PcmFormat {
                sample_rate: 384_000,
                channels: 8,
            };
        }
        let mut output = output(&mock, AudioPreferences::default());
        ready(&output, &mock, "endpoint-a");
        let settings = PlaybackProcessingSettings {
            pitch_ratio: 1.25,
            ..PlaybackProcessingSettings::default()
        };
        output.set_processing(settings, 7).unwrap();
        let count = 8192;
        let (mut producer, source, control) = PcmProducer::new(
            PcmFormat {
                sample_rate: 8_000,
                channels: 1,
            },
            count,
            0,
            None,
        )
        .unwrap();
        for index in 0..count {
            let sample = (std::f32::consts::TAU * 997.0 * index as f32 / 8_000.0).cos() * 0.1;
            assert!(producer.push_processed_frame(
                ProcessingStamp {
                    end_frame: index as u64 + 1,
                    processing_sequence: 7,
                    settings,
                    ..ProcessingStamp::default()
                },
                &[sample]
            ));
        }
        producer.finish();
        output.attach(source).unwrap();
        let token = output.token();
        let mut startup = [0.0f32; 4096 + 128];
        mock.render(0, &mut startup);
        assert!(startup[4096..].iter().any(|sample| *sample != 0.0));
        let before = control.snapshot();
        output
            .select(OutputSelection::Device {
                id: device_id("endpoint-b"),
            })
            .unwrap();
        until(|| {
            output.snapshot().state.last_error == Some(AudioOutputError::ConfigurationUnavailable)
        });
        assert_eq!(
            output.snapshot().state.active_device_id,
            Some(device_id("endpoint-a"))
        );
        assert!(output.is_ready());
        assert_eq!(output.token(), token);
        assert_eq!(
            output.format(),
            PcmFormat {
                sample_rate: 8_000,
                channels: 1
            }
        );
        assert!(mock.callbacks.lock().unwrap()[0].kernel.is_some());
        let mut continued = [0.0f32; 128];
        mock.render(0, &mut continued);
        assert!(continued.iter().any(|sample| *sample != 0.0));
        assert!(control.snapshot().position_seconds > before.position_seconds);
        assert!(control.snapshot().consumed_frames > before.consumed_frames);
        assert!(output.shutdown(Duration::from_secs(1)));
    }

    #[test]
    fn active_pitched_format_switch_preserves_pending_original_intervals_and_source_token() {
        let mock = MockControl::new();
        {
            let mut topology = mock.topology.lock().unwrap();
            topology.devices[0].format = PcmFormat {
                sample_rate: 8_000,
                channels: 1,
            };
            topology.devices[1].format = PcmFormat {
                sample_rate: 48_000,
                channels: 2,
            };
        }
        let mut output = output(&mock, AudioPreferences::default());
        ready(&output, &mock, "endpoint-a");
        let settings = PlaybackProcessingSettings {
            pitch_ratio: 1.25,
            ..PlaybackProcessingSettings::default()
        };
        output.set_processing(settings, 7).unwrap();
        let count = 6000;
        let (mut producer, source, control) = PcmProducer::new(
            PcmFormat {
                sample_rate: 8_000,
                channels: 1,
            },
            count,
            0,
            None,
        )
        .unwrap();
        for index in 0..count {
            let sample = (std::f32::consts::TAU * 997.0 * index as f32 / 8_000.0).cos() * 0.1;
            assert!(producer.push_processed_frame(
                ProcessingStamp {
                    end_frame: index as u64 + 1,
                    processing_sequence: 7,
                    settings,
                    ..ProcessingStamp::default()
                },
                &[sample]
            ));
        }
        producer.finish();
        output.attach(source).unwrap();
        let token = output.token();
        mock.render(0, &mut [0.0f32; 2048]);
        assert_eq!(control.snapshot().position_seconds, 0.0);
        assert_eq!(control.snapshot().consumed_frames, 2048);
        output
            .select(OutputSelection::Device {
                id: device_id("endpoint-b"),
            })
            .unwrap();
        ready(&output, &mock, "endpoint-b");
        assert_eq!(control.snapshot().position_seconds, 0.0);
        assert_eq!(control.snapshot().consumed_frames, 2048);
        let current = mock.latest().unwrap();
        let mut samples = [0.0f32; 256];
        mock.render(current, &mut samples);
        assert!(control.snapshot().position_seconds > 0.0);
        assert_eq!(
            control.snapshot().consumed_frames,
            2048,
            "prepared output cannot pull new source data"
        );
        let mut previous = control.snapshot().position_seconds;
        let mut batches = 0;
        while !output.ended() {
            mock.render(current, &mut samples);
            assert!(
                samples
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .all(|frame| frame[0] == frame[1])
            );
            let position = control.snapshot().position_seconds;
            assert!(position >= previous);
            previous = position;
            batches += 1;
            assert!(batches < 500);
        }
        assert_eq!(output.token(), token);
        assert_eq!(control.snapshot().consumed_frames, count as u64);
        assert!((previous - count as f64 / 8_000.0).abs() < 1e-12);
        assert_eq!(output.processing_snapshot().applied_sequence, 7);
        assert_eq!(output.processing_snapshot().sample_rate, Some(48_000));
        assert!(control.consumer_alive());
        assert!(!control.snapshot().cancelled);
        assert!(output.shutdown(Duration::from_secs(1)));
    }

    fn counted_impulse_fixture() -> (
        Arc<MockControl>,
        Output,
        PcmControl,
        crate::audio::effects_factory::EffectsBankPool,
    ) {
        use crate::audio::{
            convolution::ImpulseBuilder,
            effects::EFFECTS_MIN_REPLAY_FRAMES,
            effects_factory::{EffectsRecipe, OwnedImpulse},
        };
        use crate::domain::audio_effects::{EffectsSettings, ImpulseResourceId};
        let mock = MockControl::new();
        {
            let mut topology = mock.topology.lock().unwrap();
            topology.devices[0].format = PcmFormat {
                sample_rate: 8000,
                channels: 1,
            };
            topology.devices[1].format = PcmFormat {
                sample_rate: 16000,
                channels: 2,
            };
        }
        let mut output = output(&mock, AudioPreferences::default());
        ready(&output, &mock, "endpoint-a");
        let (mut producer, source, control) = PcmProducer::new(
            PcmFormat {
                sample_rate: 8000,
                channels: 1,
            },
            128,
            0,
            None,
        )
        .unwrap();
        for index in 0..128 {
            assert!(producer.push_frame(index, &[0.1]));
        }
        producer.finish();
        output.attach(source).unwrap();
        let id = ImpulseResourceId::Bundled("owned-format-fixture".into());
        let mut builder = ImpulseBuilder::prepare(8000, 1, 17).unwrap();
        for index in 0..17 {
            builder
                .push_frame(&[if index == 0 { 1.0 } else { 0.01 }])
                .unwrap();
        }
        let ir =
            OwnedImpulse::from_validated(builder.finish().unwrap(), id.clone(), "1".repeat(64))
                .unwrap();
        let mut settings = EffectsSettings::default();
        settings.convolution.impulse = Some(id);
        settings.convolution.send_gain = 0.3;
        let recipe = EffectsRecipe::prepare(settings, 1, 2, false, Some(ir)).unwrap();
        let pool = output.effects.bank_pool();
        let bank = pool
            .begin(
                recipe,
                BankRequest {
                    format: PcmFormat {
                        sample_rate: 8000,
                        channels: 2,
                    },
                    device_channels: 1,
                    token: output.token(),
                    source_epoch: control.current_epoch(),
                    replay_capacity: EFFECTS_MIN_REPLAY_FRAMES,
                    initial_spatial: None,
                },
            )
            .unwrap()
            .prepare(&AtomicBool::new(false))
            .unwrap();
        assert!(output.effects.request_sequence(1));
        assert!(output.effects.install_counted(bank).is_ok());
        mock.render(0, &mut [0.0f32; 7]);
        assert_eq!(control.snapshot().position_seconds, 0.0);
        (mock, output, control, pool)
    }

    #[test]
    fn counted_impulse_format_switch_replays_old_media_without_source_pull_and_drains_tail() {
        let (mock, mut output, control, pool) = counted_impulse_fixture();
        mock.drop_gate.block();
        output
            .select(OutputSelection::Device {
                id: device_id("endpoint-b"),
            })
            .unwrap();
        until(|| {
            mock.render(0, &mut [0.0f32; 1]);
            mock.drop_gate.entered.load(Ordering::Acquire)
        });
        let consumed = control.snapshot().consumed_frames;
        mock.drop_gate.release();
        ready(&output, &mock, "endpoint-b");
        let current = mock.latest().unwrap();
        assert_eq!(control.snapshot().consumed_frames, consumed);
        let mut sample = [0.0f32; 2];
        mock.render(current, &mut sample);
        assert_eq!(
            control.snapshot().consumed_frames,
            consumed,
            "Retained DSP PCM must not re-read source"
        );
        assert!(control.snapshot().position_seconds > 0.0);
        assert!(sample.iter().any(|value| *value != 0.0));
        let mut calls = 0;
        while !output.ended() {
            mock.render(current, &mut sample);
            calls += 1;
            assert!(calls < 10000, "Effects media/tail failed to drain");
        }
        assert_eq!(control.snapshot().consumed_frames, 128);
        assert!((control.snapshot().position_seconds - 128.0 / 8000.0).abs() < 1e-10);
        assert_eq!(pool.occupied_banks(), 1);
        assert!(output.shutdown(Duration::from_secs(1)));
        assert_eq!(pool.occupied_banks(), 0);
    }

    fn waiting_handoff(mock: &MockControl, output: &Output, index: usize) -> bool {
        if mock.latest() != Some(index) || !output.shared.switching.load(Ordering::Acquire) {
            return false;
        }
        let callbacks = mock.callbacks.lock().unwrap();
        callbacks[index].kernel.as_ref().is_some_and(|kernel| {
            kernel
                .handoff
                .as_ref()
                .is_some_and(|handoff| !handoff.is_empty())
        })
    }

    fn drain_counted_fixture(mock: &MockControl, output: &Output, control: &PcmControl) {
        let generation = output.shared.stream_generation.load(Ordering::Acquire);
        let current = mock
            .callbacks
            .lock()
            .unwrap()
            .iter()
            .position(|callback| callback.generation == generation && callback.kernel.is_some())
            .expect("Actual published callback remains active");
        let mut samples = [0.0f32; 2];
        for _ in 0..10000 {
            if output.ended() {
                break;
            }
            let before = control.snapshot().position_seconds;
            mock.render(current, &mut samples);
            assert!(control.snapshot().position_seconds >= before);
        }
        assert!(output.ended());
        assert_eq!(control.snapshot().consumed_frames, 128);
        assert!((control.snapshot().position_seconds - 128.0 / 8000.0).abs() < 1e-10);
    }

    #[test]
    fn canceled_counted_handoff_retargets_original_fifo_to_third_format() {
        let (mock, mut output, control, pool) = counted_impulse_fixture();
        mock.topology.lock().unwrap().devices.push(Endpoint {
            id: "endpoint-c",
            name: "C",
            format: PcmFormat {
                sample_rate: 24000,
                channels: 1,
            },
        });
        control.set_paused(true);
        output
            .select(OutputSelection::Device {
                id: device_id("endpoint-b"),
            })
            .unwrap();
        until(|| {
            mock.render(0, &mut [0.0f32; 1]);
            waiting_handoff(&mock, &output, 1)
        });
        let consumed = control.snapshot().consumed_frames;
        let position = control.snapshot().position_seconds;
        mock.defer_drop.store(true, Ordering::Release);
        output
            .select(OutputSelection::Device {
                id: device_id("endpoint-c"),
            })
            .unwrap();
        let mut late = [9.0f32; 2];
        mock.render(1, &mut late);
        assert_eq!(late, [0.0; 2]);
        assert_eq!(control.snapshot().position_seconds, position);
        mock.close_callback(1);
        mock.defer_drop.store(false, Ordering::Release);
        ready(&output, &mock, "endpoint-c");
        assert_eq!(control.snapshot().consumed_frames, consumed);
        let mut sample = [0.0f32; 1];
        mock.render(mock.latest().unwrap(), &mut sample);
        assert_eq!(sample, [0.0]);
        assert_eq!(control.snapshot().position_seconds, position);
        control.set_paused(false);
        mock.render(mock.latest().unwrap(), &mut sample);
        assert_eq!(control.snapshot().consumed_frames, consumed);
        assert!(sample[0] != 0.0);
        assert!(control.snapshot().position_seconds > position);
        assert_eq!(pool.occupied_banks(), 1);
        drain_counted_fixture(&mock, &output, &control);
        assert!(output.shutdown(Duration::from_secs(1)));
        assert_eq!(pool.occupied_banks(), 0);
    }

    #[test]
    fn faulted_counted_bank_recovers_to_new_format_with_queued_candidate_retired() {
        let (mock, mut output, control, pool) = counted_impulse_fixture();
        let old_id = output.effects.active_owner();
        let record = output.effects.record_for_owner(old_id).unwrap();
        let plan = record.bounds().preparation();
        let queued = pool
            .begin(
                record.recipe().clone(),
                BankRequest {
                    format: plan.format,
                    device_channels: plan.device_channels,
                    token: plan.token,
                    source_epoch: plan.source_epoch,
                    replay_capacity: plan.replay_capacity,
                    initial_spatial: Some(plan.initial_spatial),
                },
            )
            .unwrap()
            .prepare(&AtomicBool::new(false))
            .unwrap();
        let queued_id = queued.record().owner_id();
        assert!(output.effects.install_counted(queued).is_ok());
        assert_eq!(pool.occupied_banks(), 2);
        let consumed = control.snapshot().consumed_frames;
        mock.topology.lock().unwrap().default = "endpoint-b";
        mock.error(0, cpal::StreamError::StreamInvalidated);
        ready(&output, &mock, "endpoint-b");
        assert_eq!(control.snapshot().consumed_frames, consumed);
        assert_eq!(pool.occupied_banks(), 1);
        assert!(output.effects.record_for_owner(old_id).is_none());
        assert!(output.effects.record_for_owner(queued_id).is_none());
        let mut samples = [0.0f32; 2];
        mock.render(mock.latest().unwrap(), &mut samples);
        assert_eq!(control.snapshot().consumed_frames, consumed);
        assert!(samples.iter().any(|value| *value != 0.0));
        drain_counted_fixture(&mock, &output, &control);
        assert!(output.shutdown(Duration::from_secs(1)));
        assert_eq!(pool.occupied_banks(), 0);
    }

    enum HardInvalidation {
        Stop,
        Fail,
        Cancel,
    }

    fn invalidate_canceled_counted_handoff(kind: HardInvalidation) {
        let (mock, mut output, control, pool) = counted_impulse_fixture();
        let old_id = output.effects.active_owner();
        let old_record = output.effects.record_for_owner(old_id).unwrap();
        let token = output.token();
        output
            .select(OutputSelection::Device {
                id: device_id("endpoint-b"),
            })
            .unwrap();
        until(|| {
            mock.render(0, &mut [0.0f32; 1]);
            waiting_handoff(&mock, &output, 1)
        });
        let position = control.snapshot().position_seconds;
        mock.defer_drop.store(true, Ordering::Release);
        let expected_token = match kind {
            HardInvalidation::Stop => {
                output.clear();
                0
            }
            HardInvalidation::Fail => {
                control.set_paused(true);
                control.fail();
                token
            }
            HardInvalidation::Cancel => {
                control.set_paused(true);
                control.cancel();
                token
            }
        };
        output.select(OutputSelection::Default).unwrap();
        let mut late = [9.0f32; 2];
        mock.render(1, &mut late);
        assert_eq!(late, [0.0; 2]);
        mock.close_callback(1);
        mock.defer_drop.store(false, Ordering::Release);
        ready(&output, &mock, "endpoint-a");
        assert_eq!(output.token(), expected_token);
        assert_eq!(pool.occupied_banks(), 0);
        assert!(output.effects.record_for_owner(old_id).is_none());
        assert_eq!(control.snapshot().position_seconds, position);
        // Both real registry slots must be reusable after actual hard invalidation.
        for _ in 0..2 {
            let plan = old_record.bounds().preparation();
            let bank = pool
                .begin(
                    old_record.recipe().clone(),
                    BankRequest {
                        format: plan.format,
                        device_channels: plan.device_channels,
                        token: plan.token,
                        source_epoch: plan.source_epoch,
                        replay_capacity: plan.replay_capacity,
                        initial_spatial: Some(plan.initial_spatial),
                    },
                )
                .unwrap()
                .prepare(&AtomicBool::new(false))
                .unwrap();
            assert!(output.effects.install_counted(bank).is_ok());
        }
        assert!(output.shutdown(Duration::from_secs(1)));
        assert_eq!(pool.occupied_banks(), 0);
    }

    #[test]
    fn stopped_canceled_counted_handoff_retires_real_banks_and_registry() {
        invalidate_canceled_counted_handoff(HardInvalidation::Stop);
    }

    #[test]
    fn paused_same_epoch_source_failure_retires_canceled_counted_handoff() {
        invalidate_canceled_counted_handoff(HardInvalidation::Fail);
    }

    #[test]
    fn paused_same_epoch_source_cancel_retires_canceled_counted_handoff() {
        invalidate_canceled_counted_handoff(HardInvalidation::Cancel);
    }

    #[test]
    fn second_live_switch_keeps_partially_presented_handoff_on_existing_endpoint() {
        let (mock, mut output, control, pool) = counted_impulse_fixture();
        output
            .select(OutputSelection::Device {
                id: device_id("endpoint-b"),
            })
            .unwrap();
        until(|| {
            mock.render(0, &mut []);
            waiting_handoff(&mock, &output, 1)
        });
        ready(&output, &mock, "endpoint-b");
        let mut samples = [0.0f32; 2];
        mock.render(1, &mut samples);
        let position = control.snapshot().position_seconds;
        assert!(position > 0.0);
        output.select(OutputSelection::Default).unwrap();
        until(|| {
            mock.render(1, &mut []);
            output.snapshot().state.last_error == Some(AudioOutputError::ConfigurationUnavailable)
        });
        assert!(output.is_ready());
        assert_eq!(
            output.snapshot().state.active_device_id,
            Some(device_id("endpoint-b"))
        );
        assert!(mock.callbacks.lock().unwrap()[1].kernel.is_some());
        assert!(!output.shared.switching.load(Ordering::Acquire));
        assert_eq!(control.snapshot().position_seconds, position);
        assert!(mock.callbacks.lock().unwrap()[2].kernel.is_none());
        drain_counted_fixture(&mock, &output, &control);
        assert_eq!(pool.occupied_banks(), 1);
        assert!(output.shutdown(Duration::from_secs(1)));
    }

    fn live_effects_fixture() -> (Arc<MockControl>, Output, PcmProducer, PcmControl) {
        let mock = MockControl::new();
        let format = PcmFormat {
            sample_rate: 8_000,
            channels: 2,
        };
        mock.topology.lock().unwrap().devices[0].format = format;
        let mut output = output(&mock, AudioPreferences::default());
        ready(&output, &mock, "endpoint-a");
        let (mut producer, source, control) = PcmProducer::new(format, 12_000, 0, None).unwrap();
        for index in 0..12_000 {
            assert!(producer.push_frame(index, &[0.04, 0.04]));
        }
        let mut builder =
            crate::audio::convolution::ImpulseBuilder::prepare(8_000, 1, 2_048).unwrap();
        for index in 0..2_048 {
            builder
                .push_frame(&[if index == 0 {
                    1.0
                } else if index == 2_047 {
                    0.25
                } else {
                    0.0
                }])
                .unwrap();
        }
        let id = crate::domain::audio_effects::ImpulseResourceId::Imported(
            "live-native-response".into(),
        );
        let impulse = crate::audio::effects_factory::OwnedImpulse::from_validated(
            builder.finish().unwrap(),
            id.clone(),
            "d".repeat(64),
        )
        .unwrap();
        let mut settings = crate::domain::audio_effects::EffectsSettings::default();
        settings.convolution.impulse = Some(id);
        settings.convolution.main_gain = 0.0;
        settings.convolution.send_gain = 1.0;
        output
            .configure_effects(settings.clone(), 1, Some(impulse.clone()))
            .unwrap();
        output.attach(source).unwrap();
        output
            .configure_effects(settings, 1, Some(impulse))
            .unwrap();
        mock.render(0, &mut [0.0f32; 2 * 1_025]);
        assert_eq!(output.effects_snapshot().applied_sequence, 1);
        (mock, output, producer, control)
    }

    #[test]
    fn live_reset_reuses_actual_native_session_and_third_setting_preserves_media_scope() {
        let (mock, mut output, _producer, control) = live_effects_fixture();
        let token = output.token();
        let epoch = control.current_epoch();
        let generation = output.shared.stream_generation.load(Ordering::Acquire);
        let opens = mock.opens.load(Ordering::Acquire);
        output
            .configure_effects(Default::default(), 2, None)
            .unwrap();
        let mut third = crate::domain::audio_effects::EffectsSettings::default();
        third.convolution.main_gain = 0.5;
        // Request the newest desire while the previous actual owner is retained.
        output.configure_effects(third.clone(), 3, None).unwrap();
        until(|| {
            mock.render(0, &mut [0.0f32; 128]);
            let _ = output.configure_effects(third.clone(), 3, None);
            output.effects_snapshot().applied_sequence == 3
        });
        assert_eq!(output.token(), token);
        assert_eq!(control.current_epoch(), epoch);
        assert_eq!(
            output.shared.stream_generation.load(Ordering::Acquire),
            generation
        );
        assert_eq!(mock.opens.load(Ordering::Acquire), opens);
        assert_eq!(mock.latest(), Some(0));
        assert!(!output.ended());
        assert!(control.snapshot().position_seconds > 0.0);
        assert!(output.effects.owned_banks() <= 2);
        assert!(output.shutdown(Duration::from_secs(1)));
        assert_eq!(output.effects.owned_banks(), 0);
    }

    #[test]
    fn paused_live_native_handback_keeps_prefix_and_clock_until_actual_resume() {
        let (mock, mut output, _producer, control) = live_effects_fixture();
        output.set_paused(true);
        let before = control.snapshot();
        let generation = output.shared.stream_generation.load(Ordering::Acquire);
        output
            .configure_effects(Default::default(), 2, None)
            .unwrap();
        until(|| {
            mock.render(0, &mut [0.0f32; 128]);
            !output.shared.live_effects.occupied()
        });
        let current = control.snapshot();
        assert_eq!(current.consumed_frames, before.consumed_frames);
        assert_eq!(current.queued_frames, before.queued_frames);
        assert_eq!(current.position_seconds, before.position_seconds);
        assert_eq!(output.effects_snapshot().applied_sequence, 1);
        output.set_paused(false);
        until(|| {
            mock.render(0, &mut [0.0f32; 128]);
            output.effects_snapshot().applied_sequence == 2
        });
        assert_eq!(
            output.shared.stream_generation.load(Ordering::Acquire),
            generation
        );
        assert_eq!(mock.latest(), Some(0));
        assert!(output.shutdown(Duration::from_secs(1)));
        assert_eq!(output.effects.owned_banks(), 0);
    }

    #[test]
    fn seek_during_live_native_pin_retires_stale_candidate_and_keeps_same_session() {
        let (mock, mut output, mut producer, control) = live_effects_fixture();
        let generation = output.shared.stream_generation.load(Ordering::Acquire);
        output
            .configure_effects(Default::default(), 2, None)
            .unwrap();
        producer.begin_epoch().unwrap();
        for index in 50_000..50_128 {
            assert!(producer.push_frame(index, &[0.03, 0.03]));
        }
        producer.commit_epoch(50_000).unwrap();
        producer.finish();
        let before = control.snapshot();
        until(|| {
            mock.render(0, &mut []);
            let _ = output.configure_effects(Default::default(), 2, None);
            !output.shared.live_effects.occupied() && output.effects.owned_banks() == 1
        });
        assert_eq!(control.snapshot().consumed_frames, before.consumed_frames);
        assert_eq!(control.snapshot().position_seconds, before.position_seconds);
        until(|| {
            mock.render(0, &mut [0.0f32; 128]);
            output.effects_snapshot().applied_sequence == 2
        });
        assert_eq!(
            output.shared.stream_generation.load(Ordering::Acquire),
            generation
        );
        assert_eq!(mock.latest(), Some(0));
        assert!(control.snapshot().position_seconds >= 50_000.0 / 8_000.0);
        assert!(output.shutdown(Duration::from_secs(1)));
        assert_eq!(output.effects.owned_banks(), 0);
    }

    #[test]
    fn stop_and_shutdown_with_live_native_request_retire_all_actual_owners() {
        let (mock, mut output, _producer, control) = live_effects_fixture();
        output
            .configure_effects(Default::default(), 2, None)
            .unwrap();
        output.clear();
        let before = control.snapshot();
        mock.render(0, &mut [0.0f32; 128]);
        assert_eq!(control.snapshot().consumed_frames, before.consumed_frames);
        assert_eq!(control.snapshot().position_seconds, before.position_seconds);
        assert!(output.shutdown(Duration::from_secs(1)));
        assert_eq!(output.effects.owned_banks(), 0);
        assert!(!output.shared.live_effects.occupied());
    }

    include!("device_owner/recovery_tests.rs");

    #[test]
    fn faulted_partial_prefix_resumes_original_progress_at_another_device_rate() {
        let (mock, mut output, control, pool) = counted_impulse_fixture();
        output
            .select(OutputSelection::Device {
                id: device_id("endpoint-b"),
            })
            .unwrap();
        until(|| {
            mock.render(0, &mut []);
            waiting_handoff(&mock, &output, 1)
        });
        ready(&output, &mock, "endpoint-b");
        let consumed = control.snapshot().consumed_frames;
        mock.render(1, &mut [0.0f32; 2]);
        let position = control.snapshot().position_seconds;
        assert!(position > 0.0);
        assert_eq!(control.snapshot().consumed_frames, consumed);
        mock.topology.lock().unwrap().devices[1].format = PcmFormat {
            sample_rate: 24000,
            channels: 1,
        };
        mock.error(1, cpal::StreamError::StreamInvalidated);
        until(|| mock.latest() == Some(2));
        ready(&output, &mock, "endpoint-b");
        assert_eq!(control.snapshot().position_seconds, position);
        assert_eq!(control.snapshot().consumed_frames, consumed);
        mock.render(2, &mut [0.0f32; 1]);
        assert_eq!(control.snapshot().consumed_frames, consumed);
        let expected = position + 1.0 / 24000.0;
        assert!((control.snapshot().position_seconds - expected).abs() < 1e-10);
        assert_eq!(pool.occupied_banks(), 1);
        drain_counted_fixture(&mock, &output, &control);
        assert!(output.shutdown(Duration::from_secs(1)));
        assert_eq!(pool.occupied_banks(), 0);
    }
}

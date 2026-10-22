//! Asynchronous HTTP audio loading and real OS-device playback.
//!
//! The caller must authorize a URL before `load`; no user scripts run here.
//! UI methods only enqueue commands and read small mutex-protected snapshots.

pub mod analyser;
mod analysis_tap;
pub mod bundled_impulses;
pub mod cache;
pub mod compressor;
pub mod convolution;
pub mod decode;
pub mod dsp;
pub mod effect_admission;
pub mod effect_exchange;
pub mod effect_mix;
pub mod effect_pin;
pub mod effects;
pub mod effects_factory;
pub mod effects_handoff;
pub(crate) mod http_client;
pub mod impulse_decode;
pub mod impulse_resource;
pub mod media_source;
pub mod output;
pub mod pcm;
pub mod pitch;
pub mod prepared;
pub mod processing;
pub mod spatial;
pub mod supply;
pub mod transport;
pub(crate) mod worker_owner;

#[cfg(test)]
use crate::catalog::MAX_AUDIO_BYTES;
use anyhow::{Context, Result, bail};
#[cfg(test)]
use reqwest::Url;
use serde::Serialize;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

const CONTROL_CAPACITY: usize = 64;
const MAX_LOAD_TASKS: usize = 4;
use crate::domain::equalizer::{EqualizerSettings, EqualizerSnapshot};
use crate::domain::playback_processing::{PlaybackProcessingSettings, PlaybackProcessingSnapshot};
use crate::playback::preload::{CancelFlag, LeaseOwnership};
use cache::{CacheIoErrorCode, CacheStats, CacheStore};
use decode::{
    DecodeError, DecodeErrorKind, DecoderConfig, DecoderHandle, DecoderReady, DecoderStatus,
};
use output::{Output, OutputError};
pub use prepared::{
    AudioPreloadError, AudioPreloadErrorKind, AudioPreloadSnapshot, AudioPreloadState,
    AudioPreloadToken, AudioPrepareLimits, AudioPrepareTicket,
};
use prepared::{Permit, PreparedStream, Promotion, SharedView, View};
use transport::{
    ApprovedAudioRequest, AudioTransport, CacheIdentity, MediaIoError, MediaIoErrorKind,
    StreamControl, TransportOptions,
};
use worker_owner::{TrackedWorker, WorkerKind, WorkerPermit};

/// Selects audible OS output or the same decoder and render kernel in memory.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize)]
pub enum AudioOutputMode {
    #[default]
    Device,
    Memory,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AudioShutdownError {
    EngineTimeout,
    CacheTimeout,
    WorkerPanicked,
}

impl AudioShutdownError {
    pub fn is_timeout(self) -> bool {
        matches!(self, Self::EngineTimeout | Self::CacheTimeout)
    }
}

impl std::fmt::Display for AudioShutdownError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::EngineTimeout => "音频关闭超时；等待中的线程仍由引擎管理",
            Self::CacheTimeout => "音频缓存关闭超时；I/O线程仍由拥有者管理",
            Self::WorkerPanicked => "音频线程异常退出",
        })
    }
}

impl std::error::Error for AudioShutdownError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub enum PlaybackStatus {
    Idle,
    Loading,
    Buffering,
    Playing,
    Paused,
    Ended,
    Error,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PlaybackFailureStage {
    Transport,
    Decode,
    Output,
}

/// Structured diagnostics remain correlated with the approved media request.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct PlaybackFailure {
    pub stage: PlaybackFailureStage,
    pub decode_kind: Option<DecodeErrorKind>,
    pub media: Option<MediaIoError>,
    pub output_kind: Option<OutputError>,
    pub request_url_sha256: Option<String>,
}

impl PlaybackFailure {
    pub fn requires_url_refresh(&self) -> bool {
        self.stage == PlaybackFailureStage::Transport
            && self.media.as_ref().is_some_and(|error| {
                error.kind == MediaIoErrorKind::Network
                    && matches!(error.http_status, Some(401 | 403 | 404 | 410))
            })
    }

    fn decode(error: &DecodeError, request_url_sha256: &str) -> Self {
        Self {
            stage: if error.media.is_some() {
                PlaybackFailureStage::Transport
            } else {
                PlaybackFailureStage::Decode
            },
            decode_kind: Some(error.kind),
            media: error.media.clone(),
            output_kind: None,
            request_url_sha256: Some(request_url_sha256.into()),
        }
    }

    fn transport(error: &anyhow::Error, request_url_sha256: Option<String>) -> Self {
        Self {
            stage: PlaybackFailureStage::Transport,
            decode_kind: None,
            media: error.downcast_ref::<MediaIoError>().cloned(),
            output_kind: None,
            request_url_sha256,
        }
    }

    fn stream_open(error: &anyhow::Error, cache: &CacheStore, request_url_sha256: String) -> Self {
        let mut failure = Self::transport(error, Some(request_url_sha256));
        let cache_state = cache.stats();
        if failure.media.is_none()
            && !cache_state.initialized
            && cache_state.last_io_error == CacheIoErrorCode::Initialization
        {
            failure.media = Some(MediaIoError::new(
                MediaIoErrorKind::Storage,
                &error.to_string(),
            ));
        }
        failure
    }

    fn output(error: Option<OutputError>, request_url_sha256: Option<String>) -> Self {
        Self {
            stage: PlaybackFailureStage::Output,
            decode_kind: None,
            media: None,
            output_kind: error,
            request_url_sha256,
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct PlaybackSnapshot {
    pub has_presented_media: bool,
    pub generation: u64,
    pub rate: f64,
    pub seek_seq: u64,
    pub status: PlaybackStatus,
    pub track_id: Option<String>,
    pub position_seconds: f64,
    pub duration_seconds: f64,
    pub duration_known: bool,
    pub buffering: bool,
    pub seek_pending: bool,
    pub volume: f32,
    pub error: Option<String>,
    pub failure: Option<PlaybackFailure>,
}

impl Default for PlaybackSnapshot {
    fn default() -> Self {
        Self {
            has_presented_media: false,
            generation: 0,
            rate: 1.0,
            seek_seq: 0,
            status: PlaybackStatus::Idle,
            track_id: None,
            position_seconds: 0.0,
            duration_seconds: 0.0,
            duration_known: false,
            buffering: false,
            seek_pending: false,
            volume: 0.6,
            error: None,
            failure: None,
        }
    }
}

enum Command {
    Prepare {
        operation_id: String,
        request: Box<ApprovedAudioRequest>,
        expected_generation: u64,
        cancel: CancelFlag,
        limits: AudioPrepareLimits,
        reply: mpsc::SyncSender<std::result::Result<AudioPreloadToken, AudioPreloadError>>,
    },
    CancelPreload(String),
    Promote {
        token: AudioPreloadToken,
        expected_generation: u64,
        autoplay: bool,
    },
    Load {
        request: Box<ApprovedAudioRequest>,
        autoplay: bool,
    },
    InvalidUrl(String),
    #[cfg(test)]
    DeviceError {
        device_token: u64,
        error: String,
    },
    Play,
    Pause,
    Toggle,
    Seek(f64),
    Volume(f32),
    Stop,
    ClearError,
}

struct ControlCommand {
    command: Command,
    intent_revision: u64,
}

#[derive(Default)]
struct EqualizerControl {
    snapshot: EqualizerSnapshot,
    pending: Option<(EqualizerSettings, u64)>,
}

fn project_equalizer(control: &Mutex<EqualizerControl>, mut current: EqualizerSnapshot) {
    let mut state = control.lock().unwrap_or_else(|error| error.into_inner());
    current.desired = state.snapshot.desired;
    current.requested_sequence = state.snapshot.requested_sequence;
    state.snapshot = current;
}

#[derive(Default)]
struct ProcessingControl {
    snapshot: PlaybackProcessingSnapshot,
    pending: Option<(PlaybackProcessingSettings, u64)>,
}

fn project_processing(control: &Mutex<ProcessingControl>, mut current: PlaybackProcessingSnapshot) {
    let mut state = control.lock().unwrap_or_else(|error| error.into_inner());
    current.desired = state.snapshot.desired;
    current.requested_sequence = state.snapshot.requested_sequence;
    state.snapshot = current;
}

type SharedAnalysisPublication = Arc<Mutex<Option<(u64, u64, u64, u64, analyser::AnalysisFrame)>>>;

#[derive(Default)]
struct AudioEffectsControl {
    snapshot: crate::domain::audio_effects::EffectsSnapshot,
    spatial_applied_sequence: u64,
    impulse: Option<Arc<effects_factory::OwnedImpulse>>,
}
impl AudioEffectsControl {
    fn matches_resource(&self, impulse: Option<&Arc<effects_factory::OwnedImpulse>>) -> bool {
        match (self.impulse.as_ref(), impulse) {
            (None, None) => true,
            (Some(previous), Some(next)) => Arc::ptr_eq(previous, next),
            _ => false,
        }
    }
}
struct AudioHandle {
    spatial: Arc<Mutex<AudioEffectsControl>>,
    analysis: SharedAnalysisPublication,
    analysis_configuration: Arc<Mutex<(bool, u64)>>,
    commands: mpsc::SyncSender<ControlCommand>,
    control_admission: Mutex<()>,
    intent_revision: Arc<AtomicU64>,
    device_pause_revision: Arc<AtomicU64>,
    snapshot: Arc<Mutex<PlaybackSnapshot>>,
    output_snapshot: Arc<Mutex<crate::domain::audio_output::AudioOutputSnapshot>>,
    output_render_timing: Arc<Mutex<Option<output::OutputRenderTiming>>>,
    supply: Arc<Mutex<Option<supply::AudioSupplySnapshot>>>,
    pending_preferences: Arc<Mutex<Option<crate::domain::audio_output::AudioPreferences>>>,
    refresh_devices: Arc<AtomicBool>,
    output_failed: Arc<AtomicBool>,
    equalizer: Arc<Mutex<EqualizerControl>>,
    processing: Arc<Mutex<ProcessingControl>>,
    observation: Option<Arc<Mutex<RenderedPcm>>>,
    cache: Arc<Mutex<Option<Arc<CacheStore>>>>,
    preload: SharedView,
    shutdown_requested: Arc<AtomicBool>,
    thread: Option<TrackedWorker>,
}

impl Drop for AudioHandle {
    fn drop(&mut self) {
        self.shutdown_requested.store(true, Ordering::Release);
        if let Some(thread) = &self.thread {
            thread.unpark();
        }
    }
}

#[derive(Clone)]
pub struct AudioPlayer {
    inner: Arc<AudioHandle>,
}

impl Default for AudioPlayer {
    fn default() -> Self {
        Self::new()
    }
}

impl AudioPlayer {
    pub fn new() -> Self {
        Self::with_output(OutputMode::Device)
    }

    pub fn with_output_mode(mode: AudioOutputMode) -> Self {
        match mode {
            AudioOutputMode::Device => Self::new(),
            AudioOutputMode::Memory => Self::memory(),
        }
    }

    pub fn memory() -> Self {
        Self::with_output(OutputMode::Memory(Arc::new(Mutex::new(
            RenderedPcm::default(),
        ))))
    }

    pub fn with_transport_options(mode: AudioOutputMode, options: TransportOptions) -> Self {
        let output = match mode {
            AudioOutputMode::Device => OutputMode::Device,
            AudioOutputMode::Memory => {
                OutputMode::Memory(Arc::new(Mutex::new(RenderedPcm::default())))
            }
        };
        Self::with_output_options(output, options)
    }
    fn with_output(output_mode: OutputMode) -> Self {
        Self::with_output_options(output_mode, TransportOptions::default())
    }
    fn with_output_options(output_mode: OutputMode, options: TransportOptions) -> Self {
        let cache = Arc::new(Mutex::new(None));
        let engine_cache = cache.clone();
        let preload = Arc::new(Mutex::new(View::default()));
        let engine_preload = preload.clone();
        let (commands, receiver) = mpsc::sync_channel(CONTROL_CAPACITY);
        let intent_revision = Arc::new(AtomicU64::new(0));
        let device_pause_revision = Arc::new(AtomicU64::new(0));
        let worker_device_pause = device_pause_revision.clone();
        let snapshot = Arc::new(Mutex::new(PlaybackSnapshot::default()));
        let worker_snapshot = snapshot.clone();
        let mut initial_output = crate::domain::audio_output::AudioOutputSnapshot::default();
        if matches!(output_mode, OutputMode::Memory(_)) {
            initial_output.state.mode = crate::domain::audio_output::AudioOutputMode::Memory;
            initial_output.state.status =
                crate::domain::audio_output::AudioOutputStatus::NotApplicable;
            initial_output.state.format = Some(crate::domain::audio_output::AudioOutputFormat {
                sample_rate: 22_050,
                channels: 1,
            });
        }
        let output_snapshot = Arc::new(Mutex::new(initial_output));
        let worker_output_snapshot = output_snapshot.clone();
        let output_render_timing = Arc::new(Mutex::new(None));
        let worker_output_render_timing = output_render_timing.clone();
        let supply = Arc::new(Mutex::new(None));
        let worker_supply = supply.clone();
        let pending_preferences = Arc::new(Mutex::new(None));
        let worker_preferences = pending_preferences.clone();
        let refresh_devices = Arc::new(AtomicBool::new(false));
        let worker_refresh = refresh_devices.clone();
        let output_failed = Arc::new(AtomicBool::new(false));
        let worker_output_failed = output_failed.clone();
        let equalizer = Arc::new(Mutex::new(EqualizerControl::default()));
        let worker_equalizer = equalizer.clone();
        let processing = Arc::new(Mutex::new(ProcessingControl::default()));
        let worker_processing = processing.clone();
        let analysis = Arc::new(Mutex::new(None));
        let worker_analysis = analysis.clone();
        let analysis_configuration = Arc::new(Mutex::new((false, 1)));
        let worker_analysis_configuration = analysis_configuration.clone();
        let spatial = Arc::new(Mutex::new(AudioEffectsControl::default()));
        let worker_spatial = spatial.clone();
        let shutdown_requested = Arc::new(AtomicBool::new(false));
        let worker_shutdown = shutdown_requested.clone();
        let observation = match &output_mode {
            OutputMode::Device => None,
            OutputMode::Memory(observation) => Some(observation.clone()),
        };
        let engine_thread = (|| -> Result<TrackedWorker> {
            let permit = WorkerPermit::reserve(WorkerKind::Engine)?;
            let thread = thread::Builder::new()
                .name("lx-audio-engine".into())
                .spawn(move || {
                    run_engine(
                        receiver,
                        worker_snapshot,
                        output_mode,
                        worker_shutdown,
                        options,
                        EngineShared {
                            spatial: worker_spatial,
                            analysis_configuration: worker_analysis_configuration,
                            analysis: worker_analysis,
                            shared_cache: engine_cache,
                            preload_view: engine_preload,
                            output_snapshot: worker_output_snapshot,
                            output_render_timing: worker_output_render_timing,
                            supply: worker_supply,
                            pending_preferences: worker_preferences,
                            refresh_devices: worker_refresh,
                            output_failed: worker_output_failed,
                            device_pause_revision: worker_device_pause,
                            equalizer: worker_equalizer,
                            processing: worker_processing,
                        },
                    );
                })?;
            Ok(permit.attach(thread))
        })();
        let engine_thread = match engine_thread {
            Ok(thread) => Some(thread),
            Err(error) => {
                output_failed.store(true, Ordering::Release);
                let mut output = output_snapshot
                    .lock()
                    .unwrap_or_else(|error| error.into_inner());
                output.state.status = crate::domain::audio_output::AudioOutputStatus::Unavailable;
                output.state.last_error =
                    Some(crate::domain::audio_output::AudioOutputError::WorkerUnavailable);
                let mut state = snapshot.lock().unwrap();
                state.status = PlaybackStatus::Error;
                state.error = Some(format!("无法启动音频线程：{error}"));
                state.failure = Some(PlaybackFailure::transport(&error, None));
                None
            }
        };
        Self {
            inner: Arc::new(AudioHandle {
                spatial,
                analysis_configuration,
                analysis,
                commands,
                control_admission: Mutex::new(()),
                intent_revision,
                device_pause_revision,
                snapshot,
                output_snapshot,
                output_render_timing,
                supply,
                pending_preferences,
                refresh_devices,
                output_failed,
                equalizer,
                processing,
                observation,
                cache,
                preload,
                shutdown_requested,
                thread: engine_thread,
            }),
        }
    }

    pub fn load(&self, track_id: impl Into<String>, url: impl Into<String>, autoplay: bool) {
        let identity = CacheIdentity {
            track_id: track_id.into(),
            ..Default::default()
        };
        match ApprovedAudioRequest::new(&url.into(), identity) {
            Ok(request) => self.load_approved(request, autoplay),
            Err(error) => self.send(Command::InvalidUrl(error.to_string())),
        }
    }
    pub fn load_approved(&self, request: ApprovedAudioRequest, autoplay: bool) {
        self.send(Command::Load {
            request: Box::new(request),
            autoplay,
        });
    }
    pub fn cache_stats(&self) -> Option<CacheStats> {
        self.inner
            .cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map(|cache| cache.stats())
    }
    pub fn prepare_approved(
        &self,
        operation_id: String,
        request: ApprovedAudioRequest,
        expected_generation: u64,
        cancel: CancelFlag,
    ) -> Result<AudioPrepareTicket> {
        self.prepare_approved_with_limits(
            operation_id,
            request,
            expected_generation,
            cancel,
            AudioPrepareLimits::default(),
        )
    }
    pub fn prepare_approved_with_limits(
        &self,
        operation_id: String,
        request: ApprovedAudioRequest,
        expected_generation: u64,
        cancel: CancelFlag,
        limits: AudioPrepareLimits,
    ) -> Result<AudioPrepareTicket> {
        if operation_id.is_empty()
            || operation_id.len() > 64
            || operation_id.chars().any(char::is_control)
            || !limits.validate()
        {
            bail!("Invalid audio preparation operation or limits");
        }
        let (reply, ticket) = AudioPrepareTicket::new(cancel.clone());
        self.enqueue_preload(Command::Prepare {
            operation_id,
            request: Box::new(request),
            expected_generation,
            cancel,
            limits,
            reply,
        })?;
        Ok(ticket)
    }
    pub fn preload_snapshot(&self, operation_id: &str) -> Option<AudioPreloadSnapshot> {
        self.inner
            .preload
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .snapshot
            .as_ref()
            .filter(|snapshot| snapshot.operation_id == operation_id)
            .cloned()
    }
    pub fn cancel_preload(&self, operation_id: &str) {
        if operation_id.is_empty() || operation_id.len() > 64 {
            return;
        }
        let cancel = {
            let view = self
                .inner
                .preload
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            view.snapshot
                .as_ref()
                .filter(|snapshot| snapshot.operation_id == operation_id)
                .and(view.cancel.as_ref())
                .cloned()
        };
        if let Some(cancel) = cancel {
            cancel.cancel_speculative();
        }
        let _ = self.enqueue_preload(Command::CancelPreload(operation_id.into()));
    }
    pub fn promote_prepared(
        &self,
        token: AudioPreloadToken,
        expected_generation: u64,
        autoplay: bool,
    ) -> Result<()> {
        if token.is_cancelled() {
            bail!("Prepared audio lease was cancelled");
        }
        let current = self.snapshot();
        if current.generation != expected_generation
            || current.status != PlaybackStatus::Ended
            || token.permit.expected_generation != expected_generation
        {
            bail!("Prepared audio foreground fence is stale");
        }
        self.enqueue_preload(Command::Promote {
            token,
            expected_generation,
            autoplay,
        })
    }
    fn enqueue_preload(&self, command: Command) -> Result<()> {
        if self.inner.shutdown_requested.load(Ordering::Acquire) {
            bail!("Audio engine is closed");
        }
        let _admission = self
            .inner
            .control_admission
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let command = self.stamp_control(command)?;
        self.inner
            .commands
            .try_send(command)
            .map_err(|error| match error {
                mpsc::TrySendError::Full(_) => anyhow::anyhow!("Audio control queue is full"),
                mpsc::TrySendError::Disconnected(_) => anyhow::anyhow!("Audio engine is closed"),
            })
    }
    /// Filesystem work; call from an independent task, never an output callback.
    pub fn clear_cache(&self) -> Result<CacheStats> {
        if let Some(cancel) = self
            .inner
            .preload
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .cancel
            .clone()
        {
            cancel.cancel_speculative();
        }
        let cache = self
            .inner
            .cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .context("Audio cache is not ready")?;
        cache.clear()
    }

    pub fn play(&self) {
        self.send(Command::Play);
    }

    pub fn pause(&self) {
        self.send(Command::Pause);
    }

    pub fn toggle(&self) {
        self.send(Command::Toggle);
    }

    pub fn seek(&self, seconds: f64) {
        self.send(Command::Seek(seconds));
    }

    pub fn set_volume(&self, volume: f32) {
        self.send(Command::Volume(volume));
    }

    /// Settings admission uses one bounded latest-value slot and never opens an
    /// output or modifies an approved media request.
    pub fn set_equalizer(&self, settings: EqualizerSettings) -> Result<()> {
        self.validate_equalizer_admission(&settings)?;
        let mut control = self
            .inner
            .equalizer
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let sequence = control
            .snapshot
            .requested_sequence
            .checked_add(1)
            .context("Equalizer request sequence exhausted")?;
        control.snapshot.desired = settings;
        control.snapshot.requested_sequence = sequence;
        control.pending = Some((settings, sequence));
        drop(control);
        if let Some(thread) = &self.inner.thread {
            thread.unpark();
        }
        Ok(())
    }

    pub(crate) fn validate_equalizer_admission(&self, settings: &EqualizerSettings) -> Result<()> {
        settings.validate()?;
        if self.inner.shutdown_requested.load(Ordering::Acquire) {
            bail!("Audio engine is closed");
        }
        self.inner
            .equalizer
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .snapshot
            .requested_sequence
            .checked_add(1)
            .context("Equalizer request sequence exhausted")?;
        Ok(())
    }

    pub fn equalizer_snapshot(&self) -> EqualizerSnapshot {
        self.inner
            .equalizer
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .snapshot
    }

    pub fn validate_processing_admission(
        &self,
        settings: &PlaybackProcessingSettings,
    ) -> Result<()> {
        settings.validate()?;
        if self.inner.shutdown_requested.load(Ordering::Acquire) {
            bail!("Audio engine is closed");
        }
        self.inner
            .processing
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .snapshot
            .requested_sequence
            .checked_add(1)
            .context("Playback processing sequence exhausted")?;
        Ok(())
    }

    pub fn set_processing(&self, settings: PlaybackProcessingSettings) -> Result<()> {
        self.validate_processing_admission(&settings)?;
        let mut control = self
            .inner
            .processing
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let sequence = control
            .snapshot
            .requested_sequence
            .checked_add(1)
            .context("Playback processing sequence exhausted")?;
        control.snapshot.desired = settings;
        control.snapshot.requested_sequence = sequence;
        control.pending = Some((settings, sequence));
        drop(control);
        if let Some(thread) = &self.inner.thread {
            thread.unpark();
        }
        Ok(())
    }

    pub fn processing_snapshot(&self) -> PlaybackProcessingSnapshot {
        self.inner
            .processing
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .snapshot
    }
    pub fn analysis_snapshot(&self) -> Option<analyser::AnalysisFrame> {
        if self.inner.shutdown_requested.load(Ordering::Acquire) {
            return None;
        }
        let frame = *self
            .inner
            .analysis
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let (generation, intent, seek_seq, configuration, frame) = frame?;
        let (enabled, sequence) = *self
            .inner
            .analysis_configuration
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if !enabled || configuration != sequence {
            return None;
        }
        let snapshot = self.snapshot();
        (snapshot.generation == generation
            && snapshot.seek_seq == seek_seq
            && intent == self.inner.intent_revision.load(Ordering::Acquire)
            && matches!(
                snapshot.status,
                PlaybackStatus::Playing | PlaybackStatus::Paused | PlaybackStatus::Buffering
            )
            && *self
                .inner
                .analysis_configuration
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                == (true, sequence))
            .then_some(frame)
    }
    pub fn spatial_snapshot(&self) -> crate::domain::audio_effects::SpatialRuntimeState {
        let current = self
            .inner
            .spatial
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        crate::domain::audio_effects::SpatialRuntimeState {
            desired: current.snapshot.desired.spatial,
            requested_sequence: current.snapshot.requested_sequence,
            applied_sequence: current.spatial_applied_sequence,
            error: current.snapshot.error.clone(),
        }
    }
    pub fn effects_snapshot(&self) -> crate::domain::audio_effects::EffectsSnapshot {
        self.inner
            .spatial
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .snapshot
            .clone()
    }
    pub(crate) fn validate_effects_admission(
        &self,
        settings: &crate::domain::audio_effects::EffectsSettings,
        impulse: Option<&Arc<effects_factory::OwnedImpulse>>,
    ) -> Result<()> {
        settings.validate()?;
        anyhow::ensure!(
            !self.inner.shutdown_requested.load(Ordering::Acquire),
            "Audio engine is closed"
        );
        match (&settings.convolution.impulse, impulse) {
            (None, None) => {}
            (Some(id), Some(actual)) if id == actual.source_id() => {}
            _ => anyhow::bail!("Effect impulse requires its matching actual decoded owner"),
        }
        let current = self
            .inner
            .spatial
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        anyhow::ensure!(
            (&current.snapshot.desired == settings && current.matches_resource(impulse))
                || current.snapshot.requested_sequence < u64::MAX,
            "Effects configuration sequence exhausted"
        );
        Ok(())
    }
    pub fn set_effects(
        &self,
        settings: crate::domain::audio_effects::EffectsSettings,
        impulse: Option<Arc<effects_factory::OwnedImpulse>>,
    ) -> Result<()> {
        self.validate_effects_admission(&settings, impulse.as_ref())?;
        let mut current = self
            .inner
            .spatial
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        anyhow::ensure!(
            !self.inner.shutdown_requested.load(Ordering::Acquire),
            "Audio engine is closed"
        );
        let same_resource = current.matches_resource(impulse.as_ref());
        if current.snapshot.desired != settings || !same_resource {
            current.snapshot.requested_sequence = current
                .snapshot
                .requested_sequence
                .checked_add(1)
                .context("Effects configuration sequence exhausted")?;
            current.snapshot.desired = settings;
            current.impulse = impulse;
            current.snapshot.status = crate::domain::audio_effects::EffectsStatus::Preparing;
            current.snapshot.error = None;
        }
        Ok(())
    }
    pub(crate) fn validate_spatial_admission(
        &self,
        settings: crate::audio::spatial::SpatialSettings,
    ) -> Result<()> {
        settings.validate()?;
        anyhow::ensure!(
            !self.inner.shutdown_requested.load(Ordering::Acquire),
            "Audio engine is closed"
        );
        let current = self
            .inner
            .spatial
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        anyhow::ensure!(
            current.snapshot.desired.spatial == settings
                || current.snapshot.requested_sequence < u64::MAX,
            "Spatial configuration sequence exhausted"
        );
        Ok(())
    }
    pub fn set_spatial(&self, settings: crate::audio::spatial::SpatialSettings) -> Result<()> {
        self.validate_spatial_admission(settings)?;
        let mut current = self
            .inner
            .spatial
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if current.snapshot.desired.spatial != settings {
            current.snapshot.requested_sequence = current
                .snapshot
                .requested_sequence
                .checked_add(1)
                .context("Spatial configuration sequence exhausted")?;
            current.snapshot.desired.spatial = settings;
            current.snapshot.status = crate::domain::audio_effects::EffectsStatus::Preparing;
            current.snapshot.error = None;
        }
        Ok(())
    }
    pub(crate) fn validate_analysis_admission(&self, enabled: bool) -> Result<()> {
        anyhow::ensure!(
            !self.inner.shutdown_requested.load(Ordering::Acquire),
            "Audio engine is closed"
        );
        let current = self
            .inner
            .analysis_configuration
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        anyhow::ensure!(
            current.0 == enabled || current.1 < (u64::MAX >> 1),
            "Analysis configuration sequence exhausted"
        );
        Ok(())
    }
    pub fn set_analysis_enabled(&self, enabled: bool) -> Result<()> {
        self.validate_analysis_admission(enabled)?;
        let mut current = self
            .inner
            .analysis_configuration
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if current.0 != enabled {
            current.1 = current
                .1
                .checked_add(1)
                .filter(|sequence| *sequence <= u64::MAX >> 1)
                .context("Analysis configuration sequence exhausted")?;
            current.0 = enabled;
        }
        Ok(())
    }

    /// A single bounded slot coalesces durable preferences without competing
    /// with playback commands. Device enumeration and opening occur on the owner.
    pub fn set_output_preferences(
        &self,
        preferences: crate::domain::audio_output::AudioPreferences,
    ) -> Result<()> {
        preferences.validate()?;
        if self.inner.shutdown_requested.load(Ordering::Acquire) {
            bail!("Audio engine is closed");
        }
        let mut pending = self
            .inner
            .pending_preferences
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if self.inner.output_failed.load(Ordering::Acquire)
            || self
                .inner
                .thread
                .as_ref()
                .is_none_or(TrackedWorker::is_finished)
        {
            *pending = None;
            let mut output = self
                .inner
                .output_snapshot
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            output.preferences = preferences.clone();
            output.state.desired = preferences.selection;
            output.state.status = crate::domain::audio_output::AudioOutputStatus::Unavailable;
            output.state.last_error =
                Some(crate::domain::audio_output::AudioOutputError::WorkerUnavailable);
            return Ok(());
        }
        *pending = Some(preferences);
        drop(pending);
        if let Some(thread) = &self.inner.thread {
            thread.unpark();
        }
        Ok(())
    }

    pub fn refresh_audio_devices(&self) -> Result<()> {
        if self.inner.shutdown_requested.load(Ordering::Acquire)
            || self.inner.output_failed.load(Ordering::Acquire)
            || self
                .inner
                .thread
                .as_ref()
                .is_none_or(TrackedWorker::is_finished)
        {
            bail!("Audio engine is closed");
        }
        self.inner.refresh_devices.store(true, Ordering::Release);
        if let Some(thread) = &self.inner.thread {
            thread.unpark();
        }
        Ok(())
    }

    pub fn output_snapshot(&self) -> crate::domain::audio_output::AudioOutputSnapshot {
        let mut pending = self
            .inner
            .pending_preferences
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let mut output = self
            .inner
            .output_snapshot
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if !self.inner.shutdown_requested.load(Ordering::Acquire)
            && (self.inner.output_failed.load(Ordering::Acquire)
                || self
                    .inner
                    .thread
                    .as_ref()
                    .is_none_or(TrackedWorker::is_finished))
        {
            if let Some(preferences) = pending.take() {
                output.state.desired = preferences.selection.clone();
                output.preferences = preferences;
            }
            output.state.status = crate::domain::audio_output::AudioOutputStatus::Unavailable;
            output.state.last_error =
                Some(crate::domain::audio_output::AudioOutputError::WorkerUnavailable);
        }
        output.clone()
    }

    /// Automatic pauses cannot override a more recent playback command.
    pub fn take_device_change_pause(&self) -> bool {
        let paused = self.inner.device_pause_revision.swap(0, Ordering::AcqRel);
        paused != 0 && paused - 1 == self.inner.intent_revision.load(Ordering::Acquire)
    }

    pub fn stop(&self) {
        self.send(Command::Stop);
    }

    /// Acknowledges a nonfatal control error without interrupting playback.
    pub fn clear_error(&self) {
        self.send(Command::ClearError);
    }

    pub fn snapshot(&self) -> PlaybackSnapshot {
        self.inner.snapshot.lock().unwrap().clone()
    }

    pub fn rendered_pcm(&self) -> Option<RenderedPcm> {
        self.inner
            .observation
            .as_ref()
            .map(|state| state.lock().unwrap().clone())
    }

    /// Timing from the engine's latest observation of the actual Memory renderer.
    pub fn output_render_timing(&self) -> Option<output::OutputRenderTiming> {
        *self
            .inner
            .output_render_timing
            .lock()
            .unwrap_or_else(|error| error.into_inner())
    }

    /// The engine's latest scalar observation of the actual foreground owners.
    pub fn supply_snapshot(&self) -> Option<supply::AudioSupplySnapshot> {
        self.inner
            .supply
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }

    /// Cancels in-flight transport and joins the engine, renderer and load threads.
    /// A timeout retains ownership so the caller can retry shutdown safely.
    pub fn shutdown(&self, timeout: Duration) -> Result<()> {
        let started = Instant::now();
        self.inner.shutdown_requested.store(true, Ordering::Release);
        let cache = self
            .inner
            .cache
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        if let Some(cache) = &cache {
            cache.begin_shutdown();
        }
        let mut worker_panicked = false;
        if let Some(thread) = &self.inner.thread {
            thread.unpark();
            if !thread.wait(timeout.saturating_sub(started.elapsed())) {
                bail!(AudioShutdownError::EngineTimeout);
            }
            worker_panicked = thread.panicked();
        }
        if let Some(cache) = cache
            && !cache.shutdown(timeout.saturating_sub(started.elapsed()))
        {
            bail!(AudioShutdownError::CacheTimeout);
        }
        if worker_panicked {
            let mut output = self
                .inner
                .output_snapshot
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            output.state.status = crate::domain::audio_output::AudioOutputStatus::Unavailable;
            output.state.last_error =
                Some(crate::domain::audio_output::AudioOutputError::WorkerUnavailable);
            bail!(AudioShutdownError::WorkerPanicked);
        }
        {
            let mut effects = self
                .inner
                .spatial
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            effects.impulse.take();
            effects.snapshot.status = crate::domain::audio_effects::EffectsStatus::Idle;
            effects.snapshot.applied_sequence = 0;
            effects.spatial_applied_sequence = 0;
            effects.snapshot.sample_rate = None;
            effects.snapshot.effect_channels = 0;
            effects.snapshot.device_channels = 0;
            effects.snapshot.latency_frames = 0;
            effects.snapshot.owned_bytes = 0;
            effects.snapshot.tail_remaining_frames = 0;
            effects.snapshot.transition_remaining_frames = 0;
            effects.snapshot.compressor_reduction_db = 0.0;
        }
        let mut output = self
            .inner
            .output_snapshot
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        output.state.status = crate::domain::audio_output::AudioOutputStatus::Closed;
        output.state.active_device_id = None;
        output.state.active_device_name = None;
        output.state.format = None;
        let mut equalizer = self
            .inner
            .equalizer
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        equalizer.snapshot.sample_rate = None;
        equalizer.snapshot.active_bands.fill(false);
        equalizer.snapshot.transition_remaining_frames = 0;
        let mut processing = self
            .inner
            .processing
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        processing.snapshot.sample_rate = None;
        processing.snapshot.transition_remaining_frames = 0;
        processing.snapshot.latency_frames = 0;
        Ok(())
    }

    fn send(&self, command: Command) {
        if self.inner.shutdown_requested.load(Ordering::Acquire) {
            self.inner.snapshot.lock().unwrap().error = Some("音频引擎已关闭".into());
            return;
        }
        let _admission = self
            .inner
            .control_admission
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let command = match self.stamp_control(command) {
            Ok(command) => command,
            Err(error) => {
                self.inner.snapshot.lock().unwrap().error = Some(error.to_string());
                return;
            }
        };
        match self.inner.commands.try_send(command) {
            Ok(()) => {}
            Err(mpsc::TrySendError::Full(_)) => {
                self.inner.snapshot.lock().unwrap().error =
                    Some("音频控制队列已满；请稍后重试".into());
            }
            Err(mpsc::TrySendError::Disconnected(_)) => {
                let mut state = self.inner.snapshot.lock().unwrap();
                state.status = PlaybackStatus::Error;
                state.error = Some("音频线程已退出，请重启播放器".into());
            }
        }
    }

    fn stamp_control(&self, command: Command) -> Result<ControlCommand> {
        let changes_intent = matches!(
            &command,
            Command::Play
                | Command::Pause
                | Command::Toggle
                | Command::Stop
                | Command::Load { .. }
                | Command::Promote { .. }
                | Command::InvalidUrl(_)
        );
        let intent_revision = if changes_intent {
            self.inner
                .intent_revision
                .try_update(Ordering::AcqRel, Ordering::Acquire, |value| {
                    value.checked_add(1).filter(|next| *next < u64::MAX)
                })
                .map_err(|_| anyhow::anyhow!("Audio playback intent sequence exhausted"))?
                + 1
        } else {
            self.inner.intent_revision.load(Ordering::Acquire)
        };
        Ok(ControlCommand {
            command,
            intent_revision,
        })
    }
}

#[derive(Clone)]
enum OutputMode {
    Device,
    Memory(Arc<Mutex<RenderedPcm>>),
}

type EngineOutput = Output;
struct EffectsRequest {
    settings: crate::domain::audio_effects::EffectsSettings,
    sequence: u64,
    impulse: Option<Arc<effects_factory::OwnedImpulse>>,
}
fn effect_request(control: &Mutex<AudioEffectsControl>) -> EffectsRequest {
    let current = control.lock().unwrap_or_else(|error| error.into_inner());
    EffectsRequest {
        settings: current.snapshot.desired.clone(),
        sequence: current.snapshot.requested_sequence,
        impulse: current.impulse.clone(),
    }
}
fn open_output(
    mode: &OutputMode,
    preferences: &crate::domain::audio_output::AudioPreferences,
    equalizer: (EqualizerSettings, u64),
    processing: (PlaybackProcessingSettings, u64),
    effects: EffectsRequest,
) -> Result<Output> {
    let mut output = match mode {
        OutputMode::Device => Output::with_preferences(preferences.clone())?,
        OutputMode::Memory(observation) => Output::memory(observation.clone())?,
    };
    output.select(preferences.selection.clone())?;
    output.set_mute(preferences.muted);
    output.set_pause_on_device_change(preferences.pause_on_device_change);
    if equalizer.1 != 0 {
        output.set_equalizer(equalizer.0, equalizer.1)?;
    }
    if processing.1 != 0 {
        output.set_processing(processing.0, processing.1)?;
    }
    output.configure_effects(effects.settings, effects.sequence, effects.impulse)?;
    Ok(output)
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct RenderedPcm {
    pub batches: u64,
    pub rms: f64,
    pub peak: f32,
    pub frequency_hz: Option<f64>,
}

pub(crate) struct ActiveStream {
    decoder: DecoderHandle,
    control: StreamControl,
    request_url_sha256: String,
    attached: bool,
    committed_seek: u64,
    ready: Option<DecoderReady>,
    promotion: Option<Promotion>,
}
impl ActiveStream {
    fn cancel(&self) {
        if let Some(promotion) = &self.promotion {
            promotion.permit.cancel.cancel();
        }
        self.decoder.cancel();
        self.control.cancel();
    }
    fn finished(&mut self) -> bool {
        self.decoder.try_join() && self.control.join(Duration::ZERO)
    }
}

fn fail_unavailable_output(
    active: &mut Option<ActiveStream>,
    retired: &mut Vec<ActiveStream>,
    snapshot: &Mutex<PlaybackSnapshot>,
    preload_view: &SharedView,
    error: Option<crate::domain::audio_output::AudioOutputError>,
) {
    let Some(stream) = active.take() else {
        return;
    };
    let (message, kind) = match error {
        Some(crate::domain::audio_output::AudioOutputError::DeviceNotAvailable) => (
            "没有可用的音频输出设备，请连接或启用扬声器/耳机".to_owned(),
            Some(OutputError::DeviceNotAvailable),
        ),
        Some(error) => (format!("音频输出不可用：{error}"), None),
        None => ("音频输出不可用，请检查输出设备".to_owned(), None),
    };
    publish_promotion(
        preload_view,
        &stream,
        AudioPreloadState::Failed,
        Some(AudioPreloadError::new(
            AudioPreloadErrorKind::Transport,
            "Prepared audio output is unavailable",
        )),
    );
    stream.cancel();
    fail_with(
        snapshot,
        message,
        PlaybackFailure::output(kind, Some(stream.request_url_sha256.clone())),
    );
    // Cancellation belongs to this output failure. Retire the decoder so its
    // later cancellation result cannot overwrite the original device error.
    retired.push(stream);
}

fn publish_promotion(
    view: &SharedView,
    stream: &ActiveStream,
    state: AudioPreloadState,
    error: Option<AudioPreloadError>,
) {
    let Some(promotion) = &stream.promotion else {
        return;
    };
    if promotion.completed {
        return;
    }
    let decoded = stream.decoder.snapshot();
    let snapshot = AudioPreloadSnapshot {
        operation_id: promotion.permit.operation_id.clone(),
        nonce: promotion.permit.nonce,
        state,
        expected_generation: promotion.permit.expected_generation,
        identity: promotion.permit.identity.clone(),
        encoded_bytes: stream.control.status().encoded_downloaded_bytes,
        pcm_seconds: decoded.format.map_or(0.0, |format| {
            decoded.pcm.queued_frames as f64 / format.sample_rate as f64
        }),
        error,
    };
    let mut view = view.lock().unwrap_or_else(|error| error.into_inner());
    view.snapshot = Some(snapshot);
    view.cancel = Some(promotion.permit.cancel.clone());
}
fn publish_rejected_promotion(view: &SharedView, token: &AudioPreloadToken) {
    let mut view = view.lock().unwrap_or_else(|error| error.into_inner());
    let mut snapshot = view
        .snapshot
        .as_ref()
        .filter(|snapshot| {
            snapshot.nonce == token.permit.nonce
                && snapshot.operation_id == token.permit.operation_id
        })
        .cloned()
        .unwrap_or(AudioPreloadSnapshot {
            operation_id: token.permit.operation_id.clone(),
            nonce: token.permit.nonce,
            state: AudioPreloadState::Failed,
            expected_generation: token.permit.expected_generation,
            identity: token.permit.identity.clone(),
            encoded_bytes: 0,
            pcm_seconds: 0.0,
            error: None,
        });
    snapshot.state = AudioPreloadState::Failed;
    snapshot.error = Some(AudioPreloadError::new(
        AudioPreloadErrorKind::Stale,
        "Prepared audio promotion fence is stale",
    ));
    view.snapshot = Some(snapshot);
    view.cancel = Some(token.permit.cancel.clone());
}
fn publish_prepared(
    view: &SharedView,
    prepared: &PreparedStream,
    state: AudioPreloadState,
    error: Option<AudioPreloadError>,
) {
    let snapshot = prepared.snapshot(state, error);
    let mut view = view.lock().unwrap_or_else(|error| error.into_inner());
    view.snapshot = Some(snapshot);
    view.cancel = Some(prepared.permit.cancel.clone());
}

fn discard_prepared(
    prepared: &mut Option<PreparedStream>,
    retired: &mut Vec<ActiveStream>,
    view: &SharedView,
    error: Option<AudioPreloadError>,
) {
    if let Some(mut prepared) = prepared.take() {
        let error = error.unwrap_or_else(|| {
            AudioPreloadError::new(
                AudioPreloadErrorKind::Cancelled,
                "Audio preparation cancelled",
            )
        });
        let state = if error.kind == AudioPreloadErrorKind::Cancelled {
            AudioPreloadState::Cancelled
        } else {
            AudioPreloadState::Failed
        };
        publish_prepared(view, &prepared, state, Some(error.clone()));
        if let Some(reply) = prepared.reply.take() {
            let _ = reply.try_send(Err(error));
        }
        prepared.cancel();
        retired.push(prepared.stream);
    }
}

fn poll_prepared(
    prepared: &mut Option<PreparedStream>,
    retired: &mut Vec<ActiveStream>,
    view: &SharedView,
    current_generation: u64,
) {
    let Some(slot) = prepared.as_mut() else {
        return;
    };
    if slot.permit.cancelled() || slot.permit.expected_generation != current_generation {
        discard_prepared(prepared, retired, view, None);
        return;
    }
    let status = slot.stream.control.status();
    let decoded = slot.stream.decoder.snapshot();
    let failure = if let Some(error) = &decoded.error {
        Some(
            AudioPreloadError::new(
                AudioPreloadErrorKind::Decode,
                format!("Audio preparation decoding failed: {error}"),
            )
            .with_media(error.media.clone()),
        )
    } else if let Some(error) = status
        .error
        .as_ref()
        .filter(|error| error.kind != MediaIoErrorKind::Cancelled)
    {
        Some(
            AudioPreloadError::new(
                AudioPreloadErrorKind::Transport,
                "Audio preparation transport failed",
            )
            .with_media(Some(error.clone())),
        )
    } else if decoded.status == DecoderStatus::Cancelled {
        Some(AudioPreloadError::new(
            AudioPreloadErrorKind::Cancelled,
            "Audio preparation cancelled",
        ))
    } else if slot.ready.is_some()
        && (status.representation_epoch != slot.representation_epoch
            || status.cache.epoch != slot.cache_epoch
            || decoded.status == DecoderStatus::Seeking)
    {
        Some(AudioPreloadError::new(
            AudioPreloadErrorKind::Stale,
            "Prepared audio representation or cache changed",
        ))
    } else {
        None
    };
    if let Some(error) = failure {
        discard_prepared(prepared, retired, view, Some(error));
        return;
    }
    if slot.ready.is_none() {
        if let Some(result) = slot.stream.decoder.poll_ready() {
            match result {
                Ok(ready) => {
                    let state = slot.stream.decoder.snapshot();
                    if state.pcm.consumed_frames != 0 || state.pcm.position_seconds != 0.0 {
                        discard_prepared(
                            prepared,
                            retired,
                            view,
                            Some(AudioPreloadError::new(
                                AudioPreloadErrorKind::Stale,
                                "Prepared audio media clock advanced",
                            )),
                        );
                        return;
                    }
                    slot.stream.decoder.set_paused(true);
                    slot.representation_epoch = status.representation_epoch;
                    slot.cache_epoch = status.cache.epoch;
                    slot.ready = Some(ready);
                    publish_prepared(view, slot, AudioPreloadState::Ready, None);
                    if let Some(reply) = slot.reply.take() {
                        let token = AudioPreloadToken::new(slot.permit.clone());
                        if reply.try_send(Ok(token)).is_err() {
                            discard_prepared(prepared, retired, view, None);
                            return;
                        }
                    }
                }
                Err(error) => {
                    discard_prepared(
                        prepared,
                        retired,
                        view,
                        Some(
                            AudioPreloadError::new(
                                AudioPreloadErrorKind::Decode,
                                format!("Audio preparation decoding failed: {error}"),
                            )
                            .with_media(error.media.clone()),
                        ),
                    );
                    return;
                }
            }
        } else if status.budget_blocked {
            let since = slot.budget_since.get_or_insert_with(Instant::now);
            if since.elapsed() >= Duration::from_millis(250) {
                discard_prepared(
                    prepared,
                    retired,
                    view,
                    Some(AudioPreloadError::new(
                        AudioPreloadErrorKind::Limit,
                        "Audio preparation reached its encoded-byte budget before PCM was ready",
                    )),
                );
                return;
            }
        } else {
            slot.budget_since = None;
        }
    }
    if let Some(slot) = prepared.as_ref() {
        publish_prepared(
            view,
            slot,
            if slot.ready.is_some() {
                AudioPreloadState::Ready
            } else {
                AudioPreloadState::Preparing
            },
            None,
        );
    }
}
struct EngineShared {
    spatial: Arc<Mutex<AudioEffectsControl>>,
    analysis: SharedAnalysisPublication,
    analysis_configuration: Arc<Mutex<(bool, u64)>>,
    shared_cache: Arc<Mutex<Option<Arc<CacheStore>>>>,
    preload_view: SharedView,
    output_snapshot: Arc<Mutex<crate::domain::audio_output::AudioOutputSnapshot>>,
    output_render_timing: Arc<Mutex<Option<output::OutputRenderTiming>>>,
    supply: Arc<Mutex<Option<supply::AudioSupplySnapshot>>>,
    pending_preferences: Arc<Mutex<Option<crate::domain::audio_output::AudioPreferences>>>,
    refresh_devices: Arc<AtomicBool>,
    output_failed: Arc<AtomicBool>,
    device_pause_revision: Arc<AtomicU64>,
    equalizer: Arc<Mutex<EqualizerControl>>,
    processing: Arc<Mutex<ProcessingControl>>,
}

fn run_engine(
    receiver: mpsc::Receiver<ControlCommand>,
    snapshot: Arc<Mutex<PlaybackSnapshot>>,
    output_mode: OutputMode,
    shutdown_requested: Arc<AtomicBool>,
    options: TransportOptions,
    shared: EngineShared,
) {
    let EngineShared {
        spatial,
        analysis_configuration,
        analysis,
        shared_cache,
        preload_view,
        output_snapshot,
        output_render_timing,
        supply,
        pending_preferences,
        refresh_devices,
        output_failed,
        device_pause_revision,
        equalizer,
        processing,
    } = shared;
    let transport = match AudioTransport::new(options) {
        Ok(transport) => transport,
        Err(error) => {
            output_failed.store(true, Ordering::Release);
            let mut pending = pending_preferences
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            let mut output = output_snapshot
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            if let Some(preferences) = pending.take() {
                output.state.desired = preferences.selection.clone();
                output.preferences = preferences;
            }
            output.state.status = crate::domain::audio_output::AudioOutputStatus::Unavailable;
            output.state.last_error =
                Some(crate::domain::audio_output::AudioOutputError::WorkerUnavailable);
            drop(output);
            drop(pending);
            fail_with(
                &snapshot,
                format!("Cannot initialize audio transport: {error:#}"),
                PlaybackFailure::transport(&error, None),
            );
            if let Some(initialization) = error.downcast_ref::<transport::TransportInitError>() {
                while !initialization.shutdown(Duration::from_millis(20)) {
                    thread::park_timeout(Duration::from_millis(2));
                }
            }
            return;
        }
    };
    *shared_cache.lock().unwrap_or_else(|e| e.into_inner()) = Some(transport.cache().clone());
    let mut output: Option<EngineOutput> = None;
    let mut output_preferences = crate::domain::audio_output::AudioPreferences::default();
    let mut equalizer_request = (EqualizerSettings::default(), 0);
    let mut processing_request = (PlaybackProcessingSettings::default(), 0);
    let mut retired_outputs: Vec<EngineOutput> = Vec::new();
    let mut active: Option<ActiveStream> = None;
    let mut speculative: Option<PreparedStream> = None;
    let mut preload_nonce = 0u64;
    let mut retired: Vec<ActiveStream> = Vec::new();
    let mut last_request: Option<ApprovedAudioRequest> = None;
    let mut pending_seek: Option<(u64, Duration)> = None;
    let mut request_seek_seq = 0u64;
    let mut generation = 0u64;
    let mut desired_playing = false;
    let mut deferred: Option<ControlCommand> = None;
    let mut applied_intent_revision = 0;
    loop {
        if shutdown_requested.load(Ordering::Acquire) {
            break;
        }
        retired.retain_mut(|old| !old.finished());
        if let Some(request) = processing
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .pending
            .take()
        {
            processing_request = request;
            if let Some(stream) = active.as_ref()
                && let Err(error) = stream.decoder.set_processing(request.0, request.1)
            {
                snapshot.lock().unwrap().error = Some(format!("Cannot apply tempo: {error}"));
            }
            if let Some(slot) = speculative.as_ref() {
                let _ = slot.stream.decoder.set_processing(request.0, request.1);
            }
            if let Some(current) = output.as_ref()
                && let Err(error) = current.set_processing(request.0, request.1)
            {
                snapshot.lock().unwrap().error = Some(format!("Cannot apply pitch: {error}"));
            }
        }
        if let Some(request) = equalizer
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .pending
            .take()
        {
            equalizer_request = request;
            if let Some(current) = output.as_ref()
                && let Err(error) = current.set_equalizer(request.0, request.1)
            {
                snapshot.lock().unwrap().error = Some(format!("Cannot apply equalizer: {error}"));
            }
        }
        retired_outputs.retain_mut(|old| {
            let joined = old.shutdown(Duration::ZERO);
            if output.is_none() {
                let mut state = old.snapshot();
                if !joined {
                    state.state.status =
                        crate::domain::audio_output::AudioOutputStatus::Unavailable;
                }
                *output_snapshot
                    .lock()
                    .unwrap_or_else(|error| error.into_inner()) = state;
            }
            !joined
        });
        let preferences = pending_preferences
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take();
        let refresh = refresh_devices.swap(false, Ordering::AcqRel);
        if let Some(preferences) = preferences {
            output_preferences = preferences;
            if let Some(current) = output.as_ref() {
                let _ = current.select(output_preferences.selection.clone());
                current.set_mute(output_preferences.muted);
                current.set_pause_on_device_change(output_preferences.pause_on_device_change);
            } else {
                let mut state = output_snapshot
                    .lock()
                    .unwrap_or_else(|error| error.into_inner());
                state.preferences = output_preferences.clone();
                state.state.desired = output_preferences.selection.clone();
            }
        }
        if refresh {
            if let Some(current) = output.as_ref() {
                let _ = current.refresh();
            } else if matches!(output_mode, OutputMode::Device) {
                match open_output(
                    &output_mode,
                    &output_preferences,
                    equalizer_request,
                    processing_request,
                    effect_request(&spatial),
                ) {
                    Ok(current) => output = Some(current),
                    Err(_) => {
                        let mut state = output_snapshot
                            .lock()
                            .unwrap_or_else(|error| error.into_inner());
                        state.state.status =
                            crate::domain::audio_output::AudioOutputStatus::Unavailable;
                        state.state.last_error =
                            Some(crate::domain::audio_output::AudioOutputError::WorkerUnavailable);
                    }
                }
            }
        }
        let command = if let Some(command) = deferred.take() {
            Ok(command)
        } else {
            receiver.recv_timeout(Duration::from_millis(10))
        };
        let command = command.map(|control| {
            applied_intent_revision = control.intent_revision;
            control.command
        });
        if matches!(
            &command,
            Ok(Command::Play
                | Command::Pause
                | Command::Toggle
                | Command::Stop
                | Command::Load { .. }
                | Command::Promote { .. }
                | Command::InvalidUrl(_))
        ) && let Some(current) = output.as_ref()
        {
            current.take_device_change_pause();
        }
        match command {
            Ok(Command::Prepare {
                operation_id,
                request,
                expected_generation,
                cancel,
                limits,
                reply,
            }) => {
                let current = snapshot.lock().unwrap().clone();
                if current.generation != expected_generation
                    || active.is_none()
                    || matches!(
                        current.status,
                        PlaybackStatus::Ended | PlaybackStatus::Idle | PlaybackStatus::Error
                    )
                    || cancel.ownership() != LeaseOwnership::Speculative
                {
                    let _ = reply.try_send(Err(AudioPreloadError::new(
                        AudioPreloadErrorKind::Stale,
                        "Audio preparation foreground fence is stale",
                    )));
                    continue;
                }
                if retired.len() >= MAX_LOAD_TASKS
                    || speculative
                        .as_ref()
                        .is_some_and(|slot| slot.permit.operation_id == operation_id)
                {
                    let _ = reply.try_send(Err(AudioPreloadError::new(
                        AudioPreloadErrorKind::Limit,
                        "Audio preparation capacity is unavailable",
                    )));
                    continue;
                }
                discard_prepared(&mut speculative, &mut retired, &preload_view, None);
                let Some(nonce) = preload_nonce.checked_add(1) else {
                    let _ = reply.try_send(Err(AudioPreloadError::new(
                        AudioPreloadErrorKind::Limit,
                        "Audio preparation nonce exhausted",
                    )));
                    continue;
                };
                preload_nonce = nonce;
                let permit = Permit::new(
                    operation_id,
                    nonce,
                    expected_generation,
                    request.identity.clone(),
                    cancel.clone(),
                );
                match transport.open_speculative(
                    (*request).clone(),
                    cancel.transport_cancel(),
                    limits.encoded_bytes,
                ) {
                    Ok(lease) => {
                        let control = lease.control.clone();
                        match DecoderHandle::start(
                            lease,
                            DecoderConfig {
                                processing: processing_request.0,
                                processing_sequence: processing_request.1,
                                initially_paused: true,
                                queue_duration: limits.pcm_duration,
                                prebuffer_duration: limits.prebuffer_duration,
                                ..Default::default()
                            },
                        ) {
                            Ok(decoder) => {
                                let slot = PreparedStream {
                                    stream: ActiveStream {
                                        decoder,
                                        control,
                                        request_url_sha256: request.identity.url_sha256.clone(),
                                        attached: false,
                                        committed_seek: 0,
                                        ready: None,
                                        promotion: None,
                                    },
                                    request: *request,
                                    permit,
                                    reply: Some(reply),
                                    ready: None,
                                    representation_epoch: 0,
                                    cache_epoch: 0,
                                    budget_since: None,
                                };
                                publish_prepared(
                                    &preload_view,
                                    &slot,
                                    AudioPreloadState::Preparing,
                                    None,
                                );
                                speculative = Some(slot);
                            }
                            Err(error) => {
                                control.cancel();
                                permit.cancel();
                                let _ = reply.try_send(Err(AudioPreloadError::new(
                                    AudioPreloadErrorKind::Decode,
                                    format!("Audio preparation decoding failed: {error}"),
                                )
                                .with_media(error.media.clone())));
                            }
                        }
                    }
                    Err(error) => {
                        permit.cancel();
                        let failure = PlaybackFailure::stream_open(
                            &error,
                            transport.cache(),
                            request.identity.url_sha256.clone(),
                        );
                        let _ = reply.try_send(Err(AudioPreloadError::new(
                            AudioPreloadErrorKind::Transport,
                            "Audio preparation transport could not open approved media",
                        )
                        .with_media(failure.media)));
                    }
                }
            }
            Ok(Command::CancelPreload(operation_id)) => {
                if speculative.as_ref().is_some_and(|slot| {
                    slot.permit.operation_id == operation_id
                        && slot.permit.cancel.ownership() != LeaseOwnership::Promoted
                }) {
                    discard_prepared(&mut speculative, &mut retired, &preload_view, None);
                }
            }
            Ok(Command::Promote {
                mut token,
                expected_generation,
                autoplay,
            }) => {
                let current = snapshot.lock().unwrap().clone();
                let admitted = speculative.as_ref().is_some_and(|slot| {
                    Arc::ptr_eq(&slot.permit, &token.permit)
                        && slot.ready.is_some()
                        && slot.permit.expected_generation == expected_generation
                        && current.generation == expected_generation
                        && current.status == PlaybackStatus::Ended
                        && !slot.permit.cancelled()
                        && {
                            let status = slot.stream.control.status();
                            status.error.is_none()
                                && status.representation_epoch == slot.representation_epoch
                                && status.cache.epoch == slot.cache_epoch
                                && slot.stream.decoder.snapshot().status != DecoderStatus::Seeking
                        }
                });
                if !admitted || !token.claim() {
                    if speculative
                        .as_ref()
                        .is_some_and(|slot| Arc::ptr_eq(&slot.permit, &token.permit))
                    {
                        discard_prepared(
                            &mut speculative,
                            &mut retired,
                            &preload_view,
                            Some(AudioPreloadError::new(
                                AudioPreloadErrorKind::Stale,
                                "Prepared audio promotion fence is stale",
                            )),
                        );
                    }
                    publish_rejected_promotion(&preload_view, &token);
                    continue;
                }
                let mut slot = speculative.take().expect("validated prepared audio slot");
                if slot.stream.control.promote_to_foreground().is_err() {
                    slot.permit.cancel.cancel();
                    slot.stream.cancel();
                    publish_prepared(
                        &preload_view,
                        &slot,
                        AudioPreloadState::Failed,
                        Some(AudioPreloadError::new(
                            AudioPreloadErrorKind::Transport,
                            "Prepared audio transport promotion failed",
                        )),
                    );
                    retired.push(slot.stream);
                    continue;
                }
                publish_prepared(&preload_view, &slot, AudioPreloadState::Promoting, None);
                generation = generation.wrapping_add(1);
                *supply.lock().unwrap_or_else(|error| error.into_inner()) = None;
                if let Some(output) = output.as_ref() {
                    output.clear();
                }
                if let Some(previous) = active.take() {
                    publish_promotion(
                        &preload_view,
                        &previous,
                        AudioPreloadState::Cancelled,
                        Some(AudioPreloadError::new(
                            AudioPreloadErrorKind::Cancelled,
                            "Audio promotion superseded",
                        )),
                    );
                    previous.cancel();
                    retired.push(previous);
                }
                desired_playing = autoplay;
                pending_seek = None;
                last_request = Some(slot.request.clone());
                slot.stream.ready = slot.ready.take();
                slot.stream.promotion = Some(Promotion {
                    permit: slot.permit.clone(),
                    completed: false,
                });
                let mut state = snapshot.lock().unwrap();
                *state = PlaybackSnapshot {
                    generation,
                    track_id: Some(slot.request.identity.track_id.clone()),
                    status: PlaybackStatus::Loading,
                    volume: state.volume,
                    seek_seq: state.seek_seq,
                    ..Default::default()
                };
                active = Some(slot.stream);
            }
            Ok(Command::Load { request, autoplay }) => {
                discard_prepared(&mut speculative, &mut retired, &preload_view, None);
                generation = generation.wrapping_add(1);
                *supply.lock().unwrap_or_else(|error| error.into_inner()) = None;
                if let Some(output) = output.as_ref() {
                    output.clear();
                }
                if let Some(previous) = active.take() {
                    publish_promotion(
                        &preload_view,
                        &previous,
                        AudioPreloadState::Cancelled,
                        Some(AudioPreloadError::new(
                            AudioPreloadErrorKind::Cancelled,
                            "Audio promotion superseded",
                        )),
                    );
                    previous.cancel();
                    retired.push(previous);
                }
                desired_playing = autoplay;
                pending_seek = None;
                let request_url_sha256 = request.identity.url_sha256.clone();
                last_request = Some((*request).clone());
                {
                    let mut state = snapshot.lock().unwrap();
                    let volume = state.volume;
                    let seek_seq = state.seek_seq;
                    *state = PlaybackSnapshot {
                        volume,
                        seek_seq,
                        generation,
                        status: PlaybackStatus::Loading,
                        track_id: Some(request.identity.track_id.clone()),
                        ..Default::default()
                    };
                }
                if retired.len() >= MAX_LOAD_TASKS {
                    fail(
                        &snapshot,
                        "Too many cancelled audio workers; wait for cleanup".into(),
                    );
                    continue;
                }
                let cancel = Arc::new(AtomicBool::new(false));
                match transport.open(*request, cancel) {
                    Ok(lease) => {
                        let control = lease.control.clone();
                        match DecoderHandle::start(
                            lease,
                            DecoderConfig {
                                processing: processing_request.0,
                                processing_sequence: processing_request.1,
                                initially_paused: !desired_playing,
                                ..Default::default()
                            },
                        ) {
                            Ok(decoder) => {
                                active = Some(ActiveStream {
                                    decoder,
                                    control,
                                    request_url_sha256: request_url_sha256.clone(),
                                    attached: false,
                                    committed_seek: 0,
                                    ready: None,
                                    promotion: None,
                                })
                            }
                            Err(error) => {
                                control.cancel();
                                fail_with(
                                    &snapshot,
                                    format!("Cannot start audio decoding: {error}"),
                                    PlaybackFailure::decode(&error, &request_url_sha256),
                                );
                            }
                        }
                    }
                    Err(error) => fail_with(
                        &snapshot,
                        format!("Cannot open audio stream: {error:#}"),
                        PlaybackFailure::stream_open(&error, transport.cache(), request_url_sha256),
                    ),
                }
            }
            Ok(Command::InvalidUrl(error)) => fail(&snapshot, error),
            Ok(Command::Play) => {
                desired_playing = true;
                let status = snapshot.lock().unwrap().status;
                if matches!(status, PlaybackStatus::Ended | PlaybackStatus::Error) {
                    if let Some(request) = last_request.clone() {
                        deferred = Some(ControlCommand {
                            command: Command::Load {
                                request: Box::new(request),
                                autoplay: true,
                            },
                            intent_revision: applied_intent_revision,
                        });
                    }
                } else if let Some(stream) = &active {
                    stream.decoder.set_paused(false);
                    if let Some(output) = output.as_ref() {
                        output.take_device_change_pause();
                        output.set_paused(false);
                    }
                    let mut state = snapshot.lock().unwrap();
                    state.error = None;
                    state.failure = None;
                }
            }
            Ok(Command::Pause) => {
                desired_playing = false;
                if let Some(stream) = &active {
                    stream.decoder.set_paused(true);
                }
                if let Some(output) = output.as_ref() {
                    output.set_paused(true);
                }
            }
            Ok(Command::Toggle) => {
                let status = snapshot.lock().unwrap().status;
                deferred = Some(ControlCommand {
                    command: if desired_playing
                        && matches!(
                            status,
                            PlaybackStatus::Playing
                                | PlaybackStatus::Loading
                                | PlaybackStatus::Buffering
                        ) {
                        Command::Pause
                    } else {
                        Command::Play
                    },
                    intent_revision: applied_intent_revision,
                });
            }
            Ok(Command::Seek(seconds)) => {
                if !seconds.is_finite() {
                    snapshot.lock().unwrap().error = Some("Position must be finite".into());
                    continue;
                }
                discard_prepared(&mut speculative, &mut retired, &preload_view, None);
                let duration = snapshot.lock().unwrap().duration_seconds;
                let seconds = if duration > 0.0 {
                    seconds.clamp(0.0, duration)
                } else {
                    seconds.max(0.0)
                };
                let Ok(position) = Duration::try_from_secs_f64(seconds) else {
                    snapshot.lock().unwrap().error =
                        Some("Position exceeds supported range".into());
                    continue;
                };
                request_seek_seq = request_seek_seq
                    .max(snapshot.lock().unwrap().seek_seq)
                    .saturating_add(1);
                let status = snapshot.lock().unwrap().status;
                let should_rebuild =
                    matches!(status, PlaybackStatus::Ended | PlaybackStatus::Error)
                        || active
                            .as_ref()
                            .is_some_and(|stream| stream.decoder.snapshot().worker_finished);
                if should_rebuild {
                    if let Some(request) = last_request.clone() {
                        generation = generation.wrapping_add(1);
                        if let Some(output) = output.as_ref() {
                            output.clear();
                        }
                        if let Some(previous) = active.take() {
                            publish_promotion(
                                &preload_view,
                                &previous,
                                AudioPreloadState::Cancelled,
                                Some(AudioPreloadError::new(
                                    AudioPreloadErrorKind::Cancelled,
                                    "Audio promotion replaced by seek",
                                )),
                            );
                            previous.cancel();
                            retired.push(previous);
                        }
                        desired_playing = false;
                        {
                            let mut state = snapshot.lock().unwrap();
                            state.generation = generation;
                            state.error = None;
                            state.failure = None;
                        }
                        let cancel = Arc::new(AtomicBool::new(false));
                        match transport.open(request.clone(), cancel).and_then(|lease| {
                            let control = lease.control.clone();
                            DecoderHandle::start(
                                lease,
                                DecoderConfig {
                                    processing: processing_request.0,
                                    processing_sequence: processing_request.1,
                                    initially_paused: true,
                                    ..Default::default()
                                },
                            )
                            .map(|decoder| ActiveStream {
                                decoder,
                                control,
                                request_url_sha256: request.identity.url_sha256.clone(),
                                attached: false,
                                committed_seek: 0,
                                ready: None,
                                promotion: None,
                            })
                            .map_err(anyhow::Error::from)
                        }) {
                            Ok(stream) => {
                                active = Some(stream);
                                pending_seek = Some((request_seek_seq, position));
                                let mut state = snapshot.lock().unwrap();
                                state.generation = generation;
                                state.status = PlaybackStatus::Loading;
                                state.seek_pending = true;
                            }
                            Err(error) => {
                                let failure = error
                                    .downcast_ref::<DecodeError>()
                                    .map(|decoded| {
                                        PlaybackFailure::decode(
                                            decoded,
                                            &request.identity.url_sha256,
                                        )
                                    })
                                    .unwrap_or_else(|| {
                                        PlaybackFailure::stream_open(
                                            &error,
                                            transport.cache(),
                                            request.identity.url_sha256.clone(),
                                        )
                                    });
                                let mut state = snapshot.lock().unwrap();
                                state.error =
                                    Some(format!("Cannot rebuild audio for seek: {error:#}"));
                                state.failure = Some(failure);
                            }
                        }
                    }
                } else if let Some(stream) = &active {
                    if !stream.attached {
                        pending_seek = Some((request_seek_seq, position));
                        snapshot.lock().unwrap().seek_pending = true;
                    } else if let Err(error) = stream.decoder.seek(request_seek_seq, position) {
                        snapshot.lock().unwrap().error = Some(format!("Seek failed: {error}"));
                    } else {
                        snapshot.lock().unwrap().seek_pending = true;
                    }
                }
            }
            Ok(Command::Volume(volume)) => {
                if volume.is_finite() {
                    let volume = volume.clamp(0.0, 1.0);
                    snapshot.lock().unwrap().volume = volume;
                    if let Some(output) = output.as_ref() {
                        output.set_gain(volume);
                    }
                }
            }
            Ok(Command::Stop) => {
                *supply.lock().unwrap_or_else(|error| error.into_inner()) = None;
                discard_prepared(&mut speculative, &mut retired, &preload_view, None);
                generation = generation.wrapping_add(1);
                if let Some(output) = output.as_ref() {
                    output.clear();
                }
                if let Some(previous) = active.take() {
                    publish_promotion(
                        &preload_view,
                        &previous,
                        AudioPreloadState::Cancelled,
                        Some(AudioPreloadError::new(
                            AudioPreloadErrorKind::Cancelled,
                            "Audio promotion stopped",
                        )),
                    );
                    previous.cancel();
                    retired.push(previous);
                }
                last_request = None;
                pending_seek = None;
                deferred = None;
                desired_playing = false;
                let mut state = snapshot.lock().unwrap();
                *state = PlaybackSnapshot {
                    volume: state.volume,
                    generation,
                    seek_seq: state.seek_seq,
                    ..Default::default()
                };
            }
            Ok(Command::ClearError) => {
                let mut state = snapshot.lock().unwrap();
                state.error = None;
                state.failure = None;
            }
            #[cfg(test)]
            Ok(Command::DeviceError {
                device_token,
                error,
            }) => {
                if output
                    .as_ref()
                    .is_some_and(|output| output.token() == device_token)
                {
                    fail_with(
                        &snapshot,
                        error,
                        PlaybackFailure::output(
                            None,
                            active
                                .as_ref()
                                .map(|stream| stream.request_url_sha256.clone()),
                        ),
                    );
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => (),
        }
        poll_prepared(&mut speculative, &mut retired, &preload_view, generation);
        if active.as_ref().is_some_and(|stream| {
            stream
                .promotion
                .as_ref()
                .is_some_and(|promotion| promotion.permit.cancel.is_cancelled())
        }) {
            if let Some(stream) = active.as_ref() {
                stream.cancel();
            }
            if let Some(output) = output.as_ref() {
                output.clear();
            }
            deferred = Some(ControlCommand {
                command: Command::Stop,
                intent_revision: applied_intent_revision,
            });
            continue;
        }
        if let Some(current) = output.as_mut() {
            current.reap();
            let (effect_settings, effect_sequence, impulse) = {
                let state = spatial.lock().unwrap_or_else(|error| error.into_inner());
                (
                    state.snapshot.desired.clone(),
                    state.snapshot.requested_sequence,
                    state.impulse.clone(),
                )
            };
            let effects_result =
                current.configure_effects(effect_settings, effect_sequence, impulse);
            {
                let mut state = spatial.lock().unwrap_or_else(|error| error.into_inner());
                if state.snapshot.requested_sequence == effect_sequence {
                    state.spatial_applied_sequence = current.spatial_applied_sequence();
                    let mut observed = current.effects_snapshot();
                    observed.desired = state.snapshot.desired.clone();
                    observed.requested_sequence = effect_sequence;
                    state.snapshot = observed;
                    state.snapshot.error = effects_result
                        .err()
                        .map(|error| format!("{error:#}").chars().take(4096).collect());
                    if state.snapshot.error.is_some() {
                        state.snapshot.status = crate::domain::audio_effects::EffectsStatus::Failed;
                    }
                }
            }
            let (analysis_enabled, analysis_sequence) = *analysis_configuration
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            current.set_analysis_enabled(analysis_enabled, analysis_sequence);
            let (generation, seek_seq) = {
                let snapshot = snapshot.lock().unwrap_or_else(|error| error.into_inner());
                (snapshot.generation, snapshot.seek_seq)
            };
            *analysis.lock().unwrap_or_else(|error| error.into_inner()) =
                current.analysis_snapshot().map(|frame| {
                    (
                        generation,
                        applied_intent_revision,
                        seek_seq,
                        analysis_sequence,
                        frame,
                    )
                });
            project_equalizer(&equalizer, current.equalizer_snapshot());
            project_processing(&processing, current.processing_snapshot());
            if current.take_device_change_pause() {
                desired_playing = false;
                device_pause_revision.store(applied_intent_revision + 1, Ordering::Release);
                current.set_paused(true);
                if let Some(stream) = active.as_ref() {
                    stream.decoder.set_paused(true);
                }
            }
            *output_snapshot
                .lock()
                .unwrap_or_else(|error| error.into_inner()) = current.snapshot();
            *output_render_timing
                .lock()
                .unwrap_or_else(|error| error.into_inner()) = current.memory_render_timing();
            if let Some(error) = current.poll_error() {
                let request_url_sha256 = active
                    .as_ref()
                    .map(|stream| stream.request_url_sha256.clone());
                current.clear();
                if let Some(stream) = active.take() {
                    publish_promotion(
                        &preload_view,
                        &stream,
                        AudioPreloadState::Failed,
                        Some(AudioPreloadError::new(
                            AudioPreloadErrorKind::Transport,
                            "Prepared audio output failed",
                        )),
                    );
                    stream.cancel();
                    retired.push(stream);
                }
                if let Some(mut failed) = output.take() {
                    let joined = failed.shutdown(Duration::ZERO);
                    let mut state = failed.snapshot();
                    if !joined {
                        state.state.status =
                            crate::domain::audio_output::AudioOutputStatus::Unavailable;
                    }
                    *output_snapshot
                        .lock()
                        .unwrap_or_else(|error| error.into_inner()) = state;
                    retired_outputs.push(failed);
                }
                fail_with(
                    &snapshot,
                    format!("Audio output failed: {error}"),
                    PlaybackFailure::output(Some(error), request_url_sha256),
                );
            }
        }
        if let Some(stream) = active.as_mut() {
            if !stream.attached
                && let Some(ready) = stream
                    .ready
                    .take()
                    .map(Ok)
                    .or_else(|| stream.decoder.poll_ready())
            {
                match ready {
                    Err(error) => {
                        publish_promotion(
                            &preload_view,
                            stream,
                            AudioPreloadState::Failed,
                            Some(
                                AudioPreloadError::new(
                                    AudioPreloadErrorKind::Decode,
                                    "Prepared audio decoding failed",
                                )
                                .with_media(error.media.clone()),
                            ),
                        );
                        stream.cancel();
                        fail_with(
                            &snapshot,
                            format!("音频解码失败：{error}"),
                            PlaybackFailure::decode(&error, &stream.request_url_sha256),
                        );
                    }
                    Ok(ready) => {
                        if output.is_none() {
                            match open_output(
                                &output_mode,
                                &output_preferences,
                                equalizer_request,
                                processing_request,
                                effect_request(&spatial),
                            ) {
                                Ok(next) => output = Some(next),
                                Err(error) => {
                                    publish_promotion(
                                        &preload_view,
                                        stream,
                                        AudioPreloadState::Failed,
                                        Some(AudioPreloadError::new(
                                            AudioPreloadErrorKind::Transport,
                                            "Prepared audio output could not open",
                                        )),
                                    );
                                    stream.cancel();
                                    fail_with(
                                        &snapshot,
                                        format!("Cannot open audio output: {error:#}"),
                                        PlaybackFailure::output(
                                            error.downcast_ref::<OutputError>().copied(),
                                            Some(stream.request_url_sha256.clone()),
                                        ),
                                    );
                                    continue;
                                }
                            }
                        }
                        let current = output.as_mut().unwrap();
                        if !current.is_ready() {
                            let device_state = current.snapshot();
                            *output_snapshot
                                .lock()
                                .unwrap_or_else(|error| error.into_inner()) = device_state.clone();
                            if device_state.state.status
                                == crate::domain::audio_output::AudioOutputStatus::Unavailable
                            {
                                fail_unavailable_output(
                                    &mut active,
                                    &mut retired,
                                    &snapshot,
                                    &preload_view,
                                    device_state.state.last_error,
                                );
                            } else {
                                stream.ready = Some(ready);
                            }
                            continue;
                        }
                        current.set_paused(true);
                        current.set_gain(snapshot.lock().unwrap().volume);
                        if let Err(error) = current.attach(ready.source) {
                            publish_promotion(
                                &preload_view,
                                stream,
                                AudioPreloadState::Failed,
                                Some(AudioPreloadError::new(
                                    AudioPreloadErrorKind::Transport,
                                    "Prepared PCM output attachment failed",
                                )),
                            );
                            stream.cancel();
                            fail_with(
                                &snapshot,
                                format!("Cannot attach PCM output: {error:#}"),
                                PlaybackFailure::output(
                                    error.downcast_ref::<OutputError>().copied(),
                                    Some(stream.request_url_sha256.clone()),
                                ),
                            );
                            continue;
                        }
                        stream.decoder.set_paused(!desired_playing);
                        if let Some((seq, position)) = pending_seek.take()
                            && let Err(error) = stream.decoder.seek(seq, position)
                        {
                            snapshot.lock().unwrap().error = Some(format!("Seek failed: {error}"));
                        }
                        output.as_ref().unwrap().set_paused(!desired_playing);
                        {
                            let mut state = snapshot.lock().unwrap();
                            state.duration_known = ready.duration.is_some();
                            state.duration_seconds =
                                ready.duration.map_or(0.0, |d| d.as_secs_f64());
                            state.status = if desired_playing {
                                PlaybackStatus::Playing
                            } else {
                                PlaybackStatus::Paused
                            };
                        }
                        stream.attached = true;
                    }
                }
            }
            let decoded = stream.decoder.snapshot();
            let track_id = snapshot
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .track_id
                .clone();
            *supply.lock().unwrap_or_else(|error| error.into_inner()) =
                Some(supply::AudioSupplySnapshot::observe(
                    generation,
                    track_id,
                    &decoded,
                    &stream.control.status(),
                ));
            if decoded.status == DecoderStatus::Error {
                publish_promotion(
                    &preload_view,
                    stream,
                    AudioPreloadState::Failed,
                    Some(
                        AudioPreloadError::new(
                            AudioPreloadErrorKind::Decode,
                            "Prepared audio decoding failed",
                        )
                        .with_media(decoded.error.as_ref().and_then(|error| error.media.clone())),
                    ),
                );
                stream.cancel();
                if let Some(current) = output.as_ref() {
                    current.set_paused(true);
                }
                let failure = decoded
                    .error
                    .as_ref()
                    .map(|error| PlaybackFailure::decode(error, &stream.request_url_sha256))
                    .unwrap_or_else(|| PlaybackFailure {
                        stage: PlaybackFailureStage::Decode,
                        decode_kind: None,
                        media: None,
                        output_kind: None,
                        request_url_sha256: Some(stream.request_url_sha256.clone()),
                    });
                fail_with(
                    &snapshot,
                    format!(
                        "音频解码失败：{}",
                        decoded
                            .error
                            .map(|error| error.to_string())
                            .unwrap_or_else(|| "unknown decoding error".into())
                    ),
                    failure,
                );
            } else if stream.attached && snapshot.lock().unwrap().status != PlaybackStatus::Error {
                let mut state = snapshot.lock().unwrap();
                state.position_seconds = decoded.pcm.position_seconds;
                state.has_presented_media |=
                    output.as_ref().is_some_and(Output::has_presented_media);
                let applied_processing = output.as_ref().map(Output::processing_snapshot);
                if let Some(applied) = applied_processing
                    .as_ref()
                    .filter(|state| state.sample_rate.is_some())
                {
                    state.rate = applied.applied.playback_rate;
                }
                if decoded.seek_seq > stream.committed_seek {
                    stream.committed_seek = decoded.seek_seq;
                    state.seek_seq = decoded.seek_seq;
                    state.seek_pending = false;
                    state.error = None;
                    state.failure = None;
                }
                let ended = decoded.pcm.normal_eof && output.as_ref().is_some_and(Output::ended);
                if stream
                    .promotion
                    .as_ref()
                    .is_some_and(|promotion| !promotion.completed)
                    && (!desired_playing || decoded.pcm.consumed_frames > 0 || ended)
                {
                    publish_promotion(&preload_view, stream, AudioPreloadState::Promoted, None);
                    if let Some(promotion) = &mut stream.promotion {
                        promotion.completed = true;
                    }
                }
                state.buffering = desired_playing
                    && !ended
                    && ((decoded.pcm.queued_frames == 0
                        && !output.as_ref().is_some_and(Output::has_pending_content))
                        || output.as_ref().is_some_and(Output::processing_buffering)
                        || decoded.status == DecoderStatus::Seeking
                        || !output.as_ref().is_some_and(Output::is_ready));
                state.status = if ended {
                    PlaybackStatus::Ended
                } else if !desired_playing {
                    PlaybackStatus::Paused
                } else if state.buffering {
                    PlaybackStatus::Buffering
                } else {
                    PlaybackStatus::Playing
                };
                if state.status == PlaybackStatus::Ended && state.duration_known {
                    state.position_seconds = state.duration_seconds;
                }
            }
        }
    }
    if let Some(current) = output.as_ref() {
        current.clear();
    }
    *supply.lock().unwrap_or_else(|error| error.into_inner()) = None;
    discard_prepared(&mut speculative, &mut retired, &preload_view, None);
    if let Some(stream) = active.take() {
        publish_promotion(
            &preload_view,
            &stream,
            AudioPreloadState::Cancelled,
            Some(AudioPreloadError::new(
                AudioPreloadErrorKind::Cancelled,
                "Audio promotion owner closed",
            )),
        );
        stream.cancel();
        retired.push(stream);
    }
    for stream in &retired {
        stream.cancel();
    }
    transport.cache().begin_shutdown();
    if let Some(current) = output.take() {
        retired_outputs.push(current);
    }
    while !retired_outputs.is_empty() {
        retired_outputs.retain_mut(|current| {
            let joined = current.shutdown(Duration::from_millis(20));
            *output_snapshot
                .lock()
                .unwrap_or_else(|error| error.into_inner()) = current.snapshot();
            !joined
        });
        if !retired_outputs.is_empty() {
            thread::park_timeout(Duration::from_millis(2));
        }
    }
    // Shutdown callers time out without discarding the owned workers. The engine
    // remains responsible until cancellation actually completes.
    while !retired.is_empty() {
        retired.retain_mut(|stream| !stream.finished());
        if !retired.is_empty() {
            thread::sleep(Duration::from_millis(2));
        }
    }
    while !transport.shutdown(Duration::from_millis(20)) {
        thread::park_timeout(Duration::from_millis(2));
    }
    // Keep the cache endpoint and engine alive through the I/O worker's actual
    // join, including release of its directory ownership and thread teardown.
    while !transport.cache().shutdown(Duration::from_millis(20)) {
        thread::park_timeout(Duration::from_millis(2));
    }
    *shared_cache.lock().unwrap_or_else(|e| e.into_inner()) = None;
    let mut state = snapshot.lock().unwrap();
    state.generation = state.generation.wrapping_add(1);
    state.status = PlaybackStatus::Idle;
    state.track_id = None;
    state.position_seconds = 0.0;
    state.duration_seconds = 0.0;
    state.duration_known = false;
    state.buffering = false;
    state.seek_pending = false;
    state.failure = None;
}

fn fail(snapshot: &Mutex<PlaybackSnapshot>, error: String) {
    let mut state = snapshot.lock().unwrap();
    state.status = PlaybackStatus::Error;
    state.error = Some(error);
    state.failure = None;
}

fn fail_with(snapshot: &Mutex<PlaybackSnapshot>, error: String, failure: PlaybackFailure) {
    let mut state = snapshot.lock().unwrap();
    state.status = PlaybackStatus::Error;
    state.error = Some(error);
    state.failure = Some(failure);
}

#[cfg(test)]
fn validate_audio_url(url: &str) -> Result<Url> {
    if url.len() > 2048 {
        bail!("音频 URL 超过 2048 字符");
    }
    let url = Url::parse(url).context("音频 URL 无效")?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        bail!("只支持 HTTP(S) 音频 URL");
    }
    if !url.username().is_empty() || url.password().is_some() {
        bail!("音频 URL 不得包含账号或密码");
    }
    Ok(url)
}

#[cfg(test)]
fn fetch_audio(url: &str, cancelled: &AtomicBool) -> Result<Vec<u8>> {
    let url = validate_audio_url(url)?;
    if cancelled.load(Ordering::Acquire) {
        bail!("请求已取消");
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("无法启动音频网络运行时")?;
    runtime.block_on(fetch_audio_async(url, cancelled))
}

#[cfg(test)]
async fn fetch_audio_async(url: Url, cancelled: &AtomicBool) -> Result<Vec<u8>> {
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(20))
        // A redirect is a different destination and requires a fresh UI approval.
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .user_agent(concat!("LX-Native/", env!("CARGO_PKG_VERSION")))
        .build()?;
    let mut response = await_audio_io(client.get(url).send(), cancelled)
        .await
        .context("HTTP 音频请求失败")?;
    if !response.status().is_success() {
        bail!("音频服务器返回 HTTP {}", response.status().as_u16());
    }
    if response
        .content_length()
        .is_some_and(|length| length > MAX_AUDIO_BYTES as u64)
    {
        bail!("音频超过 64 MiB 限制");
    }
    let mut bytes = Vec::new();
    loop {
        let Some(chunk) = await_audio_io(response.chunk(), cancelled)
            .await
            .context("读取音频数据失败")?
        else {
            break;
        };
        if bytes.len() + chunk.len() > MAX_AUDIO_BYTES {
            bail!("音频超过 64 MiB 限制");
        }
        bytes.extend_from_slice(&chunk);
    }
    if bytes.is_empty() {
        bail!("服务器返回空音频");
    }
    Ok(bytes)
}

#[cfg(test)]
async fn await_audio_io<T>(
    operation: impl std::future::Future<Output = std::result::Result<T, reqwest::Error>>,
    cancelled: &AtomicBool,
) -> Result<T> {
    let mut operation = Box::pin(operation);
    loop {
        if cancelled.load(Ordering::Acquire) {
            bail!("请求已取消");
        }
        match tokio::time::timeout(Duration::from_millis(20), operation.as_mut()).await {
            Ok(result) => return result.context("音频网络传输失败"),
            Err(_) => continue,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::Catalog;
    use rodio::{Decoder, Source};
    use std::io::Cursor;
    use std::time::Instant;

    #[test]
    fn unavailable_output_retires_actual_decoder_without_reclassifying_its_cancellation() {
        let catalog = Catalog::new().unwrap();
        let transport = AudioTransport::new(TransportOptions::default()).unwrap();
        let request = ApprovedAudioRequest::approved_url(&catalog.tracks()[0].source_url).unwrap();
        let request_hash = request.identity.url_sha256.clone();
        let lease = transport
            .open(request, Arc::new(AtomicBool::new(false)))
            .unwrap();
        let control = lease.control.clone();
        let decoder = DecoderHandle::start(lease, DecoderConfig::default()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(6);
        let ready = loop {
            if let Some(result) = decoder.poll_ready() {
                break result.unwrap();
            }
            assert!(
                Instant::now() < deadline,
                "actual decoder did not become ready"
            );
            thread::sleep(Duration::from_millis(5));
        };
        let mut active = Some(ActiveStream {
            decoder,
            control,
            request_url_sha256: request_hash.clone(),
            attached: false,
            committed_seek: 0,
            ready: None,
            promotion: None,
        });
        let mut retired = Vec::new();
        let snapshot = Mutex::new(PlaybackSnapshot::default());
        let preload = Arc::new(Mutex::new(View::default()));
        fail_unavailable_output(
            &mut active,
            &mut retired,
            &snapshot,
            &preload,
            Some(crate::domain::audio_output::AudioOutputError::DeviceNotAvailable),
        );
        drop(ready);
        assert!(active.is_none());
        let deadline = Instant::now() + Duration::from_secs(5);
        while !retired.is_empty() {
            retired.retain_mut(|stream| !stream.finished());
            assert!(
                Instant::now() < deadline,
                "cancelled decoder owners did not join"
            );
            thread::sleep(Duration::from_millis(5));
        }
        let state = snapshot.lock().unwrap();
        assert_eq!(state.status, PlaybackStatus::Error);
        assert!(state.error.as_ref().unwrap().contains("音频输出设备"));
        let failure = state.failure.as_ref().unwrap();
        assert_eq!(failure.stage, PlaybackFailureStage::Output);
        assert_eq!(failure.output_kind, Some(OutputError::DeviceNotAvailable));
        assert_eq!(failure.decode_kind, None);
        assert_eq!(
            failure.request_url_sha256.as_deref(),
            Some(request_hash.as_str())
        );
        assert!(!failure.requires_url_refresh());
        assert!(transport.shutdown(Duration::from_secs(5)));
    }

    #[test]
    fn distinct_actual_response_owner_is_not_replaced_by_equal_receipt_metadata() {
        use crate::domain::audio_effects::{EffectsSettings, ImpulseResourceId};
        let make = |sample| {
            let mut builder = convolution::ImpulseBuilder::prepare(22_050, 1, 1).unwrap();
            builder.push_frame(&[sample]).unwrap();
            effects_factory::OwnedImpulse::from_validated(
                builder.finish().unwrap(),
                ImpulseResourceId::Imported("producer-response".into()),
                "c".repeat(64),
            )
            .unwrap()
        };
        let player = AudioPlayer::memory();
        let first = make(1.0);
        let old = Arc::downgrade(&first);
        let mut settings = EffectsSettings::default();
        settings.convolution.impulse = Some(first.source_id().clone());
        player
            .set_effects(settings.clone(), Some(first.clone()))
            .unwrap();
        let sequence = player.effects_snapshot().requested_sequence;
        player
            .set_effects(settings.clone(), Some(first.clone()))
            .unwrap();
        assert_eq!(player.effects_snapshot().requested_sequence, sequence);
        drop(first);
        player.set_effects(settings, Some(make(0.25))).unwrap();
        assert_eq!(player.effects_snapshot().requested_sequence, sequence + 1);
        assert!(
            old.upgrade().is_none(),
            "The earlier actual PCM owner must retire even when metadata matches"
        );
        player.shutdown(Duration::from_secs(10)).unwrap();
    }

    #[test]
    fn actual_player_uses_owned_impulse_with_spatial_settings_and_retires_it_after_shutdown() {
        use crate::domain::audio_effects::{EffectsSettings, EffectsStatus, ImpulseResourceId};
        let catalog = Catalog::new().unwrap();
        let player = AudioPlayer::memory();
        let mut builder = convolution::ImpulseBuilder::prepare(22_050, 1, 3).unwrap();
        builder.push_frame(&[1.0]).unwrap();
        builder.push_frame(&[0.5]).unwrap();
        builder.push_frame(&[0.25]).unwrap();
        let id = ImpulseResourceId::Imported("player-selected-response".into());
        let impulse = effects_factory::OwnedImpulse::from_validated(
            builder.finish().unwrap(),
            id.clone(),
            "b".repeat(64),
        )
        .unwrap();
        let actual = Arc::downgrade(&impulse);
        let mut settings = EffectsSettings::default();
        settings.convolution.impulse = Some(id);
        settings.convolution.main_gain = 0.0;
        settings.convolution.send_gain = 1.0;
        assert!(player.set_effects(settings.clone(), None).is_err());
        assert_eq!(
            player.effects_snapshot().desired,
            EffectsSettings::default()
        );
        player.set_effects(settings, Some(impulse)).unwrap();
        player
            .set_spatial(spatial::SpatialSettings {
                enabled: true,
                radius: 0.5,
                speed: 25,
            })
            .unwrap();
        let desired = player.effects_snapshot();
        assert!(desired.desired.convolution.impulse.is_some());
        assert!(desired.desired.spatial.enabled);
        assert_eq!(desired.applied_sequence, 0);
        let track = &catalog.tracks()[0];
        player.load(&track.id, &track.source_url, true);
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let current = player.effects_snapshot();
            if current.status == EffectsStatus::Ready
                && current.applied_sequence == desired.requested_sequence
            {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "Real prepared effect was not presented: {current:?}; {:?}",
                player.snapshot()
            );
            thread::sleep(Duration::from_millis(10));
        }
        let presented = player.effects_snapshot();
        assert!(presented.latency_frames >= 512 && presented.owned_bytes > 0);
        assert!(player.rendered_pcm().unwrap().peak > 1e-7);
        assert!(actual.upgrade().is_some());
        player.shutdown(Duration::from_secs(10)).unwrap();
        assert!(
            actual.upgrade().is_none(),
            "The actual selected IR owner survived completed shutdown"
        );
        assert_eq!(player.effects_snapshot().owned_bytes, 0);
        assert!(
            player
                .set_effects(EffectsSettings::default(), None)
                .is_err()
        );
    }

    #[test]
    fn url_validation_rejects_non_http_and_credentials() {
        assert!(validate_audio_url("file:///C:/private.wav").is_err());
        assert!(validate_audio_url("data:audio/wav;base64,x").is_err());
        assert!(validate_audio_url("https://user:secret@example.com/a.wav").is_err());
        assert!(validate_audio_url("http://127.0.0.1:1234/tone-1.wav").is_ok());
    }

    #[test]
    fn downloader_reads_legal_fixture_and_reports_http_failure() {
        let catalog = Catalog::new().unwrap();
        let cancelled = AtomicBool::new(false);
        let bytes = fetch_audio(&catalog.tracks()[0].source_url, &cancelled).unwrap();
        let decoder = Decoder::try_from(Cursor::new(bytes)).unwrap();
        assert!((decoder.total_duration().unwrap().as_secs_f64() - 45.0).abs() < 0.01);
        assert!(
            fetch_audio(&format!("{}/missing.wav", catalog.base_url()), &cancelled)
                .unwrap_err()
                .to_string()
                .contains("404")
        );
        cancelled.store(true, Ordering::Release);
        assert!(fetch_audio(&catalog.tracks()[1].source_url, &cancelled).is_err());
    }

    #[test]
    fn empty_engine_is_responsive_and_stop_cancels_failed_load() {
        let player = AudioPlayer::new();
        assert_eq!(player.snapshot().status, PlaybackStatus::Idle);
        player.set_volume(2.0);
        wait_until(&player, |state| state.volume == 1.0);
        player.load("invalid", "data:audio/wav,invalid", true);
        wait_until(&player, |state| state.status == PlaybackStatus::Error);
        player.stop();
        wait_until(&player, |state| {
            state.status == PlaybackStatus::Idle && state.track_id.is_none()
        });
    }

    #[test]
    fn cancelled_completion_and_stale_device_error_cannot_restart_stopped_player() {
        let catalog = Catalog::new().unwrap();
        let player = AudioPlayer::new();
        player.load("cancelled", &catalog.tracks()[0].source_url, true);
        player.stop();
        player.set_volume(0.2);
        wait_until(&player, |state| {
            state.volume == 0.2 && state.status == PlaybackStatus::Idle
        });
        // This is a callback belonging to an already discarded output device.
        player.send(Command::DeviceError {
            device_token: 99,
            error: "stale device".into(),
        });
        thread::sleep(Duration::from_millis(150));
        assert_eq!(player.snapshot().status, PlaybackStatus::Idle);
        assert!(player.snapshot().track_id.is_none());
    }

    fn memory_player() -> (AudioPlayer, Arc<Mutex<RenderedPcm>>) {
        let observation = Arc::new(Mutex::new(RenderedPcm::default()));
        let player = AudioPlayer::with_output(OutputMode::Memory(observation.clone()));
        (player, observation)
    }

    fn wait_for_pcm(observation: &Mutex<RenderedPcm>, condition: impl Fn(&RenderedPcm) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(6);
        loop {
            {
                let pcm = observation.lock().unwrap();
                if pcm.batches > 0 && condition(&pcm) {
                    return;
                }
                assert!(
                    Instant::now() < deadline,
                    "PCM check timed out: rms={}, peak={}, batches={}",
                    pcm.rms,
                    pcm.peak,
                    pcm.batches
                );
            }
            thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn production_engine_with_memory_output_controls_real_decoded_pcm() {
        let catalog = Catalog::new().unwrap();
        let tracks = catalog.tracks();
        let (player, pcm) = memory_player();

        // Autoplay=false must stay paused while the output callback renders silence.
        player.load(&tracks[0].id, &tracks[0].source_url, false);
        wait_until(&player, |state| state.status == PlaybackStatus::Paused);
        assert!((player.snapshot().duration_seconds - 45.0).abs() < 0.02);
        wait_for_pcm(&pcm, |rendered| rendered.peak == 0.0);
        thread::sleep(Duration::from_millis(120));
        assert!(player.snapshot().position_seconds < 0.04);

        player.play();
        wait_until(&player, |state| {
            state.status == PlaybackStatus::Playing && state.position_seconds > 0.65
        });
        wait_for_pcm(&pcm, |rendered| {
            rendered.rms > 0.005 && rendered.peak > 0.01
        });

        player.pause();
        wait_until(&player, |state| state.status == PlaybackStatus::Paused);
        wait_for_pcm(&pcm, |rendered| rendered.peak == 0.0);
        let paused = player.snapshot().position_seconds;
        thread::sleep(Duration::from_millis(200));
        assert!((player.snapshot().position_seconds - paused).abs() < 0.05);

        // Seeking a paused decoder changes its position without producing PCM sound.
        player.seek(10.0);
        wait_until(&player, |state| {
            state.status == PlaybackStatus::Paused && (state.position_seconds - 10.0).abs() < 0.08
        });
        assert!(player.snapshot().error.is_none());
        wait_for_pcm(&pcm, |rendered| rendered.peak == 0.0);
        player.play();
        wait_until(&player, |state| {
            state.status == PlaybackStatus::Playing && state.position_seconds > 10.15
        });
        wait_for_pcm(&pcm, |rendered| rendered.peak > 0.01);

        // Volume zero must zero the actual mixed PCM while time still advances.
        player.set_volume(0.0);
        wait_until(&player, |state| state.volume == 0.0);
        wait_for_pcm(&pcm, |rendered| rendered.rms == 0.0 && rendered.peak == 0.0);
        let muted_position = player.snapshot().position_seconds;
        wait_until(&player, |state| {
            state.position_seconds > muted_position + 0.15
        });
        player.set_volume(0.3);
        wait_until(&player, |state| state.volume == 0.3);
        wait_for_pcm(&pcm, |rendered| rendered.rms > 0.004);

        player.toggle();
        wait_until(&player, |state| state.status == PlaybackStatus::Paused);
        player.toggle();
        wait_until(&player, |state| state.status == PlaybackStatus::Playing);
        player.load(&tracks[1].id, &tracks[1].source_url, true);
        wait_until(&player, |state| {
            state.status == PlaybackStatus::Playing
                && state.track_id.as_deref() == Some(tracks[1].id.as_str())
        });
        assert!((player.snapshot().duration_seconds - 52.0).abs() < 0.02);
        assert!(
            player.snapshot().position_seconds < 1.0,
            "switch must replace the old track position"
        );

        player.load(
            "missing",
            format!("{}/missing.wav", catalog.base_url()),
            true,
        );
        wait_until(&player, |state| state.status == PlaybackStatus::Error);
        assert!(player.snapshot().error.unwrap().contains("404"));
        wait_for_pcm(&pcm, |rendered| rendered.peak == 0.0);
        player.load(&tracks[2].id, &tracks[2].source_url, true);
        wait_until(&player, |state| {
            state.status == PlaybackStatus::Playing
                && state.track_id.as_deref() == Some(tracks[2].id.as_str())
        });

        // HTTP success with a non-audio body must fail decoder initialization.
        player.load("not-audio", catalog.manifest_url(), true);
        wait_until(&player, |state| state.status == PlaybackStatus::Error);
        assert!(player.snapshot().error.unwrap().contains("音频解码失败"));
        player.load(&tracks[0].id, &tracks[0].source_url, true);
        wait_until(&player, |state| {
            state.status == PlaybackStatus::Playing
                && state.track_id.as_deref() == Some(tracks[0].id.as_str())
        });
        player.stop();
        wait_until(&player, |state| {
            state.status == PlaybackStatus::Idle && state.track_id.is_none()
        });
        wait_for_pcm(&pcm, |rendered| rendered.peak == 0.0);
    }

    #[test]
    fn production_engine_with_memory_output_finishes_replays_and_seeks_after_end() {
        let catalog = Catalog::new().unwrap();
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("short-original.wav");
        std::fs::write(&path, crate::catalog::synthesized_wav(330.0, 1.0)).unwrap();
        let track = catalog.import_local(path).unwrap();
        let (player, _pcm) = memory_player();
        player.load(&track.id, &track.source_url, true);
        wait_until(&player, |state| {
            state.status == PlaybackStatus::Playing && state.position_seconds > 0.05
        });
        player.seek(0.85);
        wait_until(&player, |state| state.status == PlaybackStatus::Ended);
        assert!((player.snapshot().position_seconds - 1.0).abs() < 0.02);

        player.play();
        wait_until(&player, |state| {
            state.status == PlaybackStatus::Playing
                && state.position_seconds > 0.05
                && state.position_seconds < 0.5
        });
        // Known duration clamps a very large position to the track end.
        player.seek(f64::MAX);
        wait_until(&player, |state| state.status == PlaybackStatus::Ended);
        player.seek(0.4);
        wait_until(&player, |state| {
            state.status == PlaybackStatus::Paused && (state.position_seconds - 0.4).abs() < 0.08
        });
        assert!(player.snapshot().error.is_none());
        player.play();
        wait_until(&player, |state| {
            state.status == PlaybackStatus::Playing && state.position_seconds > 0.5
        });
        player.stop();
        wait_until(&player, |state| state.status == PlaybackStatus::Idle);
    }

    fn wait_until(player: &AudioPlayer, condition: impl Fn(&PlaybackSnapshot) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(6);
        loop {
            let state = player.snapshot();
            if condition(&state) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "timed out, last state: {state:?}"
            );
            thread::sleep(Duration::from_millis(30));
        }
    }

    /// Run explicitly on a machine with an audio output device. Regular tests need no hardware.
    #[test]
    #[ignore = "requires real audio output; plays quiet original synthetic tones"]
    fn real_audio_device_playback_diagnostic() {
        let catalog = Catalog::new().unwrap();
        let tracks = catalog.tracks();
        let player = AudioPlayer::new();
        player.set_volume(0.25);
        player.load(&tracks[0].id, &tracks[0].source_url, true);
        wait_until(&player, |state| {
            state.status == PlaybackStatus::Playing && state.position_seconds > 0.2
        });
        assert!((player.snapshot().duration_seconds - 45.0).abs() < 0.02);
        player.pause();
        wait_until(&player, |state| state.status == PlaybackStatus::Paused);
        let paused_position = player.snapshot().position_seconds;
        thread::sleep(Duration::from_millis(200));
        assert!((player.snapshot().position_seconds - paused_position).abs() < 0.12);
        player.seek(10.0);
        wait_until(&player, |state| (state.position_seconds - 10.0).abs() < 0.2);
        player.play();
        wait_until(&player, |state| {
            state.status == PlaybackStatus::Playing && state.position_seconds > 10.1
        });
        player.load(&tracks[1].id, &tracks[1].source_url, true);
        wait_until(&player, |state| {
            state.status == PlaybackStatus::Playing
                && state.track_id.as_deref() == Some(tracks[1].id.as_str())
        });
        player.load(
            "missing",
            format!("{}/missing.wav", catalog.base_url()),
            true,
        );
        wait_until(&player, |state| state.status == PlaybackStatus::Error);
        player.load(&tracks[2].id, &tracks[2].source_url, true);
        wait_until(&player, |state| {
            state.status == PlaybackStatus::Playing
                && state.track_id.as_deref() == Some(tracks[2].id.as_str())
        });
        player.stop();
        wait_until(&player, |state| state.status == PlaybackStatus::Idle);
        println!("Real audio device: load/play/pause/seek/volume/switch/error recovery passed");
    }
}

//! Device and Memory output share a bounded, allocation-free PCM render kernel.
//!
//! Sources are prepared on the engine thread. Ownership queues are SPSC;
//! device callbacks return state to the device owner, and Memory callbacks to
//! the engine. Callback invocations never allocate or destroy PCM allocations.

use super::RenderedPcm;
use super::analysis_tap::{AnalysisMailbox, AnalysisTap};
use super::dsp::{Coefficients, Equalizer, EqualizerPlan};
use super::pcm::{MAX_CHANNELS, PcmControl, PcmFormat, PcmSource, ProcessingStamp};
use super::pitch::{PitchFrame, PitchProcessor, PreparedPitch, interpolate_stamp};
use super::{
    convolution::ConvolutionStamp,
    effect_exchange::{EffectsControl, EffectsReclaimer, EffectsRenderExchange, effect_exchange},
    effects::{
        EFFECTS_MIN_REPLAY_FRAMES, EffectsFrame, EffectsFrameKind, EffectsInput, EffectsPitch,
        PreparedEffects,
    },
    effects_factory::{BankRequest, EffectsRecipe, OwnedImpulse},
    spatial::{SpatialPhaseClock, SpatialPlan, SpatialSettings, SpatialStep, SpatialTarget},
};
use anyhow::{Context, Result, bail};
use cpal::{FromSample, SizedSample};
use rtrb::{Consumer, Producer, PushError, RingBuffer};
use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::Duration;

#[path = "output/device_owner.rs"]
mod device_owner;
#[path = "output/live_effects.rs"]
mod live_effects;
use crate::domain::audio_output::{AudioOutputSnapshot, AudioPreferences, OutputSelection};
use crate::domain::equalizer::{EQUALIZER_BAND_COUNT, EqualizerSettings, EqualizerSnapshot};
use crate::domain::playback_processing::{PlaybackProcessingSettings, PlaybackProcessingSnapshot};
use device_owner::DeviceHandle;

const SOURCE_CAPACITY: usize = 8;
const MEMORY_FORMAT: PcmFormat = PcmFormat {
    sample_rate: 22_050,
    channels: 1,
};
const MEMORY_BATCH_FRAMES: usize = 441;

#[derive(Clone, Copy, Debug, Default, serde::Serialize)]
pub struct OutputRenderTiming {
    pub last_render_micros: u64,
    pub max_render_micros: u64,
    pub last_total_micros: u64,
    pub overrun_batches: u64,
}

#[derive(Clone, Copy, Default)]
struct ProcessingRequest {
    settings: PlaybackProcessingSettings,
    sequence: u64,
}

struct ProcessingMailbox {
    producer: OnceLock<thread::ThreadId>,
    version: AtomicU64,
    sequence: AtomicU64,
    rate: AtomicU64,
    preserves: AtomicBool,
    ratio: AtomicU64,
}
impl ProcessingMailbox {
    fn new() -> Self {
        Self {
            producer: OnceLock::new(),
            version: AtomicU64::new(0),
            sequence: AtomicU64::new(0),
            rate: AtomicU64::new(1.0f64.to_bits()),
            preserves: AtomicBool::new(true),
            ratio: AtomicU64::new(1.0f64.to_bits()),
        }
    }
    fn publish(&self, request: ProcessingRequest) {
        debug_assert_eq!(
            *self.producer.get_or_init(|| thread::current().id()),
            thread::current().id()
        );
        let previous = self.version.load(Ordering::SeqCst);
        let Some(next) = previous.checked_add(2) else {
            return;
        };
        self.version.store(previous + 1, Ordering::SeqCst);
        self.sequence.store(request.sequence, Ordering::SeqCst);
        self.rate
            .store(request.settings.playback_rate.to_bits(), Ordering::SeqCst);
        self.preserves
            .store(request.settings.preserves_pitch, Ordering::SeqCst);
        self.ratio
            .store(request.settings.pitch_ratio.to_bits(), Ordering::SeqCst);
        self.version.store(next, Ordering::SeqCst);
    }
    fn read(&self) -> Option<ProcessingRequest> {
        for _ in 0..2 {
            let version = self.version.load(Ordering::SeqCst);
            if version == 0 || version & 1 != 0 {
                continue;
            }
            let request = ProcessingRequest {
                sequence: self.sequence.load(Ordering::SeqCst),
                settings: PlaybackProcessingSettings {
                    playback_rate: f64::from_bits(self.rate.load(Ordering::SeqCst)),
                    preserves_pitch: self.preserves.load(Ordering::SeqCst),
                    pitch_ratio: f64::from_bits(self.ratio.load(Ordering::SeqCst)),
                },
            };
            if version == self.version.load(Ordering::SeqCst) {
                return Some(request);
            }
        }
        None
    }
}

struct ProcessingTelemetry {
    version: AtomicU64,
    sequence: AtomicU64,
    rate: AtomicU64,
    preserves: AtomicBool,
    ratio: AtomicU64,
    latency: AtomicU32,
    remaining: AtomicU32,
    sample_rate: AtomicU32,
    pending: AtomicBool,
    buffering: AtomicBool,
}
impl ProcessingTelemetry {
    fn new() -> Self {
        Self {
            version: AtomicU64::new(0),
            sequence: AtomicU64::new(0),
            rate: AtomicU64::new(1.0f64.to_bits()),
            preserves: AtomicBool::new(true),
            ratio: AtomicU64::new(1.0f64.to_bits()),
            latency: AtomicU32::new(0),
            remaining: AtomicU32::new(0),
            sample_rate: AtomicU32::new(0),
            pending: AtomicBool::new(false),
            buffering: AtomicBool::new(false),
        }
    }
    fn publish(&self, snapshot: PlaybackProcessingSnapshot) {
        let previous = self.version.load(Ordering::SeqCst);
        let Some(next) = previous.checked_add(2) else {
            return;
        };
        self.version.store(previous + 1, Ordering::SeqCst);
        self.sequence
            .store(snapshot.applied_sequence, Ordering::SeqCst);
        self.rate
            .store(snapshot.applied.playback_rate.to_bits(), Ordering::SeqCst);
        self.preserves
            .store(snapshot.applied.preserves_pitch, Ordering::SeqCst);
        self.ratio
            .store(snapshot.applied.pitch_ratio.to_bits(), Ordering::SeqCst);
        self.latency
            .store(snapshot.latency_frames, Ordering::SeqCst);
        self.remaining
            .store(snapshot.transition_remaining_frames, Ordering::SeqCst);
        self.sample_rate
            .store(snapshot.sample_rate.unwrap_or(0), Ordering::SeqCst);
        self.version.store(next, Ordering::SeqCst);
    }
    fn read(&self) -> Option<PlaybackProcessingSnapshot> {
        for _ in 0..4 {
            let version = self.version.load(Ordering::SeqCst);
            if version & 1 != 0 {
                continue;
            }
            let sample_rate = self.sample_rate.load(Ordering::SeqCst);
            let snapshot = PlaybackProcessingSnapshot {
                applied: PlaybackProcessingSettings {
                    playback_rate: f64::from_bits(self.rate.load(Ordering::SeqCst)),
                    preserves_pitch: self.preserves.load(Ordering::SeqCst),
                    pitch_ratio: f64::from_bits(self.ratio.load(Ordering::SeqCst)),
                },
                applied_sequence: self.sequence.load(Ordering::SeqCst),
                sample_rate: (sample_rate != 0).then_some(sample_rate),
                latency_frames: self.latency.load(Ordering::SeqCst),
                transition_remaining_frames: self.remaining.load(Ordering::SeqCst),
                ..PlaybackProcessingSnapshot::default()
            };
            if version == self.version.load(Ordering::SeqCst) {
                return Some(snapshot);
            }
        }
        None
    }
}

#[derive(Clone, Copy, Default)]
struct EqualizerRequest {
    settings: EqualizerSettings,
    sequence: u64,
}

/// One producer publishes fixed atomics; callback reads at most twice.
struct PlanMailbox {
    producer: OnceLock<thread::ThreadId>,
    version: AtomicU64,
    sequence: AtomicU64,
    rate: AtomicU32,
    mask: AtomicU32,
    gains: [AtomicU64; EQUALIZER_BAND_COUNT],
    coefficients: [[AtomicU64; 5]; EQUALIZER_BAND_COUNT],
}

impl PlanMailbox {
    fn new() -> Self {
        Self {
            producer: OnceLock::new(),
            version: AtomicU64::new(0),
            sequence: AtomicU64::new(0),
            rate: AtomicU32::new(0),
            mask: AtomicU32::new(0),
            gains: std::array::from_fn(|_| AtomicU64::new(0)),
            coefficients: std::array::from_fn(|_| std::array::from_fn(|_| AtomicU64::new(0))),
        }
    }
    fn publish(&self, plan: EqualizerPlan) -> bool {
        debug_assert_eq!(
            *self.producer.get_or_init(|| thread::current().id()),
            thread::current().id(),
            "equalizer plan mailbox has one fixed producer"
        );
        let previous = self.version.load(Ordering::SeqCst);
        let Some(next) = previous.checked_add(2) else {
            return false;
        };
        self.version.store(previous + 1, Ordering::SeqCst);
        self.sequence.store(plan.sequence, Ordering::SeqCst);
        self.rate.store(plan.sample_rate, Ordering::SeqCst);
        let mask = plan
            .active_bands
            .iter()
            .enumerate()
            .fold(0, |mask, (band, active)| mask | ((*active as u32) << band));
        self.mask.store(mask, Ordering::SeqCst);
        for band in 0..EQUALIZER_BAND_COUNT {
            self.gains[band].store(plan.settings.gains_db[band].to_bits(), Ordering::SeqCst);
            for coefficient in 0..5 {
                self.coefficients[band][coefficient].store(
                    plan.sections[band].values[coefficient].to_bits(),
                    Ordering::SeqCst,
                );
            }
        }
        self.version.store(next, Ordering::SeqCst);
        true
    }
    fn read(&self) -> Option<EqualizerPlan> {
        for _ in 0..2 {
            let before = self.version.load(Ordering::SeqCst);
            if before == 0 || before & 1 != 0 {
                continue;
            }
            let sequence = self.sequence.load(Ordering::SeqCst);
            let sample_rate = self.rate.load(Ordering::SeqCst);
            let mask = self.mask.load(Ordering::SeqCst);
            let settings = EqualizerSettings {
                gains_db: std::array::from_fn(|band| {
                    f64::from_bits(self.gains[band].load(Ordering::SeqCst))
                }),
            };
            let sections = std::array::from_fn(|band| Coefficients {
                values: std::array::from_fn(|coefficient| {
                    f64::from_bits(self.coefficients[band][coefficient].load(Ordering::SeqCst))
                }),
            });
            if before == self.version.load(Ordering::SeqCst) {
                return Some(EqualizerPlan {
                    sequence,
                    sample_rate,
                    settings,
                    sections,
                    active_bands: std::array::from_fn(|band| mask & (1 << band) != 0),
                });
            }
        }
        None
    }
}

struct EqualizerTelemetry {
    version: AtomicU64,
    sequence: AtomicU64,
    rate: AtomicU32,
    mask: AtomicU32,
    gains: [AtomicU64; EQUALIZER_BAND_COUNT],
    remaining: AtomicU32,
    clipped: AtomicU64,
    non_finite: AtomicU64,
    peak: AtomicU64,
}

impl EqualizerTelemetry {
    fn new() -> Self {
        Self {
            version: AtomicU64::new(0),
            sequence: AtomicU64::new(0),
            rate: AtomicU32::new(0),
            mask: AtomicU32::new(0),
            gains: std::array::from_fn(|_| AtomicU64::new(0)),
            remaining: AtomicU32::new(0),
            clipped: AtomicU64::new(0),
            non_finite: AtomicU64::new(0),
            peak: AtomicU64::new(0),
        }
    }
    fn publish(&self, equalizer: &Equalizer, clipped: u64, non_finite: u64, peak: f64) {
        if clipped != 0 {
            self.clipped.store(
                self.clipped.load(Ordering::Relaxed).saturating_add(clipped),
                Ordering::Release,
            );
        }
        if non_finite != 0 {
            self.non_finite.store(
                self.non_finite
                    .load(Ordering::Relaxed)
                    .saturating_add(non_finite),
                Ordering::Release,
            );
        }
        // Only the unique RenderState callback writes telemetry. Device transfer
        // waits for the old kernel to close; owners and snapshot queries only read.
        let previous = f64::from_bits(self.peak.load(Ordering::Relaxed));
        if peak.is_finite() && peak > previous {
            self.peak.store(peak.to_bits(), Ordering::Release);
        }
        let plan = equalizer.plan();
        let previous = self.version.load(Ordering::SeqCst);
        let Some(next) = previous.checked_add(2) else {
            return;
        };
        self.version.store(previous + 1, Ordering::SeqCst);
        self.sequence.store(plan.sequence, Ordering::SeqCst);
        self.rate.store(plan.sample_rate, Ordering::SeqCst);
        self.mask.store(
            plan.active_bands
                .iter()
                .enumerate()
                .fold(0, |mask, (band, active)| mask | ((*active as u32) << band)),
            Ordering::SeqCst,
        );
        for band in 0..EQUALIZER_BAND_COUNT {
            self.gains[band].store(plan.settings.gains_db[band].to_bits(), Ordering::SeqCst);
        }
        self.remaining
            .store(equalizer.remaining(), Ordering::SeqCst);
        self.version.store(next, Ordering::SeqCst);
    }
    fn read(&self) -> Option<EqualizerSnapshot> {
        for _ in 0..4 {
            let before = self.version.load(Ordering::SeqCst);
            if before & 1 != 0 {
                continue;
            }
            let applied_sequence = self.sequence.load(Ordering::SeqCst);
            let rate = self.rate.load(Ordering::SeqCst);
            let mask = self.mask.load(Ordering::SeqCst);
            let applied = EqualizerSettings {
                gains_db: std::array::from_fn(|band| {
                    f64::from_bits(self.gains[band].load(Ordering::SeqCst))
                }),
            };
            let transition_remaining_frames = self.remaining.load(Ordering::SeqCst);
            if before == self.version.load(Ordering::SeqCst) {
                return Some(EqualizerSnapshot {
                    applied,
                    applied_sequence,
                    sample_rate: (rate != 0).then_some(rate),
                    active_bands: std::array::from_fn(|band| mask & (1 << band) != 0),
                    transition_remaining_frames,
                    clipped_samples: self.clipped.load(Ordering::Acquire),
                    non_finite_samples: self.non_finite.load(Ordering::Acquire),
                    peak_before_clamp: f64::from_bits(self.peak.load(Ordering::Acquire)),
                    ..EqualizerSnapshot::default()
                });
            }
        }
        None
    }
}

/// Runtime diagnostics are fixed categories so the error callback never formats
/// strings or waits for the engine. Repeated categories are coalesced.
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize)]
pub enum OutputError {
    DeviceNotAvailable,
    StreamInvalidated,
    BufferUnderrun,
    BackendSpecific,
}

impl fmt::Display for OutputError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::DeviceNotAvailable => "audio output device is unavailable",
            Self::StreamInvalidated => "audio output configuration was invalidated",
            Self::BufferUnderrun => "audio output buffer underrun",
            Self::BackendSpecific => "audio backend reported a stream error",
        })
    }
}

impl std::error::Error for OutputError {}

#[derive(Clone, Copy)]
struct EffectsObservation {
    sequence: u64,
    token: u64,
    epoch: u64,
    sample_rate: u32,
    effect_channels: u16,
    device_channels: u16,
    latency: u32,
    tail: u64,
    transition: u32,
    reduction: f32,
    bytes: usize,
}
struct EffectsTelemetry {
    version: AtomicU64,
    sequence: AtomicU64,
    token: AtomicU64,
    epoch: AtomicU64,
    rate: AtomicU32,
    channels: AtomicU32,
    latency: AtomicU32,
    tail: AtomicU64,
    transition: AtomicU32,
    reduction: AtomicU32,
    bytes: AtomicUsize,
}
impl EffectsTelemetry {
    fn new() -> Self {
        Self {
            version: AtomicU64::new(0),
            sequence: AtomicU64::new(0),
            token: AtomicU64::new(0),
            epoch: AtomicU64::new(0),
            rate: AtomicU32::new(0),
            channels: AtomicU32::new(0),
            latency: AtomicU32::new(0),
            tail: AtomicU64::new(0),
            transition: AtomicU32::new(0),
            reduction: AtomicU32::new(0),
            bytes: AtomicUsize::new(0),
        }
    }
    fn publish(&self, bank: &PreparedEffects, frame: &EffectsFrame) {
        let previous = self.version.load(Ordering::SeqCst);
        let Some(next) = previous.checked_add(2) else {
            return;
        };
        let plan = bank.preparation();
        self.version.store(previous + 1, Ordering::SeqCst);
        self.sequence.store(frame.effect_sequence, Ordering::SeqCst);
        self.token.store(frame.token, Ordering::SeqCst);
        self.epoch.store(frame.source_epoch, Ordering::SeqCst);
        self.rate.store(plan.format.sample_rate, Ordering::SeqCst);
        self.channels.store(
            u32::from(plan.format.channels) | (u32::from(plan.device_channels) << 16),
            Ordering::SeqCst,
        );
        self.latency.store(bank.latency_frames(), Ordering::SeqCst);
        self.tail
            .store(bank.remaining_tail_frames(), Ordering::SeqCst);
        self.transition
            .store(frame.transition_remaining_frames, Ordering::SeqCst);
        self.reduction
            .store(bank.reduction_db().to_bits(), Ordering::SeqCst);
        self.bytes.store(bank.owned_bytes(), Ordering::SeqCst);
        self.version.store(next, Ordering::SeqCst);
    }
    fn read(&self) -> Option<EffectsObservation> {
        for _ in 0..4 {
            let version = self.version.load(Ordering::SeqCst);
            if version == 0 || version & 1 != 0 {
                continue;
            }
            let channels = self.channels.load(Ordering::SeqCst);
            let current = EffectsObservation {
                sequence: self.sequence.load(Ordering::SeqCst),
                token: self.token.load(Ordering::SeqCst),
                epoch: self.epoch.load(Ordering::SeqCst),
                sample_rate: self.rate.load(Ordering::SeqCst),
                effect_channels: channels as u16,
                device_channels: (channels >> 16) as u16,
                latency: self.latency.load(Ordering::SeqCst),
                tail: self.tail.load(Ordering::SeqCst),
                transition: self.transition.load(Ordering::SeqCst),
                reduction: f32::from_bits(self.reduction.load(Ordering::SeqCst)),
                bytes: self.bytes.load(Ordering::SeqCst),
            };
            if version == self.version.load(Ordering::SeqCst) {
                return Some(current);
            }
        }
        None
    }
}
struct Shared {
    live_effects: live_effects::LiveEffectsMailbox,
    effects_telemetry: EffectsTelemetry,
    effects_required: AtomicBool,
    spatial_latest_target: Mutex<Option<SpatialTarget>>,
    presented_media_token: AtomicU64,
    presented_media_source_epoch: AtomicU64,
    spatial_applied: AtomicU64,
    effect_applied: AtomicU64,
    effect_token: AtomicU64,
    effect_source_epoch: AtomicU64,
    analysis: AnalysisMailbox,
    analysis_configuration: AtomicU64,
    paused: AtomicBool,
    gain: AtomicU32,
    muted: AtomicBool,
    switching: AtomicBool,
    ready: AtomicBool,
    format_rate: AtomicU32,
    format_channels: AtomicU32,
    reclaimed: AtomicUsize,
    device_change_pause: AtomicBool,
    stream_generation: AtomicU64,
    device_managed: AtomicBool,
    recovery_errors: AtomicU64,
    diagnostics: AtomicU64,
    epoch: AtomicU64,
    token: AtomicU64,
    ended_token: AtomicU64,
    ended_epoch: AtomicU64,
    shutdown: AtomicBool,
    callback_alive: AtomicBool,
    errors: AtomicU64,
    equalizer_requested: Mutex<EqualizerRequest>,
    equalizer_plan: PlanMailbox,
    equalizer_telemetry: EqualizerTelemetry,
    equalizer_observed: Mutex<EqualizerSnapshot>,
    processing_requested: Mutex<ProcessingRequest>,
    processing_plan: ProcessingMailbox,
    processing_telemetry: ProcessingTelemetry,
    processing_observed: Mutex<PlaybackProcessingSnapshot>,
    pitch_capacity: AtomicU64,
    pitch_rate: AtomicU32,
    render_last: AtomicU64,
    render_max: AtomicU64,
    render_total: AtomicU64,
    render_overruns: AtomicU64,
    #[cfg(test)]
    fence_after_pitch: AtomicU8,
    #[cfg(test)]
    effect_epoch_gate_frame: AtomicU32,
    #[cfg(test)]
    effect_epoch_gate_entered: AtomicBool,
    #[cfg(test)]
    effect_epoch_gate_release: AtomicBool,
}

impl Shared {
    fn new() -> Result<Self> {
        Ok(Self {
            live_effects: live_effects::LiveEffectsMailbox::new()?,
            effects_telemetry: EffectsTelemetry::new(),
            effects_required: AtomicBool::new(false),
            spatial_latest_target: Mutex::new(None),
            presented_media_token: AtomicU64::new(0),
            presented_media_source_epoch: AtomicU64::new(0),
            spatial_applied: AtomicU64::new(0),
            effect_applied: AtomicU64::new(0),
            effect_token: AtomicU64::new(0),
            effect_source_epoch: AtomicU64::new(0),
            analysis: AnalysisMailbox::new(),
            analysis_configuration: AtomicU64::new(2),
            paused: AtomicBool::new(false),
            gain: AtomicU32::new(1.0f32.to_bits()),
            muted: AtomicBool::new(false),
            switching: AtomicBool::new(false),
            ready: AtomicBool::new(true),
            format_rate: AtomicU32::new(MEMORY_FORMAT.sample_rate),
            format_channels: AtomicU32::new(MEMORY_FORMAT.channels as u32),
            reclaimed: AtomicUsize::new(0),
            device_change_pause: AtomicBool::new(false),
            stream_generation: AtomicU64::new(0),
            device_managed: AtomicBool::new(false),
            recovery_errors: AtomicU64::new(0),
            diagnostics: AtomicU64::new(0),
            epoch: AtomicU64::new(1),
            token: AtomicU64::new(0),
            ended_token: AtomicU64::new(0),
            ended_epoch: AtomicU64::new(0),
            shutdown: AtomicBool::new(false),
            callback_alive: AtomicBool::new(true),
            errors: AtomicU64::new(0),
            equalizer_requested: Mutex::new(EqualizerRequest::default()),
            equalizer_plan: PlanMailbox::new(),
            equalizer_telemetry: EqualizerTelemetry::new(),
            equalizer_observed: Mutex::new(EqualizerSnapshot::default()),
            processing_requested: Mutex::new(ProcessingRequest::default()),
            processing_plan: ProcessingMailbox::new(),
            processing_telemetry: ProcessingTelemetry::new(),
            processing_observed: Mutex::new(PlaybackProcessingSnapshot::default()),
            pitch_capacity: AtomicU64::new(0),
            pitch_rate: AtomicU32::new(0),
            render_last: AtomicU64::new(0),
            render_max: AtomicU64::new(0),
            render_total: AtomicU64::new(0),
            render_overruns: AtomicU64::new(0),
            #[cfg(test)]
            fence_after_pitch: AtomicU8::new(0),
            #[cfg(test)]
            effect_epoch_gate_frame: AtomicU32::new(0),
            #[cfg(test)]
            effect_epoch_gate_entered: AtomicBool::new(false),
            #[cfg(test)]
            effect_epoch_gate_release: AtomicBool::new(false),
        })
    }

    #[cfg(test)]
    fn report_error(&self, error: &cpal::StreamError) {
        self.report_error_for(self.stream_generation.load(Ordering::Acquire), error);
    }

    fn report_error_for(&self, generation: u64, error: &cpal::StreamError) {
        let flag = match error {
            cpal::StreamError::DeviceNotAvailable => 1,
            cpal::StreamError::StreamInvalidated => 2,
            cpal::StreamError::BufferUnderrun => 4,
            cpal::StreamError::BackendSpecific { .. } => 8,
        };
        let sideband = if self.device_managed.load(Ordering::Acquire) {
            match flag {
                1 | 2 => &self.recovery_errors,
                4 => &self.diagnostics,
                _ => &self.errors,
            }
        } else {
            &self.errors
        };
        let mut previous = sideband.load(Ordering::Acquire);
        loop {
            if self.stream_generation.load(Ordering::Acquire) != generation
                || previous >> 8 != generation
            {
                return;
            }
            match sideband.compare_exchange_weak(
                previous,
                previous | flag,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return,
                Err(value) => previous = value,
            }
        }
    }
}

struct Install {
    epoch: u64,
    token: u64,
    source: Box<ConvertedSource>,
}

/// A fixed-frame linear converter. No constructor reads PCM: playback position
/// advances only when the render kernel consumes media frames.
struct ConvertedSource {
    source: PcmSource,
    control: PcmControl,
    from: PcmFormat,
    to: PcmFormat,
    epoch: u64,
    phase: u64,
    current: [f32; MAX_CHANNELS],
    next: [f32; MAX_CHANNELS],
    initialized: bool,
    have_next: bool,
    ended: bool,
    output_valid: bool,
    underflow: bool,
    frame_underflow: bool,
    current_underflow: bool,
    next_underflow: bool,
    frame_stamp: Option<(ProcessingStamp, u64)>,
    frame_begin: Option<(ProcessingStamp, u64)>,
    current_stamp: Option<(ProcessingStamp, u64)>,
    current_begin: Option<(ProcessingStamp, u64)>,
    next_stamp: Option<(ProcessingStamp, u64)>,
    next_begin: Option<(ProcessingStamp, u64)>,
    previous_source_end: Option<(ProcessingStamp, u64)>,
    previous_output_end: Option<(ProcessingStamp, u64)>,
    output_stamp: Option<(ProcessingStamp, u64)>,
    output_begin: Option<(ProcessingStamp, u64)>,
    analysis_samples: [f32; MAX_CHANNELS],
}

impl ConvertedSource {
    fn new(mut source: PcmSource, to: PcmFormat) -> Self {
        source.set_deferred_clock(true);
        let from = source.format();
        let control = source.control();
        let (epoch, _) = control.render_state();
        Self {
            source,
            control,
            from,
            to,
            epoch,
            phase: 0,
            current: [0.0; MAX_CHANNELS],
            next: [0.0; MAX_CHANNELS],
            analysis_samples: [0.0; MAX_CHANNELS],
            initialized: false,
            have_next: false,
            ended: false,
            output_valid: false,
            underflow: false,
            frame_underflow: false,
            current_underflow: false,
            next_underflow: false,
            frame_stamp: None,
            frame_begin: None,
            current_stamp: None,
            current_begin: None,
            next_stamp: None,
            next_begin: None,
            previous_source_end: None,
            previous_output_end: None,
            output_stamp: None,
            output_begin: None,
        }
    }

    fn reset(&mut self, epoch: u64) {
        self.epoch = epoch;
        self.phase = 0;
        self.current.fill(0.0);
        self.next.fill(0.0);
        self.initialized = false;
        self.have_next = false;
        self.ended = false;
        self.output_valid = false;
        self.underflow = false;
        self.frame_underflow = false;
        self.current_underflow = false;
        self.next_underflow = false;
        self.frame_stamp = None;
        self.frame_begin = None;
        self.current_stamp = None;
        self.current_begin = None;
        self.next_stamp = None;
        self.next_begin = None;
        self.previous_source_end = None;
        self.previous_output_end = None;
        self.output_stamp = None;
        self.output_begin = None;
        self.analysis_samples.fill(0.0);
    }

    fn retarget(&mut self, to: PcmFormat) {
        // Phase is a fraction of an input frame expressed in output-rate units.
        // Keep the already consumed current/next frames and their fractional position.
        self.phase =
            ((self.phase as u128 * to.sample_rate as u128) / self.to.sample_rate as u128) as u64;
        self.to = to;
    }

    fn read_frame(&mut self) -> Option<[f32; MAX_CHANNELS]> {
        self.frame_underflow = false;
        self.frame_stamp = None;
        self.frame_begin = None;
        let source_epoch = self.control.render_state().0;
        if self
            .previous_source_end
            .is_none_or(|(_, epoch)| epoch != source_epoch)
        {
            let Some((end_frame, fraction, epoch)) = self.control.presentation_baseline() else {
                self.frame_underflow = true;
                self.underflow = true;
                return Some([0.0; MAX_CHANNELS]);
            };
            if epoch != source_epoch {
                self.frame_underflow = true;
                self.underflow = true;
                return Some([0.0; MAX_CHANNELS]);
            }
            self.previous_source_end = Some((
                ProcessingStamp {
                    end_frame,
                    fraction,
                    ..ProcessingStamp::default()
                },
                epoch,
            ));
        }
        let before = self.control.snapshot().underflow_frames;
        let mut frame = [0.0; MAX_CHANNELS];
        for sample in frame.iter_mut().take(self.from.channels as usize) {
            *sample = self.source.next()?;
        }
        self.frame_underflow = self.control.snapshot().underflow_frames != before;
        self.underflow |= self.frame_underflow;
        self.frame_stamp = self.source.current_source_stamp();
        if let (Some((begin, begin_epoch)), Some((stamp, epoch))) =
            (self.previous_source_end, self.frame_stamp)
            && begin_epoch == epoch
        {
            self.frame_begin = Some((
                ProcessingStamp {
                    end_frame: begin.end_frame,
                    fraction: begin.fraction,
                    ..stamp
                },
                epoch,
            ));
            self.previous_source_end = Some((stamp, epoch));
        }
        Some(frame)
    }

    fn next_frame(&mut self) -> [f32; MAX_CHANNELS] {
        self.output_valid = false;
        self.underflow = false;
        self.output_stamp = None;
        self.output_begin = None;
        let (epoch, blocked) = self.control.render_state();
        if epoch != self.epoch {
            self.reset(epoch);
        }
        if blocked || self.ended {
            return [0.0; MAX_CHANNELS];
        }

        // Identity rate preserves every interleaved frame without prefetch.
        let frame = if self.from.sample_rate == self.to.sample_rate && !self.initialized {
            match self.read_frame() {
                Some(frame) => {
                    self.output_stamp = self.frame_stamp;
                    self.output_begin = self.frame_begin;
                    frame
                }
                None => {
                    self.ended = true;
                    return [0.0; MAX_CHANNELS];
                }
            }
        } else {
            if !self.initialized {
                let Some(frame) = self.read_frame() else {
                    self.ended = true;
                    return [0.0; MAX_CHANNELS];
                };
                self.current = frame;
                self.current_underflow = self.frame_underflow;
                self.current_stamp = self.frame_stamp;
                self.current_begin = self.frame_begin;
                self.next = match self.read_frame() {
                    Some(next) => {
                        self.have_next = true;
                        self.next_underflow = self.frame_underflow;
                        self.next_stamp = self.frame_stamp;
                        self.next_begin = self.frame_begin;
                        next
                    }
                    None => {
                        self.have_next = false;
                        self.next_underflow = self.current_underflow;
                        self.next_stamp = self.current_stamp;
                        self.next_begin = self.current_begin;
                        self.current
                    }
                };
                self.initialized = true;
            }

            let weight = self.phase as f32 / self.to.sample_rate as f32;
            let starting_begin = self.current_begin;
            self.underflow |= self.current_underflow || self.next_underflow;
            let mut result = [0.0; MAX_CHANNELS];
            for (channel, sample) in result
                .iter_mut()
                .enumerate()
                .take(self.from.channels as usize)
            {
                *sample =
                    self.current[channel] + (self.next[channel] - self.current[channel]) * weight;
            }
            self.phase += self.from.sample_rate as u64;
            let mut boundary = None;
            while self.phase >= self.to.sample_rate as u64 {
                self.phase -= self.to.sample_rate as u64;
                if !self.have_next {
                    self.ended = true;
                    boundary = self.current_stamp;
                    break;
                }
                boundary = self.current_stamp;
                self.current = self.next;
                self.current_underflow = self.next_underflow;
                self.current_stamp = self.next_stamp;
                self.current_begin = self.next_begin;
                self.next = match self.read_frame() {
                    Some(next) => {
                        self.next_underflow = self.frame_underflow;
                        self.next_stamp = self.frame_stamp;
                        self.next_begin = self.frame_begin;
                        next
                    }
                    None => {
                        self.have_next = false;
                        self.next_underflow = self.current_underflow;
                        self.next_stamp = self.current_stamp;
                        self.next_begin = self.current_begin;
                        self.current
                    }
                };
            }
            self.output_stamp = if self.ended || self.phase == 0 {
                boundary.or(self.current_begin)
            } else {
                interpolate_stamp(
                    self.current_begin,
                    self.current_stamp,
                    self.phase,
                    self.to.sample_rate as u64,
                )
            };
            self.output_begin = self
                .previous_output_end
                .or(starting_begin)
                .or(self.current_begin);
            result
        };

        // A concurrent seek may invalidate cached PCM after a read. The next
        // invocation resets the converter to the published epoch.
        let (latest_epoch, blocked) = self.control.render_state();
        if latest_epoch != self.epoch || blocked {
            return [0.0; MAX_CHANNELS];
        }
        self.output_valid = self.output_stamp.is_some();
        if self.output_valid {
            self.previous_output_end = self.output_stamp;
            self.analysis_samples = frame;
        }
        map_channels(
            frame,
            self.from.channels as usize,
            self.to.channels as usize,
        )
    }
}

fn map_channels(input: [f32; MAX_CHANNELS], from: usize, to: usize) -> [f32; MAX_CHANNELS] {
    let mut result = [0.0; MAX_CHANNELS];
    if to == 1 {
        result[0] = input[..from].iter().sum::<f32>() / from as f32;
    } else if from == 1 {
        result[..to].fill(input[0]);
    } else if from <= to {
        result[..from].copy_from_slice(&input[..from]);
    } else {
        // Fold surplus channels into their corresponding output channel. This
        // preserves independent L/R PCM and bounds a full-scale downmix.
        let mut count = [0usize; MAX_CHANNELS];
        for (index, value) in input[..from].iter().enumerate() {
            result[index % to] += value;
            count[index % to] += 1;
        }
        for (value, count) in result[..to].iter_mut().zip(&count[..to]) {
            *value /= *count as f32;
        }
    }
    result
}

fn map_processed_channels(
    input: [f64; MAX_CHANNELS],
    from: usize,
    to: usize,
) -> [f64; MAX_CHANNELS] {
    let mut result = [0.0; MAX_CHANNELS];
    if to == 1 {
        result[0] = input[..from].iter().sum::<f64>() / from as f64;
    } else if from == 1 {
        result[..to].fill(input[0]);
    } else if from <= to {
        result[..from].copy_from_slice(&input[..from]);
    } else {
        let mut count = [0usize; MAX_CHANNELS];
        for (index, value) in input[..from].iter().enumerate() {
            result[index % to] += value;
            count[index % to] += 1;
        }
        for (value, count) in result[..to].iter_mut().zip(&count[..to]) {
            *value /= *count as f64;
        }
    }
    result
}

struct RenderState {
    retained_pitch_latency_frames: u32,
    retained_pitch_source_latency_frames: u32,
    retained_pitch_source_rate: u32,
    effects: EffectsRenderExchange,
    spatial_targets: Consumer<SpatialTarget>,
    analysis: AnalysisTap,
    analysis_configuration_seen: u64,
    shared: Arc<Shared>,
    format: PcmFormat,
    commands: Consumer<Install>,
    retired: Producer<Box<ConvertedSource>>,
    active: Option<Install>,
    pending: Option<Install>,
    equalizer: Equalizer,
    equalizer_silent: bool,
    equalizer_epoch: Option<u64>,
    pitch: PitchProcessor,
    processing_sequence: u64,
    pitch_epoch: Option<(u64, u64)>,
}

impl RenderState {
    fn presentation_fence(&self, source_epoch: u64) -> (bool, bool) {
        let mut temporal = self.shared.paused.load(Ordering::Acquire)
            || self.shared.switching.load(Ordering::Acquire);
        let hard = self.shared.shutdown.load(Ordering::Acquire)
            || self.active.as_ref().is_none_or(|active| {
                let (epoch, blocked) = active.source.control.render_state();
                let paused = active.source.control.paused();
                temporal |= paused;
                let failed = blocked && {
                    let state = active.source.control.snapshot();
                    state.failed || state.cancelled || !paused
                };
                epoch != source_epoch
                    || failed
                    || active.token != self.shared.token.load(Ordering::Acquire)
                    || active.epoch != self.shared.epoch.load(Ordering::Acquire)
            });
        (hard, temporal)
    }

    fn retarget(
        &mut self,
        format: PcmFormat,
        plan: EqualizerPlan,
        prepared: Option<PreparedPitch>,
        prepared_analysis: AnalysisTap,
    ) -> Result<()> {
        let processing = *self
            .shared
            .processing_requested
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        self.pitch
            .set_factor(processing.settings.pitch_ratio, processing.sequence)?;
        if let Some(prepared) = prepared {
            self.pitch.retarget_with_prepared(prepared)?;
        }
        self.analysis = prepared_analysis;
        self.analysis_configuration_seen = 0;
        self.shared.analysis.clear();
        self.shared
            .pitch_capacity
            .store(self.pitch.capacity_bound() as u64, Ordering::Release);
        self.shared
            .pitch_rate
            .store(self.pitch.sample_rate(), Ordering::Release);
        self.retained_pitch_latency_frames = (self.retained_pitch_source_latency_frames as u64
            * format.sample_rate as u64)
            .div_ceil(self.retained_pitch_source_rate as u64)
            .min(u32::MAX as u64) as u32;
        self.format = format;
        let processing_format = PcmFormat {
            sample_rate: format.sample_rate,
            channels: self.effects.bus_channels().unwrap_or(format.channels),
        };
        if let Some(active) = &mut self.active {
            active.source.retarget(processing_format);
        }
        if let Some(pending) = &mut self.pending {
            pending.source.retarget(processing_format);
        }
        self.equalizer.retarget(plan);
        self.equalizer_silent = true;
        self.equalizer_epoch = None;
        self.processing_sequence = processing.sequence;
        Ok(())
    }
    fn retire(&mut self, install: Install) -> Option<Install> {
        match self.retired.push(install.source) {
            Ok(()) => None,
            Err(PushError::Full(source)) => Some(Install { source, ..install }),
        }
    }

    fn apply_controls(&mut self) {
        let epoch = self.shared.epoch.load(Ordering::Acquire);
        if self
            .active
            .as_ref()
            .is_some_and(|active| active.epoch != epoch)
        {
            let active = self.active.take().expect("active source checked");
            self.equalizer.reset();
            self.equalizer_silent = true;
            self.equalizer_epoch = None;
            self.pitch.reset();
            self.pitch_epoch = None;
            self.analysis.invalidate();
            self.active = self.retire(active);
            if self.active.is_some() {
                return;
            }
        }
        // Admission bounds the entire active + queued + retired population, not
        // just the command queue. Each callback does at most eight replacements.
        for _ in 0..SOURCE_CAPACITY {
            if self.retired.is_full() {
                return;
            }
            let mut install = match self.pending.take() {
                Some(install) => install,
                None => match self.commands.pop() {
                    Ok(install) => install,
                    Err(_) => return,
                },
            };
            install.source.retarget(self.format);
            if install.epoch != epoch {
                self.pending = self.retire(install);
                if self.pending.is_some() {
                    return;
                }
                continue;
            }
            if let Some(active) = self.active.take() {
                self.active = self.retire(active);
                if self.active.is_some() {
                    self.pending = Some(install);
                    return;
                }
            }
            self.active = Some(install);
            self.equalizer.reset();
            self.equalizer_silent = true;
            self.equalizer_epoch = None;
            self.pitch.reset();
            self.pitch_epoch = None;
            self.analysis.invalidate();
        }
    }

    fn render<T>(&mut self, output: &mut [T])
    where
        T: SizedSample + FromSample<f32>,
    {
        self.apply_controls();
        let effect_token = self.shared.token.load(Ordering::Acquire);
        let effect_epoch = self
            .active
            .as_ref()
            .map_or(0, |active| active.source.control.current_epoch());
        self.effects.apply_with_layout(
            effect_token,
            effect_epoch,
            self.format.sample_rate,
            self.format.channels,
            self.pitch
                .has_pending_content()
                .then_some(self.pitch.channels()),
        );
        if self.shared.effects_required.load(Ordering::Acquire)
            && self.effects.active().is_none()
            && self.effects.retained_drained()
        {
            output.fill(T::from_sample(0.0));
            return;
        }
        let effect_channels = self.effects.bus_channels();
        if let Some(channels) = effect_channels
            && self.pitch.select_channels(channels)
        {
            let bus = PcmFormat {
                sample_rate: self.format.sample_rate,
                channels,
            };
            if let Some(active) = &mut self.active {
                active.source.retarget(bus);
            }
        }
        let mut spatial_target = None;
        for _ in 0..8 {
            match self.spatial_targets.pop() {
                Ok(target) => spatial_target = Some(target),
                Err(_) => break,
            }
        }
        if let Some(target) = spatial_target {
            self.effects.retarget_spatial(target);
        }
        let analysis_configuration = self.shared.analysis_configuration.load(Ordering::Acquire);
        if analysis_configuration & 1 == 0
            && self.analysis_configuration_seen != analysis_configuration
        {
            self.analysis.invalidate();
            self.shared.analysis.clear();
            self.analysis_configuration_seen = analysis_configuration;
        }
        if let Some(request) = self.shared.processing_plan.read()
            && request.sequence > self.processing_sequence
            && self
                .pitch
                .set_factor(request.settings.pitch_ratio, request.sequence)
                .is_ok()
        {
            self.processing_sequence = request.sequence;
        }
        if self.shared.equalizer_plan.sequence.load(Ordering::Acquire)
            > self.equalizer.plan().sequence
            && let Some(plan) = self.shared.equalizer_plan.read()
            && plan.sample_rate == self.format.sample_rate
        {
            if self.equalizer_silent
                || self.shared.paused.load(Ordering::Acquire)
                || self.shared.switching.load(Ordering::Acquire)
            {
                self.equalizer.retarget(plan);
            } else {
                self.equalizer.request(plan);
            }
        }
        let channels = self.format.channels as usize;
        let processing_channels = self.pitch.channels() as usize;
        let mut clipped = 0u64;
        let mut non_finite = 0u64;
        let mut peak = 0.0f64;
        let mut presented = false;
        for frame in output.chunks_mut(channels) {
            #[cfg(test)]
            if self.shared.effect_epoch_gate_frame.load(Ordering::Acquire) != 0
                && self
                    .shared
                    .effect_epoch_gate_frame
                    .fetch_sub(1, Ordering::AcqRel)
                    == 1
            {
                self.shared
                    .effect_epoch_gate_entered
                    .store(true, Ordering::Release);
                while !self
                    .shared
                    .effect_epoch_gate_release
                    .load(Ordering::Acquire)
                {
                    std::hint::spin_loop();
                }
            }
            let live_epoch = self
                .active
                .as_ref()
                .map_or(0, |active| active.source.control.current_epoch());
            self.effects.apply_with_layout(
                self.shared.token.load(Ordering::Acquire),
                live_epoch,
                self.format.sample_rate,
                self.format.channels,
                Some(self.pitch.channels()),
            );
            let paused = self.shared.paused.load(Ordering::Acquire);
            let token = self.shared.token.load(Ordering::Acquire);
            let epoch = self.shared.epoch.load(Ordering::Acquire);
            let gain = if self.shared.muted.load(Ordering::Acquire) {
                0.0
            } else {
                f32::from_bits(self.shared.gain.load(Ordering::Acquire))
            };
            if self.effects.retained_media_pending() {
                if let Ok(Some(retained)) = self.effects.next_retained_media() {
                    let mut accepted = false;
                    let mut stamp = retained.stamp.unwrap().media;
                    stamp.settings.pitch_ratio = retained.pitch.ratio;
                    stamp.processing_sequence =
                        stamp.processing_sequence.min(retained.pitch.sequence);
                    if let Some(active) = &self.active {
                        let (hard, temporal) = self.presentation_fence(retained.source_epoch);
                        if !hard && !temporal {
                            let source_rate = active.source.from.sample_rate as u64;
                            let output_rate = self.format.sample_rate as u64;
                            let latency = (stamp.latency_frames as u64 * output_rate)
                                .div_ceil(source_rate)
                                + self.retained_pitch_latency_frames as u64
                                + retained.retained_effect_latency_frames as u64;
                            stamp.latency_frames = (latency * source_rate)
                                .div_ceil(output_rate)
                                .min(u32::MAX as u64)
                                as u32;
                            accepted = active
                                .source
                                .control
                                .commit_rendered_stamp(stamp, retained.source_epoch);
                            if accepted {
                                for (target, sample) in frame.iter_mut().zip(retained.samples) {
                                    let value = sample * gain as f64;
                                    let audible = if value.is_finite() {
                                        value.clamp(-1.0, 1.0) as f32
                                    } else {
                                        non_finite += 1;
                                        0.0
                                    };
                                    peak = peak.max(f64::from(audible).abs());
                                    clipped += u64::from(value.abs() > 1.0);
                                    *target = T::from_sample(audible);
                                }
                                self.shared.processing_telemetry.publish(
                                    PlaybackProcessingSnapshot {
                                        applied: stamp.settings,
                                        applied_sequence: stamp.processing_sequence,
                                        sample_rate: Some(self.format.sample_rate),
                                        latency_frames: latency.min(u32::MAX as u64) as u32,
                                        transition_remaining_frames: (retained
                                            .pitch
                                            .transition_remaining_frames
                                            as u64
                                            * self.format.sample_rate as u64)
                                            .div_ceil(self.retained_pitch_source_rate as u64)
                                            .min(u32::MAX as u64)
                                            as u32,
                                        ..PlaybackProcessingSnapshot::default()
                                    },
                                );
                                self.shared
                                    .presented_media_source_epoch
                                    .store(retained.source_epoch, Ordering::Release);
                                self.shared
                                    .presented_media_token
                                    .store(retained.token, Ordering::Release);
                                self.shared
                                    .spatial_applied
                                    .store(retained.spatial_sequence, Ordering::Release);
                                self.shared
                                    .effect_applied
                                    .store(retained.effect_sequence, Ordering::Release);
                                self.shared
                                    .effect_source_epoch
                                    .store(retained.source_epoch, Ordering::Release);
                                self.shared
                                    .effect_token
                                    .store(retained.token, Ordering::Release);
                                presented = true;
                            }
                        }
                    }
                    if !accepted {
                        frame.fill(T::from_sample(0.0));
                    }
                    if self.effects.release_retained(&retained, accepted).is_err() {
                        self.shared.errors.fetch_or(8, Ordering::AcqRel);
                    }
                } else {
                    frame.fill(T::from_sample(0.0));
                    self.shared.errors.fetch_or(8, Ordering::AcqRel);
                }
                continue;
            }
            let mut media_valid = false;
            let mut reset_history = false;
            let mut input_stamp = None;
            let mut input_begin = None;
            let mut source_ended = false;
            let mut allowed = false;
            let mut waiting_for_effect = false;
            let mut analysis_samples = [0.0; MAX_CHANNELS];
            let mut analysis_channels = 0usize;
            let effect_pending = self
                .effects
                .active()
                .is_some_and(|bank| bank.pending_output().is_some());
            let samples = match &mut self.active {
                Some(active)
                    if !paused
                        && active.token == token
                        && active.epoch == epoch
                        && !self.shared.switching.load(Ordering::Acquire)
                        && !self.shared.shutdown.load(Ordering::Acquire) =>
                {
                    let (source_epoch, source_blocked) = active.source.control.render_state();
                    // Epoch may advance after the callback-level gate. An older
                    // sequence of this same epoch may finish its live handover;
                    // another epoch's graph can never consume the new PCM.
                    let graph_matches = self.effects.active().is_some_and(|bank| {
                        let plan = bank.preparation();
                        plan.token == token
                            && plan.source_epoch == source_epoch
                            && plan.format.sample_rate == self.format.sample_rate
                            && plan.device_channels == self.format.channels
                    });
                    waiting_for_effect =
                        self.shared.effects_required.load(Ordering::Acquire) && !graph_matches;
                    if active.source.epoch != source_epoch {
                        active.source.reset(source_epoch);
                    }
                    if self.pitch_epoch != Some((active.token, source_epoch)) {
                        self.pitch.reset();
                        self.pitch_epoch = Some((active.token, source_epoch));
                        self.shared
                            .processing_telemetry
                            .publish(PlaybackProcessingSnapshot::default());
                    }
                    reset_history = self.equalizer_epoch != Some(source_epoch);
                    self.equalizer_epoch = Some(source_epoch);
                    if source_blocked || waiting_for_effect {
                        let state = active.source.control.snapshot();
                        if !active.source.control.paused() || state.failed || state.cancelled {
                            reset_history = true;
                            self.pitch.reset();
                        }
                        // Preserve the separately fenced retained-tail route without
                        // pulling a new source frame or draining its pitch prefix.
                        allowed = waiting_for_effect && !source_blocked;
                        [0.0; MAX_CHANNELS]
                    } else {
                        allowed = true;
                        source_ended = active.source.ended;
                        if effect_pending || self.pitch.has_prepared_output() || source_ended {
                            [0.0; MAX_CHANNELS]
                        } else {
                            let samples = active.source.next_frame();
                            analysis_samples = active.source.analysis_samples;
                            analysis_channels = active.source.from.channels as usize;
                            media_valid = active.source.output_valid;
                            reset_history |= active.source.underflow;
                            input_stamp = active.source.output_stamp;
                            input_begin = active.source.output_begin;
                            source_ended = active.source.ended;
                            samples
                        }
                    }
                }
                _ => [0.0; MAX_CHANNELS],
            };
            let analysis_configuration = self.shared.analysis_configuration.load(Ordering::Acquire);
            if analysis_configuration & 1 != 0
                && media_valid
                && let Some((stamp, source_epoch)) = input_stamp
            {
                let (hard, temporal) = self.presentation_fence(source_epoch);
                if !hard && !temporal {
                    self.analysis.push(
                        &analysis_samples[..analysis_channels],
                        token,
                        source_epoch,
                        stamp,
                        &self.shared.analysis,
                        analysis_configuration >> 1,
                    );
                    self.analysis_configuration_seen = analysis_configuration;
                }
            }
            if reset_history {
                self.equalizer.reset();
                self.equalizer_silent = true;
            }
            let samples = if media_valid {
                self.equalizer_silent = false;
                let (samples, errors) = self
                    .equalizer
                    .frame(samples.map(f64::from), processing_channels);
                non_finite += errors;
                samples
            } else {
                [0.0; MAX_CHANNELS]
            };
            let mut processed = if allowed && !effect_pending && !waiting_for_effect {
                if self.pitch.has_prepared_output() {
                    self.pitch.drain()
                } else if media_valid {
                    self.pitch.set_baseline(input_begin);
                    let result = self.pitch.process(samples, input_stamp);
                    if source_ended {
                        self.pitch.begin_eof();
                    }
                    result
                } else if source_ended {
                    self.pitch.begin_eof();
                    self.pitch.drain()
                } else {
                    PitchFrame {
                        samples: [0.0; MAX_CHANNELS],
                        stamp: None,
                        pitch_ratio: 1.0,
                        pitch_sequence: self.processing_sequence,
                        transition_remaining_frames: 0,
                    }
                }
            } else {
                PitchFrame {
                    samples: [0.0; MAX_CHANNELS],
                    stamp: None,
                    pitch_ratio: 1.0,
                    pitch_sequence: 0,
                    transition_remaining_frames: 0,
                }
            };
            #[cfg(test)]
            if processed.stamp.is_some() {
                match self.shared.fence_after_pitch.swap(0, Ordering::AcqRel) {
                    1 => self.shared.paused.store(true, Ordering::Release),
                    2 => self.shared.switching.store(true, Ordering::Release),
                    3 => {
                        if let Some(active) = &self.active {
                            active.source.control.set_paused(true);
                        }
                    }
                    _ => {}
                }
            }
            if let Some((_, source_epoch)) = processed.stamp {
                let (hard, temporal) = self.presentation_fence(source_epoch);
                if hard || temporal {
                    processed.samples.fill(0.0);
                    processed.stamp = None;
                    if hard {
                        self.pitch.reset();
                    } else {
                        self.pitch.defer_last_output();
                    }
                }
            }
            let mut effect_frame = None;
            let emitted_begin = self.pitch.emitted_begin();
            if let Some(bank) = self.effects.active() {
                let result = if let Some(pending) = bank.pending_output() {
                    Ok(Some(pending))
                } else if allowed
                    && source_ended
                    && self.pitch.drained()
                    && processed.stamp.is_none()
                {
                    bank.begin_eof().and_then(|_| bank.drain())
                } else if allowed {
                    let input = processed.stamp.map(|(media, source_epoch)| EffectsInput {
                        samples: processed.samples,
                        stamp: ConvolutionStamp {
                            token,
                            source_epoch,
                            media,
                        },
                        begin: emitted_begin.map(|(media, source_epoch)| ConvolutionStamp {
                            token,
                            source_epoch,
                            media,
                        }),
                        pitch: EffectsPitch {
                            ratio: processed.pitch_ratio,
                            sequence: processed.pitch_sequence,
                            transition_remaining_frames: processed.transition_remaining_frames,
                        },
                    });
                    bank.frame(input).map(Some)
                } else {
                    Ok(None)
                };
                match result {
                    Ok(Some(value)) if value.kind != EffectsFrameKind::Padding => {
                        processed.samples = value.samples;
                        processed.stamp =
                            value.stamp.map(|stamp| (stamp.media, stamp.source_epoch));
                        processed.pitch_ratio = value.pitch.ratio;
                        processed.pitch_sequence = value.pitch.sequence;
                        processed.transition_remaining_frames =
                            value.pitch.transition_remaining_frames;
                        effect_frame = Some(value);
                    }
                    Ok(_) => {
                        processed.samples.fill(0.0);
                        processed.stamp = None;
                    }
                    Err(_) => {
                        processed.samples.fill(0.0);
                        processed.stamp = None;
                        self.shared.errors.fetch_or(8, Ordering::AcqRel);
                    }
                }
            }
            let retained_tail = if allowed
                && (media_valid || source_ended || effect_frame.is_some() || waiting_for_effect)
            {
                self.effects.next_retained_tail().ok().flatten()
            } else {
                None
            };
            if effect_frame.is_none() {
                processed.samples = map_processed_channels(
                    processed.samples,
                    self.pitch.channels() as usize,
                    channels,
                );
            }
            let mut final_samples = [0.0f32; MAX_CHANNELS];
            let mut retained_tail_mixed = false;
            if let Some(tail) = retained_tail {
                let (hard, temporal) = self.presentation_fence(tail.source_epoch);
                if !hard && !temporal {
                    retained_tail_mixed = true;
                    for (sample, wet) in processed.samples.iter_mut().zip(tail.samples) {
                        *sample += wet;
                    }
                }
            }
            for (sample, value) in final_samples
                .iter_mut()
                .take(channels)
                .zip(processed.samples)
            {
                let value = value * gain as f64;
                *sample = if value.is_finite() {
                    peak = peak.max(value.abs());
                    if value.abs() > 1.0 {
                        clipped += 1;
                    }
                    value.clamp(-1.0, 1.0) as f32
                } else {
                    non_finite += 1;
                    0.0
                };
            }
            let mut frame_presented = false;
            if let Some((mut stamp, source_epoch)) = processed.stamp
                && let Some(active) = &self.active
            {
                let source_rate = active.source.from.sample_rate as u64;
                let output_rate = self.format.sample_rate as u64;
                let pitch_latency = self.pitch.latency_frames() as u64;
                let effect_latency = self
                    .effects
                    .active()
                    .map_or(0, |bank| bank.latency_frames() as u64);
                let output_latency = (stamp.latency_frames as u64 * output_rate)
                    .div_ceil(source_rate)
                    .saturating_add(pitch_latency)
                    .saturating_add(effect_latency)
                    .min(u32::MAX as u64) as u32;
                stamp.settings.pitch_ratio = processed.pitch_ratio;
                stamp.processing_sequence = stamp.processing_sequence.min(processed.pitch_sequence);
                stamp.latency_frames = (stamp.latency_frames as u64)
                    .saturating_add(
                        ((pitch_latency + effect_latency) * source_rate).div_ceil(output_rate),
                    )
                    .min(u32::MAX as u64) as u32;
                let (hard, temporal) = self.presentation_fence(source_epoch);
                if !hard
                    && !temporal
                    && active
                        .source
                        .control
                        .commit_rendered_stamp(stamp, source_epoch)
                {
                    presented = true;
                    frame_presented = true;
                    self.shared
                        .processing_telemetry
                        .publish(PlaybackProcessingSnapshot {
                            applied: stamp.settings,
                            applied_sequence: stamp.processing_sequence,
                            sample_rate: Some(self.format.sample_rate),
                            latency_frames: output_latency,
                            transition_remaining_frames: processed.transition_remaining_frames,
                            ..PlaybackProcessingSnapshot::default()
                        });
                } else {
                    final_samples.fill(0.0);
                    let (hard, _) = self.presentation_fence(source_epoch);
                    if hard {
                        self.pitch.reset();
                    } else if effect_frame.is_none() {
                        self.pitch.defer_last_output();
                    }
                }
            }
            if let Some(value) = effect_frame {
                let (hard, temporal) = self.presentation_fence(value.source_epoch);
                if value.kind == EffectsFrameKind::Tail {
                    frame_presented = !hard && !temporal;
                }
                if (hard || temporal) && !frame_presented {
                    final_samples.fill(0.0);
                    frame_presented = false;
                }
            }
            if let Some(tail) = retained_tail {
                let (hard, temporal) = self.presentation_fence(tail.source_epoch);
                if retained_tail_mixed && !frame_presented && (hard || temporal) {
                    final_samples.fill(0.0);
                    retained_tail_mixed = false;
                }
            }
            let retained_tail_committed =
                retained_tail_mixed && (frame_presented || processed.stamp.is_none());
            for (target, value) in frame.iter_mut().zip(final_samples) {
                *target = T::from_sample(value);
            }
            if let Some(tail) = retained_tail
                && self
                    .effects
                    .release_retained(&tail, retained_tail_committed)
                    .is_err()
            {
                self.shared.errors.fetch_or(8, Ordering::AcqRel);
            }
            if frame_presented && let Some((_, source_epoch)) = processed.stamp {
                self.shared
                    .presented_media_source_epoch
                    .store(source_epoch, Ordering::Release);
                self.shared
                    .presented_media_token
                    .store(token, Ordering::Release);
            }
            if let Some(value) = effect_frame {
                let (hard, _) = self.presentation_fence(value.source_epoch);
                if let Some(bank) = self.effects.active() {
                    if hard && !frame_presented {
                        bank.invalidate();
                    } else if bank.release_pending(&value, frame_presented).is_err() {
                        self.shared.errors.fetch_or(8, Ordering::AcqRel);
                    } else if frame_presented {
                        self.shared.effects_telemetry.publish(bank, &value);
                        self.shared
                            .spatial_applied
                            .store(value.spatial_sequence, Ordering::Release);
                        self.shared
                            .effect_applied
                            .store(value.effect_sequence, Ordering::Release);
                        self.shared
                            .effect_source_epoch
                            .store(value.source_epoch, Ordering::Release);
                        self.shared
                            .effect_token
                            .store(value.token, Ordering::Release);
                    }
                }
            }
            if allowed
                && source_ended
                && self.pitch.drained()
                && self.effects.active().is_none_or(|bank| bank.drained())
                && self.effects.retained_drained()
                && let Some(active) = &self.active
            {
                self.shared
                    .ended_epoch
                    .store(active.source.epoch, Ordering::Release);
                self.shared
                    .ended_token
                    .store(active.token, Ordering::Release);
            }
        }
        let pending = self.pitch.has_pending_content()
            || !self.effects.retained_drained()
            || self.effects.active().is_some_and(|bank| {
                bank.pending_media_frames() != 0 || bank.pending_output().is_some()
            });
        self.shared
            .pitch_capacity
            .store(self.pitch.capacity_bound() as u64, Ordering::Release);
        self.shared
            .processing_telemetry
            .pending
            .store(pending, Ordering::Release);
        self.shared
            .processing_telemetry
            .buffering
            .store(pending && !presented, Ordering::Release);
        self.shared
            .equalizer_telemetry
            .publish(&self.equalizer, clipped, non_finite, peak);
    }
}

/// A callback destructor only transfers this preallocated box to a one-slot
/// return queue. It never destroys PCM rings, even if a device stops calling us.
struct RenderKernel {
    state: Option<Box<RenderState>>,
    handoff: Option<Consumer<Box<RenderState>>>,
    returned: Producer<Box<RenderState>>,
    signals: Option<Arc<KernelSignals>>,
}

struct KernelSignals {
    acknowledged: AtomicBool,
    closed: AtomicBool,
    errors: AtomicU8,
    handback_serial: AtomicU64,
    handback_ack: AtomicU64,
}

impl RenderKernel {
    fn render<T>(&mut self, output: &mut [T])
    where
        T: SizedSample + FromSample<f32>,
    {
        if self.state.is_none()
            && let Some(handoff) = &mut self.handoff
            && let Ok(state) = handoff.pop()
        {
            self.state = Some(state);
        }
        if let Some(state) = &mut self.state {
            if let Some(signals) = &self.signals {
                let serial = signals.handback_serial.load(Ordering::Acquire);
                if serial != 0 {
                    let token = state.shared.token.load(Ordering::Acquire);
                    let epoch = state
                        .active
                        .as_ref()
                        .map_or(0, |active| active.source.control.current_epoch());
                    state.effects.apply_with_layout(
                        token,
                        epoch,
                        state.format.sample_rate,
                        state.format.channels,
                        Some(state.pitch.channels()),
                    );
                    if state.effects.pinned_scope_on_callback().is_some()
                        && !self.returned.is_full()
                    {
                        self.returned
                            .push(self.state.take().unwrap())
                            .unwrap_or_else(|_| panic!("Actual state handback slot is empty"));
                        signals.handback_ack.store(serial, Ordering::Release);
                        output.fill(T::from_sample(0.0));
                        return;
                    }
                }
            }
            state.render(output);
            if let Some(signals) = &self.signals {
                // The callback has published its actual bank scope before the
                // owner may admit another replacement or commit this endpoint.
                signals.acknowledged.store(true, Ordering::Release);
            }
        } else {
            output.fill(T::from_sample(0.0));
        }
    }
}

impl Drop for RenderKernel {
    fn drop(&mut self) {
        let state = self
            .state
            .take()
            .or_else(|| self.handoff.as_mut().and_then(|handoff| handoff.pop().ok()));
        if let Some(state) = state {
            if self.signals.is_none() {
                state.shared.callback_alive.store(false, Ordering::Release);
            }
            // Exactly one RenderKernel owns the producer and sends exactly once.
            // The owner retains its consumer until the callback has been closed.
            let result = self.returned.push(state);
            assert!(result.is_ok(), "single callback return slot must be empty");
        }
        if let Some(signals) = &self.signals {
            signals.closed.store(true, Ordering::Release);
        }
    }
}

enum Backend {
    Device(DeviceHandle),
    Memory(thread::JoinHandle<()>),
}

/// Owned by the audio engine. The data callback accesses only fixed frame
/// storage, PCM atomics, control atomics, and the two nonblocking SPSC queues.
pub struct Output {
    effects: EffectsControl,
    spatial_targets: Producer<SpatialTarget>,
    spatial_owner: Option<SpatialOwner>,
    backend: Option<Backend>,
    shared: Arc<Shared>,
    commands: Producer<Install>,
    retired: Option<Consumer<Box<ConvertedSource>>>,
    returned: Option<Consumer<Box<RenderState>>>,
    local_snapshot: Mutex<AudioOutputSnapshot>,
    admitted: usize,
    next_token: u64,
    control: Option<PcmControl>,
}
struct SpatialOwner {
    plan: SpatialPlan,
    settings: crate::domain::audio_effects::EffectsSettings,
    impulse: Option<Arc<OwnedImpulse>>,
    clock: SpatialPhaseClock,
    anchor: std::time::Instant,
    graph_key: Option<(u64, u64, u32, u16, u16)>,
    pending_target: Option<SpatialTarget>,
    enhanced: bool,
}

impl Output {
    pub fn has_presented_media(&self) -> bool {
        let token = self.shared.presented_media_token.load(Ordering::Acquire);
        token != 0
            && token == self.token()
            && self.control.as_ref().is_some_and(|control| {
                control.current_epoch()
                    == self
                        .shared
                        .presented_media_source_epoch
                        .load(Ordering::Acquire)
            })
            && token == self.shared.presented_media_token.load(Ordering::Acquire)
    }
    pub fn spatial_applied_sequence(&self) -> u64 {
        let token = self.shared.effect_token.load(Ordering::Acquire);
        if token != self.token()
            || token == 0
            || self.control.as_ref().is_none_or(|control| {
                control.current_epoch() != self.shared.effect_source_epoch.load(Ordering::Acquire)
            })
        {
            return 0;
        }
        let sequence = self.shared.spatial_applied.load(Ordering::Acquire);
        if token != self.shared.effect_token.load(Ordering::Acquire) {
            return 0;
        }
        sequence
    }
    pub fn effects_snapshot(&self) -> crate::domain::audio_effects::EffectsSnapshot {
        use crate::domain::audio_effects::{EffectsSnapshot, EffectsStatus};
        let mut snapshot = EffectsSnapshot::default();
        if let Some(owner) = &self.spatial_owner {
            snapshot.desired = owner.settings.clone();
            snapshot.requested_sequence = owner.plan.sequence();
            if owner.enhanced {
                snapshot.status = EffectsStatus::Preparing;
            }
        }
        if let Some(current) = self.shared.effects_telemetry.read()
            && current.token != 0
            && current.token == self.token()
            && self
                .control
                .as_ref()
                .is_some_and(|control| control.current_epoch() == current.epoch)
            && current.sequence == self.shared.effect_applied.load(Ordering::Acquire)
            && current.sample_rate == self.format().sample_rate
            && current.device_channels == self.format().channels
            && current.token == self.token()
        {
            snapshot.applied_sequence = current.sequence;
            snapshot.sample_rate = Some(current.sample_rate);
            snapshot.effect_channels = current.effect_channels;
            snapshot.device_channels = current.device_channels;
            snapshot.latency_frames = current.latency;
            snapshot.tail_remaining_frames = current.tail;
            snapshot.transition_remaining_frames = current.transition;
            snapshot.compressor_reduction_db = current.reduction;
            snapshot.owned_bytes = current.bytes;
            snapshot.status = if current.sequence == snapshot.requested_sequence {
                EffectsStatus::Ready
            } else {
                EffectsStatus::Transitioning
            };
        }
        snapshot
    }
    pub fn configure_spatial(&mut self, settings: SpatialSettings, sequence: u64) -> Result<()> {
        let (mut effects, impulse) = self.spatial_owner.as_ref().map_or_else(
            || (Default::default(), None),
            |owner| (owner.settings.clone(), owner.impulse.clone()),
        );
        effects.spatial = settings;
        self.configure_effects(effects, sequence, impulse)
    }
    pub fn configure_effects(
        &mut self,
        settings: crate::domain::audio_effects::EffectsSettings,
        sequence: u64,
        impulse: Option<Arc<OwnedImpulse>>,
    ) -> Result<()> {
        settings.validate()?;
        match (&settings.convolution.impulse, &impulse) {
            (None, None) => {}
            (Some(id), Some(actual)) if id == actual.source_id() => {}
            _ => bail!("Effect impulse requires its matching actual decoded owner"),
        }
        anyhow::ensure!(sequence > 0, "Spatial sequence must be nonzero");
        anyhow::ensure!(
            !self.shared.shutdown.load(Ordering::Acquire),
            "Audio output is closed"
        );
        let format = self.format();
        let changed = self.spatial_owner.as_ref().is_none_or(|owner| {
            owner.plan.sequence() != sequence
                || owner.plan.sample_rate() != format.sample_rate
                || owner.settings != settings
        });
        if changed {
            let plan = SpatialPlan::prepare(settings.spatial, format.sample_rate, sequence)?;
            if let Some(owner) = &mut self.spatial_owner {
                anyhow::ensure!(
                    sequence >= owner.plan.sequence(),
                    "Spatial sequence is stale"
                );
                let now = owner.anchor.elapsed().as_nanos().min(u64::MAX as u128) as u64;
                owner.pending_target = Some(owner.clock.reconfigure(&plan, now)?);
                owner.plan = plan;
                owner.enhanced |=
                    settings.spatial.enabled || settings.convolution != Default::default();
                owner.settings = settings;
                owner.impulse = impulse;
                owner.graph_key = None;
            } else {
                let clock = SpatialPhaseClock::new(&plan, 0);
                self.spatial_owner = Some(SpatialOwner {
                    plan,
                    enhanced: settings.spatial.enabled
                        || settings.convolution != Default::default(),
                    settings,
                    impulse,
                    clock,
                    anchor: std::time::Instant::now(),
                    graph_key: None,
                    pending_target: None,
                });
            }
        }
        self.shared.effects_required.store(
            self.spatial_owner
                .as_ref()
                .is_some_and(|owner| owner.settings.convolution != Default::default()),
            Ordering::Release,
        );
        self.poll_spatial()
    }
    fn poll_spatial(&mut self) -> Result<()> {
        let Some(owner) = &mut self.spatial_owner else {
            return Ok(());
        };
        if self.shared.live_effects.retry_requested() {
            owner.graph_key = None;
        }
        let now = owner.anchor.elapsed().as_nanos().min(u64::MAX as u128) as u64;
        owner.pending_target = Some(owner.clock.sample(&owner.plan, now)?);
        *self
            .shared
            .spatial_latest_target
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = owner.pending_target;
        if let Some(target) = owner.pending_target.take()
            && let Err(PushError::Full(target)) = self.spatial_targets.push(target)
        {
            owner.pending_target = Some(target);
        }
        if !owner.enhanced {
            return Ok(());
        }
        let Some(control) = self.control.as_ref() else {
            return Ok(());
        };
        let physical = PcmFormat {
            sample_rate: self.shared.format_rate.load(Ordering::Acquire),
            channels: self.shared.format_channels.load(Ordering::Acquire) as u16,
        };
        let channels = control.format().channels.max(2);
        let key = (
            self.shared.token.load(Ordering::Acquire),
            control.current_epoch(),
            physical.sample_rate,
            channels,
            physical.channels,
        );
        if key.0 == 0 {
            owner.graph_key = None;
            return Ok(());
        }
        if owner.graph_key == Some(key)
            || self.shared.live_effects.occupied()
            || !self.shared.ready.load(Ordering::Acquire)
        {
            return Ok(());
        }
        if self
            .effects
            .record_for_owner(self.effects.active_owner())
            .is_some_and(|record| {
                let plan = record.bounds().preparation();
                plan.token == key.0
                    && plan.source_epoch == key.1
                    && plan.format.sample_rate == key.2
                    && plan.format.channels == key.3
                    && plan.device_channels == key.4
                    && record.recipe().sequence() == owner.plan.sequence()
            })
        {
            owner.graph_key = Some(key);
            return Ok(());
        }
        anyhow::ensure!(
            self.effects.owned_banks() < 2,
            "Effect bank preparation waits for actual retirement"
        );
        let recipe = EffectsRecipe::prepare(
            owner.settings.clone(),
            owner.plan.sequence(),
            channels,
            true,
            owner.impulse.clone(),
        )?;
        let pool = self.effects.bank_pool();
        let preparation = pool.begin(
            recipe,
            BankRequest {
                format: PcmFormat {
                    sample_rate: physical.sample_rate,
                    channels,
                },
                device_channels: physical.channels,
                token: key.0,
                source_epoch: key.1,
                initial_spatial: Some(
                    owner
                        .plan
                        .target(SpatialStep::ORIGIN)
                        .for_sample_rate(physical.sample_rate)?,
                ),
                replay_capacity: EFFECTS_MIN_REPLAY_FRAMES,
            },
        )?;
        let prepared = preparation
            .prepare(&AtomicBool::new(false))
            .map_err(|failure| anyhow::anyhow!(failure.error))?;
        let sequence = prepared.graph().sequence();
        anyhow::ensure!(
            self.effects.request_sequence(sequence),
            "Effect graph request is retired"
        );
        let old_record = self.effects.record_for_owner(self.effects.active_owner());
        if let Some(record) = old_record.filter(|record| {
            let old = record.bounds().preparation();
            old.token == key.0
                && old.source_epoch == key.1
                && old.format == prepared.graph().preparation().format
                && old.device_channels == key.4
                && old.sequence < sequence
        }) {
            let replacement = live_effects::PreparedLiveEffects::prepare(
                record,
                prepared,
                &self.shared.shutdown,
            )?;
            self.shared.live_effects.offer(replacement)?;
            owner.graph_key = Some(key);
            if let Some(Backend::Device(handle)) = &self.backend {
                handle.wake();
            }
            if let Some(Backend::Memory(renderer)) = &self.backend {
                renderer.thread().unpark();
            }
            return Ok(());
        }
        match self.effects.install_counted(prepared) {
            Ok(()) => owner.graph_key = Some(key),
            Err(bank) => {
                drop(bank);
                bail!("Effect banks are still owned; retry preparation after retirement");
            }
        }
        Ok(())
    }
    pub fn set_processing(
        &self,
        settings: PlaybackProcessingSettings,
        sequence: u64,
    ) -> Result<()> {
        settings.validate()?;
        let mut requested = self
            .shared
            .processing_requested
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        anyhow::ensure!(
            sequence > requested.sequence
                || (sequence == requested.sequence && settings == requested.settings),
            "Playback processing sequence is stale"
        );
        anyhow::ensure!(
            sequence != 0 || settings == PlaybackProcessingSettings::default(),
            "Playback processing sequence zero is reserved for startup"
        );
        *requested = ProcessingRequest { settings, sequence };
        self.shared.processing_plan.publish(*requested);
        Ok(())
    }

    pub fn processing_snapshot(&self) -> PlaybackProcessingSnapshot {
        let requested = *self
            .shared
            .processing_requested
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let mut observed = self
            .shared
            .processing_observed
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if let Some(snapshot) = self.shared.processing_telemetry.read() {
            *observed = snapshot;
        }
        let mut snapshot = *observed;
        snapshot.desired = requested.settings;
        snapshot.requested_sequence = requested.sequence;
        snapshot
    }

    pub fn has_pending_content(&self) -> bool {
        self.shared
            .processing_telemetry
            .pending
            .load(Ordering::Acquire)
    }

    pub fn processing_buffering(&self) -> bool {
        self.shared
            .processing_telemetry
            .buffering
            .load(Ordering::Acquire)
    }

    pub fn memory_render_timing(&self) -> Option<OutputRenderTiming> {
        matches!(self.backend, Some(Backend::Memory(_))).then(|| OutputRenderTiming {
            last_render_micros: self.shared.render_last.load(Ordering::Acquire),
            max_render_micros: self.shared.render_max.load(Ordering::Acquire),
            last_total_micros: self.shared.render_total.load(Ordering::Acquire),
            overrun_batches: self.shared.render_overruns.load(Ordering::Acquire),
        })
    }
    pub fn analysis_snapshot(&self) -> Option<super::analyser::AnalysisFrame> {
        let configuration = self.shared.analysis_configuration.load(Ordering::Acquire);
        if configuration & 1 == 0 {
            return None;
        }
        let (published, frame) = self.shared.analysis.read()?;
        if published != configuration >> 1 {
            return None;
        }
        let control = self.control.as_ref()?;
        let (epoch, unavailable) = control.analysis_state();
        (frame.stamp.token == self.shared.token.load(Ordering::Acquire)
            && frame.stamp.source_epoch == epoch
            && frame.sample_rate == self.shared.format_rate.load(Ordering::Acquire)
            && !self.shared.switching.load(Ordering::Acquire)
            && !unavailable
            && !self.shared.shutdown.load(Ordering::Acquire))
        .then_some(frame)
    }
    pub fn install_prepared_effects(
        &mut self,
        bank: Box<PreparedEffects>,
    ) -> std::result::Result<(), Box<PreparedEffects>> {
        self.effects.reclaim();
        let plan = bank.preparation();
        let Some(control) = self.control.as_ref() else {
            return Err(bank);
        };
        let format = self.format();
        if !self.is_ready()
            || self.shared.shutdown.load(Ordering::Acquire)
            || plan.token != self.token()
            || plan.source_epoch != control.current_epoch()
            || plan.format.sample_rate != format.sample_rate
            || plan.device_channels != format.channels
        {
            return Err(bank);
        }
        if !self.effects.request_sequence(plan.sequence) {
            return Err(bank);
        }
        self.effects.install(bank)
    }
    pub(crate) fn set_analysis_enabled(&self, enabled: bool, sequence: u64) {
        debug_assert!(sequence > 0 && sequence <= u64::MAX >> 1);
        self.shared
            .analysis_configuration
            .store((sequence << 1) | u64::from(enabled), Ordering::Release);
    }

    fn pair(format: PcmFormat) -> Result<(Self, RenderKernel)> {
        if format.sample_rate == 0
            || format.channels == 0
            || format.channels as usize > MAX_CHANNELS
        {
            bail!("unsupported audio output sample rate or channel count");
        }
        let shared = Arc::new(Shared::new()?);
        let pitch = PitchProcessor::new(format)?;
        shared
            .pitch_capacity
            .store(pitch.capacity_bound() as u64, Ordering::Release);
        shared
            .pitch_rate
            .store(format.sample_rate, Ordering::Release);
        shared
            .format_rate
            .store(format.sample_rate, Ordering::Release);
        shared
            .format_channels
            .store(format.channels as u32, Ordering::Release);
        let (commands, receiver) = RingBuffer::new(SOURCE_CAPACITY);
        let (retirement, retired) = RingBuffer::new(SOURCE_CAPACITY);
        let (handback, returned) = RingBuffer::new(1);
        let (effects, effects_render) = effect_exchange()?;
        let (spatial_tx, spatial_rx) = RingBuffer::new(8);
        let kernel = RenderKernel {
            state: Some(Box::new(RenderState {
                retained_pitch_latency_frames: 0,
                retained_pitch_source_latency_frames: 0,
                retained_pitch_source_rate: format.sample_rate,
                effects: effects_render,
                spatial_targets: spatial_rx,
                analysis: AnalysisTap::prepare(format)?,
                analysis_configuration_seen: 0,
                shared: shared.clone(),
                format,
                commands: receiver,
                retired: retirement,
                active: None,
                pending: None,
                equalizer: Equalizer::new(EqualizerPlan::flat(format.sample_rate)),
                equalizer_silent: true,
                equalizer_epoch: None,
                pitch,
                processing_sequence: 0,
                pitch_epoch: None,
            })),
            handoff: None,
            returned: handback,
            signals: None,
        };
        Ok((
            Self {
                effects,
                spatial_targets: spatial_tx,
                spatial_owner: None,
                backend: None,
                shared,
                commands,
                retired: Some(retired),
                returned: Some(returned),
                local_snapshot: Mutex::new(device_owner::memory_snapshot(format)),
                admitted: 0,
                next_token: 0,
                control: None,
            },
            kernel,
        ))
    }

    pub fn device() -> Result<Self> {
        Self::with_preferences(AudioPreferences::default())
    }

    pub fn with_preferences(preferences: AudioPreferences) -> Result<Self> {
        preferences.validate()?;
        let (mut output, mut kernel) = Self::pair(MEMORY_FORMAT)?;
        output.shared.ready.store(false, Ordering::Release);
        output.shared.device_managed.store(true, Ordering::Release);
        output
            .shared
            .muted
            .store(preferences.muted, Ordering::Release);
        let state = kernel.state.take().expect("new render state");
        let retired = output.retired.take().expect("new retirement receiver");
        let effect_reclaimer = output
            .effects
            .take_reclaimer()
            .expect("new effect retirement receiver");
        output.returned.take();
        output.backend = Some(Backend::Device(device_owner::start(
            output.shared.clone(),
            state,
            retired,
            effect_reclaimer,
            preferences,
        )?));
        Ok(output)
    }

    pub fn memory(observation: Arc<Mutex<RenderedPcm>>) -> Result<Self> {
        let (mut output, mut kernel) = Self::pair(MEMORY_FORMAT)?;
        let shared = output.shared.clone();
        let renderer = thread::Builder::new()
            .name("lx-pcm-renderer".into())
            .spawn(move || {
                let mut samples = [0.0f32; MEMORY_BATCH_FRAMES];
                let period = Duration::from_secs_f64(
                    MEMORY_BATCH_FRAMES as f64 / MEMORY_FORMAT.sample_rate as f64,
                );
                let mut next_deadline = std::time::Instant::now();
                while !shared.shutdown.load(Ordering::Acquire) {
                    live_effects::service_memory_owner(&mut kernel, &shared);
                    let started = std::time::Instant::now();
                    kernel.render(&mut samples);
                    let render_micros = started.elapsed().as_micros().min(u64::MAX as u128) as u64;
                    shared.render_last.store(render_micros, Ordering::Release);
                    shared.render_max.store(
                        shared.render_max.load(Ordering::Relaxed).max(render_micros),
                        Ordering::Release,
                    );
                    // Observation is deliberately outside the render kernel.
                    observe(&samples, &observation);
                    let elapsed = started.elapsed();
                    shared.render_total.store(
                        elapsed.as_micros().min(u64::MAX as u128) as u64,
                        Ordering::Release,
                    );
                    if elapsed > period {
                        shared.render_overruns.store(
                            shared
                                .render_overruns
                                .load(Ordering::Relaxed)
                                .saturating_add(1),
                            Ordering::Release,
                        );
                    }
                    next_deadline += period;
                    let now = std::time::Instant::now();
                    if now.saturating_duration_since(next_deadline) > period * 5 {
                        shared.render_overruns.store(
                            shared
                                .render_overruns
                                .load(Ordering::Relaxed)
                                .saturating_add(1),
                            Ordering::Release,
                        );
                        next_deadline = now + period;
                    }
                    while !shared.shutdown.load(Ordering::Acquire) {
                        let remaining =
                            next_deadline.saturating_duration_since(std::time::Instant::now());
                        if remaining.is_zero() {
                            break;
                        }
                        thread::park_timeout(remaining);
                    }
                }
            })
            .context("cannot start PCM output renderer")?;
        output.backend = Some(Backend::Memory(renderer));
        Ok(output)
    }

    pub fn format(&self) -> PcmFormat {
        PcmFormat {
            sample_rate: self.shared.format_rate.load(Ordering::Acquire),
            channels: self.shared.format_channels.load(Ordering::Acquire) as u16,
        }
    }

    pub fn is_ready(&self) -> bool {
        self.shared.ready.load(Ordering::Acquire)
    }

    pub fn snapshot(&self) -> AudioOutputSnapshot {
        match &self.backend {
            Some(Backend::Device(handle)) => handle.snapshot(),
            _ => self
                .local_snapshot
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .clone(),
        }
    }

    pub fn select(&self, selection: OutputSelection) -> Result<()> {
        selection.validate()?;
        match &self.backend {
            Some(Backend::Device(handle)) => handle.select(selection),
            _ => {
                let mut snapshot = self
                    .local_snapshot
                    .lock()
                    .unwrap_or_else(|error| error.into_inner());
                snapshot.preferences.selection = selection.clone();
                snapshot.state.desired = selection;
                Ok(())
            }
        }
    }

    pub fn refresh(&self) -> Result<()> {
        match &self.backend {
            Some(Backend::Device(handle)) => handle.refresh(),
            _ => Ok(()),
        }
    }

    pub fn set_mute(&self, muted: bool) {
        self.shared.muted.store(muted, Ordering::Release);
        match &self.backend {
            Some(Backend::Device(handle)) => handle.set_mute(muted),
            _ => {
                self.local_snapshot
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .preferences
                    .muted = muted
            }
        }
    }

    pub fn set_equalizer(&self, settings: EqualizerSettings, sequence: u64) -> Result<()> {
        anyhow::ensure!(
            !self.shared.shutdown.load(Ordering::Acquire),
            "audio output has closed"
        );
        settings.validate()?;
        let mut requested = self
            .shared
            .equalizer_requested
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        anyhow::ensure!(
            sequence > requested.sequence
                || (sequence == requested.sequence && settings == requested.settings),
            "Equalizer sequence is stale"
        );
        anyhow::ensure!(
            sequence != 0 || settings.is_flat(),
            "Equalizer sequence zero is reserved for flat startup"
        );
        let plan = if self.shared.device_managed.load(Ordering::Acquire) {
            None
        } else {
            Some(EqualizerPlan::prepare(
                settings,
                self.format().sample_rate,
                sequence,
            )?)
        };
        anyhow::ensure!(
            self.shared
                .equalizer_plan
                .version
                .load(Ordering::SeqCst)
                .checked_add(2)
                .is_some(),
            "Equalizer plan version exhausted"
        );
        *requested = EqualizerRequest { settings, sequence };
        drop(requested);
        if let Some(plan) = plan {
            assert!(
                self.shared.equalizer_plan.publish(plan),
                "admitted single-producer plan version"
            );
        }
        if let Some(Backend::Device(handle)) = &self.backend {
            handle.wake();
        }
        Ok(())
    }

    pub fn equalizer_snapshot(&self) -> EqualizerSnapshot {
        let requested = *self
            .shared
            .equalizer_requested
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let mut observed = self
            .shared
            .equalizer_observed
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if let Some(snapshot) = self.shared.equalizer_telemetry.read() {
            *observed = snapshot;
        }
        let mut snapshot = *observed;
        snapshot.desired = requested.settings;
        snapshot.requested_sequence = requested.sequence;
        snapshot
    }

    pub fn set_pause_on_device_change(&self, pause: bool) {
        match &self.backend {
            Some(Backend::Device(handle)) => handle.set_pause_policy(pause),
            _ => {
                self.local_snapshot
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .preferences
                    .pause_on_device_change = pause
            }
        }
    }

    pub fn take_device_change_pause(&self) -> bool {
        self.shared
            .device_change_pause
            .swap(false, Ordering::AcqRel)
    }

    pub fn shutdown(&mut self, timeout: Duration) -> bool {
        self.effects.begin_shutdown();
        self.shared.shutdown.store(true, Ordering::Release);
        self.shared.ready.store(false, Ordering::Release);
        self.clear();
        match &self.backend {
            Some(Backend::Device(handle)) => handle.shutdown(timeout),
            Some(Backend::Memory(renderer)) => {
                let started = std::time::Instant::now();
                renderer.thread().unpark();
                while !renderer.is_finished() {
                    if started.elapsed() >= timeout {
                        return false;
                    }
                    thread::sleep(
                        Duration::from_millis(1).min(timeout.saturating_sub(started.elapsed())),
                    );
                }
                if let Some(Backend::Memory(renderer)) = self.backend.take() {
                    let _ = renderer.join();
                }
                self.reap();
                self.local_snapshot
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .state
                    .status = crate::domain::audio_output::AudioOutputStatus::Closed;
                true
            }
            None => {
                self.reap();
                true
            }
        }
    }

    /// Admission counts all owned sources, including sources awaiting reclaim.
    /// Rejected sources are destroyed here on the engine thread.
    pub fn attach(&mut self, source: PcmSource) -> Result<PcmControl> {
        self.reap();
        if !self.is_ready() {
            bail!("audio output is not ready");
        }
        if !self.shared.callback_alive.load(Ordering::Acquire) {
            bail!("audio output callback has closed");
        }
        if self.admitted >= SOURCE_CAPACITY || self.commands.is_full() {
            bail!("audio source replacement queue is full; wait for reclamation");
        }
        let token = self
            .next_token
            .checked_add(1)
            .context("output token exhausted")?;
        let source = Box::new(ConvertedSource::new(source, self.format()));
        let control = source.control.clone();
        let install = Install {
            epoch: self.shared.epoch.load(Ordering::Acquire),
            token,
            source,
        };
        if self.commands.push(install).is_err() {
            bail!("audio source replacement queue is full");
        }
        self.next_token = token;
        self.admitted += 1;
        self.control = Some(control.clone());
        self.shared.ended_token.store(0, Ordering::Release);
        self.shared.token.store(token, Ordering::Release);
        Ok(control)
    }

    pub fn set_paused(&self, paused: bool) {
        self.shared.paused.store(paused, Ordering::Release);
    }

    pub fn set_gain(&self, gain: f32) {
        let gain = if gain.is_finite() {
            gain.clamp(0.0, 1.0)
        } else {
            0.0
        };
        self.shared.gain.store(gain.to_bits(), Ordering::Release);
    }

    /// Silence immediately; callback ownership is returned for output reclamation.
    /// Clearing never changes the decoder's EOF/cancel state.
    pub fn clear(&self) {
        self.shared
            .device_change_pause
            .store(false, Ordering::Release);
        self.shared.token.store(0, Ordering::Release);
        self.shared.ended_token.store(0, Ordering::Release);
        self.shared.epoch.fetch_add(1, Ordering::AcqRel);
        self.shared.live_effects.cancel_pending_on_owner();
    }

    pub fn token(&self) -> u64 {
        self.shared.token.load(Ordering::Acquire)
    }

    pub fn ended(&self) -> bool {
        let token = self.token();
        token != 0
            && self.shared.ended_token.load(Ordering::Acquire) == token
            && self.control.as_ref().is_some_and(|control| {
                control.render_state().0 == self.shared.ended_epoch.load(Ordering::Acquire)
            })
    }

    /// The callback moves boxes. Device reclamation stays on its reserved owner;
    /// Memory reclamation is performed here on the engine.
    pub fn reap(&mut self) -> usize {
        self.effects.reclaim();
        let mut reclaimed = self.shared.reclaimed.swap(0, Ordering::AcqRel);
        while let Some(source) = self.retired.as_mut().and_then(|retired| retired.pop().ok()) {
            drop(source);
            reclaimed += 1;
        }
        while let Some(mut state) = self
            .returned
            .as_mut()
            .and_then(|returned| returned.pop().ok())
        {
            state.effects.close_owned();
            if state.active.take().is_some() {
                reclaimed += 1;
            }
            if state.pending.take().is_some() {
                reclaimed += 1;
            }
            while state.commands.pop().is_ok() {
                reclaimed += 1;
            }
            drop(state);
        }
        self.admitted = self.admitted.saturating_sub(reclaimed);
        reclaimed
    }

    pub fn poll_error(&mut self) -> Option<OutputError> {
        for (flag, error) in [
            (1, OutputError::DeviceNotAvailable),
            (2, OutputError::StreamInvalidated),
            (4, OutputError::BufferUnderrun),
            (8, OutputError::BackendSpecific),
        ] {
            if self.shared.errors.fetch_and(!flag, Ordering::AcqRel) & flag != 0 {
                return Some(error);
            }
        }
        None
    }
}

impl Drop for Output {
    fn drop(&mut self) {
        self.shared.shutdown.store(true, Ordering::Release);
        self.clear();
        match self.backend.take() {
            Some(Backend::Device(handle)) => drop(handle),
            Some(Backend::Memory(renderer)) => {
                renderer.thread().unpark();
                let _ = renderer.join();
            }
            None => {}
        }
        self.reap();
    }
}

fn observe(samples: &[f32], observation: &Mutex<RenderedPcm>) {
    let mut squared = 0.0f64;
    let mut peak = 0.0f32;
    let mut previous = 0.0f32;
    let mut first_crossing = None;
    let mut last_crossing = 0.0f64;
    let mut crossings = 0usize;
    for (index, sample) in samples.iter().copied().enumerate() {
        squared += (sample as f64).powi(2);
        peak = peak.max(sample.abs());
        if previous < 0.0 && sample >= 0.0 && sample != previous {
            let crossing = index as f64 - 1.0 + (-previous / (sample - previous)) as f64;
            first_crossing.get_or_insert(crossing);
            last_crossing = crossing;
            crossings += 1;
        }
        previous = sample;
    }
    let mut state = observation
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    state.batches += 1;
    state.rms = (squared / samples.len() as f64).sqrt();
    state.peak = peak;
    state.frequency_hz = first_crossing
        .filter(|first| crossings > 1 && last_crossing > *first && peak > 0.001)
        .map(|first| {
            MEMORY_FORMAT.sample_rate as f64 * (crossings - 1) as f64 / (last_crossing - first)
        });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::pcm::PcmProducer;
    use crate::domain::equalizer::{EQUALIZER_FREQUENCIES_HZ, EqualizerPreset};
    use std::alloc::{GlobalAlloc, Layout, System};
    use std::cell::Cell;

    thread_local! {
        static WATCH_HEAP: Cell<bool> = const { Cell::new(false) };
        static HEAP_CALLS: Cell<(usize, usize)> = const { Cell::new((0, 0)) };
    }

    struct CheckedAllocator;

    // Instrumentation is restricted to the calling test thread and selected
    // callback invocations; concurrently running tests use System normally.
    unsafe impl GlobalAlloc for CheckedAllocator {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            let _ = WATCH_HEAP.try_with(|watch| {
                if watch.get() {
                    HEAP_CALLS.with(|calls| {
                        let (allocate, free) = calls.get();
                        calls.set((allocate + 1, free));
                    });
                }
            });
            unsafe { System.alloc(layout) }
        }

        unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
            let _ = WATCH_HEAP.try_with(|watch| {
                if watch.get() {
                    HEAP_CALLS.with(|calls| {
                        let (allocate, free) = calls.get();
                        calls.set((allocate, free + 1));
                    });
                }
            });
            unsafe { System.dealloc(pointer, layout) }
        }
    }

    #[global_allocator]
    static ALLOCATOR: CheckedAllocator = CheckedAllocator;

    pub(super) fn no_heap(operation: impl FnOnce()) {
        struct StopWatch;
        impl Drop for StopWatch {
            fn drop(&mut self) {
                WATCH_HEAP.with(|watch| watch.set(false));
            }
        }
        HEAP_CALLS.with(|calls| calls.set((0, 0)));
        WATCH_HEAP.with(|watch| watch.set(true));
        let guard = StopWatch;
        operation();
        drop(guard);
        assert_eq!(
            HEAP_CALLS.with(Cell::get),
            (0, 0),
            "callback touched the heap"
        );
    }

    fn pcm(format: PcmFormat, frames: usize) -> (PcmProducer, PcmSource, PcmControl) {
        PcmProducer::new(format, frames.max(1), 0, None).unwrap()
    }
    #[test]
    fn actual_effect_bank_pipeline_is_owned_until_final_callback_presentation_and_eof() {
        use crate::audio::{
            effects::{EFFECTS_MIN_REPLAY_FRAMES, EffectsAlignment, EffectsPreparation},
            spatial::{SpatialPlan, SpatialSettings, SpatialStep},
        };
        let format = PcmFormat {
            sample_rate: 22050,
            channels: 2,
        };
        let (mut producer, source, control) = pcm(format, 96);
        for index in 0..96 {
            assert!(producer.push_frame(index, &[0.1, -0.1]));
        }
        producer.finish();
        let (mut output, mut kernel) = Output::pair(format).unwrap();
        output.attach(source).unwrap();
        let spatial =
            SpatialPlan::prepare(SpatialSettings::default(), format.sample_rate, 1).unwrap();
        let prepared = PreparedEffects::prepare(
            EffectsPreparation {
                format,
                device_channels: 2,
                main_gain: 1.0,
                send_gain: 0.0,
                alignment: EffectsAlignment::Direct,
                sequence: 1,
                token: output.token(),
                source_epoch: control.current_epoch(),
                initial_spatial: spatial.target(SpatialStep::ORIGIN),
                replay_capacity: EFFECTS_MIN_REPLAY_FRAMES,
            },
            None,
            &AtomicBool::new(false),
        )
        .unwrap();
        assert!(output.install_prepared_effects(Box::new(prepared)).is_ok());
        let mut rendered = [0.0f32; 2];
        let mut audible = 0usize;
        for _ in 0..1024 {
            no_heap(|| kernel.render(&mut rendered));
            audible += usize::from(rendered.iter().any(|sample| sample.abs() > 0.001));
            if output.ended() {
                break;
            }
        }
        assert!(output.ended());
        assert_eq!(control.snapshot().consumed_frames, 96);
        assert!(audible >= 96);
        assert_eq!(output.effects.owned_banks(), 1);
        output.effects.begin_shutdown();
        no_heap(|| kernel.render(&mut rendered));
        output.reap();
        assert_eq!(output.effects.owned_banks(), 0);
    }

    #[test]
    fn configured_actual_impulse_reaches_counted_callback_graph_and_telemetry_without_heap_work() {
        use crate::audio::convolution::ImpulseBuilder;
        use crate::domain::audio_effects::{EffectsSettings, EffectsStatus, ImpulseResourceId};
        let format = PcmFormat {
            sample_rate: 22_050,
            channels: 2,
        };
        let (mut producer, source, control) = pcm(format, 96);
        for index in 0..96 {
            assert!(producer.push_frame(index, &[0.1, 0.1]));
        }
        producer.finish();
        let mut builder = ImpulseBuilder::prepare(format.sample_rate, 1, 3).unwrap();
        builder.push_frame(&[1.0]).unwrap();
        builder.push_frame(&[0.5]).unwrap();
        builder.push_frame(&[0.25]).unwrap();
        let id = ImpulseResourceId::Imported("actual-configured-response".into());
        let impulse =
            OwnedImpulse::from_validated(builder.finish().unwrap(), id.clone(), "a".repeat(64))
                .unwrap();
        let mut settings = EffectsSettings::default();
        settings.convolution.impulse = Some(id);
        settings.convolution.main_gain = 0.0;
        settings.convolution.send_gain = 1.0;
        let (mut output, mut kernel) = Output::pair(format).unwrap();
        assert!(output.configure_effects(settings.clone(), 7, None).is_err());
        assert_eq!(output.effects.owned_banks(), 0);
        output
            .configure_effects(settings.clone(), 7, Some(impulse))
            .unwrap();
        output.attach(source).unwrap();
        let mut waiting = [1.0_f32; 128];
        no_heap(|| kernel.render(&mut waiting));
        assert!(waiting.iter().all(|sample| *sample == 0.0));
        assert_eq!(control.snapshot().consumed_frames, 0);
        output
            .configure_spatial(SpatialSettings::default(), 7)
            .unwrap();
        assert_eq!(output.effects_snapshot().applied_sequence, 0);
        let mut frame = [0.0_f32; 2];
        let mut audible = 0;
        for _ in 0..2048 {
            no_heap(|| kernel.render(&mut frame));
            audible += usize::from(frame.iter().any(|sample| sample.abs() > 1e-7));
            if output.ended() {
                break;
            }
        }
        assert!(output.ended());
        assert_eq!(control.snapshot().consumed_frames, 96);
        assert!(audible >= 96);
        let observed = output.effects_snapshot();
        assert_eq!(observed.desired, settings);
        assert_eq!(observed.applied_sequence, 7);
        assert_eq!(observed.status, EffectsStatus::Ready);
        assert_eq!(observed.sample_rate, Some(format.sample_rate));
        assert_eq!(observed.effect_channels, 2);
        assert_eq!(observed.device_channels, 2);
        assert!(observed.latency_frames >= 512 && observed.owned_bytes > 0);
        output.effects.begin_shutdown();
        no_heap(|| kernel.render(&mut frame));
        output.reap();
        assert_eq!(output.effects.owned_banks(), 0);
    }
    #[test]
    fn mid_callback_actual_seek_waits_for_counted_epoch_graph_without_consuming_new_pcm() {
        use crate::audio::convolution::ImpulseBuilder;
        use crate::domain::audio_effects::{EffectsSettings, ImpulseResourceId};
        let format = PcmFormat {
            sample_rate: 22_050,
            channels: 2,
        };
        let (mut producer, source, control) = pcm(format, 128);
        for index in 0..64 {
            assert!(producer.push_frame(index, &[0.4, 0.4]));
        }
        let mut builder = ImpulseBuilder::prepare(format.sample_rate, 1, 1).unwrap();
        builder.push_frame(&[1.0]).unwrap();
        let id = ImpulseResourceId::Imported("mid-frame-actual-response".into());
        let impulse =
            OwnedImpulse::from_validated(builder.finish().unwrap(), id.clone(), "b".repeat(64))
                .unwrap();
        let mut settings = EffectsSettings::default();
        settings.convolution.impulse = Some(id);
        settings.convolution.main_gain = 0.0;
        settings.convolution.send_gain = 0.5;
        let (mut output, mut kernel) = Output::pair(format).unwrap();
        output
            .configure_effects(settings.clone(), 9, Some(impulse.clone()))
            .unwrap();
        output.attach(source).unwrap();
        output
            .configure_effects(settings.clone(), 9, Some(impulse.clone()))
            .unwrap();
        assert_eq!(output.effects.owned_banks(), 1);
        output
            .shared
            .effect_epoch_gate_frame
            .store(2, Ordering::Release);
        let gate = output.shared.clone();
        let observed = control.clone();
        let worker = std::thread::spawn(move || {
            struct Release(Arc<Shared>);
            impl Drop for Release {
                fn drop(&mut self) {
                    self.0
                        .effect_epoch_gate_release
                        .store(true, Ordering::Release);
                }
            }
            let _release = Release(gate.clone());
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            while !gate.effect_epoch_gate_entered.load(Ordering::Acquire) {
                assert!(
                    std::time::Instant::now() < deadline,
                    "Actual multi-frame callback did not enter the epoch gate"
                );
                std::thread::yield_now();
            }
            producer.begin_epoch().unwrap();
            for index in 1_000..1_032 {
                assert!(producer.push_frame(index, &[0.4, 0.4]));
            }
            producer.commit_epoch(1_000).unwrap();
            producer.finish();
            let baseline = observed.snapshot();
            (producer, baseline)
        });
        let mut rendered = [1.0f32; 128];
        no_heap(|| kernel.render(&mut rendered));
        let (producer, baseline) = worker.join().unwrap();
        let waiting = control.snapshot();
        assert_eq!(waiting.epoch, baseline.epoch);
        assert_eq!(waiting.queued_frames, 32);
        assert_eq!(producer.queued_frames(), 32);
        assert_eq!(waiting.consumed_frames, baseline.consumed_frames);
        assert_eq!(waiting.position_seconds, baseline.position_seconds);
        assert!(
            rendered[2..].iter().all(|sample| *sample == 0.0),
            "New epoch leaked unprocessed PCM: {rendered:?}"
        );
        output.reap();
        output
            .configure_effects(settings, 9, Some(impulse))
            .unwrap();
        assert_eq!(output.effects.owned_banks(), 1);
        let mut audible = false;
        let mut frame = [0.0f32; 2];
        for _ in 0..2_048 {
            no_heap(|| kernel.render(&mut frame));
            audible |= frame.iter().any(|sample| sample.abs() > 1e-7);
            if output.ended() {
                break;
            }
        }
        assert!(output.ended() && audible);
        let record = output
            .effects
            .record_for_owner(output.effects.active_owner())
            .unwrap();
        assert_eq!(record.bounds().preparation().source_epoch, waiting.epoch);
        assert_eq!(control.snapshot().queued_frames, 0);
        assert!(control.snapshot().consumed_frames > baseline.consumed_frames);
        assert_eq!(output.effects_snapshot().applied_sequence, 9);
        no_heap(|| drop(kernel));
        output.reap();
    }

    #[test]
    fn actual_analysis_fft_and_snapshot_handoff_touch_no_callback_heap_and_reject_paused_seek() {
        let (mut producer, source, control) = pcm(MEMORY_FORMAT, 2205);
        for index in 0..2205 {
            let sample = 0.4
                * (std::f32::consts::TAU * 997.0 * index as f32 / MEMORY_FORMAT.sample_rate as f32)
                    .sin();
            assert!(producer.push_frame(index as u64, &[sample]));
        }
        let (mut output, mut kernel) = Output::pair(MEMORY_FORMAT).unwrap();
        output.set_analysis_enabled(true, 2);
        output.attach(source).unwrap();
        let mut rendered = [0.0f32; 1470];
        no_heap(|| kernel.render(&mut rendered));
        let frame = output.analysis_snapshot().unwrap();
        assert_eq!(frame.sequence, 2);
        assert_eq!(frame.accepted_frames, 1470);
        assert!(frame.bins.iter().any(|bin| *bin > 0));
        output.set_analysis_enabled(false, 3);
        assert!(output.analysis_snapshot().is_none());
        output.set_analysis_enabled(true, 4);
        assert!(output.analysis_snapshot().is_none());
        let mut resumed = [0.0f32; 735];
        no_heap(|| kernel.render(&mut resumed));
        let frame = output.analysis_snapshot().unwrap();
        assert_eq!(frame.sequence, 1);
        assert_eq!(frame.accepted_frames, 735);
        output.set_paused(true);
        control.set_paused(true);
        no_heap(|| kernel.render(&mut rendered));
        assert_eq!(output.analysis_snapshot(), Some(frame));
        control.set_seeking(true);
        assert!(output.analysis_snapshot().is_none());
        control.set_seeking(false);
        producer.begin_epoch().unwrap();
        assert!(producer.push_frame(3000, &[0.0]));
        producer.commit_epoch(3000).unwrap();
        assert!(output.analysis_snapshot().is_none());
        output.set_paused(false);
        control.set_paused(false);
        let mut one = [0.0f32; 1];
        no_heap(|| kernel.render(&mut one));
        let reset = output.analysis_snapshot().unwrap();
        assert_ne!(reset.stamp.source_epoch, frame.stamp.source_epoch);
        assert_eq!(reset.sequence, 0);
        assert_eq!(reset.valid_window_frames, 0);
    }

    #[test]
    fn late_pause_replay_preserves_equalizer_history_of_the_next_unread_source_frame() {
        let (mut expected_pcm, expected_source, _) = pcm(MEMORY_FORMAT, 256);
        let (mut actual_pcm, actual_source, actual_control) = pcm(MEMORY_FORMAT, 256);
        for index in 0..256 {
            let value = if index == 0 { 0.5 } else { 0.0 };
            assert!(expected_pcm.push_frame(index, &[value]));
            assert!(actual_pcm.push_frame(index, &[value]));
        }
        let (mut expected_output, mut expected_kernel) = Output::pair(MEMORY_FORMAT).unwrap();
        let (mut actual_output, mut actual_kernel) = Output::pair(MEMORY_FORMAT).unwrap();
        expected_output
            .set_equalizer(EqualizerPreset::Rock.settings(), 1)
            .unwrap();
        actual_output
            .set_equalizer(EqualizerPreset::Rock.settings(), 1)
            .unwrap();
        expected_output.attach(expected_source).unwrap();
        actual_output.attach(actual_source).unwrap();
        let mut expected = [0.0f32; 64];
        no_heap(|| expected_kernel.render(&mut expected));
        actual_output
            .shared
            .fence_after_pitch
            .store(1, Ordering::Release);
        let mut held = [1.0f32; 1];
        no_heap(|| actual_kernel.render(&mut held));
        assert_eq!(held, [0.0]);
        assert_eq!(actual_control.snapshot().position_seconds, 0.0);
        assert!(!actual_output.has_presented_media());
        actual_output.set_paused(false);
        let mut actual = [0.0f32; 64];
        no_heap(|| actual_kernel.render(&mut actual));
        assert_eq!(
            actual, expected,
            "A retained pitch frame must freeze EQ history"
        );
        assert!(actual_output.has_presented_media());
        no_heap(|| drop(actual_kernel));
        no_heap(|| drop(expected_kernel));
        actual_output.reap();
        expected_output.reap();
    }

    #[test]
    fn seek_retired_enhanced_stereo_bus_is_folded_to_the_actual_mono_endpoint() {
        let bus = PcmFormat {
            sample_rate: 22050,
            channels: 2,
        };
        let (mut producer, source, control) = pcm(bus, 128);
        for index in 0..64 {
            assert!(producer.push_frame(index, &[0.0, 0.4]));
        }
        let (mut output, mut kernel) = Output::pair(MEMORY_FORMAT).unwrap();
        output.attach(source).unwrap();
        output
            .configure_spatial(
                SpatialSettings {
                    enabled: true,
                    ..Default::default()
                },
                1,
            )
            .unwrap();
        let mut sample = [0.0f32; 1];
        no_heap(|| kernel.render(&mut sample));
        assert_eq!(kernel.state.as_ref().unwrap().pitch.channels(), 2);
        producer.begin_epoch().unwrap();
        for index in 100..108 {
            assert!(producer.push_frame(index, &[0.0, 0.4]));
        }
        producer.commit_epoch(100).unwrap();
        no_heap(|| kernel.render(&mut sample));
        assert!(kernel.state.as_mut().unwrap().effects.active().is_none());
        assert!(
            (sample[0] - 0.2).abs() < 1e-6,
            "Uninstalled enhanced bus discarded R: {sample:?}"
        );
        assert!(control.snapshot().position_seconds > 0.0);
        no_heap(|| drop(kernel));
        output.reap();
    }

    #[test]
    fn stereo_frame_order_and_channel_mapping_are_exact() {
        let format = PcmFormat {
            sample_rate: 48_000,
            channels: 2,
        };
        let (mut producer, source, control) = pcm(format, 3);
        for (index, frame) in [[0.1, -0.2], [0.3, -0.4], [0.5, -0.6]].iter().enumerate() {
            assert!(producer.push_frame(index as u64, frame));
        }
        producer.finish();
        let (mut output, mut kernel) = Output::pair(format).unwrap();
        output.attach(source).unwrap();
        let mut samples = [0.0f32; 8];
        no_heap(|| kernel.render(&mut samples));
        assert_eq!(samples, [0.1, -0.2, 0.3, -0.4, 0.5, -0.6, 0.0, 0.0]);
        assert_eq!(control.snapshot().consumed_frames, 3);
        assert!(output.ended());
        assert!(control.consumer_alive());
        assert!(!control.snapshot().cancelled);

        let mut frame = [0.0; MAX_CHANNELS];
        frame[..4].copy_from_slice(&[0.2, 0.4, 0.6, 0.8]);
        assert_eq!(map_channels(frame, 4, 2)[..2], [0.4, 0.6]);
        assert!((map_channels(frame, 4, 1)[0] - 0.5).abs() < f32::EPSILON);
        let mut mono = [0.0; MAX_CHANNELS];
        mono[0] = 0.7;
        assert_eq!(map_channels(mono, 1, 8), [0.7; MAX_CHANNELS]);
    }

    fn frequency(samples: &[f32], sample_rate: u32) -> f64 {
        let mut first = None;
        let mut last = 0.0;
        let mut count = 0;
        for (index, pair) in samples.windows(2).enumerate() {
            if pair[0] < 0.0 && pair[1] >= 0.0 {
                let crossing = index as f64 + (-pair[0] / (pair[1] - pair[0])) as f64;
                first.get_or_insert(crossing);
                last = crossing;
                count += 1;
            }
        }
        sample_rate as f64 * (count - 1) as f64 / (last - first.unwrap())
    }

    #[test]
    fn multiple_sample_rates_and_channel_counts_preserve_tone_frequency() {
        for from_rate in [8_000, 22_050, 44_100, 48_000, 96_000] {
            for to_rate in [22_050, 44_100, 48_000] {
                for channels in [1, 2, 8] {
                    let from = PcmFormat {
                        sample_rate: from_rate,
                        channels,
                    };
                    let to = PcmFormat {
                        sample_rate: to_rate,
                        channels: 2,
                    };
                    let frames = from_rate as usize / 16 + 3;
                    let (mut producer, source, control) = pcm(from, frames);
                    for index in 0..frames {
                        let value =
                            (index as f32 * 997.0 * std::f32::consts::TAU / from_rate as f32).sin()
                                * 0.4;
                        assert!(
                            producer.push_frame(
                                index as u64,
                                &[value; MAX_CHANNELS][..channels as usize]
                            )
                        );
                    }
                    producer.finish();
                    let (mut output, mut kernel) = Output::pair(to).unwrap();
                    output.attach(source).unwrap();
                    let mut samples = vec![0.0f32; to_rate as usize / 20 * 2];
                    no_heap(|| kernel.render(&mut samples));
                    let left: Vec<_> = samples
                        .as_chunks::<2>()
                        .0
                        .iter()
                        .map(|frame| frame[0])
                        .collect();
                    let measured = frequency(&left, to_rate);
                    assert!(
                        (measured - 997.0).abs() < 1.0,
                        "{from_rate} -> {to_rate}, {channels} channels measured {measured}"
                    );
                    assert!(
                        samples
                            .as_chunks::<2>()
                            .0
                            .iter()
                            .all(|frame| frame[0] == frame[1])
                    );
                    let elapsed = control.snapshot().position_seconds;
                    assert!(
                        (elapsed - 0.05).abs() < 3.0 / from_rate as f64,
                        "media clock {elapsed} at source rate {from_rate}"
                    );
                }
            }
        }
    }

    #[test]
    fn pause_does_not_consume_and_zero_gain_still_consumes_media() {
        let (mut producer, source, control) = pcm(MEMORY_FORMAT, 20);
        for index in 0..20 {
            assert!(producer.push_frame(index, &[0.8]));
        }
        producer.finish();
        let (mut output, mut kernel) = Output::pair(MEMORY_FORMAT).unwrap();
        output.attach(source).unwrap();
        output.set_gain(0.25);
        let mut samples = [0.0f32; 4];
        no_heap(|| kernel.render(&mut samples));
        assert_eq!(samples, [0.2; 4]);
        assert_eq!(control.snapshot().consumed_frames, 4);

        output.set_paused(true);
        no_heap(|| kernel.render(&mut samples));
        assert_eq!(samples, [0.0; 4]);
        assert_eq!(control.snapshot().consumed_frames, 4);
        output.set_gain(0.0);
        output.set_paused(false);
        no_heap(|| kernel.render(&mut samples));
        assert_eq!(samples, [0.0; 4]);
        assert_eq!(control.snapshot().consumed_frames, 8);
        output.clear();
        no_heap(|| kernel.render(&mut samples));
        assert_eq!(control.snapshot().consumed_frames, 8);
        assert!(control.consumer_alive());
        assert_eq!(output.reap(), 1);
        assert!(!control.consumer_alive());
        assert!(!control.snapshot().cancelled);
    }

    #[test]
    fn underflow_is_silence_and_eof_retains_resumable_source() {
        let (mut producer, source, control) = pcm(MEMORY_FORMAT, 4);
        let (mut output, mut kernel) = Output::pair(MEMORY_FORMAT).unwrap();
        output.attach(source).unwrap();
        let mut samples = [1.0f32; 4];
        no_heap(|| kernel.render(&mut samples));
        assert_eq!(samples, [0.0; 4]);
        assert_eq!(control.snapshot().underflow_frames, 4);
        assert_eq!(control.snapshot().consumed_frames, 0);
        assert!(!output.ended());

        assert!(producer.push_frame(0, &[0.5]));
        producer.finish();
        no_heap(|| kernel.render(&mut samples));
        assert_eq!(samples, [0.5, 0.0, 0.0, 0.0]);
        assert!(output.ended());
        assert!(control.snapshot().drained);
        assert!(control.consumer_alive());
        assert!(!control.snapshot().cancelled);

        producer.begin_epoch().unwrap();
        assert!(producer.push_frame(100, &[0.4]));
        producer.commit_epoch(100).unwrap();
        producer.finish();
        assert!(!output.ended());
        no_heap(|| kernel.render(&mut samples));
        assert_eq!(samples, [0.4, 0.0, 0.0, 0.0]);
        assert!(output.ended());
        assert_eq!(control.snapshot().consumed_frames, 2);
        assert!((control.snapshot().position_seconds - 101.0 / 22_050.0).abs() < 1e-9);
    }

    #[test]
    fn resampler_outputs_last_media_frame_before_reporting_ended() {
        let from = PcmFormat {
            sample_rate: 8_000,
            channels: 1,
        };
        let to = PcmFormat {
            sample_rate: 48_000,
            channels: 1,
        };
        let (mut producer, source, control) = pcm(from, 1);
        assert!(producer.push_frame(0, &[0.4]));
        producer.finish();
        let (mut output, mut kernel) = Output::pair(to).unwrap();
        output.attach(source).unwrap();
        let mut samples = [0.0f32; 5];
        no_heap(|| kernel.render(&mut samples));
        assert_eq!(samples, [0.4; 5]);
        assert!(!output.ended());
        let mut final_frames = [0.0f32; 2];
        no_heap(|| kernel.render(&mut final_frames));
        assert_eq!(final_frames, [0.4, 0.0]);
        assert!(output.ended());
        assert_eq!(control.snapshot().consumed_frames, 1);
    }

    #[test]
    fn seek_invalidates_resampler_prefetch_without_a_paused_consumer() {
        let from = PcmFormat {
            sample_rate: 8_000,
            channels: 1,
        };
        let to = PcmFormat {
            sample_rate: 48_000,
            channels: 1,
        };
        let (mut producer, source, control) = pcm(from, 4);
        for index in 0..4 {
            assert!(producer.push_frame(index, &[0.7]));
        }
        let (mut output, mut kernel) = Output::pair(to).unwrap();
        output.attach(source).unwrap();
        let mut sample = [0.0f32; 1];
        kernel.render(&mut sample);
        assert_eq!(sample, [0.7]);
        output.set_paused(true);
        producer.begin_epoch().unwrap();
        assert!(producer.push_frame(800, &[-0.3]));
        assert!(producer.push_frame(801, &[-0.3]));
        producer.commit_epoch(800).unwrap();
        no_heap(|| kernel.render(&mut sample));
        assert_eq!(sample, [0.0]);
        assert!((control.snapshot().position_seconds - 0.1).abs() < 1e-9);
        output.set_paused(false);
        no_heap(|| kernel.render(&mut sample));
        assert_eq!(sample, [-0.3]);
    }

    #[test]
    fn replacement_admission_includes_retired_sources_and_reclaims_on_owner() {
        let (mut output, mut kernel) = Output::pair(MEMORY_FORMAT).unwrap();
        let mut controls = Vec::new();
        for index in 0..SOURCE_CAPACITY {
            let (mut producer, source, control) = pcm(MEMORY_FORMAT, 1);
            assert!(producer.push_frame(0, &[index as f32 / 10.0]));
            producer.finish();
            output.attach(source).unwrap();
            controls.push(control);
        }
        let (_, rejected, rejected_control) = pcm(MEMORY_FORMAT, 1);
        assert!(output.attach(rejected).is_err());
        assert!(!rejected_control.consumer_alive());
        let mut sample = [0.0f32; 1];
        no_heap(|| kernel.render(&mut sample));
        assert_eq!(sample, [0.7]);
        assert!(controls.iter().all(PcmControl::consumer_alive));
        assert_eq!(output.reap(), SOURCE_CAPACITY - 1);
        assert!(
            controls[..SOURCE_CAPACITY - 1]
                .iter()
                .all(|control| !control.consumer_alive())
        );
        assert!(controls.last().unwrap().consumer_alive());
        let (mut producer, source, _) = pcm(MEMORY_FORMAT, 1);
        assert!(producer.push_frame(0, &[0.2]));
        producer.finish();
        output.attach(source).unwrap();
        no_heap(|| kernel.render(&mut sample));
        assert_eq!(sample, [0.2]);
        assert_eq!(output.reap(), 1);
        assert!(!controls.last().unwrap().consumer_alive());
    }

    #[test]
    fn callback_destruction_hands_back_all_sources_without_heap_work() {
        let (mut output, kernel) = Output::pair(MEMORY_FORMAT).unwrap();
        let mut controls = Vec::new();
        for _ in 0..SOURCE_CAPACITY {
            let (_, source, control) = pcm(MEMORY_FORMAT, 1);
            output.attach(source).unwrap();
            controls.push(control);
        }
        no_heap(|| drop(kernel));
        assert!(controls.iter().all(PcmControl::consumer_alive));
        assert_eq!(output.reap(), SOURCE_CAPACITY);
        assert!(controls.iter().all(|control| !control.consumer_alive()));
        let (_, source, _) = pcm(MEMORY_FORMAT, 1);
        assert!(output.attach(source).is_err());
    }

    #[test]
    fn error_sideband_is_nonblocking_and_coalesces_repeated_categories() {
        let (mut output, _kernel) = Output::pair(MEMORY_FORMAT).unwrap();
        no_heap(|| {
            for _ in 0..1_000 {
                output
                    .shared
                    .report_error(&cpal::StreamError::BufferUnderrun);
                output
                    .shared
                    .report_error(&cpal::StreamError::DeviceNotAvailable);
            }
        });
        assert_eq!(output.poll_error(), Some(OutputError::DeviceNotAvailable));
        assert_eq!(output.poll_error(), Some(OutputError::BufferUnderrun));
        assert_eq!(output.poll_error(), None);
    }

    #[test]
    fn actual_f32_kernel_matches_all_independent_single_preset_and_stress_impulses() {
        let reference: serde_json::Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/equalizer_reference.json"
        ))
        .unwrap();
        let singles = reference["single_section_cases"].as_array().unwrap();
        let presets = reference["preset_cascade_cases"].as_array().unwrap();
        let stress = reference["stress_cascade_cases"].as_array().unwrap();
        assert_eq!(singles.len() + presets.len() + stress.len(), 186);
        for case in singles.iter().chain(presets).chain(stress) {
            let format = PcmFormat {
                sample_rate: case["sample_rate_hz"].as_u64().unwrap() as u32,
                channels: 1,
            };
            let mut settings = EqualizerSettings::default();
            let (waveform, amplitude) =
                if let Some(frequency) = case["requested_frequency_hz"].as_u64() {
                    let band = EQUALIZER_FREQUENCIES_HZ
                        .iter()
                        .position(|&value| value as u64 == frequency)
                        .unwrap();
                    settings.gains_db[band] = case["gain_db"].as_f64().unwrap();
                    (&case["unit_impulse_first_64"], 0.001f32)
                } else {
                    settings.gains_db =
                        std::array::from_fn(|band| case["gains_db"][band].as_f64().unwrap());
                    (
                        &case["unit_impulse_first_128"],
                        if settings.gains_db == [15.0; 10] {
                            1e-8f32
                        } else {
                            0.001f32
                        },
                    )
                };
            let waveform = waveform.as_array().unwrap();
            let (mut producer, source, control) = pcm(format, waveform.len());
            for frame in 0..waveform.len() {
                assert!(
                    producer.push_frame(frame as u64, &[if frame == 0 { amplitude } else { 0.0 }])
                );
            }
            producer.finish();
            let (mut output, mut kernel) = Output::pair(format).unwrap();
            output.set_equalizer(settings, 5).unwrap();
            output.attach(source).unwrap();
            let mut actual = vec![0.0f32; waveform.len()];
            no_heap(|| kernel.render(&mut actual));
            for (frame, (&sample, expected)) in actual.iter().zip(waveform).enumerate() {
                let actual = sample as f64 / amplitude as f64;
                let expected = expected.as_f64().unwrap();
                assert!(
                    (actual - expected).abs() <= 2e-6 + 2e-5 * expected.abs(),
                    "frame {frame}: {actual} != {expected}"
                );
            }
            assert_eq!(control.snapshot().consumed_frames, waveform.len() as u64);
            let snapshot = output.equalizer_snapshot();
            assert_eq!(snapshot.applied, settings);
            assert_eq!(snapshot.applied_sequence, 5);
            assert_eq!(snapshot.sample_rate, Some(format.sample_rate));
            assert_eq!(snapshot.transition_remaining_frames, 0);
            assert_eq!(snapshot.clipped_samples, 0);
            assert_eq!(snapshot.non_finite_samples, 0);
            no_heap(|| drop(kernel));
            output.reap();
        }
    }

    #[test]
    fn equalizer_flat_is_bit_exact_and_eight_channels_have_independent_histories() {
        let format = PcmFormat {
            sample_rate: 48_000,
            channels: 8,
        };
        let (mut producer, source, _) = pcm(format, 3);
        let values = [0.1f32, -0.2, -0.0, 0.3, f32::MIN_POSITIVE, -0.6, 0.7, -0.9];
        assert!(producer.push_frame(0, &values));
        producer.finish();
        let (mut output, mut kernel) = Output::pair(format).unwrap();
        output
            .set_equalizer(EqualizerSettings::default(), 9)
            .unwrap();
        output.attach(source).unwrap();
        let mut actual = [0.0f32; 8];
        no_heap(|| kernel.render(&mut actual));
        assert_eq!(actual.map(f32::to_bits), values.map(f32::to_bits));
        let (mut producer, source, _) = pcm(format, 128);
        for frame in 0..128 {
            let mut input = [0.0f32; 8];
            if frame == 0 {
                input[3] = 0.001;
            }
            assert!(producer.push_frame(frame, &input));
        }
        producer.finish();
        output
            .set_equalizer(EqualizerPreset::Rock.settings(), 10)
            .unwrap();
        output.attach(source).unwrap();
        let mut actual = [0.0f32; 128 * 8];
        no_heap(|| kernel.render(&mut actual));
        assert!(actual.as_chunks::<8>().0.iter().all(|frame| {
            frame
                .iter()
                .enumerate()
                .all(|(channel, &sample)| channel == 3 || sample == 0.0)
        }));
        assert!(
            actual
                .as_chunks::<8>()
                .0
                .iter()
                .any(|frame| frame[3] != 0.0)
        );
    }

    #[test]
    fn crossfade_coalesces_latest_plan_consumes_each_frame_once_and_reports_actual_target() {
        let (mut producer, source, control) = pcm(MEMORY_FORMAT, 2_000);
        for frame in 0..2_000 {
            assert!(producer.push_frame(
                frame,
                &[(frame as f32 * std::f32::consts::TAU * 1_000.0 / 22_050.0).cos() * 0.01]
            ));
        }
        producer.finish();
        let (mut output, mut kernel) = Output::pair(MEMORY_FORMAT).unwrap();
        output.attach(source).unwrap();
        kernel.render(&mut [0.0f32; 16]);
        let mut first = EqualizerSettings::default();
        first.gains_db[5] = 6.0;
        output.set_equalizer(first, 1).unwrap();
        let mut samples = [0.0f32; 11];
        no_heap(|| kernel.render(&mut samples));
        let snapshot = output.equalizer_snapshot();
        assert_eq!(snapshot.applied, first);
        assert_eq!(snapshot.applied_sequence, 1);
        assert_eq!(snapshot.transition_remaining_frames, 430);
        let mut second = first;
        second.gains_db[5] = -6.0;
        output.set_equalizer(second, 2).unwrap();
        no_heap(|| kernel.render(&mut samples));
        let mut latest = first;
        latest.gains_db[5] = 3.0;
        output.set_equalizer(latest, 3).unwrap();
        no_heap(|| kernel.render(&mut samples));
        let snapshot = output.equalizer_snapshot();
        assert_eq!(snapshot.desired, latest);
        assert_eq!(snapshot.applied, first);
        assert_eq!(snapshot.applied_sequence, 1);
        let mut finish = [0.0f32; 408];
        no_heap(|| kernel.render(&mut finish));
        let snapshot = output.equalizer_snapshot();
        assert_eq!(snapshot.applied_sequence, 3);
        assert_eq!(snapshot.applied, latest);
        assert_eq!(snapshot.transition_remaining_frames, 441);
        no_heap(|| kernel.render(&mut [0.0f32; 441]));
        assert_eq!(output.equalizer_snapshot().transition_remaining_frames, 0);
        assert_eq!(control.snapshot().consumed_frames, 16 + 11 * 3 + 408 + 441);
        assert_eq!(output.equalizer_snapshot().clipped_samples, 0);
        assert!(
            finish
                .windows(2)
                .map(|pair| (pair[1] - pair[0]).abs())
                .fold(0.0f32, f32::max)
                < 0.006
        );
    }

    #[test]
    fn pause_seek_underflow_replacement_and_eof_cannot_leak_old_equalizer_ringing() {
        let (mut producer, source, control) = pcm(MEMORY_FORMAT, 8);
        assert!(producer.push_frame(0, &[0.001]));
        let (mut output, mut kernel) = Output::pair(MEMORY_FORMAT).unwrap();
        output
            .set_equalizer(EqualizerPreset::Rock.settings(), 1)
            .unwrap();
        output.attach(source).unwrap();
        let mut first = [0.0f32; 2];
        no_heap(|| kernel.render(&mut first));
        assert_ne!(first[0], 0.0);
        assert_eq!(first[1], 0.0, "underflow leaked filter history");
        assert!(producer.push_frame(1, &[0.001]));
        kernel.render(&mut [0.0f32; 1]);
        let consumed = control.snapshot().consumed_frames;
        output.set_paused(true);
        let mut samples = [9.0f32; 4];
        no_heap(|| kernel.render(&mut samples));
        assert_eq!(samples, [0.0; 4]);
        assert_eq!(control.snapshot().consumed_frames, consumed);
        producer.begin_epoch().unwrap();
        for frame in 100..104 {
            assert!(producer.push_frame(frame, &[0.0]));
        }
        producer.commit_epoch(100).unwrap();
        producer.finish();
        output.set_paused(false);
        no_heap(|| kernel.render(&mut samples));
        assert_eq!(samples, [0.0; 4], "seek leaked filter history");
        no_heap(|| kernel.render(&mut samples));
        assert_eq!(samples, [0.0; 4], "EOF leaked filter history");
        let (mut next, source, _) = pcm(MEMORY_FORMAT, 4);
        for frame in 0..4 {
            assert!(next.push_frame(frame, &[0.0]));
        }
        next.finish();
        output.attach(source).unwrap();
        no_heap(|| kernel.render(&mut samples));
        assert_eq!(samples, [0.0; 4]);
    }

    #[test]
    fn clipping_non_finite_and_mute_telemetry_are_from_actual_rendered_pcm() {
        let (mut producer, source, control) = pcm(MEMORY_FORMAT, 4);
        for (frame, value) in [1.0f32, f32::NAN, 0.5, 0.5].into_iter().enumerate() {
            assert!(producer.push_frame(frame as u64, &[value]));
        }
        producer.finish();
        let (mut output, mut kernel) = Output::pair(MEMORY_FORMAT).unwrap();
        output
            .set_equalizer(
                EqualizerSettings {
                    gains_db: [15.0; 10],
                },
                6,
            )
            .unwrap();
        output.attach(source).unwrap();
        let mut samples = [0.0f32; 2];
        no_heap(|| kernel.render(&mut samples));
        let snapshot = output.equalizer_snapshot();
        assert_eq!(samples[0], 1.0);
        assert_eq!(samples[1], 0.0);
        assert!(snapshot.clipped_samples > 0);
        assert_eq!(snapshot.non_finite_samples, 1);
        assert!(snapshot.peak_before_clamp > 1.0);
        output.set_mute(true);
        no_heap(|| kernel.render(&mut samples));
        assert_eq!(samples, [0.0; 2]);
        assert_eq!(control.snapshot().consumed_frames, 4);
    }

    #[test]
    fn stale_rate_plan_shutdown_invalid_gain_and_version_exhaustion_preserve_admitted_state() {
        let (output, mut kernel) = Output::pair(MEMORY_FORMAT).unwrap();
        let settings = EqualizerPreset::Rock.settings();
        output.set_equalizer(settings, 11).unwrap();
        no_heap(|| kernel.render::<f32>(&mut []));
        assert_eq!(output.equalizer_snapshot().applied_sequence, 11);
        assert!(
            output.shared.equalizer_plan.publish(
                EqualizerPlan::prepare(EqualizerPreset::Pop.settings(), 48_000, 12).unwrap()
            )
        );
        no_heap(|| kernel.render::<f32>(&mut []));
        assert_eq!(output.equalizer_snapshot().applied_sequence, 11);
        assert_eq!(
            output.equalizer_snapshot().sample_rate,
            Some(MEMORY_FORMAT.sample_rate)
        );
        let mut invalid = settings;
        invalid.gains_db[0] = f64::NAN;
        assert!(output.set_equalizer(invalid, 13).is_err());
        assert_eq!(output.equalizer_snapshot().desired, settings);
        assert_eq!(output.equalizer_snapshot().requested_sequence, 11);
        output
            .shared
            .equalizer_plan
            .version
            .store(u64::MAX - 1, Ordering::SeqCst);
        assert!(
            output
                .set_equalizer(EqualizerPreset::Pop.settings(), 14)
                .is_err()
        );
        assert_eq!(
            output.shared.equalizer_plan.version.load(Ordering::SeqCst),
            u64::MAX - 1
        );
        assert_eq!(output.equalizer_snapshot().desired, settings);
        output.shared.shutdown.store(true, Ordering::Release);
        assert!(
            output
                .set_equalizer(EqualizerPreset::Pop.settings(), 15)
                .is_err()
        );
        assert_eq!(output.equalizer_snapshot().requested_sequence, 11);
    }

    #[test]
    fn telemetry_snapshot_remains_coherent_and_falls_back_to_last_actual_packet() {
        let (output, mut kernel) = Output::pair(MEMORY_FORMAT).unwrap();
        output
            .set_equalizer(EqualizerPreset::Vocal.settings(), 40)
            .unwrap();
        kernel.render::<f32>(&mut []);
        let actual = output.equalizer_snapshot();
        assert_eq!(actual.applied_sequence, 40);
        assert_eq!(actual.applied, EqualizerPreset::Vocal.settings());
        output
            .shared
            .equalizer_telemetry
            .version
            .fetch_add(1, Ordering::SeqCst);
        output
            .shared
            .equalizer_telemetry
            .sequence
            .store(41, Ordering::SeqCst);
        output
            .shared
            .equalizer_telemetry
            .rate
            .store(48_000, Ordering::SeqCst);
        let observed = output.equalizer_snapshot();
        assert_eq!(observed.applied_sequence, actual.applied_sequence);
        assert_eq!(observed.applied, actual.applied);
        assert_eq!(observed.sample_rate, actual.sample_rate);
        output
            .shared
            .equalizer_telemetry
            .version
            .fetch_add(1, Ordering::SeqCst);
        kernel.render::<f32>(&mut []);
        let recovered = output.equalizer_snapshot();
        assert_eq!(recovered.applied_sequence, 40);
        assert_eq!(recovered.sample_rate, Some(MEMORY_FORMAT.sample_rate));
    }

    #[test]
    fn upsampled_cached_underflow_and_final_eof_cannot_reintroduce_equalizer_history() {
        let from = PcmFormat {
            sample_rate: 8_000,
            channels: 1,
        };
        let to = PcmFormat {
            sample_rate: 48_000,
            channels: 1,
        };
        for eof in [false, true] {
            let (mut producer, source, control) = pcm(from, 8);
            assert!(producer.push_frame(0, &[0.001]));
            if eof {
                producer.finish();
            }
            let (mut output, mut kernel) = Output::pair(to).unwrap();
            output
                .set_equalizer(EqualizerPreset::Rock.settings(), 1)
                .unwrap();
            output.attach(source).unwrap();
            let mut samples = [0.0f32; 13];
            no_heap(|| kernel.render(&mut samples));
            assert!(samples[..6].iter().all(|sample| *sample != 0.0));
            assert_eq!(
                samples[6..],
                [0.0; 7],
                "cached gap/EOF leaked filter ringing, eof={eof}"
            );
            assert_eq!(control.snapshot().consumed_frames, 1);
            assert!((control.snapshot().position_seconds - 1.0 / 8_000.0).abs() < 1e-12);
            assert_eq!(output.ended(), eof);
        }
    }

    #[test]
    fn actual_pitch_fft_callback_defers_clock_and_muted_eof_drains_without_heap_operations() {
        let count = 1500;
        let settings = PlaybackProcessingSettings {
            pitch_ratio: 1.25,
            ..PlaybackProcessingSettings::default()
        };
        let (mut producer, source, control) = pcm(MEMORY_FORMAT, count);
        for index in 0..count {
            let sample = (index as f32 * std::f32::consts::TAU * 997.0 / 22_050.0).cos() * 0.1;
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
        let (mut output, mut kernel) = Output::pair(MEMORY_FORMAT).unwrap();
        output.set_processing(settings, 7).unwrap();
        let attached = output.attach(source).unwrap();
        let mut samples = [1.0f32; 128];
        for _ in 0..super::super::pitch::PITCH_WINDOW / 128 {
            no_heap(|| kernel.render(&mut samples));
            assert_eq!(samples, [0.0; 128]);
            assert_eq!(control.snapshot().position_seconds, 0.0);
            assert!(output.processing_snapshot().sample_rate.is_none());
            assert!(!output.ended());
        }
        assert!(output.has_pending_content());
        no_heap(|| kernel.render(&mut samples));
        assert!(samples.iter().any(|sample| *sample != 0.0));
        assert!(control.snapshot().position_seconds > 0.0);
        output.set_mute(true);
        let mut previous = control.snapshot().position_seconds;
        let mut batches = 0;
        while !output.ended() {
            no_heap(|| kernel.render(&mut samples));
            assert_eq!(samples, [0.0; 128]);
            let position = control.snapshot().position_seconds;
            assert!(position >= previous);
            previous = position;
            batches += 1;
            assert!(batches < 100);
        }
        assert_eq!(control.snapshot().consumed_frames, count as u64);
        assert!((previous - count as f64 / 22_050.0).abs() < 1e-12);
        let actual = output.processing_snapshot();
        assert_eq!(actual.applied, settings);
        assert_eq!(actual.applied_sequence, 7);
        assert_eq!(
            actual.latency_frames,
            super::super::pitch::PITCH_WINDOW as u32
        );
        assert_eq!(actual.sample_rate, Some(22_050));
        assert!(!output.has_pending_content());
        assert!(control.consumer_alive());
        assert!(!control.snapshot().cancelled);
        assert_eq!(attached.snapshot().position_seconds, previous);
        no_heap(|| drop(kernel));
        output.reap();
    }

    #[test]
    fn pitch_pending_content_freezes_on_pause_and_new_seek_epoch_fences_every_old_stamp() {
        let count = super::super::pitch::PITCH_WINDOW + 1024;
        let settings = PlaybackProcessingSettings {
            pitch_ratio: 0.75,
            ..PlaybackProcessingSettings::default()
        };
        let (mut producer, source, control) = pcm(MEMORY_FORMAT, count);
        for index in 0..count {
            assert!(producer.push_processed_frame(
                ProcessingStamp {
                    end_frame: index as u64 + 1,
                    processing_sequence: 7,
                    settings,
                    ..ProcessingStamp::default()
                },
                &[0.1]
            ));
        }
        let (mut output, mut kernel) = Output::pair(MEMORY_FORMAT).unwrap();
        output.set_processing(settings, 7).unwrap();
        output.attach(source).unwrap();
        let token = output.token();
        let mut samples = [9.0f32; 128];
        for _ in 0..16 {
            no_heap(|| kernel.render(&mut samples));
        }
        assert_eq!(control.snapshot().position_seconds, 0.0);
        let pulled = control.snapshot().consumed_frames;
        output.set_paused(true);
        for _ in 0..4 {
            no_heap(|| kernel.render(&mut samples));
            assert_eq!(samples, [0.0; 128]);
            assert_eq!(control.snapshot().consumed_frames, pulled);
            assert_eq!(control.snapshot().position_seconds, 0.0);
        }
        output.set_paused(false);
        for _ in 0..17 {
            no_heap(|| kernel.render(&mut samples));
        }
        assert!(control.snapshot().position_seconds > 0.0);
        producer.begin_epoch().unwrap();
        for index in 100..108 {
            assert!(producer.push_processed_frame(
                ProcessingStamp {
                    end_frame: index + 1,
                    processing_sequence: 8,
                    ..ProcessingStamp::default()
                },
                &[-0.25]
            ));
        }
        producer.commit_epoch(100).unwrap();
        producer.finish();
        output
            .set_processing(PlaybackProcessingSettings::default(), 8)
            .unwrap();
        no_heap(|| kernel.render(&mut samples));
        assert_eq!(samples[..8], [-0.25; 8]);
        assert_eq!(samples[8..], [0.0; 120]);
        assert_eq!(output.token(), token);
        assert!((control.snapshot().position_seconds - 108.0 / 22_050.0).abs() < 1e-12);
        assert_eq!(output.processing_snapshot().applied_sequence, 8);
        assert_eq!(output.processing_snapshot().latency_frames, 0);
        assert!(output.ended());
    }

    #[test]
    fn late_pause_switch_and_decoder_pause_preserve_every_popped_pitch_stamp() {
        for fence in [1, 2, 3] {
            let count = super::super::pitch::PITCH_WINDOW + 256;
            let settings = PlaybackProcessingSettings {
                pitch_ratio: 1.25,
                ..PlaybackProcessingSettings::default()
            };
            let (mut producer, source, control) = pcm(MEMORY_FORMAT, count);
            for index in 0..count {
                let sample = (std::f32::consts::TAU * 997.0 * index as f32 / 22_050.0).cos() * 0.1;
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
            let (mut output, mut kernel) = Output::pair(MEMORY_FORMAT).unwrap();
            output.set_processing(settings, 7).unwrap();
            output.attach(source).unwrap();
            let mut warmup = [0.0f32; 4096];
            no_heap(|| kernel.render(&mut warmup));
            assert_eq!(control.snapshot().position_seconds, 0.0);
            output
                .shared
                .fence_after_pitch
                .store(fence, Ordering::Release);
            let mut sample = [9.0f32; 1];
            no_heap(|| kernel.render(&mut sample));
            assert_eq!(sample, [0.0]);
            assert_eq!(control.snapshot().position_seconds, 0.0);
            assert_eq!(control.snapshot().consumed_frames, 4097);
            assert!(output.has_pending_content());
            output.shared.paused.store(false, Ordering::Release);
            output.shared.switching.store(false, Ordering::Release);
            control.set_paused(false);
            no_heap(|| kernel.render(&mut sample));
            assert!((control.snapshot().position_seconds - 1.0 / 22_050.0).abs() < 1e-12);
            assert_eq!(
                control.snapshot().consumed_frames,
                4097,
                "replay must not pull another source frame"
            );
            let mut previous = control.snapshot().position_seconds;
            let mut presented = 1;
            let mut calls = 0;
            while !output.ended() {
                no_heap(|| kernel.render(&mut sample));
                let position = control.snapshot().position_seconds;
                if position > previous {
                    assert!(
                        ((position - previous) * 22_050.0 - 1.0).abs() < 1e-7,
                        "fence {fence} discarded an original interval"
                    );
                    presented += 1;
                }
                previous = position;
                calls += 1;
                assert!(calls < 3 * count);
            }
            assert_eq!(presented, count);
            assert_eq!(control.snapshot().consumed_frames, count as u64);
            assert!((previous - count as f64 / 22_050.0).abs() < 1e-12);
        }
    }

    #[test]
    fn decoder_pause_before_output_pause_freezes_existing_pitch_content() {
        let count = super::super::pitch::PITCH_WINDOW + 256;
        let settings = PlaybackProcessingSettings {
            pitch_ratio: 0.75,
            ..PlaybackProcessingSettings::default()
        };
        let (mut producer, source, control) = pcm(MEMORY_FORMAT, count);
        for index in 0..count {
            let sample = (std::f32::consts::TAU * 997.0 * index as f32 / 22_050.0).cos() * 0.1;
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
        let (mut output, mut kernel) = Output::pair(MEMORY_FORMAT).unwrap();
        output.set_processing(settings, 7).unwrap();
        output.attach(source).unwrap();
        no_heap(|| kernel.render(&mut [0.0f32; 2048]));
        control.set_paused(true);
        assert!(!output.shared.paused.load(Ordering::Acquire));
        let mut paused = [9.0f32; 128];
        no_heap(|| kernel.render(&mut paused));
        assert_eq!(paused, [0.0; 128]);
        assert_eq!(control.snapshot().consumed_frames, 2048);
        assert_eq!(control.snapshot().position_seconds, 0.0);
        control.set_paused(false);
        let mut previous = 0.0;
        let mut presented = 0;
        let mut calls = 0;
        while !output.ended() {
            no_heap(|| kernel.render(&mut [0.0f32; 1]));
            let position = control.snapshot().position_seconds;
            if position > previous {
                assert!(((position - previous) * 22_050.0 - 1.0).abs() < 1e-7);
                presented += 1;
            }
            previous = position;
            calls += 1;
            assert!(calls < 3 * count);
        }
        assert_eq!(presented, count);
        assert_eq!(control.snapshot().consumed_frames, count as u64);
    }
}

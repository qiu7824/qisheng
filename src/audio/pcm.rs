//! Bounded PCM handoff. The consumer only touches atomics and inline samples.

use crate::domain::playback_processing::PlaybackProcessingSettings;
use rodio::{ChannelCount, SampleRate, Source};
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

pub const MAX_CHANNELS: usize = 8;
pub const GLOBAL_PCM_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_CLOCK_FRAMES: u64 = u64::MAX - 1;
static RESERVED_PCM_BYTES: AtomicUsize = AtomicUsize::new(0);
const OPEN: u8 = 0;
const EOF: u8 = 1;
const ERROR: u8 = 2;
const CANCELLED: u8 = 3;

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize)]
pub struct PcmFormat {
    pub sample_rate: u32,
    pub channels: u16,
}

/// Original media end coordinate and the processing configuration carried by
/// one complete PCM frame. The fraction is Q0.32 of an original source frame.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ProcessingStamp {
    pub end_frame: u64,
    pub fraction: u32,
    pub processing_sequence: u64,
    pub settings: PlaybackProcessingSettings,
    pub latency_frames: u32,
}

impl ProcessingStamp {
    fn valid(self) -> bool {
        (self.end_frame < MAX_CLOCK_FRAMES
            || (self.end_frame == MAX_CLOCK_FRAMES && self.fraction == 0))
            && self.settings.playback_rate.is_finite()
            && (0.5..=2.0).contains(&self.settings.playback_rate)
            && self.settings.pitch_ratio.is_finite()
            && (0.5..=1.5).contains(&self.settings.pitch_ratio)
    }

    fn seconds(self, sample_rate: u32) -> f64 {
        (self.end_frame as f64 + self.fraction as f64 / 4_294_967_296.0) / sample_rate as f64
    }
}

struct AtomicStamp {
    end_frame: AtomicU64,
    fraction: AtomicU32,
    processing_sequence: AtomicU64,
    playback_rate: AtomicU64,
    preserves_pitch: AtomicBool,
    pitch_ratio: AtomicU64,
    latency_frames: AtomicU32,
}

impl AtomicStamp {
    fn new() -> Self {
        Self {
            end_frame: AtomicU64::new(0),
            fraction: AtomicU32::new(0),
            processing_sequence: AtomicU64::new(0),
            playback_rate: AtomicU64::new(1.0f64.to_bits()),
            preserves_pitch: AtomicBool::new(true),
            pitch_ratio: AtomicU64::new(1.0f64.to_bits()),
            latency_frames: AtomicU32::new(0),
        }
    }

    fn store(&self, stamp: ProcessingStamp) {
        self.end_frame.store(stamp.end_frame, Ordering::SeqCst);
        self.fraction.store(stamp.fraction, Ordering::SeqCst);
        self.processing_sequence
            .store(stamp.processing_sequence, Ordering::SeqCst);
        self.playback_rate
            .store(stamp.settings.playback_rate.to_bits(), Ordering::SeqCst);
        self.preserves_pitch
            .store(stamp.settings.preserves_pitch, Ordering::SeqCst);
        self.pitch_ratio
            .store(stamp.settings.pitch_ratio.to_bits(), Ordering::SeqCst);
        self.latency_frames
            .store(stamp.latency_frames, Ordering::SeqCst);
    }

    fn load(&self) -> ProcessingStamp {
        ProcessingStamp {
            end_frame: self.end_frame.load(Ordering::SeqCst),
            fraction: self.fraction.load(Ordering::SeqCst),
            processing_sequence: self.processing_sequence.load(Ordering::SeqCst),
            settings: PlaybackProcessingSettings {
                playback_rate: f64::from_bits(self.playback_rate.load(Ordering::SeqCst)),
                preserves_pitch: self.preserves_pitch.load(Ordering::SeqCst),
                pitch_ratio: f64::from_bits(self.pitch_ratio.load(Ordering::SeqCst)),
            },
            latency_frames: self.latency_frames.load(Ordering::SeqCst),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PcmError(pub String);

impl fmt::Display for PcmError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for PcmError {}

/// Decoder scratch is reserved before the first audio packet is decoded.
pub struct PcmReservation {
    bytes: usize,
}
impl PcmReservation {
    pub fn new(bytes: usize) -> Result<Self, PcmError> {
        RESERVED_PCM_BYTES
            .try_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                current
                    .checked_add(bytes)
                    .filter(|total| *total <= GLOBAL_PCM_BYTES)
            })
            .map_err(|_| PcmError("global PCM memory quota exceeded".into()))?;
        Ok(Self { bytes })
    }
    pub fn shrink_to(&mut self, bytes: usize) -> Result<(), PcmError> {
        if bytes > self.bytes {
            return Err(PcmError("decoder exceeds reserved PCM scratch".into()));
        }
        RESERVED_PCM_BYTES.fetch_sub(self.bytes - bytes, Ordering::AcqRel);
        self.bytes = bytes;
        Ok(())
    }
}
impl Drop for PcmReservation {
    fn drop(&mut self) {
        RESERVED_PCM_BYTES.fetch_sub(self.bytes, Ordering::AcqRel);
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct PcmSnapshot {
    pub epoch: u64,
    /// Cumulative complete frames published into this actual PCM ring.
    pub produced_frames: u64,
    pub position_seconds: f64,
    pub consumed_frames: u64,
    pub underflow_frames: u64,
    pub queued_frames: usize,
    pub capacity_frames: usize,
    pub allocated_bytes: usize,
    pub normal_eof: bool,
    pub drained: bool,
    pub failed: bool,
    pub cancelled: bool,
    pub processing_settings: PlaybackProcessingSettings,
    pub processing_sequence: u64,
    pub processing_latency_frames: u32,
    /// Source PCM rate for a coherent presented-frame acknowledgement. None
    /// before presentation, after a new seek baseline, or on a contended read.
    pub processing_sample_rate: Option<u32>,
}

struct Slot {
    // A committed slot at absolute index i has sequence 2*i+2; odd is writing.
    sequence: AtomicU64,
    epoch: AtomicU64,
    stamp: AtomicStamp,
    samples: [AtomicU32; MAX_CHANNELS],
}

impl Slot {
    fn new() -> Self {
        Self {
            sequence: AtomicU64::new(0),
            epoch: AtomicU64::new(0),
            stamp: AtomicStamp::new(),
            samples: std::array::from_fn(|_| AtomicU32::new(0)),
        }
    }
}

struct Ring {
    slots: Box<[Slot]>,
    head: AtomicU64,
    tail: AtomicU64,
    epoch: AtomicU64,
    clock_stamp: AtomicStamp,
    clock_version: AtomicU64,
    clock_valid: AtomicBool,
    clock_position: AtomicU64,
    clock_epoch: AtomicU64,
    baselines: [ClockBaseline; 2],
    committed_clock: AtomicU64,
    consumed: AtomicU64,
    underflows: AtomicU64,
    terminal: AtomicU8,
    paused: AtomicBool,
    seeking: AtomicBool,
    consumer_alive: AtomicBool,
    consumer_ended: AtomicBool,
    #[cfg(test)]
    pause_after_publication: AtomicBool,
    format: PcmFormat,
    allocated_bytes: usize,
}

struct ClockBaseline {
    epoch: AtomicU64,
    frame: AtomicU64,
}

impl Ring {
    fn render_state(&self) -> (u64, bool) {
        let epoch = self.epoch.load(Ordering::Acquire);
        let blocked = self.paused.load(Ordering::Acquire)
            || self.seeking.load(Ordering::Acquire)
            || matches!(self.terminal.load(Ordering::Acquire), ERROR | CANCELLED);
        (epoch, blocked)
    }

    fn commit_rendered(&self, stamp: ProcessingStamp, epoch: u64) -> bool {
        if !stamp.valid() || self.render_state() != (epoch, false) {
            return false;
        }
        let version = self.clock_version.load(Ordering::SeqCst);
        let Some(next) = version.checked_add(2) else {
            return false;
        };
        if version & 1 != 0
            || self
                .clock_version
                .compare_exchange(version, version + 1, Ordering::SeqCst, Ordering::SeqCst)
                .is_err()
        {
            return false;
        }
        if self.render_state() != (epoch, false) {
            self.clock_version.store(next, Ordering::SeqCst);
            return false;
        }
        if self.clock_valid.load(Ordering::SeqCst)
            && self.clock_epoch.load(Ordering::SeqCst) == epoch
            && (stamp.end_frame, stamp.fraction)
                < (
                    self.clock_stamp.end_frame.load(Ordering::SeqCst),
                    self.clock_stamp.fraction.load(Ordering::SeqCst),
                )
        {
            self.clock_version.store(next, Ordering::SeqCst);
            return false;
        }
        self.clock_stamp.store(stamp);
        self.clock_position.store(
            stamp.seconds(self.format.sample_rate).to_bits(),
            Ordering::SeqCst,
        );
        self.clock_valid.store(true, Ordering::SeqCst);
        self.clock_epoch.store(epoch, Ordering::SeqCst);
        self.clock_version.store(next, Ordering::SeqCst);
        #[cfg(test)]
        if self.pause_after_publication.swap(false, Ordering::Relaxed) {
            self.paused.store(true, Ordering::Release);
        }
        // Publishing the frame is the presentation boundary. A later pause
        // cannot turn that committed acknowledgement into a rejected frame.
        // A later seek is fenced by its independent committed epoch baseline.
        true
    }

    fn rendered_clock(&self) -> (f64, Option<ProcessingStamp>) {
        for _ in 0..4 {
            let tag = self.committed_clock.load(Ordering::SeqCst);
            let baseline = &self.baselines[(tag & 1) as usize];
            let epoch = baseline.epoch.load(Ordering::SeqCst);
            let target = baseline.frame.load(Ordering::SeqCst);
            let version = self.clock_version.load(Ordering::SeqCst);
            if version & 1 != 0 {
                continue;
            }
            let stamp = self.clock_stamp.load();
            let content_epoch = self.clock_epoch.load(Ordering::SeqCst);
            let valid = self.clock_valid.load(Ordering::SeqCst);
            if version == self.clock_version.load(Ordering::SeqCst)
                && tag == self.committed_clock.load(Ordering::SeqCst)
            {
                return if valid && content_epoch == epoch {
                    (stamp.seconds(self.format.sample_rate), Some(stamp))
                } else {
                    (target as f64 / self.format.sample_rate as f64, None)
                };
            }
        }
        // A contended callback never waits for a writer. The position is one
        // atomic scalar; unavailable stamp metadata is explicitly not acknowledged.
        let tag = self.committed_clock.load(Ordering::SeqCst);
        let baseline = &self.baselines[(tag & 1) as usize];
        let epoch = baseline.epoch.load(Ordering::SeqCst);
        let target = baseline.frame.load(Ordering::SeqCst);
        let content_epoch = self.clock_epoch.load(Ordering::SeqCst);
        let position = f64::from_bits(self.clock_position.load(Ordering::SeqCst));
        let valid = self.clock_valid.load(Ordering::SeqCst)
            && content_epoch == epoch
            && content_epoch == self.clock_epoch.load(Ordering::SeqCst)
            && tag == self.committed_clock.load(Ordering::SeqCst);
        if valid {
            (position, None)
        } else {
            let current_tag = self.committed_clock.load(Ordering::SeqCst);
            let current = &self.baselines[(current_tag & 1) as usize];
            let frame = if current_tag == tag {
                target
            } else {
                current.frame.load(Ordering::SeqCst)
            };
            (frame as f64 / self.format.sample_rate as f64, None)
        }
    }
}

impl Drop for Ring {
    fn drop(&mut self) {
        RESERVED_PCM_BYTES.fetch_sub(self.allocated_bytes, Ordering::AcqRel);
    }
}

#[derive(Clone)]
pub struct PcmControl {
    ring: Arc<Ring>,
}

impl PcmControl {
    /// Constant-time render fence; output resampling never holds stale seek frames.
    pub fn render_state(&self) -> (u64, bool) {
        self.ring.render_state()
    }
    pub(crate) fn analysis_state(&self) -> (u64, bool) {
        let epoch = self.ring.epoch.load(Ordering::Acquire);
        let unavailable = self.ring.seeking.load(Ordering::Acquire)
            || matches!(
                self.ring.terminal.load(Ordering::Acquire),
                ERROR | CANCELLED
            );
        (epoch, unavailable)
    }

    /// Presented media clock; deferred source lookahead is counted separately.
    pub fn snapshot(&self) -> PcmSnapshot {
        let tail = self.ring.tail.load(Ordering::Acquire);
        let head = self.ring.head.load(Ordering::Acquire);
        let queued = tail.saturating_sub(head).min(self.ring.slots.len() as u64) as usize;
        let terminal = self.ring.terminal.load(Ordering::Acquire);
        let (position_seconds, stamp) = self.ring.rendered_clock();
        PcmSnapshot {
            epoch: self.ring.epoch.load(Ordering::Acquire),
            produced_frames: tail,
            position_seconds,
            consumed_frames: self.ring.consumed.load(Ordering::Relaxed),
            underflow_frames: self.ring.underflows.load(Ordering::Relaxed),
            queued_frames: queued,
            capacity_frames: self.ring.slots.len(),
            allocated_bytes: self.ring.allocated_bytes,
            normal_eof: terminal == EOF,
            drained: terminal == EOF
                && queued == 0
                && self.ring.consumer_ended.load(Ordering::Acquire),
            failed: terminal == ERROR,
            cancelled: terminal == CANCELLED,
            processing_settings: stamp
                .map_or_else(PlaybackProcessingSettings::default, |stamp| stamp.settings),
            processing_sequence: stamp.map_or(0, |stamp| stamp.processing_sequence),
            processing_latency_frames: stamp.map_or(0, |stamp| stamp.latency_frames),
            processing_sample_rate: stamp.map(|_| self.ring.format.sample_rate),
        }
    }

    /// The unique renderer acknowledges a complete frame after its delayed
    /// samples have actually been emitted. No allocation or waiting occurs.
    pub fn commit_rendered_stamp(&self, stamp: ProcessingStamp, epoch: u64) -> bool {
        self.ring.commit_rendered(stamp, epoch)
    }

    pub fn current_epoch(&self) -> u64 {
        self.ring.epoch.load(Ordering::Acquire)
    }
    pub(crate) fn format(&self) -> PcmFormat {
        self.ring.format
    }

    /// Exact start/seek target, before any delayed output is presented. A busy
    /// seek publisher is never converted through a floating-point timestamp.
    pub fn presentation_baseline(&self) -> Option<(u64, u32, u64)> {
        for _ in 0..4 {
            let tag = self.ring.committed_clock.load(Ordering::SeqCst);
            let baseline = &self.ring.baselines[(tag & 1) as usize];
            let frame = baseline.frame.load(Ordering::SeqCst);
            let epoch = baseline.epoch.load(Ordering::SeqCst);
            if tag == self.ring.committed_clock.load(Ordering::SeqCst) {
                return Some((frame, 0, epoch));
            }
        }
        None
    }
    pub fn set_paused(&self, paused: bool) {
        self.ring.paused.store(paused, Ordering::Release);
    }
    pub fn set_seeking(&self, seeking: bool) {
        self.ring.seeking.store(seeking, Ordering::Release);
    }
    pub fn paused(&self) -> bool {
        self.ring.paused.load(Ordering::Acquire)
    }
    pub fn consumer_alive(&self) -> bool {
        self.ring.consumer_alive.load(Ordering::Acquire)
    }
    pub fn fail(&self) {
        self.ring.terminal.store(ERROR, Ordering::Release);
    }
    pub fn cancel(&self) {
        self.ring.terminal.store(CANCELLED, Ordering::Release);
    }
    pub fn globally_allocated_bytes() -> usize {
        RESERVED_PCM_BYTES.load(Ordering::Acquire)
    }
}

/// Single decoder-thread writer. It cannot be cloned.
pub struct PcmProducer {
    ring: Arc<Ring>,
    epoch: u64,
}

impl PcmProducer {
    /// PCM scratch storage is charged together with the queue against the global cap.
    pub fn new(
        format: PcmFormat,
        frames: usize,
        scratch_bytes: usize,
        duration: Option<Duration>,
    ) -> Result<(Self, PcmSource, PcmControl), PcmError> {
        Self::with_reservation(
            format,
            frames,
            PcmReservation::new(scratch_bytes)?,
            duration,
        )
    }

    pub fn with_reservation(
        format: PcmFormat,
        frames: usize,
        mut scratch: PcmReservation,
        duration: Option<Duration>,
    ) -> Result<(Self, PcmSource, PcmControl), PcmError> {
        if format.sample_rate == 0
            || format.channels == 0
            || format.channels as usize > MAX_CHANNELS
            || frames == 0
        {
            return Err(PcmError(
                "unsupported PCM rate, channel count or queue capacity".into(),
            ));
        }
        let queue_bytes = frames
            .checked_mul(std::mem::size_of::<Slot>())
            .and_then(|bytes| bytes.checked_add(std::mem::size_of::<[f32; MAX_CHANNELS]>()))
            .ok_or_else(|| PcmError("PCM allocation overflow".into()))?;
        let bytes = queue_bytes
            .checked_add(scratch.bytes)
            .ok_or_else(|| PcmError("PCM allocation overflow".into()))?;
        RESERVED_PCM_BYTES
            .try_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                current
                    .checked_add(queue_bytes)
                    .filter(|total| *total <= GLOBAL_PCM_BYTES)
            })
            .map_err(|_| PcmError("global PCM memory quota exceeded".into()))?;
        scratch.bytes = 0; // The Ring owns the entire reservation from this point.
        let ring = Arc::new(Ring {
            slots: (0..frames).map(|_| Slot::new()).collect(),
            head: AtomicU64::new(0),
            tail: AtomicU64::new(0),
            epoch: AtomicU64::new(1),
            clock_stamp: AtomicStamp::new(),
            clock_version: AtomicU64::new(0),
            clock_valid: AtomicBool::new(false),
            clock_position: AtomicU64::new(0.0f64.to_bits()),
            clock_epoch: AtomicU64::new(1),
            baselines: std::array::from_fn(|_| ClockBaseline {
                epoch: AtomicU64::new(1),
                frame: AtomicU64::new(0),
            }),
            committed_clock: AtomicU64::new(0),
            consumed: AtomicU64::new(0),
            underflows: AtomicU64::new(0),
            terminal: AtomicU8::new(OPEN),
            paused: AtomicBool::new(false),
            seeking: AtomicBool::new(false),
            consumer_alive: AtomicBool::new(true),
            consumer_ended: AtomicBool::new(false),
            #[cfg(test)]
            pause_after_publication: AtomicBool::new(false),
            format,
            allocated_bytes: bytes,
        });
        let producer = Self {
            ring: ring.clone(),
            epoch: 1,
        };
        let control = PcmControl { ring: ring.clone() };
        let source = PcmSource {
            ring,
            duration,
            frame: [0.0; MAX_CHANNELS],
            frame_epoch: 0,
            frame_stamp: ProcessingStamp::default(),
            completed_stamp: None,
            deferred_clock: false,
            channel: 0,
            content: false,
        };
        Ok((producer, source, control))
    }

    /// Returns false for backpressure. No allocation or waiting takes place.
    pub fn push_frame(&mut self, frame_index: u64, samples: &[f32]) -> bool {
        if frame_index >= MAX_CLOCK_FRAMES {
            return false;
        }
        self.push_processed_frame(
            ProcessingStamp {
                end_frame: frame_index + 1,
                ..Default::default()
            },
            samples,
        )
    }

    /// Stamps and channel samples are published in one immutable slot transaction.
    pub fn push_processed_frame(&mut self, stamp: ProcessingStamp, samples: &[f32]) -> bool {
        if samples.len() != self.ring.format.channels as usize || !stamp.valid() {
            return false;
        }
        let tail = self.ring.tail.load(Ordering::Relaxed);
        if tail.saturating_sub(self.ring.head.load(Ordering::Acquire))
            >= self.ring.slots.len() as u64
        {
            return false;
        }
        let slot = &self.ring.slots[(tail % self.ring.slots.len() as u64) as usize];
        slot.sequence
            .store(tail.wrapping_mul(2).wrapping_add(1), Ordering::SeqCst);
        slot.epoch.store(self.epoch, Ordering::SeqCst);
        slot.stamp.store(stamp);
        for (target, sample) in slot.samples.iter().zip(samples) {
            target.store(sample.to_bits(), Ordering::SeqCst);
        }
        slot.sequence
            .store(tail.wrapping_mul(2).wrapping_add(2), Ordering::SeqCst);
        self.ring.tail.store(tail + 1, Ordering::Release);
        true
    }

    /// Clear unread data without waiting for a paused consumer. Atomic slot values
    /// and sequence validation also make reset concurrent with next() safe.
    pub fn begin_epoch(&mut self) -> Result<u64, PcmError> {
        self.epoch = self
            .epoch
            .checked_add(1)
            .ok_or_else(|| PcmError("PCM seek epoch exhausted".into()))?;
        self.ring.seeking.store(true, Ordering::Release);
        self.ring.epoch.store(self.epoch, Ordering::Release);
        self.ring
            .head
            .fetch_max(self.ring.tail.load(Ordering::Acquire), Ordering::AcqRel);
        self.ring.terminal.store(OPEN, Ordering::Release);
        self.ring.consumer_ended.store(false, Ordering::Release);
        Ok(self.epoch)
    }
    pub fn commit_epoch(&mut self, frame: u64) -> Result<(), PcmError> {
        if frame > MAX_CLOCK_FRAMES {
            return Err(PcmError("media position exceeds PCM clock range".into()));
        }
        let tag = self
            .ring
            .committed_clock
            .load(Ordering::Relaxed)
            .wrapping_add(1);
        let baseline = &self.ring.baselines[(tag & 1) as usize];
        baseline.frame.store(frame, Ordering::SeqCst);
        baseline.epoch.store(self.epoch, Ordering::SeqCst);
        self.ring.committed_clock.store(tag, Ordering::SeqCst);
        self.ring.seeking.store(false, Ordering::Release);
        Ok(())
    }
    pub fn finish(&mut self) {
        self.ring.terminal.store(EOF, Ordering::Release);
    }
    pub fn queued_frames(&self) -> usize {
        self.ring
            .tail
            .load(Ordering::Acquire)
            .saturating_sub(self.ring.head.load(Ordering::Acquire)) as usize
    }
    pub fn capacity_frames(&self) -> usize {
        self.ring.slots.len()
    }
}

/// Interleaved source shared by the device mixer and deterministic Memory output.
/// Underflow, pause, seek and failure emit silence and never imply normal EOF.
pub struct PcmSource {
    ring: Arc<Ring>,
    duration: Option<Duration>,
    frame: [f32; MAX_CHANNELS],
    frame_epoch: u64,
    frame_stamp: ProcessingStamp,
    completed_stamp: Option<(ProcessingStamp, u64)>,
    deferred_clock: bool,
    channel: usize,
    content: bool,
}

impl Drop for PcmSource {
    fn drop(&mut self) {
        self.ring.consumer_alive.store(false, Ordering::Release);
    }
}

impl PcmSource {
    pub fn control(&self) -> PcmControl {
        PcmControl {
            ring: self.ring.clone(),
        }
    }
    pub fn format(&self) -> PcmFormat {
        self.ring.format
    }

    pub fn set_deferred_clock(&mut self, deferred: bool) {
        self.deferred_clock = deferred;
    }

    /// Metadata for the most recently completed valid content frame. A partial
    /// frame, underflow or a changed/blocked epoch cannot masquerade as content.
    pub fn current_source_stamp(&self) -> Option<(ProcessingStamp, u64)> {
        if self.channel != 0 {
            return None;
        }
        let (stamp, epoch) = self.completed_stamp?;
        (self.ring.render_state() == (epoch, false)).then_some((stamp, epoch))
    }

    pub fn last_frame_stamp(&self) -> Option<ProcessingStamp> {
        self.current_source_stamp().map(|(stamp, _)| stamp)
    }

    pub fn current_epoch(&self) -> u64 {
        self.ring.epoch.load(Ordering::Acquire)
    }

    pub fn presentation_baseline(&self) -> Option<(u64, u32, u64)> {
        self.control().presentation_baseline()
    }
    fn pop_frame(&mut self) -> bool {
        let head = self.ring.head.load(Ordering::Acquire);
        if head >= self.ring.tail.load(Ordering::Acquire) {
            return false;
        }
        let slot = &self.ring.slots[(head % self.ring.slots.len() as u64) as usize];
        let sequence = head.wrapping_mul(2).wrapping_add(2);
        if slot.sequence.load(Ordering::SeqCst) != sequence {
            return false;
        }
        let epoch = slot.epoch.load(Ordering::SeqCst);
        let stamp = slot.stamp.load();
        for (target, value) in self.frame.iter_mut().zip(&slot.samples) {
            *target = f32::from_bits(value.load(Ordering::SeqCst));
        }
        if slot.sequence.load(Ordering::SeqCst) != sequence
            || epoch != self.ring.epoch.load(Ordering::Acquire)
        {
            return false;
        }
        if self
            .ring
            .head
            .compare_exchange(head, head + 1, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return false;
        }
        self.frame_epoch = epoch;
        self.frame_stamp = stamp;
        true
    }
}

impl Iterator for PcmSource {
    type Item = f32;
    fn next(&mut self) -> Option<f32> {
        let channels = self.ring.format.channels as usize;
        let blocked = self.ring.paused.load(Ordering::Acquire)
            || self.ring.seeking.load(Ordering::Acquire)
            || matches!(
                self.ring.terminal.load(Ordering::Acquire),
                ERROR | CANCELLED
            );
        if self.channel == 0 {
            self.completed_stamp = None;
            self.content = !blocked && self.pop_frame();
            if !self.content
                && !blocked
                && self.ring.terminal.load(Ordering::Acquire) == EOF
                && self.ring.head.load(Ordering::Acquire) >= self.ring.tail.load(Ordering::Acquire)
                && !self.ring.seeking.load(Ordering::Acquire)
                && self.ring.terminal.load(Ordering::Acquire) == EOF
            {
                self.ring.consumer_ended.store(true, Ordering::Release);
                return None;
            }
            if !self.content && !blocked {
                self.ring.underflows.fetch_add(1, Ordering::Relaxed);
            }
        }
        let content =
            self.content && !blocked && self.frame_epoch == self.ring.epoch.load(Ordering::Acquire);
        let sample = if content {
            self.frame[self.channel]
        } else {
            0.0
        };
        self.channel += 1;
        if self.channel == channels {
            self.channel = 0;
            if content {
                // Only the consumer writes this pair. Seek publishes a separate
                // baseline, so an old frame cannot overwrite the new position.
                self.completed_stamp = Some((self.frame_stamp, self.frame_epoch));
                if !self.deferred_clock {
                    self.ring
                        .commit_rendered(self.frame_stamp, self.frame_epoch);
                }
                self.ring.consumed.fetch_add(1, Ordering::Relaxed);
            }
        }
        Some(sample)
    }
}

impl Source for PcmSource {
    fn current_span_len(&self) -> Option<usize> {
        None
    }
    fn channels(&self) -> ChannelCount {
        ChannelCount::new(self.ring.format.channels).expect("validated channel count")
    }
    fn sample_rate(&self) -> SampleRate {
        SampleRate::new(self.ring.format.sample_rate).expect("validated sample rate")
    }
    fn total_duration(&self) -> Option<Duration> {
        self.duration
    }
}

#[cfg(test)]
mod presentation_tests {
    use super::*;

    #[test]
    fn post_publication_pause_cannot_revoke_an_accepted_presented_frame() {
        let (mut producer, mut source, control) = PcmProducer::new(
            PcmFormat {
                sample_rate: 1000,
                channels: 1,
            },
            2,
            0,
            None,
        )
        .unwrap();
        source.set_deferred_clock(true);
        let presented = ProcessingStamp {
            end_frame: 1,
            fraction: 1 << 31,
            processing_sequence: 7,
            settings: PlaybackProcessingSettings {
                playback_rate: 0.5,
                preserves_pitch: true,
                pitch_ratio: 1.0,
            },
            latency_frames: 31,
        };
        assert!(producer.push_processed_frame(presented, &[0.25]));
        assert_eq!(source.next(), Some(0.25));
        let (stamp, epoch) = source.current_source_stamp().unwrap();
        control
            .ring
            .pause_after_publication
            .store(true, Ordering::Relaxed);
        assert!(control.commit_rendered_stamp(stamp, epoch));
        assert!(control.paused());
        let accepted = control.snapshot();
        assert_eq!(accepted.position_seconds, 0.0015);
        assert_eq!(accepted.processing_sequence, 7);
        assert_eq!(accepted.processing_settings, presented.settings);
        assert_eq!(accepted.processing_latency_frames, 31);
        assert_eq!(accepted.processing_sample_rate, Some(1000));
        let following = ProcessingStamp {
            end_frame: 2,
            ..presented
        };
        assert!(!control.commit_rendered_stamp(following, epoch));
        let rejected = control.snapshot();
        assert_eq!(rejected.position_seconds, accepted.position_seconds);
        assert_eq!(rejected.processing_sequence, accepted.processing_sequence);
        assert_eq!(rejected.processing_settings, accepted.processing_settings);
        producer.begin_epoch().unwrap();
        producer.commit_epoch(500).unwrap();
        control.set_paused(false);
        assert!(!control.commit_rendered_stamp(stamp, epoch));
        assert_eq!(control.snapshot().position_seconds, 0.5);
    }
}

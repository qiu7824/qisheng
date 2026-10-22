//! Bounded post-pitch effect processing and final presentation ownership.
//!
//! An explicitly installed enhanced graph uses a stereo or native 3–8 channel
//! bus before device folding. A graph owns one immutable convolution/gain bank.
//! Partition alignment remains selected through deactivation until epoch reset.
//! Input gaps freeze every DSP state. Original media labels survive each delay;
//! wet tails carry stream identity without advancing the original media clock.

use super::{
    compressor::{CompressorError, CompressorFrame, CompressorStamp, PreparedCompressor},
    convolution::{CONVOLUTION_PARTITION, ConvolutionStamp, PreparedConvolution},
    effect_mix::{MixError, MixedEffectFrame, PreparedEffectMix},
    pcm::{MAX_CHANNELS, MAX_CLOCK_FRAMES, PcmFormat},
    processing::DspReservation,
    spatial::{SpatialRenderer, SpatialTarget},
};
use anyhow::{Context, Result, ensure};
use std::{
    mem::size_of,
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
};

pub const EFFECTS_MIN_REPLAY_FRAMES: usize = CONVOLUTION_PARTITION + 1023 + 1;
pub const EFFECTS_MAX_REPLAY_FRAMES: usize = EFFECTS_MIN_REPLAY_FRAMES * 48 + 1;
static NEXT_OWNER: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EffectsAlignment {
    Direct,
    Partition512,
}

#[derive(Clone, Copy, Debug)]
pub struct EffectsPreparation {
    pub format: PcmFormat,
    pub device_channels: u16,
    pub main_gain: f64,
    pub send_gain: f64,
    pub alignment: EffectsAlignment,
    pub sequence: u64,
    pub token: u64,
    pub source_epoch: u64,
    pub initial_spatial: SpatialTarget,
    pub replay_capacity: usize,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct EffectsPitch {
    pub ratio: f64,
    pub sequence: u64,
    pub transition_remaining_frames: u32,
}
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct EffectsInput {
    pub samples: [f64; MAX_CHANNELS],
    pub stamp: ConvolutionStamp,
    pub begin: Option<ConvolutionStamp>,
    pub pitch: EffectsPitch,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EffectsFrameKind {
    Padding,
    Media,
    Tail,
}
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct EffectsFrame {
    pub samples: [f64; MAX_CHANNELS],
    pub stamp: Option<ConvolutionStamp>,
    pub begin: Option<ConvolutionStamp>,
    pub token: u64,
    pub source_epoch: u64,
    pub kind: EffectsFrameKind,
    pub pitch: EffectsPitch,
    pub effect_sequence: u64,
    pub spatial_sequence: u64,
    pub transition_remaining_frames: u32,
    owner: u64,
    ordinal: u64,
    serial: Option<u64>,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EffectsError {
    Busy,
    EndOfInput,
    EndNotDeclared,
    InvalidStream,
    InvalidStamp,
    BackwardMedia,
    NonFiniteInput,
    InputOverflow,
    ReplayFull,
    InvalidReceipt,
    CounterExhausted,
    Invariant,
    Mix(MixError),
    Compressor(CompressorError),
}
impl std::fmt::Display for EffectsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Invalid effect operation: {self:?}")
    }
}
impl std::error::Error for EffectsError {}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct EffectsStatistics {
    pub accepted_media_frames: u64,
    pub presented_media_frames: u64,
    pub presented_tail_frames: u64,
    pub cast_sanitized_samples: u64,
    pub spatial_sanitized_samples: u64,
}
#[derive(Clone, Copy)]
struct OwnedInput {
    serial: u64,
    input: EffectsInput,
}
#[derive(Clone, Copy)]
struct Label {
    kind: EffectsFrameKind,
    serial: Option<u64>,
    stamp: Option<ConvolutionStamp>,
    begin: Option<ConvolutionStamp>,
    pitch: EffectsPitch,
}
impl Label {
    const PADDING: Self = Self {
        kind: EffectsFrameKind::Padding,
        serial: None,
        stamp: None,
        begin: None,
        pitch: EffectsPitch {
            ratio: 1.0,
            sequence: 0,
            transition_remaining_frames: 0,
        },
    };
    fn media(value: OwnedInput) -> Self {
        Self {
            kind: EffectsFrameKind::Media,
            serial: Some(value.serial),
            stamp: Some(value.input.stamp),
            begin: value.input.begin,
            pitch: value.input.pitch,
        }
    }
}
#[derive(Clone, Copy, Eq, PartialEq)]
enum Phase {
    Input,
    MixDrain,
    CompressorDrain,
    Finished,
    Invalidated,
}

/// Prepared and destroyed by the output owner, outside the callback.
pub struct PreparedEffects {
    preparation: EffectsPreparation,
    owner: u64,
    mix: Option<PreparedEffectMix>,
    compressor: PreparedCompressor,
    spatial: SpatialRenderer,
    dry_labels: Box<[Option<OwnedInput>]>,
    dry_head: usize,
    dry_drain_remaining: usize,
    compressor_labels: Box<[Label]>,
    label_read: usize,
    label_write: usize,
    replay: Box<[Option<OwnedInput>]>,
    replay_head: usize,
    replay_len: usize,
    pending: Option<EffectsFrame>,
    phase: Phase,
    last_stamp: Option<ConvolutionStamp>,
    last_pitch: EffectsPitch,
    next_serial: u64,
    next_ordinal: u64,
    impulse_frames: usize,
    tail_remaining: u64,
    statistics: EffectsStatistics,
    owned_bytes: usize,
    _reservation: DspReservation,
}
impl PreparedEffects {
    pub fn prepare(
        preparation: EffectsPreparation,
        bank: Option<PreparedConvolution>,
        cancel: &AtomicBool,
    ) -> Result<Self> {
        ensure!(
            !cancel.load(Ordering::Acquire),
            "Effect preparation cancelled"
        );
        let format = preparation.format;
        ensure!(
            (8000..=384000).contains(&format.sample_rate)
                && (2..=MAX_CHANNELS as u16).contains(&format.channels)
                && (1..=MAX_CHANNELS as u16).contains(&preparation.device_channels),
            "Unsupported effect bus/device format"
        );
        ensure!(
            preparation.token != 0 && preparation.source_epoch != 0,
            "Invalid effect stream identity"
        );
        ensure!(
            preparation.main_gain.is_finite()
                && preparation.send_gain.is_finite()
                && (0.0..=5.0).contains(&preparation.main_gain)
                && (0.0..=5.0).contains(&preparation.send_gain),
            "Invalid effect gains"
        );
        ensure!(
            preparation.initial_spatial.sample_rate() == format.sample_rate,
            "Spatial target and effect bus rates differ"
        );
        ensure!(
            (EFFECTS_MIN_REPLAY_FRAMES..=EFFECTS_MAX_REPLAY_FRAMES)
                .contains(&preparation.replay_capacity),
            "Invalid effect replay capacity"
        );
        if let Some(bank) = &bank {
            ensure!(
                bank.format() == format && preparation.alignment == EffectsAlignment::Partition512,
                "Convolution requires its exact bus format and partition alignment"
            );
        }
        let bytes = size_of::<Self>()
            .checked_add(CONVOLUTION_PARTITION * size_of::<Option<OwnedInput>>())
            .and_then(|bytes| bytes.checked_add(1024 * size_of::<Label>()))
            .and_then(|bytes| {
                bytes.checked_add(preparation.replay_capacity * size_of::<Option<OwnedInput>>())
            })
            .context("Effect storage overflow")?;
        let reservation = DspReservation::new(bytes)?;
        let dry_labels = fixed(CONVOLUTION_PARTITION, None)?;
        let compressor_labels = fixed(1024, Label::PADDING)?;
        let replay = fixed(preparation.replay_capacity, None)?;
        let impulse_frames = bank.as_ref().map_or(0, PreparedConvolution::impulse_frames);
        let mut mix = bank
            .map(|bank| {
                PreparedEffectMix::prepare(
                    format,
                    bank,
                    preparation.main_gain,
                    preparation.send_gain,
                )
            })
            .transpose()?;
        if let Some(mix) = &mut mix {
            mix.reset(preparation.token, preparation.source_epoch)?;
        }
        let mut compressor = PreparedCompressor::prepare(format.sample_rate, format.channels)?;
        compressor.reset(preparation.token, preparation.source_epoch)?;
        let mut spatial = SpatialRenderer::new(
            format.sample_rate,
            format.channels as usize,
            preparation.device_channels as usize,
        )?;
        ensure!(
            spatial.retarget(preparation.initial_spatial),
            "Spatial preparation rate mismatch"
        );
        spatial.reset_to_target();
        let owner = NEXT_OWNER
            .try_update(Ordering::AcqRel, Ordering::Acquire, |value| {
                value.checked_add(1)
            })
            .map_err(|_| anyhow::anyhow!("Effect owner identity exhausted"))?;
        let owned_bytes = bytes
            + compressor.reserved_bytes()
            + spatial.owned_bytes()
            + mix.as_ref().map_or(0, PreparedEffectMix::reserved_bytes);
        ensure!(
            !cancel.load(Ordering::Acquire),
            "Effect preparation cancelled"
        );
        let label_write = compressor.latency_frames() as usize;
        Ok(Self {
            preparation,
            owner,
            mix,
            compressor,
            spatial,
            dry_labels,
            dry_head: 0,
            dry_drain_remaining: 0,
            compressor_labels,
            label_read: 0,
            label_write,
            replay,
            replay_head: 0,
            replay_len: 0,
            pending: None,
            phase: Phase::Input,
            last_stamp: None,
            last_pitch: Label::PADDING.pitch,
            next_serial: 1,
            next_ordinal: 1,
            impulse_frames,
            tail_remaining: 0,
            statistics: EffectsStatistics::default(),
            owned_bytes,
            _reservation: reservation,
        })
    }
    /// Capacity for retaining old unpresented intervals before a rate handoff.
    pub fn rate_replay_capacity(frames: usize, old_rate: u32, new_rate: u32) -> Result<usize> {
        ensure!(
            (8000..=384000).contains(&old_rate) && (8000..=384000).contains(&new_rate),
            "Invalid replay rate"
        );
        let count = frames
            .checked_mul(new_rate as usize)
            .context("Rate replay overflow")?
            .div_ceil(old_rate as usize)
            .checked_add(1)
            .context("Rate replay overflow")?
            .max(EFFECTS_MIN_REPLAY_FRAMES);
        ensure!(
            count <= EFFECTS_MAX_REPLAY_FRAMES,
            "Rate replay frame quota exceeded"
        );
        Ok(count)
    }
    pub fn owned_bytes(&self) -> usize {
        self.owned_bytes
    }
    pub fn preparation(&self) -> EffectsPreparation {
        self.preparation
    }
    pub fn effect_channels(&self) -> u16 {
        self.preparation.format.channels
    }
    pub fn device_channels(&self) -> u16 {
        self.preparation.device_channels
    }
    pub fn sequence(&self) -> u64 {
        self.preparation.sequence
    }
    pub(crate) fn handoff_owner(&self) -> u64 {
        self.owner
    }
    pub(crate) fn handoff_impulse_frames(&self) -> usize {
        self.impulse_frames
    }
    pub(crate) fn handoff_pending_key(&self, frame: &EffectsFrame) -> Option<(u64, Option<u64>)> {
        self.pending
            .filter(|pending| pending == frame)
            .map(|pending| (pending.ordinal, pending.serial))
    }
    pub(crate) fn transfer_pending_for_handoff(
        &mut self,
        receipt: &mut super::effects_handoff::StoredReceipt,
    ) -> std::result::Result<(), EffectsError> {
        let pending = self.pending.ok_or(EffectsError::InvalidReceipt)?;
        let source = if pending.kind == EffectsFrameKind::Media {
            Some(self.replay[self.replay_head].ok_or(EffectsError::Invariant)?)
        } else {
            None
        };
        if !receipt.matches(
            self.owner,
            pending.ordinal,
            pending.serial,
            self.preparation,
            &pending,
            source.map(|source| source.input),
        ) {
            return Err(EffectsError::InvalidReceipt);
        }
        if let Some(source) = source {
            if Some(source.serial) != pending.serial {
                return Err(EffectsError::Invariant);
            }
            self.replay[self.replay_head] = None;
            self.replay_head = (self.replay_head + 1) % self.replay.len();
            self.replay_len -= 1;
        } else if pending.kind == EffectsFrameKind::Tail {
            self.tail_remaining = self
                .tail_remaining
                .checked_sub(1)
                .ok_or(EffectsError::Invariant)?;
        } else {
            return Err(EffectsError::InvalidReceipt);
        }
        receipt.consume();
        self.pending = None;
        Ok(())
    }
    pub fn latency_frames(&self) -> u32 {
        self.mix_latency() as u32 + self.compressor.latency_frames()
    }
    pub fn remaining_tail_frames(&self) -> u64 {
        self.tail_remaining
    }
    pub fn reduction_db(&self) -> f32 {
        self.compressor.reduction_db()
    }
    pub fn statistics(&self) -> EffectsStatistics {
        self.statistics
    }
    pub fn compressor_statistics(&self) -> super::compressor::CompressorStatistics {
        self.compressor.statistics()
    }
    pub fn pending_output(&self) -> Option<EffectsFrame> {
        self.pending
    }
    pub fn pending_media_frames(&self) -> usize {
        self.replay_len
    }
    pub fn replay_capacity(&self) -> usize {
        self.replay.len()
    }
    pub fn replay_inputs(&self) -> impl Iterator<Item = EffectsInput> + '_ {
        (0..self.replay_len).map(|index| {
            self.replay[(self.replay_head + index) % self.replay.len()]
                .expect("Admitted effect replay interval")
                .input
        })
    }
    pub fn can_accept_input(&self) -> bool {
        self.phase == Phase::Input && self.pending.is_none() && self.replay_len < self.replay.len()
    }
    pub fn retarget_spatial(&mut self, target: SpatialTarget) -> bool {
        self.spatial.retarget(target)
    }
    /// A gap does not advance a delay, detector, interpolation, counter, or label.
    /// An existing pending frame stays held even when a gap returns padding;
    /// the output owner checks `pending_output` before pulling another source.
    pub fn frame(
        &mut self,
        input: Option<EffectsInput>,
    ) -> std::result::Result<EffectsFrame, EffectsError> {
        let Some(input) = input else {
            return Ok(self.padding());
        };
        self.preflight(Some(&input))?;
        let owned = OwnedInput {
            serial: self.next_serial,
            input,
        };
        let (mixed, label) = if let Some(mix) = &mut self.mix {
            let mixed = mix
                .frame(
                    &input.samples[..self.preparation.format.channels as usize],
                    input.stamp,
                )
                .map_err(EffectsError::Mix)?;
            let delayed = self.dry_labels[self.dry_head];
            self.dry_labels[self.dry_head] = Some(owned);
            self.dry_head = (self.dry_head + 1) % CONVOLUTION_PARTITION;
            (mixed, self.mix_label(mixed, delayed)?)
        } else if self.preparation.alignment == EffectsAlignment::Partition512 {
            let delayed = self.dry_labels[self.dry_head];
            self.dry_labels[self.dry_head] = Some(owned);
            self.dry_head = (self.dry_head + 1) % CONVOLUTION_PARTITION;
            self.dry_mixed(delayed)
        } else {
            self.dry_mixed(Some(owned))
        };
        let at = (self.replay_head + self.replay_len) % self.replay.len();
        self.replay[at] = Some(owned);
        self.replay_len += 1;
        self.next_serial += 1;
        self.statistics.accepted_media_frames += 1;
        self.last_stamp = Some(input.stamp);
        self.last_pitch = input.pitch;
        self.process_mixed(mixed, label)
    }
    pub fn begin_eof(&mut self) -> std::result::Result<(), EffectsError> {
        if self.phase == Phase::Invalidated {
            return Err(EffectsError::InvalidStream);
        }
        if self.phase != Phase::Input {
            return Ok(());
        }
        self.tail_remaining = if self.statistics.accepted_media_frames == 0 {
            0
        } else {
            self.impulse_frames.saturating_sub(1) as u64
        };
        if let Some(mix) = &mut self.mix {
            mix.begin_eof();
        }
        self.dry_drain_remaining = if self.statistics.accepted_media_frames == 0 {
            0
        } else {
            self.mix_latency()
        };
        self.phase = Phase::MixDrain;
        Ok(())
    }
    pub fn drain(&mut self) -> std::result::Result<Option<EffectsFrame>, EffectsError> {
        self.preflight(None)?;
        if self.phase == Phase::Input {
            return Err(EffectsError::EndNotDeclared);
        }
        if self.phase == Phase::Finished {
            return Ok(None);
        }
        if self.phase == Phase::MixDrain {
            if let Some(mix) = &mut self.mix {
                if let Some(mixed) = mix.drain().map_err(EffectsError::Mix)? {
                    let delayed = self.dry_labels[self.dry_head].take();
                    self.dry_head = (self.dry_head + 1) % CONVOLUTION_PARTITION;
                    let label = self.mix_label(mixed, delayed)?;
                    return self.process_mixed(mixed, label).map(Some);
                }
            } else if self.dry_drain_remaining != 0 {
                self.dry_drain_remaining -= 1;
                let delayed = self.dry_labels[self.dry_head].take();
                self.dry_head = (self.dry_head + 1) % CONVOLUTION_PARTITION;
                let (mixed, label) = self.dry_mixed(delayed);
                return self.process_mixed(mixed, label).map(Some);
            }
            self.compressor.begin_eof();
            self.phase = Phase::CompressorDrain;
        }
        if let Some(frame) = self.compressor.drain().map_err(EffectsError::Compressor)? {
            let label = self.delay_label(Label::PADDING);
            return self.finish(frame, label).map(Some);
        }
        self.phase = Phase::Finished;
        Ok(None)
    }
    pub fn drained(&self) -> bool {
        self.phase == Phase::Finished
            && self.pending.is_none()
            && self.replay_len == 0
            && self.tail_remaining == 0
    }
    /// `presented` is supplied only after the final output owner commits/writes.
    /// A false acknowledgement preserves the exact already-generated frame.
    pub fn release_pending(
        &mut self,
        frame: &EffectsFrame,
        presented: bool,
    ) -> std::result::Result<(), EffectsError> {
        let pending = self.pending.ok_or(EffectsError::InvalidReceipt)?;
        if frame.owner != self.owner
            || frame.ordinal != pending.ordinal
            || frame.token != pending.token
            || frame.source_epoch != pending.source_epoch
        {
            return Err(EffectsError::InvalidReceipt);
        }
        if !presented {
            return Ok(());
        }
        if let Some(serial) = pending.serial {
            let first = self.replay[self.replay_head].ok_or(EffectsError::Invariant)?;
            if first.serial != serial {
                return Err(EffectsError::Invariant);
            }
            self.replay[self.replay_head] = None;
            self.replay_head = (self.replay_head + 1) % self.replay.len();
            self.replay_len -= 1;
            self.statistics.presented_media_frames += 1;
        } else if pending.kind == EffectsFrameKind::Tail {
            self.tail_remaining = self
                .tail_remaining
                .checked_sub(1)
                .ok_or(EffectsError::Invariant)?;
            self.statistics.presented_tail_frames += 1;
        }
        self.pending = None;
        Ok(())
    }
    /// Hard invalidation rejects every old pending frame and preserves ownership.
    pub fn invalidate(&mut self) {
        let _ = self.reset(self.preparation.token, self.preparation.source_epoch);
        self.phase = Phase::Invalidated;
    }
    pub fn reset(
        &mut self,
        token: u64,
        source_epoch: u64,
    ) -> std::result::Result<(), EffectsError> {
        if token == 0 || source_epoch == 0 {
            return Err(EffectsError::InvalidStream);
        }
        if let Some(mix) = &mut self.mix {
            mix.reset(token, source_epoch).map_err(EffectsError::Mix)?;
        }
        self.compressor
            .reset(token, source_epoch)
            .map_err(EffectsError::Compressor)?;
        self.spatial.reset_to_target();
        self.dry_labels.fill(None);
        self.compressor_labels.fill(Label::PADDING);
        self.replay.fill(None);
        self.dry_head = 0;
        self.dry_drain_remaining = 0;
        self.label_read = 0;
        self.label_write = self.compressor.latency_frames() as usize;
        self.replay_head = 0;
        self.replay_len = 0;
        self.pending = None;
        self.phase = Phase::Input;
        self.last_stamp = None;
        self.last_pitch = Label::PADDING.pitch;
        self.next_serial = 1;
        self.tail_remaining = 0;
        self.statistics = EffectsStatistics::default();
        self.preparation.token = token;
        self.preparation.source_epoch = source_epoch;
        Ok(())
    }
    fn preflight(&self, input: Option<&EffectsInput>) -> std::result::Result<(), EffectsError> {
        if self.phase == Phase::Invalidated {
            return Err(EffectsError::InvalidStream);
        }
        if self.pending.is_some() {
            return Err(EffectsError::Busy);
        }
        if self.next_ordinal == u64::MAX {
            return Err(EffectsError::CounterExhausted);
        }
        let stats = self.compressor.statistics();
        if stats.accepted_frames == u64::MAX
            || stats.processed_frames == u64::MAX
            || stats.valid_output_frames == u64::MAX
        {
            return Err(EffectsError::CounterExhausted);
        }
        let Some(input) = input else {
            return Ok(());
        };
        if self.phase != Phase::Input {
            return Err(EffectsError::EndOfInput);
        }
        if self.replay_len == self.replay.len() {
            return Err(EffectsError::ReplayFull);
        }
        if self.next_serial == u64::MAX || self.statistics.accepted_media_frames == u64::MAX {
            return Err(EffectsError::CounterExhausted);
        }
        if input.stamp.token != self.preparation.token
            || input.stamp.source_epoch != self.preparation.source_epoch
        {
            return Err(EffectsError::InvalidStream);
        }
        if !valid_stamp(input.stamp)
            || !input.pitch.ratio.is_finite()
            || !(0.5..=1.5).contains(&input.pitch.ratio)
        {
            return Err(EffectsError::InvalidStamp);
        }
        if input.begin.is_some_and(|begin| {
            !valid_stamp(begin)
                || begin.token != input.stamp.token
                || begin.source_epoch != input.stamp.source_epoch
                || coordinate(begin) > coordinate(input.stamp)
        }) {
            return Err(EffectsError::InvalidStamp);
        }
        if self
            .last_stamp
            .is_some_and(|last| coordinate(input.stamp) < coordinate(last))
        {
            return Err(EffectsError::BackwardMedia);
        }
        let bound = f64::from(f32::MAX) / (2.0 * self.preparation.main_gain.max(1.0));
        for value in input
            .samples
            .iter()
            .take(self.preparation.format.channels as usize)
        {
            if !value.is_finite() {
                return Err(EffectsError::NonFiniteInput);
            }
            if value.abs() > bound {
                return Err(EffectsError::InputOverflow);
            }
        }
        Ok(())
    }
    fn mix_latency(&self) -> usize {
        if self.preparation.alignment == EffectsAlignment::Partition512 {
            CONVOLUTION_PARTITION
        } else {
            0
        }
    }
    fn dry_mixed(&self, delayed: Option<OwnedInput>) -> (MixedEffectFrame, Label) {
        match delayed {
            Some(value) => (
                MixedEffectFrame {
                    samples: value
                        .input
                        .samples
                        .map(|sample| sample * self.preparation.main_gain),
                    stamp: Some(value.input.stamp),
                    valid: true,
                    tail: false,
                },
                Label::media(value),
            ),
            None => (
                MixedEffectFrame {
                    samples: [0.0; MAX_CHANNELS],
                    stamp: None,
                    valid: false,
                    tail: false,
                },
                Label::PADDING,
            ),
        }
    }
    fn mix_label(
        &self,
        mixed: MixedEffectFrame,
        delayed: Option<OwnedInput>,
    ) -> std::result::Result<Label, EffectsError> {
        if !mixed.valid {
            return if delayed.is_none() {
                Ok(Label::PADDING)
            } else {
                Err(EffectsError::Invariant)
            };
        }
        if mixed.tail {
            if delayed.is_some() || mixed.stamp.is_some() {
                return Err(EffectsError::Invariant);
            }
            Ok(Label {
                kind: EffectsFrameKind::Tail,
                pitch: self.last_pitch,
                ..Label::PADDING
            })
        } else {
            let delayed = delayed.ok_or(EffectsError::Invariant)?;
            if mixed.stamp != Some(delayed.input.stamp) {
                return Err(EffectsError::Invariant);
            }
            Ok(Label::media(delayed))
        }
    }
    fn process_mixed(
        &mut self,
        mixed: MixedEffectFrame,
        label: Label,
    ) -> std::result::Result<EffectsFrame, EffectsError> {
        if !mixed.valid {
            return Ok(self.padding());
        }
        let mut samples = [0.0_f32; MAX_CHANNELS];
        for (output, input) in samples
            .iter_mut()
            .zip(mixed.samples)
            .take(self.preparation.format.channels as usize)
        {
            *output = if !input.is_finite() {
                self.statistics.cast_sanitized_samples =
                    self.statistics.cast_sanitized_samples.saturating_add(1);
                0.0
            } else if input.abs() > f64::from(f32::MAX) {
                self.statistics.cast_sanitized_samples =
                    self.statistics.cast_sanitized_samples.saturating_add(1);
                input.clamp(-f64::from(f32::MAX), f64::from(f32::MAX)) as f32
            } else {
                input as f32
            };
        }
        let stamp = mixed.stamp.map(|stamp| CompressorStamp {
            token: stamp.token,
            source_epoch: stamp.source_epoch,
            media: stamp.media,
        });
        let result = self
            .compressor
            .frame(&samples[..self.preparation.format.channels as usize], stamp)
            .map_err(EffectsError::Compressor)?;
        let delayed = self.delay_label(label);
        self.finish(result, delayed)
    }
    fn delay_label(&mut self, input: Label) -> Label {
        self.compressor_labels[self.label_write] = input;
        let output = self.compressor_labels[self.label_read];
        self.compressor_labels[self.label_read] = Label::PADDING;
        self.label_read = (self.label_read + 1) % 1024;
        self.label_write = (self.label_write + 1) % 1024;
        output
    }
    fn finish(
        &mut self,
        frame: CompressorFrame,
        label: Label,
    ) -> std::result::Result<EffectsFrame, EffectsError> {
        if !frame.valid {
            return if label.kind == EffectsFrameKind::Padding {
                Ok(self.padding())
            } else {
                Err(EffectsError::Invariant)
            };
        }
        if label.kind == EffectsFrameKind::Padding {
            return Err(EffectsError::Invariant);
        }
        let stamp = frame.stamp.map(|stamp| ConvolutionStamp {
            token: stamp.token,
            source_epoch: stamp.source_epoch,
            media: stamp.media,
        });
        if stamp != label.stamp {
            return Err(EffectsError::Invariant);
        }
        let (samples, errors) = self.spatial.frame(frame.samples.map(f64::from));
        self.statistics.spatial_sanitized_samples = self
            .statistics
            .spatial_sanitized_samples
            .saturating_add(errors);
        let target = self.spatial.target();
        let result = EffectsFrame {
            samples,
            stamp,
            begin: label.begin,
            token: self.preparation.token,
            source_epoch: self.preparation.source_epoch,
            kind: label.kind,
            pitch: label.pitch,
            effect_sequence: self.preparation.sequence,
            spatial_sequence: target.sequence(),
            transition_remaining_frames: self.spatial.transition_remaining_frames(),
            owner: self.owner,
            ordinal: self.next_ordinal,
            serial: label.serial,
        };
        self.next_ordinal += 1;
        self.pending = Some(result);
        Ok(result)
    }
    fn padding(&self) -> EffectsFrame {
        EffectsFrame {
            samples: [0.0; MAX_CHANNELS],
            stamp: None,
            begin: None,
            token: self.preparation.token,
            source_epoch: self.preparation.source_epoch,
            kind: EffectsFrameKind::Padding,
            pitch: Label::PADDING.pitch,
            effect_sequence: self.preparation.sequence,
            spatial_sequence: self.spatial.target().sequence(),
            transition_remaining_frames: self.spatial.transition_remaining_frames(),
            owner: self.owner,
            ordinal: 0,
            serial: None,
        }
    }
}
fn fixed<T: Clone>(count: usize, value: T) -> Result<Box<[T]>> {
    let mut values = Vec::new();
    values
        .try_reserve_exact(count)
        .context("Cannot reserve effect storage")?;
    ensure!(
        values.capacity() <= count,
        "Effect allocation exceeded its reservation"
    );
    values.resize(count, value);
    Ok(values.into_boxed_slice())
}
fn coordinate(stamp: ConvolutionStamp) -> (u64, u32) {
    (stamp.media.end_frame, stamp.media.fraction)
}
fn valid_stamp(stamp: ConvolutionStamp) -> bool {
    let media = stamp.media;
    stamp.token != 0
        && stamp.source_epoch != 0
        && (media.end_frame < MAX_CLOCK_FRAMES
            || media.end_frame == MAX_CLOCK_FRAMES && media.fraction == 0)
        && media.settings.playback_rate.is_finite()
        && (0.5..=2.0).contains(&media.settings.playback_rate)
        && media.settings.pitch_ratio.is_finite()
        && (0.5..=1.5).contains(&media.settings.pitch_ratio)
}

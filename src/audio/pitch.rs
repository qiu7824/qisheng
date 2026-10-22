//! Fixed-window peak-region pitch shifting, following the LX Phaze worklet.
//! FFT planning, allocation and rate retargeting belong to the output owner.

use super::pcm::{MAX_CHANNELS, PcmFormat, ProcessingStamp};
use super::processing::DspReservation;
use anyhow::{Context, Result, ensure};
use rustfft::{Fft, FftPlanner, num_complex::Complex};
use std::sync::Arc;

pub(crate) const PITCH_WINDOW: usize = 4096;
const HOP: usize = 128;
const BINS: usize = PITCH_WINDOW / 2 + 1;
const FFT_PLAN_RESERVATION: usize = 1024 * 1024;

#[derive(Clone, Copy)]
pub(crate) struct PitchFrame {
    pub samples: [f64; MAX_CHANNELS],
    pub stamp: Option<(ProcessingStamp, u64)>,
    pub pitch_ratio: f64,
    pub pitch_sequence: u64,
    pub transition_remaining_frames: u32,
}

#[derive(Clone, Copy)]
struct PendingFrame {
    dry: [f64; MAX_CHANNELS],
    stamp: Option<(ProcessingStamp, u64)>,
    begin: Option<(ProcessingStamp, u64)>,
}
impl PendingFrame {
    const EMPTY: Self = Self {
        dry: [0.0; MAX_CHANNELS],
        stamp: None,
        begin: None,
    };
}

#[derive(Clone, Copy)]
struct ReadyFrame {
    frame: PitchFrame,
    dry: [f64; MAX_CHANNELS],
    begin: Option<(ProcessingStamp, u64)>,
}
impl ReadyFrame {
    const EMPTY: Self = Self {
        frame: PitchFrame {
            samples: [0.0; MAX_CHANNELS],
            stamp: None,
            pitch_ratio: 1.0,
            pitch_sequence: 0,
            transition_remaining_frames: 0,
        },
        dry: [0.0; MAX_CHANNELS],
        begin: None,
    };
}

fn fixed<T: Clone>(length: usize, value: T) -> Result<Box<[T]>> {
    let mut values = Vec::new();
    values
        .try_reserve_exact(length)
        .context("reserve fixed pitch storage")?;
    values.resize(length, value);
    Ok(values.into_boxed_slice())
}

pub(crate) struct PitchProcessor {
    format: PcmFormat,
    forward: Arc<dyn Fft<f64>>,
    inverse: Arc<dyn Fft<f64>>,
    scratch: Box<[Complex<f64>]>,
    spectrum: Box<[Complex<f64>]>,
    shifted: Box<[Complex<f64>]>,
    magnitudes: Box<[f64]>,
    peaks: Box<[usize]>,
    window: Box<[f64]>,
    history: Box<[[f64; MAX_CHANNELS]]>,
    overlap: Box<[[f64; MAX_CHANNELS]]>,
    wet: [[f64; MAX_CHANNELS]; HOP],
    input_at: usize,
    hop_at: usize,
    time_cursor: usize,
    pending: Box<[PendingFrame]>,
    pending_head: usize,
    pending_len: usize,
    pending_content: usize,
    ready: Box<[ReadyFrame]>,
    ready_head: usize,
    ready_len: usize,
    ready_content: usize,
    last_input_stamp: Option<(ProcessingStamp, u64)>,
    deferred: Option<ReadyFrame>,
    last_emitted: Option<ReadyFrame>,
    requested_ratio: f64,
    requested_sequence: u64,
    generated_ratio: f64,
    generated_sequence: u64,
    ratio_changed: bool,
    spectral_transition: u32,
    delayed: bool,
    dry_weight: f64,
    dry_start: f64,
    dry_target: f64,
    dry_total: u32,
    dry_remaining: u32,
    eof: bool,
    eof_remaining: usize,
    // Release quota only after FFT plans and every owned buffer are destroyed.
    _reservation: DspReservation,
}

pub(crate) struct PreparedPitch(PitchProcessor);

impl PitchProcessor {
    pub(crate) fn new(format: PcmFormat) -> Result<Self> {
        Self::with_capacity(format, PITCH_WINDOW + HOP)
    }

    fn with_capacity(format: PcmFormat, capacity: usize) -> Result<Self> {
        ensure!(
            format.sample_rate > 0
                && format.sample_rate <= 384_000
                && format.channels > 0
                && format.channels as usize <= MAX_CHANNELS,
            "unsupported pitch processing format"
        );
        let bytes = capacity
            .checked_mul(size_of::<PendingFrame>() + size_of::<ReadyFrame>())
            .and_then(|bytes| {
                bytes.checked_add(2 * PITCH_WINDOW * size_of::<[f64; MAX_CHANNELS]>())
            })
            .and_then(|bytes| bytes.checked_add(8 * PITCH_WINDOW * size_of::<Complex<f64>>()))
            .and_then(|bytes| {
                bytes.checked_add(
                    PITCH_WINDOW * size_of::<f64>()
                        + BINS * (size_of::<f64>() + size_of::<usize>()),
                )
            })
            .and_then(|bytes| bytes.checked_add(FFT_PLAN_RESERVATION))
            .and_then(|bytes| bytes.checked_add(size_of::<Self>()))
            .context("pitch storage size overflow")?;
        let reservation = DspReservation::new(bytes)?;
        let mut planner = FftPlanner::<f64>::new();
        let forward = planner.plan_fft_forward(PITCH_WINDOW);
        let inverse = planner.plan_fft_inverse(PITCH_WINDOW);
        let scratch_len = forward
            .get_inplace_scratch_len()
            .max(inverse.get_inplace_scratch_len());
        ensure!(
            scratch_len <= 6 * PITCH_WINDOW,
            "pitch FFT scratch exceeds its reservation"
        );
        let mut window = fixed(PITCH_WINDOW, 0.0)?;
        for (index, value) in window.iter_mut().enumerate() {
            *value =
                0.8 * (1.0 - (std::f64::consts::TAU * index as f64 / PITCH_WINDOW as f64).cos());
        }
        Ok(Self {
            format,
            _reservation: reservation,
            forward,
            inverse,
            scratch: fixed(scratch_len, Complex::new(0.0, 0.0))?,
            spectrum: fixed(PITCH_WINDOW, Complex::new(0.0, 0.0))?,
            shifted: fixed(PITCH_WINDOW, Complex::new(0.0, 0.0))?,
            magnitudes: fixed(BINS, 0.0)?,
            peaks: fixed(BINS, 0)?,
            window,
            history: fixed(PITCH_WINDOW, [0.0; MAX_CHANNELS])?,
            overlap: fixed(PITCH_WINDOW, [0.0; MAX_CHANNELS])?,
            wet: [[0.0; MAX_CHANNELS]; HOP],
            input_at: 0,
            hop_at: 0,
            time_cursor: 0,
            pending: fixed(capacity, PendingFrame::EMPTY)?,
            pending_head: 0,
            pending_len: 0,
            pending_content: 0,
            ready: fixed(capacity, ReadyFrame::EMPTY)?,
            ready_head: 0,
            ready_len: 0,
            ready_content: 0,
            last_input_stamp: None,
            deferred: None,
            last_emitted: None,
            requested_ratio: 1.0,
            requested_sequence: 0,
            generated_ratio: 1.0,
            generated_sequence: 0,
            ratio_changed: false,
            spectral_transition: 0,
            delayed: false,
            dry_weight: 1.0,
            dry_start: 1.0,
            dry_target: 1.0,
            dry_total: 0,
            dry_remaining: 0,
            eof: false,
            eof_remaining: 0,
        })
    }

    pub(crate) fn set_factor(&mut self, ratio: f64, sequence: u64) -> Result<()> {
        ensure!(
            ratio.is_finite() && (0.5..=1.5).contains(&ratio),
            "invalid independent pitch ratio"
        );
        ensure!(
            sequence > self.requested_sequence
                || (sequence == self.requested_sequence && ratio == self.requested_ratio),
            "independent pitch sequence is stale"
        );
        if ratio != self.requested_ratio {
            self.ratio_changed = true;
            self.dry_start = self.dry_weight;
            self.dry_target = if ratio == 1.0 { 1.0 } else { 0.0 };
            self.dry_total = (self.format.sample_rate as u64).div_ceil(50) as u32;
            self.dry_remaining = self.dry_total;
        }
        self.requested_ratio = ratio;
        self.requested_sequence = sequence;
        if !self.delayed && ratio != 1.0 {
            self.delayed = true;
            self.dry_weight = 0.0;
            self.dry_start = 0.0;
            self.dry_target = 0.0;
            self.dry_remaining = 0;
        }
        Ok(())
    }

    pub(crate) fn reset(&mut self) {
        self.history.fill([0.0; MAX_CHANNELS]);
        self.overlap.fill([0.0; MAX_CHANNELS]);
        self.wet.fill([0.0; MAX_CHANNELS]);
        self.pending_head = 0;
        self.pending_len = 0;
        self.pending_content = 0;
        self.ready_head = 0;
        self.ready_len = 0;
        self.ready_content = 0;
        self.last_input_stamp = None;
        self.deferred = None;
        self.last_emitted = None;
        self.input_at = 0;
        self.hop_at = 0;
        self.time_cursor = 0;
        self.generated_ratio = self.requested_ratio;
        self.generated_sequence = self.requested_sequence;
        self.ratio_changed = false;
        self.spectral_transition = 0;
        self.delayed = self.requested_ratio != 1.0;
        self.dry_weight = if self.delayed { 0.0 } else { 1.0 };
        self.dry_start = self.dry_weight;
        self.dry_target = self.dry_weight;
        self.dry_remaining = 0;
        self.eof = false;
        self.eof_remaining = 0;
    }

    fn generate_hop(&mut self) {
        let ratio = self.requested_ratio;
        if self.ratio_changed {
            self.spectral_transition = PITCH_WINDOW as u32;
            self.ratio_changed = false;
        }
        for channel in 0..self.format.channels as usize {
            for index in 0..PITCH_WINDOW {
                self.spectrum[index] = Complex::new(
                    self.history[(self.input_at + index) % PITCH_WINDOW][channel]
                        * self.window[index],
                    0.0,
                );
            }
            self.forward
                .process_with_scratch(&mut self.spectrum, &mut self.scratch);
            for index in 0..BINS {
                self.magnitudes[index] = self.spectrum[index].norm_sqr();
            }
            let mut count = 0;
            let mut index = 2;
            while index < BINS - 2 {
                let value = self.magnitudes[index];
                if self.magnitudes[index - 1] < value
                    && self.magnitudes[index - 2] < value
                    && self.magnitudes[index + 1] < value
                    && self.magnitudes[index + 2] < value
                {
                    self.peaks[count] = index;
                    count += 1;
                    index += 2;
                } else {
                    index += 1;
                }
            }
            self.shifted.fill(Complex::new(0.0, 0.0));
            for peak in 0..count {
                let at = self.peaks[peak];
                let target = (at as f64 * ratio).round() as isize;
                if target >= BINS as isize {
                    break;
                }
                let start = if peak == 0 {
                    0
                } else {
                    at - (at - self.peaks[peak - 1]) / 2
                };
                let end = if peak + 1 == count {
                    BINS
                } else {
                    at + (self.peaks[peak + 1] - at).div_ceil(2)
                };
                let delta = target - at as isize;
                let angle = std::f64::consts::TAU * delta as f64 * self.time_cursor as f64
                    / PITCH_WINDOW as f64;
                let rotation = Complex::new(angle.cos(), angle.sin());
                for bin in start..end {
                    let shifted = bin as isize + delta;
                    if shifted < 0 {
                        continue;
                    }
                    if shifted >= BINS as isize {
                        break;
                    }
                    self.shifted[shifted as usize] += self.spectrum[bin] * rotation;
                }
            }
            self.shifted[0].im = 0.0;
            self.shifted[PITCH_WINDOW / 2].im = 0.0;
            for index in 1..PITCH_WINDOW / 2 {
                self.shifted[PITCH_WINDOW - index] = self.shifted[index].conj();
            }
            self.inverse
                .process_with_scratch(&mut self.shifted, &mut self.scratch);
            for index in 0..PITCH_WINDOW {
                self.overlap[index][channel] += self.shifted[index].re * self.window[index]
                    / (PITCH_WINDOW as f64 * (PITCH_WINDOW / HOP) as f64);
            }
            for index in 0..HOP {
                self.wet[index][channel] = self.overlap[index][channel];
            }
        }
        self.overlap.copy_within(HOP.., 0);
        self.overlap[PITCH_WINDOW - HOP..].fill([0.0; MAX_CHANNELS]);
        self.generated_ratio = ratio;
        self.generated_sequence = self.requested_sequence;
        self.time_cursor = (self.time_cursor + HOP) % PITCH_WINDOW;
    }

    fn step(&mut self, input: PendingFrame) -> ReadyFrame {
        if input.stamp.is_some() {
            self.last_input_stamp = input.stamp;
        }
        if !self.delayed {
            return ReadyFrame {
                dry: input.dry,
                begin: input.begin,
                frame: PitchFrame {
                    samples: input.dry,
                    stamp: input.stamp,
                    pitch_ratio: 1.0,
                    pitch_sequence: self.requested_sequence,
                    transition_remaining_frames: 0,
                },
            };
        }
        let dry = if self.pending_len >= PITCH_WINDOW {
            let value = self.pending[self.pending_head];
            self.pending_head = (self.pending_head + 1) % self.pending.len();
            self.pending_len -= 1;
            self.pending_content -= usize::from(value.stamp.is_some());
            value
        } else {
            PendingFrame::EMPTY
        };
        let at = (self.pending_head + self.pending_len) % self.pending.len();
        self.pending[at] = input;
        self.pending_len += 1;
        self.pending_content += usize::from(input.stamp.is_some());
        if self.dry_remaining != 0 {
            let progress = (self.dry_total - self.dry_remaining + 1) as f64 / self.dry_total as f64;
            self.dry_weight = self.dry_start + (self.dry_target - self.dry_start) * progress;
            self.dry_remaining -= 1;
        }
        let wet = self.wet[self.hop_at];
        let ratio = if self.dry_target == 1.0 {
            1.0
        } else {
            self.generated_ratio
        };
        let sequence = if self.dry_target == 1.0 {
            self.requested_sequence
        } else {
            self.generated_sequence
        };
        self.spectral_transition = self.spectral_transition.saturating_sub(1);
        let remaining = self.spectral_transition.max(self.dry_remaining);
        let mut samples = [0.0; MAX_CHANNELS];
        if dry.stamp.is_some() {
            for channel in 0..self.format.channels as usize {
                samples[channel] = if self.dry_weight == 1.0 {
                    dry.dry[channel]
                } else if self.dry_weight == 0.0 {
                    wet[channel]
                } else {
                    wet[channel] + (dry.dry[channel] - wet[channel]) * self.dry_weight
                };
            }
        }
        self.history[self.input_at] = input.dry;
        self.input_at = (self.input_at + 1) % PITCH_WINDOW;
        self.hop_at += 1;
        if self.hop_at == HOP {
            self.generate_hop();
            self.hop_at = 0;
        }
        ReadyFrame {
            dry: dry.dry,
            begin: dry.begin,
            frame: PitchFrame {
                samples,
                stamp: dry.stamp,
                pitch_ratio: ratio,
                pitch_sequence: sequence,
                transition_remaining_frames: remaining,
            },
        }
    }

    pub(crate) fn process(
        &mut self,
        input: [f64; MAX_CHANNELS],
        stamp: Option<(ProcessingStamp, u64)>,
    ) -> PitchFrame {
        let begin = match (self.last_input_stamp, stamp) {
            (Some((previous, previous_epoch)), Some((current, epoch)))
                if previous_epoch == epoch =>
            {
                Some((
                    ProcessingStamp {
                        end_frame: previous.end_frame,
                        fraction: previous.fraction,
                        ..current
                    },
                    epoch,
                ))
            }
            _ => None,
        };
        let generated = self.step(PendingFrame {
            dry: input,
            stamp,
            begin,
        });
        if let Some(emitted) = self.deferred.take() {
            self.push_ready(generated);
            return self.remember(emitted);
        }
        if self.ready_len == 0 {
            return self.remember(generated);
        }
        let emitted = self.ready[self.ready_head];
        self.ready_head = (self.ready_head + 1) % self.ready.len();
        self.ready_len -= 1;
        self.ready_content -= usize::from(emitted.frame.stamp.is_some());
        self.push_ready(generated);
        self.remember(emitted)
    }

    fn push_ready(&mut self, frame: ReadyFrame) {
        assert!(
            self.ready_len < self.ready.len(),
            "preflighted pitch replay capacity"
        );
        let at = (self.ready_head + self.ready_len) % self.ready.len();
        self.ready[at] = frame;
        self.ready_len += 1;
        self.ready_content += usize::from(frame.frame.stamp.is_some());
    }

    pub(crate) fn has_prepared_output(&self) -> bool {
        self.deferred.is_some() || self.ready_len != 0
    }
    pub(crate) fn capacity_bound(&self) -> usize {
        PITCH_WINDOW
            .saturating_add(self.ready_len)
            .saturating_add(1)
    }
    pub(crate) fn sample_rate(&self) -> u32 {
        self.format.sample_rate
    }
    pub(crate) fn channels(&self) -> u16 {
        self.format.channels
    }
    pub(crate) fn emitted_begin(&self) -> Option<(ProcessingStamp, u64)> {
        self.last_emitted.and_then(|frame| frame.begin)
    }
    pub(crate) fn select_channels(&mut self, channels: u16) -> bool {
        if channels == 0 || channels as usize > MAX_CHANNELS {
            return false;
        }
        if channels == self.format.channels {
            return true;
        }
        if self.has_pending_content() {
            return false;
        }
        self.reset();
        self.format.channels = channels;
        true
    }
    pub(crate) fn has_pending_content(&self) -> bool {
        self.pending_content != 0
            || self.ready_content != 0
            || self
                .deferred
                .is_some_and(|value| value.frame.stamp.is_some())
    }
    pub(crate) fn set_baseline(&mut self, stamp: Option<(ProcessingStamp, u64)>) {
        if self.last_input_stamp.is_none() {
            self.last_input_stamp = stamp;
        }
    }
    pub(crate) fn latency_frames(&self) -> u32 {
        if self.delayed {
            (PITCH_WINDOW + self.ready_len).min(u32::MAX as usize) as u32
        } else {
            0
        }
    }
    pub(crate) fn begin_eof(&mut self) {
        if !self.eof {
            self.eof = true;
            self.eof_remaining = if self.delayed { 2 * PITCH_WINDOW } else { 0 };
        }
    }
    pub(crate) fn drained(&self) -> bool {
        self.eof
            && self.eof_remaining == 0
            && self.ready_len == 0
            && self.pending_content == 0
            && self.deferred.is_none()
    }
    pub(crate) fn drain(&mut self) -> PitchFrame {
        if let Some(frame) = self.deferred.take() {
            return self.remember(frame);
        }
        if self.ready_len != 0 {
            let frame = self.ready[self.ready_head];
            self.ready_head = (self.ready_head + 1) % self.ready.len();
            self.ready_len -= 1;
            self.ready_content -= usize::from(frame.frame.stamp.is_some());
            return self.remember(frame);
        }
        if self.eof_remaining != 0 {
            self.eof_remaining -= 1;
            let frame = self.step(PendingFrame::EMPTY);
            return self.remember(frame);
        }
        self.last_emitted = None;
        PitchFrame {
            samples: [0.0; MAX_CHANNELS],
            stamp: None,
            pitch_ratio: self.requested_ratio,
            pitch_sequence: self.requested_sequence,
            transition_remaining_frames: 0,
        }
    }

    fn remember(&mut self, frame: ReadyFrame) -> PitchFrame {
        self.last_emitted = Some(frame);
        frame.frame
    }

    pub(crate) fn defer_last_output(&mut self) {
        if let Some(frame) = self.last_emitted.take()
            && frame.frame.stamp.is_some()
        {
            assert!(
                self.deferred.is_none(),
                "one in-flight pitch presentation frame"
            );
            self.deferred = Some(frame);
        }
    }

    #[cfg(test)]
    pub(crate) fn retarget(&mut self, format: PcmFormat) -> Result<()> {
        if format == self.format {
            return Ok(());
        }
        let count = self
            .ready_len
            .checked_add(usize::from(self.deferred.is_some()))
            .and_then(|count| count.checked_add(self.pending_len))
            .context("pitch replay length overflow")?;
        let prepared = Self::prepare_retarget(format, count as u64, self.format.sample_rate)?;
        self.retarget_with_prepared(prepared)
    }

    pub(crate) fn prepare_retarget(
        format: PcmFormat,
        pending_bound: u64,
        old_rate: u32,
    ) -> Result<PreparedPitch> {
        ensure!(old_rate > 0, "invalid pitch retarget source rate");
        let converted =
            (pending_bound as u128 * format.sample_rate as u128).div_ceil(old_rate as u128);
        let capacity = usize::try_from(converted)
            .context("pitch replay exceeds addressable storage")?
            .checked_add(PITCH_WINDOW + HOP)
            .context("pitch replay capacity overflow")?;
        Ok(PreparedPitch(Self::with_capacity(format, capacity)?))
    }

    pub(crate) fn retarget_with_prepared(&mut self, prepared: PreparedPitch) -> Result<()> {
        let mut next = prepared.0;
        let format = next.format;
        let count = self
            .ready_len
            .checked_add(usize::from(self.deferred.is_some()))
            .and_then(|count| count.checked_add(self.pending_len))
            .context("pitch replay length overflow")?;
        let converted =
            (count as u128 * format.sample_rate as u128).div_ceil(self.format.sample_rate as u128);
        let converted =
            usize::try_from(converted).context("pitch replay exceeds addressable storage")?;
        ensure!(
            next.pending.len() >= converted + PITCH_WINDOW,
            "prepared pitch replay capacity is insufficient"
        );
        next.requested_ratio = self.requested_ratio;
        next.requested_sequence = self.requested_sequence;
        next.generated_ratio = self.requested_ratio;
        next.generated_sequence = self.requested_sequence;
        next.delayed = self.delayed;
        next.dry_weight = if self.requested_ratio == 1.0 {
            1.0
        } else {
            0.0
        };
        next.dry_target = next.dry_weight;
        next.dry_start = next.dry_weight;
        for index in 0..converted {
            let position = index as u128 * self.format.sample_rate as u128;
            let left = (position / format.sample_rate as u128) as usize;
            let fraction =
                (position % format.sample_rate as u128) as f64 / format.sample_rate as f64;
            let first = self.pending_at(left.min(count.saturating_sub(1)));
            let second = self.pending_at((left + 1).min(count.saturating_sub(1)));
            let samples = map_channels(
                std::array::from_fn(|channel| {
                    first.dry[channel] + (second.dry[channel] - first.dry[channel]) * fraction
                }),
                self.format.channels as usize,
                format.channels as usize,
            );
            let end_position = ((index as u128 + 1) * self.format.sample_rate as u128)
                .min(count as u128 * format.sample_rate as u128);
            let interval = ((end_position.saturating_sub(1)) / format.sample_rate as u128) as usize;
            let interval_frame = self.pending_at(interval.min(count.saturating_sub(1)));
            let numerator = (end_position - interval as u128 * format.sample_rate as u128) as u64;
            let stamp = interpolate_stamp(
                interval_frame.begin,
                interval_frame.stamp,
                numerator,
                format.sample_rate as u64,
            );
            let begin = match (next.last_input_stamp, stamp) {
                (Some((previous, previous_epoch)), Some((current, epoch)))
                    if previous_epoch == epoch =>
                {
                    Some((
                        ProcessingStamp {
                            end_frame: previous.end_frame,
                            fraction: previous.fraction,
                            ..current
                        },
                        epoch,
                    ))
                }
                _ => interval_frame.begin,
            };
            let generated = next.step(PendingFrame {
                dry: samples,
                stamp,
                begin,
            });
            if generated.frame.stamp.is_some() {
                next.push_ready(generated);
            }
        }
        if self.eof {
            next.begin_eof();
        }
        *self = next;
        Ok(())
    }

    fn pending_at(&self, index: usize) -> PendingFrame {
        let index = if let Some(value) = self.deferred {
            if index == 0 {
                return PendingFrame {
                    dry: value.dry,
                    stamp: value.frame.stamp,
                    begin: value.begin,
                };
            }
            index - 1
        } else {
            index
        };
        if index < self.ready_len {
            let value = self.ready[(self.ready_head + index) % self.ready.len()];
            PendingFrame {
                dry: value.dry,
                stamp: value.frame.stamp,
                begin: value.begin,
            }
        } else {
            self.pending[(self.pending_head + index - self.ready_len) % self.pending.len()]
        }
    }
}

pub(crate) fn interpolate_stamp(
    first: Option<(ProcessingStamp, u64)>,
    second: Option<(ProcessingStamp, u64)>,
    numerator: u64,
    denominator: u64,
) -> Option<(ProcessingStamp, u64)> {
    let (mut stamp, epoch) = first?;
    let (next, next_epoch) = second?;
    if epoch != next_epoch || denominator == 0 {
        return None;
    }
    let begin = ((stamp.end_frame as u128) << 32) + stamp.fraction as u128;
    let end = ((next.end_frame as u128) << 32) + next.fraction as u128;
    if end < begin {
        return None;
    }
    let point = begin + (end - begin) * numerator as u128 / denominator as u128;
    stamp.end_frame = (point >> 32) as u64;
    stamp.fraction = point as u32;
    Some((stamp, epoch))
}

fn map_channels(input: [f64; MAX_CHANNELS], from: usize, to: usize) -> [f64; MAX_CHANNELS] {
    let mut output = [0.0; MAX_CHANNELS];
    if to == 1 {
        output[0] = input[..from].iter().sum::<f64>() / from as f64;
    } else if from == 1 {
        output[..to].fill(input[0]);
    } else if from <= to {
        output[..from].copy_from_slice(&input[..from]);
    } else {
        let mut counts = [0; MAX_CHANNELS];
        for (index, value) in input[..from].iter().enumerate() {
            output[index % to] += value;
            counts[index % to] += 1;
        }
        for (value, count) in output[..to].iter_mut().zip(counts) {
            *value /= count as f64;
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stamp(end: u64) -> Option<(ProcessingStamp, u64)> {
        Some((
            ProcessingStamp {
                end_frame: end,
                processing_sequence: 7,
                ..ProcessingStamp::default()
            },
            1,
        ))
    }

    fn frequency(samples: &[f64], rate: u32) -> f64 {
        let mut first = None;
        let mut last = 0.0;
        let mut count = 0;
        for (index, pair) in samples.windows(2).enumerate() {
            if pair[0] < 0.0 && pair[1] >= 0.0 {
                let point = index as f64 - pair[0] / (pair[1] - pair[0]);
                first.get_or_insert(point);
                last = point;
                count += 1;
            }
        }
        assert!(count > 2);
        rate as f64 * (count - 1) as f64 / (last - first.unwrap())
    }

    #[test]
    fn unity_bypass_preserves_all_eight_channels_and_has_no_startup_latency() {
        let mut pitch = PitchProcessor::new(PcmFormat {
            sample_rate: 48_000,
            channels: 8,
        })
        .unwrap();
        pitch.set_factor(1.0, 7).unwrap();
        pitch.set_baseline(stamp(0));
        for index in 0..257 {
            let input = std::array::from_fn(|channel| (index as f64 - channel as f64) / 1024.0);
            let output = pitch.process(input, stamp(index + 1));
            assert_eq!(output.samples, input);
            assert_eq!(output.stamp, stamp(index + 1));
            assert_eq!(output.pitch_sequence, 7);
        }
        assert_eq!(pitch.latency_frames(), 0);
        assert!(!pitch.has_pending_content());
        pitch.begin_eof();
        assert!(pitch.drained());
    }

    #[test]
    fn actual_phaze_fft_moves_tone_frequency_with_fixed_delay_and_known_window_gain() {
        for rate in [22_050, 48_000] {
            for ratio in [0.5, 0.75, 1.25, 1.5] {
                let mut pitch = PitchProcessor::new(PcmFormat {
                    sample_rate: rate,
                    channels: 1,
                })
                .unwrap();
                pitch.set_factor(ratio, 7).unwrap();
                pitch.set_baseline(stamp(0));
                let mut measured = Vec::new();
                for index in 0..4 * PITCH_WINDOW {
                    let mut input = [0.0; MAX_CHANNELS];
                    input[0] =
                        0.2 * (std::f64::consts::TAU * 997.0 * index as f64 / rate as f64).cos();
                    let output = pitch.process(input, stamp(index as u64 + 1));
                    if index < PITCH_WINDOW {
                        assert!(output.stamp.is_none());
                        assert_eq!(output.samples, [0.0; MAX_CHANNELS]);
                    } else {
                        assert_eq!(
                            output.stamp.unwrap().0.end_frame,
                            (index - PITCH_WINDOW + 1) as u64
                        );
                    }
                    if index >= 2 * PITCH_WINDOW {
                        measured.push(output.samples[0]);
                    }
                }
                let actual = frequency(&measured, rate);
                assert!(
                    (actual - 997.0 * ratio).abs() < rate as f64 / PITCH_WINDOW as f64 + 2.0,
                    "{rate} Hz ratio {ratio}: {actual} Hz"
                );
                let rms = (measured.iter().map(|value| value * value).sum::<f64>()
                    / measured.len() as f64)
                    .sqrt();
                let gain = rms / (0.2 / std::f64::consts::SQRT_2);
                assert!((gain - 0.96).abs() < 0.05, "Phaze window gain {gain}");
                assert_eq!(pitch.latency_frames(), PITCH_WINDOW as u32);
            }
        }
    }

    #[test]
    fn returning_to_unity_keeps_every_unplayed_frame_until_eof_and_reset_clears_delay() {
        let mut pitch = PitchProcessor::new(PcmFormat {
            sample_rate: 22_050,
            channels: 1,
        })
        .unwrap();
        pitch.set_factor(1.25, 7).unwrap();
        pitch.set_baseline(stamp(0));
        let mut previous_end = 0;
        for index in 0..PITCH_WINDOW + 256 {
            let input = [index as f64 / 100_000.0; MAX_CHANNELS];
            if let Some((stamp, _)) = pitch.process(input, stamp(index as u64 + 1)).stamp {
                assert_eq!(stamp.end_frame, previous_end + 1);
                previous_end = stamp.end_frame;
            }
        }
        pitch.set_factor(1.0, 8).unwrap();
        for index in PITCH_WINDOW + 256..2 * PITCH_WINDOW + 768 {
            let input = [index as f64 / 100_000.0; MAX_CHANNELS];
            let output = pitch.process(input, stamp(index as u64 + 1));
            let end = output.stamp.unwrap().0.end_frame;
            assert_eq!(end, previous_end + 1);
            previous_end = end;
            if output.transition_remaining_frames == 0 {
                assert_eq!(output.samples[0], (index - PITCH_WINDOW) as f64 / 100_000.0);
            }
        }
        assert_eq!(pitch.latency_frames(), PITCH_WINDOW as u32);
        let final_end = (2 * PITCH_WINDOW + 768) as u64;
        pitch.begin_eof();
        let mut drains = 0;
        while !pitch.drained() {
            let output = pitch.drain();
            if let Some((stamp, _)) = output.stamp {
                assert_eq!(stamp.end_frame, previous_end + 1);
                previous_end = stamp.end_frame;
            }
            drains += 1;
            assert!(drains <= 2 * PITCH_WINDOW);
        }
        assert_eq!(previous_end, final_end);
        pitch.reset();
        assert_eq!(pitch.latency_frames(), 0);
        assert!(!pitch.has_pending_content());
    }

    #[test]
    fn rate_retarget_resamples_and_emits_every_pending_original_interval() {
        let mut pitch = PitchProcessor::new(PcmFormat {
            sample_rate: 8_000,
            channels: 1,
        })
        .unwrap();
        pitch.set_factor(1.25, 7).unwrap();
        pitch.set_baseline(stamp(0));
        for index in 0..PITCH_WINDOW / 2 {
            assert!(
                pitch
                    .process(
                        [index as f64 / 100_000.0; MAX_CHANNELS],
                        stamp(index as u64 + 1)
                    )
                    .stamp
                    .is_none()
            );
        }
        pitch
            .retarget(PcmFormat {
                sample_rate: 48_000,
                channels: 2,
            })
            .unwrap();
        pitch.begin_eof();
        let mut previous = 0u128;
        let mut valid = 0;
        let mut calls = 0;
        while !pitch.drained() {
            let output = pitch.drain();
            assert_eq!(output.samples[0], output.samples[1]);
            if let Some((stamp, epoch)) = output.stamp {
                assert_eq!(epoch, 1);
                let position = ((stamp.end_frame as u128) << 32) + stamp.fraction as u128;
                assert!(position >= previous);
                previous = position;
                valid += 1;
            }
            calls += 1;
            assert!(calls <= 6 * PITCH_WINDOW + 2 * PITCH_WINDOW);
        }
        assert_eq!(valid, 6 * (PITCH_WINDOW / 2));
        assert_eq!(previous, ((PITCH_WINDOW / 2) as u128) << 32);
    }

    #[test]
    fn rejected_retarget_reservation_keeps_the_existing_pending_processor() {
        let mut pitch = PitchProcessor::new(PcmFormat {
            sample_rate: 8_000,
            channels: 1,
        })
        .unwrap();
        pitch.set_factor(1.25, 7).unwrap();
        pitch.set_baseline(stamp(0));
        for index in 0..512 {
            pitch.process([0.01; MAX_CHANNELS], stamp(index + 1));
        }
        assert!(
            PitchProcessor::prepare_retarget(
                PcmFormat {
                    sample_rate: 384_000,
                    channels: 8
                },
                pitch.capacity_bound() as u64,
                8_000
            )
            .is_err()
        );
        assert_eq!(pitch.sample_rate(), 8_000);
        assert!(pitch.has_pending_content());
        pitch.begin_eof();
        let mut last = 0;
        while !pitch.drained() {
            if let Some((stamp, _)) = pitch.drain().stamp {
                last = stamp.end_frame;
            }
        }
        assert_eq!(last, 512);
    }
}

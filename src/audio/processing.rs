//! Fixed-storage tempo processing on the decoder worker, before PCM publication.

use super::pcm::{MAX_CHANNELS, MAX_CLOCK_FRAMES, PcmFormat, ProcessingStamp};
use crate::domain::playback_processing::PlaybackProcessingSettings;
use anyhow::{Context, Result, ensure};
use std::mem::size_of;
use std::sync::atomic::{AtomicUsize, Ordering};

#[cfg(test)]
mod throughput_tests;

pub const DSP_QUOTA_BYTES: usize = 64 * 1024 * 1024;
static DSP_BYTES: AtomicUsize = AtomicUsize::new(0);
const FIR_TAPS: usize = 32;
const FIR_PHASES: usize = 256;
const FRACTION_SCALE: u128 = 1u128 << 32;

pub(crate) struct DspReservation {
    bytes: usize,
}
impl DspReservation {
    pub(crate) fn new(bytes: usize) -> Result<Self> {
        let mut used = DSP_BYTES.load(Ordering::Acquire);
        loop {
            let next = used
                .checked_add(bytes)
                .context("DSP reservation overflow")?;
            ensure!(next <= DSP_QUOTA_BYTES, "DSP owned-memory quota exhausted");
            match DSP_BYTES.compare_exchange_weak(used, next, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => return Ok(Self { bytes }),
                Err(value) => used = value,
            }
        }
    }
    pub(crate) fn shrink_to(&mut self, bytes: usize) -> Result<()> {
        ensure!(
            bytes <= self.bytes,
            "DSP allocation exceeds its reservation"
        );
        DSP_BYTES.fetch_sub(self.bytes - bytes, Ordering::AcqRel);
        self.bytes = bytes;
        Ok(())
    }
    #[cfg(test)]
    pub(crate) fn used_bytes() -> usize {
        DSP_BYTES.load(Ordering::Acquire)
    }
}
impl Drop for DspReservation {
    fn drop(&mut self) {
        DSP_BYTES.fetch_sub(self.bytes, Ordering::AcqRel);
    }
}

#[derive(Clone, Copy)]
pub(crate) struct ProcessedFrame {
    pub samples: [f32; MAX_CHANNELS],
    pub stamp: ProcessingStamp,
}
impl ProcessedFrame {
    fn empty() -> Self {
        Self {
            samples: [0.0; MAX_CHANNELS],
            stamp: ProcessingStamp {
                end_frame: 0,
                fraction: 0,
                processing_sequence: 0,
                settings: PlaybackProcessingSettings::default(),
                latency_frames: 0,
            },
        }
    }
}
#[derive(Clone, Copy, Default)]
struct InputFrame {
    original: u64,
    samples: [f32; MAX_CHANNELS],
}
#[derive(Clone, Copy)]
struct Plan {
    settings: PlaybackProcessingSettings,
    sequence: u64,
    rate: u64,
}
impl Plan {
    fn new(mut settings: PlaybackProcessingSettings, sequence: u64) -> Result<Self> {
        settings.validate()?;
        settings.pitch_ratio = 1.0;
        Ok(Self {
            rate: (settings.playback_rate * FRACTION_SCALE as f64).round() as u64,
            settings,
            sequence,
        })
    }
}

pub(crate) struct WorkerProcessor {
    format: PcmFormat,
    input: Box<[InputFrame]>,
    output: Box<[ProcessedFrame]>,
    overlap: Box<[[f64; MAX_CHANNELS]]>,
    window: Box<[f64]>,
    sinc: Box<[f64]>,
    hop: usize,
    search: usize,
    origin: u64,
    end: u64,
    cursor: u128,
    epoch_start: u64,
    original_end: Option<u64>,
    output_at: usize,
    output_len: usize,
    requested: Plan,
    active: Plan,
    accepted: bool,
    primed: bool,
    last_source: u64,
    finished: bool,
    input_error: Option<&'static str>,
    // Declared last: quota is released only after all owned storage is destroyed.
    _reservation: DspReservation,
}

fn fixed<T: Clone>(length: usize, value: T) -> Result<Box<[T]>> {
    let mut data = Vec::new();
    data.try_reserve_exact(length)
        .context("reserve fixed DSP storage")?;
    data.resize(length, value);
    Ok(data.into_boxed_slice())
}

impl WorkerProcessor {
    pub(crate) fn new(format: PcmFormat) -> Result<Self> {
        ensure!(
            format.sample_rate > 0
                && format.sample_rate <= 384_000
                && format.channels > 0
                && format.channels as usize <= MAX_CHANNELS,
            "unsupported tempo processing format"
        );
        // Music geometry covers low-bass periods; correlation work remains
        // independent of the number of samples in the analysis window.
        let hop = ((format.sample_rate as f64 * 0.032).round() as usize).max(4);
        let search = ((format.sample_rate as f64 * 0.032).round() as usize).max(1);
        let capacity = (4 * hop + 2 * search + 8).max(FIR_TAPS * 4);
        let bytes = capacity
            .checked_mul(size_of::<InputFrame>())
            .and_then(|bytes| bytes.checked_add(hop * size_of::<ProcessedFrame>()))
            .and_then(|bytes| {
                bytes.checked_add(
                    hop * size_of::<[f64; MAX_CHANNELS]>()
                        + (2 * hop + FIR_TAPS * FIR_PHASES) * size_of::<f64>(),
                )
            })
            .context("DSP capacity overflow")?;
        let reservation = DspReservation::new(bytes)?;
        let plan = Plan::new(PlaybackProcessingSettings::default(), 0)?;
        let mut processor = Self {
            format,
            input: fixed(capacity, InputFrame::default())?,
            output: fixed(hop, ProcessedFrame::empty())?,
            overlap: fixed(hop, [0.0; MAX_CHANNELS])?,
            window: fixed(2 * hop, 0.0)?,
            sinc: fixed(FIR_TAPS * FIR_PHASES, 0.0)?,
            hop,
            search,
            origin: 0,
            end: 0,
            cursor: 0,
            epoch_start: 0,
            original_end: None,
            output_at: 0,
            output_len: 0,
            requested: plan,
            active: plan,
            accepted: false,
            primed: false,
            last_source: 0,
            finished: false,
            input_error: None,
            _reservation: reservation,
        };
        for (index, weight) in processor.window.iter_mut().enumerate() {
            *weight = 0.5 - 0.5 * (std::f64::consts::TAU * index as f64 / (2 * hop) as f64).cos();
        }
        processor.prepare_sinc(1.0);
        // Commit the charge from the actual fixed allocations, rather than only
        // the pre-allocation capacity plan. Oversized storage cannot be admitted.
        let actual = processor
            .input
            .len()
            .checked_mul(size_of::<InputFrame>())
            .and_then(|bytes| {
                bytes.checked_add(processor.output.len() * size_of::<ProcessedFrame>())
            })
            .and_then(|bytes| {
                bytes.checked_add(processor.overlap.len() * size_of::<[f64; MAX_CHANNELS]>())
            })
            .and_then(|bytes| {
                bytes
                    .checked_add((processor.window.len() + processor.sinc.len()) * size_of::<f64>())
            })
            .context("actual DSP allocation overflow")?;
        processor._reservation.shrink_to(actual)?;
        Ok(processor)
    }

    pub(crate) fn set_settings(
        &mut self,
        settings: PlaybackProcessingSettings,
        sequence: u64,
    ) -> Result<()> {
        let plan = Plan::new(settings, sequence)?;
        ensure!(
            !self.accepted
                || sequence > self.requested.sequence
                || (sequence == self.requested.sequence
                    && plan.settings == self.requested.settings),
            "tempo processing sequence is stale"
        );
        self.requested = plan;
        self.accepted = true;
        Ok(())
    }

    pub(crate) fn push_input(&mut self, original_frame: u64, samples: &[f32]) -> bool {
        if samples.len() != self.format.channels as usize || original_frame >= MAX_CLOCK_FRAMES {
            self.input_error = Some("invalid original PCM frame");
            return false;
        }
        if self.original_end.is_some_and(|end| original_frame < end) {
            self.input_error = Some("original PCM frame labels are not monotone");
            return false;
        }
        if self.finished || self.input_is_full() {
            return false;
        }
        let Some(next) = self.end.checked_add(1) else {
            self.input_error = Some("DSP input index exhausted");
            return false;
        };
        let input_index = (self.end % self.input.len() as u64) as usize;
        let slot = &mut self.input[input_index];
        slot.original = original_frame;
        slot.samples.fill(0.0);
        slot.samples[..samples.len()].copy_from_slice(samples);
        self.end = next;
        self.original_end = Some(original_frame + 1);
        true
    }
    pub(crate) fn input_is_full(&self) -> bool {
        self.end - self.origin >= self.input.len() as u64
    }
    pub(crate) fn input_error(&self) -> Option<&'static str> {
        self.input_error
    }
    pub(crate) fn finish(&mut self) {
        self.finished = true;
    }
    pub(crate) fn drained(&self) -> bool {
        self.finished
            && self.output_at == self.output_len
            && self.cursor >= self.end as u128 * FRACTION_SCALE
    }
    pub(crate) fn reset_seek(&mut self, original_frame: u64) {
        self.origin = 0;
        self.end = 0;
        self.cursor = 0;
        self.epoch_start = original_frame;
        self.original_end = None;
        self.output_at = 0;
        self.output_len = 0;
        self.primed = false;
        self.finished = false;
        self.input_error = None;
        self.overlap.fill([0.0; MAX_CHANNELS]);
        self.active = self.requested;
        self.prepare_sinc(self.active.settings.playback_rate);
    }

    #[cfg(test)]
    pub(crate) fn pop_output(&mut self) -> Option<ProcessedFrame> {
        self.pop_output_interruptible(|| false)
    }
    pub(crate) fn pop_output_interruptible(
        &mut self,
        mut interrupt: impl FnMut() -> bool,
    ) -> Option<ProcessedFrame> {
        if interrupt() {
            return None;
        }
        if self.output_at < self.output_len {
            let frame = self.output[self.output_at];
            self.output_at += 1;
            return Some(frame);
        }
        self.output_at = 0;
        self.output_len = 0;
        if self.active.sequence != self.requested.sequence
            || self.active.settings != self.requested.settings
        {
            let mode_changed = self.active.settings.preserves_pitch
                != self.requested.settings.preserves_pitch
                || (self.active.settings.playback_rate == 1.0)
                    != (self.requested.settings.playback_rate == 1.0);
            self.active = self.requested;
            self.prepare_sinc(self.active.settings.playback_rate);
            if mode_changed {
                self.primed = false;
                self.overlap.fill([0.0; MAX_CHANNELS]);
            }
        }
        if self.cursor >= self.end as u128 * FRACTION_SCALE {
            return None;
        }
        if self.active.settings.playback_rate == 1.0 || !self.active.settings.preserves_pitch {
            return self.varispeed();
        }
        if !self.make_hop(&mut interrupt) {
            return None;
        }
        let frame = self.output[0];
        self.output_at = 1;
        Some(frame)
    }

    fn sample(&self, index: i128, channel: usize) -> f64 {
        if index < self.origin as i128 || index >= self.end as i128 {
            return 0.0;
        }
        self.input[(index as u64 % self.input.len() as u64) as usize].samples[channel] as f64
    }
    fn stamp(&self, end: u128, latency_frames: u32) -> ProcessingStamp {
        let whole = (end / FRACTION_SCALE) as u64;
        let fraction = (end % FRACTION_SCALE) as u32;
        let (end_frame, fraction) = if whole == 0 && fraction == 0 {
            (self.epoch_start, 0)
        } else if whole >= self.end {
            (self.original_end.unwrap_or(self.epoch_start), 0)
        } else if fraction == 0 {
            (
                self.input[((whole - 1) % self.input.len() as u64) as usize].original + 1,
                0,
            )
        } else {
            (
                self.input[(whole % self.input.len() as u64) as usize].original,
                fraction,
            )
        };
        ProcessingStamp {
            end_frame,
            fraction,
            processing_sequence: self.active.sequence,
            settings: self.active.settings,
            latency_frames,
        }
    }
    fn next_cursor(&self) -> u128 {
        let next = self.cursor + self.active.rate as u128;
        if self.finished {
            next.min(self.end as u128 * FRACTION_SCALE)
        } else {
            next
        }
    }
    fn compact(&mut self, keep: u64) {
        self.origin = keep.max(self.origin).min(self.end);
    }
    fn varispeed(&mut self) -> Option<ProcessedFrame> {
        let index = (self.cursor / FRACTION_SCALE) as u64;
        let fraction = (self.cursor % FRACTION_SCALE) as f64 / FRACTION_SCALE as f64;
        let bypass = self.active.settings.playback_rate == 1.0;
        let future = if bypass {
            usize::from(fraction != 0.0)
        } else {
            FIR_TAPS / 2
        };
        if !self.finished && index.saturating_add(future as u64) >= self.end {
            return None;
        }
        let mut samples = [0.0f32; MAX_CHANNELS];
        let phase = (fraction * FIR_PHASES as f64)
            .floor()
            .min((FIR_PHASES - 1) as f64) as usize;
        for (channel, value) in samples
            .iter_mut()
            .enumerate()
            .take(self.format.channels as usize)
        {
            let sample = if bypass && fraction == 0.0 {
                self.sample(index as i128, channel)
            } else if bypass {
                let first = self.sample(index as i128, channel);
                first + (self.sample(index as i128 + 1, channel) - first) * fraction
            } else {
                (0..FIR_TAPS)
                    .map(|tap| {
                        self.sample(
                            index as i128 + tap as i128 - (FIR_TAPS / 2 - 1) as i128,
                            channel,
                        ) * self.sinc[phase * FIR_TAPS + tap]
                    })
                    .sum()
            };
            *value = sample as f32;
        }
        let end = self.next_cursor();
        let latency = if bypass {
            0
        } else {
            (FIR_TAPS as f64 / 2.0 / self.active.settings.playback_rate).ceil() as u32
        };
        let frame = ProcessedFrame {
            samples,
            stamp: self.stamp(end, latency),
        };
        self.cursor = end;
        self.compact(
            ((self.cursor / FRACTION_SCALE) as u64)
                .saturating_sub((self.search + self.hop + FIR_TAPS) as u64),
        );
        Some(frame)
    }
    fn prepare_sinc(&mut self, rate: f64) {
        let cutoff = 0.46 / rate.max(1.0);
        for phase in 0..FIR_PHASES {
            let fraction = phase as f64 / FIR_PHASES as f64;
            let mut sum = 0.0;
            for tap in 0..FIR_TAPS {
                let offset = tap as f64 - (FIR_TAPS / 2 - 1) as f64 - fraction;
                let argument = 2.0 * cutoff * offset;
                let sinc = if argument.abs() < 1e-14 {
                    1.0
                } else {
                    (std::f64::consts::PI * argument).sin() / (std::f64::consts::PI * argument)
                };
                let window = 0.54
                    - 0.46 * (std::f64::consts::TAU * tap as f64 / (FIR_TAPS - 1) as f64).cos();
                let weight = 2.0 * cutoff * sinc * window;
                self.sinc[phase * FIR_TAPS + tap] = weight;
                sum += weight;
            }
            for tap in 0..FIR_TAPS {
                self.sinc[phase * FIR_TAPS + tap] /= sum;
            }
        }
    }
    fn make_hop(&mut self, interrupt: &mut impl FnMut() -> bool) -> bool {
        let frame = 2 * self.hop;
        let base = (self.cursor / FRACTION_SCALE) as u64;
        let needed = base
            .saturating_add(self.search as u64)
            .saturating_add(frame as u64);
        if !self.finished
            && self.end
                < if self.primed {
                    needed.max(self.last_source + frame as u64)
                } else {
                    base + frame as u64
                }
        {
            return false;
        }
        let source = if self.primed {
            let lo = base.saturating_sub(self.search as u64).max(self.origin);
            let hi = base
                .saturating_add(self.search as u64)
                .min(if self.finished {
                    self.end.saturating_sub(1)
                } else {
                    self.end.saturating_sub(frame as u64)
                });
            if lo > hi {
                return false;
            }
            let target = self.last_source + self.hop as u64;
            let mut best = base.clamp(lo, hi);
            let mut best_score = f64::NEG_INFINITY;
            let stride = (hi - lo).div_ceil(128).max(1);
            for candidate in (lo..=hi).step_by(stride as usize) {
                let Some(score) = self.correlation(candidate, target, 256, interrupt) else {
                    return false;
                };
                if score > best_score + 1e-12
                    || ((score - best_score).abs() <= 1e-12
                        && candidate.abs_diff(base) < best.abs_diff(base))
                {
                    best = candidate;
                    best_score = score;
                }
            }
            let Some(ideal_score) = self.correlation(base.clamp(lo, hi), target, 256, interrupt)
            else {
                return false;
            };
            if ideal_score > best_score + 1e-12
                || ((ideal_score - best_score).abs() <= 1e-12 && best.abs_diff(base) != 0)
            {
                best = base.clamp(lo, hi);
            }
            let refine_lo = best.saturating_sub(stride).max(lo);
            let refine_hi = best.saturating_add(stride).min(hi);
            best_score = f64::NEG_INFINITY;
            for candidate in refine_lo..=refine_hi {
                let Some(score) = self.correlation(candidate, target, 512, interrupt) else {
                    return false;
                };
                if score > best_score + 1e-12
                    || ((score - best_score).abs() <= 1e-12
                        && candidate.abs_diff(base) < best.abs_diff(base))
                {
                    best = candidate;
                    best_score = score;
                }
            }
            best
        } else {
            base
        };
        let latency =
            ((frame + self.search) as f64 / self.active.settings.playback_rate).ceil() as u32;
        for index in 0..self.hop {
            if self.finished && self.cursor >= self.end as u128 * FRACTION_SCALE {
                break;
            }
            let mut samples = [0.0f32; MAX_CHANNELS];
            for (channel, value) in samples
                .iter_mut()
                .enumerate()
                .take(self.format.channels as usize)
            {
                let head = self.sample(source as i128 + index as i128, channel);
                *value = if self.primed {
                    (self.overlap[index][channel] + head * self.window[index]) as f32
                } else {
                    head as f32
                };
                self.overlap[index][channel] = self
                    .sample(source as i128 + self.hop as i128 + index as i128, channel)
                    * self.window[self.hop + index];
            }
            let end = self.next_cursor();
            self.output[self.output_len] = ProcessedFrame {
                samples,
                stamp: self.stamp(end, latency),
            };
            self.output_len += 1;
            self.cursor = end;
        }
        self.primed = true;
        self.last_source = source;
        self.compact(
            source.min(((self.cursor / FRACTION_SCALE) as u64).saturating_sub(self.search as u64)),
        );
        self.output_len != 0
    }

    fn correlation(
        &self,
        candidate: u64,
        target: u64,
        points: usize,
        interrupt: &mut impl FnMut() -> bool,
    ) -> Option<f64> {
        let mut dot = 0.0;
        let mut left_power = 0.0;
        let mut right_power = 0.0;
        for (point, index) in (0..self.hop).step_by(self.hop.div_ceil(points)).enumerate() {
            if point % 32 == 0 && interrupt() {
                return None;
            }
            for channel in 0..self.format.channels as usize {
                let left = self.sample(candidate as i128 + index as i128, channel);
                let right = self.sample(target as i128 + index as i128, channel);
                dot += left * right;
                left_power += left * left;
                right_power += right * right;
            }
        }
        let scale = (left_power * right_power).sqrt();
        Some(if scale > 1e-24 { dot / scale } else { 0.0 })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render(
        format: PcmFormat,
        settings: PlaybackProcessingSettings,
        frames: usize,
        signal: impl Fn(usize, usize) -> f32,
    ) -> Vec<ProcessedFrame> {
        let mut processor = WorkerProcessor::new(format).unwrap();
        processor.set_settings(settings, 1).unwrap();
        let mut result = Vec::new();
        for frame in 0..frames {
            let mut input = [0.0; MAX_CHANNELS];
            for (channel, sample) in input.iter_mut().enumerate().take(format.channels as usize) {
                *sample = signal(frame, channel);
            }
            while !processor.push_input(frame as u64, &input[..format.channels as usize]) {
                let output = processor
                    .pop_output()
                    .expect("bounded processor must make progress before input full");
                result.push(output);
            }
            while let Some(output) = processor.pop_output() {
                result.push(output);
            }
        }
        processor.finish();
        while let Some(output) = processor.pop_output() {
            result.push(output);
        }
        assert!(processor.drained());
        assert_eq!(
            result
                .last()
                .map(|frame| (frame.stamp.end_frame, frame.stamp.fraction)),
            Some((frames as u64, 0))
        );
        assert!(result.windows(2).all(|frames| (
            frames[0].stamp.end_frame,
            frames[0].stamp.fraction
        ) <= (
            frames[1].stamp.end_frame,
            frames[1].stamp.fraction
        )));
        result
    }
    fn frequency(frames: &[ProcessedFrame], sample_rate: u32) -> f64 {
        let mut first = None;
        let mut last = 0.0;
        let mut count = 0;
        for (index, pair) in frames.windows(2).enumerate() {
            let left = pair[0].samples[0];
            let right = pair[1].samples[0];
            if left < 0.0 && right >= 0.0 {
                let crossing = index as f64 + (-left / (right - left)) as f64;
                first.get_or_insert(crossing);
                last = crossing;
                count += 1;
            }
        }
        sample_rate as f64 * (count - 1) as f64 / (last - first.unwrap())
    }
    #[test]
    fn neutral_is_bit_exact_and_fractional_varispeed_has_correct_duration_frequency_and_stamps() {
        let format = PcmFormat {
            sample_rate: 22_050,
            channels: 2,
        };
        let result = render(
            format,
            PlaybackProcessingSettings::default(),
            64,
            |frame, channel| {
                if channel == 0 {
                    frame as f32 / 1_000.0
                } else {
                    -0.0
                }
            },
        );
        assert!(
            result
                .iter()
                .enumerate()
                .all(|(index, frame)| frame.samples[0] == index as f32 / 1_000.0
                    && frame.samples[1].to_bits() == (-0.0f32).to_bits())
        );
        for rate in [0.5, 1.5, 2.0] {
            let settings = PlaybackProcessingSettings {
                playback_rate: rate,
                preserves_pitch: false,
                pitch_ratio: 1.0,
            };
            let frames = render(format, settings, 44_100, |frame, channel| {
                if channel == 0 {
                    (frame as f64 * std::f64::consts::TAU * 997.0 / 22_050.0).sin() as f32 * 0.02
                } else {
                    0.0
                }
            });
            assert_eq!(frames.len(), (44_100.0 / rate).ceil() as usize);
            let edge = 4_410;
            assert!(
                (frequency(&frames[edge..frames.len() - edge], 22_050) - 997.0 * rate).abs() < 0.5
            );
            assert!(frames.iter().all(|frame| frame.samples[1] == 0.0));
            assert_eq!(frames[0].stamp.processing_sequence, 1);
            assert_eq!(frames[0].stamp.settings, settings);
            assert_eq!(frames[0].stamp.end_frame, rate.floor() as u64);
        }
    }
    #[test]
    fn preserved_tempo_keeps_low_bass_and_tone_frequency_with_exact_nominal_duration() {
        for sample_rate in [22_050, 48_000] {
            for tone in [31.0, 997.0] {
                for rate in [0.5, 2.0] {
                    let format = PcmFormat {
                        sample_rate,
                        channels: 1,
                    };
                    let settings = PlaybackProcessingSettings {
                        playback_rate: rate,
                        ..PlaybackProcessingSettings::default()
                    };
                    let frames = render(format, settings, sample_rate as usize * 2, |frame, _| {
                        (frame as f64 * std::f64::consts::TAU * tone / sample_rate as f64).sin()
                            as f32
                            * 0.02
                    });
                    assert_eq!(
                        frames.len(),
                        (sample_rate as f64 * 2.0 / rate).ceil() as usize
                    );
                    let edge = sample_rate as usize / 5;
                    let actual = frequency(&frames[edge..frames.len() - edge], sample_rate);
                    assert!(
                        (actual - tone).abs() < 0.6,
                        "{sample_rate}Hz rate{rate} tone{tone} became {actual}"
                    );
                    assert!(frames.iter().all(|frame| frame.samples[0].is_finite()));
                }
            }
        }
    }
    #[test]
    fn high_frequency_aliasing_is_suppressed_by_prepared_varispeed_filter() {
        let format = PcmFormat {
            sample_rate: 22_050,
            channels: 1,
        };
        let settings = PlaybackProcessingSettings {
            playback_rate: 2.0,
            preserves_pitch: false,
            pitch_ratio: 1.0,
        };
        let frames = render(format, settings, 22_050, |frame, _| {
            (frame as f64 * std::f64::consts::TAU * 9_000.0 / 22_050.0).sin() as f32 * 0.02
        });
        let body = &frames[1_000..frames.len() - 1_000];
        let rms = (body
            .iter()
            .map(|frame| (frame.samples[0] as f64).powi(2))
            .sum::<f64>()
            / body.len() as f64)
            .sqrt();
        assert!(rms < 0.0002, "aliased output RMS {rms}");
    }
    #[test]
    fn rate_changes_keep_old_staging_stamps_and_seek_discards_them() {
        let format = PcmFormat {
            sample_rate: 8_000,
            channels: 1,
        };
        let mut processor = WorkerProcessor::new(format).unwrap();
        let first = PlaybackProcessingSettings {
            playback_rate: 0.5,
            ..Default::default()
        };
        processor.set_settings(first, 10).unwrap();
        for frame in 0..512 {
            assert!(processor.push_input(frame, &[0.01]));
        }
        let old = processor.pop_output().unwrap();
        assert_eq!(old.stamp.processing_sequence, 10);
        processor
            .set_settings(
                PlaybackProcessingSettings {
                    playback_rate: 2.0,
                    ..Default::default()
                },
                11,
            )
            .unwrap();
        for _ in 0..10 {
            assert_eq!(
                processor.pop_output().unwrap().stamp.processing_sequence,
                10
            );
        }
        processor.reset_seek(80_000);
        assert!(processor.pop_output().is_none());
        for frame in 80_000..81_024 {
            assert!(processor.push_input(frame, &[0.0]));
        }
        let next = processor.pop_output().unwrap();
        assert_eq!(next.stamp.processing_sequence, 11);
        assert!(next.stamp.end_frame >= 80_000);
        assert_eq!(next.samples, [0.0; MAX_CHANNELS]);
    }
    #[test]
    fn search_interruption_is_bounded_and_dsp_quota_releases_on_actual_drop() {
        let baseline = DspReservation::used_bytes();
        let format = PcmFormat {
            sample_rate: 8_000,
            channels: 8,
        };
        {
            let mut processor = WorkerProcessor::new(format).unwrap();
            processor
                .set_settings(
                    PlaybackProcessingSettings {
                        playback_rate: 0.5,
                        ..Default::default()
                    },
                    1,
                )
                .unwrap();
            assert!(DspReservation::used_bytes() > baseline);
            for frame in 0..1_000 {
                assert!(processor.push_input(frame, &[0.01; 8]));
            }
            for _ in 0..processor.hop {
                processor.pop_output().unwrap();
            }
            let cursor = processor.cursor;
            let mut checks = 0;
            assert!(
                processor
                    .pop_output_interruptible(|| {
                        checks += 1;
                        checks > 10
                    })
                    .is_none()
            );
            assert_eq!(processor.cursor, cursor);
            assert!(checks <= 11);
            let settings = PlaybackProcessingSettings {
                playback_rate: f64::NAN,
                ..PlaybackProcessingSettings::default()
            };
            assert!(processor.set_settings(settings, 2).is_err());
            assert_eq!(processor.requested.sequence, 1);
        }
        assert_eq!(DspReservation::used_bytes(), baseline);
        let remaining = DSP_QUOTA_BYTES - baseline;
        let reservation = DspReservation::new(remaining).unwrap();
        assert!(WorkerProcessor::new(format).is_err());
        assert_eq!(DspReservation::used_bytes(), DSP_QUOTA_BYTES);
        drop(reservation);
        assert_eq!(DspReservation::used_bytes(), baseline);
    }
}

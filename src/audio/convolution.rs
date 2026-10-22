/*
 * Copyright (C) 2010 Google Inc. All rights reserved.
 *
 * Redistribution and use in source and binary forms, with or without
 * modification, are permitted provided that the following conditions
 * are met:
 *
 * 1.  Redistributions of source code must retain the above copyright
 *     notice, this list of conditions and the following disclaimer.
 * 2.  Redistributions in binary form must reproduce the above copyright
 *     notice, this list of conditions and the following disclaimer in the
 *     documentation and/or other materials provided with the distribution.
 * 3.  Neither the name of Apple Computer, Inc. ("Apple") nor the names of
 *     its contributors may be used to endorse or promote products derived
 *     from this software without specific prior written permission.
 *
 * THIS SOFTWARE IS PROVIDED BY APPLE AND ITS CONTRIBUTORS "AS IS" AND ANY
 * EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO, THE IMPLIED
 * WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE ARE
 * DISCLAIMED. IN NO EVENT SHALL APPLE OR ITS CONTRIBUTORS BE LIABLE FOR ANY
 * DIRECT, INDIRECT, INCIDENTAL, SPECIAL, EXEMPLARY, OR CONSEQUENTIAL DAMAGES
 * (INCLUDING, BUT NOT LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR SERVICES;
 * LOSS OF USE, DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER CAUSED AND
 * ON ANY THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY, OR TORT
 * (INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE OF
 * THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.
 */

//! Prepared partitioned convolution with fixed callback storage and media delay.
//! One/two-channel responses are independent stereo filters; four-channel
//! responses use LL/LR/RL/RR routing. Extra native output lanes remain zero wet.
use super::{
    pcm::{MAX_CHANNELS, MAX_CLOCK_FRAMES, PcmFormat, ProcessingStamp},
    processing::DspReservation,
};
use anyhow::{Context, Result, ensure};
use rustfft::{Fft, FftPlanner, num_complex::Complex};
use std::{
    mem::size_of,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

pub const CONVOLUTION_PARTITION: usize = 512;
const FFT_SIZE: usize = 1024;
const BINS: usize = 513;
const PLAN_BYTES: usize = 1024 * 1024;
const MAX_SCRATCH: usize = 4 * FFT_SIZE;
const FIXED_STORAGE_BYTES: usize = FFT_SIZE * size_of::<Complex<f64>>()
    + 6 * CONVOLUTION_PARTITION * size_of::<f64>()
    + 2 * CONVOLUTION_PARTITION * size_of::<Option<ConvolutionStamp>>();

pub struct ValidatedImpulse {
    sample_rate: u32,
    channels: u16,
    samples: Vec<f32>,
    _reservation: DspReservation,
}
pub struct ImpulseBuilder {
    sample_rate: u32,
    channels: u16,
    maximum_frames: usize,
    samples: Vec<f32>,
    reservation: DspReservation,
}
impl ImpulseBuilder {
    pub fn prepare(sample_rate: u32, channels: u16, maximum_frames: usize) -> Result<Self> {
        ensure!(
            (8000..=384000).contains(&sample_rate) && matches!(channels, 1 | 2 | 4),
            "Invalid impulse format"
        );
        ensure!(
            maximum_frames > 0 && maximum_frames <= sample_rate as usize * 4,
            "Invalid impulse frame budget"
        );
        let count = maximum_frames
            .checked_mul(channels as usize)
            .context("Impulse sample count overflow")?;
        let bytes = count
            .checked_mul(size_of::<f32>())
            .and_then(|bytes| bytes.checked_add(size_of::<Self>() + size_of::<ValidatedImpulse>()))
            .context("Impulse reservation overflow")?;
        let reservation = DspReservation::new(bytes)?;
        let mut samples = Vec::new();
        samples
            .try_reserve_exact(count)
            .context("Cannot reserve impulse PCM")?;
        ensure!(
            samples.capacity() <= count,
            "Impulse allocation exceeded its reservation"
        );
        Ok(Self {
            sample_rate,
            channels,
            maximum_frames,
            samples,
            reservation,
        })
    }
    pub fn push_frame(&mut self, samples: &[f32]) -> Result<()> {
        ensure!(
            samples.len() == self.channels as usize
                && samples.iter().all(|sample| sample.is_finite()),
            "Invalid impulse PCM frame"
        );
        ensure!(
            self.samples.len() / (self.channels as usize) < self.maximum_frames,
            "Impulse frame budget exhausted"
        );
        self.samples.extend_from_slice(samples);
        Ok(())
    }
    pub fn finish(self) -> Result<ValidatedImpulse> {
        ensure!(!self.samples.is_empty(), "Impulse response is empty");
        Ok(ValidatedImpulse {
            sample_rate: self.sample_rate,
            channels: self.channels,
            samples: self.samples,
            _reservation: self.reservation,
        })
    }
}
impl ValidatedImpulse {
    pub fn frames(&self) -> usize {
        self.samples.len() / self.channels as usize
    }
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }
    pub fn channels(&self) -> u16 {
        self.channels
    }
}
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ConvolutionStamp {
    pub token: u64,
    pub source_epoch: u64,
    pub media: ProcessingStamp,
}
impl ConvolutionStamp {
    fn valid(self) -> bool {
        self.token != 0
            && self.source_epoch != 0
            && (self.media.end_frame < MAX_CLOCK_FRAMES
                || self.media.end_frame == MAX_CLOCK_FRAMES && self.media.fraction == 0)
            && self.media.settings.playback_rate.is_finite()
            && (0.5..=2.0).contains(&self.media.settings.playback_rate)
            && self.media.settings.pitch_ratio.is_finite()
            && (0.5..=1.5).contains(&self.media.settings.pitch_ratio)
    }
    fn coordinate(self) -> (u64, u32) {
        (self.media.end_frame, self.media.fraction)
    }
}
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ConvolutionFrame {
    pub samples: [f64; MAX_CHANNELS],
    pub stamp: Option<ConvolutionStamp>,
    pub valid: bool,
    pub tail: bool,
}
impl ConvolutionFrame {
    fn padding() -> Self {
        Self {
            samples: [0.0; MAX_CHANNELS],
            stamp: None,
            valid: false,
            tail: false,
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConvolutionError {
    InvalidStamp,
    WrongStream,
    BackwardMedia,
    WrongChannels,
    NonFiniteInput,
    InputOverflow,
    AfterEof,
    CounterExhausted,
}
impl std::fmt::Display for ConvolutionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Invalid convolution operation: {self:?}")
    }
}
impl std::error::Error for ConvolutionError {}

pub struct PreparedConvolution {
    format: PcmFormat,
    ir_channels: usize,
    ir_frames: usize,
    partitions: usize,
    impulse: Vec<Complex<f64>>,
    history: Vec<Complex<f64>>,
    forward: Arc<dyn Fft<f64>>,
    inverse: Arc<dyn Fft<f64>>,
    scratch: Vec<Complex<f64>>,
    fft: Box<[Complex<f64>]>,
    input: [Box<[f64]>; 2],
    overlap: [Box<[f64]>; 2],
    output: [Box<[f64]>; 2],
    input_stamps: Box<[Option<ConvolutionStamp>]>,
    output_stamps: Box<[Option<ConvolutionStamp>]>,
    input_count: usize,
    output_index: usize,
    output_count: usize,
    head: usize,
    stream: Option<(u64, u64)>,
    last_stamp: Option<ConvolutionStamp>,
    accepted: u64,
    emitted: u64,
    eof: bool,
    padding_remaining: usize,
    normalization_scale: f64,
    maximum_input: f64,
    reserved_bytes: usize,
    _reservation: DspReservation,
}
impl PreparedConvolution {
    /// Resampling and every FFT plan/allocation occur on the preparation owner.
    pub fn prepare(
        format: PcmFormat,
        impulse: &ValidatedImpulse,
        normalize: bool,
        cancel: &AtomicBool,
    ) -> Result<Self> {
        ensure!(
            (8000..=384000).contains(&format.sample_rate) && (1..=8).contains(&format.channels),
            "Invalid convolution output format"
        );
        let ir_frames = (impulse.frames() as u64 * format.sample_rate as u64)
            .div_ceil(impulse.sample_rate as u64) as usize;
        let partitions = ir_frames.div_ceil(CONVOLUTION_PARTITION);
        let spectrum_count = (impulse.channels as usize + 2)
            .checked_mul(partitions)
            .and_then(|count| count.checked_mul(BINS))
            .context("Convolution storage overflow")?;
        let bytes = spectrum_count
            .checked_mul(size_of::<Complex<f64>>())
            .and_then(|bytes| {
                bytes.checked_add(impulse.channels as usize * ir_frames * size_of::<f64>())
            })
            .and_then(|bytes| {
                bytes.checked_add(
                    MAX_SCRATCH * size_of::<Complex<f64>>()
                        + PLAN_BYTES
                        + FIXED_STORAGE_BYTES
                        + size_of::<Self>(),
                )
            })
            .context("Convolution reservation overflow")?;
        let mut reservation = DspReservation::new(bytes)?;
        ensure!(
            !cancel.load(Ordering::Acquire),
            "Convolution preparation cancelled"
        );
        let mut resampled = fixed_vector(ir_frames * impulse.channels as usize, 0.0)?;
        for frame in 0..ir_frames {
            if frame.is_multiple_of(16) {
                ensure!(
                    !cancel.load(Ordering::Acquire),
                    "Convolution preparation cancelled"
                );
            }
            let samples = resample_frame(impulse, frame, format.sample_rate);
            for channel in 0..impulse.channels as usize {
                resampled[frame * impulse.channels as usize + channel] = samples[channel];
            }
        }
        let energy =
            resampled.iter().map(|value| value * value).sum::<f64>() / resampled.len() as f64;
        ensure!(
            energy.is_finite(),
            "Impulse normalization energy is non-finite"
        );
        // Chromium convolver normalization: RMS floor, -58 dB calibration,
        // sample-rate compensation and half gain for a true-stereo response.
        let normalization_scale = if normalize {
            10.0f64.powf(-58.0 / 20.0) / energy.sqrt().max(0.000125) * 44100.0
                / format.sample_rate as f64
                * if impulse.channels == 4 { 0.5 } else { 1.0 }
        } else {
            1.0
        };
        let ir_abs = resampled
            .iter()
            .map(|value| value.abs() * normalization_scale.abs())
            .sum::<f64>();
        let maximum_input = (f64::MAX
            / ((ir_abs + 1.0) * FFT_SIZE as f64 * FFT_SIZE as f64 * 16.0))
            .min(f32::MAX as f64 / ((ir_abs + 1.0) * 2.0));
        ensure!(
            maximum_input.is_finite() && maximum_input > 0.0,
            "Impulse amplification exceeds the finite processing budget"
        );
        let mut planner = FftPlanner::<f64>::new();
        let forward = planner.plan_fft_forward(FFT_SIZE);
        let inverse = planner.plan_fft_inverse(FFT_SIZE);
        let scratch_len = forward
            .get_inplace_scratch_len()
            .max(inverse.get_inplace_scratch_len());
        ensure!(
            scratch_len <= MAX_SCRATCH,
            "Convolution FFT scratch exceeds its reservation"
        );
        let mut scratch = fixed_vector(scratch_len, Complex::new(0.0, 0.0))?;
        let mut spectra = fixed_vector(
            impulse.channels as usize * partitions * BINS,
            Complex::new(0.0, 0.0),
        )?;
        let mut fft = fixed_vector(FFT_SIZE, Complex::new(0.0, 0.0))?.into_boxed_slice();
        for channel in 0..impulse.channels as usize {
            for partition in 0..partitions {
                ensure!(
                    !cancel.load(Ordering::Acquire),
                    "Convolution preparation cancelled"
                );
                fft.fill(Complex::new(0.0, 0.0));
                for index in 0..CONVOLUTION_PARTITION {
                    let frame = partition * CONVOLUTION_PARTITION + index;
                    if frame < ir_frames {
                        fft[index].re = resampled[frame * impulse.channels as usize + channel]
                            * normalization_scale;
                    }
                }
                forward.process_with_scratch(&mut fft, &mut scratch);
                spectra[(channel * partitions + partition) * BINS
                    ..(channel * partitions + partition + 1) * BINS]
                    .copy_from_slice(&fft[..BINS]);
            }
        }
        let history = fixed_vector(2 * partitions * BINS, Complex::new(0.0, 0.0))?;
        ensure!(
            !cancel.load(Ordering::Acquire),
            "Convolution preparation cancelled"
        );
        drop(resampled);
        let retained_bytes = bytes - impulse.channels as usize * ir_frames * size_of::<f64>();
        reservation.shrink_to(retained_bytes)?;
        Ok(Self {
            format,
            ir_channels: impulse.channels as usize,
            ir_frames,
            partitions,
            impulse: spectra,
            history,
            forward,
            inverse,
            scratch,
            fft,
            input: [
                fixed_vector(CONVOLUTION_PARTITION, 0.0)?.into_boxed_slice(),
                fixed_vector(CONVOLUTION_PARTITION, 0.0)?.into_boxed_slice(),
            ],
            overlap: [
                fixed_vector(CONVOLUTION_PARTITION, 0.0)?.into_boxed_slice(),
                fixed_vector(CONVOLUTION_PARTITION, 0.0)?.into_boxed_slice(),
            ],
            output: [
                fixed_vector(CONVOLUTION_PARTITION, 0.0)?.into_boxed_slice(),
                fixed_vector(CONVOLUTION_PARTITION, 0.0)?.into_boxed_slice(),
            ],
            input_stamps: fixed_vector(CONVOLUTION_PARTITION, None)?.into_boxed_slice(),
            output_stamps: fixed_vector(CONVOLUTION_PARTITION, None)?.into_boxed_slice(),
            input_count: 0,
            output_index: 0,
            output_count: 0,
            head: 0,
            stream: None,
            last_stamp: None,
            accepted: 0,
            emitted: 0,
            eof: false,
            padding_remaining: CONVOLUTION_PARTITION,
            normalization_scale,
            maximum_input,
            reserved_bytes: retained_bytes,
            _reservation: reservation,
        })
    }
    pub fn reset(
        &mut self,
        token: u64,
        source_epoch: u64,
    ) -> std::result::Result<(), ConvolutionError> {
        if token == 0 || source_epoch == 0 {
            return Err(ConvolutionError::InvalidStamp);
        }
        self.history.fill(Complex::new(0.0, 0.0));
        self.input.iter_mut().for_each(|lane| lane.fill(0.0));
        self.output.iter_mut().for_each(|lane| lane.fill(0.0));
        self.overlap.iter_mut().for_each(|lane| lane.fill(0.0));
        self.input_stamps.fill(None);
        self.output_stamps.fill(None);
        self.fft.fill(Complex::new(0.0, 0.0));
        self.input_count = 0;
        self.output_index = 0;
        self.output_count = 0;
        self.head = 0;
        self.stream = Some((token, source_epoch));
        self.last_stamp = None;
        self.accepted = 0;
        self.emitted = 0;
        self.eof = false;
        self.padding_remaining = CONVOLUTION_PARTITION;
        Ok(())
    }
    pub fn frame(
        &mut self,
        samples: &[f64],
        stamp: ConvolutionStamp,
    ) -> std::result::Result<ConvolutionFrame, ConvolutionError> {
        if self.eof {
            return Err(ConvolutionError::AfterEof);
        }
        if samples.len() != self.format.channels as usize {
            return Err(ConvolutionError::WrongChannels);
        }
        if !stamp.valid() {
            return Err(ConvolutionError::InvalidStamp);
        }
        if self.stream != Some((stamp.token, stamp.source_epoch)) {
            return Err(ConvolutionError::WrongStream);
        }
        if self
            .last_stamp
            .is_some_and(|last| stamp.coordinate() < last.coordinate())
        {
            return Err(ConvolutionError::BackwardMedia);
        }
        if !samples.iter().all(|sample| sample.is_finite()) {
            return Err(ConvolutionError::NonFiniteInput);
        }
        if samples
            .iter()
            .any(|sample| sample.abs() > self.maximum_input)
        {
            return Err(ConvolutionError::InputOverflow);
        }
        let Some(next) = self.accepted.checked_add(1) else {
            return Err(ConvolutionError::CounterExhausted);
        };
        if next.checked_add(self.ir_frames as u64 - 1).is_none() {
            return Err(ConvolutionError::CounterExhausted);
        }
        let result = if self.padding_remaining > 0 {
            self.padding_remaining -= 1;
            ConvolutionFrame::padding()
        } else {
            self.pop()
        };
        self.input[0][self.input_count] = samples[0];
        self.input[1][self.input_count] = if samples.len() == 1 {
            samples[0]
        } else {
            samples[1]
        };
        self.input_stamps[self.input_count] = Some(stamp);
        self.input_count += 1;
        self.accepted = next;
        self.last_stamp = Some(stamp);
        if self.input_count == CONVOLUTION_PARTITION {
            self.process_block();
        }
        Ok(result)
    }
    pub fn begin_eof(&mut self) {
        self.eof = true;
    }
    pub fn drain(&mut self) -> Option<ConvolutionFrame> {
        if !self.eof || self.drained() {
            return None;
        }
        if self.padding_remaining > 0 {
            self.padding_remaining -= 1;
            return Some(ConvolutionFrame::padding());
        }
        if self.output_index == self.output_count {
            self.process_block();
        }
        Some(self.pop())
    }
    pub fn drained(&self) -> bool {
        self.eof
            && (self.accepted == 0
                || self.emitted
                    >= self
                        .accepted
                        .checked_add(self.ir_frames as u64 - 1)
                        .expect("Tail total was admitted before input mutation"))
    }
    pub fn latency_frames(&self) -> u32 {
        CONVOLUTION_PARTITION as u32
    }
    pub fn reserved_bytes(&self) -> usize {
        self.reserved_bytes
    }
    pub fn normalization_scale(&self) -> f64 {
        self.normalization_scale
    }
    pub fn impulse_frames(&self) -> usize {
        self.ir_frames
    }
    pub fn wet_channels(&self) -> u16 {
        2
    }
    pub fn format(&self) -> PcmFormat {
        self.format
    }
    pub(crate) fn tighten_input_bound(&mut self, factor: f64) -> Result<()> {
        ensure!(
            factor.is_finite() && factor >= 1.0,
            "Invalid convolution amplitude margin"
        );
        self.maximum_input /= factor;
        Ok(())
    }
    fn pop(&mut self) -> ConvolutionFrame {
        if self.output_index == self.output_count {
            return ConvolutionFrame::padding();
        }
        let tail = self.emitted >= self.accepted;
        let mut samples = [0.0; MAX_CHANNELS];
        samples[0] = self.output[0][self.output_index];
        samples[1] = self.output[1][self.output_index];
        let stamp = if tail {
            None
        } else {
            self.output_stamps[self.output_index]
        };
        self.output_index += 1;
        self.emitted += 1;
        ConvolutionFrame {
            samples,
            stamp,
            valid: true,
            tail,
        }
    }
    fn process_block(&mut self) {
        for lane in 0..2 {
            self.fft.fill(Complex::new(0.0, 0.0));
            for index in 0..CONVOLUTION_PARTITION {
                self.fft[index].re = self.input[lane][index];
            }
            self.forward
                .process_with_scratch(&mut self.fft, &mut self.scratch);
            let start = (lane * self.partitions + self.head) * BINS;
            self.history[start..start + BINS].copy_from_slice(&self.fft[..BINS]);
        }
        for output_lane in 0..2 {
            self.fft.fill(Complex::new(0.0, 0.0));
            for input_lane in 0..2 {
                let route = match self.ir_channels {
                    1 if input_lane == output_lane => Some(0),
                    2 if input_lane == output_lane => Some(output_lane),
                    4 => Some(input_lane * 2 + output_lane),
                    _ => None,
                };
                if let Some(route) = route {
                    for partition in 0..self.partitions {
                        let history = (self.head + self.partitions - partition) % self.partitions;
                        let input_start = (input_lane * self.partitions + history) * BINS;
                        let ir_start = (route * self.partitions + partition) * BINS;
                        for bin in 0..BINS {
                            self.fft[bin] +=
                                self.history[input_start + bin] * self.impulse[ir_start + bin];
                        }
                    }
                }
            }
            for bin in 1..BINS - 1 {
                self.fft[FFT_SIZE - bin] = self.fft[bin].conj();
            }
            self.inverse
                .process_with_scratch(&mut self.fft, &mut self.scratch);
            for index in 0..CONVOLUTION_PARTITION {
                self.output[output_lane][index] =
                    self.fft[index].re / FFT_SIZE as f64 + self.overlap[output_lane][index];
                self.overlap[output_lane][index] =
                    self.fft[index + CONVOLUTION_PARTITION].re / FFT_SIZE as f64;
            }
        }
        self.output_stamps.copy_from_slice(&self.input_stamps);
        self.input_stamps.fill(None);
        self.input.iter_mut().for_each(|lane| lane.fill(0.0));
        self.input_count = 0;
        self.output_index = 0;
        self.output_count = CONVOLUTION_PARTITION;
        self.head = (self.head + 1) % self.partitions;
    }
}

fn fixed_vector<T: Clone>(count: usize, value: T) -> Result<Vec<T>> {
    let mut buffer = Vec::new();
    buffer
        .try_reserve_exact(count)
        .context("Cannot allocate reserved convolution storage")?;
    ensure!(
        buffer.capacity() <= count,
        "Convolution allocation exceeded its reservation"
    );
    buffer.resize(count, value);
    Ok(buffer)
}
fn resample_frame(impulse: &ValidatedImpulse, frame: usize, rate: u32) -> [f64; 4] {
    let mut sum = [0.0; 4];
    if rate == impulse.sample_rate {
        for (channel, target) in sum.iter_mut().enumerate().take(impulse.channels as usize) {
            *target = impulse.samples[frame * impulse.channels as usize + channel] as f64;
        }
        return sum;
    }
    let position = frame as f64 * impulse.sample_rate as f64 / rate as f64;
    let center = position.floor() as i64;
    let ratio = (rate as f64 / impulse.sample_rate as f64).min(1.0);
    let cutoff = ratio * 0.95;
    let radius = (64.0 / ratio).ceil() as i64;
    let mut weights = 0.0;
    for tap in -radius + 1..=radius {
        let index = center + tap;
        let distance = position - index as f64;
        let angle = std::f64::consts::PI * distance * cutoff;
        let sinc = if angle.abs() < 1e-12 {
            1.0
        } else {
            angle.sin() / angle
        };
        let window_angle = std::f64::consts::PI * distance / radius as f64;
        let window = 0.42 + 0.5 * window_angle.cos() + 0.08 * (2.0 * window_angle).cos();
        let weight = sinc * window * cutoff;
        weights += weight;
        if index >= 0 && (index as usize) < impulse.frames() {
            for (channel, target) in sum.iter_mut().enumerate().take(impulse.channels as usize) {
                *target += impulse.samples[index as usize * impulse.channels as usize + channel]
                    as f64
                    * weight;
            }
        }
    }
    for target in &mut sum {
        *target /= weights;
    }
    sum
}

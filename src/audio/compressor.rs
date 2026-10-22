/*
 * Copyright (C) 2011 Google Inc. All rights reserved.
 *
 * Redistribution and use in source and binary forms, with or without
 * modification, are permitted provided that the following conditions
 * are met:
 *
 * 1. Redistributions of source code must retain the above copyright
 *    notice, this list of conditions and the following disclaimer.
 * 2. Redistributions in binary form must reproduce the above copyright
 *    notice, this list of conditions and the following disclaimer in the
 *    documentation and/or other materials provided with the distribution.
 * 3. Neither the name of Apple Computer, Inc. ("Apple") nor the names of
 *    its contributors may be used to endorse or promote products derived
 *    from this software without specific prior written permission.
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

//! Fixed-storage LX dynamics compressor, adapted from Chromium 148.0.7778.280
//! `third_party/blink/renderer/platform/audio/dynamics_compressor.{cc,h}`.
//!
//! Electron 42.11.6 pins that Chromium version. The detector, static knee,
//! makeup, subdivision and adaptive envelope follow its f32 algorithm. Native
//! math functions are used; browser SIMD/fdlibm bit equivalence is not implied.
//! A subdivision persists across arbitrary callback lengths. Mono duplicates
//! internally; native channels beyond L/R keep their gain and share the delay.
//! Non-finite input becomes finite zero and is counted before detection. Delay
//! padding carries no media stamp. EOF drains retained audio only; Chromium's
//! separate 1.625-second metering-node lifetime does not extend media playback.
//! Preparation and destruction belong to the owner, outside audio callbacks.

use super::{
    pcm::{MAX_CHANNELS, MAX_CLOCK_FRAMES, ProcessingStamp},
    processing::DspReservation,
};
use anyhow::{Context, Result, ensure};
use std::{fmt, mem::size_of};

pub const COMPRESSOR_DELAY_CAPACITY: usize = 1024;
pub const COMPRESSOR_SUBDIVISION_FRAMES: usize = 32;
const PREPARATION_ALLOWANCE: usize = 4096;
const PRE_DELAY_SECONDS: f32 = 0.006;
const DETECTOR_RELEASE_SECONDS: f32 = 0.0025;
const METER_RELEASE_SECONDS: f32 = 0.325;
const THRESHOLD_DB: f32 = -24.0;
const KNEE_DB: f32 = 30.0;
const RATIO: f32 = 12.0;
const ATTACK_SECONDS: f32 = 0.003;
const RELEASE_SECONDS: f32 = 0.250;

// Preserve the published f32 coefficient expressions and their rounding.
#[allow(clippy::excessive_precision)]
const RELEASE_POLYNOMIAL: [f32; 5] = [
    0.9999999999999998_f32 * 0.09 + 1.8432219684323923e-16_f32 * 0.16
        - 1.9373394351676423e-16_f32 * 0.42
        + 8.824516011816245e-18_f32 * 0.98,
    -1.5788320352845888_f32 * 0.09 + 2.3305837032074286_f32 * 0.16 - 0.9141194204840429_f32 * 0.42
        + 0.1623677525612032_f32 * 0.98,
    0.5334142869106424_f32 * 0.09 - 1.272736789213631_f32 * 0.16 + 0.9258856042207512_f32 * 0.42
        - 0.18656310191776226_f32 * 0.98,
    0.08783463138207234_f32 * 0.09 - 0.1694162967925622_f32 * 0.16 + 0.08588057951595272_f32 * 0.42
        - 0.00429891410546283_f32 * 0.98,
    -0.042416883008123074_f32 * 0.09 + 0.1115693827987602_f32 * 0.16
        - 0.09764676325265872_f32 * 0.42
        + 0.028494263462021576_f32 * 0.98,
];

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CompressorStamp {
    pub token: u64,
    pub source_epoch: u64,
    pub media: ProcessingStamp,
}
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CompressorFrame {
    pub samples: [f32; MAX_CHANNELS],
    pub stamp: Option<CompressorStamp>,
    /// False only for fixed delay padding, never inferred from sample amplitude.
    pub valid: bool,
}
impl CompressorFrame {
    const EMPTY: Self = Self {
        samples: [0.0; MAX_CHANNELS],
        stamp: None,
        valid: false,
    };
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompressorChannelPolicy {
    MonoDuplicated,
    StereoLinked,
    NativeStereoWithDelayedDryExtras,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CompressorStatistics {
    pub accepted_frames: u64,
    pub processed_frames: u64,
    pub valid_output_frames: u64,
    pub sanitized_input_samples: u64,
    pub sanitized_output_samples: u64,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompressorError {
    WrongChannelCount,
    InvalidLevel,
    InvalidStamp,
    WrongStream,
    BackwardMediaPosition,
    CounterExhausted,
    EndOfInput,
    EndNotDeclared,
}
impl fmt::Display for CompressorError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "Invalid compressor operation: {self:?}")
    }
}
impl std::error::Error for CompressorError {}

#[derive(Clone, Copy)]
struct Curve {
    threshold: f32,
    knee_threshold: f32,
    knee_output_db: f32,
    knee: f32,
}
impl Curve {
    fn prepare() -> Self {
        let threshold = db_to_linear(THRESHOLD_DB);
        let knee_threshold = db_to_linear(THRESHOLD_DB + KNEE_DB);
        let mut curve = Self {
            threshold,
            knee_threshold,
            knee_output_db: 0.0,
            knee: 5.0,
        };
        let x = knee_threshold;
        // The original 1.001 literal is double, with a final float conversion.
        let x2 = (f64::from(x) * 1.001) as f32;
        let db_x = THRESHOLD_DB + KNEE_DB;
        let db_x2 = linear_to_db(x2);
        let (mut minimum, mut maximum) = (0.1_f32, 10000.0_f32);
        let mut knee = 5.0;
        for _ in 0..15 {
            let slope = (linear_to_db(curve.knee_curve(x2, knee))
                - linear_to_db(curve.knee_curve(x, knee)))
                / (db_x2 - db_x);
            if slope < 1.0 / RATIO {
                maximum = knee;
            } else {
                minimum = knee;
            }
            knee = (minimum * maximum).sqrt();
        }
        curve.knee = knee;
        curve.knee_output_db = linear_to_db(curve.knee_curve(knee_threshold, knee));
        curve
    }
    fn knee_curve(self, value: f32, knee: f32) -> f32 {
        if value < self.threshold {
            value
        } else {
            self.threshold + (1.0 - f64::from(-knee * (value - self.threshold)).exp() as f32) / knee
        }
    }
    fn saturate(self, value: f32) -> f32 {
        if value < self.knee_threshold {
            self.knee_curve(value, self.knee)
        } else {
            db_to_linear(
                self.knee_output_db
                    + (1.0 / RATIO) * (linear_to_db(value) - (THRESHOLD_DB + KNEE_DB)),
            )
        }
    }
}
fn db_to_linear(value: f32) -> f32 {
    10.0_f32.powf(0.05 * value)
}
fn linear_to_db(value: f32) -> f32 {
    20.0 * value.log10()
}
fn finite_or(value: f32, fallback: f32) -> f32 {
    if value.is_finite() { value } else { fallback }
}
fn flush_denormal(value: f32) -> f32 {
    if value.is_subnormal() { 0.0 } else { value }
}

pub struct PreparedCompressor {
    sample_rate: u32,
    channels: u16,
    delay: usize,
    slots: Box<[CompressorFrame]>,
    read_index: usize,
    write_index: usize,
    curve: Curve,
    post_gain: f32,
    attack_frames: f32,
    detector_release_frames: f32,
    release_polynomial: [f32; 5],
    metering_release: f32,
    detector_average: f32,
    compressor_gain: f32,
    metering_gain: f32,
    maximum_attack_diff_db: f32,
    scaled_desired_gain: f32,
    envelope_rate: f32,
    subdivision: usize,
    stream: Option<(u64, u64)>,
    last_stamp: Option<CompressorStamp>,
    eof: bool,
    drain_remaining: usize,
    statistics: CompressorStatistics,
    reserved_bytes: usize,
    // The owned delay and scalar plan are destroyed before quota release.
    _reservation: DspReservation,
}
impl PreparedCompressor {
    pub fn prepare(sample_rate: u32, channels: u16) -> Result<Self> {
        ensure!(
            (8000..=384000).contains(&sample_rate) && (1..=MAX_CHANNELS as u16).contains(&channels),
            "Unsupported compressor PCM format"
        );
        let bytes = COMPRESSOR_DELAY_CAPACITY
            .checked_mul(size_of::<CompressorFrame>())
            .and_then(|bytes| bytes.checked_add(size_of::<Self>() + PREPARATION_ALLOWANCE))
            .context("Compressor reservation overflow")?;
        let reservation = DspReservation::new(bytes)?;
        let mut slots = Vec::new();
        slots
            .try_reserve_exact(COMPRESSOR_DELAY_CAPACITY)
            .context("Cannot reserve compressor delay")?;
        slots.resize(COMPRESSOR_DELAY_CAPACITY, CompressorFrame::EMPTY);
        let rate = sample_rate as f32;
        let delay = ((PRE_DELAY_SECONDS * rate) as usize).min(COMPRESSOR_DELAY_CAPACITY - 1);
        let curve = Curve::prepare();
        Ok(Self {
            sample_rate,
            channels,
            delay,
            slots: slots.into_boxed_slice(),
            read_index: 0,
            write_index: delay,
            curve,
            post_gain: (1.0 / curve.saturate(1.0)).powf(0.6),
            attack_frames: ATTACK_SECONDS.max(0.001) * rate,
            detector_release_frames: DETECTOR_RELEASE_SECONDS * rate,
            release_polynomial: RELEASE_POLYNOMIAL.map(|value| value * (RELEASE_SECONDS * rate)),
            metering_release: (1.0
                - (-1.0 / (f64::from(rate) * f64::from(METER_RELEASE_SECONDS))).exp())
                as f32,
            detector_average: 0.0,
            compressor_gain: 1.0,
            metering_gain: 1.0,
            maximum_attack_diff_db: -1.0,
            scaled_desired_gain: 0.0,
            envelope_rate: 0.0,
            subdivision: 0,
            stream: None,
            last_stamp: None,
            eof: false,
            drain_remaining: 0,
            statistics: CompressorStatistics::default(),
            reserved_bytes: bytes,
            _reservation: reservation,
        })
    }
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }
    pub fn channels(&self) -> u16 {
        self.channels
    }
    pub fn channel_policy(&self) -> CompressorChannelPolicy {
        match self.channels {
            1 => CompressorChannelPolicy::MonoDuplicated,
            2 => CompressorChannelPolicy::StereoLinked,
            _ => CompressorChannelPolicy::NativeStereoWithDelayedDryExtras,
        }
    }
    pub fn reserved_bytes(&self) -> usize {
        self.reserved_bytes
    }
    pub fn latency_frames(&self) -> u32 {
        self.delay as u32
    }
    pub fn reduction_db(&self) -> f32 {
        if self.statistics.processed_frames == 0 {
            0.0
        } else {
            finite_or(self.metering_gain, 0.0)
        }
    }
    pub fn statistics(&self) -> CompressorStatistics {
        self.statistics
    }
    /// Read-only static transfer curve, before perceptual makeup and envelope.
    pub fn transfer_level(&self, amplitude: f32) -> Result<f32, CompressorError> {
        if !amplitude.is_finite() || amplitude < 0.0 {
            return Err(CompressorError::InvalidLevel);
        }
        Ok(self.curve.saturate(amplitude))
    }
    /// Resets only fixed storage. Invalid stream identity leaves all state intact.
    pub fn reset(&mut self, token: u64, source_epoch: u64) -> Result<(), CompressorError> {
        if token == 0 || source_epoch == 0 {
            return Err(CompressorError::InvalidStamp);
        }
        self.slots.fill(CompressorFrame::EMPTY);
        self.read_index = 0;
        self.write_index = self.delay;
        self.detector_average = 0.0;
        self.compressor_gain = 1.0;
        self.metering_gain = 1.0;
        self.maximum_attack_diff_db = -1.0;
        self.scaled_desired_gain = 0.0;
        self.envelope_rate = 0.0;
        self.subdivision = 0;
        self.stream = Some((token, source_epoch));
        self.last_stamp = None;
        self.eof = false;
        self.drain_remaining = 0;
        self.statistics = CompressorStatistics::default();
        Ok(())
    }
    fn preflight(&self, stamp: Option<CompressorStamp>) -> Result<(), CompressorError> {
        if self.statistics.processed_frames == u64::MAX
            || self.slots[self.read_index].valid && self.statistics.valid_output_frames == u64::MAX
        {
            return Err(CompressorError::CounterExhausted);
        }
        if let Some(stamp) = stamp {
            let media = stamp.media;
            if stamp.token == 0
                || stamp.source_epoch == 0
                || media.end_frame > MAX_CLOCK_FRAMES
                || media.end_frame == MAX_CLOCK_FRAMES && media.fraction != 0
                || !media.settings.playback_rate.is_finite()
                || !(0.5..=2.0).contains(&media.settings.playback_rate)
                || !media.settings.pitch_ratio.is_finite()
                || !(0.5..=1.5).contains(&media.settings.pitch_ratio)
            {
                return Err(CompressorError::InvalidStamp);
            }
            if self.stream != Some((stamp.token, stamp.source_epoch)) {
                return Err(CompressorError::WrongStream);
            }
            if self.last_stamp.is_some_and(|previous| {
                (media.end_frame, media.fraction)
                    < (previous.media.end_frame, previous.media.fraction)
            }) {
                return Err(CompressorError::BackwardMediaPosition);
            }
        }
        Ok(())
    }
    /// Accepts one real input frame. NaN/Inf becomes zero before the linked
    /// detector; sanitization is counted. Wrong stamps/channels mutate nothing.
    pub fn frame(
        &mut self,
        samples: &[f32],
        stamp: Option<CompressorStamp>,
    ) -> Result<CompressorFrame, CompressorError> {
        if self.eof {
            return Err(CompressorError::EndOfInput);
        }
        if samples.len() != usize::from(self.channels) {
            return Err(CompressorError::WrongChannelCount);
        }
        if self.statistics.accepted_frames == u64::MAX {
            return Err(CompressorError::CounterExhausted);
        }
        self.preflight(stamp)?;
        let mut frame = CompressorFrame {
            valid: true,
            stamp,
            ..CompressorFrame::EMPTY
        };
        for (destination, sample) in frame.samples.iter_mut().zip(samples) {
            if sample.is_finite() {
                *destination = *sample;
            } else {
                self.statistics.sanitized_input_samples =
                    self.statistics.sanitized_input_samples.saturating_add(1);
            }
        }
        self.statistics.accepted_frames += 1;
        if stamp.is_some() {
            self.last_stamp = stamp;
        }
        Ok(self.process(frame))
    }
    /// Idempotently ends input. The fixed retained delay is the only audio tail.
    pub fn begin_eof(&mut self) {
        if !self.eof {
            self.eof = true;
            self.drain_remaining =
                if self.statistics.accepted_frames > self.statistics.valid_output_frames {
                    self.delay
                } else {
                    0
                };
        }
    }
    pub fn drained(&self) -> bool {
        self.eof && self.drain_remaining == 0
    }
    pub fn drain(&mut self) -> Result<Option<CompressorFrame>, CompressorError> {
        if !self.eof {
            return Err(CompressorError::EndNotDeclared);
        }
        if self.drain_remaining == 0 {
            return Ok(None);
        }
        self.preflight(None)?;
        let frame = self.process(CompressorFrame::EMPTY);
        self.drain_remaining -= 1;
        Ok(Some(frame))
    }
    fn update_envelope(&mut self) {
        self.detector_average = finite_or(self.detector_average, 1.0);
        self.scaled_desired_gain = self.detector_average.asin() / std::f32::consts::FRAC_PI_2;
        let releasing = self.scaled_desired_gain > self.compressor_gain;
        let difference = if self.scaled_desired_gain == 0.0 {
            if releasing { -1.0 } else { 1.0 }
        } else {
            linear_to_db(self.compressor_gain / self.scaled_desired_gain)
        };
        self.envelope_rate = if releasing {
            self.maximum_attack_diff_db = -1.0;
            let difference = finite_or(difference, -1.0);
            let x = 0.25 * (difference.clamp(-12.0, 0.0) + 12.0);
            let x2 = x * x;
            let x3 = x2 * x;
            let x4 = x2 * x2;
            let [a, b, c, d, e] = self.release_polynomial;
            db_to_linear(5.0 / (a + b * x + c * x2 + d * x3 + e * x4))
        } else {
            let difference = finite_or(difference, 1.0);
            if self.maximum_attack_diff_db == -1.0 || self.maximum_attack_diff_db < difference {
                self.maximum_attack_diff_db = difference;
            }
            1.0 - (0.25 / self.maximum_attack_diff_db.max(0.5)).powf(1.0 / self.attack_frames)
        };
    }
    fn process(&mut self, input: CompressorFrame) -> CompressorFrame {
        if self.subdivision == 0 {
            self.update_envelope();
        }
        self.slots[self.write_index] = input;
        let right = if self.channels == 1 {
            input.samples[0]
        } else {
            input.samples[1]
        };
        let amplitude = input.samples[0].abs().max(right.abs());
        let shaped = self.curve.saturate(amplitude);
        let attenuation = if amplitude <= 0.0001 {
            1.0
        } else {
            shaped / amplitude
        };
        let db_attenuation = (-linear_to_db(attenuation)).max(2.0);
        let release_rate = db_to_linear(db_attenuation / self.detector_release_frames) - 1.0;
        let rate = if attenuation > self.detector_average {
            release_rate
        } else {
            1.0
        };
        self.detector_average = finite_or(
            (self.detector_average + (attenuation - self.detector_average) * rate).min(1.0),
            1.0,
        );
        if self.envelope_rate < 1.0 {
            self.compressor_gain +=
                (self.scaled_desired_gain - self.compressor_gain) * self.envelope_rate;
        } else {
            self.compressor_gain = (self.compressor_gain * self.envelope_rate).min(1.0);
        }
        let warped = f64::from(std::f32::consts::FRAC_PI_2 * self.compressor_gain).sin() as f32;
        let gain = self.post_gain * warped;
        let gain_db = linear_to_db(warped);
        if gain_db < self.metering_gain {
            self.metering_gain = gain_db;
        } else {
            self.metering_gain += (gain_db - self.metering_gain) * self.metering_release;
        }
        let mut output = self.slots[self.read_index];
        self.slots[self.read_index] = CompressorFrame::EMPTY;
        for sample in &mut output.samples[..usize::from(self.channels.min(2))] {
            let value = *sample * gain;
            if value.is_finite() {
                *sample = value;
            } else {
                *sample = 0.0;
                self.statistics.sanitized_output_samples =
                    self.statistics.sanitized_output_samples.saturating_add(1);
            }
        }
        self.read_index = (self.read_index + 1) & (COMPRESSOR_DELAY_CAPACITY - 1);
        self.write_index = (self.write_index + 1) & (COMPRESSOR_DELAY_CAPACITY - 1);
        self.statistics.processed_frames += 1;
        self.statistics.valid_output_frames += u64::from(output.valid);
        self.subdivision = (self.subdivision + 1) & (COMPRESSOR_SUBDIVISION_FRAMES - 1);
        if self.subdivision == 0 {
            self.detector_average = flush_denormal(self.detector_average);
            self.compressor_gain = flush_denormal(self.compressor_gain);
        }
        output
    }
}

//! Prepared equal-power spatial coefficients and a fixed-storage renderer.
//!
//! Positions follow one-degree wall-time ticks. Rendering has no clock, heap
//! storage, media stamps, or presentation acknowledgement. Channels beyond the
//! first stereo pair are native dry lanes; device folding follows averaging.

/*
 * Copyright (C) 2010, Google Inc. All rights reserved.
 *
 * Redistribution and use in source and binary forms, with or without
 * modification, are permitted provided that the following conditions
 * are met:
 * 1.  Redistributions of source code must retain the above copyright
 *    notice, this list of conditions and the following disclaimer.
 * 2.  Redistributions in binary form must reproduce the above copyright
 *    notice, this list of conditions and the following disclaimer in the
 *    documentation and/or other materials provided with the distribution.
 *
 * THIS SOFTWARE IS PROVIDED BY APPLE INC. AND ITS CONTRIBUTORS ``AS IS'' AND
 * ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO, THE
 * IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE
 * ARE DISCLAIMED. IN NO EVENT SHALL APPLE INC. OR ITS CONTRIBUTORS BE LIABLE
 * FOR ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL, EXEMPLARY, OR CONSEQUENTIAL
 * DAMAGES (INCLUDING, BUT NOT LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR
 * SERVICES; LOSS OF USE, DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER
 * CAUSED AND ON ANY THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT
 * LIABILITY, OR TORT (INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY
 * OUT OF THE USE OF THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH
 * DAMAGE.
 */

// Equal-power equations: Chromium 148.0.7778.280 equal_power_panner.cc.
// Position scheduling: LX Music v2.12.6, ad95d5091c9ed689fa72b5e5c849df65f5a679ce.
// Coefficient interpolation and multichannel mapping are native policies.

use super::{pcm::MAX_CHANNELS, processing::DspReservation};
use anyhow::{Result, ensure};
use serde::{Deserialize, Deserializer, Serialize, de::Error};
use std::{f64::consts::FRAC_1_SQRT_2, mem::size_of};

pub const SPATIAL_POSITIONS: usize = 360;

#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct SpatialSettings {
    pub enabled: bool,
    pub radius: f64,
    pub speed: u8,
}
impl Default for SpatialSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            radius: 0.5,
            speed: 25,
        }
    }
}
impl SpatialSettings {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.radius.is_finite() && (0.1..=3.0).contains(&self.radius),
            "Spatial radius must be finite and between 0.1 and 3.0"
        );
        ensure!(
            (1..=50).contains(&self.speed),
            "Spatial speed must be between 1 and 50"
        );
        Ok(())
    }
    pub fn tick_interval_ns(&self) -> Result<u64> {
        self.validate()?;
        Ok(u64::from(self.speed) * 2_000_000)
    }
}
#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
struct SettingsWire {
    enabled: bool,
    radius: f64,
    speed: u8,
}
impl Default for SettingsWire {
    fn default() -> Self {
        let value = SpatialSettings::default();
        Self {
            enabled: value.enabled,
            radius: value.radius,
            speed: value.speed,
        }
    }
}
impl<'de> Deserialize<'de> for SpatialSettings {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        let value = SettingsWire::deserialize(deserializer)?;
        let settings = Self {
            enabled: value.enabled,
            radius: value.radius,
            speed: value.speed,
        };
        settings.validate().map_err(D::Error::custom)?;
        Ok(settings)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SpatialStep(u16);
impl SpatialStep {
    pub const ORIGIN: Self = Self(0);
    pub fn degree(degree: u16) -> Result<Self> {
        ensure!(
            (1..=360).contains(&degree),
            "Spatial degree must be between 1 and 360"
        );
        Ok(Self(degree))
    }
    pub fn degrees(self) -> Option<u16> {
        (self.0 != 0).then_some(self.0)
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SpatialCoefficients {
    pub mono: [f64; 2],
    pub stereo: [[f64; 2]; 2],
}
impl SpatialCoefficients {
    const BYPASS: Self = Self {
        mono: [1.0, 1.0],
        stereo: [[1.0, 0.0], [0.0, 1.0]],
    };
    const ORIGIN: Self = Self {
        mono: [FRAC_1_SQRT_2, FRAC_1_SQRT_2],
        stereo: [[1.0, 0.0], [0.0, 1.0]],
    };
    fn move_towards(&mut self, target: Self, remaining: u32) {
        if remaining == 1 {
            *self = target;
            return;
        }
        let divisor = f64::from(remaining);
        for (value, desired) in self.mono.iter_mut().zip(target.mono) {
            *value += (desired - *value) / divisor;
        }
        for (row, desired_row) in self.stereo.iter_mut().zip(target.stereo) {
            for (value, desired) in row.iter_mut().zip(desired_row) {
                *value += (desired - *value) / divisor;
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SpatialTarget {
    sequence: u64,
    sample_rate: u32,
    enabled: bool,
    step: SpatialStep,
    coefficients: SpatialCoefficients,
    transition_frames: u32,
}
impl SpatialTarget {
    pub fn sequence(self) -> u64 {
        self.sequence
    }
    pub fn sample_rate(self) -> u32 {
        self.sample_rate
    }
    pub fn enabled(self) -> bool {
        self.enabled
    }
    pub fn step(self) -> SpatialStep {
        self.step
    }
    pub fn coefficients(self) -> SpatialCoefficients {
        self.coefficients
    }
    pub fn transition_frames(self) -> u32 {
        self.transition_frames
    }
    /// Geometry is unchanged; only the prepared output-clock interpolation changes.
    pub(crate) fn for_sample_rate(self, sample_rate: u32) -> Result<Self> {
        validate_rate(sample_rate)?;
        Ok(Self {
            sample_rate,
            transition_frames: sample_rate.div_ceil(50),
            ..self
        })
    }
}

/// Owns one complete table and its processwide DSP reservation.
pub struct SpatialPlan {
    settings: SpatialSettings,
    sample_rate: u32,
    sequence: u64,
    positions: [SpatialCoefficients; SPATIAL_POSITIONS],
    _reservation: DspReservation,
}
impl SpatialPlan {
    pub fn prepare(settings: SpatialSettings, sample_rate: u32, sequence: u64) -> Result<Self> {
        settings.validate()?;
        validate_rate(sample_rate)?;
        let reservation = DspReservation::new(size_of::<Self>())?;
        let positions = std::array::from_fn(|index| prepare_position(settings.radius, index + 1));
        ensure!(
            positions.iter().all(|coefficients| {
                coefficients
                    .mono
                    .iter()
                    .chain(coefficients.stereo.iter().flatten())
                    .all(|value| value.is_finite() && (0.0..=1.0).contains(value))
            }),
            "Spatial coefficients must be finite and bounded"
        );
        Ok(Self {
            settings,
            sample_rate,
            sequence,
            positions,
            _reservation: reservation,
        })
    }
    pub fn settings(&self) -> SpatialSettings {
        self.settings
    }
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }
    pub fn sequence(&self) -> u64 {
        self.sequence
    }
    pub fn owned_bytes(&self) -> usize {
        size_of::<Self>()
    }
    /// Returns active spatial coefficients independently of the enable switch.
    pub fn coefficients(&self, step: SpatialStep) -> SpatialCoefficients {
        if step == SpatialStep::ORIGIN {
            SpatialCoefficients::ORIGIN
        } else {
            self.positions[usize::from(step.0) - 1]
        }
    }
    pub fn target(&self, step: SpatialStep) -> SpatialTarget {
        SpatialTarget {
            sequence: self.sequence,
            sample_rate: self.sample_rate,
            enabled: self.settings.enabled,
            step: if self.settings.enabled {
                step
            } else {
                SpatialStep::ORIGIN
            },
            coefficients: if self.settings.enabled {
                self.coefficients(step)
            } else {
                SpatialCoefficients::BYPASS
            },
            transition_frames: self.sample_rate.div_ceil(50),
        }
    }
}
fn validate_rate(sample_rate: u32) -> Result<()> {
    ensure!(
        (8_000..=384_000).contains(&sample_rate),
        "Spatial sample rate must be between 8000 and 384000"
    );
    Ok(())
}
fn prepare_position(radius: f64, degree: usize) -> SpatialCoefficients {
    let (sine, cosine) = match degree % 360 {
        0 => (0.0, 1.0),
        90 => (1.0, 0.0),
        180 => (0.0, -1.0),
        270 => (-1.0, 0.0),
        _ => (degree as f64).to_radians().sin_cos(),
    };
    let mut azimuth = sine.atan2(-cosine).to_degrees();
    if azimuth < -90.0 {
        azimuth = -180.0 - azimuth;
    } else if azimuth > 90.0 {
        azimuth = 180.0 - azimuth;
    }
    let distance = radius * (sine * sine + 2.0 * cosine * cosine).sqrt();
    let attenuation = if distance <= 1.0 { 1.0 } else { 1.0 / distance };
    let mono = equal_power((azimuth + 90.0) / 180.0);
    let stereo = if azimuth == 0.0 {
        [[1.0, 0.0], [0.0, 1.0]]
    } else if azimuth < 0.0 {
        let [left, right] = equal_power((azimuth + 90.0) / 90.0);
        [[1.0, left], [0.0, right]]
    } else {
        let [left, right] = equal_power(azimuth / 90.0);
        [[left, 0.0], [right, 1.0]]
    };
    SpatialCoefficients {
        mono: mono.map(|value| value * attenuation),
        stereo: stereo.map(|row| row.map(|value| value * attenuation)),
    }
}
fn equal_power(position: f64) -> [f64; 2] {
    if position == 0.0 {
        [1.0, 0.0]
    } else if position == 1.0 {
        [0.0, 1.0]
    } else if position == 0.5 {
        [FRAC_1_SQRT_2, FRAC_1_SQRT_2]
    } else {
        let (sine, cosine) = (position * std::f64::consts::FRAC_PI_2).sin_cos();
        [cosine, sine]
    }
}

/// Pure control-thread clock. `now_ns` belongs to one monotonic owner timeline.
pub struct SpatialPhaseClock {
    settings: SpatialSettings,
    sequence: u64,
    sample_rate: u32,
    anchor_ns: u64,
    last_ns: u64,
    last_tick: u64,
    last_target: SpatialTarget,
}
impl SpatialPhaseClock {
    pub fn new(plan: &SpatialPlan, now_ns: u64) -> Self {
        Self {
            settings: plan.settings,
            sequence: plan.sequence,
            sample_rate: plan.sample_rate,
            anchor_ns: now_ns,
            last_ns: now_ns,
            last_tick: 0,
            last_target: plan.target(SpatialStep::ORIGIN),
        }
    }
    pub fn reconfigure(&mut self, plan: &SpatialPlan, now_ns: u64) -> Result<SpatialTarget> {
        ensure!(
            now_ns >= self.last_ns,
            "Spatial clock cannot move backwards"
        );
        let was_enabled = self.settings.enabled;
        let speed_changed = self.settings.speed != plan.settings.speed;
        let radius_changed = self.settings.radius != plan.settings.radius;
        self.last_ns = now_ns;
        self.settings = plan.settings;
        self.sequence = plan.sequence;
        self.sample_rate = plan.sample_rate;
        self.last_target.sample_rate = plan.sample_rate;
        self.last_target.transition_frames = plan.sample_rate.div_ceil(50);
        if !plan.settings.enabled || !was_enabled {
            self.anchor_ns = now_ns;
            self.last_tick = 0;
            self.last_target = plan.target(SpatialStep::ORIGIN);
        } else if speed_changed {
            self.anchor_ns = now_ns;
            self.last_tick = 0;
        } else if radius_changed {
            self.last_tick = (now_ns - self.anchor_ns) / self.interval_ns();
        } else {
            self.last_target.sequence = plan.sequence;
        }
        Ok(self.last_target)
    }
    pub fn sample(&mut self, plan: &SpatialPlan, now_ns: u64) -> Result<SpatialTarget> {
        ensure!(
            plan.sequence == self.sequence
                && plan.settings == self.settings
                && plan.sample_rate == self.sample_rate,
            "Spatial clock requires its configured plan"
        );
        ensure!(
            now_ns >= self.last_ns,
            "Spatial clock cannot move backwards"
        );
        self.last_ns = now_ns;
        if self.settings.enabled {
            let tick = (now_ns - self.anchor_ns) / self.interval_ns();
            if tick > self.last_tick {
                self.last_tick = tick;
                let degree = ((tick - 1) % 360 + 1) as u16;
                self.last_target = plan.target(SpatialStep(degree));
            }
        }
        Ok(self.last_target)
    }
    pub fn next_tick_ns(&self) -> Option<u64> {
        if !self.settings.enabled {
            return None;
        }
        self.last_tick
            .checked_add(1)?
            .checked_mul(self.interval_ns())?
            .checked_add(self.anchor_ns)
    }
    fn interval_ns(&self) -> u64 {
        u64::from(self.settings.speed) * 2_000_000
    }
}

/// Two small coefficient states; all storage is prepared before rendering.
pub struct SpatialRenderer {
    sample_rate: u32,
    input_channels: usize,
    output_channels: usize,
    current: SpatialCoefficients,
    mono_extra_gain: f64,
    target: SpatialTarget,
    remaining: u32,
    _reservation: DspReservation,
}
impl SpatialRenderer {
    pub fn new(sample_rate: u32, input_channels: usize, output_channels: usize) -> Result<Self> {
        validate_rate(sample_rate)?;
        ensure!(
            (1..=MAX_CHANNELS).contains(&input_channels)
                && (1..=MAX_CHANNELS).contains(&output_channels),
            "Spatial channels must be between 1 and 8"
        );
        let reservation = DspReservation::new(size_of::<Self>())?;
        Ok(Self {
            sample_rate,
            input_channels,
            output_channels,
            current: SpatialCoefficients::BYPASS,
            mono_extra_gain: 1.0,
            target: SpatialTarget {
                sequence: 0,
                sample_rate,
                enabled: false,
                step: SpatialStep::ORIGIN,
                coefficients: SpatialCoefficients::BYPASS,
                transition_frames: sample_rate.div_ceil(50),
            },
            remaining: 0,
            _reservation: reservation,
        })
    }
    pub fn owned_bytes(&self) -> usize {
        size_of::<Self>()
    }
    pub fn target(&self) -> SpatialTarget {
        self.target
    }
    pub fn transition_remaining_frames(&self) -> u32 {
        self.remaining
    }
    pub fn latency_frames(&self) -> u32 {
        0
    }
    /// Rejects a format mismatch before changing coefficients or transition.
    pub fn retarget(&mut self, target: SpatialTarget) -> bool {
        if target.sample_rate != self.sample_rate {
            return false;
        }
        if self.target.coefficients == target.coefficients && self.target.enabled == target.enabled
        {
            self.target = target;
            return true;
        }
        self.target = target;
        let desired_extra = if target.enabled { 0.0 } else { 1.0 };
        self.remaining =
            if self.current == target.coefficients && self.mono_extra_gain == desired_extra {
                0
            } else {
                target.transition_frames
            };
        true
    }
    /// Clears interpolation without changing the control-thread phase clock.
    pub fn reset_to_target(&mut self) {
        self.current = self.target.coefficients;
        self.mono_extra_gain = if self.target.enabled { 0.0 } else { 1.0 };
        self.remaining = 0;
    }
    pub fn frame(&mut self, mut input: [f64; MAX_CHANNELS]) -> ([f64; MAX_CHANNELS], u64) {
        if self.remaining != 0 {
            self.current
                .move_towards(self.target.coefficients, self.remaining);
            let desired = if self.target.enabled { 0.0 } else { 1.0 };
            self.mono_extra_gain = if self.remaining == 1 {
                desired
            } else {
                self.mono_extra_gain + (desired - self.mono_extra_gain) / f64::from(self.remaining)
            };
            self.remaining -= 1;
        }
        let mut errors = 0;
        for value in input.iter_mut().take(self.input_channels) {
            if !value.is_finite() {
                *value = 0.0;
                errors += 1;
            }
        }
        let mut processed = [0.0; MAX_CHANNELS];
        let processed_channels = if self.input_channels == 1 {
            processed[0] = input[0] * self.current.mono[0];
            processed[1] = input[0] * self.current.mono[1];
            2
        } else {
            processed[..self.input_channels].copy_from_slice(&input[..self.input_channels]);
            processed[0] = dot(self.current.stereo[0], input[0], input[1]);
            processed[1] = dot(self.current.stereo[1], input[0], input[1]);
            self.input_channels
        };
        for value in processed.iter_mut().take(processed_channels) {
            if !value.is_finite() {
                *value = 0.0;
                errors += 1;
            }
        }
        let mut output = fold_channels(processed, processed_channels, self.output_channels);
        if self.input_channels == 1 && self.output_channels > 2 {
            output[2..self.output_channels].fill(input[0] * self.mono_extra_gain);
        }
        for value in output.iter_mut().take(self.output_channels) {
            if !value.is_finite() {
                *value = 0.0;
                errors += 1;
            }
        }
        (output, errors)
    }
}
fn dot(coefficients: [f64; 2], left: f64, right: f64) -> f64 {
    if coefficients[1] == 0.0 {
        coefficients[0] * left
    } else if coefficients[0] == 0.0 {
        coefficients[1] * right
    } else {
        coefficients[0] * left + coefficients[1] * right
    }
}
fn fold_channels(input: [f64; MAX_CHANNELS], from: usize, to: usize) -> [f64; MAX_CHANNELS] {
    let mut output = [0.0; MAX_CHANNELS];
    if from <= to {
        output[..from].copy_from_slice(&input[..from]);
    } else {
        let mut counts = [0usize; MAX_CHANNELS];
        for index in 0..from {
            counts[index % to] += 1;
        }
        for index in 0..from {
            output[index % to] += input[index] / counts[index % to] as f64;
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_and_renderer_return_actual_reservations_on_drop() {
        let initial = DspReservation::used_bytes();
        let plan = SpatialPlan::prepare(SpatialSettings::default(), 48_000, 1).unwrap();
        let renderer = SpatialRenderer::new(48_000, 2, 2).unwrap();
        let charged = plan.owned_bytes() + renderer.owned_bytes();
        assert_eq!(DspReservation::used_bytes(), initial + charged);
        drop(renderer);
        drop(plan);
        assert_eq!(DspReservation::used_bytes(), initial);
    }
}

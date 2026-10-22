//! Fixed-storage ten-band peaking equalizer with independently prepared plans.

use super::pcm::MAX_CHANNELS;
use crate::domain::equalizer::{
    EQUALIZER_BAND_COUNT, EQUALIZER_FREQUENCIES_HZ, EQUALIZER_Q, EqualizerSettings,
};
use anyhow::{Result, ensure};

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Coefficients {
    pub values: [f64; 5],
}

impl Coefficients {
    const UNITY: Self = Self {
        values: [1.0, 0.0, 0.0, 0.0, 0.0],
    };

    fn prepare(frequency: u32, gain: f64, sample_rate: u32) -> Self {
        if gain == 0.0 || frequency as u64 * 2 >= sample_rate as u64 {
            return Self::UNITY;
        }
        let a = 10.0f64.powf(gain / 40.0);
        let omega = std::f64::consts::TAU * frequency as f64 / sample_rate as f64;
        let alpha = omega.sin() / (2.0 * EQUALIZER_Q);
        let a0 = 1.0 + alpha / a;
        Self {
            values: [
                (1.0 + alpha * a) / a0,
                -2.0 * omega.cos() / a0,
                (1.0 - alpha * a) / a0,
                -2.0 * omega.cos() / a0,
                (1.0 - alpha / a) / a0,
            ],
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct EqualizerPlan {
    pub sequence: u64,
    pub sample_rate: u32,
    pub settings: EqualizerSettings,
    pub sections: [Coefficients; EQUALIZER_BAND_COUNT],
    pub active_bands: [bool; EQUALIZER_BAND_COUNT],
}

impl EqualizerPlan {
    pub(crate) fn prepare(
        settings: EqualizerSettings,
        sample_rate: u32,
        sequence: u64,
    ) -> Result<Self> {
        settings.validate()?;
        ensure!(sample_rate > 0, "Equalizer sample rate must be positive");
        let sections = std::array::from_fn(|band| {
            Coefficients::prepare(
                EQUALIZER_FREQUENCIES_HZ[band],
                settings.gains_db[band],
                sample_rate,
            )
        });
        for section in &sections {
            let [_, _, _, a1, a2] = section.values;
            ensure!(
                section.values.iter().all(|value| value.is_finite())
                    && 1.0 + a1 + a2 > 0.0
                    && 1.0 - a1 + a2 > 0.0
                    && 1.0 - a2 > 0.0,
                "Equalizer coefficients are not finite and stable at this output rate"
            );
        }
        let active_bands = std::array::from_fn(|band| sections[band] != Coefficients::UNITY);
        Ok(Self {
            sequence,
            sample_rate,
            settings,
            sections,
            active_bands,
        })
    }

    pub(crate) fn flat(sample_rate: u32) -> Self {
        Self {
            sequence: 0,
            sample_rate,
            settings: EqualizerSettings::default(),
            sections: [Coefficients::UNITY; EQUALIZER_BAND_COUNT],
            active_bands: [false; EQUALIZER_BAND_COUNT],
        }
    }
}

struct Bank {
    plan: EqualizerPlan,
    delays: [[[f64; 2]; EQUALIZER_BAND_COUNT]; MAX_CHANNELS],
}

impl Bank {
    fn new(plan: EqualizerPlan) -> Self {
        Self {
            plan,
            delays: [[[0.0; 2]; EQUALIZER_BAND_COUNT]; MAX_CHANNELS],
        }
    }
    fn reset(&mut self) {
        self.delays = [[[0.0; 2]; EQUALIZER_BAND_COUNT]; MAX_CHANNELS];
    }
    fn sample(&mut self, channel: usize, mut value: f64) -> f64 {
        for (band, coefficients) in self.plan.sections.iter().enumerate() {
            if !self.plan.active_bands[band] {
                continue;
            }
            let [b0, b1, b2, a1, a2] = coefficients.values;
            let delay = &mut self.delays[channel][band];
            let output = b0 * value + delay[0];
            delay[0] = b1 * value - a1 * output + delay[1];
            delay[1] = b2 * value - a2 * output;
            // Only subnormal binary64 history is flushed; ordinary impulse tails
            // retain their numerical value through the measured decay range.
            for state in delay {
                if state.abs() < 1e-300 {
                    *state = 0.0;
                }
            }
            value = output;
        }
        value
    }
}

pub(crate) struct Equalizer {
    banks: [Bank; 2],
    active: usize,
    target: EqualizerPlan,
    pending: Option<EqualizerPlan>,
    remaining: u32,
    total: u32,
}

impl Equalizer {
    pub(crate) fn new(plan: EqualizerPlan) -> Self {
        Self {
            banks: [Bank::new(plan), Bank::new(plan)],
            active: 0,
            target: plan,
            pending: None,
            remaining: 0,
            total: 0,
        }
    }
    pub(crate) fn plan(&self) -> EqualizerPlan {
        self.target
    }
    pub(crate) fn remaining(&self) -> u32 {
        self.remaining
    }

    pub(crate) fn retarget(&mut self, plan: EqualizerPlan) {
        self.banks[0].plan = plan;
        self.banks[1].plan = plan;
        self.target = plan;
        self.pending = None;
        self.active = 0;
        self.remaining = 0;
        self.total = 0;
        self.banks[0].reset();
        self.banks[1].reset();
    }

    pub(crate) fn reset(&mut self) {
        let plan = self.pending.take().unwrap_or(self.target);
        self.retarget(plan);
    }

    pub(crate) fn request(&mut self, plan: EqualizerPlan) {
        if plan.sample_rate != self.target.sample_rate || plan.sequence <= self.target.sequence {
            return;
        }
        if self.remaining != 0 {
            if self
                .pending
                .is_none_or(|pending| pending.sequence < plan.sequence)
            {
                self.pending = Some(plan);
            }
        } else if self.banks[self.active].plan.sections == plan.sections {
            self.banks[0].plan = plan;
            self.banks[1].plan = plan;
            self.target = plan;
        } else {
            self.start(plan);
        }
    }

    fn start(&mut self, plan: EqualizerPlan) {
        let next = 1 - self.active;
        self.banks[next].plan = plan;
        self.banks[next].reset();
        self.target = plan;
        self.total = (plan.sample_rate as u64).div_ceil(50).max(1) as u32;
        self.remaining = self.total;
    }

    /// Processes one original frame in both banks during transition. It never
    /// reads the PCM source and cannot consume a second copy of a media frame.
    pub(crate) fn frame(
        &mut self,
        input: [f64; MAX_CHANNELS],
        channels: usize,
    ) -> ([f64; MAX_CHANNELS], u64) {
        let mut result = [0.0; MAX_CHANNELS];
        let mut non_finite = 0;
        let transition = self.remaining != 0;
        let weight = if transition {
            (self.total - self.remaining + 1) as f64 / self.total as f64
        } else {
            0.0
        };
        for channel in 0..channels {
            let old = self.banks[self.active].sample(channel, input[channel]);
            let value = if transition {
                let new = self.banks[1 - self.active].sample(channel, input[channel]);
                old + (new - old) * weight
            } else {
                old
            };
            if value.is_finite() {
                result[channel] = value;
            } else {
                non_finite += 1;
                self.banks[0].delays[channel] = [[0.0; 2]; EQUALIZER_BAND_COUNT];
                self.banks[1].delays[channel] = [[0.0; 2]; EQUALIZER_BAND_COUNT];
            }
        }
        if transition {
            self.remaining -= 1;
            if self.remaining == 0 {
                self.active = 1 - self.active;
                if let Some(plan) = self.pending.take() {
                    self.start(plan);
                }
            }
        }
        (result, non_finite)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    fn reference() -> Value {
        serde_json::from_str(include_str!(
            "../../tests/fixtures/equalizer_reference.json"
        ))
        .unwrap()
    }
    fn close(actual: f64, expected: f64, absolute: f64, relative: f64) {
        assert!(
            (actual - expected).abs() <= absolute + relative * expected.abs(),
            "{actual} != {expected}"
        );
    }
    fn settings(case: &Value) -> EqualizerSettings {
        EqualizerSettings {
            gains_db: std::array::from_fn(|band| case["gains_db"][band].as_f64().unwrap()),
        }
    }
    fn coefficients(section: &Value) -> [f64; 5] {
        ["b0", "b1", "b2", "a1", "a2"].map(|key| section["coefficients"][key].as_f64().unwrap())
    }

    #[test]
    fn all_150_single_section_coefficients_stability_and_impulses_match_independent_reference() {
        let reference = reference();
        let cases = reference["single_section_cases"].as_array().unwrap();
        assert_eq!(cases.len(), 150);
        for case in cases {
            let rate = case["sample_rate_hz"].as_u64().unwrap() as u32;
            let frequency = case["requested_frequency_hz"].as_u64().unwrap() as u32;
            let band = EQUALIZER_FREQUENCIES_HZ
                .iter()
                .position(|&value| value == frequency)
                .unwrap();
            let mut settings = EqualizerSettings::default();
            settings.gains_db[band] = case["gain_db"].as_f64().unwrap();
            let plan = EqualizerPlan::prepare(settings, rate, 1).unwrap();
            for (actual, expected) in plan.sections[band]
                .values
                .into_iter()
                .zip(coefficients(case))
            {
                close(actual, expected, 1e-12, 1e-12);
            }
            let [_, _, _, a1, a2] = plan.sections[band].values;
            assert!(1.0 + a1 + a2 > 0.0 && 1.0 - a1 + a2 > 0.0 && 1.0 - a2 > 0.0);
            if plan.active_bands[band] {
                let discriminant = a1 * a1 - 4.0 * a2;
                let radius = if discriminant < 0.0 {
                    a2.sqrt()
                } else {
                    ((-a1 + discriminant.sqrt()) / 2.0)
                        .abs()
                        .max(((-a1 - discriminant.sqrt()) / 2.0).abs())
                };
                assert!(radius < 1.0);
            }
            let mut equalizer = Equalizer::new(plan);
            for (frame, expected) in case["unit_impulse_first_64"]
                .as_array()
                .unwrap()
                .iter()
                .enumerate()
            {
                let mut input = [0.0; MAX_CHANNELS];
                input[0] = if frame == 0 { 1.0 } else { 0.0 };
                close(
                    equalizer.frame(input, 1).0[0],
                    expected.as_f64().unwrap(),
                    1e-10,
                    1e-9,
                );
            }
        }
    }

    #[test]
    fn all_27_preset_and_nine_stress_cascades_match_independent_impulses() {
        let reference = reference();
        let presets = reference["preset_cascade_cases"].as_array().unwrap();
        let stress = reference["stress_cascade_cases"].as_array().unwrap();
        assert_eq!(presets.len(), 27);
        assert_eq!(stress.len(), 9);
        for case in presets.iter().chain(stress) {
            let rate = case["sample_rate_hz"].as_u64().unwrap() as u32;
            let plan = EqualizerPlan::prepare(settings(case), rate, 1).unwrap();
            for (band, section) in case["sections"].as_array().unwrap().iter().enumerate() {
                assert_eq!(
                    plan.active_bands[band],
                    case["active_band_mask"][band].as_bool().unwrap()
                );
                for (actual, expected) in plan.sections[band]
                    .values
                    .into_iter()
                    .zip(coefficients(section))
                {
                    close(actual, expected, 1e-12, 1e-12);
                }
            }
            let mut equalizer = Equalizer::new(plan);
            for (frame, expected) in case["unit_impulse_first_128"]
                .as_array()
                .unwrap()
                .iter()
                .enumerate()
            {
                let mut input = [0.0; MAX_CHANNELS];
                input[0] = if frame == 0 { 1.0 } else { 0.0 };
                close(
                    equalizer.frame(input, 1).0[0],
                    expected.as_f64().unwrap(),
                    1e-10,
                    1e-9,
                );
            }
        }
    }

    #[test]
    fn flat_is_exact_channels_are_isolated_and_rapid_updates_finish_in_order() {
        let flat = EqualizerPlan::flat(48_000);
        let mut equalizer = Equalizer::new(flat);
        let values = [0.1f32, -0.2, 0.0, -0.0, 0.9, -0.7, f32::MIN_POSITIVE, 0.5];
        let input = values.map(f64::from);
        assert_eq!(
            equalizer.frame(input, 8).0.map(|value| value as f32),
            values
        );
        let mut settings = EqualizerSettings::default();
        settings.gains_db[5] = 6.0;
        equalizer.request(EqualizerPlan::prepare(settings, 48_000, 1).unwrap());
        settings.gains_db[5] = -6.0;
        equalizer.request(EqualizerPlan::prepare(settings, 48_000, 2).unwrap());
        settings.gains_db[5] = 3.0;
        equalizer.request(EqualizerPlan::prepare(settings, 48_000, 3).unwrap());
        for _ in 0..960 {
            let mut input = [0.0; MAX_CHANNELS];
            input[1] = 0.01;
            let result = equalizer.frame(input, 8).0;
            assert!(
                result
                    .iter()
                    .enumerate()
                    .all(|(channel, &sample)| channel == 1 || sample == 0.0)
            );
        }
        assert_eq!(equalizer.plan().sequence, 3);
        assert_eq!(equalizer.remaining(), 960);
        for _ in 0..960 {
            equalizer.frame([0.0; MAX_CHANNELS], 8);
        }
        assert_eq!(equalizer.remaining(), 0);
        equalizer.reset();
        assert_eq!(
            equalizer.frame([0.0; MAX_CHANNELS], 8).0,
            [0.0; MAX_CHANNELS]
        );
    }

    fn multiply(left: (f64, f64), right: (f64, f64)) -> (f64, f64) {
        (
            left.0 * right.0 - left.1 * right.1,
            left.0 * right.1 + left.1 * right.0,
        )
    }
    fn response(plan: EqualizerPlan, frequency: f64) -> (f64, f64) {
        let angle = -std::f64::consts::TAU * frequency / plan.sample_rate as f64;
        let z = (angle.cos(), angle.sin());
        let z2 = multiply(z, z);
        plan.sections.iter().fold((1.0, 0.0), |result, section| {
            let [b0, b1, b2, a1, a2] = section.values;
            let numerator = (b0 + b1 * z.0 + b2 * z2.0, b1 * z.1 + b2 * z2.1);
            let denominator = (1.0 + a1 * z.0 + a2 * z2.0, a1 * z.1 + a2 * z2.1);
            let norm = denominator.0 * denominator.0 + denominator.1 * denominator.1;
            multiply(
                result,
                (
                    (numerator.0 * denominator.0 + numerator.1 * denominator.1) / norm,
                    (numerator.1 * denominator.0 - numerator.0 * denominator.1) / norm,
                ),
            )
        })
    }

    #[test]
    fn all_reference_center_off_center_dc_and_nyquist_responses_match_prepared_transfer() {
        let reference = reference();
        for (group, single) in [
            ("single_section_cases", true),
            ("preset_cascade_cases", false),
            ("stress_cascade_cases", false),
        ] {
            for case in reference[group].as_array().unwrap() {
                let rate = case["sample_rate_hz"].as_u64().unwrap() as u32;
                let settings = if single {
                    let frequency = case["requested_frequency_hz"].as_u64().unwrap() as u32;
                    let band = EQUALIZER_FREQUENCIES_HZ
                        .iter()
                        .position(|&value| value == frequency)
                        .unwrap();
                    let mut settings = EqualizerSettings::default();
                    settings.gains_db[band] = case["gain_db"].as_f64().unwrap();
                    settings
                } else {
                    settings(case)
                };
                let plan = EqualizerPlan::prepare(settings, rate, 1).unwrap();
                for probe in case["responses"].as_array().unwrap() {
                    let actual = response(plan, probe["frequency_hz"].as_f64().unwrap());
                    let magnitude = probe["magnitude"].as_f64().unwrap();
                    let phase = probe["phase_radians"].as_f64().unwrap();
                    // Single sections store Cartesian values. Cascade references
                    // store the same independent transfer in polar coordinates.
                    let real = probe["real"]
                        .as_f64()
                        .unwrap_or_else(|| magnitude * phase.cos());
                    let imaginary = probe["imag"]
                        .as_f64()
                        .unwrap_or_else(|| magnitude * phase.sin());
                    close(actual.0, real, 2e-10, 2e-10);
                    close(actual.1, imaginary, 2e-10, 2e-10);
                }
            }
        }
    }

    #[test]
    fn stress_impulse_tail_decays_and_non_finite_channel_is_reset_without_crossfeed() {
        let plan = EqualizerPlan::prepare(
            EqualizerSettings {
                gains_db: [15.0; 10],
            },
            48_000,
            1,
        )
        .unwrap();
        let mut equalizer = Equalizer::new(plan);
        let mut late_peak = 0.0f64;
        for frame in 0..240_000 {
            let mut input = [0.0; MAX_CHANNELS];
            if frame == 0 {
                input[0] = 1e-8;
            }
            let output = equalizer.frame(input, 1).0[0];
            assert!(output.is_finite());
            if frame >= 192_000 {
                late_peak = late_peak.max(output.abs());
            }
        }
        assert!(late_peak < 1e-12, "unstable or persistent tail {late_peak}");
        let mut input = [0.0; MAX_CHANNELS];
        input[1] = f64::NAN;
        input[2] = 0.001;
        let (result, non_finite) = equalizer.frame(input, 8);
        assert_eq!(non_finite, 1);
        assert_eq!(result[1], 0.0);
        assert_ne!(result[2], 0.0);
        let result = equalizer.frame([0.0; MAX_CHANNELS], 8).0;
        assert_eq!(result[1], 0.0);
        assert_eq!(result[3], 0.0);
    }
}

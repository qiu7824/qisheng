//! Ten-band peaking equalizer settings and acknowledged rendering diagnostics.

use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

pub const EQUALIZER_BAND_COUNT: usize = 10;
pub const EQUALIZER_FREQUENCIES_HZ: [u32; EQUALIZER_BAND_COUNT] =
    [31, 62, 125, 250, 500, 1000, 2000, 4000, 8000, 16000];
pub const EQUALIZER_Q: f64 = 1.4;
pub const EQUALIZER_MIN_GAIN_DB: f64 = -15.0;
pub const EQUALIZER_MAX_GAIN_DB: f64 = 15.0;

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EqualizerSettings {
    pub gains_db: [f64; EQUALIZER_BAND_COUNT],
}

impl EqualizerSettings {
    pub fn validate(&self) -> Result<()> {
        for (frequency, gain) in EQUALIZER_FREQUENCIES_HZ.iter().zip(self.gains_db) {
            ensure!(
                gain.is_finite() && (EQUALIZER_MIN_GAIN_DB..=EQUALIZER_MAX_GAIN_DB).contains(&gain),
                "Equalizer band {frequency} Hz gain must be finite and between -15 and 15 dB"
            );
        }
        Ok(())
    }

    pub fn is_flat(&self) -> bool {
        self.gains_db.iter().all(|gain| *gain == 0.0)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EqualizerPreset {
    #[default]
    Flat,
    Pop,
    Dance,
    Rock,
    Classical,
    Vocal,
    Slow,
    Electronic,
    Subwoofer,
    Soft,
}

impl EqualizerPreset {
    pub const ALL: [Self; 10] = [
        Self::Flat,
        Self::Pop,
        Self::Dance,
        Self::Rock,
        Self::Classical,
        Self::Vocal,
        Self::Slow,
        Self::Electronic,
        Self::Subwoofer,
        Self::Soft,
    ];

    pub const fn name(self) -> &'static str {
        match self {
            Self::Flat => "flat",
            Self::Pop => "pop",
            Self::Dance => "dance",
            Self::Rock => "rock",
            Self::Classical => "classical",
            Self::Vocal => "vocal",
            Self::Slow => "slow",
            Self::Electronic => "electronic",
            Self::Subwoofer => "subwoofer",
            Self::Soft => "soft",
        }
    }

    pub const fn settings(self) -> EqualizerSettings {
        let gains_db = match self {
            Self::Flat => [0.0; EQUALIZER_BAND_COUNT],
            Self::Pop => [6.0, 5.0, -3.0, -2.0, 5.0, 4.0, -4.0, -3.0, 6.0, 4.0],
            Self::Dance => [4.0, 3.0, -4.0, -6.0, 0.0, 0.0, 3.0, 4.0, 4.0, 5.0],
            Self::Rock => [7.0, 6.0, 2.0, 1.0, -3.0, -4.0, 2.0, 1.0, 4.0, 5.0],
            Self::Classical => [6.0, 7.0, 1.0, 2.0, -1.0, 1.0, -4.0, -6.0, -7.0, -8.0],
            Self::Vocal => [-5.0, -6.0, -4.0, -3.0, 3.0, 4.0, 5.0, 4.0, -3.0, -3.0],
            Self::Slow => [5.0, 4.0, 2.0, 0.0, -2.0, 0.0, 3.0, 6.0, 7.0, 8.0],
            Self::Electronic => [6.0, 5.0, 0.0, -5.0, -4.0, 0.0, 6.0, 8.0, 8.0, 7.0],
            Self::Subwoofer => [8.0, 7.0, 5.0, 4.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
            Self::Soft => [-5.0, -5.0, -4.0, -4.0, 3.0, 2.0, 4.0, 4.0, 0.0, 0.0],
        };
        EqualizerSettings { gains_db }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EqualizerPresetInfo {
    pub preset: EqualizerPreset,
    pub name: String,
    pub settings: EqualizerSettings,
}

pub fn equalizer_presets() -> Vec<EqualizerPresetInfo> {
    EqualizerPreset::ALL
        .into_iter()
        .map(|preset| EqualizerPresetInfo {
            preset,
            name: preset.name().to_owned(),
            settings: preset.settings(),
        })
        .collect()
}

/// Desired settings are durable intent. Applied settings and sequence come from
/// the renderer's acknowledgement, rather than a settings commit receipt.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EqualizerSnapshot {
    pub desired: EqualizerSettings,
    pub applied: EqualizerSettings,
    pub requested_sequence: u64,
    pub applied_sequence: u64,
    pub sample_rate: Option<u32>,
    pub active_bands: [bool; EQUALIZER_BAND_COUNT],
    pub transition_remaining_frames: u32,
    pub clipped_samples: u64,
    pub non_finite_samples: u64,
    pub peak_before_clamp: f64,
}

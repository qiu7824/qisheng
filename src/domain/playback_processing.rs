//! Playback tempo, pitch preservation and independent pitch settings.

use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

pub const MIN_PLAYBACK_RATE: f64 = 0.5;
pub const MAX_PLAYBACK_RATE: f64 = 2.0;
pub const MIN_PITCH_RATIO: f64 = 0.5;
pub const MAX_PITCH_RATIO: f64 = 1.5;

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlaybackProcessingSettings {
    pub playback_rate: f64,
    pub preserves_pitch: bool,
    /// Independent pitch factor; changing this value does not change duration.
    pub pitch_ratio: f64,
}

impl Default for PlaybackProcessingSettings {
    fn default() -> Self {
        Self {
            playback_rate: 1.0,
            preserves_pitch: true,
            pitch_ratio: 1.0,
        }
    }
}

impl PlaybackProcessingSettings {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.playback_rate.is_finite()
                && (MIN_PLAYBACK_RATE..=MAX_PLAYBACK_RATE).contains(&self.playback_rate),
            "Playback rate must be finite and between 0.5 and 2.0"
        );
        ensure!(
            self.pitch_ratio.is_finite()
                && (MIN_PITCH_RATIO..=MAX_PITCH_RATIO).contains(&self.pitch_ratio),
            "Pitch ratio must be finite and between 0.5 and 1.5"
        );
        Ok(())
    }
}

/// Applied settings are acknowledged by consumed processed PCM. A durable
/// settings commit or decoder-ready receipt does not advance that sequence.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlaybackProcessingSnapshot {
    pub desired: PlaybackProcessingSettings,
    pub applied: PlaybackProcessingSettings,
    pub requested_sequence: u64,
    pub applied_sequence: u64,
    pub latency_frames: u32,
    pub transition_remaining_frames: u32,
    pub sample_rate: Option<u32>,
}

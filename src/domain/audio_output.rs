//! Bounded audio device identities, persistent preferences and output diagnostics.

use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeSet, fmt, str::FromStr};

pub const MAX_AUDIO_DEVICE_ID_BYTES: usize = 1024;
pub const MAX_AUDIO_DEVICE_NAME_BYTES: usize = 512;
pub const MAX_AUDIO_DEVICES: usize = 128;

/// The system default is a policy. An explicit device is identified by its
/// stable CPAL device ID, never by a display name or enumeration position.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum OutputSelection {
    #[default]
    Default,
    Device {
        id: String,
    },
}

impl OutputSelection {
    pub fn validate(&self) -> Result<()> {
        if let Self::Device { id } = self {
            validate_device_id(id)?;
            ensure!(
                id.split_once(':')
                    .is_some_and(|(_, device)| !device.is_empty())
                    && cpal::DeviceId::from_str(id).is_ok(),
                "Audio device selection requires a supported CPAL host and a nonempty device identity"
            );
        }
        Ok(())
    }
}

/// Mute is separate from the saved volume so unmuting restores the user's gain.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AudioPreferences {
    pub selection: OutputSelection,
    pub muted: bool,
    pub pause_on_device_change: bool,
}

impl AudioPreferences {
    pub fn validate(&self) -> Result<()> {
        self.selection.validate()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AudioDeviceInfo {
    pub id: String,
    pub name: String,
    pub is_default: bool,
}

impl AudioDeviceInfo {
    pub fn validate(&self) -> Result<()> {
        validate_device_id(&self.id)?;
        validate_device_name(&self.name)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeCause {
    UserSelection,
    Topology,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AudioOutputStatus {
    #[default]
    Initializing,
    Ready,
    Switching,
    Unavailable,
    Closed,
    NotApplicable,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AudioOutputMode {
    #[default]
    Device,
    Memory,
}

/// Fixed categories keep callback errors and query/event payloads bounded.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AudioOutputError {
    DeviceNotAvailable,
    StreamInvalidated,
    BufferUnderrun,
    BackendSpecific,
    InventoryFailed,
    ConfigurationUnavailable,
    StreamBuildFailed,
    StreamStartFailed,
    InvalidSelection,
    WorkerUnavailable,
}

impl fmt::Display for AudioOutputError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::DeviceNotAvailable => "Audio output device is unavailable",
            Self::StreamInvalidated => "Audio output configuration was invalidated",
            Self::BufferUnderrun => "Audio output buffer underrun",
            Self::BackendSpecific => "Audio backend reported a stream error",
            Self::InventoryFailed => "Cannot enumerate audio output devices",
            Self::ConfigurationUnavailable => "Audio output has no supported configuration",
            Self::StreamBuildFailed => "Cannot create audio output stream",
            Self::StreamStartFailed => "Cannot start audio output stream",
            Self::InvalidSelection => "Audio output device selection is invalid",
            Self::WorkerUnavailable => "Audio output worker is unavailable",
        })
    }
}

impl std::error::Error for AudioOutputError {}

/// The configuration of the actual opened output, after format negotiation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AudioOutputFormat {
    pub sample_rate: u32,
    pub channels: u16,
}

impl AudioOutputFormat {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.sample_rate > 0,
            "Audio output sample rate must be positive"
        );
        ensure!(
            self.channels > 0 && self.channels <= 8,
            "Audio output channel count must be between 1 and 8"
        );
        Ok(())
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AudioOutputState {
    pub desired: OutputSelection,
    pub active_device_id: Option<String>,
    pub active_device_name: Option<String>,
    pub format: Option<AudioOutputFormat>,
    pub status: AudioOutputStatus,
    pub switch_sequence: u64,
    pub fallback: bool,
    pub last_error: Option<AudioOutputError>,
    pub mode: AudioOutputMode,
}

impl AudioOutputState {
    pub fn validate(&self) -> Result<()> {
        self.desired.validate()?;
        if let Some(id) = &self.active_device_id {
            validate_device_id(id)?;
        }
        if let Some(name) = &self.active_device_name {
            validate_device_name(name)?;
        }
        if let Some(format) = self.format {
            format.validate()?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AudioOutputSnapshot {
    pub devices: Vec<AudioDeviceInfo>,
    pub state: AudioOutputState,
    pub preferences: AudioPreferences,
}

impl AudioOutputSnapshot {
    pub fn validate(&self) -> Result<()> {
        validate_device_inventory(&self.devices)?;
        self.state.validate()?;
        self.preferences.validate()
    }
}

pub fn validate_device_id(id: &str) -> Result<()> {
    validate_text(id, MAX_AUDIO_DEVICE_ID_BYTES, "Audio device identity")
}

pub fn validate_device_name(name: &str) -> Result<()> {
    validate_text(name, MAX_AUDIO_DEVICE_NAME_BYTES, "Audio device name")
}

pub fn validate_device_inventory(devices: &[AudioDeviceInfo]) -> Result<()> {
    ensure!(
        devices.len() <= MAX_AUDIO_DEVICES,
        "Audio device inventory exceeds 128 devices"
    );
    let mut identities = BTreeSet::new();
    let mut default_count = 0;
    for device in devices {
        device.validate()?;
        ensure!(
            identities.insert(&device.id),
            "Audio device inventory contains duplicate identities"
        );
        default_count += usize::from(device.is_default);
    }
    ensure!(
        default_count <= 1,
        "Audio device inventory contains multiple system defaults"
    );
    Ok(())
}

fn validate_text(value: &str, maximum: usize, name: &str) -> Result<()> {
    ensure!(
        !value.is_empty() && value.len() <= maximum && !value.chars().any(char::is_control),
        "{name} is empty, too long, or contains control characters"
    );
    Ok(())
}

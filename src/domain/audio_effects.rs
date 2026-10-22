//! Validated effect preferences. Resource identifiers carry no file authority.
pub use crate::audio::spatial::SpatialSettings;
use anyhow::{Result, ensure};
use serde::{Deserialize, Deserializer, Serialize, de::Error};

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "kind", content = "id", rename_all = "snake_case")]
pub enum ImpulseResourceId {
    Bundled(String),
    Imported(String),
}
#[derive(Deserialize)]
#[serde(
    tag = "kind",
    content = "id",
    rename_all = "snake_case",
    deny_unknown_fields
)]
enum ImpulseResourceWire {
    Bundled(String),
    Imported(String),
}
impl<'de> Deserialize<'de> for ImpulseResourceId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        let value = match ImpulseResourceWire::deserialize(deserializer)? {
            ImpulseResourceWire::Bundled(id) => Self::Bundled(id),
            ImpulseResourceWire::Imported(id) => Self::Imported(id),
        };
        value.validate().map_err(D::Error::custom)?;
        Ok(value)
    }
}
impl ImpulseResourceId {
    pub fn validate(&self) -> Result<()> {
        let (kind, id) = match self {
            Self::Bundled(id) => ("bundled", id),
            Self::Imported(id) => ("imported", id),
        };
        ensure!(
            !id.is_empty()
                && id.len() <= 128
                && id
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-')),
            "Invalid {kind} impulse resource identity"
        );
        Ok(())
    }
}
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ConvolutionSettings {
    pub impulse: Option<ImpulseResourceId>,
    pub main_gain: f64,
    pub send_gain: f64,
}
impl Default for ConvolutionSettings {
    fn default() -> Self {
        Self {
            impulse: None,
            main_gain: 1.0,
            send_gain: 0.0,
        }
    }
}
impl ConvolutionSettings {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.main_gain.is_finite()
                && self.send_gain.is_finite()
                && (0.0..=5.0).contains(&self.main_gain)
                && (0.0..=5.0).contains(&self.send_gain),
            "Convolution gains must be finite and between zero and five"
        );
        if let Some(resource) = &self.impulse {
            resource.validate()?;
        }
        Ok(())
    }
    pub fn from_lx_slider(
        impulse: Option<ImpulseResourceId>,
        main: f64,
        send: f64,
    ) -> Result<Self> {
        let settings = Self {
            impulse,
            main_gain: main / 10.0,
            send_gain: send / 10.0,
        };
        settings.validate()?;
        Ok(settings)
    }
    pub fn lx_slider_gains(&self) -> Result<(f64, f64)> {
        self.validate()?;
        Ok((self.main_gain * 10.0, self.send_gain * 10.0))
    }
}
#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
struct ConvolutionWire {
    impulse: Option<ImpulseResourceId>,
    main_gain: f64,
    send_gain: f64,
}
impl Default for ConvolutionWire {
    fn default() -> Self {
        let settings = ConvolutionSettings::default();
        Self {
            impulse: settings.impulse,
            main_gain: settings.main_gain,
            send_gain: settings.send_gain,
        }
    }
}
impl<'de> Deserialize<'de> for ConvolutionSettings {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        let wire = ConvolutionWire::deserialize(deserializer)?;
        let settings = Self {
            impulse: wire.impulse,
            main_gain: wire.main_gain,
            send_gain: wire.send_gain,
        };
        settings.validate().map_err(D::Error::custom)?;
        Ok(settings)
    }
}
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct EffectsSettings {
    pub convolution: ConvolutionSettings,
    pub spatial: SpatialSettings,
}
impl EffectsSettings {
    pub fn validate(&self) -> Result<()> {
        self.convolution.validate()?;
        self.spatial.validate()
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EffectsStatus {
    #[default]
    Idle,
    Preparing,
    Transitioning,
    Ready,
    Failed,
    Retiring,
}
#[derive(Clone, Debug, Serialize)]
pub struct EffectsSnapshot {
    pub desired: EffectsSettings,
    pub requested_sequence: u64,
    pub applied_sequence: u64,
    pub status: EffectsStatus,
    pub sample_rate: Option<u32>,
    pub effect_channels: u16,
    pub device_channels: u16,
    pub latency_frames: u32,
    pub tail_remaining_frames: u64,
    pub transition_remaining_frames: u32,
    pub compressor_reduction_db: f32,
    pub owned_bytes: usize,
    pub error: Option<String>,
}
impl Default for EffectsSnapshot {
    fn default() -> Self {
        Self {
            desired: EffectsSettings::default(),
            requested_sequence: 1,
            applied_sequence: 0,
            status: EffectsStatus::Idle,
            sample_rate: None,
            effect_channels: 0,
            device_channels: 0,
            latency_frames: 0,
            tail_remaining_frames: 0,
            transition_remaining_frames: 0,
            compressor_reduction_db: 0.0,
            owned_bytes: 0,
            error: None,
        }
    }
}
#[derive(Clone, Debug, Serialize)]
pub struct SpatialRuntimeState {
    pub desired: SpatialSettings,
    pub requested_sequence: u64,
    pub applied_sequence: u64,
    pub error: Option<String>,
}

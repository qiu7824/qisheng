//! Original LX convolution presets, bound to embedded lossless WAV bytes.
//! Lookup provides no file authority; only the selected asset is decoded.
use super::{
    effects_factory::OwnedImpulse,
    impulse_decode::{ImpulseDecodeContext, ImpulseEncodedInput, decode_impulse},
};
use crate::domain::audio_effects::{ConvolutionSettings, ImpulseResourceId};
use anyhow::{Context, Result, ensure};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::sync::Arc;

pub const SOURCE_COMMIT: &str = "ad95d5091c9ed689fa72b5e5c849df65f5a679ce";

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct BundledImpulsePreset {
    pub id: &'static str,
    pub resource_id: ImpulseResourceId,
    pub file_name: &'static str,
    pub main_gain: f64,
    pub send_gain: f64,
    pub sha256: &'static str,
    pub encoded_bytes: usize,
    pub sample_rate: u32,
    pub channels: u16,
    pub frames: u64,
}
impl BundledImpulsePreset {
    pub fn convolution_settings(&self) -> ConvolutionSettings {
        ConvolutionSettings {
            impulse: Some(self.resource_id.clone()),
            main_gain: self.main_gain,
            send_gain: self.send_gain,
        }
    }
}
struct EmbeddedPreset {
    id: &'static str,
    file_name: &'static str,
    main_gain: f64,
    send_gain: f64,
    sha256: &'static str,
    sample_rate: u32,
    channels: u16,
    frames: u64,
    bytes: &'static [u8],
}
impl EmbeddedPreset {
    fn metadata(&self) -> BundledImpulsePreset {
        BundledImpulsePreset {
            id: self.id,
            resource_id: ImpulseResourceId::Bundled(self.id.into()),
            file_name: self.file_name,
            main_gain: self.main_gain,
            send_gain: self.send_gain,
            sha256: self.sha256,
            encoded_bytes: self.bytes.len(),
            sample_rate: self.sample_rate,
            channels: self.channels,
            frames: self.frames,
        }
    }
}
static PRESETS: [EmbeddedPreset; 13] = [
    EmbeddedPreset {
        id: "telephone",
        file_name: "filter-telephone.wav",
        main_gain: 0.0,
        send_gain: 3.0,
        sha256: "4dc96614900e1b3fa85611de2ddfd39f2e26530b3b8336239dc1badbfd89f7e9",
        sample_rate: 44100,
        channels: 2,
        frames: 72312,
        bytes: include_bytes!("../../assets/impulses/filter-telephone.wav"),
    },
    EmbeddedPreset {
        id: "s2_r4_bd",
        file_name: "s2_r4_bd.wav",
        main_gain: 1.8,
        send_gain: 0.9,
        sha256: "400e242cb263d1ce83bed6b7fc2e75f498e0695735974f943d0269befcb31b2f",
        sample_rate: 48000,
        channels: 2,
        frames: 192000,
        bytes: include_bytes!("../../assets/impulses/s2_r4_bd.wav"),
    },
    EmbeddedPreset {
        id: "bright_hall",
        file_name: "bright-hall.wav",
        main_gain: 0.8,
        send_gain: 2.4,
        sha256: "4f0d8c51a4a5f7a5061c4037136a314a673abb502a86fab869a3b068a4754f83",
        sample_rate: 44100,
        channels: 4,
        frames: 65536,
        bytes: include_bytes!("../../assets/impulses/bright-hall.wav"),
    },
    EmbeddedPreset {
        id: "cinema_diningroom",
        file_name: "cinema-diningroom.wav",
        main_gain: 0.6,
        send_gain: 2.3,
        sha256: "c515a820912cd2fe54f687f6cee36bb2c0b947a6bc23ad057327a8ae9abe44b0",
        sample_rate: 44100,
        channels: 2,
        frames: 65044,
        bytes: include_bytes!("../../assets/impulses/cinema-diningroom.wav"),
    },
    EmbeddedPreset {
        id: "dining_living_true_stereo",
        file_name: "dining-living-true-stereo.wav",
        main_gain: 0.6,
        send_gain: 1.8,
        sha256: "b0287ec2305edc75d6c484c6a9e55339162acdc2edf7c18b53dfb835563b454f",
        sample_rate: 44100,
        channels: 4,
        frames: 68232,
        bytes: include_bytes!("../../assets/impulses/dining-living-true-stereo.wav"),
    },
    EmbeddedPreset {
        id: "living_bedroom_leveled",
        file_name: "living-bedroom-leveled.wav",
        main_gain: 0.6,
        send_gain: 2.1,
        sha256: "d0ec29a1ac756ee6fcd20e1d05018792e645eb606cdc8727a38497f99de34592",
        sample_rate: 44100,
        channels: 2,
        frames: 54095,
        bytes: include_bytes!("../../assets/impulses/living-bedroom-leveled.wav"),
    },
    EmbeddedPreset {
        id: "spreader50_65ms",
        file_name: "spreader50-65ms.wav",
        main_gain: 1.0,
        send_gain: 2.5,
        sha256: "40fffaf053416be74002d7636c7b469eb17c8762a0d096491c986d00f9a36e64",
        sample_rate: 44100,
        channels: 4,
        frames: 44100,
        bytes: include_bytes!("../../assets/impulses/spreader50-65ms.wav"),
    },
    EmbeddedPreset {
        id: "s3_r1_bd",
        file_name: "s3_r1_bd.wav",
        main_gain: 1.8,
        send_gain: 0.8,
        sha256: "3ae4c9ade438e69c23e2e26ecc9bc579bee345be506a6ac30349c34c66177be0",
        sample_rate: 48000,
        channels: 2,
        frames: 192000,
        bytes: include_bytes!("../../assets/impulses/s3_r1_bd.wav"),
    },
    EmbeddedPreset {
        id: "matrix_1",
        file_name: "matrix-reverb1.wav",
        main_gain: 1.5,
        send_gain: 0.9,
        sha256: "333d05904c01c774b28a05a0fae147f8475f6e4f7daa92c3e16d879867f97d0c",
        sample_rate: 44100,
        channels: 2,
        frames: 147120,
        bytes: include_bytes!("../../assets/impulses/matrix-reverb1.wav"),
    },
    EmbeddedPreset {
        id: "matrix_2",
        file_name: "matrix-reverb2.wav",
        main_gain: 1.3,
        send_gain: 1.0,
        sha256: "888b47ee698110b8e0de89aa8f03edfcd76f19485b2c08675d5ff1d72c6c1343",
        sample_rate: 44100,
        channels: 2,
        frames: 124928,
        bytes: include_bytes!("../../assets/impulses/matrix-reverb2.wav"),
    },
    EmbeddedPreset {
        id: "cardiod_35_10_spread",
        file_name: "cardiod-35-10-spread.wav",
        main_gain: 1.8,
        send_gain: 0.6,
        sha256: "cf3ab1550ea86065baa9f63a00c6265de7360e167a0bc6a1d0ca54b3f97050ad",
        sample_rate: 44100,
        channels: 2,
        frames: 158336,
        bytes: include_bytes!("../../assets/impulses/cardiod-35-10-spread.wav"),
    },
    EmbeddedPreset {
        id: "tim_omni_35_10_magnetic",
        file_name: "tim-omni-35-10-magnetic.wav",
        main_gain: 1.0,
        send_gain: 0.2,
        sha256: "d73086428420282fe2452f71e289d8521ac9b118947637e9ef97bc832de710c4",
        sample_rate: 44100,
        channels: 2,
        frames: 168237,
        bytes: include_bytes!("../../assets/impulses/tim-omni-35-10-magnetic.wav"),
    },
    EmbeddedPreset {
        id: "feedback_spring",
        file_name: "feedback-spring.wav",
        main_gain: 1.8,
        send_gain: 0.8,
        sha256: "41d038558169517e38995325de9309c28ebf7c01abda579ec58670043f1961ba",
        sample_rate: 44100,
        channels: 2,
        frames: 176400,
        bytes: include_bytes!("../../assets/impulses/feedback-spring.wav"),
    },
];

/// Metadata query never prepares PCM, opens files or creates a private vault.
pub fn presets() -> Vec<BundledImpulsePreset> {
    PRESETS.iter().map(EmbeddedPreset::metadata).collect()
}
fn embedded(id: &str) -> Result<&'static EmbeddedPreset> {
    PRESETS
        .iter()
        .find(|preset| preset.id == id)
        .context("Unknown bundled impulse preset")
}
pub fn preset(id: &str) -> Result<BundledImpulsePreset> {
    Ok(embedded(id)?.metadata())
}
pub(crate) fn prepare(id: &str, context: &ImpulseDecodeContext) -> Result<Arc<OwnedImpulse>> {
    let asset = embedded(id)?;
    decode_asset(asset, asset.bytes, context)
}
fn decode_asset(
    asset: &EmbeddedPreset,
    bytes: &[u8],
    context: &ImpulseDecodeContext,
) -> Result<Arc<OwnedImpulse>> {
    context.check()?;
    ensure!(
        bytes.len() == asset.bytes.len(),
        "Bundled impulse byte count changed"
    );
    let mut input = ImpulseEncodedInput::prepare(bytes.len())?;
    let mut digest = Sha256::new();
    for chunk in bytes.chunks(64 * 1024) {
        context.check()?;
        digest.update(chunk);
        input.append(chunk)?;
    }
    let actual_sha256 = format!("{:x}", digest.finalize());
    ensure!(
        actual_sha256 == asset.sha256,
        "Bundled impulse representation digest changed"
    );
    let report = decode_impulse(input, "wav", context)?;
    ensure!(
        report.decoded_frames == asset.frames
            && report.impulse.frames() as u64 == asset.frames
            && report.impulse.sample_rate() == asset.sample_rate
            && report.impulse.channels() == asset.channels,
        "Bundled impulse decoded geometry changed"
    );
    context.check()?;
    Ok(OwnedImpulse::from_validated(
        report.impulse,
        ImpulseResourceId::Bundled(asset.id.into()),
        actual_sha256,
    )?)
}

#[cfg(test)]
mod tests;

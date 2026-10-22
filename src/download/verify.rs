use super::{DownloadCancel, OwnedPart};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use symphonia::core::{
    audio::SampleBuffer,
    codecs::{CODEC_TYPE_NULL, DecoderOptions},
    errors::Error,
    formats::FormatOptions,
    io::{MediaSourceStream, MediaSourceStreamOptions},
    meta::{Limit, MetadataOptions},
    probe::Hint,
};

const MAX_PACKET_FRAMES: usize = 8192;
const MAX_CHANNELS: usize = 32;
const MAX_PACKET_BYTES: usize = 1024 * 1024;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PcmDigestEncoding {
    F64LeInterleavedV1,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AudioVerification {
    pub pcm_digest_encoding: PcmDigestEncoding,
    pub codec_identifier: String,
    pub source_bits_per_sample: Option<u32>,
    pub source_bits_per_coded_sample: Option<u32>,
    pub sample_rate: u32,
    pub channels: u16,
    pub decoded_frames: u64,
    pub decoded_pcm_sha256: String,
}
pub trait AudioVerifier: Send + Sync {
    fn verify(
        &self,
        part: &OwnedPart,
        extension: &str,
        cancel: &DownloadCancel,
    ) -> Result<AudioVerification>;
}
#[derive(Default)]
pub struct SymphoniaVerifier;
impl AudioVerifier for SymphoniaVerifier {
    fn verify(
        &self,
        part: &OwnedPart,
        extension: &str,
        cancel: &DownloadCancel,
    ) -> Result<AudioVerification> {
        cancel.check()?;
        part.verify()?;
        let reader = part.reader(cancel.clone())?;
        verify_owned_reader(Box::new(reader), extension, cancel, &|| part.verify())
    }
}

pub(crate) fn verify_owned_reader(
    reader: Box<dyn symphonia::core::io::MediaSource>,
    extension: &str,
    cancel: &DownloadCancel,
    check: &dyn Fn() -> Result<()>,
) -> Result<AudioVerification> {
    cancel.check()?;
    check()?;
    let stream = MediaSourceStream::new(
        reader,
        MediaSourceStreamOptions {
            buffer_len: 64 * 1024,
        },
    );
    let mut hint = Hint::new();
    hint.with_extension(extension);
    let probe = symphonia::default::get_probe()
        .format(
            &hint,
            stream,
            &FormatOptions {
                enable_gapless: true,
                ..Default::default()
            },
            &MetadataOptions {
                limit_metadata_bytes: Limit::Maximum(16 * 1024 * 1024),
                limit_visual_bytes: Limit::Maximum(8 * 1024 * 1024),
            },
        )
        .context("Downloaded audio format is invalid")?;
    let mut format = probe.format;
    let track = format
        .default_track()
        .or_else(|| {
            format
                .tracks()
                .iter()
                .find(|t| t.codec_params.codec != CODEC_TYPE_NULL)
        })
        .context("Downloaded file has no audio track")?;
    let id = track.id;
    let parameters = track.codec_params.clone();
    ensure!(
        parameters
            .max_frames_per_packet
            .is_none_or(|n| n <= MAX_PACKET_FRAMES as u64),
        "Downloaded codec packet exceeds verification memory bound"
    );
    let mut decoder = symphonia::default::get_codecs()
        .make(&parameters, &DecoderOptions { verify: true })
        .context("Downloaded audio codec is unsupported")?;
    let mut pcm = Sha256::new();
    let mut frames = 0u64;
    let mut spec = None;
    let mut samples: Option<SampleBuffer<f64>> = None;
    loop {
        cancel.check()?;
        check()?;
        let packet = match format.next_packet() {
            Ok(packet) => packet,
            Err(Error::IoError(error)) if error.kind() == std::io::ErrorKind::UnexpectedEof => {
                break;
            }
            Err(error) => return Err(error).context("Downloaded audio packet is invalid"),
        };
        ensure!(
            packet.data.len() <= MAX_PACKET_BYTES,
            "Downloaded audio packet exceeds verification memory bound"
        );
        if packet.track_id() != id {
            continue;
        }
        let decoded = decoder
            .decode(&packet)
            .context("Downloaded audio cannot be fully decoded")?;
        let actual = *decoded.spec();
        ensure!(
            decoded.capacity() <= MAX_PACKET_FRAMES
                && actual.channels.count() > 0
                && actual.channels.count() <= MAX_CHANNELS
                && actual.rate > 0,
            "Downloaded PCM exceeds verification memory bound"
        );
        ensure!(
            spec.is_none_or(|expected| expected == actual),
            "Downloaded audio changes its format during decoding"
        );
        spec = Some(actual);
        let buffer = samples
            .get_or_insert_with(|| SampleBuffer::<f64>::new(MAX_PACKET_FRAMES as u64, actual));
        buffer.copy_interleaved_ref(decoded);
        for sample in buffer.samples() {
            ensure!(sample.is_finite(), "Downloaded audio contains invalid PCM");
            pcm.update(sample.to_le_bytes());
        }
        frames = frames
            .checked_add((buffer.samples().len() / actual.channels.count()) as u64)
            .context("Downloaded frame count overflow")?;
    }
    ensure!(
        decoder.finalize().verify_ok.is_none_or(|valid| valid),
        "Downloaded audio codec integrity check failed"
    );
    let spec = spec.context("Downloaded file has no decodable audio")?;
    ensure!(frames > 0, "Downloaded audio is empty");
    if matches!(extension, "wav" | "flac") {
        ensure!(
            parameters
                .n_frames
                .is_none_or(|expected| expected == frames),
            "Downloaded lossless audio ends before or after its declared frame count"
        );
    }
    cancel.check()?;
    check()?;
    Ok(AudioVerification {
        pcm_digest_encoding: PcmDigestEncoding::F64LeInterleavedV1,
        codec_identifier: parameters.codec.to_string(),
        source_bits_per_sample: parameters.bits_per_sample,
        source_bits_per_coded_sample: parameters.bits_per_coded_sample,
        sample_rate: spec.rate,
        channels: spec.channels.count() as u16,
        decoded_frames: frames,
        decoded_pcm_sha256: format!("{:x}", pcm.finalize()),
    })
}

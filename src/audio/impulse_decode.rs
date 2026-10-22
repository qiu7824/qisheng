//! Bounded impulse decoding from an already owned input. No path grants arise.
use super::{
    convolution::{ImpulseBuilder, ValidatedImpulse},
    processing::DspReservation,
};
use anyhow::{Context, Result, ensure};
use std::{
    io::{Cursor, ErrorKind},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use symphonia::core::{
    audio::SampleBuffer,
    codecs::{
        CODEC_TYPE_FLAC, CODEC_TYPE_NULL, CODEC_TYPE_PCM_F32LE, CODEC_TYPE_PCM_F64LE,
        CODEC_TYPE_PCM_S16LE, CODEC_TYPE_PCM_S24LE, CODEC_TYPE_PCM_S32LE, CODEC_TYPE_PCM_U8,
        DecoderOptions,
    },
    errors::Error,
    formats::FormatOptions,
    io::{MediaSourceStream, MediaSourceStreamOptions},
    meta::{Limit, MetadataOptions},
    probe::Hint,
};

pub const MAX_IMPULSE_ENCODED_BYTES: usize = 8 * 1024 * 1024;
#[derive(Clone)]
pub struct ImpulseDecodeContext {
    cancel: Arc<AtomicBool>,
    deadline: Instant,
}
impl ImpulseDecodeContext {
    pub fn new(timeout: Duration) -> Result<Self> {
        ensure!(
            !timeout.is_zero() && timeout <= Duration::from_secs(120),
            "Impulse deadline must be positive and at most 120 seconds"
        );
        Ok(Self {
            cancel: Arc::new(AtomicBool::new(false)),
            deadline: Instant::now() + timeout,
        })
    }
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Release);
    }
    pub(crate) fn check(&self) -> Result<()> {
        ensure!(
            !self.cancel.load(Ordering::Acquire),
            "Impulse decode cancelled"
        );
        ensure!(
            Instant::now() < self.deadline,
            "Impulse decode deadline exceeded"
        );
        Ok(())
    }
    pub(crate) fn cancel_flag(&self) -> Arc<AtomicBool> {
        self.cancel.clone()
    }
}
const MAX_PACKET_FRAMES: usize = 8192;
const MAX_PACKET_BYTES: usize = 1024 * 1024;
pub struct ImpulseEncodedInput {
    bytes: Vec<u8>,
    maximum: usize,
    _reservation: DspReservation,
}
impl ImpulseEncodedInput {
    pub fn prepare(maximum: usize) -> Result<Self> {
        ensure!(
            maximum > 0 && maximum <= MAX_IMPULSE_ENCODED_BYTES,
            "Impulse encoded admission exceeds 8 MiB"
        );
        let reservation = DspReservation::new(maximum + std::mem::size_of::<Self>())?;
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(maximum)
            .context("Cannot reserve impulse encoded input")?;
        ensure!(
            bytes.capacity() <= maximum,
            "Impulse encoded allocation exceeds its reservation"
        );
        Ok(Self {
            bytes,
            maximum,
            _reservation: reservation,
        })
    }
    pub fn append(&mut self, bytes: &[u8]) -> Result<()> {
        ensure!(
            bytes.len() <= self.maximum - self.bytes.len(),
            "Impulse encoded byte budget exceeded"
        );
        self.bytes.extend_from_slice(bytes);
        Ok(())
    }
    pub fn len(&self) -> usize {
        self.bytes.len()
    }
    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }
}
struct ReservedInputSource {
    cursor: Cursor<Vec<u8>>,
    _reservation: DspReservation,
    context: ImpulseDecodeContext,
}
impl std::io::Read for ReservedInputSource {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        self.context.check().map_err(std::io::Error::other)?;
        let count = buffer.len().min(64 * 1024);
        std::io::Read::read(&mut self.cursor, &mut buffer[..count])
    }
}
impl std::io::Seek for ReservedInputSource {
    fn seek(&mut self, position: std::io::SeekFrom) -> std::io::Result<u64> {
        self.context.check().map_err(std::io::Error::other)?;
        std::io::Seek::seek(&mut self.cursor, position)
    }
}
impl symphonia::core::io::MediaSource for ReservedInputSource {
    fn is_seekable(&self) -> bool {
        true
    }
    fn byte_len(&self) -> Option<u64> {
        Some(self.cursor.get_ref().len() as u64)
    }
}
pub struct ImpulseDecodeReport {
    pub impulse: ValidatedImpulse,
    pub codec: String,
    pub decoded_frames: u64,
}
pub fn decode_impulse(
    input: ImpulseEncodedInput,
    extension: &str,
    context: &ImpulseDecodeContext,
) -> Result<ImpulseDecodeReport> {
    ensure!(!input.is_empty(), "Impulse encoded input exceeds 8 MiB");
    ensure!(
        matches!(extension, "wav" | "flac"),
        "Unsupported impulse extension hint"
    );
    ensure!(
        if extension == "flac" {
            input.bytes.starts_with(b"fLaC")
        } else {
            input.bytes.len() >= 12
                && &input.bytes[..4] == b"RIFF"
                && &input.bytes[8..12] == b"WAVE"
        },
        "Impulse container differs from its selected format"
    );
    context.check()?;
    let encoded = DspReservation::new(
        64 * 1024 + 2 * 256 * 1024 + MAX_PACKET_BYTES + MAX_PACKET_FRAMES * 4 * 16,
    )?;
    let stream = MediaSourceStream::new(
        Box::new(ReservedInputSource {
            cursor: Cursor::new(input.bytes),
            _reservation: input._reservation,
            context: context.clone(),
        }),
        MediaSourceStreamOptions {
            buffer_len: 64 * 1024,
        },
    );
    let mut hint = Hint::new();
    hint.with_extension(extension);
    let probed = symphonia::default::get_probe().format(
        &hint,
        stream,
        &FormatOptions {
            enable_gapless: true,
            ..Default::default()
        },
        &MetadataOptions {
            limit_metadata_bytes: Limit::Maximum(256 * 1024),
            limit_visual_bytes: Limit::Maximum(256 * 1024),
        },
    )?;
    let mut format = probed.format;
    let track = format
        .default_track()
        .or_else(|| {
            format
                .tracks()
                .iter()
                .find(|track| track.codec_params.codec != CODEC_TYPE_NULL)
        })
        .context("Impulse input has no audio track")?;
    let id = track.id;
    let parameters = track.codec_params.clone();
    ensure!(
        if extension == "flac" {
            parameters.codec == CODEC_TYPE_FLAC
        } else {
            matches!(
                parameters.codec,
                CODEC_TYPE_PCM_S32LE
                    | CODEC_TYPE_PCM_S24LE
                    | CODEC_TYPE_PCM_S16LE
                    | CODEC_TYPE_PCM_U8
                    | CODEC_TYPE_PCM_F32LE
                    | CODEC_TYPE_PCM_F64LE
            )
        },
        "Impulse codec is not an admitted lossless PCM codec"
    );
    ensure!(
        parameters
            .extra_data
            .as_ref()
            .is_none_or(|bytes| bytes.len() <= 256 * 1024),
        "Impulse codec private metadata exceeds its bound"
    );
    if parameters.codec == CODEC_TYPE_FLAC {
        let header = parameters
            .extra_data
            .as_ref()
            .context("Impulse FLAC stream information is missing")?;
        ensure!(
            header.len() >= 34,
            "Impulse FLAC stream information is truncated"
        );
        let maximum = u16::from_be_bytes([header[2], header[3]]) as usize;
        ensure!(
            maximum > 0 && maximum <= MAX_PACKET_FRAMES,
            "Impulse FLAC decoder block exceeds its admitted storage"
        );
    }
    let rate = parameters
        .sample_rate
        .context("Impulse sample rate is missing")?;
    let channels = parameters
        .channels
        .context("Impulse channel layout is missing")?
        .count();
    ensure!(
        (8000..=384000).contains(&rate) && matches!(channels, 1 | 2 | 4),
        "Impulse sample format is unsupported"
    );
    ensure!(
        parameters
            .max_frames_per_packet
            .is_none_or(|frames| frames <= MAX_PACKET_FRAMES as u64),
        "Impulse packet allocation exceeds its bound"
    );
    let maximum = rate as usize * 4;
    let admitted = parameters
        .n_frames
        .map(|frames| usize::try_from(frames).context("Impulse declared frame count overflow"))
        .transpose()?
        .unwrap_or(maximum);
    ensure!(
        admitted > 0 && admitted <= maximum,
        "Impulse duration exceeds four seconds"
    );
    let mut builder = ImpulseBuilder::prepare(rate, channels as u16, admitted)?;
    let mut decoder =
        symphonia::default::get_codecs().make(&parameters, &DecoderOptions { verify: true })?;
    let mut buffer = None;
    let mut frames = 0u64;
    loop {
        context.check()?;
        let packet = match format.next_packet() {
            Ok(packet) => packet,
            Err(Error::IoError(error)) if error.kind() == ErrorKind::UnexpectedEof => break,
            Err(error) => return Err(error).context("Impulse packet is invalid"),
        };
        ensure!(
            packet.data.len() <= MAX_PACKET_BYTES,
            "Impulse packet byte budget exceeded"
        );
        if packet.track_id() != id {
            continue;
        }
        let decoded = decoder
            .decode(&packet)
            .context("Impulse PCM cannot be decoded")?;
        let actual = *decoded.spec();
        ensure!(
            decoded.capacity() <= MAX_PACKET_FRAMES
                && actual.rate == rate
                && actual.channels.count() == channels,
            "Impulse format or packet allocation changed"
        );
        let samples = buffer
            .get_or_insert_with(|| SampleBuffer::<f32>::new(MAX_PACKET_FRAMES as u64, actual));
        samples.copy_interleaved_ref(decoded);
        for frame in samples.samples().chunks_exact(channels) {
            builder.push_frame(frame)?;
            frames = frames
                .checked_add(1)
                .context("Impulse decoded frame count overflow")?;
        }
    }
    ensure!(
        frames > 0 && decoder.finalize().verify_ok.is_none_or(|valid| valid),
        "Impulse codec integrity failed"
    );
    if matches!(extension, "wav" | "flac") {
        ensure!(
            parameters
                .n_frames
                .is_none_or(|expected| expected == frames),
            "Impulse lossless duration differs from its declaration"
        );
    }
    context.check()?;
    let impulse = builder.finish()?;
    drop(buffer);
    drop(decoder);
    drop(format);
    drop(encoded);
    Ok(ImpulseDecodeReport {
        impulse,
        codec: parameters.codec.to_string(),
        decoded_frames: frames,
    })
}

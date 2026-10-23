//! Explicit Symphonia decoding on an interruptible worker, never an output callback.

use super::pcm::{
    MAX_CHANNELS, MAX_CLOCK_FRAMES, PcmControl, PcmFormat, PcmProducer, PcmReservation,
    PcmSnapshot, PcmSource,
};
use super::processing::{ProcessedFrame, WorkerProcessor};
use super::transport::{MediaIoError, MediaIoErrorKind, StreamControl, StreamLease};
use super::worker_owner::{TrackedWorker, WorkerKind, WorkerPermit};
use crate::domain::playback_processing::PlaybackProcessingSettings;
use serde::Serialize;
use std::fmt;
use std::io::{ErrorKind, Seek, SeekFrom};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::{CODEC_TYPE_NULL, CodecParameters, Decoder, DecoderOptions};
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo};
use symphonia::core::io::{MediaSource, MediaSourceStream, MediaSourceStreamOptions};
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;
use symphonia::core::units::TimeBase;

const PROBING: u8 = 0;
const READY: u8 = 1;
const SEEKING: u8 = 2;
const NORMAL_EOF: u8 = 3;
const FAILED: u8 = 4;
const CANCELLED: u8 = 5;
const MAX_PACKET_FRAMES: usize = 65_536;
// SampleBuffer plus decoded PCM of the selected codec, conservatively f64 sized.

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub enum DecodeErrorKind {
    Cancelled,
    Timeout,
    Network,
    Protocol,
    Storage,
    Incomplete,
    Unsupported,
    Decode,
    Seek,
    Limit,
    RepresentationChanged,
    Panic,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct DecodeError {
    pub kind: DecodeErrorKind,
    pub message: String,
    pub media: Option<MediaIoError>,
}
impl DecodeError {
    fn new(kind: DecodeErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            media: None,
        }
    }
}
impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}
impl std::error::Error for DecodeError {}

#[derive(Clone, Debug)]
pub struct DecoderConfig {
    pub queue_duration: Duration,
    pub prebuffer_duration: Duration,
    pub initially_paused: bool,
    pub readiness_timeout: Duration,
    pub extension_hint: Option<String>,
    pub processing: PlaybackProcessingSettings,
    pub processing_sequence: u64,
}
impl Default for DecoderConfig {
    fn default() -> Self {
        Self {
            queue_duration: Duration::from_millis(250),
            prebuffer_duration: Duration::from_millis(40),
            initially_paused: false,
            readiness_timeout: Duration::from_secs(30),
            extension_hint: None,
            processing: PlaybackProcessingSettings::default(),
            processing_sequence: 0,
        }
    }
}

pub struct DecoderReady {
    pub source: PcmSource,
    pub format: PcmFormat,
    pub duration: Option<Duration>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub enum DecoderStatus {
    Probing,
    Ready,
    Seeking,
    Ended,
    Error,
    Cancelled,
}

#[derive(Clone, Debug)]
pub struct DecoderSnapshot {
    pub status: DecoderStatus,
    pub seek_seq: u64,
    pub format: Option<PcmFormat>,
    pub duration: Option<Duration>,
    pub pcm: PcmSnapshot,
    pub error: Option<DecodeError>,
    pub worker_finished: bool,
}

#[derive(Default)]
struct WorkerState {
    pcm: Option<PcmControl>,
    format: Option<PcmFormat>,
    duration: Option<Duration>,
    error: Option<DecodeError>,
}
struct Shared {
    state: Mutex<WorkerState>,
    pending_seek: Mutex<Option<(u64, Duration)>>,
    requested_seek: AtomicU64,
    committed_seek: AtomicU64,
    paused: AtomicBool,
    cancel: AtomicBool,
    phase: AtomicU8,
    finished: AtomicBool,
    pending_processing: Mutex<Option<(PlaybackProcessingSettings, u64)>>,
    requested_processing: AtomicU64,
}

/// Its owner performs bounded shutdown; ordinary controls never join the worker.
pub struct DecoderHandle {
    shared: Arc<Shared>,
    stream: Option<StreamControl>,
    ready: mpsc::Receiver<Result<DecoderReady, DecodeError>>,
    thread: Option<TrackedWorker>,
}

impl DecoderHandle {
    pub fn start(lease: StreamLease, config: DecoderConfig) -> Result<Self, DecodeError> {
        Self::start_inner(Box::new(lease.reader), Some(lease.control), config)
    }

    /// Local MediaSource support uses the same production decoder and PCM path.
    pub fn start_source(
        source: Box<dyn MediaSource>,
        config: DecoderConfig,
    ) -> Result<Self, DecodeError> {
        Self::start_inner(source, None, config)
    }

    fn start_inner(
        source: Box<dyn MediaSource>,
        stream: Option<StreamControl>,
        config: DecoderConfig,
    ) -> Result<Self, DecodeError> {
        config
            .processing
            .validate()
            .map_err(|error| DecodeError::new(DecodeErrorKind::Limit, error.to_string()))?;
        if config.queue_duration.is_zero()
            || config.queue_duration > Duration::from_millis(250)
            || config.prebuffer_duration.is_zero()
            || config.prebuffer_duration > config.queue_duration
            || config.readiness_timeout.is_zero()
        {
            return Err(DecodeError::new(
                DecodeErrorKind::Limit,
                "PCM queue must be bounded to 250 ms with a valid prebuffer",
            ));
        }
        let shared = Arc::new(Shared {
            state: Mutex::new(WorkerState::default()),
            pending_seek: Mutex::new(None),
            requested_seek: AtomicU64::new(0),
            committed_seek: AtomicU64::new(0),
            paused: AtomicBool::new(config.initially_paused),
            cancel: AtomicBool::new(false),
            phase: AtomicU8::new(PROBING),
            finished: AtomicBool::new(false),
            pending_processing: Mutex::new(None),
            requested_processing: AtomicU64::new(config.processing_sequence),
        });
        let worker_shared = shared.clone();
        let worker_stream = stream.clone();
        let (sender, ready) = mpsc::sync_channel(1);
        let permit = WorkerPermit::reserve(WorkerKind::Decoder)
            .map_err(|error| DecodeError::new(DecodeErrorKind::Limit, error.to_string()))?;
        let thread = thread::Builder::new()
            .name("audio-decode".into())
            .spawn(move || {
                super::scheduling::configure_audio_thread();
                let outcome = catch_unwind(AssertUnwindSafe(|| {
                    run_worker(
                        source,
                        worker_stream.as_ref(),
                        &config,
                        &worker_shared,
                        &sender,
                    )
                }));
                let error = match outcome {
                    Ok(Ok(())) => None,
                    Ok(Err(error)) => Some(error),
                    Err(_) => Some(DecodeError::new(
                        DecodeErrorKind::Panic,
                        "audio decoder worker panicked",
                    )),
                };
                if let Some(error) = error {
                    record_error(&worker_shared, error.clone());
                    let _ = sender.try_send(Err(error));
                }
                worker_shared.finished.store(true, Ordering::Release);
            })
            .map_err(|e| {
                DecodeError::new(
                    DecodeErrorKind::Limit,
                    format!("cannot start audio decoder: {e}"),
                )
            })?;
        Ok(Self {
            shared,
            stream,
            ready,
            thread: Some(permit.attach(thread)),
        })
    }

    pub fn poll_ready(&self) -> Option<Result<DecoderReady, DecodeError>> {
        self.ready.try_recv().ok()
    }
    pub fn snapshot(&self) -> DecoderSnapshot {
        let state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        let pcm = state.pcm.as_ref().map(|p| p.snapshot()).unwrap_or_default();
        let phase = self.shared.phase.load(Ordering::Acquire);
        let seek_seq = self.shared.committed_seek.load(Ordering::Acquire);
        let status = match phase {
            FAILED => DecoderStatus::Error,
            CANCELLED => DecoderStatus::Cancelled,
            _ if self.shared.requested_seek.load(Ordering::Acquire) > seek_seq => {
                DecoderStatus::Seeking
            }
            PROBING => DecoderStatus::Probing,
            SEEKING => DecoderStatus::Seeking,
            NORMAL_EOF if pcm.drained => DecoderStatus::Ended,
            _ => DecoderStatus::Ready,
        };
        DecoderSnapshot {
            status,
            seek_seq,
            format: state.format,
            duration: state.duration,
            pcm,
            error: state.error.clone(),
            worker_finished: self.thread.as_ref().is_none_or(TrackedWorker::is_finished),
        }
    }
    pub fn set_paused(&self, paused: bool) {
        self.shared.paused.store(paused, Ordering::Release);
        if let Some(pcm) = &self
            .shared
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pcm
        {
            pcm.set_paused(paused);
        }
        if let Some(thread) = &self.thread {
            thread.unpark();
        }
    }
    pub fn set_processing(
        &self,
        settings: PlaybackProcessingSettings,
        sequence: u64,
    ) -> Result<(), DecodeError> {
        settings
            .validate()
            .map_err(|error| DecodeError::new(DecodeErrorKind::Limit, error.to_string()))?;
        if self.shared.cancel.load(Ordering::Acquire)
            || self.shared.finished.load(Ordering::Acquire)
        {
            return Err(DecodeError::new(
                DecodeErrorKind::Cancelled,
                "audio decoder is closed",
            ));
        }
        let mut pending = self
            .shared
            .pending_processing
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if sequence <= self.shared.requested_processing.load(Ordering::Acquire) {
            return Ok(());
        }
        self.shared
            .requested_processing
            .store(sequence, Ordering::Release);
        *pending = Some((settings, sequence));
        drop(pending);
        if let Some(worker) = &self.thread {
            worker.unpark();
        }
        Ok(())
    }
    pub fn seek(&self, seq: u64, position: Duration) -> Result<(), DecodeError> {
        if self.shared.cancel.load(Ordering::Acquire)
            || self.shared.finished.load(Ordering::Acquire)
        {
            return Err(DecodeError::new(
                DecodeErrorKind::Cancelled,
                "audio decoder is closed",
            ));
        }
        let mut pending = self
            .shared
            .pending_seek
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if seq <= self.shared.requested_seek.load(Ordering::Acquire) {
            return Ok(());
        }
        self.shared.requested_seek.store(seq, Ordering::Release);
        *pending = Some((seq, position));
        self.shared.phase.store(SEEKING, Ordering::Release);
        let state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        let initialized = state.pcm.is_some();
        if let Some(pcm) = &state.pcm {
            pcm.set_seeking(true);
        }
        drop(state);
        // Publish the interrupt before allowing the worker to take this request.
        // A seek during initial probing is queued until a track is available.
        if initialized && let Some(stream) = &self.stream {
            stream.interrupt_read();
        }
        drop(pending);
        if let Some(thread) = &self.thread {
            thread.unpark();
        }
        Ok(())
    }
    pub fn cancel(&self) {
        self.shared.cancel.store(true, Ordering::Release);
        self.shared.phase.store(CANCELLED, Ordering::Release);
        if let Some(pcm) = &self
            .shared
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pcm
        {
            pcm.cancel();
        }
        if let Some(stream) = &self.stream {
            stream.cancel();
        }
        if let Some(thread) = &self.thread {
            thread.unpark();
        }
    }
    pub fn try_join(&mut self) -> bool {
        if self
            .thread
            .as_ref()
            .is_some_and(|thread| !thread.wait(Duration::ZERO))
        {
            return false;
        }
        self.thread.take();
        true
    }
    pub fn shutdown(&mut self, timeout: Duration) -> bool {
        let started = Instant::now();
        self.cancel();
        if self
            .thread
            .as_ref()
            .is_some_and(|worker| !worker.wait(timeout.saturating_sub(started.elapsed())))
        {
            return false;
        }
        self.thread.take();
        true
    }
}

impl Drop for DecoderHandle {
    fn drop(&mut self) {
        self.cancel();
    }
}

fn record_error(shared: &Shared, error: DecodeError) {
    let cancelled = error.kind == DecodeErrorKind::Cancelled;
    let mut state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(pcm) = &state.pcm {
        if cancelled {
            pcm.cancel();
        } else {
            pcm.fail();
        }
    }
    state.error = if cancelled { None } else { Some(error) };
    shared.phase.store(
        if cancelled { CANCELLED } else { FAILED },
        Ordering::Release,
    );
}

fn media_error(error: MediaIoError) -> DecodeError {
    let kind = match error.kind {
        MediaIoErrorKind::Cancelled => DecodeErrorKind::Cancelled,
        MediaIoErrorKind::Timeout => DecodeErrorKind::Timeout,
        MediaIoErrorKind::Network => DecodeErrorKind::Network,
        MediaIoErrorKind::Protocol => DecodeErrorKind::Protocol,
        MediaIoErrorKind::Storage => DecodeErrorKind::Storage,
        MediaIoErrorKind::Incomplete => DecodeErrorKind::Incomplete,
        MediaIoErrorKind::RepresentationChanged => DecodeErrorKind::RepresentationChanged,
    };
    DecodeError {
        kind,
        message: error.message.clone(),
        media: Some(error),
    }
}

fn translate_error(
    error: SymphoniaError,
    stream: Option<&StreamControl>,
    shared: &Shared,
) -> DecodeError {
    if shared.cancel.load(Ordering::Acquire) {
        return DecodeError::new(DecodeErrorKind::Cancelled, "audio decoding cancelled");
    }
    if let SymphoniaError::IoError(ref io) = error
        && let Some(media) = io.get_ref().and_then(|e| e.downcast_ref::<MediaIoError>())
    {
        if let Some(stream) = stream {
            let _ = stream.take_error();
        }
        return media_error(media.clone());
    }
    if let Some(media) = stream.and_then(|s| s.take_error()) {
        // A failed probe drops its media reader before this translation runs.
        // That reader's cancellation must not replace the codec's own error.
        if media.kind != MediaIoErrorKind::Cancelled {
            return media_error(media);
        }
    }
    let kind = match &error {
        SymphoniaError::Unsupported(_) => DecodeErrorKind::Unsupported,
        SymphoniaError::SeekError(_) => DecodeErrorKind::Seek,
        SymphoniaError::LimitError(_) => DecodeErrorKind::Limit,
        SymphoniaError::IoError(io) if io.kind() == ErrorKind::Unsupported => {
            DecodeErrorKind::Unsupported
        }
        SymphoniaError::IoError(io) if io.kind() == ErrorKind::UnexpectedEof => {
            DecodeErrorKind::Incomplete
        }
        SymphoniaError::IoError(_) => DecodeErrorKind::Network,
        _ => DecodeErrorKind::Decode,
    };
    DecodeError::new(kind, error.to_string())
}

struct DecoderSession {
    format_reader: Box<dyn FormatReader>,
    decoder: Box<dyn Decoder>,
    params: CodecParameters,
    track_id: u32,
    format: Option<PcmFormat>,
    samples: Option<SampleBuffer<f32>>,
    packet_start: u64,
    cursor: usize,
    skip_before: u64,
    last_frame_end: u64,
    eof: bool,
    demux_eof: bool,
}

impl DecoderSession {
    fn probe(stream: MediaSourceStream, hint: Option<&str>) -> Result<Self, SymphoniaError> {
        let mut hints = Hint::new();
        if let Some(extension) = hint {
            hints.with_extension(extension);
        }
        let probe = symphonia::default::get_probe().format(
            &hints,
            stream,
            &FormatOptions {
                enable_gapless: true,
                ..FormatOptions::default()
            },
            &MetadataOptions::default(),
        )?;
        let default_id = probe.format.default_track().map(|track| track.id);
        let tracks = probe.format.tracks();
        let mut selected = None;
        for track in tracks
            .iter()
            .filter(|track| Some(track.id) == default_id)
            .chain(tracks.iter().filter(|track| Some(track.id) != default_id))
        {
            if track.codec_params.codec == CODEC_TYPE_NULL {
                continue;
            }
            match symphonia::default::get_codecs()
                .make(&track.codec_params, &DecoderOptions::default())
            {
                Ok(decoder) => {
                    selected = Some((track.id, track.codec_params.clone(), decoder));
                    break;
                }
                Err(SymphoniaError::Unsupported(_)) => continue,
                Err(error) => return Err(error),
            }
        }
        let (track_id, params, decoder) =
            selected.ok_or(SymphoniaError::Unsupported("no supported audio track"))?;
        if params
            .max_frames_per_packet
            .is_some_and(|frames| frames > MAX_PACKET_FRAMES as u64)
        {
            return Err(SymphoniaError::LimitError(
                "audio codec packet exceeds PCM memory quota",
            ));
        }
        let format = params
            .sample_rate
            .zip(params.channels)
            .map(|(sample_rate, channels)| PcmFormat {
                sample_rate,
                channels: channels.count() as u16,
            });
        Ok(Self {
            format_reader: probe.format,
            decoder,
            params,
            track_id,
            format,
            samples: None,
            packet_start: 0,
            cursor: 0,
            skip_before: 0,
            last_frame_end: 0,
            eof: false,
            demux_eof: false,
        })
    }
    fn time_base(&self) -> Option<TimeBase> {
        self.params
            .time_base
            .or_else(|| self.format.map(|f| TimeBase::new(1, f.sample_rate)))
    }
    fn duration(&self) -> Option<Duration> {
        self.time_base()
            .zip(self.params.n_frames)
            .map(|(b, n)| b.calc_time(n).into())
    }
    fn timestamp_frame(&self, timestamp: u64, rate: u32) -> u64 {
        self.time_base()
            .map(|b| {
                let time = b.calc_time(timestamp.saturating_sub(self.params.start_ts));
                ((time.seconds as f64 + time.frac) * rate as f64).round() as u64
            })
            .unwrap_or(timestamp)
    }
    fn expected_end(&self) -> Option<u64> {
        self.params.n_frames.zip(self.format).and_then(|(n, f)| {
            n.checked_add(self.params.start_ts)
                .map(|ts| self.timestamp_frame(ts, f.sample_rate))
        })
    }
    fn has_pending(&self) -> bool {
        self.samples
            .as_ref()
            .is_some_and(|b| self.cursor < b.samples().len())
    }
    fn decode_next(&mut self) -> Result<bool, SymphoniaError> {
        self.demux_eof = false;
        loop {
            let packet = match self.format_reader.next_packet() {
                Ok(packet) => packet,
                Err(SymphoniaError::ResetRequired) => {
                    let track = self
                        .format_reader
                        .tracks()
                        .iter()
                        .find(|t| t.id == self.track_id)
                        .ok_or(SymphoniaError::Unsupported(
                            "selected audio track disappeared",
                        ))?;
                    self.params = track.codec_params.clone();
                    if self.format.is_some_and(|format| {
                        self.params
                            .channels
                            .is_some_and(|channels| channels.count() != format.channels as usize)
                            || self
                                .params
                                .sample_rate
                                .is_some_and(|rate| rate != format.sample_rate)
                    }) {
                        return Err(SymphoniaError::Unsupported(
                            "audio format changed during decoder reset",
                        ));
                    }
                    if self
                        .params
                        .max_frames_per_packet
                        .is_some_and(|frames| frames > MAX_PACKET_FRAMES as u64)
                    {
                        return Err(SymphoniaError::LimitError(
                            "audio codec packet exceeds PCM memory quota",
                        ));
                    }
                    self.decoder = symphonia::default::get_codecs()
                        .make(&self.params, &DecoderOptions::default())?;
                    continue;
                }
                Err(error) => {
                    self.demux_eof = matches!(&error, SymphoniaError::IoError(io) if io.kind() == ErrorKind::UnexpectedEof);
                    return Err(error);
                }
            };
            if packet.track_id() != self.track_id {
                continue;
            }
            // Bound packet size before handing untrusted data to a decoder.
            if packet.data.len() > 4 * 1024 * 1024 {
                return Err(SymphoniaError::LimitError(
                    "encoded audio packet exceeds 4 MiB",
                ));
            }
            let decoded = self.decoder.decode(&packet)?;
            let spec = *decoded.spec();
            let format = PcmFormat {
                sample_rate: spec.rate,
                channels: spec.channels.count() as u16,
            };
            if format.sample_rate == 0
                || format.channels == 0
                || format.channels as usize > MAX_CHANNELS
            {
                return Err(SymphoniaError::Unsupported(
                    "unsupported PCM channels or sample rate",
                ));
            }
            if self.format.is_some_and(|previous| previous != format) {
                return Err(SymphoniaError::Unsupported(
                    "audio format changed during playback",
                ));
            }
            let capacity = decoded.capacity();
            if capacity > MAX_PACKET_FRAMES {
                return Err(SymphoniaError::LimitError(
                    "decoded audio packet exceeds PCM memory quota",
                ));
            }
            self.format = Some(format);
            let packet_start = {
                let base = self
                    .params
                    .time_base
                    .unwrap_or(TimeBase::new(1, format.sample_rate));
                let time = base.calc_time(packet.ts().saturating_sub(self.params.start_ts));
                ((time.seconds as f64 + time.frac) * format.sample_rate as f64).round() as u64
            };
            let buffer = self
                .samples
                .get_or_insert_with(|| SampleBuffer::new(MAX_PACKET_FRAMES as u64, spec));
            buffer.copy_interleaved_ref(decoded);
            self.packet_start = packet_start;
            self.cursor = self
                .skip_before
                .saturating_sub(self.packet_start)
                .min((buffer.samples().len() / format.channels as usize) as u64)
                as usize
                * format.channels as usize;
            self.last_frame_end =
                self.packet_start + (buffer.samples().len() / format.channels as usize) as u64;
            return Ok(self.has_pending());
        }
    }
    fn seek(&mut self, target: Duration) -> Result<u64, SymphoniaError> {
        let rate = self
            .format
            .ok_or(SymphoniaError::Unsupported("audio format is unavailable"))?
            .sample_rate;
        let target = self
            .duration()
            .map_or(target, |duration| target.min(duration));
        let frame = (target.as_secs_f64() * rate as f64).round() as u64;
        if frame > MAX_CLOCK_FRAMES {
            return Err(SymphoniaError::LimitError("seek exceeds media clock range"));
        }
        let seek_target = if self.expected_end() == Some(frame) && frame > 0 {
            Duration::from_secs_f64((frame - 1) as f64 / rate as f64)
        } else {
            target
        };
        self.format_reader.seek(
            SeekMode::Accurate,
            SeekTo::Time {
                time: seek_target.into(),
                track_id: Some(self.track_id),
            },
        )?;
        self.decoder.reset();
        self.cursor = self.samples.as_ref().map_or(0, |b| b.samples().len());
        self.skip_before = frame;
        self.last_frame_end = frame;
        self.eof = false;
        Ok(frame)
    }
}

fn normal_eof(
    session: &DecoderSession,
    error: &SymphoniaError,
    stream: Option<&StreamControl>,
) -> bool {
    if !session.demux_eof {
        return false;
    }
    if !matches!(error, SymphoniaError::IoError(io) if io.kind() == ErrorKind::UnexpectedEof) {
        return false;
    }
    if stream.is_some_and(|s| s.status().error.is_some()) {
        return false;
    }
    session
        .expected_end()
        .is_none_or(|end| session.last_frame_end.saturating_add(1) >= end)
}

fn interrupted(error: &SymphoniaError) -> bool {
    matches!(error, SymphoniaError::IoError(io) if io.kind() == ErrorKind::Interrupted)
}

fn wait_transport_ready(
    stream: Option<&StreamControl>,
    config: &DecoderConfig,
    shared: &Shared,
) -> Result<(), DecodeError> {
    if let Some(stream) = stream {
        stream
            .wait_ready(config.readiness_timeout)
            .map_err(|error| {
                stream.take_error().map(media_error).unwrap_or_else(|| {
                    DecodeError::new(
                        if shared.cancel.load(Ordering::Acquire) {
                            DecodeErrorKind::Cancelled
                        } else {
                            DecodeErrorKind::Network
                        },
                        error.to_string(),
                    )
                })
            })?;
    }
    Ok(())
}

fn run_worker(
    source: Box<dyn MediaSource>,
    stream: Option<&StreamControl>,
    config: &DecoderConfig,
    shared: &Shared,
    ready: &mpsc::SyncSender<Result<DecoderReady, DecodeError>>,
) -> Result<(), DecodeError> {
    wait_transport_ready(stream, config, shared)?;
    if shared.cancel.load(Ordering::Acquire) {
        return Err(DecodeError::new(
            DecodeErrorKind::Cancelled,
            "audio decoding cancelled",
        ));
    }
    let media = MediaSourceStream::new(source, MediaSourceStreamOptions::default());
    let mut session = DecoderSession::probe(media, config.extension_hint.as_deref())
        .map_err(|error| translate_error(error, stream, shared))?;
    let reserved_channels = session
        .params
        .channels
        .map_or(MAX_CHANNELS, |channels| channels.count());
    if reserved_channels == 0 || reserved_channels > MAX_CHANNELS {
        return Err(DecodeError::new(
            DecodeErrorKind::Unsupported,
            "unsupported PCM channel count",
        ));
    }
    let mut scratch = PcmReservation::new(MAX_PACKET_FRAMES * reserved_channels * 12)
        .map_err(|e| DecodeError::new(DecodeErrorKind::Limit, e.to_string()))?;
    // Some compressed formats report rate and channels only after the first packet.
    loop {
        if shared.cancel.load(Ordering::Acquire) {
            return Err(DecodeError::new(
                DecodeErrorKind::Cancelled,
                "audio decoding cancelled",
            ));
        }
        match session.decode_next() {
            Ok(true) => break,
            Ok(false) => continue,
            Err(error) if normal_eof(&session, &error, stream) && session.format.is_some() => {
                session.eof = true;
                break;
            }
            Err(error) => return Err(translate_error(error, stream, shared)),
        }
    }
    let format = session.format.ok_or_else(|| {
        DecodeError::new(DecodeErrorKind::Unsupported, "audio format is unavailable")
    })?;
    let mut processing = WorkerProcessor::new(format)
        .map_err(|error| DecodeError::new(DecodeErrorKind::Limit, error.to_string()))?;
    processing
        .set_settings(config.processing, config.processing_sequence)
        .map_err(|error| DecodeError::new(DecodeErrorKind::Limit, error.to_string()))?;
    let frames = (config.queue_duration.as_secs_f64() * format.sample_rate as f64).ceil() as usize;
    let prebuffer =
        (config.prebuffer_duration.as_secs_f64() * format.sample_rate as f64).ceil() as usize;
    let scratch_bytes = MAX_PACKET_FRAMES * format.channels as usize * 12;
    scratch
        .shrink_to(scratch_bytes)
        .map_err(|e| DecodeError::new(DecodeErrorKind::Limit, e.to_string()))?;
    let (mut producer, source, pcm) =
        PcmProducer::with_reservation(format, frames.max(1), scratch, session.duration())
            .map_err(|e| DecodeError::new(DecodeErrorKind::Limit, e.to_string()))?;
    pcm.set_paused(shared.paused.load(Ordering::Acquire));
    {
        let mut state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
        state.pcm = Some(pcm.clone());
        state.format = Some(format);
        state.duration = session.duration();
    }
    let mut source = Some(source);
    let mut pending_commit: Option<(u64, u64)> = None;
    let mut delivered_ready = false;
    let mut pending_processed: Option<ProcessedFrame> = None;
    loop {
        if shared.cancel.load(Ordering::Acquire) {
            return Err(DecodeError::new(
                DecodeErrorKind::Cancelled,
                "audio decoding cancelled",
            ));
        }
        if let Some(error) = stream.and_then(StreamControl::terminal_error) {
            return Err(media_error(error));
        }
        if let Some((settings, sequence)) = shared
            .pending_processing
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take()
        {
            processing
                .set_settings(settings, sequence)
                .map_err(|error| DecodeError::new(DecodeErrorKind::Limit, error.to_string()))?;
        }
        let request = shared
            .pending_seek
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        if let Some((seq, position)) = request {
            shared.phase.store(SEEKING, Ordering::Release);
            producer
                .begin_epoch()
                .map_err(|e| DecodeError::new(DecodeErrorKind::Limit, e.to_string()))?;
            match session.seek(position) {
                Ok(frame) => {
                    processing.reset_seek(frame);
                    pending_processed = None;
                    pending_commit = Some((seq, frame));
                }
                Err(error)
                    if interrupted(&error)
                        && shared.requested_seek.load(Ordering::Acquire) != seq =>
                {
                    continue;
                }
                Err(error) => return Err(translate_error(error, stream, shared)),
            }
        }
        let can_fill =
            !delivered_ready || pending_commit.is_some() || !shared.paused.load(Ordering::Acquire);
        if can_fill && producer.queued_frames() < producer.capacity_frames() {
            // Publish prepared output before any new potentially blocking media read.
            let requested_seek = shared.requested_seek.load(Ordering::Acquire);
            while producer.queued_frames() < producer.capacity_frames() {
                if shared.cancel.load(Ordering::Acquire)
                    || shared.requested_seek.load(Ordering::Acquire) != requested_seek
                {
                    break;
                }
                let processed = pending_processed.take().or_else(|| {
                    processing.pop_output_interruptible(|| {
                        shared.cancel.load(Ordering::Acquire)
                            || shared.requested_seek.load(Ordering::Acquire) != requested_seek
                    })
                });
                if let Some(processed) = processed {
                    if !producer.push_processed_frame(
                        processed.stamp,
                        &processed.samples[..format.channels as usize],
                    ) {
                        pending_processed = Some(processed);
                        break;
                    }
                } else {
                    break;
                }
            }
            if !session.has_pending()
                && !session.eof
                && producer.queued_frames() < producer.capacity_frames()
            {
                match session.decode_next() {
                    Ok(_) => {}
                    Err(error) if normal_eof(&session, &error, stream) => {
                        session.eof = true;
                        processing.finish();
                    }
                    Err(error)
                        if interrupted(&error)
                            && (shared.cancel.load(Ordering::Acquire)
                                || shared
                                    .pending_seek
                                    .lock()
                                    .unwrap_or_else(|e| e.into_inner())
                                    .is_some()) =>
                    {
                        continue;
                    }
                    Err(error) => {
                        let error = translate_error(error, stream, shared);
                        if error.kind == DecodeErrorKind::RepresentationChanged {
                            shared.phase.store(SEEKING, Ordering::Release);
                            producer.begin_epoch().map_err(|e| {
                                DecodeError::new(DecodeErrorKind::Limit, e.to_string())
                            })?;
                            let mut media = session.format_reader.into_inner();
                            media.seek(SeekFrom::Start(0)).map_err(|e| {
                                DecodeError::new(DecodeErrorKind::Network, e.to_string())
                            })?;
                            wait_transport_ready(stream, config, shared)?;
                            session =
                                DecoderSession::probe(media, config.extension_hint.as_deref())
                                    .map_err(|e| translate_error(e, stream, shared))?;
                            if session
                                .params
                                .channels
                                .is_none_or(|channels| channels.count() != format.channels as usize)
                                || session
                                    .params
                                    .sample_rate
                                    .is_some_and(|rate| rate != format.sample_rate)
                            {
                                return Err(DecodeError::new(
                                    DecodeErrorKind::Unsupported,
                                    "audio representation changed its PCM format",
                                ));
                            }
                            pending_commit =
                                Some((shared.requested_seek.load(Ordering::Acquire), 0));
                            processing.reset_seek(0);
                            pending_processed = None;
                            continue;
                        }
                        return Err(error);
                    }
                }
            }
            if let Some(buffer) = &session.samples {
                if session.format != Some(format) {
                    return Err(DecodeError::new(
                        DecodeErrorKind::Unsupported,
                        "audio representation changed its PCM format",
                    ));
                }
                let channels = format.channels as usize;
                while producer.queued_frames() < producer.capacity_frames() {
                    if shared.cancel.load(Ordering::Acquire)
                        || shared
                            .pending_seek
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .is_some()
                    {
                        break;
                    }
                    let processed = pending_processed.take().or_else(|| {
                        processing.pop_output_interruptible(|| {
                            shared.cancel.load(Ordering::Acquire)
                                || shared.requested_seek.load(Ordering::Acquire) != requested_seek
                        })
                    });
                    if let Some(processed) = processed {
                        if !producer
                            .push_processed_frame(processed.stamp, &processed.samples[..channels])
                        {
                            pending_processed = Some(processed);
                            break;
                        }
                    } else if session.cursor + channels <= buffer.samples().len() {
                        let frame = session.packet_start + (session.cursor / channels) as u64;
                        if !processing.push_input(
                            frame,
                            &buffer.samples()[session.cursor..session.cursor + channels],
                        ) {
                            if let Some(error) = processing.input_error() {
                                return Err(DecodeError::new(DecodeErrorKind::Decode, error));
                            }
                            if processing.input_is_full() {
                                return Err(DecodeError::new(
                                    DecodeErrorKind::Limit,
                                    "tempo input queue cannot make bounded progress",
                                ));
                            }
                            break;
                        }
                        session.cursor += channels;
                    } else {
                        break;
                    }
                }
            }
        }
        if session.eof {
            processing.finish();
            if processing.drained() && pending_processed.is_none() {
                producer.finish();
            }
        }
        if producer.queued_frames() >= prebuffer || session.eof {
            if let Some((seq, frame)) = pending_commit {
                let pending = shared
                    .pending_seek
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                if pending.is_none() && shared.requested_seek.load(Ordering::Acquire) == seq {
                    producer
                        .commit_epoch(frame)
                        .map_err(|e| DecodeError::new(DecodeErrorKind::Limit, e.to_string()))?;
                    shared.committed_seek.store(seq, Ordering::Release);
                    pending_commit = None;
                    shared.phase.store(
                        if session.eof { NORMAL_EOF } else { READY },
                        Ordering::Release,
                    );
                }
            }
            if !delivered_ready && pending_commit.is_none() {
                let source = source.take().expect("PCM source is delivered once");
                ready
                    .try_send(Ok(DecoderReady {
                        source,
                        format,
                        duration: session.duration(),
                    }))
                    .map_err(|_| {
                        DecodeError::new(
                            DecodeErrorKind::Cancelled,
                            "audio output owner was dropped",
                        )
                    })?;
                delivered_ready = true;
                shared.phase.store(
                    if session.eof { NORMAL_EOF } else { READY },
                    Ordering::Release,
                );
            }
        }
        if delivered_ready && !pcm.consumer_alive() {
            if session.eof && pcm.snapshot().drained {
                shared.phase.store(NORMAL_EOF, Ordering::Release);
                return Ok(());
            }
            return Err(DecodeError::new(
                DecodeErrorKind::Cancelled,
                "audio output source was dropped",
            ));
        }
        if session.eof && pending_commit.is_none() {
            shared.phase.store(NORMAL_EOF, Ordering::Release);
        }
        if session.eof || !can_fill || producer.queued_frames() == producer.capacity_frames() {
            thread::park_timeout(Duration::from_millis(2));
        }
    }
}

#[cfg(test)]
mod lifecycle_tests {
    use super::*;
    use std::io::{self, Read};

    struct BlockedSource {
        entered: Option<mpsc::SyncSender<()>>,
        release: Mutex<mpsc::Receiver<()>>,
        dropped: Arc<AtomicBool>,
    }

    impl Read for BlockedSource {
        fn read(&mut self, _buffer: &mut [u8]) -> io::Result<usize> {
            if let Some(entered) = self.entered.take() {
                let _ = entered.send(());
                let _ = self
                    .release
                    .get_mut()
                    .unwrap_or_else(|error| error.into_inner())
                    .recv();
            }
            Ok(0)
        }
    }

    impl Seek for BlockedSource {
        fn seek(&mut self, _position: SeekFrom) -> io::Result<u64> {
            Ok(0)
        }
    }

    impl MediaSource for BlockedSource {
        fn is_seekable(&self) -> bool {
            false
        }

        fn byte_len(&self) -> Option<u64> {
            None
        }
    }

    impl Drop for BlockedSource {
        fn drop(&mut self) {
            self.dropped.store(true, Ordering::Release);
        }
    }

    struct ReleaseOnDrop(mpsc::SyncSender<()>);

    impl Drop for ReleaseOnDrop {
        fn drop(&mut self) {
            let _ = self.0.try_send(());
        }
    }

    fn blocked_decoder() -> (
        DecoderHandle,
        mpsc::Receiver<()>,
        ReleaseOnDrop,
        Arc<AtomicBool>,
    ) {
        let (entered, receiver) = mpsc::sync_channel(1);
        let (release, gate) = mpsc::sync_channel(1);
        let dropped = Arc::new(AtomicBool::new(false));
        let source = BlockedSource {
            entered: Some(entered),
            release: Mutex::new(gate),
            dropped: dropped.clone(),
        };
        let decoder =
            DecoderHandle::start_source(Box::new(source), DecoderConfig::default()).unwrap();
        (decoder, receiver, ReleaseOnDrop(release), dropped)
    }

    #[test]
    fn blocked_read_timeout_retains_decoder_for_a_later_actual_join() {
        let (mut decoder, entered, release, source_dropped) = blocked_decoder();
        entered.recv_timeout(Duration::from_secs(1)).unwrap();
        let started = Instant::now();
        assert!(!decoder.shutdown(Duration::from_millis(5)));
        assert!(started.elapsed() < Duration::from_millis(100));
        assert!(decoder.thread.is_some());
        assert!(!decoder.snapshot().worker_finished);
        assert!(!source_dropped.load(Ordering::Acquire));
        drop(release);
        assert!(decoder.shutdown(Duration::from_secs(1)));
        assert!(decoder.thread.is_none());
        assert!(decoder.snapshot().worker_finished);
        assert!(source_dropped.load(Ordering::Acquire));
    }

    #[test]
    fn decoder_drop_only_cancels_and_signals_while_a_read_is_blocked() {
        let (decoder, entered, release, source_dropped) = blocked_decoder();
        entered.recv_timeout(Duration::from_secs(1)).unwrap();
        let owner = decoder.thread.as_ref().unwrap().clone();
        let cancelled = decoder.shared.clone();
        let started = Instant::now();
        drop(decoder);
        assert!(started.elapsed() < Duration::from_millis(100));
        assert!(cancelled.cancel.load(Ordering::Acquire));
        assert!(!owner.wait(Duration::ZERO));
        assert!(!source_dropped.load(Ordering::Acquire));
        drop(release);
        assert!(owner.wait(Duration::from_secs(1)));
        assert!(source_dropped.load(Ordering::Acquire));
    }

    #[test]
    fn processed_short_eof_drains_exact_output_without_advancing_deferred_clock_and_releases_quota()
    {
        let baseline = super::super::processing::DspReservation::used_bytes();
        let original_frames = 800u32;
        let data_len = original_frames * 2;
        let mut wav = Vec::new();
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&(36 + data_len).to_le_bytes());
        wav.extend_from_slice(b"WAVEfmt ");
        wav.extend_from_slice(&16u32.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes());
        wav.extend_from_slice(&8_000u32.to_le_bytes());
        wav.extend_from_slice(&16_000u32.to_le_bytes());
        wav.extend_from_slice(&2u16.to_le_bytes());
        wav.extend_from_slice(&16u16.to_le_bytes());
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&data_len.to_le_bytes());
        for frame in 0..original_frames {
            let sample =
                ((frame as f64 * std::f64::consts::TAU * 997.0 / 8_000.0).sin() * 1_000.0) as i16;
            wav.extend_from_slice(&sample.to_le_bytes());
        }
        let config = DecoderConfig {
            processing: PlaybackProcessingSettings {
                playback_rate: 0.5,
                preserves_pitch: false,
                pitch_ratio: 1.0,
            },
            processing_sequence: 7,
            ..DecoderConfig::default()
        };
        let mut decoder =
            DecoderHandle::start_source(Box::new(io::Cursor::new(wav)), config).unwrap();
        let started = Instant::now();
        let mut ready = loop {
            if let Some(ready) = decoder.poll_ready() {
                break ready.unwrap();
            }
            assert!(started.elapsed() < Duration::from_secs(5));
            thread::sleep(Duration::from_millis(1));
        };
        let control = ready.source.control();
        ready.source.set_deferred_clock(true);
        while !control.snapshot().normal_eof {
            assert!(started.elapsed() < Duration::from_secs(5));
            thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(control.snapshot().position_seconds, 0.0);
        let mut presented = None;
        let mut count = 0;
        while ready.source.next().is_some() {
            count += 1;
            presented = ready.source.current_source_stamp();
        }
        assert_eq!(count, original_frames as usize * 2);
        assert_eq!(control.snapshot().position_seconds, 0.0);
        let (stamp, epoch) = presented.unwrap();
        assert_eq!(stamp.end_frame, original_frames as u64);
        assert_eq!(stamp.fraction, 0);
        assert_eq!(stamp.processing_sequence, 7);
        assert_eq!(stamp.settings.playback_rate, 0.5);
        assert!(control.commit_rendered_stamp(stamp, epoch));
        assert!((control.snapshot().position_seconds - 0.1).abs() < 1e-12);
        drop(ready);
        assert!(decoder.shutdown(Duration::from_secs(2)));
        assert_eq!(
            super::super::processing::DspReservation::used_bytes(),
            baseline
        );
    }
}

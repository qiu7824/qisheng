//! Feed measurements through actual decoding, PCM ownership and Memory output.

use crate::{
    audio::{
        RenderedPcm,
        cache::CacheOptions,
        decode::{DecoderConfig, DecoderHandle, DecoderSnapshot, DecoderStatus},
        output::{Output, OutputRenderTiming},
        transport::{
            ApprovedAudioRequest, AudioTransport, CacheIdentity, StreamControl, StreamStatus,
            TransportOptions,
        },
    },
    catalog::Catalog,
    domain::playback_processing::PlaybackProcessingSettings,
};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::Cursor,
    sync::{Arc, Mutex, atomic::AtomicBool},
    thread,
    time::{Duration, Instant},
};

const SOURCE_RATE: u32 = 48_000;
const SOURCE_CHANNELS: u16 = 2;
const OUTPUT_RATE: u32 = 22_050;
const OUTPUT_BATCH_FRAMES: u64 = 441;
const TONE_HZ: f64 = 1733.0;
const SEQUENCE: u64 = 3;

#[derive(Clone, Copy, Debug)]
enum FeedKind {
    DirectCursor,
    CatalogRangeCache,
}

struct FeedOwner {
    decoder: DecoderHandle,
    output: Output,
    observation: Arc<Mutex<RenderedPcm>>,
    stream: Option<StreamControl>,
    transport: Option<Arc<AudioTransport>>,
    catalog: Option<Catalog>,
    _directory: tempfile::TempDir,
}

impl Drop for FeedOwner {
    fn drop(&mut self) {
        self.output.clear();
        self.decoder.cancel();
        if let Some(stream) = &self.stream {
            stream.cancel();
        }
        let output_joined = self.output.shutdown(Duration::from_secs(5));
        let decoder_joined = self.decoder.shutdown(Duration::from_secs(5));
        let stream_joined = self
            .stream
            .as_ref()
            .is_none_or(|stream| stream.join(Duration::from_secs(5)));
        let transport_joined = self
            .transport
            .as_ref()
            .is_none_or(|transport| transport.shutdown(Duration::from_secs(5)));
        drop(self.catalog.take());
        if !thread::panicking() {
            assert!(
                output_joined && decoder_joined && stream_joined && transport_joined,
                "actual feed owners did not join: output={output_joined}, decoder={decoder_joined}, stream={stream_joined}, transport={transport_joined}"
            );
        }
    }
}

fn waveform() -> Vec<u8> {
    let frames = SOURCE_RATE as usize * 8;
    let bytes = (frames * SOURCE_CHANNELS as usize * 4) as u32;
    let mut wave = Vec::with_capacity(44 + bytes as usize);
    wave.extend_from_slice(b"RIFF");
    wave.extend_from_slice(&(36 + bytes).to_le_bytes());
    wave.extend_from_slice(b"WAVEfmt ");
    wave.extend_from_slice(&16_u32.to_le_bytes());
    wave.extend_from_slice(&3_u16.to_le_bytes());
    wave.extend_from_slice(&SOURCE_CHANNELS.to_le_bytes());
    wave.extend_from_slice(&SOURCE_RATE.to_le_bytes());
    wave.extend_from_slice(&(SOURCE_RATE * u32::from(SOURCE_CHANNELS) * 4).to_le_bytes());
    wave.extend_from_slice(&(SOURCE_CHANNELS * 4).to_le_bytes());
    wave.extend_from_slice(&32_u16.to_le_bytes());
    wave.extend_from_slice(b"data");
    wave.extend_from_slice(&bytes.to_le_bytes());
    for frame in 0..frames {
        let sample = (0.02
            * (std::f64::consts::TAU * TONE_HZ * frame as f64 / SOURCE_RATE as f64).cos())
            as f32;
        for _ in 0..SOURCE_CHANNELS {
            wave.extend_from_slice(&sample.to_le_bytes());
        }
    }
    wave
}

fn start_feed(kind: FeedKind, queue_ms: u64) -> FeedOwner {
    let directory = tempfile::Builder::new()
        .prefix("lx-actual-feed-")
        .tempdir()
        .unwrap();
    let config = DecoderConfig {
        queue_duration: Duration::from_millis(queue_ms),
        prebuffer_duration: Duration::from_millis(40),
        initially_paused: true,
        ..DecoderConfig::default()
    };
    let bytes = waveform();
    let (decoder, stream, transport, catalog) = match kind {
        FeedKind::DirectCursor => (
            DecoderHandle::start_source(Box::new(Cursor::new(bytes)), config).unwrap(),
            None,
            None,
            None,
        ),
        FeedKind::CatalogRangeCache => {
            let media_sha = format!("{:x}", Sha256::digest(&bytes));
            let path = directory.path().join("original-feed.wav");
            fs::write(&path, bytes).unwrap();
            let catalog = Catalog::new().unwrap();
            let track = catalog.import_local(path).unwrap();
            let options = TransportOptions {
                cache: CacheOptions {
                    root: directory.path().join("encoded-cache"),
                    ..CacheOptions::default()
                },
                ..TransportOptions::default()
            };
            let transport = AudioTransport::new(options).unwrap();
            let request = ApprovedAudioRequest::new(
                &track.source_url,
                CacheIdentity {
                    track_id: track.id,
                    platform: "local".into(),
                    actual_quality: "original".into(),
                    media_context_sha256: media_sha,
                    approval_id: "explicit-local-feed".into(),
                    ..CacheIdentity::default()
                },
            )
            .unwrap();
            let lease = transport
                .open_speculative(request, Arc::new(AtomicBool::new(false)), 2 * 1024 * 1024)
                .unwrap();
            let control = lease.control.clone();
            (
                DecoderHandle::start(lease, config).unwrap(),
                Some(control),
                Some(transport),
                Some(catalog),
            )
        }
    };
    let observation = Arc::new(Mutex::new(RenderedPcm::default()));
    let mut owner = FeedOwner {
        decoder,
        output: Output::memory(observation.clone()).unwrap(),
        observation,
        stream,
        transport,
        catalog,
        _directory: directory,
    };
    owner.output.set_paused(true);
    let deadline = Instant::now() + Duration::from_secs(15);
    let ready = loop {
        if let Some(result) = owner.decoder.poll_ready() {
            break result.unwrap();
        }
        let snapshot = owner.decoder.snapshot();
        assert_ne!(
            snapshot.status,
            DecoderStatus::Error,
            "actual decoder failed before readiness"
        );
        assert!(
            Instant::now() < deadline,
            "actual decoder did not produce its ready owner"
        );
        thread::sleep(Duration::from_millis(1));
    };
    let prepared_epoch = ready.source.control().current_epoch();
    let prepared_capacity = owner.decoder.snapshot().pcm.capacity_frames;
    assert_eq!(
        prepared_capacity,
        (SOURCE_RATE as u64 * queue_ms / 1000) as usize
    );
    let wanted = PlaybackProcessingSettings {
        playback_rate: 2.0,
        preserves_pitch: true,
        pitch_ratio: 1.5,
    };
    owner.decoder.set_processing(wanted, SEQUENCE).unwrap();
    owner.output.set_processing(wanted, SEQUENCE).unwrap();
    if let Some(stream) = &owner.stream {
        stream.promote_to_foreground().unwrap();
    }
    let attached = owner.output.attach(ready.source).unwrap();
    assert_eq!(attached.current_epoch(), prepared_epoch);
    assert_eq!(attached.snapshot().capacity_frames, prepared_capacity);
    owner.decoder.set_paused(false);
    owner.output.set_paused(false);
    loop {
        let processing = owner.output.processing_snapshot();
        let pcm = owner.observation.lock().unwrap().clone();
        if processing.applied == wanted
            && processing.applied_sequence == SEQUENCE
            && processing.transition_remaining_frames == 0
            && pcm.peak > 0.001
        {
            break;
        }
        assert_ne!(owner.decoder.snapshot().status, DecoderStatus::Error);
        assert!(
            Instant::now() < deadline,
            "actual feed did not acknowledge presented PCM"
        );
        thread::sleep(Duration::from_millis(1));
    }
    owner
}

struct Observation {
    pcm: RenderedPcm,
    decoder: DecoderSnapshot,
    stream: Option<StreamStatus>,
    timing: OutputRenderTiming,
}

fn observe(owner: &FeedOwner, deadline: Instant) -> Observation {
    loop {
        let before = owner.observation.lock().unwrap().clone();
        let decoder = owner.decoder.snapshot();
        let stream = owner.stream.as_ref().map(StreamControl::status);
        let timing = owner.output.memory_render_timing().unwrap();
        let after = owner.observation.lock().unwrap().clone();
        if before.batches == after.batches {
            return Observation {
                pcm: after,
                decoder,
                stream,
                timing,
            };
        }
        assert!(
            Instant::now() < deadline,
            "actual feed observation could not bracket a batch"
        );
        thread::sleep(Duration::from_millis(1));
    }
}

struct FeedEvidence {
    kind: FeedKind,
    queue_ms: u64,
    output_seconds: f64,
    media_seconds: f64,
    consumed_frames: u64,
    underflow_frames: u64,
    frequencies: Vec<f64>,
}

fn measure(kind: FeedKind, queue_ms: u64) -> FeedEvidence {
    let owner = start_feed(kind, queue_ms);
    let deadline = Instant::now() + Duration::from_secs(10);
    let start = observe(&owner, deadline);
    let mut frequencies = Vec::new();
    let mut last_batch = start.pcm.batches;
    let end = loop {
        let current = owner.observation.lock().unwrap().clone();
        if current.batches != last_batch {
            last_batch = current.batches;
            if current.peak > 0.001
                && let Some(frequency) = current.frequency_hz.filter(|value| value.is_finite())
            {
                frequencies.push(frequency);
            }
        }
        if current.batches >= start.pcm.batches + 50 {
            break observe(&owner, deadline);
        }
        assert_ne!(owner.decoder.snapshot().status, DecoderStatus::Error);
        assert!(
            Instant::now() < deadline,
            "actual renderer did not complete the feed window"
        );
        thread::sleep(Duration::from_millis(1));
    };
    assert_eq!(start.decoder.pcm.epoch, end.decoder.pcm.epoch);
    assert_eq!(
        start.decoder.pcm.capacity_frames,
        end.decoder.pcm.capacity_frames
    );
    assert_eq!(
        end.decoder.pcm.capacity_frames,
        (SOURCE_RATE as u64 * queue_ms / 1000) as usize
    );
    if let (Some(first), Some(last)) = (&start.stream, &end.stream) {
        assert_eq!(first.source_serial, last.source_serial);
        assert_eq!(first.representation_epoch, last.representation_epoch);
        assert!(last.range_supported);
        assert!(last.encoded_budget_bytes.is_none());
        assert!(!last.budget_blocked);
    }
    let evidence = FeedEvidence {
        kind,
        queue_ms,
        output_seconds: (end.pcm.batches - start.pcm.batches) as f64 * OUTPUT_BATCH_FRAMES as f64
            / OUTPUT_RATE as f64,
        media_seconds: end.decoder.pcm.position_seconds - start.decoder.pcm.position_seconds,
        consumed_frames: end.decoder.pcm.consumed_frames - start.decoder.pcm.consumed_frames,
        underflow_frames: end.decoder.pcm.underflow_frames - start.decoder.pcm.underflow_frames,
        frequencies,
    };
    let safe_start = start
        .stream
        .as_ref()
        .map(|stream| super::AudioSupplySnapshot::observe(0, None, &start.decoder, stream));
    let safe_end = end
        .stream
        .as_ref()
        .map(|stream| super::AudioSupplySnapshot::observe(0, None, &end.decoder, stream));
    eprintln!(
        "actual feed: kind={kind:?} queue_ms={queue_ms} output_seconds={:.6} media_seconds={:.6} produced_delta={} consumed_delta={} underflow_delta={} queue_start={} queue_end={} render_start={:?} render_end={:?} stream_start={:?} stream_end={:?}",
        evidence.output_seconds,
        evidence.media_seconds,
        end.decoder.pcm.produced_frames - start.decoder.pcm.produced_frames,
        evidence.consumed_frames,
        evidence.underflow_frames,
        start.decoder.pcm.queued_frames,
        end.decoder.pcm.queued_frames,
        start.timing,
        end.timing,
        safe_start,
        safe_end,
    );
    evidence
}

#[test]
fn actual_cursor_and_catalog_feed_at_prepared_and_foreground_pcm_capacities() {
    let mut evidence = Vec::new();
    for kind in [FeedKind::DirectCursor, FeedKind::CatalogRangeCache] {
        for queue_ms in [80, 250] {
            evidence.push(measure(kind, queue_ms));
        }
    }
    for mut measured in evidence {
        measured.frequencies.sort_by(f64::total_cmp);
        assert!(
            measured.frequencies.len() >= 10,
            "actual feed lost too many observable tone batches"
        );
        let actual_frequency = measured.frequencies[measured.frequencies.len() / 2];
        let expected_frequency = TONE_HZ * 1.5;
        let frequency_tolerance = 0.75 * OUTPUT_RATE as f64 / 4096.0 + 2.0;
        assert!(
            (actual_frequency - expected_frequency).abs() <= frequency_tolerance,
            "actual feed pitch differs: kind={:?}, queue_ms={}, measured={actual_frequency}, expected={expected_frequency}",
            measured.kind,
            measured.queue_ms
        );
        let consumed_media = measured.consumed_frames as f64 * 2.0 / SOURCE_RATE as f64;
        // A bracket can include the in-flight 20 ms callback. This bound only
        // compares two real counters; it does not declare missing PCM presented.
        let counter_boundary = 2.0 * OUTPUT_BATCH_FRAMES as f64 / OUTPUT_RATE as f64 * 2.0;
        assert!(
            (measured.media_seconds - consumed_media).abs() <= counter_boundary,
            "actual feed clock does not follow consumed PCM: kind={:?}, queue_ms={}, media={}, consumed_media={consumed_media}",
            measured.kind,
            measured.queue_ms,
            measured.media_seconds
        );
        {
            let expected_media = measured.output_seconds * 2.0;
            let tolerance = (expected_media * 0.10).max(0.08);
            assert!(
                (measured.media_seconds - expected_media).abs() <= tolerance,
                "actual feed starved: kind={:?}, queue_ms={}, media={}, expected={expected_media}, underflow={}",
                measured.kind,
                measured.queue_ms,
                measured.media_seconds,
                measured.underflow_frames
            );
        }
    }
}

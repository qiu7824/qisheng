use lx_gpui_player::{
    audio::{PlaybackStatus, RenderedPcm},
    backend::{
        BackendCommand, BackendConfig, BackendEvent, BackendHandle, BackendQuery,
        BackendQueryResult, BackendSnapshot, CommandEnvelope,
    },
    domain::{
        music::TrackRecord,
        playback_processing::{PlaybackProcessingSettings, PlaybackProcessingSnapshot},
    },
    playback::queue::PlayMode,
    repository::local_track_from_path,
};
use std::{
    fs,
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant},
};

pub const OUTPUT_RATE: u32 = 22_050;
pub const BATCH_FRAMES: u64 = 441;
pub const FREQUENCY: f64 = 997.0;
pub const AMPLITUDE: f64 = 0.02;

pub struct Running {
    pub directory: tempfile::TempDir,
    pub backend: BackendHandle,
    pub record: TrackRecord,
    pub source_rate: u32,
    pub channels: u16,
    pub duration_seconds: f64,
}
impl Running {
    pub fn new(source_rate: u32, channels: u16, duration_seconds: f64) -> Self {
        Self::in_directory(
            temporary_directory(),
            source_rate,
            channels,
            duration_seconds,
        )
    }
    pub fn in_directory(
        directory: tempfile::TempDir,
        source_rate: u32,
        channels: u16,
        duration_seconds: f64,
    ) -> Self {
        let path = directory.path().join("original-processing-tone.wav");
        wave(&path, source_rate, channels, duration_seconds, FREQUENCY);
        let (record, _) = local_track_from_path(&path).unwrap();
        let backend =
            BackendHandle::start(BackendConfig::headless(directory.path().join("profile")))
                .unwrap();
        let running = Self {
            directory,
            backend,
            record,
            source_rate,
            channels,
            duration_seconds,
        };
        let cursor = running.backend.event_cursor();
        running.execute(
            "import-processing-tone",
            BackendCommand::ImportAudio { paths: vec![path] },
        );
        terminal(&running.backend, cursor, "import-processing-tone");
        running.execute("processing-volume", BackendCommand::Volume { volume: 1.0 });
        running.execute(
            "processing-no-advance",
            BackendCommand::SetPlayMode {
                mode: PlayMode::None,
            },
        );
        running.execute(
            "processing-no-error-skip",
            BackendCommand::SetAutoSkipOnError { enabled: false },
        );
        running
    }
    pub fn profile(&self) -> PathBuf {
        self.directory.path().join("profile")
    }
    pub fn execute(&self, id: &str, command: BackendCommand) {
        self.backend.execute(envelope(id, command)).unwrap();
    }
    pub fn play(&self, id: &str) -> BackendSnapshot {
        self.execute(
            id,
            BackendCommand::Play {
                id: self.record.id.clone(),
            },
        );
        let playing = wait(&self.backend, 15.0, |state| {
            state.playback.status == PlaybackStatus::Playing
                && state.playback.track_id.as_deref() == Some(self.record.id.as_str())
                && state.playback.position_seconds > 0.03
                && pcm(&self.backend).peak > 0.001
        });
        assert!(playing.playback.duration_known);
        assert!((playing.playback.duration_seconds - self.duration_seconds).abs() < 0.01);
        assert!(playing.approved_audio_identity.is_some());
        playing
    }
    pub fn next_tone(&self, duration_seconds: f64, frequency: f64) -> TrackRecord {
        let path = self.directory.path().join("prepared-processing-marker.wav");
        wave(
            &path,
            self.source_rate,
            self.channels,
            duration_seconds,
            frequency,
        );
        let (record, _) = local_track_from_path(&path).unwrap();
        let cursor = self.backend.event_cursor();
        self.execute(
            "import-processing-marker",
            BackendCommand::ImportAudio { paths: vec![path] },
        );
        terminal(&self.backend, cursor, "import-processing-marker");
        record
    }
    pub fn restart(&mut self) {
        self.backend.shutdown(Duration::from_secs(10)).unwrap();
        self.backend = BackendHandle::start(BackendConfig::headless(self.profile())).unwrap();
    }
}
impl Drop for Running {
    fn drop(&mut self) {
        let result = self.backend.shutdown(Duration::from_secs(10));
        if !thread::panicking() {
            result.unwrap();
        }
    }
}

pub fn temporary_directory() -> tempfile::TempDir {
    let root = std::env::temp_dir();
    fs::create_dir_all(&root).unwrap();
    tempfile::Builder::new()
        .prefix("lx-native-processing-")
        .tempdir_in(root)
        .unwrap()
}
pub fn envelope(id: &str, command: BackendCommand) -> CommandEnvelope {
    CommandEnvelope {
        request_id: id.into(),
        origin: "processing-contract-test".into(),
        expected_revision: None,
        command,
    }
}
pub fn wait(
    backend: &BackendHandle,
    seconds: f64,
    mut condition: impl FnMut(&BackendSnapshot) -> bool,
) -> BackendSnapshot {
    let deadline = Instant::now() + Duration::from_secs_f64(seconds);
    loop {
        let snapshot = backend.snapshot();
        assert_ne!(
            snapshot.playback.status,
            PlaybackStatus::Error,
            "actual processing failed: {snapshot:?}"
        );
        if condition(&snapshot) {
            return snapshot;
        }
        assert!(
            Instant::now() < deadline,
            "processing did not converge: {snapshot:?}; PCM={:?}",
            pcm(backend)
        );
        thread::sleep(Duration::from_millis(10));
    }
}
pub fn pcm(backend: &BackendHandle) -> RenderedPcm {
    match backend.query(BackendQuery::RenderedPcm).unwrap() {
        BackendQueryResult::RenderedPcm(Some(pcm)) => pcm,
        result => panic!("actual Memory PCM unavailable: {result:?}"),
    }
}
pub fn render_timing(
    backend: &BackendHandle,
) -> Option<lx_gpui_player::audio::output::OutputRenderTiming> {
    match backend
        .query(BackendQuery::AudioOutputRenderTiming)
        .unwrap()
    {
        BackendQueryResult::AudioOutputRenderTiming(timing) => timing,
        result => panic!("actual renderer timing unavailable: {result:?}"),
    }
}
pub fn supply(
    backend: &BackendHandle,
) -> Option<lx_gpui_player::audio::supply::AudioSupplySnapshot> {
    match backend.query(BackendQuery::AudioSupply).unwrap() {
        BackendQueryResult::AudioSupply(supply) => supply,
        result => panic!("actual audio supply unavailable: {result:?}"),
    }
}
pub fn settings(backend: &BackendHandle) -> PlaybackProcessingSettings {
    match backend
        .query(BackendQuery::PlaybackProcessingSettings)
        .unwrap()
    {
        BackendQueryResult::PlaybackProcessingSettings(settings) => settings,
        result => panic!("unexpected processing settings: {result:?}"),
    }
}
pub fn processing(backend: &BackendHandle) -> PlaybackProcessingSnapshot {
    match backend
        .query(BackendQuery::PlaybackProcessingState)
        .unwrap()
    {
        BackendQueryResult::PlaybackProcessingState(state) => state,
        result => panic!("unexpected processing state: {result:?}"),
    }
}
pub fn acknowledged(
    backend: &BackendHandle,
    expected: PlaybackProcessingSettings,
) -> PlaybackProcessingSnapshot {
    wait(backend, 12.0, |_| {
        let state = processing(backend);
        state.applied == expected
            && state.desired == expected
            && state.applied_sequence == state.requested_sequence
            && state.transition_remaining_frames == 0
            && state.sample_rate.is_some()
    });
    let state = processing(backend);
    assert_eq!(state.sample_rate, Some(OUTPUT_RATE));
    state
}
pub fn cache_epoch(backend: &BackendHandle) -> u64 {
    match backend.query(BackendQuery::AudioCacheStats).unwrap() {
        BackendQueryResult::AudioCacheStats(Some(stats)) => stats.epoch,
        result => panic!("processing has no actual encoded cache: {result:?}"),
    }
}
pub fn assert_media(before: &BackendSnapshot, after: &BackendSnapshot) {
    assert_eq!(after.playback.track_id, before.playback.track_id);
    assert_eq!(after.playback.generation, before.playback.generation);
    assert_eq!(
        after.playback.duration_seconds,
        before.playback.duration_seconds
    );
    assert_eq!(after.now_playing, before.now_playing);
    assert_eq!(after.resolved_track, before.resolved_track);
    assert_eq!(after.sources, before.sources);
    assert_eq!(after.pending_playback_url, before.pending_playback_url);
    assert_eq!(
        serde_json::to_value(&after.approved_audio_identity).unwrap(),
        serde_json::to_value(&before.approved_audio_identity).unwrap()
    );
}

/// Median and coverage are measured from distinct production render batches.
pub fn frequency(
    backend: &BackendHandle,
    expected: PlaybackProcessingSettings,
    input_hz: f64,
) -> f64 {
    let state = acknowledged(backend, expected);
    let ideal = input_hz
        * expected.pitch_ratio
        * if expected.preserves_pitch {
            1.0
        } else {
            expected.playback_rate
        };
    let tolerance = if expected.pitch_ratio == 1.0 {
        (ideal * 0.01).max(3.0)
    } else {
        0.75 * OUTPUT_RATE as f64 / 4096.0 + 2.0
    };
    let start = pcm(backend).batches;
    let settle = ((0.5 + state.latency_frames as f64 / OUTPUT_RATE as f64) * OUTPUT_RATE as f64
        / BATCH_FRAMES as f64)
        .ceil() as u64;
    wait(backend, 10.0, |_| pcm(backend).batches >= start + settle);
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut last = 0;
    let mut observations = Vec::with_capacity(25);
    while observations.len() < 25 {
        let rendered = pcm(backend);
        if rendered.batches != last
            && rendered.peak > 0.001
            && rendered.rms > 0.0
            && let Some(hz) = rendered.frequency_hz.filter(|hz| hz.is_finite())
        {
            last = rendered.batches;
            observations.push(hz);
        }
        assert!(
            Instant::now() < deadline,
            "not enough actual nonzero frequency batches: {observations:?}; {:?}",
            backend.snapshot()
        );
        thread::sleep(Duration::from_millis(5));
    }
    observations.sort_by(f64::total_cmp);
    let median = observations[observations.len() / 2];
    assert!(
        (median - ideal).abs() <= tolerance,
        "actual median {median} Hz, ideal {ideal} Hz, tolerance {tolerance}, samples={observations:?}"
    );
    assert!(
        observations
            .iter()
            .filter(|hz| (**hz - ideal).abs() <= tolerance)
            .count()
            >= 20,
        "pitch is unstable across actual batches: {observations:?}"
    );
    median
}

struct ClockObservation {
    fresh: BackendSnapshot,
    cached: BackendSnapshot,
    pcm: RenderedPcm,
    render_timing: Option<lx_gpui_player::audio::output::OutputRenderTiming>,
    supply: Option<lx_gpui_player::audio::supply::AudioSupplySnapshot>,
    cursor: u64,
    elapsed: Duration,
    bracket_elapsed: Duration,
    rejected_brackets: u64,
}

impl ClockObservation {
    fn diagnostic(&self) -> String {
        format!(
            "at={:.3}s bracket={:.3}ms rejected={} batch={} cursor={} fresh(position={}, generation={}, seek={}, status={:?}, rate={}) cached(position={}, generation={}, seek={}, status={:?}) processing(requested={}, applied={}, settings={:?}, latency={}, transition={}, sample_rate={:?}) output={:?} engine_render_timing={:?} engine_supply={:?} PCM(peak={}, rms={}, frequency={:?})",
            self.elapsed.as_secs_f64(),
            self.bracket_elapsed.as_secs_f64() * 1000.0,
            self.rejected_brackets,
            self.pcm.batches,
            self.cursor,
            self.fresh.playback.position_seconds,
            self.fresh.playback.generation,
            self.fresh.playback.seek_seq,
            self.fresh.playback.status,
            self.fresh.playback.rate,
            self.cached.playback.position_seconds,
            self.cached.playback.generation,
            self.cached.playback.seek_seq,
            self.cached.playback.status,
            self.fresh.playback_processing.requested_sequence,
            self.fresh.playback_processing.applied_sequence,
            self.fresh.playback_processing.applied,
            self.fresh.playback_processing.latency_frames,
            self.fresh.playback_processing.transition_remaining_frames,
            self.fresh.playback_processing.sample_rate,
            self.fresh.audio_output.state.format,
            self.render_timing,
            self.supply,
            self.pcm.peak,
            self.pcm.rms,
            self.pcm.frequency_hz,
        )
    }
}

/// The actor's cached snapshot and renderer observation have separate publishers.
/// A fresh actor projection is usable only if the renderer's batch stayed fixed
/// across that projection. No media delta or frequency influences acceptance.
fn clock_observation(
    backend: &BackendHandle,
    begun: Instant,
    deadline: Instant,
) -> ClockObservation {
    let mut rejected_brackets = 0;
    loop {
        assert!(
            Instant::now() < deadline,
            "processing clock could not obtain a stable render batch after {rejected_brackets} brackets; cached={:?}",
            backend.snapshot()
        );
        let bracket_begun = Instant::now();
        let cached = backend.snapshot();
        let before = pcm(backend);
        assert!(
            Instant::now() < deadline,
            "processing clock PCM query exceeded its window deadline"
        );
        let (fresh, cursor) = backend.snapshot_with_cursor().unwrap();
        assert!(
            Instant::now() < deadline,
            "processing clock fresh snapshot exceeded its window deadline"
        );
        let render_timing = render_timing(backend);
        let supply = supply(backend);
        let after = pcm(backend);
        assert!(
            Instant::now() < deadline,
            "processing clock final PCM query exceeded its window deadline"
        );
        if before.batches == after.batches {
            return ClockObservation {
                fresh,
                cached,
                pcm: after,
                render_timing,
                supply,
                cursor,
                elapsed: begun.elapsed(),
                bracket_elapsed: bracket_begun.elapsed(),
                rejected_brackets,
            };
        }
        rejected_brackets += 1;
        thread::sleep(Duration::from_millis(1));
    }
}

pub fn clock_window(backend: &BackendHandle, expected_rate: f64, batches: u64) -> BackendSnapshot {
    let begun = Instant::now();
    let deadline = begun + Duration::from_secs(12);
    let start = clock_observation(backend, begun, deadline);
    assert_eq!(start.fresh.playback.status, PlaybackStatus::Playing);
    let end = loop {
        assert!(
            Instant::now() < deadline,
            "processing clock window did not finish; start [{}]; cached={:?}",
            start.diagnostic(),
            backend.snapshot(),
        );
        if pcm(backend).batches >= start.pcm.batches + batches {
            let observed = clock_observation(backend, begun, deadline);
            if observed.fresh.playback.status == PlaybackStatus::Playing {
                break observed;
            }
        }
        thread::sleep(Duration::from_millis(10));
    };
    let output_seconds =
        (end.pcm.batches - start.pcm.batches) as f64 * BATCH_FRAMES as f64 / OUTPUT_RATE as f64;
    let actual_media_seconds =
        end.fresh.playback.position_seconds - start.fresh.playback.position_seconds;
    let expected_media_seconds = output_seconds * expected_rate;
    let tolerance = (expected_media_seconds * 0.10).max(0.08);
    let diagnostic = format!(
        "requested_batches={batches} actual_batches={} requested_rate={expected_rate} fresh_media_delta={actual_media_seconds} cached_media_delta={} start [{}]; end [{}]",
        end.pcm.batches - start.pcm.batches,
        end.cached.playback.position_seconds - start.cached.playback.position_seconds,
        start.diagnostic(),
        end.diagnostic(),
    );
    eprintln!("processing clock observation: {diagnostic}");
    if let (Some(first), Some(last)) = (start.render_timing, end.render_timing) {
        eprintln!(
            "actual Memory renderer: period_micros=20000 overrun_batch_delta={} last_render_micros={} max_render_micros={} last_total_micros={}",
            last.overrun_batches.saturating_sub(first.overrun_batches),
            last.last_render_micros,
            last.max_render_micros,
            last.last_total_micros,
        );
    }
    if let (Some(first), Some(last)) = (&start.supply, &end.supply)
        && first.generation == last.generation
        && first.source_serial == last.source_serial
        && first.source_epoch == last.source_epoch
        && first.track_id == last.track_id
    {
        eprintln!(
            "actual foreground supply: generation={} source_serial={} source_epoch={} produced_delta={} consumed_delta={} underflow_delta={} received_encoded_delta={} downloaded_encoded_delta={} queued_start={} queued_end={} cache_io_start={} cache_io_end={} pending_publish_start={} pending_publish_end={} budget_blocked_start={} budget_blocked_end={} publication_count_delta={} last_publication_micros={} max_publication_micros={} range_load_count_delta={} last_range_load_micros={} max_range_load_micros={}",
            last.generation,
            last.source_serial,
            last.source_epoch,
            last.produced_pcm_frames
                .saturating_sub(first.produced_pcm_frames),
            last.consumed_pcm_frames
                .saturating_sub(first.consumed_pcm_frames),
            last.underflow_pcm_frames
                .saturating_sub(first.underflow_pcm_frames),
            last.received_encoded_bytes
                .saturating_sub(first.received_encoded_bytes),
            last.encoded_downloaded_bytes
                .saturating_sub(first.encoded_downloaded_bytes),
            first.queued_pcm_frames,
            last.queued_pcm_frames,
            first.cache.io_active,
            last.cache.io_active,
            first.cache.pending_publish_bytes,
            last.cache.pending_publish_bytes,
            first.budget_blocked,
            last.budget_blocked,
            last.published_blocks.saturating_sub(first.published_blocks),
            last.last_publication_micros,
            last.max_publication_micros,
            last.completed_range_loads
                .saturating_sub(first.completed_range_loads),
            last.last_range_load_micros,
            last.max_range_load_micros,
        );
    }
    assert!(
        (actual_media_seconds - expected_media_seconds).abs() <= tolerance,
        "actual presented media delta {actual_media_seconds}, rendered time {output_seconds}, requested rate {expected_rate}, expected delta {expected_media_seconds}, tolerance {tolerance}; {diagnostic}"
    );
    end.fresh
}

pub fn wait_finished(backend: &BackendHandle, track: &str, deadline_seconds: f64) {
    wait(backend, deadline_seconds, |state| {
        (state.playback.status == PlaybackStatus::Ended
            && state.playback.track_id.as_deref() == Some(track))
            || (state.playback.status == PlaybackStatus::Idle && state.playback.track_id.is_none())
    });
}

fn terminal(backend: &BackendHandle, cursor: u64, id: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if backend.events_after(cursor).unwrap().iter().any(|event| {
            if let BackendEvent::OperationFinished {
                operation_id,
                success,
                error,
            } = &event.payload
                && operation_id == id
            {
                assert!(*success, "{error:?}");
                return true;
            }
            false
        }) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "owned music import did not complete"
        );
        thread::sleep(Duration::from_millis(10));
    }
}
fn wave(path: &Path, sample_rate: u32, channels: u16, seconds: f64, hz: f64) {
    let frames = (sample_rate as f64 * seconds).round() as usize;
    let length = (frames * channels as usize * 4) as u32;
    let mut bytes = Vec::with_capacity(44 + length as usize);
    bytes.extend_from_slice(b"RIFF");
    bytes.extend_from_slice(&(36 + length).to_le_bytes());
    bytes.extend_from_slice(b"WAVEfmt ");
    bytes.extend_from_slice(&16u32.to_le_bytes());
    bytes.extend_from_slice(&3u16.to_le_bytes());
    bytes.extend_from_slice(&channels.to_le_bytes());
    bytes.extend_from_slice(&sample_rate.to_le_bytes());
    bytes.extend_from_slice(&(sample_rate * channels as u32 * 4).to_le_bytes());
    bytes.extend_from_slice(&(channels * 4).to_le_bytes());
    bytes.extend_from_slice(&32u16.to_le_bytes());
    bytes.extend_from_slice(b"data");
    bytes.extend_from_slice(&length.to_le_bytes());
    for frame in 0..frames {
        let sample = (AMPLITUDE
            * (std::f64::consts::TAU * hz * frame as f64 / sample_rate as f64).cos())
            as f32;
        for _ in 0..channels {
            bytes.extend_from_slice(&sample.to_le_bytes());
        }
    }
    fs::write(path, bytes).unwrap();
}

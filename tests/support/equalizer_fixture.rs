use lx_gpui_player::{
    audio::{PlaybackStatus, RenderedPcm, cache::CacheIoErrorCode},
    backend::{
        BackendCommand, BackendConfig, BackendEvent, BackendHandle, BackendQuery,
        BackendQueryResult, BackendSnapshot, CommandEnvelope,
    },
    repository::local_track_from_path,
};
use serde_json::Value;
use std::{
    fs,
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant},
};

pub const RATE: u32 = 22_050;
pub const MIX_FREQUENCIES: [f64; 3] = [500.0, 1000.0, 2000.0];
pub const AMPLITUDE: f64 = 0.01;
pub const DURATION_SECONDS: usize = 32;

pub struct Running {
    pub directory: tempfile::TempDir,
    pub backend: BackendHandle,
}

impl Running {
    pub fn new() -> Self {
        Self::in_directory(temporary_directory())
    }

    pub fn in_directory(directory: tempfile::TempDir) -> Self {
        let backend =
            BackendHandle::start(BackendConfig::headless(directory.path().join("profile")))
                .unwrap();
        Self { directory, backend }
    }

    pub fn profile(&self) -> PathBuf {
        self.directory.path().join("profile")
    }

    pub fn execute(&self, id: &str, command: BackendCommand) {
        self.backend.execute(envelope(id, command)).unwrap();
    }

    pub fn import_mix(&self) -> String {
        let path = self.directory.path().join("owned-equalizer-mixed-tone.wav");
        mixed_wave(&path);
        let (track, _) = local_track_from_path(&path).unwrap();
        let cursor = self.backend.event_cursor();
        self.execute(
            "import-equalizer-tone",
            BackendCommand::ImportAudio { paths: vec![path] },
        );
        terminal(&self.backend, cursor, "import-equalizer-tone");
        track.id
    }

    pub fn play_mix(&self) -> BackendSnapshot {
        self.execute("unity-volume", BackendCommand::Volume { volume: 1.0 });
        let id = self.import_mix();
        self.execute(
            "play-equalizer-tone",
            BackendCommand::Play { id: id.clone() },
        );
        let playing = wait(&self.backend, |state| {
            if let BackendQueryResult::AudioCacheStats(Some(stats)) =
                self.backend.query(BackendQuery::AudioCacheStats).unwrap()
                && stats.worker_finished
                && stats.last_io_error == CacheIoErrorCode::Initialization
            {
                panic!(
                    "encoded cache initialization failed before equalizer PCM: {stats:?}; owned ledger state: {:?}",
                    cache_ledger_diagnostics(&self.profile())
                );
            }
            state.playback.status == PlaybackStatus::Playing
                && state.playback.track_id.as_deref() == Some(id.as_str())
                && state.playback.position_seconds > 0.12
                && state.playback.volume == 1.0
                && pcm(&self.backend).peak > 0.005
        });
        assert!(playing.playback.duration_known);
        assert!((playing.playback.duration_seconds - DURATION_SECONDS as f64).abs() < 0.02);
        assert!(playing.approved_audio_identity.is_some());
        assert_eq!(playing.now_playing.as_ref().unwrap().id, id);
        playing
    }

    pub fn restart(&mut self) {
        self.backend.shutdown(Duration::from_secs(10)).unwrap();
        self.backend = BackendHandle::start(BackendConfig::headless(self.profile())).unwrap();
    }
}

fn cache_ledger_diagnostics(profile: &Path) -> Vec<String> {
    let root = profile.join("audio-cache");
    let mut paths = Vec::new();
    if let Ok(entries) = fs::read_dir(&root) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if let Ok(children) = fs::read_dir(&path) {
                    paths.extend(children.flatten().map(|child| child.path()));
                }
            } else {
                paths.push(path);
            }
        }
    }
    paths.sort();
    paths
        .into_iter()
        .map(|path| {
            let name = path.strip_prefix(&root).unwrap().display();
            let length = fs::metadata(&path).map(|metadata| metadata.len());
            if path
                .file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with("block-"))
            {
                return format!("{name}: {length:?} bytes");
            }
            match fs::read(&path) {
                Ok(bytes) => {
                    let nonce = String::from_utf8_lossy(bytes.get(16..48).unwrap_or_default());
                    let payload = String::from_utf8_lossy(
                        bytes.get(120..bytes.len().min(632)).unwrap_or_default(),
                    );
                    format!(
                        "{name}: {} bytes, root_nonce={nonce:?}, payload={payload:?}",
                        bytes.len()
                    )
                }
                Err(error) => format!("{name}: {length:?} bytes, read={error}"),
            }
        })
        .collect()
}

impl Drop for Running {
    fn drop(&mut self) {
        let shutdown = self.backend.shutdown(Duration::from_secs(10));
        if !thread::panicking() {
            shutdown.unwrap();
        }
    }
}

pub fn temporary_directory() -> tempfile::TempDir {
    let root = std::env::temp_dir();
    fs::create_dir_all(&root).unwrap();
    tempfile::Builder::new()
        .prefix("lx-native-equalizer-")
        .tempdir_in(root)
        .unwrap()
}

pub fn envelope(id: &str, command: BackendCommand) -> CommandEnvelope {
    CommandEnvelope {
        request_id: id.into(),
        origin: "equalizer-contract-test".into(),
        expected_revision: None,
        command,
    }
}

pub fn wait(
    backend: &BackendHandle,
    mut condition: impl FnMut(&BackendSnapshot) -> bool,
) -> BackendSnapshot {
    let deadline = Instant::now() + Duration::from_secs(12);
    loop {
        let snapshot = backend.snapshot();
        if condition(&snapshot) {
            return snapshot;
        }
        assert!(
            Instant::now() < deadline,
            "equalizer state did not converge: {snapshot:?}"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

pub fn pcm(backend: &BackendHandle) -> RenderedPcm {
    match backend.query(BackendQuery::RenderedPcm).unwrap() {
        BackendQueryResult::RenderedPcm(Some(pcm)) => pcm,
        result => panic!("equalizer requires real Memory render observations: {result:?}"),
    }
}

pub fn cache_epoch(backend: &BackendHandle) -> u64 {
    match backend.query(BackendQuery::AudioCacheStats).unwrap() {
        BackendQueryResult::AudioCacheStats(Some(stats)) => stats.epoch,
        result => panic!("equalizer playback did not own the encoded cache: {result:?}"),
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

pub fn reference() -> Value {
    serde_json::from_str(include_str!("../fixtures/equalizer_reference.json")).unwrap()
}

pub fn flat_rms() -> f64 {
    AMPLITUDE * (MIX_FREQUENCIES.len() as f64 / 2.0).sqrt()
}

pub fn single_section_mix_rms(gain: f64) -> f64 {
    let reference = reference();
    let case = reference["single_section_cases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|case| {
            case["sample_rate_hz"] == RATE
                && case["requested_frequency_hz"] == 1000
                && case["gain_db"].as_f64() == Some(gain)
        })
        .unwrap();
    response_mix_rms(&case["responses"])
}

pub fn preset_mix_rms(name: &str) -> f64 {
    let reference = reference();
    let case = reference["preset_cascade_cases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|case| case["sample_rate_hz"] == RATE && case["name"].as_str() == Some(name))
        .unwrap();
    response_mix_rms(&case["responses"])
}

fn response_mix_rms(responses: &Value) -> f64 {
    let power: f64 = MIX_FREQUENCIES
        .iter()
        .map(|frequency| {
            let response = responses
                .as_array()
                .unwrap()
                .iter()
                .find(|response| response["frequency_hz"].as_f64() == Some(*frequency))
                .unwrap();
            response["magnitude"].as_f64().unwrap().powi(2)
        })
        .sum();
    AMPLITUDE * (power / 2.0).sqrt()
}

pub fn rms_gain_error_db(observed: f64, expected: f64) -> f64 {
    20.0 * (observed / expected).log10()
}

pub fn settled_rms(
    backend: &BackendHandle,
    after_position: f64,
    settle_seconds: f64,
    expected: f64,
) -> BackendSnapshot {
    let snapshot = wait(backend, |state| {
        let observed = pcm(backend);
        state.playback.status == PlaybackStatus::Playing
            && state.playback.position_seconds >= after_position + settle_seconds
            && observed.rms.is_finite()
            && observed.rms > 0.0
            && rms_gain_error_db(observed.rms, expected).abs() < 0.1
    });
    let observed = pcm(backend);
    assert!(rms_gain_error_db(observed.rms, expected).abs() < 0.1);
    assert!(observed.peak.is_finite() && observed.peak < 1.0);
    snapshot
}

fn terminal(backend: &BackendHandle, cursor: u64, operation: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        for event in backend.events_after(cursor).unwrap() {
            if let BackendEvent::OperationFinished {
                operation_id,
                success,
                error,
            } = event.payload
                && operation_id == operation
            {
                assert!(success, "{error:?}");
                return;
            }
        }
        assert!(
            Instant::now() < deadline,
            "equalizer fixture import did not complete"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

fn mixed_wave(path: &Path) {
    let frames = RATE as usize * DURATION_SECONDS;
    let length = (frames * 4) as u32;
    let mut bytes = Vec::with_capacity(44 + length as usize);
    bytes.extend_from_slice(b"RIFF");
    bytes.extend_from_slice(&(36 + length).to_le_bytes());
    bytes.extend_from_slice(b"WAVEfmt ");
    bytes.extend_from_slice(&16u32.to_le_bytes());
    bytes.extend_from_slice(&3u16.to_le_bytes());
    bytes.extend_from_slice(&1u16.to_le_bytes());
    bytes.extend_from_slice(&RATE.to_le_bytes());
    bytes.extend_from_slice(&(RATE * 4).to_le_bytes());
    bytes.extend_from_slice(&4u16.to_le_bytes());
    bytes.extend_from_slice(&32u16.to_le_bytes());
    bytes.extend_from_slice(b"data");
    bytes.extend_from_slice(&length.to_le_bytes());
    for frame in 0..frames {
        let sample: f64 = MIX_FREQUENCIES
            .iter()
            .map(|frequency| {
                AMPLITUDE * (std::f64::consts::TAU * frequency * frame as f64 / RATE as f64).cos()
            })
            .sum();
        bytes.extend_from_slice(&(sample as f32).to_le_bytes());
    }
    fs::write(path, bytes).unwrap();
}

use lx_gpui_player::{
    audio::{PlaybackStatus, RenderedPcm},
    backend::{
        BackendCommand, BackendConfig, BackendEvent, BackendHandle, BackendQuery,
        BackendQueryResult, BackendSnapshot, CommandEnvelope,
    },
    repository::local_track_from_path,
};
use std::{
    fs,
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant},
};

pub struct Running {
    pub directory: tempfile::TempDir,
    pub backend: BackendHandle,
}

impl Running {
    pub fn new() -> Self {
        let directory = temporary_directory();
        Self::in_directory(directory)
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

    pub fn import_local_tone(&self) -> String {
        let path = self.directory.path().join("owned-output-tone.wav");
        wave(&path);
        let (track, _) = local_track_from_path(&path).unwrap();
        let cursor = self.backend.event_cursor();
        self.execute(
            "import-output-tone",
            BackendCommand::ImportAudio { paths: vec![path] },
        );
        terminal(&self.backend, cursor, "import-output-tone");
        track.id
    }

    pub fn play_local_tone(&self) -> BackendSnapshot {
        let id = self.import_local_tone();
        self.execute("play-output-tone", BackendCommand::Play { id: id.clone() });
        let playing = wait(&self.backend, |state| {
            state.playback.track_id.as_deref() == Some(id.as_str())
                && state.playback.status == PlaybackStatus::Playing
                && state.playback.position_seconds > 0.1
                && rendered_pcm(&self.backend).peak > 0.03
        });
        assert!(playing.playback.duration_known);
        assert!((playing.playback.duration_seconds - 24.0).abs() < 0.02);
        assert!(playing.approved_audio_identity.is_some());
        assert_eq!(playing.now_playing.as_ref().unwrap().id, id);
        playing
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
        .prefix("lx-native-output-contract-")
        .tempdir_in(root)
        .unwrap()
}

pub fn envelope(id: &str, command: BackendCommand) -> CommandEnvelope {
    CommandEnvelope {
        request_id: id.into(),
        origin: "output-contract-test".into(),
        expected_revision: None,
        command,
    }
}

pub fn wait(
    backend: &BackendHandle,
    mut predicate: impl FnMut(&BackendSnapshot) -> bool,
) -> BackendSnapshot {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let state = backend.snapshot();
        if predicate(&state) {
            return state;
        }
        assert!(
            Instant::now() < deadline,
            "audio output contract did not converge: {state:?}"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

pub fn rendered_pcm(backend: &BackendHandle) -> RenderedPcm {
    match backend.query(BackendQuery::RenderedPcm).unwrap() {
        BackendQueryResult::RenderedPcm(Some(pcm)) => pcm,
        other => panic!("Memory output did not expose real rendered PCM: {other:?}"),
    }
}

pub fn cache_epoch(backend: &BackendHandle) -> u64 {
    match backend.query(BackendQuery::AudioCacheStats).unwrap() {
        BackendQueryResult::AudioCacheStats(Some(stats)) => stats.epoch,
        other => panic!("unexpected encoded cache query: {other:?}"),
    }
}

pub fn terminal(backend: &BackendHandle, cursor: u64, operation: &str) {
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
            "operation {operation} did not finish"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

pub fn assert_same_media(before: &BackendSnapshot, after: &BackendSnapshot) {
    assert_eq!(after.playback.track_id, before.playback.track_id);
    assert_eq!(after.playback.generation, before.playback.generation);
    assert_eq!(after.playback.seek_seq, before.playback.seek_seq);
    assert_eq!(
        after.playback.duration_seconds,
        before.playback.duration_seconds
    );
    assert_eq!(after.sources, before.sources);
    assert_eq!(after.now_playing, before.now_playing);
    assert_eq!(after.resolved_track, before.resolved_track);
    assert_eq!(
        serde_json::to_value(&after.approved_audio_identity).unwrap(),
        serde_json::to_value(&before.approved_audio_identity).unwrap()
    );
}

fn wave(path: &Path) {
    let sample_rate = 48_000u32;
    let frames = sample_rate as usize * 24;
    let length = (frames * 2) as u32;
    let mut bytes = Vec::with_capacity(44 + length as usize);
    bytes.extend_from_slice(b"RIFF");
    bytes.extend_from_slice(&(length + 36).to_le_bytes());
    bytes.extend_from_slice(b"WAVEfmt ");
    bytes.extend_from_slice(&16u32.to_le_bytes());
    bytes.extend_from_slice(&1u16.to_le_bytes());
    bytes.extend_from_slice(&1u16.to_le_bytes());
    bytes.extend_from_slice(&sample_rate.to_le_bytes());
    bytes.extend_from_slice(&(sample_rate * 2).to_le_bytes());
    bytes.extend_from_slice(&2u16.to_le_bytes());
    bytes.extend_from_slice(&16u16.to_le_bytes());
    bytes.extend_from_slice(b"data");
    bytes.extend_from_slice(&length.to_le_bytes());
    for frame in 0..frames {
        let sample = (12_000.0
            * (std::f64::consts::TAU * 997.0 * frame as f64 / sample_rate as f64).sin())
            as i16;
        bytes.extend_from_slice(&sample.to_le_bytes());
    }
    fs::write(path, bytes).unwrap();
}

#![allow(dead_code)]

use lx_gpui_player::{
    audio::{PlaybackStatus, RenderedPcm},
    backend::{
        BackendCommand, BackendConfig, BackendEvent, BackendHandle, BackendQuery,
        BackendQueryResult, BackendSnapshot, CommandEnvelope, ErrorKind,
    },
    domain::library::{
        LibraryAssetView, LibraryOperationState, LibraryOperationView, LibraryRoot,
        LibraryScanOptions,
    },
    playback::queue::PlayMode,
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
        let temporary = std::env::temp_dir();
        fs::create_dir_all(&temporary).unwrap();
        let directory = tempfile::Builder::new()
            .prefix("lx-native-library-acceptance-")
            .tempdir_in(temporary)
            .unwrap();
        let backend =
            BackendHandle::start(BackendConfig::headless(directory.path().join("profile")))
                .unwrap();
        let running = Self { directory, backend };
        running.execute(
            "library-fixture-no-loop",
            BackendCommand::SetPlayMode {
                mode: PlayMode::None,
            },
        );
        running.execute(
            "library-fixture-no-error-skip",
            BackendCommand::SetAutoSkipOnError { enabled: false },
        );
        running
    }

    pub fn profile(&self) -> PathBuf {
        self.directory.path().join("profile")
    }

    pub fn folder(&self, name: &str) -> PathBuf {
        let path = self.directory.path().join(name);
        fs::create_dir_all(&path).unwrap();
        path
    }

    pub fn execute(&self, id: &str, command: BackendCommand) {
        self.backend.execute(envelope(id, command)).unwrap();
    }

    pub fn import_explicit(&self, id: &str, paths: Vec<PathBuf>) {
        let cursor = self.backend.event_cursor();
        self.execute(id, BackendCommand::ImportAudio { paths });
        assert_eq!(operation_finished(&self.backend, cursor, id), (true, None));
    }

    pub fn add_root(&self, id: &str, path: PathBuf, recursive: bool) -> LibraryRoot {
        self.execute(id, BackendCommand::AddLibraryRoot { path, recursive });
        let operation = library_terminal(&self.backend, id);
        assert_eq!(
            operation.state,
            LibraryOperationState::Completed,
            "{operation:?}"
        );
        root(&self.backend, operation.root_id.as_deref().unwrap()).unwrap()
    }

    pub fn scan(
        &self,
        id: &str,
        root_id: &str,
        options: LibraryScanOptions,
    ) -> LibraryOperationView {
        self.execute(
            id,
            BackendCommand::ScanLibraryRoot {
                root_id: root_id.into(),
                options,
            },
        );
        library_terminal(&self.backend, id)
    }

    pub fn restart(&mut self) {
        self.backend
            .shutdown(Duration::from_secs(15))
            .unwrap_or_else(|error| {
                panic!(
                    "Backend restart shutdown failed: {error:?}; runtime error: {:?}",
                    self.backend.snapshot().error
                )
            });
        self.backend = BackendHandle::start(BackendConfig::headless(self.profile())).unwrap();
    }

    pub fn play(&self, request: &str, track_id: &str, frequency: f64) -> BackendSnapshot {
        self.execute(
            request,
            BackendCommand::Play {
                id: track_id.into(),
            },
        );
        let playing = wait(&self.backend, 15.0, |snapshot| {
            let observed = pcm(&self.backend);
            snapshot.playback.track_id.as_deref() == Some(track_id)
                && snapshot.playback.status == PlaybackStatus::Playing
                && snapshot.playback.position_seconds > 0.1
                && observed.peak > 0.005
                && observed
                    .frequency_hz
                    .is_some_and(|hz| (hz - frequency).abs() < 3.0)
        });
        assert!(playing.approved_audio_identity.is_some());
        playing
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        let result = self.backend.shutdown(Duration::from_secs(15));
        if !thread::panicking() {
            result.unwrap();
        }
    }
}

pub fn envelope(id: &str, command: BackendCommand) -> CommandEnvelope {
    CommandEnvelope {
        request_id: id.into(),
        origin: "library-acceptance-test".into(),
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
        if condition(&snapshot) {
            return snapshot;
        }
        if Instant::now() >= deadline {
            let mut diagnostic = snapshot;
            std::sync::Arc::make_mut(&mut diagnostic.tracks).truncate(8);
            std::sync::Arc::make_mut(&mut diagnostic.queue).truncate(8);
            panic!(
                "library state did not converge: {diagnostic:?}; watches={:?}; operations={:?}; assets={:?}; candidates={:?}",
                backend.query(lx_gpui_player::backend::BackendQuery::LibraryWatches {
                    offset: 0,
                    limit: 8
                }),
                backend.query(lx_gpui_player::backend::BackendQuery::LibraryOperations {
                    offset: 0,
                    limit: 10
                }),
                backend.query(lx_gpui_player::backend::BackendQuery::LibraryAssets {
                    root_id: None,
                    after_asset_id: None,
                    limit: 8,
                    expected_revision: None
                }),
                backend.query(
                    lx_gpui_player::backend::BackendQuery::LibraryRelinkCandidates {
                        root_id: None,
                        after_candidate_id: None,
                        limit: 8
                    }
                )
            );
        }
        thread::sleep(Duration::from_millis(10));
    }
}

pub fn operation_finished(
    backend: &BackendHandle,
    cursor: u64,
    id: &str,
) -> (bool, Option<ErrorKind>) {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        for event in backend.events_after(cursor).unwrap() {
            if let BackendEvent::OperationFinished {
                operation_id,
                success,
                error,
            } = event.payload
                && operation_id == id
            {
                return (success, error.map(|error| error.kind));
            }
        }
        assert!(
            Instant::now() < deadline,
            "library operation {id} never finished"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

pub fn pcm(backend: &BackendHandle) -> RenderedPcm {
    match backend.query(BackendQuery::RenderedPcm).unwrap() {
        BackendQueryResult::RenderedPcm(Some(pcm)) => pcm,
        result => panic!("library playback did not expose actual PCM: {result:?}"),
    }
}

pub fn cache_epoch(backend: &BackendHandle) -> u64 {
    match backend.query(BackendQuery::AudioCacheStats).unwrap() {
        BackendQueryResult::AudioCacheStats(Some(stats)) => stats.epoch,
        result => panic!("library playback has no actual encoded cache: {result:?}"),
    }
}

pub fn root(backend: &BackendHandle, id: &str) -> Option<LibraryRoot> {
    match backend
        .query(BackendQuery::LibraryRoot { root_id: id.into() })
        .unwrap()
    {
        BackendQueryResult::LibraryRoot(root) => root,
        result => panic!("unexpected library root query: {result:?}"),
    }
}

pub fn library_operation(backend: &BackendHandle, id: &str) -> Option<LibraryOperationView> {
    match backend
        .query(BackendQuery::LibraryOperation {
            operation_id: id.into(),
        })
        .unwrap()
    {
        BackendQueryResult::LibraryOperation(operation) => operation.map(|operation| *operation),
        result => panic!("unexpected library operation query: {result:?}"),
    }
}

pub fn library_terminal(backend: &BackendHandle, id: &str) -> LibraryOperationView {
    let deadline = Instant::now() + Duration::from_secs(40);
    loop {
        if let Some(operation) = library_operation(backend, id)
            && operation.state != LibraryOperationState::Running
        {
            return operation;
        }
        assert!(
            Instant::now() < deadline,
            "library operation {id} did not reach a terminal query state"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

pub fn assets(backend: &BackendHandle, root_id: Option<&str>) -> Vec<LibraryAssetView> {
    let mut items = Vec::new();
    let mut after = None;
    let mut revision = None;
    let mut total = None;
    loop {
        let page = match backend
            .query(BackendQuery::LibraryAssets {
                root_id: root_id.map(str::to_owned),
                after_asset_id: after.clone(),
                limit: 73,
                expected_revision: revision,
            })
            .unwrap()
        {
            BackendQueryResult::LibraryAssets(page) => page,
            result => panic!("unexpected keyset library assets page: {result:?}"),
        };
        assert!(page.items.len() <= 73);
        assert!(serde_json::to_vec(&page).unwrap().len() <= 8 * 1024 * 1024);
        if let Some(revision) = revision {
            assert_eq!(page.revision, revision);
        }
        if let Some(total) = total {
            assert_eq!(page.total, total);
        }
        revision = Some(page.revision);
        total = Some(page.total);
        for item in &page.items {
            if let Some(previous) = items.last() {
                let previous: &LibraryAssetView = previous;
                assert!(
                    previous.asset.id < item.asset.id,
                    "keyset pages duplicated or reordered assets"
                );
            }
            items.push(item.clone());
        }
        match page.next_asset_id {
            Some(next) => {
                assert_ne!(after.as_deref(), Some(next.as_str()));
                after = Some(next);
            }
            None => break,
        }
    }
    assert_eq!(items.len(), total.unwrap());
    items
}

pub fn assert_same_media(before: &BackendSnapshot, after: &BackendSnapshot) {
    assert_eq!(after.playback.track_id, before.playback.track_id);
    assert_eq!(after.playback.generation, before.playback.generation);
    assert_eq!(after.playback.seek_seq, before.playback.seek_seq);
    assert_eq!(
        after.playback.duration_seconds,
        before.playback.duration_seconds
    );
    assert_eq!(after.now_playing, before.now_playing);
    assert_eq!(after.sources, before.sources);
    assert_eq!(after.pending_playback_url, before.pending_playback_url);
    assert_eq!(
        serde_json::to_value(&after.approved_audio_identity).unwrap(),
        serde_json::to_value(&before.approved_audio_identity).unwrap()
    );
}

pub fn wave(path: &Path, frequency: f64, seconds: f64) {
    let rate = 22_050u32;
    let frames = (rate as f64 * seconds).round() as usize;
    let length = (frames * 4) as u32;
    let mut bytes = Vec::with_capacity(44 + length as usize);
    bytes.extend_from_slice(b"RIFF");
    bytes.extend_from_slice(&(36 + length).to_le_bytes());
    bytes.extend_from_slice(b"WAVEfmt ");
    bytes.extend_from_slice(&16u32.to_le_bytes());
    bytes.extend_from_slice(&3u16.to_le_bytes());
    bytes.extend_from_slice(&1u16.to_le_bytes());
    bytes.extend_from_slice(&rate.to_le_bytes());
    bytes.extend_from_slice(&(rate * 4).to_le_bytes());
    bytes.extend_from_slice(&4u16.to_le_bytes());
    bytes.extend_from_slice(&32u16.to_le_bytes());
    bytes.extend_from_slice(b"data");
    bytes.extend_from_slice(&length.to_le_bytes());
    for frame in 0..frames {
        let sample =
            (0.1 * (std::f64::consts::TAU * frequency * frame as f64 / rate as f64).cos()) as f32;
        bytes.extend_from_slice(&sample.to_le_bytes());
    }
    fs::write(path, bytes).unwrap();
}

pub fn corpus(root: &Path, count: usize) -> Vec<PathBuf> {
    (0..count)
        .map(|index| {
            let folder = root.join(format!("batch-{:02}", index / 100));
            fs::create_dir_all(&folder).unwrap();
            let path = folder.join(format!("owned-{:04}.wav", index));
            wave(&path, 440.0 + (index % 4) as f64 * 110.0, 0.1);
            path
        })
        .collect()
}

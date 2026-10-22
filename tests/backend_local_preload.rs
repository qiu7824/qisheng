//! Catalog assets use their existing file consent without inventing a source installation.
use lx_gpui_player::{
    audio::PlaybackStatus,
    backend::{
        BackendCommand, BackendConfig, BackendEvent, BackendHandle, BackendQuery,
        BackendQueryResult, BackendSnapshot, CommandEnvelope,
    },
    domain::lists::{AddLocation, ListAction},
    playback::queue::PlayMode,
    repository::local_track_from_path,
};
use std::{
    fs,
    path::Path,
    thread,
    time::{Duration, Instant},
};

struct Running {
    backend: BackendHandle,
    directory: tempfile::TempDir,
    first: String,
    next: String,
}
impl Running {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let first_path = directory.path().join("first-original.wav");
        let next_path = directory.path().join("next-original.wav");
        wave(&first_path, 12.0, 440.0);
        wave(&next_path, 2.0, 880.0);
        let (first, _) = local_track_from_path(&first_path).unwrap();
        let (next, _) = local_track_from_path(&next_path).unwrap();
        let backend =
            BackendHandle::start(BackendConfig::headless(directory.path().join("profile")))
                .unwrap();
        command(
            &backend,
            "import",
            BackendCommand::ImportAudio {
                paths: vec![first_path, next_path],
            },
        );
        wait(&backend, |state| {
            state.tracks.iter().any(|track| track.id == next.id)
        });
        command(
            &backend,
            "list",
            BackendCommand::ListAction {
                action: Box::new(ListAction::MusicAdd {
                    list_id: "default".into(),
                    tracks: vec![first.clone(), next.clone()],
                    location: AddLocation::Bottom,
                }),
            },
        );
        command(
            &backend,
            "mode",
            BackendCommand::SetPlayMode {
                mode: PlayMode::List,
            },
        );
        Self {
            backend,
            directory,
            first: first.id,
            next: next.id,
        }
    }
    fn play(&self) {
        command(
            &self.backend,
            "play",
            BackendCommand::PlayListTrack {
                list_id: "default".into(),
                music_id: self.first.clone(),
                quality: None,
            },
        );
    }
    fn ready(&self) -> BackendSnapshot {
        wait(&self.backend, |state| {
            state
                .preload
                .as_ref()
                .is_some_and(|preload| preload.state == "ready")
        })
    }
}
impl Drop for Running {
    fn drop(&mut self) {
        self.backend.shutdown(Duration::from_secs(5)).unwrap();
    }
}
fn command(backend: &BackendHandle, id: &str, command: BackendCommand) {
    backend
        .execute(CommandEnvelope {
            request_id: id.into(),
            origin: "local-preload-test".into(),
            expected_revision: None,
            command,
        })
        .unwrap();
}
fn wait(
    backend: &BackendHandle,
    mut predicate: impl FnMut(&BackendSnapshot) -> bool,
) -> BackendSnapshot {
    let deadline = Instant::now() + Duration::from_secs(17);
    loop {
        let state = backend.snapshot();
        if predicate(&state) {
            return state;
        }
        assert!(
            Instant::now() < deadline,
            "catalog preload did not converge: {state:?}"
        );
        thread::sleep(Duration::from_millis(15));
    }
}
fn wave(path: &Path, seconds: f64, frequency: f64) {
    let frames = (seconds * 48_000.0) as usize;
    let length = (frames * 2) as u32;
    let mut bytes = Vec::with_capacity(44 + length as usize);
    bytes.extend_from_slice(b"RIFF");
    bytes.extend_from_slice(&(length + 36).to_le_bytes());
    bytes.extend_from_slice(b"WAVEfmt ");
    bytes.extend_from_slice(&16u32.to_le_bytes());
    bytes.extend_from_slice(&1u16.to_le_bytes());
    bytes.extend_from_slice(&1u16.to_le_bytes());
    bytes.extend_from_slice(&48_000u32.to_le_bytes());
    bytes.extend_from_slice(&96_000u32.to_le_bytes());
    bytes.extend_from_slice(&2u16.to_le_bytes());
    bytes.extend_from_slice(&16u16.to_le_bytes());
    bytes.extend_from_slice(b"data");
    bytes.extend_from_slice(&length.to_le_bytes());
    for frame in 0..frames {
        let sample =
            (7000.0 * (std::f64::consts::TAU * frequency * frame as f64 / 48_000.0).sin()) as i16;
        bytes.extend_from_slice(&sample.to_le_bytes());
    }
    fs::write(path, bytes).unwrap();
}

#[test]
fn registered_local_assets_prepare_and_promote_actual_pcm_without_a_source_or_new_consent() {
    let running = Running::new();
    running.play();
    let ready = running.ready();
    assert!(ready.sources.is_empty());
    assert!(ready.pending_playback_url.is_none());
    assert_eq!(
        ready.playback.track_id.as_deref(),
        Some(running.first.as_str())
    );
    let preload = ready.preload.unwrap();
    assert_eq!(preload.candidate.track_id, running.next);
    assert!(preload.pending_url.is_none());
    let identity = preload.identity.unwrap();
    assert!(identity.source_sha256.is_empty());
    assert_eq!(identity.source_policy_version, 0);
    assert_eq!(identity.actual_quality, "local");
    assert!(identity.approval_id.starts_with("catalog-asset:"));
    let previous_generation = ready.playback.generation;
    let (_, mut cursor) = running.backend.snapshot_with_cursor().unwrap();
    let mut promoted = false;
    let playing = wait(&running.backend, |state| {
        for event in running.backend.events_after(cursor).unwrap() {
            cursor = event.event_id;
            if let BackendEvent::PreloadPromoted {
                operation_id,
                track_id,
                ..
            } = event.payload
                && operation_id == preload.operation_id
                && track_id == running.next
            {
                promoted = true;
            }
        }
        promoted
            && state.playback.status == PlaybackStatus::Playing
            && state.playback.track_id.as_deref() == Some(running.next.as_str())
    });
    assert!(playing.playback.generation > previous_generation);
    assert!(playing.sources.is_empty());
    assert!(playing.pending_playback_url.is_none());
    assert_eq!(
        playing.approved_audio_identity.unwrap().url_sha256,
        identity.url_sha256
    );
    let deadline = Instant::now() + Duration::from_secs(1);
    loop {
        let BackendQueryResult::RenderedPcm(Some(pcm)) =
            running.backend.query(BackendQuery::RenderedPcm).unwrap()
        else {
            panic!("missing actual PCM observation");
        };
        if pcm
            .frequency_hz
            .is_some_and(|frequency| (frequency - 880.0).abs() < 3.0)
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "promoted asset did not render its original 880 Hz PCM"
        );
        thread::sleep(Duration::from_millis(10));
    }
    command(&running.backend, "stop", BackendCommand::Stop);
}

#[test]
fn updated_asset_discards_the_old_ready_resource_and_plays_its_new_content() {
    let running = Running::new();
    running.play();
    let ready = running.ready();
    let old_operation = ready.preload.unwrap().operation_id;
    let (_, mut cursor) = running.backend.snapshot_with_cursor().unwrap();
    let next_path = running.directory.path().join("next-original.wav");
    wave(&next_path, 2.1, 660.0);
    command(
        &running.backend,
        "reimport",
        BackendCommand::ImportAudio {
            paths: vec![next_path],
        },
    );
    let mut reimported = false;
    let mut stale_promoted = false;
    let resumed = wait(&running.backend, |state| {
        for event in running.backend.events_after(cursor).unwrap() {
            cursor = event.event_id;
            match event.payload {
                BackendEvent::OperationFinished {
                    operation_id,
                    success,
                    ..
                } if operation_id == "reimport" => {
                    assert!(success);
                    reimported = true;
                }
                BackendEvent::PreloadPromoted { operation_id, .. }
                    if operation_id == old_operation =>
                {
                    stale_promoted = true
                }
                _ => {}
            }
        }
        reimported
            && state.playback.status == PlaybackStatus::Playing
            && state.playback.track_id.as_deref() == Some(running.next.as_str())
    });
    assert!(!stale_promoted);
    assert!(resumed.pending_playback_url.is_none());
    assert!(resumed.error.is_none() && resumed.playback.error.is_none());
    let deadline = Instant::now() + Duration::from_secs(1);
    loop {
        let BackendQueryResult::RenderedPcm(Some(pcm)) =
            running.backend.query(BackendQuery::RenderedPcm).unwrap()
        else {
            panic!("missing actual PCM");
        };
        if pcm
            .frequency_hz
            .is_some_and(|frequency| (frequency - 660.0).abs() < 3.0)
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "updated asset retained the old PCM"
        );
        thread::sleep(Duration::from_millis(10));
    }
    command(&running.backend, "stop", BackendCommand::Stop);
}

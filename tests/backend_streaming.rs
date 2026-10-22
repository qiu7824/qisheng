//! Public command/query/event integration with an approved, partially available HTTP stream.
use lx_gpui_player::{
    audio::{PlaybackStatus, cache::CacheStats},
    backend::{
        BackendCommand, BackendConfig, BackendEvent, BackendHandle, BackendQuery,
        BackendQueryResult, BackendSnapshot, CommandEnvelope, ReceiptState,
    },
    domain::{
        lists::{AddLocation, ListAction},
        lyric::{LyricBundle, LyricField, LyricOrigin},
        music::{PlatformTrackRef, TrackRecord},
    },
    playback::queue::PlayMode,
    store::ProjectStore,
};
use std::{
    fs,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};
use tempfile::TempDir;

const FIRST_BLOCK: usize = 256 * 1024;

struct SlowAudio {
    base: String,
    requests: Arc<Mutex<Vec<String>>>,
    release: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}
impl SlowAudio {
    fn new() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let release = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        let log = requests.clone();
        let gate = release.clone();
        let stopping = stop.clone();
        let bytes = Arc::new(tone());
        let thread = thread::spawn(move || {
            let mut clients = Vec::new();
            while !stopping.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((socket, _)) => {
                        let log = log.clone();
                        let gate = gate.clone();
                        let stopping = stopping.clone();
                        let bytes = bytes.clone();
                        clients.push(thread::spawn(move || {
                            serve(socket, log, gate, stopping, bytes)
                        }));
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(3));
                    }
                    Err(error) => panic!("fixture accept: {error}"),
                }
            }
            for client in clients {
                client.join().unwrap();
            }
        });
        Self {
            base,
            requests,
            release,
            stop,
            thread: Some(thread),
        }
    }
    fn count(&self) -> usize {
        self.requests.lock().unwrap().len()
    }
    fn script(&self) -> String {
        format!(
            r#"/*
 * @name Streaming integration oscillator
 * @version 1.0
 */
 lx.on(lx.EVENT_NAMES.request,async({{source,action,info}})=>{{
   if(source!=='kw'||action!=='musicUrl'||info.type!=='128k')throw Error('incorrect actual quality');
   return '{}/tone.wav?quality='+info.type+'&token=private-fixture';
 }});
 lx.send(lx.EVENT_NAMES.inited,{{sources:{{kw:{{type:'music',actions:['musicUrl'],qualitys:['128k']}}}}}});
 "#,
            self.base
        )
    }
}
impl Drop for SlowAudio {
    fn drop(&mut self) {
        self.release.store(true, Ordering::Release);
        self.stop.store(true, Ordering::Release);
        self.thread.take().unwrap().join().unwrap();
    }
}
fn serve(
    mut socket: TcpStream,
    log: Arc<Mutex<Vec<String>>>,
    release: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    bytes: Arc<Vec<u8>>,
) {
    socket.set_nonblocking(false).unwrap();
    socket
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    socket
        .set_write_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let mut headers = Vec::new();
    let mut scratch = [0; 1024];
    while !headers.windows(4).any(|part| part == b"\r\n\r\n") {
        let Ok(count) = socket.read(&mut scratch) else {
            return;
        };
        if count == 0 {
            return;
        }
        headers.extend_from_slice(&scratch[..count]);
        if headers.len() > 8192 {
            return;
        }
    }
    let headers = String::from_utf8_lossy(&headers).into_owned();
    log.lock().unwrap().push(headers.clone());
    let range = headers.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        if !name.eq_ignore_ascii_case("range") {
            return None;
        }
        let (start, end) = value.trim().strip_prefix("bytes=")?.split_once('-')?;
        Some((start.parse::<usize>().ok()?, end.parse::<usize>().ok()?))
    });
    let (start, end) = range.unwrap_or((0, bytes.len() - 1));
    let end = end.min(bytes.len() - 1);
    if start >= bytes.len() {
        return;
    }
    let length = end - start + 1;
    if start > 0 {
        while !release.load(Ordering::Acquire) && !stop.load(Ordering::Acquire) {
            thread::sleep(Duration::from_millis(5));
        }
        if stop.load(Ordering::Acquire) {
            return;
        }
    }
    let status = if range.is_some() {
        "206 Partial Content"
    } else {
        "200 OK"
    };
    let mut response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: audio/wav\r\nContent-Length: {length}\r\nAccept-Ranges: bytes\r\nETag: \"oscillator-v1\"\r\nConnection: close\r\n"
    );
    if range.is_some() {
        response.push_str(&format!(
            "Content-Range: bytes {start}-{end}/{}\r\n",
            bytes.len()
        ));
    }
    response.push_str("\r\n");
    if socket.write_all(response.as_bytes()).is_err() {
        return;
    }
    let initial_end = if start == 0 {
        (FIRST_BLOCK - 1).min(end)
    } else {
        end
    };
    if socket.write_all(&bytes[start..=initial_end]).is_err() {
        return;
    }
    if initial_end < end {
        while !release.load(Ordering::Acquire) && !stop.load(Ordering::Acquire) {
            thread::sleep(Duration::from_millis(5));
        }
        if !stop.load(Ordering::Acquire) {
            let _ = socket.write_all(&bytes[initial_end + 1..=end]);
        }
    }
}
fn tone() -> Vec<u8> {
    let rate = 96_000u32;
    let count = rate * 5;
    let data_len = count * 2;
    let mut bytes = Vec::with_capacity(data_len as usize + 44);
    bytes.extend(b"RIFF");
    bytes.extend((data_len + 36).to_le_bytes());
    bytes.extend(b"WAVEfmt ");
    bytes.extend(16u32.to_le_bytes());
    bytes.extend(1u16.to_le_bytes());
    bytes.extend(1u16.to_le_bytes());
    bytes.extend(rate.to_le_bytes());
    bytes.extend((rate * 2).to_le_bytes());
    bytes.extend(2u16.to_le_bytes());
    bytes.extend(16u16.to_le_bytes());
    bytes.extend(b"data");
    bytes.extend(data_len.to_le_bytes());
    for n in 0..count {
        bytes.extend(
            (((n as f64 * 440.0 * std::f64::consts::TAU / rate as f64).sin() * 6000.0) as i16)
                .to_le_bytes(),
        );
    }
    bytes
}
struct Running {
    backend: BackendHandle,
    directory: TempDir,
    track_id: String,
}
impl Running {
    fn new() -> Self {
        let root = std::env::temp_dir().join("lx-native-backend-streaming-tests");
        fs::create_dir_all(&root).unwrap();
        let directory = tempfile::Builder::new()
            .prefix("profile-")
            .tempdir_in(root)
            .unwrap();
        let backend =
            BackendHandle::start(BackendConfig::headless(directory.path().join("profile")))
                .unwrap();
        let track = TrackRecord {
            id: "kw_stream-original".into(),
            lx_id: "kw_stream-original".into(),
            title: "Original oscillator".into(),
            artists: vec!["Fixture composer".into()],
            album: "Original fixtures".into(),
            duration_ms: None,
            platform: PlatformTrackRef {
                source: "kw".into(),
                song_id: "stream-original".into(),
                qualities: Vec::new(),
            },
            local_asset_id: None,
            metadata: serde_json::Map::new(),
            wire: None,
        };
        let track_id = track.id.clone();
        execute(
            &backend,
            "add",
            BackendCommand::ListAction {
                action: Box::new(ListAction::MusicAdd {
                    list_id: "default".into(),
                    tracks: vec![track],
                    location: AddLocation::Bottom,
                }),
            },
        );
        execute(
            &backend,
            "mode",
            BackendCommand::SetPlayMode {
                mode: PlayMode::None,
            },
        );
        execute(
            &backend,
            "preferred",
            BackendCommand::SetPlaybackQuality {
                quality: "flac".into(),
            },
        );
        Self {
            backend,
            directory,
            track_id,
        }
    }
    fn prepare(&self, audio: &SlowAudio) -> String {
        execute_and_wait(
            &self.backend,
            "import-source",
            BackendCommand::ImportSourceText {
                script: audio.script(),
            },
        );
        let id = self.backend.snapshot().sources[0].id.clone();
        execute_and_wait(
            &self.backend,
            "approve-source",
            BackendCommand::ApproveSource { id: id.clone() },
        );
        execute_and_wait(
            &self.backend,
            "resolve",
            BackendCommand::PlayListTrack {
                list_id: "default".into(),
                music_id: self.track_id.clone(),
                quality: None,
            },
        );
        wait(&self.backend, |state| state.pending_playback_url.is_some());
        id
    }
}
impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.backend.shutdown(Duration::from_secs(5));
    }
}
fn execute(
    backend: &BackendHandle,
    id: &str,
    command: BackendCommand,
) -> lx_gpui_player::backend::CommandReceipt {
    backend
        .execute(CommandEnvelope {
            request_id: id.into(),
            origin: "streaming-test".into(),
            expected_revision: None,
            command,
        })
        .unwrap()
}
fn execute_and_wait(backend: &BackendHandle, id: &str, command: BackendCommand) {
    let cursor = backend.event_cursor();
    let receipt = execute(backend, id, command);
    if receipt.state != ReceiptState::Accepted {
        return;
    }
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        for event in backend.events_after(cursor).unwrap() {
            if let BackendEvent::OperationFinished {
                operation_id,
                success,
                error,
            } = event.payload
                && operation_id == receipt.operation_id
            {
                assert!(success, "{error:?}");
                return;
            }
        }
        assert!(Instant::now() < deadline, "operation timeout");
        thread::sleep(Duration::from_millis(10));
    }
}
fn wait(backend: &BackendHandle, predicate: impl Fn(&BackendSnapshot) -> bool) -> BackendSnapshot {
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        let state = backend.snapshot();
        if predicate(&state) {
            return state;
        }
        assert!(
            Instant::now() < deadline,
            "stream state timeout: {:?}",
            state.playback
        );
        thread::sleep(Duration::from_millis(10));
    }
}
fn stats(backend: &BackendHandle) -> CacheStats {
    let BackendQueryResult::AudioCacheStats(Some(stats)) =
        backend.query(BackendQuery::AudioCacheStats).unwrap()
    else {
        panic!("cache statistics unavailable");
    };
    stats
}

#[test]
fn approved_stream_renders_before_completion_and_buffering_pause_freezes_the_clock() {
    let audio = SlowAudio::new();
    let running = Running::new();
    let source_id = running.prepare(&audio);
    thread::sleep(Duration::from_millis(100));
    assert_eq!(
        audio.count(),
        0,
        "a resolved URL must not issue a request before exact URL approval"
    );
    assert_eq!(stats(&running.backend).objects, 0);
    let pending = running.backend.snapshot().pending_playback_url.unwrap();
    assert!(
        pending.contains("quality=128k"),
        "manifest fallback must be the actual quality"
    );
    execute(
        &running.backend,
        "approve-url",
        BackendCommand::ApprovePlaybackUrl,
    );
    let identity = running.backend.snapshot().approved_audio_identity.unwrap();
    assert_eq!(identity.track_id, running.track_id);
    assert_eq!(identity.platform, "kw");
    assert_eq!(identity.actual_quality, "128k");
    assert_eq!(identity.source_sha256, source_id);
    assert_eq!(
        identity.source_policy_version,
        u64::from(lx_gpui_player::source::CURRENT_APPROVAL_POLICY_VERSION)
    );
    assert!(identity.source_epoch > 0 && identity.approval_epoch > 0);
    assert_eq!(identity.approval_id, "approve-url");
    assert_eq!(identity.url_sha256.len(), 64);
    wait(&running.backend, |state| {
        state.playback.status == PlaybackStatus::Playing && state.playback.position_seconds > 0.1
    });
    let BackendQueryResult::RenderedPcm(Some(pcm)) =
        running.backend.query(BackendQuery::RenderedPcm).unwrap()
    else {
        panic!();
    };
    assert!(pcm.peak > 0.01);
    assert!(
        !audio.release.load(Ordering::Acquire),
        "PCM must precede the remainder of the HTTP body"
    );
    let buffered = wait(&running.backend, |state| {
        state.playback.status == PlaybackStatus::Buffering
    });
    assert!(buffered.busy && buffered.playback.buffering);
    assert_eq!(buffered.now_playing.unwrap().id, running.track_id);
    execute(&running.backend, "pause-buffering", BackendCommand::Toggle);
    let paused = wait(&running.backend, |state| {
        state.playback.status == PlaybackStatus::Paused
    });
    thread::sleep(Duration::from_millis(150));
    assert!(
        (running.backend.snapshot().playback.position_seconds - paused.playback.position_seconds)
            .abs()
            < 0.02
    );
    let query_started = Instant::now();
    assert!(matches!(
        running.backend.query(BackendQuery::ListInfos).unwrap(),
        BackendQueryResult::ListInfos(_)
    ));
    assert!(query_started.elapsed() < Duration::from_millis(500));
    audio.release.store(true, Ordering::Release);
    execute(&running.backend, "resume", BackendCommand::Resume);
    wait(&running.backend, |state| {
        state.playback.status == PlaybackStatus::Playing
            && state.playback.position_seconds > paused.playback.position_seconds + 0.1
    });
    execute(&running.backend, "stop", BackendCommand::Stop);
    wait(&running.backend, |state| {
        state.playback.status == PlaybackStatus::Idle
    });
}

#[test]
fn cache_clear_is_accepted_with_deferred_statistics_and_preserves_profile_content() {
    let audio = SlowAudio::new();
    let running = Running::new();
    let source_id = running.prepare(&audio);
    let bundle = LyricBundle {
        lyric: LyricField::Text("[00:00.000]Original preserved text".into()),
        ..LyricBundle::default()
    };
    execute(
        &running.backend,
        "save-lyrics",
        BackendCommand::SaveLyrics {
            id: running.track_id.clone(),
            bundle: bundle.clone(),
        },
    );
    execute(
        &running.backend,
        "history",
        BackendCommand::RecordSearch {
            keyword: "preserved query".into(),
        },
    );
    execute(
        &running.backend,
        "approve-url",
        BackendCommand::ApprovePlaybackUrl,
    );
    wait(&running.backend, |state| {
        state.playback.status == PlaybackStatus::Playing && stats(&running.backend).blocks > 0
    });
    assert!(stats(&running.backend).blocks > 0);
    let profile = running.directory.path().join("profile");
    let saved_profile = fs::read(profile.join("player-state.json")).unwrap();
    let revision = running.backend.snapshot().revision;
    let cursor = running.backend.event_cursor();
    let receipt = execute(
        &running.backend,
        "clear-cache",
        BackendCommand::ClearAudioCache,
    );
    assert_eq!(receipt.state, ReceiptState::Accepted);
    let deadline = Instant::now() + Duration::from_secs(5);
    let cleared = loop {
        let result = running
            .backend
            .events_after(cursor)
            .unwrap()
            .into_iter()
            .find_map(|event| {
                if let BackendEvent::AudioCacheCleared {
                    operation_id,
                    stats,
                } = event.payload
                    && operation_id == receipt.operation_id
                {
                    Some(stats)
                } else {
                    None
                }
            });
        if let Some(stats) = result {
            break stats;
        }
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(10));
    };
    assert!(cleared.epoch > 0 && cleared.deferred_blocks > 0 && cleared.active_leases > 0);
    assert!(cleared.encoded_bytes > 0 && !cleared.cannot_clear && cleared.foreign_files == 0);
    assert_eq!(running.backend.snapshot().revision, revision);
    assert_eq!(
        fs::read(profile.join("player-state.json")).unwrap(),
        saved_profile
    );
    assert_eq!(
        running
            .backend
            .lyrics(&running.track_id, Some(LyricOrigin::Edited))
            .unwrap(),
        Some(bundle)
    );
    assert!(
        running
            .backend
            .snapshot()
            .sources
            .iter()
            .any(|source| source.id == source_id && source.approved)
    );
    assert!(
        ProjectStore::open(profile)
            .unwrap()
            .load()
            .unwrap()
            .sources
            .iter()
            .any(|source| source.sha256 == source_id)
    );
    let BackendQueryResult::SearchHistory(history) =
        running.backend.query(BackendQuery::SearchHistory).unwrap()
    else {
        panic!();
    };
    assert_eq!(history, vec!["preserved query"]);
    let BackendQueryResult::ListTracks(tracks) = running
        .backend
        .query(BackendQuery::ListTracks {
            list_id: "default".into(),
            offset: 0,
            limit: 100,
        })
        .unwrap()
    else {
        panic!();
    };
    assert_eq!(tracks.items[0].id, running.track_id);
    let started = Instant::now();
    execute(&running.backend, "stop-waiting", BackendCommand::Stop);
    wait(&running.backend, |state| {
        state.playback.status == PlaybackStatus::Idle
    });
    assert!(started.elapsed() < Duration::from_millis(700));
    let deadline = Instant::now() + Duration::from_secs(3);
    while stats(&running.backend).deferred_blocks != 0 {
        assert!(
            Instant::now() < deadline,
            "cleared leased blocks were not reclaimed"
        );
        thread::sleep(Duration::from_millis(15));
    }
    let reclaimed = stats(&running.backend);
    assert_eq!(reclaimed.encoded_bytes, 0);
    assert_eq!(reclaimed.blocks, 0);
    assert_eq!(reclaimed.deferred_blocks, 0);
    assert!(!reclaimed.cannot_clear);
}

#[test]
fn cache_clear_reports_foreign_content_without_deleting_user_data_or_claiming_success() {
    let audio = SlowAudio::new();
    let running = Running::new();
    let source_id = running.prepare(&audio);
    let bundle = LyricBundle {
        lyric: LyricField::Text("[00:00.000]Original preserved text".into()),
        ..LyricBundle::default()
    };
    execute(
        &running.backend,
        "save-lyrics",
        BackendCommand::SaveLyrics {
            id: running.track_id.clone(),
            bundle: bundle.clone(),
        },
    );
    execute(
        &running.backend,
        "history",
        BackendCommand::RecordSearch {
            keyword: "preserved query".into(),
        },
    );
    execute(
        &running.backend,
        "approve-url",
        BackendCommand::ApprovePlaybackUrl,
    );
    wait(&running.backend, |state| {
        state.playback.status == PlaybackStatus::Playing && stats(&running.backend).blocks > 0
    });
    assert!(stats(&running.backend).blocks > 0);
    let profile = running.directory.path().join("profile");
    let original_path = profile.join("audio-cache").join("original-user.wav");
    fs::write(&original_path, b"user recording").unwrap();
    let saved_profile = fs::read(profile.join("player-state.json")).unwrap();
    let revision = running.backend.snapshot().revision;
    let before = stats(&running.backend);
    let cursor = running.backend.event_cursor();
    let receipt = execute(
        &running.backend,
        "clear-foreign-cache",
        BackendCommand::ClearAudioCache,
    );
    assert_eq!(receipt.state, ReceiptState::Accepted);
    let deadline = Instant::now() + Duration::from_secs(5);
    let error = loop {
        let terminal = running
            .backend
            .events_after(cursor)
            .unwrap()
            .into_iter()
            .find_map(|event| match event.payload {
                BackendEvent::AudioCacheCleared { operation_id, .. }
                    if operation_id == receipt.operation_id =>
                {
                    panic!("foreign cache contents must not report successful cleanup")
                }
                BackendEvent::OperationFinished {
                    operation_id,
                    success,
                    error,
                } if operation_id == receipt.operation_id => {
                    assert!(!success);
                    Some(error.expect("cleanup failure must include its error"))
                }
                _ => None,
            });
        if let Some(error) = terminal {
            break error;
        }
        assert!(
            Instant::now() < deadline,
            "foreign cache cleanup did not publish its terminal failure"
        );
        thread::sleep(Duration::from_millis(10));
    };
    assert_eq!(error.kind, lx_gpui_player::backend::ErrorKind::Storage);
    assert!(error.message.contains("cannotClear"));
    let failed = stats(&running.backend);
    assert!(failed.cannot_clear);
    assert!(failed.epoch > before.epoch);
    assert!(failed.foreign_files >= 1 && failed.foreign_bytes >= b"user recording".len() as u64);
    assert!(failed.encoded_bytes > 0 && failed.deferred_blocks > 0 && failed.active_leases > 0);
    assert_eq!(running.backend.snapshot().revision, revision);
    assert_eq!(fs::read(&original_path).unwrap(), b"user recording");
    assert_eq!(
        fs::read(profile.join("player-state.json")).unwrap(),
        saved_profile
    );
    assert_eq!(
        running
            .backend
            .lyrics(&running.track_id, Some(LyricOrigin::Edited))
            .unwrap(),
        Some(bundle)
    );
    assert!(
        running
            .backend
            .snapshot()
            .sources
            .iter()
            .any(|source| source.id == source_id && source.approved)
    );
    assert!(
        ProjectStore::open(profile)
            .unwrap()
            .load()
            .unwrap()
            .sources
            .iter()
            .any(|source| source.sha256 == source_id)
    );
    let BackendQueryResult::SearchHistory(history) =
        running.backend.query(BackendQuery::SearchHistory).unwrap()
    else {
        panic!();
    };
    assert_eq!(history, vec!["preserved query"]);
    let BackendQueryResult::ListTracks(tracks) = running
        .backend
        .query(BackendQuery::ListTracks {
            list_id: "default".into(),
            offset: 0,
            limit: 100,
        })
        .unwrap()
    else {
        panic!();
    };
    assert_eq!(tracks.items[0].id, running.track_id);
    execute(
        &running.backend,
        "stop-after-foreign-clear",
        BackendCommand::Stop,
    );
    wait(&running.backend, |state| {
        state.playback.status == PlaybackStatus::Idle
    });
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let remaining = stats(&running.backend);
        if remaining.encoded_bytes == 0 && remaining.blocks == 0 && remaining.deferred_blocks == 0 {
            assert!(remaining.cannot_clear && remaining.foreign_files >= 1);
            break;
        }
        assert!(
            Instant::now() < deadline,
            "managed leased blocks were not reclaimed after failed foreign cleanup"
        );
        thread::sleep(Duration::from_millis(15));
    }
    assert_eq!(fs::read(&original_path).unwrap(), b"user recording");
}

#[test]
fn cancelling_or_stopping_pending_url_keeps_audio_transport_unused() {
    let audio = SlowAudio::new();
    let running = Running::new();
    running.prepare(&audio);
    execute(
        &running.backend,
        "cancel-url",
        BackendCommand::CancelPlaybackUrl,
    );
    execute(
        &running.backend,
        "approve-cancelled",
        BackendCommand::ApprovePlaybackUrl,
    );
    thread::sleep(Duration::from_millis(100));
    assert_eq!(audio.count(), 0);
    execute_and_wait(
        &running.backend,
        "resolve-again",
        BackendCommand::PlayListTrack {
            list_id: "default".into(),
            music_id: running.track_id.clone(),
            quality: None,
        },
    );
    execute(&running.backend, "stop-pending", BackendCommand::Stop);
    execute(
        &running.backend,
        "approve-stopped",
        BackendCommand::ApprovePlaybackUrl,
    );
    thread::sleep(Duration::from_millis(100));
    assert_eq!(audio.count(), 0);
    assert_eq!(stats(&running.backend).objects, 0);
}

#[test]
fn approved_audio_context_tracks_exact_quality_url_source_and_consent_epochs() {
    let audio = SlowAudio::new();
    let running = Running::new();
    let source_id = running.prepare(&audio);
    execute(
        &running.backend,
        "approve-first",
        BackendCommand::ApprovePlaybackUrl,
    );
    let first = running.backend.snapshot().approved_audio_identity.unwrap();
    execute_and_wait(
        &running.backend,
        "resolve-second",
        BackendCommand::PlayListTrack {
            list_id: "default".into(),
            music_id: running.track_id.clone(),
            quality: Some("128k".into()),
        },
    );
    assert!(running.backend.snapshot().approved_audio_identity.is_none());
    execute(
        &running.backend,
        "approve-second",
        BackendCommand::ApprovePlaybackUrl,
    );
    let second = running.backend.snapshot().approved_audio_identity.unwrap();
    assert_eq!(first.track_id, second.track_id);
    assert_eq!(first.actual_quality, second.actual_quality);
    assert_eq!(first.url_sha256, second.url_sha256);
    assert_eq!(first.source_sha256, second.source_sha256);
    assert_eq!(first.source_epoch, second.source_epoch);
    assert_ne!(first.approval_id, second.approval_id);
    assert!(second.approval_epoch > first.approval_epoch);
    let changed_script = audio
        .script()
        .replace("info.type!=='128k'", "!['128k','320k'].includes(info.type)")
        .replace("qualitys:['128k']", "qualitys:['128k','320k']");
    execute_and_wait(
        &running.backend,
        "import-second-source",
        BackendCommand::ImportSourceText {
            script: changed_script,
        },
    );
    let next_source = running
        .backend
        .snapshot()
        .sources
        .iter()
        .find(|source| source.id != source_id)
        .unwrap()
        .id
        .clone();
    execute_and_wait(
        &running.backend,
        "approve-second-source",
        BackendCommand::ApproveSource {
            id: next_source.clone(),
        },
    );
    execute_and_wait(
        &running.backend,
        "resolve-new-quality",
        BackendCommand::PlayListTrack {
            list_id: "default".into(),
            music_id: running.track_id.clone(),
            quality: Some("320k".into()),
        },
    );
    execute(
        &running.backend,
        "approve-new-quality",
        BackendCommand::ApprovePlaybackUrl,
    );
    let snapshot = running.backend.snapshot();
    let third = snapshot.approved_audio_identity.as_ref().unwrap();
    assert_eq!(third.actual_quality, "320k");
    assert_eq!(third.source_sha256, next_source);
    assert_ne!(third.source_sha256, second.source_sha256);
    assert_ne!(third.url_sha256, second.url_sha256);
    assert!(third.source_epoch > second.source_epoch);
    assert!(third.approval_epoch > second.approval_epoch);
    let encoded = serde_json::to_string(&snapshot).unwrap();
    assert!(!encoded.contains("token=private-fixture"));
    assert!(!encoded.contains("tone.wav?"));
    execute(&running.backend, "stop-context", BackendCommand::Stop);
    assert!(running.backend.snapshot().approved_audio_identity.is_none());
}

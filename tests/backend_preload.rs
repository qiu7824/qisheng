//! Real approved-source speculation through the public backend and Memory output.
use lx_gpui_player::{
    audio::PlaybackStatus,
    backend::{
        BackendCommand, BackendConfig, BackendEvent, BackendHandle, BackendQuery,
        BackendQueryResult, BackendSnapshot, CommandEnvelope, ReceiptState,
    },
    domain::{
        lists::{AddLocation, ListAction},
        lyric::{LyricBundle, LyricField},
        music::{PlatformTrackRef, TrackRecord},
    },
    playback::queue::PlayMode,
};
use std::{
    cell::Cell,
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

struct Media {
    base: String,
    requests: Arc<Mutex<Vec<String>>>,
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}
impl Media {
    fn new() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let log = requests.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = stop.clone();
        let current = Arc::new(tone(440.0));
        let next = Arc::new(tone(880.0));
        let thread = thread::spawn(move || {
            let mut clients = Vec::new();
            while !stopping.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((socket, _)) => {
                        let log = log.clone();
                        let current = current.clone();
                        let next = next.clone();
                        clients.push(thread::spawn(move || serve(socket, log, current, next)));
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(3))
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
            stop,
            thread: Some(thread),
        }
    }
    fn count(&self, name: &str) -> usize {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .filter(|path| path.starts_with(&format!("/{name}.wav")))
            .count()
    }
    fn source(&self, slow_next: bool, bad_next: bool) -> String {
        format!(
            r#"/*
 * @name Stateful preload oscillator
 * @version 1.0
 */
 let calls=0;
 lx.on(lx.EVENT_NAMES.request,async({{source,action,info}})=>{{
   if(source!=='kw'||action!=='musicUrl')throw Error('incorrect source routing');
   const id=String(info.musicInfo.songmid);
   const counter=++calls;
   if((id==='a'&&info.type!=='128k')||(id==='b'&&info.type!=='320k'))throw Error('incorrect actual candidate quality');
   if(id==='b'&&{slow_next})await new Promise(resolve=>lx.request('{}/resolve-b',{{}},()=>setTimeout(resolve,5000)));
   const target=id==='b'&&{bad_next}?'bad':id;
   return '{}/'+target+'.wav?quality='+info.type+'&counter='+counter+'&token=private';
 }});
 lx.send(lx.EVENT_NAMES.inited,{{sources:{{kw:{{type:'music',actions:['musicUrl'],qualitys:['128k','320k']}}}}}});
 "#,
            self.base, self.base
        )
    }
}
impl Drop for Media {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.thread.take().unwrap().join().unwrap();
    }
}
fn serve(
    mut socket: TcpStream,
    log: Arc<Mutex<Vec<String>>>,
    current: Arc<Vec<u8>>,
    next: Arc<Vec<u8>>,
) {
    socket.set_nonblocking(false).unwrap();
    socket
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    socket
        .set_write_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let mut bytes = Vec::new();
    let mut scratch = [0; 1024];
    while !bytes.windows(4).any(|part| part == b"\r\n\r\n") {
        let Ok(count) = socket.read(&mut scratch) else {
            return;
        };
        if count == 0 {
            return;
        }
        bytes.extend_from_slice(&scratch[..count]);
        if bytes.len() > 8192 {
            return;
        }
    }
    let request = String::from_utf8_lossy(&bytes);
    let Some(path) = request
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
    else {
        return;
    };
    log.lock().unwrap().push(path.into());
    let bad = b"unsupported original media".to_vec();
    let signal = b"ok".to_vec();
    let body = if path.starts_with("/resolve-b") {
        &signal
    } else if path.starts_with("/bad.wav") {
        &bad
    } else if path.starts_with("/b.wav") {
        next.as_ref()
    } else {
        current.as_ref()
    };
    let range = request.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        if !name.eq_ignore_ascii_case("range") {
            return None;
        }
        let (start, end) = value.trim().strip_prefix("bytes=")?.split_once('-')?;
        Some((start.parse::<usize>().ok()?, end.parse::<usize>().ok()?))
    });
    let (start, end) = range.unwrap_or((0, body.len() - 1));
    let end = end.min(body.len() - 1);
    if start >= body.len() {
        return;
    }
    let status = if range.is_some() {
        "206 Partial Content"
    } else {
        "200 OK"
    };
    let mut headers = format!(
        "HTTP/1.1 {status}\r\nContent-Type: audio/wav\r\nContent-Length: {}\r\nAccept-Ranges: bytes\r\nETag: \"{path}\"\r\nConnection: close\r\n",
        end - start + 1
    );
    if range.is_some() {
        headers.push_str(&format!(
            "Content-Range: bytes {start}-{end}/{}\r\n",
            body.len()
        ));
    }
    headers.push_str("\r\n");
    let _ = socket
        .write_all(headers.as_bytes())
        .and_then(|()| socket.write_all(&body[start..=end]));
}
fn tone(frequency: f64) -> Vec<u8> {
    let rate = 8000u32;
    let count = rate * 12;
    let size = count * 2;
    let mut bytes = Vec::with_capacity(size as usize + 44);
    bytes.extend(b"RIFF");
    bytes.extend((size + 36).to_le_bytes());
    bytes.extend(b"WAVEfmt ");
    bytes.extend(16u32.to_le_bytes());
    bytes.extend(1u16.to_le_bytes());
    bytes.extend(1u16.to_le_bytes());
    bytes.extend(rate.to_le_bytes());
    bytes.extend((rate * 2).to_le_bytes());
    bytes.extend(2u16.to_le_bytes());
    bytes.extend(16u16.to_le_bytes());
    bytes.extend(b"data");
    bytes.extend(size.to_le_bytes());
    for n in 0..count {
        bytes.extend(
            (((n as f64 * frequency * std::f64::consts::TAU / rate as f64).sin() * 6000.0) as i16)
                .to_le_bytes(),
        );
    }
    bytes
}
fn record(id: &str, quality: &str) -> TrackRecord {
    TrackRecord {
        id: format!("kw_{id}"),
        lx_id: format!("kw_{id}"),
        title: format!("Original oscillator {id}"),
        artists: vec!["Fixture composer".into()],
        album: "Original fixtures".into(),
        duration_ms: Some(12_000),
        platform: PlatformTrackRef {
            source: "kw".into(),
            song_id: id.into(),
            qualities: vec![quality.into()],
        },
        local_asset_id: None,
        metadata: serde_json::Map::new(),
        wire: None,
    }
}
struct Running {
    backend: BackendHandle,
    _directory: TempDir,
}
impl Running {
    fn new(media: &Media, slow_next: bool, bad_next: bool) -> Self {
        let root = std::env::temp_dir().join("lx-native-backend-preload-tests");
        fs::create_dir_all(&root).unwrap();
        let directory = tempfile::Builder::new()
            .prefix("profile-")
            .tempdir_in(root)
            .unwrap();
        let backend =
            BackendHandle::start(BackendConfig::headless(directory.path().join("profile")))
                .unwrap();
        execute(
            &backend,
            "add",
            BackendCommand::ListAction {
                action: Box::new(ListAction::MusicAdd {
                    list_id: "default".into(),
                    tracks: vec![
                        record("a", "128k"),
                        record("b", "320k"),
                        record("c", "128k"),
                    ],
                    location: AddLocation::Bottom,
                }),
            },
        );
        execute(
            &backend,
            "mode",
            BackendCommand::SetPlayMode {
                mode: PlayMode::List,
            },
        );
        execute(
            &backend,
            "quality",
            BackendCommand::SetPlaybackQuality {
                quality: "flac".into(),
            },
        );
        for id in ["a", "b", "c"] {
            execute(
                &backend,
                &format!("lyric-{id}"),
                BackendCommand::SaveLyrics {
                    id: format!("kw_{id}"),
                    bundle: LyricBundle {
                        lyric: LyricField::Text("[00:00.000]Original cached lyric".into()),
                        ..Default::default()
                    },
                },
            );
        }
        let cursor = backend.event_cursor();
        let receipt = execute(
            &backend,
            "source",
            BackendCommand::ImportSourceText {
                script: media.source(slow_next, bad_next),
            },
        );
        terminal(&backend, cursor, &receipt.operation_id, true);
        let source_id = backend.snapshot().sources[0].id.clone();
        let cursor = backend.event_cursor();
        let receipt = execute(
            &backend,
            "source-approve",
            BackendCommand::ApproveSource { id: source_id },
        );
        terminal(&backend, cursor, &receipt.operation_id, true);
        let cursor = backend.event_cursor();
        let receipt = execute(
            &backend,
            "play-a",
            BackendCommand::PlayListTrack {
                list_id: "default".into(),
                music_id: "kw_a".into(),
                quality: None,
            },
        );
        terminal(&backend, cursor, &receipt.operation_id, true);
        assert!(
            backend
                .snapshot()
                .pending_playback_url
                .as_ref()
                .unwrap()
                .contains("counter=1")
        );
        assert_eq!(media.count("a"), 0);
        execute(&backend, "url-a", BackendCommand::ApprovePlaybackUrl);
        wait(&backend, |state| {
            state.playback.status == PlaybackStatus::Playing
        });
        Self {
            backend,
            _directory: directory,
        }
    }
    fn pending(&self) -> lx_gpui_player::backend::BackendPreloadSnapshot {
        wait(&self.backend, |state| {
            state
                .preload
                .as_ref()
                .is_some_and(|preload| preload.state == "awaiting_approval")
        });
        self.backend.snapshot().preload.unwrap()
    }
    fn approve(&self, pending: &lx_gpui_player::backend::BackendPreloadSnapshot) {
        let cursor = self.backend.event_cursor();
        let receipt = execute(
            &self.backend,
            "approve-next",
            BackendCommand::ApprovePreloadUrl {
                operation_id: pending.operation_id.clone(),
                url: pending.pending_url.clone().unwrap(),
            },
        );
        assert_eq!(receipt.state, ReceiptState::Accepted);
        terminal(&self.backend, cursor, &receipt.operation_id, true);
        wait(&self.backend, |state| {
            state
                .preload
                .as_ref()
                .is_some_and(|preload| preload.state == "ready")
        });
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
            origin: "preload-test".into(),
            expected_revision: None,
            command,
        })
        .unwrap()
}
fn wait(backend: &BackendHandle, predicate: impl Fn(&BackendSnapshot) -> bool) -> BackendSnapshot {
    let deadline = Instant::now() + Duration::from_secs(18);
    loop {
        let state = backend.snapshot();
        if predicate(&state) {
            return state;
        }
        assert!(
            Instant::now() < deadline,
            "preload timeout: {:?} / {:?}",
            state.playback,
            state.preload.as_ref().map(|p| (&p.state, &p.failure))
        );
        thread::sleep(Duration::from_millis(15));
    }
}
fn terminal(backend: &BackendHandle, cursor: u64, id: &str, expected: bool) {
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        for event in backend.events_after(cursor).unwrap() {
            if let BackendEvent::OperationFinished {
                operation_id,
                success,
                error,
            } = event.payload
                && operation_id == id
            {
                assert_eq!(success, expected, "{error:?}");
                return;
            }
        }
        assert!(
            Instant::now() < deadline,
            "operation did not terminate: {id}"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

struct LivePromotionEvents {
    cursor: Cell<u64>,
    operation: String,
    candidate_entry: String,
    promoted: Cell<bool>,
    wrong_entry: Cell<bool>,
    lost_events: Cell<bool>,
}
impl LivePromotionEvents {
    fn start(
        backend: &BackendHandle,
        operation: &str,
        candidate_entry: &str,
    ) -> (BackendSnapshot, Self) {
        let (snapshot, cursor) = backend.snapshot_with_cursor().unwrap();
        (
            snapshot,
            Self {
                cursor: Cell::new(cursor),
                operation: operation.into(),
                candidate_entry: candidate_entry.into(),
                promoted: Cell::new(false),
                wrong_entry: Cell::new(false),
                lost_events: Cell::new(false),
            },
        )
    }
    fn poll(&self, backend: &BackendHandle) {
        match backend.events_after(self.cursor.get()) {
            Ok(events) => {
                for event in events {
                    self.cursor.set(event.event_id);
                    if let BackendEvent::PreloadPromoted {
                        operation_id,
                        entry_id,
                        ..
                    } = event.payload
                        && operation_id == self.operation
                    {
                        self.promoted.set(true);
                        self.wrong_entry
                            .set(self.wrong_entry.get() || entry_id != self.candidate_entry);
                    }
                }
            }
            Err(error) if error.kind == lx_gpui_player::backend::ErrorKind::CursorExpired => {
                let (_, cursor) = backend.snapshot_with_cursor().unwrap();
                self.cursor.set(cursor);
                self.lost_events.set(true);
            }
            Err(error) => panic!("event stream failed: {error}"),
        }
    }
    fn assert_continuous(&self) {
        assert!(
            !self.lost_events.get(),
            "a rebuilt event cursor cannot prove promotion behavior across the lost interval"
        );
        assert!(
            !self.wrong_entry.get(),
            "the promoted event must use the prepared queue occurrence"
        );
    }
}

#[test]
fn real_next_track_preparation_approves_exact_url_and_promotes_the_original_pcm_stream() {
    let media = Media::new();
    let running = Running::new(&media, false, false);
    let pending = running.pending();
    assert_eq!(pending.actual_quality, "320k");
    let current = running.backend.snapshot();
    assert_eq!(
        current.approved_audio_identity.unwrap().actual_quality,
        "128k"
    );
    assert_eq!(media.count("b"), 0);
    assert!(pending.pending_url.as_ref().unwrap().contains("counter=2"));
    let rejected = running.backend.execute(CommandEnvelope {
        request_id: "wrong-url".into(),
        origin: "preload-test".into(),
        expected_revision: None,
        command: BackendCommand::ApprovePreloadUrl {
            operation_id: pending.operation_id.clone(),
            url: pending
                .pending_url
                .as_ref()
                .unwrap()
                .replace("token=private", "token=other"),
        },
    });
    assert!(rejected.is_err());
    assert_eq!(media.count("b"), 0);
    running.approve(&pending);
    let (ready, promotion_events) = LivePromotionEvents::start(
        &running.backend,
        &pending.operation_id,
        &pending.candidate.entry_id,
    );
    assert_eq!(ready.preload.as_ref().unwrap().state, "ready");
    assert_eq!(ready.now_playing.unwrap().id, "kw_a");
    assert_eq!(
        ready.preload.unwrap().identity.unwrap().actual_quality,
        "320k"
    );
    assert_eq!(media.count("b"), 1);
    let before = ready.playback.generation;
    let promoted = wait(&running.backend, |state| {
        promotion_events.poll(&running.backend);
        state.playback.status == PlaybackStatus::Playing
            && state.playback.track_id.as_deref() == Some("kw_b")
            && state.playback.position_seconds > 0.15
            && promotion_events.promoted.get()
    });
    assert!(promoted.playback.generation > before);
    assert_eq!(
        promoted.approved_audio_identity.unwrap().actual_quality,
        "320k"
    );
    assert_eq!(
        media.count("b"),
        1,
        "promotion must retain the prepared HTTP/decode session"
    );
    promotion_events.assert_continuous();
    assert!(promotion_events.promoted.get());
    let BackendQueryResult::RenderedPcm(Some(pcm)) =
        running.backend.query(BackendQuery::RenderedPcm).unwrap()
    else {
        panic!();
    };
    assert!(pcm.peak > 0.01);
    assert!((pcm.frequency_hz.unwrap() - 880.0).abs() < 30.0);
    execute(&running.backend, "stop", BackendCommand::Stop);
}

#[test]
fn foreground_cancels_a_slow_speculative_request_and_preserves_one_js_session() {
    let media = Media::new();
    let running = Running::new(&media, true, false);
    let resolving = wait(&running.backend, |state| {
        state
            .preload
            .as_ref()
            .is_some_and(|preload| preload.state == "resolving")
            && media
                .requests
                .lock()
                .unwrap()
                .iter()
                .any(|path| path.starts_with("/resolve-b"))
    });
    let old = resolving.preload.unwrap();
    let source_epoch = resolving.approved_audio_identity.unwrap().source_epoch;
    let began = Instant::now();
    let cursor = running.backend.event_cursor();
    let receipt = execute(
        &running.backend,
        "foreground-c",
        BackendCommand::PlayListTrack {
            list_id: "default".into(),
            music_id: "kw_c".into(),
            quality: None,
        },
    );
    terminal(&running.backend, cursor, &receipt.operation_id, true);
    let pending = wait(&running.backend, |state| {
        state.pending_playback_url.is_some()
    });
    assert!(
        began.elapsed() < Duration::from_secs(4),
        "async cancellation must release the source lane before the five-second request finishes"
    );
    assert!(
        pending.pending_playback_url.unwrap().contains("counter=3"),
        "ordinary cancellation must preserve script globals"
    );
    assert_eq!(media.count("b"), 0);
    let stale = running.backend.execute(CommandEnvelope {
        request_id: "stale-old".into(),
        origin: "preload-test".into(),
        expected_revision: None,
        command: BackendCommand::ApprovePreloadUrl {
            operation_id: old.operation_id,
            url: format!("{}/b.wav?quality=320k&counter=2&token=private", media.base),
        },
    });
    assert!(stale.is_err());
    assert_eq!(media.count("b"), 0);
    execute(
        &running.backend,
        "url-c",
        BackendCommand::ApprovePlaybackUrl,
    );
    let playing = wait(&running.backend, |state| {
        state.playback.status == PlaybackStatus::Playing
            && state.playback.track_id.as_deref() == Some("kw_c")
    });
    assert_eq!(
        playing.approved_audio_identity.unwrap().source_epoch,
        source_epoch
    );
    execute(&running.backend, "stop", BackendCommand::Stop);
}

#[test]
fn ready_preload_is_cancelled_by_seek_queue_mutation_and_cache_clear_without_promotion() {
    let media = Media::new();
    let running = Running::new(&media, false, false);
    let pending = running.pending();
    running.approve(&pending);
    execute(
        &running.backend,
        "seek",
        BackendCommand::Seek { seconds: 1.0 },
    );
    assert_ne!(
        running
            .backend
            .snapshot()
            .preload
            .as_ref()
            .map(|p| p.state.as_str()),
        Some("ready")
    );
    execute(
        &running.backend,
        "mode-none",
        BackendCommand::SetPlayMode {
            mode: PlayMode::None,
        },
    );
    let stale = running.backend.execute(CommandEnvelope {
        request_id: "stale-approved".into(),
        origin: "preload-test".into(),
        expected_revision: None,
        command: BackendCommand::ApprovePreloadUrl {
            operation_id: pending.operation_id,
            url: pending.pending_url.unwrap(),
        },
    });
    assert!(stale.is_err());
    let cursor = running.backend.event_cursor();
    let receipt = execute(
        &running.backend,
        "clear-cache",
        BackendCommand::ClearAudioCache,
    );
    terminal(&running.backend, cursor, &receipt.operation_id, true);
    let started = Instant::now();
    execute(&running.backend, "stop", BackendCommand::Stop);
    wait(&running.backend, |state| {
        state.playback.status == PlaybackStatus::Idle
    });
    assert!(started.elapsed() < Duration::from_secs(1));
    assert!(
        !running
            .backend
            .events_after(0)
            .unwrap()
            .iter()
            .any(|event| matches!(event.payload, BackendEvent::PreloadPromoted { .. }))
    );
    running.backend.shutdown(Duration::from_secs(5)).unwrap();
}

#[test]
fn speculative_decode_failure_finishes_the_accepted_operation_and_keeps_current_pcm_playing() {
    let media = Media::new();
    let running = Running::new(&media, false, true);
    let pending = running.pending();
    let before = running.backend.snapshot().playback;
    let cursor = running.backend.event_cursor();
    let receipt = execute(
        &running.backend,
        "approve-bad",
        BackendCommand::ApprovePreloadUrl {
            operation_id: pending.operation_id,
            url: pending.pending_url.unwrap(),
        },
    );
    assert_eq!(receipt.state, ReceiptState::Accepted);
    terminal(&running.backend, cursor, &receipt.operation_id, false);
    let state = wait(&running.backend, |state| {
        state
            .preload
            .as_ref()
            .is_some_and(|preload| preload.state == "failed")
    });
    assert_eq!(
        state.preload.unwrap().failure.as_deref(),
        Some("preparation")
    );
    assert_eq!(state.playback.generation, before.generation);
    assert_eq!(state.playback.track_id.as_deref(), Some("kw_a"));
    assert_eq!(state.playback.status, PlaybackStatus::Playing);
    assert!(state.playback.position_seconds >= before.position_seconds);
    assert!(state.playback.error.is_none());
    assert_eq!(media.count("bad"), 1);
    execute(&running.backend, "stop", BackendCommand::Stop);
}

#[test]
fn cache_clear_after_ready_advances_to_the_same_candidate_with_fresh_explicit_consent() {
    let media = Media::new();
    let running = Running::new(&media, false, false);
    let pending = running.pending();
    running.approve(&pending);
    assert_eq!(media.count("b"), 1);
    let (ready, promotion_events) = LivePromotionEvents::start(
        &running.backend,
        &pending.operation_id,
        &pending.candidate.entry_id,
    );
    assert_eq!(ready.preload.as_ref().unwrap().state, "ready");
    let cursor = promotion_events.cursor.get();
    let receipt = execute(
        &running.backend,
        "clear-ready-cache",
        BackendCommand::ClearAudioCache,
    );
    terminal(&running.backend, cursor, &receipt.operation_id, true);
    let fallback = wait(&running.backend, |state| {
        promotion_events.poll(&running.backend);
        state
            .pending_playback_url
            .as_ref()
            .is_some_and(|url| url.contains("/b.wav") && url.contains("counter=2"))
            && state.playback.status == PlaybackStatus::Idle
            && state.playback.track_id.is_none()
    });
    assert_eq!(fallback.playback.track_id.as_deref(), None);
    assert!(fallback.approved_audio_identity.is_none());
    assert_eq!(fallback.pending_playback_url, pending.pending_url);
    assert_eq!(
        media.count("b"),
        1,
        "discarding a prepared lease must not request an unapproved replacement URL"
    );
    let BackendQueryResult::Queue(queue) = running
        .backend
        .query(BackendQuery::Queue {
            offset: 0,
            limit: 10,
        })
        .unwrap()
    else {
        panic!();
    };
    assert_eq!(queue.current.unwrap().entry_id, pending.candidate.entry_id);
    promotion_events.assert_continuous();
    assert!(!promotion_events.promoted.get());
    execute(
        &running.backend,
        "approve-fallback",
        BackendCommand::ApprovePlaybackUrl,
    );
    let resumed = wait(&running.backend, |state| {
        promotion_events.poll(&running.backend);
        state.playback.status == PlaybackStatus::Playing
            && state.playback.track_id.as_deref() == Some("kw_b")
    });
    let renewed = resumed.approved_audio_identity.unwrap();
    assert_eq!(renewed.actual_quality, "320k");
    assert_eq!(renewed.approval_id, "approve-fallback");
    assert_ne!(
        renewed.approval_epoch,
        ready.preload.unwrap().identity.unwrap().approval_epoch
    );
    assert_eq!(media.count("b"), 2);
    execute(&running.backend, "stop", BackendCommand::Stop);
    wait(&running.backend, |state| {
        promotion_events.poll(&running.backend);
        state.playback.status == PlaybackStatus::Idle
    });
    promotion_events.assert_continuous();
    assert!(
        !promotion_events.promoted.get(),
        "the discarded preload must never be promoted during fallback playback"
    );
}

#[test]
fn json_lines_cli_runs_resolution_approval_preparation_and_real_promotion() {
    use std::io::{BufRead, BufReader};
    use std::process::{Command, Stdio};
    let media = Media::new();
    let directory = tempfile::Builder::new()
        .prefix("lx-native-cli-preload-")
        .tempdir_in(std::env::temp_dir())
        .unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_lx-native-cli"))
        .args(["--audio", "memory", "--json-lines", "--data-dir"])
        .arg(directory.path().join("profile"))
        .current_dir(std::env::temp_dir())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    let output = child.stdout.take().unwrap();
    let (tx, responses) = std::sync::mpsc::channel();
    let reader = thread::spawn(move || {
        for line in BufReader::new(output).lines() {
            let Ok(line) = line else {
                break;
            };
            if tx
                .send(serde_json::from_str::<serde_json::Value>(&line).unwrap())
                .is_err()
            {
                break;
            }
        }
    });
    let exchange = |input: &mut std::process::ChildStdin, value: serde_json::Value| {
        writeln!(input, "{value}").unwrap();
        input.flush().unwrap();
        let response = responses.recv_timeout(Duration::from_secs(20)).unwrap();
        assert_eq!(response["ok"], true, "CLI operation failed");
        response["result"].clone()
    };
    let send = |input: &mut std::process::ChildStdin, id: &str, command: BackendCommand| {
        exchange(
            input,
            serde_json::to_value(CommandEnvelope {
                request_id: id.into(),
                origin: "cli-preload-test".into(),
                expected_revision: None,
                command,
            })
            .unwrap(),
        )
    };
    let poll = |input: &mut std::process::ChildStdin,
                predicate: &dyn Fn(&serde_json::Value) -> bool| {
        let deadline = Instant::now() + Duration::from_secs(18);
        loop {
            let status = exchange(input, serde_json::json!({"query":"status"}));
            if predicate(&status) {
                return status;
            }
            assert!(Instant::now() < deadline, "CLI state did not converge");
            thread::sleep(Duration::from_millis(20));
        }
    };
    let caps = exchange(&mut input, serde_json::json!({"query":"capabilities"}));
    assert!(
        caps["commands"]
            .as_array()
            .unwrap()
            .iter()
            .any(|command| command == "approve_preload_url")
    );
    send(
        &mut input,
        "lists",
        BackendCommand::ListAction {
            action: Box::new(ListAction::MusicAdd {
                list_id: "default".into(),
                tracks: vec![record("a", "128k"), record("b", "320k")],
                location: AddLocation::Bottom,
            }),
        },
    );
    send(
        &mut input,
        "mode",
        BackendCommand::SetPlayMode {
            mode: PlayMode::List,
        },
    );
    for id in ["a", "b"] {
        send(
            &mut input,
            &format!("lyric-{id}"),
            BackendCommand::SaveLyrics {
                id: format!("kw_{id}"),
                bundle: LyricBundle {
                    lyric: LyricField::Text("[00:00.000]Original cached lyric".into()),
                    ..Default::default()
                },
            },
        );
    }
    send(
        &mut input,
        "import",
        BackendCommand::ImportSourceText {
            script: media.source(false, false),
        },
    );
    let imported = poll(&mut input, &|status| {
        status["sources"]
            .as_array()
            .is_some_and(|sources| !sources.is_empty())
    });
    send(
        &mut input,
        "source-approve",
        BackendCommand::ApproveSource {
            id: imported["sources"][0]["id"].as_str().unwrap().into(),
        },
    );
    poll(&mut input, &|status| {
        status["sources"][0]["approved"] == true && status["busy"] == false
    });
    send(
        &mut input,
        "play-a",
        BackendCommand::PlayListTrack {
            list_id: "default".into(),
            music_id: "kw_a".into(),
            quality: None,
        },
    );
    poll(&mut input, &|status| {
        status["pending_playback_url"].is_string()
    });
    send(&mut input, "url-a", BackendCommand::ApprovePlaybackUrl);
    let awaiting = poll(&mut input, &|status| {
        status["preload"]["state"] == "awaiting_approval"
    });
    assert_eq!(media.count("b"), 0);
    let query = exchange(&mut input, serde_json::json!({"query":"preload"}));
    assert_eq!(query["kind"], "preload");
    assert_eq!(
        query["data"]["operation_id"],
        awaiting["preload"]["operation_id"]
    );
    send(
        &mut input,
        "url-b",
        BackendCommand::ApprovePreloadUrl {
            operation_id: awaiting["preload"]["operation_id"].as_str().unwrap().into(),
            url: awaiting["preload"]["pending_url"].as_str().unwrap().into(),
        },
    );
    poll(&mut input, &|status| status["preload"]["state"] == "ready");
    let promoted = poll(&mut input, &|status| {
        status["playback"]["track_id"] == "kw_b"
            && status["playback"]["status"] == "Playing"
            && status["playback"]["position_seconds"]
                .as_f64()
                .is_some_and(|position| position > 0.1)
    });
    assert_eq!(
        promoted["approved_audio_identity"]["actual_quality"],
        "320k"
    );
    assert_eq!(media.count("b"), 1);
    send(&mut input, "stop", BackendCommand::Stop);
    drop(input);
    let deadline = Instant::now() + Duration::from_secs(8);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            panic!("CLI shutdown did not finish");
        }
        thread::sleep(Duration::from_millis(15));
    };
    assert!(status.success());
    reader.join().unwrap();
}

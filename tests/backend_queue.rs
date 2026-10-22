use base64::{Engine, engine::general_purpose::STANDARD};
use flate2::{Compression, write::ZlibEncoder};
use lx_gpui_player::{
    backend::{
        BackendCommand, BackendConfig, BackendEvent, BackendHandle, BackendQuery,
        BackendQueryResult, CommandEnvelope, QueueItemRef, QueueSnapshot, ReceiptState,
    },
    domain::{
        lists::{AddLocation, DislikeAction, DislikeMusicInfo, ListAction, ListInfo},
        music::{LxMusicDtoVersion, TrackRecord},
    },
    platforms::{
        HttpRequest, HttpResponse, HttpTransport, Platform, ProviderContext, ProviderError,
        ProviderRegistry, ProviderRequest, ReqwestTransport, TransportConfig,
    },
    playback::queue::PlayMode,
};
use serde_json::json;
use std::{
    fs,
    io::{Read, Write},
    net::TcpListener,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};
use tempfile::TempDir;

struct FixtureServer {
    base: String,
    requests: Arc<Mutex<Vec<String>>>,
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}
impl FixtureServer {
    fn new() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let log = requests.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = stop.clone();
        let tone = original_tone(2.0);
        let lyric = fixture_lyric();
        let thread = thread::spawn(move || {
            let mut tasks = Vec::new();
            while !stopping.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let log = log.clone();
                        let tone = tone.clone();
                        let lyric = lyric.clone();
                        tasks.push(thread::spawn(move || {
                            serve_fixture(stream, log, tone, lyric)
                        }));
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2))
                    }
                    Err(error) => panic!("fixture failed: {error}"),
                }
            }
            for task in tasks {
                task.join().unwrap();
            }
        });
        Self {
            base,
            requests,
            stop,
            thread: Some(thread),
        }
    }
    fn audio_requests(&self) -> usize {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .filter(|request| request.contains("GET /tone.wav"))
            .count()
    }
}
impl Drop for FixtureServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.thread.take().unwrap().join().unwrap();
    }
}
fn serve_fixture(
    mut stream: std::net::TcpStream,
    log: Arc<Mutex<Vec<String>>>,
    tone: Vec<u8>,
    lyric: Vec<u8>,
) {
    stream
        .set_nonblocking(false)
        .expect("fixture accepted socket must be blocking");
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    let mut buffer = [0u8; 8192];
    let mut request = Vec::new();
    loop {
        let count = stream
            .read(&mut buffer)
            .expect("fixture HTTP request read failed");
        if count == 0 {
            return;
        }
        request.extend_from_slice(&buffer[..count]);
        assert!(
            request.len() <= 64 * 1024,
            "fixture request headers too large"
        );
        if request.windows(4).any(|value| value == b"\r\n\r\n") {
            break;
        }
    }
    let request = String::from_utf8_lossy(&request).into_owned();
    log.lock().unwrap().push(request.clone());
    let target = request
        .lines()
        .next()
        .unwrap()
        .split_whitespace()
        .nth(1)
        .unwrap();
    let (body, content) = if target.starts_with("/sdk-search") {
        (serde_json::to_vec(&json!({"TOTAL":"1","SHOW":"1","abslist":[{"MUSICRID":"MUSIC_7","SONGNAME":"Original oscillator","ARTIST":"Fixture composer","ALBUM":"Original fixtures","ALBUMID":"8","DURATION":"2","N_MINFO":"level:standard,bitrate:128,format:mp3,size:3m","EXTRA_ALIAS":"preserved"}]})).unwrap(),"application/json")
    } else if target.starts_with("/sdk-lyric") {
        (lyric, "application/octet-stream")
    } else {
        (tone, "audio/wav")
    };
    let header = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: {content}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    if let Err(error) = stream
        .write_all(header.as_bytes())
        .and_then(|()| stream.write_all(&body))
    {
        eprintln!("fixture response write was cancelled: {error}");
    }
}
fn original_tone(seconds: f64) -> Vec<u8> {
    let rate = 8000u32;
    let samples = (seconds * rate as f64) as u32;
    let size = samples * 2;
    let mut bytes = Vec::new();
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
    for n in 0..samples {
        let sample =
            ((n as f64 * 440.0 * std::f64::consts::TAU / rate as f64).sin() * 6000.0) as i16;
        bytes.extend(sample.to_le_bytes());
    }
    bytes
}
fn fixture_lyric() -> Vec<u8> {
    let text = b"[ti:Original fixture]\n[00:00.000]Original oscillator";
    let coded = text
        .iter()
        .enumerate()
        .map(|(i, byte)| byte ^ b"yeelion"[i % 7])
        .collect::<Vec<_>>();
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
    encoder
        .write_all(STANDARD.encode(coded).as_bytes())
        .unwrap();
    let mut output = b"tp=content\r\n\r\n".to_vec();
    output.extend(encoder.finish().unwrap());
    output
}
struct MetadataTransport {
    base: String,
    http: ReqwestTransport,
}
impl HttpTransport for MetadataTransport {
    fn execute(
        &self,
        mut request: HttpRequest,
        context: &ProviderContext,
    ) -> Result<HttpResponse, ProviderError> {
        request.url = format!(
            "{}/{}",
            self.base,
            if request.url.contains("mlyric") {
                "sdk-lyric"
            } else {
                "sdk-search"
            }
        );
        self.http.execute(request, context)
    }
}
struct Running {
    handle: BackendHandle,
    directory: TempDir,
    base: String,
}
impl Running {
    fn new(server: &FixtureServer) -> Self {
        let root = std::env::temp_dir().join("lx-native-backend-queue-fixtures");
        fs::create_dir_all(&root).unwrap();
        let directory = tempfile::Builder::new()
            .prefix("profile-")
            .tempdir_in(root)
            .unwrap();
        let mut config = BackendConfig::headless(directory.path().into());
        config.provider_registry = Some(Arc::new(ProviderRegistry::new(Arc::new(
            MetadataTransport {
                base: server.base.clone(),
                http: ReqwestTransport::new(TransportConfig {
                    proxy: None,
                    retries: 0,
                    max_response_bytes: 1024 * 1024,
                })
                .unwrap(),
            },
        ))));
        let handle = BackendHandle::start(config).unwrap();
        Self {
            handle,
            directory,
            base: server.base.clone(),
        }
    }
    fn source(&self, delay: u64) -> String {
        format!(
            r#"/*
 * @name Original queue source fixture
 * @version 1.0
 */
 lx.on(lx.EVENT_NAMES.request,async({{source,action,info}})=>{{
    if(source!=='kw'||action!=='musicUrl'||info.musicInfo.source!=='kw')throw Error('wrong platform route');
    await new Promise(resolve=>setTimeout(resolve,{delay}));
    if(info.musicInfo.name.includes('fails'))throw Error('original fixture resolver failure');
    return '{}/tone.wav?name='+encodeURIComponent(info.musicInfo.name)+'&id='+encodeURIComponent(info.musicInfo.songmid)+'&quality='+info.type;
 }});
 lx.send(lx.EVENT_NAMES.inited,{{sources:{{kw:{{type:'music',actions:['musicUrl'],qualitys:['128k']}}}}}});
 "#,
            self.base
        )
    }
    fn approve(&self, delay: u64) -> String {
        let receipt = execute(
            &self.handle,
            "import",
            BackendCommand::ImportSourceText {
                script: self.source(delay),
            },
        );
        terminal(&self.handle, &receipt.operation_id);
        let id = self.handle.snapshot().sources[0].id.clone();
        let receipt = execute(
            &self.handle,
            "approve",
            BackendCommand::ApproveSource { id: id.clone() },
        );
        terminal(&self.handle, &receipt.operation_id);
        id
    }
}
impl Drop for Running {
    fn drop(&mut self) {
        self.handle.shutdown(Duration::from_secs(5)).unwrap();
    }
}
fn execute(
    handle: &BackendHandle,
    id: &str,
    command: BackendCommand,
) -> lx_gpui_player::backend::CommandReceipt {
    handle
        .execute(CommandEnvelope {
            request_id: id.into(),
            origin: "queue-fixture".into(),
            expected_revision: None,
            command,
        })
        .unwrap()
}
fn terminal(handle: &BackendHandle, id: &str) -> bool {
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        for event in handle.events_after(0).unwrap() {
            if let BackendEvent::OperationFinished {
                operation_id,
                success,
                ..
            } = event.payload
                && operation_id == id
            {
                return success;
            }
        }
        assert!(
            Instant::now() < deadline,
            "operation did not terminate: {id}"
        );
        thread::sleep(Duration::from_millis(5));
    }
}
fn wait(
    handle: &BackendHandle,
    predicate: impl Fn(&lx_gpui_player::backend::BackendSnapshot) -> bool,
) {
    let deadline = Instant::now() + Duration::from_secs(6);
    loop {
        let snapshot = handle.snapshot();
        if predicate(&snapshot) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "state did not converge: {snapshot:?}"
        );
        thread::sleep(Duration::from_millis(5));
    }
}
fn queue(handle: &BackendHandle) -> QueueSnapshot {
    let BackendQueryResult::Queue(queue) = handle
        .query(BackendQuery::Queue {
            offset: 0,
            limit: 100,
        })
        .unwrap()
    else {
        panic!()
    };
    *queue
}
fn records(handle: &BackendHandle, list_id: &str) -> Vec<TrackRecord> {
    let BackendQueryResult::ListTracks(page) = handle
        .query(BackendQuery::ListTracks {
            list_id: list_id.into(),
            offset: 0,
            limit: 100,
        })
        .unwrap()
    else {
        panic!()
    };
    page.items
}
fn record(name: &str, song: &str) -> TrackRecord {
    TrackRecord::from_lx_dto(json!({"source":"kw","name":name,"singer":"Fixture composer","songmid":song,"albumName":"Original fixtures","interval":"00:02","types":[{"type":"128k","size":null}],"_types":{"128k":{"size":null}},"typeUrl":{},"albumId":"8"}),LxMusicDtoVersion::Legacy).unwrap()
}
fn add(handle: &BackendHandle, id: &str, list: &str, tracks: Vec<TrackRecord>) {
    execute(
        handle,
        id,
        BackendCommand::ListAction {
            action: Box::new(ListAction::MusicAdd {
                list_id: list.into(),
                tracks,
                location: AddLocation::Bottom,
            }),
        },
    );
}

#[test]
fn online_sdk_saved_list_copy_routes_real_worker_url_approval_and_memory_pcm() {
    let server = FixtureServer::new();
    let running = Running::new(&server);
    let before = running.handle.snapshot();
    let receipt = execute(
        &running.handle,
        "sdk",
        BackendCommand::ProviderRequest {
            platform: Platform::Kw,
            request: Box::new(ProviderRequest::Search {
                keyword: "original".into(),
                page: 1,
                limit: 1,
            }),
        },
    );
    assert_eq!(receipt.state, ReceiptState::Accepted);
    assert!(
        terminal(&running.handle, "sdk"),
        "{:?}",
        running.handle.provider_result("sdk")
    );
    assert_eq!(running.handle.snapshot().revision, before.revision);
    assert!(records(&running.handle, "default").is_empty());
    execute(
        &running.handle,
        "save-sdk",
        BackendCommand::SaveProviderTracks {
            operation_id: "sdk".into(),
            list_id: "default".into(),
            music_ids: Vec::new(),
            location: AddLocation::Bottom,
        },
    );
    let original = records(&running.handle, "default")[0].clone();
    let mut love = original.clone();
    love.title = "Love list copy".into();
    add(&running.handle, "save-love", "love", vec![love.clone()]);
    running.approve(10);
    execute(
        &running.handle,
        "no-loop",
        BackendCommand::SetPlayMode {
            mode: PlayMode::None,
        },
    );
    let receipt = execute(
        &running.handle,
        "play-love",
        BackendCommand::PlayListTrack {
            list_id: "love".into(),
            music_id: love.id.clone(),
            quality: None,
        },
    );
    assert_eq!(receipt.state, ReceiptState::Accepted);
    assert!(terminal(&running.handle, "play-love"));
    let snapshot = running.handle.snapshot();
    assert!(
        snapshot
            .pending_playback_url
            .as_deref()
            .unwrap()
            .contains("name=Love%20list%20copy")
    );
    assert!(
        snapshot
            .pending_playback_url
            .as_deref()
            .unwrap()
            .contains("id=7&quality=128k")
    );
    assert_eq!(server.audio_requests(), 0);
    execute(
        &running.handle,
        "approve-url",
        BackendCommand::ApprovePlaybackUrl,
    );
    wait(&running.handle, |snapshot| {
        snapshot.playback.status == lx_gpui_player::audio::PlaybackStatus::Playing
            && snapshot
                .now_playing
                .as_ref()
                .is_some_and(|track| track.title == "Love list copy")
    });
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let BackendQueryResult::RenderedPcm(Some(pcm)) =
            running.handle.query(BackendQuery::RenderedPcm).unwrap()
        else {
            panic!()
        };
        if pcm.peak > 0.01 && pcm.batches > 0 {
            break;
        }
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(10));
    }
    execute(&running.handle, "pause", BackendCommand::Pause);
    assert_eq!(
        records(&running.handle, "default")[0].title,
        "Original oscillator"
    );
    assert_eq!(records(&running.handle, "love")[0].title, "Love list copy");
    assert_eq!(queue(&running.handle).origin_list, "love");
    assert!(server.audio_requests() > 0);
}

#[test]
fn queue_temporary_refs_keep_their_list_scoped_copies_and_dislike_skips_in_order() {
    let server = FixtureServer::new();
    let running = Running::new(&server);
    running.approve(5);
    let a = record("A", "a");
    let b = record("B", "b");
    let c = record("C", "c");
    let mut temp = a.clone();
    temp.title = "Love A copy".into();
    add(
        &running.handle,
        "default",
        "default",
        vec![a.clone(), b.clone(), c.clone()],
    );
    add(&running.handle, "love", "love", vec![temp.clone()]);
    execute(
        &running.handle,
        "list-mode",
        BackendCommand::SetPlayMode {
            mode: PlayMode::List,
        },
    );
    execute(
        &running.handle,
        "play-a",
        BackendCommand::PlayListTrack {
            list_id: "default".into(),
            music_id: a.id.clone(),
            quality: None,
        },
    );
    terminal(&running.handle, "play-a");
    execute(
        &running.handle,
        "enqueue",
        BackendCommand::Enqueue {
            items: vec![QueueItemRef {
                list_id: "love".into(),
                track_id: temp.id.clone(),
            }],
            front: true,
        },
    );
    execute(&running.handle, "next-temp", BackendCommand::Next);
    assert!(terminal(&running.handle, "next-temp"));
    assert_eq!(queue(&running.handle).current.unwrap().list_id, "love");
    assert!(
        running
            .handle
            .snapshot()
            .pending_playback_url
            .unwrap()
            .contains("Love%20A%20copy")
    );
    execute(
        &running.handle,
        "dislike-b",
        BackendCommand::DislikeAction {
            action: DislikeAction::Add {
                infos: vec![DislikeMusicInfo {
                    name: "B".into(),
                    singer: String::new(),
                }],
            },
        },
    );
    execute(&running.handle, "next-c", BackendCommand::Next);
    terminal(&running.handle, "next-c");
    assert_eq!(queue(&running.handle).current.unwrap().track_id, c.id);
    assert_eq!(queue(&running.handle).origin_list, "default");
}

#[test]
fn explicit_quality_failure_never_silently_resolves_lower_quality() {
    let server = FixtureServer::new();
    let running = Running::new(&server);
    running.approve(5);
    let track = record("Quality fixture", "quality");
    add(&running.handle, "add", "default", vec![track.clone()]);
    execute(
        &running.handle,
        "disable-skip",
        BackendCommand::SetAutoSkipOnError { enabled: false },
    );
    execute(
        &running.handle,
        "strict",
        BackendCommand::PlayListTrack {
            list_id: "default".into(),
            music_id: track.id,
            quality: Some("flac".into()),
        },
    );
    assert!(!terminal(&running.handle, "strict"));
    assert!(
        running
            .handle
            .snapshot()
            .error
            .unwrap()
            .contains("explicitly requested quality")
    );
    assert_eq!(server.audio_requests(), 0);
    assert!(running.handle.snapshot().pending_playback_url.is_none());
}

#[test]
fn pause_during_real_worker_failure_keeps_pause_intent_and_cancels_error_skip_delay() {
    let server = FixtureServer::new();
    let running = Running::new(&server);
    running.approve(150);
    let bad = record("fails first", "bad");
    let good = record("Good", "good");
    add(&running.handle, "add", "default", vec![bad.clone(), good]);
    execute(
        &running.handle,
        "play-bad",
        BackendCommand::PlayListTrack {
            list_id: "default".into(),
            music_id: bad.id.clone(),
            quality: None,
        },
    );
    execute(&running.handle, "pause-pending", BackendCommand::Pause);
    assert!(!terminal(&running.handle, "play-bad"));
    assert!(!queue(&running.handle).skip_pending);
    thread::sleep(Duration::from_millis(5250));
    assert_eq!(queue(&running.handle).current.unwrap().track_id, bad.id);
    assert_eq!(server.audio_requests(), 0);
    assert!(!queue(&running.handle).skip_pending);
    execute(
        &running.handle,
        "retry-bad",
        BackendCommand::PlayListTrack {
            list_id: "default".into(),
            music_id: bad.id.clone(),
            quality: None,
        },
    );
    assert!(!terminal(&running.handle, "retry-bad"));
    assert!(queue(&running.handle).skip_pending);
    execute(&running.handle, "pause-delay", BackendCommand::Pause);
    assert!(!queue(&running.handle).skip_pending);
    thread::sleep(Duration::from_millis(5250));
    assert_eq!(queue(&running.handle).current.unwrap().track_id, bad.id);
    assert_eq!(server.audio_requests(), 0);
}

#[test]
fn restart_restores_paused_queue_and_position_without_source_execution_or_download() {
    let server = FixtureServer::new();
    let running = Running::new(&server);
    running.approve(5);
    let track = record("Restore", "restore");
    add(&running.handle, "add", "default", vec![track.clone()]);
    execute(
        &running.handle,
        "none",
        BackendCommand::SetPlayMode {
            mode: PlayMode::None,
        },
    );
    execute(
        &running.handle,
        "play",
        BackendCommand::PlayListTrack {
            list_id: "default".into(),
            music_id: track.id.clone(),
            quality: None,
        },
    );
    terminal(&running.handle, "play");
    execute(
        &running.handle,
        "approve-url",
        BackendCommand::ApprovePlaybackUrl,
    );
    wait(&running.handle, |snapshot| {
        snapshot.playback.status == lx_gpui_player::audio::PlaybackStatus::Playing
    });
    execute(&running.handle, "pause", BackendCommand::Pause);
    execute(
        &running.handle,
        "seek",
        BackendCommand::Seek { seconds: 0.35 },
    );
    wait(&running.handle, |snapshot| {
        snapshot.playback.status == lx_gpui_player::audio::PlaybackStatus::Paused
            && snapshot.playback.position_seconds >= 0.3
    });
    running.handle.shutdown(Duration::from_secs(5)).unwrap();
    let before_requests = server.requests.lock().unwrap().len();
    let reopened =
        BackendHandle::start(BackendConfig::headless(running.directory.path().into())).unwrap();
    let restored = queue(&reopened);
    assert!(restored.restore_pending);
    assert_eq!(restored.current.unwrap().track_id, track.id);
    assert_eq!(restored.mode, PlayMode::None);
    assert!(restored.position_ms >= 300);
    assert_eq!(
        reopened.snapshot().playback.status,
        lx_gpui_player::audio::PlaybackStatus::Paused
    );
    assert!(!reopened.snapshot().busy);
    thread::sleep(Duration::from_millis(100));
    assert_eq!(server.requests.lock().unwrap().len(), before_requests);
    execute(&reopened, "resume", BackendCommand::Resume);
    assert!(terminal(&reopened, "resume"));
    assert!(reopened.snapshot().pending_playback_url.is_some());
    assert_eq!(server.audio_requests(), 1);
    execute(
        &reopened,
        "approve-resume",
        BackendCommand::ApprovePlaybackUrl,
    );
    wait(&reopened, |snapshot| {
        snapshot.playback.status == lx_gpui_player::audio::PlaybackStatus::Playing
            && snapshot.playback.position_seconds >= 0.3
    });
    reopened.shutdown(Duration::from_secs(5)).unwrap();
}

#[test]
fn list_deletion_while_resolving_invalidates_old_generation_and_paused_current_clears() {
    let server = FixtureServer::new();
    let running = Running::new(&server);
    running.approve(200);
    execute(
        &running.handle,
        "create",
        BackendCommand::ListAction {
            action: Box::new(ListAction::Create {
                position: 0,
                lists: vec![ListInfo::user("user-one", "User list")],
            }),
        },
    );
    let a = record("First", "a");
    let b = record("Second", "b");
    add(
        &running.handle,
        "add",
        "user-one",
        vec![a.clone(), b.clone()],
    );
    execute(
        &running.handle,
        "play-first",
        BackendCommand::PlayListTrack {
            list_id: "user-one".into(),
            music_id: a.id.clone(),
            quality: None,
        },
    );
    execute(
        &running.handle,
        "delete-first",
        BackendCommand::ListAction {
            action: Box::new(ListAction::MusicRemove {
                list_id: "user-one".into(),
                ids: vec![a.id],
            }),
        },
    );
    assert!(terminal(&running.handle, "delete-first"));
    assert!(
        running
            .handle
            .snapshot()
            .pending_playback_url
            .unwrap()
            .contains("name=Second")
    );
    assert_eq!(queue(&running.handle).current.unwrap().track_id, b.id);
    execute(&running.handle, "pause", BackendCommand::Pause);
    execute(
        &running.handle,
        "delete-user-list",
        BackendCommand::ListAction {
            action: Box::new(ListAction::Remove {
                ids: vec!["user-one".into()],
            }),
        },
    );
    assert!(queue(&running.handle).current.is_none());
    assert!(running.handle.snapshot().pending_playback_url.is_none());
    assert_eq!(server.audio_requests(), 0);
}

#[test]
fn normal_queue_removal_changes_saved_membership_and_virtual_catalog_is_read_only() {
    let server = FixtureServer::new();
    let running = Running::new(&server);
    let catalog_entry = queue(&running.handle).entries[0].entry_id.clone();
    let revision = running.handle.snapshot().revision;
    let rejected = running
        .handle
        .execute(CommandEnvelope {
            request_id: "reject-catalog-delete".into(),
            origin: "queue-fixture".into(),
            expected_revision: None,
            command: BackendCommand::RemoveQueueEntry {
                entry_id: catalog_entry,
            },
        })
        .unwrap_err();
    assert_eq!(
        rejected.kind,
        lx_gpui_player::backend::ErrorKind::InvalidData
    );
    assert_eq!(running.handle.snapshot().revision, revision);
    let a = record("A", "a");
    let b = record("B", "b");
    add(&running.handle, "add", "default", vec![a, b.clone()]);
    execute(
        &running.handle,
        "select",
        BackendCommand::SelectList {
            list_id: "default".into(),
        },
    );
    let entry = queue(&running.handle)
        .entries
        .iter()
        .find(|entry| entry.track_id == b.id)
        .unwrap()
        .entry_id
        .clone();
    execute(
        &running.handle,
        "remove",
        BackendCommand::RemoveQueueEntry { entry_id: entry },
    );
    assert_eq!(records(&running.handle, "default").len(), 1);
    running.handle.shutdown(Duration::from_secs(5)).unwrap();
    let reopened =
        BackendHandle::start(BackendConfig::headless(running.directory.path().into())).unwrap();
    assert_eq!(queue(&reopened).entries.len(), 1);
    reopened.shutdown(Duration::from_secs(5)).unwrap();
}

#[test]
fn typed_list_and_queue_queries_do_not_mutate_repository_revision() {
    let server = FixtureServer::new();
    let running = Running::new(&server);
    add(&running.handle, "add", "default", vec![record("A", "a")]);
    let before = running.handle.snapshot().revision;
    for query in [
        BackendQuery::ListInfos,
        BackendQuery::ListTracks {
            list_id: "default".into(),
            offset: 0,
            limit: 1,
        },
        BackendQuery::Queue {
            offset: 0,
            limit: 1,
        },
        BackendQuery::DislikeRules,
        BackendQuery::PlaybackCheckpoint,
        BackendQuery::RenderedPcm,
    ] {
        running.handle.query(query).unwrap();
    }
    assert_eq!(running.handle.snapshot().revision, before);
}

#[test]
fn actual_audio_end_obeys_list_stop_while_manual_next_wraps_to_first() {
    let server = FixtureServer::new();
    let running = Running::new(&server);
    running.approve(5);
    let a = record("A", "a");
    let b = record("B", "b");
    add(
        &running.handle,
        "add",
        "default",
        vec![a.clone(), b.clone()],
    );
    execute(
        &running.handle,
        "list",
        BackendCommand::SetPlayMode {
            mode: PlayMode::List,
        },
    );
    execute(
        &running.handle,
        "play-last",
        BackendCommand::PlayListTrack {
            list_id: "default".into(),
            music_id: b.id.clone(),
            quality: None,
        },
    );
    assert!(terminal(&running.handle, "play-last"));
    execute(
        &running.handle,
        "approve-last",
        BackendCommand::ApprovePlaybackUrl,
    );
    wait(&running.handle, |snapshot| {
        snapshot.playback.status == lx_gpui_player::audio::PlaybackStatus::Playing
    });
    wait(&running.handle, |snapshot| {
        snapshot.playback.status == lx_gpui_player::audio::PlaybackStatus::Idle && !snapshot.busy
    });
    assert_eq!(queue(&running.handle).current.unwrap().track_id, b.id);
    assert!(running.handle.snapshot().pending_playback_url.is_none());
    assert_eq!(server.audio_requests(), 1);
    execute(&running.handle, "manual-next", BackendCommand::Next);
    assert!(terminal(&running.handle, "manual-next"));
    assert_eq!(queue(&running.handle).current.unwrap().track_id, a.id);
    assert!(
        running
            .handle
            .snapshot()
            .pending_playback_url
            .unwrap()
            .contains("name=A")
    );
}

#[test]
fn actual_worker_uses_current_list_metadata_hashes_media_ids_and_unknown_fields() {
    let server = FixtureServer::new();
    let running = Running::new(&server);
    let script = format!(
        r#"/* @name Current metadata source fixture */
        lx.on(lx.EVENT_NAMES.request,async({{source,info}})=>{{
            const m=info.musicInfo;
            if(source==='kg'){{if(m.hash!=='current-hash'||m.albumAudioId!==200||m.future.version!=='current'||m.removeMe!==undefined||m._types.flac24bit.hash!=='current-quality'||m.types.some(q=>q.type==='flac32bit'))throw Error('stale KG metadata');}}
            else if(source==='tx'){{if(m.songId!==71||m.strMediaMid!=='current-media'||m.future.version!=='current'||!m.futureRoot.retained)throw Error('stale TX metadata');}}
            else throw Error('wrong source');
            return '{}/tone.wav?source='+source;
        }});
        lx.send(lx.EVENT_NAMES.inited,{{sources:{{kg:{{type:'music',actions:['musicUrl'],qualitys:['128k','flac24bit']}},tx:{{type:'music',actions:['musicUrl'],qualitys:['128k']}}}}}});
    "#,
        server.base
    );
    execute(
        &running.handle,
        "import-current-meta",
        BackendCommand::ImportSourceText { script },
    );
    assert!(terminal(&running.handle, "import-current-meta"));
    let source = running.handle.snapshot().sources[0].id.clone();
    execute(
        &running.handle,
        "approve-current-meta",
        BackendCommand::ApproveSource { id: source },
    );
    assert!(terminal(&running.handle, "approve-current-meta"));
    let original = json!({"source":"kg","songmid":"7","name":"KG","singer":"Composer","interval":"00:02","albumName":"Album","hash":"original-hash","albumAudioId":100,"future":{"version":"original"},"removeMe":"old","types":[{"type":"128k","size":null},{"type":"flac32bit","size":null,"hash":"original-quality"}],"_types":{"128k":{"size":null},"flac32bit":{"size":null,"hash":"original-quality"}}});
    let seed = TrackRecord::from_lx_dto(original.clone(), LxMusicDtoVersion::Legacy).unwrap();
    add(
        &running.handle,
        "seed-original",
        "default",
        vec![seed.clone()],
    );
    let mut current = seed;
    current
        .metadata
        .insert("hash".into(), json!("current-hash"));
    current.metadata.insert("albumAudioId".into(), json!(200));
    current
        .metadata
        .insert("future".into(), json!({"version":"current"}));
    current.metadata.remove("removeMe");
    current.metadata["_qualitys"]["flac24bit"]["hash"] = json!("current-quality");
    add(
        &running.handle,
        "save-current",
        "love",
        vec![current.clone()],
    );
    execute(
        &running.handle,
        "play-current-kg",
        BackendCommand::PlayListTrack {
            list_id: "love".into(),
            music_id: current.id,
            quality: None,
        },
    );
    assert!(
        terminal(&running.handle, "play-current-kg"),
        "{:?}",
        running.handle.snapshot().error
    );
    assert!(
        running
            .handle
            .snapshot()
            .pending_playback_url
            .unwrap()
            .ends_with("source=kg")
    );
    assert_eq!(
        records(&running.handle, "default")[0].metadata["hash"],
        "original-hash"
    );
    assert_eq!(
        records(&running.handle, "love")[0]
            .wire
            .as_ref()
            .unwrap()
            .original,
        original
    );
    let modern = json!({"id":"tx_7","source":"tx","name":"TX","singer":"Composer","interval":"00:02","futureRoot":{"retained":true},"meta":{"songId":"7","albumName":"Album","id":70,"strMediaMid":"original-media","future":{"version":"original"},"qualitys":[{"type":"128k","size":null}],"_qualitys":{"128k":{"size":null}}}});
    let mut current = TrackRecord::from_lx_dto(modern, LxMusicDtoVersion::Modern).unwrap();
    current.metadata.insert("id".into(), json!(71));
    current
        .metadata
        .insert("strMediaMid".into(), json!("current-media"));
    current
        .metadata
        .insert("future".into(), json!({"version":"current"}));
    add(
        &running.handle,
        "save-current-tx",
        "love",
        vec![current.clone()],
    );
    execute(
        &running.handle,
        "play-current-tx",
        BackendCommand::PlayListTrack {
            list_id: "love".into(),
            music_id: current.id,
            quality: None,
        },
    );
    assert!(
        terminal(&running.handle, "play-current-tx"),
        "{:?}",
        running.handle.snapshot().error
    );
    assert!(
        running
            .handle
            .snapshot()
            .pending_playback_url
            .unwrap()
            .ends_with("source=tx")
    );
    assert_eq!(server.audio_requests(), 0);
}

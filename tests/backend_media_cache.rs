//! URL information reuse preserves exact media consent across the real source session.
use lx_gpui_player::{
    audio::PlaybackStatus,
    backend::{
        BackendCommand, BackendConfig, BackendEvent, BackendHandle, BackendQuery,
        BackendQueryResult, BackendSnapshot, CommandEnvelope, ReceiptState,
    },
    domain::{
        lists::{AddLocation, ListAction, ListInfo},
        media::{MediaCacheLookup, track_record_fingerprint},
        music::{LxMusicDtoVersion, TrackRecord},
    },
    platforms::{
        HttpRequest, HttpResponse, HttpTransport, ProviderContext, ProviderError, ProviderRegistry,
    },
    playback::queue::PlayMode,
    repository::MediaCacheStats,
};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

static CASES: Mutex<()> = Mutex::new(());
const FIXTURE_HEADER_DEADLINE: Duration = Duration::from_secs(12);

#[derive(Clone, Debug)]
struct SourceCall {
    ordinal: usize,
    quality: String,
    platform: String,
    dto: Value,
    at: Instant,
}
struct HttpFixture {
    base: String,
    source_calls: Arc<Mutex<Vec<SourceCall>>>,
    media_calls: Arc<Mutex<Vec<String>>>,
    reject_media: Arc<AtomicBool>,
    fail_original: Arc<AtomicBool>,
    busy: Arc<AtomicBool>,
    hold_resolve: Arc<AtomicBool>,
    stopping: Arc<AtomicBool>,
    accepted_clients: Arc<AtomicUsize>,
    thread: Option<thread::JoinHandle<()>>,
}
impl HttpFixture {
    fn new() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let source_calls = Arc::new(Mutex::new(Vec::new()));
        let media_calls = Arc::new(Mutex::new(Vec::new()));
        let reject_media = Arc::new(AtomicBool::new(false));
        let fail_original = Arc::new(AtomicBool::new(false));
        let busy = Arc::new(AtomicBool::new(false));
        let hold_resolve = Arc::new(AtomicBool::new(false));
        let stopping = Arc::new(AtomicBool::new(false));
        let accepted_clients = Arc::new(AtomicUsize::new(0));
        let shared = (
            base.clone(),
            source_calls.clone(),
            media_calls.clone(),
            reject_media.clone(),
            fail_original.clone(),
            busy.clone(),
            hold_resolve.clone(),
            stopping.clone(),
            Arc::new(wave()),
        );
        let stop = stopping.clone();
        let accepted = accepted_clients.clone();
        let thread = thread::spawn(move || {
            let mut clients = Vec::new();
            while !stop.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((socket, _)) => {
                        accepted.fetch_add(1, Ordering::AcqRel);
                        let state = shared.clone();
                        clients.push(thread::spawn(move || serve(socket, state)));
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2))
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
            source_calls,
            media_calls,
            reject_media,
            fail_original,
            busy,
            hold_resolve,
            stopping,
            accepted_clients,
            thread: Some(thread),
        }
    }
    fn source_count(&self) -> usize {
        self.source_calls.lock().unwrap().len()
    }
    fn media_count(&self) -> usize {
        self.media_calls.lock().unwrap().len()
    }
    fn script(&self, tag: &str) -> String {
        format!(
            r#"/**
 * @name Media cache {tag}
 * @version 1.0
 */
let ordinal=0;
lx.on('request',async({{source,action,info}})=>{{
  if(action!=='musicUrl')throw Error('unexpected source action');
  const serial=++ordinal;
  const address={base}+'/resolve?ordinal='+serial+'&platform='+source+'&quality='+info.type+'&tag='+{tag_json}+'&dto='+encodeURIComponent(JSON.stringify(info.musicInfo));
  const body=await new Promise((resolve,reject)=>lx.request(address,{{}},(error,response,body)=>error?reject(error):resolve(body)));
  const value=typeof body==='string'?JSON.parse(body):body;
  if(value.busy)throw Error('服务器繁忙');
  if(value.error)throw Error('original track has no playable URL');
  return value.url;
}});
lx.send('inited',{{sources:{{kw:{{type:'music',actions:['musicUrl'],qualitys:['128k','flac']}},tx:{{type:'music',actions:['musicUrl'],qualitys:['128k','flac']}}}}}});
"#,
            base = serde_json::to_string(&self.base).unwrap(),
            tag_json = serde_json::to_string(tag).unwrap()
        )
    }
}
impl Drop for HttpFixture {
    fn drop(&mut self) {
        self.hold_resolve.store(false, Ordering::Release);
        self.stopping.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            thread.join().unwrap();
        }
    }
}
type HttpState = (
    String,
    Arc<Mutex<Vec<SourceCall>>>,
    Arc<Mutex<Vec<String>>>,
    Arc<AtomicBool>,
    Arc<AtomicBool>,
    Arc<AtomicBool>,
    Arc<AtomicBool>,
    Arc<AtomicBool>,
    Arc<Vec<u8>>,
);
fn serve(mut socket: TcpStream, state: HttpState) {
    let (base, source_calls, media_calls, reject_media, fail_original, busy, hold, stop, bytes) =
        state;
    // Accepted Windows sockets can inherit the listener's nonblocking mode.
    socket.set_nonblocking(false).unwrap();
    socket
        .set_read_timeout(Some(Duration::from_millis(100)))
        .unwrap();
    socket
        .set_write_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let mut request = Vec::new();
    let mut scratch = [0; 1024];
    let header_started = Instant::now();
    let peer = socket.peer_addr().ok();
    while !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
        if stop.load(Ordering::Acquire) {
            return;
        }
        if header_started.elapsed() >= FIXTURE_HEADER_DEADLINE {
            eprintln!(
                "HTTP fixture {peer:?}: header deadline reached after {} bytes",
                request.len()
            );
            return;
        }
        let count = match socket.read(&mut scratch) {
            Ok(count) => count,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                continue;
            }
            Err(error) => {
                eprintln!(
                    "HTTP fixture {peer:?}: read failed before complete headers after {} bytes: {error:?}",
                    request.len()
                );
                return;
            }
        };
        if count == 0 {
            if !request.is_empty() {
                eprintln!(
                    "HTTP fixture {peer:?}: client closed with incomplete headers after {} bytes",
                    request.len()
                );
            }
            return;
        }
        request.extend_from_slice(&scratch[..count]);
        if request.len() > 64 * 1024 {
            eprintln!("HTTP fixture {peer:?}: request headers exceeded 64 KiB");
            return;
        }
    }
    let request = String::from_utf8_lossy(&request);
    let Some(path) = request
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
    else {
        eprintln!("HTTP fixture {peer:?}: request line has no target");
        return;
    };
    let url = reqwest::Url::parse(&format!("{base}{path}")).unwrap();
    let params: BTreeMap<String, String> = url
        .query_pairs()
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect();
    if url.path() == "/resolve" {
        let call = SourceCall {
            ordinal: params["ordinal"].parse().unwrap(),
            quality: params["quality"].clone(),
            platform: params["platform"].clone(),
            dto: serde_json::from_str(&params["dto"]).unwrap(),
            at: Instant::now(),
        };
        source_calls.lock().unwrap().push(call);
        while hold.load(Ordering::Acquire) && !stop.load(Ordering::Acquire) {
            thread::sleep(Duration::from_millis(2));
        }
        if stop.load(Ordering::Acquire) {
            return;
        }
        let body = if busy.load(Ordering::Acquire) {
            json!({"busy":true})
        } else if fail_original.load(Ordering::Acquire) && params["platform"] == "kw" {
            json!({"error":true})
        } else {
            json!({"url":format!("{base}/media.wav?ordinal={}&quality={}&platform={}&tag={}",params["ordinal"],params["quality"],params["platform"],params["tag"])})
        };
        let body = serde_json::to_vec(&body).unwrap();
        if let Err(error)=write!(socket,"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",body.len()).and_then(|_|socket.write_all(&body)) {
            eprintln!("HTTP fixture {peer:?}: resolve response write failed: {error:?}");
        }
        return;
    }
    assert_eq!(url.path(), "/media.wav");
    media_calls.lock().unwrap().push(path.to_owned());
    if reject_media.load(Ordering::Acquire) {
        if let Err(error) = socket
            .write_all(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
        {
            eprintln!("HTTP fixture {peer:?}: rejected media response write failed: {error:?}");
        }
        return;
    }
    let range = request.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        if !name.eq_ignore_ascii_case("range") {
            return None;
        }
        let (start, end) = value.trim().strip_prefix("bytes=")?.split_once('-')?;
        Some((start.parse::<usize>().ok()?, end.parse::<usize>().ok()?))
    });
    let (start, end) = range.unwrap_or((0, bytes.len() - 1));
    if start >= bytes.len() {
        if let Err(error) = write!(
            socket,
            "HTTP/1.1 416 Range Not Satisfiable\r\nContent-Range: bytes */{}\r\nETag: \"original-tone\"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            bytes.len()
        ) {
            eprintln!("HTTP fixture {peer:?}: unsatisfied range response write failed: {error:?}");
        }
        return;
    }
    let end = end.min(bytes.len() - 1);
    let status = if range.is_some() {
        "206 Partial Content"
    } else {
        "200 OK"
    };
    let mut header = format!(
        "HTTP/1.1 {status}\r\nContent-Type: audio/wav\r\nContent-Length: {}\r\nETag: \"original-tone\"\r\nConnection: close\r\n",
        end - start + 1
    );
    if range.is_some() {
        header.push_str(&format!(
            "Content-Range: bytes {start}-{end}/{}\r\n",
            bytes.len()
        ));
    }
    header.push_str("\r\n");
    if let Err(error) = socket
        .write_all(header.as_bytes())
        .and_then(|_| socket.write_all(&bytes[start..=end]))
    {
        eprintln!("HTTP fixture {peer:?}: media range {start}..={end} write failed: {error:?}");
    }
}
fn wave() -> Vec<u8> {
    let frames = 48_000u32 * 6;
    let size = frames * 2;
    let mut bytes = Vec::with_capacity(size as usize + 44);
    bytes.extend(b"RIFF");
    bytes.extend((size + 36).to_le_bytes());
    bytes.extend(b"WAVEfmt ");
    bytes.extend(16u32.to_le_bytes());
    bytes.extend(1u16.to_le_bytes());
    bytes.extend(1u16.to_le_bytes());
    bytes.extend(48_000u32.to_le_bytes());
    bytes.extend(96_000u32.to_le_bytes());
    bytes.extend(2u16.to_le_bytes());
    bytes.extend(16u16.to_le_bytes());
    bytes.extend(b"data");
    bytes.extend(size.to_le_bytes());
    for frame in 0..frames {
        bytes.extend(
            (((frame as f64 * std::f64::consts::TAU * 440.0 / 48_000.0).sin() * 7000.0) as i16)
                .to_le_bytes(),
        );
    }
    bytes
}

#[derive(Default)]
struct MetadataTransport {
    requests: AtomicUsize,
    tx_requests: AtomicUsize,
    hold_tx: AtomicBool,
}
impl HttpTransport for MetadataTransport {
    fn execute(
        &self,
        request: HttpRequest,
        context: &ProviderContext,
    ) -> Result<HttpResponse, ProviderError> {
        context.check()?;
        self.requests.fetch_add(1, Ordering::AcqRel);
        let host = reqwest::Url::parse(&request.url)
            .unwrap()
            .host_str()
            .unwrap()
            .to_owned();
        if host != "u.y.qq.com" {
            return Err(ProviderError::Network {
                message: "fixture has no candidate for this platform".into(),
            });
        }
        self.tx_requests.fetch_add(1, Ordering::AcqRel);
        while self.hold_tx.load(Ordering::Acquire) {
            context.check()?;
            thread::sleep(Duration::from_millis(2));
        }
        let body: Value = serde_json::from_slice(&request.body).unwrap();
        assert_eq!(
            body["music.search.SearchCgiService"]["param"]["num_per_page"],
            25
        );
        let response = json!({"code":0,"music.search.SearchCgiService":{"code":0,"data":{"meta":{"sum":1},"body":{"song":{"list":[{"id":301,"mid":"actual-mid","title":"Original Tone","interval":200,"album":{"mid":"album-mid","name":"Original Album"},"singer":[{"name":"Artist A"},{"name":"Artist B"}],"file":{"media_mid":"actual-media-mid","size_128mp3":1024,"size_flac":4096}}]}}}}});
        Ok(HttpResponse {
            status: 200,
            headers: BTreeMap::new(),
            bytes: serde_json::to_vec(&response).unwrap(),
        })
    }
}
struct Running {
    backend: BackendHandle,
    directory: tempfile::TempDir,
    original: TrackRecord,
    metadata: Arc<MetadataTransport>,
}
impl Running {
    fn new() -> Self {
        let base = std::env::temp_dir().join("lx-native-backend-media-cache-tests");
        fs::create_dir_all(&base).unwrap();
        let directory = tempfile::Builder::new()
            .prefix("profile-")
            .tempdir_in(base)
            .unwrap();
        let metadata = Arc::new(MetadataTransport::default());
        let mut config = BackendConfig::headless(directory.path().join("profile"));
        config.provider_registry = Some(Arc::new(ProviderRegistry::new(metadata.clone())));
        let backend = BackendHandle::start(config).unwrap();
        let mut original=TrackRecord::from_lx_dto(json!({"source":"kw","songmid":"original","name":"Original Tone","singer":"Artist A、Artist B","albumName":"Original Album","hash":"initial-hash","interval":"03:20","types":[{"type":"128k"},{"type":"flac"}],"_types":{"128k":{},"flac":{}}}),LxMusicDtoVersion::Legacy).unwrap();
        original.duration_ms = Some(200000);
        command(
            &backend,
            "list",
            BackendCommand::ListAction {
                action: Box::new(ListAction::MusicAdd {
                    list_id: "default".into(),
                    tracks: vec![original.clone()],
                    location: AddLocation::Bottom,
                }),
            },
        );
        command(
            &backend,
            "mode",
            BackendCommand::SetPlayMode {
                mode: PlayMode::None,
            },
        );
        command(
            &backend,
            "skip-off",
            BackendCommand::SetAutoSkipOnError { enabled: false },
        );
        Self {
            backend,
            directory,
            original,
            metadata,
        }
    }
    fn source(&self, server: &HttpFixture, tag: &str) -> String {
        command_wait(
            &self.backend,
            &format!("source-{tag}"),
            BackendCommand::ImportSourceText {
                script: server.script(tag),
            },
        );
        let sources = self.backend.snapshot().sources;
        let expected_name = format!("Media cache {tag}");
        let source = sources
            .iter()
            .find(|source| source.name == expected_name)
            .unwrap_or_else(|| {
                panic!(
                    "Missing source fixture {expected_name}; actual names: {:?}",
                    sources
                        .iter()
                        .map(|source| &source.name)
                        .collect::<Vec<_>>()
                )
            })
            .id
            .clone();
        command_wait(
            &self.backend,
            &format!("approve-{tag}"),
            BackendCommand::ApproveSource { id: source.clone() },
        );
        source
    }
    fn play(&self, id: &str) -> String {
        command(
            &self.backend,
            id,
            BackendCommand::PlayListTrack {
                list_id: "default".into(),
                music_id: self.original.id.clone(),
                quality: None,
            },
        );
        wait(&self.backend, |state| state.pending_playback_url.is_some())
            .pending_playback_url
            .unwrap()
    }
    fn stop(&self, id: &str) {
        command(&self.backend, id, BackendCommand::Stop);
        wait(&self.backend, |state| {
            !state.busy
                && state.pending_playback_url.is_none()
                && state.playback.status == PlaybackStatus::Idle
        });
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let query = self.backend.query(BackendQuery::AudioCacheStats).unwrap();
            if let BackendQueryResult::AudioCacheStats(stats) = query
                && stats.is_none_or(|stats| stats.active_leases == 0)
            {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "stopped media tasks retained active leases"
            );
            thread::sleep(Duration::from_millis(5));
        }
    }
    fn approve_pcm(&self, id: &str) {
        command(&self.backend, id, BackendCommand::ApprovePlaybackUrl);
        wait(&self.backend, |state| {
            state.playback.status == PlaybackStatus::Playing
        });
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            if let BackendQueryResult::RenderedPcm(Some(pcm)) =
                self.backend.query(BackendQuery::RenderedPcm).unwrap()
                && pcm.peak > 0.01
                && pcm
                    .frequency_hz
                    .is_some_and(|frequency| (frequency - 440.0).abs() < 4.0)
            {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "approved fixture did not render real Memory PCM"
            );
            thread::sleep(Duration::from_millis(5));
        }
    }
}
impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.backend.shutdown(Duration::from_secs(10));
    }
}
fn command(
    backend: &BackendHandle,
    id: &str,
    command: BackendCommand,
) -> lx_gpui_player::backend::CommandReceipt {
    backend
        .execute(CommandEnvelope {
            request_id: id.into(),
            origin: "media-cache-test".into(),
            expected_revision: None,
            command,
        })
        .unwrap()
}
fn command_wait(backend: &BackendHandle, id: &str, value: BackendCommand) {
    let cursor = backend.event_cursor();
    let receipt = command(backend, id, value);
    if receipt.state != ReceiptState::Accepted {
        return;
    }
    let deadline = Instant::now() + Duration::from_secs(12);
    loop {
        for event in backend.events_after(cursor).unwrap() {
            if let BackendEvent::OperationFinished {
                operation_id,
                success,
                error,
            } = event.payload
                && operation_id == receipt.operation_id
            {
                assert!(success, "operation {id}: {error:?}");
                return;
            }
        }
        assert!(Instant::now() < deadline, "operation {id} did not finish");
        thread::sleep(Duration::from_millis(5));
    }
}
fn wait(backend: &BackendHandle, predicate: impl Fn(&BackendSnapshot) -> bool) -> BackendSnapshot {
    let deadline = Instant::now() + Duration::from_secs(12);
    loop {
        let state = backend.snapshot();
        if predicate(&state) {
            return state;
        }
        assert!(
            Instant::now() < deadline,
            "media operation did not converge: {state:?}"
        );
        thread::sleep(Duration::from_millis(5));
    }
}
fn cache(backend: &BackendHandle) -> MediaCacheStats {
    let BackendQueryResult::MediaCacheStats(stats) =
        backend.query(BackendQuery::MediaCacheStats).unwrap()
    else {
        panic!("missing media cache query")
    };
    stats
}
fn source_count(server: &HttpFixture, expected: usize) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while server.source_count() < expected {
        assert!(Instant::now() < deadline, "source request did not arrive");
        thread::sleep(Duration::from_millis(2));
    }
    assert_eq!(server.source_count(), expected);
}

fn operation_outcome(
    backend: &BackendHandle,
    cursor: u64,
    operation: &str,
) -> (bool, Option<String>) {
    let deadline = Instant::now() + Duration::from_secs(12);
    loop {
        for event in backend.events_after(cursor).unwrap() {
            if let BackendEvent::OperationFinished {
                operation_id,
                success,
                error,
            } = event.payload
                && operation_id == operation
            {
                return (success, error.map(|error| error.message));
            }
        }
        assert!(
            Instant::now() < deadline,
            "Operation {operation} did not terminate"
        );
        thread::sleep(Duration::from_millis(5));
    }
}

fn edited_list(running: &Running, list: &str, hash: &str) -> TrackRecord {
    command(
        &running.backend,
        &format!("create-{list}"),
        BackendCommand::ListAction {
            action: Box::new(ListAction::Create {
                position: 0,
                lists: vec![ListInfo::user(list, "Independent edit")],
            }),
        },
    );
    let mut edited = running.original.clone();
    edited.metadata.insert("hash".into(), hash.into());
    command(
        &running.backend,
        &format!("add-{list}"),
        BackendCommand::ListAction {
            action: Box::new(ListAction::MusicAdd {
                list_id: list.into(),
                tracks: vec![edited.clone()],
                location: AddLocation::Bottom,
            }),
        },
    );
    edited
}

struct TxGate(Arc<MetadataTransport>);
impl TxGate {
    fn hold(metadata: Arc<MetadataTransport>) -> Self {
        metadata.hold_tx.store(true, Ordering::Release);
        Self(metadata)
    }
    fn release(&self) {
        self.0.hold_tx.store(false, Ordering::Release);
    }
    fn wait_entered(&self) {
        let deadline = Instant::now() + Duration::from_secs(3);
        while self.0.tx_requests.load(Ordering::Acquire) == 0 {
            assert!(
                Instant::now() < deadline,
                "The native TX search did not enter its gate"
            );
            thread::sleep(Duration::from_millis(2));
        }
    }
}
impl Drop for TxGate {
    fn drop(&mut self) {
        self.release();
    }
}

#[test]
fn accepted_fixture_socket_waits_for_delayed_fragmented_http_headers() {
    let _case = CASES.lock().unwrap_or_else(|error| error.into_inner());
    let server = HttpFixture::new();
    let address = server.base.strip_prefix("http://").unwrap();
    let mut client = TcpStream::connect(address).unwrap();
    client
        .set_read_timeout(Some(FIXTURE_HEADER_DEADLINE))
        .unwrap();
    client
        .set_write_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let accepted_deadline = Instant::now() + Duration::from_secs(3);
    while server.accepted_clients.load(Ordering::Acquire) != 1 {
        assert!(
            Instant::now() < accepted_deadline,
            "Fixture did not accept the client socket"
        );
        thread::sleep(Duration::from_millis(2));
    }
    let mut url = reqwest::Url::parse(&format!("{}/resolve", server.base)).unwrap();
    url.query_pairs_mut().extend_pairs([
        ("ordinal", "1"),
        ("platform", "kw"),
        ("quality", "128k"),
        ("tag", "fragmented"),
        ("dto", "{}"),
    ]);
    let target = url.as_str().strip_prefix(&server.base).unwrap();
    let fragments = [
        format!("GET {target} HTTP/1.1\r\n"),
        format!("Host: {address}\r\n"),
        "Connection: close\r\n".into(),
    ];
    for fragment in fragments {
        thread::sleep(Duration::from_millis(60));
        client
            .write_all(fragment.as_bytes())
            .expect("Accepted socket closed before the complete request headers arrived");
        assert_eq!(
            server.source_count(),
            0,
            "Partial headers must not dispatch a source request"
        );
    }
    thread::sleep(Duration::from_millis(60));
    client.write_all(b"\r\n").unwrap();
    let mut response = String::new();
    client.read_to_string(&mut response).unwrap();
    assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "{response}");
    let body: Value = serde_json::from_str(response.split_once("\r\n\r\n").unwrap().1).unwrap();
    assert!(body["url"].as_str().unwrap().contains("ordinal=1"));
    assert_eq!(server.source_count(), 1);
    assert_eq!(server.source_calls.lock().unwrap()[0].dto, json!({}));
    assert_eq!(server.media_count(), 0);
}

#[test]
fn repeated_play_reuses_resolution_in_one_js_session_but_requires_each_exact_media_approval() {
    let _case = CASES.lock().unwrap_or_else(|error| error.into_inner());
    let server = HttpFixture::new();
    let running = Running::new();
    let source = running.source(&server, "v1");
    let first = running.play("play-first");
    assert_eq!(server.source_count(), 1);
    assert_eq!(server.media_count(), 0);
    assert_eq!(cache(&running.backend).resolved_entries, 1);
    running.approve_pcm("approve-first");
    let identity = running.backend.snapshot().approved_audio_identity.unwrap();
    assert_eq!(identity.track_id, running.original.id);
    assert_eq!(identity.actual_quality, "128k");
    assert_eq!(identity.source_sha256, source);
    running.stop("stop-first");
    let previous_gets = server.media_count();
    let second = running.play("play-cached");
    assert_eq!(second, first);
    assert_eq!(server.source_count(), 1);
    thread::sleep(Duration::from_millis(150));
    assert_eq!(server.media_count(), previous_gets);
    assert!(running.backend.snapshot().approved_audio_identity.is_none());
    running.approve_pcm("approve-cached");
    assert!(server.media_count() > previous_gets);
    let calls = server.source_calls.lock().unwrap();
    assert_eq!(calls[0].ordinal, 1);
    assert_eq!(calls[0].quality, "128k");
}

#[test]
fn cached_403_forces_at_most_two_new_resolutions_and_each_new_url_remains_pending_for_approval() {
    let _case = CASES.lock().unwrap_or_else(|error| error.into_inner());
    let server = HttpFixture::new();
    let running = Running::new();
    running.source(&server, "v1");
    let cached = running.play("seed");
    running.approve_pcm("seed-approval");
    running.stop("seed-stop");
    server.reject_media.store(true, Ordering::Release);
    let before = server.media_count();
    assert_eq!(running.play("cached-failure"), cached);
    assert_eq!(server.source_count(), 1);
    let mut previous = cached;
    for attempt in 1..=2 {
        command(
            &running.backend,
            &format!("failed-approval-{attempt}"),
            BackendCommand::ApprovePlaybackUrl,
        );
        let refreshed = wait(&running.backend, |state| {
            state
                .pending_playback_url
                .as_ref()
                .is_some_and(|url| url != &previous)
        });
        let next = refreshed.pending_playback_url.unwrap();
        assert_ne!(next, previous);
        assert_eq!(server.source_count(), attempt + 1);
        assert_eq!(server.media_count(), before + attempt);
        thread::sleep(Duration::from_millis(150));
        assert_eq!(server.media_count(), before + attempt);
        assert!(refreshed.approved_audio_identity.is_none());
        previous = next;
    }
    command(
        &running.backend,
        "third-rejected-approval",
        BackendCommand::ApprovePlaybackUrl,
    );
    wait(&running.backend, |state| {
        !state.busy
            && state.pending_playback_url.is_none()
            && state.playback.status == PlaybackStatus::Error
    });
    thread::sleep(Duration::from_millis(200));
    assert_eq!(server.source_count(), 3);
    assert_eq!(server.media_count(), before + 3);
    assert_eq!(
        server
            .source_calls
            .lock()
            .unwrap()
            .iter()
            .map(|call| call.ordinal)
            .collect::<Vec<_>>(),
        vec![1, 2, 3]
    );
}

#[test]
fn metadata_quality_source_refresh_and_cache_clear_fence_resolution_without_media_gets() {
    let _case = CASES.lock().unwrap_or_else(|error| error.into_inner());
    let server = HttpFixture::new();
    let running = Running::new();
    let first_source = running.source(&server, "v1");
    running.play("first");
    running.stop("stop-first");
    command(
        &running.backend,
        "quality",
        BackendCommand::SetPlaybackQuality {
            quality: "flac".into(),
        },
    );
    let quality = running.play("quality-play");
    assert!(quality.contains("quality=flac"));
    assert_eq!(server.source_count(), 2);
    running.stop("stop-quality");
    let mut changed = running.original.clone();
    changed
        .metadata
        .insert("hash".into(), "new-platform-hash".into());
    command(
        &running.backend,
        "metadata",
        BackendCommand::ListAction {
            action: Box::new(ListAction::MusicOverwrite {
                list_id: "default".into(),
                tracks: vec![changed],
            }),
        },
    );
    running.play("metadata-play");
    assert_eq!(server.source_count(), 3);
    assert_eq!(
        server.source_calls.lock().unwrap()[2].dto["hash"],
        "new-platform-hash"
    );
    running.stop("stop-metadata");
    let second_source = running.source(&server, "v2");
    assert_ne!(second_source, first_source);
    let fresh = running.play("source-play");
    assert!(fresh.contains("tag=v2"));
    assert_eq!(server.source_count(), 4);
    running.stop("source-stop-before-refresh");
    command(
        &running.backend,
        "refresh",
        BackendCommand::RefreshPlaybackUrl,
    );
    let refreshed = wait(&running.backend, |state| {
        state
            .pending_playback_url
            .as_ref()
            .is_some_and(|url| url != &fresh)
    })
    .pending_playback_url
    .unwrap();
    assert_ne!(refreshed, fresh);
    assert_eq!(server.source_count(), 5);
    running.stop("stop-refreshed");
    command(&running.backend, "clear", BackendCommand::ClearMediaCache);
    assert_eq!(cache(&running.backend).resolved_entries, 0);
    running.play("after-clear");
    assert_eq!(server.source_count(), 6);
    assert_eq!(server.media_count(), 0);
}

#[test]
fn cancelled_or_late_resolver_results_cannot_enter_the_information_cache() {
    let _case = CASES.lock().unwrap_or_else(|error| error.into_inner());
    let server = HttpFixture::new();
    let running = Running::new();
    running.source(&server, "v1");
    server.hold_resolve.store(true, Ordering::Release);
    command(
        &running.backend,
        "late-play",
        BackendCommand::PlayListTrack {
            list_id: "default".into(),
            music_id: running.original.id.clone(),
            quality: None,
        },
    );
    source_count(&server, 1);
    command(&running.backend, "cancel-late", BackendCommand::Stop);
    server.hold_resolve.store(false, Ordering::Release);
    wait(&running.backend, |state| {
        !state.busy && state.pending_playback_url.is_none()
    });
    thread::sleep(Duration::from_millis(200));
    assert_eq!(cache(&running.backend).resolved_entries, 0);
    assert_eq!(server.media_count(), 0);
    server.hold_resolve.store(true, Ordering::Release);
    command(
        &running.backend,
        "clear-late-play",
        BackendCommand::PlayListTrack {
            list_id: "default".into(),
            music_id: running.original.id.clone(),
            quality: None,
        },
    );
    source_count(&server, 2);
    command(
        &running.backend,
        "clear-late-cache",
        BackendCommand::ClearMediaCache,
    );
    server.hold_resolve.store(false, Ordering::Release);
    wait(&running.backend, |state| {
        !state.busy && state.pending_playback_url.is_none()
    });
    thread::sleep(Duration::from_millis(200));
    assert_eq!(cache(&running.backend).resolved_entries, 0);
    assert_eq!(server.media_count(), 0);
    running.play("after-cancel");
    assert_eq!(server.source_count(), 3);
    assert_eq!(server.source_calls.lock().unwrap()[2].ordinal, 3);
    assert_eq!(cache(&running.backend).resolved_entries, 1);
    running.stop("after-cancel-stop");
}

#[test]
fn matching_and_auto_switch_preserve_the_original_list_and_actual_source_quality_and_persist_the_flag()
 {
    let _case = CASES.lock().unwrap_or_else(|error| error.into_inner());
    let server = HttpFixture::new();
    let running = Running::new();
    running.source(&server, "v1");
    let BackendQueryResult::Queue(queue) = running
        .backend
        .query(BackendQuery::Queue {
            offset: 0,
            limit: 10,
        })
        .unwrap()
    else {
        panic!()
    };
    assert!(!queue.auto_switch_source);
    command_wait(
        &running.backend,
        "find",
        BackendCommand::FindMusicMatches {
            list_id: "default".into(),
            music_id: running.original.id.clone(),
        },
    );
    let BackendQueryResult::MusicMatches(MediaCacheLookup::Hit(found)) = running
        .backend
        .query(BackendQuery::MusicMatches {
            list_id: "default".into(),
            music_id: running.original.id.clone(),
        })
        .unwrap()
    else {
        panic!("ordered real native candidates were not cached")
    };
    assert_eq!(found.candidates.len(), 1);
    assert_eq!(found.candidates[0].track.platform.source, "tx");
    assert_eq!(found.candidates[0].track.platform.song_id, "actual-mid");
    assert!(
        found.candidates[0]
            .track
            .platform
            .qualities
            .contains(&"flac".into())
    );
    assert_eq!(server.media_count(), 0);
    assert_eq!(server.source_count(), 0);
    server.fail_original.store(true, Ordering::Release);
    let searches = running.metadata.requests.load(Ordering::Acquire);
    command(
        &running.backend,
        "switch-default-off",
        BackendCommand::PlayListTrack {
            list_id: "default".into(),
            music_id: running.original.id.clone(),
            quality: None,
        },
    );
    wait(&running.backend, |state| {
        !state.busy && state.error.is_some() && state.pending_playback_url.is_none()
    });
    assert_eq!(server.source_count(), 1);
    assert_eq!(server.source_calls.lock().unwrap()[0].platform, "kw");
    assert_eq!(running.metadata.requests.load(Ordering::Acquire), searches);
    assert_eq!(server.media_count(), 0);
    running.stop("default-off-stop");
    command(
        &running.backend,
        "switch-on",
        BackendCommand::SetAutoSwitchSource { enabled: true },
    );
    command(
        &running.backend,
        "flac",
        BackendCommand::SetPlaybackQuality {
            quality: "flac".into(),
        },
    );
    let pending = running.play("switch-play");
    assert!(pending.contains("platform=tx"));
    assert!(pending.contains("quality=flac"));
    assert_eq!(server.source_count(), 3);
    assert_eq!(server.media_count(), 0);
    assert_eq!(
        server
            .source_calls
            .lock()
            .unwrap()
            .iter()
            .map(|call| call.platform.clone())
            .collect::<Vec<_>>(),
        vec!["kw", "kw", "tx"]
    );
    let calls = server.source_calls.lock().unwrap();
    assert_eq!(calls[2].dto["songmid"], "actual-mid");
    assert_eq!(calls[2].dto["strMediaMid"], "actual-media-mid");
    drop(calls);
    running.approve_pcm("switch-approve");
    let identity = running.backend.snapshot().approved_audio_identity.unwrap();
    assert_eq!(identity.track_id, running.original.id);
    assert_eq!(identity.platform, "tx");
    assert_eq!(identity.actual_quality, "flac");
    let BackendQueryResult::ListTracks(data) = running
        .backend
        .query(BackendQuery::ListTracks {
            list_id: "default".into(),
            offset: 0,
            limit: 10,
        })
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(data.items, vec![running.original.clone()]);
    assert_eq!(data.total, 1);
    running.stop("switch-stop");
    let gets = server.media_count();
    running.play("matched-cache-play");
    assert_eq!(server.source_count(), 3);
    assert_eq!(server.media_count(), gets);
    running.backend.shutdown(Duration::from_secs(10)).unwrap();
    let restored = BackendHandle::start(BackendConfig::headless(
        running.directory.path().join("profile"),
    ))
    .unwrap();
    let BackendQueryResult::PlaybackCheckpoint(Some(checkpoint)) =
        restored.query(BackendQuery::PlaybackCheckpoint).unwrap()
    else {
        panic!()
    };
    assert!(checkpoint.auto_switch_source);
    restored.shutdown(Duration::from_secs(10)).unwrap();
    assert!(running.metadata.tx_requests.load(Ordering::Acquire) > 0);
}

#[test]
fn typed_server_busy_retries_three_times_with_two_and_four_second_delays_without_switching_candidates()
 {
    let _case = CASES.lock().unwrap_or_else(|error| error.into_inner());
    let server = HttpFixture::new();
    let running = Running::new();
    running.source(&server, "v1");
    server.busy.store(true, Ordering::Release);
    command(
        &running.backend,
        "switch-on",
        BackendCommand::SetAutoSwitchSource { enabled: true },
    );
    command(
        &running.backend,
        "busy-play",
        BackendCommand::PlayListTrack {
            list_id: "default".into(),
            music_id: running.original.id.clone(),
            quality: None,
        },
    );
    wait(&running.backend, |state| {
        !state.busy && state.error.is_some() && state.pending_playback_url.is_none()
    });
    let calls = server.source_calls.lock().unwrap();
    assert_eq!(calls.len(), 3);
    assert!(calls[1].at.duration_since(calls[0].at) >= Duration::from_millis(1900));
    assert!(calls[2].at.duration_since(calls[1].at) >= Duration::from_millis(3900));
    assert_eq!(
        calls.iter().map(|call| call.ordinal).collect::<Vec<_>>(),
        vec![1, 2, 3]
    );
    drop(calls);
    assert_eq!(running.metadata.requests.load(Ordering::Acquire), 0);
    assert_eq!(server.media_count(), 0);
    assert_eq!(cache(&running.backend).resolved_entries, 0);
    thread::sleep(Duration::from_millis(200));
    assert_eq!(server.source_count(), 3);
}

#[test]
fn pending_url_approval_rejects_changed_list_metadata_without_any_media_get() {
    let _case = CASES.lock().unwrap_or_else(|error| error.into_inner());
    let server = HttpFixture::new();
    let running = Running::new();
    running.source(&server, "v1");
    let old_url = running.play("pending-before-metadata");
    assert_eq!(server.source_count(), 1);
    assert_eq!(server.media_count(), 0);
    let mut changed = running.original.clone();
    changed
        .metadata
        .insert("hash".into(), "pending-metadata-revision".into());
    command(
        &running.backend,
        "update-pending-metadata",
        BackendCommand::ListAction {
            action: Box::new(ListAction::MusicOverwrite {
                list_id: "default".into(),
                tracks: vec![changed],
            }),
        },
    );
    let rejected = running
        .backend
        .execute(CommandEnvelope {
            request_id: "approve-stale-metadata".into(),
            origin: "media-cache-test".into(),
            expected_revision: None,
            command: BackendCommand::ApprovePlaybackUrl,
        })
        .unwrap_err();
    assert_eq!(rejected.kind, lx_gpui_player::backend::ErrorKind::Conflict);
    assert!(rejected.message.contains("metadata"), "{rejected:?}");
    assert_eq!(server.media_count(), 0);
    assert!(running.backend.snapshot().approved_audio_identity.is_none());
    let fresh_url = running.play("resolve-current-metadata");
    assert_ne!(fresh_url, old_url);
    assert_eq!(server.source_count(), 2);
    assert_eq!(
        server.source_calls.lock().unwrap()[1].dto["hash"],
        "pending-metadata-revision"
    );
    assert_eq!(server.media_count(), 0);
    running.approve_pcm("approve-current-metadata");
}

#[test]
fn an_inflight_metadata_edit_terminates_the_old_resolution_and_clears_busy_without_caching_it() {
    let _case = CASES.lock().unwrap_or_else(|error| error.into_inner());
    let server = HttpFixture::new();
    let running = Running::new();
    running.source(&server, "v1");
    server.hold_resolve.store(true, Ordering::Release);
    let cursor = running.backend.event_cursor();
    let receipt = command(
        &running.backend,
        "resolve-before-edit",
        BackendCommand::PlayListTrack {
            list_id: "default".into(),
            music_id: running.original.id.clone(),
            quality: None,
        },
    );
    assert_eq!(receipt.state, ReceiptState::Accepted);
    source_count(&server, 1);
    assert!(running.backend.snapshot().busy);
    let mut changed = running.original.clone();
    changed
        .metadata
        .insert("hash".into(), "inflight-metadata-revision".into());
    command(
        &running.backend,
        "edit-inflight-record",
        BackendCommand::ListAction {
            action: Box::new(ListAction::MusicOverwrite {
                list_id: "default".into(),
                tracks: vec![changed],
            }),
        },
    );
    server.hold_resolve.store(false, Ordering::Release);
    let (success, error) = operation_outcome(&running.backend, cursor, &receipt.operation_id);
    assert!(!success);
    assert!(error.is_some());
    wait(&running.backend, |state| {
        !state.busy && state.pending_playback_url.is_none()
    });
    assert_eq!(cache(&running.backend).resolved_entries, 0);
    assert_eq!(server.media_count(), 0);
    running.play("resolve-after-edit");
    assert_eq!(server.source_count(), 2);
    assert_eq!(
        server.source_calls.lock().unwrap()[1].dto["hash"],
        "inflight-metadata-revision"
    );
    assert_eq!(cache(&running.backend).resolved_entries, 1);
    assert_eq!(server.media_count(), 0);
}

#[test]
fn explicit_matching_uses_the_independently_edited_list_record_and_preserves_the_default_record() {
    let _case = CASES.lock().unwrap_or_else(|error| error.into_inner());
    let running = Running::new();
    let edited = edited_list(&running, "edited-list", "independent-list-hash");
    assert_ne!(
        track_record_fingerprint(&edited).unwrap(),
        track_record_fingerprint(&running.original).unwrap()
    );
    let before_revision = running.backend.snapshot().revision;
    command_wait(
        &running.backend,
        "find-edited-record",
        BackendCommand::FindMusicMatches {
            list_id: "edited-list".into(),
            music_id: edited.id.clone(),
        },
    );
    let BackendQueryResult::MusicMatches(MediaCacheLookup::Hit(found)) = running
        .backend
        .query(BackendQuery::MusicMatches {
            list_id: "edited-list".into(),
            music_id: edited.id.clone(),
        })
        .unwrap()
    else {
        panic!("The independent list matching result was not cached");
    };
    assert_eq!(
        found.key.original_record_sha256,
        track_record_fingerprint(&edited).unwrap()
    );
    assert_eq!(found.candidates.len(), 1);
    assert_eq!(found.candidates[0].track.platform.source, "tx");
    assert_eq!(running.backend.snapshot().revision, before_revision);
    let BackendQueryResult::ListTrack(Some(default_record)) = running
        .backend
        .query(BackendQuery::ListTrack {
            list_id: "default".into(),
            music_id: running.original.id.clone(),
        })
        .unwrap()
    else {
        panic!();
    };
    assert_eq!(*default_record, running.original);
    let BackendQueryResult::MusicMatches(default_cache) = running
        .backend
        .query(BackendQuery::MusicMatches {
            list_id: "default".into(),
            music_id: running.original.id.clone(),
        })
        .unwrap()
    else {
        panic!();
    };
    assert_eq!(default_cache, MediaCacheLookup::Miss);
    assert_eq!(cache(&running.backend).match_entries, 1);
}

#[test]
fn a_list_edit_during_a_real_native_matching_job_rejects_the_late_candidate_set() {
    let _case = CASES.lock().unwrap_or_else(|error| error.into_inner());
    let running = Running::new();
    let mut edited = edited_list(&running, "gated-list", "before-native-search");
    let gate = TxGate::hold(running.metadata.clone());
    let cursor = running.backend.event_cursor();
    let receipt = command(
        &running.backend,
        "gated-native-search",
        BackendCommand::FindMusicMatches {
            list_id: "gated-list".into(),
            music_id: edited.id.clone(),
        },
    );
    assert_eq!(receipt.state, ReceiptState::Accepted);
    gate.wait_entered();
    edited
        .metadata
        .insert("hash".into(), "after-native-search-started".into());
    command(
        &running.backend,
        "edit-gated-list",
        BackendCommand::ListAction {
            action: Box::new(ListAction::MusicOverwrite {
                list_id: "gated-list".into(),
                tracks: vec![edited.clone()],
            }),
        },
    );
    gate.release();
    let (success, error) = operation_outcome(&running.backend, cursor, &receipt.operation_id);
    assert!(!success);
    assert!(error.is_some_and(|message| message.contains("metadata")));
    assert_eq!(cache(&running.backend).match_entries, 0);
    let BackendQueryResult::MusicMatches(value) = running
        .backend
        .query(BackendQuery::MusicMatches {
            list_id: "gated-list".into(),
            music_id: edited.id.clone(),
        })
        .unwrap()
    else {
        panic!();
    };
    assert_eq!(value, MediaCacheLookup::Miss);
    assert!(!running.backend.snapshot().busy);
}

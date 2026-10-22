use lx_gpui_player::{
    audio::PlaybackStatus,
    backend::{
        BackendCommand, BackendConfig, BackendEvent, BackendHandle, BackendSnapshot,
        CommandEnvelope, ReceiptState,
    },
    domain::{
        lists::{AddLocation, ListAction},
        lyric::{LyricBundle, LyricField},
        music::{LxMusicDtoVersion, TrackRecord},
    },
    platforms::{
        HttpRequest, HttpResponse, HttpTransport, ProviderContext, ProviderError, ProviderRegistry,
        ReqwestTransport, TransportConfig,
    },
    playback::queue::PlayMode,
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

#[derive(Clone, Debug)]
pub struct SourceCall {
    pub platform: String,
    pub quality: String,
    pub ordinal: usize,
    pub dto: Value,
    pub at: Instant,
}
#[derive(Clone, Debug)]
pub struct MediaCall {
    pub target: String,
    pub platform: String,
    pub status: u16,
    pub at: Instant,
}
pub struct Shared {
    pub sources: Mutex<Vec<SourceCall>>,
    pub media: Mutex<Vec<MediaCall>>,
    pub searches: Mutex<Vec<String>>,
    pub main_status: AtomicUsize,
    pub candidate_status: AtomicUsize,
    pub rate_remaining: AtomicUsize,
    pub source_busy: AtomicBool,
    pub hold_source: AtomicBool,
    stop: AtomicBool,
}
pub struct Server {
    pub base: String,
    pub shared: Arc<Shared>,
    thread: Option<thread::JoinHandle<()>>,
}
impl Server {
    pub fn new() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let shared = Arc::new(Shared {
            sources: Mutex::new(Vec::new()),
            media: Mutex::new(Vec::new()),
            searches: Mutex::new(Vec::new()),
            main_status: AtomicUsize::new(200),
            candidate_status: AtomicUsize::new(200),
            rate_remaining: AtomicUsize::new(0),
            source_busy: AtomicBool::new(false),
            hold_source: AtomicBool::new(false),
            stop: AtomicBool::new(false),
        });
        let stopping = shared.clone();
        let origin = base.clone();
        let thread = thread::spawn(move || {
            let mut clients = Vec::new();
            while !stopping.stop.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((socket, _)) => {
                        let state = stopping.clone();
                        let base = origin.clone();
                        clients.push(thread::spawn(move || serve(socket, &base, state)));
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2))
                    }
                    Err(error) => panic!("recovery HTTP accept: {error}"),
                }
            }
            for client in clients {
                client.join().unwrap();
            }
        });
        Self {
            base,
            shared,
            thread: Some(thread),
        }
    }
    pub fn source_calls(&self) -> Vec<SourceCall> {
        self.shared.sources.lock().unwrap().clone()
    }
    pub fn media_calls(&self) -> Vec<MediaCall> {
        self.shared.media.lock().unwrap().clone()
    }
    pub fn search_calls(&self) -> Vec<String> {
        self.shared.searches.lock().unwrap().clone()
    }
    pub fn script(&self, tag: &str) -> String {
        format!(
            r#"/**
 * @name Recovery {tag}
 * @version 1.0
 */
let ordinal=0;
lx.on('request',async({{source,action,info}})=>{{
 if(action!=='musicUrl')throw Error('unexpected source action');
 if(source==='kw'&&String(info.musicInfo.songmid)!=='recovery-original')throw Error('incorrect original record');
 if(source==='tx'&&(info.musicInfo.songmid!=='actual-mid'||info.musicInfo.songId!==301||info.musicInfo.strMediaMid!=='actual-media-mid'||info.musicInfo.albumMid!=='actual-album-mid'))throw Error('unmapped actual candidate metadata');
 const address={base}+'/resolve?platform='+source+'&quality='+info.type+'&ordinal='+(++ordinal)+'&tag='+{tag_json}+'&dto='+encodeURIComponent(JSON.stringify(info.musicInfo));
 const body=await new Promise((resolve,reject)=>lx.request(address,{{}},(error,response,body)=>error?reject(error):resolve(body)));
 const value=typeof body==='string'?JSON.parse(body):body;
 if(value.busy)throw Error('服务器繁忙');
 return value.url;
}});
lx.send('inited',{{sources:{{kw:{{type:'music',actions:['musicUrl'],qualitys:['128k','320k','flac']}},tx:{{type:'music',actions:['musicUrl'],qualitys:['128k','320k']}}}}}});
"#,
            base = serde_json::to_string(&self.base).unwrap(),
            tag_json = serde_json::to_string(tag).unwrap()
        )
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        self.shared.hold_source.store(false, Ordering::Release);
        self.shared.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let result = thread.join();
            if !thread::panicking() {
                assert!(result.is_ok());
            }
        }
    }
}

fn serve(mut socket: TcpStream, base: &str, state: Arc<Shared>) {
    socket.set_nonblocking(false).unwrap();
    socket
        .set_read_timeout(Some(Duration::from_millis(100)))
        .unwrap();
    socket
        .set_write_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    let started = Instant::now();
    let mut bytes = Vec::new();
    let mut scratch = [0; 2048];
    let mut expected = None;
    loop {
        if state.stop.load(Ordering::Acquire) {
            return;
        }
        assert!(
            started.elapsed() < Duration::from_secs(12),
            "recovery request header/body deadline"
        );
        if let Some(length) = expected
            && bytes.len() >= length
        {
            break;
        }
        match socket.read(&mut scratch) {
            Ok(0) => return,
            Ok(count) => bytes.extend_from_slice(&scratch[..count]),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock
                        | std::io::ErrorKind::TimedOut
                        | std::io::ErrorKind::Interrupted
                ) =>
            {
                continue;
            }
            Err(_) => return,
        }
        if expected.is_none()
            && let Some(end) = bytes.windows(4).position(|window| window == b"\r\n\r\n")
        {
            let header = String::from_utf8_lossy(&bytes[..end]);
            let length = header
                .lines()
                .filter_map(|line| line.split_once(':'))
                .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                .map_or(0, |(_, value)| value.trim().parse::<usize>().unwrap());
            expected = Some(end + 4 + length);
        }
        assert!(bytes.len() <= 256 * 1024);
    }
    let end = bytes
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .unwrap();
    let header = String::from_utf8_lossy(&bytes[..end]);
    let target = header
        .lines()
        .next()
        .unwrap()
        .split_whitespace()
        .nth(1)
        .unwrap();
    let url = reqwest::Url::parse(&format!("{base}{target}")).unwrap();
    let parameters: BTreeMap<_, _> = url
        .query_pairs()
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect();
    let json_reply = |socket: &mut TcpStream, status: u16, value: Value| {
        let body = serde_json::to_vec(&value).unwrap();
        let _ = write!(socket, "HTTP/1.1 {status} Original\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).and_then(|()| socket.write_all(&body));
    };
    if url.path() == "/resolve" {
        state.sources.lock().unwrap().push(SourceCall {
            platform: parameters["platform"].clone(),
            quality: parameters["quality"].clone(),
            ordinal: parameters["ordinal"].parse().unwrap(),
            dto: serde_json::from_str(&parameters["dto"]).unwrap(),
            at: Instant::now(),
        });
        while state.hold_source.load(Ordering::Acquire) && !state.stop.load(Ordering::Acquire) {
            thread::sleep(Duration::from_millis(2));
        }
        if state.stop.load(Ordering::Acquire) {
            return;
        }
        let value = if state.source_busy.load(Ordering::Acquire) {
            json!({"busy":true})
        } else {
            json!({"url":format!("{base}/media.wav?platform={}&quality={}&ordinal={}&tag={}",parameters["platform"],parameters["quality"],parameters["ordinal"],parameters["tag"])})
        };
        json_reply(&mut socket, 200, value);
        return;
    }
    if url.path() == "/metadata" {
        let platform = parameters["platform"].clone();
        state.searches.lock().unwrap().push(platform.clone());
        if platform != "tx" {
            json_reply(
                &mut socket,
                503,
                json!({"error":"no candidate on this native platform"}),
            );
            return;
        }
        let payload: Value = serde_json::from_slice(&bytes[end + 4..]).unwrap();
        assert_eq!(
            payload["music.search.SearchCgiService"]["param"]["num_per_page"],
            25
        );
        json_reply(
            &mut socket,
            200,
            json!({"code":0,"music.search.SearchCgiService":{"code":0,"data":{"meta":{"sum":1},"body":{"song":{"list":[{"id":301,"mid":"actual-mid","title":"Original recovery tone","interval":12,"album":{"mid":"actual-album-mid","name":"Original recordings"},"singer":[{"name":"Fixture composer","mid":"actual-artist-mid"}],"file":{"media_mid":"actual-media-mid","size_128mp3":1024,"size_320mp3":4096}}]}}}}}),
        );
        return;
    }
    if url.path() == "/auxiliary" {
        json_reply(
            &mut socket,
            503,
            json!({"error":"this recording has no remote auxiliary metadata"}),
        );
        return;
    }
    assert_eq!(url.path(), "/media.wav");
    let platform = parameters["platform"].clone();
    let status = if platform == "kw" {
        if state
            .rate_remaining
            .try_update(Ordering::AcqRel, Ordering::Acquire, |left| {
                left.checked_sub(1)
            })
            .is_ok()
        {
            429
        } else {
            state.main_status.load(Ordering::Acquire) as u16
        }
    } else {
        state.candidate_status.load(Ordering::Acquire) as u16
    };
    if status != 200 {
        state.media.lock().unwrap().push(MediaCall {
            target: target.into(),
            platform: platform.clone(),
            status,
            at: Instant::now(),
        });
        let _ = write!(
            socket,
            "HTTP/1.1 {status} Rejected\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        );
        return;
    }
    let wave = wave(if platform == "tx" { 880.0 } else { 440.0 });
    let range = header
        .lines()
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| name.eq_ignore_ascii_case("range"))
        .map(|(_, value)| {
            let (start, end) = value
                .trim()
                .strip_prefix("bytes=")
                .unwrap()
                .split_once('-')
                .unwrap();
            (
                start.parse::<usize>().unwrap(),
                end.parse::<usize>().unwrap_or(wave.len() - 1),
            )
        });
    let (start, finish) = range.unwrap_or((0, wave.len() - 1));
    if start >= wave.len() {
        state.media.lock().unwrap().push(MediaCall {
            target: target.into(),
            platform: platform.clone(),
            status: 416,
            at: Instant::now(),
        });
        let _ = write!(
            socket,
            "HTTP/1.1 416 Original\r\nContent-Range: bytes */{}\r\nContent-Length: 0\r\nETag: \"{platform}-original\"\r\nConnection: close\r\n\r\n",
            wave.len()
        );
        return;
    }
    let finish = finish.min(wave.len() - 1);
    state.media.lock().unwrap().push(MediaCall {
        target: target.into(),
        platform: platform.clone(),
        status: if range.is_some() { 206 } else { 200 },
        at: Instant::now(),
    });
    let mut response = format!(
        "HTTP/1.1 {} Original\r\nContent-Type: audio/wav\r\nContent-Length: {}\r\nETag: \"{platform}-original\"\r\nConnection: close\r\n",
        if range.is_some() { 206 } else { 200 },
        finish - start + 1
    );
    if range.is_some() {
        response.push_str(&format!(
            "Content-Range: bytes {start}-{finish}/{}\r\n",
            wave.len()
        ));
    }
    response.push_str("\r\n");
    let _ = socket
        .write_all(response.as_bytes())
        .and_then(|()| socket.write_all(&wave[start..=finish]));
}

fn wave(frequency: f64) -> Vec<u8> {
    let rate = 8_000_u32;
    let frames = rate * 12;
    let size = frames * 2;
    let mut bytes = Vec::new();
    bytes.extend(b"RIFF");
    bytes.extend((size + 36).to_le_bytes());
    bytes.extend(b"WAVEfmt ");
    bytes.extend(16_u32.to_le_bytes());
    bytes.extend(1_u16.to_le_bytes());
    bytes.extend(1_u16.to_le_bytes());
    bytes.extend(rate.to_le_bytes());
    bytes.extend((rate * 2).to_le_bytes());
    bytes.extend(2_u16.to_le_bytes());
    bytes.extend(16_u16.to_le_bytes());
    bytes.extend(b"data");
    bytes.extend(size.to_le_bytes());
    for frame in 0..frames {
        let sample = (std::f64::consts::TAU * frequency * f64::from(frame) / f64::from(rate)).sin();
        bytes.extend(((sample * 6_000.0) as i16).to_le_bytes());
    }
    bytes
}

pub struct NativeMetadataRoute {
    delegate: ReqwestTransport,
    base: String,
}

fn netease_search(body: &[u8]) -> bool {
    use aes::{
        Aes128,
        cipher::{BlockDecrypt, KeyInit},
    };
    let Some(hex) = std::str::from_utf8(body)
        .ok()
        .and_then(|body| body.strip_prefix("params="))
    else {
        return false;
    };
    if !hex.len().is_multiple_of(32) {
        return false;
    }
    let mut data = Vec::new();
    for pair in hex.as_bytes().as_chunks::<2>().0 {
        let Some(byte) = std::str::from_utf8(pair)
            .ok()
            .and_then(|pair| u8::from_str_radix(pair, 16).ok())
        else {
            return false;
        };
        data.push(byte);
    }
    let cipher = Aes128::new_from_slice(b"e82ckenh8dichen8").unwrap();
    for chunk in data.as_chunks_mut::<16>().0 {
        let mut block = aes::cipher::Block::<Aes128>::default();
        block.copy_from_slice(chunk);
        cipher.decrypt_block(&mut block);
        chunk.copy_from_slice(&block);
    }
    data.starts_with(b"/api/search/song/list/page-36cd479b6b5-")
}
impl HttpTransport for NativeMetadataRoute {
    fn execute(
        &self,
        mut request: HttpRequest,
        context: &ProviderContext,
    ) -> Result<HttpResponse, ProviderError> {
        context.check()?;
        let endpoint = reqwest::Url::parse(&request.url)
            .map_err(|error| ProviderError::invalid(error.to_string()))?;
        let host = endpoint.host_str().unwrap_or_default().to_owned();
        let platform = if host == "u.y.qq.com" {
            "tx"
        } else if host.contains("kugou") {
            "kg"
        } else if host.contains("migu") {
            "mg"
        } else if host.contains("163") {
            "wy"
        } else if host.contains("kuwo") {
            "kw"
        } else {
            return Err(ProviderError::invalid("unowned native metadata endpoint"));
        };
        let search = match platform {
            "tx" => serde_json::from_slice::<Value>(&request.body)
                .ok()
                .is_some_and(|body| body.get("music.search.SearchCgiService").is_some()),
            "kg" => endpoint.path() == "/song_search_v2",
            "mg" => endpoint.path() == "/music_search/v3/search/searchAll",
            "kw" => {
                host == "search.kuwo.cn"
                    && endpoint
                        .query_pairs()
                        .any(|(key, value)| key == "ft" && value == "music")
            }
            "wy" => endpoint.path() == "/eapi/batch" && netease_search(&request.body),
            _ => false,
        };
        request.url = format!(
            "{}/{}?platform={platform}",
            self.base,
            if search { "metadata" } else { "auxiliary" }
        );
        self.delegate.execute(request, context)
    }
}

pub fn original(qualities: &[&str]) -> TrackRecord {
    let kinds: Vec<_> = qualities
        .iter()
        .map(|quality| json!({"type":quality}))
        .collect();
    let mut by_kind = serde_json::Map::new();
    for quality in qualities {
        by_kind.insert((*quality).into(), json!({}));
    }
    TrackRecord::from_lx_dto(json!({"source":"kw","songmid":"recovery-original","name":"Original recovery tone","singer":"Fixture composer","albumName":"Original recordings","interval":"00:12","types":kinds,"_types":by_kind}), LxMusicDtoVersion::Legacy).unwrap()
}
pub struct Running {
    pub backend: BackendHandle,
    pub record: TrackRecord,
    pub _directory: tempfile::TempDir,
}
impl Running {
    pub fn new(server: &Server, qualities: &[&str]) -> Self {
        let base = std::env::temp_dir().join("lx-native-backend-media-recovery-tests");
        fs::create_dir_all(&base).unwrap();
        let directory = tempfile::Builder::new()
            .prefix("case-")
            .tempdir_in(base)
            .unwrap();
        let metadata = NativeMetadataRoute {
            delegate: ReqwestTransport::new(TransportConfig {
                retries: 0,
                ..Default::default()
            })
            .unwrap(),
            base: server.base.clone(),
        };
        let mut config = BackendConfig::headless(directory.path().join("profile"));
        config.provider_registry = Some(Arc::new(ProviderRegistry::new(Arc::new(metadata))));
        config.provider_timeout = Duration::from_secs(5);
        let backend = BackendHandle::start(config).unwrap();
        let record = original(qualities);
        command(
            &backend,
            "list",
            BackendCommand::ListAction {
                action: Box::new(ListAction::MusicAdd {
                    list_id: "default".into(),
                    tracks: vec![record.clone()],
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
            "original-empty-lyric",
            BackendCommand::SaveLyrics {
                id: record.id.clone(),
                bundle: LyricBundle {
                    lyric: LyricField::Text(String::new()),
                    ..Default::default()
                },
            },
        );
        command(
            &backend,
            "skip-off",
            BackendCommand::SetAutoSkipOnError { enabled: false },
        );
        let running = Self {
            backend,
            record,
            _directory: directory,
        };
        running.source(server, "primary");
        running
    }
    pub fn source(&self, server: &Server, tag: &str) {
        command(
            &self.backend,
            &format!("import-{tag}"),
            BackendCommand::ImportSourceText {
                script: server.script(tag),
            },
        );
        let name = format!("Recovery {tag}");
        let source = self
            .backend
            .snapshot()
            .sources
            .iter()
            .find(|source| source.name == name)
            .unwrap()
            .id
            .clone();
        command(
            &self.backend,
            &format!("approve-source-{tag}"),
            BackendCommand::ApproveSource { id: source },
        );
    }
    pub fn play(&self, id: &str, quality: Option<&str>) -> String {
        command(
            &self.backend,
            id,
            BackendCommand::PlayListTrack {
                list_id: "default".into(),
                music_id: self.record.id.clone(),
                quality: quality.map(str::to_owned),
            },
        );
        wait(&self.backend, |state| state.pending_playback_url.is_some())
            .pending_playback_url
            .unwrap()
    }
    pub fn approve(&self, id: &str) {
        command(&self.backend, id, BackendCommand::ApprovePlaybackUrl);
    }
}
impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.backend.shutdown(Duration::from_secs(10));
    }
}
pub fn command(backend: &BackendHandle, id: &str, command: BackendCommand) {
    let cursor = backend.event_cursor();
    let receipt = backend
        .execute(CommandEnvelope {
            request_id: id.into(),
            origin: "media-recovery-test".into(),
            expected_revision: None,
            command,
        })
        .unwrap();
    if receipt.state != ReceiptState::Accepted {
        return;
    }
    let started = Instant::now();
    loop {
        for event in backend.events_after(cursor).unwrap() {
            if let BackendEvent::OperationFinished {
                operation_id,
                success,
                error,
            } = event.payload
                && operation_id == receipt.operation_id
            {
                assert!(success, "{id}: {error:?}");
                return;
            }
        }
        assert!(
            started.elapsed() < Duration::from_secs(18),
            "{id} acknowledgement deadline"
        );
        thread::sleep(Duration::from_millis(5));
    }
}
pub fn wait(
    backend: &BackendHandle,
    predicate: impl Fn(&BackendSnapshot) -> bool,
) -> BackendSnapshot {
    let started = Instant::now();
    loop {
        let state = backend.snapshot();
        if predicate(&state) {
            return state;
        }
        assert!(started.elapsed() < Duration::from_secs(18), "{state:?}");
        thread::sleep(Duration::from_millis(5));
    }
}
pub fn wait_media(server: &Server, minimum: usize) {
    let started = Instant::now();
    while server.media_calls().len() < minimum {
        assert!(started.elapsed() < Duration::from_secs(18));
        thread::sleep(Duration::from_millis(5));
    }
}
pub fn wait_source(server: &Server, minimum: usize) {
    let started = Instant::now();
    while server.source_calls().len() < minimum {
        assert!(started.elapsed() < Duration::from_secs(18));
        thread::sleep(Duration::from_millis(5));
    }
}
pub fn idle(backend: &BackendHandle) {
    wait(backend, |state| {
        !state.busy && state.playback.status == PlaybackStatus::Idle
    });
}

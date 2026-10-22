//! Real loopback HTTP/SSE protocol fixtures; this adapter has no filesystem,
//! resolver, source approval, or audio effect capabilities.
use lx_gpui_player::open_api::*;
use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::{
    Arc, Condvar, Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use std::time::{Duration, Instant};

static CASES: Mutex<()> = Mutex::new(());
#[derive(Default)]
struct State {
    status: LxPlayerStatus,
    cursor: u64,
    actions: Vec<ControlAction>,
    journal: VecDeque<StatusUpdate>,
}
#[derive(Default)]
struct ProtocolBackend {
    state: Mutex<State>,
    expired: AtomicBool,
    gate: Mutex<Option<Arc<Gate>>>,
    artwork: Mutex<Option<(ArtworkReference, ArtworkResponse)>>,
    artwork_calls: AtomicUsize,
}
struct Gate {
    entered: AtomicBool,
    released: Mutex<bool>,
    wake: Condvar,
}
struct ReleaseGuard(Arc<Gate>);
impl Drop for ReleaseGuard {
    fn drop(&mut self) {
        self.0.release();
    }
}
impl Gate {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            entered: AtomicBool::new(false),
            released: Mutex::new(false),
            wake: Condvar::new(),
        })
    }
    fn release(&self) {
        *self.released.lock().unwrap() = true;
        self.wake.notify_all();
    }
}
impl Drop for Gate {
    fn drop(&mut self) {
        *self.released.get_mut().unwrap() = true;
        self.wake.notify_all();
    }
}
impl ProtocolBackend {
    fn update(&self, change: impl FnOnce(&mut LxPlayerStatus)) {
        let mut state = self.state.lock().unwrap();
        change(&mut state.status);
        state.cursor += 1;
        let update = StatusUpdate {
            cursor: state.cursor,
            status: state.status.clone(),
        };
        state.journal.push_back(update);
        if state.journal.len() > 256 {
            state.journal.pop_front();
        }
    }
}
impl ExternalControlBackend for ProtocolBackend {
    fn snapshot(&self) -> ControlResult<StatusSnapshot> {
        let state = self.state.lock().unwrap();
        Ok(StatusSnapshot {
            status: state.status.clone(),
            cursor: state.cursor,
        })
    }
    fn updates_after(&self, cursor: u64) -> ControlResult<Vec<StatusUpdate>> {
        if self.expired.swap(false, Ordering::AcqRel) {
            return Err(ControlError::new(
                ControlErrorKind::CursorExpired,
                "Expired",
            ));
        }
        Ok(self
            .state
            .lock()
            .unwrap()
            .journal
            .iter()
            .filter(|event| event.cursor > cursor)
            .cloned()
            .collect())
    }
    fn artwork(&self, reference: ArtworkReference) -> ControlResult<ArtworkResponse> {
        self.artwork_calls.fetch_add(1, Ordering::AcqRel);
        self.artwork
            .lock()
            .unwrap()
            .as_ref()
            .filter(|(current, _)| current == &reference)
            .map(|(_, response)| response.clone())
            .ok_or_else(|| {
                ControlError::new(ControlErrorKind::Unavailable, "Current artwork changed")
            })
    }
    fn control(&self, epoch: u64, action: ControlAction) -> ControlResult<()> {
        if epoch != 7 {
            return Err(ControlError::new(
                ControlErrorKind::Unauthorized,
                "Stale session",
            ));
        }
        let gate = { self.gate.lock().unwrap().clone() };
        if let Some(gate) = gate {
            gate.entered.store(true, Ordering::Release);
            let mut released = gate.released.lock().unwrap();
            while !*released {
                released = gate.wake.wait(released).unwrap();
            }
        }
        if let ControlAction::Seek { seconds } = action
            && seconds > self.state.lock().unwrap().status.duration
        {
            return Err(ControlError::new(
                ControlErrorKind::InvalidData,
                "Invalid offset",
            ));
        }
        if let ControlAction::SeekChecked {
            requested_seconds, ..
        } = action
            && requested_seconds > self.state.lock().unwrap().status.duration
        {
            return Err(ControlError::new(
                ControlErrorKind::InvalidData,
                "Invalid offset",
            ));
        }
        self.state.lock().unwrap().actions.push(action);
        Ok(())
    }
}
fn eventually(timeout: Duration, predicate: impl Fn() -> bool) {
    let start = Instant::now();
    while !predicate() {
        assert!(start.elapsed() < timeout, "condition deadline exceeded");
        std::thread::sleep(Duration::from_millis(5));
    }
}
fn start(backend: Arc<ProtocolBackend>, mut config: ServerConfig) -> OpenApiServer {
    config.port = 0;
    config.session_epoch = 7;
    let server = OpenApiServer::start(config, backend).unwrap();
    eventually(Duration::from_secs(2), || {
        server.view().state == ServerState::Listening
    });
    server
}
fn stop(server: &mut OpenApiServer) {
    let address = server.view().address.unwrap();
    assert!(server.shutdown(Duration::from_secs(5)).unwrap());
    assert_eq!(server.view().connections, 0);
    assert_eq!(server.view().subscriptions, 0);
    assert_eq!(server.view().state, ServerState::Disabled);
    let listener = TcpListener::bind(address).unwrap();
    drop(listener);
}
fn socket(address: SocketAddr) -> TcpStream {
    let socket = TcpStream::connect(address).unwrap();
    socket
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    socket
        .set_write_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    socket
}
fn request(address: SocketAddr, target: &str, extra: &str) -> String {
    let mut socket = socket(address);
    write!(
        socket,
        "GET {target} HTTP/1.1\r\nHost: {address}\r\n{extra}\r\n"
    )
    .unwrap();
    let mut response = String::new();
    socket.read_to_string(&mut response).unwrap();
    response
}
fn body(response: &str) -> &str {
    response.split_once("\r\n\r\n").unwrap().1
}
fn subscribe(address: SocketAddr, filter: &str, extra: &str) -> BufReader<TcpStream> {
    let mut socket = socket(address);
    write!(
        socket,
        "GET /subscribe-player-status?filter={filter} HTTP/1.1\r\nHost: {address}\r\n{extra}\r\n"
    )
    .unwrap();
    let mut socket = BufReader::new(socket);
    let mut line = String::new();
    socket.read_line(&mut line).unwrap();
    assert!(line.starts_with("HTTP/1.1 200"), "{line}");
    loop {
        line.clear();
        socket.read_line(&mut line).unwrap();
        if line == "\r\n" {
            break;
        }
    }
    socket
}
fn frame(socket: &mut BufReader<TcpStream>) -> String {
    let mut frame = String::new();
    loop {
        let mut line = String::new();
        assert!(socket.read_line(&mut line).unwrap() > 0);
        let end = line == "\n";
        frame.push_str(&line);
        if end {
            break;
        }
    }
    frame
}

#[test]
fn exact_status_filter_json_and_named_sse_goldens() {
    let status = LxPlayerStatus::default();
    assert_eq!(STATUS_FIELDS.len(), 17);
    assert_eq!(
        String::from_utf8(parse_filter(None).project(&status).unwrap()).unwrap(),
        "{\"status\":\"stoped\",\"name\":\"\",\"singer\":\"\",\"albumName\":\"\",\"lyricLineText\":\"\",\"duration\":0,\"progress\":0,\"playbackRate\":1}"
    );
    assert_eq!(
        parse_filter(Some("progress,status,progress,bogus")).fields(),
        &["status", "progress"]
    );
    assert_eq!(parse_filter(Some(" name")).fields(), &DEFAULT_FILTER);
    assert_eq!(
        parse_filter(Some("volume,mute"))
            .frames(&status, None)
            .unwrap(),
        vec![
            ("volume".into(), b"event: volume\ndata: 0\n\n".to_vec()),
            ("mute".into(), b"event: mute\ndata: false\n\n".to_vec())
        ]
    );
    let frames = parse_filter(None).frames(&status, None).unwrap();
    assert_eq!(frames[4].0, "progress");
    assert_eq!(frames[5].0, "duration");
    let mut text = status.clone();
    text.lyric_line_text = "a\n\"b\"".into();
    assert_eq!(
        parse_filter(Some("lyricLineText"))
            .frames(&text, Some(&status))
            .unwrap()[0]
            .1,
        b"event: lyricLineText\ndata: \"a\\n\\\"b\\\"\"\n\n"
    );
}
#[test]
fn js_numeric_prefix_duplicate_query_and_thirteen_route_goldens() {
    for (input, percent) in [
        ("10.9", 10),
        ("10x", 10),
        ("0x10", 16),
        ("-0", 0),
        ("1e2", 1),
    ] {
        assert_eq!(
            parse_route(&format!("/volume?volume={input}")).unwrap(),
            Route::Control(ControlAction::SetVolume { percent })
        );
    }
    assert_eq!(
        parse_route("/volume?volume=1&volume=2").unwrap(),
        Route::Control(ControlAction::SetVolume { percent: 1 })
    );
    assert_eq!(
        parse_route("/seek?offset=1.23456").unwrap(),
        Route::Control(ControlAction::SeekChecked {
            requested_seconds: 1.23456,
            seconds: 1.235
        })
    );
    assert_eq!(
        parse_route("/seek?offset=1.2junk").unwrap(),
        Route::Control(ControlAction::SeekChecked {
            requested_seconds: 1.2,
            seconds: 1.2
        })
    );
    assert_eq!(
        parse_route("/seek?offset=1.2345").unwrap(),
        Route::Control(ControlAction::SeekChecked {
            requested_seconds: 1.2345,
            seconds: 1.234
        })
    );
    assert_eq!(
        parse_route("/volume?volume=%C2%A010").unwrap(),
        Route::Control(ControlAction::SetVolume { percent: 10 })
    );
    assert!(parse_route("/volume?volume=%C2%8510").is_err());
    assert_eq!(
        parse_route("/seek?offset=1&offset=2").unwrap(),
        Route::Control(ControlAction::SeekChecked {
            requested_seconds: 1.0,
            seconds: 1.0
        })
    );
    for target in [
        "/seek",
        "/seek?offset=NaN",
        "/seek?offset=Infinity",
        "/seek?offset=-1",
        "/volume?volume=101",
        "/mute?mute=true&mute=false",
    ] {
        assert!(parse_route(target).is_err(), "{target}");
    }
    for target in [
        "/status",
        "/lyric",
        "/lyric-all",
        "/play",
        "/pause",
        "/skip-next",
        "/skip-prev",
        "/seek?offset=0",
        "/collect",
        "/uncollect",
        "/volume?volume=50",
        "/mute?mute=true",
        "/subscribe-player-status",
    ] {
        assert_ne!(parse_route(target).unwrap(), Route::Unknown);
    }
    assert_eq!(
        parse_route("/prefix/play").unwrap(),
        Route::Control(ControlAction::Play)
    );
    assert_eq!(parse_route("/play/").unwrap(), Route::Unknown);
    let Route::Status(filter) = parse_route("/status?filter=volume&filter=mute").unwrap() else {
        panic!()
    };
    assert_eq!(filter.fields(), &DEFAULT_FILTER);
}
#[test]
fn real_http_all_routes_forward_exact_atomic_actions() {
    let _case = CASES.lock().unwrap();
    let backend = Arc::new(ProtocolBackend::default());
    backend.update(|status| {
        status.duration = 10.0;
        status.lyric = "line one\nline two".into();
    });
    let mut server = start(backend.clone(), ServerConfig::default());
    let address = server.view().address.unwrap();
    assert!(request(address, "/status", "").starts_with("HTTP/1.1 200"));
    assert_eq!(body(&request(address, "/lyric", "")), "line one\nline two");
    assert_eq!(
        body(&request(address, "/lyric-all", "")),
        "{\"lyric\":\"line one\\nline two\",\"tlyric\":\"\",\"rlyric\":\"\",\"lxlyric\":\"\"}"
    );
    for target in [
        "/play",
        "/pause",
        "/skip-next",
        "/skip-prev",
        "/seek?offset=1.23456",
        "/collect",
        "/uncollect",
        "/volume?volume=29",
        "/mute?mute=true",
    ] {
        assert_eq!(body(&request(address, target, "")), "OK");
    }
    assert_eq!(
        backend.state.lock().unwrap().actions,
        vec![
            ControlAction::Play,
            ControlAction::Pause,
            ControlAction::Next,
            ControlAction::Previous,
            ControlAction::SeekChecked {
                requested_seconds: 1.23456,
                seconds: 1.235
            },
            ControlAction::CollectCurrent { collected: true },
            ControlAction::CollectCurrent { collected: false },
            ControlAction::SetVolume { percent: 29 },
            ControlAction::SetMute { muted: true }
        ]
    );
    assert_eq!(
        body(&request(address, "/seek?offset=11", "")),
        "Invalid offset"
    );
    backend.update(|status| status.duration = 1.2344);
    assert_eq!(
        body(&request(address, "/seek?offset=1.23449", "")),
        "Invalid offset"
    );
    backend.update(|status| status.duration = 1.2348);
    assert_eq!(body(&request(address, "/seek?offset=1.2347", "")), "OK");
    assert_eq!(
        body(&request(address, "/approve-source?path=anything", "")),
        "Forbidden"
    );
    let mut stream = subscribe(address, "status", "");
    assert_eq!(frame(&mut stream), "event: status\ndata: \"stoped\"\n\n");
    drop(stream);
    stop(&mut server);
}
#[test]
fn real_sse_updates_cursor_recovery_and_disconnect_release() {
    let _case = CASES.lock().unwrap();
    let backend = Arc::new(ProtocolBackend::default());
    let mut server = start(backend.clone(), ServerConfig::default());
    let mut stream = subscribe(server.view().address.unwrap(), "name,progress", "");
    assert_eq!(frame(&mut stream), "event: name\ndata: \"\"\n\n");
    assert_eq!(frame(&mut stream), "event: progress\ndata: 0\n\n");
    backend.update(|status| {
        status.name = "song".into();
        status.progress = 1.25;
    });
    assert_eq!(frame(&mut stream), "event: name\ndata: \"song\"\n\n");
    assert_eq!(frame(&mut stream), "event: progress\ndata: 1.25\n\n");
    backend.expired.store(true, Ordering::Release);
    assert_eq!(frame(&mut stream), "event: name\ndata: \"song\"\n\n");
    assert_eq!(frame(&mut stream), "event: progress\ndata: 1.25\n\n");
    drop(stream);
    eventually(Duration::from_secs(2), || server.view().subscriptions == 0);
    stop(&mut server);
}
#[test]
fn auth_origin_host_and_lan_are_checked_before_any_effect() {
    let _case = CASES.lock().unwrap();
    let backend = Arc::new(ProtocolBackend::default());
    let (token, digest) = BearerDigest::generate().unwrap();
    assert!(!format!("{digest:?}").contains(&token));
    let mut config = ServerConfig {
        authentication: Authentication::Bearer(digest),
        allowed_origins: vec!["https://trusted.example".into()],
        ..ServerConfig::default()
    };
    let mut server = start(backend.clone(), config.clone());
    let address = server.view().address.unwrap();
    for extra in [
        "".to_string(),
        format!("Authorization: Bearer {token}\r\nOrigin: https://evil.example\r\n"),
        format!("Authorization: Bearer {token}\r\nAuthorization: Bearer {token}\r\n"),
    ] {
        let response = request(address, "/play", &extra);
        assert!(!response.starts_with("HTTP/1.1 200"));
    }
    assert!(request(address, &format!("/play?token={token}"), "").starts_with("HTTP/1.1 401"));
    let auth = format!("Authorization: Bearer {token}\r\nOrigin: https://trusted.example\r\n");
    let response = request(address, "/play", &auth);
    assert!(response.starts_with("HTTP/1.1 200"));
    assert!(response.contains("Access-Control-Allow-Origin: https://trusted.example\r\n"));
    assert!(!response.contains("Allow-Origin: *"));
    let mut socket = socket(address);
    write!(
        socket,
        "GET /pause HTTP/1.1\r\nHost: attacker.example:{}\r\nAuthorization: Bearer {token}\r\n\r\n",
        address.port()
    )
    .unwrap();
    let mut response = String::new();
    socket.read_to_string(&mut response).unwrap();
    assert!(response.starts_with("HTTP/1.1 401"));
    assert_eq!(
        backend.state.lock().unwrap().actions,
        vec![ControlAction::Play]
    );
    stop(&mut server);
    config.bind_lan = true;
    assert!(config.validate().is_err());
    config.allowed_lan_hosts.push("127.0.0.1".parse().unwrap());
    assert!(config.validate().is_ok());
    config.authentication = Authentication::LegacyLoopback;
    assert!(config.validate().is_err());
    let mut legacy = start(backend, ServerConfig::default());
    assert!(
        request(
            legacy.view().address.unwrap(),
            "/play",
            "Origin: https://evil.example\r\n"
        )
        .starts_with("HTTP/1.1 401")
    );
    stop(&mut legacy);
}

#[test]
fn cross_site_browser_gets_without_origin_do_not_apply_controls() {
    let _case = CASES.lock().unwrap();
    let backend = Arc::new(ProtocolBackend::default());
    let mut server = start(backend.clone(), ServerConfig::default());
    let address = server.view().address.unwrap();
    for extra in [
        "Sec-Fetch-Site: cross-site\r\nSec-Fetch-Mode: no-cors\r\nSec-Fetch-Dest: image\r\n",
        "Referer: https://evil.example/image\r\n",
        "Sec-Fetch-Mode: navigate\r\nSec-Fetch-Dest: document\r\n",
        "Sec-Fetch-Site: same-site\r\n",
        "User-Agent: Mozilla/5.0 AppleWebKit/605.1.15 Safari/605.1.15\r\n",
    ] {
        assert!(request(address, "/skip-next", extra).starts_with("HTTP/1.1 401"));
    }
    assert!(backend.state.lock().unwrap().actions.is_empty());
    assert_eq!(
        body(&request(address, "/play", "User-Agent: curl/8.0\r\n")),
        "OK"
    );
    assert_eq!(body(&request(address, "/play", "")), "OK");
    assert_eq!(
        body(&request(
            address,
            "/pause",
            &format!("Referer: http://{address}/status\r\nSec-Fetch-Site: same-origin\r\n")
        )),
        "OK"
    );
    stop(&mut server);
    let mut approved = start(
        backend.clone(),
        ServerConfig {
            allowed_origins: vec!["https://trusted.example".into()],
            ..Default::default()
        },
    );
    assert_eq!(
        body(&request(
            approved.view().address.unwrap(),
            "/play",
            "Referer: https://trusted.example/app\r\nSec-Fetch-Site: cross-site\r\n"
        )),
        "OK"
    );
    stop(&mut approved);
}
#[test]
fn malformed_http_size_bodies_and_unknown_routes_cannot_bypass_auth() {
    let _case = CASES.lock().unwrap();
    let backend = Arc::new(ProtocolBackend::default());
    let mut server = start(backend.clone(), ServerConfig::default());
    let address = server.view().address.unwrap();
    for extra in [
        "Transfer-Encoding: chunked\r\n",
        "Content-Length: 65537\r\n",
        "Content-Length: 0\r\nContent-Length: 0\r\n",
        " folded: invalid\r\n",
    ] {
        assert!(request(address, "/play", extra).starts_with("HTTP/1.1 400"));
    }
    let mut client = socket(address);
    write!(
        client,
        "POST /prefix/play HTTP/1.1\r\nHost: {address}\r\nContent-Length: 4\r\n\r\nBODY"
    )
    .unwrap();
    client.shutdown(Shutdown::Write).unwrap();
    let mut response = String::new();
    client.read_to_string(&mut response).unwrap();
    assert_eq!(body(&response), "OK");
    assert_eq!(
        backend.state.lock().unwrap().actions,
        vec![ControlAction::Play]
    );
    let mut client = socket(address);
    write!(
        client,
        "GET /status HTTP/1.1\r\nHost: {address}\r\nX-Large: {}\r\n\r\n",
        "a".repeat(17 * 1024)
    )
    .unwrap();
    let mut response = String::new();
    let _ = client.read_to_string(&mut response);
    assert!(response.starts_with("HTTP/1.1 431"));
    stop(&mut server);
}
#[test]
fn real_subscription_admission_is_bounded_and_stop_closes_every_socket() {
    let _case = CASES.lock().unwrap();
    let backend = Arc::new(ProtocolBackend::default());
    let mut server = start(backend, ServerConfig::default());
    let address = server.view().address.unwrap();
    let mut streams = Vec::new();
    for _ in 0..16 {
        let mut stream = subscribe(address, "status", "");
        frame(&mut stream);
        streams.push(stream);
    }
    eventually(Duration::from_secs(2), || server.view().subscriptions == 16);
    assert!(
        request(address, "/subscribe-player-status?filter=status", "").starts_with("HTTP/1.1 503")
    );
    let mut idle = Vec::new();
    for _ in 0..16 {
        idle.push(socket(address));
    }
    eventually(Duration::from_secs(2), || server.view().connections == 32);
    stop(&mut server);
    for mut stream in streams {
        let mut buffer = [0; 16];
        assert_eq!(stream.read(&mut buffer).unwrap(), 0);
    }
    for mut socket in idle {
        let mut buffer = [0; 16];
        assert_eq!(socket.read(&mut buffer).unwrap(), 0);
    }
}
#[test]
fn oversize_selected_lyric_is_rejected_before_sse_headers() {
    let _case = CASES.lock().unwrap();
    let backend = Arc::new(ProtocolBackend::default());
    backend.update(|status| status.lyric = "a".repeat(300 * 1024));
    let mut server = start(backend, ServerConfig::default());
    let address = server.view().address.unwrap();
    let response = request(address, "/subscribe-player-status?filter=lyric", "");
    assert!(response.starts_with("HTTP/1.1 413"));
    assert!(!response.contains("text/event-stream"));
    assert!(request(address, "/status?filter=volume", "").starts_with("HTTP/1.1 200"));
    stop(&mut server);
}

#[test]
fn actual_head_requests_apply_control_and_suppress_ordinary_bodies() {
    let _case = CASES.lock().unwrap();
    let backend = Arc::new(ProtocolBackend::default());
    backend.update(|status| status.lyric = "line one".into());
    let mut server = start(backend.clone(), ServerConfig::default());
    let address = server.view().address.unwrap();
    for target in ["/play", "/status", "/lyric", "/lyric-all"] {
        let mut client = socket(address);
        write!(client, "HEAD {target} HTTP/1.1\r\nHost: {address}\r\n\r\n").unwrap();
        let mut response = String::new();
        client.read_to_string(&mut response).unwrap();
        assert!(response.starts_with("HTTP/1.1 200"));
        assert!(body(&response).is_empty());
    }
    assert_eq!(
        backend.state.lock().unwrap().actions,
        vec![ControlAction::Play]
    );
    stop(&mut server);
}

#[test]
fn actual_slow_sse_peer_releases_its_socket_and_subscription() {
    let _case = CASES.lock().unwrap();
    let backend = Arc::new(ProtocolBackend::default());
    backend.update(|status| status.lyric = "a".repeat(200 * 1024));
    let mut server = start(backend.clone(), ServerConfig::default());
    let address = server.view().address.unwrap();
    let mut client = socket(address);
    small_receive_window(&client);
    write!(
        client,
        "GET /subscribe-player-status?filter=lyric HTTP/1.1\r\nHost: {address}\r\n\r\n"
    )
    .unwrap();
    eventually(Duration::from_secs(2), || server.view().subscriptions == 1);
    // Never read the real socket. Changing full scalar payloads fills the actual
    // TCP window and exercises owner queue/write deadlines, without fake I/O.
    let start = Instant::now();
    let mut changed = 0;
    while server.view().subscriptions != 0 && start.elapsed() < Duration::from_secs(7) {
        backend.update(|status| {
            status.lyric = if changed % 2 == 0 { "b" } else { "c" }.repeat(200 * 1024)
        });
        changed += 1;
        std::thread::sleep(Duration::from_millis(100));
    }
    assert_eq!(server.view().subscriptions, 0);
    assert_eq!(server.view().connections, 0);
    drop(client);
    stop(&mut server);
}
fn small_receive_window(socket: &TcpStream) {
    let bytes = 2048i32;
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawSocket;
        #[link(name = "ws2_32")]
        unsafe extern "system" {
            fn setsockopt(
                socket: usize,
                level: i32,
                name: i32,
                value: *const u8,
                length: i32,
            ) -> i32;
        }
        assert_eq!(
            unsafe {
                setsockopt(
                    socket.as_raw_socket() as usize,
                    0xffff,
                    0x1002,
                    (&bytes as *const i32).cast(),
                    4,
                )
            },
            0
        );
    }
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        assert_eq!(
            unsafe {
                libc::setsockopt(
                    socket.as_raw_fd(),
                    libc::SOL_SOCKET,
                    libc::SO_RCVBUF,
                    (&bytes as *const i32).cast(),
                    std::mem::size_of_val(&bytes) as libc::socklen_t,
                )
            },
            0
        );
    }
}
#[test]
fn actual_blocked_adapter_keeps_shutdown_pending_until_real_join() {
    let _case = CASES.lock().unwrap();
    let backend = Arc::new(ProtocolBackend::default());
    let gate = Gate::new();
    let _release = ReleaseGuard(gate.clone());
    *backend.gate.lock().unwrap() = Some(gate.clone());
    let mut server = start(backend, ServerConfig::default());
    let address = server.view().address.unwrap();
    let mut client = socket(address);
    write!(client, "GET /pause HTTP/1.1\r\nHost: {address}\r\n\r\n").unwrap();
    eventually(Duration::from_secs(2), || {
        gate.entered.load(Ordering::Acquire)
    });
    assert!(!server.shutdown(Duration::from_millis(20)).unwrap());
    assert!(!server.is_finished());
    assert_eq!(server.view().state, ServerState::Stopping);
    while let Some(event) = server.try_recv() {
        assert!(!matches!(event, ServerEvent::Stopped { .. }));
    }
    gate.release();
    assert!(server.shutdown(Duration::from_secs(5)).unwrap());
    assert!(server.is_finished());
    assert!(matches!(
        server.try_recv(),
        Some(ServerEvent::Stopped { session_epoch: 7 })
    ));
    let listener = TcpListener::bind(address).unwrap();
    drop(listener);
}

#[test]
fn actual_lan_bind_requires_bearer_and_token_rotation_closes_old_streams() {
    let _case = CASES.lock().unwrap();
    let backend = Arc::new(ProtocolBackend::default());
    let (token, digest) = BearerDigest::generate().unwrap();
    let config = ServerConfig {
        bind_lan: true,
        authentication: Authentication::Bearer(digest),
        allowed_lan_hosts: vec!["127.0.0.1".parse().unwrap()],
        ..ServerConfig::default()
    };
    let mut server = start(backend.clone(), config);
    let port = server.view().address.unwrap().port();
    let address = SocketAddr::from(([127, 0, 0, 1], port));
    assert!(server.view().address.unwrap().ip().is_unspecified());
    assert!(request(address, "/status", "").starts_with("HTTP/1.1 401"));
    let mut stream = subscribe(
        address,
        "status",
        &format!("Authorization: Bearer {token}\r\n"),
    );
    frame(&mut stream);
    assert!(server.shutdown(Duration::from_secs(5)).unwrap());
    let mut bytes = [0; 16];
    assert_eq!(stream.read(&mut bytes).unwrap(), 0);
    let (next_token, next_digest) = BearerDigest::generate().unwrap();
    let mut replacement = OpenApiServer::start(
        ServerConfig {
            port,
            session_epoch: 8,
            authentication: Authentication::Bearer(next_digest),
            ..ServerConfig::default()
        },
        backend,
    )
    .unwrap();
    eventually(Duration::from_secs(2), || {
        replacement.view().state == ServerState::Listening
    });
    assert!(
        request(
            address,
            "/status",
            &format!("Authorization: Bearer {token}\r\n")
        )
        .starts_with("HTTP/1.1 401")
    );
    assert!(
        request(
            address,
            "/status",
            &format!("Authorization: Bearer {next_token}\r\n")
        )
        .starts_with("HTTP/1.1 200")
    );
    stop(&mut replacement);
}
#[test]
fn pure_deeplink_all_player_actions_and_remote_metadata_are_bounded() {
    for (action, expected) in [
        ("play", ControlAction::Play),
        ("pause", ControlAction::Pause),
        ("skipNext", ControlAction::Next),
        ("skipPrev", ControlAction::Previous),
        ("togglePlay", ControlAction::Toggle),
        ("collect", ControlAction::CollectCurrent { collected: true }),
        (
            "uncollect",
            ControlAction::CollectCurrent { collected: false },
        ),
        ("dislike", ControlAction::DislikeCurrent),
    ] {
        assert_eq!(
            parse_deeplink(&format!("lxmusic://player/{action}"))
                .unwrap()
                .action,
            DeepLinkAction::Player(expected)
        );
    }
    assert_eq!(
        parse_deeplink("lxmusic://music/search/kw/a+b")
            .unwrap()
            .action,
        DeepLinkAction::Search {
            keywords: "a+b".into(),
            source: Some("kw".into())
        }
    );
    assert_eq!(
        parse_deeplink("lxmusic://music/searchPlay/%20Song%20-%20Singer%20-%20Ignored")
            .unwrap()
            .action,
        DeepLinkAction::SearchPlay {
            name: "Song".into(),
            singer: "Singer".into(),
            album_name: String::new(),
            interval: None,
            play_later: false
        }
    );
    let music = serde_json::json!({"name":"A&amp;B","singer":"Singer","source":"kw","songmid":123,"types":[{"type":"bogus"},{"type":"128k","size":"1 MB"}],"localPath":"C:/private","approvedUrl":"https://untrusted.example"});
    let link = format!("lxmusic://music/play?data={}", percent(&music.to_string()));
    let DeepLinkAction::MusicPlay { music } = parse_deeplink(&link).unwrap().action else {
        panic!()
    };
    assert_eq!(music.wire["name"], "A&B");
    assert_eq!(music.wire["types"].as_array().unwrap().len(), 1);
    assert!(music.wire.get("localPath").is_none());
    assert!(music.wire.get("approvedUrl").is_none());
    let link = format!(
        "lxmusic://songlist/play?data={}",
        percent(r#"{"source":"wy","id":123,"index":2}"#)
    );
    assert!(matches!(
        parse_deeplink(&link).unwrap().action,
        DeepLinkAction::SonglistPlay { index: Some(2), .. }
    ));
    for link in [
        "file:///private.wav",
        "lxmusic://player/approve-source",
        "lxmusic://music/play?data=%7B%22source%22%3A%22local%22%7D",
        "lxmusic://songlist/play/wy/file%3A%2F%2F%2Fprivate",
        "lxmusic://player/play#fragment",
        "lxmusic://user@player/play",
        "lxmusic://music/search/%00",
        "lxmusic://music/search/%FF",
    ] {
        assert!(parse_deeplink(link).is_err(), "{link}");
    }
    assert!(parse_deeplink(&format!("lxmusic://music/search/{}", "x".repeat(65536))).is_err());
}
fn percent(value: &str) -> String {
    value.bytes().map(|value| format!("%{value:02X}")).collect()
}

fn parsed_music_name(name: &str) -> String {
    let music = serde_json::json!({"name":name,"singer":"Singer","source":"kw","songmid":123,"types":[{"type":"128k"}]});
    let link = format!("lxmusic://music/play?data={}", percent(&music.to_string()));
    let DeepLinkAction::MusicPlay { music } = parse_deeplink(&link).unwrap().action else {
        panic!()
    };
    music.wire["name"].as_str().unwrap().into()
}
#[test]
fn html5_body_text_handles_named_entities_and_markup_goldens() {
    assert_eq!(
        parsed_music_name(
            "<b>A&amp;B</b> &copy; &AElig; &NotEqualTilde; &CounterClockwiseContourIntegral;"
        ),
        "A&B © Æ ≂\u{338} ∳"
    );
    assert_eq!(parsed_music_name("<b>A<i>B</b>C</i>D"), "ABCD");
    assert_eq!(
        parsed_music_name("before<table><tr><td>x</td></tr></table>after"),
        "beforexafter"
    );
    assert_eq!(parsed_music_name("&#x80;&#0;&nbsp;"), "€\u{fffd}\u{a0}");
}
#[test]
fn html5_parser_is_inert_and_projects_only_actual_body_text() {
    assert_eq!(
        parsed_music_name(
            "<head><title>Hidden</title></head><body>A<template>Hidden</template><script>notExecuted()</script>B</body>"
        ),
        "AnotExecuted()B"
    );
    assert_eq!(
        parsed_music_name("<img src=\"file:///private\" onerror=\"throw 1\">Title"),
        "Title"
    );
    assert_eq!(parsed_music_name("<svg><text>music</text></svg>"), "music");
    assert_eq!(parsed_music_name("<noscript>Title</noscript>"), "Title");
    assert_eq!(
        parsed_music_name("<frameset><noframes>Title</noframes></frameset>"),
        "Title"
    );
    assert_eq!(
        parsed_music_name(
            "<body></body><frameset><noframes>Only connected frame</noframes></frameset>"
        ),
        "Only connected frame"
    );
}
#[test]
fn numeric_deeplink_identities_use_javascript_string_conversion() {
    for (json, expected) in [
        ("1e3", "1000"),
        ("1e20", "100000000000000000000"),
        ("1e21", "1e+21"),
        ("0.000001", "0.000001"),
        ("1e-7", "1e-7"),
        ("-0", "0"),
        ("9007199254740993", "9007199254740992"),
    ] {
        let link = format!(
            "lxmusic://music/search?data={}",
            percent(&format!("{{\"keywords\":{json}}}"))
        );
        assert_eq!(
            parse_deeplink(&link).unwrap().action,
            DeepLinkAction::Search {
                keywords: expected.into(),
                source: None
            }
        );
        let link = format!(
            "lxmusic://songlist/open?data={}",
            percent(&format!("{{\"source\":\"wy\",\"id\":{json}}}"))
        );
        if json == "-0" {
            assert!(parse_deeplink(&link).is_err());
        } else {
            assert_eq!(
                parse_deeplink(&link).unwrap().action,
                DeepLinkAction::SonglistOpen {
                    source: "wy".into(),
                    id: Some(expected.into()),
                    url: None
                }
            );
        }
    }
}

#[test]
fn numeric_status_wire_uses_ecmascript_decimal_and_exponent_rules() {
    let status = LxPlayerStatus {
        progress: 0.000001,
        duration: 1e21,
        playback_rate: 1e-7,
        ..Default::default()
    };
    let filter = parse_filter(Some("progress,duration,playbackRate"));
    assert_eq!(
        String::from_utf8(filter.project(&status).unwrap()).unwrap(),
        "{\"progress\":0.000001,\"duration\":1e+21,\"playbackRate\":1e-7}"
    );
    assert_eq!(
        filter.frames(&status, None).unwrap()[0].1,
        b"event: progress\ndata: 0.000001\n\n"
    );
}

fn verified_protocol_artwork(
    backend: &ProtocolBackend,
    large: bool,
) -> (ArtworkReference, ArtworkResponse) {
    use image::{ExtendedColorType, ImageEncoder};
    use sha2::{Digest, Sha256};
    let dimension = if large { 1024 } else { 2 };
    let mut pixels = vec![0; dimension as usize * dimension as usize * 4];
    let mut random = 0x63d9_1823u32;
    for pixel in &mut pixels {
        random ^= random << 13;
        random ^= random >> 17;
        random ^= random << 5;
        *pixel = random as u8;
    }
    let mut bytes = Vec::new();
    image::codecs::png::PngEncoder::new(&mut bytes)
        .write_image(&pixels, dimension, dimension, ExtendedColorType::Rgba8)
        .unwrap();
    let verified = lx_gpui_player::library::artwork_validator::validate_artwork(
        bytes,
        Some("image/png"),
        || Ok(()),
    )
    .unwrap();
    let reference = ArtworkReference {
        session_epoch: 7,
        generation: 11,
        key_sha256: format!("{:x}", Sha256::digest(b"protocol-current-artwork-key")),
        sha256: verified.metadata().sha256.clone(),
    };
    let response = ArtworkResponse {
        mime_type: "image/png",
        bytes: verified.encoded_arc(),
        retired: Arc::new(AtomicBool::new(false)),
    };
    *backend.artwork.lock().unwrap() = Some((reference.clone(), response.clone()));
    backend.update(|status| status.pic_url = reference.path().unwrap());
    (reference, response)
}
fn image_request(address: SocketAddr, method: &str, path: &str, extra: &str) -> (String, Vec<u8>) {
    let mut socket = socket(address);
    write!(
        socket,
        "{method} {path} HTTP/1.1\r\nHost: {address}\r\n{extra}\r\n"
    )
    .unwrap();
    let mut response = Vec::new();
    socket.read_to_end(&mut response).unwrap();
    let end = response
        .windows(4)
        .position(|bytes| bytes == b"\r\n\r\n")
        .unwrap()
        + 4;
    (
        String::from_utf8(response[..end].to_vec()).unwrap(),
        response[end..].to_vec(),
    )
}

#[test]
fn artwork_references_are_canonical_and_cannot_alias_control_routes_or_credentials() {
    let backend = ProtocolBackend::default();
    let (reference, _) = verified_protocol_artwork(&backend, false);
    let path = reference.path().unwrap();
    assert_eq!(parse_route(&path).unwrap(), Route::Artwork(reference));
    for invalid in [
        format!("{path}?token=private"),
        format!("{path}/status"),
        path.replacen("/v1/7/", "/v1/07/", 1),
        path.replacen("/v1/", "/v2/", 1),
        path.replacen("/v1/7/", "/v1/0/", 1),
        path.replacen("/v1/7/", "/v1/%37/", 1),
    ] {
        assert!(parse_route(&invalid).is_err(), "{invalid}");
    }
    assert!(path.starts_with('/'));
    assert!(!path.contains("token"));
}

#[test]
fn real_http_artwork_and_sse_share_authentication_and_return_exact_verified_bytes() {
    let _case = CASES.lock().unwrap();
    let backend = Arc::new(ProtocolBackend::default());
    let (reference, response) = verified_protocol_artwork(&backend, false);
    let (token, digest) = BearerDigest::generate().unwrap();
    let mut server = start(
        backend.clone(),
        ServerConfig {
            authentication: Authentication::Bearer(digest),
            allowed_origins: vec!["https://trusted.example".into()],
            ..Default::default()
        },
    );
    let address = server.view().address.unwrap();
    let path = reference.path().unwrap();
    let auth = format!("Authorization: Bearer {token}\r\n");
    for extra in [
        String::new(),
        format!("{auth}Origin: https://evil.example\r\n"),
        format!("{auth}Sec-Fetch-Site: cross-site\r\nSec-Fetch-Dest: image\r\n"),
    ] {
        assert!(
            image_request(address, "GET", &path, &extra)
                .0
                .starts_with("HTTP/1.1 401")
        );
    }
    assert_eq!(backend.artwork_calls.load(Ordering::Acquire), 0);
    let status = request(address, "/status?filter=picUrl", &auth);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(body(&status)).unwrap()["picUrl"],
        path
    );
    assert!(!status.contains(&token));
    let mut events = subscribe(address, "picUrl", &auth);
    assert_eq!(
        frame(&mut events),
        format!(
            "event: picUrl\ndata: {}\n\n",
            serde_json::to_string(&path).unwrap()
        )
    );
    let (header, bytes) = image_request(address, "GET", &path, &auth);
    assert!(header.starts_with("HTTP/1.1 200"));
    assert!(header.contains("Content-Type: image/png\r\n"));
    assert!(header.contains("Cache-Control: no-store\r\n"));
    assert!(header.contains("X-Content-Type-Options: nosniff\r\n"));
    assert_eq!(bytes.as_slice(), response.bytes.as_ref());
    let (head, bytes) = image_request(address, "HEAD", &path, &auth);
    assert!(head.contains(&format!("Content-Length: {}\r\n", response.bytes.len())));
    assert!(bytes.is_empty());
    assert!(
        image_request(address, "POST", &path, &auth)
            .0
            .starts_with("HTTP/1.1 400")
    );
    let mut stale = reference.clone();
    stale.session_epoch = 6;
    assert!(
        image_request(address, "GET", &stale.path().unwrap(), &auth)
            .0
            .starts_with("HTTP/1.1 401")
    );
    let calls = backend.artwork_calls.load(Ordering::Acquire);
    stale = reference;
    stale.generation += 1;
    assert!(
        image_request(address, "GET", &stale.path().unwrap(), &auth)
            .0
            .starts_with("HTTP/1.1 503")
    );
    assert_eq!(backend.artwork_calls.load(Ordering::Acquire), calls + 1);
    response.retired.store(true, Ordering::Release);
    backend.update(|status| status.pic_url.clear());
    assert_eq!(frame(&mut events), "event: picUrl\ndata: \"\"\n\n");
    assert!(
        image_request(address, "GET", &path, &auth)
            .0
            .starts_with("HTTP/1.1 503")
    );
    drop(events);
    stop(&mut server);
}

#[test]
fn large_verified_artwork_streams_beyond_status_budget_with_two_actual_response_slots() {
    let _case = CASES.lock().unwrap();
    let backend = Arc::new(ProtocolBackend::default());
    let (reference, response) = verified_protocol_artwork(&backend, true);
    assert!(response.bytes.len() > 256 * 1024);
    let mut server = start(backend.clone(), ServerConfig::default());
    let address = server.view().address.unwrap();
    let path = reference.path().unwrap();
    let first = socket(address);
    let second = socket(address);
    small_receive_window(&first);
    small_receive_window(&second);
    for mut stream in [&first, &second] {
        write!(stream, "GET {path} HTTP/1.1\r\nHost: {address}\r\n\r\n").unwrap();
    }
    eventually(Duration::from_secs(2), || {
        backend.artwork_calls.load(Ordering::Acquire) == 2
    });
    assert!(request(address, "/status", "").starts_with("HTTP/1.1 200"));
    let denied = image_request(address, "GET", &path, "");
    assert!(denied.0.starts_with("HTTP/1.1 503"), "{}", denied.0);
    assert_eq!(backend.artwork_calls.load(Ordering::Acquire), 2);
    drop(first);
    drop(second);
    eventually(Duration::from_secs(2), || server.view().connections == 0);
    let (header, bytes) = image_request(address, "GET", &path, "");
    assert!(header.starts_with("HTTP/1.1 200"));
    assert_eq!(bytes.as_slice(), response.bytes.as_ref());
    stop(&mut server);
}

#[test]
fn retiring_artwork_stops_a_real_partial_socket_write_and_releases_capacity() {
    let _case = CASES.lock().unwrap();
    let backend = Arc::new(ProtocolBackend::default());
    let (reference, response) = verified_protocol_artwork(&backend, true);
    let mut server = start(backend.clone(), ServerConfig::default());
    let address = server.view().address.unwrap();
    let mut slow = socket(address);
    write!(
        slow,
        "GET {} HTTP/1.1\r\nHost: {address}\r\n\r\n",
        reference.path().unwrap()
    )
    .unwrap();
    eventually(Duration::from_secs(2), || {
        backend.artwork_calls.load(Ordering::Acquire) == 1
    });
    let mut reader = BufReader::new(slow);
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    assert!(line.starts_with("HTTP/1.1 200"));
    loop {
        line.clear();
        reader.read_line(&mut line).unwrap();
        if line == "\r\n" {
            break;
        }
    }
    let mut actual_first_byte = [0u8; 1];
    reader.read_exact(&mut actual_first_byte).unwrap();
    assert_eq!(actual_first_byte[0], response.bytes[0]);
    response.retired.store(true, Ordering::Release);
    let mut partial = Vec::new();
    reader.read_to_end(&mut partial).unwrap();
    assert!(1 + partial.len() < response.bytes.len());
    eventually(Duration::from_secs(2), || server.view().connections == 0);
    let (replacement, replacement_bytes) = verified_protocol_artwork(&backend, false);
    let (header, bytes) = image_request(address, "GET", &replacement.path().unwrap(), "");
    assert!(header.starts_with("HTTP/1.1 200"));
    assert_eq!(bytes.as_slice(), replacement_bytes.bytes.as_ref());
    stop(&mut server);
}

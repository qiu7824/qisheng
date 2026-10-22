use lx_gpui_player::{
    backend::{
        BackendCommand, BackendConfig, BackendEvent, BackendHandle, BackendQuery,
        BackendQueryResult, CommandEnvelope, ErrorKind, ProviderOperationResult,
        ProviderOperationState, ReceiptState,
    },
    platforms::{
        HttpRequest, HttpResponse, HttpTransport, Platform, ProviderContext, ProviderError,
        ProviderRegistry, ProviderRequest, ProviderResponse, ReqwestTransport, TransportConfig,
    },
};
use serde_json::{Value, json};
use std::{
    fs,
    io::{BufRead, BufReader, Read, Write},
    net::TcpListener,
    process::{Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};
use tempfile::TempDir;

struct HttpFixture {
    base_url: String,
    requests: Arc<Mutex<Vec<String>>>,
    stopping: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}
impl HttpFixture {
    fn new(replies: Vec<(Value, Duration)>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let received = requests.clone();
        let stopping = Arc::new(AtomicBool::new(false));
        let stop = stopping.clone();
        let thread = thread::spawn(move || {
            for (body, delay) in replies {
                let (mut stream, _) = loop {
                    match listener.accept() {
                        Ok(stream) => break stream,
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            if stop.load(Ordering::Acquire) {
                                return;
                            }
                            thread::sleep(Duration::from_millis(2));
                        }
                        Err(error) => panic!("fixture accept failed: {error}"),
                    }
                };
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                let mut request = Vec::new();
                let mut buffer = [0u8; 4096];
                loop {
                    let count = stream.read(&mut buffer).unwrap();
                    if count == 0 {
                        break;
                    }
                    request.extend_from_slice(&buffer[..count]);
                    if request.windows(4).any(|v| v == b"\r\n\r\n") {
                        break;
                    }
                }
                received
                    .lock()
                    .unwrap()
                    .push(String::from_utf8_lossy(&request).into_owned());
                thread::sleep(delay);
                let body = serde_json::to_vec(&body).unwrap();
                let header = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                if stream.write_all(header.as_bytes()).is_ok() {
                    let _ = stream.write_all(&body);
                }
            }
        });
        Self {
            base_url,
            requests,
            stopping,
            thread: Some(thread),
        }
    }
}
impl Drop for HttpFixture {
    fn drop(&mut self) {
        self.stopping.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            thread.join().unwrap();
        }
    }
}

struct LoopbackTransport {
    base_url: String,
    network: ReqwestTransport,
}
impl HttpTransport for LoopbackTransport {
    fn execute(
        &self,
        mut request: HttpRequest,
        context: &ProviderContext,
    ) -> Result<HttpResponse, ProviderError> {
        let url = reqwest::Url::parse(&request.url).unwrap();
        request.url = format!(
            "{}{}{}",
            self.base_url,
            url.path(),
            url.query().map(|q| format!("?{q}")).unwrap_or_default()
        );
        self.network.execute(request, context)
    }
}
struct RunningBackend {
    handle: BackendHandle,
    _directory: TempDir,
}
impl RunningBackend {
    fn new(fixture: &HttpFixture, timeout: Duration) -> Self {
        let base = std::env::temp_dir().join("backend-platform-fixtures");
        fs::create_dir_all(&base).unwrap();
        let directory = tempfile::Builder::new()
            .prefix("profile-")
            .tempdir_in(base)
            .unwrap();
        let transport = Arc::new(LoopbackTransport {
            base_url: fixture.base_url.clone(),
            network: ReqwestTransport::new(TransportConfig {
                proxy: None,
                max_response_bytes: 8 * 1024 * 1024,
                retries: 0,
            })
            .unwrap(),
        });
        let mut config = BackendConfig::headless(directory.path().into());
        config.provider_registry = Some(Arc::new(ProviderRegistry::new(transport)));
        config.provider_timeout = timeout;
        let handle = BackendHandle::start(config).unwrap();
        Self {
            handle,
            _directory: directory,
        }
    }
}
impl Drop for RunningBackend {
    fn drop(&mut self) {
        self.handle.shutdown(Duration::from_secs(5)).unwrap();
    }
}
fn envelope(id: &str, command: BackendCommand) -> CommandEnvelope {
    CommandEnvelope {
        request_id: id.into(),
        origin: "provider-fixture".into(),
        expected_revision: None,
        command,
    }
}
fn search() -> ProviderRequest {
    ProviderRequest::Search {
        keyword: "Fixture +".into(),
        page: 1,
        limit: 30,
    }
}
fn response(opaque: &str) -> Value {
    json!({"TOTAL":"1","SHOW":"1","abslist":[{"MUSICRID":"MUSIC_7","SONGNAME":"Fixture Song","ARTIST":"A&B","ALBUM":"Fixture Album","ALBUMID":"8","DURATION":"185","N_MINFO":"level:standard,bitrate:128,format:mp3,size:3m;level:high,bitrate:320,format:mp3,size:8m","opaque":opaque}]})
}
fn wait_result(handle: &BackendHandle, id: &str) -> Arc<ProviderOperationResult> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let result = handle.provider_result(id).unwrap();
        if result.state != ProviderOperationState::Running {
            return result;
        }
        assert!(
            Instant::now() < deadline,
            "provider operation did not terminate"
        );
        thread::sleep(Duration::from_millis(5));
    }
}
fn wait_request(fixture: &HttpFixture, count: usize) {
    let deadline = Instant::now() + Duration::from_secs(3);
    while fixture.requests.lock().unwrap().len() < count {
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn actual_kw_http_provider_actor_and_typed_query_preserve_track_without_import_or_revision() {
    let fixture = HttpFixture::new(vec![(response("retained"), Duration::from_millis(150))]);
    let backend = RunningBackend::new(&fixture, Duration::from_secs(2));
    let before = backend.handle.snapshot();
    let cursor = backend.handle.event_cursor();
    let receipt = backend
        .handle
        .execute(envelope(
            "search",
            BackendCommand::ProviderRequest {
                platform: Platform::Kw,
                request: Box::new(search()),
            },
        ))
        .unwrap();
    assert_eq!(receipt.state, ReceiptState::Accepted);
    let running = backend
        .handle
        .provider_result(&receipt.operation_id)
        .unwrap();
    assert_eq!(running.state, ProviderOperationState::Running);
    assert_eq!(running.response, None);
    let result = wait_result(&backend.handle, &receipt.operation_id);
    assert_eq!(result.state, ProviderOperationState::Completed);
    assert_eq!(result.error, None);
    let ProviderResponse::Tracks(page) = result.response.as_deref().unwrap() else {
        panic!()
    };
    assert_eq!(page.items[0].title, "Fixture Song");
    assert_eq!(page.items[0].platform.source, "kw");
    assert_eq!(page.items[0].platform.song_id, "7");
    assert_eq!(page.items[0].metadata["kwRaw"]["opaque"], "retained");
    assert_eq!(page.items[0].duration_ms, Some(185000));
    let queried = backend
        .handle
        .provider_result(&receipt.operation_id)
        .unwrap();
    assert!(Arc::ptr_eq(&result, &queried));
    let after = backend.handle.snapshot();
    assert_eq!(after.revision, before.revision);
    assert_eq!(after.tracks.len(), before.tracks.len());
    assert_eq!(after.sources.len(), before.sources.len());
    assert_eq!(after.playback.generation, before.playback.generation);
    assert!(!after.busy);
    assert!(
        !serde_json::to_string(&after)
            .unwrap()
            .contains("Fixture Song")
    );
    let events = backend.handle.events_after(cursor).unwrap();
    assert!(events.iter().any(|event|matches!(&event.payload,BackendEvent::ProviderFinished{operation_id,state:ProviderOperationState::Completed,..} if operation_id=="search")));
    assert!(events.iter().any(|event|matches!(&event.payload,BackendEvent::OperationFinished{operation_id,success:true,..} if operation_id=="search")));
    let request = fixture.requests.lock().unwrap()[0].clone();
    assert!(
        !serde_json::to_string(&events)
            .unwrap()
            .contains("Fixture Song")
    );
    assert!(request.starts_with("GET /r.s?client=kt&all=Fixture%20%2B&pn=0&rn=30"));
    assert!(!request.to_lowercase().contains("cookie:"));
}

#[test]
fn same_json_connection_can_cancel_and_late_provider_completion_cannot_replace_result() {
    let fixture = HttpFixture::new(vec![(response("late"), Duration::from_millis(300))]);
    let backend = RunningBackend::new(&fixture, Duration::from_secs(2));
    let request = json!({"request_id":"slow","origin":"jsonl-fixture","command":"provider_request","platform":"kw","request":{"operation":"search","keyword":"x","page":1,"limit":30}});
    let receipt = backend
        .handle
        .execute(serde_json::from_value(request).unwrap())
        .unwrap();
    assert_eq!(receipt.state, ReceiptState::Accepted);
    wait_request(&fixture, 1);
    let started = Instant::now();
    backend.handle.execute(serde_json::from_value(json!({"request_id":"cancel","origin":"jsonl-fixture","command":"cancel_operation","operation_id":"slow"})).unwrap()).unwrap();
    assert!(started.elapsed() < Duration::from_millis(200));
    let cancelled = wait_result(&backend.handle, "slow");
    assert_eq!(cancelled.state, ProviderOperationState::Failed);
    assert_eq!(cancelled.error, Some(ProviderError::Cancelled));
    assert_eq!(cancelled.response, None);
    thread::sleep(Duration::from_millis(350));
    let after = backend.handle.provider_result("slow").unwrap();
    assert!(Arc::ptr_eq(&cancelled, &after));
    let completions=backend.handle.events_after(0).unwrap().into_iter().filter(|e|matches!(&e.payload,BackendEvent::ProviderFinished{operation_id,..} if operation_id=="slow")).count();
    assert_eq!(completions, 1);
}

#[test]
fn provider_deadline_and_unsupported_capability_publish_typed_failures() {
    let fixture = HttpFixture::new(vec![(response("late"), Duration::from_millis(200))]);
    let backend = RunningBackend::new(&fixture, Duration::from_millis(40));
    backend
        .handle
        .execute(envelope(
            "deadline",
            BackendCommand::ProviderRequest {
                platform: Platform::Kw,
                request: Box::new(search()),
            },
        ))
        .unwrap();
    let result = wait_result(&backend.handle, "deadline");
    assert_eq!(result.state, ProviderOperationState::Failed);
    assert_eq!(result.error, Some(ProviderError::DeadlineExceeded));
    assert_eq!(result.response, None);
    backend
        .handle
        .execute(envelope(
            "unsupported",
            BackendCommand::ProviderRequest {
                platform: Platform::Kw,
                request: Box::new(ProviderRequest::Comments {
                    id: "7".into(),
                    kind: lx_gpui_player::platforms::CommentKind::Replies,
                    page: 1,
                    limit: 20,
                    cursor: None,
                    root_comment_id: Some("1".into()),
                }),
            },
        ))
        .unwrap();
    let result = wait_result(&backend.handle, "unsupported");
    assert!(matches!(
        result.error,
        Some(ProviderError::Unsupported { .. })
    ));
    assert_eq!(fixture.requests.lock().unwrap().len(), 1);
    let events = backend.handle.events_after(0).unwrap();
    assert!(events.iter().any(|event|matches!(&event.payload,BackendEvent::OperationFinished{operation_id,error:Some(error),..} if operation_id=="deadline" && error.kind==ErrorKind::Timeout)));
}

#[test]
fn idempotent_replays_do_not_refetch_and_record_capacity_evicts_old_payloads() {
    let fixture = HttpFixture::new(vec![(response("once"), Duration::ZERO)]);
    let backend = RunningBackend::new(&fixture, Duration::from_secs(2));
    let command = envelope(
        "same",
        BackendCommand::ProviderRequest {
            platform: Platform::Kw,
            request: Box::new(search()),
        },
    );
    backend.handle.execute(command.clone()).unwrap();
    wait_result(&backend.handle, "same");
    assert_eq!(
        backend.handle.execute(command.clone()).unwrap().state,
        ReceiptState::Completed
    );
    assert_eq!(fixture.requests.lock().unwrap().len(), 1);
    let changed = envelope(
        "same",
        BackendCommand::ProviderRequest {
            platform: Platform::Kw,
            request: Box::new(ProviderRequest::Charts),
        },
    );
    assert_eq!(
        backend.handle.execute(changed).unwrap_err().kind,
        ErrorKind::Conflict
    );
    for index in 0..32 {
        let id = format!("charts-{index}");
        backend
            .handle
            .execute(envelope(
                &id,
                BackendCommand::ProviderRequest {
                    platform: Platform::Kw,
                    request: Box::new(ProviderRequest::Charts),
                },
            ))
            .unwrap();
        assert_eq!(
            wait_result(&backend.handle, &id).state,
            ProviderOperationState::Completed
        );
    }
    assert_eq!(
        backend.handle.provider_result("same").unwrap_err().kind,
        ErrorKind::Unavailable
    );
    assert!(backend.handle.provider_result("charts-0").is_ok());
    assert!(backend.handle.provider_result("charts-31").is_ok());
    assert_eq!(
        backend.handle.execute(command).unwrap().state,
        ReceiptState::Completed
    );
    assert_eq!(fixture.requests.lock().unwrap().len(), 1);
}

#[test]
fn aggregate_json_capacity_evicts_before_thirty_two_records() {
    let body = response(&"x".repeat(2 * 1024 * 1024));
    let fixture = HttpFixture::new((0..18).map(|_| (body.clone(), Duration::ZERO)).collect());
    let backend = RunningBackend::new(&fixture, Duration::from_secs(5));
    for index in 0..18 {
        let id = format!("large-{index}");
        backend
            .handle
            .execute(envelope(
                &id,
                BackendCommand::ProviderRequest {
                    platform: Platform::Kw,
                    request: Box::new(search()),
                },
            ))
            .unwrap();
        assert_eq!(
            wait_result(&backend.handle, &id).state,
            ProviderOperationState::Completed
        );
    }
    assert_eq!(
        backend.handle.provider_result("large-0").unwrap_err().kind,
        ErrorKind::Unavailable
    );
    assert!(backend.handle.provider_result("large-17").is_ok());
    assert_eq!(fixture.requests.lock().unwrap().len(), 18);
}

#[test]
fn provider_work_is_independent_of_source_playback_commands_and_shutdown_releases_profile() {
    let fixture = HttpFixture::new(vec![(response("shutdown"), Duration::from_millis(300))]);
    let backend = RunningBackend::new(&fixture, Duration::from_secs(2));
    let before = backend.handle.snapshot();
    let reader = rusqlite::Connection::open_with_flags(
        backend
            ._directory
            .path()
            .join(lx_gpui_player::repository::DATABASE_NAME),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    backend
        .handle
        .execute(envelope(
            "slow-close",
            BackendCommand::ProviderRequest {
                platform: Platform::Kw,
                request: Box::new(search()),
            },
        ))
        .unwrap();
    wait_request(&fixture, 1);
    let started = Instant::now();
    assert_eq!(backend.handle.snapshot().revision, before.revision);
    let new_changes: i64 = reader
        .query_row(
            "SELECT COUNT(*) FROM change_outbox WHERE revision>?1",
            [before.revision as i64],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        new_changes, 0,
        "Metadata provider execution must not change repository/outbox"
    );
    backend
        .handle
        .execute(envelope("pause", BackendCommand::Pause))
        .unwrap();
    assert!(started.elapsed() < Duration::from_millis(150));
    assert_eq!(
        backend.handle.provider_result("slow-close").unwrap().state,
        ProviderOperationState::Running
    );
    let BackendQueryResult::PlaybackCheckpoint(Some(checkpoint)) = backend
        .handle
        .query(BackendQuery::PlaybackCheckpoint)
        .unwrap()
    else {
        panic!("Pause must persist a playback checkpoint")
    };
    assert_eq!(checkpoint.queue.origin_list, "catalog");
    assert!(checkpoint.queue.current.is_none());
    let started = Instant::now();
    backend.handle.shutdown(Duration::from_secs(2)).unwrap();
    assert!(started.elapsed() < Duration::from_millis(500));
    assert!(backend.handle.snapshot().stopped);
    let after = backend.handle.snapshot().revision;
    assert!(after > before.revision);
    let changes = reader
        .prepare("SELECT document FROM change_outbox WHERE revision>?1 ORDER BY revision")
        .unwrap()
        .query_map([before.revision as i64], |row| row.get::<_, String>(0))
        .unwrap()
        .map(|row| {
            serde_json::from_str::<lx_gpui_player::repository::RepositoryChange>(&row.unwrap())
                .unwrap()
        })
        .collect::<Vec<_>>();
    assert!(changes.len() >= 2);
    assert!(
        changes.iter().all(|change| change.kind
            == lx_gpui_player::repository::ChangeKind::PlaybackCheckpointChanged)
    );
    assert_eq!(changes.last().unwrap().revision, after);
    drop(reader);
    assert_eq!(
        backend
            .handle
            .provider_result("slow-close")
            .unwrap_err()
            .kind,
        ErrorKind::Closed
    );
    let events = backend.handle.events_after(0).unwrap();
    assert!(events.iter().any(|event|matches!(&event.payload,BackendEvent::ProviderFinished{operation_id,error:Some(ProviderError::Cancelled),..} if operation_id=="slow-close")));
    let reopened =
        BackendHandle::start(BackendConfig::headless(backend._directory.path().into())).unwrap();
    let BackendQueryResult::PlaybackCheckpoint(Some(checkpoint)) =
        reopened.query(BackendQuery::PlaybackCheckpoint).unwrap()
    else {
        panic!("Shutdown must retain the durable checkpoint")
    };
    assert_eq!(checkpoint.queue.origin_list, "catalog");
    assert!(checkpoint.queue.current.is_none());
    reopened.shutdown(Duration::from_secs(3)).unwrap();
}

#[test]
fn cli_jsonl_same_stdin_accepts_provider_then_cancels_and_queries_typed_result() {
    let fixture = HttpFixture::new(vec![(response("late-jsonl"), Duration::from_millis(300))]);
    let base = std::env::temp_dir().join("backend-platform-fixtures");
    fs::create_dir_all(&base).unwrap();
    let directory = tempfile::Builder::new()
        .prefix("jsonl-profile-")
        .tempdir_in(base)
        .unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_lx-native-cli"))
        .arg("--data-dir")
        .arg(directory.path())
        .arg("--provider-proxy")
        .arg(&fixture.base_url)
        .arg("--json-lines")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    let mut output = BufReader::new(child.stdout.take().unwrap());
    let mut exchange = |value: Value| {
        writeln!(input, "{value}").unwrap();
        input.flush().unwrap();
        let mut line = String::new();
        output.read_line(&mut line).unwrap();
        serde_json::from_str::<Value>(&line).unwrap()
    };
    let accepted = exchange(
        json!({"request_id":"jsonl-slow","command":"provider_request","platform":"kw","request":{"operation":"search","keyword":"x","page":1,"limit":30}}),
    );
    assert_eq!(accepted["ok"], true);
    assert_eq!(accepted["result"]["receipt"]["state"], "accepted");
    wait_request(&fixture, 1);
    let started = Instant::now();
    let cancelled = exchange(
        json!({"request_id":"jsonl-cancel","command":"cancel_operation","operation_id":"jsonl-slow"}),
    );
    assert_eq!(cancelled["ok"], true);
    assert!(started.elapsed() < Duration::from_millis(200));
    let result = exchange(json!({"query":"provider_result","operation_id":"jsonl-slow"}));
    assert_eq!(result["ok"], true);
    assert_eq!(result["result"]["state"], "failed");
    assert_eq!(result["result"]["error"]["kind"], "cancelled");
    assert_eq!(result["result"]["response"], Value::Null);
    drop(exchange);
    drop(input);
    let status = child.wait().unwrap();
    let mut stderr = String::new();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut stderr)
        .unwrap();
    assert!(status.success(), "CLI failed: {stderr}");
    let request = &fixture.requests.lock().unwrap()[0];
    assert!(request.starts_with("GET http://search.kuwo.cn/r.s?"));
}

#[test]
fn cli_one_shot_waits_for_provider_terminal_response() {
    let fixture = HttpFixture::new(vec![(response("one-shot"), Duration::from_millis(60))]);
    let base = std::env::temp_dir().join("backend-platform-fixtures");
    fs::create_dir_all(&base).unwrap();
    let directory = tempfile::Builder::new()
        .prefix("one-shot-profile-")
        .tempdir_in(base)
        .unwrap();
    let command = json!({"request_id":"one-shot","command":"provider_request","platform":"kw","request":{"operation":"search","keyword":"x","page":1,"limit":30}});
    let output = Command::new(env!("CARGO_BIN_EXE_lx-native-cli"))
        .arg("--data-dir")
        .arg(directory.path())
        .arg("--provider-proxy")
        .arg(&fixture.base_url)
        .arg("--command")
        .arg(command.to_string())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["ok"], true);
    assert_eq!(result["result"]["provider_result"]["state"], "completed");
    assert_eq!(
        result["result"]["provider_result"]["response"]["kind"],
        "tracks"
    );
    assert_eq!(
        result["result"]["provider_result"]["response"]["data"]["items"][0]["title"],
        "Fixture Song"
    );
    assert_eq!(fixture.requests.lock().unwrap().len(), 1);
}

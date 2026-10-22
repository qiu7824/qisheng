//! Picture consent and persistent encoded bytes through the actual backend boundary.
use image::{ExtendedColorType, ImageEncoder};
use lofty::{
    config::WriteOptions,
    picture::{MimeType, Picture, PictureType},
    tag::{ItemKey, Tag, TagExt, TagType},
};
use lx_gpui_player::{
    backend::{
        BackendCommand, BackendConfig, BackendEvent, BackendHandle, BackendQuery,
        BackendQueryResult, CommandEnvelope, ReceiptState,
    },
    domain::{
        artwork::{
            ArtworkLookup, ArtworkProvenance, ImageCacheStats, PictureOperationView, PictureState,
        },
        lists::{AddLocation, ListAction},
        music::{LxMusicDtoVersion, TrackRecord},
    },
    pictures::service::PictureService,
    platforms::{
        HttpRequest, HttpResponse, HttpTransport, ProviderContext, ProviderError, ProviderRegistry,
        ReqwestTransport, TransportConfig,
    },
    repository::{DATABASE_NAME, local_track_from_path},
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::Path,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, Instant},
};
static CASES: Mutex<()> = Mutex::new(());

fn png() -> Vec<u8> {
    let mut bytes = Vec::new();
    image::codecs::png::PngEncoder::new(&mut bytes)
        .write_image(
            &[
                255, 0, 30, 255, 0, 255, 80, 255, 10, 20, 255, 255, 240, 230, 10, 255,
            ],
            2,
            2,
            ExtendedColorType::Rgba8,
        )
        .unwrap();
    bytes
}
struct State {
    base: String,
    body: Vec<u8>,
    metadata: AtomicUsize,
    gets: AtomicUsize,
    forbidden: AtomicUsize,
    sources: Mutex<Vec<Value>>,
    hold_metadata: AtomicBool,
    hold_body: AtomicBool,
    fail_source: AtomicBool,
    stop: AtomicBool,
    gate: Mutex<()>,
    changed: Condvar,
    mime: Mutex<String>,
}
struct Server {
    base: String,
    state: Arc<State>,
    thread: Option<thread::JoinHandle<()>>,
}
impl Server {
    fn new() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let state = Arc::new(State {
            base: base.clone(),
            body: png(),
            metadata: AtomicUsize::new(0),
            gets: AtomicUsize::new(0),
            forbidden: AtomicUsize::new(0),
            sources: Mutex::new(Vec::new()),
            hold_metadata: AtomicBool::new(false),
            hold_body: AtomicBool::new(false),
            fail_source: AtomicBool::new(false),
            stop: AtomicBool::new(false),
            gate: Mutex::new(()),
            changed: Condvar::new(),
            mime: Mutex::new("image/png".into()),
        });
        let running = state.clone();
        let handle = thread::spawn(move || {
            let mut clients = Vec::new();
            while !running.stop.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((socket, _)) => {
                        let state = running.clone();
                        clients.push(thread::spawn(move || serve(socket, state)));
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2))
                    }
                    Err(error) => panic!("picture fixture accept: {error}"),
                }
            }
            for client in clients {
                client.join().unwrap();
            }
        });
        Self {
            base,
            state,
            thread: Some(handle),
        }
    }
    fn url(&self) -> String {
        format!("{}/cover.png?signature=original-picture", self.base)
    }
    fn release(&self) {
        self.state.hold_metadata.store(false, Ordering::Release);
        self.state.hold_body.store(false, Ordering::Release);
        self.state.changed.notify_all();
    }
    fn wait(&self, predicate: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !predicate() {
            assert!(
                Instant::now() < deadline,
                "picture fixture did not observe an actual request"
            );
            thread::sleep(Duration::from_millis(2));
        }
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        self.state.stop.store(true, Ordering::Release);
        self.release();
        self.thread.take().unwrap().join().unwrap();
    }
}
fn serve(mut socket: TcpStream, state: Arc<State>) {
    socket.set_nonblocking(false).unwrap();
    socket
        .set_read_timeout(Some(Duration::from_secs(4)))
        .unwrap();
    socket
        .set_write_timeout(Some(Duration::from_secs(4)))
        .unwrap();
    let mut bytes = Vec::new();
    let mut scratch = [0; 4096];
    let end = loop {
        let Ok(count) = socket.read(&mut scratch) else {
            return;
        };
        if count == 0 {
            return;
        }
        bytes.extend_from_slice(&scratch[..count]);
        if let Some(end) = bytes.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
            break end;
        }
        assert!(bytes.len() <= 16384);
    };
    let headers = String::from_utf8_lossy(&bytes[..end]);
    let line = headers.lines().next().unwrap();
    let method = line.split_whitespace().next().unwrap().to_owned();
    let target = line.split_whitespace().nth(1).unwrap().to_owned();
    let length = headers
        .lines()
        .find_map(|line| {
            line.split_once(':')
                .filter(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                .map(|(_, value)| value.trim().parse::<usize>().unwrap())
        })
        .unwrap_or(0);
    while bytes.len() < end + 4 + length {
        let Ok(count) = socket.read(&mut scratch) else {
            return;
        };
        if count == 0 {
            return;
        }
        bytes.extend_from_slice(&scratch[..count]);
    }
    if target.starts_with("/metadata") {
        assert_eq!(method, "GET");
        state.metadata.fetch_add(1, Ordering::AcqRel);
        wait_gate(&state, &state.hold_metadata);
        reply(
            &mut socket,
            "200 OK",
            "text/plain",
            format!("{}/cover.png?signature=original-picture", state.base).as_bytes(),
        );
    } else if target.starts_with("/source") {
        assert_eq!(method, "POST");
        let value: Value = serde_json::from_slice(&bytes[end + 4..end + 4 + length]).unwrap();
        state.sources.lock().unwrap().push(value);
        let result = if state.fail_source.load(Ordering::Acquire) {
            json!({"error":"original source fixture rejection"})
        } else {
            json!({"url":format!("{}/cover.png?signature=source-picture",state.base)})
        };
        reply(
            &mut socket,
            "200 OK",
            "application/json",
            result.to_string().as_bytes(),
        );
    } else if target.starts_with("/redirect") {
        state.gets.fetch_add(1, Ordering::AcqRel);
        let _ = write!(
            socket,
            "HTTP/1.1 302 Found\r\nLocation: {}/forbidden.png\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            state.base
        );
    } else if target.starts_with("/forbidden") {
        state.forbidden.fetch_add(1, Ordering::AcqRel);
        reply(&mut socket, "200 OK", "image/png", &state.body);
    } else {
        assert!(
            target.starts_with("/cover.png"),
            "unexpected picture GET: {target}"
        );
        assert_eq!(method, "GET");
        state.gets.fetch_add(1, Ordering::AcqRel);
        wait_gate(&state, &state.hold_body);
        let mime = state.mime.lock().unwrap().clone();
        reply(&mut socket, "200 OK", &mime, &state.body);
    }
}
fn wait_gate(state: &State, gate: &AtomicBool) {
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut guard = state.gate.lock().unwrap();
    while gate.load(Ordering::Acquire) && !state.stop.load(Ordering::Acquire) {
        assert!(Instant::now() < deadline);
        guard = state
            .changed
            .wait_timeout(guard, Duration::from_millis(10))
            .unwrap()
            .0;
    }
}
fn reply(socket: &mut TcpStream, status: &str, mime: &str, body: &[u8]) {
    let header = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {mime}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = socket
        .write_all(header.as_bytes())
        .and_then(|()| socket.write_all(body));
}
struct NativeMetadata {
    base: String,
    client: ReqwestTransport,
    requests: AtomicUsize,
}
impl HttpTransport for NativeMetadata {
    fn execute(
        &self,
        request: HttpRequest,
        context: &ProviderContext,
    ) -> Result<HttpResponse, ProviderError> {
        context.check()?;
        let url = reqwest::Url::parse(&request.url).unwrap();
        assert_eq!(url.host_str(), Some("artistpicserver.kuwo.cn"));
        assert_eq!(url.path(), "/pic.web");
        assert_eq!(request.method, "GET");
        let query: std::collections::BTreeMap<_, _> = url.query_pairs().collect();
        assert_eq!(query["type"], "rid_pic");
        assert_eq!(query["pictype"], "500");
        self.requests.fetch_add(1, Ordering::AcqRel);
        let mut rewritten = request;
        rewritten.url = format!("{}/metadata", self.base);
        self.client.execute(rewritten, context)
    }
}
struct Running {
    backend: BackendHandle,
    directory: tempfile::TempDir,
    metadata: Arc<NativeMetadata>,
    original: TrackRecord,
}
impl Running {
    fn new(server: &Server, metadata_url: Option<String>) -> Self {
        let base = std::env::temp_dir().join("lx-native-artwork-tests");
        fs::create_dir_all(&base).unwrap();
        let directory = tempfile::Builder::new()
            .prefix("profile-")
            .tempdir_in(base)
            .unwrap();
        let metadata = Arc::new(NativeMetadata {
            base: server.base.clone(),
            client: ReqwestTransport::new(TransportConfig {
                proxy: None,
                max_response_bytes: 8192,
                retries: 0,
            })
            .unwrap(),
            requests: AtomicUsize::new(0),
        });
        let backend = start(directory.path(), metadata.clone());
        let original=TrackRecord::from_lx_dto(json!({"source":"kw","songmid":"picture-original","name":"Original artwork music","singer":"Fixture author","albumName":"Original pictures","img":metadata_url,"hash":"initial-picture-metadata","types":[{"type":"128k"}],"_types":{"128k":{}}}),LxMusicDtoVersion::Legacy).unwrap();
        add(&backend, &original, "add-picture-track");
        Self {
            backend,
            directory,
            metadata,
            original,
        }
    }
    fn resolve(&self, id: &str, record: &TrackRecord) -> PictureOperationView {
        command(
            &self.backend,
            id,
            BackendCommand::ResolvePicture {
                list_id: "default".into(),
                music_id: record.id.clone(),
            },
        );
        wait_view(&self.backend, id, |view| {
            matches!(
                view.state,
                PictureState::AwaitingApproval
                    | PictureState::Ready
                    | PictureState::Failed
                    | PictureState::Cancelled
            )
        })
    }
    fn approve(&self, id: &str, operation: &PictureOperationView) -> PictureOperationView {
        command(
            &self.backend,
            id,
            BackendCommand::ApprovePictureUrl {
                operation_id: operation.operation_id.clone(),
                url: operation
                    .info
                    .as_ref()
                    .unwrap()
                    .url
                    .as_ref()
                    .unwrap()
                    .as_str()
                    .into(),
            },
        );
        wait_view(&self.backend, &operation.operation_id, |view| {
            matches!(
                view.state,
                PictureState::Ready | PictureState::Failed | PictureState::Cancelled
            )
        })
    }
}
impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.backend.shutdown(Duration::from_secs(10));
    }
}
fn start(path: &Path, metadata: Arc<NativeMetadata>) -> BackendHandle {
    let mut config = BackendConfig::headless(path.join("profile"));
    config.provider_registry = Some(Arc::new(ProviderRegistry::new(metadata)));
    BackendHandle::start(config).unwrap()
}
fn command(
    backend: &BackendHandle,
    id: &str,
    command: BackendCommand,
) -> lx_gpui_player::backend::CommandReceipt {
    backend
        .execute(CommandEnvelope {
            request_id: id.into(),
            origin: "artwork-test".into(),
            expected_revision: None,
            command,
        })
        .unwrap()
}
fn add(backend: &BackendHandle, record: &TrackRecord, id: &str) {
    command(
        backend,
        id,
        BackendCommand::ListAction {
            action: Box::new(ListAction::MusicAdd {
                list_id: "default".into(),
                tracks: vec![record.clone()],
                location: AddLocation::Bottom,
            }),
        },
    );
}
fn wait_view(
    backend: &BackendHandle,
    id: &str,
    predicate: impl Fn(&PictureOperationView) -> bool,
) -> PictureOperationView {
    let deadline = Instant::now() + Duration::from_secs(12);
    loop {
        let BackendQueryResult::PictureOperations(views) = backend
            .query(BackendQuery::PictureOperations {
                offset: 0,
                limit: 100,
            })
            .unwrap()
        else {
            panic!()
        };
        if let Some(view) = views
            .into_iter()
            .find(|view| view.operation_id == id && predicate(view))
        {
            return view;
        }
        assert!(
            Instant::now() < deadline,
            "picture operation {id} did not reach its actual state"
        );
        thread::sleep(Duration::from_millis(5));
    }
}
fn stats(backend: &BackendHandle) -> ImageCacheStats {
    let BackendQueryResult::ImageCacheStats(stats) =
        backend.query(BackendQuery::ImageCacheStats).unwrap()
    else {
        panic!()
    };
    stats
}
fn bytes(backend: &BackendHandle, sha: &str) -> Vec<u8> {
    let mut bytes = Vec::new();
    loop {
        let BackendQueryResult::PictureCachedBytes(page) = backend
            .query(BackendQuery::PictureCachedBytes {
                sha256: sha.into(),
                offset: bytes.len() as u64,
                limit: 65536,
            })
            .unwrap()
        else {
            panic!()
        };
        assert!(page.bytes.len() <= 65536);
        bytes.extend(page.bytes);
        if bytes.len() as u64 == page.total {
            return bytes;
        }
    }
}
fn info(backend: &BackendHandle, record: &TrackRecord) -> ArtworkLookup {
    let BackendQueryResult::PictureInfo(info) = backend
        .query(BackendQuery::PictureInfo {
            list_id: "default".into(),
            music_id: record.id.clone(),
        })
        .unwrap()
    else {
        panic!()
    };
    info
}

#[test]
fn native_picture_resolution_and_info_hits_do_not_get_image_bytes_or_reuse_audio_consent() {
    let _case = CASES.lock().unwrap_or_else(|error| error.into_inner());
    let server = Server::new();
    let running = Running::new(&server, None);
    let revision = running.backend.snapshot().revision;
    let first = running.resolve("native-first", &running.original);
    assert_eq!(first.state, PictureState::AwaitingApproval);
    assert!(matches!(
        first.info.as_ref().unwrap().provenance,
        ArtworkProvenance::Provider { .. }
    ));
    assert_eq!(server.state.metadata.load(Ordering::Acquire), 1);
    assert_eq!(server.state.gets.load(Ordering::Acquire), 0);
    assert!(matches!(
        info(&running.backend, &running.original),
        ArtworkLookup::Hit(_)
    ));
    let second = running.resolve("native-hit", &running.original);
    assert!(second.cached);
    assert_eq!(server.state.metadata.load(Ordering::Acquire), 1);
    command(
        &running.backend,
        "audio-consent",
        BackendCommand::ApprovePlaybackUrl,
    );
    assert_eq!(server.state.gets.load(Ordering::Acquire), 0);
    let wrong = running
        .backend
        .execute(CommandEnvelope {
            request_id: "wrong-picture-url".into(),
            origin: "artwork-test".into(),
            expected_revision: None,
            command: BackendCommand::ApprovePictureUrl {
                operation_id: first.operation_id.clone(),
                url: format!("{}/different.png", server.base),
            },
        })
        .unwrap_err();
    assert_eq!(wrong.kind, lx_gpui_player::backend::ErrorKind::Conflict);
    assert_eq!(server.state.gets.load(Ordering::Acquire), 0);
    let ready = running.approve("picture-consent", &first);
    assert_eq!(ready.state, PictureState::Ready, "{ready:?}");
    assert_eq!(server.state.gets.load(Ordering::Acquire), 1);
    let blob = ready.info.unwrap().blob.unwrap();
    assert_eq!(bytes(&running.backend, &blob.sha256), server.state.body);
    assert_eq!(
        blob.sha256,
        format!("{:x}", Sha256::digest(&server.state.body))
    );
    assert_eq!((blob.width, blob.height, blob.frames), (2, 2, 1));
    assert_eq!(running.backend.snapshot().revision, revision);
    assert!(
        running
            .backend
            .query(BackendQuery::PictureCachedBytes {
                sha256: blob.sha256,
                offset: 0,
                limit: 65537
            })
            .is_err()
    );
    assert!(
        serde_json::to_vec(&running.backend.snapshot())
            .unwrap()
            .len()
            < 65536
    );
}

#[test]
fn persistent_blob_reopens_offline_with_exact_bytes_and_honest_shared_database_usage() {
    let _case = CASES.lock().unwrap_or_else(|error| error.into_inner());
    let server = Server::new();
    let running = Running::new(&server, None);
    let pending = running.resolve("disk-first", &running.original);
    let ready = running.approve("disk-approve", &pending);
    assert_eq!(ready.state, PictureState::Ready, "{ready:?}");
    let sha = ready.info.unwrap().blob.unwrap().sha256;
    running.backend.shutdown(Duration::from_secs(10)).unwrap();
    assert_eq!(PictureService::outstanding_workers(), 0);
    let reopened = start(running.directory.path(), running.metadata.clone());
    command(
        &reopened,
        "disk-offline",
        BackendCommand::ResolvePicture {
            list_id: "default".into(),
            music_id: running.original.id.clone(),
        },
    );
    let cached = wait_view(&reopened, "disk-offline", |view| {
        view.state == PictureState::Ready
    });
    assert!(cached.cached);
    assert_eq!(bytes(&reopened, &sha), server.state.body);
    assert_eq!(server.state.metadata.load(Ordering::Acquire), 1);
    assert_eq!(server.state.gets.load(Ordering::Acquire), 1);
    let deadline = Instant::now() + Duration::from_secs(5);
    let actual = loop {
        let stats = stats(&reopened);
        if stats.physical_observed_at_ms.is_some() {
            break stats;
        }
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(5));
    };
    assert_eq!(actual.blob_entries, 1);
    assert_eq!(actual.blob_bytes, server.state.body.len() as u64);
    assert!(actual.database_bytes.is_some_and(|bytes| bytes > 0));
    assert!(
        fs::metadata(running.directory.path().join("profile").join(DATABASE_NAME))
            .unwrap()
            .len()
            > 0
    );
    command(&reopened, "clear-images", BackendCommand::ClearImageCache);
    assert_eq!(stats(&reopened).blob_entries, 0);
    assert_eq!(stats(&reopened).info_entries, 0);
    assert!(
        reopened
            .query(BackendQuery::PictureCachedBytes {
                sha256: sha,
                offset: 0,
                limit: 64
            })
            .is_err()
    );
    reopened.shutdown(Duration::from_secs(10)).unwrap();
    assert_eq!(PictureService::outstanding_workers(), 0);
}

#[test]
fn metadata_urls_use_no_provider_call_and_identical_payloads_deduplicate_by_sha() {
    let _case = CASES.lock().unwrap_or_else(|error| error.into_inner());
    let server = Server::new();
    let running = Running::new(&server, Some(server.url()));
    let first = running.resolve("metadata-a", &running.original);
    assert!(matches!(
        first.info.as_ref().unwrap().provenance,
        ArtworkProvenance::Metadata { .. }
    ));
    assert_eq!(running.metadata.requests.load(Ordering::Acquire), 0);
    let first = running.approve("metadata-a-approve", &first);
    assert_eq!(first.state, PictureState::Ready, "{first:?}");
    let mut other = running.original.clone();
    other.id = "second-picture-record".into();
    other.lx_id = "kw_picture-second".into();
    other.platform.song_id = json!("picture-second");
    add(&running.backend, &other, "add-second");
    let second = running.resolve("metadata-b", &other);
    assert_eq!(second.state, PictureState::AwaitingApproval);
    let second = running.approve("metadata-b-approve", &second);
    assert_eq!(second.state, PictureState::Ready, "{second:?}");
    assert_eq!(stats(&running.backend).info_entries, 2);
    assert_eq!(stats(&running.backend).blob_entries, 1);
    assert_eq!(
        stats(&running.backend).blob_bytes,
        server.state.body.len() as u64
    );
    assert_eq!(server.state.gets.load(Ordering::Acquire), 2);
}

#[test]
fn late_metadata_result_is_cancelled_after_the_exact_list_dto_changes_and_controls_remain_responsive()
 {
    let _case = CASES.lock().unwrap_or_else(|error| error.into_inner());
    let server = Server::new();
    server.state.hold_metadata.store(true, Ordering::Release);
    let running = Running::new(&server, None);
    let cursor = running.backend.event_cursor();
    let receipt = command(
        &running.backend,
        "held-metadata",
        BackendCommand::ResolvePicture {
            list_id: "default".into(),
            music_id: running.original.id.clone(),
        },
    );
    assert_eq!(receipt.state, ReceiptState::Accepted);
    server.wait(|| server.state.metadata.load(Ordering::Acquire) == 1);
    let mut samples = Vec::new();
    for _ in 0..20 {
        let now = Instant::now();
        let _ = running.backend.snapshot_with_cursor().unwrap();
        samples.push(now.elapsed());
    }
    samples.sort_unstable();
    assert!(samples[18] < Duration::from_millis(100));
    let mut changed = running.original.clone();
    changed
        .metadata
        .insert("hash".into(), "updated-picture-dto".into());
    command(
        &running.backend,
        "change-dto",
        BackendCommand::ListAction {
            action: Box::new(ListAction::MusicOverwrite {
                list_id: "default".into(),
                tracks: vec![changed.clone()],
            }),
        },
    );
    server.release();
    let view = wait_view(&running.backend, "held-metadata", |view| {
        matches!(view.state, PictureState::Cancelled | PictureState::Failed)
    });
    assert_eq!(view.state, PictureState::Cancelled, "{view:?}");
    assert_eq!(stats(&running.backend).info_entries, 0);
    assert_eq!(server.state.gets.load(Ordering::Acquire), 0);
    assert!(matches!(
        info(&running.backend, &changed),
        ArtworkLookup::Miss
    ));
    assert!(running.backend.events_after(cursor).unwrap().iter().any(|event|matches!(&event.payload,BackendEvent::OperationFinished{operation_id,success:false,..}if operation_id==&receipt.operation_id)));
}

#[test]
fn cancel_and_clear_during_an_actual_get_do_not_publish_late_bytes_and_shutdown_joins_workers() {
    let _case = CASES.lock().unwrap_or_else(|error| error.into_inner());
    let server = Server::new();
    let running = Running::new(&server, Some(server.url()));
    let pending = running.resolve("cancel-download", &running.original);
    server.state.hold_body.store(true, Ordering::Release);
    command(
        &running.backend,
        "approve-held",
        BackendCommand::ApprovePictureUrl {
            operation_id: pending.operation_id.clone(),
            url: pending.info.unwrap().url.unwrap().as_str().into(),
        },
    );
    server.wait(|| server.state.gets.load(Ordering::Acquire) == 1);
    command(
        &running.backend,
        "cancel-held",
        BackendCommand::CancelPicture {
            operation_id: pending.operation_id.clone(),
        },
    );
    let cancelled = wait_view(&running.backend, &pending.operation_id, |view| {
        view.state == PictureState::Cancelled
    });
    assert!(
        cancelled
            .info
            .as_ref()
            .is_none_or(|info| info.blob.is_none())
    );
    command(
        &running.backend,
        "clear-held",
        BackendCommand::ClearImageCache,
    );
    server.release();
    running.backend.shutdown(Duration::from_secs(10)).unwrap();
    assert_eq!(PictureService::outstanding_workers(), 0);
    let reopened = start(running.directory.path(), running.metadata.clone());
    assert_eq!(stats(&reopened).blob_entries, 0);
    assert_eq!(stats(&reopened).info_entries, 0);
    reopened.shutdown(Duration::from_secs(10)).unwrap();
}

#[test]
fn picture_redirects_and_declared_type_mismatches_fail_without_approving_another_url() {
    let _case = CASES.lock().unwrap_or_else(|error| error.into_inner());
    let server = Server::new();
    let running = Running::new(&server, Some(format!("{}/redirect", server.base)));
    let pending = running.resolve("redirect-meta", &running.original);
    let failed = running.approve("approve-redirect", &pending);
    assert_eq!(failed.state, PictureState::Failed, "{failed:?}");
    assert_eq!(server.state.gets.load(Ordering::Acquire), 1);
    assert_eq!(server.state.forbidden.load(Ordering::Acquire), 0);
    assert_eq!(stats(&running.backend).blob_entries, 0);
    let mut corrected = running.original.clone();
    corrected
        .metadata
        .insert("picUrl".into(), server.url().into());
    command(
        &running.backend,
        "new-mime-picture",
        BackendCommand::ListAction {
            action: Box::new(ListAction::MusicOverwrite {
                list_id: "default".into(),
                tracks: vec![corrected.clone()],
            }),
        },
    );
    *server.state.mime.lock().unwrap() = "image/jpeg".into();
    let pending = running.resolve("type-meta", &corrected);
    let failed = running.approve("approve-type", &pending);
    assert_eq!(failed.state, PictureState::Failed, "{failed:?}");
    assert_eq!(stats(&running.backend).blob_entries, 0);
}

fn tagged_wav(path: &Path) -> Vec<u8> {
    let count = 8000u32;
    let data = count * 2;
    let mut bytes = b"RIFF".to_vec();
    bytes.extend((data + 36).to_le_bytes());
    bytes.extend(b"WAVEfmt ");
    bytes.extend(16u32.to_le_bytes());
    bytes.extend(1u16.to_le_bytes());
    bytes.extend(1u16.to_le_bytes());
    bytes.extend(8000u32.to_le_bytes());
    bytes.extend(16000u32.to_le_bytes());
    bytes.extend(2u16.to_le_bytes());
    bytes.extend(16u16.to_le_bytes());
    bytes.extend(b"data");
    bytes.extend(data.to_le_bytes());
    for index in 0..count {
        let sample =
            ((index as f64 * std::f64::consts::TAU * 440.0 / 8000.0).sin() * 6000.0) as i16;
        bytes.extend(sample.to_le_bytes());
    }
    bytes.truncate(44 + data as usize);
    fs::write(path, bytes).unwrap();
    let picture = png();
    let mut tag = Tag::new(TagType::Id3v2);
    tag.insert_text(ItemKey::TrackTitle, "Original embedded artwork".into());
    tag.push_picture(
        Picture::unchecked(picture.clone())
            .pic_type(PictureType::CoverFront)
            .mime_type(MimeType::Png)
            .build(),
    );
    tag.save_to_path(path, WriteOptions::default()).unwrap();
    picture
}
fn import_local(running: &Running, path: &Path) -> TrackRecord {
    command(
        &running.backend,
        "import-local",
        BackendCommand::ImportAudio {
            paths: vec![path.into()],
        },
    );
    let (local, _) = local_track_from_path(path).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !running
        .backend
        .snapshot()
        .tracks
        .iter()
        .any(|track| track.id == local.id)
    {
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(5));
    }
    add(&running.backend, &local, "add-local-to-list");
    local
}
#[test]
fn embedded_artwork_uses_catalog_permission_and_preserves_the_exact_audio_file() {
    let _case = CASES.lock().unwrap_or_else(|error| error.into_inner());
    let server = Server::new();
    let running = Running::new(&server, None);
    let path = running.directory.path().join("original-with-artwork.wav");
    let expected = tagged_wav(&path);
    let audio_before = fs::read(&path).unwrap();
    let local = import_local(&running, &path);
    let ready = running.resolve("embedded-picture", &local);
    assert_eq!(ready.state, PictureState::Ready, "{ready:?}");
    let picture = ready.info.unwrap();
    assert!(matches!(
        picture.provenance,
        ArtworkProvenance::Embedded { .. }
    ));
    assert!(picture.url.is_none());
    assert_eq!(
        bytes(&running.backend, &picture.blob.unwrap().sha256),
        expected
    );
    assert_eq!(fs::read(&path).unwrap(), audio_before);
    assert_eq!(server.state.gets.load(Ordering::Acquire), 0);
    assert_eq!(running.metadata.requests.load(Ordering::Acquire), 0);
}
fn source_script(server: &Server, version: &str) -> String {
    format!(
        r#"/**
 * @name Picture source {version}
 * @version {version}
 * @author Fixture composer
 */
let fixturePictureOrdinal=0;
globalThis.lx.on(globalThis.lx.EVENT_NAMES.request,({{source,action,info}})=>new Promise((resolve,reject)=>{{
  const payload={{source,action,musicInfo:info.musicInfo,ordinal:++fixturePictureOrdinal}};
  globalThis.lx.request('{base}/source',{{method:'POST',headers:{{'Content-Type':'application/json'}},body:payload}},(error,response)=>{{if(error)return reject(error);if(response.body.error)return reject(Error(response.body.error));resolve(response.body.url);}});
}}));
globalThis.lx.send(globalThis.lx.EVENT_NAMES.inited,{{sources:{{local:{{name:'Actual local pictures',type:'music',actions:['pic','musicUrl'],qualitys:[]}}}}}});
"#,
        base = server.base
    )
}
fn terminal(backend: &BackendHandle, id: &str, command_value: BackendCommand) {
    let cursor = backend.event_cursor();
    let receipt = command(backend, id, command_value);
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
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(5));
    }
}
fn enable_source(running: &Running, server: &Server, version: &str) -> String {
    let script = source_script(server, version);
    let id = lx_gpui_player::source::script_hash(&script);
    terminal(
        &running.backend,
        &format!("import-source-{version}"),
        BackendCommand::ImportSourceText { script },
    );
    assert!(
        running
            .backend
            .snapshot()
            .sources
            .iter()
            .any(|source| source.id == id),
        "Imported picture source SHA was not registered"
    );
    terminal(
        &running.backend,
        &format!("approve-source-{version}"),
        BackendCommand::ApproveSource { id: id.clone() },
    );
    terminal(
        &running.backend,
        &format!("enable-source-{version}"),
        BackendCommand::EnableSource { id: id.clone() },
    );
    id
}
#[test]
fn local_source_pic_uses_one_real_session_and_actual_catalog_dto_then_falls_back_to_embedded_bytes()
{
    let _case = CASES.lock().unwrap_or_else(|error| error.into_inner());
    let server = Server::new();
    let running = Running::new(&server, None);
    let path = running.directory.path().join("source-with-artwork.wav");
    let expected = tagged_wav(&path);
    let mut local = import_local(&running, &path);
    local
        .metadata
        .insert("filePath".into(), "untrusted-placeholder.wav".into());
    command(
        &running.backend,
        "bad-raw-filepath",
        BackendCommand::ListAction {
            action: Box::new(ListAction::MusicOverwrite {
                list_id: "default".into(),
                tracks: vec![running.original.clone(), local.clone()],
            }),
        },
    );
    enable_source(&running, &server, "1");
    let first = running.resolve("local-source-pic", &local);
    assert_eq!(first.state, PictureState::AwaitingApproval, "{first:?}");
    assert_eq!(server.state.gets.load(Ordering::Acquire), 0);
    let calls = server.state.sources.lock().unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0]["source"], "local");
    assert_eq!(calls[0]["action"], "pic");
    let file = calls[0]["musicInfo"]["filePath"].as_str().unwrap();
    assert!(file.starts_with("http://127.0.0.1:"));
    assert_ne!(file, "untrusted-placeholder.wav");
    drop(calls);
    command(
        &running.backend,
        "clear-source-picture-info",
        BackendCommand::ClearImageCache,
    );
    server.state.fail_source.store(true, Ordering::Release);
    let fallback = running.resolve("local-source-fallback", &local);
    assert_eq!(fallback.state, PictureState::Ready, "{fallback:?}");
    let calls = server.state.sources.lock().unwrap();
    assert_eq!(calls.len(), 2);
    assert_eq!(
        calls[1]["ordinal"], 2,
        "picture operations must retain the same JS session"
    );
    drop(calls);
    let picture = fallback.info.unwrap();
    assert!(matches!(
        picture.provenance,
        ArtworkProvenance::Embedded { .. }
    ));
    assert_eq!(
        bytes(&running.backend, &picture.blob.unwrap().sha256),
        expected
    );
    assert_eq!(server.state.gets.load(Ordering::Acquire), 0);
}

#[test]
fn changed_source_sha_revokes_pending_picture_consent_and_resolves_a_new_provenance() {
    let _case = CASES.lock().unwrap_or_else(|error| error.into_inner());
    let server = Server::new();
    let running = Running::new(&server, None);
    let path = running.directory.path().join("policy-picture.wav");
    tagged_wav(&path);
    let local = import_local(&running, &path);
    let old_source = enable_source(&running, &server, "1.0.0");
    let pending = running.resolve("old-source-picture", &local);
    assert_eq!(pending.state, PictureState::AwaitingApproval, "{pending:?}");
    let new_source = enable_source(&running, &server, "2.0.0");
    assert_ne!(old_source, new_source);
    assert!(
        running
            .backend
            .execute(CommandEnvelope {
                request_id: "approve-old-picture".into(),
                origin: "artwork-test".into(),
                expected_revision: None,
                command: BackendCommand::ApprovePictureUrl {
                    operation_id: pending.operation_id,
                    url: pending.info.unwrap().url.unwrap().as_str().into()
                }
            })
            .is_err()
    );
    assert_eq!(server.state.gets.load(Ordering::Acquire), 0);
    let current = running.resolve("new-source-picture", &local);
    assert_eq!(current.state, PictureState::AwaitingApproval, "{current:?}");
    assert!(
        matches!(current.info.unwrap().provenance,ArtworkProvenance::Source{source_sha256,..}if source_sha256==new_source)
    );
    assert_eq!(server.state.sources.lock().unwrap().len(), 2);
}

#[test]
fn expired_persistent_picture_information_resolves_again_without_downloading_payloads() {
    let _case = CASES.lock().unwrap_or_else(|error| error.into_inner());
    let server = Server::new();
    let running = Running::new(&server, None);
    let pending = running.resolve("ttl-picture", &running.original);
    let mut stored = pending.info.unwrap();
    stored.resolved_at_ms = 0;
    stored.expire_at_ms = 1;
    stored.validate().unwrap();
    let document = serde_json::to_string(&stored).unwrap();
    let connection =
        rusqlite::Connection::open(running.directory.path().join("profile").join(DATABASE_NAME))
            .unwrap();
    connection.execute("UPDATE artwork_info SET resolved_at_ms=0,expire_at_ms=1,last_access_ms=0,byte_size=?1,document=?2 WHERE cache_key=?3",rusqlite::params![document.len() as i64,document,stored.key.fingerprint().unwrap()]).unwrap();
    drop(connection);
    assert!(matches!(
        info(&running.backend, &running.original),
        ArtworkLookup::Expired
    ));
    let fresh = running.resolve("ttl-picture-fresh", &running.original);
    assert_eq!(fresh.state, PictureState::AwaitingApproval, "{fresh:?}");
    assert!(!fresh.cached);
    assert_eq!(server.state.metadata.load(Ordering::Acquire), 2);
    assert_eq!(server.state.gets.load(Ordering::Acquire), 0);
}

#[test]
fn offline_reopen_rejects_corrupted_stored_picture_bytes_without_a_network_get() {
    let _case = CASES.lock().unwrap_or_else(|error| error.into_inner());
    let server = Server::new();
    let running = Running::new(&server, None);
    let pending = running.resolve("corruption-meta", &running.original);
    let ready = running.approve("corruption-approve", &pending);
    assert_eq!(ready.state, PictureState::Ready, "{ready:?}");
    let sha = ready.info.unwrap().blob.unwrap().sha256;
    running.backend.shutdown(Duration::from_secs(10)).unwrap();
    let mut corrupted = server.state.body.clone();
    let damaged = corrupted.len() / 2;
    corrupted[damaged] ^= 0x80;
    let connection =
        rusqlite::Connection::open(running.directory.path().join("profile").join(DATABASE_NAME))
            .unwrap();
    connection
        .execute(
            "UPDATE artwork_blobs SET data=?1 WHERE sha256=?2",
            rusqlite::params![corrupted, sha],
        )
        .unwrap();
    drop(connection);
    let reopened = start(running.directory.path(), running.metadata.clone());
    command(
        &reopened,
        "corrupted-offline",
        BackendCommand::ResolvePicture {
            list_id: "default".into(),
            music_id: running.original.id.clone(),
        },
    );
    let rejected = wait_view(&reopened, "corrupted-offline", |view| {
        matches!(view.state, PictureState::Failed | PictureState::Cancelled)
    });
    assert_eq!(rejected.state, PictureState::Failed, "{rejected:?}");
    assert_eq!(server.state.gets.load(Ordering::Acquire), 1);
    assert_eq!(server.state.metadata.load(Ordering::Acquire), 1);
    reopened.shutdown(Duration::from_secs(10)).unwrap();
    assert_eq!(PictureService::outstanding_workers(), 0);
}

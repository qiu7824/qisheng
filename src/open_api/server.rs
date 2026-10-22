use super::protocol::{
    Filter, MAX_STATUS_BYTES, Route, invalid, lyrics, parse_route, status_value,
};
use super::{
    ArtworkResponse, ControlError, ControlErrorKind, ControlResult, ExternalControlBackend,
    LxPlayerStatus,
};
use crate::audio::worker_owner::{TrackedWorker, WorkerKind, WorkerPermit};
use sha2::{Digest, Sha256};
use std::collections::VecDeque;
use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
    mpsc::{self, Receiver, SyncSender, TrySendError},
};
use std::time::{Duration, Instant};

const CONNECTIONS: usize = 32;
const SUBSCRIPTIONS: usize = 16;
const JOBS: usize = 32;
const MAX_HEADER: usize = 16 * 1024;
const MAX_BODY: usize = 64 * 1024;
const FRAME_COUNT: usize = 64;
const FRAME_BYTES: usize = 256 * 1024;
const ARTWORK_RESPONSES: usize = 2;
const ARTWORK_CHUNK_BYTES: usize = 16 * 1024;
const READ_TIMEOUT: Duration = Duration::from_secs(4);
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);
const JOB_TIMEOUT: Duration = Duration::from_secs(6);
const POLL_INTERVAL: Duration = Duration::from_millis(100);
const HEARTBEAT: Duration = Duration::from_secs(15);

/// Only a digest is retained by the endpoint; Debug never exposes credentials.
#[derive(Clone)]
pub struct BearerDigest([u8; 32]);
impl std::fmt::Debug for BearerDigest {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("BearerDigest([redacted])")
    }
}
impl BearerDigest {
    pub fn from_token(token: &str) -> ControlResult<Self> {
        if token.len() != 64 || !token.bytes().all(|value| value.is_ascii_hexdigit()) {
            return Err(invalid("Bearer token must encode 256 random bits"));
        }
        Ok(Self(Sha256::digest(token.as_bytes()).into()))
    }
    pub fn from_sha256(digest: [u8; 32]) -> Self {
        Self(digest)
    }
    pub fn generate() -> ControlResult<(String, Self)> {
        let mut random = [0u8; 32];
        getrandom::fill(&mut random)
            .map_err(|_| unavailable("Cannot generate endpoint credential"))?;
        let token: String = random.iter().map(|value| format!("{value:02x}")).collect();
        let digest = Self::from_token(&token)?;
        Ok((token, digest))
    }
    fn matches(&self, token: &str) -> bool {
        if token.len() != 64 || !token.bytes().all(|value| value.is_ascii_hexdigit()) {
            return false;
        }
        let candidate: [u8; 32] = Sha256::digest(token.as_bytes()).into();
        self.0
            .iter()
            .zip(candidate)
            .fold(0u8, |difference, (left, right)| {
                difference | (*left ^ right)
            })
            == 0
    }
}
#[derive(Clone, Debug, Default)]
pub enum Authentication {
    #[default]
    LegacyLoopback,
    Bearer(BearerDigest),
}
#[derive(Clone, Debug)]
pub struct ServerConfig {
    pub port: u16,
    pub bind_lan: bool,
    pub authentication: Authentication,
    pub allowed_origins: Vec<String>,
    /// Literal local interface IPs, without a port, explicitly enabled for LAN.
    pub allowed_lan_hosts: Vec<IpAddr>,
    pub session_epoch: u64,
}
impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            port: 23330,
            bind_lan: false,
            authentication: Authentication::LegacyLoopback,
            allowed_origins: Vec::new(),
            allowed_lan_hosts: Vec::new(),
            session_epoch: 1,
        }
    }
}
impl ServerConfig {
    pub fn validate(&self) -> ControlResult<()> {
        if self.session_epoch == 0
            || self.allowed_origins.len() > 16
            || self.allowed_lan_hosts.len() > 16
        {
            return Err(invalid("Invalid endpoint configuration"));
        }
        if self.bind_lan
            && (!matches!(self.authentication, Authentication::Bearer(_))
                || self.allowed_lan_hosts.is_empty())
        {
            return Err(invalid(
                "LAN requires a bearer credential and explicit local hosts",
            ));
        }
        if self.allowed_lan_hosts.iter().any(|host| {
            host.is_unspecified()
                || host.is_multicast()
                || matches!(host, IpAddr::V4(host) if host.is_broadcast())
        }) {
            return Err(invalid("Invalid LAN host"));
        }
        for origin in &self.allowed_origins {
            let url = reqwest::Url::parse(origin).map_err(|_| invalid("Invalid allowed Origin"))?;
            if origin.len() > 2048
                || !["http", "https"].contains(&url.scheme())
                || !url.username().is_empty()
                || url.password().is_some()
                || url.host_str().is_none()
                || url.path() != "/"
                || url.query().is_some()
                || url.fragment().is_some()
                || url.origin().ascii_serialization() != *origin
            {
                return Err(invalid("Allowed Origin must be an exact HTTP origin"));
            }
        }
        Ok(())
    }
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ServerState {
    #[default]
    Disabled,
    Starting,
    Listening,
    Stopping,
    Failed,
}
#[derive(Clone, Debug)]
pub struct ServerView {
    pub state: ServerState,
    pub address: Option<SocketAddr>,
    pub session_epoch: u64,
    pub connections: usize,
    pub subscriptions: usize,
    pub last_error: Option<ControlError>,
}
#[derive(Clone, Debug)]
pub enum ServerEvent {
    Listening {
        address: SocketAddr,
        session_epoch: u64,
    },
    Failed {
        error: ControlError,
        session_epoch: u64,
    },
    Stopped {
        session_epoch: u64,
    },
}
struct Shared {
    closing: AtomicBool,
    owner_finished: AtomicBool,
    view: Mutex<ServerView>,
    artwork_slots: Arc<AtomicUsize>,
}
/// Construction is the explicit enable operation. No listener exists by default.
pub struct OpenApiServer {
    shared: Arc<Shared>,
    events: Receiver<ServerEvent>,
    send_events: SyncSender<ServerEvent>,
    workers: Vec<TrackedWorker>,
}
/// A profile can retain cancellation/join ownership without retaining a backend
/// adapter, listener, or public server. TrackedWorker clones share actual joins.
#[derive(Clone)]
pub(crate) struct OpenApiShutdownToken {
    shared: Arc<Shared>,
    workers: Vec<TrackedWorker>,
    events: SyncSender<ServerEvent>,
}
impl OpenApiShutdownToken {
    pub(crate) fn begin_shutdown(&self) {
        self.shared.closing.store(true, Ordering::Release);
        {
            let mut view = self
                .shared
                .view
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            if !matches!(view.state, ServerState::Disabled | ServerState::Failed) {
                view.state = ServerState::Stopping;
            }
        }
        for worker in &self.workers {
            worker.unpark();
        }
    }
    pub(crate) fn wait(&self, timeout: Duration) -> ControlResult<bool> {
        let start = Instant::now();
        for worker in &self.workers {
            if !worker.wait(timeout.saturating_sub(start.elapsed())) {
                return Ok(false);
            }
        }
        let mut view = self
            .shared
            .view
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if view.connections != 0
            || view.subscriptions != 0
            || !self.shared.owner_finished.load(Ordering::Acquire)
        {
            return Ok(false);
        }
        let first_join = view.state != ServerState::Disabled;
        view.state = ServerState::Disabled;
        view.address = None;
        if self.workers.iter().any(TrackedWorker::panicked) {
            return Err(unavailable("Control worker panicked"));
        }
        if first_join {
            let _ = self.events.try_send(ServerEvent::Stopped {
                session_epoch: view.session_epoch,
            });
        }
        Ok(true)
    }
}
impl OpenApiServer {
    pub fn start(
        config: ServerConfig,
        backend: Arc<dyn ExternalControlBackend>,
    ) -> ControlResult<Self> {
        config.validate()?;
        // Reserve all actual threads before binding/spawning any of them.
        let mut permits = Vec::with_capacity(3);
        for _ in 0..3 {
            permits.push(
                WorkerPermit::reserve(WorkerKind::ExternalControl)
                    .map_err(|_| unavailable("External control worker capacity exhausted"))?,
            );
        }
        let bind = if config.bind_lan {
            Ipv4Addr::UNSPECIFIED
        } else {
            Ipv4Addr::LOCALHOST
        };
        let listener = TcpListener::bind((bind, config.port))
            .map_err(|_| unavailable("Cannot bind control endpoint"))?;
        listener
            .set_nonblocking(true)
            .map_err(|_| unavailable("Cannot configure control listener"))?;
        let address = listener
            .local_addr()
            .map_err(|_| unavailable("Cannot inspect control listener"))?;
        let shared = Arc::new(Shared {
            closing: AtomicBool::new(false),
            owner_finished: AtomicBool::new(false),
            artwork_slots: Arc::new(AtomicUsize::new(0)),
            view: Mutex::new(ServerView {
                state: ServerState::Starting,
                address: Some(address),
                session_epoch: config.session_epoch,
                connections: 0,
                subscriptions: 0,
                last_error: None,
            }),
        });
        let (jobs, receive_jobs) = mpsc::sync_channel(JOBS);
        let receive_jobs = Arc::new(Mutex::new(receive_jobs));
        let (complete, completions) = mpsc::sync_channel(JOBS);
        let (send_events, events) = mpsc::sync_channel(8);
        let publish_events = send_events.clone();
        let mut workers = Vec::with_capacity(3);
        for index in 0..2 {
            let permit = permits.pop().unwrap();
            let receive = receive_jobs.clone();
            let send = complete.clone();
            let owned = shared.clone();
            let adapter = backend.clone();
            let epoch = config.session_epoch;
            match std::thread::Builder::new()
                .name(format!("lx-control-adapter-{index}"))
                .spawn(move || adapter_loop(receive, send, owned, adapter, epoch))
            {
                Ok(thread) => workers.push(permit.attach(thread)),
                Err(_) => {
                    shared.closing.store(true, Ordering::Release);
                    return Err(unavailable("Cannot start control adapter"));
                }
            }
        }
        drop(complete);
        let permit = permits.pop().unwrap();
        let owned = shared.clone();
        let final_state = shared.clone();
        let thread = std::thread::Builder::new()
            .name("lx-control-sockets".into())
            .spawn(move || {
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    owner_loop(
                        listener,
                        config,
                        address,
                        jobs,
                        completions,
                        owned.clone(),
                        &send_events,
                    )
                }));
                owned.closing.store(true, Ordering::Release);
                if outcome.is_err() {
                    // Unwinding has already dropped the owned sockets/listener.
                    {
                        let mut view = owned.view.lock().unwrap_or_else(|error| error.into_inner());
                        view.connections = 0;
                        view.subscriptions = 0;
                    }
                    fail(
                        &owned,
                        &send_events,
                        unavailable("Control socket owner panicked"),
                    );
                }
                owned.owner_finished.store(true, Ordering::Release);
            })
            .map_err(|_| {
                final_state.closing.store(true, Ordering::Release);
                unavailable("Cannot start control socket owner")
            })?;
        workers.push(permit.attach(thread));
        Ok(Self {
            shared,
            events,
            send_events: publish_events,
            workers,
        })
    }
    pub fn view(&self) -> ServerView {
        self.shared
            .view
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }
    pub fn try_recv(&mut self) -> Option<ServerEvent> {
        self.events.try_recv().ok()
    }
    pub fn is_finished(&self) -> bool {
        self.shared.owner_finished.load(Ordering::Acquire)
            && self.workers.iter().all(TrackedWorker::is_finished)
    }
    pub(crate) fn shutdown_token(&self) -> OpenApiShutdownToken {
        OpenApiShutdownToken {
            shared: self.shared.clone(),
            workers: self.workers.clone(),
            events: self.send_events.clone(),
        }
    }
    pub fn shutdown(&mut self, timeout: Duration) -> ControlResult<bool> {
        let token = self.shutdown_token();
        token.begin_shutdown();
        token.wait(timeout)
    }
}
impl Drop for OpenApiServer {
    fn drop(&mut self) {
        self.shared.closing.store(true, Ordering::Release);
        for worker in &self.workers {
            worker.unpark();
        }
    }
}

enum JobKind {
    Initial(Route),
    Poll {
        cursor: u64,
        filter: Filter,
        previous: Arc<LxPlayerStatus>,
    },
}
struct Job {
    id: u64,
    kind: JobKind,
    deadline: Instant,
}
enum Completed {
    Response {
        code: u16,
        body: Vec<u8>,
        content_type: &'static str,
    },
    Artwork {
        response: ArtworkResponse,
        permit: ArtworkPermit,
    },
    Stream {
        cursor: u64,
        status: Arc<LxPlayerStatus>,
        frames: Vec<(String, Vec<u8>)>,
    },
    NoUpdate,
}
struct Completion {
    id: u64,
    result: ControlResult<Completed>,
}
struct ArtworkPermit(Arc<AtomicUsize>);
impl ArtworkPermit {
    fn reserve(slots: &Arc<AtomicUsize>) -> ControlResult<Self> {
        slots
            .try_update(Ordering::AcqRel, Ordering::Acquire, |active| {
                (active < ARTWORK_RESPONSES).then_some(active + 1)
            })
            .map_err(|_| {
                ControlError::new(
                    ControlErrorKind::Busy,
                    "Artwork response capacity exhausted",
                )
            })?;
        Ok(Self(slots.clone()))
    }
}
impl Drop for ArtworkPermit {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}
fn adapter_loop(
    receive: Arc<Mutex<Receiver<Job>>>,
    send: SyncSender<Completion>,
    shared: Arc<Shared>,
    backend: Arc<dyn ExternalControlBackend>,
    epoch: u64,
) {
    while !shared.closing.load(Ordering::Acquire) {
        let job = {
            receive
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .recv_timeout(Duration::from_millis(20))
        };
        let Ok(job) = job else {
            continue;
        };
        if shared.closing.load(Ordering::Acquire) {
            break;
        }
        let result = if Instant::now() > job.deadline {
            Err(ControlError::new(
                ControlErrorKind::Timeout,
                "Control request timed out",
            ))
        } else {
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                perform(&*backend, epoch, job.kind, &shared.artwork_slots)
            }))
            .unwrap_or_else(|_| Err(unavailable("Control adapter panicked")))
        };
        let mut completion = Completion { id: job.id, result };
        loop {
            if shared.closing.load(Ordering::Acquire) {
                return;
            }
            match send.try_send(completion) {
                Ok(()) => break,
                Err(TrySendError::Full(returned)) => {
                    completion = returned;
                    std::thread::park_timeout(Duration::from_millis(2));
                }
                Err(TrySendError::Disconnected(_)) => return,
            }
        }
    }
}
fn perform(
    backend: &dyn ExternalControlBackend,
    epoch: u64,
    kind: JobKind,
    artwork_slots: &Arc<AtomicUsize>,
) -> ControlResult<Completed> {
    match kind {
        JobKind::Initial(Route::Artwork(reference)) => {
            reference.validate()?;
            if reference.session_epoch != epoch {
                return Err(ControlError::new(
                    ControlErrorKind::Unauthorized,
                    "External artwork epoch changed",
                ));
            }
            let permit = ArtworkPermit::reserve(artwork_slots)?;
            let response = backend.artwork(reference.clone())?;
            if response.retired.load(Ordering::Acquire) {
                return Err(ControlError::new(
                    ControlErrorKind::Unavailable,
                    "Current verified artwork retired",
                ));
            }
            if response.bytes.is_empty()
                || response.bytes.len() > crate::domain::artwork::MAX_ARTWORK_BYTES
                || !["image/jpeg", "image/png", "image/gif", "image/webp"]
                    .contains(&response.mime_type)
                || format!("{:x}", Sha256::digest(&response.bytes)) != reference.sha256
            {
                return Err(unavailable("Invalid verified artwork response"));
            }
            Ok(Completed::Artwork { response, permit })
        }
        JobKind::Initial(Route::Control(action)) => {
            backend.control(epoch, action)?;
            Ok(Completed::Response {
                code: 200,
                body: b"OK".to_vec(),
                content_type: "text/plain; charset=utf-8",
            })
        }
        JobKind::Initial(route) => {
            let snapshot = backend.snapshot()?;
            status_value(&snapshot.status)?;
            match route {
                Route::Status(filter) => Ok(Completed::Response {
                    code: 200,
                    body: filter.project(&snapshot.status)?,
                    content_type: "application/json; charset=utf-8",
                }),
                Route::Lyric => {
                    if snapshot.status.lyric.len() > MAX_STATUS_BYTES {
                        return Err(super::protocol::too_large());
                    }
                    Ok(Completed::Response {
                        code: 200,
                        body: snapshot.status.lyric.into_bytes(),
                        content_type: "text/plain; charset=utf-8",
                    })
                }
                Route::LyricAll => Ok(Completed::Response {
                    code: 200,
                    body: lyrics(&snapshot.status)?,
                    content_type: "application/json; charset=utf-8",
                }),
                Route::Subscribe(filter) => {
                    let frames = filter.frames(&snapshot.status, None)?;
                    if frames.iter().map(|(_, frame)| frame.len()).sum::<usize>()
                        > FRAME_BYTES - 2048
                    {
                        return Err(super::protocol::too_large());
                    }
                    Ok(Completed::Stream {
                        cursor: snapshot.cursor,
                        status: Arc::new(snapshot.status),
                        frames,
                    })
                }
                _ => Err(invalid("Unknown control route")),
            }
        }
        JobKind::Poll {
            cursor,
            filter,
            previous,
        } => {
            let update = match backend.updates_after(cursor) {
                Ok(updates) => {
                    if updates.len() > 256
                        || updates
                            .windows(2)
                            .any(|pair| pair[0].cursor >= pair[1].cursor)
                        || updates.first().is_some_and(|item| item.cursor <= cursor)
                    {
                        return Err(unavailable("Invalid control event cursor"));
                    }
                    updates
                        .into_iter()
                        .last()
                        .map(|update| (update.status, update.cursor, false))
                }
                Err(error) if error.kind == ControlErrorKind::CursorExpired => {
                    let snapshot = backend.snapshot()?;
                    Some((snapshot.status, snapshot.cursor, true))
                }
                Err(error) => return Err(error),
            };
            let Some((status, cursor, recovered)) = update else {
                return Ok(Completed::NoUpdate);
            };
            let frames = filter.frames(&status, (!recovered).then_some(&*previous))?;
            Ok(Completed::Stream {
                cursor,
                status: Arc::new(status),
                frames,
            })
        }
    }
}

struct Pending {
    id: u64,
    deadline: Instant,
    initial: bool,
}
struct StreamState {
    filter: Filter,
    status: Arc<LxPlayerStatus>,
    cursor: u64,
    next_poll: Instant,
    next_heartbeat: Instant,
}
struct Frame {
    field: Option<String>,
    bytes: Vec<u8>,
    offset: usize,
}
struct ArtworkTransfer {
    response: ArtworkResponse,
    offset: usize,
    _permit: ArtworkPermit,
}
struct Connection {
    socket: TcpStream,
    peer: SocketAddr,
    input: Vec<u8>,
    body_remaining: Option<usize>,
    request: Option<Request>,
    created: Instant,
    last_progress: Instant,
    frames: VecDeque<Frame>,
    queued_bytes: usize,
    pending: Option<Pending>,
    stream: Option<StreamState>,
    artwork: Option<ArtworkTransfer>,
    artwork_retired: Option<Arc<AtomicBool>>,
    subscription_reserved: bool,
    close_after_write: bool,
    peer_read_closed: bool,
    head_response: bool,
    origin: Option<String>,
}
impl Connection {
    fn new(socket: TcpStream, peer: SocketAddr) -> Self {
        let now = Instant::now();
        Self {
            socket,
            peer,
            input: Vec::with_capacity(2048),
            body_remaining: None,
            request: None,
            created: now,
            last_progress: now,
            frames: VecDeque::new(),
            queued_bytes: 0,
            pending: None,
            stream: None,
            artwork: None,
            artwork_retired: None,
            subscription_reserved: false,
            close_after_write: false,
            peer_read_closed: false,
            head_response: false,
            origin: None,
        }
    }
    fn queue(&mut self, field: Option<String>, bytes: Vec<u8>) -> bool {
        // Node HTTP suppresses response bodies for HEAD, including SSE writes.
        if self.head_response && !bytes.starts_with(b"HTTP/1.1 ") {
            return true;
        }
        if let Some(name) = field.as_ref()
            && let Some(frame) = self
                .frames
                .iter_mut()
                .rev()
                .find(|frame| frame.field.as_ref() == Some(name) && frame.offset == 0)
        {
            let total = self.queued_bytes - frame.bytes.len() + bytes.len();
            if total > FRAME_BYTES {
                return false;
            }
            self.queued_bytes = total;
            frame.bytes = bytes;
            return true;
        }
        if self.frames.len() >= FRAME_COUNT || self.queued_bytes + bytes.len() > FRAME_BYTES {
            return false;
        }
        if self.frames.is_empty() {
            self.last_progress = Instant::now();
        }
        self.queued_bytes += bytes.len();
        self.frames.push_back(Frame {
            field,
            bytes,
            offset: 0,
        });
        true
    }
    fn response(&mut self, code: u16, body: Vec<u8>, content_type: &str) -> bool {
        self.pending = None;
        self.close_after_write = true;
        self.artwork = None;
        self.artwork_retired = None;
        self.frames.clear();
        self.queued_bytes = 0;
        let header = response_header(code, content_type, Some(body.len()), self.origin.as_deref());
        // Headers are small; the body keeps a separate fixed 256 KiB projection budget.
        if body.len() > MAX_STATUS_BYTES {
            return false;
        }
        self.queued_bytes = header.len() + if self.head_response { 0 } else { body.len() };
        self.frames.push_back(Frame {
            field: None,
            bytes: header,
            offset: 0,
        });
        if !body.is_empty() && !self.head_response {
            self.frames.push_back(Frame {
                field: None,
                bytes: body,
                offset: 0,
            });
        }
        self.last_progress = Instant::now();
        true
    }
    fn artwork_response(&mut self, response: ArtworkResponse, permit: ArtworkPermit) -> bool {
        self.pending = None;
        self.close_after_write = true;
        self.frames.clear();
        self.queued_bytes = 0;
        if response.retired.load(Ordering::Acquire) {
            return false;
        }
        self.artwork_retired = Some(response.retired.clone());
        let header = response_header(
            200,
            response.mime_type,
            Some(response.bytes.len()),
            self.origin.as_deref(),
        );
        if !self.queue(None, header) {
            return false;
        }
        if !self.head_response {
            self.artwork = Some(ArtworkTransfer {
                response,
                offset: 0,
                _permit: permit,
            });
        }
        true
    }
}
impl Drop for Connection {
    fn drop(&mut self) {
        let _ = self.socket.shutdown(Shutdown::Both);
    }
}
fn owner_loop(
    listener: TcpListener,
    config: ServerConfig,
    address: SocketAddr,
    jobs: SyncSender<Job>,
    completions: Receiver<Completion>,
    shared: Arc<Shared>,
    events: &SyncSender<ServerEvent>,
) {
    {
        shared
            .view
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .state = ServerState::Listening;
    }
    let _ = events.try_send(ServerEvent::Listening {
        address,
        session_epoch: config.session_epoch,
    });
    let mut connections: Vec<Option<Connection>> = (0..CONNECTIONS).map(|_| None).collect();
    let mut next_id = 1u64;
    while !shared.closing.load(Ordering::Acquire) {
        for _ in 0..CONNECTIONS {
            let Some(slot) = connections.iter_mut().find(|slot| slot.is_none()) else {
                break;
            };
            match listener.accept() {
                Ok((socket, peer)) => {
                    if socket.set_nonblocking(true).is_ok() {
                        let _ = socket.set_nodelay(true);
                        *slot = Some(Connection::new(socket, peer));
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(_) => {
                    fail(&shared, events, unavailable("Control listener failed"));
                    shared.closing.store(true, Ordering::Release);
                    break;
                }
            }
        }
        for _ in 0..JOBS {
            let Ok(completion) = completions.try_recv() else {
                break;
            };
            let Some(connection) = connections.iter_mut().flatten().find(|connection| {
                connection
                    .pending
                    .as_ref()
                    .is_some_and(|pending| pending.id == completion.id)
            }) else {
                continue;
            };
            if shared.closing.load(Ordering::Acquire) {
                break;
            }
            let pending = connection.pending.take().unwrap();
            if Instant::now() > pending.deadline {
                connection.close_after_write = true;
                connection.frames.clear();
                connection.queued_bytes = 0;
                continue;
            }
            match completion.result {
                Ok(Completed::Response {
                    code,
                    body,
                    content_type,
                }) => {
                    connection.response(code, body, content_type);
                }
                Ok(Completed::Artwork { response, permit }) => {
                    if !connection.artwork_response(response, permit) {
                        connection.frames.clear();
                        connection.queued_bytes = 0;
                    }
                }
                Ok(Completed::Stream {
                    cursor,
                    status,
                    frames,
                }) => {
                    if pending.initial {
                        let Some(Request {
                            route: Route::Subscribe(filter),
                            ..
                        }) = connection.request.take()
                        else {
                            connection.close_after_write = true;
                            continue;
                        };
                        let header = response_header(
                            200,
                            "text/event-stream",
                            None,
                            connection.origin.as_deref(),
                        );
                        if !connection.queue(None, header) {
                            connection.close_after_write = true;
                            continue;
                        }
                        connection.stream = Some(StreamState {
                            filter,
                            status: status.clone(),
                            cursor,
                            next_poll: Instant::now() + POLL_INTERVAL,
                            next_heartbeat: Instant::now() + HEARTBEAT,
                        });
                    } else if let Some(stream) = &mut connection.stream {
                        stream.cursor = cursor;
                        stream.status = status.clone();
                        stream.next_poll = Instant::now() + POLL_INTERVAL;
                    }
                    for (field, frame) in frames {
                        if !connection.queue(Some(field), frame) {
                            connection.close_after_write = true;
                            connection.frames.clear();
                            connection.queued_bytes = 0;
                            break;
                        }
                    }
                }
                Ok(Completed::NoUpdate) => {
                    if let Some(stream) = &mut connection.stream {
                        stream.next_poll = Instant::now() + POLL_INTERVAL;
                    }
                }
                Err(error) => {
                    if !pending.initial {
                        connection.close_after_write = true;
                        connection.frames.clear();
                        connection.queued_bytes = 0;
                    } else {
                        let code = error_code(&error);
                        connection.response(
                            code,
                            error.message.into_bytes(),
                            "text/plain; charset=utf-8",
                        );
                    }
                }
            }
        }
        let mut subscriptions = connections
            .iter()
            .flatten()
            .filter(|connection| connection.subscription_reserved)
            .count();
        for slot in &mut connections {
            let Some(connection) = slot else {
                continue;
            };
            let keep = step_connection(
                connection,
                &config,
                address,
                &jobs,
                &mut next_id,
                &mut subscriptions,
                &shared.closing,
            );
            if !keep {
                if connection.subscription_reserved {
                    subscriptions -= 1;
                }
                *slot = None;
            }
        }
        {
            let mut view = shared
                .view
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            view.connections = connections.iter().flatten().count();
            view.subscriptions = subscriptions;
        }
        std::thread::park_timeout(Duration::from_millis(2));
    }
    drop(connections);
    drop(listener);
    drop(jobs);
    {
        let mut view = shared
            .view
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        view.connections = 0;
        view.subscriptions = 0;
        if view.state != ServerState::Failed {
            view.state = ServerState::Stopping;
        }
    }
}
fn step_connection(
    connection: &mut Connection,
    config: &ServerConfig,
    address: SocketAddr,
    jobs: &SyncSender<Job>,
    next_id: &mut u64,
    subscriptions: &mut usize,
    closing: &AtomicBool,
) -> bool {
    if closing.load(Ordering::Acquire) {
        return false;
    }
    let now = Instant::now();
    if connection
        .pending
        .as_ref()
        .is_some_and(|pending| now >= pending.deadline)
    {
        return false;
    }
    if connection.stream.is_none()
        && !connection.close_after_write
        && connection.pending.is_none()
        && now.duration_since(connection.created) >= READ_TIMEOUT
    {
        return false;
    }
    if (!connection.frames.is_empty() || connection.artwork.is_some())
        && now.duration_since(connection.last_progress) >= WRITE_TIMEOUT
    {
        return false;
    }
    // Each connection has one request. Pipelining and request bodies never
    // accumulate another backend job. Streams still read to detect real FIN.
    for _ in 0..4 {
        if connection.peer_read_closed {
            break;
        }
        let mut bytes = [0u8; 4096];
        match connection.socket.read(&mut bytes) {
            Ok(0) => {
                if connection.stream.is_some()
                    || connection.subscription_reserved
                    || connection.body_remaining != Some(0)
                {
                    return false;
                }
                connection.peer_read_closed = true;
                break;
            }
            Ok(count) => {
                if connection.stream.is_some()
                    || connection.pending.is_some()
                    || connection.close_after_write
                {
                    return false;
                }
                connection.last_progress = now;
                if let Some(remaining) = &mut connection.body_remaining {
                    if count > *remaining {
                        return false;
                    }
                    *remaining -= count;
                } else {
                    if connection.input.len() + count > MAX_HEADER + 4096 {
                        connection.response(
                            431,
                            b"Request headers too large".to_vec(),
                            "text/plain; charset=utf-8",
                        );
                        break;
                    }
                    connection.input.extend_from_slice(&bytes[..count]);
                    if let Some(end) = connection
                        .input
                        .windows(4)
                        .position(|part| part == b"\r\n\r\n")
                    {
                        let header_end = end + 4;
                        if header_end > MAX_HEADER {
                            connection.response(
                                431,
                                b"Request headers too large".to_vec(),
                                "text/plain; charset=utf-8",
                            );
                            break;
                        }
                        match parse_request(&connection.input[..header_end]) {
                            Ok(mut request) => {
                                let consumed = connection.input.len() - header_end;
                                if consumed > request.body {
                                    return false;
                                }
                                connection.head_response = request.head;
                                match authenticate(&request, connection.peer, config, address) {
                                    Ok(origin) => connection.origin = origin,
                                    Err(()) => {
                                        connection.response(
                                            401,
                                            b"Unauthorized".to_vec(),
                                            "text/plain; charset=utf-8",
                                        );
                                        break;
                                    }
                                }
                                request.route = match parse_route(&request.target) {
                                    Ok(route) => route,
                                    Err(error) => {
                                        connection.response(
                                            400,
                                            error.message.into_bytes(),
                                            "text/plain; charset=utf-8",
                                        );
                                        break;
                                    }
                                };
                                if matches!(request.route, Route::Artwork(_))
                                    && (!request.get_or_head || request.body != 0)
                                {
                                    connection.response(
                                        400,
                                        b"Artwork requires GET or HEAD without a body".to_vec(),
                                        "text/plain; charset=utf-8",
                                    );
                                    break;
                                }
                                connection.body_remaining = Some(request.body - consumed);
                                connection.request = Some(request);
                                connection.input.clear();
                            }
                            Err(error) => {
                                connection.response(
                                    400,
                                    error.message.into_bytes(),
                                    "text/plain; charset=utf-8",
                                );
                                break;
                            }
                        }
                    }
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
            Err(_) => return false,
        }
    }
    if connection.pending.is_none()
        && connection.stream.is_none()
        && !connection.close_after_write
        && connection.body_remaining == Some(0)
    {
        let Some(request) = connection.request.as_ref() else {
            return false;
        };
        if request.route == Route::Unknown {
            return connection.response(401, b"Forbidden".to_vec(), "text/plain; charset=utf-8");
        }
        if matches!(request.route, Route::Subscribe(_)) {
            if *subscriptions >= SUBSCRIPTIONS {
                return connection.response(
                    503,
                    b"Subscription capacity exhausted".to_vec(),
                    "text/plain; charset=utf-8",
                );
            }
            *subscriptions += 1;
            connection.subscription_reserved = true;
        }
        let kind = JobKind::Initial(request.route.clone());
        if closing.load(Ordering::Acquire) {
            return false;
        }
        if !enqueue(connection, jobs, next_id, kind, true) {
            connection.response(
                503,
                b"Control capacity exhausted".to_vec(),
                "text/plain; charset=utf-8",
            );
        }
    }
    if let Some(stream) = &mut connection.stream {
        if now >= stream.next_heartbeat {
            stream.next_heartbeat = now + HEARTBEAT;
            if !connection.queue(None, b": keep-alive\n\n".to_vec()) {
                return false;
            }
        }
        if connection.pending.is_none()
            && let Some(stream) = &connection.stream
            && now >= stream.next_poll
        {
            let kind = JobKind::Poll {
                cursor: stream.cursor,
                filter: stream.filter.clone(),
                previous: stream.status.clone(),
            };
            if closing.load(Ordering::Acquire) {
                return false;
            }
            if !enqueue(connection, jobs, next_id, kind, false) {
                return false;
            }
        }
    }
    for _ in 0..64 {
        if closing.load(Ordering::Acquire) {
            return false;
        }
        if connection
            .artwork_retired
            .as_ref()
            .is_some_and(|retired| retired.load(Ordering::Acquire))
        {
            return false;
        }
        let Some(frame) = connection.frames.front_mut() else {
            break;
        };
        match connection.socket.write(&frame.bytes[frame.offset..]) {
            Ok(0) => return false,
            Ok(count) => {
                frame.offset += count;
                connection.queued_bytes -= count;
                connection.last_progress = Instant::now();
                if frame.offset == frame.bytes.len() {
                    connection.frames.pop_front();
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
            Err(_) => return false,
        }
    }
    if connection.frames.is_empty()
        && let Some(transfer) = &mut connection.artwork
    {
        for _ in 0..4 {
            if closing.load(Ordering::Acquire) || transfer.response.retired.load(Ordering::Acquire)
            {
                return false;
            }
            let end = (transfer.offset + ARTWORK_CHUNK_BYTES).min(transfer.response.bytes.len());
            match connection
                .socket
                .write(&transfer.response.bytes[transfer.offset..end])
            {
                Ok(0) => return false,
                Ok(count) => {
                    transfer.offset += count;
                    connection.last_progress = Instant::now();
                    if transfer.offset == transfer.response.bytes.len() {
                        break;
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(_) => return false,
            }
        }
        if transfer.offset == transfer.response.bytes.len() {
            connection.artwork = None;
        }
    }
    !(connection.close_after_write && connection.frames.is_empty() && connection.artwork.is_none())
}
fn enqueue(
    connection: &mut Connection,
    jobs: &SyncSender<Job>,
    next_id: &mut u64,
    kind: JobKind,
    initial: bool,
) -> bool {
    let id = *next_id;
    let Some(next) = id.checked_add(1) else {
        return false;
    };
    *next_id = next;
    let deadline = Instant::now() + JOB_TIMEOUT;
    if jobs.try_send(Job { id, kind, deadline }).is_err() {
        return false;
    }
    connection.pending = Some(Pending {
        id,
        deadline,
        initial,
    });
    true
}

struct Request {
    target: String,
    head: bool,
    get_or_head: bool,
    route: Route,
    headers: Vec<(String, String)>,
    body: usize,
}
fn parse_request(bytes: &[u8]) -> ControlResult<Request> {
    let value = std::str::from_utf8(bytes).map_err(|_| invalid("Invalid request headers"))?;
    let mut lines = value.split("\r\n");
    let line = lines.next().unwrap_or_default();
    let mut pieces = line.split(' ');
    let method = pieces.next().unwrap_or_default();
    let target = pieces.next().unwrap_or_default();
    let version = pieces.next().unwrap_or_default();
    if method.is_empty()
        || !method.bytes().all(token_char)
        || !["HTTP/1.0", "HTTP/1.1"].contains(&version)
        || pieces.next().is_some()
    {
        return Err(invalid("Invalid request line"));
    }
    let mut headers: Vec<(String, String)> = Vec::new();
    for line in lines {
        if line.is_empty() {
            break;
        }
        if headers.len() >= 64 {
            return Err(invalid("Too many headers"));
        }
        let (key, value) = line
            .split_once(':')
            .ok_or_else(|| invalid("Invalid request header"))?;
        if key.is_empty()
            || !key.bytes().all(token_char)
            || value
                .bytes()
                .any(|byte| byte < 32 && byte != 9 || byte == 127)
        {
            return Err(invalid("Invalid request header"));
        }
        headers.push((
            key.to_ascii_lowercase(),
            value.trim_matches([' ', '\t']).into(),
        ));
    }
    let single = |key: &str| -> ControlResult<Option<&str>> {
        let mut values = headers
            .iter()
            .filter(|(name, _)| name == key)
            .map(|(_, value)| value.as_str());
        let first = values.next();
        if values.next().is_some() {
            return Err(invalid("Duplicate security header"));
        }
        Ok(first)
    };
    for key in [
        "host",
        "origin",
        "authorization",
        "user-agent",
        "referer",
        "sec-fetch-site",
        "sec-fetch-mode",
        "sec-fetch-dest",
        "content-length",
        "transfer-encoding",
    ] {
        single(key)?;
    }
    if single("transfer-encoding")?.is_some() {
        return Err(invalid("Transfer encoding is unsupported"));
    }
    let body = single("content-length")?
        .map(|value| {
            if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
                return Err(invalid("Invalid content length"));
            }
            value
                .parse::<usize>()
                .map_err(|_| invalid("Invalid content length"))
        })
        .transpose()?
        .unwrap_or(0);
    if body > MAX_BODY {
        return Err(invalid("Request body too large"));
    }
    if target.len() > 8192 {
        return Err(invalid("Request target too large"));
    }
    Ok(Request {
        target: target.into(),
        head: method == "HEAD",
        get_or_head: method == "GET" || method == "HEAD",
        route: Route::Unknown,
        headers,
        body,
    })
}
fn token_char(value: u8) -> bool {
    value.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&value)
}
fn authenticate(
    request: &Request,
    peer: SocketAddr,
    config: &ServerConfig,
    address: SocketAddr,
) -> Result<Option<String>, ()> {
    let header = |name: &str| {
        request
            .headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    };
    let host = header("host").ok_or(())?;
    let (hostname, port) = if host.starts_with('[') {
        let end = host.find(']').ok_or(())?;
        let hostname = &host[1..end];
        let suffix = &host[end + 1..];
        let port = if suffix.is_empty() {
            80
        } else {
            suffix
                .strip_prefix(':')
                .ok_or(())?
                .parse::<u16>()
                .map_err(|_| ())?
        };
        (hostname, port)
    } else {
        let (hostname, port) = host
            .rsplit_once(':')
            .map(|(name, port)| Ok((name, port.parse::<u16>().map_err(|_| ())?)))
            .unwrap_or(Ok((host, 80)))?;
        (hostname, port)
    };
    if port != address.port() {
        return Err(());
    }
    let ip = hostname.parse::<IpAddr>().ok();
    let loopback_host =
        hostname.eq_ignore_ascii_case("localhost") || ip.is_some_and(|ip| ip.is_loopback());
    if !loopback_host
        && !(config.bind_lan && ip.is_some_and(|ip| config.allowed_lan_hosts.contains(&ip)))
    {
        return Err(());
    }
    let origin = header("origin");
    if origin.is_some_and(|value| {
        !config
            .allowed_origins
            .iter()
            .any(|allowed| allowed == value)
    }) {
        return Err(());
    }
    let referer = header("referer");
    let approved_referer = if let Some(referer) = referer {
        let url = reqwest::Url::parse(referer).map_err(|_| ())?;
        if !["http", "https"].contains(&url.scheme())
            || !url.username().is_empty()
            || url.password().is_some()
            || url.host_str().is_none()
        {
            return Err(());
        }
        let ref_host = url.host_str().unwrap().trim_matches(['[', ']']);
        let same_endpoint = url.scheme() == "http"
            && url.port_or_known_default() == Some(port)
            && ref_host.eq_ignore_ascii_case(hostname);
        let explicitly_allowed = config
            .allowed_origins
            .iter()
            .any(|allowed| *allowed == url.origin().ascii_serialization());
        if !same_endpoint && !explicitly_allowed {
            return Err(());
        }
        true
    } else {
        false
    };
    // Image and top-level navigation requests can omit Origin. Browser-generated
    // Fetch Metadata and Referer still fence cross-site requests; headers remain
    // request metadata, never credentials or source/network permissions.
    let site = header("sec-fetch-site");
    if let Some(site) = site {
        if !["none", "same-origin", "same-site", "cross-site"].contains(&site) {
            return Err(());
        }
        if origin.is_none() && !approved_referer && !["none", "same-origin"].contains(&site) {
            return Err(());
        }
    } else if origin.is_none()
        && !approved_referer
        && (header("sec-fetch-mode").is_some() || header("sec-fetch-dest").is_some())
    {
        return Err(());
    }
    if matches!(config.authentication, Authentication::LegacyLoopback)
        && origin.is_none()
        && referer.is_none()
        && site.is_none()
        && let Some(agent) = header("user-agent")
    {
        let agent = agent.to_ascii_lowercase();
        if [
            "mozilla/",
            "applewebkit/",
            "chrome/",
            "chromium/",
            "firefox/",
            "safari/",
            "edg/",
            "opera/",
            "opr/",
        ]
        .iter()
        .any(|browser| agent.contains(browser))
        {
            return Err(());
        }
    }
    match &config.authentication {
        Authentication::LegacyLoopback if !config.bind_lan && peer.ip().is_loopback() => {}
        Authentication::Bearer(digest) => {
            if !header("authorization")
                .and_then(|value| value.strip_prefix("Bearer "))
                .is_some_and(|token| digest.matches(token))
            {
                return Err(());
            }
        }
        _ => return Err(()),
    }
    Ok(origin.map(str::to_owned))
}
fn response_header(
    code: u16,
    content_type: &str,
    length: Option<usize>,
    origin: Option<&str>,
) -> Vec<u8> {
    let reason = match code {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        413 => "Content Too Large",
        431 => "Request Header Fields Too Large",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        _ => "Error",
    };
    let mut response = format!("HTTP/1.1 {code} {reason}\r\nContent-Type: {content_type}\r\n");
    response.push_str("X-Content-Type-Options: nosniff\r\n");
    if content_type.starts_with("image/") {
        response.push_str("Cache-Control: no-store\r\n");
    }
    if let Some(length) = length {
        response.push_str(&format!(
            "Content-Length: {length}\r\nConnection: close\r\n"
        ));
    } else {
        response.push_str("Connection: keep-alive\r\nCache-Control: no-cache\r\n");
    }
    if let Some(origin) = origin {
        response.push_str(&format!(
            "Access-Control-Allow-Origin: {origin}\r\nVary: Origin\r\n"
        ));
    }
    response.push_str("\r\n");
    response.into_bytes()
}
fn error_code(error: &ControlError) -> u16 {
    match error.kind {
        ControlErrorKind::InvalidData if error.message == "Response exceeds resource budget" => 413,
        ControlErrorKind::InvalidData => 400,
        ControlErrorKind::Unauthorized => 401,
        ControlErrorKind::Timeout => 504,
        ControlErrorKind::Busy | ControlErrorKind::Closed | ControlErrorKind::Unavailable => 503,
        ControlErrorKind::CursorExpired => 500,
    }
}
fn fail(shared: &Shared, events: &SyncSender<ServerEvent>, error: ControlError) {
    let mut view = shared
        .view
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    view.state = ServerState::Failed;
    view.last_error = Some(error.clone());
    let _ = events.try_send(ServerEvent::Failed {
        error,
        session_epoch: view.session_epoch,
    });
}
fn unavailable(message: &str) -> ControlError {
    ControlError::new(ControlErrorKind::Unavailable, message)
}

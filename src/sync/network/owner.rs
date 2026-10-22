use super::super::{PeerCapabilities, SyncError, SyncErrorKind, SyncResult};
use super::backend::*;
use super::memory::{Budget, Credit};
use crate::audio::worker_owner::{TrackedWorker, WorkerKind, WorkerPermit};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use std::net::{Ipv4Addr, SocketAddr, TcpListener};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    mpsc::{self, Receiver, SyncSender},
};
use std::time::{Duration, Instant};

#[derive(Clone)]
pub struct PairingCode(String);
impl std::fmt::Debug for PairingCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PairingCode([redacted])")
    }
}
impl PairingCode {
    pub fn new(code: &str) -> SyncResult<Self> {
        if code.len() != 6 || !code.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(error("Pairing code must contain six digits"));
        }
        Ok(Self(code.into()))
    }
    pub fn generate() -> SyncResult<Self> {
        let mut bytes = [0; 4];
        getrandom::fill(&mut bytes).map_err(|_| error("Cannot generate pairing code"))?;
        Ok(Self(format!(
            "{:06}",
            u32::from_le_bytes(bytes) % 1_000_000
        )))
    }
    pub fn expose(&self) -> &str {
        &self.0
    }
}
#[derive(Clone, Debug)]
pub struct ServerConfig {
    pub listen_address: SocketAddr,
    pub bind_lan: bool,
    pub server_id: String,
    pub server_name: String,
    pub pairing_code: PairingCode,
    pub pairing_epoch: u64,
    pub capabilities: PeerCapabilities,
    pub tls: Option<Arc<rustls::ServerConfig>>,
}
impl ServerConfig {
    pub fn loopback(
        server_id: String,
        server_name: String,
        pairing_code: PairingCode,
        pairing_epoch: u64,
    ) -> Self {
        Self {
            listen_address: SocketAddr::from((Ipv4Addr::LOCALHOST, 9527)),
            bind_lan: false,
            server_id,
            server_name,
            pairing_code,
            pairing_epoch,
            capabilities: PeerCapabilities {
                list: Some(false),
                dislike: Some(false),
            },
            tls: None,
        }
    }
    fn validate(&self) -> SyncResult<()> {
        if !self.bind_lan && !self.listen_address.ip().is_loopback() {
            return Err(error("LAN sync requires explicit enable"));
        }
        if self.server_id.is_empty()
            || self.server_id.len() > 256
            || self.server_name.is_empty()
            || self.server_name.len() > 64
            || self.server_name.contains(['\0', '\r', '\n'])
            || self.pairing_epoch == 0
        {
            return Err(error("Invalid sync server configuration"));
        }
        Ok(())
    }
}
#[derive(Clone)]
pub struct PeerEndpoint {
    pub url: String,
    pub pinned_address: SocketAddr,
    pub tls: Option<Arc<rustls::ClientConfig>>,
}
impl std::fmt::Debug for PeerEndpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PeerEndpoint")
            .field("pinned_address", &self.pinned_address)
            .finish()
    }
}
impl PeerEndpoint {
    pub(crate) fn base_url(&self) -> SyncResult<reqwest::Url> {
        let mut url =
            reqwest::Url::parse(&self.url).map_err(|_| error("Invalid sync peer origin"))?;
        if !["http", "https"].contains(&url.scheme())
            || !url.username().is_empty()
            || url.password().is_some()
            || url.host_str().is_none()
            || url.query().is_some()
            || url.fragment().is_some()
            || url.port_or_known_default() != Some(self.pinned_address.port())
        {
            return Err(error("Invalid sync peer origin"));
        }
        if url.scheme() == "https" && self.tls.is_none() {
            return Err(error(
                "HTTPS sync requires explicit verified TLS configuration",
            ));
        }
        if self.url.len() > 2048 {
            return Err(error("Sync peer origin too long"));
        }
        if url.path().ends_with('/') {
            let path = url.path().trim_end_matches('/').to_string();
            url.set_path(&path);
        }
        Ok(url)
    }
}
#[derive(Clone, Debug)]
pub struct ClientConfig {
    pub endpoint: PeerEndpoint,
    pub device_name: String,
    pub credentials: Option<ClientCredentials>,
    pub pairing_code: Option<PairingCode>,
    pub pairing_epoch: u64,
    pub capabilities: PeerCapabilities,
    pub reconnect_limit: u32,
}
impl ClientConfig {
    fn validate(&self) -> SyncResult<()> {
        self.endpoint.base_url()?;
        if self.device_name.is_empty()
            || self.device_name.len() > 256
            || self.device_name.contains(['\0', '\r', '\n'])
            || self.pairing_epoch == 0
            || self.reconnect_limit > 20
            || self.credentials.is_none() && self.pairing_code.is_none()
        {
            return Err(error("Invalid sync client configuration"));
        }
        Ok(())
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NetworkState {
    Starting,
    Listening,
    Authenticating,
    Syncing,
    Synced,
    Degraded,
    Retiring,
    Disabled,
}
#[derive(Clone, Debug)]
pub struct NetworkView {
    pub state: NetworkState,
    pub address: Option<SocketAddr>,
    pub connections: usize,
    pub peers: usize,
    pub pending_rpc: usize,
    pub reserved_frame_bytes: usize,
    pub workspace_bytes: usize,
    pub last_error: Option<SyncError>,
}
#[derive(Clone, Debug)]
pub enum NetworkEvent {
    Listening(SocketAddr),
    Authenticated { peer_id: String },
    Synced { peer_id: String },
    PeerClosed { peer_id: String },
    Error(SyncError),
    Stopped,
}
pub(super) enum Mode {
    Server {
        listener: TcpListener,
        config: ServerConfig,
    },
    Client(ClientConfig),
}
pub(super) enum Command {
    Revoke(String),
}
pub(super) struct Shared {
    #[cfg(debug_assertions)]
    pub diagnostics: Mutex<TransportDiagnostics>,
    pub closing: AtomicBool,
    pub finished: AtomicBool,
    pub view: Mutex<NetworkView>,
    pub frames: Budget,
    pub workspace: Budget,
    pub tickets: Mutex<Vec<SessionTicket>>,
    pub connections: AtomicUsize,
    pub peers: AtomicUsize,
    pub pending: AtomicUsize,
    pub nonce: AtomicU64,
}
#[cfg(debug_assertions)]
#[derive(Default)]
pub(super) struct TransportDiagnostics {
    pub encode_started: Option<std::time::Instant>,
    pub encode_bytes: usize,
    pub encode_ms: u64,
    pub write_started: Option<std::time::Instant>,
    pub write_bytes: usize,
    pub write_ms: u64,
    pub write_complete: bool,
    pub send_buffer: usize,
    pub tcp_calls: u64,
    pub tcp_max_request: usize,
    pub tcp_accepted: u64,
}
#[derive(Clone)]
pub(super) struct Context {
    pub shared: Arc<Shared>,
    pub backend: BackendBridge,
    pub crypto: CryptoBridge,
    pub events: SyncSender<NetworkEvent>,
}
impl Context {
    pub fn event(&self, event: NetworkEvent) {
        let _ = self.events.try_send(event);
    }
    pub fn state(&self, state: NetworkState) {
        self.shared
            .view
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .state = state;
    }
    pub fn fail(&self, failure: SyncError) {
        let mut view = self
            .shared
            .view
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        view.state = NetworkState::Degraded;
        view.last_error = Some(failure.clone());
        drop(view);
        self.event(NetworkEvent::Error(failure));
    }
    pub fn new_ticket(&self, peer: &PeerRecord) -> SyncResult<SessionTicket> {
        let epoch = self.shared.nonce.fetch_add(1, Ordering::AcqRel);
        if epoch == u64::MAX {
            return Err(error("Sync session sequence exhausted"));
        }
        let ticket = SessionTicket::new(peer.peer_id.clone(), peer.auth_epoch, epoch);
        let mut tickets = self
            .shared
            .tickets
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if tickets.len() >= 8 {
            return Err(quota("Sync peer capacity exhausted"));
        }
        tickets.push(ticket.clone());
        Ok(ticket)
    }
    pub fn retire_ticket(&self, ticket: &SessionTicket) {
        ticket.retire();
        self.shared
            .tickets
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .retain(|old| old.session_epoch != ticket.session_epoch);
    }
}
pub struct SyncNetwork {
    shared: Arc<Shared>,
    commands: SyncSender<Command>,
    events: Receiver<NetworkEvent>,
    send_events: SyncSender<NetworkEvent>,
    workers: Vec<TrackedWorker>,
}
#[derive(Clone)]
pub(crate) struct SyncShutdownToken {
    shared: Arc<Shared>,
    workers: Vec<TrackedWorker>,
    events: SyncSender<NetworkEvent>,
}
impl SyncShutdownToken {
    pub(crate) fn begin_shutdown(&self) {
        self.shared.closing.store(true, Ordering::Release);
        for ticket in self
            .shared
            .tickets
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .iter()
        {
            ticket.retire();
        }
        let mut view = self
            .shared
            .view
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if view.state != NetworkState::Disabled {
            view.state = NetworkState::Retiring;
        }
        drop(view);
        for worker in &self.workers {
            worker.unpark();
        }
    }
    pub(crate) fn wait(&self, timeout: Duration) -> SyncResult<bool> {
        let start = Instant::now();
        for worker in &self.workers {
            if !worker.wait(timeout.saturating_sub(start.elapsed())) {
                return Ok(false);
            }
        }
        if !self.shared.finished.load(Ordering::Acquire)
            || self.shared.connections.load(Ordering::Acquire) != 0
            || self.shared.peers.load(Ordering::Acquire) != 0
            || self.shared.frames.used() != 0
            || self.shared.workspace.used() != 0
        {
            return Ok(false);
        }
        let mut view = self
            .shared
            .view
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let first = view.state != NetworkState::Disabled;
        view.state = NetworkState::Disabled;
        view.address = None;
        if self.workers.iter().any(TrackedWorker::panicked) {
            return Err(error("Sync worker panicked"));
        }
        if first {
            let _ = self.events.try_send(NetworkEvent::Stopped);
        }
        Ok(true)
    }
}
impl SyncNetwork {
    pub fn start_server(config: ServerConfig, backend: Arc<dyn SyncBackend>) -> SyncResult<Self> {
        config.validate()?;
        Self::start(
            move || {
                let listener = TcpListener::bind(config.listen_address)
                    .map_err(|_| error("Cannot bind sync listener"))?;
                listener
                    .set_nonblocking(true)
                    .map_err(|_| error("Cannot configure sync listener"))?;
                Ok(Mode::Server { listener, config })
            },
            backend,
        )
    }
    pub fn start_client(config: ClientConfig, backend: Arc<dyn SyncBackend>) -> SyncResult<Self> {
        config.validate()?;
        Self::start(move || Ok(Mode::Client(config)), backend)
    }
    fn start(
        mode: impl FnOnce() -> SyncResult<Mode>,
        backend: Arc<dyn SyncBackend>,
    ) -> SyncResult<Self> {
        let mut permits = Vec::new();
        for _ in 0..4 {
            permits.push(
                WorkerPermit::reserve(WorkerKind::Sync)
                    .map_err(|_| quota("Sync worker capacity exhausted"))?,
            );
        }
        let mode = mode()?;
        let address = match &mode {
            Mode::Server { listener, .. } => Some(
                listener
                    .local_addr()
                    .map_err(|_| error("Cannot inspect sync listener"))?,
            ),
            Mode::Client(_) => None,
        };
        let mut seed = [0; 8];
        getrandom::fill(&mut seed).map_err(|_| error("Cannot generate sync session epoch"))?;
        let seed = (u64::from_le_bytes(seed) & (i64::MAX as u64 - 1024)).max(1);
        let shared = Arc::new(Shared {
            #[cfg(debug_assertions)]
            diagnostics: Mutex::new(TransportDiagnostics::default()),
            closing: AtomicBool::new(false),
            finished: AtomicBool::new(false),
            view: Mutex::new(NetworkView {
                state: NetworkState::Starting,
                address,
                connections: 0,
                peers: 0,
                pending_rpc: 0,
                reserved_frame_bytes: 0,
                workspace_bytes: 0,
                last_error: None,
            }),
            frames: Budget::new(64 * 1024 * 1024),
            workspace: Budget::new(64 * 1024 * 1024),
            tickets: Mutex::new(Vec::new()),
            connections: AtomicUsize::new(0),
            peers: AtomicUsize::new(0),
            pending: AtomicUsize::new(0),
            nonce: AtomicU64::new(seed),
        });
        let (job_send, job_receive) = mpsc::sync_channel(32);
        let job_receive = Arc::new(Mutex::new(job_receive));
        let mut workers = Vec::new();
        for index in 0..2 {
            let permit = permits.pop().unwrap();
            let receive = job_receive.clone();
            let owned = shared.clone();
            let target = backend.clone();
            let thread = std::thread::Builder::new()
                .name(format!("lx-sync-backend-{index}"))
                .spawn(move || backend_loop(receive, owned, target))
                .map_err(|_| {
                    shared.closing.store(true, Ordering::Release);
                    error("Cannot start sync backend adapter")
                })?;
            workers.push(permit.attach(thread));
        }
        let (crypto_send, crypto_receive) = mpsc::sync_channel(16);
        let permit = permits.pop().unwrap();
        let owned = shared.clone();
        let thread = std::thread::Builder::new()
            .name("lx-sync-crypto".into())
            .spawn(move || crypto_loop(crypto_receive, owned))
            .map_err(|_| {
                shared.closing.store(true, Ordering::Release);
                error("Cannot start sync crypto owner")
            })?;
        workers.push(permit.attach(thread));
        let (commands, command_receive) = mpsc::sync_channel(32);
        let (send_events, events) = mpsc::sync_channel(64);
        let context = Context {
            shared: shared.clone(),
            backend: BackendBridge {
                send: job_send,
                shared: shared.clone(),
            },
            crypto: CryptoBridge {
                send: crypto_send,
                shared: shared.clone(),
            },
            events: send_events.clone(),
        };
        let permit = permits.pop().unwrap();
        let owned = shared.clone();
        let thread = std::thread::Builder::new()
            .name("lx-sync-io".into())
            .spawn(move || {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    let runtime = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .map_err(|_| error("Cannot create sync runtime"))?;
                    runtime.block_on(super::protocol::run(mode, command_receive, &context))
                }));
                if let Ok(Err(failure)) = result {
                    context.fail(failure);
                } else if result.is_err() {
                    context.fail(error("Sync IO owner panicked"));
                }
                owned.closing.store(true, Ordering::Release);
                for ticket in owned
                    .tickets
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .iter()
                {
                    ticket.retire();
                }
                owned.finished.store(true, Ordering::Release);
            })
            .map_err(|_| {
                shared.closing.store(true, Ordering::Release);
                error("Cannot start sync IO owner")
            })?;
        workers.push(permit.attach(thread));
        Ok(Self {
            shared,
            commands,
            events,
            send_events,
            workers,
        })
    }
    pub fn view(&self) -> NetworkView {
        let mut view = self
            .shared
            .view
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        view.connections = self.shared.connections.load(Ordering::Acquire);
        view.peers = self.shared.peers.load(Ordering::Acquire);
        view.pending_rpc = self.shared.pending.load(Ordering::Acquire);
        view.reserved_frame_bytes = self.shared.frames.used();
        view.workspace_bytes = self.shared.workspace.used();
        view
    }
    pub fn try_recv(&self) -> Option<NetworkEvent> {
        self.events.try_recv().ok()
    }
    #[cfg(debug_assertions)]
    pub fn debug_transport_stats(
        &self,
    ) -> (usize, u64, bool, usize, u64, bool, usize, u64, usize, u64) {
        let diag = self
            .shared
            .diagnostics
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        (
            diag.encode_bytes,
            diag.encode_started.map_or(diag.encode_ms, |time| {
                time.elapsed().as_millis().min(u64::MAX as u128) as u64
            }),
            diag.encode_started.is_some(),
            diag.write_bytes,
            diag.write_started.map_or(diag.write_ms, |time| {
                time.elapsed().as_millis().min(u64::MAX as u128) as u64
            }),
            diag.write_complete,
            diag.send_buffer,
            diag.tcp_calls,
            diag.tcp_max_request,
            diag.tcp_accepted,
        )
    }
    pub fn revoke(&self, peer_id: &str) -> SyncResult<()> {
        if peer_id.is_empty() || peer_id.len() > 256 {
            return Err(error("Invalid peer id"));
        }
        if self.shared.closing.load(Ordering::Acquire) {
            return Err(error("Sync owner is closing"));
        }
        for ticket in self
            .shared
            .tickets
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .iter()
            .filter(|ticket| ticket.peer_id == peer_id)
        {
            ticket.retire();
        }
        self.commands
            .try_send(Command::Revoke(peer_id.into()))
            .map_err(|_| quota("Sync command capacity exhausted"))
    }
    pub fn is_finished(&self) -> bool {
        self.shared.finished.load(Ordering::Acquire)
            && self.workers.iter().all(TrackedWorker::is_finished)
    }
    pub(crate) fn shutdown_token(&self) -> SyncShutdownToken {
        SyncShutdownToken {
            shared: self.shared.clone(),
            workers: self.workers.clone(),
            events: self.send_events.clone(),
        }
    }
    pub fn shutdown(&mut self, timeout: Duration) -> SyncResult<bool> {
        let token = self.shutdown_token();
        token.begin_shutdown();
        token.wait(timeout)
    }
}
impl Drop for SyncNetwork {
    fn drop(&mut self) {
        self.shutdown_token().begin_shutdown();
    }
}

pub(super) enum BackendValue {
    Unit,
    Peers(Vec<PeerRecord>),
    Snapshot(SyncSnapshot),
    Baseline(Option<SyncBaseline>),
    Receipt(SyncReceipt),
    Changes(SyncChanges),
    Mode(Option<super::super::SyncMode>),
}
pub(super) struct BackendOwned {
    pub value: BackendValue,
    pub credit: Credit,
}
type BackendFn = Box<dyn FnOnce(&dyn SyncBackend) -> SyncResult<BackendValue> + Send>;
struct BackendJob {
    operation: BackendFn,
    credit: Credit,
    send: tokio::sync::oneshot::Sender<SyncResult<BackendOwned>>,
}
#[derive(Clone)]
pub(super) struct BackendBridge {
    send: SyncSender<BackendJob>,
    shared: Arc<Shared>,
}
impl BackendBridge {
    pub async fn call(
        &self,
        cost: usize,
        operation: impl FnOnce(&dyn SyncBackend) -> SyncResult<BackendValue> + Send + 'static,
    ) -> SyncResult<BackendOwned> {
        let start = Instant::now();
        let credit = loop {
            if self.shared.closing.load(Ordering::Acquire) {
                return Err(error("Sync owner is closing"));
            }
            match self.shared.workspace.reserve(cost) {
                Ok(credit) => break credit,
                Err(error) => {
                    if start.elapsed() > Duration::from_secs(6) {
                        return Err(error);
                    }
                    tokio::time::sleep(Duration::from_millis(2)).await;
                }
            }
        };
        let (send, receive) = tokio::sync::oneshot::channel();
        self.send
            .try_send(BackendJob {
                operation: Box::new(operation),
                credit,
                send,
            })
            .map_err(|_| quota("Sync backend queue full"))?;
        tokio::time::timeout(
            Duration::from_secs(6).saturating_sub(start.elapsed()),
            receive,
        )
        .await
        .map_err(|_| error("Sync backend timed out"))?
        .map_err(|_| error("Sync backend closed"))?
    }
}
fn backend_loop(
    receive: Arc<Mutex<Receiver<BackendJob>>>,
    shared: Arc<Shared>,
    backend: Arc<dyn SyncBackend>,
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
        let mut credit = job.credit;
        let outcome =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| (job.operation)(&*backend)))
                .unwrap_or_else(|_| Err(error("Sync backend adapter panicked")))
                .and_then(|value| {
                    credit.shrink(backend_footprint(&value))?;
                    Ok(BackendOwned { value, credit })
                });
        let _ = job.send.send(outcome);
    }
}
fn backend_footprint(value: &BackendValue) -> usize {
    match value {
        BackendValue::Unit | BackendValue::Mode(_) => 0,
        BackendValue::Peers(peers) => peers
            .iter()
            .map(|peer| {
                peer.peer_id.capacity()
                    + peer.client_id.capacity()
                    + peer.device_name.capacity()
                    + 512
            })
            .sum(),
        BackendValue::Snapshot(snapshot) => snapshot_footprint(snapshot),
        BackendValue::Baseline(baseline) => baseline
            .as_ref()
            .map(|baseline| data_footprint(&baseline.data) + baseline.digest.capacity() + 256)
            .unwrap_or(0),
        BackendValue::Receipt(receipt) => receipt.digest.capacity() + 256,
        BackendValue::Changes(SyncChanges::NeedsSnapshot) => 0,
        BackendValue::Changes(SyncChanges::Delta { records, snapshot }) => {
            snapshot_footprint(snapshot)
                + records
                    .iter()
                    .map(|record| super::memory::value_footprint(&record.action) + 128)
                    .sum::<usize>()
        }
    }
}
pub(super) fn data_footprint(data: &SyncData) -> usize {
    match data {
        SyncData::List(lists) => super::memory::list_footprint(lists),
        SyncData::Dislike(rules) => rules.capacity() + 64,
    }
}
pub(super) fn snapshot_footprint(snapshot: &SyncSnapshot) -> usize {
    super::memory::list_footprint(&snapshot.lists) + snapshot.dislike.capacity() + 256
}
pub(super) enum CryptoValue {
    Frame(super::super::RpcFrame, Credit),
    Pair { peer: PeerRecord, response: String },
    KeyPair(super::super::PairingKeyPair),
}
type CryptoFn = Box<dyn FnOnce() -> SyncResult<CryptoValue> + Send>;
struct CryptoJob {
    operation: CryptoFn,
    send: tokio::sync::oneshot::Sender<SyncResult<CryptoValue>>,
}
#[derive(Clone)]
pub(super) struct CryptoBridge {
    send: SyncSender<CryptoJob>,
    shared: Arc<Shared>,
}
impl CryptoBridge {
    pub async fn call(
        &self,
        operation: impl FnOnce() -> SyncResult<CryptoValue> + Send + 'static,
    ) -> SyncResult<CryptoValue> {
        if self.shared.closing.load(Ordering::Acquire) {
            return Err(error("Sync owner is closing"));
        }
        let (send, receive) = tokio::sync::oneshot::channel();
        self.send
            .try_send(CryptoJob {
                operation: Box::new(operation),
                send,
            })
            .map_err(|_| quota("Sync crypto queue full"))?;
        tokio::time::timeout(Duration::from_secs(10), receive)
            .await
            .map_err(|_| error("Sync crypto timed out"))?
            .map_err(|_| error("Sync crypto owner closed"))?
    }
}
fn crypto_loop(receive: Receiver<CryptoJob>, shared: Arc<Shared>) {
    while !shared.closing.load(Ordering::Acquire) {
        let Ok(job) = receive.recv_timeout(Duration::from_millis(20)) else {
            continue;
        };
        if shared.closing.load(Ordering::Acquire) {
            break;
        }
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(job.operation))
            .unwrap_or_else(|_| Err(error("Sync crypto job panicked")));
        let _ = job.send.send(result);
    }
}
pub(super) fn error(message: &str) -> SyncError {
    SyncError::new(SyncErrorKind::InvalidData, message)
}
pub(super) fn quota(message: &str) -> SyncError {
    SyncError::new(SyncErrorKind::Quota, message)
}
pub(super) fn random_id() -> SyncResult<String> {
    let mut bytes = [0; 16];
    getrandom::fill(&mut bytes).map_err(|_| error("Cannot generate sync identity"))?;
    Ok(STANDARD.encode(bytes))
}

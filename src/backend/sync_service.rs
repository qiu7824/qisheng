//! Explicit profile sync service and actor-owned repository adapter.
use super::{BackendError, BackendHandle, BackendInner, ErrorKind, RuntimeMessage};
use crate::sync::network::*;
use crate::sync::{Dataset, PeerCapabilities, SyncError, SyncErrorKind, SyncMode, SyncResult};
use std::sync::{
    Arc, RwLock, RwLockReadGuard, Weak,
    atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering},
    mpsc,
};
use std::time::{Duration, Instant};

pub(crate) struct SyncOwnerGate {
    pub epoch: u64,
    pub profile_epoch: u64,
    pub closing: AtomicBool,
    retired: AtomicBool,
    commit: RwLock<()>,
    inflight: AtomicUsize,
    role: AtomicU8,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ManagedSyncRole {
    Server,
    Client,
}
impl SyncOwnerGate {
    pub(crate) fn new(epoch: u64, profile_epoch: u64) -> Arc<Self> {
        Arc::new(Self {
            epoch,
            profile_epoch,
            closing: AtomicBool::new(false),
            retired: AtomicBool::new(false),
            commit: RwLock::new(()),
            inflight: AtomicUsize::new(0),
            role: AtomicU8::new(0),
        })
    }
    pub(crate) fn begin_close(&self) {
        self.closing.store(true, Ordering::Release);
    }
    pub(crate) fn check(&self, cancel: &AtomicBool) -> Result<(), BackendError> {
        if self.closing.load(Ordering::Acquire) || cancel.load(Ordering::Acquire) {
            return Err(BackendError::new(
                ErrorKind::Cancelled,
                "Sync owner/request retired",
            ));
        }
        Ok(())
    }
    pub(crate) fn effect_guard<'a>(
        &'a self,
        cancel: &AtomicBool,
    ) -> Result<RwLockReadGuard<'a, ()>, BackendError> {
        let guard = self
            .commit
            .read()
            .unwrap_or_else(|error| error.into_inner());
        self.check(cancel)?;
        Ok(guard)
    }
    pub(crate) fn operation(
        self: &Arc<Self>,
        cancel: &AtomicBool,
    ) -> Result<SyncActorOperation, BackendError> {
        self.check(cancel)?;
        self.inflight
            .try_update(Ordering::AcqRel, Ordering::Acquire, |value| {
                (value < 32).then_some(value + 1)
            })
            .map_err(|_| BackendError::new(ErrorKind::Busy, "Sync actor admission exhausted"))?;
        Ok(SyncActorOperation(self.clone()))
    }
    pub(crate) fn retirement_complete(&self) -> bool {
        self.closing.load(Ordering::Acquire)
            && self.retired.load(Ordering::Acquire)
            && self.inflight.load(Ordering::Acquire) == 0
            && self.commit.try_write().is_ok()
    }
    pub(crate) fn mark_retired(&self) {
        self.retired.store(true, Ordering::Release);
    }
    pub(crate) fn configure_role(&self, role: ManagedSyncRole) -> Result<(), BackendError> {
        let role = match role {
            ManagedSyncRole::Server => 1,
            ManagedSyncRole::Client => 2,
        };
        match self
            .role
            .compare_exchange(0, role, Ordering::AcqRel, Ordering::Acquire)
        {
            Ok(_) => Ok(()),
            Err(current) if current == role => Ok(()),
            _ => Err(BackendError::new(
                ErrorKind::Cancelled,
                "Active sync owner role cannot change",
            )),
        }
    }
    pub(crate) fn role(&self) -> Result<ManagedSyncRole, BackendError> {
        match self.role.load(Ordering::Acquire) {
            1 => Ok(ManagedSyncRole::Server),
            2 => Ok(ManagedSyncRole::Client),
            _ => Err(BackendError::new(
                ErrorKind::Cancelled,
                "Sync owner role has not been configured",
            )),
        }
    }
    fn wait_retired(&self, deadline: Instant) -> bool {
        while !self.retirement_complete() {
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        true
    }
}
pub(crate) struct SyncActorOperation(Arc<SyncOwnerGate>);
impl Drop for SyncActorOperation {
    fn drop(&mut self) {
        self.0.inflight.fetch_sub(1, Ordering::AcqRel);
    }
}

pub(crate) enum SyncRequest {
    CloseOwner,
    Peers,
    ConfigurePairing {
        epoch: u64,
        enabled: bool,
        role: ManagedSyncRole,
    },
    StorePeer {
        epoch: u64,
        peer: PeerRecord,
    },
    Begin {
        ticket: SessionTicket,
    },
    Retire {
        ticket: SessionTicket,
    },
    Snapshot {
        ticket: SessionTicket,
    },
    Baseline {
        ticket: SessionTicket,
        dataset: Dataset,
    },
    Apply {
        ticket: SessionTicket,
        expected_sequence: Option<u64>,
        operation: SyncOperation,
    },
    Confirm {
        ticket: SessionTicket,
        data: SyncData,
        revision: u64,
        sequence: u64,
        digest: String,
    },
    Changes {
        ticket: SessionTicket,
        sequence: u64,
        limit: usize,
    },
    Mode {
        ticket: SessionTicket,
        dataset: Dataset,
    },
    Revoke {
        peer_id: String,
    },
}
pub(crate) enum SyncResponse {
    Unit,
    Peers(Vec<PeerRecord>),
    Snapshot(SyncSnapshot),
    Baseline(Option<SyncBaseline>),
    Receipt(SyncReceipt),
    Changes(SyncChanges),
    Mode(Option<SyncMode>),
}
pub(crate) enum SyncManagementRequest {
    Peers,
    ClientCredentials {
        server_id: String,
    },
    ChooseMode {
        owner_epoch: u64,
        peer_id: String,
        auth_epoch: u64,
        session_epoch: u64,
        dataset: Dataset,
        mode: SyncMode,
    },
}
pub(crate) enum SyncManagementResponse {
    Unit,
    Peers(Vec<SyncPeerView>),
    ClientCredentials(ClientCredentials),
}
#[derive(Clone, Debug)]
pub struct SyncPeerView {
    pub peer_id: String,
    pub client_id: String,
    pub device_name: String,
    pub is_mobile: bool,
    pub auth_epoch: u64,
    pub owner_epoch: Option<u64>,
    pub session_epoch: Option<u64>,
    pub capabilities: PeerCapabilities,
    pub list_mode: Option<SyncMode>,
    pub dislike_mode: Option<SyncMode>,
}

#[derive(Clone)]
pub(super) struct RegisteredSyncEndpoint {
    pub(super) gate: Weak<SyncOwnerGate>,
    token: SyncShutdownToken,
}
impl RegisteredSyncEndpoint {
    pub(super) fn begin_shutdown(&self) {
        if let Some(gate) = self.gate.upgrade() {
            gate.begin_close();
        }
        self.token.begin_shutdown();
    }
    pub(super) fn begin_close(&self) {
        self.begin_shutdown();
    }
    pub(super) fn wait(&self, timeout: Duration) -> Result<bool, BackendError> {
        let deadline = Instant::now() + timeout;
        if let Some(gate) = self.gate.upgrade()
            && !gate.wait_retired(deadline)
        {
            return Ok(false);
        }
        self.token
            .wait(deadline.saturating_duration_since(Instant::now()))
            .map_err(sync_backend_error)
    }
}

pub struct BackendSyncSession {
    backend: Weak<BackendInner>,
    pub(super) gate: Arc<SyncOwnerGate>,
}
impl BackendSyncSession {
    pub fn epoch(&self) -> u64 {
        self.gate.epoch
    }
    pub fn retire(&self) {
        self.gate.begin_close();
        self.enqueue_close_owner();
    }
    fn enqueue_close_owner(&self) {
        if let Some(inner) = self.backend.upgrade() {
            let (reply, _) = mpsc::sync_channel(1);
            let _ = BackendHandle { inner }.send_query(RuntimeMessage::SyncRequest {
                gate: self.gate.clone(),
                cancel: Arc::new(AtomicBool::new(false)),
                request: SyncRequest::CloseOwner,
                reply,
            });
        }
    }
    fn call(&self, request: SyncRequest) -> SyncResult<SyncResponse> {
        let cancel = Arc::new(AtomicBool::new(false));
        self.gate.check(&cancel).map_err(to_sync_error)?;
        let inner = self
            .backend
            .upgrade()
            .ok_or_else(|| SyncError::new(SyncErrorKind::Cancelled, "Player backend stopped"))?;
        let (reply, receive) = mpsc::sync_channel(1);
        let message = RuntimeMessage::SyncRequest {
            gate: self.gate.clone(),
            cancel: cancel.clone(),
            request,
            reply,
        };
        let result = BackendHandle { inner }
            .send_query(message)
            .map_err(to_sync_error);
        result?;
        receive
            .recv_timeout(Duration::from_secs(6))
            .map_err(|error| {
                cancel.store(true, Ordering::Release);
                SyncError::new(
                    SyncErrorKind::Cancelled,
                    match error {
                        mpsc::RecvTimeoutError::Timeout => "Sync actor response timed out",
                        mpsc::RecvTimeoutError::Disconnected => "Sync actor stopped",
                    },
                )
            })?
            .map_err(to_sync_error)
    }
    fn unit(&self, request: SyncRequest) -> SyncResult<()> {
        match self.call(request)? {
            SyncResponse::Unit => Ok(()),
            _ => Err(protocol_response()),
        }
    }
    fn configure_pairing(
        &self,
        epoch: u64,
        enabled: bool,
        role: ManagedSyncRole,
    ) -> SyncResult<()> {
        self.unit(SyncRequest::ConfigurePairing {
            epoch,
            enabled,
            role,
        })
    }
}
impl Drop for BackendSyncSession {
    fn drop(&mut self) {
        self.retire();
    }
}
impl SyncBackend for BackendSyncSession {
    fn peers(&self) -> SyncResult<Vec<PeerRecord>> {
        match self.call(SyncRequest::Peers)? {
            SyncResponse::Peers(peers) => Ok(peers),
            _ => Err(protocol_response()),
        }
    }
    fn store_peer(&self, epoch: u64, peer: PeerRecord) -> SyncResult<()> {
        self.unit(SyncRequest::StorePeer { epoch, peer })
    }
    fn begin_session(&self, ticket: &SessionTicket) -> SyncResult<()> {
        self.unit(SyncRequest::Begin {
            ticket: ticket.clone(),
        })
    }
    fn retire_session(&self, ticket: &SessionTicket) {
        ticket.retire();
        if let Some(inner) = self.backend.upgrade() {
            let (reply, _) = mpsc::sync_channel(1);
            let _ = BackendHandle { inner }.send_query(RuntimeMessage::SyncRequest {
                gate: self.gate.clone(),
                cancel: Arc::new(AtomicBool::new(false)),
                request: SyncRequest::Retire {
                    ticket: ticket.clone(),
                },
                reply,
            });
        }
    }
    fn snapshot(&self, ticket: &SessionTicket) -> SyncResult<SyncSnapshot> {
        match self.call(SyncRequest::Snapshot {
            ticket: ticket.clone(),
        })? {
            SyncResponse::Snapshot(snapshot) => Ok(snapshot),
            _ => Err(protocol_response()),
        }
    }
    fn baseline(
        &self,
        ticket: &SessionTicket,
        dataset: Dataset,
    ) -> SyncResult<Option<SyncBaseline>> {
        match self.call(SyncRequest::Baseline {
            ticket: ticket.clone(),
            dataset,
        })? {
            SyncResponse::Baseline(data) => Ok(data),
            _ => Err(protocol_response()),
        }
    }
    fn apply_remote(
        &self,
        ticket: &SessionTicket,
        expected_sequence: Option<u64>,
        operation: SyncOperation,
    ) -> SyncResult<SyncReceipt> {
        match self.call(SyncRequest::Apply {
            ticket: ticket.clone(),
            expected_sequence,
            operation,
        })? {
            SyncResponse::Receipt(receipt) => Ok(receipt),
            _ => Err(protocol_response()),
        }
    }
    fn confirm_baseline(
        &self,
        ticket: &SessionTicket,
        data: SyncData,
        revision: u64,
        sequence: u64,
        digest: &str,
    ) -> SyncResult<()> {
        self.unit(SyncRequest::Confirm {
            ticket: ticket.clone(),
            data,
            revision,
            sequence,
            digest: digest.into(),
        })
    }
    fn changes_after(
        &self,
        ticket: &SessionTicket,
        sequence: u64,
        limit: usize,
    ) -> SyncResult<SyncChanges> {
        match self.call(SyncRequest::Changes {
            ticket: ticket.clone(),
            sequence,
            limit,
        })? {
            SyncResponse::Changes(data) => Ok(data),
            _ => Err(protocol_response()),
        }
    }
    fn select_mode(
        &self,
        ticket: &SessionTicket,
        dataset: Dataset,
    ) -> SyncResult<Option<SyncMode>> {
        match self.call(SyncRequest::Mode {
            ticket: ticket.clone(),
            dataset,
        })? {
            SyncResponse::Mode(mode) => Ok(mode),
            _ => Err(protocol_response()),
        }
    }
    fn revoke_peer(&self, peer_id: &str) -> SyncResult<()> {
        self.unit(SyncRequest::Revoke {
            peer_id: peer_id.into(),
        })
    }
}
fn protocol_response() -> SyncError {
    SyncError::new(SyncErrorKind::InvalidData, "Unexpected sync actor response")
}
fn to_sync_error(error: BackendError) -> SyncError {
    SyncError::new(
        match error.kind {
            ErrorKind::Cancelled | ErrorKind::Closed | ErrorKind::Timeout => {
                SyncErrorKind::Cancelled
            }
            ErrorKind::Busy => SyncErrorKind::Quota,
            _ => SyncErrorKind::InvalidData,
        },
        error.message,
    )
}
pub(super) fn sync_backend_error(error: SyncError) -> BackendError {
    BackendError::new(
        match error.kind {
            SyncErrorKind::Cancelled => ErrorKind::Cancelled,
            SyncErrorKind::Quota => ErrorKind::Busy,
            SyncErrorKind::Authentication => ErrorKind::Unavailable,
            _ => ErrorKind::InvalidData,
        },
        error.message,
    )
}

impl BackendHandle {
    pub fn sync_session(&self) -> Result<Arc<BackendSyncSession>, BackendError> {
        let _open = self
            .inner
            .sync_session_open
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if self.inner.sync_admission_closed.load(Ordering::Acquire) {
            return Err(BackendError::new(
                ErrorKind::Closed,
                "Sync admission closed",
            ));
        }
        let (reply, receive) = mpsc::sync_channel(1);
        let cancel = Arc::new(AtomicBool::new(false));
        self.send_query(RuntimeMessage::OpenSyncSession {
            cancel: cancel.clone(),
            reply,
        })?;
        let gate = receive.recv_timeout(Duration::from_secs(6)).map_err(|_| {
            cancel.store(true, Ordering::Release);
            BackendError::new(ErrorKind::Timeout, "Cannot open sync owner session")
        })??;
        Ok(Arc::new(BackendSyncSession {
            backend: Arc::downgrade(&self.inner),
            gate,
        }))
    }
    pub fn start_sync_server(&self, config: ServerConfig) -> SyncResult<BackendSyncNetwork> {
        let epoch = config.pairing_epoch;
        self.start_sync(epoch, true, ManagedSyncRole::Server, move |session| {
            SyncNetwork::start_server(config, session)
        })
    }
    pub fn start_sync_client(&self, config: ClientConfig) -> SyncResult<BackendSyncNetwork> {
        let epoch = config.pairing_epoch;
        let pair = config.pairing_code.is_some();
        self.start_sync(epoch, pair, ManagedSyncRole::Client, move |session| {
            SyncNetwork::start_client(config, session)
        })
    }
    pub fn start_sync_client_saved(
        &self,
        mut config: ClientConfig,
        server_id: String,
    ) -> SyncResult<BackendSyncNetwork> {
        if config.pairing_code.is_some() || config.credentials.is_some() {
            return Err(SyncError::new(
                SyncErrorKind::InvalidData,
                "Saved-client startup requires a selected server ID without supplied keys or pairing code",
            ));
        }
        let SyncManagementResponse::ClientCredentials(credentials) = self
            .sync_management(SyncManagementRequest::ClientCredentials { server_id })
            .map_err(to_sync_error)?
        else {
            return Err(protocol_response());
        };
        config.credentials = Some(credentials);
        self.start_sync_client(config)
    }
    fn start_sync(
        &self,
        pairing_epoch: u64,
        pairing: bool,
        role: ManagedSyncRole,
        start: impl FnOnce(Arc<BackendSyncSession>) -> SyncResult<SyncNetwork>,
    ) -> SyncResult<BackendSyncNetwork> {
        let mut endpoints = self
            .inner
            .sync_endpoints
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        endpoints.retain(|endpoint| !matches!(endpoint.wait(Duration::ZERO), Ok(true)));
        if endpoints.len() >= 4 {
            return Err(SyncError::new(
                SyncErrorKind::Quota,
                "Retiring sync endpoint capacity reached",
            ));
        }
        let session = self.sync_session().map_err(to_sync_error)?;
        session.configure_pairing(pairing_epoch, pairing, role)?;
        let network = start(session.clone())?;
        let endpoint = RegisteredSyncEndpoint {
            gate: Arc::downgrade(&session.gate),
            token: network.shutdown_token(),
        };
        if self.inner.shutdown.load(Ordering::Acquire)
            || self.inner.sync_admission_closed.load(Ordering::Acquire)
        {
            endpoint.begin_shutdown();
            endpoints.push(endpoint);
            return Err(SyncError::new(
                SyncErrorKind::Cancelled,
                "Backend stopped during sync startup",
            ));
        }
        endpoints.push(endpoint);
        Ok(BackendSyncNetwork { network, session })
    }
    pub fn sync_peers(&self) -> Result<Vec<SyncPeerView>, BackendError> {
        match self.sync_management(SyncManagementRequest::Peers)? {
            SyncManagementResponse::Peers(peers) => Ok(peers),
            _ => Err(BackendError::new(
                ErrorKind::InvalidData,
                "Unexpected sync management response",
            )),
        }
    }
    pub fn choose_sync_mode(
        &self,
        owner_epoch: u64,
        peer_id: String,
        auth_epoch: u64,
        session_epoch: u64,
        dataset: Dataset,
        mode: SyncMode,
    ) -> Result<(), BackendError> {
        match self.sync_management(SyncManagementRequest::ChooseMode {
            owner_epoch,
            peer_id,
            auth_epoch,
            session_epoch,
            dataset,
            mode,
        })? {
            SyncManagementResponse::Unit => Ok(()),
            _ => Err(BackendError::new(
                ErrorKind::InvalidData,
                "Unexpected mode response",
            )),
        }
    }
    fn sync_management(
        &self,
        request: SyncManagementRequest,
    ) -> Result<SyncManagementResponse, BackendError> {
        let (reply, receive) = mpsc::sync_channel(1);
        let cancel = Arc::new(AtomicBool::new(false));
        self.send_query(RuntimeMessage::SyncManagement {
            request,
            cancel: cancel.clone(),
            reply,
        })?;
        receive.recv_timeout(Duration::from_secs(6)).map_err(|_| {
            cancel.store(true, Ordering::Release);
            BackendError::new(ErrorKind::Timeout, "Sync management response timed out")
        })?
    }
}
pub struct BackendSyncNetwork {
    network: SyncNetwork,
    session: Arc<BackendSyncSession>,
}
impl BackendSyncNetwork {
    pub fn epoch(&self) -> u64 {
        self.session.epoch()
    }
    pub fn view(&self) -> NetworkView {
        self.network.view()
    }
    pub fn try_recv(&self) -> Option<NetworkEvent> {
        self.network.try_recv()
    }
    pub fn revoke(&self, peer_id: &str) -> SyncResult<()> {
        self.session.revoke_peer(peer_id)?;
        self.network.revoke(peer_id)
    }
    pub fn shutdown(&mut self, timeout: Duration) -> SyncResult<bool> {
        let deadline = Instant::now() + timeout;
        self.session.retire();
        self.network.shutdown_token().begin_shutdown();
        if !self.session.gate.wait_retired(deadline) {
            return Ok(false);
        }
        self.network
            .shutdown(deadline.saturating_duration_since(Instant::now()))
    }
    pub fn is_finished(&self) -> bool {
        self.network.is_finished()
    }
}
impl Drop for BackendSyncNetwork {
    fn drop(&mut self) {
        self.session.retire();
        self.network.shutdown_token().begin_shutdown();
    }
}

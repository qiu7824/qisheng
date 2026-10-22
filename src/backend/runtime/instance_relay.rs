use super::*;
use crate::repository::{ProfileGuard, RepositoryOpenFailure, RepositoryOpenOwner};
use crate::system_control::instance_relay::{
    RelayAcknowledgement, RelayBackend, RelayDispatch, RelayDisposition, RelayError,
    RelayErrorKind, RelayFence, RelayRequest,
};

/// A pre-open startup failure retains the exact supplied lock descriptor.
/// Taking the owner transfers it without acquiring another profile lock.
pub struct RetainedProfileGuardStartupFailure {
    error: anyhow::Error,
    owner: Mutex<Option<ProfileGuard>>,
}
impl RetainedProfileGuardStartupFailure {
    pub fn take_guard(&self) -> Option<ProfileGuard> {
        self.owner.lock().unwrap_or_else(|e| e.into_inner()).take()
    }
    pub fn has_guard(&self) -> bool {
        self.owner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some()
    }
    pub fn error(&self) -> &anyhow::Error {
        &self.error
    }
    pub(super) fn preserve(error: anyhow::Error, guard: ProfileGuard) -> Self {
        Self {
            error,
            owner: Mutex::new(Some(guard)),
        }
    }
}
impl std::fmt::Debug for RetainedProfileGuardStartupFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RetainedProfileGuardStartupFailure")
            .field("error", &self.error)
            .field("has_guard", &self.has_guard())
            .finish()
    }
}
impl std::fmt::Display for RetainedProfileGuardStartupFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Cannot prepare guarded backend: {:#}", self.error)
    }
}
impl std::error::Error for RetainedProfileGuardStartupFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.error.as_ref())
    }
}
#[cfg(windows)]
use crate::system_control::instance_relay::{
    RelayClient, RelayClientFailure, RelayEndpoint, RelayServer,
};

/// Initialization keeps the exact guarded repository through every failed
/// startup phase. Attached services retain their independent actual join owner.
pub struct RetainedRepositoryInitializationFailure {
    error: anyhow::Error,
    owner: Mutex<Option<Arc<Repository>>>,
}
impl RetainedRepositoryInitializationFailure {
    pub fn take_repository(&self) -> Option<Arc<Repository>> {
        self.owner.lock().unwrap_or_else(|e| e.into_inner()).take()
    }
    pub fn has_repository(&self) -> bool {
        self.owner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some()
    }
    pub fn error(&self) -> &anyhow::Error {
        &self.error
    }
    pub(super) fn preserve(error: anyhow::Error, repository: Arc<Repository>) -> Self {
        Self {
            error,
            owner: Mutex::new(Some(repository)),
        }
    }
}
impl std::fmt::Debug for RetainedRepositoryInitializationFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RetainedRepositoryInitializationFailure")
            .field("error", &self.error)
            .field("has_repository", &self.has_repository())
            .finish()
    }
}
impl std::fmt::Display for RetainedRepositoryInitializationFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Cannot initialize guarded backend: {:#}", self.error)
    }
}
impl std::error::Error for RetainedRepositoryInitializationFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.error.as_ref())
    }
}

/// The mutex makes an opening SQLite owner inspectable without claiming that
/// its connection is safe for concurrent access.
pub struct RetainedRepositoryOpenFailure {
    error: anyhow::Error,
    owner: Mutex<Option<RepositoryOpenOwner>>,
}
impl RetainedRepositoryOpenFailure {
    pub fn take_owner(&self) -> Option<RepositoryOpenOwner> {
        self.owner.lock().unwrap_or_else(|e| e.into_inner()).take()
    }
    pub fn has_owner(&self) -> bool {
        self.owner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some()
    }
    pub(super) fn preserve(failure: RepositoryOpenFailure) -> Self {
        Self {
            error: failure.error,
            owner: Mutex::new(Some(failure.owner)),
        }
    }
}
impl std::fmt::Debug for RetainedRepositoryOpenFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RetainedRepositoryOpenFailure")
            .field("error", &self.error)
            .field("has_owner", &self.has_owner())
            .finish()
    }
}
impl std::fmt::Display for RetainedRepositoryOpenFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Cannot open guarded repository: {:#}", self.error)
    }
}
impl std::error::Error for RetainedRepositoryOpenFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.error.as_ref())
    }
}

struct ActorBridge {
    commands: mpsc::SyncSender<RuntimeMessage>,
    shutdown: Arc<AtomicBool>,
}
impl RelayBackend for ActorBridge {
    fn dispatch_deeplink(
        &self,
        request: RelayRequest,
        fence: RelayFence,
    ) -> std::result::Result<RelayDispatch, RelayError> {
        fence.check()?;
        if self.shutdown.load(Ordering::Acquire) {
            return Err(relay_error(RelayErrorKind::Cancelled));
        }
        let (reply, receiver) = mpsc::sync_channel(1);
        self.commands
            .try_send(RuntimeMessage::RelayInput {
                request,
                fence: fence.clone(),
                reply,
            })
            .map_err(|error| {
                relay_error(match error {
                    mpsc::TrySendError::Full(_) => RelayErrorKind::Busy,
                    mpsc::TrySendError::Disconnected(_) => RelayErrorKind::Cancelled,
                })
            })?;
        Ok(RelayDispatch::from_receiver(receiver, fence))
    }
}
fn relay_error(kind: RelayErrorKind) -> RelayError {
    RelayError {
        kind,
        os_code: None,
        delivery_unconfirmed: false,
    }
}

pub(super) struct InstanceRelayRuntime {
    bridge: Arc<ActorBridge>,
    closing: bool,
    #[cfg(windows)]
    endpoint: Option<RelayEndpoint>,
    #[cfg(windows)]
    server: Option<RelayServer>,
}
impl InstanceRelayRuntime {
    pub(super) fn new(
        commands: mpsc::SyncSender<RuntimeMessage>,
        shutdown: Arc<AtomicBool>,
    ) -> Self {
        Self {
            bridge: Arc::new(ActorBridge { commands, shutdown }),
            closing: false,
            #[cfg(windows)]
            endpoint: None,
            #[cfg(windows)]
            server: None,
        }
    }
    #[cfg(windows)]
    pub(super) fn prepare(&mut self, endpoint: Option<RelayEndpoint>) {
        self.endpoint = endpoint;
    }
    pub(super) fn start(&mut self) -> Result<()> {
        #[cfg(windows)]
        if let Some(endpoint) = self.endpoint.take() {
            match RelayServer::start_verified_endpoint(endpoint, self.bridge.clone()) {
                Ok(server) => self.server = Some(server),
                Err(failure) => {
                    self.server = failure.owner.map(|owner| *owner);
                    self.closing = true;
                    return Err(anyhow::Error::new(failure.error));
                }
            }
        }
        Ok(())
    }
}
impl Runtime {
    pub(super) fn start_instance_relay(&mut self) -> Result<()> {
        self.instance_relay.start()
    }
    pub(super) fn begin_instance_relay_shutdown(&mut self) {
        self.instance_relay.closing = true;
        #[cfg(windows)]
        if let Some(server) = &self.instance_relay.server {
            server.begin_shutdown();
        }
    }
    pub(super) fn poll_instance_relay_quiesce_step(&mut self) -> Result<bool> {
        ensure!(
            self.instance_relay.closing,
            "Instance relay admission remains open"
        );
        #[cfg(windows)]
        if let Some(server) = self.instance_relay.server.as_mut() {
            if !server.shutdown(Duration::ZERO)? {
                return Ok(false);
            }
            self.instance_relay.server = None;
        }
        Ok(true)
    }
    pub(super) fn relay_input(
        &mut self,
        request: RelayRequest,
        fence: RelayFence,
    ) -> std::result::Result<RelayAcknowledgement, RelayError> {
        fence.check()?;
        if self.instance_relay.closing
            || self.instance_relay.bridge.shutdown.load(Ordering::Acquire)
        {
            return Err(relay_error(RelayErrorKind::Cancelled));
        }
        #[cfg(windows)]
        if !self
            .instance_relay
            .server
            .as_ref()
            .is_some_and(|server| server.accepts_fence(&fence))
        {
            return Err(relay_error(RelayErrorKind::Cancelled));
        }
        #[cfg(not(windows))]
        return Err(relay_error(RelayErrorKind::Unauthorized));
        #[cfg(windows)]
        {
            let envelope = CommandEnvelope {
                request_id: request.request_id().into(),
                origin: "instance_relay".into(),
                expected_revision: None,
                command: BackendCommand::DispatchDeepLink {
                    url: request.link().into(),
                },
            };
            // No native I/O or parsing of arbitrary backend commands occurs in
            // this actor branch. The ordinary deep-link handler validates again.
            fence.check()?;
            let receipt = self.execute(envelope).map_err(|error| {
                relay_error(match error.kind {
                    ErrorKind::Busy => RelayErrorKind::Busy,
                    ErrorKind::Cancelled | ErrorKind::Closed => RelayErrorKind::Cancelled,
                    ErrorKind::Timeout => RelayErrorKind::Timeout,
                    _ => RelayErrorKind::Invalid,
                })
            })?;
            Ok(RelayAcknowledgement {
                request_id: receipt.request_id,
                operation_id: Some(receipt.operation_id),
                disposition: match receipt.state {
                    ReceiptState::Applied => RelayDisposition::Applied,
                    ReceiptState::Committed => RelayDisposition::Committed,
                    ReceiptState::Accepted => RelayDisposition::Accepted,
                    ReceiptState::Completed => RelayDisposition::Completed,
                },
            })
        }
    }
}

/// A second instance uses the actual descriptor retained by a failed acquire;
/// it never opens another repository or claims a writer lock.
#[cfg(windows)]
pub fn forward_deeplink_to_existing_profile(
    guard: &ProfileGuard,
    request_id: String,
    link: String,
    timeout: Duration,
) -> std::result::Result<RelayAcknowledgement, RelayClientFailure> {
    let request = RelayRequest::new(request_id, link)
        .map_err(|error| RelayClientFailure { error, owner: None })?;
    let endpoint = RelayEndpoint::from_existing_guard(guard)
        .map_err(|error| RelayClientFailure { error, owner: None })?;
    let client = RelayClient::start(endpoint, request, timeout)
        .map_err(|error| RelayClientFailure { error, owner: None })?;
    client.finish(timeout)
}

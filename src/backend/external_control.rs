use super::{BackendError, BackendHandle, BackendInner, ErrorKind, RuntimeMessage};
use crate::open_api::{
    ArtworkReference, ArtworkResponse, ControlAction, ControlError, ControlErrorKind,
    ControlResult, ExternalControlBackend, OpenApiServer, ServerConfig, ServerEvent, ServerView,
    StatusSnapshot, StatusUpdate,
};
use std::sync::{
    Arc, RwLock, Weak,
    atomic::{AtomicBool, AtomicU64, Ordering},
    mpsc,
};
use std::time::Duration;

pub(crate) struct ExternalSessionGate {
    pub epoch: u64,
    pub alive: RwLock<bool>,
    pub retired: AtomicBool,
    pub artwork: RwLock<Option<Weak<AtomicBool>>>,
}
impl ExternalSessionGate {
    pub(crate) fn retire(&self) {
        self.retired.store(true, Ordering::Release);
        if let Some(retired) = self
            .artwork
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .as_ref()
            .and_then(Weak::upgrade)
        {
            retired.store(true, Ordering::Release);
        }
        *self
            .alive
            .write()
            .unwrap_or_else(|error| error.into_inner()) = false;
    }
}
#[derive(Clone)]
pub(super) struct RegisteredEndpoint {
    gate: Weak<ExternalSessionGate>,
    token: crate::open_api::OpenApiShutdownToken,
}
impl RegisteredEndpoint {
    pub(super) fn begin_close(&self) {
        self.token.begin_shutdown();
    }
    pub(super) fn begin_shutdown(&self) {
        if let Some(gate) = self.gate.upgrade() {
            gate.retire();
        }
        self.token.begin_shutdown();
    }
    pub(super) fn wait(&self, timeout: Duration) -> Result<bool, BackendError> {
        self.token
            .wait(timeout)
            .map_err(|error| BackendError::new(ErrorKind::OperationFailed, error.message))
    }
}

/// A revocable session whose requests are checked by the player command owner.
pub struct BackendExternalSession {
    backend: Weak<BackendInner>,
    pub(super) gate: Arc<ExternalSessionGate>,
    sequence: AtomicU64,
}

impl BackendExternalSession {
    pub fn epoch(&self) -> u64 {
        self.gate.epoch
    }
    pub fn retire(&self) {
        self.gate.retire();
    }
    fn check(&self) -> ControlResult<()> {
        if !*self
            .gate
            .alive
            .read()
            .unwrap_or_else(|error| error.into_inner())
        {
            return Err(ControlError::new(
                ControlErrorKind::Unauthorized,
                "External control session retired",
            ));
        }
        Ok(())
    }
    fn backend(&self) -> ControlResult<BackendHandle> {
        self.backend
            .upgrade()
            .map(|inner| BackendHandle { inner })
            .ok_or_else(|| ControlError::new(ControlErrorKind::Closed, "Player backend stopped"))
    }
}
impl Drop for BackendExternalSession {
    fn drop(&mut self) {
        self.retire();
    }
}

impl ExternalControlBackend for BackendExternalSession {
    fn snapshot(&self) -> ControlResult<StatusSnapshot> {
        self.check()?;
        let (reply, receiver) = mpsc::sync_channel(1);
        self.backend()?
            .send_query(RuntimeMessage::ExternalSnapshot {
                gate: self.gate.clone(),
                reply,
            })
            .map_err(control_error)?;
        receive(receiver)
    }
    fn updates_after(&self, cursor: u64) -> ControlResult<Vec<StatusUpdate>> {
        self.check()?;
        let (reply, receiver) = mpsc::sync_channel(1);
        self.backend()?
            .send_query(RuntimeMessage::ExternalUpdates {
                gate: self.gate.clone(),
                cursor,
                reply,
            })
            .map_err(control_error)?;
        receive(receiver)
    }
    fn artwork(&self, reference: ArtworkReference) -> ControlResult<ArtworkResponse> {
        self.check()?;
        reference.validate()?;
        if reference.session_epoch != self.gate.epoch {
            return Err(ControlError::new(
                ControlErrorKind::Unauthorized,
                "External artwork epoch changed",
            ));
        }
        let (reply, receiver) = mpsc::sync_channel(1);
        self.backend()?
            .send_query(RuntimeMessage::ExternalArtwork {
                gate: self.gate.clone(),
                reference,
                reply,
            })
            .map_err(control_error)?;
        receiver
            .recv_timeout(Duration::from_secs(6))
            .map_err(|error| {
                ControlError::new(
                    match error {
                        mpsc::RecvTimeoutError::Timeout => ControlErrorKind::Timeout,
                        mpsc::RecvTimeoutError::Disconnected => ControlErrorKind::Closed,
                    },
                    "External artwork owner did not acknowledge",
                )
            })?
    }
    fn control(&self, session_epoch: u64, action: ControlAction) -> ControlResult<()> {
        self.check()?;
        if session_epoch != self.gate.epoch {
            return Err(ControlError::new(
                ControlErrorKind::Unauthorized,
                "External control epoch changed",
            ));
        }
        let (reply, receiver) = mpsc::sync_channel(1);
        let sequence = self.sequence.fetch_add(1, Ordering::Relaxed);
        self.backend()?
            .send_query(RuntimeMessage::ExternalCommand {
                gate: self.gate.clone(),
                action,
                request_id: format!("external-{}-{sequence}", self.gate.epoch),
                reply,
            })
            .map_err(control_error)?;
        receive(receiver)
    }
}
fn receive<T>(receiver: mpsc::Receiver<Result<T, BackendError>>) -> ControlResult<T> {
    receiver
        .recv_timeout(Duration::from_secs(6))
        .map_err(|error| {
            ControlError::new(
                match error {
                    mpsc::RecvTimeoutError::Timeout => ControlErrorKind::Timeout,
                    mpsc::RecvTimeoutError::Disconnected => ControlErrorKind::Closed,
                },
                "External command owner did not acknowledge",
            )
        })?
        .map_err(control_error)
}
pub(super) fn control_error(error: BackendError) -> ControlError {
    let kind = match error.kind {
        ErrorKind::Closed | ErrorKind::Cancelled => ControlErrorKind::Closed,
        ErrorKind::Timeout => ControlErrorKind::Timeout,
        ErrorKind::Busy => ControlErrorKind::Busy,
        ErrorKind::CursorExpired => ControlErrorKind::CursorExpired,
        ErrorKind::Unavailable | ErrorKind::Storage | ErrorKind::OperationFailed => {
            ControlErrorKind::Unavailable
        }
        ErrorKind::InvalidData | ErrorKind::Conflict => ControlErrorKind::InvalidData,
    };
    ControlError::new(kind, error.message)
}

impl BackendHandle {
    pub fn external_control_session(&self) -> Result<Arc<BackendExternalSession>, BackendError> {
        let _opening = self
            .inner
            .external_session_open
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if self.inner.external_admission_closed.load(Ordering::Acquire) {
            return Err(BackendError::new(
                ErrorKind::Closed,
                "External admission is closed",
            ));
        }
        let (reply, receiver) = mpsc::sync_channel(1);
        let cancel = Arc::new(AtomicBool::new(false));
        self.send_query(RuntimeMessage::OpenExternalSession {
            cancel: cancel.clone(),
            reply,
        })?;
        let gate = receiver
            .recv_timeout(Duration::from_secs(6))
            .map_err(|_| {
                cancel.store(true, Ordering::Release);
                BackendError::new(
                    ErrorKind::Timeout,
                    "Cannot establish external control session",
                )
            })??;
        Ok(Arc::new(BackendExternalSession {
            backend: Arc::downgrade(&self.inner),
            gate,
            sequence: AtomicU64::new(1),
        }))
    }
    /// Starts an explicitly enabled endpoint. No listener is created at backend startup.
    pub fn start_open_api(&self, mut config: ServerConfig) -> ControlResult<BackendOpenApiServer> {
        config.validate()?;
        let mut endpoints = self
            .inner
            .external_endpoints
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        endpoints.retain(|endpoint| !matches!(endpoint.wait(Duration::ZERO), Ok(true)));
        if endpoints.len() >= 4 {
            return Err(ControlError::new(
                ControlErrorKind::Busy,
                "Retiring endpoint capacity reached",
            ));
        }
        let session = self.external_control_session().map_err(control_error)?;
        config.session_epoch = session.epoch();
        let server = OpenApiServer::start(config, session.clone())?;
        let endpoint = RegisteredEndpoint {
            gate: Arc::downgrade(&session.gate),
            token: server.shutdown_token(),
        };
        if self.inner.shutdown.load(Ordering::Acquire)
            || self.inner.external_admission_closed.load(Ordering::Acquire)
        {
            endpoint.begin_shutdown();
            endpoints.push(endpoint);
            return Err(ControlError::new(
                ControlErrorKind::Closed,
                "Backend shut down while endpoint started",
            ));
        }
        endpoints.push(endpoint);
        Ok(BackendOpenApiServer { server, session })
    }
}

/// Retires the actor session before closing sockets or waiting for server workers.
pub struct BackendOpenApiServer {
    server: OpenApiServer,
    session: Arc<BackendExternalSession>,
}
impl BackendOpenApiServer {
    pub fn view(&self) -> ServerView {
        self.server.view()
    }
    pub fn try_recv(&mut self) -> Option<ServerEvent> {
        self.server.try_recv()
    }
    pub fn epoch(&self) -> u64 {
        self.session.epoch()
    }
    pub fn shutdown(&mut self, timeout: Duration) -> ControlResult<bool> {
        self.session.retire();
        self.server.shutdown(timeout)
    }
    pub fn is_finished(&self) -> bool {
        self.server.is_finished()
    }
}
impl Drop for BackendOpenApiServer {
    fn drop(&mut self) {
        self.session.retire();
    }
}

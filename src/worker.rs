//! Process boundary for LX script validation and explicitly approved execution.
//!
//! The executable runs its own `--source-worker` entry point. No third-party
//! executable is launched. Every operation has a deadline, pipes are bounded,
//! and Windows workers additionally have a 512 MiB / one-process Job limit.

use crate::domain::lyric::LyricBundle;
use crate::source::{
    ImportedSource, MAX_SCRIPT_BYTES, RuntimeManifest, SourceHostEvent, SourceRequest,
    SourceRequestCancelled, SourceResult, SourceSession,
};
use anyhow::{Context, Result, anyhow, bail, ensure};
use serde::{Deserialize, Serialize};
use std::{
    io::{self, BufRead, BufReader, Write},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{self, Receiver, RecvTimeoutError, SyncSender},
    },
    thread,
    time::{Duration, Instant},
};

pub const VALIDATION_TIMEOUT: Duration = Duration::from_secs(5);
/// Covers the runtime's 70 s asynchronous operation budget plus IPC teardown.
/// Runtime CPU budgets remain separate; UI/controller cancellation kills immediately.
pub const OPERATION_TIMEOUT: Duration = Duration::from_secs(75);
const MAX_PROTOCOL_BYTES: usize = 2 * 1024 * 1024;

#[derive(Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum WorkerRequest {
    Validate {
        script: String,
    },
    ValidateFile {
        path: PathBuf,
    },
    Init {
        script: String,
    },
    Request {
        request: SourceRequest,
    },
    RequestCancellable {
        request: SourceRequest,
        operation_id: u64,
    },
    CancelOperation {
        operation_id: u64,
    },
}

impl WorkerRequest {
    fn deadline(&self) -> Duration {
        match self {
            Self::Validate { .. } | Self::ValidateFile { .. } => VALIDATION_TIMEOUT,
            Self::Init { .. } | Self::Request { .. } | Self::RequestCancellable { .. } => {
                OPERATION_TIMEOUT
            }
            Self::CancelOperation { .. } => Duration::ZERO,
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum WorkerResponse {
    Validated {
        source: ImportedSource,
    },
    Initialized {
        manifest: RuntimeManifest,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        host_events: Vec<SourceHostEvent>,
    },
    Resolved {
        url: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        host_events: Vec<SourceHostEvent>,
    },
    Lyrics {
        lyrics: LyricBundle,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        host_events: Vec<SourceHostEvent>,
    },
    Error {
        message: String,
        #[serde(default)]
        failure: Option<crate::source::SourceFailureKind>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        host_events: Vec<SourceHostEvent>,
    },
    Cancelled,
}
impl WorkerResponse {
    fn error(message: impl Into<String>) -> Self {
        Self::Error {
            message: message.into(),
            failure: None,
            host_events: Vec::new(),
        }
    }
}

/// Imports unknown JavaScript in a short-lived, bounded process without evaluating it.
/// A parser stack overflow or native parser stall cannot bring down the controller.
pub fn validate_source(script: &str) -> Result<ImportedSource> {
    ensure!(
        script.len() <= MAX_SCRIPT_BYTES,
        "Source script exceeds the 1 MiB import limit"
    );
    let mut worker = SourceWorker::spawn()?;
    match worker.exchange(&WorkerRequest::Validate {
        script: script.into(),
    })? {
        WorkerResponse::Validated { source } => {
            ensure!(
                !source.approved && source.script == script,
                "Source validator returned unexpected content or execution approval"
            );
            source.verify_integrity()?;
            Ok(source)
        }
        WorkerResponse::Error { message, .. } => {
            Err(anyhow!("Source validation failed: {message}"))
        }
        _ => {
            worker.cancel.terminate();
            bail!("Source worker returned an unexpected validation response");
        }
    }
}

enum PipeEvent {
    Response(Vec<u8>),
    Failed(String),
}

/// UI threads can kill a source while its resolver is waiting in another thread.
#[derive(Clone)]
pub struct WorkerCancel {
    child: Arc<Mutex<Child>>,
    terminated: Arc<AtomicBool>,
}

impl WorkerCancel {
    /// Idempotent and intentionally does not wait for any pending IPC response.
    pub fn terminate(&self) {
        self.terminated.store(true, Ordering::Release);
        let mut child = self.child.lock().unwrap_or_else(|error| error.into_inner());
        let _ = child.kill();
    }
}

pub struct SourceWorker {
    cancel: WorkerCancel,
    outgoing: Option<SyncSender<Vec<u8>>>,
    incoming: Receiver<PipeEvent>,
    manifest: RuntimeManifest,
    initialized: bool,
    host_events: std::collections::VecDeque<SourceHostEvent>,
    operation_serial: u64,
    // Closing the Job also kills a worker if ordinary process termination fails.
    _job: WorkerJob,
}

impl SourceWorker {
    pub fn is_initialized(&self) -> bool {
        self.initialized
    }
    pub fn is_stopped(&self) -> bool {
        if self.cancel.terminated.load(Ordering::Acquire) {
            return true;
        }
        self.cancel
            .child
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .try_wait()
            .map_or(true, |status| status.is_some())
    }
    /// Reads one explicitly selected script inside the cancellable validation process.
    /// The file path is never exposed as a JavaScript filesystem capability.
    pub fn validate_file(&mut self, path: &Path) -> Result<ImportedSource> {
        ensure!(path.is_absolute(), "Source path must be absolute");
        match self.exchange(&WorkerRequest::ValidateFile { path: path.into() })? {
            WorkerResponse::Validated { source } => {
                ensure!(
                    !source.approved,
                    "Static file validation must not approve execution"
                );
                source.verify_integrity()?;
                Ok(source)
            }
            WorkerResponse::Error { message, .. } => {
                bail!("Source file validation failed: {message}")
            }
            _ => bail!("Unexpected source file validation response"),
        }
    }
    /// Call only after recording the user's approval of these exact script bytes.
    pub fn start(script: &str) -> Result<Self> {
        let mut worker = Self::prepare()?;
        worker.initialize(script)?;
        Ok(worker)
    }

    /// Starts only the bounded transport. Record its cancel handle before executing JS.
    pub fn prepare() -> Result<Self> {
        Self::spawn()
    }

    /// Syntax-only validation on an already prepared, cancellable worker.
    pub fn validate_script(&mut self, script: &str) -> Result<ImportedSource> {
        ensure!(
            script.len() <= MAX_SCRIPT_BYTES,
            "Source script exceeds the 1 MiB import limit"
        );
        match self.exchange(&WorkerRequest::Validate {
            script: script.into(),
        })? {
            WorkerResponse::Validated { source } => {
                ensure!(
                    !source.approved && source.script == script,
                    "Unexpected validated source content or approval"
                );
                source.verify_integrity()?;
                Ok(source)
            }
            WorkerResponse::Error { message, .. } => bail!("Source validation failed: {message}"),
            _ => bail!("Unexpected source validation response"),
        }
    }

    /// Initializes on a resolver thread; the controller already owns its cancel handle.
    pub fn initialize(&mut self, script: &str) -> Result<()> {
        ensure!(
            script.len() <= MAX_SCRIPT_BYTES,
            "Source script exceeds the 1 MiB worker limit"
        );
        ensure!(
            !self.initialized,
            "Source worker is already initialized; prepare a new worker to switch sources"
        );
        let response = self.exchange(&WorkerRequest::Init {
            script: script.into(),
        })?;
        match response {
            WorkerResponse::Initialized { manifest, .. } => {
                self.manifest = manifest;
                self.initialized = true;
            }
            WorkerResponse::Error { message, .. } => {
                bail!("Source initialization failed: {message}")
            }
            _ => {
                self.cancel.terminate();
                bail!("Source worker returned an unexpected initialization response");
            }
        }
        Ok(())
    }

    /// Creates only the transport/process boundary; it does not parse or evaluate a script.
    fn spawn() -> Result<Self> {
        let current = std::env::current_exe().context("Cannot locate this player's executable")?;
        let executable = resolve_worker_executable(&current)?;
        let mut command = Command::new(executable);
        command
            .arg("--source-worker")
            .env_clear()
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        // Windows needs this to locate system DLLs; no user environment is exposed.
        if let Some(system_root) = std::env::var_os("SystemRoot") {
            command.env("SystemRoot", system_root);
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            use windows_sys::Win32::System::Threading::CREATE_NO_WINDOW;
            command.creation_flags(CREATE_NO_WINDOW);
        }
        let mut child = command
            .spawn()
            .context("Cannot start isolated source worker")?;
        let job = match WorkerJob::attach(&child) {
            Ok(job) => job,
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error);
            }
        };
        let stdin = child.stdin.take().context("Worker stdin is unavailable")?;
        let stdout = child
            .stdout
            .take()
            .context("Worker stdout is unavailable")?;
        let cancel = WorkerCancel {
            child: Arc::new(Mutex::new(child)),
            terminated: Arc::new(AtomicBool::new(false)),
        };
        let (events_tx, incoming) = mpsc::sync_channel(1);
        let (outgoing, messages_rx) = mpsc::sync_channel::<Vec<u8>>(1);

        let writer_events = events_tx.clone();
        if let Err(error) = thread::Builder::new()
            .name("lx-source-ipc-write".into())
            .spawn(move || {
                let mut stdin = stdin;
                while let Ok(message) = messages_rx.recv() {
                    if let Err(error) = stdin.write_all(&message).and_then(|_| stdin.flush()) {
                        let _ = writer_events.send(PipeEvent::Failed(format!(
                            "Source worker write failed: {error}"
                        )));
                        break;
                    }
                }
            })
        {
            cancel.terminate();
            return Err(error).context("Cannot start source IPC writer");
        }
        if let Err(error) = thread::Builder::new()
            .name("lx-source-ipc-read".into())
            .spawn(move || {
                let mut reader = BufReader::new(stdout);
                loop {
                    let event = match read_bounded_line(&mut reader, MAX_PROTOCOL_BYTES) {
                        Ok(Some(line)) => PipeEvent::Response(line),
                        Ok(None) => {
                            PipeEvent::Failed("Source worker exited or closed its output".into())
                        }
                        Err(error) => {
                            PipeEvent::Failed(format!("Source worker output rejected: {error}"))
                        }
                    };
                    let finished = matches!(event, PipeEvent::Failed(_));
                    if events_tx.send(event).is_err() || finished {
                        break;
                    }
                }
            })
        {
            cancel.terminate();
            return Err(error).context("Cannot start source IPC reader");
        }
        Ok(Self {
            cancel,
            outgoing: Some(outgoing),
            incoming,
            manifest: RuntimeManifest {
                sources: Default::default(),
            },
            initialized: false,
            host_events: std::collections::VecDeque::new(),
            operation_serial: 0,
            _job: job,
        })
    }

    pub fn manifest(&self) -> &RuntimeManifest {
        &self.manifest
    }

    /// Advisory source metadata only; the host decides whether to display it.
    pub fn take_host_events(&mut self) -> Vec<SourceHostEvent> {
        self.host_events.drain(..).collect()
    }

    pub fn cancel_handle(&self) -> WorkerCancel {
        self.cancel.clone()
    }

    pub fn request(&mut self, request: &SourceRequest) -> Result<String> {
        self.request_result(request)?.into_url()
    }

    pub fn request_result(&mut self, request: &SourceRequest) -> Result<SourceResult> {
        let response = self.exchange(&WorkerRequest::Request {
            request: request.clone(),
        })?;
        self.decode_result(request, response)
    }

    /// Cancels only this request, preserving the initialized JavaScript session.
    /// Cancellation is cooperative while JavaScript executes synchronous code.
    /// The cancellation response acknowledges HTTP, timer and Promise cleanup
    /// before the same session is available to the next request.
    pub fn request_cancellable(
        &mut self,
        request: &SourceRequest,
        cancelled: impl Fn() -> bool,
    ) -> Result<SourceResult> {
        if cancelled() {
            return Err(SourceRequestCancelled.into());
        }
        self.operation_serial = self
            .operation_serial
            .checked_add(1)
            .context("Source operation identifier exhausted")?;
        let operation_id = self.operation_serial;
        let response = self.exchange_cancellable(
            &WorkerRequest::RequestCancellable {
                request: request.clone(),
                operation_id,
            },
            Some((&cancelled, operation_id)),
        )?;
        if cancelled() {
            return Err(SourceRequestCancelled.into());
        }
        self.decode_result(request, response)
    }

    pub fn request_url_cancellable(
        &mut self,
        request: &SourceRequest,
        cancelled: impl Fn() -> bool,
    ) -> Result<String> {
        self.request_cancellable(request, cancelled)?.into_url()
    }

    fn decode_result(
        &mut self,
        request: &SourceRequest,
        response: WorkerResponse,
    ) -> Result<SourceResult> {
        match response {
            WorkerResponse::Resolved { url, .. } => {
                ensure!(
                    request.action != "lyric",
                    "Source worker returned a URL for a lyric request"
                );
                Ok(SourceResult::Url(url))
            }
            WorkerResponse::Lyrics { lyrics, .. } => {
                ensure!(
                    request.action == "lyric" && request.source == "local",
                    "Source worker returned lyrics for a URL request"
                );
                crate::source::validate_source_lyrics(&lyrics)?;
                Ok(SourceResult::Lyrics(lyrics))
            }
            WorkerResponse::Error {
                message, failure, ..
            } => match failure {
                Some(kind) => Err(crate::source::SourceRequestFailure { kind, message }.into()),
                None => Err(anyhow!(message)),
            },
            WorkerResponse::Cancelled => Err(SourceRequestCancelled.into()),
            _ => {
                self.cancel.terminate();
                bail!("Source worker returned an unexpected request response");
            }
        }
    }

    fn exchange(&mut self, request: &WorkerRequest) -> Result<WorkerResponse> {
        self.exchange_cancellable(request, None)
    }

    fn exchange_cancellable(
        &mut self,
        request: &WorkerRequest,
        cancellation: Option<(&dyn Fn() -> bool, u64)>,
    ) -> Result<WorkerResponse> {
        ensure!(
            !self.cancel.terminated.load(Ordering::Acquire),
            "Source worker has been stopped; enable the source again"
        );
        let mut message =
            serde_json::to_vec(request).context("Cannot encode source worker request")?;
        ensure!(
            message.len() <= MAX_PROTOCOL_BYTES,
            "Source worker request exceeds the 2 MiB IPC limit"
        );
        message.push(b'\n');
        let deadline = request.deadline();
        let started = Instant::now();
        loop {
            if cancellation.is_some_and(|(cancelled, _)| cancelled()) {
                return Err(SourceRequestCancelled.into());
            }
            ensure!(
                !self.cancel.terminated.load(Ordering::Acquire),
                "Source worker has been stopped"
            );
            let sender = self
                .outgoing
                .as_ref()
                .context("Source worker input is closed")?;
            match sender.try_send(message) {
                Ok(()) => break,
                Err(mpsc::TrySendError::Full(pending)) => message = pending,
                Err(mpsc::TrySendError::Disconnected(_)) => {
                    self.cancel.terminate();
                    bail!("Source worker input channel closed");
                }
            }
            if started.elapsed() >= deadline {
                self.cancel.terminate();
                bail!("Source worker input exceeded its operation deadline");
            }
            thread::sleep(Duration::from_millis(1));
        }
        let mut cancellation_sent = false;
        let incoming = loop {
            if let Some((cancelled, operation_id)) = cancellation
                && !cancellation_sent
                && cancelled()
            {
                let mut message =
                    serde_json::to_vec(&WorkerRequest::CancelOperation { operation_id })?;
                message.push(b'\n');
                match self
                    .outgoing
                    .as_ref()
                    .context("Source worker input is closed")?
                    .try_send(message)
                {
                    Ok(()) => cancellation_sent = true,
                    Err(mpsc::TrySendError::Full(_)) => {}
                    Err(mpsc::TrySendError::Disconnected(_)) => {
                        break Err(RecvTimeoutError::Disconnected);
                    }
                }
            }
            let remaining = deadline.saturating_sub(started.elapsed());
            if remaining.is_zero() {
                break Err(RecvTimeoutError::Timeout);
            }
            let poll = if cancellation.is_some() {
                remaining.min(Duration::from_millis(5))
            } else {
                remaining
            };
            match self.incoming.recv_timeout(poll) {
                Err(RecvTimeoutError::Timeout) if started.elapsed() < deadline => {}
                result => break result,
            }
        };
        let response = match incoming {
            Ok(PipeEvent::Response(line)) => {
                serde_json::from_slice(&line).context("Source worker returned invalid JSON")
            }
            Ok(PipeEvent::Failed(message)) => Err(anyhow!(message)),
            Err(RecvTimeoutError::Timeout) => Err(anyhow!(
                "Source worker exceeded its {} second deadline and was terminated; retry with a new worker to recover",
                deadline.as_secs()
            )),
            Err(RecvTimeoutError::Disconnected) => {
                Err(anyhow!("Source worker response channel closed"))
            }
        };
        if response.is_err() {
            self.cancel.terminate();
        }
        if response.is_ok()
            && (cancellation_sent || cancellation.is_some_and(|(cancelled, _)| cancelled()))
        {
            return Ok(WorkerResponse::Cancelled);
        }
        if let Ok(response) = &response {
            let events: &[SourceHostEvent] = match response {
                WorkerResponse::Initialized { host_events, .. }
                | WorkerResponse::Resolved { host_events, .. }
                | WorkerResponse::Lyrics { host_events, .. }
                | WorkerResponse::Error { host_events, .. } => host_events,
                _ => &[],
            };
            for event in events {
                if self.host_events.len() >= 64 {
                    self.host_events.pop_front();
                }
                self.host_events.push_back(event.clone());
            }
        }
        response
    }
}

/// Cargo integration tests run in `target/<profile>/deps/<test>-<hash>`.
/// Only debug builds in this exact layout may use Cargo's adjacent player binary;
/// no environment override, PATH lookup or alternative executable search exists.
fn resolve_worker_executable(current: &Path) -> Result<PathBuf> {
    #[cfg(debug_assertions)]
    if let Some(deps) = current
        .parent()
        .filter(|parent| parent.file_name() == Some(std::ffi::OsStr::new("deps")))
    {
        let profile = deps
            .parent()
            .context("Cargo test executable has no profile directory")?;
        let name = if cfg!(windows) {
            "lx-native-cli.exe"
        } else {
            "lx-native-cli"
        };
        let candidate = profile.join(name);
        ensure!(
            candidate.is_file(),
            "Cargo's compiled player executable is missing: {}",
            candidate.display()
        );
        return Ok(candidate);
    }
    Ok(current.to_path_buf())
}

impl Drop for SourceWorker {
    fn drop(&mut self) {
        self.cancel.terminate();
        self.outgoing.take();
        let mut child = self
            .cancel
            .child
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let _ = child.wait();
    }
}

/// Put this before GPUI or any other application initialization in `main`.
/// Returns `true` when the process handled its private worker entry point.
pub fn worker_main() -> bool {
    if std::env::args_os().nth(1).as_deref() != Some(std::ffi::OsStr::new("--source-worker")) {
        return false;
    }
    let _ = run_worker();
    true
}

fn validate_selected_file(path: &Path) -> Result<ImportedSource> {
    use std::io::Read;
    ensure!(path.is_absolute(), "Source path must be absolute");
    let file = std::fs::File::open(path).context("Cannot read selected source file")?;
    ensure!(
        file.metadata()?.is_file(),
        "Selected source is not a regular file"
    );
    let mut bytes = Vec::new();
    file.take((MAX_SCRIPT_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= MAX_SCRIPT_BYTES, "Source file exceeds 1 MiB");
    let script = String::from_utf8(bytes).context("Source file must contain UTF-8 JavaScript")?;
    crate::source::import_script(&script)
}

fn run_worker() -> Result<()> {
    let stdout = io::stdout();
    let mut writer = stdout.lock();
    let mut session: Option<SourceSession> = None;
    let cancelled_through = Arc::new(AtomicU64::new(0));
    let cancellation = cancelled_through.clone();
    let (incoming, requests) = mpsc::sync_channel(1);
    thread::Builder::new()
        .name("lx-source-command-read".into())
        .spawn(move || {
            let stdin = io::stdin();
            let mut reader = BufReader::new(stdin.lock());
            loop {
                let request = match read_bounded_line(&mut reader, MAX_PROTOCOL_BYTES) {
                    Ok(Some(line)) => serde_json::from_slice::<WorkerRequest>(&line)
                        .map_err(|error| anyhow!("Invalid source worker request: {error}")),
                    Ok(None) => break,
                    Err(error) => {
                        let _ = incoming.send(Err(error.into()));
                        break;
                    }
                };
                if let Ok(WorkerRequest::CancelOperation { operation_id }) = request {
                    if operation_id > 0 {
                        cancellation.fetch_max(operation_id, Ordering::AcqRel);
                    }
                } else if incoming.send(request).is_err() {
                    break;
                }
            }
            cancellation.store(u64::MAX, Ordering::Release);
        })
        .context("Cannot start source command reader")?;
    let mut previous_operation = 0;
    while let Ok(request) = requests.recv() {
        let response = match request {
            Ok(WorkerRequest::Validate { script }) => match crate::source::import_script(&script) {
                Ok(source) => WorkerResponse::Validated { source },
                Err(error) => WorkerResponse::error(format!("{error:#}")),
            },
            Ok(WorkerRequest::ValidateFile { path }) => match validate_selected_file(&path) {
                Ok(source) => WorkerResponse::Validated { source },
                Err(error) => WorkerResponse::error(format!("{error:#}")),
            },
            Ok(WorkerRequest::Init { script }) if session.is_none() => {
                match SourceSession::new_authorized(&script) {
                    Ok(mut created) => {
                        let manifest = created.manifest().clone();
                        let host_events = created.take_host_events();
                        session = Some(created);
                        WorkerResponse::Initialized {
                            manifest,
                            host_events,
                        }
                    }
                    Err(error) => WorkerResponse::error(format!("{error:#}")),
                }
            }
            Ok(WorkerRequest::Init { .. }) => WorkerResponse::error(
                "A worker can initialize only one source; start a new worker to switch sources",
            ),
            Ok(WorkerRequest::Request { request }) => match session.as_mut() {
                Some(session) => {
                    let result = session.request_result(&request);
                    let host_events = session.take_host_events();
                    match result {
                        Ok(SourceResult::Url(url)) => WorkerResponse::Resolved { url, host_events },
                        Ok(SourceResult::Lyrics(lyrics)) => WorkerResponse::Lyrics {
                            lyrics,
                            host_events,
                        },
                        Err(error) => WorkerResponse::Error {
                            failure: error
                                .downcast_ref::<crate::source::SourceRequestFailure>()
                                .map(|error| error.kind),
                            message: format!("{error:#}"),
                            host_events,
                        },
                    }
                }
                None => WorkerResponse::error("Source worker has not initialized successfully"),
            },
            Ok(WorkerRequest::RequestCancellable {
                request,
                operation_id,
            }) => {
                if operation_id == 0 || operation_id <= previous_operation {
                    WorkerResponse::error("Source operation identifier is stale or zero")
                } else {
                    previous_operation = operation_id;
                    match session.as_mut() {
                        Some(session) => {
                            let result = session.request_result_cancellable(
                                &request,
                                cancelled_through.clone(),
                                operation_id,
                            );
                            let host_events = session.take_host_events();
                            match result {
                                Ok(SourceResult::Url(url)) => {
                                    WorkerResponse::Resolved { url, host_events }
                                }
                                Ok(SourceResult::Lyrics(lyrics)) => WorkerResponse::Lyrics {
                                    lyrics,
                                    host_events,
                                },
                                Err(error) if error.is::<SourceRequestCancelled>() => {
                                    WorkerResponse::Cancelled
                                }
                                Err(error) => WorkerResponse::Error {
                                    failure: error
                                        .downcast_ref::<crate::source::SourceRequestFailure>()
                                        .map(|error| error.kind),
                                    message: format!("{error:#}"),
                                    host_events,
                                },
                            }
                        }
                        None => {
                            WorkerResponse::error("Source worker has not initialized successfully")
                        }
                    }
                }
            }
            Ok(WorkerRequest::CancelOperation { .. }) => {
                unreachable!("cancellation is consumed by the command reader")
            }
            Err(error) => WorkerResponse::error(format!("{error:#}")),
        };
        let encoded = serde_json::to_vec(&response)?;
        ensure!(
            encoded.len() <= MAX_PROTOCOL_BYTES,
            "Source worker response exceeds the 2 MiB IPC limit"
        );
        writer.write_all(&encoded)?;
        writer.write_all(b"\n")?;
        writer.flush()?;
    }
    Ok(())
}

/// `read_line` alone can allocate indefinitely before it encounters a newline.
fn read_bounded_line(reader: &mut impl BufRead, limit: usize) -> io::Result<Option<Vec<u8>>> {
    let mut line = Vec::new();
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            return if line.is_empty() {
                Ok(None)
            } else {
                Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "Incomplete JSON IPC line",
                ))
            };
        }
        let newline = available.iter().position(|byte| *byte == b'\n');
        let count = newline.map_or(available.len(), |position| position + 1);
        if line.len().saturating_add(count) > limit.saturating_add(1) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "JSON IPC line exceeds its size limit",
            ));
        }
        line.extend_from_slice(&available[..count]);
        reader.consume(count);
        if newline.is_some() {
            line.pop();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            return Ok(Some(line));
        }
    }
}

#[cfg(windows)]
struct WorkerJob {
    _handle: std::os::windows::io::OwnedHandle,
}

#[cfg(windows)]
impl WorkerJob {
    fn attach(child: &Child) -> Result<Self> {
        use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
        use windows_sys::Win32::System::JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_ACTIVE_PROCESS,
            JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOB_OBJECT_LIMIT_PROCESS_MEMORY,
            JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
            SetInformationJobObject,
        };
        // SAFETY: null security/name pointers create an unnamed, non-inherited Job.
        let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        ensure!(
            !handle.is_null(),
            "Cannot create bounded source worker Job: {}",
            io::Error::last_os_error()
        );
        // SAFETY: CreateJobObjectW returned a unique valid owned handle.
        let owned = unsafe { OwnedHandle::from_raw_handle(handle) };
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_ACTIVE_PROCESS
            | JOB_OBJECT_LIMIT_PROCESS_MEMORY
            | JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        limits.BasicLimitInformation.ActiveProcessLimit = 1;
        limits.ProcessMemoryLimit = 512 * 1024 * 1024;
        // SAFETY: the pointer and size describe the exact structure expected by this information class.
        let configured = unsafe {
            SetInformationJobObject(
                handle,
                JobObjectExtendedLimitInformation,
                (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                std::mem::size_of_val(&limits) as u32,
            )
        };
        ensure!(
            configured != 0,
            "Cannot configure source worker memory/process limits: {}",
            io::Error::last_os_error()
        );
        // SAFETY: both handles remain valid for this call. Owned Job handle outlives the worker.
        let attached = unsafe { AssignProcessToJobObject(handle, child.as_raw_handle()) };
        ensure!(
            attached != 0,
            "Cannot attach source worker to its bounded Job: {}",
            io::Error::last_os_error()
        );
        Ok(Self { _handle: owned })
    }
}

#[cfg(not(windows))]
struct WorkerJob;

#[cfg(not(windows))]
impl WorkerJob {
    fn attach(_: &Child) -> Result<Self> {
        Ok(Self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn ipc_lines_are_bounded_even_without_newline() {
        let mut reader = Cursor::new(vec![b'x'; 17]);
        assert!(read_bounded_line(&mut reader, 16).is_err());
    }

    #[test]
    fn ipc_accepts_exact_limit_and_multiple_lines() {
        let mut reader = Cursor::new(b"1234\n{}\r\n".to_vec());
        assert_eq!(read_bounded_line(&mut reader, 4).unwrap().unwrap(), b"1234");
        assert_eq!(read_bounded_line(&mut reader, 4).unwrap().unwrap(), b"{}");
        assert!(read_bounded_line(&mut reader, 4).unwrap().is_none());
    }

    #[test]
    fn incomplete_ipc_is_rejected() {
        assert!(read_bounded_line(&mut Cursor::new(b"{}"), 4).is_err());
    }

    #[test]
    fn validation_has_short_deadline_and_async_runtime_has_bounded_network_budget() {
        assert_eq!(
            WorkerRequest::Validate {
                script: String::new()
            }
            .deadline(),
            Duration::from_secs(5)
        );
        assert_eq!(
            WorkerRequest::Init {
                script: String::new()
            }
            .deadline(),
            Duration::from_secs(75)
        );
        assert_eq!(
            WorkerRequest::Request {
                request: SourceRequest {
                    source: "local".into(),
                    action: "musicUrl".into(),
                    info: serde_json::Value::Null
                }
            }
            .deadline(),
            Duration::from_secs(75)
        );
    }

    #[test]
    fn request_waits_for_an_earlier_cancel_to_leave_the_bounded_input_queue() {
        let mut worker = SourceWorker::prepare().unwrap();
        worker.initialize("/* @name Bounded input fixture\n@version 1.0.0\n@author LX Native */\nlet calls=0;lx.on('request',async()=> 'http://127.0.0.1/fixture.wav?calls='+ ++calls);lx.send('inited',{sources:{kw:{name:'Fixture',type:'music',actions:['musicUrl'],qualitys:['128k']}}});").unwrap();
        let actual = worker.outgoing.take().unwrap();
        let (proxy, queued) = mpsc::sync_channel(1);
        let mut old_cancel =
            serde_json::to_vec(&WorkerRequest::CancelOperation { operation_id: 0 }).unwrap();
        old_cancel.push(b'\n');
        proxy.send(old_cancel).unwrap();
        worker.outgoing = Some(proxy);
        let forwarder = thread::spawn(move || {
            thread::sleep(Duration::from_millis(60));
            while let Ok(message) = queued.recv() {
                if actual.send(message).is_err() {
                    break;
                }
            }
        });
        let request = SourceRequest {
            source: "kw".into(),
            action: "musicUrl".into(),
            info: serde_json::json!({"type":"128k", "musicInfo":{}}),
        };
        let result = worker.request_url_cancellable(&request, || false).unwrap();
        assert!(result.contains("calls=1"));
        assert!(!worker.is_stopped());
        drop(worker);
        forwarder.join().unwrap();
    }

    #[test]
    fn packaged_executable_keeps_its_own_worker_entry_point() {
        let current = PathBuf::from("installed-player").join("lx-gpui-player.exe");
        assert_eq!(resolve_worker_executable(&current).unwrap(), current);
    }

    #[cfg(debug_assertions)]
    #[test]
    fn cargo_test_layout_uses_only_adjacent_compiled_app_and_rejects_missing_app() {
        let dir = tempfile::tempdir().unwrap();
        let profile = dir.path().join("debug");
        let deps = profile.join("deps");
        std::fs::create_dir_all(&deps).unwrap();
        let current = deps.join("ui-test-fixture.exe");
        assert!(resolve_worker_executable(&current).is_err());
        let name = if cfg!(windows) {
            "lx-native-cli.exe"
        } else {
            "lx-native-cli"
        };
        let candidate = profile.join(name);
        std::fs::write(&candidate, b"Path resolution fixture only; never executed").unwrap();
        assert_eq!(resolve_worker_executable(&current).unwrap(), candidate);
        // A similarly named directory does not authorize the Cargo-layout override.
        let non_cargo = profile.join("other-deps").join("ui-test-fixture.exe");
        assert_eq!(resolve_worker_executable(&non_cargo).unwrap(), non_cargo);
    }
}

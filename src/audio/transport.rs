//! Approved HTTP audio transport, independent of decoding and output callbacks.
use super::cache::{CacheLease, CacheOptions, CacheStats, CacheStore};
use super::http_client::{
    HTTP_DATA_FRAME_LIMIT, LimitedHttpClient as Client, LimitedResponse as Response,
};
use super::media_source::StreamMediaSource;
use super::worker_owner::{TrackedWorker, WorkerKind, WorkerPermit};
use anyhow::{Result, bail};
use reqwest::header::{CONTENT_ENCODING, CONTENT_RANGE, ETAG};
use reqwest::{StatusCode, Url};
use sha2::{Digest, Sha256};
use std::collections::VecDeque;
use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, Weak, mpsc};
use std::thread;
use std::time::{Duration, Instant};
use tokio::runtime::{Builder, Handle};
use tokio::sync::Notify;

const RUNTIME_START_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct CacheIdentity {
    pub track_id: String,
    pub platform: String,
    pub actual_quality: String,
    pub source_sha256: String,
    pub source_policy_version: u64,
    pub source_epoch: u64,
    pub url_sha256: String,
    pub approval_id: String,
    pub approval_epoch: u64,
    pub media_context_sha256: String,
}

/// Construction asserts caller approval of this exact URL; transport never resolves or redirects it.
#[derive(Clone)]
pub struct ApprovedAudioRequest {
    url: Url,
    pub identity: CacheIdentity,
}

impl fmt::Debug for ApprovedAudioRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ApprovedAudioRequest")
            .field("identity", &self.identity)
            .finish_non_exhaustive()
    }
}

impl ApprovedAudioRequest {
    pub fn new(url: &str, mut identity: CacheIdentity) -> Result<Self> {
        if url.len() > 2048 {
            bail!("audio URL exceeds 2048 bytes");
        }
        if url
            .bytes()
            .any(|byte| byte.is_ascii_control() || byte.is_ascii_whitespace())
        {
            bail!("audio URL contains whitespace or control characters");
        }
        let parsed = Url::parse(url).map_err(|_| anyhow::anyhow!("invalid audio URL"))?;
        let user_info = url.split_once("://").is_some_and(|(_, tail)| {
            tail.split(['/', '?', '#'])
                .next()
                .is_some_and(|authority| authority.contains('@'))
        });
        if !matches!(parsed.scheme(), "http" | "https")
            || parsed.host_str().is_none()
            || user_info
            || !parsed.username().is_empty()
            || parsed.password().is_some()
        {
            bail!("audio URL must be HTTP(S), with a host and no user information");
        }
        let digest = format!("{:x}", Sha256::digest(url.as_bytes()));
        if !identity.url_sha256.is_empty() && identity.url_sha256 != digest {
            bail!("approved audio URL digest mismatch");
        }
        identity.url_sha256 = digest;
        Ok(Self {
            url: parsed,
            identity,
        })
    }

    pub fn approved_url(url: &str) -> Result<Self> {
        Self::new(url, CacheIdentity::default())
    }

    fn cache_key(&self) -> String {
        let i = &self.identity;
        let mut hash = Sha256::new();
        for value in [
            &i.track_id,
            &i.platform,
            &i.actual_quality,
            &i.source_sha256,
            &i.url_sha256,
            &i.approval_id,
        ] {
            hash.update((value.len() as u64).to_le_bytes());
            hash.update(value.as_bytes());
        }
        for value in [i.source_policy_version, i.source_epoch, i.approval_epoch] {
            hash.update(value.to_le_bytes());
        }
        format!("{:x}", hash.finalize())
    }

    fn reusable_key(&self, block_size: usize) -> Option<String> {
        let identity = &self.identity;
        if identity.media_context_sha256.len() != 64
            || !identity
                .media_context_sha256
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return None;
        }
        let mut hash = Sha256::new();
        hash.update(b"lx-encoded-representation-v1");
        hash.update((block_size as u64).to_le_bytes());
        for value in [
            &identity.media_context_sha256,
            &identity.url_sha256,
            &identity.track_id,
            &identity.platform,
            &identity.actual_quality,
            &identity.source_sha256,
        ] {
            hash.update((value.len() as u64).to_le_bytes());
            hash.update(value.as_bytes());
        }
        hash.update(identity.source_policy_version.to_le_bytes());
        Some(format!("{:x}", hash.finalize()))
    }
}

#[derive(Clone, Debug)]
pub struct TransportOptions {
    pub cache: CacheOptions,
    pub connect_timeout: Duration,
    pub header_timeout: Duration,
    pub idle_timeout: Duration,
    pub max_retries: usize,
    pub sequential_lead_blocks: usize,
    /// Verified Range lookahead after the reader cursor; zero is demand-only.
    pub range_lookahead_blocks: usize,
}

impl Default for TransportOptions {
    fn default() -> Self {
        Self {
            cache: CacheOptions::default(),
            connect_timeout: Duration::from_secs(10),
            header_timeout: Duration::from_secs(20),
            idle_timeout: Duration::from_secs(20),
            max_retries: 2,
            sequential_lead_blocks: 2,
            range_lookahead_blocks: 2,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
pub enum MediaIoErrorKind {
    Cancelled,
    Timeout,
    Network,
    Protocol,
    Incomplete,
    RepresentationChanged,
    Storage,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct MediaIoError {
    pub kind: MediaIoErrorKind,
    pub message: String,
    pub http_status: Option<u16>,
}

impl fmt::Display for MediaIoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}
impl std::error::Error for MediaIoError {}

impl MediaIoError {
    pub(crate) fn new(kind: MediaIoErrorKind, message: &str) -> Self {
        Self {
            kind,
            message: message.into(),
            http_status: None,
        }
    }
    fn http(status: StatusCode) -> Self {
        Self {
            kind: MediaIoErrorKind::Network,
            message: format!("audio HTTP request returned status {}", status.as_u16()),
            http_status: Some(status.as_u16()),
        }
    }
    pub(crate) fn io_error(&self) -> std::io::Error {
        let kind = match self.kind {
            MediaIoErrorKind::Cancelled => std::io::ErrorKind::Interrupted,
            MediaIoErrorKind::Timeout => std::io::ErrorKind::TimedOut,
            MediaIoErrorKind::Protocol | MediaIoErrorKind::RepresentationChanged => {
                std::io::ErrorKind::InvalidData
            }
            MediaIoErrorKind::Incomplete => std::io::ErrorKind::UnexpectedEof,
            _ => std::io::ErrorKind::Other,
        };
        std::io::Error::new(kind, self.clone())
    }
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct StreamStatus {
    pub source_serial: u64,
    pub byte_len: Option<u64>,
    pub range_supported: bool,
    pub representation_epoch: u64,
    pub downloaded_bytes: u64,
    pub eof: bool,
    pub error: Option<MediaIoError>,
    pub cache: CacheStats,
    /// Cumulative application HTTP DATA bytes, including retries and metadata seeks.
    pub encoded_downloaded_bytes: u64,
    pub encoded_budget_bytes: Option<u64>,
    pub encoded_inflight_bytes: u64,
    pub budget_blocked: bool,
    pub reusable_registered: bool,
    pub cache_random_access: bool,
    pub cursor_bytes: u64,
    pub wanted_byte_offset: Option<u64>,
    pub published_blocks: u64,
    pub last_publication_micros: u64,
    pub max_publication_micros: u64,
    pub completed_range_loads: u64,
    pub last_range_load_micros: u64,
    pub max_range_load_micros: u64,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct LookaheadIdentity {
    pub offset: u64,
    pub read_epoch: u64,
    pub representation_epoch: u64,
}

pub(crate) struct StreamState {
    pub byte_len: Option<u64>,
    pub range: bool,
    pub remote_range: bool,
    pub epoch: u64,
    pub received: u64,
    pub eof: bool,
    pub eof_pending: bool,
    pub ready: bool,
    pub finished: bool,
    pub error: Option<MediaIoError>,
    pub sideband: Option<MediaIoError>,
    pub wanted: Option<u64>,
    pub cursor: u64,
    pub sequential_until: u64,
    pub strong_etag: Option<String>,
    pub change_pending: bool,
    pub budget_limit: Option<u64>,
    pub budget_used: u64,
    pub budget_reserved: u64,
    pub budget_blocked: bool,
    pub reusable_registered: bool,
    pub published_blocks: u64,
    pub last_publication_micros: u64,
    pub max_publication_micros: u64,
    pub completed_range_loads: u64,
    pub last_range_load_micros: u64,
    pub max_range_load_micros: u64,
    pub lookahead_suppressed_cursor: Option<u64>,
    pub lookahead_in_flight: Option<LookaheadIdentity>,
}

pub(crate) struct StreamShared {
    pending_publications: Mutex<VecDeque<PendingPublication>>,
    pub source_serial: u64,
    pub state: Mutex<StreamState>,
    pub changed: Condvar,
    pub notify: Notify,
    pub cancel: Arc<AtomicBool>,
    pub read_epoch: AtomicU64,
    pub cache: Arc<CacheStore>,
    pub object: String,
    pub block_size: u64,
    pub publication_read_timeout: Duration,
    pub reusable_key: Option<String>,
    pub cache_epoch: u64,
    _lease: CacheLease,
}
struct PendingPublication {
    ticket: super::cache::PublicationTicket,
    epoch: u64,
    #[cfg(test)]
    offset: u64,
    started: Instant,
}

impl StreamShared {
    pub fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Acquire)
    }
    pub fn wake(&self) {
        self.changed.notify_all();
        self.notify.notify_one();
    }
    pub(crate) fn error(&self, error: MediaIoError) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.sideband = Some(error.clone());
        state.error = Some(error);
        drop(state);
        self.wake();
    }

    pub(crate) fn error_if_current(
        &self,
        error: MediaIoError,
        representation_epoch: u64,
        read_epoch: u64,
    ) -> bool {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.epoch != representation_epoch
            || state.change_pending
            || self.read_epoch.load(Ordering::Acquire) != read_epoch
            || self.cancelled()
        {
            return false;
        }
        state.sideband = Some(error.clone());
        state.error = Some(error);
        drop(state);
        self.wake();
        true
    }
}

#[derive(Clone)]
pub struct StreamControl {
    pub(crate) shared: Arc<StreamShared>,
}

impl fmt::Debug for StreamControl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.status().fmt(f)
    }
}

impl StreamControl {
    pub fn status(&self) -> StreamStatus {
        let state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        StreamStatus {
            source_serial: self.shared.source_serial,
            byte_len: state.byte_len,
            range_supported: state.remote_range,
            representation_epoch: state.epoch,
            downloaded_bytes: state.received,
            eof: state.eof,
            error: state.error.clone(),
            cache: self.shared.cache.stats(),
            encoded_downloaded_bytes: state.budget_used,
            encoded_budget_bytes: state.budget_limit,
            encoded_inflight_bytes: state.budget_reserved,
            budget_blocked: state.budget_blocked,
            reusable_registered: state.reusable_registered,
            cache_random_access: state.range && !state.remote_range,
            cursor_bytes: state.cursor,
            wanted_byte_offset: state.wanted,
            published_blocks: state.published_blocks,
            last_publication_micros: state.last_publication_micros,
            max_publication_micros: state.max_publication_micros,
            completed_range_loads: state.completed_range_loads,
            last_range_load_micros: state.last_range_load_micros,
            max_range_load_micros: state.max_range_load_micros,
        }
    }

    pub(crate) fn terminal_error(&self) -> Option<MediaIoError> {
        let error = self
            .shared
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .error
            .clone();
        error.or_else(|| {
            self.shared
                .cache
                .publication_failed(&self.shared.object)
                .then(|| {
                    MediaIoError::new(
                        MediaIoErrorKind::Storage,
                        "write encoded audio cache failed",
                    )
                })
        })
    }

    /// Interrupts the current blocking read without poisoning later reads or seeks.
    pub fn interrupt_read(&self) {
        self.shared.read_epoch.fetch_add(1, Ordering::AcqRel);
        self.shared.wake();
    }

    /// The caller validates its prepared-source fence; this only lifts the same stream's byte limit.
    pub fn promote_to_foreground(&self) -> Result<()> {
        let mut state = self
            .shared
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let completed = state.eof
            && state
                .error
                .as_ref()
                .is_none_or(|error| error.kind == MediaIoErrorKind::Cancelled);
        if self.shared.cancelled() && !completed {
            bail!("cannot promote a cancelled audio stream");
        }
        if let Some(error) = &state.error
            && !completed
        {
            return Err(error.clone().into());
        }
        state.budget_limit = None;
        state.budget_blocked = false;
        drop(state);
        self.shared.wake();
        Ok(())
    }
    pub fn cancel(&self) {
        self.shared.cancel.store(true, Ordering::Release);
        self.interrupt_read();
    }
    pub fn take_error(&self) -> Option<MediaIoError> {
        self.shared
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .sideband
            .take()
    }
    pub fn wait_ready(&self, timeout: Duration) -> Result<()> {
        let deadline = Instant::now() + timeout;
        let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        while !state.ready {
            if let Some(error) = &state.error {
                return Err(error.clone().into());
            }
            if self.shared.cancelled() {
                bail!("audio transport cancelled");
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                bail!("audio transport readiness timeout");
            }
            state = self
                .shared
                .changed
                .wait_timeout(state, remaining.min(Duration::from_millis(25)))
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
        Ok(())
    }
    pub fn join(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        while !state.finished {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return false;
            }
            state = self
                .shared
                .changed
                .wait_timeout(state, remaining.min(Duration::from_millis(25)))
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
        true
    }
}

#[derive(Debug)]
pub struct StreamLease {
    pub reader: StreamMediaSource,
    pub control: StreamControl,
}

/// Initialization failures retain the native runtime owner for bounded retries.
#[derive(Debug)]
pub struct TransportInitError {
    message: String,
    owner: TrackedWorker,
}

impl TransportInitError {
    fn new(message: String, owner: TrackedWorker) -> Self {
        Self { message, owner }
    }

    pub fn worker_finished(&self) -> bool {
        self.owner.is_finished()
    }

    pub fn shutdown(&self, timeout: Duration) -> bool {
        self.owner.wait(timeout)
    }
}

impl fmt::Display for TransportInitError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for TransportInitError {}

struct RuntimeOwner {
    close: Arc<AtomicBool>,
    worker: TrackedWorker,
}

impl RuntimeOwner {
    fn start() -> Result<(Handle, Self)> {
        let permit = WorkerPermit::reserve(WorkerKind::TransportRuntime)?;
        let close = Arc::new(AtomicBool::new(false));
        let worker_close = close.clone();
        let (ready, receiver) = mpsc::sync_channel(1);
        let thread = thread::Builder::new()
            .name("audio-transport-owner".into())
            .spawn(move || {
                let runtime = match Builder::new_multi_thread()
                    .worker_threads(2)
                    .thread_name("audio-transport")
                    .enable_all()
                    .build()
                {
                    Ok(runtime) => runtime,
                    Err(error) => {
                        let _ = ready.try_send(Err(error.to_string()));
                        return;
                    }
                };
                if ready.try_send(Ok(runtime.handle().clone())).is_err() {
                    return;
                }
                while !worker_close.load(Ordering::Acquire) {
                    thread::park();
                }
                // Normal Runtime destruction waits for native runtime and
                // blocking workers on this already registered owner thread.
                drop(runtime);
            })?;
        let owner = Self {
            close,
            worker: permit.attach(thread),
        };
        let result = match receiver.recv_timeout(RUNTIME_START_TIMEOUT) {
            Ok(Ok(handle)) => return Ok((handle, owner)),
            Ok(Err(message)) => format!("Cannot initialize audio runtime: {message}"),
            Err(error) => format!("Audio runtime initialization did not complete: {error}"),
        };
        Err(TransportInitError::new(result, owner.worker.clone()).into())
    }

    fn begin_shutdown(&self) {
        self.close.store(true, Ordering::Release);
        self.worker.unpark();
    }
}

impl Drop for RuntimeOwner {
    fn drop(&mut self) {
        self.begin_shutdown();
    }
}

struct StreamCompletion(Arc<StreamShared>);

impl Drop for StreamCompletion {
    fn drop(&mut self) {
        let mut state = self
            .0
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        state.finished = true;
        drop(state);
        self.0.wake();
    }
}

pub struct AudioTransport {
    options: TransportOptions,
    cache: Arc<CacheStore>,
    client: Client,
    runtime: Mutex<Option<Handle>>,
    runtime_owner: RuntimeOwner,
    streams: Mutex<VecDeque<Weak<StreamShared>>>,
    next_stream: AtomicU64,
    closing: AtomicBool,
}

impl AudioTransport {
    pub fn new(options: TransportOptions) -> Result<Arc<Self>> {
        if options.connect_timeout.is_zero()
            || options.header_timeout.is_zero()
            || options.idle_timeout.is_zero()
            || options.sequential_lead_blocks == 0
            || options.range_lookahead_blocks > 2
            || options.max_retries > 8
        {
            bail!("invalid audio transport timeout or retry policy");
        }
        let client = Client::new(options.connect_timeout)?;
        let (runtime, runtime_owner) = RuntimeOwner::start()?;
        let cache = CacheStore::new(options.cache.clone()).map_err(|error| {
            TransportInitError::new(
                format!("Cannot initialize encoded cache: {error:#}"),
                runtime_owner.worker.clone(),
            )
        })?;
        Ok(Arc::new(Self {
            options,
            cache,
            client,
            runtime: Mutex::new(Some(runtime)),
            runtime_owner,
            streams: Mutex::new(VecDeque::new()),
            next_stream: AtomicU64::new(1),
            closing: AtomicBool::new(false),
        }))
    }

    pub fn cache(&self) -> &Arc<CacheStore> {
        &self.cache
    }

    pub fn outstanding_runtime_workers() -> usize {
        super::worker_owner::outstanding(WorkerKind::TransportRuntime)
    }

    pub fn open(
        self: &Arc<Self>,
        request: ApprovedAudioRequest,
        cancel: Arc<AtomicBool>,
    ) -> Result<StreamLease> {
        self.open_with_budget(request, cancel, None)
    }

    pub fn open_speculative(
        self: &Arc<Self>,
        request: ApprovedAudioRequest,
        cancel: Arc<AtomicBool>,
        encoded_budget_bytes: u64,
    ) -> Result<StreamLease> {
        if encoded_budget_bytes < self.options.cache.block_size as u64 {
            bail!("encoded preparation budget must admit at least one complete cache block");
        }
        self.open_with_budget(request, cancel, Some(encoded_budget_bytes))
    }

    fn open_with_budget(
        self: &Arc<Self>,
        request: ApprovedAudioRequest,
        cancel: Arc<AtomicBool>,
        budget_limit: Option<u64>,
    ) -> Result<StreamLease> {
        if self.closing.load(Ordering::Acquire) {
            bail!("audio transport is closed");
        }
        let serial = self.next_stream.fetch_add(1, Ordering::Relaxed);
        let object = format!("{}-{serial}", request.cache_key());
        let lease = self.cache.lease(object.clone())?;
        let shared = Arc::new(StreamShared {
            pending_publications: Mutex::new(VecDeque::new()),
            source_serial: serial,
            state: Mutex::new(StreamState {
                byte_len: None,
                range: false,
                remote_range: false,
                epoch: 0,
                received: 0,
                eof: false,
                eof_pending: false,
                ready: false,
                finished: false,
                error: None,
                sideband: None,
                wanted: Some(0),
                cursor: 0,
                sequential_until: 0,
                strong_etag: None,
                change_pending: false,
                budget_limit,
                budget_used: 0,
                budget_reserved: 0,
                budget_blocked: false,
                reusable_registered: false,
                published_blocks: 0,
                last_publication_micros: 0,
                max_publication_micros: 0,
                completed_range_loads: 0,
                last_range_load_micros: 0,
                max_range_load_micros: 0,
                lookahead_suppressed_cursor: None,
                lookahead_in_flight: None,
            }),
            changed: Condvar::new(),
            notify: Notify::new(),
            cancel,
            read_epoch: AtomicU64::new(0),
            cache: self.cache.clone(),
            object,
            block_size: self.options.cache.block_size as u64,
            publication_read_timeout: self.options.idle_timeout,
            reusable_key: request.reusable_key(self.options.cache.block_size),
            cache_epoch: self.cache.stats().epoch,
            _lease: lease,
        });
        // One worker owns the object's blocks; an epoch change invalidates all old intervals.
        let client = self.client.clone();
        let options = self.options.clone();
        let worker = shared.clone();
        let runtime = self.runtime.lock().unwrap_or_else(|e| e.into_inner());
        let Some(runtime) = runtime.as_ref() else {
            bail!("audio transport is closed");
        };
        if self.closing.load(Ordering::Acquire) {
            bail!("audio transport is closed");
        }
        // Registration shares the runtime admission lock with shutdown. Every
        // admitted task is visible before its runtime can be closed.
        let mut streams = self.streams.lock().unwrap_or_else(|e| e.into_inner());
        streams.retain(|value| value.strong_count() > 0);
        streams.push_back(Arc::downgrade(&shared));
        drop(streams);
        let completion = StreamCompletion(worker.clone());
        runtime.spawn(async move {
            let _completion = completion;
            if let Err(error) = run_stream(client, request, options, worker.clone()).await {
                worker.error(error);
            }
            if let Err(error) = finish_publications(&worker).await
                && !worker.cancelled()
            {
                worker.error(error);
            }
        });
        let control = StreamControl {
            shared: shared.clone(),
        };
        Ok(StreamLease {
            reader: StreamMediaSource::new(shared),
            control,
        })
    }

    pub fn shutdown(&self, timeout: Duration) -> bool {
        let started = Instant::now();
        self.closing.store(true, Ordering::Release);
        self.runtime
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take();
        let streams: Vec<_> = self
            .streams
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .filter_map(Weak::upgrade)
            .collect();
        for shared in &streams {
            StreamControl {
                shared: shared.clone(),
            }
            .cancel();
        }
        self.cache.begin_shutdown();
        self.runtime_owner.begin_shutdown();
        let streams_finished = streams
            .into_iter()
            .all(|shared| StreamControl { shared }.join(timeout.saturating_sub(started.elapsed())));
        let runtime_joined = self
            .runtime_owner
            .worker
            .wait(timeout.saturating_sub(started.elapsed()));
        let cache_joined = self
            .cache
            .shutdown(timeout.saturating_sub(started.elapsed()));
        streams_finished && runtime_joined && cache_joined
    }
}

impl Drop for AudioTransport {
    fn drop(&mut self) {
        self.closing.store(true, Ordering::Release);
        for shared in self
            .streams
            .get_mut()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .filter_map(Weak::upgrade)
        {
            StreamControl { shared }.cancel();
        }
        self.cache.begin_shutdown();
        self.runtime_owner.begin_shutdown();
    }
}

#[cfg(test)]
mod lifecycle_tests {
    use super::*;

    #[test]
    fn gated_representation_transition_keeps_old_ram_fenced_and_rejects_stale_eof_errors() {
        use std::fs::OpenOptions;
        use std::io::{Read, Seek, SeekFrom, Write};
        use std::net::TcpListener;

        let directory = tempfile::Builder::new()
            .prefix("lx-eof-generation-")
            .tempdir()
            .unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/audio", listener.local_addr().unwrap());
        let server = thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            socket
                .set_write_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut header = Vec::new();
            while !header.ends_with(b"\r\n\r\n") {
                let mut byte = [0];
                socket.read_exact(&mut byte).unwrap();
                header.push(byte[0]);
                assert!(header.len() <= 8192);
            }
            assert!(String::from_utf8(header).unwrap().contains("bytes=0-1023"));
            socket.write_all(b"HTTP/1.1 206 Partial Content\r\nConnection: close\r\nContent-Range: bytes 0-1023/1024\r\nContent-Length: 1024\r\nETag: \"native-generation\"\r\n\r\n").unwrap();
            socket.write_all(&[0x81; 1024]).unwrap();
        });
        let transport = AudioTransport::new(TransportOptions {
            cache: CacheOptions {
                root: directory.path().join("cache"),
                block_size: 1024,
                ram_bytes: 4096,
                disk_bytes: 32 * 1024 * 1024,
                max_objects: 4,
                max_blocks: 8,
            },
            range_lookahead_blocks: 0,
            ..TransportOptions::default()
        })
        .unwrap();
        transport.cache.wait_ready(Duration::from_secs(5)).unwrap();
        let gate = OpenOptions::new()
            .read(true)
            .write(true)
            .open(transport.cache.options().root.join(".lx-cache-owner.lock"))
            .unwrap();
        gate.lock().unwrap();
        let mut lease = transport
            .open(
                ApprovedAudioRequest::approved_url(&url).unwrap(),
                Arc::new(AtomicBool::new(false)),
            )
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while transport.cache.stats().pending_ram_blocks != 1
            || !transport.cache.stats().io_waiting_for_registry
        {
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(2));
        }
        let mut body = [0; 1024];
        lease.reader.read_exact(&mut body).unwrap();
        assert_eq!(body, [0x81; 1024]);
        assert_eq!(lease.reader.position(), 1024);
        let shared = lease.control.shared.clone();
        let old_read_epoch = shared.read_epoch.load(Ordering::Acquire);
        let state = shared.state.lock().unwrap();
        let old_representation_epoch = state.epoch;
        let (entered, ready) = mpsc::sync_channel(1);
        let (finished, result) = mpsc::sync_channel(1);
        let changed = shared.clone();
        let transition = thread::spawn(move || {
            entered.send(()).unwrap();
            finished.send(representation_changed(&changed)).unwrap();
        });
        ready.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(matches!(
            result.recv_timeout(Duration::from_millis(40)),
            Err(mpsc::RecvTimeoutError::Timeout)
        ));
        // The actual reader's state fence must also protect its still-visible
        // accepted RAM bytes from being removed by the transition.
        assert_eq!(transport.cache.stats().pending_ram_blocks, 1);
        let mut remaining = [0; 16];
        assert_eq!(
            transport
                .cache
                .read(&shared.object, 0, &mut remaining)
                .unwrap(),
            Some(16)
        );
        assert_eq!(remaining, [0x81; 16]);
        drop(state);
        result
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .unwrap();
        transition.join().unwrap();
        let error = lease.reader.read(&mut [0; 16]).unwrap_err();
        assert_eq!(
            error
                .get_ref()
                .unwrap()
                .downcast_ref::<MediaIoError>()
                .unwrap()
                .kind,
            MediaIoErrorKind::RepresentationChanged
        );
        lease.reader.seek(SeekFrom::Start(0)).unwrap();
        assert!(!shared.error_if_current(
            MediaIoError::new(MediaIoErrorKind::Storage, "old payload failure"),
            old_representation_epoch,
            old_read_epoch,
        ));
        let current_epoch = shared.state.lock().unwrap().epoch;
        assert!(!shared.error_if_current(
            MediaIoError::new(MediaIoErrorKind::Timeout, "old read deadline"),
            current_epoch,
            old_read_epoch,
        ));
        assert!(shared.state.lock().unwrap().error.is_none());
        assert_eq!(
            shared.state.lock().unwrap().sideband.as_ref().unwrap().kind,
            MediaIoErrorKind::RepresentationChanged
        );
        lease.control.cancel();
        gate.unlock().unwrap();
        drop(lease.reader);
        assert!(lease.control.join(Duration::from_secs(5)));
        assert!(transport.shutdown(Duration::from_secs(5)));
        server.join().unwrap();
    }

    struct ReleaseOnDrop(mpsc::SyncSender<()>);

    impl Drop for ReleaseOnDrop {
        fn drop(&mut self) {
            let _ = self.0.try_send(());
        }
    }

    fn blocked_runtime() -> (
        Handle,
        RuntimeOwner,
        mpsc::Receiver<()>,
        ReleaseOnDrop,
        tokio::task::JoinHandle<()>,
    ) {
        let (handle, owner) = RuntimeOwner::start().unwrap();
        let (entered, receiver) = mpsc::sync_channel(1);
        let (release, gate) = mpsc::sync_channel(1);
        let task = handle.spawn_blocking(move || {
            let _ = entered.send(());
            let _ = gate.recv();
        });
        (handle, owner, receiver, ReleaseOnDrop(release), task)
    }

    #[test]
    fn runtime_timeout_keeps_native_blocking_workers_owned_until_actual_join() {
        let (_handle, owner, entered, release, _task) = blocked_runtime();
        entered.recv_timeout(Duration::from_secs(1)).unwrap();
        owner.begin_shutdown();
        let started = Instant::now();
        assert!(!owner.worker.wait(Duration::from_millis(5)));
        assert!(started.elapsed() < Duration::from_millis(100));
        assert!(!owner.worker.is_finished());
        assert!(AudioTransport::outstanding_runtime_workers() >= 1);
        drop(release);
        assert!(owner.worker.wait(Duration::from_secs(2)));
        assert!(!owner.worker.panicked());
    }

    #[test]
    fn runtime_owner_drop_only_signals_and_keeps_native_teardown_owned() {
        let (_handle, owner, entered, release, _task) = blocked_runtime();
        entered.recv_timeout(Duration::from_secs(1)).unwrap();
        let tracked = owner.worker.clone();
        let closing = owner.close.clone();
        let started = Instant::now();
        drop(owner);
        assert!(started.elapsed() < Duration::from_millis(100));
        assert!(closing.load(Ordering::Acquire));
        assert!(!tracked.wait(Duration::ZERO));
        drop(release);
        assert!(tracked.wait(Duration::from_secs(2)));
    }

    #[test]
    fn real_cache_admission_failure_returns_a_runtime_owner_that_can_be_rejoined() {
        let options = TransportOptions {
            cache: CacheOptions {
                block_size: 0,
                ..CacheOptions::default()
            },
            ..TransportOptions::default()
        };
        let error = match AudioTransport::new(options) {
            Ok(_) => panic!("invalid cache quota must fail initialization"),
            Err(error) => error,
        };
        let initialization = error.downcast_ref::<TransportInitError>().unwrap();
        assert!(
            initialization
                .to_string()
                .contains("invalid encoded cache quota")
        );
        assert!(initialization.shutdown(Duration::from_secs(2)));
        assert!(initialization.worker_finished());
    }
}

async fn cancellation(shared: &StreamShared) {
    interruption(shared, None).await;
}

async fn interruption(shared: &StreamShared, read_epoch: Option<u64>) {
    loop {
        if shared.cancelled()
            || read_epoch.is_some_and(|epoch| shared.read_epoch.load(Ordering::Acquire) != epoch)
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

struct FetchRequest<'a> {
    interval: Option<(u64, Option<u64>)>,
    validator: Option<&'a str>,
    read_epoch: Option<u64>,
}

/// Protocol buffers are independently fixed to 8 KiB; this meters delivered HTTP DATA frames.
async fn wait_budget(
    shared: &StreamShared,
    needed: u64,
    read_epoch: Option<u64>,
    reserve: bool,
) -> Result<(), MediaIoError> {
    loop {
        if shared.cancelled()
            || read_epoch.is_some_and(|epoch| shared.read_epoch.load(Ordering::Acquire) != epoch)
        {
            return Err(MediaIoError::new(
                MediaIoErrorKind::Cancelled,
                "audio budget wait interrupted",
            ));
        }
        let allowed = {
            let mut state = shared
                .state
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            let total = state
                .budget_used
                .checked_add(state.budget_reserved)
                .and_then(|used| used.checked_add(needed));
            let allowed = state
                .budget_limit
                .is_none_or(|limit| total.is_some_and(|total| total <= limit));
            state.budget_blocked = !allowed;
            if allowed && reserve {
                state.budget_reserved += needed;
            }
            allowed
        };
        if allowed {
            return Ok(());
        }
        shared.changed.notify_all();
        // This deliberate suspension starts no network idle timer.
        tokio::select! {
            _ = interruption(shared, read_epoch) => (),
            _ = shared.notify.notified() => (),
            _ = tokio::time::sleep(Duration::from_millis(25)) => (),
        }
    }
}

struct EncodedReadPermit<'a> {
    shared: &'a StreamShared,
    reserved: u64,
}
impl EncodedReadPermit<'_> {
    fn consume(&self, bytes: usize) -> Result<(), MediaIoError> {
        let mut state = self
            .shared
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        state.budget_used = state.budget_used.saturating_add(bytes as u64);
        if bytes as u64 > self.reserved {
            return Err(MediaIoError::new(
                MediaIoErrorKind::Protocol,
                "audio HTTP DATA exceeded its read reservation",
            ));
        }
        Ok(())
    }
}
impl Drop for EncodedReadPermit<'_> {
    fn drop(&mut self) {
        let mut state = self
            .shared
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        state.budget_reserved = state.budget_reserved.saturating_sub(self.reserved);
        drop(state);
        self.shared.wake();
    }
}

async fn send(
    client: &Client,
    request: &ApprovedAudioRequest,
    fetch: FetchRequest<'_>,
    options: &TransportOptions,
    shared: &StreamShared,
) -> Result<Response, MediaIoError> {
    let range = fetch.interval.map(|(offset, end)| {
        end.map_or_else(
            || format!("bytes={offset}-"),
            |end| format!("bytes={offset}-{end}"),
        )
    });
    let needed = {
        let state = shared
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        fetch.interval.map_or(1, |(offset, end)| {
            let span = end.map_or(shared.block_size, |end| {
                end.saturating_sub(offset).saturating_add(1)
            });
            state
                .byte_len
                .map_or(span, |length| span.min(length.saturating_sub(offset)))
        })
    };
    wait_budget(shared, needed, fetch.read_epoch, false).await?;
    tokio::select! {
        _ = interruption(shared, fetch.read_epoch) => Err(MediaIoError::new(MediaIoErrorKind::Cancelled, "audio request interrupted")),
        result = tokio::time::timeout(options.header_timeout, client.get(&request.url, range.as_deref(), fetch.validator)) => match result {
            Err(_) => Err(MediaIoError::new(MediaIoErrorKind::Timeout, "audio response headers timed out")),
            Ok(Err(_)) => Err(MediaIoError::new(MediaIoErrorKind::Network, "audio HTTP request failed")),
            Ok(Ok(response)) => Ok(response),
        }
    }
}

async fn next_chunk(
    response: &mut Response,
    options: &TransportOptions,
    shared: &StreamShared,
    read_epoch: Option<u64>,
) -> Result<Option<impl AsRef<[u8]>>, MediaIoError> {
    let reservation = response
        .remaining_body_bytes()
        .map_or(HTTP_DATA_FRAME_LIMIT as u64, |remaining| {
            remaining.min(HTTP_DATA_FRAME_LIMIT as u64)
        });
    wait_budget(shared, reservation, read_epoch, true).await?;
    let permit = EncodedReadPermit {
        shared,
        reserved: reservation,
    };
    let result = tokio::select! {
        _ = interruption(shared, read_epoch) => Err(MediaIoError::new(MediaIoErrorKind::Cancelled, "audio response interrupted")),
        result = tokio::time::timeout(options.idle_timeout, response.chunk()) => match result {
            Err(_) => Err(MediaIoError::new(MediaIoErrorKind::Timeout, "audio response idle timeout")),
            Ok(Err(_)) => Err(MediaIoError::new(MediaIoErrorKind::Incomplete, "audio response body interrupted")),
            Ok(Ok(chunk)) => Ok(chunk),
        }
    };
    if let Ok(Some(chunk)) = &result {
        permit.consume(chunk.len())?;
    }
    result
}

#[derive(Debug, PartialEq, Eq)]
struct ContentRange {
    start: u64,
    end: u64,
    total: Option<u64>,
}

fn decimal(value: &str) -> Option<u64> {
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    value.parse().ok()
}

fn unsatisfied_length(value: &str) -> Option<u64> {
    let (unit, value) = value.split_once(' ')?;
    if !unit.eq_ignore_ascii_case("bytes") {
        return None;
    }
    decimal(value.strip_prefix("*/")?)
}

fn content_range(value: &str) -> Result<ContentRange, MediaIoError> {
    let invalid = || MediaIoError::new(MediaIoErrorKind::Protocol, "invalid audio Content-Range");
    let (unit, value) = value.split_once(' ').ok_or_else(invalid)?;
    if !unit.eq_ignore_ascii_case("bytes") {
        return Err(invalid());
    }
    let (interval, total) = value.split_once('/').ok_or_else(invalid)?;
    let (start, end) = interval.split_once('-').ok_or_else(invalid)?;
    let start = decimal(start).ok_or_else(invalid)?;
    let end = decimal(end).ok_or_else(invalid)?;
    let total: Option<u64> = if total == "*" {
        None
    } else {
        Some(decimal(total).ok_or_else(invalid)?)
    };
    if start > end || end == u64::MAX || total.is_some_and(|total| end >= total) {
        return Err(invalid());
    }
    Ok(ContentRange { start, end, total })
}

fn strong_etag(response: &Response) -> Option<String> {
    let value = response.headers().get(ETAG)?.to_str().ok()?;
    if value.starts_with('"')
        && value.ends_with('"')
        && value.len() >= 2
        && value.as_bytes()[1..value.len() - 1]
            .iter()
            .all(|byte| *byte >= 0x21 && *byte != b'"' && *byte != 0x7f)
    {
        Some(value.to_owned())
    } else {
        None
    }
}

fn check_encoding(response: &Response) -> Result<(), MediaIoError> {
    if response
        .headers()
        .get(CONTENT_ENCODING)
        .is_some_and(|value| {
            !value
                .to_str()
                .is_ok_and(|value| value.trim().eq_ignore_ascii_case("identity"))
        })
    {
        return Err(MediaIoError::new(
            MediaIoErrorKind::Protocol,
            "encoded audio HTTP representation is unsupported",
        ));
    }
    Ok(())
}

fn representation_changed(shared: &StreamShared) -> Result<(), MediaIoError> {
    let mut state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
    state.epoch = state.epoch.wrapping_add(1);
    state.byte_len = None;
    state.eof = false;
    state.eof_pending = false;
    state.change_pending = true;
    state.ready = false;
    state.range = false;
    state.strong_etag = None;
    state.remote_range = false;
    state.reusable_registered = false;
    state.sequential_until = 0;
    state.cursor = 0;
    state.lookahead_suppressed_cursor = None;
    state.lookahead_in_flight = None;
    state.sideband = Some(MediaIoError::new(
        MediaIoErrorKind::RepresentationChanged,
        "audio representation changed",
    ));
    shared.read_epoch.fetch_add(1, Ordering::AcqRel);
    // Readers hold this same fence while observing cache availability. Publish
    // the new representation before removing the old generation's RAM owners.
    let result = shared
        .cache
        .invalidate(&shared.object)
        .map_err(|_| MediaIoError::new(MediaIoErrorKind::Storage, "invalidate audio cache failed"));
    drop(state);
    shared.wake();
    result
}

fn restart_changed(
    shared: &StreamShared,
    restarts: &mut usize,
    options: &TransportOptions,
) -> Result<(), MediaIoError> {
    if *restarts > options.max_retries {
        return Err(MediaIoError::new(
            MediaIoErrorKind::RepresentationChanged,
            "audio representation exceeded the restart limit",
        ));
    }
    *restarts += 1;
    representation_changed(shared)?;
    shared
        .state
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .wanted = Some(0);
    Ok(())
}

async fn publish(shared: &StreamShared, start: u64, bytes: Vec<u8>) -> Result<(), MediaIoError> {
    if shared.cancelled() {
        return Err(MediaIoError::new(
            MediaIoErrorKind::Cancelled,
            "audio response cancelled",
        ));
    }
    let publication_started = Instant::now();
    #[cfg(test)]
    let registration_started = Instant::now();
    // Representation invalidation removes old blocks before publication; a stream owns one network worker.
    register_reusable(shared).await;
    #[cfg(test)]
    shared.cache.record_publication_stage(
        super::cache::PublicationStage::RegisterBefore,
        shared.source_serial,
        start,
        registration_started.elapsed(),
    );
    poll_publications(shared)?;
    let (foreground, epoch) = {
        let state = shared
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        (
            state.budget_limit.is_none() && state.remote_range && state.strong_etag.is_some(),
            state.epoch,
        )
    };
    if foreground {
        let ticket = shared
            .cache
            .stage_foreground(&shared.object, start, bytes, shared.cancel.clone())
            .map_err(|_| {
                MediaIoError::new(
                    MediaIoErrorKind::Storage,
                    "admit encoded audio cache failed",
                )
            })?;
        let ready = ticket.ram_ready;
        shared
            .pending_publications
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .push_back(PendingPublication {
                ticket,
                epoch,
                #[cfg(test)]
                offset: start,
                started: publication_started,
            });
        shared.wake();
        if !ready {
            finish_publications(shared).await?;
        }
        return Ok(());
    }
    finish_publications(shared).await?;
    shared
        .cache
        .publish_async(shared.object.clone(), start, bytes, shared.cancel.clone())
        .await
        .map_err(|_| {
            MediaIoError::new(
                if shared.cancelled() {
                    MediaIoErrorKind::Cancelled
                } else {
                    MediaIoErrorKind::Storage
                },
                if shared.cancelled() {
                    "audio response cancelled"
                } else {
                    "write encoded audio cache failed"
                },
            )
        })?;
    #[cfg(test)]
    let registration_started = Instant::now();
    register_reusable(shared).await;
    #[cfg(test)]
    shared.cache.record_publication_stage(
        super::cache::PublicationStage::RegisterAfter,
        shared.source_serial,
        start,
        registration_started.elapsed(),
    );
    let publication_micros = publication_started
        .elapsed()
        .as_micros()
        .min(u64::MAX as u128) as u64;
    #[cfg(test)]
    if shared
        .state
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .published_blocks
        < 32
    {
        eprintln!(
            "transport publication: cache_id={} source_serial={} offset={start} total_us={publication_micros}",
            shared.cache.publication_diagnostic_id(),
            shared.source_serial
        );
    }
    {
        let mut state = shared
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        state.published_blocks = state.published_blocks.saturating_add(1);
        state.last_publication_micros = publication_micros;
        state.max_publication_micros = state.max_publication_micros.max(publication_micros);
    }
    shared.wake();
    Ok(())
}

fn poll_publications(shared: &StreamShared) -> Result<(), MediaIoError> {
    loop {
        let completed = {
            let mut pending = shared
                .pending_publications
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            if pending
                .front()
                .is_some_and(|publication| publication.ticket.result().is_some())
            {
                pending.pop_front()
            } else {
                None
            }
        };
        let Some(publication) = completed else {
            return Ok(());
        };
        let current = shared
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .epoch
            == publication.epoch;
        if !current {
            continue;
        }
        if publication.ticket.result().unwrap().is_err() {
            return Err(MediaIoError::new(
                if shared.cancelled() {
                    MediaIoErrorKind::Cancelled
                } else {
                    MediaIoErrorKind::Storage
                },
                "write encoded audio cache failed",
            ));
        }
        let micros = publication
            .started
            .elapsed()
            .as_micros()
            .min(u64::MAX as u128) as u64;
        let mut state = shared
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        state.published_blocks = state.published_blocks.saturating_add(1);
        state.last_publication_micros = micros;
        state.max_publication_micros = state.max_publication_micros.max(micros);
        #[cfg(test)]
        if state.published_blocks <= 32 {
            eprintln!(
                "RAM publication durable acknowledgement: cache_id={} source_serial={} offset={} total_us={micros}",
                shared.cache.publication_diagnostic_id(),
                shared.source_serial,
                publication.offset
            );
        }
    }
}
async fn finish_publications(shared: &StreamShared) -> Result<(), MediaIoError> {
    let mut failure = None;
    loop {
        let ticket = shared
            .pending_publications
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .front()
            .map(|publication| publication.ticket.clone());
        let Some(ticket) = ticket else {
            return failure.map_or(Ok(()), Err);
        };
        let _ = ticket.wait().await;
        if let Err(error) = poll_publications(shared) {
            failure = Some(error);
        }
    }
}
async fn register_reusable(shared: &StreamShared) {
    let Some(key) = shared.reusable_key.as_ref() else {
        return;
    };
    let metadata = {
        let state = shared
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if state.budget_limit.is_some() || state.reusable_registered {
            return;
        }
        state
            .strong_etag
            .as_ref()
            .zip(state.byte_len)
            .map(|(tag, total)| (tag.clone(), total, state.epoch))
    };
    let Some((tag, total, epoch)) = metadata else {
        return;
    };
    if total == 0 || shared.cancelled() {
        return;
    }
    // Reuse is optional. Transient blocks retain their normal ownership and error checks.
    if let Ok(true) = shared
        .cache
        .register_reusable_representation_async(
            key.clone(),
            shared.object.clone(),
            tag,
            total,
            shared.cache_epoch,
            shared.cancel.clone(),
        )
        .await
    {
        let mut state = shared
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if state.epoch == epoch && state.budget_limit.is_none() && !shared.cancelled() {
            state.reusable_registered = true;
        }
    }
}

async fn try_reusable(
    client: &Client,
    request: &ApprovedAudioRequest,
    options: &TransportOptions,
    shared: &StreamShared,
) -> Result<bool, MediaIoError> {
    let Some(key) = &shared.reusable_key else {
        return Ok(false);
    };
    if shared
        .state
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .budget_limit
        .is_some()
    {
        return Ok(false);
    }
    let candidate = match shared
        .cache
        .lookup_reusable_async(key.clone(), shared.cancel.clone())
        .await
    {
        Ok(Some(candidate)) => candidate,
        Ok(None) => return Ok(false),
        Err(_) if shared.cancelled() => {
            return Err(MediaIoError::new(
                MediaIoErrorKind::Cancelled,
                "audio reuse cancelled",
            ));
        }
        Err(_) => return Ok(false),
    };
    if candidate.block_size as u64 != shared.block_size || candidate.total == 0 {
        return Ok(false);
    }
    let epoch = shared.read_epoch.load(Ordering::Acquire);
    let mut response = send(
        client,
        request,
        FetchRequest {
            interval: Some((0, Some(0))),
            validator: Some(&candidate.strong_etag),
            read_epoch: Some(epoch),
        },
        options,
        shared,
    )
    .await?;
    if !matches!(
        response.status(),
        StatusCode::OK | StatusCode::PARTIAL_CONTENT
    ) {
        return Err(MediaIoError::http(response.status()));
    }
    check_encoding(&response)?;
    let remote_range = response.status() == StatusCode::PARTIAL_CONTENT;
    let complete = candidate.verified_bytes == candidate.total
        && candidate.offsets.len() as u64 == candidate.total.div_ceil(shared.block_size)
        && candidate
            .offsets
            .iter()
            .enumerate()
            .all(|(index, offset)| *offset == index as u64 * shared.block_size);
    let same = if remote_range {
        let range = content_range(
            response
                .headers()
                .get(CONTENT_RANGE)
                .and_then(|value| value.to_str().ok())
                .ok_or_else(|| {
                    MediaIoError::new(
                        MediaIoErrorKind::Protocol,
                        "audio reuse 206 lacks Content-Range",
                    )
                })?,
        )?;
        if range.start != 0
            || range.end != 0
            || response.content_length().is_some_and(|length| length != 1)
        {
            return Err(MediaIoError::new(
                MediaIoErrorKind::Protocol,
                "audio reuse validation exceeds its one-byte interval",
            ));
        }
        let body = range_body(&mut response, 1, options, shared, epoch).await?;
        body.len() == 1
            && range.total == Some(candidate.total)
            && strong_etag(&response).as_deref() == Some(candidate.strong_etag.as_str())
    } else if complete
        && response.content_length() == Some(candidate.total)
        && strong_etag(&response).as_deref() == Some(candidate.strong_etag.as_str())
    {
        next_chunk(&mut response, options, shared, Some(epoch))
            .await?
            .is_some_and(|chunk| !chunk.as_ref().is_empty())
    } else {
        false
    };
    if !same {
        let _ = shared
            .cache
            .invalidate_reusable_candidate_async(&candidate, shared.cancel.clone())
            .await;
        return Ok(false);
    }
    if shared.cancelled() || shared.read_epoch.load(Ordering::Acquire) != epoch {
        return Err(MediaIoError::new(
            MediaIoErrorKind::Cancelled,
            "audio reuse validation interrupted",
        ));
    }
    let count = match shared.cache.bind_reusable(&shared.object, &candidate) {
        Ok(count) => count,
        Err(_) => return Ok(false),
    };
    if count == 0 {
        return Ok(false);
    }
    if !remote_range && shared.cache.protect_reusable_view(&shared.object).is_err() {
        shared.cache.invalidate(&shared.object).map_err(|_| {
            MediaIoError::new(
                MediaIoErrorKind::Storage,
                "rollback incomplete reusable representation failed",
            )
        })?;
        return Ok(false);
    }
    let interrupted = {
        let mut state = shared
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if shared.cancelled() || shared.read_epoch.load(Ordering::Acquire) != epoch {
            true
        } else {
            state.strong_etag = Some(candidate.strong_etag);
            state.byte_len = Some(candidate.total);
            state.range = true;
            state.remote_range = remote_range;
            state.eof = complete;
            state.eof_pending = false;
            state.ready = true;
            state.reusable_registered = false;
            false
        }
    };
    if interrupted {
        shared.cache.invalidate(&shared.object).map_err(|_| {
            MediaIoError::new(
                MediaIoErrorKind::Storage,
                "rollback interrupted reuse binding failed",
            )
        })?;
        return Err(MediaIoError::new(
            MediaIoErrorKind::Cancelled,
            "audio reuse binding interrupted",
        ));
    }
    shared.wake();
    register_reusable(shared).await;
    Ok(true)
}

async fn sequential(
    response: &mut Response,
    options: &TransportOptions,
    shared: &StreamShared,
    expected: Option<u64>,
) -> Result<(), MediaIoError> {
    let mut offset = 0;
    let mut block = Vec::with_capacity(shared.block_size as usize);
    loop {
        // Deliberate backpressure is excluded from the next expected-data idle timeout.
        wait_capacity(offset, options, shared).await?;
        let Some(chunk) = next_chunk(response, options, shared, None).await? else {
            break;
        };
        let mut bytes = chunk.as_ref();
        while !bytes.is_empty() {
            wait_capacity(offset, options, shared).await?;
            let take = bytes.len().min(shared.block_size as usize - block.len());
            block.extend_from_slice(&bytes[..take]);
            bytes = &bytes[take..];
            {
                let mut state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
                state.received = state.received.saturating_add(take as u64);
            }
            if block.len() == shared.block_size as usize {
                offset += block.len() as u64;
                if expected == Some(offset) {
                    shared
                        .state
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .eof_pending = true;
                }
                let bytes =
                    std::mem::replace(&mut block, Vec::with_capacity(shared.block_size as usize));
                publish(shared, offset - bytes.len() as u64, bytes).await?;
                let mut state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
                state.sequential_until = offset;
                drop(state);
                shared.wake();
            }
        }
    }
    let total = offset + block.len() as u64;
    if expected.is_some_and(|expected| expected != total) {
        return Err(MediaIoError::new(
            MediaIoErrorKind::Incomplete,
            "audio body length does not match response headers",
        ));
    }
    shared
        .state
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .eof_pending = true;
    if !block.is_empty() {
        publish(shared, offset, block).await?;
    }
    finish_publications(shared).await?;
    let complete_cached_view = shared
        .cache
        .protect_complete_view(&shared.object, total)
        .is_ok();
    let mut state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
    state.byte_len = Some(total);
    state.sequential_until = total;
    state.eof = true;
    state.eof_pending = false;
    // Only an owned complete cached representation permits random access to a
    // sequential response. The server's Range capability remains unchanged.
    state.range = complete_cached_view;
    drop(state);
    shared.wake();
    Ok(())
}

async fn wait_capacity(
    offset: u64,
    options: &TransportOptions,
    shared: &StreamShared,
) -> Result<(), MediaIoError> {
    loop {
        if shared.cancelled() {
            return Err(MediaIoError::new(
                MediaIoErrorKind::Cancelled,
                "audio response cancelled",
            ));
        }
        let cursor = shared
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .cursor;
        let lead = (options.sequential_lead_blocks as u64 * shared.block_size)
            .min(
                (shared.cache.capacity_blocks().saturating_sub(1) as u64)
                    .saturating_mul(shared.block_size),
            )
            .min(
                (options.cache.disk_bytes / shared.block_size)
                    .saturating_sub(1)
                    .saturating_mul(shared.block_size),
            )
            .min(
                (options.cache.max_blocks.saturating_sub(1) as u64)
                    .saturating_mul(shared.block_size),
            );
        if offset <= cursor.saturating_add(lead) {
            return Ok(());
        }
        tokio::select! { _ = cancellation(shared) => (), _ = shared.notify.notified() => (), _ = tokio::time::sleep(Duration::from_millis(25)) => () }
    }
}

async fn range_body(
    response: &mut Response,
    expected: u64,
    options: &TransportOptions,
    shared: &StreamShared,
    read_epoch: u64,
) -> Result<Vec<u8>, MediaIoError> {
    let mut bytes = Vec::with_capacity(expected as usize);
    while let Some(chunk) = next_chunk(response, options, shared, Some(read_epoch)).await? {
        let chunk = chunk.as_ref();
        if bytes.len() as u64 + chunk.len() as u64 > expected {
            return Err(MediaIoError::new(
                MediaIoErrorKind::Protocol,
                "audio range body exceeds declared interval",
            ));
        }
        bytes.extend_from_slice(chunk);
    }
    if bytes.len() as u64 != expected {
        return Err(MediaIoError::new(
            MediaIoErrorKind::Incomplete,
            "audio range body is short",
        ));
    }
    Ok(bytes)
}

struct VerifiedBlock {
    bytes: Vec<u8>,
    total: Option<u64>,
    end_confirmed: bool,
}

/// Servers may split a satisfiable Range. Concatenate only contiguous responses with the same strong ETag.
async fn range_block(
    client: &Client,
    request: &ApprovedAudioRequest,
    mut response: Response,
    mut range: ContentRange,
    options: &TransportOptions,
    shared: &StreamShared,
    read_epoch: u64,
) -> Result<VerifiedBlock, MediaIoError> {
    let start = range.start;
    let requested_end = start.saturating_add(shared.block_size - 1);
    let validator = strong_etag(&response).ok_or_else(|| {
        MediaIoError::new(
            MediaIoErrorKind::Protocol,
            "audio block requires a strong validator",
        )
    })?;
    let mut bytes = Vec::with_capacity(shared.block_size as usize);
    let mut total = range.total;
    for _ in 0..64 {
        let expected = range.end - range.start + 1;
        if response
            .content_length()
            .is_some_and(|length| length != expected)
        {
            return Err(MediaIoError::new(
                MediaIoErrorKind::Protocol,
                "audio range Content-Length mismatch",
            ));
        }
        bytes.extend_from_slice(
            &range_body(&mut response, expected, options, shared, read_epoch).await?,
        );
        let next = start + bytes.len() as u64;
        if total == Some(next) {
            return Ok(VerifiedBlock {
                bytes,
                total,
                end_confirmed: true,
            });
        }
        if bytes.len() == shared.block_size as usize {
            return Ok(VerifiedBlock {
                bytes,
                total,
                end_confirmed: false,
            });
        }
        response = send(
            client,
            request,
            FetchRequest {
                interval: Some((next, Some(requested_end))),
                validator: Some(&validator),
                read_epoch: Some(read_epoch),
            },
            options,
            shared,
        )
        .await?;
        if response.status() == StatusCode::RANGE_NOT_SATISFIABLE {
            let length = response
                .headers()
                .get(CONTENT_RANGE)
                .and_then(|value| value.to_str().ok())
                .and_then(unsatisfied_length)
                .ok_or_else(|| {
                    MediaIoError::new(
                        MediaIoErrorKind::Protocol,
                        "invalid unsatisfied audio tail range",
                    )
                })?;
            if next < length {
                return Err(MediaIoError::new(
                    MediaIoErrorKind::Protocol,
                    "audio tail 416 precedes representation end",
                ));
            }
            if next != length
                || total.is_some_and(|old| old != length)
                || strong_etag(&response).is_some_and(|value| value != validator)
            {
                return Err(MediaIoError::new(
                    MediaIoErrorKind::RepresentationChanged,
                    "audio tail representation changed",
                ));
            }
            return Ok(VerifiedBlock {
                bytes,
                total: Some(length),
                end_confirmed: true,
            });
        }
        if response.status() == StatusCode::OK {
            return Err(MediaIoError::new(
                MediaIoErrorKind::RepresentationChanged,
                "audio partial range validator changed",
            ));
        }
        if response.status() != StatusCode::PARTIAL_CONTENT {
            return Err(MediaIoError::http(response.status()));
        }
        check_encoding(&response)?;
        if strong_etag(&response).as_deref() != Some(validator.as_str()) {
            return Err(MediaIoError::new(
                MediaIoErrorKind::RepresentationChanged,
                "audio partial range validator changed",
            ));
        }
        range = content_range(
            response
                .headers()
                .get(CONTENT_RANGE)
                .and_then(|value| value.to_str().ok())
                .ok_or_else(|| {
                    MediaIoError::new(
                        MediaIoErrorKind::Protocol,
                        "audio partial range lacks Content-Range",
                    )
                })?,
        )?;
        if range.start != next || range.end > requested_end {
            return Err(MediaIoError::new(
                MediaIoErrorKind::Protocol,
                "audio partial range is not contiguous",
            ));
        }
        if total.is_some() && total != range.total {
            return Err(MediaIoError::new(
                MediaIoErrorKind::RepresentationChanged,
                "audio partial range total changed",
            ));
        }
        if range.total.is_some() {
            total = range.total;
        }
    }
    Err(MediaIoError::new(
        MediaIoErrorKind::Incomplete,
        "audio server exceeded the partial-range limit",
    ))
}

fn sequential_length(response: &Response) -> Result<Option<u64>, MediaIoError> {
    match response.status() {
        StatusCode::OK => {
            check_encoding(response)?;
            Ok(response.content_length())
        }
        StatusCode::PARTIAL_CONTENT => {
            check_encoding(response)?;
            let range = content_range(
                response
                    .headers()
                    .get(CONTENT_RANGE)
                    .and_then(|v| v.to_str().ok())
                    .ok_or_else(|| {
                        MediaIoError::new(
                            MediaIoErrorKind::Protocol,
                            "sequential audio 206 lacks Content-Range",
                        )
                    })?,
            )?;
            if range.start != 0 || range.total.is_none_or(|total| range.end + 1 != total) {
                return Err(MediaIoError::new(
                    MediaIoErrorKind::Protocol,
                    "unvalidated audio requires a complete sequential response",
                ));
            }
            let length = range.end + 1;
            if response
                .content_length()
                .is_some_and(|value| value != length)
            {
                return Err(MediaIoError::new(
                    MediaIoErrorKind::Protocol,
                    "sequential audio Content-Length mismatch",
                ));
            }
            Ok(Some(length))
        }
        _ => Err(MediaIoError::http(response.status())),
    }
}

async fn sequential_retries(
    client: &Client,
    request: &ApprovedAudioRequest,
    mut response: Response,
    options: &TransportOptions,
    shared: &StreamShared,
) -> Result<(), MediaIoError> {
    let mut failures = 0;
    loop {
        let expected = sequential_length(&response)?;
        {
            finish_publications(shared).await?;
            let mut state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
            state.range = false;
            state.remote_range = false;
            state.byte_len = expected;
            state.ready = true;
            state.strong_etag = strong_etag(&response);
        }
        shared.wake();
        match sequential(&mut response, options, shared, expected).await {
            Ok(()) => return Ok(()),
            Err(error)
                if matches!(
                    error.kind,
                    MediaIoErrorKind::Incomplete
                        | MediaIoErrorKind::Timeout
                        | MediaIoErrorKind::Network
                ) && failures < options.max_retries =>
            {
                failures += 1;
                representation_changed(shared)?;
                response = loop {
                    match send(
                        client,
                        request,
                        FetchRequest {
                            interval: None,
                            validator: None,
                            read_epoch: None,
                        },
                        options,
                        shared,
                    )
                    .await
                    {
                        Err(error)
                            if error.kind == MediaIoErrorKind::Cancelled && !shared.cancelled() =>
                        {
                            continue;
                        }
                        result => break result?,
                    }
                };
            }
            Err(error) => return Err(error),
        }
    }
}

async fn run_stream(
    client: Client,
    request: ApprovedAudioRequest,
    options: TransportOptions,
    shared: Arc<StreamShared>,
) -> Result<(), MediaIoError> {
    shared
        .cache
        .wait_ready_async(&shared.cancel)
        .await
        .map_err(|_| {
            MediaIoError::new(
                if shared.cancelled() {
                    MediaIoErrorKind::Cancelled
                } else {
                    MediaIoErrorKind::Storage
                },
                if shared.cancelled() {
                    "audio transport cancelled"
                } else {
                    "initialize encoded audio cache failed"
                },
            )
        })?;
    let reused = match try_reusable(&client, &request, &options, &shared).await {
        Err(error) if error.kind == MediaIoErrorKind::Cancelled && !shared.cancelled() => false,
        result => result?,
    };
    let mut initial = !reused;
    let mut body_failures = 0;
    let mut representation_restarts = 0;
    'requests: loop {
        poll_publications(&shared)?;
        if shared.cache.publication_failed(&shared.object) {
            return Err(MediaIoError::new(
                MediaIoErrorKind::Storage,
                "write encoded audio cache failed",
            ));
        }
        if shared.cancelled() {
            return Err(MediaIoError::new(
                MediaIoErrorKind::Cancelled,
                "audio transport cancelled",
            ));
        }
        let Some(work) = next_range_work(&shared, &options) else {
            register_reusable(&shared).await;
            tokio::select! { _ = cancellation(&shared) => (), _ = shared.notify.notified() => (), _ = tokio::time::sleep(Duration::from_millis(25)) => () }
            continue;
        };
        let RangeWork {
            offset,
            validator,
            read_epoch,
            lookahead,
        } = work;
        if shared.cache.contains(&shared.object, offset) {
            register_reusable(&shared).await;
            continue;
        }
        let _lookahead = lookahead.then(|| InFlightLookahead::begin(&shared, offset, read_epoch));
        let end = offset.saturating_add(shared.block_size - 1);
        let range_load_started = Instant::now();
        let mut attempts = 0;
        let response = loop {
            match send(
                &client,
                &request,
                FetchRequest {
                    interval: Some((offset, Some(end))),
                    validator: validator.as_deref(),
                    read_epoch: Some(read_epoch),
                },
                &options,
                &shared,
            )
            .await
            {
                Ok(response) => break response,
                Err(error) if error.kind == MediaIoErrorKind::Cancelled && !shared.cancelled() => {
                    continue 'requests;
                }
                Err(error)
                    if error.kind != MediaIoErrorKind::Cancelled
                        && attempts < options.max_retries =>
                {
                    attempts += 1;
                }
                Err(error) if lookahead && deferred_lookahead_error(&error) => {
                    suppress_lookahead(&shared);
                    continue 'requests;
                }
                Err(error) => return Err(error),
            }
        };
        if response.status() == StatusCode::RANGE_NOT_SATISFIABLE {
            let total = response
                .headers()
                .get(CONTENT_RANGE)
                .and_then(|v| v.to_str().ok())
                .and_then(unsatisfied_length)
                .ok_or_else(|| {
                    MediaIoError::new(
                        MediaIoErrorKind::Protocol,
                        "invalid unsatisfied audio range",
                    )
                })?;
            finish_publications(&shared).await?;
            let mut state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
            if strong_etag(&response).is_some_and(|value| Some(value) != state.strong_etag)
                && !initial
            {
                drop(state);
                restart_changed(&shared, &mut representation_restarts, &options)?;
                initial = true;
                continue;
            }
            if offset < total {
                return Err(MediaIoError::new(
                    MediaIoErrorKind::Protocol,
                    "audio 416 precedes representation end",
                ));
            }
            if state.byte_len.is_some_and(|old| old != total) {
                drop(state);
                restart_changed(&shared, &mut representation_restarts, &options)?;
                initial = true;
                continue;
            }
            state.byte_len = Some(total);
            state.eof = true;
            state.eof_pending = false;
            state.ready = true;
            drop(state);
            shared.wake();
            initial = false;
            continue;
        }
        if response.status() == StatusCode::OK {
            if !initial {
                restart_changed(&shared, &mut representation_restarts, &options)?;
            }
            return sequential_retries(&client, &request, response, &options, &shared).await;
        }
        if response.status() != StatusCode::PARTIAL_CONTENT {
            let error = MediaIoError::http(response.status());
            if lookahead && deferred_lookahead_error(&error) {
                suppress_lookahead(&shared);
                continue;
            }
            return Err(error);
        }
        check_encoding(&response)?;
        let range = content_range(
            response
                .headers()
                .get(CONTENT_RANGE)
                .and_then(|v| v.to_str().ok())
                .ok_or_else(|| {
                    MediaIoError::new(MediaIoErrorKind::Protocol, "audio 206 lacks Content-Range")
                })?,
        )?;
        if range.start != offset || range.end > end {
            return Err(MediaIoError::new(
                MediaIoErrorKind::Protocol,
                "audio range does not match requested interval",
            ));
        }
        let etag = strong_etag(&response);
        let changed = {
            let state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
            !initial
                && (etag != state.strong_etag
                    || (state.byte_len.is_some() && range.total != state.byte_len))
        };
        if changed {
            restart_changed(&shared, &mut representation_restarts, &options)?;
            initial = true;
            shared.wake();
            continue;
        }
        if etag.is_none() {
            // A weak/missing validator cannot safely concatenate separate range responses.
            drop(response);
            let full = match send(
                &client,
                &request,
                FetchRequest {
                    interval: None,
                    validator: None,
                    read_epoch: Some(read_epoch),
                },
                &options,
                &shared,
            )
            .await
            {
                Err(error) if error.kind == MediaIoErrorKind::Cancelled && !shared.cancelled() => {
                    continue;
                }
                result => result?,
            };
            return sequential_retries(&client, &request, full, &options, &shared).await;
        }
        let expected = range.end - range.start + 1;
        if response
            .content_length()
            .is_some_and(|length| length != expected)
        {
            return Err(MediaIoError::new(
                MediaIoErrorKind::Protocol,
                "audio range Content-Length mismatch",
            ));
        }
        {
            let mut state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
            state.strong_etag = etag;
            state.byte_len = range.total;
            state.range = true;
            state.remote_range = true;
            state.ready = true;
        }
        shared.wake();
        let verified = match range_block(
            &client, &request, response, range, &options, &shared, read_epoch,
        )
        .await
        {
            Ok(verified) => verified,
            Err(error) if error.kind == MediaIoErrorKind::RepresentationChanged => {
                restart_changed(&shared, &mut representation_restarts, &options)?;
                initial = true;
                continue;
            }
            Err(error) if error.kind == MediaIoErrorKind::Cancelled && !shared.cancelled() => {
                body_failures = 0;
                continue;
            }
            Err(error) if lookahead && deferred_lookahead_error(&error) => {
                body_failures = 0;
                suppress_lookahead(&shared);
                continue;
            }
            Err(error)
                if matches!(
                    error.kind,
                    MediaIoErrorKind::Incomplete
                        | MediaIoErrorKind::Timeout
                        | MediaIoErrorKind::Network
                ) && error.http_status.is_none()
                    && body_failures < options.max_retries =>
            {
                body_failures += 1;
                initial = false;
                let mut state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
                if state.wanted.is_none() {
                    state.wanted = Some(offset);
                }
                continue;
            }
            Err(error) => return Err(error),
        };
        body_failures = 0;
        let end_confirmed = verified.end_confirmed;
        {
            let mut state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
            state.received = state.received.saturating_add(verified.bytes.len() as u64);
            state.eof_pending |= end_confirmed;
            state.byte_len = verified.total;
        }
        publish(&shared, offset, verified.bytes).await?;
        if end_confirmed {
            finish_publications(&shared).await?;
            let mut state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
            state.eof = true;
            state.eof_pending = false;
        }
        let range_load_micros = range_load_started
            .elapsed()
            .as_micros()
            .min(u64::MAX as u128) as u64;
        {
            let mut state = shared
                .state
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            state.completed_range_loads = state.completed_range_loads.saturating_add(1);
            state.last_range_load_micros = range_load_micros;
            state.max_range_load_micros = state.max_range_load_micros.max(range_load_micros);
        }
        initial = false;
        shared.wake();
    }
}

struct RangeWork {
    offset: u64,
    validator: Option<String>,
    read_epoch: u64,
    lookahead: bool,
}

struct InFlightLookahead<'a> {
    shared: &'a StreamShared,
    identity: LookaheadIdentity,
}
impl<'a> InFlightLookahead<'a> {
    fn begin(shared: &'a StreamShared, offset: u64, read_epoch: u64) -> Self {
        let mut state = shared
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let identity = LookaheadIdentity {
            offset,
            read_epoch,
            representation_epoch: state.epoch,
        };
        state.lookahead_in_flight = Some(identity);
        Self { shared, identity }
    }
}
impl Drop for InFlightLookahead<'_> {
    fn drop(&mut self) {
        let mut state = self
            .shared
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if state.lookahead_in_flight == Some(self.identity) {
            state.lookahead_in_flight = None;
        }
    }
}

fn next_range_work(shared: &StreamShared, options: &TransportOptions) -> Option<RangeWork> {
    let mut state = shared
        .state
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let read_epoch = shared.read_epoch.load(Ordering::Acquire);
    if let Some(offset) = state.wanted.take() {
        return Some(RangeWork {
            offset,
            validator: state.strong_etag.clone(),
            read_epoch,
            lookahead: false,
        });
    }
    let lead = options
        .range_lookahead_blocks
        .min(shared.cache.capacity_blocks().saturating_sub(1));
    let total = state.byte_len?;
    let validator = state.strong_etag.as_ref()?;
    let cursor = state.cursor / shared.block_size * shared.block_size;
    if lead == 0
        || !state.ready
        || !state.range
        || !state.remote_range
        || state.change_pending
        || state.error.is_some()
        || shared.cancelled()
        || state.lookahead_suppressed_cursor == Some(cursor)
    {
        return None;
    }
    for ahead in 0..=lead {
        let offset = cursor.checked_add((ahead as u64).checked_mul(shared.block_size)?)?;
        if offset >= total {
            break;
        }
        if shared.cache.contains(&shared.object, offset) {
            continue;
        }
        let bytes = (total - offset).min(shared.block_size);
        if state.budget_limit.is_some_and(|limit| {
            state
                .budget_used
                .saturating_add(state.budget_reserved)
                .saturating_add(bytes)
                > limit
        }) || !shared
            .cache
            .can_admit_lookahead(&shared.object, bytes as usize)
        {
            return None;
        }
        return Some(RangeWork {
            offset,
            validator: Some(validator.clone()),
            read_epoch,
            lookahead: true,
        });
    }
    None
}

fn deferred_lookahead_error(error: &MediaIoError) -> bool {
    match error.http_status {
        Some(408 | 500..=599) => true,
        Some(_) => false,
        None => matches!(
            error.kind,
            MediaIoErrorKind::Network | MediaIoErrorKind::Timeout | MediaIoErrorKind::Incomplete
        ),
    }
}

fn suppress_lookahead(shared: &StreamShared) {
    let mut state = shared
        .state
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    state.lookahead_suppressed_cursor = Some(state.cursor / shared.block_size * shared.block_size);
}

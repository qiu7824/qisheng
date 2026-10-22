//! A deliberately limited LX v2 custom-source adapter.
//!
//! Import parses JavaScript without evaluating it. The execution functions are
//! intended for the approved worker process, never the UI thread. HTTP(S) runs
//! asynchronously after explicit approval; no filesystem/environment/imports are
//! exposed. Engine budgets supplement the parent's process deadline.

use crate::domain::lyric::{LyricBundle, LyricField};
use anyhow::{Context as _, Result, anyhow, bail, ensure};
use boa_engine::{
    Context, JsNativeError, JsResult, JsString, JsValue, NativeFunction, Script, Source,
    builtins::promise::{OperationType, Promise},
    context::HostHooks,
    job::{Job, JobExecutor},
    module::IdleModuleLoader,
    object::{JsObject, builtins::JsPromise},
    property::Attribute,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
#[cfg(test)]
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{
    cell::{Cell, RefCell},
    collections::{BTreeMap, VecDeque},
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

#[derive(Debug)]
pub struct SourceRequestCancelled;
impl std::fmt::Display for SourceRequestCancelled {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("Source request was cancelled")
    }
}
impl std::error::Error for SourceRequestCancelled {}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SourceFailureKind {
    Rejected,
    RateLimited,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SourceRequestFailure {
    pub kind: SourceFailureKind,
    pub message: String,
}
impl std::fmt::Display for SourceRequestFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}
impl std::error::Error for SourceRequestFailure {}

#[derive(Clone)]
struct OperationCancellation {
    cancelled_through: Arc<AtomicU64>,
    operation_id: u64,
}
impl OperationCancellation {
    fn is_cancelled(&self) -> bool {
        self.cancelled_through.load(Ordering::Acquire) >= self.operation_id
    }
}

pub const MAX_SCRIPT_BYTES: usize = 1024 * 1024;
/// Policy 2 permits the explicitly disclosed HTTP(S) source-network capabilities.
/// Earlier approvals covered a narrower loopback-only network surface.
pub const CURRENT_APPROVAL_POLICY_VERSION: u32 = 2;
const MAX_JOBS: usize = 4096;
const HOST_BUDGET: Duration = Duration::from_secs(5);
const OPERATION_BUDGET: Duration = Duration::from_secs(70);
pub const MIN_TIMER_DELAY_MS: u64 = 1;
pub const MAX_TIMER_DELAY_MS: u64 = 60_000;
pub const MAX_PENDING_TIMERS: usize = 64;
pub const MAX_TIMERS_PER_OPERATION: usize = 256;
pub const MAX_TIMER_CALLBACKS_PER_OPERATION: usize = 512;
const MAX_HOST_EVENTS_PER_OPERATION: usize = 16;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum SourceHostEvent {
    UpdateAlert {
        log: String,
        update_url: Option<String>,
    },
    OpenDevToolsRequested,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct SourceMetadata {
    pub name: String,
    pub description: String,
    pub version: String,
    pub author: String,
    pub homepage: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ImportedSource {
    pub metadata: SourceMetadata,
    pub script: String,
    /// Stable identity and consent binding: approval belongs to these exact bytes.
    pub sha256: String,
    #[serde(default)]
    pub approved: bool,
    /// Legacy state has no policy field and therefore deserializes as version 0.
    #[serde(default)]
    pub approval_policy_version: u32,
}

impl ImportedSource {
    pub fn verify_integrity(&self) -> Result<()> {
        ensure!(
            self.sha256 == script_hash(&self.script),
            "Stored source integrity check failed; import it again"
        );
        Ok(())
    }

    pub fn require_approval(&self) -> Result<()> {
        self.verify_integrity()?;
        ensure!(self.approved, "Source execution requires explicit approval");
        ensure!(
            self.approval_policy_version == CURRENT_APPROVAL_POLICY_VERSION,
            "Source network permissions changed; explicit approval is required again"
        );
        Ok(())
    }

    /// Records approval of the current permission surface and these exact bytes.
    pub fn approve(&mut self) -> Result<()> {
        ensure!(
            self.script.len() <= MAX_SCRIPT_BYTES,
            "Source script exceeds the 1 MiB approval limit"
        );
        self.verify_integrity()?;
        self.approval_policy_version = CURRENT_APPROVAL_POLICY_VERSION;
        self.approved = true;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct SourceCapabilities {
    #[serde(rename = "type")]
    pub kind: String,
    pub actions: Vec<String>,
    /// Upstream intentionally spells this key `qualitys`.
    pub qualitys: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct RuntimeManifest {
    pub sources: BTreeMap<String, SourceCapabilities>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SourceRequest {
    pub source: String,
    pub action: String,
    pub info: Value,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum SourceResult {
    Url(String),
    Lyrics(LyricBundle),
}

impl SourceResult {
    pub fn into_url(self) -> Result<String> {
        match self {
            Self::Url(url) => Ok(url),
            Self::Lyrics(_) => bail!("Lyric response requires the typed request_result API"),
        }
    }
}

pub fn script_hash(script: &str) -> String {
    format!("{:x}", Sha256::digest(script.as_bytes()))
}

/// Reads the leading LX comment and syntax-checks the script. No code is run.
pub fn import_script(script: &str) -> Result<ImportedSource> {
    ensure!(
        script.len() <= MAX_SCRIPT_BYTES,
        "Source script exceeds the 1 MiB import limit"
    );
    ensure!(
        !script.contains('\0'),
        "Source script contains a NUL character"
    );
    let normalized = script.trim_start_matches('\u{feff}').trim_start();
    ensure!(
        normalized.starts_with("/*"),
        "Invalid LX source: expected a leading metadata block comment"
    );
    let end = normalized
        .find("*/")
        .context("Invalid LX source: metadata comment is not closed")?;
    let metadata = read_metadata(&normalized[2..end]);
    let mut context = Context::default();
    // Script::parse only constructs the AST; it does not compile/evaluate globals.
    Script::parse(Source::from_bytes(script), None, &mut context)
        .map_err(|error| anyhow!("JavaScript syntax error: {error}"))?;
    Ok(ImportedSource {
        metadata,
        script: script.into(),
        sha256: script_hash(script),
        approved: false,
        approval_policy_version: 0,
    })
}

fn read_metadata(comment: &str) -> SourceMetadata {
    let mut fields = BTreeMap::new();
    for line in comment.lines() {
        let line = line.trim().trim_start_matches('*').trim();
        if let Some(line) = line.strip_prefix('@')
            && let Some(split) = line.find(char::is_whitespace)
        {
            fields.insert(&line[..split], line[split..].trim());
        }
    }
    let field = |key: &str, max: usize| {
        let value = fields.get(key).copied().unwrap_or("");
        let units: Vec<u16> = value.encode_utf16().collect();
        if units.len() > max {
            // LX uses JS String.length / substring(0, max), both UTF-16 based.
            // A cut through a surrogate pair is normalized to U+FFFD for the
            // Rust UTF-8 model, matching its visible replacement character.
            format!("{}...", String::from_utf16_lossy(&units[..max]))
        } else {
            value.to_owned()
        }
    };
    let mut metadata = SourceMetadata {
        name: field("name", 24),
        description: field("description", 36),
        version: field("version", 36),
        author: field("author", 56),
        homepage: field("homepage", 1024),
    };
    if metadata.name.is_empty() {
        metadata.name = "Unnamed LX source".into();
    }
    metadata
}

/// Executes a trusted fixture or an explicitly approved script inside a worker.
pub fn execute_init(script: &str) -> Result<RuntimeManifest> {
    Ok(SourceSession::new(script)?.manifest().clone())
}

/// URL-only compatibility wrapper; lyric callers use execute_request_result.
pub fn execute_request(script: &str, request: &SourceRequest) -> Result<String> {
    SourceSession::new(script)?.request(request)
}

pub fn execute_request_result(script: &str, request: &SourceRequest) -> Result<SourceResult> {
    SourceSession::new(script)?.request_result(request)
}

/// One enabled source lifecycle. Requests reuse its globals and closure state.
/// The parent worker owns this value and destroys it on switch/disable/delete.
pub struct SourceSession {
    runtime: Runtime,
    manifest: RuntimeManifest,
}

impl SourceSession {
    pub fn new(script: &str) -> Result<Self> {
        Self::with_network_policy(script, false)
    }

    /// Called by the worker only for the current approved network policy.
    pub fn new_authorized(script: &str) -> Result<Self> {
        Self::with_network_policy(script, true)
    }

    fn with_network_policy(script: &str, remote_allowed: bool) -> Result<Self> {
        let mut runtime = Runtime::new(script, remote_allowed)?;
        let manifest = runtime.manifest()?;
        Ok(Self { runtime, manifest })
    }

    pub fn manifest(&self) -> &RuntimeManifest {
        &self.manifest
    }

    pub fn take_host_events(&mut self) -> Vec<SourceHostEvent> {
        self.runtime
            .async_bridge
            .borrow_mut()
            .events
            .drain(..)
            .collect()
    }

    pub fn request(&mut self, request: &SourceRequest) -> Result<String> {
        self.request_result(request)?.into_url()
    }

    pub(crate) fn request_result_cancellable(
        &mut self,
        request: &SourceRequest,
        cancelled_through: Arc<AtomicU64>,
        operation_id: u64,
    ) -> Result<SourceResult> {
        ensure!(
            operation_id > 0,
            "Source operation identifier must be nonzero"
        );
        *self.runtime.executor.cancellation.borrow_mut() = Some(OperationCancellation {
            cancelled_through,
            operation_id,
        });
        let result = self
            .runtime
            .check_cancelled()
            .and_then(|()| self.request_result(request));
        self.runtime.executor.cancellation.borrow_mut().take();
        result
    }

    pub fn request_result(&mut self, request: &SourceRequest) -> Result<SourceResult> {
        ensure!(
            matches!(request.action.as_str(), "musicUrl" | "pic" | "lyric"),
            "Unsupported LX action: {}",
            request.action
        );
        ensure!(
            request.source == "local" || request.action == "musicUrl",
            "LX pic/lyric actions are supported only for local sources"
        );
        let capabilities = self
            .manifest
            .sources
            .get(&request.source)
            .context("Source platform was not declared during initialization")?;
        ensure!(
            capabilities.actions.contains(&request.action),
            "Source did not declare action {}",
            request.action
        );
        if request.action == "musicUrl" {
            let quality = request
                .info
                .get("type")
                .context("Request info.type is missing")?;
            if request.source == "local" {
                ensure!(
                    quality.is_null(),
                    "Local source request info.type must be null"
                );
            } else {
                let quality = quality
                    .as_str()
                    .context("Non-local musicUrl info.type must be a quality string")?;
                ensure!(
                    capabilities.qualitys.iter().any(|item| item == quality),
                    "Source does not support requested quality {quality}"
                );
            }
        }
        ensure!(
            request.info.get("musicInfo").is_some_and(Value::is_object),
            "Request info.musicInfo must contain flattened legacy track fields"
        );
        let request_json = serde_json::to_string(request)?;
        self.runtime.begin_operation(OperationKind::Request)?;
        if let Err(error) = self
            .runtime
            .eval(&format!("__lxHost.dispatch({request_json});"))
        {
            self.runtime.end_operation();
            return Err(error);
        }
        self.runtime.drain()?;
        if let Some(message) = self.runtime.json("__lxHost.responseError()")?.as_str() {
            return Err(SourceRequestFailure {
                kind: if message == "服务器繁忙" {
                    SourceFailureKind::RateLimited
                } else {
                    SourceFailureKind::Rejected
                },
                message: message.to_owned(),
            }
            .into());
        }
        let result = self.runtime.json("__lxHost.result()")?;
        if request.action == "lyric" {
            let bundle =
                LyricBundle::from_value(result.get("result").context("Lyric result is missing")?)?;
            validate_source_lyrics(&bundle)?;
            return Ok(SourceResult::Lyrics(bundle));
        }
        let url = result
            .get("result")
            .and_then(Value::as_str)
            .context("musicUrl/pic handler must resolve to a string")?;
        ensure!(
            url.encode_utf16().count() <= 2048
                && (url.starts_with("http:") || url.starts_with("https:")),
            "Source returned an invalid HTTP(S) URL"
        );
        let parsed = reqwest::Url::parse(url).context("Source returned a malformed URL")?;
        ensure!(
            matches!(parsed.scheme(), "http" | "https") && parsed.host_str().is_some(),
            "Source URL must have an HTTP(S) host"
        );
        ensure!(
            parsed.username().is_empty() && parsed.password().is_none(),
            "Credentials in source URLs are not supported"
        );
        Ok(SourceResult::Url(url.into()))
    }
}

/// UTF-16 limits match LX's JavaScript string-length contract. Optional wrong types
/// are rejected rather than silently converted to null; valid absence is preserved.
pub fn validate_source_lyrics(bundle: &LyricBundle) -> Result<()> {
    bundle.validate()?;
    let lyric = match &bundle.lyric {
        LyricField::Text(text) => text,
        _ => bail!("Local lyric result.lyric must be a string"),
    };
    ensure!(
        lyric.encode_utf16().count() <= 51_200,
        "Local lyric result.lyric exceeds 51200 UTF-16 units"
    );
    for (name, field, limit) in [
        ("tlyric", &bundle.tlyric, 5120),
        ("rlyric", &bundle.rlyric, 5120),
        ("lxlyric", &bundle.lxlyric, 8192),
    ] {
        if let LyricField::Text(text) = field {
            ensure!(
                text.encode_utf16().count() < limit,
                "Local lyric result.{name} must contain fewer than {limit} UTF-16 units"
            );
        }
    }
    Ok(())
}

struct Runtime {
    context: Context,
    hooks: Rc<Rejections>,
    http: Rc<RefCell<crate::source_http::HttpBridge>>,
    executor: Rc<BoundedJobs>,
    async_bridge: Rc<RefCell<AsyncBridge>>,
    operation_kind: OperationKind,
    operation_started: Instant,
    eval_cpu: Duration,
}

#[derive(Clone, Copy)]
enum OperationKind {
    Init,
    Request,
}

struct TimerEntry {
    due: Instant,
    delay: Duration,
    repeating: bool,
}
struct AsyncBridge {
    timers: BTreeMap<u64, TimerEntry>,
    next_id: u64,
    created: usize,
    fired: usize,
    active: bool,
    events: VecDeque<SourceHostEvent>,
    emitted: usize,
}
impl Default for AsyncBridge {
    fn default() -> Self {
        Self {
            timers: BTreeMap::new(),
            next_id: 1,
            created: 0,
            fired: 0,
            active: false,
            events: VecDeque::new(),
            emitted: 0,
        }
    }
}
impl AsyncBridge {
    fn begin(&mut self) {
        self.timers.clear();
        self.created = 0;
        self.fired = 0;
        self.emitted = 0;
        self.active = true;
    }
    fn end(&mut self) {
        self.timers.clear();
        self.active = false;
    }
    fn schedule(&mut self, delay: f64, repeating: bool) -> Result<u64> {
        ensure!(
            self.active,
            "Source timers are available only during an active operation"
        );
        ensure!(
            self.timers.len() < MAX_PENDING_TIMERS,
            "Source pending timer limit (64) exceeded"
        );
        ensure!(
            self.created < MAX_TIMERS_PER_OPERATION,
            "Source timer creation budget (256) exceeded"
        );
        ensure!(
            self.next_id < 9_007_199_254_740_991,
            "Source timer identifier budget exceeded"
        );
        let delay = if !delay.is_finite() || delay < MIN_TIMER_DELAY_MS as f64 {
            MIN_TIMER_DELAY_MS
        } else {
            (delay.trunc() as u64).min(MAX_TIMER_DELAY_MS)
        };
        let id = self.next_id;
        self.next_id += 1;
        self.created += 1;
        let delay = Duration::from_millis(delay);
        self.timers.insert(
            id,
            TimerEntry {
                due: Instant::now() + delay,
                delay,
                repeating,
            },
        );
        Ok(id)
    }
    fn poll(&mut self) -> Result<Vec<u64>> {
        if !self.active {
            return Ok(Vec::new());
        }
        let now = Instant::now();
        let mut due = self
            .timers
            .iter()
            .filter(|(_, entry)| entry.due <= now)
            .map(|(id, entry)| (entry.due, *id))
            .collect::<Vec<_>>();
        due.sort_unstable();
        due.truncate(1);
        ensure!(
            due.len() <= MAX_TIMER_CALLBACKS_PER_OPERATION.saturating_sub(self.fired),
            "Source timer callback budget (512) exceeded"
        );
        self.fired += due.len();
        for (_, id) in &due {
            let repeating = self.timers[id].repeating;
            if repeating {
                let entry = self.timers.get_mut(id).unwrap();
                entry.due = now + entry.delay;
            } else {
                self.timers.remove(id);
            }
        }
        Ok(due.into_iter().map(|(_, id)| id).collect())
    }
    fn emit(&mut self, event: SourceHostEvent) -> Result<()> {
        ensure!(
            self.active,
            "Source host events require an active operation"
        );
        ensure!(
            self.emitted < MAX_HOST_EVENTS_PER_OPERATION && self.events.len() < 64,
            "Source host event budget exceeded"
        );
        self.emitted += 1;
        self.events.push_back(event);
        Ok(())
    }
}

fn async_bridge(context: &mut Context) -> JsResult<Rc<RefCell<AsyncBridge>>> {
    context
        .get_data::<Rc<RefCell<AsyncBridge>>>()
        .cloned()
        .ok_or_else(|| {
            JsNativeError::error()
                .with_message("Source async bridge is unavailable")
                .into()
        })
}
fn async_error(error: anyhow::Error) -> boa_engine::JsError {
    JsNativeError::error()
        .with_message(error.to_string())
        .into()
}
fn native_timer_schedule(
    _: &JsValue,
    args: &[JsValue],
    context: &mut Context,
) -> JsResult<JsValue> {
    let delay = args
        .first()
        .map(|value| value.to_number(context))
        .transpose()?
        .unwrap_or(0.0);
    let repeating = args.get(1).is_some_and(JsValue::to_boolean);
    let id = async_bridge(context)?
        .borrow_mut()
        .schedule(delay, repeating)
        .map_err(async_error)?;
    Ok(JsValue::from(id as f64))
}
fn native_timer_cancel(_: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    if let Some(id) = args.first().and_then(JsValue::as_number) {
        async_bridge(context)?
            .borrow_mut()
            .timers
            .remove(&(id as u64));
    }
    Ok(JsValue::undefined())
}
fn native_timer_poll(_: &JsValue, _: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let timers = async_bridge(context)?
        .borrow_mut()
        .poll()
        .map_err(async_error)?;
    Ok(JsString::from(serde_json::to_string(&timers).expect("timer IDs serialize")).into())
}
fn native_host_event(_: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let text = args
        .first()
        .ok_or_else(|| JsNativeError::typ().with_message("Source host event metadata is missing"))?
        .to_string(context)?
        .to_std_string_escaped();
    if text.len() > 16 * 1024 {
        return Err(JsNativeError::range()
            .with_message("Source host event metadata is too large")
            .into());
    }
    let mut event: SourceHostEvent = serde_json::from_str(&text)
        .map_err(|error| JsNativeError::typ().with_message(error.to_string()))?;
    if let SourceHostEvent::UpdateAlert { log, update_url } = &mut event {
        if log.is_empty() || log.encode_utf16().count() > 1027 {
            return Err(JsNativeError::typ()
                .with_message("Source update alert log is invalid")
                .into());
        }
        if update_url.as_ref().is_some_and(|url| {
            url.encode_utf16().count() > 1024
                || reqwest::Url::parse(url).map_or(true, |url| {
                    !matches!(url.scheme(), "http" | "https")
                        || url.host_str().is_none()
                        || !url.username().is_empty()
                        || url.password().is_some()
                })
        }) {
            *update_url = None;
        }
    }
    async_bridge(context)?
        .borrow_mut()
        .emit(event)
        .map_err(async_error)?;
    Ok(JsValue::undefined())
}

impl Runtime {
    fn new(script: &str, remote_allowed: bool) -> Result<Self> {
        ensure!(
            script.len() <= MAX_SCRIPT_BYTES,
            "Source script exceeds runtime size limit"
        );
        let hooks = Rc::new(Rejections::default());
        let executor = Rc::new(BoundedJobs::default());
        let mut context = Context::builder()
            .host_hooks(hooks.clone())
            .job_executor(executor.clone())
            .module_loader(Rc::new(IdleModuleLoader))
            .build()
            .map_err(|error| anyhow!("Could not create source runtime: {error}"))?;
        context
            .runtime_limits_mut()
            .set_loop_iteration_limit(100_000);
        context.runtime_limits_mut().set_recursion_limit(64);
        context.runtime_limits_mut().set_stack_size_limit(4096);
        let http = crate::source_http::register(&mut context, remote_allowed)?;
        let async_bridge = Rc::new(RefCell::new(AsyncBridge::default()));
        context.insert_data(async_bridge.clone());
        for (name, length, function) in [
            (
                "__lx_timer_schedule",
                2,
                native_timer_schedule
                    as fn(&JsValue, &[JsValue], &mut Context) -> JsResult<JsValue>,
            ),
            ("__lx_timer_cancel", 1, native_timer_cancel),
            ("__lx_timer_poll", 0, native_timer_poll),
            ("__lx_host_event", 1, native_host_event),
        ] {
            context
                .register_global_callable(
                    JsString::from(name),
                    length,
                    NativeFunction::from_fn_ptr(function),
                )
                .map_err(|error| anyhow!("Could not register source event pump: {error}"))?;
        }
        crate::source_utils::register(&mut context)
            .map_err(|error| anyhow!("Could not register source utilities: {error}"))?;
        // Shared memory would allow native Atomics.wait to block beyond loop limits.
        context
            .register_global_property(
                JsString::from("SharedArrayBuffer"),
                JsValue::undefined(),
                Attribute::empty(),
            )
            .map_err(|error| anyhow!("Could not disable shared memory: {error}"))?;
        let mut runtime = Self {
            context,
            hooks,
            http,
            executor,
            async_bridge,
            operation_kind: OperationKind::Init,
            operation_started: Instant::now(),
            eval_cpu: Duration::ZERO,
        };
        let leading = script.trim_start_matches('\u{feff}').trim_start();
        let metadata = leading
            .strip_prefix("/*")
            .and_then(|comment| comment.find("*/").map(|end| read_metadata(&comment[..end])))
            .unwrap_or_default();
        let mut current_script = serde_json::to_value(metadata)?;
        current_script["rawScript"] = Value::String(script.into());
        runtime.eval(&format!("Object.defineProperty(globalThis,'__lx_script_info',{{value:{current_script},configurable:true}});"))?;
        runtime.eval(include_str!("source_bootstrap.js"))?;
        runtime.begin_operation(OperationKind::Init)?;
        runtime
            .eval(script)
            .context("Source initialization failed")?;
        runtime.drain().context("Source initialization failed")?;
        Ok(runtime)
    }

    fn eval(&mut self, script: &str) -> Result<JsValue> {
        self.check_cancelled()?;
        ensure!(
            self.eval_cpu + self.executor.active_time.get() < HOST_BUDGET,
            "Source operation CPU budget (5 seconds) exceeded"
        );
        let started = Instant::now();
        let result = self
            .context
            .eval(Source::from_bytes(script))
            .map_err(|error| anyhow!("{error}"));
        self.eval_cpu += started.elapsed();
        self.check_cancelled()?;
        ensure!(
            self.eval_cpu + self.executor.active_time.get() < HOST_BUDGET,
            "Source operation CPU budget (5 seconds) exceeded"
        );
        result
    }

    fn drain(&mut self) -> Result<()> {
        let result = self.drain_until_idle();
        self.end_operation();
        result
    }

    fn begin_operation(&mut self, kind: OperationKind) -> Result<()> {
        self.end_operation();
        self.operation_kind = kind;
        self.operation_started = Instant::now();
        self.eval_cpu = Duration::ZERO;
        self.http.borrow_mut().begin_operation();
        self.executor.reset_budget();
        self.hooks.promises.borrow_mut().clear();
        self.async_bridge.borrow_mut().begin();
        Ok(())
    }

    fn end_operation(&mut self) {
        self.http.borrow_mut().cancel_all();
        self.async_bridge.borrow_mut().end();
        // Cleanup is host-owned, bounded code; exhausted user CPU budgets must not skip it.
        let _ = self
            .context
            .eval(Source::from_bytes("__lxHost.clearAsync();"));
        self.http.borrow_mut().cancel_all();
        self.async_bridge.borrow_mut().end();
        self.executor.queue.borrow_mut().clear();
    }

    fn drain_until_idle(&mut self) -> Result<()> {
        self.check_cancelled()?;
        self.run_microtasks()?;
        if self.operation_complete()? {
            return Ok(());
        }
        loop {
            self.check_cancelled()?;
            ensure!(
                self.operation_started.elapsed() <= OPERATION_BUDGET,
                "Source operation exceeded its 70-second deadline"
            );
            if self.http.borrow().has_pending() {
                self.eval("__lxHost.pollRequests();")?;
                self.run_microtasks()?;
                if self.operation_complete()? {
                    return Ok(());
                }
            }
            if self
                .async_bridge
                .borrow()
                .timers
                .values()
                .any(|timer| timer.due <= Instant::now())
            {
                self.eval("__lxHost.pollTimers();")?;
                self.run_microtasks()?;
                if self.operation_complete()? {
                    return Ok(());
                }
            }
            if !self.http.borrow().has_pending() && self.async_bridge.borrow().timers.is_empty() {
                return Ok(());
            }
            let wait = self
                .async_bridge
                .borrow()
                .timers
                .values()
                .map(|timer| timer.due.saturating_duration_since(Instant::now()))
                .min()
                .unwrap_or(Duration::from_millis(10))
                .min(Duration::from_millis(10));
            if !wait.is_zero() {
                std::thread::sleep(wait);
            }
        }
    }

    fn operation_complete(&mut self) -> Result<bool> {
        Ok(self
            .eval(match self.operation_kind {
                OperationKind::Init => "__lxHost.isComplete('init')",
                OperationKind::Request => "__lxHost.isComplete('request')",
            })?
            .to_boolean())
    }

    fn run_microtasks(&mut self) -> Result<()> {
        self.check_cancelled()?;
        let result = self
            .context
            .run_jobs()
            .map_err(|error| anyhow!("Source jobs failed: {error}"));
        self.check_cancelled()?;
        result?;
        ensure!(
            self.eval_cpu + self.executor.active_time.get() < HOST_BUDGET,
            "Source operation CPU budget (5 seconds) exceeded"
        );
        if let Some(message) = self.hooks.first_error(&mut self.context) {
            bail!("Unhandled source Promise rejection: {message}");
        }
        Ok(())
    }

    fn check_cancelled(&self) -> Result<()> {
        if self
            .executor
            .cancellation
            .borrow()
            .as_ref()
            .is_some_and(OperationCancellation::is_cancelled)
        {
            return Err(SourceRequestCancelled.into());
        }
        Ok(())
    }

    fn json(&mut self, expression: &str) -> Result<Value> {
        let value = self.eval(expression)?;
        let text = value
            .as_string()
            .context("Host adapter did not return JSON text")?
            .to_std_string_escaped();
        serde_json::from_str(&text).context("Host adapter returned invalid JSON")
    }

    fn manifest(&mut self) -> Result<RuntimeManifest> {
        let value = self.json("__lxHost.manifest()")?;
        serde_json::from_value(value).context("Source initialization returned invalid capabilities")
    }
}

#[derive(Default)]
struct Rejections {
    promises: RefCell<Vec<JsObject<Promise>>>,
}

impl HostHooks for Rejections {
    fn promise_rejection_tracker(
        &self,
        promise: &JsObject<Promise>,
        operation: OperationType,
        _: &mut Context,
    ) {
        match operation {
            OperationType::Reject => self.promises.borrow_mut().push(promise.clone()),
            OperationType::Handle => self.promises.borrow_mut().retain(|item| item != promise),
        }
    }
}

impl Rejections {
    fn first_error(&self, context: &mut Context) -> Option<String> {
        let promise = self.promises.borrow().first()?.clone();
        let promise = JsPromise::from_object(promise.upcast()).ok()?;
        let reason = promise.state().as_rejected()?.clone();
        if let Some(object) = reason.as_object()
            && let Ok(message) = object.get(JsString::from("message"), context)
            && let Some(message) = message.as_string()
        {
            return Some(message.to_std_string_escaped());
        }
        Some(reason.display().to_string())
    }
}

#[derive(Default)]
struct BoundedJobs {
    queue: RefCell<VecDeque<Job>>,
    overflow: Cell<bool>,
    count: Cell<usize>,
    active_time: Cell<Duration>,
    cancellation: RefCell<Option<OperationCancellation>>,
}

impl BoundedJobs {
    fn reset_budget(&self) {
        self.queue.borrow_mut().clear();
        self.overflow.set(false);
        self.count.set(0);
        self.active_time.set(Duration::ZERO);
    }
}

impl JobExecutor for BoundedJobs {
    fn enqueue_job(self: Rc<Self>, job: Job, _: &mut Context) {
        if self.queue.borrow().len() >= MAX_JOBS {
            self.overflow.set(true);
        } else {
            self.queue.borrow_mut().push_back(job);
        }
    }

    fn run_jobs(self: Rc<Self>, context: &mut Context) -> JsResult<()> {
        let started = Instant::now();
        loop {
            if self
                .cancellation
                .borrow()
                .as_ref()
                .is_some_and(OperationCancellation::is_cancelled)
            {
                self.queue.borrow_mut().clear();
                return Err(JsNativeError::error()
                    .with_message("Source request was cancelled")
                    .into());
            }
            let next = self.queue.borrow_mut().pop_front();
            let Some(job) = next else {
                self.active_time
                    .set(self.active_time.get() + started.elapsed());
                return Ok(());
            };
            self.count.set(self.count.get() + 1);
            if self.overflow.get()
                || self.count.get() > MAX_JOBS
                || self.active_time.get() + started.elapsed() > HOST_BUDGET
            {
                self.queue.borrow_mut().clear();
                return Err(JsNativeError::error()
                    .with_message("Source Promise job budget exceeded")
                    .into());
            }
            match job {
                Job::PromiseJob(job) => {
                    job.call(context)?;
                }
                Job::GenericJob(job) => {
                    job.call(context)?;
                }
                Job::FinalizationRegistryCleanupJob(_) => {}
                _ => {
                    return Err(JsNativeError::error()
                        .with_message("Unsupported asynchronous source job")
                        .into());
                }
            }
        }
    }
}

#[cfg(test)]
fn loopback_http(url: &str, options: &str) -> Result<Value> {
    crate::source_http::test_request(url, options)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::thread;

    const DIRECT: &str = include_str!("../fixtures/legal-direct-source.js");
    const HTTP_FIXTURE: &str = include_str!("../fixtures/legal-local-source.js");
    const INIT: &str = "lx.send('inited',{sources:{local:{type:'music',actions:['musicUrl','pic','lyric'],qualitys:[]}}});";

    fn request(url: &str) -> SourceRequest {
        SourceRequest {
            source: "local".into(),
            action: "musicUrl".into(),
            info: json!({
                "type": null, "musicInfo": { "name": "Fixture tone", "singer": "LX Native Lab",
                "source": "local", "songmid": "tone-1", "interval": "0:45", "albumName": "Original synthetic audio",
                "img": "", "typeUrl": {}, "albumId": "", "types": [], "_types": {},
                "filePath": url, "ext": "wav" }
            }),
        }
    }

    fn script(handler: &str) -> String {
        format!("lx.on('request',{handler}); {INIT}")
    }

    fn serve_once(
        body: String,
        status: &str,
        extra_headers: &str,
    ) -> (String, thread::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let status = status.to_owned();
        let extra_headers = extra_headers.to_owned();
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let mut request_bytes = [0; 4096];
            let count = stream.read(&mut request_bytes).unwrap();
            let response = format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n{extra_headers}\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes()).unwrap();
            String::from_utf8_lossy(&request_bytes[..count]).into_owned()
        });
        (base, handle)
    }

    #[test]
    fn import_does_not_execute_and_approval_binds_to_hash() {
        let source = import_script("/*\n * @name Static only\n * @version 1.0\n */\nthrow new Error('must not execute'); while(true){}").unwrap();
        assert_eq!(source.metadata.name, "Static only");
        assert!(!source.approved);
        assert!(source.require_approval().is_err());
        let mut approved = source;
        approved.approve().unwrap();
        approved.require_approval().unwrap();
        approved.script.push_str("\n// changed");
        assert!(approved.require_approval().is_err());
    }

    #[test]
    fn imported_source_roundtrip_preserves_consent_and_script() {
        let mut source = import_script(DIRECT).unwrap();
        source.approve().unwrap();
        let saved = serde_json::to_string(&source).unwrap();
        let restored: ImportedSource = serde_json::from_str(&saved).unwrap();
        restored.require_approval().unwrap();
        assert_eq!(restored.script, DIRECT);
        assert_eq!(restored.sha256.len(), 64);
    }

    #[test]
    fn metadata_uses_upstream_utf16_limits_and_ellipsis() {
        let metadata = read_metadata(&format!(
            "\n * @name {}\n * @description {}\n * @version {}\n * @author {}\n * @homepage {}\n",
            "a".repeat(25),
            "音".repeat(37),
            "v".repeat(37),
            "x".repeat(57),
            "u".repeat(1025)
        ));
        assert_eq!(metadata.name, format!("{}...", "a".repeat(24)));
        assert_eq!(metadata.description, format!("{}...", "音".repeat(36)));
        assert_eq!(metadata.version, format!("{}...", "v".repeat(36)));
        assert_eq!(metadata.author, format!("{}...", "x".repeat(56)));
        assert_eq!(metadata.homepage, format!("{}...", "u".repeat(1024)));
        assert_eq!(
            read_metadata(&format!(" * @name {}", "🎵".repeat(12))).name,
            "🎵".repeat(12)
        );
        assert_eq!(
            read_metadata(&format!(" * @name {}", "🎵".repeat(13))).name,
            format!("{}...", "🎵".repeat(12))
        );
        assert_eq!(
            read_metadata(&format!(" * @name {}🎵", "a".repeat(23))).name,
            format!("{}\u{fffd}...", "a".repeat(23))
        );
    }

    #[test]
    fn damaged_or_missing_metadata_is_rejected_without_execution() {
        assert!(import_script(include_str!("../fixtures/broken-source.js")).is_err());
        assert!(import_script("lx.send('inited',{});").is_err());
        assert!(import_script("/* unclosed").is_err());
        assert!(import_script(&format!("/* */{}", " ".repeat(MAX_SCRIPT_BYTES))).is_err());
    }

    #[test]
    fn initialization_manifest_and_legacy_local_request_work() {
        let manifest = execute_init(DIRECT).unwrap();
        assert_eq!(manifest.sources["local"].kind, "music");
        assert!(manifest.sources["local"].qualitys.is_empty());
        let url = "http://127.0.0.1:32123/tone-1.wav";
        assert_eq!(execute_request(DIRECT, &request(url)).unwrap(), url);
        let mut bad = request(url);
        bad.info["type"] = json!("128k");
        assert!(execute_request(DIRECT, &bad).is_err());
        bad = request(url);
        bad.info["musicInfo"]
            .as_object_mut()
            .unwrap()
            .remove("songmid");
        assert!(execute_request(DIRECT, &bad).is_err());
    }

    #[test]
    fn missing_handler_and_initializer_exception_are_errors() {
        assert!(execute_init(include_str!("../fixtures/no-handler-source.js")).is_err());
        let error = execute_init(include_str!("../fixtures/init-throw-source.js")).unwrap_err();
        assert!(format!("{error:#}").contains("initialization"));
        assert!(execute_init("lx.on('request',async()=> 'http://127.0.0.1/a');").is_err());
    }

    #[test]
    fn duplicate_initialization_and_unhandled_rejection_fail() {
        assert!(
            execute_init(&format!(
                "{} {INIT}",
                script("async()=> 'http://127.0.0.1/a'")
            ))
            .is_err()
        );
        assert!(
            execute_init(&format!(
                "{} Promise.reject(new Error('fixture rejection'));",
                script("async()=> 'http://127.0.0.1/a'")
            ))
            .is_err()
        );
        execute_init(&format!(
            "{} Promise.reject(new Error('handled')).catch(()=>{{}});",
            script("async()=> 'http://127.0.0.1/a'")
        ))
        .unwrap();
    }

    #[test]
    fn request_registration_replaces_and_session_preserves_closures() {
        let fixture = format!(
            "let count=0; lx.on('request', async()=> 'http://127.0.0.1/old'); {}",
            script("async()=> 'http://127.0.0.1/tone-'+(++count)+'.wav'")
        );
        let mut session = SourceSession::new(&fixture).unwrap();
        assert_eq!(
            session
                .request(&request("http://127.0.0.1:1/tone-1.wav"))
                .unwrap(),
            "http://127.0.0.1/tone-1.wav"
        );
        assert_eq!(
            session
                .request(&request("http://127.0.0.1:1/tone-1.wav"))
                .unwrap(),
            "http://127.0.0.1/tone-2.wav"
        );
    }

    #[test]
    fn request_failure_does_not_poison_the_enabled_session() {
        let fixture = format!(
            "let attempts=0; {}",
            script(
                "async()=> {if(++attempts===1)throw Error('intentional first failure');return 'http://127.0.0.1/recovered.wav';}"
            )
        );
        let mut session = SourceSession::new(&fixture).unwrap();
        let request = request("http://127.0.0.1:1/tone-1.wav");
        assert!(session.request(&request).is_err());
        assert_eq!(
            session.request(&request).unwrap(),
            "http://127.0.0.1/recovered.wav"
        );
    }

    #[test]
    fn synchronous_handlers_bad_urls_and_unsupported_actions_fail() {
        let request = request("http://127.0.0.1:1/tone-1.wav");
        assert!(execute_request(&script("()=> 'http://127.0.0.1/a'"), &request).is_err());
        assert!(
            execute_request(include_str!("../fixtures/wrong-url-source.js"), &request).is_err()
        );
        assert!(
            execute_request(&script("async()=> ({url:'http://127.0.0.1/a'})"), &request).is_err()
        );
        assert!(
            execute_request(
                &script("async()=> 'https://user:secret@example.test/a'"),
                &request
            )
            .is_err()
        );
        let mut lyric = request;
        lyric.action = "lyric".into();
        assert!(
            format!("{}", execute_request(DIRECT, &lyric).unwrap_err())
                .contains("Source did not declare action lyric")
        );
    }

    #[test]
    fn capability_filtering_and_quality_validation_match_lx_contract() {
        let fixture = "lx.on('request',async()=> 'http://127.0.0.1/a'); lx.send('inited',{sources:{kw:{type:'music',actions:['musicUrl','search'],qualitys:['128k','320k','invented']},alien:{type:'music',actions:['musicUrl'],qualitys:[]}}});";
        let manifest = execute_init(fixture).unwrap();
        assert_eq!(manifest.sources.len(), 1);
        assert_eq!(manifest.sources["kw"].actions, ["musicUrl"]);
        assert_eq!(manifest.sources["kw"].qualitys, ["128k", "320k"]);
        let mut request = request("http://127.0.0.1:1/a");
        request.source = "kw".into();
        request.info["type"] = json!("flac");
        assert!(execute_request(fixture, &request).is_err());
        request.info["type"] = json!("128k");
        execute_request(fixture, &request).unwrap();
    }

    #[test]
    fn infinite_loop_and_recursive_microtasks_are_bounded() {
        assert!(execute_init("while(true){}").is_err());
        let fixture = format!(
            "{} const loop=()=>Promise.resolve().then(loop); loop();",
            script("async()=> 'http://127.0.0.1/a'")
        );
        let error = execute_init(&fixture).unwrap_err();
        assert!(format!("{error:#}").contains("budget"));
    }

    #[test]
    fn invalid_crypto_keys_and_unavailable_host_storage_report_errors() {
        let fixture = script(
            "async()=> { lx.utils.crypto.rsaEncrypt(lx.utils.buffer.from('a'),'b'); return 'http://127.0.0.1/a'; }",
        );
        let error = execute_request(&fixture, &request("http://127.0.0.1:1/a")).unwrap_err();
        let message = format!("{error:#}");
        assert!(
            message.contains("RSA key must be an unencrypted RSA PEM"),
            "{message}"
        );
        assert!(execute_init("setTimeout('invalid callback',1);").is_err());
        assert!(execute_init("lx.storage.get('key');").is_err());
    }

    #[test]
    fn current_script_metadata_and_pic_info_follow_official_shape() {
        let fixture = format!(
            "/*\n * @name Metadata fixture\n * @version 1.2.3\n */\n{}",
            script(
                "async({action,info})=> {if(lx.currentScriptInfo.name!=='Metadata fixture'||lx.currentScriptInfo.version!=='1.2.3'||!lx.currentScriptInfo.rawScript.startsWith('/*'))throw Error('metadata failed');if(action!=='pic'||'type' in info)throw Error('pic shape failed');return 'http://127.0.0.1/cover.png';}"
            )
        );
        let mut pic = request("http://127.0.0.1:1/a");
        pic.action = "pic".into();
        pic.info.as_object_mut().unwrap().remove("type");
        assert_eq!(
            execute_request(&fixture, &pic).unwrap(),
            "http://127.0.0.1/cover.png"
        );
    }

    #[test]
    fn loopback_callback_fixture_requests_real_catalog() {
        // The fixture discovers the loopback catalog from this session's filePath.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let url = format!("{base}/tone-1.wav");
        let body = json!([{ "id": "tone-1", "title": "Fixture tone", "url": url }]).to_string();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buffer = [0; 4096];
            let count = stream.read(&mut buffer).unwrap();
            assert!(
                String::from_utf8_lossy(&buffer[..count]).starts_with("GET /catalog.json HTTP/1.1")
            );
            write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
        });
        assert_eq!(execute_request(HTTP_FIXTURE, &request(&url)).unwrap(), url);
        server.join().unwrap();
    }

    #[test]
    fn loopback_request_can_initialize_asynchronously() {
        let (base, server) = serve_once("{\"ready\":true}".into(), "200 OK", "");
        let fixture = format!(
            "lx.on('request',async()=>'{base}/a'); lx.request('{base}/init',{{}},(err,resp,body)=>{{if(err)throw err;if(resp.statusCode!==200||!body.ready)throw Error('bad response');{INIT}}});"
        );
        execute_init(&fixture).unwrap();
        assert!(server.join().unwrap().starts_with("GET /init "));
    }

    #[test]
    fn loopback_policy_and_invalid_transport_options_are_enforced() {
        assert!(loopback_http("https://example.com/", "{}").is_err());
        assert!(loopback_http("http://192.168.1.1/", "{}").is_err());
        assert!(loopback_http("http://127.0.0.1:1/", r#"{"method":"CONNECT"}"#).is_err());
        assert!(loopback_http("http://127.0.0.1:1/", r#"{"body":"x"}"#).is_err());
        assert!(
            loopback_http("http://127.0.0.1:1/", r#"{"headers":{"Host":"forbidden"}}"#).is_err()
        );
        let (base, server) =
            serve_once("".into(), "302 Found", "Location: https://example.com/\r\n");
        assert_eq!(loopback_http(&base, "{}").unwrap()["statusCode"], 302);
        server.join().unwrap();
    }

    #[test]
    fn callback_errors_and_send_before_dispatch_cancellation_work() {
        let fixture = script(
            "()=>new Promise((resolve,reject)=>{lx.request('https://example.com/',{},(error,response,body)=>{if(!error||response!==null||body!==null)reject(Error('invalid error callback'));else reject(error);});})",
        );
        assert!(
            format!(
                "{:#}",
                execute_request(&fixture, &request("http://127.0.0.1:1/a")).unwrap_err()
            )
            .contains("Network access denied")
        );
        let own_listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let own_url = format!("http://{}/", own_listener.local_addr().unwrap());
        let fixture = format!(
            "let called=false; const cancel=lx.request('{own_url}',{{}},()=>{{called=true;}});cancel();cancel(); {}",
            script(
                "async()=> {if(called)throw Error('cancel failed');return 'http://127.0.0.1/a';}"
            )
        );
        execute_request(&fixture, &request("http://127.0.0.1:1/a")).unwrap();
    }

    #[test]
    fn non_json_http_body_and_raw_byte_metadata_are_preserved() {
        let (base, server) =
            serve_once("fixture text".into(), "404 Not Found", "X-Fixture: yes\r\n");
        let result = loopback_http(&base, "{}").unwrap();
        assert_eq!(result["statusCode"], 404);
        assert_eq!(result["body"], "fixture text");
        assert_eq!(result["bytes"], 12);
        assert_eq!(result["raw"], "Zml4dHVyZSB0ZXh0");
        server.join().unwrap();
    }
}

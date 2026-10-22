//! Bounded speculative playback state. This module never resolves a source or reads media.
use super::queue::{AdvanceReason, MAX_QUEUE_ID_BYTES, QueueEntry, QueueState};
use sha2::{Digest, Sha256};
use std::{
    fmt,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU8, Ordering},
    },
    time::Duration,
};

pub const MAX_PRELOAD_URL_BYTES: usize = 2_048;
pub const MAX_PRELOAD_QUALITY_BYTES: usize = 64;
pub const MAX_PRELOAD_OPERATION_BYTES: usize = 64;
pub const PRELOAD_SEEK_SETTLE: Duration = Duration::from_secs(3);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PreloadState {
    Idle,
    Resolving,
    AwaitingApproval,
    Preparing,
    Ready,
    Failed,
    Cancelled,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PreloadFailure {
    Resolution,
    Preparation,
    Unavailable,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Invalidation {
    Queue,
    Mode,
    List,
    Dislike,
    TemporaryQueue,
    Quality,
    Source,
    SourceRevoked,
    Seek,
    Stop,
    Clear,
    Playback,
    ApprovalRevoked,
    Cache,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PreloadError {
    InvalidField,
    InvalidUrl,
    InvalidState,
    Stale,
    EpochExhausted,
}

impl fmt::Display for PreloadError {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        out.write_str(match self {
            Self::InvalidField => "Invalid preload context field",
            Self::InvalidUrl => "Invalid preload media URL",
            Self::InvalidState => "Preload operation is in a different state",
            Self::Stale => "Preload operation is stale or cancelled",
            Self::EpochExhausted => "Preload operation epoch is exhausted",
        })
    }
}
impl std::error::Error for PreloadError {}
pub type PreloadResult<T> = Result<T, PreloadError>;

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct UrlDigest(pub [u8; 32]);
impl UrlDigest {
    pub fn of(url: &str) -> Self {
        Self(Sha256::digest(url.as_bytes()).into())
    }
}
impl fmt::Debug for UrlDigest {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        out.write_str("UrlDigest(")?;
        for byte in self.0 {
            write!(out, "{byte:02x}")?;
        }
        out.write_str(")")
    }
}

/// Authority is either a verified source installation or an explicitly registered catalog asset.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PreloadAuthority {
    Source {
        sha256: String,
        policy_version: u32,
    },
    CatalogAsset {
        asset_id: String,
        content_stamp: String,
    },
}
impl PreloadAuthority {
    fn validate(&self) -> PreloadResult<()> {
        let (digest, policy_valid) = match self {
            Self::Source {
                sha256,
                policy_version,
            } => (sha256, *policy_version > 0),
            Self::CatalogAsset {
                asset_id,
                content_stamp,
            } => {
                valid_id(asset_id)?;
                (content_stamp, true)
            }
        };
        if !policy_valid
            || digest.len() != 64
            || !digest.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(PreloadError::InvalidField);
        }
        Ok(())
    }
}

/// A context read from the actor immediately before each state transition.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreloadContext {
    pub playback_generation: u64,
    pub current_entry_id: String,
    pub queue_revision: u64,
    pub authority: PreloadAuthority,
    pub source_epoch: u64,
    pub actual_quality: String,
    pub approval_epoch: u64,
    pub authority_approved: bool,
}
impl PreloadContext {
    pub fn validate(&self) -> PreloadResult<()> {
        valid_id(&self.current_entry_id)?;
        self.authority.validate()?;
        if matches!(self.authority, PreloadAuthority::CatalogAsset { .. })
            && self.actual_quality != "local"
        {
            return Err(PreloadError::InvalidField);
        }
        if self.actual_quality.is_empty()
            || self.actual_quality.len() > MAX_PRELOAD_QUALITY_BYTES
            || self.actual_quality.chars().any(char::is_control)
        {
            return Err(PreloadError::InvalidField);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreloadFence {
    pub operation_epoch: u64,
    pub playback_generation: u64,
    pub current_entry_id: String,
    pub queue_revision: u64,
    pub candidate_entry_id: String,
    pub authority: PreloadAuthority,
    pub source_epoch: u64,
    pub actual_quality: String,
    pub approval_epoch: u64,
    pub url_digest: Option<UrlDigest>,
}
impl PreloadFence {
    fn new(epoch: u64, context: &PreloadContext, candidate: &QueueEntry) -> Self {
        Self {
            operation_epoch: epoch,
            playback_generation: context.playback_generation,
            current_entry_id: context.current_entry_id.clone(),
            queue_revision: context.queue_revision,
            candidate_entry_id: candidate.entry_id.clone(),
            authority: context.authority.clone(),
            source_epoch: context.source_epoch,
            actual_quality: context.actual_quality.clone(),
            approval_epoch: context.approval_epoch,
            url_digest: None,
        }
    }
    pub fn matches(&self, context: &PreloadContext) -> bool {
        context.authority_approved
            && self.playback_generation == context.playback_generation
            && self.current_entry_id == context.current_entry_id
            && self.queue_revision == context.queue_revision
            && self.authority == context.authority
            && self.source_epoch == context.source_epoch
            && self.actual_quality == context.actual_quality
            && self.approval_epoch == context.approval_epoch
    }
}

#[derive(Clone, Copy, Debug)]
pub struct TriggerTiming {
    pub duration_seconds: f64,
    pub position_seconds: f64,
    pub since_seek: Duration,
}
impl TriggerTiming {
    pub fn should_preload(self) -> bool {
        self.duration_seconds.is_finite()
            && self.position_seconds.is_finite()
            && self.duration_seconds > 10.0
            && self.position_seconds >= 0.0
            && self.position_seconds <= self.duration_seconds
            && self.duration_seconds - self.position_seconds < 10.0
            && self.since_seek >= PRELOAD_SEEK_SETTLE
    }
}

/// Cancellation is shared with workers; speculative cancellation cannot stop a promoted owner.
#[derive(Clone)]
pub struct CancelFlag(Arc<AtomicU8>, Arc<AtomicBool>);
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LeaseOwnership {
    Speculative,
    Cancelled,
    Promoted,
}
impl Default for CancelFlag {
    fn default() -> Self {
        Self::new()
    }
}
impl CancelFlag {
    pub fn new() -> Self {
        Self(Arc::new(AtomicU8::new(0)), Arc::new(AtomicBool::new(false)))
    }
    pub fn ownership(&self) -> LeaseOwnership {
        match self.0.load(Ordering::Acquire) {
            0 => LeaseOwnership::Speculative,
            2 => LeaseOwnership::Promoted,
            _ => LeaseOwnership::Cancelled,
        }
    }
    pub fn is_cancelled(&self) -> bool {
        self.ownership() == LeaseOwnership::Cancelled
    }
    pub fn cancel_speculative(&self) -> bool {
        let cancelled = self
            .0
            .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
            .is_ok();
        if cancelled {
            self.1.store(true, Ordering::Release);
        }
        cancelled
    }
    /// Foreground owner cancellation, including Stop and output shutdown.
    pub fn cancel(&self) {
        self.0.store(1, Ordering::Release);
        self.1.store(true, Ordering::Release);
    }
    /// Interruptible media workers share cancellation without inferring lease ownership.
    pub fn transport_cancel(&self) -> Arc<AtomicBool> {
        self.1.clone()
    }
    pub(crate) fn promote(&self) -> bool {
        self.0
            .compare_exchange(0, 2, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }
    fn same(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}
impl fmt::Debug for CancelFlag {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        out.debug_tuple("CancelFlag")
            .field(&self.ownership())
            .finish()
    }
}

#[derive(Clone, Debug)]
pub struct ResolveAction {
    pub operation_id: String,
    pub fence: PreloadFence,
    pub candidate: QueueEntry,
    pub cancel: CancelFlag,
}

#[derive(Clone)]
pub struct PrepareAction {
    pub operation_id: String,
    pub fence: PreloadFence,
    pub candidate: QueueEntry,
    pub url: String,
    pub cancel: CancelFlag,
}
impl fmt::Debug for PrepareAction {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        out.debug_struct("PrepareAction")
            .field("operation_id", &self.operation_id)
            .field("fence", &self.fence)
            .field("candidate", &self.candidate)
            .field("url", &"[redacted]")
            .field("cancel", &self.cancel)
            .finish()
    }
}

#[derive(Clone, Debug)]
pub struct CancelAction {
    pub operation_id: String,
    pub fence: PreloadFence,
    pub reason: Invalidation,
    pub cancel: CancelFlag,
}

/// Exact URL is available only for the separate user approval projection.
#[derive(Clone)]
pub struct PreloadSnapshot {
    pub state: PreloadState,
    pub operation_id: Option<String>,
    pub fence: Option<PreloadFence>,
    pub candidate: Option<QueueEntry>,
    pub pending_url: Option<String>,
    pub failure: Option<PreloadFailure>,
}
impl fmt::Debug for PreloadSnapshot {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        out.debug_struct("PreloadSnapshot")
            .field("state", &self.state)
            .field("operation_id", &self.operation_id)
            .field("fence", &self.fence)
            .field("candidate", &self.candidate)
            .field(
                "pending_url",
                &self.pending_url.as_ref().map(|_| "[redacted]"),
            )
            .field("failure", &self.failure)
            .finish()
    }
}

struct SpeculativeLease<T> {
    value: Option<T>,
    cancel: CancelFlag,
}
impl<T> Drop for SpeculativeLease<T> {
    fn drop(&mut self) {
        self.cancel.cancel_speculative();
    }
}

/// Dropping this lease cancels foreground work. `into_parts` transfers that duty to the actor.
pub struct ForegroundLease<T> {
    value: Option<T>,
    cancel: CancelFlag,
}
impl<T> ForegroundLease<T> {
    pub fn value(&self) -> &T {
        self.value.as_ref().expect("owned foreground value")
    }
    pub fn cancel_flag(&self) -> CancelFlag {
        self.cancel.clone()
    }
    pub fn into_parts(mut self) -> (T, CancelFlag) {
        (
            self.value.take().expect("owned foreground value"),
            self.cancel.clone(),
        )
    }
}
impl<T> Drop for ForegroundLease<T> {
    fn drop(&mut self) {
        if self.value.is_some() {
            self.cancel.cancel();
        }
    }
}

struct Operation<T> {
    id: String,
    fence: PreloadFence,
    candidate: QueueEntry,
    state: PreloadState,
    url: Option<String>,
    failure: Option<PreloadFailure>,
    cancel: CancelFlag,
    lease: Option<SpeculativeLease<T>>,
}

pub struct PreloadService<T> {
    epoch: u64,
    operation: Option<Operation<T>>,
}
impl<T> Default for PreloadService<T> {
    fn default() -> Self {
        Self::new()
    }
}
impl<T> PreloadService<T> {
    pub fn new() -> Self {
        Self {
            epoch: 0,
            operation: None,
        }
    }
    pub fn operation_epoch(&self) -> u64 {
        self.epoch
    }
    pub fn snapshot(&self) -> PreloadSnapshot {
        let op = self.operation.as_ref();
        PreloadSnapshot {
            state: op.map_or(PreloadState::Idle, |op| op.state),
            operation_id: op.map(|op| op.id.clone()),
            fence: op.map(|op| op.fence.clone()),
            candidate: op.map(|op| op.candidate.clone()),
            pending_url: op
                .filter(|op| op.state == PreloadState::AwaitingApproval)
                .and_then(|op| op.url.clone()),
            failure: op.and_then(|op| op.failure),
        }
    }
    /// Uses the queue's Ended plan, preserving random selection and temporary FIFO ownership.
    pub fn begin(
        &mut self,
        queue: &mut QueueState,
        context: &PreloadContext,
        timing: TriggerTiming,
        eligible: impl Fn(&QueueEntry) -> bool,
    ) -> PreloadResult<Option<ResolveAction>> {
        context.validate()?;
        if queue.revision != context.queue_revision
            || queue.current.as_ref().map(|entry| entry.entry_id.as_str())
                != Some(context.current_entry_id.as_str())
        {
            return Err(PreloadError::Stale);
        }
        if !context.authority_approved || !timing.should_preload() {
            return Ok(None);
        }
        if self
            .operation
            .as_ref()
            .is_some_and(|op| op.fence.operation_epoch == self.epoch && op.fence.matches(context))
        {
            return Ok(None);
        }
        queue.validate().map_err(|_| PreloadError::InvalidField)?;
        for entry in queue
            .entries
            .iter()
            .chain(queue.temporary.iter())
            .chain(queue.current.iter())
            .chain(queue.history().iter())
            .chain(queue.future().iter())
        {
            valid_entry(entry)?;
        }
        let epoch = self
            .epoch
            .checked_add(1)
            .ok_or(PreloadError::EpochExhausted)?;
        let Some(candidate) = queue.preview_next(AdvanceReason::Ended, eligible) else {
            return Ok(None);
        };
        // No fallible work follows replacement of the previous operation.
        self.cancel_current();
        self.epoch = epoch;
        let action = ResolveAction {
            operation_id: format!("preload-{epoch}"),
            fence: PreloadFence::new(epoch, context, &candidate),
            candidate,
            cancel: CancelFlag::new(),
        };
        self.operation = Some(Operation {
            id: action.operation_id.clone(),
            fence: action.fence.clone(),
            candidate: action.candidate.clone(),
            state: PreloadState::Resolving,
            url: None,
            failure: None,
            cancel: action.cancel.clone(),
            lease: None,
        });
        Ok(Some(action))
    }
    /// Resolving an approved source does not grant permission for a media request.
    pub fn resolved(
        &mut self,
        operation_id: &str,
        context: &PreloadContext,
        url: String,
    ) -> PreloadResult<bool> {
        context.validate()?;
        valid_operation_id(operation_id)?;
        validate_url(&url)?;
        let Some(op) = self.matching_mut(operation_id, context) else {
            return Ok(false);
        };
        if op.state != PreloadState::Resolving {
            return Ok(false);
        }
        op.fence.url_digest = Some(UrlDigest::of(&url));
        op.url = Some(url);
        op.state = PreloadState::AwaitingApproval;
        Ok(true)
    }
    /// Approval is exact URL and operation scoped; repeating it cannot start another worker.
    pub fn approve(
        &mut self,
        operation_id: &str,
        context: &PreloadContext,
        exact_url: &str,
    ) -> PreloadResult<PrepareAction> {
        context.validate()?;
        valid_operation_id(operation_id)?;
        validate_url(exact_url)?;
        let op = self
            .matching_mut(operation_id, context)
            .ok_or(PreloadError::Stale)?;
        if op.state != PreloadState::AwaitingApproval {
            return Err(PreloadError::InvalidState);
        }
        if op.fence.url_digest != Some(UrlDigest::of(exact_url))
            || op.url.as_deref() != Some(exact_url)
        {
            return Err(PreloadError::Stale);
        }
        let action = PrepareAction {
            operation_id: op.id.clone(),
            fence: op.fence.clone(),
            candidate: op.candidate.clone(),
            url: exact_url.into(),
            cancel: op.cancel.clone(),
        };
        op.state = PreloadState::Preparing;
        Ok(action)
    }
    /// Required immediately before transport execution and again on preparation completion.
    pub fn validate_action(&self, action: &PrepareAction, context: &PreloadContext) -> bool {
        context.validate().is_ok()
            && validate_url(&action.url).is_ok()
            && self.operation.as_ref().is_some_and(|op| {
                op.state == PreloadState::Preparing
                    && op.id == action.operation_id
                    && op.fence == action.fence
                    && op.fence.operation_epoch == self.epoch
                    && op.fence.matches(context)
                    && op.candidate == action.candidate
                    && op.cancel.same(&action.cancel)
                    && op.cancel.ownership() == LeaseOwnership::Speculative
                    && op.url.as_deref() == Some(action.url.as_str())
                    && op.fence.url_digest == Some(UrlDigest::of(&action.url))
            })
    }
    pub fn ready(
        &mut self,
        action: &PrepareAction,
        context: &PreloadContext,
        value: T,
    ) -> PreloadResult<bool> {
        context.validate()?;
        if !self.validate_action(action, context) {
            return Ok(false);
        }
        let op = self.operation.as_mut().expect("validated operation");
        op.lease = Some(SpeculativeLease {
            value: Some(value),
            cancel: op.cancel.clone(),
        });
        op.state = PreloadState::Ready;
        Ok(true)
    }
    pub fn fail(
        &mut self,
        operation_id: &str,
        context: &PreloadContext,
        failure: PreloadFailure,
    ) -> PreloadResult<bool> {
        context.validate()?;
        valid_operation_id(operation_id)?;
        let epoch = self.epoch;
        let Some(op) = self.operation.as_mut().filter(|op| {
            op.id == operation_id
                && op.fence.operation_epoch == epoch
                && op.fence.matches(context)
                && op.cancel.ownership() != LeaseOwnership::Promoted
        }) else {
            return Ok(false);
        };
        if !matches!(
            op.state,
            PreloadState::Resolving | PreloadState::AwaitingApproval | PreloadState::Preparing
        ) {
            return Ok(false);
        }
        op.cancel.cancel_speculative();
        op.url = None;
        op.failure = Some(failure);
        op.state = PreloadState::Failed;
        Ok(true)
    }
    /// Call after the real advance, using its entry and the actor's context captured before advance.
    pub fn promote(
        &mut self,
        operation_id: &str,
        pre_advance_context: &PreloadContext,
        advanced_entry_id: &str,
    ) -> PreloadResult<ForegroundLease<T>> {
        pre_advance_context.validate()?;
        valid_operation_id(operation_id)?;
        valid_id(advanced_entry_id)?;
        let op = self
            .matching_mut(operation_id, pre_advance_context)
            .ok_or(PreloadError::Stale)?;
        if op.state != PreloadState::Ready {
            return Err(PreloadError::InvalidState);
        }
        if op.fence.candidate_entry_id != advanced_entry_id
            || op.lease.as_ref().is_none_or(|lease| lease.value.is_none())
        {
            return Err(PreloadError::Stale);
        }
        // CAS is the ownership boundary. Any racing speculative cancellation makes promotion fail.
        if !op.cancel.promote() {
            return Err(PreloadError::Stale);
        }
        let mut lease = op.lease.take().expect("validated ready lease");
        let foreground = ForegroundLease {
            value: lease.value.take(),
            cancel: lease.cancel.clone(),
        };
        self.operation = None;
        Ok(foreground)
    }
    /// Every queue/source/quality/approval mutation calls this before scheduling more work.
    pub fn invalidate(&mut self, reason: Invalidation) -> PreloadResult<Option<CancelAction>> {
        let epoch = self
            .epoch
            .checked_add(1)
            .ok_or(PreloadError::EpochExhausted)?;
        let action = self
            .operation
            .as_ref()
            .filter(|op| !matches!(op.state, PreloadState::Failed | PreloadState::Cancelled))
            .map(|op| CancelAction {
                operation_id: op.id.clone(),
                fence: op.fence.clone(),
                reason,
                cancel: op.cancel.clone(),
            });
        self.cancel_current();
        self.epoch = epoch;
        Ok(action)
    }
    /// Returns the external cancellation action when a current fence has ceased to match.
    pub fn synchronize(
        &mut self,
        context: &PreloadContext,
        reason: Invalidation,
    ) -> PreloadResult<Option<CancelAction>> {
        context.validate()?;
        if self.operation.as_ref().is_some_and(|op| {
            !matches!(op.state, PreloadState::Failed | PreloadState::Cancelled)
                && !op.fence.matches(context)
        }) {
            self.invalidate(reason)
        } else {
            Ok(None)
        }
    }
    fn matching_mut(&mut self, id: &str, context: &PreloadContext) -> Option<&mut Operation<T>> {
        let epoch = self.epoch;
        self.operation.as_mut().filter(|op| {
            op.id == id
                && op.fence.operation_epoch == epoch
                && op.fence.matches(context)
                && op.cancel.ownership() == LeaseOwnership::Speculative
        })
    }
    fn cancel_current(&mut self) {
        if let Some(op) = self.operation.as_mut() {
            op.cancel.cancel_speculative();
            op.lease = None;
            op.url = None;
            if op.state != PreloadState::Failed {
                op.state = PreloadState::Cancelled;
            }
        }
    }
}
impl<T> Drop for PreloadService<T> {
    fn drop(&mut self) {
        self.cancel_current();
    }
}

fn valid_id(id: &str) -> PreloadResult<()> {
    if id.is_empty() || id.len() > MAX_QUEUE_ID_BYTES || id.chars().any(char::is_control) {
        Err(PreloadError::InvalidField)
    } else {
        Ok(())
    }
}
fn valid_operation_id(id: &str) -> PreloadResult<()> {
    if id.len() > MAX_PRELOAD_OPERATION_BYTES {
        return Err(PreloadError::InvalidField);
    }
    valid_id(id)
}
fn valid_entry(entry: &QueueEntry) -> PreloadResult<()> {
    valid_id(&entry.entry_id)?;
    valid_id(&entry.track_id)?;
    valid_id(&entry.list_id)
}
fn validate_url(url: &str) -> PreloadResult<()> {
    if url.is_empty() || url.len() > MAX_PRELOAD_URL_BYTES || url.chars().any(char::is_control) {
        return Err(PreloadError::InvalidUrl);
    }
    let parsed = reqwest::Url::parse(url).map_err(|_| PreloadError::InvalidUrl)?;
    if !matches!(parsed.scheme(), "http" | "https")
        || parsed.host_str().is_none()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.fragment().is_some()
    {
        return Err(PreloadError::InvalidUrl);
    }
    Ok(())
}

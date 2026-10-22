use anyhow::{Result, ensure};
use reqwest::Url;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fmt,
    sync::{
        Arc,
        atomic::{AtomicU8, Ordering},
    },
    time::Duration,
};

pub const MAX_DOWNLOAD_TASKS: usize = 100;
pub const MAX_DOWNLOAD_WORKERS: usize = 4;
pub const MAX_DOWNLOAD_BYTES: u64 = 1024 * 1024 * 1024;
pub const MAX_OWNED_PART_BYTES: u64 = 4 * MAX_DOWNLOAD_BYTES;
pub const COPY_BUFFER_BYTES: usize = 64 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DownloadState {
    Waiting,
    Resolving,
    AwaitingUrlApproval,
    Transferring,
    Retrying,
    Pausing,
    Paused,
    Verifying,
    CommittingAudio,
    PostProcessing,
    Completed,
    Failed,
    Cancelling,
    Cancelled,
}
impl DownloadState {
    /// Process restart never resumes network or filesystem mutation implicitly.
    pub fn after_restart(self) -> Self {
        match self {
            Self::Completed | Self::Failed | Self::Cancelled | Self::Paused => self,
            _ => Self::Paused,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "policy", rename_all = "snake_case")]
pub enum CollisionPolicy {
    Skip,
    Overwrite { observed: FileIdentity },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct FileIdentity {
    pub device: u64,
    pub object: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct DownloadContext {
    pub profile_id: String,
    pub task_id: String,
    pub operation_epoch: u64,
    pub source_id: String,
    pub source_content_sha256: String,
    pub policy_epoch: u64,
}
impl DownloadContext {
    pub fn validate(&self) -> Result<()> {
        for value in [&self.profile_id, &self.task_id, &self.source_id] {
            ensure!(
                !value.is_empty() && value.len() <= 256 && !value.chars().any(char::is_control),
                "Invalid download context identity"
            );
        }
        ensure!(
            self.operation_epoch != 0 && self.policy_epoch != 0,
            "Invalid download context epoch"
        );
        ensure!(
            self.source_content_sha256.len() == 64
                && self
                    .source_content_sha256
                    .bytes()
                    .all(|b| b.is_ascii_hexdigit()),
            "Invalid approved source content fingerprint"
        );
        Ok(())
    }
}

/// Created only after the caller has matched the pending URL, source and policy.
/// The digest is an equality guard; it does not establish consent on its own.
#[derive(Clone)]
pub struct ApprovedDownloadUrl {
    pub(crate) url: Url,
    pub(crate) context: DownloadContext,
    pub(crate) digest: String,
}
impl ApprovedDownloadUrl {
    pub fn from_exact_approval(
        pending_url: &str,
        approved_url: &str,
        pending_context: &DownloadContext,
        approved_context: &DownloadContext,
    ) -> Result<Self> {
        pending_context.validate()?;
        ensure!(
            pending_context == approved_context && pending_url == approved_url,
            "Download approval does not match its pending context"
        );
        let url = validate_url(approved_url)?;
        Ok(Self {
            url,
            context: approved_context.clone(),
            digest: format!("{:x}", Sha256::digest(approved_url.as_bytes())),
        })
    }
    pub fn context(&self) -> &DownloadContext {
        &self.context
    }
    pub fn digest(&self) -> &str {
        &self.digest
    }
}
impl fmt::Debug for ApprovedDownloadUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ApprovedDownloadUrl")
            .field("context", &self.context)
            .field("digest", &self.digest)
            .finish()
    }
}
pub(crate) fn validate_url(value: &str) -> Result<Url> {
    ensure!(
        value.len() <= 2048 && !value.chars().any(|c| c.is_whitespace() || c.is_control()),
        "Invalid download URL"
    );
    let url = Url::parse(value).map_err(|_| anyhow::anyhow!("Invalid download URL"))?;
    ensure!(
        matches!(url.scheme(), "http" | "https")
            && url.host_str().is_some()
            && url.username().is_empty()
            && url.password().is_none(),
        "Unsupported download URL authority"
    );
    Ok(url)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StopReason {
    Pause,
    Cancel,
    AuthorityRevoked,
    Shutdown,
}
#[derive(Clone, Default)]
pub struct DownloadCancel(Arc<AtomicU8>);
impl DownloadCancel {
    pub fn stop(&self, reason: StopReason) {
        let value = match reason {
            StopReason::Pause => 1,
            StopReason::Cancel => 2,
            StopReason::AuthorityRevoked => 3,
            StopReason::Shutdown => 4,
        };
        let _ = self
            .0
            .compare_exchange(0, value, Ordering::AcqRel, Ordering::Acquire);
    }
    pub fn reason(&self) -> Option<StopReason> {
        match self.0.load(Ordering::Acquire) {
            1 => Some(StopReason::Pause),
            2 => Some(StopReason::Cancel),
            3 => Some(StopReason::AuthorityRevoked),
            4 => Some(StopReason::Shutdown),
            _ => None,
        }
    }
    pub(crate) fn check(&self) -> Result<()> {
        ensure!(self.reason().is_none(), "Download operation stopped");
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct TransferOptions {
    pub connect_timeout: Duration,
    pub header_timeout: Duration,
    pub idle_timeout: Duration,
    pub retry_delay: Duration,
    pub automatic_retries: u8,
    pub representation_restarts: u8,
    pub max_bytes: u64,
}
impl Default for TransferOptions {
    fn default() -> Self {
        Self {
            connect_timeout: Duration::from_secs(10),
            header_timeout: Duration::from_secs(60),
            idle_timeout: Duration::from_secs(20),
            retry_delay: Duration::from_secs(1),
            automatic_retries: 2,
            representation_restarts: 2,
            max_bytes: MAX_DOWNLOAD_BYTES,
        }
    }
}
impl TransferOptions {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            !self.connect_timeout.is_zero()
                && !self.header_timeout.is_zero()
                && !self.idle_timeout.is_zero()
                && !self.retry_delay.is_zero(),
            "Download deadlines must be positive"
        );
        ensure!(
            self.automatic_retries <= 2
                && self.representation_restarts <= 2
                && self.max_bytes > 0
                && self.max_bytes <= MAX_DOWNLOAD_BYTES,
            "Download transfer bounds exceeded"
        );
        Ok(())
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct TransferCheckpoint {
    pub bytes: u64,
    pub sha256: String,
    pub strong_etag: Option<String>,
    pub remote_total: Option<u64>,
    pub completed_body: bool,
    #[serde(default)]
    pub representation: Option<DownloadRepresentationIdentity>,
}

/// Representation provenance is data, and never restores execution approval.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DownloadRepresentationIdentity {
    pub profile_id: String,
    pub task_id: String,
    pub source_id: String,
    pub source_content_sha256: String,
    pub policy_epoch: u64,
    pub exact_url_sha256: String,
    pub resolved_track_sha256: String,
    pub actual_quality: super::planning::DownloadQuality,
}
impl DownloadRepresentationIdentity {
    pub fn from_approved_url(
        approved: &ApprovedDownloadUrl,
        resolved_track_sha256: String,
        actual_quality: super::planning::DownloadQuality,
    ) -> Result<Self> {
        let context = approved.context();
        let identity = Self {
            profile_id: context.profile_id.clone(),
            task_id: context.task_id.clone(),
            source_id: context.source_id.clone(),
            source_content_sha256: context.source_content_sha256.clone(),
            policy_epoch: context.policy_epoch,
            exact_url_sha256: approved.digest().to_owned(),
            resolved_track_sha256,
            actual_quality,
        };
        identity.validate()?;
        Ok(identity)
    }
    pub fn validate(&self) -> Result<()> {
        let context = DownloadContext {
            profile_id: self.profile_id.clone(),
            task_id: self.task_id.clone(),
            operation_epoch: 1,
            source_id: self.source_id.clone(),
            source_content_sha256: self.source_content_sha256.clone(),
            policy_epoch: self.policy_epoch,
        };
        context.validate()?;
        for digest in [&self.exact_url_sha256, &self.resolved_track_sha256] {
            ensure!(
                digest.len() == 64 && digest.bytes().all(|byte| byte.is_ascii_hexdigit()),
                "Invalid download representation fingerprint"
            );
        }
        Ok(())
    }
    pub(crate) fn validate_for(&self, approved: &ApprovedDownloadUrl) -> Result<()> {
        self.validate()?;
        ensure!(
            Self::from_approved_url(
                approved,
                self.resolved_track_sha256.clone(),
                self.actual_quality
            )? == *self,
            "Download representation differs from current URL execution approval"
        );
        Ok(())
    }
}
#[derive(Clone, Debug, Serialize)]
pub struct DownloadProgress {
    pub context: DownloadContext,
    pub state: DownloadState,
    pub bytes: u64,
    pub total: Option<u64>,
    pub retry_count: u8,
    pub representation_resets: u8,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DownloadErrorKind {
    Network,
    Timeout,
    Http,
    Protocol,
    Permission,
    Conflict,
    Disk,
    Verification,
    UnsupportedCapability,
    Internal,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct DownloadError {
    pub kind: DownloadErrorKind,
    pub message: String,
    pub http_status: Option<u16>,
}
impl DownloadError {
    pub(crate) fn new(kind: DownloadErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            http_status: None,
        }
    }
    pub(crate) fn file(error: &std::io::Error) -> Self {
        let kind = match error.kind() {
            std::io::ErrorKind::PermissionDenied => DownloadErrorKind::Permission,
            std::io::ErrorKind::AlreadyExists | std::io::ErrorKind::NotFound => {
                DownloadErrorKind::Conflict
            }
            _ => DownloadErrorKind::Disk,
        };
        Self::new(kind, "Download destination I/O failed")
    }
}

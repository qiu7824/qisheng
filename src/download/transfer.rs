use super::{
    ApprovedDownloadUrl, AudioVerifier, DownloadCancel, DownloadContext, DownloadError,
    DownloadErrorKind, DownloadProgress, DownloadState, OwnedPart, OwnedPartRecord, StopReason,
    TransferCheckpoint, TransferOptions, VerifiedOwnedAudio, model::validate_url,
};
use crate::audio::http_client::{LimitedHttpClient, LimitedResponse};
use anyhow::{Result, ensure};
use reqwest::{
    StatusCode,
    header::{CONTENT_ENCODING, CONTENT_LENGTH, CONTENT_RANGE, ETAG, LOCATION},
};
use std::{
    future::Future,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

/// Revoked by the source owner whenever the selected content or policy changes.
#[derive(Clone)]
pub struct DownloadExecutionLease {
    context: DownloadContext,
    revoked: Arc<std::sync::atomic::AtomicBool>,
}
impl DownloadExecutionLease {
    pub fn from_approved_context(context: DownloadContext) -> Result<Self> {
        context.validate()?;
        Ok(Self {
            context,
            revoked: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        })
    }
    pub fn revoke(&self) {
        self.revoked
            .store(true, std::sync::atomic::Ordering::Release);
    }
    pub(crate) fn check_context(&self, context: &DownloadContext) -> Result<()> {
        self.check(context)
    }
    pub(crate) fn check(&self, context: &DownloadContext) -> Result<()> {
        ensure!(
            context == &self.context && !self.revoked.load(std::sync::atomic::Ordering::Acquire),
            "Download source execution authority revoked"
        );
        Ok(())
    }
}

pub struct TransferJob {
    pub approved_url: ApprovedDownloadUrl,
    pub representation: super::DownloadRepresentationIdentity,
    pub execution_lease: DownloadExecutionLease,
    pub part: OwnedPart,
    pub checkpoint: Option<TransferCheckpoint>,
    pub extension: String,
    pub redirect_count: u8,
    pub options: TransferOptions,
    pub verifier: Arc<dyn AudioVerifier>,
}
impl TransferJob {
    pub fn validate(&self) -> Result<()> {
        self.options.validate()?;
        self.execution_lease.check(self.approved_url.context())?;
        self.representation.validate_for(&self.approved_url)?;
        ensure!(
            self.representation.actual_quality.extension() == self.extension,
            "Download extension differs from its planned representation quality"
        );
        self.part.verify()?;
        ensure!(
            matches!(
                self.extension.as_str(),
                "mp3" | "flac" | "wav" | "ape" | "m4a" | "ogg"
            ),
            "Unsupported download extension"
        );
        ensure!(
            self.redirect_count <= 2,
            "Download redirect hop bound exceeded"
        );
        if let Some(checkpoint) = &self.checkpoint {
            if let Some(identity) = &checkpoint.representation {
                identity.validate()?;
            }
            ensure!(
                checkpoint.bytes <= self.options.max_bytes
                    && checkpoint
                        .remote_total
                        .is_none_or(|n| n <= self.options.max_bytes)
                    && checkpoint.sha256.len() == 64
                    && checkpoint.sha256.bytes().all(|b| b.is_ascii_hexdigit()),
                "Invalid download transfer checkpoint"
            );
            ensure!(
                checkpoint
                    .strong_etag
                    .as_ref()
                    .is_none_or(|s| is_strong_etag(s)),
                "Invalid persisted download validator"
            );
        }
        Ok(())
    }
}

pub enum TransferOutcome {
    Verified(Box<VerifiedOwnedAudio>),
    Stopped {
        reason: StopReason,
        part: OwnedPartRecord,
        checkpoint: Option<TransferCheckpoint>,
    },
    AwaitingRedirectApproval {
        exact_url: String,
        context: DownloadContext,
        redirect_count: u8,
        part: OwnedPartRecord,
        checkpoint: TransferCheckpoint,
    },
    RefreshRequired {
        status: u16,
        part: OwnedPartRecord,
        checkpoint: TransferCheckpoint,
    },
    Failed {
        error: DownloadError,
        part: OwnedPartRecord,
        checkpoint: Option<TransferCheckpoint>,
    },
}
pub(crate) struct TransferView {
    pub progress: DownloadProgress,
    pub checkpoint: Option<TransferCheckpoint>,
}

fn file_error(error: anyhow::Error) -> DownloadError {
    if let Some(io) = error.downcast_ref::<std::io::Error>() {
        DownloadError::file(io)
    } else {
        DownloadError::new(
            DownloadErrorKind::Conflict,
            "Download owned file authority or identity changed",
        )
    }
}
fn check(job: &TransferJob, cancel: &DownloadCancel) -> std::result::Result<(), DownloadError> {
    if job
        .execution_lease
        .check(job.approved_url.context())
        .is_err()
        || job.part.verify().is_err()
    {
        cancel.stop(StopReason::AuthorityRevoked);
        return Err(DownloadError::new(
            DownloadErrorKind::Permission,
            "Download execution authority revoked",
        ));
    }
    if cancel.reason().is_some() {
        return Err(DownloadError::new(
            DownloadErrorKind::Permission,
            "Download operation stopped",
        ));
    }
    Ok(())
}
async fn cancellable<T>(
    future: impl Future<Output = T>,
    deadline: Duration,
    job: &TransferJob,
    cancel: &DownloadCancel,
) -> std::result::Result<T, DownloadError> {
    tokio::pin!(future);
    let timeout = tokio::time::sleep(deadline);
    tokio::pin!(timeout);
    loop {
        check(job, cancel)?;
        tokio::select! {
            value = &mut future => return Ok(value),
            _ = &mut timeout => return Err(DownloadError::new(DownloadErrorKind::Timeout, "Download HTTP deadline exceeded")),
            _ = tokio::time::sleep(Duration::from_millis(20)) => {},
        }
    }
}
async fn retry_wait(
    job: &TransferJob,
    cancel: &DownloadCancel,
) -> std::result::Result<(), DownloadError> {
    cancellable(
        tokio::time::sleep(job.options.retry_delay),
        job.options.retry_delay + Duration::from_millis(100),
        job,
        cancel,
    )
    .await
}
fn publish(
    view: &Mutex<TransferView>,
    state: DownloadState,
    bytes: u64,
    total: Option<u64>,
    retries: u8,
    resets: u8,
) {
    let mut view = view.lock().unwrap_or_else(|e| e.into_inner());
    view.progress.state = state;
    view.progress.bytes = bytes;
    view.progress.total = total;
    view.progress.retry_count = retries;
    view.progress.representation_resets = resets;
}

enum AttemptResult {
    Complete,
    Reset,
    Redirect(String),
    Refresh(u16),
}
struct Representation {
    identity: Option<super::DownloadRepresentationIdentity>,
    etag: Option<String>,
    total: Option<u64>,
    completed: bool,
}

pub(crate) async fn execute(
    mut job: TransferJob,
    cancel: DownloadCancel,
    view: Arc<Mutex<TransferView>>,
) -> TransferOutcome {
    let mut representation = Representation {
        identity: None,
        etag: None,
        total: None,
        completed: false,
    };
    let result = run(&mut job, &cancel, &view, &mut representation).await;
    // A stop token prohibits further data writes. Its own final fsync/hash is
    // necessary to report durable partial state, and still verifies the grant.
    let checkpoint = job
        .part
        .checkpoint_if_initialized(
            representation.etag.clone(),
            representation.total,
            representation.completed,
        )
        .ok()
        .map(|mut checkpoint| {
            checkpoint.representation = representation.identity.clone();
            checkpoint
        });
    if let Some(checkpoint) = &checkpoint {
        let mut view = view.lock().unwrap_or_else(|e| e.into_inner());
        view.progress.bytes = checkpoint.bytes;
        view.progress.total = checkpoint.remote_total;
        view.checkpoint = Some(checkpoint.clone());
    }
    let record = job.part.record().clone();
    if let Some(reason) = cancel.reason() {
        return TransferOutcome::Stopped {
            reason,
            part: record,
            checkpoint,
        };
    }
    match result {
        Ok(AttemptResult::Complete) => {
            publish(
                &view,
                DownloadState::Verifying,
                checkpoint.as_ref().map_or(0, |c| c.bytes),
                representation.total,
                0,
                0,
            );
            match job.verifier.verify(&job.part, &job.extension, &cancel) {
                Ok(verification) => {
                    if let Err(error) = check(&job, &cancel) {
                        if let Some(reason) = cancel.reason() {
                            return TransferOutcome::Stopped {
                                reason,
                                part: record,
                                checkpoint,
                            };
                        }
                        return TransferOutcome::Failed {
                            error,
                            part: record,
                            checkpoint,
                        };
                    }
                    match checkpoint.and_then(|checkpoint| {
                        job.part
                            .into_verified(
                                checkpoint,
                                verification,
                                job.execution_lease.clone(),
                                job.approved_url.context().clone(),
                                cancel.clone(),
                            )
                            .ok()
                    }) {
                        Some(audio) => TransferOutcome::Verified(Box::new(audio)),
                        None => TransferOutcome::Failed {
                            error: DownloadError::new(
                                DownloadErrorKind::Verification,
                                "Cannot seal verified download audio",
                            ),
                            part: record,
                            checkpoint: None,
                        },
                    }
                }
                Err(_) => {
                    let _ = check(&job, &cancel);
                    if let Some(reason) = cancel.reason() {
                        TransferOutcome::Stopped {
                            reason,
                            part: record,
                            checkpoint,
                        }
                    } else {
                        TransferOutcome::Failed {
                            error: DownloadError::new(
                                DownloadErrorKind::Verification,
                                "Downloaded file failed complete audio verification",
                            ),
                            part: record,
                            checkpoint,
                        }
                    }
                }
            }
        }
        Ok(AttemptResult::Redirect(url)) => {
            if let Some(checkpoint) = checkpoint {
                TransferOutcome::AwaitingRedirectApproval {
                    exact_url: url,
                    context: job.approved_url.context().clone(),
                    redirect_count: job.redirect_count + 1,
                    part: record,
                    checkpoint,
                }
            } else {
                TransferOutcome::Failed {
                    error: DownloadError::new(
                        DownloadErrorKind::Disk,
                        "Cannot checkpoint redirected download",
                    ),
                    part: record,
                    checkpoint: None,
                }
            }
        }
        Ok(AttemptResult::Refresh(status)) => {
            if let Some(checkpoint) = checkpoint {
                TransferOutcome::RefreshRequired {
                    status,
                    part: record,
                    checkpoint,
                }
            } else {
                TransferOutcome::Failed {
                    error: DownloadError::new(
                        DownloadErrorKind::Disk,
                        "Cannot checkpoint download URL refresh",
                    ),
                    part: record,
                    checkpoint: None,
                }
            }
        }
        Ok(AttemptResult::Reset) => {
            unreachable!("Representation reset is handled within its fixed budget")
        }
        Err(error) => TransferOutcome::Failed {
            error,
            part: record,
            checkpoint,
        },
    }
}

async fn run(
    job: &mut TransferJob,
    cancel: &DownloadCancel,
    view: &Mutex<TransferView>,
    representation: &mut Representation,
) -> std::result::Result<AttemptResult, DownloadError> {
    job.validate().map_err(file_error)?;
    check(job, cancel)?;
    job.part.bind_execution(
        job.execution_lease.clone(),
        job.approved_url.context().clone(),
        cancel.clone(),
    );
    let mut retries = 0u8;
    let mut resets = 0u8;
    let matched = job
        .checkpoint
        .as_ref()
        .and_then(|checkpoint| checkpoint.representation.as_ref())
        == Some(&job.representation);
    if !matched {
        if job.part.len().map_err(file_error)? > 0 {
            reset(job, cancel, representation, &mut resets)?;
        }
        job.checkpoint = None;
    }
    let initial = job.part.hash(cancel).map_err(file_error)?;
    if let Some(checkpoint) = &job.checkpoint {
        if checkpoint.bytes != initial.0 || checkpoint.sha256 != initial.1 {
            return Err(DownloadError::new(
                DownloadErrorKind::Conflict,
                "Owned partial checkpoint does not match its actual bytes",
            ));
        }
        representation.etag = checkpoint.strong_etag.clone();
        representation.total = checkpoint.remote_total;
    } else if initial.0 != 0 {
        return Err(DownloadError::new(
            DownloadErrorKind::Conflict,
            "Nonempty owned partial requires a verified checkpoint",
        ));
    }
    representation.identity = Some(job.representation.clone());
    let client = LimitedHttpClient::new(job.options.connect_timeout).map_err(|_| {
        DownloadError::new(
            DownloadErrorKind::Internal,
            "Cannot initialize download HTTP transport",
        )
    })?;
    loop {
        check(job, cancel)?;
        if job.part.len().map_err(file_error)? > 0 && representation.etag.is_none() {
            reset(job, cancel, representation, &mut resets)?;
        }
        let bytes = job.part.len().map_err(file_error)?;
        publish(
            view,
            DownloadState::Transferring,
            bytes,
            representation.total,
            retries,
            resets,
        );
        match attempt(&client, job, cancel, view, representation, retries, resets).await {
            Ok(AttemptResult::Reset) => reset(job, cancel, representation, &mut resets)?,
            Ok(value) => return Ok(value),
            Err(error)
                if matches!(
                    error.kind,
                    DownloadErrorKind::Network
                        | DownloadErrorKind::Timeout
                        | DownloadErrorKind::Http
                ) && retries < job.options.automatic_retries
                    && cancel.reason().is_none() =>
            {
                retries += 1;
                job.part.sync().map_err(file_error)?;
                publish(
                    view,
                    DownloadState::Retrying,
                    job.part.len().map_err(file_error)?,
                    representation.total,
                    retries,
                    resets,
                );
                retry_wait(job, cancel).await?;
            }
            Err(error) => return Err(error),
        }
    }
}
fn reset(
    job: &mut TransferJob,
    cancel: &DownloadCancel,
    representation: &mut Representation,
    resets: &mut u8,
) -> std::result::Result<(), DownloadError> {
    if *resets >= job.options.representation_restarts {
        return Err(DownloadError::new(
            DownloadErrorKind::Protocol,
            "Download representation restart budget exhausted",
        ));
    }
    job.part.reset(cancel).map_err(file_error)?;
    *resets += 1;
    representation.etag = None;
    representation.total = None;
    representation.completed = false;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn attempt(
    client: &LimitedHttpClient,
    job: &mut TransferJob,
    cancel: &DownloadCancel,
    view: &Mutex<TransferView>,
    representation: &mut Representation,
    retries: u8,
    resets: u8,
) -> std::result::Result<AttemptResult, DownloadError> {
    let size = job.part.len().map_err(file_error)?;
    let overlap = size.min(10) as usize;
    let start = size - overlap as u64;
    let tail = job.part.tail(overlap, cancel).map_err(file_error)?;
    let range = (size > 0).then(|| format!("bytes={start}-"));
    let request = client.get(
        &job.approved_url.url,
        range.as_deref(),
        if size > 0 {
            representation.etag.as_deref()
        } else {
            None
        },
    );
    let mut response = cancellable(request, job.options.header_timeout, job, cancel)
        .await?
        .map_err(|_| {
            DownloadError::new(
                DownloadErrorKind::Network,
                "Download HTTP connection or headers failed",
            )
        })?;
    let status = response.status();
    if matches!(status.as_u16(), 301 | 302 | 303 | 307 | 308) {
        if job.redirect_count >= 2 {
            return Err(DownloadError::new(
                DownloadErrorKind::Protocol,
                "Download redirect hop bound exceeded",
            ));
        }
        let location = response
            .headers()
            .get(LOCATION)
            .and_then(|v| v.to_str().ok())
            .ok_or_else(|| {
                DownloadError::new(
                    DownloadErrorKind::Protocol,
                    "Download redirect has no valid Location",
                )
            })?;
        let next = job.approved_url.url.join(location).map_err(|_| {
            DownloadError::new(
                DownloadErrorKind::Protocol,
                "Download redirect Location is invalid",
            )
        })?;
        validate_url(next.as_str()).map_err(|_| {
            DownloadError::new(
                DownloadErrorKind::Protocol,
                "Download redirect authority is invalid",
            )
        })?;
        return Ok(AttemptResult::Redirect(next.to_string()));
    }
    if matches!(status.as_u16(), 401 | 403 | 410) {
        return Ok(AttemptResult::Refresh(status.as_u16()));
    }
    check_encoding(&response)?;
    let etag = strong_etag(&response);
    if status == StatusCode::RANGE_NOT_SATISFIABLE {
        let total = response
            .headers()
            .get(CONTENT_RANGE)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.strip_prefix("bytes */"))
            .and_then(|s| s.parse::<u64>().ok());
        if size > 0
            && total == Some(size)
            && representation.total == Some(size)
            && etag.is_some()
            && etag == representation.etag
        {
            representation.completed = true;
            return Ok(AttemptResult::Complete);
        }
        return Ok(AttemptResult::Reset);
    }
    if status == StatusCode::OK && size > 0 {
        return Ok(AttemptResult::Reset);
    }
    let mut expected_body = response.content_length();
    let total;
    if status == StatusCode::PARTIAL_CONTENT {
        let parsed = response
            .headers()
            .get(CONTENT_RANGE)
            .and_then(|v| v.to_str().ok())
            .and_then(parse_range)
            .ok_or_else(|| {
                DownloadError::new(
                    DownloadErrorKind::Protocol,
                    "Download partial response has invalid Content-Range",
                )
            })?;
        if parsed.0 != start || parsed.1 >= parsed.2 || parsed.2 > job.options.max_bytes {
            return Err(DownloadError::new(
                DownloadErrorKind::Protocol,
                "Download Content-Range differs from its requested interval",
            ));
        }
        let length = parsed.1 - parsed.0 + 1;
        if expected_body.is_some_and(|n| n != length) {
            return Err(DownloadError::new(
                DownloadErrorKind::Protocol,
                "Download Content-Length disagrees with Content-Range",
            ));
        }
        expected_body = Some(length);
        total = Some(parsed.2);
        if size > 0
            && (etag != representation.etag || representation.total.is_some_and(|n| n != parsed.2))
        {
            return Ok(AttemptResult::Reset);
        }
        if size > 0 && etag.is_none() {
            return Ok(AttemptResult::Reset);
        }
        // Open-ended continuation must end at representation EOF.
        if parsed.1 + 1 != parsed.2 {
            return Err(DownloadError::new(
                DownloadErrorKind::Protocol,
                "Download partial response ends before its requested EOF",
            ));
        }
    } else if status == StatusCode::OK {
        total = expected_body;
    } else {
        return Err(DownloadError {
            kind: DownloadErrorKind::Http,
            message: "Download HTTP status failed".into(),
            http_status: Some(status.as_u16()),
        });
    }
    if total.is_some_and(|n| n > job.options.max_bytes) {
        return Err(DownloadError::new(
            DownloadErrorKind::Protocol,
            "Download representation exceeds byte limit",
        ));
    }
    representation.etag = etag;
    representation.total = total;
    let mut received = 0u64;
    let mut overlap_seen = 0usize;
    let mut last_progress = Instant::now() - Duration::from_secs(1);
    let mut last_sync = Instant::now();
    loop {
        let chunk = cancellable(response.chunk(), job.options.idle_timeout, job, cancel)
            .await?
            .map_err(|_| {
                DownloadError::new(
                    DownloadErrorKind::Network,
                    "Download HTTP DATA framing failed",
                )
            })?;
        let Some(bytes) = chunk else {
            break;
        };
        received = received.checked_add(bytes.len() as u64).ok_or_else(|| {
            DownloadError::new(DownloadErrorKind::Protocol, "Download byte count overflow")
        })?;
        if expected_body.is_some_and(|n| received > n) {
            return Err(DownloadError::new(
                DownloadErrorKind::Protocol,
                "Download response exceeds declared length",
            ));
        }
        let mut begin = 0;
        while overlap_seen < overlap && begin < bytes.len() {
            if bytes[begin] != tail[overlap_seen] {
                return Ok(AttemptResult::Reset);
            }
            overlap_seen += 1;
            begin += 1;
        }
        if begin < bytes.len() {
            check(job, cancel)?;
            job.part
                .append(&bytes[begin..], cancel, job.options.max_bytes)
                .map_err(file_error)?;
        }
        if last_sync.elapsed() >= Duration::from_secs(1) {
            let mut checkpoint = job
                .part
                .checkpoint(
                    cancel,
                    representation.etag.clone(),
                    representation.total,
                    false,
                )
                .map_err(file_error)?;
            checkpoint.representation = representation.identity.clone();
            view.lock().unwrap_or_else(|e| e.into_inner()).checkpoint = Some(checkpoint);
            last_sync = Instant::now();
        }
        if last_progress.elapsed() >= Duration::from_millis(100) {
            publish(
                view,
                DownloadState::Transferring,
                job.part.len().map_err(file_error)?,
                total,
                retries,
                resets,
            );
            last_progress = Instant::now();
        }
    }
    if expected_body.is_some_and(|n| received != n) || overlap_seen != overlap {
        return Err(DownloadError::new(
            DownloadErrorKind::Network,
            "Download response ended before its declared length",
        ));
    }
    let final_length = job.part.len().map_err(file_error)?;
    if final_length == 0 || total.is_some_and(|n| n != final_length) {
        return Err(DownloadError::new(
            DownloadErrorKind::Protocol,
            "Download final length differs from its representation",
        ));
    }
    representation.total = Some(final_length);
    representation.completed = true;
    job.part.sync().map_err(file_error)?;
    publish(
        view,
        DownloadState::Verifying,
        final_length,
        Some(final_length),
        retries,
        resets,
    );
    Ok(AttemptResult::Complete)
}
fn check_encoding(response: &LimitedResponse) -> std::result::Result<(), DownloadError> {
    if response
        .headers()
        .get(CONTENT_ENCODING)
        .is_some_and(|value| {
            value
                .to_str()
                .ok()
                .is_none_or(|value| !value.eq_ignore_ascii_case("identity"))
        })
    {
        return Err(DownloadError::new(
            DownloadErrorKind::Protocol,
            "Download Content-Encoding must be identity",
        ));
    }
    if response.headers().get(CONTENT_LENGTH).is_some_and(|value| {
        value
            .to_str()
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .is_none()
    }) {
        return Err(DownloadError::new(
            DownloadErrorKind::Protocol,
            "Download Content-Length is invalid",
        ));
    }
    Ok(())
}
fn is_strong_etag(value: &str) -> bool {
    value.len() >= 2
        && value.len() <= 1024
        && value.starts_with('"')
        && value.ends_with('"')
        && !value.chars().any(char::is_control)
}
fn strong_etag(response: &LimitedResponse) -> Option<String> {
    response
        .headers()
        .get(ETAG)
        .and_then(|v| v.to_str().ok())
        .filter(|s| is_strong_etag(s))
        .map(str::to_owned)
}
fn parse_range(value: &str) -> Option<(u64, u64, u64)> {
    let (range, total) = value.strip_prefix("bytes ")?.split_once('/')?;
    let (start, end) = range.split_once('-')?;
    let start = start.parse().ok()?;
    let end = end.parse().ok()?;
    let total = total.parse().ok()?;
    (start <= end && end < total).then_some((start, end, total))
}

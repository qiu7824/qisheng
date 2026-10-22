//! Speculative audio resources stay owned by the engine until atomic admission.

use super::decode::DecoderReady;
use super::transport::{ApprovedAudioRequest, CacheIdentity, MediaIoError, MediaIoErrorKind};
use crate::playback::preload::{CancelFlag, LeaseOwnership};
use std::fmt;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

const SPECULATIVE: u8 = 0;
const CANCELLED: u8 = 1;
const PROMOTED: u8 = 2;
pub const MAX_PRELOAD_ENCODED_BYTES: u64 = 2 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AudioPrepareLimits {
    pub encoded_bytes: u64,
    pub pcm_duration: Duration,
    pub prebuffer_duration: Duration,
}
impl Default for AudioPrepareLimits {
    fn default() -> Self {
        Self {
            encoded_bytes: MAX_PRELOAD_ENCODED_BYTES,
            pcm_duration: Duration::from_millis(80),
            prebuffer_duration: Duration::from_millis(40),
        }
    }
}
impl AudioPrepareLimits {
    pub(super) fn validate(&self) -> bool {
        self.encoded_bytes > 0
            && self.encoded_bytes <= MAX_PRELOAD_ENCODED_BYTES
            && !self.pcm_duration.is_zero()
            && self.pcm_duration <= Duration::from_millis(250)
            && !self.prebuffer_duration.is_zero()
            && self.prebuffer_duration <= self.pcm_duration
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AudioPreloadState {
    Preparing,
    Ready,
    Failed,
    Cancelled,
    Promoting,
    Promoted,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize)]
pub enum AudioPreloadErrorKind {
    Cancelled,
    Stale,
    Limit,
    Transport,
    Decode,
    Closed,
}
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
pub struct AudioPreloadError {
    pub kind: AudioPreloadErrorKind,
    pub message: String,
    pub media: Option<MediaIoError>,
}
impl AudioPreloadError {
    pub(super) fn new(kind: AudioPreloadErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            media: None,
        }
    }

    pub(super) fn with_media(mut self, media: Option<MediaIoError>) -> Self {
        if self.kind == AudioPreloadErrorKind::Decode
            && let Some(error) = &media
        {
            self.kind = if error.kind == MediaIoErrorKind::Cancelled {
                AudioPreloadErrorKind::Cancelled
            } else {
                AudioPreloadErrorKind::Transport
            };
        }
        self.media = media;
        self
    }

    pub fn requires_url_refresh(&self) -> bool {
        self.media.as_ref().is_some_and(|error| {
            error.kind == MediaIoErrorKind::Network
                && matches!(error.http_status, Some(401 | 403 | 404 | 410))
        })
    }
}
impl fmt::Display for AudioPreloadError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}
impl std::error::Error for AudioPreloadError {}

#[derive(Clone, Debug)]
pub struct AudioPreloadSnapshot {
    pub operation_id: String,
    pub nonce: u64,
    pub state: AudioPreloadState,
    pub expected_generation: u64,
    pub identity: CacheIdentity,
    pub encoded_bytes: u64,
    pub pcm_seconds: f64,
    pub error: Option<AudioPreloadError>,
}

pub(super) struct Permit {
    pub operation_id: String,
    pub nonce: u64,
    pub expected_generation: u64,
    pub identity: CacheIdentity,
    pub cancel: CancelFlag,
    ownership: AtomicU8,
}
impl Permit {
    pub(super) fn new(
        operation_id: String,
        nonce: u64,
        expected_generation: u64,
        identity: CacheIdentity,
        cancel: CancelFlag,
    ) -> Arc<Self> {
        Arc::new(Self {
            operation_id,
            nonce,
            expected_generation,
            identity,
            cancel,
            ownership: AtomicU8::new(SPECULATIVE),
        })
    }
    pub(super) fn cancelled(&self) -> bool {
        self.ownership.load(Ordering::Acquire) == CANCELLED || self.cancel.is_cancelled()
    }
    pub(super) fn cancel(&self) {
        if self
            .ownership
            .compare_exchange(SPECULATIVE, CANCELLED, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            self.cancel.cancel();
        }
    }
}

/// Sole-use proof of prepared resource ownership; it never clones a PCM consumer.
pub struct AudioPreloadToken {
    pub(super) permit: Arc<Permit>,
    armed: bool,
}
impl AudioPreloadToken {
    pub(super) fn new(permit: Arc<Permit>) -> Self {
        Self {
            permit,
            armed: true,
        }
    }
    pub fn identity(&self) -> &CacheIdentity {
        &self.permit.identity
    }
    pub fn nonce(&self) -> u64 {
        self.permit.nonce
    }
    pub fn is_cancelled(&self) -> bool {
        self.permit.cancelled()
    }
    pub(super) fn claim(&mut self) -> bool {
        match self.permit.cancel.ownership() {
            LeaseOwnership::Speculative if !self.permit.cancel.promote() => return false,
            LeaseOwnership::Cancelled => return false,
            _ => {}
        }
        if self
            .permit
            .ownership
            .compare_exchange(SPECULATIVE, PROMOTED, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return false;
        }
        self.armed = false;
        true
    }
}
impl fmt::Debug for AudioPreloadToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AudioPreloadToken")
            .field("operation_id", &self.permit.operation_id)
            .field("nonce", &self.permit.nonce)
            .field("identity", &self.permit.identity)
            .finish_non_exhaustive()
    }
}
impl Drop for AudioPreloadToken {
    fn drop(&mut self) {
        if self.armed {
            self.permit.cancel();
        }
    }
}

pub struct AudioPrepareTicket {
    receiver: mpsc::Receiver<Result<AudioPreloadToken, AudioPreloadError>>,
    cancel: CancelFlag,
    completed: bool,
}
impl AudioPrepareTicket {
    pub(super) fn new(
        cancel: CancelFlag,
    ) -> (
        mpsc::SyncSender<Result<AudioPreloadToken, AudioPreloadError>>,
        Self,
    ) {
        let (sender, receiver) = mpsc::sync_channel(1);
        (
            sender,
            Self {
                receiver,
                cancel,
                completed: false,
            },
        )
    }
    pub fn try_result(&mut self) -> Option<Result<AudioPreloadToken, AudioPreloadError>> {
        if self.completed {
            return None;
        }
        match self.receiver.try_recv() {
            Ok(result) => {
                self.completed = true;
                Some(result)
            }
            Err(mpsc::TryRecvError::Empty) => None,
            Err(mpsc::TryRecvError::Disconnected) => {
                self.completed = true;
                Some(Err(AudioPreloadError::new(
                    AudioPreloadErrorKind::Closed,
                    "Audio preparation owner closed",
                )))
            }
        }
    }
}
impl Drop for AudioPrepareTicket {
    fn drop(&mut self) {
        if !self.completed {
            self.cancel.cancel_speculative();
        }
    }
}

#[derive(Default)]
pub(super) struct View {
    pub snapshot: Option<AudioPreloadSnapshot>,
    pub cancel: Option<CancelFlag>,
}
pub(super) type SharedView = Arc<Mutex<View>>;
pub(super) struct PreparedStream {
    pub stream: super::ActiveStream,
    pub request: ApprovedAudioRequest,
    pub permit: Arc<Permit>,
    pub reply: Option<mpsc::SyncSender<Result<AudioPreloadToken, AudioPreloadError>>>,
    pub ready: Option<DecoderReady>,
    pub representation_epoch: u64,
    pub cache_epoch: u64,
    pub budget_since: Option<Instant>,
}
pub(super) struct Promotion {
    pub permit: Arc<Permit>,
    pub completed: bool,
}
impl PreparedStream {
    pub(super) fn cancel(&mut self) {
        self.permit.cancel();
        self.permit.cancel.cancel();
        self.stream.cancel();
        self.ready.take();
    }
    pub(super) fn snapshot(
        &self,
        state: AudioPreloadState,
        error: Option<AudioPreloadError>,
    ) -> AudioPreloadSnapshot {
        let decoded = self.stream.decoder.snapshot();
        AudioPreloadSnapshot {
            operation_id: self.permit.operation_id.clone(),
            nonce: self.permit.nonce,
            state,
            expected_generation: self.permit.expected_generation,
            identity: self.permit.identity.clone(),
            encoded_bytes: self.stream.control.status().encoded_downloaded_bytes,
            pcm_seconds: decoded.format.map_or(0.0, |format| {
                decoded.pcm.queued_frames as f64 / format.sample_rate as f64
            }),
            error,
        }
    }
}

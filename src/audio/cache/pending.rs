//! Live RAM availability and independently acknowledged durable publication.
use super::*;
use tokio::sync::Notify;

#[derive(Clone)]
/// Acknowledges the owned payload/header publication after its IO stages settle.
/// Cross-stream reuse independently requires a successful verified seal; a
/// failed optional reuse promotion does not invalidate the durable payload.
pub(crate) struct PublicationTicket {
    completion: Arc<PublicationCompletion>,
    pub(crate) ram_ready: bool,
}
pub(super) struct PublicationCompletion {
    result: Mutex<Option<std::result::Result<(), String>>>,
    changed: Notify,
}
impl PublicationCompletion {
    pub(super) fn new() -> Arc<Self> {
        Arc::new(Self {
            result: Mutex::new(None),
            changed: Notify::new(),
        })
    }
    pub(super) fn complete(&self, result: std::result::Result<(), String>) {
        *self
            .result
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(result);
        self.changed.notify_waiters();
    }
    pub(super) fn is_pending(&self) -> bool {
        self.result
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .is_none()
    }
}
impl PublicationTicket {
    pub(super) fn new(completion: Arc<PublicationCompletion>, ram_ready: bool) -> Self {
        Self {
            completion,
            ram_ready,
        }
    }
    pub(crate) fn result(&self) -> Option<std::result::Result<(), String>> {
        self.completion
            .result
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }
    pub(crate) async fn wait(&self) -> std::result::Result<(), String> {
        loop {
            let changed = self.completion.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if let Some(result) = self.result() {
                return result;
            }
            changed.await;
        }
    }
}
pub(super) struct PendingRam {
    pub generation: u64,
    pub serial: u64,
    pub bytes: Arc<[u8]>,
    pub cancel: Arc<AtomicBool>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum PublicationReadBoundary {
    Ready,
    Pending,
    Failed,
    Closing,
}

impl CacheStore {
    /// Waits on the actual IO owner's condition variable; only accepted RAM
    /// publications of the current object generation participate.
    pub(crate) fn foreground_boundary(
        &self,
        object: &str,
        wait: Duration,
    ) -> PublicationReadBoundary {
        let observe = |state: &State| {
            if state.closing || self.core.access.closing.load(Ordering::Acquire) {
                return PublicationReadBoundary::Closing;
            }
            let Some(entry) = state.objects.get(object) else {
                return PublicationReadBoundary::Ready;
            };
            if entry.publication_error {
                return PublicationReadBoundary::Failed;
            }
            if state.pending_ram.iter().any(|((name, _), pending)| {
                name == object && pending.generation == entry.generation
            }) {
                PublicationReadBoundary::Pending
            } else {
                PublicationReadBoundary::Ready
            }
        };
        let mut state = self
            .core
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if observe(&state) == PublicationReadBoundary::Pending && !wait.is_zero() {
            state = self
                .core
                .changed
                .wait_timeout(state, wait)
                .unwrap_or_else(|error| error.into_inner())
                .0;
        }
        observe(&state)
    }

    pub(crate) fn stage_foreground(
        &self,
        object: &str,
        offset: u64,
        bytes: Vec<u8>,
        cancel: Arc<AtomicBool>,
    ) -> Result<PublicationTicket> {
        let completion = PublicationCompletion::new();
        let ready = self.enqueue(
            object,
            offset,
            bytes,
            cancel,
            WriteReply::Staged(completion.clone()),
            true,
        )?;
        Ok(PublicationTicket::new(completion, ready))
    }
    pub(crate) fn publication_failed(&self, object: &str) -> bool {
        self.core
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .objects
            .get(object)
            .is_some_and(|value| value.publication_error)
    }
}

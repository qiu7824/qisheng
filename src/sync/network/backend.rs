use super::super::{
    Dataset, DislikeAction, ListAction, ListData, PeerCapabilities, SharedKey, SyncMode,
    SyncResult, dislike_digest,
};
use serde_json::Value;
use std::sync::{
    Arc, RwLock, RwLockReadGuard,
    atomic::{AtomicBool, Ordering},
};

#[derive(Clone, Debug)]
pub struct PeerRecord {
    pub peer_id: String,
    pub client_id: String,
    pub key: SharedKey,
    pub device_name: String,
    pub is_mobile: bool,
    pub auth_epoch: u64,
    pub capabilities: PeerCapabilities,
}
#[derive(Clone, Debug)]
pub struct ClientCredentials {
    pub server_id: String,
    pub client_id: String,
    pub key: SharedKey,
    pub server_name: String,
    pub auth_epoch: u64,
}
/// Correlation metadata; it cannot grant data or filesystem authority by itself.
#[derive(Clone, Debug)]
pub struct SessionTicket {
    pub peer_id: String,
    pub auth_epoch: u64,
    pub session_epoch: u64,
    alive: Arc<AtomicBool>,
    commit: Arc<RwLock<()>>,
    fenced: Arc<AtomicBool>,
}
impl SessionTicket {
    pub(crate) fn same_lifetime(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.alive, &other.alive)
    }
    pub fn new(peer_id: String, auth_epoch: u64, session_epoch: u64) -> Self {
        Self {
            peer_id,
            auth_epoch,
            session_epoch,
            alive: Arc::new(AtomicBool::new(true)),
            commit: Arc::new(RwLock::new(())),
            fenced: Arc::new(AtomicBool::new(false)),
        }
    }
    pub fn is_active(&self) -> bool {
        self.alive.load(Ordering::Acquire)
    }
    /// Rejects new effects immediately. Repository retirement completes the
    /// write fence before unregistering the session or acknowledging revoke.
    pub fn retire(&self) {
        self.alive.store(false, Ordering::Release);
    }
    pub(crate) fn commit_guard(&self) -> SyncResult<SessionCommitGuard<'_>> {
        let guard = self
            .commit
            .read()
            .unwrap_or_else(|error| error.into_inner());
        if !self.is_active() {
            return Err(super::super::SyncError::new(
                super::super::SyncErrorKind::Cancelled,
                "Sync session retired before commit",
            ));
        }
        Ok(SessionCommitGuard { _guard: guard })
    }
    pub(crate) fn retirement_complete(&self) -> bool {
        !self.is_active() && self.fenced.load(Ordering::Acquire)
    }
    pub(crate) fn complete_retirement(&self) {
        self.retire();
        let _fence = self
            .commit
            .write()
            .unwrap_or_else(|error| error.into_inner());
        self.fenced.store(true, Ordering::Release);
    }
}
pub(crate) struct SessionCommitGuard<'a> {
    _guard: RwLockReadGuard<'a, ()>,
}
#[derive(Clone, Debug)]
pub struct SyncSnapshot {
    pub lists: ListData,
    pub dislike: String,
    pub repository_revision: u64,
    pub sequence: u64,
}
#[derive(Clone, Debug, PartialEq)]
pub enum SyncData {
    List(ListData),
    Dislike(String),
}
impl SyncData {
    pub fn dataset(&self) -> Dataset {
        match self {
            Self::List(_) => Dataset::List,
            Self::Dislike(_) => Dataset::Dislike,
        }
    }
    pub fn digest(&self) -> SyncResult<String> {
        match self {
            Self::List(data) => data.digest(),
            Self::Dislike(rules) => dislike_digest(rules),
        }
    }
    pub fn wire(&self) -> Value {
        match self {
            Self::List(data) => data.wire(),
            Self::Dislike(rules) => Value::String(rules.clone()),
        }
    }
}
impl SyncSnapshot {
    pub fn data(&self, dataset: Dataset) -> SyncData {
        match dataset {
            Dataset::List => SyncData::List(self.lists.clone()),
            Dataset::Dislike => SyncData::Dislike(self.dislike.clone()),
        }
    }
}
#[derive(Clone, Debug)]
pub struct SyncBaseline {
    pub data: SyncData,
    pub repository_revision: u64,
    pub sequence: u64,
    pub digest: String,
}
#[derive(Clone, Debug)]
pub enum SyncOperation {
    Replace(SyncData),
    List(ListAction),
    Dislike(DislikeAction),
}
#[derive(Clone, Debug)]
pub struct SyncReceipt {
    pub repository_revision: u64,
    pub sequence: u64,
    pub digest: String,
}
#[derive(Clone, Debug)]
pub struct SyncDelta {
    pub sequence: u64,
    pub dataset: Dataset,
    pub action: Value,
}
#[derive(Clone, Debug)]
/// One coherent repository read captures all eligible records through snapshot.S.
/// If they do not fit limit, return NeedsSnapshot instead of mixing a partial
/// batch with a newer snapshot; confirming S must never cover unsent S+1.
pub enum SyncChanges {
    Delta {
        records: Vec<SyncDelta>,
        snapshot: SyncSnapshot,
    },
    NeedsSnapshot,
}

/// Every write, including pairing/baseline ACK, validates the live ticket and
/// approved dataset scope at commit. Network receipt is never a data commit.
pub trait SyncBackend: Send + Sync + 'static {
    fn peers(&self) -> SyncResult<Vec<PeerRecord>>;
    fn store_peer(&self, pairing_epoch: u64, peer: PeerRecord) -> SyncResult<()>;
    fn begin_session(&self, ticket: &SessionTicket) -> SyncResult<()>;
    fn retire_session(&self, ticket: &SessionTicket);
    fn snapshot(&self, ticket: &SessionTicket) -> SyncResult<SyncSnapshot>;
    fn baseline(
        &self,
        ticket: &SessionTicket,
        dataset: Dataset,
    ) -> SyncResult<Option<SyncBaseline>>;
    fn apply_remote(
        &self,
        ticket: &SessionTicket,
        expected_sequence: Option<u64>,
        operation: SyncOperation,
    ) -> SyncResult<SyncReceipt>;
    fn confirm_baseline(
        &self,
        ticket: &SessionTicket,
        data: SyncData,
        repository_revision: u64,
        sequence: u64,
        digest: &str,
    ) -> SyncResult<()>;
    fn changes_after(
        &self,
        ticket: &SessionTicket,
        sequence: u64,
        limit: usize,
    ) -> SyncResult<SyncChanges>;
    fn select_mode(&self, ticket: &SessionTicket, dataset: Dataset)
    -> SyncResult<Option<SyncMode>>;
    fn revoke_peer(&self, peer_id: &str) -> SyncResult<()>;
}

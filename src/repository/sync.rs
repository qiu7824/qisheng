//! Transactional sync datasets and private peer records. Metadata never grants I/O.
use super::{
    ChangeKind, Repository, RepositoryChange, commit_change, decode_document, encode_document,
    lists, schema,
};
use crate::domain::{
    lists as native,
    music::{LxMusicDtoVersion, TrackRecord},
};
use crate::sync::network::{
    PeerRecord, SessionTicket, SyncBaseline, SyncChanges, SyncData, SyncDelta, SyncOperation,
    SyncReceipt, SyncSnapshot,
};
use crate::sync::{
    Dataset, DislikeAction, ListAction, ListData, MusicInfo, PeerCapabilities, SharedKey, UserList,
};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, VecDeque};

pub(super) const SCHEMA_12: &str = r#"
CREATE TABLE sync_profile_state (
 singleton INTEGER PRIMARY KEY CHECK(singleton=1),
 profile_epoch INTEGER NOT NULL CHECK(profile_epoch>0),
 pairing_epoch INTEGER NOT NULL CHECK(pairing_epoch>0),
 pairing_enabled INTEGER NOT NULL DEFAULT 0 CHECK(pairing_enabled IN (0,1)),
 sequence INTEGER NOT NULL DEFAULT 0 CHECK(sequence>=0),
 journal_floor INTEGER NOT NULL DEFAULT 0 CHECK(journal_floor>=0)
);
INSERT INTO sync_profile_state VALUES(1,1,1,0,0,0);
CREATE TABLE sync_peers (
 peer_id TEXT PRIMARY KEY CHECK(length(peer_id) BETWEEN 1 AND 256),
 client_id TEXT NOT NULL CHECK(length(client_id) BETWEEN 1 AND 256),
 auth_epoch INTEGER NOT NULL CHECK(auth_epoch>0),
 revoked INTEGER NOT NULL CHECK(revoked IN (0,1)),
 private_key TEXT CHECK(private_key IS NULL OR length(private_key)=24),
 document TEXT NOT NULL CHECK(json_valid(document)),
 byte_size INTEGER NOT NULL CHECK(byte_size BETWEEN 1 AND 16384 AND byte_size=length(CAST(document AS BLOB))),
 CHECK(revoked=1 OR private_key IS NOT NULL)
);
CREATE TABLE sync_snapshots (
 dataset TEXT NOT NULL CHECK(dataset IN ('list','dislike')),
 digest TEXT NOT NULL CHECK(length(digest)=32),
 sha256 TEXT NOT NULL CHECK(length(sha256)=64),
 document TEXT NOT NULL CHECK(json_valid(document)),
 byte_size INTEGER NOT NULL CHECK(byte_size BETWEEN 1 AND 8388608 AND byte_size=length(CAST(document AS BLOB))),
 PRIMARY KEY(dataset,digest,sha256)
);
CREATE TABLE sync_current (
 dataset TEXT PRIMARY KEY CHECK(dataset IN ('list','dislike')),
 digest TEXT,
 sha256 TEXT,
 available INTEGER NOT NULL DEFAULT 1 CHECK(available IN (0,1)),
 source_revision INTEGER NOT NULL CHECK(source_revision>=0),
 sequence INTEGER NOT NULL CHECK(sequence>=0),
 CHECK((digest IS NULL AND sha256 IS NULL) OR (digest IS NOT NULL AND sha256 IS NOT NULL AND length(digest)=32 AND length(sha256)=64)),
 FOREIGN KEY(dataset,digest,sha256) REFERENCES sync_snapshots(dataset,digest,sha256)
);
CREATE TABLE sync_baselines (
 peer_id TEXT NOT NULL REFERENCES sync_peers(peer_id) ON DELETE CASCADE,
 dataset TEXT NOT NULL CHECK(dataset IN ('list','dislike')),
 common_digest TEXT NOT NULL,
 common_sha256 TEXT NOT NULL CHECK(length(common_sha256)=64),
 captured_repository_revision INTEGER NOT NULL CHECK(captured_repository_revision>=0),
 covered_sequence INTEGER NOT NULL CHECK(covered_sequence>=0),
 PRIMARY KEY(peer_id,dataset),
 FOREIGN KEY(dataset,common_digest,common_sha256) REFERENCES sync_snapshots(dataset,digest,sha256)
);
CREATE TABLE sync_journal (
 sequence INTEGER PRIMARY KEY CHECK(sequence>0),
 dataset TEXT NOT NULL CHECK(dataset IN ('list','dislike')),
 repository_revision INTEGER NOT NULL CHECK(repository_revision>0),
 origin_peer TEXT,
 action_document TEXT CHECK(action_document IS NULL OR json_valid(action_document)),
 byte_size INTEGER NOT NULL CHECK(byte_size BETWEEN 0 AND 8388608 AND byte_size=CASE WHEN action_document IS NULL THEN 0 ELSE length(CAST(action_document AS BLOB)) END)
);
CREATE TABLE sync_list_metadata (
 list_id TEXT PRIMARY KEY REFERENCES music_lists(id) ON DELETE CASCADE,
 document TEXT NOT NULL CHECK(json_valid(document)),
 byte_size INTEGER NOT NULL CHECK(byte_size BETWEEN 1 AND 16384 AND byte_size=length(CAST(document AS BLOB)))
);
CREATE TABLE sync_capture_override (
 dataset TEXT PRIMARY KEY CHECK(dataset IN ('list','dislike')),
 document TEXT NOT NULL CHECK(json_valid(document)),
 byte_size INTEGER NOT NULL CHECK(byte_size BETWEEN 1 AND 8388608 AND byte_size=length(CAST(document AS BLOB)))
);
CREATE INDEX sync_journal_dataset_sequence ON sync_journal(dataset,sequence);
"#;
pub(super) const SCHEMA_13: &str = r#"
CREATE TABLE sync_mode_preferences (
 peer_id TEXT NOT NULL REFERENCES sync_peers(peer_id) ON DELETE CASCADE,
 dataset TEXT NOT NULL CHECK(dataset IN ('list','dislike')),
 auth_epoch INTEGER NOT NULL CHECK(auth_epoch>0),
 document TEXT NOT NULL CHECK(json_valid(document) AND length(CAST(document AS BLOB))<=128),
 PRIMARY KEY(peer_id,dataset)
);
"#;
const MAX_PEERS: usize = 8;
const MAX_PEER_BYTES: i64 = MAX_PEERS as i64 * 16384;
const MAX_JOURNAL_ROWS: i64 = 1000;
const MAX_JOURNAL_BYTES: i64 = 64 * 1024 * 1024;
// 8 peers × 2 datasets × 8 MiB, plus two current snapshots and GC headroom.
const MAX_SNAPSHOT_BYTES: i64 = 160 * 1024 * 1024;
const MAX_DELTA_BYTES: usize = 1024 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize)]
struct PeerPublic {
    peer_id: String,
    client_id: String,
    device_name: String,
    is_mobile: bool,
    auth_epoch: u64,
    list: Option<bool>,
    dislike: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    role: Option<StoredPeerRole>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum StoredPeerRole {
    Server,
    Client,
}
impl From<&PeerRecord> for PeerPublic {
    fn from(peer: &PeerRecord) -> Self {
        Self {
            peer_id: peer.peer_id.clone(),
            client_id: peer.client_id.clone(),
            device_name: peer.device_name.clone(),
            is_mobile: peer.is_mobile,
            auth_epoch: peer.auth_epoch,
            list: peer.capabilities.list,
            dislike: peer.capabilities.dislike,
            role: None,
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SyncProfileState {
    pub profile_epoch: u64,
    pub pairing_epoch: u64,
    pub pairing_enabled: bool,
    pub sequence: u64,
    pub journal_floor: u64,
}
#[derive(Clone)]
struct IssuedCut {
    dataset: Dataset,
    digest: String,
    sha256: String,
    revision: u64,
    sequence: u64,
}
struct RegisteredSession {
    ticket: SessionTicket,
    profile_epoch: u64,
    cuts: VecDeque<IssuedCut>,
}
#[derive(Default)]
pub(crate) struct SyncSessionRegistry {
    sessions: HashMap<String, RegisteredSession>,
}
impl std::fmt::Debug for SyncSessionRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SyncSessionRegistry")
            .field("sessions", &self.sessions.len())
            .finish()
    }
}

fn dataset_name(dataset: Dataset) -> &'static str {
    if dataset == Dataset::List {
        "list"
    } else {
        "dislike"
    }
}
fn epoch(value: u64) -> Result<i64> {
    ensure!(value > 0 && value <= i64::MAX as u64, "Invalid sync epoch");
    Ok(value as i64)
}
fn sequence(value: u64) -> Result<i64> {
    ensure!(value <= i64::MAX as u64, "Invalid sync sequence");
    Ok(value as i64)
}
fn state(connection: &Connection) -> Result<SyncProfileState> {
    Ok(connection.query_row("SELECT profile_epoch,pairing_epoch,pairing_enabled,sequence,journal_floor FROM sync_profile_state WHERE singleton=1",[],|row|Ok(SyncProfileState{profile_epoch:row.get::<_,i64>(0)? as u64,pairing_epoch:row.get::<_,i64>(1)? as u64,pairing_enabled:row.get(2)?,sequence:row.get::<_,i64>(3)? as u64,journal_floor:row.get::<_,i64>(4)? as u64}))?)
}
fn validate_peer(peer: &PeerRecord) -> Result<()> {
    for id in [&peer.peer_id, &peer.client_id] {
        ensure!(
            !id.is_empty() && id.len() <= 256 && !id.contains('\0'),
            "Invalid sync peer identity"
        );
    }
    ensure!(
        peer.device_name.len() <= 256 && !peer.device_name.contains('\0'),
        "Invalid sync peer name"
    );
    epoch(peer.auth_epoch)?;
    ensure!(
        peer.capabilities.list.is_some() || peer.capabilities.dislike.is_some(),
        "Sync peer has no approved datasets"
    );
    Ok(())
}
fn read_peer(connection: &Connection, id: &str) -> Result<Option<PeerRecord>> {
    let row: Option<(i64, bool, Option<String>, String)> = connection
        .query_row(
            "SELECT auth_epoch,revoked,private_key,document FROM sync_peers WHERE peer_id=?1",
            [id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .optional()?;
    let Some((auth_epoch, revoked, key, document)) = row else {
        return Ok(None);
    };
    if revoked {
        return Ok(None);
    }
    let public: PeerPublic = decode_document(&document)?;
    ensure!(
        public.peer_id == id && public.auth_epoch == auth_epoch as u64,
        "Sync peer identity mismatch"
    );
    let key = SharedKey::from_base64(
        key.as_deref()
            .context("Active sync peer private key missing")?,
    )?;
    let peer = PeerRecord {
        peer_id: public.peer_id,
        client_id: public.client_id,
        key,
        device_name: public.device_name,
        is_mobile: public.is_mobile,
        auth_epoch: public.auth_epoch,
        capabilities: PeerCapabilities {
            list: public.list,
            dislike: public.dislike,
        },
    };
    validate_peer(&peer)?;
    Ok(Some(peer))
}
fn check_ticket(
    connection: &Connection,
    registry: &SyncSessionRegistry,
    ticket: &SessionTicket,
    dataset: Option<Dataset>,
) -> Result<()> {
    ensure!(ticket.is_active(), "Sync session is retired");
    let registered = registry
        .sessions
        .get(&ticket.peer_id)
        .context("Sync session is not registered")?;
    ensure!(
        registered.ticket.same_lifetime(ticket)
            && registered.ticket.session_epoch == ticket.session_epoch
            && registered.ticket.auth_epoch == ticket.auth_epoch,
        "Sync session registry identity mismatch"
    );
    let profile = state(connection)?;
    ensure!(
        profile.profile_epoch == registered.profile_epoch,
        "Sync profile epoch changed"
    );
    let peer = read_peer(connection, &ticket.peer_id)?.context("Sync peer is revoked")?;
    ensure!(
        peer.auth_epoch == ticket.auth_epoch,
        "Sync authentication epoch changed"
    );
    if let Some(dataset) = dataset {
        ensure!(
            match dataset {
                Dataset::List => peer.capabilities.list.is_some(),
                Dataset::Dislike => peer.capabilities.dislike.is_some(),
            },
            "Sync dataset scope denied"
        );
    }
    Ok(())
}

impl Repository {
    pub fn sync_check_session(
        &self,
        ticket: &SessionTicket,
        dataset: Option<Dataset>,
    ) -> Result<()> {
        let registry = self
            .sync_registry
            .lock()
            .map_err(|_| anyhow::anyhow!("Sync registry poisoned"))?;
        let connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("Repository lock poisoned"))?;
        check_ticket(&connection, &registry, ticket, dataset)
    }
    pub fn sync_mode(
        &self,
        ticket: &SessionTicket,
        dataset: Dataset,
    ) -> Result<Option<crate::sync::SyncMode>> {
        let registry = self
            .sync_registry
            .lock()
            .map_err(|_| anyhow::anyhow!("Sync registry poisoned"))?;
        let connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("Repository lock poisoned"))?;
        check_ticket(&connection, &registry, ticket, Some(dataset))?;
        let document:Option<String>=connection.query_row("SELECT document FROM sync_mode_preferences WHERE peer_id=?1 AND dataset=?2 AND auth_epoch=?3",params![ticket.peer_id,dataset_name(dataset),epoch(ticket.auth_epoch)?],|row|row.get(0)).optional()?;
        let mode = document
            .map(|document| decode_document::<crate::sync::SyncMode>(&document))
            .transpose()?;
        if let Some(mode) = mode {
            validate_mode(dataset, mode)?;
        }
        Ok(mode)
    }
    pub fn sync_choose_mode_with_commit_guard<G>(
        &self,
        ticket: &SessionTicket,
        dataset: Dataset,
        mode: crate::sync::SyncMode,
        check: &dyn Fn() -> Result<()>,
        acquire: impl FnOnce() -> Result<G>,
    ) -> Result<()> {
        validate_mode(dataset, mode)?;
        check()?;
        let registry = self
            .sync_registry
            .lock()
            .map_err(|_| anyhow::anyhow!("Sync registry poisoned"))?;
        let mut connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("Repository lock poisoned"))?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        check_ticket(&tx, &registry, ticket, Some(dataset))?;
        tx.execute("INSERT INTO sync_mode_preferences(peer_id,dataset,auth_epoch,document) VALUES(?1,?2,?3,?4) ON CONFLICT(peer_id,dataset) DO UPDATE SET auth_epoch=excluded.auth_epoch,document=excluded.document",params![ticket.peer_id,dataset_name(dataset),epoch(ticket.auth_epoch)?,encode_document(&mode)?])?;
        let _commit = acquire()?;
        let _ticket_commit = ticket.commit_guard()?;
        check()?;
        check_ticket(&tx, &registry, ticket, Some(dataset))?;
        tx.commit()?;
        Ok(())
    }
    pub fn sync_profile_state(&self) -> Result<SyncProfileState> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("Repository lock was poisoned"))?;
        schema::validate_current(&connection)?;
        state(&connection)
    }
    pub fn sync_configure_pairing(
        &self,
        profile_epoch: u64,
        pairing_epoch: u64,
        enabled: bool,
    ) -> Result<()> {
        self.sync_configure_pairing_with_commit_guard(
            profile_epoch,
            pairing_epoch,
            enabled,
            &|| Ok(()),
            || Ok(()),
        )
    }
    pub fn sync_configure_pairing_with_commit_guard<G>(
        &self,
        profile_epoch: u64,
        pairing_epoch: u64,
        enabled: bool,
        check: &dyn Fn() -> Result<()>,
        acquire: impl FnOnce() -> Result<G>,
    ) -> Result<()> {
        check()?;
        let _registry = self
            .sync_registry
            .lock()
            .map_err(|_| anyhow::anyhow!("Sync registry poisoned"))?;
        let mut connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("Repository lock was poisoned"))?;
        schema::validate_current(&connection)?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        ensure!(
            state(&tx)?.profile_epoch == profile_epoch,
            "Sync profile epoch conflict"
        );
        tx.execute(
            "UPDATE sync_profile_state SET pairing_epoch=?1,pairing_enabled=?2 WHERE singleton=1",
            params![epoch(pairing_epoch)?, enabled],
        )?;
        let _commit = acquire()?;
        check()?;
        tx.commit()?;
        Ok(())
    }
    pub fn sync_peers(&self) -> Result<Vec<PeerRecord>> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("Repository lock was poisoned"))?;
        schema::validate_current(&connection)?;
        let ids = {
            let mut statement = connection.prepare(
                "SELECT peer_id FROM sync_peers WHERE revoked=0 ORDER BY peer_id LIMIT 9",
            )?;
            statement
                .query_map([], |row| row.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        ensure!(ids.len() <= MAX_PEERS, "Sync peer limit exceeded");
        ids.iter()
            .map(|id| read_peer(&connection, id)?.context("Active sync peer missing"))
            .collect()
    }
    pub fn sync_client_credentials(
        &self,
        server_id: &str,
    ) -> Result<crate::sync::network::ClientCredentials> {
        ensure!(
            !server_id.is_empty() && server_id.len() <= 256 && !server_id.contains('\0'),
            "Invalid saved server identity"
        );
        let connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("Repository lock poisoned"))?;
        schema::validate_current(&connection)?;
        let peer = read_peer(&connection, server_id)?
            .context("Saved server pairing is unavailable or revoked")?;
        let document: String = connection.query_row(
            "SELECT document FROM sync_peers WHERE peer_id=?1",
            [server_id],
            |row| row.get(0),
        )?;
        let public: PeerPublic = decode_document(&document)?;
        ensure!(
            public.role == Some(StoredPeerRole::Client)
                && public.peer_id == server_id
                && public.client_id == peer.client_id
                && public.auth_epoch == peer.auth_epoch,
            "Saved pairing is not a confirmed client-side server record"
        );
        Ok(crate::sync::network::ClientCredentials {
            server_id: peer.peer_id,
            client_id: peer.client_id,
            key: peer.key,
            server_name: peer.device_name,
            auth_epoch: peer.auth_epoch,
        })
    }
    pub fn sync_store_peer(&self, pairing_epoch: u64, peer: &PeerRecord) -> Result<()> {
        self.sync_store_peer_with_commit_guard(pairing_epoch, peer, &|| Ok(()), || Ok(()))
    }
    pub fn sync_store_peer_with_commit_guard<G>(
        &self,
        pairing_epoch: u64,
        peer: &PeerRecord,
        check: &dyn Fn() -> Result<()>,
        acquire: impl FnOnce() -> Result<G>,
    ) -> Result<()> {
        self.sync_store_peer_role_with_commit_guard(pairing_epoch, peer, None, check, acquire)
    }
    pub fn sync_store_client_peer_with_commit_guard<G>(
        &self,
        pairing_epoch: u64,
        peer: &PeerRecord,
        check: &dyn Fn() -> Result<()>,
        acquire: impl FnOnce() -> Result<G>,
    ) -> Result<()> {
        self.sync_store_peer_role_with_commit_guard(
            pairing_epoch,
            peer,
            Some(StoredPeerRole::Client),
            check,
            acquire,
        )
    }
    pub fn sync_store_server_peer_with_commit_guard<G>(
        &self,
        pairing_epoch: u64,
        peer: &PeerRecord,
        check: &dyn Fn() -> Result<()>,
        acquire: impl FnOnce() -> Result<G>,
    ) -> Result<()> {
        self.sync_store_peer_role_with_commit_guard(
            pairing_epoch,
            peer,
            Some(StoredPeerRole::Server),
            check,
            acquire,
        )
    }
    fn sync_store_peer_role_with_commit_guard<G>(
        &self,
        pairing_epoch: u64,
        peer: &PeerRecord,
        role: Option<StoredPeerRole>,
        check: &dyn Fn() -> Result<()>,
        acquire: impl FnOnce() -> Result<G>,
    ) -> Result<()> {
        check()?;
        validate_peer(peer)?;
        let mut registry = self
            .sync_registry
            .lock()
            .map_err(|_| anyhow::anyhow!("Sync registry poisoned"))?;
        let mut connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("Repository lock was poisoned"))?;
        schema::validate_current(&connection)?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let profile = state(&tx)?;
        ensure!(
            profile.pairing_enabled && profile.pairing_epoch == pairing_epoch,
            "Sync pairing is disabled or expired"
        );
        let exists: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM sync_peers WHERE peer_id=?1)",
            [&peer.peer_id],
            |row| row.get(0),
        )?;
        let count: i64 = tx.query_row("SELECT count(*) FROM sync_peers", [], |row| row.get(0))?;
        ensure!(
            exists || count < MAX_PEERS as i64,
            "Sync peer limit exceeded"
        );
        let prior_epoch: Option<i64> = tx
            .query_row(
                "SELECT auth_epoch FROM sync_peers WHERE peer_id=?1",
                [&peer.peer_id],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(current) = prior_epoch {
            ensure!(
                peer.auth_epoch > current as u64,
                "Authentication rotation must advance epoch"
            );
        }
        let mut public = PeerPublic::from(peer);
        public.role = role;
        let public = encode_document(&public)?;
        ensure!(public.len() <= 16384, "Sync peer document oversized");
        tx.execute("INSERT INTO sync_peers(peer_id,client_id,auth_epoch,revoked,private_key,document,byte_size) VALUES(?1,?2,?3,0,?4,?5,?6) ON CONFLICT(peer_id) DO UPDATE SET client_id=excluded.client_id,auth_epoch=excluded.auth_epoch,revoked=0,private_key=excluded.private_key,document=excluded.document,byte_size=excluded.byte_size",params![peer.peer_id,peer.client_id,epoch(peer.auth_epoch)?,peer.key.expose_base64(),public,public.len() as i64])?;
        let bytes: i64 = tx.query_row(
            "SELECT coalesce(sum(byte_size),0) FROM sync_peers",
            [],
            |row| row.get(0),
        )?;
        ensure!(
            bytes <= MAX_PEER_BYTES,
            "Sync peer history byte limit exceeded"
        );
        let _commit = acquire()?;
        check()?;
        tx.commit()?;
        if let Some(old) = registry.sessions.remove(&peer.peer_id) {
            old.ticket.retire();
        }
        Ok(())
    }
    pub fn sync_begin_session(&self, ticket: &SessionTicket) -> Result<()> {
        self.sync_begin_session_with_commit_guard(ticket, &|| Ok(()), || Ok(()))
    }
    pub fn sync_begin_session_with_commit_guard<G>(
        &self,
        ticket: &SessionTicket,
        check: &dyn Fn() -> Result<()>,
        acquire: impl FnOnce() -> Result<G>,
    ) -> Result<()> {
        check()?;
        epoch(ticket.session_epoch)?;
        ensure!(ticket.is_active(), "Sync session is retired");
        let mut registry = self
            .sync_registry
            .lock()
            .map_err(|_| anyhow::anyhow!("Sync registry poisoned"))?;
        let connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("Repository lock was poisoned"))?;
        schema::validate_current(&connection)?;
        let peer = read_peer(&connection, &ticket.peer_id)?.context("Sync peer is revoked")?;
        ensure!(
            peer.auth_epoch == ticket.auth_epoch,
            "Sync auth epoch mismatch"
        );
        ensure!(
            registry.sessions.contains_key(&ticket.peer_id) || registry.sessions.len() < MAX_PEERS,
            "Sync session limit exceeded"
        );
        let profile_epoch = state(&connection)?.profile_epoch;
        if registry
            .sessions
            .get(&ticket.peer_id)
            .is_some_and(|old| old.ticket.same_lifetime(ticket))
        {
            return check_ticket(&connection, &registry, ticket, None);
        }
        let _commit = acquire()?;
        check()?;
        if let Some(old) = registry.sessions.insert(
            ticket.peer_id.clone(),
            RegisteredSession {
                ticket: ticket.clone(),
                profile_epoch,
                cuts: VecDeque::new(),
            },
        ) {
            old.ticket.retire();
        }
        Ok(())
    }
    pub fn sync_retire_session(&self, ticket: &SessionTicket) -> Result<()> {
        ticket.complete_retirement();
        ensure!(
            ticket.retirement_complete(),
            "Sync retirement fence incomplete"
        );
        let mut registry = self
            .sync_registry
            .lock()
            .map_err(|_| anyhow::anyhow!("Sync registry poisoned"))?;
        if registry
            .sessions
            .get(&ticket.peer_id)
            .is_some_and(|registered| registered.ticket.same_lifetime(ticket))
        {
            registry.sessions.remove(&ticket.peer_id);
        }
        Ok(())
    }
    pub fn sync_revoke_peer(&self, id: &str) -> Result<()> {
        self.sync_revoke_peer_with_commit_guard(id, &|| Ok(()), || Ok(()))
    }
    pub fn sync_revoke_peer_with_commit_guard<G>(
        &self,
        id: &str,
        check: &dyn Fn() -> Result<()>,
        acquire: impl FnOnce() -> Result<G>,
    ) -> Result<()> {
        check()?;
        let mut registry = self
            .sync_registry
            .lock()
            .map_err(|_| anyhow::anyhow!("Sync registry poisoned"))?;
        let mut connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("Repository lock was poisoned"))?;
        schema::validate_current(&connection)?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current: Option<(i64, bool)> = tx
            .query_row(
                "SELECT auth_epoch,revoked FROM sync_peers WHERE peer_id=?1",
                [id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        if let Some((auth, false)) = current {
            ensure!(auth < i64::MAX, "Sync authentication epoch exhausted");
            tx.execute("UPDATE sync_peers SET revoked=1,private_key=NULL,auth_epoch=auth_epoch+1 WHERE peer_id=?1",[id])?;
        }
        tx.execute("DELETE FROM sync_baselines WHERE peer_id=?1", [id])?;
        let _commit = acquire()?;
        check()?;
        tx.commit()?;
        if let Some(old) = registry.sessions.remove(id) {
            old.ticket.retire();
        }
        Ok(())
    }
    /// Removes an explicitly revoked pairing record. Active peers cannot be forgotten.
    pub fn sync_forget_peer(&self, id: &str, expected_auth_epoch: u64) -> Result<()> {
        let mut registry = self
            .sync_registry
            .lock()
            .map_err(|_| anyhow::anyhow!("Sync registry poisoned"))?;
        let mut connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("Repository lock poisoned"))?;
        schema::validate_current(&connection)?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let removed = tx.execute("DELETE FROM sync_peers WHERE peer_id=?1 AND revoked=1 AND private_key IS NULL AND auth_epoch=?2",params![id,epoch(expected_auth_epoch)?])?;
        ensure!(removed == 1, "Revoked peer epoch conflict");
        gc_snapshots(&tx)?;
        tx.commit()?;
        if let Some(old) = registry.sessions.remove(id) {
            old.ticket.retire();
        }
        Ok(())
    }
    pub fn sync_rotate_profile(&self, expected_epoch: u64) -> Result<u64> {
        let mut registry = self
            .sync_registry
            .lock()
            .map_err(|_| anyhow::anyhow!("Sync registry poisoned"))?;
        let mut connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("Repository lock was poisoned"))?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let profile = state(&tx)?;
        ensure!(
            profile.profile_epoch == expected_epoch,
            "Sync profile epoch conflict"
        );
        let next = profile
            .profile_epoch
            .checked_add(1)
            .context("Sync profile epoch exhausted")?;
        tx.execute(
            "UPDATE sync_profile_state SET profile_epoch=?1,pairing_enabled=0 WHERE singleton=1",
            [epoch(next)?],
        )?;
        tx.commit()?;
        for old in registry.sessions.drain().map(|(_, old)| old) {
            old.ticket.retire();
        }
        Ok(next)
    }
}

fn validate_mode(dataset: Dataset, mode: crate::sync::SyncMode) -> Result<()> {
    ensure!(
        dataset == Dataset::List
            || !matches!(
                mode,
                crate::sync::SyncMode::OverwriteLocalRemoteFull
                    | crate::sync::SyncMode::OverwriteRemoteLocalFull
            ),
        "Dislike does not support full-list overwrite modes"
    );
    Ok(())
}

fn value_text(value: &Value) -> Option<String> {
    match value {
        Value::String(value) => Some(value.clone()),
        Value::Number(value) => value
            .as_f64()
            .map(|value| ryu_js::Buffer::new().format_finite(value).to_owned()),
        _ => None,
    }
}
fn wire_user_info(connection: &Connection, info: &native::ListInfo) -> Result<Map<String, Value>> {
    let archived: Option<String> = connection
        .query_row(
            "SELECT document FROM sync_list_metadata WHERE list_id=?1",
            [&info.id],
            |row| row.get(0),
        )
        .optional()?;
    let backup: Option<String> = if archived.is_none() {
        connection
            .query_row(
                "SELECT document FROM backup_list_metadata WHERE list_id=?1",
                [&info.id],
                |row| row.get(0),
            )
            .optional()?
    } else {
        None
    };
    let prior = archived
        .as_ref()
        .map(|document| serde_json::from_str::<Value>(document))
        .transpose()?
        .or(backup
            .as_ref()
            .map(|document| serde_json::from_str::<Value>(document))
            .transpose()?
            .and_then(|document| document.get("wire").cloned()));
    let mut object = Map::new();
    object.insert("id".into(), Value::String(info.id.clone()));
    object.insert("name".into(), Value::String(info.name.clone()));
    for (key, value) in [
        ("source", info.source.as_ref()),
        ("sourceListId", info.source_list_id.as_ref()),
    ] {
        let previous = prior.as_ref().and_then(|prior| prior.get(key));
        match value {
            Some(value) => {
                let wire = previous
                    .filter(|previous| value_text(previous).as_ref() == Some(value))
                    .cloned()
                    .unwrap_or_else(|| Value::String(value.clone()));
                object.insert(key.into(), wire);
            }
            None => {
                if previous == Some(&Value::Null) {
                    object.insert(key.into(), Value::Null);
                }
            }
        }
    }
    match info.location_update_time {
        Some(time) => {
            object.insert("locationUpdateTime".into(), Value::from(time));
        }
        None => {
            // Native timestamps are u64; an exact LX nonnegative fractional
            // Number remains metadata until a native reorder supplies a time.
            if let Some(value) = prior
                .as_ref()
                .and_then(|prior| prior.get("locationUpdateTime"))
                && (value.is_null() || value.is_number())
            {
                object.insert("locationUpdateTime".into(), value.clone());
            }
        }
    }
    Ok(object)
}
fn project_lists(connection: &Connection) -> Result<ListData> {
    let tracks = |id: &str| -> Result<Vec<MusicInfo>> {
        lists::query_tracks(connection, id)?
            .into_iter()
            .map(|track| {
                MusicInfo::from_wire(track.to_lx_dto(LxMusicDtoVersion::Modern))
                    .map_err(anyhow::Error::from)
            })
            .collect()
    };
    let default_list = tracks(native::DEFAULT_LIST)?;
    let love_list = tracks(native::LOVE_LIST)?;
    let mut user_list = Vec::new();
    for info in lists::query_infos(connection, true)?
        .into_iter()
        .filter(|info| info.id != "download")
    {
        let mut object = wire_user_info(connection, &info)?;
        object.insert(
            "list".into(),
            Value::Array(
                tracks(&info.id)?
                    .iter()
                    .map(|track| track.wire().clone())
                    .collect(),
            ),
        );
        user_list.push(UserList::from_wire(&Value::Object(object))?);
    }
    let data = ListData {
        default_list,
        love_list,
        user_list,
    };
    data.validate()?;
    Ok(data)
}
fn project_data(connection: &Connection, dataset: Dataset) -> Result<SyncData> {
    match dataset {
        Dataset::List => Ok(SyncData::List(project_lists(connection)?)),
        Dataset::Dislike => {
            let rules: String = connection.query_row(
                "SELECT rules FROM dislike_state WHERE singleton=1",
                [],
                |row| row.get(0),
            )?;
            crate::sync::dislike_digest(&rules)?;
            Ok(SyncData::Dislike(rules))
        }
    }
}
fn parse_data(dataset: Dataset, document: &str) -> Result<SyncData> {
    ensure!(
        document.len() <= crate::sync::MAX_JSON_BYTES,
        "Sync snapshot byte limit exceeded"
    );
    let value: Value = serde_json::from_str(document)?;
    match dataset {
        Dataset::List => Ok(SyncData::List(ListData::from_wire(&value)?)),
        Dataset::Dislike => {
            let rules = value
                .as_str()
                .context("Invalid dislike snapshot")?
                .to_string();
            crate::sync::dislike_digest(&rules)?;
            Ok(SyncData::Dislike(rules))
        }
    }
}
fn serialize_data(data: &SyncData) -> Result<(String, String, String)> {
    let document = crate::sync::canonical_json(&data.wire())?;
    if document.len() > crate::sync::MAX_JSON_BYTES {
        return Err(crate::sync::SyncError::new(
            crate::sync::SyncErrorKind::Quota,
            "Sync snapshot exceeds byte limit",
        )
        .into());
    }
    let digest = data.digest()?;
    let sha = format!("{:x}", Sha256::digest(document.as_bytes()));
    Ok((document, digest, sha))
}
fn save_snapshot(tx: &Transaction<'_>, data: &SyncData) -> Result<(String, String)> {
    let (document, digest, sha) = serialize_data(data)?;
    let dataset = dataset_name(data.dataset());
    let previous: Option<String> = tx
        .query_row(
            "SELECT document FROM sync_snapshots WHERE dataset=?1 AND digest=?2 AND sha256=?3",
            params![dataset, digest, sha],
            |row| row.get(0),
        )
        .optional()?;
    if let Some(existing_document) = previous {
        ensure!(
            existing_document == document,
            "Sync digest collision or snapshot corruption"
        );
    } else {
        tx.execute("INSERT INTO sync_snapshots(dataset,digest,sha256,document,byte_size) VALUES(?1,?2,?3,?4,?5)",params![dataset,digest,sha,document,document.len() as i64])?;
    }
    Ok((digest, sha))
}
fn load_snapshot(
    connection: &Connection,
    dataset: Dataset,
    digest: &str,
    sha: &str,
) -> Result<SyncData> {
    let document: String = connection.query_row(
        "SELECT document FROM sync_snapshots WHERE dataset=?1 AND digest=?2 AND sha256=?3",
        params![dataset_name(dataset), digest, sha],
        |row| row.get(0),
    )?;
    ensure!(
        format!("{:x}", Sha256::digest(document.as_bytes())) == sha,
        "Sync snapshot checksum mismatch"
    );
    let data = parse_data(dataset, &document)?;
    ensure!(
        data.digest()? == digest,
        "Sync snapshot wire digest mismatch"
    );
    Ok(data)
}
fn ensure_current(tx: &Transaction<'_>, peer: &PeerRecord) -> Result<()> {
    let profile = state(tx)?;
    let revision: i64 = tx.query_row(
        "SELECT revision FROM repository_state WHERE singleton=1",
        [],
        |row| row.get(0),
    )?;
    for dataset in [Dataset::List, Dataset::Dislike] {
        if match dataset {
            Dataset::List => peer.capabilities.list.is_none(),
            Dataset::Dislike => peer.capabilities.dislike.is_none(),
        } {
            continue;
        }
        let exists: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM sync_current WHERE dataset=?1)",
            [dataset_name(dataset)],
            |row| row.get(0),
        )?;
        if !exists {
            let data = project_data(tx, dataset)?;
            let (digest, sha) = save_snapshot(tx, &data)?;
            tx.execute("INSERT INTO sync_current(dataset,digest,sha256,available,source_revision,sequence) VALUES(?1,?2,?3,1,?4,?5)",params![dataset_name(dataset),digest,sha,revision,sequence(profile.sequence)?])?;
        }
    }
    Ok(())
}
fn snapshot_from(tx: &Transaction<'_>, peer: &PeerRecord) -> Result<SyncSnapshot> {
    ensure_current(tx, peer)?;
    let profile = state(tx)?;
    let revision: i64 = tx.query_row(
        "SELECT revision FROM repository_state WHERE singleton=1",
        [],
        |row| row.get(0),
    )?;
    let read = |dataset: Dataset| -> Result<SyncData> {
        let (digest, sha, available): (Option<String>, Option<String>, bool) = tx.query_row(
            "SELECT digest,sha256,available FROM sync_current WHERE dataset=?1",
            [dataset_name(dataset)],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        ensure!(available, "Sync dataset is not currently representable");
        load_snapshot(
            tx,
            dataset,
            digest.as_deref().context("Sync current digest missing")?,
            sha.as_deref().context("Sync current SHA missing")?,
        )
    };
    let lists = if peer.capabilities.list.is_some() {
        let SyncData::List(data) = read(Dataset::List)? else {
            unreachable!()
        };
        data
    } else {
        ListData::default()
    };
    let dislike = if peer.capabilities.dislike.is_some() {
        let SyncData::Dislike(data) = read(Dataset::Dislike)? else {
            unreachable!()
        };
        data
    } else {
        String::new()
    };
    Ok(SyncSnapshot {
        lists,
        dislike,
        repository_revision: revision as u64,
        sequence: profile.sequence,
    })
}
fn issue_cut(
    registry: &mut SyncSessionRegistry,
    ticket: &SessionTicket,
    snapshot: &SyncSnapshot,
    peer: &PeerRecord,
) -> Result<()> {
    let registered = registry
        .sessions
        .get_mut(&ticket.peer_id)
        .context("Sync session missing")?;
    for dataset in [Dataset::List, Dataset::Dislike] {
        if match dataset {
            Dataset::List => peer.capabilities.list.is_none(),
            Dataset::Dislike => peer.capabilities.dislike.is_none(),
        } {
            continue;
        }
        let data = snapshot.data(dataset);
        let (_, digest, sha256) = serialize_data(&data)?;
        registered.cuts.push_back(IssuedCut {
            dataset,
            digest,
            sha256,
            revision: snapshot.repository_revision,
            sequence: snapshot.sequence,
        });
    }
    while registered.cuts.len() > 32 {
        registered.cuts.pop_front();
    }
    Ok(())
}
fn gc_snapshots(tx: &Transaction<'_>) -> Result<()> {
    tx.execute("DELETE FROM sync_snapshots WHERE NOT EXISTS(SELECT 1 FROM sync_current c WHERE c.dataset=sync_snapshots.dataset AND c.digest=sync_snapshots.digest AND c.sha256=sync_snapshots.sha256) AND NOT EXISTS(SELECT 1 FROM sync_baselines b WHERE b.dataset=sync_snapshots.dataset AND b.common_digest=sync_snapshots.digest AND b.common_sha256=sync_snapshots.sha256)",[])?;
    let bytes: i64 = tx.query_row(
        "SELECT coalesce(sum(byte_size),0) FROM sync_snapshots",
        [],
        |row| row.get(0),
    )?;
    ensure!(
        bytes <= MAX_SNAPSHOT_BYTES,
        "Sync common snapshot quota exceeded"
    );
    Ok(())
}
fn trim_journal(tx: &Transaction<'_>) -> Result<()> {
    loop {
        let (rows, bytes): (i64, i64) = tx.query_row(
            "SELECT count(*),coalesce(sum(byte_size),0) FROM sync_journal",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        if rows <= MAX_JOURNAL_ROWS && bytes <= MAX_JOURNAL_BYTES {
            break;
        }
        let first: i64 = tx.query_row("SELECT min(sequence) FROM sync_journal", [], |row| {
            row.get(0)
        })?;
        tx.execute("DELETE FROM sync_journal WHERE sequence=?1", [first])?;
        tx.execute(
            "UPDATE sync_profile_state SET journal_floor=max(journal_floor,?1) WHERE singleton=1",
            [first],
        )?;
    }
    Ok(())
}

/// Only existing public membership can turn a local library metadata update
/// into a sync dataset change. The IDs are SQL selectors, never file grants.
fn public_library_metadata_change(
    tx: &Transaction<'_>,
    kind: &ChangeKind,
    details: &Value,
) -> Result<bool> {
    let (ids, asset_ids) = match kind {
        ChangeKind::LocalAssetsUpserted => (
            details
                .get("track_ids")
                .context("Local batch track selectors missing")?
                .clone(),
            false,
        ),
        ChangeKind::LibraryScanBatchCommitted => (
            details
                .get("changed_asset_ids")
                .context("Scan asset selectors missing")?
                .clone(),
            true,
        ),
        ChangeKind::LocalAssetRelinked | ChangeKind::LocalTagsEdited => (
            serde_json::json!([details
                .get("asset_id")
                .and_then(Value::as_str)
                .context("Relink asset selector missing")?]),
            true,
        ),
        ChangeKind::LibraryRootRelinked => (
            details
                .get("relocated_asset_ids")
                .context("Root relocation selectors missing")?
                .clone(),
            true,
        ),
        _ => return Ok(false),
    };
    let selectors = ids.as_array().context("Invalid metadata selectors")?;
    ensure!(
        selectors.iter().all(Value::is_string),
        "Invalid metadata selector identity"
    );
    if selectors.is_empty() {
        return Ok(false);
    }
    let encoded = serde_json::to_string(&ids)?;
    let sql = if asset_ids {
        "SELECT EXISTS(SELECT 1 FROM json_each(?1) ids JOIN local_assets a ON a.identity_key=ids.value JOIN music_list_items m ON m.track_id=a.track_id WHERE ids.type='text' AND m.list_id NOT IN ('temp','download'))"
    } else {
        "SELECT EXISTS(SELECT 1 FROM json_each(?1) ids JOIN music_list_items m ON m.track_id=ids.value WHERE ids.type='text' AND m.list_id NOT IN ('temp','download'))"
    };
    Ok(tx.query_row(sql, [encoded], |row| row.get(0))?)
}

/// Runs inside the caller's existing data transaction, never touches registry.
pub(super) fn record_change(
    tx: &Transaction<'_>,
    kind: &ChangeKind,
    details: &Value,
    revision: u64,
) -> Result<()> {
    let dataset = match kind {
        ChangeKind::ListsChanged => Dataset::List,
        ChangeKind::DislikeChanged => Dataset::Dislike,
        _ if public_library_metadata_change(tx, kind, details)? => Dataset::List,
        _ => return Ok(()),
    };
    if dataset == Dataset::List && private_list_change(details) {
        return Ok(());
    }
    let previous: Option<(Option<String>, Option<String>)> = tx
        .query_row(
            "SELECT digest,sha256 FROM sync_current WHERE dataset=?1",
            [dataset_name(dataset)],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let override_document: Option<String> = tx
        .query_row(
            "SELECT document FROM sync_capture_override WHERE dataset=?1",
            [dataset_name(dataset)],
            |row| row.get(0),
        )
        .optional()?;
    let data = if let Some(document) = override_document {
        parse_data(dataset, &document)
    } else {
        project_data(tx, dataset)
    };
    let encoded = match data {
        Ok(data) => match serialize_data(&data) {
            Ok(_) => Some(save_snapshot(tx, &data)?),
            Err(error) if representation_failure(&error) => None,
            Err(error) => return Err(error),
        },
        Err(error) if representation_failure(&error) => None,
        Err(error) => return Err(error),
    };
    if encoded
        .as_ref()
        .map(|(digest, sha)| (Some(digest.as_str()), Some(sha.as_str())))
        == previous
            .as_ref()
            .map(|(digest, sha)| (digest.as_deref(), sha.as_deref()))
        && encoded.is_some()
    {
        tx.execute(
            "DELETE FROM sync_capture_override WHERE dataset=?1",
            [dataset_name(dataset)],
        )?;
        return Ok(());
    }
    let current = state(tx)?.sequence;
    let next = current.checked_add(1).context("Sync sequence exhausted")?;
    let origin = details
        .get("sync_origin_peer")
        .and_then(Value::as_str)
        .map(str::to_string);
    let dislike_raw: Option<String> = if dataset == Dataset::Dislike {
        Some(tx.query_row(
            "SELECT rules FROM dislike_state WHERE singleton=1",
            [],
            |row| row.get(0),
        )?)
    } else {
        None
    };
    let action = wire_change(kind, details, dislike_raw.as_deref())
        .ok()
        .flatten()
        .and_then(|value| crate::sync::canonical_json(&value).ok())
        .filter(|document| document.len() <= MAX_DELTA_BYTES);
    let bytes = action.as_ref().map(String::len).unwrap_or(0);
    tx.execute(
        "UPDATE sync_profile_state SET sequence=?1 WHERE singleton=1",
        [sequence(next)?],
    )?;
    tx.execute("INSERT INTO sync_journal(sequence,dataset,repository_revision,origin_peer,action_document,byte_size) VALUES(?1,?2,?3,?4,?5,?6)",params![sequence(next)?,dataset_name(dataset),sequence(revision)?,origin,action,bytes as i64])?;
    let digest = encoded.as_ref().map(|(digest, _)| digest.as_str());
    let sha = encoded.as_ref().map(|(_, sha)| sha.as_str());
    tx.execute("INSERT INTO sync_current(dataset,digest,sha256,available,source_revision,sequence) VALUES(?1,?2,?3,?4,?5,?6) ON CONFLICT(dataset) DO UPDATE SET digest=excluded.digest,sha256=excluded.sha256,available=excluded.available,source_revision=excluded.source_revision,sequence=excluded.sequence",params![dataset_name(dataset),digest,sha,encoded.is_some(),sequence(revision)?,sequence(next)?])?;
    tx.execute(
        "DELETE FROM sync_capture_override WHERE dataset=?1",
        [dataset_name(dataset)],
    )?;
    trim_journal(tx)?;
    gc_snapshots(tx)?;
    Ok(())
}
fn representation_failure(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<crate::sync::SyncError>()
        .is_some_and(|error| {
            matches!(
                error.kind,
                crate::sync::SyncErrorKind::InvalidData
                    | crate::sync::SyncErrorKind::Quota
                    | crate::sync::SyncErrorKind::Unsupported
            )
        })
}

fn private_id(id: &str) -> bool {
    matches!(id, "temp" | "download")
}
fn private_list_change(details: &Value) -> bool {
    let Some(value) = details.get("action") else {
        return false;
    };
    let Ok(action) = serde_json::from_value::<native::ListAction>(value.clone()) else {
        return false;
    };
    match action {
        native::ListAction::MusicOverwrite { list_id, .. }
        | native::ListAction::MusicAdd { list_id, .. }
        | native::ListAction::MusicRemove { list_id, .. }
        | native::ListAction::MusicUpdatePosition { list_id, .. } => private_id(&list_id),
        native::ListAction::MusicMove { from_id, to_id, .. } => {
            private_id(&from_id) && private_id(&to_id)
        }
        native::ListAction::MusicUpdate { updates } => {
            updates.iter().all(|update| private_id(&update.list_id))
        }
        native::ListAction::MusicClear { list_ids } => list_ids.iter().all(|id| private_id(id)),
        native::ListAction::Create { lists, .. } | native::ListAction::Update { lists } => {
            lists.iter().all(|info| private_id(&info.id))
        }
        native::ListAction::Remove { ids } | native::ListAction::UpdatePosition { ids, .. } => {
            ids.iter().all(|id| private_id(id))
        }
        native::ListAction::DataOverwrite { .. } => false,
    }
}
fn native_info_wire(info: &native::ListInfo) -> Value {
    let mut map = Map::new();
    map.insert("id".into(), Value::String(info.id.clone()));
    map.insert("name".into(), Value::String(info.name.clone()));
    if let Some(value) = &info.source {
        map.insert("source".into(), Value::String(value.clone()));
    }
    if let Some(value) = &info.source_list_id {
        map.insert("sourceListId".into(), Value::String(value.clone()));
    }
    if let Some(value) = info.location_update_time {
        map.insert("locationUpdateTime".into(), Value::from(value));
    }
    Value::Object(map)
}
fn native_music_wire(tracks: &[TrackRecord]) -> Result<Vec<Value>> {
    tracks
        .iter()
        .map(|track| {
            Ok(
                MusicInfo::from_wire(track.to_lx_dto(LxMusicDtoVersion::Modern))?
                    .wire()
                    .clone(),
            )
        })
        .collect()
}
fn envelope(action: &str, data: Value) -> Value {
    serde_json::json!({"action":action,"data":data})
}
fn location_wire(location: native::AddLocation) -> &'static str {
    match location {
        native::AddLocation::Top => "top",
        native::AddLocation::Bottom => "bottom",
    }
}
/// Native identity selectors are not necessarily LX wire IDs. A full snapshot
/// represents these removals/reorders without guessing a peer's identity.
fn wire_selectors(ids: &[String]) -> bool {
    !ids.iter()
        .any(|id| id.starts_with("track:") || id.starts_with("local-"))
}
fn wire_change(
    kind: &ChangeKind,
    details: &Value,
    dislike_raw: Option<&str>,
) -> Result<Option<Value>> {
    if let Some(value) = details.get("sync_wire_action") {
        if *kind == ChangeKind::ListsChanged {
            ListAction::from_wire(value)?;
        } else {
            DislikeAction::from_wire(value)?;
        }
        return Ok(Some(value.clone()));
    }
    let Some(value) = details.get("action") else {
        return Ok(None);
    };
    if *kind == ChangeKind::DislikeChanged {
        let action: native::DislikeAction = serde_json::from_value(value.clone())?;
        let wire = match action {
            native::DislikeAction::Clear => serde_json::json!({"action":"dislike_music_clear"}),
            // Native overwrite trims empty lines; emit its actual committed text.
            native::DislikeAction::Overwrite { .. } => envelope(
                "dislike_data_overwrite",
                Value::String(
                    dislike_raw
                        .context("Committed raw dislike rules missing")?
                        .to_owned(),
                ),
            ),
            native::DislikeAction::Add { infos } => {
                envelope("dislike_music_add", serde_json::to_value(infos)?)
            }
        };
        DislikeAction::from_wire(&wire)?;
        return Ok(Some(wire));
    }
    let action: native::ListAction = serde_json::from_value(value.clone())?;
    let wire = match action {
        native::ListAction::DataOverwrite { .. } => return Ok(None),
        native::ListAction::Create { position, lists } => envelope("list_create", serde_json::json!({"position":position,"listInfos":lists.iter().map(native_info_wire).collect::<Vec<_>>()})),
        native::ListAction::Remove { ids } => envelope("list_remove", serde_json::to_value(ids)?),
        native::ListAction::Update { lists } => envelope("list_update", Value::Array(lists.iter().map(native_info_wire).collect())),
        native::ListAction::UpdatePosition { ids, position } => envelope("list_update_position", serde_json::json!({"ids":ids,"position":position})),
        native::ListAction::MusicOverwrite { list_id, tracks } => envelope("list_music_overwrite", serde_json::json!({"listId":list_id,"musicInfos":native_music_wire(&tracks)?})),
        native::ListAction::MusicAdd { list_id, tracks, location } => envelope("list_music_add", serde_json::json!({"id":list_id,"musicInfos":native_music_wire(&tracks)?,"addMusicLocationType":location_wire(location)})),
        native::ListAction::MusicMove { from_id, to_id, tracks, location } => envelope("list_music_move", serde_json::json!({"fromId":from_id,"toId":to_id,"musicInfos":native_music_wire(&tracks)?,"addMusicLocationType":location_wire(location)})),
        native::ListAction::MusicRemove { list_id, ids } => {
            if !wire_selectors(&ids) { return Ok(None); }
            envelope("list_music_remove", serde_json::json!({"listId":list_id,"ids":ids}))
        }
        native::ListAction::MusicUpdate { updates } => envelope("list_music_update", Value::Array(updates.iter().map(|update| Ok(serde_json::json!({"id":update.list_id,"musicInfo":native_music_wire(std::slice::from_ref(&update.track))?.remove(0)}))).collect::<Result<Vec<_>>>()?)),
        native::ListAction::MusicClear { list_ids } => envelope("list_music_clear", serde_json::to_value(list_ids)?),
        native::ListAction::MusicUpdatePosition { list_id, ids, position } => {
            if !wire_selectors(&ids) { return Ok(None); }
            envelope("list_music_update_position", serde_json::json!({"listId":list_id,"ids":ids,"position":position}))
        }
    };
    ListAction::from_wire(&wire)?;
    Ok(Some(wire))
}

fn list_action_wire(action: &ListAction) -> Value {
    use crate::sync::AddLocation;
    let music =
        |items: &[MusicInfo]| Value::Array(items.iter().map(|item| item.wire().clone()).collect());
    let location = |value: &AddLocation| {
        if *value == AddLocation::Top {
            "top"
        } else {
            "bottom"
        }
    };
    match action {
        ListAction::DataOverwrite(data) => envelope("list_data_overwrite", data.wire()),
        ListAction::Create { position, lists } => envelope(
            "list_create",
            serde_json::json!({"position":position,"listInfos":lists}),
        ),
        ListAction::Remove(ids) => envelope("list_remove", serde_json::json!(ids)),
        ListAction::Update(lists) => envelope("list_update", serde_json::json!(lists)),
        ListAction::UpdatePosition { position, ids } => envelope(
            "list_update_position",
            serde_json::json!({"position":position,"ids":ids}),
        ),
        ListAction::MusicAdd {
            id,
            music: items,
            location: value,
        } => envelope(
            "list_music_add",
            serde_json::json!({"id":id,"musicInfos":music(items),"addMusicLocationType":location(value)}),
        ),
        ListAction::MusicMove {
            from_id,
            to_id,
            music: items,
            location: value,
        } => envelope(
            "list_music_move",
            serde_json::json!({"fromId":from_id,"toId":to_id,"musicInfos":music(items),"addMusicLocationType":location(value)}),
        ),
        ListAction::MusicRemove { list_id, ids } => envelope(
            "list_music_remove",
            serde_json::json!({"listId":list_id,"ids":ids}),
        ),
        ListAction::MusicUpdate(updates) => envelope(
            "list_music_update",
            Value::Array(
                updates
                    .iter()
                    .map(|(id, music)| serde_json::json!({"id":id,"musicInfo":music.wire()}))
                    .collect(),
            ),
        ),
        ListAction::MusicUpdatePosition {
            list_id,
            position,
            ids,
        } => envelope(
            "list_music_update_position",
            serde_json::json!({"listId":list_id,"position":position,"ids":ids}),
        ),
        ListAction::MusicOverwrite {
            list_id,
            music: items,
        } => envelope(
            "list_music_overwrite",
            serde_json::json!({"listId":list_id,"musicInfos":music(items)}),
        ),
        ListAction::MusicClear(ids) => envelope("list_music_clear", serde_json::json!(ids)),
    }
}
fn dislike_action_wire(action: &DislikeAction) -> Value {
    match action {
        DislikeAction::DataOverwrite(rules) => {
            envelope("dislike_data_overwrite", Value::String(rules.clone()))
        }
        DislikeAction::MusicAdd(infos) => envelope(
            "dislike_music_add",
            Value::Array(
                infos
                    .iter()
                    .map(|(name, singer)| serde_json::json!({"name":name,"singer":singer}))
                    .collect(),
            ),
        ),
        DislikeAction::MusicClear => serde_json::json!({"action":"dislike_music_clear"}),
    }
}
fn native_track(music: &MusicInfo) -> Result<TrackRecord> {
    let mut track = TrackRecord::from_lx_dto(music.wire().clone(), LxMusicDtoVersion::Modern)?;
    // A lexical local path is metadata. Only the existing SQL binding resolver
    // may attach an already approved native asset; no new asset is created.
    track.local_asset_id = None;
    Ok(track)
}
fn native_tracks(music: &[MusicInfo]) -> Result<Vec<TrackRecord>> {
    music.iter().map(native_track).collect()
}
fn native_info(value: &Value) -> Result<native::ListInfo> {
    let mut object = value
        .as_object()
        .context("List info must be object")?
        .clone();
    object.insert("list".into(), Value::Array(Vec::new()));
    let info = UserList::from_wire(&Value::Object(object))?;
    Ok(native::ListInfo {
        id: info.id,
        name: info
            .name
            .as_str()
            .context("List name must be string")?
            .to_owned(),
        source: info
            .source
            .as_ref()
            .and_then(Value::as_str)
            .map(str::to_owned),
        source_list_id: info.source_list_id.as_ref().and_then(value_text),
        location_update_time: info.location_update_time.as_ref().and_then(Value::as_u64),
        meta: None,
    })
}
fn preserve_info(tx: &Transaction<'_>, value: &Value) -> Result<()> {
    let id = value
        .get("id")
        .and_then(Value::as_str)
        .context("List id missing")?;
    let mut object = value
        .as_object()
        .context("List info must be object")?
        .clone();
    object.remove("list");
    let document = crate::sync::canonical_json(&Value::Object(object))?;
    ensure!(document.len() <= 16384, "Sync list metadata exceeds limit");
    tx.execute("INSERT INTO sync_list_metadata(list_id,document,byte_size) VALUES(?1,?2,?3) ON CONFLICT(list_id) DO UPDATE SET document=excluded.document,byte_size=excluded.byte_size",params![id,document,document.len() as i64])?;
    Ok(())
}
fn native_replace(tx: &Transaction<'_>, data: &ListData) -> Result<native::ListAction> {
    data.validate()?;
    let mut users: Vec<native::UserList> = data
        .user_list
        .iter()
        .map(|list| {
            Ok(native::UserList {
                info: native_info(&list.wire())?,
                tracks: native_tracks(&list.list)?,
            })
        })
        .collect::<Result<_>>()?;
    if let Some(info) = lists::query_infos(tx, true)?
        .into_iter()
        .find(|info| info.id == "download")
    {
        users.push(native::UserList {
            tracks: lists::query_tracks(tx, "download")?,
            info,
        });
    }
    Ok(native::ListAction::DataOverwrite {
        data: native::ListData {
            default_list: native_tracks(&data.default_list)?,
            love_list: native_tracks(&data.love_list)?,
            user_list: users,
            temp_list: None,
        },
    })
}
fn native_list_action(tx: &Transaction<'_>, action: &ListAction) -> Result<native::ListAction> {
    use crate::sync::AddLocation;
    let location = |value: &AddLocation| {
        if *value == AddLocation::Top {
            native::AddLocation::Top
        } else {
            native::AddLocation::Bottom
        }
    };
    Ok(match action {
        ListAction::DataOverwrite(data) => return native_replace(tx, data),
        ListAction::Create { position, lists } => native::ListAction::Create {
            position: *position,
            lists: lists.iter().map(native_info).collect::<Result<_>>()?,
        },
        ListAction::Remove(ids) => native::ListAction::Remove { ids: ids.clone() },
        ListAction::Update(lists) => native::ListAction::Update {
            lists: lists.iter().map(native_info).collect::<Result<_>>()?,
        },
        ListAction::UpdatePosition { position, ids } => native::ListAction::UpdatePosition {
            position: *position,
            ids: ids.clone(),
        },
        ListAction::MusicAdd {
            id,
            music,
            location: value,
        } => native::ListAction::MusicAdd {
            list_id: id.clone(),
            tracks: native_tracks(music)?,
            location: location(value),
        },
        ListAction::MusicMove {
            from_id,
            to_id,
            music,
            location: value,
        } => native::ListAction::MusicMove {
            from_id: from_id.clone(),
            to_id: to_id.clone(),
            tracks: native_tracks(music)?,
            location: location(value),
        },
        ListAction::MusicRemove { list_id, ids } => native::ListAction::MusicRemove {
            list_id: list_id.clone(),
            ids: ids.clone(),
        },
        ListAction::MusicUpdate(updates) => native::ListAction::MusicUpdate {
            updates: updates
                .iter()
                .map(|(id, music)| {
                    Ok(native::ListMusicUpdate {
                        list_id: id.clone(),
                        track: native_track(music)?,
                    })
                })
                .collect::<Result<_>>()?,
        },
        ListAction::MusicUpdatePosition {
            list_id,
            position,
            ids,
        } => native::ListAction::MusicUpdatePosition {
            list_id: list_id.clone(),
            position: *position,
            ids: ids.clone(),
        },
        ListAction::MusicOverwrite { list_id, music } => native::ListAction::MusicOverwrite {
            list_id: list_id.clone(),
            tracks: native_tracks(music)?,
        },
        ListAction::MusicClear(ids) => native::ListAction::MusicClear {
            list_ids: ids.clone(),
        },
    })
}
fn capture_override(tx: &Transaction<'_>, data: &SyncData) -> Result<()> {
    let (document, _, _) = serialize_data(data)?;
    tx.execute("INSERT INTO sync_capture_override(dataset,document,byte_size) VALUES(?1,?2,?3) ON CONFLICT(dataset) DO UPDATE SET document=excluded.document,byte_size=excluded.byte_size",params![dataset_name(data.dataset()),document,document.len() as i64])?;
    Ok(())
}

impl Repository {
    /// Applies one peer operation in the same transaction as the native data,
    /// repository revision, bounded journal and coherent dataset snapshot.
    pub fn sync_apply_remote(
        &self,
        ticket: &SessionTicket,
        expected_sequence: Option<u64>,
        operation: &SyncOperation,
    ) -> Result<(RepositoryChange, SyncReceipt)> {
        self.sync_apply_remote_checked(ticket, expected_sequence, operation, &|| Ok(()))
    }
    pub fn sync_apply_remote_checked(
        &self,
        ticket: &SessionTicket,
        expected_sequence: Option<u64>,
        operation: &SyncOperation,
        check: &dyn Fn() -> Result<()>,
    ) -> Result<(RepositoryChange, SyncReceipt)> {
        self.sync_apply_remote_with_commit_guard(
            ticket,
            expected_sequence,
            operation,
            check,
            || Ok(()),
        )
    }
    pub fn sync_apply_remote_with_commit_guard<G>(
        &self,
        ticket: &SessionTicket,
        expected_sequence: Option<u64>,
        operation: &SyncOperation,
        check: &dyn Fn() -> Result<()>,
        acquire: impl FnOnce() -> Result<G>,
    ) -> Result<(RepositoryChange, SyncReceipt)> {
        check()?;
        let (dataset, wire, replacement) = match operation {
            SyncOperation::Replace(data) => {
                serialize_data(data)?;
                (data.dataset(), None, true)
            }
            SyncOperation::List(action) => {
                let wire = list_action_wire(action);
                ListAction::from_wire(&wire)?;
                ensure!(
                    crate::sync::canonical_json(&wire)?.len() <= crate::sync::MAX_JSON_BYTES,
                    "Sync action byte limit exceeded"
                );
                (
                    Dataset::List,
                    Some(wire),
                    matches!(action, ListAction::DataOverwrite(_)),
                )
            }
            SyncOperation::Dislike(action) => {
                let wire = dislike_action_wire(action);
                DislikeAction::from_wire(&wire)?;
                (
                    Dataset::Dislike,
                    Some(wire),
                    matches!(action, DislikeAction::DataOverwrite(_)),
                )
            }
        };
        ensure!(
            !replacement || expected_sequence.is_some(),
            "Full sync replacement requires a coherent sequence CAS"
        );
        let mut registry = self
            .sync_registry
            .lock()
            .map_err(|_| anyhow::anyhow!("Sync registry poisoned"))?;
        let mut connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("Repository lock poisoned"))?;
        schema::validate_current(&connection)?;
        check()?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        check_ticket(&tx, &registry, ticket, Some(dataset))?;
        if let Some(expected) = expected_sequence {
            ensure!(
                state(&tx)?.sequence == expected,
                "Sync dataset sequence conflict"
            );
        }
        check()?;
        let list_action = match operation {
            SyncOperation::Replace(SyncData::List(data)) => {
                Some(ListAction::DataOverwrite(data.clone()))
            }
            SyncOperation::List(action) => Some(action.clone()),
            _ => None,
        };
        if let Some(action) = list_action {
            let native = native_list_action(&tx, &action)?;
            lists::apply(&tx, &native)?;
            match &action {
                ListAction::Create { lists, .. } | ListAction::Update(lists) => {
                    for info in lists {
                        preserve_info(&tx, info)?;
                    }
                }
                ListAction::DataOverwrite(data) => {
                    for info in &data.user_list {
                        preserve_info(&tx, &info.wire())?;
                    }
                    capture_override(&tx, &SyncData::List(data.clone()))?;
                }
                _ => {}
            }
        } else {
            let action = match operation {
                SyncOperation::Replace(SyncData::Dislike(rules)) => {
                    DislikeAction::DataOverwrite(rules.clone())
                }
                SyncOperation::Dislike(action) => action.clone(),
                _ => unreachable!(),
            };
            let rules = match action {
                DislikeAction::DataOverwrite(rules) => rules,
                DislikeAction::MusicClear => String::new(),
                DislikeAction::MusicAdd(infos) => {
                    let mut rules: String = tx.query_row(
                        "SELECT rules FROM dislike_state WHERE singleton=1",
                        [],
                        |row| row.get(0),
                    )?;
                    for (name, singer) in infos {
                        if !rules.is_empty() {
                            rules.push('\n');
                        }
                        rules.push_str(&name);
                        rules.push('@');
                        rules.push_str(&singer);
                    }
                    rules
                }
            };
            crate::sync::dislike_digest(&rules)?;
            tx.execute(
                "UPDATE dislike_state SET rules=?1 WHERE singleton=1",
                [&rules],
            )?;
            if replacement {
                capture_override(&tx, &SyncData::Dislike(rules))?;
            }
        }
        let current: i64 = tx.query_row(
            "SELECT revision FROM repository_state WHERE singleton=1",
            [],
            |row| row.get(0),
        )?;
        let mut details = serde_json::json!({"origin":"sync","sync_origin_peer":ticket.peer_id,"operation":if replacement {"snapshot"} else {"delta"},"dataset":dataset_name(dataset)});
        if !replacement
            && let Some(wire) = wire
            && crate::sync::canonical_json(&wire)?.len() <= MAX_DELTA_BYTES
        {
            details
                .as_object_mut()
                .unwrap()
                .insert("sync_wire_action".into(), wire);
        }
        let change = commit_change(
            &tx,
            current,
            if dataset == Dataset::List {
                ChangeKind::ListsChanged
            } else {
                ChangeKind::DislikeChanged
            },
            None,
            details,
        )?;
        check()?;
        check_ticket(&tx, &registry, ticket, Some(dataset))?;
        let (digest, sha, available): (Option<String>, Option<String>, bool) = tx.query_row(
            "SELECT digest,sha256,available FROM sync_current WHERE dataset=?1",
            [dataset_name(dataset)],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        ensure!(available, "Applied sync dataset is not representable");
        let digest = digest.context("Applied sync digest missing")?;
        let data = load_snapshot(
            &tx,
            dataset,
            &digest,
            sha.as_deref().context("Applied sync SHA missing")?,
        )?;
        let (_, _, sha256) = serialize_data(&data)?;
        let receipt = SyncReceipt {
            repository_revision: change.revision,
            sequence: state(&tx)?.sequence,
            digest: digest.clone(),
        };
        let cut = IssuedCut {
            dataset,
            digest,
            sha256,
            revision: receipt.repository_revision,
            sequence: receipt.sequence,
        };
        check()?;
        check_ticket(&tx, &registry, ticket, Some(dataset))?;
        let _commit = acquire()?;
        let _ticket_commit = ticket.commit_guard()?;
        check()?;
        check_ticket(&tx, &registry, ticket, Some(dataset))?;
        tx.commit()
            .context("Cannot commit remote sync transaction")?;
        let cuts = &mut registry.sessions.get_mut(&ticket.peer_id).unwrap().cuts;
        cuts.push_back(cut);
        while cuts.len() > 32 {
            cuts.pop_front();
        }
        Ok((change, receipt))
    }
    pub fn sync_snapshot(&self, ticket: &SessionTicket) -> Result<SyncSnapshot> {
        let mut registry = self
            .sync_registry
            .lock()
            .map_err(|_| anyhow::anyhow!("Sync registry poisoned"))?;
        let mut connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("Repository lock poisoned"))?;
        schema::validate_current(&connection)?;
        let tx = connection.transaction()?;
        check_ticket(&tx, &registry, ticket, None)?;
        let peer = read_peer(&tx, &ticket.peer_id)?.context("Peer missing")?;
        let snapshot = snapshot_from(&tx, &peer)?;
        check_ticket(&tx, &registry, ticket, None)?;
        tx.commit()?;
        issue_cut(&mut registry, ticket, &snapshot, &peer)?;
        Ok(snapshot)
    }
    pub fn sync_baseline(
        &self,
        ticket: &SessionTicket,
        dataset: Dataset,
    ) -> Result<Option<SyncBaseline>> {
        let registry = self
            .sync_registry
            .lock()
            .map_err(|_| anyhow::anyhow!("Sync registry poisoned"))?;
        let connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("Repository lock poisoned"))?;
        check_ticket(&connection, &registry, ticket, Some(dataset))?;
        let row:Option<(String,String,i64,i64)>=connection.query_row("SELECT common_digest,common_sha256,captured_repository_revision,covered_sequence FROM sync_baselines WHERE peer_id=?1 AND dataset=?2",params![ticket.peer_id,dataset_name(dataset)],|row|Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?))).optional()?;
        row.map(|(digest, sha, revision, sequence)| {
            Ok(SyncBaseline {
                data: load_snapshot(&connection, dataset, &digest, &sha)?,
                repository_revision: revision as u64,
                sequence: sequence as u64,
                digest,
            })
        })
        .transpose()
    }
    pub fn sync_confirm_baseline(
        &self,
        ticket: &SessionTicket,
        data: &SyncData,
        revision: u64,
        covered: u64,
        digest: &str,
    ) -> Result<()> {
        self.sync_confirm_baseline_with_commit_guard(
            ticket,
            data,
            revision,
            covered,
            digest,
            &|| Ok(()),
            || Ok(()),
        )
    }
    #[expect(
        clippy::too_many_arguments,
        reason = "Session ticket, coherent cut revision, covered sequence, digest and two commit guards prove independent ACK conditions"
    )]
    pub fn sync_confirm_baseline_with_commit_guard<G>(
        &self,
        ticket: &SessionTicket,
        data: &SyncData,
        revision: u64,
        covered: u64,
        digest: &str,
        check: &dyn Fn() -> Result<()>,
        acquire: impl FnOnce() -> Result<G>,
    ) -> Result<()> {
        check()?;
        let mut registry = self
            .sync_registry
            .lock()
            .map_err(|_| anyhow::anyhow!("Sync registry poisoned"))?;
        let mut connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("Repository lock poisoned"))?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let dataset = data.dataset();
        check_ticket(&tx, &registry, ticket, Some(dataset))?;
        let (_, actual, sha) = serialize_data(data)?;
        ensure!(actual == digest, "Sync ACK digest mismatch");
        ensure!(
            covered <= state(&tx)?.sequence,
            "Cannot acknowledge future sync state"
        );
        let registered = registry
            .sessions
            .get(&ticket.peer_id)
            .context("Sync session missing")?;
        ensure!(
            registered.cuts.iter().any(|cut| cut.dataset == dataset
                && cut.digest == digest
                && cut.sha256 == sha
                && cut.sequence == covered
                && cut.revision == revision),
            "Sync ACK has no exact issued coherent cut"
        );
        let previous: Option<i64> = tx
            .query_row(
                "SELECT covered_sequence FROM sync_baselines WHERE peer_id=?1 AND dataset=?2",
                params![ticket.peer_id, dataset_name(dataset)],
                |row| row.get(0),
            )
            .optional()?;
        ensure!(
            previous.is_none_or(|previous| covered >= previous as u64),
            "Sync ACK cannot move coverage backwards"
        );
        let (saved, saved_sha) = save_snapshot(&tx, data)?;
        tx.execute("INSERT INTO sync_baselines(peer_id,dataset,common_digest,common_sha256,captured_repository_revision,covered_sequence) VALUES(?1,?2,?3,?4,?5,?6) ON CONFLICT(peer_id,dataset) DO UPDATE SET common_digest=excluded.common_digest,common_sha256=excluded.common_sha256,captured_repository_revision=excluded.captured_repository_revision,covered_sequence=excluded.covered_sequence",params![ticket.peer_id,dataset_name(dataset),saved,saved_sha,sequence(revision)?,sequence(covered)?])?;
        gc_snapshots(&tx)?;
        check_ticket(&tx, &registry, ticket, Some(dataset))?;
        let _commit = acquire()?;
        let _ticket_commit = ticket.commit_guard()?;
        check()?;
        check_ticket(&tx, &registry, ticket, Some(dataset))?;
        tx.commit()?;
        registry
            .sessions
            .get_mut(&ticket.peer_id)
            .unwrap()
            .cuts
            .retain(|cut| cut.dataset != dataset || cut.sequence > covered);
        Ok(())
    }
    pub fn sync_changes_after(
        &self,
        ticket: &SessionTicket,
        after: u64,
        limit: usize,
    ) -> Result<SyncChanges> {
        ensure!(limit <= 1000, "Sync journal page too large");
        let mut registry = self
            .sync_registry
            .lock()
            .map_err(|_| anyhow::anyhow!("Sync registry poisoned"))?;
        let mut connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("Repository lock poisoned"))?;
        let tx = connection.transaction()?;
        check_ticket(&tx, &registry, ticket, None)?;
        let peer = read_peer(&tx, &ticket.peer_id)?.context("Peer missing")?;
        let profile = state(&tx)?;
        ensure!(after <= profile.sequence, "Sync cursor is in the future");
        if after < profile.journal_floor {
            return Ok(SyncChanges::NeedsSnapshot);
        }
        let snapshot = snapshot_from(&tx, &peer)?;
        let rows = {
            let mut statement=tx.prepare("SELECT sequence,dataset,action_document,origin_peer FROM sync_journal WHERE sequence>?1 AND sequence<=?2 AND (origin_peer IS NULL OR origin_peer!=?4) AND ((dataset='list' AND ?5) OR (dataset='dislike' AND ?6)) ORDER BY sequence LIMIT ?3")?;
            statement
                .query_map(
                    params![
                        sequence(after)?,
                        sequence(snapshot.sequence)?,
                        (limit + 1) as i64,
                        ticket.peer_id,
                        peer.capabilities.list.is_some(),
                        peer.capabilities.dislike.is_some(),
                    ],
                    |row| {
                        Ok((
                            row.get::<_, i64>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, Option<String>>(2)?,
                            row.get::<_, Option<String>>(3)?,
                        ))
                    },
                )?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        let mut records = Vec::new();
        for (seq, dataset, document, origin) in rows {
            let dataset = if dataset == "list" {
                Dataset::List
            } else {
                Dataset::Dislike
            };
            if origin.as_deref() == Some(ticket.peer_id.as_str())
                || match dataset {
                    Dataset::List => peer.capabilities.list.is_none(),
                    Dataset::Dislike => peer.capabilities.dislike.is_none(),
                }
            {
                continue;
            }
            let Some(document) = document else {
                return Ok(SyncChanges::NeedsSnapshot);
            };
            let action: Value = serde_json::from_str(&document)?;
            if dataset == Dataset::List {
                ListAction::from_wire(&action)?;
            } else {
                DislikeAction::from_wire(&action)?;
            }
            records.push(SyncDelta {
                sequence: seq as u64,
                dataset,
                action,
            });
        }
        if records.len() > limit {
            return Ok(SyncChanges::NeedsSnapshot);
        }
        check_ticket(&tx, &registry, ticket, None)?;
        tx.commit()?;
        issue_cut(&mut registry, ticket, &snapshot, &peer)?;
        Ok(SyncChanges::Delta { records, snapshot })
    }
}

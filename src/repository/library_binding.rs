use super::{
    ChangeKind, Repository, RepositoryChange, decode_document, encode_document, library::*, schema,
    tracks,
};
use crate::domain::{
    library::*,
    library_watch::*,
    music::{TrackRecord, normalize_local_path, stable_id},
};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

const WORK_BYTES: i64 = 8 * 1024 * 1024;
const VALID_CANDIDATES_FROM: &str = "FROM library_relink_candidates c JOIN library_roots r ON r.root_id=c.root_id AND r.active=1 JOIN library_watch_preferences w ON w.root_id=c.root_id AND w.enabled=1 JOIN local_asset_locations l ON l.asset_id=c.asset_id WHERE c.expires_at_ms>?1 AND (?2 IS NULL OR c.root_id=?2) AND json_extract(c.document,'$.root_id')=c.root_id AND json_extract(c.document,'$.asset_id')=c.asset_id AND json_extract(c.document,'$.grant_epoch')=r.grant_epoch AND json_extract(c.document,'$.watch_generation')=w.watch_generation AND json_extract(c.document,'$.binding_revision')=l.binding_revision AND json_extract(c.document,'$.old_path')=json_extract(l.document,'$.path')";
#[derive(Clone, Debug)]
pub struct LibraryRelinkRepositoryCommit {
    pub result: LibraryRelinkResult,
    pub change: RepositoryChange,
}
#[derive(Serialize, Deserialize)]
struct ExactFileGrant {
    path: PathBuf,
    file_identity: Option<FileIdentity>,
}
#[derive(Serialize, Deserialize)]
struct IdentityReservation {
    path: PathBuf,
    asset_id: String,
    track_id: String,
    lx_id: String,
    file_identity: Option<FileIdentity>,
}

pub(super) fn physical_identity_key(identity: &FileIdentity) -> Result<String> {
    identity.validate()?;
    Ok(format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(identity)?)
    ))
}
pub(super) fn initialize_bindings(transaction: &Transaction<'_>) -> Result<()> {
    let mut statement = transaction
        .prepare("SELECT asset_id,document FROM local_asset_locations ORDER BY asset_id")?;
    for row in statement.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })? {
        let (id, document) = row?;
        let asset = asset_at(transaction, &id)?.context("Migration local asset is missing")?;
        let mut location: AssetLocation = decode_document(&document)?;
        ensure!(
            normalize_local_path(&location.path) == normalize_local_path(&asset.path),
            LibraryError::invalid("Migration asset location disagrees with its asset")
        );
        location.size_bytes = Some(asset.size_bytes);
        location.modified_ms = asset.modified_ms;
        let key = location
            .file_identity
            .as_ref()
            .map(physical_identity_key)
            .transpose()?;
        transaction.execute("UPDATE local_asset_locations SET physical_identity_key=?2,document=?3 WHERE asset_id=?1",params![id,key,encode_document(&location)?])?;
    }
    let mut statement =
        transaction.prepare("SELECT asset_id FROM local_file_grants ORDER BY asset_id")?;
    for row in statement.query_map([], |row| row.get::<_, String>(0))? {
        let id = row?;
        let asset = asset_at(transaction, &id)?.context("Migration file grant asset is missing")?;
        write_exact_file_grant(transaction, &asset, false)?;
    }
    let mut statement = transaction.prepare("SELECT root_id,document FROM library_roots")?;
    for row in statement.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })? {
        let (id, document) = row?;
        let root: LibraryRoot = decode_document(&document)?;
        root.validate()?;
        let preference = LibraryWatchPreference {
            root_id: id.clone(),
            enabled: false,
            watch_generation: 0,
            options: LibraryWatchOptions::default(),
        };
        transaction.execute("INSERT INTO library_root_locations VALUES(?1,1)", [&id])?;
        transaction.execute(
            "INSERT INTO library_watch_preferences VALUES(?1,0,0,?2)",
            params![id, encode_document(&preference)?],
        )?;
        transaction.execute("INSERT INTO library_dirty_state VALUES(?1,0,0,0,0)", [&id])?;
    }
    Ok(())
}
pub(super) fn write_exact_file_grant(
    transaction: &Transaction<'_>,
    asset: &crate::domain::music::LocalAsset,
    renew: bool,
) -> Result<()> {
    let key = library_path_key(&asset.path, native_library_path_policy())?;
    let binding = binding_at(transaction, &asset.id)?;
    let document = encode_document(&ExactFileGrant {
        path: normalize_local_path(&asset.path),
        file_identity: binding.and_then(|value| value.file_identity),
    })?;
    let previous: Option<i64> = transaction
        .query_row(
            "SELECT epoch FROM local_file_grants WHERE asset_id=?1",
            [&asset.id],
            |row| row.get(0),
        )
        .optional()?;
    let epoch = match previous {
        Some(value) if renew => next_number(value as u64)?,
        Some(value) => value as u64,
        None => 1,
    };
    transaction.execute("INSERT INTO local_file_grants(asset_id,epoch,grant_path_key,document) VALUES(?1,?2,?3,?4) ON CONFLICT(asset_id) DO UPDATE SET epoch=excluded.epoch,grant_path_key=excluded.grant_path_key,document=excluded.document",params![asset.id,epoch as i64,key,document])?;
    Ok(())
}
pub(super) fn exact_file_epoch(connection: &Connection, asset_id: &str) -> Result<Option<u64>> {
    let row:Option<(i64,String,String)> = connection.query_row("SELECT f.epoch,f.document,l.document FROM local_file_grants f JOIN local_asset_locations l ON l.asset_id=f.asset_id AND l.path_key=f.grant_path_key WHERE f.asset_id=?1",[asset_id],|row|Ok((row.get(0)?,row.get(1)?,row.get(2)?))).optional()?;
    let Some((epoch, grant_doc, location_doc)) = row else {
        return Ok(None);
    };
    let grant: ExactFileGrant = decode_document(&grant_doc)?;
    let location: AssetLocation = decode_document(&location_doc)?;
    if normalize_local_path(&grant.path) != normalize_local_path(&location.path)
        && !grant
            .file_identity
            .as_ref()
            .zip(location.file_identity.as_ref())
            .is_some_and(|(a, b)| a == b)
    {
        return Ok(None);
    }
    if grant
        .file_identity
        .as_ref()
        .is_some_and(|identity| location.file_identity.as_ref() != Some(identity))
    {
        return Ok(None);
    }
    Ok(Some(epoch as u64))
}
pub(super) fn ensure_root_work_rows(
    transaction: &Transaction<'_>,
    root: &LibraryRoot,
) -> Result<()> {
    transaction.execute(
        "INSERT INTO library_root_locations VALUES(?1,1) ON CONFLICT(root_id) DO NOTHING",
        [&root.root_id],
    )?;
    let preference = LibraryWatchPreference {
        root_id: root.root_id.clone(),
        enabled: false,
        watch_generation: 0,
        options: LibraryWatchOptions::default(),
    };
    transaction.execute(
        "INSERT INTO library_watch_preferences VALUES(?1,0,0,?2) ON CONFLICT(root_id) DO NOTHING",
        params![root.root_id, encode_document(&preference)?],
    )?;
    transaction.execute(
        "INSERT INTO library_dirty_state VALUES(?1,0,0,0,0) ON CONFLICT(root_id) DO NOTHING",
        [&root.root_id],
    )?;
    Ok(())
}
pub(super) fn disable_root_watch(transaction: &Transaction<'_>, root_id: &str) -> Result<()> {
    if let Some(mut preference) = watch_at(transaction, root_id)? {
        preference.watch_generation = next_number(preference.watch_generation)?;
        preference.enabled = false;
        save_watch(transaction, &preference)?;
    }
    transaction.execute(
        "DELETE FROM library_relink_candidates WHERE root_id=?1",
        [root_id],
    )?;
    Ok(())
}

impl Repository {
    fn work_transaction<T>(
        &self,
        check: &dyn Fn() -> Result<()>,
        operation: impl FnOnce(&Transaction<'_>) -> Result<T>,
    ) -> Result<T> {
        check()?;
        let mut connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("Repository lock was poisoned"))?;
        schema::validate_current(&connection)?;
        check()?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let result = operation(&tx)?;
        check()?;
        tx.commit()?;
        Ok(result)
    }
    pub fn prepare_library_watch(
        &self,
        root_id: &str,
        enabled: bool,
        options: &LibraryWatchOptions,
        expected_generation: Option<u64>,
    ) -> Result<LibraryWatchToken> {
        validate_library_id(root_id)?;
        options.validate()?;
        let connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("Repository lock was poisoned"))?;
        schema::validate_current(&connection)?;
        let root = require_root(&connection, root_id)?;
        let preference =
            watch_at(&connection, root_id)?.context("Library watch preference missing")?;
        ensure!(
            expected_generation.is_none_or(|value| value == preference.watch_generation),
            LibraryError::conflict("Library watch generation conflict")
        );
        Ok(LibraryWatchToken {
            root_id: root_id.into(),
            grant_epoch: root.grant_epoch,
            watch_generation: next_number(preference.watch_generation)?,
            enabled,
            options: options.clone(),
        })
    }
    pub fn commit_library_watch(
        &self,
        token: &LibraryWatchToken,
        check: &dyn Fn() -> Result<()>,
    ) -> Result<(LibraryWatchPreference, RepositoryChange)> {
        token.validate()?;
        self.write_checked(None,|tx|{
            let mut root=require_root(tx,&token.root_id)?; let old=watch_at(tx,&token.root_id)?.context("Library watch preference missing")?;
            ensure!(root.grant_epoch==token.grant_epoch && next_number(old.watch_generation)?==token.watch_generation,LibraryError::conflict("Library watch preparation token expired"));
            let preference=LibraryWatchPreference{root_id:token.root_id.clone(),enabled:token.enabled,watch_generation:token.watch_generation,options:token.options.clone()};
            root.watch_enabled=token.enabled;
            tx.execute("UPDATE library_roots SET document=?2 WHERE root_id=?1",params![root.root_id,encode_document(&root)?])?;
            save_watch(tx,&preference)?;
            Ok((preference,ChangeKind::LibraryWatchChanged,None,serde_json::json!({"root_id":token.root_id,"enabled":token.enabled,"watch_generation":token.watch_generation})))
        },check)
    }
    pub fn library_watch(&self, root_id: &str) -> Result<Option<LibraryWatchPreference>> {
        validate_library_id(root_id)?;
        let connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("Repository lock was poisoned"))?;
        schema::validate_current(&connection)?;
        if root_at(&connection, root_id)?.is_none() {
            return Ok(None);
        };
        watch_at(&connection, root_id)
    }
    pub fn library_watches_page(&self, offset: usize, limit: usize) -> Result<LibraryWatchPage> {
        page_limits(offset, limit)?;
        let connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("Repository lock was poisoned"))?;
        schema::validate_current(&connection)?;
        let total: i64 = connection.query_row(
            "SELECT count(*) FROM library_roots WHERE active=1",
            [],
            |row| row.get(0),
        )?;
        let mut stmt = connection.prepare(
            "SELECT root_id FROM library_roots WHERE active=1 ORDER BY root_id LIMIT ?1 OFFSET ?2",
        )?;
        let mut items = Vec::new();
        let mut bytes = 4096;
        for row in stmt.query_map(params![limit as i64, offset as i64], |row| {
            row.get::<_, String>(0)
        })? {
            let item = watch_at(&connection, &row?)?.context("Watch preference missing")?;
            let size = serde_json::to_vec(&item)?.len() + 1;
            if bytes + size > MAX_LIBRARY_PAGE_BYTES {
                break;
            }
            bytes += size;
            items.push(item);
        }
        Ok(LibraryWatchPage {
            items,
            total: total as usize,
            offset,
            limit,
            revision: current_revision(&connection)?,
        })
    }
    pub fn mark_library_dirty(
        &self,
        root_id: &str,
        grant_epoch: u64,
        watch_generation: u64,
        received_sequence: u64,
        full_rescan_required: bool,
        check: &dyn Fn() -> Result<()>,
    ) -> Result<LibraryDirtyState> {
        ensure!(
            received_sequence <= i64::MAX as u64,
            LibraryError::invalid("Library event sequence overflow")
        );
        self.work_transaction(check, |tx| {
            require_watch(tx, root_id, grant_epoch, watch_generation)?;
            let mut dirty = dirty_at(tx, root_id)?.context("Library dirty state missing")?;
            ensure!(
                received_sequence >= dirty.received_sequence,
                LibraryError::conflict("Library event sequence is stale")
            );
            dirty.received_sequence = received_sequence;
            dirty.full_rescan_required |= full_rescan_required;
            save_dirty(tx, &dirty)?;
            Ok(dirty)
        })
    }
    pub fn cover_library_dirty(
        &self,
        root_id: &str,
        grant_epoch: u64,
        watch_generation: u64,
        covered_sequence: u64,
        complete: bool,
        check: &dyn Fn() -> Result<()>,
    ) -> Result<LibraryDirtyState> {
        self.work_transaction(check, |tx| {
            require_watch(tx, root_id, grant_epoch, watch_generation)?;
            let mut dirty = dirty_at(tx, root_id)?.context("Library dirty state missing")?;
            ensure!(
                covered_sequence <= dirty.received_sequence,
                LibraryError::conflict("Library coverage exceeds received events")
            );
            if complete {
                dirty.covered_sequence = dirty.covered_sequence.max(covered_sequence);
                if dirty.covered_sequence == dirty.received_sequence {
                    dirty.full_rescan_required = false;
                }
            }
            save_dirty(tx, &dirty)?;
            Ok(dirty)
        })
    }
    pub fn library_dirty(&self, root_id: &str) -> Result<Option<LibraryDirtyState>> {
        validate_library_id(root_id)?;
        let connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("Repository lock was poisoned"))?;
        schema::validate_current(&connection)?;
        dirty_at(&connection, root_id)
    }
    pub fn library_asset_binding(&self, asset_id: &str) -> Result<Option<LibraryAssetBinding>> {
        validate_library_id(asset_id)?;
        let connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("Repository lock was poisoned"))?;
        schema::validate_current(&connection)?;
        binding_at(&connection, asset_id)
    }
    pub fn library_root_binding(&self, root_id: &str) -> Result<Option<LibraryRootBinding>> {
        validate_library_id(root_id)?;
        let connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("Repository lock was poisoned"))?;
        schema::validate_current(&connection)?;
        root_binding_at(&connection, root_id)
    }
    pub fn prepare_library_identity(
        &self,
        item: &LibraryPreparedAsset,
        check: &dyn Fn() -> Result<()>,
    ) -> Result<LibraryPreparedAsset> {
        self.prepare_library_identities(std::slice::from_ref(item), check)?
            .pop()
            .context("Prepared library identity is missing")
    }
    pub fn prepare_library_identities(
        &self,
        items: &[LibraryPreparedAsset],
        check: &dyn Fn() -> Result<()>,
    ) -> Result<Vec<LibraryPreparedAsset>> {
        ensure!(
            items.len() <= MAX_LIBRARY_BATCH_ITEMS,
            LibraryError::budget("Library identity batch exceeds its item budget")
        );
        let mut bytes = 0usize;
        for item in items {
            validate_prepared_identity(item)?;
            bytes = bytes
                .checked_add(serde_json::to_vec(item)?.len())
                .context("Library identity batch size overflow")?;
            ensure!(
                bytes <= MAX_LIBRARY_BATCH_BYTES,
                LibraryError::budget("Library identity batch exceeds its byte budget")
            );
        }
        if items.is_empty() {
            check()?;
            return Ok(Vec::new());
        }
        self.work_transaction(check, |tx| {
            let now = now_ms()?;
            tx.execute(
                "DELETE FROM library_identity_reservations WHERE expires_at_ms<=?1",
                [now as i64],
            )?;
            let mut results = Vec::with_capacity(items.len());
            for item in items {
                check()?;
                results.push(prepare_identity_at(tx, item, now)?);
            }
            let count: i64 = tx.query_row(
                "SELECT count(*) FROM library_identity_reservations",
                [],
                |row| row.get(0),
            )?;
            ensure!(
                count <= MAX_LIBRARY_ASSETS as i64,
                LibraryError::budget("Library identity reservation budget exceeded")
            );
            check_work_storage(tx, "library_identity_reservations", WORK_BYTES)?;
            Ok(results)
        })
    }
    // These fields jointly fence the persisted representation and its live proof.
    #[allow(clippy::too_many_arguments)]
    pub fn put_library_content_evidence(
        &self,
        asset_id: &str,
        binding_revision: u64,
        identity: &FileIdentity,
        size_bytes: u64,
        modified_ms: Option<u64>,
        sha256: &str,
        check: &dyn Fn() -> Result<()>,
    ) -> Result<()> {
        validate_content_sha256(sha256)?;
        identity.validate()?;
        let record = super::LibraryContentEvidenceCommit {
            asset_id: asset_id.to_owned(),
            expected_binding_revision: binding_revision,
            file_identity: identity.clone(),
            size_bytes,
            modified_ms,
            content_sha256: sha256.to_owned(),
        };
        self.work_transaction(check, |transaction| {
            super::library_evidence_batch::update_content_evidence_at(transaction, &record)
        })
    }
    pub fn library_assets_for_identity(
        &self,
        identity: &FileIdentity,
        limit: usize,
    ) -> Result<Vec<LibraryAssetBinding>> {
        page_limits(0, limit)?;
        let key = physical_identity_key(identity)?;
        let connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("Repository lock was poisoned"))?;
        schema::validate_current(&connection)?;
        let mut stmt=connection.prepare("SELECT asset_id FROM local_asset_locations WHERE physical_identity_key=?1 ORDER BY asset_id LIMIT ?2")?;
        let mut result = Vec::new();
        let mut bytes = 4096;
        for row in stmt.query_map(params![key, limit as i64], |row| row.get::<_, String>(0))? {
            let value = binding_at(&connection, &row?)?.context("Identity binding missing")?;
            let size = serde_json::to_vec(&value)?.len() + 1;
            if bytes + size > MAX_LIBRARY_PAGE_BYTES {
                break;
            }
            bytes += size;
            result.push(value);
        }
        Ok(result)
    }
    pub fn put_library_relink_candidate(
        &self,
        candidate: &LibraryRelinkCandidate,
        check: &dyn Fn() -> Result<()>,
    ) -> Result<()> {
        candidate.validate()?;
        self.work_transaction(check,|tx|{require_watch(tx,&candidate.root_id,candidate.grant_epoch,candidate.watch_generation)?;let binding=binding_at(tx,&candidate.asset_id)?.context("Candidate asset missing")?;
            ensure!(binding.binding_revision==candidate.binding_revision && normalize_local_path(&binding.path)==normalize_local_path(&candidate.old_path),LibraryError::conflict("Candidate source binding changed"));
            let root=require_root(tx,&candidate.root_id)?;ensure!(path_in_root(&candidate.new_path,&root),LibraryError::invalid("Candidate lies outside its root"));
            let member:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM library_membership WHERE root_id=?1 AND asset_id=?2)",params![root.root_id,candidate.asset_id],|row|row.get(0))?;ensure!(member,LibraryError::conflict("Candidate has no original root membership"));
            let now=now_ms()?;ensure!(candidate.expires_at_ms>now,LibraryError::invalid("Candidate already expired"));tx.execute("DELETE FROM library_relink_candidates WHERE expires_at_ms<=?1",[now as i64])?;
            tx.execute("INSERT INTO library_relink_candidates VALUES(?1,?2,?3,?4,?5) ON CONFLICT(candidate_id) DO UPDATE SET asset_id=excluded.asset_id,root_id=excluded.root_id,expires_at_ms=excluded.expires_at_ms,document=excluded.document",params![candidate.candidate_id,candidate.asset_id,candidate.root_id,candidate.expires_at_ms as i64,encode_document(candidate)?])?;
            let count:i64=tx.query_row("SELECT count(*) FROM library_relink_candidates",[],|row|row.get(0))?;ensure!(count<=MAX_RELINK_CANDIDATES as i64,LibraryError::budget("Relink candidate budget exceeded"));check_work_storage(tx,"library_relink_candidates",WORK_BYTES)?;Ok(())})
    }
    pub fn library_relink_candidate(
        &self,
        candidate_id: &str,
        now_ms: u64,
    ) -> Result<Option<LibraryRelinkCandidate>> {
        validate_library_id(candidate_id)?;
        let connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("Repository lock was poisoned"))?;
        schema::validate_current(&connection)?;
        candidate_at(&connection, candidate_id, now_ms)
    }
    pub fn library_relink_candidates_page(
        &self,
        root_id: Option<&str>,
        after: Option<&str>,
        limit: usize,
        now_ms: u64,
    ) -> Result<LibraryRelinkCandidatePage> {
        page_limits(0, limit)?;
        ensure!(
            now_ms <= i64::MAX as u64,
            LibraryError::invalid("Candidate time overflow")
        );
        if let Some(id) = root_id {
            validate_library_id(id)?
        }
        if let Some(id) = after {
            validate_library_id(id)?
        }
        let connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("Repository lock was poisoned"))?;
        schema::validate_current(&connection)?;
        let total: i64 = connection.query_row(
            &format!("SELECT count(*) {VALID_CANDIDATES_FROM}"),
            params![now_ms as i64, root_id],
            |row| row.get(0),
        )?;
        let mut stmt=connection.prepare(&format!("SELECT c.candidate_id {VALID_CANDIDATES_FROM} AND (?3 IS NULL OR c.candidate_id>?3) ORDER BY c.candidate_id LIMIT ?4"))?;
        let mut items = Vec::new();
        let mut bytes = 4096;
        let mut more = false;
        for row in stmt.query_map(
            params![now_ms as i64, root_id, after, limit as i64 + 1],
            |row| row.get::<_, String>(0),
        )? {
            if items.len() == limit {
                more = true;
                break;
            }
            if let Some(item) = candidate_at(&connection, &row?, now_ms)? {
                let size = serde_json::to_vec(&item)?.len() + 1;
                if bytes + size > MAX_LIBRARY_PAGE_BYTES {
                    more = true;
                    break;
                }
                bytes += size;
                items.push(item);
            }
        }
        let next_candidate_id = if more {
            items.last().map(|item| item.candidate_id.clone())
        } else {
            None
        };
        Ok(LibraryRelinkCandidatePage {
            items,
            total: total as usize,
            next_candidate_id,
            revision: current_revision(&connection)?,
        })
    }

    pub fn library_scan_scope(&self, token: &LibraryScanToken) -> Result<LibraryScanScope> {
        token.validate()?;
        let connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("Repository lock was poisoned"))?;
        schema::validate_current(&connection)?;
        scope_at(&connection, token)
    }
    pub fn preview_manual_relink_authority(
        &self,
        prepared: &LibraryPreparedAsset,
        expected_binding_revision: u64,
    ) -> Result<LocalAssetAuthority> {
        validate_prepared_identity(prepared)?;
        ensure!(
            prepared.file_identity.is_some(),
            LibraryError::invalid("Relink preview requires the actual selected file identity")
        );
        let connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("Repository lock was poisoned"))?;
        schema::validate_current(&connection)?;
        let old =
            asset_at(&connection, &prepared.asset.id)?.context("Relink preview asset missing")?;
        let binding =
            binding_at(&connection, &old.id)?.context("Relink preview binding missing")?;
        ensure!(
            binding.binding_revision == expected_binding_revision
                && old.track_id == prepared.track.id
                && prepared.asset.track_id == old.track_id,
            LibraryError::conflict("Relink preview source binding changed")
        );
        let previous: Option<i64> = connection
            .query_row(
                "SELECT epoch FROM local_file_grants WHERE asset_id=?1",
                [&old.id],
                |row| row.get(0),
            )
            .optional()?;
        let renew = exact_file_epoch(&connection, &old.id)?.is_none()
            || normalize_local_path(&old.path) != normalize_local_path(&prepared.asset.path)
            || binding.file_identity != prepared.file_identity;
        let epoch = match previous {
            None => 1,
            Some(epoch) if renew => next_number(epoch as u64)?,
            Some(epoch) => epoch as u64,
        };
        let mut stmt = connection
            .prepare("SELECT root_id FROM library_membership WHERE asset_id=?1 ORDER BY root_id")?;
        let mut roots = Vec::new();
        for row in stmt.query_map([&old.id], |row| row.get::<_, String>(0))? {
            if let Some(root) = root_at(&connection, &row?)?
                && path_in_root(&prepared.asset.path, &root)
            {
                roots.push(root);
            }
        }
        Ok(LocalAssetAuthority {
            asset_id: old.id,
            track_id: old.track_id,
            path: normalize_local_path(&prepared.asset.path),
            explicit_file_epoch: Some(epoch),
            roots,
        })
    }
    pub fn commit_library_relink(
        &self,
        request: &LibraryRelinkCommit,
        check: &dyn Fn() -> Result<()>,
    ) -> Result<LibraryRelinkRepositoryCommit> {
        validate_library_id(&request.operation_id)?;
        validate_prepared_identity(&request.prepared)?;
        ensure!(
            request.expected_binding_revision > 0
                && request.expected_binding_revision <= i64::MAX as u64,
            LibraryError::invalid("Invalid expected binding revision")
        );
        if let Some(hash) = &request.content_sha256 {
            validate_content_sha256(hash)?;
        }
        let digest = format!(
            "{:x}",
            Sha256::digest(serde_json::to_vec(&(
                request.operation_id.as_str(),
                request.expected_binding_revision,
                &request.prepared,
                &request.authorization,
                request.evidence,
                &request.content_sha256,
                request.allow_content_change
            ))?)
        );
        check()?;
        {
            let connection = self
                .connection
                .lock()
                .map_err(|_| anyhow::anyhow!("Repository lock was poisoned"))?;
            let old:Option<(String,String)>=connection.query_row("SELECT request_digest,document FROM library_relink_results WHERE operation_id=?1",[&request.operation_id],|row|Ok((row.get(0)?,row.get(1)?))).optional()?;
            if let Some((old_digest, document)) = old {
                ensure!(
                    old_digest == digest,
                    LibraryError::conflict("Relink operation ID was reused")
                );
                let result: LibraryRelinkResult = decode_document(&document)?;
                return Ok(LibraryRelinkRepositoryCommit {
                    change: RepositoryChange {
                        revision: result.revision,
                        kind: ChangeKind::LocalAssetRelinked,
                        track_id: Some(result.track_id.clone()),
                        details: serde_json::json!({"operation_id":result.operation_id}),
                    },
                    result,
                });
            }
        }
        let (result,change)=self.write_checked(None,|tx|{
            let item=&request.prepared;let old=asset_at(tx,&item.asset.id)?.context("Relink asset missing")?;let binding=binding_at(tx,&old.id)?.context("Relink binding missing")?;
            let original=track_at(tx,&old.track_id)?.context("Relink track missing")?;
            ensure!(binding.binding_revision==request.expected_binding_revision && item.asset.track_id==old.track_id&&item.track.id==original.id&&item.track.lx_id==original.lx_id&&item.asset.available&&item.track.platform.source=="local",LibraryError::conflict("Relink source identity or binding changed"));
            let target_key=library_path_key(&item.asset.path,native_library_path_policy())?;
            let occupied:Option<String>=tx.query_row("SELECT asset_id FROM local_asset_locations WHERE path_key=?1",[&target_key],|row|row.get(0)).optional()?;
            ensure!(occupied.is_none_or(|id|id==old.id),LibraryError::conflict("Relink target already belongs to another asset"));
            let stable_stamp=binding.file_identity.as_ref().zip(item.file_identity.as_ref()).is_some_and(|(a,b)|a==b)&&old.size_bytes==item.asset.size_bytes&&old.modified_ms==item.asset.modified_ms;
            let old_hash=binding.content_sha256.as_ref().filter(|_|binding.hash_size_bytes==Some(old.size_bytes)&&binding.hash_modified_ms==old.modified_ms);
            let same_hash=old_hash.zip(request.content_sha256.as_ref()).is_some_and(|(a,b)|a.eq_ignore_ascii_case(b));
            let known_different=old_hash.zip(request.content_sha256.as_ref()).is_some_and(|(a,b)|!a.eq_ignore_ascii_case(b));
            ensure!(request.allow_content_change||!known_different,LibraryError::conflict("Selected file content differs from the original asset"));
            let manual=matches!(request.authorization,LibraryRelinkAuthorization::Manual);
            let renew_explicit=manual&&(exact_file_epoch(tx,&old.id)?.is_none()||normalize_local_path(&old.path)!=normalize_local_path(&item.asset.path)||binding.file_identity!=item.file_identity);
            if let LibraryRelinkAuthorization::Root{root_id,grant_epoch,watch_generation}=&request.authorization {
                let root=require_watch(tx,root_id,*grant_epoch,*watch_generation)?;
                let preference=watch_at(tx,root_id)?.context("Relink watch preference missing")?;
                let member:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM library_membership WHERE root_id=?1 AND asset_id=?2)",params![root_id,old.id],|row|row.get(0))?;
                ensure!(preference.options.auto_relink&&member&&path_in_root(&old.path,&root)&&path_in_root(&item.asset.path,&root),LibraryError::conflict("Automatic relocation has no original root scope"));
                let same_id=binding.file_identity.as_ref().zip(item.file_identity.as_ref()).is_some_and(|(a,b)|a==b);
                ensure!(!known_different&&same_id&&match request.evidence {RelinkEvidence::SameOpenObject=>stable_stamp,RelinkEvidence::SameFileAndContent=>same_hash,RelinkEvidence::ManualSelection=>false},LibraryError::conflict("Automatic relocation requires verified object/content evidence"));
            }
            let media_changed=known_different||!(same_hash || (request.evidence==RelinkEvidence::SameOpenObject&&stable_stamp));
            let mut track=item.track.clone();track.wire=original.wire;let mut metadata=original.metadata;metadata.extend(track.metadata);let path=normalize_local_path(&item.asset.path).to_str().context("Relink path is not Unicode")?.to_owned();metadata.insert("filePath".into(),path.clone().into());metadata.insert("songId".into(),path.clone().into());metadata.insert("ext".into(),item.asset.extension.clone().into());track.metadata=metadata;track.platform.song_id=path.into();
            check()?;tracks::write_track(tx,&track,&encode_document(&track)?)?;tracks::write_asset(tx,&item.asset,&encode_document(&item.asset)?)?;
            let revision=next_number(binding.binding_revision)?;
            let identity=item.file_identity.clone().ok_or_else(||LibraryError::invalid("Relink requires the selected file's actual identity"))?;
            let content=request.content_sha256.as_ref().map(|hash|hash.to_ascii_lowercase()).or_else(||stable_stamp.then(||old_hash.cloned()).flatten());
            let location=AssetLocation{path:normalize_local_path(&item.asset.path),file_identity:Some(identity.clone()),hash_size_bytes:content.as_ref().map(|_|item.asset.size_bytes),hash_modified_ms:content.as_ref().and(item.asset.modified_ms),content_sha256:content,size_bytes:Some(item.asset.size_bytes),modified_ms:item.asset.modified_ms};
            tx.execute("UPDATE local_asset_locations SET path_key=?2,binding_revision=?3,physical_identity_key=?4,document=?5 WHERE asset_id=?1",params![old.id,target_key,revision as i64,physical_identity_key(&identity)?,encode_document(&location)?])?;
            let roots={let mut stmt=tx.prepare("SELECT root_id FROM library_membership WHERE asset_id=?1")?;stmt.query_map([&old.id],|row|row.get::<_,String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?};
            for root_id in roots{check()?;if root_at(tx,&root_id)?.is_none_or(|root|!path_in_root(&item.asset.path,&root)){tx.execute("DELETE FROM library_membership WHERE root_id=?1 AND asset_id=?2",params![root_id,old.id])?;}}
            if renew_explicit{write_exact_file_grant(tx,&item.asset,true)?;}
            refresh_list_metadata(tx,&track,check)?;
            tx.execute("DELETE FROM library_relink_candidates WHERE asset_id=?1",[&old.id])?;
            tx.execute("DELETE FROM library_identity_reservations WHERE path_key=?1",[&target_key])?;
            let result=LibraryRelinkResult{operation_id:request.operation_id.clone(),asset_id:old.id.clone(),track_id:old.track_id.clone(),binding_revision:revision,old_path:old.path,new_path:item.asset.path.clone(),media_changed,explicit_grant_renewed:renew_explicit,revision:next_revision(tx)?};
            tx.execute("INSERT INTO library_relink_results VALUES(?1,?2,?3,?4)",params![request.operation_id,digest,old.id,encode_document(&result)?])?;
            prune_results(tx)?;check_library_storage(tx)?;
            Ok((result,ChangeKind::LocalAssetRelinked,Some(track.id),serde_json::json!({"operation_id":request.operation_id,"asset_id":old.id,"binding_revision":revision,"media_changed":media_changed})))
        },check)?;
        Ok(LibraryRelinkRepositoryCommit { result, change })
    }
    pub fn library_relink_result(&self, operation_id: &str) -> Result<Option<LibraryRelinkResult>> {
        validate_library_id(operation_id)?;
        let connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("Repository lock was poisoned"))?;
        schema::validate_current(&connection)?;
        let doc: Option<String> = connection
            .query_row(
                "SELECT document FROM library_relink_results WHERE operation_id=?1",
                [operation_id],
                |row| row.get(0),
            )
            .optional()?;
        doc.map(|doc| decode_document(&doc)).transpose()
    }
    pub fn rebind_library_root(
        &self,
        root_id: &str,
        draft: &LibraryRootDraft,
        expected_grant_epoch: u64,
        expected_location_revision: u64,
        manual: bool,
        check: &dyn Fn() -> Result<()>,
    ) -> Result<(LibraryRoot, RepositoryChange)> {
        draft.validate()?;
        validate_library_id(root_id)?;
        self.write_checked(None,|tx|{
            let mut root=require_root(tx,root_id)?;let binding=root_binding_at(tx,root_id)?.context("Root binding missing")?;
            ensure!(root.grant_epoch==expected_grant_epoch&&binding.location_revision==expected_location_revision,LibraryError::conflict("Root relocation token expired"));
            let same=root.anchor==draft.anchor&&root.recursive==draft.recursive&&root.path_policy==draft.path_policy;
            if !manual{let preference=watch_at(tx,root_id)?.context("Root watch preference missing")?;ensure!(same&&preference.enabled&&preference.options.root_move_policy==RootMovePolicy::FollowSameObject,LibraryError::conflict("Root relocation requires an explicitly enabled same-object policy"));}
            if same&&normalize_local_path(&root.path)==normalize_local_path(&draft.path){return Ok((root.clone(),ChangeKind::LibraryRootRelinked,None,serde_json::json!({"root_id":root_id,"grant_epoch":root.grant_epoch,"location_revision":binding.location_revision,"same_object":true,"relocated_asset_ids":[],"unchanged":true})));}
            let key=library_path_key(&draft.path,draft.path_policy)?;let occupied:Option<String>=tx.query_row("SELECT root_id FROM library_roots WHERE path_key=?1",[&key],|row|row.get(0)).optional()?;ensure!(occupied.is_none_or(|id|id==root_id),LibraryError::conflict("Root target is already registered"));
            cancel_root_runs(tx,root_id,"Root location changed during scan")?;root.generation=next_number(root.generation)?;
            if !same{root.grant_epoch=next_number(root.grant_epoch)?;root.last_complete_generation=0;root.watch_enabled=false;tx.execute("DELETE FROM library_membership WHERE root_id=?1",[root_id])?;disable_root_watch(tx,root_id)?;}
            let previous_root=root.clone();
            root.path=normalize_local_path(&draft.path);root.anchor=draft.anchor.clone();root.recursive=draft.recursive;root.path_policy=draft.path_policy;
            tx.execute("UPDATE library_roots SET path_key=?2,grant_epoch=?3,generation=?4,document=?5 WHERE root_id=?1",params![root_id,key,root.grant_epoch as i64,root.generation as i64,encode_document(&root)?])?;
            let mut relocated_asset_ids=Vec::new();
            if same {
                let members={let mut stmt=tx.prepare("SELECT asset_id FROM library_membership WHERE root_id=?1 ORDER BY asset_id")?;stmt.query_map([root_id],|row|row.get::<_,String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?};
                ensure!(members.len()<=MAX_LIBRARY_ASSETS,LibraryError::budget("Root relocation asset budget exceeded"));
                for asset_id in members {check()?;let asset=asset_at(tx,&asset_id)?.context("Root relocation member missing")?;let relative=relative_member_path(&asset.path,&previous_root)?;let new_path=root.path.join(relative);rebind_asset_locator(tx,&asset_id,&new_path,check)?;relocated_asset_ids.push(asset_id);}
            }
            let location_revision=next_number(binding.location_revision)?;tx.execute("UPDATE library_root_locations SET location_revision=?2 WHERE root_id=?1",params![root_id,location_revision as i64])?;
            if same {let mut preference=watch_at(tx,root_id)?.context("Root watch preference missing")?;preference.watch_generation=next_number(preference.watch_generation)?;save_watch(tx,&preference)?;}
            tx.execute("DELETE FROM library_relink_candidates WHERE root_id=?1",[root_id])?;
            check_library_storage(tx)?;
            Ok((root.clone(),ChangeKind::LibraryRootRelinked,None,serde_json::json!({"root_id":root_id,"grant_epoch":root.grant_epoch,"location_revision":location_revision,"same_object":same,"relocated_asset_ids":relocated_asset_ids})))
        },check)
    }
}

fn page_limits(offset: usize, limit: usize) -> Result<()> {
    ensure!(
        (1..=1000).contains(&limit) && offset <= i64::MAX as usize,
        LibraryError::invalid("Invalid library page limit")
    );
    Ok(())
}
fn now_ms() -> Result<u64> {
    let value = SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis();
    ensure!(
        value < i64::MAX as u128 - 600000,
        LibraryError::invalid("Library clock overflow")
    );
    Ok(value as u64)
}
fn random_identity() -> Result<String> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes)
        .map_err(|error| anyhow::anyhow!("Cannot allocate local identity: {error}"))?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}
pub(super) fn fresh_root_id() -> Result<String> {
    Ok(format!("library-root-v2:{}", random_identity()?))
}
fn prepare_identity_at(
    tx: &Transaction<'_>,
    item: &LibraryPreparedAsset,
    now: u64,
) -> Result<LibraryPreparedAsset> {
    let key = library_path_key(&item.asset.path, native_library_path_policy())?;
    let existing: Option<String> = tx
        .query_row(
            "SELECT asset_id FROM local_asset_locations WHERE path_key=?1",
            [&key],
            |row| row.get(0),
        )
        .optional()?;
    let mut result = item.clone();
    if let Some(id) = existing {
        let asset = asset_at(tx, &id)?.context("Identity asset disappeared")?;
        let binding = binding_at(tx, &id)?.context("Identity binding disappeared")?;
        ensure!(
            normalize_local_path(&asset.path) == normalize_local_path(&item.asset.path)
                || binding
                    .file_identity
                    .as_ref()
                    .zip(item.file_identity.as_ref())
                    .is_some_and(|(a, b)| a == b),
            LibraryError::conflict("Library physical path collision")
        );
        let track: TrackRecord =
            track_at(tx, &asset.track_id)?.context("Identity track disappeared")?;
        assign_identity(&mut result, &asset.id, &track.id, &track.lx_id);
        return Ok(result);
    }
    let reservation: Option<String> = tx
        .query_row(
            "SELECT document FROM library_identity_reservations WHERE path_key=?1",
            [&key],
            |row| row.get(0),
        )
        .optional()?;
    if let Some(document) = reservation {
        let saved: IdentityReservation = decode_document(&document)?;
        ensure!(
            normalize_local_path(&saved.path) == normalize_local_path(&item.asset.path)
                || saved
                    .file_identity
                    .as_ref()
                    .zip(item.file_identity.as_ref())
                    .is_some_and(|(a, b)| a == b),
            LibraryError::conflict("Reserved library path collision")
        );
        assign_identity(&mut result, &saved.asset_id, &saved.track_id, &saved.lx_id);
        return Ok(result);
    }
    let collision:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM local_assets WHERE identity_key=?1) OR EXISTS(SELECT 1 FROM tracks WHERE id=?2 OR json_extract(document,'$.lx_id')=?3) OR EXISTS(SELECT 1 FROM library_identity_reservations WHERE asset_id=?1 OR track_id=?2 OR lx_id=?3)",params![item.asset.id,item.track.id,item.track.lx_id],|row|row.get(0))?;
    if collision {
        let entropy = random_identity()?;
        assign_identity(
            &mut result,
            &format!("asset-v2:{entropy}"),
            &format!("local-v2:{entropy}"),
            &format!("local-v2:{entropy}"),
        );
    }
    let saved = IdentityReservation {
        path: normalize_local_path(&item.asset.path),
        asset_id: result.asset.id.clone(),
        track_id: result.track.id.clone(),
        lx_id: result.track.lx_id.clone(),
        file_identity: result.file_identity.clone(),
    };
    tx.execute(
        "INSERT INTO library_identity_reservations VALUES(?1,?2,?3,?4,?5,?6,?7)",
        params![
            key,
            saved.asset_id,
            saved.track_id,
            saved.lx_id,
            saved
                .file_identity
                .as_ref()
                .map(physical_identity_key)
                .transpose()?,
            now.saturating_add(600000) as i64,
            encode_document(&saved)?
        ],
    )?;
    Ok(result)
}
fn assign_identity(item: &mut LibraryPreparedAsset, asset_id: &str, track_id: &str, lx_id: &str) {
    item.asset.id = asset_id.into();
    item.asset.track_id = track_id.into();
    item.track.id = track_id.into();
    item.track.local_asset_id = Some(asset_id.into());
    item.track.lx_id = lx_id.into();
}
fn validate_prepared_identity(item: &LibraryPreparedAsset) -> Result<()> {
    validate_library_path(&item.asset.path)?;
    validate_library_id(&item.asset.id)?;
    validate_library_id(&item.track.id)?;
    ensure!(
        item.asset.track_id == item.track.id
            && item.track.local_asset_id.as_deref() == Some(item.asset.id.as_str()),
        LibraryError::invalid("Local identity pair disagrees")
    );
    if let Some(id) = &item.file_identity {
        id.validate()?
    }
    Ok(())
}
fn track_at(connection: &Connection, track_id: &str) -> Result<Option<TrackRecord>> {
    let doc: Option<String> = connection
        .query_row(
            "SELECT document FROM tracks WHERE id=?1",
            [track_id],
            |row| row.get(0),
        )
        .optional()?;
    doc.map(|doc| decode_document(&doc)).transpose()
}
fn location_at(connection: &Connection, asset_id: &str) -> Result<Option<AssetLocation>> {
    let doc: Option<String> = connection
        .query_row(
            "SELECT document FROM local_asset_locations WHERE asset_id=?1",
            [asset_id],
            |row| row.get(0),
        )
        .optional()?;
    doc.map(|doc| decode_document(&doc)).transpose()
}
pub(super) fn binding_at(
    connection: &Connection,
    asset_id: &str,
) -> Result<Option<LibraryAssetBinding>> {
    let row: Option<(i64, String)> = connection
        .query_row(
            "SELECT binding_revision,document FROM local_asset_locations WHERE asset_id=?1",
            [asset_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    row.map(|(revision, doc)| {
        let location: AssetLocation = decode_document(&doc)?;
        Ok(LibraryAssetBinding {
            asset_id: asset_id.into(),
            path: location.path,
            binding_revision: revision as u64,
            file_identity: location.file_identity,
            content_sha256: location.content_sha256,
            hash_size_bytes: location.hash_size_bytes,
            hash_modified_ms: location.hash_modified_ms,
        })
    })
    .transpose()
}
fn root_binding_at(connection: &Connection, root_id: &str) -> Result<Option<LibraryRootBinding>> {
    let Some(root) = root_at(connection, root_id)? else {
        return Ok(None);
    };
    let rev: i64 = connection.query_row(
        "SELECT location_revision FROM library_root_locations WHERE root_id=?1",
        [root_id],
        |row| row.get(0),
    )?;
    Ok(Some(LibraryRootBinding {
        root_id: root_id.into(),
        path: root.path,
        location_revision: rev as u64,
    }))
}
fn watch_at(connection: &Connection, root_id: &str) -> Result<Option<LibraryWatchPreference>> {
    let row:Option<(i64,bool,String)>=connection.query_row("SELECT watch_generation,enabled,document FROM library_watch_preferences WHERE root_id=?1",[root_id],|row|Ok((row.get(0)?,row.get(1)?,row.get(2)?))).optional()?;
    row.map(|(generation, enabled, doc)| {
        let preference: LibraryWatchPreference = decode_document(&doc)?;
        preference.options.validate()?;
        ensure!(
            preference.root_id == root_id
                && preference.watch_generation == generation as u64
                && preference.enabled == enabled,
            LibraryError::invalid("Watch preference columns disagree")
        );
        Ok(preference)
    })
    .transpose()
}
fn save_watch(transaction: &Transaction<'_>, preference: &LibraryWatchPreference) -> Result<()> {
    transaction.execute("UPDATE library_watch_preferences SET watch_generation=?2,enabled=?3,document=?4 WHERE root_id=?1",params![preference.root_id,preference.watch_generation as i64,preference.enabled,encode_document(preference)?])?;
    transaction.execute("UPDATE library_dirty_state SET watch_generation=?2,received_sequence=0,covered_sequence=0,full_rescan_required=?3 WHERE root_id=?1",params![preference.root_id,preference.watch_generation as i64,preference.enabled])?;
    transaction.execute(
        "DELETE FROM library_relink_candidates WHERE root_id=?1",
        [&preference.root_id],
    )?;
    Ok(())
}
fn require_watch(
    connection: &Connection,
    root_id: &str,
    epoch: u64,
    generation: u64,
) -> Result<LibraryRoot> {
    validate_library_id(root_id)?;
    let root = require_root(connection, root_id)?;
    let preference = watch_at(connection, root_id)?.context("Watch preference missing")?;
    ensure!(
        root.grant_epoch == epoch
            && preference.enabled
            && preference.watch_generation == generation,
        LibraryError::conflict("Watch event token expired")
    );
    Ok(root)
}
fn dirty_at(connection: &Connection, root_id: &str) -> Result<Option<LibraryDirtyState>> {
    connection.query_row("SELECT watch_generation,received_sequence,covered_sequence,full_rescan_required FROM library_dirty_state WHERE root_id=?1",[root_id],|row|Ok(LibraryDirtyState{root_id:root_id.into(),watch_generation:row.get::<_,i64>(0)? as u64,received_sequence:row.get::<_,i64>(1)? as u64,covered_sequence:row.get::<_,i64>(2)? as u64,full_rescan_required:row.get(3)?})).optional().map_err(Into::into)
}
fn save_dirty(transaction: &Transaction<'_>, state: &LibraryDirtyState) -> Result<()> {
    transaction.execute("UPDATE library_dirty_state SET received_sequence=?2,covered_sequence=?3,full_rescan_required=?4 WHERE root_id=?1 AND watch_generation=?5",params![state.root_id,state.received_sequence as i64,state.covered_sequence as i64,state.full_rescan_required,state.watch_generation as i64])?;
    Ok(())
}
fn check_work_storage(connection: &Connection, table: &str, limit: i64) -> Result<()> {
    let sql = match table {
        "library_identity_reservations" => {
            "SELECT coalesce(sum(length(CAST(document AS BLOB))),0) FROM library_identity_reservations"
        }
        "library_relink_candidates" => {
            "SELECT coalesce(sum(length(CAST(document AS BLOB))),0) FROM library_relink_candidates"
        }
        "library_relink_results" => {
            "SELECT coalesce(sum(length(CAST(document AS BLOB))),0) FROM library_relink_results"
        }
        _ => return Err(LibraryError::invalid("Unknown work storage table").into()),
    };
    let bytes: i64 = connection.query_row(sql, [], |row| row.get(0))?;
    ensure!(
        bytes <= limit,
        LibraryError::budget("Library work metadata budget exceeded")
    );
    Ok(())
}
fn candidate_at(
    connection: &Connection,
    id: &str,
    now: u64,
) -> Result<Option<LibraryRelinkCandidate>> {
    ensure!(
        now <= i64::MAX as u64,
        LibraryError::invalid("Candidate time overflow")
    );
    let doc: Option<String> = connection
        .query_row(
            &format!("SELECT c.document {VALID_CANDIDATES_FROM} AND c.candidate_id=?3"),
            params![now as i64, Option::<&str>::None, id],
            |row| row.get(0),
        )
        .optional()?;
    let Some(doc) = doc else { return Ok(None) };
    let candidate: LibraryRelinkCandidate = decode_document(&doc)?;
    candidate.validate()?;
    if require_watch(
        connection,
        &candidate.root_id,
        candidate.grant_epoch,
        candidate.watch_generation,
    )
    .is_err()
    {
        return Ok(None);
    };
    let current = binding_at(connection, &candidate.asset_id)?;
    if current.is_none_or(|value| {
        value.binding_revision != candidate.binding_revision || value.path != candidate.old_path
    }) {
        return Ok(None);
    }
    Ok(Some(candidate))
}
pub(super) fn scope_at(
    connection: &Connection,
    token: &LibraryScanToken,
) -> Result<LibraryScanScope> {
    token.validate()?;
    let doc:String=connection.query_row("SELECT scope_document FROM library_scan_runs WHERE root_id=?1 AND generation=?2 AND operation_id=?3 AND grant_epoch=?4",params![token.root_id,token.generation as i64,token.operation_id,token.grant_epoch as i64],|row|row.get(0))?;
    let scope: LibraryScanScope = decode_document(&doc)?;
    scope.validate()?;
    Ok(scope)
}
fn prune_results(transaction: &Transaction<'_>) -> Result<()> {
    transaction.execute("DELETE FROM library_relink_results WHERE rowid IN (SELECT rowid FROM library_relink_results ORDER BY rowid DESC LIMIT -1 OFFSET 64)",[])?;
    check_work_storage(transaction, "library_relink_results", WORK_BYTES)
}
fn relative_member_path(path: &std::path::Path, root: &LibraryRoot) -> Result<PathBuf> {
    ensure!(
        path_in_root(path, root),
        LibraryError::conflict("Root relocation member escaped its scope")
    );
    let path = normalize_local_path(path);
    let count = normalize_local_path(&root.path).components().count();
    Ok(path.components().skip(count).collect())
}
pub(super) fn resolve_local_list_track(
    connection: &Connection,
    incoming: &TrackRecord,
) -> Result<TrackRecord> {
    if incoming.platform.source != "local" {
        return Ok(incoming.clone());
    }
    if incoming.local_asset_id.is_none()
        && !incoming.metadata.contains_key("filePath")
        && let Some(existing) = track_at(connection, &incoming.id)?
        && existing.platform.source == "local"
        && existing.local_asset_id.is_none()
        && !existing.metadata.contains_key("filePath")
        && existing.lx_id == incoming.lx_id
    {
        // Existing synthetic in-memory catalog entries carry no disk grant.
        return Ok(incoming.clone());
    }
    let wire_matches = {
        let mut stmt=connection.prepare("SELECT a.track_id FROM local_assets a JOIN tracks t ON t.id=a.track_id WHERE json_extract(t.document,'$.lx_id')=?1 UNION SELECT a.track_id FROM music_list_items i JOIN local_assets a ON a.track_id=i.track_id WHERE i.music_id=?1 ORDER BY 1 LIMIT 2")?;
        stmt.query_map([&incoming.lx_id], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?
    };
    ensure!(
        wire_matches.len() <= 1,
        LibraryError::conflict("Local LX music identity refers to multiple assets")
    );
    let wire_match = wire_matches.into_iter().next();
    let path = incoming
        .metadata
        .get("filePath")
        .and_then(serde_json::Value::as_str)
        .map(PathBuf::from);
    let path_key = path
        .as_ref()
        .filter(|path| validate_library_path(path).is_ok())
        .map(|path| library_path_key(path, native_library_path_policy()))
        .transpose()?;
    let path_match: Option<String> = if let Some(key) = &path_key {
        connection.query_row("SELECT a.track_id FROM local_asset_locations l JOIN local_assets a ON a.identity_key=l.asset_id WHERE l.path_key=?1",[key],|row|row.get(0)).optional()?
    } else {
        None
    };
    ensure!(
        wire_match
            .as_ref()
            .zip(path_match.as_ref())
            .is_none_or(|(a, b)| a == b),
        LibraryError::conflict("Local LX identity and current path refer to different assets")
    );
    let mut result = incoming.clone();
    if let Some(id) = wire_match.or(path_match) {
        let existing = track_at(connection, &id)?.context("Local list binding track missing")?;
        let asset_id = existing
            .local_asset_id
            .clone()
            .context("Local list binding asset missing")?;
        let asset = asset_at(connection, &asset_id)?.context("Local list binding asset missing")?;
        result.id = existing.id;
        result.local_asset_id = Some(asset_id);
        result.platform.song_id = asset
            .path
            .to_str()
            .context("Local list path is not Unicode")?
            .into();
        result
            .metadata
            .insert("filePath".into(), asset.path.to_str().unwrap().into());
        result
            .metadata
            .insert("songId".into(), result.platform.song_id.clone());
        result.metadata.insert("ext".into(), asset.extension.into());
    } else {
        result.id = stable_id(
            "unbound-local",
            serde_json::to_vec(&(&incoming.lx_id, path_key))?.as_slice(),
        );
        result.local_asset_id = None;
    }
    Ok(result)
}

fn rebind_asset_locator(
    transaction: &Transaction<'_>,
    asset_id: &str,
    path: &std::path::Path,
    check: &dyn Fn() -> Result<()>,
) -> Result<()> {
    let mut asset = asset_at(transaction, asset_id)?.context("Locator asset missing")?;
    let mut location = location_at(transaction, asset_id)?.context("Locator binding missing")?;
    let binding = binding_at(transaction, asset_id)?.context("Locator binding missing")?;
    let key = library_path_key(path, native_library_path_policy())?;
    let occupied: Option<String> = transaction
        .query_row(
            "SELECT asset_id FROM local_asset_locations WHERE path_key=?1",
            [&key],
            |row| row.get(0),
        )
        .optional()?;
    ensure!(
        occupied.is_none_or(|id| id == asset_id),
        LibraryError::conflict("Root relocation destination is already bound")
    );
    asset.path = normalize_local_path(path);
    location.path = asset.path.clone();
    transaction.execute(
        "UPDATE local_assets SET document=?2 WHERE identity_key=?1",
        params![asset_id, encode_document(&asset)?],
    )?;
    transaction.execute("UPDATE local_asset_locations SET path_key=?2,binding_revision=?3,document=?4 WHERE asset_id=?1",params![asset_id,key,next_number(binding.binding_revision)? as i64,encode_document(&location)?])?;
    let mut track = track_at(transaction, &asset.track_id)?.context("Locator track missing")?;
    let path_text = asset
        .path
        .to_str()
        .context("Locator path is not Unicode")?
        .to_owned();
    track.platform.song_id = path_text.clone().into();
    track
        .metadata
        .insert("filePath".into(), path_text.clone().into());
    track.metadata.insert("songId".into(), path_text.into());
    tracks::write_track(transaction, &track, &encode_document(&track)?)?;
    refresh_list_metadata(transaction, &track, check)?;
    let roots = {
        let mut stmt =
            transaction.prepare("SELECT root_id FROM library_membership WHERE asset_id=?1")?;
        stmt.query_map([asset_id], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?
    };
    for root_id in roots {
        check()?;
        if root_at(transaction, &root_id)?.is_none_or(|root| !path_in_root(&asset.path, &root)) {
            transaction.execute(
                "DELETE FROM library_membership WHERE root_id=?1 AND asset_id=?2",
                params![root_id, asset_id],
            )?;
        }
    }
    transaction.execute(
        "DELETE FROM library_relink_candidates WHERE asset_id=?1",
        [asset_id],
    )?;
    Ok(())
}

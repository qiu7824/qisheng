use super::{
    ChangeKind, Repository, RepositoryChange, decode_document, encode_document, schema, tracks,
};
use crate::domain::{
    library::*,
    music::{LocalAsset, normalize_local_path, stable_id},
};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde::{Deserialize, Serialize};
use std::{collections::HashSet, path::Path};

const MAX_LIBRARY_METADATA_BYTES: i64 = 64 * 1024 * 1024;
const MAX_ROOT_DOCUMENT_BYTES: i64 = 8 * 1024 * 1024;
const MAX_SCAN_SEEN_BYTES: i64 = 8 * 1024 * 1024;
const PAGE_ENVELOPE_BYTES: usize = 4096;
const MAX_VERIFIED_ROOT_PARAMETER_BYTES: usize = 256 * 1024;
#[path = "download_library.rs"]
mod download_library;
pub(crate) use download_library::DownloadReadCommit;

#[derive(Clone, Debug)]
pub struct LibraryBatchCommit {
    pub change: RepositoryChange,
    pub changed_asset_ids: Vec<String>,
}
#[derive(Clone, Debug)]
pub struct LibraryScanFinish {
    pub change: RepositoryChange,
    pub missing_asset_ids: Vec<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct AssetLocation {
    pub(super) path: std::path::PathBuf,
    pub(super) file_identity: Option<FileIdentity>,
    #[serde(default)]
    pub(super) content_sha256: Option<String>,
    #[serde(default)]
    pub(super) hash_size_bytes: Option<u64>,
    #[serde(default)]
    pub(super) hash_modified_ms: Option<u64>,
    #[serde(default)]
    pub(super) size_bytes: Option<u64>,
    #[serde(default)]
    pub(super) modified_ms: Option<u64>,
}

pub(super) fn initialize_library_assets(transaction: &Transaction<'_>) -> Result<()> {
    check_library_storage(transaction)?;
    let mut statement =
        transaction.prepare("SELECT identity_key FROM local_assets ORDER BY identity_key")?;
    for row in statement.query_map([], |row| row.get::<_, String>(0))? {
        let asset = asset_at(transaction, &row?)?.ok_or_else(|| {
            LibraryError::conflict("Legacy local asset disappeared during migration")
        })?;
        write_explicit_file_grant(transaction, &asset)?;
    }
    check_library_storage(transaction)
}

pub(super) fn write_explicit_file_grant(
    transaction: &Transaction<'_>,
    asset: &LocalAsset,
) -> Result<()> {
    write_location(transaction, asset, None)?;
    write_file_grant_only(transaction, asset)
}

pub(super) fn write_file_grant_only(
    transaction: &Transaction<'_>,
    asset: &LocalAsset,
) -> Result<()> {
    validate_library_id(&asset.id)?;
    validate_library_id(&asset.track_id)?;
    validate_library_path(&asset.path)?;
    // Explicit re-import retains the grant epoch. Content stamps separately
    // fence the actual file representation and prepared PCM/cache work.
    super::library_binding::write_exact_file_grant(transaction, asset, false)
}

pub(super) fn write_location(
    transaction: &Transaction<'_>,
    asset: &LocalAsset,
    identity: Option<&FileIdentity>,
) -> Result<()> {
    validate_library_path(&asset.path)?;
    if let Some(identity) = identity {
        identity.validate()?;
    }
    let key = library_path_key(&asset.path, native_library_path_policy())?;
    let existing: Option<String> = transaction
        .query_row(
            "SELECT asset_id FROM local_asset_locations WHERE path_key=?1",
            [&key],
            |row| row.get(0),
        )
        .optional()?;
    ensure!(
        existing.is_none_or(|value| value == asset.id),
        LibraryError::conflict("Library path is already bound to another asset")
    );
    let previous: Option<String> = transaction
        .query_row(
            "SELECT document FROM local_asset_locations WHERE asset_id=?1",
            [&asset.id],
            |row| row.get(0),
        )
        .optional()?;
    let previous: Option<AssetLocation> = previous
        .map(|document| decode_document(&document))
        .transpose()?;
    if let Some(previous) = &previous
        && normalize_local_path(&previous.path) != normalize_local_path(&asset.path)
    {
        ensure!(
            previous
                .file_identity
                .as_ref()
                .zip(identity)
                .is_some_and(|(old, new)| old == new),
            LibraryError::conflict(
                "Distinct canonical paths require matching physical file identities"
            )
        );
    }
    let identity = identity.cloned().or_else(|| {
        previous
            .as_ref()
            .filter(|old| normalize_local_path(&old.path) == normalize_local_path(&asset.path))
            .and_then(|old| old.file_identity.clone())
    });
    let same = previous.as_ref().is_some_and(|old| {
        normalize_local_path(&old.path) == normalize_local_path(&asset.path)
            && old.file_identity == identity
            && old.size_bytes == Some(asset.size_bytes)
            && old.modified_ms == asset.modified_ms
    });
    let document = encode_document(&AssetLocation {
        path: normalize_local_path(&asset.path),
        file_identity: identity.clone(),
        content_sha256: previous
            .as_ref()
            .filter(|_| same)
            .and_then(|old| old.content_sha256.clone()),
        hash_size_bytes: previous
            .as_ref()
            .filter(|_| same)
            .and_then(|old| old.hash_size_bytes),
        hash_modified_ms: previous
            .as_ref()
            .filter(|_| same)
            .and_then(|old| old.hash_modified_ms),
        size_bytes: Some(asset.size_bytes),
        modified_ms: asset.modified_ms,
    })?;
    let physical_key = identity
        .as_ref()
        .map(super::library_binding::physical_identity_key)
        .transpose()?;
    transaction.execute(
        "INSERT INTO local_asset_locations(asset_id,path_key,document,physical_identity_key) VALUES(?1,?2,?3,?4) ON CONFLICT(asset_id) DO UPDATE SET path_key=excluded.path_key,document=excluded.document,physical_identity_key=excluded.physical_identity_key,binding_revision=local_asset_locations.binding_revision+?5",
        params![asset.id, key, document, physical_key, i64::from(!same)],
    )?;
    transaction.execute(
        "DELETE FROM library_identity_reservations WHERE path_key=?1",
        [&key],
    )?;
    if !same {
        transaction.execute(
            "DELETE FROM library_relink_candidates WHERE asset_id=?1",
            [&asset.id],
        )?;
    }
    Ok(())
}

pub(super) fn check_library_storage(connection: &Connection) -> Result<()> {
    let (count, bytes): (i64, i64) = connection.query_row(
        "SELECT count(*),coalesce(sum(length(CAST(a.document AS BLOB))+length(CAST(t.document AS BLOB))),0) FROM local_assets a JOIN tracks t ON t.id=a.track_id",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    ensure!(
        count <= MAX_LIBRARY_ASSETS as i64 && bytes <= MAX_LIBRARY_METADATA_BYTES,
        LibraryError::budget("Local-library metadata exceeds its storage budget")
    );
    let members: i64 =
        connection.query_row("SELECT count(*) FROM library_membership", [], |row| {
            row.get(0)
        })?;
    ensure!(
        members <= MAX_LIBRARY_MEMBERSHIPS as i64,
        LibraryError::budget("Library membership budget exceeded")
    );
    Ok(())
}

impl Repository {
    pub fn upsert_explicit_library_tracks(
        &self,
        items: &[LibraryPreparedAsset],
        check: &dyn Fn() -> Result<()>,
    ) -> Result<RepositoryChange> {
        ensure!(
            !items.is_empty() && items.len() <= 1000,
            LibraryError::invalid("Explicit file batch must contain 1 to 1000 entries")
        );
        check()?;
        let mut encoded = Vec::with_capacity(items.len());
        let mut bytes = 0usize;
        for item in items {
            check()?;
            validate_prepared(item)?;
            let track = encode_document(&item.track)?;
            let asset = encode_document(&item.asset)?;
            bytes = bytes
                .checked_add(track.len())
                .and_then(|value| value.checked_add(asset.len()))
                .context("Explicit file batch size overflow")?;
            ensure!(
                bytes <= 32 * 1024 * 1024,
                LibraryError::budget("Explicit file batch exceeds 32 MiB")
            );
            encoded.push((track, asset));
        }
        let (_, change) = self.write_checked(None, |transaction| {
            for (item, (track, asset)) in items.iter().zip(&encoded) {
                check()?;
                tracks::write_track(transaction, &item.track, track)?;
                tracks::write_asset(transaction, &item.asset, asset)?;
                write_location(transaction, &item.asset, item.file_identity.as_ref())?;
                write_file_grant_only(transaction, &item.asset)?;
                refresh_list_metadata(transaction, &item.track, check)?;
            }
            check_library_storage(transaction)?;
            Ok(((), ChangeKind::LocalAssetsUpserted, None, serde_json::json!({"count":items.len(),"track_ids":items.iter().map(|item|&item.track.id).collect::<Vec<_>>()})))
        }, check)?;
        Ok(change)
    }

    pub fn add_library_root(
        &self,
        draft: &LibraryRootDraft,
    ) -> Result<(LibraryRoot, RepositoryChange)> {
        draft.validate()?;
        let key = library_path_key(&draft.path, draft.path_policy)?;
        self.write(None, |transaction| {
            let existing: Option<(bool, String)> = transaction
                .query_row(
                    "SELECT active,document FROM library_roots WHERE path_key=?1",
                    [&key],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?;
            let (root_id, grant_epoch, generation, last_complete_generation, watch_enabled) = if let Some((active, document)) = existing {
                let previous: LibraryRoot = decode_document(&document)?;
                previous.validate()?;
                ensure!(normalize_local_path(&previous.path) == normalize_local_path(&draft.path)
                    || previous.anchor == draft.anchor,
                    LibraryError::conflict("Distinct canonical roots share a library path identity"));
                let same = active
                    && previous.anchor == draft.anchor
                    && previous.recursive == draft.recursive
                    && previous.path_policy == draft.path_policy;
                if !same {
                    cancel_root_runs(transaction, &previous.root_id, "Library root grant changed")?;
                    super::library_binding::disable_root_watch(transaction,&previous.root_id)?;
                    transaction.execute("DELETE FROM library_membership WHERE root_id=?1", [&previous.root_id])?;
                }
                (
                    previous.root_id,
                    if same { previous.grant_epoch } else { next_number(previous.grant_epoch)? },
                    previous.generation,
                    if same { previous.last_complete_generation } else { 0 },
                    same && previous.watch_enabled,
                )
            } else {
                let proposed = stable_id("library-root", key.as_bytes());
                let occupied:bool=transaction.query_row("SELECT EXISTS(SELECT 1 FROM library_roots WHERE root_id=?1)",[&proposed],|row|row.get(0))?;
                let root_id=if occupied { super::library_binding::fresh_root_id()? } else { proposed };
                (root_id, 1, 0, 0, false)
            };
            let active_roots: i64 = transaction.query_row(
                "SELECT count(*) FROM library_roots WHERE active=1 AND root_id!=?1",
                [&root_id],
                |row| row.get(0),
            )?;
            ensure!(active_roots < MAX_LIBRARY_ROOTS as i64, LibraryError::budget("Library root budget exceeded"));
            let root = LibraryRoot {
                root_id,
                path: normalize_local_path(&draft.path),
                recursive: draft.recursive,
                grant_epoch,
                anchor: draft.anchor.clone(),
                path_policy: draft.path_policy,
                watch_enabled,
                generation,
                last_complete_generation,
            };
            root.validate()?;
            transaction.execute(
                "INSERT INTO library_roots(root_id,path_key,active,grant_epoch,generation,document) VALUES(?1,?2,1,?3,?4,?5) ON CONFLICT(root_id) DO UPDATE SET path_key=excluded.path_key,active=1,grant_epoch=excluded.grant_epoch,generation=excluded.generation,document=excluded.document",
                params![root.root_id, key, root.grant_epoch as i64, root.generation as i64, encode_document(&root)?],
            )?;
            super::library_binding::ensure_root_work_rows(transaction,&root)?;
            let bytes: i64 = transaction.query_row("SELECT coalesce(sum(length(CAST(document AS BLOB))),0) FROM library_roots", [], |row| row.get(0))?;
            ensure!(bytes <= MAX_ROOT_DOCUMENT_BYTES, LibraryError::budget("Library root metadata budget exceeded"));
            Ok((root.clone(), ChangeKind::LibraryRootChanged, None, serde_json::json!({"root_id":root.root_id,"grant_epoch":root.grant_epoch,"active":true})))
        })
    }

    pub fn remove_library_root(&self, root_id: &str) -> Result<RepositoryChange> {
        validate_library_id(root_id)?;
        let (_, change) = self.write(None, |transaction| {
            let mut root = require_root(transaction, root_id)?;
            root.grant_epoch = next_number(root.grant_epoch)?;
            root.watch_enabled = false;
            cancel_root_runs(transaction, root_id, "Library root grant revoked")?;
            super::library_binding::disable_root_watch(transaction,root_id)?;
            transaction.execute("DELETE FROM library_membership WHERE root_id=?1", [root_id])?;
            transaction.execute(
                "UPDATE library_roots SET active=0,grant_epoch=?2,document=?3 WHERE root_id=?1",
                params![root_id, root.grant_epoch as i64, encode_document(&root)?],
            )?;
            Ok(((), ChangeKind::LibraryRootChanged, None, serde_json::json!({"root_id":root_id,"grant_epoch":root.grant_epoch,"active":false})))
        })?;
        Ok(change)
    }

    pub fn library_root(&self, root_id: &str) -> Result<Option<LibraryRoot>> {
        validate_library_id(root_id)?;
        let connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("Repository lock was poisoned"))?;
        schema::validate_current(&connection)?;
        root_at(&connection, root_id)
    }

    pub fn library_roots_page(&self, offset: usize, limit: usize) -> Result<LibraryRootPage> {
        validate_page(offset, limit)?;
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
        let revision = current_revision(&connection)?;
        let mut statement = connection.prepare(
            "SELECT root_id FROM library_roots WHERE active=1 ORDER BY root_id LIMIT ?1 OFFSET ?2",
        )?;
        let mut items = Vec::new();
        let mut bytes = PAGE_ENVELOPE_BYTES;
        for row in statement.query_map(params![limit as i64, offset as i64], |row| {
            row.get::<_, String>(0)
        })? {
            let root = require_root(&connection, &row?)?;
            let length = serde_json::to_vec(&root)?.len() + 1;
            if bytes + length > MAX_LIBRARY_PAGE_BYTES {
                break;
            }
            bytes += length;
            items.push(root);
        }
        Ok(LibraryRootPage {
            items,
            total: total as usize,
            offset,
            limit,
            revision,
        })
    }

    pub fn local_asset_authority(&self, asset_id: &str) -> Result<Option<LocalAssetAuthority>> {
        validate_library_id(asset_id)?;
        let connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("Repository lock was poisoned"))?;
        schema::validate_current(&connection)?;
        authority_at(&connection, asset_id)
    }

    pub fn local_asset_file_identity(&self, asset_id: &str) -> Result<Option<FileIdentity>> {
        validate_library_id(asset_id)?;
        let connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("Repository lock was poisoned"))?;
        schema::validate_current(&connection)?;
        let document: Option<String> = connection
            .query_row(
                "SELECT document FROM local_asset_locations WHERE asset_id=?1",
                [asset_id],
                |row| row.get(0),
            )
            .optional()?;
        let location: Option<AssetLocation> = document
            .map(|document| decode_document(&document))
            .transpose()?;
        let identity = location.and_then(|location| location.file_identity);
        if let Some(identity) = &identity {
            identity.validate()?;
        }
        Ok(identity)
    }

    pub fn library_asset_at_path(&self, path: &Path) -> Result<Option<LocalAsset>> {
        let key = library_path_key(path, native_library_path_policy())?;
        let connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("Repository lock was poisoned"))?;
        schema::validate_current(&connection)?;
        let asset_id: Option<String> = connection
            .query_row(
                "SELECT asset_id FROM local_asset_locations WHERE path_key=?1",
                [&key],
                |row| row.get(0),
            )
            .optional()?;
        asset_id
            .map(|asset_id| asset_at(&connection, &asset_id))
            .transpose()
            .map(Option::flatten)
    }

    pub fn library_assets_page(
        &self,
        root_id: Option<&str>,
        after: Option<&str>,
        limit: usize,
        expected_revision: Option<u64>,
    ) -> Result<LibraryAssetPage> {
        validate_page(0, limit)?;
        if let Some(value) = root_id {
            validate_library_id(value)?;
        }
        if let Some(value) = after {
            validate_library_id(value)?;
        }
        let connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("Repository lock was poisoned"))?;
        schema::validate_current(&connection)?;
        let revision = current_revision(&connection)?;
        ensure!(
            expected_revision.is_none_or(|value| value == revision),
            LibraryError::conflict("Library changed while being paged")
        );
        if let Some(root_id) = root_id {
            require_root(&connection, root_id)?;
        }
        let filter = "(?1 IS NULL OR EXISTS(SELECT 1 FROM library_membership m WHERE m.asset_id=a.identity_key AND m.root_id=?1))";
        let total: i64 = connection.query_row(
            &format!("SELECT count(*) FROM local_assets a WHERE {filter}"),
            [root_id],
            |row| row.get(0),
        )?;
        let sql = format!(
            "SELECT a.identity_key FROM local_assets a WHERE {filter} AND (?2 IS NULL OR a.identity_key>?2) ORDER BY a.identity_key LIMIT ?3"
        );
        let mut statement = connection.prepare(&sql)?;
        let mut items = Vec::new();
        let mut bytes = PAGE_ENVELOPE_BYTES;
        let mut more = false;
        for row in statement.query_map(params![root_id, after, limit as i64 + 1], |row| {
            row.get::<_, String>(0)
        })? {
            if items.len() >= limit {
                more = true;
                break;
            }
            let asset = asset_at(&connection, &row?)?
                .ok_or_else(|| LibraryError::conflict("Library page asset disappeared"))?;
            let authority = authority_at(&connection, &asset.id)?;
            let item = LibraryAssetView {
                explicit_file_grant: authority
                    .as_ref()
                    .is_some_and(|value| value.explicit_file_epoch.is_some()),
                root_ids: authority.as_ref().map_or_else(Vec::new, |value| {
                    value
                        .roots
                        .iter()
                        .map(|root| root.root_id.clone())
                        .collect()
                }),
                authorized: authority.is_some(),
                asset,
            };
            let length = serde_json::to_vec(&item)?.len() + 1;
            ensure!(
                length + PAGE_ENVELOPE_BYTES <= MAX_LIBRARY_PAGE_BYTES,
                LibraryError::budget("Library asset exceeds page byte budget")
            );
            if bytes + length > MAX_LIBRARY_PAGE_BYTES {
                more = true;
                break;
            }
            bytes += length;
            items.push(item);
        }
        let next_asset_id = if more {
            items.last().map(|item| item.asset.id.clone())
        } else {
            None
        };
        Ok(LibraryAssetPage {
            items,
            total: total as usize,
            next_asset_id,
            revision,
        })
    }

    pub fn library_summary(&self) -> Result<LibrarySummary> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("Repository lock was poisoned"))?;
        schema::validate_current(&connection)?;
        Ok(LibrarySummary {
            roots: connection.query_row("SELECT count(*) FROM library_roots WHERE active=1", [], |row| row.get::<_, i64>(0))? as usize,
            authorized_assets: connection.query_row("SELECT count(*) FROM local_assets a WHERE EXISTS(SELECT 1 FROM local_file_grants f JOIN local_asset_locations l ON l.asset_id=f.asset_id AND l.path_key=f.grant_path_key WHERE f.asset_id=a.identity_key AND ((json_extract(f.document,'$.path')=json_extract(l.document,'$.path') AND json_extract(f.document,'$.file_identity') IS NULL) OR json_extract(f.document,'$.file_identity')=json_extract(l.document,'$.file_identity'))) OR EXISTS(SELECT 1 FROM library_membership m JOIN library_roots r ON r.root_id=m.root_id AND r.active=1 WHERE m.asset_id=a.identity_key)", [], |row| row.get::<_, i64>(0))? as usize,
            running_operations: connection.query_row("SELECT count(*) FROM library_scan_runs WHERE state='running'", [], |row| row.get::<_, i64>(0))? as usize,
            latest_operation_id: connection.query_row("SELECT operation_id FROM library_scan_runs ORDER BY rowid DESC LIMIT 1", [], |row| row.get(0)).optional()?,
            revision: current_revision(&connection)?,
        })
    }

    /// Counts effective directory grants supplied by the filesystem owner.
    /// Verification is read-only: stale epochs and removed roots never match.
    pub fn library_summary_with_verified_roots(
        &self,
        roots: &[(String, u64)],
    ) -> Result<LibrarySummary> {
        ensure!(
            roots.len() <= MAX_LIBRARY_ROOTS,
            LibraryError::budget("Too many verified library roots")
        );
        for (root_id, epoch) in roots {
            validate_library_id(root_id)?;
            ensure!(
                *epoch > 0 && *epoch <= i64::MAX as u64,
                LibraryError::invalid("Invalid verified library root epoch")
            );
        }
        let verified = serde_json::to_string(roots)?;
        ensure!(
            verified.len() <= MAX_VERIFIED_ROOT_PARAMETER_BYTES,
            LibraryError::budget("Verified library root parameter exceeds 256 KiB")
        );
        let connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("Repository lock was poisoned"))?;
        schema::validate_current(&connection)?;
        let authorized_assets: i64 = connection.query_row(
            "WITH verified AS MATERIALIZED (SELECT json_extract(value,'$[0]') AS root_id,json_extract(value,'$[1]') AS epoch FROM json_each(?1)), authorized AS (SELECT f.asset_id FROM local_file_grants f JOIN local_asset_locations l ON l.asset_id=f.asset_id AND l.path_key=f.grant_path_key WHERE (json_extract(f.document,'$.path')=json_extract(l.document,'$.path') AND json_extract(f.document,'$.file_identity') IS NULL) OR json_extract(f.document,'$.file_identity')=json_extract(l.document,'$.file_identity') UNION SELECT m.asset_id FROM verified v JOIN library_roots r ON r.root_id=v.root_id AND r.grant_epoch=v.epoch AND r.active=1 JOIN library_membership m ON m.root_id=r.root_id) SELECT count(*) FROM authorized e JOIN local_assets a ON a.identity_key=e.asset_id",
            [&verified], |row| row.get(0),
        )?;
        Ok(LibrarySummary {
            roots: connection.query_row(
                "SELECT count(*) FROM library_roots WHERE active=1",
                [],
                |row| row.get::<_, i64>(0),
            )? as usize,
            authorized_assets: authorized_assets as usize,
            running_operations: connection.query_row(
                "SELECT count(*) FROM library_scan_runs WHERE state='running'",
                [],
                |row| row.get::<_, i64>(0),
            )? as usize,
            latest_operation_id: connection
                .query_row(
                    "SELECT operation_id FROM library_scan_runs ORDER BY rowid DESC LIMIT 1",
                    [],
                    |row| row.get(0),
                )
                .optional()?,
            revision: current_revision(&connection)?,
        })
    }

    pub fn begin_library_scan(
        &self,
        root_id: &str,
        operation_id: &str,
    ) -> Result<(LibraryScanToken, RepositoryChange)> {
        self.begin_library_scan_scoped(
            root_id,
            operation_id,
            &crate::domain::library_watch::LibraryScanScope::Root,
        )
    }

    pub fn begin_library_scan_scoped(
        &self,
        root_id: &str,
        operation_id: &str,
        scope: &crate::domain::library_watch::LibraryScanScope,
    ) -> Result<(LibraryScanToken, RepositoryChange)> {
        validate_library_id(root_id)?;
        validate_library_id(operation_id)?;
        scope.validate()?;
        self.write(None, |transaction| {
            let mut root = require_root(transaction, root_id)?;
            let busy: bool = transaction.query_row("SELECT EXISTS(SELECT 1 FROM library_scan_runs WHERE root_id=?1 AND state='running')", [root_id], |row| row.get(0))?;
            ensure!(!busy, LibraryError::new(LibraryErrorKind::Busy, "Library root is already being scanned"));
            prune_operations(transaction)?;
            root.generation = next_number(root.generation)?;
            let token = LibraryScanToken { root_id: root_id.into(), grant_epoch: root.grant_epoch, generation: root.generation, operation_id: operation_id.into() };
            let mut view = LibraryOperationView::running(&token);
            view.repository_revision = next_revision(transaction)?;
            transaction.execute("UPDATE library_roots SET generation=?2,document=?3 WHERE root_id=?1", params![root_id, root.generation as i64, encode_document(&root)?])?;
            transaction.execute("INSERT INTO library_scan_runs(root_id,generation,operation_id,grant_epoch,state,document,scope_document) VALUES(?1,?2,?3,?4,'running',?5,?6)", params![root_id, token.generation as i64, operation_id, token.grant_epoch as i64, encode_document(&view)?,encode_document(scope)?])?;
            trim_operation_storage(transaction, Some(operation_id))?;
            Ok((token.clone(), ChangeKind::LibraryScanStarted, None, serde_json::json!({"root_id":root_id,"operation_id":operation_id,"generation":token.generation})))
        })
    }

    pub fn apply_library_scan_batch(
        &self,
        token: &LibraryScanToken,
        seen: &[LibrarySeenFile],
        items: &[LibraryPreparedAsset],
        check: &dyn Fn() -> Result<()>,
    ) -> Result<LibraryBatchCommit> {
        token.validate()?;
        validate_batch(seen, items)?;
        check()?;
        let (changed_asset_ids, change) = self.write_checked(None, |transaction| {
            check()?;
            let (root, mut view) = require_token(transaction, token)?;
            let scope = super::library_binding::scope_at(transaction, token)?;
            for file in seen {
                check()?;
                ensure!(path_in_root(&file.path, &root), LibraryError::invalid("Discovered file is outside the selected root"));
                ensure!(scope.contains(&root,&file.path),LibraryError::invalid("Discovered file is outside scan scope"));
                let path_key = library_path_key(&file.path, native_library_path_policy())?;
                let resolved_asset_id: Option<String> = if let Some(asset_id) = &file.asset_id {
                    Some(asset_id.clone())
                } else {
                    transaction.query_row("SELECT asset_id FROM local_asset_locations WHERE path_key=?1", [&path_key], |row| row.get(0)).optional()?
                };
                let registered = if let Some(asset_id) = &resolved_asset_id {
                    if let Some(existing) = asset_at(transaction, asset_id)? {
                        ensure!(library_path_key(&existing.path, native_library_path_policy())? == path_key, LibraryError::conflict("Seen asset path changed"));
                        transaction.execute("INSERT INTO library_membership(root_id,asset_id,last_seen_generation) VALUES(?1,?2,?3) ON CONFLICT(root_id,asset_id) DO UPDATE SET last_seen_generation=excluded.last_seen_generation", params![root.root_id, asset_id, token.generation as i64])?;
                        Some(asset_id.as_str())
                    } else {
                        ensure!(items.iter().any(|item| &item.asset.id == asset_id && library_path_key(&item.asset.path, native_library_path_policy()).ok().as_deref() == Some(&path_key)), LibraryError::conflict("Seen asset is not registered or prepared"));
                        None
                    }
                } else { None };
                transaction.execute("INSERT INTO library_scan_seen(root_id,generation,path_key,path,asset_id) VALUES(?1,?2,?3,?4,?5) ON CONFLICT(root_id,generation,path_key) DO UPDATE SET asset_id=coalesce(excluded.asset_id,library_scan_seen.asset_id)", params![token.root_id, token.generation as i64, path_key, file.path.to_str(), registered])?;
            }
            let mut changed = Vec::new();
            for item in items {
                check()?;
                ensure!(path_in_root(&item.asset.path, &root), LibraryError::invalid("Prepared file is outside the selected root"));
                ensure!(scope.contains(&root,&item.asset.path),LibraryError::invalid("Prepared file is outside scan scope"));
                let mut stored_track = item.track.clone();
                let previous: Option<(String, String)> = transaction.query_row("SELECT t.document,a.document FROM local_assets a JOIN tracks t ON t.id=a.track_id WHERE a.identity_key=?1", [&item.asset.id], |row| Ok((row.get(0)?, row.get(1)?))).optional()?;
                if let Some((document, _)) = &previous {
                    let previous_track: crate::domain::music::TrackRecord = decode_document(document)?;
                    ensure!(previous_track.id == item.track.id, LibraryError::conflict("Library scan cannot rebind an existing asset identity"));
                    stored_track.lx_id = previous_track.lx_id;
                    stored_track.wire = previous_track.wire;
                    let mut metadata = previous_track.metadata;
                    metadata.extend(stored_track.metadata);
                    stored_track.metadata = metadata;
                }
                let track_document = encode_document(&stored_track)?;
                let asset_document = encode_document(&item.asset)?;
                let same = previous.as_ref().is_some_and(|(track, asset)| track == &track_document && asset == &asset_document);
                tracks::write_track(transaction, &stored_track, &track_document)?;
                tracks::write_asset(transaction, &item.asset, &asset_document)?;
                write_location(transaction, &item.asset, item.file_identity.as_ref())?;
                transaction.execute("INSERT INTO library_membership(root_id,asset_id,last_seen_generation) VALUES(?1,?2,?3) ON CONFLICT(root_id,asset_id) DO UPDATE SET last_seen_generation=excluded.last_seen_generation", params![root.root_id, item.asset.id, token.generation as i64])?;
                let path_key = library_path_key(&item.asset.path, native_library_path_policy())?;
                transaction.execute("INSERT INTO library_scan_seen(root_id,generation,path_key,path,asset_id) VALUES(?1,?2,?3,?4,?5) ON CONFLICT(root_id,generation,path_key) DO UPDATE SET asset_id=excluded.asset_id", params![token.root_id, token.generation as i64, path_key, item.asset.path.to_str(), item.asset.id])?;
                if !same {
                    changed.push(item.asset.id.clone());
                    refresh_list_metadata(transaction, &stored_track, check)?;
                }
            }
            view.phase = LibraryPhase::Committing;
            view.committed_files = view.committed_files.checked_add(items.len()).context("Library commit counter overflow")?;
            view.changed_files = view.changed_files.checked_add(changed.len()).context("Library change counter overflow")?;
            view.repository_revision = next_revision(transaction)?;
            view.validate()?;
            update_run(transaction, &view)?;
            let (seen_count, seen_bytes): (i64,i64) = transaction.query_row("SELECT count(*),coalesce(sum(length(CAST(path AS BLOB))+length(path_key)+coalesce(length(asset_id),0)),0) FROM library_scan_seen WHERE root_id=?1 AND generation=?2", params![token.root_id, token.generation as i64], |row| Ok((row.get(0)?,row.get(1)?)))?;
            ensure!(seen_count <= MAX_LIBRARY_ASSETS as i64 && seen_bytes <= MAX_SCAN_SEEN_BYTES, LibraryError::budget("Library scan discovery storage budget exceeded"));
            check_library_storage(transaction)?;
            trim_operation_storage(transaction, Some(&token.operation_id))?;
            require_token(transaction, token)?;
            check()?;
            Ok((changed.clone(), ChangeKind::LibraryScanBatchCommitted, None, serde_json::json!({"root_id":token.root_id,"operation_id":token.operation_id,"count":items.len(),"changed_asset_ids":changed})))
        }, check)?;
        Ok(LibraryBatchCommit {
            change,
            changed_asset_ids,
        })
    }

    pub fn finish_library_scan(
        &self,
        token: &LibraryScanToken,
        outcome: &LibraryOperationView,
        discovery_complete: bool,
        check: &dyn Fn() -> Result<()>,
    ) -> Result<LibraryScanFinish> {
        token.validate()?;
        outcome.validate()?;
        ensure!(
            outcome.operation_id == token.operation_id
                && outcome.root_id.as_deref() == Some(&token.root_id)
                && outcome.generation == Some(token.generation),
            LibraryError::invalid("Library outcome does not match its scan token")
        );
        ensure!(
            outcome.state != LibraryOperationState::Running,
            LibraryError::invalid("Library scan has no terminal outcome")
        );
        let finalize = discovery_complete
            && matches!(
                outcome.state,
                LibraryOperationState::Completed | LibraryOperationState::Partial
            );
        if finalize {
            check()?;
        }
        let final_check = || if finalize { check() } else { Ok(()) };
        let (missing_asset_ids, change) = self.write_checked(None, |transaction| {
            let (mut root, previous) = require_token(transaction, token)?;
            let scope = super::library_binding::scope_at(transaction, token)?;
            let mut view = outcome.clone();
            view.committed_files = previous.committed_files;
            view.changed_files = previous.changed_files;
            view.missing_files = 0;
            view.phase = LibraryPhase::Finalizing;
            if view.state == LibraryOperationState::Completed && !discovery_complete { view.state = LibraryOperationState::Partial; }
            let mut missing = Vec::new();
            if finalize {
                check()?;
                let candidate_ids = {
                    let mut statement = transaction.prepare("SELECT m.asset_id FROM library_membership m JOIN local_asset_locations l ON l.asset_id=m.asset_id WHERE m.root_id=?1 AND NOT EXISTS(SELECT 1 FROM library_scan_seen s WHERE s.root_id=?1 AND s.generation=?2 AND (s.asset_id=m.asset_id OR s.path_key=l.path_key)) ORDER BY m.asset_id")?;
                    statement.query_map(params![token.root_id, token.generation as i64], |row| row.get::<_,String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?
                };
                ensure!(candidate_ids.len() <= MAX_LIBRARY_ASSETS, LibraryError::budget("Library missing-file budget exceeded"));
                for asset_id in candidate_ids {
                    check()?;
                    let mut asset = asset_at(transaction, &asset_id)?.ok_or_else(|| LibraryError::conflict("Library member disappeared during finalization"))?;
                    if !scope.contains(&root,&asset.path) { continue; }
                    if asset.available {
                        asset.available = false;
                        transaction.execute("UPDATE local_assets SET document=?2 WHERE identity_key=?1", params![asset.id, encode_document(&asset)?])?;
                        missing.push(asset_id);
                    }
                }
                if scope == crate::domain::library_watch::LibraryScanScope::Root { root.last_complete_generation = token.generation; }
                transaction.execute("UPDATE library_roots SET document=?2 WHERE root_id=?1", params![root.root_id, encode_document(&root)?])?;
                require_token(transaction, token)?;
                check()?;
            }
            view.missing_files = missing.len();
            view.repository_revision = next_revision(transaction)?;
            view.validate()?;
            update_run(transaction, &view)?;
            transaction.execute("DELETE FROM library_scan_seen WHERE root_id=?1 AND generation=?2", params![token.root_id, token.generation as i64])?;
            trim_operation_storage(transaction, Some(&token.operation_id))?;
            if finalize { check()?; }
            Ok((missing.clone(), ChangeKind::LibraryScanFinished, None, serde_json::json!({"root_id":token.root_id,"operation_id":token.operation_id,"state":view.state,"committed_files":view.committed_files,"missing_asset_ids":missing})))
        }, &final_check)?;
        Ok(LibraryScanFinish {
            change,
            missing_asset_ids,
        })
    }

    pub fn library_operation(&self, operation_id: &str) -> Result<Option<LibraryOperationView>> {
        validate_library_id(operation_id)?;
        let connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("Repository lock was poisoned"))?;
        schema::validate_current(&connection)?;
        operation_at(&connection, operation_id)
    }

    pub fn library_operations_page(
        &self,
        offset: usize,
        limit: usize,
    ) -> Result<LibraryOperationPage> {
        validate_page(offset, limit)?;
        let connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("Repository lock was poisoned"))?;
        schema::validate_current(&connection)?;
        let total: i64 =
            connection.query_row("SELECT count(*) FROM library_scan_runs", [], |row| {
                row.get(0)
            })?;
        let revision = current_revision(&connection)?;
        let mut statement = connection.prepare(
            "SELECT operation_id FROM library_scan_runs ORDER BY rowid DESC LIMIT ?1 OFFSET ?2",
        )?;
        let mut items = Vec::new();
        let mut bytes = PAGE_ENVELOPE_BYTES;
        for row in statement.query_map(params![limit as i64, offset as i64], |row| {
            row.get::<_, String>(0)
        })? {
            let view = operation_at(&connection, &row?)?.ok_or_else(|| {
                LibraryError::conflict("Library operation disappeared while being paged")
            })?;
            let length = serde_json::to_vec(&view)?.len() + 1;
            if bytes + length > MAX_LIBRARY_PAGE_BYTES {
                break;
            }
            bytes += length;
            items.push(view);
        }
        Ok(LibraryOperationPage {
            items,
            total: total as usize,
            offset,
            limit,
            revision,
        })
    }

    pub(super) fn recover_library_scans(&self) -> Result<()> {
        let running = {
            let connection = self
                .connection
                .lock()
                .map_err(|_| anyhow::anyhow!("Repository lock was poisoned"))?;
            connection.query_row(
                "SELECT EXISTS(SELECT 1 FROM library_scan_runs WHERE state='running')",
                [],
                |row| row.get::<_, bool>(0),
            )?
        };
        if !running {
            return Ok(());
        }
        self.write(None, |transaction| {
            let views = {
                let mut statement = transaction
                    .prepare("SELECT document FROM library_scan_runs WHERE state='running'")?;
                statement
                    .query_map([], |row| row.get::<_, String>(0))?
                    .map(|row| decode_document::<LibraryOperationView>(&row?))
                    .collect::<Result<Vec<_>>>()?
            };
            let count = views.len();
            for mut view in views {
                view.state = LibraryOperationState::Interrupted;
                view.error = Some(LibraryError::new(
                    LibraryErrorKind::Unavailable,
                    "Library scan was interrupted before completion",
                ));
                view.repository_revision = next_revision(transaction)?;
                update_run(transaction, &view)?;
            }
            transaction.execute("DELETE FROM library_scan_seen", [])?;
            trim_operation_storage(transaction, None)?;
            Ok((
                (),
                ChangeKind::LibraryScansInterrupted,
                None,
                serde_json::json!({"count":count}),
            ))
        })?;
        Ok(())
    }
}

fn validate_batch(seen: &[LibrarySeenFile], items: &[LibraryPreparedAsset]) -> Result<()> {
    ensure!(
        (!seen.is_empty() || !items.is_empty())
            && seen.len() <= MAX_LIBRARY_BATCH_ITEMS
            && items.len() <= MAX_LIBRARY_BATCH_ITEMS,
        LibraryError::invalid("Library batch must contain bounded entries")
    );
    let mut bytes = 0usize;
    let mut identities = HashSet::new();
    for file in seen {
        validate_library_path(&file.path)?;
        if let Some(id) = &file.asset_id {
            validate_library_id(id)?;
        }
        bytes = bytes
            .checked_add(serde_json::to_vec(file)?.len())
            .context("Library batch size overflow")?;
    }
    for item in items {
        validate_prepared(item)?;
        ensure!(
            identities.insert(&item.asset.id),
            LibraryError::invalid("Library batch repeats an asset")
        );
        bytes = bytes
            .checked_add(
                serde_json::to_vec(&(&item.track, &item.asset, &item.file_identity))?.len(),
            )
            .context("Library batch size overflow")?;
    }
    ensure!(
        bytes <= MAX_LIBRARY_BATCH_BYTES,
        LibraryError::budget("Library batch exceeds 1 MiB")
    );
    Ok(())
}

fn validate_prepared(item: &LibraryPreparedAsset) -> Result<()> {
    tracks::validate_track(&item.track)?;
    tracks::validate_asset(&item.asset)?;
    validate_library_path(&item.asset.path)?;
    validate_library_id(&item.asset.id)?;
    if let Some(identity) = &item.file_identity {
        identity.validate()?;
    }
    ensure!(
        item.track.platform.source == "local"
            && item.asset.track_id == item.track.id
            && item.track.local_asset_id.as_deref() == Some(item.asset.id.as_str())
            && item.asset.available,
        LibraryError::invalid("Prepared library asset has inconsistent identities")
    );
    Ok(())
}

pub(super) fn root_at(connection: &Connection, root_id: &str) -> Result<Option<LibraryRoot>> {
    let row: Option<(i64, i64, String)> = connection
        .query_row(
            "SELECT grant_epoch,generation,document FROM library_roots WHERE root_id=?1 AND active=1",
            [root_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    row.map(|(epoch, generation, document)| {
        let root: LibraryRoot = decode_document(&document)?;
        root.validate()?;
        ensure!(
            root.root_id == root_id
                && root.grant_epoch == epoch as u64
                && root.generation == generation as u64,
            LibraryError::invalid("Stored library root token is inconsistent")
        );
        Ok(root)
    })
    .transpose()
}
pub(super) fn require_root(connection: &Connection, root_id: &str) -> Result<LibraryRoot> {
    root_at(connection, root_id)?.ok_or_else(|| {
        LibraryError::new(
            LibraryErrorKind::Unavailable,
            "Library root grant is unavailable",
        )
        .into()
    })
}
pub(super) fn asset_at(connection: &Connection, asset_id: &str) -> Result<Option<LocalAsset>> {
    let row: Option<(String, String)> = connection
        .query_row(
            "SELECT track_id,document FROM local_assets WHERE identity_key=?1",
            [asset_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    row.map(|(track_id, document)| {
        let asset: LocalAsset = decode_document(&document)?;
        validate_library_id(&asset.id)?;
        validate_library_path(&asset.path)?;
        ensure!(
            asset.id == asset_id && asset.track_id == track_id,
            LibraryError::invalid("Stored local asset identity is inconsistent")
        );
        Ok(asset)
    })
    .transpose()
}
fn operation_at(
    connection: &Connection,
    operation_id: &str,
) -> Result<Option<LibraryOperationView>> {
    let row: Option<(String, i64, String, String)> = connection
        .query_row(
            "SELECT root_id,generation,state,document FROM library_scan_runs WHERE operation_id=?1",
            [operation_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .optional()?;
    row.map(|(root_id, generation, state, document)| {
        let view: LibraryOperationView = decode_document(&document)?;
        view.validate()?;
        ensure!(
            view.operation_id == operation_id
                && view.root_id.as_deref() == Some(root_id.as_str())
                && view.generation == Some(generation as u64)
                && serde_json::to_value(view.state)?.as_str() == Some(state.as_str()),
            LibraryError::invalid("Stored library operation token is inconsistent")
        );
        Ok(view)
    })
    .transpose()
}
fn authority_at(connection: &Connection, asset_id: &str) -> Result<Option<LocalAssetAuthority>> {
    let Some(asset) = asset_at(connection, asset_id)? else {
        return Ok(None);
    };
    let explicit_file_epoch = super::library_binding::exact_file_epoch(connection, asset_id)?;
    let roots = {
        let mut statement = connection.prepare("SELECT r.document FROM library_membership m JOIN library_roots r ON r.root_id=m.root_id AND r.active=1 WHERE m.asset_id=?1 ORDER BY r.root_id")?;
        statement
            .query_map([asset_id], |row| row.get::<_, String>(0))?
            .map(|row| decode_document::<LibraryRoot>(&row?))
            .collect::<Result<Vec<_>>>()?
    };
    for root in &roots {
        root.validate()?;
        ensure!(
            root_at(connection, &root.root_id)?.as_ref() == Some(root),
            LibraryError::invalid("Stored library root grant is inconsistent")
        );
        ensure!(
            path_in_root(&asset.path, root),
            LibraryError::conflict("Library member path escaped its grant")
        );
    }
    if explicit_file_epoch.is_none() && roots.is_empty() {
        return Ok(None);
    }
    Ok(Some(LocalAssetAuthority {
        asset_id: asset.id,
        track_id: asset.track_id,
        path: asset.path,
        explicit_file_epoch,
        roots,
    }))
}
pub(super) fn require_token(
    connection: &Connection,
    token: &LibraryScanToken,
) -> Result<(LibraryRoot, LibraryOperationView)> {
    let root = require_root(connection, &token.root_id)?;
    ensure!(
        root.grant_epoch == token.grant_epoch && root.generation == token.generation,
        LibraryError::conflict("Library root scan token is stale")
    );
    let matched: bool = connection.query_row("SELECT EXISTS(SELECT 1 FROM library_scan_runs WHERE root_id=?1 AND generation=?2 AND operation_id=?3 AND grant_epoch=?4 AND state='running')", params![token.root_id, token.generation as i64, token.operation_id, token.grant_epoch as i64], |row| row.get(0))?;
    ensure!(
        matched,
        LibraryError::conflict("Library scan is no longer running")
    );
    let view = operation_at(connection, &token.operation_id)?
        .ok_or_else(|| LibraryError::conflict("Library scan record disappeared"))?;
    Ok((root, view))
}
fn update_run(transaction: &Transaction<'_>, view: &LibraryOperationView) -> Result<()> {
    view.validate()?;
    let state = match view.state {
        LibraryOperationState::Running => "running",
        LibraryOperationState::Completed => "completed",
        LibraryOperationState::Partial => "partial",
        LibraryOperationState::Cancelled => "cancelled",
        LibraryOperationState::Failed => "failed",
        LibraryOperationState::Interrupted => "interrupted",
    };
    let updated = transaction.execute(
        "UPDATE library_scan_runs SET state=?2,document=?3 WHERE operation_id=?1",
        params![view.operation_id, state, encode_document(view)?],
    )?;
    ensure!(
        updated == 1,
        LibraryError::conflict("Library scan record disappeared")
    );
    Ok(())
}
pub(super) fn cancel_root_runs(
    transaction: &Transaction<'_>,
    root_id: &str,
    message: &str,
) -> Result<()> {
    let views = {
        let mut statement = transaction.prepare(
            "SELECT document FROM library_scan_runs WHERE root_id=?1 AND state='running'",
        )?;
        statement
            .query_map([root_id], |row| row.get::<_, String>(0))?
            .map(|row| decode_document::<LibraryOperationView>(&row?))
            .collect::<Result<Vec<_>>>()?
    };
    for mut view in views {
        view.state = LibraryOperationState::Cancelled;
        view.error = Some(LibraryError::new(LibraryErrorKind::Cancelled, message));
        view.repository_revision = next_revision(transaction)?;
        update_run(transaction, &view)?;
    }
    trim_operation_storage(transaction, None)?;
    transaction.execute("DELETE FROM library_scan_seen WHERE root_id=?1", [root_id])?;
    Ok(())
}
pub(super) fn prune_operations(transaction: &Transaction<'_>) -> Result<()> {
    let count: i64 =
        transaction.query_row("SELECT count(*) FROM library_scan_runs", [], |row| {
            row.get(0)
        })?;
    let remove = count.saturating_sub(MAX_LIBRARY_OPERATIONS as i64 - 1);
    if remove > 0 {
        transaction.execute("DELETE FROM library_scan_runs WHERE rowid IN (SELECT rowid FROM library_scan_runs WHERE state!='running' ORDER BY rowid LIMIT ?1)", [remove])?;
    }
    let count: i64 =
        transaction.query_row("SELECT count(*) FROM library_scan_runs", [], |row| {
            row.get(0)
        })?;
    ensure!(
        count < MAX_LIBRARY_OPERATIONS as i64,
        LibraryError::new(
            LibraryErrorKind::Busy,
            "Library operation retention is full"
        )
    );
    Ok(())
}
pub(super) fn trim_operation_storage(
    transaction: &Transaction<'_>,
    keep: Option<&str>,
) -> Result<()> {
    loop {
        let bytes: i64 = transaction.query_row(
            "SELECT coalesce(sum(length(CAST(document AS BLOB))),0) FROM library_scan_runs",
            [],
            |row| row.get(0),
        )?;
        if bytes <= MAX_LIBRARY_PAGE_BYTES as i64 {
            return Ok(());
        }
        let oldest: Option<i64> = transaction.query_row("SELECT rowid FROM library_scan_runs WHERE state!='running' AND (?1 IS NULL OR operation_id!=?1) ORDER BY rowid LIMIT 1", [keep], |row| row.get(0)).optional()?;
        let oldest = oldest
            .ok_or_else(|| LibraryError::budget("Running library operations exceed 8 MiB"))?;
        transaction.execute("DELETE FROM library_scan_runs WHERE rowid=?1", [oldest])?;
    }
}
pub(super) fn current_revision(connection: &Connection) -> Result<u64> {
    let revision: i64 = connection.query_row(
        "SELECT revision FROM repository_state WHERE singleton=1",
        [],
        |row| row.get(0),
    )?;
    Ok(revision as u64)
}
pub(super) fn next_revision(connection: &Connection) -> Result<u64> {
    next_number(current_revision(connection)?)
}
pub(super) fn next_number(value: u64) -> Result<u64> {
    ensure!(
        value < i64::MAX as u64,
        LibraryError::budget("Library sequence overflow")
    );
    Ok(value + 1)
}
fn validate_page(offset: usize, limit: usize) -> Result<()> {
    ensure!(
        (1..=1000).contains(&limit) && offset <= i64::MAX as usize,
        LibraryError::invalid("Invalid library page limits")
    );
    Ok(())
}
pub(super) fn path_in_root(path: &Path, root: &LibraryRoot) -> bool {
    let path = normalize_local_path(path);
    let directory = normalize_local_path(&root.path);
    let (path, directory) = match root.path_policy {
        LibraryPathPolicy::Sensitive => (path, directory),
        LibraryPathPolicy::WindowsInsensitive => {
            let (Some(path), Some(directory)) = (path.to_str(), directory.to_str()) else {
                return false;
            };
            (
                std::path::PathBuf::from(path.to_lowercase()),
                std::path::PathBuf::from(directory.to_lowercase()),
            )
        }
    };
    path != directory
        && path.starts_with(&directory)
        && (root.recursive || path.parent() == Some(directory.as_path()))
}
pub(super) fn refresh_list_metadata(
    transaction: &Transaction<'_>,
    track: &crate::domain::music::TrackRecord,
    check: &dyn Fn() -> Result<()>,
) -> Result<()> {
    let mut statement = transaction
        .prepare("SELECT list_id,music_id,document FROM music_list_items WHERE track_id=?1")?;
    for row in statement.query_map([&track.id], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
        ))
    })? {
        check()?;
        let (list_id, music_id, document) = row?;
        let mut item: crate::domain::music::TrackRecord = decode_document(&document)?;
        // Keep archival LX IDs, wire extensions and per-list identities.
        item.title.clone_from(&track.title);
        item.artists.clone_from(&track.artists);
        item.album.clone_from(&track.album);
        item.duration_ms = track.duration_ms;
        item.local_asset_id.clone_from(&track.local_asset_id);
        if item.platform.source == "local" && track.platform.source == "local" {
            item.platform.song_id.clone_from(&track.platform.song_id);
        }
        for key in ["albumName", "filePath", "songId", "ext"] {
            if let Some(value) = track.metadata.get(key) {
                item.metadata.insert(key.into(), value.clone());
            }
        }
        transaction.execute(
            "UPDATE music_list_items SET document=?3 WHERE list_id=?1 AND music_id=?2",
            params![list_id, music_id, encode_document(&item)?],
        )?;
    }
    Ok(())
}

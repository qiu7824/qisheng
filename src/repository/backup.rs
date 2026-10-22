//! Coherent backup previews and one checked SQLite import transaction.

use super::{
    ChangeKind, Repository, RepositoryChange, checked_revision, decode_document, encode_document,
    lists, schema,
};
use crate::{
    backup::{
        BackupEncoding, ConflictPolicy, ImportPlan, LxBackup, LxBackupKind, MAX_BACKUP_BYTES,
        encode_lx,
        lx::{ImportedListMetadata, ListImportDisposition, LocalReference},
        settings::{LxSettings, SettingsPreview},
    },
    domain::lists::{DEFAULT_LIST, LOVE_LIST, ListAction, ListData, ListInfo, TEMP_LIST, UserList},
};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    io::{self, Write},
};

const MAX_IMPORT_RECEIPTS: i64 = 64;
const MAX_IMPORT_RECEIPT_BYTES: i64 = 16 * 1024 * 1024;
const MAX_LIST_ARCHIVES: i64 = 1000;

#[derive(Clone, Debug)]
pub struct LxBackupPreview {
    pub expected_user_revision: u64,
    pub input_digest: String,
    pub plan: ImportPlan,
    backup: LxBackup,
    policy: ConflictPolicy,
    sealed_revision: u64,
    sealed_digest: String,
}

#[derive(Clone, Debug)]
pub struct LxBackupCommit {
    pub operation_id: String,
    pub input_digest: String,
    pub change: RepositoryChange,
    pub replayed: bool,
    pub settings: Option<SettingsPreview>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) struct StoredSettings {
    archived: Map<String, Value>,
    preferences: Map<String, Value>,
    requires_activation: Vec<String>,
}
impl From<&SettingsPreview> for StoredSettings {
    fn from(value: &SettingsPreview) -> Self {
        Self {
            archived: value.archived.clone(),
            preferences: value.preferences.clone(),
            requires_activation: value.requires_activation.clone(),
        }
    }
}
impl StoredSettings {
    pub(super) fn checked_preview(&self) -> Result<SettingsPreview> {
        let preview =
            LxSettings::from_value(&Value::Object(self.archived.clone()), false)?.preview()?;
        ensure!(
            preview.preferences == self.preferences
                && preview.requires_activation == self.requires_activation,
            "Stored settings archive differs from its safe projection"
        );
        Ok(preview)
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) struct StoredListMetadata {
    pub(super) info: ListInfo,
    pub(super) wire: Map<String, Value>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
struct StoredMetadataMapping {
    imported_id: String,
    destination_id: String,
    applied: bool,
    metadata: StoredListMetadata,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
struct StoredImport {
    operation_id: String,
    input_digest: String,
    expected_revision: u64,
    revision: u64,
    kind: String,
    migrated_legacy: bool,
    metadata: Vec<StoredMetadataMapping>,
    settings: Option<StoredSettings>,
    #[serde(default = "preferences_already_applied")]
    preferences_applied: bool,
    change: RepositoryChange,
}
fn preferences_already_applied() -> bool {
    true
}
impl StoredImport {
    fn result(&self, replayed: bool) -> Result<LxBackupCommit> {
        validate_operation(&self.operation_id)?;
        validate_digest(&self.input_digest)?;
        ensure!(
            self.revision
                == self
                    .expected_revision
                    .checked_add(1)
                    .context("Backup revision overflow")?
                && self.change.revision == self.revision
                && self.change.kind == ChangeKind::ListsChanged
                && self.change.details["operation_id"] == self.operation_id
                && self.change.details["input_digest"] == self.input_digest
                && self.change.details["origin"] == "lx_backup",
            "Backup receipt identity differs from its committed change"
        );
        Ok(LxBackupCommit {
            operation_id: self.operation_id.clone(),
            input_digest: self.input_digest.clone(),
            change: self.change.clone(),
            replayed,
            settings: self
                .settings
                .as_ref()
                .map(StoredSettings::checked_preview)
                .transpose()?,
        })
    }
}

#[derive(Debug)]
struct ReplayedImport;
impl fmt::Display for ReplayedImport {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Backup operation is already committed")
    }
}
impl std::error::Error for ReplayedImport {}

impl Repository {
    pub fn preview_lx_backup(
        &self,
        backup: &LxBackup,
        policy: ConflictPolicy,
    ) -> Result<LxBackupPreview> {
        backup.validate()?;
        let mut connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("Repository lock was poisoned"))?;
        schema::validate_current(&connection)?;
        let transaction = connection.transaction()?;
        let revision = revision_at(&transaction)?;
        let current = list_data_at(&transaction)?;
        let mut plan = backup.preview(&current, &policy)?;
        resolve_plan(&transaction, &mut plan)?;
        // Reject archives that cannot be persisted before returning a preview.
        if let Some(settings) = &plan.settings {
            encode_document(&StoredSettings::from(settings))?;
        }
        for metadata in &plan.archived_list_metadata {
            encode_document(&StoredListMetadata {
                info: metadata.info.clone(),
                wire: metadata.wire.clone(),
            })?;
        }
        let digest = fingerprint(backup, &policy, revision, &plan, &|| Ok(()))?;
        validate_archive_admission(&transaction, backup, revision, &digest, &plan)?;
        transaction.commit()?;
        Ok(LxBackupPreview {
            expected_user_revision: revision,
            input_digest: digest.clone(),
            plan,
            backup: backup.clone(),
            policy,
            sealed_revision: revision,
            sealed_digest: digest,
        })
    }

    pub fn commit_lx_backup(
        &self,
        operation_id: &str,
        preview: &LxBackupPreview,
        check: &dyn Fn() -> Result<()>,
    ) -> Result<LxBackupCommit> {
        validate_operation(operation_id)?;
        check()?;
        ensure!(
            preview.expected_user_revision == preview.sealed_revision
                && preview.input_digest == preview.sealed_digest
                && fingerprint(
                    &preview.backup,
                    &preview.policy,
                    preview.expected_user_revision,
                    &preview.plan,
                    check
                )? == preview.sealed_digest,
            "Backup preview was modified; create a new preview"
        );
        let mut replay = None;
        let mut stored_receipt = None;
        let result = self.write_checked(None, |transaction| {
            if let Some(receipt) = import_at(transaction, operation_id)? {
                ensure!(receipt.input_digest == preview.input_digest, "Backup operation identity was reused for different input or preview");
                replay = Some(receipt.result(true)?);
                return Err(ReplayedImport.into());
            }
            ensure!(pending_preferences_at(transaction)?.is_empty(), "Complete the pending backup preferences before importing another backup");
            ensure!(!transaction.query_row("SELECT EXISTS(SELECT 1 FROM native_backup_imports WHERE preferences_applied=0)", [], |row| row.get::<_, bool>(0))?, "Complete the pending native backup preferences before importing another backup");
            let current_revision = revision_at(transaction)?;
            ensure!(current_revision == preview.expected_user_revision, "Repository revision conflict");
            let current = list_data_at(transaction)?;
            let mut plan = preview.backup.preview(&current, &preview.policy)?;
            resolve_plan(transaction, &mut plan)?;
            ensure!(fingerprint(&preview.backup, &preview.policy, current_revision, &plan, check)? == preview.input_digest, "Backup preview bindings or metadata changed");
            lists::apply(transaction, &ListAction::DataOverwrite { data: plan.lists.clone() })?;
            let revision = current_revision.checked_add(1).context("Backup revision overflow")?;
            let mut metadata = Vec::with_capacity(plan.archived_list_metadata.len());
            let actual_infos: BTreeMap<_, _> = lists::query_infos(transaction, false)?.into_iter().map(|info| (info.id.clone(), info)).collect();
            for change in &plan.list_changes {
                let imported = plan.archived_list_metadata.iter().find(|entry| entry.info.id == change.imported_id).context("Imported list metadata is missing")?;
                let applied = change.disposition != ListImportDisposition::KeptExisting;
                metadata.push(StoredMetadataMapping { imported_id: change.imported_id.clone(), destination_id: change.destination_id.clone(), applied, metadata: StoredListMetadata { info: imported.info.clone(), wire: imported.wire.clone() } });
                if applied {
                    let actual = actual_infos.get(&change.destination_id).context("Imported destination list is missing")?;
                    let mut compatible = StoredListMetadata { info: imported.info.clone(), wire: imported.wire.clone() };
                    compatible.info.id = actual.id.clone(); compatible.info.name = actual.name.clone();
                    compatible.wire.insert("id".into(), actual.id.clone().into()); compatible.wire.insert("name".into(), actual.name.clone().into());
                    let document = encode_document(&compatible)?;
                    transaction.execute("INSERT INTO backup_list_metadata(list_id,revision,document,byte_size) VALUES(?1,?2,?3,?4) ON CONFLICT(list_id) DO UPDATE SET revision=excluded.revision,document=excluded.document,byte_size=excluded.byte_size", params![actual.id, checked_revision(revision)?, document, document.len() as i64])?;
                }
            }
            let settings = plan.settings.as_ref().map(StoredSettings::from);
            if let Some(settings) = &settings {
                let document = encode_document(settings)?;
                transaction.execute("INSERT INTO compat_settings(singleton,revision,document,byte_size) VALUES(1,?1,?2,?3) ON CONFLICT(singleton) DO UPDATE SET revision=excluded.revision,document=excluded.document,byte_size=excluded.byte_size", params![checked_revision(revision)?, document, document.len() as i64])?;
            }
            let changes: Vec<_> = plan.list_changes.iter().map(|change| serde_json::json!({"imported_id":change.imported_id,"destination_id":change.destination_id,"disposition":disposition(change.disposition)})).collect();
            let details = serde_json::json!({"origin":"lx_backup","operation_id":operation_id,"input_digest":preview.input_digest,"list_changes":changes,"settings_imported":settings.is_some()});
            let change = RepositoryChange { revision, kind: ChangeKind::ListsChanged, track_id: None, details: details.clone() };
            let preferences_applied = settings.is_none();
            let receipt = StoredImport { operation_id: operation_id.into(), input_digest: preview.input_digest.clone(), expected_revision: current_revision, revision, kind: kind(preview.backup.kind).into(), migrated_legacy: preview.backup.migrated_legacy, metadata, settings, preferences_applied, change };
            let document = encode_document(&receipt)?;
            transaction.execute("INSERT INTO backup_imports(operation_id,input_digest,expected_revision,revision,document,byte_size) VALUES(?1,?2,?3,?4,?5,?6)", params![operation_id, preview.input_digest, checked_revision(current_revision)?, checked_revision(revision)?, document, document.len() as i64])?;
            prune_receipts(transaction)?;
            check_storage(transaction)?;
            stored_receipt = Some(receipt);
            Ok(((), ChangeKind::ListsChanged, None, details))
        }, check);
        match result {
            Ok(((), change)) => {
                let receipt = stored_receipt.context("Committed backup receipt is missing")?;
                ensure!(
                    receipt.change == change,
                    "Committed backup change differs from its receipt"
                );
                receipt.result(false)
            }
            Err(error) if error.is::<ReplayedImport>() => {
                replay.context("Replayed backup receipt is missing")
            }
            Err(error) => Err(error),
        }
    }

    pub fn lx_backup_result(&self, operation_id: &str) -> Result<Option<LxBackupCommit>> {
        validate_operation(operation_id)?;
        let connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("Repository lock was poisoned"))?;
        schema::validate_current(&connection)?;
        import_at(&connection, operation_id)?
            .map(|receipt| receipt.result(true))
            .transpose()
    }
    /// Only unfinished preference applications are eligible for restart replay.
    pub fn pending_lx_backup_preferences(&self) -> Result<Vec<LxBackupCommit>> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("Repository lock was poisoned"))?;
        schema::validate_current(&connection)?;
        pending_preferences_at(&connection)?
            .into_iter()
            .map(|receipt| receipt.result(true))
            .collect()
    }
    /// Called after the player-state preferences and their marker have been saved.
    /// This updates the receipt without adding a user revision or outbox event.
    pub fn acknowledge_lx_backup_preferences(
        &self,
        operation_id: &str,
        input_digest: &str,
    ) -> Result<()> {
        validate_operation(operation_id)?;
        validate_digest(input_digest)?;
        let mut connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("Repository lock was poisoned"))?;
        schema::validate_current(&connection)?;
        let transaction =
            connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let mut receipt = import_at(&transaction, operation_id)?
            .context("Backup receipt is no longer available")?;
        ensure!(
            receipt.input_digest == input_digest,
            "Backup preference receipt identity differs"
        );
        if !receipt.preferences_applied {
            ensure!(
                receipt.settings.is_some(),
                "Pending backup receipt has no preferences"
            );
            receipt.preferences_applied = true;
            let document = encode_document(&receipt)?;
            transaction.execute(
                "UPDATE backup_imports SET document=?2,byte_size=?3 WHERE operation_id=?1",
                params![operation_id, document, document.len() as i64],
            )?;
            check_storage(&transaction)?;
        }
        transaction.commit()?;
        Ok(())
    }
    pub fn lx_compat_settings(&self) -> Result<Option<SettingsPreview>> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("Repository lock was poisoned"))?;
        schema::validate_current(&connection)?;
        let row: Option<(i64, String, i64)> = connection
            .query_row(
                "SELECT revision,document,byte_size FROM compat_settings WHERE singleton=1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        row.map(|(revision, document, bytes)| {
            ensure!(
                revision > 0 && bytes == document.len() as i64,
                "Settings archive SQL identity is invalid"
            );
            let settings: StoredSettings = decode_document(&document)?;
            settings.checked_preview()
        })
        .transpose()
    }
    pub fn lx_list_metadata(&self, list_id: &str) -> Result<Option<ImportedListMetadata>> {
        ensure!(
            !list_id.is_empty() && list_id.len() <= 1024 && !list_id.contains('\0'),
            "List archive identity is invalid"
        );
        let connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("Repository lock was poisoned"))?;
        schema::validate_current(&connection)?;
        let row: Option<(i64, String, i64)> = connection
            .query_row(
                "SELECT revision,document,byte_size FROM backup_list_metadata WHERE list_id=?1",
                [list_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        row.map(|(revision, document, bytes)| {
            ensure!(
                revision > 0 && bytes == document.len() as i64,
                "List archive SQL identity is invalid"
            );
            let value: StoredListMetadata = decode_document(&document)?;
            ensure!(
                value.info.id == list_id
                    && value.wire.get("id").and_then(Value::as_str) == Some(list_id),
                "List archive identity differs from its document"
            );
            Ok(ImportedListMetadata {
                info: value.info,
                wire: value.wire,
            })
        })
        .transpose()
    }
}

fn revision_at(connection: &Connection) -> Result<u64> {
    let revision: i64 = connection.query_row(
        "SELECT revision FROM repository_state WHERE singleton=1",
        [],
        |row| row.get(0),
    )?;
    ensure!(revision >= 0, "Repository revision is invalid");
    Ok(revision as u64)
}
pub(super) fn list_data_at(connection: &Connection) -> Result<ListData> {
    let (songs, song_bytes): (i64, i64) = connection.query_row(
        "SELECT count(*),coalesce(sum(length(CAST(document AS BLOB))),0) FROM music_list_items",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let (count, info_bytes): (i64, i64) = connection.query_row(
        "SELECT count(*),coalesce(sum(length(CAST(document AS BLOB))),0) FROM music_lists",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    ensure!(
        songs <= crate::backup::lx::MAX_IMPORT_TRACKS as i64
            && count <= MAX_LIST_ARCHIVES
            && song_bytes.saturating_add(info_bytes) <= MAX_BACKUP_BYTES as i64,
        "Current lists exceed the bounded backup preview budget"
    );
    let user_list = lists::query_infos(connection, true)?
        .into_iter()
        .map(|info| {
            Ok(UserList {
                tracks: lists::query_tracks(connection, &info.id)?,
                info,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(ListData {
        default_list: lists::query_tracks(connection, DEFAULT_LIST)?,
        love_list: lists::query_tracks(connection, LOVE_LIST)?,
        temp_list: Some(lists::query_tracks(connection, TEMP_LIST)?),
        user_list,
    })
}
pub(super) fn resolve_plan(connection: &Connection, plan: &mut ImportPlan) -> Result<()> {
    for track in plan
        .lists
        .default_list
        .iter_mut()
        .chain(&mut plan.lists.love_list)
        .chain(
            plan.lists
                .user_list
                .iter_mut()
                .flat_map(|list| &mut list.tracks),
        )
        .chain(plan.lists.temp_list.iter_mut().flatten())
    {
        *track = super::library_binding::resolve_local_list_track(connection, track)?;
    }
    let mut seen = BTreeSet::new();
    plan.local_references.clear();
    for track in plan
        .lists
        .default_list
        .iter()
        .chain(&plan.lists.love_list)
        .chain(plan.lists.user_list.iter().flat_map(|list| &list.tracks))
        .chain(plan.lists.temp_list.iter().flatten())
    {
        if track.platform.source == "local" && seen.insert(track.id.clone()) {
            plan.local_references.push(LocalReference {
                track_id: track.id.clone(),
                declared_path: track
                    .metadata
                    .get("filePath")
                    .and_then(Value::as_str)
                    .context("Imported local path is missing")?
                    .into(),
            });
        }
    }
    Ok(())
}
fn import_at(connection: &Connection, operation_id: &str) -> Result<Option<StoredImport>> {
    let row: Option<(String, i64, i64, String, i64)> = connection.query_row("SELECT input_digest,expected_revision,revision,document,byte_size FROM backup_imports WHERE operation_id=?1", [operation_id], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?))).optional()?;
    row.map(|(digest, expected, revision, document, bytes)| {
        let receipt: StoredImport = decode_document(&document)?;
        ensure!(
            receipt.operation_id == operation_id
                && receipt.input_digest == digest
                && checked_revision(receipt.expected_revision)? == expected
                && checked_revision(receipt.revision)? == revision
                && bytes == document.len() as i64,
            "Backup receipt SQL identity differs from its document"
        );
        receipt.result(true)?;
        Ok(receipt)
    })
    .transpose()
}
fn prune_receipts(connection: &Connection) -> Result<()> {
    connection.execute("DELETE FROM backup_imports WHERE operation_id IN (SELECT operation_id FROM backup_imports ORDER BY revision DESC,operation_id DESC LIMIT -1 OFFSET ?1)", [MAX_IMPORT_RECEIPTS])?;
    loop {
        let bytes: i64 = connection.query_row(
            "SELECT coalesce(sum(byte_size),0) FROM backup_imports",
            [],
            |row| row.get(0),
        )?;
        if bytes <= MAX_IMPORT_RECEIPT_BYTES {
            break;
        }
        connection.execute("DELETE FROM backup_imports WHERE operation_id=(SELECT operation_id FROM backup_imports ORDER BY revision,operation_id LIMIT 1)", [])?;
    }
    Ok(())
}
fn pending_preferences_at(connection: &Connection) -> Result<Vec<StoredImport>> {
    check_storage(connection)?;
    let mut statement = connection
        .prepare("SELECT operation_id FROM backup_imports ORDER BY revision,operation_id")?;
    let ids: Vec<String> = statement
        .query_map([], |row| row.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    let mut pending = Vec::new();
    for id in ids {
        let receipt = import_at(connection, &id)?.context("Backup receipt disappeared")?;
        if !receipt.preferences_applied {
            ensure!(
                receipt.settings.is_some(),
                "Pending backup receipt has no preferences"
            );
            pending.push(receipt);
        }
    }
    ensure!(
        pending.len() <= 1,
        "Multiple pending backup preference applications are invalid"
    );
    Ok(pending)
}
fn check_storage(connection: &Connection) -> Result<()> {
    for (table, count_limit, byte_limit) in [
        (
            "backup_imports",
            MAX_IMPORT_RECEIPTS,
            MAX_IMPORT_RECEIPT_BYTES,
        ),
        (
            "backup_list_metadata",
            MAX_LIST_ARCHIVES,
            MAX_IMPORT_RECEIPT_BYTES,
        ),
        ("compat_settings", 1, super::MAX_DOCUMENT_BYTES as i64),
    ] {
        let (count, bytes): (i64, i64) = connection.query_row(
            &format!("SELECT count(*),coalesce(sum(byte_size),0) FROM {table}"),
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        ensure!(
            count <= count_limit && bytes <= byte_limit,
            "Backup archive storage budget exceeded"
        );
    }
    Ok(())
}
fn validate_operation(operation: &str) -> Result<()> {
    ensure!(
        !operation.is_empty() && operation.len() <= 256 && !operation.chars().any(char::is_control),
        "Backup operation identity is invalid"
    );
    Ok(())
}
fn validate_digest(digest: &str) -> Result<()> {
    ensure!(
        digest.len() == 64
            && digest
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
        "Backup digest is invalid"
    );
    Ok(())
}
fn disposition(value: ListImportDisposition) -> &'static str {
    match value {
        ListImportDisposition::Added => "added",
        ListImportDisposition::ReplacedTracks => "replaced_tracks",
        ListImportDisposition::KeptExisting => "kept_existing",
        ListImportDisposition::Copied => "copied",
    }
}
fn kind(value: LxBackupKind) -> &'static str {
    match value {
        LxBackupKind::AllData => "allData_v2",
        LxBackupKind::Settings => "setting_v2",
        LxBackupKind::PlayList => "playList_v2",
        LxBackupKind::PlayListPart => "playListPart_v2",
    }
}

fn validate_archive_admission(
    connection: &Connection,
    backup: &LxBackup,
    expected_revision: u64,
    input_digest: &str,
    plan: &ImportPlan,
) -> Result<()> {
    ensure!(
        plan.lists.user_list.len() + 3 <= MAX_LIST_ARCHIVES as usize,
        "Imported lists exceed the repository preview budget"
    );
    let revision = expected_revision
        .checked_add(1)
        .context("Backup revision overflow")?;
    // A maximal escaped operation ID reserves all receipt overhead at preview.
    let operation_id = "\"".repeat(256);
    let changes: Vec<_> = plan.list_changes.iter().map(|change| serde_json::json!({"imported_id":change.imported_id,"destination_id":change.destination_id,"disposition":disposition(change.disposition)})).collect();
    let settings = plan.settings.as_ref().map(StoredSettings::from);
    let change = RepositoryChange {
        revision,
        kind: ChangeKind::ListsChanged,
        track_id: None,
        details: serde_json::json!({"origin":"lx_backup","operation_id":operation_id,"input_digest":input_digest,"list_changes":changes,"settings_imported":settings.is_some()}),
    };
    let mut metadata = Vec::new();
    let (mut count, mut bytes): (i64, i64) = connection.query_row(
        "SELECT count(*),coalesce(sum(byte_size),0) FROM backup_list_metadata",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let mut infos: BTreeMap<_, _> = lists::query_infos(connection, false)?
        .into_iter()
        .map(|info| (info.id.clone(), info))
        .collect();
    for list in &plan.lists.user_list {
        infos.insert(list.info.id.clone(), list.info.clone());
    }
    for imported_change in &plan.list_changes {
        let imported = plan
            .archived_list_metadata
            .iter()
            .find(|entry| entry.info.id == imported_change.imported_id)
            .context("Imported list metadata is missing")?;
        let applied = imported_change.disposition != ListImportDisposition::KeptExisting;
        metadata.push(StoredMetadataMapping {
            imported_id: imported_change.imported_id.clone(),
            destination_id: imported_change.destination_id.clone(),
            applied,
            metadata: StoredListMetadata {
                info: imported.info.clone(),
                wire: imported.wire.clone(),
            },
        });
        if applied {
            let info = infos
                .get(&imported_change.destination_id)
                .context("Imported destination metadata is missing")?;
            let mut mapped = StoredListMetadata {
                info: imported.info.clone(),
                wire: imported.wire.clone(),
            };
            mapped.info.id = info.id.clone();
            mapped.info.name = info.name.clone();
            mapped.wire.insert("id".into(), info.id.clone().into());
            mapped.wire.insert("name".into(), info.name.clone().into());
            let document = encode_document(&mapped)?;
            let existing: Option<i64> = connection
                .query_row(
                    "SELECT byte_size FROM backup_list_metadata WHERE list_id=?1",
                    [&info.id],
                    |row| row.get(0),
                )
                .optional()?;
            if let Some(previous) = existing {
                bytes -= previous;
            } else {
                count += 1;
            }
            bytes += document.len() as i64;
        }
    }
    ensure!(
        count <= MAX_LIST_ARCHIVES && bytes <= MAX_IMPORT_RECEIPT_BYTES,
        "Imported list compatibility archives exceed the repository budget"
    );
    let preferences_applied = settings.is_none();
    let receipt = StoredImport {
        operation_id,
        input_digest: input_digest.into(),
        expected_revision,
        revision,
        kind: kind(backup.kind).into(),
        migrated_legacy: backup.migrated_legacy,
        metadata,
        settings,
        preferences_applied,
        change,
    };
    encode_document(&receipt)?;
    Ok(())
}

#[derive(Serialize)]
struct PlanFingerprint<'a> {
    lists: &'a ListData,
    changes: Vec<ChangeFingerprint<'a>>,
    metadata: Vec<MetadataFingerprint<'a>>,
    settings: Option<SettingsFingerprint<'a>>,
    local_references: Vec<LocalFingerprint<'a>>,
}
#[derive(Serialize)]
struct ChangeFingerprint<'a> {
    imported_id: &'a str,
    destination_id: &'a str,
    disposition: &'static str,
}
#[derive(Serialize)]
struct MetadataFingerprint<'a> {
    info: &'a ListInfo,
    wire: &'a Map<String, Value>,
}
#[derive(Serialize)]
struct SettingsFingerprint<'a> {
    archived: &'a Map<String, Value>,
    preferences: &'a Map<String, Value>,
    requires_activation: &'a [String],
}
#[derive(Serialize)]
struct LocalFingerprint<'a> {
    track_id: &'a str,
    declared_path: &'a str,
}

struct HashWriter<'a> {
    hash: Sha256,
    bytes: usize,
    unchecked: usize,
    check: &'a dyn Fn() -> Result<()>,
}
impl Write for HashWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > MAX_BACKUP_BYTES.saturating_sub(self.bytes) {
            return Err(io::Error::other("Backup plan exceeds 16 MiB"));
        }
        for chunk in bytes.chunks(64 * 1024) {
            self.unchecked += chunk.len();
            if self.unchecked >= 64 * 1024 {
                (self.check)().map_err(|error| io::Error::other(error.to_string()))?;
                self.unchecked = 0;
            }
            self.hash.update(chunk);
        }
        self.bytes += bytes.len();
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
fn fingerprint(
    backup: &LxBackup,
    policy: &ConflictPolicy,
    revision: u64,
    plan: &ImportPlan,
    check: &dyn Fn() -> Result<()>,
) -> Result<String> {
    let input = encode_lx(backup, BackupEncoding::Json)?;
    let mut digest = Sha256::new();
    digest.update(b"lx-backup-preview-v1\0");
    digest.update(revision.to_le_bytes());
    digest.update((input.len() as u64).to_le_bytes());
    for chunk in input.chunks(64 * 1024) {
        check()?;
        digest.update(chunk);
    }
    let policy = match policy {
        ConflictPolicy::ReplaceTracks => serde_json::json!({"policy":"replace_tracks"}),
        ConflictPolicy::KeepExisting => serde_json::json!({"policy":"keep_existing"}),
        ConflictPolicy::Copy { new_list_id } => {
            serde_json::json!({"policy":"copy","new_list_id":new_list_id})
        }
    };
    digest.update(serde_json::to_vec(&policy)?);
    let value = PlanFingerprint {
        lists: &plan.lists,
        changes: plan
            .list_changes
            .iter()
            .map(|change| ChangeFingerprint {
                imported_id: &change.imported_id,
                destination_id: &change.destination_id,
                disposition: disposition(change.disposition),
            })
            .collect(),
        metadata: plan
            .archived_list_metadata
            .iter()
            .map(|metadata| MetadataFingerprint {
                info: &metadata.info,
                wire: &metadata.wire,
            })
            .collect(),
        settings: plan.settings.as_ref().map(|settings| SettingsFingerprint {
            archived: &settings.archived,
            preferences: &settings.preferences,
            requires_activation: &settings.requires_activation,
        }),
        local_references: plan
            .local_references
            .iter()
            .map(|reference| LocalFingerprint {
                track_id: &reference.track_id,
                declared_path: &reference.declared_path,
            })
            .collect(),
    };
    let mut writer = HashWriter {
        hash: digest,
        bytes: 0,
        unchecked: 0,
        check,
    };
    serde_json::to_writer(&mut writer, &value)?;
    check()?;
    Ok(format!("{:x}", writer.hash.finalize()))
}

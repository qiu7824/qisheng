//! Atomic logical backup import and its durable player-preference continuation.

use super::{
    ChangeKind, Repository, RepositoryChange, checked_revision, decode_document, encode_document,
    lists, schema, sync,
};
use crate::{
    backup::{
        ConflictPolicy, MAX_BACKUP_BYTES,
        lx::ListImportDisposition,
        native::{
            NativeBackup, NativeDownloadPlan, NativeEditedLyrics, NativeImportPlan,
            NativeSourceArchive,
        },
    },
    domain::{
        lists::ListAction,
        lyric::{LyricField, LyricKind},
    },
    source::import_script,
    store::StoreData,
};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    fmt,
    io::{self, Write},
};

const MAX_RECEIPTS: i64 = 64;
const MAX_ARCHIVE_BYTES: i64 = 16 * 1024 * 1024;

#[derive(Clone, Debug)]
pub struct NativeBackupPreview {
    pub expected_user_revision: u64,
    pub input_digest: String,
    pub plan: NativeImportPlan,
    backup: NativeBackup,
    policy: ConflictPolicy,
    sealed_revision: u64,
    sealed_digest: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NativeBackupCounts {
    pub lists: usize,
    pub declared_paths: usize,
    pub sources: usize,
    pub download_plans: usize,
    pub edited_lyrics: usize,
    pub applied_edited_lyrics: usize,
    pub archived_edited_lyrics: usize,
}

#[derive(Clone, Debug)]
pub struct NativeBackupCommit {
    pub operation_id: String,
    pub input_digest: String,
    pub change: RepositoryChange,
    pub replayed: bool,
    pub counts: NativeBackupCounts,
    pub preferences_applied: bool,
}

/// These records convey data only; none can be used as file or network grants.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeBackupArchive {
    pub declared_local_paths: Vec<String>,
    pub sources: Vec<NativeSourceArchive>,
    pub download_plans: Option<Vec<NativeDownloadPlan>>,
    pub edited_lyrics: Option<Vec<NativeEditedLyrics>>,
    pub extensions: Map<String, Value>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredImport {
    operation_id: String,
    input_digest: String,
    expected_revision: u64,
    revision: u64,
    preferences: StoreData,
    archive_sha256: String,
    preferences_applied: bool,
    counts: NativeBackupCounts,
    change: RepositoryChange,
}

impl StoredImport {
    fn result(&self, replayed: bool) -> Result<NativeBackupCommit> {
        validate_operation(&self.operation_id)?;
        validate_digest(&self.input_digest)?;
        validate_digest(&self.archive_sha256)?;
        validate_preferences(&self.preferences)?;
        let lyric_count = self
            .counts
            .applied_edited_lyrics
            .checked_add(self.counts.archived_edited_lyrics)
            .context("Native backup receipt lyric count overflow")?;
        ensure!(
            self.revision
                == self
                    .expected_revision
                    .checked_add(1)
                    .context("Native backup revision overflow")?
                && self.change.revision == self.revision
                && self.change.kind == ChangeKind::ListsChanged
                && self.change.track_id.is_none()
                && self.change.details["origin"] == "native_backup"
                && self.change.details["operation_id"] == self.operation_id
                && self.change.details["input_digest"] == self.input_digest
                && serde_json::from_value::<NativeBackupCounts>(
                    self.change.details["counts"].clone()
                )? == self.counts
                && self.counts.edited_lyrics == lyric_count,
            "Native backup receipt differs from its committed change"
        );
        Ok(NativeBackupCommit {
            operation_id: self.operation_id.clone(),
            input_digest: self.input_digest.clone(),
            change: self.change.clone(),
            replayed,
            counts: self.counts.clone(),
            preferences_applied: self.preferences_applied,
        })
    }
}

#[derive(Debug)]
struct ReplayedImport;
impl fmt::Display for ReplayedImport {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Native backup operation is already committed")
    }
}
impl std::error::Error for ReplayedImport {}

impl Repository {
    pub fn preview_native_backup(
        &self,
        backup: &NativeBackup,
        policy: ConflictPolicy,
    ) -> Result<NativeBackupPreview> {
        backup.validate()?;
        let mut connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("Repository lock was poisoned"))?;
        schema::validate_current(&connection)?;
        let transaction = connection.transaction()?;
        check_storage(&transaction)?;
        let revision = revision_at(&transaction)?;
        let mut plan = backup.preview(&super::backup::list_data_at(&transaction)?, &policy)?;
        super::backup::resolve_plan(&transaction, &mut plan.list_plan)?;
        let digest = fingerprint(backup, &policy, revision, &plan, &|| Ok(()))?;
        let archive = archive_from(backup);
        let document = encode_document(&archive)?;
        validate_archive(&archive)?;
        let counts = counts_at(&transaction, &plan)?;
        let receipt = receipt_for(
            &"\"".repeat(256),
            &digest,
            revision,
            &plan.store_preferences,
            &document,
            counts,
        )?;
        ensure!(
            (encode_document(&receipt)?.len() + document.len()) as i64 <= MAX_ARCHIVE_BYTES,
            "Native backup exceeds the receipt and archive budget"
        );
        transaction.commit()?;
        Ok(NativeBackupPreview {
            expected_user_revision: revision,
            input_digest: digest.clone(),
            plan,
            backup: backup.clone(),
            policy,
            sealed_revision: revision,
            sealed_digest: digest,
        })
    }

    pub fn commit_native_backup(
        &self,
        operation_id: &str,
        preview: &NativeBackupPreview,
        check: &dyn Fn() -> Result<()>,
    ) -> Result<NativeBackupCommit> {
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
            "Native backup preview was modified; create a new preview"
        );
        let mut replay = None;
        let mut committed = None;
        let result = self.write_checked(None, |transaction| {
            if let Some(receipt) = import_at(transaction, operation_id)? {
                ensure!(receipt.input_digest == preview.input_digest, "Native backup operation identity was reused for different input or preview");
                replay = Some(receipt.result(true)?);
                return Err(ReplayedImport.into());
            }
            let lx_conflict: bool = transaction.query_row("SELECT EXISTS(SELECT 1 FROM backup_imports WHERE coalesce(json_extract(document,'$.preferences_applied'),1)=0)", [], |row| row.get(0))?;
            ensure!(!lx_conflict && pending_at(transaction)?.is_empty(), "Complete the pending backup preferences before importing another backup");
            let revision = revision_at(transaction)?;
            ensure!(revision == preview.expected_user_revision, "Repository revision conflict");
            let mut plan = preview.backup.preview(&super::backup::list_data_at(transaction)?, &preview.policy)?;
            super::backup::resolve_plan(transaction, &mut plan.list_plan)?;
            ensure!(fingerprint(&preview.backup, &preview.policy, revision, &plan, check)? == preview.input_digest,
                "Native backup preview bindings changed");
            let counts = counts_at(transaction, &plan)?;
            lists::apply(transaction, &ListAction::DataOverwrite { data: plan.list_plan.lists.clone() })?;
            transaction.execute("UPDATE dislike_state SET rules=?1 WHERE singleton=1", [&plan.dislike.rules])?;
            if let Some(lyrics) = &plan.edited_lyrics {
                for lyric in lyrics {
                    check()?;
                    let exists: bool = transaction.query_row("SELECT EXISTS(SELECT 1 FROM tracks WHERE id=?1)", [&lyric.track_id], |row| row.get(0))?;
                    if exists { replace_edited_lyrics(transaction, lyric)?; }
                }
            }
            let archive = archive_from(&preview.backup);
            validate_archive(&archive)?;
            let archive_document = encode_document(&archive)?;
            let receipt = receipt_for(operation_id, &preview.input_digest, revision, &plan.store_preferences, &archive_document, counts)?;
            let document = encode_document(&receipt)?;
            transaction.execute("INSERT INTO native_backup_imports(operation_id,input_digest,expected_revision,revision,preferences_applied,document,byte_size) VALUES(?1,?2,?3,?4,0,?5,?6)",
                params![operation_id, receipt.input_digest, checked_revision(revision)?, checked_revision(receipt.revision)?, document, document.len() as i64])?;
            transaction.execute("INSERT INTO native_backup_archives(operation_id,sha256,document,byte_size) VALUES(?1,?2,?3,?4)",
                params![operation_id, receipt.archive_sha256, archive_document, archive_document.len() as i64])?;
            prune_receipts(transaction)?;
            check_storage(transaction)?;
            // Both datasets have the same repository revision and transaction.
            sync::record_change(transaction, &ChangeKind::DislikeChanged, &receipt.change.details, receipt.revision)?;
            let details = receipt.change.details.clone();
            committed = Some(receipt);
            Ok(((), ChangeKind::ListsChanged, None, details))
        }, check);
        match result {
            Ok(((), change)) => {
                let receipt = committed.context("Committed native backup receipt is missing")?;
                ensure!(
                    receipt.change == change,
                    "Committed native backup change differs from receipt"
                );
                receipt.result(false)
            }
            Err(error) if error.is::<ReplayedImport>() => {
                replay.context("Replayed native backup receipt is missing")
            }
            Err(error) => Err(error),
        }
    }

    pub fn native_backup_result(&self, operation_id: &str) -> Result<Option<NativeBackupCommit>> {
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

    pub fn native_backup_archive(&self, operation_id: &str) -> Result<Option<NativeBackupArchive>> {
        validate_operation(operation_id)?;
        let connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("Repository lock was poisoned"))?;
        schema::validate_current(&connection)?;
        let Some(receipt) = import_at(&connection, operation_id)? else {
            return Ok(None);
        };
        Ok(Some(archive_at(&connection, &receipt)?))
    }

    pub fn pending_native_backup_preferences(&self) -> Result<Vec<NativeBackupCommit>> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("Repository lock was poisoned"))?;
        schema::validate_current(&connection)?;
        pending_at(&connection)?
            .into_iter()
            .map(|receipt| receipt.result(true))
            .collect()
    }

    /// The current machine authority is retained; appended scripts need approval.
    pub fn native_backup_preview_store_candidate(
        &self,
        preview: &NativeBackupPreview,
        current: &StoreData,
    ) -> Result<StoreData> {
        ensure!(
            preview.expected_user_revision == preview.sealed_revision
                && preview.input_digest == preview.sealed_digest
                && fingerprint(
                    &preview.backup,
                    &preview.policy,
                    preview.expected_user_revision,
                    &preview.plan,
                    &|| Ok(())
                )? == preview.sealed_digest,
            "Native backup preview was modified; create a new preview"
        );
        let archive = archive_from(&preview.backup);
        project_candidate(&preview.plan.store_preferences, &archive, current)
    }

    /// The current machine authority is retained; appended scripts need approval.
    pub fn native_backup_store_candidate(
        &self,
        commit: &NativeBackupCommit,
        current: &StoreData,
    ) -> Result<StoreData> {
        validate_operation(&commit.operation_id)?;
        let connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("Repository lock was poisoned"))?;
        schema::validate_current(&connection)?;
        let receipt = import_at(&connection, &commit.operation_id)?
            .context("Native backup receipt is no longer available")?;
        let actual = receipt.result(true)?;
        ensure!(
            actual.input_digest == commit.input_digest
                && actual.change == commit.change
                && actual.counts == commit.counts,
            "Native backup preference receipt identity differs"
        );
        let archive = archive_at(&connection, &receipt)?;
        project_candidate(&receipt.preferences, &archive, current)
    }

    /// Acknowledgement follows the atomic JSON preference marker and adds no revision.
    pub fn acknowledge_native_backup_preferences(
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
            .context("Native backup receipt is no longer available")?;
        ensure!(
            receipt.input_digest == input_digest,
            "Native backup preference receipt identity differs"
        );
        if !receipt.preferences_applied {
            receipt.preferences_applied = true;
            let document = encode_document(&receipt)?;
            transaction.execute("UPDATE native_backup_imports SET preferences_applied=1,document=?2,byte_size=?3 WHERE operation_id=?1",
                params![operation_id, document, document.len() as i64])?;
            check_storage(&transaction)?;
        }
        transaction.commit()?;
        Ok(())
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

fn counts_at(connection: &Connection, plan: &NativeImportPlan) -> Result<NativeBackupCounts> {
    let known: BTreeSet<_> = plan
        .list_plan
        .lists
        .default_list
        .iter()
        .chain(&plan.list_plan.lists.love_list)
        .chain(plan.list_plan.lists.temp_list.iter().flatten())
        .chain(
            plan.list_plan
                .lists
                .user_list
                .iter()
                .flat_map(|list| &list.tracks),
        )
        .map(|track| track.id.as_str())
        .collect();
    let mut applied = 0;
    let total = plan.edited_lyrics.as_ref().map_or(0, Vec::len);
    if let Some(lyrics) = &plan.edited_lyrics {
        for lyric in lyrics {
            if known.contains(lyric.track_id.as_str())
                || connection.query_row(
                    "SELECT EXISTS(SELECT 1 FROM tracks WHERE id=?1)",
                    [&lyric.track_id],
                    |row| row.get::<_, bool>(0),
                )?
            {
                applied += 1;
            }
        }
    }
    Ok(NativeBackupCounts {
        lists: plan.list_plan.list_changes.len(),
        declared_paths: plan.declared_local_paths.len(),
        sources: plan.unapproved_sources.len(),
        download_plans: plan.download_plans.as_ref().map_or(0, Vec::len),
        edited_lyrics: total,
        applied_edited_lyrics: applied,
        archived_edited_lyrics: total - applied,
    })
}

fn archive_from(backup: &NativeBackup) -> NativeBackupArchive {
    NativeBackupArchive {
        declared_local_paths: backup.declared_local_paths.clone(),
        sources: backup.sources.clone(),
        download_plans: backup.downloads.clone(),
        edited_lyrics: backup.edited_lyrics.clone(),
        extensions: backup.extensions.clone(),
    }
}

fn validate_preferences(preferences: &StoreData) -> Result<()> {
    ensure!(
        preferences.sources.is_empty()
            && preferences.selected.is_none()
            && preferences.local_files.is_empty(),
        "Native backup preference archive contains authority records"
    );
    ensure!(
        preferences.volume.is_finite() && (0.0..=1.0).contains(&preferences.volume),
        "Native backup volume is invalid"
    );
    preferences.audio_preferences.validate()?;
    preferences.equalizer.validate()?;
    preferences.playback_processing.validate()?;
    preferences.spatial.validate()?;
    preferences.timeout_stop.validate()?;
    Ok(())
}

fn validate_archive(archive: &NativeBackupArchive) -> Result<()> {
    let backup = NativeBackup {
        format: "lx_native_backup".into(),
        version: 1,
        store: StoreData::default(),
        lists: Default::default(),
        dislike_rules: String::new(),
        declared_local_paths: archive.declared_local_paths.clone(),
        sources: archive.sources.clone(),
        downloads: archive.download_plans.clone(),
        edited_lyrics: archive.edited_lyrics.clone(),
        extensions: archive.extensions.clone(),
    };
    backup.validate()?;
    for source in &archive.sources {
        import_script(&source.script)?;
    }
    Ok(())
}

fn receipt_for(
    operation_id: &str,
    digest: &str,
    revision: u64,
    preferences: &StoreData,
    archive_document: &str,
    counts: NativeBackupCounts,
) -> Result<StoredImport> {
    validate_preferences(preferences)?;
    let next = revision
        .checked_add(1)
        .context("Native backup revision overflow")?;
    let details = serde_json::json!({"origin":"native_backup","operation_id":operation_id,"input_digest":digest,"counts":counts});
    Ok(StoredImport {
        operation_id: operation_id.into(),
        input_digest: digest.into(),
        expected_revision: revision,
        revision: next,
        preferences: preferences.clone(),
        archive_sha256: sha256(archive_document),
        preferences_applied: false,
        counts,
        change: RepositoryChange {
            revision: next,
            kind: ChangeKind::ListsChanged,
            track_id: None,
            details,
        },
    })
}

fn import_at(connection: &Connection, operation_id: &str) -> Result<Option<StoredImport>> {
    let row: Option<(String,i64,i64,i64,String,i64)> = connection.query_row(
        "SELECT input_digest,expected_revision,revision,preferences_applied,document,byte_size FROM native_backup_imports WHERE operation_id=?1", [operation_id],
        |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?,row.get(4)?,row.get(5)?))).optional()?;
    row.map(|(digest, expected, revision, applied, document, bytes)| {
        let receipt: StoredImport = decode_document(&document)?;
        ensure!(
            receipt.operation_id == operation_id
                && receipt.input_digest == digest
                && checked_revision(receipt.expected_revision)? == expected
                && checked_revision(receipt.revision)? == revision
                && applied == i64::from(receipt.preferences_applied)
                && bytes == document.len() as i64,
            "Native backup receipt SQL identity differs from its document"
        );
        receipt.result(true)?;
        archive_at(connection, &receipt)?;
        Ok(receipt)
    })
    .transpose()
}

fn archive_at(connection: &Connection, receipt: &StoredImport) -> Result<NativeBackupArchive> {
    let (digest, document, bytes): (String, String, i64) = connection
        .query_row(
            "SELECT sha256,document,byte_size FROM native_backup_archives WHERE operation_id=?1",
            [&receipt.operation_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .context("Native backup data archive is missing")?;
    ensure!(
        bytes == document.len() as i64
            && digest == receipt.archive_sha256
            && sha256(&document) == digest,
        "Native backup data archive integrity differs from its receipt"
    );
    let archive: NativeBackupArchive = decode_document(&document)?;
    validate_archive(&archive)?;
    ensure!(
        archive.declared_local_paths.len() == receipt.counts.declared_paths
            && archive.sources.len() == receipt.counts.sources
            && archive.download_plans.as_ref().map_or(0, Vec::len) == receipt.counts.download_plans
            && archive.edited_lyrics.as_ref().map_or(0, Vec::len) == receipt.counts.edited_lyrics,
        "Native backup archive counts differ from receipt"
    );
    Ok(archive)
}

fn pending_at(connection: &Connection) -> Result<Vec<StoredImport>> {
    check_storage(connection)?;
    let mut statement = connection.prepare("SELECT operation_id FROM native_backup_imports WHERE preferences_applied=0 ORDER BY revision,operation_id")?;
    let ids: Vec<String> = statement
        .query_map([], |row| row.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    ensure!(
        ids.len() <= 1,
        "Multiple pending native backup preferences are invalid"
    );
    ids.into_iter()
        .map(|id| import_at(connection, &id)?.context("Pending native backup receipt disappeared"))
        .collect()
}

fn check_storage(connection: &Connection) -> Result<()> {
    let (receipts, receipt_bytes): (i64, i64) = connection.query_row(
        "SELECT count(*),coalesce(sum(byte_size),0) FROM native_backup_imports",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let (archives, archive_bytes): (i64, i64) = connection.query_row(
        "SELECT count(*),coalesce(sum(byte_size),0) FROM native_backup_archives",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    ensure!(
        receipts <= MAX_RECEIPTS
            && archives == receipts
            && receipt_bytes.saturating_add(archive_bytes) <= MAX_ARCHIVE_BYTES,
        "Native backup archive storage budget exceeded"
    );
    Ok(())
}

fn prune_receipts(connection: &Connection) -> Result<()> {
    loop {
        let (count,bytes): (i64,i64) = connection.query_row("SELECT count(*),coalesce(sum(i.byte_size+a.byte_size),0) FROM native_backup_imports i JOIN native_backup_archives a USING(operation_id)", [], |row| Ok((row.get(0)?,row.get(1)?)))?;
        if count <= MAX_RECEIPTS && bytes <= MAX_ARCHIVE_BYTES {
            return Ok(());
        }
        let removed = connection.execute("DELETE FROM native_backup_imports WHERE operation_id=(SELECT operation_id FROM native_backup_imports WHERE preferences_applied=1 ORDER BY revision,operation_id LIMIT 1)", [])?;
        ensure!(
            removed == 1,
            "Pending native backup exceeds the archive storage budget"
        );
    }
}

fn replace_edited_lyrics(transaction: &Transaction<'_>, lyric: &NativeEditedLyrics) -> Result<()> {
    lyric.bundle.validate()?;
    transaction.execute(
        "INSERT INTO lyric_bundles(track_id,origin) VALUES(?1,'edited') ON CONFLICT DO NOTHING",
        [&lyric.track_id],
    )?;
    transaction.execute(
        "DELETE FROM lyric_fields WHERE track_id=?1 AND origin='edited'",
        [&lyric.track_id],
    )?;
    for (kind, name) in [
        (LyricKind::Lyric, "lyric"),
        (LyricKind::Tlyric, "tlyric"),
        (LyricKind::Rlyric, "rlyric"),
        (LyricKind::Lxlyric, "lxlyric"),
    ] {
        let (state, text) = match lyric.bundle.field(kind) {
            LyricField::Missing => ("missing", None),
            LyricField::Null => ("null", None),
            LyricField::Text(text) => ("text", Some(text.as_str())),
        };
        transaction.execute("INSERT INTO lyric_fields(track_id,origin,kind,state,text) VALUES(?1,'edited',?2,?3,?4)", params![lyric.track_id,name,state,text])?;
    }
    Ok(())
}

fn project_preferences(preferences: &StoreData, current: &StoreData) -> StoreData {
    let mut projected = current.clone();
    projected.volume = preferences.volume;
    projected.audio_preferences = preferences.audio_preferences.clone();
    projected.equalizer = preferences.equalizer;
    projected.playback_processing = preferences.playback_processing;
    projected.audio_analysis_enabled = preferences.audio_analysis_enabled;
    projected.spatial = preferences.spatial;
    if let Some(effects) = &mut projected.effects {
        effects.settings.spatial = projected.spatial;
    }
    projected.timeout_stop = preferences.timeout_stop;
    projected
}

fn project_candidate(
    preferences: &StoreData,
    archive: &NativeBackupArchive,
    current: &StoreData,
) -> Result<StoreData> {
    validate_preferences(preferences)?;
    validate_archive(archive)?;
    let mut candidate = project_preferences(preferences, current);
    let mut hashes: BTreeSet<_> = current
        .sources
        .iter()
        .map(|source| source.sha256.clone())
        .collect();
    for source in &archive.sources {
        let imported = import_script(&source.script)?;
        ensure!(
            !imported.approved && imported.approval_policy_version == 0,
            "Native backup source archive acquired authority"
        );
        if hashes.insert(imported.sha256.clone()) {
            candidate.sources.push(imported);
        }
    }
    ensure!(
        candidate.sources.len() - current.sources.len() <= 64,
        "Native backup source count exceeded"
    );
    Ok(candidate)
}

fn validate_operation(operation: &str) -> Result<()> {
    ensure!(
        !operation.is_empty() && operation.len() <= 256 && !operation.chars().any(char::is_control),
        "Native backup operation identity is invalid"
    );
    Ok(())
}
fn validate_digest(digest: &str) -> Result<()> {
    ensure!(
        digest.len() == 64
            && digest
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
        "Native backup digest is invalid"
    );
    Ok(())
}
fn sha256(document: &str) -> String {
    format!("{:x}", Sha256::digest(document.as_bytes()))
}

type SettingsFingerprint<'a> = (&'a Map<String, Value>, &'a Map<String, Value>, &'a [String]);

#[derive(Serialize)]
struct PlanFingerprint<'a> {
    lists: &'a crate::domain::lists::ListData,
    changes: Vec<(&'a str, &'a str, &'static str)>,
    metadata: Vec<(&'a crate::domain::lists::ListInfo, &'a Map<String, Value>)>,
    settings: Option<SettingsFingerprint<'a>>,
    local_references: Vec<(&'a str, &'a str)>,
    preferences: &'a StoreData,
    sources: &'a [crate::source::ImportedSource],
    paths: &'a [String],
    dislike: &'a crate::domain::lists::DislikeRules,
    downloads: &'a Option<Vec<NativeDownloadPlan>>,
    lyrics: &'a Option<Vec<NativeEditedLyrics>>,
    extensions: &'a Map<String, Value>,
}

struct HashWriter<'a> {
    hash: Sha256,
    bytes: usize,
    unchecked: usize,
    check: &'a dyn Fn() -> Result<()>,
}
impl Write for HashWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > (2 * MAX_BACKUP_BYTES).saturating_sub(self.bytes) {
            return Err(io::Error::other("Native backup plan exceeds 32 MiB"));
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
    backup: &NativeBackup,
    policy: &ConflictPolicy,
    revision: u64,
    plan: &NativeImportPlan,
    check: &dyn Fn() -> Result<()>,
) -> Result<String> {
    check()?;
    let mut hash = Sha256::new();
    hash.update(b"lx-native-backup-preview-v1\0");
    hash.update(revision.to_le_bytes());
    let input = serde_json::to_vec(backup)?;
    ensure!(
        input.len() <= MAX_BACKUP_BYTES,
        "Native backup input exceeds 16 MiB"
    );
    hash.update((input.len() as u64).to_le_bytes());
    for chunk in input.chunks(64 * 1024) {
        check()?;
        hash.update(chunk);
    }
    let policy = match policy {
        ConflictPolicy::ReplaceTracks => "replace_tracks",
        ConflictPolicy::KeepExisting => "keep_existing",
        ConflictPolicy::Copy { .. } => {
            anyhow::bail!("Native backup requires merge or keep-existing policy")
        }
    };
    hash.update(policy.as_bytes());
    let projection = PlanFingerprint {
        lists: &plan.list_plan.lists,
        changes: plan
            .list_plan
            .list_changes
            .iter()
            .map(|change| {
                (
                    change.imported_id.as_str(),
                    change.destination_id.as_str(),
                    match change.disposition {
                        ListImportDisposition::Added => "added",
                        ListImportDisposition::ReplacedTracks => "replaced_tracks",
                        ListImportDisposition::KeptExisting => "kept_existing",
                        ListImportDisposition::Copied => "copied",
                    },
                )
            })
            .collect(),
        metadata: plan
            .list_plan
            .archived_list_metadata
            .iter()
            .map(|metadata| (&metadata.info, &metadata.wire))
            .collect(),
        settings: plan.list_plan.settings.as_ref().map(|settings| {
            (
                &settings.archived,
                &settings.preferences,
                settings.requires_activation.as_slice(),
            )
        }),
        local_references: plan
            .list_plan
            .local_references
            .iter()
            .map(|reference| {
                (
                    reference.track_id.as_str(),
                    reference.declared_path.as_str(),
                )
            })
            .collect(),
        preferences: &plan.store_preferences,
        sources: &plan.unapproved_sources,
        paths: &plan.declared_local_paths,
        dislike: &plan.dislike,
        downloads: &plan.download_plans,
        lyrics: &plan.edited_lyrics,
        extensions: &plan.extensions,
    };
    let mut writer = HashWriter {
        hash,
        bytes: 0,
        unchecked: 0,
        check,
    };
    serde_json::to_writer(&mut writer, &projection)?;
    check()?;
    Ok(format!("{:x}", writer.hash.finalize()))
}

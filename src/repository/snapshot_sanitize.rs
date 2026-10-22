//! Removes execution authority from a private current-schema database image.
//!
//! The image owner must compact this connection and verify its integrity before
//! publishing it. This module never opens paths or reconstructs permissions from
//! persisted documents.

use super::schema;
use crate::{
    backup::settings::LxSettings,
    domain::{
        artwork::{ArtworkBlobMeta, ArtworkInfo, ArtworkProvenance},
        library::LibraryRoot,
        library_watch::{LibraryWatchPreference, RootMovePolicy},
        music::LocalAsset,
    },
    download::{
        CollisionPolicy, DownloadState,
        task_model::{DownloadSettings, DownloadStageState, DownloadTask, StoredDestinationGrant},
    },
};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::de::{self, DeserializeSeed, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::atomic::{AtomicUsize, Ordering},
};

const MIB: i64 = 1024 * 1024;
const IMAGE_DOCUMENT_BUDGET: i64 = 1024 * MIB;
const MAX_IMAGE_ROWS: i64 = 1_000_000;
const MAX_DOCUMENT_NODES: usize = 65_536;
const MAX_SCHEMA_OBJECTS: usize = 256;
const MAX_SCHEMA_SQL_BYTES: usize = 1024 * 1024;
const MAX_SCHEMA_OBJECT_SQL: usize = 64 * 1024;
const SCHEMA_PREPARATION_BYTES: usize = 8 * 1024 * 1024;
static SCHEMA_RESERVED_BYTES: AtomicUsize = AtomicUsize::new(0);
struct SchemaReservation;
impl SchemaReservation {
    fn acquire() -> Result<Self> {
        let mut current = SCHEMA_RESERVED_BYTES.load(Ordering::Acquire);
        loop {
            ensure!(
                current <= 4 * SCHEMA_PREPARATION_BYTES - SCHEMA_PREPARATION_BYTES,
                "Snapshot schema preparation quota exceeded"
            );
            match SCHEMA_RESERVED_BYTES.compare_exchange_weak(
                current,
                current + SCHEMA_PREPARATION_BYTES,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return Ok(Self),
                Err(next) => current = next,
            }
        }
    }
}
impl Drop for SchemaReservation {
    fn drop(&mut self) {
        SCHEMA_RESERVED_BYTES.fetch_sub(SCHEMA_PREPARATION_BYTES, Ordering::AcqRel);
    }
}
#[derive(Debug, PartialEq, Eq)]
struct SchemaObject {
    kind: String,
    table: String,
    sql: Option<String>,
}
const ARCHIVE_TABLE: &str = "snapshot_metadata_archive";
const ARCHIVE_SQL: &str = "CREATE TABLE snapshot_metadata_archive (kind TEXT NOT NULL CHECK(kind IN ('completed_download','remote_artwork')),object_id TEXT NOT NULL,document TEXT NOT NULL CHECK(json_valid(document)),byte_size INTEGER NOT NULL CHECK(byte_size BETWEEN 1 AND 262144 AND byte_size=length(CAST(document AS BLOB))),PRIMARY KEY(kind,object_id))";

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct SnapshotSanitizeReport {
    pub revoked_peers: u64,
    pub inactive_library_roots: u64,
    pub revoked_download_destinations: u64,
    pub paused_download_tasks: u64,
    pub scrubbed_settings_archives: u64,
    pub removed_transient_rows: u64,
    pub archived_metadata_rows: u64,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SafeArtifact {
    basename: String,
    bytes: u64,
    sha256: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CompletedDownloadMetadata {
    task_id: String,
    list_id: Option<String>,
    list_name: Option<String>,
    requested_quality: String,
    actual_quality: String,
    created_at_ms: u64,
    updated_at_ms: u64,
    output: Option<SafeArtifact>,
    metadata_stage: DownloadStageState,
    cover_stage: DownloadStageState,
    embedded_lyrics_stage: DownloadStageState,
    lyrics_stage: DownloadStageState,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RemoteArtworkMetadata {
    original_track_id: String,
    original_platform: String,
    original_record_sha256: String,
    matched_record_sha256: Option<String>,
    provenance: String,
    source_sha256: Option<String>,
    policy_version: Option<u64>,
    blob: Option<ArtworkBlobMeta>,
    resolved_at_ms: u64,
    expire_at_ms: u64,
}

struct Table {
    name: &'static str,
    columns: &'static str,
    documents: &'static [(&'static str, i64)],
}

// Column names are also an allow-list: hidden or additional cells cannot carry
// authority outside the known schema's document columns.
const TABLES: &[Table] = &[
    Table {
        name: "tag_write_approvals",
        columns: "approval_id asset_id approval_epoch revoked document byte_size",
        documents: &[("document", 65536)],
    },
    Table {
        name: "tag_edit_operations",
        columns: "operation_id approval_id asset_id operation_epoch ledger_revision phase document byte_size",
        documents: &[("document", MIB)],
    },
    Table {
        name: "tag_edit_artifacts",
        columns: "operation_id role phase actual_bytes reserved_bytes document byte_size",
        documents: &[("document", 65536)],
    },
    Table {
        name: "physical_snapshot_state",
        columns: "singleton profile_id profile_epoch operation_epoch selection_epoch",
        documents: &[],
    },
    Table {
        name: "physical_snapshot_operations",
        columns: "operation_id profile_id profile_epoch operation_epoch selection_epoch expected_snapshot_revision maximum_image_bytes request_digest ledger_revision state document byte_size",
        documents: &[("document", MIB)],
    },
    Table {
        name: "tracks",
        columns: "id document",
        documents: &[("document", 8 * MIB)],
    },
    Table {
        name: "local_assets",
        columns: "identity_key track_id document",
        documents: &[("document", 8 * MIB)],
    },
    Table {
        name: "lyric_bundles",
        columns: "track_id origin",
        documents: &[],
    },
    Table {
        name: "lyric_fields",
        columns: "track_id origin kind state text",
        documents: &[],
    },
    Table {
        name: "repository_state",
        columns: "singleton revision",
        documents: &[],
    },
    Table {
        name: "change_outbox",
        columns: "revision document",
        documents: &[("document", 8 * MIB)],
    },
    Table {
        name: "import_markers",
        columns: "name source_digest revision",
        documents: &[],
    },
    Table {
        name: "profile_metadata",
        columns: "singleton digest change_document",
        documents: &[("change_document", 8 * MIB)],
    },
    Table {
        name: "music_lists",
        columns: "id kind position document scroll_position update_document",
        documents: &[("document", 8 * MIB), ("update_document", 8 * MIB)],
    },
    Table {
        name: "music_list_items",
        columns: "list_id music_id track_id position document",
        documents: &[("document", 8 * MIB)],
    },
    Table {
        name: "dislike_state",
        columns: "singleton rules",
        documents: &[],
    },
    Table {
        name: "search_history",
        columns: "word position",
        documents: &[],
    },
    Table {
        name: "playback_state",
        columns: "singleton document",
        documents: &[("document", 32 * MIB)],
    },
    Table {
        name: "resolved_media",
        columns: "cache_key original_track_id original_platform actual_quality source_sha256 source_policy_version music_info_sha256 resolved_at_ms expire_at_ms last_access_ms byte_size document",
        documents: &[("document", 8 * MIB)],
    },
    Table {
        name: "source_matches",
        columns: "cache_key original_track_id original_platform original_record_sha256 algorithm_version matched_at_ms expire_at_ms last_access_ms byte_size document",
        documents: &[("document", 8 * MIB)],
    },
    Table {
        name: "artwork_blobs",
        columns: "sha256 byte_size last_access_ms document data",
        documents: &[("document", 4096)],
    },
    Table {
        name: "artwork_info",
        columns: "cache_key original_track_id original_record_sha256 resolved_at_ms expire_at_ms last_access_ms byte_size blob_sha256 document",
        documents: &[("document", 8 * MIB)],
    },
    Table {
        name: "library_roots",
        columns: "root_id path_key active grant_epoch generation document",
        documents: &[("document", 8 * MIB)],
    },
    Table {
        name: "local_file_grants",
        columns: "asset_id epoch grant_path_key document",
        documents: &[("document", 8 * MIB)],
    },
    Table {
        name: "local_asset_locations",
        columns: "asset_id path_key document binding_revision physical_identity_key",
        documents: &[("document", 8 * MIB)],
    },
    Table {
        name: "library_membership",
        columns: "root_id asset_id last_seen_generation",
        documents: &[],
    },
    Table {
        name: "library_scan_runs",
        columns: "root_id generation operation_id grant_epoch state document scope_document",
        documents: &[("document", 8 * MIB), ("scope_document", MIB)],
    },
    Table {
        name: "library_scan_seen",
        columns: "root_id generation path_key path asset_id",
        documents: &[],
    },
    Table {
        name: "library_root_locations",
        columns: "root_id location_revision",
        documents: &[],
    },
    Table {
        name: "library_watch_preferences",
        columns: "root_id watch_generation enabled document",
        documents: &[("document", 8 * MIB)],
    },
    Table {
        name: "library_dirty_state",
        columns: "root_id watch_generation received_sequence covered_sequence full_rescan_required",
        documents: &[],
    },
    Table {
        name: "library_identity_reservations",
        columns: "path_key asset_id track_id lx_id physical_identity_key expires_at_ms document",
        documents: &[("document", 8 * MIB)],
    },
    Table {
        name: "library_relink_candidates",
        columns: "candidate_id asset_id root_id expires_at_ms document",
        documents: &[("document", 8 * MIB)],
    },
    Table {
        name: "library_relink_results",
        columns: "operation_id request_digest asset_id document",
        documents: &[("document", 8 * MIB)],
    },
    Table {
        name: "download_destination_grants",
        columns: "id profile_id epoch revision revoked document",
        documents: &[("document", 8 * MIB)],
    },
    Table {
        name: "download_settings",
        columns: "singleton document",
        documents: &[("document", 8 * MIB)],
    },
    Table {
        name: "download_tasks",
        columns: "id profile_id grant_id position task_revision operation_epoch state document byte_size",
        documents: &[("document", 256 * 1024)],
    },
    Table {
        name: "backup_imports",
        columns: "operation_id input_digest expected_revision revision document byte_size",
        documents: &[("document", 8 * MIB)],
    },
    Table {
        name: "native_backup_imports",
        columns: "operation_id input_digest expected_revision revision preferences_applied document byte_size",
        documents: &[("document", 8 * MIB)],
    },
    Table {
        name: "native_backup_archives",
        columns: "operation_id sha256 document byte_size",
        documents: &[("document", 8 * MIB)],
    },
    Table {
        name: "compat_settings",
        columns: "singleton revision document byte_size",
        documents: &[("document", 8 * MIB)],
    },
    Table {
        name: "backup_list_metadata",
        columns: "list_id revision document byte_size",
        documents: &[("document", 8 * MIB)],
    },
    Table {
        name: "download_owned_artifacts",
        columns: "nonce task_id operation_epoch phase actual_bytes reserved_bytes sha256 document byte_size",
        documents: &[("document", 65536)],
    },
    Table {
        name: "download_allocation_intents",
        columns: "nonce task_id operation_epoch reserved_bytes document byte_size",
        documents: &[("document", 65536)],
    },
    Table {
        name: "sync_profile_state",
        columns: "singleton profile_epoch pairing_epoch pairing_enabled sequence journal_floor",
        documents: &[],
    },
    Table {
        name: "sync_peers",
        columns: "peer_id client_id auth_epoch revoked private_key document byte_size",
        documents: &[("document", 16384)],
    },
    Table {
        name: "sync_snapshots",
        columns: "dataset digest sha256 document byte_size",
        documents: &[("document", 8 * MIB)],
    },
    Table {
        name: "sync_current",
        columns: "dataset digest sha256 available source_revision sequence",
        documents: &[],
    },
    Table {
        name: "sync_baselines",
        columns: "peer_id dataset common_digest common_sha256 captured_repository_revision covered_sequence",
        documents: &[],
    },
    Table {
        name: "sync_journal",
        columns: "sequence dataset repository_revision origin_peer action_document byte_size",
        documents: &[("action_document", 8 * MIB)],
    },
    Table {
        name: "sync_list_metadata",
        columns: "list_id document byte_size",
        documents: &[("document", 16384)],
    },
    Table {
        name: "sync_capture_override",
        columns: "dataset document byte_size",
        documents: &[("document", 8 * MIB)],
    },
    Table {
        name: "sync_mode_preferences",
        columns: "peer_id dataset auth_epoch document",
        documents: &[("document", 128)],
    },
];

/// Purges authority only on an exclusively owned, unpublished database image.
///
/// The caller installs SQLite's cancellation handler and subsequently runs
/// `VACUUM` and integrity checks on the same actual-file connection. All data
/// mutations are committed together. Journal-enabled callers can roll back;
/// a disposable journal-OFF image is quarantined for cleanup on every failure.
pub(crate) fn purge_snapshot_authority(
    snapshot: &mut Connection,
    check: &dyn Fn() -> Result<()>,
) -> Result<SnapshotSanitizeReport> {
    check()?;
    ensure!(
        snapshot.is_autocommit(),
        "Snapshot sanitization needs an idle connection"
    );
    schema::validate_current(snapshot)?;
    validate_schema(snapshot, check)?;
    snapshot.pragma_update(None, "secure_delete", true)?;
    let tx = snapshot.transaction_with_behavior(TransactionBehavior::Immediate)?;
    validate_documents(&tx, check)?;
    if !archive_exists(&tx)? {
        tx.execute_batch(&format!("{ARCHIVE_SQL};"))?;
    }
    validate_metadata_archive(&tx, check)?;
    let revision: i64 = tx.query_row(
        "SELECT revision FROM repository_state WHERE singleton=1",
        [],
        |row| row.get(0),
    )?;
    let mut report = SnapshotSanitizeReport::default();
    validate_peers(&tx, check)?;
    report.revoked_peers =
        tx.execute("UPDATE sync_peers SET revoked=1,private_key=NULL", [])? as u64;
    tx.execute("UPDATE sync_profile_state SET pairing_enabled=0,profile_epoch=CASE WHEN profile_epoch<9223372036854775807 THEN profile_epoch+1 ELSE profile_epoch END,pairing_epoch=CASE WHEN pairing_epoch<9223372036854775807 THEN pairing_epoch+1 ELSE pairing_epoch END", [])?;
    for table in [
        "tag_edit_artifacts",
        "tag_edit_operations",
        "tag_write_approvals",
        "sync_baselines",
        "sync_mode_preferences",
        "sync_capture_override",
        "local_file_grants",
        "library_scan_seen",
        "library_scan_runs",
        "library_dirty_state",
        "library_identity_reservations",
        "library_relink_candidates",
        "library_relink_results",
        "resolved_media",
        "source_matches",
        "download_allocation_intents",
        "download_owned_artifacts",
        "physical_snapshot_operations",
        "native_backup_archives",
        "native_backup_imports",
    ] {
        check()?;
        report.removed_transient_rows += tx.execute(&format!("DELETE FROM {table}"), [])? as u64;
    }
    tx.execute("UPDATE physical_snapshot_state SET profile_id=lower(hex(randomblob(16))),profile_epoch=1,operation_epoch=0,selection_epoch=0 WHERE singleton=1",[])?;
    sanitize_library(&tx, check, &mut report)?;
    sanitize_downloads(&tx, check, &mut report)?;
    sanitize_settings(&tx, check, &mut report)?;
    sanitize_artwork(&tx, check, &mut report)?;
    validate_metadata_archive(&tx, check)?;
    validate_purged_authority(&tx)?;
    let unchanged_revision: i64 = tx.query_row(
        "SELECT revision FROM repository_state WHERE singleton=1",
        [],
        |row| row.get(0),
    )?;
    ensure!(
        revision == unchanged_revision,
        "Snapshot sanitization changed the user revision"
    );
    check()?;
    tx.commit()?;
    Ok(report)
}

fn validate_schema(connection: &Connection, check: &dyn Fn() -> Result<()>) -> Result<()> {
    validate_schema_for_version(connection, super::SCHEMA_VERSION, check)
}
pub(crate) fn validate_schema_for_version(
    connection: &Connection,
    version: u32,
    check: &dyn Fn() -> Result<()>,
) -> Result<()> {
    ensure!(
        schema::version(connection)? == version,
        "Snapshot schema version differs from its requested reference"
    );
    schema::validate_identity(connection, version)?;
    // Only production-owned SQL is executed to build the reference. Source
    // sqlite_schema text is bounded metadata, never a migration input.
    let _reservation = SchemaReservation::acquire()?;
    let trusted = schema::trusted_reference_for_version(version)?;
    check()?;
    if archive_exists(connection)? {
        trusted.execute_batch(ARCHIVE_SQL)?;
    }
    let expected = schema_objects(&trusted, check)?;
    let actual = schema_objects(connection, check)?;
    ensure!(
        actual == expected,
        "Snapshot DDL differs from the trusted production schema"
    );
    for table in TABLES {
        if version != super::SCHEMA_VERSION {
            break;
        }
        check()?;
        let expected: BTreeSet<_> = table.columns.split_whitespace().collect();
        let mut found = BTreeSet::new();
        let mut statement = connection.prepare(&format!("PRAGMA table_xinfo({})", table.name))?;
        for column in statement.query_map([], |row| {
            Ok((row.get::<_, String>(1)?, row.get::<_, i64>(6)?))
        })? {
            let (name, hidden) = column?;
            ensure!(
                hidden == 0 && expected.contains(name.as_str()),
                "Snapshot has unrecognized table cells"
            );
            found.insert(name);
        }
        ensure!(
            found.len() == expected.len(),
            "Snapshot table is incomplete"
        );
    }
    if archive_exists(connection)? {
        validate_metadata_archive(connection, check)?;
    }
    Ok(())
}
fn schema_objects(
    connection: &Connection,
    check: &dyn Fn() -> Result<()>,
) -> Result<BTreeMap<String, SchemaObject>> {
    let count: i64 =
        connection.query_row("SELECT count(*) FROM sqlite_schema", [], |row| row.get(0))?;
    let count = usize::try_from(count).context("Snapshot schema object count is negative")?;
    ensure!(
        count <= MAX_SCHEMA_OBJECTS,
        "Snapshot schema object quota exceeded"
    );
    let mut objects = BTreeMap::new();
    let mut total = 0usize;
    let mut statement = connection.prepare("SELECT CASE WHEN length(CAST(type AS BLOB))<=16 THEN type END,CASE WHEN length(CAST(name AS BLOB))<=256 THEN name END,CASE WHEN length(CAST(tbl_name AS BLOB))<=256 THEN tbl_name END,length(CAST(sql AS BLOB)),CASE WHEN length(CAST(sql AS BLOB))<=65536 THEN sql END FROM sqlite_schema ORDER BY name")?;
    let mut rows = statement.query([])?;
    while let Some(row) = rows.next()? {
        check()?;
        let kind: String = row
            .get::<_, Option<String>>(0)?
            .context("Snapshot schema kind quota exceeded")?;
        let name: String = row
            .get::<_, Option<String>>(1)?
            .context("Snapshot schema name quota exceeded")?;
        let table: String = row
            .get::<_, Option<String>>(2)?
            .context("Snapshot schema table-name quota exceeded")?;
        let length: Option<i64> = row.get(3)?;
        let sql: Option<String> = row.get(4)?;
        ensure!(
            matches!(kind.as_str(), "table" | "index" | "trigger" | "view")
                && name.len() <= 256
                && table.len() <= 256,
            "Snapshot schema object metadata is invalid"
        );
        if let Some(length) = length {
            let length =
                usize::try_from(length).context("Snapshot schema SQL length is negative")?;
            ensure!(
                length <= MAX_SCHEMA_OBJECT_SQL
                    && sql.as_ref().is_some_and(|sql| sql.len() == length),
                "Snapshot DDL exceeds its object quota"
            );
            total = total
                .checked_add(length)
                .context("Snapshot schema byte count overflow")?;
            ensure!(
                total <= MAX_SCHEMA_SQL_BYTES,
                "Snapshot schema byte quota exceeded"
            );
        } else {
            ensure!(
                sql.is_none() && kind == "index" && name.starts_with("sqlite_autoindex_"),
                "Snapshot has an unrecognized implicit schema object"
            );
        }
        let object = SchemaObject {
            kind,
            table,
            sql: sql.map(|sql| canonical_schema_sql(&sql)).transpose()?,
        };
        ensure!(
            objects.insert(name, object).is_none(),
            "Snapshot schema object name is duplicated"
        );
    }
    Ok(objects)
}
/// Fold only unquoted ASCII whitespace. Quotes, escapes, comments and literal
/// contents stay exact; semantic equivalence never hides extra credential text.
fn canonical_schema_sql(sql: &str) -> Result<String> {
    let mut output = String::with_capacity(sql.len());
    let mut characters = sql.chars().peekable();
    let mut quote = None;
    let mut pending_space = false;
    while let Some(character) = characters.next() {
        if let Some(end) = quote {
            output.push(character);
            if character == end {
                if end != ']' && characters.peek() == Some(&end) {
                    output.push(characters.next().unwrap());
                } else {
                    quote = None;
                }
            }
        } else if character.is_ascii_whitespace() {
            pending_space = true;
        } else {
            if pending_space && !output.is_empty() {
                output.push(' ');
            }
            pending_space = false;
            output.push(character);
            if character == '-' && characters.peek() == Some(&'-') {
                output.push(characters.next().unwrap());
                for next in characters.by_ref() {
                    output.push(next);
                    if next == '\n' {
                        break;
                    }
                }
                continue;
            }
            if character == '/' && characters.peek() == Some(&'*') {
                output.push(characters.next().unwrap());
                let mut closed = false;
                while let Some(next) = characters.next() {
                    output.push(next);
                    if next == '*' && characters.peek() == Some(&'/') {
                        output.push(characters.next().unwrap());
                        closed = true;
                        break;
                    }
                }
                ensure!(closed, "Snapshot DDL has an unterminated comment");
                continue;
            }
            quote = match character {
                '\'' | '"' | '`' => Some(character),
                '[' => Some(']'),
                _ => None,
            };
        }
    }
    ensure!(
        quote.is_none(),
        "Snapshot DDL has an unterminated quoted token"
    );
    Ok(output)
}

fn validate_documents(connection: &Connection, check: &dyn Fn() -> Result<()>) -> Result<()> {
    let mut total_bytes = 0_i64;
    let mut total_rows = 0_i64;
    for table in TABLES {
        check()?;
        let rows: i64 =
            connection.query_row(&format!("SELECT count(*) FROM {}", table.name), [], |row| {
                row.get(0)
            })?;
        total_rows = total_rows
            .checked_add(rows)
            .context("Snapshot row count overflow")?;
        ensure!(total_rows <= MAX_IMAGE_ROWS, "Snapshot row budget exceeded");
        for &(column, limit) in table.documents {
            let bytes: i64 = connection.query_row(
                &format!(
                    "SELECT coalesce(sum(length(CAST({column} AS BLOB))),0) FROM {}",
                    table.name
                ),
                [],
                |row| row.get(0),
            )?;
            total_bytes = total_bytes
                .checked_add(bytes)
                .context("Snapshot document size overflow")?;
            ensure!(
                total_bytes <= IMAGE_DOCUMENT_BUDGET,
                "Snapshot document budget exceeds 1 GiB"
            );
            visit_documents(connection, table.name, column, limit, check, |_, _| Ok(()))?;
        }
    }
    for (table, count, bytes) in [
        ("tag_write_approvals", 128, 8 * MIB),
        ("tag_edit_operations", 64, 16 * MIB),
        ("tag_edit_artifacts", 192, 12 * MIB),
        ("sync_peers", 8, 8 * 16384),
        ("physical_snapshot_operations", 68, 16 * MIB),
        ("sync_snapshots", MAX_IMAGE_ROWS, 160 * MIB),
        ("sync_journal", 1000, 64 * MIB),
        ("backup_imports", 64, 16 * MIB),
        ("native_backup_imports", 64, 16 * MIB),
        ("native_backup_archives", 64, 16 * MIB),
        ("backup_list_metadata", 1000, 16 * MIB),
        ("compat_settings", 1, 8 * MIB),
        ("download_tasks", 100, 16 * MIB),
        ("download_allocation_intents", 400, 16 * MIB),
        ("download_owned_artifacts", 400, 16 * MIB),
        ("download_destination_grants", 16, 16 * MIB),
        ("download_settings", 1, 8 * MIB),
        ("resolved_media", 4096, 8 * MIB),
        ("source_matches", 4096, 8 * MIB),
        ("artwork_info", 4096, 8 * MIB),
        ("library_scan_runs", 64, 8 * MIB),
        ("library_relink_candidates", 64, 8 * MIB),
    ] {
        check()?;
        let column = if table == "sync_journal" {
            "action_document"
        } else {
            "document"
        };
        let (rows, size): (i64, i64) = connection.query_row(
            &format!(
                "SELECT count(*),coalesce(sum(length(CAST({column} AS BLOB))),0) FROM {table}"
            ),
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        ensure!(
            rows <= count && size <= bytes,
            "Snapshot table storage budget exceeded"
        );
    }
    let download_ledger: (i64, i64, i64) = connection.query_row(
        "SELECT (SELECT count(*) FROM download_allocation_intents)+(SELECT count(*) FROM download_owned_artifacts),(SELECT coalesce(sum(byte_size),0) FROM download_allocation_intents)+(SELECT coalesce(sum(byte_size),0) FROM download_owned_artifacts),(SELECT coalesce(sum(reserved_bytes),0) FROM download_allocation_intents)+(SELECT coalesce(sum(reserved_bytes),0) FROM download_owned_artifacts)",
        [],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;
    ensure!(
        download_ledger.0 <= 400
            && download_ledger.1 <= 16 * MIB
            && download_ledger.2 <= 4 * 1024 * MIB,
        "Snapshot download allocation ledger exceeds its shared budget"
    );
    let native_archive_bytes: i64 = connection.query_row(
        "SELECT (SELECT coalesce(sum(byte_size),0) FROM native_backup_imports) + (SELECT coalesce(sum(byte_size),0) FROM native_backup_archives)",
        [],
        |row| row.get(0),
    )?;
    ensure!(
        native_archive_bytes <= 16 * MIB,
        "Snapshot native backup storage budget exceeded"
    );
    let tag_ledger_bytes:i64=connection.query_row("SELECT (SELECT coalesce(sum(byte_size),0) FROM tag_write_approvals)+(SELECT coalesce(sum(byte_size),0) FROM tag_edit_operations)+(SELECT coalesce(sum(byte_size),0) FROM tag_edit_artifacts)",[],|row|row.get(0))?;
    ensure!(
        tag_ledger_bytes <= 16 * MIB,
        "Snapshot tag ledger budget exceeded"
    );
    let (blob_rows, blob_bytes): (i64, i64) = connection.query_row(
        "SELECT count(*),coalesce(sum(length(data)),0) FROM artwork_blobs",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    ensure!(
        blob_rows <= 1024 && blob_bytes <= 64 * MIB,
        "Snapshot artwork storage budget exceeded"
    );
    for (table, count) in [
        ("local_assets", 50_000),
        ("library_membership", 200_000),
        ("library_identity_reservations", 50_000),
    ] {
        let rows: i64 =
            connection.query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
                row.get(0)
            })?;
        ensure!(rows <= count, "Snapshot library row budget exceeded");
    }
    let oversized_lyrics: bool = connection.query_row("SELECT EXISTS(SELECT 1 FROM lyric_fields GROUP BY track_id,origin HAVING sum(coalesce(length(CAST(text AS BLOB)),0))>10485760)", [], |row| row.get(0))?;
    ensure!(!oversized_lyrics, "Snapshot lyric bundle exceeds 10 MiB");
    // SQL byte-size projections are checked against physical document bytes even
    // when an image was created with CHECK enforcement disabled.
    for table in TABLES.iter().filter(|table| {
        table
            .columns
            .split_whitespace()
            .any(|column| column == "byte_size")
    }) {
        let invalid: bool = if table.name == "artwork_blobs" {
            connection.query_row(
                "SELECT EXISTS(SELECT 1 FROM artwork_blobs WHERE byte_size!=length(data))",
                [],
                |row| row.get(0),
            )?
        } else if table.name == "sync_journal" {
            connection.query_row("SELECT EXISTS(SELECT 1 FROM sync_journal WHERE byte_size!=coalesce(length(CAST(action_document AS BLOB)),0))", [], |row| row.get(0))?
        } else {
            connection.query_row(&format!("SELECT EXISTS(SELECT 1 FROM {} WHERE byte_size!=length(CAST(document AS BLOB)))", table.name), [], |row| row.get(0))?
        };
        ensure!(
            !invalid,
            "Snapshot document byte-size projection is inconsistent"
        );
    }
    Ok(())
}

/// A one-row keyset page bounds allocations independently of total image size.
fn visit_documents(
    connection: &Connection,
    table: &str,
    column: &str,
    limit: i64,
    check: &dyn Fn() -> Result<()>,
    mut visit: impl FnMut(i64, Value) -> Result<()>,
) -> Result<()> {
    let mut last = None::<i64>;
    let mut statement = connection.prepare(&format!("SELECT rowid,length(CAST({column} AS BLOB)),CASE WHEN length(CAST({column} AS BLOB))<=?2 THEN {column} END FROM {table} WHERE (?1 IS NULL OR rowid>?1) AND {column} IS NOT NULL ORDER BY rowid LIMIT 1"))?;
    // Subsequent pages use a direct rowid range so a large image cannot turn
    // the nullable first-page predicate into repeated scans from the start.
    let mut next_statement = connection.prepare(&format!("SELECT rowid,length(CAST({column} AS BLOB)),CASE WHEN length(CAST({column} AS BLOB))<=?2 THEN {column} END FROM {table} WHERE rowid>?1 AND {column} IS NOT NULL ORDER BY rowid LIMIT 1"))?;
    loop {
        check()?;
        let active = if last.is_some() {
            &mut next_statement
        } else {
            &mut statement
        };
        let row: Option<(i64, i64, Option<String>)> = active
            .query_row(params![last, limit], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })
            .optional()?;
        let Some((id, bytes, document)) = row else {
            break;
        };
        ensure!(
            bytes > 0 && bytes <= limit,
            "Snapshot document exceeds its row budget"
        );
        let document = document.context("Snapshot document could not be bounded")?;
        ensure!(
            document.len() as i64 == bytes,
            "Snapshot document byte length is inconsistent"
        );
        // A compact JSON array can have far more heap nodes than encoded
        // bytes suggest. Inspect its shape without building a Value first.
        let mut budget = JsonBudget { nodes: 0, check };
        let mut decoder = serde_json::Deserializer::from_str(&document);
        JsonShape {
            budget: &mut budget,
            depth: 0,
        }
        .deserialize(&mut decoder)?;
        decoder.end()?;
        check()?;
        let value = serde_json::from_str(&document)
            .context("Snapshot contains an invalid JSON document")?;
        visit(id, value)?;
        last = Some(id);
    }
    Ok(())
}
struct JsonBudget<'a> {
    nodes: usize,
    check: &'a dyn Fn() -> Result<()>,
}
struct JsonShape<'a, 'b> {
    budget: &'a mut JsonBudget<'b>,
    depth: usize,
}
impl<'de> DeserializeSeed<'de> for JsonShape<'_, '_> {
    type Value = ();
    fn deserialize<D: de::Deserializer<'de>>(
        self,
        decoder: D,
    ) -> std::result::Result<(), D::Error> {
        self.budget.nodes += 1;
        if self.depth > 64 || self.budget.nodes > MAX_DOCUMENT_NODES {
            return Err(de::Error::custom(
                "Snapshot JSON shape exceeds its preparation quota",
            ));
        }
        if self.budget.nodes.is_multiple_of(256) {
            (self.budget.check)().map_err(de::Error::custom)?;
        }
        decoder.deserialize_any(self)
    }
}
impl<'de> Visitor<'de> for JsonShape<'_, '_> {
    type Value = ();
    fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("bounded snapshot JSON")
    }
    fn visit_bool<E: de::Error>(self, _: bool) -> std::result::Result<(), E> {
        Ok(())
    }
    fn visit_i64<E: de::Error>(self, _: i64) -> std::result::Result<(), E> {
        Ok(())
    }
    fn visit_u64<E: de::Error>(self, _: u64) -> std::result::Result<(), E> {
        Ok(())
    }
    fn visit_f64<E: de::Error>(self, _: f64) -> std::result::Result<(), E> {
        Ok(())
    }
    fn visit_str<E: de::Error>(self, _: &str) -> std::result::Result<(), E> {
        Ok(())
    }
    fn visit_string<E: de::Error>(self, _: String) -> std::result::Result<(), E> {
        Ok(())
    }
    fn visit_unit<E: de::Error>(self) -> std::result::Result<(), E> {
        Ok(())
    }
    fn visit_none<E: de::Error>(self) -> std::result::Result<(), E> {
        Ok(())
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> std::result::Result<(), A::Error> {
        while seq
            .next_element_seed(JsonShape {
                budget: self.budget,
                depth: self.depth + 1,
            })?
            .is_some()
        {}
        Ok(())
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> std::result::Result<(), A::Error> {
        while map.next_key::<de::IgnoredAny>()?.is_some() {
            self.budget.nodes += 1;
            if self.budget.nodes > MAX_DOCUMENT_NODES {
                return Err(de::Error::custom("Snapshot JSON key quota exceeded"));
            }
            map.next_value_seed(JsonShape {
                budget: self.budget,
                depth: self.depth + 1,
            })?;
        }
        Ok(())
    }
}

fn validate_peers(connection: &Connection, check: &dyn Fn() -> Result<()>) -> Result<()> {
    visit_documents(
        connection,
        "sync_peers",
        "document",
        16384,
        check,
        |rowid, value| {
            let object = value
                .as_object()
                .context("Snapshot peer document must be an object")?;
            ensure!(
                object.keys().all(|key| matches!(
                    key.as_str(),
                    "peer_id"
                        | "client_id"
                        | "device_name"
                        | "is_mobile"
                        | "auth_epoch"
                        | "list"
                        | "dislike"
                        | "role"
                )),
                "Snapshot peer document contains unrecognized authority fields"
            );
            let (peer, client, epoch): (String, String, i64) = connection.query_row(
                "SELECT peer_id,client_id,auth_epoch FROM sync_peers WHERE rowid=?1",
                [rowid],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )?;
            ensure!(
                value["peer_id"].as_str() == Some(peer.as_str())
                    && value["client_id"].as_str() == Some(client.as_str())
                    && value["auth_epoch"].as_i64() == Some(epoch)
                    && epoch > 0
                    && value["device_name"].is_string()
                    && value["is_mobile"].is_boolean(),
                "Snapshot peer projection is inconsistent"
            );
            for field in ["list", "dislike"] {
                ensure!(
                    value[field].is_boolean() || value[field].is_null(),
                    "Snapshot peer capability metadata is invalid"
                );
            }
            Ok(())
        },
    )
}

fn sanitize_library(
    connection: &Connection,
    check: &dyn Fn() -> Result<()>,
    report: &mut SnapshotSanitizeReport,
) -> Result<()> {
    visit_documents(
        connection,
        "library_roots",
        "document",
        8 * MIB,
        check,
        |rowid, document| {
            let mut root: LibraryRoot = serde_json::from_value(document)?;
            root.validate()?;
            let (id, epoch, generation): (String, i64, i64) = connection.query_row(
                "SELECT root_id,grant_epoch,generation FROM library_roots WHERE rowid=?1",
                [rowid],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )?;
            ensure!(
                id == root.root_id
                    && epoch == i64::try_from(root.grant_epoch)?
                    && generation == i64::try_from(root.generation)?,
                "Snapshot library root projection is inconsistent"
            );
            root.watch_enabled = false;
            connection.execute(
                "UPDATE library_roots SET active=0,document=?2 WHERE rowid=?1",
                params![rowid, serde_json::to_string(&root)?],
            )?;
            report.inactive_library_roots += 1;
            Ok(())
        },
    )?;
    visit_documents(
        connection,
        "library_watch_preferences",
        "document",
        8 * MIB,
        check,
        |rowid, document| {
            let mut preference: LibraryWatchPreference = serde_json::from_value(document)?;
            preference.options.validate()?;
            let (id, generation, enabled): (String, i64, bool) = connection.query_row("SELECT root_id,watch_generation,enabled FROM library_watch_preferences WHERE rowid=?1", [rowid], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?;
            ensure!(
                id == preference.root_id
                    && generation == i64::try_from(preference.watch_generation)?
                    && enabled == preference.enabled,
                "Snapshot library watch projection is inconsistent"
            );
            preference.enabled = false;
            preference.options.auto_relink = false;
            preference.options.root_move_policy = RootMovePolicy::Pause;
            connection.execute(
                "UPDATE library_watch_preferences SET enabled=0,document=?2 WHERE rowid=?1",
                params![rowid, serde_json::to_string(&preference)?],
            )?;
            Ok(())
        },
    )?;
    visit_documents(
        connection,
        "local_assets",
        "document",
        8 * MIB,
        check,
        |rowid, document| {
            let mut asset: LocalAsset = serde_json::from_value(document)?;
            super::tracks::validate_asset(&asset)?;
            let (id, track): (String, String) = connection.query_row(
                "SELECT identity_key,track_id FROM local_assets WHERE rowid=?1",
                [rowid],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?;
            ensure!(
                id == asset.id && track == asset.track_id,
                "Snapshot local asset projection is inconsistent"
            );
            asset.available = false;
            connection.execute(
                "UPDATE local_assets SET document=?2 WHERE rowid=?1",
                params![rowid, serde_json::to_string(&asset)?],
            )?;
            Ok(())
        },
    )
}

fn sanitize_downloads(
    connection: &Connection,
    check: &dyn Fn() -> Result<()>,
    report: &mut SnapshotSanitizeReport,
) -> Result<()> {
    visit_documents(
        connection,
        "download_destination_grants",
        "document",
        8 * MIB,
        check,
        |rowid, document| {
            let mut grant: StoredDestinationGrant = serde_json::from_value(document)?;
            grant.validate()?;
            let (id, profile, epoch, revision, revoked): (String, String, i64, i64, bool) = connection.query_row("SELECT id,profile_id,epoch,revision,revoked FROM download_destination_grants WHERE rowid=?1", [rowid], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)))?;
            ensure!(
                id == grant.id
                    && profile == grant.profile_id
                    && epoch == i64::try_from(grant.epoch)?
                    && revision == i64::try_from(grant.revision)?
                    && revoked == grant.revoked,
                "Snapshot download destination projection is inconsistent"
            );
            grant.revoked = true;
            connection.execute(
                "UPDATE download_destination_grants SET revoked=1,document=?2 WHERE rowid=?1",
                params![rowid, serde_json::to_string(&grant)?],
            )?;
            report.revoked_download_destinations += 1;
            Ok(())
        },
    )?;
    visit_documents(
        connection,
        "download_settings",
        "document",
        8 * MIB,
        check,
        |rowid, document| {
            let mut settings: DownloadSettings = serde_json::from_value(document)?;
            settings.validate()?;
            settings.enabled = false;
            settings.destination_grant_id = None;
            connection.execute(
                "UPDATE download_settings SET document=?2 WHERE rowid=?1",
                params![rowid, serde_json::to_string(&settings)?],
            )?;
            Ok(())
        },
    )?;
    visit_documents(
        connection,
        "download_tasks",
        "document",
        256 * 1024,
        check,
        |rowid, document| {
            let mut task: DownloadTask = serde_json::from_value(document)?;
            task.validate()?;
            let (id, profile, grant, order, revision, epoch, state): (String, String, String, i64, i64, i64, String) = connection.query_row("SELECT id,profile_id,grant_id,position,task_revision,operation_epoch,state FROM download_tasks WHERE rowid=?1", [rowid], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?, row.get(6)?)))?;
            ensure!(
                id == task.id
                    && profile == task.profile_id
                    && grant == task.destination.grant_id
                    && order == i64::from(task.order)
                    && revision == i64::try_from(task.revision)?
                    && epoch == i64::try_from(task.operation_epoch)?
                    && serde_json::to_value(task.state)?.as_str() == Some(state.as_str()),
                "Snapshot download task projection is inconsistent"
            );
            if task.state == DownloadState::Completed {
                let metadata = CompletedDownloadMetadata {
                    task_id: task.id.clone(),
                    list_id: task.list_id.clone(),
                    list_name: task.list_name_snapshot.clone(),
                    requested_quality: task.requested_quality.as_str().into(),
                    actual_quality: task.actual_quality.as_str().into(),
                    created_at_ms: task.created_at_ms,
                    updated_at_ms: task.updated_at_ms,
                    output: task.output_audio.as_ref().map(|output| SafeArtifact {
                        basename: output.basename.clone(),
                        bytes: output.bytes,
                        sha256: output.sha256.clone(),
                    }),
                    metadata_stage: task.stages.metadata.state,
                    cover_stage: task.stages.cover.state,
                    embedded_lyrics_stage: task.stages.embedded_lyrics.state,
                    lyrics_stage: task.stages.lyrics.state,
                };
                archive_metadata(connection, "completed_download", &task.id, &metadata)?;
                report.archived_metadata_rows += 1;
            }
            task.discard_imported_authority();
            task.settings.enabled = false;
            task.settings.destination_grant_id = None;
            task.destination.collision = CollisionPolicy::Skip;
            task.validate()?;
            let document = serde_json::to_string(&task)?;
            connection.execute("UPDATE download_tasks SET state='paused',task_revision=?4,operation_epoch=?5,document=?2,byte_size=?3 WHERE rowid=?1", params![rowid, document, document.len() as i64, i64::try_from(task.revision)?, i64::try_from(task.operation_epoch)?])?;
            report.paused_download_tasks += 1;
            Ok(())
        },
    )
}

fn sanitize_artwork(
    connection: &Connection,
    check: &dyn Fn() -> Result<()>,
    report: &mut SnapshotSanitizeReport,
) -> Result<()> {
    visit_documents(
        connection,
        "artwork_info",
        "document",
        8 * MIB,
        check,
        |rowid, document| {
            let info: ArtworkInfo = serde_json::from_value(document)?;
            info.validate()?;
            let (cache_key, track, original_sha256, resolved, expiry, blob): (String, String, String, i64, i64, Option<String>) = connection.query_row("SELECT cache_key,original_track_id,original_record_sha256,resolved_at_ms,expire_at_ms,blob_sha256 FROM artwork_info WHERE rowid=?1", [rowid], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?)))?;
            ensure!(
                info.key.fingerprint()? == cache_key
                    && info.key.original_track_id == track
                    && info.key.original_record_sha256 == original_sha256
                    && i64::try_from(info.resolved_at_ms)? == resolved
                    && i64::try_from(info.expire_at_ms)? == expiry
                    && info.blob.as_ref().map(|blob| blob.sha256.as_str()) == blob.as_deref(),
                "Snapshot artwork projection is inconsistent"
            );
            let (provenance, source_sha256, policy_version) = match &info.provenance {
                ArtworkProvenance::Embedded { .. } => return Ok(()),
                ArtworkProvenance::Metadata { .. } => ("metadata", None, None),
                ArtworkProvenance::Provider { .. } => ("provider", None, None),
                ArtworkProvenance::Source {
                    source_sha256,
                    policy_version,
                } => ("source", Some(source_sha256.clone()), Some(*policy_version)),
            };
            let metadata = RemoteArtworkMetadata {
                original_track_id: info.key.original_track_id,
                original_platform: info.key.original_platform,
                original_record_sha256: info.key.original_record_sha256,
                matched_record_sha256: info.key.matched_record_sha256,
                provenance: provenance.into(),
                source_sha256,
                policy_version,
                blob: info.blob,
                resolved_at_ms: info.resolved_at_ms,
                expire_at_ms: info.expire_at_ms,
            };
            archive_metadata(connection, "remote_artwork", &cache_key, &metadata)?;
            connection.execute("DELETE FROM artwork_info WHERE rowid=?1", [rowid])?;
            report.archived_metadata_rows += 1;
            report.removed_transient_rows += 1;
            Ok(())
        },
    )
}

fn archive_exists(connection: &Connection) -> Result<bool> {
    Ok(connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name=?1)",
        [ARCHIVE_TABLE],
        |row| row.get(0),
    )?)
}

fn archive_metadata(
    connection: &Connection,
    kind: &str,
    id: &str,
    metadata: &impl Serialize,
) -> Result<()> {
    let document = serde_json::to_string(metadata)?;
    ensure!(
        document.len() <= 256 * 1024,
        "Snapshot metadata exceeds its row budget"
    );
    connection.execute("INSERT INTO snapshot_metadata_archive(kind,object_id,document,byte_size) VALUES(?1,?2,?3,?4) ON CONFLICT(kind,object_id) DO UPDATE SET document=excluded.document,byte_size=excluded.byte_size", params![kind, id, document, document.len() as i64])?;
    Ok(())
}

fn validate_metadata_archive(
    connection: &Connection,
    check: &dyn Fn() -> Result<()>,
) -> Result<()> {
    let sql: String = connection.query_row(
        "SELECT sql FROM sqlite_schema WHERE type='table' AND name=?1",
        [ARCHIVE_TABLE],
        |row| row.get(0),
    )?;
    let normalize = |text: &str| {
        text.trim_end_matches(';')
            .chars()
            .filter(|character| !character.is_whitespace())
            .flat_map(char::to_lowercase)
            .collect::<String>()
    };
    ensure!(
        normalize(&sql) == normalize(ARCHIVE_SQL),
        "Snapshot metadata table definition is invalid"
    );
    let expected = [
        ("kind", "TEXT", 1_i64, 1_i64),
        ("object_id", "TEXT", 1, 2),
        ("document", "TEXT", 1, 0),
        ("byte_size", "INTEGER", 1, 0),
    ];
    let mut statement = connection.prepare("PRAGMA table_xinfo(snapshot_metadata_archive)")?;
    let mut count = 0;
    for row in statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, i64>(3)?,
            row.get::<_, i64>(5)?,
            row.get::<_, i64>(6)?,
        ))
    })? {
        let (name, kind, not_null, primary_key, hidden) = row?;
        let expected = expected
            .get(count)
            .context("Snapshot metadata has additional cells")?;
        ensure!(
            (name.as_str(), kind.as_str(), not_null, primary_key) == *expected && hidden == 0,
            "Snapshot metadata table shape is invalid"
        );
        count += 1;
    }
    ensure!(
        count == expected.len(),
        "Snapshot metadata table is incomplete"
    );
    let (downloads, artwork, bytes, invalid): (i64, i64, i64, bool) = connection.query_row("SELECT coalesce(sum(kind='completed_download'),0),coalesce(sum(kind='remote_artwork'),0),coalesce(sum(byte_size),0),EXISTS(SELECT 1 FROM snapshot_metadata_archive WHERE kind NOT IN ('completed_download','remote_artwork') OR length(object_id) NOT BETWEEN 1 AND 1024 OR byte_size!=length(CAST(document AS BLOB)) OR byte_size NOT BETWEEN 1 AND 262144) FROM snapshot_metadata_archive", [], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)))?;
    ensure!(
        downloads <= 100 && artwork <= 4096 && bytes <= 16 * MIB && !invalid,
        "Snapshot metadata archive budget or projection is invalid"
    );
    visit_documents(
        connection,
        ARCHIVE_TABLE,
        "document",
        256 * 1024,
        check,
        |rowid, value| {
            let (kind, id): (String, String) = connection.query_row(
                "SELECT kind,object_id FROM snapshot_metadata_archive WHERE rowid=?1",
                [rowid],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?;
            if kind == "completed_download" {
                let value: CompletedDownloadMetadata = serde_json::from_value(value)?;
                ensure!(
                    value.task_id == id && value.updated_at_ms >= value.created_at_ms,
                    "Snapshot download archive identity is invalid"
                );
                if let Some(output) = value.output {
                    crate::download::validate_basename(&output.basename)?;
                    ensure!(
                        output.bytes > 0 && output.bytes <= 1024 * 1024 * 1024,
                        "Snapshot download archive size is invalid"
                    );
                    crate::domain::artwork::digest(&output.sha256)?;
                }
            } else {
                let value: RemoteArtworkMetadata = serde_json::from_value(value)?;
                ensure!(
                    !value.original_track_id.is_empty()
                        && value.original_track_id.len() <= 1024
                        && value.expire_at_ms > value.resolved_at_ms
                        && matches!(
                            value.provenance.as_str(),
                            "metadata" | "provider" | "source"
                        ),
                    "Snapshot artwork archive identity is invalid"
                );
                crate::domain::artwork::digest(&value.original_record_sha256)?;
                if let Some(blob) = value.blob {
                    blob.validate()?;
                }
            }
            Ok(())
        },
    )
}

fn sanitize_settings(
    connection: &Connection,
    check: &dyn Fn() -> Result<()>,
    report: &mut SnapshotSanitizeReport,
) -> Result<()> {
    visit_documents(
        connection,
        "compat_settings",
        "document",
        8 * MIB,
        check,
        |rowid, mut document| {
            sanitize_settings_archive(&mut document)?;
            update_sized_document(connection, "compat_settings", rowid, &document)?;
            report.scrubbed_settings_archives += 1;
            Ok(())
        },
    )?;
    visit_documents(
        connection,
        "backup_imports",
        "document",
        8 * MIB,
        check,
        |rowid, mut document| {
            let object = document
                .as_object_mut()
                .context("Snapshot backup receipt must be an object")?;
            if let Some(settings) = object
                .get_mut("settings")
                .filter(|settings| !settings.is_null())
            {
                sanitize_settings_archive(settings)?;
                report.scrubbed_settings_archives += 1;
            }
            update_sized_document(connection, "backup_imports", rowid, &document)
        },
    )
}

fn sanitize_settings_archive(document: &mut Value) -> Result<()> {
    let object = document
        .as_object_mut()
        .context("Snapshot settings archive must be an object")?;
    ensure!(
        object.keys().all(|key| matches!(
            key.as_str(),
            "archived" | "preferences" | "requires_activation"
        )),
        "Snapshot settings wrapper contains unrecognized cells"
    );
    let archived = object
        .get_mut("archived")
        .context("Snapshot settings archive is missing")?;
    ensure!(
        archived.is_object(),
        "Snapshot archived settings must be an object"
    );
    scrub_settings_value(archived, "", 0)?;
    let preview = LxSettings::from_value(archived, false)?.preview()?;
    object.insert("archived".into(), Value::Object(preview.archived));
    object.insert("preferences".into(), Value::Object(preview.preferences));
    object.insert(
        "requires_activation".into(),
        serde_json::to_value(preview.requires_activation)?,
    );
    Ok(())
}

fn normalized_setting_path(path: &str) -> String {
    path.chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

fn private_setting(path: &str) -> bool {
    let normalized = normalized_setting_path(path);
    if [
        "sourceapproval",
        "sourceapprovals",
        "urlapproval",
        "urlapprovals",
        "localfilegrant",
        "localfilegrants",
        "downloadgrant",
        "downloadgrants",
        "downloaddestinationgrant",
        "downloaddestinationgrants",
        "readgrants",
        "writegrants",
        "serviceapprovals",
    ]
    .iter()
    .any(|name| normalized == *name)
    {
        return true;
    }
    let service_namespace = path
        .split('.')
        .next()
        .map(normalized_setting_path)
        .unwrap_or_default();
    let service = matches!(service_namespace.as_str(), "sync" | "openapi" | "api")
        || (service_namespace == "network" && normalized.starts_with("networkproxy"));
    service
        && [
            "authkey",
            "privatekey",
            "sharedkey",
            "token",
            "password",
            "secret",
            "authorization",
            "cookie",
            "clientkey",
            "serverkey",
            "apikey",
        ]
        .iter()
        .any(|suffix| normalized.ends_with(suffix))
}

fn activation_setting(path: &str) -> bool {
    matches!(
        normalized_setting_path(path).as_str(),
        "commonisagreepact"
            | "isagreepact"
            | "syncenable"
            | "openapienable"
            | "openapibindlan"
            | "apienable"
            | "downloadenable"
            | "playerstartupautoplay"
    )
}

fn scrub_settings_value(value: &mut Value, path: &str, depth: usize) -> Result<()> {
    ensure!(depth <= 64, "Snapshot settings nesting exceeds its budget");
    match value {
        Value::Object(object) => {
            let keys: Vec<_> = object.keys().cloned().collect();
            for key in keys {
                let child_path = if path.is_empty() {
                    key.clone()
                } else {
                    format!("{path}.{key}")
                };
                if private_setting(&child_path) || private_setting(&key) {
                    object.remove(&key);
                } else if activation_setting(&child_path) {
                    object.insert(key, false.into());
                } else if let Some(child) = object.get_mut(&key) {
                    scrub_settings_value(child, &child_path, depth + 1)?;
                }
            }
        }
        Value::Array(values) => {
            for child in values {
                scrub_settings_value(child, path, depth + 1)?;
            }
        }
        _ => (),
    }
    Ok(())
}

fn update_sized_document(
    connection: &Connection,
    table: &str,
    rowid: i64,
    value: &Value,
) -> Result<()> {
    let document = serde_json::to_string(value)?;
    ensure!(
        document.len() as i64 <= 8 * MIB,
        "Sanitized archive exceeds its row budget"
    );
    connection.execute(
        &format!("UPDATE {table} SET document=?2,byte_size=?3 WHERE rowid=?1"),
        params![rowid, document, document.len() as i64],
    )?;
    Ok(())
}

fn validate_purged_authority(connection: &Connection) -> Result<()> {
    let pending_allocations: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM download_allocation_intents)",
        [],
        |row| row.get(0),
    )?;
    ensure!(
        !pending_allocations,
        "Sanitized snapshot retains native allocation intents"
    );
    ensure!(!connection.query_row("SELECT EXISTS(SELECT 1 FROM tag_write_approvals) OR EXISTS(SELECT 1 FROM tag_edit_operations) OR EXISTS(SELECT 1 FROM tag_edit_artifacts)",[],|row|row.get::<_,bool>(0))?,"Sanitized snapshot retains local-tag write or recovery authority");
    ensure!(
        !connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM native_backup_imports) OR EXISTS(SELECT 1 FROM native_backup_archives)",
            [],
            |row| row.get::<_, bool>(0),
        )?,
        "Sanitized snapshot still contains native preference recovery records"
    );
    ensure!(
        !connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM physical_snapshot_operations)",
            [],
            |row| row.get::<_, bool>(0)
        )?,
        "Sanitized snapshot still contains recovery authority metadata"
    );
    let state:(i64,i64,i64)=connection.query_row("SELECT profile_epoch,operation_epoch,selection_epoch FROM physical_snapshot_state WHERE singleton=1",[],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?;
    ensure!(
        state == (1, 0, 0),
        "Sanitized snapshot retains snapshot allocation epochs"
    );
    let active: bool = connection.query_row("SELECT EXISTS(SELECT 1 FROM sync_profile_state WHERE pairing_enabled!=0) OR EXISTS(SELECT 1 FROM sync_peers WHERE revoked!=1 OR private_key IS NOT NULL) OR EXISTS(SELECT 1 FROM local_file_grants) OR EXISTS(SELECT 1 FROM library_roots WHERE active!=0 OR json_extract(document,'$.watch_enabled')!=0) OR EXISTS(SELECT 1 FROM library_watch_preferences WHERE enabled!=0 OR json_extract(document,'$.enabled')!=0 OR json_extract(document,'$.options.auto_relink')!=0) OR EXISTS(SELECT 1 FROM download_destination_grants WHERE revoked!=1 OR json_extract(document,'$.revoked')!=1) OR EXISTS(SELECT 1 FROM download_settings WHERE json_extract(document,'$.enabled')!=0 OR json_extract(document,'$.destination_grant_id') IS NOT NULL) OR EXISTS(SELECT 1 FROM download_tasks WHERE state!='paused' OR json_extract(document,'$.state')!='paused' OR json_extract(document,'$.owned_part') IS NOT NULL OR json_extract(document,'$.url_bookmark') IS NOT NULL OR json_extract(document,'$.original_representation') IS NOT NULL OR json_extract(document,'$.commit_journal') IS NOT NULL OR json_extract(document,'$.output_audio') IS NOT NULL) OR EXISTS(SELECT 1 FROM download_owned_artifacts) OR EXISTS(SELECT 1 FROM resolved_media) OR EXISTS(SELECT 1 FROM source_matches)", [], |row| row.get(0))?;
    ensure!(
        !active,
        "Sanitized snapshot still contains execution authority"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        domain::{
            artwork::{ArtworkAuthority, ArtworkKey, ArtworkUrl},
            music::TrackRecord,
        },
        download::{
            DownloadContext, FileIdentity, OwnedPartRecord,
            planning::DownloadQuality,
            task_model::{
                DownloadArtifactProof, DownloadCommitJournal, DownloadCommitPhase,
                DownloadStageRecords, PrivateUrlBookmark,
            },
        },
    };
    use serde_json::json;
    use std::{cell::Cell, path::PathBuf};

    const PEER_SECRET: &str = "QUJDREVGR0hJSktMTU5PUA==";
    const SETTINGS_SECRET: &str = "private-settings-fixture-token";
    const PRIVATE_URL: &str = "https://example.invalid/audio?credential=private-url-fixture-token";

    fn fixture_path(name: &str) -> PathBuf {
        if cfg!(windows) {
            PathBuf::from(format!("C:\\snapshot-fixture\\{name}"))
        } else {
            PathBuf::from(format!("/snapshot-fixture/{name}"))
        }
    }

    fn settings_archive() -> Value {
        let settings = LxSettings::from_value(
            &json!({
                "version":"2.1.0", "player.volume":0.6, "sync.enable":true,
                "sync.authKey":SETTINGS_SECRET, "openAPI.token":SETTINGS_SECRET,
                "sourceApprovals":{"source":SETTINGS_SECRET},
                "theme.custom":{"token":"ordinary-theme-label", "color":"#c0ffee"},
                "sync":{"client":{"key":SETTINGS_SECRET},"enable":true},
                "future.preference":{"nested":[{"name":"retained"}]}
            }),
            false,
        )
        .unwrap()
        .preview()
        .unwrap();
        serde_json::to_value(super::super::backup::StoredSettings::from(&settings)).unwrap()
    }

    fn fixture() -> Connection {
        let mut connection = Connection::open_in_memory().unwrap();
        connection
            .pragma_update(None, "foreign_keys", true)
            .unwrap();
        schema::migrate(&mut connection, 0).unwrap();
        connection
            .execute("UPDATE repository_state SET revision=17", [])
            .unwrap();
        let track: TrackRecord = serde_json::from_value(json!({
            "id":"track", "lx_id":"kw_track", "title":"Preserved title", "artists":["Artist"], "album":"Album", "duration_ms":42,
            "platform":{"source":"kw","song_id":"song","qualities":["128k"]}, "local_asset_id":"asset",
            "metadata":{"future":{"token":"ordinary-track-metadata"}},
            "wire":{"version":"modern","original":{"futureWire":{"key":"retained"}}}
        })).unwrap();
        let document = serde_json::to_string(&track).unwrap();
        connection
            .execute("INSERT INTO tracks VALUES('track',?1)", [&document])
            .unwrap();
        connection
            .execute(
                "INSERT INTO music_list_items VALUES('love','kw_track','track',0,?1)",
                [&document],
            )
            .unwrap();
        connection
            .execute("INSERT INTO lyric_bundles VALUES('track','edited')", [])
            .unwrap();
        connection.execute("INSERT INTO lyric_fields VALUES('track','edited','lyric','text','[00:00] Preserved lyric')", []).unwrap();
        connection.execute("INSERT INTO change_outbox VALUES(17,'{\"revision\":17,\"details\":{\"retained\":true}}')", []).unwrap();
        let asset = LocalAsset {
            id: "asset".into(),
            track_id: "track".into(),
            path: fixture_path("song.mp3"),
            size_bytes: 128,
            modified_ms: Some(1),
            fingerprint: None,
            extension: "mp3".into(),
            available: true,
        };
        connection
            .execute(
                "INSERT INTO local_assets VALUES('asset','track',?1)",
                [serde_json::to_string(&asset).unwrap()],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO local_file_grants VALUES('asset',1,'fixture-path','{}')",
                [],
            )
            .unwrap();
        let root: LibraryRoot = serde_json::from_value(json!({"root_id":"root","path":fixture_path("library"),"recursive":true,"grant_epoch":1,"anchor":{"kind":"file","volume":"volume","file_id":"file-id"},"path_policy":"sensitive","watch_enabled":true,"generation":0,"last_complete_generation":0})).unwrap();
        connection
            .execute(
                "INSERT INTO library_roots VALUES('root','root-path',1,1,0,?1)",
                [serde_json::to_string(&root).unwrap()],
            )
            .unwrap();
        let watch = LibraryWatchPreference {
            root_id: "root".into(),
            enabled: true,
            watch_generation: 1,
            options: crate::domain::library_watch::LibraryWatchOptions {
                auto_relink: true,
                root_move_policy: RootMovePolicy::FollowSameObject,
                ..Default::default()
            },
        };
        connection
            .execute(
                "INSERT INTO library_watch_preferences VALUES('root',1,1,?1)",
                [serde_json::to_string(&watch).unwrap()],
            )
            .unwrap();
        let peer = json!({"peer_id":"peer","client_id":"client","device_name":"Device","is_mobile":false,"auth_epoch":1,"list":true,"dislike":null}).to_string();
        connection
            .execute(
                "INSERT INTO sync_peers VALUES('peer','client',1,0,?1,?2,?3)",
                params![PEER_SECRET, peer, peer.len() as i64],
            )
            .unwrap();
        connection
            .execute("UPDATE sync_profile_state SET pairing_enabled=1", [])
            .unwrap();
        let archive = settings_archive().to_string();
        connection
            .execute(
                "INSERT INTO compat_settings VALUES(1,17,?1,?2)",
                params![archive, archive.len() as i64],
            )
            .unwrap();
        let receipt = json!({"operation_id":"import","input_digest":"a".repeat(64),"expected_revision":16,"revision":17,"kind":"settings","migrated_legacy":false,"metadata":[],"settings":settings_archive(),"preferences_applied":true,"change":{"revision":17,"kind":"lists_changed","track_id":null,"details":{"origin":"lx_backup","operation_id":"import","input_digest":"a".repeat(64)}}}).to_string();
        connection
            .execute(
                "INSERT INTO backup_imports VALUES('import',?1,16,17,?2,?3)",
                params!["a".repeat(64), receipt, receipt.len() as i64],
            )
            .unwrap();
        let identity = FileIdentity {
            device: 1,
            object: "aabb".into(),
        };
        let grant = StoredDestinationGrant {
            id: "destination".into(),
            profile_id: "profile".into(),
            path: fixture_path("downloads"),
            identity: identity.clone(),
            epoch: 1,
            revision: 1,
            revoked: false,
        };
        connection
            .execute(
                "INSERT INTO download_destination_grants VALUES('destination','profile',1,1,0,?1)",
                [serde_json::to_string(&grant).unwrap()],
            )
            .unwrap();
        let settings = DownloadSettings {
            enabled: true,
            destination_grant_id: Some("destination".into()),
            ..Default::default()
        };
        connection
            .execute(
                "INSERT INTO download_settings VALUES(1,?1)",
                [serde_json::to_string(&settings).unwrap()],
            )
            .unwrap();
        let mut stages = DownloadStageRecords::for_settings(&settings, DownloadQuality::K128);
        stages.metadata.state = DownloadStageState::Succeeded;
        stages.cover.state = DownloadStageState::Succeeded;
        let output = DownloadArtifactProof {
            basename: "song.mp3".into(),
            identity: identity.clone(),
            bytes: 128,
            sha256: "b".repeat(64),
        };
        stages.cover.artifact = Some(output.clone());
        let part = OwnedPartRecord {
            basename: format!(".lx-download-{}.part", "c".repeat(32)),
            nonce: "c".repeat(32),
            identity: identity.clone(),
            destination_identity: identity.clone(),
            destination_epoch: 1,
            final_basename: "song.mp3".into(),
        };
        let context = DownloadContext {
            profile_id: "profile".into(),
            task_id: "task".into(),
            operation_epoch: 1,
            source_id: "source".into(),
            source_content_sha256: "d".repeat(64),
            policy_epoch: 1,
        };
        let task = DownloadTask {
            id: "task".into(),
            profile_id: "profile".into(),
            original_record: track.clone(),
            resolved_record: Some(track.clone()),
            requested_quality: DownloadQuality::K128,
            actual_quality: DownloadQuality::K128,
            list_id: Some("love".into()),
            list_name_snapshot: Some("Favorites".into()),
            order: 0,
            revision: 7,
            operation_epoch: 1,
            settings,
            destination: crate::download::task_model::DownloadDestinationBinding {
                grant_id: "destination".into(),
                grant_epoch: 1,
                root_identity: identity,
                relative_components: vec!["song.mp3".into()],
                collision: CollisionPolicy::Skip,
            },
            state: DownloadState::Completed,
            pause_reason: None,
            owned_part: Some(part.clone()),
            original_transfer: None,
            original_representation: None,
            url_bookmark: Some(PrivateUrlBookmark::new(PRIVATE_URL.into(), context).unwrap()),
            transfer_attempts: 1,
            retry_at_ms: None,
            error: None,
            output_audio: Some(output.clone()),
            audio_available: true,
            stages,
            commit_journal: Some(DownloadCommitJournal {
                task_revision: 7,
                operation_epoch: 1,
                grant_epoch: 1,
                phase: DownloadCommitPhase::AudioPublished,
                original_part: part,
                candidate: output,
                observed_target: None,
                rollback_original: None,
                sidecar: None,
                retry: None,
            }),
            created_at_ms: 1,
            updated_at_ms: 2,
        };
        task.validate().unwrap();
        let document = serde_json::to_string(&task).unwrap();
        connection.execute("INSERT INTO download_tasks VALUES('task','profile','destination',0,7,1,'completed',?1,?2)", params![document,document.len() as i64]).unwrap();
        let info = ArtworkInfo::new(
            ArtworkKey::new(&track, None, ArtworkAuthority::Native).unwrap(),
            ArtworkProvenance::Metadata {
                platform: "kw".into(),
            },
            Some(
                ArtworkUrl::new(
                    "https://example.invalid/cover?credential=private-artwork-fixture-token",
                )
                .unwrap(),
            ),
            None,
            1,
        )
        .unwrap();
        let document = serde_json::to_string(&info).unwrap();
        connection
            .execute(
                "INSERT INTO artwork_info VALUES(?1,'track',?2,?3,?4,?3,?5,NULL,?6)",
                params![
                    info.key.fingerprint().unwrap(),
                    info.key.original_record_sha256,
                    info.resolved_at_ms as i64,
                    info.expire_at_ms as i64,
                    document.len() as i64,
                    document
                ],
            )
            .unwrap();
        connection
    }

    fn text_at(connection: &Connection, table: &str) -> String {
        connection
            .query_row(
                &format!("SELECT document FROM {table} LIMIT 1"),
                [],
                |row| row.get(0),
            )
            .unwrap()
    }

    #[test]
    fn purge_revokes_authority_preserves_user_documents_and_archives_history() {
        let mut connection = fixture();
        let preserved = ["tracks", "music_list_items", "change_outbox"]
            .map(|table| (table, text_at(&connection, table)));
        let report = purge_snapshot_authority(&mut connection, &|| Ok(())).unwrap();
        assert_eq!(report.archived_metadata_rows, 2);
        validate_purged_authority(&connection).unwrap();
        schema::integrity_check(&connection).unwrap();
        for (table, before) in preserved {
            assert_eq!(text_at(&connection, table), before);
        }
        assert_eq!(
            connection
                .query_row("SELECT revision FROM repository_state", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            17
        );
        let task: DownloadTask =
            serde_json::from_str(&text_at(&connection, "download_tasks")).unwrap();
        task.validate().unwrap();
        assert_eq!(task.state, DownloadState::Paused);
        assert!(
            task.owned_part.is_none()
                && task.url_bookmark.is_none()
                && task.output_audio.is_none()
                && task.commit_journal.is_none()
        );
        assert_eq!(
            task.original_record.metadata["future"]["token"],
            "ordinary-track-metadata"
        );
        let settings: super::super::backup::StoredSettings =
            serde_json::from_str(&text_at(&connection, "compat_settings")).unwrap();
        let preview = settings.checked_preview().unwrap();
        assert_eq!(
            preview.archived["theme.custom"]["token"],
            "ordinary-theme-label"
        );
        assert_eq!(
            preview.archived["future.preference"]["nested"][0]["name"],
            "retained"
        );
        assert_eq!(preview.archived["sync.enable"], false);
        for table in ["compat_settings", "backup_imports", "download_tasks"] {
            let document = text_at(&connection, table);
            assert!(
                !document.contains(SETTINGS_SECRET)
                    && !document.contains("private-url-fixture-token")
            );
        }
        let archive: String = connection
            .query_row(
                "SELECT group_concat(document) FROM snapshot_metadata_archive",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(archive.contains("song.mp3"));
        assert!(
            !archive.contains("credential")
                && !archive.contains("private-artwork-fixture-token")
                && !archive.contains("root_identity")
        );
        assert_eq!(
            connection
                .query_row("SELECT text FROM lyric_fields", [], |row| row
                    .get::<_, String>(0))
                .unwrap(),
            "[00:00] Preserved lyric"
        );
        purge_snapshot_authority(&mut connection, &|| Ok(())).unwrap();
        assert_eq!(
            connection
                .query_row(
                    "SELECT count(*) FROM snapshot_metadata_archive",
                    [],
                    |row| row.get::<_, i64>(0)
                )
                .unwrap(),
            2
        );
    }

    #[test]
    fn saturated_epochs_still_disable_pairing() {
        let mut connection = fixture();
        connection.execute("UPDATE sync_profile_state SET profile_epoch=9223372036854775807,pairing_epoch=9223372036854775807",[]).unwrap();
        purge_snapshot_authority(&mut connection, &|| Ok(())).unwrap();
        let state: (i64, i64, bool) = connection
            .query_row(
                "SELECT profile_epoch,pairing_epoch,pairing_enabled FROM sync_profile_state",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(state, (i64::MAX, i64::MAX, false));
    }
    #[test]
    fn native_restore_receipts_and_source_archives_are_removed_from_private_image() {
        let mut connection = fixture();
        let receipt =
            json!({"preferences_applied":false,"private":"native-pending-secret"}).to_string();
        let archive = json!({"sources":[{"script":"native-source-secret"}],"extensions":{"token":"native-extension-secret"}}).to_string();
        connection
            .execute(
                "INSERT INTO native_backup_imports VALUES('native-import',?1,16,17,0,?2,?3)",
                params!["a".repeat(64), receipt, receipt.len() as i64],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO native_backup_archives VALUES('native-import',?1,?2,?3)",
                params!["b".repeat(64), archive, archive.len() as i64],
            )
            .unwrap();
        let tracks = text_at(&connection, "tracks");
        purge_snapshot_authority(&mut connection, &|| Ok(())).unwrap();
        validate_purged_authority(&connection).unwrap();
        schema::integrity_check(&connection).unwrap();
        assert_eq!(text_at(&connection, "tracks"), tracks);
        for table in ["native_backup_imports", "native_backup_archives"] {
            assert_eq!(
                connection
                    .query_row(&format!("SELECT count(*) FROM {table}"), [], |row| row
                        .get::<_, i64>(0))
                    .unwrap(),
                0
            );
        }
    }

    #[test]
    fn production_schema_matches_all_explicit_and_implicit_indexes() {
        let connection = fixture();
        validate_schema(&connection, &|| Ok(())).unwrap();
        let objects = schema_objects(&connection, &|| Ok(())).unwrap();
        assert!(
            objects
                .values()
                .any(|object| object.kind == "index" && object.sql.is_none())
        );
        assert!(objects.contains_key("tracks_lx_identity"));
    }
    #[test]
    fn extra_expression_index_with_credential_literal_is_rejected_before_purge() {
        let mut connection = fixture();
        connection.execute_batch("CREATE INDEX extra_credential ON search_history((word || 'schema-private-token-fixture'))").unwrap();
        assert!(purge_snapshot_authority(&mut connection, &|| Ok(())).is_err());
        assert_eq!(
            connection
                .query_row("SELECT private_key FROM sync_peers", [], |row| row
                    .get::<_, String>(0))
                .unwrap(),
            PEER_SECRET
        );
        assert!(
            connection
                .query_row(
                    "SELECT sql FROM sqlite_schema WHERE name='extra_credential'",
                    [],
                    |row| row.get::<_, String>(0)
                )
                .unwrap()
                .contains("schema-private-token-fixture")
        );
    }
    #[test]
    fn changed_known_check_and_default_ddl_are_rejected_with_same_column_names() {
        for ddl in [
            "CREATE TABLE search_history(word TEXT PRIMARY KEY NOT NULL,position INTEGER NOT NULL CHECK(position>=0 AND 'schema-private-token-fixture'!=''))",
            "CREATE TABLE search_history(word TEXT PRIMARY KEY NOT NULL DEFAULT 'schema-private-token-fixture',position INTEGER NOT NULL CHECK(position>=0))",
        ] {
            let mut connection = fixture();
            connection
                .execute_batch("DROP TABLE search_history")
                .unwrap();
            connection.execute_batch(ddl).unwrap();
            assert!(purge_snapshot_authority(&mut connection, &|| Ok(())).is_err());
            assert_eq!(
                connection
                    .query_row("SELECT private_key FROM sync_peers", [], |row| row
                        .get::<_, String>(0))
                    .unwrap(),
                PEER_SECRET
            );
        }
    }
    #[test]
    fn sql_canonicalization_preserves_literals_quoted_names_and_comments() {
        assert_eq!(
            canonical_schema_sql(" CREATE  TABLE x (a TEXT DEFAULT 'secret  value') ").unwrap(),
            "CREATE TABLE x (a TEXT DEFAULT 'secret  value')"
        );
        assert_ne!(
            canonical_schema_sql("CREATE TABLE x (a DEFAULT 'secret value')").unwrap(),
            canonical_schema_sql("CREATE TABLE x (a DEFAULT 'secret  value')").unwrap()
        );
        assert_eq!(
            canonical_schema_sql("CREATE /* 'private  text' */ TABLE `x``y` (a TEXT)").unwrap(),
            "CREATE /* 'private  text' */ TABLE `x``y` (a TEXT)"
        );
    }
    #[test]
    fn dense_json_nodes_fail_before_authority_changes_while_large_text_is_preserved() {
        let mut dense = fixture();
        let document = text_at(&dense, "tracks");
        let mut value: Value = serde_json::from_str(&document).unwrap();
        value["metadata"]["dense"] = Value::Array(vec![Value::Null; MAX_DOCUMENT_NODES]);
        dense
            .execute(
                "UPDATE tracks SET document=?1",
                [serde_json::to_string(&value).unwrap()],
            )
            .unwrap();
        assert!(purge_snapshot_authority(&mut dense, &|| Ok(())).is_err());
        assert_eq!(
            dense
                .query_row("SELECT private_key FROM sync_peers", [], |row| row
                    .get::<_, String>(0))
                .unwrap(),
            PEER_SECRET
        );
        let mut text = fixture();
        let mut value: Value = serde_json::from_str(&text_at(&text, "tracks")).unwrap();
        let payload = "opaque".repeat(100_000);
        value["metadata"]["largeText"] = payload.clone().into();
        text.execute(
            "UPDATE tracks SET document=?1",
            [serde_json::to_string(&value).unwrap()],
        )
        .unwrap();
        purge_snapshot_authority(&mut text, &|| Ok(())).unwrap();
        let value: Value = serde_json::from_str(&text_at(&text, "tracks")).unwrap();
        assert_eq!(value["metadata"]["largeText"], payload);
    }

    #[test]
    fn invalid_projection_rolls_back_all_purges() {
        let mut connection = fixture();
        connection
            .execute("UPDATE library_roots SET generation=1", [])
            .unwrap();
        assert!(purge_snapshot_authority(&mut connection, &|| Ok(())).is_err());
        assert_eq!(
            connection
                .query_row("SELECT private_key FROM sync_peers", [], |row| row
                    .get::<_, String>(0))
                .unwrap(),
            PEER_SECRET
        );
        assert!(
            connection
                .query_row(
                    "SELECT pairing_enabled FROM sync_profile_state",
                    [],
                    |row| row.get::<_, bool>(0)
                )
                .unwrap()
        );
        assert_eq!(
            connection
                .query_row("SELECT count(*) FROM local_file_grants", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            1
        );
        assert!(!archive_exists(&connection).unwrap());
    }

    #[test]
    fn cancellation_during_purge_rolls_back() {
        let total = Cell::new(0_u32);
        purge_snapshot_authority(&mut fixture(), &|| {
            total.set(total.get() + 1);
            Ok(())
        })
        .unwrap();
        let cutoff = total.get() - 1;
        let mut connection = fixture();
        let calls = Cell::new(0_u32);
        let cancelled = purge_snapshot_authority(&mut connection, &|| {
            let next = calls.get() + 1;
            calls.set(next);
            ensure!(next < cutoff, "Fixture cancellation");
            Ok(())
        });
        assert!(cancelled.is_err());
        assert_eq!(
            connection
                .query_row("SELECT private_key FROM sync_peers", [], |row| row
                    .get::<_, String>(0))
                .unwrap(),
            PEER_SECRET
        );
        assert!(text_at(&connection, "download_tasks").contains(PRIVATE_URL));
    }

    #[test]
    fn unknown_schema_and_archive_fields_are_rejected() {
        let mut connection = fixture();
        connection
            .execute_batch("CREATE TABLE unexpected_permission(token TEXT)")
            .unwrap();
        assert!(purge_snapshot_authority(&mut connection, &|| Ok(())).is_err());
        connection.execute_batch("DROP TABLE unexpected_permission; CREATE TRIGGER unexpected_trigger AFTER UPDATE ON sync_peers BEGIN UPDATE sync_profile_state SET pairing_enabled=1; END").unwrap();
        assert!(purge_snapshot_authority(&mut connection, &|| Ok(())).is_err());
        connection
            .execute_batch("DROP TRIGGER unexpected_trigger")
            .unwrap();
        purge_snapshot_authority(&mut connection, &|| Ok(())).unwrap();
        connection.execute("UPDATE snapshot_metadata_archive SET document=json_set(document,'$.private_key','secret'),byte_size=length(CAST(json_set(document,'$.private_key','secret') AS BLOB))",[]).unwrap();
        assert!(purge_snapshot_authority(&mut connection, &|| Ok(())).is_err());
    }

    #[test]
    fn compacted_private_image_removes_secret_body_bytes_and_leaves_live_cells() {
        let live = fixture();
        let folder = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
        let path = folder.path().join("private.sqlite3");
        live.backup("main", &path, None).unwrap();
        let mut private = Connection::open(&path).unwrap();
        purge_snapshot_authority(&mut private, &|| Ok(())).unwrap();
        private.execute_batch("VACUUM").unwrap();
        schema::integrity_check(&private).unwrap();
        drop(private);
        let bytes = std::fs::read(&path).unwrap();
        for secret in [
            PEER_SECRET,
            SETTINGS_SECRET,
            "private-url-fixture-token",
            "private-artwork-fixture-token",
        ] {
            assert!(
                !bytes
                    .windows(secret.len())
                    .any(|window| window == secret.as_bytes())
            );
        }
        assert_eq!(
            live.query_row("SELECT private_key FROM sync_peers", [], |row| row
                .get::<_, String>(0))
                .unwrap(),
            PEER_SECRET
        );
        assert!(text_at(&live, "compat_settings").contains(SETTINGS_SECRET));
        assert_eq!(
            live.query_row("SELECT revision FROM repository_state", [], |row| row
                .get::<_, i64>(0))
                .unwrap(),
            17
        );
        assert!(
            live.query_row(
                "SELECT pairing_enabled FROM sync_profile_state",
                [],
                |row| row.get::<_, bool>(0)
            )
            .unwrap()
        );
    }
}

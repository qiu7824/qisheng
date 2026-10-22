use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, TransactionBehavior};

pub const SCHEMA_VERSION: u32 = 17;

const VERSION_16: &str = r#"
CREATE TABLE tag_write_approvals (
 approval_id TEXT PRIMARY KEY NOT NULL CHECK(length(approval_id) BETWEEN 1 AND 256),
 asset_id TEXT NOT NULL REFERENCES local_assets(identity_key) ON DELETE RESTRICT,
 approval_epoch INTEGER NOT NULL UNIQUE CHECK(approval_epoch>0),
 revoked INTEGER NOT NULL CHECK(revoked IN (0,1)),
 document TEXT NOT NULL CHECK(json_valid(document)),
 byte_size INTEGER NOT NULL CHECK(byte_size BETWEEN 1 AND 65536 AND byte_size=length(CAST(document AS BLOB)))
);
CREATE TABLE tag_edit_operations (
 operation_id TEXT PRIMARY KEY NOT NULL CHECK(length(operation_id) BETWEEN 1 AND 256),
 approval_id TEXT NOT NULL REFERENCES tag_write_approvals(approval_id) ON DELETE RESTRICT,
 asset_id TEXT NOT NULL REFERENCES local_assets(identity_key) ON DELETE RESTRICT,
 operation_epoch INTEGER NOT NULL UNIQUE CHECK(operation_epoch>0),
 ledger_revision INTEGER NOT NULL CHECK(ledger_revision>0),
 phase TEXT NOT NULL CHECK(phase IN ('registered','prepared','backup_sealed','candidate_verified','installing','disk_verified','repo_committed','published','restoring','restored')),
 document TEXT NOT NULL CHECK(json_valid(document)),
 byte_size INTEGER NOT NULL CHECK(byte_size BETWEEN 1 AND 1048576 AND byte_size=length(CAST(document AS BLOB)))
);
CREATE INDEX tag_edit_pending_asset ON tag_edit_operations(asset_id,phase);
CREATE TABLE tag_edit_artifacts (
 operation_id TEXT NOT NULL REFERENCES tag_edit_operations(operation_id) ON DELETE RESTRICT,
 role TEXT NOT NULL CHECK(role IN ('journal','backup','candidate')),
 phase TEXT NOT NULL CHECK(phase IN ('reserved','allocated','sealed','retiring','retired')),
 actual_bytes INTEGER NOT NULL CHECK(actual_bytes>=0),
 reserved_bytes INTEGER NOT NULL CHECK(reserved_bytes>=0),
 document TEXT NOT NULL CHECK(json_valid(document)),
 byte_size INTEGER NOT NULL CHECK(byte_size BETWEEN 1 AND 65536 AND byte_size=length(CAST(document AS BLOB))),
 PRIMARY KEY(operation_id,role)
);
"#;

const VERSION_15: &str = r#"
CREATE TABLE native_backup_imports (
    operation_id TEXT PRIMARY KEY NOT NULL CHECK(length(operation_id) BETWEEN 1 AND 256),
    input_digest TEXT NOT NULL CHECK(length(input_digest)=64),
    expected_revision INTEGER NOT NULL CHECK(expected_revision>=0),
    revision INTEGER NOT NULL CHECK(revision=expected_revision+1),
    preferences_applied INTEGER NOT NULL CHECK(preferences_applied IN (0,1)),
    document TEXT NOT NULL CHECK(json_valid(document)),
    byte_size INTEGER NOT NULL CHECK(byte_size BETWEEN 1 AND 8388608 AND byte_size=length(CAST(document AS BLOB)))
);
CREATE TABLE native_backup_archives (
    operation_id TEXT PRIMARY KEY NOT NULL REFERENCES native_backup_imports(operation_id) ON DELETE CASCADE,
    sha256 TEXT NOT NULL CHECK(length(sha256)=64),
    document TEXT NOT NULL CHECK(json_valid(document)),
    byte_size INTEGER NOT NULL CHECK(byte_size BETWEEN 1 AND 8388608 AND byte_size=length(CAST(document AS BLOB)))
);
"#;
pub const APPLICATION_ID: i32 = 0x4c584e41;
const VERSION_14: &str = r#"
CREATE TABLE physical_snapshot_state (
 singleton INTEGER PRIMARY KEY CHECK(singleton=1),
 profile_id TEXT NOT NULL CHECK(length(profile_id)=32),
 profile_epoch INTEGER NOT NULL CHECK(profile_epoch>0),
 operation_epoch INTEGER NOT NULL CHECK(operation_epoch>=0),
 selection_epoch INTEGER NOT NULL CHECK(selection_epoch>=0)
);
INSERT INTO physical_snapshot_state VALUES(1,lower(hex(randomblob(16))),1,0,0);
CREATE TABLE physical_snapshot_operations (
 operation_id TEXT PRIMARY KEY NOT NULL CHECK(length(operation_id) BETWEEN 1 AND 256),
 profile_id TEXT NOT NULL CHECK(length(profile_id)=32),
 profile_epoch INTEGER NOT NULL CHECK(profile_epoch>0),
 operation_epoch INTEGER NOT NULL CHECK(operation_epoch>0),
 selection_epoch INTEGER NOT NULL CHECK(selection_epoch>0),
 expected_snapshot_revision INTEGER NOT NULL CHECK(expected_snapshot_revision>=0),
 maximum_image_bytes INTEGER NOT NULL CHECK(maximum_image_bytes BETWEEN 1 AND 1073741824),
 request_digest TEXT NOT NULL CHECK(length(request_digest)=64),
 ledger_revision INTEGER NOT NULL CHECK(ledger_revision>0),
 state TEXT NOT NULL CHECK(state IN ('registered','receipt_committed','finished')),
 document TEXT NOT NULL CHECK(json_valid(document)),
 byte_size INTEGER NOT NULL CHECK(byte_size BETWEEN 1 AND 1048576 AND byte_size=length(CAST(document AS BLOB))),
 UNIQUE(profile_id,profile_epoch,operation_epoch),UNIQUE(profile_id,profile_epoch,selection_epoch)
);
CREATE INDEX physical_snapshot_pending ON physical_snapshot_operations(state,operation_epoch);
"#;

// Version 1 is the minimal native track/library schema, not an LX database.
const VERSION_1: &str = r#"
CREATE TABLE tracks (
    id TEXT PRIMARY KEY NOT NULL,
    document TEXT NOT NULL CHECK(json_valid(document))
);
CREATE TABLE local_assets (
    identity_key TEXT PRIMARY KEY NOT NULL,
    track_id TEXT NOT NULL UNIQUE REFERENCES tracks(id) ON DELETE CASCADE,
    document TEXT NOT NULL CHECK(json_valid(document))
);
"#;

const VERSION_2: &str = r#"
CREATE TABLE lyric_bundles (
    track_id TEXT NOT NULL REFERENCES tracks(id) ON DELETE CASCADE,
    origin TEXT NOT NULL CHECK(origin IN ('raw','edited')),
    PRIMARY KEY(track_id, origin)
);
CREATE TABLE lyric_fields (
    track_id TEXT NOT NULL,
    origin TEXT NOT NULL,
    kind TEXT NOT NULL CHECK(kind IN ('lyric','tlyric','rlyric','lxlyric')),
    state TEXT NOT NULL CHECK(state IN ('missing','null','text')),
    text TEXT,
    PRIMARY KEY(track_id, origin, kind),
    FOREIGN KEY(track_id, origin) REFERENCES lyric_bundles(track_id, origin) ON DELETE CASCADE,
    CHECK((state = 'text' AND text IS NOT NULL) OR (state != 'text' AND text IS NULL))
);
CREATE TABLE repository_state (
    singleton INTEGER PRIMARY KEY CHECK(singleton = 1),
    revision INTEGER NOT NULL CHECK(revision >= 0)
);
INSERT INTO repository_state(singleton, revision) VALUES(1, 0);
CREATE TABLE change_outbox (
    revision INTEGER PRIMARY KEY,
    document TEXT NOT NULL CHECK(json_valid(document))
);
CREATE TABLE import_markers (
    name TEXT PRIMARY KEY NOT NULL,
    source_digest TEXT NOT NULL,
    revision INTEGER NOT NULL
);
"#;

const VERSION_3: &str = r#"
CREATE TABLE profile_metadata (
    singleton INTEGER PRIMARY KEY CHECK(singleton = 1),
    digest TEXT NOT NULL CHECK(length(digest) = 64),
    change_document TEXT NOT NULL CHECK(json_valid(change_document))
);
"#;

const VERSION_4: &str = r#"
CREATE TABLE music_lists (
    id TEXT PRIMARY KEY NOT NULL,
    kind TEXT NOT NULL CHECK(kind IN ('builtin','user')),
    position INTEGER NOT NULL CHECK(position >= 0),
    document TEXT NOT NULL CHECK(json_valid(document)),
    scroll_position REAL NOT NULL DEFAULT 0 CHECK(scroll_position >= 0),
    update_document TEXT CHECK(update_document IS NULL OR json_valid(update_document)),
    CHECK((kind='builtin' AND id IN ('default','love','temp')) OR (kind='user' AND id NOT IN ('default','love','temp')))
);
INSERT INTO music_lists(id,kind,position,document) VALUES
('default','builtin',0,'{"id":"default","name":"list__name_default","location_update_time":null}'),
('love','builtin',1,'{"id":"love","name":"list__name_love","location_update_time":null}'),
('temp','builtin',2,'{"id":"temp","name":"临时列表","location_update_time":null}');
CREATE TABLE music_list_items (
    list_id TEXT NOT NULL REFERENCES music_lists(id) ON DELETE CASCADE,
    music_id TEXT NOT NULL,
    track_id TEXT NOT NULL REFERENCES tracks(id) ON DELETE CASCADE,
    position INTEGER NOT NULL CHECK(position >= 0),
    document TEXT NOT NULL CHECK(json_valid(document)),
    PRIMARY KEY(list_id,music_id)
);
CREATE INDEX music_list_items_order ON music_list_items(list_id,position);
CREATE INDEX music_list_items_track ON music_list_items(track_id);
CREATE TABLE dislike_state(singleton INTEGER PRIMARY KEY CHECK(singleton=1),rules TEXT NOT NULL);
INSERT INTO dislike_state VALUES(1,'');
CREATE TABLE search_history(word TEXT PRIMARY KEY NOT NULL,position INTEGER NOT NULL CHECK(position >= 0));
"#;

const VERSION_5: &str = r#"
CREATE TABLE playback_state (
    singleton INTEGER PRIMARY KEY CHECK(singleton=1),
    document TEXT NOT NULL CHECK(json_valid(document))
);
"#;

const VERSION_6: &str = r#"
CREATE TABLE resolved_media (
    cache_key TEXT PRIMARY KEY NOT NULL CHECK(length(cache_key)=64),
    original_track_id TEXT NOT NULL CHECK(length(original_track_id) BETWEEN 1 AND 1024),
    original_platform TEXT NOT NULL CHECK(length(original_platform) BETWEEN 1 AND 64),
    actual_quality TEXT NOT NULL CHECK(length(actual_quality) BETWEEN 1 AND 64),
    source_sha256 TEXT NOT NULL CHECK(length(source_sha256)=64),
    source_policy_version INTEGER NOT NULL CHECK(source_policy_version>0),
    music_info_sha256 TEXT NOT NULL CHECK(length(music_info_sha256)=64),
    resolved_at_ms INTEGER NOT NULL CHECK(resolved_at_ms>=0),
    expire_at_ms INTEGER NOT NULL CHECK(expire_at_ms>resolved_at_ms),
    last_access_ms INTEGER NOT NULL CHECK(last_access_ms>=0),
    byte_size INTEGER NOT NULL CHECK(byte_size BETWEEN 1 AND 8388608),
    document TEXT NOT NULL CHECK(json_valid(document)),
    CHECK(byte_size=length(CAST(document AS BLOB))),
    UNIQUE(original_track_id,original_platform,actual_quality,source_sha256,source_policy_version,music_info_sha256)
);
CREATE INDEX resolved_media_track ON resolved_media(original_track_id);
CREATE INDEX resolved_media_source ON resolved_media(source_sha256,source_policy_version);
CREATE INDEX resolved_media_expiry ON resolved_media(expire_at_ms);
CREATE INDEX resolved_media_lru ON resolved_media(last_access_ms,cache_key);
CREATE TABLE source_matches (
    cache_key TEXT PRIMARY KEY NOT NULL CHECK(length(cache_key)=64),
    original_track_id TEXT NOT NULL CHECK(length(original_track_id) BETWEEN 1 AND 1024),
    original_platform TEXT NOT NULL CHECK(length(original_platform) BETWEEN 1 AND 64),
    original_record_sha256 TEXT NOT NULL CHECK(length(original_record_sha256)=64),
    algorithm_version INTEGER NOT NULL CHECK(algorithm_version>0),
    matched_at_ms INTEGER NOT NULL CHECK(matched_at_ms>=0),
    expire_at_ms INTEGER NOT NULL CHECK(expire_at_ms>matched_at_ms),
    last_access_ms INTEGER NOT NULL CHECK(last_access_ms>=0),
    byte_size INTEGER NOT NULL CHECK(byte_size BETWEEN 1 AND 8388608),
    document TEXT NOT NULL CHECK(json_valid(document)),
    CHECK(byte_size=length(CAST(document AS BLOB))),
    UNIQUE(original_track_id,original_platform,original_record_sha256,algorithm_version)
);
CREATE INDEX source_matches_track ON source_matches(original_track_id);
CREATE INDEX source_matches_expiry ON source_matches(expire_at_ms);
CREATE INDEX source_matches_lru ON source_matches(last_access_ms,cache_key);
"#;

const VERSION_7: &str = r#"
CREATE TABLE artwork_blobs (
    sha256 TEXT PRIMARY KEY NOT NULL CHECK(length(sha256)=64),
    byte_size INTEGER NOT NULL CHECK(byte_size BETWEEN 1 AND 8388608),
    last_access_ms INTEGER NOT NULL CHECK(last_access_ms>=0),
    document TEXT NOT NULL CHECK(json_valid(document)),
    data BLOB NOT NULL CHECK(length(data)=byte_size)
);
CREATE INDEX artwork_blobs_lru ON artwork_blobs(last_access_ms,sha256);
CREATE TABLE artwork_info (
    cache_key TEXT PRIMARY KEY NOT NULL CHECK(length(cache_key)=64),
    original_track_id TEXT NOT NULL,
    original_record_sha256 TEXT NOT NULL CHECK(length(original_record_sha256)=64),
    resolved_at_ms INTEGER NOT NULL CHECK(resolved_at_ms>=0),
    expire_at_ms INTEGER NOT NULL CHECK(expire_at_ms>resolved_at_ms),
    last_access_ms INTEGER NOT NULL CHECK(last_access_ms>=0),
    byte_size INTEGER NOT NULL CHECK(byte_size BETWEEN 1 AND 8388608),
    blob_sha256 TEXT REFERENCES artwork_blobs(sha256) ON DELETE SET NULL,
    document TEXT NOT NULL CHECK(json_valid(document)),
    CHECK(byte_size=length(CAST(document AS BLOB)))
);
CREATE INDEX artwork_info_lru ON artwork_info(last_access_ms,cache_key);
CREATE INDEX artwork_info_track ON artwork_info(original_track_id);
"#;

const VERSION_8: &str = r#"
CREATE TABLE library_roots (
    root_id TEXT PRIMARY KEY NOT NULL,
    path_key TEXT NOT NULL UNIQUE,
    active INTEGER NOT NULL CHECK(active IN (0,1)),
    grant_epoch INTEGER NOT NULL CHECK(grant_epoch>0),
    generation INTEGER NOT NULL CHECK(generation>=0),
    document TEXT NOT NULL CHECK(json_valid(document))
);
CREATE TABLE local_file_grants (
    asset_id TEXT PRIMARY KEY NOT NULL REFERENCES local_assets(identity_key) ON DELETE CASCADE,
    epoch INTEGER NOT NULL CHECK(epoch>0)
);
CREATE TABLE local_asset_locations (
    asset_id TEXT PRIMARY KEY NOT NULL REFERENCES local_assets(identity_key) ON DELETE CASCADE,
    path_key TEXT NOT NULL UNIQUE,
    document TEXT NOT NULL CHECK(json_valid(document))
);
CREATE TABLE library_membership (
    root_id TEXT NOT NULL REFERENCES library_roots(root_id) ON DELETE CASCADE,
    asset_id TEXT NOT NULL REFERENCES local_assets(identity_key) ON DELETE CASCADE,
    last_seen_generation INTEGER NOT NULL CHECK(last_seen_generation>=0),
    PRIMARY KEY(root_id,asset_id)
);
CREATE INDEX library_membership_asset ON library_membership(asset_id,root_id);
CREATE TABLE library_scan_runs (
    root_id TEXT NOT NULL REFERENCES library_roots(root_id) ON DELETE CASCADE,
    generation INTEGER NOT NULL CHECK(generation>0),
    operation_id TEXT NOT NULL UNIQUE,
    grant_epoch INTEGER NOT NULL CHECK(grant_epoch>0),
    state TEXT NOT NULL CHECK(state IN ('running','completed','partial','cancelled','failed','interrupted')),
    document TEXT NOT NULL CHECK(json_valid(document)),
    PRIMARY KEY(root_id,generation)
);
CREATE INDEX library_scan_runs_state ON library_scan_runs(state);
CREATE TABLE library_scan_seen (
    root_id TEXT NOT NULL,
    generation INTEGER NOT NULL,
    path_key TEXT NOT NULL,
    path TEXT NOT NULL,
    asset_id TEXT REFERENCES local_assets(identity_key) ON DELETE SET NULL,
    PRIMARY KEY(root_id,generation,path_key),
    FOREIGN KEY(root_id,generation) REFERENCES library_scan_runs(root_id,generation) ON DELETE CASCADE
);
CREATE INDEX library_scan_seen_asset ON library_scan_seen(root_id,generation,asset_id);
"#;

const VERSION_9: &str = r#"
ALTER TABLE local_file_grants ADD COLUMN grant_path_key TEXT;
ALTER TABLE local_file_grants ADD COLUMN document TEXT CHECK(document IS NULL OR json_valid(document));
ALTER TABLE local_asset_locations ADD COLUMN binding_revision INTEGER NOT NULL DEFAULT 1 CHECK(binding_revision>0);
ALTER TABLE local_asset_locations ADD COLUMN physical_identity_key TEXT;
CREATE INDEX local_asset_locations_physical ON local_asset_locations(physical_identity_key);
ALTER TABLE library_scan_runs ADD COLUMN scope_document TEXT NOT NULL DEFAULT '{"scope":"root"}' CHECK(json_valid(scope_document));
CREATE TABLE library_root_locations(root_id TEXT PRIMARY KEY NOT NULL REFERENCES library_roots(root_id) ON DELETE CASCADE,location_revision INTEGER NOT NULL CHECK(location_revision>0));
CREATE TABLE library_watch_preferences(root_id TEXT PRIMARY KEY NOT NULL REFERENCES library_roots(root_id) ON DELETE CASCADE,watch_generation INTEGER NOT NULL CHECK(watch_generation>=0),enabled INTEGER NOT NULL CHECK(enabled IN(0,1)),document TEXT NOT NULL CHECK(json_valid(document)));
CREATE TABLE library_dirty_state(root_id TEXT PRIMARY KEY NOT NULL REFERENCES library_roots(root_id) ON DELETE CASCADE,watch_generation INTEGER NOT NULL CHECK(watch_generation>=0),received_sequence INTEGER NOT NULL CHECK(received_sequence>=0),covered_sequence INTEGER NOT NULL CHECK(covered_sequence>=0 AND covered_sequence<=received_sequence),full_rescan_required INTEGER NOT NULL CHECK(full_rescan_required IN(0,1)));
CREATE TABLE library_identity_reservations(path_key TEXT PRIMARY KEY NOT NULL,asset_id TEXT NOT NULL UNIQUE,track_id TEXT NOT NULL UNIQUE,lx_id TEXT NOT NULL UNIQUE,physical_identity_key TEXT,expires_at_ms INTEGER NOT NULL CHECK(expires_at_ms>0),document TEXT NOT NULL CHECK(json_valid(document)));
CREATE TABLE library_relink_candidates(candidate_id TEXT PRIMARY KEY NOT NULL,asset_id TEXT NOT NULL REFERENCES local_assets(identity_key) ON DELETE CASCADE,root_id TEXT NOT NULL REFERENCES library_roots(root_id) ON DELETE CASCADE,expires_at_ms INTEGER NOT NULL CHECK(expires_at_ms>0),document TEXT NOT NULL CHECK(json_valid(document)));
CREATE INDEX library_relink_candidates_root ON library_relink_candidates(root_id,candidate_id);
CREATE TABLE library_relink_results(operation_id TEXT PRIMARY KEY NOT NULL,request_digest TEXT NOT NULL CHECK(length(request_digest)=64),asset_id TEXT NOT NULL REFERENCES local_assets(identity_key) ON DELETE CASCADE,document TEXT NOT NULL CHECK(json_valid(document)));
"#;

const VERSION_10: &str = r#"
CREATE TABLE download_destination_grants (
    id TEXT PRIMARY KEY NOT NULL,
    profile_id TEXT NOT NULL,
    epoch INTEGER NOT NULL CHECK(epoch>0),
    revision INTEGER NOT NULL CHECK(revision>0),
    revoked INTEGER NOT NULL CHECK(revoked IN(0,1)),
    document TEXT NOT NULL CHECK(json_valid(document))
);
CREATE TABLE download_settings (
    singleton INTEGER PRIMARY KEY CHECK(singleton=1),
    document TEXT NOT NULL CHECK(json_valid(document))
);
CREATE TABLE download_tasks (
    id TEXT PRIMARY KEY NOT NULL,
    profile_id TEXT NOT NULL,
    grant_id TEXT NOT NULL REFERENCES download_destination_grants(id),
    position INTEGER NOT NULL CHECK(position BETWEEN 0 AND 99),
    task_revision INTEGER NOT NULL CHECK(task_revision>0),
    operation_epoch INTEGER NOT NULL CHECK(operation_epoch>0),
    state TEXT NOT NULL,
    document TEXT NOT NULL CHECK(json_valid(document)),
    byte_size INTEGER NOT NULL CHECK(byte_size BETWEEN 1 AND 262144),
    CHECK(byte_size=length(CAST(document AS BLOB)))
);
CREATE INDEX download_tasks_order ON download_tasks(position,id);
CREATE INDEX tracks_lx_identity ON tracks(json_extract(document,'$.lx_id'));
"#;

const VERSION_11: &str = r#"
CREATE TABLE backup_imports (
    operation_id TEXT PRIMARY KEY NOT NULL CHECK(length(operation_id) BETWEEN 1 AND 256),
    input_digest TEXT NOT NULL CHECK(length(input_digest)=64),
    expected_revision INTEGER NOT NULL CHECK(expected_revision>=0),
    revision INTEGER NOT NULL CHECK(revision=expected_revision+1),
    document TEXT NOT NULL CHECK(json_valid(document)),
    byte_size INTEGER NOT NULL CHECK(byte_size BETWEEN 1 AND 8388608 AND byte_size=length(CAST(document AS BLOB)))
);
CREATE TABLE compat_settings (
    singleton INTEGER PRIMARY KEY CHECK(singleton=1),
    revision INTEGER NOT NULL CHECK(revision>=0),
    document TEXT NOT NULL CHECK(json_valid(document)),
    byte_size INTEGER NOT NULL CHECK(byte_size BETWEEN 1 AND 8388608 AND byte_size=length(CAST(document AS BLOB)))
);
CREATE TABLE backup_list_metadata (
    list_id TEXT PRIMARY KEY REFERENCES music_lists(id) ON DELETE CASCADE,
    revision INTEGER NOT NULL CHECK(revision>=0),
    document TEXT NOT NULL CHECK(json_valid(document)),
    byte_size INTEGER NOT NULL CHECK(byte_size BETWEEN 1 AND 8388608 AND byte_size=length(CAST(document AS BLOB)))
);
CREATE TABLE download_owned_artifacts (
    nonce TEXT PRIMARY KEY CHECK(length(nonce)=32),
    task_id TEXT NOT NULL REFERENCES download_tasks(id),
    operation_epoch INTEGER NOT NULL CHECK(operation_epoch>0),
    phase TEXT NOT NULL CHECK(phase IN ('allocated','verified','published','removed')),
    actual_bytes INTEGER NOT NULL CHECK(actual_bytes>=0 AND actual_bytes<=1073741824),
    reserved_bytes INTEGER NOT NULL CHECK(reserved_bytes>=0 AND reserved_bytes<=1073741824),
    sha256 TEXT CHECK(sha256 IS NULL OR length(sha256)=64),
    document TEXT NOT NULL CHECK(json_valid(document)),
    byte_size INTEGER NOT NULL CHECK(byte_size BETWEEN 1 AND 65536 AND byte_size=length(CAST(document AS BLOB)))
);
CREATE INDEX download_owned_artifacts_task ON download_owned_artifacts(task_id,operation_epoch);
"#;

const VERSION_17: &str = r#"
CREATE TABLE download_allocation_intents (
    nonce TEXT PRIMARY KEY CHECK(length(nonce)=32 AND nonce NOT GLOB '*[^0-9A-Fa-f]*'),
    task_id TEXT NOT NULL REFERENCES download_tasks(id),
    operation_epoch INTEGER NOT NULL CHECK(operation_epoch>0),
    reserved_bytes INTEGER NOT NULL CHECK(reserved_bytes BETWEEN 1 AND 1073741824),
    document TEXT NOT NULL CHECK(json_valid(document)),
    byte_size INTEGER NOT NULL CHECK(byte_size BETWEEN 1 AND 65536 AND byte_size=length(CAST(document AS BLOB)))
);
CREATE INDEX download_allocation_intents_task ON download_allocation_intents(task_id,operation_epoch);
"#;

pub(crate) fn integrity_check(connection: &Connection) -> Result<()> {
    let mut statement = connection.prepare("PRAGMA integrity_check")?;
    let messages = statement.query_map([], |row| row.get::<_, String>(0))?;
    for message in messages {
        let message = message?;
        ensure!(
            message == "ok",
            "Native repository failed its integrity check: {}",
            message.chars().take(512).collect::<String>()
        );
    }
    let foreign_key_error: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_foreign_key_check)",
        [],
        |row| row.get(0),
    )?;
    ensure!(
        !foreign_key_error,
        "Native repository has invalid references"
    );
    Ok(())
}

pub(crate) fn version(connection: &Connection) -> Result<u32> {
    let version: i64 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
    ensure!(
        version >= 0 && version <= SCHEMA_VERSION as i64,
        "Unsupported native repository schema version {version}"
    );
    Ok(version as u32)
}

pub(crate) fn validate_identity(connection: &Connection, version: u32) -> Result<()> {
    let application_id: i32 =
        connection.pragma_query_value(None, "application_id", |row| row.get(0))?;
    if version == 0 {
        let count: i64 = connection.query_row(
            "SELECT count(*) FROM sqlite_schema WHERE name NOT LIKE 'sqlite_%'",
            [],
            |row| row.get(0),
        )?;
        ensure!(
            application_id == 0 && count == 0,
            "Refusing to initialize an unrecognized database"
        );
    } else {
        ensure!(
            application_id == APPLICATION_ID,
            "Database is not an LX Native repository"
        );
    }
    Ok(())
}

pub(crate) fn migrate(connection: &mut Connection, old_version: u32) -> Result<()> {
    if old_version == SCHEMA_VERSION {
        return validate_tables(connection);
    }
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    if old_version == 0 {
        transaction
            .execute_batch(VERSION_1)
            .context("Cannot create repository schema 1")?;
    }
    if old_version <= 1 {
        transaction
            .execute_batch(VERSION_2)
            .context("Cannot migrate repository schema 2")?;
    }
    if old_version <= 2 {
        transaction
            .execute_batch(VERSION_3)
            .context("Cannot migrate repository schema 3")?;
    }
    if old_version <= 3 {
        transaction
            .execute_batch(VERSION_4)
            .context("Cannot migrate repository schema 4")?;
    }
    if old_version <= 4 {
        transaction
            .execute_batch(VERSION_5)
            .context("Cannot migrate repository schema 5")?;
    }
    if old_version <= 5 {
        transaction
            .execute_batch(VERSION_6)
            .context("Cannot migrate repository schema 6")?;
    }
    if old_version <= 6 {
        transaction
            .execute_batch(VERSION_7)
            .context("Cannot migrate repository schema 7")?;
    }
    if old_version <= 7 {
        transaction
            .execute_batch(VERSION_8)
            .context("Cannot migrate repository schema 8")?;
    }
    if old_version <= 8 {
        transaction
            .execute_batch(VERSION_9)
            .context("Cannot migrate repository schema 9")?;
        if old_version <= 7 {
            super::library::initialize_library_assets(&transaction)?;
        }
        super::library_binding::initialize_bindings(&transaction)?;
    }
    if old_version <= 9 {
        transaction
            .execute_batch(VERSION_10)
            .context("Cannot migrate repository schema 10")?;
    }
    if old_version <= 10 {
        transaction
            .execute_batch(VERSION_11)
            .context("Cannot migrate repository schema 11")?;
    }
    if old_version <= 11 {
        transaction
            .execute_batch(super::sync::SCHEMA_12)
            .context("Cannot migrate repository schema 12")?;
    }
    if old_version <= 12 {
        transaction
            .execute_batch(super::sync::SCHEMA_13)
            .context("Cannot migrate repository schema 13")?;
    }
    if old_version <= 13 {
        transaction
            .execute_batch(VERSION_14)
            .context("Cannot migrate repository schema 14")?;
    }
    if old_version <= 14 {
        transaction
            .execute_batch(VERSION_15)
            .context("Cannot migrate repository schema 15")?;
    }
    if old_version <= 15 {
        transaction
            .execute_batch(VERSION_16)
            .context("Cannot migrate repository schema 16")?;
    }
    if old_version <= 16 {
        transaction
            .execute_batch(VERSION_17)
            .context("Cannot migrate repository schema 17")?;
    }
    transaction.pragma_update(None, "application_id", APPLICATION_ID)?;
    transaction.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    validate_tables(&transaction)?;
    integrity_check(&transaction)?;
    transaction
        .commit()
        .context("Cannot commit repository migration")?;
    Ok(())
}

/// Builds a reference only from production SQL. Untrusted sqlite_schema text
/// is never accepted as a migration script, including for historical images.
pub(crate) fn trusted_reference_for_version(version: u32) -> Result<Connection> {
    ensure!(
        (1..=SCHEMA_VERSION).contains(&version),
        "Unsupported historical native schema"
    );
    let mut connection = Connection::open_in_memory()?;
    for (kind, value) in [
        (rusqlite::ffi::SQLITE_LIMIT_SQL_LENGTH, 64 * 1024),
        (rusqlite::ffi::SQLITE_LIMIT_LENGTH, 1024 * 1024),
        (rusqlite::ffi::SQLITE_LIMIT_COLUMN, 128),
        (rusqlite::ffi::SQLITE_LIMIT_EXPR_DEPTH, 64),
        (rusqlite::ffi::SQLITE_LIMIT_ATTACHED, 0),
        (rusqlite::ffi::SQLITE_LIMIT_VARIABLE_NUMBER, 256),
    ] {
        unsafe { rusqlite::ffi::sqlite3_limit(connection.handle(), kind, value) };
    }
    connection.execute_batch("PRAGMA page_size=4096; PRAGMA max_page_count=512; PRAGMA cache_size=-128; PRAGMA mmap_size=0; PRAGMA temp_store=MEMORY; PRAGMA foreign_keys=ON;")?;
    let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let scripts = [
        VERSION_1,
        VERSION_2,
        VERSION_3,
        VERSION_4,
        VERSION_5,
        VERSION_6,
        VERSION_7,
        VERSION_8,
        VERSION_9,
        VERSION_10,
        VERSION_11,
        super::sync::SCHEMA_12,
        super::sync::SCHEMA_13,
        VERSION_14,
        VERSION_15,
        VERSION_16,
        VERSION_17,
    ];
    for script in scripts.into_iter().take(version as usize) {
        tx.execute_batch(script)?;
    }
    tx.pragma_update(None, "application_id", APPLICATION_ID)?;
    tx.pragma_update(None, "user_version", version)?;
    tx.commit()?;
    Ok(connection)
}

pub(crate) fn validate_current(connection: &Connection) -> Result<()> {
    let version = version(connection)?;
    ensure!(
        version == SCHEMA_VERSION,
        "Repository schema changed while open"
    );
    validate_identity(connection, version)
}

fn validate_tables(connection: &Connection) -> Result<()> {
    for query in [
        "SELECT approval_id,asset_id,approval_epoch,revoked,document,byte_size FROM tag_write_approvals LIMIT 0",
        "SELECT operation_id,approval_id,asset_id,operation_epoch,ledger_revision,phase,document,byte_size FROM tag_edit_operations LIMIT 0",
        "SELECT operation_id,role,phase,actual_bytes,reserved_bytes,document,byte_size FROM tag_edit_artifacts LIMIT 0",
        "SELECT singleton,profile_id,profile_epoch,operation_epoch,selection_epoch FROM physical_snapshot_state LIMIT 0",
        "SELECT operation_id,profile_id,profile_epoch,operation_epoch,selection_epoch,expected_snapshot_revision,maximum_image_bytes,request_digest,ledger_revision,state,document,byte_size FROM physical_snapshot_operations LIMIT 0",
        "SELECT singleton,profile_epoch,pairing_epoch,pairing_enabled,sequence,journal_floor FROM sync_profile_state LIMIT 0",
        "SELECT peer_id,dataset,auth_epoch,document FROM sync_mode_preferences LIMIT 0",
        "SELECT peer_id,client_id,auth_epoch,revoked,private_key,document,byte_size FROM sync_peers LIMIT 0",
        "SELECT dataset,digest,sha256,document,byte_size FROM sync_snapshots LIMIT 0",
        "SELECT dataset,digest,sha256,available,source_revision,sequence FROM sync_current LIMIT 0",
        "SELECT peer_id,dataset,common_digest,common_sha256,captured_repository_revision,covered_sequence FROM sync_baselines LIMIT 0",
        "SELECT sequence,dataset,repository_revision,origin_peer,action_document,byte_size FROM sync_journal LIMIT 0",
        "SELECT list_id,document,byte_size FROM sync_list_metadata LIMIT 0",
        "SELECT dataset,document,byte_size FROM sync_capture_override LIMIT 0",
        "SELECT operation_id,input_digest,expected_revision,revision,document,byte_size FROM backup_imports LIMIT 0",
        "SELECT operation_id,input_digest,expected_revision,revision,preferences_applied,document,byte_size FROM native_backup_imports LIMIT 0",
        "SELECT operation_id,sha256,document,byte_size FROM native_backup_archives LIMIT 0",
        "SELECT singleton,revision,document,byte_size FROM compat_settings LIMIT 0",
        "SELECT list_id,revision,document,byte_size FROM backup_list_metadata LIMIT 0",
        "SELECT nonce,task_id,operation_epoch,reserved_bytes,document,byte_size FROM download_allocation_intents LIMIT 0",
        "SELECT nonce,task_id,operation_epoch,phase,actual_bytes,reserved_bytes,sha256,document,byte_size FROM download_owned_artifacts LIMIT 0",
        "SELECT id,profile_id,epoch,revision,revoked,document FROM download_destination_grants LIMIT 0",
        "SELECT singleton,document FROM download_settings LIMIT 0",
        "SELECT id,profile_id,grant_id,position,task_revision,operation_epoch,state,document,byte_size FROM download_tasks LIMIT 0",
        "SELECT id, document FROM tracks LIMIT 0",
        "SELECT identity_key, track_id, document FROM local_assets LIMIT 0",
        "SELECT track_id, origin FROM lyric_bundles LIMIT 0",
        "SELECT track_id, origin, kind, state, text FROM lyric_fields LIMIT 0",
        "SELECT singleton, revision FROM repository_state LIMIT 0",
        "SELECT revision, document FROM change_outbox LIMIT 0",
        "SELECT name, source_digest, revision FROM import_markers LIMIT 0",
        "SELECT singleton, digest, change_document FROM profile_metadata LIMIT 0",
        "SELECT id,kind,position,document,scroll_position,update_document FROM music_lists LIMIT 0",
        "SELECT list_id,music_id,track_id,position,document FROM music_list_items LIMIT 0",
        "SELECT singleton,rules FROM dislike_state LIMIT 0",
        "SELECT word,position FROM search_history LIMIT 0",
        "SELECT singleton,document FROM playback_state LIMIT 0",
        "SELECT cache_key,original_track_id,original_platform,actual_quality,source_sha256,source_policy_version,music_info_sha256,resolved_at_ms,expire_at_ms,last_access_ms,byte_size,document FROM resolved_media LIMIT 0",
        "SELECT cache_key,original_track_id,original_platform,original_record_sha256,algorithm_version,matched_at_ms,expire_at_ms,last_access_ms,byte_size,document FROM source_matches LIMIT 0",
        "SELECT sha256,byte_size,last_access_ms,document,data FROM artwork_blobs LIMIT 0",
        "SELECT cache_key,original_track_id,original_record_sha256,resolved_at_ms,expire_at_ms,last_access_ms,byte_size,blob_sha256,document FROM artwork_info LIMIT 0",
        "SELECT root_id,path_key,active,grant_epoch,generation,document FROM library_roots LIMIT 0",
        "SELECT asset_id,epoch FROM local_file_grants LIMIT 0",
        "SELECT asset_id,path_key,document FROM local_asset_locations LIMIT 0",
        "SELECT root_id,asset_id,last_seen_generation FROM library_membership LIMIT 0",
        "SELECT root_id,generation,operation_id,grant_epoch,state,document FROM library_scan_runs LIMIT 0",
        "SELECT root_id,generation,path_key,path,asset_id FROM library_scan_seen LIMIT 0",
        "SELECT asset_id,epoch,grant_path_key,document FROM local_file_grants LIMIT 0",
        "SELECT asset_id,binding_revision,physical_identity_key FROM local_asset_locations LIMIT 0",
        "SELECT root_id,location_revision FROM library_root_locations LIMIT 0",
        "SELECT root_id,watch_generation,enabled,document FROM library_watch_preferences LIMIT 0",
        "SELECT root_id,watch_generation,received_sequence,covered_sequence,full_rescan_required FROM library_dirty_state LIMIT 0",
        "SELECT path_key,asset_id,track_id,lx_id,physical_identity_key,expires_at_ms,document FROM library_identity_reservations LIMIT 0",
        "SELECT candidate_id,asset_id,root_id,expires_at_ms,document FROM library_relink_candidates LIMIT 0",
        "SELECT operation_id,request_digest,asset_id,document FROM library_relink_results LIMIT 0",
        "SELECT root_id,generation,scope_document FROM library_scan_runs LIMIT 0",
    ] {
        connection
            .prepare(query)
            .context("Native repository schema is incomplete")?;
    }
    let count: i64 = connection.query_row(
        "SELECT count(*) FROM repository_state WHERE singleton=1 AND revision>=0",
        [],
        |row| row.get(0),
    )?;
    ensure!(count == 1, "Native repository revision record is missing");
    let builtin_count: i64 = connection.query_row(
        "SELECT count(*) FROM music_lists WHERE kind='builtin' AND id IN ('default','love','temp')",
        [],
        |row| row.get(0),
    )?;
    ensure!(builtin_count == 3, "Native built-in lists are missing");
    let dislike_count: i64 = connection.query_row(
        "SELECT count(*) FROM dislike_state WHERE singleton=1",
        [],
        |row| row.get(0),
    )?;
    ensure!(dislike_count == 1, "Native dislike state is missing");
    Ok(())
}

#[cfg(test)]
mod historical_reference_tests {
    use super::*;
    #[test]
    fn historical_production_ddl_is_verified_before_migration_and_rejects_mutated_constraints() {
        for version in 1..=SCHEMA_VERSION {
            let mut reference = trusted_reference_for_version(version).unwrap();
            super::super::snapshot_sanitize::validate_schema_for_version(
                &reference,
                version,
                &|| Ok(()),
            )
            .unwrap();
            migrate(&mut reference, version).unwrap();
            super::super::snapshot_sanitize::validate_schema_for_version(
                &reference,
                SCHEMA_VERSION,
                &|| Ok(()),
            )
            .unwrap();
        }
        let reference = trusted_reference_for_version(15).unwrap();
        reference
            .execute_batch("CREATE TABLE foreign_write_permission(token TEXT);")
            .unwrap();
        assert!(
            super::super::snapshot_sanitize::validate_schema_for_version(
                &reference,
                15,
                &|| Ok(())
            )
            .is_err()
        );
    }
}

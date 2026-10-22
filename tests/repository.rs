use lx_gpui_player::{
    domain::{
        lyric::{LyricBundle, LyricField, LyricOrigin},
        music::{LxMusicDtoVersion, TrackRecord},
    },
    repository::{
        APPLICATION_ID, ChangeKind, DATABASE_NAME, Repository, SCHEMA_VERSION,
        local_track_from_path,
    },
};
use rusqlite::Connection;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{fs, path::Path};
use tempfile::TempDir;

fn profile() -> TempDir {
    let base = std::env::var_os("TMP")
        .or_else(|| std::env::var_os("TEMP"))
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    fs::create_dir_all(&base).unwrap();
    tempfile::Builder::new()
        .prefix("lx-native-repository-")
        .tempdir_in(base)
        .unwrap()
}

fn track() -> TrackRecord {
    TrackRecord::from_lx_dto(json!({
        "id":"kw_test", "source":"kw", "name":"原创测试曲", "singer":"测试作者",
        "interval":"00:12", "meta":{"songId":"test", "albumName":"测试专辑", "qualitys":[{"type":"320k"}], "custom":{"retain":true}}
    }), LxMusicDtoVersion::Modern).unwrap()
}

fn bundle(text: &str) -> LyricBundle {
    LyricBundle {
        lyric: LyricField::Text(text.into()),
        tlyric: LyricField::Null,
        rlyric: LyricField::Missing,
        lxlyric: LyricField::Text(String::new()),
    }
}

#[test]
fn tracks_and_outbox_survive_reopen_without_changing_ids() {
    let profile = profile();
    let track = track();
    let repository = Repository::open(profile.path()).unwrap();
    let change = repository.upsert_track(&track).unwrap();
    assert_eq!(change.revision, 1);
    assert_eq!(change.kind, ChangeKind::TrackUpserted);
    assert_eq!(repository.track(&track.id).unwrap(), Some(track.clone()));
    repository.integrity_check().unwrap();
    drop(repository);
    let repository = Repository::open(profile.path()).unwrap();
    assert_eq!(repository.tracks().unwrap(), vec![track]);
    assert_eq!(repository.changes_since(0, 10).unwrap(), vec![change]);
}

#[test]
fn missing_null_and_empty_lyrics_are_distinct_and_edits_are_independent() {
    let profile = profile();
    let repository = Repository::open(profile.path()).unwrap();
    let track = track();
    repository.upsert_track(&track).unwrap();
    assert_eq!(
        repository.lyrics(&track.id, LyricOrigin::Raw).unwrap(),
        None
    );
    let raw = bundle("[00:00.00]原创原文");
    let edited = bundle("[00:00.00]用户编辑");
    repository
        .replace_lyrics(&track.id, LyricOrigin::Raw, &raw)
        .unwrap();
    repository
        .replace_lyrics(&track.id, LyricOrigin::Edited, &edited)
        .unwrap();
    drop(repository);
    let repository = Repository::open(profile.path()).unwrap();
    assert_eq!(
        repository.lyrics(&track.id, LyricOrigin::Raw).unwrap(),
        Some(raw.clone())
    );
    assert_eq!(
        repository.lyrics(&track.id, LyricOrigin::Edited).unwrap(),
        Some(edited.clone())
    );
    repository.clear_raw_lyrics(&track.id).unwrap();
    assert_eq!(
        repository.lyrics(&track.id, LyricOrigin::Raw).unwrap(),
        None
    );
    assert_eq!(
        repository.lyrics(&track.id, LyricOrigin::Edited).unwrap(),
        Some(edited)
    );
    repository
        .replace_lyrics(&track.id, LyricOrigin::Raw, &raw)
        .unwrap();
    repository.restore_raw_lyrics(&track.id).unwrap();
    assert_eq!(
        repository.lyrics(&track.id, LyricOrigin::Raw).unwrap(),
        Some(raw)
    );
    assert_eq!(
        repository.lyrics(&track.id, LyricOrigin::Edited).unwrap(),
        None
    );
}

#[test]
fn bundle_replacement_removes_previous_extended_fields_and_never_duplicates_rows() {
    let profile = profile();
    let repository = Repository::open(profile.path()).unwrap();
    let track = track();
    repository.upsert_track(&track).unwrap();
    let mut old = bundle("old");
    old.tlyric = LyricField::Text("old translation".into());
    repository
        .replace_lyrics(&track.id, LyricOrigin::Edited, &old)
        .unwrap();
    let new = LyricBundle::default();
    repository
        .replace_lyrics(&track.id, LyricOrigin::Edited, &new)
        .unwrap();
    repository
        .replace_lyrics(&track.id, LyricOrigin::Edited, &new)
        .unwrap();
    assert_eq!(
        repository.lyrics(&track.id, LyricOrigin::Edited).unwrap(),
        Some(new)
    );
    let inspection = Connection::open(repository.path()).unwrap();
    assert_eq!(
        inspection
            .query_row("SELECT count(*) FROM lyric_fields", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        4
    );
}

#[test]
fn failed_mid_bundle_insert_rolls_back_data_revision_and_outbox() {
    let profile = profile();
    let repository = Repository::open(profile.path()).unwrap();
    let track = track();
    repository.upsert_track(&track).unwrap();
    let raw = bundle("old");
    repository
        .replace_lyrics(&track.id, LyricOrigin::Raw, &raw)
        .unwrap();
    let revision = repository.revision().unwrap();
    let inspection = Connection::open(repository.path()).unwrap();
    inspection.execute_batch("CREATE TRIGGER fail_lyric_channel BEFORE INSERT ON lyric_fields WHEN NEW.kind='rlyric' BEGIN SELECT RAISE(ABORT,'injected write failure'); END;").unwrap();
    assert!(
        repository
            .replace_lyrics(&track.id, LyricOrigin::Raw, &bundle("new"))
            .is_err()
    );
    assert_eq!(
        repository.lyrics(&track.id, LyricOrigin::Raw).unwrap(),
        Some(raw)
    );
    assert_eq!(repository.revision().unwrap(), revision);
    assert!(repository.changes_since(revision, 10).unwrap().is_empty());
}

#[test]
fn stale_revision_and_invalid_track_writes_do_not_publish_changes() {
    let profile = profile();
    let repository = Repository::open(profile.path()).unwrap();
    let track = track();
    repository.upsert_track(&track).unwrap();
    assert!(
        repository
            .upsert_track_at_revision(&track, Some(0))
            .is_err()
    );
    assert!(
        repository
            .replace_lyrics("absent", LyricOrigin::Raw, &bundle("invalid"))
            .is_err()
    );
    assert_eq!(repository.revision().unwrap(), 1);
    assert_eq!(repository.changes_since(0, 10).unwrap().len(), 1);
}

#[test]
fn another_writer_is_rejected_and_os_lock_is_released_on_drop() {
    let profile = profile();
    let repository = Repository::open(profile.path()).unwrap();
    assert!(Repository::open(profile.path()).is_err());
    drop(repository);
    Repository::open(profile.path()).unwrap();
}

#[test]
fn future_schema_and_corrupt_database_are_preserved_byte_for_byte() {
    let future = profile();
    let path = future.path().join(DATABASE_NAME);
    let connection = Connection::open(&path).unwrap();
    connection
        .pragma_update(None, "user_version", SCHEMA_VERSION + 1)
        .unwrap();
    connection
        .execute_batch(
            "CREATE TABLE future_data(value TEXT); INSERT INTO future_data VALUES('preserve');",
        )
        .unwrap();
    drop(connection);
    let before = fs::read(&path).unwrap();
    assert!(Repository::open(future.path()).is_err());
    assert_eq!(fs::read(&path).unwrap(), before);
    let corrupt = profile();
    let path = corrupt.path().join(DATABASE_NAME);
    fs::write(&path, b"damaged sqlite bytes").unwrap();
    assert!(Repository::open(corrupt.path()).is_err());
    assert_eq!(fs::read(path).unwrap(), b"damaged sqlite bytes");
}

fn version_one(path: &Path) -> Connection {
    let connection = Connection::open(path).unwrap();
    connection.execute_batch("CREATE TABLE tracks(id TEXT PRIMARY KEY NOT NULL,document TEXT NOT NULL CHECK(json_valid(document))); CREATE TABLE local_assets(identity_key TEXT PRIMARY KEY NOT NULL,track_id TEXT NOT NULL UNIQUE REFERENCES tracks(id) ON DELETE CASCADE,document TEXT NOT NULL CHECK(json_valid(document)));").unwrap();
    connection
        .pragma_update(None, "application_id", APPLICATION_ID)
        .unwrap();
    connection.pragma_update(None, "user_version", 1).unwrap();
    connection
}

#[test]
fn version_one_migration_preserves_tracks_and_creates_valid_pre_migration_backup() {
    let profile = profile();
    let path = profile.path().join(DATABASE_NAME);
    let connection = version_one(&path);
    let track = track();
    connection
        .execute(
            "INSERT INTO tracks(id,document) VALUES(?1,?2)",
            rusqlite::params![track.id, serde_json::to_string(&track).unwrap()],
        )
        .unwrap();
    drop(connection);
    let repository = Repository::open(profile.path()).unwrap();
    assert_eq!(repository.track(&track.id).unwrap(), Some(track));
    repository.integrity_check().unwrap();
    let backups: Vec<_> = fs::read_dir(profile.path())
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "bak"))
        .collect();
    assert_eq!(backups.len(), 1);
    let backup = Connection::open(&backups[0]).unwrap();
    assert_eq!(
        backup
            .pragma_query_value(None, "user_version", |row| row.get::<_, u32>(0))
            .unwrap(),
        1
    );
    assert_eq!(
        backup
            .query_row("PRAGMA integrity_check", [], |row| row.get::<_, String>(0))
            .unwrap(),
        "ok"
    );
}

#[test]
fn migration_failure_rolls_back_earlier_ddl_and_retains_original_version() {
    let profile = profile();
    let path = profile.path().join(DATABASE_NAME);
    let connection = version_one(&path);
    connection.execute_batch("CREATE TABLE change_outbox(sentinel TEXT); INSERT INTO change_outbox VALUES('retain');").unwrap();
    drop(connection);
    assert!(Repository::open(profile.path()).is_err());
    let inspection = Connection::open(path).unwrap();
    assert_eq!(
        inspection
            .pragma_query_value(None, "user_version", |row| row.get::<_, u32>(0))
            .unwrap(),
        1
    );
    assert_eq!(
        inspection
            .query_row("SELECT sentinel FROM change_outbox", [], |row| row
                .get::<_, String>(0))
            .unwrap(),
        "retain"
    );
    assert_eq!(
        inspection
            .query_row(
                "SELECT count(*) FROM sqlite_schema WHERE name='lyric_bundles'",
                [],
                |row| row.get::<_, u32>(0)
            )
            .unwrap(),
        0
    );
}

#[test]
fn local_migration_is_stable_deduplicated_and_never_rewrites_project_store() {
    let profile = profile();
    let audio = profile.path().join("合法本地曲目.wav");
    fs::write(&audio, b"fixture metadata only").unwrap();
    let old_json = profile.path().join("player-state.json");
    let old_bytes =
        br#"{"version":1,"data":{"sources":[],"selected":null,"local_files":[],"volume":0.7}}"#;
    fs::write(&old_json, old_bytes).unwrap();
    let digest = format!("{:x}", Sha256::digest(old_bytes));
    let repository = Repository::open(profile.path()).unwrap();
    let result = repository
        .import_legacy_local_files(&[audio.clone(), audio.clone()], &digest)
        .unwrap();
    assert_eq!(result.imported.len(), 1);
    assert!(!result.already_imported);
    assert_eq!(repository.tracks().unwrap().len(), 1);
    assert_eq!(fs::read(&old_json).unwrap(), old_bytes);
    let identity = result.imported[0].clone();
    drop(repository);
    let repository = Repository::open(profile.path()).unwrap();
    let repeated = repository
        .import_legacy_local_files(&[audio], &digest)
        .unwrap();
    assert!(repeated.already_imported);
    assert_eq!(repeated.imported, vec![identity]);
    assert_eq!(repeated.revision, result.revision);
}

#[test]
fn failed_legacy_import_does_not_leave_rows_or_import_marker() {
    let profile = profile();
    let audio = profile.path().join("local.wav");
    fs::write(&audio, b"fixture").unwrap();
    let repository = Repository::open(profile.path()).unwrap();
    assert!(
        repository
            .import_legacy_local_files(&[audio, "relative.wav".into()], &"a".repeat(64))
            .is_err()
    );
    assert!(repository.tracks().unwrap().is_empty());
    assert!(
        repository
            .import_marker("project-store-v1-local-files")
            .unwrap()
            .is_none()
    );
    assert_eq!(repository.revision().unwrap(), 0);
}

#[test]
fn asset_collision_rolls_back_track_and_outbox_and_missing_files_remain_indexed() {
    let profile = profile();
    let (track, asset) = local_track_from_path(&profile.path().join("missing.wav")).unwrap();
    assert!(!asset.available);
    let repository = Repository::open(profile.path()).unwrap();
    repository.upsert_local_track(&track, &asset).unwrap();
    let mut collision = track.clone();
    collision.id = "different-track".into();
    let mut other_asset = asset.clone();
    other_asset.track_id = collision.id.clone();
    assert!(
        repository
            .upsert_local_track(&collision, &other_asset)
            .is_err()
    );
    assert!(repository.track("different-track").unwrap().is_none());
    assert_eq!(repository.revision().unwrap(), 1);
    assert_eq!(repository.local_asset(&asset.id).unwrap(), Some(asset));
}

#[test]
fn backup_contains_latest_committed_wal_data_and_acknowledgment_preserves_revision() {
    let profile = profile();
    let repository = Repository::open(profile.path()).unwrap();
    let track = track();
    repository.upsert_track(&track).unwrap();
    let destination = profile.path().join("snapshot.sqlite3");
    repository.backup_to(&destination).unwrap();
    assert!(repository.backup_to(&destination).is_err());
    let backup = Connection::open(destination).unwrap();
    assert_eq!(
        backup
            .query_row("SELECT count(*) FROM tracks", [], |row| row
                .get::<_, u32>(0))
            .unwrap(),
        1
    );
    repository.acknowledge_changes_through(1).unwrap();
    assert_eq!(repository.revision().unwrap(), 1);
    assert!(repository.changes_since(0, 10).unwrap().is_empty());
    assert!(repository.acknowledge_changes_through(2).is_err());
}

#[test]
fn local_batch_is_one_transaction_and_one_outbox_record() {
    let profile = profile();
    let first = local_track_from_path(&profile.path().join("batch-one.wav")).unwrap();
    let second = local_track_from_path(&profile.path().join("batch-two.wav")).unwrap();
    let repository = Repository::open(profile.path()).unwrap();
    let change = repository
        .upsert_local_tracks(&[first.clone(), second.clone()])
        .unwrap();
    assert_eq!(change.kind, ChangeKind::LocalAssetsUpserted);
    assert_eq!(change.revision, 1);
    assert_eq!(change.details["count"], 2);
    assert_eq!(repository.tracks().unwrap().len(), 2);
    assert_eq!(repository.local_assets().unwrap().len(), 2);
    assert_eq!(repository.changes_since(0, 10).unwrap(), vec![change]);
    drop(repository);
    let repository = Repository::open(profile.path()).unwrap();
    assert_eq!(repository.track(&first.0.id).unwrap(), Some(first.0));
    assert_eq!(
        repository.local_asset(&second.1.id).unwrap(),
        Some(second.1)
    );
}

#[test]
fn local_batch_collision_after_first_insert_rolls_back_every_item_and_revision() {
    let profile = profile();
    let (existing_track, existing_asset) =
        local_track_from_path(&profile.path().join("existing.wav")).unwrap();
    let repository = Repository::open(profile.path()).unwrap();
    repository
        .upsert_local_track(&existing_track, &existing_asset)
        .unwrap();
    let first = local_track_from_path(&profile.path().join("first-new.wav")).unwrap();
    let mut collision_track = existing_track.clone();
    collision_track.id = "new-track-colliding-with-existing-asset".into();
    let mut collision_asset = existing_asset.clone();
    collision_asset.track_id = collision_track.id.clone();
    assert!(
        repository
            .upsert_local_tracks(&[first.clone(), (collision_track.clone(), collision_asset)])
            .is_err()
    );
    assert!(repository.track(&first.0.id).unwrap().is_none());
    assert!(repository.track(&collision_track.id).unwrap().is_none());
    assert_eq!(
        repository.track(&existing_track.id).unwrap(),
        Some(existing_track)
    );
    assert_eq!(repository.local_assets().unwrap(), vec![existing_asset]);
    assert_eq!(repository.revision().unwrap(), 1);
    assert!(repository.changes_since(1, 10).unwrap().is_empty());
}

#[test]
fn profile_digest_replay_survives_outbox_ack_and_restart_without_rewinding_revision() {
    let profile = profile();
    let repository = Repository::open(profile.path()).unwrap();
    let first = repository.record_profile_change(&"a".repeat(64)).unwrap();
    assert_eq!(first.kind, ChangeKind::ProfileChanged);
    assert_eq!(first.details, json!({"digest":"a".repeat(64)}));
    repository.upsert_track(&track()).unwrap();
    assert_eq!(
        repository.record_profile_change(&"A".repeat(64)).unwrap(),
        first
    );
    assert_eq!(repository.revision().unwrap(), 2);
    assert_eq!(repository.changes_since(0, 10).unwrap().len(), 2);
    repository.acknowledge_changes_through(2).unwrap();
    drop(repository);
    let repository = Repository::open(profile.path()).unwrap();
    assert_eq!(
        repository.record_profile_change(&"a".repeat(64)).unwrap(),
        first
    );
    assert_eq!(repository.revision().unwrap(), 2);
    assert!(repository.changes_since(0, 10).unwrap().is_empty());
    let second = repository.record_profile_change(&"b".repeat(64)).unwrap();
    assert_eq!(second.revision, 3);
    let third = repository.record_profile_change(&"a".repeat(64)).unwrap();
    assert_eq!(third.revision, 4);
}

#[test]
fn invalid_profile_digest_and_mid_metadata_failure_do_not_commit_changes() {
    let profile = profile();
    let repository = Repository::open(profile.path()).unwrap();
    let first = repository.record_profile_change(&"a".repeat(64)).unwrap();
    assert!(repository.record_profile_change("not a digest").is_err());
    let inspection = Connection::open(repository.path()).unwrap();
    inspection.execute_batch("CREATE TRIGGER reject_profile_update BEFORE UPDATE ON profile_metadata BEGIN SELECT RAISE(ABORT,'metadata write failure'); END;").unwrap();
    assert!(repository.record_profile_change(&"b".repeat(64)).is_err());
    assert_eq!(repository.revision().unwrap(), 1);
    assert_eq!(
        repository.record_profile_change(&"a".repeat(64)).unwrap(),
        first
    );
    assert_eq!(repository.changes_since(0, 10).unwrap(), vec![first]);
}

#[test]
fn schema_two_upgrade_keeps_library_and_revision_then_accepts_profile_metadata() {
    let profile = profile();
    let repository = Repository::open(profile.path()).unwrap();
    let track = track();
    repository.upsert_track(&track).unwrap();
    let path = repository.path().to_path_buf();
    drop(repository);
    let connection = Connection::open(&path).unwrap();
    connection
        .execute_batch("DROP TABLE download_allocation_intents; DROP TABLE tag_edit_artifacts; DROP TABLE tag_edit_operations; DROP TABLE tag_write_approvals; DROP TABLE native_backup_archives; DROP TABLE native_backup_imports; DROP TABLE physical_snapshot_operations; DROP TABLE physical_snapshot_state; DROP TABLE sync_mode_preferences; DROP TABLE sync_capture_override; DROP TABLE sync_baselines; DROP TABLE sync_current; DROP TABLE sync_journal; DROP TABLE sync_list_metadata; DROP TABLE sync_snapshots; DROP TABLE sync_peers; DROP TABLE sync_profile_state; DROP TABLE download_owned_artifacts; DROP TABLE backup_list_metadata; DROP TABLE compat_settings; DROP TABLE backup_imports; DROP TABLE download_tasks; DROP TABLE download_settings; DROP TABLE download_destination_grants; DROP INDEX tracks_lx_identity; DROP TABLE library_relink_results; DROP TABLE library_relink_candidates; DROP TABLE library_identity_reservations; DROP TABLE library_dirty_state; DROP TABLE library_watch_preferences; DROP TABLE library_root_locations; DROP TABLE library_scan_seen; DROP TABLE library_scan_runs; DROP TABLE library_membership; DROP TABLE local_asset_locations; DROP TABLE local_file_grants; DROP TABLE library_roots; DROP TABLE artwork_info; DROP TABLE artwork_blobs; DROP TABLE resolved_media; DROP TABLE source_matches; DROP TABLE playback_state; DROP TABLE music_list_items; DROP TABLE music_lists; DROP TABLE dislike_state; DROP TABLE search_history; DROP TABLE profile_metadata;")
        .unwrap();
    connection.pragma_update(None, "user_version", 2).unwrap();
    drop(connection);
    let repository = Repository::open(profile.path()).unwrap();
    assert_eq!(repository.track(&track.id).unwrap(), Some(track));
    assert_eq!(repository.revision().unwrap(), 1);
    let change = repository.record_profile_change(&"a".repeat(64)).unwrap();
    assert_eq!(change.revision, 2);
    repository.integrity_check().unwrap();
}

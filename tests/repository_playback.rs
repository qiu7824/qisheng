use lx_gpui_player::{
    playback::queue::{AdvanceReason, PlayMode, QueueState},
    repository::{ChangeKind, DATABASE_NAME, PlaybackCheckpoint, Repository, SCHEMA_VERSION},
};
use tempfile::tempdir_in;
fn directory() -> tempfile::TempDir {
    tempdir_in(std::env::temp_dir()).unwrap()
}
fn checkpoint() -> PlaybackCheckpoint {
    let mut queue = QueueState::new(77);
    queue.set_list(
        "playlist-owned".into(),
        vec!["a".into(), "b".into(), "c".into()],
    );
    queue.select_track("b").unwrap();
    queue.set_mode(PlayMode::Random);
    queue.enqueue(vec!["t".into(), "t".into()], false);
    queue.preview_next(AdvanceReason::Ended, |_| true);
    PlaybackCheckpoint {
        queue,
        position_ms: 12345,
        lyric_offset_ms: -400,
        quality: "320k".into(),
        auto_skip_on_error: false,
        auto_switch_source: true,
    }
}
#[test]
fn absent_checkpoint_does_not_invent_a_current_track() {
    let directory = directory();
    let repo = Repository::open(directory.path()).unwrap();
    assert!(repo.playback_checkpoint().unwrap().is_none());
    assert_eq!(repo.revision().unwrap(), 0);
}
#[test]
fn queue_history_temporary_preview_and_user_controls_survive_restart() {
    let directory = directory();
    let expected = checkpoint();
    let repo = Repository::open(directory.path()).unwrap();
    let change = repo
        .save_playback_checkpoint(&expected, Some(0), "cli")
        .unwrap();
    assert_eq!(change.kind, ChangeKind::PlaybackCheckpointChanged);
    assert_eq!(change.details["origin"], "cli");
    assert_eq!(change.track_id.as_deref(), Some("b"));
    assert_eq!(change.revision, 1);
    let outbox = repo.changes_since(0, 10).unwrap();
    assert_eq!(outbox.len(), 1);
    drop(repo);
    let repo = Repository::open(directory.path()).unwrap();
    let mut actual = repo.playback_checkpoint().unwrap().unwrap();
    assert_eq!(
        serde_json::to_value(&actual).unwrap(),
        serde_json::to_value(&expected).unwrap()
    );
    assert_eq!(
        actual
            .queue
            .advance(AdvanceReason::Ended, |_| true)
            .unwrap()
            .track_id,
        "t"
    );
    assert_eq!(actual.queue.temporary.len(), 1);
    repo.integrity_check().unwrap();
}
#[test]
fn optimistic_conflict_and_invalid_control_data_leave_state_and_outbox_unchanged() {
    let directory = directory();
    let repo = Repository::open(directory.path()).unwrap();
    let state = checkpoint();
    repo.save_playback_checkpoint(&state, None, "local")
        .unwrap();
    assert!(
        repo.save_playback_checkpoint(&state, Some(0), "local")
            .is_err()
    );
    let mut invalid = state.clone();
    invalid.position_ms = u64::MAX;
    assert!(
        repo.save_playback_checkpoint(&invalid, None, "local")
            .is_err()
    );
    invalid = state.clone();
    invalid.lyric_offset_ms = i64::MIN;
    assert!(
        repo.save_playback_checkpoint(&invalid, None, "local")
            .is_err()
    );
    invalid = state.clone();
    invalid.quality = "foo".into();
    assert!(
        repo.save_playback_checkpoint(&invalid, None, "local")
            .is_err()
    );
    invalid.quality = "x\nsecret".into();
    assert!(
        repo.save_playback_checkpoint(&invalid, None, "local")
            .is_err()
    );
    assert!(repo.save_playback_checkpoint(&state, None, "").is_err());
    assert_eq!(repo.revision().unwrap(), 1);
    assert_eq!(repo.changes_since(0, 10).unwrap().len(), 1);
    assert_eq!(
        serde_json::to_value(repo.playback_checkpoint().unwrap()).unwrap(),
        serde_json::to_value(Some(state)).unwrap()
    );
}
#[test]
fn duplicate_entry_identity_is_rejected_before_commit() {
    let directory = directory();
    let repo = Repository::open(directory.path()).unwrap();
    let mut invalid = checkpoint();
    invalid.queue.entries[1].entry_id = invalid.queue.entries[0].entry_id.clone();
    assert!(
        repo.save_playback_checkpoint(&invalid, None, "local")
            .is_err()
    );
    assert_eq!(repo.revision().unwrap(), 0);
    assert!(repo.playback_checkpoint().unwrap().is_none());
}
#[test]
fn schema_four_upgrade_preserves_lists_and_creates_backup_without_a_restore_record() {
    let directory = directory();
    let repo = Repository::open(directory.path()).unwrap();
    drop(repo);
    let connection = rusqlite::Connection::open(directory.path().join(DATABASE_NAME)).unwrap();
    connection
        .execute_batch("DROP TABLE download_allocation_intents; DROP TABLE tag_edit_artifacts; DROP TABLE tag_edit_operations; DROP TABLE tag_write_approvals; DROP TABLE native_backup_archives; DROP TABLE native_backup_imports; DROP TABLE physical_snapshot_operations; DROP TABLE physical_snapshot_state; DROP TABLE sync_mode_preferences; DROP TABLE sync_capture_override; DROP TABLE sync_baselines; DROP TABLE sync_current; DROP TABLE sync_journal; DROP TABLE sync_list_metadata; DROP TABLE sync_snapshots; DROP TABLE sync_peers; DROP TABLE sync_profile_state; DROP TABLE download_owned_artifacts; DROP TABLE backup_list_metadata; DROP TABLE compat_settings; DROP TABLE backup_imports; DROP TABLE download_tasks; DROP TABLE download_settings; DROP TABLE download_destination_grants; DROP INDEX tracks_lx_identity; DROP TABLE library_relink_results; DROP TABLE library_relink_candidates; DROP TABLE library_identity_reservations; DROP TABLE library_dirty_state; DROP TABLE library_watch_preferences; DROP TABLE library_root_locations; DROP TABLE library_scan_seen; DROP TABLE library_scan_runs; DROP TABLE library_membership; DROP TABLE local_asset_locations; DROP TABLE local_file_grants; DROP TABLE library_roots; DROP TABLE artwork_info; DROP TABLE artwork_blobs; DROP TABLE resolved_media; DROP TABLE source_matches; DROP TABLE playback_state; PRAGMA user_version=4;")
        .unwrap();
    drop(connection);
    let repo = Repository::open(directory.path()).unwrap();
    assert_eq!(
        rusqlite::Connection::open(repo.path())
            .unwrap()
            .pragma_query_value(None, "user_version", |row| row.get::<_, u32>(0))
            .unwrap(),
        SCHEMA_VERSION
    );
    assert!(repo.playback_checkpoint().unwrap().is_none());
    assert_eq!(repo.list_infos().unwrap().len(), 3);
    repo.integrity_check().unwrap();
    assert!(std::fs::read_dir(directory.path()).unwrap().any(|p| {
        p.unwrap()
            .file_name()
            .to_string_lossy()
            .contains("schema-4-")
    }));
}
#[test]
fn invalid_persisted_queue_is_reported_without_replacing_its_document() {
    let directory = directory();
    let repo = Repository::open(directory.path()).unwrap();
    let state = checkpoint();
    repo.save_playback_checkpoint(&state, None, "local")
        .unwrap();
    drop(repo);
    let mut value = serde_json::to_value(&state).unwrap();
    value["queue"]["rng"] = serde_json::json!(0);
    let document = serde_json::to_string(&value).unwrap();
    let connection = rusqlite::Connection::open(directory.path().join(DATABASE_NAME)).unwrap();
    connection
        .execute(
            "UPDATE playback_state SET document=?1 WHERE singleton=1",
            [&document],
        )
        .unwrap();
    drop(connection);
    let repo = Repository::open(directory.path()).unwrap();
    assert!(repo.playback_checkpoint().is_err());
    assert_eq!(repo.revision().unwrap(), 1);
    drop(repo);
    let connection = rusqlite::Connection::open(directory.path().join(DATABASE_NAME)).unwrap();
    let retained: String = connection
        .query_row(
            "SELECT document FROM playback_state WHERE singleton=1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(retained, document);
}

#[test]
fn fifty_thousand_stable_ids_fit_the_checkpoint_budget_and_restore_full_order() {
    let directory = directory();
    let repo = Repository::open(directory.path()).unwrap();
    let mut state = PlaybackCheckpoint::default();
    state.queue.set_list(
        "00000000-0000-4000-8000-000000000000".into(),
        (0..50_000u64)
            .map(|id| format!("track:{id:064x}"))
            .collect(),
    );
    let bytes = serde_json::to_vec(&state).unwrap();
    assert!(bytes.len() > 8 * 1024 * 1024);
    repo.save_playback_checkpoint(&state, Some(0), "bulk-list")
        .unwrap();
    drop(repo);
    let repo = Repository::open(directory.path()).unwrap();
    let restored = repo.playback_checkpoint().unwrap().unwrap();
    assert_eq!(restored.queue.entries.len(), 50_000);
    assert_eq!(restored.queue.entries.first(), state.queue.entries.first());
    assert_eq!(restored.queue.entries.last(), state.queue.entries.last());
    repo.integrity_check().unwrap();
}

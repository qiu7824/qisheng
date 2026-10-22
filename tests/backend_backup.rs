#[path = "support/library_fixture.rs"]
mod fixture;
use fixture::Running;
use lx_gpui_player::backend::{BackendCommand, BackendQuery, BackendQueryResult};
use std::fs;

fn prepare(running: &Running, id: &str, path: std::path::PathBuf) {
    let cursor = running.backend.event_cursor();
    running.execute(
        id,
        BackendCommand::PreviewLxBackup {
            path,
            replace_tracks: true,
            copy_list_id: None,
        },
    );
    assert!(fixture::operation_finished(&running.backend, cursor, id).0);
}
#[test]
fn real_selected_backup_preview_is_inert_then_commit_restores_lists_and_safe_preferences_only() {
    let running = Running::new();
    let path = running.folder("selected-backup").join("original.lxmc");
    let input = serde_json::json!({"type":"allData_v2","setting":{"player.volume":0.31,"player.isMute":true,"sync.enable":true,
        "openAPI.enable":true,"sourceApprovals":{"malicious":true},"unknownSetting":"preserved"},"playList":[
        {"id":"imported-list","name":"恢复列表","unknownList":7,"list":[{"id":"kw_imported","source":"kw","name":"恢复歌曲","singer":"作者",
            "meta":{"songId":"imported","albumName":"专辑","_qualitys":{"128k":{}},"qualitys":[{"type":"128k"}]}}]}]});
    fs::write(&path, serde_json::to_vec(&input).unwrap()).unwrap();
    let before = running.backend.snapshot_with_cursor().unwrap().0;
    prepare(&running, "backup-inert-preview", path);
    assert_eq!(running.backend.snapshot().revision, before.revision);
    let BackendQueryResult::LxBackupPreview(Some(preview)) = running
        .backend
        .query(BackendQuery::LxBackupPreview)
        .unwrap()
    else {
        panic!("Preview")
    };
    assert!(preview.settings_present);
    assert_eq!(preview.imported_lists, 1);
    assert!(preview.requires_activation.contains(&"sync.enable".into()));
    running.execute(
        "backup-real-commit",
        BackendCommand::CommitLxBackup {
            preview_operation_id: preview.operation_id,
        },
    );
    let BackendQueryResult::ListTracks(rows) = running
        .backend
        .query(BackendQuery::ListTracks {
            list_id: "imported-list".into(),
            offset: 0,
            limit: 10,
        })
        .unwrap()
    else {
        panic!("Tracks")
    };
    assert_eq!(rows.items.len(), 1);
    assert_eq!(rows.items[0].title, "恢复歌曲");
    assert!((running.backend.snapshot().playback.volume - 0.31).abs() < 0.001);
    assert!(running.backend.snapshot().audio_output.preferences.muted);
    assert!(running.backend.snapshot().sources.is_empty());
    let BackendQueryResult::LxBackupImportResult(Some(result)) = running
        .backend
        .query(BackendQuery::LxBackupImportResult {
            operation_id: "backup-real-commit".into(),
        })
        .unwrap()
    else {
        panic!("Receipt")
    };
    assert_eq!(result.operation_id, "backup-real-commit");
    assert!(result.revision > before.revision);
    assert!(!result.input_digest.is_empty());
}
#[test]
fn stale_backup_preview_cannot_overwrite_new_user_settings_or_lists() {
    let running = Running::new();
    let path = running.folder("stale-backup").join("settings.json");
    fs::write(
        &path,
        br#"{"type":"setting_v2","data":{"player.volume":0.12}}"#,
    )
    .unwrap();
    prepare(&running, "backup-stale-preview", path);
    running.execute(
        "backup-user-volume",
        BackendCommand::Volume { volume: 0.63 },
    );
    let revision = running.backend.snapshot().revision;
    assert!(
        running
            .backend
            .execute(fixture::envelope(
                "backup-stale-commit",
                BackendCommand::CommitLxBackup {
                    preview_operation_id: "backup-stale-preview".into()
                }
            ))
            .is_err()
    );
    assert_eq!(running.backend.snapshot().revision, revision);
    assert!((running.backend.snapshot().playback.volume - 0.63).abs() < 0.001);
}

#[test]
fn restart_finishes_sql_only_backup_and_never_reapplies_acknowledged_preferences() {
    use lx_gpui_player::{
        backup::{BackupEncoding, ConflictPolicy, decode_lx},
        repository::Repository,
    };
    use std::time::Duration;
    let mut running = Running::new();
    running.execute(
        "before-recovery-volume",
        BackendCommand::Volume { volume: 0.64 },
    );
    running.backend.shutdown(Duration::from_secs(5)).unwrap();
    let repo = Repository::open(running.profile()).unwrap();
    let input=decode_lx(br#"{"type":"setting_v2","data":{"player.volume":0.27,"player.isMute":true,"sync.enable":true}}"#,BackupEncoding::Json).unwrap();
    let preview = repo
        .preview_lx_backup(&input, ConflictPolicy::ReplaceTracks)
        .unwrap();
    repo.commit_lx_backup("sql-only-recovery", &preview, &|| Ok(()))
        .unwrap();
    assert_eq!(repo.pending_lx_backup_preferences().unwrap().len(), 1);
    drop(repo);
    running.restart();
    assert!((running.backend.snapshot().playback.volume - 0.27).abs() < 0.001);
    assert!(running.backend.snapshot().audio_output.preferences.muted);
    running.execute(
        "after-recovery-volume",
        BackendCommand::Volume { volume: 0.83 },
    );
    running.restart();
    assert!((running.backend.snapshot().playback.volume - 0.83).abs() < 0.001);
    running.backend.shutdown(Duration::from_secs(5)).unwrap();
    let repo = Repository::open(running.profile()).unwrap();
    assert!(repo.pending_lx_backup_preferences().unwrap().is_empty());
    drop(repo);
}

#[test]
fn restart_acknowledges_saved_marker_after_sql_ack_failure_without_overwriting_later_volume() {
    use lx_gpui_player::{repository::DATABASE_NAME, store::ProjectStore};
    use rusqlite::Connection;
    let mut running = Running::new();
    let path = running.folder("ack-failure-backup").join("settings.json");
    fs::write(
        &path,
        br#"{"type":"setting_v2","data":{"player.volume":0.22}}"#,
    )
    .unwrap();
    prepare(&running, "ack-failure-preview", path);
    let connection = Connection::open(running.profile().join(DATABASE_NAME)).unwrap();
    connection.execute_batch("CREATE TRIGGER fail_backup_ack BEFORE UPDATE OF document ON backup_imports BEGIN SELECT RAISE(ABORT,'injected preference ACK failure'); END;").unwrap();
    assert!(
        running
            .backend
            .execute(fixture::envelope(
                "ack-failure-import",
                BackendCommand::CommitLxBackup {
                    preview_operation_id: "ack-failure-preview".into()
                }
            ))
            .is_err()
    );
    let BackendQueryResult::LxBackupImportResult(Some(receipt)) = running
        .backend
        .query(BackendQuery::LxBackupImportResult {
            operation_id: "ack-failure-import".into(),
        })
        .unwrap()
    else {
        panic!("Durable receipt")
    };
    let store = ProjectStore::open(running.profile()).unwrap();
    assert!(
        store
            .backup_preferences_applied(&receipt.operation_id, &receipt.input_digest)
            .unwrap()
    );
    assert!((store.load().unwrap().volume - 0.22).abs() < 0.001);
    running.execute(
        "after-failed-ack-user-volume",
        BackendCommand::Volume { volume: 0.91 },
    );
    connection
        .execute_batch("DROP TRIGGER fail_backup_ack;")
        .unwrap();
    drop(connection);
    drop(store);
    running.restart();
    assert!((running.backend.snapshot().playback.volume - 0.91).abs() < 0.001);
    running.restart();
    assert!((running.backend.snapshot().playback.volume - 0.91).abs() < 0.001);
}

#[test]
fn pending_backup_does_not_replace_a_corrupt_player_state_with_defaults() {
    use lx_gpui_player::{
        backend::{BackendConfig, BackendHandle},
        backup::{BackupEncoding, ConflictPolicy, decode_lx},
        repository::Repository,
    };
    use std::time::Duration;
    let running = Running::new();
    running.execute(
        "corrupt-before-volume",
        BackendCommand::Volume { volume: 0.65 },
    );
    running.backend.shutdown(Duration::from_secs(5)).unwrap();
    let repo = Repository::open(running.profile()).unwrap();
    let input = decode_lx(
        br#"{"type":"setting_v2","data":{"player.volume":0.17}}"#,
        BackupEncoding::Json,
    )
    .unwrap();
    let preview = repo
        .preview_lx_backup(&input, ConflictPolicy::ReplaceTracks)
        .unwrap();
    repo.commit_lx_backup("corrupt-state-pending", &preview, &|| Ok(()))
        .unwrap();
    drop(repo);
    let state = running.profile().join("player-state.json");
    let original = fs::read(&state).unwrap();
    fs::write(&state, b"corrupt player state").unwrap();
    assert!(BackendHandle::start(BackendConfig::headless(running.profile())).is_err());
    assert_eq!(fs::read(&state).unwrap(), b"corrupt player state");
    let repo = Repository::open(running.profile()).unwrap();
    assert_eq!(repo.pending_lx_backup_preferences().unwrap().len(), 1);
    drop(repo);
    fs::write(&state, original).unwrap();
    let backend = BackendHandle::start(BackendConfig::headless(running.profile())).unwrap();
    assert!((backend.snapshot().playback.volume - 0.17).abs() < 0.001);
    backend.shutdown(Duration::from_secs(5)).unwrap();
}

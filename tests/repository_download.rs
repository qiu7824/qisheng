use lx_gpui_player::{
    domain::music::{LxMusicDtoVersion, TrackRecord},
    download::{
        CollisionPolicy, DownloadState, FileIdentity, planning::DownloadQuality, task_model::*,
    },
    repository::{ChangeKind, Repository, SCHEMA_VERSION},
};
use std::{
    path::Path,
    sync::atomic::{AtomicUsize, Ordering},
};

fn identity() -> FileIdentity {
    FileIdentity {
        device: 1,
        object: "aa".repeat(16),
    }
}
fn grant(path: &Path) -> StoredDestinationGrant {
    StoredDestinationGrant {
        id: "destination".into(),
        profile_id: "profile".into(),
        path: path.to_path_buf(),
        identity: identity(),
        epoch: 1,
        revision: 1,
        revoked: false,
    }
}
fn task(index: u32) -> DownloadTask {
    let original_record = TrackRecord::from_lx_dto(serde_json::json!({"source":"kw","songmid":format!("song-{index}"),
        "name":"歌","singer":"人","albumName":"专辑","interval":"00:36","types":[{"type":"128k"}],"_types":{"128k":{}}}), LxMusicDtoVersion::Legacy).unwrap();
    let settings = DownloadSettings::default();
    let stages = DownloadStageRecords::for_settings(&settings, DownloadQuality::K128);
    DownloadTask {
        id: format!("task-{index}"),
        profile_id: "profile".into(),
        original_record,
        resolved_record: None,
        requested_quality: DownloadQuality::K128,
        actual_quality: DownloadQuality::K128,
        list_id: None,
        list_name_snapshot: None,
        order: index % 100,
        revision: 1,
        operation_epoch: 1,
        settings,
        destination: DownloadDestinationBinding {
            grant_id: "destination".into(),
            grant_epoch: 1,
            root_identity: identity(),
            relative_components: vec![format!("song-{index}.mp3")],
            collision: CollisionPolicy::Skip,
        },
        state: DownloadState::Waiting,
        pause_reason: None,
        owned_part: None,
        original_transfer: None,
        original_representation: None,
        url_bookmark: None,
        transfer_attempts: 0,
        retry_at_ms: None,
        error: None,
        output_audio: None,
        audio_available: false,
        stages,
        commit_journal: None,
        created_at_ms: 1,
        updated_at_ms: 1,
    }
}
fn profile() -> tempfile::TempDir {
    tempfile::tempdir_in(std::env::temp_dir()).unwrap()
}
fn setup(repo: &Repository, path: &Path) {
    repo.put_download_destination(&grant(path), None, None, &|| Ok(()))
        .unwrap();
}

#[test]
fn restart_pauses_tasks_and_preserves_metadata_without_opening_missing_destination() {
    let profile = profile();
    let destination = profile.path().join("does-not-exist");
    let repo = Repository::open(profile.path()).unwrap();
    setup(&repo, &destination);
    repo.insert_download_tasks(&[task(0)], None, &|| Ok(()))
        .unwrap();
    let mut active = repo.download_task("task-0").unwrap().unwrap();
    active.revision += 1;
    active.state = DownloadState::Transferring;
    repo.update_download_task(&active, 1, 1, None, &|| Ok(()))
        .unwrap();
    let before = repo.revision().unwrap();
    drop(repo);
    let repo = Repository::open(profile.path()).unwrap();
    let paused = repo.download_task("task-0").unwrap().unwrap();
    assert_eq!(paused.state, DownloadState::Paused);
    assert_eq!(paused.operation_epoch, 2);
    assert_eq!(paused.revision, 3);
    assert_eq!(repo.revision().unwrap(), before + 1);
    assert!(!destination.exists());
    assert_eq!(
        repo.changes_since(before, 10).unwrap()[0].kind,
        ChangeKind::DownloadTasksInterrupted
    );
    let revision = repo.revision().unwrap();
    drop(repo);
    let repo = Repository::open(profile.path()).unwrap();
    assert_eq!(repo.revision().unwrap(), revision);
}

#[test]
fn capacity_and_duplicate_quality_identity_fail_atomically_with_revision_and_outbox_unchanged() {
    let profile = profile();
    let repo = Repository::open(profile.path()).unwrap();
    setup(&repo, profile.path());
    let rows = (0..100).map(task).collect::<Vec<_>>();
    repo.insert_download_tasks(&rows, None, &|| Ok(())).unwrap();
    let revision = repo.revision().unwrap();
    assert!(
        repo.insert_download_tasks(&[task(100)], Some(revision), &|| Ok(()))
            .is_err()
    );
    assert_eq!(repo.download_tasks().unwrap().len(), 100);
    assert_eq!(repo.revision().unwrap(), revision);
    assert!(repo.changes_since(revision, 10).unwrap().is_empty());
    let mut duplicate = task(0);
    duplicate.id = "other-task".into();
    duplicate.order = 99;
    assert!(
        repo.insert_download_tasks(&[duplicate], None, &|| Ok(()))
            .is_err()
    );
    assert_eq!(repo.revision().unwrap(), revision);
}

#[test]
fn actual_mid_batch_sql_failure_and_cooperative_cancel_roll_back_all_download_records() {
    let profile = profile();
    let repo = Repository::open(profile.path()).unwrap();
    setup(&repo, profile.path());
    let db = rusqlite::Connection::open(repo.path()).unwrap();
    db.execute_batch("CREATE TRIGGER download_failure BEFORE INSERT ON download_tasks WHEN NEW.id='task-1' BEGIN SELECT RAISE(ABORT,'test failure'); END;").unwrap();
    let revision = repo.revision().unwrap();
    assert!(
        repo.insert_download_tasks(&[task(0), task(1)], None, &|| Ok(()))
            .is_err()
    );
    assert!(repo.download_tasks().unwrap().is_empty());
    assert_eq!(repo.revision().unwrap(), revision);
    db.execute_batch("DROP TRIGGER download_failure").unwrap();
    let calls = AtomicUsize::new(0);
    assert!(
        repo.insert_download_tasks(&[task(0), task(1)], None, &|| {
            if calls.fetch_add(1, Ordering::SeqCst) >= 3 {
                anyhow::bail!("Cancelled before durable commit");
            }
            Ok(())
        })
        .is_err()
    );
    assert!(repo.download_tasks().unwrap().is_empty());
    assert_eq!(repo.revision().unwrap(), revision);
    assert!(repo.changes_since(revision, 10).unwrap().is_empty());
}

#[test]
fn stale_task_epochs_and_destination_revocation_cannot_reactivate_a_transfer() {
    let profile = profile();
    let repo = Repository::open(profile.path()).unwrap();
    setup(&repo, profile.path());
    repo.insert_download_tasks(&[task(0)], None, &|| Ok(()))
        .unwrap();
    let mut paused = task(0);
    paused.revision = 2;
    paused.operation_epoch = 2;
    paused.state = DownloadState::Paused;
    repo.update_download_task(&paused, 1, 1, None, &|| Ok(()))
        .unwrap();
    let mut stale = paused.clone();
    stale.revision = 3;
    stale.state = DownloadState::Transferring;
    assert!(
        repo.update_download_task(&stale, 1, 1, None, &|| Ok(()))
            .is_err()
    );
    let mut revoked = grant(profile.path());
    revoked.revoked = true;
    revoked.epoch = 2;
    revoked.revision = 2;
    repo.put_download_destination(&revoked, Some(1), None, &|| Ok(()))
        .unwrap();
    assert!(
        repo.update_download_task(&stale, 2, 2, None, &|| Ok(()))
            .is_err()
    );
    let retained = repo.download_task("task-0").unwrap().unwrap();
    assert_eq!(retained.state, DownloadState::Paused);
    repo.remove_download_task("task-0", 2, 2, None).unwrap();
    assert!(repo.download_tasks().unwrap().is_empty());
}

#[test]
fn settings_require_a_current_selected_destination_and_preserve_exact_defaults() {
    let profile = profile();
    let repo = Repository::open(profile.path()).unwrap();
    assert_eq!(
        repo.download_settings().unwrap(),
        DownloadSettings::default()
    );
    let settings = DownloadSettings {
        destination_grant_id: Some("destination".into()),
        enabled: true,
        ..DownloadSettings::default()
    };
    let revision = repo.revision().unwrap();
    assert!(repo.save_download_settings(&settings, None).is_err());
    assert_eq!(repo.revision().unwrap(), revision);
    setup(&repo, profile.path());
    repo.save_download_settings(&settings, None).unwrap();
    assert_eq!(repo.download_settings().unwrap(), settings);
}

#[test]
fn actual_schema_nine_upgrade_preserves_tracks_and_records_a_pre_migration_backup() {
    let profile = profile();
    let repo = Repository::open(profile.path()).unwrap();
    let original = task(0).original_record;
    repo.upsert_track(&original).unwrap();
    let database = repo.path().to_path_buf();
    let revision = repo.revision().unwrap();
    drop(repo);
    let db = rusqlite::Connection::open(&database).unwrap();
    db.execute_batch("DROP TABLE download_allocation_intents; DROP TABLE tag_edit_artifacts; DROP TABLE tag_edit_operations; DROP TABLE tag_write_approvals; DROP TABLE native_backup_archives; DROP TABLE native_backup_imports; DROP TABLE physical_snapshot_operations; DROP TABLE physical_snapshot_state; DROP TABLE sync_mode_preferences; DROP TABLE sync_capture_override; DROP TABLE sync_baselines; DROP TABLE sync_current; DROP TABLE sync_journal; DROP TABLE sync_list_metadata; DROP TABLE sync_snapshots; DROP TABLE sync_peers; DROP TABLE sync_profile_state; DROP TABLE download_owned_artifacts; DROP TABLE backup_list_metadata; DROP TABLE compat_settings; DROP TABLE backup_imports; DROP TABLE download_tasks; DROP TABLE download_settings; DROP TABLE download_destination_grants; DROP INDEX tracks_lx_identity; PRAGMA user_version=9;").unwrap();
    drop(db);
    let repo = Repository::open(profile.path()).unwrap();
    assert_eq!(SCHEMA_VERSION, 17);
    assert_eq!(repo.track(&original.id).unwrap(), Some(original));
    assert_eq!(repo.revision().unwrap(), revision);
    assert!(repo.download_tasks().unwrap().is_empty());
    assert_eq!(
        std::fs::read_dir(profile.path())
            .unwrap()
            .filter(|entry| entry
                .as_ref()
                .unwrap()
                .path()
                .extension()
                .is_some_and(|ext| ext == "bak"))
            .count(),
        1
    );
    repo.integrity_check().unwrap();
}

#[test]
fn quality_replanning_collision_rolls_back_without_making_the_profile_unreadable() {
    let profile = profile();
    let repo = Repository::open(profile.path()).unwrap();
    setup(&repo, profile.path());
    let first = task(0);
    let mut second = first.clone();
    second.id = "flac-task".into();
    second.actual_quality = DownloadQuality::Flac;
    second.requested_quality = DownloadQuality::Flac;
    second.destination.relative_components = vec!["song-0.flac".into()];
    second.order = 1;
    second.state = DownloadState::Paused;
    repo.insert_download_tasks(&[first, second.clone()], None, &|| Ok(()))
        .unwrap();
    let revision = repo.revision().unwrap();
    second.actual_quality = DownloadQuality::K128;
    second.destination.relative_components = vec!["song-0.mp3".into()];
    second.revision = 2;
    second.operation_epoch = 2;
    assert!(
        repo.update_download_task(&second, 1, 1, None, &|| Ok(()))
            .is_err()
    );
    assert_eq!(repo.revision().unwrap(), revision);
    assert!(repo.changes_since(revision, 10).unwrap().is_empty());
    assert_eq!(repo.download_tasks().unwrap().len(), 2);
    drop(repo);
    let repo = Repository::open(profile.path()).unwrap();
    assert_eq!(repo.download_tasks().unwrap().len(), 2);
    repo.integrity_check().unwrap();
}

#[test]
fn artifact_accounting_ack_is_epoch_bound_quota_checked_and_independent_of_user_revision() {
    use lx_gpui_player::download::{
        DownloadContext, OwnedPartRecord,
        artifact::{OwnedArtifactAllocation, OwnedArtifactIntent, OwnedArtifactKind},
    };
    let profile = profile();
    let repo = Repository::open(profile.path()).unwrap();
    setup(&repo, profile.path());
    repo.insert_download_tasks(&[task(0)], None, &|| Ok(()))
        .unwrap();
    let mut value = repo.download_task("task-0").unwrap().unwrap();
    let context = DownloadContext {
        profile_id: "profile".into(),
        task_id: value.id.clone(),
        operation_epoch: 2,
        source_id: "11".repeat(32),
        source_content_sha256: "11".repeat(32),
        policy_epoch: 1,
    };
    value.operation_epoch = 2;
    value.revision += 1;
    value.state = DownloadState::AwaitingUrlApproval;
    value.url_bookmark = Some(
        PrivateUrlBookmark::new("http://127.0.0.1:1/approved.mp3".into(), context.clone()).unwrap(),
    );
    repo.update_download_task(&value, 1, 1, None, &|| Ok(()))
        .unwrap();
    let revision = repo.revision().unwrap();
    let allocation = |index: u8| {
        let nonce = format!("{index:02x}").repeat(16);
        OwnedArtifactAllocation {
            kind: OwnedArtifactKind::Transfer,
            record: OwnedPartRecord {
                basename: format!(".lx-download-{nonce}.part"),
                nonce,
                identity: FileIdentity {
                    device: 1,
                    object: format!("{index:02x}").repeat(16),
                },
                destination_identity: identity(),
                destination_epoch: 1,
                final_basename: "song-0.mp3".into(),
            },
            working_directory_path: profile.path().to_path_buf(),
            working_directory_identity: identity(),
            selected_root_identity: identity(),
            relative_group: None,
            reserved_max_bytes: 1024 * 1024 * 1024,
            context: context.clone(),
        }
    };
    for index in 1..=4 {
        let created = allocation(index);
        repo.record_download_allocation_intent(
            &OwnedArtifactIntent::from_created(&created),
            &|| Ok(()),
        )
        .unwrap();
        repo.record_download_artifact(&created, &|| Ok(())).unwrap();
    }
    assert!(
        repo.record_download_allocation_intent(
            &OwnedArtifactIntent::from_created(&allocation(5)),
            &|| Ok(())
        )
        .is_err()
    );
    assert!(
        repo.record_download_artifact(&allocation(5), &|| Ok(()))
            .is_err()
    );
    assert_eq!(repo.download_owned_artifacts(None).unwrap().len(), 4);
    assert_eq!(repo.revision().unwrap(), revision);
    assert!(repo.changes_since(revision, 10).unwrap().is_empty());
    let first = allocation(1);
    repo.verify_download_artifact(&first.record.nonce, 2, 100, &"22".repeat(32), &|| Ok(()))
        .unwrap();
    assert!(
        repo.publish_download_artifact(&first.record.nonce, 2, 100, &"22".repeat(32))
            .is_err()
    );
    value.revision += 1;
    value.state = DownloadState::Paused;
    value.output_audio = Some(DownloadArtifactProof {
        basename: "song-0.mp3".into(),
        identity: first.record.identity.clone(),
        bytes: 100,
        sha256: "22".repeat(32),
    });
    value.audio_available = true;
    repo.update_download_task(&value, 2, 2, None, &|| Ok(()))
        .unwrap();
    repo.publish_download_artifact(&first.record.nonce, 2, 100, &"22".repeat(32))
        .unwrap();
    repo.record_download_allocation_intent(
        &OwnedArtifactIntent::from_created(&allocation(5)),
        &|| Ok(()),
    )
    .unwrap();
    repo.record_download_artifact(&allocation(5), &|| Ok(()))
        .unwrap();
    let mut wrong = allocation(6);
    wrong.context.operation_epoch = 1;
    assert!(repo.record_download_artifact(&wrong, &|| Ok(())).is_err());
    let mut collision = first.clone();
    collision.record.identity.object = "ff".repeat(16);
    assert!(
        repo.record_download_artifact(&collision, &|| Ok(()))
            .is_err()
    );
    repo.integrity_check().unwrap();
}

#[test]
fn schema_ten_migration_preserves_paused_task_and_creates_recovery_tables_without_file_access() {
    let profile = profile();
    let repo = Repository::open(profile.path()).unwrap();
    setup(&repo, &profile.path().join("missing-destination"));
    let mut paused = task(0);
    paused.state = DownloadState::Paused;
    repo.insert_download_tasks(&[paused], None, &|| Ok(()))
        .unwrap();
    let revision = repo.revision().unwrap();
    let path = repo.path().to_path_buf();
    drop(repo);
    let db = rusqlite::Connection::open(path).unwrap();
    db.execute_batch("DROP TABLE download_allocation_intents; DROP TABLE tag_edit_artifacts; DROP TABLE tag_edit_operations; DROP TABLE tag_write_approvals; DROP TABLE native_backup_archives; DROP TABLE native_backup_imports; DROP TABLE physical_snapshot_operations; DROP TABLE physical_snapshot_state; DROP TABLE sync_mode_preferences; DROP TABLE sync_capture_override; DROP TABLE sync_baselines; DROP TABLE sync_current; DROP TABLE sync_journal; DROP TABLE sync_list_metadata; DROP TABLE sync_snapshots; DROP TABLE sync_peers; DROP TABLE sync_profile_state; DROP TABLE download_owned_artifacts; DROP TABLE backup_list_metadata; DROP TABLE compat_settings; DROP TABLE backup_imports; PRAGMA user_version=10;").unwrap();
    drop(db);
    let repo = Repository::open(profile.path()).unwrap();
    assert_eq!(repo.revision().unwrap(), revision);
    assert_eq!(
        repo.download_tasks().unwrap()[0].state,
        DownloadState::Paused
    );
    assert!(repo.download_owned_artifacts(None).unwrap().is_empty());
    repo.integrity_check().unwrap();
    assert!(!profile.path().join("missing-destination").exists());
}

use lx_gpui_player::{
    backup::{BackupEncoding, ConflictPolicy, LxBackup, decode_lx},
    domain::{
        library::LibraryPreparedAsset,
        lists::{ListAction, ListInfo},
        music::{LxMusicDtoVersion, TrackRecord},
    },
    library::{authority::RootAuthority, scan::ScanContext},
    repository::{ChangeKind, Repository, local_track_from_path},
};
use rusqlite::Connection;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs,
    sync::{
        Arc, Barrier,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

fn profile() -> tempfile::TempDir {
    tempfile::tempdir_in(std::env::temp_dir()).unwrap()
}
fn song(id: &str) -> Value {
    json!({"id":format!("kw_{id}"),"name":format!("曲目-{id}"),"singer":"艺人","source":"kw","meta":{"songId":id,"albumName":"专辑","vendor":{"keep":true}}})
}
fn record(id: &str) -> TrackRecord {
    TrackRecord::from_lx_dto(song(id), LxMusicDtoVersion::Modern).unwrap()
}
fn list(id: &str, music: &str) -> Value {
    json!({"id":id,"name":format!("备份-{id}"),"source":"kw","sourceListId":"官方列表","locationUpdateTime":1234,"unknownList":{"text":"保留"},"list":[song(music)]})
}
fn backup(value: Value) -> LxBackup {
    decode_lx(&serde_json::to_vec(&value).unwrap(), BackupEncoding::Json).unwrap()
}
fn two_lists() -> LxBackup {
    backup(
        json!({"type":"allData_v2","setting":{"version":"2.1.0","player.volume":0.25,"sync.enable":true,"openAPI.enable":true,"unknownVisual":{"theme":"紫色"}},"playList":[list("a","1"),list("b","2")]}),
    )
}
fn count(connection: &Connection, table: &str) -> i64 {
    connection
        .query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
            row.get(0)
        })
        .unwrap()
}

#[test]
fn whole_import_commits_one_revision_outbox_and_compatible_archives() {
    let directory = profile();
    let repo = Repository::open(directory.path()).unwrap();
    repo.apply_list_action(
        &ListAction::Create {
            position: 0,
            lists: vec![
                ListInfo::user("a", "本地名称"),
                ListInfo::user("untouched", "保留原表"),
            ],
        },
        None,
    )
    .unwrap();
    repo.apply_list_action(
        &ListAction::MusicOverwrite {
            list_id: "untouched".into(),
            tracks: vec![record("old")],
        },
        None,
    )
    .unwrap();
    let expected = repo.revision().unwrap();
    let preview = repo
        .preview_lx_backup(&two_lists(), ConflictPolicy::ReplaceTracks)
        .unwrap();
    assert_eq!(preview.expected_user_revision, expected);
    let committed = repo
        .commit_lx_backup("import-all", &preview, &|| Ok(()))
        .unwrap();
    assert!(!committed.replayed);
    assert_eq!(committed.change.revision, expected + 1);
    assert_eq!(committed.change.kind, ChangeKind::ListsChanged);
    assert_eq!(
        repo.changes_since(expected, 20).unwrap().as_slice(),
        std::slice::from_ref(&committed.change)
    );
    let lists = repo.list_data().unwrap();
    assert_eq!(
        lists
            .user_list
            .iter()
            .map(|list| list.info.id.as_str())
            .collect::<Vec<_>>(),
        ["a", "untouched", "b"]
    );
    assert_eq!(lists.user_list[0].info.name, "本地名称");
    assert_eq!(lists.user_list[1].tracks[0].lx_id, "kw_old");
    assert_eq!(lists.user_list[2].tracks[0].lx_id, "kw_2");
    let metadata = repo.lx_list_metadata("a").unwrap().unwrap();
    assert_eq!(metadata.info.name, "本地名称");
    assert_eq!(metadata.wire["unknownList"], json!({"text":"保留"}));
    assert_eq!(metadata.info.source_list_id.as_deref(), Some("官方列表"));
    let settings = repo.lx_compat_settings().unwrap().unwrap();
    assert_eq!(settings.archived["unknownVisual"], json!({"theme":"紫色"}));
    assert_eq!(settings.archived["sync.enable"], true);
    assert_eq!(settings.preferences["sync.enable"], false);
    assert_eq!(settings.preferences["openAPI.enable"], false);
    drop(repo);
    let reopened = Repository::open(directory.path()).unwrap();
    assert_eq!(reopened.list_data().unwrap(), lists);
    assert_eq!(
        reopened
            .lx_backup_result("import-all")
            .unwrap()
            .unwrap()
            .change,
        committed.change
    );
    assert_eq!(reopened.lx_compat_settings().unwrap().unwrap(), settings);
}

#[test]
fn second_list_sql_failure_rolls_back_first_list_settings_receipt_and_outbox() {
    let directory = profile();
    let repo = Repository::open(directory.path()).unwrap();
    repo.apply_list_action(
        &ListAction::MusicOverwrite {
            list_id: "default".into(),
            tracks: vec![record("original")],
        },
        None,
    )
    .unwrap();
    let before = repo.list_data().unwrap();
    let revision = repo.revision().unwrap();
    let preview = repo
        .preview_lx_backup(&two_lists(), ConflictPolicy::ReplaceTracks)
        .unwrap();
    let connection = Connection::open(repo.path()).unwrap();
    connection.execute_batch("CREATE TRIGGER fail_backup_second_list BEFORE INSERT ON music_list_items WHEN NEW.list_id='b' BEGIN SELECT RAISE(ABORT,'injected second-list failure'); END;").unwrap();
    assert!(
        repo.commit_lx_backup("sql-failure", &preview, &|| Ok(()))
            .is_err()
    );
    assert_eq!(repo.list_data().unwrap(), before);
    assert_eq!(repo.revision().unwrap(), revision);
    assert!(repo.changes_since(revision, 20).unwrap().is_empty());
    for table in ["backup_imports", "backup_list_metadata", "compat_settings"] {
        assert_eq!(count(&connection, table), 0);
    }
    assert!(repo.lx_backup_result("sql-failure").unwrap().is_none());
    connection
        .execute_batch("DROP TRIGGER fail_backup_second_list;")
        .unwrap();
    let retry = repo
        .commit_lx_backup("sql-failure", &preview, &|| Ok(()))
        .unwrap();
    assert_eq!(retry.change.revision, revision + 1);
    assert_eq!(repo.list_tracks("a").unwrap()[0].lx_id, "kw_1");
    assert_eq!(repo.list_tracks("b").unwrap()[0].lx_id, "kw_2");
}

#[test]
fn stale_or_mutated_preview_cannot_overwrite_a_new_revision() {
    let directory = profile();
    let repo = Repository::open(directory.path()).unwrap();
    let preview = repo
        .preview_lx_backup(&two_lists(), ConflictPolicy::ReplaceTracks)
        .unwrap();
    repo.apply_list_action(
        &ListAction::MusicOverwrite {
            list_id: "love".into(),
            tracks: vec![record("newer")],
        },
        None,
    )
    .unwrap();
    let current = repo.list_data().unwrap();
    let revision = repo.revision().unwrap();
    assert!(
        repo.commit_lx_backup("stale", &preview, &|| Ok(()))
            .is_err()
    );
    let mut changed = preview.clone();
    changed.expected_user_revision = revision;
    assert!(
        repo.commit_lx_backup("changed-revision", &changed, &|| Ok(()))
            .is_err()
    );
    let fresh = repo
        .preview_lx_backup(&two_lists(), ConflictPolicy::ReplaceTracks)
        .unwrap();
    let mut modified = fresh.clone();
    modified.plan.lists.user_list[0].tracks[0].title = "篡改预览".into();
    assert!(
        repo.commit_lx_backup("changed-plan", &modified, &|| Ok(()))
            .is_err()
    );
    let mut modified_digest = fresh;
    modified_digest.input_digest = "a".repeat(64);
    assert!(
        repo.commit_lx_backup("changed-digest", &modified_digest, &|| Ok(()))
            .is_err()
    );
    assert_eq!(repo.list_data().unwrap(), current);
    assert_eq!(repo.revision().unwrap(), revision);
    assert!(repo.changes_since(revision, 20).unwrap().is_empty());
}

#[test]
fn cancellation_at_final_commit_check_restores_every_sql_write() {
    let directory = profile();
    let repo = Repository::open(directory.path()).unwrap();
    // A successful identical-size transaction establishes the actual final guard
    // call, without depending on the implementation's internal guard count.
    let checks = AtomicUsize::new(0);
    let fixture = two_lists();
    let first = repo
        .preview_lx_backup(&fixture, ConflictPolicy::ReplaceTracks)
        .unwrap();
    let calibration = repo
        .commit_lx_backup("calibrate", &first, &|| {
            checks.fetch_add(1, Ordering::AcqRel);
            Ok(())
        })
        .unwrap();
    repo.acknowledge_lx_backup_preferences("calibrate", &calibration.input_digest)
        .unwrap();
    let total = checks.load(Ordering::Acquire);
    assert!(total >= 4);
    let before = repo.list_data().unwrap();
    let revision = repo.revision().unwrap();
    let preview = repo
        .preview_lx_backup(&fixture, ConflictPolicy::ReplaceTracks)
        .unwrap();
    let cancelled = AtomicUsize::new(0);
    let result = repo.commit_lx_backup("cancel-at-commit", &preview, &|| {
        let called = cancelled.fetch_add(1, Ordering::AcqRel) + 1;
        anyhow::ensure!(called < total, "injected cancellation before commit");
        Ok(())
    });
    assert!(result.is_err());
    assert_eq!(cancelled.load(Ordering::Acquire), total);
    assert_eq!(repo.list_data().unwrap(), before);
    assert_eq!(repo.revision().unwrap(), revision);
    assert!(repo.changes_since(revision, 20).unwrap().is_empty());
    assert!(repo.lx_backup_result("cancel-at-commit").unwrap().is_none());
    assert!(
        repo.commit_lx_backup("cancel-before-start", &preview, &|| anyhow::bail!(
            "cancelled"
        ))
        .is_err()
    );
    assert_eq!(repo.revision().unwrap(), revision);
}

#[test]
fn concurrent_same_operation_is_idempotent_and_different_digest_is_refused() {
    let directory = profile();
    let repo = Arc::new(Repository::open(directory.path()).unwrap());
    let preview = Arc::new(
        repo.preview_lx_backup(&two_lists(), ConflictPolicy::ReplaceTracks)
            .unwrap(),
    );
    let expected = preview.expected_user_revision;
    let barrier = Arc::new(Barrier::new(3));
    let workers: Vec<_> = (0..2)
        .map(|_| {
            let repo = repo.clone();
            let preview = preview.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                repo.commit_lx_backup("one-operation", &preview, &|| Ok(()))
                    .unwrap()
            })
        })
        .collect();
    barrier.wait();
    let results: Vec<_> = workers
        .into_iter()
        .map(|worker| worker.join().unwrap())
        .collect();
    assert_eq!(results.iter().filter(|result| result.replayed).count(), 1);
    assert_eq!(results[0].change, results[1].change);
    assert_eq!(repo.revision().unwrap(), expected + 1);
    assert_eq!(repo.changes_since(expected, 20).unwrap().len(), 1);
    let changed =
        backup(json!({"type":"setting_v2","data":{"version":"2.1.0","player.volume":0.8}}));
    let changed = repo
        .preview_lx_backup(&changed, ConflictPolicy::ReplaceTracks)
        .unwrap();
    assert!(
        repo.commit_lx_backup("one-operation", &changed, &|| Ok(()))
            .is_err()
    );
    assert_eq!(repo.revision().unwrap(), expected + 1);
}

#[test]
fn imported_local_references_reuse_stable_binding_without_creating_read_grants() {
    let directory = profile();
    let media = directory.path().join("media");
    fs::create_dir(&media).unwrap();
    let path = media.join("selected.wav");
    let mut wav = Vec::new();
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&52_u32.to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16_u32.to_le_bytes());
    wav.extend_from_slice(&1_u16.to_le_bytes());
    wav.extend_from_slice(&1_u16.to_le_bytes());
    wav.extend_from_slice(&8000_u32.to_le_bytes());
    wav.extend_from_slice(&16000_u32.to_le_bytes());
    wav.extend_from_slice(&2_u16.to_le_bytes());
    wav.extend_from_slice(&16_u16.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&16_u32.to_le_bytes());
    wav.extend_from_slice(&[0; 16]);
    fs::write(&path, wav).unwrap();
    let authority =
        RootAuthority::open(&media, false, &ScanContext::new(Duration::from_secs(30))).unwrap();
    let opened = authority.open_file(&path).unwrap();
    let identity = opened.identity().clone();
    let (track, mut asset) = local_track_from_path(&path).unwrap();
    asset.fingerprint = Some(format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(&identity).unwrap())
    ));
    let mut item = LibraryPreparedAsset {
        track,
        asset,
        file_identity: Some(identity),
    };
    let repo = Repository::open(directory.path().join("profile")).unwrap();
    item = repo.prepare_library_identity(&item, &|| Ok(())).unwrap();
    repo.upsert_explicit_library_tracks(&[item.clone()], &|| Ok(()))
        .unwrap();
    let connection = Connection::open(repo.path()).unwrap();
    let grants = count(&connection, "local_file_grants");
    let assets = count(&connection, "local_assets");
    let unknown = media.join("never-selected.flac");
    let local = |id: &str, path: &std::path::Path| json!({"id":id,"name":"备份本地标签","singer":"","source":"local","meta":{"songId":path.to_str().unwrap(),"filePath":path.to_str().unwrap(),"ext":"flac"}});
    let input = backup(
        json!({"type":"playListPart_v2","data":{"id":"local-import","name":"本地引用","list":[local("external-selected-id", &path),local("external-unapproved-id", &unknown)]}}),
    );
    let preview = repo
        .preview_lx_backup(&input, ConflictPolicy::ReplaceTracks)
        .unwrap();
    assert_eq!(preview.plan.lists.user_list[0].tracks[0].id, item.track.id);
    assert_eq!(
        preview.plan.lists.user_list[0].tracks[0]
            .local_asset_id
            .as_deref(),
        Some(item.asset.id.as_str())
    );
    assert!(
        preview.plan.lists.user_list[0].tracks[1]
            .local_asset_id
            .is_none()
    );
    repo.commit_lx_backup("local-binding", &preview, &|| Ok(()))
        .unwrap();
    let imported = repo.list_tracks("local-import").unwrap();
    assert_eq!(imported[0].id, item.track.id);
    assert_eq!(imported[0].lx_id, "external-selected-id");
    assert!(imported[1].id.starts_with("unbound-local:"));
    assert!(imported[1].local_asset_id.is_none());
    assert_eq!(count(&connection, "local_file_grants"), grants);
    assert_eq!(count(&connection, "local_assets"), assets);
    assert!(repo.library_asset_at_path(&unknown).unwrap().is_none());
    assert!(!unknown.exists());
    drop(opened);
    let barrier = authority.retirement_barrier();
    drop(authority);
    assert!(barrier.wait(Duration::from_secs(3)));
}

#[test]
fn copied_and_kept_metadata_maps_to_the_actual_destination_and_receipts_are_bounded() {
    let directory = profile();
    let repo = Repository::open(directory.path()).unwrap();
    let original = backup(json!({"type":"playListPart_v2","data":list("a","1")}));
    let preview = repo
        .preview_lx_backup(&original, ConflictPolicy::ReplaceTracks)
        .unwrap();
    repo.commit_lx_backup("original", &preview, &|| Ok(()))
        .unwrap();
    let previous = repo.lx_list_metadata("a").unwrap().unwrap();
    let changed = backup(
        json!({"type":"playListPart_v2","data":{"id":"a","name":"changed","unknownList":{"text":"不可覆盖"},"list":[song("2")]}}),
    );
    let kept = repo
        .preview_lx_backup(&changed, ConflictPolicy::KeepExisting)
        .unwrap();
    repo.commit_lx_backup("kept", &kept, &|| Ok(())).unwrap();
    assert_eq!(repo.lx_list_metadata("a").unwrap().unwrap(), previous);
    let copied = repo
        .preview_lx_backup(
            &changed,
            ConflictPolicy::Copy {
                new_list_id: "copy-a".into(),
            },
        )
        .unwrap();
    repo.commit_lx_backup("copy", &copied, &|| Ok(())).unwrap();
    let metadata = repo.lx_list_metadata("copy-a").unwrap().unwrap();
    assert_eq!(metadata.info.id, "copy-a");
    assert_eq!(metadata.wire["id"], "copy-a");
    assert_eq!(metadata.wire["unknownList"], json!({"text":"不可覆盖"}));
    let settings =
        backup(json!({"type":"setting_v2","data":{"version":"2.1.0","player.volume":0.5}}));
    for index in 0..65 {
        let preview = repo
            .preview_lx_backup(&settings, ConflictPolicy::ReplaceTracks)
            .unwrap();
        let result = repo
            .commit_lx_backup(&format!("bounded-{index}"), &preview, &|| Ok(()))
            .unwrap();
        repo.acknowledge_lx_backup_preferences(&result.operation_id, &result.input_digest)
            .unwrap();
    }
    let connection = Connection::open(repo.path()).unwrap();
    assert_eq!(count(&connection, "backup_imports"), 64);
    assert!(repo.lx_backup_result("bounded-0").unwrap().is_none());
    assert!(repo.lx_backup_result("bounded-64").unwrap().is_some());
    let before = repo.revision().unwrap();
    assert!(
        repo.commit_lx_backup("original", &preview, &|| Ok(()))
            .is_err()
    );
    assert_eq!(repo.revision().unwrap(), before);
}

#[test]
fn pending_preferences_are_durable_bounded_and_ack_does_not_add_a_revision() {
    let directory = profile();
    let repo = Repository::open(directory.path()).unwrap();
    let preview = repo
        .preview_lx_backup(&two_lists(), ConflictPolicy::ReplaceTracks)
        .unwrap();
    let receipt = repo
        .commit_lx_backup("pending-prefs", &preview, &|| Ok(()))
        .unwrap();
    let revision = repo.revision().unwrap();
    let pending = repo.pending_lx_backup_preferences().unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].operation_id, receipt.operation_id);
    let next = repo
        .preview_lx_backup(&two_lists(), ConflictPolicy::ReplaceTracks)
        .unwrap();
    assert!(
        repo.commit_lx_backup("must-not-prune-pending", &next, &|| Ok(()))
            .is_err()
    );
    assert!(
        repo.acknowledge_lx_backup_preferences("pending-prefs", &"0".repeat(64))
            .is_err()
    );
    assert_eq!(repo.revision().unwrap(), revision);
    drop(repo);
    let repo = Repository::open(directory.path()).unwrap();
    assert_eq!(repo.pending_lx_backup_preferences().unwrap().len(), 1);
    repo.acknowledge_lx_backup_preferences(&receipt.operation_id, &receipt.input_digest)
        .unwrap();
    repo.acknowledge_lx_backup_preferences(&receipt.operation_id, &receipt.input_digest)
        .unwrap();
    assert!(repo.pending_lx_backup_preferences().unwrap().is_empty());
    assert_eq!(repo.revision().unwrap(), revision);
    assert!(repo.changes_since(revision, 10).unwrap().is_empty());
}

#[test]
fn receipts_without_preference_phase_are_historical_and_are_not_replayed() {
    let directory = profile();
    let repo = Repository::open(directory.path()).unwrap();
    let preview = repo
        .preview_lx_backup(&two_lists(), ConflictPolicy::ReplaceTracks)
        .unwrap();
    let receipt = repo
        .commit_lx_backup("old-preference-receipt", &preview, &|| Ok(()))
        .unwrap();
    let connection = Connection::open(repo.path()).unwrap();
    let document: String = connection
        .query_row(
            "SELECT document FROM backup_imports WHERE operation_id=?1",
            [&receipt.operation_id],
            |row| row.get(0),
        )
        .unwrap();
    let mut value: Value = serde_json::from_str(&document).unwrap();
    value.as_object_mut().unwrap().remove("preferences_applied");
    let document = serde_json::to_string(&value).unwrap();
    connection
        .execute(
            "UPDATE backup_imports SET document=?2,byte_size=?3 WHERE operation_id=?1",
            rusqlite::params![receipt.operation_id, document, document.len() as i64],
        )
        .unwrap();
    assert!(repo.pending_lx_backup_preferences().unwrap().is_empty());
}

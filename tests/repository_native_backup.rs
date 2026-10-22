use lx_gpui_player::{
    backup::{
        ConflictPolicy, NativeBackup,
        native::{NativeDownloadPlan, NativeEditedLyrics, NativeSourceArchive},
    },
    domain::{
        lists::{ListAction, ListData, ListInfo, UserList},
        lyric::{LyricBundle, LyricField, LyricOrigin},
        music::{LxMusicDtoVersion, TrackRecord},
    },
    download::{planning::DownloadQuality, task_model::DownloadSettings},
    repository::{ChangeKind, NativeBackupPreview, Repository, SCHEMA_VERSION},
    source::import_script,
    store::{ProjectStore, StoreData},
};
use rusqlite::Connection;
use serde_json::json;
use std::{
    path::PathBuf,
    sync::atomic::{AtomicUsize, Ordering},
};

fn profile() -> tempfile::TempDir {
    tempfile::tempdir_in(std::env::temp_dir()).unwrap()
}
fn record(id: &str) -> TrackRecord {
    TrackRecord::from_lx_dto(json!({"id":format!("kw_{id}"),"source":"kw","name":format!("曲目-{id}"),"singer":"作者","meta":{"songId":id,"albumName":"专辑"}}), LxMusicDtoVersion::Modern).unwrap()
}
fn edited(id: &str) -> NativeEditedLyrics {
    NativeEditedLyrics {
        track_id: id.into(),
        bundle: LyricBundle {
            lyric: LyricField::Text("[00:01.00]编辑歌词".into()),
            tlyric: LyricField::Null,
            rlyric: LyricField::Text(String::new()),
            lxlyric: LyricField::Missing,
        },
    }
}
fn backup() -> NativeBackup {
    let song = record("restored");
    NativeBackup {
        format: "lx_native_backup".into(),
        version: 1,
        store: StoreData {
            volume: 0.23,
            audio_analysis_enabled: true,
            ..Default::default()
        },
        lists: ListData {
            default_list: vec![song.clone()],
            user_list: vec![UserList {
                info: ListInfo::user("imported", "恢复列表"),
                tracks: vec![song.clone()],
            }],
            ..Default::default()
        },
        dislike_rules: "  不喜欢@作者\n@艺人".into(),
        declared_local_paths: vec!["C:\\备份文件\\未选择.flac".into()],
        sources: vec![NativeSourceArchive {
            script: "/* @name Archived source */\nglobalThis.archived = true;".into(),
        }],
        downloads: Some(vec![NativeDownloadPlan {
            record: song.clone(),
            requested_quality: DownloadQuality::K128,
            list_id: Some("imported".into()),
            list_name_snapshot: Some("恢复列表".into()),
            order: 0,
            settings: DownloadSettings::default(),
        }]),
        edited_lyrics: Some(vec![edited(&song.id), edited("unknown-track-in-archive")]),
        extensions: json!({"future":{"version":7,"enabled":true}})
            .as_object()
            .unwrap()
            .clone(),
    }
}
fn sql_count(db: &Connection, table: &str) -> i64 {
    db.query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
        row.get(0)
    })
    .unwrap()
}

#[test]
fn import_commits_lists_dislike_known_edits_and_safe_archive_in_one_revision() {
    let directory = profile();
    let repo = Repository::open(directory.path()).unwrap();
    let data = backup();
    let preview = repo
        .preview_native_backup(&data, ConflictPolicy::ReplaceTracks)
        .unwrap();
    let before = repo.revision().unwrap();
    let committed = repo
        .commit_native_backup("native-whole", &preview, &|| Ok(()))
        .unwrap();
    assert_eq!(committed.change.revision, before + 1);
    assert_eq!(committed.change.kind, ChangeKind::ListsChanged);
    assert_eq!(
        repo.changes_since(before, 20).unwrap().as_slice(),
        std::slice::from_ref(&committed.change)
    );
    assert_eq!(
        repo.list_data().unwrap().default_list,
        data.lists.default_list
    );
    assert!(repo.dislike_rules().unwrap().matches("不喜欢", "作者"));
    assert_eq!(
        repo.lyrics(&record("restored").id, LyricOrigin::Edited)
            .unwrap(),
        Some(edited(&record("restored").id).bundle)
    );
    assert!(repo.track("unknown-track-in-archive").unwrap().is_none());
    let archive = repo.native_backup_archive("native-whole").unwrap().unwrap();
    assert_eq!(archive.edited_lyrics.as_ref().unwrap().len(), 2);
    assert_eq!(
        archive.edited_lyrics.as_ref().unwrap()[1].track_id,
        "unknown-track-in-archive"
    );
    assert_eq!(archive.declared_local_paths, data.declared_local_paths);
    assert_eq!(archive.extensions, data.extensions);
    assert!(!archive.download_plans.as_ref().unwrap()[0].settings.enabled);
    assert!(
        archive.download_plans.as_ref().unwrap()[0]
            .settings
            .destination_grant_id
            .is_none()
    );
    assert_eq!(committed.counts.applied_edited_lyrics, 1);
    assert_eq!(committed.counts.archived_edited_lyrics, 1);
    let db = Connection::open(repo.path()).unwrap();
    for table in [
        "download_tasks",
        "download_destination_grants",
        "local_file_grants",
        "library_roots",
    ] {
        assert_eq!(sql_count(&db, table), 0);
    }
    assert_eq!(repo.sync_profile_state().unwrap().sequence, 2);
    repo.integrity_check().unwrap();
}

#[test]
fn candidate_preserves_current_authority_and_appends_only_new_unapproved_scripts() {
    let directory = profile();
    let repo = Repository::open(directory.path()).unwrap();
    let mut data = backup();
    data.store.spatial = lx_gpui_player::audio::spatial::SpatialSettings {
        enabled: true,
        radius: 0.7,
        speed: 25,
    };
    let mut incoming_effects =
        lx_gpui_player::backend::EffectsPreferences::from_legacy_spatial(data.store.spatial);
    incoming_effects.settings.convolution.main_gain = 2.0;
    data.store.effects = Some(incoming_effects);
    let mut current_effects =
        lx_gpui_player::backend::EffectsPreferences::from_legacy_spatial(Default::default());
    current_effects.settings.convolution.main_gain = 0.5;
    let mut existing = import_script(&data.sources[0].script).unwrap();
    existing.approve().unwrap();
    let current = StoreData {
        selected: Some(existing.sha256.clone()),
        sources: vec![existing.clone()],
        local_files: vec![PathBuf::from("C:\\已选择.wav")],
        volume: 0.75,
        effects: Some(current_effects),
        ..Default::default()
    };
    data.sources.push(NativeSourceArchive {
        script: "/* @name New unapproved */\nglobalThis.newSource = 2;".into(),
    });
    let preview = repo
        .preview_native_backup(&data, ConflictPolicy::ReplaceTracks)
        .unwrap();
    let projected = preview.plan.project_store(&current);
    let projected_effects = projected.effects.as_ref().unwrap();
    assert_eq!(projected_effects.settings.spatial, projected.spatial);
    assert_eq!(projected_effects.settings.convolution.main_gain, 0.5);
    let prepared = repo
        .native_backup_preview_store_candidate(&preview, &current)
        .unwrap();
    let committed = repo
        .commit_native_backup("source-union", &preview, &|| Ok(()))
        .unwrap();
    let candidate = repo
        .native_backup_store_candidate(&committed, &current)
        .unwrap();
    assert_eq!(
        serde_json::to_value(prepared).unwrap(),
        serde_json::to_value(&candidate).unwrap()
    );
    assert_eq!(candidate.selected, current.selected);
    assert_eq!(candidate.local_files, current.local_files);
    assert_eq!(
        serde_json::to_value(&candidate.sources[0]).unwrap(),
        serde_json::to_value(existing).unwrap()
    );
    assert_eq!(candidate.sources.len(), 2);
    assert!(!candidate.sources[1].approved);
    assert_eq!(candidate.sources[1].approval_policy_version, 0);
    assert!(candidate.sources[1].require_approval().is_err());
    assert_eq!(candidate.volume, 0.23);
    assert_eq!(candidate.spatial, data.store.spatial);
    let effects = candidate.effects.as_ref().unwrap();
    assert_eq!(effects.settings.spatial, candidate.spatial);
    assert_eq!(effects.settings.convolution.main_gain, 0.5);
    assert_eq!(
        serde_json::to_value(&effects.resources).unwrap(),
        serde_json::to_value(&current.effects.as_ref().unwrap().resources).unwrap()
    );
}

#[test]
fn unchanged_operation_replays_without_revision_and_reused_identity_is_rejected() {
    let directory = profile();
    let repo = Repository::open(directory.path()).unwrap();
    let data = backup();
    let preview = repo
        .preview_native_backup(&data, ConflictPolicy::ReplaceTracks)
        .unwrap();
    let first = repo
        .commit_native_backup("same-operation", &preview, &|| Ok(()))
        .unwrap();
    let replay = repo
        .commit_native_backup("same-operation", &preview, &|| Ok(()))
        .unwrap();
    assert!(replay.replayed);
    assert_eq!(replay.change, first.change);
    assert_eq!(repo.revision().unwrap(), first.change.revision);
    let next = repo
        .preview_native_backup(&data, ConflictPolicy::ReplaceTracks)
        .unwrap();
    assert!(
        repo.commit_native_backup("same-operation", &next, &|| Ok(()))
            .unwrap_err()
            .to_string()
            .contains("reused")
    );
    let mut forged = first;
    forged.input_digest = "0".repeat(64);
    assert!(
        repo.native_backup_store_candidate(&forged, &StoreData::default())
            .is_err()
    );
}

fn tamper(preview: &mut NativeBackupPreview, field: usize) {
    match field {
        0 => preview.expected_user_revision += 1,
        1 => preview.input_digest = "0".repeat(64),
        2 => preview.plan.store_preferences.volume = 0.91,
        3 => preview.plan.list_plan.lists.default_list.clear(),
        4 => preview.plan.unapproved_sources[0].approved = true,
        5 => preview
            .plan
            .declared_local_paths
            .push("C:\\added.wav".into()),
        6 => preview.plan.dislike.rules.clear(),
        7 => {
            preview.plan.download_plans.as_mut().unwrap()[0]
                .settings
                .enabled = true
        }
        8 => preview.plan.edited_lyrics.as_mut().unwrap()[0].bundle.lyric = LyricField::Missing,
        9 => {
            preview
                .plan
                .extensions
                .insert("changed".into(), true.into());
        }
        _ => unreachable!(),
    }
}

#[test]
fn public_preview_mutation_and_repository_revision_conflict_are_rejected() {
    let directory = profile();
    let repo = Repository::open(directory.path()).unwrap();
    let preview = repo
        .preview_native_backup(&backup(), ConflictPolicy::ReplaceTracks)
        .unwrap();
    for field in 0..10 {
        let mut modified = preview.clone();
        tamper(&mut modified, field);
        assert!(
            repo.commit_native_backup(&format!("tampered-{field}"), &modified, &|| Ok(()))
                .unwrap_err()
                .to_string()
                .contains("modified")
        );
        assert!(
            repo.native_backup_preview_store_candidate(&modified, &StoreData::default())
                .is_err()
        );
    }
    let before = repo.revision().unwrap();
    repo.record_search("intervening-change", None).unwrap();
    assert!(
        repo.commit_native_backup("conflicted", &preview, &|| Ok(()))
            .unwrap_err()
            .to_string()
            .contains("revision conflict")
    );
    assert_eq!(repo.revision().unwrap(), before + 1);
    assert!(repo.native_backup_result("conflicted").unwrap().is_none());
}

#[test]
fn sql_lyric_failure_rolls_back_lists_dislike_archive_receipt_and_sync_cut() {
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
    let sequence = repo.sync_profile_state().unwrap().sequence;
    let preview = repo
        .preview_native_backup(&backup(), ConflictPolicy::ReplaceTracks)
        .unwrap();
    let db = Connection::open(repo.path()).unwrap();
    db.execute_batch("CREATE TRIGGER fail_native_edit BEFORE INSERT ON lyric_fields WHEN NEW.origin='edited' BEGIN SELECT RAISE(ABORT,'injected native edit failure'); END;").unwrap();
    assert!(
        repo.commit_native_backup("failed-sql", &preview, &|| Ok(()))
            .is_err()
    );
    assert_eq!(repo.list_data().unwrap(), before);
    assert_eq!(repo.revision().unwrap(), revision);
    assert_eq!(repo.sync_profile_state().unwrap().sequence, sequence);
    assert_eq!(repo.dislike_rules().unwrap().rules, "");
    assert!(repo.changes_since(revision, 20).unwrap().is_empty());
    for table in [
        "native_backup_imports",
        "native_backup_archives",
        "lyric_bundles",
    ] {
        assert_eq!(sql_count(&db, table), 0);
    }
}

#[test]
fn checked_cancellation_rolls_back_even_at_last_precommit_checkpoint() {
    let directory = profile();
    let repo = Repository::open(directory.path()).unwrap();
    let preview = repo
        .preview_native_backup(&backup(), ConflictPolicy::ReplaceTracks)
        .unwrap();
    let checkpoints = AtomicUsize::new(0);
    repo.commit_native_backup("measure-checkpoints", &preview, &|| {
        checkpoints.fetch_add(1, Ordering::SeqCst);
        Ok(())
    })
    .unwrap();
    let total = checkpoints.load(Ordering::SeqCst);
    assert!(total >= 7);
    for fail_at in [1, 5, total] {
        let directory = profile();
        let repo = Repository::open(directory.path()).unwrap();
        let preview = repo
            .preview_native_backup(&backup(), ConflictPolicy::ReplaceTracks)
            .unwrap();
        let before = repo.revision().unwrap();
        let calls = AtomicUsize::new(0);
        let result = repo.commit_native_backup("measure-checkpoints", &preview, &|| {
            if calls.fetch_add(1, Ordering::SeqCst) + 1 == fail_at {
                anyhow::bail!("cancelled at checked boundary");
            }
            Ok(())
        });
        assert!(result.is_err(), "checkpoint {fail_at} was not cancelled");
        assert_eq!(repo.revision().unwrap(), before);
        assert!(repo.list_data().unwrap().default_list.is_empty());
        assert_eq!(repo.dislike_rules().unwrap().rules, "");
        assert!(
            repo.native_backup_result("measure-checkpoints")
                .unwrap()
                .is_none()
        );
        assert_eq!(repo.sync_profile_state().unwrap().sequence, 0);
    }
}

#[test]
fn pending_saga_survives_restart_and_json_marker_prevents_reapplying_preferences() {
    let directory = profile();
    let repo = Repository::open(directory.path()).unwrap();
    let store = ProjectStore::open(directory.path().into()).unwrap();
    let current = StoreData {
        volume: 0.8,
        ..Default::default()
    };
    store.save(&current).unwrap();
    let preview = repo
        .preview_native_backup(&backup(), ConflictPolicy::ReplaceTracks)
        .unwrap();
    let committed = repo
        .commit_native_backup("pending-saga", &preview, &|| Ok(()))
        .unwrap();
    let candidate = repo
        .native_backup_store_candidate(&committed, &current)
        .unwrap();
    store
        .apply_native_backup_preferences(
            &committed.operation_id,
            &committed.input_digest,
            &candidate,
        )
        .unwrap();
    // Simulate exit between the JSON marker and its SQLite acknowledgement.
    drop(repo);
    drop(store);
    let repo = Repository::open(directory.path()).unwrap();
    let store = ProjectStore::open(directory.path().into()).unwrap();
    let pending = repo.pending_native_backup_preferences().unwrap();
    assert_eq!(pending.len(), 1);
    assert!(!pending[0].preferences_applied);
    assert!(
        store
            .backup_preferences_applied(&committed.operation_id, &committed.input_digest)
            .unwrap()
    );
    let mut later = store.load().unwrap();
    later.volume = 0.61;
    store.save(&later).unwrap();
    store
        .apply_native_backup_preferences(
            &committed.operation_id,
            &committed.input_digest,
            &candidate,
        )
        .unwrap();
    assert_eq!(store.load().unwrap().volume, 0.61);
    let revision = repo.revision().unwrap();
    repo.acknowledge_native_backup_preferences(&committed.operation_id, &committed.input_digest)
        .unwrap();
    assert!(repo.pending_native_backup_preferences().unwrap().is_empty());
    assert_eq!(repo.revision().unwrap(), revision);
    assert!(
        repo.native_backup_result("pending-saga")
            .unwrap()
            .unwrap()
            .preferences_applied
    );
}

#[test]
fn pending_native_and_lx_preference_sagas_cannot_overwrite_each_other() {
    let directory = profile();
    let repo = Repository::open(directory.path()).unwrap();
    let native = repo
        .preview_native_backup(&backup(), ConflictPolicy::ReplaceTracks)
        .unwrap();
    let native = repo
        .commit_native_backup("pending-native", &native, &|| Ok(()))
        .unwrap();
    let lx = lx_gpui_player::backup::decode_lx(
        &serde_json::to_vec(&json!({"type":"setting_v2","data":{"player.volume":0.6}})).unwrap(),
        lx_gpui_player::backup::BackupEncoding::Json,
    )
    .unwrap();
    let lx_preview = repo
        .preview_lx_backup(&lx, ConflictPolicy::ReplaceTracks)
        .unwrap();
    assert!(
        repo.commit_lx_backup("blocked-lx", &lx_preview, &|| Ok(()))
            .unwrap_err()
            .to_string()
            .contains("pending")
    );
    repo.acknowledge_native_backup_preferences(&native.operation_id, &native.input_digest)
        .unwrap();
    repo.commit_lx_backup("pending-lx", &lx_preview, &|| Ok(()))
        .unwrap();
    let native = repo
        .preview_native_backup(&backup(), ConflictPolicy::ReplaceTracks)
        .unwrap();
    assert!(
        repo.commit_native_backup("blocked-native", &native, &|| Ok(()))
            .unwrap_err()
            .to_string()
            .contains("pending")
    );
}

#[test]
fn receipt_limit_preserves_pending_import_and_prunes_completed_archive_pairs() {
    let directory = profile();
    let repo = Repository::open(directory.path()).unwrap();
    let mut data = backup();
    data.sources.clear();
    data.edited_lyrics = None;
    data.downloads = None;
    for index in 0..65 {
        let preview = repo
            .preview_native_backup(&data, ConflictPolicy::KeepExisting)
            .unwrap();
        let result = repo
            .commit_native_backup(&format!("bounded-{index}"), &preview, &|| Ok(()))
            .unwrap();
        if index < 64 {
            repo.acknowledge_native_backup_preferences(&result.operation_id, &result.input_digest)
                .unwrap();
        }
    }
    let db = Connection::open(repo.path()).unwrap();
    assert_eq!(sql_count(&db, "native_backup_imports"), 64);
    assert_eq!(sql_count(&db, "native_backup_archives"), 64);
    assert!(repo.native_backup_result("bounded-0").unwrap().is_none());
    assert_eq!(
        repo.pending_native_backup_preferences().unwrap()[0].operation_id,
        "bounded-64"
    );
    repo.integrity_check().unwrap();
}

#[test]
fn archive_tampering_is_rejected_and_schema_fourteen_migration_retains_data() {
    let directory = profile();
    let repo = Repository::open(directory.path()).unwrap();
    let preview = repo
        .preview_native_backup(&backup(), ConflictPolicy::ReplaceTracks)
        .unwrap();
    repo.commit_native_backup("archive-integrity", &preview, &|| Ok(()))
        .unwrap();
    let db = Connection::open(repo.path()).unwrap();
    let mut archive: serde_json::Value = serde_json::from_str(
        &db.query_row("SELECT document FROM native_backup_archives", [], |row| {
            row.get::<_, String>(0)
        })
        .unwrap(),
    )
    .unwrap();
    archive["extensions"]["tampered"] = true.into();
    let document = serde_json::to_string(&archive).unwrap();
    db.execute(
        "UPDATE native_backup_archives SET document=?1,byte_size=?2",
        rusqlite::params![document, document.len() as i64],
    )
    .unwrap();
    assert!(
        repo.native_backup_result("archive-integrity")
            .unwrap_err()
            .to_string()
            .contains("integrity")
    );
    let expected = repo.list_data().unwrap();
    let revision = repo.revision().unwrap();
    let path = repo.path().to_path_buf();
    drop(db);
    drop(repo);
    let db = Connection::open(path).unwrap();
    db.execute_batch("DROP TABLE download_allocation_intents; DROP TABLE tag_edit_artifacts; DROP TABLE tag_edit_operations; DROP TABLE tag_write_approvals; DROP TABLE native_backup_archives; DROP TABLE native_backup_imports; PRAGMA user_version=14;").unwrap();
    drop(db);
    let repo = Repository::open(directory.path()).unwrap();
    assert_eq!(SCHEMA_VERSION, 17);
    assert_eq!(repo.list_data().unwrap(), expected);
    assert_eq!(repo.revision().unwrap(), revision);
    assert!(repo.pending_native_backup_preferences().unwrap().is_empty());
    repo.integrity_check().unwrap();
}

#[test]
fn corrupted_receipt_lyric_count_overflow_returns_error_without_panicking() {
    let directory = profile();
    let repo = Repository::open(directory.path()).unwrap();
    let preview = repo
        .preview_native_backup(&backup(), ConflictPolicy::ReplaceTracks)
        .unwrap();
    repo.commit_native_backup("corrupt-count", &preview, &|| Ok(()))
        .unwrap();
    let db = Connection::open(repo.path()).unwrap();
    let mut receipt: serde_json::Value = serde_json::from_str(
        &db.query_row("SELECT document FROM native_backup_imports", [], |row| {
            row.get::<_, String>(0)
        })
        .unwrap(),
    )
    .unwrap();
    receipt["counts"]["applied_edited_lyrics"] = usize::MAX.into();
    receipt["counts"]["archived_edited_lyrics"] = 1_usize.into();
    receipt["change"]["details"]["counts"] = receipt["counts"].clone();
    let document = serde_json::to_string(&receipt).unwrap();
    db.execute(
        "UPDATE native_backup_imports SET document=?1,byte_size=?2",
        rusqlite::params![document, document.len() as i64],
    )
    .unwrap();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        repo.native_backup_result("corrupt-count")
    }));
    assert!(result.is_ok(), "Corrupt receipt count must not panic");
    assert!(
        result
            .unwrap()
            .unwrap_err()
            .to_string()
            .contains("lyric count overflow")
    );
}

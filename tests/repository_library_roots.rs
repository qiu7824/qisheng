use lx_gpui_player::{
    domain::{
        library::*,
        lists::{AddLocation, ListAction},
        lyric::{LyricBundle, LyricField, LyricOrigin},
    },
    repository::{DATABASE_NAME, Repository, SCHEMA_VERSION, local_track_from_path},
};
use rusqlite::{Connection, params};
use std::{
    fs,
    path::Path,
    sync::atomic::{AtomicUsize, Ordering},
    time::Duration,
};

fn directory() -> tempfile::TempDir {
    tempfile::tempdir_in(std::env::temp_dir()).unwrap()
}
fn wav(path: &Path) {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"RIFF");
    bytes.extend_from_slice(&52u32.to_le_bytes());
    bytes.extend_from_slice(b"WAVEfmt ");
    bytes.extend_from_slice(&16u32.to_le_bytes());
    bytes.extend_from_slice(&1u16.to_le_bytes());
    bytes.extend_from_slice(&1u16.to_le_bytes());
    bytes.extend_from_slice(&8000u32.to_le_bytes());
    bytes.extend_from_slice(&16000u32.to_le_bytes());
    bytes.extend_from_slice(&2u16.to_le_bytes());
    bytes.extend_from_slice(&16u16.to_le_bytes());
    bytes.extend_from_slice(b"data");
    bytes.extend_from_slice(&16u32.to_le_bytes());
    bytes.extend_from_slice(&[0u8; 16]);
    fs::write(path, bytes).unwrap();
}
fn draft(path: &Path, recursive: bool) -> LibraryRootDraft {
    #[cfg(windows)]
    {
        lx_gpui_player::library::authority::RootAuthority::open(
            path,
            recursive,
            &lx_gpui_player::library::scan::ScanContext::new(Duration::from_secs(30)),
        )
        .unwrap()
        .draft()
    }
    #[cfg(not(windows))]
    {
        use std::os::unix::fs::MetadataExt;
        let metadata = fs::metadata(path).unwrap();
        LibraryRootDraft {
            path: fs::canonicalize(path).unwrap(),
            recursive,
            anchor: FileIdentity {
                kind: "unix_inode".into(),
                volume: metadata.dev().to_string(),
                file_id: metadata.ino().to_string(),
            },
            path_policy: LibraryPathPolicy::Sensitive,
        }
    }
}
fn prepared(path: &Path) -> LibraryPreparedAsset {
    let (track, asset) = local_track_from_path(path).unwrap();
    LibraryPreparedAsset {
        track,
        asset,
        file_identity: None,
    }
}
fn finish(
    repo: &Repository,
    token: &LibraryScanToken,
    complete: bool,
    state: LibraryOperationState,
) {
    let mut view = LibraryOperationView::running(token);
    view.state = state;
    repo.finish_library_scan(token, &view, complete, &|| Ok(()))
        .unwrap();
}
fn import_root(
    repo: &Repository,
    root: &LibraryRoot,
    item: &LibraryPreparedAsset,
    operation: &str,
) {
    let (token, _) = repo.begin_library_scan(&root.root_id, operation).unwrap();
    repo.apply_library_scan_batch(&token, &[], std::slice::from_ref(item), &|| Ok(()))
        .unwrap();
    finish(repo, &token, true, LibraryOperationState::Completed);
}

#[test]
fn directory_grants_are_distinct_from_explicit_files_and_survive_reopen_without_reapproval() {
    let profile = directory();
    let media = directory();
    let nested = media.path().join("selected 子目录");
    fs::create_dir(&nested).unwrap();
    let root_only_path = nested.join("root-only.wav");
    let explicit_path = nested.join("explicit.wav");
    wav(&root_only_path);
    wav(&explicit_path);
    let root_only = prepared(&root_only_path);
    let explicit = prepared(&explicit_path);
    let repo = Repository::open(profile.path()).unwrap();
    let (parent, _) = repo.add_library_root(&draft(media.path(), true)).unwrap();
    let (child, _) = repo.add_library_root(&draft(&nested, false)).unwrap();
    import_root(&repo, &parent, &root_only, "parent");
    import_root(&repo, &child, &root_only, "child");
    repo.upsert_local_track(&explicit.track, &explicit.asset)
        .unwrap();
    let initial = repo
        .local_asset_authority(&root_only.asset.id)
        .unwrap()
        .unwrap();
    assert!(initial.explicit_file_epoch.is_none());
    assert_eq!(initial.roots.len(), 2);
    repo.remove_library_root(&parent.root_id).unwrap();
    assert_eq!(
        repo.local_asset_authority(&root_only.asset.id)
            .unwrap()
            .unwrap()
            .roots
            .len(),
        1
    );
    repo.remove_library_root(&child.root_id).unwrap();
    assert!(
        repo.local_asset_authority(&root_only.asset.id)
            .unwrap()
            .is_none()
    );
    assert!(repo.local_asset(&root_only.asset.id).unwrap().is_some());
    assert!(
        repo.local_asset_authority(&explicit.asset.id)
            .unwrap()
            .unwrap()
            .explicit_file_epoch
            .is_some()
    );
    drop(repo);
    let repo = Repository::open(profile.path()).unwrap();
    assert!(
        repo.local_asset_authority(&root_only.asset.id)
            .unwrap()
            .is_none()
    );
    assert!(
        repo.local_asset_authority(&explicit.asset.id)
            .unwrap()
            .is_some()
    );
    assert_eq!(repo.library_summary().unwrap().authorized_assets, 1);
}

#[test]
fn root_tombstone_epoch_fences_old_batches_after_removal_and_readding_same_path() {
    let profile = directory();
    let media = directory();
    let audio = media.path().join("stable.wav");
    wav(&audio);
    let item = prepared(&audio);
    let repo = Repository::open(profile.path()).unwrap();
    let root_draft = draft(media.path(), true);
    let (root, _) = repo.add_library_root(&root_draft).unwrap();
    let (old, _) = repo.begin_library_scan(&root.root_id, "old").unwrap();
    repo.remove_library_root(&root.root_id).unwrap();
    let (replacement, _) = repo.add_library_root(&root_draft).unwrap();
    assert_eq!(replacement.root_id, root.root_id);
    assert!(replacement.grant_epoch > old.grant_epoch);
    let revision = repo.revision().unwrap();
    assert!(
        repo.apply_library_scan_batch(&old, &[], std::slice::from_ref(&item), &|| Ok(()))
            .is_err()
    );
    assert_eq!(repo.revision().unwrap(), revision);
    assert!(repo.track(&item.track.id).unwrap().is_none());
    let old_view = repo.library_operation("old").unwrap().unwrap();
    assert_eq!(old_view.state, LibraryOperationState::Cancelled);
    import_root(&repo, &replacement, &item, "new");
    assert_eq!(
        repo.track(&item.track.id).unwrap().unwrap().id,
        item.track.id
    );
}

#[test]
fn mid_batch_sql_failure_and_cooperative_cancel_roll_back_all_tables_revision_and_outbox() {
    let profile = directory();
    let media = directory();
    for name in ["first.wav", "second.wav"] {
        wav(&media.path().join(name));
    }
    let items = [
        prepared(&media.path().join("first.wav")),
        prepared(&media.path().join("second.wav")),
    ];
    let repo = Repository::open(profile.path()).unwrap();
    let (root, _) = repo.add_library_root(&draft(media.path(), true)).unwrap();
    let (token, _) = repo.begin_library_scan(&root.root_id, "rollback").unwrap();
    let revision = repo.revision().unwrap();
    let inspection = Connection::open(repo.path()).unwrap();
    inspection.execute_batch("CREATE TRIGGER fail_second_library_track BEFORE INSERT ON tracks WHEN json_extract(NEW.document,'$.title')='second' BEGIN SELECT RAISE(ABORT,'second fixture rejected'); END;").unwrap();
    assert!(
        repo.apply_library_scan_batch(&token, &[], &items, &|| Ok(()))
            .is_err()
    );
    assert_eq!(repo.revision().unwrap(), revision);
    for table in [
        "tracks",
        "local_assets",
        "local_asset_locations",
        "local_file_grants",
        "library_membership",
        "library_scan_seen",
    ] {
        assert_eq!(
            inspection
                .query_row(&format!("SELECT count(*) FROM {table}"), [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            0,
            "{table}"
        );
    }
    assert!(repo.changes_since(revision, 20).unwrap().is_empty());
    inspection
        .execute_batch("DROP TRIGGER fail_second_library_track")
        .unwrap();
    let calls = AtomicUsize::new(0);
    let check = || {
        if calls.fetch_add(1, Ordering::AcqRel) >= 5 {
            return Err(LibraryError::new(
                LibraryErrorKind::Cancelled,
                "Cancelled during prepared batch",
            )
            .into());
        }
        Ok(())
    };
    assert!(
        repo.apply_library_scan_batch(&token, &[], &items, &check)
            .is_err()
    );
    assert_eq!(repo.revision().unwrap(), revision);
    assert!(repo.local_assets().unwrap().is_empty());
    assert!(repo.changes_since(revision, 20).unwrap().is_empty());
    repo.integrity_check().unwrap();
}

#[test]
fn only_complete_discovery_marks_absent_members_and_keeps_lists_and_edited_lyrics() {
    let profile = directory();
    let media = directory();
    let path = media.path().join("retained.wav");
    wav(&path);
    let item = prepared(&path);
    let repo = Repository::open(profile.path()).unwrap();
    let (root, _) = repo.add_library_root(&draft(media.path(), true)).unwrap();
    import_root(&repo, &root, &item, "first");
    repo.apply_list_action(
        &ListAction::MusicAdd {
            list_id: "love".into(),
            tracks: vec![item.track.clone()],
            location: AddLocation::Bottom,
        },
        None,
    )
    .unwrap();
    let edited = LyricBundle {
        lyric: LyricField::Text("[00:00.000]保留编辑歌词".into()),
        tlyric: LyricField::Null,
        rlyric: LyricField::Missing,
        lxlyric: LyricField::Missing,
    };
    repo.replace_lyrics(&item.track.id, LyricOrigin::Edited, &edited)
        .unwrap();
    fs::remove_file(&path).unwrap();
    for (operation, state) in [
        ("incomplete", LibraryOperationState::Partial),
        ("cancelled", LibraryOperationState::Cancelled),
        ("failed", LibraryOperationState::Failed),
    ] {
        let (token, _) = repo.begin_library_scan(&root.root_id, operation).unwrap();
        finish(&repo, &token, false, state);
        assert!(repo.local_asset(&item.asset.id).unwrap().unwrap().available);
    }
    let (token, _) = repo.begin_library_scan(&root.root_id, "complete").unwrap();
    finish(&repo, &token, true, LibraryOperationState::Completed);
    assert!(!repo.local_asset(&item.asset.id).unwrap().unwrap().available);
    assert_eq!(
        repo.list_track("love", &item.track.id).unwrap().unwrap().id,
        item.track.id
    );
    assert_eq!(
        repo.lyrics(&item.track.id, LyricOrigin::Edited).unwrap(),
        Some(edited)
    );
    assert!(
        repo.local_asset_authority(&item.asset.id)
            .unwrap()
            .is_some()
    );
    wav(&path);
    let restored = prepared(&path);
    assert_eq!(restored.asset.id, item.asset.id);
    import_root(&repo, &root, &restored, "restore");
    assert!(repo.local_asset(&item.asset.id).unwrap().unwrap().available);
    assert_eq!(
        repo.library_operation("complete")
            .unwrap()
            .unwrap()
            .missing_files,
        1
    );
}

#[test]
fn seen_but_unreadable_existing_file_is_not_missing_and_retains_previous_tags() {
    let profile = directory();
    let media = directory();
    let path = media.path().join("existing.wav");
    wav(&path);
    let mut item = prepared(&path);
    item.track.title = "原标签标题".into();
    let repo = Repository::open(profile.path()).unwrap();
    let (root, _) = repo.add_library_root(&draft(media.path(), true)).unwrap();
    import_root(&repo, &root, &item, "good");
    fs::write(&path, b"damaged metadata fixture").unwrap();
    let (token, _) = repo.begin_library_scan(&root.root_id, "bad-tags").unwrap();
    repo.apply_library_scan_batch(
        &token,
        &[LibrarySeenFile {
            path: fs::canonicalize(&path).unwrap(),
            asset_id: None,
        }],
        &[],
        &|| Ok(()),
    )
    .unwrap();
    finish(&repo, &token, true, LibraryOperationState::Partial);
    assert!(repo.local_asset(&item.asset.id).unwrap().unwrap().available);
    assert_eq!(
        repo.track(&item.track.id).unwrap().unwrap().title,
        "原标签标题"
    );
    assert_eq!(
        repo.library_operation("bad-tags")
            .unwrap()
            .unwrap()
            .missing_files,
        0
    );
}

#[test]
fn migration_from_schema_seven_grants_existing_assets_once_preserves_identity_and_backup() {
    let profile = directory();
    let media = directory();
    let path = media.path().join("旧批准 音乐.wav");
    wav(&path);
    let item = prepared(&path);
    let repo = Repository::open(profile.path()).unwrap();
    repo.upsert_local_track(&item.track, &item.asset).unwrap();
    let revision = repo.revision().unwrap();
    let database = repo.path().to_path_buf();
    drop(repo);
    let connection = Connection::open(&database).unwrap();
    connection.execute_batch("DROP TABLE download_allocation_intents; DROP TABLE tag_edit_artifacts; DROP TABLE tag_edit_operations; DROP TABLE tag_write_approvals; DROP TABLE native_backup_archives; DROP TABLE native_backup_imports; DROP TABLE physical_snapshot_operations; DROP TABLE physical_snapshot_state; DROP TABLE sync_mode_preferences; DROP TABLE sync_capture_override; DROP TABLE sync_baselines; DROP TABLE sync_current; DROP TABLE sync_journal; DROP TABLE sync_list_metadata; DROP TABLE sync_snapshots; DROP TABLE sync_peers; DROP TABLE sync_profile_state; DROP TABLE download_owned_artifacts; DROP TABLE backup_list_metadata; DROP TABLE compat_settings; DROP TABLE backup_imports; DROP TABLE download_tasks; DROP TABLE download_settings; DROP TABLE download_destination_grants; DROP INDEX tracks_lx_identity; DROP TABLE library_relink_results; DROP TABLE library_relink_candidates; DROP TABLE library_identity_reservations; DROP TABLE library_dirty_state; DROP TABLE library_watch_preferences; DROP TABLE library_root_locations; DROP TABLE library_scan_seen; DROP TABLE library_membership; DROP TABLE library_scan_runs; DROP TABLE local_asset_locations; DROP TABLE local_file_grants; DROP TABLE library_roots; PRAGMA user_version=7;").unwrap();
    drop(connection);
    fs::remove_file(&path).unwrap();
    let repo = Repository::open(profile.path()).unwrap();
    assert_eq!(repo.revision().unwrap(), revision);
    assert_eq!(
        repo.local_asset(&item.asset.id).unwrap(),
        Some(item.asset.clone())
    );
    assert!(
        repo.local_asset_authority(&item.asset.id)
            .unwrap()
            .unwrap()
            .explicit_file_epoch
            .is_some()
    );
    assert_eq!(
        repo.local_asset_file_identity(&item.asset.id).unwrap(),
        None
    );
    assert_eq!(
        Connection::open(repo.path())
            .unwrap()
            .pragma_query_value(None, "user_version", |row| row.get::<_, u32>(0))
            .unwrap(),
        SCHEMA_VERSION
    );
    let backups = fs::read_dir(profile.path())
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "bak"))
        .collect::<Vec<_>>();
    assert_eq!(backups.len(), 1);
    let backup = Connection::open(&backups[0]).unwrap();
    assert_eq!(
        backup
            .pragma_query_value(None, "user_version", |row| row.get::<_, u32>(0))
            .unwrap(),
        7
    );
    assert_eq!(
        backup
            .query_row("SELECT identity_key FROM local_assets", [], |row| row
                .get::<_, String>(0))
            .unwrap(),
        item.asset.id
    );
    drop(repo);
    let repo = Repository::open(profile.path()).unwrap();
    assert_eq!(repo.revision().unwrap(), revision);
    assert!(
        repo.local_asset_authority(&item.asset.id)
            .unwrap()
            .is_some()
    );
}

#[test]
fn failed_migration_with_path_collision_rolls_back_ddl_and_keeps_schema_seven() {
    let profile = directory();
    let media = directory();
    let path = media.path().join("same.wav");
    wav(&path);
    let item = prepared(&path);
    let repo = Repository::open(profile.path()).unwrap();
    repo.upsert_local_track(&item.track, &item.asset).unwrap();
    let database = repo.path().to_path_buf();
    drop(repo);
    let connection = Connection::open(&database).unwrap();
    connection.execute_batch("DROP TABLE download_allocation_intents; DROP TABLE tag_edit_artifacts; DROP TABLE tag_edit_operations; DROP TABLE tag_write_approvals; DROP TABLE native_backup_archives; DROP TABLE native_backup_imports; DROP TABLE physical_snapshot_operations; DROP TABLE physical_snapshot_state; DROP TABLE sync_mode_preferences; DROP TABLE sync_capture_override; DROP TABLE sync_baselines; DROP TABLE sync_current; DROP TABLE sync_journal; DROP TABLE sync_list_metadata; DROP TABLE sync_snapshots; DROP TABLE sync_peers; DROP TABLE sync_profile_state; DROP TABLE download_owned_artifacts; DROP TABLE backup_list_metadata; DROP TABLE compat_settings; DROP TABLE backup_imports; DROP TABLE download_tasks; DROP TABLE download_settings; DROP TABLE download_destination_grants; DROP INDEX tracks_lx_identity; DROP TABLE library_relink_results; DROP TABLE library_relink_candidates; DROP TABLE library_identity_reservations; DROP TABLE library_dirty_state; DROP TABLE library_watch_preferences; DROP TABLE library_root_locations; DROP TABLE library_scan_seen; DROP TABLE library_membership; DROP TABLE library_scan_runs; DROP TABLE local_asset_locations; DROP TABLE local_file_grants; DROP TABLE library_roots; PRAGMA user_version=7;").unwrap();
    let mut other_track = item.track.clone();
    let mut other_asset = item.asset.clone();
    other_track.id = "retained-other-track".into();
    other_asset.id = "retained-other-asset".into();
    other_asset.track_id = other_track.id.clone();
    other_track.local_asset_id = Some(other_asset.id.clone());
    connection
        .execute(
            "INSERT INTO tracks(id,document) VALUES(?1,?2)",
            params![other_track.id, serde_json::to_string(&other_track).unwrap()],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO local_assets(identity_key,track_id,document) VALUES(?1,?2,?3)",
            params![
                other_asset.id,
                other_asset.track_id,
                serde_json::to_string(&other_asset).unwrap()
            ],
        )
        .unwrap();
    drop(connection);
    assert!(Repository::open(profile.path()).is_err());
    let connection = Connection::open(&database).unwrap();
    assert_eq!(
        connection
            .pragma_query_value(None, "user_version", |row| row.get::<_, u32>(0))
            .unwrap(),
        7
    );
    assert_eq!(
        connection
            .query_row("SELECT count(*) FROM local_assets", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        2
    );
    assert_eq!(
        connection
            .query_row(
                "SELECT count(*) FROM sqlite_schema WHERE name='local_file_grants'",
                [],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
        0
    );
    assert_eq!(
        connection
            .query_row("PRAGMA integrity_check", [], |row| row.get::<_, String>(0))
            .unwrap(),
        "ok"
    );
}

#[test]
fn actual_restart_interrupts_inflight_scan_without_missing_unvisited_members() {
    let profile = directory();
    let media = directory();
    let audio = media.path().join("keep.wav");
    wav(&audio);
    let item = prepared(&audio);
    let repo = Repository::open(profile.path()).unwrap();
    let (root, _) = repo.add_library_root(&draft(media.path(), true)).unwrap();
    import_root(&repo, &root, &item, "complete-before");
    let (token, _) = repo
        .begin_library_scan(&root.root_id, "interrupted")
        .unwrap();
    repo.apply_library_scan_batch(
        &token,
        &[LibrarySeenFile {
            path: audio.clone(),
            asset_id: Some(item.asset.id.clone()),
        }],
        &[],
        &|| Ok(()),
    )
    .unwrap();
    let revision = repo.revision().unwrap();
    drop(repo);
    let repo = Repository::open(profile.path()).unwrap();
    assert!(repo.revision().unwrap() > revision);
    assert_eq!(
        repo.library_operation("interrupted")
            .unwrap()
            .unwrap()
            .state,
        LibraryOperationState::Interrupted
    );
    assert!(repo.local_asset(&item.asset.id).unwrap().unwrap().available);
    assert!(
        repo.apply_library_scan_batch(&token, &[], &[item], &|| Ok(()))
            .is_err()
    );
    assert_eq!(repo.library_summary().unwrap().running_operations, 0);
    repo.begin_library_scan(&root.root_id, "new-scan").unwrap();
}

#[test]
fn keyset_pages_are_byte_bounded_revision_checked_and_keep_unauthorized_indexed_records() {
    let profile = directory();
    let media = directory();
    let repo = Repository::open(profile.path()).unwrap();
    let (root, _) = repo.add_library_root(&draft(media.path(), true)).unwrap();
    let mut expected = Vec::new();
    let mut items = Vec::new();
    for n in 0..7 {
        let path = media.path().join(format!("{n}.wav"));
        wav(&path);
        let item = prepared(&path);
        expected.push(item.asset.id.clone());
        items.push(item);
    }
    let (token, _) = repo
        .begin_library_scan(&root.root_id, "import-all")
        .unwrap();
    repo.apply_library_scan_batch(&token, &[], &items, &|| Ok(()))
        .unwrap();
    finish(&repo, &token, true, LibraryOperationState::Completed);
    expected.sort();
    let revision = repo.revision().unwrap();
    let mut actual = Vec::new();
    let mut after = None;
    loop {
        let page = repo
            .library_assets_page(Some(&root.root_id), after.as_deref(), 2, Some(revision))
            .unwrap();
        assert_eq!(page.total, 7);
        assert!(serde_json::to_vec(&page).unwrap().len() <= MAX_LIBRARY_PAGE_BYTES);
        actual.extend(page.items.iter().map(|item| item.asset.id.clone()));
        after = page.next_asset_id;
        if after.is_none() {
            break;
        }
    }
    assert_eq!(actual, expected);
    repo.remove_library_root(&root.root_id).unwrap();
    assert!(
        repo.library_assets_page(None, None, 2, Some(revision))
            .is_err()
    );
    let page = repo.library_assets_page(None, None, 100, None).unwrap();
    assert_eq!(page.total, 7);
    assert!(
        page.items
            .iter()
            .all(|item| !item.authorized && !item.explicit_file_grant && item.root_ids.is_empty())
    );
    assert!(repo.library_assets_page(None, None, 1001, None).is_err());
    assert!(profile.path().join(DATABASE_NAME).is_file());
}

#[test]
fn asset_page_stops_at_bytes_before_count_and_next_key_delivers_every_long_missing_path() {
    let profile = directory();
    let media = directory();
    let repo = Repository::open(profile.path()).unwrap();
    let mut parent = media.path().to_path_buf();
    for n in 0..60 {
        parent.push(format!("{n:02}-{}", "x".repeat(197)));
    }
    let mut items = Vec::new();
    for n in 0..700 {
        let (track, asset) =
            local_track_from_path(&parent.join(format!("missing-{n}.wav"))).unwrap();
        assert!(!asset.available);
        items.push((track, asset));
    }
    for batch in items.chunks(200) {
        repo.upsert_local_tracks(batch).unwrap();
    }
    let first = repo.library_assets_page(None, None, 1000, None).unwrap();
    assert!(!first.items.is_empty() && first.items.len() < 700);
    assert!(serde_json::to_vec(&first).unwrap().len() <= MAX_LIBRARY_PAGE_BYTES);
    let second = repo
        .library_assets_page(
            None,
            first.next_asset_id.as_deref(),
            1000,
            Some(first.revision),
        )
        .unwrap();
    assert_eq!(first.items.len() + second.items.len(), 700);
    assert!(second.next_asset_id.is_none());
    assert!(serde_json::to_vec(&second).unwrap().len() <= MAX_LIBRARY_PAGE_BYTES);
}

#[cfg(windows)]
#[test]
fn explicit_file_batch_persists_actual_opened_file_identity_with_one_durable_grant() {
    use lx_gpui_player::library::{authority::RootAuthority, scan::ScanContext};
    let profile = directory();
    let media = directory();
    let path = media.path().join("selected.wav");
    wav(&path);
    let mut item = prepared(&path);
    let authority = RootAuthority::open(
        media.path(),
        false,
        &ScanContext::new(Duration::from_secs(30)),
    )
    .unwrap();
    let file = authority.open_file(&path).unwrap();
    item.file_identity = Some(file.identity().clone());
    let repo = Repository::open(profile.path()).unwrap();
    let revision = repo.revision().unwrap();
    let change = repo
        .upsert_explicit_library_tracks(std::slice::from_ref(&item), &|| Ok(()))
        .unwrap();
    assert_eq!(change.revision, revision + 1);
    assert_eq!(
        repo.local_asset_file_identity(&item.asset.id).unwrap(),
        item.file_identity
    );
    assert!(
        repo.local_asset_authority(&item.asset.id)
            .unwrap()
            .unwrap()
            .explicit_file_epoch
            .is_some()
    );
    assert!(
        repo.local_asset_authority(&item.asset.id)
            .unwrap()
            .unwrap()
            .roots
            .is_empty()
    );
    drop(repo);
    let repo = Repository::open(profile.path()).unwrap();
    assert_eq!(
        repo.local_asset_file_identity(&item.asset.id).unwrap(),
        item.file_identity
    );
    assert_eq!(repo.library_summary().unwrap().authorized_assets, 1);
}

#[test]
fn scan_tag_refresh_preserves_original_lx_identity_unknown_fields_and_list_items() {
    let profile = directory();
    let media = directory();
    let audio = media.path().join("metadata.wav");
    wav(&audio);
    let mut original = prepared(&audio);
    original.track.lx_id = "legacy-local-identity".into();
    original
        .track
        .metadata
        .insert("custom-field".into(), serde_json::json!({"keep":true}));
    let repo = Repository::open(profile.path()).unwrap();
    repo.upsert_local_track(&original.track, &original.asset)
        .unwrap();
    repo.apply_list_action(
        &ListAction::MusicAdd {
            list_id: "love".into(),
            tracks: vec![original.track.clone()],
            location: AddLocation::Bottom,
        },
        None,
    )
    .unwrap();
    let (root, _) = repo.add_library_root(&draft(media.path(), true)).unwrap();
    let mut refresh = prepared(&audio);
    refresh.track.title = "新标签标题".into();
    refresh.track.album = "新专辑".into();
    import_root(&repo, &root, &refresh, "refresh");
    let track = repo.track(&original.track.id).unwrap().unwrap();
    assert_eq!(track.lx_id, "legacy-local-identity");
    assert_eq!(track.title, "新标签标题");
    assert_eq!(
        track.metadata["custom-field"],
        serde_json::json!({"keep":true})
    );
    let listed = repo
        .list_track("love", "legacy-local-identity")
        .unwrap()
        .unwrap();
    assert_eq!(listed.lx_id, "legacy-local-identity");
    assert_eq!(listed.title, "新标签标题");
    assert_eq!(listed.album, "新专辑");
    assert_eq!(
        listed.metadata["custom-field"],
        serde_json::json!({"keep":true})
    );
}

#[test]
fn nonrecursive_root_scope_rejects_nested_or_unselected_files_without_partial_grants() {
    let profile = directory();
    let media = directory();
    let outside = directory();
    fs::create_dir(media.path().join("nested")).unwrap();
    let selected = media.path().join("selected.wav");
    let nested = media.path().join("nested/deep.wav");
    let other = outside.path().join("private.wav");
    for path in [&selected, &nested, &other] {
        wav(path);
    }
    let repo = Repository::open(profile.path()).unwrap();
    let (root, _) = repo.add_library_root(&draft(media.path(), false)).unwrap();
    let (token, _) = repo.begin_library_scan(&root.root_id, "scope").unwrap();
    let good = prepared(&selected);
    let before = repo.revision().unwrap();
    for forbidden in [&nested, &other] {
        let bad = prepared(forbidden);
        assert!(
            repo.apply_library_scan_batch(&token, &[], &[good.clone(), bad], &|| Ok(()))
                .is_err()
        );
        assert_eq!(repo.revision().unwrap(), before);
        assert!(repo.local_assets().unwrap().is_empty());
        assert_eq!(repo.library_summary().unwrap().authorized_assets, 0);
    }
    repo.apply_library_scan_batch(&token, &[], &[good], &|| Ok(()))
        .unwrap();
    finish(&repo, &token, true, LibraryOperationState::Completed);
    assert_eq!(repo.library_summary().unwrap().authorized_assets, 1);
}

#[test]
fn migration_rejects_a_legacy_asset_document_that_disagrees_with_its_sql_identity() {
    let profile = directory();
    let media = directory();
    let audio = media.path().join("identity.wav");
    wav(&audio);
    let item = prepared(&audio);
    let repo = Repository::open(profile.path()).unwrap();
    repo.upsert_local_track(&item.track, &item.asset).unwrap();
    let database = repo.path().to_path_buf();
    drop(repo);
    let connection = Connection::open(&database).unwrap();
    connection.execute_batch("DROP TABLE download_allocation_intents; DROP TABLE tag_edit_artifacts; DROP TABLE tag_edit_operations; DROP TABLE tag_write_approvals; DROP TABLE native_backup_archives; DROP TABLE native_backup_imports; DROP TABLE physical_snapshot_operations; DROP TABLE physical_snapshot_state; DROP TABLE sync_mode_preferences; DROP TABLE sync_capture_override; DROP TABLE sync_baselines; DROP TABLE sync_current; DROP TABLE sync_journal; DROP TABLE sync_list_metadata; DROP TABLE sync_snapshots; DROP TABLE sync_peers; DROP TABLE sync_profile_state; DROP TABLE download_owned_artifacts; DROP TABLE backup_list_metadata; DROP TABLE compat_settings; DROP TABLE backup_imports; DROP TABLE download_tasks; DROP TABLE download_settings; DROP TABLE download_destination_grants; DROP INDEX tracks_lx_identity; DROP TABLE library_relink_results; DROP TABLE library_relink_candidates; DROP TABLE library_identity_reservations; DROP TABLE library_dirty_state; DROP TABLE library_watch_preferences; DROP TABLE library_root_locations; DROP TABLE library_scan_seen; DROP TABLE library_membership; DROP TABLE library_scan_runs; DROP TABLE local_asset_locations; DROP TABLE local_file_grants; DROP TABLE library_roots; PRAGMA user_version=7;").unwrap();
    let mut altered = item.asset.clone();
    altered.id = "different-document-identity".into();
    connection
        .execute(
            "UPDATE local_assets SET document=?2 WHERE identity_key=?1",
            params![item.asset.id, serde_json::to_string(&altered).unwrap()],
        )
        .unwrap();
    drop(connection);
    assert!(Repository::open(profile.path()).is_err());
    let connection = Connection::open(&database).unwrap();
    assert_eq!(
        connection
            .pragma_query_value(None, "user_version", |row| row.get::<_, u32>(0))
            .unwrap(),
        7
    );
    assert_eq!(
        connection
            .query_row(
                "SELECT count(*) FROM sqlite_schema WHERE name='local_file_grants'",
                [],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
        0
    );
    let preserved: String = connection
        .query_row(
            "SELECT document FROM local_assets WHERE identity_key=?1",
            [&item.asset.id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        serde_json::from_str::<lx_gpui_player::domain::music::LocalAsset>(&preserved).unwrap(),
        altered
    );
}

#[cfg(windows)]
fn physically_prepared(path: &Path) -> LibraryPreparedAsset {
    use lx_gpui_player::library::{authority::RootAuthority, scan::ScanContext};
    let mut item = prepared(path);
    let root = RootAuthority::open(
        path.parent().unwrap(),
        false,
        &ScanContext::new(Duration::from_secs(30)),
    )
    .unwrap();
    item.file_identity = Some(root.open_file(path).unwrap().identity().clone());
    item
}

#[cfg(windows)]
#[test]
fn unicode_lowercase_collision_cannot_replace_a_different_physical_root() {
    let profile = directory();
    let media = directory();
    let first = media.path().join("\u{0130}");
    let second = media.path().join("i\u{0307}");
    fs::create_dir(&first).unwrap();
    fs::create_dir(&second).unwrap();
    let first = draft(&first, true);
    let second = draft(&second, true);
    assert_ne!(first.anchor, second.anchor);
    assert_eq!(
        library_path_key(&first.path, first.path_policy).unwrap(),
        library_path_key(&second.path, second.path_policy).unwrap()
    );
    let repo = Repository::open(profile.path()).unwrap();
    let (stored, _) = repo.add_library_root(&first).unwrap();
    let revision = repo.revision().unwrap();
    let error = repo.add_library_root(&second).unwrap_err();
    assert_eq!(
        error.downcast_ref::<LibraryError>().unwrap().kind,
        LibraryErrorKind::Conflict
    );
    assert_eq!(repo.revision().unwrap(), revision);
    assert_eq!(repo.library_root(&stored.root_id).unwrap(), Some(stored));
    assert!(repo.changes_since(revision, 20).unwrap().is_empty());
}

#[cfg(windows)]
#[test]
fn unicode_asset_collision_is_rejected_with_both_strong_and_legacy_weak_location_proofs() {
    let profile = directory();
    let media = directory();
    let first_dir = media.path().join("\u{0130}");
    let second_dir = media.path().join("i\u{0307}");
    fs::create_dir(&first_dir).unwrap();
    fs::create_dir(&second_dir).unwrap();
    let first_path = first_dir.join("owned.wav");
    let second_path = second_dir.join("owned.wav");
    wav(&first_path);
    wav(&second_path);
    let first = physically_prepared(&first_path);
    let second = physically_prepared(&second_path);
    assert_eq!(first.asset.id, second.asset.id);
    assert_eq!(first.track.id, second.track.id);
    assert_ne!(first.file_identity, second.file_identity);
    let repo = Repository::open(profile.path()).unwrap();
    let (root, _) = repo.add_library_root(&draft(media.path(), true)).unwrap();
    import_root(&repo, &root, &first, "first-physical-file");
    let (token, _) = repo
        .begin_library_scan(&root.root_id, "different-physical-file")
        .unwrap();
    let revision = repo.revision().unwrap();
    let error = repo
        .apply_library_scan_batch(&token, &[], std::slice::from_ref(&second), &|| Ok(()))
        .unwrap_err();
    assert_eq!(
        error.downcast_ref::<LibraryError>().unwrap().kind,
        LibraryErrorKind::Conflict
    );
    assert_eq!(repo.revision().unwrap(), revision);
    assert_eq!(
        repo.local_asset(&first.asset.id).unwrap(),
        Some(first.asset.clone())
    );
    assert_eq!(
        repo.local_asset_file_identity(&first.asset.id).unwrap(),
        first.file_identity
    );
    assert!(
        repo.local_asset_authority(&first.asset.id)
            .unwrap()
            .unwrap()
            .explicit_file_epoch
            .is_none()
    );
    let error = repo
        .upsert_explicit_library_tracks(std::slice::from_ref(&second), &|| Ok(()))
        .unwrap_err();
    assert_eq!(
        error.downcast_ref::<LibraryError>().unwrap().kind,
        LibraryErrorKind::Conflict
    );
    assert_eq!(repo.revision().unwrap(), revision);
    assert!(
        repo.local_asset_authority(&first.asset.id)
            .unwrap()
            .unwrap()
            .explicit_file_epoch
            .is_none()
    );
    drop(repo);

    let weak_profile = directory();
    let weak = Repository::open(weak_profile.path()).unwrap();
    weak.upsert_local_track(&first.track, &first.asset).unwrap();
    assert!(
        weak.local_asset_file_identity(&first.asset.id)
            .unwrap()
            .is_none()
    );
    let before = weak.revision().unwrap();
    let error = weak
        .upsert_explicit_library_tracks(std::slice::from_ref(&second), &|| Ok(()))
        .unwrap_err();
    assert_eq!(
        error.downcast_ref::<LibraryError>().unwrap().kind,
        LibraryErrorKind::Conflict
    );
    assert_eq!(weak.revision().unwrap(), before);
    assert_eq!(
        weak.local_asset(&first.asset.id).unwrap(),
        Some(first.asset)
    );
}

#[cfg(windows)]
#[test]
fn same_physical_case_aliases_and_same_literal_path_replacement_keep_stable_asset_ids() {
    use lx_gpui_player::domain::music::normalize_local_path;
    let profile = directory();
    let media = directory();
    let named_root = media.path().join("OriginalRoot");
    fs::create_dir(&named_root).unwrap();
    let path = named_root.join("Case.wav");
    wav(&path);
    let first = physically_prepared(&path);
    let repo = Repository::open(profile.path()).unwrap();
    let first_draft = draft(&named_root, false);
    let (root, _) = repo.add_library_root(&first_draft).unwrap();
    let mut alias_draft = draft(&media.path().join("ORIGINALROOT"), false);
    alias_draft.path = normalize_local_path(&media.path().join("ORIGINALROOT"));
    assert_eq!(first_draft.anchor, alias_draft.anchor);
    let (alias_root, _) = repo.add_library_root(&alias_draft).unwrap();
    assert_eq!(root.root_id, alias_root.root_id);
    assert_eq!(root.grant_epoch, alias_root.grant_epoch);
    repo.upsert_explicit_library_tracks(std::slice::from_ref(&first), &|| Ok(()))
        .unwrap();
    let alias_path = named_root.join("CASE.WAV");
    let mut alias = physically_prepared(&alias_path);
    alias.asset.path = normalize_local_path(&alias_path);
    assert_eq!(first.file_identity, alias.file_identity);
    repo.upsert_explicit_library_tracks(std::slice::from_ref(&alias), &|| Ok(()))
        .unwrap();
    assert_eq!(
        repo.local_asset_file_identity(&first.asset.id).unwrap(),
        first.file_identity
    );
    assert_eq!(
        repo.local_asset(&first.asset.id).unwrap().unwrap().path,
        alias.asset.path
    );

    // A replacement at the exact registered literal pathname is an explicitly
    // selected new representation, rather than a Unicode path-identity alias.
    fs::remove_file(&alias_path).unwrap();
    wav(&alias_path);
    let mut replacement = physically_prepared(&alias_path);
    replacement.asset.path = alias.asset.path.clone();
    assert_eq!(replacement.asset.id, first.asset.id);
    assert_ne!(replacement.file_identity, first.file_identity);
    repo.upsert_explicit_library_tracks(std::slice::from_ref(&replacement), &|| Ok(()))
        .unwrap();
    assert_eq!(
        repo.local_asset_file_identity(&first.asset.id).unwrap(),
        replacement.file_identity
    );
    assert_eq!(
        repo.local_asset(&first.asset.id).unwrap().unwrap().path,
        replacement.asset.path
    );
}

#[test]
fn effective_summary_requires_verified_epoch_matches_and_counts_shared_or_explicit_assets_once() {
    let profile = directory();
    let media = directory();
    let explicit_media = directory();
    let child_path = media.path().join("nested verified root");
    fs::create_dir(&child_path).unwrap();
    let root_only_path = media.path().join("parent-only.wav");
    let shared_path = child_path.join("shared.wav");
    let explicit_path = explicit_media.path().join("explicit.wav");
    for path in [&root_only_path, &shared_path, &explicit_path] {
        wav(path);
    }
    let parent_only = prepared(&root_only_path);
    let shared = prepared(&shared_path);
    let explicit = prepared(&explicit_path);
    let repo = Repository::open(profile.path()).unwrap();
    let (parent, _) = repo.add_library_root(&draft(media.path(), true)).unwrap();
    let (child, _) = repo.add_library_root(&draft(&child_path, false)).unwrap();
    let (token, _) = repo
        .begin_library_scan(&parent.root_id, "parent-members")
        .unwrap();
    repo.apply_library_scan_batch(&token, &[], &[parent_only, shared.clone()], &|| Ok(()))
        .unwrap();
    finish(&repo, &token, true, LibraryOperationState::Completed);
    import_root(&repo, &child, &shared, "child-members");
    repo.upsert_local_track(&explicit.track, &explicit.asset)
        .unwrap();
    repo.begin_library_scan(&parent.root_id, "pending-health")
        .unwrap();
    let baseline = repo.library_summary().unwrap();
    assert_eq!(baseline.authorized_assets, 3);
    let parent_verified = (parent.root_id.clone(), parent.grant_epoch);
    let child_verified = (child.root_id.clone(), child.grant_epoch);
    let none = repo.library_summary_with_verified_roots(&[]).unwrap();
    assert_eq!(none.authorized_assets, 1);
    assert_eq!(none.roots, baseline.roots);
    assert_eq!(none.running_operations, baseline.running_operations);
    assert_eq!(none.latest_operation_id, baseline.latest_operation_id);
    assert_eq!(none.revision, baseline.revision);
    assert_eq!(
        repo.library_summary_with_verified_roots(std::slice::from_ref(&parent_verified))
            .unwrap()
            .authorized_assets,
        3
    );
    assert_eq!(
        repo.library_summary_with_verified_roots(std::slice::from_ref(&child_verified))
            .unwrap()
            .authorized_assets,
        2
    );
    assert_eq!(
        repo.library_summary_with_verified_roots(&[
            parent_verified.clone(),
            child_verified.clone(),
            parent_verified.clone()
        ])
        .unwrap()
        .authorized_assets,
        3
    );
    assert_eq!(
        repo.library_summary_with_verified_roots(&[
            (parent.root_id.clone(), parent.grant_epoch + 1),
            child_verified.clone()
        ])
        .unwrap()
        .authorized_assets,
        2
    );
    assert_eq!(
        repo.library_summary_with_verified_roots(&[("unknown' OR 1=1 --".into(), 1)])
            .unwrap()
            .authorized_assets,
        1
    );
    assert_eq!(repo.revision().unwrap(), baseline.revision);
    assert!(
        repo.changes_since(baseline.revision, 20)
            .unwrap()
            .is_empty()
    );

    repo.remove_library_root(&parent.root_id).unwrap();
    let remaining = repo
        .library_summary_with_verified_roots(&[parent_verified, child_verified])
        .unwrap();
    assert_eq!(remaining.roots, 1);
    assert_eq!(remaining.authorized_assets, 2);
    assert_eq!(remaining.running_operations, 0);
    assert_eq!(
        remaining.latest_operation_id.as_deref(),
        Some("pending-health")
    );
    repo.remove_library_root(&child.root_id).unwrap();
    assert_eq!(
        repo.library_summary_with_verified_roots(&[])
            .unwrap()
            .authorized_assets,
        1
    );
}

#[test]
fn verified_root_summary_rejects_oversized_parameters_and_invalid_epochs_without_writes() {
    let profile = directory();
    let repo = Repository::open(profile.path()).unwrap();
    assert!(
        repo.library_summary_with_verified_roots(&vec![("root".into(), 1); MAX_LIBRARY_ROOTS + 1])
            .is_err()
    );
    assert!(
        repo.library_summary_with_verified_roots(&[("root".into(), 0)])
            .is_err()
    );
    assert!(
        repo.library_summary_with_verified_roots(&[("root".into(), i64::MAX as u64 + 1)])
            .is_err()
    );
    assert!(
        repo.library_summary_with_verified_roots(&[("root\0invalid".into(), 1)])
            .is_err()
    );
    let escaped = "\u{0001}".repeat(1024);
    let pairs = vec![(escaped, 1); MAX_LIBRARY_ROOTS];
    assert!(repo.library_summary_with_verified_roots(&pairs).is_err());
    assert_eq!(repo.revision().unwrap(), 0);
    assert!(repo.changes_since(0, 20).unwrap().is_empty());
}

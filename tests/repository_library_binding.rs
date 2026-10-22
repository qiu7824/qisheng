use lx_gpui_player::{
    domain::{
        library::*,
        library_watch::*,
        lists::{AddLocation, ListAction},
        lyric::{LyricBundle, LyricField, LyricOrigin},
        music::{LxMusicDtoVersion, TrackRecord},
    },
    repository::{Repository, local_track_from_path},
};
use sha2::{Digest, Sha256};
use std::{
    fs,
    path::Path,
    sync::atomic::{AtomicUsize, Ordering},
    time::Duration,
};
fn folder() -> tempfile::TempDir {
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
    bytes.extend_from_slice(&[0; 16]);
    fs::write(path, bytes).unwrap();
}
fn draft(path: &Path) -> LibraryRootDraft {
    let a = lx_gpui_player::library::authority::RootAuthority::open(
        path,
        true,
        &lx_gpui_player::library::scan::ScanContext::new(Duration::from_secs(30)),
    )
    .unwrap();
    let d = a.draft();
    let b = a.retirement_barrier();
    drop(a);
    assert!(b.wait(Duration::from_secs(3)));
    d
}
fn item(path: &Path) -> LibraryPreparedAsset {
    let a = lx_gpui_player::library::authority::RootAuthority::open(
        path.parent().unwrap(),
        false,
        &lx_gpui_player::library::scan::ScanContext::new(Duration::from_secs(30)),
    )
    .unwrap();
    let opened = a.open_file(path).unwrap();
    let id = opened.identity().clone();
    let (track, mut asset) = local_track_from_path(path).unwrap();
    asset.fingerprint = Some(format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(&id).unwrap())
    ));
    let barrier = a.retirement_barrier();
    drop(opened);
    drop(a);
    assert!(barrier.wait(Duration::from_secs(3)));
    LibraryPreparedAsset {
        track,
        asset,
        file_identity: Some(id),
    }
}
fn stable(mut new: LibraryPreparedAsset, old: &LibraryPreparedAsset) -> LibraryPreparedAsset {
    new.track.id = old.track.id.clone();
    new.track.lx_id = old.track.lx_id.clone();
    new.track.local_asset_id = Some(old.asset.id.clone());
    new.asset.id = old.asset.id.clone();
    new.asset.track_id = old.track.id.clone();
    new
}
fn hash(path: &Path) -> String {
    format!("{:x}", Sha256::digest(fs::read(path).unwrap()))
}
fn request(
    op: &str,
    old: &LibraryPreparedAsset,
    new: LibraryPreparedAsset,
    revision: u64,
) -> LibraryRelinkCommit {
    LibraryRelinkCommit {
        operation_id: op.into(),
        expected_binding_revision: revision,
        prepared: stable(new, old),
        authorization: LibraryRelinkAuthorization::Manual,
        evidence: RelinkEvidence::SameFileAndContent,
        content_sha256: None,
        allow_content_change: false,
    }
}

#[test]
fn manual_actual_file_relocation_preserves_ids_lists_edits_and_current_lx_path() {
    let profile = folder();
    let media = folder();
    let a = media.path().join("old.wav");
    let b = media.path().join("new.wav");
    wav(&a);
    let original = item(&a);
    let repo = Repository::open(profile.path()).unwrap();
    repo.upsert_explicit_library_tracks(std::slice::from_ref(&original), &|| Ok(()))
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
    let edited = LyricBundle {
        lyric: LyricField::Text("[00:00.000]保留编辑".into()),
        tlyric: LyricField::Null,
        rlyric: LyricField::Missing,
        lxlyric: LyricField::Missing,
    };
    repo.replace_lyrics(&original.track.id, LyricOrigin::Edited, &edited)
        .unwrap();
    let binding = repo
        .library_asset_binding(&original.asset.id)
        .unwrap()
        .unwrap();
    let sha = hash(&a);
    repo.put_library_content_evidence(
        &original.asset.id,
        binding.binding_revision,
        original.file_identity.as_ref().unwrap(),
        original.asset.size_bytes,
        original.asset.modified_ms,
        &sha,
        &|| Ok(()),
    )
    .unwrap();
    let old_epoch = repo
        .local_asset_authority(&original.asset.id)
        .unwrap()
        .unwrap()
        .explicit_file_epoch
        .unwrap();
    fs::rename(&a, &b).unwrap();
    let mut req = request("manual", &original, item(&b), binding.binding_revision);
    req.content_sha256 = Some(hash(&b));
    let commit = repo.commit_library_relink(&req, &|| Ok(())).unwrap();
    assert!(!commit.result.media_changed);
    assert!(commit.result.explicit_grant_renewed);
    assert_eq!(commit.result.asset_id, original.asset.id);
    assert_eq!(commit.result.track_id, original.track.id);
    assert!(
        repo.local_asset_authority(&original.asset.id)
            .unwrap()
            .unwrap()
            .explicit_file_epoch
            .unwrap()
            > old_epoch
    );
    let listed = repo
        .list_track("love", &original.track.lx_id)
        .unwrap()
        .unwrap();
    assert_eq!(listed.id, original.track.id);
    assert_eq!(
        listed.platform.song_id,
        serde_json::json!(lx_gpui_player::domain::music::normalize_local_path(&b))
    );
    assert_eq!(
        listed.to_lx_dto(LxMusicDtoVersion::Modern)["meta"]["filePath"],
        serde_json::json!(lx_gpui_player::domain::music::normalize_local_path(&b))
    );
    assert_eq!(
        repo.lyrics(&original.track.id, LyricOrigin::Edited)
            .unwrap(),
        Some(edited)
    );
    let revision = repo.revision().unwrap();
    assert_eq!(
        repo.commit_library_relink(&req, &|| Ok(())).unwrap().result,
        commit.result
    );
    assert_eq!(repo.revision().unwrap(), revision);
    drop(repo);
    let repo = Repository::open(profile.path()).unwrap();
    assert_eq!(
        repo.library_relink_result("manual").unwrap(),
        Some(commit.result)
    );
    assert_eq!(
        repo.local_asset(&original.asset.id).unwrap().unwrap().path,
        lx_gpui_player::domain::music::normalize_local_path(&b)
    );
}

#[test]
fn automatic_same_root_move_does_not_make_original_explicit_grant_follow_new_path() {
    let profile = folder();
    let media = folder();
    let a = media.path().join("a.wav");
    let b = media.path().join("b.wav");
    wav(&a);
    let original = item(&a);
    let repo = Repository::open(profile.path()).unwrap();
    repo.upsert_explicit_library_tracks(std::slice::from_ref(&original), &|| Ok(()))
        .unwrap();
    let (root, _) = repo.add_library_root(&draft(media.path())).unwrap();
    let (scan, _) = repo.begin_library_scan(&root.root_id, "baseline").unwrap();
    repo.apply_library_scan_batch(&scan, &[], std::slice::from_ref(&original), &|| Ok(()))
        .unwrap();
    let mut out = LibraryOperationView::running(&scan);
    out.state = LibraryOperationState::Completed;
    repo.finish_library_scan(&scan, &out, true, &|| Ok(()))
        .unwrap();
    let options = LibraryWatchOptions {
        auto_relink: true,
        ..LibraryWatchOptions::default()
    };
    let token = repo
        .prepare_library_watch(&root.root_id, true, &options, None)
        .unwrap();
    repo.commit_library_watch(&token, &|| Ok(())).unwrap();
    let binding = repo
        .library_asset_binding(&original.asset.id)
        .unwrap()
        .unwrap();
    fs::rename(&a, &b).unwrap();
    let mut req = request("auto", &original, item(&b), binding.binding_revision);
    req.authorization = LibraryRelinkAuthorization::Root {
        root_id: root.root_id.clone(),
        grant_epoch: root.grant_epoch,
        watch_generation: token.watch_generation,
    };
    req.evidence = RelinkEvidence::SameOpenObject;
    let result = repo.commit_library_relink(&req, &|| Ok(())).unwrap();
    assert!(!result.result.media_changed);
    assert!(!result.result.explicit_grant_renewed);
    let authority = repo
        .local_asset_authority(&original.asset.id)
        .unwrap()
        .unwrap();
    assert!(authority.explicit_file_epoch.is_none());
    assert_eq!(authority.roots.len(), 1);
    repo.remove_library_root(&root.root_id).unwrap();
    assert!(
        repo.local_asset_authority(&original.asset.id)
            .unwrap()
            .is_none()
    );
    assert_eq!(
        repo.library_summary_with_verified_roots(&[])
            .unwrap()
            .authorized_assets,
        0
    );
}

#[test]
fn old_path_reuse_allocates_new_ids_and_stale_lx_json_cannot_regrant_or_rebind_original() {
    let profile = folder();
    let media = folder();
    let a = media.path().join("old.wav");
    let b = media.path().join("moved.wav");
    wav(&a);
    let original = item(&a);
    let repo = Repository::open(profile.path()).unwrap();
    repo.upsert_explicit_library_tracks(std::slice::from_ref(&original), &|| Ok(()))
        .unwrap();
    let (binding,) = (repo
        .library_asset_binding(&original.asset.id)
        .unwrap()
        .unwrap(),);
    fs::rename(&a, &b).unwrap();
    let mut req = request("move", &original, item(&b), binding.binding_revision);
    req.evidence = RelinkEvidence::SameOpenObject;
    repo.commit_library_relink(&req, &|| Ok(())).unwrap();
    wav(&a);
    let raw = item(&a);
    assert_eq!(raw.asset.id, original.asset.id);
    let allocated = repo.prepare_library_identity(&raw, &|| Ok(())).unwrap();
    assert_ne!(allocated.asset.id, original.asset.id);
    assert_ne!(allocated.track.id, original.track.id);
    assert_ne!(allocated.track.lx_id, original.track.lx_id);
    let repeated = repo.prepare_library_identity(&raw, &|| Ok(())).unwrap();
    assert_eq!(repeated.asset.id, allocated.asset.id);
    repo.upsert_explicit_library_tracks(std::slice::from_ref(&allocated), &|| Ok(()))
        .unwrap();
    let stale = TrackRecord::from_lx_dto(
        original.track.to_lx_dto(LxMusicDtoVersion::Modern),
        LxMusicDtoVersion::Modern,
    )
    .unwrap();
    let before = repo.revision().unwrap();
    assert!(
        repo.apply_list_action(
            &ListAction::MusicAdd {
                list_id: "love".into(),
                tracks: vec![stale],
                location: AddLocation::Bottom
            },
            None
        )
        .is_err()
    );
    assert_eq!(repo.revision().unwrap(), before);
    assert_eq!(
        repo.local_asset(&original.asset.id).unwrap().unwrap().path,
        lx_gpui_player::domain::music::normalize_local_path(&b)
    );
    let legacy = repo
        .import_legacy_local_files(&[a], &"a".repeat(64))
        .unwrap();
    assert!(legacy.imported.is_empty());
    assert_eq!(
        repo.local_asset(&original.asset.id).unwrap().unwrap().path,
        lx_gpui_player::domain::music::normalize_local_path(&b)
    );
}

#[test]
fn scoped_lx_import_recovers_moved_binding_without_trusting_uploaded_native_ids_or_granting_unknown_paths()
 {
    let profile = folder();
    let media = folder();
    let a = media.path().join("old.wav");
    let b = media.path().join("new.wav");
    wav(&a);
    let original = item(&a);
    let repo = Repository::open(profile.path()).unwrap();
    repo.upsert_explicit_library_tracks(std::slice::from_ref(&original), &|| Ok(()))
        .unwrap();
    let binding = repo
        .library_asset_binding(&original.asset.id)
        .unwrap()
        .unwrap();
    fs::rename(&a, &b).unwrap();
    let mut req = request("move", &original, item(&b), binding.binding_revision);
    req.evidence = RelinkEvidence::SameOpenObject;
    repo.commit_library_relink(&req, &|| Ok(())).unwrap();
    let mut stale = TrackRecord::from_lx_dto(
        original.track.to_lx_dto(LxMusicDtoVersion::Modern),
        LxMusicDtoVersion::Modern,
    )
    .unwrap();
    stale.id = "forged-native-id".into();
    stale.local_asset_id = Some("forged-asset".into());
    repo.apply_list_action(
        &ListAction::MusicAdd {
            list_id: "love".into(),
            tracks: vec![stale],
            location: AddLocation::Bottom,
        },
        None,
    )
    .unwrap();
    let restored = repo
        .list_track("love", &original.track.lx_id)
        .unwrap()
        .unwrap();
    assert_eq!(restored.id, original.track.id);
    assert_eq!(restored.local_asset_id, Some(original.asset.id.clone()));
    assert_eq!(
        restored.metadata["filePath"],
        serde_json::json!(lx_gpui_player::domain::music::normalize_local_path(&b))
    );
    let unknown=TrackRecord::from_lx_dto(serde_json::json!({"id":"unknown-local","source":"local","name":"未知文件","singer":"","interval":null,"meta":{"songId":"unknown","filePath":media.path().join("unknown.wav"),"albumName":""}}),LxMusicDtoVersion::Modern).unwrap();
    repo.apply_list_action(
        &ListAction::MusicAdd {
            list_id: "love".into(),
            tracks: vec![unknown],
            location: AddLocation::Bottom,
        },
        None,
    )
    .unwrap();
    let rows = repo.list_tracks("love").unwrap();
    let unbound = rows
        .iter()
        .find(|track| track.lx_id == "unknown-local")
        .unwrap();
    assert!(unbound.local_asset_id.is_none());
    assert!(unbound.id.starts_with("unbound-local:"));
    assert_eq!(repo.local_assets().unwrap().len(), 1);
}

#[test]
fn cancel_target_collision_and_old_binding_cas_preserve_all_location_and_list_rows() {
    let profile = folder();
    let media = folder();
    let a = media.path().join("a.wav");
    let b = media.path().join("b.wav");
    let c = media.path().join("c.wav");
    for path in [&a, &b, &c] {
        wav(path);
    }
    let original = item(&a);
    let occupied = item(&b);
    let repo = Repository::open(profile.path()).unwrap();
    repo.upsert_explicit_library_tracks(&[original.clone(), occupied], &|| Ok(()))
        .unwrap();
    let binding = repo
        .library_asset_binding(&original.asset.id)
        .unwrap()
        .unwrap();
    let before = repo.revision().unwrap();
    let collision = request("occupied", &original, item(&b), binding.binding_revision);
    assert!(repo.commit_library_relink(&collision, &|| Ok(())).is_err());
    assert_eq!(repo.revision().unwrap(), before);
    let cancelled = request("cancelled", &original, item(&c), binding.binding_revision);
    let calls = AtomicUsize::new(0);
    let check = || {
        if calls.fetch_add(1, Ordering::AcqRel) >= 3 {
            Err(LibraryError::new(LibraryErrorKind::Cancelled, "cancelled before commit").into())
        } else {
            Ok(())
        }
    };
    assert!(repo.commit_library_relink(&cancelled, &check).is_err());
    assert_eq!(repo.revision().unwrap(), before);
    assert_eq!(
        repo.library_asset_binding(&original.asset.id).unwrap(),
        Some(binding.clone())
    );
    assert!(repo.library_relink_result("cancelled").unwrap().is_none());
    assert!(repo.changes_since(before, 20).unwrap().is_empty());
    let mut stale = request("stale", &original, item(&c), binding.binding_revision + 1);
    stale.allow_content_change = true;
    assert!(repo.commit_library_relink(&stale, &|| Ok(())).is_err());
    assert_eq!(repo.revision().unwrap(), before);
    repo.integrity_check().unwrap();
}

#[test]
fn actual_root_rename_preserves_root_and_asset_identity_and_reused_old_root_path_gets_new_id() {
    let profile = folder();
    let media = folder();
    let old = media.path().join("old-root");
    let new = media.path().join("new-root");
    fs::create_dir(&old).unwrap();
    let file = old.join("song.wav");
    wav(&file);
    let original = item(&file);
    let repo = Repository::open(profile.path()).unwrap();
    let (root, _) = repo.add_library_root(&draft(&old)).unwrap();
    let (scan, _) = repo.begin_library_scan(&root.root_id, "baseline").unwrap();
    repo.apply_library_scan_batch(&scan, &[], std::slice::from_ref(&original), &|| Ok(()))
        .unwrap();
    let mut out = LibraryOperationView::running(&scan);
    out.state = LibraryOperationState::Completed;
    repo.finish_library_scan(&scan, &out, true, &|| Ok(()))
        .unwrap();
    let binding = repo.library_root_binding(&root.root_id).unwrap().unwrap();
    fs::rename(&old, &new).unwrap();
    let (moved, _) = repo
        .rebind_library_root(
            &root.root_id,
            &draft(&new),
            root.grant_epoch,
            binding.location_revision,
            true,
            &|| Ok(()),
        )
        .unwrap();
    assert_eq!(moved.root_id, root.root_id);
    assert_eq!(moved.grant_epoch, root.grant_epoch);
    assert_eq!(
        repo.local_asset(&original.asset.id).unwrap().unwrap().path,
        lx_gpui_player::domain::music::normalize_local_path(&new.join("song.wav"))
    );
    assert_eq!(
        repo.track(&original.track.id).unwrap().unwrap().lx_id,
        original.track.lx_id
    );
    fs::create_dir(&old).unwrap();
    let (reused, _) = repo.add_library_root(&draft(&old)).unwrap();
    assert_ne!(reused.root_id, root.root_id);
    assert_eq!(repo.library_roots_page(0, 100).unwrap().total, 2);
}

#[test]
fn manual_authority_preview_uses_historical_exact_epoch_without_writes_and_matches_commit() {
    let profile = folder();
    let media = folder();
    let a = media.path().join("before.wav");
    let b = media.path().join("after.wav");
    wav(&a);
    let original = item(&a);
    let repo = Repository::open(profile.path()).unwrap();
    repo.upsert_explicit_library_tracks(std::slice::from_ref(&original), &|| Ok(()))
        .unwrap();
    let (root, _) = repo.add_library_root(&draft(media.path())).unwrap();
    let (scan, _) = repo
        .begin_library_scan(&root.root_id, "baseline-preview")
        .unwrap();
    repo.apply_library_scan_batch(&scan, &[], std::slice::from_ref(&original), &|| Ok(()))
        .unwrap();
    let mut out = LibraryOperationView::running(&scan);
    out.state = LibraryOperationState::Completed;
    repo.finish_library_scan(&scan, &out, true, &|| Ok(()))
        .unwrap();
    let options = LibraryWatchOptions {
        auto_relink: true,
        ..LibraryWatchOptions::default()
    };
    let watch = repo
        .prepare_library_watch(&root.root_id, true, &options, None)
        .unwrap();
    repo.commit_library_watch(&watch, &|| Ok(())).unwrap();
    let old_binding = repo
        .library_asset_binding(&original.asset.id)
        .unwrap()
        .unwrap();
    let no_change = repo
        .preview_manual_relink_authority(&original, old_binding.binding_revision)
        .unwrap();
    assert_eq!(no_change.explicit_file_epoch, Some(1));
    fs::rename(&a, &b).unwrap();
    let mut automatic = request(
        "auto-preview",
        &original,
        item(&b),
        old_binding.binding_revision,
    );
    automatic.authorization = LibraryRelinkAuthorization::Root {
        root_id: root.root_id.clone(),
        grant_epoch: root.grant_epoch,
        watch_generation: watch.watch_generation,
    };
    automatic.evidence = RelinkEvidence::SameOpenObject;
    repo.commit_library_relink(&automatic, &|| Ok(())).unwrap();
    assert!(
        repo.local_asset_authority(&original.asset.id)
            .unwrap()
            .unwrap()
            .explicit_file_epoch
            .is_none()
    );
    let current = repo
        .library_asset_binding(&original.asset.id)
        .unwrap()
        .unwrap();
    let selected = stable(item(&b), &original);
    let before = repo.revision().unwrap();
    let preview = repo
        .preview_manual_relink_authority(&selected, current.binding_revision)
        .unwrap();
    assert_eq!(preview.explicit_file_epoch, Some(2));
    assert_eq!(preview.roots.len(), 1);
    assert_eq!(repo.revision().unwrap(), before);
    assert!(repo.changes_since(before, 10).unwrap().is_empty());
    let mut manual = request(
        "manual-preview",
        &original,
        selected.clone(),
        current.binding_revision,
    );
    manual.evidence = RelinkEvidence::SameOpenObject;
    repo.commit_library_relink(&manual, &|| Ok(())).unwrap();
    let actual = repo
        .local_asset_authority(&original.asset.id)
        .unwrap()
        .unwrap();
    assert_eq!(actual, preview);
    assert!(
        repo.preview_manual_relink_authority(&selected, current.binding_revision)
            .is_err()
    );
    let next = repo
        .library_asset_binding(&original.asset.id)
        .unwrap()
        .unwrap();
    assert_eq!(
        repo.preview_manual_relink_authority(&selected, next.binding_revision)
            .unwrap()
            .explicit_file_epoch,
        Some(2)
    );
}

#[test]
fn schema_eight_migration_binds_existing_file_grant_without_reading_a_now_missing_file() {
    use rusqlite::{Connection, params};
    let profile = folder();
    let media = folder();
    let path = media.path().join("legacy-eight.wav");
    wav(&path);
    let original = item(&path);
    let repo = Repository::open(profile.path()).unwrap();
    repo.upsert_explicit_library_tracks(std::slice::from_ref(&original), &|| Ok(()))
        .unwrap();
    let revision = repo.revision().unwrap();
    let database = repo.path().to_path_buf();
    drop(repo);
    let db = Connection::open(&database).unwrap();
    db.execute_batch("PRAGMA foreign_keys=OFF; DROP TABLE download_allocation_intents; DROP TABLE tag_edit_artifacts; DROP TABLE tag_edit_operations; DROP TABLE tag_write_approvals; DROP TABLE native_backup_archives; DROP TABLE native_backup_imports; DROP TABLE physical_snapshot_operations; DROP TABLE physical_snapshot_state; DROP TABLE sync_mode_preferences; DROP TABLE sync_capture_override; DROP TABLE sync_baselines; DROP TABLE sync_current; DROP TABLE sync_journal; DROP TABLE sync_list_metadata; DROP TABLE sync_snapshots; DROP TABLE sync_peers; DROP TABLE sync_profile_state; DROP TABLE download_owned_artifacts; DROP TABLE backup_list_metadata; DROP TABLE compat_settings; DROP TABLE backup_imports; DROP TABLE download_tasks; DROP TABLE download_settings; DROP TABLE download_destination_grants; DROP INDEX tracks_lx_identity; DROP TABLE library_root_locations; DROP TABLE library_watch_preferences; DROP TABLE library_dirty_state; DROP TABLE library_identity_reservations; DROP TABLE library_relink_candidates; DROP TABLE library_relink_results; DROP INDEX local_asset_locations_physical; ALTER TABLE local_asset_locations DROP COLUMN physical_identity_key; ALTER TABLE local_asset_locations DROP COLUMN binding_revision; ALTER TABLE local_file_grants DROP COLUMN grant_path_key; ALTER TABLE local_file_grants DROP COLUMN document; ALTER TABLE library_scan_runs DROP COLUMN scope_document; PRAGMA user_version=8;").unwrap();
    let legacy_location =
        serde_json::json!({"path":original.asset.path,"file_identity":original.file_identity});
    db.execute(
        "UPDATE local_asset_locations SET document=?2 WHERE asset_id=?1",
        params![original.asset.id, legacy_location.to_string()],
    )
    .unwrap();
    drop(db);
    fs::remove_file(&path).unwrap();
    let repo = Repository::open(profile.path()).unwrap();
    assert_eq!(repo.revision().unwrap(), revision);
    let binding = repo
        .library_asset_binding(&original.asset.id)
        .unwrap()
        .unwrap();
    assert_eq!(binding.binding_revision, 1);
    assert_eq!(binding.file_identity, original.file_identity);
    assert_eq!(binding.path, original.asset.path);
    assert!(binding.content_sha256.is_none());
    assert_eq!(
        repo.local_asset_authority(&original.asset.id)
            .unwrap()
            .unwrap()
            .explicit_file_epoch,
        Some(1)
    );
    let backups = fs::read_dir(profile.path())
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "bak"))
        .collect::<Vec<_>>();
    assert_eq!(backups.len(), 1);
    assert_eq!(
        Connection::open(&backups[0])
            .unwrap()
            .pragma_query_value(None, "user_version", |row| row.get::<_, u32>(0))
            .unwrap(),
        8
    );
    repo.integrity_check().unwrap();
}

#[test]
fn completed_import_releases_identity_reservation_while_failed_transaction_retains_retry_identity()
{
    use rusqlite::Connection;
    let profile = folder();
    let media = folder();
    let a = media.path().join("reserved.wav");
    wav(&a);
    let raw = item(&a);
    let repo = Repository::open(profile.path()).unwrap();
    let reserved = repo.prepare_library_identity(&raw, &|| Ok(())).unwrap();
    let inspect = Connection::open(repo.path()).unwrap();
    let count = || {
        inspect
            .query_row(
                "SELECT count(*) FROM library_identity_reservations",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap()
    };
    assert_eq!(count(), 1);
    assert!(repo.local_assets().unwrap().is_empty());
    inspect.execute_batch("CREATE TRIGGER reject_reserved_asset BEFORE INSERT ON local_assets BEGIN SELECT RAISE(ABORT,'reserved fixture failure'); END;").unwrap();
    assert!(
        repo.upsert_explicit_library_tracks(std::slice::from_ref(&reserved), &|| Ok(()))
            .is_err()
    );
    assert_eq!(count(), 1);
    assert_eq!(
        repo.prepare_library_identity(&raw, &|| Ok(()))
            .unwrap()
            .asset
            .id,
        reserved.asset.id
    );
    inspect
        .execute_batch("DROP TRIGGER reject_reserved_asset")
        .unwrap();
    repo.upsert_explicit_library_tracks(std::slice::from_ref(&reserved), &|| Ok(()))
        .unwrap();
    assert_eq!(count(), 0);
    assert_eq!(
        repo.prepare_library_identity(&raw, &|| Ok(()))
            .unwrap()
            .asset
            .id,
        reserved.asset.id
    );
    assert_eq!(count(), 0);
}

#[test]
fn identity_batch_resolves_internal_collisions_atomically_and_preserves_retry_ids() {
    let profile = folder();
    let media = folder();
    let first_path = media.path().join("batch-first.wav");
    let second_path = media.path().join("batch-second.wav");
    wav(&first_path);
    wav(&second_path);
    let first = item(&first_path);
    let second = stable(item(&second_path), &first);
    let inputs = vec![first, second];
    let repo = Repository::open(profile.path()).unwrap();
    let inspect = rusqlite::Connection::open(repo.path()).unwrap();
    let count = || {
        inspect
            .query_row(
                "SELECT count(*) FROM library_identity_reservations",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap()
    };
    let revision = repo.revision().unwrap();
    let calls = AtomicUsize::new(0);
    assert!(
        repo.prepare_library_identities(&inputs, &|| {
            if calls.fetch_add(1, Ordering::SeqCst) >= 4 {
                anyhow::bail!("Cancelled before reservation commit");
            }
            Ok(())
        })
        .is_err()
    );
    assert_eq!(count(), 0);
    assert_eq!(repo.revision().unwrap(), revision);
    let allocated = repo
        .prepare_library_identities(&inputs, &|| Ok(()))
        .unwrap();
    assert_eq!(count(), 2);
    assert_eq!(allocated[0].asset.id, inputs[0].asset.id);
    assert_ne!(allocated[1].asset.id, allocated[0].asset.id);
    assert_ne!(allocated[1].track.id, allocated[0].track.id);
    assert_ne!(allocated[1].track.lx_id, allocated[0].track.lx_id);
    let repeated = repo
        .prepare_library_identities(&inputs, &|| Ok(()))
        .unwrap();
    assert_eq!(
        serde_json::to_value(&repeated).unwrap(),
        serde_json::to_value(&allocated).unwrap()
    );
    assert_eq!(repo.revision().unwrap(), revision);
    repo.upsert_explicit_library_tracks(&allocated, &|| Ok(()))
        .unwrap();
    assert_eq!(count(), 0);
    assert_eq!(
        serde_json::to_value(
            repo.prepare_library_identities(&inputs, &|| Ok(()))
                .unwrap()
        )
        .unwrap(),
        serde_json::to_value(&allocated).unwrap()
    );
    assert_eq!(count(), 0);
    repo.integrity_check().unwrap();
}

#[test]
fn oversized_identity_batch_creates_no_reservations() {
    let profile = folder();
    let media = folder();
    let path = media.path().join("batch-budget.wav");
    wav(&path);
    let raw = item(&path);
    let repo = Repository::open(profile.path()).unwrap();
    assert!(
        repo.prepare_library_identities(
            &vec![raw.clone(); MAX_LIBRARY_BATCH_ITEMS + 1],
            &|| Ok(())
        )
        .is_err()
    );
    let mut large = raw;
    large.track.metadata.insert(
        "large".into(),
        serde_json::json!("x".repeat(MAX_LIBRARY_BATCH_BYTES)),
    );
    assert!(
        repo.prepare_library_identities(&[large], &|| Ok(()))
            .is_err()
    );
    let inspect = rusqlite::Connection::open(repo.path()).unwrap();
    assert_eq!(
        inspect
            .query_row(
                "SELECT count(*) FROM library_identity_reservations",
                [],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
        0
    );
}

#[test]
fn candidate_page_and_total_share_binding_watch_epoch_filters_and_keyset_does_not_skip_valid_rows()
{
    use rusqlite::{Connection, params};
    let profile = folder();
    let media = folder();
    let a = media.path().join("candidate.wav");
    wav(&a);
    let original = item(&a);
    let repo = Repository::open(profile.path()).unwrap();
    let (root, _) = repo.add_library_root(&draft(media.path())).unwrap();
    let (scan, _) = repo
        .begin_library_scan(&root.root_id, "candidate-baseline")
        .unwrap();
    repo.apply_library_scan_batch(&scan, &[], std::slice::from_ref(&original), &|| Ok(()))
        .unwrap();
    let mut out = LibraryOperationView::running(&scan);
    out.state = LibraryOperationState::Completed;
    repo.finish_library_scan(&scan, &out, true, &|| Ok(()))
        .unwrap();
    let watch = repo
        .prepare_library_watch(&root.root_id, true, &LibraryWatchOptions::default(), None)
        .unwrap();
    repo.commit_library_watch(&watch, &|| Ok(())).unwrap();
    let binding = repo
        .library_asset_binding(&original.asset.id)
        .unwrap()
        .unwrap();
    let expiry = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
        + 60000;
    let make = |id: &str| LibraryRelinkCandidate {
        candidate_id: id.into(),
        asset_id: original.asset.id.clone(),
        root_id: root.root_id.clone(),
        grant_epoch: root.grant_epoch,
        watch_generation: watch.watch_generation,
        old_path: original.asset.path.clone(),
        new_path: media.path().join(format!("{id}.wav")),
        new_identity: original.file_identity.clone().unwrap(),
        content_sha256: None,
        binding_revision: binding.binding_revision,
        evidence: RelinkEvidence::ManualSelection,
        requires_selection: true,
        expires_at_ms: expiry,
    };
    for id in ["candidate-a", "candidate-b", "candidate-c"] {
        repo.put_library_relink_candidate(&make(id), &|| Ok(()))
            .unwrap();
    }
    let inspect = Connection::open(repo.path()).unwrap();
    let stale = make("candidate-a");
    let mut stale_value = serde_json::to_value(stale).unwrap();
    stale_value["binding_revision"] = serde_json::json!(binding.binding_revision + 1);
    inspect
        .execute(
            "UPDATE library_relink_candidates SET document=?2 WHERE candidate_id=?1",
            params!["candidate-a", stale_value.to_string()],
        )
        .unwrap();
    let first = repo
        .library_relink_candidates_page(None, None, 1, expiry - 60000)
        .unwrap();
    assert_eq!(first.total, 2);
    assert_eq!(first.items[0].candidate_id, "candidate-b");
    assert_eq!(first.next_candidate_id.as_deref(), Some("candidate-b"));
    assert!(
        repo.library_relink_candidate("candidate-a", expiry - 60000)
            .unwrap()
            .is_none()
    );
    let second = repo
        .library_relink_candidates_page(None, first.next_candidate_id.as_deref(), 1, expiry - 60000)
        .unwrap();
    assert_eq!(second.total, 2);
    assert_eq!(second.items[0].candidate_id, "candidate-c");
    assert!(second.next_candidate_id.is_none());
    let next = repo
        .prepare_library_watch(
            &root.root_id,
            true,
            &LibraryWatchOptions::default(),
            Some(watch.watch_generation),
        )
        .unwrap();
    repo.commit_library_watch(&next, &|| Ok(())).unwrap();
    let empty = repo
        .library_relink_candidates_page(None, None, 100, expiry - 60000)
        .unwrap();
    assert_eq!(empty.total, 0);
    assert!(empty.items.is_empty());
    assert_eq!(
        inspect
            .query_row(
                "SELECT count(*) FROM library_relink_candidates",
                [],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
        0
    );
}

#[test]
fn distinct_per_list_lx_ids_and_archived_wire_survive_binding_relink_and_reimport() {
    use rusqlite::{Connection, params};
    let profile = folder();
    let media = folder();
    let a = media.path().join("copy-old.wav");
    let b = media.path().join("copy-new.wav");
    let c = media.path().join("other.wav");
    wav(&a);
    wav(&c);
    let original = item(&a);
    let other = item(&c);
    let repo = Repository::open(profile.path()).unwrap();
    repo.upsert_explicit_library_tracks(&[original.clone(), other.clone()], &|| Ok(()))
        .unwrap();
    let copy = |id: &str| {
        let mut value = original.track.to_lx_dto(LxMusicDtoVersion::Modern);
        value["id"] = id.into();
        value["opaque"] = serde_json::json!({"preserved":true});
        TrackRecord::from_lx_dto(value, LxMusicDtoVersion::Modern).unwrap()
    };
    let first = copy("list-copy-one");
    let second = copy("list-copy-two");
    repo.apply_list_action(
        &ListAction::MusicAdd {
            list_id: "love".into(),
            tracks: vec![first.clone()],
            location: AddLocation::Bottom,
        },
        None,
    )
    .unwrap();
    repo.apply_list_action(
        &ListAction::MusicAdd {
            list_id: "default".into(),
            tracks: vec![second.clone()],
            location: AddLocation::Bottom,
        },
        None,
    )
    .unwrap();
    for (list, id) in [("love", "list-copy-one"), ("default", "list-copy-two")] {
        let saved = repo.list_track(list, id).unwrap().unwrap();
        assert_eq!(saved.id, original.track.id);
        assert_eq!(saved.lx_id, id);
        assert_eq!(saved.wire.as_ref().unwrap().original["id"], id);
    }
    assert_eq!(
        repo.track(&original.track.id).unwrap().unwrap().lx_id,
        original.track.lx_id
    );
    let binding = repo
        .library_asset_binding(&original.asset.id)
        .unwrap()
        .unwrap();
    fs::rename(&a, &b).unwrap();
    let mut req = request("copy-move", &original, item(&b), binding.binding_revision);
    req.evidence = RelinkEvidence::SameOpenObject;
    repo.commit_library_relink(&req, &|| Ok(())).unwrap();
    let moved = repo.list_track("love", "list-copy-one").unwrap().unwrap();
    let current = moved.to_lx_dto(LxMusicDtoVersion::Modern);
    assert_eq!(current["id"], "list-copy-one");
    assert_eq!(current["opaque"], serde_json::json!({"preserved":true}));
    assert_eq!(
        current["meta"]["filePath"],
        serde_json::json!(lx_gpui_player::domain::music::normalize_local_path(&b))
    );
    repo.apply_list_action(
        &ListAction::MusicAdd {
            list_id: "temp".into(),
            tracks: vec![TrackRecord::from_lx_dto(current, LxMusicDtoVersion::Modern).unwrap()],
            location: AddLocation::Bottom,
        },
        None,
    )
    .unwrap();
    assert_eq!(
        repo.list_track("temp", "list-copy-one")
            .unwrap()
            .unwrap()
            .id,
        original.track.id
    );
    assert_eq!(
        repo.track(&original.track.id).unwrap().unwrap().lx_id,
        original.track.lx_id
    );
    wav(&a);
    let raw = item(&a);
    let new = repo.prepare_library_identity(&raw, &|| Ok(())).unwrap();
    repo.upsert_explicit_library_tracks(std::slice::from_ref(&new), &|| Ok(()))
        .unwrap();
    let before = repo.revision().unwrap();
    assert!(
        repo.apply_list_action(
            &ListAction::MusicAdd {
                list_id: "temp".into(),
                tracks: vec![first.clone()],
                location: AddLocation::Bottom
            },
            None
        )
        .is_err()
    );
    assert_eq!(repo.revision().unwrap(), before);
    let mut ambiguous = other.track.clone();
    ambiguous.lx_id = "list-copy-one".into();
    let inspect = Connection::open(repo.path()).unwrap();
    inspect.execute("INSERT INTO music_list_items(list_id,music_id,track_id,position,document) VALUES('default',?1,?2,99,?3)",params![ambiguous.lx_id,ambiguous.id,serde_json::to_string(&ambiguous).unwrap()]).unwrap();
    let mut incoming = moved;
    incoming.metadata.insert(
        "filePath".into(),
        serde_json::json!(lx_gpui_player::domain::music::normalize_local_path(&b)),
    );
    assert!(
        repo.apply_list_action(
            &ListAction::MusicAdd {
                list_id: "temp".into(),
                tracks: vec![incoming],
                location: AddLocation::Bottom
            },
            None
        )
        .is_err()
    );
    assert_eq!(repo.revision().unwrap(), before);
    assert_eq!(
        repo.track(&original.track.id).unwrap().unwrap().lx_id,
        original.track.lx_id
    );
}

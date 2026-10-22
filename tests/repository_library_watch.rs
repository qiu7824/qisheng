use lx_gpui_player::{
    domain::{library::*, library_watch::*},
    repository::{Repository, local_track_from_path},
};
use std::{
    fs,
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};
fn folder() -> tempfile::TempDir {
    tempfile::tempdir_in(std::env::temp_dir()).unwrap()
}
fn draft(path: &Path) -> LibraryRootDraft {
    #[cfg(any(windows, target_os = "linux"))]
    {
        let authority = lx_gpui_player::library::authority::RootAuthority::open(
            path,
            true,
            &lx_gpui_player::library::scan::ScanContext::new(Duration::from_secs(30)),
        )
        .unwrap();
        let draft = authority.draft();
        let barrier = authority.retirement_barrier();
        drop(authority);
        assert!(barrier.wait(Duration::from_secs(3)));
        draft
    }
    #[cfg(not(any(windows, target_os = "linux")))]
    {
        LibraryRootDraft {
            path: path.to_path_buf(),
            recursive: true,
            anchor: FileIdentity {
                kind: "fixture".into(),
                volume: "temp".into(),
                file_id: path.to_str().unwrap().into(),
            },
            path_policy: LibraryPathPolicy::Sensitive,
        }
    }
}
fn item(path: &Path) -> LibraryPreparedAsset {
    fs::write(path, b"owned index fixture").unwrap();
    let (track, asset) = local_track_from_path(path).unwrap();
    LibraryPreparedAsset {
        track,
        asset,
        file_identity: None,
    }
}
fn full(repo: &Repository, root: &LibraryRoot, items: &[LibraryPreparedAsset], op: &str) {
    let (token, _) = repo.begin_library_scan(&root.root_id, op).unwrap();
    repo.apply_library_scan_batch(&token, &[], items, &|| Ok(()))
        .unwrap();
    let mut out = LibraryOperationView::running(&token);
    out.state = LibraryOperationState::Completed;
    repo.finish_library_scan(&token, &out, true, &|| Ok(()))
        .unwrap();
}

#[test]
fn watch_preparation_is_invisible_cancel_rolls_back_and_stale_generation_cannot_commit() {
    let profile = folder();
    let media = folder();
    let repo = Repository::open(profile.path()).unwrap();
    let (root, _) = repo.add_library_root(&draft(media.path())).unwrap();
    let revision = repo.revision().unwrap();
    let token = repo
        .prepare_library_watch(
            &root.root_id,
            true,
            &LibraryWatchOptions::default(),
            Some(0),
        )
        .unwrap();
    assert_eq!(repo.revision().unwrap(), revision);
    assert!(!repo.library_watch(&root.root_id).unwrap().unwrap().enabled);
    let cancelled = AtomicBool::new(true);
    let check = || {
        if cancelled.load(Ordering::Acquire) {
            Err(LibraryError::new(LibraryErrorKind::Cancelled, "cancelled watch activation").into())
        } else {
            Ok(())
        }
    };
    assert!(repo.commit_library_watch(&token, &check).is_err());
    assert_eq!(repo.revision().unwrap(), revision);
    assert!(repo.changes_since(revision, 10).unwrap().is_empty());
    cancelled.store(false, Ordering::Release);
    let (preference, change) = repo.commit_library_watch(&token, &check).unwrap();
    assert!(preference.enabled);
    assert_eq!(change.revision, revision + 1);
    assert!(
        repo.library_root(&root.root_id)
            .unwrap()
            .unwrap()
            .watch_enabled
    );
    assert!(repo.commit_library_watch(&token, &check).is_err());
    assert!(
        repo.prepare_library_watch(
            &root.root_id,
            false,
            &LibraryWatchOptions::default(),
            Some(0)
        )
        .is_err()
    );
    let off = repo
        .prepare_library_watch(
            &root.root_id,
            false,
            &preference.options,
            Some(preference.watch_generation),
        )
        .unwrap();
    repo.commit_library_watch(&off, &check).unwrap();
    assert!(
        !repo
            .library_root(&root.root_id)
            .unwrap()
            .unwrap()
            .watch_enabled
    );
}

#[test]
fn dirty_coverage_retains_events_received_during_scan_without_changing_user_revision() {
    let profile = folder();
    let media = folder();
    let repo = Repository::open(profile.path()).unwrap();
    let (root, _) = repo.add_library_root(&draft(media.path())).unwrap();
    let token = repo
        .prepare_library_watch(&root.root_id, true, &LibraryWatchOptions::default(), None)
        .unwrap();
    repo.commit_library_watch(&token, &|| Ok(())).unwrap();
    let revision = repo.revision().unwrap();
    repo.mark_library_dirty(
        &root.root_id,
        root.grant_epoch,
        token.watch_generation,
        10,
        true,
        &|| Ok(()),
    )
    .unwrap();
    repo.mark_library_dirty(
        &root.root_id,
        root.grant_epoch,
        token.watch_generation,
        12,
        false,
        &|| Ok(()),
    )
    .unwrap();
    let incomplete = repo
        .cover_library_dirty(
            &root.root_id,
            root.grant_epoch,
            token.watch_generation,
            10,
            false,
            &|| Ok(()),
        )
        .unwrap();
    assert_eq!(incomplete.covered_sequence, 0);
    assert!(incomplete.full_rescan_required);
    let old = repo
        .cover_library_dirty(
            &root.root_id,
            root.grant_epoch,
            token.watch_generation,
            10,
            true,
            &|| Ok(()),
        )
        .unwrap();
    assert_eq!(old.covered_sequence, 10);
    assert_eq!(old.received_sequence, 12);
    assert!(old.full_rescan_required);
    let fresh = repo
        .cover_library_dirty(
            &root.root_id,
            root.grant_epoch,
            token.watch_generation,
            12,
            true,
            &|| Ok(()),
        )
        .unwrap();
    assert!(!fresh.full_rescan_required);
    assert_eq!(repo.revision().unwrap(), revision);
    assert!(repo.changes_since(revision, 10).unwrap().is_empty());
    assert!(
        repo.mark_library_dirty(
            &root.root_id,
            root.grant_epoch,
            token.watch_generation,
            11,
            false,
            &|| Ok(())
        )
        .is_err()
    );
    repo.remove_library_root(&root.root_id).unwrap();
    assert!(
        repo.mark_library_dirty(
            &root.root_id,
            root.grant_epoch,
            token.watch_generation,
            13,
            true,
            &|| Ok(())
        )
        .is_err()
    );
}

#[test]
fn complete_incremental_path_scan_marks_only_its_declared_missing_scope() {
    let profile = folder();
    let media = folder();
    fs::create_dir(media.path().join("nested")).unwrap();
    let repo = Repository::open(profile.path()).unwrap();
    let (root, _) = repo.add_library_root(&draft(media.path())).unwrap();
    let a = item(&media.path().join("a.wav"));
    let b = item(&media.path().join("b.wav"));
    let c = item(&media.path().join("nested/c.wav"));
    full(&repo, &root, &[a.clone(), b.clone(), c.clone()], "full");
    fs::remove_file(&a.asset.path).unwrap();
    fs::remove_file(&b.asset.path).unwrap();
    let scope = LibraryScanScope::Paths {
        paths: vec!["a.wav".into()],
    };
    let (token, _) = repo
        .begin_library_scan_scoped(&root.root_id, "only-a", &scope)
        .unwrap();
    assert_eq!(repo.library_scan_scope(&token).unwrap(), scope);
    let before = repo.revision().unwrap();
    assert!(
        repo.apply_library_scan_batch(&token, &[], std::slice::from_ref(&c), &|| Ok(()))
            .is_err()
    );
    assert_eq!(repo.revision().unwrap(), before);
    let mut out = LibraryOperationView::running(&token);
    out.state = LibraryOperationState::Completed;
    let finished = repo
        .finish_library_scan(&token, &out, true, &|| Ok(()))
        .unwrap();
    assert_eq!(finished.missing_asset_ids, vec![a.asset.id.clone()]);
    assert!(!repo.local_asset(&a.asset.id).unwrap().unwrap().available);
    assert!(repo.local_asset(&b.asset.id).unwrap().unwrap().available);
    assert!(repo.local_asset(&c.asset.id).unwrap().unwrap().available);
    assert_eq!(
        repo.library_root(&root.root_id)
            .unwrap()
            .unwrap()
            .last_complete_generation,
        1
    );
    let invalid = LibraryScanScope::Subtree {
        path: "../outside".into(),
    };
    assert!(
        repo.begin_library_scan_scoped(&root.root_id, "escape", &invalid)
            .is_err()
    );
}

#[test]
fn complete_subtree_scan_preserves_unvisited_siblings_and_watch_preferences_survive_reopen() {
    let profile = folder();
    let media = folder();
    fs::create_dir(media.path().join("nested")).unwrap();
    let repo = Repository::open(profile.path()).unwrap();
    let (root, _) = repo.add_library_root(&draft(media.path())).unwrap();
    let a = item(&media.path().join("a.wav"));
    let c = item(&media.path().join("nested/c.wav"));
    full(&repo, &root, &[a.clone(), c.clone()], "full");
    let options = LibraryWatchOptions {
        debounce_ms: 100,
        max_wait_ms: 500,
        root_move_policy: RootMovePolicy::FollowSameObject,
        auto_relink: true,
    };
    let token = repo
        .prepare_library_watch(&root.root_id, true, &options, None)
        .unwrap();
    repo.commit_library_watch(&token, &|| Ok(())).unwrap();
    fs::remove_file(&c.asset.path).unwrap();
    let (scan, _) = repo
        .begin_library_scan_scoped(
            &root.root_id,
            "subtree",
            &LibraryScanScope::Subtree {
                path: "nested".into(),
            },
        )
        .unwrap();
    let mut out = LibraryOperationView::running(&scan);
    out.state = LibraryOperationState::Completed;
    repo.finish_library_scan(&scan, &out, true, &|| Ok(()))
        .unwrap();
    assert!(repo.local_asset(&a.asset.id).unwrap().unwrap().available);
    assert!(!repo.local_asset(&c.asset.id).unwrap().unwrap().available);
    let revision = repo.revision().unwrap();
    drop(repo);
    let repo = Repository::open(profile.path()).unwrap();
    assert_eq!(
        repo.library_watch(&root.root_id).unwrap().unwrap().options,
        options
    );
    assert_eq!(repo.revision().unwrap(), revision);
    assert_eq!(repo.library_watches_page(0, 100).unwrap().total, 1);
}

#![cfg(any(windows, target_os = "linux", target_os = "macos"))]
use lx_gpui_player::domain::library::LibraryRoot;
use lx_gpui_player::domain::library_watch::LibraryWatchToken;
use lx_gpui_player::library::authority::{AuthorityRegistry, RootAuthority, RootHealth};
use lx_gpui_player::library::scan::ScanContext;
use lx_gpui_player::library::watch::{NativeWatch, NativeWatchEvent};
use std::path::Path;
use std::time::{Duration, Instant};

fn root(registry: &AuthorityRegistry, path: &Path) -> LibraryRoot {
    let opened =
        RootAuthority::open(path, true, &ScanContext::new(Duration::from_secs(30))).unwrap();
    let draft = opened.draft();
    let root = LibraryRoot {
        root_id: "root:owner".into(),
        path: draft.path,
        recursive: true,
        grant_epoch: 1,
        anchor: draft.anchor,
        path_policy: draft.path_policy,
        watch_enabled: false,
        generation: 0,
        last_complete_generation: 0,
    };
    registry
        .accept_root_proof(&root, opened.proof().unwrap())
        .unwrap();
    root
}
fn token(root: &LibraryRoot, generation: u64) -> LibraryWatchToken {
    LibraryWatchToken {
        root_id: root.root_id.clone(),
        grant_epoch: root.grant_epoch,
        watch_generation: generation,
        enabled: true,
        options: Default::default(),
    }
}
fn next(watch: &mut NativeWatch) -> NativeWatchEvent {
    let began = Instant::now();
    loop {
        if let Some(event) = watch.try_recv() {
            return event;
        }
        assert!(
            began.elapsed() < Duration::from_secs(10),
            "native owner completion timeout"
        );
        std::thread::sleep(Duration::from_millis(2));
    }
}
fn prepared(watch: &mut NativeWatch) {
    match next(watch) {
        NativeWatchEvent::Prepared { result, .. } => result.unwrap(),
        event => panic!("{event:?}"),
    }
}
fn finish(mut watch: NativeWatch, registry: AuthorityRegistry) {
    assert!(watch.shutdown(Duration::from_secs(5)).unwrap());
    registry.begin_shutdown();
    assert!(registry.retirement_barrier().wait(Duration::from_secs(5)));
}

#[test]
fn native_watch_requires_existing_root_grant_and_cannot_install_an_intent() {
    let temp = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
    let authority = AuthorityRegistry::default();
    let selected = root(&authority, temp.path());
    authority.begin_shutdown();
    assert!(authority.retirement_barrier().wait(Duration::from_secs(5)));
    let fresh = AuthorityRegistry::default();
    let mut watch = NativeWatch::new(fresh.clone()).unwrap();
    watch
        .prepare(selected.clone(), token(&selected, 1), 0)
        .unwrap();
    assert!(matches!(
        next(&mut watch),
        NativeWatchEvent::Prepared { result: Err(_), .. }
    ));
    assert_eq!(fresh.root_health(&selected.root_id, 1), RootHealth::Unknown);
    assert!(watch.dirty(&selected.root_id, 1).is_none());
    finish(watch, fresh);
}
#[test]
fn contexts_are_reserved_before_subscription_and_released_after_actual_stop() {
    let temp = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
    let authority = AuthorityRegistry::default();
    let selected = root(&authority, temp.path());
    let mut watch = NativeWatch::new(authority.clone()).unwrap();
    for generation in 1..=128 {
        watch
            .prepare(selected.clone(), token(&selected, generation), 0)
            .unwrap();
        prepared(&mut watch);
    }
    assert!(
        watch
            .prepare(selected.clone(), token(&selected, 129), 0)
            .is_err()
    );
    watch.stop(&selected.root_id, 1).unwrap();
    assert!(matches!(
        next(&mut watch),
        NativeWatchEvent::Stopped {
            watch_generation: 1,
            ..
        }
    ));
    assert!(watch.dirty(&selected.root_id, 1).is_none());
    watch
        .prepare(selected.clone(), token(&selected, 129), 0)
        .unwrap();
    prepared(&mut watch);
    finish(watch, authority);
}
#[test]
fn overlapping_roots_keep_other_subscription_after_one_native_stop() {
    let temp = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
    let child = temp.path().join("child");
    std::fs::create_dir(&child).unwrap();
    let registry = AuthorityRegistry::default();
    let parent = root(&registry, temp.path());
    let opened =
        RootAuthority::open(&child, true, &ScanContext::new(Duration::from_secs(30))).unwrap();
    let draft = opened.draft();
    let child_root = LibraryRoot {
        root_id: "root:child".into(),
        path: draft.path,
        anchor: draft.anchor,
        path_policy: draft.path_policy,
        ..parent.clone()
    };
    registry
        .accept_root_proof(&child_root, opened.proof().unwrap())
        .unwrap();
    drop(opened);
    let parent_token = token(&parent, 1);
    let child_token = token(&child_root, 1);
    let mut watch = NativeWatch::new(registry.clone()).unwrap();
    watch
        .prepare(parent.clone(), parent_token.clone(), 0)
        .unwrap();
    prepared(&mut watch);
    watch
        .prepare(child_root.clone(), child_token.clone(), 0)
        .unwrap();
    prepared(&mut watch);
    watch.activate(parent_token).unwrap();
    let mut started_parent = false;
    while !started_parent {
        started_parent = matches!(next(&mut watch), NativeWatchEvent::Dirty { .. });
    }
    watch.activate(child_token).unwrap();
    let mut started_child = false;
    while !started_child {
        started_child = matches!(next(&mut watch), NativeWatchEvent::Dirty { .. });
    }
    watch.stop(&parent.root_id, 1).unwrap();
    loop {
        if matches!(next(&mut watch), NativeWatchEvent::Stopped { root_id, .. } if root_id == parent.root_id)
        {
            break;
        }
    }
    std::fs::write(child.join("still-live.wav"), b"event").unwrap();
    loop {
        match next(&mut watch) {
            NativeWatchEvent::Dirty { token, hints, .. }
                if token.root_id == child_root.root_id
                    && hints.iter().any(|h| h.path == Path::new("still-live.wav")) =>
            {
                break;
            }
            NativeWatchEvent::Error { error, .. } | NativeWatchEvent::OwnerError { error } => {
                panic!("{error}")
            }
            _ => {}
        }
    }
    assert_eq!(
        registry.root_health(&parent.root_id, 1),
        RootHealth::Verified
    );
    assert_eq!(
        registry.root_health(&child_root.root_id, 1),
        RootHealth::Verified
    );
    finish(watch, registry);
}
#[cfg(windows)]
#[test]
fn actual_root_rename_reports_same_anchor_and_native_stop_closes_retired_subscription() {
    let temp = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
    let directory = temp.path().join("selected");
    std::fs::create_dir(&directory).unwrap();
    let registry = AuthorityRegistry::default();
    let selected = root(&registry, &directory);
    let mut watch = NativeWatch::new(registry.clone()).unwrap();
    watch
        .prepare(selected.clone(), token(&selected, 1), 0)
        .unwrap();
    prepared(&mut watch);
    watch.activate(token(&selected, 1)).unwrap();
    loop {
        if matches!(next(&mut watch), NativeWatchEvent::Dirty { .. }) {
            break;
        }
    }
    let moved = temp.path().join("moved");
    std::fs::rename(&directory, &moved).unwrap();
    loop {
        match next(&mut watch) {
            NativeWatchEvent::RootMoved {
                token,
                current_path,
                anchor,
            } => {
                assert_eq!(token.root_id, selected.root_id);
                assert_eq!(
                    current_path,
                    lx_gpui_player::domain::music::normalize_local_path(
                        &std::fs::canonicalize(&moved).unwrap()
                    )
                );
                assert_eq!(anchor, selected.anchor);
                break;
            }
            NativeWatchEvent::Error { error, .. } | NativeWatchEvent::OwnerError { error } => {
                panic!("{error}")
            }
            _ => {}
        }
    }
    assert!(watch.dirty(&selected.root_id, 1).unwrap().error.is_some());
    watch.stop(&selected.root_id, 1).unwrap();
    loop {
        if matches!(
            next(&mut watch),
            NativeWatchEvent::Stopped {
                watch_generation: 1,
                ..
            }
        ) {
            break;
        }
    }
    finish(watch, registry);
    std::fs::rename(&moved, temp.path().join("closed")).unwrap();
}

#[test]
fn overlapping_follow_roots_keep_native_subscriptions_across_two_actual_parent_moves() {
    use lx_gpui_player::domain::library_watch::RootMovePolicy;
    let temp = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
    let original = temp.path().join("original");
    let child = original.join("child");
    std::fs::create_dir_all(&child).unwrap();
    let registry = AuthorityRegistry::default();
    let parent = root(&registry, &original);
    let opened =
        RootAuthority::open(&child, true, &ScanContext::new(Duration::from_secs(30))).unwrap();
    let draft = opened.draft();
    let child_root = LibraryRoot {
        root_id: "root:moving-child".into(),
        path: draft.path,
        anchor: draft.anchor,
        path_policy: draft.path_policy,
        ..parent.clone()
    };
    registry
        .accept_root_proof(&child_root, opened.proof().unwrap())
        .unwrap();
    drop(opened);
    let mut parent_token = token(&parent, 1);
    let mut child_token = token(&child_root, 1);
    parent_token.options.root_move_policy = RootMovePolicy::FollowSameObject;
    child_token.options.root_move_policy = RootMovePolicy::FollowSameObject;
    registry
        .apply_root_move_policy(&parent.root_id, 1, RootMovePolicy::FollowSameObject)
        .unwrap();
    registry
        .apply_root_move_policy(&child_root.root_id, 1, RootMovePolicy::FollowSameObject)
        .unwrap();
    let mut watch = NativeWatch::new(registry.clone()).unwrap();
    watch
        .prepare(parent.clone(), parent_token.clone(), 0)
        .unwrap();
    prepared(&mut watch);
    watch
        .prepare(child_root.clone(), child_token.clone(), 0)
        .unwrap();
    prepared(&mut watch);
    watch.activate(parent_token.clone()).unwrap();
    loop {
        if matches!(next(&mut watch), NativeWatchEvent::Dirty { .. }) {
            break;
        }
    }
    watch.activate(child_token.clone()).unwrap();
    loop {
        if matches!(next(&mut watch), NativeWatchEvent::Dirty { .. }) {
            break;
        }
    }
    let mut previous = original;
    for name in ["first-move", "second-move"] {
        let destination = temp.path().join(name);
        std::fs::rename(&previous, &destination).unwrap();
        let expected = lx_gpui_player::domain::music::normalize_local_path(
            &std::fs::canonicalize(&destination).unwrap(),
        );
        let began = Instant::now();
        loop {
            let parent_moved = watch.dirty(&parent.root_id, 1).unwrap().root_moved_path;
            let child_moved = watch.dirty(&child_root.root_id, 1).unwrap().root_moved_path;
            if parent_moved.as_deref() == Some(expected.as_path())
                && child_moved.as_deref() == Some(expected.join("child").as_path())
            {
                break;
            }
            while let Some(event) = watch.try_recv() {
                if let NativeWatchEvent::Error { error, .. }
                | NativeWatchEvent::OwnerError { error } = event
                {
                    panic!("{error}");
                }
            }
            assert!(
                began.elapsed() < Duration::from_secs(5),
                "Actual move was not sampled on both subscriptions"
            );
            std::thread::sleep(Duration::from_millis(2));
        }
        let parent_snapshot = watch.dirty(&parent.root_id, 1).unwrap();
        let child_snapshot = watch.dirty(&child_root.root_id, 1).unwrap();
        assert!(parent_snapshot.error.is_none() && child_snapshot.error.is_none());
        watch
            .cover(&parent_token, parent_snapshot.received_sequence, true)
            .unwrap();
        assert_eq!(
            watch.dirty(&parent.root_id, 1).unwrap().root_moved_path,
            Some(expected)
        );
        let before = watch
            .dirty(&child_root.root_id, 1)
            .unwrap()
            .received_sequence;
        std::fs::write(destination.join("child/live.wav"), b"still watched").unwrap();
        let began = Instant::now();
        while watch
            .dirty(&child_root.root_id, 1)
            .unwrap()
            .received_sequence
            == before
        {
            assert!(
                began.elapsed() < Duration::from_secs(5),
                "Moved child watch was closed prematurely"
            );
            std::thread::sleep(Duration::from_millis(2));
        }
        previous = destination;
    }
    finish(watch, registry);
}

#[cfg(windows)]
#[test]
fn immutable_volume_sensor_allows_unselected_ancestor_move_with_actual_same_object_reader() {
    use lx_gpui_player::domain::library::LocalAssetAuthority;
    use lx_gpui_player::domain::library_watch::RootMovePolicy;
    use std::io::{Read, Seek, SeekFrom};
    let temp = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
    let ancestor = temp.path().join("ancestor");
    let directory = ancestor.join("parent/selected");
    std::fs::create_dir_all(&directory).unwrap();
    let file = directory.join("owned.wav");
    std::fs::write(&file, b"same physical body").unwrap();
    let registry = AuthorityRegistry::default();
    let selected = root(&registry, &directory);
    registry
        .apply_root_move_policy(&selected.root_id, 1, RootMovePolicy::FollowSameObject)
        .unwrap();
    let mut request = token(&selected, 1);
    request.options.root_move_policy = RootMovePolicy::FollowSameObject;
    let authorization = registry
        .install(
            &LocalAssetAuthority {
                asset_id: "asset:ancestor".into(),
                track_id: "local-ancestor".into(),
                path: selected.path.join("owned.wav"),
                explicit_file_epoch: None,
                roots: vec![selected.clone()],
            },
            None,
        )
        .unwrap();
    let mut reader = authorization.open().unwrap();
    let expected_identity = reader.identity().clone();
    let mut watch = NativeWatch::new(registry.clone()).unwrap();
    watch.prepare(selected.clone(), request.clone(), 0).unwrap();
    prepared(&mut watch);
    watch.activate(request).unwrap();
    loop {
        if matches!(next(&mut watch), NativeWatchEvent::Dirty { .. }) {
            break;
        }
    }
    let destination = temp.path().join("ancestor-moved");
    std::fs::rename(&ancestor, &destination).unwrap();
    let expected = lx_gpui_player::domain::music::normalize_local_path(
        &std::fs::canonicalize(destination.join("parent/selected")).unwrap(),
    );
    loop {
        if matches!(next(&mut watch), NativeWatchEvent::RootMoved { current_path, .. } if current_path == expected)
        {
            break;
        }
    }
    reader.seek(SeekFrom::Start(0)).unwrap();
    let mut body = Vec::new();
    reader.read_to_end(&mut body).unwrap();
    assert_eq!(body, b"same physical body");
    assert_eq!(reader.identity(), &expected_identity);
    assert_eq!(
        watch.dirty(&selected.root_id, 1).unwrap().root_moved_path,
        Some(expected)
    );
    drop(reader);
    drop(authorization);
    finish(watch, registry);
}

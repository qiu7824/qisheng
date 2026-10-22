#![cfg(any(windows, target_os = "linux"))]
use lx_gpui_player::domain::library::LibraryRoot;
use lx_gpui_player::domain::library_watch::LibraryWatchToken;
use lx_gpui_player::library::authority::{AuthorityRegistry, RootAuthority};
use lx_gpui_player::library::scan::ScanContext;
use lx_gpui_player::library::watch::{NativeWatch, NativeWatchEvent, NativeWatchKind};
use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};

fn selected(
    registry: &AuthorityRegistry,
    path: &Path,
    root_id: &str,
    recursive: bool,
) -> LibraryRoot {
    let opened =
        RootAuthority::open(path, recursive, &ScanContext::new(Duration::from_secs(30))).unwrap();
    let draft = opened.draft();
    let root = LibraryRoot {
        root_id: root_id.into(),
        path: draft.path,
        recursive,
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
fn wait(
    watch: &mut NativeWatch,
    predicate: impl Fn(&NativeWatchEvent) -> bool,
) -> NativeWatchEvent {
    let started = Instant::now();
    loop {
        while let Some(event) = watch.try_recv() {
            match &event {
                NativeWatchEvent::Prepared {
                    result: Err(error), ..
                } => panic!("native preparation failed: {error}"),
                NativeWatchEvent::Error { error, .. } | NativeWatchEvent::OwnerError { error } => {
                    panic!("native owner failed: {error}")
                }
                _ => {}
            }
            if predicate(&event) {
                return event;
            }
        }
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "native notification timeout"
        );
        std::thread::sleep(Duration::from_millis(2));
    }
}
fn begin(watch: &mut NativeWatch, root: &LibraryRoot, token: &LibraryWatchToken) {
    watch.prepare(root.clone(), token.clone(), 0).unwrap();
    wait(watch, |event| {
        matches!(event, NativeWatchEvent::Prepared { .. })
    });
    watch.activate(token.clone()).unwrap();
    wait(watch, |event| {
        matches!(event, NativeWatchEvent::Watching { .. })
    });
    wait(watch, |event| {
        matches!(
            event,
            NativeWatchEvent::Dirty {
                full_rescan_required: true,
                ..
            }
        )
    });
    let snapshot = watch.dirty(&root.root_id, token.watch_generation).unwrap();
    watch
        .cover(token, snapshot.received_sequence, true)
        .unwrap();
}
fn hint(watch: &mut NativeWatch, path: &str, kind: NativeWatchKind) {
    wait(watch, |event| {
        matches!(event, NativeWatchEvent::Dirty { hints, .. }
        if hints.iter().any(|hint| hint.path == Path::new(path) && hint.kind == kind))
    });
}
fn finish(mut watch: NativeWatch, registry: AuthorityRegistry) {
    assert!(watch.shutdown(Duration::from_secs(5)).unwrap());
    registry.begin_shutdown();
    assert!(registry.retirement_barrier().wait(Duration::from_secs(5)));
}

#[test]
fn actual_create_modify_rename_delete_notifications_are_relative_hints() {
    let temp = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
    let registry = AuthorityRegistry::default();
    let root = selected(&registry, temp.path(), "root:native-events", true);
    let token = token(&root, 1);
    let mut watch = NativeWatch::new(registry.clone()).unwrap();
    begin(&mut watch, &root, &token);
    fs::write(temp.path().join("中文 track.wav"), b"first").unwrap();
    hint(&mut watch, "中文 track.wav", NativeWatchKind::Created);
    fs::write(temp.path().join("中文 track.wav"), b"second representation").unwrap();
    hint(&mut watch, "中文 track.wav", NativeWatchKind::Modified);
    fs::rename(
        temp.path().join("中文 track.wav"),
        temp.path().join("renamed.wav"),
    )
    .unwrap();
    wait(&mut watch, |event| {
        matches!(event, NativeWatchEvent::Dirty { hints, .. }
        if hints.iter().any(|h| h.path == Path::new("中文 track.wav") && h.kind == NativeWatchKind::RenamedOld)
            || hints.iter().any(|h| h.path == Path::new("renamed.wav") && h.kind == NativeWatchKind::RenamedNew))
    });
    fs::remove_file(temp.path().join("renamed.wav")).unwrap();
    hint(&mut watch, "renamed.wav", NativeWatchKind::Removed);
    let snapshot = watch.dirty(&root.root_id, 1).unwrap();
    assert!(snapshot.received_sequence >= 4);
    assert!(snapshot.full_rescan_required);
    finish(watch, registry);
}
#[test]
fn native_subscription_buffers_changes_before_activation_without_publishing_dirty() {
    let temp = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
    let registry = AuthorityRegistry::default();
    let root = selected(&registry, temp.path(), "root:subscribe-first", true);
    let token = token(&root, 1);
    let mut watch = NativeWatch::new(registry.clone()).unwrap();
    watch.prepare(root.clone(), token.clone(), 31).unwrap();
    wait(&mut watch, |event| {
        matches!(event, NativeWatchEvent::Prepared { .. })
    });
    fs::write(temp.path().join("during-commit.wav"), b"native changes").unwrap();
    let start = Instant::now();
    loop {
        let state = watch.dirty(&root.root_id, 1).unwrap();
        if state.received_sequence > 31 {
            break;
        }
        assert!(start.elapsed() < Duration::from_secs(5));
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(watch.try_recv().is_none());
    watch.activate(token.clone()).unwrap();
    wait(&mut watch, |event| {
        matches!(event, NativeWatchEvent::Watching { .. })
    });
    wait(&mut watch, |event| {
        matches!(
            event,
            NativeWatchEvent::Dirty {
                full_rescan_required: true,
                ..
            }
        )
    });
    let state = watch.dirty(&root.root_id, 1).unwrap();
    assert!(state.received_sequence > 31 && state.pending_paths > 0);
    watch.cover(&token, 31, true).unwrap();
    assert!(watch.dirty(&root.root_id, 1).unwrap().full_rescan_required);
    finish(watch, registry);
}
#[test]
fn new_recursive_subdirectory_is_subscribed_and_reported_after_installation_gap() {
    let temp = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
    fs::create_dir(temp.path().join("existing")).unwrap();
    let registry = AuthorityRegistry::default();
    let root = selected(&registry, temp.path(), "root:recursive", true);
    let token = token(&root, 1);
    let mut watch = NativeWatch::new(registry.clone()).unwrap();
    begin(&mut watch, &root, &token);
    fs::write(temp.path().join("existing/old.wav"), b"existing subtree").unwrap();
    hint(&mut watch, "existing/old.wav", NativeWatchKind::Created);
    fs::create_dir_all(temp.path().join("new/deeper")).unwrap();
    wait(&mut watch, |event| {
        matches!(event, NativeWatchEvent::Dirty { .. })
    });
    let began = Instant::now();
    let mut found = false;
    while !found {
        fs::write(
            temp.path().join("new/deeper/after-install.wav"),
            b"changing",
        )
        .unwrap();
        std::thread::sleep(Duration::from_millis(25));
        while let Some(event) = watch.try_recv() {
            if let NativeWatchEvent::Dirty { hints, .. } = event {
                found |= hints
                    .iter()
                    .any(|h| h.path == Path::new("new/deeper/after-install.wav"));
            }
        }
        assert!(
            began.elapsed() < Duration::from_secs(10),
            "new recursive wd was never installed"
        );
    }
    finish(watch, registry);
}
#[test]
fn nonrecursive_native_subscription_does_not_report_child_payloads() {
    let temp = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
    fs::create_dir(temp.path().join("private-child")).unwrap();
    let registry = AuthorityRegistry::default();
    let root = selected(&registry, temp.path(), "root:nonrecursive", false);
    let token = token(&root, 1);
    let mut watch = NativeWatch::new(registry.clone()).unwrap();
    begin(&mut watch, &root, &token);
    let before = watch.dirty(&root.root_id, 1).unwrap().received_sequence;
    fs::write(
        temp.path().join("private-child/not-included.wav"),
        b"private",
    )
    .unwrap();
    let began = Instant::now();
    while began.elapsed() < Duration::from_millis(250) {
        assert!(watch.try_recv().is_none());
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(
        watch.dirty(&root.root_id, 1).unwrap().received_sequence,
        before
    );
    fs::write(temp.path().join("selected.wav"), b"selected").unwrap();
    hint(&mut watch, "selected.wav", NativeWatchKind::Created);
    finish(watch, registry);
}
#[test]
fn absent_generation_zero_stop_has_deterministic_acknowledgement() {
    let registry = AuthorityRegistry::default();
    let mut watch = NativeWatch::new(registry.clone()).unwrap();
    watch.stop("root:absent", 0).unwrap();
    let event = wait(&mut watch, |event| {
        matches!(event, NativeWatchEvent::Stopped { .. })
    });
    assert!(
        matches!(event, NativeWatchEvent::Stopped { root_id, watch_generation: 0 } if root_id == "root:absent")
    );
    finish(watch, registry);
}
#[test]
fn application_backpressure_preserves_atomic_full_dirty_after_actual_fs_storm() {
    let temp = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
    let registry = AuthorityRegistry::default();
    let root = selected(&registry, temp.path(), "root:channel-overflow", true);
    let token = token(&root, 1);
    let mut watch = NativeWatch::new(registry.clone()).unwrap();
    begin(&mut watch, &root, &token);
    for index in 0..90 {
        let before = watch.dirty(&root.root_id, 1).unwrap().received_sequence;
        fs::write(temp.path().join(format!("{index:03}.wav")), b"event").unwrap();
        let start = Instant::now();
        while watch.dirty(&root.root_id, 1).unwrap().received_sequence == before {
            assert!(start.elapsed() < Duration::from_secs(5));
            std::thread::sleep(Duration::from_millis(1));
        }
    }
    let mut stable_since = Instant::now();
    let mut last_sequence = watch.dirty(&root.root_id, 1).unwrap().received_sequence;
    while stable_since.elapsed() < Duration::from_millis(80) {
        let received = watch.dirty(&root.root_id, 1).unwrap().received_sequence;
        if received != last_sequence {
            last_sequence = received;
            stable_since = Instant::now();
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    let state = watch.dirty(&root.root_id, 1).unwrap();
    assert!(
        state.dropped_events > 0,
        "64-element application event channel never filled"
    );
    assert!(state.full_rescan_required);
    assert!(state.pending_paths <= 200);
    let mut consumed = 0;
    while watch.try_recv().is_some() {
        consumed += 1;
    }
    assert!(consumed <= 64);
    watch.cover(&token, state.received_sequence, true).unwrap();
    assert!(watch.dirty(&root.root_id, 1).unwrap().covered_sequence >= state.received_sequence);
    finish(watch, registry);
}

#[test]
fn unselected_neighbor_changes_do_not_produce_relative_hints_or_dirty_sequences() {
    let temp = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
    let directory = temp.path().join("selected");
    let neighbor = temp.path().join("unselected");
    fs::create_dir(&directory).unwrap();
    fs::create_dir(&neighbor).unwrap();
    let registry = AuthorityRegistry::default();
    let root = selected(&registry, &directory, "root:bounded-scope", true);
    let token = token(&root, 1);
    let mut watch = NativeWatch::new(registry.clone()).unwrap();
    begin(&mut watch, &root, &token);
    let before = watch.dirty(&root.root_id, 1).unwrap().received_sequence;
    for index in 0..12 {
        fs::write(neighbor.join(format!("outside-{index}.wav")), b"unselected").unwrap();
    }
    let began = Instant::now();
    while began.elapsed() < Duration::from_millis(250) {
        assert!(
            watch.try_recv().is_none(),
            "Unselected names were forwarded"
        );
        std::thread::sleep(Duration::from_millis(2));
    }
    let snapshot = watch.dirty(&root.root_id, 1).unwrap();
    assert_eq!(snapshot.received_sequence, before);
    assert!(!snapshot.full_rescan_required);
    fs::write(directory.join("selected.wav"), b"selected").unwrap();
    hint(&mut watch, "selected.wav", NativeWatchKind::Created);
    finish(watch, registry);
}

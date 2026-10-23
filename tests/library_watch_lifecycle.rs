#![cfg(any(windows, target_os = "linux", target_os = "macos"))]

#[path = "support/library_watch_fixture.rs"]
mod fixture;

use fixture::{Running, assets, named, options, set_watch, settled, watch, wave};
use lx_gpui_player::{
    backend::{BackendCommand, BackendQuery, BackendQueryResult, ErrorKind},
    domain::library_watch::LibraryWatchState,
};
#[cfg(windows)]
use std::path::Path;
use std::{fs, time::Duration};

#[cfg(windows)]
fn exclusive_directory(path: &Path) -> std::io::Result<fs::File> {
    use std::os::windows::fs::OpenOptionsExt;
    fs::OpenOptions::new()
        .read(true)
        .share_mode(0)
        .custom_flags(windows_sys::Win32::Storage::FileSystem::FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)
}

#[test]
fn watch_off_is_a_native_owner_boundary_not_a_revocation_and_reenable_baselines_the_gap() {
    let running = Running::new();
    let selected = running.folder("watch-off-selected");
    let first = selected.join("still-granted-997.wav");
    wave(&first, 997.0, 30.0);
    let selected_root = running.add_root("watch-off-root-add", selected.clone(), true);
    set_watch(
        &running,
        "watch-off-initial-enable",
        &selected_root,
        true,
        options(false),
    );
    let existing = named(&running.backend, "still-granted-997.wav");
    let active = watch(&running.backend, &selected_root.root_id).unwrap();
    set_watch(
        &running,
        "watch-off-native-stop",
        &selected_root,
        false,
        options(false),
    );
    let stopped = watch(&running.backend, &selected_root.root_id).unwrap();
    assert_eq!(stopped.state, LibraryWatchState::Disabled);
    assert!(!stopped.preference.enabled);
    assert!(stopped.preference.watch_generation > active.preference.watch_generation);
    let silent_gap = selected.join("created-during-watch-off.wav");
    wave(&silent_gap, 880.0, 0.1);
    assert!(
        assets(&running.backend, Some(&selected_root.root_id))
            .iter()
            .all(|item| item.asset.path.file_name() != silent_gap.file_name())
    );
    running.play(
        "watch-off-reading-grant-survives",
        &existing.asset.track_id,
        997.0,
    );
    fixture::stop(&running, "watch-off-stop-reader");
    set_watch(
        &running,
        "watch-off-reenable",
        &selected_root,
        true,
        options(false),
    );
    let restarted = settled(&running, &selected_root, |rows| {
        fixture::file_indexed(rows, &silent_gap)
    });
    assert!(restarted.preference.watch_generation > stopped.preference.watch_generation);
    assert_ne!(
        restarted.last_scan_operation_id,
        active.last_scan_operation_id
    );
    let BackendQueryResult::LibraryWatches(page) = running
        .backend
        .query(BackendQuery::LibraryWatches {
            offset: 0,
            limit: 10,
        })
        .unwrap()
    else {
        panic!("unexpected runtime watch page")
    };
    assert_eq!(page.total, 1);
    assert_eq!(page.items[0].state, LibraryWatchState::Watching);
    assert!(page.items[0].preference.enabled);
}

#[test]
fn public_shutdown_timeout_retains_ownership_until_retry_joins_and_releases_native_fh() {
    let mut running = Running::new();
    let selected = running.folder("watch-shutdown-native-pending");
    let file = selected.join("shutdown-owned.wav");
    wave(&file, 997.0, 0.1);
    let selected_root = running.add_root("watch-shutdown-add", selected.clone(), true);
    set_watch(
        &running,
        "watch-shutdown-enable",
        &selected_root,
        true,
        options(false),
    );
    #[cfg(windows)]
    assert!(
        exclusive_directory(&selected).is_err(),
        "active native/root owner did not retain an actual directory FH"
    );
    let timeout = running.backend.shutdown(Duration::ZERO).unwrap_err();
    assert_eq!(timeout.kind, ErrorKind::Timeout);
    assert_eq!(
        running
            .backend
            .execute(fixture::envelope(
                "watch-shutdown-no-late-command",
                BackendCommand::ScanLibraryRoot {
                    root_id: selected_root.root_id.clone(),
                    options: Default::default(),
                }
            ))
            .unwrap_err()
            .kind,
        ErrorKind::Closed
    );
    running.backend.shutdown(Duration::from_secs(15)).unwrap();
    assert!(running.backend.snapshot().stopped);
    running.backend.shutdown(Duration::ZERO).unwrap();
    #[cfg(windows)]
    drop(
        exclusive_directory(&selected)
            .expect("successful public shutdown left native/proof directory FH alive"),
    );
    let moved = running.directory.path().join("after-native-join");
    fs::rename(&selected, &moved).unwrap();
    fs::rename(&moved, &selected).unwrap();
    let offline = selected.join("created-with-profile-closed.wav");
    wave(&offline, 880.0, 0.1);
    running.restart();
    let restored = settled(&running, &selected_root, |rows| {
        fixture::file_indexed(rows, &offline)
    });
    assert!(restored.preference.enabled);
    assert_eq!(restored.state, LibraryWatchState::Watching);
    assert!(restored.last_scan_operation_id.is_some());
    assert_eq!(running.backend.snapshot().library.running_operations, 0);
}

#[test]
fn real_event_burst_and_old_generation_work_cannot_reactivate_a_disabled_watch() {
    let running = Running::new();
    let selected = running.folder("watch-old-generation");
    let first = selected.join("retained-first.wav");
    wave(&first, 440.0, 0.1);
    let selected_root = running.add_root("watch-generation-add", selected.clone(), true);
    set_watch(
        &running,
        "watch-generation-enable",
        &selected_root,
        true,
        options(false),
    );
    let before = watch(&running.backend, &selected_root.root_id).unwrap();
    fixture::corpus(&selected, 401);
    fixture::wait(&running.backend, 15.0, |_| {
        watch(&running.backend, &selected_root.root_id)
            .unwrap()
            .received_sequence
            > before.received_sequence
    });
    set_watch(
        &running,
        "watch-generation-stop-burst",
        &selected_root,
        false,
        options(false),
    );
    let stopped = watch(&running.backend, &selected_root.root_id).unwrap();
    assert!(stopped.preference.watch_generation > before.preference.watch_generation);
    fixture::bounded_watch_generation(
        &running.backend,
        &selected_root.root_id,
        stopped.preference.watch_generation,
    );
    assert_eq!(
        watch(&running.backend, &selected_root.root_id)
            .unwrap()
            .state,
        LibraryWatchState::Disabled
    );
    let late = selected.join("only-new-baseline-can-discover.wav");
    wave(&late, 997.0, 0.1);
    assert!(
        running
            .backend
            .tracks("only-new-baseline-can-discover", 0, 10)
            .unwrap()
            .items
            .is_empty()
    );
    set_watch(
        &running,
        "watch-generation-enable-again",
        &selected_root,
        true,
        options(false),
    );
    let current = settled(&running, &selected_root, |rows| {
        fixture::file_indexed(rows, &late)
            && rows
                .iter()
                .filter(|item| {
                    item.asset.available && item.root_ids.contains(&selected_root.root_id)
                })
                .count()
                == 403
    });
    assert!(current.preference.watch_generation > stopped.preference.watch_generation);
    assert!(
        assets(&running.backend, Some(&selected_root.root_id))
            .iter()
            .all(|item| item.asset.available)
    );
    assert!(named(&running.backend, "retained-first.wav").authorized);
}

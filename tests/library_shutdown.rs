#[path = "support/library_fixture.rs"]
mod fixture;

use fixture::{Running, assets, corpus, envelope, library_operation, library_terminal, root, wave};
use lx_gpui_player::{
    backend::{BackendCommand, BackendConfig, BackendHandle, ErrorKind},
    domain::library::{LibraryOperationState, LibraryScanOptions},
};
use std::{fs, time::Duration};

#[test]
fn cancelling_a_real_large_scan_keeps_unvisited_old_members_and_the_last_complete_generation() {
    let running = Running::new();
    let selected = running.folder("cancel-selected");
    let deleted = selected.join("old-member.wav");
    wave(&deleted, 440.0, 0.1);
    let added = running.add_root("cancel-add", selected.clone(), true);
    let complete = running.scan(
        "cancel-original-complete",
        &added.root_id,
        LibraryScanOptions::default(),
    );
    assert_eq!(complete.state, LibraryOperationState::Completed);
    let old = assets(&running.backend, Some(&added.root_id))
        .pop()
        .unwrap();
    fs::remove_file(deleted).unwrap();
    corpus(&selected, 1600);
    running.execute(
        "cancel-large-scan",
        BackendCommand::ScanLibraryRoot {
            root_id: added.root_id.clone(),
            options: LibraryScanOptions::default(),
        },
    );
    let active = library_operation(&running.backend, "cancel-large-scan").unwrap();
    assert_eq!(active.state, LibraryOperationState::Running);
    running.execute(
        "cancel-large-operation",
        BackendCommand::CancelOperation {
            operation_id: "cancel-large-scan".into(),
        },
    );
    let cancelled = library_terminal(&running.backend, "cancel-large-scan");
    assert_eq!(
        cancelled.state,
        LibraryOperationState::Cancelled,
        "{cancelled:?}"
    );
    assert_eq!(cancelled.missing_files, 0);
    let after = assets(&running.backend, None);
    assert!(
        after
            .iter()
            .find(|item| item.asset.id == old.asset.id)
            .unwrap()
            .asset
            .available
    );
    assert_eq!(
        root(&running.backend, &added.root_id)
            .unwrap()
            .last_complete_generation,
        complete.generation.unwrap()
    );
    assert_eq!(running.backend.snapshot().library.running_operations, 0);
}

#[test]
fn shutdown_timeout_can_be_retried_until_real_library_worker_join_releases_the_profile() {
    let running = Running::new();
    let selected = running.folder("shutdown-selected");
    corpus(&selected, 1600);
    let added = running.add_root("shutdown-add", selected, true);
    running.execute(
        "shutdown-large-scan",
        BackendCommand::ScanLibraryRoot {
            root_id: added.root_id.clone(),
            options: LibraryScanOptions::default(),
        },
    );
    assert_eq!(
        library_operation(&running.backend, "shutdown-large-scan")
            .unwrap()
            .state,
        LibraryOperationState::Running
    );
    assert!(running.backend.snapshot().library.running_operations >= 1);
    let first = running.backend.shutdown(Duration::ZERO).unwrap_err();
    assert_eq!(first.kind, ErrorKind::Timeout);
    assert_eq!(
        running
            .backend
            .execute(envelope(
                "closed-library-command",
                BackendCommand::ScanLibraryRoot {
                    root_id: added.root_id,
                    options: LibraryScanOptions::default(),
                }
            ))
            .unwrap_err()
            .kind,
        ErrorKind::Closed
    );
    running.backend.shutdown(Duration::from_secs(15)).unwrap();
    assert!(running.backend.snapshot().stopped);
    running.backend.shutdown(Duration::ZERO).unwrap();
    let restored = BackendHandle::start(BackendConfig::headless(running.profile())).unwrap();
    let final_operation = library_operation(&restored, "shutdown-large-scan").unwrap();
    assert!(matches!(
        final_operation.state,
        LibraryOperationState::Cancelled | LibraryOperationState::Interrupted
    ));
    assert_eq!(restored.snapshot().library.running_operations, 0);
    restored.shutdown(Duration::from_secs(15)).unwrap();
}

#[path = "support/library_fixture.rs"]
mod fixture;

use fixture::{Running, assets, root, wave};
use lx_gpui_player::{
    backend::BackendCommand,
    domain::library::{LibraryOperationState, LibraryScanOptions},
};
use std::fs;

#[test]
fn revoked_root_cannot_reread_changed_sidecar_even_when_the_old_raw_lyric_cache_is_retained() {
    use lx_gpui_player::{
        lyrics::service::{LyricProvenance, LyricRequest, LyricService},
        repository::Repository,
    };
    use std::{sync::Arc, time::Duration};
    let running = Running::new();
    let selected = running.folder("lyric-selected");
    let path = selected.join("lyric-owned.wav");
    wave(&path, 997.0, 20.0);
    fs::write(
        path.with_extension("lrc"),
        "[00:00.000]ORIGINAL AUTHORIZED SIDECAR",
    )
    .unwrap();
    let added = running.add_root("lyric-root-add", selected, true);
    assert_eq!(
        running
            .scan(
                "lyric-root-scan",
                &added.root_id,
                LibraryScanOptions::default()
            )
            .state,
        LibraryOperationState::Completed
    );
    let asset = assets(&running.backend, Some(&added.root_id))
        .pop()
        .unwrap();
    running.play("lyric-root-play", &asset.asset.track_id, 997.0);
    fixture::wait(&running.backend, 12.0, |snapshot| {
        snapshot.lyrics.as_ref().is_some_and(|lyrics| {
            lyrics.provenance == LyricProvenance::LocalSidecar
                && lyrics.bundle.lyric.text() == Some("[00:00.000]ORIGINAL AUTHORIZED SIDECAR")
        })
    });
    running.execute(
        "lyric-root-revoke",
        BackendCommand::RemoveLibraryRoot {
            root_id: added.root_id,
        },
    );
    running.backend.shutdown(Duration::from_secs(15)).unwrap();
    fs::write(
        path.with_extension("lrc"),
        "[00:00.000]NEW CONTENT WITHOUT A GRANT",
    )
    .unwrap();
    let repository = Arc::new(Repository::open(running.profile()).unwrap());
    let service = LyricService::new(repository);
    let prepared = service
        .prepare(LyricRequest::new(asset.asset.track_id, 91), None)
        .unwrap();
    assert_eq!(prepared.provenance, LyricProvenance::RawCache);
    assert_eq!(
        prepared.bundle.lyric.text(),
        Some("[00:00.000]ORIGINAL AUTHORIZED SIDECAR")
    );
}

#[test]
fn depth_limited_scan_cannot_mark_unvisited_deleted_members_missing() {
    let running = Running::new();
    let selected = running.folder("depth-selected");
    let nested = selected.join("unvisited-depth");
    fs::create_dir_all(&nested).unwrap();
    let retained = selected.join("retained.wav");
    let deleted = nested.join("deleted-later.wav");
    wave(&retained, 440.0, 0.1);
    wave(&deleted, 880.0, 0.1);
    fs::write(nested.join("keep-directory.txt"), b"owned non-audio marker").unwrap();
    let added = running.add_root("depth-add", selected, true);
    let first = running.scan(
        "depth-initial-complete",
        &added.root_id,
        LibraryScanOptions::default(),
    );
    assert_eq!(first.state, LibraryOperationState::Completed);
    let initial = assets(&running.backend, Some(&added.root_id));
    let missing_id = initial
        .iter()
        .find(|item| item.asset.path.ends_with("deleted-later.wav"))
        .unwrap()
        .asset
        .id
        .clone();
    fs::remove_file(&deleted).unwrap();
    let partial = running.scan(
        "depth-partial",
        &added.root_id,
        LibraryScanOptions {
            max_depth: 0,
            ..Default::default()
        },
    );
    assert_eq!(partial.state, LibraryOperationState::Partial, "{partial:?}");
    assert_eq!(partial.missing_files, 0);
    assert!(!partial.warnings.is_empty());
    let after_partial = assets(&running.backend, None);
    assert!(
        after_partial
            .iter()
            .find(|item| item.asset.id == missing_id)
            .unwrap()
            .asset
            .available
    );
    assert_eq!(
        root(&running.backend, &added.root_id)
            .unwrap()
            .last_complete_generation,
        first.generation.unwrap()
    );
    let complete = running.scan(
        "depth-final-complete",
        &added.root_id,
        LibraryScanOptions::default(),
    );
    assert_eq!(
        complete.state,
        LibraryOperationState::Completed,
        "{complete:?}"
    );
    assert_eq!(complete.missing_files, 1);
    let after_complete = assets(&running.backend, None);
    assert!(
        !after_complete
            .iter()
            .find(|item| item.asset.id == missing_id)
            .unwrap()
            .asset
            .available
    );
    assert!(
        after_complete
            .iter()
            .find(|item| item.asset.path.ends_with("retained.wav"))
            .unwrap()
            .asset
            .available
    );
}

#[test]
fn a_failed_budget_scan_retains_unvisited_members_and_complete_generation() {
    let running = Running::new();
    let selected = running.folder("budget-selected");
    let files = fixture::corpus(&selected, 5);
    let added = running.add_root("budget-add", selected, true);
    let first = running.scan(
        "budget-initial",
        &added.root_id,
        LibraryScanOptions::default(),
    );
    assert_eq!(first.state, LibraryOperationState::Completed);
    let initial = assets(&running.backend, Some(&added.root_id));
    let deleted_id = initial
        .iter()
        .find(|item| item.asset.path.ends_with("owned-0004.wav"))
        .unwrap()
        .asset
        .id
        .clone();
    fs::remove_file(&files[4]).unwrap();
    let failed = running.scan(
        "budget-failed",
        &added.root_id,
        LibraryScanOptions {
            max_files: 1,
            ..Default::default()
        },
    );
    assert_ne!(failed.state, LibraryOperationState::Completed);
    assert_eq!(failed.missing_files, 0);
    assert!(failed.error.is_some() || !failed.warnings.is_empty());
    assert!(
        assets(&running.backend, None)
            .iter()
            .find(|item| item.asset.id == deleted_id)
            .unwrap()
            .asset
            .available
    );
    assert_eq!(
        root(&running.backend, &added.root_id)
            .unwrap()
            .last_complete_generation,
        first.generation.unwrap()
    );
}

#[test]
fn a_discovered_but_invalid_audio_file_remains_seen_and_keeps_its_stable_identity() {
    let running = Running::new();
    let selected = running.folder("changed-selected");
    let path = selected.join("changed.wav");
    wave(&path, 440.0, 0.1);
    let added = running.add_root("changed-add", selected, true);
    assert_eq!(
        running
            .scan(
                "changed-initial",
                &added.root_id,
                LibraryScanOptions::default()
            )
            .state,
        LibraryOperationState::Completed
    );
    let before = assets(&running.backend, Some(&added.root_id))
        .pop()
        .unwrap();
    fs::write(&path, b"owned invalid WAV payload").unwrap();
    let partial = running.scan(
        "changed-invalid-metadata",
        &added.root_id,
        LibraryScanOptions::default(),
    );
    assert_eq!(partial.state, LibraryOperationState::Partial, "{partial:?}");
    assert_eq!(partial.discovered_files, 1);
    assert_eq!(partial.missing_files, 0);
    assert!(partial.skipped_files >= 1);
    let after = assets(&running.backend, None)
        .into_iter()
        .find(|item| item.asset.id == before.asset.id)
        .unwrap();
    assert_eq!(after.asset.track_id, before.asset.track_id);
    assert_eq!(after.asset.id, before.asset.id);
    assert!(after.root_ids.contains(&added.root_id));
}

#[test]
fn replacing_a_root_directory_requires_fresh_authorization_after_restart() {
    let mut running = Running::new();
    let selected = running.folder("anchor-selected");
    wave(&selected.join("same-path.wav"), 440.0, 20.0);
    let added = running.add_root("anchor-add", selected.clone(), true);
    assert_eq!(
        running
            .scan(
                "anchor-initial",
                &added.root_id,
                LibraryScanOptions::default()
            )
            .state,
        LibraryOperationState::Completed
    );
    let before = assets(&running.backend, Some(&added.root_id))
        .pop()
        .unwrap();
    // Directory replacement occurs while the application is closed; active
    // Windows authority handles may correctly prohibit delete/rename sharing.
    running
        .backend
        .shutdown(std::time::Duration::from_secs(15))
        .unwrap();
    let moved = running.directory.path().join("former-authorized-directory");
    assert!(
        selected.starts_with(running.directory.path())
            && moved.starts_with(running.directory.path())
    );
    fs::rename(&selected, &moved).unwrap();
    fs::create_dir(&selected).unwrap();
    wave(&selected.join("same-path.wav"), 1760.0, 20.0);
    running.restart();
    let after = assets(&running.backend, None)
        .into_iter()
        .find(|item| item.asset.id == before.asset.id)
        .unwrap();
    assert!(!after.authorized && !after.explicit_file_grant);
    let denied = running.scan(
        "anchor-invalid-scan",
        &added.root_id,
        LibraryScanOptions::default(),
    );
    assert_ne!(denied.state, LibraryOperationState::Completed);
    assert_eq!(denied.missing_files, 0);
    assert!(denied.error.is_some());
    running.execute(
        "anchor-remove-tombstone",
        BackendCommand::RemoveLibraryRoot {
            root_id: added.root_id.clone(),
        },
    );
    let fresh = running.add_root("anchor-reapprove", selected, true);
    assert!(fresh.grant_epoch > added.grant_epoch);
    assert_eq!(
        running
            .scan(
                "anchor-fresh-scan",
                &fresh.root_id,
                LibraryScanOptions::default()
            )
            .state,
        LibraryOperationState::Completed
    );
    let restored = assets(&running.backend, Some(&fresh.root_id))
        .pop()
        .unwrap();
    assert!(restored.authorized && !restored.explicit_file_grant);
    assert_eq!(restored.asset.id, before.asset.id);
    assert_eq!(restored.asset.track_id, before.asset.track_id);
}

#[cfg(windows)]
#[test]
fn a_nested_junction_cannot_grant_audio_from_an_unselected_neighbor_directory() {
    use std::os::windows::process::CommandExt;
    let running = Running::new();
    let selected = running.folder("junction-selected");
    let neighbor = running.folder("junction-neighbor");
    wave(&selected.join("inside.wav"), 440.0, 0.1);
    wave(&neighbor.join("outside.wav"), 880.0, 0.1);
    let link = selected.join("nested-junction");
    assert!(
        link.starts_with(running.directory.path())
            && neighbor.starts_with(running.directory.path())
    );
    let output = std::process::Command::new("cmd.exe")
        .args(["/c", "mklink", "/J"])
        .arg(&link)
        .arg(&neighbor)
        .creation_flags(0x08000000)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "junction creation failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    struct Link(std::path::PathBuf);
    impl Drop for Link {
        fn drop(&mut self) {
            let _ = fs::remove_dir(&self.0);
        }
    }
    let _remove_link_only = Link(link);
    let added = running.add_root("junction-add", selected, true);
    let result = running.scan(
        "junction-scan",
        &added.root_id,
        LibraryScanOptions::default(),
    );
    assert_eq!(result.missing_files, 0);
    let indexed = assets(&running.backend, Some(&added.root_id));
    assert_eq!(indexed.len(), 1);
    assert!(indexed[0].asset.path.ends_with("inside.wav"));
    assert_eq!(running.backend.tracks("outside", 0, 10).unwrap().total, 0);
    assert!(neighbor.join("outside.wav").is_file());
}

#[cfg(unix)]
#[test]
fn a_nested_symlink_cannot_grant_audio_from_an_unselected_neighbor_directory() {
    let running = Running::new();
    let selected = running.folder("link-selected");
    let neighbor = running.folder("link-neighbor");
    wave(&selected.join("inside.wav"), 440.0, 0.1);
    wave(&neighbor.join("outside.wav"), 880.0, 0.1);
    std::os::unix::fs::symlink(&neighbor, selected.join("nested-link")).unwrap();
    let added = running.add_root("link-add", selected, true);
    running.scan("link-scan", &added.root_id, LibraryScanOptions::default());
    let indexed = assets(&running.backend, Some(&added.root_id));
    assert_eq!(indexed.len(), 1);
    assert!(indexed[0].asset.path.ends_with("inside.wav"));
    assert!(neighbor.join("outside.wav").is_file());
}

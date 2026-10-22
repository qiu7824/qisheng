#[path = "support/library_fixture.rs"]
mod fixture;

use fixture::{
    Running, assert_same_media, assets, cache_epoch, corpus, envelope, library_terminal, root, wave,
};
use lx_gpui_player::{
    backend::{BackendCommand, BackendQuery, BackendQueryResult, ErrorKind},
    domain::{
        library::{LibraryOperationState, LibraryScanOptions},
        lists::{AddLocation, ListAction},
        lyric::{LyricBundle, LyricOrigin},
    },
    repository::local_track_from_path,
    store::ProjectStore,
};
use std::{fs, time::Duration};

#[test]
fn narrowing_a_root_replaces_its_epoch_and_revokes_live_routes_without_escalating_other_grants() {
    use lx_gpui_player::{
        audio::PlaybackStatus,
        domain::artwork::PictureState,
        lyrics::service::{LyricProvenance, LyricRequest, LyricService},
        repository::Repository,
    };
    use std::sync::Arc;

    let mut running = Running::new();
    let selected = running.folder("scope-parent");
    let nested = selected.join("recursive-only");
    let shared = selected.join("independent-child-root");
    fs::create_dir_all(&nested).unwrap();
    fs::create_dir_all(&shared).unwrap();
    let top = selected.join("top-level.wav");
    let root_only = nested.join("nested-root-only.wav");
    let explicit = nested.join("nested-explicit.wav");
    let other = shared.join("nested-other-root.wav");
    wave(&top, 440.0, 30.0);
    wave(&root_only, 997.0, 30.0);
    wave(&explicit, 660.0, 30.0);
    wave(&other, 880.0, 30.0);
    add_owned_picture(&root_only);
    fs::write(
        root_only.with_extension("lrc"),
        "[00:00.000]ORIGINAL RECURSIVE SIDECAR",
    )
    .unwrap();
    let (top_record, _) = local_track_from_path(&top).unwrap();
    let (nested_record, _) = local_track_from_path(&root_only).unwrap();
    let (explicit_record, _) = local_track_from_path(&explicit).unwrap();
    let (other_record, _) = local_track_from_path(&other).unwrap();
    running.import_explicit("scope-independent-file", vec![explicit]);
    let parent = running.add_root("scope-parent-add", selected.clone(), true);
    let child = running.add_root("scope-child-add", shared, false);
    for (request, root_id) in [
        ("scope-parent-scan", &parent.root_id),
        ("scope-child-scan", &child.root_id),
    ] {
        assert_eq!(
            running
                .scan(request, root_id, LibraryScanOptions::default())
                .state,
            LibraryOperationState::Completed
        );
    }
    running.execute(
        "scope-user-list",
        BackendCommand::ListAction {
            action: Box::new(ListAction::MusicAdd {
                list_id: "default".into(),
                tracks: vec![
                    top_record.clone(),
                    nested_record.clone(),
                    explicit_record.clone(),
                    other_record.clone(),
                ],
                location: AddLocation::Bottom,
            }),
        },
    );
    assert_eq!(
        resolve_picture(&running, "scope-picture-before", &nested_record.id).state,
        PictureState::Ready
    );
    enable_catalog_url_source(&running);
    let old_url = play_with_catalog_url(&running, "scope-nested-capture", &nested_record.id, 997.0);
    let other_url = catalog_track_url(&old_url, &other_record.id);
    let explicit_url = catalog_track_url(&old_url, &explicit_record.id);
    running.play("scope-other-before", &other_record.id, 880.0);
    assert_catalog_audio(&other_url);
    running.play("scope-explicit-before", &explicit_record.id, 660.0);
    assert_catalog_audio(&explicit_url);
    running.play("scope-nested-before", &nested_record.id, 997.0);
    fixture::wait(&running.backend, 12.0, |snapshot| {
        snapshot.lyrics.as_ref().is_some_and(|lyrics| {
            lyrics.provenance == LyricProvenance::LocalSidecar
                && lyrics.bundle.lyric.text() == Some("[00:00.000]ORIGINAL RECURSIVE SIDECAR")
        })
    });
    assert_catalog_audio(&old_url);
    corpus(&nested.join("old-epoch-discovery"), 401);
    running.execute(
        "scope-old-epoch-scan",
        BackendCommand::ScanLibraryRoot {
            root_id: parent.root_id.clone(),
            options: LibraryScanOptions::default(),
        },
    );
    assert_eq!(
        fixture::library_operation(&running.backend, "scope-old-epoch-scan")
            .unwrap()
            .state,
        LibraryOperationState::Running
    );
    let previous = root(&running.backend, &parent.root_id).unwrap();
    let narrowed = running.add_root("scope-narrow-parent", selected, false);
    assert_eq!(narrowed.root_id, parent.root_id);
    assert_eq!(narrowed.grant_epoch, previous.grant_epoch + 1);
    assert!(!narrowed.recursive);
    assert_eq!(narrowed.generation, previous.generation);
    assert_eq!(narrowed.last_complete_generation, 0);
    let old_operation = library_terminal(&running.backend, "scope-old-epoch-scan");
    assert_ne!(old_operation.state, LibraryOperationState::Running);
    assert_eq!(
        old_operation.root_id.as_deref(),
        Some(parent.root_id.as_str())
    );
    assert!(assets(&running.backend, Some(&parent.root_id)).is_empty());
    assert_eq!(running.backend.snapshot().library.running_operations, 0);
    let BackendQueryResult::LibraryOperations(operations) = running
        .backend
        .query(BackendQuery::LibraryOperations {
            offset: 0,
            limit: 100,
        })
        .unwrap()
    else {
        panic!("unexpected library operation page")
    };
    assert!(
        operations
            .items
            .iter()
            .all(|operation| operation.state != LibraryOperationState::Running)
    );
    fixture::wait(&running.backend, 8.0, |snapshot| {
        snapshot.playback.status != PlaybackStatus::Playing
    });
    let indexed = assets(&running.backend, None);
    for record in [&nested_record, &top_record] {
        let asset = indexed
            .iter()
            .find(|asset| asset.asset.track_id == record.id)
            .unwrap();
        assert!(!asset.authorized && !asset.explicit_file_grant);
        assert!(asset.root_ids.is_empty());
        let BackendQueryResult::ListTrack(saved) = running
            .backend
            .query(BackendQuery::ListTrack {
                list_id: "default".into(),
                music_id: record.id.clone(),
            })
            .unwrap()
        else {
            panic!("unexpected retained list track")
        };
        assert_eq!(saved.unwrap().id, record.id);
    }
    let overlap = indexed
        .iter()
        .find(|asset| asset.asset.track_id == other_record.id)
        .unwrap();
    assert!(overlap.authorized && !overlap.explicit_file_grant);
    assert_eq!(overlap.root_ids, vec![child.root_id.clone()]);
    let file = indexed
        .iter()
        .find(|asset| asset.asset.track_id == explicit_record.id)
        .unwrap();
    assert!(file.authorized && file.explicit_file_grant && file.root_ids.is_empty());
    let response = catalog_request(&old_url);
    assert!(
        !response.status().is_success(),
        "old recursive-only HTTP route survived the new grant epoch"
    );
    assert!(!response.bytes().unwrap().starts_with(b"RIFF"));
    assert_catalog_audio(&other_url);
    assert_catalog_audio(&explicit_url);
    running.execute("scope-clear-picture-info", BackendCommand::ClearImageCache);
    assert!(
        running
            .backend
            .execute(envelope(
                "scope-picture-denied",
                BackendCommand::ResolvePicture {
                    list_id: "default".into(),
                    music_id: nested_record.id.clone(),
                }
            ))
            .is_err()
    );
    assert!(
        running
            .backend
            .execute(envelope(
                "scope-lyric-denied",
                BackendCommand::LoadLyrics {
                    id: nested_record.id.clone(),
                }
            ))
            .is_err()
    );
    assert_play_denied(&running, "scope-new-play-denied", &nested_record.id);
    assert_eq!(
        running
            .scan(
                "scope-explicit-nonrecursive-rescan",
                &parent.root_id,
                LibraryScanOptions::default()
            )
            .state,
        LibraryOperationState::Completed
    );
    let new_members = assets(&running.backend, Some(&parent.root_id));
    assert_eq!(new_members.len(), 1);
    assert_eq!(new_members[0].asset.track_id, top_record.id);
    assert!(new_members[0].authorized && !new_members[0].explicit_file_grant);
    running.play("scope-top-after-rescan", &top_record.id, 440.0);
    running.play("scope-other-after", &other_record.id, 880.0);
    running.play("scope-explicit-after", &explicit_record.id, 660.0);
    running.backend.shutdown(Duration::from_secs(15)).unwrap();
    fs::write(
        root_only.with_extension("lrc"),
        "[00:00.000]UNAUTHORIZED NEW SIDECAR",
    )
    .unwrap();
    {
        let repository = Arc::new(Repository::open(running.profile()).unwrap());
        let service = LyricService::new(repository);
        let prepared = service
            .prepare(LyricRequest::new(&nested_record.id, 17), None)
            .unwrap();
        assert_eq!(prepared.provenance, LyricProvenance::RawCache);
        assert_eq!(
            prepared.bundle.lyric.text(),
            Some("[00:00.000]ORIGINAL RECURSIVE SIDECAR")
        );
    }
    running.restart();
    let indexed = assets(&running.backend, None);
    let nested = indexed
        .iter()
        .find(|asset| asset.asset.track_id == nested_record.id)
        .unwrap();
    assert!(!nested.authorized && !nested.explicit_file_grant && nested.root_ids.is_empty());
    assert_play_denied(&running, "scope-reopened-play-denied", &nested_record.id);
}

#[test]
fn unchanged_root_only_rescans_strengthen_missing_file_proof_without_resetting_presented_audio() {
    use lx_gpui_player::{
        audio::PlaybackStatus,
        domain::library::{LibraryOperationView, LibraryPreparedAsset, LibrarySeenFile},
        repository::Repository,
    };

    let mut running = Running::new();
    let selected = running.folder("proof-selected");
    let path = selected.join("root-only-proof.wav");
    wave(&path, 997.0, 30.0);
    let added = running.add_root("proof-add", selected, true);
    let metadata = lx_gpui_player::catalog::Catalog::new()
        .unwrap()
        .prepare_local(&path)
        .unwrap()
        .library_asset();
    let track = metadata.track;
    let mut asset = metadata.asset;
    let original_asset_id = asset.id.clone();
    running.backend.shutdown(Duration::from_secs(15)).unwrap();
    {
        let repository = Repository::open(running.profile()).unwrap();
        // Metadata restored without a physical-file proof remains root-only.
        // A known physical binding cannot be downgraded by an unproven update.
        asset.fingerprint = None;
        let (token, _) = repository
            .begin_library_scan(&added.root_id, "proof-unproven-metadata")
            .unwrap();
        repository
            .apply_library_scan_batch(
                &token,
                &[LibrarySeenFile {
                    path: asset.path.clone(),
                    asset_id: Some(asset.id.clone()),
                }],
                &[LibraryPreparedAsset {
                    track,
                    asset: asset.clone(),
                    file_identity: None,
                }],
                &|| Ok(()),
            )
            .unwrap();
        let mut terminal = LibraryOperationView::running(&token);
        terminal.state = LibraryOperationState::Completed;
        terminal.discovered_files = 1;
        terminal.prepared_files = 1;
        repository
            .finish_library_scan(&token, &terminal, true, &|| Ok(()))
            .unwrap();
        assert!(
            repository
                .local_asset_file_identity(&asset.id)
                .unwrap()
                .is_none()
        );
        assert!(
            repository
                .local_asset(&asset.id)
                .unwrap()
                .unwrap()
                .fingerprint
                .is_none()
        );
    }
    running.restart();
    fixture::wait(&running.backend, 8.0, |_| {
        assets(&running.backend, Some(&added.root_id))
            .iter()
            .any(|item| item.authorized)
    });
    let before = assets(&running.backend, Some(&added.root_id))
        .pop()
        .unwrap();
    assert!(before.authorized && !before.explicit_file_grant);
    assert!(before.asset.fingerprint.is_none());
    let playing = running.play("proof-real-play", &asset.track_id, 997.0);
    let epoch = cache_epoch(&running.backend);
    let position = playing.playback.position_seconds;
    let strengthened = running.scan(
        "proof-strengthening-rescan",
        &added.root_id,
        LibraryScanOptions::default(),
    );
    assert_eq!(
        strengthened.state,
        LibraryOperationState::Completed,
        "{strengthened:?}"
    );
    assert_eq!(strengthened.committed_files, 1);
    assert_eq!(strengthened.missing_files, 0);
    let updated = assets(&running.backend, Some(&added.root_id))
        .pop()
        .unwrap();
    assert_eq!(updated.asset.id, original_asset_id);
    assert_eq!(updated.asset.track_id, asset.track_id);
    assert!(updated.asset.fingerprint.is_some());
    assert!(updated.authorized && !updated.explicit_file_grant);
    let after = fixture::wait(&running.backend, 5.0, |snapshot| {
        snapshot.playback.position_seconds > position + 0.15
    });
    assert_eq!(after.playback.status, PlaybackStatus::Playing);
    assert_same_media(&playing, &after);
    assert_eq!(cache_epoch(&running.backend), epoch);
    assert!(
        fixture::pcm(&running.backend)
            .frequency_hz
            .is_some_and(|frequency| (frequency - 997.0).abs() < 3.0)
    );
    let unchanged = running.scan(
        "proof-already-proven-rescan",
        &added.root_id,
        LibraryScanOptions::default(),
    );
    assert_eq!(unchanged.state, LibraryOperationState::Completed);
    assert_eq!(unchanged.changed_files, 0);
    assert_same_media(&playing, &running.backend.snapshot());
    assert_eq!(cache_epoch(&running.backend), epoch);
    running.backend.shutdown(Duration::from_secs(15)).unwrap();
    let repository = Repository::open(running.profile()).unwrap();
    assert!(
        repository
            .local_asset_file_identity(&original_asset_id)
            .unwrap()
            .is_some()
    );
}

#[test]
fn adding_an_explicit_root_is_idempotent_and_does_not_scan_or_grant_its_neighbor() {
    let running = Running::new();
    let selected = running.folder("库 已选");
    let neighbor = running.folder("库 未选相邻");
    wave(&selected.join("owned.wav"), 440.0, 0.1);
    wave(&neighbor.join("not-selected.wav"), 880.0, 0.1);
    let first = running.add_root("add-selected", selected.clone(), true);
    assert!(assets(&running.backend, None).is_empty());
    assert_eq!(first.generation, 0);
    assert!(!first.watch_enabled);
    let second = running.add_root("add-selected-again", selected.join("."), true);
    assert_eq!(second.root_id, first.root_id);
    assert_eq!(second.grant_epoch, first.grant_epoch);
    let roots = match running
        .backend
        .query(BackendQuery::LibraryRoots {
            offset: 0,
            limit: 10,
        })
        .unwrap()
    {
        BackendQueryResult::LibraryRoots(page) => page,
        other => panic!("{other:?}"),
    };
    assert_eq!(roots.total, 1);
    assert_eq!(running.backend.snapshot().library.roots, 1);
    assert_eq!(running.backend.snapshot().library.authorized_assets, 0);
    let outcome = running.scan(
        "scan-selected",
        &first.root_id,
        LibraryScanOptions::default(),
    );
    assert_eq!(
        outcome.state,
        LibraryOperationState::Completed,
        "{outcome:?}"
    );
    let indexed = assets(&running.backend, Some(&first.root_id));
    assert_eq!(indexed.len(), 1);
    assert!(indexed[0].authorized);
    assert!(!indexed[0].explicit_file_grant);
    assert!(indexed[0].asset.path.ends_with("owned.wav"));
    assert_eq!(
        running.backend.tracks("not-selected", 0, 10).unwrap().total,
        0
    );
    assert!(
        ProjectStore::open(running.profile())
            .unwrap()
            .load()
            .unwrap()
            .local_files
            .is_empty()
    );
}

#[test]
fn a_401_file_scan_commits_real_multiple_batches_and_keyset_pages_without_unrelated_media_reset() {
    let running = Running::new();
    let standalone = running.folder("explicit").join("long-tone.wav");
    wave(&standalone, 997.0, 30.0);
    let (track, _) = local_track_from_path(&standalone).unwrap();
    running.import_explicit("explicit-for-scan", vec![standalone]);
    let playing = running.play("play-during-scan", &track.id, 997.0);
    let epoch = cache_epoch(&running.backend);
    let selected = running.folder("many-selected");
    let files = corpus(&selected, 401);
    let added = running.add_root("add-many", selected, true);
    let before = running.backend.snapshot().revision;
    let outcome = running.scan("scan-many", &added.root_id, LibraryScanOptions::default());
    assert_eq!(
        outcome.state,
        LibraryOperationState::Completed,
        "{outcome:?}"
    );
    assert_eq!(outcome.discovered_files, files.len());
    assert_eq!(outcome.prepared_files, files.len());
    assert_eq!(outcome.committed_files, files.len());
    assert_eq!(outcome.skipped_files, 0);
    assert_eq!(outcome.missing_files, 0);
    assert!(
        outcome.repository_revision >= before + 4,
        "401 items require multiple bounded commit transactions"
    );
    let indexed = assets(&running.backend, Some(&added.root_id));
    assert_eq!(indexed.len(), files.len());
    assert!(
        indexed
            .iter()
            .all(|item| item.authorized && !item.explicit_file_grant && item.asset.available)
    );
    let committed_root = root(&running.backend, &added.root_id).unwrap();
    assert_eq!(committed_root.generation, outcome.generation.unwrap());
    assert_eq!(
        committed_root.last_complete_generation,
        committed_root.generation
    );
    assert_eq!(running.backend.snapshot().library.running_operations, 0);
    assert_same_media(&playing, &running.backend.snapshot());
    assert_eq!(cache_epoch(&running.backend), epoch);
    let page = match running
        .backend
        .query(BackendQuery::LibraryAssets {
            root_id: Some(added.root_id.clone()),
            after_asset_id: None,
            limit: 1,
            expected_revision: None,
        })
        .unwrap()
    {
        BackendQueryResult::LibraryAssets(page) => page,
        other => panic!("{other:?}"),
    };
    running.execute(
        "unrelated-profile-revision",
        BackendCommand::Volume { volume: 0.61 },
    );
    let stale = running
        .backend
        .query(BackendQuery::LibraryAssets {
            root_id: Some(added.root_id),
            after_asset_id: page.next_asset_id,
            limit: 1,
            expected_revision: Some(page.revision),
        })
        .unwrap_err();
    assert_eq!(stale.kind, ErrorKind::Conflict);
    assert_same_media(&playing, &running.backend.snapshot());
    assert_eq!(cache_epoch(&running.backend), epoch);
}

#[test]
fn root_revocation_and_restart_do_not_escalate_membership_into_an_explicit_file_grant() {
    let mut running = Running::new();
    let selected = running.folder("selected-parent");
    let overlapping = selected.join("nested");
    fs::create_dir_all(&overlapping).unwrap();
    let only_root = selected.join("root-only.wav");
    let other_root = overlapping.join("other-root.wav");
    let explicit = selected.join("explicit.wav");
    wave(&only_root, 330.0, 20.0);
    wave(&other_root, 880.0, 20.0);
    wave(&explicit, 997.0, 20.0);
    let (only_record, _) = local_track_from_path(&only_root).unwrap();
    let (other_record, _) = local_track_from_path(&other_root).unwrap();
    let (explicit_record, _) = local_track_from_path(&explicit).unwrap();
    running.import_explicit("explicit-independent-grant", vec![explicit.clone()]);
    let parent = running.add_root("add-parent", selected, true);
    let child = running.add_root("add-child", overlapping, false);
    assert_eq!(
        running
            .scan(
                "scan-parent",
                &parent.root_id,
                LibraryScanOptions::default()
            )
            .state,
        LibraryOperationState::Completed
    );
    assert_eq!(
        running
            .scan("scan-child", &child.root_id, LibraryScanOptions::default())
            .state,
        LibraryOperationState::Completed
    );
    running.execute(
        "preserve-root-list",
        BackendCommand::ListAction {
            action: Box::new(ListAction::MusicAdd {
                list_id: "default".into(),
                tracks: vec![
                    only_record.clone(),
                    other_record.clone(),
                    explicit_record.clone(),
                ],
                location: AddLocation::Bottom,
            }),
        },
    );
    let edited = LyricBundle::from_value(
        &serde_json::json!({"lyric":"[00:00.000]Stored original user lyric"}),
    )
    .unwrap();
    running.execute(
        "preserve-root-edited-lyrics",
        BackendCommand::SaveLyrics {
            id: only_record.id.clone(),
            bundle: edited.clone(),
        },
    );
    running.execute(
        "remove-parent",
        BackendCommand::RemoveLibraryRoot {
            root_id: parent.root_id.clone(),
        },
    );
    assert!(root(&running.backend, &parent.root_id).is_none());
    running.execute("save-after-revoke", BackendCommand::Volume { volume: 0.44 });
    let saved = ProjectStore::open(running.profile())
        .unwrap()
        .load()
        .unwrap();
    let root_only_canonical = fs::canonicalize(&only_root).unwrap();
    let other_root_canonical = fs::canonicalize(&other_root).unwrap();
    assert!(saved.local_files.iter().all(|path| {
        let canonical = fs::canonicalize(path).unwrap_or_else(|_| path.clone());
        canonical != root_only_canonical && canonical != other_root_canonical
    }));
    running.restart();
    fixture::wait(&running.backend, 8.0, |_| {
        assets(&running.backend, Some(&child.root_id))
            .iter()
            .any(|item| item.authorized)
    });
    let indexed = assets(&running.backend, None);
    let only = indexed
        .iter()
        .find(|item| item.asset.track_id == only_record.id)
        .unwrap();
    assert!(!only.authorized && !only.explicit_file_grant);
    assert!(only.root_ids.is_empty());
    let overlap = indexed
        .iter()
        .find(|item| item.asset.track_id == other_record.id)
        .unwrap();
    assert!(overlap.authorized && !overlap.explicit_file_grant);
    assert_eq!(overlap.root_ids, vec![child.root_id.clone()]);
    let file = indexed
        .iter()
        .find(|item| item.asset.track_id == explicit_record.id)
        .unwrap();
    assert!(file.authorized && file.explicit_file_grant);
    assert_eq!(
        running
            .backend
            .lyrics(&only_record.id, Some(LyricOrigin::Edited))
            .unwrap(),
        Some(edited)
    );
    let membership = match running
        .backend
        .query(BackendQuery::ListTrack {
            list_id: "default".into(),
            music_id: only_record.id.clone(),
        })
        .unwrap()
    {
        BackendQueryResult::ListTrack(track) => track.unwrap(),
        other => panic!("{other:?}"),
    };
    assert_eq!(membership.id, only_record.id);
    let denied = running.backend.execute(envelope(
        "play-revoked-root",
        BackendCommand::Play {
            id: only_record.id.clone(),
        },
    ));
    if denied.is_ok() {
        fixture::wait(&running.backend, 8.0, |snapshot| !snapshot.busy);
        assert_ne!(
            running.backend.snapshot().playback.track_id.as_deref(),
            Some(only_record.id.as_str())
        );
        assert!(running.backend.snapshot().error.is_some());
    }
    running.play("play-other-root", &other_record.id, 880.0);
    running.play("play-explicit-file", &explicit_record.id, 997.0);
}

#[test]
fn invalid_roots_scan_options_and_stale_revisions_preserve_saved_library_state() {
    let running = Running::new();
    let selected = running.folder("valid-root");
    let file = selected.join("owned.wav");
    wave(&file, 440.0, 0.1);
    for (index, path) in [
        std::path::PathBuf::from("relative-library"),
        file,
        running.directory.path().join("does-not-exist"),
    ]
    .into_iter()
    .enumerate()
    {
        let id = format!("invalid-root-{index}");
        if running
            .backend
            .execute(envelope(
                &id,
                BackendCommand::AddLibraryRoot {
                    path,
                    recursive: true,
                },
            ))
            .is_ok()
        {
            let outcome = library_terminal(&running.backend, &id);
            assert_eq!(outcome.state, LibraryOperationState::Failed);
            assert!(outcome.error.is_some());
        }
        assert_eq!(running.backend.snapshot().library.roots, 0);
        assert!(assets(&running.backend, None).is_empty());
    }
    let added = running.add_root("valid-after-failures", selected, true);
    let before = root(&running.backend, &added.root_id).unwrap();
    let revision = running.backend.snapshot().revision;
    for (index, options) in [
        LibraryScanOptions {
            max_files: 0,
            ..Default::default()
        },
        LibraryScanOptions {
            max_entries: 0,
            ..Default::default()
        },
        LibraryScanOptions {
            max_depth: 65,
            ..Default::default()
        },
        LibraryScanOptions {
            timeout_ms: 600_001,
            ..Default::default()
        },
    ]
    .into_iter()
    .enumerate()
    {
        let error = running
            .backend
            .execute(envelope(
                &format!("invalid-scan-{index}"),
                BackendCommand::ScanLibraryRoot {
                    root_id: added.root_id.clone(),
                    options,
                },
            ))
            .unwrap_err();
        assert_eq!(error.kind, ErrorKind::InvalidData);
        assert_eq!(root(&running.backend, &added.root_id).unwrap(), before);
        assert_eq!(running.backend.snapshot().revision, revision);
    }
    let mut stale = envelope(
        "stale-library-root-removal",
        BackendCommand::RemoveLibraryRoot {
            root_id: added.root_id.clone(),
        },
    );
    stale.expected_revision = Some(revision.saturating_sub(1));
    assert_eq!(
        running.backend.execute(stale).unwrap_err().kind,
        ErrorKind::Conflict
    );
    assert_eq!(root(&running.backend, &added.root_id).unwrap(), before);
}

fn add_owned_picture(path: &std::path::Path) {
    use image::{ExtendedColorType, ImageEncoder};
    use lofty::{
        config::WriteOptions,
        picture::{MimeType, Picture, PictureType},
        tag::{Tag, TagExt, TagType},
    };
    let mut png = Vec::new();
    image::codecs::png::PngEncoder::new(&mut png)
        .write_image(
            &[
                20, 80, 160, 255, 240, 30, 60, 255, 0, 180, 70, 255, 220, 220, 20, 255,
            ],
            2,
            2,
            ExtendedColorType::Rgba8,
        )
        .unwrap();
    let mut tag = Tag::new(TagType::Id3v2);
    tag.push_picture(
        Picture::unchecked(png)
            .pic_type(PictureType::CoverFront)
            .mime_type(MimeType::Png)
            .build(),
    );
    tag.save_to_path(path, WriteOptions::default()).unwrap();
}

fn resolve_picture(
    running: &Running,
    request: &str,
    track_id: &str,
) -> lx_gpui_player::domain::artwork::PictureOperationView {
    use lx_gpui_player::domain::artwork::PictureState;
    running.execute(
        request,
        BackendCommand::ResolvePicture {
            list_id: "default".into(),
            music_id: track_id.into(),
        },
    );
    let mut result = None;
    fixture::wait(&running.backend, 15.0, |_| {
        let BackendQueryResult::PictureOperations(views) = running
            .backend
            .query(BackendQuery::PictureOperations {
                offset: 0,
                limit: 100,
            })
            .unwrap()
        else {
            panic!("unexpected picture operations")
        };
        result = views.into_iter().find(|view| {
            view.operation_id == request
                && matches!(
                    view.state,
                    PictureState::Ready | PictureState::Failed | PictureState::Cancelled
                )
        });
        result.is_some()
    });
    result.unwrap()
}

fn enable_catalog_url_source(running: &Running) {
    let script = r#"/**
 * @name Original library loopback authority fixture
 */
let alerted=false;
lx.on('request',async({action,info})=>{
  if(action!=='musicUrl')throw Error('unsupported fixture action');
  const url=info.musicInfo.filePath;
  if(typeof url!=='string'||!url.startsWith('http://127.0.0.1:'))throw Error('catalog DTO is not loopback');
  if(!alerted){lx.send('updateAlert',{log:'library-catalog-url='+url});alerted=true;}
  return url;
});
lx.send('inited',{sources:{local:{type:'music',actions:['musicUrl'],qualitys:[]}}});"#;
    let id = lx_gpui_player::source::script_hash(script);
    for (request, command) in [
        (
            "scope-source-import",
            BackendCommand::ImportSourceText {
                script: script.into(),
            },
        ),
        (
            "scope-source-approve",
            BackendCommand::ApproveSource { id: id.clone() },
        ),
        ("scope-source-enable", BackendCommand::EnableSource { id }),
    ] {
        let cursor = running.backend.event_cursor();
        running.execute(request, command);
        assert_eq!(
            fixture::operation_finished(&running.backend, cursor, request),
            (true, None)
        );
    }
}

fn play_with_catalog_url(
    running: &Running,
    request: &str,
    track_id: &str,
    frequency: f64,
) -> String {
    use lx_gpui_player::{backend::BackendEvent, source::SourceHostEvent};
    let cursor = running.backend.event_cursor();
    running.play(request, track_id, frequency);
    let mut url = None;
    fixture::wait(&running.backend, 10.0, |_| {
        for event in running.backend.events_after(cursor).unwrap() {
            if let BackendEvent::SourceHost {
                payload: SourceHostEvent::UpdateAlert { log, .. },
                ..
            } = event.payload
                && let Some(value) = log.strip_prefix("library-catalog-url=")
                && value.ends_with(&format!("/{track_id}.wav"))
            {
                url = Some(value.to_owned());
            }
        }
        url.is_some()
    });
    url.unwrap()
}

fn catalog_track_url(captured: &str, track_id: &str) -> String {
    let mut url = reqwest::Url::parse(captured).unwrap();
    assert_eq!(url.host_str(), Some("127.0.0.1"));
    assert_eq!(url.scheme(), "http");
    url.set_path(&format!("/{track_id}.wav"));
    url.into()
}

fn catalog_request(url: &str) -> reqwest::blocking::Response {
    let parsed = reqwest::Url::parse(url).unwrap();
    assert_eq!(parsed.host_str(), Some("127.0.0.1"));
    assert_eq!(parsed.scheme(), "http");
    reqwest::blocking::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(3))
        .build()
        .unwrap()
        .get(url)
        .header(reqwest::header::RANGE, "bytes=0-43")
        .send()
        .unwrap()
}

fn assert_catalog_audio(url: &str) {
    let response = catalog_request(url);
    assert!(response.status().is_success(), "{response:?}");
    let bytes = response.bytes().unwrap();
    assert_eq!(bytes.len(), 44);
    assert_eq!(&bytes[..4], b"RIFF");
    assert_eq!(&bytes[8..12], b"WAVE");
}

fn assert_play_denied(running: &Running, request: &str, track_id: &str) {
    use lx_gpui_player::audio::PlaybackStatus;
    if running
        .backend
        .execute(envelope(
            request,
            BackendCommand::Play {
                id: track_id.into(),
            },
        ))
        .is_ok()
    {
        let snapshot = fixture::wait(&running.backend, 8.0, |snapshot| !snapshot.busy);
        assert!(
            snapshot.error.is_some(),
            "revoked local playback was silently accepted"
        );
        assert!(
            snapshot.playback.track_id.as_deref() != Some(track_id)
                || snapshot.playback.status != PlaybackStatus::Playing
        );
    }
}

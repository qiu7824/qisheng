#![cfg(any(windows, target_os = "linux", target_os = "macos"))]

#[path = "support/library_watch_fixture.rs"]
mod fixture;

use fixture::{
    Running, assets, binding, corpus, fresh, named, options, root, set_watch, settled, watch, wave,
};
use lx_gpui_player::{
    backend::{BackendCommand, BackendQuery, BackendQueryResult},
    domain::{
        library::{LibraryOperationState, LibraryScanOptions},
        library_watch::LibraryWatchState,
        lists::{AddLocation, ListAction},
        lyric::LyricOrigin,
    },
};
use std::fs;

#[test]
fn same_object_body_rewrite_with_restored_mtime_refreshes_hash_and_encoded_audio() {
    let running = Running::new();
    let selected = running.folder("same-stamp-selected");
    let path = selected.join("same-stamp.wav");
    wave(&path, 997.0, 2.0);
    let selected_root = running.add_root("same-stamp-root", selected.clone(), true);
    set_watch(
        &running,
        "same-stamp-enable",
        &selected_root,
        true,
        options(true),
    );
    let original = named(&running.backend, "same-stamp.wav");
    let old_binding = fixture::content_evidence(&running, &original.asset.id);
    running.play("same-stamp-warm-audio", &original.asset.track_id, 997.0);
    fixture::wait(&running.backend, 5.0, |snapshot| {
        snapshot.playback.status == lx_gpui_player::audio::PlaybackStatus::Ended
    });
    fixture::stop(&running, "same-stamp-stop");
    set_watch(
        &running,
        "same-stamp-disable",
        &selected_root,
        false,
        options(true),
    );
    let modified = fs::metadata(&path).unwrap().modified().unwrap();
    wave(&path, 1321.0, 2.0);
    fs::OpenOptions::new()
        .write(true)
        .open(&path)
        .unwrap()
        .set_times(fs::FileTimes::new().set_modified(modified))
        .unwrap();
    set_watch(
        &running,
        "same-stamp-reenable",
        &selected_root,
        true,
        options(true),
    );
    let changed = named(&running.backend, "same-stamp.wav");
    let new_binding = fixture::content_evidence(&running, &changed.asset.id);
    assert_eq!(changed.asset.id, original.asset.id);
    assert_eq!(changed.asset.track_id, original.asset.track_id);
    assert_eq!(changed.asset.size_bytes, original.asset.size_bytes);
    assert_eq!(changed.asset.modified_ms, original.asset.modified_ms);
    assert_eq!(new_binding.file_identity, old_binding.file_identity);
    assert_ne!(new_binding.content_sha256, old_binding.content_sha256);
    running.play("same-stamp-play-new-body", &original.asset.track_id, 1321.0);
}

#[test]
fn actual_native_create_modify_delete_index_only_selected_scope_and_keep_unrelated_pcm() {
    let running = Running::new();
    let selected = running.folder("watch-selected");
    let neighbor = running.folder("watch-unselected-neighbor");
    let explicit = running.folder("independent-file").join("steady-997.wav");
    wave(&explicit, 997.0, 36.0);
    running.import_explicit("watch-import-independent", vec![explicit.clone()]);
    let independent = named(&running.backend, "steady-997.wav");
    let playing = running.play("watch-play-independent", &independent.asset.track_id, 997.0);
    let epoch = fixture::cache_epoch(&running.backend);
    let selected_root = running.add_root("watch-root-add", selected.clone(), true);
    set_watch(
        &running,
        "watch-enable",
        &selected_root,
        true,
        options(false),
    );
    let initial = watch(&running.backend, &selected_root.root_id).unwrap();
    let first = selected.join("first-440.wav");
    let changed = selected.join("changed-660.wav");
    let nested = selected.join("actual-new-child");
    fs::create_dir(&nested).unwrap();
    let child = nested.join("child-880.wav");
    wave(&first, 440.0, 0.1);
    wave(&changed, 660.0, 0.1);
    wave(&child, 880.0, 0.1);
    wave(&neighbor.join("unselected-550.wav"), 550.0, 0.1);
    let created = settled(&running, &selected_root, |rows| {
        [&first, &changed, &child]
            .iter()
            .all(|path| fixture::file_indexed(rows, path))
    });
    assert!(created.received_sequence > initial.received_sequence);
    assert_eq!(
        created.preference.watch_generation,
        initial.preference.watch_generation
    );
    assert!(
        running
            .backend
            .tracks("unselected-550", 0, 10)
            .unwrap()
            .items
            .is_empty()
    );
    let root_assets = assets(&running.backend, Some(&selected_root.root_id));
    assert_eq!(root_assets.len(), 3);
    assert!(
        root_assets
            .iter()
            .all(|item| item.authorized && !item.explicit_file_grant)
    );
    fixture::media_continues(&playing, &fresh(&running.backend));
    assert_eq!(fixture::cache_epoch(&running.backend), epoch);
    fixture::actual_frequency(&running.backend, 997.0);

    let old_changed = named(&running.backend, "changed-660.wav");
    wave(&changed, 1321.0, 0.3);
    settled(&running, &selected_root, |rows| {
        rows.iter().any(|item| {
            item.asset.id == old_changed.asset.id
                && item.asset.size_bytes > old_changed.asset.size_bytes
        })
    });
    assert_eq!(
        named(&running.backend, "changed-660.wav").asset.track_id,
        old_changed.asset.track_id
    );
    fixture::media_continues(&playing, &fresh(&running.backend));
    assert_eq!(fixture::cache_epoch(&running.backend), epoch);

    let deleted = named(&running.backend, "first-440.wav");
    let mut record = lx_gpui_player::repository::local_track_from_path(&first)
        .unwrap()
        .0;
    record.id = deleted.asset.track_id.clone();
    record.local_asset_id = Some(deleted.asset.id.clone());
    running.execute(
        "watch-retain-deleted-list",
        BackendCommand::ListAction {
            action: Box::new(ListAction::MusicAdd {
                list_id: "default".into(),
                tracks: vec![record],
                location: AddLocation::Bottom,
            }),
        },
    );
    running.execute(
        "watch-retain-deleted-lyric",
        BackendCommand::SaveLyrics {
            id: deleted.asset.track_id.clone(),
            bundle: serde_json::from_value(
                serde_json::json!({"lyric":"[00:00.000]Retained user data"}),
            )
            .unwrap(),
        },
    );
    fs::remove_file(&first).unwrap();
    settled(&running, &selected_root, |rows| {
        rows.iter()
            .any(|item| item.asset.id == deleted.asset.id && !item.asset.available)
    });
    let BackendQueryResult::ListTrack(Some(saved)) = running
        .backend
        .query(BackendQuery::ListTrack {
            list_id: "default".into(),
            music_id: deleted.asset.track_id.clone(),
        })
        .unwrap()
    else {
        panic!("native delete erased a user list record")
    };
    assert_eq!(saved.id, deleted.asset.track_id);
    assert_eq!(
        running
            .backend
            .lyrics(&deleted.asset.track_id, Some(LyricOrigin::Edited))
            .unwrap()
            .unwrap()
            .lyric
            .text(),
        Some("[00:00.000]Retained user data")
    );
    fixture::media_continues(&playing, &fresh(&running.backend));
    fixture::stop(&running, "watch-stop-independent");
    running.play(
        "watch-play-actual-modified-file",
        &old_changed.asset.track_id,
        1321.0,
    );
    fixture::play_denied(
        &running,
        "watch-deleted-play-denied",
        &deleted.asset.track_id,
    );
}

#[test]
fn overlapping_native_subscriptions_survive_other_watch_off_and_root_revocation() {
    let running = Running::new();
    let selected = running.folder("overlap-parent");
    let child_path = selected.join("overlap-child");
    fs::create_dir(&child_path).unwrap();
    let parent = running.add_root("overlap-add-parent", selected.clone(), true);
    let child = running.add_root("overlap-add-child", child_path.clone(), false);
    set_watch(
        &running,
        "overlap-enable-parent",
        &parent,
        true,
        options(false),
    );
    set_watch(
        &running,
        "overlap-enable-child",
        &child,
        true,
        options(false),
    );
    let parent_sequence = watch(&running.backend, &parent.root_id)
        .unwrap()
        .received_sequence;
    let child_sequence = watch(&running.backend, &child.root_id)
        .unwrap()
        .received_sequence;
    let path = child_path.join("overlap-owned.wav");
    wave(&path, 440.0, 30.0);
    settled(&running, &parent, |rows| fixture::file_indexed(rows, &path));
    settled(&running, &child, |rows| {
        rows.iter().any(|item| {
            item.asset.path.file_name() == path.file_name()
                && item.root_ids.contains(&child.root_id)
        })
    });
    let indexed = named(&running.backend, "overlap-owned.wav");
    assert_eq!(indexed.root_ids.len(), 2);
    assert!(
        indexed.root_ids.contains(&parent.root_id) && indexed.root_ids.contains(&child.root_id)
    );
    assert!(
        watch(&running.backend, &parent.root_id)
            .unwrap()
            .received_sequence
            > parent_sequence
    );
    assert!(
        watch(&running.backend, &child.root_id)
            .unwrap()
            .received_sequence
            > child_sequence
    );
    set_watch(
        &running,
        "overlap-disable-parent",
        &parent,
        false,
        options(false),
    );
    let disabled_generation = watch(&running.backend, &parent.root_id)
        .unwrap()
        .preference
        .watch_generation;
    running.execute(
        "overlap-remove-parent",
        BackendCommand::RemoveLibraryRoot {
            root_id: parent.root_id.clone(),
        },
    );
    assert!(root(&running.backend, &parent.root_id).is_none());
    wave(&path, 880.0, 31.0);
    settled(&running, &child, |rows| {
        rows.iter().any(|item| {
            item.asset.id == indexed.asset.id && item.asset.size_bytes > indexed.asset.size_bytes
        })
    });
    let retained = named(&running.backend, "overlap-owned.wav");
    assert_eq!(retained.root_ids, vec![child.root_id.clone()]);
    assert!(retained.authorized && !retained.explicit_file_grant);
    running.play(
        "overlap-read-remaining-grant",
        &indexed.asset.track_id,
        880.0,
    );
    let unselected = selected.join("parent-now-unselected.wav");
    wave(&unselected, 550.0, 0.1);
    let marker = child_path.join("second-child-event.wav");
    wave(&marker, 660.0, 0.1);
    settled(&running, &child, |rows| {
        fixture::file_indexed(rows, &marker)
    });
    assert!(
        running
            .backend
            .tracks("parent-now-unselected", 0, 10)
            .unwrap()
            .items
            .is_empty()
    );
    if let Some(view) = watch(&running.backend, &parent.root_id) {
        assert!(!view.preference.enabled);
        assert_eq!(view.state, LibraryWatchState::Disabled);
        assert!(view.preference.watch_generation >= disabled_generation);
    }
}

#[test]
fn native_events_received_during_a_real_multibatch_scan_remain_dirty_until_covered() {
    let running = Running::new();
    let selected = running.folder("gap-selected");
    let original = corpus(&selected, 401);
    let selected_root = running.add_root("gap-root-add", selected.clone(), true);
    set_watch(
        &running,
        "gap-watch-enable",
        &selected_root,
        true,
        options(false),
    );
    assert_eq!(
        assets(&running.backend, Some(&selected_root.root_id)).len(),
        original.len()
    );
    let before = watch(&running.backend, &selected_root.root_id).unwrap();
    running.execute(
        "gap-real-scan-in-flight",
        BackendCommand::ScanLibraryRoot {
            root_id: selected_root.root_id.clone(),
            options: LibraryScanOptions::default(),
        },
    );
    assert_eq!(
        fixture::library_operation(&running.backend, "gap-real-scan-in-flight")
            .unwrap()
            .state,
        LibraryOperationState::Running
    );
    let late = selected.join("created-after-scan-dispatch.wav");
    wave(&late, 997.0, 0.1);
    let late_child = selected.join("created-subtree-after-dispatch");
    fs::create_dir(&late_child).unwrap();
    let second = late_child.join("child-after-dispatch.wav");
    wave(&second, 880.0, 0.1);
    fixture::wait(&running.backend, 20.0, |_| {
        watch(&running.backend, &selected_root.root_id)
            .unwrap()
            .received_sequence
            > before.received_sequence
    });
    let received = watch(&running.backend, &selected_root.root_id)
        .unwrap()
        .received_sequence;
    let old_scan = fixture::library_terminal(&running.backend, "gap-real-scan-in-flight");
    assert!(matches!(
        old_scan.state,
        LibraryOperationState::Completed | LibraryOperationState::Partial
    ));
    let final_view = settled(&running, &selected_root, |rows| {
        fixture::file_indexed(rows, &late)
            && fixture::file_indexed(rows, &second)
            && rows
                .iter()
                .filter(|item| {
                    item.root_ids.contains(&selected_root.root_id) && item.asset.available
                })
                .count()
                == 403
    });
    assert!(final_view.covered_sequence >= received);
    assert_ne!(
        final_view.last_scan_operation_id.as_deref(),
        Some("gap-real-scan-in-flight")
    );
    assert_eq!(running.backend.snapshot().library.running_operations, 0);
    assert!(
        assets(&running.backend, Some(&selected_root.root_id))
            .iter()
            .all(|item| item.asset.available)
    );
}

#[test]
fn actual_double_rename_and_cross_root_move_do_not_follow_the_old_exact_file_grant() {
    let running = Running::new();
    let left = running.folder("rename-left-selected");
    let right = running.folder("rename-right-selected");
    let first = left.join("original-exact.wav");
    wave(&first, 997.0, 30.0);
    running.import_explicit("rename-exact-import", vec![first.clone()]);
    let old = named(&running.backend, "original-exact.wav");
    let left_root = running.add_root("rename-left-root", left.clone(), true);
    let right_root = running.add_root("rename-right-root", right.clone(), true);
    set_watch(
        &running,
        "rename-left-watch",
        &left_root,
        true,
        options(true),
    );
    set_watch(
        &running,
        "rename-right-watch",
        &right_root,
        true,
        options(true),
    );
    let old_binding = fixture::content_evidence(&running, &old.asset.id);
    let middle = left.join("intermediate.wav");
    let last = left.join("same-object-final.wav");
    fs::rename(&first, &middle).unwrap();
    fs::rename(&middle, &last).unwrap();
    settled(&running, &left_root, |rows| {
        rows.iter()
            .any(|item| item.asset.id == old.asset.id && item.asset.path == last)
    });
    let rebound = binding(&running.backend, &old.asset.id);
    assert_eq!(rebound.file_identity, old_binding.file_identity);
    assert!(rebound.binding_revision > old_binding.binding_revision);
    let within = named(&running.backend, "same-object-final.wav");
    assert_eq!(within.asset.track_id, old.asset.track_id);
    assert!(within.authorized && !within.explicit_file_grant);
    let cross = right.join("cross-root-without-common-ancestor.wav");
    fs::rename(&last, &cross).unwrap();
    settled(&running, &right_root, |rows| {
        fixture::file_indexed(rows, &cross)
    });
    settled(&running, &left_root, |rows| {
        rows.iter()
            .any(|item| item.asset.id == old.asset.id && !item.asset.available)
    });
    assert_ne!(binding(&running.backend, &old.asset.id).path, cross);
    let independently_indexed = named(&running.backend, "cross-root-without-common-ancestor.wav");
    assert_ne!(independently_indexed.asset.id, old.asset.id);
    assert_ne!(independently_indexed.asset.track_id, old.asset.track_id);
    assert!(independently_indexed.authorized && !independently_indexed.explicit_file_grant);
    running.execute(
        "cross-remove-new-root",
        BackendCommand::RemoveLibraryRoot {
            root_id: right_root.root_id,
        },
    );
    fixture::play_denied(
        &running,
        "cross-no-transferred-exact-grant",
        &independently_indexed.asset.track_id,
    );
}

#[test]
fn actual_exact_sidecar_event_refreshes_the_same_audio_without_granting_neighbor_sidecars() {
    let running = Running::new();
    let selected = running.folder("sidecar-watch-selected");
    let neighbor = running.folder("sidecar-watch-unselected");
    let audio = selected.join("sidecar-owned-997.wav");
    wave(&audio, 997.0, 36.0);
    fs::write(
        audio.with_extension("lrc"),
        "[00:00.000]Original watched sidecar",
    )
    .unwrap();
    let selected_root = running.add_root("sidecar-watch-root", selected, true);
    set_watch(
        &running,
        "sidecar-watch-enable",
        &selected_root,
        true,
        options(false),
    );
    let indexed = named(&running.backend, "sidecar-owned-997.wav");
    let playing = running.play("sidecar-watch-play", &indexed.asset.track_id, 997.0);
    fixture::wait(&running.backend, 12.0, |state| {
        state.lyrics.as_ref().is_some_and(|lyrics| {
            lyrics.bundle.lyric.text() == Some("[00:00.000]Original watched sidecar")
        })
    });
    let before = fresh(&running.backend);
    let epoch = fixture::cache_epoch(&running.backend);
    let sequence = watch(&running.backend, &selected_root.root_id)
        .unwrap()
        .received_sequence;
    fs::write(
        audio.with_extension("lrc"),
        "[00:00.000]Actual changed exact sidecar",
    )
    .unwrap();
    fs::write(
        neighbor.join("private-neighbor.lrc"),
        "[00:00.000]Not selected",
    )
    .unwrap();
    fs::write(
        audio.parent().unwrap().join("orphan-without-audio.lrc"),
        "[00:00.000]No audio grant",
    )
    .unwrap();
    fixture::wait(&running.backend, 20.0, |state| {
        watch(&running.backend, &selected_root.root_id)
            .unwrap()
            .received_sequence
            > sequence
            && state.lyrics.as_ref().is_some_and(|lyrics| {
                lyrics.bundle.lyric.text() == Some("[00:00.000]Actual changed exact sidecar")
            })
    });
    settled(&running, &selected_root, |rows| {
        rows.iter()
            .filter(|item| item.root_ids.contains(&selected_root.root_id))
            .count()
            == 1
    });
    fixture::media_continues(&before, &fresh(&running.backend));
    assert_eq!(
        fresh(&running.backend).playback.generation,
        playing.playback.generation
    );
    assert_eq!(fixture::cache_epoch(&running.backend), epoch);
    fixture::actual_frequency(&running.backend, 997.0);
    assert!(
        running
            .backend
            .tracks("private-neighbor", 0, 10)
            .unwrap()
            .items
            .is_empty()
    );
    assert!(
        running
            .backend
            .tracks("orphan-without-audio", 0, 10)
            .unwrap()
            .items
            .is_empty()
    );
}

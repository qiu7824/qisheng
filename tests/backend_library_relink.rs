#![cfg(any(windows, target_os = "linux", target_os = "macos"))]

#[path = "support/library_watch_fixture.rs"]
mod fixture;

use fixture::{
    Running, assets, binding, content_evidence, fresh, named, options, relink, root, root_binding,
    set_watch, settled, wave,
};
use lx_gpui_player::{
    audio::PlaybackStatus,
    backend::{BackendCommand, BackendEvent, BackendQuery, BackendQueryResult, ErrorKind},
    domain::{
        library::{LibraryAssetView, LibraryScanOptions},
        library_watch::RootMovePolicy,
        lists::{AddLocation, ListAction},
        lyric::LyricOrigin,
        music::{LxMusicDtoVersion, TrackRecord},
        playback_processing::PlaybackProcessingSettings,
    },
    playback::queue::PlayMode,
};
use std::{fs, path::Path, thread, time::Duration};

fn user_record(
    running: &Running,
    item: &LibraryAssetView,
    path: &Path,
    request: &str,
) -> TrackRecord {
    let mut parsed = TrackRecord::from_lx_dto(serde_json::json!({
        "id":format!("wire:{}",item.asset.id), "name":"Owned local wire record", "singer":null,
        "source":"local", "interval":"00:36", "unknownTop":null,
        "meta":{"filePath":path,"songId":path,"albumName":"Owned fixture","unknownMeta":{"retained":true}},
    }), LxMusicDtoVersion::Modern).unwrap();
    parsed.id = item.asset.track_id.clone();
    parsed.local_asset_id = Some(item.asset.id.clone());
    running.execute(
        request,
        BackendCommand::ListAction {
            action: Box::new(ListAction::MusicAdd {
                list_id: "default".into(),
                tracks: vec![parsed],
                location: AddLocation::Bottom,
            }),
        },
    );
    list_record(running, "default", &item.asset.track_id)
}

fn list_record(running: &Running, list_id: &str, music_id: &str) -> TrackRecord {
    match running
        .backend
        .query(BackendQuery::ListTrack {
            list_id: list_id.into(),
            music_id: music_id.into(),
        })
        .unwrap()
    {
        BackendQueryResult::ListTrack(Some(record)) => *record,
        other => panic!("persistent user record missing: {other:?}"),
    }
}

#[test]
fn actual_same_root_rename_preserves_current_pcm_dsp_and_ready_preload_then_promotes_the_marker() {
    let running = Running::new();
    let selected = running.folder("continuous-root");
    let current = selected.join("current-997.wav");
    let next = selected.join("next-1733.wav");
    wave(&current, 997.0, 36.0);
    wave(&next, 1733.0, 24.0);
    let selected_root = running.add_root("continuous-root-add", selected.clone(), true);
    set_watch(
        &running,
        "continuous-watch-enable",
        &selected_root,
        true,
        options(true),
    );
    let current_item = named(&running.backend, "current-997.wav");
    let next_item = named(&running.backend, "next-1733.wav");
    let current_hash = content_evidence(&running, &current_item.asset.id);
    let next_hash = content_evidence(&running, &next_item.asset.id);
    let current_record = user_record(
        &running,
        &current_item,
        &current,
        "continuous-current-record",
    );
    let next_record = user_record(&running, &next_item, &next, "continuous-next-record");
    running.execute(
        "continuous-list-order",
        BackendCommand::ListAction {
            action: Box::new(ListAction::MusicOverwrite {
                list_id: "default".into(),
                tracks: vec![current_record.clone(), next_record.clone()],
            }),
        },
    );
    running.execute(
        "continuous-list-mode",
        BackendCommand::SetPlayMode {
            mode: PlayMode::List,
        },
    );
    running.execute(
        "continuous-play",
        BackendCommand::PlayListTrack {
            list_id: "default".into(),
            music_id: current_record.id.clone(),
            quality: None,
        },
    );
    fixture::wait(&running.backend, 12.0, |state| {
        state.playback.status == PlaybackStatus::Playing
    });
    let wanted = PlaybackProcessingSettings {
        playback_rate: 1.25,
        preserves_pitch: true,
        pitch_ratio: 1.25,
    };
    running.execute(
        "continuous-processing",
        BackendCommand::SetPlaybackProcessing { settings: wanted },
    );
    let mut eq = lx_gpui_player::domain::equalizer::EqualizerSettings::default();
    eq.gains_db[5] = 3.0;
    running.execute(
        "continuous-eq",
        BackendCommand::SetEqualizer { settings: eq },
    );
    fixture::wait(&running.backend, 15.0, |state| {
        state.playback_processing.applied == wanted
            && state.playback_processing.applied_sequence
                == state.playback_processing.requested_sequence
            && state.playback_processing.transition_remaining_frames == 0
            && state.equalizer.applied == eq
            && state.equalizer.applied_sequence == state.equalizer.requested_sequence
    });
    fixture::actual_frequency(&running.backend, 997.0 * wanted.pitch_ratio);
    running.execute(
        "continuous-enter-preload-window",
        BackendCommand::Seek { seconds: 27.0 },
    );
    let ready = fixture::wait(&running.backend, 15.0, |state| {
        state
            .preload
            .as_ref()
            .is_some_and(|preload| preload.state == "ready")
    });
    let prepared = ready.preload.unwrap();
    assert_eq!(prepared.candidate.track_id, next_item.asset.track_id);
    let promotion_cursor = running.backend.event_cursor();
    let before = fresh(&running.backend);
    let epoch = fixture::cache_epoch(&running.backend);
    let moved_current = selected.join("same-object-current-moved.wav");
    let moved_next = selected.join("same-object-next-moved.wav");
    fs::rename(&current, &moved_current).unwrap();
    fs::rename(&next, &moved_next).unwrap();
    settled(&running, &selected_root, |rows| {
        rows.iter()
            .any(|item| item.asset.id == current_item.asset.id && item.asset.path == moved_current)
            && rows
                .iter()
                .any(|item| item.asset.id == next_item.asset.id && item.asset.path == moved_next)
    });
    let rebound = binding(&running.backend, &current_item.asset.id);
    assert!(rebound.binding_revision > current_hash.binding_revision);
    assert_eq!(rebound.file_identity, current_hash.file_identity);
    assert_eq!(rebound.content_sha256, current_hash.content_sha256);
    assert_eq!(
        binding(&running.backend, &next_item.asset.id).content_sha256,
        next_hash.content_sha256
    );
    let after = fresh(&running.backend);
    fixture::media_continues(&before, &after);
    assert!(after.playback.position_seconds > before.playback.position_seconds);
    assert_eq!(after.playback_processing.applied, wanted);
    assert_eq!(after.equalizer.applied, eq);
    assert_eq!(fixture::cache_epoch(&running.backend), epoch);
    let still_prepared = after.preload.unwrap();
    assert_eq!(still_prepared.operation_id, prepared.operation_id);
    assert_eq!(still_prepared.candidate, prepared.candidate);
    assert_eq!(still_prepared.state, "ready", "{still_prepared:?}");
    assert_eq!(
        serde_json::to_value(&still_prepared.identity).unwrap(),
        serde_json::to_value(&prepared.identity).unwrap()
    );
    fixture::actual_frequency(&running.backend, 997.0 * wanted.pitch_ratio);
    running.execute("continuous-mute", BackendCommand::SetMute { muted: true });
    let position = fresh(&running.backend).playback.position_seconds;
    fixture::wait(&running.backend, 8.0, |state| {
        state.playback.position_seconds > position + 0.1
            && fixture::pcm(&running.backend).peak == 0.0
    });
    running.execute(
        "continuous-unmute",
        BackendCommand::SetMute { muted: false },
    );
    fixture::actual_frequency(&running.backend, 997.0 * wanted.pitch_ratio);
    running.execute("continuous-pause", BackendCommand::Pause);
    let paused = fixture::wait(&running.backend, 8.0, |state| {
        state.playback.status == PlaybackStatus::Paused
    });
    thread::sleep(Duration::from_millis(120));
    assert!(
        (fresh(&running.backend).playback.position_seconds - paused.playback.position_seconds)
            .abs()
            < 0.02
    );
    running.execute("continuous-resume", BackendCommand::Resume);
    let marker = fixture::wait(&running.backend, 15.0, |state| {
        state.playback.track_id.as_deref() == Some(next_item.asset.track_id.as_str())
            && state.playback.status == PlaybackStatus::Playing
    });
    assert!(marker.playback.generation > before.playback.generation);
    fixture::actual_frequency(&running.backend, 1733.0 * wanted.pitch_ratio);
    fixture::wait(&running.backend, 5.0, |_| {
        running.backend.events_after(promotion_cursor).unwrap().iter().any(|event| matches!(&event.payload,
        BackendEvent::PreloadPromoted { operation_id, track_id, .. } if operation_id == &prepared.operation_id && track_id == &next_item.asset.track_id))
    });
    assert_eq!(
        marker.approved_audio_identity.as_ref().unwrap().url_sha256,
        prepared.identity.as_ref().unwrap().url_sha256
    );
    assert_eq!(fixture::cache_epoch(&running.backend), epoch);
    let moved_record = list_record(&running, "default", &current_record.id);
    assert_eq!(moved_record.id, current_record.id);
    assert_eq!(moved_record.lx_id, current_record.lx_id);
    assert_eq!(
        moved_record.metadata["filePath"],
        serde_json::json!(moved_current)
    );
}

#[test]
fn manual_same_content_relink_keeps_the_live_catalog_route_and_does_not_regrant_reused_old_json_path()
 {
    let mut running = Running::new();
    let selected = running.folder("manual-original-root");
    let original = selected.join("original-selected.wav");
    let chosen = running
        .folder("manual-chosen-exact-file")
        .join("chosen-copy.wav");
    wave(&original, 997.0, 36.0);
    running.import_explicit("manual-old-exact-grant", vec![original.clone()]);
    let old = named(&running.backend, "original-selected.wav");
    let selected_root = running.add_root("manual-root-add", selected.clone(), true);
    set_watch(
        &running,
        "manual-watch-hash",
        &selected_root,
        true,
        options(true),
    );
    let old_binding = content_evidence(&running, &old.asset.id);
    set_watch(
        &running,
        "manual-watch-off-before-selection",
        &selected_root,
        false,
        options(true),
    );
    let record = user_record(&running, &old, &original, "manual-wire-record");
    let edited = serde_json::from_value(
        serde_json::json!({"lyric":"[00:00.000]Persistent manually edited lyric"}),
    )
    .unwrap();
    running.execute(
        "manual-save-lyric",
        BackendCommand::SaveLyrics {
            id: record.id.clone(),
            bundle: edited,
        },
    );
    let old_url = fixture::capture_native_url(&running, "manual-url-play", &record.id, 997.0);
    let before = fresh(&running.backend);
    let epoch = fixture::cache_epoch(&running.backend);
    fixture::http_wave(&old_url, true);
    fs::copy(&original, &chosen).unwrap();
    fixture::same_contents(&original, &chosen);
    let result = relink(
        &running,
        "manual-select-identical-copy",
        &old.asset.id,
        chosen.clone(),
        old_binding.binding_revision,
        false,
    );
    assert!(!result.media_changed && result.explicit_grant_renewed);
    assert_eq!(result.asset_id, old.asset.id);
    assert_eq!(result.track_id, old.asset.track_id);
    assert_eq!(result.new_path, chosen);
    assert!(
        original.is_file() && chosen.is_file(),
        "relink moved or removed a physical file"
    );
    fixture::media_continues(&before, &fresh(&running.backend));
    assert_eq!(fixture::cache_epoch(&running.backend), epoch);
    fixture::actual_frequency(&running.backend, 997.0);
    fixture::http_wave(&old_url, true);
    let current = list_record(&running, "default", &record.id);
    assert_eq!(current.id, record.id);
    assert_eq!(current.lx_id, record.lx_id);
    for version in [LxMusicDtoVersion::Modern, LxMusicDtoVersion::Legacy] {
        let dto = current.to_lx_dto(version);
        let projected = match version {
            LxMusicDtoVersion::Modern => &dto["meta"]["filePath"],
            LxMusicDtoVersion::Legacy => &dto["filePath"],
        };
        assert_eq!(projected, &serde_json::json!(chosen));
        assert_eq!(dto["unknownTop"], serde_json::Value::Null);
    }
    let copy = TrackRecord::from_lx_dto(
        current.to_lx_dto(LxMusicDtoVersion::Modern),
        LxMusicDtoVersion::Modern,
    )
    .unwrap();
    running.execute(
        "manual-copy-current-lx-dto",
        BackendCommand::ListAction {
            action: Box::new(ListAction::MusicAdd {
                list_id: "love".into(),
                tracks: vec![copy],
                location: AddLocation::Bottom,
            }),
        },
    );
    assert_eq!(list_record(&running, "love", &record.lx_id).id, record.id);
    let archived = running
        .folder("unselected-archive")
        .join("physical-old-original.wav");
    fs::rename(&original, &archived).unwrap();
    wave(&original, 880.0, 36.0);
    running.scan(
        "manual-explicit-rescan-reused-path",
        &selected_root.root_id,
        LibraryScanOptions::default(),
    );
    let reused = named(&running.backend, "original-selected.wav");
    assert_ne!(reused.asset.id, old.asset.id);
    assert_ne!(reused.asset.track_id, old.asset.track_id);
    assert!(!reused.explicit_file_grant);
    assert_eq!(binding(&running.backend, &old.asset.id).path, chosen);
    running.execute(
        "manual-revoke-old-root",
        BackendCommand::RemoveLibraryRoot {
            root_id: selected_root.root_id,
        },
    );
    running.execute(
        "manual-save-json-current-grant",
        BackendCommand::Volume { volume: 0.37 },
    );
    running.restart();
    let moved = named(&running.backend, "chosen-copy.wav");
    assert_eq!(moved.asset.id, old.asset.id);
    assert!(moved.authorized && moved.explicit_file_grant);
    let unselected_reuse = named(&running.backend, "original-selected.wav");
    assert!(!unselected_reuse.authorized && !unselected_reuse.explicit_file_grant);
    assert_eq!(list_record(&running, "love", &record.lx_id).id, record.id);
    assert_eq!(
        running
            .backend
            .lyrics(&record.id, Some(LyricOrigin::Edited))
            .unwrap()
            .unwrap()
            .lyric
            .text(),
        Some("[00:00.000]Persistent manually edited lyric")
    );
    fixture::play_denied(
        &running,
        "manual-reused-old-json-path-denied",
        &reused.asset.track_id,
    );
    running.play(
        "manual-current-exact-selection-after-restart",
        &record.id,
        997.0,
    );
}

#[test]
fn binding_cas_collision_and_known_different_content_fail_before_live_router_mutation() {
    let running = Running::new();
    let selected = running.folder("relink-rollback-root");
    let original = selected.join("rollback-original-997.wav");
    let occupied = selected.join("occupied-other-1321.wav");
    let changed = running
        .folder("manual-different-content")
        .join("new-880.wav");
    wave(&original, 997.0, 36.0);
    wave(&occupied, 1321.0, 36.0);
    wave(&changed, 880.0, 36.0);
    let selected_root = running.add_root("rollback-root-add", selected, true);
    set_watch(
        &running,
        "rollback-enable-hash",
        &selected_root,
        true,
        options(true),
    );
    let old = named(&running.backend, "rollback-original-997.wav");
    let known = content_evidence(&running, &old.asset.id);
    set_watch(
        &running,
        "rollback-watch-off",
        &selected_root,
        false,
        options(true),
    );
    let old_url =
        fixture::capture_native_url(&running, "rollback-url-play", &old.asset.track_id, 997.0);
    let before = fresh(&running.backend);
    let epoch = fixture::cache_epoch(&running.backend);
    assert_eq!(
        fixture::relink_denied(
            &running,
            "rollback-stale-binding",
            &old.asset.id,
            changed.clone(),
            known.binding_revision + 1
        ),
        ErrorKind::Conflict
    );
    assert_eq!(
        fixture::relink_denied(
            &running,
            "rollback-occupied-binding",
            &old.asset.id,
            occupied.clone(),
            known.binding_revision
        ),
        ErrorKind::Conflict
    );
    let _ = fixture::relink_denied(
        &running,
        "rollback-known-hash-mismatch",
        &old.asset.id,
        changed.clone(),
        known.binding_revision,
    );
    assert_eq!(binding(&running.backend, &old.asset.id), known);
    assert!(original.is_file() && occupied.is_file() && changed.is_file());
    fixture::media_continues(&before, &fresh(&running.backend));
    fixture::actual_frequency(&running.backend, 997.0);
    fixture::http_wave(&old_url, true);
    assert_eq!(fixture::cache_epoch(&running.backend), epoch);
    let changed_result = relink(
        &running,
        "rollback-allow-new-representation",
        &old.asset.id,
        changed.clone(),
        known.binding_revision,
        true,
    );
    assert!(changed_result.media_changed && changed_result.explicit_grant_renewed);
    assert_eq!(changed_result.track_id, old.asset.track_id);
    assert_eq!(changed_result.asset_id, old.asset.id);
    assert_eq!(binding(&running.backend, &old.asset.id).path, changed);
    running.play("rollback-hear-actual-new-880", &old.asset.track_id, 880.0);
    let new_state = fresh(&running.backend);
    assert!(new_state.playback.generation > before.playback.generation);
    assert_ne!(
        serde_json::to_value(&new_state.approved_audio_identity).unwrap(),
        serde_json::to_value(&before.approved_audio_identity).unwrap()
    );
    assert!(original.is_file() && occupied.is_file());
}

#[test]
fn default_root_move_pauses_and_explicit_rebind_keeps_the_original_anchor_not_a_replacement() {
    let running = Running::new();
    let selected = running.folder("root-default-pause");
    let file = selected.join("original-anchor-997.wav");
    wave(&file, 997.0, 36.0);
    let selected_root = running.add_root("root-pause-add", selected.clone(), true);
    set_watch(
        &running,
        "root-pause-watch",
        &selected_root,
        true,
        options(true),
    );
    let indexed = named(&running.backend, "original-anchor-997.wav");
    let old_location = root_binding(&running.backend, &selected_root.root_id);
    let old_url =
        fixture::capture_native_url(&running, "root-pause-url", &indexed.asset.track_id, 997.0);
    let moved = running
        .directory
        .path()
        .join("actual-same-root-object-moved");
    fs::rename(&selected, &moved).unwrap();
    fs::create_dir(&selected).unwrap();
    wave(
        &selected.join("replacement-anchor-unselected-880.wav"),
        880.0,
        36.0,
    );
    fixture::wait(&running.backend, 15.0, |state| {
        state.playback.status != PlaybackStatus::Playing && !state.busy
    });
    assert_eq!(
        root(&running.backend, &selected_root.root_id).unwrap().path,
        selected
    );
    fixture::http_wave(&old_url, false);
    assert!(
        running
            .backend
            .tracks("replacement-anchor-unselected", 0, 10)
            .unwrap()
            .items
            .is_empty()
    );
    let cursor = running.backend.event_cursor();
    let receipt = running
        .backend
        .execute(fixture::envelope(
            "root-explicit-same-object-rebind",
            BackendCommand::RebindLibraryRoot {
                root_id: selected_root.root_id.clone(),
                path: moved.clone(),
                expected_grant_epoch: selected_root.grant_epoch,
                expected_location_revision: old_location.location_revision,
            },
        ))
        .unwrap();
    if receipt.state == lx_gpui_player::backend::ReceiptState::Accepted {
        assert_eq!(
            fixture::operation_finished(
                &running.backend,
                cursor,
                "root-explicit-same-object-rebind"
            ),
            (true, None)
        );
    }
    let rebound = root(&running.backend, &selected_root.root_id).unwrap();
    assert_eq!(rebound.root_id, selected_root.root_id);
    assert_eq!(rebound.anchor, selected_root.anchor);
    assert_eq!(rebound.grant_epoch, selected_root.grant_epoch);
    assert_eq!(rebound.path, moved);
    assert!(
        root_binding(&running.backend, &rebound.root_id).location_revision
            > old_location.location_revision
    );
    running.play(
        "root-rebound-original-object",
        &indexed.asset.track_id,
        997.0,
    );
    let replacement = running.add_root("root-explicitly-select-reused-old-path", selected, true);
    assert_ne!(replacement.root_id, selected_root.root_id);
    assert_ne!(replacement.anchor, selected_root.anchor);
}

#[test]
fn explicit_follow_same_root_object_updates_location_without_expanding_its_permission_scope() {
    let running = Running::new();
    let selected = running.folder("follow-original-root");
    let file = selected.join("followed-997.wav");
    wave(&file, 997.0, 36.0);
    let selected_root = running.add_root("follow-root-add", selected.clone(), true);
    let mut follow = options(true);
    follow.root_move_policy = RootMovePolicy::FollowSameObject;
    set_watch(
        &running,
        "follow-policy-enable",
        &selected_root,
        true,
        follow,
    );
    let indexed = named(&running.backend, "followed-997.wav");
    content_evidence(&running, &indexed.asset.id);
    let location = root_binding(&running.backend, &selected_root.root_id);
    let old_url = fixture::capture_native_url(
        &running,
        "follow-old-url-play",
        &indexed.asset.track_id,
        997.0,
    );
    let before = fresh(&running.backend);
    let epoch = fixture::cache_epoch(&running.backend);
    let moved = running
        .directory
        .path()
        .join("followed-same-object-new-location");
    fs::rename(&selected, &moved).unwrap();
    fs::create_dir(&selected).unwrap();
    wave(
        &selected.join("old-name-new-anchor-unselected.wav"),
        880.0,
        0.1,
    );
    fixture::wait(&running.backend, 20.0, |_| {
        root_binding(&running.backend, &selected_root.root_id).path == moved
    });
    let current = root(&running.backend, &selected_root.root_id).unwrap();
    assert_eq!(current.anchor, selected_root.anchor);
    assert_eq!(current.grant_epoch, selected_root.grant_epoch);
    assert_eq!(current.recursive, selected_root.recursive);
    assert!(
        root_binding(&running.backend, &current.root_id).location_revision
            > location.location_revision
    );
    assert_eq!(
        binding(&running.backend, &indexed.asset.id).path,
        moved.join("followed-997.wav")
    );
    fixture::media_continues(&before, &fresh(&running.backend));
    fixture::actual_frequency(&running.backend, 997.0);
    fixture::http_wave(&old_url, true);
    assert_eq!(fixture::cache_epoch(&running.backend), epoch);
    assert!(
        running
            .backend
            .tracks("old-name-new-anchor-unselected", 0, 10)
            .unwrap()
            .items
            .is_empty()
    );
    assert_eq!(assets(&running.backend, Some(&current.root_id)).len(), 1);
}

#[test]
fn real_hardlink_ambiguity_requires_selection_and_candidate_confirmation_reopens_the_current_object()
 {
    let running = Running::new();
    let selected = running.folder("candidate-ambiguity-selected");
    let original = selected.join("candidate-original.wav");
    wave(&original, 997.0, 36.0);
    let selected_root = running.add_root("candidate-root-add", selected.clone(), true);
    set_watch(
        &running,
        "candidate-hash-before-rename",
        &selected_root,
        true,
        options(true),
    );
    let old = named(&running.backend, "candidate-original.wav");
    let known = content_evidence(&running, &old.asset.id);
    set_watch(
        &running,
        "candidate-watch-off-for-hardlinks",
        &selected_root,
        false,
        options(true),
    );
    let first = selected.join("same-object-link-one.wav");
    let second = selected.join("same-object-link-two.wav");
    fs::rename(&original, &first).unwrap();
    fs::hard_link(&first, &second).unwrap();
    fixture::same_contents(&first, &second);
    set_watch(
        &running,
        "candidate-baseline-real-ambiguous-links",
        &selected_root,
        true,
        options(true),
    );
    assert_eq!(binding(&running.backend, &old.asset.id).path, known.path);
    assert_eq!(
        binding(&running.backend, &old.asset.id).binding_revision,
        known.binding_revision
    );
    let mut candidates = Vec::new();
    fixture::wait(&running.backend, 30.0, |_| {
        candidates = match running
            .backend
            .query(BackendQuery::LibraryRelinkCandidates {
                root_id: Some(selected_root.root_id.clone()),
                after_candidate_id: None,
                limit: 64,
            })
            .unwrap()
        {
            BackendQueryResult::LibraryRelinkCandidates(page) => page
                .items
                .into_iter()
                .filter(|candidate| candidate.asset_id == old.asset.id)
                .collect(),
            other => panic!("unexpected actual candidate page: {other:?}"),
        };
        candidates
            .iter()
            .any(|candidate| candidate.new_path == first)
            && candidates
                .iter()
                .any(|candidate| candidate.new_path == second)
    });
    assert!(
        candidates
            .iter()
            .all(|candidate| candidate.requires_selection)
    );
    let selected_candidate = candidates
        .into_iter()
        .find(|candidate| candidate.new_path == first)
        .unwrap();
    assert_eq!(selected_candidate.binding_revision, known.binding_revision);
    let moved_again = selected.join("old-candidate-object-moved-again.wav");
    fs::rename(&first, &moved_again).unwrap();
    wave(&first, 880.0, 36.0);
    let cursor = running.backend.event_cursor();
    let result = running.backend.execute(fixture::envelope(
        "candidate-current-object-recheck",
        BackendCommand::ConfirmLibraryRelink {
            candidate_id: selected_candidate.candidate_id,
            expected_binding_revision: known.binding_revision,
            allow_content_change: false,
        },
    ));
    if let Ok(receipt) = result {
        assert_eq!(
            receipt.state,
            lx_gpui_player::backend::ReceiptState::Accepted
        );
        assert!(
            !fixture::operation_finished(
                &running.backend,
                cursor,
                "candidate-current-object-recheck"
            )
            .0
        );
    }
    assert_eq!(binding(&running.backend, &old.asset.id).path, known.path);
    assert_eq!(
        binding(&running.backend, &old.asset.id).binding_revision,
        known.binding_revision
    );
    assert!(first.is_file() && second.is_file() && moved_again.is_file());
}

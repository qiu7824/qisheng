use lx_gpui_player::playback::queue::*;
fn queue(mode: PlayMode) -> QueueState {
    let mut queue = QueueState::new(7);
    queue.set_list("owned".into(), vec!["a".into(), "b".into(), "c".into()]);
    queue.set_mode(mode);
    queue.select_track("a").unwrap();
    queue
}
#[test]
fn manual_and_automatic_modes_have_distinct_end_rules() {
    for mode in [PlayMode::List, PlayMode::None, PlayMode::SingleLoop] {
        let mut q = queue(mode);
        let automatic = q.preview_next(AdvanceReason::Ended, |_| true);
        assert_eq!(
            automatic.map(|e| e.track_id),
            match mode {
                PlayMode::List => Some("b".into()),
                PlayMode::SingleLoop => Some("a".into()),
                _ => None,
            }
        );
        assert_eq!(
            q.advance(AdvanceReason::Manual, |_| true).unwrap().track_id,
            "b"
        );
    }
    let mut q = queue(PlayMode::List);
    q.select_track("c").unwrap();
    assert!(q.advance(AdvanceReason::Ended, |_| true).is_none());
    assert_eq!(
        q.advance(AdvanceReason::Manual, |_| true).unwrap().track_id,
        "a"
    );
}
#[test]
fn previews_do_not_consume_temporary_entries_and_restore_origin_position() {
    let mut q = queue(PlayMode::None);
    q.enqueue(vec!["x".into(), "y".into()], false);
    let first = q.preview_next(AdvanceReason::Ended, |_| true).unwrap();
    for _ in 0..4 {
        assert_eq!(
            q.preview_next(AdvanceReason::Ended, |_| true),
            Some(first.clone())
        );
        assert_eq!(q.temporary.len(), 2);
    }
    assert_eq!(
        q.advance(AdvanceReason::Ended, |_| true).unwrap().track_id,
        "x"
    );
    assert_eq!(
        q.advance(AdvanceReason::Ended, |_| true).unwrap().track_id,
        "y"
    );
    assert!(q.advance(AdvanceReason::Ended, |_| true).is_none());
    assert_eq!(
        q.advance(AdvanceReason::Manual, |_| true).unwrap().track_id,
        "b"
    );
}
#[test]
fn random_preview_is_stable_avoids_repeats_then_resets_finite_history() {
    let mut q = queue(PlayMode::Random);
    let first = q.preview_next(AdvanceReason::Ended, |_| true).unwrap();
    for _ in 0..5 {
        assert_eq!(
            q.preview_next(AdvanceReason::Ended, |_| true),
            Some(first.clone())
        );
    }
    assert_eq!(q.advance(AdvanceReason::Ended, |_| true), Some(first));
    let next = q.advance(AdvanceReason::Ended, |_| true).unwrap();
    assert_ne!(next.track_id, "a");
    let again = q.advance(AdvanceReason::Ended, |_| true).unwrap();
    assert_ne!(again.track_id, next.track_id);
}
#[test]
fn duplicate_tracks_keep_separate_ids_and_survive_order_reconciliation() {
    let mut q = QueueState::new(9);
    q.set_list(
        "d".into(),
        vec!["same".into(), "same".into(), "other".into()],
    );
    let ids: Vec<_> = q.entries.iter().map(|e| e.entry_id.clone()).collect();
    assert_ne!(ids[0], ids[1]);
    q.select(&ids[1]).unwrap();
    q.set_list(
        "d".into(),
        vec!["other".into(), "same".into(), "same".into()],
    );
    assert_eq!(q.entries[2].entry_id, ids[1]);
    assert_eq!(q.current.as_ref().unwrap().entry_id, ids[1]);
}
#[test]
fn previous_replays_the_same_future_and_deleted_entries_are_skipped() {
    let mut q = queue(PlayMode::Random);
    let b = q.advance(AdvanceReason::Manual, |_| true).unwrap();
    assert_eq!(q.previous(|_| true).unwrap().track_id, "a");
    assert_eq!(q.advance(AdvanceReason::Manual, |_| true), Some(b.clone()));
    q.remove(&b.entry_id);
    assert_ne!(
        q.advance(AdvanceReason::Manual, |_| true).unwrap().entry_id,
        b.entry_id
    );
}
#[test]
fn empty_disliked_and_invalid_lists_terminate_without_retry_loops() {
    let mut q = queue(PlayMode::Random);
    assert!(q.preview_next(AdvanceReason::Ended, |_| false).is_none());
    assert!(q.advance(AdvanceReason::Failed, |_| false).is_none());
    q.set_list("empty".into(), Vec::new());
    assert!(q.previous(|_| true).is_none());
    assert!(q.advance(AdvanceReason::Manual, |_| true).is_none());
}
#[test]
fn one_song_modes_and_state_roundtrip_keep_cached_random_choice() {
    let mut q = QueueState::new(8);
    q.set_list("single".into(), vec!["a".into()]);
    q.select_track("a").unwrap();
    q.set_mode(PlayMode::Random);
    let expected = q.preview_next(AdvanceReason::Ended, |_| true);
    let mut restored: QueueState =
        serde_json::from_str(&serde_json::to_string(&q).unwrap()).unwrap();
    assert_eq!(restored.advance(AdvanceReason::Ended, |_| true), expected);
}

#[test]
fn removing_or_filtering_the_current_entry_preserves_the_unfiltered_position() {
    for mode in [PlayMode::List, PlayMode::ListLoop, PlayMode::SingleLoop] {
        let mut q = queue(mode);
        q.select_track("b").unwrap();
        let id = q.current.as_ref().unwrap().entry_id.clone();
        q.remove(&id);
        q.validate().unwrap();
        assert_eq!(
            q.advance(AdvanceReason::Ended, |_| true).unwrap().track_id,
            "c"
        );
        q.validate().unwrap();
        let mut q = queue(mode);
        q.select_track("b").unwrap();
        assert_eq!(
            q.advance(AdvanceReason::Ended, |entry| entry.track_id != "b")
                .unwrap()
                .track_id,
            "c"
        );
    }
    let mut q = queue(PlayMode::ListLoop);
    q.select_track("b").unwrap();
    assert_eq!(
        q.previous(|entry| entry.track_id != "b").unwrap().track_id,
        "a"
    );
}

#[test]
fn deletion_boundaries_survive_multiple_removals_reorders_and_checkpoint_roundtrip() {
    let mut q = QueueState::new(4);
    q.set_list(
        "owned".into(),
        ["a", "b", "c", "d"].map(str::to_owned).to_vec(),
    );
    q.select_track("b").unwrap();
    q.set_list("owned".into(), ["d", "a", "c"].map(str::to_owned).to_vec());
    q.validate().unwrap();
    assert_eq!(
        q.preview_next(AdvanceReason::Ended, |_| true)
            .unwrap()
            .track_id,
        "c"
    );
    let mut restored: QueueState =
        serde_json::from_str(&serde_json::to_string(&q).unwrap()).unwrap();
    restored.validate().unwrap();
    assert_eq!(
        restored
            .advance(AdvanceReason::Ended, |_| true)
            .unwrap()
            .track_id,
        "c"
    );
    let mut q = queue(PlayMode::ListLoop);
    q.select_track("b").unwrap();
    let b = q.current.as_ref().unwrap().entry_id.clone();
    let a = q.entries[0].entry_id.clone();
    q.remove(&b);
    q.remove(&a);
    q.validate().unwrap();
    assert_eq!(
        q.advance(AdvanceReason::Ended, |_| true).unwrap().track_id,
        "c"
    );
    let mut q = queue(PlayMode::List);
    q.select_track("c").unwrap();
    q.remove(&q.current.as_ref().unwrap().entry_id.clone());
    assert!(q.advance(AdvanceReason::Ended, |_| true).is_none());
    assert_eq!(
        q.advance(AdvanceReason::Manual, |_| true).unwrap().track_id,
        "a"
    );
}

#[test]
fn ordinary_previous_uses_list_order_after_an_explicit_jump() {
    for mode in [
        PlayMode::ListLoop,
        PlayMode::List,
        PlayMode::SingleLoop,
        PlayMode::None,
    ] {
        let mut q = queue(mode);
        q.select_track("c").unwrap();
        assert_eq!(q.previous(|_| true).unwrap().track_id, "b");
        assert_eq!(
            q.advance(AdvanceReason::Manual, |_| true).unwrap().track_id,
            "c"
        );
    }
}

#[test]
fn temporary_previous_restores_consumed_entries_before_original_pending_items() {
    let mut q = queue(PlayMode::ListLoop);
    q.enqueue(vec!["x".into(), "y".into()], false);
    let x = q.advance(AdvanceReason::Manual, |_| true).unwrap();
    assert_eq!(q.previous(|_| true).unwrap().track_id, "a");
    q.validate().unwrap();
    assert_eq!(
        q.temporary
            .iter()
            .map(|entry| entry.track_id.as_str())
            .collect::<Vec<_>>(),
        vec!["x", "y"]
    );
    assert_eq!(q.advance(AdvanceReason::Manual, |_| true), Some(x));
    assert_eq!(
        q.advance(AdvanceReason::Manual, |_| true).unwrap().track_id,
        "y"
    );
    assert_eq!(q.previous(|_| true).unwrap().track_id, "x");
    assert_eq!(q.previous(|_| true).unwrap().track_id, "a");
    q.enqueue(vec!["z".into()], true);
    q.validate().unwrap();
    assert_eq!(
        q.advance(AdvanceReason::Manual, |_| true).unwrap().track_id,
        "z"
    );
    assert_eq!(
        q.advance(AdvanceReason::Manual, |_| true).unwrap().track_id,
        "x"
    );
    assert_eq!(
        q.advance(AdvanceReason::Manual, |_| true).unwrap().track_id,
        "y"
    );
    q.validate().unwrap();
}

#[test]
fn cleared_or_removed_temporary_entries_cannot_return_from_history_or_future() {
    let mut q = queue(PlayMode::ListLoop);
    q.enqueue(vec!["x".into(), "y".into()], false);
    q.advance(AdvanceReason::Manual, |_| true);
    q.advance(AdvanceReason::Manual, |_| true);
    assert_eq!(q.previous(|_| true).unwrap().track_id, "x");
    q.clear_temporary();
    q.validate().unwrap();
    assert!(q.future().iter().all(|entry| !entry.temporary));
    assert_eq!(
        q.advance(AdvanceReason::Manual, |_| true).unwrap().track_id,
        "b"
    );
    assert_eq!(q.previous(|_| true).unwrap().track_id, "a");
    let mut q = queue(PlayMode::ListLoop);
    q.enqueue(vec!["x".into()], false);
    let x = q.advance(AdvanceReason::Manual, |_| true).unwrap();
    assert!(q.remove(&x.entry_id));
    q.validate().unwrap();
    assert_eq!(q.previous(|_| true).unwrap().track_id, "a");
    assert_eq!(
        q.advance(AdvanceReason::Manual, |_| true).unwrap().track_id,
        "b"
    );
    assert!(q.history().iter().all(|entry| entry.entry_id != x.entry_id));
}

#[test]
fn explicit_normal_selection_discards_temporary_playback_navigation() {
    let mut q = queue(PlayMode::ListLoop);
    q.enqueue(vec!["x".into(), "y".into()], false);
    q.advance(AdvanceReason::Manual, |_| true);
    q.select_track("c").unwrap();
    q.validate().unwrap();
    assert!(q.temporary.is_empty());
    assert_eq!(q.previous(|_| true).unwrap().track_id, "b");
}

#[test]
fn foreign_list_temporary_refs_keep_list_identity_and_restore_origin() {
    let mut q = queue(PlayMode::ListLoop);
    q.enqueue_refs(
        vec![
            ("foreign".into(), "x".into()),
            ("catalog".into(), "local".into()),
        ],
        false,
    );
    q.validate().unwrap();
    let x = q.advance(AdvanceReason::Ended, |_| true).unwrap();
    assert_eq!((x.list_id.as_str(), x.track_id.as_str()), ("foreign", "x"));
    assert_eq!(q.origin_list, "owned");
    assert_eq!(
        q.advance(AdvanceReason::Ended, |_| true).unwrap().list_id,
        "catalog"
    );
    assert_eq!(
        q.advance(AdvanceReason::Ended, |_| true).unwrap().track_id,
        "b"
    );
    q.validate().unwrap();
}

#[test]
fn external_policy_changes_invalidate_cached_choices_and_none_recovers() {
    let mut q = queue(PlayMode::ListLoop);
    assert!(q.preview_next(AdvanceReason::Ended, |_| false).is_none());
    assert_eq!(
        q.preview_next(AdvanceReason::Ended, |_| true)
            .unwrap()
            .track_id,
        "b"
    );
    q.enqueue(vec!["x".into()], false);
    assert_eq!(
        q.preview_next(AdvanceReason::Ended, |entry| entry.track_id != "x")
            .unwrap()
            .track_id,
        "b"
    );
    let revision = q.revision;
    q.invalidate_plan();
    assert_ne!(q.revision, revision);
    assert_eq!(
        q.preview_next(AdvanceReason::Ended, |_| true)
            .unwrap()
            .track_id,
        "x"
    );
    assert_eq!(q.temporary.len(), 1);
}

#[test]
fn entering_random_counts_current_then_plays_a_finite_nonrepeating_cycle() {
    let mut q = queue(PlayMode::ListLoop);
    q.set_mode(PlayMode::Random);
    let mut first = std::collections::HashSet::from(["a".to_owned()]);
    for _ in 0..2 {
        assert!(first.insert(q.advance(AdvanceReason::Ended, |_| true).unwrap().track_id));
    }
    assert_eq!(first.len(), 3);
    let last = q.current.as_ref().unwrap().track_id.clone();
    let mut second = std::collections::HashSet::new();
    for index in 0..3 {
        let entry = q.advance(AdvanceReason::Ended, |_| true).unwrap();
        if index == 0 {
            assert_ne!(entry.track_id, last);
        }
        assert!(second.insert(entry.track_id));
    }
    assert_eq!(second.len(), 3);
    q.validate().unwrap();
    let mut q = queue(PlayMode::Random);
    q.select_track("c").unwrap();
    assert_eq!(
        q.previous(|entry| entry.track_id == "c").unwrap().track_id,
        "c"
    );
}

#[test]
fn failed_single_loop_skips_failed_current_without_changing_end_or_none_modes() {
    let mut q = queue(PlayMode::SingleLoop);
    assert_eq!(
        q.preview_next(AdvanceReason::Ended, |_| true)
            .unwrap()
            .track_id,
        "a"
    );
    assert_eq!(
        q.advance(AdvanceReason::Failed, |_| true).unwrap().track_id,
        "b"
    );
    q.validate().unwrap();
    for mode in [PlayMode::SingleLoop, PlayMode::ListLoop, PlayMode::Random] {
        let mut q = QueueState::new(1);
        q.set_list("one".into(), vec!["a".into()]);
        q.select_track("a").unwrap();
        q.set_mode(mode);
        assert!(q.advance(AdvanceReason::Failed, |_| true).is_none());
        q.validate().unwrap();
    }
    let mut q = queue(PlayMode::SingleLoop);
    q.enqueue(vec!["x".into()], false);
    q.advance(AdvanceReason::Ended, |_| true);
    assert_eq!(
        q.advance(AdvanceReason::Failed, |_| true).unwrap().track_id,
        "a"
    );
    let mut q = queue(PlayMode::None);
    assert!(q.advance(AdvanceReason::Failed, |_| true).is_none());
}

#[test]
fn navigation_history_is_bounded_and_future_checkpoint_is_valid() {
    let mut q = queue(PlayMode::Random);
    for index in 0..1200 {
        q.select_track(["b", "c", "a"][index % 3]).unwrap();
    }
    assert_eq!(q.history().len(), 1000);
    for _ in 0..1000 {
        q.previous(|_| true).unwrap();
    }
    assert_eq!(q.future().len(), 1000);
    q.validate().unwrap();
    let restored: QueueState = serde_json::from_str(&serde_json::to_string(&q).unwrap()).unwrap();
    restored.validate().unwrap();
}

fn malformed(mut value: serde_json::Value, change: impl FnOnce(&mut serde_json::Value)) {
    change(&mut value);
    let q: QueueState = serde_json::from_value(value).unwrap();
    assert!(q.validate().is_err());
}

#[test]
fn checkpoint_validation_rejects_identity_rng_counter_and_cached_plan_corruption() {
    let mut q = queue(PlayMode::ListLoop);
    q.preview_next(AdvanceReason::Ended, |_| true);
    q.validate().unwrap();
    let value = serde_json::to_value(&q).unwrap();
    malformed(value.clone(), |v| v["rng"] = 0.into());
    malformed(value.clone(), |v| v["next_id"] = 0.into());
    malformed(value.clone(), |v| v["next_id"] = u64::MAX.into());
    malformed(value.clone(), |v| v["origin_list"] = "".into());
    malformed(value.clone(), |v| {
        v["entries"][1]["entry_id"] = v["entries"][0]["entry_id"].clone()
    });
    malformed(value.clone(), |v| {
        v["entries"][0]["track_id"] = "a\0b".into()
    });
    malformed(value.clone(), |v| {
        v["entries"][0]["list_id"] = "foreign".into()
    });
    malformed(value.clone(), |v| {
        v["entries"][0]["temporary"] = true.into()
    });
    malformed(value.clone(), |v| v["current"]["track_id"] = "other".into());
    malformed(value.clone(), |v| {
        v["entries"][0]["entry_id"] = "queue-0001".into()
    });
    malformed(value.clone(), |v| {
        v["played"] = serde_json::json!(["queue-9999"])
    });
    malformed(value.clone(), |v| v["pending"]["revision"] = 0.into());
    malformed(value.clone(), |v| {
        v["pending"]["random_reset"] = true.into()
    });
    malformed(value, |v| {
        v["pending"]["reason"] = "Failed".into();
        v["pending"]["entry"] = v["current"].clone();
    });
}

#[test]
fn checkpoint_validation_accepts_deleted_current_history_but_enforces_collection_caps() {
    let mut q = queue(PlayMode::ListLoop);
    q.select_track("b").unwrap();
    let b = q.current.as_ref().unwrap().entry_id.clone();
    q.remove(&b);
    q.validate().unwrap();
    assert!(q.current.as_ref().is_some_and(|entry| entry.entry_id == b));
    assert!(!q.entries.iter().any(|entry| entry.entry_id == b));
    let mut q = QueueState::new(1);
    q.set_list("owned".into(), vec!["a".into(); MAX_NORMAL_ENTRIES + 1]);
    assert!(q.validate().is_err());
    let mut q = queue(PlayMode::ListLoop);
    q.enqueue(vec!["x".into(); MAX_TEMPORARY_ENTRIES + 1], false);
    assert!(q.validate().is_err());
    let value = serde_json::to_value(queue(PlayMode::ListLoop)).unwrap();
    malformed(value, |v| {
        let entry = v["entries"][0].clone();
        v["history"] = serde_json::json!(vec![entry; MAX_QUEUE_HISTORY + 1]);
    });
}

#[test]
fn unchanged_origin_reconciliation_preserves_saved_random_preview_and_rng() {
    let mut q = queue(PlayMode::Random);
    let preview = q.preview_next(AdvanceReason::Ended, |_| true).unwrap();
    let snapshot = serde_json::to_value(&q).unwrap();
    let revision = q.revision;
    let mut restored: QueueState = serde_json::from_value(snapshot.clone()).unwrap();
    restored.set_list("owned".into(), vec!["a".into(), "b".into(), "c".into()]);
    restored.validate().unwrap();
    assert_eq!(restored.revision, revision);
    assert_eq!(serde_json::to_value(&restored).unwrap(), snapshot);
    assert_eq!(
        restored.advance(AdvanceReason::Ended, |_| true),
        Some(preview)
    );
    restored.invalidate_plan();
    restored.validate().unwrap();
    assert!(serde_json::to_value(&restored).unwrap()["pending"].is_null());
}

#[test]
fn clearing_missing_restored_current_removes_retirement_without_selecting() {
    let mut q = queue(PlayMode::ListLoop);
    q.enqueue(vec!["x".into()], false);
    q.advance(AdvanceReason::Manual, |_| true);
    q.clear_temporary();
    q.validate().unwrap();
    let origin = serde_json::to_value(&q).unwrap()["normal_cursor"].clone();
    let history = q.history().clone();
    q.clear_current();
    q.validate().unwrap();
    assert!(q.current.is_none());
    assert_eq!(q.history(), &history);
    assert_eq!(serde_json::to_value(&q).unwrap()["normal_cursor"], origin);
    assert_eq!(
        q.advance(AdvanceReason::Manual, |_| true).unwrap().track_id,
        "b"
    );
}

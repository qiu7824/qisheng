use lx_gpui_player::playback::{
    preload::{
        CancelFlag, Invalidation, LeaseOwnership, MAX_PRELOAD_URL_BYTES, PreloadAuthority,
        PreloadContext, PreloadError, PreloadFailure, PreloadService, PreloadState, TriggerTiming,
        UrlDigest,
    },
    queue::{AdvanceReason, PlayMode, QueueState},
};
use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

fn fixture() -> (QueueState, PreloadContext) {
    let mut queue = QueueState::new(7919);
    queue.set_list("songs".into(), vec!["a".into(), "b".into(), "c".into()]);
    queue.select_track("a").unwrap();
    let context = context(&queue);
    (queue, context)
}
fn context(queue: &QueueState) -> PreloadContext {
    PreloadContext {
        playback_generation: 7,
        current_entry_id: queue.current.as_ref().unwrap().entry_id.clone(),
        queue_revision: queue.revision,
        authority: PreloadAuthority::Source {
            sha256: "a".repeat(64),
            policy_version: 2,
        },
        source_epoch: 3,
        actual_quality: "320k".into(),
        approval_epoch: 5,
        authority_approved: true,
    }
}

#[test]
fn catalog_asset_authority_is_closed_and_its_permission_and_content_stamp_fence_preparation() {
    let (mut queue, mut context) = fixture();
    context.authority = PreloadAuthority::CatalogAsset {
        asset_id: "registered-asset-b".into(),
        content_stamp: "c".repeat(64),
    };
    context.actual_quality = "local".into();
    let mut service = PreloadService::new();
    let action = prepare(&mut service, &mut queue, &context);
    assert!(service.validate_action(&action, &context));
    for authority in [
        PreloadAuthority::CatalogAsset {
            asset_id: "registered-asset-c".into(),
            content_stamp: "c".repeat(64),
        },
        PreloadAuthority::CatalogAsset {
            asset_id: "registered-asset-b".into(),
            content_stamp: "d".repeat(64),
        },
        PreloadAuthority::Source {
            sha256: "c".repeat(64),
            policy_version: 2,
        },
    ] {
        let mut changed = context.clone();
        changed.authority = authority;
        assert!(!service.validate_action(&action, &changed));
    }
    let mut invalid = context.clone();
    invalid.actual_quality = "320k".into();
    assert_eq!(invalid.validate(), Err(PreloadError::InvalidField));
    invalid = context.clone();
    invalid.authority = PreloadAuthority::Source {
        sha256: String::new(),
        policy_version: 0,
    };
    assert_eq!(invalid.validate(), Err(PreloadError::InvalidField));
    invalid = context.clone();
    invalid.authority_approved = false;
    assert!(!service.validate_action(&action, &invalid));
    assert!(service.ready(&action, &context, ()).unwrap());
    let entry = queue.advance(AdvanceReason::Ended, |_| true).unwrap();
    let foreground = service
        .promote(&action.operation_id, &context, &entry.entry_id)
        .unwrap();
    assert_eq!(
        foreground.cancel_flag().ownership(),
        LeaseOwnership::Promoted
    );
}
fn timing() -> TriggerTiming {
    TriggerTiming {
        duration_seconds: 20.0,
        position_seconds: 11.0,
        since_seek: Duration::from_secs(3),
    }
}
fn prepare<T>(
    service: &mut PreloadService<T>,
    queue: &mut QueueState,
    context: &PreloadContext,
) -> lx_gpui_player::playback::preload::PrepareAction {
    let action = service
        .begin(queue, context, timing(), |_| true)
        .unwrap()
        .unwrap();
    let url = "https://media.example/stream?token=private";
    assert!(
        service
            .resolved(&action.operation_id, context, url.into())
            .unwrap()
    );
    service.approve(&action.operation_id, context, url).unwrap()
}

#[test]
fn trigger_is_strict_at_ten_seconds_and_settles_for_three_seconds() {
    for (duration, position, milliseconds, expected) in [
        (10.0, 9.0, 3000, false),
        (10.001, 1.0, 3000, true),
        (20.0, 10.0, 3000, false),
        (20.0, 10.001, 3000, true),
        (20.0, 11.0, 2999, false),
        (20.0, 11.0, 3000, true),
        (20.0, 20.0, 3000, true),
        (20.0, 21.0, 3000, false),
        (20.0, -1.0, 3000, false),
        (f64::NAN, 11.0, 3000, false),
        (20.0, f64::INFINITY, 3000, false),
    ] {
        assert_eq!(
            TriggerTiming {
                duration_seconds: duration,
                position_seconds: position,
                since_seek: Duration::from_millis(milliseconds)
            }
            .should_preload(),
            expected
        );
    }
}

#[test]
fn approved_source_only_resolves_and_exact_url_approval_starts_preparation_once() {
    let (mut queue, context) = fixture();
    let mut service = PreloadService::<()>::new();
    let resolve = service
        .begin(&mut queue, &context, timing(), |_| true)
        .unwrap()
        .unwrap();
    assert_eq!(service.snapshot().state, PreloadState::Resolving);
    assert!(service.snapshot().pending_url.is_none());
    assert_eq!(
        service
            .approve(&resolve.operation_id, &context, "https://media.example/a")
            .unwrap_err(),
        PreloadError::InvalidState
    );
    assert!(
        service
            .resolved(
                &resolve.operation_id,
                &context,
                "https://media.example/a?token=1".into()
            )
            .unwrap()
    );
    assert_eq!(service.snapshot().state, PreloadState::AwaitingApproval);
    assert_eq!(
        service.snapshot().pending_url.as_deref(),
        Some("https://media.example/a?token=1")
    );
    assert_eq!(
        service
            .approve(
                &resolve.operation_id,
                &context,
                "https://media.example/a?token=2"
            )
            .unwrap_err(),
        PreloadError::Stale
    );
    assert_eq!(service.snapshot().state, PreloadState::AwaitingApproval);
    let prepare = service
        .approve(
            &resolve.operation_id,
            &context,
            "https://media.example/a?token=1",
        )
        .unwrap();
    assert!(service.validate_action(&prepare, &context));
    assert!(service.snapshot().pending_url.is_none());
    assert_eq!(
        service
            .approve(&resolve.operation_id, &context, &prepare.url)
            .unwrap_err(),
        PreloadError::InvalidState
    );
    assert_eq!(service.snapshot().state, PreloadState::Preparing);
}

#[test]
fn stale_operation_and_duplicate_resolution_cannot_replace_pending_approval() {
    let (mut queue, context) = fixture();
    let mut service = PreloadService::<()>::new();
    let resolve = service
        .begin(&mut queue, &context, timing(), |_| true)
        .unwrap()
        .unwrap();
    assert!(
        !service
            .resolved("preload-0", &context, "https://media.example/stale".into())
            .unwrap()
    );
    assert!(
        service
            .resolved(
                &resolve.operation_id,
                &context,
                "https://media.example/first".into()
            )
            .unwrap()
    );
    assert!(
        !service
            .resolved(
                &resolve.operation_id,
                &context,
                "https://media.example/second".into()
            )
            .unwrap()
    );
    assert_eq!(
        service.snapshot().pending_url.as_deref(),
        Some("https://media.example/first")
    );
}

#[test]
fn digest_binds_signed_query_and_debug_output_hides_url() {
    let (mut queue, context) = fixture();
    let mut service = PreloadService::<()>::new();
    let prepare = prepare(&mut service, &mut queue, &context);
    assert_ne!(
        UrlDigest::of(&prepare.url),
        UrlDigest::of("https://media.example/stream?token=other")
    );
    assert_eq!(prepare.fence.url_digest, Some(UrlDigest::of(&prepare.url)));
    assert!(!format!("{prepare:?}").contains("private"));
    assert!(!format!("{:?}", service.snapshot()).contains("private"));
    let mut forged = prepare.clone();
    forged.url = "https://media.example/stream?token=other".into();
    assert!(!service.validate_action(&forged, &context));
    let mut forged_cancel = prepare.clone();
    forged_cancel.cancel = CancelFlag::new();
    assert!(!service.validate_action(&forged_cancel, &context));
}

#[test]
fn every_context_epoch_rejects_stale_completion_and_cancels_on_synchronize() {
    for field in 0..9 {
        let (mut queue, context) = fixture();
        let mut service = PreloadService::<()>::new();
        let action = prepare(&mut service, &mut queue, &context);
        let mut changed = context.clone();
        match field {
            0 => changed.playback_generation += 1,
            1 => changed.current_entry_id = "queue-999".into(),
            2 => changed.queue_revision += 1,
            3 => {
                changed.authority = PreloadAuthority::Source {
                    sha256: "b".repeat(64),
                    policy_version: 2,
                }
            }
            4 => {
                changed.authority = PreloadAuthority::Source {
                    sha256: "a".repeat(64),
                    policy_version: 3,
                }
            }
            5 => changed.source_epoch += 1,
            6 => changed.actual_quality = "flac".into(),
            7 => changed.approval_epoch += 1,
            _ => changed.authority_approved = false,
        }
        assert!(!service.validate_action(&action, &changed));
        assert!(!service.ready(&action, &changed, ()).unwrap());
        let cancel = service
            .synchronize(&changed, Invalidation::SourceRevoked)
            .unwrap()
            .unwrap();
        assert_eq!(cancel.operation_id, action.operation_id);
        assert!(action.cancel.is_cancelled());
        assert_eq!(service.snapshot().state, PreloadState::Cancelled);
    }
}

#[test]
fn invalidation_advances_epoch_and_old_approvals_never_reactivate() {
    for reason in [
        Invalidation::Queue,
        Invalidation::Mode,
        Invalidation::List,
        Invalidation::Dislike,
        Invalidation::TemporaryQueue,
        Invalidation::Quality,
        Invalidation::Source,
        Invalidation::SourceRevoked,
        Invalidation::Seek,
        Invalidation::Stop,
        Invalidation::Clear,
        Invalidation::Playback,
        Invalidation::ApprovalRevoked,
        Invalidation::Cache,
    ] {
        let (mut queue, context) = fixture();
        let mut service = PreloadService::<()>::new();
        let old = prepare(&mut service, &mut queue, &context);
        let old_epoch = service.operation_epoch();
        assert_eq!(service.invalidate(reason).unwrap().unwrap().reason, reason);
        assert!(service.operation_epoch() > old_epoch);
        let new = service
            .begin(&mut queue, &context, timing(), |_| true)
            .unwrap()
            .unwrap();
        assert_ne!(new.operation_id, old.operation_id);
        assert!(!service.ready(&old, &context, ()).unwrap());
        assert!(
            !service
                .resolved(&old.operation_id, &context, old.url.clone())
                .unwrap()
        );
        assert_eq!(
            service
                .approve(&old.operation_id, &context, &old.url)
                .unwrap_err(),
            PreloadError::Stale
        );
        assert_eq!(service.snapshot().operation_id, Some(new.operation_id));
    }
}

#[test]
fn unapproved_source_cannot_begin_or_reuse_approval() {
    let (mut queue, mut context) = fixture();
    let mut service = PreloadService::<()>::new();
    context.authority_approved = false;
    assert!(
        service
            .begin(&mut queue, &context, timing(), |_| true)
            .unwrap()
            .is_none()
    );
    assert_eq!(service.snapshot().state, PreloadState::Idle);
    context.authority_approved = true;
    let prepare = prepare(&mut service, &mut queue, &context);
    context.authority_approved = false;
    assert!(!service.validate_action(&prepare, &context));
    assert!(
        service
            .synchronize(&context, Invalidation::SourceRevoked)
            .unwrap()
            .is_some()
    );
    assert!(prepare.cancel.is_cancelled());
}

#[test]
fn random_preselection_is_the_real_ended_plan_and_does_not_advance() {
    let (mut queue, _) = fixture();
    queue.set_mode(PlayMode::Random);
    let context = context(&queue);
    let current = queue.current.clone();
    let revision = queue.revision;
    let history = queue.history().clone();
    let mut service = PreloadService::<()>::new();
    let resolve = service
        .begin(&mut queue, &context, timing(), |_| true)
        .unwrap()
        .unwrap();
    for _ in 0..4 {
        assert_eq!(
            queue.preview_next(AdvanceReason::Ended, |_| true),
            Some(resolve.candidate.clone())
        );
        assert!(
            service
                .begin(&mut queue, &context, timing(), |_| true)
                .unwrap()
                .is_none()
        );
    }
    assert_eq!(queue.current, current);
    assert_eq!(queue.revision, revision);
    assert_eq!(queue.history(), &history);
    assert_eq!(
        queue.advance(AdvanceReason::Ended, |_| true),
        Some(resolve.candidate)
    );
}

#[test]
fn temporary_fifo_and_foreign_list_identity_are_not_consumed_by_preload() {
    let (mut queue, _) = fixture();
    queue.enqueue_refs(
        vec![
            ("foreign".into(), "temporary-a".into()),
            ("foreign".into(), "temporary-b".into()),
        ],
        false,
    );
    let context = context(&queue);
    let before = queue.temporary.clone();
    let mut service = PreloadService::<()>::new();
    let action = prepare(&mut service, &mut queue, &context);
    assert!(action.candidate.temporary);
    assert_eq!(action.candidate.list_id, "foreign");
    assert_eq!(queue.temporary, before);
    assert_eq!(
        queue.preview_next(AdvanceReason::Ended, |_| true),
        before.front().cloned()
    );
    service.invalidate(Invalidation::Stop).unwrap();
    assert_eq!(queue.temporary, before);
    assert_eq!(
        queue.advance(AdvanceReason::Ended, |_| true),
        before.front().cloned()
    );
    assert_eq!(queue.temporary.len(), 1);
}

#[test]
fn none_mode_does_not_preload_and_single_loop_keeps_same_entry() {
    let (mut queue, _) = fixture();
    queue.set_mode(PlayMode::None);
    let mut service = PreloadService::<()>::new();
    let ctx = context(&queue);
    assert!(
        service
            .begin(&mut queue, &ctx, timing(), |_| true)
            .unwrap()
            .is_none()
    );
    queue.set_mode(PlayMode::SingleLoop);
    let ctx = context(&queue);
    let action = service
        .begin(&mut queue, &ctx, timing(), |_| true)
        .unwrap()
        .unwrap();
    assert_eq!(action.candidate.entry_id, ctx.current_entry_id);
}

#[test]
fn eligibility_uses_the_queue_plan_for_disliked_and_unavailable_entries() {
    let (mut queue, context) = fixture();
    let mut service = PreloadService::<()>::new();
    let action = service
        .begin(&mut queue, &context, timing(), |entry| {
            entry.track_id != "b"
        })
        .unwrap()
        .unwrap();
    assert_eq!(action.candidate.track_id, "c");
    assert_eq!(
        queue
            .advance(AdvanceReason::Ended, |entry| entry.track_id != "b")
            .unwrap(),
        action.candidate
    );
}

#[test]
fn failed_preload_keeps_current_queue_and_does_not_retry_each_tick() {
    let (mut queue, context) = fixture();
    let before = serde_json::to_value(&queue).unwrap();
    let mut service = PreloadService::<()>::new();
    let action = prepare(&mut service, &mut queue, &context);
    let previewed = serde_json::to_value(&queue).unwrap();
    assert_eq!(before["current"], previewed["current"]);
    assert!(
        service
            .fail(&action.operation_id, &context, PreloadFailure::Preparation)
            .unwrap()
    );
    assert!(action.cancel.is_cancelled());
    assert_eq!(service.snapshot().state, PreloadState::Failed);
    assert_eq!(
        service.snapshot().failure,
        Some(PreloadFailure::Preparation)
    );
    assert_eq!(serde_json::to_value(&queue).unwrap(), previewed);
    assert!(
        service
            .begin(&mut queue, &context, timing(), |_| true)
            .unwrap()
            .is_none()
    );
    assert!(!service.ready(&action, &context, ()).unwrap());
}

#[test]
fn cancelled_preparation_ticket_can_record_failure_without_overriding_other_fences() {
    let (mut queue, context) = fixture();
    let mut service = PreloadService::<()>::new();
    let action = prepare(&mut service, &mut queue, &context);
    let before = serde_json::to_value(&queue).unwrap();
    assert!(action.cancel.cancel_speculative());
    assert!(action.cancel.transport_cancel().load(Ordering::Acquire));
    assert!(
        service
            .fail(&action.operation_id, &context, PreloadFailure::Preparation)
            .unwrap()
    );
    assert_eq!(service.snapshot().state, PreloadState::Failed);
    assert_eq!(
        service.snapshot().failure,
        Some(PreloadFailure::Preparation)
    );
    assert_eq!(serde_json::to_value(&queue).unwrap(), before);
    assert!(
        !service
            .fail(&action.operation_id, &context, PreloadFailure::Resolution)
            .unwrap()
    );
    assert_eq!(
        service.snapshot().failure,
        Some(PreloadFailure::Preparation)
    );
    assert!(!service.ready(&action, &context, ()).unwrap());

    let (mut queue, context) = fixture();
    let mut service = PreloadService::<()>::new();
    let action = prepare(&mut service, &mut queue, &context);
    action.cancel.cancel_speculative();
    let mut stale = context.clone();
    stale.approval_epoch += 1;
    assert!(
        !service
            .fail(&action.operation_id, &stale, PreloadFailure::Preparation)
            .unwrap()
    );
    assert_eq!(service.snapshot().state, PreloadState::Preparing);
    service.invalidate(Invalidation::Stop).unwrap();
    assert!(
        !service
            .fail(&action.operation_id, &context, PreloadFailure::Preparation)
            .unwrap()
    );
    assert_eq!(service.snapshot().state, PreloadState::Cancelled);
}

#[derive(Debug)]
struct CountedValue(Arc<AtomicUsize>);
impl Drop for CountedValue {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

#[test]
fn promotion_transfers_lease_once_and_late_speculative_cancel_preserves_foreground() {
    let (mut queue, context) = fixture();
    let drops = Arc::new(AtomicUsize::new(0));
    let mut service = PreloadService::new();
    let action = prepare(&mut service, &mut queue, &context);
    let io_cancel = action.cancel.transport_cancel();
    assert!(!io_cancel.load(Ordering::Acquire));
    assert!(
        service
            .ready(&action, &context, CountedValue(drops.clone()))
            .unwrap()
    );
    let advanced = queue.advance(AdvanceReason::Ended, |_| true).unwrap();
    let lease = service
        .promote(&action.operation_id, &context, &advanced.entry_id)
        .unwrap();
    assert_eq!(action.cancel.ownership(), LeaseOwnership::Promoted);
    assert_eq!(drops.load(Ordering::SeqCst), 0);
    assert!(!action.cancel.cancel_speculative());
    assert!(!io_cancel.load(Ordering::Acquire));
    service.invalidate(Invalidation::Clear).unwrap();
    drop(service);
    assert!(!action.cancel.is_cancelled());
    assert_eq!(lease.cancel_flag().ownership(), LeaseOwnership::Promoted);
    drop(lease);
    assert!(action.cancel.is_cancelled());
    assert!(io_cancel.load(Ordering::Acquire));
    assert_eq!(drops.load(Ordering::SeqCst), 1);
}

#[test]
fn foreground_into_parts_transfers_cancellation_duty_to_output_owner() {
    let (mut queue, context) = fixture();
    let mut service = PreloadService::<usize>::new();
    let action = prepare(&mut service, &mut queue, &context);
    let io_cancel = action.cancel.transport_cancel();
    assert!(service.ready(&action, &context, 42).unwrap());
    let lease = service
        .promote(&action.operation_id, &context, &action.candidate.entry_id)
        .unwrap();
    let (value, flag) = lease.into_parts();
    assert_eq!(value, 42);
    assert!(!flag.is_cancelled());
    assert!(!io_cancel.load(Ordering::Acquire));
    assert_eq!(service.snapshot().state, PreloadState::Idle);
    assert!(
        service
            .promote(&action.operation_id, &context, &action.candidate.entry_id)
            .is_err()
    );
    flag.cancel();
    assert!(flag.is_cancelled());
    assert!(io_cancel.load(Ordering::Acquire));
}

#[test]
fn cancelled_or_wrong_candidate_never_promotes_and_ready_is_dropped_once() {
    let (mut queue, context) = fixture();
    let drops = Arc::new(AtomicUsize::new(0));
    let mut service = PreloadService::new();
    let action = prepare(&mut service, &mut queue, &context);
    assert!(
        service
            .ready(&action, &context, CountedValue(drops.clone()))
            .unwrap()
    );
    assert!(matches!(
        service.promote(&action.operation_id, &context, "queue-999"),
        Err(PreloadError::Stale)
    ));
    assert_eq!(drops.load(Ordering::SeqCst), 0);
    action.cancel.cancel_speculative();
    assert!(matches!(
        service.promote(&action.operation_id, &context, &action.candidate.entry_id),
        Err(PreloadError::Stale)
    ));
    service.invalidate(Invalidation::Stop).unwrap();
    assert_eq!(drops.load(Ordering::SeqCst), 1);
}

#[test]
fn stale_ready_completion_drops_only_its_injected_value() {
    let (mut queue, context) = fixture();
    let drops = Arc::new(AtomicUsize::new(0));
    let mut service = PreloadService::new();
    let old = prepare(&mut service, &mut queue, &context);
    service.invalidate(Invalidation::Seek).unwrap();
    let new = prepare(&mut service, &mut queue, &context);
    assert!(
        !service
            .ready(&old, &context, CountedValue(drops.clone()))
            .unwrap()
    );
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    assert!(!new.cancel.is_cancelled());
    assert_eq!(service.snapshot().state, PreloadState::Preparing);
}

#[test]
fn cancel_flag_race_has_a_single_speculative_ownership_transition() {
    let flag = CancelFlag::new();
    let mut workers = Vec::new();
    for _ in 0..8 {
        let flag = flag.clone();
        workers.push(std::thread::spawn(move || flag.cancel_speculative()));
    }
    assert_eq!(
        workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .filter(|cancelled| *cancelled)
            .count(),
        1
    );
    assert!(flag.is_cancelled());
    assert!(!flag.cancel_speculative());
}

#[test]
fn invalid_fields_and_oversized_urls_are_rejected_before_mutating_state_or_queue() {
    let (mut queue, context) = fixture();
    let before = serde_json::to_value(&queue).unwrap();
    let mut service = PreloadService::<()>::new();
    let mut invalid = context.clone();
    invalid.actual_quality = "x".repeat(65);
    assert_eq!(
        service
            .begin(&mut queue, &invalid, timing(), |_| true)
            .unwrap_err(),
        PreloadError::InvalidField
    );
    assert_eq!(serde_json::to_value(&queue).unwrap(), before);
    assert_eq!(service.operation_epoch(), 0);
    let action = service
        .begin(&mut queue, &context, timing(), |_| true)
        .unwrap()
        .unwrap();
    for invalid in [
        "file:///private/a".into(),
        "https://user:pass@example/a".into(),
        "https://example/a#fragment".into(),
        "https://example/a\nheader".into(),
        format!(
            "https://example/a?token={}",
            "a".repeat(MAX_PRELOAD_URL_BYTES)
        ),
    ] {
        assert_eq!(
            service
                .resolved(&action.operation_id, &context, invalid)
                .unwrap_err(),
            PreloadError::InvalidUrl
        );
        assert_eq!(service.snapshot().state, PreloadState::Resolving);
        assert!(service.snapshot().pending_url.is_none());
    }
}

#[test]
fn url_byte_limit_accepts_exact_boundary_and_rejects_one_more_byte() {
    for (extra, expected) in [(0, true), (1, false)] {
        let (mut queue, context) = fixture();
        let mut service = PreloadService::<()>::new();
        let action = service
            .begin(&mut queue, &context, timing(), |_| true)
            .unwrap()
            .unwrap();
        let prefix = "https://media.example/a?token=";
        let url = format!(
            "{prefix}{}",
            "a".repeat(MAX_PRELOAD_URL_BYTES - prefix.len() + extra)
        );
        let result = service.resolved(&action.operation_id, &context, url);
        assert_eq!(result.is_ok(), expected);
        assert_eq!(
            service.snapshot().state,
            if expected {
                PreloadState::AwaitingApproval
            } else {
                PreloadState::Resolving
            }
        );
    }
}

#[test]
fn invalid_candidate_fields_do_not_publish_a_queue_preview() {
    let (mut queue, context) = fixture();
    queue.entries[1].track_id = "bad\ntrack".into();
    let before = serde_json::to_value(&queue).unwrap();
    let mut service = PreloadService::<()>::new();
    assert_eq!(
        service
            .begin(&mut queue, &context, timing(), |_| true)
            .unwrap_err(),
        PreloadError::InvalidField
    );
    assert_eq!(serde_json::to_value(&queue).unwrap(), before);
    assert_eq!(service.snapshot().state, PreloadState::Idle);
}

#[test]
fn ready_promotion_requires_current_source_and_approval_fence() {
    for source_revoked in [false, true] {
        let (mut queue, context) = fixture();
        let mut service = PreloadService::<()>::new();
        let action = prepare(&mut service, &mut queue, &context);
        assert!(service.ready(&action, &context, ()).unwrap());
        let mut changed = context.clone();
        if source_revoked {
            changed.authority_approved = false;
        } else {
            changed.approval_epoch += 1;
        }
        assert!(matches!(
            service.promote(&action.operation_id, &changed, &action.candidate.entry_id),
            Err(PreloadError::Stale)
        ));
        assert_eq!(action.cancel.ownership(), LeaseOwnership::Speculative);
        service
            .synchronize(&changed, Invalidation::ApprovalRevoked)
            .unwrap();
        assert!(action.cancel.is_cancelled());
    }
}

#[test]
fn speculative_cancel_racing_promotion_never_cancels_a_promoted_owner() {
    for _ in 0..32 {
        let (mut queue, context) = fixture();
        let mut service = PreloadService::<()>::new();
        let action = prepare(&mut service, &mut queue, &context);
        let io_cancel = action.cancel.transport_cancel();
        assert!(!io_cancel.load(std::sync::atomic::Ordering::Acquire));
        assert!(service.ready(&action, &context, ()).unwrap());
        let flag = action.cancel.clone();
        let worker = std::thread::spawn(move || flag.cancel_speculative());
        let promoted = service.promote(&action.operation_id, &context, &action.candidate.entry_id);
        let cancelled = worker.join().unwrap();
        match promoted {
            Ok(lease) => {
                assert!(!cancelled);
                assert_eq!(action.cancel.ownership(), LeaseOwnership::Promoted);
                assert!(!action.cancel.is_cancelled());
                assert!(!io_cancel.load(std::sync::atomic::Ordering::Acquire));
                drop(lease);
                assert!(io_cancel.load(std::sync::atomic::Ordering::Acquire));
            }
            Err(PreloadError::Stale) => {
                assert!(cancelled);
                assert!(action.cancel.is_cancelled());
                assert!(io_cancel.load(std::sync::atomic::Ordering::Acquire));
            }
            Err(error) => panic!("Unexpected promotion error: {error}"),
        }
    }
}

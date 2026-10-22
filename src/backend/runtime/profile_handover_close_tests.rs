use super::*;
use crate::{
    backend::{BackendConfig, BackendHandle},
    repository::protect_close_proof_for_profile_for_test,
};
use windows_sys::Win32::Foundation::{HANDLE_FLAG_PROTECT_FROM_CLOSE, SetHandleInformation};

#[test]
fn actual_backend_handover_keeps_guard_and_proof_handle_until_native_close_really_succeeds() {
    let directory =
        tempfile::tempdir_in(r"E:\codex操作目录\tasks\lx-gpui-player\test-temp").unwrap();
    let config = BackendConfig::headless(directory.path().to_path_buf());
    let backend = BackendHandle::start(config.clone()).unwrap();
    let (snapshot, _) = backend.snapshot_with_cursor().unwrap();
    let _fault = protect_close_proof_for_profile_for_test(directory.path()).unwrap();
    let ticket = backend
        .begin_profile_quiesce("actual-native-close-proof".into(), snapshot.revision)
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    let (raw, continuity) = loop {
        let view = ticket.observe(Duration::from_millis(10)).unwrap();
        assert_ne!(
            view.phase, "ready",
            "Handover claimed ready while its native proof was protected"
        );
        let captured = {
            let state = ticket
                .slot
                .state
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            match state.outcome.as_ref() {
                Some(OwnedQuiesceOutcome::Retained(owner)) => match &owner.owner {
                    RepositoryCloseOwner::Guard(guard) => guard
                        .pending_close_handle_for_test()
                        .map(|raw| (raw, guard.continuity_token().unwrap())),
                    _ => None,
                },
                _ => None,
            }
        };
        if let Some(captured) = captured {
            break captured;
        }
        assert!(
            Instant::now() < deadline,
            "Actual post-SQLite-close guard did not remain retained"
        );
        thread::sleep(Duration::from_millis(5));
    };
    let failure = ticket.take_handover(Duration::ZERO).unwrap_err();
    let ticket = failure.owner;
    assert_ne!(ticket.view().phase, "ready");
    assert!(ProfileGuard::acquire(directory.path()).is_err());
    let start = Instant::now();
    while start.elapsed() < Duration::from_millis(40) {
        let view = ticket.view();
        assert_ne!(
            view.phase, "ready",
            "Guard verification substituted for actual native close"
        );
        let state = ticket
            .slot
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if let Some(OwnedQuiesceOutcome::Retained(owner)) = state.outcome.as_ref()
            && let RepositoryCloseOwner::Guard(guard) = &owner.owner
        {
            assert_eq!(guard.pending_close_handle_for_test(), Some(raw));
            guard.verify_continuity(&continuity).unwrap();
        }
        drop(state);
        thread::sleep(Duration::from_millis(5));
    }
    assert_ne!(
        unsafe { SetHandleInformation(raw, HANDLE_FLAG_PROTECT_FROM_CLOSE, 0) },
        0
    );
    let deadline = Instant::now() + Duration::from_secs(20);
    while ticket.view().phase != "ready" {
        assert!(
            Instant::now() < deadline,
            "Native proof close did not finish after unprotecting its original handle"
        );
        thread::sleep(Duration::from_millis(5));
    }
    let handover = ticket.take_handover(Duration::from_secs(20)).unwrap();
    assert!(handover.guard.pending_close_handle_for_test().is_none());
    handover.guard.verify_continuity(&continuity).unwrap();
    assert!(backend.retirement_state().actor_joined && backend.retirement_state().runtime_retired);
    assert!(ProfileGuard::acquire(directory.path()).is_err());
    let restarted = BackendHandle::start_with_guard(config, handover.guard).unwrap();
    restarted.snapshot_with_cursor().unwrap();
    restarted.shutdown(Duration::from_secs(15)).unwrap();
    ProfileGuard::acquire(directory.path())
        .unwrap()
        .verify()
        .unwrap();
}

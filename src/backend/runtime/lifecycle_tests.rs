use super::*;
use crate::repository::ProfileGuard;
use std::sync::OnceLock;
static CASE: Mutex<()> = Mutex::new(());
struct AttachedStartupFault {
    entered: mpsc::SyncSender<()>,
    release: mpsc::Receiver<()>,
    owner: mpsc::SyncSender<startup_ownership::ActorLifecycle>,
}
static STARTUP_FAULTS: OnceLock<Mutex<HashMap<std::path::PathBuf, AttachedStartupFault>>> =
    OnceLock::new();
pub(super) fn fail_attached_startup_if_requested(
    core: &mut Runtime,
    directory: &std::path::Path,
    lifecycle: startup_ownership::ActorLifecycle,
) -> Result<()> {
    let Some(faults) = STARTUP_FAULTS.get() else {
        return Ok(());
    };
    let mut faults = faults.lock().unwrap_or_else(|e| e.into_inner());
    if faults.is_empty() {
        return Ok(());
    }
    let key = std::fs::canonicalize(directory)?;
    let fault = faults.remove(&key);
    drop(faults);
    if let Some(fault) = fault {
        let _ = fault.owner.send(lifecycle);
        core.tasks.push(thread::spawn(move || {
            let _ = fault.entered.send(());
            let _ = fault.release.recv();
        }));
        anyhow::bail!("Attached runtime startup fault with an actual gated worker");
    }
    Ok(())
}

fn isolated_profile() -> tempfile::TempDir {
    let base = std::env::temp_dir().join("lx-native-lifecycle-fixtures");
    std::fs::create_dir_all(&base).unwrap();
    tempfile::Builder::new()
        .prefix("owned-profile-")
        .tempdir_in(base)
        .unwrap()
}
fn gate(backend: &BackendHandle) -> mpsc::SyncSender<()> {
    let (entered, observed) = mpsc::sync_channel(1);
    let (release, blocked) = mpsc::sync_channel(1);
    backend
        .inner
        .commands
        .send(RuntimeMessage::TestGatedTask {
            entered,
            release: blocked,
        })
        .unwrap_or_else(|_| panic!("Actor fixture admission failed"));
    observed.recv_timeout(Duration::from_secs(5)).unwrap();
    release
}
fn wait(mut test: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while !test() {
        assert!(
            Instant::now() < deadline,
            "Actual lifecycle did not reach its terminal state"
        );
        thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn actor_panic_retains_whole_runtime_and_actual_worker_after_all_public_handles_drop() {
    let _case = CASE.lock().unwrap_or_else(|e| e.into_inner());
    let directory = isolated_profile();
    let backend =
        BackendHandle::start(BackendConfig::headless(directory.path().to_path_buf())).unwrap();
    let release = gate(&backend);
    let lifecycle = backend.inner.lifecycle.clone();
    backend
        .inner
        .commands
        .send(RuntimeMessage::TestActorPanic)
        .unwrap_or_else(|_| panic!("Actor failure fixture admission failed"));
    wait(|| {
        let _ = lifecycle.poll();
        lifecycle.view().actor_joined
    });
    let view = lifecycle.view();
    assert!(!view.runtime_retired);
    assert!(
        view.error
            .as_ref()
            .is_some_and(|error| error.message.contains("actor panicked"))
    );
    drop(backend);
    assert!(ProfileGuard::acquire(directory.path()).is_err());
    release.send(()).unwrap();
    wait(|| lifecycle.poll().unwrap_or(false));
    assert!(lifecycle.view().actor_joined && lifecycle.view().runtime_retired);
    ProfileGuard::acquire(directory.path())
        .unwrap()
        .verify()
        .unwrap();
}

#[test]
fn timed_out_handover_returns_the_same_owner_until_real_worker_join_and_sqlite_close() {
    let _case = CASE.lock().unwrap_or_else(|e| e.into_inner());
    let directory = isolated_profile();
    let backend =
        BackendHandle::start(BackendConfig::headless(directory.path().to_path_buf())).unwrap();
    let release = gate(&backend);
    let ticket = backend
        .begin_profile_quiesce("gated-handover".into(), backend.snapshot().revision)
        .unwrap();
    let failure = ticket.take_handover(Duration::from_millis(25)).unwrap_err();
    assert_eq!(failure.error.kind, ErrorKind::Timeout);
    assert_eq!(failure.owner.scope().operation_id, "gated-handover");
    assert!(ProfileGuard::acquire(directory.path()).is_err());
    assert!(!backend.retirement_state().runtime_retired);
    release.send(()).unwrap();
    let handover = failure
        .owner
        .take_handover(Duration::from_secs(15))
        .unwrap();
    assert!(backend.retirement_state().actor_joined && backend.retirement_state().runtime_retired);
    handover.guard.verify().unwrap();
    assert!(ProfileGuard::acquire(directory.path()).is_err());
    let restored = BackendHandle::start_with_guard(
        BackendConfig::headless(directory.path().to_path_buf()),
        handover.guard,
    )
    .unwrap();
    assert!(ProfileGuard::acquire(directory.path()).is_err());
    restored.shutdown(Duration::from_secs(15)).unwrap();
}

#[test]
fn dropped_handover_observer_does_not_release_the_profile_before_actual_retirement() {
    let _case = CASE.lock().unwrap_or_else(|e| e.into_inner());
    let directory = isolated_profile();
    let backend =
        BackendHandle::start(BackendConfig::headless(directory.path().to_path_buf())).unwrap();
    let release = gate(&backend);
    let ticket = backend
        .begin_profile_quiesce("dropped-observer".into(), backend.snapshot().revision)
        .unwrap();
    wait(|| ticket.view().actor_joined);
    drop(ticket);
    drop(backend);
    assert!(ProfileGuard::acquire(directory.path()).is_err());
    release.send(()).unwrap();
    wait(|| match ProfileGuard::acquire(directory.path()) {
        Ok(guard) => {
            guard.verify().unwrap();
            true
        }
        Err(_) => false,
    });
}

#[test]
fn ordinary_shutdown_timeout_keeps_the_real_runtime_and_later_observation_joins_it() {
    let _case = CASE.lock().unwrap_or_else(|e| e.into_inner());
    let directory = isolated_profile();
    let backend =
        BackendHandle::start(BackendConfig::headless(directory.path().to_path_buf())).unwrap();
    let release = gate(&backend);
    let error = backend.shutdown(Duration::from_millis(25)).unwrap_err();
    assert_eq!(error.kind, ErrorKind::Timeout);
    assert!(!backend.retirement_state().runtime_retired);
    assert!(ProfileGuard::acquire(directory.path()).is_err());
    release.send(()).unwrap();
    backend.shutdown(Duration::from_secs(15)).unwrap();
    assert!(backend.retirement_state().actor_joined && backend.retirement_state().runtime_retired);
    ProfileGuard::acquire(directory.path())
        .unwrap()
        .verify()
        .unwrap();
}

#[test]
fn failed_attached_initialization_retains_repository_until_the_whole_core_actually_joins() {
    let _case = CASE.lock().unwrap_or_else(|e| e.into_inner());
    let directory = isolated_profile();
    let (entered, observed) = mpsc::sync_channel(1);
    let (release, blocked) = mpsc::sync_channel(1);
    let (owner, owned) = mpsc::sync_channel(1);
    STARTUP_FAULTS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(
            std::fs::canonicalize(directory.path()).unwrap(),
            AttachedStartupFault {
                entered,
                release: blocked,
                owner,
            },
        );
    let guard = ProfileGuard::acquire(directory.path()).unwrap();
    let error = match BackendHandle::start_with_guard(
        BackendConfig::headless(directory.path().to_path_buf()),
        guard,
    ) {
        Err(error) => error,
        Ok(backend) => {
            backend.shutdown(Duration::from_secs(15)).unwrap();
            panic!("Attached startup fault was not returned");
        }
    };
    observed.recv_timeout(Duration::from_secs(5)).unwrap();
    let lifecycle = owned.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(lifecycle.has_owned_runtime());
    let failure = error
        .downcast_ref::<instance_relay::RetainedRepositoryInitializationFailure>()
        .expect("All post-open initialization errors must retain their actual repository");
    assert!(failure.has_repository());
    assert!(failure.error().to_string().contains("actual gated worker"));
    assert!(ProfileGuard::acquire(directory.path()).is_err());
    let repository = failure.take_repository().unwrap();
    assert!(!failure.has_repository());
    let pending = repository.close_into_guard().unwrap_err();
    let crate::repository::RepositoryCloseOwner::Shared(mut repository) = pending.owner else {
        panic!("A live actual Core must still own the repository");
    };
    assert!(ProfileGuard::acquire(directory.path()).is_err());
    release.send(()).unwrap();
    wait(|| lifecycle.poll().unwrap_or(false));
    assert!(!lifecycle.has_owned_runtime());
    let deadline = Instant::now() + Duration::from_secs(15);
    let guard = loop {
        match repository.close_into_guard() {
            Ok(guard) => break guard,
            Err(failure) => {
                repository = match failure.owner {
                    crate::repository::RepositoryCloseOwner::Shared(repository) => repository,
                    crate::repository::RepositoryCloseOwner::Exclusive(repository) => {
                        Arc::<Repository>::from(repository)
                    }
                    crate::repository::RepositoryCloseOwner::Guard(_) => {
                        panic!("A fresh owned fixture guard failed verification")
                    }
                };
            }
        }
        assert!(
            Instant::now() < deadline,
            "Attached Core did not actually retire"
        );
        thread::sleep(Duration::from_millis(5));
    };
    guard.verify().unwrap();
    assert!(ProfileGuard::acquire(directory.path()).is_err());
    let backend = BackendHandle::start_with_guard(
        BackendConfig::headless(directory.path().to_path_buf()),
        guard,
    )
    .unwrap();
    backend.shutdown(Duration::from_secs(15)).unwrap();
    ProfileGuard::acquire(directory.path())
        .unwrap()
        .verify()
        .unwrap();
}

#[test]
fn handover_worker_panic_remains_fatal_after_actual_close_and_returns_the_same_guard() {
    let _case = CASE.lock().unwrap_or_else(|e| e.into_inner());
    let directory = isolated_profile();
    let backend =
        BackendHandle::start(BackendConfig::headless(directory.path().to_path_buf())).unwrap();
    let release_sibling = gate(&backend);
    let (entered, observed) = mpsc::sync_channel(1);
    let (release_panic, blocked) = mpsc::sync_channel(1);
    backend
        .inner
        .commands
        .send(RuntimeMessage::TestGatedPanicTask {
            entered,
            release: blocked,
        })
        .unwrap_or_else(|_| panic!("Panicking worker fixture admission failed"));
    observed.recv_timeout(Duration::from_secs(5)).unwrap();
    let ticket = backend
        .begin_profile_quiesce("fatal-worker-handover".into(), backend.snapshot().revision)
        .unwrap();
    wait(|| ticket.view().actor_joined);
    release_panic.send(()).unwrap();
    wait(|| ticket.view().error.is_some());
    let failure = ticket.take_handover(Duration::from_millis(25)).unwrap_err();
    let original = failure.error.clone();
    assert_eq!(original.kind, ErrorKind::OperationFailed);
    assert_eq!(original.message, "A profile handover worker panicked");
    assert!(backend.inner.lifecycle.has_owned_runtime());
    assert!(!backend.retirement_state().runtime_retired);
    assert!(ProfileGuard::acquire(directory.path()).is_err());
    let failure = failure
        .owner
        .take_failed_guard(Duration::from_millis(25))
        .unwrap_err();
    assert_eq!(failure.error.message, original.message);
    assert!(ProfileGuard::acquire(directory.path()).is_err());
    release_sibling.send(()).unwrap();
    wait(|| {
        failure.owner.view().phase == "failed_guard" && backend.retirement_state().runtime_retired
    });
    let closed = failure.owner.observe(Duration::ZERO).unwrap();
    assert!(closed.actor_joined);
    assert_eq!(closed.error.unwrap().message, original.message);
    assert!(!backend.inner.lifecycle.has_owned_runtime());
    let failure = failure.owner.take_handover(Duration::ZERO).unwrap_err();
    assert_eq!(failure.error.message, original.message);
    let stopped = backend.shutdown(Duration::from_secs(15)).unwrap_err();
    assert_eq!(stopped.kind, original.kind);
    assert_eq!(stopped.message, original.message);
    assert!(ProfileGuard::acquire(directory.path()).is_err());
    let guard = failure
        .owner
        .take_failed_guard(Duration::from_secs(15))
        .unwrap();
    guard.verify().unwrap();
    assert!(ProfileGuard::acquire(directory.path()).is_err());
    let restarted = BackendHandle::start_with_guard(
        BackendConfig::headless(directory.path().to_path_buf()),
        guard,
    )
    .unwrap();
    restarted.shutdown(Duration::from_secs(15)).unwrap();
    ProfileGuard::acquire(directory.path())
        .unwrap()
        .verify()
        .unwrap();
}

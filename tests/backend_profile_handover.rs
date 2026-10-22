use lx_gpui_player::{
    backend::{
        BackendCommand, BackendConfig, BackendHandle, CommandEnvelope, ErrorKind,
        RetainedProfileGuardStartupFailure,
    },
    repository::{ProfileGuard, Repository},
};
use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

static CASE: Mutex<()> = Mutex::new(());

fn close_actual_repository(mut repository: Arc<Repository>) -> ProfileGuard {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        match repository.close_into_guard() {
            Ok(guard) => return guard,
            Err(failure) => {
                repository = match failure.owner {
                    lx_gpui_player::repository::RepositoryCloseOwner::Shared(repository) => {
                        repository
                    }
                    lx_gpui_player::repository::RepositoryCloseOwner::Exclusive(repository) => {
                        Arc::<Repository>::from(repository)
                    }
                    lx_gpui_player::repository::RepositoryCloseOwner::Guard(_) => {
                        panic!("Fresh fixture guard failed verification")
                    }
                };
            }
        }
        assert!(
            Instant::now() < deadline,
            "Actual initialization retirement did not close"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn profile() -> tempfile::TempDir {
    let base = std::env::temp_dir().join("lx-native-lifecycle-fixtures");
    std::fs::create_dir_all(&base).unwrap();
    tempfile::Builder::new()
        .prefix("public-handover-")
        .tempdir_in(base)
        .unwrap()
}
#[test]
fn actual_handover_carries_the_same_lock_through_sqlite_close_and_restart() {
    let _case = CASE.lock().unwrap_or_else(|e| e.into_inner());
    let directory = profile();
    let config = BackendConfig::headless(directory.path().to_path_buf());
    let backend = BackendHandle::start(config.clone()).unwrap();
    let (snapshot, _) = backend.snapshot_with_cursor().unwrap();
    let ticket = backend
        .begin_profile_quiesce("public-continuous-guard".into(), snapshot.revision)
        .unwrap();
    assert!(ProfileGuard::acquire(directory.path()).is_err());
    let handover = ticket.take_handover(Duration::from_secs(15)).unwrap();
    assert_eq!(handover.scope.expected_revision, snapshot.revision);
    assert_eq!(handover.checkpoint.store_digest.len(), 64);
    assert_eq!(handover.checkpoint.checkpoint_digest.len(), 64);
    assert!(backend.retirement_state().actor_joined && backend.retirement_state().runtime_retired);
    handover.guard.verify().unwrap();
    assert!(ProfileGuard::acquire(directory.path()).is_err());
    let restarted = BackendHandle::start_with_guard(config, handover.guard).unwrap();
    assert!(ProfileGuard::acquire(directory.path()).is_err());
    restarted.shutdown(Duration::from_secs(15)).unwrap();
    ProfileGuard::acquire(directory.path())
        .unwrap()
        .verify()
        .unwrap();
}
#[test]
fn stale_handover_revision_rejects_before_retirement_and_reopens_mutation_admission() {
    let _case = CASE.lock().unwrap_or_else(|e| e.into_inner());
    let directory = profile();
    let backend =
        BackendHandle::start(BackendConfig::headless(directory.path().to_path_buf())).unwrap();
    let (snapshot, _) = backend.snapshot_with_cursor().unwrap();
    let ticket = backend
        .begin_profile_quiesce("stale-quiesce".into(), snapshot.revision + 1)
        .unwrap();
    let view = ticket.observe(Duration::from_secs(5)).unwrap();
    assert_eq!(view.phase, "rejected");
    assert_eq!(view.error.unwrap().kind, ErrorKind::Conflict);
    assert!(
        !backend.retirement_state().actor_joined && !backend.retirement_state().runtime_retired
    );
    backend.snapshot_with_cursor().unwrap();
    backend
        .execute(CommandEnvelope {
            request_id: "after-stale-handover".into(),
            origin: "test".into(),
            expected_revision: None,
            command: BackendCommand::Volume { volume: 0.37 },
        })
        .unwrap();
    assert!(ProfileGuard::acquire(directory.path()).is_err());
    let (snapshot, _) = backend.snapshot_with_cursor().unwrap();
    backend
        .begin_profile_quiesce("after-stale-handover".into(), snapshot.revision)
        .unwrap()
        .take_handover(Duration::from_secs(15))
        .unwrap()
        .guard
        .verify()
        .unwrap();
}

#[test]
fn full_actual_lifecycle_admission_returns_the_original_unopened_guard() {
    let _case = CASE.lock().unwrap_or_else(|e| e.into_inner());
    let profiles = (0..4).map(|_| profile()).collect::<Vec<_>>();
    let running = profiles
        .iter()
        .map(|directory| {
            BackendHandle::start(BackendConfig::headless(directory.path().to_path_buf())).unwrap()
        })
        .collect::<Vec<_>>();
    let target = profile();
    let guard = ProfileGuard::acquire(target.path()).unwrap();
    let error = match BackendHandle::start_with_guard(
        BackendConfig::headless(target.path().to_path_buf()),
        guard,
    ) {
        Err(error) => error,
        Ok(backend) => {
            backend.shutdown(Duration::from_secs(15)).unwrap();
            panic!("A fifth live lifecycle was admitted");
        }
    };
    let failure = error
        .downcast_ref::<RetainedProfileGuardStartupFailure>()
        .expect("Admission failure must retain the exact pre-open guard");
    assert!(failure.has_guard());
    assert!(failure.error().to_string().contains("capacity reached"));
    assert!(ProfileGuard::acquire(target.path()).is_err());
    let original = failure.take_guard().unwrap();
    assert!(!failure.has_guard());
    original.verify().unwrap();
    assert!(ProfileGuard::acquire(target.path()).is_err());
    drop(original);
    ProfileGuard::acquire(target.path())
        .unwrap()
        .verify()
        .unwrap();
    for backend in running {
        backend.shutdown(Duration::from_secs(15)).unwrap();
    }
}

#[test]
fn wrong_config_returns_the_same_guard_and_correct_retry_keeps_lock_ownership_continuous() {
    let _case = CASE.lock().unwrap_or_else(|e| e.into_inner());
    let target = profile();
    let foreign = profile();
    let guard = ProfileGuard::acquire(target.path()).unwrap();
    let error = match BackendHandle::start_with_guard(
        BackendConfig::headless(foreign.path().to_path_buf()),
        guard,
    ) {
        Err(error) => error,
        Ok(backend) => {
            backend.shutdown(Duration::from_secs(15)).unwrap();
            panic!("A mismatched profile accepted the guard");
        }
    };
    let failure = error
        .downcast_ref::<RetainedProfileGuardStartupFailure>()
        .expect("Config mismatch must retain the actual supplied guard");
    assert!(failure.has_guard());
    assert!(
        failure
            .error()
            .to_string()
            .contains("another backend profile")
    );
    assert!(ProfileGuard::acquire(target.path()).is_err());
    ProfileGuard::acquire(foreign.path())
        .unwrap()
        .verify()
        .unwrap();
    let original = failure.take_guard().unwrap();
    original.verify().unwrap();
    assert!(ProfileGuard::acquire(target.path()).is_err());
    let backend = BackendHandle::start_with_guard(
        BackendConfig::headless(target.path().to_path_buf()),
        original,
    )
    .unwrap();
    assert!(ProfileGuard::acquire(target.path()).is_err());
    backend.shutdown(Duration::from_secs(15)).unwrap();
    ProfileGuard::acquire(target.path())
        .unwrap()
        .verify()
        .unwrap();
}

#[test]
fn damaged_json_during_durable_preference_recovery_returns_the_actual_open_repository() {
    use lx_gpui_player::backend::RetainedRepositoryInitializationFailure;
    use lx_gpui_player::backup::{BackupEncoding, ConflictPolicy, decode_lx};
    let _case = CASE.lock().unwrap_or_else(|e| e.into_inner());
    let directory = profile();
    let guard = ProfileGuard::acquire(directory.path()).unwrap();
    let repository = Arc::new(Repository::open_with_guard(guard).unwrap());
    let setting=decode_lx(&serde_json::to_vec(&serde_json::json!({"type":"setting_v2","data":{"version":"2.1.0","player.volume":0.52}})).unwrap(),BackupEncoding::Json).unwrap();
    let preview = repository
        .preview_lx_backup(&setting, ConflictPolicy::ReplaceTracks)
        .unwrap();
    repository
        .commit_lx_backup("pending-initialization-preferences", &preview, &|| Ok(()))
        .unwrap();
    assert_eq!(repository.pending_lx_backup_preferences().unwrap().len(), 1);
    let guard = repository.close_into_guard().unwrap();
    let state = directory.path().join("player-state.json");
    std::fs::write(&state, b"{invalid-json").unwrap();
    let error = match BackendHandle::start_with_guard(
        BackendConfig::headless(directory.path().to_path_buf()),
        guard,
    ) {
        Err(error) => error,
        Ok(backend) => {
            backend.shutdown(Duration::from_secs(15)).unwrap();
            panic!("Damaged JSON with pending preferences was accepted");
        }
    };
    let failure = error
        .downcast_ref::<RetainedRepositoryInitializationFailure>()
        .expect("Post-open Store initialization must preserve the actual repository");
    assert!(failure.has_repository());
    assert!(
        failure
            .error()
            .to_string()
            .contains("Repair the saved player state")
    );
    assert_eq!(std::fs::read(&state).unwrap(), b"{invalid-json");
    assert!(ProfileGuard::acquire(directory.path()).is_err());
    let repository = failure.take_repository().unwrap();
    assert!(!failure.has_repository());
    assert_eq!(repository.pending_lx_backup_preferences().unwrap().len(), 1);
    let guard = close_actual_repository(repository);
    assert!(ProfileGuard::acquire(directory.path()).is_err());
    std::fs::remove_file(&state).unwrap();
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

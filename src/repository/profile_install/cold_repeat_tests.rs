//! Actual second-process exits while resuming an already journaled group.
use super::*;

fn context() -> PhysicalRestoreContext {
    PhysicalRestoreContext::new(Duration::from_secs(30)).unwrap()
}
fn select(folder: &Path, epoch: u64) -> (ProfileGuard, PhysicalProfileRecoverySelection) {
    let guard = ProfileGuard::acquire(folder.join("target")).unwrap();
    let staging = folder.join("target").join(".lx-snapshot-staging");
    let journal = std::fs::read_dir(&staging)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| journal_nonce(name).is_ok())
        })
        .unwrap();
    let mut selection =
        PhysicalProfileRecoverySelection::open_selected(&guard, journal, epoch, &context())
            .unwrap();
    selection
        .approve_current_content(&selection.preview().unwrap().approval_sha256.clone())
        .unwrap();
    (guard, selection)
}

#[test]
#[ignore = "Real second-process exit helper invoked by the repeated cold matrix"]
fn repeat_crash_child() {
    let folder = PathBuf::from(std::env::var_os("LX_PROFILE_REPEAT_CHILD_DIR").unwrap());
    let (guard, selection) = select(&folder, 222);
    let limits = PhysicalRestoreLimits::new(128 * 1024 * 1024).unwrap();
    let recovered =
        PhysicalProfileInstallation::recover_selected(guard, selection, &limits, &context())
            .unwrap();
    super::super::CRASH_POINT
        .with(|selected| *selected.borrow_mut() = Some("installed".to_owned()));
    let resumed = recovered.resume_installation(&context()).unwrap();
    // An already installed group may return without a redundant checkpoint.
    // This is still a real process exit with the actual resumed owner live.
    assert_eq!(resumed.phase, GroupPhase::Installed);
    super::super::crash_point("installed");
    panic!("Second actual process exit did not occur");
}

fn initial_child(folder: &Path, point: &str) {
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "repository::profile_install::group::cold::tests::crash_child",
            "--ignored",
            "--nocapture",
        ])
        .env("LX_PROFILE_GROUP_CHILD_DIR", folder)
        .env("LX_PROFILE_GROUP_CHILD_POINT", point)
        .status()
        .unwrap();
    assert_eq!(
        status.code(),
        Some(77),
        "Initial native exit missed {point}"
    );
}
fn second_child(folder: &Path) {
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "repository::profile_install::group::cold::repeat_tests::repeat_crash_child",
            "--ignored",
            "--nocapture",
        ])
        .env("LX_PROFILE_REPEAT_CHILD_DIR", folder)
        .status()
        .unwrap();
    assert_eq!(status.code(), Some(77), "Second native exit did not occur");
}
#[test]
fn actual_second_process_exit_keeps_every_resume_journal_chain_monotone_and_recoverable() {
    for initial in [
        "captured",
        "original-main-moved",
        "replacement-main-moved",
        "installed",
    ] {
        let folder = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
        initial_child(folder.path(), initial);
        second_child(folder.path());
        let (guard, selection) = select(folder.path(), 223);
        let record = selection.record.as_ref().unwrap();
        assert_eq!(record.phase, GroupPhase::Installed);
        let expected_revision = record.image.as_ref().unwrap().restored_revision;
        let continuity = guard.continuity_token().unwrap();
        let limits = PhysicalRestoreLimits::new(128 * 1024 * 1024).unwrap();
        let recovered =
            PhysicalProfileInstallation::recover_selected(guard, selection, &limits, &context())
                .unwrap();
        let terminal = recovered
            .resume_installation(&context())
            .unwrap()
            .reopen(&context())
            .unwrap()
            .commit(&context())
            .unwrap();
        let repository = terminal.into_repository();
        repository
            .profile_guard
            .verify_continuity(&continuity)
            .unwrap();
        repository.integrity_check().unwrap();
        assert_eq!(repository.revision().unwrap(), expected_revision);
        assert_eq!(limits.stats(), (0, 0));
        assert_eq!(
            std::fs::read_dir(folder.path().join("target").join(".lx-snapshot-staging"))
                .unwrap()
                .count(),
            0,
            "Repeated cold exit at {initial} left an actual private object"
        );
    }
}

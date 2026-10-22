use super::*;

fn context() -> PhysicalRestoreContext {
    PhysicalRestoreContext::new(Duration::from_secs(30)).unwrap()
}
fn operation_folder() -> tempfile::TempDir {
    tempfile::tempdir_in(std::env::temp_dir()).unwrap()
}
#[test]
#[ignore = "Real process-exit helper invoked by terminal cold matrix"]
fn crash_child() {
    let folder = PathBuf::from(std::env::var_os("LX_PROFILE_TERMINAL_CHILD_DIR").unwrap());
    let point = std::env::var("LX_PROFILE_TERMINAL_CHILD_POINT").unwrap();
    let installed = std::env::var("LX_PROFILE_TERMINAL_CHILD_INSTALLED").unwrap() == "true";
    let default_wal = std::env::var("LX_PROFILE_TERMINAL_CHILD_WAL").unwrap() == "true";
    super::super::super::CRASH_POINT.with(|selected| *selected.borrow_mut() = Some(point));
    let (source, _) = super::super::super::super::tests::image(&folder);
    let target = Arc::new(Repository::open(folder.join("target")).unwrap());
    target.record_profile_change(&"f".repeat(64)).unwrap();
    std::fs::write(
        folder.join("target").join(STORE_NAME),
        br#"{"source":"current-approved-source"}"#,
    )
    .unwrap();
    let limits = PhysicalRestoreLimits::new(128 * 1024 * 1024).unwrap();
    let context = context();
    let prepared = target
        .prepare_physical_restore(
            super::super::super::super::tests::request(&target),
            PhysicalRestoreSelection::open_selected(source).unwrap(),
            &context,
            &limits,
            &|_| Ok(()),
        )
        .unwrap();
    let guard = target.close_into_guard().unwrap();
    let mut owner = prepared.install_files(guard, &context).unwrap();
    if !installed {
        owner = owner.rollback(&context).unwrap();
    }
    owner = owner.reopen(&context).unwrap();
    if default_wal {
        // Force an actual live WAL/SHM pair while the reopened writer owns it.
        owner
            .repository()
            .unwrap()
            .record_profile_change(&"d".repeat(64))
            .unwrap();
    } else {
        // This matrix deliberately isolates the no-sidecar terminal bound.
        owner
            .repository()
            .unwrap()
            .connection
            .lock()
            .unwrap()
            .pragma_update(None, "journal_mode", "DELETE")
            .unwrap();
    }
    if installed {
        owner.commit(&context).unwrap();
    } else {
        owner.finish_rollback(&context).unwrap();
    }
    panic!("Actual terminal cleanup crash point was not reached");
}
fn run_crash(point: &str, installed: bool, default_wal: bool) -> tempfile::TempDir {
    let folder = operation_folder();
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "repository::profile_install::group::cold::terminal::tests::crash_child",
            "--ignored",
            "--nocapture",
        ])
        .env("LX_PROFILE_TERMINAL_CHILD_DIR", folder.path())
        .env("LX_PROFILE_TERMINAL_CHILD_POINT", point)
        .env("LX_PROFILE_TERMINAL_CHILD_INSTALLED", installed.to_string())
        .env("LX_PROFILE_TERMINAL_CHILD_WAL", default_wal.to_string())
        .status()
        .unwrap();
    assert_eq!(
        status.code(),
        Some(77),
        "Actual child did not terminate at {point}"
    );
    folder
}
fn manifest(folder: &Path) -> PathBuf {
    std::fs::read_dir(folder.join("target").join(".lx-snapshot-staging"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| {
            path.extension()
                .is_some_and(|extension| extension == "manifest")
        })
        .unwrap()
}
fn select(
    folder: &Path,
    limits: &PhysicalRestoreLimits,
) -> (ProfileGuard, PhysicalProfileTerminalSelection) {
    let guard = ProfileGuard::acquire(folder.join("target")).unwrap();
    let mut selection = PhysicalProfileTerminalSelection::open_selected(
        &guard,
        manifest(folder),
        123,
        limits,
        &context(),
    )
    .unwrap();
    assert_eq!(
        limits.stats().0,
        1,
        "Old charge must exist before fresh approval"
    );
    selection
        .approve_current_content(&selection.preview().unwrap().approval_sha256.clone())
        .unwrap();
    (guard, selection)
}
fn reject_foreign_creation_witness(kind: &str, field: &str) {
    let folder = run_crash("terminal-group-journal-removed", true, false);
    let target = folder.path().join("target");
    let main = std::fs::read(target.join(DATABASE_NAME)).unwrap();
    let store = std::fs::read(target.join(STORE_NAME)).unwrap();
    let guard = ProfileGuard::acquire(&target).unwrap();
    let path = manifest(folder.path());
    let private = SnapshotDirectory::open_selected(path.parent().unwrap(), 131).unwrap();
    let name = path.file_name().unwrap().to_str().unwrap();
    let source = SnapshotDirectory::pin_profile(folder.path().join("source"), 131).unwrap();
    let foreign = if field == "target" {
        source.identity().clone()
    } else {
        let mut file = Some(source.open_group_file(DATABASE_NAME, false).unwrap());
        let identity = source.group_file_identity(file.as_ref().unwrap()).unwrap();
        close_owned(&mut file).unwrap();
        identity
    };
    let mut actual = Some(private.open_group_file(name, false).unwrap());
    let identity = private
        .group_file_identity(actual.as_ref().unwrap())
        .unwrap();
    let mut bytes = Vec::new();
    actual.as_mut().unwrap().read_to_end(&mut bytes).unwrap();
    let facts = parse_phase_facts(&bytes).unwrap();
    let mut rewritten = Vec::new();
    let mut changed = 0;
    for fact in facts {
        let mut body = fact.body;
        if fact.checkpoint {
            let mut value: serde_json::Value = serde_json::from_slice(&body).unwrap();
            if value.get("kind").and_then(|value| value.as_str()) == Some(kind) {
                let recorded: FileIdentity = serde_json::from_value(value[field].clone()).unwrap();
                assert_ne!(foreign, recorded);
                value[field] = serde_json::to_value(&foreign).unwrap();
                body = serde_json::to_vec(&value).unwrap();
                changed += 1;
            }
        }
        let mut payload = vec![fact.phase as u8 | if fact.checkpoint { 0x80 } else { 0 }];
        payload.extend_from_slice(&body);
        rewritten.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        rewritten.extend_from_slice(&Sha256::digest(&payload));
        rewritten.extend_from_slice(&payload);
    }
    assert_eq!(changed, 1);
    let file = actual.as_mut().unwrap();
    file.set_len(0).unwrap();
    file.seek(SeekFrom::Start(0)).unwrap();
    file.write_all(&rewritten).unwrap();
    file.sync_all().unwrap();
    assert_eq!(
        private.group_file_identity(file).unwrap(),
        identity,
        "The forged witness must retain the same actual manifest object"
    );
    close_owned(&mut actual).unwrap();
    let limits = PhysicalRestoreLimits::new(128 * 1024 * 1024).unwrap();
    let failure =
        PhysicalProfileTerminalSelection::open_selected(&guard, &path, 132, &limits, &context())
            .unwrap_err();
    assert!(
        failure.error.to_string().contains("Group creation"),
        "{}",
        failure.error
    );
    assert!(failure.owner.as_ref().unwrap().preview().is_none());
    assert_eq!(std::fs::read(target.join(DATABASE_NAME)).unwrap(), main);
    assert_eq!(std::fs::read(target.join(STORE_NAME)).unwrap(), store);
    assert_eq!(std::fs::read(&path).unwrap(), rewritten);
    assert!(ProfileGuard::acquire(&target).is_err());
    let charged = limits.stats();
    assert_eq!(charged.0, 1);
    assert!(charged.1 > 0);
    drop(failure);
    assert_eq!(limits.stats(), charged);
}
#[test]
fn actual_terminal_manifest_rejects_foreign_creation_target_and_retains_charge() {
    reject_foreign_creation_witness("profile_group_allocation_intent", "target");
}
#[test]
fn actual_terminal_manifest_rejects_foreign_allocated_fid_and_retains_charge() {
    reject_foreign_creation_witness("profile_group_allocated", "identity");
}
#[test]
fn actual_terminal_no_sidecar_crash_matrix_recovers_both_directions_with_same_guard() {
    for installed in [true, false] {
        for point in [
            "terminal-group-journal-pending",
            "terminal-group-journal-removed",
            "terminal-prepare-file-pending",
            "terminal-prepare-file-removed",
        ] {
            let folder = run_crash(point, installed, false);
            let limits = PhysicalRestoreLimits::new(128 * 1024 * 1024).unwrap();
            let (guard, selection) = select(folder.path(), &limits);
            let expected_revision = if installed {
                selection
                    .group
                    .record
                    .as_ref()
                    .unwrap()
                    .image
                    .as_ref()
                    .unwrap()
                    .restored_revision
            } else {
                1
            };
            let continuity = guard.continuity_token().unwrap();
            let recovered =
                PhysicalProfileTerminalRecovery::recover_selected(guard, selection, &context())
                    .unwrap();
            assert!(ProfileGuard::acquire(folder.path().join("target")).is_err());
            let terminal = recovered.finish(&context()).unwrap();
            assert_eq!(terminal.installed(), installed);
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
                0
            );
        }
    }
}
#[test]
fn default_reopened_wal_and_shm_stay_actual_and_charged_after_terminal_crash() {
    let folder = run_crash("terminal-group-journal-removed", true, true);
    let target = folder.path().join("target");
    let wal = std::fs::read(target.join("native-library.sqlite3-wal")).unwrap();
    let shm = std::fs::read(target.join("native-library.sqlite3-shm")).unwrap();
    let limits = PhysicalRestoreLimits::new(128 * 1024 * 1024).unwrap();
    let (guard, selection) = select(folder.path(), &limits);
    let failure = PhysicalProfileTerminalRecovery::recover_selected(guard, selection, &context())
        .unwrap()
        .finish(&context())
        .unwrap_err();
    assert!(failure.error.to_string().contains("unowned sidecar"));
    assert!(ProfileGuard::acquire(&target).is_err());
    assert_eq!(
        std::fs::read(target.join("native-library.sqlite3-wal")).unwrap(),
        wal
    );
    assert_eq!(
        std::fs::read(target.join("native-library.sqlite3-shm")).unwrap(),
        shm
    );
    let charged = limits.stats();
    assert_eq!(charged.0, 1);
    assert!(charged.1 > 0);
    drop(failure);
    assert_eq!(
        limits.stats(),
        charged,
        "Selection/owner Drop cannot refund old physical liability"
    );
}
#[test]
fn actual_terminal_selection_reconstructs_old_charge_over_capacity_once_and_blocks_new_work() {
    let folder = run_crash("terminal-group-journal-removed", true, false);
    let limits = PhysicalRestoreLimits::new(1).unwrap();
    let guard = ProfileGuard::acquire(folder.path().join("target")).unwrap();
    let selection = PhysicalProfileTerminalSelection::open_selected(
        &guard,
        manifest(folder.path()),
        127,
        &limits,
        &context(),
    )
    .unwrap();
    let request = selection.group.record.as_ref().unwrap().request.clone();
    let root = selection.group.target.identity().clone();
    let charged = limits.stats();
    assert_eq!(
        charged,
        (1, request.maximum_image_bytes * 3 + 65 * 1024 * 1024)
    );
    drop(selection);
    assert_eq!(limits.stats(), charged);
    let mut fresh = request;
    fresh.operation_id = "fresh-after-discovered-liability".to_owned();
    assert!(limits.reserve(&fresh, &root).is_err());
    let mut selection = PhysicalProfileTerminalSelection::open_selected(
        &guard,
        manifest(folder.path()),
        128,
        &limits,
        &context(),
    )
    .unwrap();
    assert_eq!(
        limits.stats(),
        charged,
        "Reselection must not duplicate the same old liability"
    );
    selection
        .approve_current_content(&selection.preview().unwrap().approval_sha256.clone())
        .unwrap();
    PhysicalProfileTerminalRecovery::recover_selected(guard, selection, &context())
        .unwrap()
        .finish(&context())
        .unwrap();
    assert_eq!(limits.stats(), (0, 0));
}
#[test]
fn terminal_manifest_native_close_failure_keeps_same_handle_and_same_guard_for_retry() {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::{HANDLE_FLAG_PROTECT_FROM_CLOSE, SetHandleInformation};
    let folder = run_crash("terminal-group-journal-removed", true, false);
    let limits = PhysicalRestoreLimits::new(128 * 1024 * 1024).unwrap();
    let (guard, selection) = select(folder.path(), &limits);
    let continuity = guard.continuity_token().unwrap();
    let raw = selection.manifest.as_ref().unwrap().as_raw_handle();
    assert_ne!(
        unsafe {
            SetHandleInformation(
                raw,
                HANDLE_FLAG_PROTECT_FROM_CLOSE,
                HANDLE_FLAG_PROTECT_FROM_CLOSE,
            )
        },
        0
    );
    let failure = PhysicalProfileTerminalRecovery::recover_selected(guard, selection, &context())
        .unwrap_err();
    let PhysicalProfileTerminalRecoveryOwner::Recovery(owner) = failure.owner else {
        panic!("Actual manifest owner was lost");
    };
    assert_eq!(
        owner.manifest_inspection.as_ref().unwrap().as_raw_handle(),
        raw
    );
    assert!(ProfileGuard::acquire(folder.path().join("target")).is_err());
    assert_ne!(
        unsafe { SetHandleInformation(raw, HANDLE_FLAG_PROTECT_FROM_CLOSE, 0) },
        0
    );
    let repository = owner.finish(&context()).unwrap().into_repository();
    repository
        .profile_guard
        .verify_continuity(&continuity)
        .unwrap();
    assert_eq!(limits.stats(), (0, 0));
}
#[test]
fn terminal_selection_rejects_a_reacquired_guard_and_keeps_its_old_charge() {
    let folder = run_crash("terminal-group-journal-removed", true, false);
    let limits = PhysicalRestoreLimits::new(128 * 1024 * 1024).unwrap();
    let (guard, selection) = select(folder.path(), &limits);
    drop(guard);
    let guard = ProfileGuard::acquire(folder.path().join("target")).unwrap();
    let charged = limits.stats();
    let failure = PhysicalProfileTerminalRecovery::recover_selected(guard, selection, &context())
        .unwrap_err();
    assert!(
        failure
            .error
            .to_string()
            .contains("released and reacquired")
    );
    assert!(matches!(
        failure.owner,
        PhysicalProfileTerminalRecoveryOwner::Admission(_)
    ));
    assert!(ProfileGuard::acquire(folder.path().join("target")).is_err());
    assert_eq!(limits.stats(), charged);
}
#[test]
fn partial_native_close_retry_rejects_changed_same_fid_body_under_old_approval() {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::{HANDLE_FLAG_PROTECT_FROM_CLOSE, SetHandleInformation};
    let folder = run_crash("terminal-group-journal-removed", true, false);
    let limits = PhysicalRestoreLimits::new(128 * 1024 * 1024).unwrap();
    let (guard, selection) = select(folder.path(), &limits);
    let recovered =
        PhysicalProfileTerminalRecovery::recover_selected(guard, selection, &context()).unwrap();
    let raw = recovered
        .installation
        .replacements
        .iter()
        .find(|file| file.role == "store")
        .unwrap()
        .file
        .as_ref()
        .unwrap()
        .as_raw_handle();
    assert_ne!(
        unsafe {
            SetHandleInformation(
                raw,
                HANDLE_FLAG_PROTECT_FROM_CLOSE,
                HANDLE_FLAG_PROTECT_FROM_CLOSE,
            )
        },
        0
    );
    let failure = recovered.finish(&context()).unwrap_err();
    let PhysicalProfileTerminalRecoveryOwner::Recovery(owner) = failure.owner else {
        panic!("Partial-close owner was lost");
    };
    assert!(
        owner
            .installation
            .replacements
            .iter()
            .find(|file| file.role == "main")
            .unwrap()
            .file
            .is_none()
    );
    let main = folder.path().join("target").join(DATABASE_NAME);
    let mut bytes = std::fs::read(&main).unwrap();
    bytes[24..28].copy_from_slice(&12345u32.to_be_bytes());
    std::fs::write(&main, &bytes).unwrap();
    assert_ne!(
        unsafe { SetHandleInformation(raw, HANDLE_FLAG_PROTECT_FROM_CLOSE, 0) },
        0
    );
    let charged = limits.stats();
    let retry = owner.finish(&context()).unwrap_err();
    assert!(
        retry
            .error
            .to_string()
            .contains("body changed after approval")
    );
    assert!(ProfileGuard::acquire(folder.path().join("target")).is_err());
    assert_eq!(std::fs::read(&main).unwrap(), bytes);
    assert_eq!(limits.stats(), charged);
}
#[test]
fn contradictory_removed_preparation_fact_keeps_actual_file_and_charge() {
    let folder = run_crash("terminal-group-journal-removed", true, false);
    let path = manifest(folder.path());
    let facts = parse_phase_facts(&std::fs::read(&path).unwrap()).unwrap();
    let metadata: ActualFileMetadata = facts
        .iter()
        .filter(|fact| fact.checkpoint)
        .find_map(|fact| {
            let body: serde_json::Value = serde_json::from_slice(&fact.body).unwrap();
            if body["kind"] == "allocation" && !body["file"]["identity"].is_null() {
                Some(serde_json::from_value(body["file"].clone()).unwrap())
            } else {
                None
            }
        })
        .unwrap();
    let private_file = path
        .parent()
        .unwrap()
        .join(metadata.temporary_name.as_ref().unwrap());
    let actual = std::fs::read(&private_file).unwrap();
    let guard = ProfileGuard::acquire(folder.path().join("target")).unwrap();
    let body = serde_json::json!({"kind":"cleanup", "fact":{
        "file":metadata, "stamp":{"changed":0,"written":0,"length":actual.len()},
        "sha256":format!("{:x}",Sha256::digest(&actual)), "pending_zero_links":false,"removed":true
    }});
    let mut payload = vec![SnapshotPhase::CompactVerified as u8 | 0x80];
    payload.extend_from_slice(&serde_json::to_vec(&body).unwrap());
    let mut append = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap();
    append
        .write_all(&(payload.len() as u32).to_le_bytes())
        .unwrap();
    append.write_all(&Sha256::digest(&payload)).unwrap();
    append.write_all(&payload).unwrap();
    append.sync_all().unwrap();
    drop(append);
    let limits = PhysicalRestoreLimits::new(128 * 1024 * 1024).unwrap();
    let failure =
        PhysicalProfileTerminalSelection::open_selected(&guard, &path, 130, &limits, &context())
            .unwrap_err();
    assert!(
        failure
            .error
            .to_string()
            .contains("zero-link disposition witness")
    );
    assert_eq!(std::fs::read(&private_file).unwrap(), actual);
    assert_eq!(limits.stats().0, 1);
    drop(failure);
    assert_eq!(limits.stats().0, 1);
}
#[test]
fn foreign_group_journal_leaf_remains_unchanged_and_retains_old_charge() {
    let folder = run_crash("terminal-group-journal-removed", true, false);
    let path = manifest(folder.path());
    let facts = parse_phase_facts(&std::fs::read(&path).unwrap()).unwrap();
    let cleanups = latest_cleanups(&facts).unwrap();
    let name = cleanups
        .keys()
        .find(|name| journal_nonce(name).is_ok())
        .unwrap();
    let foreign = path.parent().unwrap().join(name);
    std::fs::write(&foreign, b"foreign actual journal occupant").unwrap();
    let guard = ProfileGuard::acquire(folder.path().join("target")).unwrap();
    let limits = PhysicalRestoreLimits::new(128 * 1024 * 1024).unwrap();
    let failure =
        PhysicalProfileTerminalSelection::open_selected(&guard, &path, 124, &limits, &context())
            .unwrap_err();
    assert!(
        !failure
            .owner
            .as_ref()
            .unwrap()
            .group
            .obstructions
            .is_empty()
    );
    assert_eq!(
        std::fs::read(&foreign).unwrap(),
        b"foreign actual journal occupant"
    );
    let charged = limits.stats();
    drop(failure);
    assert_eq!(limits.stats(), charged);
    assert_eq!(charged.0, 1);
}
#[test]
fn unregistered_preparation_allocation_stays_actual_and_charged_before_approval() {
    let folder = run_crash("terminal-group-journal-removed", true, false);
    let path = manifest(folder.path());
    let bytes = std::fs::read(&path).unwrap();
    let facts = parse_phase_facts(&bytes).unwrap();
    let created: serde_json::Value = serde_json::from_slice(&facts.first().unwrap().body).unwrap();
    let request: PhysicalRestoreRequest =
        serde_json::from_value(created["restore_request"].clone()).unwrap();
    let guard = ProfileGuard::acquire(folder.path().join("target")).unwrap();
    let private = SnapshotDirectory::open_selected(path.parent().unwrap(), 125).unwrap();
    let nonce = "a1".repeat(16);
    let unknown = private.path().join(format!(".lx-snapshot-{nonce}.part"));
    std::fs::write(&unknown, b"unregistered actual allocation").unwrap();
    let metadata = ActualFileMetadata {
        operation_id: request.operation_id,
        nonce: nonce.clone(),
        temporary_name: Some(format!(".lx-snapshot-{nonce}.part")),
        directory: private.path().to_owned(),
        directory_identity: private.identity().clone(),
        selection_epoch: 125,
        identity: None,
        bytes: 0,
        published: false,
        final_basename: None,
        cleanup_probe: None,
    };
    let mut body = vec![SnapshotPhase::CompactVerified as u8 | 0x80];
    body.extend_from_slice(
        &serde_json::to_vec(
            &serde_json::json!({"kind":"allocation","role":"auxiliary","file":metadata}),
        )
        .unwrap(),
    );
    let mut append = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap();
    append
        .write_all(&(body.len() as u32).to_le_bytes())
        .unwrap();
    append.write_all(&Sha256::digest(&body)).unwrap();
    append.write_all(&body).unwrap();
    append.sync_all().unwrap();
    drop(append);
    let limits = PhysicalRestoreLimits::new(128 * 1024 * 1024).unwrap();
    let failure =
        PhysicalProfileTerminalSelection::open_selected(&guard, &path, 126, &limits, &context())
            .unwrap_err();
    assert!(failure.error.to_string().contains("durable actual FID"));
    assert!(failure.owner.as_ref().unwrap().preview().is_none());
    assert_eq!(
        std::fs::read(&unknown).unwrap(),
        b"unregistered actual allocation"
    );
    let charged = limits.stats();
    drop(failure);
    assert_eq!(limits.stats(), charged);
    assert_eq!(charged.0, 1);
}

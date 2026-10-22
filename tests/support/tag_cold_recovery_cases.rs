use super::*;
use std::{
    process::{Command, Stdio},
    time::Instant,
};

fn context() -> ScanContext {
    ScanContext::new(Duration::from_secs(30))
}
fn patch() -> LocalTagPatch {
    LocalTagPatch {
        title: TagField::Set("Actual same-handle title".into()),
        artists: TagField::Set(vec!["Actual artist".into()]),
        album: TagField::Set("Actual album".into()),
        ..Default::default()
    }
}
fn owned_failure(error: anyhow::Error) -> TagOwnedInstallation {
    match error
        .downcast::<TagInstallError>()
        .expect("Native failure must retain its owned installation")
    {
        TagInstallError::Owned { owner, .. } => *owner,
        _ => panic!("Cold recovery dropped its native owner"),
    }
}
fn child(profile: &std::path::Path, operation: &str, mode: &str, role: &str) -> i32 {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args([
            "--exact",
            "cold_recovery_cases::cold_process_helper",
            "--nocapture",
        ])
        .env("LX_TAG_COLD_PROFILE", profile)
        .env("LX_TAG_COLD_OPERATION", operation)
        .env("LX_TAG_COLD_MODE", mode)
        .env("LX_TAG_COLD_ROLE", role)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    use std::os::windows::process::CommandExt;
    command.creation_flags(windows_sys::Win32::System::Threading::CREATE_NO_WINDOW);
    let mut child = command.spawn().unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status.code().unwrap();
        }
        assert!(
            Instant::now() < deadline,
            "Isolated cold retirement helper did not stop"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}
fn recovery(repository: &Arc<Repository>, token: &TagEditToken) -> RepositoryTagEditCoordinator {
    let operation = repository
        .tag_edit_operation(&token.operation_id)
        .unwrap()
        .unwrap();
    let mut current = token.selection.clone();
    if let Some(commit) = operation.commit.as_ref().or(operation.restoration.as_ref()) {
        current.expected_binding_revision = commit.receipt.committed_binding_revision.unwrap();
    }
    current.expected_body_sha256 = Some(format!(
        "{:x}",
        Sha256::digest(fs::read(&current.path).unwrap())
    ));
    let approval = repository
        .approve_tag_recovery(
            "cold-current-explicit-approval",
            token,
            &current,
            &|| Ok(()),
        )
        .unwrap();
    RepositoryTagEditCoordinator::for_recovery(
        repository.clone(),
        token.clone(),
        approval.approval_id,
        approval.approval_epoch,
        Arc::new(|| Ok(())),
        Arc::new(|| Ok(())),
    )
    .unwrap()
}
fn journal(operation: &lx_gpui_player::repository::TagEditOperation) -> PathBuf {
    operation
        .artifacts
        .iter()
        .find(|slot| slot.role == "journal")
        .unwrap()
        .path
        .clone()
}

#[test]
fn cold_process_helper() {
    let Some(profile) = std::env::var_os("LX_TAG_COLD_PROFILE") else {
        return;
    };
    let operation = std::env::var("LX_TAG_COLD_OPERATION").unwrap();
    let mode = std::env::var("LX_TAG_COLD_MODE").unwrap();
    let role = std::env::var("LX_TAG_COLD_ROLE").unwrap();
    assert!(["backup", "candidate", "journal"].contains(&role.as_str()));
    let repository = Arc::new(Repository::open(&profile).unwrap());
    let token = repository
        .tag_edit_operation(&operation)
        .unwrap()
        .unwrap()
        .token;
    let unstarted = TagOwnedInstallation::unstarted(token.selection.clone());
    if mode == "setup" {
        let db = rusqlite::Connection::open(repository.path()).unwrap();
        db.execute_batch("ALTER TABLE tag_edit_artifacts RENAME TO retained_tag_artifacts;")
            .unwrap();
        assert!(
            RepositoryTagEditCoordinator::new(
                repository.clone(),
                token.clone(),
                Arc::new(|| Ok(())),
                Arc::new(|| Ok(()))
            )
            .is_err()
        );
        assert!(unstarted.is_unstarted());
        assert!(!unstarted.has_live_handles());
        assert!(unstarted.has_retirement_obligation());
        let count: i64 = db
            .query_row(
                "SELECT count(*) FROM retained_tag_artifacts WHERE operation_id=?1",
                [&operation],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 3);
        std::process::exit(76);
    }
    let mut coordinator = RepositoryTagEditCoordinator::new(
        repository.clone(),
        token.clone(),
        Arc::new(|| Ok(())),
        Arc::new(|| Ok(())),
    )
    .unwrap();
    let directory = coordinator.journal_directory().unwrap();
    fs::create_dir_all(&directory).unwrap();
    let mut owner = unstarted
        .start_selected(&directory, &patch(), &context(), &mut coordinator)
        .unwrap();
    let db = rusqlite::Connection::open(repository.path()).unwrap();
    if mode == "deleted" {
        db.execute_batch(&format!("CREATE TRIGGER fail_cold_ack BEFORE UPDATE ON tag_edit_artifacts WHEN NEW.phase='retired' AND OLD.role='{role}' BEGIN SELECT RAISE(ABORT,'actual deleted object ACK stop'); END;")).unwrap();
    } else {
        assert_eq!(mode, "original_close");
        db.execute_batch("CREATE TRIGGER fail_cold_original_ack BEFORE UPDATE ON tag_edit_operations WHEN json_extract(NEW.document,'$.original_owner_retired')=1 BEGIN SELECT RAISE(ABORT,'actual original close ACK stop'); END;").unwrap();
    }
    assert!(owner.retire(&context(), &mut coordinator).is_err());
    let pending = repository.tag_edit_operation(&operation).unwrap().unwrap();
    if mode == "deleted" {
        let artifact = pending
            .artifacts
            .iter()
            .find(|slot| slot.role == role)
            .unwrap();
        assert_eq!(
            artifact.phase,
            lx_gpui_player::repository::TagEditArtifactPhase::Retiring
        );
        assert!(!artifact.path.exists());
        assert!(artifact.identity.is_some());
        assert!(pending.original_owner_retired);
        std::process::exit(75);
    }
    assert!(!pending.original_owner_retired);
    assert_eq!(pending.phase, TagEditPhase::Published);
    assert!(pending.original_retirement_intent.is_some());
    let file = fs::OpenOptions::new()
        .write(true)
        .open(&token.selection.path)
        .unwrap();
    drop(file);
    std::process::exit(77);
}

#[test]
fn actual_deleted_and_closed_each_role_before_sql_ack_recovers_exact_fid_absence_after_restart() {
    for role in ["backup", "candidate", "journal"] {
        let fixture = Fixture::new(&format!("cold-deleted-{role}"));
        let token = fixture.begin();
        let budget = fixture.budget();
        let Fixture {
            repository,
            profile,
            _media,
            selection,
            ..
        } = fixture;
        drop(repository);
        assert_eq!(
            child(profile.path(), &token.operation_id, "deleted", role),
            75
        );
        let repository = Arc::new(Repository::open(profile.path()).unwrap());
        let pending = repository
            .tag_edit_operation(&token.operation_id)
            .unwrap()
            .unwrap();
        let artifact = pending
            .artifacts
            .iter()
            .find(|slot| slot.role == role)
            .unwrap();
        assert!(!artifact.path.exists());
        assert_eq!(
            artifact.phase,
            lx_gpui_player::repository::TagEditArtifactPhase::Retiring
        );
        let expected_charge = pending
            .artifacts
            .iter()
            .filter(|slot| slot.phase != lx_gpui_player::repository::TagEditArtifactPhase::Retired)
            .map(|slot| slot.maximum_bytes)
            .sum::<u64>();
        assert_eq!(
            repository.tag_edit_quota().unwrap().charged_bytes,
            expected_charge
        );
        let mut old_permission = RepositoryTagEditCoordinator::new(
            repository.clone(),
            token.clone(),
            Arc::new(|| Ok(())),
            Arc::new(|| Ok(())),
        )
        .unwrap();
        let mut rejected = owned_failure(
            recover_selected_tags_owned(
                &selection,
                &journal(&pending),
                &context(),
                &mut old_permission,
            )
            .unwrap_err(),
        );
        rejected.retire(&context(), &mut old_permission).unwrap();
        assert_eq!(
            repository.tag_edit_quota().unwrap().charged_bytes,
            expected_charge
        );
        let replacement = (role == "backup").then(|| artifact.path.clone());
        if let Some(path) = &replacement {
            fs::write(path, b"foreign same-name object").unwrap();
        }
        let db = rusqlite::Connection::open(repository.path()).unwrap();
        db.execute_batch("DROP TRIGGER fail_cold_ack;").unwrap();
        let mut coordinator = recovery(&repository, &token);
        let mut owner = recover_selected_tags_owned(
            &selection,
            &journal(&pending),
            &context(),
            &mut coordinator,
        )
        .unwrap();
        assert!(owner.is_retirement_only());
        owner.retire(&context(), &mut coordinator).unwrap();
        assert!(owner.is_retired());
        assert!(!owner.has_live_handles());
        let completed = repository
            .tag_edit_operation(&token.operation_id)
            .unwrap()
            .unwrap();
        assert!(completed.is_finished());
        assert_eq!(repository.tag_edit_quota().unwrap().charged_bytes, 0);
        assert_eq!(repository.tag_edit_quota().unwrap().pending_operations, 0);
        if let Some(path) = replacement {
            assert_eq!(fs::read(path).unwrap(), b"foreign same-name object");
        }
        assert_eq!(
            format!("{:x}", Sha256::digest(fs::read(&selection.path).unwrap())),
            completed.commit.as_ref().unwrap().receipt.new_body_sha256
        );
        let revision = completed.ledger_revision;
        owner.retire(&context(), &mut coordinator).unwrap();
        assert_eq!(
            repository
                .tag_edit_operation(&token.operation_id)
                .unwrap()
                .unwrap()
                .ledger_revision,
            revision
        );
        assert!(budget.total().unwrap() > expected_charge);
    }
}

#[test]
fn actual_coordinator_sql_setup_failure_before_native_admission_is_recoverable_after_restart() {
    let fixture = Fixture::new("cold-setup-no-open");
    let token = fixture.begin();
    let budget = fixture.budget();
    let db_path = fixture.repository.path().to_path_buf();
    let Fixture {
        repository,
        profile,
        _media,
        selection,
        old_body,
    } = fixture;
    drop(repository);
    assert_eq!(
        child(profile.path(), &token.operation_id, "setup", "backup"),
        76
    );
    let db = rusqlite::Connection::open(&db_path).unwrap();
    db.execute_batch("ALTER TABLE retained_tag_artifacts RENAME TO tag_edit_artifacts;")
        .unwrap();
    let integrity: String = db
        .query_row("PRAGMA integrity_check", [], |row| row.get(0))
        .unwrap();
    assert_eq!(integrity, "ok");
    drop(db);
    let repository = Arc::new(Repository::open(profile.path()).unwrap());
    let pending = repository
        .tag_edit_operation(&token.operation_id)
        .unwrap()
        .unwrap();
    assert_eq!(pending.installation_attempted, Some(false));
    assert!(repository.tag_recovery_unopened(&token).unwrap());
    assert_eq!(
        repository.tag_edit_quota().unwrap().charged_bytes,
        budget.total().unwrap()
    );
    assert!(pending.artifacts.iter().all(|slot| !slot.path.exists()));
    let mut coordinator = recovery(&repository, &token);
    let mut owner =
        recover_selected_tags_owned(&selection, &journal(&pending), &context(), &mut coordinator)
            .unwrap();
    assert!(owner.is_unstarted());
    assert!(!owner.has_live_handles());
    owner.retire(&context(), &mut coordinator).unwrap();
    let done = repository
        .tag_edit_operation(&token.operation_id)
        .unwrap()
        .unwrap();
    assert!(done.is_finished() && done.original_never_opened);
    assert!(done.artifacts.iter().all(|slot| slot.never_allocated));
    assert_eq!(repository.tag_edit_quota().unwrap().charged_bytes, 0);
    assert_eq!(fs::read(selection.path).unwrap(), old_body);
}

#[test]
fn legacy_original_close_without_preclose_intent_rejects_new_writer_substitution_and_keeps_charge()
{
    let fixture = Fixture::new("cold-original-close-gap");
    let token = fixture.begin();
    let budget = fixture.budget();
    let Fixture {
        repository,
        profile,
        _media,
        selection,
        ..
    } = fixture;
    drop(repository);
    assert_eq!(
        child(
            profile.path(),
            &token.operation_id,
            "original_close",
            "backup"
        ),
        77
    );
    let repository = Arc::new(Repository::open(profile.path()).unwrap());
    let pending = repository
        .tag_edit_operation(&token.operation_id)
        .unwrap()
        .unwrap();
    let db = rusqlite::Connection::open(repository.path()).unwrap();
    db.execute_batch("DROP TRIGGER fail_cold_original_ack;")
        .unwrap();
    db.execute("UPDATE tag_edit_operations SET document=json_remove(document,'$.original_retirement_intent'),byte_size=length(CAST(json_remove(document,'$.original_retirement_intent') AS BLOB)) WHERE operation_id=?1", [&token.operation_id]).unwrap();
    let mut coordinator = recovery(&repository, &token);
    let mut failed = owned_failure(
        recover_selected_tags_owned(&selection, &journal(&pending), &context(), &mut coordinator)
            .unwrap_err(),
    );
    assert!(failed.is_retirement_only());
    assert!(!failed.has_live_handles());
    failed.retire(&context(), &mut coordinator).unwrap();
    assert!(failed.is_retired());
    assert_eq!(
        repository.tag_edit_quota().unwrap().charged_bytes,
        budget.total().unwrap()
    );
    let preserved = repository
        .tag_edit_operation(&token.operation_id)
        .unwrap()
        .unwrap();
    assert!(!preserved.original_owner_retired && !preserved.is_finished());
    assert_eq!(preserved.ledger_revision, pending.ledger_revision);
}

#[test]
fn unstarted_placeholder_and_unknown_legacy_attempt_cannot_refund_an_attempted_admission() {
    let fixture = Fixture::new("cold-attempt-fence");
    let token = fixture.begin();
    fixture
        .repository
        .prepare_tag_installation_open(&token, &|| Ok(()))
        .unwrap();
    let mut coordinator = fixture.coordinator(token.clone());
    let mut false_placeholder = TagOwnedInstallation::unstarted(token.selection.clone());
    assert!(
        false_placeholder
            .retire(&context(), &mut coordinator)
            .is_err()
    );
    assert_eq!(
        fixture.repository.tag_edit_quota().unwrap().charged_bytes,
        fixture.budget().total().unwrap()
    );
    let db = rusqlite::Connection::open(fixture.repository.path()).unwrap();
    db.execute("UPDATE tag_edit_operations SET document=json_remove(document,'$.installation_attempted'),byte_size=length(CAST(json_remove(document,'$.installation_attempted') AS BLOB)) WHERE operation_id=?1", [&token.operation_id]).unwrap();
    assert!(!fixture.repository.tag_recovery_unopened(&token).unwrap());
    assert_eq!(
        fixture
            .repository
            .tag_edit_operation(&token.operation_id)
            .unwrap()
            .unwrap()
            .installation_attempted,
        None
    );
    let mut recovery = recovery(&fixture.repository, &token);
    let pending = fixture
        .repository
        .tag_edit_operation(&token.operation_id)
        .unwrap()
        .unwrap();
    let mut failed = owned_failure(
        recover_selected_tags_owned(
            &token.selection,
            &journal(&pending),
            &context(),
            &mut recovery,
        )
        .unwrap_err(),
    );
    failed.retire(&context(), &mut recovery).unwrap();
    assert_eq!(
        fixture.repository.tag_edit_quota().unwrap().charged_bytes,
        fixture.budget().total().unwrap()
    );
    assert!(
        !fixture
            .repository
            .tag_edit_operation(&token.operation_id)
            .unwrap()
            .unwrap()
            .original_never_opened
    );
    assert_eq!(fs::read(fixture.selection.path).unwrap(), fixture.old_body);
}

#[test]
fn actual_preclose_intent_recovers_distinct_original_retirement_receipt_after_crash_and_ack_failure()
 {
    for fail_ack in [false, true] {
        let fixture = Fixture::new(if fail_ack {
            "cold-original-recovery-ack"
        } else {
            "cold-original-recovery"
        });
        let token = fixture.begin();
        let budget = fixture.budget();
        let Fixture {
            repository,
            profile,
            _media,
            selection,
            ..
        } = fixture;
        drop(repository);
        assert_eq!(
            child(
                profile.path(),
                &token.operation_id,
                "original_close",
                "backup"
            ),
            77
        );
        let repository = Arc::new(Repository::open(profile.path()).unwrap());
        let pending = repository
            .tag_edit_operation(&token.operation_id)
            .unwrap()
            .unwrap();
        let intent = pending.original_retirement_intent.clone().unwrap();
        assert!(!pending.original_owner_retired);
        assert!(pending.original_close.is_none() && pending.original_recovery.is_none());
        let db = rusqlite::Connection::open(repository.path()).unwrap();
        db.execute_batch("DROP TRIGGER fail_cold_original_ack;")
            .unwrap();
        if fail_ack {
            db.execute_batch("CREATE TRIGGER fail_original_recovery_ack BEFORE UPDATE ON tag_edit_operations WHEN json_extract(NEW.document,'$.original_recovery') IS NOT NULL BEGIN SELECT RAISE(ABORT,'actual recovered writer exclusion ACK failure'); END;").unwrap();
        }
        let before = fs::read(&selection.path).unwrap();
        let mut coordinator = recovery(&repository, &token);
        let mut owner = recover_selected_tags_owned(
            &selection,
            &journal(&pending),
            &context(),
            &mut coordinator,
        )
        .unwrap();
        assert!(owner.is_retirement_only());
        assert!(owner.has_live_handles());
        if fail_ack {
            assert!(owner.retire(&context(), &mut coordinator).is_err());
            let retained = repository
                .tag_edit_operation(&token.operation_id)
                .unwrap()
                .unwrap();
            assert!(!retained.original_owner_retired);
            assert!(retained.original_recovery.is_none() && retained.original_close.is_none());
            assert_eq!(
                repository.tag_edit_quota().unwrap().charged_bytes,
                budget.total().unwrap()
            );
            assert!(
                fs::OpenOptions::new()
                    .write(true)
                    .open(&selection.path)
                    .is_err()
            );
            db.execute_batch("DROP TRIGGER fail_original_recovery_ack;")
                .unwrap();
        }
        owner.retire(&context(), &mut coordinator).unwrap();
        assert!(owner.is_retired() && !owner.has_live_handles());
        let completed = repository
            .tag_edit_operation(&token.operation_id)
            .unwrap()
            .unwrap();
        assert!(completed.is_finished() && completed.original_owner_retired);
        assert!(completed.original_close.is_none());
        let recovered = completed.original_recovery.as_ref().unwrap();
        assert_eq!(recovered.proof, "same_object_sharing_calibration");
        assert_eq!(recovered.intent, intent);
        assert_eq!(
            recovered.body_sha256,
            completed.commit.as_ref().unwrap().receipt.new_body_sha256
        );
        assert_eq!(repository.tag_edit_quota().unwrap().charged_bytes, 0);
        assert_eq!(fs::read(&selection.path).unwrap(), before);
        let modified = fs::metadata(&selection.path)
            .unwrap()
            .modified()
            .unwrap()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        assert_eq!(
            u64::try_from(modified).unwrap(),
            intent.observation.modified_nanos.unwrap()
        );
        let revision = completed.ledger_revision;
        owner.retire(&context(), &mut coordinator).unwrap();
        assert_eq!(
            repository
                .tag_edit_operation(&token.operation_id)
                .unwrap()
                .unwrap()
                .ledger_revision,
            revision
        );
    }
}

#[test]
fn actual_same_object_exclusive_writer_blocks_cold_original_recovery_without_losing_native_attempt_or_charge()
 {
    let fixture = Fixture::new("cold-original-writer-busy");
    let token = fixture.begin();
    let budget = fixture.budget();
    let Fixture {
        repository,
        profile,
        _media,
        selection,
        ..
    } = fixture;
    drop(repository);
    assert_eq!(
        child(
            profile.path(),
            &token.operation_id,
            "original_close",
            "backup"
        ),
        77
    );
    let repository = Arc::new(Repository::open(profile.path()).unwrap());
    let db = rusqlite::Connection::open(repository.path()).unwrap();
    db.execute_batch("DROP TRIGGER fail_cold_original_ack;")
        .unwrap();
    let pending = repository
        .tag_edit_operation(&token.operation_id)
        .unwrap()
        .unwrap();
    let mut coordinator = recovery(&repository, &token);
    use std::os::windows::fs::OpenOptionsExt;
    let actual_writer = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .share_mode(0)
        .open(&selection.path)
        .unwrap();
    let mut failed = owned_failure(
        recover_selected_tags_owned(&selection, &journal(&pending), &context(), &mut coordinator)
            .unwrap_err(),
    );
    assert!(failed.admission_was_busy());
    failed.retire(&context(), &mut coordinator).unwrap();
    assert!(failed.is_retired() && !failed.has_live_handles());
    assert_eq!(
        repository.tag_edit_quota().unwrap().charged_bytes,
        budget.total().unwrap()
    );
    let retained = repository
        .tag_edit_operation(&token.operation_id)
        .unwrap()
        .unwrap();
    assert!(!retained.original_owner_retired && retained.original_recovery.is_none());
    drop(actual_writer);
    let mut retry =
        recover_selected_tags_owned(&selection, &journal(&pending), &context(), &mut coordinator)
            .unwrap();
    retry.retire(&context(), &mut coordinator).unwrap();
    assert!(
        repository
            .tag_edit_operation(&token.operation_id)
            .unwrap()
            .unwrap()
            .is_finished()
    );
    assert_eq!(repository.tag_edit_quota().unwrap().charged_bytes, 0);
}

#[test]
fn changed_actual_original_after_recovery_approval_rejects_retirement_proof_and_keeps_charge() {
    let fixture = Fixture::new("cold-original-changed");
    let token = fixture.begin();
    let budget = fixture.budget();
    let Fixture {
        repository,
        profile,
        _media,
        selection,
        ..
    } = fixture;
    drop(repository);
    assert_eq!(
        child(
            profile.path(),
            &token.operation_id,
            "original_close",
            "backup"
        ),
        77
    );
    let repository = Arc::new(Repository::open(profile.path()).unwrap());
    let db = rusqlite::Connection::open(repository.path()).unwrap();
    db.execute_batch("DROP TRIGGER fail_cold_original_ack;")
        .unwrap();
    let pending = repository
        .tag_edit_operation(&token.operation_id)
        .unwrap()
        .unwrap();
    let mut coordinator = recovery(&repository, &token);
    use std::io::Write;
    let mut file = fs::OpenOptions::new()
        .append(true)
        .open(&selection.path)
        .unwrap();
    file.write_all(b"actual foreign modification").unwrap();
    file.sync_all().unwrap();
    drop(file);
    let modified = fs::read(&selection.path).unwrap();
    let mut failed = owned_failure(
        recover_selected_tags_owned(&selection, &journal(&pending), &context(), &mut coordinator)
            .unwrap_err(),
    );
    assert!(failed.has_live_handles());
    failed.retire(&context(), &mut coordinator).unwrap();
    assert!(failed.is_retired() && !failed.has_live_handles());
    assert_eq!(fs::read(&selection.path).unwrap(), modified);
    assert_eq!(
        repository.tag_edit_quota().unwrap().charged_bytes,
        budget.total().unwrap()
    );
    assert!(
        !repository
            .tag_edit_operation(&token.operation_id)
            .unwrap()
            .unwrap()
            .original_owner_retired
    );
}

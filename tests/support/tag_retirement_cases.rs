use super::*;
use lx_gpui_player::library::tag_install::{
    TagOwnedInstallation, TagRecoveryArtifactState, install_selected_tags_owned,
    recover_selected_tags_owned,
};
use std::{
    process::{Command, Stdio},
    time::{Instant, UNIX_EPOCH},
};

fn patch() -> LocalTagPatch {
    LocalTagPatch {
        title: TagField::Set("Actual same-handle title".into()),
        artists: TagField::Set(vec!["Actual artist".into()]),
        album: TagField::Set("Actual album".into()),
        ..Default::default()
    }
}
fn install_owned(
    fixture: &Fixture,
    coordinator: &mut RepositoryTagEditCoordinator,
) -> Result<TagOwnedInstallation> {
    let directory = coordinator.journal_directory()?;
    fs::create_dir_all(&directory)?;
    install_selected_tags_owned(
        fixture.selection.clone(),
        &directory,
        &patch(),
        &ScanContext::new(Duration::from_secs(30)),
        coordinator,
    )
}
fn failed_owner(error: anyhow::Error) -> TagOwnedInstallation {
    match error
        .downcast::<TagInstallError>()
        .expect("Owned failure must keep its native object")
    {
        TagInstallError::Owned { owner, .. } => *owner,
        _ => panic!("Selected owned installer lost its actual failure owner"),
    }
}

#[test]
fn actual_closed_writer_and_deleted_artifacts_release_quota_only_after_ack_and_preserve_final_mtime()
 {
    let fixture = Fixture::new("owned-complete");
    let token = fixture.begin();
    let mut coordinator = fixture.coordinator(token.clone());
    let mut owner = install_owned(&fixture, &mut coordinator).unwrap();
    let receipt = owner.receipt().unwrap().clone();
    let operation = fixture
        .repository
        .tag_edit_operation(&token.operation_id)
        .unwrap()
        .unwrap();
    assert_eq!(operation.phase, TagEditPhase::Published);
    assert!(!operation.is_finished());
    assert_eq!(
        fixture.repository.tag_edit_quota().unwrap().charged_bytes,
        fixture.budget().total().unwrap()
    );
    assert!(
        fs::OpenOptions::new()
            .write(true)
            .open(&fixture.selection.path)
            .is_err()
    );
    owner
        .retire(&ScanContext::new(Duration::from_secs(30)), &mut coordinator)
        .unwrap();
    assert!(owner.is_retired());
    let finished = fixture
        .repository
        .tag_edit_operation(&token.operation_id)
        .unwrap()
        .unwrap();
    assert!(finished.is_finished());
    assert!(finished.original_owner_retired);
    assert!(finished.artifacts.iter().all(|artifact| artifact.phase
        == lx_gpui_player::repository::TagEditArtifactPhase::Retired
        && artifact.actual_bytes == 0
        && !artifact.path.exists()));
    assert_eq!(
        fixture.repository.tag_edit_quota().unwrap().charged_bytes,
        0
    );
    assert_eq!(
        fixture
            .repository
            .tag_edit_quota()
            .unwrap()
            .pending_operations,
        0
    );
    assert!(fixture.repository.pending_tag_edits().unwrap().is_empty());
    let actual = fs::metadata(&fixture.selection.path)
        .unwrap()
        .modified()
        .unwrap()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    assert_eq!(
        u64::try_from(actual).unwrap(),
        receipt
            .installed
            .as_ref()
            .unwrap()
            .after
            .modified_nanos
            .unwrap()
    );
    assert_eq!(
        format!(
            "{:x}",
            Sha256::digest(fs::read(&fixture.selection.path).unwrap())
        ),
        receipt.new_body_sha256
    );
    let ledger = finished.ledger_revision;
    owner
        .retire(&ScanContext::new(Duration::from_secs(30)), &mut coordinator)
        .unwrap();
    assert_eq!(
        fixture
            .repository
            .tag_edit_operation(&token.operation_id)
            .unwrap()
            .unwrap()
            .ledger_revision,
        ledger
    );
}

#[test]
fn actual_deleted_backup_ack_failure_retains_witness_and_charge_then_retries_without_another_delete()
 {
    let fixture = Fixture::new("owned-ack-gap");
    let token = fixture.begin();
    let mut coordinator = fixture.coordinator(token.clone());
    let mut owner = install_owned(&fixture, &mut coordinator).unwrap();
    let db = rusqlite::Connection::open(fixture.repository.path()).unwrap();
    db.execute_batch("CREATE TRIGGER fail_backup_retired_ack BEFORE UPDATE ON tag_edit_artifacts WHEN NEW.phase='retired' AND OLD.role='backup' BEGIN SELECT RAISE(ABORT,'actual retirement ACK failure'); END;").unwrap();
    assert!(
        owner
            .retire(&ScanContext::new(Duration::from_secs(30)), &mut coordinator)
            .is_err()
    );
    let pending = fixture
        .repository
        .tag_edit_operation(&token.operation_id)
        .unwrap()
        .unwrap();
    let backup = pending
        .artifacts
        .iter()
        .find(|item| item.role == "backup")
        .unwrap();
    assert_eq!(
        backup.phase,
        lx_gpui_player::repository::TagEditArtifactPhase::Retiring
    );
    assert!(!backup.path.exists());
    assert!(pending.original_owner_retired);
    assert!(!pending.is_finished());
    assert_eq!(
        fixture.repository.tag_edit_quota().unwrap().charged_bytes,
        fixture.budget().total().unwrap() - fixture.budget().original_bytes
    );
    db.execute_batch("DROP TRIGGER fail_backup_retired_ack;")
        .unwrap();
    owner
        .retire(&ScanContext::new(Duration::from_secs(30)), &mut coordinator)
        .unwrap();
    assert!(
        fixture
            .repository
            .tag_edit_operation(&token.operation_id)
            .unwrap()
            .unwrap()
            .is_finished()
    );
    assert_eq!(
        fixture.repository.tag_edit_quota().unwrap().charged_bytes,
        0
    );
}

#[test]
fn busy_before_actual_writer_and_ordinary_cancel_release_only_proven_unopened_slots() {
    for cancelled in [false, true] {
        let fixture = Fixture::new(if cancelled {
            "owned-cancelled"
        } else {
            "owned-busy"
        });
        let token = fixture.begin();
        let hold = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&fixture.selection.path)
            .unwrap();
        let continuation: Arc<dyn Fn() -> Result<()> + Send + Sync> = if cancelled {
            Arc::new(|| anyhow::bail!("actual requested cancellation"))
        } else {
            Arc::new(|| Ok(()))
        };
        let mut coordinator = RepositoryTagEditCoordinator::new(
            fixture.repository.clone(),
            token.clone(),
            continuation,
            Arc::new(|| Ok(())),
        )
        .unwrap();
        let mut owner = failed_owner(install_owned(&fixture, &mut coordinator).unwrap_err());
        if !cancelled {
            assert!(owner.admission_was_busy());
        }
        assert_eq!(
            fixture.repository.tag_edit_quota().unwrap().charged_bytes,
            fixture.budget().total().unwrap()
        );
        owner
            .retire(&ScanContext::new(Duration::from_secs(30)), &mut coordinator)
            .unwrap();
        let done = fixture
            .repository
            .tag_edit_operation(&token.operation_id)
            .unwrap()
            .unwrap();
        assert!(done.is_finished());
        assert!(done.original_never_opened);
        assert!(done.artifacts.iter().all(|item| item.never_allocated));
        assert_eq!(
            fixture.repository.tag_edit_quota().unwrap().charged_bytes,
            0
        );
        assert_eq!(fs::read(&fixture.selection.path).unwrap(), fixture.old_body);
        drop(hold);
    }
}

#[test]
fn retired_artifact_receipt_and_unknown_legacy_allocation_are_distinct_cold_states() {
    let fixture = Fixture::new("cold-ledger-state");
    let token = fixture.begin();
    assert!(matches!(
        fixture
            .repository
            .tag_recovery_artifact(&token, TagInstallArtifactRole::Backup)
            .unwrap(),
        TagRecoveryArtifactState::NeverAllocated
    ));
    let db = rusqlite::Connection::open(fixture.repository.path()).unwrap();
    db.execute("UPDATE tag_edit_artifacts SET document=json_remove(document,'$.allocation_attempted'),byte_size=length(CAST(json_remove(document,'$.allocation_attempted') AS BLOB)) WHERE operation_id=?1 AND role='backup'",[&token.operation_id]).unwrap();
    assert!(matches!(
        fixture
            .repository
            .tag_recovery_artifact(&token, TagInstallArtifactRole::Backup)
            .unwrap(),
        TagRecoveryArtifactState::Uncertain
    ));
    assert_eq!(
        fixture.repository.tag_edit_quota().unwrap().charged_bytes,
        fixture.budget().total().unwrap()
    );
}

#[test]
fn retirement_process_helper() {
    let Some(profile) = std::env::var_os("LX_TAG_RETIRE_TEST_PROFILE") else {
        return;
    };
    let repository = Arc::new(Repository::open(&profile).unwrap());
    let token = repository
        .tag_edit_operation("cold-actual-tag")
        .unwrap()
        .unwrap()
        .token;
    let mut coordinator = RepositoryTagEditCoordinator::new(
        repository.clone(),
        token.clone(),
        Arc::new(|| Ok(())),
        Arc::new(|| Ok(())),
    )
    .unwrap();
    let directory = coordinator.journal_directory().unwrap();
    fs::create_dir_all(&directory).unwrap();
    let mut owner = install_selected_tags_owned(
        token.selection.clone(),
        &directory,
        &patch(),
        &ScanContext::new(Duration::from_secs(30)),
        &mut coordinator,
    )
    .unwrap();
    let db = rusqlite::Connection::open(repository.path()).unwrap();
    db.execute_batch("CREATE TRIGGER fail_cold_before_delete BEFORE UPDATE ON tag_edit_artifacts WHEN NEW.phase='retiring' AND OLD.role='backup' BEGIN SELECT RAISE(ABORT,'actual pre-delete process stop'); END;").unwrap();
    assert!(
        owner
            .retire(&ScanContext::new(Duration::from_secs(30)), &mut coordinator)
            .is_err()
    );
    assert!(
        repository
            .tag_edit_operation(&token.operation_id)
            .unwrap()
            .unwrap()
            .original_owner_retired
    );
    std::process::exit(74);
}

#[test]
fn actual_process_restart_requires_fresh_recovery_approval_and_finishes_remaining_native_files() {
    let fixture = Fixture::new("cold-actual-tag");
    for index in 0..63 {
        let mut unused = fixture.selection.clone();
        unused.operation_id = format!("unused-install-operation-{index}");
        fixture
            .repository
            .approve_tag_write(
                &format!("unused-install-approval-{index}"),
                &unused,
                &|| Ok(()),
            )
            .unwrap();
    }
    let token = fixture.begin();
    let before = fixture.repository.tag_document_budget().unwrap();
    assert!(before.charged_bytes <= before.maximum_bytes);
    assert!(
        fixture
            .repository
            .approve_tag_write("install-capacity-overflow", &fixture.selection, &|| Ok(()))
            .is_err()
    );
    let Fixture {
        repository,
        profile,
        _media,
        selection,
        ..
    } = fixture;
    drop(repository);
    let mut child = Command::new(std::env::current_exe().unwrap());
    child
        .args([
            "--exact",
            "retirement_cases::retirement_process_helper",
            "--nocapture",
        ])
        .env("LX_TAG_RETIRE_TEST_PROFILE", profile.path())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    {
        use std::os::windows::process::CommandExt;
        child.creation_flags(windows_sys::Win32::System::Threading::CREATE_NO_WINDOW);
    }
    let mut child = child.spawn().unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        assert!(
            Instant::now() < deadline,
            "Isolated retirement helper did not stop"
        );
        std::thread::sleep(Duration::from_millis(10));
    };
    assert_eq!(status.code(), Some(74));
    let repository = Arc::new(Repository::open(profile.path()).unwrap());
    let operation = repository
        .tag_edit_operation(&token.operation_id)
        .unwrap()
        .unwrap();
    assert!(!operation.is_finished());
    let db = rusqlite::Connection::open(repository.path()).unwrap();
    db.execute_batch("DROP TRIGGER fail_cold_before_delete;")
        .unwrap();
    drop(db);
    let mut current = selection.clone();
    current.expected_binding_revision = operation
        .commit
        .as_ref()
        .unwrap()
        .receipt
        .committed_binding_revision
        .unwrap();
    current.expected_body_sha256 = Some(format!(
        "{:x}",
        Sha256::digest(fs::read(&selection.path).unwrap())
    ));
    let approval = repository
        .approve_tag_recovery("current-explicit-recovery", &token, &current, &|| Ok(()))
        .unwrap();
    let charged = repository.tag_document_budget().unwrap();
    assert!(charged.charged_bytes <= charged.maximum_bytes);
    assert_eq!(
        charged.charged_bytes, before.charged_bytes,
        "The current explicit recovery document consumes the existing reserved allowance"
    );
    let db = rusqlite::Connection::open(repository.path()).unwrap();
    let rows: (i64, i64) = db.query_row("SELECT count(*), sum(CASE WHEN json_extract(document,'$.purpose.kind')='recovery' THEN 1 ELSE 0 END) FROM tag_write_approvals", [], |row| Ok((row.get(0)?, row.get(1)?))).unwrap();
    assert_eq!(rows, (65, 1));
    drop(db);
    let recovery_id = approval.approval_id.clone();
    let mut coordinator = RepositoryTagEditCoordinator::for_recovery(
        repository.clone(),
        token.clone(),
        approval.approval_id,
        approval.approval_epoch,
        Arc::new(|| Ok(())),
        Arc::new(|| Ok(())),
    )
    .unwrap();
    let journal = operation
        .artifacts
        .iter()
        .find(|item| item.role == "journal")
        .unwrap()
        .path
        .clone();
    let mut owner = recover_selected_tags_owned(
        &token.selection,
        &journal,
        &ScanContext::new(Duration::from_secs(30)),
        &mut coordinator,
    )
    .unwrap();
    owner
        .retire(&ScanContext::new(Duration::from_secs(30)), &mut coordinator)
        .unwrap();
    assert!(owner.is_retired());
    assert!(
        repository
            .tag_edit_operation(&token.operation_id)
            .unwrap()
            .unwrap()
            .is_finished()
    );
    assert_eq!(repository.tag_edit_quota().unwrap().charged_bytes, 0);
    let completed = repository
        .tag_edit_operation(&token.operation_id)
        .unwrap()
        .unwrap();
    assert_eq!(completed.recovery_approval_history.len(), 1);
    assert_eq!(
        completed.recovery_approval_history[0].approval_id,
        recovery_id
    );
    assert!(completed.recovery_approval_history[0].revoked);
    assert!(
        repository
            .tag_write_approval(&recovery_id)
            .unwrap()
            .is_none()
    );
    assert!(
        repository
            .approve_tag_write(&recovery_id, &current, &|| Ok(()))
            .is_err()
    );
    let db = rusqlite::Connection::open(repository.path()).unwrap();
    let rows: i64 = db
        .query_row("SELECT count(*) FROM tag_write_approvals", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(rows, 64);
    for index in 0..63 {
        assert!(
            !repository
                .tag_write_approval(&format!("unused-install-approval-{index}"))
                .unwrap()
                .unwrap()
                .revoked
        );
    }
    assert_eq!(
        repository
            .local_asset_authority(&selection.asset_id)
            .unwrap()
            .unwrap()
            .explicit_file_epoch,
        Some(selection.grant_epoch)
    );
    assert_eq!(repository.tag_document_budget().unwrap().reserved_bytes, 0);
}

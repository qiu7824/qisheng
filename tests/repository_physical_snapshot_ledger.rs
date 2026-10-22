//! Repository receipt tests confirm trusted controller metadata transactions;
//! synthetic locators do not assert physical file creation or deletion proof.
use lx_gpui_player::repository::{
    Repository, SCHEMA_VERSION,
    physical_snapshot_ledger::{
        PhysicalSnapshotLedgerContext, PhysicalSnapshotLedgerJournal, PhysicalSnapshotLedgerState,
    },
    snapshot_export::{
        PhysicalSnapshotCleanupFact, PhysicalSnapshotReceipt, PhysicalSnapshotRecoveryJournal,
        PhysicalSnapshotRecoveryLocator, PhysicalSnapshotRequest,
    },
};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{path::PathBuf, sync::Arc};
fn repo() -> (tempfile::TempDir, Arc<Repository>) {
    let dir = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
    let repo = Arc::new(Repository::open(dir.path().join("profile")).unwrap());
    (dir, repo)
}
fn context(repo: &Repository, id: &str) -> PhysicalSnapshotLedgerContext {
    let (profile_id, profile_epoch) = repo.physical_snapshot_profile_scope().unwrap();
    PhysicalSnapshotLedgerContext {
        operation_id: id.into(),
        profile_id,
        profile_epoch,
        operation_epoch: repo.physical_snapshot_next_operation_epoch().unwrap(),
        selection_epoch: repo.physical_snapshot_next_selection_epoch().unwrap(),
        expected_snapshot_revision: repo.revision().unwrap(),
        maximum_image_bytes: 1024 * 1024,
        basename: format!("{id}.sqlite3"),
    }
}
fn path(name: &str) -> PathBuf {
    if cfg!(windows) {
        PathBuf::from(format!("C:\\snapshot-metadata-fixture\\{name}"))
    } else {
        PathBuf::from(format!("/snapshot-metadata-fixture/{name}"))
    }
}
fn locator(c: &PhysicalSnapshotLedgerContext) -> PhysicalSnapshotRecoveryLocator {
    let request = PhysicalSnapshotRequest {
        operation_id: c.operation_id.clone(),
        expected_revision: c.expected_snapshot_revision,
        maximum_image_bytes: c.maximum_image_bytes,
        basename: c.basename.clone(),
    };
    let nonce = format!("{:032x}", c.operation_epoch);
    let digest = format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(&request).unwrap())
    );
    serde_json::from_value(json!({"operation_id":c.operation_id,"request_digest":digest,"request":request,"selected_identity":{"device":7,"object":"b".repeat(32)},"manifest":{
        "operation_id":c.operation_id,"nonce":nonce,"temporary_name":format!(".lx-snapshot-{nonce}.manifest"),"directory":path("private"),"directory_identity":{"device":7,"object":"a".repeat(32)},
        "selection_epoch":c.selection_epoch,"identity":{"device":7,"object":format!("{:032x}",c.operation_epoch+100)},"bytes":0,"published":false,"final_basename":null,"cleanup_probe":null}})).unwrap()
}
fn fact(loc: &PhysicalSnapshotRecoveryLocator, stage: u8) -> PhysicalSnapshotCleanupFact {
    let mut file = serde_json::to_value(loc).unwrap()["manifest"].clone();
    file["bytes"] = 64.into();
    serde_json::from_value(json!({"native":{"file":file,"stamp":{"changed":1,"written":1,"length":64},"sha256":"c".repeat(64),"pending_zero_links":stage>=1,"removed":stage==2}})).unwrap()
}
fn receipt(c: &PhysicalSnapshotLedgerContext) -> PhysicalSnapshotReceipt {
    PhysicalSnapshotReceipt {
        operation_id: c.operation_id.clone(),
        path: path(&c.basename),
        bytes: 128,
        sha256: "d".repeat(64),
        file_device: 7,
        file_object: format!("{:032x}", c.operation_epoch + 1000),
        root_device: 7,
        root_object: "b".repeat(32),
        snapshot_revision: c.expected_snapshot_revision,
        schema_version: SCHEMA_VERSION,
        selection_epoch: c.selection_epoch,
    }
}
#[test]
fn registration_receipt_and_monotonic_cleanup_are_atomic_and_do_not_change_user_revision() {
    let (_dir, repo) = repo();
    let before = repo.revision().unwrap();
    let c = context(&repo, "atomic");
    let loc = locator(&c);
    let journal = PhysicalSnapshotLedgerJournal::new(repo.clone(), c.clone()).unwrap();
    journal.begin_registration(&loc).unwrap();
    journal.begin_registration(&loc).unwrap();
    let first = repo.physical_snapshot_record("atomic").unwrap().unwrap();
    assert_eq!(first.ledger_revision, 1);
    journal.record_cleanup(&loc, &fact(&loc, 0)).unwrap();
    journal.record_cleanup(&loc, &fact(&loc, 0)).unwrap();
    assert!(journal.record_cleanup(&loc, &fact(&loc, 2)).is_ok());
    assert!(journal.record_cleanup(&loc, &fact(&loc, 1)).is_err());
    let output = receipt(&c);
    journal.commit_receipt(&output).unwrap();
    journal.commit_receipt(&output).unwrap();
    journal.finished(&loc).unwrap();
    journal.finished(&loc).unwrap();
    let stored = repo.physical_snapshot_record("atomic").unwrap().unwrap();
    assert_eq!(stored.state, PhysicalSnapshotLedgerState::Finished);
    assert!(stored.recovery.cleanup.is_empty());
    assert_eq!(
        stored.recovery.publication.unwrap().file_object,
        output.file_object
    );
    assert_eq!(repo.revision().unwrap(), before);
}
#[test]
fn stale_wrapper_cas_and_conflicting_receipts_cannot_change_current_record() {
    let (_dir, repo) = repo();
    let c = context(&repo, "cas");
    let loc = locator(&c);
    let first = PhysicalSnapshotLedgerJournal::new(repo.clone(), c.clone()).unwrap();
    first.begin_registration(&loc).unwrap();
    let stale = PhysicalSnapshotLedgerJournal::new(repo.clone(), c.clone()).unwrap();
    first.record_cleanup(&loc, &fact(&loc, 0)).unwrap();
    assert!(stale.record_cleanup(&loc, &fact(&loc, 1)).is_err());
    let output = receipt(&c);
    first.commit_receipt(&output).unwrap();
    let mut foreign = output.clone();
    foreign.sha256 = "f".repeat(64);
    assert!(first.commit_receipt(&foreign).is_err());
    let current = repo.physical_snapshot_record("cas").unwrap().unwrap();
    assert_eq!(current.recovery.publication.unwrap().sha256, output.sha256);
    assert!(!current.recovery.cleanup[0].removed());
}
#[test]
fn actual_sql_failure_rolls_back_state_and_allows_same_callback_retry() {
    let (_dir, repo) = repo();
    let c = context(&repo, "fault");
    let loc = locator(&c);
    let journal = PhysicalSnapshotLedgerJournal::new(repo.clone(), c.clone()).unwrap();
    journal.begin_registration(&loc).unwrap();
    let fault = rusqlite::Connection::open(repo.path()).unwrap();
    fault.execute_batch("CREATE TRIGGER fail_snapshot_update BEFORE UPDATE ON physical_snapshot_operations BEGIN SELECT RAISE(ABORT,'fixture SQL fault'); END").unwrap();
    assert!(journal.commit_receipt(&receipt(&c)).is_err());
    let current = repo.physical_snapshot_record("fault").unwrap().unwrap();
    assert_eq!(current.ledger_revision, 1);
    assert_eq!(current.state, PhysicalSnapshotLedgerState::Registered);
    assert!(current.recovery.publication.is_none());
    fault
        .execute_batch("DROP TRIGGER fail_snapshot_update")
        .unwrap();
    journal.commit_receipt(&receipt(&c)).unwrap();
    assert_eq!(
        repo.physical_snapshot_record("fault")
            .unwrap()
            .unwrap()
            .state,
        PhysicalSnapshotLedgerState::ReceiptCommitted
    );
}
#[test]
fn four_pending_and_65_finished_history_boundary_keep_current_completion() {
    let (_dir, repo) = repo();
    for i in 0..65 {
        let c = context(&repo, &format!("history-{i}"));
        let loc = locator(&c);
        let j = PhysicalSnapshotLedgerJournal::new(repo.clone(), c).unwrap();
        j.begin_registration(&loc).unwrap();
        j.record_cleanup(&loc, &fact(&loc, 2)).unwrap();
        j.finished(&loc).unwrap();
    }
    let records = repo.physical_snapshot_records().unwrap();
    assert_eq!(records.len(), 64);
    assert!(
        repo.physical_snapshot_record("history-0")
            .unwrap()
            .is_none()
    );
    assert_eq!(
        repo.physical_snapshot_record("history-64")
            .unwrap()
            .unwrap()
            .state,
        PhysicalSnapshotLedgerState::Finished
    );
    for i in 0..4 {
        let c = context(&repo, &format!("pending-{i}"));
        let loc = locator(&c);
        PhysicalSnapshotLedgerJournal::new(repo.clone(), c)
            .unwrap()
            .begin_registration(&loc)
            .unwrap();
    }
    let c = context(&repo, "pending-fifth");
    let loc = locator(&c);
    assert!(
        PhysicalSnapshotLedgerJournal::new(repo.clone(), c)
            .unwrap()
            .begin_registration(&loc)
            .is_err()
    );
    assert_eq!(repo.physical_snapshot_pending_records().unwrap().len(), 4);
    assert!(
        repo.physical_snapshot_record("pending-fifth")
            .unwrap()
            .is_none()
    );
}
#[test]
fn changed_nonce_fid_epoch_and_unconfirmed_finish_are_rejected() {
    let (_dir, repo) = repo();
    let c = context(&repo, "bindings");
    let loc = locator(&c);
    let j = PhysicalSnapshotLedgerJournal::new(repo.clone(), c.clone()).unwrap();
    j.begin_registration(&loc).unwrap();
    assert!(j.finished(&loc).is_err());
    let mut changed = serde_json::to_value(&loc).unwrap();
    changed["manifest"]["nonce"] = "9".repeat(32).into();
    let changed: PhysicalSnapshotRecoveryLocator = serde_json::from_value(changed).unwrap();
    assert!(j.begin_registration(&changed).is_err());
    j.record_cleanup(&loc, &fact(&loc, 0)).unwrap();
    let mut changed = serde_json::to_value(fact(&loc, 1)).unwrap();
    changed["native"]["file"]["identity"]["object"] = "e".repeat(32).into();
    assert!(
        j.record_cleanup(&loc, &serde_json::from_value(changed).unwrap())
            .is_err()
    );
    let mut stale = c;
    stale.profile_epoch += 1;
    assert!(PhysicalSnapshotLedgerJournal::new(repo.clone(), stale).is_err());
}
#[test]
fn restart_preserves_receipt_and_epoch_high_water_without_reusing_scope() {
    let (dir, repo) = repo();
    let c = context(&repo, "restart");
    let loc = locator(&c);
    let j = PhysicalSnapshotLedgerJournal::new(repo.clone(), c.clone()).unwrap();
    j.begin_registration(&loc).unwrap();
    j.commit_receipt(&receipt(&c)).unwrap();
    let scope = repo.physical_snapshot_profile_scope().unwrap();
    drop(j);
    drop(repo);
    let repo = Arc::new(Repository::open(dir.path().join("profile")).unwrap());
    assert_eq!(repo.physical_snapshot_profile_scope().unwrap(), scope);
    assert!(repo.physical_snapshot_next_operation_epoch().unwrap() > c.operation_epoch);
    assert!(repo.physical_snapshot_next_selection_epoch().unwrap() > c.selection_epoch);
    let j = PhysicalSnapshotLedgerJournal::new(repo.clone(), c).unwrap();
    j.authorize_locator(&loc).unwrap();
    assert_eq!(
        repo.physical_snapshot_pending_records().unwrap()[0].state,
        PhysicalSnapshotLedgerState::ReceiptCommitted
    );
}

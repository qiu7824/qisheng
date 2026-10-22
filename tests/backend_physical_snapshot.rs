#![cfg(windows)]

#[path = "support/library_fixture.rs"]
mod fixture;

use fixture::Running;
use lx_gpui_player::{
    backend::{
        BackendCommand, BackendConfig, BackendEvent, BackendHandle, BackendQuery,
        BackendQueryResult, ErrorKind, PhysicalSnapshotRecoveryMode, PhysicalSnapshotView,
    },
    domain::music::{LxMusicDtoVersion, TrackRecord},
    repository::{DATABASE_NAME, Repository, SCHEMA_VERSION},
    sync::{PeerCapabilities, SharedKey, network::PeerRecord},
};
use rusqlite::{Connection, OpenFlags};
use sha2::{Digest, Sha256};
use std::{fs, path::PathBuf, time::Duration};

const IMAGE_LIMIT: u64 = 8 * 1024 * 1024;

fn query(backend: &BackendHandle, id: &str) -> PhysicalSnapshotView {
    match backend
        .query(BackendQuery::PhysicalSnapshot {
            operation_id: id.into(),
        })
        .unwrap()
    {
        BackendQueryResult::PhysicalSnapshot(Some(view)) => view,
        other => panic!("Missing physical snapshot view: {other:?}"),
    }
}
fn export(
    backend: &BackendHandle,
    id: &str,
    directory: PathBuf,
    basename: &str,
) -> (bool, PhysicalSnapshotView) {
    let cursor = backend.event_cursor();
    backend
        .execute(fixture::envelope(
            id,
            BackendCommand::ExportPhysicalSnapshot {
                directory,
                basename: basename.into(),
                maximum_image_bytes: IMAGE_LIMIT,
            },
        ))
        .unwrap();
    let success = fixture::operation_finished(backend, cursor, id).0;
    (success, query(backend, id))
}
fn recover(
    backend: &BackendHandle,
    command_id: &str,
    snapshot: &str,
    directory: PathBuf,
    action: PhysicalSnapshotRecoveryMode,
) -> (bool, PhysicalSnapshotView) {
    let cursor = backend.event_cursor();
    backend
        .execute(fixture::envelope(
            command_id,
            BackendCommand::RecoverPhysicalSnapshot {
                snapshot_operation_id: snapshot.into(),
                directory,
                action,
            },
        ))
        .unwrap();
    let success = fixture::operation_finished(backend, cursor, command_id).0;
    (success, query(backend, snapshot))
}

#[test]
fn public_physical_export_is_actual_sanitized_sqlite_with_durable_receipt_and_unchanged_profile() {
    let temporary = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
    let profile = temporary.path().join("profile");
    let secret;
    let track;
    {
        let repository = Repository::open(&profile).unwrap();
        track = TrackRecord::from_lx_dto(
            serde_json::json!({
                "id":"kw_physical_actor", "source":"kw", "name":"Persisted native track",
                "meta":{"songId":"physical_actor"}
            }),
            LxMusicDtoVersion::Modern,
        )
        .unwrap();
        repository.upsert_track(&track).unwrap();
        let scope = repository.sync_profile_state().unwrap();
        repository
            .sync_configure_pairing(scope.profile_epoch, scope.pairing_epoch, true)
            .unwrap();
        let key = SharedKey::from_bytes([0xa7; 16]);
        secret = key.expose_base64();
        repository
            .sync_store_peer(
                scope.pairing_epoch,
                &PeerRecord {
                    peer_id: "physical-actor-peer".into(),
                    client_id: "physical-actor-client".into(),
                    key,
                    device_name: "Native snapshot fixture".into(),
                    is_mobile: false,
                    auth_epoch: 1,
                    capabilities: PeerCapabilities {
                        list: Some(false),
                        dislike: Some(false),
                    },
                },
            )
            .unwrap();
    }
    let backend = BackendHandle::start(BackendConfig::headless(profile.clone())).unwrap();
    let directory = temporary.path().join("exports");
    fs::create_dir(&directory).unwrap();
    let revision = backend.snapshot_with_cursor().unwrap().0.revision;
    let (success, view) = export(
        &backend,
        "physical-actual-sqlite",
        directory.clone(),
        "library.sqlite3",
    );
    assert!(success, "{view:?}");
    assert_eq!(view.state, "published");
    assert_eq!(view.snapshot_revision, revision);
    assert!(!view.pending_recovery && !view.has_live_owner);
    let receipt = view.receipt.unwrap();
    let bytes = fs::read(&receipt.path).unwrap();
    assert!(bytes.starts_with(b"SQLite format 3\0"));
    assert_eq!(receipt.bytes, bytes.len() as u64);
    assert_eq!(receipt.sha256, format!("{:x}", Sha256::digest(&bytes)));
    assert_eq!(receipt.schema_version, SCHEMA_VERSION);
    assert!(
        !bytes
            .windows(secret.len())
            .any(|window| window == secret.as_bytes())
    );
    let image =
        Connection::open_with_flags(&receipt.path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    assert_eq!(
        image
            .query_row("PRAGMA integrity_check", [], |row| row.get::<_, String>(0))
            .unwrap(),
        "ok"
    );
    assert_eq!(
        image
            .query_row(
                "SELECT count(*) FROM sync_peers WHERE private_key IS NOT NULL OR revoked=0",
                [],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
        0
    );
    assert_eq!(
        image
            .query_row(
                "SELECT count(*) FROM physical_snapshot_operations",
                [],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
        0
    );
    let document: String = image
        .query_row(
            "SELECT document FROM tracks WHERE id=?1",
            [&track.id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        serde_json::from_str::<TrackRecord>(&document).unwrap(),
        track
    );
    drop(image);
    assert_eq!(backend.snapshot_with_cursor().unwrap().0.revision, revision);
    let live = Connection::open_with_flags(
        profile.join(DATABASE_NAME),
        OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    assert_eq!(
        live.query_row(
            "SELECT private_key FROM sync_peers WHERE peer_id='physical-actor-peer'",
            [],
            |row| row.get::<_, String>(0)
        )
        .unwrap(),
        secret
    );
    drop(live);
    assert_eq!(fs::read_dir(&directory).unwrap().count(), 1);
    backend.shutdown(Duration::from_secs(15)).unwrap();
    let reopened = BackendHandle::start(BackendConfig::headless(profile)).unwrap();
    let restored = query(&reopened, "physical-actual-sqlite");
    assert_eq!(restored.state, "published");
    assert!(!restored.pending_recovery && !restored.has_live_owner);
    assert_eq!(restored.receipt.unwrap().sha256, receipt.sha256);
    reopened.shutdown(Duration::from_secs(15)).unwrap();
}

#[test]
fn collision_preserves_foreign_final_and_explicit_cleanup_retires_actual_private_owner() {
    let running = Running::new();
    let directory = running.folder("physical-collision");
    let foreign = directory.join("existing.sqlite3");
    fs::write(&foreign, b"foreign final remains intact").unwrap();
    let revision = running.backend.snapshot_with_cursor().unwrap().0.revision;
    let (success, failed) = export(
        &running.backend,
        "physical-conflict",
        directory.clone(),
        "existing.sqlite3",
    );
    assert!(!success);
    assert!(failed.pending_recovery && failed.has_live_owner);
    assert!(failed.receipt.is_none());
    assert_eq!(fs::read(&foreign).unwrap(), b"foreign final remains intact");
    let wrong = running.folder("physical-wrong-root");
    let (success, retained) = recover(
        &running.backend,
        "physical-wrong-cleanup",
        "physical-conflict",
        wrong.clone(),
        PhysicalSnapshotRecoveryMode::CleanupPrivate,
    );
    assert!(!success);
    assert!(retained.pending_recovery && retained.has_live_owner);
    assert_eq!(fs::read_dir(&wrong).unwrap().count(), 0);
    let (success, cleaned) = recover(
        &running.backend,
        "physical-explicit-cleanup",
        "physical-conflict",
        directory.clone(),
        PhysicalSnapshotRecoveryMode::CleanupPrivate,
    );
    assert!(success, "{cleaned:?}");
    assert_eq!(cleaned.state, "cleaned");
    assert!(!cleaned.pending_recovery && !cleaned.has_live_owner);
    assert_eq!(fs::read(&foreign).unwrap(), b"foreign final remains intact");
    assert_eq!(fs::read_dir(&directory).unwrap().count(), 1);
    assert_eq!(
        running.backend.snapshot_with_cursor().unwrap().0.revision,
        revision
    );
}

#[test]
fn cold_start_restores_pending_without_writers_and_requires_same_actual_destination() {
    let temporary = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
    let profile = temporary.path().join("profile");
    let directory = temporary.path().join("exports");
    let wrong = temporary.path().join("wrong");
    fs::create_dir(&directory).unwrap();
    fs::create_dir(&wrong).unwrap();
    let foreign = directory.join("existing.sqlite3");
    fs::write(&foreign, b"cold foreign file").unwrap();
    let backend = BackendHandle::start(BackendConfig::headless(profile.clone())).unwrap();
    let (success, failed) = export(
        &backend,
        "physical-cold-pending",
        directory.clone(),
        "existing.sqlite3",
    );
    assert!(!success && failed.pending_recovery);
    // Settled pending accounting is an explicit failure, even after handles safely close.
    let shutdown = backend.shutdown(Duration::from_secs(15)).unwrap_err();
    assert_eq!(
        shutdown.kind,
        ErrorKind::OperationFailed,
        "The actor must actually join after safe handle release"
    );
    assert_eq!(
        shutdown.message,
        "Physical snapshot recovery remains pending"
    );
    let retired = backend.retirement_state();
    assert!(
        retired.actor_joined && retired.runtime_retired,
        "Actual retirement still owns the failed snapshot: {retired:?}"
    );
    {
        let settled = Repository::open(profile.clone()).unwrap();
        assert_eq!(
            settled.physical_snapshot_pending_records().unwrap().len(),
            1,
            "Shutdown cleared the charged cold-recovery ledger"
        );
    }
    let mut before = fs::read_dir(&directory)
        .unwrap()
        .map(|item| item.unwrap().file_name())
        .collect::<Vec<_>>();
    before.sort();
    let reopened = BackendHandle::start(BackendConfig::headless(profile.clone())).unwrap();
    let restored = query(&reopened, "physical-cold-pending");
    assert_eq!(restored.state, "recovery_needed");
    assert!(restored.pending_recovery && !restored.has_live_owner);
    let mut after = fs::read_dir(&directory)
        .unwrap()
        .map(|item| item.unwrap().file_name())
        .collect::<Vec<_>>();
    after.sort();
    assert_eq!(before, after, "Startup reopened a destination writer");
    let (success, rejected) = recover(
        &reopened,
        "physical-cold-wrong-root",
        "physical-cold-pending",
        wrong.clone(),
        PhysicalSnapshotRecoveryMode::ResumePublication,
    );
    assert!(!success && rejected.pending_recovery);
    assert_eq!(fs::read_dir(&wrong).unwrap().count(), 0);
    let (success, cleaned) = recover(
        &reopened,
        "physical-cold-cleanup",
        "physical-cold-pending",
        directory,
        PhysicalSnapshotRecoveryMode::CleanupPrivate,
    );
    assert!(success, "{cleaned:?}");
    assert!(!cleaned.pending_recovery && !cleaned.has_live_owner);
    assert_eq!(fs::read(&foreign).unwrap(), b"cold foreign file");
    reopened.shutdown(Duration::from_secs(15)).unwrap();
    let repository = Repository::open(profile).unwrap();
    assert!(
        repository
            .physical_snapshot_pending_records()
            .unwrap()
            .is_empty()
    );
}

#[test]
fn four_failed_owners_still_consume_admission_until_each_actual_cleanup_finishes() {
    let running = Running::new();
    let directory = running.folder("physical-four");
    fs::write(directory.join("existing.sqlite3"), b"foreign shared final").unwrap();
    for index in 0..4 {
        let (success, view) = export(
            &running.backend,
            &format!("physical-retained-{index}"),
            directory.clone(),
            "existing.sqlite3",
        );
        assert!(!success && view.pending_recovery && view.has_live_owner);
    }
    let result = running.backend.execute(fixture::envelope(
        "physical-fifth",
        BackendCommand::ExportPhysicalSnapshot {
            directory: directory.clone(),
            basename: "fifth.sqlite3".into(),
            maximum_image_bytes: IMAGE_LIMIT,
        },
    ));
    assert!(result.is_err());
    assert!(!directory.join("fifth.sqlite3").exists());
    let BackendQueryResult::PhysicalSnapshots(views) = running
        .backend
        .query(BackendQuery::PhysicalSnapshots)
        .unwrap()
    else {
        panic!("Physical history query")
    };
    assert_eq!(views.iter().filter(|view| view.pending_recovery).count(), 4);
    for index in 0..4 {
        let (success, view) = recover(
            &running.backend,
            &format!("physical-clean-{index}"),
            &format!("physical-retained-{index}"),
            directory.clone(),
            PhysicalSnapshotRecoveryMode::CleanupPrivate,
        );
        assert!(success, "{view:?}");
    }
    let (success, view) = export(
        &running.backend,
        "physical-after-cleanup",
        directory,
        "after.sqlite3",
    );
    assert!(success && !view.pending_recovery, "{view:?}");
}

#[test]
fn invalid_leaf_and_missing_destination_leave_no_pending_physical_owner() {
    let running = Running::new();
    let directory = running.folder("physical-invalid");
    let (success, view) = export(
        &running.backend,
        "physical-invalid-leaf",
        directory.clone(),
        "../escape.sqlite3",
    );
    assert!(!success && !view.pending_recovery && !view.has_live_owner);
    assert!(!running.directory.path().join("escape.sqlite3").exists());
    assert_eq!(fs::read_dir(&directory).unwrap().count(), 0);
    let missing = running.directory.path().join("absent-destination");
    let (success, view) = export(
        &running.backend,
        "physical-missing-dir",
        missing.clone(),
        "safe.sqlite3",
    );
    assert!(!success && !view.pending_recovery && !view.has_live_owner);
    assert!(!missing.exists());
}

fn registration_failure_keeps_original_owner_until_recovery(explicit_recovery: bool) {
    let temporary = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
    let profile = temporary.path().join("profile");
    let destination = temporary.path().join("exports");
    fs::create_dir(&destination).unwrap();
    let backend = BackendHandle::start(BackendConfig::headless(profile.clone())).unwrap();
    let fault = Connection::open(profile.join(DATABASE_NAME)).unwrap();
    fault.execute_batch("CREATE TRIGGER fixture_physical_registration_failure BEFORE INSERT ON physical_snapshot_operations BEGIN SELECT RAISE(ABORT,'fixture registration failure'); END;").unwrap();
    let cursor = backend.event_cursor();
    let (success, view) = export(
        &backend,
        "physical-unregistered-owner",
        destination.clone(),
        "never.sqlite3",
    );
    assert!(!success);
    assert!(view.pending_recovery && view.has_live_owner);
    assert!(view.receipt.is_none());
    assert_eq!(
        fault
            .query_row(
                "SELECT count(*) FROM physical_snapshot_operations",
                [],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
        0
    );
    assert!(
        fs::read_dir(profile.join(".lx-snapshot-staging"))
            .unwrap()
            .next()
            .is_some(),
        "The genuine unregistered manifest must still be held"
    );
    fault
        .execute_batch("DROP TRIGGER fixture_physical_registration_failure;")
        .unwrap();
    if explicit_recovery {
        let (success, cleaned) = recover(
            &backend,
            "physical-registration-retry",
            "physical-unregistered-owner",
            destination.clone(),
            PhysicalSnapshotRecoveryMode::CleanupPrivate,
        );
        assert!(success, "{cleaned:?}");
        assert!(!cleaned.pending_recovery && !cleaned.has_live_owner);
        assert_eq!(cleaned.state, "cleaned");
    }
    drop(fault);
    backend.shutdown(Duration::from_secs(15)).unwrap();
    let terminal_events: Vec<_> = backend.events_after(cursor).unwrap().into_iter().filter(|event| matches!(&event.payload,
        BackendEvent::OperationFinished { operation_id, .. } if operation_id == "physical-unregistered-owner")).collect();
    assert_eq!(
        terminal_events.len(),
        1,
        "Shutdown cleanup must not emit another terminal result for the failed request: {terminal_events:?}"
    );
    assert!(!destination.join("never.sqlite3").exists());
    assert_eq!(
        fs::read_dir(profile.join(".lx-snapshot-staging"))
            .unwrap()
            .count(),
        0
    );
    let repository = Repository::open(profile).unwrap();
    assert!(
        repository
            .physical_snapshot_pending_records()
            .unwrap()
            .is_empty()
    );
}

#[test]
fn shutdown_retries_registration_failure_on_its_same_live_native_owner() {
    registration_failure_keeps_original_owner_until_recovery(false);
}

#[test]
fn explicit_recovery_retries_registration_failure_on_its_same_live_native_owner() {
    registration_failure_keeps_original_owner_until_recovery(true);
}

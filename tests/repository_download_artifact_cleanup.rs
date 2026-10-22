//! SQLite accounting tests use controller metadata facts; only the explicit
//! Windows case obtains a real native deletion receipt from an owned handle.

use lx_gpui_player::{
    domain::music::{LxMusicDtoVersion, TrackRecord},
    download::{
        CollisionPolicy, DownloadContext, DownloadRepresentationIdentity, DownloadState,
        FileIdentity, OwnedPartRecord, TransferCheckpoint,
        artifact::{OwnedArtifactAllocation, OwnedArtifactKind},
        planning::DownloadQuality,
        task_model::*,
    },
    repository::Repository,
};
use rusqlite::{Connection, params};
#[path = "support/native_producer_helpers.rs"]
#[allow(
    dead_code,
    reason = "Shared native fixtures expose constructors used by sibling integration targets"
)]
mod native_producer_helpers;
use std::{
    path::Path,
    sync::atomic::{AtomicUsize, Ordering},
};

fn identity(byte: u8) -> FileIdentity {
    FileIdentity {
        device: 1,
        object: format!("{byte:02x}").repeat(16),
    }
}
fn context() -> DownloadContext {
    DownloadContext {
        profile_id: "profile".into(),
        task_id: "task".into(),
        operation_epoch: 2,
        source_id: "controller-source".into(),
        source_content_sha256: "11".repeat(32),
        policy_epoch: 1,
    }
}
fn initial(root: FileIdentity) -> DownloadTask {
    let settings = DownloadSettings {
        embed_cover: false,
        ..DownloadSettings::default()
    };
    DownloadTask{id:"task".into(),profile_id:"profile".into(),original_record:TrackRecord::from_lx_dto(serde_json::json!({"id":"kw_1","name":"合法控制器记录","source":"kw","meta":{"songId":1}}),LxMusicDtoVersion::Modern).unwrap(),resolved_record:None,requested_quality:DownloadQuality::K128,actual_quality:DownloadQuality::K128,list_id:None,list_name_snapshot:None,order:0,revision:1,operation_epoch:1,settings:settings.clone(),destination:DownloadDestinationBinding{grant_id:"destination".into(),grant_epoch:1,root_identity:root,relative_components:vec!["song.mp3".into()],collision:CollisionPolicy::Skip},state:DownloadState::Waiting,pause_reason:None,owned_part:None,original_transfer:None,original_representation:None,url_bookmark:None,transfer_attempts:0,retry_at_ms:None,error:None,output_audio:None,audio_available:false,stages:DownloadStageRecords::for_settings(&settings,DownloadQuality::K128),commit_journal:None,created_at_ms:1,updated_at_ms:1}
}
fn setup(repo: &Repository, path: &Path, root: FileIdentity) -> DownloadTask {
    repo.put_download_destination(
        &StoredDestinationGrant {
            id: "destination".into(),
            profile_id: "profile".into(),
            path: path.to_path_buf(),
            identity: root.clone(),
            epoch: 1,
            revision: 1,
            revoked: false,
        },
        None,
        None,
        &|| Ok(()),
    )
    .unwrap();
    repo.insert_download_tasks(&[initial(root)], None, &|| Ok(()))
        .unwrap();
    let mut task = repo.download_task("task").unwrap().unwrap();
    task.revision += 1;
    task.operation_epoch = 2;
    task.state = DownloadState::AwaitingUrlApproval;
    task.url_bookmark = Some(
        PrivateUrlBookmark::new("https://fixture.invalid/song.mp3".into(), context()).unwrap(),
    );
    repo.update_download_task(&task, 1, 1, None, &|| Ok(()))
        .unwrap();
    task
}
fn allocation(path: &Path, root: FileIdentity, object: u8) -> OwnedArtifactAllocation {
    let nonce = format!("{object:02x}").repeat(16);
    OwnedArtifactAllocation {
        kind: OwnedArtifactKind::Transfer,
        record: OwnedPartRecord {
            basename: format!(".lx-download-{nonce}.part"),
            nonce,
            identity: identity(object),
            destination_identity: root.clone(),
            destination_epoch: 1,
            final_basename: "song.mp3".into(),
        },
        working_directory_path: path.to_path_buf(),
        working_directory_identity: root.clone(),
        selected_root_identity: root,
        relative_group: None,
        reserved_max_bytes: 65536,
        context: context(),
    }
}
fn checkpoint(bytes: u64, sha: String) -> TransferCheckpoint {
    TransferCheckpoint {
        bytes,
        sha256: sha,
        strong_etag: Some("\"fixture\"".into()),
        remote_total: None,
        completed_body: true,
        representation: Some(DownloadRepresentationIdentity {
            profile_id: "profile".into(),
            task_id: "task".into(),
            source_id: context().source_id,
            source_content_sha256: context().source_content_sha256,
            policy_epoch: 1,
            exact_url_sha256: "22".repeat(32),
            resolved_track_sha256: "33".repeat(32),
            actual_quality: DownloadQuality::K128,
        }),
    }
}
fn persist_output(
    repo: &Repository,
    mut task: DownloadTask,
    cp: TransferCheckpoint,
    output: FileIdentity,
) -> DownloadTask {
    let revision = task.revision;
    let epoch = task.operation_epoch;
    task.revision += 1;
    task.state = DownloadState::Paused;
    task.original_representation = cp.representation.clone();
    task.original_transfer = Some(cp.clone());
    task.audio_available = true;
    task.output_audio = Some(DownloadArtifactProof {
        basename: "song.mp3".into(),
        identity: output,
        bytes: cp.bytes,
        sha256: "44".repeat(32),
    });
    repo.update_download_task(&task, revision, epoch, None, &|| Ok(()))
        .unwrap();
    task
}
fn ready(directory: &tempfile::TempDir) -> (Repository, OwnedArtifactAllocation, DownloadTask) {
    let repo = Repository::open(directory.path().join("profile")).unwrap();
    let root = identity(0xaa);
    let task = setup(&repo, directory.path(), root.clone());
    let original = allocation(directory.path(), root, 1);
    native_producer_helpers::record_metadata_allocation(&repo, &original, &|| Ok(())).unwrap();
    repo.verify_download_artifact(&original.record.nonce, 2, 100, &"55".repeat(32), &|| Ok(()))
        .unwrap();
    let task = persist_output(&repo, task, checkpoint(100, "55".repeat(32)), identity(2));
    (repo, original, task)
}
fn row(repo: &Repository) -> lx_gpui_player::repository::DownloadOwnedArtifact {
    repo.download_owned_artifacts(Some("task"))
        .unwrap()
        .into_iter()
        .next()
        .unwrap()
}

#[test]
fn removal_ack_preserves_allocation_facts_and_replays_after_restart_without_user_changes() {
    let directory = native_producer_helpers::fixture_directory().unwrap();
    let (repo, original, _) = ready(&directory);
    let revision = repo.revision().unwrap();
    repo.acknowledge_removed_download_original("task", 2, &original.record)
        .unwrap();
    let removed = row(&repo);
    assert_eq!(removed.phase, "removed");
    assert_eq!(removed.actual_bytes, 0);
    assert_eq!(removed.reserved_bytes, 0);
    assert_eq!(removed.allocation, original);
    assert_eq!(removed.sha256.as_deref(), Some("55".repeat(32).as_str()));
    assert_eq!(repo.revision().unwrap(), revision);
    assert!(repo.changes_since(revision, 10).unwrap().is_empty());
    drop(repo);
    let repo = Repository::open(directory.path().join("profile")).unwrap();
    repo.acknowledge_removed_download_original("task", 2, &original.record)
        .unwrap();
    assert_eq!(row(&repo).phase, "removed");
    assert_eq!(repo.revision().unwrap(), revision);
    assert!(
        repo.record_download_artifact(&original, &|| Ok(()))
            .is_err()
    );
    assert_eq!(row(&repo).reserved_bytes, 0);
}

#[test]
fn allocated_missing_output_and_published_final_identity_are_never_removed() {
    let directory = native_producer_helpers::fixture_directory().unwrap();
    let repo = Repository::open(directory.path().join("profile")).unwrap();
    let root = identity(0xaa);
    let task = setup(&repo, directory.path(), root.clone());
    let original = allocation(directory.path(), root, 1);
    native_producer_helpers::record_metadata_allocation(&repo, &original, &|| Ok(())).unwrap();
    assert!(
        repo.acknowledge_removed_download_original("task", 2, &original.record)
            .is_err()
    );
    assert_eq!(row(&repo).phase, "allocated");
    repo.verify_download_artifact(&original.record.nonce, 2, 100, &"55".repeat(32), &|| Ok(()))
        .unwrap();
    assert!(
        repo.acknowledge_removed_download_original("task", 2, &original.record)
            .is_err()
    );
    let mut same = persist_output(
        &repo,
        task,
        checkpoint(100, "55".repeat(32)),
        original.record.identity.clone(),
    );
    same.output_audio.as_mut().unwrap().sha256 = "55".repeat(32);
    let previous = same.revision;
    same.revision += 1;
    repo.update_download_task(&same, previous, 2, None, &|| Ok(()))
        .unwrap();
    assert!(
        repo.acknowledge_removed_download_original("task", 2, &original.record)
            .is_err()
    );
    assert_eq!(row(&repo).phase, "verified");
    repo.publish_download_artifact(&original.record.nonce, 2, 100, &"55".repeat(32))
        .unwrap();
    assert!(
        repo.acknowledge_removed_download_original("task", 2, &original.record)
            .is_err()
    );
    assert_eq!(row(&repo).phase, "published");
    assert_eq!(row(&repo).actual_bytes, 100);
}

#[test]
fn exact_nonce_record_source_provenance_and_original_epoch_are_required() {
    let directory = native_producer_helpers::fixture_directory().unwrap();
    let (repo, original, mut task) = ready(&directory);
    let revision = repo.revision().unwrap();
    for wrong in [
        OwnedPartRecord {
            identity: identity(7),
            ..original.record.clone()
        },
        OwnedPartRecord {
            destination_epoch: 2,
            ..original.record.clone()
        },
        OwnedPartRecord {
            final_basename: "foreign.mp3".into(),
            ..original.record.clone()
        },
        OwnedPartRecord {
            nonce: "ff".repeat(16),
            basename: format!(".lx-download-{}.part", "ff".repeat(16)),
            ..original.record.clone()
        },
    ] {
        assert!(
            repo.acknowledge_removed_download_original("task", 2, &wrong)
                .is_err()
        );
    }
    assert!(
        repo.acknowledge_removed_download_original("other-task", 2, &original.record)
            .is_err()
    );
    assert!(
        repo.acknowledge_removed_download_original("task", 3, &original.record)
            .is_err()
    );
    assert_eq!(repo.revision().unwrap(), revision);
    assert_eq!(row(&repo).reserved_bytes, 100);
    let old = task.revision;
    task.revision += 1;
    task.operation_epoch = 3;
    task.url_bookmark = Some(
        PrivateUrlBookmark::new(
            "https://fixture.invalid/song.mp3".into(),
            DownloadContext {
                operation_epoch: 3,
                ..context()
            },
        )
        .unwrap(),
    );
    repo.update_download_task(&task, old, 2, None, &|| Ok(()))
        .unwrap();
    assert!(
        repo.acknowledge_removed_download_original("task", 3, &original.record)
            .is_err()
    );
    repo.acknowledge_removed_download_original("task", 2, &original.record)
        .unwrap();
    assert_eq!(row(&repo).phase, "removed");
    let directory = native_producer_helpers::fixture_directory().unwrap();
    let (repo, original, mut task) = ready(&directory);
    let old = task.revision;
    task.revision += 1;
    task.original_representation.as_mut().unwrap().source_id = "foreign-source".into();
    task.original_transfer.as_mut().unwrap().representation = task.original_representation.clone();
    repo.update_download_task(&task, old, 2, None, &|| Ok(()))
        .unwrap();
    assert!(
        repo.acknowledge_removed_download_original("task", 2, &original.record)
            .is_err()
    );
    assert_eq!(row(&repo).phase, "verified");
}

#[test]
fn sql_failure_and_final_guard_leave_verified_quota_for_recovery() {
    let directory = native_producer_helpers::fixture_directory().unwrap();
    let (repo, original, _) = ready(&directory);
    let revision = repo.revision().unwrap();
    let db = Connection::open(repo.path()).unwrap();
    db.execute_batch("CREATE TRIGGER fail_removal_ack BEFORE UPDATE ON download_owned_artifacts WHEN NEW.phase='removed' BEGIN SELECT RAISE(ABORT,'injected cleanup ACK failure'); END;").unwrap();
    assert!(
        repo.acknowledge_removed_download_original("task", 2, &original.record)
            .is_err()
    );
    assert_eq!(row(&repo).phase, "verified");
    assert_eq!(row(&repo).actual_bytes, 100);
    assert_eq!(row(&repo).reserved_bytes, 100);
    db.execute_batch("DROP TRIGGER fail_removal_ack;").unwrap();
    let calls = AtomicUsize::new(0);
    assert!(
        repo.acknowledge_removed_download_original_checked("task", 2, &original.record, &|| {
            anyhow::ensure!(
                calls.fetch_add(1, Ordering::SeqCst) == 0,
                "profile retired before ACK commit"
            );
            Ok(())
        })
        .is_err()
    );
    assert_eq!(row(&repo).phase, "verified");
    assert_eq!(repo.revision().unwrap(), revision);
    assert!(repo.changes_since(revision, 10).unwrap().is_empty());
    drop(db);
    drop(repo);
    let repo = Repository::open(directory.path().join("profile")).unwrap();
    repo.acknowledge_removed_download_original("task", 2, &original.record)
        .unwrap();
    assert_eq!(row(&repo).phase, "removed");
}

#[test]
fn tag_copy_pending_original_and_partial_checkpoint_cannot_release_original_accounting() {
    let directory = native_producer_helpers::fixture_directory().unwrap();
    let (repo, original, mut task) = ready(&directory);
    let mut copy = allocation(directory.path(), original.selected_root_identity.clone(), 7);
    copy.kind = OwnedArtifactKind::TagCopy;
    native_producer_helpers::record_metadata_allocation(&repo, &copy, &|| Ok(())).unwrap();
    repo.verify_download_artifact(&copy.record.nonce, 2, 100, &"55".repeat(32), &|| Ok(()))
        .unwrap();
    assert!(
        repo.acknowledge_removed_download_original("task", 2, &copy.record)
            .is_err()
    );
    let previous = task.revision;
    task.revision += 1;
    task.owned_part = Some(original.record.clone());
    repo.update_download_task(&task, previous, 2, None, &|| Ok(()))
        .unwrap();
    assert!(
        repo.acknowledge_removed_download_original("task", 2, &original.record)
            .is_err()
    );
    let previous = task.revision;
    task.revision += 1;
    task.owned_part = None;
    task.original_transfer.as_mut().unwrap().completed_body = false;
    repo.update_download_task(&task, previous, 2, None, &|| Ok(()))
        .unwrap();
    assert!(
        repo.acknowledge_removed_download_original("task", 2, &original.record)
            .is_err()
    );
    let values = repo.download_owned_artifacts(Some("task")).unwrap();
    assert!(
        values
            .iter()
            .all(|value| value.phase == "verified" && value.reserved_bytes == 100)
    );
}

#[test]
fn retained_removed_history_also_consumes_the_bounded_metadata_row_budget() {
    let directory = native_producer_helpers::fixture_directory().unwrap();
    let repo = Repository::open(directory.path().join("profile")).unwrap();
    let root = identity(0xaa);
    setup(&repo, directory.path(), root.clone());
    let mut db = Connection::open(repo.path()).unwrap();
    let transaction = db.transaction().unwrap();
    for index in 0..400_u32 {
        let mut value = allocation(directory.path(), root.clone(), 1);
        let nonce = format!("{index:032x}");
        value.record.nonce = nonce.clone();
        value.record.basename = format!(".lx-download-{nonce}.part");
        value.record.identity.object = format!("{:032x}", index + 1);
        let document = serde_json::to_string(&value).unwrap();
        transaction.execute("INSERT INTO download_owned_artifacts(nonce,task_id,operation_epoch,phase,actual_bytes,reserved_bytes,sha256,document,byte_size) VALUES(?1,'task',2,'removed',0,0,?2,?3,?4)",params![nonce,"55".repeat(32),document,document.len() as i64]).unwrap();
    }
    transaction.commit().unwrap();
    let extra = allocation(directory.path(), root, 0xfe);
    let revision = repo.revision().unwrap();
    assert!(
        native_producer_helpers::record_metadata_allocation(&repo, &extra, &|| Ok(())).is_err()
    );
    assert_eq!(repo.download_owned_artifacts(None).unwrap().len(), 400);
    assert_eq!(repo.revision().unwrap(), revision);
    let reserved: i64 = db
        .query_row(
            "SELECT sum(reserved_bytes) FROM download_owned_artifacts",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(reserved, 0);
}

#[test]
#[cfg(windows)]
fn actual_native_original_close_and_removal_precedes_accounting_ack_and_keeps_final_file() {
    use lx_gpui_player::{
        download::{DestinationGrant, DownloadCancel, run_download_producer},
        library::{authority::RootAuthority, scan::ScanContext},
    };
    use sha2::{Digest, Sha256};
    use std::{fs, io::Write, sync::Arc, time::Duration};
    let directory = native_producer_helpers::fixture_directory().unwrap();
    let media = directory.path().join("media");
    fs::create_dir(&media).unwrap();
    let mut grant = DestinationGrant::open_selected(&media, 1).unwrap();
    let repo = Arc::new(Repository::open(directory.path().join("profile")).unwrap());
    let task = setup(&repo, grant.path(), grant.identity().clone());
    let journal = Arc::new(native_producer_helpers::RepositoryJournal::new(
        repo.clone(),
    ));
    let mut part = journal
        .create(
            grant.plan("song.mp3", CollisionPolicy::Skip).unwrap(),
            OwnedArtifactKind::Transfer,
            context(),
            65536,
            DownloadCancel::default(),
        )
        .unwrap();
    let original = OwnedArtifactAllocation {
        kind: OwnedArtifactKind::Transfer,
        record: part.record().clone(),
        working_directory_path: grant.path().to_path_buf(),
        working_directory_identity: grant.working_directory_identity().clone(),
        selected_root_identity: grant.identity().clone(),
        relative_group: None,
        reserved_max_bytes: 65536,
        context: context(),
    };
    let body = b"self-authored private fixture bytes for native cleanup";
    let (returned, mut cp) = run_download_producer(move || {
        part.rewrite_io(DownloadCancel::default())?
            .write_all(body)?;
        let checkpoint = part.checkpoint(&DownloadCancel::default(), None, None, true)?;
        Ok((part, checkpoint))
    })
    .unwrap();
    part = returned;
    cp.representation = checkpoint(body.len() as u64, cp.sha256.clone()).representation;
    repo.verify_download_artifact(&original.record.nonce, 2, cp.bytes, &cp.sha256, &|| Ok(()))
        .unwrap();
    let final_path = grant.path().join("song.mp3");
    fs::write(&final_path, body).unwrap();
    let root = RootAuthority::open(
        grant.path(),
        false,
        &ScanContext::new(Duration::from_secs(30)),
    )
    .unwrap();
    let opened = root.open_file(&final_path).unwrap();
    let final_id = FileIdentity {
        device: u64::from_str_radix(&opened.identity().volume, 16).unwrap(),
        object: opened.identity().file_id.clone(),
    };
    let barrier = root.retirement_barrier();
    drop(opened);
    drop(root);
    assert!(barrier.wait(Duration::from_secs(3)));
    let mut completed = persist_output(&repo, task, cp.clone(), final_id.clone());
    let previous = completed.revision;
    completed.revision += 1;
    completed.output_audio.as_mut().unwrap().sha256 = format!("{:x}", Sha256::digest(body));
    repo.update_download_task(&completed, previous, 2, None, &|| Ok(()))
        .unwrap();
    grant
        .verify_owned_artifact(
            "song.mp3",
            &final_id,
            body.len() as u64,
            &cp.sha256,
            &DownloadCancel::default(),
        )
        .unwrap();
    let partial_path = part.path();
    let receipt = part.remove_owned_after_task_commit().unwrap();
    assert_eq!(receipt, original.record);
    assert!(!partial_path.exists());
    repo.acknowledge_removed_download_original("task", 2, &receipt)
        .unwrap();
    assert_eq!(row(&repo).phase, "removed");
    assert_eq!(fs::read(final_path).unwrap(), body);
    drop(journal);
    grant.close_retained_unique().unwrap();
    assert!(grant.verify().is_err());
}

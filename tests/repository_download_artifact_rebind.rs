//! Transfer accounting facts remain separate from native file authority.

use lx_gpui_player::{
    domain::{
        media::track_record_fingerprint,
        music::{LxMusicDtoVersion, TrackRecord},
    },
    download::{
        CollisionPolicy, DownloadContext, DownloadRepresentationIdentity, DownloadState,
        FileIdentity, MAX_DOWNLOAD_BYTES, OwnedPartRecord, TransferCheckpoint,
        artifact::{OwnedArtifactAllocation, OwnedArtifactKind},
        planning::DownloadQuality,
        task_model::*,
    },
    repository::{DownloadOwnedArtifact, Repository},
};
use std::{
    path::Path,
    sync::atomic::{AtomicUsize, Ordering},
};
#[path = "support/native_producer_helpers.rs"]
#[allow(
    dead_code,
    reason = "Shared native fixtures expose constructors used by sibling integration targets"
)]
mod native_producer_helpers;

const EMPTY_SHA256: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
const OLD_URL: &str = "https://fixture.invalid/original.mp3";
const NEW_URL: &str = "https://fixture.invalid/fresh.mp3";

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
        source_id: "original-source".into(),
        source_content_sha256: "11".repeat(32),
        policy_epoch: 1,
    }
}

fn setup(repo: &Repository, path: &Path, root: FileIdentity) -> DownloadTask {
    repo.put_download_destination(
        &StoredDestinationGrant {
            id: "destination".into(),
            profile_id: "profile".into(),
            path: path.to_owned(),
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
    let settings = DownloadSettings {
        embed_cover: false,
        ..Default::default()
    };
    let mut task = DownloadTask {
        id: "task".into(),
        profile_id: "profile".into(),
        original_record: TrackRecord::from_lx_dto(
            serde_json::json!({
                "id":"kw_transfer_rebind", "name":"Transfer provenance fixture", "source":"kw",
                "meta":{"songId":"transfer-rebind"}
            }),
            LxMusicDtoVersion::Modern,
        )
        .unwrap(),
        resolved_record: None,
        requested_quality: DownloadQuality::K128,
        actual_quality: DownloadQuality::K128,
        list_id: None,
        list_name_snapshot: None,
        order: 0,
        revision: 1,
        operation_epoch: 1,
        settings: settings.clone(),
        destination: DownloadDestinationBinding {
            grant_id: "destination".into(),
            grant_epoch: 1,
            root_identity: root,
            relative_components: vec!["song.mp3".into()],
            collision: CollisionPolicy::Skip,
        },
        state: DownloadState::Waiting,
        pause_reason: None,
        owned_part: None,
        original_transfer: None,
        original_representation: None,
        url_bookmark: None,
        transfer_attempts: 0,
        retry_at_ms: None,
        error: None,
        output_audio: None,
        audio_available: false,
        stages: DownloadStageRecords::for_settings(&settings, DownloadQuality::K128),
        commit_journal: None,
        created_at_ms: 1,
        updated_at_ms: 1,
    };
    repo.insert_download_tasks(&[task.clone()], None, &|| Ok(()))
        .unwrap();
    task.operation_epoch = 2;
    task.state = DownloadState::Paused;
    task.url_bookmark = Some(PrivateUrlBookmark::new(OLD_URL.into(), context()).unwrap());
    save(repo, &mut task, 1);
    task
}

fn save(repo: &Repository, task: &mut DownloadTask, old_epoch: u64) {
    let revision = task.revision;
    task.revision += 1;
    task.updated_at_ms += 1;
    repo.update_download_task(task, revision, old_epoch, None, &|| Ok(()))
        .unwrap();
}

fn allocation(
    path: &Path,
    root: FileIdentity,
    object: u8,
    reservation: u64,
) -> OwnedArtifactAllocation {
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
        working_directory_path: path.to_owned(),
        working_directory_identity: root.clone(),
        selected_root_identity: root,
        relative_group: None,
        reserved_max_bytes: reservation,
        context: context(),
    }
}

fn representation(task: &DownloadTask) -> DownloadRepresentationIdentity {
    let bookmark = task.url_bookmark.as_ref().unwrap();
    DownloadRepresentationIdentity {
        profile_id: task.profile_id.clone(),
        task_id: task.id.clone(),
        source_id: bookmark.context.source_id.clone(),
        source_content_sha256: bookmark.context.source_content_sha256.clone(),
        policy_epoch: bookmark.context.policy_epoch,
        exact_url_sha256: bookmark.sha256.clone(),
        resolved_track_sha256: track_record_fingerprint(
            task.resolved_record
                .as_ref()
                .unwrap_or(&task.original_record),
        )
        .unwrap(),
        actual_quality: task.actual_quality,
    }
}

fn empty(task: &DownloadTask) -> TransferCheckpoint {
    TransferCheckpoint {
        bytes: 0,
        sha256: EMPTY_SHA256.into(),
        strong_etag: None,
        remote_total: None,
        completed_body: false,
        representation: Some(representation(task)),
    }
}

fn fresh_context(repo: &Repository, task: &mut DownloadTask) {
    let old_epoch = task.operation_epoch;
    task.operation_epoch += 1;
    let mut current = context();
    current.operation_epoch = task.operation_epoch;
    current.source_id = "fresh-source".into();
    current.source_content_sha256 = "22".repeat(32);
    current.policy_epoch = 2;
    task.url_bookmark = Some(PrivateUrlBookmark::new(NEW_URL.into(), current).unwrap());
    save(repo, task, old_epoch);
}

fn row(repo: &Repository, nonce: &str) -> DownloadOwnedArtifact {
    repo.download_owned_artifacts(Some("task"))
        .unwrap()
        .into_iter()
        .find(|artifact| artifact.allocation.record.nonce == nonce)
        .unwrap()
}

fn assert_same(left: &DownloadOwnedArtifact, right: &DownloadOwnedArtifact) {
    assert_eq!(left.allocation, right.allocation);
    assert_eq!(left.phase, right.phase);
    assert_eq!(left.actual_bytes, right.actual_bytes);
    assert_eq!(left.reserved_bytes, right.reserved_bytes);
    assert_eq!(left.sha256, right.sha256);
}

struct Fixture {
    directory: native_producer_helpers::FixtureDirectory,
    repo: Repository,
    original: OwnedArtifactAllocation,
    task: DownloadTask,
}

impl Fixture {
    fn new(verified: bool, reservation: u64) -> Self {
        let directory = native_producer_helpers::fixture_directory().unwrap();
        let repo = Repository::open(directory.path().join("profile")).unwrap();
        let root = identity(0xaa);
        let mut task = setup(&repo, directory.path(), root.clone());
        let original = allocation(directory.path(), root, 1, reservation);
        native_producer_helpers::record_metadata_allocation(&repo, &original, &|| Ok(())).unwrap();
        task.owned_part = Some(original.record.clone());
        let mut checkpoint = empty(&task);
        if verified {
            checkpoint.bytes = 100;
            checkpoint.sha256 = "33".repeat(32);
            checkpoint.completed_body = true;
            checkpoint.strong_etag = Some("\"original\"".into());
            checkpoint.remote_total = Some(100);
            repo.verify_download_artifact(
                &original.record.nonce,
                2,
                100,
                &checkpoint.sha256,
                &|| Ok(()),
            )
            .unwrap();
        }
        task.original_representation = checkpoint.representation.clone();
        task.original_transfer = Some(checkpoint);
        save(&repo, &mut task, 2);
        Self {
            directory,
            repo,
            original,
            task,
        }
    }

    fn fresh(&mut self) -> (OwnedArtifactAllocation, TransferCheckpoint) {
        fresh_context(&self.repo, &mut self.task);
        let mut next = self.original.clone();
        next.context = self.task.url_bookmark.as_ref().unwrap().context.clone();
        (next, empty(&self.task))
    }
}

#[test]
fn allocated_and_verified_raw_objects_rebind_without_changing_task_revision_or_outbox() {
    for verified in [false, true] {
        let mut fixture = Fixture::new(verified, 65_536);
        let (next, checkpoint) = fixture.fresh();
        let revision = fixture.repo.revision().unwrap();
        let old_task = fixture.repo.download_task("task").unwrap().unwrap();
        fixture
            .repo
            .rebind_download_transfer_artifact(&next, &checkpoint, &|| Ok(()))
            .unwrap();
        let rebound = row(&fixture.repo, &next.record.nonce);
        assert_eq!(rebound.allocation, next);
        assert_eq!(rebound.phase, "allocated");
        assert_eq!(rebound.actual_bytes, 0);
        assert_eq!(rebound.reserved_bytes, next.reserved_max_bytes);
        assert!(rebound.sha256.is_none());
        let task = fixture.repo.download_task("task").unwrap().unwrap();
        assert_eq!(task.revision, old_task.revision);
        assert_eq!(task.original_transfer, old_task.original_transfer);
        assert_eq!(
            task.original_representation,
            old_task.original_representation
        );
        fixture
            .repo
            .rebind_download_transfer_artifact(&next, &checkpoint, &|| Ok(()))
            .unwrap();
        assert_same(&row(&fixture.repo, &next.record.nonce), &rebound);
        assert_eq!(fixture.repo.revision().unwrap(), revision);
        assert!(fixture.repo.changes_since(revision, 10).unwrap().is_empty());
        let profile = fixture.directory.path().join("profile");
        drop(fixture.repo);
        let reopened = Repository::open(profile).unwrap();
        assert_same(&row(&reopened, &next.record.nonce), &rebound);
        assert_eq!(reopened.revision().unwrap(), revision);
    }
}

#[test]
fn nonempty_or_stale_representation_checkpoints_cannot_rebind() {
    let mut fixture = Fixture::new(true, 65_536);
    let (next, checkpoint) = fixture.fresh();
    let original = row(&fixture.repo, &next.record.nonce);
    let revision = fixture.repo.revision().unwrap();
    let changes: &[fn(&mut TransferCheckpoint)] = &[
        |cp| cp.bytes = 1,
        |cp| cp.sha256 = "00".repeat(32),
        |cp| cp.completed_body = true,
        |cp| cp.strong_etag = Some("\"stale\"".into()),
        |cp| cp.remote_total = Some(0),
        |cp| cp.representation = None,
        |cp| cp.representation.as_mut().unwrap().profile_id = "foreign-profile".into(),
        |cp| cp.representation.as_mut().unwrap().task_id = "foreign-task".into(),
        |cp| cp.representation.as_mut().unwrap().source_id = "foreign-source".into(),
        |cp| cp.representation.as_mut().unwrap().source_content_sha256 = "44".repeat(32),
        |cp| cp.representation.as_mut().unwrap().policy_epoch += 1,
        |cp| cp.representation.as_mut().unwrap().exact_url_sha256 = "55".repeat(32),
        |cp| cp.representation.as_mut().unwrap().resolved_track_sha256 = "66".repeat(32),
        |cp| cp.representation.as_mut().unwrap().actual_quality = DownloadQuality::Flac,
    ];
    for change in changes {
        let mut wrong = checkpoint.clone();
        change(&mut wrong);
        assert!(
            fixture
                .repo
                .rebind_download_transfer_artifact(&next, &wrong, &|| Ok(()))
                .is_err()
        );
        assert_same(&row(&fixture.repo, &next.record.nonce), &original);
    }
    assert_eq!(fixture.repo.revision().unwrap(), revision);
    assert!(fixture.repo.changes_since(revision, 10).unwrap().is_empty());
}

#[test]
fn rebind_cannot_change_owned_identity_namespace_role_or_maximum_reservation() {
    let mut fixture = Fixture::new(true, 65_536);
    let (next, checkpoint) = fixture.fresh();
    let original = row(&fixture.repo, &next.record.nonce);
    let changes: &[fn(&mut OwnedArtifactAllocation)] = &[
        |a| a.record.identity = identity(0x77),
        |a| a.record.destination_epoch += 1,
        |a| a.record.final_basename = "foreign.mp3".into(),
        |a| {
            a.record.nonce = "ff".repeat(16);
            a.record.basename = format!(".lx-download-{}.part", a.record.nonce);
        },
        |a| a.working_directory_path = a.working_directory_path.join("foreign"),
        |a| a.working_directory_identity = identity(0x88),
        |a| {
            a.selected_root_identity = identity(0x99);
            a.record.destination_identity = a.selected_root_identity.clone();
        },
        |a| a.relative_group = Some("foreign-group".into()),
        |a| a.reserved_max_bytes -= 1,
        |a| a.kind = OwnedArtifactKind::TagCopy,
        |a| a.context.operation_epoch -= 1,
        |a| a.context.source_id = "foreign-source".into(),
        |a| a.context.profile_id = "foreign-profile".into(),
        |a| a.context.task_id = "foreign-task".into(),
    ];
    for change in changes {
        let mut wrong = next.clone();
        change(&mut wrong);
        assert!(
            fixture
                .repo
                .rebind_download_transfer_artifact(&wrong, &checkpoint, &|| Ok(()))
                .is_err()
        );
        assert_same(&row(&fixture.repo, &next.record.nonce), &original);
    }
}

#[test]
fn changed_source_needs_a_new_epoch_and_same_context_cannot_erase_verified_bytes() {
    let mut fixture = Fixture::new(true, 65_536);
    let mut changed = context();
    changed.source_id = "changed-source".into();
    fixture.task.url_bookmark =
        Some(PrivateUrlBookmark::new(NEW_URL.into(), changed.clone()).unwrap());
    save(&fixture.repo, &mut fixture.task, 2);
    let mut allocation = fixture.original.clone();
    allocation.context = changed;
    assert!(
        fixture
            .repo
            .rebind_download_transfer_artifact(&allocation, &empty(&fixture.task), &|| Ok(()))
            .is_err()
    );
    let (next, checkpoint) = fixture.fresh();
    fixture
        .repo
        .rebind_download_transfer_artifact(&next, &checkpoint, &|| Ok(()))
        .unwrap();
    fixture
        .repo
        .verify_download_artifact(
            &next.record.nonce,
            next.context.operation_epoch,
            20,
            &"77".repeat(32),
            &|| Ok(()),
        )
        .unwrap();
    assert!(
        fixture
            .repo
            .rebind_download_transfer_artifact(&next, &checkpoint, &|| Ok(()))
            .is_err()
    );
    let retained = row(&fixture.repo, &next.record.nonce);
    assert_eq!(retained.phase, "verified");
    assert_eq!(retained.actual_bytes, 20);
    assert_eq!(retained.reserved_bytes, 20);
}

#[test]
fn current_task_output_owner_url_and_destination_must_still_match() {
    let mut fixture = Fixture::new(true, 65_536);
    let (next, checkpoint) = fixture.fresh();
    let baseline = row(&fixture.repo, &next.record.nonce);
    fixture.task.output_audio = Some(DownloadArtifactProof {
        basename: "song.mp3".into(),
        identity: identity(9),
        bytes: 100,
        sha256: "77".repeat(32),
    });
    save(&fixture.repo, &mut fixture.task, 3);
    assert!(
        fixture
            .repo
            .rebind_download_transfer_artifact(&next, &checkpoint, &|| Ok(()))
            .is_err()
    );
    fixture.task.output_audio = None;
    fixture.task.owned_part.as_mut().unwrap().identity = identity(9);
    save(&fixture.repo, &mut fixture.task, 3);
    assert!(
        fixture
            .repo
            .rebind_download_transfer_artifact(&next, &checkpoint, &|| Ok(()))
            .is_err()
    );
    fixture.task.owned_part = Some(next.record.clone());
    fixture.task.url_bookmark = Some(
        PrivateUrlBookmark::new(
            "https://fixture.invalid/different.mp3".into(),
            next.context.clone(),
        )
        .unwrap(),
    );
    save(&fixture.repo, &mut fixture.task, 3);
    assert!(
        fixture
            .repo
            .rebind_download_transfer_artifact(&next, &checkpoint, &|| Ok(()))
            .is_err()
    );
    fixture.task.url_bookmark =
        Some(PrivateUrlBookmark::new(NEW_URL.into(), next.context.clone()).unwrap());
    save(&fixture.repo, &mut fixture.task, 3);
    let mut grant = fixture.repo.download_destinations().unwrap().pop().unwrap();
    grant.revoked = true;
    grant.epoch += 1;
    grant.revision += 1;
    fixture
        .repo
        .put_download_destination(&grant, Some(1), None, &|| Ok(()))
        .unwrap();
    assert!(
        fixture
            .repo
            .rebind_download_transfer_artifact(&next, &checkpoint, &|| Ok(()))
            .is_err()
    );
    assert_same(&row(&fixture.repo, &next.record.nonce), &baseline);
}

#[test]
fn terminal_publication_and_removal_accounting_cannot_be_rebound() {
    for phase in ["published", "removed"] {
        let mut fixture = Fixture::new(true, 65_536);
        let db = rusqlite::Connection::open(fixture.repo.path()).unwrap();
        db.execute("UPDATE download_owned_artifacts SET phase=?2,actual_bytes=?3,reserved_bytes=0 WHERE nonce=?1",
            rusqlite::params![fixture.original.record.nonce, phase, if phase == "published" { 100 } else { 0 }]).unwrap();
        let (next, checkpoint) = fixture.fresh();
        let baseline = row(&fixture.repo, &next.record.nonce);
        assert!(
            fixture
                .repo
                .rebind_download_transfer_artifact(&next, &checkpoint, &|| Ok(()))
                .is_err()
        );
        assert_same(&row(&fixture.repo, &next.record.nonce), &baseline);
    }
}

#[test]
fn sql_failure_and_both_live_guard_boundaries_roll_back_rebind() {
    let mut fixture = Fixture::new(true, 65_536);
    let (next, checkpoint) = fixture.fresh();
    let baseline = row(&fixture.repo, &next.record.nonce);
    let revision = fixture.repo.revision().unwrap();
    assert!(
        fixture
            .repo
            .rebind_download_transfer_artifact(&next, &checkpoint, &|| anyhow::bail!(
                "retired before rebind transaction"
            ))
            .is_err()
    );
    assert_same(&row(&fixture.repo, &next.record.nonce), &baseline);
    let calls = AtomicUsize::new(0);
    assert!(
        fixture
            .repo
            .rebind_download_transfer_artifact(&next, &checkpoint, &|| {
                anyhow::ensure!(
                    calls.fetch_add(1, Ordering::SeqCst) == 0,
                    "retired before rebind commit"
                );
                Ok(())
            })
            .is_err()
    );
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_same(&row(&fixture.repo, &next.record.nonce), &baseline);
    let db = rusqlite::Connection::open(fixture.repo.path()).unwrap();
    db.execute_batch("CREATE TRIGGER fail_transfer_rebind BEFORE UPDATE OF operation_epoch ON download_owned_artifacts BEGIN SELECT RAISE(ABORT,'injected rebind ACK failure'); END;").unwrap();
    assert!(
        fixture
            .repo
            .rebind_download_transfer_artifact(&next, &checkpoint, &|| Ok(()))
            .is_err()
    );
    assert_same(&row(&fixture.repo, &next.record.nonce), &baseline);
    assert_eq!(fixture.repo.revision().unwrap(), revision);
    assert!(fixture.repo.changes_since(revision, 10).unwrap().is_empty());
    db.execute_batch("DROP TRIGGER fail_transfer_rebind;")
        .unwrap();
    fixture
        .repo
        .rebind_download_transfer_artifact(&next, &checkpoint, &|| Ok(()))
        .unwrap();
}

#[test]
fn restoring_original_maximum_reservation_obeys_the_shared_four_gib_budget() {
    let mut fixture = Fixture::new(false, MAX_DOWNLOAD_BYTES);
    for object in 2..=4 {
        let other = allocation(
            fixture.directory.path(),
            fixture.original.selected_root_identity.clone(),
            object,
            MAX_DOWNLOAD_BYTES,
        );
        native_producer_helpers::record_metadata_allocation(&fixture.repo, &other, &|| Ok(()))
            .unwrap();
    }
    fixture
        .repo
        .verify_download_artifact(
            &fixture.original.record.nonce,
            2,
            100,
            &"33".repeat(32),
            &|| Ok(()),
        )
        .unwrap();
    let extra = allocation(
        fixture.directory.path(),
        fixture.original.selected_root_identity.clone(),
        5,
        1,
    );
    native_producer_helpers::record_metadata_allocation(&fixture.repo, &extra, &|| Ok(())).unwrap();
    let (next, checkpoint) = fixture.fresh();
    let baseline = row(&fixture.repo, &next.record.nonce);
    let revision = fixture.repo.revision().unwrap();
    assert!(
        fixture
            .repo
            .rebind_download_transfer_artifact(&next, &checkpoint, &|| Ok(()))
            .is_err()
    );
    assert_same(&row(&fixture.repo, &next.record.nonce), &baseline);
    assert_eq!(fixture.repo.revision().unwrap(), revision);
    assert!(fixture.repo.changes_since(revision, 10).unwrap().is_empty());
    fixture
        .repo
        .verify_download_artifact(&extra.record.nonce, 2, 0, EMPTY_SHA256, &|| Ok(()))
        .unwrap();
    fixture
        .repo
        .rebind_download_transfer_artifact(&next, &checkpoint, &|| Ok(()))
        .unwrap();
    let reserved = fixture
        .repo
        .download_owned_artifacts(None)
        .unwrap()
        .iter()
        .map(|artifact| artifact.reserved_bytes)
        .sum::<u64>();
    assert_eq!(reserved, 4 * MAX_DOWNLOAD_BYTES);
}

#[test]
#[cfg(windows)]
fn actual_native_zero_length_restart_is_reported_while_the_same_original_handle_is_held() {
    use lx_gpui_player::download::{DestinationGrant, DownloadCancel, run_download_producer};
    use std::io::Write;
    use std::sync::Arc;
    let directory = native_producer_helpers::fixture_directory().unwrap();
    let media = directory.path().join("media");
    std::fs::create_dir(&media).unwrap();
    let mut grant = DestinationGrant::open_selected(&media, 1).unwrap();
    let repo = Arc::new(Repository::open(directory.path().join("profile")).unwrap());
    let mut task = setup(&repo, grant.path(), grant.identity().clone());
    let plan = grant.plan("song.mp3", CollisionPolicy::Skip).unwrap();
    let journal = Arc::new(native_producer_helpers::RepositoryJournal::new(
        repo.clone(),
    ));
    let mut part = journal
        .create(
            plan.clone(),
            OwnedArtifactKind::Transfer,
            context(),
            65_536,
            DownloadCancel::default(),
        )
        .unwrap();
    let original = OwnedArtifactAllocation {
        kind: OwnedArtifactKind::Transfer,
        record: part.record().clone(),
        working_directory_path: grant.path().to_owned(),
        working_directory_identity: grant.working_directory_identity().clone(),
        selected_root_identity: grant.identity().clone(),
        relative_group: None,
        reserved_max_bytes: 65_536,
        context: context(),
    };
    let cancel = DownloadCancel::default();
    let write_cancel = cancel.clone();
    let (returned, mut old) = run_download_producer(move || {
        part.rewrite_io(write_cancel.clone())?
            .write_all(b"original private transfer bytes")?;
        let checkpoint = part.checkpoint(&write_cancel, None, None, false)?;
        Ok((part, checkpoint))
    })
    .unwrap();
    part = returned;
    old.representation = Some(representation(&task));
    repo.verify_download_artifact(
        &original.record.nonce,
        2,
        old.bytes,
        &old.sha256,
        &|| Ok(()),
    )
    .unwrap();
    task.owned_part = Some(original.record.clone());
    task.original_representation = old.representation.clone();
    task.original_transfer = Some(old);
    save(&repo, &mut task, 2);
    fresh_context(&repo, &mut task);
    let reset_cancel = cancel.clone();
    let (returned, mut reset) = run_download_producer(move || {
        let checkpoint = part.restart_for_representation(&reset_cancel)?;
        Ok((part, checkpoint))
    })
    .unwrap();
    part = returned;
    reset.representation = Some(representation(&task));
    assert_eq!(reset.bytes, 0);
    assert_eq!(reset.sha256, EMPTY_SHA256);
    assert_eq!(part.record(), &original.record);
    let mut next = original.clone();
    next.context = task.url_bookmark.as_ref().unwrap().context.clone();
    let revision = repo.revision().unwrap();
    repo.rebind_download_transfer_artifact(&next, &reset, &|| {
        part.verify()?;
        anyhow::ensure!(
            part.is_empty()?,
            "Original handle changed before accounting ACK"
        );
        Ok(())
    })
    .unwrap();
    assert_eq!(repo.revision().unwrap(), revision);
    assert_eq!(row(&repo, &next.record.nonce).allocation, next);
    assert_eq!(row(&repo, &next.record.nonce).reserved_bytes, 65_536);
    assert!(!grant.path().join("song.mp3").exists());
    task.original_representation = reset.representation.clone();
    task.original_transfer = Some(reset);
    save(&repo, &mut task, 3);
    assert_eq!(
        repo.download_task("task")
            .unwrap()
            .unwrap()
            .original_representation,
        task.original_representation
    );
    let receipt = part.remove_owned_after_task_commit().unwrap();
    assert_eq!(receipt, original.record);
    assert!(!plan.grant().path().join(&receipt.basename).exists());
    drop(plan);
    drop(journal);
    grant.close_retained_unique().unwrap();
    assert!(grant.verify().is_err());
}

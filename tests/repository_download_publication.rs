use lx_gpui_player::{
    domain::music::{LxMusicDtoVersion, TrackRecord},
    download::{
        CollisionPolicy, DownloadContext, DownloadState, FileIdentity, OwnedPartRecord,
        artifact::{OwnedArtifactAllocation, OwnedArtifactIntent, OwnedArtifactKind},
        planning::DownloadQuality,
        task_model::*,
    },
    repository::Repository,
};
use rusqlite::Connection;
use std::sync::atomic::{AtomicUsize, Ordering};

fn identity(byte: &str) -> FileIdentity {
    FileIdentity {
        device: 1,
        object: byte.repeat(16),
    }
}
struct Case {
    repository: Repository,
    directory: tempfile::TempDir,
    previous: DownloadTask,
    next: DownloadTask,
    artifacts: Vec<(String, u64, u64, String)>,
}
fn case(sidecar: bool) -> Case {
    let temporary_root = std::env::temp_dir().join("lx-native-fixtures");
    let fixture_root = temporary_root.as_path();
    std::fs::create_dir_all(fixture_root).unwrap();
    let directory = tempfile::Builder::new()
        .prefix("publication-sql-")
        .tempdir_in(fixture_root)
        .unwrap();
    let repository = Repository::open(directory.path().join("profile")).unwrap();
    let root = identity("aa");
    let context = DownloadContext {
        profile_id: "profile".into(),
        task_id: "task".into(),
        operation_epoch: 2,
        source_id: "fixture-source".into(),
        source_content_sha256: "11".repeat(32),
        policy_epoch: 1,
    };
    let settings = DownloadSettings {
        embed_cover: false,
        download_lyrics: sidecar,
        ..DownloadSettings::default()
    };
    let mut task = DownloadTask {
        id: "task".into(), profile_id: "profile".into(),
        original_record: TrackRecord::from_lx_dto(serde_json::json!({"source":"kw","songmid":"fixture-song","name":"原创测试记录","singer":"测试作者","types":[{"type":"128k"}],"_types":{"128k":{}}}), LxMusicDtoVersion::Legacy).unwrap(),
        resolved_record: None, requested_quality: DownloadQuality::K128, actual_quality: DownloadQuality::K128,
        list_id: None, list_name_snapshot: None, order: 0, revision: 1, operation_epoch: 1,
        destination: DownloadDestinationBinding { grant_id: "destination".into(), grant_epoch: 1, root_identity: root.clone(), relative_components: vec!["song.mp3".into()], collision: CollisionPolicy::Skip },
        state: DownloadState::Waiting, pause_reason: None, owned_part: None, original_transfer: None,
        original_representation: None, url_bookmark: None, transfer_attempts: 0, retry_at_ms: None,
        error: None, output_audio: None, audio_available: false,
        stages: DownloadStageRecords::for_settings(&settings, DownloadQuality::K128), settings,
        commit_journal: None, created_at_ms: 1, updated_at_ms: 1,
    };
    repository
        .put_download_destination(
            &StoredDestinationGrant {
                id: "destination".into(),
                profile_id: "profile".into(),
                path: directory.path().to_path_buf(),
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
    repository
        .insert_download_tasks(&[task.clone()], None, &|| Ok(()))
        .unwrap();
    task.revision = 2;
    task.operation_epoch = 2;
    task.state = DownloadState::AwaitingUrlApproval;
    task.url_bookmark = Some(
        PrivateUrlBookmark::new("https://fixture.invalid/song.mp3".into(), context.clone())
            .unwrap(),
    );
    repository
        .update_download_task(&task, 1, 1, None, &|| Ok(()))
        .unwrap();
    let mut artifacts = Vec::new();
    let mut next = task.clone();
    next.revision += 1;
    next.updated_at_ms += 1;
    next.state = DownloadState::Paused;
    next.audio_available = true;
    for (kind, object, basename, bytes, hash) in [
        (OwnedArtifactKind::Transfer, "01", "song.mp3", 100u64, "55"),
        (
            OwnedArtifactKind::LyricSidecar,
            "02",
            "song.lrc",
            20u64,
            "66",
        ),
    ]
    .into_iter()
    .take(if sidecar { 2 } else { 1 })
    {
        let nonce = object.repeat(16);
        let proof = DownloadArtifactProof {
            basename: basename.into(),
            identity: identity(object),
            bytes,
            sha256: hash.repeat(32),
        };
        let allocation = OwnedArtifactAllocation {
            kind,
            record: OwnedPartRecord {
                basename: format!(".lx-download-{nonce}.part"),
                nonce: nonce.clone(),
                identity: proof.identity.clone(),
                destination_identity: root.clone(),
                destination_epoch: 1,
                final_basename: basename.into(),
            },
            working_directory_path: directory.path().to_path_buf(),
            working_directory_identity: root.clone(),
            selected_root_identity: root.clone(),
            relative_group: None,
            reserved_max_bytes: 65536,
            context: context.clone(),
        };
        repository
            .record_download_allocation_intent(
                &OwnedArtifactIntent::from_created(&allocation),
                &|| Ok(()),
            )
            .unwrap();
        repository
            .record_download_artifact(&allocation, &|| Ok(()))
            .unwrap();
        repository
            .verify_download_artifact(&nonce, 2, bytes, &proof.sha256, &|| Ok(()))
            .unwrap();
        artifacts.push((nonce, 2, bytes, proof.sha256.clone()));
        if kind == OwnedArtifactKind::LyricSidecar {
            next.stages.lyrics.state = DownloadStageState::Succeeded;
            next.stages.lyrics.artifact = Some(proof);
        } else {
            next.output_audio = Some(proof);
        }
    }
    Case {
        directory,
        repository,
        previous: task,
        next,
        artifacts,
    }
}
fn assert_uncommitted(case: &Case, revision: u64) {
    assert_eq!(case.repository.revision().unwrap(), revision);
    assert_eq!(
        serde_json::to_value(case.repository.download_task("task").unwrap().unwrap()).unwrap(),
        serde_json::to_value(&case.previous).unwrap()
    );
    assert!(
        case.repository
            .changes_since(revision, 20)
            .unwrap()
            .is_empty()
    );
    assert!(
        case.repository
            .download_owned_artifacts(Some("task"))
            .unwrap()
            .iter()
            .all(|row| row.phase == "verified" && row.reserved_bytes == row.actual_bytes)
    );
}

#[test]
fn actual_artifact_sql_failure_rolls_back_the_task_output_and_change_event() {
    let case = case(false);
    let revision = case.repository.revision().unwrap();
    let fault = Connection::open(case.repository.path()).unwrap();
    fault.execute_batch("CREATE TRIGGER reject_publication BEFORE UPDATE OF phase ON download_owned_artifacts WHEN NEW.phase='published' BEGIN SELECT RAISE(ABORT,'actual artifact ACK refusal'); END;").unwrap();
    let error =
        case.repository
            .commit_download_publication(&case.next, 2, 2, Some(revision), &case.artifacts, &|| {
                Ok(())
            })
            .unwrap_err();
    assert!(error.to_string().contains("actual artifact ACK refusal"));
    assert_uncommitted(&case, revision);
    fault
        .execute_batch("DROP TRIGGER reject_publication;")
        .unwrap();
    let change =
        case.repository
            .commit_download_publication(&case.next, 2, 2, Some(revision), &case.artifacts, &|| {
                Ok(())
            })
            .unwrap();
    assert_eq!(change.revision, revision + 1);
    let row = case
        .repository
        .download_owned_artifacts(Some("task"))
        .unwrap()
        .pop()
        .unwrap();
    assert_eq!(row.phase, "published");
    assert_eq!(row.reserved_bytes, 0);
    assert!(
        case.repository
            .download_task("task")
            .unwrap()
            .unwrap()
            .audio_available
    );
}

#[test]
fn second_artifact_sql_refusal_rolls_back_first_artifact_task_and_outbox_together() {
    let case = case(true);
    let revision = case.repository.revision().unwrap();
    let fault = Connection::open(case.repository.path()).unwrap();
    fault.execute_batch("CREATE TRIGGER reject_sidecar_publication BEFORE UPDATE OF phase ON download_owned_artifacts WHEN NEW.phase='published' AND json_extract(NEW.document,'$.kind')='lyric_sidecar' BEGIN SELECT RAISE(ABORT,'actual second artifact refusal'); END;").unwrap();
    assert!(
        case.repository
            .commit_download_publication(&case.next, 2, 2, Some(revision), &case.artifacts, &|| Ok(
                ()
            ))
            .is_err()
    );
    assert_uncommitted(&case, revision);
    fault
        .execute_batch("DROP TRIGGER reject_sidecar_publication;")
        .unwrap();
    case.repository
        .commit_download_publication(
            &case.next,
            2,
            2,
            Some(revision),
            &case.artifacts,
            &|| Ok(()),
        )
        .unwrap();
    assert_eq!(
        case.repository.changes_since(revision, 20).unwrap().len(),
        1
    );
    assert!(
        case.repository
            .download_owned_artifacts(Some("task"))
            .unwrap()
            .iter()
            .all(|row| row.phase == "published" && row.reserved_bytes == 0)
    );
}

#[test]
fn actual_live_guard_refusal_after_sql_changes_preserves_every_publication_row() {
    let case = case(true);
    let revision = case.repository.revision().unwrap();
    let checks = AtomicUsize::new(0);
    let check = || {
        if checks.fetch_add(1, Ordering::SeqCst) >= 2 {
            anyhow::bail!("Actual retained owner changed before commit");
        }
        Ok(())
    };
    assert!(
        case.repository
            .commit_download_publication(&case.next, 2, 2, Some(revision), &case.artifacts, &check)
            .is_err()
    );
    assert!(checks.load(Ordering::SeqCst) >= 3);
    assert_uncommitted(&case, revision);
    assert!(case.directory.path().exists());
}

#[test]
fn incomplete_or_duplicate_publication_batches_never_commit_a_partial_ack() {
    let case = case(true);
    let revision = case.repository.revision().unwrap();
    for artifacts in [
        vec![case.artifacts[1].clone()],
        vec![case.artifacts[0].clone()],
        vec![case.artifacts[0].clone(), case.artifacts[0].clone()],
    ] {
        assert!(
            case.repository
                .commit_download_publication(&case.next, 2, 2, Some(revision), &artifacts, &|| Ok(
                    ()
                ))
                .is_err()
        );
        assert_uncommitted(&case, revision);
    }
}

#[test]
fn unresolved_native_intent_rejects_publication_without_changing_verified_objects() {
    let case = case(true);
    let mut pending = OwnedArtifactIntent::from_created(
        &case
            .repository
            .download_owned_artifacts(Some("task"))
            .unwrap()[0]
            .allocation,
    );
    pending.kind = OwnedArtifactKind::TagCopy;
    pending.nonce = "39".repeat(16);
    pending.basename = format!(".lx-download-{}.part", pending.nonce);
    pending.final_basename = "song.mp3".into();
    case.repository
        .record_download_allocation_intent(&pending, &|| Ok(()))
        .unwrap();
    let revision = case.repository.revision().unwrap();
    assert!(
        case.repository
            .commit_download_publication(&case.next, 2, 2, Some(revision), &case.artifacts, &|| Ok(
                ()
            ))
            .is_err()
    );
    assert_uncommitted(&case, revision);
    assert_eq!(
        case.repository
            .download_allocation_intents(Some("task"))
            .unwrap(),
        vec![pending]
    );
}

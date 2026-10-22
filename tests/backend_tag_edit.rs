#![cfg(windows)]
//! Real backend consent, native tag installation and physical quota retirement.
#[allow(
    dead_code,
    reason = "Shared encoded fixture also exposes sibling formats"
)]
#[path = "support/encoded_audio_fixture.rs"]
mod encoded;

use lofty::{
    file::TaggedFileExt,
    id3::v2::Id3v2Tag,
    probe::Probe,
    tag::{ItemKey, TagType},
};
use lx_gpui_player::{
    audio::PlaybackStatus,
    backend::{
        BackendCommand, BackendConfig, BackendError, BackendEvent, BackendHandle, BackendQuery,
        BackendQueryResult, CommandEnvelope, ErrorKind, LocalTagEditPatch, LocalTagReadView,
        ReceiptState, TagEditField,
    },
    repository::{Repository, TagEditArtifactPhase, TagEditBudget},
};
use sha2::{Digest, Sha256};
use std::{
    fs,
    path::PathBuf,
    thread,
    time::{Duration, Instant},
};

fn command(id: &str, command: BackendCommand) -> CommandEnvelope {
    CommandEnvelope {
        request_id: id.into(),
        origin: "backend-tag-test".into(),
        expected_revision: None,
        command,
    }
}
fn terminal(
    handle: &BackendHandle,
    id: &str,
    command_value: BackendCommand,
) -> (bool, Option<BackendError>) {
    let mut cursor = handle.event_cursor();
    let receipt = match handle.execute(command(id, command_value)) {
        Ok(receipt) => receipt,
        Err(error) => return (false, Some(error)),
    };
    assert_eq!(
        receipt.state,
        ReceiptState::Accepted,
        "Tag/import worker must publish its real terminal event"
    );
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        for event in handle.events_after(cursor).unwrap() {
            cursor = event.event_id;
            if let BackendEvent::OperationFinished {
                operation_id,
                success,
                error,
            } = event.payload
                && operation_id == receipt.operation_id
            {
                return (success, error);
            }
        }
        assert!(
            Instant::now() < deadline,
            "Actual backend operation {id} did not finish: {:?}",
            handle.snapshot()
        );
        thread::sleep(Duration::from_millis(10));
    }
}
fn success(handle: &BackendHandle, id: &str, value: BackendCommand) {
    let result = terminal(handle, id, value);
    assert!(
        result.0,
        "Actual backend operation {id} failed: {:?}",
        result.1
    );
}
fn read(handle: &BackendHandle, id: &str, asset: &str, operation: &str) -> LocalTagReadView {
    success(
        handle,
        id,
        BackendCommand::ReadLocalTags {
            asset_id: asset.into(),
            tag_operation_id: operation.into(),
        },
    );
    let BackendQueryResult::LocalTags(Some(view)) = handle
        .query(BackendQuery::LocalTags {
            operation_id: id.into(),
        })
        .unwrap()
    else {
        panic!("Completed actual local-tag reader has no selection");
    };
    assert!(view.metadata_error.is_none(), "{:?}", view.metadata_error);
    view
}
fn budget() -> TagEditBudget {
    TagEditBudget {
        original_bytes: 1024 * 1024,
        journal_bytes: 2 * 1024 * 1024,
        backup_bytes: 1024 * 1024,
        candidate_bytes: 1024 * 1024,
    }
}
fn patch() -> LocalTagEditPatch {
    LocalTagEditPatch {
        title: TagEditField::Set("实际后端标签标题".into()),
        artists: TagEditField::Set(vec!["作者甲".into(), "Artist B".into()]),
        album: TagEditField::Set("后端校验专辑".into()),
        lyrics: TagEditField::Set("[00:00.00]实际标签歌词\n[00:00.10]第二行".into()),
        ..Default::default()
    }
}

#[test]
fn fresh_profile_public_tag_api_requires_independent_consent_and_retires_actual_owned_files() {
    let base = PathBuf::from(r"E:\codex操作目录\tasks\lx-gpui-player\test-temp");
    fs::create_dir_all(&base).unwrap();
    let temp = tempfile::Builder::new()
        .prefix("backend-tag-edit-")
        .tempdir_in(base)
        .unwrap();
    let profile = temp.path().join("fresh-profile");
    let path = temp.path().join("isolated-selected.mp3");
    let payload = encoded::silent_mpeg_layer3();
    fs::write(&path, &payload).unwrap();
    assert!(!profile.join("tag-edits").exists());
    let backend = BackendHandle::start(BackendConfig::headless(profile.clone())).unwrap();
    assert!(!profile.join("tag-edits").exists());
    success(
        &backend,
        "tag-fixture-import",
        BackendCommand::ImportAudio {
            paths: vec![path.clone()],
        },
    );
    let BackendQueryResult::LibraryAssets(page) = backend
        .query(BackendQuery::LibraryAssets {
            root_id: None,
            after_asset_id: None,
            limit: 10,
            expected_revision: None,
        })
        .unwrap()
    else {
        panic!("Library asset page missing");
    };
    assert_eq!(page.items.len(), 1);
    assert!(page.items[0].authorized && page.items[0].explicit_file_grant);
    let asset = page.items[0].asset.id.clone();
    let selection = read(&backend, "tag-fixture-read", &asset, "tag-fixture-durable").selection;
    assert_eq!(selection.path, path);
    assert_eq!(
        selection.expected_body_sha256.as_deref(),
        Some(format!("{:x}", Sha256::digest(&payload)).as_str())
    );
    assert!(!profile.join("tag-edits").exists());

    let denied = terminal(
        &backend,
        "tag-fixture-read-is-not-write",
        BackendCommand::EditLocalTags {
            approval_id: "tag-fixture-read".into(),
            expected_approval_epoch: 1,
            budget: budget(),
            patch: patch(),
        },
    );
    assert!(
        !denied.0,
        "Existing library read consent must not supply independent tag write approval"
    );
    assert_eq!(fs::read(&path).unwrap(), payload);
    let BackendQueryResult::TagEdits(entries) = backend.query(BackendQuery::TagEdits).unwrap()
    else {
        panic!("Tag edit query missing");
    };
    assert!(entries.is_empty());

    success(
        &backend,
        "tag-fixture-approve",
        BackendCommand::ApproveTagWrite {
            approval_id: "tag-fixture-write-consent".into(),
            selection: selection.clone(),
        },
    );
    let BackendQueryResult::TagWriteApproval(Some(approval)) = backend
        .query(BackendQuery::TagWriteApproval {
            approval_id: "tag-fixture-write-consent".into(),
        })
        .unwrap()
    else {
        panic!("Independent write approval missing");
    };
    assert!(approval.approval_epoch > 0 && !approval.revoked);
    assert!(
        !profile.join("tag-edits").exists(),
        "Actual install must create its own staging directory"
    );
    success(
        &backend,
        "tag-fixture-edit",
        BackendCommand::EditLocalTags {
            approval_id: approval.approval_id.clone(),
            expected_approval_epoch: approval.approval_epoch,
            budget: budget(),
            patch: patch(),
        },
    );
    let BackendQueryResult::TagEdit(Some(view)) = backend
        .query(BackendQuery::TagEdit {
            operation_id: selection.operation_id.clone(),
        })
        .unwrap()
    else {
        panic!("Completed durable edit is absent");
    };
    assert!(view.finished && view.operation.finished);
    assert!(!view.running && !view.has_live_owner && !view.pending_recovery);
    assert!(view.operation.original_owner_retired && view.operation.original_close.is_some());
    assert_eq!(view.operation.artifacts.len(), 3);
    assert!(
        view.operation
            .artifacts
            .iter()
            .all(|artifact| artifact.phase == TagEditArtifactPhase::Retired
                && artifact.actual_bytes == 0)
    );
    let commit = view.operation.commit.as_ref().unwrap();
    assert_eq!(
        commit.receipt.audio_sha256,
        format!("{:x}", Sha256::digest(&payload))
    );
    let body = fs::read(&path).unwrap();
    assert!(
        body.ends_with(&payload),
        "Every actual MPEG payload byte must survive tag installation"
    );
    assert_eq!(
        commit.receipt.new_body_sha256,
        format!("{:x}", Sha256::digest(&body))
    );
    assert!(profile.join("tag-edits").is_dir());
    assert_eq!(fs::read_dir(profile.join("tag-edits")).unwrap().count(), 0);
    let tagged = Probe::open(&path).unwrap().read().unwrap();
    let tags = tagged.tag(TagType::Id3v2).unwrap();
    assert_eq!(
        tags.get_string(ItemKey::TrackTitle),
        Some("实际后端标签标题")
    );
    assert_eq!(tags.get_string(ItemKey::AlbumTitle), Some("后端校验专辑"));
    assert_eq!(
        tags.get_strings(ItemKey::TrackArtist).collect::<Vec<_>>(),
        vec!["作者甲", "Artist B"]
    );
    let id3: Id3v2Tag = tags.clone().into();
    assert!(id3.unsync_text().any(|text| text.language == *b"zho"
        && text.description.is_empty()
        && text.content == "[00:00.00]实际标签歌词\n[00:00.10]第二行"));
    let refreshed = read(
        &backend,
        "tag-fixture-read-after",
        &asset,
        "tag-fixture-observe",
    );
    assert_eq!(refreshed.title.as_deref(), Some("实际后端标签标题"));
    assert_eq!(
        refreshed.selection.expected_identity,
        selection.expected_identity
    );
    assert_eq!(
        refreshed.selection.expected_body_sha256.as_deref(),
        Some(commit.receipt.new_body_sha256.as_str())
    );
    let mut changed = patch();
    changed.title = TagEditField::Set("不同操作内容".into());
    let collision = terminal(
        &backend,
        "tag-fixture-different-patch",
        BackendCommand::EditLocalTags {
            approval_id: approval.approval_id,
            expected_approval_epoch: approval.approval_epoch,
            budget: budget(),
            patch: changed,
        },
    );
    assert!(
        !collision.0,
        "The same durable tag operation cannot accept a different patch"
    );
    assert_eq!(fs::read(&path).unwrap(), body);
    backend.shutdown(Duration::from_secs(10)).unwrap();
    let repository = Repository::open(&profile).unwrap();
    let quota = repository.tag_edit_quota().unwrap();
    assert_eq!(quota.pending_operations, 0);
    assert_eq!(quota.charged_bytes, 0);
    let stored = repository
        .tag_edit_operation(&selection.operation_id)
        .unwrap()
        .unwrap();
    assert!(stored.is_finished());
}

#[test]
fn playing_and_paused_local_track_reject_tag_mutation_before_reservation_then_edit_after_stop() {
    let temp = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
    let profile = temp.path().join("profile");
    let path = temp.path().join("playing-selected.mp3");
    let payload = encoded::silent_mpeg_layer3().repeat(128);
    fs::write(&path, &payload).unwrap();
    let backend = BackendHandle::start(BackendConfig::headless(profile.clone())).unwrap();
    success(
        &backend,
        "busy-import",
        BackendCommand::ImportAudio {
            paths: vec![path.clone()],
        },
    );
    let BackendQueryResult::LibraryAssets(page) = backend
        .query(BackendQuery::LibraryAssets {
            root_id: None,
            after_asset_id: None,
            limit: 10,
            expected_revision: None,
        })
        .unwrap()
    else {
        panic!("Library asset page missing");
    };
    let asset = &page.items[0].asset;
    let selection = read(&backend, "busy-read", &asset.id, "busy-durable").selection;
    success(
        &backend,
        "busy-approve",
        BackendCommand::ApproveTagWrite {
            approval_id: "busy-write-consent".into(),
            selection: selection.clone(),
        },
    );
    let BackendQueryResult::TagWriteApproval(Some(approval)) = backend
        .query(BackendQuery::TagWriteApproval {
            approval_id: "busy-write-consent".into(),
        })
        .unwrap()
    else {
        panic!("Write approval missing");
    };
    backend
        .execute(command(
            "busy-play",
            BackendCommand::Play {
                id: asset.track_id.clone(),
            },
        ))
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let playback = backend.snapshot().playback;
        if playback.status == PlaybackStatus::Playing
            && playback.has_presented_media
            && playback.position_seconds > 0.05
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "Real Memory playback failed to present: {playback:?}"
        );
        thread::sleep(Duration::from_millis(10));
    }
    let playing = backend.snapshot().playback;
    for (id, expected) in [
        ("busy-edit-playing", PlaybackStatus::Playing),
        ("busy-edit-paused", PlaybackStatus::Paused),
    ] {
        if expected == PlaybackStatus::Paused {
            backend
                .execute(command("busy-pause", BackendCommand::Pause))
                .unwrap();
            let deadline = Instant::now() + Duration::from_secs(5);
            while backend.snapshot().playback.status != PlaybackStatus::Paused {
                assert!(Instant::now() < deadline, "Real playback did not pause");
                thread::sleep(Duration::from_millis(10));
            }
        }
        let denied = terminal(
            &backend,
            id,
            BackendCommand::EditLocalTags {
                approval_id: approval.approval_id.clone(),
                expected_approval_epoch: approval.approval_epoch,
                budget: budget(),
                patch: patch(),
            },
        );
        assert!(!denied.0);
        assert_eq!(denied.1.unwrap().kind, ErrorKind::Busy);
        let after = backend.snapshot().playback;
        assert_eq!(after.status, expected);
        assert_eq!(after.generation, playing.generation);
        assert_eq!(after.track_id.as_deref(), Some(asset.track_id.as_str()));
        assert!(after.position_seconds >= playing.position_seconds);
        assert_eq!(fs::read(&path).unwrap(), payload);
        assert!(!profile.join("tag-edits").exists());
        let BackendQueryResult::TagEdit(view) = backend
            .query(BackendQuery::TagEdit {
                operation_id: selection.operation_id.clone(),
            })
            .unwrap()
        else {
            panic!("Tag edit result missing");
        };
        assert!(
            view.is_none(),
            "Busy playback must reject before durable reservation"
        );
    }
    backend
        .execute(command("busy-stop", BackendCommand::Stop))
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while backend.snapshot().playback.status != PlaybackStatus::Idle {
        assert!(Instant::now() < deadline, "Real playback did not stop");
        thread::sleep(Duration::from_millis(10));
    }
    success(
        &backend,
        "busy-edit-after-stop",
        BackendCommand::EditLocalTags {
            approval_id: approval.approval_id.clone(),
            expected_approval_epoch: approval.approval_epoch,
            budget: budget(),
            patch: patch(),
        },
    );
    let BackendQueryResult::TagEdit(Some(view)) = backend
        .query(BackendQuery::TagEdit {
            operation_id: selection.operation_id.clone(),
        })
        .unwrap()
    else {
        panic!("Completed tag edit missing");
    };
    assert!(view.finished && !view.has_live_owner);
    let installed = fs::read(&path).unwrap();
    assert!(installed.ends_with(&payload));
    backend
        .execute(command(
            "busy-play-installed",
            BackendCommand::Play {
                id: asset.track_id.clone(),
            },
        ))
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let playback = backend.snapshot().playback;
        if playback.status == PlaybackStatus::Playing && playback.has_presented_media {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "Installed track did not present: {playback:?}"
        );
        thread::sleep(Duration::from_millis(10));
    }
    let replay_generation = backend.snapshot().playback.generation;
    success(
        &backend,
        "busy-replay-completed",
        BackendCommand::EditLocalTags {
            approval_id: approval.approval_id,
            expected_approval_epoch: approval.approval_epoch,
            budget: budget(),
            patch: patch(),
        },
    );
    assert_eq!(fs::read(&path).unwrap(), installed);
    assert_eq!(backend.snapshot().playback.generation, replay_generation);
    assert_eq!(backend.snapshot().playback.status, PlaybackStatus::Playing);
    backend.shutdown(Duration::from_secs(10)).unwrap();
    let quota = Repository::open(profile).unwrap().tag_edit_quota().unwrap();
    assert_eq!(quota.pending_operations, 0);
    assert_eq!(quota.charged_bytes, 0);
}

#[test]
fn retired_busy_intent_replay_is_failed_and_a_new_intent_can_install_after_reader_close() {
    use std::fs::OpenOptions;
    use std::os::windows::fs::OpenOptionsExt;
    let temp = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
    let profile = temp.path().join("profile");
    let path = temp.path().join("reader-held.mp3");
    let payload = encoded::silent_mpeg_layer3();
    fs::write(&path, &payload).unwrap();
    let backend = BackendHandle::start(BackendConfig::headless(profile.clone())).unwrap();
    success(
        &backend,
        "retired-import",
        BackendCommand::ImportAudio {
            paths: vec![path.clone()],
        },
    );
    let BackendQueryResult::LibraryAssets(page) = backend
        .query(BackendQuery::LibraryAssets {
            root_id: None,
            after_asset_id: None,
            limit: 10,
            expected_revision: None,
        })
        .unwrap()
    else {
        panic!("Library assets missing");
    };
    let asset = &page.items[0].asset;
    let selection = read(&backend, "retired-read", &asset.id, "retired-busy-intent").selection;
    success(
        &backend,
        "retired-approve",
        BackendCommand::ApproveTagWrite {
            approval_id: "retired-busy-approval".into(),
            selection: selection.clone(),
        },
    );
    let BackendQueryResult::TagWriteApproval(Some(approval)) = backend
        .query(BackendQuery::TagWriteApproval {
            approval_id: "retired-busy-approval".into(),
        })
        .unwrap()
    else {
        panic!("Tag approval missing");
    };
    let held = OpenOptions::new()
        .read(true)
        .share_mode(1)
        .open(&path)
        .unwrap();
    let attempt = || BackendCommand::EditLocalTags {
        approval_id: approval.approval_id.clone(),
        expected_approval_epoch: approval.approval_epoch,
        budget: budget(),
        patch: patch(),
    };
    let failed = terminal(&backend, "retired-busy-attempt", attempt());
    assert!(!failed.0);
    assert_eq!(failed.1.unwrap().kind, ErrorKind::Busy);
    let BackendQueryResult::TagEdit(Some(view)) = backend
        .query(BackendQuery::TagEdit {
            operation_id: selection.operation_id.clone(),
        })
        .unwrap()
    else {
        panic!("Retired failed intent missing");
    };
    assert!(view.finished && view.operation.commit.is_none());
    assert!(!view.has_live_owner);
    assert_eq!(fs::read(&path).unwrap(), payload);
    let replay = terminal(&backend, "retired-busy-replay", attempt());
    assert!(
        !replay.0,
        "Physical retirement must not turn a failed edit into successful replay"
    );
    assert_eq!(replay.1.unwrap().kind, ErrorKind::Conflict);
    assert_eq!(fs::read(&path).unwrap(), payload);
    assert_eq!(held.metadata().unwrap().len(), payload.len() as u64);
    drop(held);
    let next = read(
        &backend,
        "retired-fresh-read",
        &asset.id,
        "retired-fresh-intent",
    )
    .selection;
    success(
        &backend,
        "retired-fresh-approve",
        BackendCommand::ApproveTagWrite {
            approval_id: "retired-fresh-approval".into(),
            selection: next,
        },
    );
    let BackendQueryResult::TagWriteApproval(Some(fresh)) = backend
        .query(BackendQuery::TagWriteApproval {
            approval_id: "retired-fresh-approval".into(),
        })
        .unwrap()
    else {
        panic!("Fresh tag approval missing");
    };
    success(
        &backend,
        "retired-fresh-edit",
        BackendCommand::EditLocalTags {
            approval_id: fresh.approval_id,
            expected_approval_epoch: fresh.approval_epoch,
            budget: budget(),
            patch: patch(),
        },
    );
    assert!(fs::read(&path).unwrap().ends_with(&payload));
    backend.shutdown(Duration::from_secs(10)).unwrap();
    let quota = Repository::open(profile).unwrap().tag_edit_quota().unwrap();
    assert_eq!(quota.pending_operations, 0);
    assert_eq!(quota.charged_bytes, 0);
}

use lx_gpui_player::{
    domain::music::{LxMusicDtoVersion, TrackRecord},
    download::{
        ApprovedDownloadUrl, CollisionPolicy, DownloadContext, DownloadError, DownloadErrorKind,
        DownloadRepresentationIdentity, DownloadState, FileIdentity, OwnedPartRecord,
        TransferCheckpoint, planning::DownloadQuality, task_model::*,
    },
};

fn identity(byte: &str) -> FileIdentity {
    FileIdentity {
        device: 1,
        object: byte.repeat(16),
    }
}
fn task() -> DownloadTask {
    let original_record = TrackRecord::from_lx_dto(serde_json::json!({"id":"kw_fixture", "source":"kw", "name":"歌", "singer":"人", "meta":{"songId":"song", "albumName":"专辑", "_qualitys":{"128k":{}}, "qualitys":[{"type":"128k"}]}}), LxMusicDtoVersion::Modern).unwrap();
    let settings = DownloadSettings::default();
    let stages = DownloadStageRecords::for_settings(&settings, DownloadQuality::K128);
    DownloadTask {
        id: "task".into(),
        profile_id: "profile".into(),
        original_record,
        resolved_record: None,
        requested_quality: DownloadQuality::K128,
        actual_quality: DownloadQuality::K128,
        list_id: Some("list".into()),
        list_name_snapshot: Some("歌单".into()),
        order: 0,
        revision: 1,
        operation_epoch: 1,
        settings,
        destination: DownloadDestinationBinding {
            grant_id: "destination".into(),
            grant_epoch: 1,
            root_identity: identity("aa"),
            relative_components: vec!["歌 - 人.mp3".into()],
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
        stages,
        commit_journal: None,
        created_at_ms: 1,
        updated_at_ms: 1,
    }
}
fn add_partial(task: &mut DownloadTask) {
    let context = DownloadContext {
        profile_id: task.profile_id.clone(),
        task_id: task.id.clone(),
        operation_epoch: task.operation_epoch,
        source_id: "selected-source".into(),
        source_content_sha256: "11".repeat(32),
        policy_epoch: 1,
    };
    let url = "http://127.0.0.1:1/song?token=private-secret";
    let approved = ApprovedDownloadUrl::from_exact_approval(url, url, &context, &context).unwrap();
    let origin = DownloadRepresentationIdentity::from_approved_url(
        &approved,
        "22".repeat(32),
        task.actual_quality,
    )
    .unwrap();
    task.url_bookmark = Some(PrivateUrlBookmark::new(url.into(), context).unwrap());
    task.original_representation = Some(origin.clone());
    task.owned_part = Some(OwnedPartRecord {
        basename: format!(".lx-download-{}.part", "ab".repeat(16)),
        nonce: "ab".repeat(16),
        identity: identity("33"),
        destination_identity: task.destination.root_identity.clone(),
        destination_epoch: task.destination.grant_epoch,
        final_basename: "歌 - 人.mp3".into(),
    });
    task.original_transfer = Some(TransferCheckpoint {
        bytes: 40,
        sha256: "44".repeat(32),
        strong_etag: Some("\"private-validator\"".into()),
        remote_total: Some(100),
        completed_body: false,
        representation: Some(origin),
    });
}
fn add_output(task: &mut DownloadTask) {
    task.output_audio = Some(DownloadArtifactProof {
        basename: "歌 - 人.mp3".into(),
        identity: identity("55"),
        bytes: 200,
        sha256: "66".repeat(32),
    });
    task.audio_available = true;
}

#[test]
fn exact_download_defaults_and_typed_unsupported_embedding_are_preserved() {
    let settings = DownloadSettings::default();
    settings.validate().unwrap();
    assert!(!settings.enabled);
    assert_eq!(settings.max_concurrent, 3);
    assert!(settings.destination_grant_id.is_none());
    assert!(settings.skip_existing);
    assert!(!settings.group_by_list);
    assert!(settings.embed_cover);
    assert!(!settings.embed_lyrics);
    assert!(!settings.download_lyrics);
    assert!(settings.embedded_lyrics.include_awlrc);
    assert!(settings.sidecar_lyrics.include_awlrc);
    let mut selected = settings.clone();
    selected.embed_lyrics = true;
    selected.download_lyrics = true;
    let stages = DownloadStageRecords::for_settings(&selected, DownloadQuality::Wav);
    assert_eq!(
        stages.metadata.state,
        DownloadStageState::SkippedUnsupportedBaseline
    );
    assert_eq!(
        stages.cover.state,
        DownloadStageState::SkippedUnsupportedBaseline
    );
    assert_eq!(
        stages.embedded_lyrics.state,
        DownloadStageState::SkippedUnsupportedBaseline
    );
    assert_eq!(stages.lyrics.state, DownloadStageState::Pending);
    assert!(!stages.all_satisfied());
    selected.max_concurrent = 5;
    assert!(selected.validate().is_err());
}

#[test]
fn restart_pauses_actual_live_phases_and_preserves_verified_partial_without_grant_restore() {
    for state in [
        DownloadState::Waiting,
        DownloadState::Resolving,
        DownloadState::Transferring,
        DownloadState::Verifying,
        DownloadState::PostProcessing,
        DownloadState::Cancelling,
    ] {
        let mut value = task();
        add_partial(&mut value);
        value.state = state;
        value.stages.metadata.state = DownloadStageState::Running;
        let checkpoint = value.original_transfer.clone();
        let part = value.owned_part.clone();
        value.after_restart(2).unwrap();
        assert_eq!(value.state, DownloadState::Paused);
        assert_eq!(value.pause_reason, Some(DownloadPauseReason::Restart));
        assert_eq!(value.operation_epoch, 2);
        assert_eq!(value.revision, 2);
        assert_eq!(value.original_transfer, checkpoint);
        assert_eq!(value.owned_part, part);
        assert_eq!(value.stages.metadata.state, DownloadStageState::Pending);
        let restored: DownloadTask =
            serde_json::from_str(&serde_json::to_string(&value).unwrap()).unwrap();
        restored.validate().unwrap();
        assert_eq!(restored.state, DownloadState::Paused);
    }
    let mut paused = task();
    paused.state = DownloadState::Paused;
    paused.pause_reason = Some(DownloadPauseReason::User);
    paused.after_restart(50).unwrap();
    assert_eq!(paused.pause_reason, Some(DownloadPauseReason::User));
    assert_eq!(paused.operation_epoch, 1);
}

#[test]
fn query_and_debug_do_not_expose_private_url_validator_or_untrusted_error_text() {
    let mut value = task();
    add_partial(&mut value);
    value.error = Some(DownloadError {
        kind: DownloadErrorKind::Http,
        message: "http://private.example/?token=secret".into(),
        http_status: Some(403),
    });
    let private = serde_json::to_string(&value).unwrap();
    assert!(private.contains("private-secret"));
    let safe = serde_json::to_string(&value.query_view()).unwrap();
    assert!(!safe.contains("private-secret"));
    assert!(!safe.contains("private-validator"));
    assert!(!safe.contains("private.example"));
    assert!(!format!("{value:?}").contains("private-secret"));
    assert!(!format!("{:?}", value.url_bookmark.as_ref().unwrap()).contains("private-secret"));
    assert_eq!(value.query_view().progress_percent, Some(40.0));
    assert_eq!(value.query_view().bytes, 40);
}

#[test]
fn postprocess_failure_keeps_good_audio_and_completed_history_can_be_unavailable() {
    let mut value = task();
    add_output(&mut value);
    value.state = DownloadState::Failed;
    value.stages.metadata.state = DownloadStageState::Failed;
    value.stages.metadata.error = Some(DownloadError {
        kind: DownloadErrorKind::Verification,
        message: "Tag readback failed".into(),
        http_status: None,
    });
    value.validate().unwrap();
    assert!(value.query_view().audio_available);
    assert_eq!(value.query_view().compatibility_status, "error");
    value.state = DownloadState::Completed;
    assert_eq!(
        value.validate().unwrap_err(),
        DownloadDocumentError::InvalidState
    );
    value.stages.metadata.state = DownloadStageState::Succeeded;
    value.stages.metadata.error = None;
    value.stages.cover.state = DownloadStageState::Succeeded;
    value.validate().unwrap();
    value.audio_available = false;
    value.validate().unwrap();
    assert!(!value.query_view().audio_available);
    assert!(value.query_view().is_complate);
}

#[test]
fn imported_metadata_cannot_restore_own_file_url_or_completed_audio_authority() {
    let mut value = task();
    add_partial(&mut value);
    add_output(&mut value);
    value.discard_imported_authority();
    value.validate().unwrap();
    assert_eq!(value.state, DownloadState::Paused);
    assert_eq!(
        value.pause_reason,
        Some(DownloadPauseReason::DestinationReapproval)
    );
    assert!(value.owned_part.is_none());
    assert!(value.original_transfer.is_none());
    assert!(value.original_representation.is_none());
    assert!(value.url_bookmark.is_none());
    assert!(value.output_audio.is_none());
    assert!(!value.audio_available);
    let grant = StoredDestinationGrant {
        id: "destination".into(),
        profile_id: "profile".into(),
        path: std::env::temp_dir(),
        identity: identity("aa"),
        epoch: 2,
        revision: 1,
        revoked: false,
    };
    assert_eq!(
        value.require_current_destination(&grant).unwrap_err(),
        DownloadDocumentError::InvalidBinding
    );
}

#[test]
fn provenance_and_part_epoch_mismatches_are_rejected_as_data_without_executing_them() {
    let mut value = task();
    add_partial(&mut value);
    value.validate().unwrap();
    value
        .original_transfer
        .as_mut()
        .unwrap()
        .representation
        .as_mut()
        .unwrap()
        .policy_epoch += 1;
    assert_eq!(
        value.validate().unwrap_err(),
        DownloadDocumentError::InvalidBinding
    );
    value.original_transfer.as_mut().unwrap().representation =
        value.original_representation.clone();
    value.owned_part.as_mut().unwrap().destination_epoch += 1;
    assert_eq!(
        value.validate().unwrap_err(),
        DownloadDocumentError::InvalidBinding
    );
    let mut exhausted = task();
    exhausted.operation_epoch = u64::MAX;
    assert_eq!(
        exhausted.after_restart(5).unwrap_err(),
        DownloadDocumentError::EpochExhausted
    );
    assert_eq!(exhausted.state, DownloadState::Waiting);
}

#[test]
fn profile_capacity_and_quality_identity_dedup_cover_completed_failed_and_cancelled_rows() {
    let mut rows = Vec::new();
    for index in 0..100 {
        let mut value = task();
        value.id = format!("task-{index}");
        value.original_record.lx_id = format!("music-{index}");
        value.order = index;
        value.state = DownloadState::Cancelled;
        rows.push(value);
    }
    validate_profile_downloads(&rows).unwrap();
    rows.push(task());
    assert_eq!(
        validate_profile_downloads(&rows).unwrap_err(),
        DownloadDocumentError::ProfileBound
    );
    let first = task();
    let mut second = first.clone();
    second.id = "other-task-id".into();
    second.list_id = Some("other-list".into());
    second.destination.grant_id = "other-destination".into();
    assert_eq!(
        validate_profile_downloads(&[first, second]).unwrap_err(),
        DownloadDocumentError::DuplicateTask
    );
}

#[test]
fn task_and_aggregate_document_bounds_are_enforced_without_file_or_network_activity() {
    let mut huge = task();
    huge.original_record.metadata.insert(
        "large".into(),
        serde_json::json!("x".repeat(MAX_DOWNLOAD_TASK_DOCUMENT_BYTES)),
    );
    assert_eq!(
        huge.validate().unwrap_err(),
        DownloadDocumentError::DocumentBound
    );
    let mut rows = Vec::new();
    for index in 0..100 {
        let mut value = task();
        value.id = format!("row-{index}");
        value.original_record.lx_id = format!("song-{index}");
        value.order = index;
        value
            .original_record
            .metadata
            .insert("large".into(), serde_json::json!("x".repeat(180 * 1024)));
        rows.push(value);
    }
    assert_eq!(
        validate_profile_downloads(&rows).unwrap_err(),
        DownloadDocumentError::DocumentBound
    );
}

fn journal(value: &DownloadTask) -> DownloadCommitJournal {
    DownloadCommitJournal {
        task_revision: value.revision,
        operation_epoch: value.operation_epoch,
        grant_epoch: value.destination.grant_epoch,
        phase: DownloadCommitPhase::Prepared,
        original_part: value.owned_part.clone().unwrap(),
        candidate: DownloadArtifactProof {
            basename: value.owned_part.as_ref().unwrap().basename.clone(),
            identity: identity("33"),
            bytes: 40,
            sha256: "44".repeat(32),
        },
        observed_target: None,
        rollback_original: None,
        sidecar: None,
        retry: None,
    }
}

#[test]
fn output_and_journal_files_are_bound_to_their_original_plan_and_own_nonce() {
    let mut value = task();
    add_partial(&mut value);
    add_output(&mut value);
    value.output_audio.as_mut().unwrap().basename = "other.mp3".into();
    assert_eq!(
        value.validate().unwrap_err(),
        DownloadDocumentError::InvalidBinding
    );
    value.output_audio.as_mut().unwrap().basename = "歌 - 人.mp3".into();
    let valid = journal(&value);
    value.commit_journal = Some(valid.clone());
    value.validate().unwrap();
    for changed in [0, 1, 2, 3] {
        let mut invalid = valid.clone();
        match changed {
            0 => invalid.original_part.destination_identity = identity("ff"),
            1 => invalid.original_part.destination_epoch += 1,
            2 => invalid.original_part.final_basename = "other.mp3".into(),
            _ => invalid.candidate.basename = "user-file.mp3".into(),
        }
        value.commit_journal = Some(invalid);
        assert_eq!(
            value.validate().unwrap_err(),
            DownloadDocumentError::InvalidBinding
        );
    }
    let mut published = valid;
    published.phase = DownloadCommitPhase::AudioPublished;
    published.candidate.basename = "歌 - 人.mp3".into();
    value.commit_journal = Some(published);
    value.validate().unwrap();
}

#[test]
fn overwrite_observations_rollback_and_sidecars_cannot_target_other_user_files() {
    let mut value = task();
    add_partial(&mut value);
    let mut original = journal(&value);
    value.destination.collision = CollisionPolicy::Overwrite {
        observed: identity("77"),
    };
    original.observed_target = Some(DownloadArtifactProof {
        basename: "歌 - 人.mp3".into(),
        identity: identity("77"),
        bytes: 10,
        sha256: "88".repeat(32),
    });
    original.rollback_original = Some(DownloadArtifactProof {
        basename: format!(".lx-download-{}.rollback", "99".repeat(16)),
        identity: identity("99"),
        bytes: 10,
        sha256: "88".repeat(32),
    });
    value.commit_journal = Some(original.clone());
    value.validate().unwrap();
    let mut wrong = original.clone();
    wrong.observed_target.as_mut().unwrap().identity = identity("ff");
    value.commit_journal = Some(wrong);
    assert_eq!(
        value.validate().unwrap_err(),
        DownloadDocumentError::InvalidBinding
    );
    let mut wrong = original.clone();
    wrong.rollback_original.as_mut().unwrap().basename = "unrelated-user-file.mp3".into();
    value.commit_journal = Some(wrong);
    assert_eq!(
        value.validate().unwrap_err(),
        DownloadDocumentError::InvalidBinding
    );
    let mut wrong = original;
    wrong.sidecar = Some(DownloadArtifactProof {
        basename: "other-song.lrc".into(),
        identity: identity("11"),
        bytes: 10,
        sha256: "aa".repeat(32),
    });
    value.commit_journal = Some(wrong);
    assert_eq!(
        value.validate().unwrap_err(),
        DownloadDocumentError::InvalidBinding
    );
}

#[test]
fn completed_tasks_cannot_hide_enabled_stages_by_marking_them_disabled() {
    let mut value = task();
    add_output(&mut value);
    value.state = DownloadState::Completed;
    value.stages.metadata.state = DownloadStageState::Succeeded;
    value.stages.cover.state = DownloadStageState::Disabled;
    assert_eq!(
        value.validate().unwrap_err(),
        DownloadDocumentError::InvalidState
    );
    value.stages.cover.state = DownloadStageState::Succeeded;
    value.settings.embed_lyrics = true;
    assert_eq!(
        value.validate().unwrap_err(),
        DownloadDocumentError::InvalidState
    );
    value.stages.embedded_lyrics.state = DownloadStageState::Succeeded;
    value.settings.download_lyrics = true;
    assert_eq!(
        value.validate().unwrap_err(),
        DownloadDocumentError::InvalidState
    );
    value.stages.lyrics.state = DownloadStageState::Succeeded;
    value.validate().unwrap();
    value.stages.metadata.state = DownloadStageState::SkippedUnsupportedBaseline;
    assert_eq!(
        value.validate().unwrap_err(),
        DownloadDocumentError::InvalidState
    );
}

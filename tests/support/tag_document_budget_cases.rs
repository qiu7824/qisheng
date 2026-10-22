use super::*;
use std::{path::Path, sync::Barrier};

fn prepare(repository: &Arc<Repository>, media: &Path, index: usize) -> TagWriteSelection {
    let path = media.join(format!("document-pressure-{index}.flac"));
    let bytes = encoded::mono_verbatim_flac();
    fs::write(&path, &bytes).unwrap();
    let context = ScanContext::new(Duration::from_secs(30));
    let root = RootAuthority::open(media, false, &context).unwrap();
    let opened = root.open_file(&path).unwrap();
    let identity = opened.identity().clone();
    let barrier = root.retirement_barrier();
    drop(opened);
    drop(root);
    assert!(barrier.wait(Duration::from_secs(3)));
    let (track, asset) = local_track_from_path(&path).unwrap();
    repository
        .upsert_explicit_library_tracks(
            &[LibraryPreparedAsset {
                track: track.clone(),
                asset: asset.clone(),
                file_identity: Some(identity.clone()),
            }],
            &|| Ok(()),
        )
        .unwrap();
    let binding = repository
        .library_asset_binding(&asset.id)
        .unwrap()
        .unwrap();
    let grant = repository
        .local_asset_authority(&asset.id)
        .unwrap()
        .unwrap()
        .explicit_file_epoch
        .unwrap();
    TagWriteSelection {
        operation_id: format!("document-operation-{index}"),
        asset_id: asset.id,
        path,
        expected_identity: identity,
        root_id: None,
        grant_epoch: grant,
        expected_binding_revision: binding.binding_revision,
        expected_body_sha256: Some(format!("{:x}", Sha256::digest(bytes))),
    }
}
fn budget() -> TagEditBudget {
    TagEditBudget {
        original_bytes: 2 * 1024 * 1024,
        candidate_bytes: 2 * 1024 * 1024,
        backup_bytes: 2 * 1024 * 1024,
        journal_bytes: 8 * 1024 * 1024,
    }
}
fn patch() -> LocalTagPatch {
    LocalTagPatch {
        title: TagField::Set("Actual document budget title".into()),
        lyrics: TagField::Set("x".repeat(900 * 1024)),
        ..Default::default()
    }
}
fn install_and_retire(repository: &Arc<Repository>, token: TagEditToken) {
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
        token.selection,
        &directory,
        &patch(),
        &ScanContext::new(Duration::from_secs(30)),
        &mut coordinator,
    )
    .unwrap();
    owner
        .retire(&ScanContext::new(Duration::from_secs(30)), &mut coordinator)
        .unwrap();
    assert!(owner.is_retired() && !owner.has_live_handles());
}

#[test]
fn actual_completed_metadata_and_concurrent_admissions_reserve_all_future_document_growth_before_io()
 {
    let profile = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
    let media = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
    let repository = Arc::new(Repository::open(profile.path()).unwrap());
    for index in 0..15 {
        let selection = prepare(&repository, media.path(), index);
        let id = format!("document-approval-{index}");
        let approval = repository
            .approve_tag_write(&id, &selection, &|| Ok(()))
            .unwrap();
        let operation = repository
            .begin_tag_edit(
                &id,
                approval.approval_epoch,
                budget(),
                &intent_digest(),
                &|| Ok(()),
            )
            .unwrap();
        assert!(operation.document_budget_reserved);
        install_and_retire(&repository, operation.token);
        let finished = repository
            .tag_edit_operation(&selection.operation_id)
            .unwrap()
            .unwrap();
        assert!(finished.is_finished() && finished.commit.is_some());
    }
    let mut attempts = Vec::new();
    for index in 15..19 {
        let selection = prepare(&repository, media.path(), index);
        let id = format!("document-approval-{index}");
        let approval = repository
            .approve_tag_write(&id, &selection, &|| Ok(()))
            .unwrap();
        attempts.push((id, approval.approval_epoch, selection));
    }
    // A genuine admission from an older ledger has no future document booking.
    // Removing only the two versioned booking markers models that old record;
    // the actual original, identity, approval, artifacts and receipts stay intact.
    let legacy_selection = prepare(&repository, media.path(), 19);
    let legacy_approval = repository
        .approve_tag_write("legacy-pressure-approval", &legacy_selection, &|| Ok(()))
        .unwrap();
    let legacy = repository
        .begin_tag_edit(
            &legacy_approval.approval_id,
            legacy_approval.approval_epoch,
            budget(),
            &intent_digest(),
            &|| Ok(()),
        )
        .unwrap();
    let db = rusqlite::Connection::open(repository.path()).unwrap();
    db.execute("UPDATE tag_edit_operations SET document=json_remove(document,'$.document_budget_reserved','$.recovery_budget_reserved'),byte_size=length(CAST(json_remove(document,'$.document_budget_reserved','$.recovery_budget_reserved') AS BLOB)) WHERE operation_id=?1", [&legacy.token.operation_id]).unwrap();
    drop(db);
    let base = repository.tag_document_budget().unwrap();
    assert!(base.actual_bytes > 12 * 1024 * 1024);
    assert_eq!(base.reserved_bytes, 0);
    let mut before_files = fs::read_dir(profile.path().join("tag-edits"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect::<Vec<_>>();
    before_files.sort();
    let barrier = Arc::new(Barrier::new(5));
    let workers = attempts
        .iter()
        .map(|(id, epoch, selection)| {
            let repository = repository.clone();
            let barrier = barrier.clone();
            let id = id.clone();
            let epoch = *epoch;
            let selection = selection.clone();
            std::thread::spawn(move || {
                barrier.wait();
                (
                    selection,
                    repository.begin_tag_edit(&id, epoch, budget(), &intent_digest(), &|| Ok(())),
                )
            })
        })
        .collect::<Vec<_>>();
    barrier.wait();
    let outcomes = workers
        .into_iter()
        .map(|worker| worker.join().unwrap())
        .collect::<Vec<_>>();
    let mut after_files = fs::read_dir(profile.path().join("tag-edits"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect::<Vec<_>>();
    after_files.sort();
    assert_eq!(
        after_files, before_files,
        "SQL document admission must create no actual artifact"
    );
    let admitted = outcomes.iter().filter(|(_, result)| result.is_ok()).count();
    assert!(
        (1..4).contains(&admitted),
        "Actual future document reservations must reject before all four physical admissions"
    );
    let peak = repository.tag_document_budget().unwrap();
    assert_eq!(peak.charged_bytes, peak.actual_bytes + peak.reserved_bytes);
    assert!(peak.charged_bytes <= peak.maximum_bytes && peak.reserved_bytes > 0);
    let legacy_before = repository
        .tag_edit_operation(&legacy.token.operation_id)
        .unwrap()
        .unwrap();
    let revision_before = repository.revision().unwrap();
    let db = rusqlite::Connection::open(repository.path()).unwrap();
    let snapshot = |db: &rusqlite::Connection| -> Vec<(String, String)> {
        let mut statement = db.prepare("SELECT 'approval:'||approval_id,document FROM tag_write_approvals UNION ALL SELECT 'operation:'||operation_id,document FROM tag_edit_operations UNION ALL SELECT 'artifact:'||operation_id||':'||role,document FROM tag_edit_artifacts ORDER BY 1").unwrap();
        statement
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .map(|row| row.unwrap())
            .collect()
    };
    let documents_before = snapshot(&db);
    assert!(
        repository
            .reserve_tag_documents(&legacy.token, &|| Ok(()))
            .is_err(),
        "A legacy booking must reject before opening any native file when other actual admissions own the remaining document allowance"
    );
    assert_eq!(snapshot(&db), documents_before);
    assert_eq!(repository.revision().unwrap(), revision_before);
    assert_eq!(
        serde_json::to_value(
            repository
                .tag_edit_operation(&legacy.token.operation_id)
                .unwrap()
                .unwrap()
        )
        .unwrap(),
        serde_json::to_value(legacy_before).unwrap()
    );
    assert_eq!(repository.tag_document_budget().unwrap(), peak);
    assert_eq!(
        repository
            .local_asset_authority(&legacy_selection.asset_id)
            .unwrap()
            .unwrap()
            .explicit_file_epoch,
        Some(legacy_selection.grant_epoch)
    );
    assert_eq!(
        fs::read(&legacy_selection.path).unwrap(),
        encoded::mono_verbatim_flac()
    );
    drop(db);
    for (selection, result) in &outcomes {
        assert_eq!(
            repository
                .local_asset_authority(&selection.asset_id)
                .unwrap()
                .unwrap()
                .explicit_file_epoch,
            Some(selection.grant_epoch)
        );
        if result.is_err() {
            assert!(
                repository
                    .tag_edit_operation(&selection.operation_id)
                    .unwrap()
                    .is_none()
            );
        }
    }
    // Complete a genuine large-metadata commit while the other admitted slots
    // retain their full future reservations. The pre-existing budget stays safe.
    let completed = outcomes
        .iter()
        .find_map(|(_, result)| result.as_ref().ok())
        .unwrap();
    install_and_retire(&repository, completed.token.clone());
    let after_commit = repository.tag_document_budget().unwrap();
    assert!(after_commit.charged_bytes <= after_commit.maximum_bytes);
    for (_, result) in outcomes {
        if let Ok(operation) = result {
            if repository
                .tag_edit_operation(&operation.token.operation_id)
                .unwrap()
                .unwrap()
                .is_finished()
            {
                continue;
            }
            let mut coordinator = RepositoryTagEditCoordinator::new(
                repository.clone(),
                operation.token.clone(),
                Arc::new(|| Ok(())),
                Arc::new(|| Ok(())),
            )
            .unwrap();
            let mut owner = TagOwnedInstallation::unstarted(operation.token.selection);
            owner
                .retire(&ScanContext::new(Duration::from_secs(30)), &mut coordinator)
                .unwrap();
        }
    }
    let mut coordinator = RepositoryTagEditCoordinator::new(
        repository.clone(),
        legacy.token.clone(),
        Arc::new(|| Ok(())),
        Arc::new(|| Ok(())),
    )
    .unwrap();
    let mut owner = TagOwnedInstallation::unstarted(legacy.token.selection);
    owner
        .retire(&ScanContext::new(Duration::from_secs(30)), &mut coordinator)
        .unwrap();
    let finished = repository.tag_document_budget().unwrap();
    assert_eq!(finished.reserved_bytes, 0);
    assert_eq!(finished.charged_bytes, finished.actual_bytes);
    assert_eq!(repository.tag_edit_quota().unwrap().charged_bytes, 0);
    assert_eq!(repository.tag_edit_quota().unwrap().pending_operations, 0);
}

#[test]
fn legacy_missing_document_marker_is_adopted_transactionally_before_another_native_open() {
    let fixture = Fixture::new("legacy-document-booking");
    let token = fixture.begin();
    let db = rusqlite::Connection::open(fixture.repository.path()).unwrap();
    db.execute("UPDATE tag_edit_operations SET document=json_remove(document,'$.document_budget_reserved'),byte_size=length(CAST(json_remove(document,'$.document_budget_reserved') AS BLOB)) WHERE operation_id=?1", [&token.operation_id]).unwrap();
    assert!(
        !fixture
            .repository
            .tag_edit_operation(&token.operation_id)
            .unwrap()
            .unwrap()
            .document_budget_reserved
    );
    assert_eq!(
        fixture
            .repository
            .tag_document_budget()
            .unwrap()
            .reserved_bytes,
        0
    );
    let mut coordinator = fixture.coordinator(token.clone());
    assert!(
        fixture
            .repository
            .tag_edit_operation(&token.operation_id)
            .unwrap()
            .unwrap()
            .document_budget_reserved
    );
    assert!(
        fixture
            .repository
            .tag_document_budget()
            .unwrap()
            .reserved_bytes
            > 0
    );
    let mut owner = TagOwnedInstallation::unstarted(token.selection);
    owner
        .retire(&ScanContext::new(Duration::from_secs(30)), &mut coordinator)
        .unwrap();
    assert_eq!(
        fixture
            .repository
            .tag_document_budget()
            .unwrap()
            .reserved_bytes,
        0
    );
    assert_eq!(
        fixture.repository.tag_edit_quota().unwrap().charged_bytes,
        0
    );
    assert_eq!(fs::read(&fixture.selection.path).unwrap(), fixture.old_body);
}

#[test]
fn finished_validation_cache_rechecks_same_revision_raw_sql_document_and_sealed_columns() {
    let fixture = Fixture::new("finished-cache-validation");
    let token = fixture.begin();
    let mut coordinator = fixture.coordinator(token.clone());
    let directory = coordinator.journal_directory().unwrap();
    fs::create_dir_all(&directory).unwrap();
    let mut owner = install_selected_tags_owned(
        token.selection.clone(),
        &directory,
        &LocalTagPatch {
            title: TagField::Set("Finished cache title".into()),
            ..Default::default()
        },
        &ScanContext::new(Duration::from_secs(30)),
        &mut coordinator,
    )
    .unwrap();
    owner
        .retire(&ScanContext::new(Duration::from_secs(30)), &mut coordinator)
        .unwrap();
    assert!(owner.is_retired() && !owner.has_live_handles());
    let original = fixture
        .repository
        .tag_edit_operation(&token.operation_id)
        .unwrap()
        .unwrap();
    assert!(original.is_finished());
    let quota = fixture.repository.tag_edit_quota().unwrap();
    let documents = fixture.repository.tag_document_budget().unwrap();
    assert_eq!(quota.charged_bytes, 0);
    assert!(fixture.repository.pending_tag_edits().unwrap().is_empty());
    let file_before = fs::read(&fixture.selection.path).unwrap();
    let db = rusqlite::Connection::open(fixture.repository.path()).unwrap();
    let operation_document: String = db
        .query_row(
            "SELECT document FROM tag_edit_operations WHERE operation_id=?1",
            [&token.operation_id],
            |row| row.get(0),
        )
        .unwrap();
    let artifact_document: String = db
        .query_row(
            "SELECT document FROM tag_edit_artifacts WHERE operation_id=?1 AND role='backup'",
            [&token.operation_id],
            |row| row.get(0),
        )
        .unwrap();
    let assert_rejected = || {
        assert!(fixture.repository.tag_edit_quota().is_err());
        assert!(fixture.repository.tag_document_budget().is_err());
        assert!(fixture.repository.pending_tag_edits().is_err());
    };
    // Same revision and byte length, but a changed installed fact no longer
    // agrees with the actual pre-close receipt digest.
    let mut changed: serde_json::Value = serde_json::from_str(&operation_document).unwrap();
    changed["commit"]["receipt"]["installed"]["metadata"]["title"] = "Xinished cache title".into();
    let changed = serde_json::to_string(&changed).unwrap();
    assert_eq!(changed.len(), operation_document.len());
    db.execute(
        "UPDATE tag_edit_operations SET document=?2 WHERE operation_id=?1",
        rusqlite::params![token.operation_id, changed],
    )
    .unwrap();
    assert_rejected();
    db.execute(
        "UPDATE tag_edit_operations SET document=?2 WHERE operation_id=?1",
        rusqlite::params![token.operation_id, operation_document],
    )
    .unwrap();
    assert_eq!(fixture.repository.tag_edit_quota().unwrap(), quota);

    // The artifact document is equally part of the terminal evidence, even
    // when the operation row and every scalar artifact column stay unchanged.
    let mut changed: serde_json::Value = serde_json::from_str(&artifact_document).unwrap();
    let epoch = original.token.operation_epoch;
    changed["operation_epoch"] = serde_json::json!(if epoch == 1 { 2 } else { epoch - 1 });
    let changed = serde_json::to_string(&changed).unwrap();
    assert_eq!(changed.len(), artifact_document.len());
    db.execute(
        "UPDATE tag_edit_artifacts SET document=?2 WHERE operation_id=?1 AND role='backup'",
        rusqlite::params![token.operation_id, changed],
    )
    .unwrap();
    assert_rejected();
    db.execute(
        "UPDATE tag_edit_artifacts SET document=?2 WHERE operation_id=?1 AND role='backup'",
        rusqlite::params![token.operation_id, artifact_document],
    )
    .unwrap();
    assert_eq!(fixture.repository.tag_document_budget().unwrap(), documents);

    db.execute(
        "UPDATE tag_edit_operations SET phase='restored' WHERE operation_id=?1",
        [&token.operation_id],
    )
    .unwrap();
    assert_rejected();
    db.execute(
        "UPDATE tag_edit_operations SET phase='published' WHERE operation_id=?1",
        [&token.operation_id],
    )
    .unwrap();
    db.execute(
        "UPDATE tag_edit_artifacts SET phase='sealed' WHERE operation_id=?1 AND role='backup'",
        [&token.operation_id],
    )
    .unwrap();
    assert_rejected();
    db.execute(
        "UPDATE tag_edit_artifacts SET phase='retired' WHERE operation_id=?1 AND role='backup'",
        [&token.operation_id],
    )
    .unwrap();
    assert_eq!(fixture.repository.tag_edit_quota().unwrap(), quota);
    assert_eq!(fixture.repository.tag_document_budget().unwrap(), documents);
    assert!(fixture.repository.pending_tag_edits().unwrap().is_empty());
    let current = fixture
        .repository
        .tag_edit_operation(&token.operation_id)
        .unwrap()
        .unwrap();
    assert_eq!(current.ledger_revision, original.ledger_revision);
    assert_eq!(
        serde_json::to_value(current).unwrap(),
        serde_json::to_value(original).unwrap()
    );
    assert_eq!(fs::read(&fixture.selection.path).unwrap(), file_before);
    assert_eq!(
        fixture
            .repository
            .local_asset_authority(&fixture.selection.asset_id)
            .unwrap()
            .unwrap()
            .explicit_file_epoch,
        Some(fixture.selection.grant_epoch)
    );
}

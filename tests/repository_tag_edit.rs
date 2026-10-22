#![cfg(windows)]

#[path = "support/tag_cold_recovery_cases.rs"]
mod cold_recovery_cases;
#[path = "support/tag_document_budget_cases.rs"]
mod document_budget_cases;
#[allow(
    dead_code,
    reason = "The shared encoded fixture exposes sibling format constructors"
)]
#[path = "support/encoded_audio_fixture.rs"]
mod encoded;
#[path = "support/tag_metadata_budget_cases.rs"]
mod metadata_budget_cases;
#[path = "support/tag_retirement_cases.rs"]
mod retirement_cases;
use anyhow::Result;
use lx_gpui_player::{
    domain::{
        library::LibraryPreparedAsset,
        lists::{DEFAULT_LIST, ListAction},
    },
    library::{
        authority::RootAuthority,
        scan::ScanContext,
        tag_install::*,
        tag_write::{LocalTagPatch, TagField},
    },
    repository::{
        ChangeKind, DATABASE_NAME, Repository, RepositoryTagEditCoordinator, SCHEMA_VERSION,
        TagEditBudget, TagEditPhase, TagEditToken, local_track_from_path,
    },
};
use sha2::{Digest, Sha256};
use std::{fs, path::PathBuf, sync::Arc, time::Duration};

fn intent_digest() -> String {
    format!("{:x}",Sha256::digest(serde_json::to_vec(&serde_json::json!({"title":"Actual same-handle title","artists":["Actual artist"],"album":"Actual album"})).unwrap()))
}

struct Fixture {
    repository: Arc<Repository>,
    profile: tempfile::TempDir,
    _media: tempfile::TempDir,
    selection: TagWriteSelection,
    old_body: Vec<u8>,
}
impl Fixture {
    fn new(operation: &str) -> Self {
        let profile = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
        let media = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
        let path = media.path().join("selected.flac");
        let old_body = encoded::mono_verbatim_flac();
        fs::write(&path, &old_body).unwrap();
        let context = ScanContext::new(Duration::from_secs(30));
        let root = RootAuthority::open(media.path(), false, &context).unwrap();
        let opened = root.open_file(&path).unwrap();
        let identity = opened.identity().clone();
        let barrier = root.retirement_barrier();
        drop(opened);
        drop(root);
        assert!(barrier.wait(Duration::from_secs(3)));
        let repository = Arc::new(Repository::open(profile.path()).unwrap());
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
        repository
            .apply_list_action(
                &ListAction::MusicAdd {
                    list_id: DEFAULT_LIST.into(),
                    tracks: vec![track],
                    location: Default::default(),
                },
                None,
            )
            .unwrap();
        let binding = repository
            .library_asset_binding(&asset.id)
            .unwrap()
            .unwrap();
        let selection = TagWriteSelection {
            operation_id: operation.into(),
            asset_id: asset.id,
            path,
            expected_identity: identity,
            root_id: None,
            grant_epoch: repository
                .local_asset_authority(&binding.asset_id)
                .unwrap()
                .unwrap()
                .explicit_file_epoch
                .unwrap(),
            expected_binding_revision: binding.binding_revision,
            expected_body_sha256: Some(format!("{:x}", Sha256::digest(&old_body))),
        };
        Self {
            repository,
            profile,
            _media: media,
            selection,
            old_body,
        }
    }
    fn budget(&self) -> TagEditBudget {
        TagEditBudget {
            original_bytes: self.old_body.len() as u64 + 16 * 1024 * 1024,
            journal_bytes: 1024 * 1024,
            backup_bytes: self.old_body.len() as u64,
            candidate_bytes: self.old_body.len() as u64 + 16 * 1024 * 1024,
        }
    }
    fn begin(&self) -> TagEditToken {
        let approval = self
            .repository
            .approve_tag_write("explicit-tag-consent", &self.selection, &|| Ok(()))
            .unwrap();
        self.repository
            .begin_tag_edit(
                &approval.approval_id,
                approval.approval_epoch,
                self.budget(),
                &intent_digest(),
                &|| Ok(()),
            )
            .unwrap()
            .token
    }
    fn coordinator(&self, token: TagEditToken) -> RepositoryTagEditCoordinator {
        RepositoryTagEditCoordinator::new(
            self.repository.clone(),
            token,
            Arc::new(|| Ok(())),
            Arc::new(|| Ok(())),
        )
        .unwrap()
    }
    fn install(
        &self,
        coordinator: &mut dyn TagInstallCoordinator,
    ) -> Result<TagInstallationReceipt> {
        let context = ScanContext::new(Duration::from_secs(30));
        let directory = coordinator.journal_directory()?;
        fs::create_dir_all(&directory)?;
        let writer = NativeWriterToken::from_explicit_selection(
            self.selection.clone(),
            &context,
            coordinator,
        )?;
        install_tags(
            writer,
            &directory,
            &LocalTagPatch {
                title: TagField::Set("Actual same-handle title".into()),
                artists: TagField::Set(vec!["Actual artist".into()]),
                album: TagField::Set("Actual album".into()),
                ..Default::default()
            },
            &context,
            coordinator,
        )
    }
}

#[test]
fn explicit_write_authority_is_independent_revocable_and_fenced_by_binding_cas() {
    let fixture = Fixture::new("tag-authorization");
    assert!(
        fixture
            .repository
            .tag_write_approval("explicit-tag-consent")
            .unwrap()
            .is_none()
    );
    assert!(
        fixture
            .repository
            .begin_tag_edit(
                "explicit-tag-consent",
                1,
                fixture.budget(),
                &intent_digest(),
                &|| Ok(())
            )
            .is_err()
    );
    assert_eq!(fs::read(&fixture.selection.path).unwrap(), fixture.old_body);
    let approval = fixture
        .repository
        .approve_tag_write("explicit-tag-consent", &fixture.selection, &|| Ok(()))
        .unwrap();
    let revision = fixture.repository.revision().unwrap();
    let repeated = fixture
        .repository
        .approve_tag_write("explicit-tag-consent", &fixture.selection, &|| Ok(()))
        .unwrap();
    assert_eq!(repeated.approval_epoch, approval.approval_epoch);
    assert_eq!(fixture.repository.revision().unwrap(), revision);
    fixture
        .repository
        .revoke_tag_write(&approval.approval_id, approval.approval_epoch, &|| Ok(()))
        .unwrap();
    assert!(
        fixture
            .repository
            .begin_tag_edit(
                &approval.approval_id,
                approval.approval_epoch,
                fixture.budget(),
                &intent_digest(),
                &|| Ok(())
            )
            .is_err()
    );
    assert!(
        fixture
            .repository
            .local_asset_authority(&fixture.selection.asset_id)
            .unwrap()
            .unwrap()
            .explicit_file_epoch
            .is_some()
    );
    let approval = fixture
        .repository
        .approve_tag_write("new-explicit-consent", &fixture.selection, &|| Ok(()))
        .unwrap();
    let db = rusqlite::Connection::open(fixture.repository.path()).unwrap();
    db.execute(
        "UPDATE local_asset_locations SET binding_revision=binding_revision+1 WHERE asset_id=?1",
        [&fixture.selection.asset_id],
    )
    .unwrap();
    assert!(
        fixture
            .repository
            .begin_tag_edit(
                &approval.approval_id,
                approval.approval_epoch,
                fixture.budget(),
                &intent_digest(),
                &|| Ok(())
            )
            .is_err()
    );
    assert_eq!(fs::read(&fixture.selection.path).unwrap(), fixture.old_body);
}

#[test]
fn real_installer_commits_tags_body_binding_lists_outbox_and_replays_sealed_receipt() {
    let fixture = Fixture::new("tag-real-commit");
    let token = fixture.begin();
    let before = fixture.repository.revision().unwrap();
    let mut coordinator = fixture.coordinator(token.clone());
    let receipt = fixture.install(&mut coordinator).unwrap();
    assert_ne!(receipt.old_body_sha256, receipt.new_body_sha256);
    assert_eq!(
        receipt.new_body_sha256,
        format!(
            "{:x}",
            Sha256::digest(fs::read(&fixture.selection.path).unwrap())
        )
    );
    let operation = fixture
        .repository
        .tag_edit_operation(&token.operation_id)
        .unwrap()
        .unwrap();
    assert_eq!(operation.phase, TagEditPhase::Published);
    let commit = operation.commit.unwrap();
    assert_eq!(commit.receipt.audio_sha256, receipt.audio_sha256);
    fixture
        .repository
        .verify_tag_edit_commit(&token, &receipt)
        .unwrap();
    let binding = fixture
        .repository
        .library_asset_binding(&fixture.selection.asset_id)
        .unwrap()
        .unwrap();
    assert_eq!(
        binding.binding_revision,
        fixture.selection.expected_binding_revision + 1
    );
    assert_eq!(
        binding.content_sha256.as_deref(),
        Some(receipt.new_body_sha256.as_str())
    );
    let authority = fixture
        .repository
        .local_asset_authority(&fixture.selection.asset_id)
        .unwrap()
        .unwrap();
    assert_eq!(
        authority.explicit_file_epoch,
        Some(fixture.selection.grant_epoch)
    );
    let track = fixture
        .repository
        .track(&authority.track_id)
        .unwrap()
        .unwrap();
    assert_eq!(track.title, "Actual same-handle title");
    assert_eq!(track.artists, vec!["Actual artist"]);
    assert_eq!(track.album, "Actual album");
    let item = fixture
        .repository
        .list_tracks(DEFAULT_LIST)
        .unwrap()
        .pop()
        .unwrap();
    assert_eq!(item.title, track.title);
    assert_eq!(item.artists, track.artists);
    assert_eq!(item.album, track.album);
    let changes = fixture.repository.changes_since(before, 100).unwrap();
    assert_eq!(
        changes
            .iter()
            .filter(|change| change.kind == ChangeKind::LocalTagsEdited)
            .count(),
        1
    );
    assert_eq!(changes.last().unwrap().revision, commit.change.revision);
    let current = fixture.repository.revision().unwrap();
    let replay = fixture.repository.begin_tag_edit(
        "explicit-tag-consent",
        token.approval_epoch,
        fixture.budget(),
        &intent_digest(),
        &|| Ok(()),
    );
    // The same immutable intent returns its durable receipt without admitting
    // a fresh writer or incrementing repository/outbox revision.
    assert_eq!(
        replay.unwrap().commit.unwrap().receipt.new_body_sha256,
        receipt.new_body_sha256
    );
    assert_eq!(fixture.repository.revision().unwrap(), current);
    assert!(!fixture.repository.pending_tag_edits().unwrap().is_empty());
}

#[test]
fn actual_sqlite_commit_failure_restores_original_audio_and_keeps_owned_recovery_ledger() {
    let fixture = Fixture::new("tag-real-sql-failure");
    let token = fixture.begin();
    let db = rusqlite::Connection::open(fixture.repository.path()).unwrap();
    db.execute_batch("CREATE TRIGGER reject_tag_commit BEFORE INSERT ON change_outbox WHEN json_extract(NEW.document,'$.kind')='local_tags_edited' BEGIN SELECT RAISE(ABORT,'actual injected SQLite commit failure'); END;").unwrap();
    let before = fixture.repository.revision().unwrap();
    let mut coordinator = fixture.coordinator(token.clone());
    assert!(fixture.install(&mut coordinator).is_err());
    assert_eq!(fs::read(&fixture.selection.path).unwrap(), fixture.old_body);
    let operation = fixture
        .repository
        .tag_edit_operation(&token.operation_id)
        .unwrap()
        .unwrap();
    assert!(operation.commit.is_none());
    assert_eq!(operation.phase, TagEditPhase::Restored);
    assert!(
        fixture
            .repository
            .changes_since(before, 100)
            .unwrap()
            .iter()
            .all(|change| change.kind != ChangeKind::LocalTagsEdited)
    );
    let binding = fixture
        .repository
        .library_asset_binding(&fixture.selection.asset_id)
        .unwrap()
        .unwrap();
    assert_eq!(
        binding.binding_revision,
        fixture.selection.expected_binding_revision + 1
    );
    assert_eq!(
        binding.content_sha256.as_deref(),
        fixture.selection.expected_body_sha256.as_deref()
    );
    let restored_asset = fixture
        .repository
        .local_asset_authority(&fixture.selection.asset_id)
        .unwrap()
        .unwrap();
    assert_eq!(
        restored_asset.explicit_file_epoch,
        Some(fixture.selection.grant_epoch)
    );
    db.execute_batch("DROP TRIGGER reject_tag_commit;").unwrap();
    drop(db);
    drop(coordinator);
    let operation_id = token.operation_id.clone();
    let Fixture {
        repository,
        profile,
        _media,
        ..
    } = fixture;
    drop(repository);
    let reopened = Repository::open(profile.path()).unwrap();
    assert!(
        reopened
            .tag_edit_operation(&operation_id)
            .unwrap()
            .is_some()
    );
    assert!(!reopened.pending_tag_edits().unwrap().is_empty());
}

#[test]
fn reservations_precede_actual_artifact_creation_and_reject_global_four_gib_overflow() {
    let fixture = Fixture::new("tag-quota-first");
    let mut budget = fixture.budget();
    budget.original_bytes = 1024 * 1024 * 1024;
    budget.backup_bytes = 1024 * 1024 * 1024;
    budget.candidate_bytes = 1024 * 1024 * 1024;
    budget.journal_bytes = 1024 * 1024;
    let approval = fixture
        .repository
        .approve_tag_write("large-reservation", &fixture.selection, &|| Ok(()))
        .unwrap();
    let operation = fixture
        .repository
        .begin_tag_edit(
            &approval.approval_id,
            approval.approval_epoch,
            budget,
            &intent_digest(),
            &|| Ok(()),
        )
        .unwrap();
    assert!(!fixture.profile.path().join("tag-edits").exists());
    assert!(
        operation
            .artifacts
            .iter()
            .all(|artifact| artifact.actual_bytes == 0
                && artifact.identity.is_none()
                && !artifact.path.exists())
    );
    // Admit another actual asset into the same Repository. Its combined
    // reservations would exceed four GiB even though no artifact exists yet.
    let path = fixture._media.path().join("second.flac");
    fs::write(&path, &fixture.old_body).unwrap();
    let context = ScanContext::new(Duration::from_secs(30));
    let root = RootAuthority::open(fixture._media.path(), false, &context).unwrap();
    let file = root.open_file(&path).unwrap();
    let identity = file.identity().clone();
    let barrier = root.retirement_barrier();
    drop(file);
    drop(root);
    assert!(barrier.wait(Duration::from_secs(3)));
    let (track, asset) = local_track_from_path(&path).unwrap();
    fixture
        .repository
        .upsert_explicit_library_tracks(
            &[LibraryPreparedAsset {
                track,
                asset: asset.clone(),
                file_identity: Some(identity.clone()),
            }],
            &|| Ok(()),
        )
        .unwrap();
    let second = TagWriteSelection {
        operation_id: "tag-quota-second".into(),
        asset_id: asset.id,
        path,
        expected_identity: identity,
        root_id: None,
        grant_epoch: 1,
        expected_binding_revision: 1,
        expected_body_sha256: fixture.selection.expected_body_sha256.clone(),
    };
    let second_approval = fixture
        .repository
        .approve_tag_write("second-real-approval", &second, &|| Ok(()))
        .unwrap();
    assert!(
        fixture
            .repository
            .begin_tag_edit(
                &second_approval.approval_id,
                second_approval.approval_epoch,
                budget,
                &intent_digest(),
                &|| Ok(())
            )
            .is_err()
    );
    assert!(
        fixture
            .repository
            .tag_edit_operation(&second.operation_id)
            .unwrap()
            .is_none()
    );
    assert!(!fixture.profile.path().join("tag-edits").exists());
}

#[test]
fn schema_fifteen_upgrade_creates_a_real_validated_backup_and_preserves_existing_library() {
    let Fixture {
        repository,
        profile,
        _media,
        selection,
        ..
    } = Fixture::new("migration-library-record");
    let lists = repository.list_data().unwrap();
    let expected_items = repository.list_tracks(DEFAULT_LIST).unwrap();
    assert_eq!(expected_items.len(), 1);
    let expected_item = serde_json::to_value(&expected_items[0]).unwrap();
    let track_id = repository
        .local_asset_authority(&selection.asset_id)
        .unwrap()
        .unwrap()
        .track_id;
    let expected_track =
        serde_json::to_value(repository.track(&track_id).unwrap().unwrap()).unwrap();
    let revision = repository.revision().unwrap();
    drop(repository);
    let path = profile.path().join(DATABASE_NAME);
    let db = rusqlite::Connection::open(&path).unwrap();
    db.execute_batch("DROP TABLE download_allocation_intents; DROP TABLE tag_edit_artifacts; DROP TABLE tag_edit_operations; DROP TABLE tag_write_approvals; PRAGMA user_version=15;").unwrap();
    drop(db);
    let reopened = Repository::open(profile.path()).unwrap();
    assert_eq!(SCHEMA_VERSION, 17);
    assert_eq!(reopened.list_data().unwrap(), lists);
    assert_eq!(reopened.revision().unwrap(), revision);
    assert_eq!(
        serde_json::to_value(reopened.track(&track_id).unwrap().unwrap()).unwrap(),
        expected_track
    );
    assert_eq!(
        serde_json::to_value(&reopened.list_tracks(DEFAULT_LIST).unwrap()[0]).unwrap(),
        expected_item
    );
    reopened.integrity_check().unwrap();
    let backup_prefix = format!("{}.schema-15-", path.file_stem().unwrap().to_string_lossy());
    let backups: Vec<PathBuf> = fs::read_dir(profile.path())
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            let name = path.file_name().unwrap().to_string_lossy();
            name.starts_with(&backup_prefix) && name.ends_with(".bak")
        })
        .collect();
    assert_eq!(backups.len(), 1);
    let backup = rusqlite::Connection::open_with_flags(
        &backups[0],
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    assert_eq!(
        backup
            .pragma_query_value(None, "user_version", |row| row.get::<_, u32>(0))
            .unwrap(),
        15
    );
    assert_eq!(
        backup
            .query_row("PRAGMA integrity_check", [], |row| row.get::<_, String>(0))
            .unwrap(),
        "ok"
    );
    let backup_track: String = backup
        .query_row(
            "SELECT document FROM tracks WHERE id=?1",
            [&track_id],
            |row| row.get(0),
        )
        .unwrap();
    let backup_item: String = backup
        .query_row(
            "SELECT document FROM music_list_items WHERE list_id=?1 AND track_id=?2",
            rusqlite::params![DEFAULT_LIST, track_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&backup_track).unwrap(),
        expected_track
    );
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&backup_item).unwrap(),
        expected_item
    );
    let backup_revision = backup
        .query_row(
            "SELECT revision FROM repository_state WHERE singleton=1",
            [],
            |row| row.get::<_, i64>(0),
        )
        .unwrap();
    assert_eq!(u64::try_from(backup_revision).unwrap(), revision);
}

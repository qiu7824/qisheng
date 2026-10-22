use super::*;
use std::time::Duration;

fn context() -> ImpulseDecodeContext {
    ImpulseDecodeContext::new(Duration::from_secs(30)).unwrap()
}

fn interrupted(
    phase: ImpulseVaultCreationPhase,
) -> (
    tempfile::TempDir,
    SnapshotDirectory,
    ImpulseVaultCreationFact,
    FileIdentity,
) {
    let temp = tempfile::tempdir().unwrap();
    let profile = SnapshotDirectory::pin_profile(temp.path(), 1).unwrap();
    let mut creator = ImpulseVaultCreator::prepare(&profile).unwrap();
    let mut durable = None;
    assert!(
        creator
            .advance(&context(), &mut |fact| {
                if fact.phase == phase {
                    anyhow::bail!("Injected creation FID acknowledgement failure");
                }
                durable = Some(fact.clone());
                Ok(())
            })
            .is_err()
    );
    let original = if phase == ImpulseVaultCreationPhase::DirectoryPinned {
        platform::identity(creator.allocated_directory.as_ref().unwrap()).unwrap()
    } else {
        platform::identity(creator.owner.as_ref().unwrap()).unwrap()
    };
    drop(creator);
    (temp, profile, durable.unwrap(), original)
}

#[test]
fn cold_unknown_directory_requires_actual_selection_and_preserves_foreign_objects() {
    let (temp, profile, fact, original) = interrupted(ImpulseVaultCreationPhase::DirectoryPinned);
    assert!(fact.directory_identity.is_none());
    let namespace = temp.path().join(DIRECTORY);
    let mut resumed = ImpulseVaultCreator::resume(&profile, &fact).unwrap();
    assert!(resumed.advance(&context(), &mut |_| Ok(())).is_err());
    assert!(!namespace.join(OWNER).exists());
    let foreign_directory = temp.path().join("foreign-directory");
    fs::create_dir(&foreign_directory).unwrap();
    assert!(
        resumed
            .reapprove_namespace(
                SnapshotDirectory::open_selected(&foreign_directory, 1).unwrap(),
                None,
                &context(),
            )
            .is_err()
    );
    let foreign = namespace.join("foreign.txt");
    fs::write(&foreign, b"foreign original").unwrap();
    assert!(
        resumed
            .reapprove_namespace(
                SnapshotDirectory::open_selected(&namespace, 1).unwrap(),
                None,
                &context(),
            )
            .is_err()
    );
    assert_eq!(fs::read(&foreign).unwrap(), b"foreign original");
    assert!(resumed.fact.directory_identity.is_none());
    fs::remove_file(&foreign).unwrap();
    resumed
        .reapprove_namespace(
            SnapshotDirectory::open_selected(&namespace, 1).unwrap(),
            None,
            &context(),
        )
        .unwrap();
    resumed.advance(&context(), &mut |_| Ok(())).unwrap();
    assert!(
        resumed
            .receipt()
            .unwrap()
            .directory_identity
            .matches(&original)
    );
}

#[test]
fn cold_owner_selection_and_failed_journal_keep_the_original_writable_descriptor() {
    let (temp, profile, fact, original) = interrupted(ImpulseVaultCreationPhase::OwnerAllocated);
    assert!(fact.owner_identity.is_none());
    let namespace = temp.path().join(DIRECTORY);
    let owner = namespace.join(OWNER);
    let authority = crate::library::authority::AuthorityRegistry::default();
    let mut resumed = ImpulseVaultCreator::resume(&profile, &fact).unwrap();
    assert!(
        resumed
            .reapprove_namespace(
                SnapshotDirectory::open_selected(&namespace, 1).unwrap(),
                None,
                &context(),
            )
            .is_err()
    );
    let foreign = temp.path().join("foreign-owner.lock");
    fs::write(&foreign, []).unwrap();
    assert!(
        resumed
            .reapprove_namespace(
                SnapshotDirectory::open_selected(&namespace, 1).unwrap(),
                Some(authority.open_selection(&foreign).unwrap()),
                &context(),
            )
            .is_err()
    );
    assert_eq!(fs::metadata(&foreign).unwrap().len(), 0);
    assert_eq!(fs::metadata(&owner).unwrap().len(), 0);
    assert!(resumed.owner.is_none());
    resumed
        .reapprove_namespace(
            SnapshotDirectory::open_selected(&namespace, 1).unwrap(),
            Some(authority.open_selection(&owner).unwrap()),
            &context(),
        )
        .unwrap();
    assert!(
        resumed
            .advance(&context(), &mut |_| anyhow::bail!(
                "Failed durable recovery ACK"
            ))
            .is_err()
    );
    assert_eq!(
        platform::identity(resumed.owner.as_ref().unwrap()).unwrap(),
        original
    );
    assert!(fs::rename(&owner, temp.path().join("displaced.lock")).is_err());
    resumed
        .reapprove_namespace(
            SnapshotDirectory::open_selected(&namespace, 1).unwrap(),
            None,
            &context(),
        )
        .unwrap();
    resumed.advance(&context(), &mut |_| Ok(())).unwrap();
    assert!(resumed.receipt().unwrap().owner_identity.matches(&original));
}

#[test]
fn same_owner_fid_with_foreign_body_is_rejected_before_inventory_or_write() {
    let (temp, profile, fact, original) = interrupted(ImpulseVaultCreationPhase::OwnerAllocated);
    let namespace = temp.path().join(DIRECTORY);
    let path = namespace.join(OWNER);
    fs::write(&path, b"changed same native object").unwrap();
    let authority = crate::library::authority::AuthorityRegistry::default();
    let selected = authority.open_selection(&path).unwrap();
    assert!(SelectedOwnerApproval::matches_identity(
        selected.identity(),
        &original
    ));
    let mut resumed = ImpulseVaultCreator::resume(&profile, &fact).unwrap();
    assert!(
        resumed
            .reapprove_namespace(
                SnapshotDirectory::open_selected(&namespace, 1).unwrap(),
                Some(selected),
                &context(),
            )
            .is_err()
    );
    assert_eq!(resumed.fact, fact);
    assert!(resumed.owner.is_none());
    assert_eq!(fs::read(&path).unwrap(), b"changed same native object");
}

#[test]
fn cold_preallocation_intent_uses_a_retained_exclusive_directory_allocation() {
    let temp = tempfile::tempdir().unwrap();
    let profile = SnapshotDirectory::pin_profile(temp.path(), 1).unwrap();
    let mut creator = ImpulseVaultCreator::prepare(&profile).unwrap();
    let fact = creator.fact().clone();
    assert!(
        creator
            .advance(&context(), &mut |_| anyhow::bail!("Intent ACK failed"))
            .is_err()
    );
    drop(creator);
    let mut resumed = ImpulseVaultCreator::resume(&profile, &fact).unwrap();
    resumed.advance(&context(), &mut |_| Ok(())).unwrap();
    assert!(
        resumed
            .receipt()
            .unwrap()
            .directory_identity
            .matches(&platform::identity(resumed.allocated_directory.as_ref().unwrap()).unwrap())
    );
}

#[test]
fn consumed_selection_uses_the_retained_writer_and_rejects_foreign_or_revoked_grants() {
    let (temp, profile, fact, original) = interrupted(ImpulseVaultCreationPhase::OwnerAllocated);
    let namespace = temp.path().join(DIRECTORY);
    let owner_path = namespace.join(OWNER);
    let authority = crate::library::authority::AuthorityRegistry::default();
    let mut resumed = ImpulseVaultCreator::resume(&profile, &fact).unwrap();
    resumed
        .reapprove_namespace(
            SnapshotDirectory::open_selected(&namespace, 1).unwrap(),
            Some(authority.open_selection(&owner_path).unwrap()),
            &context(),
        )
        .unwrap();
    let grant = resumed.owner_grant.as_ref().unwrap();
    let writer = resumed.owner.as_ref().unwrap();
    assert!(
        grant.check().is_err(),
        "Global cached reader validation must not gain a sharing bypass"
    );
    grant.check_retained_file(writer).unwrap();
    let foreign = temp.path().join("foreign-writer.lock");
    fs::write(&foreign, b"foreign writer body").unwrap();
    let foreign_writer = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&foreign)
        .unwrap();
    assert!(grant.check_retained_file(&foreign_writer).is_err());
    assert_eq!(fs::read(&foreign).unwrap(), b"foreign writer body");
    let cancelled = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let revocable = grant.with_cancel(cancelled.clone()).unwrap();
    revocable.check_retained_file(writer).unwrap();
    cancelled.store(true, std::sync::atomic::Ordering::Release);
    assert!(revocable.check_retained_file(writer).is_err());
    let metadata_only = crate::domain::library::LocalAssetAuthority {
        asset_id: "unselected-owner".into(),
        track_id: "unselected-owner".into(),
        path: owner_path,
        explicit_file_epoch: Some(1),
        roots: Vec::new(),
    };
    let unselected = authority.install(&metadata_only, None).unwrap();
    assert!(
        unselected.check_retained_file(writer).is_err(),
        "Persisted intent without a selected FID became a grant"
    );
    resumed.advance(&context(), &mut |_| Ok(())).unwrap();
    assert!(resumed.receipt().unwrap().owner_identity.matches(&original));
}

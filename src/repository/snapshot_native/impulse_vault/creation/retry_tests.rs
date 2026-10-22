use super::*;
use std::time::Duration;

#[test]
fn pinned_unattested_directory_is_rechecked_before_same_creator_retry() {
    let temp = tempfile::tempdir().unwrap();
    let profile = SnapshotDirectory::pin_profile(temp.path(), 1).unwrap();
    let namespace = temp.path().join(DIRECTORY);
    let foreign = namespace.join("foreign.txt");
    let foreign_body = b"preserve the actual foreign representation";
    let mut creator = ImpulseVaultCreator::prepare(&profile).unwrap();
    creator.obstruction_after_allocate = Some(foreign_body.to_vec());
    let context = ImpulseDecodeContext::new(Duration::from_secs(30)).unwrap();
    let mut observed = Vec::new();

    assert!(
        creator
            .advance(&context, &mut |fact| {
                observed.push(fact.clone());
                Ok(())
            })
            .is_err()
    );
    assert_eq!(fs::read(&foreign).unwrap(), foreign_body);
    assert!(creator.directory.is_some());
    assert!(creator.fact.directory_identity.is_none());
    assert!(creator.owner.is_none());
    assert!(!namespace.join(OWNER).exists());
    let original_directory = creator.directory.as_ref().unwrap().identity().clone();

    // Removing this test's own obstruction allows a retry of the same retained
    // directory proof. No original owner or new namespace is substituted.
    fs::remove_file(&foreign).unwrap();
    creator
        .advance(&context, &mut |fact| {
            observed.push(fact.clone());
            Ok(())
        })
        .unwrap();

    let receipt = creator.receipt().unwrap();
    assert_eq!(creator.fact.phase, ImpulseVaultCreationPhase::Ready);
    assert!(receipt.directory_identity.matches(&original_directory));
    let actual_owner = platform::identity(creator.owner.as_ref().unwrap()).unwrap();
    assert!(receipt.owner_identity.matches(&actual_owner));
    let pinned_index = observed
        .iter()
        .position(|fact| fact.phase == ImpulseVaultCreationPhase::DirectoryPinned)
        .unwrap();
    let allocated_index = observed
        .iter()
        .position(|fact| fact.phase == ImpulseVaultCreationPhase::OwnerAllocated)
        .unwrap();
    assert!(pinned_index < allocated_index);
    assert!(observed[pinned_index].directory_identity.is_some());
    assert!(observed[pinned_index].owner_identity.is_none());
    assert!(observed[allocated_index].owner_identity.is_some());
}

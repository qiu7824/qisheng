use lx_gpui_player::download::postprocess_retry::RetryDeletionAttestation;

#[test]
fn native_lowercase_volume_guid_is_preserved_by_retry_json_roundtrip() {
    let native = RetryDeletionAttestation {
        volume_guid: r"\\?\volume{9ee3a8dc-cc2a-4c0e-a4ec-96b2f9a3d495}\".into(),
        filesystem: "NTFS".into(),
    };
    native.validate().unwrap();
    let restored: RetryDeletionAttestation =
        serde_json::from_slice(&serde_json::to_vec(&native).unwrap()).unwrap();
    assert_eq!(restored, native);
    restored.validate().unwrap();
}

#[test]
fn retry_deletion_rejects_noncanonical_aliases_and_malformed_volume_identifiers() {
    for value in [
        r"\\?\Volume{9ee3a8dc-cc2a-4c0e-a4ec-96b2f9a3d495}\",
        r"E:\",
        r"\\?\volume{9ee3a8dc-cc2a-4c0e-a4ec-96b2f9a3d49z}\",
        r"\\?\volume{9ee3a8dc-cc2a-4c0e-a4ec-96b2f9a3d495}\foreign",
        r"\\?\volume{9ee3a8dc-cc2a-4c0e-a4ec-96b2f9a3d495}",
    ] {
        assert!(
            RetryDeletionAttestation {
                volume_guid: value.into(),
                filesystem: "NTFS".into()
            }
            .validate()
            .is_err(),
            "{value}"
        );
    }
    assert!(
        RetryDeletionAttestation {
            volume_guid: r"\\?\volume{9ee3a8dc-cc2a-4c0e-a4ec-96b2f9a3d495}\".into(),
            filesystem: "FAT32".into()
        }
        .validate()
        .is_err()
    );
}

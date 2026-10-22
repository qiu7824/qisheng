use super::*;
use std::{
    process::{Command, Stdio},
    time::Instant,
};

fn escaped_lyrics_flac() -> Vec<u8> {
    let base = encoded::mono_verbatim_flac();
    let lyric = format!("LYRICS={}", "\"".repeat(700 * 1024));
    let vendor = b"owned-fixture";
    let mut comments = Vec::new();
    comments.extend_from_slice(&(vendor.len() as u32).to_le_bytes());
    comments.extend_from_slice(vendor);
    comments.extend_from_slice(&1u32.to_le_bytes());
    comments.extend_from_slice(&(lyric.len() as u32).to_le_bytes());
    comments.extend_from_slice(lyric.as_bytes());
    assert!(comments.len() < 0x0100_0000);
    let mut bytes = base[..42].to_vec();
    bytes[4] = 0;
    bytes.push(0x84);
    bytes.extend_from_slice(&(comments.len() as u32).to_be_bytes()[1..]);
    bytes.extend_from_slice(&comments);
    bytes.extend_from_slice(&base[42..]);
    bytes
}
fn fixture() -> Fixture {
    let profile = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
    let media = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
    let path = media.path().join("escaped-original.flac");
    let old_body = escaped_lyrics_flac();
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
    let epoch = repository
        .local_asset_authority(&asset.id)
        .unwrap()
        .unwrap()
        .explicit_file_epoch
        .unwrap();
    Fixture {
        repository,
        profile,
        _media: media,
        selection: TagWriteSelection {
            operation_id: "escaped-existing-metadata".into(),
            asset_id: asset.id,
            path,
            expected_identity: identity,
            root_id: None,
            grant_epoch: epoch,
            expected_binding_revision: binding.binding_revision,
            expected_body_sha256: Some(format!("{:x}", Sha256::digest(&old_body))),
        },
        old_body,
    }
}

#[test]
fn escaped_metadata_process_helper() {
    let Some(profile) = std::env::var_os("LX_TAG_ESCAPED_PROFILE") else {
        return;
    };
    let repository = Arc::new(Repository::open(profile).unwrap());
    let operation = repository
        .tag_edit_operation("escaped-existing-metadata")
        .unwrap()
        .unwrap();
    let token = operation.token;
    let mut coordinator = RepositoryTagEditCoordinator::new(
        repository.clone(),
        token.clone(),
        Arc::new(|| Ok(())),
        Arc::new(|| Ok(())),
    )
    .unwrap();
    let directory = coordinator.journal_directory().unwrap();
    fs::create_dir_all(&directory).unwrap();
    let patch = LocalTagPatch {
        title: TagField::Set("Actual same-handle title".into()),
        artists: TagField::Set(vec!["Actual artist".into()]),
        album: TagField::Set("Actual album".into()),
        ..Default::default()
    };
    let result = install_selected_tags_owned(
        token.selection.clone(),
        &directory,
        &patch,
        &ScanContext::new(Duration::from_secs(30)),
        &mut coordinator,
    );
    let mut owner = match result {
        Ok(_) => panic!("Escaped metadata must be rejected within the persistent record budget"),
        Err(error) => match error.downcast::<TagInstallError>().unwrap() {
            TagInstallError::Owned { owner, .. } => *owner,
            _ => panic!("Metadata rejection lost its actual native owner"),
        },
    };
    if owner
        .retire(&ScanContext::new(Duration::from_secs(30)), &mut coordinator)
        .is_err()
    {
        // Retry the actual mandatory rollback to distinguish journal framing
        // failure from the remaining SQLite restoration document budget.
        for _ in 0..2 {
            let _ =
                owner.retry_restore(&ScanContext::new(Duration::from_secs(30)), &mut coordinator);
        }
        let _ = owner.retire(&ScanContext::new(Duration::from_secs(30)), &mut coordinator);
    }
    let current = repository
        .tag_edit_operation(&token.operation_id)
        .unwrap()
        .unwrap();
    if owner.is_retired() && !owner.has_live_handles() && current.is_finished() {
        std::process::exit(65);
    }
    assert!(owner.has_live_handles());
    assert!(!current.is_finished());
    assert!(current.commit.is_none());
    std::process::exit(64);
}

#[test]
fn escaped_existing_metadata_rejects_before_original_write_and_retires_all_actual_owners() {
    use lofty::{file::TaggedFileExt, tag::ItemKey};
    let fixture = fixture();
    let tagged = lofty::probe::Probe::open(&fixture.selection.path)
        .unwrap()
        .read()
        .unwrap();
    let lyrics = tagged
        .primary_tag()
        .unwrap()
        .get_string(ItemKey::Lyrics)
        .unwrap();
    assert_eq!(lyrics.len(), 700 * 1024);
    assert!(lyrics.len() < 1024 * 1024);
    assert!(serde_json::to_vec(lyrics).unwrap().len() > 1024 * 1024);
    let approval = fixture
        .repository
        .approve_tag_write("explicit-escaped-consent", &fixture.selection, &|| Ok(()))
        .unwrap();
    let mut budget = fixture.budget();
    budget.journal_bytes = 8 * 1024 * 1024;
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
    let expected_mtime = fs::metadata(&fixture.selection.path)
        .unwrap()
        .modified()
        .unwrap();
    let Fixture {
        repository,
        profile,
        _media,
        selection,
        old_body,
    } = fixture;
    drop(repository);
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args([
            "--exact",
            "metadata_budget_cases::escaped_metadata_process_helper",
            "--nocapture",
        ])
        .env("LX_TAG_ESCAPED_PROFILE", profile.path())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    use std::os::windows::process::CommandExt;
    command.creation_flags(windows_sys::Win32::System::Threading::CREATE_NO_WINDOW);
    let mut child = command.spawn().unwrap();
    let deadline = Instant::now() + Duration::from_secs(45);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        assert!(
            Instant::now() < deadline,
            "Isolated metadata budget worker did not stop"
        );
        std::thread::sleep(Duration::from_millis(10));
    };
    assert_eq!(fs::read(&selection.path).unwrap(), old_body);
    assert_eq!(
        fs::metadata(&selection.path).unwrap().modified().unwrap(),
        expected_mtime,
        "Persistent metadata budget must reject before any actual original write"
    );
    assert_eq!(
        status.code(),
        Some(65),
        "Actual metadata budget rejection did not retire its native owners"
    );
    let repository = Repository::open(profile.path()).unwrap();
    let completed = repository
        .tag_edit_operation(&operation.token.operation_id)
        .unwrap()
        .unwrap();
    assert!(completed.is_finished());
    assert!(completed.commit.is_none() && completed.restoration.is_none());
    assert_eq!(repository.tag_edit_quota().unwrap().charged_bytes, 0);
    assert_eq!(
        repository
            .library_asset_binding(&selection.asset_id)
            .unwrap()
            .unwrap()
            .binding_revision,
        selection.expected_binding_revision
    );
}

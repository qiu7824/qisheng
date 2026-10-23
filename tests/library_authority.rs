#![cfg(any(windows, target_os = "linux", target_os = "macos"))]
use lx_gpui_player::domain::library::{LibraryRoot, LocalAssetAuthority};
use lx_gpui_player::library::authority::{AuthorityRegistry, RootAuthority, RootHealth};
use lx_gpui_player::library::scan::ScanContext;
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

fn context() -> ScanContext {
    ScanContext::new(Duration::from_secs(20))
}
fn root(path: &std::path::Path, id: &str, recursive: bool) -> LibraryRoot {
    let authority = RootAuthority::open(path, recursive, &context()).unwrap();
    let barrier = authority.retirement_barrier();
    let draft = authority.draft();
    drop(authority);
    assert!(barrier.wait(Duration::from_secs(3)));
    LibraryRoot {
        root_id: id.into(),
        path: draft.path,
        recursive,
        grant_epoch: 1,
        anchor: draft.anchor,
        path_policy: draft.path_policy,
        watch_enabled: false,
        generation: 1,
        last_complete_generation: 0,
    }
}
fn intent(
    path: &std::path::Path,
    roots: Vec<LibraryRoot>,
    explicit: Option<u64>,
) -> LocalAssetAuthority {
    LocalAssetAuthority {
        asset_id: "asset:test".into(),
        track_id: "local-test".into(),
        path: fs::canonicalize(path).unwrap(),
        explicit_file_epoch: explicit,
        roots,
    }
}
#[test]
fn verified_handle_clones_have_independent_cursors_and_cancel_before_reads() {
    let temp = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
    let directory = temp.path().join("中文 audio directory");
    fs::create_dir(&directory).unwrap();
    let path = directory.join("ordered.wav");
    fs::write(&path, b"abcdefghijkl").unwrap();
    let registry = AuthorityRegistry::default();
    let authorization = registry
        .install(
            &intent(&path, vec![root(&directory, "root:one", true)], None),
            None,
        )
        .unwrap();
    let cancel = Arc::new(AtomicBool::new(false));
    let mut first = authorization
        .with_cancel(cancel.clone())
        .unwrap()
        .open()
        .unwrap();
    let mut second = first.try_clone().unwrap();
    let mut bytes = [0; 3];
    first.read_exact(&mut bytes).unwrap();
    assert_eq!(&bytes, b"abc");
    second.seek(SeekFrom::Start(6)).unwrap();
    second.read_exact(&mut bytes).unwrap();
    assert_eq!(&bytes, b"ghi");
    first.read_exact(&mut bytes).unwrap();
    assert_eq!(&bytes, b"def");
    assert_eq!(first.identity(), second.identity());
    cancel.store(true, Ordering::Release);
    assert!(first.read(&mut bytes).is_err());
    assert!(second.read(&mut bytes).is_err());
    assert_eq!(fs::read(path).unwrap(), b"abcdefghijkl");
}
#[test]
fn persisted_intent_is_unknown_until_actual_directory_proof_without_audio_payload_open() {
    let temp = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
    let path = temp.path().join("audio.wav");
    fs::write(&path, b"owned audio payload").unwrap();
    let selected = root(temp.path(), "root:health", true);
    let record = intent(&path, vec![selected.clone()], None);
    let registry = AuthorityRegistry::default();
    registry.install_root(&selected).unwrap();
    registry.install(&record, None).unwrap();
    assert_eq!(
        registry.root_health(&selected.root_id, 1),
        RootHealth::Unknown
    );
    assert!(!registry.effective_authority(&record));
    #[cfg(windows)]
    let _exclusive_audio = {
        use std::os::windows::fs::OpenOptionsExt;
        fs::OpenOptions::new()
            .read(true)
            .write(true)
            .share_mode(0)
            .open(&path)
            .unwrap()
    };
    assert_eq!(
        registry.verify_root(&selected, &context()).unwrap(),
        RootHealth::Verified
    );
    assert!(registry.effective_authority(&record));
    registry.begin_shutdown();
    assert!(!registry.effective_authority(&record));
    assert!(registry.retirement_barrier().wait(Duration::from_secs(3)));
}
#[test]
fn unavailable_root_can_restore_same_anchor_but_a_new_anchor_needs_explicit_epoch() {
    let temp = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
    let path = temp.path().join("selected");
    fs::create_dir(&path).unwrap();
    let audio = path.join("audio.wav");
    fs::write(&audio, b"original").unwrap();
    let selected = root(&path, "root:recover", true);
    let record = intent(&audio, vec![selected.clone()], None);
    let registry = AuthorityRegistry::default();
    registry.install_root(&selected).unwrap();
    registry.install(&record, None).unwrap();
    let offline = temp.path().join("offline");
    fs::rename(&path, &offline).unwrap();
    assert!(registry.verify_root(&selected, &context()).is_err());
    assert_eq!(
        registry.root_health(&selected.root_id, 1),
        RootHealth::Unavailable
    );
    assert!(!registry.effective_authority(&record));
    fs::rename(&offline, &path).unwrap();
    assert_eq!(
        registry.verify_root(&selected, &context()).unwrap(),
        RootHealth::Verified
    );
    assert!(registry.effective_authority(&record));
    registry.begin_shutdown();
    assert!(registry.retirement_barrier().wait(Duration::from_secs(3)));
    drop(registry);
    let registry = AuthorityRegistry::default();
    registry.install_root(&selected).unwrap();
    registry.install(&record, None).unwrap();
    fs::rename(&path, &offline).unwrap();
    fs::create_dir(&path).unwrap();
    fs::write(&audio, b"replacement").unwrap();
    assert!(registry.verify_root(&selected, &context()).is_err());
    assert_eq!(
        registry.root_health(&selected.root_id, 1),
        RootHealth::NeedsReapproval
    );
    assert!(!registry.effective_authority(&record));
    assert!(registry.verify_root(&selected, &context()).is_err());
    let new_authority = RootAuthority::open(&path, true, &context()).unwrap();
    let new_draft = new_authority.draft();
    let mut renewed = selected.clone();
    renewed.grant_epoch = 2;
    renewed.anchor = new_draft.anchor;
    renewed.path = new_draft.path;
    registry
        .accept_root_proof(&renewed, new_authority.proof().unwrap())
        .unwrap();
    let renewed_record = intent(&audio, vec![renewed.clone()], None);
    registry.install(&renewed_record, None).unwrap();
    assert_eq!(
        registry.root_health(&renewed.root_id, 2),
        RootHealth::Verified
    );
    assert!(registry.effective_authority(&renewed_record));
    assert!(!registry.effective_authority(&record));
    drop(new_authority);
    registry.begin_shutdown();
    assert!(registry.retirement_barrier().wait(Duration::from_secs(3)));
}
#[test]
fn shutdown_keeps_live_reader_proof_count_until_actual_retirement() {
    let temp = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
    let audio = temp.path().join("audio.wav");
    fs::write(&audio, b"owned").unwrap();
    let registry = AuthorityRegistry::default();
    let record = intent(&audio, vec![root(temp.path(), "root:held", true)], None);
    let authorization = registry.install(&record, None).unwrap();
    let mut opened = authorization.open().unwrap();
    assert!(registry.retirement_pending() > 0);
    registry.begin_shutdown();
    assert!(opened.read(&mut [0]).is_err());
    assert!(!registry.retirement_barrier().wait(Duration::ZERO));
    drop(opened);
    assert!(registry.retirement_barrier().wait(Duration::from_secs(3)));
    assert_eq!(registry.retirement_pending(), 0);
    assert!(authorization.open().is_err());
}
#[test]
fn revocation_preserves_other_proven_roots_and_explicit_grants() {
    let temp = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
    let nested = temp.path().join("nested");
    fs::create_dir(&nested).unwrap();
    let path = nested.join("audio.wav");
    fs::write(&path, b"abcdef").unwrap();
    let first_root = root(temp.path(), "root:parent", true);
    let second_root = root(&nested, "root:child", false);
    let registry = AuthorityRegistry::default();
    let record = intent(
        &path,
        vec![first_root.clone(), second_root.clone()],
        Some(1),
    );
    let authorization = registry.install(&record, None).unwrap();
    let mut opened = authorization.open().unwrap();
    let mut byte = [0];
    registry.revoke_root(&first_root.root_id);
    opened.read_exact(&mut byte).unwrap();
    assert_eq!(byte, [b'a']);
    registry.revoke_root(&second_root.root_id);
    opened.read_exact(&mut byte).unwrap();
    assert_eq!(byte, [b'b']);
    registry.revoke_file(&record.asset_id);
    assert!(opened.read(&mut byte).is_err());
    assert!(registry.install(&record, None).is_err());
    assert!(authorization.open().is_err());
}
#[test]
fn removing_one_root_and_reinstalling_intent_keeps_existing_other_grant_reader_valid() {
    let temp = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
    let nested = temp.path().join("nested");
    fs::create_dir(&nested).unwrap();
    let path = nested.join("audio.wav");
    fs::write(&path, b"abcdef").unwrap();
    let parent = root(temp.path(), "root:parent", true);
    let child = root(&nested, "root:child", true);
    let registry = AuthorityRegistry::default();
    let authorization = registry
        .install(
            &intent(&path, vec![parent.clone(), child.clone()], None),
            None,
        )
        .unwrap();
    let mut opened = authorization.open().unwrap();
    registry.revoke_root(&parent.root_id);
    let refreshed = registry
        .install(&intent(&path, vec![child], None), None)
        .unwrap();
    let mut data = [0; 6];
    opened.read_exact(&mut data).unwrap();
    assert_eq!(&data, b"abcdef");
    refreshed.open().unwrap().read_exact(&mut data).unwrap();
    assert_eq!(&data, b"abcdef");
}
#[test]
fn learning_physical_identity_keeps_the_same_open_file_valid_and_rejects_a_mismatch() {
    let temp = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
    let path = temp.path().join("audio.wav");
    fs::write(&path, b"abcdef").unwrap();
    let registry = AuthorityRegistry::default();
    let record = intent(
        &path,
        vec![root(temp.path(), "root:identity", true)],
        Some(1),
    );
    let authorization = registry.install(&record, None).unwrap();
    let mut opened = authorization.open().unwrap();
    let identity = opened.identity().clone();
    registry.install(&record, Some(identity.clone())).unwrap();
    opened.read_exact(&mut [0; 3]).unwrap();
    registry.install(&record, None).unwrap();
    opened.read_exact(&mut [0; 3]).unwrap();
    let mut different = identity;
    let replacement = if different.file_id.starts_with('f') {
        "0"
    } else {
        "f"
    };
    different.file_id.replace_range(..1, replacement);
    registry.install(&record, Some(different)).unwrap();
    assert!(opened.read(&mut [0]).is_err());
    assert!(authorization.open().is_err());
}
#[test]
fn only_a_missing_exact_sidecar_retains_not_found_kind() {
    let temp = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
    let path = temp.path().join("audio.wav");
    fs::write(&path, [1]).unwrap();
    let registry = AuthorityRegistry::default();
    let record = intent(&path, Vec::new(), Some(1));
    let authorization = registry.install(&record, None).unwrap();
    let error = authorization.open_sidecar("lrc").unwrap_err();
    assert_eq!(
        error.downcast_ref::<std::io::Error>().unwrap().kind(),
        std::io::ErrorKind::NotFound
    );
    registry.revoke_file(&record.asset_id);
    let error = authorization.open_sidecar("lrc").unwrap_err();
    assert!(error.downcast_ref::<std::io::Error>().is_none());
}
#[test]
fn exact_sidecars_share_parent_proof_and_cannot_derive_arbitrary_extensions() {
    let temp = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
    let audio = temp.path().join("song.wav");
    fs::write(&audio, [1]).unwrap();
    fs::write(audio.with_extension("lrc"), b"[00:00.00]owned lyric").unwrap();
    let registry = AuthorityRegistry::default();
    let record = intent(&audio, Vec::new(), Some(1));
    let authorization = registry.install(&record, None).unwrap();
    let mut sidecar = authorization.open_sidecar("lrc").unwrap();
    let mut lyric = String::new();
    sidecar.read_to_string(&mut lyric).unwrap();
    assert_eq!(lyric, "[00:00.00]owned lyric");
    assert!(authorization.open_sidecar("txt").is_err());
    registry.revoke_file(&record.asset_id);
    sidecar.seek(SeekFrom::Start(0)).unwrap_err();
}
#[test]
fn replaced_root_and_non_recursive_escape_require_reapproval() {
    let temp = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
    let selected = temp.path().join("selected");
    fs::create_dir(&selected).unwrap();
    let nested = selected.join("nested");
    fs::create_dir(&nested).unwrap();
    fs::write(nested.join("audio.wav"), [1]).unwrap();
    let record = root(&selected, "root:selected", false);
    let barrier;
    {
        let authority = RootAuthority::restore(&record, &context()).unwrap();
        barrier = authority.retirement_barrier();
        assert!(authority.open_file(nested.join("audio.wav")).is_err());
    }
    assert!(barrier.wait(Duration::from_secs(3)));
    fs::rename(&selected, temp.path().join("original")).unwrap();
    fs::create_dir(&selected).unwrap();
    fs::write(selected.join("outside.wav"), [9]).unwrap();
    let error = RootAuthority::restore(&record, &context())
        .err()
        .unwrap()
        .to_string();
    assert!(error.contains("NeedsReapproval"), "{error}");
    assert_eq!(
        fs::read(temp.path().join("original/nested/audio.wav")).unwrap(),
        [1]
    );
}
#[cfg(windows)]
#[test]
fn unicode_lowercase_collision_cannot_substitute_a_different_physical_root_grant() {
    let temp = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
    let selected = temp.path().join("\u{130}");
    let sibling = temp.path().join("i\u{307}");
    fs::create_dir(&selected).unwrap();
    fs::create_dir(&sibling).unwrap();
    let first_path = selected.join("audio.wav");
    let second_path = sibling.join("audio.wav");
    fs::write(&first_path, b"owned first").unwrap();
    fs::write(&second_path, b"owned second").unwrap();
    let first = RootAuthority::open(&selected, true, &context()).unwrap();
    let second = RootAuthority::open(&sibling, true, &context()).unwrap();
    assert_ne!(first.draft().anchor, second.draft().anchor);
    assert_eq!(
        selected.to_string_lossy().to_lowercase(),
        sibling.to_string_lossy().to_lowercase()
    );
    assert!(first.open_file(&second_path).is_err());
    assert!(first.open_directory(&sibling).is_err());
    assert!(second.open_file(&first_path).is_err());
    let mut own_file = first.open_file(&first_path).unwrap();
    let mut actual = String::new();
    own_file.read_to_string(&mut actual).unwrap();
    assert_eq!(actual, "owned first");
    let first_record = root(&selected, "root:unicode-first", true);
    let second_record = root(&sibling, "root:unicode-second", true);
    let registry = AuthorityRegistry::default();
    let authorization = registry
        .install(
            &intent(
                &second_path,
                vec![first_record.clone(), second_record.clone()],
                None,
            ),
            None,
        )
        .unwrap();
    let mut opened = authorization.open().unwrap();
    let mut bytes = [0; 5];
    opened.read_exact(&mut bytes).unwrap();
    assert_eq!(&bytes, b"owned");
    // The similarly spelled first root was never a proof for this opened file.
    registry.revoke_root(&second_record.root_id);
    assert!(opened.read(&mut bytes).is_err());
    assert!(authorization.open().is_err());
}
#[cfg(windows)]
#[test]
fn nested_junction_never_expands_a_root_grant_or_sidecar_access() {
    use std::os::windows::process::CommandExt;
    let selected = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
    let outside = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
    fs::write(outside.path().join("secret.wav"), [9]).unwrap();
    let link = selected.path().join("nested");
    let status = std::process::Command::new("cmd.exe")
        .args(["/D", "/C", "mklink", "/J"])
        .arg(&link)
        .arg(outside.path())
        .creation_flags(0x08000000)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .unwrap();
    assert!(status.success());
    let authority = RootAuthority::open(selected.path(), true, &context()).unwrap();
    assert!(authority.open_file(link.join("secret.wav")).is_err());
    assert!(authority.open_directory(&link).is_err());
    assert_eq!(fs::read(outside.path().join("secret.wav")).unwrap(), [9]);
}
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn openat_rejects_nested_links_and_retains_a_renamed_file_inside_its_physical_root() {
    use std::os::unix::fs::symlink;
    let selected = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
    let outside = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
    fs::write(outside.path().join("secret.wav"), [9]).unwrap();
    symlink(outside.path(), selected.path().join("nested")).unwrap();
    let authority = RootAuthority::open(selected.path(), true, &context()).unwrap();
    assert!(
        authority
            .open_file(selected.path().join("nested/secret.wav"))
            .is_err()
    );
    let path = selected.path().join("audio.wav");
    fs::write(&path, [1]).unwrap();
    let mut opened = authority.open_file(&path).unwrap();
    fs::rename(&path, selected.path().join("moved.wav")).unwrap();
    let mut byte = [0];
    opened.read_exact(&mut byte).unwrap();
    assert_eq!(byte, [1]);
}

#[test]
fn actual_active_file_relocation_keeps_read_cursor_and_never_reads_reused_old_path() {
    let temp = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
    let selected = temp.path().join("selected");
    fs::create_dir(&selected).unwrap();
    let old = selected.join("old.wav");
    let new = selected.join("new.wav");
    fs::write(&old, b"abcdefghijkl").unwrap();
    let registry = AuthorityRegistry::default();
    let selected_root = root(&selected, "root:movable", true);
    let original = intent(&old, vec![selected_root.clone()], None);
    let authorization = registry.install(&original, None).unwrap();
    let mut opened = authorization.open().unwrap();
    let identity = opened.identity().clone();
    registry.install(&original, Some(identity.clone())).unwrap();
    let mut bytes = [0; 3];
    opened.read_exact(&mut bytes).unwrap();
    assert_eq!(&bytes, b"abc");
    fs::rename(&old, &new).unwrap();
    fs::write(&old, b"UNAUTHORIZED").unwrap();
    opened.read_exact(&mut bytes).unwrap();
    assert_eq!(&bytes, b"def");
    assert!(authorization.open().is_err());
    let target_root = RootAuthority::restore(&selected_root, &context()).unwrap();
    let target = target_root.open_file(&new).unwrap();
    let mut rebound = original.clone();
    rebound.path = fs::canonicalize(&new).unwrap();
    #[cfg(windows)]
    let prepared = registry
        .prepare_asset_relocation(&rebound, &identity, &target)
        .unwrap();
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    let prepared = {
        use sha2::Digest;
        let original_sha = format!("{:x}", sha2::Sha256::digest(b"abcdefghijkl"));
        assert!(
            registry
                .prepare_asset_relocation(&rebound, &identity, &target)
                .is_err()
        );
        registry
            .prepare_asset_relocation_with_content(
                &rebound,
                &identity,
                &target,
                &original_sha,
                &original_sha,
            )
            .unwrap()
    };
    #[cfg(windows)]
    assert_eq!(
        prepared.evidence(),
        lx_gpui_player::domain::library_watch::RelinkEvidence::SameOpenObject
    );
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    assert_eq!(
        prepared.evidence(),
        lx_gpui_player::domain::library_watch::RelinkEvidence::SameFileAndContent
    );
    let current = registry
        .publish_asset_relocation(prepared, &rebound)
        .unwrap();
    opened.read_exact(&mut bytes).unwrap();
    assert_eq!(&bytes, b"ghi");
    assert_eq!(authorization.current_path(), rebound.path);
    current.open().unwrap().read_exact(&mut bytes).unwrap();
    assert_eq!(&bytes, b"abc");
    registry.revoke_root(&selected_root.root_id);
    assert!(opened.read(&mut bytes).is_err());
}

#[test]
fn exact_file_permission_does_not_follow_a_renamed_open_object() {
    let temp = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
    let old = temp.path().join("exact.wav");
    let new = temp.path().join("renamed.wav");
    fs::write(&old, b"exact grant").unwrap();
    let registry = AuthorityRegistry::default();
    let authorization = registry
        .install(&intent(&old, Vec::new(), Some(1)), None)
        .unwrap();
    let mut opened = authorization.open().unwrap();
    fs::rename(&old, &new).unwrap();
    assert!(opened.read(&mut [0]).is_err());
    fs::rename(&new, &old).unwrap();
    assert!(
        opened.read(&mut [0]).is_err(),
        "An expired opened proof must stay expired"
    );
    authorization.open().unwrap().read_exact(&mut [0]).unwrap();
}

#[test]
fn moving_an_active_file_out_of_its_root_permanently_stops_that_reader() {
    let temp = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
    let selected = temp.path().join("root");
    fs::create_dir(&selected).unwrap();
    let inside = selected.join("audio.wav");
    let outside = temp.path().join("outside.wav");
    fs::write(&inside, b"owned").unwrap();
    let registry = AuthorityRegistry::default();
    let authorization = registry
        .install(
            &intent(&inside, vec![root(&selected, "root:exit", true)], None),
            None,
        )
        .unwrap();
    let mut opened = authorization.open().unwrap();
    fs::rename(&inside, &outside).unwrap();
    assert!(opened.read(&mut [0]).is_err());
    fs::rename(&outside, &inside).unwrap();
    assert!(opened.read(&mut [0]).is_err());
    authorization.open().unwrap().read_exact(&mut [0]).unwrap();
}

#[test]
fn root_pause_denies_moved_reader_and_same_path_replacement() {
    let temp = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
    let selected = temp.path().join("selected");
    let moved = temp.path().join("moved");
    fs::create_dir(&selected).unwrap();
    let audio = selected.join("audio.wav");
    fs::write(&audio, b"original").unwrap();
    let selected_root = root(&selected, "root:pause", true);
    let registry = AuthorityRegistry::default();
    let authorization = registry
        .install(&intent(&audio, vec![selected_root.clone()], None), None)
        .unwrap();
    let mut opened = authorization.open().unwrap();
    fs::rename(&selected, &moved).unwrap();
    fs::create_dir(&selected).unwrap();
    fs::write(&audio, b"replacement").unwrap();
    assert!(opened.read(&mut [0]).is_err());
    assert!(registry.verify_root(&selected_root, &context()).is_err());
    assert_eq!(
        registry.root_health(&selected_root.root_id, 1),
        RootHealth::NeedsReapproval
    );
    assert!(authorization.open().is_err());
    assert!(
        registry
            .prepare_root_relocation(&selected_root, &moved)
            .is_err()
    );
    let selected_again = RootAuthority::open(&moved, true, &context()).unwrap();
    let prepared = registry
        .prepare_manual_root_relocation(&selected_root, selected_again.proof().unwrap())
        .unwrap();
    prepared.check().unwrap();
    let mut committed = selected_root.clone();
    committed.path = prepared.draft().path;
    registry
        .publish_root_relocation(prepared, &committed)
        .unwrap();
    assert_eq!(committed.grant_epoch, selected_root.grant_epoch);
    assert!(
        opened.read(&mut [0]).is_err(),
        "Manual reapproval must not revive the expired reader"
    );
    let mut bytes = [0; 8];
    authorization
        .open()
        .unwrap()
        .read_exact(&mut bytes)
        .unwrap();
    assert_eq!(&bytes, b"original");
}

#[test]
fn explicit_follow_same_root_object_preserves_readers_and_updates_all_shared_locators() {
    use lx_gpui_player::domain::library_watch::RootMovePolicy;
    let temp = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
    let selected = temp.path().join("selected");
    let moved = temp.path().join("moved");
    fs::create_dir(&selected).unwrap();
    let audio = selected.join("audio.wav");
    fs::write(&audio, b"abcdefghijkl").unwrap();
    let selected_root = root(&selected, "root:follow", true);
    let registry = AuthorityRegistry::default();
    let original = intent(&audio, vec![selected_root.clone()], None);
    let authorization = registry.install(&original, None).unwrap();
    registry
        .apply_root_move_policy(&selected_root.root_id, 1, RootMovePolicy::FollowSameObject)
        .unwrap();
    let mut opened = authorization.open().unwrap();
    let mut bytes = [0; 3];
    opened.read_exact(&mut bytes).unwrap();
    fs::rename(&selected, &moved).unwrap();
    fs::create_dir(&selected).unwrap();
    fs::write(&audio, b"replacement").unwrap();
    opened.read_exact(&mut bytes).unwrap();
    assert_eq!(&bytes, b"def");
    assert!(
        registry
            .prepare_root_relocation(&selected_root, &selected)
            .is_err()
    );
    authorization
        .open()
        .unwrap()
        .read_exact(&mut bytes)
        .unwrap();
    assert_eq!(
        &bytes, b"abc",
        "Follow reads must use the original root FH before locator publication"
    );
    let prepared = registry
        .prepare_root_relocation(&selected_root, &moved)
        .unwrap();
    let mut committed = selected_root.clone();
    committed.path =
        lx_gpui_player::domain::music::normalize_local_path(&fs::canonicalize(&moved).unwrap());
    committed.generation += 1;
    registry
        .publish_root_relocation(prepared, &committed)
        .unwrap();
    opened.read_exact(&mut bytes).unwrap();
    assert_eq!(&bytes, b"ghi");
    assert_eq!(
        authorization.current_path(),
        committed.path.join("audio.wav")
    );
    authorization
        .open()
        .unwrap()
        .read_exact(&mut bytes)
        .unwrap();
    assert_eq!(&bytes, b"abc");
    registry.revoke_root(&committed.root_id);
    assert!(opened.read(&mut bytes).is_err());
}

#[cfg(windows)]
#[test]
fn anchored_directory_enumeration_pins_its_namespace_only_for_the_iterator_lifetime() {
    let temp = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
    let selected = temp.path().join("selected");
    let moved = temp.path().join("moved");
    fs::create_dir(&selected).unwrap();
    fs::write(selected.join("owned.wav"), b"owned").unwrap();
    let authority = RootAuthority::open(&selected, true, &context()).unwrap();
    let directory = authority.open_directory(&selected).unwrap();
    let entries = directory.read_dir().unwrap();
    assert_eq!(
        fs::rename(&selected, &moved).unwrap_err().raw_os_error(),
        Some(32)
    );
    drop(entries);
    fs::rename(&selected, &moved).unwrap();
    fs::create_dir(&selected).unwrap();
    fs::write(selected.join("unselected.wav"), b"unselected").unwrap();
    assert!(directory.read_dir().is_err());
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn linux_automatic_same_inode_relocation_rejects_weak_stamp_and_forged_mtime_content() {
    use sha2::Digest;
    let temp = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
    let old = temp.path().join("old.wav");
    let new = temp.path().join("new.wav");
    fs::write(&old, b"abcdefghijkl").unwrap();
    let original_modified = fs::metadata(&old).unwrap().modified().unwrap();
    let original_sha = format!("{:x}", sha2::Sha256::digest(b"abcdefghijkl"));
    let selected = root(temp.path(), "root:linux-content", true);
    let registry = AuthorityRegistry::default();
    let original = intent(&old, vec![selected.clone()], None);
    let authorization = registry.install(&original, None).unwrap();
    let opened = authorization.open().unwrap();
    let identity = opened.identity().clone();
    registry.install(&original, Some(identity.clone())).unwrap();
    fs::rename(&old, &new).unwrap();
    let target_root = RootAuthority::restore(&selected, &context()).unwrap();
    let target = target_root.open_file(&new).unwrap();
    let mut rebound = original;
    rebound.path = fs::canonicalize(&new).unwrap();
    assert!(
        registry
            .prepare_asset_relocation(&rebound, &identity, &target)
            .is_err()
    );
    let prepared = registry
        .prepare_asset_relocation_with_content(
            &rebound,
            &identity,
            &target,
            &original_sha,
            &original_sha,
        )
        .unwrap();
    assert_eq!(
        prepared.evidence(),
        lx_gpui_player::domain::library_watch::RelinkEvidence::SameFileAndContent
    );
    prepared.check().unwrap();
    std::thread::sleep(Duration::from_millis(2));
    fs::write(&new, b"mnopqrstuvwx").unwrap();
    fs::OpenOptions::new()
        .write(true)
        .open(&new)
        .unwrap()
        .set_times(fs::FileTimes::new().set_modified(original_modified))
        .unwrap();
    assert_eq!(
        fs::metadata(&new).unwrap().modified().unwrap(),
        original_modified
    );
    assert!(
        prepared.check().is_err(),
        "Kernel ctime must reject mutation after the hash even with restored mtime"
    );
    assert!(
        registry
            .prepare_asset_relocation_with_content(
                &rebound,
                &identity,
                &target,
                &original_sha,
                &original_sha
            )
            .is_err(),
        "Actual file bytes must match the trusted old digest"
    );
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn linux_non_regular_replacement_is_rejected_without_waiting_for_a_fifo_writer() {
    use std::os::unix::ffi::OsStrExt;
    let temp = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
    let path = temp.path().join("audio.wav");
    let name = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
    let authority = RootAuthority::open(temp.path(), true, &context()).unwrap();
    let started = std::time::Instant::now();
    assert!(authority.open_file(&path).is_err());
    assert!(started.elapsed() < Duration::from_secs(1));
}

#[test]
fn live_relocation_hints_use_actual_original_handles_and_mark_hardlinks_for_selection() {
    let temp = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
    let old = temp.path().join("old.wav");
    let new = temp.path().join("new.wav");
    fs::write(&old, b"original object").unwrap();
    let selected = root(temp.path(), "root:hints", true);
    let registry = AuthorityRegistry::default();
    let original = intent(&old, vec![selected.clone()], None);
    let authorization = registry.install(&original, None).unwrap();
    let opened = authorization.open().unwrap();
    let identity = opened.identity().clone();
    registry.install(&original, Some(identity.clone())).unwrap();
    assert!(
        registry
            .live_relocation_hints(&selected.root_id, 1, 200)
            .unwrap()
            .is_empty()
    );
    fs::rename(&old, &new).unwrap();
    fs::write(&old, b"different object at old name").unwrap();
    let hints = registry
        .live_relocation_hints(&selected.root_id, 1, 200)
        .unwrap();
    assert_eq!(hints.len(), 1);
    assert_eq!(hints[0].asset_id, original.asset_id);
    assert_eq!(hints[0].old_path, original.path);
    assert_eq!(
        hints[0].current_path,
        lx_gpui_player::domain::music::normalize_local_path(&new)
    );
    assert_eq!(hints[0].file_identity, identity);
    assert!(!hints[0].requires_selection);
    assert!(
        registry
            .live_relocation_hints(&selected.root_id, 2, 200)
            .is_err()
    );
    assert!(
        registry
            .live_relocation_hints(&selected.root_id, 1, 201)
            .is_err()
    );
    assert!(
        registry
            .live_relocation_hints(&selected.root_id, 1, 0)
            .unwrap()
            .is_empty()
    );
    fs::hard_link(&new, temp.path().join("alias.wav")).unwrap();
    let hints = registry
        .live_relocation_hints(&selected.root_id, 1, 200)
        .unwrap();
    assert_eq!(hints.len(), 1);
    assert!(hints[0].requires_selection);
    drop(opened);
    assert!(
        registry
            .live_relocation_hints(&selected.root_id, 1, 200)
            .unwrap()
            .is_empty(),
        "Expected identity must not become a live original handle"
    );
}

#[test]
fn verified_native_binding_recovers_a_cached_file_without_a_live_original_data_handle() {
    use lx_gpui_player::domain::library_watch::{LibraryAssetBinding, RelinkEvidence};
    use sha2::Digest;
    let temp = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
    let old = temp.path().join("old.wav");
    let new = temp.path().join("new.wav");
    fs::write(&old, b"verified original body").unwrap();
    let selected = root(temp.path(), "root:historical", true);
    let registry = AuthorityRegistry::default();
    let original = intent(&old, vec![selected.clone()], None);
    let authorization = registry.install(&original, None).unwrap();
    let opened = authorization.open().unwrap();
    let identity = opened.identity().clone();
    let metadata = opened.metadata().unwrap();
    registry.install(&original, Some(identity.clone())).unwrap();
    let old_binding = LibraryAssetBinding {
        asset_id: original.asset_id.clone(),
        path: original.path.clone(),
        binding_revision: 7,
        file_identity: Some(identity.clone()),
        content_sha256: Some(format!(
            "{:x}",
            sha2::Sha256::digest(b"verified original body")
        )),
        hash_size_bytes: Some(metadata.len()),
        hash_modified_ms: metadata
            .modified()
            .ok()
            .and_then(|stamp| stamp.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|stamp| stamp.as_millis() as u64),
    };
    drop(opened);
    fs::rename(&old, &new).unwrap();
    fs::write(&old, b"new body at reused old path").unwrap();
    assert!(
        registry
            .live_relocation_hints(&selected.root_id, 1, 200)
            .unwrap()
            .is_empty()
    );
    let selected_authority = RootAuthority::restore(&selected, &context()).unwrap();
    let target = selected_authority.open_file(&new).unwrap();
    let mut rebound = original.clone();
    rebound.path = lx_gpui_player::domain::music::normalize_local_path(&new);
    let prepared = registry
        .prepare_asset_relocation_from_binding(&old_binding, &rebound, &target)
        .unwrap();
    assert_eq!(prepared.evidence(), RelinkEvidence::SameFileAndContent);
    assert_eq!(prepared.binding_revision(), Some(7));
    prepared.check().unwrap();
    let installed = registry
        .publish_asset_relocation(prepared, &rebound)
        .unwrap();
    let mut bytes = Vec::new();
    installed.open().unwrap().read_to_end(&mut bytes).unwrap();
    assert_eq!(bytes, b"verified original body");
    assert_eq!(authorization.current_path(), rebound.path);
    assert_eq!(fs::read(&old).unwrap(), b"new body at reused old path");
}

#[test]
fn historical_content_admission_rejects_missing_hash_wrong_body_and_original_same_object_alias() {
    use lx_gpui_player::domain::library_watch::LibraryAssetBinding;
    use sha2::Digest;
    let temp = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
    let old = temp.path().join("old.wav");
    let new = temp.path().join("new.wav");
    fs::write(&old, b"abcdefghijkl").unwrap();
    let selected = root(temp.path(), "root:historical-deny", true);
    let registry = AuthorityRegistry::default();
    let original = intent(&old, vec![selected.clone()], None);
    let opened = registry.install(&original, None).unwrap().open().unwrap();
    let identity = opened.identity().clone();
    let modified = opened.metadata().unwrap().modified().unwrap();
    registry.install(&original, Some(identity.clone())).unwrap();
    let mut binding = LibraryAssetBinding {
        asset_id: original.asset_id.clone(),
        path: original.path.clone(),
        binding_revision: 1,
        file_identity: Some(identity),
        content_sha256: Some(format!("{:x}", sha2::Sha256::digest(b"abcdefghijkl"))),
        hash_size_bytes: Some(12),
        hash_modified_ms: None,
    };
    drop(opened);
    fs::rename(&old, &new).unwrap();
    let selected_authority = RootAuthority::restore(&selected, &context()).unwrap();
    let target = selected_authority.open_file(&new).unwrap();
    let mut rebound = original;
    rebound.path = lx_gpui_player::domain::music::normalize_local_path(&new);
    let sha = binding.content_sha256.take();
    assert!(
        registry
            .prepare_asset_relocation_from_binding(&binding, &rebound, &target)
            .is_err()
    );
    binding.content_sha256 = sha;
    fs::hard_link(&new, &old).unwrap();
    assert!(
        registry
            .prepare_asset_relocation_from_binding(&binding, &rebound, &target)
            .is_err()
    );
    fs::remove_file(&old).unwrap();
    drop(target);
    fs::write(&new, b"mnopqrstuvwx").unwrap();
    fs::OpenOptions::new()
        .write(true)
        .open(&new)
        .unwrap()
        .set_times(fs::FileTimes::new().set_modified(modified))
        .unwrap();
    let target = selected_authority.open_file(&new).unwrap();
    assert!(
        registry
            .prepare_asset_relocation_from_binding(&binding, &rebound, &target)
            .is_err(),
        "A matching actual file ID, size and restored mtime must not replace whole-content proof"
    );
}

#[cfg(windows)]
#[test]
fn approved_hardlink_alias_reads_its_exact_namespace_even_when_primary_is_outside_root() {
    let temp = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
    let selected = temp.path().join("selected");
    fs::create_dir(&selected).unwrap();
    let primary = temp.path().join("outside-primary.wav");
    let alias = selected.join("approved-alias.wav");
    fs::write(&primary, b"same physical object").unwrap();
    fs::hard_link(&primary, &alias).unwrap();
    let selected_root = root(&selected, "root:chosen-alias", true);
    let authority = RootAuthority::restore(&selected_root, &context()).unwrap();
    let mut opened = authority.open_file(&alias).unwrap();
    let mut bytes = Vec::new();
    opened.read_to_end(&mut bytes).unwrap();
    assert_eq!(bytes, b"same physical object");
    assert_eq!(
        opened.current_path().unwrap(),
        lx_gpui_player::domain::music::normalize_local_path(&alias)
    );
    assert!(authority.open_file(&primary).is_err());
    let registry = AuthorityRegistry::default();
    let exact = registry
        .install(
            &intent(&alias, Vec::new(), Some(1)),
            Some(opened.identity().clone()),
        )
        .unwrap();
    let mut exact_reader = exact.open().unwrap();
    exact_reader.read_exact(&mut [0; 1]).unwrap();
    let renamed = selected.join("renamed-alias.wav");
    fs::rename(&alias, &renamed).unwrap();
    assert!(
        exact_reader.read(&mut [0]).is_err(),
        "A primary hardlink must not make an exact alias grant follow rename"
    );
    assert!(
        opened.read(&mut [0]).is_err(),
        "Unknown outside primary must not become a root read locator after the approved alias disappears"
    );
}

#[cfg(windows)]
#[test]
fn expired_reader_cache_permission_uses_actual_identity_scope_without_inventing_live_evidence() {
    let temp = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
    let selected = temp.path().join("selected");
    fs::create_dir(&selected).unwrap();
    let old = selected.join("old.wav");
    let new = selected.join("new.wav");
    let outside = temp.path().join("outside.wav");
    fs::write(&old, b"cached body").unwrap();
    let selected_root = root(&selected, "root:cached-scope", true);
    let registry = AuthorityRegistry::default();
    let authorization = registry
        .install(&intent(&old, vec![selected_root.clone()], None), None)
        .unwrap();
    let opened = authorization.open().unwrap();
    drop(opened);
    fs::rename(&old, &new).unwrap();
    assert!(
        authorization.check().is_ok(),
        "Actual same object remains inside the approved physical root"
    );
    assert!(
        registry
            .live_relocation_hints(&selected_root.root_id, 1, 200)
            .unwrap()
            .is_empty(),
        "A metadata-by-ID scope lookup is not an original live data FD"
    );
    fs::rename(&new, &outside).unwrap();
    assert!(
        authorization.check().is_err(),
        "Cached PCM cannot outlive the object's actual root scope"
    );
    fs::rename(&outside, &new).unwrap();
    fs::rename(&selected, temp.path().join("root-moved")).unwrap();
    assert!(
        authorization.check().is_err(),
        "Root Pause is enforced even with no live data reader"
    );
}

#[test]
fn same_verified_content_manual_copy_preserves_only_a_legitimate_root_backed_reader() {
    use sha2::Digest;
    let temp = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
    let selected = temp.path().join("selected");
    fs::create_dir(&selected).unwrap();
    let original_path = selected.join("original.wav");
    let copy = temp.path().join("selected-copy.wav");
    fs::write(&original_path, b"abcdefghijkl").unwrap();
    fs::copy(&original_path, &copy).unwrap();
    let selected_root = root(&selected, "root:manual-continuity", true);
    let registry = AuthorityRegistry::default();
    let original = intent(&original_path, vec![selected_root.clone()], Some(1));
    let authorization = registry.install(&original, None).unwrap();
    let mut reader = authorization.open().unwrap();
    let old_identity = reader.identity().clone();
    registry
        .install(&original, Some(old_identity.clone()))
        .unwrap();
    reader.read_exact(&mut [0; 3]).unwrap();
    let target = registry.open_selection(&copy).unwrap();
    assert_ne!(target.identity(), &old_identity);
    let mut committed = original.clone();
    committed.path = lx_gpui_player::domain::music::normalize_local_path(&copy);
    committed.explicit_file_epoch = Some(2);
    committed.roots.clear();
    let sha = format!("{:x}", sha2::Sha256::digest(b"abcdefghijkl"));
    let prepared = registry
        .prepare_replacement_same_content(&committed, Some(target.identity().clone()), &sha, &sha)
        .unwrap();
    let new_authorization = registry
        .publish_replacement_same_content(prepared, &committed)
        .unwrap();
    let mut bytes = [0; 3];
    reader.read_exact(&mut bytes).unwrap();
    assert_eq!(&bytes, b"def");
    assert_eq!(reader.identity(), &old_identity);
    assert_eq!(
        authorization.current_path(),
        original.path,
        "Old actual reader must not retarget its physical namespace"
    );
    let new_reader = new_authorization.open().unwrap();
    assert_eq!(new_reader.identity(), target.identity());
    registry.revoke_root(&selected_root.root_id);
    assert!(reader.read(&mut bytes).is_err());
    assert!(
        new_authorization.open().is_ok(),
        "The new explicit selection remains independent"
    );

    let exact_registry = AuthorityRegistry::default();
    let exact_original = intent(&original_path, Vec::new(), Some(1));
    let exact_authorization = exact_registry
        .install(&exact_original, Some(old_identity))
        .unwrap();
    let mut exact_reader = exact_authorization.open().unwrap();
    let prepared = exact_registry
        .prepare_replacement_same_content(&committed, Some(target.identity().clone()), &sha, &sha)
        .unwrap();
    exact_registry
        .publish_replacement_same_content(prepared, &committed)
        .unwrap();
    assert!(
        exact_reader.read(&mut [0]).is_err(),
        "Matching SHA must not renew an old exact-only physical reader"
    );
    assert!(exact_authorization.open().is_err());
}

#[test]
fn actual_change_stamp_detects_same_size_rewrite_with_restored_mtime_and_checks_cancellation() {
    let temp = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
    let path = temp.path().join("same-size.wav");
    fs::write(&path, b"abcdef").unwrap();
    let registry = AuthorityRegistry::default();
    let authorization = registry
        .install(
            &intent(
                &path,
                vec![root(temp.path(), "root:change-stamp", true)],
                None,
            ),
            None,
        )
        .unwrap();
    let opened = authorization.open().unwrap();
    let before = opened.change_stamp().unwrap();
    let identity = opened.identity().clone();
    let modified = opened.metadata().unwrap().modified().unwrap();
    drop(opened);
    std::thread::sleep(Duration::from_millis(10));
    fs::write(&path, b"ghijkl").unwrap();
    fs::OpenOptions::new()
        .write(true)
        .open(&path)
        .unwrap()
        .set_times(fs::FileTimes::new().set_modified(modified))
        .unwrap();
    let cancel = Arc::new(AtomicBool::new(false));
    let opened = authorization
        .with_cancel(cancel.clone())
        .unwrap()
        .open()
        .unwrap();
    assert_eq!(opened.identity(), &identity);
    assert_eq!(opened.metadata().unwrap().modified().unwrap(), modified);
    assert_eq!(opened.metadata().unwrap().len(), 6);
    assert_ne!(
        opened.change_stamp().unwrap(),
        before,
        "Kernel change time must invalidate cached tags despite forged unchanged mtime/length"
    );
    cancel.store(true, Ordering::Release);
    assert!(opened.change_stamp().is_err());
}

#[test]
fn actual_metadata_snapshot_returns_plain_current_fields_and_rejects_cancelled_or_escaped_scope() {
    let temp = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
    let selected = temp.path().join("selected-snapshot-root");
    let outside = temp.path().join("outside-snapshot-root");
    fs::create_dir(&selected).unwrap();
    fs::create_dir(&outside).unwrap();
    let path = selected.join("snapshot.wav");
    fs::write(&path, b"original body").unwrap();
    let selected_root = root(&selected, "root:metadata-snapshot", true);
    let registry = AuthorityRegistry::default();
    let authorization = registry
        .install(&intent(&path, vec![selected_root.clone()], None), None)
        .unwrap();
    let cancel = Arc::new(AtomicBool::new(false));
    let opened = authorization
        .with_cancel(cancel.clone())
        .unwrap()
        .open()
        .unwrap();
    let snapshot = opened.metadata_snapshot().unwrap();
    assert_eq!(snapshot.metadata.len(), 13);
    assert_eq!(
        lx_gpui_player::domain::music::normalize_local_path(&snapshot.path),
        lx_gpui_player::domain::music::normalize_local_path(&path)
    );
    assert_eq!(snapshot.change_stamp, opened.change_stamp().unwrap());
    cancel.store(true, Ordering::Release);
    assert!(opened.metadata_snapshot().is_err());
    cancel.store(false, Ordering::Release);
    drop(opened);
    let escaped = authorization.open().unwrap();
    fs::rename(&path, outside.join("escaped.wav")).unwrap();
    assert!(escaped.metadata_snapshot().is_err());
    fs::rename(outside.join("escaped.wav"), &path).unwrap();
    assert!(
        escaped.metadata_snapshot().is_err(),
        "A physically failed old opened scope must not revive"
    );
    drop(escaped);
    let current = authorization.open().unwrap();
    registry.revoke_root(&selected_root.root_id);
    assert!(current.metadata_snapshot().is_err());
    drop(current);
    registry.begin_shutdown();
    assert!(registry.retirement_barrier().wait(Duration::from_secs(3)));
    // Retaining the returned DTO never retains a namespace/file proof.
    assert!(fs::OpenOptions::new().write(true).open(&path).is_ok());
    assert_eq!(snapshot.metadata.len(), 13);
}

#![cfg(any(windows, target_os = "linux", target_os = "macos"))]

use lx_gpui_player::{
    backup::{
        BackupEncoding, NativeBackup, NativeBackupOptions,
        csv::{CsvMapping, decode_csv, export_music_csv},
        decode_lx, decode_native, encode_lx, encode_native,
        file_export::{
            ExportContext, ExportDirectory, ExportErrorKind, ExportFileIdentity, ExportLimiter,
            MAX_EXPORT_BYTES, export_bytes, export_with,
        },
    },
    domain::{
        lists::ListData,
        music::{LxMusicDtoVersion, TrackRecord},
    },
    store::StoreData,
};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{Read, Write},
    time::Duration,
};

fn context() -> ExportContext {
    ExportContext::new(Duration::from_secs(120)).unwrap()
}
fn selected() -> (tempfile::TempDir, ExportDirectory, ExportLimiter) {
    let temp = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
    let directory = ExportDirectory::open_selected(temp.path(), 7).unwrap();
    (temp, directory, ExportLimiter::default())
}
fn physical_identity(path: &std::path::Path) -> ExportFileIdentity {
    #[cfg(windows)]
    {
        use std::os::windows::{fs::OpenOptionsExt, io::AsRawHandle};
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_ID_INFO, FileIdInfo,
            GetFileInformationByHandleEx,
        };
        let file = fs::OpenOptions::new()
            .read(true)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .open(path)
            .unwrap();
        let mut info = FILE_ID_INFO::default();
        assert_ne!(
            unsafe {
                GetFileInformationByHandleEx(
                    file.as_raw_handle(),
                    FileIdInfo,
                    &mut info as *mut _ as *mut _,
                    std::mem::size_of_val(&info) as u32,
                )
            },
            0
        );
        ExportFileIdentity {
            device: info.VolumeSerialNumber,
            object: info
                .FileId
                .Identifier
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect(),
        }
    }
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        use std::os::unix::fs::MetadataExt;
        let file = fs::File::open(path).unwrap();
        let metadata = file.metadata().unwrap();
        ExportFileIdentity {
            device: metadata.dev(),
            object: metadata.ino().to_string(),
        }
    }
}
fn assert_no_private_files(path: &std::path::Path) {
    assert!(!fs::read_dir(path).unwrap().any(|entry| {
        entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".lx-export-")
    }));
}
fn music() -> TrackRecord {
    TrackRecord::from_lx_dto(
        json!({"id":"kw_17","source":"kw","name":"星河,\"夜\"\n序曲",
        "singer":"甲、乙","interval":"03:02","meta":{"songId":17,"albumName":"秋日"}}),
        LxMusicDtoVersion::Modern,
    )
    .unwrap()
}

#[test]
fn real_files_roundtrip_all_four_lx_shapes_json_and_gzip_with_actual_receipts() {
    let (temp, directory, limiter) = selected();
    let fixtures = [
        json!({"type":"allData_v2","setting":{"version":"2.1.0","player.volume":0.7},
            "playList":[{"id":"default","name":"默认","list":[]}],"opaque":"保留"}),
        json!({"type":"setting_v2","data":{"version":"2.1.0","player.volume":0.7,"unknown":null}}),
        json!({"type":"playList_v2","data":[{"id":"custom","name":"自建","list":[]}]}),
        json!({"type":"playListPart_v2","data":{"id":"custom","name":"自建","list":[]}}),
    ];
    for (index, fixture) in fixtures.iter().enumerate() {
        let parsed =
            decode_lx(&serde_json::to_vec(fixture).unwrap(), BackupEncoding::Json).unwrap();
        for (extension, encoding) in [
            ("json", BackupEncoding::Json),
            ("lxmc", BackupEncoding::Gzip),
        ] {
            let bytes = encode_lx(&parsed, encoding).unwrap();
            let basename = format!("备份-{index}.{extension}");
            let receipt = export_bytes(
                directory.plan(&basename).unwrap(),
                &format!("lx-{index}-{extension}"),
                &bytes,
                &context(),
                &limiter,
            )
            .unwrap();
            assert_eq!(receipt.path, directory.path().join(&basename));
            assert_eq!(receipt.identity, physical_identity(&receipt.path));
            assert_eq!(receipt.directory_identity, physical_identity(temp.path()));
            assert_eq!(receipt.selection_epoch, 7);
            assert_eq!(receipt.bytes, bytes.len() as u64);
            assert_eq!(receipt.sha256, format!("{:x}", Sha256::digest(&bytes)));
            assert_eq!(fs::read(&receipt.path).unwrap(), bytes);
            assert_eq!(
                decode_lx(&fs::read(&receipt.path).unwrap(), BackupEncoding::Auto)
                    .unwrap()
                    .to_value()
                    .unwrap(),
                *fixture
            );
        }
    }
    assert_no_private_files(temp.path());
    assert_eq!(limiter.stats().active, 0);
    assert_eq!(limiter.stats().reserved_bytes, 0);
}

#[test]
fn csv_quotes_unicode_and_native_logical_backup_are_published_without_grants() {
    let (temp, directory, limiter) = selected();
    let tracks = vec![music()];
    let csv = export_music_csv(&tracks, ["歌名".into(), "歌手".into(), "专辑".into()]).unwrap();
    let receipt = export_bytes(
        directory.plan("歌单.csv").unwrap(),
        "csv",
        &csv,
        &context(),
        &limiter,
    )
    .unwrap();
    let table = decode_csv(&fs::read(&receipt.path).unwrap()).unwrap();
    let mapped = table
        .map_music(&CsvMapping {
            name: 0,
            singer: Some(1),
            album: Some(2),
        })
        .unwrap();
    assert_eq!(mapped[0].name, tracks[0].title);
    assert_eq!(mapped[0].singer, "甲、乙");
    let store = StoreData {
        local_files: vec!["C:/unselected/file.flac".into()],
        ..StoreData::default()
    };
    let lists = ListData {
        default_list: tracks,
        ..ListData::default()
    };
    let native = NativeBackup::from_snapshot(
        &store,
        &lists,
        "曲目@歌手",
        &[],
        &[],
        NativeBackupOptions::default(),
    )
    .unwrap();
    let bytes = encode_native(&native, BackupEncoding::Json).unwrap();
    let receipt = export_bytes(
        directory.plan("native-v1.json").unwrap(),
        "native",
        &bytes,
        &context(),
        &limiter,
    )
    .unwrap();
    let restored = decode_native(&fs::read(&receipt.path).unwrap(), BackupEncoding::Auto).unwrap();
    assert!(restored.store.local_files.is_empty());
    assert!(restored.store.sources.is_empty() && restored.store.selected.is_none());
    assert_eq!(restored.declared_local_paths, ["C:/unselected/file.flac"]);
    assert_eq!(restored.lists, lists);
    assert_eq!(receipt.identity, physical_identity(&receipt.path));
    assert_no_private_files(temp.path());
}

#[test]
fn existing_target_and_target_created_during_production_are_never_overwritten() {
    let (temp, directory, limiter) = selected();
    for race in [false, true] {
        let basename = if race {
            "appeared.json"
        } else {
            "existing.json"
        };
        let target = temp.path().join(basename);
        if !race {
            fs::write(&target, b"original user file").unwrap();
        }
        let mut original_identity = None;
        let failure = export_with(
            directory.plan(basename).unwrap(),
            "conflict",
            12,
            &context(),
            &limiter,
            |writer| {
                writer.write_all(b"new snapshot")?;
                if race {
                    fs::write(&target, b"original user file")?;
                }
                original_identity = Some(physical_identity(&target));
                Ok(())
            },
        )
        .unwrap_err();
        assert_eq!(failure.kind, ExportErrorKind::Conflict);
        let part = failure.part.unwrap();
        assert!(!part.metadata().published);
        assert_ne!(
            part.metadata().identity.as_ref(),
            original_identity.as_ref()
        );
        assert_eq!(fs::read(&target).unwrap(), b"original user file");
        part.cleanup(&context()).unwrap();
        assert_eq!(physical_identity(&target), original_identity.unwrap());
        assert_eq!(fs::read(&target).unwrap(), b"original user file");
    }
    assert_no_private_files(temp.path());
    assert_eq!(limiter.stats().active, 0);
}

#[test]
fn cancelled_partial_write_retains_owner_and_fresh_context_cleans_only_that_object() {
    let (temp, directory, limiter) = selected();
    let control = context();
    let failure = export_with(
        directory.plan("cancelled.json").unwrap(),
        "cancelled",
        128 * 1024,
        &control,
        &limiter,
        |writer| {
            writer.write_all(&[3; 64 * 1024])?;
            control.cancel();
            writer.write_all(&[4; 64 * 1024])?;
            Ok(())
        },
    )
    .unwrap_err();
    assert_eq!(failure.kind, ExportErrorKind::Cancelled);
    let part = failure.part.unwrap();
    assert_eq!(part.metadata().bytes, 64 * 1024);
    assert!(!temp.path().join("cancelled.json").exists());
    assert_eq!(limiter.stats().active, 1);
    assert_eq!(limiter.stats().reserved_bytes, 128 * 1024);
    let failure = part.cleanup(&control).unwrap_err();
    assert_eq!(failure.kind, ExportErrorKind::Cancelled);
    failure.part.unwrap().cleanup(&context()).unwrap();
    assert_no_private_files(temp.path());
    assert_eq!(limiter.stats().active, 0);
    assert_eq!(limiter.stats().reserved_bytes, 0);
}

#[test]
fn producer_failure_and_panic_preserve_recoverable_parts_and_never_publish() {
    let (temp, directory, limiter) = selected();
    for panic in [false, true] {
        let basename = if panic { "panic.json" } else { "failed.json" };
        let failure = export_with(
            directory.plan(basename).unwrap(),
            "producer",
            12,
            &context(),
            &limiter,
            |writer| {
                writer.write_all(b"owned bytes")?;
                if panic {
                    panic!("injected producer unwind");
                }
                anyhow::bail!("injected producer failure")
            },
        )
        .unwrap_err();
        assert_eq!(failure.kind, ExportErrorKind::Producer);
        assert!(!temp.path().join(basename).exists());
        assert_eq!(failure.part.as_ref().unwrap().metadata().bytes, 11);
        failure.part.unwrap().cleanup(&context()).unwrap();
    }
    assert_no_private_files(temp.path());
}

#[test]
fn active_part_and_aggregate_reservations_are_bounded_and_released_after_cleanup() {
    let (temp, directory, limiter) = selected();
    let mut parts = Vec::new();
    for i in 0..4 {
        let failure = export_with(
            directory.plan(&format!("pending-{i}.json")).unwrap(),
            "pending",
            0,
            &context(),
            &limiter,
            |_| anyhow::bail!("hold private object"),
        )
        .unwrap_err();
        parts.push(failure.part.unwrap());
    }
    let rejected = export_bytes(
        directory.plan("fifth.json").unwrap(),
        "fifth",
        &[],
        &context(),
        &limiter,
    )
    .unwrap_err();
    assert_eq!(rejected.kind, ExportErrorKind::Quota);
    assert!(rejected.part.is_none());
    assert_eq!(limiter.stats().active, 4);
    for part in parts.drain(..) {
        part.cleanup(&context()).unwrap();
    }
    for i in 0..2 {
        let failure = export_with(
            directory.plan(&format!("large-{i}.json")).unwrap(),
            "reserved",
            MAX_EXPORT_BYTES / 2,
            &context(),
            &limiter,
            |_| anyhow::bail!("hold reservation"),
        )
        .unwrap_err();
        parts.push(failure.part.unwrap());
    }
    assert_eq!(limiter.stats().active, 2);
    assert_eq!(limiter.stats().reserved_bytes, MAX_EXPORT_BYTES);
    let rejected = export_bytes(
        directory.plan("one-byte.json").unwrap(),
        "over-budget",
        b"x",
        &context(),
        &limiter,
    )
    .unwrap_err();
    assert_eq!(rejected.kind, ExportErrorKind::Quota);
    assert!(rejected.part.is_none());
    for part in parts {
        part.cleanup(&context()).unwrap();
    }
    assert_no_private_files(temp.path());
    assert_eq!(limiter.stats().reserved_bytes, 0);
}

#[test]
fn streaming_full_bound_is_verified_and_undeclared_extra_bytes_are_rejected() {
    let (temp, directory, limiter) = selected();
    let block = [0x57u8; 64 * 1024];
    let receipt = export_with(
        directory.plan("bounded.bin").unwrap(),
        "full-bound",
        MAX_EXPORT_BYTES,
        &context(),
        &limiter,
        |writer| {
            for _ in 0..256 {
                writer.write_all(&block)?;
            }
            Ok(())
        },
    )
    .unwrap();
    assert_eq!(receipt.bytes, MAX_EXPORT_BYTES);
    let mut actual = fs::File::open(&receipt.path).unwrap();
    assert_eq!(actual.metadata().unwrap().len(), MAX_EXPORT_BYTES);
    let mut digest = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = actual.read(&mut buffer).unwrap();
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    assert_eq!(receipt.sha256, format!("{:x}", digest.finalize()));
    let failure = export_with(
        directory.plan("too-much.json").unwrap(),
        "undeclared",
        64 * 1024,
        &context(),
        &limiter,
        |writer| {
            writer.write_all(&block)?;
            writer.write_all(b"x")?;
            Ok(())
        },
    )
    .unwrap_err();
    assert_eq!(failure.kind, ExportErrorKind::Quota);
    assert_eq!(failure.part.as_ref().unwrap().metadata().bytes, 64 * 1024);
    failure.part.unwrap().cleanup(&context()).unwrap();
    let failure = export_with(
        directory.plan("over-limit.json").unwrap(),
        "over-limit",
        MAX_EXPORT_BYTES + 1,
        &context(),
        &limiter,
        |_| Ok(()),
    )
    .unwrap_err();
    assert_eq!(failure.kind, ExportErrorKind::Quota);
    assert!(failure.part.is_none());
    assert_no_private_files(temp.path());
}

#[test]
fn filename_escape_expired_request_and_revoked_grant_cannot_create_files() {
    let (temp, directory, limiter) = selected();
    for bad in [
        "../outside.json",
        "sub/file.json",
        "\\outside.json",
        "x:stream",
        "CON.json",
        "trailing.",
        "",
        ".",
        "..",
    ] {
        assert!(directory.plan(bad).is_err(), "{bad:?}");
    }
    let expired = ExportContext::new(Duration::from_nanos(1)).unwrap();
    std::thread::sleep(Duration::from_millis(1));
    let failure = export_bytes(
        directory.plan("expired.json").unwrap(),
        "expired",
        b"{}",
        &expired,
        &limiter,
    )
    .unwrap_err();
    assert_eq!(failure.kind, ExportErrorKind::Deadline);
    assert!(failure.part.is_none());
    let plan = directory.plan("revoked.json").unwrap();
    directory.revoke();
    assert!(export_bytes(plan, "revoked", b"{}", &context(), &limiter).is_err());
    assert_eq!(fs::read_dir(temp.path()).unwrap().count(), 0);
    assert_eq!(limiter.stats().active, 0);
}

#[test]
fn ignored_writer_quota_error_cannot_publish_a_truncated_snapshot() {
    let (temp, directory, limiter) = selected();
    let failure = export_with(
        directory.plan("ignored-error.json").unwrap(),
        "ignored-error",
        2,
        &context(),
        &limiter,
        |writer| {
            writer.write_all(b"{}")?;
            assert!(writer.write_all(b"extra bytes").is_err());
            Ok(())
        },
    )
    .unwrap_err();
    assert_eq!(failure.kind, ExportErrorKind::Quota);
    assert!(!temp.path().join("ignored-error.json").exists());
    assert_eq!(failure.part.as_ref().unwrap().metadata().bytes, 2);
    failure.part.unwrap().cleanup(&context()).unwrap();
    assert_no_private_files(temp.path());
}

#[test]
fn revoked_partial_owner_requires_fresh_same_object_selection_for_cleanup() {
    let (temp, directory, limiter) = selected();
    let failure = export_with(
        directory.plan("revoked-part.json").unwrap(),
        "revoke-part",
        12,
        &context(),
        &limiter,
        |writer| {
            writer.write_all(b"owned bytes")?;
            directory.revoke();
            Ok(())
        },
    )
    .unwrap_err();
    assert_eq!(failure.kind, ExportErrorKind::Changed);
    assert!(!temp.path().join("revoked-part.json").exists());
    let metadata = failure.part.as_ref().unwrap().metadata();
    let failure = failure.part.unwrap().cleanup(&context()).unwrap_err();
    assert_eq!(failure.kind, ExportErrorKind::Changed);
    let unrelated = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
    let wrong = ExportDirectory::open_selected(unrelated.path(), 8).unwrap();
    let failure = failure
        .part
        .unwrap()
        .cleanup_in(&wrong, &context())
        .unwrap_err();
    assert_eq!(failure.kind, ExportErrorKind::Changed);
    assert_eq!(fs::read_dir(unrelated.path()).unwrap().count(), 0);
    assert_eq!(limiter.stats().active, 1);
    let fresh = ExportDirectory::open_selected(temp.path(), 9).unwrap();
    let removed = failure
        .part
        .unwrap()
        .cleanup_in(&fresh, &context())
        .unwrap();
    assert_eq!(removed.identity, metadata.identity);
    assert_eq!(removed.selection_epoch, metadata.selection_epoch);
    assert_eq!(removed.directory_identity, metadata.directory_identity);
    assert!(directory.plan("old-grant.json").is_err());
    assert_no_private_files(temp.path());
    assert_eq!(limiter.stats().active, 0);
    assert_eq!(limiter.stats().reserved_bytes, 0);
}

#[cfg(windows)]
#[test]
fn windows_junction_selection_and_hardlinked_destination_do_not_expand_write_scope() {
    use std::os::windows::process::CommandExt;
    let (temp, directory, limiter) = selected();
    let outside = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
    let junction = temp.path().join("outside-junction");
    let status = std::process::Command::new("cmd.exe")
        .args(["/D", "/C", "mklink", "/J"])
        .arg(&junction)
        .arg(outside.path())
        .creation_flags(0x08000000)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .unwrap();
    assert!(status.success());
    assert!(ExportDirectory::open_selected(&junction, 12).is_err());
    assert_eq!(fs::read_dir(outside.path()).unwrap().count(), 0);
    let original = outside.path().join("original.json");
    fs::write(&original, b"user data").unwrap();
    let alias = temp.path().join("alias.json");
    fs::hard_link(&original, &alias).unwrap();
    let identity = physical_identity(&original);
    let failure = export_bytes(
        directory.plan("alias.json").unwrap(),
        "hardlink-conflict",
        b"{}",
        &context(),
        &limiter,
    )
    .unwrap_err();
    assert_eq!(failure.kind, ExportErrorKind::Conflict);
    failure.part.unwrap().cleanup(&context()).unwrap();
    assert_eq!(physical_identity(&alias), identity);
    assert_eq!(fs::read(&original).unwrap(), b"user data");
    assert_eq!(fs::read(&alias).unwrap(), b"user data");
}

#[cfg(windows)]
#[test]
fn windows_original_ancestor_handles_block_namespace_replacement_during_export() {
    let temp = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
    let root = temp.path().join("selected");
    fs::create_dir(&root).unwrap();
    let directory = ExportDirectory::open_selected(&root, 9).unwrap();
    let limiter = ExportLimiter::default();
    let receipt = export_with(
        directory.plan("safe.json").unwrap(),
        "pin",
        2,
        &context(),
        &limiter,
        |writer| {
            assert!(fs::rename(&root, temp.path().join("moved")).is_err());
            writer.write_all(b"{}")?;
            Ok(())
        },
    )
    .unwrap();
    assert_eq!(fs::read(&receipt.path).unwrap(), b"{}");
    assert_eq!(receipt.identity, physical_identity(&receipt.path));
    drop(directory);
    fs::rename(&root, temp.path().join("moved")).unwrap();
}

#[cfg(windows)]
#[test]
fn windows_pending_delete_retains_retry_credential_after_original_handle_closes() {
    let (temp, directory, limiter) = selected();
    let mut reader = None;
    let failure = export_with(
        directory.plan("private.json").unwrap(),
        "delayed-delete",
        12,
        &context(),
        &limiter,
        |writer| {
            writer.write_all(b"owned bytes")?;
            let private = fs::read_dir(temp.path())?.next().unwrap()?.path();
            reader = Some(fs::File::open(private)?);
            anyhow::bail!("retain a reader of the private inode")
        },
    )
    .unwrap_err();
    let metadata = failure.part.as_ref().unwrap().metadata();
    let failure = failure.part.unwrap().cleanup(&context()).unwrap_err();
    assert!(failure.part.is_some());
    assert_eq!(limiter.stats().active, 1);
    drop(reader.take());
    // A different object may occupy the old nonce after actual last-close.
    // Exact original-OID absence must succeed without opening or deleting it.
    let reused = temp.path().join(metadata.temporary_name.as_ref().unwrap());
    fs::create_dir(&reused).unwrap();
    fs::write(reused.join("foreign.txt"), b"preserve this new object").unwrap();
    let reused_identity = physical_identity(&reused);
    let removed = failure.part.unwrap().cleanup(&context()).unwrap();
    assert_eq!(removed.identity, metadata.identity);
    assert_eq!(physical_identity(&reused), reused_identity);
    assert_eq!(
        fs::read(reused.join("foreign.txt")).unwrap(),
        b"preserve this new object"
    );
    assert!(!temp.path().join("private.json").exists());
    assert_eq!(limiter.stats().active, 0);
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn unix_temporary_storage_does_not_follow_a_replaced_selected_namespace() {
    let temp = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
    let root = temp.path().join("selected");
    let moved = temp.path().join("moved");
    fs::create_dir(&root).unwrap();
    let directory = ExportDirectory::open_selected(&root, 11).unwrap();
    let limiter = ExportLimiter::default();
    let failure = export_with(
        directory.plan("safe.json").unwrap(),
        "moved-root",
        2,
        &context(),
        &limiter,
        |writer| {
            assert_eq!(
                fs::read_dir(&root)?.count(),
                usize::from(cfg!(target_os = "macos"))
            );
            writer.write_all(b"{}")?;
            fs::rename(&root, &moved)?;
            fs::create_dir(&root)?;
            Ok(())
        },
    )
    .unwrap_err();
    assert!(!root.join("safe.json").exists() && !moved.join("safe.json").exists());
    assert!(failure.part.is_some());
    fs::remove_dir(&root).unwrap();
    fs::rename(&moved, &root).unwrap();
    failure.part.unwrap().cleanup(&context()).unwrap();
    assert_eq!(fs::read_dir(root).unwrap().count(), 0);
    assert_eq!(limiter.stats().active, 0);
}

#[path = "support/library_fixture.rs"]
mod fixture;
use fixture::Running;
use lx_gpui_player::{
    backend::{
        BackendCommand, BackendQuery, BackendQueryResult, BackupExportEncoding, BackupExportFormat,
        BackupExportView,
    },
    backup::{BackupEncoding, LxBackupKind, decode_lx, decode_native},
};
use sha2::{Digest, Sha256};
use std::{fs, path::PathBuf, time::Duration};

fn export(
    running: &Running,
    id: &str,
    directory: PathBuf,
    basename: &str,
    format: BackupExportFormat,
    encoding: BackupExportEncoding,
) -> (bool, BackupExportView) {
    let cursor = running.backend.event_cursor();
    running.execute(
        id,
        BackendCommand::ExportBackup {
            directory,
            basename: basename.into(),
            format,
            encoding,
            include_sources: false,
            include_downloads: false,
            include_edited_lyrics: false,
        },
    );
    let success = fixture::operation_finished(&running.backend, cursor, id).0;
    let BackendQueryResult::BackupExport(Some(view)) = running
        .backend
        .query(BackendQuery::BackupExport {
            operation_id: id.into(),
        })
        .unwrap()
    else {
        panic!("Missing export result")
    };
    (success, *view)
}
#[test]
fn real_lx_four_types_native_and_csv_export_use_verified_files_without_mutating_profile() {
    let running = Running::new();
    running.execute("export-volume", BackendCommand::Volume { volume: 0.23 });
    let directory = running.folder("exports");
    let revision = running.backend.snapshot_with_cursor().unwrap().0.revision;
    for (index, format, kind) in [
        (0, BackupExportFormat::LxAllData, LxBackupKind::AllData),
        (1, BackupExportFormat::LxSettings, LxBackupKind::Settings),
        (2, BackupExportFormat::LxPlayList, LxBackupKind::PlayList),
        (
            3,
            BackupExportFormat::LxPlayListPart {
                list_id: "default".into(),
            },
            LxBackupKind::PlayListPart,
        ),
    ] {
        let id = format!("file-export-{index}");
        let (success, view) = export(
            &running,
            &id,
            directory.clone(),
            &format!("备份 {index}.lxmc"),
            format,
            BackupExportEncoding::Gzip,
        );
        assert!(success, "{view:?}");
        assert_eq!(view.state, "published");
        assert_eq!(view.snapshot_revision, revision);
        let receipt = view.receipt.unwrap();
        let bytes = fs::read(&receipt.path).unwrap();
        assert_eq!(receipt.bytes, bytes.len() as u64);
        assert_eq!(receipt.sha256, format!("{:x}", Sha256::digest(&bytes)));
        assert!(!receipt.identity.object.is_empty());
        let decoded = decode_lx(&bytes, BackupEncoding::Auto).unwrap();
        assert_eq!(decoded.kind, kind);
        if let Some(settings) = decoded.settings {
            assert!((settings.values["player.volume"].as_f64().unwrap() - 0.23).abs() < 0.001);
            assert!(!settings.values.contains_key("sync.authKey"));
        }
    }
    let (success, native) = export(
        &running,
        "native-export",
        directory.clone(),
        "native.json",
        BackupExportFormat::Native,
        BackupExportEncoding::Json,
    );
    assert!(success, "{native:?}");
    let decoded = decode_native(&fs::read(native.path).unwrap(), BackupEncoding::Json).unwrap();
    assert!(
        decoded.store.sources.is_empty()
            && decoded.store.selected.is_none()
            && decoded.store.local_files.is_empty()
    );
    assert!(decoded.downloads.is_none());
    let (success, csv) = export(
        &running,
        "csv-export",
        directory.clone(),
        "曲库.csv",
        BackupExportFormat::Csv {
            list_id: "default".into(),
        },
        BackupExportEncoding::Json,
    );
    assert!(success, "{csv:?}");
    assert!(
        String::from_utf8(fs::read(csv.path).unwrap())
            .unwrap()
            .contains("歌曲")
    );
    assert_eq!(
        running.backend.snapshot_with_cursor().unwrap().0.revision,
        revision
    );
    assert!(fs::read_dir(&directory).unwrap().all(|entry| {
        !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .ends_with(".part")
    }));
    running.backend.shutdown(Duration::from_secs(5)).unwrap();
}
#[test]
fn foreign_target_is_preserved_and_private_failed_owner_can_be_explicitly_cleaned() {
    let running = Running::new();
    let directory = running.folder("conflict");
    let path = directory.join("existing.json");
    fs::write(&path, b"foreign user document").unwrap();
    let (success, view) = export(
        &running,
        "conflicting-export",
        directory.clone(),
        "existing.json",
        BackupExportFormat::Native,
        BackupExportEncoding::Json,
    );
    assert!(!success);
    assert_eq!(fs::read(&path).unwrap(), b"foreign user document");
    assert!(view.owned_part.as_ref().is_some_and(|part| !part.published));
    let cursor = running.backend.event_cursor();
    running.execute(
        "cleanup-conflicting-export",
        BackendCommand::CleanupBackupExport {
            export_operation_id: "conflicting-export".into(),
            directory: directory.clone(),
        },
    );
    assert!(fixture::operation_finished(&running.backend, cursor, "cleanup-conflicting-export").0);
    let BackendQueryResult::BackupExport(Some(view)) = running
        .backend
        .query(BackendQuery::BackupExport {
            operation_id: "conflicting-export".into(),
        })
        .unwrap()
    else {
        panic!("result")
    };
    assert_eq!(view.state, "cleaned");
    assert!(view.owned_part.is_none());
    assert_eq!(fs::read(&path).unwrap(), b"foreign user document");
}
#[test]
fn missing_directory_and_leaf_escape_do_not_create_any_output_or_write_grant() {
    let running = Running::new();
    let missing = running.directory.path().join("missing");
    let (success, view) = export(
        &running,
        "missing-export-dir",
        missing.clone(),
        "out.json",
        BackupExportFormat::Native,
        BackupExportEncoding::Json,
    );
    assert!(!success);
    assert!(!missing.exists());
    assert!(view.owned_part.is_none());
    let directory = running.folder("selected");
    let (success, view) = export(
        &running,
        "escaped-export",
        directory.clone(),
        "../escaped.json",
        BackupExportFormat::Native,
        BackupExportEncoding::Json,
    );
    assert!(!success);
    assert!(view.owned_part.is_none());
    assert!(fs::read_dir(directory).unwrap().next().is_none());
    assert!(!running.directory.path().join("escaped.json").exists());
}

#[test]
fn imported_compatibility_keys_and_list_extensions_survive_export_without_secret_fields() {
    let running = Running::new();
    let input = running.folder("import").join("compat.json");
    fs::write(&input,serde_json::to_vec(&serde_json::json!({
        "type":"allData_v2","setting":{"version":"2.1.0","visual.unknown":{"palette":"night"},"player.volume":0.31,"sync.authKey":"private-pairing-key","openAPI.token":"private-bearer","sourceApprovals":{"installed":true}},
        "playList":[{"id":"compat-list","name":"含扩展列表","source":null,"locationUpdateTime":null,"custom":{"key":"值"},"list":[]}]
    })).unwrap()).unwrap();
    let cursor = running.backend.event_cursor();
    running.execute(
        "compat-preview",
        BackendCommand::PreviewLxBackup {
            path: input,
            replace_tracks: true,
            copy_list_id: None,
        },
    );
    assert!(fixture::operation_finished(&running.backend, cursor, "compat-preview").0);
    running.execute(
        "compat-commit",
        BackendCommand::CommitLxBackup {
            preview_operation_id: "compat-preview".into(),
        },
    );
    running.execute(
        "actual-pref-after-import",
        BackendCommand::Volume { volume: 0.64 },
    );
    let directory = running.folder("compat-export");
    let (success, view) = export(
        &running,
        "compat-export",
        directory,
        "all.json",
        BackupExportFormat::LxAllData,
        BackupExportEncoding::Json,
    );
    assert!(success, "{view:?}");
    let decoded = decode_lx(&fs::read(view.path).unwrap(), BackupEncoding::Json).unwrap();
    let settings = decoded.settings.unwrap().values;
    assert_eq!(settings["visual.unknown"]["palette"], "night");
    assert!((settings["player.volume"].as_f64().unwrap() - 0.64).abs() < 0.001);
    for key in ["sync.authKey", "openAPI.token", "sourceApprovals"] {
        assert!(!settings.contains_key(key));
    }
    let list = decoded
        .lists
        .into_iter()
        .find(|list| list.info.id == "compat-list")
        .unwrap();
    assert_eq!(list.wire["custom"]["key"], "值");
    assert!(list.wire["source"].is_null());
    assert!(list.wire["locationUpdateTime"].is_null());
}

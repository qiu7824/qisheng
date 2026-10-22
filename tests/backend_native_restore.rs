//! Native archive imports exercise the actual actor, SQLite transaction and
//! ProjectStore file/receipt saga without restoring execution permissions.

#[path = "support/library_fixture.rs"]
mod fixture;
use fixture::Running;
use lx_gpui_player::{
    backend::{
        BackendCommand, BackendQuery, BackendQueryResult, NativeBackupImportView,
        NativeBackupPreviewView,
    },
    backup::native::{NativeDownloadPlan, NativeEditedLyrics},
    backup::{BackupEncoding, NativeBackup, NativeBackupOptions, encode_native},
    domain::{
        lists::{AddLocation, ListAction, ListData},
        lyric::{LyricBundle, LyricField, LyricOrigin},
        music::{LxMusicDtoVersion, TrackRecord},
    },
    download::{planning::DownloadQuality, task_model::DownloadSettings},
    open_api::{ServerConfig, ServerState},
    repository::Repository,
    source::import_script,
    store::{ProjectStore, StoreData},
};
use std::{
    fs,
    io::{Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    path::PathBuf,
    time::Duration,
};

fn track(id: &str, title: &str) -> TrackRecord {
    TrackRecord::from_lx_dto(
        serde_json::json!({"source":"kw", "songmid":id,
        "name":title, "singer":"Original imported artist", "albumName":"Native archive album",
        "interval":"00:36", "types":[{"type":"128k"}], "_types":{"128k":{}}}),
        LxMusicDtoVersion::Legacy,
    )
    .unwrap()
}
fn archive(running: &Running, backup: &NativeBackup, name: &str) -> PathBuf {
    let path = running
        .folder("native-archives")
        .join(format!("{name}.json"));
    fs::write(&path, encode_native(backup, BackupEncoding::Json).unwrap()).unwrap();
    path
}
fn preview(
    running: &Running,
    id: &str,
    path: PathBuf,
    replace_tracks: bool,
) -> NativeBackupPreviewView {
    let cursor = running.backend.event_cursor();
    running.execute(
        id,
        BackendCommand::PreviewNativeBackup {
            path,
            replace_tracks,
        },
    );
    assert!(fixture::operation_finished(&running.backend, cursor, id).0);
    let BackendQueryResult::NativeBackupPreview(Some(preview)) = running
        .backend
        .query(BackendQuery::NativeBackupPreview)
        .unwrap()
    else {
        panic!("Expected native preview");
    };
    assert_eq!(preview.operation_id, id);
    preview
}
fn commit(
    running: &Running,
    id: &str,
    preview: &NativeBackupPreviewView,
) -> Result<(), lx_gpui_player::backend::BackendError> {
    running
        .backend
        .execute(fixture::envelope(
            id,
            BackendCommand::CommitNativeBackup {
                preview_operation_id: preview.operation_id.clone(),
                input_digest: preview.input_digest.clone(),
            },
        ))
        .map(|_| ())
}
fn receipt(running: &Running, id: &str) -> Option<NativeBackupImportView> {
    let BackendQueryResult::NativeBackupImportResult(result) = running
        .backend
        .query(BackendQuery::NativeBackupImportResult {
            operation_id: id.into(),
        })
        .unwrap()
    else {
        panic!("Expected native receipt");
    };
    result
}
fn source(running: &Running, id: &str, script: String, approve: bool) -> String {
    let hash = lx_gpui_player::source::script_hash(&script);
    let cursor = running.backend.event_cursor();
    running.execute(id, BackendCommand::ImportSourceText { script });
    assert!(fixture::operation_finished(&running.backend, cursor, id).0);
    if approve {
        running.execute(
            &format!("{id}-approve"),
            BackendCommand::ApproveSource { id: hash.clone() },
        );
    }
    hash
}
fn list_ids(running: &Running) -> Vec<String> {
    let BackendQueryResult::ListTracks(page) = running
        .backend
        .query(BackendQuery::ListTracks {
            list_id: "default".into(),
            offset: 0,
            limit: 100,
        })
        .unwrap()
    else {
        panic!("Expected default list");
    };
    page.items.into_iter().map(|track| track.id).collect()
}

fn http_body(address: SocketAddr, target: &str) -> String {
    let mut stream = TcpStream::connect(address).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(6)))
        .unwrap();
    write!(stream, "GET {target} HTTP/1.1\r\nHost: {address}\r\n\r\n").unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    response.split_once("\r\n\r\n").unwrap().1.to_owned()
}

#[test]
fn actual_native_restore_commits_all_sections_with_inert_source_and_download_archives() {
    let mut running = Running::new();
    let prior = track("native-prior", "Prior list music");
    running.execute(
        "native-prior-list",
        BackendCommand::ListAction {
            action: Box::new(ListAction::MusicAdd {
                list_id: "default".into(),
                tracks: vec![prior],
                location: AddLocation::Top,
            }),
        },
    );
    let existing_script = "/** @name Existing actual source grant */\nconst unchanged = 1;";
    let existing = source(
        &running,
        "native-existing-source",
        existing_script.into(),
        true,
    );
    let forbidden = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    forbidden.set_nonblocking(true).unwrap();
    let dormant_script = format!(
        "/** @name Archived dormant source */\nlx.request('http://{}/must-not-run',{{}},()=>{{}});",
        forbidden.local_addr().unwrap()
    );
    let imported = import_script(&dormant_script).unwrap();
    let restored = track("native-known-lyric", "Native restored music");
    let unknown = track("native-unknown-lyric", "Archived lyric music");
    let unknown_id = unknown.id.clone();
    let bundle = LyricBundle {
        lyric: LyricField::Text("[00:01.00]真实编辑歌词".into()),
        ..Default::default()
    };
    let saved = StoreData {
        volume: 0.43,
        sources: vec![import_script(existing_script).unwrap(), imported.clone()],
        local_files: vec!["C:/unselected-native/never-open.flac".into()],
        ..Default::default()
    };
    let mut backup = NativeBackup::from_snapshot(
        &saved,
        &ListData {
            default_list: vec![restored.clone()],
            ..Default::default()
        },
        "Native restored music@Original imported artist",
        &[],
        &[
            NativeEditedLyrics {
                track_id: restored.id.clone(),
                bundle: bundle.clone(),
            },
            NativeEditedLyrics {
                track_id: unknown.id.clone(),
                bundle: bundle.clone(),
            },
        ],
        NativeBackupOptions {
            include_sources: true,
            include_downloads: true,
            include_edited_lyrics: true,
        },
    )
    .unwrap();
    backup.downloads = Some(vec![NativeDownloadPlan {
        record: unknown,
        requested_quality: DownloadQuality::K128,
        list_id: Some("default".into()),
        list_name_snapshot: Some("Original archive list".into()),
        order: 0,
        settings: DownloadSettings {
            enabled: false,
            ..Default::default()
        },
    }]);
    backup.extensions.insert(
        "portable.future".into(),
        serde_json::json!({"preserved":true}),
    );
    let baseline = ProjectStore::open(running.profile())
        .unwrap()
        .load()
        .unwrap();
    let path = archive(&running, &backup, "all-sections");
    let preview = preview(&running, "native-all-preview", path, true);
    assert_eq!(preview.source_archives, 2);
    assert_eq!(preview.download_plans, 1);
    assert_eq!(preview.edited_lyrics, 2);
    assert_eq!(preview.declared_local_paths, 1);
    assert!(
        preview
            .requires_activation
            .iter()
            .any(|kind| kind == "source_scripts")
    );
    commit(&running, "native-all-commit", &preview).unwrap();
    let result = receipt(&running, "native-all-commit").unwrap();
    assert!(result.preferences_applied);
    assert_eq!(result.counts.sources, 2);
    assert_eq!(result.counts.download_plans, 1);
    assert_eq!(result.counts.applied_edited_lyrics, 1);
    assert_eq!(result.counts.archived_edited_lyrics, 1);
    assert_eq!(
        list_ids(&running).as_slice(),
        std::slice::from_ref(&restored.id)
    );
    let BackendQueryResult::DislikeRules(dislike) =
        running.backend.query(BackendQuery::DislikeRules).unwrap()
    else {
        panic!("Expected dislike rules");
    };
    assert!(dislike.matches("Native restored music", "Original imported artist"));
    let stored = ProjectStore::open(running.profile())
        .unwrap()
        .load()
        .unwrap();
    assert!((stored.volume - 0.43).abs() < 0.00001);
    assert_eq!(stored.local_files, baseline.local_files);
    assert_eq!(stored.selected, baseline.selected);
    assert!(stored.sources.len() >= baseline.sources.len());
    assert_eq!(
        serde_json::to_value(&stored.sources[..baseline.sources.len()]).unwrap(),
        serde_json::to_value(&baseline.sources).unwrap()
    );
    let granted = stored
        .sources
        .iter()
        .find(|source| source.sha256 == existing)
        .unwrap();
    assert!(granted.approved);
    granted.require_approval().unwrap();
    let archived = stored
        .sources
        .iter()
        .find(|source| source.sha256 == imported.sha256)
        .unwrap();
    assert!(!archived.approved);
    assert_eq!(archived.approval_policy_version, 0);
    assert!(archived.require_approval().is_err());
    assert!(forbidden.accept().is_err());
    let BackendQueryResult::DownloadTasks(downloads) =
        running.backend.query(BackendQuery::DownloadTasks).unwrap()
    else {
        panic!("Expected download tasks");
    };
    assert!(downloads.is_empty());
    running.execute(
        "native-user-volume-after-import",
        BackendCommand::Volume { volume: 0.75 },
    );
    commit(&running, "native-all-commit", &preview).unwrap();
    assert!(
        (ProjectStore::open(running.profile())
            .unwrap()
            .load()
            .unwrap()
            .volume
            - 0.75)
            .abs()
            < 0.00001
    );
    running.restart();
    assert_eq!(
        ProjectStore::open(running.profile())
            .unwrap()
            .load()
            .unwrap()
            .sources
            .len(),
        baseline.sources.len() + 1
    );
    assert!(forbidden.accept().is_err());
    assert!(
        receipt(&running, "native-all-commit")
            .unwrap()
            .preferences_applied
    );
    running.backend.shutdown(Duration::from_secs(5)).unwrap();
    let repository = Repository::open(running.profile()).unwrap();
    let saved_archive = repository
        .native_backup_archive("native-all-commit")
        .unwrap()
        .unwrap();
    assert_eq!(
        saved_archive.declared_local_paths,
        ["C:/unselected-native/never-open.flac"]
    );
    assert_eq!(saved_archive.sources.len(), 2);
    assert_eq!(saved_archive.download_plans.as_ref().unwrap().len(), 1);
    assert!(
        !saved_archive.download_plans.as_ref().unwrap()[0]
            .settings
            .enabled
    );
    assert!(
        saved_archive.download_plans.as_ref().unwrap()[0]
            .settings
            .destination_grant_id
            .is_none()
    );
    assert!(
        saved_archive
            .edited_lyrics
            .as_ref()
            .unwrap()
            .iter()
            .any(|lyric| lyric.track_id == unknown_id && lyric.bundle == bundle)
    );
    assert_eq!(
        saved_archive.extensions["portable.future"]["preserved"],
        true
    );
    assert_eq!(
        repository
            .lyrics(&restored.id, LyricOrigin::Edited)
            .unwrap(),
        Some(bundle)
    );
}

#[test]
fn native_preview_digest_and_current_repository_cas_prevent_partial_mutation() {
    let running = Running::new();
    let prior = track("native-cas-prior", "Current music");
    running.execute(
        "native-cas-current",
        BackendCommand::ListAction {
            action: Box::new(ListAction::MusicAdd {
                list_id: "default".into(),
                tracks: vec![prior.clone()],
                location: AddLocation::Top,
            }),
        },
    );
    let restored = track("native-cas-incoming", "Incoming music");
    let backup = NativeBackup::from_snapshot(
        &StoreData {
            volume: 0.21,
            ..Default::default()
        },
        &ListData {
            default_list: vec![restored],
            ..Default::default()
        },
        "",
        &[],
        &[],
        NativeBackupOptions::default(),
    )
    .unwrap();
    let path = archive(&running, &backup, "cas");
    let prepared = preview(&running, "native-cas-preview", path, true);
    let mut altered = prepared.clone();
    altered.input_digest = "0".repeat(64);
    assert!(commit(&running, "native-wrong-digest", &altered).is_err());
    assert!(receipt(&running, "native-wrong-digest").is_none());
    running.execute(
        "native-change-after-preview",
        BackendCommand::Volume { volume: 0.64 },
    );
    assert!(commit(&running, "native-stale-preview", &prepared).is_err());
    assert!(receipt(&running, "native-stale-preview").is_none());
    assert_eq!(list_ids(&running), [prior.id]);
    assert!(
        (ProjectStore::open(running.profile())
            .unwrap()
            .load()
            .unwrap()
            .volume
            - 0.64)
            .abs()
            < 0.00001
    );
}

#[test]
fn native_store_file_failure_preserves_pending_receipt_then_real_restart_applies_once() {
    let mut running = Running::new();
    running.execute(
        "native-file-primary",
        BackendCommand::Volume { volume: 0.68 },
    );
    let old_json = fs::read(running.profile().join("player-state.json")).unwrap();
    let restored = track("native-file-incoming", "After recovered store write");
    let backup = NativeBackup::from_snapshot(
        &StoreData {
            volume: 0.27,
            ..Default::default()
        },
        &ListData {
            default_list: vec![restored.clone()],
            ..Default::default()
        },
        "",
        &[],
        &[],
        NativeBackupOptions::default(),
    )
    .unwrap();
    let path = archive(&running, &backup, "file-failure");
    let prepared = preview(&running, "native-file-preview", path, true);
    let obstruction = running.profile().join("player-state.json.bak");
    assert!(!obstruction.exists());
    fs::create_dir(&obstruction).unwrap();
    assert!(commit(&running, "native-file-commit", &prepared).is_err());
    let pending = receipt(&running, "native-file-commit").unwrap();
    assert!(!pending.preferences_applied);
    assert_eq!(list_ids(&running), [restored.id]);
    assert_eq!(
        fs::read(running.profile().join("player-state.json")).unwrap(),
        old_json
    );
    fs::remove_dir(&obstruction).unwrap();
    running.restart();
    assert!(
        receipt(&running, "native-file-commit")
            .unwrap()
            .preferences_applied
    );
    assert!(
        (ProjectStore::open(running.profile())
            .unwrap()
            .load()
            .unwrap()
            .volume
            - 0.27)
            .abs()
            < 0.00001
    );
    running.execute(
        "native-file-user-update",
        BackendCommand::Volume { volume: 0.81 },
    );
    running.restart();
    assert!(
        (ProjectStore::open(running.profile())
            .unwrap()
            .load()
            .unwrap()
            .volume
            - 0.81)
            .abs()
            < 0.00001
    );
}

#[test]
fn native_source_union_budget_is_checked_before_any_repository_commit() {
    let running = Running::new();
    for index in 0..5 {
        source(
            &running,
            &format!("native-existing-large-{index}"),
            format!(
                "/** @name Existing large source {index} */\n/*{}*/",
                "x".repeat(900_000)
            ),
            false,
        );
    }
    let mut imported = StoreData {
        volume: 0.22,
        ..Default::default()
    };
    for index in 0..5 {
        imported.sources.push(
            import_script(&format!(
                "/** @name Incoming large source {index} */\n/*{}*/",
                "y".repeat(900_000)
            ))
            .unwrap(),
        );
    }
    let backup = NativeBackup::from_snapshot(
        &imported,
        &ListData {
            default_list: vec![track("native-budget", "Budget candidate")],
            ..Default::default()
        },
        "",
        &[],
        &[],
        NativeBackupOptions {
            include_sources: true,
            ..Default::default()
        },
    )
    .unwrap();
    let path = archive(&running, &backup, "source-union-budget");
    let prepared = preview(&running, "native-budget-preview", path, true);
    let before = running.backend.snapshot();
    assert!(commit(&running, "native-budget-commit", &prepared).is_err());
    assert!(receipt(&running, "native-budget-commit").is_none());
    assert_eq!(running.backend.snapshot().revision, before.revision);
    assert!(list_ids(&running).is_empty());
    assert_eq!(
        ProjectStore::open(running.profile())
            .unwrap()
            .load()
            .unwrap()
            .sources
            .len(),
        5
    );
}

#[test]
fn native_preference_marker_rejects_reused_operation_with_another_digest() {
    let running = Running::new();
    running.backend.shutdown(Duration::from_secs(5)).unwrap();
    let store = ProjectStore::open(running.profile()).unwrap();
    let mut candidate = store.load().unwrap();
    candidate.volume = 0.36;
    store
        .apply_native_backup_preferences("native-marker", &"a".repeat(64), &candidate)
        .unwrap();
    let first = fs::read(running.profile().join("player-state.json")).unwrap();
    candidate.volume = 0.91;
    assert!(
        store
            .apply_native_backup_preferences("native-marker", &"b".repeat(64), &candidate)
            .is_err()
    );
    assert_eq!(
        fs::read(running.profile().join("player-state.json")).unwrap(),
        first
    );
    let repo = Repository::open(running.profile()).unwrap();
    repo.integrity_check().unwrap();
}

#[test]
fn native_import_refreshes_actual_current_edited_lyrics_without_restarting_playback() {
    let running = Running::new();
    let path = running
        .folder("native-playing-lyrics")
        .join("same-generation.wav");
    fixture::wave(&path, 440.0, 36.0);
    let original_store = ProjectStore::open(running.profile())
        .unwrap()
        .load()
        .unwrap();
    running.import_explicit("native-lyric-audio-import", vec![path.clone()]);
    let after_import = ProjectStore::open(running.profile())
        .unwrap()
        .load()
        .unwrap();
    assert_eq!(after_import.local_files, original_store.local_files);
    let selected_path = fs::canonicalize(&path).unwrap();
    let actual_asset = fixture::assets(&running.backend, None)
        .into_iter()
        .find(|asset| {
            fs::canonicalize(&asset.asset.path).is_ok_and(|actual| actual == selected_path)
        })
        .expect("The selected audio must retain its durable local asset binding");
    let BackendQueryResult::LibraryAssetBinding(Some(binding)) = running
        .backend
        .query(BackendQuery::LibraryAssetBinding {
            asset_id: actual_asset.asset.id.clone(),
        })
        .unwrap()
    else {
        panic!("Expected durable actual asset binding");
    };
    assert!(binding.file_identity.is_some());
    assert!(binding.binding_revision > 0);
    let grants = rusqlite::Connection::open_with_flags(
        running
            .profile()
            .join(lx_gpui_player::repository::DATABASE_NAME),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .unwrap();
    let actual_grant: bool = grants.query_row(
        "SELECT EXISTS(SELECT 1 FROM local_file_grants f JOIN local_asset_locations l ON l.asset_id=f.asset_id AND l.path_key=f.grant_path_key WHERE f.asset_id=?1 AND f.epoch>0 AND json_extract(f.document,'$.file_identity')=json_extract(l.document,'$.file_identity'))",
        [&actual_asset.asset.id], |row| row.get(0)).unwrap();
    assert!(actual_grant);
    drop(grants);
    let (record, _) = lx_gpui_player::repository::local_track_from_path(&path).unwrap();
    let old = LyricBundle {
        lyric: LyricField::Text("[00:00.00]原来的编辑行".into()),
        ..Default::default()
    };
    running.execute(
        "native-lyric-original",
        BackendCommand::SaveLyrics {
            id: record.id.clone(),
            bundle: old.clone(),
        },
    );
    let playback = running.play("native-lyric-start", &record.id, 440.0);
    fixture::wait(&running.backend, 5.0, |state| {
        state
            .lyrics
            .as_ref()
            .is_some_and(|lyrics| lyrics.bundle == old)
    });
    let mut endpoint = running
        .backend
        .start_open_api(ServerConfig {
            port: 0,
            ..Default::default()
        })
        .unwrap();
    fixture::wait(&running.backend, 3.0, |_| {
        endpoint.view().state == ServerState::Listening
    });
    let address = endpoint.view().address.unwrap();
    assert_eq!(http_body(address, "/lyric"), old.lyric.text().unwrap());
    let replacement = LyricBundle {
        lyric: LyricField::Text("[00:00.00]恢复后的编辑行".into()),
        ..Default::default()
    };
    let backup = NativeBackup::from_snapshot(
        &StoreData::default(),
        &ListData {
            default_list: vec![record.clone()],
            ..Default::default()
        },
        "",
        &[],
        &[NativeEditedLyrics {
            track_id: record.id.clone(),
            bundle: replacement.clone(),
        }],
        NativeBackupOptions {
            include_edited_lyrics: true,
            ..Default::default()
        },
    )
    .unwrap();
    let path = archive(&running, &backup, "same-generation-lyrics");
    let prepared = preview(&running, "native-lyric-preview", path, true);
    commit(&running, "native-lyric-commit", &prepared).unwrap();
    let updated = fixture::wait(&running.backend, 5.0, |state| {
        state
            .lyrics
            .as_ref()
            .is_some_and(|lyrics| lyrics.bundle == replacement)
            && state
                .lyric_frame
                .as_ref()
                .is_some_and(|frame| frame.line_text == "恢复后的编辑行")
    });
    assert_eq!(updated.playback.generation, playback.playback.generation);
    assert_eq!(
        updated.playback.track_id.as_deref(),
        Some(record.id.as_str())
    );
    assert_eq!(
        http_body(address, "/lyric"),
        replacement.lyric.text().unwrap()
    );
    let status: serde_json::Value =
        serde_json::from_str(&http_body(address, "/status?filter=lyric,lyricLineText")).unwrap();
    assert_eq!(status["lyric"], replacement.lyric.text().unwrap());
    assert_eq!(status["lyricLineText"], "恢复后的编辑行");
    assert!(endpoint.shutdown(Duration::from_secs(5)).unwrap());
}

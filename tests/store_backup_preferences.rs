use lx_gpui_player::{
    source::import_script,
    store::{BackupPreferencesConflict, ProjectStore, StoreData},
};
use serde_json::{Value, json};
use std::fs;

fn directory() -> tempfile::TempDir {
    tempfile::tempdir_in(std::env::temp_dir()).unwrap()
}
fn store(directory: &tempfile::TempDir) -> ProjectStore {
    ProjectStore::open(directory.path().to_path_buf()).unwrap()
}
fn digest() -> String {
    "a".repeat(64)
}
fn state(directory: &tempfile::TempDir) -> std::path::PathBuf {
    directory.path().join("player-state.json")
}
fn backup(directory: &tempfile::TempDir) -> std::path::PathBuf {
    directory.path().join("player-state.json.bak")
}
fn raw(directory: &tempfile::TempDir) -> Value {
    serde_json::from_slice(&fs::read(state(directory)).unwrap()).unwrap()
}

#[test]
fn preference_and_private_marker_share_one_saved_envelope_and_survive_restart() {
    let directory = directory();
    let first = store(&directory);
    assert!(
        !first
            .backup_preferences_applied("operation-1", &digest())
            .unwrap()
    );
    let mut candidate = first.load().unwrap();
    candidate.volume = 0.25;
    candidate.audio_preferences.muted = true;
    candidate.equalizer.gains_db[0] = 3.0;
    candidate.playback_processing.playback_rate = 1.25;
    first
        .apply_backup_preferences("operation-1", &digest(), &candidate)
        .unwrap();
    let document = raw(&directory);
    assert_eq!(document["data"]["volume"], 0.25);
    assert_eq!(document["data"]["audio_preferences"]["muted"], true);
    assert_eq!(
        document["applied_backup_preferences"],
        json!([{"operation_id":"operation-1","input_digest":digest()}])
    );
    assert!(document["data"].get("applied_backup_preferences").is_none());
    drop(first);
    let reopened = store(&directory);
    let loaded = reopened.load().unwrap();
    assert_eq!(loaded.volume, 0.25);
    assert_eq!(loaded.equalizer.gains_db[0], 3.0);
    assert_eq!(loaded.playback_processing.playback_rate, 1.25);
    assert!(
        reopened
            .backup_preferences_applied("operation-1", &digest())
            .unwrap()
    );
}

#[test]
fn replay_preserves_later_user_volume_and_normal_save_retains_markers() {
    let directory = directory();
    let first = store(&directory);
    let mut imported = first.load().unwrap();
    imported.volume = 0.2;
    first
        .apply_backup_preferences("once", &digest(), &imported)
        .unwrap();
    let mut changed = first.load().unwrap();
    changed.volume = 0.9;
    changed.audio_preferences.muted = true;
    first.save(&changed).unwrap();
    let body = fs::read(state(&directory)).unwrap();
    drop(first);
    let reopened = store(&directory);
    reopened
        .apply_backup_preferences("once", &digest(), &imported)
        .unwrap();
    assert_eq!(fs::read(state(&directory)).unwrap(), body);
    assert_eq!(reopened.load().unwrap().volume, 0.9);
    assert!(reopened.load().unwrap().audio_preferences.muted);
    assert!(
        reopened
            .backup_preferences_applied("once", &digest())
            .unwrap()
    );
}

#[test]
fn duplicate_operation_with_a_different_digest_conflicts_before_writing() {
    let directory = directory();
    let first = store(&directory);
    let candidate = first.load().unwrap();
    first
        .apply_backup_preferences("same", &digest(), &candidate)
        .unwrap();
    let primary = fs::read(state(&directory)).unwrap();
    let files = fs::read_dir(directory.path()).unwrap().count();
    let conflict = first
        .apply_backup_preferences("same", &"b".repeat(64), &candidate)
        .unwrap_err();
    assert!(conflict.is::<BackupPreferencesConflict>());
    assert!(
        first
            .backup_preferences_applied("same", &"b".repeat(64))
            .unwrap_err()
            .is::<BackupPreferencesConflict>()
    );
    assert_eq!(fs::read(state(&directory)).unwrap(), primary);
    assert_eq!(fs::read_dir(directory.path()).unwrap().count(), files);
}

#[test]
fn preference_import_cannot_change_source_approval_selection_or_local_records() {
    let directory = directory();
    let first = store(&directory);
    let mut source = import_script("/* @name 已批准测试源 */\nconst testSource = 1;").unwrap();
    source.approve().unwrap();
    let selected = source.sha256.clone();
    let initial = StoreData {
        sources: vec![source],
        selected: Some(selected.clone()),
        local_files: vec![directory.path().join("selected-reference.wav")],
        ..StoreData::default()
    };
    first.save(&initial).unwrap();
    let current = first.load().unwrap();
    let mut altered = current.clone();
    altered.sources[0].approved = false;
    assert!(
        first
            .apply_backup_preferences("approval-change", &digest(), &altered)
            .is_err()
    );
    altered = current.clone();
    altered.sources[0].metadata.name = "改名".into();
    assert!(
        first
            .apply_backup_preferences("source-change", &digest(), &altered)
            .is_err()
    );
    altered = current.clone();
    altered.selected = None;
    assert!(
        first
            .apply_backup_preferences("selection-change", &digest(), &altered)
            .is_err()
    );
    altered = current.clone();
    altered
        .local_files
        .push(directory.path().join("unselected-reference.flac"));
    assert!(
        first
            .apply_backup_preferences("file-change", &digest(), &altered)
            .is_err()
    );
    let mut candidate = current.clone();
    candidate.volume = 0.4;
    first
        .apply_backup_preferences("safe-preferences", &digest(), &candidate)
        .unwrap();
    let restored = first.load().unwrap();
    assert!(restored.sources[0].approved);
    assert_eq!(restored.sources[0].sha256, selected);
    assert_eq!(restored.selected, current.selected);
    assert_eq!(restored.local_files, current.local_files);
    assert_eq!(restored.volume, 0.4);
    for operation in [
        "approval-change",
        "source-change",
        "selection-change",
        "file-change",
    ] {
        assert!(
            !first
                .backup_preferences_applied(operation, &digest())
                .unwrap()
        );
    }
}

#[test]
fn real_filesystem_replacement_failure_preserves_primary_and_omits_marker() {
    let directory = directory();
    let first = store(&directory);
    let current = first.load().unwrap();
    first.save(&current).unwrap();
    let primary = fs::read(state(&directory)).unwrap();
    // An actual directory occupying the backup leaf makes remove_file fail.
    fs::create_dir(backup(&directory)).unwrap();
    let mut candidate = current.clone();
    candidate.volume = 0.1;
    assert!(
        first
            .apply_backup_preferences("disk-failure", &digest(), &candidate)
            .is_err()
    );
    assert_eq!(fs::read(state(&directory)).unwrap(), primary);
    assert_eq!(first.load().unwrap().volume, current.volume);
    assert!(
        !first
            .backup_preferences_applied("disk-failure", &digest())
            .unwrap()
    );
    assert!(fs::read_dir(directory.path()).unwrap().all(|entry| {
        !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".player-state-")
    }));
    fs::remove_dir(backup(&directory)).unwrap();
    first
        .apply_backup_preferences("disk-failure", &digest(), &candidate)
        .unwrap();
    assert_eq!(first.load().unwrap().volume, 0.1);
    assert!(
        first
            .backup_preferences_applied("disk-failure", &digest())
            .unwrap()
    );
}

#[test]
fn missing_primary_recovers_complete_backup_with_markers_without_reset() {
    let directory = directory();
    let first = store(&directory);
    let mut candidate = first.load().unwrap();
    candidate.volume = 0.3;
    first
        .apply_backup_preferences("recover", &digest(), &candidate)
        .unwrap();
    let mut later = first.load().unwrap();
    later.volume = 0.8;
    first.save(&later).unwrap();
    fs::remove_file(state(&directory)).unwrap();
    drop(first);
    let reopened = store(&directory);
    assert_eq!(reopened.load().unwrap().volume, 0.3);
    assert!(
        reopened
            .backup_preferences_applied("recover", &digest())
            .unwrap()
    );
    reopened
        .apply_backup_preferences("recover", &digest(), &candidate)
        .unwrap();
    assert!(!state(&directory).exists());
    let mut restored = reopened.load().unwrap();
    restored.volume = 0.6;
    reopened.save(&restored).unwrap();
    assert!(state(&directory).exists());
    assert_eq!(reopened.load().unwrap().volume, 0.6);
    assert!(
        reopened
            .backup_preferences_applied("recover", &digest())
            .unwrap()
    );
}

#[test]
fn corrupt_primary_is_not_hidden_by_healthy_backup_or_overwritten() {
    let directory = directory();
    let first = store(&directory);
    let candidate = first.load().unwrap();
    first
        .apply_backup_preferences("old", &digest(), &candidate)
        .unwrap();
    first.save(&candidate).unwrap();
    let original_backup = fs::read(backup(&directory)).unwrap();
    fs::write(state(&directory), b"damaged state").unwrap();
    assert!(first.load().is_err());
    assert!(first.backup_preferences_applied("old", &digest()).is_err());
    assert!(
        first
            .apply_backup_preferences("new", &digest(), &candidate)
            .is_err()
    );
    assert!(first.save(&candidate).is_err());
    assert_eq!(fs::read(state(&directory)).unwrap(), b"damaged state");
    assert_eq!(fs::read(backup(&directory)).unwrap(), original_backup);
}

#[test]
fn old_state_defaults_marker_empty_and_uploaded_store_data_cannot_inject_markers() {
    let directory = directory();
    let first = store(&directory);
    let old = json!({"version":1,"data":StoreData::default()});
    fs::write(state(&directory), serde_json::to_vec(&old).unwrap()).unwrap();
    assert_eq!(first.load().unwrap().volume, 0.7);
    assert!(
        !first
            .backup_preferences_applied("uploaded", &digest())
            .unwrap()
    );
    let mut uploaded = serde_json::to_value(first.load().unwrap()).unwrap();
    uploaded["applied_backup_preferences"] =
        json!([{"operation_id":"uploaded","input_digest":digest()}]);
    let typed: StoreData = serde_json::from_value(uploaded).unwrap();
    first.save(&typed).unwrap();
    assert!(
        !first
            .backup_preferences_applied("uploaded", &digest())
            .unwrap()
    );
    assert!(raw(&directory).get("applied_backup_preferences").is_none());
}

#[test]
fn invalid_or_duplicate_private_markers_are_reported_and_preserved() {
    for markers in [json!([{"operation_id":"bad","input_digest":"short"}]),json!([{"operation_id":"same","input_digest":digest()},{"operation_id":"same","input_digest":digest()}]),json!([{"operation_id":"bad\ncontrol","input_digest":digest()}]),json!([{"operation_id":"bad","input_digest":digest(),"approval":true}]),json!((0..65).map(|index| json!({"operation_id":format!("marker-{index}"),"input_digest":digest()})).collect::<Vec<_>>())] {
        let directory = directory(); let first = store(&directory); let envelope = json!({"version":1,"data":StoreData::default(),"applied_backup_preferences":markers}); let bytes = serde_json::to_vec(&envelope).unwrap(); fs::write(state(&directory), &bytes).unwrap();
        assert!(first.load().is_err()); assert!(first.backup_preferences_applied("new", &digest()).is_err()); assert!(first.apply_backup_preferences("new", &digest(), &StoreData::default()).is_err()); assert!(first.save(&StoreData::default()).is_err()); assert_eq!(fs::read(state(&directory)).unwrap(), bytes);
    }
}

#[test]
fn application_history_is_bounded_and_survives_ordinary_preference_saves() {
    let directory = directory();
    let first = store(&directory);
    for index in 0..65 {
        let mut candidate = first.load().unwrap();
        candidate.volume = index as f32 / 100.0;
        first
            .apply_backup_preferences(&format!("bounded-{index}"), &digest(), &candidate)
            .unwrap();
    }
    let document = raw(&directory);
    let markers = document["applied_backup_preferences"].as_array().unwrap();
    assert_eq!(markers.len(), 64);
    assert_eq!(markers[0]["operation_id"], "bounded-1");
    assert_eq!(markers[63]["operation_id"], "bounded-64");
    assert!(
        !first
            .backup_preferences_applied("bounded-0", &digest())
            .unwrap()
    );
    assert!(
        first
            .backup_preferences_applied("bounded-64", &digest())
            .unwrap()
    );
    let mut changed = first.load().unwrap();
    changed.volume = 0.99;
    first.save(&changed).unwrap();
    drop(first);
    let reopened = store(&directory);
    assert_eq!(reopened.load().unwrap().volume, 0.99);
    assert_eq!(
        raw(&directory)["applied_backup_preferences"],
        document["applied_backup_preferences"]
    );
    assert!(
        reopened
            .backup_preferences_applied("bounded-64", &digest())
            .unwrap()
    );
}

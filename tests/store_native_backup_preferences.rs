use lx_gpui_player::{
    source::import_script,
    store::{BackupPreferencesConflict, ProjectStore, StoreData},
};
use std::fs;

fn digest() -> String {
    "c".repeat(64)
}

#[test]
fn empty_effect_inventory_allows_preference_changes_across_legacy_optional_field() {
    let directory = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
    let store = ProjectStore::open(directory.path().to_path_buf()).unwrap();
    store.save(&StoreData::default()).unwrap();
    let mut candidate = store.load().unwrap();
    let mut effects = lx_gpui_player::backend::EffectsPreferences::default();
    effects.settings.convolution.main_gain = 0.5;
    candidate.effects = Some(effects);
    store
        .apply_native_backup_preferences("effect-settings-native", &digest(), &candidate)
        .unwrap();
    assert_eq!(
        store
            .load()
            .unwrap()
            .effects
            .unwrap()
            .settings
            .convolution
            .main_gain,
        0.5
    );
    let mut candidate = store.load().unwrap();
    candidate.effects = None;
    store
        .apply_backup_preferences("effect-settings-legacy", &digest(), &candidate)
        .unwrap();
    assert!(store.load().unwrap().effects.is_none());
}

#[test]
fn native_sources_are_unapproved_and_marker_replay_preserves_later_settings() {
    let directory = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
    let store = ProjectStore::open(directory.path().to_path_buf()).unwrap();
    let mut approved = import_script("/* @name Existing */\nconst existing = 1;").unwrap();
    approved.approve().unwrap();
    let initial = StoreData {
        selected: Some(approved.sha256.clone()),
        sources: vec![approved],
        local_files: vec![directory.path().join("selected.wav")],
        ..StoreData::default()
    };
    store.save(&initial).unwrap();
    let mut candidate = store.load().unwrap();
    candidate.volume = 0.2;
    candidate
        .sources
        .push(import_script("/* @name Archived */\nconst archived = 2;").unwrap());
    store
        .apply_native_backup_preferences("native-1", &digest(), &candidate)
        .unwrap();
    let saved = store.load().unwrap();
    assert!(saved.sources[0].require_approval().is_ok());
    assert!(saved.sources[1].require_approval().is_err());
    assert_eq!(saved.selected, initial.selected);
    assert_eq!(saved.local_files, initial.local_files);
    assert_eq!(saved.volume, 0.2);
    let mut later = saved;
    later.volume = 0.9;
    later.sources[1].approve().unwrap();
    store.save(&later).unwrap();
    let bytes = fs::read(directory.path().join("player-state.json")).unwrap();
    drop(store);
    let store = ProjectStore::open(directory.path().to_path_buf()).unwrap();
    store
        .apply_native_backup_preferences("native-1", &digest(), &candidate)
        .unwrap();
    assert_eq!(
        fs::read(directory.path().join("player-state.json")).unwrap(),
        bytes
    );
    assert_eq!(store.load().unwrap().volume, 0.9);
    assert!(store.load().unwrap().sources[1].require_approval().is_ok());
    assert!(
        store
            .apply_native_backup_preferences("native-1", &"d".repeat(64), &candidate)
            .unwrap_err()
            .is::<BackupPreferencesConflict>()
    );
}

#[test]
fn native_import_rejects_approval_tampering_and_authority_changes_without_writing() {
    let directory = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
    let store = ProjectStore::open(directory.path().to_path_buf()).unwrap();
    let mut approved = import_script("/* @name Existing */\nconst existing = 1;").unwrap();
    approved.approve().unwrap();
    let initial = StoreData {
        selected: Some(approved.sha256.clone()),
        sources: vec![approved],
        local_files: vec![directory.path().join("selected.wav")],
        ..StoreData::default()
    };
    store.save(&initial).unwrap();
    let current = store.load().unwrap();
    let bytes = fs::read(directory.path().join("player-state.json")).unwrap();
    let archived = import_script("/* @name Archive */\nconst archive = 2;").unwrap();
    let mut variants = Vec::new();
    for change in 0..8 {
        let mut candidate = current.clone();
        candidate.sources.push(archived.clone());
        match change {
            0 => candidate.sources[1].approve().unwrap(),
            1 => candidate.sources[1].approval_policy_version = 1,
            2 => candidate.sources[1].metadata.name = "tampered".into(),
            3 => candidate.sources[1].sha256 = "e".repeat(64),
            4 => candidate.sources.push(archived.clone()),
            5 => candidate.selected = Some(archived.sha256.clone()),
            6 => candidate.local_files.clear(),
            7 => candidate.sources[0].approved = false,
            _ => unreachable!(),
        }
        variants.push(candidate);
    }
    for (index, candidate) in variants.iter().enumerate() {
        let operation = format!("invalid-{index}");
        assert!(
            store
                .apply_native_backup_preferences(&operation, &digest(), candidate)
                .is_err()
        );
        assert!(
            !store
                .backup_preferences_applied(&operation, &digest())
                .unwrap()
        );
        assert_eq!(
            fs::read(directory.path().join("player-state.json")).unwrap(),
            bytes
        );
    }
    let mut candidate = current.clone();
    candidate.sources.push(archived);
    assert!(
        store
            .apply_backup_preferences("lx-rejects-new-source", &digest(), &candidate)
            .is_err()
    );
}

#[test]
fn failed_native_save_leaves_existing_state_and_no_applied_marker() {
    let directory = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
    let store = ProjectStore::open(directory.path().to_path_buf()).unwrap();
    store.save(&StoreData::default()).unwrap();
    let bytes = fs::read(directory.path().join("player-state.json")).unwrap();
    fs::create_dir(directory.path().join("player-state.json.bak")).unwrap();
    let mut candidate = store.load().unwrap();
    candidate
        .sources
        .push(import_script("/* @name Archive */\nconst archive = 2;").unwrap());
    assert!(
        store
            .apply_native_backup_preferences("failed", &digest(), &candidate)
            .is_err()
    );
    assert_eq!(
        fs::read(directory.path().join("player-state.json")).unwrap(),
        bytes
    );
    assert!(
        !store
            .backup_preferences_applied("failed", &digest())
            .unwrap()
    );
}

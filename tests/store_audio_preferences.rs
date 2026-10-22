use lx_gpui_player::{
    domain::audio_output::{AudioPreferences, MAX_AUDIO_DEVICE_ID_BYTES, OutputSelection},
    store::{ProjectStore, StoreData},
};
use serde_json::json;
use std::{fs, path::PathBuf};
use tempfile::TempDir;

const STATE_NAME: &str = "player-state.json";
const BACKUP_NAME: &str = "player-state.json.bak";

fn profile() -> TempDir {
    let base = std::env::temp_dir().join("lx-native-store-audio-preferences-tests");
    fs::create_dir_all(&base).unwrap();
    tempfile::Builder::new()
        .prefix("state-")
        .tempdir_in(base)
        .unwrap()
}

fn device_selection() -> OutputSelection {
    OutputSelection::Device {
        id: cpal::DeviceId(cpal::ALL_HOSTS[0], "persisted-output-fixture".into()).to_string(),
    }
}

#[test]
fn version_one_state_without_audio_preferences_loads_without_rewriting() {
    let directory = profile();
    let path = directory.path().join(STATE_NAME);
    let legacy = br#"{
  "version": 1,
  "data": {
    "sources": [],
    "selected": null,
    "local_files": ["local-music.wav"],
    "volume": 0.37
  }
}"#;
    fs::write(&path, legacy).unwrap();
    let store = ProjectStore::open(directory.path().into()).unwrap();
    let mut data = store.load().unwrap();
    assert_eq!(data.audio_preferences, AudioPreferences::default());
    assert_eq!(data.volume, 0.37);
    assert_eq!(data.local_files, vec![PathBuf::from("local-music.wav")]);
    assert_eq!(fs::read(&path).unwrap(), legacy);
    assert!(!directory.path().join(BACKUP_NAME).exists());

    data.audio_preferences.muted = true;
    store.save(&data).unwrap();
    let current: serde_json::Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
    assert_eq!(current["version"], 1);
    assert_eq!(
        current["data"]["audio_preferences"]["selection"],
        json!({"type":"default"})
    );
    assert_eq!(current["data"]["audio_preferences"]["muted"], true);
    assert_eq!(
        fs::read(directory.path().join(BACKUP_NAME)).unwrap(),
        legacy
    );
}

#[test]
fn desired_device_mute_and_pause_policy_survive_store_reopen() {
    let directory = profile();
    let preferences = AudioPreferences {
        selection: device_selection(),
        muted: true,
        pause_on_device_change: true,
    };
    let data = StoreData {
        volume: 0.43,
        audio_preferences: preferences.clone(),
        ..Default::default()
    };
    {
        let store = ProjectStore::open(directory.path().into()).unwrap();
        store.save(&data).unwrap();
    }
    let reopened = ProjectStore::open(directory.path().into()).unwrap();
    let restored = reopened.load().unwrap();
    assert_eq!(restored.audio_preferences, preferences);
    assert_eq!(restored.volume, 0.43);
    let encoded: serde_json::Value =
        serde_json::from_slice(&fs::read(directory.path().join(STATE_NAME)).unwrap()).unwrap();
    let OutputSelection::Device { id } = &preferences.selection else {
        unreachable!();
    };
    assert_eq!(
        encoded["data"]["audio_preferences"]["selection"],
        json!({"type":"device","id":id})
    );
}

#[test]
fn invalid_device_ids_do_not_replace_primary_or_backup() {
    let directory = profile();
    let store = ProjectStore::open(directory.path().into()).unwrap();
    store.save(&StoreData::default()).unwrap();
    let baseline = StoreData {
        volume: 0.29,
        audio_preferences: AudioPreferences {
            selection: device_selection(),
            ..Default::default()
        },
        ..Default::default()
    };
    store.save(&baseline).unwrap();
    let primary = fs::read(directory.path().join(STATE_NAME)).unwrap();
    let backup = fs::read(directory.path().join(BACKUP_NAME)).unwrap();
    for id in [
        String::new(),
        "device-without-host".into(),
        "unsupported-host:device".into(),
        format!("{}:", cpal::ALL_HOSTS[0]),
        "wasapi:device\nidentity".into(),
        "wasapi:device\0identity".into(),
        "wasapi:device\u{0085}identity".into(),
        "x".repeat(MAX_AUDIO_DEVICE_ID_BYTES + 1),
        "设".repeat(MAX_AUDIO_DEVICE_ID_BYTES / 3 + 1),
    ] {
        let mut rejected = baseline.clone();
        rejected.audio_preferences.selection = OutputSelection::Device { id };
        rejected.audio_preferences.muted = true;
        rejected.volume = 0.91;
        assert!(store.save(&rejected).is_err());
        assert_eq!(
            fs::read(directory.path().join(STATE_NAME)).unwrap(),
            primary
        );
        assert_eq!(
            fs::read(directory.path().join(BACKUP_NAME)).unwrap(),
            backup
        );
        assert_eq!(
            store.load().unwrap().audio_preferences,
            baseline.audio_preferences
        );
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 2);
    }
}

#[test]
fn invalid_stored_audio_preferences_are_reported_and_preserved() {
    let directory = profile();
    let path = directory.path().join(STATE_NAME);
    let envelope = json!({
        "version": 1,
        "data": {
            "sources": [],
            "selected": null,
            "local_files": [],
            "volume": 0.61,
            "audio_preferences": {
                "selection": {"type":"device", "id":""},
                "muted": false,
                "pause_on_device_change": false
            }
        }
    });
    let bytes = serde_json::to_vec_pretty(&envelope).unwrap();
    fs::write(&path, &bytes).unwrap();
    let store = ProjectStore::open(directory.path().into()).unwrap();
    assert!(
        store
            .load()
            .unwrap_err()
            .to_string()
            .contains("device identity")
    );
    assert!(store.save(&StoreData::default()).is_err());
    assert_eq!(fs::read(&path).unwrap(), bytes);
    assert!(!directory.path().join(BACKUP_NAME).exists());
    assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 1);
}

#[test]
fn damaged_backup_after_interrupted_replacement_cannot_be_hidden_by_a_save() {
    let directory = profile();
    let backup = directory.path().join(BACKUP_NAME);
    let invalid_id = format!("{}:", cpal::ALL_HOSTS[0]);
    let envelope = json!({
        "version": 1,
        "data": {
            "sources": [],
            "selected": null,
            "local_files": [],
            "volume": 0.61,
            "audio_preferences": {
                "selection": {"type":"device", "id":invalid_id},
                "muted": false,
                "pause_on_device_change": false
            }
        }
    });
    let bytes = serde_json::to_vec_pretty(&envelope).unwrap();
    fs::write(&backup, &bytes).unwrap();
    let store = ProjectStore::open(directory.path().into()).unwrap();
    assert!(store.load().is_err());
    assert!(store.save(&StoreData::default()).is_err());
    assert_eq!(fs::read(&backup).unwrap(), bytes);
    assert!(!directory.path().join(STATE_NAME).exists());
    assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 1);
}

#[test]
fn mute_and_output_selection_changes_preserve_saved_volume() {
    let directory = profile();
    let store = ProjectStore::open(directory.path().into()).unwrap();
    let mut data = StoreData {
        volume: 0.64,
        ..Default::default()
    };
    store.save(&data).unwrap();
    data.audio_preferences.muted = true;
    data.audio_preferences.selection = device_selection();
    data.audio_preferences.pause_on_device_change = true;
    store.save(&data).unwrap();
    data = ProjectStore::open(directory.path().into())
        .unwrap()
        .load()
        .unwrap();
    assert!(data.audio_preferences.muted);
    assert_eq!(data.volume, 0.64);

    data.volume = 0.22;
    store.save(&data).unwrap();
    data = store.load().unwrap();
    assert!(data.audio_preferences.muted);
    assert_eq!(data.volume, 0.22);
    data.audio_preferences.muted = false;
    data.audio_preferences.selection = OutputSelection::Default;
    store.save(&data).unwrap();
    let restored = ProjectStore::open(directory.path().into())
        .unwrap()
        .load()
        .unwrap();
    assert!(!restored.audio_preferences.muted);
    assert_eq!(
        restored.audio_preferences.selection,
        OutputSelection::Default
    );
    assert!(restored.audio_preferences.pause_on_device_change);
    assert_eq!(restored.volume, 0.22);
}

#[test]
fn partial_audio_preferences_default_new_fields_without_rewriting() {
    let directory = profile();
    let path = directory.path().join(STATE_NAME);
    let legacy = br#"{"version":1,"data":{"sources":[],"selected":null,"local_files":[],"volume":0.5,"audio_preferences":{"muted":true}}}"#;
    fs::write(&path, legacy).unwrap();
    let restored = ProjectStore::open(directory.path().into())
        .unwrap()
        .load()
        .unwrap();
    assert_eq!(
        restored.audio_preferences,
        AudioPreferences {
            muted: true,
            ..Default::default()
        }
    );
    assert_eq!(fs::read(&path).unwrap(), legacy);
}

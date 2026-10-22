use lx_gpui_player::{
    domain::{
        audio_output::{AudioPreferences, OutputSelection},
        equalizer::{EQUALIZER_BAND_COUNT, EqualizerPreset, EqualizerSettings},
    },
    store::{ProjectStore, StoreData},
};
use serde_json::json;
use std::{fs, path::PathBuf};
use tempfile::TempDir;

const STATE_NAME: &str = "player-state.json";
const BACKUP_NAME: &str = "player-state.json.bak";

fn profile() -> TempDir {
    let base = std::env::temp_dir().join("lx-native-store-equalizer-tests");
    fs::create_dir_all(&base).unwrap();
    tempfile::Builder::new()
        .prefix("state-")
        .tempdir_in(base)
        .unwrap()
}

fn preferences() -> AudioPreferences {
    AudioPreferences {
        selection: OutputSelection::Device {
            id: cpal::DeviceId(cpal::ALL_HOSTS[0], "equalizer-persistence-fixture".into())
                .to_string(),
        },
        muted: true,
        pause_on_device_change: true,
    }
}

#[test]
fn legacy_state_loads_flat_equalizer_without_rewriting_and_preserves_prior_fields() {
    let directory = profile();
    let path = directory.path().join(STATE_NAME);
    let legacy = br#"{
  "version": 1,
  "data": {
    "sources": [],
    "selected": null,
    "local_files": ["equalizer-local-track.wav"],
    "volume": 0.37,
    "audio_preferences": {
      "selection": {"type":"default"},
      "muted": true,
      "pause_on_device_change": true
    }
  }
}"#;
    fs::write(&path, legacy).unwrap();
    let store = ProjectStore::open(directory.path().into()).unwrap();
    let mut restored = store.load().unwrap();
    assert_eq!(restored.equalizer.gains_db, [0.0; EQUALIZER_BAND_COUNT]);
    assert!(restored.equalizer.is_flat());
    assert_eq!(restored.volume, 0.37);
    assert!(restored.audio_preferences.muted);
    assert!(restored.audio_preferences.pause_on_device_change);
    assert_eq!(
        restored.local_files,
        vec![PathBuf::from("equalizer-local-track.wav")]
    );
    assert_eq!(fs::read(&path).unwrap(), legacy);
    assert!(!directory.path().join(BACKUP_NAME).exists());

    restored.equalizer = EqualizerPreset::Pop.settings();
    store.save(&restored).unwrap();
    let envelope: serde_json::Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
    assert_eq!(envelope["version"], 1);
    assert_eq!(
        envelope["data"]["equalizer"]["gains_db"],
        json!(restored.equalizer.gains_db)
    );
    assert_eq!(
        fs::read(directory.path().join(BACKUP_NAME)).unwrap(),
        legacy
    );
}

#[test]
fn fractional_equalizer_gains_survive_reopen_independently_of_volume_and_output_preferences() {
    let directory = profile();
    let equalizer = EqualizerSettings {
        gains_db: [-15.0, -6.0, 0.0, 6.0, 15.0, 0.125, -2.75, 14.5, -14.5, 1.25],
    };
    let data = StoreData {
        volume: 0.43,
        audio_preferences: preferences(),
        equalizer,
        ..Default::default()
    };
    {
        let store = ProjectStore::open(directory.path().into()).unwrap();
        store.save(&data).unwrap();
    }
    let store = ProjectStore::open(directory.path().into()).unwrap();
    let mut restored = store.load().unwrap();
    assert_eq!(restored.equalizer, equalizer);
    assert_eq!(restored.volume, data.volume);
    assert_eq!(restored.audio_preferences, data.audio_preferences);

    restored.volume = 0.21;
    restored.audio_preferences.muted = false;
    restored.audio_preferences.selection = OutputSelection::Default;
    store.save(&restored).unwrap();
    let reopened = ProjectStore::open(directory.path().into())
        .unwrap()
        .load()
        .unwrap();
    assert_eq!(reopened.equalizer, equalizer);
    assert_eq!(reopened.volume, 0.21);
    assert!(!reopened.audio_preferences.muted);
    assert_eq!(
        reopened.audio_preferences.selection,
        OutputSelection::Default
    );
}

#[test]
fn every_fixed_preset_saves_exact_gains_and_flat_reset_survives_restart() {
    let directory = profile();
    for preset in EqualizerPreset::ALL {
        let store = ProjectStore::open(directory.path().into()).unwrap();
        let settings = preset.settings();
        let data = StoreData {
            volume: 0.52,
            equalizer: settings,
            ..Default::default()
        };
        store.save(&data).unwrap();
        drop(store);
        let restored = ProjectStore::open(directory.path().into())
            .unwrap()
            .load()
            .unwrap();
        assert_eq!(restored.equalizer, settings);
        assert_eq!(restored.volume, 0.52);
        let envelope: serde_json::Value =
            serde_json::from_slice(&fs::read(directory.path().join(STATE_NAME)).unwrap()).unwrap();
        assert_eq!(
            envelope["data"]["equalizer"]["gains_db"],
            json!(settings.gains_db)
        );
    }
    let store = ProjectStore::open(directory.path().into()).unwrap();
    let mut data = store.load().unwrap();
    data.equalizer = EqualizerPreset::Flat.settings();
    store.save(&data).unwrap();
    assert_eq!(
        ProjectStore::open(directory.path().into())
            .unwrap()
            .load()
            .unwrap()
            .equalizer,
        EqualizerSettings::default()
    );
}

#[test]
fn every_invalid_gain_leaves_primary_backup_and_other_settings_unchanged() {
    let directory = profile();
    let store = ProjectStore::open(directory.path().into()).unwrap();
    store.save(&StoreData::default()).unwrap();
    let baseline = StoreData {
        volume: 0.64,
        audio_preferences: preferences(),
        equalizer: EqualizerPreset::Classical.settings(),
        ..Default::default()
    };
    store.save(&baseline).unwrap();
    let primary = fs::read(directory.path().join(STATE_NAME)).unwrap();
    let backup = fs::read(directory.path().join(BACKUP_NAME)).unwrap();
    for index in 0..EQUALIZER_BAND_COUNT {
        for gain in [
            f64::NAN,
            f64::INFINITY,
            f64::NEG_INFINITY,
            -15.000001,
            15.000001,
        ] {
            let mut rejected = baseline.clone();
            rejected.equalizer.gains_db[index] = gain;
            rejected.volume = 0.19;
            rejected.audio_preferences.muted = false;
            assert!(store.save(&rejected).is_err());
            assert_eq!(
                fs::read(directory.path().join(STATE_NAME)).unwrap(),
                primary
            );
            assert_eq!(
                fs::read(directory.path().join(BACKUP_NAME)).unwrap(),
                backup
            );
            let restored = store.load().unwrap();
            assert_eq!(restored.equalizer, baseline.equalizer);
            assert_eq!(restored.audio_preferences, baseline.audio_preferences);
            assert_eq!(restored.volume, baseline.volume);
            assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 2);
        }
    }
}

#[test]
fn invalid_stored_equalizer_shapes_or_values_are_reported_without_replacement() {
    let directory = profile();
    let path = directory.path().join(STATE_NAME);
    let store = ProjectStore::open(directory.path().into()).unwrap();
    for equalizer in [
        json!({"gains_db":vec![0.0; 9]}),
        json!({"gains_db":vec![0.0; 11]}),
        json!({"gains_db":[0.0,0.0,0.0,0.0,0.0,0.0,0.0,0.0,0.0,15.01]}),
        json!({"gains_db":[null,0.0,0.0,0.0,0.0,0.0,0.0,0.0,0.0,0.0]}),
        json!({"gains_db":"invalid"}),
        json!({"gains_db":vec![0.0; 10], "preamp":0.0}),
    ] {
        let bytes = serde_json::to_vec_pretty(&json!({
            "version":1,
            "data":{
                "sources":[], "selected":null, "local_files":[], "volume":0.61,
                "equalizer":equalizer
            }
        }))
        .unwrap();
        fs::write(&path, &bytes).unwrap();
        assert!(store.load().is_err());
        assert!(store.save(&StoreData::default()).is_err());
        assert_eq!(fs::read(&path).unwrap(), bytes);
        assert!(!directory.path().join(BACKUP_NAME).exists());
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 1);
    }
}

#[test]
fn invalid_recovery_backup_cannot_be_hidden_by_new_default_settings() {
    let directory = profile();
    let backup = directory.path().join(BACKUP_NAME);
    let bytes = serde_json::to_vec_pretty(&json!({
        "version":1,
        "data":{
            "sources":[], "selected":null, "local_files":[], "volume":0.61,
            "equalizer":{"gains_db":[0.0,0.0,0.0,0.0,0.0,0.0,0.0,0.0,0.0,-15.01]}
        }
    }))
    .unwrap();
    fs::write(&backup, &bytes).unwrap();
    let store = ProjectStore::open(directory.path().into()).unwrap();
    assert!(store.load().is_err());
    assert!(store.save(&StoreData::default()).is_err());
    assert_eq!(fs::read(&backup).unwrap(), bytes);
    assert!(!directory.path().join(STATE_NAME).exists());
    assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 1);
}

#[test]
fn interrupted_replacement_restores_equalizer_from_backup_without_rewriting() {
    let directory = profile();
    let store = ProjectStore::open(directory.path().into()).unwrap();
    let data = StoreData {
        equalizer: EqualizerPreset::Electronic.settings(),
        volume: 0.33,
        ..Default::default()
    };
    store.save(&data).unwrap();
    let primary = directory.path().join(STATE_NAME);
    let backup = directory.path().join(BACKUP_NAME);
    fs::rename(&primary, &backup).unwrap();
    let bytes = fs::read(&backup).unwrap();
    let restored = ProjectStore::open(directory.path().into())
        .unwrap()
        .load()
        .unwrap();
    assert_eq!(restored.equalizer, data.equalizer);
    assert_eq!(restored.volume, data.volume);
    assert_eq!(fs::read(&backup).unwrap(), bytes);
    assert!(!primary.exists());
}

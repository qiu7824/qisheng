use lx_gpui_player::{
    domain::{
        audio_output::{AudioPreferences, OutputSelection},
        equalizer::EqualizerPreset,
        playback_processing::PlaybackProcessingSettings,
    },
    source::import_script,
    store::{ProjectStore, StoreData},
};
use serde_json::{Value, json};
use std::{fs, path::PathBuf};
use tempfile::TempDir;

const STATE_NAME: &str = "player-state.json";
const BACKUP_NAME: &str = "player-state.json.bak";

fn profile() -> TempDir {
    let base = std::env::temp_dir().join("lx-native-store-playback-processing-tests");
    fs::create_dir_all(&base).unwrap();
    tempfile::Builder::new()
        .prefix("state-")
        .tempdir_in(base)
        .unwrap()
}

fn prior_settings() -> StoreData {
    let mut source = import_script(
        "/* @name Playback processing storage fixture */\nglobalThis.storageFixture = true;",
    )
    .unwrap();
    source.approve().unwrap();
    StoreData {
        selected: Some(source.sha256.clone()),
        sources: vec![source],
        local_files: vec![
            PathBuf::from("processing-left.wav"),
            PathBuf::from("processing-right.flac"),
        ],
        volume: 0.43,
        audio_preferences: AudioPreferences {
            selection: OutputSelection::Device {
                id: cpal::DeviceId(cpal::ALL_HOSTS[0], "processing-storage-fixture".into())
                    .to_string(),
            },
            muted: true,
            pause_on_device_change: true,
        },
        equalizer: EqualizerPreset::Rock.settings(),
        ..Default::default()
    }
}

fn assert_unrelated_fields(actual: &StoreData, expected: &StoreData) {
    assert_eq!(actual.equalizer, expected.equalizer);
    assert_eq!(actual.audio_preferences, expected.audio_preferences);
    assert_eq!(actual.volume, expected.volume);
    assert_eq!(actual.selected, expected.selected);
    assert_eq!(actual.local_files, expected.local_files);
    assert_eq!(
        serde_json::to_value(&actual.sources).unwrap(),
        serde_json::to_value(&expected.sources).unwrap()
    );
    actual.sources[0].require_approval().unwrap();
}

#[test]
fn legacy_profile_defaults_processing_without_rewriting_or_changing_existing_settings() {
    let directory = profile();
    let path = directory.path().join(STATE_NAME);
    let prior = prior_settings();
    let mut envelope = json!({"version":1,"data":prior});
    envelope["data"]
        .as_object_mut()
        .unwrap()
        .remove("playback_processing");
    let legacy = serde_json::to_vec_pretty(&envelope).unwrap();
    fs::write(&path, &legacy).unwrap();
    let store = ProjectStore::open(directory.path().into()).unwrap();
    let mut restored = store.load().unwrap();
    assert_eq!(
        restored.playback_processing,
        PlaybackProcessingSettings {
            playback_rate: 1.0,
            preserves_pitch: true,
            pitch_ratio: 1.0,
        }
    );
    assert_unrelated_fields(&restored, &prior);
    assert_eq!(fs::read(&path).unwrap(), legacy);
    assert!(!directory.path().join(BACKUP_NAME).exists());

    restored.playback_processing.playback_rate = 1.5;
    store.save(&restored).unwrap();
    let current: Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
    assert_eq!(current["version"], 1);
    assert_eq!(
        current["data"]["playback_processing"],
        json!({
            "playback_rate":1.5,"preserves_pitch":true,"pitch_ratio":1.0
        })
    );
    assert_eq!(
        fs::read(directory.path().join(BACKUP_NAME)).unwrap(),
        legacy
    );
}

#[test]
fn tempo_pitch_and_preservation_restore_exactly_and_remain_independent_of_other_fields() {
    let directory = profile();
    let prior = prior_settings();
    for settings in [
        PlaybackProcessingSettings {
            playback_rate: 0.5,
            preserves_pitch: true,
            pitch_ratio: 0.5,
        },
        PlaybackProcessingSettings {
            playback_rate: 2.0,
            preserves_pitch: false,
            pitch_ratio: 1.5,
        },
        PlaybackProcessingSettings {
            playback_rate: 1.125,
            preserves_pitch: true,
            pitch_ratio: 0.9375,
        },
        PlaybackProcessingSettings {
            playback_rate: 1.75,
            preserves_pitch: false,
            pitch_ratio: 1.25,
        },
        PlaybackProcessingSettings::default(),
    ] {
        let mut data = prior.clone();
        data.playback_processing = settings;
        let store = ProjectStore::open(directory.path().into()).unwrap();
        store.save(&data).unwrap();
        drop(store);
        let reopened = ProjectStore::open(directory.path().into())
            .unwrap()
            .load()
            .unwrap();
        assert_eq!(reopened.playback_processing, settings);
        assert_unrelated_fields(&reopened, &prior);
    }
    let store = ProjectStore::open(directory.path().into()).unwrap();
    let mut data = store.load().unwrap();
    data.playback_processing = PlaybackProcessingSettings {
        playback_rate: 1.625,
        preserves_pitch: false,
        pitch_ratio: 0.75,
    };
    store.save(&data).unwrap();
    data.volume = 0.21;
    data.audio_preferences.muted = false;
    data.equalizer = EqualizerPreset::Vocal.settings();
    store.save(&data).unwrap();
    let restored = ProjectStore::open(directory.path().into())
        .unwrap()
        .load()
        .unwrap();
    assert_eq!(restored.playback_processing, data.playback_processing);
    assert_unrelated_fields(&restored, &data);
}

#[test]
fn invalid_rates_and_pitch_ratios_preserve_primary_backup_and_the_entire_saved_profile() {
    let directory = profile();
    let store = ProjectStore::open(directory.path().into()).unwrap();
    let mut baseline = prior_settings();
    store.save(&baseline).unwrap();
    baseline.playback_processing = PlaybackProcessingSettings {
        playback_rate: 1.75,
        preserves_pitch: true,
        pitch_ratio: 0.625,
    };
    store.save(&baseline).unwrap();
    let primary = fs::read(directory.path().join(STATE_NAME)).unwrap();
    let backup = fs::read(directory.path().join(BACKUP_NAME)).unwrap();
    for (rate, values) in [
        (
            true,
            [
                f64::NAN,
                f64::INFINITY,
                f64::NEG_INFINITY,
                0.0,
                0.499999,
                2.000001,
            ],
        ),
        (
            false,
            [
                f64::NAN,
                f64::INFINITY,
                f64::NEG_INFINITY,
                0.0,
                0.499999,
                1.500001,
            ],
        ),
    ] {
        for value in values {
            let mut rejected = baseline.clone();
            if rate {
                rejected.playback_processing.playback_rate = value;
            } else {
                rejected.playback_processing.pitch_ratio = value;
            }
            rejected.playback_processing.preserves_pitch = false;
            rejected.volume = 0.19;
            rejected.audio_preferences.muted = false;
            rejected.equalizer = EqualizerPreset::Dance.settings();
            rejected
                .local_files
                .push(PathBuf::from("uncommitted-track.wav"));
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
            assert_eq!(restored.playback_processing, baseline.playback_processing);
            assert_unrelated_fields(&restored, &baseline);
            assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 2);
        }
    }
}

#[test]
fn invalid_first_processing_save_does_not_create_player_state() {
    let directory = profile();
    let store = ProjectStore::open(directory.path().into()).unwrap();
    let mut data = prior_settings();
    data.playback_processing.playback_rate = f64::NAN;
    assert!(store.save(&data).is_err());
    data.playback_processing.playback_rate = 1.0;
    data.playback_processing.pitch_ratio = f64::INFINITY;
    assert!(store.save(&data).is_err());
    assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 0);
}

#[test]
fn invalid_stored_processing_shapes_and_values_are_reported_and_never_overwritten() {
    let directory = profile();
    let path = directory.path().join(STATE_NAME);
    let store = ProjectStore::open(directory.path().into()).unwrap();
    let prior = prior_settings();
    for settings in [
        json!({"playback_rate":0.49,"preserves_pitch":true,"pitch_ratio":1.0}),
        json!({"playback_rate":2.01,"preserves_pitch":true,"pitch_ratio":1.0}),
        json!({"playback_rate":1.0,"preserves_pitch":true,"pitch_ratio":0.49}),
        json!({"playback_rate":1.0,"preserves_pitch":true,"pitch_ratio":1.51}),
        json!({"playback_rate":null,"preserves_pitch":true,"pitch_ratio":1.0}),
        json!({"playback_rate":1.0,"preserves_pitch":"true","pitch_ratio":1.0}),
        json!({"playback_rate":1.0,"preserves_pitch":true}),
        json!({"playback_rate":1.0,"preserves_pitch":true,"pitch_ratio":1.0,"algorithm":"invalid"}),
    ] {
        let mut envelope = json!({"version":1,"data":prior});
        envelope["data"]["playback_processing"] = settings;
        let bytes = serde_json::to_vec_pretty(&envelope).unwrap();
        fs::write(&path, &bytes).unwrap();
        assert!(store.load().is_err());
        assert!(store.save(&prior).is_err());
        assert_eq!(fs::read(&path).unwrap(), bytes);
        assert!(!directory.path().join(BACKUP_NAME).exists());
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 1);
    }
}

#[test]
fn interrupted_replacement_loads_processing_from_backup_without_rewriting() {
    let directory = profile();
    let store = ProjectStore::open(directory.path().into()).unwrap();
    let mut data = prior_settings();
    data.playback_processing = PlaybackProcessingSettings {
        playback_rate: 1.5,
        preserves_pitch: false,
        pitch_ratio: 0.875,
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
    assert_eq!(restored.playback_processing, data.playback_processing);
    assert_unrelated_fields(&restored, &data);
    assert_eq!(fs::read(&backup).unwrap(), bytes);
    assert!(!primary.exists());
}

#[test]
fn invalid_recovery_backup_cannot_be_hidden_by_default_processing_or_volume_changes() {
    let directory = profile();
    let backup = directory.path().join(BACKUP_NAME);
    let prior = prior_settings();
    let mut envelope = json!({"version":1,"data":prior});
    envelope["data"]["playback_processing"]["pitch_ratio"] = json!(1.51);
    let bytes = serde_json::to_vec_pretty(&envelope).unwrap();
    fs::write(&backup, &bytes).unwrap();
    let store = ProjectStore::open(directory.path().into()).unwrap();
    assert!(store.load().is_err());
    assert!(store.save(&StoreData::default()).is_err());
    assert!(store.save(&prior).is_err());
    assert_eq!(fs::read(&backup).unwrap(), bytes);
    assert!(!directory.path().join(STATE_NAME).exists());
    assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 1);
}

use lx_gpui_player::{
    backup::{
        BackupEncoding, NativeBackup, NativeBackupOptions, decode_native, encode_native,
        settings::LxSettings,
    },
    domain::lists::ListData,
    playback::timeout_stop::TimeoutStopPreferences,
    store::StoreData,
};
use serde_json::json;

#[test]
fn lx_wait_and_minute_preferences_restore_without_a_live_deadline() {
    for input in [json!("30"), json!(30)] {
        let settings = LxSettings::from_value(
            &json!({
                "version":"2.1.0", "player.waitPlayEndStop":false,
                "player.waitPlayEndStopTime":input,
            }),
            false,
        )
        .unwrap();
        let restored = settings
            .preview()
            .unwrap()
            .project_store(&StoreData::default())
            .unwrap();
        assert_eq!(
            restored.timeout_stop,
            TimeoutStopPreferences {
                wait_for_track_end: false,
                last_seconds: Some(1800.0),
            }
        );
        let wire = serde_json::to_value(&restored).unwrap();
        assert!(wire["timeout_stop"].get("deadline").is_none());
    }
    for input in [json!("-1"), json!("30.5"), json!(0), json!(1441), json!({})] {
        assert!(
            LxSettings::from_value(&json!({"player.waitPlayEndStopTime":input}), false).is_err()
        );
    }
    let defaults = LxSettings::from_value(&json!({"player.waitPlayEndStopTime":""}), false)
        .unwrap()
        .preview()
        .unwrap()
        .project_store(&StoreData::default())
        .unwrap();
    assert!(defaults.timeout_stop.wait_for_track_end);
    assert!(defaults.timeout_stop.last_seconds.is_none());
}

#[test]
fn project_backup_preserves_native_seconds_and_rejects_unvalidated_timer_values() {
    let original = StoreData {
        timeout_stop: TimeoutStopPreferences {
            wait_for_track_end: false,
            last_seconds: Some(0.25),
        },
        ..Default::default()
    };
    let snapshot = NativeBackup::from_snapshot(
        &original,
        &ListData::default(),
        "",
        &[],
        &[],
        NativeBackupOptions::default(),
    )
    .unwrap();
    let encoded = encode_native(&snapshot, BackupEncoding::Json).unwrap();
    let restored = decode_native(&encoded, BackupEncoding::Json).unwrap();
    assert_eq!(restored.store.timeout_stop, original.timeout_stop);
    let mut invalid = original.clone();
    invalid.timeout_stop.last_seconds = Some(f64::NAN);
    assert!(
        NativeBackup::from_snapshot(
            &invalid,
            &ListData::default(),
            "",
            &[],
            &[],
            NativeBackupOptions::default()
        )
        .is_err()
    );
    let mut wire: serde_json::Value = serde_json::from_slice(&encoded).unwrap();
    wire["store"]["timeout_stop"]["last_seconds"] = json!(-1);
    assert!(decode_native(&serde_json::to_vec(&wire).unwrap(), BackupEncoding::Json).is_err());
}

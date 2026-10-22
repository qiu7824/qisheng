//! Equalizer controls are checked against an independent oracle and actual decoded PCM.

#[path = "support/equalizer_fixture.rs"]
mod fixture;

use fixture::{
    RATE, Running, assert_media, cache_epoch, envelope, flat_rms, pcm, preset_mix_rms, reference,
    settled_rms, single_section_mix_rms, temporary_directory, wait,
};
use lx_gpui_player::{
    audio::PlaybackStatus,
    backend::{
        BackendCommand, BackendHandle, BackendQuery, BackendQueryResult, ErrorKind, ReceiptState,
    },
    domain::equalizer::{
        EqualizerPreset, EqualizerPresetInfo, EqualizerSettings, EqualizerSnapshot,
    },
    store::ProjectStore,
};
use std::{fs, thread, time::Duration};

fn settings(backend: &BackendHandle) -> EqualizerSettings {
    match backend.query(BackendQuery::EqualizerSettings).unwrap() {
        BackendQueryResult::EqualizerSettings(settings) => settings,
        result => panic!("unexpected equalizer settings result: {result:?}"),
    }
}

fn state(backend: &BackendHandle) -> EqualizerSnapshot {
    match backend.query(BackendQuery::EqualizerState).unwrap() {
        BackendQueryResult::EqualizerState(state) => state,
        result => panic!("unexpected equalizer rendering state: {result:?}"),
    }
}

fn presets(backend: &BackendHandle) -> Vec<EqualizerPresetInfo> {
    match backend.query(BackendQuery::EqualizerPresets).unwrap() {
        BackendQueryResult::EqualizerPresets(presets) => presets,
        result => panic!("unexpected equalizer presets result: {result:?}"),
    }
}

fn center_settings(gain: f64) -> EqualizerSettings {
    let mut settings = EqualizerSettings::default();
    settings.gains_db[5] = gain;
    settings
}

fn acknowledged(backend: &BackendHandle, expected: EqualizerSettings) -> EqualizerSnapshot {
    wait(backend, |_| {
        let state = state(backend);
        state.desired == expected
            && state.applied == expected
            && state.requested_sequence == state.applied_sequence
            && state.transition_remaining_frames == 0
    });
    let state = state(backend);
    assert_eq!(state.sample_rate, Some(RATE));
    assert_eq!(state.non_finite_samples, 0);
    assert_eq!(state.clipped_samples, 0);
    state
}

fn assert_bank_unchanged(before: &EqualizerSnapshot, after: &EqualizerSnapshot) {
    assert_eq!(after.desired, before.desired);
    assert_eq!(after.applied, before.applied);
    assert_eq!(after.requested_sequence, before.requested_sequence);
    assert_eq!(after.applied_sequence, before.applied_sequence);
    assert_eq!(after.sample_rate, before.sample_rate);
    assert_eq!(after.active_bands, before.active_bands);
    assert_eq!(
        after.transition_remaining_frames,
        before.transition_remaining_frames
    );
}

#[test]
fn live_center_band_gain_matches_independent_mixed_pcm_response_without_media_reset() {
    let running = Running::new();
    let playing = running.play_mix();
    let baseline = settled_rms(
        &running.backend,
        playing.playback.position_seconds,
        0.1,
        flat_rms(),
    );
    let epoch = cache_epoch(&running.backend);
    let flat_state = acknowledged(&running.backend, EqualizerSettings::default());
    let boost = center_settings(6.0);
    let receipt = running
        .backend
        .execute(envelope(
            "live-boost",
            BackendCommand::SetEqualizer { settings: boost },
        ))
        .unwrap();
    assert_eq!(receipt.state, ReceiptState::Committed);
    assert_eq!(settings(&running.backend), boost);
    let boosted = settled_rms(
        &running.backend,
        baseline.playback.position_seconds,
        2.0,
        single_section_mix_rms(6.0),
    );
    let boosted_state = acknowledged(&running.backend, boost);
    assert!(boosted_state.applied_sequence > flat_state.applied_sequence);
    assert!(boosted_state.active_bands[5]);
    assert!(!boosted_state.active_bands[9]);
    assert_media(&baseline, &boosted);
    assert_eq!(boosted.playback.seek_seq, baseline.playback.seek_seq);
    assert_eq!(cache_epoch(&running.backend), epoch);
    assert!(pcm(&running.backend).rms > flat_rms() * 1.4);

    running.execute("live-flat", BackendCommand::ResetEqualizer);
    let reset = settled_rms(
        &running.backend,
        boosted.playback.position_seconds,
        0.25,
        flat_rms(),
    );
    acknowledged(&running.backend, EqualizerSettings::default());
    assert_media(&baseline, &reset);
    assert_eq!(reset.playback.seek_seq, baseline.playback.seek_seq);
    assert_eq!(cache_epoch(&running.backend), epoch);
}

#[test]
fn preset_inventory_and_commands_match_all_nine_fixed_upstream_vectors() {
    let running = Running::new();
    let playing = running.play_mix();
    let epoch = cache_epoch(&running.backend);
    let reference = reference();
    let goldens = reference["upstream_presets"].as_object().unwrap();
    assert_eq!(goldens.len(), 9);
    let inventory = presets(&running.backend);
    assert_eq!(inventory.len(), 10);
    assert_eq!(inventory[0].preset, EqualizerPreset::Flat);
    assert_eq!(inventory[0].name, "flat");
    assert_eq!(inventory[0].settings, EqualizerSettings::default());
    for (name, vector) in goldens {
        let expected = EqualizerSettings {
            gains_db: std::array::from_fn(|index| vector[index].as_f64().unwrap()),
        };
        let preset: EqualizerPreset = serde_json::from_value(serde_json::json!(name)).unwrap();
        let entries: Vec<_> = inventory
            .iter()
            .filter(|info| info.preset == preset)
            .collect();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, *name);
        assert_eq!(entries[0].settings, expected);
        running.execute(
            &format!("preset-{name}"),
            BackendCommand::SetEqualizerPreset { preset },
        );
        assert_eq!(settings(&running.backend), expected);
        acknowledged(&running.backend, expected);
        assert_media(&playing, &running.backend.snapshot());
        assert_eq!(cache_epoch(&running.backend), epoch);
    }
    running.execute(
        "settled-rock",
        BackendCommand::SetEqualizerPreset {
            preset: EqualizerPreset::Rock,
        },
    );
    let before = running.backend.snapshot();
    let rock = settled_rms(
        &running.backend,
        before.playback.position_seconds,
        3.0,
        preset_mix_rms("rock"),
    );
    assert_media(&playing, &rock);
    assert_eq!(rock.playback.seek_seq, playing.playback.seek_seq);
    assert_eq!(cache_epoch(&running.backend), epoch);
}

#[test]
fn mute_pause_seek_and_resume_apply_latest_equalizer_without_old_ringing_or_clock_reset() {
    let running = Running::new();
    let playing = running.play_mix();
    let epoch = cache_epoch(&running.backend);
    running.execute(
        "mute-equalizer-playback",
        BackendCommand::SetMute { muted: true },
    );
    let muted = wait(&running.backend, |state| {
        state.playback.status == PlaybackStatus::Playing
            && state.playback.position_seconds > playing.playback.position_seconds + 0.15
            && pcm(&running.backend).peak == 0.0
    });
    let boost = center_settings(6.0);
    running.execute(
        "equalizer-while-muted",
        BackendCommand::SetEqualizer { settings: boost },
    );
    acknowledged(&running.backend, boost);
    let still_muted = wait(&running.backend, |state| {
        state.playback.position_seconds > muted.playback.position_seconds + 0.15
            && pcm(&running.backend).rms == 0.0
            && pcm(&running.backend).peak == 0.0
    });
    assert_media(&playing, &still_muted);
    running.execute("pause-equalizer-playback", BackendCommand::Pause);
    let paused = wait(&running.backend, |state| {
        state.playback.status == PlaybackStatus::Paused
    });
    thread::sleep(Duration::from_millis(80));
    assert!(
        (running.backend.snapshot().playback.position_seconds - paused.playback.position_seconds)
            .abs()
            < 0.03
    );
    let cut = center_settings(-6.0);
    running.execute(
        "equalizer-while-paused",
        BackendCommand::SetEqualizer { settings: cut },
    );
    assert_eq!(settings(&running.backend), cut);
    running.execute(
        "seek-paused-equalizer",
        BackendCommand::Seek { seconds: 6.0 },
    );
    let sought = wait(&running.backend, |state| {
        state.playback.status == PlaybackStatus::Paused
            && state.playback.seek_seq == playing.playback.seek_seq + 1
            && (state.playback.position_seconds - 6.0).abs() < 0.03
            && pcm(&running.backend).peak == 0.0
    });
    running.execute(
        "unmute-paused-equalizer",
        BackendCommand::SetMute { muted: false },
    );
    thread::sleep(Duration::from_millis(80));
    assert_eq!(
        running.backend.snapshot().playback.status,
        PlaybackStatus::Paused
    );
    assert_eq!(pcm(&running.backend).peak, 0.0);
    running.execute("resume-equalizer-playback", BackendCommand::Resume);
    let resumed = settled_rms(&running.backend, 6.0, 2.0, single_section_mix_rms(-6.0));
    acknowledged(&running.backend, cut);
    assert_media(&playing, &resumed);
    assert_eq!(resumed.playback.seek_seq, sought.playback.seek_seq);
    assert_eq!(cache_epoch(&running.backend), epoch);
}

#[test]
fn legacy_profile_defaults_and_restart_restore_requested_gain_including_inactive_high_band() {
    let directory = temporary_directory();
    let profile = directory.path().join("profile");
    fs::create_dir_all(&profile).unwrap();
    fs::write(
        profile.join("player-state.json"),
        serde_json::to_vec(&serde_json::json!({
            "version": 1,
            "data": { "sources": [], "selected": null, "local_files": [], "volume": 1.0 }
        }))
        .unwrap(),
    )
    .unwrap();
    let mut running = Running::in_directory(directory);
    assert_eq!(settings(&running.backend), EqualizerSettings::default());
    let mut expected = center_settings(6.0);
    expected.gains_db[9] = 15.0;
    running.execute(
        "saved-equalizer",
        BackendCommand::SetEqualizer { settings: expected },
    );
    assert_eq!(
        ProjectStore::open(running.profile())
            .unwrap()
            .load()
            .unwrap()
            .equalizer,
        expected
    );
    running.restart();
    assert_eq!(settings(&running.backend), expected);
    let playing = running.play_mix();
    let restored = settled_rms(
        &running.backend,
        playing.playback.position_seconds,
        2.0,
        single_section_mix_rms(6.0),
    );
    let actual = acknowledged(&running.backend, expected);
    assert_eq!(actual.desired.gains_db[9], 15.0);
    assert_eq!(actual.applied.gains_db[9], 15.0);
    assert!(!actual.active_bands[9]);
    assert_media(&playing, &restored);
}

#[test]
fn invalid_settings_and_storage_failures_keep_the_applied_bank_media_and_retry_contract() {
    let running = Running::new();
    let playing = running.play_mix();
    let boost = center_settings(6.0);
    running.execute(
        "rollback-baseline",
        BackendCommand::SetEqualizer { settings: boost },
    );
    let baseline = settled_rms(
        &running.backend,
        playing.playback.position_seconds,
        2.0,
        single_section_mix_rms(6.0),
    );
    let bank = acknowledged(&running.backend, boost);
    let revision = running.backend.snapshot().revision;
    let epoch = cache_epoch(&running.backend);
    let path = running.profile().join("player-state.json");
    let original = fs::read(&path).unwrap();
    for (index, invalid) in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, 15.01, -15.01]
        .into_iter()
        .enumerate()
    {
        let mut invalid_settings = boost;
        invalid_settings.gains_db[index] = invalid;
        let error = running
            .backend
            .execute(envelope(
                &format!("invalid-equalizer-{index}"),
                BackendCommand::SetEqualizer {
                    settings: invalid_settings,
                },
            ))
            .unwrap_err();
        assert_eq!(error.kind, ErrorKind::InvalidData);
    }
    assert_eq!(fs::read(&path).unwrap(), original);
    assert_eq!(running.backend.snapshot().revision, revision);
    assert_eq!(settings(&running.backend), boost);
    assert_bank_unchanged(&bank, &state(&running.backend));

    let damaged = b"damaged settings must remain intact";
    fs::write(&path, damaged).unwrap();
    let failed = envelope(
        "failed-equalizer",
        BackendCommand::SetEqualizer {
            settings: center_settings(-6.0),
        },
    );
    assert!(running.backend.execute(failed.clone()).is_err());
    assert!(
        running
            .backend
            .execute(envelope(
                "failed-equalizer-preset",
                BackendCommand::SetEqualizerPreset {
                    preset: EqualizerPreset::Pop
                },
            ))
            .is_err()
    );
    assert!(
        running
            .backend
            .execute(envelope(
                "failed-equalizer-reset",
                BackendCommand::ResetEqualizer
            ))
            .is_err()
    );
    assert_eq!(fs::read(&path).unwrap(), damaged);
    assert_eq!(settings(&running.backend), boost);
    assert_bank_unchanged(&bank, &state(&running.backend));
    assert_eq!(running.backend.snapshot().revision, revision);
    let still_playing = settled_rms(
        &running.backend,
        baseline.playback.position_seconds,
        0.25,
        single_section_mix_rms(6.0),
    );
    assert_media(&baseline, &still_playing);
    assert_eq!(still_playing.playback.seek_seq, baseline.playback.seek_seq);
    assert_eq!(cache_epoch(&running.backend), epoch);

    fs::write(&path, original).unwrap();
    assert!(running.backend.execute(failed).is_err());
    let mut retry = envelope(
        "equalizer-new-retry-id",
        BackendCommand::SetEqualizer {
            settings: center_settings(-6.0),
        },
    );
    retry.expected_revision = Some(revision);
    let receipt = running.backend.execute(retry.clone()).unwrap();
    assert!(receipt.revision > revision);
    let cut = settled_rms(
        &running.backend,
        still_playing.playback.position_seconds,
        2.0,
        single_section_mix_rms(-6.0),
    );
    let cut_bank = acknowledged(&running.backend, center_settings(-6.0));
    let replay = running.backend.execute(retry.clone()).unwrap();
    assert_eq!(replay.revision, receipt.revision);
    assert_bank_unchanged(&cut_bank, &state(&running.backend));
    let mut collision = retry;
    collision.command = BackendCommand::ResetEqualizer;
    assert_eq!(
        running.backend.execute(collision).unwrap_err().kind,
        ErrorKind::Conflict
    );
    let mut stale = envelope("stale-equalizer", BackendCommand::ResetEqualizer);
    stale.expected_revision = Some(revision);
    assert_eq!(
        running.backend.execute(stale).unwrap_err().kind,
        ErrorKind::Conflict
    );
    assert_eq!(settings(&running.backend), center_settings(-6.0));
    assert_media(&baseline, &cut);
    assert_eq!(cut.playback.seek_seq, baseline.playback.seek_seq);
    assert_eq!(cache_epoch(&running.backend), epoch);
}

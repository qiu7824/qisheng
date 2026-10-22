//! Public output controls consume real PCM without changing the approved media request.

#[path = "support/output_fixture.rs"]
mod fixture;

use fixture::{
    Running, assert_same_media, cache_epoch, envelope, rendered_pcm, temporary_directory, wait,
};
use lx_gpui_player::{
    audio::{
        AudioOutputMode as PlayerOutputMode, AudioPlayer, PlaybackFailureStage, PlaybackStatus,
        cache::CacheOptions,
        transport::{AudioTransport, TransportOptions},
    },
    backend::{
        BackendCommand, BackendConfig, BackendEvent, BackendHandle, BackendQuery,
        BackendQueryResult, ErrorKind, ReceiptState,
    },
    domain::audio_output::{
        AudioDeviceInfo, AudioOutputFormat, AudioOutputMode, AudioOutputState, AudioOutputStatus,
        AudioPreferences, OutputSelection,
    },
    store::ProjectStore,
};
use std::{
    fs, thread,
    time::{Duration, Instant},
};

fn settings(backend: &BackendHandle) -> AudioPreferences {
    match backend.query(BackendQuery::AudioSettings).unwrap() {
        BackendQueryResult::AudioSettings(settings) => settings,
        other => panic!("unexpected audio settings result: {other:?}"),
    }
}

fn output_state(backend: &BackendHandle) -> AudioOutputState {
    match backend.query(BackendQuery::AudioOutputState).unwrap() {
        BackendQueryResult::AudioOutputState(state) => state,
        other => panic!("unexpected audio output result: {other:?}"),
    }
}

fn inventory(backend: &BackendHandle) -> Vec<AudioDeviceInfo> {
    match backend.query(BackendQuery::AudioDevices).unwrap() {
        BackendQueryResult::AudioDevices(devices) => devices,
        other => panic!("unexpected device inventory result: {other:?}"),
    }
}

fn desired_fixture_device() -> OutputSelection {
    // ALL_HOSTS is the compile-time backend list; no host or endpoint is opened.
    OutputSelection::Device {
        id: cpal::DeviceId(
            cpal::ALL_HOSTS[0],
            "deterministic-unavailable-fixture".into(),
        )
        .to_string(),
    }
}

fn assert_memory_output(backend: &BackendHandle, desired: &OutputSelection) {
    let state = output_state(backend);
    assert_eq!(state.mode, AudioOutputMode::Memory);
    assert_eq!(state.status, AudioOutputStatus::NotApplicable);
    assert_eq!(state.desired, *desired);
    assert!(state.active_device_id.is_none());
    assert!(state.active_device_name.is_none());
    assert_eq!(
        state.format,
        Some(AudioOutputFormat {
            sample_rate: 22_050,
            channels: 1
        })
    );
    assert!(!state.fallback);
    assert!(state.last_error.is_none());
    assert!(inventory(backend).is_empty());
}

#[test]
fn fatal_cache_initialization_exposes_unavailable_output_and_joins_runtime_without_a_renderer() {
    let directory = temporary_directory();
    let baseline = AudioTransport::outstanding_runtime_workers();
    let player = AudioPlayer::with_transport_options(
        PlayerOutputMode::Memory,
        TransportOptions {
            cache: CacheOptions {
                root: directory.path().join("invalid-cache"),
                block_size: 0,
                ..Default::default()
            },
            ..Default::default()
        },
    );
    let deadline = Instant::now() + Duration::from_secs(3);
    while player.snapshot().status != PlaybackStatus::Error {
        assert!(
            Instant::now() < deadline,
            "invalid cache quota did not fail initialization"
        );
        thread::sleep(Duration::from_millis(1));
    }
    let failed = player.snapshot();
    assert_eq!(
        failed.failure.unwrap().stage,
        PlaybackFailureStage::Transport
    );
    assert!(
        failed
            .error
            .unwrap()
            .contains("invalid encoded cache quota")
    );
    assert!(player.cache_stats().is_none());
    let unavailable = player.output_snapshot();
    assert_eq!(unavailable.state.mode, AudioOutputMode::Memory);
    assert_eq!(unavailable.state.status, AudioOutputStatus::Unavailable);
    assert!(unavailable.state.active_device_id.is_none());
    assert!(unavailable.devices.is_empty());
    assert_eq!(player.rendered_pcm().unwrap().batches, 0);

    let expected = AudioPreferences {
        selection: desired_fixture_device(),
        muted: true,
        pause_on_device_change: true,
    };
    player.set_output_preferences(expected.clone()).unwrap();
    while player.output_snapshot().preferences != expected {
        assert!(
            Instant::now() < deadline,
            "preferences accepted after fatal initialization were lost"
        );
        thread::sleep(Duration::from_millis(1));
    }
    let terminal = player.output_snapshot();
    assert_eq!(terminal.state.status, AudioOutputStatus::Unavailable);
    assert_eq!(terminal.state.desired, expected.selection);
    player.shutdown(Duration::from_secs(2)).unwrap();
    assert_eq!(
        player.output_snapshot().state.status,
        AudioOutputStatus::Closed
    );
    assert_eq!(AudioTransport::outstanding_runtime_workers(), baseline);
    assert_eq!(player.rendered_pcm().unwrap().batches, 0);
}

#[test]
fn idle_output_events_and_snapshot_cursor_are_coherent_and_memory_refresh_is_quiet() {
    let running = Running::new();
    wait(&running.backend, |state| {
        state.audio_output.state.status == AudioOutputStatus::NotApplicable
    });
    let (initial, mut cursor) = running.backend.snapshot_with_cursor().unwrap();
    assert_eq!(initial.playback.status, PlaybackStatus::Idle);
    let desired = desired_fixture_device();
    let selected = AudioPreferences {
        selection: desired.clone(),
        ..Default::default()
    };
    let muted = AudioPreferences {
        muted: true,
        ..selected.clone()
    };
    let pause_option = AudioPreferences {
        pause_on_device_change: true,
        ..muted.clone()
    };
    for (id, command, expected) in [
        (
            "idle-device-preference",
            BackendCommand::SetAudioOutputDevice { selection: desired },
            selected,
        ),
        ("idle-mute", BackendCommand::SetMute { muted: true }, muted),
        (
            "idle-pause-option",
            BackendCommand::SetPauseOnDeviceChange { enabled: true },
            pause_option,
        ),
    ] {
        let receipt = running.backend.execute(envelope(id, command)).unwrap();
        wait(&running.backend, |state| {
            state.audio_output.preferences == expected
        });
        let (snapshot, next_cursor) = running.backend.snapshot_with_cursor().unwrap();
        let output_events = running.backend.events_after(cursor).unwrap();
        let latest = output_events
            .iter()
            .rev()
            .find_map(|event| {
                if let BackendEvent::AudioOutputChanged { output } = &event.payload {
                    Some((event, output))
                } else {
                    None
                }
            })
            .expect("an idle output preference change must publish an output event");
        assert!(latest.0.event_id <= next_cursor);
        assert_eq!(latest.0.repository_revision, receipt.revision);
        assert_eq!(**latest.1, snapshot.audio_output);
        assert_eq!(snapshot.audio_output.preferences, expected);
        assert_eq!(
            settings(&running.backend),
            snapshot.audio_output.preferences
        );
        assert_eq!(output_state(&running.backend), snapshot.audio_output.state);
        assert_eq!(inventory(&running.backend), snapshot.audio_output.devices);
        assert_same_media(&initial, &snapshot);
        assert_eq!(snapshot.playback.status, PlaybackStatus::Idle);
        cursor = next_cursor;
    }
    let (before_refresh, cursor) = running.backend.snapshot_with_cursor().unwrap();
    running.execute("idle-memory-refresh", BackendCommand::RefreshAudioDevices);
    let deadline = Instant::now() + Duration::from_millis(80);
    loop {
        let (after_refresh, _) = running.backend.snapshot_with_cursor().unwrap();
        assert_eq!(after_refresh.audio_output, before_refresh.audio_output);
        assert!(
            running
                .backend
                .events_after(cursor)
                .unwrap()
                .iter()
                .all(|event| { !matches!(event.payload, BackendEvent::AudioOutputChanged { .. }) })
        );
        if Instant::now() >= deadline {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn memory_desired_device_and_inventory_refresh_keep_the_same_pcm_pipeline_running() {
    let running = Running::new();
    let playing = running.play_local_tone();
    let epoch = cache_epoch(&running.backend);
    assert_memory_output(&running.backend, &OutputSelection::Default);
    running.execute(
        "pause-on-device-change",
        BackendCommand::SetPauseOnDeviceChange { enabled: true },
    );
    let desired = desired_fixture_device();
    let receipt = running
        .backend
        .execute(envelope(
            "desired-memory-endpoint",
            BackendCommand::SetAudioOutputDevice {
                selection: desired.clone(),
            },
        ))
        .unwrap();
    assert_eq!(receipt.state, ReceiptState::Committed);
    assert_eq!(settings(&running.backend).selection, desired);
    assert!(settings(&running.backend).pause_on_device_change);
    wait(&running.backend, |_| {
        output_state(&running.backend).desired == desired
    });
    assert_memory_output(&running.backend, &desired);
    let before_refresh = output_state(&running.backend);
    running.execute(
        "refresh-memory-inventory",
        BackendCommand::RefreshAudioDevices,
    );
    let still_playing = wait(&running.backend, |state| {
        state.playback.status == PlaybackStatus::Playing
            && state.playback.position_seconds > playing.playback.position_seconds + 0.12
            && rendered_pcm(&running.backend).peak > 0.03
    });
    assert_same_media(&playing, &still_playing);
    assert_eq!(cache_epoch(&running.backend), epoch);
    assert_eq!(output_state(&running.backend), before_refresh);
    assert_memory_output(&running.backend, &desired);

    running.execute(
        "restore-default-policy",
        BackendCommand::SetAudioOutputDevice {
            selection: OutputSelection::Default,
        },
    );
    wait(&running.backend, |_| {
        output_state(&running.backend).desired == OutputSelection::Default
    });
    assert_memory_output(&running.backend, &OutputSelection::Default);
    assert_same_media(&playing, &running.backend.snapshot());
    assert_eq!(cache_epoch(&running.backend), epoch);
}

#[test]
fn legacy_settings_load_defaults_and_restart_applies_saved_mute_gain_and_desired_device() {
    let directory = temporary_directory();
    let profile = directory.path().join("profile");
    fs::create_dir_all(&profile).unwrap();
    fs::write(
        profile.join("player-state.json"),
        serde_json::to_vec(&serde_json::json!({
            "version": 1,
            "data": {
                "sources": [], "selected": null, "local_files": [], "volume": 0.31
            }
        }))
        .unwrap(),
    )
    .unwrap();
    let mut running = Running::in_directory(directory);
    wait(&running.backend, |state| state.playback.volume == 0.31);
    assert_eq!(settings(&running.backend), AudioPreferences::default());
    let desired = desired_fixture_device();
    running.execute(
        "saved-desired",
        BackendCommand::SetAudioOutputDevice {
            selection: desired.clone(),
        },
    );
    running.execute("saved-mute", BackendCommand::SetMute { muted: true });
    running.execute(
        "saved-pause-option",
        BackendCommand::SetPauseOnDeviceChange { enabled: true },
    );
    running.execute("saved-volume", BackendCommand::Volume { volume: 0.19 });
    let expected = AudioPreferences {
        selection: desired.clone(),
        muted: true,
        pause_on_device_change: true,
    };
    let persisted = ProjectStore::open(running.profile())
        .unwrap()
        .load()
        .unwrap();
    assert_eq!(persisted.audio_preferences, expected);
    assert_eq!(persisted.volume, 0.19);
    running.restart();
    wait(&running.backend, |state| state.playback.volume == 0.19);
    assert_eq!(settings(&running.backend), expected);
    let id = running.import_local_tone();
    running.execute("play-after-restart", BackendCommand::Play { id });
    let muted_playback = wait(&running.backend, |state| {
        state.playback.status == PlaybackStatus::Playing
            && state.playback.position_seconds > 0.12
            && rendered_pcm(&running.backend).peak == 0.0
    });
    assert_eq!(muted_playback.playback.volume, 0.19);
    assert_memory_output(&running.backend, &desired);
    running.execute(
        "unmute-after-restart",
        BackendCommand::SetMute { muted: false },
    );
    let audible = wait(&running.backend, |state| {
        let pcm = rendered_pcm(&running.backend);
        state.playback.volume == 0.19 && pcm.peak > 0.06 && pcm.peak < 0.08
    });
    assert_same_media(&muted_playback, &audible);
    assert_eq!(settings(&running.backend).selection, desired);
}

#[test]
fn mute_preserves_user_volume_and_real_pcm_clock_while_rendering_zero_gain() {
    let running = Running::new();
    running.play_local_tone();
    running.execute(
        "volume-before-mute",
        BackendCommand::Volume { volume: 0.375 },
    );
    let audible = wait(&running.backend, |state| {
        let pcm = rendered_pcm(&running.backend);
        state.playback.volume == 0.375
            && pcm.peak > 0.12
            && pcm.peak < 0.15
            && pcm.frequency_hz.is_some_and(|hz| (hz - 997.0).abs() < 2.0)
    });
    let epoch = cache_epoch(&running.backend);
    let receipt = running
        .backend
        .execute(envelope("mute", BackendCommand::SetMute { muted: true }))
        .unwrap();
    assert_eq!(receipt.state, ReceiptState::Committed);
    let muted = wait(&running.backend, |state| {
        let pcm = rendered_pcm(&running.backend);
        state.playback.status == PlaybackStatus::Playing
            && state.playback.volume == 0.375
            && state.playback.position_seconds > audible.playback.position_seconds + 0.12
            && pcm.batches > 0
            && pcm.peak == 0.0
            && pcm.rms == 0.0
    });
    assert_same_media(&audible, &muted);
    assert_eq!(cache_epoch(&running.backend), epoch);
    assert!(settings(&running.backend).muted);

    running.execute(
        "volume-during-mute",
        BackendCommand::Volume { volume: 0.125 },
    );
    let changed_volume = wait(&running.backend, |state| {
        state.playback.volume == 0.125
            && state.playback.position_seconds > muted.playback.position_seconds + 0.12
            && rendered_pcm(&running.backend).peak == 0.0
    });
    assert_same_media(&audible, &changed_volume);
    assert!(settings(&running.backend).muted);
    running.execute("unmute", BackendCommand::SetMute { muted: false });
    let unmuted = wait(&running.backend, |state| {
        let pcm = rendered_pcm(&running.backend);
        state.playback.volume == 0.125 && pcm.peak > 0.04 && pcm.peak < 0.05
    });
    assert_same_media(&audible, &unmuted);
    assert_eq!(cache_epoch(&running.backend), epoch);
    assert!(!settings(&running.backend).muted);
}

#[test]
fn failed_output_preference_storage_rolls_back_live_controls_and_requires_a_new_retry_id() {
    let running = Running::new();
    let playing = running.play_local_tone();
    running.execute(
        "valid-settings-baseline",
        BackendCommand::Volume { volume: 0.7 },
    );
    wait(&running.backend, |state| {
        state.playback.volume == 0.7 && rendered_pcm(&running.backend).peak > 0.03
    });
    let profile = running.profile();
    let path = profile.join("player-state.json");
    let original = fs::read(&path).unwrap();
    let before_settings = settings(&running.backend);
    let before_output = output_state(&running.backend);
    let epoch = cache_epoch(&running.backend);
    let revision = running.backend.snapshot().revision;
    let damaged = b"damaged user settings must not be overwritten";
    fs::write(&path, damaged).unwrap();
    let failed = envelope("storage-muted", BackendCommand::SetMute { muted: true });
    assert!(running.backend.execute(failed.clone()).is_err());
    assert!(
        running
            .backend
            .execute(envelope(
                "storage-pause-option",
                BackendCommand::SetPauseOnDeviceChange { enabled: true }
            ))
            .is_err()
    );
    assert!(
        running
            .backend
            .execute(envelope(
                "storage-selection",
                BackendCommand::SetAudioOutputDevice {
                    selection: desired_fixture_device()
                }
            ))
            .is_err()
    );
    assert_eq!(fs::read(&path).unwrap(), damaged);
    assert_eq!(settings(&running.backend), before_settings);
    assert_eq!(output_state(&running.backend), before_output);
    assert_eq!(running.backend.snapshot().revision, revision);
    let still_audible = wait(&running.backend, |state| {
        state.playback.position_seconds > playing.playback.position_seconds + 0.1
            && rendered_pcm(&running.backend).peak > 0.03
    });
    assert_same_media(&playing, &still_audible);
    assert_eq!(cache_epoch(&running.backend), epoch);

    fs::write(&path, original).unwrap();
    assert!(running.backend.execute(failed).is_err());
    assert_eq!(settings(&running.backend), before_settings);
    running.execute(
        "storage-muted-retry",
        BackendCommand::SetMute { muted: true },
    );
    wait(&running.backend, |_| {
        rendered_pcm(&running.backend).peak == 0.0
    });
    assert!(settings(&running.backend).muted);
    assert!(running.backend.snapshot().revision > revision);
}

#[test]
fn malformed_output_ids_and_revision_conflicts_preserve_media_and_request_id_contracts() {
    let running = Running::new();
    let playing = running.play_local_tone();
    let before_settings = settings(&running.backend);
    let before_output = output_state(&running.backend);
    let revision = running.backend.snapshot().revision;
    let epoch = cache_epoch(&running.backend);
    for (index, id) in [
        "".into(),
        "missing-host".into(),
        "host:\0endpoint".into(),
        "x".repeat(4097),
    ]
    .into_iter()
    .enumerate()
    {
        let error = running
            .backend
            .execute(envelope(
                &format!("malformed-output-{index}"),
                BackendCommand::SetAudioOutputDevice {
                    selection: OutputSelection::Device { id },
                },
            ))
            .unwrap_err();
        assert_eq!(error.kind, ErrorKind::InvalidData);
    }
    assert_eq!(settings(&running.backend), before_settings);
    assert_eq!(output_state(&running.backend), before_output);
    assert_eq!(running.backend.snapshot().revision, revision);
    assert_same_media(&playing, &running.backend.snapshot());
    assert_eq!(cache_epoch(&running.backend), epoch);

    let mut request = envelope("idempotent-muted", BackendCommand::SetMute { muted: true });
    request.expected_revision = Some(revision);
    let first = running.backend.execute(request.clone()).unwrap();
    let replay = running.backend.execute(request.clone()).unwrap();
    assert_eq!(replay.operation_id, first.operation_id);
    assert_eq!(replay.revision, first.revision);
    let mut collision = request;
    collision.command = BackendCommand::SetMute { muted: false };
    assert_eq!(
        running.backend.execute(collision).unwrap_err().kind,
        ErrorKind::Conflict
    );

    let mut stale = envelope(
        "stale-output-selection",
        BackendCommand::SetAudioOutputDevice {
            selection: desired_fixture_device(),
        },
    );
    stale.expected_revision = Some(revision);
    let after_mute = settings(&running.backend);
    assert_eq!(
        running.backend.execute(stale).unwrap_err().kind,
        ErrorKind::Conflict
    );
    assert_eq!(settings(&running.backend), after_mute);
    assert_eq!(output_state(&running.backend), before_output);
    assert_same_media(&playing, &running.backend.snapshot());
    assert_eq!(cache_epoch(&running.backend), epoch);
}

#[test]
#[ignore = "explicit physical CPAL run requires two usable output endpoints"]
fn physical_two_endpoint_switch_preserves_the_consumed_pcm_media_clock() {
    let directory = temporary_directory();
    let backend =
        BackendHandle::start(BackendConfig::new(directory.path().join("profile"))).unwrap();
    let running = Running { directory, backend };
    let id = running.import_local_tone();
    running.execute("physical-play", BackendCommand::Play { id });
    let playing = wait(&running.backend, |state| {
        let output = output_state(&running.backend);
        state.playback.status == PlaybackStatus::Playing
            && state.playback.position_seconds > 0.12
            && output.mode == AudioOutputMode::Device
            && output.status == AudioOutputStatus::Ready
            && output.active_device_id.is_some()
    });
    let original = output_state(&running.backend);
    let first_id = original.active_device_id.clone().unwrap();
    let devices = inventory(&running.backend);
    assert!(
        devices.len() >= 2,
        "physical switch validation requires two output endpoints"
    );
    let other_id = devices
        .iter()
        .find(|device| device.id != first_id)
        .expect("physical switch validation requires distinct stable endpoint IDs")
        .id
        .clone();
    let epoch = cache_epoch(&running.backend);
    assert!(matches!(
        running.backend.query(BackendQuery::RenderedPcm).unwrap(),
        BackendQueryResult::RenderedPcm(None)
    ));
    running.execute(
        "physical-second-endpoint",
        BackendCommand::SetAudioOutputDevice {
            selection: OutputSelection::Device {
                id: other_id.clone(),
            },
        },
    );
    let second = wait(&running.backend, |state| {
        let output = output_state(&running.backend);
        output.status == AudioOutputStatus::Ready
            && output.active_device_id.as_deref() == Some(other_id.as_str())
            && output.switch_sequence > original.switch_sequence
            && state.playback.status == PlaybackStatus::Playing
            && state.playback.position_seconds > playing.playback.position_seconds + 0.12
    });
    assert_same_media(&playing, &second);
    assert_eq!(cache_epoch(&running.backend), epoch);
    let second_output = output_state(&running.backend);
    running.execute(
        "physical-first-endpoint",
        BackendCommand::SetAudioOutputDevice {
            selection: OutputSelection::Device {
                id: first_id.clone(),
            },
        },
    );
    let returned = wait(&running.backend, |state| {
        let output = output_state(&running.backend);
        output.status == AudioOutputStatus::Ready
            && output.active_device_id.as_deref() == Some(first_id.as_str())
            && output.switch_sequence > second_output.switch_sequence
            && state.playback.status == PlaybackStatus::Playing
            && state.playback.position_seconds > second.playback.position_seconds + 0.12
    });
    assert_same_media(&playing, &returned);
    assert_eq!(cache_epoch(&running.backend), epoch);
}

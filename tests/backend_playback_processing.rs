//! Processing acknowledgement, frequency and media time come from actual owned PCM.

#[path = "support/playback_processing_fixture.rs"]
mod fixture;

use fixture::{
    FREQUENCY, OUTPUT_RATE, Running, acknowledged, assert_media, cache_epoch, clock_window,
    envelope, frequency, pcm, processing, settings, temporary_directory, wait, wait_finished,
};
use lx_gpui_player::{
    audio::PlaybackStatus,
    backend::{
        BackendCommand, BackendEvent, BackendQuery, BackendQueryResult, ErrorKind, ReceiptState,
    },
    domain::{
        lists::{AddLocation, ListAction},
        playback_processing::{PlaybackProcessingSettings, PlaybackProcessingSnapshot},
    },
    playback::queue::PlayMode,
    store::ProjectStore,
};
use std::{
    fs, thread,
    time::{Duration, Instant},
};

fn controls(rate: f64, preserves_pitch: bool, ratio: f64) -> PlaybackProcessingSettings {
    PlaybackProcessingSettings {
        playback_rate: rate,
        preserves_pitch,
        pitch_ratio: ratio,
    }
}

fn timed_case(source_rate: u32, channels: u16, expected: PlaybackProcessingSettings) {
    let running = Running::new(source_rate, channels, 8.0);
    assert_eq!(processing(&running.backend).sample_rate, None);
    let receipt = running
        .backend
        .execute(envelope(
            "timed-processing",
            BackendCommand::SetPlaybackProcessing { settings: expected },
        ))
        .unwrap();
    assert_eq!(receipt.state, ReceiptState::Committed);
    assert_eq!(processing(&running.backend).sample_rate, None);
    let playing = running.play("timed-play");
    let started = Instant::now();
    let epoch = cache_epoch(&running.backend);
    let applied = acknowledged(&running.backend, expected);
    frequency(&running.backend, expected, FREQUENCY);
    let measured = clock_window(&running.backend, expected.playback_rate, 50);
    assert_media(&playing, &measured);
    assert_eq!(measured.playback.seek_seq, playing.playback.seek_seq);
    assert_eq!(measured.playback.rate, expected.playback_rate);
    assert_eq!(cache_epoch(&running.backend), epoch);

    // Begin at actual audible PCM and its original-media coordinate. Duration
    // is independently timed through the real sink reaching EOF/idle.
    let nominal =
        (running.duration_seconds - playing.playback.position_seconds) / expected.playback_rate;
    let latency = applied.latency_frames as f64 / OUTPUT_RATE as f64;
    wait_finished(
        &running.backend,
        &running.record.id,
        nominal * 1.3 + latency + 3.0,
    );
    let elapsed = started.elapsed().as_secs_f64();
    let allowance = nominal * 0.15 + latency + 0.1;
    assert!(
        (elapsed - nominal).abs() <= allowance,
        "actual rendered duration {elapsed}s, original remainder {}s, nominal {nominal}s, declared latency {latency}s, allowance {allowance}s, controls={expected:?}",
        running.duration_seconds - playing.playback.position_seconds
    );
}

#[test]
fn preserved_pitch_half_and_double_rates_render_the_actual_scaled_duration_at_two_source_formats() {
    timed_case(22_050, 1, controls(0.5, true, 1.0));
    timed_case(48_000, 2, controls(2.0, true, 1.0));
}

#[test]
fn varispeed_doubles_frequency_and_independent_pitch_changes_frequency_without_duration_change() {
    timed_case(22_050, 1, controls(2.0, false, 1.0));
    timed_case(48_000, 2, controls(1.0, true, 1.5));
    timed_case(22_050, 1, controls(1.0, true, 0.5));
}

#[test]
fn live_rate_mute_paused_pitch_seek_and_reset_preserve_media_and_apply_actual_new_pcm() {
    // This case exercises a sequence of live controls rather than EOF timing.
    // Leave enough source media for every control acknowledgement at double speed.
    let running = Running::new(48_000, 2, 20.0);
    let playing = running.play("live-processing-play");
    let epoch = cache_epoch(&running.backend);
    let baseline = acknowledged(&running.backend, PlaybackProcessingSettings::default());
    frequency(
        &running.backend,
        PlaybackProcessingSettings::default(),
        FREQUENCY,
    );
    running.execute(
        "live-double-rate",
        BackendCommand::SetPlaybackRate { rate: 2.0 },
    );
    let doubled = controls(2.0, true, 1.0);
    let applied = acknowledged(&running.backend, doubled);
    assert!(applied.applied_sequence > baseline.applied_sequence);
    frequency(&running.backend, doubled, FREQUENCY);
    let fast = clock_window(&running.backend, 2.0, 25);
    assert_media(&playing, &fast);
    running.execute(
        "live-varispeed-enable",
        BackendCommand::SetPreservesPitch { enabled: false },
    );
    frequency(&running.backend, controls(2.0, false, 1.0), FREQUENCY);
    running.execute(
        "retained-processing-volume",
        BackendCommand::Volume { volume: 0.375 },
    );
    running.execute("processing-mute", BackendCommand::SetMute { muted: true });
    wait(&running.backend, 5.0, |state| {
        state.playback.volume == 0.375
            && state.playback.status == PlaybackStatus::Playing
            && pcm(&running.backend).peak == 0.0
    });
    let muted = clock_window(&running.backend, 2.0, 25);
    assert_eq!(pcm(&running.backend).rms, 0.0);
    assert_media(&playing, &muted);
    running.execute("processing-pause", BackendCommand::Pause);
    let paused = wait(&running.backend, 5.0, |state| {
        state.playback.status == PlaybackStatus::Paused
    });
    thread::sleep(Duration::from_millis(100));
    assert!(
        (running.backend.snapshot().playback.position_seconds - paused.playback.position_seconds)
            .abs()
            < 0.03
    );
    running.execute(
        "paused-processing-rate",
        BackendCommand::SetPlaybackRate { rate: 0.5 },
    );
    running.execute(
        "paused-preserve-pitch",
        BackendCommand::SetPreservesPitch { enabled: true },
    );
    running.execute(
        "paused-processing-pitch",
        BackendCommand::SetPitchRatio { ratio: 1.5 },
    );
    let latest = controls(0.5, true, 1.5);
    assert_eq!(settings(&running.backend), latest);
    running.execute("processing-seek", BackendCommand::Seek { seconds: 2.0 });
    let sought = wait(&running.backend, 10.0, |state| {
        state.playback.status == PlaybackStatus::Paused
            && state.playback.seek_seq == playing.playback.seek_seq + 1
            && (state.playback.position_seconds - 2.0).abs() < 0.04
            && pcm(&running.backend).peak == 0.0
    });
    running.execute(
        "processing-unmute-paused",
        BackendCommand::SetMute { muted: false },
    );
    thread::sleep(Duration::from_millis(80));
    assert_eq!(
        running.backend.snapshot().playback.status,
        PlaybackStatus::Paused
    );
    assert_eq!(pcm(&running.backend).peak, 0.0);
    running.execute("processing-resume", BackendCommand::Resume);
    acknowledged(&running.backend, latest);
    frequency(&running.backend, latest, FREQUENCY);
    let resumed = clock_window(&running.backend, 0.5, 50);
    assert_media(&playing, &resumed);
    assert_eq!(resumed.playback.seek_seq, sought.playback.seek_seq);
    assert_eq!(resumed.playback.volume, 0.375);
    running.execute("processing-reset", BackendCommand::ResetPlaybackProcessing);
    frequency(
        &running.backend,
        PlaybackProcessingSettings::default(),
        FREQUENCY,
    );
    let reset = clock_window(&running.backend, 1.0, 25);
    assert_media(&playing, &reset);
    assert_eq!(reset.playback.seek_seq, sought.playback.seek_seq);
    assert_eq!(reset.playback.volume, 0.375);
    assert_eq!(cache_epoch(&running.backend), epoch);
}

#[test]
fn legacy_profile_and_restart_apply_saved_processing_to_actual_pcm() {
    let directory = temporary_directory();
    let profile = directory.path().join("profile");
    fs::create_dir_all(&profile).unwrap();
    fs::write(
        profile.join("player-state.json"),
        serde_json::to_vec(&serde_json::json!({
            "version": 1,
            "data": { "sources": [], "selected": null, "local_files": [], "volume": 0.375 }
        }))
        .unwrap(),
    )
    .unwrap();
    let mut running = Running::in_directory(directory, 48_000, 2, 12.0);
    assert_eq!(
        settings(&running.backend),
        PlaybackProcessingSettings::default()
    );
    let expected = controls(1.25, false, 1.5);
    running.execute(
        "saved-processing",
        BackendCommand::SetPlaybackProcessing { settings: expected },
    );
    running.execute(
        "saved-processing-volume",
        BackendCommand::Volume { volume: 0.375 },
    );
    let persisted = ProjectStore::open(running.profile())
        .unwrap()
        .load()
        .unwrap();
    assert_eq!(persisted.playback_processing, expected);
    assert_eq!(persisted.volume, 0.375);
    running.restart();
    assert_eq!(settings(&running.backend), expected);
    assert_eq!(processing(&running.backend).sample_rate, None);
    let playing = running.play("restored-processing-play");
    frequency(&running.backend, expected, FREQUENCY);
    let measured = clock_window(&running.backend, 1.25, 25);
    assert_media(&playing, &measured);
    assert_eq!(measured.playback.volume, 0.375);
}

fn assert_ack_unchanged(before: PlaybackProcessingSnapshot, after: PlaybackProcessingSnapshot) {
    assert_eq!(after.desired, before.desired);
    assert_eq!(after.applied, before.applied);
    assert_eq!(after.requested_sequence, before.requested_sequence);
    assert_eq!(after.applied_sequence, before.applied_sequence);
    assert_eq!(after.sample_rate, before.sample_rate);
    assert_eq!(after.latency_frames, before.latency_frames);
    assert_eq!(
        after.transition_remaining_frames,
        before.transition_remaining_frames
    );
}

#[test]
fn invalid_processing_and_storage_failure_roll_back_pcm_acknowledgement_and_retry_ids() {
    let running = Running::new(22_050, 1, 12.0);
    let expected = controls(0.75, true, 1.0);
    running.execute(
        "processing-rollback-baseline",
        BackendCommand::SetPlaybackProcessing { settings: expected },
    );
    let playing = running.play("processing-rollback-play");
    frequency(&running.backend, expected, FREQUENCY);
    let before = processing(&running.backend);
    let epoch = cache_epoch(&running.backend);
    let revision = running.backend.snapshot().revision;
    let path = running.profile().join("player-state.json");
    let original = fs::read(&path).unwrap();
    for (index, command) in [
        BackendCommand::SetPlaybackRate { rate: f64::NAN },
        BackendCommand::SetPlaybackRate {
            rate: f64::INFINITY,
        },
        BackendCommand::SetPlaybackRate { rate: 0.49 },
        BackendCommand::SetPlaybackRate { rate: 2.01 },
        BackendCommand::SetPitchRatio {
            ratio: f64::NEG_INFINITY,
        },
        BackendCommand::SetPitchRatio { ratio: 0.49 },
        BackendCommand::SetPitchRatio { ratio: 1.51 },
        BackendCommand::SetPlaybackProcessing {
            settings: controls(1.0, true, f64::NAN),
        },
    ]
    .into_iter()
    .enumerate()
    {
        let error = running
            .backend
            .execute(envelope(&format!("invalid-processing-{index}"), command))
            .unwrap_err();
        assert_eq!(error.kind, ErrorKind::InvalidData);
    }
    assert_eq!(fs::read(&path).unwrap(), original);
    assert_eq!(settings(&running.backend), expected);
    assert_eq!(running.backend.snapshot().revision, revision);
    assert_ack_unchanged(before, processing(&running.backend));
    let damaged = b"damaged processing profile must remain intact";
    fs::write(&path, damaged).unwrap();
    let failed = envelope(
        "failed-processing",
        BackendCommand::SetPlaybackRate { rate: 1.5 },
    );
    assert!(running.backend.execute(failed.clone()).is_err());
    for (id, command) in [
        ("failed-pitch", BackendCommand::SetPitchRatio { ratio: 1.5 }),
        (
            "failed-preserve",
            BackendCommand::SetPreservesPitch { enabled: false },
        ),
        (
            "failed-processing-reset",
            BackendCommand::ResetPlaybackProcessing,
        ),
    ] {
        assert!(running.backend.execute(envelope(id, command)).is_err());
    }
    assert_eq!(fs::read(&path).unwrap(), damaged);
    assert_eq!(settings(&running.backend), expected);
    assert_eq!(running.backend.snapshot().revision, revision);
    assert_ack_unchanged(before, processing(&running.backend));
    let still_playing = clock_window(&running.backend, 0.75, 25);
    assert_media(&playing, &still_playing);
    assert_eq!(cache_epoch(&running.backend), epoch);

    fs::write(&path, original).unwrap();
    assert!(running.backend.execute(failed).is_err());
    let next = controls(1.5, false, 1.0);
    let mut retry = envelope(
        "new-processing-retry",
        BackendCommand::SetPlaybackProcessing { settings: next },
    );
    retry.expected_revision = Some(revision);
    let receipt = running.backend.execute(retry.clone()).unwrap();
    frequency(&running.backend, next, FREQUENCY);
    let accepted = processing(&running.backend);
    let replay = running.backend.execute(retry.clone()).unwrap();
    assert_eq!(replay.revision, receipt.revision);
    assert_ack_unchanged(accepted, processing(&running.backend));
    let mut collision = retry;
    collision.command = BackendCommand::ResetPlaybackProcessing;
    assert_eq!(
        running.backend.execute(collision).unwrap_err().kind,
        ErrorKind::Conflict
    );
    let mut stale = envelope("stale-processing", BackendCommand::ResetPlaybackProcessing);
    stale.expected_revision = Some(revision);
    assert_eq!(
        running.backend.execute(stale).unwrap_err().kind,
        ErrorKind::Conflict
    );
    let applied = clock_window(&running.backend, 1.5, 25);
    assert_media(&playing, &applied);
    assert_eq!(applied.playback.seek_seq, playing.playback.seek_seq);
    assert_eq!(cache_epoch(&running.backend), epoch);
}

#[test]
fn processing_changes_preserve_ready_preload_identity_and_promote_current_actual_pitch() {
    let running = Running::new(48_000, 2, 12.0);
    let next = running.next_tone(8.0, 1733.0);
    running.execute(
        "processing-preload-list",
        BackendCommand::ListAction {
            action: Box::new(ListAction::MusicAdd {
                list_id: "default".into(),
                tracks: vec![running.record.clone(), next.clone()],
                location: AddLocation::Bottom,
            }),
        },
    );
    running.execute(
        "processing-preload-mode",
        BackendCommand::SetPlayMode {
            mode: PlayMode::List,
        },
    );
    running.execute(
        "processing-preload-play",
        BackendCommand::PlayListTrack {
            list_id: "default".into(),
            music_id: running.record.id.clone(),
            quality: None,
        },
    );
    let ready = wait(&running.backend, 12.0, |state| {
        state
            .preload
            .as_ref()
            .is_some_and(|preload| preload.state == "ready")
    });
    let preload = ready.preload.clone().unwrap();
    assert_eq!(preload.candidate.track_id, next.id);
    let identity = preload.identity.clone().unwrap();
    let epoch = cache_epoch(&running.backend);
    let (_, mut cursor) = running.backend.snapshot_with_cursor().unwrap();
    running.execute(
        "processing-ready-rate",
        BackendCommand::SetPlaybackRate { rate: 2.0 },
    );
    running.execute(
        "processing-ready-pitch",
        BackendCommand::SetPitchRatio { ratio: 1.5 },
    );
    let wanted = controls(2.0, true, 1.5);
    acknowledged(&running.backend, wanted);
    let still_ready = running.backend.snapshot();
    let same_resource = still_ready.preload.as_ref().unwrap();
    assert_eq!(same_resource.operation_id, preload.operation_id);
    assert_eq!(same_resource.state, "ready");
    assert_eq!(
        serde_json::to_value(&same_resource.identity).unwrap(),
        serde_json::to_value(&preload.identity).unwrap()
    );
    assert_media(&ready, &still_ready);
    assert_eq!(cache_epoch(&running.backend), epoch);
    let mut promoted = false;
    let next_playback = wait(&running.backend, 12.0, |state| {
        for event in running.backend.events_after(cursor).unwrap() {
            cursor = event.event_id;
            if let BackendEvent::PreloadPromoted {
                operation_id,
                track_id,
                ..
            } = event.payload
                && operation_id == preload.operation_id
                && track_id == next.id
            {
                promoted = true;
            }
        }
        promoted
            && state.playback.track_id.as_deref() == Some(next.id.as_str())
            && state.playback.status == PlaybackStatus::Playing
    });
    assert!(next_playback.playback.generation > ready.playback.generation);
    assert_eq!(
        next_playback
            .approved_audio_identity
            .as_ref()
            .unwrap()
            .url_sha256,
        identity.url_sha256
    );
    assert!(next_playback.sources.is_empty());
    assert!(next_playback.pending_playback_url.is_none());
    frequency(&running.backend, wanted, 1733.0);
    let actual = clock_window(&running.backend, 2.0, 25);
    assert_eq!(actual.playback.track_id.as_deref(), Some(next.id.as_str()));
    let supply = fixture::supply(&running.backend).expect("current foreground decoder owner");
    assert_eq!(supply.generation, actual.playback.generation);
    assert_eq!(supply.track_id.as_deref(), Some(next.id.as_str()));
    assert_eq!(
        supply.decoder_format,
        Some(lx_gpui_player::audio::pcm::PcmFormat {
            sample_rate: running.source_rate,
            channels: running.channels,
        })
    );
    assert_eq!(
        actual.playback.generation,
        next_playback.playback.generation
    );
    assert_eq!(cache_epoch(&running.backend), epoch);
    assert!(
        matches!(running.backend.query(BackendQuery::PlaybackProcessingSettings).unwrap(), BackendQueryResult::PlaybackProcessingSettings(value) if value == wanted)
    );
}

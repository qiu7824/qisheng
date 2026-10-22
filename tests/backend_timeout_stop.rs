#[path = "support/playback_processing_fixture.rs"]
#[allow(dead_code)]
mod fixture;
use lx_gpui_player::{
    audio::PlaybackStatus,
    backend::{BackendCommand, BackendEvent, BackendQuery, BackendQueryResult},
    playback::{
        queue::PlayMode,
        timeout_stop::{TimeoutStopState, TimeoutStopStatus},
    },
};
use std::{
    thread,
    time::{Duration, Instant},
};

fn timer(running: &fixture::Running) -> TimeoutStopState {
    match running.backend.query(BackendQuery::TimeoutStop).unwrap() {
        BackendQueryResult::TimeoutStop(state) => state,
        other => panic!("Timer response {other:?}"),
    }
}
fn arm(running: &fixture::Running, id: &str, seconds: f64, wait_for_track_end: bool) {
    running.execute(
        id,
        BackendCommand::StartTimeoutStop {
            seconds,
            wait_for_track_end: Some(wait_for_track_end),
        },
    );
}
#[test]
fn expiry_pauses_real_pcm_once_and_explicit_resume_releases_the_latch() {
    let running = fixture::Running::new(22050, 1, 5.0);
    running.play("timer-play");
    let cursor = running.backend.event_cursor();
    arm(&running, "timer-arm", 0.3, false);
    fixture::wait(&running.backend, 3.0, |state| {
        state.playback.status == PlaybackStatus::Paused
    });
    let expired = timer(&running);
    assert_eq!(expired.status, TimeoutStopStatus::Elapsed);
    assert!(expired.blocks_automatic_playback);
    let before = running.backend.snapshot().playback.position_seconds;
    thread::sleep(Duration::from_millis(150));
    assert_eq!(running.backend.snapshot().playback.position_seconds, before);
    let changes = running
        .backend
        .events_after(cursor)
        .unwrap()
        .into_iter()
        .filter(|event| {
            matches!(&event.payload, BackendEvent::TimeoutStopChanged { state }
            if state.status == TimeoutStopStatus::Elapsed)
        })
        .count();
    assert_eq!(changes, 1);
    running.execute("timer-resume", BackendCommand::Resume);
    fixture::wait(&running.backend, 3.0, |state| {
        state.playback.status == PlaybackStatus::Playing
            && state.playback.position_seconds > before + 0.05
    });
    assert_eq!(timer(&running).status, TimeoutStopStatus::Idle);
}
#[test]
fn wait_for_track_end_blocks_single_loop_and_cancel_allows_manual_play() {
    let running = fixture::Running::new(22050, 1, 1.4);
    running.execute(
        "timer-single-loop",
        BackendCommand::SetPlayMode {
            mode: PlayMode::SingleLoop,
        },
    );
    let playing = running.play("timer-wait-play");
    arm(&running, "timer-wait-arm", 0.2, true);
    let deadline = Instant::now() + Duration::from_secs(3);
    while timer(&running).status == TimeoutStopStatus::Armed {
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        timer(&running).status,
        TimeoutStopStatus::WaitingForTrackEnd
    );
    assert_eq!(
        running.backend.snapshot().playback.status,
        PlaybackStatus::Playing
    );
    fixture::wait(&running.backend, 3.0, |state| {
        state.playback.status == PlaybackStatus::Ended
    });
    thread::sleep(Duration::from_millis(150));
    let ended = running.backend.snapshot();
    assert_eq!(ended.playback.status, PlaybackStatus::Ended);
    assert_eq!(ended.playback.generation, playing.playback.generation);
    assert_eq!(timer(&running).status, TimeoutStopStatus::Elapsed);
    running.execute("timer-wait-cancel", BackendCommand::CancelTimeoutStop);
    assert!(!timer(&running).blocks_automatic_playback);
    running.play("timer-after-cancel-play");
}
#[test]
fn countdown_continues_while_paused_and_replacement_invalidity_and_restart_are_isolated() {
    let mut running = fixture::Running::new(22050, 1, 5.0);
    running.play("timer-paused-play");
    running.execute("timer-pause", BackendCommand::Pause);
    arm(&running, "timer-paused-arm", 0.2, false);
    let deadline = Instant::now() + Duration::from_secs(3);
    while timer(&running).status != TimeoutStopStatus::Elapsed {
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(10));
    }
    arm(&running, "timer-replace", 60.0, true);
    let before = timer(&running);
    let revision = running.backend.snapshot().revision;
    assert!(
        running
            .backend
            .execute(fixture::envelope(
                "timer-invalid",
                BackendCommand::StartTimeoutStop {
                    seconds: f64::NAN,
                    wait_for_track_end: Some(false)
                }
            ))
            .is_err()
    );
    let after = timer(&running);
    assert_eq!(after.sequence, before.sequence);
    assert_eq!(after.seconds, Some(60.0));
    assert_eq!(after.status, TimeoutStopStatus::Armed);
    assert_eq!(running.backend.snapshot().revision, revision);
    running.restart();
    assert_eq!(timer(&running).status, TimeoutStopStatus::Idle);
    assert!(!timer(&running).blocks_automatic_playback);
    let BackendQueryResult::TimeoutStopPreferences(prefs) = running
        .backend
        .query(BackendQuery::TimeoutStopPreferences)
        .unwrap()
    else {
        panic!("Timer preferences")
    };
    assert!(prefs.wait_for_track_end);
    assert_eq!(prefs.last_seconds, Some(60.0));
}

#[test]
fn saved_wait_preference_is_used_when_omitted_and_applies_before_an_armed_deadline() {
    let running = fixture::Running::new(22050, 1, 5.0);
    running.play("timer-pref-play");
    running.execute(
        "timer-pref-arm",
        BackendCommand::StartTimeoutStop {
            seconds: 0.4,
            wait_for_track_end: None,
        },
    );
    running.execute(
        "timer-pref-update",
        BackendCommand::SetTimeoutStopPreferences {
            settings: lx_gpui_player::playback::timeout_stop::TimeoutStopPreferences {
                wait_for_track_end: false,
                last_seconds: Some(1800.0),
            },
        },
    );
    assert_eq!(timer(&running).seconds, Some(0.4));
    fixture::wait(&running.backend, 3.0, |state| {
        state.playback.status == PlaybackStatus::Paused
    });
    assert!(!timer(&running).wait_for_track_end);
    running.execute(
        "timer-pref-new",
        BackendCommand::StartTimeoutStop {
            seconds: 60.0,
            wait_for_track_end: None,
        },
    );
    assert!(!timer(&running).wait_for_track_end);
}

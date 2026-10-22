//! Recovery uses real HTTP outcomes and keeps approval, matching, and playback separate.
#[path = "support/backend_recovery.rs"]
mod fixture;
use fixture::*;
use lx_gpui_player::{
    audio::PlaybackStatus,
    backend::{BackendCommand, BackendQuery, BackendQueryResult, CommandEnvelope},
    domain::lists::ListAction,
};
use std::{
    collections::BTreeMap,
    sync::{Mutex, atomic::Ordering},
    thread,
    time::{Duration, Instant},
};

static CASES: Mutex<()> = Mutex::new(());

fn allow_switch(running: &Running) {
    command(
        &running.backend,
        "switch-on",
        BackendCommand::SetAutoSwitchSource { enabled: true },
    );
}
fn assert_original_list(running: &Running) {
    let BackendQueryResult::ListTracks(data) = running
        .backend
        .query(BackendQuery::ListTracks {
            list_id: "default".into(),
            offset: 0,
            limit: 10,
        })
        .unwrap()
    else {
        panic!("missing original list");
    };
    assert_eq!(data.items, vec![running.record.clone()]);
}
fn assert_single_search_per_platform(server: &Server) {
    let mut counts = BTreeMap::new();
    for platform in server.search_calls() {
        *counts.entry(platform).or_insert(0usize) += 1;
    }
    assert!(!counts.is_empty());
    assert!(
        counts.values().all(|count| *count == 1),
        "matching storm: {counts:?}"
    );
    assert_eq!(counts.get("tx"), Some(&1));
}

#[test]
fn http429_retries_same_approved_url_with_two_four_second_backoff_and_no_resolution_or_matching() {
    let _case = CASES.lock().unwrap_or_else(|error| error.into_inner());
    let server = Server::new();
    server.shared.rate_remaining.store(2, Ordering::Release);
    let running = Running::new(&server, &["128k"]);
    allow_switch(&running);
    let url = running.play("play-rate-limit", None);
    assert!(server.media_calls().is_empty());
    running.approve("approve-rate-limit");
    wait_media(&server, 1);
    let first_identity = running.backend.snapshot().approved_audio_identity.unwrap();
    wait(&running.backend, |state| {
        state.playback.status == PlaybackStatus::Playing
    });
    let calls = server.media_calls();
    assert_eq!(calls.len(), 3);
    assert_eq!(
        calls.iter().map(|call| call.status).collect::<Vec<_>>(),
        vec![429, 429, 206]
    );
    assert!(calls[1].at.duration_since(calls[0].at) >= Duration::from_millis(1900));
    assert!(calls[2].at.duration_since(calls[1].at) >= Duration::from_millis(3900));
    assert!(
        calls
            .iter()
            .all(|call| format!("{}{}", server.base, call.target) == url)
    );
    let state = running.backend.snapshot();
    let final_identity = state.approved_audio_identity.unwrap();
    assert_eq!(final_identity.approval_epoch, first_identity.approval_epoch);
    assert_eq!(final_identity.approval_id, first_identity.approval_id);
    assert_eq!(final_identity.source_sha256, first_identity.source_sha256);
    assert_eq!(final_identity.url_sha256, first_identity.url_sha256);
    assert!(state.pending_playback_url.is_none());
    assert_eq!(server.source_calls().len(), 1);
    assert!(server.search_calls().is_empty());
    let started = Instant::now();
    loop {
        if let BackendQueryResult::RenderedPcm(Some(pcm)) =
            running.backend.query(BackendQuery::RenderedPcm).unwrap()
            && pcm.batches > 0
            && pcm.peak > 0.01
        {
            break;
        }
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "real Memory output did not render the recovered data"
        );
        thread::sleep(Duration::from_millis(5));
    }
    assert_original_list(&running);
}

#[test]
fn persistent_http429_exhausts_its_budget_without_source_or_matching_storm() {
    let _case = CASES.lock().unwrap_or_else(|error| error.into_inner());
    let server = Server::new();
    server.shared.main_status.store(429, Ordering::Release);
    let running = Running::new(&server, &["128k"]);
    allow_switch(&running);
    running.play("play-always-busy", None);
    running.approve("approve-always-busy");
    wait_media(&server, 3);
    thread::sleep(Duration::from_millis(2300));
    assert_eq!(server.media_calls().len(), 3);
    assert_eq!(server.source_calls().len(), 1);
    assert!(server.search_calls().is_empty());
    let state = running.backend.snapshot();
    assert!(state.pending_playback_url.is_none());
    assert_eq!(state.playback.status, PlaybackStatus::Error);
    assert_original_list(&running);
}

#[test]
fn pause_stop_metadata_and_source_changes_cancel_the_pending_http429_timer() {
    let _case = CASES.lock().unwrap_or_else(|error| error.into_inner());
    for action in ["pause", "stop", "metadata", "disable", "switch"] {
        let server = Server::new();
        server.shared.main_status.store(429, Ordering::Release);
        let running = Running::new(&server, &["128k"]);
        running.play("play-before-cancel", None);
        running.approve("approve-before-cancel");
        wait_media(&server, 1);
        wait(&running.backend, |state| {
            state.playback.status == PlaybackStatus::Error
        });
        match action {
            "pause" => command(&running.backend, "pause", BackendCommand::Pause),
            "stop" => command(&running.backend, "stop", BackendCommand::Stop),
            "metadata" => {
                let mut changed = running.record.clone();
                changed
                    .metadata
                    .insert("recovery_revision".into(), "changed-before-timer".into());
                command(
                    &running.backend,
                    "edit",
                    BackendCommand::ListAction {
                        action: Box::new(ListAction::MusicOverwrite {
                            list_id: "default".into(),
                            tracks: vec![changed],
                        }),
                    },
                );
            }
            "disable" => command(&running.backend, "disable", BackendCommand::DisableSource),
            "switch" => running.source(&server, "secondary"),
            _ => unreachable!(),
        }
        thread::sleep(Duration::from_millis(2300));
        assert_eq!(
            server.media_calls().len(),
            1,
            "{action} allowed a stale retry GET"
        );
        assert!(
            server.search_calls().is_empty(),
            "{action} triggered fallback metadata"
        );
    }
}

fn reach_fallback(
    server: &Server,
    running: &Running,
    status: u16,
    quality: Option<&str>,
) -> String {
    server
        .shared
        .main_status
        .store(usize::from(status), Ordering::Release);
    let mut previous = running.play("play-rejected-candidate", quality);
    assert!(server.media_calls().is_empty());
    for attempt in 1..=2 {
        running.approve(&format!("approve-rejected-{attempt}"));
        let refreshed = wait(&running.backend, |state| {
            state
                .pending_playback_url
                .as_ref()
                .is_some_and(|url| url != &previous)
        });
        previous = refreshed.pending_playback_url.unwrap();
        assert!(previous.contains("platform=kw"));
        assert_eq!(server.source_calls().len(), attempt + 1);
        assert!(
            server.search_calls().is_empty(),
            "fallback occurred before two actual refreshes"
        );
    }
    running.approve("approve-final-original");
    wait(&running.backend, |state| {
        state
            .pending_playback_url
            .as_ref()
            .is_some_and(|url| url.contains("platform=tx"))
    })
    .pending_playback_url
    .unwrap()
}

#[test]
fn two_actual_http_rejections_then_native_fallback_recompute_quality_and_preserve_the_original() {
    let _case = CASES.lock().unwrap_or_else(|error| error.into_inner());
    for status in [401_u16, 403, 404, 410] {
        let server = Server::new();
        let running = Running::new(&server, &["128k"]);
        allow_switch(&running);
        command(
            &running.backend,
            "preferred-320",
            BackendCommand::SetPlaybackQuality {
                quality: "320k".into(),
            },
        );
        let candidate_url = reach_fallback(&server, &running, status, None);
        assert!(candidate_url.contains("quality=320k"));
        let calls = server.source_calls();
        assert_eq!(calls.len(), 4);
        assert!(
            calls[..3]
                .iter()
                .all(|call| call.platform == "kw" && call.quality == "128k")
        );
        assert_eq!(calls[3].platform, "tx");
        assert_eq!(calls[3].quality, "320k");
        assert_eq!(calls[3].dto["songmid"], "actual-mid");
        assert_eq!(calls[3].dto["songId"], 301);
        assert_eq!(calls[3].dto["strMediaMid"], "actual-media-mid");
        assert_eq!(calls[3].dto["albumMid"], "actual-album-mid");
        assert_single_search_per_platform(&server);
        assert!(
            server
                .media_calls()
                .iter()
                .all(|call| call.platform == "kw")
        );
        assert_original_list(&running);
        thread::sleep(Duration::from_millis(150));
        assert!(
            server
                .media_calls()
                .iter()
                .all(|call| call.platform == "kw")
        );
        running.approve("approve-actual-candidate");
        let playing = wait(&running.backend, |state| {
            state.playback.status == PlaybackStatus::Playing
        });
        let identity = playing.approved_audio_identity.unwrap();
        assert_eq!(identity.track_id, running.record.id);
        assert_eq!(identity.platform, "tx");
        assert_eq!(identity.actual_quality, "320k");
        assert!(
            server
                .media_calls()
                .iter()
                .any(|call| call.platform == "tx")
        );
        assert_original_list(&running);
    }
}

#[test]
fn explicit_quality_survives_fallback_and_an_unsupported_candidate_is_never_resolved() {
    let _case = CASES.lock().unwrap_or_else(|error| error.into_inner());
    {
        let server = Server::new();
        let running = Running::new(&server, &["128k", "320k"]);
        allow_switch(&running);
        command(
            &running.backend,
            "preferred-320",
            BackendCommand::SetPlaybackQuality {
                quality: "320k".into(),
            },
        );
        let candidate = reach_fallback(&server, &running, 403, Some("128k"));
        assert!(candidate.contains("quality=128k"));
        assert_eq!(server.source_calls().last().unwrap().quality, "128k");
        assert!(
            server
                .media_calls()
                .iter()
                .all(|call| call.platform == "kw")
        );
        assert_original_list(&running);
    }
    {
        let server = Server::new();
        server.shared.main_status.store(403, Ordering::Release);
        let running = Running::new(&server, &["flac"]);
        allow_switch(&running);
        let mut previous = running.play("play-strict-flac", Some("flac"));
        for attempt in 1..=2 {
            running.approve(&format!("approve-strict-{attempt}"));
            previous = wait(&running.backend, |state| {
                state
                    .pending_playback_url
                    .as_ref()
                    .is_some_and(|url| url != &previous)
            })
            .pending_playback_url
            .unwrap();
        }
        running.approve("approve-last-strict");
        wait(&running.backend, |state| {
            !state.busy
                && (state.playback.status == PlaybackStatus::Error
                    || (state.playback.status == PlaybackStatus::Idle && state.error.is_some()))
                && state.pending_playback_url.is_none()
        });
        thread::sleep(Duration::from_millis(300));
        assert_eq!(server.source_calls().len(), 3);
        assert!(
            server
                .source_calls()
                .iter()
                .all(|call| call.platform == "kw" && call.quality == "flac")
        );
        assert_single_search_per_platform(&server);
        assert_original_list(&running);
    }
}

#[test]
fn typed_source_busy_and_cancelled_resolution_never_launch_native_fallback() {
    let _case = CASES.lock().unwrap_or_else(|error| error.into_inner());
    {
        let server = Server::new();
        server.shared.source_busy.store(true, Ordering::Release);
        let running = Running::new(&server, &["128k"]);
        allow_switch(&running);
        running
            .backend
            .execute(CommandEnvelope {
                request_id: "busy-play".into(),
                origin: "media-recovery-test".into(),
                expected_revision: None,
                command: BackendCommand::PlayListTrack {
                    list_id: "default".into(),
                    music_id: running.record.id.clone(),
                    quality: None,
                },
            })
            .unwrap();
        wait(&running.backend, |state| {
            !state.busy && state.error.is_some()
        });
        let calls = server.source_calls();
        assert_eq!(calls.len(), 3);
        assert_eq!(
            calls.iter().map(|call| call.ordinal).collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
        assert!(calls[1].at.duration_since(calls[0].at) >= Duration::from_millis(1900));
        assert!(calls[2].at.duration_since(calls[1].at) >= Duration::from_millis(3900));
        assert!(server.search_calls().is_empty());
        assert!(server.media_calls().is_empty());
    }
    {
        let server = Server::new();
        server.shared.hold_source.store(true, Ordering::Release);
        let running = Running::new(&server, &["128k"]);
        allow_switch(&running);
        running
            .backend
            .execute(CommandEnvelope {
                request_id: "cancelled-play".into(),
                origin: "media-recovery-test".into(),
                expected_revision: None,
                command: BackendCommand::PlayListTrack {
                    list_id: "default".into(),
                    music_id: running.record.id.clone(),
                    quality: None,
                },
            })
            .unwrap();
        wait_source(&server, 1);
        command(&running.backend, "cancelled-stop", BackendCommand::Stop);
        server.shared.hold_source.store(false, Ordering::Release);
        idle(&running.backend);
        thread::sleep(Duration::from_millis(300));
        assert_eq!(server.source_calls().len(), 1);
        assert!(server.search_calls().is_empty());
        assert!(server.media_calls().is_empty());
        assert!(running.backend.snapshot().pending_playback_url.is_none());
        assert_original_list(&running);
    }
}

#[test]
fn repeatedly_rejected_actual_candidates_exhaust_a_global_chain_without_platform_cycles() {
    let _case = CASES.lock().unwrap_or_else(|error| error.into_inner());
    let server = Server::new();
    server.shared.main_status.store(403, Ordering::Release);
    server.shared.candidate_status.store(403, Ordering::Release);
    let running = Running::new(&server, &["128k"]);
    allow_switch(&running);
    let mut url = running.play("play-finite-chain", None);
    let mut exhausted = false;
    for approval in 1..=7 {
        running.approve(&format!("chain-approval-{approval}"));
        wait_media(&server, approval);
        let started = Instant::now();
        let mut terminal_since = None;
        loop {
            let state = running.backend.snapshot();
            if let Some(next) = state
                .pending_playback_url
                .as_ref()
                .filter(|next| *next != &url)
            {
                url = next.clone();
                break;
            }
            if !state.busy
                && state.pending_playback_url.is_none()
                && (state.playback.status == PlaybackStatus::Error
                    || (state.playback.status == PlaybackStatus::Idle && state.error.is_some()))
            {
                let since = *terminal_since.get_or_insert_with(Instant::now);
                if since.elapsed() >= Duration::from_millis(750) {
                    exhausted = true;
                    break;
                }
            } else {
                terminal_since = None;
            }
            assert!(started.elapsed() < Duration::from_secs(8), "{state:?}");
            thread::sleep(Duration::from_millis(5));
        }
        if exhausted {
            break;
        }
    }
    assert!(
        exhausted,
        "rejected metadata platforms were visited in an unbounded cycle"
    );
    let calls = server.source_calls();
    assert!(calls.len() <= 6, "{calls:?}");
    let first_tx = calls.iter().position(|call| call.platform == "tx").unwrap();
    assert!(calls[..first_tx].iter().all(|call| call.platform == "kw"));
    assert!(calls[first_tx..].iter().all(|call| call.platform == "tx"));
    assert_single_search_per_platform(&server);
    thread::sleep(Duration::from_millis(300));
    assert_eq!(server.source_calls().len(), calls.len());
    assert!(running.backend.snapshot().pending_playback_url.is_none());
    assert_original_list(&running);
}

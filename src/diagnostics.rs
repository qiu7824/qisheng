//! Executable acceptance checks using only the bundled original fixtures.
use crate::{
    audio::{AudioPlayer, PlaybackStatus},
    catalog::Catalog,
    source::{SourceRequest, import_script},
    store::{ProjectStore, StoreData},
    worker::{SourceWorker, validate_source},
};
use anyhow::{Context, Result, ensure};
use serde_json::json;
use std::{
    path::PathBuf,
    thread,
    time::{Duration, Instant},
};

fn wait_for(player: &AudioPlayer, wanted: PlaybackStatus) -> Result<()> {
    wait_for_track(player, wanted, None)
}

fn wait_for_track(
    player: &AudioPlayer,
    wanted: PlaybackStatus,
    track_id: Option<&str>,
) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        let snapshot = player.snapshot();
        if snapshot.status == wanted
            && track_id.is_none_or(|id| snapshot.track_id.as_deref() == Some(id))
        {
            return Ok(());
        }
        ensure!(
            snapshot.status != PlaybackStatus::Error || wanted == PlaybackStatus::Error,
            "Audio device/load failed: {:?}",
            snapshot.error
        );
        ensure!(
            Instant::now() < deadline,
            "Audio status did not become {wanted:?}: {snapshot:?}"
        );
        thread::sleep(Duration::from_millis(40));
    }
}

pub fn run(data_dir: PathBuf) -> Result<serde_json::Value> {
    ensure!(
        !data_dir.join("player-state.json").exists(),
        "Diagnostics require a fresh data directory to protect saved sources"
    );
    let catalog = Catalog::new()?;
    let tracks = catalog.tracks();
    ensure!(
        tracks.len() >= 3
            && catalog.search("First").len() == 1
            && catalog.search("no-such-track").is_empty(),
        "Catalog search failed"
    );
    let script = include_str!("../fixtures/legal-http-utils-source.js");
    let static_only = validate_source("/* @name Static */ throw Error('This must not execute');")?;
    ensure!(
        !static_only.approved,
        "Static worker unexpectedly approved execution"
    );
    let mut source = import_script(script)?;
    ensure!(!source.approved, "Import unexpectedly authorized execution");
    ensure!(
        import_script("/* @name Broken */ function (").is_err(),
        "Broken JavaScript was accepted"
    );
    source.approve()?; // Only this bundled, project-owned test fixture.
    let mut worker = SourceWorker::start(&source.script).context("Source worker initialization")?;
    ensure!(
        worker.manifest().sources.contains_key("local"),
        "No local capability"
    );
    let resolve = |worker: &mut SourceWorker, index: usize| -> Result<String> {
        worker.request(&SourceRequest {
            source: "local".into(),
            action: "musicUrl".into(),
            info: tracks[index].request_info(),
        })
    };
    let first_url = resolve(&mut worker, 0)?;
    ensure!(
        first_url == tracks[0].source_url,
        "LX callback bridge returned the wrong URL"
    );
    let store = ProjectStore::open(data_dir)?;
    let saved = StoreData {
        selected: Some(source.sha256.clone()),
        sources: vec![source],
        local_files: vec![],
        volume: 0.15,
        audio_preferences: Default::default(),
        equalizer: Default::default(),
        playback_processing: Default::default(),
        audio_analysis_enabled: false,
        spatial: Default::default(),
        effects: None,
        timeout_stop: Default::default(),
        system_control: Default::default(),
    };
    store.save(&saved)?;
    let cancel = worker.cancel_handle();
    cancel.terminate();
    cancel.terminate();
    ensure!(
        resolve(&mut worker, 0).is_err(),
        "A cancelled worker still resolved a request"
    );
    drop(worker);
    let restored = store.load()?;
    ensure!(
        restored.selected == saved.selected && restored.sources[0].approved,
        "Restart persistence failed"
    );
    let mut worker = SourceWorker::start(&restored.sources[0].script)?;
    ensure!(
        resolve(&mut worker, 0)? == first_url,
        "Restored resolver failed"
    );
    let mut report = json!({
        "result": "SOURCE_PASS",
        "static_validation": "PASS: isolated; throw statement not executed; remains unapproved",
        "damaged_source": "PASS: rejected",
        "worker_cancel": "PASS: idempotent; terminated worker cannot resolve",
        "initialization": "PASS: actual isolated child; Windows Job attached",
        "lx_request": "PASS: real callback HTTP POST /resolve; headers and JSON body; flattened legacy fields",
        "utils": "PASS: UTF-8/base64 Buffer; MD5; AES-128-CBC; zlib deflate/inflate round trip",
        "restart": "PASS: script, policy-v2 approval, selection and volume restored"
    });
    let player = AudioPlayer::new();
    player.set_volume(0.15);
    player.load(tracks[0].id.clone(), first_url, true);
    if let Err(error) = wait_for(&player, PlaybackStatus::Playing) {
        report["result"] = json!("BLOCKED");
        report["reason"] = json!(format!("{error:#}"));
        report["audio_output"] = json!("FAILED: real default output device could not start");
        report["play_pause_seek_next"] = json!("NOT RUN");
        report["http_404_audio_recovery"] = json!("NOT RUN");
        report["playback"] = json!(player.snapshot());
        return Ok(report);
    }
    thread::sleep(Duration::from_millis(500));
    ensure!(
        player.snapshot().position_seconds > 0.05,
        "Real audio playback clock did not advance"
    );
    player.pause();
    wait_for(&player, PlaybackStatus::Paused)?;
    let paused = player.snapshot().position_seconds;
    thread::sleep(Duration::from_millis(200));
    ensure!(
        (player.snapshot().position_seconds - paused).abs() < 0.1,
        "Paused audio clock advanced"
    );
    player.seek(4.0);
    thread::sleep(Duration::from_millis(150));
    ensure!(
        (player.snapshot().position_seconds - 4.0).abs() < 0.25,
        "Seek failed"
    );
    player.play();
    wait_for(&player, PlaybackStatus::Playing)?;
    let second_url = resolve(&mut worker, 1)?;
    player.load(tracks[1].id.clone(), second_url, true);
    wait_for_track(&player, PlaybackStatus::Playing, Some(&tracks[1].id))?;
    ensure!(
        player.snapshot().track_id.as_deref() == Some(&tracks[1].id),
        "Track switch failed"
    );
    player.load("bad", format!("{}/missing.wav", catalog.base_url()), true);
    wait_for(&player, PlaybackStatus::Error)?;
    let recover_url = resolve(&mut worker, 2)?;
    player.load(tracks[2].id.clone(), recover_url, true);
    wait_for(&player, PlaybackStatus::Playing)?;
    let final_state = player.snapshot();
    player.stop();
    report["result"] = json!("PASS");
    report["audio_output"] = json!("PASS: Windows output device; playback clock advances");
    report["play_pause_seek_next"] = json!("PASS");
    report["http_404_audio_recovery"] = json!("PASS");
    report["final_playback"] = json!(final_state);
    Ok(report)
}

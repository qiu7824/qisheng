#[path = "support/playback_processing_fixture.rs"]
#[allow(dead_code)]
mod fixture;
#[path = "support/library_fixture.rs"]
mod local;
use lx_gpui_player::{
    audio::{PlaybackStatus, analyser::AnalysisFrame},
    backend::{BackendCommand, BackendHandle, BackendQuery, BackendQueryResult},
    domain::equalizer::EqualizerSettings,
};
use std::{
    thread,
    time::{Duration, Instant},
};
fn analysis(backend: &BackendHandle) -> Option<AnalysisFrame> {
    match backend.query(BackendQuery::AudioAnalysis).unwrap() {
        BackendQueryResult::AudioAnalysis(frame) => frame.map(|frame| *frame),
        other => panic!("analysis response {other:?}"),
    }
}
fn wait_frame(backend: &BackendHandle, ready: impl Fn(&AnalysisFrame) -> bool) -> AnalysisFrame {
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        if let Some(frame) = analysis(backend)
            && ready(&frame)
        {
            return frame;
        }
        assert!(
            Instant::now() < deadline,
            "Real audio analysis did not become ready"
        );
        thread::sleep(Duration::from_millis(10));
    }
}
#[test]
fn real_render_analysis_taps_before_equalizer_and_gain_and_holds_while_paused() {
    let running = fixture::Running::new(22050, 1, 10.0);
    assert!(analysis(&running.backend).is_none());
    running.execute(
        "enable-analysis",
        BackendCommand::SetAudioAnalysisEnabled { enabled: true },
    );
    running.play("analysis-play");
    let initial = wait_frame(&running.backend, |frame| frame.sequence >= 20);
    running.execute(
        "analysis-disable",
        BackendCommand::SetAudioAnalysisEnabled { enabled: false },
    );
    assert!(analysis(&running.backend).is_none());
    running.execute(
        "analysis-enable-again",
        BackendCommand::SetAudioAnalysisEnabled { enabled: true },
    );
    let initial = wait_frame(&running.backend, |frame| {
        frame.sequence >= 20 && frame.stamp.media_interval_end > initial.stamp.media_interval_end
    });
    let peak = initial
        .decibels
        .iter()
        .enumerate()
        .max_by(|a, b| a.1.total_cmp(b.1))
        .unwrap()
        .0;
    assert!(((peak as f64 * initial.sample_rate as f64 / 256.0) - fixture::FREQUENCY).abs() < 90.0);
    assert_eq!(initial.valid_window_frames, 256);
    assert!(initial.decibels.iter().all(|value| value.is_finite()));
    let mut eq = EqualizerSettings::default();
    eq.gains_db.fill(-12.0);
    running.execute("analysis-eq", BackendCommand::SetEqualizer { settings: eq });
    running.execute("analysis-gain", BackendCommand::Volume { volume: 0.05 });
    running.execute("analysis-mute", BackendCommand::SetMute { muted: true });
    let after = wait_frame(&running.backend, |frame| {
        frame.sequence > initial.sequence + 15
    });
    assert_eq!(after.stamp.token, initial.stamp.token);
    assert!(
        (after.decibels[peak] - initial.decibels[peak]).abs() < 0.6,
        "Analysis unexpectedly includes EQ/gain: {:?} {:?}",
        initial.decibels[peak],
        after.decibels[peak]
    );
    running.execute("analysis-pause", BackendCommand::Pause);
    fixture::wait(&running.backend, 5.0, |snapshot| {
        snapshot.playback.status == PlaybackStatus::Paused
    });
    thread::sleep(Duration::from_millis(100));
    let paused = analysis(&running.backend).unwrap();
    thread::sleep(Duration::from_millis(150));
    assert_eq!(analysis(&running.backend), Some(paused));
    let serialized = serde_json::to_value(paused).unwrap();
    assert_eq!(serialized["bins"].as_array().unwrap().len(), 128);
    assert_eq!(serialized["decibels"].as_array().unwrap().len(), 128);
    running.execute("analysis-stop", BackendCommand::Stop);
    fixture::wait(&running.backend, 5.0, |snapshot| {
        snapshot.playback.status == PlaybackStatus::Idle
    });
    assert!(analysis(&running.backend).is_none());
    running.backend.shutdown(Duration::from_secs(5)).unwrap();
    assert_eq!(
        running
            .backend
            .query(BackendQuery::AudioAnalysis)
            .unwrap_err()
            .kind,
        lx_gpui_player::backend::ErrorKind::Closed
    );
}

#[test]
fn actual_five_point_one_lfe_only_pcm_is_excluded_before_memory_mono_mapping() {
    let running = local::Running::new();
    running.execute(
        "enable-lfe-analysis",
        BackendCommand::SetAudioAnalysisEnabled { enabled: true },
    );
    let path = running.folder("lfe").join("lfe-only.wav");
    let rate = 22050u32;
    let frames = rate as usize * 4;
    let length = (frames * 6 * 4) as u32;
    let mut bytes = Vec::with_capacity(44 + length as usize);
    bytes.extend_from_slice(b"RIFF");
    bytes.extend_from_slice(&(36 + length).to_le_bytes());
    bytes.extend_from_slice(b"WAVEfmt ");
    bytes.extend_from_slice(&16u32.to_le_bytes());
    bytes.extend_from_slice(&3u16.to_le_bytes());
    bytes.extend_from_slice(&6u16.to_le_bytes());
    bytes.extend_from_slice(&rate.to_le_bytes());
    bytes.extend_from_slice(&(rate * 24).to_le_bytes());
    bytes.extend_from_slice(&24u16.to_le_bytes());
    bytes.extend_from_slice(&32u16.to_le_bytes());
    bytes.extend_from_slice(b"data");
    bytes.extend_from_slice(&length.to_le_bytes());
    for index in 0..frames {
        for channel in 0..6 {
            let value = if channel == 3 {
                0.5 * (std::f64::consts::TAU * 440.0 * index as f64 / rate as f64).sin() as f32
            } else {
                0.0
            };
            bytes.extend_from_slice(&value.to_le_bytes());
        }
    }
    std::fs::write(&path, bytes).unwrap();
    running.import_explicit("lfe-import", vec![path]);
    let id = running
        .backend
        .snapshot()
        .tracks
        .iter()
        .find(|track| track.title.contains("lfe-only"))
        .unwrap()
        .id
        .clone();
    running.execute("lfe-play", BackendCommand::Play { id });
    let frame = wait_frame(&running.backend, |frame| frame.sequence >= 8);
    assert_eq!(frame.channels, 6);
    assert_eq!(
        frame.channel_mix,
        lx_gpui_player::audio::analyser::AnalysisChannelMix::Speakers
    );
    assert!(frame.decibels.iter().all(|value| *value == -1000.0));
    let BackendQueryResult::RenderedPcm(Some(pcm)) =
        running.backend.query(BackendQuery::RenderedPcm).unwrap()
    else {
        panic!("PCM")
    };
    assert!(
        pcm.peak > 0.01,
        "Memory downmix should actually contain LFE while pre-map analyser excludes it"
    );
}

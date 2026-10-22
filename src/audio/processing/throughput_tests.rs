//! Decoder-worker throughput measured with independent PCM and media-coordinate oracles.

use super::{ProcessedFrame, WorkerProcessor};
use crate::audio::pcm::{MAX_CHANNELS, PcmFormat};
use crate::domain::playback_processing::PlaybackProcessingSettings;
use std::time::Instant;

const INPUT_SECONDS: usize = 8;
const TONE_HZ: f64 = 997.0;
const AMPLITUDE: f64 = 0.02;
const SEQUENCE: u64 = 17;

fn tone_input(format: PcmFormat) -> Vec<[f32; MAX_CHANNELS]> {
    let frames = format.sample_rate as usize * INPUT_SECONDS;
    (0..frames)
        .map(|frame| {
            let tone = (AMPLITUDE
                * (std::f64::consts::TAU * TONE_HZ * frame as f64 / format.sample_rate as f64)
                    .cos()) as f32;
            let mut samples = [0.0; MAX_CHANNELS];
            samples[..format.channels as usize].fill(tone);
            samples
        })
        .collect()
}

fn positive_crossing_frequency(frames: &[ProcessedFrame], rate: u32, channel: usize) -> f64 {
    let mut first = None;
    let mut last = 0.0;
    let mut count = 0_u64;
    for (index, pair) in frames.windows(2).enumerate() {
        let left = f64::from(pair[0].samples[channel]);
        let right = f64::from(pair[1].samples[channel]);
        if left < 0.0 && right >= 0.0 {
            let crossing = index as f64 - left / (right - left);
            first.get_or_insert(crossing);
            last = crossing;
            count += 1;
        }
    }
    assert!(
        count > 100,
        "actual output has too few complete tone periods"
    );
    rate as f64 * (count - 1) as f64 / (last - first.unwrap())
}

fn measured_worker(format: PcmFormat, pitch_ratio: f64) {
    let settings = PlaybackProcessingSettings {
        playback_rate: 2.0,
        preserves_pitch: true,
        pitch_ratio,
    };
    let input = tone_input(format);
    let expected_frames = input.len().div_ceil(2);
    let mut actual = Vec::with_capacity(expected_frames);
    let setup_started = Instant::now();
    let mut worker = WorkerProcessor::new(format).unwrap();
    worker.set_settings(settings, SEQUENCE).unwrap();
    let setup_elapsed = setup_started.elapsed();

    // Input synthesis, owned-storage admission and independent output analysis
    // are excluded from this measurement. The production worker is unchanged.
    let started = Instant::now();
    for (original_frame, samples) in input.iter().enumerate() {
        while !worker.push_input(original_frame as u64, &samples[..format.channels as usize]) {
            assert!(
                worker.input_error().is_none(),
                "actual worker rejected input: {:?}",
                worker.input_error()
            );
            let frame = worker
                .pop_output()
                .expect("full input storage must make actual bounded progress");
            assert!(actual.len() < expected_frames);
            actual.push(frame);
        }
        while let Some(frame) = worker.pop_output() {
            assert!(actual.len() < expected_frames);
            actual.push(frame);
        }
    }
    worker.finish();
    while let Some(frame) = worker.pop_output() {
        assert!(actual.len() < expected_frames);
        actual.push(frame);
    }
    let elapsed = started.elapsed();

    assert!(
        worker.drained(),
        "actual worker did not finish its owned PCM"
    );
    assert_eq!(actual.len(), expected_frames);
    let end = actual.last().unwrap().stamp;
    assert_eq!((end.end_frame, end.fraction), (input.len() as u64, 0));
    assert!(actual.windows(2).all(|frames| {
        (frames[0].stamp.end_frame, frames[0].stamp.fraction)
            <= (frames[1].stamp.end_frame, frames[1].stamp.fraction)
    }));
    let tempo_settings = PlaybackProcessingSettings {
        pitch_ratio: 1.0,
        ..settings
    };
    assert!(actual.iter().all(|frame| {
        frame.stamp.processing_sequence == SEQUENCE
            && frame.stamp.settings == tempo_settings
            && frame.samples.iter().all(|sample| sample.is_finite())
    }));
    // Independent pitch is applied by the output stage. A worker-only timing
    // must not claim that an unapplied pitch ratio changed these PCM samples.
    let edge = format.sample_rate as usize / 5;
    let body = &actual[edge..actual.len() - edge];
    let mut frequencies = [0.0; MAX_CHANNELS];
    for (channel, frequency) in frequencies
        .iter_mut()
        .enumerate()
        .take(format.channels as usize)
    {
        *frequency = positive_crossing_frequency(body, format.sample_rate, channel);
        assert!(
            (*frequency - TONE_HZ).abs() < 0.6,
            "actual worker pitch changed: format={format:?}, settings={settings:?}, channel={channel}, measured={frequency}Hz"
        );
        let rms = (body
            .iter()
            .map(|frame| f64::from(frame.samples[channel]).powi(2))
            .sum::<f64>()
            / body.len() as f64)
            .sqrt();
        assert!(rms > AMPLITUDE / 3.0, "actual worker output lost its tone");
    }
    assert!(actual.iter().all(|frame| {
        frame.samples[format.channels as usize..]
            .iter()
            .all(|sample| *sample == 0.0)
    }));

    let output_seconds = actual.len() as f64 / format.sample_rate as f64;
    eprintln!(
        "worker throughput: source_rate={} channels={} requested_rate={} requested_pitch={} worker_pitch={} input_frames={} output_frames={} output_seconds={output_seconds:.6} setup_ms={:.3} elapsed_seconds={:.6} elapsed_per_output_second={:.6} output_seconds_per_elapsed_second={:.6} frequency_hz={:?} final_stamp=({}, {})",
        format.sample_rate,
        format.channels,
        settings.playback_rate,
        settings.pitch_ratio,
        tempo_settings.pitch_ratio,
        input.len(),
        actual.len(),
        setup_elapsed.as_secs_f64() * 1000.0,
        elapsed.as_secs_f64(),
        elapsed.as_secs_f64() / output_seconds,
        output_seconds / elapsed.as_secs_f64(),
        &frequencies[..format.channels as usize],
        end.end_frame,
        end.fraction,
    );
}

#[test]
fn actual_preserved_double_rate_worker_throughput_at_both_source_formats_and_pitch_requests() {
    for format in [
        PcmFormat {
            sample_rate: 22_050,
            channels: 1,
        },
        PcmFormat {
            sample_rate: 48_000,
            channels: 2,
        },
    ] {
        for pitch_ratio in [1.0, 1.5] {
            measured_worker(format, pitch_ratio);
        }
    }
}

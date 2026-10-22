use lx_gpui_player::audio::RenderedPcm;
use lx_gpui_player::audio::output::Output;
use lx_gpui_player::audio::pcm::{MAX_CHANNELS, PcmControl, PcmFormat, PcmProducer, PcmSource};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

fn wait_until(mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(4);
    while !condition() {
        assert!(Instant::now() < deadline, "PCM output condition timed out");
        thread::sleep(Duration::from_millis(5));
    }
}

fn tone(
    format: PcmFormat,
    frequency: f32,
    seconds: f32,
) -> (PcmSource, PcmControl, thread::JoinHandle<()>) {
    let (mut producer, source, control) = PcmProducer::new(format, 4_096, 0, None).unwrap();
    let producer_control = control.clone();
    let worker = thread::spawn(move || {
        let frames = (format.sample_rate as f32 * seconds) as u64;
        for index in 0..frames {
            let sample = (index as f32 * frequency * std::f32::consts::TAU
                / format.sample_rate as f32)
                .sin()
                * 0.4;
            let frame = [sample; MAX_CHANNELS];
            while !producer.push_frame(index, &frame[..format.channels as usize]) {
                if !producer_control.consumer_alive() {
                    return;
                }
                thread::sleep(Duration::from_micros(50));
            }
        }
        producer.finish();
    });
    (source, control, worker)
}

#[test]
fn memory_backend_resamples_and_applies_live_pause_and_gain_to_pcm() {
    let observation = Arc::new(Mutex::new(RenderedPcm::default()));
    let mut output = Output::memory(observation.clone()).unwrap();
    assert_eq!(
        output.format(),
        PcmFormat {
            sample_rate: 22_050,
            channels: 1
        }
    );
    let (source, control, producer) = tone(
        PcmFormat {
            sample_rate: 48_000,
            channels: 8,
        },
        997.0,
        3.0,
    );
    output.attach(source).unwrap();
    wait_until(|| {
        let pcm = observation.lock().unwrap();
        pcm.rms > 0.1
            && pcm
                .frequency_hz
                .is_some_and(|frequency| (frequency - 997.0).abs() < 2.0)
    });

    output.set_paused(true);
    wait_until(|| observation.lock().unwrap().peak == 0.0);
    let paused = control.snapshot().consumed_frames;
    thread::sleep(Duration::from_millis(80));
    assert_eq!(control.snapshot().consumed_frames, paused);

    output.set_gain(0.0);
    output.set_paused(false);
    wait_until(|| control.snapshot().consumed_frames > paused + 4_800);
    assert_eq!(observation.lock().unwrap().peak, 0.0);
    assert!(!output.ended());
    output.set_gain(0.5);
    wait_until(|| {
        let pcm = observation.lock().unwrap();
        pcm.rms > 0.10 && pcm.rms < 0.16 && pcm.peak > 0.18 && pcm.peak <= 0.201
    });
    assert!(output.poll_error().is_none());

    // Setters run concurrently with the production Memory render thread.
    for index in 0..400 {
        output.set_paused(index % 3 == 0);
        output.set_gain(if index % 2 == 0 { 0.0 } else { 0.5 });
    }
    output.set_paused(false);
    output.set_gain(0.5);
    output.clear();
    wait_until(|| {
        output.reap();
        !control.consumer_alive()
    });
    assert!(!control.snapshot().cancelled);
    wait_until(|| observation.lock().unwrap().peak == 0.0);
    drop(output);
    producer.join().unwrap();
}

#[test]
fn memory_eof_keeps_source_owned_until_clear_and_shutdown_joins_renderer() {
    let observation = Arc::new(Mutex::new(RenderedPcm::default()));
    let mut output = Output::memory(observation.clone()).unwrap();
    let (source, control, producer) = tone(
        PcmFormat {
            sample_rate: 16_000,
            channels: 2,
        },
        440.0,
        0.12,
    );
    let attached = output.attach(source).unwrap();
    let token = output.token();
    assert_ne!(token, 0);
    wait_until(|| output.ended());
    producer.join().unwrap();
    assert!(control.snapshot().normal_eof);
    assert!(control.snapshot().drained);
    assert_eq!(attached.snapshot().consumed_frames, 1_920);
    assert!(control.consumer_alive());
    assert!(!control.snapshot().cancelled);
    assert_eq!(output.reap(), 0);
    output.clear();
    assert_eq!(output.token(), 0);
    assert!(!output.ended());
    wait_until(|| {
        output.reap();
        !control.consumer_alive()
    });
    assert!(control.snapshot().normal_eof);
    assert!(!control.snapshot().cancelled);
    drop(output);
    let batches = observation.lock().unwrap().batches;
    thread::sleep(Duration::from_millis(40));
    assert_eq!(observation.lock().unwrap().batches, batches);
}

#[test]
fn dropping_memory_output_reclaims_a_source_and_unblocks_its_producer() {
    let observation = Arc::new(Mutex::new(RenderedPcm::default()));
    let mut output = Output::memory(observation).unwrap();
    output.set_paused(true);
    let (source, control, producer) = tone(
        PcmFormat {
            sample_rate: 44_100,
            channels: 2,
        },
        330.0,
        10.0,
    );
    output.attach(source).unwrap();
    wait_until(|| control.snapshot().queued_frames == 4_096);
    assert_eq!(control.snapshot().consumed_frames, 0);
    drop(output);
    assert!(!control.consumer_alive());
    assert!(!control.snapshot().cancelled);
    producer.join().unwrap();
}

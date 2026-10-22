use lx_gpui_player::audio::pcm::{GLOBAL_PCM_BYTES, PcmControl, PcmFormat, PcmProducer};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

#[test]
fn empty_pause_error_and_cancel_do_not_fake_eof_or_advance_media_clock() {
    let (mut producer, mut source, control) = PcmProducer::new(
        PcmFormat {
            sample_rate: 1000,
            channels: 2,
        },
        8,
        0,
        None,
    )
    .unwrap();
    for _ in 0..100 {
        assert_eq!(source.next(), Some(0.0));
    }
    assert_eq!(control.snapshot().position_seconds, 0.0);
    assert_eq!(control.snapshot().consumed_frames, 0);
    assert!(producer.push_frame(0, &[0.25, -0.5]));
    control.set_paused(true);
    assert_eq!(source.next(), Some(0.0));
    assert_eq!(source.next(), Some(0.0));
    assert_eq!(control.snapshot().queued_frames, 1);
    control.set_paused(false);
    assert_eq!(source.next(), Some(0.25));
    assert_eq!(control.snapshot().position_seconds, 0.0);
    assert_eq!(source.next(), Some(-0.5));
    assert_eq!(control.snapshot().position_seconds, 0.001);
    control.fail();
    for _ in 0..20 {
        assert_eq!(source.next(), Some(0.0));
    }
    assert_eq!(control.snapshot().position_seconds, 0.001);
    assert!(!control.snapshot().normal_eof);
    control.cancel();
    assert_eq!(source.next(), Some(0.0));
    assert!(control.snapshot().cancelled);
}

#[test]
fn only_real_eof_after_the_complete_last_frame_is_drained_returns_none() {
    let (mut producer, mut source, control) = PcmProducer::new(
        PcmFormat {
            sample_rate: 48000,
            channels: 2,
        },
        1,
        0,
        None,
    )
    .unwrap();
    assert!(producer.push_frame(0, &[0.1, 0.2]));
    assert!(!producer.push_frame(1, &[0.3, 0.4]));
    producer.finish();
    assert_eq!(source.next(), Some(0.1));
    assert!(!control.snapshot().drained);
    assert_eq!(source.next(), Some(0.2));
    assert!(!control.snapshot().drained);
    assert_eq!(source.next(), None);
    assert!(control.snapshot().drained);
    assert_eq!(control.snapshot().consumed_frames, 1);
}

#[test]
fn paused_seek_invalidates_a_full_queue_and_rejects_a_partial_old_frame() {
    let (mut producer, mut source, control) = PcmProducer::new(
        PcmFormat {
            sample_rate: 1000,
            channels: 2,
        },
        2,
        0,
        None,
    )
    .unwrap();
    assert!(producer.push_frame(0, &[1.0, 2.0]));
    assert!(producer.push_frame(1, &[3.0, 4.0]));
    assert_eq!(source.next(), Some(1.0));
    control.set_paused(true);
    producer.begin_epoch().unwrap();
    assert_eq!(producer.queued_frames(), 0);
    assert!(producer.push_frame(2000, &[5.0, 6.0]));
    producer.commit_epoch(2000).unwrap();
    assert_eq!(control.snapshot().position_seconds, 2.0);
    assert_eq!(source.next(), Some(0.0));
    assert_eq!(control.snapshot().position_seconds, 2.0);
    control.set_paused(false);
    assert_eq!(source.next(), Some(5.0));
    assert_eq!(source.next(), Some(6.0));
    assert_eq!(control.snapshot().position_seconds, 2.001);
}

#[test]
fn epochs_extend_past_65535_without_clock_wrap_or_failure() {
    let (mut producer, mut source, control) = PcmProducer::new(
        PcmFormat {
            sample_rate: 1000,
            channels: 1,
        },
        1,
        0,
        None,
    )
    .unwrap();
    for epoch in 2..70_000_u64 {
        assert_eq!(producer.begin_epoch().unwrap(), epoch);
        producer.commit_epoch(epoch * 10).unwrap();
    }
    assert!(producer.push_frame(700_000, &[0.75]));
    assert_eq!(source.next(), Some(0.75));
    assert_eq!(control.snapshot().position_seconds, 700.001);
}

#[test]
fn concurrent_reset_and_slot_reuse_never_returns_torn_channel_frames() {
    let (mut producer, mut source, control) = PcmProducer::new(
        PcmFormat {
            sample_rate: 1000,
            channels: 2,
        },
        2,
        0,
        None,
    )
    .unwrap();
    let done = Arc::new(AtomicBool::new(false));
    let worker_done = done.clone();
    let consumer = thread::spawn(move || {
        while !worker_done.load(Ordering::Acquire) {
            let first = source.next().unwrap();
            let second = source.next().unwrap();
            // An epoch may change between channels; silence is then intentional.
            assert!(first == 0.0 || second == 0.0 || second == -first);
        }
    });
    for epoch in 2..30_000_u64 {
        producer.begin_epoch().unwrap();
        let value = epoch as f32;
        assert!(producer.push_frame(epoch * 4, &[value, -value]));
        producer.commit_epoch(epoch * 4).unwrap();
        let snapshot = control.snapshot();
        assert!(snapshot.position_seconds >= epoch as f64 * 4.0 / 1000.0);
        assert!(snapshot.position_seconds <= (epoch * 4 + 1) as f64 / 1000.0);
    }
    done.store(true, Ordering::Release);
    consumer.join().unwrap();
}

#[test]
fn allocation_is_finite_and_charged_globally_including_scratch() {
    let oversize = PcmProducer::new(
        PcmFormat {
            sample_rate: 44100,
            channels: 2,
        },
        1,
        GLOBAL_PCM_BYTES,
        None,
    );
    assert!(oversize.is_err());
    let (producer, source, control) = PcmProducer::new(
        PcmFormat {
            sample_rate: 44100,
            channels: 2,
        },
        16,
        4096,
        None,
    )
    .unwrap();
    let snapshot = control.snapshot();
    assert_eq!(snapshot.capacity_frames, 16);
    assert!(snapshot.allocated_bytes >= 4096 + 16 * 32);
    assert!(PcmControl::globally_allocated_bytes() <= GLOBAL_PCM_BYTES);
    drop(producer);
    drop(source);
    drop(control);
}

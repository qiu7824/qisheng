use lx_gpui_player::{
    audio::pcm::{
        GLOBAL_PCM_BYTES, MAX_CHANNELS, MAX_CLOCK_FRAMES, PcmControl, PcmFormat, PcmProducer,
        ProcessingStamp,
    },
    domain::playback_processing::PlaybackProcessingSettings,
};
use std::{
    alloc::{GlobalAlloc, Layout, System},
    cell::Cell,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
};

static CASES: Mutex<()> = Mutex::new(());
const HALF: u32 = 1 << 31;

thread_local! {
    static WATCH: Cell<bool> = const { Cell::new(false) };
    static HEAP: Cell<(usize, usize)> = const { Cell::new((0, 0)) };
}
struct Allocator;
unsafe impl GlobalAlloc for Allocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let _ = WATCH.try_with(|watch| {
            if watch.get() {
                HEAP.with(|calls| {
                    let (a, d) = calls.get();
                    calls.set((a + 1, d));
                });
            }
        });
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        let _ = WATCH.try_with(|watch| {
            if watch.get() {
                HEAP.with(|calls| {
                    let (a, d) = calls.get();
                    calls.set((a, d + 1));
                });
            }
        });
        unsafe { System.dealloc(pointer, layout) }
    }
}
#[global_allocator]
static ALLOCATOR: Allocator = Allocator;

fn no_heap(operation: impl FnOnce()) {
    struct Guard;
    impl Drop for Guard {
        fn drop(&mut self) {
            WATCH.with(|watch| watch.set(false));
        }
    }
    HEAP.with(|calls| calls.set((0, 0)));
    WATCH.with(|watch| watch.set(true));
    let guard = Guard;
    operation();
    drop(guard);
    assert_eq!(
        HEAP.with(Cell::get),
        (0, 0),
        "PCM render metadata touched the heap"
    );
}

fn slow() -> PlaybackProcessingSettings {
    PlaybackProcessingSettings {
        playback_rate: 0.5,
        preserves_pitch: true,
        pitch_ratio: 1.0,
    }
}
fn fast() -> PlaybackProcessingSettings {
    PlaybackProcessingSettings {
        playback_rate: 2.0,
        preserves_pitch: false,
        pitch_ratio: 1.5,
    }
}
fn stamp(
    end_frame: u64,
    fraction: u32,
    sequence: u64,
    settings: PlaybackProcessingSettings,
) -> ProcessingStamp {
    ProcessingStamp {
        end_frame,
        fraction,
        processing_sequence: sequence,
        settings,
        latency_frames: if sequence & 1 == 0 { 47 } else { 31 },
    }
}
fn format(channels: u16) -> PcmFormat {
    PcmFormat {
        sample_rate: 1000,
        channels,
    }
}

#[test]
fn legacy_complete_frames_keep_the_original_clock_and_default_processing_acknowledgement() {
    let _case = CASES.lock().unwrap();
    let (mut producer, mut source, control) = PcmProducer::new(format(2), 2, 0, None).unwrap();
    assert_eq!(control.snapshot().processing_sample_rate, None);
    assert!(producer.push_frame(10, &[0.25, -0.5]));
    assert_eq!(source.next(), Some(0.25));
    assert_eq!(source.current_source_stamp(), None);
    assert_eq!(control.snapshot().position_seconds, 0.0);
    assert_eq!(source.next(), Some(-0.5));
    let (actual, epoch) = source.current_source_stamp().unwrap();
    assert_eq!(actual.end_frame, 11);
    assert_eq!(actual.fraction, 0);
    assert_eq!(actual.settings, PlaybackProcessingSettings::default());
    assert_eq!(actual.processing_sequence, 0);
    assert_eq!(epoch, 1);
    let snapshot = control.snapshot();
    assert_eq!(snapshot.position_seconds, 0.011);
    assert_eq!(snapshot.consumed_frames, 1);
    assert_eq!(snapshot.processing_sample_rate, Some(1000));
    assert_eq!(
        snapshot.processing_settings,
        PlaybackProcessingSettings::default()
    );
}

#[test]
fn deferred_lookahead_and_queued_rate_changes_acknowledge_the_emitted_fractional_stamp() {
    let _case = CASES.lock().unwrap();
    let (mut producer, mut source, control) = PcmProducer::new(format(1), 4, 0, None).unwrap();
    source.set_deferred_clock(true);
    let stamps = [
        stamp(0, HALF, 1, slow()),
        stamp(1, 0, 1, slow()),
        stamp(3, 0, 2, fast()),
        stamp(5, 0, 2, fast()),
    ];
    for (index, stamp) in stamps.iter().copied().enumerate() {
        assert!(producer.push_processed_frame(stamp, &[index as f32 / 10.0]));
    }
    let baseline = source.presentation_baseline().unwrap();
    assert_eq!(baseline, (0, 0, 1));
    assert_eq!(source.next(), Some(0.0));
    let first = source.current_source_stamp().unwrap();
    assert_eq!(source.next(), Some(0.1));
    let second = source.current_source_stamp().unwrap();
    assert_eq!(control.snapshot().consumed_frames, 2);
    assert_eq!(control.snapshot().position_seconds, 0.0);
    assert_eq!(control.snapshot().processing_sample_rate, None);
    assert!(control.commit_rendered_stamp(first.0, first.1));
    let rendered = control.snapshot();
    assert_eq!(rendered.position_seconds, 0.0005);
    assert_eq!(rendered.processing_settings, slow());
    assert_eq!(rendered.processing_sequence, 1);
    assert_eq!(rendered.processing_latency_frames, 31);
    assert!(control.commit_rendered_stamp(second.0, second.1));
    assert_eq!(control.snapshot().position_seconds, 0.001);
    for expected in &stamps[2..] {
        source.next().unwrap();
        let (actual, epoch) = source.current_source_stamp().unwrap();
        assert_eq!(actual, *expected);
        assert!(control.commit_rendered_stamp(actual, epoch));
    }
    let rendered = control.snapshot();
    assert_eq!(rendered.position_seconds, 0.005);
    assert_eq!(rendered.consumed_frames, 4);
    assert_eq!(rendered.processing_settings, fast());
    assert_eq!(rendered.processing_sequence, 2);
    assert!(!control.commit_rendered_stamp(first.0, first.1));
    assert_eq!(control.snapshot().position_seconds, 0.005);
}

#[test]
fn real_zero_samples_commit_but_underflow_pause_and_failure_do_not_advance_the_clock() {
    let _case = CASES.lock().unwrap();
    let (mut producer, mut source, control) = PcmProducer::new(format(1), 2, 0, None).unwrap();
    source.set_deferred_clock(true);
    assert_eq!(source.next(), Some(0.0));
    assert_eq!(source.current_source_stamp(), None);
    assert_eq!(control.snapshot().consumed_frames, 0);
    let emitted = stamp(5, HALF, 7, slow());
    assert!(producer.push_processed_frame(emitted, &[0.0]));
    assert_eq!(source.next(), Some(0.0));
    let (actual, epoch) = source.current_source_stamp().unwrap();
    assert!(control.commit_rendered_stamp(actual, epoch));
    assert_eq!(control.snapshot().position_seconds, 0.0055);
    control.set_paused(true);
    assert_eq!(source.next(), Some(0.0));
    assert_eq!(source.current_source_stamp(), None);
    assert!(!control.commit_rendered_stamp(stamp(9, 0, 8, fast()), epoch));
    control.set_paused(false);
    assert_eq!(source.next(), Some(0.0));
    assert_eq!(source.last_frame_stamp(), None);
    control.fail();
    assert!(!control.commit_rendered_stamp(stamp(9, 0, 8, fast()), epoch));
    assert_eq!(control.snapshot().position_seconds, 0.0055);
    assert_eq!(control.snapshot().consumed_frames, 1);
}

#[test]
fn paused_seek_rejects_delayed_old_stamps_and_partial_old_stereo_frames() {
    let _case = CASES.lock().unwrap();
    let (mut producer, mut source, control) = PcmProducer::new(format(2), 4, 0, None).unwrap();
    source.set_deferred_clock(true);
    let old = stamp(100, HALF, 3, fast());
    assert!(producer.push_processed_frame(old, &[0.25, -0.25]));
    assert_eq!(source.next(), Some(0.25));
    assert_eq!(source.next(), Some(-0.25));
    let old_epoch = source.current_epoch();
    assert!(producer.push_processed_frame(old, &[0.75, -0.75]));
    assert_eq!(source.next(), Some(0.75));
    control.set_paused(true);
    producer.begin_epoch().unwrap();
    producer.commit_epoch(500).unwrap();
    assert_eq!(source.presentation_baseline(), Some((500, 0, 2)));
    assert_eq!(control.snapshot().position_seconds, 0.5);
    assert!(!control.commit_rendered_stamp(old, old_epoch));
    assert_eq!(source.next(), Some(0.0));
    assert_eq!(source.current_source_stamp(), None);
    let new = stamp(500, HALF, 15, slow());
    assert!(producer.push_processed_frame(new, &[0.5, -0.5]));
    control.set_paused(false);
    assert_eq!(source.next(), Some(0.5));
    assert_eq!(source.next(), Some(-0.5));
    let (actual, epoch) = source.current_source_stamp().unwrap();
    assert_eq!(actual, new);
    assert_eq!(control.snapshot().position_seconds, 0.5);
    assert_eq!(control.snapshot().processing_sample_rate, None);
    assert!(control.commit_rendered_stamp(actual, epoch));
    assert_eq!(control.snapshot().position_seconds, 0.5005);
    assert_eq!(control.snapshot().processing_sequence, 15);
    assert_eq!(control.snapshot().consumed_frames, 2);
}

#[test]
fn concurrent_presented_clock_reads_never_mix_fraction_sequence_and_processing_settings() {
    let _case = CASES.lock().unwrap();
    const FRAMES: u64 = 10_000;
    let (mut producer, mut source, control) = PcmProducer::new(format(1), 16, 0, None).unwrap();
    source.set_deferred_clock(true);
    let done = Arc::new(AtomicBool::new(false));
    let finished = done.clone();
    let rendered_control = control.clone();
    let renderer = thread::spawn(move || {
        while source.next().is_some() {
            if let Some((stamp, epoch)) = source.current_source_stamp() {
                assert!(rendered_control.commit_rendered_stamp(stamp, epoch));
            } else {
                thread::yield_now();
            }
        }
        finished.store(true, Ordering::Release);
    });
    for frame in 1..=FRAMES {
        let expected = stamp(
            frame,
            if frame & 1 == 0 { 0 } else { HALF },
            frame,
            if frame & 1 == 0 { fast() } else { slow() },
        );
        while !producer.push_processed_frame(expected, &[0.01]) {
            thread::yield_now();
        }
        let read = control.snapshot();
        if read.processing_sample_rate.is_some() {
            let sequence = read.processing_sequence;
            assert!(sequence > 0);
            let fraction = if sequence & 1 == 0 { 0.0 } else { 0.5 };
            assert_eq!(read.position_seconds, (sequence as f64 + fraction) / 1000.0);
            assert_eq!(
                read.processing_settings,
                if sequence & 1 == 0 { fast() } else { slow() }
            );
            assert_eq!(
                read.processing_latency_frames,
                if sequence & 1 == 0 { 47 } else { 31 }
            );
        }
    }
    producer.finish();
    while !done.load(Ordering::Acquire) {
        thread::yield_now();
    }
    renderer.join().unwrap();
    let final_state = control.snapshot();
    assert_eq!(final_state.processing_sequence, FRAMES);
    assert_eq!(final_state.position_seconds, 10.0);
    assert_eq!(final_state.consumed_frames, FRAMES);
    assert!(final_state.drained);
}

#[test]
fn stamped_pcm_quota_includes_all_slot_metadata_and_rejects_invalid_tags() {
    let _case = CASES.lock().unwrap();
    let baseline = PcmControl::globally_allocated_bytes();
    assert!(PcmProducer::new(format(1), 100_000, 0, None).is_err());
    assert_eq!(PcmControl::globally_allocated_bytes(), baseline);
    let (mut producer, mut source, control) = PcmProducer::new(format(1), 2, 0, None).unwrap();
    let allocated = control.snapshot().allocated_bytes;
    let minimum_slot = std::mem::size_of::<ProcessingStamp>()
        + std::mem::size_of::<[f32; MAX_CHANNELS]>()
        + 2 * std::mem::size_of::<u64>();
    assert!(allocated >= 2 * minimum_slot + std::mem::size_of::<[f32; MAX_CHANNELS]>());
    assert!(allocated < GLOBAL_PCM_BYTES);
    let invalid = [
        ProcessingStamp {
            end_frame: u64::MAX,
            ..Default::default()
        },
        ProcessingStamp {
            end_frame: MAX_CLOCK_FRAMES,
            fraction: 1,
            ..Default::default()
        },
        ProcessingStamp {
            settings: PlaybackProcessingSettings {
                playback_rate: f64::NAN,
                ..Default::default()
            },
            ..Default::default()
        },
        ProcessingStamp {
            settings: PlaybackProcessingSettings {
                pitch_ratio: 1.51,
                ..Default::default()
            },
            ..Default::default()
        },
    ];
    for stamp in invalid {
        assert!(!producer.push_processed_frame(stamp, &[0.1]));
        assert!(!control.commit_rendered_stamp(stamp, 1));
    }
    assert_eq!(control.snapshot().queued_frames, 0);
    assert!(!producer.push_frame(MAX_CLOCK_FRAMES, &[0.1]));
    assert!(producer.push_frame(MAX_CLOCK_FRAMES - 1, &[0.1]));
    assert_eq!(source.next(), Some(0.1));
    assert!(control.snapshot().position_seconds.is_finite());
    drop(producer);
    drop(source);
    drop(control);
    assert_eq!(PcmControl::globally_allocated_bytes(), baseline);
}

#[test]
fn frame_capture_snapshot_and_deferred_render_commit_allocate_and_free_nothing() {
    let _case = CASES.lock().unwrap();
    let (mut producer, mut source, control) = PcmProducer::new(format(2), 2, 0, None).unwrap();
    source.set_deferred_clock(true);
    let expected = stamp(1, HALF, 1, slow());
    assert!(producer.push_processed_frame(expected, &[0.1, -0.1]));
    no_heap(|| {
        assert_eq!(source.next(), Some(0.1));
        assert_eq!(source.next(), Some(-0.1));
        let (actual, epoch) = source.current_source_stamp().unwrap();
        assert_eq!(actual, expected);
        assert!(control.commit_rendered_stamp(actual, epoch));
        assert_eq!(control.snapshot().position_seconds, 0.0015);
        assert_eq!(source.presentation_baseline(), Some((0, 0, 1)));
    });
}

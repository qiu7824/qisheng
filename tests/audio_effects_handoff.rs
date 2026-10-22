use lx_gpui_player::audio::{
    convolution::{ConvolutionStamp, ImpulseBuilder, PreparedConvolution},
    effects::{
        EFFECTS_MIN_REPLAY_FRAMES, EffectsAlignment, EffectsFrame, EffectsFrameKind, EffectsInput,
        EffectsPitch, EffectsPreparation, PreparedEffects,
    },
    effects_handoff::{HandoffBounds, HandoffError, HandoffReservation, RetainedHandoff},
    pcm::{PcmFormat, ProcessingStamp},
    spatial::{SpatialPlan, SpatialSettings, SpatialStep},
};
use std::{
    alloc::{GlobalAlloc, Layout, System},
    cell::Cell,
    sync::{Arc, atomic::AtomicBool},
};

thread_local! {
    static WATCH: Cell<bool> = const { Cell::new(false) };
    static HEAP: Cell<(usize, usize)> = const { Cell::new((0, 0)) };
}
struct Allocator;
unsafe impl GlobalAlloc for Allocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let _ = WATCH.try_with(|watch| {
            if watch.get() {
                HEAP.with(|heap| {
                    let (a, d) = heap.get();
                    heap.set((a + 1, d));
                });
            }
        });
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        let _ = WATCH.try_with(|watch| {
            if watch.get() {
                HEAP.with(|heap| {
                    let (a, d) = heap.get();
                    heap.set((a, d + 1));
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
    HEAP.with(|heap| heap.set((0, 0)));
    WATCH.with(|watch| watch.set(true));
    let guard = Guard;
    operation();
    drop(guard);
    assert_eq!(HEAP.with(Cell::get), (0, 0));
}
fn graph(rate: u32, device: u16, capacity: usize, impulse_frames: usize) -> Box<PreparedEffects> {
    let format = PcmFormat {
        sample_rate: rate,
        channels: 2,
    };
    let spatial = SpatialPlan::prepare(SpatialSettings::default(), rate, 7).unwrap();
    let bank = if impulse_frames == 0 {
        None
    } else {
        let mut builder = ImpulseBuilder::prepare(rate, 1, impulse_frames).unwrap();
        for index in 0..impulse_frames {
            builder
                .push_frame(&[if index == 0 {
                    1.0
                } else if index == impulse_frames - 1 {
                    0.25
                } else {
                    0.0
                }])
                .unwrap();
        }
        Some(
            PreparedConvolution::prepare(
                format,
                &builder.finish().unwrap(),
                false,
                &AtomicBool::new(false),
            )
            .unwrap(),
        )
    };
    Box::new(
        PreparedEffects::prepare(
            EffectsPreparation {
                format,
                device_channels: device,
                main_gain: 1.0,
                send_gain: if impulse_frames == 0 { 0.0 } else { 1.0 },
                alignment: if impulse_frames == 0 {
                    EffectsAlignment::Direct
                } else {
                    EffectsAlignment::Partition512
                },
                sequence: 7,
                token: 1,
                source_epoch: 2,
                initial_spatial: spatial.target(SpatialStep::ORIGIN),
                replay_capacity: capacity,
            },
            bank,
            &AtomicBool::new(false),
        )
        .unwrap(),
    )
}
fn input(index: u64) -> EffectsInput {
    let stamp = ConvolutionStamp {
        token: 1,
        source_epoch: 2,
        media: ProcessingStamp {
            end_frame: index,
            processing_sequence: index,
            latency_frames: 19,
            ..Default::default()
        },
    };
    EffectsInput {
        samples: [
            0.1 + index as f64 / 10_000.0,
            -0.2,
            0.0,
            0.0,
            0.0,
            0.0,
            0.0,
            0.0,
        ],
        stamp,
        begin: Some(ConvolutionStamp {
            media: ProcessingStamp {
                end_frame: index - 1,
                ..stamp.media
            },
            ..stamp
        }),
        pitch: EffectsPitch {
            ratio: 1.25,
            sequence: index + 3,
            transition_remaining_frames: 0,
        },
    }
}
fn reserve(old: &PreparedEffects, new: Box<PreparedEffects>) -> HandoffReservation {
    let plan = new.preparation();
    HandoffReservation::reserve(
        HandoffBounds::from_graph(old),
        plan.format,
        plan.device_channels,
        Some(new),
        &AtomicBool::new(false),
    )
    .unwrap_or_else(|failure| panic!("{:?}", failure.error))
}
fn capture(reservation: HandoffReservation, old: Box<PreparedEffects>) -> RetainedHandoff {
    let mut retained = reservation
        .capture(old, &AtomicBool::new(false))
        .unwrap_or_else(|failure| panic!("{:?}", failure.error));
    while !retained.advance_capture(&AtomicBool::new(false)).unwrap() {}
    retained
}
fn present(graph: &mut PreparedEffects, frame: EffectsFrame) {
    if frame.kind != EffectsFrameKind::Padding {
        graph.release_pending(&frame, true).unwrap();
    }
}

#[test]
fn retained_media_is_resampled_once_and_old_presented_counters_are_not_faked() {
    let mut old = graph(8_000, 2, EFFECTS_MIN_REPLAY_FRAMES, 0);
    let mut oracle = graph(8_000, 2, EFFECTS_MIN_REPLAY_FRAMES, 0);
    let mut expected = Vec::new();
    for index in 1..=200 {
        let a = old.frame(Some(input(index))).unwrap();
        let b = oracle.frame(Some(input(index))).unwrap();
        if index != 200 {
            present(&mut old, a);
        } else {
            expected.push(b);
        }
        present(&mut oracle, b);
    }
    let presented = old.statistics().presented_media_frames;
    let pending_intervals = old.pending_media_frames();
    oracle.begin_eof().unwrap();
    while let Some(frame) = oracle.drain().unwrap() {
        if frame.kind == EffectsFrameKind::Media {
            expected.push(frame);
        }
        present(&mut oracle, frame);
    }
    assert_eq!(expected.len(), pending_intervals);
    let reservation = reserve(&old, graph(16_000, 2, EFFECTS_MIN_REPLAY_FRAMES, 0));
    let mut retained = capture(reservation, old);
    assert_eq!(
        retained
            .old_graph()
            .unwrap()
            .statistics()
            .presented_media_frames,
        presented
    );
    assert!(retained.old_graph().unwrap().drained());
    assert_eq!(retained.retained_media_intervals(), expected.len());
    assert!(retained.new_graph_mut().is_none());
    let original_start = expected.first().unwrap().begin.unwrap().media.end_frame;
    let original_end = expected.last().unwrap().stamp.unwrap().media.end_frame;
    let mut count = 0;
    let mut previous = (original_start, 0);
    while let Some(frame) = retained.next_media().unwrap() {
        let position = count as f64 / 2.0;
        let left = position.floor() as usize;
        let right = (left + 1).min(expected.len() - 1);
        let fraction = position.fract();
        for lane in 0..2 {
            let expected_sample = expected[left.min(expected.len() - 1)].samples[lane]
                + (expected[right].samples[lane]
                    - expected[left.min(expected.len() - 1)].samples[lane])
                    * fraction;
            assert_eq!(frame.samples[lane], expected_sample);
        }
        let begin = frame.begin.unwrap().media;
        assert_eq!((begin.end_frame, begin.fraction), previous);
        let end = frame.stamp.unwrap().media;
        previous = (end.end_frame, end.fraction);
        assert_eq!(frame.token, 1);
        assert_eq!(frame.source_epoch, 2);
        retained.release_pending(&frame, true).unwrap();
        count += 1;
    }
    assert_eq!(count, expected.len() * 2);
    assert_eq!(previous, (original_end, 0));
    assert!(retained.media_prefix_drained());
    assert!(retained.tail_drained());
    assert_eq!(
        retained
            .new_graph_mut()
            .unwrap()
            .statistics()
            .accepted_media_frames,
        0
    );
    let old = retained.take_retired_old().unwrap();
    drop(old);
}

#[test]
fn one_over_forty_eight_downsample_covers_the_complete_first_and_last_interval() {
    let mut old = graph(384_000, 2, EFFECTS_MIN_REPLAY_FRAMES, 0);
    for index in 1..=49 {
        assert_eq!(
            old.frame(Some(input(index))).unwrap().kind,
            EffectsFrameKind::Padding
        );
    }
    let mut retained = capture(
        reserve(&old, graph(8_000, 1, EFFECTS_MIN_REPLAY_FRAMES, 0)),
        old,
    );
    let first = retained.next_media().unwrap().unwrap();
    assert_eq!(first.begin.unwrap().media.end_frame, 0);
    assert_eq!(first.stamp.unwrap().media.end_frame, 48);
    retained.release_pending(&first, true).unwrap();
    let second = retained.next_media().unwrap().unwrap();
    assert_eq!(second.begin.unwrap().media.end_frame, 48);
    assert_eq!(second.stamp.unwrap().media.end_frame, 49);
    assert_eq!(second.stamp.unwrap().media.fraction, 0);
    retained.release_pending(&second, true).unwrap();
    assert!(retained.next_media().unwrap().is_none());
}

#[test]
fn false_or_modified_presentations_keep_the_exact_pending_frame() {
    let mut old = graph(8_000, 2, EFFECTS_MIN_REPLAY_FRAMES, 0);
    old.frame(Some(input(1))).unwrap();
    let mut retained = capture(
        reserve(&old, graph(16_000, 1, EFFECTS_MIN_REPLAY_FRAMES, 0)),
        old,
    );
    let original = retained.next_media().unwrap().unwrap();
    retained.release_pending(&original, false).unwrap();
    assert_eq!(retained.next_media().unwrap(), Some(original));
    let mut edited = original;
    edited.samples[0] += 0.01;
    assert_eq!(
        retained.release_pending(&edited, true),
        Err(HandoffError::InvalidReceipt)
    );
    edited = original;
    edited.stamp.as_mut().unwrap().media.end_frame += 1;
    assert_eq!(
        retained.release_pending(&edited, true),
        Err(HandoffError::InvalidReceipt)
    );
    edited = original;
    edited.pitch.ratio = 1.0;
    assert_eq!(
        retained.release_pending(&edited, true),
        Err(HandoffError::InvalidReceipt)
    );
    assert_eq!(retained.pending_output(), Some(original));
    retained.release_pending(&original, true).unwrap();
    assert_eq!(
        retained.release_pending(&original, true),
        Err(HandoffError::InvalidReceipt)
    );
}

#[test]
fn same_format_retains_the_actual_whole_bank_without_preparing_a_second() {
    let mut old = graph(8_000, 1, EFFECTS_MIN_REPLAY_FRAMES, 0);
    for index in 1..=49 {
        old.frame(Some(input(index))).unwrap();
    }
    let pending = old.pending_output();
    let stats = old.statistics();
    let format = old.preparation().format;
    let reservation = HandoffReservation::reserve(
        HandoffBounds::from_graph(&old),
        format,
        1,
        None,
        &AtomicBool::new(false),
    )
    .unwrap_or_else(|failure| panic!("{:?}", failure.error));
    let mut retained = reservation
        .capture(old, &AtomicBool::new(false))
        .unwrap_or_else(|failure| panic!("{:?}", failure.error));
    assert!(retained.same_format());
    assert!(retained.ready());
    assert_eq!(
        retained.whole_graph_mut().unwrap().pending_output(),
        pending
    );
    assert_eq!(retained.whole_graph_mut().unwrap().statistics(), stats);
    assert!(retained.take_retired_old().is_none());
    assert!(retained.take_unused_candidate().is_none());
    let candidate = graph(8_000, 1, EFFECTS_MIN_REPLAY_FRAMES, 0);
    let bounds = HandoffBounds::from_graph(retained.whole_graph_mut().unwrap());
    let failure = match HandoffReservation::reserve(
        bounds,
        format,
        1,
        Some(candidate),
        &AtomicBool::new(false),
    ) {
        Ok(_) => panic!("Same-format configuration replacement was accepted"),
        Err(failure) => failure,
    };
    assert_eq!(failure.error, HandoffError::InvalidFormat);
    assert!(failure.new_graph.is_some());
}

#[test]
fn cancellation_and_missing_interval_return_the_original_owners() {
    let mut old = graph(8_000, 2, EFFECTS_MIN_REPLAY_FRAMES, 0);
    old.frame(Some(input(1))).unwrap();
    let stats = old.statistics();
    let candidate = graph(16_000, 1, EFFECTS_MIN_REPLAY_FRAMES, 0);
    let failure = match HandoffReservation::reserve(
        HandoffBounds::from_graph(&old),
        candidate.preparation().format,
        1,
        Some(candidate),
        &AtomicBool::new(true),
    ) {
        Ok(_) => panic!(),
        Err(failure) => failure,
    };
    assert_eq!(failure.error, HandoffError::Cancelled);
    assert!(failure.new_graph.is_some());
    assert_eq!(old.statistics(), stats);
    let reservation = reserve(&old, failure.new_graph.unwrap());
    let failure = match reservation.capture(old, &AtomicBool::new(true)) {
        Ok(_) => panic!(),
        Err(failure) => failure,
    };
    assert_eq!(failure.error, HandoffError::Cancelled);
    assert_eq!(failure.old_graph.statistics(), stats);
    let mut old = failure.old_graph;
    let mut missing = input(2);
    missing.begin = None;
    old.frame(Some(missing)).unwrap();
    let stats = old.statistics();
    let failure = match failure.reservation.capture(old, &AtomicBool::new(false)) {
        Ok(_) => panic!(),
        Err(failure) => failure,
    };
    assert_eq!(failure.error, HandoffError::MissingBegin);
    assert_eq!(failure.old_graph.statistics(), stats);
    let mut old = failure.old_graph;
    assert!(old.frame(Some(input(3))).is_ok());
}

#[test]
fn interrupted_capture_keeps_fifo_and_old_bank_for_retry_without_fake_ack() {
    let mut old = graph(8_000, 2, EFFECTS_MIN_REPLAY_FRAMES, 3_000);
    old.frame(Some(input(1))).unwrap();
    let mut retained = reserve(&old, graph(16_000, 1, EFFECTS_MIN_REPLAY_FRAMES, 3_000))
        .capture(old, &AtomicBool::new(false))
        .unwrap_or_else(|failure| panic!("{:?}", failure.error));
    assert!(!retained.advance_capture(&AtomicBool::new(false)).unwrap());
    let before = retained.retained_tail_source_frames();
    assert!(before > 0);
    assert_eq!(
        retained.advance_capture(&AtomicBool::new(true)),
        Err(HandoffError::Cancelled)
    );
    assert_eq!(retained.retained_tail_source_frames(), before);
    while !retained.advance_capture(&AtomicBool::new(false)).unwrap() {}
    let stats = retained.old_graph().unwrap().statistics();
    assert_eq!(stats.presented_media_frames, 0);
    assert_eq!(stats.presented_tail_frames, 0);
    assert_eq!(retained.retained_tail_source_frames(), 2_999);
    assert_eq!(retained.next_tail(), Err(HandoffError::MediaPending));
    while let Some(frame) = retained.next_media().unwrap() {
        retained.release_pending(&frame, true).unwrap();
    }
    assert!(retained.new_graph_mut().is_some());
    let mut tail_count = 0;
    while let Some(frame) = retained.next_tail().unwrap() {
        assert_eq!(frame.kind, EffectsFrameKind::Tail);
        assert!(frame.stamp.is_none());
        assert!(frame.begin.is_none());
        assert_eq!(frame.token, 1);
        assert_eq!(frame.source_epoch, 2);
        assert!(frame.samples.iter().all(|sample| sample.is_finite()));
        retained.release_pending(&frame, true).unwrap();
        tail_count += 1;
    }
    assert_eq!(tail_count, 5_998);
    assert!(retained.tail_drained());
}

#[test]
fn actual_large_graph_capacity_is_included_before_capture() {
    fn bytes(capacity: usize) -> usize {
        let old = graph(8_000, 2, capacity, 0);
        let retained = capture(
            reserve(&old, graph(16_000, 1, EFFECTS_MIN_REPLAY_FRAMES, 0)),
            old,
        );
        retained.owned_buffer_bytes()
    }
    let small = bytes(EFFECTS_MIN_REPLAY_FRAMES);
    let large = bytes(EFFECTS_MIN_REPLAY_FRAMES * 2);
    assert!(large > small + EFFECTS_MIN_REPLAY_FRAMES * std::mem::size_of::<EffectsInput>());
}

#[test]
fn callback_reading_ack_and_tail_overlay_have_no_heap_operations() {
    let mut old = graph(8_000, 2, EFFECTS_MIN_REPLAY_FRAMES, 17);
    for index in 1..=9 {
        old.frame(Some(input(index))).unwrap();
    }
    let mut retained = capture(
        reserve(&old, graph(16_000, 1, EFFECTS_MIN_REPLAY_FRAMES, 17)),
        old,
    );
    // The preparation owner, rather than the callback, performs actual old-bank retirement.
    drop(retained.take_retired_old().unwrap());
    no_heap(|| {
        while let Some(frame) = retained.next_media().unwrap() {
            retained.release_pending(&frame, false).unwrap();
            assert_eq!(retained.pending_output(), Some(frame));
            retained.release_pending(&frame, true).unwrap();
        }
        while let Some(frame) = retained.next_tail().unwrap() {
            assert!(frame.stamp.is_none());
            std::hint::black_box(frame.samples);
            retained.release_pending(&frame, true).unwrap();
        }
        assert!(retained.media_prefix_drained());
        assert!(retained.tail_drained());
    });
}

#[test]
fn wrong_actual_owner_and_noncontiguous_labels_reject_without_mutating_bank() {
    let old = graph(8_000, 2, EFFECTS_MIN_REPLAY_FRAMES, 0);
    let reservation = reserve(&old, graph(16_000, 1, EFFECTS_MIN_REPLAY_FRAMES, 0));
    let foreign = graph(8_000, 2, EFFECTS_MIN_REPLAY_FRAMES, 0);
    let failure = match reservation.capture(foreign, &AtomicBool::new(false)) {
        Ok(_) => panic!(),
        Err(failure) => failure,
    };
    assert_eq!(failure.error, HandoffError::WrongOwner);
    let mut old = old;
    old.frame(Some(input(1))).unwrap();
    let mut gap = input(3);
    gap.begin = Some(input(3).stamp);
    old.frame(Some(gap)).unwrap();
    let stats = old.statistics();
    let failure = match failure.reservation.capture(old, &AtomicBool::new(false)) {
        Ok(_) => panic!(),
        Err(failure) => failure,
    };
    assert_eq!(failure.error, HandoffError::InvalidLabel);
    assert_eq!(failure.old_graph.statistics(), stats);
}

#[test]
fn shared_actual_bounds_survive_failed_candidate_and_the_entire_retained_handoff() {
    let mut old = graph(8_000, 2, EFFECTS_MIN_REPLAY_FRAMES, 0);
    old.frame(Some(input(1))).unwrap();
    let bounds = HandoffBounds::try_arc_from_graph(&old).unwrap();
    let weak = Arc::downgrade(&bounds);
    assert!(bounds.arc_reserved_bytes() > 0);
    let next = graph(16_000, 1, EFFECTS_MIN_REPLAY_FRAMES, 0);
    let failed = match HandoffReservation::reserve_shared(
        bounds.clone(),
        PcmFormat {
            sample_rate: 16_000,
            channels: 2,
        },
        1,
        Some(next),
        &AtomicBool::new(true),
    ) {
        Ok(_) => panic!("Cancelled candidate was admitted"),
        Err(failed) => failed,
    };
    assert_eq!(failed.error, HandoffError::Cancelled);
    assert_eq!(Arc::strong_count(&bounds), 1);
    let reservation = match HandoffReservation::reserve_shared(
        bounds.clone(),
        PcmFormat {
            sample_rate: 16_000,
            channels: 2,
        },
        1,
        failed.new_graph,
        &AtomicBool::new(false),
    ) {
        Ok(reservation) => reservation,
        Err(_) => panic!("Actual shared bounds were not reusable"),
    };
    let mut retained = capture(reservation, old);
    drop(bounds);
    assert!(
        weak.upgrade().is_some(),
        "Retained FIFO dropped its actual bounds"
    );
    while let Some(frame) = retained.next_media().unwrap() {
        retained.release_pending(&frame, true).unwrap();
    }
    drop(retained);
    assert!(weak.upgrade().is_none());
}

#[test]
fn an_unreserved_arc_of_metadata_does_not_create_shared_allocation_admission() {
    let old = graph(8_000, 2, EFFECTS_MIN_REPLAY_FRAMES, 0);
    let unreserved = Arc::new(HandoffBounds::from_graph(&old));
    let next = graph(16_000, 1, EFFECTS_MIN_REPLAY_FRAMES, 0);
    let failed = match HandoffReservation::reserve_shared(
        unreserved,
        PcmFormat {
            sample_rate: 16_000,
            channels: 2,
        },
        1,
        Some(next),
        &AtomicBool::new(false),
    ) {
        Ok(_) => panic!("Unreserved shared bounds were admitted"),
        Err(failed) => failed,
    };
    assert_eq!(failed.error, HandoffError::Quota);
    assert!(failed.new_graph.is_some());
    assert_eq!(old.statistics().accepted_media_frames, 0);
}

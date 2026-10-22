use lx_gpui_player::audio::{
    compressor::{CompressorStamp, PreparedCompressor},
    convolution::{ConvolutionStamp, ImpulseBuilder, PreparedConvolution},
    effects::{
        EFFECTS_MIN_REPLAY_FRAMES, EffectsAlignment, EffectsError, EffectsFrame, EffectsFrameKind,
        EffectsInput, EffectsPitch, EffectsPreparation, PreparedEffects,
    },
    pcm::{PcmFormat, ProcessingStamp},
    spatial::{SpatialPlan, SpatialRenderer, SpatialSettings, SpatialStep},
};
use std::{
    alloc::{GlobalAlloc, Layout, System},
    cell::Cell,
    sync::atomic::AtomicBool,
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
fn input(index: u64, channels: usize) -> EffectsInput {
    let mut samples = [0.0; 8];
    for (lane, sample) in samples.iter_mut().enumerate().take(channels) {
        *sample = (lane + 1) as f64 / 16.0;
    }
    let stamp = ConvolutionStamp {
        token: 1,
        source_epoch: 2,
        media: ProcessingStamp {
            end_frame: index,
            fraction: 17,
            processing_sequence: index + 4,
            latency_frames: 23,
            ..Default::default()
        },
    };
    EffectsInput {
        samples,
        stamp,
        begin: Some(ConvolutionStamp {
            media: ProcessingStamp {
                end_frame: index.saturating_sub(1),
                fraction: 17,
                ..stamp.media
            },
            ..stamp
        }),
        pitch: EffectsPitch {
            ratio: 1.2,
            sequence: index + 9,
            transition_remaining_frames: index as u32,
        },
    }
}
fn preparation(channels: u16, device: u16, alignment: EffectsAlignment) -> EffectsPreparation {
    let spatial = SpatialPlan::prepare(SpatialSettings::default(), 8_000, 19).unwrap();
    EffectsPreparation {
        format: PcmFormat {
            sample_rate: 8_000,
            channels,
        },
        device_channels: device,
        main_gain: 1.0,
        send_gain: 0.0,
        alignment,
        sequence: 19,
        token: 1,
        source_epoch: 2,
        initial_spatial: spatial.target(SpatialStep::ORIGIN),
        replay_capacity: EFFECTS_MIN_REPLAY_FRAMES,
    }
}
fn bank(format: PcmFormat, frames: usize) -> PreparedConvolution {
    let mut builder = ImpulseBuilder::prepare(format.sample_rate, 1, frames).unwrap();
    for frame in 0..frames {
        builder
            .push_frame(&[if frame == 0 {
                1.0
            } else if frame == frames - 1 {
                0.5
            } else {
                0.0
            }])
            .unwrap();
    }
    let impulse = builder.finish().unwrap();
    PreparedConvolution::prepare(format, &impulse, false, &AtomicBool::new(false)).unwrap()
}
fn prepare(request: EffectsPreparation, impulse: usize) -> PreparedEffects {
    let convolution = (impulse != 0).then(|| bank(request.format, impulse));
    PreparedEffects::prepare(request, convolution, &AtomicBool::new(false)).unwrap()
}
fn acknowledge(effects: &mut PreparedEffects, frame: EffectsFrame, output: &mut Vec<EffectsFrame>) {
    if frame.kind != EffectsFrameKind::Padding {
        assert_eq!(effects.pending_output(), Some(frame));
        output.push(frame);
        effects.release_pending(&frame, true).unwrap();
    }
}
fn complete(effects: &mut PreparedEffects, count: u64, channels: usize) -> Vec<EffectsFrame> {
    let mut output = Vec::new();
    for index in 1..=count {
        let frame = effects.frame(Some(input(index, channels))).unwrap();
        acknowledge(effects, frame, &mut output);
    }
    effects.begin_eof().unwrap();
    let mut steps = 0;
    while let Some(frame) = effects.drain().unwrap() {
        acknowledge(effects, frame, &mut output);
        steps += 1;
        assert!(steps < 10_000);
    }
    assert!(effects.drained());
    assert_eq!(effects.pending_media_frames(), 0);
    assert_eq!(effects.remaining_tail_frames(), 0);
    output
}

#[test]
fn exact_original_labels_and_pitch_metadata_survive_every_delay() {
    let request = preparation(2, 2, EffectsAlignment::Partition512);
    let mut effects = prepare(request, 7);
    let mut steps = 0;
    let mut first_media = None;
    for index in 1..=600 {
        steps += 1;
        let frame = effects.frame(Some(input(index, 2))).unwrap();
        if frame.kind != EffectsFrameKind::Padding {
            let original = input(frame.stamp.unwrap().media.end_frame, 2);
            assert_eq!(frame.stamp, Some(original.stamp));
            assert_eq!(frame.begin, original.begin);
            assert_eq!(frame.pitch, original.pitch);
            assert_eq!(frame.effect_sequence, 19);
            first_media.get_or_insert(steps);
            effects.release_pending(&frame, true).unwrap();
        }
    }
    assert_eq!(first_media, Some(effects.latency_frames() as usize + 1));
    assert_eq!(effects.latency_frames(), 512 + 48);
    assert!(effects.owned_bytes() > 1536 * std::mem::size_of::<EffectsInput>());
}

#[test]
fn short_eof_wet_tail_is_valid_unstamped_and_compressor_drains_last() {
    let mut request = preparation(2, 2, EffectsAlignment::Partition512);
    request.main_gain = 0.0;
    request.send_gain = 1.0;
    let mut effects = prepare(request, 640);
    let output = complete(&mut effects, 8, 2);
    assert_eq!(output.len(), 8 + 639);
    for (index, frame) in output[..8].iter().enumerate() {
        assert_eq!(frame.kind, EffectsFrameKind::Media);
        assert_eq!(frame.stamp, Some(input(index as u64 + 1, 2).stamp));
    }
    assert!(
        output[8..]
            .iter()
            .all(|frame| frame.kind == EffectsFrameKind::Tail
                && frame.stamp.is_none()
                && frame.token == 1
                && frame.source_epoch == 2)
    );
    assert!(output.last().unwrap().samples[0].abs() > 0.0);
    assert_eq!(effects.statistics().presented_tail_frames, 639);
    assert_eq!(effects.compressor_statistics().accepted_frames, 647);
    assert_eq!(effects.compressor_statistics().valid_output_frames, 647);
    assert!(effects.drain().unwrap().is_none());
}

#[test]
fn gaps_and_temporal_pause_preserve_exact_final_frame_and_replay() {
    let mut effects = prepare(preparation(2, 2, EffectsAlignment::Direct), 0);
    for index in 1..=49 {
        let frame = effects.frame(Some(input(index, 2))).unwrap();
        if frame.kind == EffectsFrameKind::Media {
            assert_eq!(index, 49);
            let before = effects.statistics();
            let compressor = effects.compressor_statistics();
            let replay = effects.replay_inputs().collect::<Vec<_>>();
            effects.release_pending(&frame, false).unwrap();
            for _ in 0..50 {
                assert_eq!(effects.frame(None).unwrap().kind, EffectsFrameKind::Padding);
                assert_eq!(effects.frame(Some(input(50, 2))), Err(EffectsError::Busy));
                assert_eq!(effects.drain(), Err(EffectsError::Busy));
            }
            assert_eq!(effects.pending_output(), Some(frame));
            assert_eq!(effects.statistics(), before);
            assert_eq!(effects.compressor_statistics(), compressor);
            assert_eq!(effects.replay_inputs().collect::<Vec<_>>(), replay);
            effects.release_pending(&frame, true).unwrap();
            assert_eq!(effects.pending_media_frames(), 48);
            assert_eq!(
                effects.replay_inputs().next().unwrap().stamp,
                input(2, 2).stamp
            );
        }
    }
}

#[test]
fn rejected_inputs_leave_admission_buffers_and_all_dsp_counters_unchanged() {
    let mut effects = prepare(preparation(2, 2, EffectsAlignment::Partition512), 2);
    let before = effects.statistics();
    let compressor = effects.compressor_statistics();
    let mut invalid = input(1, 2);
    invalid.stamp.source_epoch = 3;
    assert_eq!(
        effects.frame(Some(invalid)),
        Err(EffectsError::InvalidStream)
    );
    invalid = input(1, 2);
    invalid.samples[0] = f64::NAN;
    assert_eq!(
        effects.frame(Some(invalid)),
        Err(EffectsError::NonFiniteInput)
    );
    invalid = input(1, 2);
    invalid.samples[0] = f64::MAX;
    assert_eq!(
        effects.frame(Some(invalid)),
        Err(EffectsError::InputOverflow)
    );
    invalid = input(1, 2);
    invalid.pitch.ratio = 0.0;
    assert_eq!(
        effects.frame(Some(invalid)),
        Err(EffectsError::InvalidStamp)
    );
    invalid = input(1, 2);
    invalid.begin.as_mut().unwrap().media.end_frame = 9;
    assert_eq!(
        effects.frame(Some(invalid)),
        Err(EffectsError::InvalidStamp)
    );
    assert_eq!(effects.pending_media_frames(), 0);
    assert!(effects.pending_output().is_none());
    assert_eq!(effects.statistics(), before);
    assert_eq!(effects.compressor_statistics(), compressor);
    assert_eq!(
        effects.frame(Some(input(1, 2))).unwrap().kind,
        EffectsFrameKind::Padding
    );
    assert_eq!(effects.pending_media_frames(), 1);
    assert_eq!(
        effects.frame(Some(input(0, 2))),
        Err(EffectsError::BackwardMedia)
    );
}

#[test]
fn partition_latched_dry_extras_are_aligned_and_never_compressed_or_panned() {
    for impulse in [0, 1] {
        let mut effects = prepare(preparation(8, 8, EffectsAlignment::Partition512), impulse);
        let output = complete(&mut effects, 1, 8);
        assert_eq!(output.len(), 1);
        assert_eq!(effects.latency_frames(), 560);
        let expected = input(1, 8);
        assert_eq!(output[0].samples[2..], expected.samples[2..]);
        assert_eq!(output[0].stamp, Some(expected.stamp));
    }
}

#[test]
fn stereo_effect_bus_remains_intact_until_final_mono_device_fold() {
    let spatial = SpatialPlan::prepare(
        SpatialSettings {
            enabled: true,
            radius: 0.5,
            speed: 25,
        },
        8_000,
        20,
    )
    .unwrap();
    let target = spatial.target(SpatialStep::degree(45).unwrap());
    let mut request = preparation(2, 1, EffectsAlignment::Direct);
    request.initial_spatial = target;
    let mut effects = prepare(request, 0);
    let mut compressor = PreparedCompressor::prepare(8_000, 2).unwrap();
    compressor.reset(1, 2).unwrap();
    let mut renderer = SpatialRenderer::new(8_000, 2, 1).unwrap();
    renderer.retarget(target);
    renderer.reset_to_target();
    let mut actual_valid = 0;
    for index in 1..=200 {
        let mut input = input(index, 2);
        input.samples[0] = 0.2;
        input.samples[1] = 0.6;
        let expected = compressor
            .frame(
                &[0.2, 0.6],
                Some(CompressorStamp {
                    token: 1,
                    source_epoch: 2,
                    media: input.stamp.media,
                }),
            )
            .unwrap();
        let actual = effects.frame(Some(input)).unwrap();
        if expected.valid {
            let expected = renderer.frame(expected.samples.map(f64::from)).0;
            assert_eq!(actual.samples, expected);
            assert_eq!(actual.spatial_sequence, 20);
            assert_eq!(actual.samples[1..], [0.0; 7]);
            effects.release_pending(&actual, true).unwrap();
            actual_valid += 1;
        } else {
            assert_eq!(actual.kind, EffectsFrameKind::Padding);
        }
    }
    assert_eq!(actual_valid, 152);
}

#[test]
fn hard_epoch_and_foreign_graph_receipts_cannot_release_new_pending_audio() {
    let request = preparation(2, 2, EffectsAlignment::Direct);
    let mut first = prepare(request, 0);
    let mut second = prepare(request, 0);
    for index in 1..=49 {
        first.frame(Some(input(index, 2))).unwrap();
        second.frame(Some(input(index, 2))).unwrap();
    }
    let old = first.pending_output().unwrap();
    let foreign = second.pending_output().unwrap();
    assert_eq!(
        first.release_pending(&foreign, true),
        Err(EffectsError::InvalidReceipt)
    );
    assert_eq!(first.pending_output(), Some(old));
    first.invalidate();
    assert!(first.pending_output().is_none());
    assert_eq!(first.pending_media_frames(), 0);
    assert_eq!(
        first.frame(Some(input(50, 2))),
        Err(EffectsError::InvalidStream)
    );
    first.reset(1, 3).unwrap();
    for index in 1..=49 {
        let mut next = input(index, 2);
        next.stamp.source_epoch = 3;
        next.begin.as_mut().unwrap().source_epoch = 3;
        first.frame(Some(next)).unwrap();
    }
    let new = first.pending_output().unwrap();
    assert_eq!(
        first.release_pending(&old, true),
        Err(EffectsError::InvalidReceipt)
    );
    assert_eq!(first.pending_output(), Some(new));
    first.release_pending(&new, true).unwrap();
}

#[test]
fn prepared_rate_bound_and_invalid_configuration_reject_before_ownership_install() {
    assert_eq!(
        PreparedEffects::rate_replay_capacity(1536, 8_000, 384_000).unwrap(),
        73_729
    );
    assert_eq!(
        PreparedEffects::rate_replay_capacity(1536, 384_000, 8_000).unwrap(),
        1536
    );
    assert!(PreparedEffects::rate_replay_capacity(usize::MAX, 8_000, 384_000).is_err());
    let mut request = preparation(2, 2, EffectsAlignment::Direct);
    request.replay_capacity = 1535;
    assert!(PreparedEffects::prepare(request, None, &AtomicBool::new(false)).is_err());
    request = preparation(2, 2, EffectsAlignment::Direct);
    request.format.channels = 1;
    assert!(PreparedEffects::prepare(request, None, &AtomicBool::new(false)).is_err());
    request = preparation(2, 2, EffectsAlignment::Direct);
    assert!(
        PreparedEffects::prepare(
            request,
            Some(bank(request.format, 1)),
            &AtomicBool::new(false)
        )
        .is_err()
    );
    assert!(PreparedEffects::prepare(request, None, &AtomicBool::new(true)).is_err());
    let mut effects = prepare(preparation(2, 2, EffectsAlignment::Direct), 0);
    effects.begin_eof().unwrap();
    assert!(effects.drain().unwrap().is_none());
    assert!(effects.drained());
}

#[test]
fn rendered_tail_must_keep_one_final_frame_until_real_presentation_ack() {
    let mut effects = prepare(preparation(2, 2, EffectsAlignment::Partition512), 2);
    assert_eq!(
        effects.frame(Some(input(1, 2))).unwrap().kind,
        EffectsFrameKind::Padding
    );
    effects.begin_eof().unwrap();
    let mut held = None;
    for _ in 0..2_000 {
        let frame = effects.drain().unwrap().unwrap();
        match frame.kind {
            EffectsFrameKind::Padding => {}
            EffectsFrameKind::Media => {
                effects.release_pending(&frame, true).unwrap();
            }
            EffectsFrameKind::Tail => {
                held = Some(frame);
                break;
            }
        }
    }
    let tail = held.unwrap();
    assert!(!effects.drained());
    assert_eq!(effects.remaining_tail_frames(), 1);
    effects.release_pending(&tail, false).unwrap();
    assert_eq!(effects.pending_output(), Some(tail));
    assert_eq!(effects.drain(), Err(EffectsError::Busy));
    effects.release_pending(&tail, true).unwrap();
    while effects.drain().unwrap().is_some() {}
    assert!(effects.drained());
}

#[test]
fn accepted_pipeline_gap_ack_reset_and_eof_perform_no_heap_operations() {
    let mut effects = prepare(preparation(8, 1, EffectsAlignment::Partition512), 31);
    no_heap(|| {
        for index in 1..=2_000 {
            assert_eq!(effects.frame(None).unwrap().kind, EffectsFrameKind::Padding);
            let frame = effects.frame(Some(input(index, 8))).unwrap();
            if frame.kind != EffectsFrameKind::Padding {
                effects.release_pending(&frame, false).unwrap();
                assert_eq!(effects.pending_output(), Some(frame));
                effects.release_pending(&frame, true).unwrap();
            }
        }
        effects.begin_eof().unwrap();
        let mut steps = 0;
        while let Some(frame) = effects.drain().unwrap() {
            if frame.kind != EffectsFrameKind::Padding {
                effects.release_pending(&frame, true).unwrap();
            }
            steps += 1;
            assert!(steps < 2_000);
        }
        assert!(effects.drained());
        effects.invalidate();
        effects.reset(1, 9).unwrap();
    });
}

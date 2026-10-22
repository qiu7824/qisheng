use lx_gpui_player::audio::{
    compressor::{
        CompressorChannelPolicy, CompressorError, CompressorFrame, CompressorStamp,
        PreparedCompressor,
    },
    pcm::{MAX_CLOCK_FRAMES, ProcessingStamp},
};
use serde::Deserialize;
use std::{
    alloc::{GlobalAlloc, Layout, System},
    cell::Cell,
    io::Read,
};

thread_local! {
    static WATCH: Cell<bool> = const { Cell::new(false) };
    static HEAP: Cell<(usize,usize)> = const { Cell::new((0,0)) };
}
struct Allocator;
unsafe impl GlobalAlloc for Allocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let _ = WATCH.try_with(|watch| {
            if watch.get() {
                HEAP.with(|count| {
                    let (a, d) = count.get();
                    count.set((a + 1, d));
                });
            }
        });
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        let _ = WATCH.try_with(|watch| {
            if watch.get() {
                HEAP.with(|count| {
                    let (a, d) = count.get();
                    count.set((a, d + 1));
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
    HEAP.with(|count| count.set((0, 0)));
    WATCH.with(|watch| watch.set(true));
    let guard = Guard;
    operation();
    drop(guard);
    assert_eq!(
        HEAP.with(Cell::get),
        (0, 0),
        "Callback kernel allocated or destroyed heap storage"
    );
}

#[derive(Deserialize)]
struct Golden {
    model: String,
    math: String,
    cases: Vec<Case>,
    static_curve: Vec<[f32; 3]>,
}
#[derive(Deserialize)]
struct Case {
    rate: u32,
    channels: u16,
    input_frames: usize,
    latency_frames: u32,
    input: Vec<[f32; 2]>,
    output: Vec<[f32; 2]>,
}
fn golden() -> Golden {
    // Recorded from the unchanged original C++ algorithm, with the separately
    // declared host-CRT compatibility backend rather than a Rust test mirror.
    let mut decoder = flate2::read::GzDecoder::new(
        include_bytes!("support/compressor_chromium148.json.gz").as_slice(),
    );
    let mut bytes = Vec::new();
    decoder
        .by_ref()
        .take(4 * 1024 * 1024 + 1)
        .read_to_end(&mut bytes)
        .unwrap();
    assert!(bytes.len() <= 4 * 1024 * 1024);
    serde_json::from_slice(&bytes).unwrap()
}
fn stamp(end: u64) -> CompressorStamp {
    CompressorStamp {
        token: 7,
        source_epoch: 3,
        media: ProcessingStamp {
            end_frame: end,
            fraction: (end as u32).wrapping_mul(1997),
            processing_sequence: 19,
            latency_frames: 73,
            ..ProcessingStamp::default()
        },
    }
}
fn close(a: f32, b: f32, message: &str) {
    assert!(
        (a - b).abs() <= 3e-5 + 3e-5 * b.abs(),
        "{message}: native {a}, original C++ {b}"
    );
}

#[test]
fn original_cpp_mono_and_stereo_goldens_cover_knee_attack_release_and_all_delay_caps() {
    let fixture = golden();
    assert_eq!(fixture.model, "Chromium 148.0.7778.280");
    assert!(fixture.math.contains("not fdlibm bit identity"));
    assert_eq!(fixture.cases.len(), 8);
    for case in fixture.cases {
        let mut kernel = PreparedCompressor::prepare(case.rate, case.channels).unwrap();
        kernel.reset(7, 3).unwrap();
        assert_eq!(kernel.latency_frames(), case.latency_frames);
        assert_eq!(case.input.len(), case.input_frames);
        for (index, input) in case.input.iter().enumerate() {
            let frame = kernel
                .frame(
                    &input[..usize::from(case.channels)],
                    Some(stamp(index as u64 + 1)),
                )
                .unwrap();
            for channel in 0..usize::from(case.channels) {
                close(
                    frame.samples[channel],
                    case.output[index][channel],
                    "primary transient golden",
                );
            }
            assert_eq!(frame.valid, index >= case.latency_frames as usize);
            let expected = if frame.valid {
                Some(stamp((index - case.latency_frames as usize) as u64 + 1))
            } else {
                None
            };
            assert_eq!(frame.stamp, expected);
            assert!(kernel.reduction_db().is_finite());
        }
        kernel.begin_eof();
        let mut index = case.input_frames;
        while let Some(frame) = kernel.drain().unwrap() {
            for channel in 0..usize::from(case.channels) {
                close(
                    frame.samples[channel],
                    case.output[index][channel],
                    "primary EOF delay golden",
                );
            }
            assert!(frame.valid);
            assert_eq!(
                frame.stamp,
                Some(stamp((index - case.latency_frames as usize) as u64 + 1))
            );
            index += 1;
        }
        assert_eq!(index, case.input_frames + case.latency_frames as usize);
        assert!(kernel.drained());
        assert_eq!(
            kernel.statistics().accepted_frames,
            case.input_frames as u64
        );
        assert_eq!(
            kernel.statistics().valid_output_frames,
            case.input_frames as u64
        );
    }
}

#[test]
fn original_cpp_static_curve_sweep_checks_threshold_to_upper_knee_and_ratio() {
    let fixture = golden();
    let kernel = PreparedCompressor::prepare(48000, 2).unwrap();
    assert_eq!(fixture.static_curve.len(), 99);
    for [db, input, expected] in fixture.static_curve {
        let actual = kernel.transfer_level(input).unwrap();
        close(actual, expected, "primary static curve");
        if db < -24.0 {
            assert_eq!(actual, input);
        }
    }
    assert_eq!(kernel.transfer_level(0.0).unwrap(), 0.0);
    assert_eq!(
        kernel.transfer_level(-1.0),
        Err(CompressorError::InvalidLevel)
    );
    assert_eq!(
        kernel.transfer_level(f32::NAN),
        Err(CompressorError::InvalidLevel)
    );
    assert_eq!(kernel.statistics().processed_frames, 0);
}

#[test]
fn exact_stamps_survive_delay_and_short_eof_never_extends_media_clock() {
    for (rate, delay) in [
        (8000, 48),
        (22050, 132),
        (44100, 264),
        (48000, 288),
        (96000, 576),
        (192000, 1023),
        (384000, 1023),
    ] {
        let mut kernel = PreparedCompressor::prepare(rate, 2).unwrap();
        kernel.reset(7, 3).unwrap();
        assert_eq!(kernel.latency_frames(), delay);
        for index in 1..=5 {
            let frame = kernel.frame(&[0.01, -0.02], Some(stamp(index))).unwrap();
            assert!(!frame.valid && frame.stamp.is_none());
        }
        kernel.begin_eof();
        kernel.begin_eof();
        let mut output = Vec::new();
        let mut padding = 0;
        while let Some(frame) = kernel.drain().unwrap() {
            if frame.valid {
                output.push(frame.stamp.unwrap());
            } else {
                padding += 1;
                assert!(frame.stamp.is_none());
            }
        }
        assert_eq!(output, (1..=5).map(stamp).collect::<Vec<_>>());
        assert_eq!(padding, delay as usize - 5);
        assert_eq!(kernel.statistics().processed_frames, 5 + u64::from(delay));
        assert_eq!(
            kernel.frame(&[0.0, 0.0], Some(stamp(6))),
            Err(CompressorError::EndOfInput)
        );
        assert_eq!(kernel.drain().unwrap(), None);
    }
}

#[test]
fn empty_eof_has_no_fabricated_tail_and_pause_is_a_true_no_processing_interval() {
    let mut kernel = PreparedCompressor::prepare(44100, 1).unwrap();
    assert_eq!(kernel.drain(), Err(CompressorError::EndNotDeclared));
    kernel.begin_eof();
    assert!(kernel.drained());
    assert!(kernel.drain().unwrap().is_none());
    assert_eq!(kernel.statistics().processed_frames, 0);
    kernel.reset(7, 3).unwrap();
    for _ in 0..500 {
        kernel.frame(&[0.5], None).unwrap();
    }
    let before = (kernel.statistics(), kernel.reduction_db());
    // A paused owner reads telemetry, without feeding fictitious zero input.
    for _ in 0..1000 {
        assert_eq!((kernel.statistics(), kernel.reduction_db()), before);
    }
    let mut same = PreparedCompressor::prepare(44100, 1).unwrap();
    same.reset(7, 3).unwrap();
    for _ in 0..500 {
        same.frame(&[0.5], None).unwrap();
    }
    for _ in 0..300 {
        assert_eq!(
            kernel.frame(&[0.25], None).unwrap(),
            same.frame(&[0.25], None).unwrap()
        );
    }
}

#[test]
fn invalid_stream_backward_and_nonfinite_metadata_leave_every_state_unchanged() {
    let mut kernel = PreparedCompressor::prepare(48000, 2).unwrap();
    let mut control = PreparedCompressor::prepare(48000, 2).unwrap();
    kernel.reset(7, 3).unwrap();
    control.reset(7, 3).unwrap();
    for index in 1..=600 {
        let sample = [(index % 17) as f32 / 17.0, -0.3];
        assert_eq!(
            kernel.frame(&sample, Some(stamp(index))).unwrap(),
            control.frame(&sample, Some(stamp(index))).unwrap()
        );
    }
    let stats = kernel.statistics();
    let reduction = kernel.reduction_db();
    let mut wrong = stamp(601);
    wrong.token = 8;
    assert_eq!(
        kernel.frame(&[f32::NAN, 1.0], Some(wrong)),
        Err(CompressorError::WrongStream)
    );
    wrong = stamp(601);
    wrong.source_epoch = 4;
    assert_eq!(
        kernel.frame(&[1.0, 1.0], Some(wrong)),
        Err(CompressorError::WrongStream)
    );
    assert_eq!(
        kernel.frame(&[1.0, 1.0], Some(stamp(599))),
        Err(CompressorError::BackwardMediaPosition)
    );
    wrong = stamp(601);
    wrong.media.settings.playback_rate = f64::NAN;
    assert_eq!(
        kernel.frame(&[1.0, 1.0], Some(wrong)),
        Err(CompressorError::InvalidStamp)
    );
    wrong = stamp(601);
    wrong.media.end_frame = MAX_CLOCK_FRAMES;
    wrong.media.fraction = 1;
    assert_eq!(
        kernel.frame(&[1.0, 1.0], Some(wrong)),
        Err(CompressorError::InvalidStamp)
    );
    assert_eq!(
        kernel.frame(&[1.0], Some(stamp(601))),
        Err(CompressorError::WrongChannelCount)
    );
    assert_eq!(kernel.reset(0, 3), Err(CompressorError::InvalidStamp));
    assert_eq!(kernel.statistics(), stats);
    assert_eq!(kernel.reduction_db(), reduction);
    for index in 601..=1300 {
        assert_eq!(
            kernel.frame(&[0.5, -0.75], Some(stamp(index))).unwrap(),
            control.frame(&[0.5, -0.75], Some(stamp(index))).unwrap()
        );
    }
}

#[test]
fn reset_clears_old_stream_samples_and_all_delayed_stamps_without_replacing_storage() {
    let mut kernel = PreparedCompressor::prepare(44100, 2).unwrap();
    kernel.reset(7, 3).unwrap();
    for index in 1..=700 {
        kernel.frame(&[0.9, -0.8], Some(stamp(index))).unwrap();
    }
    let reserved = kernel.reserved_bytes();
    kernel.reset(9, 4).unwrap();
    assert_eq!(kernel.reserved_bytes(), reserved);
    for index in 1..=kernel.latency_frames() + 10 {
        let new_stamp = CompressorStamp {
            token: 9,
            source_epoch: 4,
            ..stamp(u64::from(index))
        };
        let output = kernel.frame(&[0.0, 0.0], Some(new_stamp)).unwrap();
        assert!(output.samples.iter().all(|sample| *sample == 0.0));
        assert!(
            output
                .stamp
                .is_none_or(|stamp| stamp.token == 9 && stamp.source_epoch == 4)
        );
    }
}

#[test]
fn nonfinite_input_is_counted_before_detection_and_matches_explicit_zero_policy() {
    let mut kernel = PreparedCompressor::prepare(48000, 2).unwrap();
    let mut zero = PreparedCompressor::prepare(48000, 2).unwrap();
    for _ in 0..2000 {
        assert_eq!(
            kernel.frame(&[f32::NAN, f32::INFINITY], None).unwrap(),
            zero.frame(&[0.0, 0.0], None).unwrap()
        );
    }
    assert_eq!(kernel.statistics().sanitized_input_samples, 4000);
    assert!(kernel.reduction_db().is_finite());
    assert_eq!(zero.statistics().sanitized_input_samples, 0);
    for _ in 0..2000 {
        let output = kernel
            .frame(&[f32::MIN_POSITIVE / 2.0, -f32::MIN_POSITIVE / 2.0], None)
            .unwrap();
        assert!(output.samples.iter().all(|sample| sample.is_finite()));
        assert!(kernel.reduction_db().is_finite());
    }
}

#[test]
fn stereo_link_preserves_channel_ratio_and_native_extras_keep_exact_delayed_gain() {
    let mut kernel = PreparedCompressor::prepare(48000, 8).unwrap();
    assert_eq!(
        kernel.channel_policy(),
        CompressorChannelPolicy::NativeStereoWithDelayedDryExtras
    );
    kernel.reset(7, 3).unwrap();
    for index in 1..=3000 {
        let frame = kernel
            .frame(
                &[1.0, -0.25, 0.125, -0.5, 0.75, -0.875, 0.03125, -0.0625],
                Some(stamp(index)),
            )
            .unwrap();
        if frame.valid {
            assert!((frame.samples[1] + 0.25 * frame.samples[0]).abs() < 1e-6);
            assert_eq!(
                &frame.samples[2..],
                &[0.125, -0.5, 0.75, -0.875, 0.03125, -0.0625]
            );
            assert_eq!(
                frame.stamp,
                Some(stamp(index - u64::from(kernel.latency_frames())))
            );
        }
    }
    assert_eq!(
        PreparedCompressor::prepare(48000, 1)
            .unwrap()
            .channel_policy(),
        CompressorChannelPolicy::MonoDuplicated
    );
    assert_eq!(
        PreparedCompressor::prepare(48000, 2)
            .unwrap()
            .channel_policy(),
        CompressorChannelPolicy::StereoLinked
    );
}

#[test]
fn callback_frame_reset_rejection_and_eof_have_no_heap_growth_or_destruction() {
    let mut kernel = PreparedCompressor::prepare(48000, 8).unwrap();
    kernel.reset(7, 3).unwrap();
    no_heap(|| {
        for index in 1..=4000 {
            let output = kernel.frame(&[0.3; 8], Some(stamp(index))).unwrap();
            assert!(output.samples.iter().all(|sample| sample.is_finite()));
        }
        let invalid = CompressorStamp {
            token: 0,
            ..stamp(4001)
        };
        assert_eq!(
            kernel.frame(&[f32::NAN; 8], Some(invalid)),
            Err(CompressorError::InvalidStamp)
        );
        kernel.begin_eof();
        while let Some(frame) = kernel.drain().unwrap() {
            assert!(frame.valid);
        }
        kernel.reset(12, 9).unwrap();
        for _ in 0..2000 {
            kernel.frame(&[0.0; 8], None).unwrap();
        }
    });
    assert!(kernel.reserved_bytes() >= 1024 * std::mem::size_of::<CompressorFrame>());
}

#[test]
fn invalid_preparation_formats_are_rejected_without_promising_unsupported_layouts() {
    for (rate, channels) in [(0, 1), (7999, 1), (384001, 2), (44100, 0), (44100, 9)] {
        assert!(PreparedCompressor::prepare(rate, channels).is_err());
    }
}

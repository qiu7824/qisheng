use lx_gpui_player::audio::{
    analyser::{
        ANALYSER_BINS, ANALYSER_FFT_SIZE, ANALYSER_SILENT_DB, AnalysisChannelMix, AnalysisError,
        AnalysisFrame, AnalysisStamp, LxAnalyser,
    },
    pcm::{MAX_CLOCK_FRAMES, PcmFormat},
};
use std::{
    alloc::{GlobalAlloc, Layout, System},
    cell::Cell,
    f64::consts::TAU,
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
                HEAP.with(|counts| {
                    let (a, d) = counts.get();
                    counts.set((a + 1, d));
                });
            }
        });
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        let _ = WATCH.try_with(|watch| {
            if watch.get() {
                HEAP.with(|counts| {
                    let (a, d) = counts.get();
                    counts.set((a, d + 1));
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
    HEAP.with(|counts| counts.set((0, 0)));
    WATCH.with(|watch| watch.set(true));
    let guard = Guard;
    operation();
    drop(guard);
    assert_eq!(
        HEAP.with(Cell::get),
        (0, 0),
        "Analyser callback work allocated or destroyed heap storage"
    );
}
fn stamp(end: u64) -> AnalysisStamp {
    AnalysisStamp {
        token: 7,
        source_epoch: 11,
        media_interval_end: end,
        media_fraction: 0,
    }
}
fn prepare(channels: u16) -> LxAnalyser {
    let mut analyser = LxAnalyser::prepare(PcmFormat {
        sample_rate: 48000,
        channels,
    })
    .unwrap();
    analyser.reset(stamp(0)).unwrap();
    analyser
}
fn tone(bin: f64, amplitude: f64) -> [f32; ANALYSER_FFT_SIZE] {
    std::array::from_fn(|index| {
        (amplitude * (TAU * bin * index as f64 / ANALYSER_FFT_SIZE as f64).sin()) as f32
    })
}
fn push_mono(analyser: &mut LxAnalyser, input: &[f32], start: u64) {
    for (index, sample) in input.iter().enumerate() {
        analyser
            .push_frame(&[*sample], stamp(start + index as u64 + 1))
            .unwrap();
    }
}
fn magnitude(decibels: f64) -> f64 {
    if decibels == ANALYSER_SILENT_DB {
        0.0
    } else {
        10.0_f64.powf(decibels / 20.0)
    }
}
fn oracle(
    input: &[f32; ANALYSER_FFT_SIZE],
    previous: &[f64; ANALYSER_BINS],
) -> ([f64; ANALYSER_BINS], [u8; ANALYSER_BINS]) {
    let mut magnitudes = [0.0; ANALYSER_BINS];
    let mut bins = [0; ANALYSER_BINS];
    for frequency in 0..ANALYSER_BINS {
        let mut real = 0.0;
        let mut imaginary = 0.0;
        for (index, sample) in input.iter().enumerate() {
            let window_phase = TAU * index as f64 / 256.0;
            let window = (1.0 - 0.16) / 2.0 - 0.5 * window_phase.cos()
                + 0.16 / 2.0 * (2.0 * window_phase).cos();
            let phase = -TAU * frequency as f64 * index as f64 / 256.0;
            real += f64::from(*sample) * window * phase.cos() / 256.0;
            imaginary += f64::from(*sample) * window * phase.sin() / 256.0;
        }
        let current = (real * real + imaginary * imaginary).sqrt();
        magnitudes[frequency] = 0.8 * previous[frequency] + 0.2 * current;
        let db = if magnitudes[frequency] > 0.0 {
            20.0 * magnitudes[frequency].log10()
        } else {
            f64::NEG_INFINITY
        };
        bins[frequency] = ((db + 100.0) * 255.0 / 70.0).floor().clamp(0.0, 255.0) as u8;
    }
    (magnitudes, bins)
}
fn matches_oracle(
    frame: &AnalysisFrame,
    expected: &[f64; ANALYSER_BINS],
    bins: &[u8; ANALYSER_BINS],
) {
    for (index, value) in expected.iter().enumerate() {
        assert!(
            (magnitude(frame.decibels[index]) - value).abs() < 2e-12,
            "independent DFT bin{index}"
        );
    }
    assert_eq!(&frame.bins, bins);
    assert!(frame.decibels.iter().all(|value| value.is_finite()));
}

#[test]
fn silence_returns_fixed_zero_bytes_and_finite_native_floor() {
    let mut analyser = prepare(1);
    assert_eq!(analyser.snapshot().valid_window_frames, 0);
    assert_eq!(analyser.analyse(0).unwrap().sequence, 0);
    push_mono(&mut analyser, &[0.0; 256], 0);
    let frame = *analyser.analyse(1).unwrap();
    assert_eq!(frame.bins, [0; 128]);
    assert_eq!(frame.decibels, [ANALYSER_SILENT_DB; 128]);
    assert_eq!(frame.valid_window_frames, 256);
    assert_eq!(frame.accepted_frames, 256);
    assert_eq!(frame.stamp, stamp(256));
    assert_eq!(frame.fft_size, 256);
    assert_eq!(frame.sample_rate, 48000);
}

#[test]
fn exact_bin_tone_uses_blackman_gain_fft_normalization_and_byte_floor() {
    let input = tone(7.0, 0.01);
    let mut analyser = prepare(1);
    push_mono(&mut analyser, &input, 0);
    let frame = *analyser.analyse(1).unwrap();
    let (expected, bins) = oracle(&input, &[0.0; 128]);
    matches_oracle(&frame, &expected, &bins);
    // The first smoothed peak is .2 * (.42/2) * amplitude; window coherent
    // gain is not divided out. The specified byte mapping floors to118.
    assert!((magnitude(frame.decibels[7]) - 0.00042).abs() < 2e-10);
    assert_eq!(frame.bins[7], 118);
    assert!(frame.bins[7] > frame.bins[6] && frame.bins[6] > frame.bins[5]);
}

#[test]
fn off_bin_tone_and_dc_match_an_independent_scalar_dft() {
    let input = tone(18.375, 0.002);
    let mut analyser = prepare(1);
    push_mono(&mut analyser, &input, 0);
    let (expected, bins) = oracle(&input, &[0.0; 128]);
    matches_oracle(analyser.analyse(1).unwrap(), &expected, &bins);
    analyser.reset(stamp(0)).unwrap();
    let input = [0.001_f32; 256];
    push_mono(&mut analyser, &input, 0);
    let frame = *analyser.analyse(1).unwrap();
    let (expected, bins) = oracle(&input, &[0.0; 128]);
    matches_oracle(&frame, &expected, &bins);
    assert!((magnitude(frame.decibels[0]) - 0.084 * f64::from(input[0])).abs() < 1e-13);
}

#[test]
fn smoothing_occurs_once_per_new_quantum_with_new_pcm_and_decays_on_real_silence() {
    let input = tone(9.0, 0.01);
    let mut analyser = prepare(1);
    push_mono(&mut analyser, &input, 0);
    let first = *analyser.analyse(5).unwrap();
    let (magnitudes, _) = oracle(&input, &[0.0; 128]);
    for _ in 0..30 {
        assert_eq!(*analyser.analyse(5).unwrap(), first);
        assert_eq!(analyser.snapshot(), first);
    }
    assert_eq!(*analyser.analyse(6).unwrap(), first); // No new accepted PCM.
    push_mono(&mut analyser, &input, 256);
    assert_eq!(*analyser.analyse(6).unwrap(), first);
    let second = *analyser.analyse(7).unwrap();
    let (next, bins) = oracle(&input, &magnitudes);
    matches_oracle(&second, &next, &bins);
    assert_eq!(second.sequence, first.sequence + 1);
    push_mono(&mut analyser, &[0.0; 256], 512);
    let silence = *analyser.analyse(8).unwrap();
    assert!((magnitude(silence.decibels[9]) - 0.8 * magnitude(second.decibels[9])).abs() < 1e-13);
    assert_eq!(silence.stamp, stamp(768));
    assert!(silence.decibels[9] < second.decibels[9]);
}

#[test]
fn stereo_anti_phase_cancels_and_speakers_downmix_preserves_standard_coefficients() {
    let input = tone(8.0, 0.02);
    let mut stereo = prepare(2);
    for (index, value) in input.iter().enumerate() {
        stereo
            .push_frame(&[*value, -*value], stamp(index as u64 + 1))
            .unwrap();
    }
    assert_eq!(stereo.analyse(1).unwrap().bins, [0; 128]);
    let mut stereo = prepare(2);
    let mut mono = prepare(1);
    for (index, value) in input.iter().enumerate() {
        let tag = stamp(index as u64 + 1);
        stereo.push_frame(&[*value, 0.0], tag).unwrap();
        mono.push_frame(&[*value * 0.5], tag).unwrap();
    }
    assert_eq!(
        stereo.analyse(1).unwrap().decibels,
        mono.analyse(1).unwrap().decibels
    );
    let mut quad = prepare(4);
    let mut mono = prepare(1);
    for index in 0..256 {
        let tag = stamp(index + 1);
        quad.push_frame(&[0.002, 0.004, 0.006, 0.008], tag).unwrap();
        mono.push_frame(&[0.005], tag).unwrap();
    }
    let quad = *quad.analyse(1).unwrap();
    let mono = *mono.analyse(1).unwrap();
    assert!((magnitude(quad.decibels[0]) - magnitude(mono.decibels[0])).abs() < 1e-10);
    let mut surround = prepare(6);
    for index in 0..256 {
        surround
            .push_frame(&[0.0, 0.0, 0.0, 1.0, 0.0, 0.0], stamp(index + 1))
            .unwrap();
    }
    assert_eq!(surround.analyse(1).unwrap().bins, [0; 128]);
    surround.reset(stamp(0)).unwrap();
    for index in 0..256 {
        surround
            .push_frame(&[0.001, 0.002, 0.003, 0.9, 0.004, 0.005], stamp(index + 1))
            .unwrap();
    }
    let frame = surround.analyse(1).unwrap();
    let expected = 0.084
        * (std::f64::consts::FRAC_1_SQRT_2 * (f64::from(0.001_f32) + f64::from(0.002_f32))
            + f64::from(0.003_f32)
            + 0.5 * (f64::from(0.004_f32) + f64::from(0.005_f32)));
    assert!((magnitude(frame.decibels[0]) - expected).abs() < 1e-13);
}

#[test]
fn nonstandard_native_layouts_are_labeled_and_nonfinite_input_is_counted() {
    for channels in [3, 5, 7, 8] {
        let mut native = prepare(channels);
        for index in 0..256 {
            let mut values = [0.0; 8];
            values[0] = 0.008;
            native
                .push_frame(&values[..channels as usize], stamp(index + 1))
                .unwrap();
        }
        let frame = native.analyse(1).unwrap();
        assert_eq!(frame.channel_mix, AnalysisChannelMix::NativeAverage);
        assert!(
            (magnitude(frame.decibels[0]) - 0.084 * f64::from(0.008_f32) / f64::from(channels))
                .abs()
                < 1e-13
        );
    }
    let mut analyser = prepare(2);
    for index in 0..256 {
        analyser
            .push_frame(&[f32::NAN, f32::INFINITY], stamp(index + 1))
            .unwrap();
    }
    let frame = *analyser.analyse(1).unwrap();
    assert_eq!(frame.sanitized_input_samples, 512);
    assert_eq!(frame.bins, [0; 128]);
    analyser.reset(stamp(0)).unwrap();
    for index in 0..256 {
        analyser
            .push_frame(&[f32::MAX, -f32::MAX], stamp(index + 1))
            .unwrap();
    }
    assert_eq!(analyser.analyse(1).unwrap().bins, [0; 128]);
}

#[test]
fn reset_clears_old_epoch_window_and_bad_stamp_or_quantum_preserves_state() {
    let mut analyser = prepare(1);
    push_mono(&mut analyser, &tone(4.0, 0.5), 0);
    let previous = *analyser.analyse(4).unwrap();
    assert!(previous.bins.iter().any(|byte| *byte > 0));
    assert_eq!(
        analyser.push_frame(
            &[0.2],
            AnalysisStamp {
                source_epoch: 12,
                ..stamp(257)
            }
        ),
        Err(AnalysisError::WrongStream)
    );
    assert_eq!(
        analyser.push_frame(&[0.2], stamp(200)),
        Err(AnalysisError::BackwardMediaPosition)
    );
    assert_eq!(
        analyser.push_frame(&[0.1, 0.2], stamp(257)),
        Err(AnalysisError::WrongChannelCount)
    );
    assert_eq!(
        analyser.analyse(3).unwrap_err(),
        AnalysisError::StaleQuantum
    );
    assert_eq!(analyser.snapshot(), previous);
    assert_eq!(
        analyser
            .reset(AnalysisStamp {
                token: 0,
                ..stamp(0)
            })
            .unwrap_err(),
        AnalysisError::InvalidStamp
    );
    assert_eq!(analyser.snapshot(), previous);
    let next = AnalysisStamp {
        token: 8,
        source_epoch: 12,
        media_interval_end: 10000,
        media_fraction: 0,
    };
    let reset = analyser.reset(next).unwrap();
    assert_eq!(reset.sequence, 0);
    assert_eq!(reset.bins, [0; 128]);
    assert_eq!(reset.valid_window_frames, 0);
    assert_eq!(reset.stamp, next);
    assert_eq!(
        analyser.push_frame(
            &[0.1],
            AnalysisStamp {
                media_interval_end: MAX_CLOCK_FRAMES,
                media_fraction: 1,
                ..next
            }
        ),
        Err(AnalysisError::InvalidStamp)
    );
    analyser
        .push_frame(
            &[0.0],
            AnalysisStamp {
                media_interval_end: 10001,
                ..next
            },
        )
        .unwrap();
    assert_eq!(analyser.analyse(0).unwrap().stamp.media_interval_end, 10001);
}

#[test]
fn partial_window_is_zero_padded_and_circular_history_uses_only_latest_samples() {
    let input = tone(12.25, 0.01);
    let mut analyser = prepare(1);
    push_mono(&mut analyser, &input[..73], 0);
    let mut padded = [0.0; 256];
    padded[183..].copy_from_slice(&input[..73]);
    let (expected, bins) = oracle(&padded, &[0.0; 128]);
    matches_oracle(analyser.analyse(1).unwrap(), &expected, &bins);
    analyser.reset(stamp(0)).unwrap();
    push_mono(&mut analyser, &[0.5; 256], 0);
    push_mono(&mut analyser, &input, 256);
    let (expected, bins) = oracle(&input, &[0.0; 128]);
    matches_oracle(analyser.analyse(2).unwrap(), &expected, &bins);
    assert_eq!(analyser.snapshot().accepted_frames, 512);
}

#[test]
fn byte_mapping_clips_high_and_low_levels_and_format_validation_precedes_planning() {
    for amplitude in [0.0, 1e-8, 1.0] {
        let input = tone(16.0, amplitude);
        let mut analyser = prepare(1);
        push_mono(&mut analyser, &input, 0);
        let frame = analyser.analyse(1).unwrap();
        assert_eq!(frame.bins[16], if amplitude == 1.0 { 255 } else { 0 });
        assert!(frame.decibels.iter().all(|value| value.is_finite()));
    }
    for format in [
        PcmFormat {
            sample_rate: 0,
            channels: 1,
        },
        PcmFormat {
            sample_rate: 384001,
            channels: 1,
        },
        PcmFormat {
            sample_rate: 48000,
            channels: 0,
        },
        PcmFormat {
            sample_rate: 48000,
            channels: 9,
        },
    ] {
        assert!(LxAnalyser::prepare(format).is_err());
    }
}

#[test]
fn callback_push_analyse_reset_snapshot_and_idle_queries_never_touch_heap() {
    let mut analyser = prepare(2);
    let input = tone(8.0, 0.1);
    let reservation = analyser.reserved_bytes();
    assert!(reservation >= std::mem::size_of::<LxAnalyser>());
    no_heap(|| {
        for quantum in 1..=32 {
            for (index, sample) in input.iter().enumerate() {
                analyser
                    .push_frame(
                        &[*sample, *sample],
                        stamp((quantum - 1) * 256 + index as u64 + 1),
                    )
                    .unwrap();
            }
            std::hint::black_box(*analyser.analyse(quantum).unwrap());
            std::hint::black_box(analyser.snapshot());
            std::hint::black_box(*analyser.analyse(quantum).unwrap());
        }
        for quantum in 33..200 {
            std::hint::black_box(*analyser.analyse(quantum).unwrap());
        }
        std::hint::black_box(analyser.reset(stamp(0)).unwrap());
    });
    assert_eq!(analyser.reserved_bytes(), reservation);
    assert_eq!(analyser.snapshot().accepted_frames, 0);
}

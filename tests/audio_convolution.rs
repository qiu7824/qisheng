use lx_gpui_player::audio::{
    convolution::{
        CONVOLUTION_PARTITION, ConvolutionError, ConvolutionFrame, ConvolutionStamp,
        ImpulseBuilder, PreparedConvolution, ValidatedImpulse,
    },
    pcm::{PcmFormat, ProcessingStamp},
};
use std::sync::atomic::AtomicBool;
fn build_impulse(rate: u32, channels: u16, samples: Vec<f32>) -> anyhow::Result<ValidatedImpulse> {
    anyhow::ensure!(
        channels != 0 && samples.len().is_multiple_of(channels as usize),
        "Invalid fixture layout"
    );
    let mut builder = ImpulseBuilder::prepare(rate, channels, samples.len() / channels as usize)?;
    for frame in samples.chunks_exact(channels as usize) {
        builder.push_frame(frame)?;
    }
    builder.finish()
}
fn stamp(index: usize) -> ConvolutionStamp {
    ConvolutionStamp {
        token: 1,
        source_epoch: 2,
        media: ProcessingStamp {
            end_frame: index as u64 + 1,
            ..ProcessingStamp::default()
        },
    }
}
fn render(kernel: &mut PreparedConvolution, input: &[Vec<f64>]) -> Vec<ConvolutionFrame> {
    kernel.reset(1, 2).unwrap();
    let mut output = Vec::new();
    for (index, samples) in input.iter().enumerate() {
        let frame = kernel.frame(samples, stamp(index)).unwrap();
        if frame.valid {
            output.push(frame);
        }
    }
    kernel.begin_eof();
    while let Some(frame) = kernel.drain() {
        if frame.valid {
            output.push(frame);
        }
    }
    assert!(kernel.drained());
    assert!(kernel.drain().is_none());
    output
}
fn direct(input: &[Vec<f64>], ir: &[f32], channels: usize) -> Vec<[f64; 2]> {
    let ir_frames = ir.len() / channels;
    let mut output = vec![[0.0; 2]; input.len() + ir_frames - 1];
    for (sample_index, samples) in input.iter().enumerate() {
        let left = samples[0];
        let right = *samples.get(1).unwrap_or(&left);
        for tap in 0..ir_frames {
            match channels {
                1 => {
                    output[sample_index + tap][0] += left * ir[tap] as f64;
                    output[sample_index + tap][1] += right * ir[tap] as f64;
                }
                2 => {
                    output[sample_index + tap][0] += left * ir[tap * 2] as f64;
                    output[sample_index + tap][1] += right * ir[tap * 2 + 1] as f64;
                }
                4 => {
                    output[sample_index + tap][0] +=
                        left * ir[tap * 4] as f64 + right * ir[tap * 4 + 2] as f64;
                    output[sample_index + tap][1] +=
                        left * ir[tap * 4 + 1] as f64 + right * ir[tap * 4 + 3] as f64;
                }
                _ => panic!("IR layout"),
            }
        }
    }
    output
}
#[test]
fn partitioned_stereo_routes_and_long_tail_match_independent_direct_convolution() {
    for ir_channels in [1, 2, 4] {
        let ir = (0..(1029 * ir_channels))
            .map(|index| {
                if index % (ir_channels * 127) == 0 {
                    0.12 / (1.0 + (index / ir_channels) as f32)
                } else {
                    0.0
                }
            })
            .collect::<Vec<_>>();
        let impulse = build_impulse(22050, ir_channels as u16, ir.clone()).unwrap();
        let mut kernel = PreparedConvolution::prepare(
            PcmFormat {
                sample_rate: 22050,
                channels: 2,
            },
            &impulse,
            false,
            &AtomicBool::new(false),
        )
        .unwrap();
        let input = (0..1237)
            .map(|index| {
                vec![
                    0.2 * (index as f64 * 0.037).sin(),
                    0.13 * (index as f64 * 0.051).cos(),
                ]
            })
            .collect::<Vec<_>>();
        let reference = direct(&input, &ir, ir_channels);
        let output = render(&mut kernel, &input);
        assert_eq!(output.len(), reference.len());
        for (index, (actual, expected)) in output.iter().zip(reference).enumerate() {
            for (lane, value) in expected.iter().enumerate() {
                assert!(
                    (actual.samples[lane] - value).abs() < 1e-9 + value.abs() * 1e-8,
                    "channel {ir_channels} frame {index} lane {lane}"
                );
            }
            assert!(actual.samples[2..].iter().all(|value| *value == 0.0));
            let expected_stamp = stamp(index.min(input.len() - 1));
            assert_eq!(
                actual.stamp,
                if index >= input.len() {
                    None
                } else {
                    Some(expected_stamp)
                }
            );
            assert_eq!(actual.tail, index >= input.len());
        }
    }
}
#[test]
fn unit_impulse_has_exact_declared_delay_and_empty_eof_invents_no_signal() {
    let impulse = build_impulse(48000, 1, vec![1.0]).unwrap();
    let mut kernel = PreparedConvolution::prepare(
        PcmFormat {
            sample_rate: 48000,
            channels: 1,
        },
        &impulse,
        false,
        &AtomicBool::new(false),
    )
    .unwrap();
    kernel.reset(1, 2).unwrap();
    kernel.begin_eof();
    assert!(kernel.drained());
    assert!(kernel.drain().is_none());
    kernel.reset(1, 2).unwrap();
    assert_eq!(kernel.latency_frames(), CONVOLUTION_PARTITION as u32);
    for index in 0..CONVOLUTION_PARTITION {
        assert!(
            !kernel
                .frame(&[if index == 0 { 0.5 } else { 0.0 }], stamp(index))
                .unwrap()
                .valid
        );
    }
    let first = kernel.frame(&[0.0], stamp(CONVOLUTION_PARTITION)).unwrap();
    assert!(first.valid);
    assert!((first.samples[0] - 0.5).abs() < 1e-12);
    assert_eq!(first.stamp, Some(stamp(0)));
}
#[test]
fn normalization_has_independent_rms_floor_rate_and_true_stereo_compensation() {
    for rate in [8000, 22050, 44100, 48000, 96000] {
        for channels in [1, 2, 4] {
            let samples = vec![0.25f32; channels * 17];
            let impulse = build_impulse(rate, channels as u16, samples).unwrap();
            let kernel = PreparedConvolution::prepare(
                PcmFormat {
                    sample_rate: rate,
                    channels: 2,
                },
                &impulse,
                true,
                &AtomicBool::new(false),
            )
            .unwrap();
            let expected = 10.0f64.powf(-58.0 / 20.0) / 0.25 * 44100.0 / rate as f64
                * if channels == 4 { 0.5 } else { 1.0 };
            assert!((kernel.normalization_scale() - expected).abs() < 1e-14);
        }
    }
    let silence = build_impulse(44100, 1, vec![0.0; 17]).unwrap();
    let kernel = PreparedConvolution::prepare(
        PcmFormat {
            sample_rate: 44100,
            channels: 1,
        },
        &silence,
        true,
        &AtomicBool::new(false),
    )
    .unwrap();
    assert!((kernel.normalization_scale() - 10.0f64.powf(-58.0 / 20.0) / 0.000125).abs() < 1e-12);
}
#[test]
fn invalid_input_and_cancelled_preparation_do_not_mutate_a_live_stream() {
    let impulse = build_impulse(22050, 1, vec![0.2, 0.1]).unwrap();
    assert!(
        PreparedConvolution::prepare(
            PcmFormat {
                sample_rate: 22050,
                channels: 1
            },
            &impulse,
            false,
            &AtomicBool::new(true)
        )
        .is_err()
    );
    let mut kernel = PreparedConvolution::prepare(
        PcmFormat {
            sample_rate: 22050,
            channels: 1,
        },
        &impulse,
        false,
        &AtomicBool::new(false),
    )
    .unwrap();
    kernel.reset(1, 2).unwrap();
    assert_eq!(
        kernel.frame(&[f64::NAN], stamp(0)).unwrap_err(),
        ConvolutionError::NonFiniteInput
    );
    assert_eq!(
        kernel.frame(&[f64::MAX], stamp(0)).unwrap_err(),
        ConvolutionError::InputOverflow
    );
    assert_eq!(
        kernel.frame(&[0.5, 0.2], stamp(0)).unwrap_err(),
        ConvolutionError::WrongChannels
    );
    let mut wrong = stamp(0);
    wrong.source_epoch = 3;
    assert_eq!(
        kernel.frame(&[0.5], wrong).unwrap_err(),
        ConvolutionError::WrongStream
    );
    assert!(!kernel.frame(&[0.5], stamp(0)).unwrap().valid);
    kernel.begin_eof();
    let all = std::iter::from_fn(|| kernel.drain()).collect::<Vec<_>>();
    assert_eq!(
        all.iter().take_while(|frame| !frame.valid).count(),
        CONVOLUTION_PARTITION - 1
    );
    let output = all
        .into_iter()
        .filter(|frame| frame.valid)
        .collect::<Vec<_>>();
    assert_eq!(output.len(), 2);
    assert_eq!(output[0].stamp, Some(stamp(0)));
    assert!(output[1].tail && output[1].stamp.is_none());
    assert!((output[0].samples[0] - 0.1).abs() < 1e-8);
    assert!((output[1].samples[0] - 0.05).abs() < 1e-8);
    assert_eq!(
        kernel.frame(&[0.0], stamp(1)).unwrap_err(),
        ConvolutionError::AfterEof
    );
    assert!(build_impulse(22050, 3, vec![0.0; 3]).is_err());
    assert!(build_impulse(22050, 1, vec![f32::INFINITY]).is_err());
}

#[test]
fn extreme_downsampling_preserves_passband_and_rejects_aliasing_in_actual_prepared_ir() {
    let source_rate = 384000u32;
    for frequency in [1000.0, 3000.0, 8000.0] {
        let mut builder = ImpulseBuilder::prepare(source_rate, 1, 19200).unwrap();
        for index in 0..19200 {
            builder
                .push_frame(&[(0.2
                    * (std::f64::consts::TAU * frequency * index as f64 / source_rate as f64).sin())
                    as f32])
                .unwrap();
        }
        let impulse = builder.finish().unwrap();
        let mut kernel = PreparedConvolution::prepare(
            PcmFormat {
                sample_rate: 8000,
                channels: 1,
            },
            &impulse,
            false,
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(kernel.wet_channels(), 2);
        let output = render(&mut kernel, &[vec![1.0]]);
        let actual = output[80..320]
            .iter()
            .map(|frame| frame.samples[0])
            .collect::<Vec<_>>();
        if frequency < 4000.0 {
            let (sine, cosine) =
                actual
                    .iter()
                    .enumerate()
                    .fold((0.0, 0.0), |(sin, cos), (index, sample)| {
                        let angle =
                            std::f64::consts::TAU * frequency * (index + 80) as f64 / 8000.0;
                        (sin + sample * angle.sin(), cos + sample * angle.cos())
                    });
            let amplitude = 2.0 * (sine * sine + cosine * cosine).sqrt() / actual.len() as f64;
            assert!(
                (20.0 * (amplitude / 0.2).log10()).abs() < 0.1,
                "Passband {frequency}: {amplitude}"
            );
        } else {
            let rms = (actual.iter().map(|sample| sample * sample).sum::<f64>()
                / actual.len() as f64)
                .sqrt();
            assert!(rms < 0.0001, "Aliasing RMS: {rms}");
        }
    }
}

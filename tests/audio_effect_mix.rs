use lx_gpui_player::audio::{
    convolution::{CONVOLUTION_PARTITION, ConvolutionStamp, ImpulseBuilder, PreparedConvolution},
    effect_mix::{MixedEffectFrame, PreparedEffectMix},
    pcm::{PcmFormat, ProcessingStamp},
};
use std::sync::atomic::AtomicBool;
fn stamp(index: usize) -> ConvolutionStamp {
    ConvolutionStamp {
        token: 7,
        source_epoch: 3,
        media: ProcessingStamp {
            end_frame: index as u64 + 1,
            ..ProcessingStamp::default()
        },
    }
}
fn bank(channels: u16, ir: &[f32], main: f64, send: f64) -> PreparedEffectMix {
    let mut builder = ImpulseBuilder::prepare(22050, 1, ir.len()).unwrap();
    for value in ir {
        builder.push_frame(&[*value]).unwrap();
    }
    let impulse = builder.finish().unwrap();
    let format = PcmFormat {
        sample_rate: 22050,
        channels,
    };
    let convolution =
        PreparedConvolution::prepare(format, &impulse, false, &AtomicBool::new(false)).unwrap();
    let mut mix = PreparedEffectMix::prepare(format, convolution, main, send).unwrap();
    mix.reset(7, 3).unwrap();
    mix
}
fn collect(mix: &mut PreparedEffectMix, input: &[Vec<f64>]) -> Vec<MixedEffectFrame> {
    let mut output = Vec::new();
    for (index, samples) in input.iter().enumerate() {
        let frame = mix.frame(samples, stamp(index)).unwrap();
        if frame.valid {
            output.push(frame);
        }
    }
    mix.begin_eof();
    while let Some(frame) = mix.drain().unwrap() {
        if frame.valid {
            output.push(frame);
        }
    }
    assert!(mix.drained());
    output
}
#[test]
fn dry_and_wet_are_aligned_at_the_same_original_media_frames_and_tail_has_no_clock() {
    let mut mix = bank(2, &[1.0, 0.25], 0.7, 0.3);
    let input = (0..713)
        .map(|index| {
            vec![
                (index as f64 * 0.11).sin() * 0.2,
                (index as f64 * 0.17).cos() * 0.1,
            ]
        })
        .collect::<Vec<_>>();
    let output = collect(&mut mix, &input);
    assert_eq!(output.len(), input.len() + 1);
    for (index, frame) in output.iter().enumerate() {
        for lane in 0..2 {
            let dry = input.get(index).map_or(0.0, |sample| sample[lane]);
            let previous = if index > 0 {
                input[index - 1][lane]
            } else {
                0.0
            };
            let expected = dry + previous * 0.075;
            assert!((frame.samples[lane] - expected).abs() < 1e-9);
        }
        assert_eq!(
            frame.stamp,
            if index < input.len() {
                Some(stamp(index))
            } else {
                None
            }
        );
        assert_eq!(frame.tail, index >= input.len());
    }
}
#[test]
fn short_eof_and_native_extra_channels_have_exact_equal_delay() {
    let mut mix = bank(6, &[1.0], 1.0, 0.0);
    assert!(
        !mix.frame(&[0.1, 0.2, 0.3, 0.4, 0.5, 0.6], stamp(0))
            .unwrap()
            .valid
    );
    mix.begin_eof();
    for _ in 0..CONVOLUTION_PARTITION - 1 {
        assert!(!mix.drain().unwrap().unwrap().valid);
    }
    let frame = mix.drain().unwrap().unwrap();
    assert!(frame.valid);
    assert_eq!(frame.samples[..6], [0.1, 0.2, 0.3, 0.4, 0.5, 0.6]);
    assert_eq!(frame.stamp, Some(stamp(0)));
    assert!(mix.drain().unwrap().is_none());
}
#[test]
fn mono_wet_lane_width_is_explicit_and_invalid_input_does_not_consume_the_dry_ring() {
    let mut mix = bank(1, &[1.0], 0.5, 0.5);
    assert_eq!(mix.output_channels(), 2);
    assert!(mix.frame(&[0.1, 0.2], stamp(0)).is_err());
    let output = collect(&mut mix, &[vec![0.2]]);
    assert_eq!(output.len(), 1);
    assert!((output[0].samples[0] - 0.2).abs() < 1e-9);
    assert_eq!(output[0].samples[0], output[0].samples[1]);
}

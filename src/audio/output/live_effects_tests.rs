use super::{LiveEffectsBegin, LiveEffectsCapture, LiveEffectsProgress, PreparedLiveEffects};
use crate::audio::{
    convolution::ImpulseBuilder,
    effect_pin::EffectPinTicket,
    effects::EFFECTS_MIN_REPLAY_FRAMES,
    effects_factory::{BankRequest, EffectsRecipe, OwnedImpulse},
    output::{Output, RenderKernel, RenderState, tests::no_heap},
    pcm::{PcmControl, PcmFormat, PcmProducer},
};
use crate::domain::audio_effects::{EffectsSettings, ImpulseResourceId};
use std::sync::atomic::{AtomicBool, Ordering};

fn next_settings() -> EffectsSettings {
    let mut settings = EffectsSettings::default();
    settings.convolution.main_gain = 0.5;
    settings
}

fn dry_reference() -> (Output, RenderKernel) {
    let format = PcmFormat {
        sample_rate: 8_000,
        channels: 2,
    };
    let (mut producer, source, _) = PcmProducer::new(format, 8_000, 0, None).unwrap();
    for index in 0..8_000 {
        assert!(producer.push_frame(index, &[0.04, 0.04]));
    }
    producer.finish();
    let (mut output, kernel) = Output::pair(format).unwrap();
    output.configure_effects(next_settings(), 2, None).unwrap();
    output.attach(source).unwrap();
    output.configure_effects(next_settings(), 2, None).unwrap();
    (output, kernel)
}

fn prepared_live_fixture() -> (
    Output,
    RenderKernel,
    PcmControl,
    PreparedLiveEffects,
    EffectPinTicket,
) {
    let format = PcmFormat {
        sample_rate: 8_000,
        channels: 2,
    };
    let (mut producer, source, control) = PcmProducer::new(format, 8_000, 0, None).unwrap();
    for index in 0..8_000 {
        assert!(producer.push_frame(index, &[0.04, 0.04]));
    }
    producer.finish();
    let mut builder = ImpulseBuilder::prepare(format.sample_rate, 1, 2_048).unwrap();
    for index in 0..2_048 {
        builder
            .push_frame(&[match index {
                0 => 1.0,
                2_047 => 0.25,
                _ => 0.0,
            }])
            .unwrap();
    }
    let id = ImpulseResourceId::Imported("live-owner-response".into());
    let impulse =
        OwnedImpulse::from_validated(builder.finish().unwrap(), id.clone(), "c".repeat(64))
            .unwrap();
    let mut settings = EffectsSettings::default();
    settings.convolution.impulse = Some(id);
    settings.convolution.main_gain = 0.0;
    settings.convolution.send_gain = 1.0;
    let (mut output, mut kernel) = Output::pair(format).unwrap();
    output
        .configure_effects(settings.clone(), 1, Some(impulse.clone()))
        .unwrap();
    output.attach(source).unwrap();
    output
        .configure_effects(settings, 1, Some(impulse))
        .unwrap();
    no_heap(|| kernel.render(&mut [0.0f32; 2 * 1_025]));
    assert!(control.snapshot().position_seconds > 0.0);

    let old_record = output
        .effects
        .record_for_owner(output.effects.active_owner())
        .unwrap();
    let candidate = output
        .effects
        .bank_pool()
        .begin(
            EffectsRecipe::prepare(next_settings(), 2, 2, true, None).unwrap(),
            BankRequest {
                format,
                device_channels: format.channels,
                token: output.token(),
                source_epoch: control.current_epoch(),
                initial_spatial: None,
                replay_capacity: EFFECTS_MIN_REPLAY_FRAMES,
            },
        )
        .unwrap()
        .prepare(&AtomicBool::new(false))
        .unwrap();
    let prepared =
        PreparedLiveEffects::prepare(old_record, candidate, &AtomicBool::new(false)).unwrap();
    assert_eq!(prepared.sequence(), 2);
    assert_eq!(output.effects.owned_banks(), 2);
    assert!(output.effects.request_sequence(2));
    let pin = output
        .effects
        .pin_owner()
        .request(prepared.scope())
        .unwrap();
    no_heap(|| kernel.render::<f32>(&mut []));
    assert!(pin.is_pinned_for(prepared.scope()));
    (output, kernel, control, prepared, pin)
}

fn begin_actual_capture(
    kernel: &mut RenderKernel,
    prepared: PreparedLiveEffects,
    pin: &EffectPinTicket,
) -> (Box<RenderState>, LiveEffectsCapture) {
    let mut state = kernel.state.take().unwrap();
    let capture = match prepared
        .start_on_owner(&mut state, pin, &AtomicBool::new(false))
        .unwrap()
    {
        LiveEffectsBegin::Capturing(capture) => capture,
        LiveEffectsBegin::Deferred(_) => panic!("Initial actual bank has no prior handoff"),
    };
    assert_eq!(capture.sequence(), 2);
    (state, capture)
}

fn finish_actual_capture(
    output: &Output,
    control: &PcmControl,
    state: &mut RenderState,
    capture: &LiveEffectsCapture,
) {
    let before = control.snapshot();
    for _ in 0..1_024 {
        let progress = capture
            .advance_on_owner(state, &AtomicBool::new(false))
            .unwrap();
        let after = control.snapshot();
        assert_eq!(after.consumed_frames, before.consumed_frames);
        assert_eq!(after.queued_frames, before.queued_frames);
        assert_eq!(after.position_seconds, before.position_seconds);
        if progress == LiveEffectsProgress::Ready {
            assert_eq!(output.effects.owned_banks(), 1);
            assert!(state.effects.retained_media_pending());
            assert!(!state.effects.retained_drained());
            return;
        }
        assert_eq!(progress, LiveEffectsProgress::Pending);
        assert_eq!(output.effects.owned_banks(), 2);
    }
    panic!("Bounded actual live capture did not finish");
}

#[test]
fn same_format_live_candidate_preserves_prefix_and_presents_new_media_during_old_tail() {
    let (mut output, mut kernel, control, prepared, pin) = prepared_live_fixture();
    let token = output.token();
    let epoch = control.current_epoch();
    let consumed = control.snapshot().consumed_frames;
    let (mut state, capture) = begin_actual_capture(&mut kernel, prepared, &pin);
    finish_actual_capture(&output, &control, &mut state, &capture);
    kernel.state = Some(state);
    drop(pin);
    let mut previous = control.snapshot().position_seconds;
    while kernel
        .state
        .as_ref()
        .unwrap()
        .effects
        .retained_media_pending()
    {
        no_heap(|| kernel.render(&mut [0.0f32; 2]));
        let current = control.snapshot();
        assert_eq!(current.consumed_frames, consumed);
        assert!(current.position_seconds >= previous);
        previous = current.position_seconds;
    }
    assert!(!kernel.state.as_ref().unwrap().effects.retained_drained());

    let (mut reference, mut reference_kernel) = dry_reference();
    let mut new_media_during_tail = false;
    let mut mixed_tail = false;
    for _ in 0..2_048 {
        let tail = kernel
            .state
            .as_mut()
            .unwrap()
            .effects
            .next_retained_tail()
            .unwrap();
        let mut samples = [0.0f32; 2];
        let mut expected_dry = [0.0f32; 2];
        no_heap(|| reference_kernel.render(&mut expected_dry));
        no_heap(|| kernel.render(&mut samples));
        for lane in 0..2 {
            let retained = tail.as_ref().map_or(0.0, |tail| tail.samples[lane]);
            let expected = f64::from(expected_dry[lane]) + retained;
            assert!(
                (f64::from(samples[lane]) - expected.clamp(-1.0, 1.0)).abs() < 1e-5,
                "New graph and actual retained tail diverged at lane {lane}"
            );
        }
        if output.shared.effect_applied.load(Ordering::Acquire) == 2
            && !kernel.state.as_ref().unwrap().effects.retained_drained()
        {
            new_media_during_tail = true;
            mixed_tail |= tail.is_some_and(|tail| {
                tail.samples[..2].iter().any(|sample| sample.abs() > 1e-5)
                    && samples
                        .iter()
                        .zip(expected_dry)
                        .any(|(actual, dry)| (*actual - dry).abs() > 1e-5)
            });
        }
        assert_eq!(output.token(), token);
        assert_eq!(control.current_epoch(), epoch);
    }
    assert!(
        new_media_during_tail,
        "New media waited for the old IR tail"
    );
    assert!(mixed_tail, "The actual old delayed IR tail was not mixed");
    assert_eq!(output.effects_snapshot().applied_sequence, 2);
    assert!(control.snapshot().consumed_frames > consumed);
    assert!(!output.ended());
    assert_eq!(output.effects.owned_banks(), 1);
    no_heap(|| drop(reference_kernel));
    reference.reap();
    assert_eq!(reference.effects.owned_banks(), 0);
    no_heap(|| drop(kernel));
    output.reap();
    assert_eq!(output.effects.owned_banks(), 0);
}

#[test]
fn paused_live_prefix_keeps_exact_pending_output_and_media_clock_until_resume() {
    let (mut output, mut kernel, control, prepared, pin) = prepared_live_fixture();
    let (mut state, capture) = begin_actual_capture(&mut kernel, prepared, &pin);
    finish_actual_capture(&output, &control, &mut state, &capture);
    kernel.state = Some(state);
    drop(pin);
    output.set_paused(true);
    let before = control.snapshot();
    for _ in 0..4 {
        let mut samples = [1.0f32; 128];
        no_heap(|| kernel.render(&mut samples));
        assert!(samples.iter().all(|sample| *sample == 0.0));
        let current = control.snapshot();
        assert_eq!(current.consumed_frames, before.consumed_frames);
        assert_eq!(current.queued_frames, before.queued_frames);
        assert_eq!(current.position_seconds, before.position_seconds);
        assert!(
            kernel
                .state
                .as_ref()
                .unwrap()
                .effects
                .retained_media_pending()
        );
    }
    output.set_paused(false);
    for _ in 0..2_048 {
        no_heap(|| kernel.render(&mut [0.0f32; 2]));
        if output.shared.effect_applied.load(Ordering::Acquire) == 2 {
            break;
        }
    }
    assert_eq!(output.effects_snapshot().applied_sequence, 2);
    assert!(control.snapshot().position_seconds > before.position_seconds);
    no_heap(|| drop(kernel));
    output.reap();
    assert_eq!(output.effects.owned_banks(), 0);
}

#[test]
fn cancelled_owner_capture_keeps_both_actual_banks_and_resumes_without_source_refeeding() {
    let (mut output, mut kernel, control, prepared, pin) = prepared_live_fixture();
    let (mut state, capture) = begin_actual_capture(&mut kernel, prepared, &pin);
    let before = control.snapshot();
    assert_eq!(
        capture
            .advance_on_owner(&mut state, &AtomicBool::new(false))
            .unwrap(),
        LiveEffectsProgress::Pending
    );
    assert_eq!(output.effects.owned_banks(), 2);
    for _ in 0..3 {
        assert_eq!(
            capture
                .advance_on_owner(&mut state, &AtomicBool::new(true))
                .unwrap(),
            LiveEffectsProgress::Suspended
        );
        assert_eq!(output.effects.owned_banks(), 2);
        let current = control.snapshot();
        assert_eq!(current.consumed_frames, before.consumed_frames);
        assert_eq!(current.queued_frames, before.queued_frames);
        assert_eq!(current.position_seconds, before.position_seconds);
    }
    finish_actual_capture(&output, &control, &mut state, &capture);
    kernel.state = Some(state);
    drop(pin);
    for _ in 0..2_048 {
        no_heap(|| kernel.render(&mut [0.0f32; 2]));
        if output.shared.effect_applied.load(Ordering::Acquire) == 2 {
            break;
        }
    }
    assert_eq!(output.effects_snapshot().applied_sequence, 2);
    assert!(control.snapshot().position_seconds > before.position_seconds);
    no_heap(|| drop(kernel));
    output.reap();
    assert_eq!(output.effects.owned_banks(), 0);
}

#[test]
fn production_memory_mailbox_defers_only_prefix_then_recaptures_previous_tail_for_third_setting() {
    let (mut output, mut kernel, control, prepared, pin) = prepared_live_fixture();
    let token = output.token();
    let epoch = control.current_epoch();
    drop(pin);
    no_heap(|| kernel.render::<f32>(&mut []));
    output.shared.live_effects.offer(prepared).unwrap();
    let before = control.snapshot();
    super::service_memory_owner(&mut kernel, &output.shared);
    assert!(!output.shared.live_effects.occupied());
    assert_eq!(control.snapshot().consumed_frames, before.consumed_frames);
    assert_eq!(control.snapshot().position_seconds, before.position_seconds);
    assert!(
        kernel
            .state
            .as_ref()
            .unwrap()
            .effects
            .retained_media_pending()
    );

    let mut third = next_settings();
    third.convolution.main_gain = 0.7;
    output.configure_effects(third, 3, None).unwrap();
    assert!(output.shared.live_effects.occupied());
    super::service_memory_owner(&mut kernel, &output.shared);
    assert!(output.shared.live_effects.occupied());
    assert_eq!(output.effects.owned_banks(), 2);
    for _ in 0..2_048 {
        super::service_memory_owner(&mut kernel, &output.shared);
        no_heap(|| kernel.render(&mut [0.0f32; 2]));
        assert_eq!(output.token(), token);
        assert_eq!(control.current_epoch(), epoch);
        assert!(output.effects.owned_banks() <= 2);
        if output.effects_snapshot().applied_sequence == 3 {
            break;
        }
    }
    assert_eq!(output.effects_snapshot().applied_sequence, 3);
    assert!(
        !kernel.state.as_ref().unwrap().effects.retained_drained(),
        "The third actual graph waited for the retained old tail"
    );
    assert!(!output.shared.live_effects.occupied());
    no_heap(|| drop(kernel));
    output.reap();
    assert_eq!(output.effects.owned_banks(), 0);
}

fn recovery_long_impulse_fixture() -> (
    Arc<MockControl>,
    Output,
    PcmControl,
    crate::audio::effects_factory::EffectsBankPool,
) {
    let mock = MockControl::new();
    {
        let mut topology = mock.topology.lock().unwrap();
        topology.devices[0].format = PcmFormat {
            sample_rate: 8_000,
            channels: 1,
        };
        topology.devices[1].format = PcmFormat {
            sample_rate: 16_000,
            channels: 2,
        };
    }
    let mut output = output(&mock, AudioPreferences::default());
    ready(&output, &mock, "endpoint-a");
    let (mut producer, source, control) = PcmProducer::new(
        PcmFormat {
            sample_rate: 8_000,
            channels: 1,
        },
        128,
        0,
        None,
    )
    .unwrap();
    for index in 0..128 {
        assert!(producer.push_frame(index, &[0.1]));
    }
    producer.finish();
    output.attach(source).unwrap();
    let id = crate::domain::audio_effects::ImpulseResourceId::Bundled(
        "device-recovery-long-impulse".into(),
    );
    let mut builder = crate::audio::convolution::ImpulseBuilder::prepare(8_000, 1, 2_051).unwrap();
    for index in 0..2_051 {
        builder
            .push_frame(&[match index {
                0 => 1.0,
                2_050 => 0.5,
                _ => 0.001,
            }])
            .unwrap();
    }
    let ir = crate::audio::effects_factory::OwnedImpulse::from_validated(
        builder.finish().unwrap(),
        id.clone(),
        "4".repeat(64),
    )
    .unwrap();
    let mut settings = crate::domain::audio_effects::EffectsSettings::default();
    settings.convolution.impulse = Some(id);
    settings.convolution.send_gain = 0.3;
    let recipe =
        crate::audio::effects_factory::EffectsRecipe::prepare(settings, 1, 2, false, Some(ir))
            .unwrap();
    let pool = output.effects.bank_pool();
    let bank = pool
        .begin(
            recipe,
            BankRequest {
                format: PcmFormat {
                    sample_rate: 8_000,
                    channels: 2,
                },
                device_channels: 1,
                token: output.token(),
                source_epoch: control.current_epoch(),
                replay_capacity: crate::audio::effects::EFFECTS_MIN_REPLAY_FRAMES,
                initial_spatial: None,
            },
        )
        .unwrap()
        .prepare(&AtomicBool::new(false))
        .unwrap();
    assert!(output.effects.request_sequence(1));
    assert!(output.effects.install_counted(bank).is_ok());
    mock.render(0, &mut [0.0f32; 7]);
    assert_eq!(control.snapshot().position_seconds, 0.0);
    output
        .select(OutputSelection::Device {
            id: device_id("endpoint-b"),
        })
        .unwrap();
    until(|| {
        mock.render(0, &mut []);
        waiting_handoff(&mock, &output, 1)
    });
    ready(&output, &mock, "endpoint-b");
    let consumed = control.snapshot().consumed_frames;
    for _ in 0..128 {
        let pending = {
            let callbacks = mock.callbacks.lock().unwrap();
            callbacks[1]
                .kernel
                .as_ref()
                .unwrap()
                .state
                .as_ref()
                .unwrap()
                .effects
                .retained_media_pending()
        };
        if !pending {
            break;
        }
        mock.render(1, &mut [0.0f32; 2]);
        assert_eq!(control.snapshot().consumed_frames, consumed);
    }
    {
        let mut callbacks = mock.callbacks.lock().unwrap();
        let state = callbacks[1]
            .kernel
            .as_mut()
            .unwrap()
            .state
            .as_mut()
            .unwrap();
        assert!(!state.effects.retained_media_pending());
        assert!(state.effects.has_handoff());
        assert_eq!(
            state
                .effects
                .active()
                .unwrap()
                .statistics()
                .accepted_media_frames,
            0
        );
    }
    assert_eq!(pool.occupied_banks(), 1);
    (mock, output, control, pool)
}

fn recovery_current_pending_and_previous_tail(
    mock: &MockControl,
    control: &PcmControl,
) -> (
    crate::audio::effects::EffectsFrame,
    crate::audio::effects_handoff::HandoffFrame,
) {
    let first_tail = {
        let mut callbacks = mock.callbacks.lock().unwrap();
        callbacks[1]
            .kernel
            .as_mut()
            .unwrap()
            .state
            .as_mut()
            .unwrap()
            .effects
            .next_retained_tail()
            .unwrap()
            .unwrap()
    };
    let consumed = control.snapshot().consumed_frames;
    for _ in 0..4 {
        mock.render(1, &mut [0.0f32; 2]);
    }
    assert!(control.snapshot().consumed_frames > consumed);
    control.set_paused(true);
    let mut callbacks = mock.callbacks.lock().unwrap();
    let state = callbacks[1]
        .kernel
        .as_mut()
        .unwrap()
        .state
        .as_mut()
        .unwrap();
    assert!(state.effects.has_handoff());
    assert!(!state.effects.retained_media_pending());
    assert!(!state.effects.retained_drained());
    let tail = state.effects.next_retained_tail().unwrap().unwrap();
    assert_ne!(
        tail, first_tail,
        "The physical callback must ACK part of the old tail"
    );
    let bank = state.effects.active().unwrap();
    assert!(bank.statistics().accepted_media_frames >= 4);
    assert_eq!(bank.statistics().presented_media_frames, 0);
    assert!(bank.pending_media_frames() >= 4);
    // Advance only the actual bank's already accepted delayed PCM. No source
    // read, raw-input replay, presentation receipt, or clock commit occurs here.
    bank.begin_eof().unwrap();
    let mut pending = None;
    for _ in 0..4_096 {
        let frame = bank.drain().unwrap().unwrap();
        if frame.kind == crate::audio::effects::EffectsFrameKind::Media {
            pending = Some(frame);
            break;
        }
        assert_eq!(frame.kind, crate::audio::effects::EffectsFrameKind::Padding);
    }
    let pending = pending.expect("The actual A bank retains its first future media output");
    assert_eq!(bank.pending_output(), Some(pending));
    assert!(
        pending.samples[..2]
            .iter()
            .any(|sample| sample.abs() > 1.0e-6)
    );
    assert!(tail.samples[..2].iter().any(|sample| sample.abs() > 1.0e-6));
    (pending, tail)
}

fn recovery_assert_clocked_drain(
    mock: &MockControl,
    output: &Output,
    control: &PcmControl,
    index: usize,
    channels: usize,
) {
    let mut sample = [0.0f32; 2];
    let mut position = control.snapshot().position_seconds;
    for _ in 0..20_000 {
        if output.ended() {
            break;
        }
        mock.render(index, &mut sample[..channels]);
        let next = control.snapshot().position_seconds;
        assert!(
            next >= position,
            "Recovery must preserve the original media clock"
        );
        position = next;
    }
    assert!(output.ended());
    assert_eq!(control.snapshot().consumed_frames, 128);
    assert!((position - 128.0 / 8_000.0).abs() < 1.0e-10);
}

fn recovery_future_media_partial_tail_case(format: PcmFormat, queued_candidate: bool) {
    let (mock, mut output, control, pool) = recovery_long_impulse_fixture();
    let (pending_a, previous_tail) = recovery_current_pending_and_previous_tail(&mock, &control);
    let owner_a = output.effects.active_owner();
    let record = output.effects.record_for_owner(owner_a).unwrap();
    let old_plan = record.bounds().preparation();
    let queued_id = if queued_candidate {
        let queued = pool
            .begin(
                record.recipe().clone(),
                BankRequest {
                    format: old_plan.format,
                    device_channels: old_plan.device_channels,
                    token: old_plan.token,
                    source_epoch: old_plan.source_epoch,
                    replay_capacity: old_plan.replay_capacity,
                    initial_spatial: Some(old_plan.initial_spatial),
                },
            )
            .unwrap()
            .prepare(&AtomicBool::new(false))
            .unwrap();
        let id = queued.record().owner_id();
        assert!(output.effects.install_counted(queued).is_ok());
        assert_eq!(pool.occupied_banks(), 2);
        Some(id)
    } else {
        None
    };
    let consumed = control.snapshot().consumed_frames;
    let position = control.snapshot().position_seconds;
    assert!(position > 0.0);
    mock.topology.lock().unwrap().devices[1].format = format;
    mock.error(1, cpal::StreamError::StreamInvalidated);
    until(|| mock.latest() == Some(2));
    ready(&output, &mock, "endpoint-b");
    assert_eq!(control.snapshot().position_seconds, position);
    assert_eq!(control.snapshot().consumed_frames, consumed);
    assert_eq!(pool.occupied_banks(), 1);
    assert!(output.effects.record_for_owner(owner_a).is_none());
    if let Some(id) = queued_id {
        assert!(output.effects.record_for_owner(id).is_none());
    }
    let mut sample = [9.0f32; 2];
    mock.render(2, &mut sample[..format.channels as usize]);
    assert_eq!(
        sample[..format.channels as usize],
        [0.0f32; 2][..format.channels as usize]
    );
    assert_eq!(control.snapshot().position_seconds, position);
    assert_eq!(control.snapshot().consumed_frames, consumed);
    control.set_paused(false);
    mock.render(2, &mut sample[..format.channels as usize]);
    assert_eq!(
        control.snapshot().consumed_frames,
        consumed,
        "Retained PCM must not pull source"
    );
    let combined = [
        pending_a.samples[0] + previous_tail.samples[0],
        pending_a.samples[1] + previous_tail.samples[1],
    ];
    let expected = if format.channels == 1 {
        [(combined[0] + combined[1]) / 2.0, 0.0]
    } else {
        combined
    };
    for lane in 0..format.channels as usize {
        assert!(
            (f64::from(sample[lane]) - expected[lane].clamp(-1.0, 1.0)).abs() < 1.0e-6,
            "Native map of actual pending A plus remaining old tail differs at lane {lane}: {:?} vs {:?}",
            sample,
            expected
        );
    }
    assert!(
        (control.snapshot().position_seconds - position - 1.0 / f64::from(format.sample_rate))
            .abs()
            < 1.0e-10
    );
    recovery_assert_clocked_drain(&mock, &output, &control, 2, format.channels as usize);
    assert_eq!(pool.occupied_banks(), 1);
    assert!(output.shutdown(Duration::from_secs(1)));
    assert_eq!(pool.occupied_banks(), 0);
}

#[test]
fn faulted_future_effect_media_and_partially_acked_tail_recover_to_mono_with_exact_mix() {
    recovery_future_media_partial_tail_case(
        PcmFormat {
            sample_rate: 24_000,
            channels: 1,
        },
        true,
    );
}

#[test]
fn faulted_future_effect_media_and_partially_acked_tail_recover_to_stereo_with_exact_mix() {
    recovery_future_media_partial_tail_case(
        PcmFormat {
            sample_rate: 32_000,
            channels: 2,
        },
        false,
    );
}

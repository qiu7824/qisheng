use lx_gpui_player::audio::{
    pcm::MAX_CHANNELS,
    spatial::{
        SpatialCoefficients, SpatialPhaseClock, SpatialPlan, SpatialRenderer, SpatialSettings,
        SpatialStep,
    },
};
use std::{
    alloc::{GlobalAlloc, Layout, System},
    cell::Cell,
    f64::consts::{FRAC_1_SQRT_2, FRAC_PI_2, PI},
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
    assert_eq!(
        HEAP.with(Cell::get),
        (0, 0),
        "Spatial rendering allocated or freed heap storage"
    );
}
fn settings(radius: f64, speed: u8) -> SpatialSettings {
    SpatialSettings {
        enabled: true,
        radius,
        speed,
    }
}
fn prepare_plan(radius: f64, speed: u8, sequence: u64) -> SpatialPlan {
    SpatialPlan::prepare(settings(radius, speed), 48_000, sequence).unwrap()
}
fn degree(value: u16) -> SpatialStep {
    SpatialStep::degree(value).unwrap()
}
fn pair(left: f64, right: f64) -> [f64; MAX_CHANNELS] {
    let mut frame = [0.0; MAX_CHANNELS];
    frame[0] = left;
    frame[1] = right;
    frame
}
fn near(actual: f64, expected: f64) {
    assert!(
        (actual - expected).abs() <= 3e-13,
        "actual={actual:.17e} expected={expected:.17e}"
    );
}

// Independent scalar oracle uses normalized listener-vector projection and
// explicit channel equations, rather than the kernel's atan2/table helpers.
fn scalar_coefficients(radius: f64, degree: u16) -> SpatialCoefficients {
    let theta = f64::from(degree) * PI / 180.0;
    let x = radius * theta.sin();
    let y = radius * theta.cos();
    let z = y;
    let horizontal = (x * x + z * z).sqrt();
    let angle_from_right = (x / horizontal).clamp(-1.0, 1.0).acos() * 180.0 / PI;
    let projected_angle = if -z / horizontal < 0.0 {
        360.0 - angle_from_right
    } else {
        angle_from_right
    };
    let mut azimuth = 90.0 - projected_angle;
    if azimuth < -180.0 {
        azimuth += 360.0;
    }
    if azimuth < -90.0 {
        azimuth = -180.0 - azimuth;
    }
    if azimuth > 90.0 {
        azimuth = 180.0 - azimuth;
    }
    let distance = (x * x + y * y + z * z).sqrt();
    let gain = 1.0 / distance.max(1.0);
    let mono_angle = (azimuth + 90.0) / 180.0 * FRAC_PI_2;
    let mono = [gain * mono_angle.cos(), gain * mono_angle.sin()];
    let stereo = if azimuth <= 0.0 {
        let angle = (azimuth + 90.0) / 90.0 * FRAC_PI_2;
        [[gain, gain * angle.cos()], [0.0, gain * angle.sin()]]
    } else {
        let angle = azimuth / 90.0 * FRAC_PI_2;
        [[gain * angle.cos(), 0.0], [gain * angle.sin(), gain]]
    };
    SpatialCoefficients { mono, stereo }
}

#[test]
fn settings_round_trip_rejects_invalid_and_unknown_values() {
    let default = SpatialSettings::default();
    assert_eq!(
        default,
        SpatialSettings {
            enabled: false,
            radius: 0.5,
            speed: 25
        }
    );
    assert_eq!(
        serde_json::from_str::<SpatialSettings>("{}").unwrap(),
        default
    );
    let encoded = serde_json::to_string(&settings(3.0, 50)).unwrap();
    assert_eq!(
        serde_json::from_str::<SpatialSettings>(&encoded).unwrap(),
        settings(3.0, 50)
    );
    for document in [
        r#"{"radius":0}"#,
        r#"{"radius":3.01}"#,
        r#"{"speed":0}"#,
        r#"{"speed":51}"#,
        r#"{"speed":256}"#,
        r#"{"radius":null}"#,
        r#"{"other":1}"#,
    ] {
        assert!(
            serde_json::from_str::<SpatialSettings>(document).is_err(),
            "{document}"
        );
    }
    for radius in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        assert!(SpatialPlan::prepare(settings(radius, 25), 48_000, 1).is_err());
    }
    assert_eq!(default.tick_interval_ns().unwrap(), 50_000_000);
    assert!(SpatialStep::degree(0).is_err());
    assert!(SpatialStep::degree(361).is_err());
    assert!(SpatialRenderer::new(0, 2, 2).is_err());
    assert!(SpatialRenderer::new(48_000, 0, 2).is_err());
    assert!(SpatialRenderer::new(48_000, 2, 9).is_err());
}

#[test]
fn every_prepared_position_matches_independent_scalar_projection() {
    for radius in [0.1, 0.5, 1.0, 3.0] {
        let plan = prepare_plan(radius, 25, 1);
        for angle in 1..=360 {
            let actual = plan.coefficients(degree(angle));
            let expected = scalar_coefficients(radius, angle);
            for (a, e) in actual.mono.iter().zip(expected.mono) {
                near(*a, e);
            }
            for (a, e) in actual
                .stereo
                .iter()
                .flatten()
                .zip(expected.stereo.iter().flatten())
            {
                near(*a, *e);
            }
            assert!(
                actual
                    .mono
                    .iter()
                    .chain(actual.stereo.iter().flatten())
                    .all(|value| value.is_finite() && (0.0..=1.0).contains(value))
            );
        }
        assert!(plan.owned_bytes() >= 360 * std::mem::size_of::<SpatialCoefficients>());
    }
}

#[test]
fn mono_power_distance_and_front_rear_cardinals_are_exact() {
    let plan = prepare_plan(0.5, 25, 1);
    assert_eq!(
        plan.coefficients(SpatialStep::ORIGIN).mono,
        [FRAC_1_SQRT_2; 2]
    );
    assert_eq!(plan.coefficients(degree(90)).mono, [0.0, 1.0]);
    assert_eq!(plan.coefficients(degree(270)).mono, [1.0, 0.0]);
    for angle in 1..=360 {
        let [left, right] = plan.coefficients(degree(angle)).mono;
        near(left * left + right * right, 1.0);
    }
    let large = prepare_plan(3.0, 25, 2);
    near(large.coefficients(degree(90)).mono[1], 1.0 / 3.0);
    near(
        large.coefficients(degree(180)).stereo[0][0],
        1.0 / (3.0 * 2.0f64.sqrt()),
    );
    assert_eq!(
        large.coefficients(degree(180)),
        large.coefficients(degree(360))
    );
    assert_ne!(
        large.coefficients(degree(90)).mono[1],
        large.coefficients(degree(180)).stereo[0][0]
    );
}

#[test]
fn stereo_center_is_bit_exact_and_cardinals_use_crossfeed() {
    let plan = prepare_plan(0.5, 25, 1);
    let identity = [[1.0, 0.0], [0.0, 1.0]];
    assert_eq!(plan.coefficients(SpatialStep::ORIGIN).stereo, identity);
    assert_eq!(plan.coefficients(degree(180)).stereo, identity);
    assert_eq!(plan.coefficients(degree(360)).stereo, identity);
    let mut renderer = SpatialRenderer::new(48_000, 2, 2).unwrap();
    renderer.retarget(plan.target(degree(180)));
    renderer.reset_to_target();
    for frame in [pair(-0.0, 0.125), pair(-0.27, 0.8125)] {
        let (actual, errors) = renderer.frame(frame);
        assert_eq!(errors, 0);
        assert_eq!(actual.map(f64::to_bits), frame.map(f64::to_bits));
    }
    renderer.retarget(plan.target(degree(90)));
    renderer.reset_to_target();
    assert_eq!(renderer.frame(pair(0.25, 0.75)).0[..2], [0.0, 1.0]);
    renderer.retarget(plan.target(degree(270)));
    renderer.reset_to_target();
    assert_eq!(renderer.frame(pair(0.25, 0.75)).0[..2], [1.0, 0.0]);
    renderer.retarget(plan.target(degree(90)));
    renderer.reset_to_target();
    assert_eq!(renderer.frame(pair(1.0, 1.0)).0[..2], [0.0, 2.0]);
}

#[test]
fn renderer_scalar_oracle_keeps_stereo_crossfeed_separate_from_mono_gains() {
    for radius in [0.1, 0.5, 1.0, 3.0] {
        let plan = prepare_plan(radius, 25, 1);
        let mut renderer = SpatialRenderer::new(48_000, 2, 2).unwrap();
        let mut mono = SpatialRenderer::new(48_000, 1, 2).unwrap();
        for angle in 1..=360 {
            let coefficients = scalar_coefficients(radius, angle);
            let target = plan.target(degree(angle));
            renderer.retarget(target);
            renderer.reset_to_target();
            mono.retarget(target);
            mono.reset_to_target();
            let input = pair(0.23, -0.79);
            let (actual, errors) = renderer.frame(input);
            assert_eq!(errors, 0);
            for (lane, sample) in actual.iter().take(2).enumerate() {
                near(
                    *sample,
                    input[0] * coefficients.stereo[lane][0]
                        + input[1] * coefficients.stereo[lane][1],
                );
            }
            let (actual, errors) = mono.frame(pair(0.37, 0.0));
            assert_eq!(errors, 0);
            for (lane, sample) in actual.iter().take(2).enumerate() {
                near(*sample, 0.37 * coefficients.mono[lane]);
            }
        }
    }
}

#[test]
fn fake_clock_tick_and_rotation_durations_preserve_wall_time() {
    for (speed, revolution_ms) in [(1, 720), (25, 18_000), (50, 36_000)] {
        let plan = prepare_plan(0.5, speed, 1);
        let anchor = 1_234;
        let mut clock = SpatialPhaseClock::new(&plan, anchor);
        let tick = u64::from(speed) * 2_000_000;
        assert_eq!(clock.next_tick_ns(), Some(anchor + tick));
        assert_eq!(
            clock.sample(&plan, anchor + tick - 1).unwrap().step(),
            SpatialStep::ORIGIN
        );
        assert_eq!(
            clock.sample(&plan, anchor + tick).unwrap().step(),
            degree(1)
        );
        assert_eq!(
            clock
                .sample(&plan, anchor + revolution_ms * 1_000_000)
                .unwrap()
                .step(),
            degree(360)
        );
        assert_eq!(
            clock
                .sample(&plan, anchor + revolution_ms * 1_000_000 + tick)
                .unwrap()
                .step(),
            degree(1)
        );
        // A paused or replaced source does not consume, reset, or own the clock.
        assert_eq!(
            clock
                .sample(&plan, anchor + 10 * revolution_ms * 1_000_000 + 7 * tick)
                .unwrap()
                .step(),
            degree(7)
        );
    }
}

#[test]
fn speed_restart_keeps_last_pose_until_first_new_tick() {
    let old = prepare_plan(3.0, 25, 4);
    let new = prepare_plan(3.0, 1, 5);
    let mut clock = SpatialPhaseClock::new(&old, 0);
    let prior = clock.sample(&old, 2_000_000_000).unwrap();
    assert_eq!(prior.step(), degree(40));
    assert_eq!(clock.reconfigure(&new, 2_000_000_000).unwrap(), prior);
    assert_eq!(clock.sample(&new, 2_001_999_999).unwrap(), prior);
    let updated = clock.sample(&new, 2_002_000_000).unwrap();
    assert_eq!(updated.step(), degree(1));
    assert_eq!(updated.sequence(), 5);
    assert_eq!(updated.coefficients(), new.coefficients(degree(1)));
}

#[test]
fn radius_changes_at_next_tick_and_disable_moves_to_origin() {
    let old = prepare_plan(0.5, 25, 1);
    let new = prepare_plan(3.0, 25, 2);
    let mut clock = SpatialPhaseClock::new(&old, 0);
    let prior = clock.sample(&old, 90_000_000).unwrap();
    assert_eq!(prior.step(), degree(1));
    assert_eq!(clock.reconfigure(&new, 90_000_000).unwrap(), prior);
    assert_eq!(clock.sample(&new, 99_999_999).unwrap(), prior);
    let updated = clock.sample(&new, 100_000_000).unwrap();
    assert_eq!(updated.step(), degree(2));
    assert_eq!(updated.coefficients(), new.coefficients(degree(2)));
    let stopped = SpatialPlan::prepare(SpatialSettings::default(), 48_000, 3).unwrap();
    let origin = clock.reconfigure(&stopped, 100_000_001).unwrap();
    assert!(!origin.enabled());
    assert_eq!(origin.step(), SpatialStep::ORIGIN);
    assert_eq!(clock.next_tick_ns(), None);
    assert_eq!(clock.sample(&stopped, 10_000_000_000).unwrap(), origin);
    let enabled = prepare_plan(3.0, 25, 4);
    let origin = clock.reconfigure(&enabled, 10_000_000_000).unwrap();
    assert!(origin.enabled());
    assert_eq!(origin.step(), SpatialStep::ORIGIN);
    assert_eq!(
        clock.sample(&enabled, 10_050_000_000).unwrap().step(),
        degree(1)
    );
}

#[test]
fn clock_rejects_wrong_plan_and_backward_time_without_moving_phase() {
    let plan = prepare_plan(0.5, 25, 1);
    let wrong = prepare_plan(0.5, 25, 2);
    let mut clock = SpatialPhaseClock::new(&plan, 100);
    assert!(clock.sample(&wrong, 101).is_err());
    assert!(clock.reconfigure(&wrong, 99).is_err());
    assert!(clock.sample(&plan, 99).is_err());
    assert_eq!(clock.sample(&plan, 50_000_100).unwrap().step(), degree(1));
    let mut extreme = SpatialPhaseClock::new(&plan, u64::MAX);
    assert_eq!(extreme.next_tick_ns(), None);
    assert_eq!(
        extreme.sample(&plan, u64::MAX).unwrap().step(),
        SpatialStep::ORIGIN
    );
}

#[test]
fn native_extra_channels_are_isolated_and_mono_folding_occurs_after_pan() {
    let plan = prepare_plan(0.5, 25, 1);
    let input = [0.2, 0.6, -0.3, 0.4, -0.5, 0.7, 0.8, -0.9];
    let mut renderer = SpatialRenderer::new(48_000, 8, 8).unwrap();
    renderer.retarget(plan.target(degree(90)));
    renderer.reset_to_target();
    let (actual, errors) = renderer.frame(input);
    assert_eq!(errors, 0);
    assert_eq!(actual[..2], [0.0, 0.8]);
    assert_eq!(
        actual[2..].iter().map(|v| v.to_bits()).collect::<Vec<_>>(),
        input[2..].iter().map(|v| v.to_bits()).collect::<Vec<_>>()
    );
    let mut folded = SpatialRenderer::new(48_000, 2, 1).unwrap();
    folded.retarget(plan.target(degree(90)));
    folded.reset_to_target();
    near(folded.frame(pair(0.2, 0.6)).0[0], 0.4);
    let mut mono = SpatialRenderer::new(48_000, 1, 1).unwrap();
    mono.retarget(plan.target(degree(90)));
    mono.reset_to_target();
    near(mono.frame(pair(1.0, 0.0)).0[0], 0.5);
    let mut eight_to_two = SpatialRenderer::new(48_000, 8, 2).unwrap();
    eight_to_two.retarget(plan.target(degree(90)));
    eight_to_two.reset_to_target();
    let actual = eight_to_two.frame(input).0;
    near(actual[0], (0.0 - 0.3 - 0.5 + 0.8) / 4.0);
    near(actual[1], (0.8 + 0.4 + 0.7 - 0.9) / 4.0);
}

#[test]
fn disabled_mono_mapping_is_exact_and_activation_has_bounded_twenty_ms_blend() {
    let enabled = SpatialPlan::prepare(settings(0.5, 25), 8_000, 1).unwrap();
    let stopped = SpatialPlan::prepare(SpatialSettings::default(), 8_000, 2).unwrap();
    let mut renderer = SpatialRenderer::new(8_000, 1, 8).unwrap();
    assert_eq!(renderer.frame(pair(1.0, 0.0)), ([1.0; 8], 0));
    assert!(renderer.retarget(enabled.target(SpatialStep::ORIGIN)));
    assert_eq!(renderer.transition_remaining_frames(), 160);
    let mut prior = [1.0; 8];
    for frame in 1..=160 {
        let (actual, errors) = renderer.frame(pair(1.0, 0.0));
        assert_eq!(errors, 0);
        for lane in 0..8 {
            assert!(actual[lane] <= prior[lane] && actual[lane] >= 0.0);
        }
        assert_eq!(renderer.transition_remaining_frames(), 160 - frame);
        prior = actual;
    }
    assert_eq!(
        prior,
        [FRAC_1_SQRT_2, FRAC_1_SQRT_2, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]
    );
    assert!(renderer.retarget(stopped.target(SpatialStep::ORIGIN)));
    for _ in 0..160 {
        renderer.frame(pair(1.0, 0.0));
    }
    assert_eq!(renderer.frame(pair(0.75, 0.0)), ([0.75; 8], 0));
    assert_eq!(renderer.latency_frames(), 0);
}

#[test]
fn latest_target_transition_and_reset_keep_finite_peak_bounds() {
    let plan = prepare_plan(0.5, 25, 1);
    let mut renderer = SpatialRenderer::new(48_000, 2, 2).unwrap();
    for iteration in 0..5_000 {
        renderer.retarget(plan.target(degree(if iteration % 2 == 0 { 90 } else { 270 })));
        let (output, errors) = renderer.frame(pair(1.0, 1.0));
        assert_eq!(errors, 0);
        assert!(
            output
                .iter()
                .all(|value| value.is_finite() && value.abs() <= 2.0)
        );
    }
    renderer.retarget(plan.target(degree(90)));
    renderer.reset_to_target();
    assert_eq!(renderer.transition_remaining_frames(), 0);
    assert_eq!(renderer.frame(pair(1.0, 1.0)), (pair(0.0, 2.0), 0));
    renderer.retarget(plan.target(degree(270)));
    for _ in 0..960 {
        renderer.frame(pair(1.0, 1.0));
    }
    assert_eq!(renderer.frame(pair(1.0, 1.0)), (pair(2.0, 0.0), 0));
}

#[test]
fn format_mismatch_cannot_replace_current_target_or_interpolation() {
    let plan = prepare_plan(0.5, 25, 1);
    let wrong = SpatialPlan::prepare(settings(3.0, 50), 96_000, 2).unwrap();
    let mut renderer = SpatialRenderer::new(48_000, 2, 2).unwrap();
    renderer.retarget(plan.target(degree(90)));
    renderer.frame(pair(1.0, 1.0));
    let target = renderer.target();
    let remaining = renderer.transition_remaining_frames();
    assert!(!renderer.retarget(wrong.target(degree(270))));
    assert_eq!(renderer.target(), target);
    assert_eq!(renderer.transition_remaining_frames(), remaining);
}

#[test]
fn all_layouts_and_supported_rates_stay_finite_on_silence_and_invalid_input() {
    for sample_rate in [8_000, 22_050, 44_100, 48_000, 96_000, 192_000, 384_000] {
        let plan = SpatialPlan::prepare(settings(3.0, 50), sample_rate, 1).unwrap();
        assert_eq!(
            plan.target(degree(1)).transition_frames(),
            sample_rate.div_ceil(50)
        );
        for input_channels in 1..=8 {
            for output_channels in 1..=8 {
                let mut renderer =
                    SpatialRenderer::new(sample_rate, input_channels, output_channels).unwrap();
                renderer.retarget(plan.target(degree(27)));
                renderer.reset_to_target();
                assert_eq!(renderer.frame([0.0; 8]), ([0.0; 8], 0));
                let (actual, errors) = renderer.frame([f64::NAN; 8]);
                assert_eq!(errors, input_channels as u64);
                assert_eq!(actual, [0.0; 8]);
                let (actual, errors) = renderer.frame([f64::INFINITY; 8]);
                assert_eq!(errors, input_channels as u64);
                assert_eq!(actual, [0.0; 8]);
                let (actual, _) = renderer.frame([f64::MAX; 8]);
                assert!(actual.iter().all(|value| value.is_finite()));
            }
        }
    }
}

#[test]
fn callback_retarget_render_and_reset_have_no_heap_allocation_or_destruction() {
    let plan = prepare_plan(0.5, 25, 1);
    let enabled_left = plan.target(degree(270));
    let enabled_right = plan.target(degree(90));
    let stopped = SpatialPlan::prepare(SpatialSettings::default(), 48_000, 2).unwrap();
    let disabled = stopped.target(SpatialStep::ORIGIN);
    let wrong = SpatialPlan::prepare(settings(0.5, 25), 96_000, 3)
        .unwrap()
        .target(degree(1));
    let mut renderer = SpatialRenderer::new(48_000, 8, 1).unwrap();
    no_heap(|| {
        for iteration in 0..20_000 {
            assert!(renderer.retarget(if iteration % 2 == 0 {
                enabled_left
            } else {
                enabled_right
            }));
            let (output, errors) = renderer.frame([0.1; 8]);
            assert_eq!(errors, 0);
            std::hint::black_box(output);
        }
        assert!(!renderer.retarget(wrong));
        renderer.retarget(disabled);
        renderer.reset_to_target();
        std::hint::black_box(renderer.frame([0.25; 8]));
    });
}

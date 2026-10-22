use lx_gpui_player::{
    audio::{
        convolution::{ConvolutionStamp, ImpulseBuilder, PreparedConvolution},
        effects::{
            EFFECTS_MIN_REPLAY_FRAMES, EffectsAlignment, EffectsFrameKind, EffectsInput,
            EffectsPitch, EffectsPreparation, PreparedEffects,
        },
        effects_factory::{
            BankRequest, CountedBank, EffectsBankPool, EffectsRecipe, FactoryError, OwnedImpulse,
        },
        pcm::{PcmFormat, ProcessingStamp},
        spatial::{SpatialPlan, SpatialStep},
    },
    domain::audio_effects::{EffectsSettings, ImpulseResourceId},
};
use sha2::{Digest, Sha256};
use std::{
    sync::{
        Arc, Barrier,
        atomic::{AtomicBool, Ordering},
    },
    thread,
};

fn request() -> BankRequest {
    BankRequest {
        format: PcmFormat {
            sample_rate: 8_000,
            channels: 2,
        },
        device_channels: 2,
        token: 1,
        source_epoch: 2,
        replay_capacity: EFFECTS_MIN_REPLAY_FRAMES,
        initial_spatial: None,
    }
}
fn dry_recipe() -> Arc<EffectsRecipe> {
    EffectsRecipe::prepare(EffectsSettings::default(), 1, 2, false, None).unwrap()
}
fn impulse() -> Arc<OwnedImpulse> {
    let mut builder = ImpulseBuilder::prepare(8_000, 1, 3).unwrap();
    let samples = [1.0_f32, 0.0, 0.5];
    let mut hash = Sha256::new();
    for value in samples {
        builder.push_frame(&[value]).unwrap();
        hash.update(value.to_le_bytes());
    }
    OwnedImpulse::from_validated(
        builder.finish().unwrap(),
        ImpulseResourceId::Imported("fixture-ir".into()),
        format!("{:x}", hash.finalize()),
    )
    .unwrap()
}
fn wet_recipe(owner: Arc<OwnedImpulse>) -> Arc<EffectsRecipe> {
    let mut settings = EffectsSettings::default();
    settings.convolution.impulse = Some(owner.source_id().clone());
    settings.convolution.main_gain = 0.0;
    settings.convolution.send_gain = 1.0;
    EffectsRecipe::prepare(settings, 2, 2, false, Some(owner)).unwrap()
}
fn input(index: u64) -> EffectsInput {
    let stamp = ConvolutionStamp {
        token: 1,
        source_epoch: 2,
        media: ProcessingStamp {
            end_frame: index,
            ..Default::default()
        },
    };
    EffectsInput {
        samples: [
            if index == 1 { 0.1 } else { 0.0 },
            if index == 1 { -0.2 } else { 0.0 },
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
            ratio: 1.0,
            sequence: 1,
            transition_remaining_frames: 0,
        },
    }
}
fn render(bank: &mut CountedBank) -> Vec<[f64; 8]> {
    let mut values = Vec::new();
    for index in 1..=5 {
        let frame = bank.frame(Some(input(index))).unwrap();
        if frame.kind != EffectsFrameKind::Padding {
            values.push(frame.samples);
            bank.release_pending(&frame, true).unwrap();
        }
    }
    bank.begin_eof().unwrap();
    while let Some(frame) = bank.drain().unwrap() {
        if frame.kind != EffectsFrameKind::Padding {
            values.push(frame.samples);
            bank.release_pending(&frame, true).unwrap();
        }
    }
    assert!(bank.drained());
    values
}

#[test]
fn two_actual_slots_cover_both_preparation_and_boxes_until_real_drop() {
    let pool = EffectsBankPool::new().unwrap();
    let recipe = dry_recipe();
    let first = pool.begin(recipe.clone(), request()).unwrap();
    let second = pool.begin(recipe.clone(), request()).unwrap();
    assert_eq!(pool.occupied_banks(), 2);
    assert!(matches!(
        pool.begin(recipe.clone(), request()),
        Err(FactoryError::Busy)
    ));
    let first = first.prepare(&AtomicBool::new(false)).unwrap();
    assert_eq!(pool.occupied_banks(), 2);
    assert_eq!(first.graph().effect_channels(), 2);
    drop(second);
    assert_eq!(pool.occupied_banks(), 1);
    let second = pool
        .begin(recipe.clone(), request())
        .unwrap()
        .prepare(&AtomicBool::new(false))
        .unwrap();
    assert_eq!(pool.occupied_banks(), 2);
    let id = first.record().owner_id();
    assert_ne!(id, second.record().owner_id());
    drop(first);
    assert_eq!(pool.occupied_banks(), 1);
    drop(second);
    assert_eq!(pool.occupied_banks(), 0);
}

#[test]
fn cancellation_retains_slot_owned_worker_until_real_owner_drop_or_retry() {
    let pool = EffectsBankPool::new().unwrap();
    let recipe = dry_recipe();
    let owner = pool.begin(recipe.clone(), request()).unwrap();
    let cancel = AtomicBool::new(true);
    let failure = match owner.prepare(&cancel) {
        Ok(_) => panic!(),
        Err(failure) => failure,
    };
    assert_eq!(failure.error, FactoryError::Cancelled);
    assert!(!failure.owner.has_actual_bank());
    assert_eq!(pool.occupied_banks(), 1);
    let other = pool.begin(recipe, request()).unwrap();
    assert_eq!(pool.occupied_banks(), 2);
    cancel.store(false, Ordering::Release);
    let completed = failure.owner.prepare(&cancel).unwrap();
    assert_eq!(pool.occupied_banks(), 2);
    drop(other);
    assert_eq!(pool.occupied_banks(), 1);
    drop(completed);
    assert_eq!(pool.occupied_banks(), 0);
}

#[test]
fn missing_or_mismatched_ir_is_typed_unavailable_without_fallback() {
    let mut settings = EffectsSettings::default();
    settings.convolution.impulse = Some(ImpulseResourceId::Imported("fixture-ir".into()));
    assert!(matches!(
        EffectsRecipe::prepare(settings.clone(), 1, 2, false, None),
        Err(FactoryError::Unavailable)
    ));
    settings.convolution.impulse = Some(ImpulseResourceId::Imported("different-ir".into()));
    assert!(matches!(
        EffectsRecipe::prepare(settings, 1, 2, false, Some(impulse())),
        Err(FactoryError::InvalidRecipe)
    ));
    assert!(matches!(
        EffectsRecipe::prepare(EffectsSettings::default(), 1, 2, false, Some(impulse())),
        Err(FactoryError::InvalidRecipe)
    ));
}

#[test]
fn immutable_recipe_and_record_keep_actual_ir_and_bound_allocations_alive() {
    let pool = EffectsBankPool::new().unwrap();
    let owner = impulse();
    let weak = Arc::downgrade(&owner);
    let recipe = wet_recipe(owner.clone());
    assert_eq!(owner.actual_strong_owners(), 2);
    assert!(owner.metadata_owned_bytes() > 0);
    drop(owner);
    assert!(weak.upgrade().is_some());
    let bank = pool
        .begin(recipe.clone(), request())
        .unwrap()
        .prepare(&AtomicBool::new(false))
        .unwrap();
    let record = bank.record().clone();
    let bounds = record.bounds().clone();
    assert_eq!(record.recipe().validated_impulse().unwrap().frames(), 3);
    assert_eq!(bounds.old_format(), request().format);
    assert!(bounds.arc_reserved_bytes() > 0);
    assert!(bank.bank_owned_bytes() > bank.graph().owned_bytes());
    assert!(record.metadata_owned_bytes() > 0);
    assert!(recipe.metadata_owned_bytes() > 0);
    drop(recipe);
    drop(bank);
    assert_eq!(pool.occupied_banks(), 0);
    assert!(weak.upgrade().is_some());
    drop(record);
    assert!(weak.upgrade().is_none());
    // The independent actual bounds Arc survives record/Box retirement safely.
    assert_eq!(bounds.old_device_channels(), 2);
    assert!(bounds.arc_reserved_bytes() > 0);
}

#[test]
fn actual_ir_bank_matches_direct_original_dsp_preparation_numerically() {
    let pool = EffectsBankPool::new().unwrap();
    let recipe = wet_recipe(impulse());
    let mut bank = pool
        .begin(recipe.clone(), request())
        .unwrap()
        .prepare(&AtomicBool::new(false))
        .unwrap();
    let bank_output = render(&mut bank);
    assert_eq!(bank_output.len(), 7);
    assert!(bank_output[2][0].abs() > 0.0);
    assert!(
        bank_output
            .last()
            .unwrap()
            .iter()
            .all(|sample| sample.abs() < 1e-12)
    );
    let spatial =
        SpatialPlan::prepare(recipe.settings().spatial, 8_000, recipe.sequence()).unwrap();
    let convolution = PreparedConvolution::prepare(
        request().format,
        recipe.validated_impulse().unwrap(),
        recipe.normalize(),
        &AtomicBool::new(false),
    )
    .unwrap();
    let mut direct = PreparedEffects::prepare(
        EffectsPreparation {
            format: request().format,
            device_channels: 2,
            main_gain: 0.0,
            send_gain: 1.0,
            alignment: EffectsAlignment::Partition512,
            sequence: recipe.sequence(),
            token: 1,
            source_epoch: 2,
            initial_spatial: spatial.target(SpatialStep::ORIGIN),
            replay_capacity: EFFECTS_MIN_REPLAY_FRAMES,
        },
        Some(convolution),
        &AtomicBool::new(false),
    )
    .unwrap();
    let mut expected = Vec::new();
    for index in 1..=5 {
        let frame = direct.frame(Some(input(index))).unwrap();
        if frame.kind != EffectsFrameKind::Padding {
            expected.push(frame.samples);
            direct.release_pending(&frame, true).unwrap();
        }
    }
    direct.begin_eof().unwrap();
    while let Some(frame) = direct.drain().unwrap() {
        if frame.kind != EffectsFrameKind::Padding {
            expected.push(frame.samples);
            direct.release_pending(&frame, true).unwrap();
        }
    }
    assert_eq!(bank_output, expected);
    assert!(direct.drained());
}

#[test]
fn atomic_admission_under_concurrency_never_reserves_a_third_slot() {
    let pool = EffectsBankPool::new().unwrap();
    let recipe = dry_recipe();
    let acquired = Arc::new(Barrier::new(5));
    let release = Arc::new(Barrier::new(5));
    thread::scope(|scope| {
        for _ in 0..4 {
            let pool = pool.clone();
            let recipe = recipe.clone();
            let acquired = acquired.clone();
            let release = release.clone();
            scope.spawn(move || {
                let holder = pool.begin(recipe, request());
                acquired.wait();
                release.wait();
                drop(holder);
            });
        }
        acquired.wait();
        assert_eq!(pool.occupied_banks(), 2);
        release.wait();
    });
    assert_eq!(pool.occupied_banks(), 0);
}

#[test]
fn invalid_requests_and_invalid_representation_never_admit_a_bank() {
    let pool = EffectsBankPool::new().unwrap();
    let recipe = dry_recipe();
    let mut wrong = request();
    wrong.format.channels = 1;
    assert!(matches!(
        pool.begin(recipe.clone(), wrong),
        Err(FactoryError::InvalidRequest)
    ));
    wrong = request();
    wrong.source_epoch = 0;
    assert!(matches!(
        pool.begin(recipe, wrong),
        Err(FactoryError::InvalidRequest)
    ));
    assert_eq!(pool.occupied_banks(), 0);
    let mut builder = ImpulseBuilder::prepare(8_000, 1, 1).unwrap();
    builder.push_frame(&[1.0]).unwrap();
    assert!(matches!(
        OwnedImpulse::from_validated(
            builder.finish().unwrap(),
            ImpulseResourceId::Imported("fixture-ir".into()),
            "not-a-sha256".into()
        ),
        Err(FactoryError::InvalidRecipe)
    ));
}

#[test]
fn dropping_public_pool_handle_does_not_release_live_bank_slot_or_ir() {
    let pool = EffectsBankPool::new().unwrap();
    let observer = pool.clone();
    let mut bank = pool
        .begin(wet_recipe(impulse()), request())
        .unwrap()
        .prepare(&AtomicBool::new(false))
        .unwrap();
    drop(pool);
    assert_eq!(observer.occupied_banks(), 1);
    assert_eq!(render(&mut bank).len(), 7);
    assert_eq!(observer.occupied_banks(), 1);
    drop(bank);
    assert_eq!(observer.occupied_banks(), 0);
}

#[test]
fn shutdown_is_monotonic_across_clones_and_keeps_existing_holder_counted() {
    let pool = EffectsBankPool::new().unwrap();
    let clone = pool.clone();
    let recipe = dry_recipe();
    let holder = pool.begin(recipe.clone(), request()).unwrap();
    clone.begin_shutdown();
    assert!(pool.is_closed());
    assert!(clone.is_closed());
    assert!(matches!(
        pool.begin(recipe.clone(), request()),
        Err(FactoryError::Closed)
    ));
    assert!(matches!(
        clone.begin(recipe, request()),
        Err(FactoryError::Closed)
    ));
    assert_eq!(pool.occupied_banks(), 1);
    let failure = holder.prepare(&AtomicBool::new(false)).err().unwrap();
    assert_eq!(failure.error, FactoryError::Closed);
    assert_eq!(pool.occupied_banks(), 1);
    drop(failure);
    assert_eq!(pool.occupied_banks(), 0);
    pool.begin_shutdown();
    assert!(clone.is_closed());
}

#[test]
fn shutdown_does_not_release_an_existing_actual_box_before_drop() {
    let pool = EffectsBankPool::new().unwrap();
    let recipe = dry_recipe();
    let mut bank = pool
        .begin(recipe, request())
        .unwrap()
        .prepare(&AtomicBool::new(false))
        .unwrap();
    pool.begin_shutdown();
    assert_eq!(pool.occupied_banks(), 1);
    // The existing actual owner is still usable for retained/drain processing.
    assert_eq!(render(&mut bank).len(), 5);
    assert_eq!(pool.occupied_banks(), 1);
    drop(bank);
    assert_eq!(pool.occupied_banks(), 0);
}

#[test]
fn spatial_sequence_is_independent_of_immutable_ir_gain_sequence() {
    let pool = EffectsBankPool::new().unwrap();
    let recipe = dry_recipe();
    let spatial = SpatialPlan::prepare(recipe.settings().spatial, 8_000, 20).unwrap();
    let mut requested = request();
    requested.initial_spatial = Some(spatial.target(SpatialStep::ORIGIN));
    let mut bank = pool
        .begin(recipe, requested)
        .unwrap()
        .prepare(&AtomicBool::new(false))
        .unwrap();
    assert_eq!(bank.record().recipe().sequence(), 1);
    for index in 1..=49 {
        let frame = bank.frame(Some(input(index))).unwrap();
        if frame.kind != EffectsFrameKind::Padding {
            assert_eq!(frame.effect_sequence, 1);
            assert_eq!(frame.spatial_sequence, 20);
            bank.release_pending(&frame, true).unwrap();
        }
    }
}

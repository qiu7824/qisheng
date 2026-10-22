use lx_gpui_player::audio::{
    effect_exchange::effect_exchange,
    effect_pin::{PinScope, PinStatus},
    effects::{EFFECTS_MIN_REPLAY_FRAMES, EffectsAlignment, EffectsPreparation, PreparedEffects},
    effects_factory::{BankRequest, EffectsBankPool, EffectsRecipe},
    pcm::PcmFormat,
    spatial::{SpatialPlan, SpatialSettings, SpatialStep},
};
use std::sync::atomic::AtomicBool;
fn bank(sequence: u64, epoch: u64) -> Box<PreparedEffects> {
    let format = PcmFormat {
        sample_rate: 22050,
        channels: 2,
    };
    let plan =
        SpatialPlan::prepare(SpatialSettings::default(), format.sample_rate, sequence).unwrap();
    Box::new(
        PreparedEffects::prepare(
            EffectsPreparation {
                format,
                device_channels: 1,
                main_gain: 1.0,
                send_gain: 0.0,
                alignment: EffectsAlignment::Direct,
                sequence,
                token: 7,
                source_epoch: epoch,
                initial_spatial: plan.target(SpatialStep::ORIGIN),
                replay_capacity: EFFECTS_MIN_REPLAY_FRAMES,
            },
            None,
            &AtomicBool::new(false),
        )
        .unwrap(),
    )
}
#[test]
fn actual_bank_count_is_released_only_by_owner_drop_and_never_admits_a_third() {
    let (mut control, mut render) = effect_exchange().unwrap();
    assert!(control.request_sequence(1));
    assert!(control.install(bank(1, 3)).is_ok());
    render.apply(7, 3, 22050, 1);
    assert_eq!(control.owned_banks(), 1);
    assert!(control.request_sequence(2));
    assert!(control.install(bank(2, 3)).is_ok());
    assert_eq!(control.owned_banks(), 2);
    assert!(control.install(bank(2, 3)).is_err());
    render.apply(7, 3, 22050, 1);
    assert_eq!(render.active().unwrap().sequence(), 2);
    assert_eq!(control.owned_banks(), 2);
    assert_eq!(control.reclaim(), 1);
    assert_eq!(control.owned_banks(), 1);
    control.begin_shutdown();
    render.apply(7, 3, 22050, 1);
    assert!(render.active().is_none());
    assert_eq!(control.owned_banks(), 1);
    assert_eq!(control.reclaim(), 1);
    assert_eq!(control.owned_banks(), 0);
    render.close_on_owner();
    assert!(!control.request_sequence(4));
    assert!(control.install(bank(3, 3)).is_err());
}
#[test]
fn stale_sequence_and_actual_epoch_are_rejected_without_installing_a_candidate() {
    let (mut control, mut render) = effect_exchange().unwrap();
    assert!(control.request_sequence(3));
    assert!(control.install(bank(2, 3)).is_err());
    assert!(control.install(bank(3, 3)).is_ok());
    render.apply(7, 4, 22050, 1);
    assert!(render.active().is_none());
    assert_eq!(control.owned_banks(), 1);
    control.reclaim();
    assert_eq!(control.owned_banks(), 0);
    assert!(!control.request_sequence(2));
    render.close_on_owner();
}

#[test]
fn a_real_active_pin_freezes_replacement_and_returns_unselected_owners() {
    let (mut control, mut render) = effect_exchange().unwrap();
    assert!(control.request_sequence(1));
    assert!(control.install(bank(1, 3)).is_ok());
    render.apply(7, 3, 22050, 1);
    let actual_owner = control.active_owner();
    assert_ne!(actual_owner, 0);
    let scope = PinScope {
        bank_owner: actual_owner,
        media_token: 7,
        source_epoch: 3,
        sample_rate: 22050,
        device_channels: 1,
    };
    let pin = control.pin_owner().request(scope).unwrap();
    assert!(control.request_sequence(2));
    assert!(control.install(bank(2, 3)).is_ok());
    render.apply(7, 3, 22050, 1);
    assert!(pin.is_pinned_for(scope));
    assert_eq!(render.active().unwrap().sequence(), 1);
    assert_eq!(control.active_owner(), actual_owner);
    assert_eq!(
        control.owned_banks(),
        2,
        "Returning a candidate cannot invent its destruction"
    );
    assert_eq!(control.reclaim(), 1);
    assert_eq!(control.owned_banks(), 1);
    pin.cancel();
    render.apply(7, 3, 22050, 1);
    assert_eq!(pin.status(), PinStatus::Released);
    assert!(control.install(bank(2, 3)).is_ok());
    render.apply(7, 3, 22050, 1);
    assert_ne!(control.active_owner(), actual_owner);
    assert_eq!(render.active().unwrap().sequence(), 2);
    control.reclaim();
    render.close_on_owner();
    assert_eq!(pin.status(), PinStatus::Closed);
}

#[test]
fn hard_stream_change_rejects_a_pin_instead_of_freezing_a_foreign_bank() {
    let (mut control, mut render) = effect_exchange().unwrap();
    assert!(control.request_sequence(1));
    assert!(control.install(bank(1, 3)).is_ok());
    render.apply(7, 3, 22050, 1);
    let pin = control
        .pin_owner()
        .request(PinScope {
            bank_owner: control.active_owner(),
            media_token: 7,
            source_epoch: 3,
            sample_rate: 22050,
            device_channels: 1,
        })
        .unwrap();
    render.apply(7, 3, 22050, 1);
    assert_eq!(pin.status(), PinStatus::Pinned);
    render.apply(7, 4, 22050, 1);
    assert_eq!(pin.status(), PinStatus::Rejected);
    assert_eq!(control.active_owner(), 0);
    assert!(render.active().is_none());
    assert_eq!(control.reclaim(), 1);
    render.close_on_owner();
}

fn counted(pool: &EffectsBankPool) -> lx_gpui_player::audio::effects_factory::CountedBank {
    let recipe = EffectsRecipe::prepare(Default::default(), 1, 2, true, None).unwrap();
    pool.begin(
        recipe,
        BankRequest {
            format: PcmFormat {
                sample_rate: 22050,
                channels: 2,
            },
            device_channels: 1,
            token: 7,
            source_epoch: 3,
            replay_capacity: EFFECTS_MIN_REPLAY_FRAMES,
            initial_spatial: None,
        },
    )
    .unwrap()
    .prepare(&AtomicBool::new(false))
    .unwrap()
}
#[test]
fn a_counted_preparation_and_actual_callback_owner_share_one_pool_and_record_lifetime() {
    let (mut control, mut render) = effect_exchange().unwrap();
    let pool = control.bank_pool();
    let actual = counted(&pool);
    let owner_id = actual.record().owner_id();
    let weak = std::sync::Arc::downgrade(actual.record());
    assert_eq!(
        control.owned_banks(),
        1,
        "Prepared but uninstalled bank is already admitted"
    );
    assert!(control.request_sequence(1));
    assert!(control.install_counted(actual).is_ok());
    render.apply(7, 3, 22050, 1);
    assert_eq!(control.active_owner(), owner_id);
    let record = control.record_for_owner(owner_id).unwrap();
    assert_eq!(record.recipe().effect_channels(), 2);
    drop(record);
    control.begin_shutdown();
    render.apply(7, 3, 22050, 1);
    assert_eq!(
        pool.occupied_banks(),
        1,
        "Returned queue still owns the actual bank"
    );
    assert!(weak.upgrade().is_some());
    assert_eq!(control.reclaim(), 1);
    assert_eq!(pool.occupied_banks(), 0);
    assert!(
        weak.upgrade().is_none(),
        "Actual destruction must retire its bound registry entry"
    );
    render.close_on_owner();
}
#[test]
fn foreign_pool_rejection_returns_the_same_owned_bank_until_real_drop() {
    let (mut control, render) = effect_exchange().unwrap();
    let foreign = EffectsBankPool::new().unwrap();
    let actual = counted(&foreign);
    let owner_id = actual.record().owner_id();
    assert!(control.request_sequence(1));
    let returned = match control.install_counted(actual) {
        Ok(()) => panic!("Foreign admission pool was accepted"),
        Err(actual) => actual,
    };
    assert_eq!(returned.record().owner_id(), owner_id);
    assert_eq!(foreign.occupied_banks(), 1);
    assert_eq!(control.owned_banks(), 0);
    drop(returned);
    assert_eq!(foreign.occupied_banks(), 0);
    render.close_on_owner();
}

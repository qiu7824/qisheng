use lx_gpui_player::{
    audio::{effect_admission::EffectAdmission, pcm::PcmFormat},
    domain::audio_effects::EffectsSettings,
};
fn format() -> PcmFormat {
    PcmFormat {
        sample_rate: 48000,
        channels: 2,
    }
}
#[test]
fn later_request_cancels_but_cannot_reuse_candidate_storage_until_actual_drop() {
    let mut owner = EffectAdmission::default();
    owner.request(EffectsSettings::default()).unwrap();
    let old = owner.begin_preparation(2, format()).unwrap();
    owner.request(EffectsSettings::default()).unwrap();
    assert!(old.cancelled());
    assert!(!owner.can_install(&old, 2, format()));
    assert!(owner.begin_preparation(2, format()).is_err());
    owner.preparation_dropped(&old).unwrap();
    let current = owner.begin_preparation(2, format()).unwrap();
    owner.installed(&current, 2, format()).unwrap();
    assert_eq!(owner.active_sequence(), Some(current.sequence()));
    owner.begin_shutdown();
    assert!(!owner.is_drained());
    owner.active_dropped(current.sequence()).unwrap();
    assert!(owner.is_drained());
}
#[test]
fn old_bank_retirement_blocks_a_third_bank_and_late_epoch_cannot_install() {
    let mut owner = EffectAdmission::default();
    owner.request(EffectsSettings::default()).unwrap();
    let first = owner.begin_preparation(1, format()).unwrap();
    owner.installed(&first, 1, format()).unwrap();
    owner.request(EffectsSettings::default()).unwrap();
    let second = owner.begin_preparation(1, format()).unwrap();
    assert!(!owner.can_install(&second, 2, format()));
    owner.installed(&second, 1, format()).unwrap();
    owner.request(EffectsSettings::default()).unwrap();
    assert!(owner.begin_preparation(2, format()).is_err());
    owner.retired_dropped(first.sequence()).unwrap();
    assert!(owner.begin_preparation(2, format()).is_ok());
    owner.begin_shutdown();
    assert!(owner.request(EffectsSettings::default()).is_err());
}

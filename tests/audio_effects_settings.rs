use lx_gpui_player::domain::audio_effects::{
    ConvolutionSettings, EffectsSettings, ImpulseResourceId,
};
use serde_json::json;
#[test]
fn old_empty_preferences_are_inert_and_slider_units_roundtrip() {
    let settings: EffectsSettings = serde_json::from_value(json!({})).unwrap();
    assert!(settings.convolution.impulse.is_none());
    assert!(!settings.spatial.enabled);
    let convolution = ConvolutionSettings::from_lx_slider(
        Some(ImpulseResourceId::Bundled("small_room".into())),
        15.0,
        2.0,
    )
    .unwrap();
    assert_eq!(convolution.main_gain, 1.5);
    assert_eq!(convolution.send_gain, 0.2);
    assert_eq!(convolution.lx_slider_gains().unwrap(), (15.0, 2.0));
}
#[test]
fn imported_metadata_cannot_become_a_path_or_unsupported_gain() {
    for id in [
        "../outside",
        "C:\\secret.wav",
        "resource/file",
        "https://example.com",
        "",
        "space name",
    ] {
        assert!(ImpulseResourceId::Imported(id.into()).validate().is_err());
    }
    for value in [
        json!({"main_gain":6.0}),
        json!({"send_gain":-0.01}),
        json!({"path":"C:/music.wav"}),
        json!({"impulse":{"kind":"imported","id":"../outside"}}),
    ] {
        assert!(serde_json::from_value::<ConvolutionSettings>(value).is_err());
    }
    assert!(ConvolutionSettings::from_lx_slider(None, f64::INFINITY, 0.0).is_err());
    assert!(ConvolutionSettings::from_lx_slider(None, 10.0, f64::NAN).is_err());
    assert!(serde_json::from_value::<EffectsSettings>(json!({"spatial":{"radius":0.0}})).is_err());
}

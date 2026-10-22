#[path = "support/playback_processing_fixture.rs"]
#[allow(dead_code)]
mod fixture;
use lx_gpui_player::{
    audio::spatial::SpatialSettings,
    backend::{BackendCommand, BackendHandle, BackendQuery, BackendQueryResult},
    domain::audio_effects::SpatialRuntimeState,
    store::ProjectStore,
};
use std::{
    thread,
    time::{Duration, Instant},
};
fn state(backend: &BackendHandle) -> SpatialRuntimeState {
    match backend.query(BackendQuery::SpatialState).unwrap() {
        BackendQueryResult::SpatialState(state) => state,
        other => panic!("Spatial response {other:?}"),
    }
}
#[test]
fn real_pcm_spatial_configuration_is_persisted_but_applied_only_after_presentation() {
    let running = fixture::Running::new(22050, 2, 8.0);
    assert!(!state(&running.backend).desired.enabled);
    let settings = SpatialSettings {
        enabled: true,
        radius: 0.7,
        speed: 25,
    };
    running.execute(
        "spatial-enable",
        BackendCommand::SetSpatialSettings { settings },
    );
    assert_eq!(state(&running.backend).applied_sequence, 0);
    running.play("spatial-real-play");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let actual = state(&running.backend);
        if actual.applied_sequence == actual.requested_sequence {
            assert!(actual.error.is_none(), "{actual:?}");
            break;
        }
        assert!(Instant::now() < deadline, "Spatial not applied: {actual:?}");
        thread::sleep(Duration::from_millis(10));
    }
    let BackendQueryResult::RenderedPcm(Some(pcm)) =
        running.backend.query(BackendQuery::RenderedPcm).unwrap()
    else {
        panic!("PCM")
    };
    assert!(pcm.peak > 0.01);
    let before = running.backend.snapshot().playback.generation;
    running.execute(
        "spatial-disable",
        BackendCommand::SetSpatialSettings {
            settings: SpatialSettings {
                enabled: false,
                ..settings
            },
        },
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    while state(&running.backend).applied_sequence != state(&running.backend).requested_sequence {
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(running.backend.snapshot().playback.generation, before);
    running.backend.shutdown(Duration::from_secs(5)).unwrap();
    let stored = ProjectStore::open(running.profile())
        .unwrap()
        .load()
        .unwrap();
    assert_eq!(
        stored.spatial,
        SpatialSettings {
            enabled: false,
            ..settings
        }
    );
}
#[test]
fn invalid_spatial_request_preserves_user_revision_and_durable_preferences() {
    let running = fixture::Running::new(22050, 2, 2.0);
    let before = running.backend.snapshot().revision;
    let desired = state(&running.backend).desired;
    assert!(
        running
            .backend
            .execute(fixture::envelope(
                "spatial-invalid",
                BackendCommand::SetSpatialSettings {
                    settings: SpatialSettings {
                        enabled: true,
                        radius: f64::NAN,
                        speed: 25
                    }
                }
            ))
            .is_err()
    );
    assert_eq!(running.backend.snapshot().revision, before);
    assert_eq!(state(&running.backend).desired, desired);
}

use lx_gpui_player::system_control::*;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
use std::time::{Duration, Instant};

#[test]
fn system_settings_require_explicit_start_and_do_not_capture_media_keys_twice() {
    let mut settings = SystemSettings::default();
    assert!(!settings.enabled);
    assert!(!settings.global_hotkeys_enabled);
    settings.validate().unwrap();
    settings.hotkeys.push(settings.hotkeys[0].clone());
    assert!(settings.validate().is_err());
    settings.hotkeys.pop();
    settings.hotkeys.push(HotkeyBinding {
        chord: HotkeyChord {
            control: false,
            alt: false,
            shift: false,
            windows: false,
            virtual_key: 0xb3,
        },
        action: SystemAction::Toggle,
    });
    assert!(settings.validate().is_err());
    settings.media_session_enabled = false;
    settings.validate().unwrap();
}
#[test]
fn system_projection_and_seek_fences_reject_invalid_clock_or_old_generation() {
    let mut value = SystemProjection {
        position_ms: 1,
        ..Default::default()
    };
    assert!(value.validate().is_err());
    value.duration_ms = 10;
    value.playback_rate = f64::NAN;
    assert!(value.validate().is_err());
    let input = SystemInput {
        owner_epoch: 1,
        sequence: 1,
        generation: None,
        action: SystemAction::SeekRelative { offset_ms: 5000 },
    };
    assert!(input.validate().is_err());
    SystemInput {
        generation: Some(2),
        ..input
    }
    .validate()
    .unwrap();
}
struct Backend {
    entered: AtomicU64,
    release: AtomicBool,
    generation: AtomicU64,
    accepted: Mutex<Vec<SystemInput>>,
}
impl SystemBackend for Backend {
    fn dispatch_system_input(
        &self,
        input: SystemInput,
        fence: SystemInputFence,
    ) -> anyhow::Result<()> {
        self.entered.fetch_add(1, Ordering::AcqRel);
        while !self.release.load(Ordering::Acquire) && !fence.is_closing() {
            std::thread::sleep(Duration::from_millis(1));
        }
        fence.validate(input)?;
        anyhow::ensure!(
            input
                .generation
                .is_none_or(|g| g == self.generation.load(Ordering::Acquire)),
            "old generation"
        );
        self.accepted.lock().unwrap().push(input);
        Ok(())
    }
}
#[test]
fn dispatcher_bounds_actual_pending_inputs_and_joins_after_cancellation() {
    let backend = Arc::new(Backend {
        entered: AtomicU64::new(0),
        release: AtomicBool::new(false),
        generation: AtomicU64::new(2),
        accepted: Mutex::new(Vec::new()),
    });
    let mut dispatcher = SystemDispatcher::start(7, backend.clone()).unwrap();
    let input = dispatcher.sender();
    input.try_submit(SystemAction::Toggle, None).unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    while backend.entered.load(Ordering::Acquire) == 0 {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(1));
    }
    for _ in 0..MAX_SYSTEM_INPUTS {
        input.try_submit(SystemAction::Toggle, None).unwrap();
    }
    assert!(input.try_submit(SystemAction::Toggle, None).is_err());
    assert!(dispatcher.shutdown(Duration::from_secs(2)).unwrap());
    assert!(backend.accepted.lock().unwrap().is_empty());
    assert!(input.try_submit(SystemAction::Toggle, None).is_err());
}
#[test]
fn dispatcher_passes_the_original_seek_generation_to_the_final_backend_guard() {
    let backend = Arc::new(Backend {
        entered: AtomicU64::new(0),
        release: AtomicBool::new(true),
        generation: AtomicU64::new(2),
        accepted: Mutex::new(Vec::new()),
    });
    let mut dispatcher = SystemDispatcher::start(8, backend.clone()).unwrap();
    dispatcher
        .sender()
        .try_submit(SystemAction::SeekAbsolute { position_ms: 10 }, Some(1))
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    while dispatcher.last_error().is_none() {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(backend.accepted.lock().unwrap().is_empty());
    assert!(dispatcher.last_error().unwrap().contains("old generation"));
    assert!(dispatcher.shutdown(Duration::from_secs(2)).unwrap());
}

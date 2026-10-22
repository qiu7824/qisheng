#![cfg(windows)]
use lx_gpui_player::system_control::*;
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
struct Backend;
impl SystemBackend for Backend {
    fn dispatch_system_input(
        &self,
        input: SystemInput,
        fence: SystemInputFence,
    ) -> anyhow::Result<()> {
        fence.validate(input)?;
        Ok(())
    }
}

#[test]
#[ignore = "Requires explicit native desktop media-session acceptance"]
fn actual_hidden_hwnd_media_session_owner_unregisters_and_joins() {
    let mut dispatcher = SystemDispatcher::start(101, Arc::new(Backend)).unwrap();
    let mut owner = WindowsSystemOwner::start(
        SystemSettings {
            enabled: true,
            global_hotkeys_enabled: false,
            power_blocker_enabled: false,
            ..Default::default()
        },
        SystemProjection::default(),
        dispatcher.sender(),
    )
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let state = owner.state();
        assert!(state.last_error.is_none(), "{state:?}");
        if state.ready {
            assert_eq!(state.registered_hotkeys, 0);
            assert!(state.media_session_active);
            break;
        }
        assert!(Instant::now() < deadline, "{state:?}");
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(owner.shutdown(Duration::from_secs(5)).unwrap());
    let final_state = owner.state();
    assert!(final_state.thread_exited);
    assert_eq!(final_state.registered_hotkeys, 0);
    assert!(!final_state.media_session_active);
    assert!(!final_state.power_request_active);
    assert!(dispatcher.shutdown(Duration::from_secs(5)).unwrap());
}

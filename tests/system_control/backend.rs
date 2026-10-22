use lx_gpui_player::{
    backend::{
        BackendCommand, BackendConfig, BackendEvent, BackendHandle, BackendQuery,
        BackendQueryResult, CommandEnvelope, ErrorKind, ReceiptState,
    },
    store::ProjectStore,
    system_control::{SystemControlView, SystemSettings},
};
use std::{
    fs, thread,
    time::{Duration, Instant},
};
fn profile() -> tempfile::TempDir {
    let base = std::env::temp_dir().join("lx-native-system-control-tests");
    fs::create_dir_all(&base).unwrap();
    tempfile::Builder::new()
        .prefix("profile-")
        .tempdir_in(base)
        .unwrap()
}
fn execute(
    backend: &BackendHandle,
    id: &str,
    settings: SystemSettings,
) -> lx_gpui_player::backend::CommandReceipt {
    backend
        .execute(CommandEnvelope {
            request_id: id.into(),
            origin: "system_acceptance".into(),
            expected_revision: None,
            command: BackendCommand::SetSystemControlSettings { settings },
        })
        .unwrap()
}
fn state(backend: &BackendHandle) -> SystemControlView {
    let BackendQueryResult::SystemControlState(view) =
        backend.query(BackendQuery::SystemControlState).unwrap()
    else {
        panic!("wrong system state")
    };
    view
}
fn settings(backend: &BackendHandle) -> SystemSettings {
    let BackendQueryResult::SystemControlSettings(settings) =
        backend.query(BackendQuery::SystemControlSettings).unwrap()
    else {
        panic!("wrong system preferences")
    };
    settings
}
fn custom_disabled() -> SystemSettings {
    SystemSettings {
        enabled: false,
        media_session_enabled: false,
        global_hotkeys_enabled: false,
        power_blocker_enabled: false,
        hotkeys: Vec::new(),
    }
}
#[test]
fn legacy_profile_defaults_system_controls_off_without_rewriting_or_starting_os_services() {
    let directory = profile();
    let path = directory.path().join("player-state.json");
    let legacy =
        br#"{"version":1,"data":{"sources":[],"selected":null,"local_files":[],"volume":0.51}}"#;
    fs::write(&path, legacy).unwrap();
    let data = ProjectStore::open(directory.path().into())
        .unwrap()
        .load()
        .unwrap();
    assert_eq!(data.system_control, SystemSettings::default());
    assert!(!data.system_control.enabled);
    assert!(!data.system_control.global_hotkeys_enabled);
    assert_eq!(fs::read(&path).unwrap(), legacy);
    let backend = BackendHandle::start(BackendConfig::headless(directory.path().into())).unwrap();
    assert_eq!(settings(&backend), SystemSettings::default());
    let view = state(&backend);
    assert!(view.owner_epoch.is_none());
    assert!(!view.native_ready && !view.media_session_active && !view.power_request_active);
    assert_eq!(view.registered_hotkeys, 0);
    assert_eq!(fs::read(&path).unwrap(), legacy);
    backend.shutdown(Duration::from_secs(15)).unwrap();
    assert_eq!(fs::read(&path).unwrap(), legacy);
}
#[test]
fn disabled_nondefault_preferences_survive_public_save_query_shutdown_and_restart() {
    let directory = profile();
    let backend = BackendHandle::start(BackendConfig::headless(directory.path().into())).unwrap();
    let desired = custom_disabled();
    assert_ne!(desired, SystemSettings::default());
    assert_eq!(
        execute(&backend, "save-custom-disabled", desired.clone()).state,
        ReceiptState::Committed
    );
    assert_eq!(settings(&backend), desired);
    let view = state(&backend);
    assert!(view.owner_epoch.is_none());
    assert!(!view.native_ready && !view.media_session_active && !view.power_request_active);
    assert_eq!(view.registered_hotkeys, 0);
    assert!(!view.store_recovery_pending);
    backend.shutdown(Duration::from_secs(15)).unwrap();
    let saved = ProjectStore::open(directory.path().into())
        .unwrap()
        .load()
        .unwrap();
    assert_eq!(saved.system_control, desired);
    let restarted = BackendHandle::start(BackendConfig::headless(directory.path().into())).unwrap();
    assert_eq!(settings(&restarted), desired);
    assert!(state(&restarted).owner_epoch.is_none());
    restarted.shutdown(Duration::from_secs(15)).unwrap();
}
#[test]
fn invalid_hotkeys_are_rejected_before_any_durable_or_native_change() {
    let directory = profile();
    let backend = BackendHandle::start(BackendConfig::headless(directory.path().into())).unwrap();
    let prior = custom_disabled();
    execute(&backend, "invalid-baseline", prior.clone());
    let before = fs::read(directory.path().join("player-state.json")).unwrap();
    let mut bad = SystemSettings::default();
    bad.hotkeys.push(bad.hotkeys[0].clone());
    let error = backend
        .execute(CommandEnvelope {
            request_id: "invalid-duplicate-hotkey".into(),
            origin: "system_acceptance".into(),
            expected_revision: None,
            command: BackendCommand::SetSystemControlSettings { settings: bad },
        })
        .unwrap_err();
    assert_eq!(error.kind, ErrorKind::InvalidData);
    assert_eq!(settings(&backend), prior);
    assert_eq!(
        fs::read(directory.path().join("player-state.json")).unwrap(),
        before
    );
    assert!(state(&backend).owner_epoch.is_none());
    backend.shutdown(Duration::from_secs(15)).unwrap();
}
#[test]
fn a_failed_disabled_store_commit_retains_a_real_ticket_until_profile_restoration() {
    let directory = profile();
    let backend = BackendHandle::start(BackendConfig::headless(directory.path().into())).unwrap();
    let prior = custom_disabled();
    execute(&backend, "store-fault-baseline", prior.clone());
    let state_path = directory.path().join("player-state.json");
    let backup_path = directory.path().join("player-state.json.bak");
    let before = fs::read(&state_path).unwrap();
    fs::create_dir(&backup_path).unwrap();
    let requested = SystemSettings {
        media_session_enabled: true,
        ..prior.clone()
    };
    let cursor = backend.event_cursor();
    let receipt = execute(&backend, "store-fault-pending", requested);
    assert_eq!(receipt.state, ReceiptState::Accepted);
    let pending = state(&backend);
    assert!(pending.store_recovery_pending);
    assert_eq!(
        pending.store_recovery_operation.as_deref(),
        Some("store-fault-pending")
    );
    assert!(pending.owner_epoch.is_none());
    assert_eq!(settings(&backend), prior);
    assert_eq!(fs::read(&state_path).unwrap(), before);
    let error = backend
        .execute(CommandEnvelope {
            request_id: "frozen-other-store-mutation".into(),
            origin: "system_acceptance".into(),
            expected_revision: None,
            command: BackendCommand::Volume { volume: 0.19 },
        })
        .unwrap_err();
    assert_eq!(error.kind, ErrorKind::Storage);
    assert!(
        backend
            .events_after(cursor)
            .unwrap()
            .iter()
            .all(|event| !matches!(&event.payload,
        BackendEvent::OperationFinished {operation_id,..} if operation_id=="store-fault-pending"))
    );
    fs::remove_dir(&backup_path).unwrap();
    backend
        .execute(CommandEnvelope {
            request_id: "retry-store-restoration".into(),
            origin: "system_acceptance".into(),
            expected_revision: None,
            command: BackendCommand::RetrySystemControl,
        })
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some((success, error)) =
            backend
                .events_after(cursor)
                .unwrap()
                .iter()
                .find_map(|event| match &event.payload {
                    BackendEvent::OperationFinished {
                        operation_id,
                        success,
                        error,
                    } if operation_id == "store-fault-pending" => Some((*success, error.clone())),
                    _ => None,
                })
        {
            assert!(!success);
            assert_eq!(error.unwrap().kind, ErrorKind::Storage);
            break;
        }
        assert!(Instant::now() < deadline, "{:?}", state(&backend));
        thread::sleep(Duration::from_millis(10));
    }
    let recovered = state(&backend);
    assert!(!recovered.store_recovery_pending);
    assert!(recovered.store_recovery_operation.is_none());
    assert_eq!(settings(&backend), prior);
    assert_eq!(
        ProjectStore::open(directory.path().into())
            .unwrap()
            .load()
            .unwrap()
            .system_control,
        prior
    );
    backend.shutdown(Duration::from_secs(15)).unwrap();
}

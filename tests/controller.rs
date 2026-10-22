//! Actual controller, isolated storage and the actual bounded source-worker binary.
//! No snapshots are injected and no audio output device is opened by these checks.

use lx_gpui_player::{
    controller::ControllerHandle,
    source::{CURRENT_APPROVAL_POLICY_VERSION, import_script},
    store::{ProjectStore, StoreData},
    ui_data::{Page, UiCommand, UiSnapshot},
};
use std::{
    io::Read,
    net::TcpListener,
    sync::{Arc, mpsc},
    thread,
    time::{Duration, Instant},
};

fn wait_for(
    controller: &ControllerHandle,
    phase: &str,
    ready: impl Fn(&UiSnapshot) -> bool,
) -> UiSnapshot {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let snapshot = controller.snapshot.lock().unwrap().clone();
        assert!(
            snapshot.error.is_none(),
            "{phase}: actual controller error: {:?}",
            snapshot.error
        );
        if ready(&snapshot) {
            return snapshot;
        }
        assert!(
            Instant::now() < deadline,
            "{phase}: actual state did not converge: {snapshot:?}"
        );
        thread::sleep(Duration::from_millis(20));
    }
}

fn import_unapproved(controller: &ControllerHandle) -> String {
    // This is the existing controller's import of the project's original legal fixture.
    controller.commands.send(UiCommand::AddDemoSource).unwrap();
    let imported = wait_for(controller, "static fixture import", |snapshot| {
        snapshot.sources.len() == 1 && !snapshot.busy
    });
    let source = &imported.sources[0];
    assert!(
        !source.approved && !source.enabled,
        "Import must not approve or activate its script"
    );
    source.id.clone()
}

fn wait_enabled(controller: &ControllerHandle, id: &str) {
    let enabled = wait_for(controller, "actual worker initialization", |snapshot| {
        !snapshot.busy
            && snapshot.sources.iter().any(|source| {
                source.id == id
                    && source.approved
                    && source.enabled
                    && source.status.contains("local")
            })
    });
    let source = enabled
        .sources
        .iter()
        .find(|source| source.id == id)
        .unwrap();
    assert!(
        source.status.contains("local"),
        "Successful initialization must expose the real local manifest: {source:?}"
    );
    assert!(!enabled.playing && enabled.now_playing.is_none());
}

fn shutdown(controller: ControllerHandle) {
    // Drop both public handles, then observe the actual controller's exit.
    let old_snapshot = Arc::downgrade(&controller.snapshot);
    drop(controller);
    let deadline = Instant::now() + Duration::from_secs(5);
    while old_snapshot.strong_count() != 0 {
        assert!(
            Instant::now() < deadline,
            "Old controller did not exit after all public handles were dropped"
        );
        thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn actual_controller_import_approve_disable_reenable_and_delete_persist_correctly() {
    let directory = tempfile::tempdir().unwrap();
    let controller = ControllerHandle::start(directory.path().into()).unwrap();
    let store = ProjectStore::open(directory.path().into()).unwrap();
    let id = import_unapproved(&controller);
    let imported = store.load().unwrap();
    assert_eq!(imported.sources.len(), 1);
    assert!(!imported.sources[0].approved);
    assert!(imported.selected.is_none());

    controller
        .commands
        .send(UiCommand::ApproveSource(id.clone()))
        .unwrap();
    wait_enabled(&controller, &id);
    let approved = store.load().unwrap();
    assert!(approved.sources[0].approved);
    assert_eq!(
        approved.sources[0].approval_policy_version,
        CURRENT_APPROVAL_POLICY_VERSION
    );
    approved.sources[0].require_approval().unwrap();
    assert_eq!(approved.selected.as_deref(), Some(id.as_str()));

    controller.commands.send(UiCommand::DisableSource).unwrap();
    let disabled = wait_for(&controller, "source disable", |snapshot| {
        !snapshot.busy && snapshot.sources.len() == 1 && !snapshot.sources[0].enabled
    });
    assert!(disabled.sources[0].approved);
    assert!(store.load().unwrap().selected.is_none());

    controller
        .commands
        .send(UiCommand::EnableSource(id.clone()))
        .unwrap();
    wait_enabled(&controller, &id);
    assert_eq!(store.load().unwrap().selected.as_deref(), Some(id.as_str()));

    controller
        .commands
        .send(UiCommand::DeleteSource(id))
        .unwrap();
    let deleted = wait_for(&controller, "delete active source", |snapshot| {
        snapshot.sources.is_empty() && !snapshot.busy
    });
    assert!(!deleted.playing && deleted.now_playing.is_none());
    let deleted = store.load().unwrap();
    assert!(deleted.sources.is_empty() && deleted.selected.is_none());
}

#[test]
fn actual_controller_restores_selected_approval_lazily_and_starts_a_fresh_worker_on_play() {
    let directory = tempfile::tempdir().unwrap();
    let store = ProjectStore::open(directory.path().into()).unwrap();
    let script = r#"/* @name Lazy controller source fixture */
        let counter=0;
        lx.on(lx.EVENT_NAMES.request,async()=> 'http://127.0.0.1/fixture-counter-'+(++counter)+'.wav');
        lx.send(lx.EVENT_NAMES.inited,{sources:{local:{type:'music',actions:['musicUrl'],qualitys:[]}}});
    "#;
    let mut source = import_script(script).unwrap();
    source.approve().unwrap();
    let id = source.sha256.clone();
    store
        .save(&StoreData {
            sources: vec![source],
            selected: Some(id.clone()),
            ..StoreData::default()
        })
        .unwrap();
    let controller = ControllerHandle::start(directory.path().into()).unwrap();
    let lazy = wait_for(
        &controller,
        "selected approval restored without execution",
        |snapshot| {
            !snapshot.busy
                && snapshot.sources.iter().any(|source| {
                    source.id == id
                        && source.approved
                        && source.enabled
                        && source.status.contains("待按需初始化")
                })
        },
    );
    assert!(!lazy.playing && lazy.pending_playback_url.is_none());
    let track = lazy.tracks[0].id.clone();
    controller
        .commands
        .send(UiCommand::Play(track.clone()))
        .unwrap();
    let first = wait_for(&controller, "first lazy worker resolution", |snapshot| {
        !snapshot.busy
            && snapshot
                .pending_playback_url
                .as_deref()
                .is_some_and(|url| url.ends_with("fixture-counter-1.wav"))
    });
    assert!(!first.playing);
    controller.commands.send(UiCommand::Next).unwrap();
    let second = wait_for(
        &controller,
        "same enabled worker closure state",
        |snapshot| {
            !snapshot.busy
                && snapshot
                    .pending_playback_url
                    .as_deref()
                    .is_some_and(|url| url.ends_with("fixture-counter-2.wav"))
        },
    );
    assert!(!second.playing);
    let before = store.load().unwrap();
    assert_eq!(before.selected.as_deref(), Some(id.as_str()));
    assert!(before.sources[0].approved);

    shutdown(controller);

    let restarted = ControllerHandle::start(directory.path().into()).unwrap();
    let lazy = wait_for(
        &restarted,
        "restart restores paused intent without running source",
        |snapshot| {
            !snapshot.busy
                && !snapshot.playing
                && snapshot.pending_playback_url.is_none()
                && snapshot.sources.iter().any(|source| {
                    source.id == id
                        && source.approved
                        && source.enabled
                        && source.status.contains("待按需初始化")
                })
        },
    );
    assert_eq!(lazy.status, "已暂停");
    restarted.commands.send(UiCommand::Toggle).unwrap();
    let resumed = wait_for(
        &restarted,
        "explicit resume initializes a fresh worker closure",
        |snapshot| {
            !snapshot.busy
                && snapshot
                    .pending_playback_url
                    .as_deref()
                    .is_some_and(|url| url.ends_with("fixture-counter-1.wav"))
        },
    );
    assert!(!resumed.playing);
    let restored = store.load().unwrap();
    assert_eq!(restored.selected.as_deref(), Some(id.as_str()));
    assert!(restored.sources[0].approved);
    assert_eq!(
        restored.sources[0].approval_policy_version,
        CURRENT_APPROVAL_POLICY_VERSION
    );
    restored.sources[0].require_approval().unwrap();
    assert_eq!(restored.sources[0].script, before.sources[0].script);
    restored.sources[0].verify_integrity().unwrap();
}

#[test]
fn actual_controller_switches_between_two_approved_fixture_workers() {
    let directory = tempfile::tempdir().unwrap();
    let store = ProjectStore::open(directory.path().into()).unwrap();
    let original = include_str!("../fixtures/legal-local-source.js");
    // These are known project-owned scripts; only their leading names differ.
    // No unknown script is parsed inside the test/controller process.
    let a = import_script(&original.replace("@name Local Tone Catalog", "@name Switch fixture A"))
        .unwrap();
    let b = import_script(&original.replace("@name Local Tone Catalog", "@name Switch fixture B"))
        .unwrap();
    assert_ne!(a.sha256, b.sha256);
    assert!(!a.approved && !b.approved);
    let a_id = a.sha256.clone();
    let b_id = b.sha256.clone();
    store
        .save(&StoreData {
            sources: vec![a, b],
            ..Default::default()
        })
        .unwrap();

    let controller = ControllerHandle::start(directory.path().into()).unwrap();
    wait_for(&controller, "load unapproved fixtures", |snapshot| {
        snapshot.sources.len() == 2
            && snapshot
                .sources
                .iter()
                .all(|source| !source.approved && !source.enabled)
    });
    controller
        .commands
        .send(UiCommand::ApproveSource(a_id.clone()))
        .unwrap();
    wait_enabled(&controller, &a_id);
    controller
        .commands
        .send(UiCommand::ApproveSource(b_id.clone()))
        .unwrap();
    wait_enabled(&controller, &b_id);

    let switched = controller.snapshot.lock().unwrap().clone();
    assert_eq!(
        switched
            .sources
            .iter()
            .filter(|source| source.enabled)
            .count(),
        1
    );
    assert!(
        switched
            .sources
            .iter()
            .any(|source| source.id == a_id && source.approved && !source.enabled)
    );
    assert!(
        switched
            .sources
            .iter()
            .any(|source| source.id == b_id && source.approved && source.enabled)
    );
    let persisted = store.load().unwrap();
    assert_eq!(persisted.selected.as_deref(), Some(b_id.as_str()));
    assert!(persisted.sources.iter().all(|source| source.approved));
    shutdown(controller);
}

#[test]
fn legacy_network_approval_never_autoexecutes_or_restores_selected_source() {
    let directory = tempfile::tempdir().unwrap();
    // A valid original script which would immediately throw if evaluated.
    let mut source = import_script("/* @name Legacy permission fixture */\nthrow new Error('Legacy approval must never execute');").unwrap();
    source.approve().unwrap();
    let id = source.sha256.clone();
    let mut legacy = serde_json::json!({
        "version": 1,
        "data": { "sources": [source], "selected": id, "local_files": [], "volume": 0.7 }
    });
    legacy["data"]["sources"][0]
        .as_object_mut()
        .unwrap()
        .remove("approval_policy_version");
    std::fs::write(
        directory.path().join("player-state.json"),
        serde_json::to_vec(&legacy).unwrap(),
    )
    .unwrap();

    let controller = ControllerHandle::start(directory.path().into()).unwrap();
    controller
        .commands
        .send(UiCommand::Navigate(Page::Sources))
        .unwrap();
    let migrated = wait_for(
        &controller,
        "legacy network permission migration",
        |snapshot| snapshot.page == Page::Sources && snapshot.sources.len() == 1 && !snapshot.busy,
    );
    assert!(!migrated.sources[0].approved && !migrated.sources[0].enabled);
    let restored = ProjectStore::open(directory.path().into())
        .unwrap()
        .load()
        .unwrap();
    assert!(restored.selected.is_none());
    assert!(!restored.sources[0].approved);
    assert!(restored.sources[0].require_approval().is_err());
    assert_eq!(restored.sources[0].approval_policy_version, 0);
    shutdown(controller);
}

struct SlowInitServer {
    url: String,
    started: mpsc::Receiver<()>,
    disconnected: mpsc::Receiver<Result<(), String>>,
}

impl SlowInitServer {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}/init", listener.local_addr().unwrap());
        let (started_tx, started) = mpsc::channel();
        let (closed_tx, disconnected) = mpsc::channel();
        thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(8);
            let mut connection = loop {
                match listener.accept() {
                    Ok((connection, _)) => break connection,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        if Instant::now() >= deadline {
                            return;
                        }
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(_) => return,
                }
            };
            // Windows accepted sockets inherit the listener's nonblocking mode.
            // This connection intentionally waits for EOF after controller cancel.
            connection.set_nonblocking(false).unwrap();
            connection
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let mut request = Vec::new();
            let mut buffer = [0; 512];
            while !request.windows(4).any(|part| part == b"\r\n\r\n") {
                match connection.read(&mut buffer) {
                    Ok(0) | Err(_) => return,
                    Ok(count) => request.extend_from_slice(&buffer[..count]),
                }
                if request.len() > 8192 {
                    return;
                }
            }
            // Deliberately never send response headers: initialization is awaiting HTTP.
            let _ = started_tx.send(());
            let result = match connection.read(&mut buffer) {
                Ok(0) => Ok(()),
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::ConnectionReset
                            | std::io::ErrorKind::ConnectionAborted
                            | std::io::ErrorKind::BrokenPipe
                    ) =>
                {
                    Ok(())
                }
                Ok(_) => Err("Unexpected bytes after the fixture GET headers".into()),
                Err(error) => Err(format!("Initialization connection remained open: {error}")),
            };
            let _ = closed_tx.send(result);
        });
        Self {
            url,
            started,
            disconnected,
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum InitCancellation {
    Disable,
    DeleteSource,
    DropController,
    Switch,
}

#[test]
fn controller_cancels_pending_initialization_http_on_disable_delete_drop_and_switch() {
    for mode in [
        InitCancellation::Disable,
        InitCancellation::DeleteSource,
        InitCancellation::DropController,
        InitCancellation::Switch,
    ] {
        let directory = tempfile::tempdir().unwrap();
        let server = SlowInitServer::start();
        let slow_script = r#"/* @name Original slow init fixture */
lx.on(lx.EVENT_NAMES.request, async () => 'http://127.0.0.1:9/original.wav');
lx.request('__URL__', { method: 'get', timeout: 60000 }, (error) => {
  if (error) throw new Error('Original init fixture request failed');
  lx.send(lx.EVENT_NAMES.inited, { sources: { local: { type: 'music', actions: ['musicUrl'], qualitys: [] } } });
});"#.replace("__URL__", &server.url);
        let slow = import_script(&slow_script).unwrap();
        let fast = import_script(include_str!("../fixtures/legal-local-source.js")).unwrap();
        let slow_id = slow.sha256.clone();
        let fast_id = fast.sha256.clone();
        let store = ProjectStore::open(directory.path().into()).unwrap();
        store
            .save(&StoreData {
                sources: vec![slow, fast],
                ..Default::default()
            })
            .unwrap();
        let mut controller = Some(ControllerHandle::start(directory.path().into()).unwrap());
        controller
            .as_ref()
            .unwrap()
            .commands
            .send(UiCommand::ApproveSource(slow_id.clone()))
            .unwrap();
        server
            .started
            .recv_timeout(Duration::from_secs(8))
            .unwrap_or_else(|error| {
                panic!(
                    "{mode:?}: source never reached pending initialization HTTP: {error}; {:?}",
                    controller.as_ref().unwrap().snapshot.lock().unwrap()
                )
            });
        wait_for(
            controller.as_ref().unwrap(),
            "pending asynchronous initializer",
            |snapshot| snapshot.busy,
        );
        let old_snapshot = Arc::downgrade(&controller.as_ref().unwrap().snapshot);
        let cancelled_at = Instant::now();
        match mode {
            InitCancellation::Disable => controller
                .as_ref()
                .unwrap()
                .commands
                .send(UiCommand::DisableSource)
                .unwrap(),
            InitCancellation::DeleteSource => controller
                .as_ref()
                .unwrap()
                .commands
                .send(UiCommand::DeleteSource(slow_id.clone()))
                .unwrap(),
            InitCancellation::DropController => drop(controller.take()),
            InitCancellation::Switch => controller
                .as_ref()
                .unwrap()
                .commands
                .send(UiCommand::ApproveSource(fast_id.clone()))
                .unwrap(),
        }
        server
            .disconnected
            .recv_timeout(Duration::from_secs(2))
            .unwrap_or_else(|error| {
                panic!("{mode:?}: cancelled initialization kept its HTTP connection open: {error}")
            })
            .unwrap();
        assert!(
            cancelled_at.elapsed() < Duration::from_secs(2),
            "{mode:?}: initialization cancellation exceeded two seconds"
        );
        if let Some(controller) = controller {
            if matches!(mode, InitCancellation::Switch) {
                wait_enabled(&controller, &fast_id);
                assert_eq!(
                    store.load().unwrap().selected.as_deref(),
                    Some(fast_id.as_str())
                );
            } else {
                let stopped = wait_for(&controller, "cancel pending initializer", |snapshot| {
                    !snapshot.busy && snapshot.sources.iter().all(|source| !source.enabled)
                });
                assert!(store.load().unwrap().selected.is_none());
                if matches!(mode, InitCancellation::DeleteSource) {
                    assert!(stopped.sources.iter().all(|source| source.id != slow_id));
                    assert_eq!(store.load().unwrap().sources.len(), 1);
                }
            }
            shutdown(controller);
        } else {
            let deadline = Instant::now() + Duration::from_secs(2);
            while old_snapshot.strong_count() != 0 {
                assert!(
                    Instant::now() < deadline,
                    "Dropped controller did not exit after cancelling its initializer"
                );
                thread::sleep(Duration::from_millis(10));
            }
        }
    }
}

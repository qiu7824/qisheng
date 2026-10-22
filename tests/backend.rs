//! The public backend is exercised without GPUI or an output device.
use lx_gpui_player::{
    audio::PlaybackStatus,
    backend::{
        BackendCommand, BackendConfig, BackendEvent, BackendHandle, BackendSnapshot,
        CommandEnvelope, EVENT_CAPACITY, ErrorKind, ReceiptState,
    },
    repository::Repository,
    store::ProjectStore,
};
use std::{
    fs,
    io::{Read, Write},
    net::TcpListener,
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};
use tempfile::TempDir;

struct RunningBackend {
    handle: BackendHandle,
    directory: TempDir,
}

impl RunningBackend {
    fn new() -> Self {
        let base = std::env::var_os("TMP")
            .or_else(|| std::env::var_os("TEMP"))
            .map(std::path::PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        fs::create_dir_all(&base).unwrap();
        let directory = tempfile::Builder::new()
            .prefix("lx-native-backend-")
            .tempdir_in(base)
            .unwrap();
        let handle =
            BackendHandle::start(BackendConfig::headless(directory.path().into())).unwrap();
        Self { handle, directory }
    }

    fn store(&self) -> ProjectStore {
        ProjectStore::open(self.directory.path().into()).unwrap()
    }
}

impl Drop for RunningBackend {
    fn drop(&mut self) {
        let _ = self.handle.shutdown(Duration::from_secs(10));
    }
}

fn command(request: &str, command: BackendCommand) -> CommandEnvelope {
    CommandEnvelope {
        request_id: request.into(),
        origin: "backend-test".into(),
        expected_revision: None,
        command,
    }
}

fn wait_for(
    handle: &BackendHandle,
    phase: &str,
    ready: impl Fn(&BackendSnapshot) -> bool,
) -> BackendSnapshot {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let snapshot = handle.snapshot();
        if ready(&snapshot) {
            return snapshot;
        }
        assert!(
            Instant::now() < deadline,
            "{phase} did not converge: {snapshot:?}"
        );
        thread::sleep(Duration::from_millis(15));
    }
}

fn wait_operation(
    handle: &BackendHandle,
    cursor: u64,
    operation: &str,
) -> (bool, Option<ErrorKind>) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        for event in handle.events_after(cursor).unwrap() {
            if let BackendEvent::OperationFinished {
                operation_id,
                success,
                error,
            } = event.payload
                && operation_id == operation
            {
                return (success, error.map(|error| error.kind));
            }
        }
        assert!(
            Instant::now() < deadline,
            "Operation {operation} never published a terminal event"
        );
        thread::sleep(Duration::from_millis(15));
    }
}

fn import_source(handle: &BackendHandle, request: &str, script: &str) -> String {
    let cursor = handle.event_cursor();
    let receipt = handle
        .execute(command(
            request,
            BackendCommand::ImportSourceText {
                script: script.into(),
            },
        ))
        .unwrap();
    assert_eq!(receipt.state, ReceiptState::Accepted);
    assert_eq!(
        wait_operation(handle, cursor, &receipt.operation_id),
        (true, None)
    );
    let snapshot = wait_for(handle, "source import", |snapshot| !snapshot.busy);
    snapshot.sources.last().unwrap().id.clone()
}

#[test]
fn memory_backend_has_independent_profiles_and_paged_search_without_gpui() {
    let first = RunningBackend::new();
    let second = RunningBackend::new();
    let first_page = first.handle.tracks("", 0, 1).unwrap();
    assert_eq!(first_page.total, 3);
    assert_eq!(first_page.items.len(), 1);
    let next_page = first.handle.tracks("", 1, 1).unwrap();
    assert_ne!(first_page.items[0].id, next_page.items[0].id);
    let search = first.handle.tracks("  VIOLET  ", 0, 10).unwrap();
    assert_eq!(search.total, 1);
    assert_eq!(search.items[0].id, "tone-2");
    assert_eq!(first.handle.tracks("native lab", 0, 10).unwrap().total, 3);
    assert!(first.handle.tracks("", 99, 10).unwrap().items.is_empty());
    assert_eq!(
        first.handle.tracks("", 0, 0).unwrap_err().kind,
        ErrorKind::InvalidData
    );
    assert_eq!(
        first.handle.tracks("", 0, 1001).unwrap_err().kind,
        ErrorKind::InvalidData
    );
    first
        .handle
        .execute(command(
            "first-volume",
            BackendCommand::Volume { volume: 0.25 },
        ))
        .unwrap();
    assert_eq!(second.store().load().unwrap().volume, 0.7);
    assert!(first.handle.snapshot().error.is_none());
    assert!(second.handle.snapshot().error.is_none());
}

#[test]
fn volume_commits_to_disk_before_revision_event_and_restarts_with_committed_value() {
    let backend = RunningBackend::new();
    let cursor = backend.handle.event_cursor();
    let previous_revision = backend.handle.snapshot().revision;
    let receipt = backend
        .handle
        .execute(command(
            "saved-volume",
            BackendCommand::Volume { volume: 0.375 },
        ))
        .unwrap();
    assert_eq!(receipt.state, ReceiptState::Committed);
    assert!(receipt.revision > previous_revision);
    let events = backend.handle.events_after(cursor).unwrap();
    assert!(
        events
            .iter()
            .any(|event| event.repository_revision == receipt.revision)
    );
    for event in events
        .iter()
        .filter(|event| event.repository_revision == receipt.revision)
    {
        assert_eq!(
            backend.store().load().unwrap().volume,
            0.375,
            "event {} was visible before the persisted volume",
            event.event_id
        );
    }
    wait_for(&backend.handle, "audio volume projection", |snapshot| {
        snapshot.playback.volume == 0.375
    });
    backend.handle.shutdown(Duration::from_secs(10)).unwrap();
    let restarted =
        BackendHandle::start(BackendConfig::headless(backend.directory.path().into())).unwrap();
    wait_for(&restarted, "restored committed volume", |snapshot| {
        snapshot.playback.volume == 0.375
    });
    restarted.shutdown(Duration::from_secs(10)).unwrap();
}

#[test]
fn failed_volume_persistence_preserves_disk_audio_and_revision() {
    let backend = RunningBackend::new();
    wait_for(&backend.handle, "initial volume", |snapshot| {
        snapshot.playback.volume == 0.7
    });
    let revision = backend.handle.snapshot().revision;
    let path = backend.directory.path().join("player-state.json");
    let damaged = b"invalid user data must remain intact";
    fs::write(&path, damaged).unwrap();
    assert!(
        backend
            .handle
            .execute(command(
                "failed-volume",
                BackendCommand::Volume { volume: 0.1 }
            ))
            .is_err()
    );
    let snapshot = backend.handle.snapshot();
    assert_eq!(snapshot.revision, revision);
    assert_eq!(snapshot.playback.volume, 0.7);
    assert_eq!(fs::read(path).unwrap(), damaged);
}

#[test]
fn expected_revision_conflict_has_no_commit_and_request_ids_are_idempotent() {
    let backend = RunningBackend::new();
    let revision = backend.handle.snapshot().revision;
    let mut request = command("idempotent-volume", BackendCommand::Volume { volume: 0.42 });
    request.expected_revision = Some(revision);
    let first = backend.handle.execute(request.clone()).unwrap();
    let cursor = backend.handle.event_cursor();
    let second = backend.handle.execute(request.clone()).unwrap();
    assert_eq!(first.operation_id, second.operation_id);
    assert_eq!(first.revision, second.revision);
    assert_eq!(backend.handle.snapshot().revision, first.revision);
    assert!(
        !backend
            .handle
            .events_after(cursor)
            .unwrap()
            .iter()
            .any(|event| event.repository_revision > first.revision)
    );
    let mut collision = request;
    collision.command = BackendCommand::Volume { volume: 0.15 };
    assert_eq!(
        backend.handle.execute(collision).unwrap_err().kind,
        ErrorKind::Conflict
    );
    let mut stale = command("stale-volume", BackendCommand::Volume { volume: 0.2 });
    stale.expected_revision = Some(revision);
    assert_eq!(
        backend.handle.execute(stale).unwrap_err().kind,
        ErrorKind::Conflict
    );
    assert_eq!(backend.store().load().unwrap().volume, 0.42);
}

#[test]
fn source_import_is_static_unapproved_and_async_replay_does_not_duplicate_it() {
    let backend = RunningBackend::new();
    let script =
        "/* @name Backend static import fixture */\nthrow new Error('must not execute on import');";
    let cursor = backend.handle.event_cursor();
    let request = command(
        "static-import",
        BackendCommand::ImportSourceText {
            script: script.into(),
        },
    );
    let receipt = backend.handle.execute(request.clone()).unwrap();
    assert_eq!(receipt.state, ReceiptState::Accepted);
    assert_eq!(
        wait_operation(&backend.handle, cursor, &receipt.operation_id),
        (true, None)
    );
    let replay = backend.handle.execute(request).unwrap();
    assert_eq!(replay.operation_id, receipt.operation_id);
    let snapshot = wait_for(&backend.handle, "static source visible", |snapshot| {
        snapshot.sources.len() == 1 && !snapshot.busy
    });
    assert!(!snapshot.sources[0].approved && !snapshot.sources[0].enabled);
    assert!(snapshot.error.is_none());
    let persisted = backend.store().load().unwrap();
    assert_eq!(persisted.sources.len(), 1);
    assert!(!persisted.sources[0].approved);
    assert!(persisted.selected.is_none());
    assert!(
        backend
            .handle
            .execute(command(
                "enable-without-permission",
                BackendCommand::EnableSource {
                    id: snapshot.sources[0].id.clone()
                }
            ))
            .is_err()
    );
}

#[test]
fn slow_event_cursor_expires_and_fresh_snapshot_cursor_can_resume() {
    let backend = RunningBackend::new();
    let old_cursor = backend.handle.event_cursor();
    for index in 0..EVENT_CAPACITY + 12 {
        let volume = if index % 2 == 0 { 0.2 } else { 0.8 };
        backend
            .handle
            .execute(command(
                &format!("volume-{index}"),
                BackendCommand::Volume { volume },
            ))
            .unwrap();
    }
    assert_eq!(
        backend.handle.events_after(old_cursor).unwrap_err().kind,
        ErrorKind::CursorExpired
    );
    let snapshot = backend.handle.snapshot();
    let fresh_cursor = backend.handle.event_cursor();
    let receipt = backend
        .handle
        .execute(command(
            "after-fresh-cursor",
            BackendCommand::Volume { volume: 0.6 },
        ))
        .unwrap();
    assert!(receipt.revision > snapshot.revision);
    assert!(
        backend
            .handle
            .events_after(fresh_cursor)
            .unwrap()
            .iter()
            .any(|event| event.repository_revision == receipt.revision)
    );
}

#[test]
fn explicit_shutdown_joins_worker_closes_commands_and_releases_profile_writer() {
    let backend = RunningBackend::new();
    assert!(
        BackendHandle::start(BackendConfig::headless(backend.directory.path().into())).is_err()
    );
    let cursor = backend.handle.event_cursor();
    backend.handle.shutdown(Duration::from_secs(10)).unwrap();
    assert!(backend.handle.snapshot().stopped);
    assert!(
        backend
            .handle
            .events_after(cursor)
            .unwrap()
            .iter()
            .any(|event| matches!(event.payload, BackendEvent::Stopped))
    );
    assert_eq!(
        backend
            .handle
            .execute(command("after-shutdown", BackendCommand::Pause))
            .unwrap_err()
            .kind,
        ErrorKind::Closed
    );
    let repository = Repository::open(backend.directory.path()).unwrap();
    repository.integrity_check().unwrap();
    drop(repository);
    let restarted =
        BackendHandle::start(BackendConfig::headless(backend.directory.path().into())).unwrap();
    restarted.shutdown(Duration::from_secs(10)).unwrap();
    backend.handle.shutdown(Duration::from_secs(1)).unwrap();
}

struct DeferredInitServer {
    url: String,
    started: mpsc::Receiver<()>,
    release: mpsc::Sender<()>,
    thread: Option<thread::JoinHandle<()>>,
}

impl DeferredInitServer {
    fn new() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}/initialization", listener.local_addr().unwrap());
        let (started_tx, started) = mpsc::channel();
        let (release, released) = mpsc::channel();
        let thread = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(10);
            let mut connection = loop {
                match listener.accept() {
                    Ok((connection, _)) => break connection,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        if released.try_recv().is_ok() || Instant::now() >= deadline {
                            return;
                        }
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(_) => return,
                }
            };
            connection.set_nonblocking(false).unwrap();
            connection
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let mut request = Vec::new();
            let mut bytes = [0; 512];
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                match connection.read(&mut bytes) {
                    Ok(0) | Err(_) => return,
                    Ok(count) => request.extend_from_slice(&bytes[..count]),
                }
                if request.len() > 8192 {
                    return;
                }
            }
            let _ = started_tx.send(());
            let _ = released.recv_timeout(Duration::from_secs(10));
            let _ = connection
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok");
        });
        Self {
            url,
            started,
            release,
            thread: Some(thread),
        }
    }

    fn script(&self) -> String {
        r#"/* @name Original delayed backend initializer */
lx.on(lx.EVENT_NAMES.request, async () => 'http://127.0.0.1:9/original.wav');
lx.request('__URL__', {method:'get', timeout:60000}, (error) => {
  if (error) throw new Error('Delayed initialization failed');
  lx.send(lx.EVENT_NAMES.inited, {sources:{local:{type:'music',actions:['musicUrl'],qualitys:[]}}});
});"#
            .replace("__URL__", &self.url)
    }
}

impl Drop for DeferredInitServer {
    fn drop(&mut self) {
        let _ = self.release.send(());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[test]
fn cancel_operation_finishes_initializer_and_late_result_never_reenables_source() {
    let backend = RunningBackend::new();
    let server = DeferredInitServer::new();
    let id = import_source(&backend.handle, "import-delayed", &server.script());
    let cursor = backend.handle.event_cursor();
    let receipt = backend
        .handle
        .execute(command(
            "enable-delayed",
            BackendCommand::ApproveSource { id: id.clone() },
        ))
        .unwrap();
    server
        .started
        .recv_timeout(Duration::from_secs(10))
        .unwrap();
    assert!(backend.handle.snapshot().busy);
    backend
        .handle
        .execute(command(
            "cancel-delayed",
            BackendCommand::CancelOperation {
                operation_id: receipt.operation_id.clone(),
            },
        ))
        .unwrap();
    assert_eq!(
        wait_operation(&backend.handle, cursor, &receipt.operation_id),
        (false, Some(ErrorKind::Cancelled))
    );
    server.release.send(()).unwrap();
    wait_for(
        &backend.handle,
        "cancelled source remains inactive",
        |snapshot| !snapshot.busy && snapshot.sources.iter().all(|source| !source.enabled),
    );
    thread::sleep(Duration::from_millis(200));
    assert!(
        backend
            .handle
            .snapshot()
            .sources
            .iter()
            .all(|source| !source.enabled)
    );
    assert!(backend.store().load().unwrap().selected.is_none());
}

#[test]
fn replacing_initializer_isolates_old_completion_and_preserves_new_selection() {
    let backend = RunningBackend::new();
    let server = DeferredInitServer::new();
    let slow_id = import_source(&backend.handle, "import-slow", &server.script());
    let fast_id = import_source(
        &backend.handle,
        "import-fast",
        include_str!("../fixtures/legal-local-source.js"),
    );
    let cursor = backend.handle.event_cursor();
    let slow = backend
        .handle
        .execute(command(
            "start-slow",
            BackendCommand::ApproveSource {
                id: slow_id.clone(),
            },
        ))
        .unwrap();
    server
        .started
        .recv_timeout(Duration::from_secs(10))
        .unwrap();
    let fast = backend
        .handle
        .execute(command(
            "start-fast",
            BackendCommand::ApproveSource {
                id: fast_id.clone(),
            },
        ))
        .unwrap();
    assert_eq!(
        wait_operation(&backend.handle, cursor, &slow.operation_id),
        (false, Some(ErrorKind::Cancelled))
    );
    assert_eq!(
        wait_operation(&backend.handle, cursor, &fast.operation_id),
        (true, None)
    );
    server.release.send(()).unwrap();
    wait_for(&backend.handle, "new source selected", |snapshot| {
        !snapshot.busy
            && snapshot
                .sources
                .iter()
                .any(|source| source.id == fast_id && source.enabled)
    });
    thread::sleep(Duration::from_millis(200));
    let snapshot = backend.handle.snapshot();
    assert!(
        snapshot
            .sources
            .iter()
            .any(|source| source.id == fast_id && source.enabled)
    );
    assert!(
        snapshot
            .sources
            .iter()
            .all(|source| source.id != slow_id || !source.enabled)
    );
    assert_eq!(
        backend.store().load().unwrap().selected.as_deref(),
        Some(fast_id.as_str())
    );
}

#[test]
fn memory_playback_controls_are_available_without_ui_or_device_initialization() {
    let backend = RunningBackend::new();
    let track = backend.handle.tracks("", 0, 1).unwrap().items[0].clone();
    backend
        .handle
        .execute(command(
            "play-memory",
            BackendCommand::Play {
                id: track.id.clone(),
            },
        ))
        .unwrap();
    wait_for(&backend.handle, "memory playback", |snapshot| {
        snapshot.playback.status == PlaybackStatus::Playing
    });
    backend
        .handle
        .execute(command("pause-memory", BackendCommand::Pause))
        .unwrap();
    wait_for(&backend.handle, "paused memory playback", |snapshot| {
        snapshot.playback.status == PlaybackStatus::Paused
    });
    let seek = backend.handle.snapshot().playback.seek_seq;
    backend
        .handle
        .execute(command(
            "seek-memory",
            BackendCommand::Seek { seconds: 5.0 },
        ))
        .unwrap();
    wait_for(&backend.handle, "paused seek", |snapshot| {
        snapshot.playback.seek_seq > seek && (snapshot.playback.position_seconds - 5.0).abs() < 0.1
    });
    backend
        .handle
        .execute(command("stop-memory", BackendCommand::Stop))
        .unwrap();
    wait_for(&backend.handle, "stopped memory playback", |snapshot| {
        snapshot.playback.status == PlaybackStatus::Idle
    });
}

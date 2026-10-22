//! Playback intent survives a real source request before Memory audio is loaded.

use lx_gpui_player::{
    audio::PlaybackStatus,
    backend::{
        BackendCommand, BackendConfig, BackendEvent, BackendHandle, BackendSnapshot,
        CommandEnvelope, ReceiptState,
    },
};
use std::{
    io::{Read, Write},
    net::TcpListener,
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};
use tempfile::TempDir;

struct RunningBackend {
    handle: BackendHandle,
    _directory: TempDir,
}

impl RunningBackend {
    fn new() -> Self {
        let directory = tempfile::Builder::new()
            .prefix("lx-native-backend-pause-")
            .tempdir()
            .unwrap();
        let handle =
            BackendHandle::start(BackendConfig::headless(directory.path().into())).unwrap();
        Self {
            handle,
            _directory: directory,
        }
    }
}

impl Drop for RunningBackend {
    fn drop(&mut self) {
        let _ = self.handle.shutdown(Duration::from_secs(10));
    }
}

fn envelope(request_id: &str, command: BackendCommand) -> CommandEnvelope {
    CommandEnvelope {
        request_id: request_id.into(),
        origin: "backend-pause-test".into(),
        expected_revision: None,
        command,
    }
}

fn wait_for(handle: &BackendHandle, ready: impl Fn(&BackendSnapshot) -> bool) -> BackendSnapshot {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let state = handle.snapshot();
        if ready(&state) {
            return state;
        }
        assert!(
            Instant::now() < deadline,
            "State did not converge: {state:?}"
        );
        thread::sleep(Duration::from_millis(15));
    }
}

fn wait_operation(handle: &BackendHandle, cursor: u64, operation_id: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        for event in handle.events_after(cursor).unwrap() {
            if let BackendEvent::OperationFinished {
                operation_id: finished,
                success,
                error,
            } = event.payload
                && finished == operation_id
            {
                assert!(success, "Operation failed: {error:?}");
                return;
            }
        }
        assert!(Instant::now() < deadline, "Operation did not complete");
        thread::sleep(Duration::from_millis(15));
    }
}

struct SlowMusicUrlServer {
    url: String,
    started: mpsc::Receiver<()>,
    release: mpsc::Sender<()>,
    thread: Option<thread::JoinHandle<()>>,
}

impl SlowMusicUrlServer {
    fn new() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}/resolve", listener.local_addr().unwrap());
        let (started_tx, started) = mpsc::channel();
        let (release, released) = mpsc::channel();
        let thread = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(10);
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        if released.try_recv().is_ok() || Instant::now() >= deadline {
                            return;
                        }
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(_) => return,
                }
            };
            // Windows accepted sockets inherit the listener's nonblocking state.
            stream.set_nonblocking(false).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            stream
                .set_write_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let mut headers = Vec::new();
            let mut chunk = [0; 512];
            while !headers.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                match stream.read(&mut chunk) {
                    Ok(0) | Err(_) => return,
                    Ok(count) => headers.extend_from_slice(&chunk[..count]),
                }
                if headers.len() > 8192 {
                    return;
                }
            }
            let _ = started_tx.send(());
            let _ = released.recv_timeout(Duration::from_secs(10));
            let _ = stream
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
        format!(
            r#"/*
 * @name Delayed music URL fixture
 * @author LX Native contributors
 */
lx.on(lx.EVENT_NAMES.request, async ({{action,info}}) => {{
  if (action !== 'musicUrl') throw new Error('Unexpected action');
  await new Promise((resolve,reject) => lx.request({}, {{timeout:60000}}, (error) => error ? reject(error) : resolve()));
  return info.musicInfo.filePath;
}});
lx.send(lx.EVENT_NAMES.inited, {{sources:{{local:{{type:'music',actions:['musicUrl'],qualitys:[]}}}}}});
"#,
            serde_json::to_string(&self.url).unwrap()
        )
    }

    fn wait_started(&self) {
        self.started.recv_timeout(Duration::from_secs(10)).unwrap();
    }

    fn respond(&self) {
        self.release.send(()).unwrap();
    }
}

impl Drop for SlowMusicUrlServer {
    fn drop(&mut self) {
        let _ = self.release.send(());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn approve_slow_source(handle: &BackendHandle, server: &SlowMusicUrlServer) {
    let cursor = handle.event_cursor();
    handle
        .execute(envelope(
            "import-source",
            BackendCommand::ImportSourceText {
                script: server.script(),
            },
        ))
        .unwrap();
    wait_operation(handle, cursor, "import-source");
    let id = handle.snapshot().sources[0].id.clone();
    let cursor = handle.event_cursor();
    handle
        .execute(envelope(
            "approve-source",
            BackendCommand::ApproveSource { id },
        ))
        .unwrap();
    wait_operation(handle, cursor, "approve-source");
    wait_for(handle, |state| !state.busy && state.sources[0].enabled);
}

#[test]
fn pause_during_music_url_resolution_keeps_real_memory_audio_paused() {
    let backend = RunningBackend::new();
    let server = SlowMusicUrlServer::new();
    approve_slow_source(&backend.handle, &server);
    let cursor = backend.handle.event_cursor();
    let receipt = backend
        .handle
        .execute(envelope(
            "play-tone",
            BackendCommand::Play {
                id: "tone-1".into(),
            },
        ))
        .unwrap();
    assert_eq!(receipt.state, ReceiptState::Accepted);
    server.wait_started();
    backend
        .handle
        .execute(envelope("pause-pending", BackendCommand::Pause))
        .unwrap();
    server.respond();
    wait_operation(&backend.handle, cursor, "play-tone");
    let paused = wait_for(&backend.handle, |state| {
        state.playback.track_id.as_deref() == Some("tone-1")
            && state.playback.status == PlaybackStatus::Paused
    });
    assert!((paused.playback.duration_seconds - 45.0).abs() < 0.02);
    assert!(paused.playback.position_seconds < 0.05);
    thread::sleep(Duration::from_millis(200));
    let still_paused = backend.handle.snapshot();
    assert_eq!(still_paused.playback.status, PlaybackStatus::Paused);
    assert!(
        (still_paused.playback.position_seconds - paused.playback.position_seconds).abs() < 0.05
    );
}

#[test]
fn resume_after_pending_pause_plays_real_memory_audio_when_url_arrives() {
    let backend = RunningBackend::new();
    let server = SlowMusicUrlServer::new();
    approve_slow_source(&backend.handle, &server);
    let cursor = backend.handle.event_cursor();
    backend
        .handle
        .execute(envelope(
            "play-tone",
            BackendCommand::Play {
                id: "tone-1".into(),
            },
        ))
        .unwrap();
    server.wait_started();
    backend
        .handle
        .execute(envelope("pause-pending", BackendCommand::Pause))
        .unwrap();
    backend
        .handle
        .execute(envelope("resume-pending", BackendCommand::Resume))
        .unwrap();
    server.respond();
    wait_operation(&backend.handle, cursor, "play-tone");
    let playing = wait_for(&backend.handle, |state| {
        state.playback.track_id.as_deref() == Some("tone-1")
            && state.playback.status == PlaybackStatus::Playing
            && state.playback.position_seconds > 0.1
    });
    assert!((playing.playback.duration_seconds - 45.0).abs() < 0.02);
    assert!(playing.error.is_none());
}

//! Lyric cancellation reuses one initialized source session through the real process boundary.

use lx_gpui_player::{
    audio::PlaybackStatus,
    backend::{
        BackendCommand, BackendConfig, BackendEvent, BackendHandle, BackendSnapshot,
        CommandEnvelope,
    },
    domain::lyric::{LyricBundle, LyricOrigin},
};
use serde_json::json;
use std::{
    fs,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

struct Running {
    directory: tempfile::TempDir,
    backend: BackendHandle,
}
impl Running {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let backend =
            BackendHandle::start(BackendConfig::headless(directory.path().join("profile")))
                .unwrap();
        Self { directory, backend }
    }
    fn execute(&self, id: &str, command: BackendCommand) {
        self.backend
            .execute(CommandEnvelope {
                request_id: id.into(),
                origin: "reload-test".into(),
                expected_revision: None,
                command,
            })
            .unwrap();
    }
    fn import_audio(&self) -> String {
        let path = self.directory.path().join("reload.wav");
        write_wave(&path);
        let cursor = self.backend.event_cursor();
        self.execute(
            "import-audio",
            BackendCommand::ImportAudio { paths: vec![path] },
        );
        terminal(&self.backend, cursor, "import-audio");
        self.backend.tracks("reload", 0, 10).unwrap().items[0]
            .id
            .clone()
    }
    fn enable_source(&self, server: &SourceServer) {
        let script = format!(
            r#"/**
 * @name Original lazy reload source
 */
let lyricRequests = 0;
lx.on('request', async ({{ action, info }}) => {{
  if (!info.musicInfo.filePath.startsWith('http://127.0.0.1:')) throw Error('Private path escaped');
  if (action === 'musicUrl') return info.musicInfo.filePath;
  const ordinal = ++lyricRequests;
  if (ordinal === 2) await lx.send('updateAlert', {{log:'Lyric reload uses request 2.'}});
  return new Promise((resolve, reject) => lx.request({}, {{}}, (error, response, body) => error ? reject(error) : resolve(body)));
}});
lx.request({}, {{}}, (error) => {{
  if (error) throw error;
  lx.send('inited', {{ sources: {{ local: {{ type: 'music', actions: ['musicUrl', 'lyric'], qualitys: [] }} }} }});
}});
"#,
            serde_json::to_string(&format!("{}/lyric", server.url)).unwrap(),
            serde_json::to_string(&format!("{}/init", server.url)).unwrap()
        );
        let cursor = self.backend.event_cursor();
        self.execute("import-source", BackendCommand::ImportSourceText { script });
        terminal(&self.backend, cursor, "import-source");
        let id = self.backend.snapshot().sources[0].id.clone();
        let cursor = self.backend.event_cursor();
        self.execute("approve-source", BackendCommand::ApproveSource { id });
        terminal(&self.backend, cursor, "approve-source");
    }
}
impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.backend.shutdown(Duration::from_secs(10));
    }
}

fn write_wave(path: &Path) {
    let size = 8000u32 * 2 * 30;
    let mut bytes = Vec::with_capacity(44 + size as usize);
    bytes.extend_from_slice(b"RIFF");
    bytes.extend_from_slice(&(36 + size).to_le_bytes());
    bytes.extend_from_slice(b"WAVEfmt ");
    bytes.extend_from_slice(&16u32.to_le_bytes());
    bytes.extend_from_slice(&1u16.to_le_bytes());
    bytes.extend_from_slice(&1u16.to_le_bytes());
    bytes.extend_from_slice(&8000u32.to_le_bytes());
    bytes.extend_from_slice(&16000u32.to_le_bytes());
    bytes.extend_from_slice(&2u16.to_le_bytes());
    bytes.extend_from_slice(&16u16.to_le_bytes());
    bytes.extend_from_slice(b"data");
    bytes.extend_from_slice(&size.to_le_bytes());
    bytes.resize(44 + size as usize, 0);
    fs::write(path, bytes).unwrap();
}
fn wait(backend: &BackendHandle, condition: impl Fn(&BackendSnapshot) -> bool) -> BackendSnapshot {
    let deadline = Instant::now() + Duration::from_secs(12);
    loop {
        let state = backend.snapshot();
        if condition(&state) {
            return state;
        }
        assert!(
            Instant::now() < deadline,
            "Reload did not converge: {state:?}"
        );
        thread::sleep(Duration::from_millis(15));
    }
}
fn terminal(backend: &BackendHandle, cursor: u64, operation: &str) {
    let deadline = Instant::now() + Duration::from_secs(12);
    loop {
        for event in backend.events_after(cursor).unwrap() {
            if let BackendEvent::OperationFinished {
                operation_id,
                success,
                error,
            } = event.payload
                && operation_id == operation
            {
                assert!(success, "{error:?}");
                return;
            }
        }
        assert!(
            Instant::now() < deadline,
            "Operation {operation} did not finish"
        );
        thread::sleep(Duration::from_millis(15));
    }
}

struct SourceServer {
    url: String,
    first_lyric: mpsc::Receiver<()>,
    init_calls: Arc<AtomicUsize>,
    lyric_calls: Arc<AtomicUsize>,
    first_closed: Arc<AtomicBool>,
    release: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}
impl SourceServer {
    fn new() -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let (sender, first_lyric) = mpsc::sync_channel(1);
        let init_calls = Arc::new(AtomicUsize::new(0));
        let lyric_calls = Arc::new(AtomicUsize::new(0));
        let first_closed = Arc::new(AtomicBool::new(false));
        let release = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        let init = init_calls.clone();
        let lyrics = lyric_calls.clone();
        let closed = first_closed.clone();
        let released = release.clone();
        let stopping = stop.clone();
        let worker = thread::spawn(move || {
            let mut clients = Vec::new();
            while !stopping.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let init = init.clone();
                        let lyrics = lyrics.clone();
                        let released = released.clone();
                        let stopping = stopping.clone();
                        let closed = closed.clone();
                        let sender = sender.clone();
                        clients.push(thread::spawn(move || {
                            serve(stream, init, lyrics, released, stopping, sender, closed)
                        }));
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5))
                    }
                    Err(_) => break,
                }
            }
            for client in clients {
                let _ = client.join();
            }
        });
        Self {
            url,
            first_lyric,
            init_calls,
            lyric_calls,
            first_closed,
            release,
            stop,
            worker: Some(worker),
        }
    }
}
impl Drop for SourceServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.release.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}
fn serve(
    mut stream: TcpStream,
    init: Arc<AtomicUsize>,
    lyrics: Arc<AtomicUsize>,
    release: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    first: mpsc::SyncSender<()>,
    closed: Arc<AtomicBool>,
) {
    stream.set_nonblocking(false).unwrap();
    let _ = stream.set_read_timeout(Some(Duration::from_secs(3)));
    let mut request = Vec::new();
    let mut buffer = [0u8; 1024];
    while request.len() < 8192 && !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
        match stream.read(&mut buffer) {
            Ok(0) | Err(_) => return,
            Ok(count) => request.extend_from_slice(&buffer[..count]),
        }
    }
    let request = String::from_utf8_lossy(&request);
    let body = if request.starts_with("GET /init ") {
        init.fetch_add(1, Ordering::AcqRel);
        "{}"
    } else {
        let number = lyrics.fetch_add(1, Ordering::AcqRel);
        if number == 0 {
            let _ = first.send(());
            stream
                .set_read_timeout(Some(Duration::from_millis(10)))
                .unwrap();
            let deadline = Instant::now() + Duration::from_secs(15);
            while !release.load(Ordering::Acquire)
                && !stop.load(Ordering::Acquire)
                && Instant::now() < deadline
            {
                match stream.read(&mut buffer[..1]) {
                    Ok(0) => {
                        closed.store(true, Ordering::Release);
                        return;
                    }
                    Err(error)
                        if matches!(
                            error.kind(),
                            std::io::ErrorKind::ConnectionReset
                                | std::io::ErrorKind::ConnectionAborted
                        ) =>
                    {
                        closed.store(true, Ordering::Release);
                        return;
                    }
                    Err(error)
                        if matches!(
                            error.kind(),
                            std::io::ErrorKind::TimedOut
                                | std::io::ErrorKind::WouldBlock
                                | std::io::ErrorKind::Interrupted
                        ) => {}
                    Ok(_) => panic!("bodyless lyric GET unexpectedly sent additional bytes"),
                    Err(error) => {
                        panic!("lyric socket failed before cancellation was observed: {error}")
                    }
                }
            }
            r#"{"lyric":"[00:00.000]OLD"}"#
        } else {
            r#"{"lyric":"[00:00.000]RELOADED","tlyric":null,"rlyric":""}"#
        }
    };
    let _ = write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
}

#[test]
fn load_lyrics_reuses_cancelled_worker_session_and_completes_the_new_request() {
    let running = Running::new();
    let server = SourceServer::new();
    let id = running.import_audio();
    running.enable_source(&server);
    running.execute("play", BackendCommand::Play { id: id.clone() });
    server
        .first_lyric
        .recv_timeout(Duration::from_secs(8))
        .unwrap();
    let playing = wait(&running.backend, |state| {
        state.playback.status == PlaybackStatus::Playing
            && state.playback.track_id.as_deref() == Some(id.as_str())
            && state.playback.has_presented_media
    });
    let generation = playing.playback.generation;
    let cursor = running.backend.event_cursor();
    running.execute("reload", BackendCommand::LoadLyrics { id: id.clone() });
    terminal(&running.backend, cursor, "reload");
    let state = wait(&running.backend, |state| {
        state
            .lyrics
            .as_ref()
            .is_some_and(|lyrics| lyrics.bundle.lyric.text() == Some("[00:00.000]RELOADED"))
    });
    assert_eq!(state.playback.generation, generation);
    assert_eq!(state.playback.status, PlaybackStatus::Playing);
    assert!(state.error.is_none());
    assert!(state.playback.error.is_none());
    assert!(state.lyrics_error.is_none());
    assert_eq!(server.init_calls.load(Ordering::Acquire), 1);
    assert_eq!(server.lyric_calls.load(Ordering::Acquire), 2);
    assert!(state.sources[0].enabled);
    let closed_deadline = Instant::now() + Duration::from_secs(3);
    while !server.first_closed.load(Ordering::Acquire) {
        assert!(
            Instant::now() < closed_deadline,
            "the cancelled lyric HTTP connection did not close"
        );
        thread::sleep(Duration::from_millis(10));
    }
    assert!(running.backend.events_after(cursor).unwrap().iter().any(|event| {
        matches!(&event.payload, BackendEvent::SourceHost { payload: lx_gpui_player::source::SourceHostEvent::UpdateAlert { log, .. }, .. } if log == "Lyric reload uses request 2.")
    }), "the reloaded lyric must execute as request 2 in the same JavaScript closure");
    server.release.store(true, Ordering::Release);
    thread::sleep(Duration::from_millis(120));
    assert_eq!(
        running
            .backend
            .lyrics(&id, Some(LyricOrigin::Raw))
            .unwrap()
            .unwrap()
            .lyric
            .text(),
        Some("[00:00.000]RELOADED")
    );
}

#[test]
fn replacing_inflight_lyrics_with_edited_cache_does_not_execute_source_initialization() {
    let running = Running::new();
    let server = SourceServer::new();
    let id = running.import_audio();
    running.enable_source(&server);
    running.execute("play", BackendCommand::Play { id: id.clone() });
    server
        .first_lyric
        .recv_timeout(Duration::from_secs(8))
        .unwrap();
    let edited =
        LyricBundle::from_value(&json!({"lyric":"[00:00.000]EDITED","tlyric":null})).unwrap();
    running.execute(
        "edit",
        BackendCommand::SaveLyrics {
            id: id.clone(),
            bundle: edited.clone(),
        },
    );
    wait(&running.backend, |state| {
        state
            .lyrics
            .as_ref()
            .is_some_and(|lyrics| lyrics.bundle == edited)
    });
    let cursor = running.backend.event_cursor();
    running.execute(
        "reload-edited",
        BackendCommand::LoadLyrics { id: id.clone() },
    );
    terminal(&running.backend, cursor, "reload-edited");
    assert_eq!(server.init_calls.load(Ordering::Acquire), 1);
    assert_eq!(server.lyric_calls.load(Ordering::Acquire), 1);
    assert!(running.backend.snapshot().sources[0].enabled);
    server.release.store(true, Ordering::Release);
    thread::sleep(Duration::from_millis(120));
    assert!(
        running
            .backend
            .lyrics(&id, Some(LyricOrigin::Raw))
            .unwrap()
            .is_none()
    );
    assert_eq!(
        running
            .backend
            .lyrics(&id, Some(LyricOrigin::Edited))
            .unwrap(),
        Some(edited)
    );
}

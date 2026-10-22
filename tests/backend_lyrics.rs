use lx_gpui_player::{
    audio::PlaybackStatus,
    backend::{
        BackendCommand, BackendConfig, BackendEvent, BackendHandle, BackendSnapshot,
        CommandEnvelope,
    },
    domain::lyric::{LyricBundle, LyricOrigin},
    lyrics::service::{LyricExportFormat, LyricProvenance},
};
use serde_json::json;
use std::{
    fs,
    io::{Read, Write},
    net::TcpListener,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

struct Profile {
    directory: tempfile::TempDir,
    handle: BackendHandle,
}
impl Profile {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let handle =
            BackendHandle::start(BackendConfig::headless(directory.path().join("profile")))
                .unwrap();
        Self { directory, handle }
    }
    fn command(&self, id: &str, command: BackendCommand) {
        self.handle.execute(envelope(id, command)).unwrap();
    }
    fn import(&self, name: &str, lyric: Option<&str>) -> String {
        let path = self.directory.path().join(format!("{name}.wav"));
        wave(&path);
        if let Some(lyric) = lyric {
            fs::write(path.with_extension("lrc"), lyric).unwrap();
        }
        let cursor = self.handle.event_cursor();
        let id = format!("import-{name}");
        self.command(&id, BackendCommand::ImportAudio { paths: vec![path] });
        wait_operation(&self.handle, cursor, &id);
        self.handle
            .tracks(name, 0, 10)
            .unwrap()
            .items
            .into_iter()
            .find(|track| track.title == name)
            .unwrap()
            .id
    }
    fn source(&self, script: &str) -> String {
        let script = format!("/**\n * @name Original backend lyric fixture\n */\n{script}");
        let cursor = self.handle.event_cursor();
        self.command("import-source", BackendCommand::ImportSourceText { script });
        wait_operation(&self.handle, cursor, "import-source");
        let id = self.handle.snapshot().sources.last().unwrap().id.clone();
        let cursor = self.handle.event_cursor();
        self.command(
            "approve-source",
            BackendCommand::ApproveSource { id: id.clone() },
        );
        wait_operation(&self.handle, cursor, "approve-source");
        id
    }
}
impl Drop for Profile {
    fn drop(&mut self) {
        let _ = self.handle.shutdown(Duration::from_secs(10));
    }
}
fn envelope(id: &str, command: BackendCommand) -> CommandEnvelope {
    CommandEnvelope {
        request_id: id.into(),
        origin: "lyrics-test".into(),
        expected_revision: None,
        command,
    }
}
fn wait(handle: &BackendHandle, condition: impl Fn(&BackendSnapshot) -> bool) -> BackendSnapshot {
    let deadline = Instant::now() + Duration::from_secs(12);
    loop {
        let state = handle.snapshot();
        if condition(&state) {
            return state;
        }
        assert!(
            Instant::now() < deadline,
            "Backend lyrics did not converge: {state:?}"
        );
        thread::sleep(Duration::from_millis(15));
    }
}
fn wait_operation(handle: &BackendHandle, cursor: u64, id: &str) {
    let deadline = Instant::now() + Duration::from_secs(12);
    loop {
        for event in handle.events_after(cursor).unwrap() {
            if let BackendEvent::OperationFinished {
                operation_id,
                success,
                error,
            } = event.payload
                && operation_id == id
            {
                assert!(success, "{error:?}");
                return;
            }
        }
        assert!(Instant::now() < deadline, "Operation {id} did not finish");
        thread::sleep(Duration::from_millis(15));
    }
}
fn wave(path: &Path) {
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

#[test]
fn actual_memory_playback_drives_sidecar_word_frames_pause_and_backwards_seek() {
    let profile = Profile::new();
    let id = profile.import(
        "timed",
        Some("[00:00.000]<0,1000>A\n[00:01.000]<0,1000>B\n[00:02.000]<0,1000>C"),
    );
    let cursor = profile.handle.event_cursor();
    profile.command("play", BackendCommand::Play { id: id.clone() });
    let state = wait(&profile.handle, |state| {
        state.playback.status == PlaybackStatus::Playing
            && state.lyrics.is_some()
            && state.lyric_frame.is_some()
    });
    assert_eq!(
        state.lyrics.as_ref().unwrap().provenance,
        LyricProvenance::LocalSidecar
    );
    assert_eq!(
        state.lyrics.as_ref().unwrap().generation,
        state.playback.generation
    );
    assert!(profile.handle.events_after(cursor).unwrap().iter().any(
        |event| matches!(&event.payload,BackendEvent::LyricChanged{track_id,..} if track_id==&id)
    ));
    assert!(
        profile
            .handle
            .lyrics(&id, Some(LyricOrigin::Raw))
            .unwrap()
            .is_some()
    );
    profile.command("pause", BackendCommand::Pause);
    wait(&profile.handle, |state| {
        state.playback.status == PlaybackStatus::Paused
    });
    profile.command("seek-forward", BackendCommand::Seek { seconds: 1.25 });
    let forward = wait(&profile.handle, |state| {
        state.lyric_frame.as_ref().is_some_and(|frame| {
            frame.seek_seq == state.playback.seek_seq
                && frame.line_index == Some(1)
                && frame.word_progress > 0.2
        })
    });
    let frame = forward.lyric_frame.unwrap();
    assert_eq!(frame.line_text, "B");
    assert!((frame.word_progress - 0.25).abs() < 0.05);
    thread::sleep(Duration::from_millis(120));
    assert_eq!(
        profile.handle.snapshot().lyric_frame.as_deref(),
        Some(frame.as_ref())
    );
    profile.command("seek-back", BackendCommand::Seek { seconds: 0.25 });
    let backwards = wait(&profile.handle, |state| {
        state.lyric_frame.as_ref().is_some_and(|frame| {
            frame.seek_seq > forward.playback.seek_seq && frame.line_index == Some(0)
        })
    });
    assert_eq!(backwards.lyric_frame.unwrap().line_text, "A");
    profile.command("stop", BackendCommand::Stop);
    let stopped = wait(&profile.handle, |state| {
        state.playback.status == PlaybackStatus::Idle
    });
    assert!(stopped.lyrics.is_none());
    assert!(stopped.lyric_frame.is_none());
}

#[test]
fn backend_edit_restore_clear_and_export_are_persistent_public_operations() {
    let profile = Profile::new();
    let id = profile.import("editable", Some("[00:00.000]original"));
    profile.command("play", BackendCommand::Play { id: id.clone() });
    wait(&profile.handle, |state| state.lyrics.is_some());
    let edited =
        LyricBundle::from_value(&json!({"lyric":"[00:00.000]edited","tlyric":null,"rlyric":""}))
            .unwrap();
    let before = profile.handle.snapshot().revision;
    profile.command(
        "save-edit",
        BackendCommand::SaveLyrics {
            id: id.clone(),
            bundle: edited.clone(),
        },
    );
    assert!(profile.handle.snapshot().revision > before);
    assert_eq!(
        profile
            .handle
            .lyrics(&id, Some(LyricOrigin::Edited))
            .unwrap(),
        Some(edited.clone())
    );
    wait(&profile.handle, |state| {
        state
            .lyrics
            .as_ref()
            .is_some_and(|lyrics| lyrics.provenance == LyricProvenance::Edited)
    });
    profile.command(
        "clear-raw",
        BackendCommand::ClearRawLyrics { id: id.clone() },
    );
    assert!(
        profile
            .handle
            .lyrics(&id, Some(LyricOrigin::Raw))
            .unwrap()
            .is_none()
    );
    assert_eq!(
        serde_json::from_str::<LyricBundle>(
            &profile
                .handle
                .export_lyrics(&id, LyricExportFormat::BundleJson)
                .unwrap()
                .text
        )
        .unwrap(),
        edited
    );
    profile.handle.shutdown(Duration::from_secs(10)).unwrap();
    let restarted = BackendHandle::start(BackendConfig::headless(
        profile.directory.path().join("profile"),
    ))
    .unwrap();
    assert_eq!(
        restarted.lyrics(&id, Some(LyricOrigin::Edited)).unwrap(),
        Some(edited)
    );
    restarted
        .execute(envelope(
            "restore",
            BackendCommand::RestoreLyrics { id: id.clone() },
        ))
        .unwrap();
    assert!(
        restarted
            .lyrics(&id, Some(LyricOrigin::Edited))
            .unwrap()
            .is_none()
    );
    restarted
        .execute(envelope(
            "play-restarted",
            BackendCommand::Play { id: id.clone() },
        ))
        .unwrap();
    let restored = wait(&restarted, |state| {
        state
            .lyrics
            .as_ref()
            .is_some_and(|lyrics| lyrics.bundle.lyric.text() == Some("[00:00.000]original"))
    });
    assert_eq!(restored.playback.track_id, Some(id));
    restarted.shutdown(Duration::from_secs(10)).unwrap();
}

#[test]
fn missing_lyrics_do_not_turn_a_successful_audio_playback_into_error() {
    let profile = Profile::new();
    let id = profile.import("without", None);
    profile.command("play", BackendCommand::Play { id: id.clone() });
    let state = wait(&profile.handle, |state| {
        state.playback.status == PlaybackStatus::Playing && state.lyrics_error.is_some()
    });
    assert!(state.playback.error.is_none());
    assert!(state.error.is_none());
    assert!(state.lyrics.is_none());
    assert!(profile.handle.lyrics(&id, None).unwrap().is_none());
    profile.command("volume", BackendCommand::Volume { volume: 0.3 });
    wait(&profile.handle, |state| {
        state.playback.volume == 0.3 && state.playback.status == PlaybackStatus::Playing
    });
}

#[test]
fn authorized_worker_returns_lyrics_via_opaque_dto_and_preserves_closure_between_requests() {
    let profile = Profile::new();
    let id = profile.import("from-source", None);
    profile.source("let calls=0;lx.on('request',async({action,info})=>{if(!info.musicInfo.filePath.startsWith('http://127.0.0.1:'))throw Error('private path escaped');if(action==='musicUrl')return info.musicInfo.filePath;return {lyric:'[00:00.000]source-'+(++calls),tlyric:null,rlyric:''};});lx.send('inited',{sources:{local:{type:'music',actions:['musicUrl','lyric'],qualitys:[]}}});");
    let cursor = profile.handle.event_cursor();
    profile.command("play", BackendCommand::Play { id: id.clone() });
    wait_operation(&profile.handle, cursor, "play");
    wait(&profile.handle, |state| {
        state
            .lyrics
            .as_ref()
            .is_some_and(|lyrics| lyrics.bundle.lyric.text() == Some("[00:00.000]source-1"))
    });
    profile.command(
        "clear-first-raw",
        BackendCommand::ClearRawLyrics { id: id.clone() },
    );
    let state = wait(&profile.handle, |state| {
        state
            .lyrics
            .as_ref()
            .is_some_and(|lyrics| lyrics.bundle.lyric.text() == Some("[00:00.000]source-2"))
    });
    assert_eq!(
        state.lyrics.unwrap().provenance,
        LyricProvenance::AuthorizedSource
    );
    assert!(state.playback.error.is_none());
    let cursor = profile.handle.event_cursor();
    profile.command(
        "reload-current",
        BackendCommand::LoadLyrics { id: id.clone() },
    );
    wait_operation(&profile.handle, cursor, "reload-current");
    let replay = profile
        .handle
        .execute(envelope(
            "reload-current",
            BackendCommand::LoadLyrics { id },
        ))
        .unwrap();
    assert_eq!(
        replay.state,
        lx_gpui_player::backend::ReceiptState::Completed
    );
}

struct SlowServer {
    url: String,
    connected: mpsc::Receiver<()>,
    release: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}
impl SlowServer {
    fn new() -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}/lyric", listener.local_addr().unwrap());
        let (tx, connected) = mpsc::sync_channel(1);
        let release = Arc::new(AtomicBool::new(false));
        let flag = release.clone();
        let thread = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(15);
            while !flag.load(Ordering::Acquire) && Instant::now() < deadline {
                if let Ok((mut stream, _)) = listener.accept() {
                    stream.set_nonblocking(false).unwrap();
                    let _ = stream.set_read_timeout(Some(Duration::from_secs(3)));
                    let mut request = [0u8; 4096];
                    let _ = stream.read(&mut request);
                    let _ = tx.send(());
                    while !flag.load(Ordering::Acquire) && Instant::now() < deadline {
                        thread::sleep(Duration::from_millis(10));
                    }
                    let body = r#"{"lyric":"[00:00.000]OLD"}"#;
                    let _ = write!(
                        stream,
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    break;
                }
                thread::sleep(Duration::from_millis(10));
            }
        });
        Self {
            url,
            connected,
            release,
            thread: Some(thread),
        }
    }
}
impl Drop for SlowServer {
    fn drop(&mut self) {
        self.release.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[test]
fn slow_source_lyric_cannot_block_switch_or_publish_into_new_track() {
    let profile = Profile::new();
    let a = profile.import("slow-a", None);
    let b = profile.import("ready-b", Some("[00:00.000]<0,1000>B"));
    let server = SlowServer::new();
    let script = format!(
        "lx.on('request',async({{action,info}})=>{{if(action==='musicUrl')return info.musicInfo.filePath;return new Promise((resolve,reject)=>lx.request({},{{}},(error,response,body)=>error?reject(error):resolve(body)));}});lx.send('inited',{{sources:{{local:{{type:'music',actions:['musicUrl','lyric'],qualitys:[]}}}}}});",
        serde_json::to_string(&server.url).unwrap()
    );
    profile.source(&script);
    profile.command("play-a", BackendCommand::Play { id: a });
    server
        .connected
        .recv_timeout(Duration::from_secs(8))
        .unwrap();
    let started = Instant::now();
    profile.command("play-b", BackendCommand::Play { id: b.clone() });
    assert!(started.elapsed() < Duration::from_secs(1));
    let state = wait(&profile.handle, |state| {
        state.playback.track_id.as_deref() == Some(&b)
            && state
                .lyrics
                .as_ref()
                .is_some_and(|lyrics| lyrics.document.lines[0].text == "B")
    });
    let generation = state.playback.generation;
    server.release.store(true, Ordering::Release);
    thread::sleep(Duration::from_millis(150));
    let after = profile.handle.snapshot();
    assert_eq!(after.playback.generation, generation);
    assert_eq!(
        after.lyrics.unwrap().bundle.lyric.text(),
        Some("[00:00.000]<0,1000>B")
    );
    assert!(after.playback.error.is_none());
}

#[test]
fn actor_snapshot_cursor_contains_committed_state_before_resuming_events() {
    let profile = Profile::new();
    profile.command("volume-before-view", BackendCommand::Volume { volume: 0.4 });
    let (snapshot, cursor) = profile.handle.snapshot_with_cursor().unwrap();
    assert_eq!(snapshot.revision, profile.handle.snapshot().revision);
    profile.command("volume-after-view", BackendCommand::Volume { volume: 0.5 });
    let next = profile.handle.events_after(cursor).unwrap();
    assert!(next.iter().all(|event| event.event_id > cursor));
    assert!(
        next.iter()
            .any(|event| event.repository_revision > snapshot.revision)
    );
}

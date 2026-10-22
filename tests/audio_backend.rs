use lx_gpui_player::audio::{AudioOutputMode, AudioPlayer, PlaybackSnapshot, PlaybackStatus};
use lx_gpui_player::catalog::Catalog;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::{Duration, Instant};

fn wait(player: &AudioPlayer, condition: impl Fn(&PlaybackSnapshot) -> bool) -> PlaybackSnapshot {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let snapshot = player.snapshot();
        if condition(&snapshot) {
            return snapshot;
        }
        assert!(
            Instant::now() < deadline,
            "audio wait timed out: {snapshot:?}"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn public_memory_output_renders_pcm_and_tracks_pause_seek_and_replacement() {
    assert_eq!(AudioOutputMode::default(), AudioOutputMode::Device);
    let catalog = Catalog::new().unwrap();
    let tracks = catalog.tracks();
    let player = AudioPlayer::with_output_mode(AudioOutputMode::Memory);
    player.load(&tracks[0].id, &tracks[0].source_url, false);
    let loaded = wait(&player, |s| s.status == PlaybackStatus::Paused);
    assert_eq!(loaded.generation, 1);
    assert_eq!(loaded.rate, 1.0);
    assert_eq!(loaded.seek_seq, 0);

    player.play();
    wait(&player, |s| {
        s.status == PlaybackStatus::Playing
            && s.position_seconds > 0.1
            && player.rendered_pcm().unwrap().peak > 0.01
    });
    player.pause();
    let paused = wait(&player, |s| s.status == PlaybackStatus::Paused);
    thread::sleep(Duration::from_millis(100));
    assert!((player.snapshot().position_seconds - paused.position_seconds).abs() < 0.05);

    player.seek(3.2);
    let sought = wait(&player, |s| {
        s.status == PlaybackStatus::Paused
            && s.seek_seq == 1
            && (s.position_seconds - 3.2).abs() < 0.08
    });
    assert_eq!(sought.generation, loaded.generation);
    player.load(&tracks[1].id, &tracks[1].source_url, true);
    let replaced = wait(&player, |s| {
        s.status == PlaybackStatus::Playing && s.track_id.as_deref() == Some(tracks[1].id.as_str())
    });
    assert!(replaced.generation > sought.generation);
    assert_eq!(replaced.seek_seq, sought.seek_seq);

    player.stop();
    let stopped = wait(&player, |s| {
        s.status == PlaybackStatus::Idle && s.track_id.is_none()
    });
    assert!(stopped.generation > replaced.generation);
    player.shutdown(Duration::from_secs(2)).unwrap();
    player.shutdown(Duration::ZERO).unwrap();
    let batches = player.rendered_pcm().unwrap().batches;
    thread::sleep(Duration::from_millis(60));
    assert_eq!(player.rendered_pcm().unwrap().batches, batches);
}

/// A project-owned HTTP endpoint that withholds headers or stalls its audio body.
struct StalledHttp {
    url: String,
    accepted: mpsc::Receiver<()>,
    release: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl StalledHttp {
    fn new(send_headers: bool) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let url = format!("http://{}/original.wav", listener.local_addr().unwrap());
        listener.set_nonblocking(true).unwrap();
        let release = Arc::new(AtomicBool::new(false));
        let server_release = release.clone();
        let (signal, accepted) = mpsc::sync_channel(1);
        let thread = thread::spawn(move || {
            let mut stream = loop {
                if server_release.load(Ordering::Acquire) {
                    return;
                }
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("fixture accept failed: {error}"),
                }
            };
            stream.set_nonblocking(false).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut request = [0_u8; 4096];
            let length = stream.read(&mut request).unwrap();
            assert!(length > 0);
            if send_headers {
                stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: audio/wav\r\nContent-Length: 400000\r\nConnection: close\r\n\r\n").unwrap();
            }
            signal.send(()).unwrap();
            while !server_release.load(Ordering::Acquire) {
                thread::sleep(Duration::from_millis(5));
            }
        });
        Self {
            url,
            accepted,
            release,
            thread: Some(thread),
        }
    }

    fn connected(&self) {
        self.accepted.recv_timeout(Duration::from_secs(3)).unwrap();
    }
}

impl Drop for StalledHttp {
    fn drop(&mut self) {
        self.release.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let result = thread.join();
            if !thread::panicking() {
                assert!(result.is_ok(), "HTTP fixture thread failed");
            }
        }
    }
}

#[test]
fn shutdown_cancels_stalled_headers_and_stalled_body_within_deadline() {
    for send_headers in [false, true] {
        let server = StalledHttp::new(send_headers);
        let player = AudioPlayer::memory();
        player.load("stalled-original", &server.url, true);
        server.connected();
        wait(&player, |s| s.status == PlaybackStatus::Loading);
        let started = Instant::now();
        player.shutdown(Duration::from_secs(2)).unwrap();
        assert!(started.elapsed() < Duration::from_secs(2));
        assert_eq!(player.snapshot().status, PlaybackStatus::Idle);
        assert!(player.snapshot().track_id.is_none());
    }
}

#[test]
fn stopped_stalled_load_cannot_replace_a_new_generation() {
    let server = StalledHttp::new(true);
    let catalog = Catalog::new().unwrap();
    let track = catalog.tracks()[0].clone();
    let player = AudioPlayer::memory();
    player.load("stalled-original", &server.url, true);
    server.connected();
    let loading = wait(&player, |s| s.status == PlaybackStatus::Loading);
    player.stop();
    let stopped = wait(&player, |s| s.status == PlaybackStatus::Idle);
    assert!(stopped.generation > loading.generation);
    player.load(&track.id, &track.source_url, true);
    let playing = wait(&player, |s| {
        s.status == PlaybackStatus::Playing && s.track_id.as_deref() == Some(track.id.as_str())
    });
    assert!(playing.generation > stopped.generation);
    player.shutdown(Duration::from_secs(2)).unwrap();
}

#[test]
fn real_eight_khz_pcm_can_seek_backward_while_paused_without_changing_track_generation() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("original-eight-khz.wav");
    let data_bytes = 8_000_u32 * 2 * 30;
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"RIFF");
    bytes.extend_from_slice(&(36 + data_bytes).to_le_bytes());
    bytes.extend_from_slice(b"WAVEfmt ");
    bytes.extend_from_slice(&16_u32.to_le_bytes());
    bytes.extend_from_slice(&1_u16.to_le_bytes());
    bytes.extend_from_slice(&1_u16.to_le_bytes());
    bytes.extend_from_slice(&8_000_u32.to_le_bytes());
    bytes.extend_from_slice(&16_000_u32.to_le_bytes());
    bytes.extend_from_slice(&2_u16.to_le_bytes());
    bytes.extend_from_slice(&16_u16.to_le_bytes());
    bytes.extend_from_slice(b"data");
    bytes.extend_from_slice(&data_bytes.to_le_bytes());
    bytes.resize(44 + data_bytes as usize, 0);
    std::fs::write(&path, bytes).unwrap();
    let catalog = Catalog::new().unwrap();
    let track = catalog.import_local(path).unwrap();
    let player = AudioPlayer::memory();
    player.set_volume(0.3);
    player.load(&track.id, &track.source_url, true);
    let playing = wait(&player, |s| {
        s.status == PlaybackStatus::Playing && s.position_seconds > 0.02
    });
    player.pause();
    wait(&player, |s| s.status == PlaybackStatus::Paused);
    for (seek_seq, seconds) in [(1, 1.25), (2, 0.25), (3, 2.5), (4, 0.0)] {
        player.seek(seconds);
        let sought = wait(&player, |s| {
            s.seek_seq == seek_seq && (s.position_seconds - seconds).abs() < 0.05
        });
        assert_eq!(sought.status, PlaybackStatus::Paused);
        assert_eq!(sought.generation, playing.generation);
        assert_eq!(sought.volume, 0.3);
        assert!(sought.error.is_none(), "seek failed: {sought:?}");
    }
    player.shutdown(Duration::from_secs(2)).unwrap();
}

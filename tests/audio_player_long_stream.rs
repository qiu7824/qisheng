//! A continuous 21-second range body stays valid through the real Memory player.
use lx_gpui_player::audio::{
    AudioOutputMode, AudioPlayer, PlaybackSnapshot, PlaybackStatus,
    cache::CacheOptions,
    transport::{ApprovedAudioRequest, CacheIdentity, TransportOptions},
};
use std::{
    fs,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

const BLOCK_BYTES: usize = 256 * 1024;
const RATE: u32 = 48_000;
const SLOW_CHUNKS: usize = 84;

struct ContinuousRange {
    url: String,
    first_sent: Arc<AtomicBool>,
    second_started: Arc<Mutex<Option<Instant>>>,
    second_finished: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}
impl ContinuousRange {
    fn new() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!(
            "http://{}/original-long.wav",
            listener.local_addr().unwrap()
        );
        let first_sent = Arc::new(AtomicBool::new(false));
        let second_started = Arc::new(Mutex::new(None));
        let second_finished = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = stop.clone();
        let first = first_sent.clone();
        let began = second_started.clone();
        let complete = second_finished.clone();
        let wave = Arc::new(original_wave());
        let thread = thread::spawn(move || {
            let mut clients = Vec::new();
            while !stopping.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((socket, _)) => {
                        let wave = wave.clone();
                        let first = first.clone();
                        let began = began.clone();
                        let complete = complete.clone();
                        let stopping = stopping.clone();
                        clients.push(thread::spawn(move || {
                            serve(socket, wave, first, began, complete, stopping)
                        }));
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(3))
                    }
                    Err(error) => panic!("fixture accept failed: {error}"),
                }
            }
            for client in clients {
                client.join().unwrap();
            }
        });
        Self {
            url,
            first_sent,
            second_started,
            second_finished,
            stop,
            thread: Some(thread),
        }
    }
}
impl Drop for ContinuousRange {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.thread.take().unwrap().join().unwrap();
    }
}
fn serve(
    mut socket: TcpStream,
    wave: Arc<Vec<u8>>,
    first: Arc<AtomicBool>,
    began: Arc<Mutex<Option<Instant>>>,
    complete: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
) {
    socket.set_nonblocking(false).unwrap();
    socket
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    socket
        .set_write_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    let mut headers = Vec::new();
    let mut scratch = [0; 1024];
    while !headers.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
        let Ok(count) = socket.read(&mut scratch) else {
            return;
        };
        if count == 0 {
            return;
        }
        headers.extend_from_slice(&scratch[..count]);
        if headers.len() > 8192 {
            return;
        }
    }
    let headers = String::from_utf8_lossy(&headers);
    let range = headers.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        if !name.eq_ignore_ascii_case("range") {
            return None;
        }
        let (start, end) = value.trim().strip_prefix("bytes=")?.split_once('-')?;
        Some((start.parse::<usize>().ok()?, end.parse::<usize>().ok()?))
    });
    let Some((start, requested_end)) = range else {
        return;
    };
    if start >= wave.len() {
        return;
    }
    let end = requested_end.min(wave.len() - 1);
    let response = format!(
        "HTTP/1.1 206 Partial Content\r\nContent-Type: audio/wav\r\nContent-Length: {}\r\nContent-Range: bytes {start}-{end}/{}\r\nAccept-Ranges: bytes\r\nETag: \"original-long-v1\"\r\nConnection: close\r\n\r\n",
        end - start + 1,
        wave.len()
    );
    if socket.write_all(response.as_bytes()).is_err() {
        return;
    }
    if start == BLOCK_BYTES && !complete.load(Ordering::Acquire) {
        {
            let mut started = began.lock().unwrap();
            if started.is_none() {
                *started = Some(Instant::now());
            }
        }
        let body = &wave[start..=end];
        for index in 0..SLOW_CHUNKS {
            thread::sleep(Duration::from_millis(250));
            if stop.load(Ordering::Acquire) {
                return;
            }
            let low = body.len() * index / SLOW_CHUNKS;
            let high = body.len() * (index + 1) / SLOW_CHUNKS;
            if socket.write_all(&body[low..high]).is_err() {
                return;
            }
        }
        complete.store(true, Ordering::Release);
    } else {
        if socket.write_all(&wave[start..=end]).is_err() {
            return;
        }
        if start == 0 {
            first.store(true, Ordering::Release);
        }
    }
}
fn original_wave() -> Vec<u8> {
    let frames = RATE * 35;
    let size = frames * 2;
    let mut bytes = Vec::with_capacity(size as usize + 44);
    bytes.extend(b"RIFF");
    bytes.extend((size + 36).to_le_bytes());
    bytes.extend(b"WAVEfmt ");
    bytes.extend(16u32.to_le_bytes());
    bytes.extend(1u16.to_le_bytes());
    bytes.extend(1u16.to_le_bytes());
    bytes.extend(RATE.to_le_bytes());
    bytes.extend((RATE * 2).to_le_bytes());
    bytes.extend(2u16.to_le_bytes());
    bytes.extend(16u16.to_le_bytes());
    bytes.extend(b"data");
    bytes.extend(size.to_le_bytes());
    for frame in 0..frames {
        let frequency = if frame < RATE * 25 { 440.0 } else { 880.0 };
        let sample = ((frame as f64 * frequency * std::f64::consts::TAU / RATE as f64).sin()
            * 6000.0) as i16;
        bytes.extend(sample.to_le_bytes());
    }
    bytes
}
fn wait(
    player: &AudioPlayer,
    seconds: u64,
    ready: impl Fn(&PlaybackSnapshot) -> bool,
) -> PlaybackSnapshot {
    let deadline = Instant::now() + Duration::from_secs(seconds);
    loop {
        let snapshot = player.snapshot();
        assert_ne!(
            snapshot.status,
            PlaybackStatus::Error,
            "player failed: {:?}",
            snapshot.error
        );
        if ready(&snapshot) {
            return snapshot;
        }
        assert!(
            Instant::now() < deadline,
            "player state did not converge: {snapshot:?}"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn continuous_second_block_outlasts_twenty_seconds_then_resumes_content_clock_and_tail_pcm() {
    let root = std::env::temp_dir().join("lx-native-player-long-stream-tests");
    fs::create_dir_all(&root).unwrap();
    let directory = tempfile::Builder::new()
        .prefix("cache-")
        .tempdir_in(root)
        .unwrap();
    let fixture = ContinuousRange::new();
    let player = AudioPlayer::with_transport_options(
        AudioOutputMode::Memory,
        TransportOptions {
            cache: CacheOptions {
                root: directory.path().join("audio-cache"),
                block_size: BLOCK_BYTES,
                ram_bytes: 512 * 1024,
                disk_bytes: 8 * 1024 * 1024,
                max_objects: 4,
                max_blocks: 32,
            },
            connect_timeout: Duration::from_secs(2),
            header_timeout: Duration::from_secs(3),
            idle_timeout: Duration::from_secs(20),
            max_retries: 0,
            sequential_lead_blocks: 2,
            range_lookahead_blocks: 2,
        },
    );
    let request = ApprovedAudioRequest::new(
        &fixture.url,
        CacheIdentity {
            track_id: "owned-continuous-range".into(),
            platform: "local".into(),
            actual_quality: "local".into(),
            approval_id: "owned-original-fixture".into(),
            ..Default::default()
        },
    )
    .unwrap();
    player.load_approved(request, true);
    let first_pcm = wait(&player, 6, |state| {
        state.status == PlaybackStatus::Playing
            && state.position_seconds > 0.15
            && player.rendered_pcm().is_some_and(|pcm| pcm.peak > 0.01)
    });
    assert!(fixture.first_sent.load(Ordering::Acquire));
    assert!(
        !fixture.second_finished.load(Ordering::Acquire),
        "first PCM must precede completion of the slow second block"
    );
    assert!(first_pcm.duration_known && (first_pcm.duration_seconds - 35.0).abs() < 0.02);
    let buffering_deadline = Instant::now() + Duration::from_secs(6);
    let buffered = loop {
        let candidate = player.snapshot();
        assert_ne!(candidate.status, PlaybackStatus::Error);
        if candidate.status == PlaybackStatus::Buffering
            && candidate.buffering
            && candidate.position_seconds > 2.0
        {
            thread::sleep(Duration::from_millis(150));
            let confirmed = player.snapshot();
            if confirmed.status == PlaybackStatus::Buffering
                && (confirmed.position_seconds - candidate.position_seconds).abs() < 0.02
            {
                break confirmed;
            }
        }
        assert!(
            Instant::now() < buffering_deadline,
            "player did not enter sustained content underflow"
        );
        thread::sleep(Duration::from_millis(10));
    };
    let frozen_at = buffered.position_seconds;
    let generation = buffered.generation;
    let stall_deadline = Instant::now() + Duration::from_secs(24);
    let mut frozen_samples = 0;
    while !fixture.second_finished.load(Ordering::Acquire) {
        let snapshot = player.snapshot();
        assert_ne!(
            snapshot.status,
            PlaybackStatus::Error,
            "continuous body failed after a total-request deadline: {:?}",
            snapshot.error
        );
        assert_eq!(snapshot.generation, generation);
        if snapshot.status == PlaybackStatus::Buffering {
            assert!(
                (snapshot.position_seconds - frozen_at).abs() < 0.02,
                "underflow must not advance the content clock"
            );
            frozen_samples += 1;
        }
        assert!(
            Instant::now() < stall_deadline,
            "second block did not finish its continuous body"
        );
        thread::sleep(Duration::from_millis(50));
    }
    assert!(
        frozen_samples >= 200,
        "the actual player must stay responsive through a prolonged underflow"
    );
    assert!(fixture.second_started.lock().unwrap().unwrap().elapsed() > Duration::from_secs(20));
    let resumed = wait(&player, 6, |state| {
        state.status == PlaybackStatus::Playing && state.position_seconds > frozen_at + 0.15
    });
    assert_eq!(resumed.generation, generation);
    assert!(!resumed.buffering && resumed.error.is_none());
    player.pause();
    let paused = wait(&player, 3, |state| state.status == PlaybackStatus::Paused);
    player.seek(30.0);
    let sought = wait(&player, 6, |state| {
        state.status == PlaybackStatus::Paused
            && !state.seek_pending
            && state.seek_seq > paused.seek_seq
            && (state.position_seconds - 30.0).abs() < 0.05
    });
    assert_eq!(sought.generation, generation);
    player.play();
    wait(&player, 6, |state| {
        state.status == PlaybackStatus::Playing
            && state.position_seconds > 30.15
            && player.rendered_pcm().is_some_and(|pcm| {
                pcm.frequency_hz
                    .is_some_and(|frequency| (frequency - 880.0).abs() < 30.0)
            })
    });
    let stopped = Instant::now();
    player.stop();
    wait(&player, 3, |state| {
        state.status == PlaybackStatus::Idle && state.track_id.is_none()
    });
    player.shutdown(Duration::from_secs(5)).unwrap();
    assert!(stopped.elapsed() < Duration::from_secs(5));
}

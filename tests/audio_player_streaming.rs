use lx_gpui_player::audio::{AudioPlayer, PlaybackStatus, transport::TransportOptions};
use std::{
    io::{Read, Write},
    net::TcpListener,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread,
    time::{Duration, Instant},
};
struct WaveServer {
    url: String,
    stop: Arc<AtomicBool>,
    sent: Arc<AtomicU64>,
    ranges: Arc<Mutex<Vec<(u64, u64)>>>,
    thread: Option<thread::JoinHandle<()>>,
    length: u64,
}
impl WaveServer {
    fn new() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}/large.wav", listener.local_addr().unwrap());
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let sent = Arc::new(AtomicU64::new(0));
        let counted = sent.clone();
        let ranges = Arc::new(Mutex::new(Vec::new()));
        let requests = ranges.clone();
        let data_bytes = 68 * 1024 * 1024u64;
        let length = data_bytes + 44;
        let thread = thread::spawn(move || {
            let mut clients = Vec::new();
            while !stopped.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((mut socket, _)) => {
                        let stopped = stopped.clone();
                        let counted = counted.clone();
                        let requests = requests.clone();
                        clients.push(thread::spawn(move||{socket.set_nonblocking(false).unwrap();socket.set_read_timeout(Some(Duration::from_secs(2))).unwrap();socket.set_write_timeout(Some(Duration::from_secs(2))).unwrap();let mut raw=Vec::new();let mut buf=[0u8;1024];while !raw.windows(4).any(|x|x==b"\r\n\r\n"){let n=match socket.read(&mut buf){Ok(0)|Err(_)=>return,Ok(n)=>n};raw.extend_from_slice(&buf[..n]);if raw.len()>16*1024{return;}}
        let text=String::from_utf8(raw).unwrap();let range=text.lines().find_map(|line|line.to_ascii_lowercase().strip_prefix("range: bytes=").map(str::to_string));let(start,end)=range.map(|r|{let(a,b)=r.split_once('-').unwrap();(a.parse::<u64>().unwrap(),b.parse::<u64>().unwrap_or(length-1).min(length-1))}).unwrap_or((0,length-1));requests.lock().unwrap().push((start,end));
        if start>=length {let _=write!(socket,"HTTP/1.1 416 Range Not Satisfiable\r\nContent-Range: bytes */{length}\r\nContent-Length: 0\r\nETag: \"original-v1\"\r\nConnection: close\r\n\r\n");return;}
        let _=write!(socket,"HTTP/1.1 206 Partial Content\r\nContent-Type: audio/wav\r\nContent-Range: bytes {start}-{end}/{length}\r\nContent-Length: {}\r\nETag: \"original-v1\"\r\nConnection: close\r\n\r\n",end-start+1);
        let mut offset=start;while offset<=end&&!stopped.load(Ordering::Acquire){let n=(end-offset+1).min(16*1024)as usize;let body=wave_bytes(offset,n,data_bytes);if socket.write_all(&body).is_err(){return;}counted.fetch_add(n as u64,Ordering::AcqRel);offset+=n as u64;thread::sleep(Duration::from_millis(2));}
        }));
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2))
                    }
                    Err(_) => break,
                }
            }
            for client in clients {
                client.join().unwrap();
            }
        });
        Self {
            url,
            stop,
            sent,
            ranges,
            thread: Some(thread),
            length,
        }
    }
}
impl Drop for WaveServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.thread.take().unwrap().join().unwrap();
    }
}
fn wave_bytes(offset: u64, count: usize, data_bytes: u64) -> Vec<u8> {
    let rate = 48_000u32;
    let mut header = Vec::new();
    header.extend(b"RIFF");
    header.extend((data_bytes as u32 + 36).to_le_bytes());
    header.extend(b"WAVEfmt ");
    header.extend(16u32.to_le_bytes());
    header.extend(1u16.to_le_bytes());
    header.extend(1u16.to_le_bytes());
    header.extend(rate.to_le_bytes());
    header.extend((rate * 2).to_le_bytes());
    header.extend(2u16.to_le_bytes());
    header.extend(16u16.to_le_bytes());
    header.extend(b"data");
    header.extend((data_bytes as u32).to_le_bytes());
    (offset..offset + count as u64)
        .map(|position| {
            if position < 44 {
                return header[position as usize];
            }
            let byte = position - 44;
            let frame = byte / 2;
            let frequency = if frame < data_bytes / 2 / 3 {
                440.0
            } else if frame < data_bytes / 2 * 2 / 3 {
                660.0
            } else {
                880.0
            };
            let sample = ((frame as f64 / rate as f64 * frequency * std::f64::consts::TAU).sin()
                * 6000.0) as i16;
            sample.to_le_bytes()[(byte % 2) as usize]
        })
        .collect()
}
fn wait(
    player: &AudioPlayer,
    predicate: impl Fn(&lx_gpui_player::audio::PlaybackSnapshot) -> bool,
) {
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        let state = player.snapshot();
        if predicate(&state) {
            return;
        }
        assert!(Instant::now() < deadline, "state timeout: {state:?}");
        thread::sleep(Duration::from_millis(5));
    }
}
fn frequency(player: &AudioPlayer, target: f64) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let pcm = player.rendered_pcm().unwrap();
        if pcm.frequency_hz.is_some_and(|f| (f - target).abs() < 25.0) && pcm.peak > 0.01 {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "frequency target {target}: {pcm:?}, state {:?}",
            player.snapshot()
        );
        thread::sleep(Duration::from_millis(10));
    }
}
#[test]
fn actual_player_streams_large_wave_before_transfer_finishes_and_seeks_correct_content_to_eof() {
    let server = WaveServer::new();
    let directory = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
    let mut options = TransportOptions::default();
    options.cache.root = directory.path().join("managed-cache");
    options.cache.disk_bytes = 2 * 1024 * 1024;
    options.cache.ram_bytes = 512 * 1024;
    let player = AudioPlayer::with_transport_options(
        lx_gpui_player::audio::AudioOutputMode::Memory,
        options,
    );
    player.load("large-owned-wave", &server.url, true);
    wait(&player, |state| {
        matches!(
            state.status,
            PlaybackStatus::Playing | PlaybackStatus::Buffering
        )
    });
    frequency(&player, 440.0);
    assert!(
        server.sent.load(Ordering::Acquire) < server.length / 8,
        "Whole media downloaded before first PCM"
    );
    let duration = player.snapshot().duration_seconds;
    assert!(duration > 600.0 && player.snapshot().duration_known);
    player.seek(duration / 2.0);
    wait(&player, |state| state.seek_seq >= 1 && !state.seek_pending);
    frequency(&player, 660.0);
    player.pause();
    wait(&player, |state| state.status == PlaybackStatus::Paused);
    let seq = player.snapshot().seek_seq;
    player.seek(duration - 1.0);
    wait(&player, |state| {
        state.seek_seq > seq && !state.seek_pending && state.status == PlaybackStatus::Paused
    });
    let paused = player.snapshot().position_seconds;
    thread::sleep(Duration::from_millis(80));
    assert!((player.snapshot().position_seconds - paused).abs() < 0.005);
    player.play();
    frequency(&player, 880.0);
    wait(&player, |state| state.status == PlaybackStatus::Ended);
    assert!((player.snapshot().position_seconds - duration).abs() < 0.001);
    assert!(
        server
            .ranges
            .lock()
            .unwrap()
            .iter()
            .any(|(start, _)| *start > 32 * 1024 * 1024)
    );
    let stats = player.cache_stats().unwrap();
    assert!(stats.disk_bytes <= 2 * 1024 * 1024 && stats.ram_bytes <= 512 * 1024);
    player.seek(0.2);
    wait(&player, |state| {
        state.status == PlaybackStatus::Paused && !state.seek_pending
    });
    player.play();
    frequency(&player, 440.0);
    player.stop();
    wait(&player, |state| state.status == PlaybackStatus::Idle);
    player.shutdown(Duration::from_secs(5)).unwrap();
}

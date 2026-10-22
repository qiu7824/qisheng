use lx_gpui_player::audio::cache::CacheOptions;
use lx_gpui_player::audio::transport::{
    ApprovedAudioRequest, AudioTransport, CacheIdentity, MediaIoErrorKind, TransportOptions,
};
use lx_gpui_player::audio::{
    AudioOutputMode, AudioPlayer, PlaybackFailureStage, PlaybackSnapshot, PlaybackStatus,
    decode::DecodeErrorKind,
};
use std::io::{Read, Seek, SeekFrom, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use symphonia::core::io::MediaSource;

#[path = "support/lookahead_tests.rs"]
mod lookahead_tests;
#[path = "support/publication_eof_tests.rs"]
mod publication_eof_tests;

#[derive(Clone, Debug)]
struct Request {
    range: String,
    if_range: Option<String>,
}

struct Loopback {
    url: String,
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl Loopback {
    fn start(
        handler: impl Fn(Request, &mut TcpStream, &AtomicBool) + Send + Sync + 'static,
    ) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/audio", listener.local_addr().unwrap());
        listener.set_nonblocking(true).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let handler = Arc::new(handler);
        let thread = thread::spawn(move || {
            let mut clients = Vec::new();
            while !flag.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((mut socket, _)) => {
                        // Windows accepted sockets inherit listener nonblocking mode.
                        socket.set_nonblocking(false).unwrap();
                        socket
                            .set_read_timeout(Some(Duration::from_secs(1)))
                            .unwrap();
                        socket
                            .set_write_timeout(Some(Duration::from_millis(250)))
                            .unwrap();
                        let handler = handler.clone();
                        let stop = flag.clone();
                        clients.push(thread::spawn(move || {
                            let mut header = Vec::new();
                            let mut byte = [0];
                            while header.len() < 8192 && !header.ends_with(b"\r\n\r\n") {
                                if socket.read_exact(&mut byte).is_err() {
                                    return;
                                }
                                header.push(byte[0]);
                            }
                            let header = String::from_utf8(header).unwrap();
                            let find = |name: &str| {
                                header
                                    .lines()
                                    .filter_map(|line| line.split_once(':'))
                                    .find(|(key, _)| key.eq_ignore_ascii_case(name))
                                    .map(|(_, value)| value.trim().to_owned())
                            };
                            handler(
                                Request {
                                    range: find("range").unwrap_or_default(),
                                    if_range: find("if-range"),
                                },
                                &mut socket,
                                &stop,
                            );
                        }));
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2))
                    }
                    Err(error) => panic!("loopback accept: {error}"),
                }
            }
            for client in clients {
                client.join().unwrap();
            }
        });
        Self {
            url,
            stop,
            thread: Some(thread),
        }
    }
}

impl Drop for Loopback {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            thread.join().unwrap();
        }
    }
}

fn options(directory: &tempfile::TempDir) -> TransportOptions {
    TransportOptions {
        cache: CacheOptions {
            root: directory.path().join("encoded"),
            block_size: 1024,
            ram_bytes: 2048,
            disk_bytes: 32 * 1024,
            max_objects: 4,
            max_blocks: 4,
        },
        connect_timeout: Duration::from_secs(1),
        header_timeout: Duration::from_secs(2),
        idle_timeout: Duration::from_secs(1),
        max_retries: 0,
        sequential_lead_blocks: 1,
        range_lookahead_blocks: 0,
    }
}

fn interval(request: &Request, total: u64) -> (u64, u64) {
    let text = request.range.strip_prefix("bytes=").unwrap();
    let (start, end) = text.split_once('-').unwrap();
    let start = start.parse::<u64>().unwrap();
    let end = if end.is_empty() {
        total.saturating_sub(1)
    } else {
        end.parse::<u64>().unwrap().min(total.saturating_sub(1))
    };
    (start, end)
}

fn data(start: u64, length: usize, salt: u8) -> Vec<u8> {
    (0..length)
        .map(|index| ((start + index as u64) % 251) as u8 ^ salt)
        .collect()
}

fn headers(socket: &mut TcpStream, status: &str, values: &str) {
    let _ = write!(
        socket,
        "HTTP/1.1 {status}\r\nConnection: close\r\n{values}\r\n"
    );
}

fn range_reply(request: &Request, socket: &mut TcpStream, total: u64, etag: &str, salt: u8) {
    let (start, end) = interval(request, total);
    if start >= total {
        headers(
            socket,
            "416 Range Not Satisfiable",
            &format!("Content-Range: bytes */{total}\r\nContent-Length: 0\r\nETag: {etag}\r\n"),
        );
        return;
    }
    let length = (end - start + 1) as usize;
    headers(
        socket,
        "206 Partial Content",
        &format!(
            "Content-Range: bytes {start}-{end}/{total}\r\nContent-Length: {length}\r\nETag: {etag}\r\n"
        ),
    );
    let _ = socket.write_all(&data(start, length, salt));
}

fn open(
    transport: &Arc<AudioTransport>,
    server: &Loopback,
) -> lx_gpui_player::audio::transport::StreamLease {
    transport
        .open(
            ApprovedAudioRequest::approved_url(&server.url).unwrap(),
            Arc::new(AtomicBool::new(false)),
        )
        .unwrap()
}

fn wait_playback(
    player: &AudioPlayer,
    condition: impl Fn(&PlaybackSnapshot) -> bool,
) -> PlaybackSnapshot {
    let started = Instant::now();
    loop {
        let snapshot = player.snapshot();
        if condition(&snapshot) {
            return snapshot;
        }
        assert!(started.elapsed() < Duration::from_secs(5), "{snapshot:?}");
        thread::sleep(Duration::from_millis(2));
    }
}

fn failure_wave() -> Vec<u8> {
    let rate = 8_000_u32;
    let data_bytes = rate * 2 * 4;
    let mut wave = Vec::new();
    wave.extend(b"RIFF");
    wave.extend((data_bytes + 36).to_le_bytes());
    wave.extend(b"WAVEfmt ");
    wave.extend(16_u32.to_le_bytes());
    wave.extend(1_u16.to_le_bytes());
    wave.extend(1_u16.to_le_bytes());
    wave.extend(rate.to_le_bytes());
    wave.extend((rate * 2).to_le_bytes());
    wave.extend(2_u16.to_le_bytes());
    wave.extend(16_u16.to_le_bytes());
    wave.extend(b"data");
    wave.extend(data_bytes.to_le_bytes());
    for frame in 0..rate * 4 {
        let sample = (std::f64::consts::TAU * 440.0 * f64::from(frame) / f64::from(rate)).sin();
        wave.extend(((sample * 6_000.0) as i16).to_le_bytes());
    }
    wave
}

#[test]
fn memory_player_retains_real_http_status_and_only_exact_rejections_require_url_refresh() {
    for status in [401_u16, 403, 404, 410, 429, 500] {
        let server = Loopback::start(move |_, socket, _| {
            headers(
                socket,
                &format!("{status} Rejected"),
                "Content-Length: 0\r\n",
            );
        });
        let directory = tempfile::tempdir().unwrap();
        let player =
            AudioPlayer::with_transport_options(AudioOutputMode::Memory, options(&directory));
        let request = ApprovedAudioRequest::new(
            &server.url,
            CacheIdentity {
                track_id: "http-rejection".into(),
                ..Default::default()
            },
        )
        .unwrap();
        let expected_url = request.identity.url_sha256.clone();
        player.load_approved(request, true);
        let failed = wait_playback(&player, |snapshot| snapshot.status == PlaybackStatus::Error);
        let failure = failed.failure.as_ref().unwrap();
        assert_eq!(failure.stage, PlaybackFailureStage::Transport);
        assert_eq!(failure.decode_kind, Some(DecodeErrorKind::Network));
        let media = failure.media.as_ref().unwrap();
        assert_eq!(media.kind, MediaIoErrorKind::Network);
        assert_eq!(media.http_status, Some(status));
        assert_eq!(
            failure.request_url_sha256.as_deref(),
            Some(expected_url.as_str())
        );
        assert_eq!(
            failure.requires_url_refresh(),
            matches!(status, 401 | 403 | 404 | 410)
        );
        assert!(failed.error.is_some());
        let serialized = serde_json::to_value(&failed).unwrap();
        assert_eq!(serialized["failure"]["stage"], "transport");
        assert_eq!(serialized["failure"]["media"]["http_status"], status);
        player.clear_error();
        wait_playback(&player, |snapshot| {
            snapshot.error.is_none() && snapshot.failure.is_none()
        });
        player.stop();
        let stopped = wait_playback(&player, |snapshot| snapshot.status == PlaybackStatus::Idle);
        assert!(stopped.generation > failed.generation);
        assert!(stopped.failure.is_none());
        player.shutdown(Duration::from_secs(2)).unwrap();
    }
}

#[test]
fn memory_player_protocol_and_header_timeout_failures_are_distinct_from_url_rejection() {
    for protocol in [true, false] {
        let server = Loopback::start(move |_, socket, stop| {
            if protocol {
                headers(
                    socket,
                    "200 OK",
                    "Content-Encoding: gzip\r\nContent-Length: 0\r\n",
                );
            } else {
                while !stop.load(Ordering::Acquire) {
                    thread::sleep(Duration::from_millis(2));
                }
            }
        });
        let directory = tempfile::tempdir().unwrap();
        let mut configured = options(&directory);
        configured.header_timeout = if protocol {
            Duration::from_secs(2)
        } else {
            Duration::from_millis(40)
        };
        let player = AudioPlayer::with_transport_options(AudioOutputMode::Memory, configured);
        player.load("typed-transport-failure", &server.url, true);
        let snapshot = wait_playback(&player, |snapshot| snapshot.status == PlaybackStatus::Error);
        let failure = snapshot.failure.as_ref().unwrap();
        let media = failure.media.as_ref().unwrap();
        assert_eq!(
            media.kind,
            if protocol {
                MediaIoErrorKind::Protocol
            } else {
                MediaIoErrorKind::Timeout
            }
        );
        assert_eq!(
            failure.decode_kind,
            Some(if protocol {
                DecodeErrorKind::Protocol
            } else {
                DecodeErrorKind::Timeout
            })
        );
        assert_eq!(media.http_status, None);
        assert!(!failure.requires_url_refresh());
        player.shutdown(Duration::from_secs(2)).unwrap();
    }
}

#[test]
fn real_cache_initialization_failure_remains_storage_without_url_refresh() {
    let directory = tempfile::tempdir().unwrap();
    let mut configured = options(&directory);
    std::fs::write(
        &configured.cache.root,
        b"a real file cannot be a cache directory",
    )
    .unwrap();
    configured.header_timeout = Duration::from_secs(2);
    let player = AudioPlayer::with_transport_options(AudioOutputMode::Memory, configured);
    player.load(
        "real-storage-failure",
        "http://127.0.0.1:9/original.wav",
        true,
    );
    let snapshot = wait_playback(&player, |snapshot| snapshot.status == PlaybackStatus::Error);
    let failure = snapshot.failure.as_ref().unwrap();
    assert_eq!(failure.stage, PlaybackFailureStage::Transport);
    let media = failure.media.as_ref().unwrap();
    assert_eq!(media.kind, MediaIoErrorKind::Storage);
    assert_eq!(media.http_status, None);
    assert!(!failure.requires_url_refresh());
    player.shutdown(Duration::from_secs(2)).unwrap();
}

#[test]
fn running_http_rejection_retains_identity_and_new_load_clears_the_old_failure() {
    let wave = Arc::new(failure_wave());
    let media = wave.clone();
    let release = Arc::new(AtomicBool::new(false));
    let gate = release.clone();
    let first = Arc::new(AtomicBool::new(false));
    let served = first.clone();
    let server = Loopback::start(move |request, socket, stop| {
        if request.range == "bytes=0-16383" && !served.swap(true, Ordering::AcqRel) {
            headers(
                socket,
                "206 Partial Content",
                &format!(
                    "Content-Range: bytes 0-16383/{}\r\nContent-Length: 16384\r\nETag: \"typed-original\"\r\n",
                    media.len()
                ),
            );
            let _ = socket.write_all(&media[..16_384]);
        } else {
            while !gate.load(Ordering::Acquire) && !stop.load(Ordering::Acquire) {
                thread::sleep(Duration::from_millis(2));
            }
            headers(socket, "403 Forbidden", "Content-Length: 0\r\n");
        }
    });
    let directory = tempfile::tempdir().unwrap();
    let mut configured = options(&directory);
    configured.cache.block_size = 16_384;
    configured.cache.ram_bytes = 32_768;
    configured.cache.disk_bytes = 128 * 1024;
    configured.cache.max_blocks = 8;
    configured.header_timeout = Duration::from_secs(60);
    let player = AudioPlayer::with_transport_options(AudioOutputMode::Memory, configured);
    let request = ApprovedAudioRequest::new(
        &server.url,
        CacheIdentity {
            track_id: "running-original".into(),
            ..Default::default()
        },
    )
    .unwrap();
    let expected_url = request.identity.url_sha256.clone();
    player.load_approved(request, false);
    let ready = wait_playback(&player, |snapshot| {
        snapshot.status == PlaybackStatus::Paused
    });
    assert!(ready.failure.is_none());
    release.store(true, Ordering::Release);
    player.play();
    let failed = wait_playback(&player, |snapshot| snapshot.status == PlaybackStatus::Error);
    assert_eq!(failed.generation, ready.generation);
    let failure = failed.failure.as_ref().unwrap();
    assert_eq!(failure.media.as_ref().unwrap().http_status, Some(403));
    assert_eq!(
        failure.request_url_sha256.as_deref(),
        Some(expected_url.as_str())
    );
    assert!(failure.requires_url_refresh());
    let good_wave = wave.clone();
    let good = Loopback::start(move |request, socket, _| {
        let (start, end) = interval(&request, good_wave.len() as u64);
        headers(
            socket,
            "206 Partial Content",
            &format!(
                "Content-Range: bytes {start}-{end}/{}\r\nContent-Length: {}\r\nETag: \"recovered-original\"\r\n",
                good_wave.len(),
                end - start + 1
            ),
        );
        let _ = socket.write_all(&good_wave[start as usize..=end as usize]);
    });
    player.load("recovered-original", &good.url, false);
    let recovered = wait_playback(&player, |snapshot| {
        snapshot.status == PlaybackStatus::Paused
            && snapshot.track_id.as_deref() == Some("recovered-original")
    });
    assert!(recovered.generation > failed.generation);
    assert!(recovered.error.is_none());
    assert!(recovered.failure.is_none());
    player.shutdown(Duration::from_secs(2)).unwrap();
}

#[test]
fn real_ranges_seek_tail_and_send_strong_if_range() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let record = seen.clone();
    let server = Loopback::start(move |request, socket, _| {
        record.lock().unwrap().push(request.clone());
        range_reply(&request, socket, 4099, "\"v1\"", 0);
    });
    let directory = tempfile::tempdir().unwrap();
    let transport = AudioTransport::new(options(&directory)).unwrap();
    let mut lease = open(&transport, &server);
    lease.control.wait_ready(Duration::from_secs(2)).unwrap();
    assert!(lease.reader.is_seekable());
    assert_eq!(lease.reader.byte_len(), Some(4099));
    let mut first = [0; 32];
    lease.reader.read_exact(&mut first).unwrap();
    assert_eq!(first.as_slice(), data(0, 32, 0));
    lease.reader.seek(SeekFrom::End(-32)).unwrap();
    let mut tail = [0; 32];
    lease.reader.read_exact(&mut tail).unwrap();
    assert_eq!(tail.as_slice(), data(4067, 32, 0));
    assert_eq!(lease.reader.read(&mut first).unwrap(), 0);
    let seen = seen.lock().unwrap();
    assert_eq!(seen[0].range, "bytes=0-1023");
    assert!(
        seen.iter()
            .skip(1)
            .all(|request| request.if_range.as_deref() == Some("\"v1\""))
    );
    assert!(
        seen.iter()
            .any(|request| request.range == "bytes=4096-5119")
    );
    lease.control.cancel();
    assert!(lease.control.join(Duration::from_secs(1)));
}

#[test]
fn shutdown_during_stalled_headers_finishes_aborted_streams_and_joins_runtime_and_cache() {
    let accepted = Arc::new(AtomicBool::new(false));
    let pending = accepted.clone();
    let server = Loopback::start(move |_, _, stop| {
        pending.store(true, Ordering::Release);
        while !stop.load(Ordering::Acquire) {
            thread::sleep(Duration::from_millis(2));
        }
    });
    let directory = tempfile::tempdir().unwrap();
    let mut configured = options(&directory);
    configured.header_timeout = Duration::from_secs(60);
    let transport = AudioTransport::new(configured).unwrap();
    let mut lease = open(&transport, &server);
    let started = Instant::now();
    while !accepted.load(Ordering::Acquire) {
        assert!(started.elapsed() < Duration::from_secs(5));
        thread::sleep(Duration::from_millis(1));
    }
    assert!(!lease.control.join(Duration::ZERO));
    let started = Instant::now();
    let _ = transport.shutdown(Duration::ZERO);
    assert!(started.elapsed() < Duration::from_millis(100));
    assert!(transport.shutdown(Duration::from_secs(5)));
    assert!(lease.control.join(Duration::ZERO));
    assert!(transport.cache().stats().worker_finished);
    let error = lease.reader.read(&mut [0; 1]).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::Interrupted);
    let typed = error
        .get_ref()
        .and_then(|source| source.downcast_ref::<lx_gpui_player::audio::transport::MediaIoError>())
        .unwrap();
    assert_eq!(typed.kind, MediaIoErrorKind::Cancelled);
}

#[test]
fn source_larger_than_sixty_four_mib_reads_without_whole_file_loading() {
    let total = 96 * 1024 * 1024 + 17;
    let server = Loopback::start(move |request, socket, _| {
        range_reply(&request, socket, total, "\"large\"", 0)
    });
    let directory = tempfile::tempdir().unwrap();
    let transport = AudioTransport::new(options(&directory)).unwrap();
    let mut lease = open(&transport, &server);
    let mut first = [0; 64];
    lease.reader.read_exact(&mut first).unwrap();
    assert_eq!(first.as_slice(), data(0, 64, 0));
    assert!(lease.control.status().downloaded_bytes < total);
    let offset = 70 * 1024 * 1024 + 7;
    lease.reader.seek(SeekFrom::Start(offset)).unwrap();
    lease.reader.read_exact(&mut first).unwrap();
    assert_eq!(first.as_slice(), data(offset, 64, 0));
    assert_eq!(lease.reader.byte_len(), Some(total));
    assert!(lease.control.status().cache.disk_bytes <= 32 * 1024);
    assert!(lease.control.status().cache.ram_bytes <= 2048);
    lease.control.cancel();
    assert!(lease.control.join(Duration::from_secs(1)));
}

#[test]
fn ignored_range_is_an_absolute_zero_single_response() {
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    let server = Loopback::start(move |_, socket, _| {
        counter.fetch_add(1, Ordering::Relaxed);
        headers(socket, "200 OK", "Content-Length: 3091\r\n");
        let _ = socket.write_all(&data(0, 3091, 7));
    });
    let directory = tempfile::tempdir().unwrap();
    let transport = AudioTransport::new(options(&directory)).unwrap();
    let mut lease = open(&transport, &server);
    let mut first = [0; 32];
    lease.reader.read_exact(&mut first).unwrap();
    assert_eq!(first.as_slice(), data(0, 32, 7));
    lease.reader.seek(SeekFrom::Start(2048)).unwrap();
    lease.reader.read_exact(&mut first).unwrap();
    assert_eq!(first.as_slice(), data(2048, 32, 7));
    assert!(!lease.control.status().range_supported);
    assert_eq!(calls.load(Ordering::Relaxed), 1);
    lease.control.cancel();
    assert!(lease.control.join(Duration::from_secs(1)));
}

#[test]
fn weak_validator_uses_one_complete_response_without_range_or_if_range() {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let record = requests.clone();
    let server = Loopback::start(move |request, socket, _| {
        record.lock().unwrap().push(request.clone());
        if request.range.is_empty() {
            headers(
                socket,
                "200 OK",
                "Content-Length: 2051\r\nETag: W/\"weak\"\r\n",
            );
            let _ = socket.write_all(&data(0, 2051, 0));
        } else {
            range_reply(&request, socket, 2051, "W/\"weak\"", 0);
        }
    });
    let directory = tempfile::tempdir().unwrap();
    let transport = AudioTransport::new(options(&directory)).unwrap();
    let mut lease = open(&transport, &server);
    let mut bytes = Vec::new();
    lease.reader.read_to_end(&mut bytes).unwrap();
    assert_eq!(bytes, data(0, 2051, 0));
    {
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert!(requests[1].range.is_empty());
        assert!(requests.iter().all(|request| request.if_range.is_none()));
    }
    assert!(!lease.control.status().range_supported);
    assert!(lease.control.status().cache_random_access);
    assert!(lease.reader.is_seekable());
    lease.reader.seek(SeekFrom::Start(31)).unwrap();
    let mut backwards = [0_u8; 97];
    lease.reader.read_exact(&mut backwards).unwrap();
    assert_eq!(backwards.as_slice(), data(31, backwards.len(), 0));
    assert!(lease.control.join(Duration::from_secs(1)));
    assert_eq!(
        requests.lock().unwrap().len(),
        2,
        "cached seek must not initiate another HTTP request"
    );
}

#[test]
fn complete_sequential_response_with_evicted_intervals_cannot_claim_random_access() {
    let calls = Arc::new(AtomicUsize::new(0));
    let record = calls.clone();
    let total = 12 * 1024 + 17;
    let server = Loopback::start(move |_, socket, _| {
        record.fetch_add(1, Ordering::AcqRel);
        headers(socket, "200 OK", &format!("Content-Length: {total}\r\n"));
        let _ = socket.write_all(&data(0, total, 19));
    });
    let directory = tempfile::tempdir().unwrap();
    let transport = AudioTransport::new(options(&directory)).unwrap();
    let mut lease = open(&transport, &server);
    let mut complete = Vec::new();
    lease.reader.read_to_end(&mut complete).unwrap();
    assert_eq!(complete, data(0, total, 19));
    assert!(lease.control.join(Duration::from_secs(1)));
    let status = lease.control.status();
    assert!(status.eof);
    assert!(!status.range_supported);
    assert!(!status.cache_random_access);
    assert!(!lease.reader.is_seekable());
    assert_eq!(status.cache.protected_views, 0);
    assert!(status.cache.blocks <= 4);
    assert!(status.cache.disk_bytes <= 32 * 1024);
    assert!(status.cache.ram_bytes <= 2048);
    let error = lease.reader.seek(SeekFrom::Start(0)).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::Unsupported);
    assert_eq!(lease.reader.position(), total as u64);
    assert_eq!(calls.load(Ordering::Acquire), 1);
}

#[test]
fn unvalidated_unknown_total_partial_response_cannot_be_reported_as_eof() {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let record = requests.clone();
    let server = Loopback::start(move |request, socket, _| {
        record.lock().unwrap().push(request);
        headers(
            socket,
            "206 Partial Content",
            "Content-Range: bytes 0-1023/*\r\nContent-Length: 1024\r\nETag: W/\"weak\"\r\n",
        );
        let _ = socket.write_all(&data(0, 1024, 0));
    });
    let directory = tempfile::tempdir().unwrap();
    let transport = AudioTransport::new(options(&directory)).unwrap();
    let mut lease = open(&transport, &server);
    assert_eq!(
        lease.reader.read(&mut [0; 1]).unwrap_err().kind(),
        std::io::ErrorKind::InvalidData
    );
    assert_eq!(
        lease.control.take_error().unwrap().kind,
        MediaIoErrorKind::Protocol
    );
    assert!(!lease.control.status().eof);
    assert_eq!(lease.control.status().cache.blocks, 0);
    assert!(requests.lock().unwrap()[1].range.is_empty());
}

#[test]
fn contiguous_partial_ranges_publish_only_verified_complete_blocks() {
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    let server = Loopback::start(move |request, socket, _| {
        counter.fetch_add(1, Ordering::Relaxed);
        let (start, end) = interval(&request, 3072);
        let end = end.min(start + 255);
        let length = (end - start + 1) as usize;
        headers(
            socket,
            "206 Partial Content",
            &format!(
                "Content-Range: bytes {start}-{end}/3072\r\nContent-Length: {length}\r\nETag: \"v1\"\r\n"
            ),
        );
        let _ = socket.write_all(&data(start, length, 41));
    });
    let directory = tempfile::tempdir().unwrap();
    let transport = AudioTransport::new(options(&directory)).unwrap();
    let mut lease = open(&transport, &server);
    let mut bytes = Vec::new();
    lease.reader.read_to_end(&mut bytes).unwrap();
    assert_eq!(bytes, data(0, 3072, 41));
    assert_eq!(calls.load(Ordering::Relaxed), 12);
    assert_eq!(lease.control.status().cache.blocks, 3);
    lease.control.cancel();
    assert!(lease.control.join(Duration::from_secs(1)));
}

#[test]
fn split_range_http_rejection_preserves_status_without_retries_or_an_unapproved_get() {
    for status in [429, 403, 404, 410, 503] {
        let unapproved_calls = Arc::new(AtomicUsize::new(0));
        let forbidden_counter = unapproved_calls.clone();
        let unapproved = Loopback::start(move |_, socket, _| {
            forbidden_counter.fetch_add(1, Ordering::Relaxed);
            headers(socket, "200 OK", "Content-Length: 0\r\n");
        });
        let unapproved_url = unapproved.url.clone();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let recorded = requests.clone();
        let server = Loopback::start(move |request, socket, _| {
            recorded.lock().unwrap().push(request.clone());
            if request.range == "bytes=0-1023" {
                headers(
                    socket,
                    "206 Partial Content",
                    "Content-Range: bytes 0-255/2048\r\nContent-Length: 256\r\nETag: \"v1\"\r\n",
                );
                let _ = socket.write_all(&data(0, 256, 41));
            } else {
                headers(
                    socket,
                    &format!("{status} Rejected"),
                    &format!("Content-Length: 0\r\nLocation: {unapproved_url}\r\n"),
                );
            }
        });
        let directory = tempfile::tempdir().unwrap();
        let mut config = options(&directory);
        config.max_retries = 2;
        let transport = AudioTransport::new(config).unwrap();
        let mut lease = open(&transport, &server);
        assert!(lease.reader.read(&mut [0; 1]).is_err());
        assert!(lease.control.join(Duration::from_secs(2)));
        let failure = lease.control.status().error.unwrap();
        assert_eq!(failure.kind, MediaIoErrorKind::Network);
        assert_eq!(failure.http_status, Some(status));
        let calls = requests.lock().unwrap();
        assert_eq!(calls.len(), 2, "HTTP {status} was retried within transport");
        assert_eq!(calls[0].range, "bytes=0-1023");
        assert!(calls[0].if_range.is_none());
        assert_eq!(calls[1].range, "bytes=256-1023");
        assert_eq!(calls[1].if_range.as_deref(), Some("\"v1\""));
        assert_eq!(unapproved_calls.load(Ordering::Relaxed), 0);
        assert_eq!(lease.control.status().cache.blocks, 0);
        assert!(!lease.control.status().eof);
    }
}

#[test]
fn strong_unknown_total_short_tail_is_confirmed_by_same_representation_416() {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let record = requests.clone();
    let server = Loopback::start(move |request, socket, _| {
        record.lock().unwrap().push(request.clone());
        let (start, end) = interval(&request, 1507);
        if start >= 1507 {
            headers(
                socket,
                "416 Range Not Satisfiable",
                "Content-Range: bytes */1507\r\nContent-Length: 0\r\nETag: \"v1\"\r\n",
            );
        } else {
            let length = (end - start + 1) as usize;
            headers(
                socket,
                "206 Partial Content",
                &format!(
                    "Content-Range: bytes {start}-{end}/*\r\nContent-Length: {length}\r\nETag: \"v1\"\r\n"
                ),
            );
            let _ = socket.write_all(&data(start, length, 5));
        }
    });
    let directory = tempfile::tempdir().unwrap();
    let transport = AudioTransport::new(options(&directory)).unwrap();
    let mut lease = open(&transport, &server);
    let mut bytes = Vec::new();
    lease.reader.read_to_end(&mut bytes).unwrap();
    assert_eq!(bytes, data(0, 1507, 5));
    assert_eq!(lease.reader.byte_len(), Some(1507));
    assert!(lease.control.status().eof);
    assert!(
        requests
            .lock()
            .unwrap()
            .iter()
            .any(|request| request.range == "bytes=1507-2047"
                && request.if_range.as_deref() == Some("\"v1\""))
    );
    lease.control.cancel();
    assert!(lease.control.join(Duration::from_secs(1)));
}

#[test]
fn changed_416_can_restart_the_new_representation_at_zero() {
    let changed = Arc::new(AtomicBool::new(false));
    let flag = changed.clone();
    let server = Loopback::start(move |request, socket, _| {
        let (start, _) = interval(&request, 4096);
        if start >= 3072 {
            flag.store(true, Ordering::Release);
        }
        if flag.load(Ordering::Acquire) {
            range_reply(&request, socket, 2048, "\"v2\"", 79);
        } else {
            range_reply(&request, socket, 4096, "\"v1\"", 0);
        }
    });
    let directory = tempfile::tempdir().unwrap();
    let transport = AudioTransport::new(options(&directory)).unwrap();
    let mut lease = open(&transport, &server);
    lease.reader.read_exact(&mut [0; 1]).unwrap();
    lease.reader.seek(SeekFrom::Start(3072)).unwrap();
    assert_eq!(
        lease.reader.read(&mut [0; 1]).unwrap_err().kind(),
        std::io::ErrorKind::InvalidData
    );
    lease.reader.seek(SeekFrom::Start(0)).unwrap();
    let mut bytes = [0; 32];
    lease.reader.read_exact(&mut bytes).unwrap();
    assert_eq!(bytes.as_slice(), data(0, 32, 79));
    assert_eq!(lease.reader.byte_len(), Some(2048));
    assert!(lease.control.status().error.is_none());
    lease.control.cancel();
    assert!(lease.control.join(Duration::from_secs(1)));
}

#[test]
fn repeated_representation_changes_stop_at_a_finite_restart_limit() {
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    let server = Loopback::start(move |request, socket, _| {
        counter.fetch_add(1, Ordering::Relaxed);
        let (start, end) = interval(&request, 3072);
        let end = end.min(start + 255);
        let etag = if start == 0 { "\"v1\"" } else { "\"v2\"" };
        let length = (end - start + 1) as usize;
        headers(
            socket,
            "206 Partial Content",
            &format!(
                "Content-Range: bytes {start}-{end}/3072\r\nContent-Length: {length}\r\nETag: {etag}\r\n"
            ),
        );
        let _ = socket.write_all(&data(start, length, 0));
    });
    let directory = tempfile::tempdir().unwrap();
    let transport = AudioTransport::new(options(&directory)).unwrap();
    let mut lease = open(&transport, &server);
    assert_eq!(
        lease.reader.read(&mut [0; 1]).unwrap_err().kind(),
        std::io::ErrorKind::InvalidData
    );
    assert!(lease.control.join(Duration::from_secs(2)));
    assert_eq!(
        lease.control.status().error.unwrap().kind,
        MediaIoErrorKind::RepresentationChanged
    );
    assert_eq!(calls.load(Ordering::Relaxed), 4);
    assert_eq!(lease.control.status().cache.blocks, 0);
}

#[test]
fn chunked_unknown_length_becomes_known_only_at_real_eof() {
    let server = Loopback::start(|_, socket, stop| {
        headers(socket, "200 OK", "Transfer-Encoding: chunked\r\n");
        for offset in (0..3091).step_by(173) {
            if stop.load(Ordering::Acquire) {
                return;
            }
            let bytes = data(offset as u64, 173.min(3091 - offset), 0);
            if write!(socket, "{:x}\r\n", bytes.len()).is_err()
                || socket.write_all(&bytes).is_err()
                || socket.write_all(b"\r\n").is_err()
            {
                return;
            }
            thread::sleep(Duration::from_millis(3));
        }
        let _ = socket.write_all(b"0\r\n\r\n");
    });
    let directory = tempfile::tempdir().unwrap();
    let transport = AudioTransport::new(options(&directory)).unwrap();
    let mut lease = open(&transport, &server);
    lease.control.wait_ready(Duration::from_secs(2)).unwrap();
    assert_eq!(lease.reader.byte_len(), None);
    assert_eq!(
        lease.reader.seek(SeekFrom::End(0)).unwrap_err().kind(),
        std::io::ErrorKind::Unsupported
    );
    let mut bytes = Vec::new();
    lease.reader.read_to_end(&mut bytes).unwrap();
    assert_eq!(bytes, data(0, 3091, 0));
    assert_eq!(lease.reader.byte_len(), Some(3091));
    assert!(lease.control.status().eof);
}

#[test]
fn invalid_content_ranges_never_publish_bytes_or_eof() {
    for value in [
        "bytes 1-1023/4096",
        "bytes 1023-0/4096",
        "bytes 0-1023/500",
        "items 0-1023/4096",
        "bytes 0-2047/4096",
    ] {
        let value = value.to_owned();
        let server = Loopback::start(move |_, socket, _| {
            headers(
                socket,
                "206 Partial Content",
                &format!("Content-Range: {value}\r\nContent-Length: 1024\r\nETag: \"v1\"\r\n"),
            );
            let _ = socket.write_all(&data(0, 1024, 0));
        });
        let directory = tempfile::tempdir().unwrap();
        let transport = AudioTransport::new(options(&directory)).unwrap();
        let mut lease = open(&transport, &server);
        let error = lease.reader.read(&mut [0; 1]).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert_eq!(
            lease.control.take_error().unwrap().kind,
            MediaIoErrorKind::Protocol
        );
        assert_eq!(lease.control.status().cache.blocks, 0);
        assert!(!lease.control.status().eof);
        assert!(lease.control.join(Duration::from_secs(1)));
    }
}

#[test]
fn unsatisfied_empty_range_is_eof_but_range_before_total_is_protocol_error() {
    for total in [0, 100] {
        let server = Loopback::start(move |_, socket, _| {
            headers(
                socket,
                "416 Range Not Satisfiable",
                &format!("Content-Range: bytes */{total}\r\nContent-Length: 0\r\n"),
            )
        });
        let directory = tempfile::tempdir().unwrap();
        let transport = AudioTransport::new(options(&directory)).unwrap();
        let mut lease = open(&transport, &server);
        let result = lease.reader.read(&mut [0; 1]);
        if total == 0 {
            assert_eq!(result.unwrap(), 0);
            assert_eq!(lease.reader.byte_len(), Some(0));
        } else {
            assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::InvalidData);
            assert!(!lease.control.status().eof);
        }
        lease.control.cancel();
        assert!(lease.control.join(Duration::from_secs(1)));
    }
}

#[test]
fn unknown_total_range_416_locks_the_validated_end() {
    let server = Loopback::start(|request, socket, _| {
        let (start, _) = interval(&request, 4096);
        if start >= 2000 {
            headers(
                socket,
                "416 Range Not Satisfiable",
                "Content-Range: bytes */2000\r\nContent-Length: 0\r\nETag: \"v1\"\r\n",
            );
        } else {
            headers(
                socket,
                "206 Partial Content",
                "Content-Range: bytes 0-1023/*\r\nContent-Length: 1024\r\nETag: \"v1\"\r\n",
            );
            let _ = socket.write_all(&data(0, 1024, 0));
        }
    });
    let directory = tempfile::tempdir().unwrap();
    let transport = AudioTransport::new(options(&directory)).unwrap();
    let mut lease = open(&transport, &server);
    lease.reader.read_exact(&mut [0; 1]).unwrap();
    assert_eq!(lease.reader.byte_len(), None);
    lease.reader.seek(SeekFrom::Start(4096)).unwrap();
    assert_eq!(lease.reader.read(&mut [0; 1]).unwrap(), 0);
    assert_eq!(lease.reader.byte_len(), Some(2000));
    lease.control.cancel();
    assert!(lease.control.join(Duration::from_secs(1)));
}

#[test]
fn changed_strong_etag_invalidates_old_blocks_and_exposes_epoch() {
    let changed = Arc::new(AtomicBool::new(false));
    let flag = changed.clone();
    let server = Loopback::start(move |request, socket, _| {
        let (start, _) = interval(&request, 4096);
        if start >= 1024 {
            flag.store(true, Ordering::Release);
        }
        if flag.load(Ordering::Acquire) {
            range_reply(&request, socket, 4096, "\"v2\"", 99);
        } else {
            range_reply(&request, socket, 4096, "\"v1\"", 0);
        }
    });
    let directory = tempfile::tempdir().unwrap();
    let transport = AudioTransport::new(options(&directory)).unwrap();
    let mut lease = open(&transport, &server);
    lease.reader.read_exact(&mut [0; 1]).unwrap();
    lease.reader.seek(SeekFrom::Start(1024)).unwrap();
    assert_eq!(
        lease.reader.read(&mut [0; 1]).unwrap_err().kind(),
        std::io::ErrorKind::InvalidData
    );
    assert_eq!(lease.control.status().representation_epoch, 1);
    assert_eq!(
        lease.control.take_error().unwrap().kind,
        MediaIoErrorKind::RepresentationChanged
    );
    lease.reader.seek(SeekFrom::Start(0)).unwrap();
    let mut bytes = [0; 32];
    lease.reader.read_exact(&mut bytes).unwrap();
    assert_eq!(bytes.as_slice(), data(0, 32, 99));
    lease.control.cancel();
    assert!(lease.control.join(Duration::from_secs(1)));
}

#[test]
fn a_200_after_if_range_restarts_from_zero_in_a_new_epoch() {
    let server = Loopback::start(|request, socket, _| {
        if request.if_range.is_some() {
            headers(socket, "200 OK", "Content-Length: 3072\r\nETag: \"v2\"\r\n");
            let _ = socket.write_all(&data(0, 3072, 33));
        } else {
            range_reply(&request, socket, 3072, "\"v1\"", 0);
        }
    });
    let directory = tempfile::tempdir().unwrap();
    let transport = AudioTransport::new(options(&directory)).unwrap();
    let mut lease = open(&transport, &server);
    lease.reader.read_exact(&mut [0; 1]).unwrap();
    lease.reader.seek(SeekFrom::Start(1024)).unwrap();
    assert_eq!(
        lease.reader.read(&mut [0; 1]).unwrap_err().kind(),
        std::io::ErrorKind::InvalidData
    );
    lease.reader.seek(SeekFrom::Start(0)).unwrap();
    let mut bytes = [0; 32];
    lease.reader.read_exact(&mut bytes).unwrap();
    assert_eq!(bytes.as_slice(), data(0, 32, 33));
    assert!(!lease.control.status().range_supported);
    lease.control.cancel();
    assert!(lease.control.join(Duration::from_secs(1)));
}

#[test]
fn incomplete_range_retries_only_a_strong_validated_block() {
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    let server = Loopback::start(move |request, socket, _| {
        let call = counter.fetch_add(1, Ordering::Relaxed);
        if call == 0 {
            headers(
                socket,
                "206 Partial Content",
                "Content-Range: bytes 0-1023/2048\r\nContent-Length: 1024\r\nETag: \"v1\"\r\n",
            );
            let _ = socket.write_all(&data(0, 37, 0));
        } else {
            assert_eq!(request.if_range.as_deref(), Some("\"v1\""));
            range_reply(&request, socket, 2048, "\"v1\"", 0);
        }
    });
    let directory = tempfile::tempdir().unwrap();
    let mut config = options(&directory);
    config.max_retries = 1;
    let transport = AudioTransport::new(config).unwrap();
    let mut lease = open(&transport, &server);
    let mut bytes = [0; 1024];
    lease.reader.read_exact(&mut bytes).unwrap();
    assert_eq!(bytes.as_slice(), data(0, 1024, 0));
    assert_eq!(calls.load(Ordering::Relaxed), 2);
    assert!(lease.control.take_error().is_none());
    lease.control.cancel();
    assert!(lease.control.join(Duration::from_secs(1)));
}

#[test]
fn persistent_short_response_is_an_error_instead_of_eof() {
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    let server = Loopback::start(move |_, socket, _| {
        counter.fetch_add(1, Ordering::Relaxed);
        headers(
            socket,
            "206 Partial Content",
            "Content-Range: bytes 0-1023/2048\r\nContent-Length: 1024\r\nETag: \"v1\"\r\n",
        );
        let _ = socket.write_all(&[1; 37]);
    });
    let directory = tempfile::tempdir().unwrap();
    let mut config = options(&directory);
    config.max_retries = 1;
    let transport = AudioTransport::new(config).unwrap();
    let mut lease = open(&transport, &server);
    assert_eq!(
        lease.reader.read(&mut [0; 1]).unwrap_err().kind(),
        std::io::ErrorKind::UnexpectedEof
    );
    assert_eq!(calls.load(Ordering::Relaxed), 2);
    assert_eq!(
        lease.control.take_error().unwrap().kind,
        MediaIoErrorKind::Incomplete
    );
    assert_eq!(lease.control.status().cache.blocks, 0);
    assert!(!lease.control.status().eof);
}

#[test]
fn waiting_for_data_uses_idle_timeout() {
    let server = Loopback::start(|_, socket, stop| {
        headers(
            socket,
            "206 Partial Content",
            "Content-Range: bytes 0-1023/2048\r\nContent-Length: 1024\r\nETag: \"v1\"\r\n",
        );
        while !stop.load(Ordering::Acquire) {
            thread::sleep(Duration::from_millis(10));
        }
    });
    let directory = tempfile::tempdir().unwrap();
    let mut config = options(&directory);
    config.idle_timeout = Duration::from_millis(80);
    let transport = AudioTransport::new(config).unwrap();
    let mut lease = open(&transport, &server);
    let start = Instant::now();
    assert_eq!(
        lease.reader.read(&mut [0; 1]).unwrap_err().kind(),
        std::io::ErrorKind::TimedOut
    );
    assert!(start.elapsed() < Duration::from_secs(1));
    assert!(lease.control.join(Duration::from_secs(1)));
}

#[test]
fn sustained_audio_response_can_exceed_twenty_seconds() {
    let server = Loopback::start(|_, socket, stop| {
        headers(
            socket,
            "206 Partial Content",
            "Content-Range: bytes 0-1023/1024\r\nContent-Length: 1024\r\nETag: \"slow\"\r\n",
        );
        for offset in (0..1024).step_by(16) {
            if stop.load(Ordering::Acquire) || socket.write_all(&data(offset, 16, 0)).is_err() {
                return;
            }
            thread::sleep(Duration::from_millis(325));
        }
    });
    let directory = tempfile::tempdir().unwrap();
    let transport = AudioTransport::new(options(&directory)).unwrap();
    let mut lease = open(&transport, &server);
    let start = Instant::now();
    let mut bytes = [0; 1024];
    lease.reader.read_exact(&mut bytes).unwrap();
    assert!(start.elapsed() >= Duration::from_secs(20));
    assert_eq!(bytes.as_slice(), data(0, 1024, 0));
    lease.control.cancel();
    assert!(lease.control.join(Duration::from_secs(1)));
}

#[test]
fn interrupt_wakes_read_and_next_seek_cancels_the_stalled_old_request() {
    let server = Loopback::start(|request, socket, stop| {
        let (start, _) = interval(&request, 3072);
        if start == 0 {
            headers(
                socket,
                "206 Partial Content",
                "Content-Range: bytes 0-1023/3072\r\nContent-Length: 1024\r\nETag: \"v1\"\r\n",
            );
            while !stop.load(Ordering::Acquire) {
                thread::sleep(Duration::from_millis(10));
            }
        } else {
            range_reply(&request, socket, 3072, "\"v1\"", 0);
        }
    });
    let directory = tempfile::tempdir().unwrap();
    let transport = AudioTransport::new(options(&directory)).unwrap();
    let lease = open(&transport, &server);
    lease.control.wait_ready(Duration::from_secs(2)).unwrap();
    let control = lease.control.clone();
    let mut reader = lease.reader;
    let waiting = thread::spawn(move || {
        let result = reader.read(&mut [0; 1]);
        (reader, result)
    });
    thread::sleep(Duration::from_millis(25));
    let start = Instant::now();
    control.interrupt_read();
    let (mut reader, result) = waiting.join().unwrap();
    assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::Interrupted);
    assert!(start.elapsed() < Duration::from_millis(150));
    reader.seek(SeekFrom::Start(1024)).unwrap();
    let mut bytes = [0; 16];
    reader.read_exact(&mut bytes).unwrap();
    assert_eq!(bytes.as_slice(), data(1024, 16, 0));
    assert!(start.elapsed() < Duration::from_millis(500));
    control.cancel();
    assert!(control.join(Duration::from_secs(1)));
}

#[test]
fn decoder_direct_seek_supersedes_a_stalled_range_without_external_interrupt() {
    let server = Loopback::start(|request, socket, stop| {
        let (start, _) = interval(&request, 3072);
        if start == 0 {
            headers(
                socket,
                "206 Partial Content",
                "Content-Range: bytes 0-1023/3072\r\nContent-Length: 1024\r\nETag: \"v1\"\r\n",
            );
            while !stop.load(Ordering::Acquire) {
                thread::sleep(Duration::from_millis(10));
            }
        } else {
            range_reply(&request, socket, 3072, "\"v1\"", 0);
        }
    });
    let directory = tempfile::tempdir().unwrap();
    let transport = AudioTransport::new(options(&directory)).unwrap();
    let mut lease = open(&transport, &server);
    lease.control.wait_ready(Duration::from_secs(2)).unwrap();
    let start = Instant::now();
    lease.reader.seek(SeekFrom::Start(1024)).unwrap();
    let mut bytes = [0; 16];
    lease.reader.read_exact(&mut bytes).unwrap();
    assert_eq!(bytes.as_slice(), data(1024, 16, 0));
    assert!(start.elapsed() < Duration::from_millis(500));
    lease.control.cancel();
    assert!(lease.control.join(Duration::from_secs(1)));
}

#[test]
fn sequential_backpressure_obeys_one_block_and_fractional_disk_quotas() {
    for (blocks, disk) in [(1, 32 * 1024), (2, 32 * 1024 + 1536), (2, 32 * 1024 + 2048)] {
        let server = Loopback::start(|_, socket, _| {
            headers(socket, "200 OK", "Content-Length: 4099\r\n");
            let _ = socket.write_all(&data(0, 4099, 17));
        });
        let directory = tempfile::tempdir().unwrap();
        let mut config = options(&directory);
        config.cache.max_blocks = blocks;
        config.cache.disk_bytes = disk;
        config.sequential_lead_blocks = 3;
        let transport = AudioTransport::new(config).unwrap();
        let mut lease = open(&transport, &server);
        let mut bytes = Vec::new();
        lease.reader.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, data(0, 4099, 17));
        assert!(lease.control.status().cache.blocks <= blocks);
        assert!(lease.control.status().cache.disk_bytes <= disk);
        assert!(lease.control.take_error().is_none());
    }
}

#[test]
fn external_cancel_interrupts_headers_and_joins_the_worker() {
    let server = Loopback::start(|_, _, stop| {
        while !stop.load(Ordering::Acquire) {
            thread::sleep(Duration::from_millis(10));
        }
    });
    let directory = tempfile::tempdir().unwrap();
    let transport = AudioTransport::new(options(&directory)).unwrap();
    let cancel = Arc::new(AtomicBool::new(false));
    let lease = transport
        .open(
            ApprovedAudioRequest::approved_url(&server.url).unwrap(),
            cancel.clone(),
        )
        .unwrap();
    let mut reader = lease.reader;
    let waiting = thread::spawn(move || reader.read(&mut [0; 1]));
    thread::sleep(Duration::from_millis(25));
    let start = Instant::now();
    cancel.store(true, Ordering::Release);
    assert_eq!(
        waiting.join().unwrap().unwrap_err().kind(),
        std::io::ErrorKind::Interrupted
    );
    assert!(lease.control.join(Duration::from_millis(500)));
    assert!(start.elapsed() < Duration::from_millis(500));
}

#[test]
fn redirects_never_expand_exact_url_approval() {
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    let target = Loopback::start(move |request, socket, _| {
        counter.fetch_add(1, Ordering::Relaxed);
        range_reply(&request, socket, 1024, "\"v1\"", 0);
    });
    let location = target.url.clone();
    let server = Loopback::start(move |_, socket, _| {
        headers(
            socket,
            "302 Found",
            &format!("Location: {location}\r\nContent-Length: 0\r\n"),
        )
    });
    let directory = tempfile::tempdir().unwrap();
    let transport = AudioTransport::new(options(&directory)).unwrap();
    let mut lease = open(&transport, &server);
    assert!(lease.reader.read(&mut [0; 1]).is_err());
    assert_eq!(calls.load(Ordering::Relaxed), 0);
}

#[test]
fn request_validation_preserves_exact_digest_and_redacts_debug_url() {
    for url in [
        "file:///audio.wav",
        "http://user:secret@example.test/a",
        "http://@example.test/a",
        "https://user@example.test/a",
        "not-a-url",
        "http://example.test/a b",
    ] {
        assert!(ApprovedAudioRequest::approved_url(url).is_err());
    }
    assert!(
        ApprovedAudioRequest::approved_url(&format!("https://example.test/{}", "a".repeat(2048)))
            .is_err()
    );
    let identity = CacheIdentity {
        url_sha256: "incorrect".into(),
        ..CacheIdentity::default()
    };
    assert!(ApprovedAudioRequest::new("https://example.test/audio", identity).is_err());
    let request =
        ApprovedAudioRequest::approved_url("https://example.test/audio?signature=secret-query")
            .unwrap();
    let debug = format!("{request:?}");
    assert!(!debug.contains("secret-query"));
    assert!(!debug.contains("signature="));
    assert_eq!(request.identity.url_sha256.len(), 64);
}

#[test]
fn http_failure_keeps_the_numeric_status_without_echoing_url_or_error_body() {
    for code in [302_u16, 403, 404, 429, 500] {
        let server = Loopback::start(move |_, socket, _| {
            headers(
                socket,
                &format!("{code} Failed"),
                "Content-Length: 0\r\nContent-Encoding: gzip\r\n",
            );
        });
        let directory = tempfile::tempdir().unwrap();
        let transport = AudioTransport::new(options(&directory)).unwrap();
        let request =
            ApprovedAudioRequest::approved_url(&format!("{}?signature=private-value", server.url))
                .unwrap();
        let mut lease = transport
            .open(request, Arc::new(AtomicBool::new(false)))
            .unwrap();
        assert!(lease.reader.read(&mut [0; 1]).is_err());
        let error = lease.control.take_error().unwrap();
        assert_eq!(error.http_status, Some(code));
        assert!(error.message.contains(&code.to_string()));
        assert!(!error.message.contains("signature"));
        assert!(!error.message.contains("private-value"));
    }
}

#[test]
fn unvalidated_fallback_keeps_http_failure_status_before_inspecting_content_encoding() {
    let server = Loopback::start(|request, socket, _| {
        if request.range.is_empty() {
            headers(
                socket,
                "403 Forbidden",
                "Content-Length: 0\r\nContent-Encoding: gzip\r\n",
            );
        } else {
            headers(
                socket,
                "206 Partial Content",
                "Content-Range: bytes 0-1023/2048\r\nContent-Length: 1024\r\n",
            );
            let _ = socket.write_all(&[1; 1024]);
        }
    });
    let directory = tempfile::tempdir().unwrap();
    let transport = AudioTransport::new(options(&directory)).unwrap();
    let mut lease = open(&transport, &server);
    assert!(lease.reader.read(&mut [0; 1]).is_err());
    assert_eq!(lease.control.take_error().unwrap().http_status, Some(403));
}

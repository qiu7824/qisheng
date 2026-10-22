use lx_gpui_player::audio::transport::{ApprovedAudioRequest, CacheIdentity, TransportOptions};
use lx_gpui_player::audio::{
    AudioOutputMode, AudioPlayer, AudioPreloadErrorKind, AudioPreloadState, AudioPreloadToken,
    AudioPrepareTicket, PlaybackStatus,
};
use lx_gpui_player::playback::preload::CancelFlag;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

static CASES: Mutex<()> = Mutex::new(());

fn wav(seconds: f64, frequency: f64) -> Vec<u8> {
    let frames = (8000.0 * seconds).round() as u32;
    let size = frames * 2;
    let mut bytes = Vec::with_capacity(size as usize + 44);
    bytes.extend_from_slice(b"RIFF");
    bytes.extend_from_slice(&(size + 36).to_le_bytes());
    bytes.extend_from_slice(b"WAVEfmt ");
    bytes.extend_from_slice(&16_u32.to_le_bytes());
    bytes.extend_from_slice(&1_u16.to_le_bytes());
    bytes.extend_from_slice(&1_u16.to_le_bytes());
    bytes.extend_from_slice(&8000_u32.to_le_bytes());
    bytes.extend_from_slice(&16000_u32.to_le_bytes());
    bytes.extend_from_slice(&2_u16.to_le_bytes());
    bytes.extend_from_slice(&16_u16.to_le_bytes());
    bytes.extend_from_slice(b"data");
    bytes.extend_from_slice(&size.to_le_bytes());
    for frame in 0..frames {
        let sample = (std::f64::consts::TAU * frequency * frame as f64 / 8000.0).sin() * 20000.0;
        bytes.extend_from_slice(&(sample as i16).to_le_bytes());
    }
    bytes
}

struct Server {
    url: String,
    next_requests: Arc<AtomicUsize>,
    next_zero_ranges: Arc<AtomicUsize>,
    release: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}
impl Server {
    fn new(main_seconds: f64, next: Vec<u8>) -> Self {
        Self::with_rejected_next(main_seconds, next, None)
    }

    fn with_rejected_next(main_seconds: f64, next: Vec<u8>, rejection: Option<u16>) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        listener.set_nonblocking(true).unwrap();
        let release = Arc::new(AtomicBool::new(false));
        let worker_release = release.clone();
        let next_requests = Arc::new(AtomicUsize::new(0));
        let requests = next_requests.clone();
        let next_zero_ranges = Arc::new(AtomicUsize::new(0));
        let zeros = next_zero_ranges.clone();
        let main = wav(main_seconds, 220.0);
        let thread = thread::spawn(move || {
            while !worker_release.load(Ordering::Acquire) {
                let mut stream = match listener.accept() {
                    Ok((stream, _)) => stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2));
                        continue;
                    }
                    Err(error) => panic!("fixture accept failed: {error}"),
                };
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                stream
                    .set_write_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut request = [0_u8; 8192];
                let Ok(size) = stream.read(&mut request) else {
                    continue;
                };
                if size == 0 {
                    continue;
                }
                let request = String::from_utf8_lossy(&request[..size]);
                let is_next = request
                    .lines()
                    .next()
                    .is_some_and(|line| line.contains("/next.wav"));
                if is_next && let Some(status) = rejection {
                    requests.fetch_add(1, Ordering::AcqRel);
                    let _ = write!(
                        stream,
                        "HTTP/1.1 {status} Rejected\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    );
                    continue;
                }
                let data = if is_next { &next } else { &main };
                let range = request.lines().find_map(|line| {
                    let line = line.to_ascii_lowercase();
                    let text = line.strip_prefix("range: bytes=")?;
                    let (start, end) = text.trim().split_once('-')?;
                    Some((
                        start.parse::<usize>().ok()?,
                        end.parse::<usize>().unwrap_or(data.len().saturating_sub(1)),
                    ))
                });
                let start = range.map_or(0, |range| range.0);
                if is_next {
                    requests.fetch_add(1, Ordering::AcqRel);
                    if start == 0 {
                        zeros.fetch_add(1, Ordering::AcqRel);
                    }
                }
                if start >= data.len() {
                    let header = format!(
                        "HTTP/1.1 416 Range Not Satisfiable\r\nContent-Range: bytes */{}\r\nETag: \"original-v1\"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                        data.len()
                    );
                    let _ = stream.write_all(header.as_bytes());
                    continue;
                }
                let end = range.map_or(data.len() - 1, |range| range.1.min(data.len() - 1));
                let size = end - start + 1;
                let header = if range.is_some() {
                    format!(
                        "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {start}-{end}/{}\r\nETag: \"original-v1\"\r\nContent-Type: audio/wav\r\nContent-Length: {size}\r\nConnection: close\r\n\r\n",
                        data.len()
                    )
                } else {
                    format!(
                        "HTTP/1.1 200 OK\r\nETag: \"original-v1\"\r\nContent-Type: audio/wav\r\nContent-Length: {size}\r\nConnection: close\r\n\r\n"
                    )
                };
                if stream.write_all(header.as_bytes()).is_ok() {
                    let _ = stream.write_all(&data[start..=end]);
                }
            }
        });
        Self {
            url,
            next_requests,
            next_zero_ranges,
            release,
            thread: Some(thread),
        }
    }
    fn request(&self, next: bool) -> ApprovedAudioRequest {
        ApprovedAudioRequest::new(
            &format!(
                "{}/{}",
                self.url,
                if next { "next.wav" } else { "main.wav" }
            ),
            CacheIdentity {
                track_id: if next { "next".into() } else { "main".into() },
                platform: "original".into(),
                actual_quality: "wav".into(),
                source_sha256: "a".repeat(64),
                source_policy_version: 1,
                source_epoch: 1,
                approval_id: if next {
                    "approved-next".into()
                } else {
                    "approved-main".into()
                },
                approval_epoch: 1,
                ..Default::default()
            },
        )
        .unwrap()
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        self.release.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            thread.join().unwrap();
        }
    }
}
fn player() -> (tempfile::TempDir, AudioPlayer) {
    let directory = tempfile::tempdir().unwrap();
    let mut options = TransportOptions::default();
    options.cache.root = directory.path().join("encoded");
    (
        directory,
        AudioPlayer::with_transport_options(AudioOutputMode::Memory, options),
    )
}
fn wait(
    player: &AudioPlayer,
    condition: impl Fn(&lx_gpui_player::audio::PlaybackSnapshot) -> bool,
) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let snapshot = player.snapshot();
        if condition(&snapshot) {
            return;
        }
        assert_ne!(
            snapshot.status,
            PlaybackStatus::Error,
            "audio failed: {snapshot:?}"
        );
        assert!(Instant::now() < deadline, "audio timeout: {snapshot:?}");
        thread::sleep(Duration::from_millis(2));
    }
}
fn prepared(ticket: &mut AudioPrepareTicket) -> AudioPreloadToken {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if let Some(result) = ticket.try_result() {
            return result.unwrap();
        }
        assert!(Instant::now() < deadline, "prepare timeout");
        thread::sleep(Duration::from_millis(2));
    }
}

#[test]
fn paused_owned_preparation_promotes_same_stream_without_a_second_get() {
    let _guard = CASES.lock().unwrap_or_else(|error| error.into_inner());
    let server = Server::new(0.4, wav(0.8, 660.0));
    let (_directory, player) = player();
    player.load_approved(server.request(false), true);
    wait(&player, |state| state.status == PlaybackStatus::Playing);
    let generation = player.snapshot().generation;
    let cancel = CancelFlag::new();
    let mut ticket = player
        .prepare_approved(
            "preload-1".into(),
            server.request(true),
            generation,
            cancel.clone(),
        )
        .unwrap();
    let token = prepared(&mut ticket);
    let nonce = token.nonce();
    assert_eq!(token.identity().track_id, "next");
    let state = player.preload_snapshot("preload-1").unwrap();
    assert_eq!(state.state, AudioPreloadState::Ready);
    assert_eq!(state.nonce, nonce);
    assert!(state.pcm_seconds > 0.0 && state.pcm_seconds <= 0.250);
    assert!(state.encoded_bytes <= 2 * 1024 * 1024);
    assert_eq!(server.next_zero_ranges.load(Ordering::Acquire), 1);
    wait(&player, |state| state.status == PlaybackStatus::Ended);
    let requests = server.next_requests.load(Ordering::Acquire);
    player.promote_prepared(token, generation, true).unwrap();
    wait(&player, |state| {
        state.track_id.as_deref() == Some("next")
            && state.status == PlaybackStatus::Playing
            && state.position_seconds > 0.03
    });
    assert!(player.snapshot().generation > generation);
    assert_eq!(server.next_requests.load(Ordering::Acquire), requests);
    assert_eq!(server.next_zero_ranges.load(Ordering::Acquire), 1);
    assert_eq!(
        player.preload_snapshot("preload-1").unwrap().state,
        AudioPreloadState::Promoted
    );
    player.cancel_preload("preload-1");
    assert!(!cancel.transport_cancel().load(Ordering::Acquire));
    thread::sleep(Duration::from_millis(30));
    assert_ne!(player.snapshot().status, PlaybackStatus::Idle);
    player.stop();
    wait(&player, |state| state.status == PlaybackStatus::Idle);
    assert!(cancel.transport_cancel().load(Ordering::Acquire));
    player.shutdown(Duration::from_secs(2)).unwrap();
}

#[test]
fn cancelled_and_stale_preparation_issue_no_media_requests() {
    let _guard = CASES.lock().unwrap_or_else(|error| error.into_inner());
    let server = Server::new(1.0, wav(0.8, 660.0));
    let (_directory, player) = player();
    player.load_approved(server.request(false), true);
    wait(&player, |state| state.status == PlaybackStatus::Playing);
    let generation = player.snapshot().generation;
    for cancelled in [false, true] {
        let flag = CancelFlag::new();
        if cancelled {
            flag.cancel_speculative();
        }
        let mut ticket = player
            .prepare_approved(
                format!("preload-{cancelled}"),
                server.request(true),
                if cancelled {
                    generation
                } else {
                    generation - 1
                },
                flag,
            )
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if let Some(result) = ticket.try_result() {
                assert_eq!(result.unwrap_err().kind, AudioPreloadErrorKind::Stale);
                break;
            }
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(2));
        }
    }
    assert_eq!(server.next_requests.load(Ordering::Acquire), 0);
    assert_eq!(player.snapshot().generation, generation);
    assert!(player.snapshot().error.is_none());
    player.shutdown(Duration::from_secs(2)).unwrap();
}

#[test]
fn dropped_token_and_ready_cache_clear_cancel_only_speculative_resources() {
    let _guard = CASES.lock().unwrap_or_else(|error| error.into_inner());
    let server = Server::new(1.0, wav(0.8, 660.0));
    let (_directory, player) = player();
    player.load_approved(server.request(false), true);
    wait(&player, |state| state.status == PlaybackStatus::Playing);
    let generation = player.snapshot().generation;
    for clear in [false, true] {
        let flag = CancelFlag::new();
        let op = format!("preload-{clear}");
        let mut ticket = player
            .prepare_approved(op.clone(), server.request(true), generation, flag.clone())
            .unwrap();
        let token = prepared(&mut ticket);
        if clear {
            player.clear_cache().unwrap();
            assert!(token.is_cancelled());
            drop(token);
        } else {
            drop(token);
        }
        let deadline = Instant::now() + Duration::from_secs(2);
        while player
            .preload_snapshot(&op)
            .is_none_or(|state| state.state != AudioPreloadState::Cancelled)
        {
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(2));
        }
        assert!(flag.transport_cancel().load(Ordering::Acquire));
        assert_eq!(player.snapshot().generation, generation);
        assert!(player.snapshot().error.is_none());
    }
    player.shutdown(Duration::from_secs(2)).unwrap();
}

#[test]
fn preparation_decode_failure_does_not_change_foreground_playback() {
    let _guard = CASES.lock().unwrap_or_else(|error| error.into_inner());
    let server = Server::new(1.0, b"malformed original audio".to_vec());
    let (_directory, player) = player();
    player.load_approved(server.request(false), true);
    wait(&player, |state| state.status == PlaybackStatus::Playing);
    let generation = player.snapshot().generation;
    let mut ticket = player
        .prepare_approved(
            "preload-failure".into(),
            server.request(true),
            generation,
            CancelFlag::new(),
        )
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if let Some(result) = ticket.try_result() {
            let error = result.unwrap_err();
            assert_eq!(error.kind, AudioPreloadErrorKind::Decode);
            assert!(error.media.is_none());
            assert!(!error.requires_url_refresh());
            break;
        }
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(2));
    }
    assert_eq!(player.snapshot().generation, generation);
    assert_eq!(player.snapshot().track_id.as_deref(), Some("main"));
    assert!(player.snapshot().error.is_none());
    player.shutdown(Duration::from_secs(2)).unwrap();
}

#[test]
fn preparation_http_rejection_preserves_typed_status_without_foreground_failure() {
    let _guard = CASES.lock().unwrap_or_else(|error| error.into_inner());
    for status in [403_u16, 410] {
        let server = Server::with_rejected_next(8.0, wav(0.8, 660.0), Some(status));
        let (_directory, player) = player();
        player.load_approved(server.request(false), true);
        wait(&player, |state| state.status == PlaybackStatus::Playing);
        let foreground = player.snapshot();
        let operation = format!("preload-http-{status}");
        let mut ticket = player
            .prepare_approved(
                operation.clone(),
                server.request(true),
                foreground.generation,
                CancelFlag::new(),
            )
            .unwrap();
        let started = Instant::now();
        let error = loop {
            if let Some(result) = ticket.try_result() {
                break result.unwrap_err();
            }
            assert!(started.elapsed() < Duration::from_secs(3));
            thread::sleep(Duration::from_millis(2));
        };
        assert_eq!(error.kind, AudioPreloadErrorKind::Transport);
        assert_eq!(error.media.as_ref().unwrap().http_status, Some(status));
        assert!(error.requires_url_refresh());
        let prepared = player.preload_snapshot(&operation).unwrap();
        assert_eq!(prepared.state, AudioPreloadState::Failed);
        assert_eq!(
            prepared
                .error
                .as_ref()
                .unwrap()
                .media
                .as_ref()
                .unwrap()
                .http_status,
            Some(status)
        );
        let current = player.snapshot();
        assert_eq!(current.generation, foreground.generation);
        assert_eq!(current.track_id, foreground.track_id);
        assert!(current.error.is_none());
        assert!(current.failure.is_none());
        assert!(server.next_requests.load(Ordering::Acquire) > 0);
        player.shutdown(Duration::from_secs(2)).unwrap();
    }
}

#[test]
fn paused_and_muted_promotions_complete_without_waiting_for_audible_pcm() {
    let _guard = CASES.lock().unwrap_or_else(|error| error.into_inner());
    for autoplay in [false, true] {
        let server = Server::new(0.2, wav(0.8, 660.0));
        let (_directory, player) = player();
        player.load_approved(server.request(false), true);
        wait(&player, |state| state.status == PlaybackStatus::Playing);
        let generation = player.snapshot().generation;
        let mut ticket = player
            .prepare_approved(
                "preload-gain".into(),
                server.request(true),
                generation,
                CancelFlag::new(),
            )
            .unwrap();
        let token = prepared(&mut ticket);
        wait(&player, |state| state.status == PlaybackStatus::Ended);
        player.set_volume(0.0);
        player
            .promote_prepared(token, generation, autoplay)
            .unwrap();
        wait(&player, |state| {
            state.track_id.as_deref() == Some("next")
                && state.status
                    == if autoplay {
                        PlaybackStatus::Playing
                    } else {
                        PlaybackStatus::Paused
                    }
        });
        let deadline = Instant::now() + Duration::from_secs(2);
        while player
            .preload_snapshot("preload-gain")
            .is_none_or(|snapshot| snapshot.state != AudioPreloadState::Promoted)
        {
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(2));
        }
        if autoplay {
            wait(&player, |state| state.position_seconds > 0.02);
        } else {
            assert_eq!(player.snapshot().position_seconds, 0.0);
        }
        assert_eq!(server.next_zero_ranges.load(Ordering::Acquire), 1);
        player.shutdown(Duration::from_secs(2)).unwrap();
    }
}

#[test]
fn foreground_replacement_rejects_queued_promotion_and_retains_its_diagnostic_tag() {
    let _guard = CASES.lock().unwrap_or_else(|error| error.into_inner());
    let server = Server::new(0.3, wav(0.8, 660.0));
    let (_directory, player) = player();
    player.load_approved(server.request(false), true);
    wait(&player, |state| state.status == PlaybackStatus::Playing);
    let generation = player.snapshot().generation;
    let mut ticket = player
        .prepare_approved(
            "preload-stale".into(),
            server.request(true),
            generation,
            CancelFlag::new(),
        )
        .unwrap();
    let token = prepared(&mut ticket);
    let nonce = token.nonce();
    wait(&player, |state| state.status == PlaybackStatus::Ended);
    player.load_approved(server.request(false), false);
    let _ = player.promote_prepared(token, generation, true);
    wait(&player, |state| {
        state.generation > generation
            && state.track_id.as_deref() == Some("main")
            && state.status == PlaybackStatus::Paused
    });
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let snapshot = player.preload_snapshot("preload-stale").unwrap();
        assert_eq!(snapshot.nonce, nonce);
        assert_eq!(snapshot.expected_generation, generation);
        if matches!(
            snapshot.state,
            AudioPreloadState::Failed | AudioPreloadState::Cancelled
        ) {
            break;
        }
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(2));
    }
    assert_eq!(server.next_zero_ranges.load(Ordering::Acquire), 1);
    assert_eq!(player.snapshot().track_id.as_deref(), Some("main"));
    player.shutdown(Duration::from_secs(2)).unwrap();
}

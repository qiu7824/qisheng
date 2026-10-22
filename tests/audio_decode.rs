use lx_gpui_player::audio::decode::{
    DecodeErrorKind, DecoderConfig, DecoderHandle, DecoderReady, DecoderStatus,
};
use rodio::Source;
use std::fs::File;
use std::io::Read;
use std::io::Write;
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, mpsc};
use std::thread;
use std::time::{Duration, Instant};

// Independent fixture cases share the production global PCM quota. Their worker
// and consumer concurrency remains internal to each case.
static DECODE_TESTS: Mutex<()> = Mutex::new(());

fn decoder_test_guard() -> MutexGuard<'static, ()> {
    DECODE_TESTS
        .lock()
        .unwrap_or_else(|error| error.into_inner())
}

fn wav(rate: u32, channels: u16, bits: u16, float: bool, seconds: f64) -> Vec<u8> {
    let frames = (rate as f64 * seconds).round() as usize;
    let bytes_per_sample = bits as usize / 8;
    let data_bytes = frames * channels as usize * bytes_per_sample;
    let mut bytes = Vec::with_capacity(data_bytes + 44);
    bytes.extend_from_slice(b"RIFF");
    bytes.extend_from_slice(&(data_bytes as u32 + 36).to_le_bytes());
    bytes.extend_from_slice(b"WAVEfmt ");
    bytes.extend_from_slice(&16_u32.to_le_bytes());
    bytes.extend_from_slice(&(if float { 3_u16 } else { 1_u16 }).to_le_bytes());
    bytes.extend_from_slice(&channels.to_le_bytes());
    bytes.extend_from_slice(&rate.to_le_bytes());
    bytes.extend_from_slice(&(rate * channels as u32 * bytes_per_sample as u32).to_le_bytes());
    bytes.extend_from_slice(&(channels * bytes_per_sample as u16).to_le_bytes());
    bytes.extend_from_slice(&bits.to_le_bytes());
    bytes.extend_from_slice(b"data");
    bytes.extend_from_slice(&(data_bytes as u32).to_le_bytes());
    for frame in 0..frames {
        let seconds = frame as f64 / rate as f64;
        let frequency = if seconds < 1.0 { 220.0 } else { 880.0 };
        for channel in 0..channels {
            let sample =
                (std::f64::consts::TAU * frequency * (channel + 1) as f64 * seconds).sin() * 0.6;
            if float {
                bytes.extend_from_slice(&(sample as f32).to_le_bytes());
            } else {
                match bits {
                    8 => bytes.push((sample * 127.0 + 128.0).round() as u8),
                    16 => bytes.extend_from_slice(
                        &((sample * i16::MAX as f64).round() as i16).to_le_bytes(),
                    ),
                    24 => bytes.extend_from_slice(
                        &((sample * 8_388_607.0).round() as i32).to_le_bytes()[..3],
                    ),
                    _ => panic!("unsupported test fixture"),
                }
            }
        }
    }
    bytes
}

fn file_source(bytes: &[u8]) -> (tempfile::TempDir, File) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("original.wav");
    File::create(&path).unwrap().write_all(bytes).unwrap();
    (dir, File::open(path).unwrap())
}

fn ready(handle: &DecoderHandle) -> DecoderReady {
    let deadline = Instant::now() + Duration::from_secs(4);
    loop {
        if let Some(result) = handle.poll_ready() {
            return result.unwrap();
        }
        assert!(
            Instant::now() < deadline,
            "decoder readiness timeout: {:?}",
            handle.snapshot()
        );
        thread::sleep(Duration::from_millis(2));
    }
}

fn wait(
    handle: &DecoderHandle,
    condition: impl Fn(&lx_gpui_player::audio::decode::DecoderSnapshot) -> bool,
) {
    let deadline = Instant::now() + Duration::from_secs(4);
    loop {
        let snapshot = handle.snapshot();
        if condition(&snapshot) {
            return;
        }
        assert!(snapshot.error.is_none(), "decoder failed: {snapshot:?}");
        assert!(
            Instant::now() < deadline,
            "decoder wait timeout: {snapshot:?}"
        );
        thread::sleep(Duration::from_millis(2));
    }
}

fn content_frames(
    source: &mut lx_gpui_player::audio::pcm::PcmSource,
    count: usize,
) -> Vec<Vec<f32>> {
    let control = source.control();
    let channels = source.channels().get() as usize;
    let mut result = Vec::with_capacity(count);
    let deadline = Instant::now() + Duration::from_secs(4);
    while result.len() < count {
        let before = control.snapshot().consumed_frames;
        let frame: Vec<f32> = (0..channels)
            .map(|_| source.next().expect("unexpected audio EOF"))
            .collect();
        if control.snapshot().consumed_frames > before {
            result.push(frame);
        } else {
            thread::sleep(Duration::from_millis(1));
        }
        assert!(Instant::now() < deadline, "PCM frame timeout");
    }
    result
}

fn frequency(frames: &[Vec<f32>], channel: usize, sample_rate: u32) -> f64 {
    let crossings = frames
        .windows(2)
        .filter(|pair| pair[0][channel] <= 0.0 && pair[1][channel] > 0.0)
        .count();
    crossings as f64 * sample_rate as f64 / frames.len() as f64
}

fn join_worker(handle: &mut DecoderHandle) {
    let deadline = Instant::now() + Duration::from_secs(2);
    while !handle.try_join() {
        assert!(Instant::now() < deadline, "decoder worker join timeout");
        thread::sleep(Duration::from_millis(2));
    }
}

#[test]
fn explicit_decoder_preserves_sample_formats_rates_and_interleaved_channels() {
    let _guard = decoder_test_guard();
    for (rate, channels, bits, float) in [
        (8000, 1, 8, false),
        (22050, 2, 16, false),
        (48000, 2, 24, false),
        (32000, 1, 32, true),
    ] {
        let (_dir, file) = file_source(&wav(rate, channels, bits, float, 0.4));
        let mut handle =
            DecoderHandle::start_source(Box::new(file), DecoderConfig::default()).unwrap();
        let mut ready = ready(&handle);
        assert_eq!(ready.format.sample_rate, rate);
        assert_eq!(ready.format.channels, channels);
        assert!((ready.duration.unwrap().as_secs_f64() - 0.4).abs() < 1.0 / rate as f64);
        let frames = content_frames(&mut ready.source, rate as usize / 10);
        assert!((frequency(&frames, 0, rate) - 220.0).abs() < 12.0);
        if channels == 2 {
            assert!((frequency(&frames, 1, rate) - 440.0).abs() < 12.0);
        }
        assert!((handle.snapshot().pcm.position_seconds - 0.1).abs() < 1.0 / rate as f64);
        assert!(handle.shutdown(Duration::from_secs(1)));
    }
}

#[test]
fn accurate_seek_prebuffers_while_paused_and_commits_only_the_latest_target() {
    let _guard = decoder_test_guard();
    let rate = 22050;
    let (_dir, file) = file_source(&wav(rate, 2, 16, false, 3.0));
    let config = DecoderConfig {
        initially_paused: true,
        ..DecoderConfig::default()
    };
    let mut handle = DecoderHandle::start_source(Box::new(file), config).unwrap();
    let mut ready = ready(&handle);
    for _ in 0..100 {
        assert_eq!(ready.source.next(), Some(0.0));
    }
    assert_eq!(handle.snapshot().pcm.position_seconds, 0.0);
    for seq in 1..=20 {
        handle
            .seek(seq, Duration::from_secs_f64(1.0 + seq as f64 * 0.035))
            .unwrap();
    }
    wait(&handle, |s| {
        s.seek_seq == 20 && s.status == DecoderStatus::Ready
    });
    let position = handle.snapshot().pcm.position_seconds;
    assert!((position - 1.7).abs() <= 1.0 / rate as f64);
    for _ in 0..100 {
        assert_eq!(ready.source.next(), Some(0.0));
    }
    assert_eq!(handle.snapshot().pcm.position_seconds, position);
    assert!(handle.snapshot().pcm.queued_frames > 0);
    handle.set_paused(false);
    let frames = content_frames(&mut ready.source, rate as usize / 10);
    assert!((frequency(&frames, 0, rate) - 880.0).abs() < 12.0);
    assert!((frequency(&frames, 1, rate) - 1760.0).abs() < 12.0);
    assert!((handle.snapshot().pcm.position_seconds - 1.8).abs() <= 1.0 / rate as f64);
    assert!(handle.shutdown(Duration::from_secs(1)));
}

#[test]
fn real_eof_drains_all_content_and_a_seek_to_end_preserves_pause() {
    let _guard = decoder_test_guard();
    let (_dir, file) = file_source(&wav(8000, 1, 16, false, 0.12));
    let mut handle = DecoderHandle::start_source(Box::new(file), DecoderConfig::default()).unwrap();
    let mut ready = ready(&handle);
    let _ = content_frames(&mut ready.source, 960);
    let deadline = Instant::now() + Duration::from_secs(2);
    while ready.source.next().is_some() {
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(handle.snapshot().status, DecoderStatus::Ended);
    assert!((handle.snapshot().pcm.position_seconds - 0.12).abs() < 0.0002);
    handle.set_paused(true);
    handle.seek(1, Duration::from_secs(9)).unwrap();
    wait(&handle, |s| s.seek_seq == 1);
    assert!((handle.snapshot().pcm.position_seconds - 0.12).abs() < 0.0002);
    assert_eq!(ready.source.next(), Some(0.0));
    assert!(handle.shutdown(Duration::from_secs(1)));
}

#[test]
fn consumed_source_drop_preserves_normal_eof_for_late_owner_polling() {
    let _guard = decoder_test_guard();
    for (channels, poll_delay) in [(1, 3), (2, 15)] {
        let (_dir, file) = file_source(&wav(8000, channels, 16, false, 0.012));
        let mut handle =
            DecoderHandle::start_source(Box::new(file), DecoderConfig::default()).unwrap();
        let decoded = ready(&handle);
        // Iterator::count observes None and then drops the source just as a mixer
        // does. The owner intentionally does not poll its snapshot beforehand.
        assert_eq!(decoded.source.count(), 96 * channels as usize);
        thread::sleep(Duration::from_millis(poll_delay));
        let snapshot = handle.snapshot();
        assert_eq!(snapshot.status, DecoderStatus::Ended);
        assert!(snapshot.pcm.normal_eof);
        assert!(snapshot.pcm.drained);
        assert!(!snapshot.pcm.cancelled);
        assert!(snapshot.error.is_none());
        join_worker(&mut handle);
        assert!(handle.snapshot().worker_finished);
        assert_eq!(handle.snapshot().status, DecoderStatus::Ended);
        assert!((handle.snapshot().pcm.position_seconds - 0.012).abs() < 0.0002);
    }
}

#[test]
fn source_drop_with_unconsumed_eof_pcm_remains_cancellation() {
    let _guard = decoder_test_guard();
    let (_dir, file) = file_source(&wav(8000, 1, 16, false, 0.012));
    let mut handle = DecoderHandle::start_source(Box::new(file), DecoderConfig::default()).unwrap();
    let decoded = ready(&handle);
    assert!(handle.snapshot().pcm.normal_eof);
    assert!(!handle.snapshot().pcm.drained);
    drop(decoded.source);
    thread::sleep(Duration::from_millis(15));
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let snapshot = handle.snapshot();
        assert_ne!(snapshot.status, DecoderStatus::Ended);
        if snapshot.worker_finished {
            assert_eq!(snapshot.status, DecoderStatus::Cancelled);
            assert!(snapshot.pcm.cancelled);
            assert!(!snapshot.pcm.drained);
            break;
        }
        assert!(
            Instant::now() < deadline,
            "consumer-drop cancellation timeout"
        );
        thread::sleep(Duration::from_millis(2));
    }
    join_worker(&mut handle);
}

#[test]
fn truncated_and_unknown_streams_report_errors_without_ended_or_seek_success() {
    let _guard = decoder_test_guard();
    let mut short = wav(8000, 1, 16, false, 1.0);
    short.truncate(44 + 40);
    for (bytes, expected) in [
        (short, Some(DecodeErrorKind::Incomplete)),
        (b"unsupported protected audio format".to_vec(), None),
    ] {
        let (_dir, file) = file_source(&bytes);
        let mut handle =
            DecoderHandle::start_source(Box::new(file), DecoderConfig::default()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let snapshot = handle.snapshot();
            assert_ne!(snapshot.status, DecoderStatus::Ended);
            assert_eq!(snapshot.seek_seq, 0);
            if let Some(error) = snapshot.error {
                if let Some(expected) = expected {
                    assert_eq!(error.kind, expected);
                }
                break;
            }
            assert!(Instant::now() < deadline, "decode error timeout");
            thread::sleep(Duration::from_millis(2));
        }
        assert!(handle.shutdown(Duration::from_secs(1)));
    }
}

#[test]
fn cancel_wakes_a_decoder_waiting_for_network_bytes_without_false_ended() {
    let _guard = decoder_test_guard();
    use lx_gpui_player::audio::transport::{
        ApprovedAudioRequest, AudioTransport, TransportOptions,
    };
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let url = format!("http://{}/original.wav", listener.local_addr().unwrap());
    listener.set_nonblocking(true).unwrap();
    let release = Arc::new(AtomicBool::new(false));
    let server_release = release.clone();
    let (signal, accepted) = mpsc::sync_channel(1);
    let server = thread::spawn(move || {
        let mut connection = loop {
            if server_release.load(Ordering::Acquire) {
                return;
            }
            match listener.accept() {
                Ok((connection, _)) => break connection,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(2))
                }
                Err(error) => panic!("fixture accept failed: {error}"),
            }
        };
        connection.set_nonblocking(false).unwrap();
        connection
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut request = [0_u8; 4096];
        assert!(connection.read(&mut request).unwrap() > 0);
        connection.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: audio/wav\r\nContent-Length: 160044\r\nConnection: close\r\n\r\n").unwrap();
        // No byte block is ready: the decoder must wait rather than read EOF.
        signal.send(()).unwrap();
        while !server_release.load(Ordering::Acquire) {
            thread::sleep(Duration::from_millis(2));
        }
    });
    let cache = tempfile::tempdir().unwrap();
    let mut options = TransportOptions::default();
    options.cache.root = cache.path().join("encoded");
    let transport = AudioTransport::new(options).unwrap();
    let lease = transport
        .open(
            ApprovedAudioRequest::approved_url(&url).unwrap(),
            Arc::new(AtomicBool::new(false)),
        )
        .unwrap();
    let mut handle = DecoderHandle::start(lease, DecoderConfig::default()).unwrap();
    accepted.recv_timeout(Duration::from_secs(2)).unwrap();
    thread::sleep(Duration::from_millis(30));
    assert_ne!(handle.snapshot().status, DecoderStatus::Ended);
    assert!(!handle.snapshot().pcm.normal_eof);
    let started = Instant::now();
    assert!(handle.shutdown(Duration::from_secs(1)));
    assert!(started.elapsed() < Duration::from_millis(500));
    assert_eq!(handle.snapshot().status, DecoderStatus::Cancelled);
    release.store(true, Ordering::Release);
    server.join().unwrap();
    assert!(transport.shutdown(Duration::from_secs(1)));
}

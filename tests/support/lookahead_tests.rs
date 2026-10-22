use super::{Loopback, Request, data, failure_wave, headers, interval, options, range_reply};
use lx_gpui_player::audio::transport::{
    ApprovedAudioRequest, AudioTransport, MediaIoErrorKind, StreamControl, StreamStatus,
};
use lx_gpui_player::audio::{
    RenderedPcm,
    decode::{DecoderConfig, DecoderHandle, DecoderStatus},
    output::Output,
};
use std::{
    io::{Read, Seek, SeekFrom, Write},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};
use symphonia::core::io::MediaSource;

fn wait_status(control: &StreamControl, condition: impl Fn(&StreamStatus) -> bool) -> StreamStatus {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let status = control.status();
        if condition(&status) {
            return status;
        }
        assert!(
            Instant::now() < deadline,
            "actual lookahead state did not converge"
        );
        thread::sleep(Duration::from_millis(2));
    }
}
fn record(log: &Mutex<Vec<Request>>, request: &Request) {
    log.lock().unwrap().push(request.clone());
}

#[test]
fn encoded_budget_bounds_real_lookahead_and_promotion_keeps_the_same_stream() {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let log = requests.clone();
    let server = Loopback::start(move |request, socket, _| {
        record(&log, &request);
        range_reply(&request, socket, 8192, "\"budget-v1\"", 7);
    });
    let directory = tempfile::tempdir().unwrap();
    let mut config = options(&directory);
    config.range_lookahead_blocks = 2;
    let transport = AudioTransport::new(config).unwrap();
    let mut lease = transport
        .open_speculative(
            ApprovedAudioRequest::approved_url(&server.url).unwrap(),
            Arc::new(AtomicBool::new(false)),
            2048,
        )
        .unwrap();
    let serial = lease.control.status().source_serial;
    let mut first = [0_u8; 1];
    lease.reader.read_exact(&mut first).unwrap();
    assert_eq!(first[0], data(0, 1, 7)[0]);
    let bounded = wait_status(&lease.control, |status| status.published_blocks == 2);
    assert_eq!(bounded.encoded_downloaded_bytes, 2048);
    assert_eq!(bounded.encoded_inflight_bytes, 0);
    assert_eq!(bounded.encoded_budget_bytes, Some(2048));
    assert!(!bounded.budget_blocked);
    // Observe the normal worker wake cadence after both actual publications.
    thread::sleep(Duration::from_millis(100));
    {
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert_eq!(interval(&requests[0], 8192).0, 0);
        assert_eq!(interval(&requests[1], 8192).0, 1024);
        assert_eq!(requests[1].if_range.as_deref(), Some("\"budget-v1\""));
    }
    lease.control.promote_to_foreground().unwrap();
    assert_eq!(lease.control.status().source_serial, serial);
    assert!(lease.control.status().encoded_budget_bytes.is_none());
    lease.reader.seek(SeekFrom::Start(2048)).unwrap();
    let mut promoted = [0_u8; 31];
    lease.reader.read_exact(&mut promoted).unwrap();
    assert_eq!(promoted.as_slice(), data(2048, promoted.len(), 7));
    assert_eq!(lease.control.status().source_serial, serial);
    lease.control.cancel();
    assert!(lease.control.join(Duration::from_secs(2)));
}

#[test]
fn actual_cache_capacity_limits_lookahead_while_retaining_the_reader_block() {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let log = requests.clone();
    let server = Loopback::start(move |request, socket, _| {
        record(&log, &request);
        range_reply(&request, socket, 8192, "\"capacity-v1\"", 0);
    });
    let directory = tempfile::tempdir().unwrap();
    let mut config = options(&directory);
    config.range_lookahead_blocks = 2;
    config.cache.max_blocks = 2;
    let transport = AudioTransport::new(config).unwrap();
    let mut lease = transport
        .open(
            ApprovedAudioRequest::approved_url(&server.url).unwrap(),
            Arc::new(AtomicBool::new(false)),
        )
        .unwrap();
    lease.reader.read_exact(&mut [0_u8; 1]).unwrap();
    wait_status(&lease.control, |status| status.published_blocks == 2);
    thread::sleep(Duration::from_millis(100));
    assert_eq!(requests.lock().unwrap().len(), 2);
    let bounded = lease.control.status();
    assert!(bounded.cache.blocks <= 2);
    assert!(bounded.cache.disk_bytes <= 32 * 1024);
    assert!(bounded.cache.ram_bytes <= 2048);
    assert!(bounded.error.is_none());
    lease.reader.seek(SeekFrom::Start(0)).unwrap();
    let mut retained = [0_u8; 97];
    lease.reader.read_exact(&mut retained).unwrap();
    assert_eq!(retained.as_slice(), data(0, retained.len(), 0));
    lease.control.cancel();
    assert!(lease.control.join(Duration::from_secs(2)));
    assert_eq!(requests.lock().unwrap().len(), 2);
}

#[test]
fn seek_closes_the_inflight_lookahead_body_and_prioritizes_the_actual_demand() {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let log = requests.clone();
    let gate = Arc::new(AtomicBool::new(true));
    let first_future = gate.clone();
    let closed = Arc::new(AtomicBool::new(false));
    let socket_closed = closed.clone();
    let (entered, waiting) = mpsc::sync_channel(1);
    let server = Loopback::start(move |request, socket, stop| {
        record(&log, &request);
        let (start, end) = interval(&request, 8192);
        if start == 1024 && first_future.swap(false, Ordering::AcqRel) {
            headers(
                socket,
                "206 Partial Content",
                &format!(
                    "Content-Range: bytes {start}-{end}/8192\r\nContent-Length: {}\r\nETag: \"seek-v1\"\r\n",
                    end - start + 1
                ),
            );
            entered.send(()).unwrap();
            socket
                .set_read_timeout(Some(Duration::from_millis(20)))
                .unwrap();
            let mut byte = [0_u8; 1];
            while !stop.load(Ordering::Acquire) {
                match socket.read(&mut byte) {
                    Ok(0) => {
                        socket_closed.store(true, Ordering::Release);
                        return;
                    }
                    Err(error)
                        if matches!(
                            error.kind(),
                            std::io::ErrorKind::ConnectionReset
                                | std::io::ErrorKind::ConnectionAborted
                        ) =>
                    {
                        socket_closed.store(true, Ordering::Release);
                        return;
                    }
                    Err(error)
                        if matches!(
                            error.kind(),
                            std::io::ErrorKind::TimedOut
                                | std::io::ErrorKind::WouldBlock
                                | std::io::ErrorKind::Interrupted
                        ) => {}
                    Ok(_) => panic!("bodyless Range GET unexpectedly sent body bytes"),
                    Err(error) => panic!("held lookahead socket failed: {error}"),
                }
            }
        } else {
            range_reply(&request, socket, 8192, "\"seek-v1\"", 13);
        }
    });
    let directory = tempfile::tempdir().unwrap();
    let mut config = options(&directory);
    config.range_lookahead_blocks = 2;
    let transport = AudioTransport::new(config).unwrap();
    let mut lease = transport
        .open(
            ApprovedAudioRequest::approved_url(&server.url).unwrap(),
            Arc::new(AtomicBool::new(false)),
        )
        .unwrap();
    lease.reader.read_exact(&mut [0_u8; 1]).unwrap();
    waiting.recv_timeout(Duration::from_secs(5)).unwrap();
    let serial = lease.control.status().source_serial;
    lease.reader.seek(SeekFrom::Start(5120)).unwrap();
    let mut demanded = [0_u8; 33];
    lease.reader.read_exact(&mut demanded).unwrap();
    assert_eq!(demanded.as_slice(), data(5120, demanded.len(), 13));
    let deadline = Instant::now() + Duration::from_secs(3);
    while !closed.load(Ordering::Acquire) {
        assert!(
            Instant::now() < deadline,
            "obsolete lookahead HTTP body did not close"
        );
        thread::sleep(Duration::from_millis(2));
    }
    let log = requests.lock().unwrap();
    assert_eq!(interval(&log[0], 8192).0, 0);
    assert_eq!(interval(&log[1], 8192).0, 1024);
    assert_eq!(interval(&log[2], 8192).0, 5120);
    assert!(
        log.iter()
            .skip(1)
            .all(|request| request.if_range.as_deref() == Some("\"seek-v1\""))
    );
    drop(log);
    assert_eq!(lease.control.status().source_serial, serial);
    assert_eq!(lease.control.status().representation_epoch, 0);
    lease.control.cancel();
    assert!(lease.control.join(Duration::from_secs(2)));
}

#[test]
fn changed_lookahead_validator_invalidates_every_old_interval_before_new_bytes() {
    let changed = Arc::new(AtomicBool::new(false));
    let released = changed.clone();
    let gate = Arc::new(AtomicBool::new(true));
    let first_future = gate.clone();
    let (entered, waiting) = mpsc::sync_channel(1);
    let server = Loopback::start(move |request, socket, stop| {
        if interval(&request, 8192).0 == 1024 && first_future.swap(false, Ordering::AcqRel) {
            entered.send(()).unwrap();
            while !released.load(Ordering::Acquire) && !stop.load(Ordering::Acquire) {
                thread::sleep(Duration::from_millis(2));
            }
        }
        if released.load(Ordering::Acquire) {
            range_reply(&request, socket, 8192, "\"changed-v2\"", 91);
        } else {
            range_reply(&request, socket, 8192, "\"changed-v1\"", 0);
        }
    });
    let directory = tempfile::tempdir().unwrap();
    let mut config = options(&directory);
    config.range_lookahead_blocks = 2;
    let transport = AudioTransport::new(config).unwrap();
    let mut lease = transport
        .open(
            ApprovedAudioRequest::approved_url(&server.url).unwrap(),
            Arc::new(AtomicBool::new(false)),
        )
        .unwrap();
    let mut original = [0_u8; 1];
    lease.reader.read_exact(&mut original).unwrap();
    assert_eq!(original[0], data(0, 1, 0)[0]);
    waiting.recv_timeout(Duration::from_secs(5)).unwrap();
    changed.store(true, Ordering::Release);
    wait_status(&lease.control, |status| status.representation_epoch == 1);
    assert_eq!(
        lease.reader.read(&mut [0_u8; 1]).unwrap_err().kind(),
        std::io::ErrorKind::InvalidData
    );
    assert_eq!(
        lease.control.take_error().unwrap().kind,
        MediaIoErrorKind::RepresentationChanged
    );
    lease.reader.seek(SeekFrom::Start(0)).unwrap();
    let mut current = [0_u8; 97];
    lease.reader.read_exact(&mut current).unwrap();
    assert_eq!(current.as_slice(), data(0, current.len(), 91));
    assert_eq!(lease.control.status().representation_epoch, 1);
    lease.control.cancel();
    assert!(lease.control.join(Duration::from_secs(2)));
}

#[test]
fn unknown_total_weak_validation_and_200_never_extend_range_lookahead() {
    for mode in 0..3 {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let log = requests.clone();
        let server = Loopback::start(move |request, socket, _| {
            record(&log, &request);
            match mode {
                0 => {
                    headers(
                        socket,
                        "206 Partial Content",
                        "Content-Range: bytes 0-1023/*\r\nContent-Length: 1024\r\nETag: \"unknown-v1\"\r\n",
                    );
                    let _ = socket.write_all(&data(0, 1024, 0));
                }
                1 if !request.range.is_empty() => {
                    range_reply(&request, socket, 2051, "W/\"weak\"", 0)
                }
                _ => {
                    headers(socket, "200 OK", "Content-Length: 2051\r\n");
                    let _ = socket.write_all(&data(0, 2051, 0));
                }
            }
        });
        let directory = tempfile::tempdir().unwrap();
        let mut config = options(&directory);
        config.range_lookahead_blocks = 2;
        let transport = AudioTransport::new(config).unwrap();
        let mut lease = transport
            .open(
                ApprovedAudioRequest::approved_url(&server.url).unwrap(),
                Arc::new(AtomicBool::new(false)),
            )
            .unwrap();
        if mode == 0 {
            lease.reader.read_exact(&mut [0_u8; 1]).unwrap();
            assert_eq!(lease.reader.byte_len(), None);
            thread::sleep(Duration::from_millis(100));
        } else {
            let mut body = Vec::new();
            lease.reader.read_to_end(&mut body).unwrap();
            assert_eq!(body, data(0, 2051, 0));
            assert!(lease.control.join(Duration::from_secs(2)));
            assert!(!lease.control.status().range_supported);
        }
        lease.control.cancel();
        assert!(lease.control.join(Duration::from_secs(2)));
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), if mode == 1 { 2 } else { 1 });
        if mode == 1 {
            assert!(requests[1].range.is_empty());
        }
    }
}

#[test]
fn failed_lookahead_does_not_poison_present_bytes_and_demand_reports_the_real_error() {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let log = requests.clone();
    let server = Loopback::start(move |request, socket, _| {
        record(&log, &request);
        if interval(&request, 8192).0 == 0 {
            range_reply(&request, socket, 8192, "\"deferred-v1\"", 0);
        } else {
            headers(socket, "503 Service Unavailable", "Content-Length: 0\r\n");
        }
    });
    let directory = tempfile::tempdir().unwrap();
    let mut config = options(&directory);
    config.range_lookahead_blocks = 2;
    let transport = AudioTransport::new(config).unwrap();
    let mut lease = transport
        .open(
            ApprovedAudioRequest::approved_url(&server.url).unwrap(),
            Arc::new(AtomicBool::new(false)),
        )
        .unwrap();
    lease.reader.read_exact(&mut [0_u8; 1]).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while requests.lock().unwrap().len() < 2 {
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(2));
    }
    thread::sleep(Duration::from_millis(100));
    assert!(lease.control.status().error.is_none());
    assert_eq!(requests.lock().unwrap().len(), 2);
    let mut present = [0_u8; 97];
    lease.reader.read_exact(&mut present).unwrap();
    assert_eq!(present.as_slice(), data(1, present.len(), 0));
    lease.reader.seek(SeekFrom::Start(1024)).unwrap();
    assert!(lease.reader.read(&mut [0_u8; 1]).is_err());
    assert_eq!(lease.control.take_error().unwrap().http_status, Some(503));
    assert!(lease.control.join(Duration::from_secs(2)));
    assert_eq!(requests.lock().unwrap().len(), 3);
}

#[test]
fn forbidden_lookahead_preserves_http_status_and_fences_actual_queued_pcm_while_paused() {
    let body = Arc::new(failure_wave());
    let bytes = body.clone();
    let release = Arc::new(AtomicBool::new(false));
    let gate = release.clone();
    let (entered, waiting) = mpsc::sync_channel(1);
    let first_future = Arc::new(AtomicBool::new(true));
    let first = first_future.clone();
    let server = Loopback::start(move |request, socket, stop| {
        let (start, end) = interval(&request, bytes.len() as u64);
        if start >= 16 * 1024 {
            if first.swap(false, Ordering::AcqRel) {
                entered.send(()).unwrap();
            }
            while !gate.load(Ordering::Acquire) && !stop.load(Ordering::Acquire) {
                thread::sleep(Duration::from_millis(2));
            }
            headers(socket, "403 Forbidden", "Content-Length: 0\r\n");
        } else {
            headers(
                socket,
                "206 Partial Content",
                &format!(
                    "Content-Range: bytes {start}-{end}/{}\r\nContent-Length: {}\r\nETag: \"permission-v1\"\r\nContent-Type: audio/wav\r\n",
                    bytes.len(),
                    end - start + 1
                ),
            );
            let _ = socket.write_all(&bytes[start as usize..=end as usize]);
        }
    });
    let directory = tempfile::tempdir().unwrap();
    let mut config = options(&directory);
    config.range_lookahead_blocks = 2;
    config.cache.block_size = 16 * 1024;
    config.cache.ram_bytes = 64 * 1024;
    config.cache.disk_bytes = 128 * 1024;
    let transport = AudioTransport::new(config).unwrap();
    let lease = transport
        .open(
            ApprovedAudioRequest::approved_url(&server.url).unwrap(),
            Arc::new(AtomicBool::new(false)),
        )
        .unwrap();
    let stream = lease.control.clone();
    let mut decoder = DecoderHandle::start(
        lease,
        DecoderConfig {
            queue_duration: Duration::from_millis(80),
            initially_paused: true,
            ..DecoderConfig::default()
        },
    )
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    let ready = loop {
        if let Some(result) = decoder.poll_ready() {
            break result.unwrap();
        }
        assert!(
            Instant::now() < deadline,
            "actual paused decoder did not fill its ready PCM"
        );
        thread::sleep(Duration::from_millis(2));
    };
    waiting.recv_timeout(Duration::from_secs(5)).unwrap();
    let pcm = ready.source.control();
    let before = pcm.snapshot();
    assert!(before.queued_frames > 0);
    assert_eq!(before.consumed_frames, 0);
    let observation = Arc::new(Mutex::new(RenderedPcm::default()));
    let mut output = Output::memory(observation.clone()).unwrap();
    output.set_paused(true);
    output.attach(ready.source).unwrap();
    release.store(true, Ordering::Release);
    wait_status(&stream, |status| {
        status
            .error
            .as_ref()
            .is_some_and(|error| error.http_status == Some(403))
    });
    let deadline = Instant::now() + Duration::from_secs(5);
    while decoder.snapshot().status != DecoderStatus::Error {
        assert!(
            Instant::now() < deadline,
            "paused/full PCM decoder did not observe the transport refusal"
        );
        thread::sleep(Duration::from_millis(2));
    }
    let failed = decoder.snapshot();
    assert_eq!(
        failed
            .error
            .as_ref()
            .and_then(|error| error.media.as_ref())
            .and_then(|media| media.http_status),
        Some(403)
    );
    assert!(failed.pcm.failed);
    assert!(failed.pcm.queued_frames > 0);
    output.set_paused(false);
    decoder.set_paused(false);
    let batch = observation.lock().unwrap().batches;
    while observation.lock().unwrap().batches < batch + 3 {
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(2));
    }
    let rendered = observation.lock().unwrap().clone();
    assert_eq!(rendered.peak, 0.0);
    assert_eq!(rendered.rms, 0.0);
    assert_eq!(pcm.snapshot().consumed_frames, 0);
    assert_eq!(stream.take_error().unwrap().http_status, Some(403));
    assert!(output.shutdown(Duration::from_secs(5)));
    assert!(decoder.shutdown(Duration::from_secs(5)));
    assert!(stream.join(Duration::from_secs(5)));
    assert!(transport.shutdown(Duration::from_secs(5)));
}

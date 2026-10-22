use lx_gpui_player::audio::transport::{
    ApprovedAudioRequest, AudioTransport, StreamControl, StreamLease, TransportOptions,
};
use std::io::{Read, Seek, SeekFrom, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const SPEC_BYTES: u64 = 2 * 1024 * 1024;
const HTTP_DATA_FRAME_LIMIT: usize = 8192;

struct Http {
    url: String,
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}
impl Http {
    fn start(
        handler: impl Fn(String, &mut TcpStream, &AtomicBool) + Send + Sync + 'static,
    ) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!(
            "http://{}/audio?signature=private-test-value",
            listener.local_addr().unwrap()
        );
        listener.set_nonblocking(true).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let closing = stop.clone();
        let handler = Arc::new(handler);
        let worker = thread::spawn(move || {
            let mut connections = Vec::new();
            while !closing.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((mut socket, _)) => {
                        socket.set_nonblocking(false).unwrap();
                        socket
                            .set_read_timeout(Some(Duration::from_secs(1)))
                            .unwrap();
                        socket
                            .set_write_timeout(Some(Duration::from_secs(1)))
                            .unwrap();
                        let handler = handler.clone();
                        let stop = closing.clone();
                        connections.push(thread::spawn(move || {
                            let mut request = Vec::new();
                            let mut byte = [0];
                            while request.len() < 8192 && !request.ends_with(b"\r\n\r\n") {
                                if socket.read_exact(&mut byte).is_err() {
                                    return;
                                }
                                request.push(byte[0]);
                            }
                            let request = String::from_utf8(request).unwrap();
                            let range = request
                                .lines()
                                .filter_map(|line| line.split_once(':'))
                                .find(|(name, _)| name.eq_ignore_ascii_case("range"))
                                .map(|(_, value)| value.trim().to_owned())
                                .unwrap_or_default();
                            handler(range, &mut socket, &stop);
                        }));
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2))
                    }
                    Err(error) => panic!("accept failed: {error}"),
                }
            }
            for connection in connections {
                connection.join().unwrap();
            }
        });
        Self {
            url,
            stop,
            worker: Some(worker),
        }
    }
}
impl Drop for Http {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            worker.join().unwrap();
        }
    }
}

fn bytes(start: u64, len: usize) -> Vec<u8> {
    (0..len)
        .map(|index| ((start + index as u64) % 251) as u8)
        .collect()
}
fn headers(socket: &mut TcpStream, status: &str, fields: &str) {
    write!(
        socket,
        "HTTP/1.1 {status}\r\nConnection: close\r\n{fields}\r\n"
    )
    .unwrap();
}
fn bounds(range: &str, total: u64) -> (u64, u64) {
    let (start, end) = range
        .strip_prefix("bytes=")
        .unwrap()
        .split_once('-')
        .unwrap();
    let start = start.parse::<u64>().unwrap();
    let end = if end.is_empty() {
        total - 1
    } else {
        end.parse::<u64>().unwrap().min(total - 1)
    };
    (start, end)
}
fn range_reply(range: &str, socket: &mut TcpStream, total: u64) {
    let (start, end) = bounds(range, total);
    if start >= total {
        headers(
            socket,
            "416 Range Not Satisfiable",
            &format!("Content-Range: bytes */{total}\r\nETag: \"v1\"\r\nContent-Length: 0\r\n"),
        );
        return;
    }
    let len = (end - start + 1) as usize;
    headers(
        socket,
        "206 Partial Content",
        &format!(
            "Content-Range: bytes {start}-{end}/{total}\r\nETag: \"v1\"\r\nContent-Length: {len}\r\n"
        ),
    );
    let _ = socket.write_all(&bytes(start, len));
}
fn transport(
    directory: &tempfile::TempDir,
    block_size: usize,
    retries: usize,
) -> Arc<AudioTransport> {
    let mut options = TransportOptions::default();
    options.cache.root = directory.path().join("encoded");
    options.cache.block_size = block_size;
    options.cache.disk_bytes = 16 * 1024 * 1024;
    options.cache.ram_bytes = 512 * 1024;
    options.cache.max_blocks = 128;
    options.max_retries = retries;
    options.header_timeout = Duration::from_secs(2);
    options.idle_timeout = Duration::from_millis(80);
    AudioTransport::new(options).unwrap()
}
fn open(transport: &Arc<AudioTransport>, server: &Http, budget: u64) -> StreamLease {
    transport
        .open_speculative(
            ApprovedAudioRequest::approved_url(&server.url).unwrap(),
            Arc::new(AtomicBool::new(false)),
            budget,
        )
        .unwrap()
}
fn wait_blocked(control: &StreamControl) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let status = control.status();
        assert!(
            status.error.is_none(),
            "unexpected media error: {:?}",
            status.error
        );
        if status.budget_blocked {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "budget did not block: {status:?}"
        );
        thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn metadata_seeks_share_a_cumulative_budget_and_promotion_keeps_the_original_lease() {
    let ranges = Arc::new(Mutex::new(Vec::new()));
    let recorded = ranges.clone();
    let total = 8 * 1024 * 1024 + 17;
    let server = Http::start(move |range, socket, _| {
        recorded.lock().unwrap().push(range.clone());
        range_reply(&range, socket, total);
    });
    let directory = tempfile::tempdir().unwrap();
    let transport = transport(&directory, 256 * 1024, 0);
    let mut lease = open(&transport, &server, SPEC_BYTES);
    let mut byte = [0];
    lease.reader.read_exact(&mut byte).unwrap();
    lease.reader.seek(SeekFrom::End(-17)).unwrap();
    let mut tail = [0; 17];
    lease.reader.read_exact(&mut tail).unwrap();
    assert_eq!(tail.as_slice(), bytes(total - 17, 17));
    for offset in 1..=6_u64 {
        lease
            .reader
            .seek(SeekFrom::Start(offset * 1024 * 1024))
            .unwrap();
        lease.reader.read_exact(&mut byte).unwrap();
    }
    assert_eq!(
        lease.control.status().encoded_downloaded_bytes,
        7 * 256 * 1024 + 17
    );
    let control = lease.control.clone();
    let mut reader = lease.reader;
    let waiting = thread::spawn(move || {
        reader.seek(SeekFrom::Start(7 * 1024 * 1024)).unwrap();
        let mut result = [0; 32];
        reader.read_exact(&mut result).unwrap();
        (reader, result)
    });
    wait_blocked(&control);
    let count = ranges.lock().unwrap().len();
    thread::sleep(Duration::from_millis(220));
    assert_eq!(ranges.lock().unwrap().len(), count);
    assert!(control.status().error.is_none());
    assert!(
        control.status().encoded_downloaded_bytes + control.status().encoded_inflight_bytes
            <= SPEC_BYTES
    );
    control.promote_to_foreground().unwrap();
    let (_reader, result) = waiting.join().unwrap();
    assert_eq!(result.as_slice(), bytes(7 * 1024 * 1024, 32));
    assert_eq!(
        ranges
            .lock()
            .unwrap()
            .iter()
            .filter(|range| range.as_str() == "bytes=0-262143")
            .count(),
        1
    );
    assert_eq!(control.status().encoded_budget_bytes, None);
    assert!(control.status().encoded_downloaded_bytes > SPEC_BYTES);
    control.cancel();
    assert!(control.join(Duration::from_secs(1)));
}

#[test]
fn a_200_response_waits_at_two_mib_without_idle_timeout_or_promotion_get() {
    let count = Arc::new(AtomicUsize::new(0));
    let requests = count.clone();
    let total = 3 * 1024 * 1024 + 19;
    let server = Http::start(move |_, socket, stop| {
        requests.fetch_add(1, Ordering::Relaxed);
        headers(socket, "200 OK", &format!("Content-Length: {total}\r\n"));
        for offset in (0..total).step_by(4096) {
            if stop.load(Ordering::Acquire)
                || socket
                    .write_all(&bytes(offset as u64, 4096.min(total - offset)))
                    .is_err()
            {
                return;
            }
        }
    });
    let directory = tempfile::tempdir().unwrap();
    let transport = transport(&directory, 256 * 1024, 0);
    let lease = open(&transport, &server, SPEC_BYTES);
    let control = lease.control.clone();
    let mut reader = lease.reader;
    let waiting = thread::spawn(move || {
        let mut result = Vec::new();
        reader.read_to_end(&mut result).unwrap();
        (reader, result)
    });
    wait_blocked(&control);
    let blocked = control.status();
    let used = blocked.encoded_downloaded_bytes;
    let frame_limit = HTTP_DATA_FRAME_LIMIT as u64;
    let block_size = transport.cache().options().block_size as u64;
    assert!(SPEC_BYTES - frame_limit < used && used <= SPEC_BYTES);
    assert!(used + blocked.encoded_inflight_bytes <= SPEC_BYTES);
    assert!(blocked.cache.encoded_bytes >= (SPEC_BYTES - frame_limit) / block_size * block_size);
    assert!(blocked.cache.encoded_bytes <= used);
    assert_eq!(blocked.cache.encoded_bytes % block_size, 0);
    thread::sleep(Duration::from_millis(250));
    let suspended = control.status();
    assert!(suspended.error.is_none());
    assert!(suspended.budget_blocked);
    assert_eq!(suspended.encoded_downloaded_bytes, used);
    assert!(suspended.encoded_downloaded_bytes + suspended.encoded_inflight_bytes <= SPEC_BYTES);
    assert_eq!(count.load(Ordering::Relaxed), 1);
    control.promote_to_foreground().unwrap();
    let (_reader, result) = waiting.join().unwrap();
    assert_eq!(result, bytes(0, total));
    assert_eq!(count.load(Ordering::Relaxed), 1);
    assert_eq!(control.status().encoded_downloaded_bytes, total as u64);
}

#[test]
fn a_single_large_http_chunk_stays_bounded_and_resumes_after_promotion() {
    let count = Arc::new(AtomicUsize::new(0));
    let requests = count.clone();
    let total = 3 * 1024 * 1024 + 173;
    let server = Http::start(move |_, socket, stop| {
        requests.fetch_add(1, Ordering::Relaxed);
        headers(socket, "200 OK", "Transfer-Encoding: chunked\r\n");
        if write!(socket, "{total:x}\r\n").is_err() {
            return;
        }
        for offset in (0..total).step_by(4096) {
            if stop.load(Ordering::Acquire)
                || socket
                    .write_all(&bytes(offset as u64, 4096.min(total - offset)))
                    .is_err()
            {
                return;
            }
        }
        let _ = socket.write_all(b"\r\n0\r\n\r\n");
    });
    let directory = tempfile::tempdir().unwrap();
    let transport = transport(&directory, 256 * 1024, 0);
    let lease = open(&transport, &server, SPEC_BYTES);
    let control = lease.control.clone();
    let mut reader = lease.reader;
    let waiting = thread::spawn(move || {
        let mut result = Vec::new();
        reader.read_to_end(&mut result).unwrap();
        (reader, result)
    });
    wait_blocked(&control);
    assert!(control.status().encoded_downloaded_bytes <= SPEC_BYTES);
    assert!(control.status().encoded_downloaded_bytes > SPEC_BYTES - 8192);
    assert_eq!(control.status().byte_len, None);
    thread::sleep(Duration::from_millis(250));
    assert!(control.status().error.is_none());
    control.promote_to_foreground().unwrap();
    let (_reader, result) = waiting.join().unwrap();
    assert_eq!(result, bytes(0, total));
    assert_eq!(count.load(Ordering::Relaxed), 1);
    assert_eq!(control.status().byte_len, Some(total as u64));
}

#[test]
fn cancel_wakes_both_budget_and_reader_waits_and_rejects_promotion() {
    let server = Http::start(|_, socket, stop| {
        headers(socket, "200 OK", "Content-Length: 4194304\r\n");
        for offset in (0..4194304).step_by(4096) {
            if stop.load(Ordering::Acquire) || socket.write_all(&bytes(offset, 4096)).is_err() {
                return;
            }
        }
    });
    let directory = tempfile::tempdir().unwrap();
    let transport = transport(&directory, 256 * 1024, 0);
    let lease = open(&transport, &server, SPEC_BYTES);
    let control = lease.control.clone();
    let mut reader = lease.reader;
    let waiting = thread::spawn(move || {
        let mut output = [0; 4096];
        loop {
            if let Err(error) = reader.read(&mut output) {
                return error;
            }
        }
    });
    wait_blocked(&control);
    let start = Instant::now();
    control.cancel();
    assert_eq!(
        waiting.join().unwrap().kind(),
        std::io::ErrorKind::Interrupted
    );
    assert!(control.join(Duration::from_millis(500)));
    assert!(start.elapsed() < Duration::from_millis(500));
    assert!(control.promote_to_foreground().is_err());
}

#[test]
fn a_budget_smaller_than_a_complete_cache_block_is_rejected_without_http() {
    let count = Arc::new(AtomicUsize::new(0));
    let requests = count.clone();
    let server = Http::start(move |_, _, _| {
        requests.fetch_add(1, Ordering::Relaxed);
    });
    let directory = tempfile::tempdir().unwrap();
    let transport = transport(&directory, 4 * 1024 * 1024, 0);
    assert!(
        transport
            .open_speculative(
                ApprovedAudioRequest::approved_url(&server.url).unwrap(),
                Arc::new(AtomicBool::new(false)),
                SPEC_BYTES
            )
            .is_err()
    );
    assert_eq!(transport.cache().stats().objects, 0);
    assert_eq!(count.load(Ordering::Relaxed), 0);
}

#[test]
fn failed_range_body_bytes_are_charged_before_retry_and_metadata_seek() {
    let count = Arc::new(AtomicUsize::new(0));
    let requests = count.clone();
    let server = Http::start(move |range, socket, _| {
        let call = requests.fetch_add(1, Ordering::Relaxed);
        if call == 0 {
            headers(
                socket,
                "206 Partial Content",
                "Content-Range: bytes 0-65535/131072\r\nContent-Length: 65536\r\nETag: \"v1\"\r\n",
            );
            let _ = socket.write_all(&bytes(0, 32768));
        } else {
            range_reply(&range, socket, 131072);
        }
    });
    let directory = tempfile::tempdir().unwrap();
    let transport = transport(&directory, 65536, 1);
    let mut lease = open(&transport, &server, 98304);
    lease.reader.read_exact(&mut [0; 1]).unwrap();
    assert_eq!(lease.control.status().encoded_downloaded_bytes, 98304);
    assert_eq!(lease.control.status().downloaded_bytes, 65536);
    let control = lease.control.clone();
    let mut reader = lease.reader;
    let waiting = thread::spawn(move || {
        reader.seek(SeekFrom::Start(65536)).unwrap();
        reader.read_exact(&mut [0; 1]).unwrap();
        reader
    });
    wait_blocked(&control);
    assert_eq!(count.load(Ordering::Relaxed), 2);
    control.promote_to_foreground().unwrap();
    let _reader = waiting.join().unwrap();
    assert_eq!(count.load(Ordering::Relaxed), 3);
    assert_eq!(control.status().encoded_downloaded_bytes, 163840);
    control.cancel();
    assert!(control.join(Duration::from_secs(1)));
}

#[test]
fn an_unknown_small_span_never_polls_a_frame_larger_than_the_remaining_budget() {
    let count = Arc::new(AtomicUsize::new(0));
    let requests = count.clone();
    let server = Http::start(move |range, socket, _| {
        requests.fetch_add(1, Ordering::Relaxed);
        let (start, _) = bounds(&range, 12288);
        if start == 0 {
            range_reply(&range, socket, 12288);
        } else {
            headers(
                socket,
                "206 Partial Content",
                "Content-Range: bytes 8192-12287/12288\r\nTransfer-Encoding: chunked\r\nETag: \"v1\"\r\n",
            );
            let _ = socket.write_all(b"2000\r\n");
            let _ = socket.write_all(&bytes(8192, 8192));
            let _ = socket.write_all(b"\r\n0\r\n\r\n");
        }
    });
    let directory = tempfile::tempdir().unwrap();
    let transport = transport(&directory, 8192, 0);
    let mut lease = open(&transport, &server, 12288);
    lease.reader.read_exact(&mut [0; 1]).unwrap();
    let control = lease.control.clone();
    let mut reader = lease.reader;
    let waiting = thread::spawn(move || {
        reader.seek(SeekFrom::Start(8192)).unwrap();
        let result = reader.read(&mut [0; 1]);
        (reader, result)
    });
    wait_blocked(&control);
    assert_eq!(control.status().encoded_downloaded_bytes, 8192);
    assert_eq!(control.status().encoded_inflight_bytes, 0);
    assert_eq!(control.status().cache.encoded_bytes, 8192);
    thread::sleep(Duration::from_millis(200));
    assert!(control.status().error.is_none());
    control.promote_to_foreground().unwrap();
    let (_reader, result) = waiting.join().unwrap();
    assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::InvalidData);
    assert_eq!(count.load(Ordering::Relaxed), 2);
}

#[test]
fn exact_content_length_ending_at_budget_can_confirm_eof_without_promotion() {
    let count = Arc::new(AtomicUsize::new(0));
    let requests = count.clone();
    let server = Http::start(move |range, socket, _| {
        requests.fetch_add(1, Ordering::Relaxed);
        range_reply(&range, socket, 8192);
    });
    let directory = tempfile::tempdir().unwrap();
    let transport = transport(&directory, 8192, 0);
    let mut lease = open(&transport, &server, 8192);
    let mut result = Vec::new();
    lease.reader.read_to_end(&mut result).unwrap();
    assert_eq!(result, bytes(0, 8192));
    assert!(lease.control.status().eof);
    assert!(!lease.control.status().budget_blocked);
    assert_eq!(lease.control.status().encoded_downloaded_bytes, 8192);
    assert!(lease.control.status().error.is_none());
    lease.control.promote_to_foreground().unwrap();
    lease.reader.seek(SeekFrom::Start(0)).unwrap();
    lease.reader.read_exact(&mut [0; 1]).unwrap();
    assert_eq!(count.load(Ordering::Relaxed), 1);
    lease.control.cancel();
    assert!(lease.control.join(Duration::from_secs(1)));
}

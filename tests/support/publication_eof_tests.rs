use super::*;
use lx_gpui_player::audio::media_source::StreamMediaSource;
use lx_gpui_player::audio::transport::{MediaIoError, StreamLease};
use std::fs::{self, File, OpenOptions, TryLockError};
use std::sync::mpsc;

fn until(mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !condition() {
        assert!(
            Instant::now() < deadline,
            "actual EOF fixture did not converge"
        );
        thread::sleep(Duration::from_millis(2));
    }
}

struct PendingEof {
    _directory: tempfile::TempDir,
    transport: Arc<AudioTransport>,
    _server: Loopback,
    lease: StreamLease,
    gate: File,
    requests: Arc<AtomicUsize>,
}

fn pending_eof(idle_timeout: Duration) -> PendingEof {
    let directory = tempfile::Builder::new()
        .prefix("lx-pending-eof-")
        .tempdir()
        .unwrap();
    let requests = Arc::new(AtomicUsize::new(0));
    let counted = requests.clone();
    let server = Loopback::start(move |request, socket, _| {
        counted.fetch_add(1, Ordering::AcqRel);
        range_reply(&request, socket, 1024, "\"immutable-final-body\"", 0x63);
    });
    let mut configured = options(&directory);
    configured.cache.disk_bytes = 32 * 1024 * 1024;
    configured.cache.ram_bytes = 4096;
    configured.cache.max_blocks = 8;
    configured.idle_timeout = idle_timeout;
    let transport = AudioTransport::new(configured).unwrap();
    transport
        .cache()
        .wait_ready(Duration::from_secs(5))
        .unwrap();
    let gate = OpenOptions::new()
        .read(true)
        .write(true)
        .open(
            transport
                .cache()
                .options()
                .root
                .join(".lx-cache-owner.lock"),
        )
        .unwrap();
    gate.lock().unwrap();
    let probe = OpenOptions::new()
        .read(true)
        .write(true)
        .open(
            transport
                .cache()
                .options()
                .root
                .join(".lx-cache-owner.lock"),
        )
        .unwrap();
    assert!(matches!(probe.try_lock(), Err(TryLockError::WouldBlock)));
    let mut lease = open(&transport, &server);
    until(|| {
        let stats = transport.cache().stats();
        stats.pending_ram_blocks == 1 && stats.io_waiting_for_registry
    });
    let mut actual = vec![0; 1024];
    lease.reader.read_exact(&mut actual).unwrap();
    assert_eq!(actual, data(0, 1024, 0x63));
    assert_eq!(lease.reader.position(), 1024);
    assert!(!lease.control.status().eof);
    assert_eq!(transport.cache().stats().pending_publish_bytes, 1024);
    PendingEof {
        _directory: directory,
        transport,
        _server: server,
        lease,
        gate,
        requests,
    }
}

fn read_eof(
    mut reader: StreamMediaSource,
) -> (
    thread::JoinHandle<StreamMediaSource>,
    mpsc::Receiver<std::io::Result<usize>>,
) {
    let (entered, ready) = mpsc::sync_channel(1);
    let (sender, receiver) = mpsc::sync_channel(1);
    let worker = thread::spawn(move || {
        entered.send(()).unwrap();
        let result = reader.read(&mut [0; 16]);
        sender.send(result).unwrap();
        reader
    });
    ready.recv_timeout(Duration::from_secs(5)).unwrap();
    (worker, receiver)
}

#[test]
fn actual_last_ram_bytes_cannot_report_eof_before_the_owned_payload_acknowledgement() {
    let PendingEof {
        _directory,
        transport,
        _server,
        lease,
        gate,
        requests,
    } = pending_eof(Duration::from_secs(2));
    let control = lease.control;
    let (worker, result) = read_eof(lease.reader);
    assert!(matches!(
        result.recv_timeout(Duration::from_millis(40)),
        Err(mpsc::RecvTimeoutError::Timeout)
    ));
    assert!(transport.cache().stats().io_waiting_for_registry);
    assert_eq!(transport.cache().stats().pending_publish_bytes, 1024);
    gate.unlock().unwrap();
    assert_eq!(
        result
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .unwrap(),
        0
    );
    let mut reader = worker.join().unwrap();
    until(|| control.status().eof);
    assert_eq!(transport.cache().stats().pending_publish_bytes, 0);
    assert_eq!(reader.read(&mut [0; 16]).unwrap(), 0);
    assert_eq!(requests.load(Ordering::Acquire), 1);
    drop(reader);
    assert!(control.join(Duration::from_secs(5)));
    assert!(transport.shutdown(Duration::from_secs(5)));
    assert_eq!(requests.load(Ordering::Acquire), 1);
}

#[test]
fn actual_late_durable_manifest_failure_returns_storage_instead_of_successful_eof() {
    let PendingEof {
        _directory,
        transport,
        _server,
        lease,
        gate,
        requests,
    } = pending_eof(Duration::from_secs(2));
    let root = &transport.cache().options().root;
    let namespace = fs::read_dir(root)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| {
            path.is_dir()
                && path
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with("managed-")
        })
        .unwrap();
    for slot in ["manifest.0", "manifest.1"] {
        fs::write(namespace.join(slot), b"foreign invalid durable ledger").unwrap();
    }
    let control = lease.control;
    let (worker, result) = read_eof(lease.reader);
    assert!(matches!(
        result.recv_timeout(Duration::from_millis(40)),
        Err(mpsc::RecvTimeoutError::Timeout)
    ));
    gate.unlock().unwrap();
    let error = result
        .recv_timeout(Duration::from_secs(5))
        .unwrap()
        .unwrap_err();
    assert_eq!(
        error
            .get_ref()
            .unwrap()
            .downcast_ref::<MediaIoError>()
            .unwrap()
            .kind,
        MediaIoErrorKind::Storage
    );
    let reader = worker.join().unwrap();
    assert!(!control.status().eof);
    assert_eq!(transport.cache().stats().pending_publish_bytes, 0);
    drop(reader);
    assert!(control.join(Duration::from_secs(5)));
    assert!(transport.shutdown(Duration::from_secs(5)));
    assert_eq!(requests.load(Ordering::Acquire), 1);
    for slot in ["manifest.0", "manifest.1"] {
        assert_eq!(
            fs::read(namespace.join(slot)).unwrap(),
            b"foreign invalid durable ledger"
        );
    }
}

#[test]
fn known_end_without_an_accepted_ram_job_remains_immediate_and_makes_no_extra_request() {
    let directory = tempfile::Builder::new()
        .prefix("lx-known-eof-")
        .tempdir()
        .unwrap();
    let requests = Arc::new(AtomicUsize::new(0));
    let counted = requests.clone();
    let release = Arc::new(AtomicBool::new(false));
    let held = release.clone();
    let server = Loopback::start(move |_, socket, stop| {
        counted.fetch_add(1, Ordering::AcqRel);
        headers(
            socket,
            "200 OK",
            "Content-Length: 1024\r\nETag: \"known-full-body\"\r\n",
        );
        while !held.load(Ordering::Acquire) && !stop.load(Ordering::Acquire) {
            thread::sleep(Duration::from_millis(1));
        }
        let _ = socket.write_all(&data(0, 1024, 0x3b));
    });
    let transport = AudioTransport::new(options(&directory)).unwrap();
    let mut lease = open(&transport, &server);
    until(|| lease.control.status().byte_len == Some(1024));
    assert_eq!(transport.cache().stats().pending_publish_bytes, 0);
    assert_eq!(lease.reader.seek(SeekFrom::End(0)).unwrap(), 1024);
    let started = Instant::now();
    assert_eq!(lease.reader.read(&mut [0; 16]).unwrap(), 0);
    assert!(started.elapsed() < Duration::from_millis(100));
    assert_eq!(requests.load(Ordering::Acquire), 1);
    lease.control.cancel();
    release.store(true, Ordering::Release);
    drop(lease.reader);
    assert!(lease.control.join(Duration::from_secs(5)));
    assert!(transport.shutdown(Duration::from_secs(5)));
    assert_eq!(requests.load(Ordering::Acquire), 1);
}

#[test]
fn pending_eof_wait_honors_read_cancellation_and_its_existing_idle_deadline() {
    for cancel in [false, true] {
        let PendingEof {
            _directory,
            transport,
            _server,
            lease,
            gate,
            requests,
        } = pending_eof(if cancel {
            Duration::from_secs(2)
        } else {
            Duration::from_millis(60)
        });
        let control = lease.control;
        let (worker, result) = read_eof(lease.reader);
        if cancel {
            control.cancel();
        }
        let error = result
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .unwrap_err();
        let typed = error
            .get_ref()
            .unwrap()
            .downcast_ref::<MediaIoError>()
            .unwrap();
        assert_eq!(
            typed.kind,
            if cancel {
                MediaIoErrorKind::Cancelled
            } else {
                MediaIoErrorKind::Timeout
            }
        );
        if !cancel {
            assert_eq!(transport.cache().stats().pending_publish_bytes, 1024);
            assert!(transport.cache().stats().io_waiting_for_registry);
        }
        control.cancel();
        gate.unlock().unwrap();
        drop(worker.join().unwrap());
        assert!(control.join(Duration::from_secs(5)));
        assert!(transport.shutdown(Duration::from_secs(5)));
        assert_eq!(requests.load(Ordering::Acquire), 1);
    }
}

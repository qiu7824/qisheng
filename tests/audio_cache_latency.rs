use lx_gpui_player::audio::{
    AudioOutputMode, AudioPlayer, PlaybackStatus,
    cache::{CacheIoErrorCode, CacheOptions, CacheStore},
    transport::{
        ApprovedAudioRequest, AudioTransport, CacheIdentity, MediaIoError, MediaIoErrorKind,
        TransportOptions,
    },
};
use std::{
    fs,
    io::{Read, Seek, SeekFrom, Write},
    net::{TcpListener, TcpStream},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};
#[path = "support/cache_lock.rs"]
mod cache_lock;
use cache_lock::{
    CASES, RegistryLock, assert_p95, fresh_directory, logical_bytes, measured, seed, timed,
    wait_initialized, wait_registry_wait, wait_worker_count,
};
use symphonia::core::io::MediaSource;

#[test]
fn cache_registry_lock_child_fixture() {
    cache_lock::fixture_child();
}

fn options(root: &std::path::Path) -> CacheOptions {
    CacheOptions {
        root: root.into(),
        block_size: 1024,
        ram_bytes: 1024,
        disk_bytes: 32 * 1024,
        max_objects: 4,
        max_blocks: 16,
    }
}

#[test]
fn cache_constructor_and_hot_ram_controls_are_bounded_while_bootstrap_waits_for_an_os_lock() {
    let _case = CASES.lock().unwrap_or_else(|error| error.into_inner());
    let directory = fresh_directory("bootstrap-");
    let root = directory.path().join("cache");
    seed(&root);
    let mut lock = RegistryLock::hold(directory.path(), &root);
    let configured = options(&root);
    let (cache, construction) = measured(&mut lock, move || CacheStore::new(configured).unwrap());
    assert!(construction < Duration::from_millis(100));
    let waiting = wait_registry_wait(|| Some(cache.stats()));
    assert!(waiting.initializing && !waiting.initialized && !waiting.worker_finished);
    let (readiness, waited) = timed(|| cache.wait_ready(Duration::from_millis(100)));
    assert!(
        readiness.is_err(),
        "readiness must not report success before actual registry initialization"
    );
    assert!(
        waited < Duration::from_millis(200),
        "readiness ignored its deadline"
    );
    let baseline_workers = CacheStore::outstanding_workers();
    assert!(baseline_workers > 0);
    let mut snapshot_times = Vec::new();
    let mut lease_times = Vec::new();
    let mut pin_times = Vec::new();
    let mut drop_times = Vec::new();
    let mut read_times = Vec::new();
    for index in 0..20 {
        let observed = cache.clone();
        let (_, elapsed) = measured(&mut lock, move || observed.stats());
        snapshot_times.push(elapsed);
        let owned = cache.clone();
        let (lease, elapsed) = measured(&mut lock, move || {
            owned.lease(format!("bootstrap-{index}")).unwrap()
        });
        lease_times.push(elapsed);
        let owned = cache.clone();
        let (pin, elapsed) = measured(&mut lock, move || {
            owned.pin(&format!("bootstrap-{index}"), 0)
        });
        pin_times.push(elapsed);
        let (_, elapsed) = measured(&mut lock, move || drop(pin));
        drop_times.push(elapsed);
        let (_, elapsed) = measured(&mut lock, move || drop(lease));
        drop_times.push(elapsed);
        let owned = cache.clone();
        let (read, elapsed) = measured(&mut lock, move || {
            owned.read("missing", 0, &mut [0; 8]).unwrap()
        });
        read_times.push(elapsed);
        assert_eq!(read, None);
    }
    assert_p95(snapshot_times, "bootstrap statistics");
    assert_p95(lease_times, "bootstrap lease");
    assert_p95(pin_times, "bootstrap pin");
    assert_p95(drop_times, "bootstrap guard Drop");
    assert_p95(read_times, "missing block read");
    lock.assert_held();
    lock.release();
    wait_initialized(&cache);
    let lease = cache.lease("verified".into()).unwrap();
    cache.publish("verified", 0, &[0x5a; 1024]).unwrap();
    let mut bytes = [0; 16];
    assert_eq!(cache.read("verified", 0, &mut bytes).unwrap(), Some(16));
    assert_eq!(bytes, [0x5a; 16]);
    drop(lease);
    let cleared = cache
        .request_clear()
        .unwrap()
        .wait(Duration::from_secs(5))
        .unwrap();
    assert_eq!(cleared.encoded_bytes, 0);
    assert_eq!(cleared.blocks, 0);
    assert_eq!(cleared.retiring_blocks, 0);
    assert!(cleared.disk_bytes <= 32 * 1024);
    assert_eq!(cleared.disk_bytes, logical_bytes(&root));
    assert!(cache.shutdown(Duration::from_secs(5)));
    assert!(cache.stats().worker_finished);
}

#[test]
fn clear_waits_honestly_but_ram_read_lease_pin_and_drop_remain_fast_and_foreign_data_survives() {
    let _case = CASES.lock().unwrap_or_else(|error| error.into_inner());
    let directory = fresh_directory("clear-");
    let root = directory.path().join("cache");
    let cache = CacheStore::new(options(&root)).unwrap();
    wait_initialized(&cache);
    let lease = cache.lease("owned".into()).unwrap();
    cache.publish("owned", 0, &[0x6b; 1024]).unwrap();
    let original = root.join("original-user.wav");
    fs::write(&original, b"formal user recording").unwrap();
    let mut lock = RegistryLock::hold(directory.path(), &root);
    let (ticket, accepted) = timed(|| cache.request_clear().unwrap());
    assert!(accepted < Duration::from_millis(100));
    let waiting = wait_registry_wait(|| Some(cache.stats()));
    assert!(waiting.deferred_blocks > 0 && waiting.io_active);
    let mut read_times = Vec::new();
    let mut lease_times = Vec::new();
    let mut pin_times = Vec::new();
    let mut drop_times = Vec::new();
    for _ in 0..20 {
        let owned = cache.clone();
        let ((read, bytes), elapsed) = measured(&mut lock, move || {
            let mut bytes = [0; 16];
            (owned.read("owned", 0, &mut bytes).unwrap(), bytes)
        });
        assert_eq!(read, Some(16));
        assert_eq!(bytes, [0x6b; 16]);
        read_times.push(elapsed);
        let owned = cache.clone();
        let (extra, elapsed) = measured(&mut lock, move || owned.lease("owned".into()).unwrap());
        lease_times.push(elapsed);
        let owned = cache.clone();
        let (pin, elapsed) = measured(&mut lock, move || owned.pin("owned", 0));
        pin_times.push(elapsed);
        let (_, elapsed) = measured(&mut lock, move || drop(pin));
        drop_times.push(elapsed);
        let (_, elapsed) = measured(&mut lock, move || drop(extra));
        drop_times.push(elapsed);
        assert!(
            cache.stats().epoch >= waiting.epoch,
            "an I/O wait must not roll back RAM invalidation"
        );
    }
    assert_p95(read_times, "verified RAM read during clear");
    assert_p95(lease_times, "active lease during clear");
    assert_p95(pin_times, "pin during clear");
    assert_p95(drop_times, "guard Drop during clear");
    let (barrier, waited) = timed(|| ticket.wait(Duration::from_millis(100)));
    assert!(
        barrier.is_err(),
        "clear cannot claim a completed ownership audit while the registry remains locked"
    );
    assert!(
        waited < Duration::from_millis(200),
        "clear barrier ignored its 100 ms deadline: elapsed={waited:?}"
    );
    let (_, dropped) = measured(&mut lock, move || drop(lease));
    assert!(dropped < Duration::from_millis(100));
    lock.assert_held();
    lock.release();
    let error = ticket.wait(Duration::from_secs(5)).unwrap_err();
    assert!(error.to_string().contains("cannotClear"));
    let audit = cache
        .request_clear()
        .unwrap()
        .wait(Duration::from_secs(5))
        .unwrap_err();
    assert!(audit.to_string().contains("cannotClear"));
    let stats = cache.stats();
    assert!(stats.cannot_clear && stats.foreign_files >= 1);
    assert_eq!(stats.last_io_error, CacheIoErrorCode::Audit);
    assert_eq!(stats.encoded_bytes, 0);
    assert_eq!(stats.blocks, 0);
    assert_eq!(stats.retiring_blocks, 0);
    assert_eq!(fs::read(original).unwrap(), b"formal user recording");
    assert!(logical_bytes(&root) <= 32 * 1024);
    assert!(cache.shutdown(Duration::from_secs(5)));
    assert!(cache.stats().worker_finished);
}

#[test]
fn deadline_shutdown_retains_real_ownership_until_a_retry_actually_joins_the_io_worker() {
    let _case = CASES.lock().unwrap_or_else(|error| error.into_inner());
    let directory = fresh_directory("join-");
    let root = directory.path().join("cache");
    seed(&root);
    let mut lock = RegistryLock::hold(directory.path(), &root);
    let configured = options(&root);
    let cache = CacheStore::new(configured).unwrap();
    wait_registry_wait(|| Some(cache.stats()));
    let outstanding = CacheStore::outstanding_workers();
    assert!(outstanding > 0);
    let (closed, elapsed) = timed(|| cache.shutdown(Duration::ZERO));
    assert!(elapsed < Duration::from_millis(100));
    if !closed {
        assert!(
            CacheStore::outstanding_workers() > 0,
            "an unjoined cache worker must remain owned after timeout"
        );
    } else {
        assert!(
            cache.stats().worker_finished,
            "successful shutdown must correspond to a finished worker"
        );
    }
    lock.release();
    let joined = Instant::now();
    assert!(cache.shutdown(Duration::from_secs(5)));
    assert!(joined.elapsed() < Duration::from_secs(5));
    assert!(cache.stats().worker_finished);
    let recovered = CacheStore::new(options(&root)).unwrap();
    wait_initialized(&recovered);
    let stats = recovered
        .request_clear()
        .unwrap()
        .wait(Duration::from_secs(5))
        .unwrap();
    assert_eq!(stats.blocks, 0);
    assert_eq!(stats.encoded_bytes, 0);
    assert!(!stats.cannot_clear);
    assert_eq!(stats.disk_bytes, logical_bytes(&root));
    assert!(stats.disk_bytes <= 32 * 1024);
    assert!(recovered.shutdown(Duration::from_secs(5)));
}

#[test]
fn constructors_and_final_store_drop_keep_p95_below_100ms_and_reap_owners_under_the_held_lock() {
    let _case = CASES.lock().unwrap_or_else(|error| error.into_inner());
    wait_worker_count(0);
    let directory = fresh_directory("final-drop-");
    let root = directory.path().join("cache");
    seed(&root);
    let mut lock = RegistryLock::hold(directory.path(), &root);
    let mut constructors = Vec::new();
    let mut final_drops = Vec::new();
    for _ in 0..20 {
        let configured = options(&root);
        let (cache, elapsed) = measured(&mut lock, move || CacheStore::new(configured).unwrap());
        constructors.push(elapsed);
        let waiting = wait_registry_wait(|| Some(cache.stats()));
        assert!(waiting.initializing && !waiting.initialized);
        assert_eq!(CacheStore::outstanding_workers(), 1);
        assert_eq!(
            Arc::strong_count(&cache),
            1,
            "the measured Drop must be final"
        );
        let (_, elapsed) = measured(&mut lock, move || drop(cache));
        final_drops.push(elapsed);
        wait_worker_count(0);
        lock.assert_held();
    }
    assert_p95(constructors, "cache constructors");
    assert_p95(final_drops, "final CacheStore Drop");
    lock.release();
    let reopened = CacheStore::new(options(&root)).unwrap();
    wait_initialized(&reopened);
    let audit = reopened
        .request_clear()
        .unwrap()
        .wait(Duration::from_secs(5))
        .unwrap();
    assert_eq!(audit.disk_bytes, logical_bytes(&root));
    assert!(audit.disk_bytes <= 32 * 1024);
    assert!(!audit.cannot_clear);
    assert!(reopened.shutdown(Duration::from_secs(5)));
    wait_worker_count(0);
}

#[test]
fn cooperative_shutdown_can_physically_join_a_registry_waiter_before_the_lock_is_released() {
    let _case = CASES.lock().unwrap_or_else(|error| error.into_inner());
    wait_worker_count(0);
    let directory = fresh_directory("held-shutdown-");
    let root = directory.path().join("cache");
    seed(&root);
    let mut lock = RegistryLock::hold(directory.path(), &root);
    let cache = CacheStore::new(options(&root)).unwrap();
    wait_registry_wait(|| Some(cache.stats()));
    let (joined, elapsed) = timed(|| cache.shutdown(Duration::from_secs(5)));
    assert!(
        joined,
        "registry lock retry must cooperate with cancellation"
    );
    assert!(elapsed < Duration::from_secs(5));
    assert!(cache.stats().worker_finished);
    assert_eq!(CacheStore::outstanding_workers(), 0);
    lock.assert_held();
    lock.release();
}

#[test]
fn asynchronous_publish_accounts_for_queued_bytes_and_cancels_without_unlocking_the_registry() {
    let _case = CASES.lock().unwrap_or_else(|error| error.into_inner());
    wait_worker_count(0);
    let directory = fresh_directory("publish-cancel-");
    let root = directory.path().join("cache");
    let cache = CacheStore::new(options(&root)).unwrap();
    wait_initialized(&cache);
    let lease = cache.lease("cancelled".into()).unwrap();
    let mut lock = RegistryLock::hold(directory.path(), &root);
    let cancel = Arc::new(AtomicBool::new(false));
    let owned = cache.clone();
    let cancellation = cancel.clone();
    let (reply, result) = std::sync::mpsc::sync_channel(1);
    let publisher = thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let outcome = runtime.block_on(owned.publish_async(
            "cancelled".into(),
            0,
            vec![0x73; 1024],
            cancellation,
        ));
        reply.send(outcome).unwrap();
    });
    let waiting = wait_registry_wait(|| Some(cache.stats()));
    assert_eq!(waiting.pending_publish_bytes, 1024);
    assert_eq!(waiting.blocks, 0);
    let started = Instant::now();
    cancel.store(true, Ordering::Release);
    let error = result
        .recv_timeout(Duration::from_millis(100))
        .unwrap()
        .unwrap_err();
    assert!(started.elapsed() < Duration::from_millis(100));
    assert!(error.to_string().contains("cancel"), "{error:#}");
    publisher.join().unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while cache.stats().pending_publish_bytes != 0 {
        assert!(
            Instant::now() < deadline,
            "cancelled publication retained its RAM reservation"
        );
        thread::sleep(Duration::from_millis(2));
    }
    assert!(!cache.contains("cancelled", 0));
    assert_eq!(cache.read("cancelled", 0, &mut [0; 8]).unwrap(), None);
    drop(lease);
    assert!(cache.shutdown(Duration::from_secs(5)));
    assert_eq!(CacheStore::outstanding_workers(), 0);
    lock.assert_held();
    lock.release();
    let reopened = CacheStore::new(options(&root)).unwrap();
    wait_initialized(&reopened);
    let audit = reopened
        .request_clear()
        .unwrap()
        .wait(Duration::from_secs(5))
        .unwrap();
    assert_eq!(audit.blocks, 0);
    assert_eq!(audit.encoded_bytes, 0);
    assert_eq!(audit.retiring_blocks, 0);
    assert_eq!(audit.disk_bytes, logical_bytes(&root));
    assert!(audit.disk_bytes <= 32 * 1024);
    assert!(reopened.shutdown(Duration::from_secs(5)));
    wait_worker_count(0);
}

struct RangeMedia {
    url: String,
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}
impl RangeMedia {
    fn new() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}/original.wav", listener.local_addr().unwrap());
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = stop.clone();
        let bytes = Arc::new(wave());
        let thread = thread::spawn(move || {
            let mut clients = Vec::new();
            while !stopping.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((mut socket, _)) => {
                        let bytes = bytes.clone();
                        clients.push(thread::spawn(move || Self::serve(&mut socket, &bytes)));
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2))
                    }
                    Err(error) => panic!("fixture accept: {error}"),
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

    fn serve(socket: &mut TcpStream, bytes: &[u8]) {
        socket.set_nonblocking(false).unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        socket
            .set_write_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut headers = Vec::new();
        let mut scratch = [0; 1024];
        while !headers.windows(4).any(|part| part == b"\r\n\r\n") {
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
        let request = String::from_utf8_lossy(&headers);
        let range = request.lines().find_map(|line| {
            let (name, value) = line.split_once(':')?;
            if !name.eq_ignore_ascii_case("range") {
                return None;
            }
            let (start, end) = value.trim().strip_prefix("bytes=")?.split_once('-')?;
            Some((start.parse::<usize>().ok()?, end.parse::<usize>().ok()?))
        });
        let (start, requested) = range.unwrap_or((0, bytes.len() - 1));
        if start >= bytes.len() {
            return;
        }
        let end = requested.min(bytes.len() - 1);
        let status = if range.is_some() {
            "206 Partial Content"
        } else {
            "200 OK"
        };
        let mut reply = format!(
            "HTTP/1.1 {status}\r\nContent-Type: audio/wav\r\nContent-Length: {}\r\nETag: \"latency-original\"\r\nConnection: close\r\n",
            end - start + 1
        );
        if range.is_some() {
            reply.push_str(&format!(
                "Content-Range: bytes {start}-{end}/{}\r\n",
                bytes.len()
            ));
        }
        reply.push_str("\r\n");
        let _ = socket
            .write_all(reply.as_bytes())
            .and_then(|()| socket.write_all(&bytes[start..=end]));
    }
}
impl Drop for RangeMedia {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.thread.take().unwrap().join().unwrap();
    }
}
fn wave() -> Vec<u8> {
    let rate = 8000u32;
    let count = rate * 30;
    let size = count * 2;
    let mut bytes = Vec::with_capacity(size as usize + 44);
    bytes.extend(b"RIFF");
    bytes.extend((size + 36).to_le_bytes());
    bytes.extend(b"WAVEfmt ");
    bytes.extend(16u32.to_le_bytes());
    bytes.extend(1u16.to_le_bytes());
    bytes.extend(1u16.to_le_bytes());
    bytes.extend(rate.to_le_bytes());
    bytes.extend((rate * 2).to_le_bytes());
    bytes.extend(2u16.to_le_bytes());
    bytes.extend(16u16.to_le_bytes());
    bytes.extend(b"data");
    bytes.extend(size.to_le_bytes());
    for frame in 0..count {
        bytes.extend(
            (((frame as f64 * 440.0 * std::f64::consts::TAU / rate as f64).sin() * 6000.0) as i16)
                .to_le_bytes(),
        );
    }
    bytes
}

#[test]
fn actual_media_source_seek_ram_read_snapshot_and_stop_are_fast_during_a_blocked_clear_audit() {
    let _case = CASES.lock().unwrap_or_else(|error| error.into_inner());
    wait_worker_count(0);
    let directory = fresh_directory("media-source-");
    let root = directory.path().join("cache");
    let media = RangeMedia::new();
    let expected = wave();
    let transport = AudioTransport::new(TransportOptions {
        cache: CacheOptions {
            root: root.clone(),
            block_size: 1024,
            ram_bytes: 64 * 1024,
            disk_bytes: 512 * 1024,
            max_objects: 32,
            max_blocks: 32,
        },
        connect_timeout: Duration::from_secs(1),
        header_timeout: Duration::from_secs(2),
        idle_timeout: Duration::from_secs(2),
        max_retries: 0,
        sequential_lead_blocks: 1,
        range_lookahead_blocks: 0,
    })
    .unwrap();
    wait_initialized(transport.cache());
    let mut sources = Vec::new();
    for index in 0..20 {
        let mut source = transport
            .open_speculative(
                ApprovedAudioRequest::new(
                    &media.url,
                    CacheIdentity {
                        track_id: format!("source-latency-{index}"),
                        platform: "local".into(),
                        actual_quality: "wav".into(),
                        approval_id: "owned-original-wave".into(),
                        ..Default::default()
                    },
                )
                .unwrap(),
                Arc::new(AtomicBool::new(false)),
                1024,
            )
            .unwrap();
        source.control.wait_ready(Duration::from_secs(5)).unwrap();
        assert!(source.reader.is_seekable());
        assert_eq!(source.reader.byte_len(), Some(expected.len() as u64));
        let mut head = [0; 16];
        source.reader.read_exact(&mut head).unwrap();
        assert_eq!(head.as_slice(), &expected[..16]);
        sources.push(source);
    }
    assert_eq!(transport.cache().stats().blocks, 20);
    assert_eq!(transport.cache().stats().ram_bytes, 20 * 1024);
    let mut lock = RegistryLock::hold(directory.path(), &root);
    let clear = transport.cache().request_clear().unwrap();
    let waiting = wait_registry_wait(|| Some(transport.cache().stats()));
    assert_eq!(waiting.deferred_blocks, 20);
    let mut seeks = Vec::new();
    let mut reads = Vec::new();
    let mut snapshots = Vec::new();
    let mut stops = Vec::new();
    let mut drops = Vec::new();
    let mut measured_sources = Vec::new();
    for (index, source) in sources.into_iter().enumerate() {
        let position = 44 + index as u64;
        let ((source, actual), elapsed) = measured(&mut lock, move || {
            let mut source = source;
            let actual = source.reader.seek(SeekFrom::Start(position)).unwrap();
            (source, actual)
        });
        seeks.push(elapsed);
        assert_eq!(actual, position);
        let ((source, bytes), elapsed) = measured(&mut lock, move || {
            let mut source = source;
            let mut bytes = [0; 16];
            source.reader.read_exact(&mut bytes).unwrap();
            (source, bytes)
        });
        reads.push(elapsed);
        assert_eq!(
            bytes.as_slice(),
            &expected[position as usize..position as usize + 16]
        );
        let control = source.control.clone();
        let (snapshot, elapsed) = measured(&mut lock, move || control.status());
        snapshots.push(elapsed);
        assert!(snapshot.cache.io_waiting_for_registry);
        assert_eq!(snapshot.byte_len, Some(expected.len() as u64));
        assert!(snapshot.encoded_downloaded_bytes <= 1024);
        measured_sources.push(source);
    }
    assert_p95(seeks, "StreamMediaSource Seek");
    assert_p95(reads, "StreamMediaSource verified small RAM read");
    assert_p95(snapshots, "StreamControl source snapshot");
    assert!(clear.wait(Duration::from_millis(100)).is_err());
    for source in measured_sources {
        let control = source.control.clone();
        let (_, elapsed) = measured(&mut lock, move || control.cancel());
        stops.push(elapsed);
        assert!(source.control.join(Duration::from_secs(5)));
        let (_, elapsed) = measured(&mut lock, move || drop(source));
        drops.push(elapsed);
    }
    assert_p95(stops, "StreamControl Stop");
    assert_p95(drops, "final StreamLease Drop");
    let deadline = Instant::now() + Duration::from_secs(5);
    while transport.cache().stats().active_leases != 0 {
        assert!(
            Instant::now() < deadline,
            "finished source tasks retained cache leases"
        );
        thread::sleep(Duration::from_millis(2));
    }
    let retired = transport.cache().stats();
    assert_eq!(retired.active_leases, 0);
    assert_eq!(retired.pinned_blocks, 0);
    assert_eq!(retired.blocks, 0);
    assert_eq!(retired.retiring_blocks, 20);
    assert!(clear.wait(Duration::from_millis(100)).is_err());
    lock.assert_held();
    lock.release();
    clear.wait(Duration::from_secs(5)).unwrap();
    let audit = transport
        .cache()
        .request_clear()
        .unwrap()
        .wait(Duration::from_secs(5))
        .unwrap();
    assert_eq!(audit.blocks, 0);
    assert_eq!(audit.encoded_bytes, 0);
    assert_eq!(audit.retiring_blocks, 0);
    assert_eq!(audit.pending_publish_bytes, 0);
    assert_eq!(audit.disk_bytes, logical_bytes(&root));
    assert!(audit.disk_bytes <= 512 * 1024);
    assert!(transport.shutdown(Duration::from_secs(5)));
    assert!(transport.cache().shutdown(Duration::from_secs(5)));
    assert!(transport.cache().stats().worker_finished);
    wait_worker_count(0);
}

#[test]
fn runtime_shutdown_cancels_a_real_blocked_publication_and_joins_without_releasing_the_registry() {
    let _case = CASES.lock().unwrap_or_else(|error| error.into_inner());
    wait_worker_count(0);
    let directory = fresh_directory("runtime-publish-shutdown-");
    let root = directory.path().join("cache");
    let media = RangeMedia::new();
    let transport = AudioTransport::new(TransportOptions {
        cache: options(&root),
        ..Default::default()
    })
    .unwrap();
    wait_initialized(transport.cache());
    let mut lock = RegistryLock::hold(directory.path(), &root);
    let mut source = transport
        .open(
            ApprovedAudioRequest::approved_url(&media.url).unwrap(),
            Arc::new(AtomicBool::new(false)),
        )
        .unwrap();
    let waiting = wait_registry_wait(|| Some(transport.cache().stats()));
    assert!(waiting.pending_publish_bytes > 0);
    assert!(!waiting.initializing && waiting.initialized);
    let (_, admission) = timed(|| transport.shutdown(Duration::ZERO));
    assert!(admission < Duration::from_millis(100));
    let started = Instant::now();
    assert!(transport.shutdown(Duration::from_secs(5)));
    assert!(started.elapsed() < Duration::from_secs(5));
    assert!(source.control.join(Duration::ZERO));
    assert!(
        source
            .control
            .status()
            .error
            .is_none_or(|error| error.kind == MediaIoErrorKind::Cancelled)
    );
    let interrupted = source.reader.read(&mut [0; 16]).unwrap_err();
    assert_eq!(interrupted.kind(), std::io::ErrorKind::Interrupted);
    assert_eq!(
        interrupted
            .get_ref()
            .and_then(|error| error.downcast_ref::<MediaIoError>())
            .map(|error| error.kind),
        Some(MediaIoErrorKind::Cancelled)
    );
    assert!(transport.cache().stats().worker_finished);
    assert_eq!(transport.cache().stats().pending_publish_bytes, 0);
    assert_eq!(CacheStore::outstanding_workers(), 0);
    assert_eq!(AudioTransport::outstanding_runtime_workers(), 0);
    lock.assert_held();
    lock.release();
    drop(source);
    let reopened = CacheStore::new(options(&root)).unwrap();
    wait_initialized(&reopened);
    let audit = reopened
        .request_clear()
        .unwrap()
        .wait(Duration::from_secs(5))
        .unwrap();
    assert_eq!(audit.blocks, 0);
    assert_eq!(audit.encoded_bytes, 0);
    assert_eq!(audit.retiring_blocks, 0);
    assert_eq!(audit.disk_bytes, logical_bytes(&root));
    assert!(audit.disk_bytes <= 32 * 1024);
    assert!(reopened.shutdown(Duration::from_secs(5)));
    wait_worker_count(0);
}

#[test]
fn real_memory_player_controls_and_shutdown_do_not_wait_for_registry_audit_io() {
    let _case = CASES.lock().unwrap_or_else(|error| error.into_inner());
    let directory = fresh_directory("player-");
    let root = directory.path().join("cache");
    let media = RangeMedia::new();
    let player = AudioPlayer::with_transport_options(
        AudioOutputMode::Memory,
        TransportOptions {
            cache: CacheOptions {
                root: root.clone(),
                ram_bytes: 1024 * 1024,
                disk_bytes: 4 * 1024 * 1024,
                max_blocks: 16,
                max_objects: 4,
                ..Default::default()
            },
            ..Default::default()
        },
    );
    player.load_approved(
        ApprovedAudioRequest::new(
            &media.url,
            CacheIdentity {
                track_id: "original-latency".into(),
                platform: "local".into(),
                actual_quality: "local".into(),
                approval_id: "owned-fixture".into(),
                ..Default::default()
            },
        )
        .unwrap(),
        true,
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    while player.snapshot().status != PlaybackStatus::Playing
        || player.rendered_pcm().unwrap().peak < 0.01
    {
        assert!(Instant::now() < deadline, "Memory PCM did not start");
        thread::sleep(Duration::from_millis(10));
    }
    let mut lock = RegistryLock::hold(directory.path(), &root);
    let clearing = player.clone();
    let (done, cleared) = std::sync::mpsc::sync_channel(1);
    let clear_task = thread::spawn(move || {
        let _ = done.send(clearing.clear_cache());
    });
    wait_registry_wait(|| player.cache_stats());
    let mut snapshots = Vec::new();
    let mut seeks = Vec::new();
    for index in 0..20 {
        let owned = player.clone();
        let (_, elapsed) = measured(&mut lock, move || owned.snapshot());
        snapshots.push(elapsed);
        let owned = player.clone();
        let (_, elapsed) = measured(&mut lock, move || owned.seek(0.1 + index as f64 * 0.01));
        seeks.push(elapsed);
    }
    assert_p95(snapshots, "AudioPlayer snapshot");
    assert_p95(seeks, "AudioPlayer seek admission");
    assert!(
        cleared.recv_timeout(Duration::from_millis(100)).is_err(),
        "clear must still own its blocked I/O task"
    );
    let owned = player.clone();
    let (_, stop_latency) = measured(&mut lock, move || owned.stop());
    assert!(stop_latency < Duration::from_millis(100));
    let applied = Instant::now();
    while player.snapshot().status != PlaybackStatus::Idle {
        assert!(
            applied.elapsed() < Duration::from_millis(100),
            "the real audio engine did not apply Stop within the control deadline"
        );
        thread::sleep(Duration::from_millis(1));
    }
    let (outcome, admission) = timed(|| player.shutdown(Duration::ZERO));
    assert!(admission < Duration::from_millis(100));
    lock.release();
    clear_task.join().unwrap();
    let _ = outcome;
    let started = Instant::now();
    player.shutdown(Duration::from_secs(5)).unwrap();
    assert!(started.elapsed() < Duration::from_secs(5));
    assert!(
        player
            .cache_stats()
            .is_none_or(|stats| stats.worker_finished),
        "successful shutdown must leave no unfinished cache worker owned by the player"
    );
}

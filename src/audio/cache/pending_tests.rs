use super::*;
use std::fs::{self, File, OpenOptions};
use std::thread;

fn fixture() -> (tempfile::TempDir, Arc<CacheStore>) {
    fixture_with_ram(4096)
}

fn fixture_with_ram(ram_bytes: u64) -> (tempfile::TempDir, Arc<CacheStore>) {
    let directory = tempfile::Builder::new()
        .prefix("lx-cache-pending-")
        .tempdir()
        .unwrap();
    let cache = CacheStore::new(CacheOptions {
        root: directory.path().join("cache"),
        block_size: 1024,
        ram_bytes,
        disk_bytes: 32 * 1024 * 1024,
        max_objects: 4,
        max_blocks: 8,
    })
    .unwrap();
    cache.wait_ready(Duration::from_secs(5)).unwrap();
    (directory, cache)
}

#[test]
fn foreground_admission_preserves_ram_and_original_queue_quotas_during_actual_io() {
    let (_directory, cache) = fixture_with_ram(1024);
    let lease = cache.lease("bounded".into()).unwrap();
    let gate = registry_gate(&cache);
    let first = cache
        .stage_foreground(
            "bounded",
            0,
            vec![0x72; 1024],
            Arc::new(AtomicBool::new(false)),
        )
        .unwrap();
    assert!(first.ram_ready);
    until(|| cache.stats().io_waiting_for_registry);
    let second = cache
        .stage_foreground(
            "bounded",
            1024,
            vec![0xb3; 1024],
            Arc::new(AtomicBool::new(false)),
        )
        .unwrap();
    assert!(!second.ram_ready);
    assert!(
        cache
            .stage_foreground(
                "bounded",
                2048,
                vec![0x09; 1024],
                Arc::new(AtomicBool::new(false)),
            )
            .is_err()
    );
    assert!(first.result().is_none() && second.result().is_none());
    let stats = cache.stats();
    assert_eq!(stats.ram_bytes, 1024);
    assert_eq!(stats.pending_publish_bytes, 2048);
    assert_eq!(stats.pending_ram_blocks, 1);
    let mut observed = [0; 16];
    assert_eq!(cache.read("bounded", 0, &mut observed).unwrap(), Some(16));
    assert_eq!(observed, [0x72; 16]);
    assert_eq!(cache.read("bounded", 1024, &mut observed).unwrap(), None);
    let clear = cache.request_clear().unwrap();
    assert!(clear.wait(Duration::from_millis(30)).is_err());
    assert_eq!(cache.read("bounded", 0, &mut observed).unwrap(), Some(16));
    gate.unlock().unwrap();
    complete(&first).unwrap();
    complete(&second).unwrap();
    clear.wait(Duration::from_secs(5)).unwrap();
    let stats = cache.stats();
    assert_eq!(stats.pending_publish_bytes, 0);
    assert_eq!(stats.pending_disk_reservation_bytes, 0);
    assert!(stats.ram_bytes <= cache.options().ram_bytes);
    assert_eq!(
        cache.read("bounded", 1024, &mut observed).unwrap(),
        Some(16)
    );
    assert_eq!(observed, [0xb3; 16]);
    drop(lease);
    cache
        .request_clear()
        .unwrap()
        .wait(Duration::from_secs(5))
        .unwrap();
    assert!(cache.shutdown(Duration::from_secs(5)));
}

fn registry_gate(cache: &CacheStore) -> File {
    let gate = OpenOptions::new()
        .read(true)
        .write(true)
        .open(cache.options().root.join(".lx-cache-owner.lock"))
        .unwrap();
    gate.lock().unwrap();
    gate
}

fn until(mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !condition() {
        assert!(
            Instant::now() < deadline,
            "actual cache owner did not converge"
        );
        thread::sleep(Duration::from_millis(2));
    }
}

fn complete(ticket: &PublicationTicket) -> std::result::Result<(), String> {
    until(|| ticket.result().is_some());
    ticket.result().unwrap()
}

#[test]
fn foreground_ram_is_readable_under_the_actual_registry_lock_and_reuse_waits_for_seal() {
    let (_directory, cache) = fixture();
    let lease = cache.lease("live".into()).unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    assert!(
        runtime
            .block_on(cache.register_reusable_representation_async(
                "a".repeat(64),
                "live".into(),
                "\"actual-body\"".into(),
                1024,
                cache.stats().epoch,
                Arc::new(AtomicBool::new(false)),
            ))
            .unwrap()
    );
    let gate = registry_gate(&cache);
    let bytes: Vec<_> = (0..1024).map(|index| (index % 251) as u8).collect();
    let ticket = cache
        .stage_foreground("live", 0, bytes.clone(), Arc::new(AtomicBool::new(false)))
        .unwrap();
    assert!(ticket.ram_ready);
    until(|| cache.stats().io_waiting_for_registry);
    assert!(ticket.result().is_none());
    let stats = cache.stats();
    assert_eq!(stats.pending_ram_blocks, 1);
    assert_eq!(stats.pending_publish_bytes, 1024);
    assert_eq!(stats.blocks, 0);
    assert_eq!(stats.reusable_blocks, 0);
    assert!(stats.pending_disk_reservation_bytes > 1024);
    assert!(stats.disk_bytes + stats.pending_disk_reservation_bytes <= cache.options().disk_bytes);
    assert!(stats.ram_bytes <= cache.options().ram_bytes);
    let mut observed = [0; 37];
    assert_eq!(cache.read("live", 28, &mut observed).unwrap(), Some(37));
    assert_eq!(observed, bytes[28..65]);
    let pin = cache.pin("live", 0);
    assert_eq!(cache.stats().pinned_blocks, 1);
    gate.unlock().unwrap();
    complete(&ticket).unwrap();
    assert_eq!(cache.stats().pending_ram_blocks, 0);
    assert_eq!(cache.stats().pending_disk_reservation_bytes, 0);
    assert_eq!(cache.stats().reusable_blocks, 1);
    assert_eq!(cache.read("live", 28, &mut observed).unwrap(), Some(37));
    assert_eq!(observed, bytes[28..65]);
    cache
        .request_clear()
        .unwrap()
        .wait(Duration::from_secs(5))
        .unwrap();
    assert_eq!(cache.read("live", 28, &mut observed).unwrap(), Some(37));
    drop(pin);
    drop(lease);
    let stats = cache
        .request_clear()
        .unwrap()
        .wait(Duration::from_secs(5))
        .unwrap();
    assert_eq!(stats.blocks, 0);
    assert_eq!(stats.pending_publish_bytes, 0);
    assert_eq!(stats.pending_disk_reservation_bytes, 0);
    assert!(cache.shutdown(Duration::from_secs(5)));
}

#[test]
fn invalidated_pending_generation_cannot_erase_or_publish_the_replacement_bytes() {
    let (_directory, cache) = fixture();
    let lease = cache.lease("replace".into()).unwrap();
    let gate = registry_gate(&cache);
    let cancel = Arc::new(AtomicBool::new(false));
    let old = cache
        .stage_foreground("replace", 0, vec![0x19; 1024], cancel.clone())
        .unwrap();
    assert!(old.ram_ready);
    until(|| cache.stats().io_waiting_for_registry);
    cancel.store(true, Ordering::Release);
    let mut bytes = [0; 16];
    assert_eq!(cache.read("replace", 0, &mut bytes).unwrap(), None);
    cache.invalidate("replace").unwrap();
    let new = cache
        .stage_foreground(
            "replace",
            0,
            vec![0xe7; 1024],
            Arc::new(AtomicBool::new(false)),
        )
        .unwrap();
    assert!(new.ram_ready);
    assert_eq!(cache.read("replace", 0, &mut bytes).unwrap(), Some(16));
    assert_eq!(bytes, [0xe7; 16]);
    gate.unlock().unwrap();
    assert!(complete(&old).is_err());
    complete(&new).unwrap();
    assert!(!cache.publication_failed("replace"));
    assert_eq!(cache.read("replace", 0, &mut bytes).unwrap(), Some(16));
    assert_eq!(bytes, [0xe7; 16]);
    let stats = cache.stats();
    assert_eq!(stats.pending_publish_bytes, 0);
    assert_eq!(stats.pending_disk_reservation_bytes, 0);
    assert!(stats.ram_bytes <= cache.options().ram_bytes);
    drop(lease);
    cache
        .request_clear()
        .unwrap()
        .wait(Duration::from_secs(5))
        .unwrap();
    assert!(cache.shutdown(Duration::from_secs(5)));
}

#[test]
fn late_actual_create_collision_fences_ram_and_preserves_the_foreign_file() {
    let (_directory, cache) = fixture();
    let lease = cache.lease("failure".into()).unwrap();
    let gate = registry_gate(&cache);
    let ticket = cache
        .stage_foreground(
            "failure",
            0,
            vec![0xc2; 1024],
            Arc::new(AtomicBool::new(false)),
        )
        .unwrap();
    assert!(ticket.ram_ready);
    until(|| cache.stats().io_waiting_for_registry);
    let serial = cache
        .core
        .state
        .lock()
        .unwrap()
        .pending_ram
        .get(&("failure".into(), 0))
        .unwrap()
        .serial;
    let namespace = fs::read_dir(&cache.options().root)
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
    let foreign = namespace.join(format!("block-{serial:016x}.bin"));
    fs::write(&foreign, b"foreign retained body").unwrap();
    let mut bytes = [0; 16];
    assert_eq!(cache.read("failure", 0, &mut bytes).unwrap(), Some(16));
    assert_eq!(bytes, [0xc2; 16]);
    gate.unlock().unwrap();
    assert!(complete(&ticket).is_err());
    assert!(cache.publication_failed("failure"));
    assert!(cache.read("failure", 0, &mut bytes).is_err());
    assert!(!cache.contains("failure", 0));
    assert_eq!(cache.stats().pending_ram_blocks, 0);
    assert_eq!(cache.stats().pending_disk_reservation_bytes, 0);
    assert_eq!(fs::read(&foreign).unwrap(), b"foreign retained body");
    drop(lease);
    assert!(cache.shutdown(Duration::from_secs(5)));
    assert_eq!(fs::read(&foreign).unwrap(), b"foreign retained body");
}

#[test]
fn actual_active_writer_unwind_wakes_the_ticket_and_keeps_partial_disk_ownership_accounted() {
    let (_directory, cache) = fixture();
    let lease = cache.lease("unwind".into()).unwrap();
    let gate = registry_gate(&cache);
    cache
        .core
        .diagnostics
        .unwind_after_header
        .store(true, Ordering::Release);
    let ticket = cache
        .stage_foreground(
            "unwind",
            0,
            vec![0x42; 1024],
            Arc::new(AtomicBool::new(false)),
        )
        .unwrap();
    assert!(ticket.ram_ready);
    until(|| cache.stats().io_waiting_for_registry);
    let (entered, ready) = mpsc::sync_channel(1);
    let (completed, result) = mpsc::sync_channel(1);
    let observed = ticket.clone();
    let waiter = thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        entered.send(()).unwrap();
        completed.send(runtime.block_on(observed.wait())).unwrap();
    });
    ready.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(matches!(
        result.recv_timeout(Duration::from_millis(40)),
        Err(mpsc::RecvTimeoutError::Timeout)
    ));
    assert!(ticket.result().is_none());
    gate.unlock().unwrap();
    let failure = result
        .recv_timeout(Duration::from_secs(5))
        .unwrap()
        .unwrap_err();
    assert!(failure.contains("worker exited before publication completed"));
    waiter.join().unwrap();
    assert!(cache.shutdown(Duration::from_secs(5)));
    assert!(cache.owner.is_finished());
    assert!(cache.owner.panicked());
    let stats = cache.stats();
    assert_eq!(stats.pending_publish_bytes, 0);
    assert_eq!(stats.pending_disk_reservation_bytes, 0);
    assert!(cache.publication_failed("unwind"));
    let namespace = fs::read_dir(&cache.options().root)
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
    let block = fs::read_dir(&namespace)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| {
            path.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("block-")
        })
        .unwrap();
    let mut opening = OpenOptions::new();
    opening.read(true).write(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        opening.share_mode(0);
    }
    let actual = opening.open(&block).unwrap();
    assert_eq!(actual.metadata().unwrap().len(), HEADER_BYTES as u64);
    let physical_bytes: u64 = fs::read_dir(&cache.options().root)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .map(|path| {
            if path.is_dir() {
                fs::read_dir(path)
                    .unwrap()
                    .map(|entry| entry.unwrap().metadata().unwrap().len())
                    .sum()
            } else {
                fs::metadata(path).unwrap().len()
            }
        })
        .sum();
    assert!(stats.disk_bytes >= physical_bytes);
    assert!(stats.disk_bytes <= cache.options().disk_bytes);
    drop(actual);
    drop(lease);
}

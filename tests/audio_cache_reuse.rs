use lx_gpui_player::audio::cache::{CacheOptions, CacheStore};
use std::{
    fs,
    io::{Seek, SeekFrom, Write},
    path::Path,
    sync::{Arc, Mutex, atomic::AtomicBool},
    time::Duration,
};
static CASES: Mutex<()> = Mutex::new(());
fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}
fn directory() -> tempfile::TempDir {
    let root = std::env::temp_dir().join("lx-native-audio-reuse-tests");
    fs::create_dir_all(&root).unwrap();
    tempfile::Builder::new()
        .prefix("reuse-")
        .tempdir_in(root)
        .unwrap()
}
fn options(root: &Path) -> CacheOptions {
    CacheOptions {
        root: root.to_owned(),
        block_size: 1024,
        ram_bytes: 1024,
        disk_bytes: 128 * 1024,
        max_objects: 8,
        max_blocks: 16,
    }
}
fn store(root: &Path) -> Arc<CacheStore> {
    let cache = CacheStore::new(options(root)).unwrap();
    cache.wait_ready(Duration::from_secs(5)).unwrap();
    cache
}
fn cancel() -> Arc<AtomicBool> {
    Arc::new(AtomicBool::new(false))
}
fn key() -> String {
    "a".repeat(64)
}
fn seed(cache: &Arc<CacheStore>, object: &str) -> lx_gpui_player::audio::cache::CacheLease {
    let lease = cache.lease(object.into()).unwrap();
    assert!(
        runtime()
            .block_on(cache.register_reusable_representation_async(
                key(),
                object.into(),
                "\"version-one\"".into(),
                1041,
                cache.stats().epoch,
                cancel()
            ))
            .unwrap()
    );
    cache.publish(object, 0, &[0x35; 1024]).unwrap();
    cache.publish(object, 1024, &[0x67; 17]).unwrap();
    lease
}
fn paths(root: &Path) -> Vec<std::path::PathBuf> {
    let mut result = Vec::new();
    for namespace in fs::read_dir(root).unwrap() {
        let namespace = namespace.unwrap();
        if namespace.file_type().unwrap().is_dir() {
            for block in fs::read_dir(namespace.path()).unwrap() {
                let block = block.unwrap();
                if block.file_name().to_string_lossy().starts_with("block-") {
                    result.push(block.path());
                }
            }
        }
    }
    result.sort();
    result
}
fn physical(root: &Path) -> u64 {
    fs::read_dir(root)
        .unwrap()
        .map(|entry| {
            let entry = entry.unwrap();
            let metadata = fs::symlink_metadata(entry.path()).unwrap();
            if metadata.is_dir() {
                physical(&entry.path())
            } else {
                metadata.len()
            }
        })
        .sum()
}

#[test]
fn warm_views_share_verified_storage_and_pins_and_clear_cannot_unlink_another_active_alias() {
    let _case = CASES.lock().unwrap();
    let directory = directory();
    let root = directory.path().join("cache");
    let cache = store(&root);
    let original = seed(&cache, "stream-one");
    drop(original);
    let candidate = runtime()
        .block_on(cache.lookup_reusable_async(key(), cancel()))
        .unwrap()
        .unwrap();
    assert_eq!(candidate.offsets, vec![0, 1024]);
    assert_eq!(candidate.verified_bytes, 1041);
    let before = cache.stats().disk_bytes;
    let first = cache.lease("stream-two".into()).unwrap();
    let second = cache.lease("stream-three".into()).unwrap();
    assert_eq!(cache.bind_reusable("stream-two", &candidate).unwrap(), 2);
    assert_eq!(cache.bind_reusable("stream-three", &candidate).unwrap(), 2);
    let first_pin = cache.pin("stream-two", 0);
    let second_pin = cache.pin("stream-three", 0);
    assert_eq!(cache.stats().disk_bytes, before);
    assert_eq!(cache.stats().encoded_bytes, 1041);
    assert_eq!(cache.stats().blocks, 2);
    assert_eq!(cache.stats().pinned_blocks, 1);
    let stats = cache.clear().unwrap();
    assert_eq!(stats.deferred_blocks, 2);
    assert_eq!(stats.reusable_blocks, 0);
    let mut bytes = [0; 8];
    assert_eq!(cache.read("stream-two", 0, &mut bytes).unwrap(), Some(8));
    assert_eq!(bytes, [0x35; 8]);
    assert!(cache.bind_reusable("stream-two", &candidate).is_err());
    drop(first);
    drop(first_pin);
    assert_eq!(
        cache.read("stream-three", 1024, &mut bytes).unwrap(),
        Some(8)
    );
    assert_eq!(bytes, [0x67; 8]);
    assert_eq!(paths(&root).len(), 2);
    drop(second);
    drop(second_pin);
    let stats = cache.clear().unwrap();
    assert_eq!(stats.blocks, 0);
    assert_eq!(stats.retiring_blocks, 0);
    assert_eq!(stats.disk_bytes, physical(&root));
    assert!(cache.shutdown(Duration::from_secs(5)));
}

#[test]
fn cold_reopen_keeps_only_version_two_data_and_v1_transient_blocks_still_recover() {
    let _case = CASES.lock().unwrap();
    let directory = directory();
    let root = directory.path().join("cache");
    let cache = store(&root);
    let persistent = seed(&cache, "verified");
    let transient = cache.lease("transient".into()).unwrap();
    cache.publish("transient", 0, &[0x91; 1024]).unwrap();
    assert_eq!(paths(&root).len(), 3);
    drop(persistent);
    drop(transient);
    assert!(cache.shutdown(Duration::from_secs(5)));
    drop(cache);
    let reopened = store(&root);
    assert_eq!(paths(&root).len(), 2);
    assert!(reopened.stats().recovered_bytes >= 1144);
    let candidate = runtime()
        .block_on(reopened.lookup_reusable_async(key(), cancel()))
        .unwrap()
        .unwrap();
    let lease = reopened.lease("different-stream-serial".into()).unwrap();
    assert_eq!(
        reopened
            .bind_reusable("different-stream-serial", &candidate)
            .unwrap(),
        2
    );
    let mut output = [0; 17];
    assert_eq!(
        reopened
            .read("different-stream-serial", 1024, &mut output)
            .unwrap(),
        Some(17)
    );
    assert_eq!(output, [0x67; 17]);
    assert_eq!(reopened.stats().disk_bytes, physical(&root));
    drop(lease);
    assert!(reopened.shutdown(Duration::from_secs(5)));
}

#[test]
fn promotion_registers_existing_complete_blocks_and_persists_subsequent_foreground_blocks() {
    let _case = CASES.lock().unwrap();
    let directory = directory();
    let root = directory.path().join("cache");
    let cache = store(&root);
    let lease = cache.lease("promoted".into()).unwrap();
    cache.publish("promoted", 0, &[0x35; 1024]).unwrap();
    assert!(
        runtime()
            .block_on(cache.lookup_reusable_async(key(), cancel()))
            .unwrap()
            .is_none()
    );
    assert!(
        runtime()
            .block_on(cache.register_reusable_representation_async(
                key(),
                "promoted".into(),
                "\"version-one\"".into(),
                1041,
                0,
                cancel()
            ))
            .unwrap()
    );
    cache.publish("promoted", 1024, &[0x67; 17]).unwrap();
    assert_eq!(cache.stats().reusable_bytes, 1041);
    drop(lease);
    assert!(cache.shutdown(Duration::from_secs(5)));
    drop(cache);
    let reopened = store(&root);
    let candidate = runtime()
        .block_on(reopened.lookup_reusable_async(key(), cancel()))
        .unwrap()
        .unwrap();
    assert_eq!(candidate.verified_bytes, 1041);
    assert!(reopened.shutdown(Duration::from_secs(5)));
}

#[test]
fn cold_promotion_deduplicates_an_existing_verified_interval_without_dropping_its_owned_reader() {
    let _case = CASES.lock().unwrap();
    let directory = directory();
    let root = directory.path().join("cache");
    let cache = store(&root);
    let lease = seed(&cache, "seed");
    drop(lease);
    assert!(cache.shutdown(Duration::from_secs(5)));
    drop(cache);
    let cache = store(&root);
    let lease = cache.lease("speculative-before-promotion".into()).unwrap();
    cache
        .publish("speculative-before-promotion", 0, &[0x35; 1024])
        .unwrap();
    assert_eq!(paths(&root).len(), 3);
    assert!(
        runtime()
            .block_on(cache.register_reusable_representation_async(
                key(),
                "speculative-before-promotion".into(),
                "\"version-one\"".into(),
                1041,
                0,
                cancel()
            ))
            .unwrap()
    );
    assert_eq!(paths(&root).len(), 2);
    assert_eq!(cache.stats().reusable_bytes, 1041);
    assert_eq!(cache.stats().retiring_blocks, 0);
    assert_eq!(cache.stats().disk_bytes, physical(&root));
    let candidate = runtime()
        .block_on(cache.lookup_reusable_async(key(), cancel()))
        .unwrap()
        .unwrap();
    let view = cache.lease("another-view".into()).unwrap();
    cache.bind_reusable("another-view", &candidate).unwrap();
    let mut bytes = [0; 17];
    assert_eq!(
        cache.read("another-view", 1024, &mut bytes).unwrap(),
        Some(17)
    );
    assert_eq!(bytes, [0x67; 17]);
    drop(view);
    drop(lease);
    assert!(cache.shutdown(Duration::from_secs(5)));
}

#[test]
fn cold_body_replacement_is_a_miss_and_remains_preserved_and_charged_after_clear() {
    let _case = CASES.lock().unwrap();
    let directory = directory();
    let root = directory.path().join("cache");
    let cache = store(&root);
    let lease = seed(&cache, "original");
    drop(lease);
    assert!(cache.shutdown(Duration::from_secs(5)));
    drop(cache);
    let path = paths(&root)[0].clone();
    let mut changed = fs::OpenOptions::new().write(true).open(&path).unwrap();
    changed.seek(SeekFrom::Start(120 + 3)).unwrap();
    changed.write_all(&[0xee]).unwrap();
    changed.sync_all().unwrap();
    drop(changed);
    let original = fs::read(&path).unwrap();
    let reopened = store(&root);
    assert!(
        runtime()
            .block_on(reopened.lookup_reusable_async(key(), cancel()))
            .unwrap()
            .is_none()
    );
    assert!(reopened.stats().cannot_clear);
    assert!(reopened.clear().is_err());
    assert_eq!(fs::read(&path).unwrap(), original);
    assert!(reopened.stats().disk_bytes >= physical(&root));
    assert!(reopened.shutdown(Duration::from_secs(5)));
}

#[cfg(windows)]
#[test]
fn warm_verified_handles_deny_body_writes_and_path_deletion_until_worker_shutdown() {
    let _case = CASES.lock().unwrap();
    let directory = directory();
    let root = directory.path().join("cache");
    let cache = store(&root);
    let lease = seed(&cache, "immutable");
    let path = paths(&root)[0].clone();
    assert!(fs::OpenOptions::new().write(true).open(&path).is_err());
    assert!(fs::remove_file(&path).is_err());
    let candidate = runtime()
        .block_on(cache.lookup_reusable_async(key(), cancel()))
        .unwrap()
        .unwrap();
    drop(candidate);
    drop(lease);
    assert!(fs::OpenOptions::new().write(true).open(&path).is_err());
    assert!(cache.shutdown(Duration::from_secs(5)));
    assert!(fs::OpenOptions::new().write(true).open(&path).is_ok());
}

#[test]
fn clear_and_candidate_cas_fences_do_not_remove_a_newer_representation() {
    let _case = CASES.lock().unwrap();
    let directory = directory();
    let root = directory.path().join("cache");
    let cache = store(&root);
    let lease = seed(&cache, "old");
    let old = runtime()
        .block_on(cache.lookup_reusable_async(key(), cancel()))
        .unwrap()
        .unwrap();
    let next = cache.lease("new".into()).unwrap();
    cache.publish("new", 0, &[0x77; 1024]).unwrap();
    assert!(
        runtime()
            .block_on(cache.register_reusable_representation_async(
                key(),
                "new".into(),
                "\"version-two\"".into(),
                1024,
                0,
                cancel()
            ))
            .unwrap()
    );
    assert!(
        !runtime()
            .block_on(cache.invalidate_reusable_candidate_async(&old, cancel()))
            .unwrap()
    );
    let current = runtime()
        .block_on(cache.lookup_reusable_async(key(), cancel()))
        .unwrap()
        .unwrap();
    assert_eq!(current.strong_etag, "\"version-two\"");
    let target = cache.lease("reader".into()).unwrap();
    assert!(cache.bind_reusable("reader", &old).is_err());
    let mut changed = current.clone();
    changed.strong_etag = "\"forged\"".into();
    assert!(cache.bind_reusable("reader", &changed).is_err());
    let mut changed = current.clone();
    changed.total += 1;
    assert!(cache.bind_reusable("reader", &changed).is_err());
    let mut changed = current.clone();
    changed.block_size += 1;
    assert!(cache.bind_reusable("reader", &changed).is_err());
    let mut changed = current.clone();
    changed.verified_bytes += 1;
    assert!(cache.bind_reusable("reader", &changed).is_err());
    cache.bind_reusable("reader", &current).unwrap();
    let mut output = [0; 8];
    assert_eq!(cache.read("reader", 0, &mut output).unwrap(), Some(8));
    assert_eq!(output, [0x77; 8]);
    cache.clear().unwrap();
    assert!(
        !runtime()
            .block_on(cache.register_reusable_representation_async(
                key(),
                "new".into(),
                "\"version-two\"".into(),
                1024,
                0,
                cancel()
            ))
            .unwrap()
    );
    assert!(
        !runtime()
            .block_on(cache.register_reusable_representation_async(
                key(),
                "new".into(),
                "\"version-two\"".into(),
                1024,
                cache.stats().epoch,
                cancel()
            ))
            .unwrap()
    );
    drop(target);
    drop(next);
    drop(lease);
    cache.clear().unwrap();
    assert!(cache.shutdown(Duration::from_secs(5)));
}

#[test]
fn weak_etags_incomplete_intervals_and_cancelled_registration_never_create_reusable_admission() {
    let _case = CASES.lock().unwrap();
    let directory = directory();
    let cache = store(&directory.path().join("cache"));
    let lease = cache.lease("partial".into()).unwrap();
    cache.publish("partial", 0, &[3; 17]).unwrap();
    assert!(
        runtime()
            .block_on(cache.register_reusable_representation_async(
                key(),
                "partial".into(),
                "W/\"weak\"".into(),
                4096,
                0,
                cancel()
            ))
            .is_err()
    );
    assert!(
        !runtime()
            .block_on(cache.register_reusable_representation_async(
                key(),
                "partial".into(),
                "\"strong\"".into(),
                4096,
                0,
                cancel()
            ))
            .unwrap()
    );
    assert!(
        runtime()
            .block_on(cache.lookup_reusable_async(key(), cancel()))
            .unwrap()
            .is_none()
    );
    let cancelled = Arc::new(AtomicBool::new(true));
    assert!(
        runtime()
            .block_on(cache.register_reusable_representation_async(
                key(),
                "partial".into(),
                "\"strong\"".into(),
                17,
                0,
                cancelled
            ))
            .is_err()
    );
    drop(lease);
    assert!(cache.shutdown(Duration::from_secs(5)));
}

#[test]
fn invalidating_a_stream_clears_its_old_persistence_binding_before_any_new_body_is_published() {
    let _case = CASES.lock().unwrap();
    let directory = directory();
    let root = directory.path().join("cache");
    let cache = store(&root);
    let producer = seed(&cache, "producer");
    let old = runtime()
        .block_on(cache.lookup_reusable_async(key(), cancel()))
        .unwrap()
        .unwrap();
    let old_view = cache.lease("old-view".into()).unwrap();
    cache.bind_reusable("old-view", &old).unwrap();
    cache.invalidate("producer").unwrap();
    cache.publish("producer", 0, &[0x77; 1024]).unwrap();
    let unchanged = runtime()
        .block_on(cache.lookup_reusable_async(key(), cancel()))
        .unwrap()
        .unwrap();
    assert_eq!(unchanged.strong_etag, "\"version-one\"");
    let validator_view = cache.lease("validator-view".into()).unwrap();
    cache.bind_reusable("validator-view", &unchanged).unwrap();
    let mut bytes = [0; 8];
    assert_eq!(
        cache.read("validator-view", 0, &mut bytes).unwrap(),
        Some(8)
    );
    assert_eq!(bytes, [0x35; 8]);
    assert_eq!(cache.read("producer", 0, &mut bytes).unwrap(), Some(8));
    assert_eq!(bytes, [0x77; 8]);
    assert!(
        runtime()
            .block_on(cache.register_reusable_representation_async(
                key(),
                "producer".into(),
                "\"version-two\"".into(),
                1024,
                0,
                cancel()
            ))
            .unwrap()
    );
    let new = runtime()
        .block_on(cache.lookup_reusable_async(key(), cancel()))
        .unwrap()
        .unwrap();
    assert_eq!(new.strong_etag, "\"version-two\"");
    assert_eq!(cache.read("old-view", 0, &mut bytes).unwrap(), Some(8));
    assert_eq!(bytes, [0x35; 8]);
    drop(validator_view);
    drop(old_view);
    drop(producer);
    assert!(cache.shutdown(Duration::from_secs(5)));
}

#[test]
fn exact_full_storage_rejects_multi_ledger_maintenance_before_any_slot_changes_or_quota_growth() {
    let _case = CASES.lock().unwrap();
    let directory = directory();
    let root = directory.path().join("cache");
    let cache = store(&root);
    let lease = seed(&cache, "active");
    let quota = cache.options().disk_bytes;
    let filler = root.join("formal-user-data.bin");
    let file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&filler)
        .unwrap();
    let user_bytes = quota - physical(&root);
    file.set_len(user_bytes).unwrap();
    file.sync_all().unwrap();
    drop(file);
    let before = ledger_bytes(&root);
    assert_eq!(physical(&root), quota);
    let error = cache.clear().unwrap_err();
    assert!(error.to_string().contains("quota"), "{error:#}");
    assert_eq!(physical(&root), quota);
    assert_eq!(ledger_bytes(&root), before);
    assert!(cache.stats().cannot_clear);
    assert_eq!(
        cache.stats().last_io_error,
        lx_gpui_player::audio::cache::CacheIoErrorCode::Audit
    );
    assert_eq!(cache.stats().disk_bytes, quota);
    assert!(
        runtime()
            .block_on(cache.lookup_reusable_async(key(), cancel()))
            .unwrap()
            .is_none(),
        "failed maintenance must not resurrect the cleared index"
    );
    let mut bytes = [0; 8];
    assert_eq!(cache.read("active", 0, &mut bytes).unwrap(), Some(8));
    assert_eq!(bytes, [0x35; 8]);
    assert_eq!(fs::read(&filler).unwrap(), vec![0; user_bytes as usize]);
    drop(lease);
    assert!(cache.shutdown(Duration::from_secs(5)));
}

#[test]
fn a_complete_protected_view_keeps_every_interval_until_its_lease_releases_under_real_quota_pressure()
 {
    let _case = CASES.lock().unwrap();
    let directory = directory();
    let root = directory.path().join("cache");
    let mut configured = options(&root);
    configured.max_blocks = 2;
    let cache = CacheStore::new(configured).unwrap();
    cache.wait_ready(Duration::from_secs(5)).unwrap();
    let seed_lease = seed(&cache, "seed");
    drop(seed_lease);
    let candidate = runtime()
        .block_on(cache.lookup_reusable_async(key(), cancel()))
        .unwrap()
        .unwrap();
    let protected = cache.lease("complete-200-view".into()).unwrap();
    cache
        .bind_reusable("complete-200-view", &candidate)
        .unwrap();
    cache.protect_reusable_view("complete-200-view").unwrap();
    assert_eq!(cache.stats().protected_views, 1);
    assert_eq!(cache.stats().pinned_blocks, 0);
    let pressure = cache.lease("pressure".into()).unwrap();
    let before = cache.stats().disk_bytes;
    let error = cache.publish("pressure", 0, &[0x88; 1024]).unwrap_err();
    assert!(error.to_string().contains("quota"), "{error:#}");
    assert_eq!(cache.stats().disk_bytes, before);
    assert_eq!(paths(&root).len(), 2);
    let mut output = [0; 17];
    assert_eq!(
        cache.read("complete-200-view", 1024, &mut output).unwrap(),
        Some(17)
    );
    assert_eq!(output, [0x67; 17]);
    cache.clear().unwrap();
    assert_eq!(cache.stats().protected_views, 1);
    assert_eq!(
        cache.read("complete-200-view", 0, &mut output).unwrap(),
        Some(17)
    );
    assert_eq!(output, [0x35; 17]);
    drop(protected);
    assert_eq!(cache.stats().protected_views, 0);
    cache.publish("pressure", 0, &[0x88; 1024]).unwrap();
    assert_eq!(cache.read("pressure", 0, &mut output).unwrap(), Some(17));
    assert_eq!(output, [0x88; 17]);
    drop(pressure);
    cache.clear().unwrap();
    assert!(cache.shutdown(Duration::from_secs(5)));
}

#[test]
fn a_polling_reader_cannot_observe_the_last_reusable_body_before_its_durable_index_commit() {
    use std::{sync::atomic::Ordering, thread, time::Instant};
    let _case = CASES.lock().unwrap();
    let directory = directory();
    let root = directory.path().join("cache");
    let cache = store(&root);
    let lease = cache.lease("reader-visible".into()).unwrap();
    let cancellation = cancel();
    assert!(
        runtime()
            .block_on(cache.register_reusable_representation_async(
                key(),
                "reader-visible".into(),
                "\"complete\"".into(),
                1024,
                0,
                cancellation.clone()
            ))
            .unwrap()
    );
    let publisher = cache.clone();
    let signal = cancellation.clone();
    let work = thread::spawn(move || {
        runtime().block_on(publisher.publish_async(
            "reader-visible".into(),
            0,
            vec![0x39; 1024],
            signal,
        ))
    });
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut bytes = [0; 1024];
    loop {
        if cache
            .read("reader-visible", 0, &mut bytes)
            .unwrap()
            .is_some()
        {
            break;
        }
        assert!(Instant::now() < deadline, "last body did not publish");
        thread::yield_now();
    }
    assert_eq!(bytes, [0x39; 1024]);
    let newest = fs::read_dir(&root)
        .unwrap()
        .filter_map(|entry| {
            let entry = entry.unwrap();
            entry.file_type().unwrap().is_dir().then_some(entry.path())
        })
        .flat_map(|directory| [directory.join("manifest.0"), directory.join("manifest.1")])
        .filter_map(|path| {
            let bytes = fs::read(path).ok()?;
            serde_json::from_slice::<serde_json::Value>(&bytes[120..]).ok()
        })
        .max_by_key(|value| value["sequence"].as_u64().unwrap())
        .unwrap();
    assert!(newest["data"]["blocks"].as_object().unwrap().values().any(
        |block| block["reuse"]["representation"]["data_key"] == key()
            && block["reuse"]["representation"]["strong_etag"] == "\"complete\""
    ));
    cancellation.store(true, Ordering::Release);
    if let Err(error) = work.join().unwrap() {
        assert!(
            error.to_string().contains("cancel"),
            "unexpected publication error after the final read: {error:#}"
        );
    }
    drop(lease);
    assert!(cache.shutdown(Duration::from_secs(5)));
    drop(cache);
    let reopened = store(&root);
    let candidate = runtime()
        .block_on(reopened.lookup_reusable_async(key(), cancel()))
        .unwrap()
        .unwrap();
    assert_eq!(candidate.verified_bytes, 1024);
    assert!(reopened.shutdown(Duration::from_secs(5)));
}

fn ledger_bytes(root: &Path) -> std::collections::BTreeMap<std::path::PathBuf, Vec<u8>> {
    let mut result = std::collections::BTreeMap::new();
    for entry in fs::read_dir(root).unwrap() {
        let entry = entry.unwrap();
        if entry.file_type().unwrap().is_dir() {
            for child in fs::read_dir(entry.path()).unwrap() {
                let child = child.unwrap();
                if child.file_name().to_string_lossy().starts_with("manifest.") {
                    result.insert(child.path(), fs::read(child.path()).unwrap());
                }
            }
        } else if entry
            .file_name()
            .to_string_lossy()
            .starts_with(".lx-cache-registry.")
        {
            result.insert(entry.path(), fs::read(entry.path()).unwrap());
        }
    }
    result
}

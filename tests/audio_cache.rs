use lx_gpui_player::audio::cache::{CacheOptions, CacheStore};

fn store(
    directory: &tempfile::TempDir,
    ram: u64,
    disk: u64,
    objects: usize,
) -> std::sync::Arc<CacheStore> {
    store_with_blocks(directory, ram, disk, objects, 16)
}

fn store_with_blocks(
    directory: &tempfile::TempDir,
    ram: u64,
    disk: u64,
    objects: usize,
    blocks: usize,
) -> std::sync::Arc<CacheStore> {
    let cache = CacheStore::new(CacheOptions {
        root: directory.path().join("cache"),
        block_size: 1024,
        ram_bytes: ram,
        disk_bytes: disk,
        max_objects: objects,
        max_blocks: blocks,
    })
    .unwrap();
    cache.wait_ready(std::time::Duration::from_secs(5)).unwrap();
    cache
}

#[test]
fn lru_block_quota_respects_the_active_block_pin() {
    let directory = tempfile::tempdir().unwrap();
    let cache = store_with_blocks(&directory, 1024, 32 * 1024, 2, 2);
    let _lease = cache.lease("track".into()).unwrap();
    let _pin = cache.pin("track", 0);
    cache.publish("track", 0, &[1; 1024]).unwrap();
    cache.publish("track", 1024, &[2; 1024]).unwrap();
    cache.publish("track", 2048, &[3; 1024]).unwrap();
    let mut bytes = [0; 8];
    assert_eq!(cache.read("track", 0, &mut bytes).unwrap(), Some(8));
    assert_eq!(bytes, [1; 8]);
    assert!(!cache.contains("track", 1024));
    assert!(cache.contains("track", 2048));
    assert_eq!(cache.stats().encoded_bytes, 2048);
    assert!(cache.stats().disk_bytes <= 32 * 1024);
    assert!(cache.stats().ram_bytes <= 1024);
}

#[test]
fn clear_defers_active_lease_and_never_removes_adjacent_user_files() {
    let directory = tempfile::tempdir().unwrap();
    let cache = store(&directory, 1024, 32 * 1024, 2);
    let root = directory.path().join("cache");
    for name in [
        "original.wav",
        "download.mp3",
        "edited.lrc",
        "source-approval.json",
    ] {
        std::fs::write(root.join(name), name).unwrap();
    }
    let lease = cache.lease("active".into()).unwrap();
    cache.publish("active", 0, &[5; 1024]).unwrap();
    assert!(
        cache
            .clear()
            .unwrap_err()
            .to_string()
            .contains("cannotClear")
    );
    let statistics = cache.stats();
    assert!(statistics.cannot_clear);
    assert_eq!(statistics.foreign_files, 4);
    assert_eq!(statistics.epoch, 1);
    assert_eq!(statistics.deferred_blocks, 1);
    let mut bytes = [0; 1];
    assert_eq!(cache.read("active", 0, &mut bytes).unwrap(), Some(1));
    assert_eq!(bytes, [5]);
    drop(lease);
    assert_eq!(cache.stats().blocks, 0);
    for name in [
        "original.wav",
        "download.mp3",
        "edited.lrc",
        "source-approval.json",
    ] {
        assert_eq!(std::fs::read(root.join(name)).unwrap(), name.as_bytes());
    }
    drop(cache);
    assert!(root.join("original.wav").exists());
}

#[test]
fn object_quota_rejects_active_overflow_then_reclaims_inactive_lru() {
    let directory = tempfile::tempdir().unwrap();
    let cache = store(&directory, 1024, 32 * 1024, 1);
    let lease = cache.lease("first".into()).unwrap();
    cache.publish("first", 0, &[1; 1024]).unwrap();
    assert!(cache.lease("second".into()).is_err());
    drop(lease);
    let _lease = cache.lease("second".into()).unwrap();
    assert!(!cache.contains("first", 0));
    assert_eq!(cache.stats().objects, 1);
}

#[test]
fn ram_eviction_preserves_exact_disk_intervals_and_partial_final_block() {
    let directory = tempfile::tempdir().unwrap();
    let cache = store(&directory, 1024, 32 * 1024, 2);
    let _lease = cache.lease("track".into()).unwrap();
    for (offset, value) in [(0, 1), (1024, 2), (2048, 3)] {
        cache.publish("track", offset, &[value; 1024]).unwrap();
    }
    cache.publish("track", 3072, &[4; 19]).unwrap();
    let mut output = [0; 1024];
    assert_eq!(cache.read("track", 512, &mut output).unwrap(), Some(512));
    assert_eq!(&output[..512], &[1; 512]);
    assert_eq!(cache.read("track", 3072, &mut output).unwrap(), Some(19));
    assert_eq!(&output[..19], &[4; 19]);
    assert_eq!(cache.read("track", 3091, &mut output).unwrap(), None);
    assert!(cache.stats().ram_bytes <= 1024);
    assert_eq!(cache.stats().encoded_bytes, 3091);
    assert!(cache.stats().disk_bytes <= 32 * 1024);
}

#[test]
fn interval_and_quota_validation_rejects_unbounded_or_unaligned_blocks() {
    let directory = tempfile::tempdir().unwrap();
    let cache = store(&directory, 1024, 32 * 1024, 2);
    let _lease = cache.lease("track".into()).unwrap();
    assert!(cache.publish("track", 1, &[1; 8]).is_err());
    assert!(cache.publish("track", 0, &[]).is_err());
    assert!(cache.publish("track", 0, &[1; 1025]).is_err());
    assert!(cache.publish("missing", 0, &[1; 8]).is_err());
    let mut options = cache.options().clone();
    options.max_objects = 0;
    assert!(CacheStore::new(options).is_err());
}

#[test]
fn an_active_pin_alone_defers_clear_and_object_reclamation() {
    let directory = tempfile::tempdir().unwrap();
    let cache = store(&directory, 1024, 32 * 1024, 1);
    let lease = cache.lease("first".into()).unwrap();
    let pin = cache.pin("first", 0);
    cache.publish("first", 0, &[3; 1024]).unwrap();
    drop(lease);
    let stats = cache.clear().unwrap();
    assert_eq!(stats.deferred_blocks, 1);
    assert_eq!(stats.pinned_blocks, 1);
    assert!(cache.lease("second".into()).is_err());
    assert!(cache.contains("first", 0));
    drop(pin);
    assert_eq!(cache.stats().blocks, 0);
    assert!(cache.lease("second".into()).is_ok());
}

#[test]
fn failed_block_creation_does_not_overwrite_an_unregistered_file() {
    let directory = tempfile::tempdir().unwrap();
    let cache = store(&directory, 1024, 32 * 1024, 2);
    let _lease = cache.lease("track".into()).unwrap();
    let managed = std::fs::read_dir(directory.path().join("cache"))
        .unwrap()
        .find(|entry| entry.as_ref().unwrap().file_type().unwrap().is_dir())
        .unwrap()
        .unwrap()
        .path();
    let collision = managed.join("block-0000000000000002.bin");
    std::fs::write(&collision, b"formal-user-file").unwrap();
    assert!(cache.publish("track", 0, &[1; 1024]).is_err());
    assert_eq!(std::fs::read(&collision).unwrap(), b"formal-user-file");
    assert!(
        cache
            .clear()
            .unwrap_err()
            .to_string()
            .contains("cannotClear")
    );
    assert!(collision.exists());
    assert_eq!(cache.stats().encoded_bytes, 0);
    assert!(cache.stats().cannot_clear);
}

#[test]
fn clear_before_first_block_is_inherited_until_the_last_lease_and_pin_end() {
    let directory = tempfile::tempdir().unwrap();
    let cache = store(&directory, 2048, 32 * 1024, 2);
    let lease = cache.lease("active".into()).unwrap();
    let pin = cache.pin("active", 0);
    let cleared = cache.clear().unwrap();
    assert_eq!(cleared.blocks, 0);
    assert_eq!(cleared.epoch, 1);
    cache.publish("active", 0, &[1; 1024]).unwrap();
    cache.publish("active", 1024, &[2; 1024]).unwrap();
    assert_eq!(cache.stats().deferred_blocks, 2);
    drop(lease);
    cache.publish("active", 2048, &[3; 1024]).unwrap();
    assert_eq!(cache.stats().deferred_blocks, 3);
    assert_eq!(cache.stats().blocks, 3);
    drop(pin);
    assert_eq!(cache.stats().blocks, 0);
    assert_eq!(cache.stats().objects, 0);
    assert_eq!(cache.stats().encoded_bytes, 0);
    assert!(!cache.stats().cannot_clear);
}

#[test]
fn a_second_lease_keeps_clear_pending_for_blocks_published_after_clear() {
    let directory = tempfile::tempdir().unwrap();
    let cache = store(&directory, 2048, 32 * 1024, 2);
    let first = cache.lease("active".into()).unwrap();
    cache.publish("active", 0, &[1; 1024]).unwrap();
    cache.clear().unwrap();
    let second = cache.lease("active".into()).unwrap();
    drop(first);
    cache.publish("active", 1024, &[2; 1024]).unwrap();
    assert_eq!(cache.stats().deferred_blocks, 2);
    drop(second);
    assert_eq!(cache.stats().blocks, 0);
    assert_eq!(cache.stats().objects, 0);
}

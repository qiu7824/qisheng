use lx_gpui_player::audio::cache::{CacheOptions, CacheStore};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

const QUOTA: u64 = 32 * 1024;
const HEADER_BYTES: usize = 120;
const FIXTURE_MODE: &str = "LX_CACHE_RECOVERY_FIXTURE_MODE";
const FIXTURE_ROOT: &str = "LX_CACHE_RECOVERY_FIXTURE_ROOT";
const FIXTURE_READY: &str = "LX_CACHE_RECOVERY_FIXTURE_READY";
const FIXTURE_RELEASE: &str = "LX_CACHE_RECOVERY_FIXTURE_RELEASE";
const OBJECT: &str = "fixture-audio";

fn options(root: &Path) -> CacheOptions {
    CacheOptions {
        root: root.to_owned(),
        block_size: 1024,
        ram_bytes: 1024,
        disk_bytes: QUOTA,
        max_objects: 4,
        max_blocks: 16,
    }
}

fn store(root: &Path) -> Arc<CacheStore> {
    let cache = CacheStore::new(options(root)).unwrap();
    cache.wait_ready(Duration::from_secs(5)).unwrap();
    cache
}

fn is_link(metadata: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        metadata.file_attributes() & 0x400 != 0
    }
    #[cfg(not(windows))]
    {
        metadata.file_type().is_symlink()
    }
}

fn logical_file_length(directory: &Path) -> u64 {
    fs::read_dir(directory)
        .unwrap()
        .map(|entry| {
            let entry = entry.unwrap();
            let metadata = fs::symlink_metadata(entry.path()).unwrap();
            assert!(
                !is_link(&metadata),
                "quota inspection must not follow links"
            );
            if metadata.is_dir() {
                logical_file_length(&entry.path())
            } else {
                assert!(metadata.is_file());
                metadata.len()
            }
        })
        .sum()
}

fn namespaces(root: &Path) -> Vec<PathBuf> {
    let mut paths: Vec<_> = fs::read_dir(root)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            let metadata = fs::symlink_metadata(path).unwrap();
            metadata.is_dir()
                && !is_link(&metadata)
                && path
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with("managed-")
        })
        .collect();
    paths.sort();
    paths
}

fn blocks(namespace: &Path) -> Vec<PathBuf> {
    let mut paths: Vec<_> = fs::read_dir(namespace)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            let metadata = fs::symlink_metadata(path).unwrap();
            metadata.is_file()
                && !is_link(&metadata)
                && path
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with("block-")
        })
        .collect();
    paths.sort();
    paths
}

struct OwnerChild {
    child: Child,
    ready: PathBuf,
    release: PathBuf,
}

impl OwnerChild {
    fn spawn(directory: &tempfile::TempDir, mode: &str) -> Self {
        let root = directory.path().join("cache");
        let ready = directory.path().join("owner-ready");
        let release = directory.path().join("owner-release");
        let child = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "cache_owner_child_fixture", "--nocapture"])
            .env(FIXTURE_MODE, mode)
            .env(FIXTURE_ROOT, &root)
            .env(FIXTURE_READY, &ready)
            .env(FIXTURE_RELEASE, &release)
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        Self {
            child,
            ready,
            release,
        }
    }

    fn wait_ready(&mut self, root: &Path) -> PathBuf {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if self.ready.exists() {
                let name = fs::read_to_string(&self.ready).unwrap();
                assert!(name.starts_with("managed-"));
                assert!(!name.contains('/') && !name.contains('\\'));
                return root.join(name);
            }
            if let Some(status) = self.child.try_wait().unwrap() {
                panic!("cache owner fixture exited before readiness: {status}");
            }
            assert!(Instant::now() < deadline, "cache owner fixture timed out");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn wait_success(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                assert!(status.success(), "cache owner fixture failed: {status}");
                return;
            }
            assert!(
                Instant::now() < deadline,
                "cache owner fixture did not exit"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn release(&mut self) {
        fs::write(&self.release, b"release").unwrap();
        self.wait_success();
    }

    fn kill(&mut self) {
        self.child.kill().unwrap();
        let status = self.child.wait().unwrap();
        assert!(!status.success());
    }
}

impl Drop for OwnerChild {
    fn drop(&mut self) {
        if !matches!(self.child.try_wait(), Ok(Some(_))) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

#[test]
fn cache_owner_child_fixture() {
    let Some(mode) = std::env::var_os(FIXTURE_MODE) else {
        return;
    };
    let root = PathBuf::from(std::env::var_os(FIXTURE_ROOT).unwrap());
    let ready = PathBuf::from(std::env::var_os(FIXTURE_READY).unwrap());
    let release = PathBuf::from(std::env::var_os(FIXTURE_RELEASE).unwrap());
    assert_eq!(ready.parent(), root.parent());
    assert_eq!(release.parent(), root.parent());
    let cache = store(&root);
    let _lease = cache.lease(OBJECT.into()).unwrap();
    cache.publish(OBJECT, 0, &[0x5a; 1024]).unwrap();
    cache.publish(OBJECT, 1024, &[0x6b; 173]).unwrap();
    let namespace = namespaces(&root);
    assert_eq!(namespace.len(), 1);
    let ready_pending = ready.with_extension("pending");
    fs::write(
        &ready_pending,
        namespace[0].file_name().unwrap().as_encoded_bytes(),
    )
    .unwrap();
    fs::rename(ready_pending, &ready).unwrap();
    match mode.to_str().unwrap() {
        "exit" => std::process::exit(0),
        "hold" => {
            let deadline = Instant::now() + Duration::from_secs(60);
            while !release.exists() {
                assert!(Instant::now() < deadline, "cache owner was never released");
                std::thread::sleep(Duration::from_millis(20));
            }
            let mut bytes = [0; 1024];
            assert_eq!(cache.read(OBJECT, 0, &mut bytes).unwrap(), Some(1024));
            assert_eq!(bytes, [0x5a; 1024]);
            std::process::exit(0);
        }
        mode => panic!("unknown cache owner fixture mode: {mode}"),
    }
}

#[test]
fn abrupt_exit_reclaims_registered_blocks_without_reusing_media() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("cache");
    let mut child = OwnerChild::spawn(&directory, "exit");
    let namespace = child.wait_ready(&root);
    child.wait_success();
    let files = blocks(&namespace);
    assert_eq!(files.len(), 2);
    let physical_blocks: u64 = files
        .iter()
        .map(|path| fs::metadata(path).unwrap().len())
        .sum();
    assert_eq!(physical_blocks, 1197 + 2 * HEADER_BYTES as u64);

    let cache = store(&root);
    let stats = cache.stats();
    assert_eq!(stats.blocks, 0);
    assert_eq!(stats.encoded_bytes, 0);
    assert_eq!(stats.recovered_bytes, physical_blocks);
    assert_eq!(stats.namespaces, 1);
    assert!(!stats.cannot_clear);
    assert!(!namespace.exists());
    assert!(files.iter().all(|path| !path.exists()));
    assert_eq!(stats.disk_bytes, logical_file_length(&root));
    assert!(stats.disk_bytes > 0 && stats.disk_bytes <= QUOTA);
    let _lease = cache.lease(OBJECT.into()).unwrap();
    assert_eq!(cache.read(OBJECT, 0, &mut [0; 8]).unwrap(), None);
    for name in [
        ".lx-cache-owner.lock",
        ".lx-cache-registry.0",
        ".lx-cache-registry.1",
    ] {
        assert!(root.join(name).is_file());
    }
}

#[test]
fn live_process_owner_survives_constructor_and_clear_until_release() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("cache");
    let mut child = OwnerChild::spawn(&directory, "hold");
    let namespace = child.wait_ready(&root);
    let files = blocks(&namespace);
    let original: Vec<_> = files.iter().map(|path| fs::read(path).unwrap()).collect();
    let physical_blocks: u64 = original.iter().map(|bytes| bytes.len() as u64).sum();
    let observer = store(&root);
    assert_eq!(observer.stats().namespaces, 2);
    assert_eq!(observer.stats().blocks, 0);
    assert_eq!(observer.stats().encoded_bytes, 0);
    let _lease = observer.lease(OBJECT.into()).unwrap();
    assert_eq!(observer.read(OBJECT, 0, &mut [0; 8]).unwrap(), None);
    let stats = observer.clear().unwrap();
    assert_eq!(stats.recovered_bytes, 0);
    assert_eq!(stats.namespaces, 2);
    assert_eq!(stats.disk_bytes, logical_file_length(&root));
    for (path, bytes) in files.iter().zip(original) {
        assert_eq!(fs::read(path).unwrap(), bytes);
    }
    assert!(child.child.try_wait().unwrap().is_none());

    child.release();
    let stats = observer.clear().unwrap();
    assert_eq!(stats.recovered_bytes, physical_blocks);
    assert_eq!(stats.namespaces, 1);
    assert_eq!(stats.disk_bytes, logical_file_length(&root));
    assert!(!namespace.exists());
}

#[test]
fn killed_owner_releases_the_os_lock_and_reopen_recovers_its_namespace() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("cache");
    let mut child = OwnerChild::spawn(&directory, "hold");
    let namespace = child.wait_ready(&root);
    let bytes: u64 = blocks(&namespace)
        .iter()
        .map(|path| fs::metadata(path).unwrap().len())
        .sum();
    assert!(child.child.try_wait().unwrap().is_none());
    child.kill();

    let cache = store(&root);
    assert_eq!(cache.stats().blocks, 0);
    assert_eq!(cache.stats().encoded_bytes, 0);
    assert_eq!(cache.stats().recovered_bytes, bytes);
    assert_eq!(cache.stats().namespaces, 1);
    assert!(!namespace.exists());
}

#[test]
fn multiple_live_stores_share_a_physical_quota_including_metadata_and_headers() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("cache");
    let first = store(&root);
    let second = store(&root);
    let _first_lease = first.lease("first".into()).unwrap();
    let _second_lease = second.lease("second".into()).unwrap();
    let mut pins = Vec::new();
    let mut accepted = [0_u64; 2];
    let mut rejected = 0;
    for index in 0..17 {
        for (slot, cache, object, value) in
            [(0, &first, "first", 0x41), (1, &second, "second", 0x42)]
        {
            let offset = index * 1024;
            match cache.publish(object, offset, &[value; 1024]) {
                Ok(()) => {
                    accepted[slot] += 1024;
                    pins.push(cache.pin(object, offset));
                    assert_eq!(cache.stats().encoded_bytes, accepted[slot]);
                    assert_eq!(cache.stats().disk_bytes, logical_file_length(&root));
                }
                Err(error) => {
                    assert!(error.to_string().contains("quota"), "{error:#}");
                    rejected += 1;
                }
            }
            assert!(logical_file_length(&root) <= QUOTA);
        }
    }
    assert!(accepted.iter().all(|bytes| *bytes > 0));
    assert!(rejected > 0);
    let first_stats = first.clear().unwrap();
    let second_stats = second.clear().unwrap();
    assert_eq!(first_stats.namespaces, 2);
    assert_eq!(second_stats.namespaces, 2);
    assert_eq!(second_stats.disk_bytes, logical_file_length(&root));
    assert!(second_stats.disk_bytes > accepted.iter().sum::<u64>());
    assert!(second_stats.disk_bytes <= QUOTA);
    assert_eq!(first_stats.encoded_bytes, accepted[0]);
    assert_eq!(second_stats.encoded_bytes, accepted[1]);
}

#[test]
fn clear_reports_unknown_regular_files_and_preserves_their_exact_bytes() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("cache");
    let cache = store(&root);
    let lease = cache.lease("owned".into()).unwrap();
    cache.publish("owned", 0, &[9; 1024]).unwrap();
    drop(lease);
    let unknown = [
        ("download.mp3", b"user audio".as_slice()),
        (".lx-cache-registry.2", b"user registry".as_slice()),
        ("block-0123456789abcdef.bin", b"user block".as_slice()),
    ];
    for (name, bytes) in unknown {
        fs::write(root.join(name), bytes).unwrap();
    }
    let error = cache.clear().unwrap_err();
    assert!(error.to_string().contains("cannotClear"), "{error:#}");
    let stats = cache.stats();
    assert!(stats.cannot_clear);
    assert_eq!(stats.foreign_files, unknown.len());
    assert_eq!(
        stats.foreign_bytes,
        unknown
            .iter()
            .map(|(_, bytes)| bytes.len() as u64)
            .sum::<u64>()
    );
    assert_eq!(stats.blocks, 0);
    assert_eq!(stats.encoded_bytes, 0);
    assert_eq!(stats.disk_bytes, logical_file_length(&root));
    drop(cache);
    let reopened = store(&root);
    assert_eq!(reopened.stats().blocks, 0);
    assert_eq!(reopened.stats().foreign_files, unknown.len());
    assert!(
        reopened
            .clear()
            .unwrap_err()
            .to_string()
            .contains("cannotClear")
    );
    for (name, bytes) in unknown {
        assert_eq!(fs::read(root.join(name)).unwrap(), bytes);
    }
}

#[test]
fn unknown_namespace_with_a_managed_name_is_never_claimed_or_recursively_deleted() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("cache");
    let cache = store(&root);
    let unknown = root.join("managed-ffffffffffffffffffffffffffffffff");
    let nested = unknown.join("nested");
    fs::create_dir_all(&nested).unwrap();
    let sentinel = nested.join("original.wav");
    fs::write(&sentinel, b"formal user audio").unwrap();
    assert!(
        cache
            .clear()
            .unwrap_err()
            .to_string()
            .contains("cannotClear")
    );
    assert!(cache.stats().cannot_clear);
    assert!(cache.stats().foreign_files >= 1);
    assert_eq!(fs::read(&sentinel).unwrap(), b"formal user audio");
    drop(cache);
    let rejected = CacheStore::new(options(&root)).unwrap();
    let error = rejected.wait_ready(Duration::from_secs(5)).unwrap_err();
    assert!(error.to_string().contains("cannotClear"), "{error:#}");
    assert_eq!(fs::read(&sentinel).unwrap(), b"formal user audio");
    assert!(unknown.is_dir());
}

#[test]
fn dead_registered_namespace_preserves_an_unregistered_block_name() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("cache");
    let mut child = OwnerChild::spawn(&directory, "exit");
    let namespace = child.wait_ready(&root);
    child.wait_success();
    let registered = blocks(&namespace);
    let foreign = namespace.join("block-ffffffffffffffff.bin");
    fs::write(&foreign, b"formal user file").unwrap();

    let cache = store(&root);
    assert!(registered.iter().all(|path| !path.exists()));
    assert_eq!(cache.stats().blocks, 0);
    assert_eq!(cache.stats().encoded_bytes, 0);
    assert!(cache.stats().cannot_clear);
    assert_eq!(cache.stats().foreign_files, 1);
    assert_eq!(cache.stats().foreign_bytes, 16);
    assert_eq!(cache.stats().disk_bytes, logical_file_length(&root));
    assert!(
        cache
            .clear()
            .unwrap_err()
            .to_string()
            .contains("cannotClear")
    );
    drop(cache);
    assert_eq!(fs::read(&foreign).unwrap(), b"formal user file");
    assert!(namespace.is_dir());
}

#[test]
fn a_registered_name_with_a_replaced_ownership_header_is_preserved_and_reported() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("cache");
    let mut child = OwnerChild::spawn(&directory, "exit");
    let namespace = child.wait_ready(&root);
    child.wait_success();
    let registered = blocks(&namespace);
    assert_eq!(registered.len(), 2);
    fs::write(&registered[0], b"formal user replacement").unwrap();

    let cache = store(&root);
    let stats = cache.stats();
    assert_eq!(stats.blocks, 0);
    assert_eq!(stats.encoded_bytes, 0);
    assert!(stats.cannot_clear);
    assert!(stats.foreign_files >= 1);
    assert!(stats.foreign_bytes >= b"formal user replacement".len() as u64);
    assert!(stats.disk_bytes >= logical_file_length(&root));
    assert!(stats.disk_bytes <= QUOTA);
    assert!(!registered[1].exists());
    assert!(
        cache
            .clear()
            .unwrap_err()
            .to_string()
            .contains("cannotClear")
    );
    drop(cache);
    assert_eq!(
        fs::read(&registered[0]).unwrap(),
        b"formal user replacement"
    );
}

fn corrupt_newest_checksum(namespace: &Path) {
    let (path, mut bytes, mut value) = ["manifest.0", "manifest.1"]
        .into_iter()
        .map(|name| {
            let path = namespace.join(name);
            let bytes = fs::read(&path).unwrap();
            let value: serde_json::Value = serde_json::from_slice(&bytes[HEADER_BYTES..]).unwrap();
            (path, bytes, value)
        })
        .max_by_key(|(_, _, value)| value["sequence"].as_u64().unwrap())
        .unwrap();
    value["checksum"] = serde_json::Value::String("f".repeat(64));
    bytes.truncate(HEADER_BYTES);
    bytes.extend(serde_json::to_vec(&value).unwrap());
    fs::write(path, bytes).unwrap();
}

#[test]
fn a_bad_newest_manifest_falls_back_and_preserves_blocks_outside_the_trusted_snapshot() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("cache");
    let mut child = OwnerChild::spawn(&directory, "exit");
    let namespace = child.wait_ready(&root);
    child.wait_success();
    let registered = blocks(&namespace);
    assert_eq!(registered.len(), 2);
    let newer = fs::read(&registered[1]).unwrap();
    let older_length = fs::metadata(&registered[0]).unwrap().len();
    corrupt_newest_checksum(&namespace);

    let cache = store(&root);
    assert!(!registered[0].exists());
    assert_eq!(fs::read(&registered[1]).unwrap(), newer);
    assert_eq!(cache.stats().recovered_bytes, older_length);
    assert_eq!(cache.stats().blocks, 0);
    assert!(cache.stats().cannot_clear);
    assert_eq!(cache.stats().foreign_files, 1);
    assert_eq!(cache.stats().foreign_bytes, newer.len() as u64);
    assert!(
        cache
            .clear()
            .unwrap_err()
            .to_string()
            .contains("cannotClear")
    );
    drop(cache);
    assert_eq!(fs::read(&registered[1]).unwrap(), newer);
}

#[test]
fn an_interrupted_body_with_an_intact_registered_header_is_reclaimed() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("cache");
    let mut child = OwnerChild::spawn(&directory, "exit");
    let namespace = child.wait_ready(&root);
    child.wait_success();
    let registered = blocks(&namespace);
    let file = fs::OpenOptions::new()
        .write(true)
        .open(&registered[0])
        .unwrap();
    file.set_len(HEADER_BYTES as u64 + 7).unwrap();
    drop(file);
    let physical: u64 = registered
        .iter()
        .map(|path| fs::metadata(path).unwrap().len())
        .sum();

    let cache = store(&root);
    assert_eq!(cache.stats().recovered_bytes, physical);
    assert_eq!(cache.stats().blocks, 0);
    assert_eq!(cache.stats().encoded_bytes, 0);
    assert!(!cache.stats().cannot_clear);
    assert!(!namespace.exists());
    assert_eq!(cache.stats().disk_bytes, logical_file_length(&root));
}

#[cfg(windows)]
struct Junction(PathBuf);

#[cfg(windows)]
impl Junction {
    fn new(path: &Path, target: &Path) -> Self {
        let output = Command::new("cmd.exe")
            .args(["/D", "/C", "mklink", "/J"])
            .arg(path)
            .arg(target)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "junction fixture failed: {output:?}"
        );
        assert!(is_link(&fs::symlink_metadata(path).unwrap()));
        Self(path.to_owned())
    }
}

#[cfg(windows)]
impl Drop for Junction {
    fn drop(&mut self) {
        // Remove only the fixture's reparse point, never its target tree.
        fs::remove_dir(&self.0).unwrap();
    }
}

#[cfg(windows)]
#[test]
fn windows_junctions_are_rejected_without_following_or_deleting_their_targets() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("cache");
    let target = directory.path().join("formal-target");
    fs::create_dir(&target).unwrap();
    let sentinel = target.join("original.wav");
    fs::write(&sentinel, b"preserved formal audio").unwrap();
    let cache = store(&root);
    let link_path = root.join("managed-ffffffffffffffffffffffffffffffff");
    let junction = Junction::new(&link_path, &target);
    assert!(
        cache
            .clear()
            .unwrap_err()
            .to_string()
            .contains("cannotClear")
    );
    assert!(cache.stats().cannot_clear);
    assert!(cache.stats().foreign_files >= 1);
    let rejected = CacheStore::new(options(&root)).unwrap();
    let error = rejected.wait_ready(Duration::from_secs(5)).unwrap_err();
    assert!(error.to_string().contains("cannotClear"), "{error:#}");
    assert_eq!(fs::read(&sentinel).unwrap(), b"preserved formal audio");
    assert!(link_path.exists());
    drop(junction);
    cache.clear().unwrap();
    assert_eq!(fs::read(&sentinel).unwrap(), b"preserved formal audio");

    let ancestor_path = directory.path().join("linked-ancestor");
    let ancestor = Junction::new(&ancestor_path, &target);
    let rejected = CacheStore::new(options(&ancestor_path.join("child-cache"))).unwrap();
    let error = rejected.wait_ready(Duration::from_secs(5)).unwrap_err();
    assert!(error.to_string().contains("cannotClear"), "{error:#}");
    assert!(!target.join("child-cache").exists());
    drop(ancestor);
    assert_eq!(fs::read(&sentinel).unwrap(), b"preserved formal audio");
}

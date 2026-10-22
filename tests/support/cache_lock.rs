use lx_gpui_player::audio::cache::{CacheOptions, CacheStats, CacheStore};
use std::{
    fs::{self, OpenOptions, TryLockError},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{Arc, Mutex, mpsc},
    thread,
    time::{Duration, Instant},
};

const FIXTURE_LOCK: &str = "LX_CACHE_LATENCY_LOCK";
const FIXTURE_READY: &str = "LX_CACHE_LATENCY_READY";
const FIXTURE_RELEASE: &str = "LX_CACHE_LATENCY_RELEASE";
const ROOT_LOCK: &str = ".lx-cache-owner.lock";
pub static CASES: Mutex<()> = Mutex::new(());

fn assert_not_link(path: &Path) {
    let metadata = fs::symlink_metadata(path).unwrap();
    assert!(
        !metadata.file_type().is_symlink(),
        "fixture paths must not follow links"
    );
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        assert_eq!(
            metadata.file_attributes() & 0x400,
            0,
            "fixture paths must not follow reparse points"
        );
    }
}

fn assert_fixture_scope(directory: &Path) {
    assert!(directory.is_absolute(), "fixture paths must be absolute");
    let temp_root = std::env::temp_dir().canonicalize().unwrap();
    let resolved = directory.canonicalize().unwrap();
    assert!(
        resolved.starts_with(&temp_root),
        "cache latency fixtures must stay within the configured temporary root"
    );
    assert!(resolved.is_dir());
    for ancestor in directory.ancestors() {
        let resolved = ancestor.canonicalize().unwrap();
        assert!(resolved.starts_with(&temp_root));
        assert_not_link(ancestor);
        if resolved == temp_root {
            return;
        }
    }
    panic!("fixture path has no ancestor equal to the configured temporary root");
}

pub fn fixture_child() {
    let Some(lock) = std::env::var_os(FIXTURE_LOCK) else {
        return;
    };
    let lock = PathBuf::from(lock);
    let ready = PathBuf::from(std::env::var_os(FIXTURE_READY).unwrap());
    let release = PathBuf::from(std::env::var_os(FIXTURE_RELEASE).unwrap());
    assert_fixture_scope(ready.parent().unwrap());
    assert_fixture_scope(lock.parent().unwrap());
    assert_eq!(ready.parent(), release.parent());
    assert!(lock.starts_with(ready.parent().unwrap()));
    assert!(
        lock.canonicalize()
            .unwrap()
            .starts_with(ready.parent().unwrap().canonicalize().unwrap())
    );
    assert_eq!(lock.file_name().unwrap(), ROOT_LOCK);
    assert_not_link(&lock);
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&lock)
        .unwrap();
    assert!(file.metadata().unwrap().is_file());
    file.lock().unwrap();
    let pending = ready.with_extension("pending");
    fs::write(&pending, b"registry OS lock held").unwrap();
    fs::rename(pending, ready).unwrap();
    let deadline = Instant::now() + Duration::from_secs(90);
    while !release.exists() {
        assert!(
            Instant::now() < deadline,
            "registry lock fixture was never released"
        );
        thread::sleep(Duration::from_millis(5));
    }
    file.unlock().unwrap();
}

pub struct RegistryLock {
    child: Child,
    path: PathBuf,
    release_path: PathBuf,
}
impl RegistryLock {
    pub fn hold(directory: &Path, root: &Path) -> Self {
        assert_fixture_scope(directory);
        assert_fixture_scope(root);
        assert!(root.starts_with(directory));
        assert!(
            root.canonicalize()
                .unwrap()
                .starts_with(directory.canonicalize().unwrap())
        );
        let path = root.join(ROOT_LOCK);
        assert_not_link(&path);
        assert!(
            path.is_file(),
            "the production cache must create its own valid registry lock"
        );
        let ready = directory.join("registry-lock-ready");
        let release_path = directory.join("registry-lock-release");
        assert!(!ready.exists() && !release_path.exists());
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "cache_registry_lock_child_fixture",
                "--nocapture",
            ])
            .env(FIXTURE_LOCK, &path)
            .env(FIXTURE_READY, &ready)
            .env(FIXTURE_RELEASE, &release_path)
            .current_dir(directory)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x0800_0000);
        }
        let child = command.spawn().unwrap();
        let mut guard = Self {
            child,
            path,
            release_path,
        };
        let deadline = Instant::now() + Duration::from_secs(8);
        while !ready.exists() {
            if let Some(status) = guard.child.try_wait().unwrap() {
                panic!("registry lock fixture exited before acquiring its lock: {status}");
            }
            assert!(
                Instant::now() < deadline,
                "registry lock fixture did not acknowledge its OS lock"
            );
            thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(fs::read(ready).unwrap(), b"registry OS lock held");
        guard.assert_held();
        guard
    }
    pub fn assert_held(&mut self) {
        assert!(self.child.try_wait().unwrap().is_none());
        let probe = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&self.path)
            .unwrap();
        assert!(
            matches!(probe.try_lock(), Err(TryLockError::WouldBlock)),
            "a separate process must genuinely own the registry OS lock"
        );
    }
    pub fn release(&mut self) {
        if self.child.try_wait().unwrap().is_some() {
            return;
        }
        fs::write(&self.release_path, b"release").unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                assert!(status.success());
                break;
            }
            assert!(
                Instant::now() < deadline,
                "registry lock fixture did not exit after release"
            );
            thread::sleep(Duration::from_millis(5));
        }
    }
}
impl Drop for RegistryLock {
    fn drop(&mut self) {
        if !matches!(self.child.try_wait(), Ok(Some(_))) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

pub fn fresh_directory(prefix: &str) -> tempfile::TempDir {
    let root = std::env::temp_dir().join("lx-native-cache-latency-tests");
    fs::create_dir_all(&root).unwrap();
    assert_fixture_scope(&root);
    let directory = tempfile::Builder::new()
        .prefix(prefix)
        .tempdir_in(root)
        .unwrap();
    assert_fixture_scope(directory.path());
    directory
}

pub fn seed(root: &Path) {
    let cache = CacheStore::new(CacheOptions {
        root: root.into(),
        ..Default::default()
    })
    .unwrap();
    cache.wait_ready(Duration::from_secs(5)).unwrap();
    assert!(
        cache.shutdown(Duration::from_secs(5)),
        "cache seed must physically join its I/O owner"
    );
}

pub fn wait_registry_wait(stats: impl Fn() -> Option<CacheStats>) -> CacheStats {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(stats) = stats()
            && stats.io_active
            && stats.io_waiting_for_registry
        {
            return stats;
        }
        assert!(
            Instant::now() < deadline,
            "production I/O diagnostics did not confirm an actual WouldBlock on the held registry lock"
        );
        thread::sleep(Duration::from_millis(2));
    }
}

pub fn measured<T: Send + 'static>(
    lock: &mut RegistryLock,
    operation: impl FnOnce() -> T + Send + 'static,
) -> (T, Duration) {
    measured_with_watchdog(lock, Duration::from_secs(1), operation)
}

pub fn measured_with_watchdog<T: Send + 'static>(
    lock: &mut RegistryLock,
    timeout: Duration,
    operation: impl FnOnce() -> T + Send + 'static,
) -> (T, Duration) {
    let (reply, result) = mpsc::sync_channel(1);
    let started = Instant::now();
    let worker = thread::spawn(move || {
        let value = operation();
        let _ = reply.send(value);
    });
    let value = match result.recv_timeout(timeout) {
        Ok(value) => value,
        Err(error) => {
            lock.release();
            let _ = worker.join();
            panic!(
                "operation was blocked by the cache filesystem beyond its latency watchdog: {error}"
            );
        }
    };
    let elapsed = started.elapsed();
    worker.join().unwrap();
    (value, elapsed)
}

pub fn timed<T>(operation: impl FnOnce() -> T) -> (T, Duration) {
    let started = Instant::now();
    let value = operation();
    (value, started.elapsed())
}

pub fn assert_p95(mut samples: Vec<Duration>, operation: &str) {
    assert!(samples.len() >= 20);
    samples.sort_unstable();
    let p95 = samples[(samples.len() * 95).div_ceil(100) - 1];
    assert!(
        p95 < Duration::from_millis(100),
        "{operation} p95 exceeded 100 ms while the real registry OS lock was held: {p95:?}"
    );
}

pub fn logical_bytes(directory: &Path) -> u64 {
    fs::read_dir(directory)
        .unwrap()
        .map(|entry| {
            let path = entry.unwrap().path();
            let metadata = fs::symlink_metadata(&path).unwrap();
            #[cfg(windows)]
            {
                use std::os::windows::fs::MetadataExt;
                assert_eq!(
                    metadata.file_attributes() & 0x400,
                    0,
                    "quota audit must not follow a reparse point"
                );
            }
            assert!(!metadata.file_type().is_symlink());
            if metadata.is_dir() {
                logical_bytes(&path)
            } else {
                assert!(metadata.is_file());
                metadata.len()
            }
        })
        .sum()
}

pub fn wait_initialized(cache: &Arc<CacheStore>) {
    cache.wait_ready(Duration::from_secs(5)).unwrap();
    assert!(cache.stats().initialized && !cache.stats().initializing);
}

pub fn wait_worker_count(expected: usize) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while CacheStore::outstanding_workers() != expected {
        assert!(
            Instant::now() < deadline,
            "cache I/O owners were not physically joined: expected {expected}, actual {}",
            CacheStore::outstanding_workers()
        );
        thread::sleep(Duration::from_millis(2));
    }
}

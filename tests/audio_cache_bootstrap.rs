use lx_gpui_player::audio::cache::{CacheOptions, CacheStore};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    fs::{self, OpenOptions},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tempfile::TempDir;

const HEADER_BYTES: usize = 120;
const ROOT_LOCK: &str = ".lx-cache-owner.lock";
const ROOT_SLOTS: [&str; 2] = [".lx-cache-registry.0", ".lx-cache-registry.1"];
const QUOTA: u64 = 32 * 1024;

fn options(root: &Path, quota: u64) -> CacheOptions {
    CacheOptions {
        root: root.to_owned(),
        block_size: 128,
        ram_bytes: 1024,
        disk_bytes: quota,
        max_objects: 4,
        max_blocks: 16,
    }
}

fn ready(root: &Path) -> Arc<CacheStore> {
    let cache = CacheStore::new(options(root, QUOTA)).unwrap();
    cache.wait_ready(Duration::from_secs(5)).unwrap();
    cache
}

struct OriginalBootstrap {
    directory: TempDir,
    original_root: PathBuf,
    lock: Vec<u8>,
    empty_slot: Vec<u8>,
    populated_slot: Vec<u8>,
}

impl OriginalBootstrap {
    fn new() -> Self {
        let base = std::env::temp_dir().join("lx-native-cache-bootstrap-tests");
        fs::create_dir_all(&base).unwrap();
        let directory = tempfile::Builder::new()
            .prefix("bootstrap-")
            .tempdir_in(base)
            .unwrap();
        let original_root = directory.path().join("original");
        let cache = ready(&original_root);
        assert!(cache.shutdown(Duration::from_secs(5)));
        drop(cache);
        let lock = fs::read(original_root.join(ROOT_LOCK)).unwrap();
        assert_eq!(lock.len(), HEADER_BYTES);
        assert_eq!(&lock[..16], b"LXENCODEDCACHE01");
        assert_eq!(&lock[80..88], b"REGLOCK!");
        let mut empty_slot = None;
        let mut populated_slot = None;
        for name in ROOT_SLOTS {
            let bytes = fs::read(original_root.join(name)).unwrap();
            assert!(bytes.len() > HEADER_BYTES);
            assert_eq!(&bytes[16..48], &lock[16..48]);
            assert_eq!(&bytes[80..88], b"REGSLOT!");
            let document: Value = serde_json::from_slice(&bytes[HEADER_BYTES..]).unwrap();
            let namespaces = document["data"]["namespaces"].as_array().unwrap();
            if namespaces.is_empty() {
                empty_slot = Some(bytes);
            } else {
                populated_slot = Some(bytes);
            }
        }
        Self {
            directory,
            original_root,
            lock,
            empty_slot: empty_slot.expect("original root retains an empty bootstrap snapshot"),
            populated_slot: populated_slot.expect("actual initialization registered its namespace"),
        }
    }

    fn interrupted(&self, name: &str, present: [bool; 2]) -> PathBuf {
        let root = self.directory.path().join(name);
        fs::create_dir(&root).unwrap();
        fs::write(root.join(ROOT_LOCK), &self.lock).unwrap();
        // Each fixture uses an actual original empty owned ledger, after its
        // worker has joined. Only its JSON payload is truncated to model cancel.
        for (slot, present) in present.into_iter().enumerate() {
            if present {
                let path = root.join(ROOT_SLOTS[slot]);
                fs::write(&path, &self.empty_slot).unwrap();
                let file = OpenOptions::new().write(true).open(path).unwrap();
                file.set_len(HEADER_BYTES as u64).unwrap();
                file.sync_all().unwrap();
            }
        }
        root
    }
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

#[derive(Debug, PartialEq, Eq)]
enum Entry {
    Directory,
    File(Vec<u8>),
    Link(PathBuf),
}

fn image(root: &Path) -> BTreeMap<PathBuf, Entry> {
    fn visit(root: &Path, directory: &Path, output: &mut BTreeMap<PathBuf, Entry>) {
        for item in fs::read_dir(directory).unwrap() {
            let path = item.unwrap().path();
            let relative = path.strip_prefix(root).unwrap().to_owned();
            let metadata = fs::symlink_metadata(&path).unwrap();
            if is_link(&metadata) {
                output.insert(relative, Entry::Link(fs::read_link(&path).unwrap()));
            } else if metadata.is_dir() {
                output.insert(relative, Entry::Directory);
                visit(root, &path, output);
            } else {
                assert!(metadata.is_file());
                output.insert(relative, Entry::File(fs::read(path).unwrap()));
            }
        }
    }
    let mut output = BTreeMap::new();
    visit(root, root, &mut output);
    output
}

fn rejected_without_changes(root: &Path, quota: u64) {
    let before = image(root);
    let cache = CacheStore::new(options(root, quota)).unwrap();
    assert!(cache.wait_ready(Duration::from_secs(5)).is_err());
    assert!(cache.shutdown(Duration::from_secs(5)));
    drop(cache);
    assert_eq!(image(root), before);
}

#[test]
fn owned_empty_bootstrap_recovers_with_both_one_or_no_registry_slots() {
    let original = OriginalBootstrap::new();
    for (index, present) in [[true, true], [true, false], [false, true], [false, false]]
        .into_iter()
        .enumerate()
    {
        let root = original.interrupted(&format!("recovery-{index}"), present);
        let cache = ready(&root);
        assert!(cache.stats().initialized);
        assert!(!cache.stats().cannot_clear);
        assert_eq!(cache.stats().encoded_bytes, 0);
        assert_eq!(fs::read(root.join(ROOT_LOCK)).unwrap(), original.lock);
        for name in ROOT_SLOTS {
            let bytes = fs::read(root.join(name)).unwrap();
            assert!(bytes.len() > HEADER_BYTES);
            assert_eq!(&bytes[..HEADER_BYTES], &original.empty_slot[..HEADER_BYTES]);
            serde_json::from_slice::<Value>(&bytes[HEADER_BYTES..]).unwrap();
        }
        assert!(cache.shutdown(Duration::from_secs(5)));
    }
}

#[test]
fn bootstrap_with_an_actual_owned_namespace_is_preserved() {
    let original = OriginalBootstrap::new();
    let root = original.interrupted("namespace-present", [true, true]);
    let namespace = fs::read_dir(&original.original_root)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| fs::symlink_metadata(path).unwrap().is_dir())
        .unwrap();
    let copied = root.join(namespace.file_name().unwrap());
    fs::create_dir(&copied).unwrap();
    for item in fs::read_dir(namespace).unwrap() {
        let item = item.unwrap();
        assert!(fs::symlink_metadata(item.path()).unwrap().is_file());
        fs::write(
            copied.join(item.file_name()),
            fs::read(item.path()).unwrap(),
        )
        .unwrap();
    }
    fs::write(
        copied.join("preserved-audio.bin"),
        b"original namespace content",
    )
    .unwrap();
    rejected_without_changes(&root, QUOTA);
}

#[test]
fn foreign_root_entries_block_bootstrap_repair_even_when_both_slots_are_missing() {
    let original = OriginalBootstrap::new();
    let root = original.interrupted("foreign-root-file", [false, false]);
    fs::write(root.join("original-audio.wav"), b"formal audio content").unwrap();
    rejected_without_changes(&root, QUOTA);
}

#[test]
fn mismatched_incomplete_and_foreign_headers_are_preserved() {
    let original = OriginalBootstrap::new();
    for index in 0..3 {
        let root = original.interrupted(&format!("invalid-header-{index}"), [true, true]);
        let mut bytes = original.empty_slot[..HEADER_BYTES].to_vec();
        match index {
            0 => bytes[16] = if bytes[16] == b'a' { b'b' } else { b'a' },
            1 => bytes.truncate(HEADER_BYTES - 1),
            2 => bytes[80..88].copy_from_slice(b"FOREIGN!"),
            _ => unreachable!(),
        }
        fs::write(root.join(ROOT_SLOTS[0]), bytes).unwrap();
        rejected_without_changes(&root, QUOTA);
    }
}

#[test]
fn partial_or_corrupt_populated_payloads_never_become_an_empty_registry() {
    let original = OriginalBootstrap::new();
    for (index, payload) in [b"{".as_slice(), b"formal foreign ledger bytes".as_slice()]
        .into_iter()
        .enumerate()
    {
        let root = original.interrupted(&format!("partial-payload-{index}"), [true, true]);
        let mut bytes = original.empty_slot[..HEADER_BYTES].to_vec();
        bytes.extend_from_slice(payload);
        fs::write(root.join(ROOT_SLOTS[0]), bytes).unwrap();
        rejected_without_changes(&root, QUOTA);
    }
    let root = original.interrupted("corrupt-populated-ledger", [true, true]);
    let mut document: Value =
        serde_json::from_slice(&original.populated_slot[HEADER_BYTES..]).unwrap();
    assert!(
        !document["data"]["namespaces"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    document["checksum"] = Value::String("invalid-checksum".into());
    let mut bytes = original.populated_slot[..HEADER_BYTES].to_vec();
    bytes.extend_from_slice(&serde_json::to_vec(&document).unwrap());
    fs::write(root.join(ROOT_SLOTS[0]), bytes).unwrap();
    rejected_without_changes(&root, QUOTA);
}

#[test]
fn missing_slot_creation_and_bootstrap_payload_are_preflighted_against_quota() {
    let original = OriginalBootstrap::new();
    let root = original.interrupted("bootstrap-quota", [true, false]);
    rejected_without_changes(&root, 256);
}

#[cfg(windows)]
struct Junction(PathBuf);

#[cfg(windows)]
impl Junction {
    fn new(path: &Path, target: &Path) -> Self {
        let output = std::process::Command::new("cmd.exe")
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
        fs::remove_dir(&self.0).unwrap();
    }
}

#[cfg(windows)]
#[test]
fn registry_slot_reparse_point_and_its_target_are_preserved() {
    let original = OriginalBootstrap::new();
    let root = original.interrupted("bootstrap-junction", [false, true]);
    let target = original.directory.path().join("preserved-target");
    fs::create_dir(&target).unwrap();
    let sentinel = target.join("original.wav");
    fs::write(&sentinel, b"formal target content").unwrap();
    let junction = Junction::new(&root.join(ROOT_SLOTS[0]), &target);
    rejected_without_changes(&root, QUOTA);
    assert_eq!(fs::read(sentinel).unwrap(), b"formal target content");
    drop(junction);
}

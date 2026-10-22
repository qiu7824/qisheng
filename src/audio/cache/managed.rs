//! Durable ownership ledger for transient blocks; this does not reuse media across sessions.
use super::reusable;
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

pub(super) const HEADER_BYTES: usize = 120;
const MAGIC: &[u8; 16] = b"LXENCODEDCACHE01";
const ROOT_LOCK: &str = ".lx-cache-owner.lock";
const ROOT_SLOTS: [&str; 2] = [".lx-cache-registry.0", ".lx-cache-registry.1"];
const OWNER: &str = "owner.lock";
const NS_SLOTS: [&str; 2] = ["manifest.0", "manifest.1"];
const MAX_NAMESPACES: usize = 128;
const MAX_BLOCKS: usize = 8192;
const MAX_ROOT_DOCUMENT: u64 = 64 * 1024;
const MAX_NAMESPACE_DOCUMENT: u64 = 2 * 1024 * 1024;
pub(super) const MAX_PUBLICATION_LEDGER_RESERVATION: u64 =
    2 * (MAX_NAMESPACE_DOCUMENT + HEADER_BYTES as u64);
const ZERO: &str = "00000000000000000000000000000000";

#[derive(Clone, Copy, Debug, Default)]
pub(super) struct Account {
    pub disk_bytes: u64,
    pub foreign_files: usize,
    pub foreign_bytes: u64,
    pub live_namespaces: usize,
    pub recovered_bytes: u64,
    pub unmeasurable: bool,
    pub blocks: usize,
}

#[derive(Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct Registry {
    namespaces: BTreeSet<String>,
    #[serde(default, skip_serializing_if = "zero_epoch")]
    reuse_epoch: u64,
}
fn zero_epoch(value: &u64) -> bool {
    *value == 0
}

#[derive(Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct Manifest {
    blocks: BTreeMap<String, Registration>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Registration {
    token: String,
    reserved: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    reuse: Option<reusable::Registration>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    body_sha256: Option<String>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Envelope<T> {
    sequence: u64,
    data: T,
    checksum: String,
}

#[derive(Clone)]
struct Snapshot<T> {
    sequence: u64,
    active: usize,
    data: T,
}
struct RootGuard {
    file: File,
    nonce: String,
    registry: Snapshot<Registry>,
}

pub(super) struct RegistryAccess {
    pub closing: AtomicBool,
    pub waiting: AtomicBool,
    job_cancel: Mutex<Option<Arc<AtomicBool>>>,
    quota: u64,
}
impl RegistryAccess {
    pub fn new(quota: u64) -> Self {
        Self {
            closing: AtomicBool::new(false),
            waiting: AtomicBool::new(false),
            job_cancel: Mutex::new(None),
            quota,
        }
    }
    pub fn set_job_cancel(&self, cancel: Option<Arc<AtomicBool>>) {
        *self
            .job_cancel
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = cancel;
    }
    pub fn job_cancelled(&self) -> bool {
        self.job_cancel
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .as_ref()
            .is_some_and(|cancel| cancel.load(Ordering::Acquire))
    }
    fn check(&self) -> Result<()> {
        if self.closing.load(Ordering::Acquire) {
            bail!("cache I/O shutdown requested");
        }
        if self.job_cancelled() {
            bail!("cache publication cancelled");
        }
        Ok(())
    }
}

#[derive(Clone)]
pub(super) struct BlockProof {
    directory: PathBuf,
    expected: [u8; HEADER_BYTES],
}
pub(super) struct VerifiedBlock {
    pub path: PathBuf,
    pub proof: BlockProof,
    pub len: usize,
    pub offset: u64,
    pub sha256: String,
    pub reader: File,
}
pub(super) struct VerifiedRepresentation {
    pub metadata: reusable::Metadata,
    pub blocks: Vec<VerifiedBlock>,
}

pub(super) struct Directory {
    root: PathBuf,
    name: String,
    nonce: String,
    _owner: Option<File>,
    manifest: Mutex<Snapshot<Manifest>>,
    account: Mutex<Account>,
    quota: u64,
    capacity: usize,
    access: Arc<RegistryAccess>,
}

pub(super) enum Publication {
    Quota,
    Written { path: PathBuf, proof: BlockProof },
}

fn random_token() -> Result<String> {
    let mut bytes = [0; 16];
    getrandom::fill(&mut bytes)
        .map_err(|_| anyhow::anyhow!("cache ownership entropy unavailable"))?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn hex(value: &str, length: usize) -> bool {
    value.len() == length
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn namespace_nonce(name: &str) -> Option<&str> {
    let value = name.strip_prefix("managed-")?;
    hex(value, 32).then_some(value)
}

fn valid_block(name: &str) -> bool {
    name.strip_prefix("block-")
        .and_then(|name| name.strip_suffix(".bin"))
        .is_some_and(|value| hex(value, 16))
}

fn link(metadata: &fs::Metadata) -> bool {
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

fn actual_directory(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path).context("inspect cache directory")?;
    ensure!(
        metadata.is_dir() && !link(&metadata),
        "cannotClear: cache directory is not an actual directory"
    );
    Ok(())
}

fn actual_ancestry(path: &Path) -> Result<()> {
    for directory in path.ancestors() {
        actual_directory(directory)?;
    }
    Ok(())
}

fn exists(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error).context("inspect managed cache path without following links"),
    }
}

fn actual_file(path: &Path) -> Result<fs::Metadata> {
    let metadata = fs::symlink_metadata(path).context("inspect cache file")?;
    ensure!(
        metadata.is_file() && !link(&metadata),
        "cannotClear: cache file is a link or non-regular file"
    );
    Ok(metadata)
}

fn open_file(path: &Path, write: bool, create: bool) -> Result<File> {
    if !create {
        actual_file(path)?;
    }
    let mut options = OpenOptions::new();
    options.read(true).write(write).create_new(create);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        // Open the reparse point itself and reject it rather than following its target.
        options
            .custom_flags(0x0020_0000)
            .share_mode(0x1 | 0x2 | 0x4);
    }
    let file = options.open(path).context("open managed cache file")?;
    ensure!(
        file.metadata()?.is_file() && !link(&file.metadata()?),
        "cannotClear: opened cache file is not regular"
    );
    Ok(file)
}

fn header(root: &str, namespace: &str, role: &[u8; 8], token: &str) -> [u8; HEADER_BYTES] {
    let mut output = [0; HEADER_BYTES];
    output[..16].copy_from_slice(MAGIC);
    output[16..48].copy_from_slice(root.as_bytes());
    output[48..80].copy_from_slice(namespace.as_bytes());
    output[80..88].copy_from_slice(role);
    output[88..].copy_from_slice(token.as_bytes());
    output
}

fn verify_header(file: &mut File, expected: &[u8; HEADER_BYTES]) -> Result<()> {
    file.seek(SeekFrom::Start(0))?;
    let mut bytes = [0; HEADER_BYTES];
    file.read_exact(&mut bytes)
        .context("cannotClear: cache ownership header is incomplete")?;
    ensure!(
        bytes == *expected,
        "cannotClear: cache ownership header does not match its registration"
    );
    Ok(())
}

fn create_header(path: &Path, expected: &[u8; HEADER_BYTES]) -> Result<File> {
    let mut file = open_file(path, true, true)?;
    file.write_all(expected)?;
    file.sync_all()?;
    Ok(file)
}

fn envelope<T: Serialize>(sequence: u64, data: &T) -> Result<Vec<u8>> {
    let checksum = format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(&(sequence, data))?)
    );
    Ok(serde_json::to_vec(&Envelope {
        sequence,
        data,
        checksum,
    })?)
}

fn read_snapshot<T: DeserializeOwned + Serialize>(
    directory: &Path,
    names: [&str; 2],
    expected: &[u8; HEADER_BYTES],
    maximum: u64,
) -> Result<Snapshot<T>> {
    let mut best: Option<Snapshot<T>> = None;
    for (active, name) in names.into_iter().enumerate() {
        let path = directory.join(name);
        if !exists(&path)? {
            continue;
        }
        let metadata = actual_file(&path)?;
        ensure!(
            metadata.len() <= maximum + HEADER_BYTES as u64,
            "cannotClear: cache ledger exceeds its limit"
        );
        let mut file = open_file(&path, false, false)?;
        verify_header(&mut file, expected)?;
        let mut bytes = Vec::with_capacity((metadata.len() - HEADER_BYTES as u64) as usize);
        file.take(maximum + 1).read_to_end(&mut bytes)?;
        let Ok(value) = serde_json::from_slice::<Envelope<T>>(&bytes) else {
            continue;
        };
        let checksum = format!(
            "{:x}",
            Sha256::digest(serde_json::to_vec(&(value.sequence, &value.data))?)
        );
        if value.checksum != checksum {
            continue;
        }
        if best
            .as_ref()
            .is_none_or(|old| old.sequence < value.sequence)
        {
            best = Some(Snapshot {
                sequence: value.sequence,
                active,
                data: value.data,
            });
        }
    }
    best.context("cannotClear: cache ledger has no complete trusted snapshot")
}

fn next_snapshot<T: Serialize>(snapshot: &Snapshot<T>) -> Result<(usize, Vec<u8>)> {
    let sequence = snapshot
        .sequence
        .checked_add(1)
        .context("cache ledger sequence exhausted")?;
    Ok((1 - snapshot.active, envelope(sequence, &snapshot.data)?))
}

fn snapshot_growth<T: Serialize>(
    directory: &Path,
    names: [&str; 2],
    snapshot: &Snapshot<T>,
) -> Result<u64> {
    let (slot, bytes) = next_snapshot(snapshot)?;
    let old = fs::symlink_metadata(directory.join(names[slot])).map_or(0, |value| value.len());
    Ok((HEADER_BYTES as u64 + bytes.len() as u64).saturating_sub(old))
}

fn physical_usage(root: &Path, nonce: &str) -> Result<u64> {
    let registered = read_snapshot::<Registry>(
        root,
        ROOT_SLOTS,
        &header(nonce, ZERO, b"REGSLOT!", ZERO),
        MAX_ROOT_DOCUMENT,
    )
    .map(|snapshot| snapshot.data.namespaces)
    .unwrap_or_default();
    let mut bytes = 0u64;
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let metadata = fs::symlink_metadata(entry.path())?;
        ensure!(
            !link(&metadata),
            "cannotClear: unmeasurable cache link was preserved"
        );
        if metadata.is_file() {
            bytes = bytes.saturating_add(metadata.len());
        } else {
            ensure!(
                metadata.is_dir()
                    && registered.contains(&entry.file_name().to_string_lossy().into_owned()),
                "cannotClear: unregistered cache directory was preserved"
            );
            for child in fs::read_dir(entry.path())? {
                let child = child?;
                let metadata = fs::symlink_metadata(child.path())?;
                ensure!(
                    metadata.is_file() && !link(&metadata),
                    "cannotClear: unmeasurable namespace entry was preserved"
                );
                bytes = bytes.saturating_add(metadata.len());
            }
        }
    }
    Ok(bytes)
}
fn ensure_growth(
    directory: &Path,
    names: [&str; 2],
    expected: &[u8; HEADER_BYTES],
    growth: u64,
    access: &RegistryAccess,
) -> Result<()> {
    if growth == 0 {
        return Ok(());
    }
    access.check()?;
    let root = if names == ROOT_SLOTS {
        directory
    } else {
        directory
            .parent()
            .context("invalid cache namespace ancestry")?
    };
    let nonce = std::str::from_utf8(&expected[16..48]).context("invalid cache ownership nonce")?;
    let before = physical_usage(root, nonce)?;
    ensure!(
        before
            .checked_add(growth)
            .is_some_and(|needed| needed <= access.quota),
        "cache ledger maintenance quota exhausted"
    );
    Ok(())
}

fn write_snapshot<T: Serialize>(
    directory: &Path,
    names: [&str; 2],
    expected: &[u8; HEADER_BYTES],
    maximum: u64,
    snapshot: &mut Snapshot<T>,
    access: &RegistryAccess,
) -> Result<()> {
    access.check()?;
    let (slot, bytes) = next_snapshot(snapshot)?;
    ensure!(
        bytes.len() as u64 <= maximum,
        "cache ledger document quota exhausted"
    );
    let path = directory.join(names[slot]);
    let growth = (HEADER_BYTES as u64 + bytes.len() as u64)
        .saturating_sub(fs::symlink_metadata(&path).map_or(0, |value| value.len()));
    ensure_growth(directory, names, expected, growth, access)?;
    let mut file = if exists(&path)? {
        open_file(&path, true, false)?
    } else {
        create_header(&path, expected)?
    };
    // Ownership bytes never change or truncate, so a crash leaves the other slot usable.
    verify_header(&mut file, expected)?;
    file.seek(SeekFrom::Start(HEADER_BYTES as u64))?;
    file.write_all(&bytes)?;
    file.set_len(HEADER_BYTES as u64 + bytes.len() as u64)?;
    file.sync_all()?;
    snapshot.sequence += 1;
    snapshot.active = slot;
    Ok(())
}

fn initialize<T: Serialize>(
    directory: &Path,
    names: [&str; 2],
    expected: &[u8; HEADER_BYTES],
    maximum: u64,
    data: T,
    access: &RegistryAccess,
) -> Result<Snapshot<T>> {
    let allocation = 2 * HEADER_BYTES as u64 + envelope(1, &data)?.len() as u64;
    ensure_growth(directory, names, expected, allocation, access)?;
    for name in names {
        create_header(&directory.join(name), expected)?;
    }
    let mut snapshot = Snapshot {
        sequence: 0,
        active: 1,
        data,
    };
    write_snapshot(directory, names, expected, maximum, &mut snapshot, access)?;
    Ok(snapshot)
}

fn recover_empty_root_bootstrap(
    root: &Path,
    locked: &mut File,
    nonce: &str,
    access: &RegistryAccess,
) -> Result<Snapshot<Registry>> {
    access.check()?;
    let lock_header = header(nonce, ZERO, b"REGLOCK!", ZERO);
    let slot_header = header(nonce, ZERO, b"REGSLOT!", ZERO);
    let mut present = [false; 2];
    let mut lock_present = false;
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let name = entry.file_name();
        let expected = if name == ROOT_LOCK {
            lock_present = true;
            &lock_header
        } else if let Some(slot) = ROOT_SLOTS.iter().position(|slot| name == *slot) {
            present[slot] = true;
            &slot_header
        } else {
            bail!("cannotClear: incomplete root bootstrap contains additional storage");
        };
        let metadata = actual_file(&entry.path())?;
        ensure!(
            metadata.len() == HEADER_BYTES as u64,
            "cannotClear: incomplete root bootstrap contains a ledger payload"
        );
        if name == ROOT_LOCK {
            // Windows byte-range locks also reject reads through another file
            // handle in this process. Verify through the still-locked owner.
            ensure!(
                locked.metadata()?.len() == HEADER_BYTES as u64,
                "cannotClear: root bootstrap lock changed during verification"
            );
            verify_header(locked, expected)?;
        } else {
            let mut file = open_file(&entry.path(), false, false)?;
            ensure!(
                file.metadata()?.len() == HEADER_BYTES as u64,
                "cannotClear: root bootstrap changed during verification"
            );
            verify_header(&mut file, expected)?;
        }
    }
    ensure!(
        lock_present,
        "cannotClear: root bootstrap lock path is missing"
    );
    // The caller still holds the existing registry's OS lock. No namespace,
    // payload or foreign entry can be discarded by this narrowly empty repair.
    let data = Registry::default();
    let payload = envelope(1, &data)?;
    let missing = present.iter().filter(|&&exists| !exists).count() as u64;
    ensure_growth(
        root,
        ROOT_SLOTS,
        &slot_header,
        missing * HEADER_BYTES as u64 + payload.len() as u64,
        access,
    )?;
    for (slot, exists) in present.into_iter().enumerate() {
        if !exists {
            access.check()?;
            create_header(&root.join(ROOT_SLOTS[slot]), &slot_header)?;
        }
    }
    let mut snapshot = Snapshot {
        sequence: 0,
        active: 1,
        data,
    };
    write_snapshot(
        root,
        ROOT_SLOTS,
        &slot_header,
        MAX_ROOT_DOCUMENT,
        &mut snapshot,
        access,
    )?;
    Ok(snapshot)
}

fn root_guard(root: &Path, access: &RegistryAccess) -> Result<RootGuard> {
    access.waiting.store(false, Ordering::Release);
    access.check()?;
    actual_ancestry(root)?;
    let path = root.join(ROOT_LOCK);
    let (mut file, created_lock) = if exists(&path)? {
        (open_file(&path, true, false)?, false)
    } else {
        let expected = header(&random_token()?, ZERO, b"REGLOCK!", ZERO);
        ensure_growth(root, ROOT_SLOTS, &expected, HEADER_BYTES as u64, access)?;
        match create_header(&path, &expected) {
            Ok(file) => (file, true),
            Err(_) if exists(&path)? => (open_file(&path, true, false)?, false),
            Err(error) => return Err(error),
        }
    };
    loop {
        if let Err(error) = access.check() {
            access.waiting.store(false, Ordering::Release);
            return Err(error);
        }
        match file.try_lock() {
            Ok(()) => {
                access.waiting.store(false, Ordering::Release);
                break;
            }
            Err(TryLockError::WouldBlock) => {
                access.waiting.store(true, Ordering::Release);
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(TryLockError::Error(error)) => {
                access.waiting.store(false, Ordering::Release);
                return Err(error).context("lock encoded cache registry");
            }
        }
    }
    let mut bytes = [0; HEADER_BYTES];
    file.seek(SeekFrom::Start(0))?;
    file.read_exact(&mut bytes)?;
    let nonce = std::str::from_utf8(&bytes[16..48])
        .unwrap_or_default()
        .to_owned();
    ensure!(
        hex(&nonce, 32) && bytes == header(&nonce, ZERO, b"REGLOCK!", ZERO),
        "cannotClear: foreign cache registry lock"
    );
    let expected = header(&nonce, ZERO, b"REGSLOT!", ZERO);
    let mut has_slot = false;
    for name in ROOT_SLOTS {
        has_slot |= exists(&root.join(name))?;
    }
    let registry = if has_slot {
        read_snapshot(root, ROOT_SLOTS, &expected, MAX_ROOT_DOCUMENT).or_else(|error| {
            recover_empty_root_bootstrap(root, &mut file, &nonce, access)
                .with_context(|| format!("cache root snapshot was incomplete: {error:#}"))
        })?
    } else if created_lock {
        initialize(
            root,
            ROOT_SLOTS,
            &expected,
            MAX_ROOT_DOCUMENT,
            Registry::default(),
            access,
        )?
    } else {
        recover_empty_root_bootstrap(root, &mut file, &nonce, access)?
    };
    ensure!(
        registry.data.namespaces.len() <= MAX_NAMESPACES
            && registry
                .data
                .namespaces
                .iter()
                .all(|name| namespace_nonce(name).is_some()),
        "cannotClear: invalid cache namespace registry"
    );
    Ok(RootGuard {
        file,
        nonce,
        registry,
    })
}

impl Drop for RootGuard {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

fn namespace_manifest(root: &str, directory: &Path, namespace: &str) -> Result<Snapshot<Manifest>> {
    let snapshot: Snapshot<Manifest> = read_snapshot(
        directory,
        NS_SLOTS,
        &header(root, namespace, b"NSSLOT!!", ZERO),
        MAX_NAMESPACE_DOCUMENT,
    )?;
    ensure!(
        snapshot.data.blocks.len() <= MAX_BLOCKS
            && snapshot
                .data
                .blocks
                .iter()
                .all(|(name, item)| valid_block(name)
                    && hex(&item.token, 32)
                    && item.reserved > 0
                    && item.reserved <= 4 * 1024 * 1024),
        "cannotClear: invalid cache block registry"
    );
    Ok(snapshot)
}

fn inspect_namespace(
    directory: &Path,
    snapshot: &Snapshot<Manifest>,
    account: &mut Account,
) -> Result<()> {
    actual_directory(directory)?;
    let mut present = BTreeSet::new();
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let metadata = fs::symlink_metadata(entry.path())?;
        if link(&metadata) || !metadata.is_file() {
            account.foreign_files += 1;
            account.unmeasurable = true;
            continue;
        }
        let length = metadata.len();
        if let Some(item) = snapshot.data.blocks.get(&name) {
            account.disk_bytes = account
                .disk_bytes
                .saturating_add(length.max(item.reserved + HEADER_BYTES as u64));
            account.blocks += 1;
            present.insert(name);
        } else {
            account.disk_bytes = account.disk_bytes.saturating_add(length);
            if name != OWNER && !NS_SLOTS.contains(&name.as_str()) {
                account.foreign_files += 1;
                account.foreign_bytes = account.foreign_bytes.saturating_add(length);
            }
        }
    }
    for (name, item) in &snapshot.data.blocks {
        if !present.contains(name) {
            account.disk_bytes = account
                .disk_bytes
                .saturating_add(item.reserved + HEADER_BYTES as u64);
            account.blocks += 1;
        }
    }
    Ok(())
}

fn account(root: &Path, guard: &RootGuard) -> Result<Account> {
    let mut output = Account::default();
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if guard.registry.data.namespaces.contains(&name) {
            continue;
        }
        let metadata = fs::symlink_metadata(entry.path())?;
        if link(&metadata) || !metadata.is_file() {
            output.foreign_files += 1;
            output.unmeasurable = true;
        } else {
            output.disk_bytes = output.disk_bytes.saturating_add(metadata.len());
            if name != ROOT_LOCK && !ROOT_SLOTS.contains(&name.as_str()) {
                output.foreign_files += 1;
                output.foreign_bytes = output.foreign_bytes.saturating_add(metadata.len());
            }
        }
    }
    for name in &guard.registry.data.namespaces {
        let directory = root.join(name);
        if !exists(&directory)? {
            continue;
        }
        actual_directory(&directory)?;
        let namespace = namespace_nonce(name).context("invalid cache namespace")?;
        let manifest = namespace_manifest(&guard.nonce, &directory, namespace)?;
        inspect_namespace(&directory, &manifest, &mut output)?;
        output.live_namespaces += 1;
    }
    Ok(output)
}

fn remove_registered(
    directory: &Path,
    root: &str,
    namespace: &str,
    name: &str,
    item: &Registration,
) -> Result<u64> {
    let path = directory.join(name);
    if !exists(&path)? {
        return Ok(0);
    }
    let metadata = actual_file(&path)?;
    if let Some(digest) = &item.body_sha256 {
        let proof = BlockProof {
            directory: directory.to_owned(),
            expected: header(root, namespace, b"NSBLOCK!", &item.token),
        };
        drop(verified_reader(
            &path,
            &proof,
            item.reserved as usize,
            digest,
            None,
        )?);
    }
    let mut file = open_file(&path, false, false)?;
    verify_header(
        &mut file,
        &header(root, namespace, b"NSBLOCK!", &item.token),
    )?;
    drop(file);
    fs::remove_file(&path).context("remove registered encoded cache block")?;
    Ok(metadata.len())
}

fn recover(
    root: &Path,
    guard: &mut RootGuard,
    skip: Option<&str>,
    access: &RegistryAccess,
) -> Result<Account> {
    let mut recovered = 0;
    let mut failed_cleanup = 0;
    let mut failed_foreign_bytes = 0_u64;
    let names: Vec<_> = guard.registry.data.namespaces.iter().cloned().collect();
    for name in names {
        access.check()?;
        if skip == Some(name.as_str()) {
            continue;
        }
        let directory = root.join(&name);
        if !exists(&directory)? {
            guard.registry.data.namespaces.remove(&name);
            continue;
        }
        actual_directory(&directory)?;
        let namespace = namespace_nonce(&name).context("invalid cache namespace")?;
        let owner_path = directory.join(OWNER);
        if !exists(&owner_path)? {
            if fs::read_dir(&directory)?.next().is_none() {
                fs::remove_dir(&directory)?;
                guard.registry.data.namespaces.remove(&name);
                continue;
            }
            bail!("cannotClear: registered namespace has no verifiable owner");
        }
        let mut owner = open_file(&owner_path, true, false)?;
        match owner.try_lock() {
            Ok(()) => (),
            Err(TryLockError::WouldBlock) => continue,
            Err(TryLockError::Error(_)) => {
                bail!("cannotClear: namespace owner lock could not be checked")
            }
        }
        verify_header(
            &mut owner,
            &header(&guard.nonce, namespace, b"NSOWNER!", ZERO),
        )?;
        let mut manifest = match namespace_manifest(&guard.nonce, &directory, namespace) {
            Ok(manifest) => manifest,
            Err(error) => {
                // Reclamation may have stopped between removing metadata slots and the owner.
                let entries: Vec<_> = fs::read_dir(&directory)?.collect::<std::io::Result<_>>()?;
                if entries.iter().all(|entry| {
                    entry.file_name() == OWNER
                        || NS_SLOTS.iter().any(|slot| entry.file_name() == *slot)
                }) {
                    for slot in NS_SLOTS {
                        let path = directory.join(slot);
                        if exists(&path)? {
                            let mut file = open_file(&path, false, false)?;
                            verify_header(
                                &mut file,
                                &header(&guard.nonce, namespace, b"NSSLOT!!", ZERO),
                            )?;
                            drop(file);
                            fs::remove_file(path)?;
                        }
                    }
                    fs::remove_file(&owner_path)?;
                    drop(owner);
                    fs::remove_dir(&directory)?;
                    guard.registry.data.namespaces.remove(&name);
                    continue;
                }
                return Err(error);
            }
        };
        let blocks: Vec<_> = manifest
            .data
            .blocks
            .iter()
            .map(|(name, item)| (name.clone(), item.clone()))
            .collect();
        for (block, item) in blocks {
            access.check()?;
            if let Some(reuse) = &item.reuse {
                reuse
                    .validate(item.reserved as usize)
                    .context("cannotClear: invalid persistent block index")?;
                if reuse.representation.ledger_epoch == guard.registry.data.reuse_epoch {
                    continue;
                }
            }
            if let Ok(length) =
                remove_registered(&directory, &guard.nonce, namespace, &block, &item)
            {
                recovered += length;
                manifest.data.blocks.remove(&block);
                write_snapshot(
                    &directory,
                    NS_SLOTS,
                    &header(&guard.nonce, namespace, b"NSSLOT!!", ZERO),
                    MAX_NAMESPACE_DOCUMENT,
                    &mut manifest,
                    access,
                )?;
            } else {
                let path = directory.join(&block);
                // A registered name alone does not confer ownership after its header is replaced.
                if let Ok(metadata) = fs::symlink_metadata(&path)
                    && metadata.is_file()
                    && !link(&metadata)
                {
                    failed_cleanup += 1;
                    failed_foreign_bytes = failed_foreign_bytes.saturating_add(metadata.len());
                }
            }
        }
        let mut remaining = Account::default();
        inspect_namespace(&directory, &manifest, &mut remaining)?;
        if remaining.foreign_files == 0 && manifest.data.blocks.is_empty() {
            for slot in NS_SLOTS {
                let path = directory.join(slot);
                let mut file = open_file(&path, false, false)?;
                verify_header(
                    &mut file,
                    &header(&guard.nonce, namespace, b"NSSLOT!!", ZERO),
                )?;
                drop(file);
                fs::remove_file(path)?;
            }
            // Registry lock prevents another cooperating process replacing this lock path.
            fs::remove_file(owner_path)?;
            drop(owner);
            fs::remove_dir(&directory)?;
            guard.registry.data.namespaces.remove(&name);
        }
    }
    write_snapshot(
        root,
        ROOT_SLOTS,
        &header(&guard.nonce, ZERO, b"REGSLOT!", ZERO),
        MAX_ROOT_DOCUMENT,
        &mut guard.registry,
        access,
    )?;
    let mut output = account(root, guard)?;
    output.recovered_bytes = recovered;
    output.foreign_files += failed_cleanup;
    output.foreign_bytes = output.foreign_bytes.saturating_add(failed_foreign_bytes);
    Ok(output)
}

impl Directory {
    pub fn new(
        root: &Path,
        quota: u64,
        block_size: usize,
        max_blocks: usize,
        access: Arc<RegistryAccess>,
    ) -> Result<Self> {
        let root = std::path::absolute(root)?;
        ensure!(
            !root
                .components()
                .any(|component| matches!(component, Component::ParentDir)),
            "cache root contains unresolved parent components"
        );
        for parent in root.ancestors() {
            match fs::symlink_metadata(parent) {
                Ok(metadata) => ensure!(
                    metadata.is_dir() && !link(&metadata),
                    "cannotClear: cache root ancestry contains a link"
                ),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
                Err(error) => return Err(error).context("inspect encoded cache ancestry"),
            }
        }
        fs::create_dir_all(&root)?;
        let mut guard = root_guard(&root, &access)?;
        let before = recover(&root, &mut guard, None, &access)?;
        ensure!(
            !before.unmeasurable,
            "cannotClear: unregistered cache directories or links were preserved"
        );
        ensure!(
            guard.registry.data.namespaces.len() < MAX_NAMESPACES,
            "cannotClear: encoded cache namespace quota exhausted"
        );
        let namespace = random_token()?;
        let name = format!("managed-{namespace}");
        let directory = root.join(&name);
        guard.registry.data.namespaces.insert(name.clone());
        let empty = Manifest::default();
        let initial_metadata = 3 * HEADER_BYTES as u64 + envelope(1, &empty)?.len() as u64;
        let growth = snapshot_growth(&root, ROOT_SLOTS, &guard.registry)?;
        ensure!(
            before
                .disk_bytes
                .saturating_add(initial_metadata)
                .saturating_add(growth)
                < quota,
            "encoded cache disk quota cannot admit a namespace"
        );
        // Registration precedes mkdir, so an interrupted constructor cannot create an unlisted namespace.
        write_snapshot(
            &root,
            ROOT_SLOTS,
            &header(&guard.nonce, ZERO, b"REGSLOT!", ZERO),
            MAX_ROOT_DOCUMENT,
            &mut guard.registry,
            &access,
        )?;
        fs::create_dir(&directory)?;
        let owner = create_header(
            &directory.join(OWNER),
            &header(&guard.nonce, &namespace, b"NSOWNER!", ZERO),
        )?;
        owner.try_lock().context("lock new encoded cache owner")?;
        let manifest = initialize(
            &directory,
            NS_SLOTS,
            &header(&guard.nonce, &namespace, b"NSSLOT!!", ZERO),
            MAX_NAMESPACE_DOCUMENT,
            empty,
            &access,
        )?;
        let mut current = account(&root, &guard)?;
        current.recovered_bytes = before.recovered_bytes;
        current.foreign_files = current.foreign_files.max(before.foreign_files);
        current.foreign_bytes = current.foreign_bytes.max(before.foreign_bytes);
        // Include both bounded snapshot entries and the ownership header in the sequential lead bound.
        let capacity = ((quota.saturating_sub(current.disk_bytes))
            / (block_size as u64 + HEADER_BYTES as u64 + 512)) as usize;
        ensure!(
            capacity > 0,
            "encoded cache quota cannot admit one verified block"
        );
        Ok(Self {
            root,
            name,
            nonce: guard.nonce.clone(),
            _owner: Some(owner),
            manifest: Mutex::new(manifest),
            account: Mutex::new(current),
            quota,
            capacity: capacity.min(max_blocks),
            access,
        })
    }

    pub fn path(&self) -> PathBuf {
        self.root.join(&self.name)
    }
    pub fn capacity(&self) -> usize {
        self.capacity
    }
    pub fn account(&self) -> Account {
        *self
            .account
            .lock()
            .unwrap_or_else(|error| error.into_inner())
    }

    fn remember(&self, mut next: Account) {
        let mut account = self
            .account
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        next.recovered_bytes = next.recovered_bytes.saturating_add(account.recovered_bytes);
        // Only a full ownership audit may clear a previous foreign-file finding.
        next.foreign_files = next.foreign_files.max(account.foreign_files);
        next.foreign_bytes = next.foreign_bytes.max(account.foreign_bytes);
        next.unmeasurable |= account.unmeasurable;
        *account = next;
    }

    pub fn publish(
        &self,
        serial: u64,
        bytes: &[u8],
        #[cfg(test)] diagnostics: &super::diagnostics::PublicationDiagnostics,
        #[cfg(test)] source_serial: Option<u64>,
        #[cfg(test)] offset: u64,
    ) -> Result<Publication> {
        #[cfg(test)]
        let audit_started = std::time::Instant::now();
        let guard = root_guard(&self.root, &self.access)?;
        let directory = self.path();
        actual_directory(&directory)?;
        let mut manifest = self
            .manifest
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        *manifest = namespace_manifest(
            &self.nonce,
            &directory,
            namespace_nonce(&self.name).unwrap(),
        )?;
        let name = format!("block-{serial:016x}.bin");
        let path = directory.join(&name);
        if fs::symlink_metadata(&path).is_ok() {
            bail!("cannotClear: an unregistered block path already exists");
        }
        let registration = Registration {
            token: random_token()?,
            reserved: bytes.len() as u64,
            reuse: None,
            body_sha256: None,
        };
        manifest
            .data
            .blocks
            .insert(name.clone(), registration.clone());
        let before = account(&self.root, &guard)?;
        let growth = snapshot_growth(&directory, NS_SLOTS, &manifest)?;
        let needed = before
            .disk_bytes
            .saturating_add(bytes.len() as u64 + HEADER_BYTES as u64)
            .saturating_add(growth);
        if before.unmeasurable || needed > self.quota || before.blocks >= MAX_BLOCKS {
            manifest.data.blocks.remove(&name);
            self.remember(before);
            #[cfg(test)]
            diagnostics.record(
                super::PublicationStage::PublicationAudits,
                Some(serial),
                source_serial,
                offset,
                audit_started.elapsed(),
            );
            return Ok(Publication::Quota);
        }
        // Retain the reservation even if the ledger write fails after a durable partial update.
        let mut reserved = before;
        reserved.disk_bytes = needed;
        reserved.blocks = reserved.blocks.saturating_add(1);
        self.remember(reserved);
        #[cfg(test)]
        let audit_before = audit_started.elapsed();
        {
            #[cfg(test)]
            let _span = diagnostics.span(
                super::PublicationStage::ReservationSnapshot,
                Some(serial),
                source_serial,
                offset,
            );
            write_snapshot(
                &directory,
                NS_SLOTS,
                &header(
                    &self.nonce,
                    namespace_nonce(&self.name).unwrap(),
                    b"NSSLOT!!",
                    ZERO,
                ),
                MAX_NAMESPACE_DOCUMENT,
                &mut manifest,
                &self.access,
            )?;
        }
        let result = (|| -> Result<()> {
            let mut file = {
                #[cfg(test)]
                let _span = diagnostics.span(
                    super::PublicationStage::OwnershipHeader,
                    Some(serial),
                    source_serial,
                    offset,
                );
                create_header(
                    &path,
                    &header(
                        &self.nonce,
                        namespace_nonce(&self.name).unwrap(),
                        b"NSBLOCK!",
                        &registration.token,
                    ),
                )?
            };
            #[cfg(test)]
            if diagnostics
                .unwind_after_header
                .swap(false, Ordering::AcqRel)
            {
                panic!("injected active encoded publication unwind after actual header sync");
            }
            #[cfg(test)]
            let _span = diagnostics.span(
                super::PublicationStage::PayloadWriteSync,
                Some(serial),
                source_serial,
                offset,
            );
            file.write_all(bytes)?;
            file.sync_all()?;
            Ok(())
        })();
        if let Err(error) = result {
            // Never delete a create_new collision or an ambiguous partial ownership header.
            if remove_registered(
                &directory,
                &self.nonce,
                namespace_nonce(&self.name).unwrap(),
                &name,
                &registration,
            )
            .is_ok()
            {
                manifest.data.blocks.remove(&name);
                write_snapshot(
                    &directory,
                    NS_SLOTS,
                    &header(
                        &self.nonce,
                        namespace_nonce(&self.name).unwrap(),
                        b"NSSLOT!!",
                        ZERO,
                    ),
                    MAX_NAMESPACE_DOCUMENT,
                    &mut manifest,
                    &self.access,
                )?;
            }
            self.remember(account(&self.root, &guard)?);
            return Err(error);
        }
        #[cfg(test)]
        let audit_after_started = std::time::Instant::now();
        self.remember(account(&self.root, &guard)?);
        #[cfg(test)]
        diagnostics.record(
            super::PublicationStage::PublicationAudits,
            Some(serial),
            source_serial,
            offset,
            audit_before.saturating_add(audit_after_started.elapsed()),
        );
        Ok(Publication::Written {
            path,
            proof: BlockProof {
                directory,
                expected: header(
                    &self.nonce,
                    namespace_nonce(&self.name).unwrap(),
                    b"NSBLOCK!",
                    &registration.token,
                ),
            },
        })
    }

    pub fn remove(&self, path: &Path) -> Result<()> {
        let guard = root_guard(&self.root, &self.access)?;
        let directory = self.path();
        actual_directory(&directory)?;
        ensure!(
            path.parent() == Some(directory.as_path()),
            "cannotClear: block is outside its managed namespace"
        );
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .context("invalid encoded block name")?;
        let mut manifest = self
            .manifest
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        *manifest = namespace_manifest(
            &self.nonce,
            &directory,
            namespace_nonce(&self.name).unwrap(),
        )?;
        let item = match manifest.data.blocks.get(name).cloned() {
            Some(item) => item,
            None if !exists(path)? => {
                // A preceding deregistration may have reached disk before reporting an error.
                self.remember(account(&self.root, &guard)?);
                return Ok(());
            }
            None => bail!("cannotClear: encoded block has no durable registration"),
        };
        remove_registered(
            &directory,
            &self.nonce,
            namespace_nonce(&self.name).unwrap(),
            name,
            &item,
        )?;
        manifest.data.blocks.remove(name);
        write_snapshot(
            &directory,
            NS_SLOTS,
            &header(
                &self.nonce,
                namespace_nonce(&self.name).unwrap(),
                b"NSSLOT!!",
                ZERO,
            ),
            MAX_NAMESPACE_DOCUMENT,
            &mut manifest,
            &self.access,
        )?;
        self.remember(account(&self.root, &guard)?);
        Ok(())
    }

    pub fn read_with_proof(path: &Path, proof: &BlockProof) -> Result<File> {
        actual_ancestry(&proof.directory)?;
        ensure!(
            path.parent() == Some(proof.directory.as_path()),
            "encoded cache reader escaped its namespace"
        );
        let mut file = open_file(path, false, false)?;
        verify_header(&mut file, &proof.expected)?;
        Ok(file)
    }

    pub fn ledger_epoch(&self) -> Result<u64> {
        Ok(root_guard(&self.root, &self.access)?
            .registry
            .data
            .reuse_epoch)
    }

    pub fn promote(&self, path: &Path, reuse: reusable::Registration) -> Result<Option<File>> {
        let guard = root_guard(&self.root, &self.access)?;
        if reuse.representation.ledger_epoch != guard.registry.data.reuse_epoch {
            return Ok(None);
        }
        let directory = path.parent().context("invalid reusable block directory")?;
        let name = directory
            .file_name()
            .and_then(|value| value.to_str())
            .context("invalid reusable namespace")?;
        ensure!(
            directory.parent() == Some(self.root.as_path())
                && guard.registry.data.namespaces.contains(name),
            "cannotClear: reusable block escaped registered storage"
        );
        let namespace = namespace_nonce(name).context("invalid reusable namespace")?;
        let mut manifest = namespace_manifest(&guard.nonce, directory, namespace)?;
        let block = path
            .file_name()
            .and_then(|value| value.to_str())
            .context("invalid reusable block")?;
        let item = manifest
            .data
            .blocks
            .get_mut(block)
            .context("reusable block has no registration")?;
        reuse.validate(item.reserved as usize)?;
        let proof = BlockProof {
            directory: directory.to_owned(),
            expected: header(&guard.nonce, namespace, b"NSBLOCK!", &item.token),
        };
        let reader = verified_reader(
            path,
            &proof,
            item.reserved as usize,
            &reuse.body_sha256,
            Some(&self.access),
        )?;
        item.body_sha256 = Some(reuse.body_sha256.clone());
        item.reuse = Some(reuse);
        let before = account(&self.root, &guard)?;
        let growth = snapshot_growth(directory, NS_SLOTS, &manifest)?;
        let (_, bytes) = next_snapshot(&manifest)?;
        if before.unmeasurable
            || before.disk_bytes.saturating_add(growth) > self.quota
            || bytes.len() as u64 > MAX_NAMESPACE_DOCUMENT
        {
            self.remember(before);
            return Ok(None);
        }
        let mut reserved = before;
        reserved.disk_bytes = reserved.disk_bytes.saturating_add(growth);
        self.remember(reserved);
        write_snapshot(
            directory,
            NS_SLOTS,
            &header(&guard.nonce, namespace, b"NSSLOT!!", ZERO),
            MAX_NAMESPACE_DOCUMENT,
            &mut manifest,
            &self.access,
        )?;
        self.remember(account(&self.root, &guard)?);
        Ok(Some(reader))
    }

    pub fn lookup_reusable(&self, data_key: &str) -> Result<Option<VerifiedRepresentation>> {
        let guard = root_guard(&self.root, &self.access)?;
        let mut result: Option<VerifiedRepresentation> = None;
        for name in &guard.registry.data.namespaces {
            self.access.check()?;
            let directory = self.root.join(name);
            if !exists(&directory)? {
                continue;
            }
            actual_directory(&directory)?;
            let namespace = namespace_nonce(name).context("invalid reusable namespace")?;
            let manifest = namespace_manifest(&guard.nonce, &directory, namespace)?;
            let blocks: Vec<_> = manifest
                .data
                .blocks
                .iter()
                .map(|(name, item)| (name.clone(), item.clone()))
                .collect();
            for (name, item) in blocks {
                let Some(reuse) = &item.reuse else {
                    continue;
                };
                reuse
                    .validate(item.reserved as usize)
                    .context("cannotClear: invalid reusable index")?;
                if reuse.representation.data_key != data_key
                    || reuse.representation.ledger_epoch != guard.registry.data.reuse_epoch
                {
                    continue;
                }
                let path = directory.join(&name);
                let proof = BlockProof {
                    directory: directory.clone(),
                    expected: header(&guard.nonce, namespace, b"NSBLOCK!", &item.token),
                };
                let reader = match verified_reader(
                    &path,
                    &proof,
                    item.reserved as usize,
                    &reuse.body_sha256,
                    Some(&self.access),
                ) {
                    Ok(reader) => reader,
                    Err(error) if error.is::<BodyCorrupt>() => {
                        // A replaced body is preserved even when its former ownership header survived.
                        let mut before = account(&self.root, &guard)?;
                        before.foreign_files = before.foreign_files.saturating_add(1);
                        before.foreign_bytes = before.foreign_bytes.saturating_add(
                            fs::symlink_metadata(&path).map_or(0, |meta| meta.len()),
                        );
                        self.remember(before);
                        return Ok(None);
                    }
                    Err(error) => {
                        let mut before = self.account();
                        before.unmeasurable = true;
                        before.foreign_files = before.foreign_files.max(1);
                        self.remember(before);
                        return Err(error);
                    }
                };
                if let Some(old) = &result {
                    if old.metadata.strong_etag != reuse.representation.strong_etag
                        || old.metadata.total != reuse.representation.total
                        || old.metadata.block_size != reuse.representation.block_size
                    {
                        return Ok(None);
                    }
                } else {
                    result = Some(VerifiedRepresentation {
                        metadata: reuse.representation.clone(),
                        blocks: Vec::new(),
                    });
                }
                let result = result.as_mut().unwrap();
                if !result.blocks.iter().any(|old| old.offset == reuse.offset) {
                    result.blocks.push(VerifiedBlock {
                        path,
                        proof,
                        len: item.reserved as usize,
                        offset: reuse.offset,
                        sha256: reuse.body_sha256.clone(),
                        reader,
                    });
                }
            }
        }
        self.remember(account(&self.root, &guard)?);
        Ok(result)
    }

    pub fn clear_reusable(&self, data_key: Option<&str>) -> Result<()> {
        self.clear_reusable_filtered(data_key, None).map(|_| ())
    }
    pub fn clear_reusable_if(&self, metadata: &reusable::Metadata) -> Result<bool> {
        self.clear_reusable_filtered(Some(&metadata.data_key), Some(metadata))
    }
    fn clear_reusable_filtered(
        &self,
        data_key: Option<&str>,
        expected: Option<&reusable::Metadata>,
    ) -> Result<bool> {
        let mut guard = root_guard(&self.root, &self.access)?;
        let before = account(&self.root, &guard)?;
        self.remember(before);
        let mut staged = guard.registry.clone();
        let mut plans = Vec::new();
        let mut growth = 0u64;
        if data_key.is_none() {
            staged.data.reuse_epoch = guard
                .registry
                .data
                .reuse_epoch
                .checked_add(1)
                .context("reusable ledger epoch exhausted")?;
            growth = snapshot_growth(&self.root, ROOT_SLOTS, &staged)?;
        }
        for name in &guard.registry.data.namespaces {
            self.access.check()?;
            let directory = self.root.join(name);
            if !exists(&directory)? {
                continue;
            }
            actual_directory(&directory)?;
            let namespace = namespace_nonce(name).context("invalid reusable namespace")?;
            let mut manifest = namespace_manifest(&guard.nonce, &directory, namespace)?;
            let mut changed = false;
            for item in manifest.data.blocks.values_mut() {
                if item.reuse.as_ref().is_some_and(|reuse| {
                    data_key.is_none_or(|key| reuse.representation.data_key == key)
                        && expected.is_none_or(|expected| reuse.representation == *expected)
                }) {
                    item.reuse = None;
                    changed = true;
                }
            }
            if changed {
                growth = growth
                    .checked_add(snapshot_growth(&directory, NS_SLOTS, &manifest)?)
                    .context("cache maintenance reservation overflow")?;
                plans.push((directory, namespace.to_owned(), manifest));
            }
        }
        // Reserve the whole multi-ledger maintenance operation before changing its first slot.
        ensure_growth(
            &self.root,
            ROOT_SLOTS,
            &header(&guard.nonce, ZERO, b"REGSLOT!", ZERO),
            growth,
            &self.access,
        )?;
        let any_changed = !plans.is_empty();
        if data_key.is_none() {
            guard.registry = staged;
            write_snapshot(
                &self.root,
                ROOT_SLOTS,
                &header(&guard.nonce, ZERO, b"REGSLOT!", ZERO),
                MAX_ROOT_DOCUMENT,
                &mut guard.registry,
                &self.access,
            )?;
        }
        for (directory, namespace, mut manifest) in plans {
            write_snapshot(
                &directory,
                NS_SLOTS,
                &header(&guard.nonce, &namespace, b"NSSLOT!!", ZERO),
                MAX_NAMESPACE_DOCUMENT,
                &mut manifest,
                &self.access,
            )?;
        }
        self.remember(account(&self.root, &guard)?);
        Ok(any_changed)
    }

    pub fn remove_owned(&self, path: &Path) -> Result<()> {
        if path.parent() == Some(self.path().as_path()) {
            return self.remove(path);
        }
        let guard = root_guard(&self.root, &self.access)?;
        let directory = path.parent().context("invalid owned block directory")?;
        let name = directory
            .file_name()
            .and_then(|name| name.to_str())
            .context("invalid owned namespace")?;
        ensure!(
            directory.parent() == Some(self.root.as_path())
                && guard.registry.data.namespaces.contains(name),
            "cannotClear: block escaped registered storage"
        );
        actual_directory(directory)?;
        let namespace = namespace_nonce(name).context("invalid owned namespace")?;
        let mut owner = open_file(&directory.join(OWNER), true, false)?;
        verify_header(
            &mut owner,
            &header(&guard.nonce, namespace, b"NSOWNER!", ZERO),
        )?;
        owner
            .try_lock()
            .context("cannotClear: reusable namespace still has an active owner")?;
        let mut manifest = namespace_manifest(&guard.nonce, directory, namespace)?;
        let block = path
            .file_name()
            .and_then(|name| name.to_str())
            .context("invalid owned block")?;
        if let Some(item) = manifest.data.blocks.get(block).cloned() {
            remove_registered(directory, &guard.nonce, namespace, block, &item)?;
            manifest.data.blocks.remove(block);
            write_snapshot(
                directory,
                NS_SLOTS,
                &header(&guard.nonce, namespace, b"NSSLOT!!", ZERO),
                MAX_NAMESPACE_DOCUMENT,
                &mut manifest,
                &self.access,
            )?;
        } else {
            ensure!(
                !exists(path)?,
                "cannotClear: unregistered reusable path was preserved"
            );
        }
        self.remember(account(&self.root, &guard)?);
        Ok(())
    }

    pub fn refresh_and_recover(&self) -> Result<Account> {
        let mut guard = root_guard(&self.root, &self.access)?;
        let mut account = recover(&self.root, &mut guard, Some(&self.name), &self.access)?;
        let mut manifest = self
            .manifest
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        *manifest = namespace_manifest(
            &self.nonce,
            &self.path(),
            namespace_nonce(&self.name).unwrap(),
        )?;
        for (name, item) in &manifest.data.blocks {
            let path = self.path().join(name);
            if !exists(&path)? {
                continue;
            }
            let verified = if let Some(digest) = &item.body_sha256 {
                let proof = BlockProof {
                    directory: self.path(),
                    expected: header(
                        &self.nonce,
                        namespace_nonce(&self.name).unwrap(),
                        b"NSBLOCK!",
                        &item.token,
                    ),
                };
                verified_reader(
                    &path,
                    &proof,
                    item.reserved as usize,
                    digest,
                    Some(&self.access),
                )
                .map(|_| ())
            } else {
                open_file(&path, false, false).and_then(|mut file| {
                    verify_header(
                        &mut file,
                        &header(
                            &self.nonce,
                            namespace_nonce(&self.name).unwrap(),
                            b"NSBLOCK!",
                            &item.token,
                        ),
                    )
                })
            };
            if verified.is_err() {
                account.foreign_files += 1;
                if let Ok(metadata) = fs::symlink_metadata(&path)
                    && metadata.is_file()
                    && !link(&metadata)
                {
                    account.foreign_bytes = account.foreign_bytes.saturating_add(metadata.len());
                }
            }
        }
        let mut old = self
            .account
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        account.recovered_bytes = account.recovered_bytes.saturating_add(old.recovered_bytes);
        *old = account;
        Ok(account)
    }
}

impl Drop for Directory {
    fn drop(&mut self) {
        // Release only this owner; registry serialization protects orphan reclamation.
        if let Some(owner) = self._owner.take() {
            let _ = owner.unlock();
            drop(owner);
        }
        // Registered storage remains charged and is recovered only after this owner releases.
    }
}

#[derive(Debug)]
struct BodyCorrupt;
impl std::fmt::Display for BodyCorrupt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("reusable body digest or length mismatch")
    }
}
impl std::error::Error for BodyCorrupt {}
fn verified_reader(
    path: &Path,
    proof: &BlockProof,
    len: usize,
    sha256: &str,
    access: Option<&RegistryAccess>,
) -> Result<File> {
    if let Some(access) = access {
        access.check()?;
    }
    actual_ancestry(&proof.directory)?;
    actual_file(path)?;
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(0x0020_0000).share_mode(0x1);
    }
    let mut reader = options
        .open(path)
        .context("open immutable reusable block")?;
    ensure!(
        reader.metadata()?.is_file() && !link(&reader.metadata()?),
        "cannotClear: reusable file is not regular"
    );
    verify_header(&mut reader, &proof.expected)?;
    if reader.metadata()?.len() != HEADER_BYTES as u64 + len as u64 {
        return Err(BodyCorrupt.into());
    }
    let mut remaining = len;
    let mut digest = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    while remaining > 0 {
        if let Some(access) = access {
            access.check()?;
        }
        let count = remaining.min(buffer.len());
        reader.read_exact(&mut buffer[..count])?;
        digest.update(&buffer[..count]);
        remaining -= count;
    }
    if format!("{:x}", digest.finalize()) != sha256 {
        return Err(BodyCorrupt.into());
    }
    Ok(reader)
}

//! Bounded encoded blocks with RAM-only admission and an owned filesystem worker.
#[cfg(test)]
mod diagnostics;
mod io_worker;
#[cfg(test)]
pub(crate) use diagnostics::PublicationStage;
mod managed;
mod pending;
#[cfg(test)]
mod pending_tests;
use pending::PendingRam;
pub(crate) use pending::{PublicationReadBoundary, PublicationTicket};
mod reusable;
use super::worker_owner::{TrackedWorker, WorkerKind, WorkerPermit};
use anyhow::{Result, bail};
use io_worker::{Core, WriteReply};
use managed::{Account, BlockProof, Directory, HEADER_BYTES};
pub use reusable::ReusableRepresentation;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
    mpsc,
};
use std::time::{Duration, Instant};
#[derive(Clone, Debug)]
pub struct CacheOptions {
    pub root: PathBuf,
    pub block_size: usize,
    pub ram_bytes: u64,
    pub disk_bytes: u64,
    pub max_objects: usize,
    pub max_blocks: usize,
}
impl Default for CacheOptions {
    fn default() -> Self {
        Self {
            root: std::env::temp_dir().join("lx-native-encoded-audio"),
            block_size: 256 * 1024,
            ram_bytes: 8 * 1024 * 1024,
            disk_bytes: 512 * 1024 * 1024,
            max_objects: 64,
            max_blocks: 4096,
        }
    }
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub enum CacheIoErrorCode {
    #[default]
    None,
    Initialization,
    Publish,
    Reclaim,
    Audit,
    Closed,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct CacheStats {
    pub ram_bytes: u64,
    pub disk_bytes: u64,
    pub encoded_bytes: u64,
    pub foreign_files: usize,
    pub foreign_bytes: u64,
    pub cannot_clear: bool,
    pub recovered_bytes: u64,
    pub namespaces: usize,
    pub objects: usize,
    pub blocks: usize,
    pub active_leases: usize,
    pub pinned_blocks: usize,
    pub deferred_blocks: usize,
    pub epoch: u64,
    pub initialized: bool,
    pub initializing: bool,
    pub io_active: bool,
    pub io_waiting_for_registry: bool,
    pub pending_publish_bytes: u64,
    pub pending_ram_blocks: usize,
    pub pending_disk_reservation_bytes: u64,
    pub retiring_blocks: usize,
    pub worker_finished: bool,
    pub last_io_error: CacheIoErrorCode,
    pub reusable_representations: usize,
    pub reusable_blocks: usize,
    pub reusable_bytes: u64,
    pub protected_views: usize,
}
struct FileBlock {
    pub path: PathBuf,
    pub proof: BlockProof,
    pub len: usize,
    pub serial: u64,
    pub readers: AtomicUsize,
    pub sha256: String,
    pub reader: Mutex<Option<std::fs::File>>,
}
struct Block {
    pub file: Arc<FileBlock>,
    pub ram: Option<Arc<[u8]>>,
    pub touched: u64,
    pub deferred: bool,
    pub ready: bool,
}
#[derive(Default)]
struct Object {
    pub publication_error: bool,
    pub leases: usize,
    pub touched: u64,
    pub clear_pending_epoch: Option<u64>,
    pub generation: u64,
    pub publishing: usize,
    pub reuse: Option<reusable::Metadata>,
    pub protected: bool,
}
struct Retired {
    pub file: Arc<FileBlock>,
    pub failed: bool,
}
struct WriteJob {
    pub object: String,
    pub offset: u64,
    pub generation: u64,
    pub serial: u64,
    pub bytes: Arc<[u8]>,
    pub pending_disk_reservation: u64,
    pub cancel: Arc<AtomicBool>,
    pub reply: WriteReply,
    #[cfg(test)]
    diagnostic_enqueued_at: Instant,
    #[cfg(test)]
    diagnostic_dequeued_at: Option<Instant>,
}
struct ClearJob {
    pub reply: mpsc::SyncSender<std::result::Result<CacheStats, String>>,
}
#[derive(Default)]
struct State {
    pub pending_ram: HashMap<(String, u64), PendingRam>,
    pub pending_disk_reservation: u64,
    pub objects: HashMap<String, Object>,
    pub blocks: HashMap<(String, u64), Block>,
    pub pins: HashMap<(String, u64), usize>,
    pub retired: HashMap<u64, Retired>,
    pub writes: VecDeque<WriteJob>,
    pub clears: VecDeque<ClearJob>,
    pub reuse_jobs: VecDeque<reusable::Job>,
    pub reusable: BTreeMap<String, reusable::Record>,
    pub reuse_fenced: bool,
    pub serial: u64,
    pub ram: u64,
    pub epoch: u64,
    pub pending_bytes: u64,
    pub closing: bool,
    pub initialized: bool,
    pub init_error: Option<String>,
    pub error_code: CacheIoErrorCode,
    pub account: Account,
}
pub struct CacheStore {
    core: Arc<Core>,
    owner: TrackedWorker,
}
pub struct CacheLease {
    store: Arc<CacheStore>,
    object: String,
}
pub struct CachePin {
    store: Arc<CacheStore>,
    key: (String, u64),
    admitted: bool,
}
pub struct CacheClearTicket {
    receiver: mpsc::Receiver<std::result::Result<CacheStats, String>>,
    core: Arc<Core>,
}
impl CacheClearTicket {
    pub fn wait(&self, timeout: Duration) -> Result<CacheStats> {
        match self.receiver.recv_timeout(timeout) {
            Ok(Ok(stats)) => Ok(stats),
            Ok(Err(message)) => bail!(message),
            Err(mpsc::RecvTimeoutError::Timeout) => bail!("cache clear barrier timed out"),
            Err(_) => bail!("cache I/O worker closed before its clear barrier"),
        }
    }
    pub fn stats(&self) -> CacheStats {
        self.core.stats()
    }
}
impl CacheStore {
    #[cfg(test)]
    pub(crate) fn record_publication_stage(
        &self,
        stage: PublicationStage,
        source_serial: u64,
        offset: u64,
        elapsed: Duration,
    ) {
        self.core
            .diagnostics
            .record(stage, None, Some(source_serial), offset, elapsed);
    }
    #[cfg(test)]
    pub(crate) fn publication_diagnostic_id(&self) -> u64 {
        self.core.diagnostics.id()
    }
    pub fn new(options: CacheOptions) -> Result<Arc<Self>> {
        if options.block_size == 0
            || options.block_size > 4 * 1024 * 1024
            || options.disk_bytes < options.block_size as u64
            || options.max_objects == 0
            || options.max_objects > 4096
            || options.max_blocks == 0
            || options.max_blocks > 8192
        {
            bail!("invalid encoded cache quota");
        }
        let permit = WorkerPermit::reserve(WorkerKind::CacheIo)?;
        let core = Arc::new(Core::new(options));
        let worker = core.clone();
        let handle = std::thread::Builder::new()
            .name("encoded-cache-io".into())
            .spawn(move || io_worker::run(worker))?;
        let owner = permit.attach(handle);
        Ok(Arc::new(Self { core, owner }))
    }
    pub fn options(&self) -> &CacheOptions {
        &self.core.options
    }
    pub fn capacity_blocks(&self) -> usize {
        self.core.capacity.load(Ordering::Acquire).max(1)
    }
    pub fn stats(&self) -> CacheStats {
        let mut stats = self.core.stats();
        stats.worker_finished = self.owner.is_finished();
        stats
    }
    pub fn wait_ready(&self, timeout: Duration) -> Result<()> {
        let deadline = Instant::now() + timeout;
        let mut state = self.core.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.closing || self.core.access.closing.load(Ordering::Acquire) {
            bail!("cache initialization cancelled");
        }
        while !state.initialized {
            if let Some(error) = &state.init_error {
                bail!("{error}");
            }
            if state.closing {
                bail!("cache initialization cancelled");
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                bail!("cache initialization timed out");
            }
            state = self
                .core
                .changed
                .wait_timeout(state, remaining)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
        Ok(())
    }
    pub async fn wait_ready_async(&self, cancel: &AtomicBool) -> Result<()> {
        loop {
            {
                let state = self.core.state.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(error) = &state.init_error {
                    bail!("{error}");
                }
                if state.closing
                    || self.core.access.closing.load(Ordering::Acquire)
                    || cancel.load(Ordering::Acquire)
                {
                    bail!("cache initialization cancelled");
                }
                if state.initialized {
                    return Ok(());
                }
            }
            tokio::select! {_=self.core.notify.notified()=>(),_=tokio::time::sleep(Duration::from_millis(10))=>()}
        }
    }
    pub fn lease(self: &Arc<Self>, object: String) -> Result<CacheLease> {
        let mut state = self.core.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.closing || self.core.access.closing.load(Ordering::Acquire) {
            bail!("cache is closing");
        }
        if let Some(error) = &state.init_error {
            bail!("{error}");
        }
        if !state.objects.contains_key(&object) && state.objects.len() >= self.options().max_objects
        {
            let old = state
                .objects
                .iter()
                .filter(|(key, v)| {
                    v.leases == 0
                        && v.publishing == 0
                        && !state.pins.keys().any(|(pin, _)| pin == *key)
                })
                .min_by_key(|(_, v)| v.touched)
                .map(|(key, _)| key.clone());
            if let Some(old) = old {
                Core::retire_object(&mut state, &old);
            } else {
                bail!("encoded cache active-object quota exhausted");
            }
        }
        state.serial += 1;
        let serial = state.serial;
        let entry = state.objects.entry(object.clone()).or_default();
        entry.leases += 1;
        entry.touched = serial;
        self.core.refresh(&state);
        drop(state);
        self.core.wake();
        Ok(CacheLease {
            store: self.clone(),
            object,
        })
    }
    pub fn pin(self: &Arc<Self>, object: &str, offset: u64) -> CachePin {
        let key = (object.to_owned(), offset);
        let mut state = self.core.state.lock().unwrap_or_else(|e| e.into_inner());
        let admitted = !state.closing
            && !self.core.access.closing.load(Ordering::Acquire)
            && state.init_error.is_none();
        if admitted {
            *state.pins.entry(key.clone()).or_default() += 1;
            self.core.refresh(&state);
        }
        CachePin {
            store: self.clone(),
            key,
            admitted,
        }
    }
    fn enqueue(
        &self,
        object: &str,
        offset: u64,
        bytes: Vec<u8>,
        cancel: Arc<AtomicBool>,
        reply: WriteReply,
        foreground: bool,
    ) -> Result<bool> {
        if bytes.is_empty()
            || bytes.len() > self.options().block_size
            || !offset.is_multiple_of(self.options().block_size as u64)
        {
            bail!("invalid encoded block interval");
        }
        let foreground_digest = foreground.then(|| {
            use sha2::{Digest, Sha256};
            format!("{:x}", Sha256::digest(&bytes))
        });
        let mut state = self.core.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.closing
            || self.core.access.closing.load(Ordering::Acquire)
            || cancel.load(Ordering::Acquire)
        {
            bail!("cache publication cancelled");
        }
        if let Some(error) = &state.init_error {
            bail!("{error}");
        }
        let entry = state
            .objects
            .get(object)
            .ok_or_else(|| anyhow::anyhow!("encoded cache lease missing"))?;
        if entry.publication_error {
            bail!("encoded cache durable publication failed");
        }
        let generation = entry.generation;
        let known_interval_matches = entry
            .reuse
            .as_ref()
            .and_then(|metadata| {
                state
                    .reusable
                    .get(&metadata.data_key)
                    .filter(|record| record.metadata == *metadata)
            })
            .and_then(|record| record.files.get(&offset))
            .is_none_or(|file| {
                file.len == bytes.len() && foreground_digest.as_ref() == Some(&file.sha256)
            });
        let capacity = self
            .options()
            .ram_bytes
            .max((self.options().block_size as u64).saturating_mul(2));
        if state.writes.len() >= 64 || state.pending_bytes + bytes.len() as u64 > capacity {
            bail!("cache publication queue quota exhausted");
        }
        while state.ram + state.pending_bytes + bytes.len() as u64 > capacity {
            let key = state
                .blocks
                .iter()
                .filter(|(_, block)| block.ram.is_some())
                .min_by_key(|(_, block)| block.touched)
                .map(|(key, _)| key.clone());
            if let Some(key) = key {
                let block = state.blocks.get_mut(&key).unwrap();
                let len = block.file.len as u64;
                block.ram = None;
                state.ram -= len;
            } else {
                break;
            }
        }
        state.serial += 1;
        let serial = state.serial;
        let reservation = (bytes.len() as u64)
            .saturating_add(HEADER_BYTES as u64)
            .saturating_add(managed::MAX_PUBLICATION_LEDGER_RESERVATION);
        let pending_jobs = state
            .objects
            .values()
            .map(|object| object.publishing)
            .sum::<usize>();
        let ram_ready = foreground
            && known_interval_matches
            && state.initialized
            && !state.account.unmeasurable
            && state.account.foreign_files == 0
            && state
                .ram
                .saturating_add(state.pending_bytes)
                .saturating_add(bytes.len() as u64)
                <= self.options().ram_bytes
            && Core::all_files(&state)
                .len()
                .saturating_add(state.retired.len())
                .saturating_add(pending_jobs)
                < self.capacity_blocks()
            && state
                .account
                .disk_bytes
                .saturating_add(state.pending_disk_reservation)
                .saturating_add(reservation)
                <= self.options().disk_bytes;
        let bytes: Arc<[u8]> = Arc::from(bytes);
        let pending_disk_reservation = if ram_ready { reservation } else { 0 };
        if ram_ready {
            state.pending_disk_reservation =
                state.pending_disk_reservation.saturating_add(reservation);
            state.pending_ram.insert(
                (object.to_owned(), offset),
                PendingRam {
                    generation,
                    serial,
                    bytes: bytes.clone(),
                    cancel: cancel.clone(),
                },
            );
        }
        state.pending_bytes += bytes.len() as u64;
        state.objects.get_mut(object).unwrap().publishing += 1;
        state.writes.push_back(WriteJob {
            object: object.to_owned(),
            offset,
            generation,
            serial,
            bytes,
            pending_disk_reservation,
            cancel,
            reply,
            #[cfg(test)]
            diagnostic_enqueued_at: Instant::now(),
            #[cfg(test)]
            diagnostic_dequeued_at: None,
        });
        self.core.refresh(&state);
        drop(state);
        self.core.wake();
        Ok(ram_ready)
    }
    pub fn publish(&self, object: &str, offset: u64, bytes: &[u8]) -> Result<()> {
        let (sender, receiver) = mpsc::sync_channel(1);
        self.enqueue(
            object,
            offset,
            bytes.to_vec(),
            Arc::new(AtomicBool::new(false)),
            WriteReply::Sync(sender),
            false,
        )?;
        match receiver.recv() {
            Ok(Ok(())) => Ok(()),
            Ok(Err(error)) => bail!(error),
            Err(_) => bail!("cache I/O worker closed during publication"),
        }
    }
    pub async fn publish_async(
        self: &Arc<Self>,
        object: String,
        offset: u64,
        bytes: Vec<u8>,
        cancel: Arc<AtomicBool>,
    ) -> Result<()> {
        let (sender, receiver) = tokio::sync::oneshot::channel();
        self.enqueue(
            &object,
            offset,
            bytes,
            cancel.clone(),
            WriteReply::Async(sender),
            false,
        )?;
        tokio::select! {result=receiver=>match result{Ok(Ok(()))=>Ok(()),Ok(Err(error))=>Err(anyhow::anyhow!(error)),Err(_)=>Err(anyhow::anyhow!("cache I/O worker closed during publication"))},_=async{while !cancel.load(Ordering::Acquire){tokio::time::sleep(Duration::from_millis(5)).await;}}=>Err(anyhow::anyhow!("cache publication cancelled"))}
    }
    pub fn read(&self, object: &str, position: u64, output: &mut [u8]) -> Result<Option<usize>> {
        self.read_inner(object, position, output)
    }
    pub(crate) fn try_read(
        &self,
        object: &str,
        position: u64,
        output: &mut [u8],
    ) -> Result<Option<usize>> {
        self.read_inner(object, position, output)
    }
    fn read_inner(&self, object: &str, position: u64, output: &mut [u8]) -> Result<Option<usize>> {
        let (file, ram, within, count) = {
            let mut state = self.core.state.lock().unwrap_or_else(|e| e.into_inner());
            if state.closing || self.core.access.closing.load(Ordering::Acquire) {
                bail!("cache reader is closing");
            }
            state.serial += 1;
            let serial = state.serial;
            let start =
                position / self.options().block_size as u64 * self.options().block_size as u64;
            if state
                .objects
                .get(object)
                .is_some_and(|entry| entry.publication_error)
            {
                bail!("encoded cache durable publication failed");
            }
            let key = (object.to_owned(), start);
            if let Some(pending) = state.pending_ram.get(&key)
                && !pending.cancel.load(Ordering::Acquire)
                && state
                    .objects
                    .get(object)
                    .is_some_and(|entry| entry.generation == pending.generation)
            {
                let within = (position - start) as usize;
                if within < pending.bytes.len() {
                    let count = output.len().min(pending.bytes.len() - within);
                    output[..count].copy_from_slice(&pending.bytes[within..within + count]);
                    return Ok(Some(count));
                }
            }
            let Some(block) = state.blocks.get_mut(&(object.to_owned(), start)) else {
                return Ok(None);
            };
            if !block.ready {
                return Ok(None);
            }
            let within = (position - start) as usize;
            if within >= block.file.len {
                return Ok(None);
            }
            block.touched = serial;
            block.file.readers.fetch_add(1, Ordering::AcqRel);
            self.core.readers.fetch_add(1, Ordering::AcqRel);
            (
                block.file.clone(),
                block.ram.clone(),
                within,
                output.len().min(block.file.len - within),
            )
        };
        let _guard = ReadGuard {
            file: file.clone(),
            core: self.core.clone(),
        };
        if let Some(ram) = ram {
            output[..count].copy_from_slice(&ram[within..within + count]);
        } else {
            let mut retained = file
                .reader
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            if let Some(input) = retained.as_mut() {
                #[cfg(not(windows))]
                {
                    use sha2::{Digest, Sha256};
                    let mut bytes = vec![0u8; file.len];
                    input.seek(SeekFrom::Start(HEADER_BYTES as u64))?;
                    input.read_exact(&mut bytes)?;
                    if format!("{:x}", Sha256::digest(&bytes)) != file.sha256 {
                        bail!("reusable body digest mismatch");
                    }
                    output[..count].copy_from_slice(&bytes[within..within + count]);
                }
                #[cfg(windows)]
                {
                    input.seek(SeekFrom::Start(HEADER_BYTES as u64 + within as u64))?;
                    input.read_exact(&mut output[..count])?;
                }
            } else {
                drop(retained);
                let mut input = Directory::read_with_proof(&file.path, &file.proof)?;
                input.seek(SeekFrom::Start(HEADER_BYTES as u64 + within as u64))?;
                input.read_exact(&mut output[..count])?;
            }
        }
        Ok(Some(count))
    }
    pub fn contains(&self, object: &str, start: u64) -> bool {
        let state = self.core.state.lock().unwrap_or_else(|e| e.into_inner());
        !state.closing
            && !self.core.access.closing.load(Ordering::Acquire)
            && !state
                .objects
                .get(object)
                .is_some_and(|entry| entry.publication_error)
            && (state
                .pending_ram
                .get(&(object.to_owned(), start))
                .is_some_and(|pending| {
                    !pending.cancel.load(Ordering::Acquire)
                        && state
                            .objects
                            .get(object)
                            .is_some_and(|entry| entry.generation == pending.generation)
                })
                || state
                    .blocks
                    .get(&(object.to_owned(), start))
                    .is_some_and(|block| block.ready))
    }
    pub(crate) fn try_contains(&self, object: &str, start: u64) -> Option<bool> {
        Some(self.contains(object, start))
    }

    /// Advisory RAM-only admission. The filesystem owner still performs the
    /// actual quota reservation and verifies every retained file before writing.
    pub(crate) fn can_admit_lookahead(&self, object: &str, bytes: usize) -> bool {
        let state = self
            .core
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let capacity = self.capacity_blocks().min(self.options().max_blocks);
        let pending_capacity = self
            .options()
            .ram_bytes
            .max((self.options().block_size as u64).saturating_mul(2));
        if state.closing
            || self.core.access.closing.load(Ordering::Acquire)
            || state.init_error.is_some()
            || !state.initialized
            || state.writes.len() >= 64
            || state.pending_bytes.saturating_add(bytes as u64) > pending_capacity
            || state.objects.get(object).is_none_or(|entry| {
                entry.leases == 0 || entry.clear_pending_epoch.is_some() || entry.publication_error
            })
        {
            return false;
        }
        // Alias over-counting is conservative and does not clone file owners.
        let resident_bound = state
            .blocks
            .len()
            .saturating_add(state.retired.len())
            .saturating_add(
                state
                    .reusable
                    .values()
                    .map(|record| record.files.len())
                    .sum::<usize>(),
            )
            .saturating_add(
                state
                    .objects
                    .values()
                    .map(|object| object.publishing)
                    .sum::<usize>(),
            );
        let required_bytes = (bytes as u64).saturating_add(HEADER_BYTES as u64);
        if resident_bound < capacity
            && state
                .account
                .disk_bytes
                .saturating_add(state.pending_disk_reservation)
                .saturating_add(required_bytes)
                <= self.options().disk_bytes
        {
            return true;
        }
        state
            .blocks
            .values()
            .any(|block| Core::file_is_evictable(&state, &block.file))
            || state
                .reusable
                .values()
                .flat_map(|record| record.files.values())
                .any(|file| Core::file_is_evictable(&state, file))
    }

    /// Protect every interval of one completed response for its active lease.
    /// This does not admit additional storage or combine response validators.
    pub(crate) fn protect_complete_view(&self, object: &str, total: u64) -> Result<()> {
        let mut state = self
            .core
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if state.closing || self.core.access.closing.load(Ordering::Acquire) {
            bail!("cache is closing");
        }
        let block_size = self.options().block_size as u64;
        if total == 0 || total.div_ceil(block_size) > self.capacity_blocks() as u64 {
            bail!("complete response exceeds the bounded cache view");
        }
        let entry = state
            .objects
            .get(object)
            .ok_or_else(|| anyhow::anyhow!("complete response lease missing"))?;
        if entry.leases == 0 || entry.publishing != 0 || entry.clear_pending_epoch.is_some() {
            bail!("complete response lease cannot retain its current intervals");
        }
        let mut offset = 0_u64;
        while offset < total {
            let expected = (total - offset).min(block_size) as usize;
            if !state
                .blocks
                .get(&(object.to_owned(), offset))
                .is_some_and(|block| block.ready && !block.deferred && block.file.len == expected)
            {
                bail!("complete response has a missing or unverified interval");
            }
            offset += expected as u64;
        }
        state.objects.get_mut(object).unwrap().protected = true;
        self.core.refresh(&state);
        Ok(())
    }
    pub fn request_clear(&self) -> Result<CacheClearTicket> {
        let (sender, receiver) = mpsc::sync_channel(1);
        let mut state = self.core.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.closing || self.core.access.closing.load(Ordering::Acquire) {
            bail!("cache is closing");
        }
        if let Some(error) = &state.init_error {
            bail!("{error}");
        }
        if state.clears.len() >= 16 {
            bail!("cache clear barrier queue is full");
        }
        state.epoch = state.epoch.wrapping_add(1);
        state.reuse_fenced = true;
        let epoch = state.epoch;
        for object in state.objects.values_mut() {
            object.clear_pending_epoch = Some(epoch);
        }
        for block in state.blocks.values_mut() {
            block.deferred = true;
        }
        Core::remove_reusable(&mut state, None);
        let inactive: Vec<_> = state
            .objects
            .iter()
            .filter(|(key, v)| {
                v.leases == 0 && v.publishing == 0 && !state.pins.keys().any(|(pin, _)| pin == *key)
            })
            .map(|(key, _)| key.clone())
            .collect();
        for object in inactive {
            Core::retire_object(&mut state, &object);
        }
        state.clears.push_back(ClearJob { reply: sender });
        self.core.refresh(&state);
        drop(state);
        self.core.wake();
        Ok(CacheClearTicket {
            receiver,
            core: self.core.clone(),
        })
    }
    pub fn clear(&self) -> Result<CacheStats> {
        self.request_clear()?.wait(Duration::from_secs(60))
    }
    pub fn invalidate(&self, object: &str) -> Result<()> {
        let mut state = self.core.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.closing || self.core.access.closing.load(Ordering::Acquire) {
            bail!("cache is closing");
        }
        if let Some(entry) = state.objects.get_mut(object) {
            entry.generation = entry.generation.wrapping_add(1);
            entry.reuse = None;
            entry.protected = false;
            entry.publication_error = false;
        }
        state.pending_ram.retain(|(key, _), _| key != object);
        let keys: Vec<_> = state
            .blocks
            .keys()
            .filter(|(key, _)| key == object)
            .cloned()
            .collect();
        for key in keys {
            Core::retire_block(&mut state, &key);
        }
        self.core.refresh(&state);
        drop(state);
        self.core.wake();
        Ok(())
    }
    /// Protect a complete validated cached view for its existing lease lifetime.
    pub fn protect_reusable_view(&self, object: &str) -> Result<()> {
        let mut state = self
            .core
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if state.closing || self.core.access.closing.load(Ordering::Acquire) {
            bail!("cache is closing");
        }
        let entry = state
            .objects
            .get(object)
            .ok_or_else(|| anyhow::anyhow!("reusable view lease missing"))?;
        let metadata = entry
            .reuse
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("reusable view is not bound"))?;
        if entry.leases == 0 {
            bail!("reusable view lease is inactive");
        }
        let mut offset = 0u64;
        while offset < metadata.total {
            let expected = (metadata.total - offset).min(metadata.block_size as u64) as usize;
            if !state
                .blocks
                .get(&(object.to_owned(), offset))
                .is_some_and(|block| block.file.len == expected)
            {
                bail!("reusable view has an unverified hole");
            }
            offset = offset
                .checked_add(expected as u64)
                .ok_or_else(|| anyhow::anyhow!("reusable view interval overflow"))?;
        }
        state.objects.get_mut(object).unwrap().protected = true;
        self.core.refresh(&state);
        Ok(())
    }
    pub fn begin_shutdown(&self) {
        self.core.access.closing.store(true, Ordering::Release);
        let mut state = self.core.state.lock().unwrap_or_else(|e| e.into_inner());
        state.closing = true;
        let jobs = self.core.cancel_queued(&mut state);
        for clear in state.clears.drain(..) {
            let _ = clear
                .reply
                .try_send(Err("cache clear barrier cancelled".into()));
        }
        self.core.refresh(&state);
        drop(state);
        for job in jobs {
            job.reply
                .complete(Err("cache publication cancelled".into()));
        }
        self.core.wake();
    }
    pub fn shutdown(&self, timeout: Duration) -> bool {
        self.begin_shutdown();
        self.owner.wait(timeout)
    }
    pub fn outstanding_workers() -> usize {
        super::worker_owner::outstanding(WorkerKind::CacheIo)
    }
}
struct ReadGuard {
    file: Arc<FileBlock>,
    core: Arc<Core>,
}
impl Drop for ReadGuard {
    fn drop(&mut self) {
        self.file.readers.fetch_sub(1, Ordering::AcqRel);
        self.core.readers.fetch_sub(1, Ordering::AcqRel);
        self.core.wake();
    }
}
impl Drop for CacheLease {
    fn drop(&mut self) {
        let mut state = self
            .store
            .core
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(object) = state.objects.get_mut(&self.object) {
            object.leases = object.leases.saturating_sub(1);
        }
        Core::release_object(&mut state, &self.object);
        self.store.core.refresh(&state);
        drop(state);
        self.store.core.wake();
    }
}
impl Drop for CachePin {
    fn drop(&mut self) {
        if !self.admitted {
            return;
        }
        let mut state = self
            .store
            .core
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(count) = state.pins.get_mut(&self.key) {
            *count -= 1;
            if *count == 0 {
                state.pins.remove(&self.key);
            }
        }
        Core::release_object(&mut state, &self.key.0);
        self.store.core.refresh(&state);
        drop(state);
        self.store.core.wake();
    }
}
impl Drop for CacheStore {
    fn drop(&mut self) {
        self.begin_shutdown();
    }
}

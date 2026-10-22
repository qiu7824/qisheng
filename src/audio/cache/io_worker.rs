//! Filesystem serialization owned by one tracked worker, never by cache endpoints.
use super::managed::{Directory, Publication, RegistryAccess};
use super::reusable;
use super::{
    Block, CacheIoErrorCode, CacheOptions, CacheStats, FileBlock, Retired, State, WriteJob,
};
use anyhow::{Result, bail};
use sha2::{Digest, Sha256};
use std::cell::Cell;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::time::Duration;
use tokio::sync::{Notify, oneshot};

pub(super) enum WriteReply {
    Staged(Arc<super::pending::PublicationCompletion>),
    Sync(mpsc::SyncSender<std::result::Result<(), String>>),
    Async(oneshot::Sender<std::result::Result<(), String>>),
}
impl WriteReply {
    pub fn complete(self, result: std::result::Result<(), String>) {
        match self {
            Self::Staged(completion) => completion.complete(result),
            Self::Sync(sender) => {
                let _ = sender.try_send(result);
            }
            Self::Async(sender) => {
                let _ = sender.send(result);
            }
        }
    }
}

struct ActivePublication<'a> {
    core: &'a Core,
    directory: &'a Directory,
    completion: Arc<super::pending::PublicationCompletion>,
    object: String,
    generation: u64,
    serial: u64,
    offset: u64,
    len: u64,
    reservation: u64,
    settled: Cell<bool>,
}
impl<'a> ActivePublication<'a> {
    fn new(core: &'a Core, directory: &'a Directory, job: &WriteJob) -> Option<Self> {
        let WriteReply::Staged(completion) = &job.reply else {
            return None;
        };
        Some(Self {
            core,
            directory,
            completion: completion.clone(),
            object: job.object.clone(),
            generation: job.generation,
            serial: job.serial,
            offset: job.offset,
            len: job.bytes.len() as u64,
            reservation: job.pending_disk_reservation,
            settled: Cell::new(false),
        })
    }
}
impl Drop for ActivePublication<'_> {
    fn drop(&mut self) {
        if !self.completion.is_pending() {
            return;
        }
        // The consumed WriteJob and its native publication stack have already
        // unwound. Preserve the directory's conservative physical reservation;
        // only its no-longer-owned RAM job charge is settled here.
        let account = self.directory.account();
        let mut state = self
            .core
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        state.account = account;
        if !self.settled.get() {
            state.pending_bytes = state.pending_bytes.saturating_sub(self.len);
            state.pending_disk_reservation = state
                .pending_disk_reservation
                .saturating_sub(self.reservation);
        }
        if let Some(object) = state.objects.get_mut(&self.object) {
            if !self.settled.get() {
                object.publishing = object.publishing.saturating_sub(1);
            }
            if object.generation == self.generation {
                object.publication_error = true;
            }
        }
        state.pending_ram.retain(|(name, _), pending| {
            name != &self.object || pending.generation != self.generation
        });
        let key = (self.object.clone(), self.offset);
        if state
            .blocks
            .get(&key)
            .is_some_and(|block| block.file.serial == self.serial)
        {
            Core::retire_block(&mut state, &key);
        }
        state.error_code = CacheIoErrorCode::Publish;
        self.core.refresh(&state);
        drop(state);
        self.completion.complete(Err(
            "cache I/O worker exited before publication completed".into()
        ));
        self.core.wake();
    }
}

pub(super) struct Core {
    #[cfg(test)]
    pub(super) diagnostics: super::diagnostics::PublicationDiagnostics,
    pub options: CacheOptions,
    pub state: Mutex<State>,
    pub changed: Condvar,
    pub notify: Notify,
    pub access: Arc<RegistryAccess>,
    pub capacity: AtomicUsize,
    pub readers: AtomicUsize,
    active: AtomicBool,
    finished: AtomicBool,
    snapshot: Mutex<CacheStats>,
}
impl Core {
    pub fn new(options: CacheOptions) -> Self {
        let access = Arc::new(RegistryAccess::new(options.disk_bytes));
        Self {
            #[cfg(test)]
            diagnostics: Default::default(),
            capacity: AtomicUsize::new(options.max_blocks),
            options,
            state: Mutex::new(State::default()),
            changed: Condvar::new(),
            notify: Notify::new(),
            access,
            readers: AtomicUsize::new(0),
            active: AtomicBool::new(false),
            finished: AtomicBool::new(false),
            snapshot: Mutex::new(CacheStats {
                initializing: true,
                ..CacheStats::default()
            }),
        }
    }
    pub fn wake(&self) {
        self.changed.notify_all();
        self.notify.notify_waiters();
    }
    pub fn refresh(&self, state: &State) {
        let files = Self::all_files(state);
        let reusable: BTreeMap<_, _> = state
            .reusable
            .values()
            .flat_map(|record| record.files.values())
            .map(|file| (file.serial, file))
            .collect();
        let pinned: std::collections::BTreeSet<_> = state
            .pins
            .keys()
            .filter_map(|key| state.blocks.get(key).map(|block| block.file.serial))
            .collect();
        let pending_pins = state
            .pins
            .keys()
            .filter(|key| !state.blocks.contains_key(*key))
            .count();
        let deferred: std::collections::BTreeSet<_> = state
            .blocks
            .values()
            .filter(|block| block.deferred)
            .map(|block| block.file.serial)
            .collect();
        let stats = CacheStats {
            ram_bytes: state.ram.saturating_add(
                state
                    .pending_ram
                    .values()
                    .map(|value| value.bytes.len() as u64)
                    .sum::<u64>(),
            ),
            pending_ram_blocks: state.pending_ram.len(),
            pending_disk_reservation_bytes: state.pending_disk_reservation,
            disk_bytes: state.account.disk_bytes,
            encoded_bytes: files.values().map(|file| file.len as u64).sum(),
            foreign_files: state.account.foreign_files,
            foreign_bytes: state.account.foreign_bytes,
            cannot_clear: state.account.unmeasurable
                || state.account.foreign_files != 0
                || state.retired.values().any(|item| item.failed)
                || state.error_code == CacheIoErrorCode::Audit,
            recovered_bytes: state.account.recovered_bytes,
            namespaces: state.account.live_namespaces,
            objects: state.objects.len(),
            blocks: files.len(),
            active_leases: state.objects.values().map(|object| object.leases).sum(),
            pinned_blocks: pinned.len() + pending_pins,
            deferred_blocks: deferred.len(),
            epoch: state.epoch,
            initialized: state.initialized,
            initializing: !state.initialized && state.init_error.is_none() && !state.closing,
            pending_publish_bytes: state.pending_bytes,
            retiring_blocks: state.retired.len(),
            last_io_error: state.error_code,
            reusable_representations: state.reusable.len(),
            reusable_blocks: reusable.len(),
            reusable_bytes: reusable.values().map(|file| file.len as u64).sum(),
            protected_views: state
                .objects
                .values()
                .filter(|object| object.protected && object.leases > 0)
                .count(),
            ..CacheStats::default()
        };
        *self
            .snapshot
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = stats;
    }
    pub fn stats(&self) -> CacheStats {
        let mut stats = *self
            .snapshot
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        stats.io_active = self.active.load(Ordering::Acquire);
        stats.io_waiting_for_registry = self.access.waiting.load(Ordering::Acquire);
        stats.worker_finished = self.finished.load(Ordering::Acquire);
        stats
    }
    pub fn retire_block(state: &mut State, key: &(String, u64)) {
        if let Some(block) = state.blocks.remove(key) {
            if block.ram.is_some() {
                state.ram = state.ram.saturating_sub(block.file.len as u64);
            }
            if state
                .blocks
                .values()
                .any(|old| old.file.serial == block.file.serial)
                || state.reusable.values().any(|record| {
                    record
                        .files
                        .values()
                        .any(|old| old.serial == block.file.serial)
                })
            {
                return;
            }
            state.retired.insert(
                block.file.serial,
                Retired {
                    file: block.file,
                    failed: false,
                },
            );
        }
    }
    pub fn retire_object(state: &mut State, object: &str) {
        state.pending_ram.retain(|(key, _), _| key != object);
        let keys: Vec<_> = state
            .blocks
            .keys()
            .filter(|(name, _)| name == object)
            .cloned()
            .collect();
        for key in keys {
            Self::retire_block(state, &key);
        }
        state.objects.remove(object);
    }
    pub fn release_object(state: &mut State, name: &str) {
        let release = state.objects.get(name).is_some_and(|object| {
            (object.clear_pending_epoch.is_some() || object.reuse.is_some())
                && object.leases == 0
                && object.publishing == 0
                && !state.pins.keys().any(|(key, _)| key == name)
        });
        if release {
            Self::retire_object(state, name);
        }
    }
    pub fn cancel_queued(&self, state: &mut State) -> Vec<WriteJob> {
        for job in state.reuse_jobs.drain(..) {
            job.cancel().store(true, Ordering::Release);
            job.finish_cancelled();
        }
        let jobs: Vec<_> = state.writes.drain(..).collect();
        for job in &jobs {
            job.cancel.store(true, Ordering::Release);
            state.pending_bytes = state.pending_bytes.saturating_sub(job.bytes.len() as u64);
            state.pending_disk_reservation = state
                .pending_disk_reservation
                .saturating_sub(job.pending_disk_reservation);
            let key = (job.object.clone(), job.offset);
            if state
                .pending_ram
                .get(&key)
                .is_some_and(|pending| pending.serial == job.serial)
            {
                state.pending_ram.remove(&key);
            }
            if let Some(object) = state.objects.get_mut(&job.object) {
                object.publishing = object.publishing.saturating_sub(1);
            }
        }
        jobs
    }
    pub fn all_files(state: &State) -> BTreeMap<u64, Arc<FileBlock>> {
        state
            .blocks
            .values()
            .map(|block| (block.file.serial, block.file.clone()))
            .chain(
                state
                    .reusable
                    .values()
                    .flat_map(|record| record.files.values())
                    .map(|file| (file.serial, file.clone())),
            )
            .collect()
    }
    pub fn file_is_evictable(state: &State, file: &FileBlock) -> bool {
        file.readers.load(Ordering::Acquire) == 0
            && !state.blocks.iter().any(|(alias, old)| {
                old.file.serial == file.serial
                    && (state.pins.contains_key(alias)
                        || state
                            .objects
                            .get(&alias.0)
                            .is_some_and(|object| object.protected && object.leases > 0))
            })
    }
    pub fn remove_reusable(state: &mut State, key: Option<&str>) {
        let keys: Vec<_> = state
            .reusable
            .keys()
            .filter(|data| key.is_none_or(|key| data.as_str() == key))
            .cloned()
            .collect();
        for key in keys {
            if let Some(record) = state.reusable.remove(&key) {
                for file in record.files.into_values() {
                    if !state
                        .blocks
                        .values()
                        .any(|block| block.file.serial == file.serial)
                    {
                        state.retired.entry(file.serial).or_insert(super::Retired {
                            file,
                            failed: false,
                        });
                    }
                }
            }
        }
    }
}

struct Finish(Arc<Core>);
impl Drop for Finish {
    fn drop(&mut self) {
        let (jobs, files) = {
            let mut state = self
                .0
                .state
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            if !state.initialized && state.init_error.is_none() {
                state.init_error = Some("cache initialization cancelled".into());
            }
            state.closing = true;
            self.0.access.closing.store(true, Ordering::Release);
            let jobs = self.0.cancel_queued(&mut state);
            for clear in state.clears.drain(..) {
                let _ = clear.reply.try_send(Err(
                    "cache I/O worker closed before its clear barrier".into(),
                ));
            }
            self.0.refresh(&state);
            let mut files = Core::all_files(&state);
            files.extend(
                state
                    .retired
                    .values()
                    .map(|item| (item.file.serial, item.file.clone())),
            );
            state.blocks.clear();
            state.reusable.clear();
            state.retired.clear();
            state.ram = 0;
            state.pending_ram.clear();
            (jobs, files)
        };
        drop(files);
        for job in jobs {
            job.reply
                .complete(Err("cache publication cancelled".into()));
        }
        self.0.active.store(false, Ordering::Release);
        self.0.access.waiting.store(false, Ordering::Release);
        self.0.finished.store(true, Ordering::Release);
        self.0.wake();
    }
}

pub(super) fn run(core: Arc<Core>) {
    let _finish = Finish(core.clone());
    core.active.store(true, Ordering::Release);
    let directory = match Directory::new(
        &core.options.root,
        core.options.disk_bytes,
        core.options.block_size,
        core.options.max_blocks,
        core.access.clone(),
    ) {
        Ok(directory) => directory,
        Err(error) => {
            let mut state = core.state.lock().unwrap_or_else(|error| error.into_inner());
            state.init_error = Some(format!("{error:#}"));
            state.error_code = CacheIoErrorCode::Initialization;
            state.account.unmeasurable = true;
            core.refresh(&state);
            drop(state);
            core.wake();
            return;
        }
    };
    {
        let mut state = core.state.lock().unwrap_or_else(|error| error.into_inner());
        state.account = directory.account();
        state.initialized = true;
        core.capacity.store(directory.capacity(), Ordering::Release);
        core.refresh(&state);
    }
    core.active.store(false, Ordering::Release);
    core.wake();
    loop {
        let work = {
            let mut state = core.state.lock().unwrap_or_else(|error| error.into_inner());
            if state.closing {
                break;
            }
            if let Some(clear) = state.clears.pop_front() {
                Some(Work::Clear(clear))
            } else if let Some(job) = state.reuse_jobs.pop_front() {
                Some(Work::Reusable(job))
            } else if let Some(write) = state.writes.pop_front() {
                #[cfg(test)]
                let write = {
                    let mut write = write;
                    write.diagnostic_dequeued_at = Some(std::time::Instant::now());
                    write
                };
                Some(Work::Write(write))
            } else if state
                .retired
                .values()
                .any(|item| !item.failed && item.file.readers.load(Ordering::Acquire) == 0)
            {
                Some(Work::Reclaim)
            } else {
                let _guard = core
                    .changed
                    .wait_timeout(state, Duration::from_millis(25))
                    .unwrap_or_else(|error| error.into_inner());
                None
            }
        };
        let Some(work) = work else {
            continue;
        };
        core.active.store(true, Ordering::Release);
        match work {
            Work::Write(job) => {
                let publication = ActivePublication::new(&core, &directory, &job);
                core.access.set_job_cancel(Some(job.cancel.clone()));
                write(&core, &directory, job, publication.as_ref());
                core.access.set_job_cancel(None);
            }
            Work::Reusable(job) => {
                core.access.set_job_cancel(Some(job.cancel()));
                reuse_job(&core, &directory, job);
                core.access.set_job_cancel(None);
            }
            Work::Clear(job) => {
                let result = clear(&core, &directory).map_err(|error| format!("{error:#}"));
                let _ = job.reply.try_send(result);
            }
            Work::Reclaim => {
                let _ = reclaim(&core, &directory, false);
            }
        }
        core.active.store(false, Ordering::Release);
        core.wake();
    }
    // Releasing this namespace owner is the last filesystem action before actual thread return.
    drop(directory);
}

enum Work {
    Write(WriteJob),
    Clear(super::ClearJob),
    Reclaim,
    Reusable(reusable::Job),
}

fn update_account(core: &Core, directory: &Directory, error: Option<CacheIoErrorCode>) {
    let account = directory.account();
    let mut state = core.state.lock().unwrap_or_else(|error| error.into_inner());
    state.account = account;
    if let Some(error) = error {
        state.error_code = error;
    }
    core.refresh(&state);
}

/// A retired block stays charged until unlink AND durable deregistration have succeeded.
fn reclaim(core: &Core, directory: &Directory, retry_failed: bool) -> Result<bool> {
    let files: Vec<_> = {
        let state = core.state.lock().unwrap_or_else(|error| error.into_inner());
        state
            .retired
            .values()
            .filter(|item| {
                (retry_failed || !item.failed) && item.file.readers.load(Ordering::Acquire) == 0
            })
            .map(|item| item.file.clone())
            .collect()
    };
    let mut first_error = None;
    let mut progress = false;
    for file in files {
        if core.access.closing.load(Ordering::Acquire) {
            bail!("cache reclamation cancelled");
        }
        file.reader
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take();
        let result = directory.remove_owned(&file.path);
        let account = directory.account();
        let mut state = core.state.lock().unwrap_or_else(|error| error.into_inner());
        state.account = account;
        match result {
            Ok(()) => {
                state.retired.remove(&file.serial);
                progress = true;
            }
            Err(error) => {
                if core.access.closing.load(Ordering::Acquire) || core.access.job_cancelled() {
                    core.refresh(&state);
                    return Err(error);
                }
                if let Some(item) = state.retired.get_mut(&file.serial) {
                    item.failed = true;
                }
                state.error_code = CacheIoErrorCode::Reclaim;
                if first_error.is_none() {
                    first_error = Some(error);
                }
            }
        }
        core.refresh(&state);
    }
    if let Some(error) = first_error {
        Err(error)
    } else {
        Ok(progress)
    }
}

fn current(core: &Core, job: &WriteJob) -> bool {
    if job.cancel.load(Ordering::Acquire) || core.access.closing.load(Ordering::Acquire) {
        return false;
    }
    let state = core.state.lock().unwrap_or_else(|error| error.into_inner());
    !state.closing
        && state
            .objects
            .get(&job.object)
            .is_some_and(|object| object.generation == job.generation && !object.publication_error)
}

fn evict(core: &Core, key: &(String, u64)) -> bool {
    let mut state = core.state.lock().unwrap_or_else(|error| error.into_inner());
    let files = Core::all_files(&state);
    let preferred = state.blocks.get(key).map(|block| block.file.serial);
    let victim = files
        .values()
        .filter(|file| Core::file_is_evictable(&state, file))
        .min_by_key(|file| {
            (
                usize::from(Some(file.serial) != preferred),
                state
                    .blocks
                    .values()
                    .filter(|block| block.file.serial == file.serial)
                    .map(|block| block.touched)
                    .min()
                    .unwrap_or(file.serial),
            )
        })
        .cloned();
    if let Some(victim) = victim {
        let serial = victim.serial;
        for record in state.reusable.values_mut() {
            record.files.retain(|_, file| file.serial != serial);
        }
        state.reusable.retain(|_, record| !record.files.is_empty());
        let aliases: Vec<_> = state
            .blocks
            .iter()
            .filter(|(_, block)| block.file.serial == serial)
            .map(|(key, _)| key.clone())
            .collect();
        for alias in aliases {
            Core::retire_block(&mut state, &alias);
        }
        state.retired.entry(serial).or_insert(super::Retired {
            file: victim,
            failed: false,
        });
        core.refresh(&state);
        true
    } else {
        false
    }
}

fn write(
    core: &Core,
    directory: &Directory,
    job: WriteJob,
    publication: Option<&ActivePublication<'_>>,
) {
    #[cfg(test)]
    core.diagnostics.record(
        super::PublicationStage::Queue,
        Some(job.serial),
        super::diagnostics::source_serial(&job.object),
        job.offset,
        job.diagnostic_dequeued_at
            .unwrap()
            .saturating_duration_since(job.diagnostic_enqueued_at),
    );
    let result = (|| -> Result<Option<(std::path::PathBuf, super::managed::BlockProof)>> {
        if !current(core, &job) {
            bail!("cache publication cancelled");
        }
        let key = (job.object.clone(), job.offset);
        // Existing readers retain their Arc hazard; replacement cannot unlink beneath them.
        {
            let mut state = core.state.lock().unwrap_or_else(|error| error.into_inner());
            if state.blocks.contains_key(&key) {
                Core::retire_block(&mut state, &key);
            }
            core.refresh(&state);
        }
        for _ in 0..=core.options.max_blocks {
            if !current(core, &job) {
                bail!("cache publication cancelled");
            }
            let _ = reclaim(core, directory, false);
            let resident = {
                let state = core.state.lock().unwrap_or_else(|error| error.into_inner());
                Core::all_files(&state).len() + state.retired.len()
            };
            if resident >= core.options.max_blocks {
                if !evict(core, &key) {
                    bail!("encoded cache active-block quota exhausted");
                }
                continue;
            }
            let publication = {
                #[cfg(test)]
                let _span = core.diagnostics.span(
                    super::PublicationStage::DirectoryPublish,
                    Some(job.serial),
                    super::diagnostics::source_serial(&job.object),
                    job.offset,
                );
                #[cfg(test)]
                let publication = directory.publish(
                    job.serial,
                    &job.bytes,
                    &core.diagnostics,
                    super::diagnostics::source_serial(&job.object),
                    job.offset,
                )?;
                #[cfg(not(test))]
                let publication = directory.publish(job.serial, &job.bytes)?;
                publication
            };
            match publication {
                Publication::Written { path, proof } => {
                    return Ok(Some((path, proof)));
                }
                Publication::Quota => {
                    update_account(core, directory, None);
                    if !evict(core, &key) {
                        bail!("encoded cache disk quota exhausted");
                    }
                }
            }
        }
        bail!("encoded cache disk quota exhausted")
    })();
    let len = job.bytes.len();
    let account = directory.account();
    let success = result.is_ok();
    let mut state = core.state.lock().unwrap_or_else(|error| error.into_inner());
    state.account = account;
    let result = match result {
        Ok(Some((path, proof))) => {
            let file = Arc::new(FileBlock {
                path,
                proof,
                len,
                serial: job.serial,
                readers: AtomicUsize::new(0),
                sha256: format!("{:x}", Sha256::digest(&job.bytes)),
                reader: Mutex::new(None),
            });
            let admitted = !state.closing
                && !job.cancel.load(Ordering::Acquire)
                && state.objects.get(&job.object).is_some_and(|object| {
                    object.generation == job.generation && !object.publication_error
                });
            if admitted {
                let deferred = state
                    .objects
                    .get(&job.object)
                    .is_some_and(|object| object.clear_pending_epoch.is_some());
                state.blocks.insert(
                    (job.object.clone(), job.offset),
                    Block {
                        file,
                        ram: None,
                        touched: job.serial,
                        deferred,
                        ready: false,
                    },
                );
                Ok(())
            } else {
                state.retired.insert(
                    file.serial,
                    Retired {
                        file,
                        failed: false,
                    },
                );
                Err("cache publication cancelled".into())
            }
        }
        Ok(None) => Ok(()),
        Err(error) => Err(format!("{error:#}")),
    };
    if !success && !state.closing && !job.cancel.load(Ordering::Acquire) {
        state.error_code = CacheIoErrorCode::Publish;
        if job.pending_disk_reservation != 0
            && let Some(object) = state.objects.get_mut(&job.object)
            && object.generation == job.generation
        {
            object.publication_error = true;
        }
        state.pending_ram.retain(|(object, _), pending| {
            object != &job.object || pending.generation != job.generation
        });
    }
    let persistence = state
        .objects
        .get(&job.object)
        .filter(|object| {
            object.clear_pending_epoch.is_none()
                && object.generation == job.generation
                && !object.publication_error
        })
        .and_then(|object| object.reuse.clone())
        .and_then(|metadata| {
            state
                .blocks
                .get(&(job.object.clone(), job.offset))
                .filter(|block| block.file.serial == job.serial)
                .map(|block| (metadata, block.file.clone()))
        });
    core.refresh(&state);
    drop(state);
    if let Some((metadata, file)) = persistence
        && persist_block(
            core,
            directory,
            &job.object,
            job.offset,
            metadata,
            file,
            &job.cancel,
        )
        .is_err()
    {
        update_account(core, directory, Some(CacheIoErrorCode::Publish));
    }
    let result = {
        let mut state = core.state.lock().unwrap_or_else(|error| error.into_inner());
        if let Some(object) = state.objects.get_mut(&job.object) {
            object.publishing = object.publishing.saturating_sub(1);
        }
        let admitted = !state.closing
            && !job.cancel.load(Ordering::Acquire)
            && state.objects.get(&job.object).is_some_and(|object| {
                object.generation == job.generation && !object.publication_error
            });
        let key = (job.object.clone(), job.offset);
        if state
            .pending_ram
            .get(&key)
            .is_some_and(|pending| pending.serial == job.serial)
        {
            state.pending_ram.remove(&key);
        }
        // One accepted immutable allocation remains charged until every owned
        // IO stage settles, then transfers to resident RAM exactly once.
        state.pending_bytes = state.pending_bytes.saturating_sub(len as u64);
        state.pending_disk_reservation = state
            .pending_disk_reservation
            .saturating_sub(job.pending_disk_reservation);
        if let Some(publication) = publication {
            publication.settled.set(true);
        }
        if admitted && result.is_ok() && len as u64 <= core.options.ram_bytes {
            while state
                .ram
                .saturating_add(state.pending_bytes)
                .saturating_add(len as u64)
                > core.options.ram_bytes
            {
                let victim = state
                    .blocks
                    .iter()
                    .filter(|(_, block)| block.ram.is_some())
                    .min_by_key(|(_, block)| block.touched)
                    .map(|(key, _)| key.clone());
                let Some(victim) = victim else {
                    break;
                };
                let block = state.blocks.get_mut(&victim).unwrap();
                let length = block.file.len as u64;
                block.ram = None;
                state.ram = state.ram.saturating_sub(length);
            }
            if state
                .ram
                .saturating_add(state.pending_bytes)
                .saturating_add(len as u64)
                <= core.options.ram_bytes
                && let Some(block) = state.blocks.get_mut(&key)
                && block.ram.is_none()
            {
                block.ram = Some(job.bytes.clone());
                state.ram = state.ram.saturating_add(len as u64);
            }
        }
        if admitted {
            if let Some(block) = state.blocks.get_mut(&key) {
                block.ready = true;
            }
        } else if state
            .blocks
            .get(&key)
            .is_some_and(|block| block.file.serial == job.serial)
        {
            Core::retire_block(&mut state, &key);
        }
        Core::release_object(&mut state, &job.object);
        core.refresh(&state);
        if admitted {
            result
        } else {
            Err("cache publication cancelled".into())
        }
    };
    job.reply.complete(result);
    core.wake();
}

fn registration_current(
    core: &Core,
    object: &str,
    epoch: u64,
    generation: u64,
    cancel: &AtomicBool,
) -> bool {
    let state = core.state.lock().unwrap_or_else(|error| error.into_inner());
    !state.closing
        && !state.reuse_fenced
        && state.epoch == epoch
        && !cancel.load(Ordering::Acquire)
        && state.objects.get(object).is_some_and(|object| {
            object.generation == generation && object.clear_pending_epoch.is_none()
        })
}
fn persist_block(
    core: &Core,
    directory: &Directory,
    object: &str,
    offset: u64,
    metadata: reusable::Metadata,
    file: Arc<FileBlock>,
    cancel: &AtomicBool,
) -> Result<bool> {
    #[cfg(test)]
    let _span = core.diagnostics.span(
        super::PublicationStage::PersistBlock,
        Some(file.serial),
        super::diagnostics::source_serial(object),
        offset,
    );
    if cancel.load(Ordering::Acquire) {
        return Ok(false);
    }
    {
        let state = core.state.lock().unwrap_or_else(|error| error.into_inner());
        if state.reusable.len() >= core.options.max_objects
            && !state.reusable.contains_key(&metadata.data_key)
        {
            return Ok(false);
        }
    }
    let cached = {
        let state = core.state.lock().unwrap_or_else(|error| error.into_inner());
        state
            .reusable
            .get(&metadata.data_key)
            .filter(|record| record.metadata == metadata)
            .and_then(|record| record.files.get(&offset))
            .cloned()
    };
    if cached
        .as_ref()
        .is_some_and(|cached| cached.sha256 != file.sha256 || cached.len != file.len)
    {
        directory.clear_reusable(Some(&metadata.data_key))?;
        let mut state = core.state.lock().unwrap_or_else(|error| error.into_inner());
        Core::remove_reusable(&mut state, Some(&metadata.data_key));
        if let Some(object) = state.objects.get_mut(object) {
            object.reuse = None;
        }
        core.refresh(&state);
        bail!("one strong validator contained conflicting cached block bytes");
    }
    let registration = reusable::Registration {
        representation: metadata.clone(),
        offset,
        body_sha256: file.sha256.clone(),
    };
    if registration.validate(file.len).is_err() {
        return Ok(false);
    }
    let Some(reader) = directory.promote(&file.path, registration)? else {
        return Ok(false);
    };
    *file
        .reader
        .lock()
        .unwrap_or_else(|error| error.into_inner()) = Some(reader);
    let mut state = core.state.lock().unwrap_or_else(|error| error.into_inner());
    let current = !state.closing
        && !cancel.load(Ordering::Acquire)
        && state.objects.get(object).is_some_and(|object| {
            object.clear_pending_epoch.is_none()
                && object
                    .reuse
                    .as_ref()
                    .is_some_and(|current| current.data_key == metadata.data_key)
        });
    if current {
        if let Some(cached) = cached.filter(|cached| cached.serial != file.serial) {
            for block in state.blocks.values_mut() {
                if block.file.serial == file.serial {
                    block.file = cached.clone();
                }
            }
            state.retired.entry(file.serial).or_insert(super::Retired {
                file,
                failed: false,
            });
            core.refresh(&state);
            drop(state);
            update_account(core, directory, None);
            return Ok(true);
        }
        state
            .reusable
            .entry(metadata.data_key.clone())
            .or_insert_with(|| reusable::Record {
                metadata: metadata.clone(),
                files: BTreeMap::new(),
            })
            .files
            .insert(offset, file);
        core.refresh(&state);
        drop(state);
        update_account(core, directory, None);
        Ok(true)
    } else {
        drop(state);
        core.access.set_job_cancel(None);
        let result = directory.clear_reusable(Some(&metadata.data_key));
        result?;
        update_account(core, directory, None);
        Ok(false)
    }
}
fn install_record(
    core: &Arc<Core>,
    verified: super::managed::VerifiedRepresentation,
    epoch: u64,
    cancel: &AtomicBool,
) -> Option<super::ReusableRepresentation> {
    if verified.metadata.block_size != core.options.block_size {
        return None;
    }
    let mut unused = Vec::new();
    let candidate = {
        let mut state = core.state.lock().unwrap_or_else(|error| error.into_inner());
        if state.closing || state.epoch != epoch || cancel.load(Ordering::Acquire) {
            return None;
        }
        let data_key = verified.metadata.data_key.clone();
        if state.reusable.len() >= core.options.max_objects
            && !state.reusable.contains_key(&data_key)
        {
            return None;
        }
        let old = Core::all_files(&state);
        let mut files = BTreeMap::new();
        let mut added = 0;
        for block in verified.blocks {
            if let Some(file) = old.values().find(|file| file.path == block.path) {
                unused.push(block.reader);
                files.insert(block.offset, file.clone());
            } else {
                if old.len() + added >= core.options.max_blocks {
                    unused.push(block.reader);
                    continue;
                }
                state.serial += 1;
                added += 1;
                let serial = state.serial;
                files.insert(
                    block.offset,
                    Arc::new(FileBlock {
                        path: block.path,
                        proof: block.proof,
                        len: block.len,
                        serial,
                        readers: AtomicUsize::new(0),
                        sha256: block.sha256,
                        reader: Mutex::new(Some(block.reader)),
                    }),
                );
            }
        }
        if files.is_empty() {
            return None;
        }
        let candidate = reusable::candidate(core, verified.metadata.clone(), &files, epoch);
        Core::remove_reusable(&mut state, Some(&data_key));
        for file in files.values() {
            state.retired.remove(&file.serial);
        }
        state.reusable.insert(
            data_key,
            reusable::Record {
                metadata: verified.metadata,
                files,
            },
        );
        core.refresh(&state);
        candidate
    };
    drop(unused);
    Some(candidate)
}
fn reuse_job(core: &Arc<Core>, directory: &Directory, job: reusable::Job) {
    match job {
        reusable::Job::Lookup {
            data_key,
            epoch,
            cancel,
            reply,
        } => {
            let outcome = (|| -> Result<Option<super::ReusableRepresentation>> {
                if core
                    .state
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .reuse_fenced
                {
                    return Ok(None);
                }
                let Some(verified) = directory.lookup_reusable(&data_key)? else {
                    return Ok(None);
                };
                Ok(install_record(core, verified, epoch, &cancel))
            })();
            if outcome.is_err()
                || directory.account().foreign_files > 0
                || directory.account().unmeasurable
            {
                update_account(core, directory, Some(CacheIoErrorCode::Audit));
            } else {
                update_account(core, directory, None);
            }
            let _ = reply.send(outcome.map_err(|error| format!("{error:#}")));
        }
        reusable::Job::Register {
            mut metadata,
            object,
            generation,
            epoch,
            cancel,
            reply,
        } => {
            let outcome = (|| -> Result<bool> {
                if !registration_current(core, &object, epoch, generation, &cancel) {
                    return Ok(false);
                }
                metadata.ledger_epoch = directory.ledger_epoch()?;
                let prior = directory.lookup_reusable(&metadata.data_key)?;
                let same = prior.as_ref().is_some_and(|prior| {
                    prior.metadata.strong_etag == metadata.strong_etag
                        && prior.metadata.total == metadata.total
                        && prior.metadata.block_size == metadata.block_size
                });
                if same {
                    if let Some(prior) = prior {
                        let _ = install_record(core, prior, epoch, &cancel);
                    }
                } else {
                    drop(prior);
                }
                if !same {
                    directory.clear_reusable(Some(&metadata.data_key))?;
                    let mut state = core.state.lock().unwrap_or_else(|error| error.into_inner());
                    Core::remove_reusable(&mut state, Some(&metadata.data_key));
                    core.refresh(&state);
                }
                if !registration_current(core, &object, epoch, generation, &cancel) {
                    return Ok(false);
                }
                let files = {
                    let mut state = core.state.lock().unwrap_or_else(|error| error.into_inner());
                    state.objects.get_mut(&object).unwrap().reuse = Some(metadata.clone());
                    state
                        .blocks
                        .iter()
                        .filter(|((name, _), _)| name == &object)
                        .map(|((_, offset), block)| (*offset, block.file.clone()))
                        .collect::<Vec<_>>()
                };
                let mut admitted = true;
                for (offset, file) in files {
                    admitted &= persist_block(
                        core,
                        directory,
                        &object,
                        offset,
                        metadata.clone(),
                        file,
                        &cancel,
                    )?;
                }
                let _ = reclaim(core, directory, false)?;
                if !registration_current(core, &object, epoch, generation, &cancel) {
                    return Ok(false);
                }
                Ok(admitted)
            })();
            update_account(
                core,
                directory,
                outcome.as_ref().err().map(|_| CacheIoErrorCode::Publish),
            );
            let _ = reply.send(outcome.map_err(|error| format!("{error:#}")));
        }
        reusable::Job::Invalidate {
            data_key,
            expected,
            epoch,
            cancel,
            reply,
        } => {
            let outcome = (|| -> Result<bool> {
                {
                    let state = core.state.lock().unwrap_or_else(|error| error.into_inner());
                    if state.closing || state.epoch != epoch || cancel.load(Ordering::Acquire) {
                        return Ok(false);
                    }
                }
                if let Some(expected) = &expected {
                    if !directory.clear_reusable_if(expected)? {
                        return Ok(false);
                    }
                } else {
                    directory.clear_reusable(Some(&data_key))?;
                }
                let mut state = core.state.lock().unwrap_or_else(|error| error.into_inner());
                if expected.as_ref().is_none_or(|expected| {
                    state
                        .reusable
                        .get(&data_key)
                        .is_some_and(|record| record.metadata == *expected)
                }) {
                    Core::remove_reusable(&mut state, Some(&data_key));
                }
                for object in state.objects.values_mut() {
                    if object
                        .reuse
                        .as_ref()
                        .is_some_and(|metadata| metadata.data_key == data_key)
                    {
                        object.reuse = None;
                    }
                }
                core.refresh(&state);
                Ok(true)
            })();
            update_account(
                core,
                directory,
                outcome.as_ref().err().map(|_| CacheIoErrorCode::Audit),
            );
            let _ = reply.send(outcome.map_err(|error| format!("{error:#}")));
        }
    }
}

fn clear(core: &Core, directory: &Directory) -> Result<CacheStats> {
    let mut failure = directory.clear_reusable(None).err();
    {
        let mut state = core.state.lock().unwrap_or_else(|error| error.into_inner());
        state.account = directory.account();
        if failure.is_some() {
            state.error_code = CacheIoErrorCode::Audit;
        } else {
            state.reuse_fenced = false;
        }
        core.refresh(&state);
    }
    let mut retry_failed = true;
    loop {
        if let Err(error) = reclaim(core, directory, retry_failed)
            && failure.is_none()
        {
            failure = Some(error);
        }
        retry_failed = false;
        let mut state = core.state.lock().unwrap_or_else(|error| error.into_inner());
        if state.closing {
            bail!("cache clear barrier cancelled");
        }
        // Include blocks retired by lease or pin release while filesystem work was in flight.
        if state.retired.values().any(|item| !item.failed) {
            state = core
                .changed
                .wait_timeout(state, Duration::from_millis(10))
                .unwrap_or_else(|error| error.into_inner())
                .0;
            drop(state);
            continue;
        }
        drop(state);
        let audit = directory.refresh_and_recover();
        let mut state = core.state.lock().unwrap_or_else(|error| error.into_inner());
        match audit {
            Ok(account) => state.account = account,
            Err(error) => {
                state.account.unmeasurable = true;
                state.account.foreign_files = state.account.foreign_files.max(1);
                state.error_code = CacheIoErrorCode::Audit;
                if failure.is_none() {
                    failure = Some(error);
                }
            }
        }
        if state.closing {
            bail!("cache clear barrier cancelled");
        }
        if state.retired.values().any(|item| !item.failed) {
            core.refresh(&state);
            drop(state);
            continue;
        }
        if failure.is_none() && state.retired.is_empty() {
            state.error_code = if state.account.foreign_files != 0 || state.account.unmeasurable {
                CacheIoErrorCode::Audit
            } else {
                CacheIoErrorCode::None
            };
        }
        core.refresh(&state);
        if let Some(error) = failure {
            bail!("cannotClear: {error:#}");
        }
        let stats = core.stats();
        if stats.cannot_clear {
            bail!("cannotClear: foreign or unverifiable cache storage was preserved");
        }
        return Ok(stats);
    }
}

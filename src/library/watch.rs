//! Bounded native change notifications. Names are hints, never access grants.
use super::authority::AuthorityRegistry;
use crate::audio::worker_owner::{TrackedWorker, WorkerKind, WorkerPermit};
use crate::domain::library::{FileIdentity, LibraryError, LibraryErrorKind, LibraryRoot};
use crate::domain::library_watch::LibraryWatchToken;
use anyhow::{Result, ensure};
use std::collections::{BTreeMap, VecDeque};
use std::path::{Component, Path, PathBuf};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    mpsc::{self, Receiver, SyncSender, TrySendError},
};
use std::time::Duration;

#[cfg(windows)]
#[path = "watch_windows.rs"]
mod platform;
#[cfg(target_os = "linux")]
#[path = "watch_linux.rs"]
mod platform;
#[cfg(target_os = "macos")]
#[path = "watch_macos.rs"]
mod platform;

const MAX_CONTEXTS: usize = 128;
const MAX_HINTS: usize = 200;
const MAX_HINT_BYTES: usize = 64 * 1024;
const CONTROL_CAPACITY: usize = 32;
const EVENT_CAPACITY: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NativeWatchKind {
    Created,
    Modified,
    Removed,
    RenamedOld,
    RenamedNew,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NativeWatchHint {
    pub path: PathBuf,
    pub kind: NativeWatchKind,
    pub is_directory: Option<bool>,
    pub rename_cookie: Option<u32>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NativeDirtySnapshot {
    pub root_id: String,
    pub grant_epoch: u64,
    pub watch_generation: u64,
    pub received_sequence: u64,
    pub covered_sequence: u64,
    pub full_rescan_required: bool,
    pub dropped_events: u64,
    pub pending_paths: usize,
    pub error: Option<LibraryError>,
    pub root_moved_path: Option<PathBuf>,
}
#[derive(Debug)]
pub enum NativeWatchEvent {
    Prepared {
        token: LibraryWatchToken,
        result: Result<()>,
    },
    Watching {
        token: LibraryWatchToken,
    },
    Stopped {
        root_id: String,
        watch_generation: u64,
    },
    Dirty {
        token: LibraryWatchToken,
        sequence: u64,
        hints: Vec<NativeWatchHint>,
        full_rescan_required: bool,
    },
    RootMoved {
        token: LibraryWatchToken,
        current_path: PathBuf,
        anchor: FileIdentity,
    },
    Error {
        token: LibraryWatchToken,
        error: LibraryError,
    },
    OwnerError {
        error: LibraryError,
    },
}

/// Admission reserves ownership before the filesystem owner receives a request.
pub struct NativeWatch {
    commands: SyncSender<Command>,
    events: Receiver<Envelope>,
    shared: Arc<Shared>,
    worker: TrackedWorker,
}
impl NativeWatch {
    pub fn new(authority: AuthorityRegistry) -> Result<Self> {
        let permit = WorkerPermit::reserve(WorkerKind::LibraryWatch)?;
        let shared = Arc::new(Shared::default());
        #[cfg(target_os = "linux")]
        shared.wake_fd.store(-1, Ordering::Release);
        let (commands, receive) = mpsc::sync_channel(CONTROL_CAPACITY);
        let (send, events) = mpsc::sync_channel(EVENT_CAPACITY);
        let owned = shared.clone();
        let thread = std::thread::Builder::new()
            .name("lx-library-watch".into())
            .spawn(move || {
                let mut output = Output::new(send, owned.clone());
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    platform::run(authority, receive, owned.clone(), &mut output)
                }));
                if let Ok(Err(error)) = &result {
                    output.control(
                        NativeWatchEvent::OwnerError {
                            error: watch_error(error),
                        },
                        None,
                    );
                }
                if let Err(payload) = result {
                    output.control(
                        NativeWatchEvent::OwnerError {
                            error: LibraryError::new(
                                LibraryErrorKind::Unavailable,
                                "Native watch owner panicked",
                            ),
                        },
                        None,
                    );
                    owned.finished.store(true, Ordering::Release);
                    std::panic::resume_unwind(payload);
                }
                owned.finished.store(true, Ordering::Release);
            })?;
        Ok(Self {
            commands,
            events,
            shared,
            worker: permit.attach(thread),
        })
    }
    pub fn prepare(
        &self,
        root: LibraryRoot,
        token: LibraryWatchToken,
        start_sequence: u64,
    ) -> Result<()> {
        root.validate()?;
        token.validate()?;
        ensure!(
            token.enabled && token.root_id == root.root_id && token.grant_epoch == root.grant_epoch,
            LibraryError::invalid("Watch token does not match its root grant")
        );
        ensure!(
            start_sequence <= i64::MAX as u64,
            LibraryError::invalid("Invalid watch sequence")
        );
        self.check_open()?;
        let credit = Credit::reserve(self.shared.control_count.clone(), CONTROL_CAPACITY)?;
        let slot = Credit::reserve(self.shared.context_count.clone(), MAX_CONTEXTS)?;
        let dirty = Arc::new(DirtyState::new(token.clone(), start_sequence));
        let key = (token.root_id.clone(), token.watch_generation);
        {
            let mut states = self.shared.dirty.lock().unwrap_or_else(|e| e.into_inner());
            ensure!(
                !states.contains_key(&key),
                LibraryError::conflict("Watch generation already admitted")
            );
            ensure!(
                states.len() < MAX_CONTEXTS,
                LibraryError::budget("Watch dirty-state budget exceeded")
            );
            states.insert(key.clone(), dirty.clone());
        }
        let command = Command::Prepare {
            root,
            token,
            dirty,
            slot,
            credit,
        };
        if self.commands.try_send(command).is_err() {
            self.shared
                .dirty
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&key);
            return Err(
                LibraryError::new(LibraryErrorKind::Busy, "Watch request queue is full").into(),
            );
        }
        self.wake();
        Ok(())
    }
    pub fn activate(&self, token: LibraryWatchToken) -> Result<()> {
        token.validate()?;
        self.enqueue(|credit| Command::Activate { token, credit })
    }
    pub fn stop(&self, root_id: &str, watch_generation: u64) -> Result<()> {
        crate::domain::library::validate_library_id(root_id)?;
        ensure!(
            watch_generation <= i64::MAX as u64,
            LibraryError::invalid("Invalid watch generation")
        );
        self.enqueue(|credit| Command::Stop {
            root_id: root_id.into(),
            generation: watch_generation,
            credit,
        })
    }
    pub fn try_recv(&mut self) -> Option<NativeWatchEvent> {
        self.events.try_recv().ok().map(|envelope| envelope.event)
    }
    pub fn dirty(&self, root_id: &str, watch_generation: u64) -> Option<NativeDirtySnapshot> {
        self.shared
            .dirty
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&(root_id.into(), watch_generation))
            .map(|state| state.snapshot())
    }
    pub fn is_finished(&self) -> bool {
        self.shared.finished.load(Ordering::Acquire) || self.worker.is_finished()
    }
    pub fn cover(&self, token: &LibraryWatchToken, sequence: u64, full_scan: bool) -> Result<()> {
        token.validate()?;
        let state = self
            .shared
            .dirty
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&(token.root_id.clone(), token.watch_generation))
            .cloned()
            .ok_or_else(|| LibraryError::conflict("Watch generation is no longer owned"))?;
        ensure!(
            state.token == *token,
            LibraryError::conflict("Watch grant changed")
        );
        state.cover(sequence, full_scan)
    }
    pub fn shutdown(&mut self, timeout: Duration) -> Result<bool> {
        self.shared.closing.store(true, Ordering::Release);
        self.wake();
        if !self.worker.wait(timeout) {
            return Ok(false);
        }
        ensure!(!self.worker.panicked(), "Native watch owner panicked");
        ensure!(
            self.shared.context_count.load(Ordering::Acquire) == 0,
            "Native watch contexts remain owned"
        );
        #[cfg(windows)]
        ensure!(
            self.shared.kernel_count.load(Ordering::Acquire) == 0,
            "Native watch sensors remain owned"
        );
        self.shared
            .dirty
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        Ok(true)
    }
    fn check_open(&self) -> Result<()> {
        ensure!(
            !self.shared.closing.load(Ordering::Acquire)
                && !self.shared.finished.load(Ordering::Acquire),
            LibraryError::new(LibraryErrorKind::Unavailable, "Watch owner is closing")
        );
        Ok(())
    }
    fn enqueue(&self, make: impl FnOnce(Credit) -> Command) -> Result<()> {
        self.check_open()?;
        let credit = Credit::reserve(self.shared.control_count.clone(), CONTROL_CAPACITY)?;
        self.commands.try_send(make(credit)).map_err(|_| {
            LibraryError::new(LibraryErrorKind::Busy, "Watch request queue is full")
        })?;
        self.wake();
        Ok(())
    }
    fn wake(&self) {
        self.worker.unpark();
        #[cfg(target_os = "linux")]
        {
            let _guard = self
                .shared
                .wake_lock
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let descriptor = self.shared.wake_fd.load(Ordering::Acquire);
            if descriptor >= 0 {
                let value = 1u64;
                unsafe {
                    libc::write(descriptor, &value as *const _ as *const _, 8);
                }
            }
        }
    }
}
impl Drop for NativeWatch {
    fn drop(&mut self) {
        self.shared.closing.store(true, Ordering::Release);
        self.wake();
    }
}

#[derive(Default)]
struct Shared {
    closing: AtomicBool,
    finished: AtomicBool,
    context_count: Arc<AtomicUsize>,
    #[cfg(windows)]
    kernel_count: Arc<AtomicUsize>,
    control_count: Arc<AtomicUsize>,
    dirty: Mutex<BTreeMap<(String, u64), Arc<DirtyState>>>,
    #[cfg(test)]
    gate: Mutex<Option<Arc<CompletionGate>>>,
    #[cfg(all(test, windows))]
    native_gaps: AtomicUsize,
    #[cfg(test)]
    cancelled_completions: AtomicUsize,
    #[cfg(test)]
    control_peak: AtomicUsize,
    #[cfg(target_os = "linux")]
    wake_fd: std::sync::atomic::AtomicI32,
    #[cfg(target_os = "linux")]
    wake_lock: Mutex<()>,
}
#[cfg(test)]
#[derive(Default)]
struct CompletionGate {
    entered: AtomicBool,
    release: AtomicBool,
    cancel_only: AtomicBool,
}
struct Credit(Arc<AtomicUsize>);
impl Credit {
    fn reserve(count: Arc<AtomicUsize>, capacity: usize) -> Result<Self> {
        count
            .try_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                (n < capacity).then_some(n + 1)
            })
            .map_err(|_| LibraryError::budget("Native watch ownership budget exceeded"))?;
        Ok(Self(count))
    }
}
impl Drop for Credit {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}
enum Command {
    Prepare {
        root: LibraryRoot,
        token: LibraryWatchToken,
        dirty: Arc<DirtyState>,
        slot: Credit,
        credit: Credit,
    },
    Activate {
        token: LibraryWatchToken,
        credit: Credit,
    },
    Stop {
        root_id: String,
        generation: u64,
        credit: Credit,
    },
}
struct DirtyState {
    token: LibraryWatchToken,
    received: AtomicU64,
    covered: AtomicU64,
    // Encodes sequence + 1; zero means that no full scan is required.
    full_at: AtomicU64,
    dropped: AtomicU64,
    paths: Mutex<VecDeque<(u64, PathBuf)>>,
    error: Mutex<Option<LibraryError>>,
    root_moved_path: Mutex<Option<PathBuf>>,
}
impl DirtyState {
    fn new(token: LibraryWatchToken, sequence: u64) -> Self {
        Self {
            token,
            received: AtomicU64::new(sequence),
            covered: AtomicU64::new(sequence),
            full_at: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
            paths: Mutex::new(VecDeque::new()),
            error: Mutex::new(None),
            root_moved_path: Mutex::new(None),
        }
    }
    fn snapshot(&self) -> NativeDirtySnapshot {
        NativeDirtySnapshot {
            root_id: self.token.root_id.clone(),
            grant_epoch: self.token.grant_epoch,
            watch_generation: self.token.watch_generation,
            received_sequence: self.received.load(Ordering::Acquire),
            covered_sequence: self.covered.load(Ordering::Acquire),
            full_rescan_required: self.full_at.load(Ordering::Acquire) != 0,
            dropped_events: self.dropped.load(Ordering::Acquire),
            pending_paths: self.paths.lock().unwrap_or_else(|e| e.into_inner()).len(),
            error: self.error.lock().unwrap_or_else(|e| e.into_inner()).clone(),
            root_moved_path: self
                .root_moved_path
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone(),
        }
    }
    fn mark(&self, hints: &[NativeWatchHint], full: bool) -> u64 {
        let old = self
            .received
            .try_update(Ordering::AcqRel, Ordering::Acquire, |s| {
                (s < i64::MAX as u64).then_some(s + 1)
            })
            .unwrap_or(i64::MAX as u64);
        let sequence = old.saturating_add(1).min(i64::MAX as u64);
        if old == i64::MAX as u64 {
            self.fail(&LibraryError::budget("Native watch sequence exhausted"));
        }
        let mut require_full = full || old == i64::MAX as u64;
        let mut omitted = false;
        if let Ok(mut paths) = self.paths.try_lock() {
            let mut bytes: usize = paths.iter().map(|(_, p)| p.as_os_str().len() * 4).sum();
            for hint in hints {
                let cost = hint.path.as_os_str().len() * 4;
                if paths.len() >= MAX_HINTS || bytes + cost > MAX_HINT_BYTES {
                    require_full = true;
                    omitted = true;
                    break;
                }
                paths.push_back((sequence, hint.path.clone()));
                bytes += cost;
            }
        } else {
            require_full = true;
            omitted = true;
        }
        if omitted {
            self.dropped.fetch_add(1, Ordering::AcqRel);
        }
        if require_full {
            let marker = if old == i64::MAX as u64 {
                u64::MAX
            } else {
                sequence + 1
            };
            self.full_at.fetch_max(marker, Ordering::AcqRel);
        }
        sequence
    }
    fn baseline(&self) {
        self.full_at
            .fetch_max(self.received.load(Ordering::Acquire) + 1, Ordering::AcqRel);
    }
    fn overflow(&self, sequence: u64) {
        self.dropped.fetch_add(1, Ordering::AcqRel);
        self.full_at.fetch_max(sequence + 1, Ordering::AcqRel);
    }
    fn fail(&self, error: &LibraryError) {
        let mut bounded = error.clone();
        bounded.message = bounded.message.chars().take(1024).collect();
        *self.error.lock().unwrap_or_else(|e| e.into_inner()) = Some(bounded);
        self.full_at
            .fetch_max(self.received.load(Ordering::Acquire) + 1, Ordering::AcqRel);
    }
    fn root_moved(&self, path: &Path) {
        if path
            .to_str()
            .is_some_and(|text| text.len() <= 16 * 1024 && !text.contains('\0'))
        {
            *self
                .root_moved_path
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = Some(path.to_path_buf());
            self.full_at
                .fetch_max(self.received.load(Ordering::Acquire) + 1, Ordering::AcqRel);
        } else {
            *self
                .root_moved_path
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = None;
            self.fail(&LibraryError::budget(
                "Native root locator exceeds its bounded representation",
            ));
        }
    }
    fn cover(&self, sequence: u64, full_scan: bool) -> Result<()> {
        ensure!(
            sequence <= self.received.load(Ordering::Acquire),
            LibraryError::invalid("Cannot cover an unreceived sequence")
        );
        self.covered.fetch_max(sequence, Ordering::AcqRel);
        self.paths
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|(s, _)| *s > sequence);
        if full_scan {
            let mut marker = self.full_at.load(Ordering::Acquire);
            while marker != 0 && marker <= sequence + 1 {
                match self.full_at.compare_exchange_weak(
                    marker,
                    0,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                ) {
                    Ok(_) => break,
                    Err(now) => marker = now,
                }
            }
        }
        Ok(())
    }
}
struct Envelope {
    event: NativeWatchEvent,
    _credit: Option<Credit>,
}
struct Output {
    sender: SyncSender<Envelope>,
    waiting: VecDeque<Envelope>,
    shared: Arc<Shared>,
}
#[derive(Clone, Copy, Debug)]
enum NativeDirtyReason {
    Native,
    #[cfg(windows)]
    SharedCancelGap,
    #[cfg(windows)]
    RootMoved,
    #[cfg(windows)]
    SensorError,
}
impl Output {
    fn new(sender: SyncSender<Envelope>, shared: Arc<Shared>) -> Self {
        Self {
            sender,
            waiting: VecDeque::new(),
            shared,
        }
    }
    fn control(&mut self, event: NativeWatchEvent, credit: Option<Credit>) {
        match &event {
            NativeWatchEvent::RootMoved {
                token,
                current_path,
                ..
            } => {
                if let Some(state) = self
                    .shared
                    .dirty
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .get(&(token.root_id.clone(), token.watch_generation))
                {
                    state.root_moved(current_path);
                }
            }
            NativeWatchEvent::Error { token, error } => {
                if let Some(state) = self
                    .shared
                    .dirty
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .get(&(token.root_id.clone(), token.watch_generation))
                {
                    state.fail(error);
                }
            }
            NativeWatchEvent::OwnerError { error } => {
                for state in self
                    .shared
                    .dirty
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .values()
                {
                    state.fail(error);
                }
            }
            _ => {}
        }
        if self.shared.closing.load(Ordering::Acquire)
            && !matches!(event, NativeWatchEvent::OwnerError { .. })
        {
            return;
        }
        // A command owns one of 32 credits through caller consumption. Native
        // diagnostics share at most 32 further slots and coalesce by generation.
        if credit.is_none()
            && let Some(previous) = self.waiting.iter_mut().find(|envelope| {
                envelope._credit.is_none() && same_diagnostic(&envelope.event, &event)
            })
        {
            previous.event = event;
            return;
        }
        if credit.is_none() && self.waiting.len() >= CONTROL_CAPACITY {
            self.diagnostic_lost(&event);
            return;
        }
        let envelope = Envelope {
            event,
            _credit: credit,
        };
        if self.waiting.is_empty() {
            match self.sender.try_send(envelope) {
                Ok(()) => return,
                Err(TrySendError::Full(envelope)) => self.waiting.push_back(envelope),
                Err(TrySendError::Disconnected(_)) => {
                    self.shared.closing.store(true, Ordering::Release)
                }
            }
        } else {
            self.waiting.push_back(envelope);
        }
        debug_assert!(self.waiting.len() <= 2 * CONTROL_CAPACITY);
        #[cfg(test)]
        self.shared
            .control_peak
            .fetch_max(self.waiting.len(), Ordering::AcqRel);
    }
    fn diagnostic_lost(&self, event: &NativeWatchEvent) {
        let states = self.shared.dirty.lock().unwrap_or_else(|e| e.into_inner());
        match event {
            NativeWatchEvent::Error { token, .. } | NativeWatchEvent::RootMoved { token, .. } => {
                if let Some(state) = states.get(&(token.root_id.clone(), token.watch_generation)) {
                    state.overflow(state.received.load(Ordering::Acquire));
                    if matches!(event, NativeWatchEvent::RootMoved { .. }) {
                        state.fail(&LibraryError::new(
                            LibraryErrorKind::Unavailable,
                            "Native watch root moved",
                        ));
                    }
                }
            }
            NativeWatchEvent::OwnerError { .. } => {
                for state in states.values() {
                    state.overflow(state.received.load(Ordering::Acquire));
                }
            }
            _ => {}
        }
    }
    fn flush(&mut self) {
        while let Some(envelope) = self.waiting.pop_front() {
            match self.sender.try_send(envelope) {
                Ok(()) => {}
                Err(TrySendError::Full(envelope)) => {
                    self.waiting.push_front(envelope);
                    break;
                }
                Err(TrySendError::Disconnected(_)) => {
                    self.shared.closing.store(true, Ordering::Release);
                    break;
                }
            }
        }
    }
    fn dirty(&mut self, state: &DirtyState, hints: Vec<NativeWatchHint>, full: bool, active: bool) {
        self.dirty_cause(state, hints, full, active, NativeDirtyReason::Native);
    }
    fn dirty_cause(
        &mut self,
        state: &DirtyState,
        hints: Vec<NativeWatchHint>,
        full: bool,
        active: bool,
        _reason: NativeDirtyReason,
    ) {
        let sequence = state.mark(&hints, full);
        if !active {
            return;
        }
        let event = NativeWatchEvent::Dirty {
            token: state.token.clone(),
            sequence,
            hints,
            full_rescan_required: state.full_at.load(Ordering::Acquire) != 0,
        };
        if self
            .sender
            .try_send(Envelope {
                event,
                _credit: None,
            })
            .is_err()
        {
            state.overflow(sequence);
        }
    }
    fn baseline(&mut self, state: &DirtyState) {
        state.baseline();
        let sequence = state.received.load(Ordering::Acquire);
        let event = NativeWatchEvent::Dirty {
            token: state.token.clone(),
            sequence,
            hints: Vec::new(),
            full_rescan_required: true,
        };
        if self
            .sender
            .try_send(Envelope {
                event,
                _credit: None,
            })
            .is_err()
        {
            state.overflow(sequence);
        }
    }
    fn forget(&self, root_id: &str, generation: u64) {
        self.shared
            .dirty
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&(root_id.into(), generation));
    }
}
fn same_diagnostic(left: &NativeWatchEvent, right: &NativeWatchEvent) -> bool {
    match (left, right) {
        (NativeWatchEvent::Error { token: a, .. }, NativeWatchEvent::Error { token: b, .. })
        | (
            NativeWatchEvent::RootMoved { token: a, .. },
            NativeWatchEvent::RootMoved { token: b, .. },
        ) => a == b,
        (NativeWatchEvent::OwnerError { .. }, NativeWatchEvent::OwnerError { .. }) => true,
        _ => false,
    }
}
fn safe_relative(path: &Path) -> bool {
    !path.as_os_str().is_empty()
        && path.as_os_str().len() <= 16 * 1024
        && path.components().all(|c| matches!(c, Component::Normal(_)))
        && path
            .to_str()
            .is_some_and(|p| !p.contains([':', '\0']) && !p.starts_with(['/', '\\']))
}
fn watch_error(error: &anyhow::Error) -> LibraryError {
    error
        .downcast_ref::<LibraryError>()
        .cloned()
        .unwrap_or_else(|| {
            LibraryError::new(
                LibraryErrorKind::Unavailable,
                error.to_string().chars().take(1024).collect::<String>(),
            )
        })
}

#[cfg(all(test, any(windows, target_os = "linux")))]
mod tests {
    use super::*;
    use crate::library::authority::RootAuthority;
    use crate::library::scan::ScanContext;
    use std::time::Instant;

    struct ReleaseGate(Arc<CompletionGate>);
    impl Drop for ReleaseGate {
        fn drop(&mut self) {
            self.0.release.store(true, Ordering::Release);
        }
    }
    fn root(authority: &AuthorityRegistry, path: &Path) -> LibraryRoot {
        let opened =
            RootAuthority::open(path, true, &ScanContext::new(Duration::from_secs(30))).unwrap();
        let draft = opened.draft();
        let selected = LibraryRoot {
            root_id: "root:watch-unit".into(),
            path: draft.path,
            recursive: draft.recursive,
            grant_epoch: 1,
            anchor: draft.anchor,
            path_policy: draft.path_policy,
            watch_enabled: false,
            generation: 0,
            last_complete_generation: 0,
        };
        authority
            .accept_root_proof(&selected, opened.proof().unwrap())
            .unwrap();
        selected
    }
    fn token(root: &LibraryRoot) -> LibraryWatchToken {
        LibraryWatchToken {
            root_id: root.root_id.clone(),
            grant_epoch: root.grant_epoch,
            watch_generation: 1,
            enabled: true,
            options: Default::default(),
        }
    }
    fn event(watch: &mut NativeWatch) -> NativeWatchEvent {
        let start = Instant::now();
        loop {
            if let Some(event) = watch.try_recv() {
                return event;
            }
            assert!(
                start.elapsed() < Duration::from_secs(10),
                "native event did not arrive"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
    }
    fn watching(watch: &mut NativeWatch, selected: &LibraryRoot, token: &LibraryWatchToken) {
        watch.prepare(selected.clone(), token.clone(), 0).unwrap();
        match event(watch) {
            NativeWatchEvent::Prepared { result, .. } => result.unwrap(),
            other => panic!("{other:?}"),
        }
        watch.activate(token.clone()).unwrap();
        assert!(matches!(event(watch), NativeWatchEvent::Watching { .. }));
        assert!(matches!(
            event(watch),
            NativeWatchEvent::Dirty {
                full_rescan_required: true,
                ..
            }
        ));
        let snapshot = watch.dirty(&token.root_id, token.watch_generation).unwrap();
        watch
            .cover(token, snapshot.received_sequence, true)
            .unwrap();
    }

    #[test]
    fn covered_sequence_preserves_later_paths_and_full_marker() {
        let selected = LibraryWatchToken {
            root_id: "root:coverage".into(),
            grant_epoch: 1,
            watch_generation: 1,
            enabled: true,
            options: Default::default(),
        };
        let dirty = DirtyState::new(selected, 7);
        let hint = NativeWatchHint {
            path: "one.wav".into(),
            kind: NativeWatchKind::Created,
            is_directory: Some(false),
            rename_cookie: None,
        };
        assert_eq!(dirty.mark(std::slice::from_ref(&hint), false), 8);
        assert_eq!(
            dirty.mark(
                &[NativeWatchHint {
                    path: "two.wav".into(),
                    ..hint
                }],
                true
            ),
            9
        );
        dirty.cover(8, true).unwrap();
        let snapshot = dirty.snapshot();
        assert_eq!(snapshot.received_sequence, 9);
        assert_eq!(snapshot.covered_sequence, 8);
        assert_eq!(snapshot.pending_paths, 1);
        assert!(snapshot.full_rescan_required);
        dirty.cover(9, false).unwrap();
        assert!(dirty.snapshot().full_rescan_required);
        dirty.cover(9, true).unwrap();
        assert!(!dirty.snapshot().full_rescan_required);
        dirty.fail(&LibraryError::new(
            LibraryErrorKind::Unavailable,
            "Native subscription ended",
        ));
        dirty.cover(9, true).unwrap();
        assert!(dirty.snapshot().error.is_some());
    }
    #[test]
    fn zero_sequence_baseline_is_distinct_from_clean() {
        let selected = LibraryWatchToken {
            root_id: "root:zero".into(),
            grant_epoch: 1,
            watch_generation: 1,
            enabled: true,
            options: Default::default(),
        };
        let dirty = DirtyState::new(selected, 0);
        dirty.baseline();
        assert!(dirty.snapshot().full_rescan_required);
        assert_eq!(dirty.snapshot().received_sequence, 0);
        dirty.mark(&[], true);
        dirty.cover(0, true).unwrap();
        assert!(dirty.snapshot().full_rescan_required);
        assert_eq!(dirty.snapshot().received_sequence, 1);
    }
    #[test]
    fn native_cancellation_completion_gate_retains_context_until_actual_owner_join() {
        let temp = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
        let authority = AuthorityRegistry::default();
        let selected = root(&authority, temp.path());
        let token = token(&selected);
        let mut watch = NativeWatch::new(authority.clone()).unwrap();
        watching(&mut watch, &selected, &token);
        let gate = Arc::new(CompletionGate::default());
        gate.cancel_only.store(true, Ordering::Release);
        let release = ReleaseGate(gate.clone());
        *watch.shared.gate.lock().unwrap() = Some(gate.clone());
        watch.stop(&token.root_id, token.watch_generation).unwrap();
        let start = Instant::now();
        while !gate.entered.load(Ordering::Acquire) {
            assert!(
                start.elapsed() < Duration::from_secs(5),
                "native cancellation completion was not received"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(watch.shared.context_count.load(Ordering::Acquire), 1);
        assert_eq!(
            watch.shared.cancelled_completions.load(Ordering::Acquire),
            1
        );
        assert!(!watch.worker.wait(Duration::ZERO));
        assert!(!watch.shutdown(Duration::from_millis(10)).unwrap());
        assert_eq!(watch.shared.context_count.load(Ordering::Acquire), 1);
        drop(release);
        assert!(watch.shutdown(Duration::from_secs(5)).unwrap());
        assert_eq!(watch.shared.context_count.load(Ordering::Acquire), 0);
        authority.begin_shutdown();
        assert!(authority.retirement_barrier().wait(Duration::from_secs(5)));
    }
    #[test]
    fn actual_grant_retirement_storm_bounds_diagnostics_and_keeps_dirty_fallback() {
        let temp = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
        let authority = AuthorityRegistry::default();
        let selected = root(&authority, temp.path());
        let mut watch = NativeWatch::new(authority.clone()).unwrap();
        for generation in 1..=MAX_CONTEXTS as u64 {
            let mut request = token(&selected);
            request.watch_generation = generation;
            watch.prepare(selected.clone(), request, 0).unwrap();
            match event(&mut watch) {
                NativeWatchEvent::Prepared { result, .. } => result.unwrap(),
                other => panic!("{other:?}"),
            }
        }
        #[cfg(windows)]
        assert_eq!(watch.shared.kernel_count.load(Ordering::Acquire), 1);
        authority.revoke_root(&selected.root_id);
        let began = Instant::now();
        while watch.shared.context_count.load(Ordering::Acquire) != 0 {
            assert!(
                began.elapsed() < Duration::from_secs(10),
                "Native retirement did not consume cancellation"
            );
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(watch.shared.control_peak.load(Ordering::Acquire) <= 2 * CONTROL_CAPACITY);
        let mut lost = 0;
        for generation in 1..=MAX_CONTEXTS as u64 {
            let dirty = watch.dirty(&selected.root_id, generation).unwrap();
            assert!(dirty.full_rescan_required);
            assert!(dirty.error.is_some());
            lost += dirty.dropped_events;
        }
        assert!(
            lost > 0,
            "Actual diagnostic storm did not reach the bounded fallback"
        );
        assert!(watch.shared.dirty.lock().unwrap().len() <= MAX_CONTEXTS);
        assert!(watch.shutdown(Duration::from_secs(5)).unwrap());
        authority.begin_shutdown();
        assert!(authority.retirement_barrier().wait(Duration::from_secs(5)));
    }
    #[cfg(windows)]
    #[test]
    fn stopping_one_shared_sensor_subscriber_consumes_cancel_before_rearming_the_other() {
        let temp = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
        let authority = AuthorityRegistry::default();
        let selected = root(&authority, temp.path());
        let first = token(&selected);
        let mut second = first.clone();
        second.watch_generation = 2;
        let mut watch = NativeWatch::new(authority.clone()).unwrap();
        watching(&mut watch, &selected, &first);
        watch.prepare(selected.clone(), second.clone(), 0).unwrap();
        match event(&mut watch) {
            NativeWatchEvent::Prepared { result, .. } => result.unwrap(),
            other => panic!("{other:?}"),
        }
        watch.activate(second.clone()).unwrap();
        while !matches!(event(&mut watch), NativeWatchEvent::Dirty { .. }) {}
        let gate = Arc::new(CompletionGate::default());
        gate.cancel_only.store(true, Ordering::Release);
        let release = ReleaseGate(gate.clone());
        *watch.shared.gate.lock().unwrap() = Some(gate.clone());
        watch
            .stop(&second.root_id, second.watch_generation)
            .unwrap();
        let began = Instant::now();
        while !gate.entered.load(Ordering::Acquire) {
            assert!(began.elapsed() < Duration::from_secs(5));
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(watch.shared.context_count.load(Ordering::Acquire), 2);
        assert_eq!(watch.shared.kernel_count.load(Ordering::Acquire), 1);
        assert!(!watch.worker.wait(Duration::ZERO));
        std::fs::write(temp.path().join("during-cancel.wav"), b"actual change").unwrap();
        *watch.shared.gate.lock().unwrap() = None;
        drop(release);
        while !matches!(
            event(&mut watch),
            NativeWatchEvent::Stopped {
                watch_generation: 2,
                ..
            }
        ) {}
        assert_eq!(watch.shared.context_count.load(Ordering::Acquire), 1);
        assert_eq!(watch.shared.kernel_count.load(Ordering::Acquire), 1);
        assert!(watch.dirty(&first.root_id, 1).unwrap().full_rescan_required);
        let before = watch.dirty(&first.root_id, 1).unwrap().received_sequence;
        std::fs::write(temp.path().join("after-rearm.wav"), b"actual change").unwrap();
        let began = Instant::now();
        while watch.dirty(&first.root_id, 1).unwrap().received_sequence == before {
            assert!(began.elapsed() < Duration::from_secs(5));
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(watch.shutdown(Duration::from_secs(5)).unwrap());
        assert_eq!(watch.shared.kernel_count.load(Ordering::Acquire), 0);
        authority.begin_shutdown();
        assert!(authority.retirement_barrier().wait(Duration::from_secs(5)));
    }
    #[cfg(windows)]
    #[test]
    fn actual_namespace_storm_during_dequeue_rearm_gap_requires_full_rescan() {
        let temp = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
        let authority = AuthorityRegistry::default();
        let selected = root(&authority, temp.path());
        let token = token(&selected);
        let mut watch = NativeWatch::new(authority.clone()).unwrap();
        watching(&mut watch, &selected, &token);
        let gate = Arc::new(CompletionGate::default());
        let release = ReleaseGate(gate.clone());
        *watch.shared.gate.lock().unwrap() = Some(gate.clone());
        std::fs::write(temp.path().join("first.wav"), b"event").unwrap();
        let start = Instant::now();
        while !gate.entered.load(Ordering::Acquire) {
            assert!(start.elapsed() < Duration::from_secs(5));
            std::thread::sleep(Duration::from_millis(1));
        }
        for index in 0..6000 {
            std::fs::write(
                temp.path().join(format!(
                    "{index:05}_abcdefghijklmnopqrstuvwxyz_abcdefghijklmnopqrstuvwxyz_gap.wav"
                )),
                b"storm",
            )
            .unwrap();
        }
        *watch.shared.gate.lock().unwrap() = None;
        drop(release);
        let start = Instant::now();
        while watch.shared.native_gaps.load(Ordering::Acquire) == 0 {
            while watch.try_recv().is_some() {}
            assert!(
                start.elapsed() < Duration::from_secs(10),
                "The actual native notification buffer did not report the gap"
            );
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(
            watch
                .dirty(&token.root_id, token.watch_generation)
                .unwrap()
                .full_rescan_required
        );
        assert!(watch.shutdown(Duration::from_secs(5)).unwrap());
        authority.begin_shutdown();
        assert!(authority.retirement_barrier().wait(Duration::from_secs(5)));
    }
}

#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
mod platform {
    use super::*;
    pub(super) fn run(
        _: AuthorityRegistry,
        _: Receiver<Command>,
        _: Arc<Shared>,
        _: &mut Output,
    ) -> Result<()> {
        anyhow::bail!("UnsupportedCapability: native watch requires Windows or Linux")
    }
}

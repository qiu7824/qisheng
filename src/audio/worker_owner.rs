//! Bounded ownership for workers that may outlive a caller's shutdown deadline.

use anyhow::{Result, bail};
use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::{self, JoinHandle, Thread};
use std::time::{Duration, Instant};

const CAPACITY: usize = 256;
const REAP_INTERVAL: Duration = Duration::from_millis(10);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum WorkerKind {
    Engine,
    CacheIo,
    Decoder,
    TransportRuntime,
    Artwork,
    Library,
    LibraryWatch,
    LibraryRetirement,
    Download,
    ExternalControl,
    Sync,
    DeviceOwner,
    #[cfg(test)]
    Test,
}

struct State {
    kind: WorkerKind,
    handle: Mutex<Option<JoinHandle<()>>>,
    wake: OnceLock<Thread>,
    attached: AtomicBool,
    abandoned: AtomicBool,
    observed_finished: AtomicBool,
    joined: AtomicBool,
    panicked: AtomicBool,
}

struct Supervisor {
    registry: Arc<Mutex<Vec<Arc<State>>>>,
    // A process-lifetime service owns the reaper itself, including its handle.
    thread: JoinHandle<()>,
}

static SUPERVISOR: OnceLock<std::result::Result<Supervisor, String>> = OnceLock::new();

fn supervisor() -> Result<&'static Supervisor> {
    let result = SUPERVISOR.get_or_init(|| {
        let registry = Arc::new(Mutex::new(Vec::<Arc<State>>::with_capacity(CAPACITY)));
        let workers = registry.clone();
        let thread = thread::Builder::new()
            .name("audio-worker-owner".into())
            .spawn(move || {
                loop {
                    let finished = {
                        let mut registry =
                            workers.lock().unwrap_or_else(|error| error.into_inner());
                        registry.retain(|state| {
                            !state.joined.load(Ordering::Acquire)
                                && !state.abandoned.load(Ordering::Acquire)
                        });
                        let mut finished = Vec::new();
                        for state in registry.iter() {
                            if !state.attached.load(Ordering::Acquire)
                                || state.observed_finished.load(Ordering::Acquire)
                            {
                                continue;
                            }
                            let mut handle = state
                                .handle
                                .lock()
                                .unwrap_or_else(|error| error.into_inner());
                            if handle.as_ref().is_some_and(JoinHandle::is_finished) {
                                state.observed_finished.store(true, Ordering::Release);
                                if let Some(handle) = handle.take() {
                                    finished.push((state.clone(), handle));
                                }
                            }
                        }
                        finished
                    };
                    // Actual joins and thread/TLS teardown cannot block registry queries.
                    for (state, handle) in finished {
                        state
                            .panicked
                            .store(handle.join().is_err(), Ordering::Release);
                        state.joined.store(true, Ordering::Release);
                    }
                    thread::park_timeout(REAP_INTERVAL);
                }
            })
            .map_err(|error| format!("Cannot start audio worker owner: {error}"))?;
        Ok(Supervisor { registry, thread })
    });
    match result {
        Ok(supervisor) => Ok(supervisor),
        Err(error) => bail!("{error}"),
    }
}

/// Reserve before spawning, so destruction can never require emergency admission.
pub(crate) struct WorkerPermit {
    state: Arc<State>,
    attached: bool,
}

impl WorkerPermit {
    /// A pre-spawn observer cannot report an exit until this exact permit has
    /// attached and its actual worker has been joined by the supervisor.
    pub(crate) fn tracker(&self) -> TrackedWorker {
        TrackedWorker {
            state: self.state.clone(),
        }
    }
    pub(crate) fn reserve(kind: WorkerKind) -> Result<Self> {
        let supervisor = supervisor()?;
        Self::reserve_in(&supervisor.registry, kind)
    }

    fn reserve_in(registry: &Mutex<Vec<Arc<State>>>, kind: WorkerKind) -> Result<Self> {
        let state = Arc::new(State {
            kind,
            handle: Mutex::new(None),
            wake: OnceLock::new(),
            attached: AtomicBool::new(false),
            abandoned: AtomicBool::new(false),
            observed_finished: AtomicBool::new(false),
            joined: AtomicBool::new(false),
            panicked: AtomicBool::new(false),
        });
        let mut registry = registry.lock().unwrap_or_else(|error| error.into_inner());
        registry.retain(|state| {
            !state.joined.load(Ordering::Acquire) && !state.abandoned.load(Ordering::Acquire)
        });
        if registry.len() >= CAPACITY {
            bail!("Audio worker ownership capacity is exhausted");
        }
        registry.push(state.clone());
        Ok(Self {
            state,
            attached: false,
        })
    }

    pub(crate) fn attach(mut self, handle: JoinHandle<()>) -> TrackedWorker {
        let _ = self.state.wake.set(handle.thread().clone());
        *self
            .state
            .handle
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(handle);
        self.state.attached.store(true, Ordering::Release);
        self.attached = true;
        if let Some(Ok(supervisor)) = SUPERVISOR.get() {
            supervisor.thread.thread().unpark();
        }
        TrackedWorker {
            state: self.state.clone(),
        }
    }
}

impl Drop for WorkerPermit {
    fn drop(&mut self) {
        if !self.attached {
            self.state.abandoned.store(true, Ordering::Release);
        }
    }
}

/// Dropping a caller's reference leaves the registered owner responsible for join.
#[derive(Clone)]
pub(crate) struct TrackedWorker {
    state: Arc<State>,
}

impl fmt::Debug for TrackedWorker {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TrackedWorker")
            .field("kind", &self.state.kind)
            .field("finished", &self.is_finished())
            .field("joined", &self.state.joined.load(Ordering::Acquire))
            .finish()
    }
}

impl TrackedWorker {
    pub(crate) fn same_worker(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.state, &other.state)
    }
    pub(crate) fn thread_id(&self) -> Option<std::thread::ThreadId> {
        self.state.wake.get().map(Thread::id)
    }
    pub(crate) fn is_finished(&self) -> bool {
        if self.state.observed_finished.load(Ordering::Acquire)
            || self.state.joined.load(Ordering::Acquire)
        {
            return true;
        }
        if !self.state.attached.load(Ordering::Acquire) {
            return false;
        }
        self.state
            .handle
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .as_ref()
            .is_some_and(JoinHandle::is_finished)
    }
    pub(crate) fn wait(&self, timeout: Duration) -> bool {
        if let Some(Ok(supervisor)) = SUPERVISOR.get() {
            supervisor.thread.thread().unpark();
        }
        let started = Instant::now();
        loop {
            if self.state.joined.load(Ordering::Acquire) {
                return true;
            }
            if started.elapsed() >= timeout {
                return false;
            }
            thread::sleep(Duration::from_millis(1).min(timeout.saturating_sub(started.elapsed())));
        }
    }
    pub(crate) fn panicked(&self) -> bool {
        self.state.panicked.load(Ordering::Acquire)
    }
    pub(crate) fn unpark(&self) {
        if let Some(thread) = self.state.wake.get() {
            thread.unpark();
        }
    }
}

pub(crate) fn outstanding(kind: WorkerKind) -> usize {
    let Some(Ok(supervisor)) = SUPERVISOR.get() else {
        return 0;
    };
    supervisor
        .registry
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .iter()
        .filter(|state| {
            state.kind == kind
                && !state.joined.load(Ordering::Acquire)
                && !state.abandoned.load(Ordering::Acquire)
        })
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::sync::mpsc;
    static CASES: Mutex<()> = Mutex::new(());

    struct TeardownGate {
        entered: mpsc::SyncSender<()>,
        release: mpsc::Receiver<()>,
    }

    impl Drop for TeardownGate {
        fn drop(&mut self) {
            let _ = self.entered.send(());
            let _ = self.release.recv();
        }
    }

    thread_local! {
        static TEARDOWN_GATE: RefCell<Option<TeardownGate>> = const { RefCell::new(None) };
    }

    struct ReleaseOnDrop(mpsc::SyncSender<()>);

    impl Drop for ReleaseOnDrop {
        fn drop(&mut self) {
            let _ = self.0.try_send(());
        }
    }

    #[test]
    fn timeout_keeps_the_worker_owned_and_release_allows_rejoin() {
        let _case = CASES.lock().unwrap();
        let permit = WorkerPermit::reserve(WorkerKind::Test).unwrap();
        let (release, receiver) = mpsc::sync_channel(1);
        let tracked = permit.attach(thread::spawn(move || {
            receiver.recv().unwrap();
        }));
        assert!(!tracked.is_finished());
        assert!(!tracked.wait(Duration::from_millis(5)));
        assert_eq!(outstanding(WorkerKind::Test), 1);
        release.send(()).unwrap();
        assert!(tracked.wait(Duration::from_secs(1)));
        assert!(tracked.is_finished());
        assert!(!tracked.panicked());
        assert_eq!(outstanding(WorkerKind::Test), 0);
    }

    #[test]
    fn pre_spawn_tracker_binds_the_actual_thread_and_waits_for_its_join() {
        let _case = CASES.lock().unwrap();
        let permit = WorkerPermit::reserve(WorkerKind::Test).unwrap();
        let before = permit.tracker();
        assert!(before.thread_id().is_none());
        assert!(!before.wait(Duration::ZERO));
        let (release, receiver) = mpsc::sync_channel(1);
        let handle = thread::spawn(move || {
            receiver.recv().unwrap();
        });
        let actual = handle.thread().id();
        let after = permit.attach(handle);
        assert_eq!(before.thread_id(), Some(actual));
        assert_eq!(after.thread_id(), Some(actual));
        assert!(!before.wait(Duration::ZERO));
        release.send(()).unwrap();
        assert!(before.wait(Duration::from_secs(1)));
        assert!(after.wait(Duration::ZERO));
    }

    #[test]
    fn last_caller_drop_does_not_detach_an_active_handle() {
        let _case = CASES.lock().unwrap();
        let permit = WorkerPermit::reserve(WorkerKind::Test).unwrap();
        let (release, receiver) = mpsc::sync_channel(1);
        let tracked = permit.attach(thread::spawn(move || {
            receiver.recv().unwrap();
        }));
        let state = Arc::downgrade(&tracked.state);
        drop(tracked);
        assert!(state.upgrade().is_some());
        assert_eq!(outstanding(WorkerKind::Test), 1);
        release.send(()).unwrap();
        let started = Instant::now();
        while outstanding(WorkerKind::Test) != 0 {
            assert!(started.elapsed() < Duration::from_secs(1));
            thread::sleep(Duration::from_millis(1));
        }
    }

    #[test]
    fn abandoned_permit_releases_capacity_without_starting_a_worker() {
        let _case = CASES.lock().unwrap();
        let permit = WorkerPermit::reserve(WorkerKind::Test).unwrap();
        assert_eq!(outstanding(WorkerKind::Test), 1);
        drop(permit);
        assert_eq!(outstanding(WorkerKind::Test), 0);
    }

    #[test]
    fn exhausted_capacity_rejects_before_a_worker_can_start() {
        let registry = Mutex::new(Vec::with_capacity(CAPACITY));
        let mut permits = Vec::with_capacity(CAPACITY);
        for _ in 0..CAPACITY {
            permits.push(WorkerPermit::reserve_in(&registry, WorkerKind::Test).unwrap());
        }
        let mut started = false;
        let admission = WorkerPermit::reserve_in(&registry, WorkerKind::Test).inspect(|_| {
            started = true;
        });
        assert!(admission.is_err());
        assert!(!started);
        assert_eq!(registry.lock().unwrap().len(), CAPACITY);
        drop(permits.pop());
        assert!(WorkerPermit::reserve_in(&registry, WorkerKind::Test).is_ok());
    }

    #[test]
    fn observed_completion_does_not_report_joined_during_thread_teardown() {
        let _case = CASES.lock().unwrap();
        let permit = WorkerPermit::reserve(WorkerKind::Test).unwrap();
        let (entered, teardown_entered) = mpsc::sync_channel(1);
        let (release, receiver) = mpsc::sync_channel(1);
        let release = ReleaseOnDrop(release);
        let tracked = permit.attach(thread::spawn(move || {
            TEARDOWN_GATE.with(|gate| {
                *gate.borrow_mut() = Some(TeardownGate {
                    entered,
                    release: receiver,
                });
            });
        }));
        teardown_entered
            .recv_timeout(Duration::from_secs(1))
            .unwrap();
        let started = Instant::now();
        while !tracked.is_finished() {
            assert!(started.elapsed() < Duration::from_secs(1));
            thread::yield_now();
        }
        assert!(!tracked.wait(Duration::from_millis(5)));
        assert_eq!(outstanding(WorkerKind::Test), 1);
        drop(release);
        assert!(tracked.wait(Duration::from_secs(1)));
        assert_eq!(outstanding(WorkerKind::Test), 0);
    }

    #[test]
    fn worker_panic_is_reported_only_after_the_owned_join() {
        let _case = CASES.lock().unwrap();
        let permit = WorkerPermit::reserve(WorkerKind::Test).unwrap();
        let tracked = permit.attach(thread::spawn(|| panic!("owned worker test panic")));
        assert!(tracked.wait(Duration::from_secs(1)));
        assert!(tracked.is_finished());
        assert!(tracked.panicked());
        assert_eq!(outstanding(WorkerKind::Test), 0);
    }
}

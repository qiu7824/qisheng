use super::*;
use crate::system_control::ManagedWorker;
use std::sync::{Condvar, OnceLock};

pub(super) struct ActorThreadOwner {
    state: Mutex<ActorThreadState>,
}
struct ActorThreadState {
    handle: Option<thread::JoinHandle<()>>,
    attaching: bool,
    joined: bool,
    error: Option<String>,
}
impl ActorThreadOwner {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(ActorThreadState {
                handle: None,
                attaching: false,
                joined: true,
                error: None,
            }),
        })
    }
    fn begin_attach(&self) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.attaching = true;
        state.joined = false;
    }
    fn attach(&self, handle: thread::JoinHandle<()>) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.handle = Some(handle);
        state.attaching = false;
    }
    fn reject_attach(&self) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.attaching = false;
        state.joined = true;
    }
    pub(super) fn join_finished(&self) -> Result<bool> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.attaching
            || state
                .handle
                .as_ref()
                .is_some_and(|handle| !handle.is_finished())
        {
            return Ok(false);
        }
        if let Some(handle) = state.handle.take() {
            if handle.join().is_err() {
                state.error = Some("Backend actor thread panicked".into());
            }
            state.joined = true;
        }
        if let Some(error) = &state.error {
            anyhow::bail!("{error}");
        }
        Ok(state.joined)
    }
    fn joined(&self) -> bool {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).joined
    }
}

struct ProfileRetirement {
    begun: bool,
    seed: Option<profile_handover::ProfileQuiesceSeed>,
    slot: Arc<profile_handover::ProfileQuiesceSlot>,
}
struct InitializationRepositoryOwner {
    repository: Arc<Repository>,
    retirement: Option<crate::library::authority::RetirementBarrier>,
}

const MAX_STARTUP_OWNERS: usize = 4;
type StartupTerminal = Arc<Mutex<Option<Result<(), BackendError>>>>;
struct StartupHolder {
    core: Mutex<Option<Runtime>>,
    retiring: AtomicBool,
    begun: AtomicBool,
    error: Mutex<Option<String>>,
    actor: Arc<ActorThreadOwner>,
    completed: AtomicBool,
    handover: Mutex<Option<ProfileRetirement>>,
    repository: Mutex<Option<InitializationRepositoryOwner>>,
    terminal: Mutex<Option<StartupTerminal>>,
}
impl StartupHolder {
    fn report_fatal(&self, error: &BackendError) {
        *self.error.lock().unwrap_or_else(|e| e.into_inner()) = Some(error.message.clone());
        if let Some(terminal) = self
            .terminal
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
        {
            *terminal.lock().unwrap_or_else(|e| e.into_inner()) = Some(Err(error.clone()));
        }
    }
    fn release_repository_after_retirement(&self) -> bool {
        let mut owner = self.repository.lock().unwrap_or_else(|e| e.into_inner());
        if owner.as_ref().is_some_and(|owner| {
            owner
                .retirement
                .as_ref()
                .is_some_and(|barrier| !barrier.wait(Duration::ZERO))
        }) {
            return false;
        }
        owner.take();
        true
    }
}

#[derive(Clone)]
pub(in crate::backend) struct ActorLifecycle {
    pool: Arc<StartupPool>,
    holder: Arc<StartupHolder>,
}
impl ActorLifecycle {
    #[cfg(test)]
    pub(super) fn has_owned_runtime(&self) -> bool {
        self.holder
            .core
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some()
    }
    pub(in crate::backend) fn view(&self) -> BackendRetirementView {
        BackendRetirementView {
            actor_joined: self.holder.actor.joined(),
            runtime_retired: self.holder.completed.load(Ordering::Acquire),
            error: self
                .holder
                .error
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .as_ref()
                .map(|error| BackendError::new(ErrorKind::OperationFailed, error.clone())),
        }
    }
    pub(in crate::backend) fn poll(&self) -> Result<bool> {
        let joined = self.holder.actor.join_finished()?;
        if joined {
            self.pool.changed.notify_all();
        }
        let retired = self.holder.completed.load(Ordering::Acquire);
        if retired {
            self.pool.settle_idle()?;
        } else {
            let mut state = self.pool.state.lock().unwrap_or_else(|e| e.into_inner());
            if state.slots.iter().any(Option::is_some) {
                self.pool.ensure_keeper(&mut state)?;
            }
        }
        Ok(joined && retired)
    }
}
struct PoolState {
    slots: Vec<Option<Arc<StartupHolder>>>,
    keeper: Option<ManagedWorker>,
    running: bool,
    joining: bool,
    worker_id: Option<thread::ThreadId>,
    error: Option<String>,
}
struct StartupPool {
    state: Mutex<PoolState>,
    changed: Condvar,
}
static POOL: OnceLock<Arc<StartupPool>> = OnceLock::new();
impl StartupPool {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(PoolState {
                slots: (0..MAX_STARTUP_OWNERS).map(|_| None).collect(),
                keeper: None,
                running: false,
                joining: false,
                worker_id: None,
                error: None,
            }),
            changed: Condvar::new(),
        })
    }
    fn settle_idle(&self) -> Result<()> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        while state.joining {
            state = self.changed.wait(state).unwrap_or_else(|e| e.into_inner());
        }
        if state.slots.iter().any(Option::is_some) {
            return Ok(());
        }
        while state.running {
            self.changed.notify_all();
            if state
                .keeper
                .as_ref()
                .is_some_and(ManagedWorker::is_finished)
            {
                state.running = false;
                break;
            }
            state = self
                .changed
                .wait_timeout(state, Duration::from_millis(20))
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
        let keeper = state.keeper.take();
        state.joining = keeper.is_some();
        drop(state);
        let result = if let Some(mut keeper) = keeper {
            while !keeper.is_finished() {
                thread::park_timeout(Duration::from_millis(1));
            }
            keeper.join_finished()
        } else {
            Ok(())
        };
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.joining = false;
        if let Err(error) = &result {
            state.error = Some(error.to_string());
        }
        self.changed.notify_all();
        result
    }
    fn ensure_keeper(self: &Arc<Self>, state: &mut PoolState) -> Result<()> {
        if state.running
            && state
                .keeper
                .as_ref()
                .is_some_and(|keeper| !keeper.is_finished())
        {
            return Ok(());
        }
        if let Some(mut keeper) = state.keeper.take() {
            while !keeper.is_finished() {
                thread::park_timeout(Duration::from_millis(1));
            }
            if let Err(error) = keeper.join_finished() {
                state.error = Some(error.to_string());
            }
        }
        state.running = false;
        let target = self.clone();
        let keeper = ManagedWorker::spawn("lx-failed-startup-owner", move || target.run())?;
        state.keeper = Some(keeper);
        state.running = true;
        Ok(())
    }
    fn run(self: Arc<Self>) {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .worker_id = Some(thread::current().id());
        loop {
            let holders = {
                let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
                if state.slots.iter().all(Option::is_none) {
                    state.running = false;
                    state.worker_id = None;
                    self.changed.notify_all();
                    return;
                }
                let holders = state
                    .slots
                    .iter()
                    .filter_map(Clone::clone)
                    .filter(|holder| holder.retiring.load(Ordering::Acquire))
                    .collect::<Vec<_>>();
                if holders.is_empty() {
                    drop(self.changed.wait(state));
                    continue;
                }
                holders
            };
            for holder in holders {
                if !holder.actor.joined() {
                    match holder.actor.join_finished() {
                        Ok(false) => continue,
                        Ok(true) => {}
                        Err(error) => {
                            *holder.error.lock().unwrap_or_else(|e| e.into_inner()) =
                                Some(error.to_string());
                            if !holder.actor.joined() {
                                continue;
                            }
                        }
                    }
                }
                let mut core = holder.core.lock().unwrap_or_else(|e| e.into_inner());
                let mut handover = holder.handover.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(retirement) = handover.as_mut() {
                    retirement.slot.mark_actor_joined();
                    if let Some(runtime) = core.as_mut() {
                        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            let seed = retirement
                                .seed
                                .as_mut()
                                .expect("Handover retains its checkpoint seed");
                            if !retirement.begun {
                                runtime.begin_profile_quiesce_services(seed, &retirement.slot)?;
                                retirement.begun = true;
                            }
                            let ready = runtime.poll_profile_quiesce(seed);
                            if let Some(error) = &seed.fatal_error {
                                retirement.slot.record_fatal(error.clone());
                                holder.report_fatal(error);
                            }
                            Ok::<_, anyhow::Error>(
                                ready? && retirement.slot.endpoints_joined(&seed.scope)?,
                            )
                        }));
                        match result {
                            Ok(Ok(true)) => {
                                let runtime = core.take().expect("Joined handover retains its runtime");
                                let seed = retirement.seed.take().expect("Joined handover retains its seed");
                                match runtime.into_handover_repository(seed.scope.clone(), seed, &retirement.slot) {
                                    Ok(owner) => retirement.slot.publish_retained(owner),
                                    Err(failure) => {
                                        retirement.slot.record_error(failure.error);
                                        retirement.seed = Some(*failure.seed);
                                        *core = Some(*failure.runtime);
                                    }
                                }
                            }
                            Ok(Ok(false)) => {},
                            Ok(Err(error)) => retirement.slot.record_error(BackendError::new(ErrorKind::OperationFailed, error.to_string())),
                            Err(_) => retirement.slot.record_error(BackendError::new(ErrorKind::OperationFailed, "Profile retirement step panicked; the whole runtime remains retained")),
                        }
                    }
                    if core.is_none()
                        && holder.release_repository_after_retirement()
                        && retirement.slot.retry_close()
                    {
                        self.release(&holder);
                        holder.completed.store(true, Ordering::Release);
                    }
                    continue;
                }
                let Some(runtime) = core.as_mut() else {
                    if holder.release_repository_after_retirement() {
                        self.release(&holder);
                        holder.completed.store(true, Ordering::Release);
                    }
                    continue;
                };
                let step = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    if !holder.begun.load(Ordering::Acquire) {
                        runtime.begin_failed_startup_retirement()?;
                        holder.begun.store(true, Ordering::Release);
                    }
                    runtime.poll_failed_startup_retirement()
                }));
                match step {
                    Ok(Ok(true)) => {
                        core.take();
                        if holder.release_repository_after_retirement() {
                            self.release(&holder);
                            holder.completed.store(true, Ordering::Release);
                        }
                    }
                    Ok(Ok(false)) => {}
                    Ok(Err(error)) => {
                        *holder.error.lock().unwrap_or_else(|e| e.into_inner()) =
                            Some(error.to_string())
                    }
                    Err(_) => {
                        *holder.error.lock().unwrap_or_else(|e| e.into_inner()) = Some(
                            "Runtime retirement step panicked; its actual owner remains retained"
                                .into(),
                        );
                    }
                }
            }
            let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            drop(self.changed.wait_timeout(state, Duration::from_millis(20)));
        }
    }
    fn release(&self, holder: &Arc<StartupHolder>) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(slot) = state.slots.iter_mut().find(|slot| {
            slot.as_ref()
                .is_some_and(|actual| Arc::ptr_eq(actual, holder))
        }) {
            *slot = None;
        }
        let on_keeper = state.worker_id == Some(thread::current().id());
        self.changed.notify_all();
        drop(state);
        // A keeper never joins itself. Its actual handle remains charged in the
        // pool until an observer/next admission actually joins it before restart.
        if !on_keeper {
            let _ = self.settle_idle();
        }
    }
}
fn pool() -> Arc<StartupPool> {
    POOL.get_or_init(StartupPool::new).clone()
}
pub(super) struct StartupAdmission {
    pool: Arc<StartupPool>,
    holder: Arc<StartupHolder>,
    committed: bool,
}
pub(super) struct ActorStartupTicket {
    pool: Arc<StartupPool>,
    holder: Arc<StartupHolder>,
}
impl StartupAdmission {
    pub(super) fn reserve() -> Result<Self> {
        Self::reserve_on(pool())
    }
    fn reserve_on(pool: Arc<StartupPool>) -> Result<Self> {
        pool.settle_idle()?;
        let holder = Arc::new(StartupHolder {
            core: Mutex::new(None),
            retiring: AtomicBool::new(false),
            begun: AtomicBool::new(false),
            error: Mutex::new(None),
            actor: ActorThreadOwner::new(),
            completed: AtomicBool::new(false),
            handover: Mutex::new(None),
            repository: Mutex::new(None),
            terminal: Mutex::new(None),
        });
        {
            let mut state = pool.state.lock().unwrap_or_else(|e| e.into_inner());
            while state.joining {
                state = pool.changed.wait(state).unwrap_or_else(|e| e.into_inner());
            }
            let index=state.slots.iter().position(Option::is_none).context("Startup retirement capacity reached; an earlier failed runtime still owns resources")?;
            state.slots[index] = Some(holder.clone());
            if let Err(error) = pool.ensure_keeper(&mut state) {
                state.slots[index] = None;
                return Err(error);
            }
            pool.changed.notify_all();
        }
        Ok(Self {
            pool,
            holder,
            committed: false,
        })
    }
    pub(super) fn attach(&self, core: Runtime) {
        *self.holder.core.lock().unwrap_or_else(|e| e.into_inner()) = Some(core);
    }
    pub(super) fn retain_repository(
        &self,
        repository: Arc<Repository>,
        retirement: Option<crate::library::authority::RetirementBarrier>,
    ) -> Result<()> {
        let mut owner = self
            .holder
            .repository
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        ensure!(
            owner
                .as_ref()
                .is_none_or(|owner| Arc::ptr_eq(&owner.repository, &repository)),
            "Startup repository ownership changed"
        );
        *owner = Some(InitializationRepositoryOwner {
            repository,
            retirement,
        });
        Ok(())
    }
    pub(super) fn attach_shutdown_outcome(&self, terminal: StartupTerminal) {
        *self
            .holder
            .terminal
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(terminal);
    }
    pub(super) fn core(&self) -> std::sync::MutexGuard<'_, Option<Runtime>> {
        self.holder.core.lock().unwrap_or_else(|e| e.into_inner())
    }
    pub(super) fn actor_ticket(&self) -> ActorStartupTicket {
        ActorStartupTicket {
            pool: self.pool.clone(),
            holder: self.holder.clone(),
        }
    }
    pub(super) fn lifecycle(&self) -> ActorLifecycle {
        ActorLifecycle {
            pool: self.pool.clone(),
            holder: self.holder.clone(),
        }
    }
    pub(super) fn begin_actor_spawn(&self) {
        self.holder.actor.begin_attach();
    }
    pub(super) fn attach_actor(&self, handle: thread::JoinHandle<()>) {
        self.holder.actor.attach(handle);
        self.pool.changed.notify_all();
    }
    pub(super) fn actor_spawn_failed(&self) {
        self.holder.actor.reject_attach();
    }
    pub(super) fn commit(&mut self) {
        self.committed = true;
    }
}
impl Drop for StartupAdmission {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        if self
            .holder
            .core
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some()
            || self
                .holder
                .repository
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .is_some()
        {
            self.holder.retiring.store(true, Ordering::Release);
            self.pool.changed.notify_all();
        } else {
            self.pool.release(&self.holder);
        }
    }
}
impl ActorStartupTicket {
    pub(super) fn take(&self) -> Runtime {
        self.holder
            .core
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
            .expect("reserved actor startup owns a runtime")
    }
    pub(super) fn retire(
        self,
        core: Runtime,
        handover: Option<(
            profile_handover::ProfileQuiesceSeed,
            Arc<profile_handover::ProfileQuiesceSlot>,
        )>,
        failure: Option<String>,
    ) {
        if let Some(error) = failure {
            *self.holder.error.lock().unwrap_or_else(|e| e.into_inner()) = Some(error);
        }
        *self.holder.core.lock().unwrap_or_else(|e| e.into_inner()) = Some(core);
        if let Some((seed, slot)) = handover {
            *self
                .holder
                .handover
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = Some(ProfileRetirement {
                begun: false,
                seed: Some(seed),
                slot,
            });
        }
        self.holder.retiring.store(true, Ordering::Release);
        self.pool.changed.notify_all();
    }
}
impl Runtime {
    fn begin_failed_startup_retirement(&mut self) -> Result<()> {
        self.external_control.retire();
        let _ = self.retire_sync_service();
        self.deep_links.begin_shutdown();
        self.begin_effect_shutdown();
        self.begin_download_shutdown();
        self.begin_cleanup_shutdown();
        self.begin_system_control_shutdown()?;
        self.begin_instance_relay_shutdown();
        self.begin_tag_shutdown();
        self.cancel_runtime();
        for job in self.provider_jobs.values() {
            job.cancel.store(true, Ordering::Release);
        }
        for cancel in self.import_cancels.values() {
            cancel.terminate();
        }
        for cancel in self.file_cancels.values() {
            cancel.store(true, Ordering::Release);
        }
        self.cancel_all_picture_requests();
        self.backup_exports.begin_shutdown();
        self.physical_snapshots.begin_shutdown();
        self.pictures.begin_shutdown();
        self.shutdown_library_operations();
        Ok(())
    }
    fn poll_failed_startup_retirement(&mut self) -> Result<bool> {
        for _ in 0..32 {
            let Ok(completion) = self.completed_rx.try_recv() else {
                break;
            };
            self.dispatch_completion(completion);
        }
        self.poll_downloads();
        self.poll_cleanup();
        self.poll_backup_exports();
        self.poll_physical_snapshots();
        ensure!(
            !self.reap_shutdown_tasks(),
            "A startup service task panicked"
        );
        let mut ready = self.tasks.is_empty() && self.provider_tasks.is_empty();
        ready &= self.poll_download_quiesce_step()?;
        ready &= self.retry_cleanup_retirement();
        ready &= self.retire_physical_snapshot_owners()?;
        ready &= self.poll_system_control_quiesce_step()?;
        ready &= self.poll_instance_relay_quiesce_step()?;
        ready &= self.poll_tag_quiesce_step()?;
        ready &= self.poll_effect_quiesce()?;
        // A durably registered cold locator still charges its quota after the
        // actual snapshot descriptors close. It is an error outcome, not a
        // remaining worker or handle that prevents this Runtime from retiring.
        ready &= !self.backup_exports.has_running()
            && self.backup_exports.retained_count() == 0
            && !self.physical_snapshots.has_running()
            && !self.physical_snapshots.has_live_owners();
        match self.audio.shutdown(Duration::ZERO) {
            Ok(()) => {}
            Err(error)
                if error
                    .downcast_ref::<crate::audio::AudioShutdownError>()
                    .is_some_and(|error| error.is_timeout()) =>
            {
                ready = false
            }
            Err(error) => return Err(error),
        }
        ready &= self.catalog.shutdown(Duration::ZERO)?;
        ready &= self.pictures.shutdown(Duration::ZERO);
        ready &= self.library.shutdown(Duration::ZERO)?;
        Ok(ready)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn staged_admission_scope_actually_joins_keeper_before_restart() {
        let pool = StartupPool::new();
        for _ in 0..2 {
            let admission = StartupAdmission::reserve_on(pool.clone()).unwrap();
            assert!(pool.state.lock().unwrap().keeper.is_some());
            drop(admission);
            let state = pool.state.lock().unwrap();
            assert!(!state.running && !state.joining);
            assert!(state.keeper.is_none());
            assert!(state.slots.iter().all(Option::is_none));
        }
    }
    #[test]
    fn failed_observer_exit_retains_finished_keeper_until_an_actual_join() {
        let pool = StartupPool::new();
        let mut admission = StartupAdmission::reserve_on(pool.clone()).unwrap();
        admission.holder.retiring.store(true, Ordering::Release);
        pool.changed.notify_all();
        admission.commit();
        drop(admission);
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let state = pool.state.lock().unwrap();
            if !state.running {
                assert!(state.keeper.is_some());
                break;
            }
            drop(state);
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(1));
        }
        pool.settle_idle().unwrap();
        assert!(pool.state.lock().unwrap().keeper.is_none());
        let next = StartupAdmission::reserve_on(pool.clone()).unwrap();
        drop(next);
        assert!(pool.state.lock().unwrap().keeper.is_none());
    }
    #[test]
    fn four_staged_slots_remain_charged_until_their_observers_release() {
        let pool = StartupPool::new();
        let admissions = (0..MAX_STARTUP_OWNERS)
            .map(|_| StartupAdmission::reserve_on(pool.clone()).unwrap())
            .collect::<Vec<_>>();
        assert!(StartupAdmission::reserve_on(pool.clone()).is_err());
        assert_eq!(
            pool.state
                .lock()
                .unwrap()
                .slots
                .iter()
                .filter(|slot| slot.is_some())
                .count(),
            4
        );
        drop(admissions);
        assert!(pool.state.lock().unwrap().keeper.is_none());
    }
}

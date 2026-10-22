use anyhow::{Result, ensure};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::thread;
use std::time::Duration;
const MAX_WORKERS: usize = 32;
enum Entry {
    Free,
    Reserved,
    Retiring(thread::JoinHandle<()>),
}
struct State {
    entries: Vec<Entry>,
    owner: Option<thread::JoinHandle<()>>,
    running: bool,
}
struct Supervisor {
    state: Mutex<State>,
    wake: Condvar,
}
static SUPERVISOR: OnceLock<Arc<Supervisor>> = OnceLock::new();
impl Supervisor {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(State {
                entries: (0..MAX_WORKERS).map(|_| Entry::Free).collect(),
                owner: None,
                running: false,
            }),
            wake: Condvar::new(),
        })
    }
    fn reserve(self: &Arc<Self>) -> Result<usize> {
        self.join_idle()?;
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        self.ensure_running(&mut state)?;
        let index = state
            .entries
            .iter()
            .position(|entry| matches!(entry, Entry::Free))
            .ok_or_else(|| anyhow::anyhow!("System worker capacity reached"))?;
        state.entries[index] = Entry::Reserved;
        Ok(index)
    }
    fn ensure_running(self: &Arc<Self>, state: &mut State) -> Result<()> {
        if state
            .owner
            .as_ref()
            .is_some_and(|owner| owner.is_finished())
        {
            state.running = false;
        }
        if state.running {
            return Ok(());
        }
        if let Some(owner) = state.owner.take() {
            owner
                .join()
                .map_err(|_| anyhow::anyhow!("System retirement supervisor panicked"))?;
        }
        if !state
            .entries
            .iter()
            .any(|entry| matches!(entry, Entry::Retiring(_)))
        {
            return Ok(());
        }
        let target = self.clone();
        let owner = thread::Builder::new()
            .name("lx-system-owner-retirement".into())
            .spawn(move || target.run())?;
        state.owner = Some(owner);
        state.running = true;
        Ok(())
    }
    fn run(self: Arc<Self>) {
        loop {
            let retiring = {
                let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(index) = state.entries.iter().position(
                    |entry| matches!(entry,Entry::Retiring(handle) if handle.is_finished()),
                ) {
                    match std::mem::replace(&mut state.entries[index], Entry::Reserved) {
                        Entry::Retiring(handle) => Some((index, handle)),
                        _ => unreachable!(),
                    }
                } else if state
                    .entries
                    .iter()
                    .any(|entry| matches!(entry, Entry::Retiring(_)))
                {
                    drop(self.wake.wait_timeout(state, Duration::from_millis(20)));
                    None
                } else {
                    state.running = false;
                    self.wake.notify_all();
                    return;
                }
            };
            if let Some((index, handle)) = retiring {
                let _ = handle.join();
                self.state.lock().unwrap_or_else(|e| e.into_inner()).entries[index] = Entry::Free;
                self.wake.notify_all();
            }
        }
    }
    fn join_idle(&self) -> Result<()> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state
            .entries
            .iter()
            .any(|entry| matches!(entry, Entry::Retiring(_)))
        {
            return Ok(());
        }
        while state.running {
            self.wake.notify_all();
            if state
                .owner
                .as_ref()
                .is_some_and(|owner| owner.is_finished())
            {
                state.running = false;
                break;
            }
            state = self
                .wake
                .wait_timeout(state, Duration::from_millis(20))
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
        if let Some(owner) = state.owner.take() {
            owner
                .join()
                .map_err(|_| anyhow::anyhow!("System retirement supervisor panicked"))?;
        }
        Ok(())
    }
    fn release(&self, index: usize) {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).entries[index] = Entry::Free;
        self.wake.notify_all();
    }
    fn retain(self: &Arc<Self>, index: usize, handle: thread::JoinHandle<()>) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.entries[index] = Entry::Retiring(handle);
        // Record the actual handle before attempting a supervisor restart. A
        // spawn failure leaves that same charged slot for the next admission.
        let _ = self.ensure_running(&mut state);
        self.wake.notify_all();
    }
}
fn supervisor() -> Arc<Supervisor> {
    SUPERVISOR.get_or_init(Supervisor::new).clone()
}
pub(crate) struct ManagedWorker {
    handle: Option<thread::JoinHandle<()>>,
    supervisor: Arc<Supervisor>,
    index: usize,
    reserved: bool,
}
impl ManagedWorker {
    pub(crate) fn spawn(name: &str, work: impl FnOnce() + Send + 'static) -> Result<Self> {
        Self::spawn_on(supervisor(), name, work)
    }
    fn spawn_on(
        owner: Arc<Supervisor>,
        name: &str,
        work: impl FnOnce() + Send + 'static,
    ) -> Result<Self> {
        let index = owner.reserve()?;
        match thread::Builder::new().name(name.into()).spawn(work) {
            Ok(handle) => Ok(Self {
                handle: Some(handle),
                supervisor: owner,
                index,
                reserved: true,
            }),
            Err(error) => {
                owner.release(index);
                Err(error.into())
            }
        }
    }
    pub(crate) fn is_finished(&self) -> bool {
        self.handle
            .as_ref()
            .is_none_or(|handle| handle.is_finished())
    }
    pub(crate) fn join_finished(&mut self) -> Result<()> {
        if self.handle.is_none() && !self.reserved {
            return self.supervisor.join_idle();
        }
        ensure!(self.is_finished(), "System worker has not actually exited");
        let result = self.handle.take().map(|handle| handle.join());
        self.supervisor.release(self.index);
        self.reserved = false;
        self.supervisor.join_idle()?;
        if result.is_some_and(|result| result.is_err()) {
            anyhow::bail!("System worker panicked");
        }
        Ok(())
    }
}
impl Drop for ManagedWorker {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            self.supervisor.retain(self.index, handle);
            self.reserved = false;
        } else if self.reserved {
            self.supervisor.release(self.index);
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Instant;
    #[test]
    fn explicit_join_leaves_no_running_or_unjoined_supervisor() {
        let owner = Supervisor::new();
        let mut worker =
            ManagedWorker::spawn_on(owner.clone(), "system-explicit-lifecycle", || {}).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while !worker.is_finished() {
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(1));
        }
        worker.join_finished().unwrap();
        let state = owner.state.lock().unwrap();
        assert!(!state.running);
        assert!(state.owner.is_none());
        assert!(
            state
                .entries
                .iter()
                .all(|entry| matches!(entry, Entry::Free))
        );
    }
    #[test]
    fn dropped_actual_worker_is_charged_until_join_then_supervisor_exits_and_restarts() {
        let owner = Supervisor::new();
        for iteration in 0..2 {
            let (release, blocked) = mpsc::sync_channel(1);
            let worker =
                ManagedWorker::spawn_on(owner.clone(), "system-retirement-lifecycle", move || {
                    blocked.recv().unwrap()
                })
                .unwrap();
            let index = worker.index;
            drop(worker);
            {
                let state = owner.state.lock().unwrap();
                assert!(matches!(state.entries[index], Entry::Retiring(_)));
                assert!(state.owner.is_some());
            }
            release.send(()).unwrap();
            let deadline = Instant::now() + Duration::from_secs(2);
            loop {
                let state = owner.state.lock().unwrap();
                if matches!(state.entries[index], Entry::Free) && !state.running {
                    break;
                }
                drop(state);
                assert!(Instant::now() < deadline, "iteration {iteration}");
                thread::sleep(Duration::from_millis(1));
            }
            owner.join_idle().unwrap();
            assert!(owner.state.lock().unwrap().owner.is_none());
        }
    }
}

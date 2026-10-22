use super::{MAX_SYSTEM_INPUTS, SystemAction, SystemInput, supervisor::ManagedWorker};
use anyhow::{Result, ensure};
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

/// Implementations apply owner and generation fences inside the backend actor,
/// immediately before their ordinary command effects.
pub trait SystemBackend: Send + Sync + 'static {
    fn dispatch_system_input(&self, input: SystemInput, fence: SystemInputFence) -> Result<()>;
}
struct Gate {
    epoch: u64,
    next: AtomicU64,
    closing: Arc<AtomicBool>,
    paused: AtomicBool,
    admission_revision: AtomicU64,
}
/// An owned fence remains live through a queued backend request. Backend actors
/// must sample it again immediately before applying the ordinary command.
#[derive(Clone)]
pub struct SystemInputFence {
    gate: Arc<Gate>,
    admission_revision: u64,
}
impl SystemInputFence {
    pub fn owner_epoch(&self) -> u64 {
        self.gate.epoch
    }
    pub fn is_closing(&self) -> bool {
        self.gate.closing.load(Ordering::Acquire)
    }
    pub fn is_paused(&self) -> bool {
        self.gate.paused.load(Ordering::Acquire)
    }
    pub fn same_owner(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.gate, &other.gate)
    }
    pub fn validate(&self, input: SystemInput) -> Result<()> {
        input.validate()?;
        ensure!(
            input.owner_epoch == self.owner_epoch(),
            "System input owner changed"
        );
        ensure!(
            !self.is_closing() && !self.is_paused(),
            "System input admission is closed"
        );
        ensure!(
            self.gate.admission_revision.load(Ordering::Acquire) == self.admission_revision,
            "System input configuration changed"
        );
        Ok(())
    }
}
struct QueuedSystemInput {
    input: SystemInput,
    admission_revision: u64,
}
#[derive(Clone)]
pub struct SystemInputSender {
    sender: mpsc::SyncSender<QueuedSystemInput>,
    gate: Arc<Gate>,
}
impl SystemInputSender {
    pub fn owner_epoch(&self) -> u64 {
        self.gate.epoch
    }
    pub fn is_closing(&self) -> bool {
        self.gate.closing.load(Ordering::Acquire)
    }
    pub fn fence(&self) -> SystemInputFence {
        SystemInputFence {
            gate: self.gate.clone(),
            admission_revision: self.gate.admission_revision.load(Ordering::Acquire),
        }
    }
    pub fn try_submit(&self, action: SystemAction, generation: Option<u64>) -> Result<()> {
        let admission_revision = self.gate.admission_revision.load(Ordering::Acquire);
        ensure!(
            !self.is_closing() && !self.gate.paused.load(Ordering::Acquire),
            "System control input admission is closed"
        );
        action.validate()?;
        let sequence = loop {
            let old = self.gate.next.load(Ordering::Relaxed);
            let next = old
                .checked_add(1)
                .ok_or_else(|| anyhow::anyhow!("System input sequence exhausted"))?;
            if self
                .gate
                .next
                .compare_exchange_weak(old, next, Ordering::AcqRel, Ordering::Relaxed)
                .is_ok()
            {
                break next;
            }
        };
        let input = SystemInput {
            owner_epoch: self.gate.epoch,
            sequence,
            generation,
            action,
        };
        input.validate()?;
        self.sender
            .try_send(QueuedSystemInput {
                input,
                admission_revision,
            })
            .map_err(|e| match e {
                mpsc::TrySendError::Full(_) => {
                    anyhow::anyhow!("System control input queue is full")
                }
                mpsc::TrySendError::Disconnected(_) => {
                    anyhow::anyhow!("System control dispatcher has stopped")
                }
            })
    }
    pub(crate) fn close(&self) {
        self.gate.closing.store(true, Ordering::Release);
    }
    pub(crate) fn pause(&self) {
        self.gate.paused.store(true, Ordering::Release);
        if self
            .gate
            .admission_revision
            .try_update(Ordering::AcqRel, Ordering::Acquire, |r| r.checked_add(1))
            .is_err()
        {
            self.close();
        }
    }
    pub(crate) fn resume(&self) {
        self.gate.paused.store(false, Ordering::Release);
    }
}
pub struct SystemDispatcher {
    sender: SystemInputSender,
    worker: Option<ManagedWorker>,
    failures: Arc<Mutex<Option<String>>>,
}
impl SystemDispatcher {
    pub fn start(epoch: u64, backend: Arc<dyn SystemBackend>) -> Result<Self> {
        ensure!(epoch > 0, "System owner epoch is invalid");
        let (sender, receiver) = mpsc::sync_channel::<QueuedSystemInput>(MAX_SYSTEM_INPUTS);
        let gate = Arc::new(Gate {
            epoch,
            next: AtomicU64::new(0),
            closing: Arc::new(AtomicBool::new(false)),
            paused: AtomicBool::new(false),
            admission_revision: AtomicU64::new(1),
        });
        let failures = Arc::new(Mutex::new(None));
        let output = failures.clone();
        let closing = gate.closing.clone();
        let owner = gate.clone();
        let worker = ManagedWorker::spawn("lx-system-command-dispatcher", move || {
            while !closing.load(Ordering::Acquire) {
                match receiver.recv_timeout(Duration::from_millis(20)) {
                    Ok(queued) => {
                        if closing.load(Ordering::Acquire) {
                            break;
                        }
                        let fence = SystemInputFence {
                            gate: owner.clone(),
                            admission_revision: queued.admission_revision,
                        };
                        let outcome = fence
                            .validate(queued.input)
                            .and_then(|_| backend.dispatch_system_input(queued.input, fence));
                        if let Err(error) = outcome {
                            *output.lock().unwrap_or_else(|e| e.into_inner()) =
                                Some(format!("{error:#}").chars().take(1024).collect());
                        }
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => (),
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                }
            }
        })?;
        Ok(Self {
            sender: SystemInputSender { sender, gate },
            worker: Some(worker),
            failures,
        })
    }
    pub fn sender(&self) -> SystemInputSender {
        self.sender.clone()
    }
    pub fn last_error(&self) -> Option<String> {
        self.failures
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
    pub fn begin_shutdown(&self) {
        self.sender.close();
    }
    pub fn shutdown(&mut self, timeout: Duration) -> Result<bool> {
        self.begin_shutdown();
        let deadline = Instant::now() + timeout;
        while self.worker.as_ref().is_some_and(|w| !w.is_finished()) {
            if Instant::now() >= deadline {
                return Ok(false);
            }
            thread::sleep(Duration::from_millis(2));
        }
        if let Some(mut worker) = self.worker.take() {
            worker.join_finished()?;
        }
        Ok(true)
    }
}
impl Drop for SystemDispatcher {
    fn drop(&mut self) {
        self.begin_shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Backend;
    impl SystemBackend for Backend {
        fn dispatch_system_input(&self, input: SystemInput, fence: SystemInputFence) -> Result<()> {
            fence.validate(input)
        }
    }
    #[test]
    fn an_owned_backend_fence_rejects_the_old_configuration_after_resume() {
        let mut dispatcher = SystemDispatcher::start(42, Arc::new(Backend)).unwrap();
        let sender = dispatcher.sender();
        let original = sender.fence();
        let input = SystemInput {
            owner_epoch: 42,
            sequence: 1,
            generation: None,
            action: SystemAction::Toggle,
        };
        original.validate(input).unwrap();
        sender.pause();
        assert!(original.validate(input).is_err());
        sender.resume();
        assert!(original.validate(input).is_err());
        let current = sender.fence();
        assert!(original.same_owner(&current));
        current.validate(input).unwrap();
        let mut foreign = SystemDispatcher::start(42, Arc::new(Backend)).unwrap();
        assert!(!current.same_owner(&foreign.sender().fence()));
        assert!(foreign.shutdown(Duration::from_secs(2)).unwrap());
        assert!(dispatcher.shutdown(Duration::from_secs(2)).unwrap());
        assert!(current.validate(input).is_err());
    }
}

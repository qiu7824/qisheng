#![cfg(windows)]
use super::*;
use crate::backend::{
    BackendConfig, BackendEvent, BackendHandle, BackendQuery, BackendQueryResult, CommandEnvelope,
};
use std::{
    collections::HashMap,
    path::Path,
    sync::{
        Condvar, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
};

const WAIT: Duration = Duration::from_secs(30);
#[derive(Default)]
struct Gate {
    state: Mutex<(bool, bool)>,
    changed: Condvar,
}
impl Gate {
    fn enter(&self, context: Option<&ImpulseDecodeContext>) -> Result<()> {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        state.0 = true;
        self.changed.notify_all();
        while !state.1 {
            if let Some(context) = context {
                context.check()?;
            }
            state = self
                .changed
                .wait_timeout(state, Duration::from_millis(10))
                .unwrap()
                .0;
        }
        Ok(())
    }
    fn wait_entered(&self) {
        let end = Instant::now() + WAIT;
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        while !state.0 {
            assert!(
                Instant::now() < end,
                "Actual worker did not enter fault gate"
            );
            state = self
                .changed
                .wait_timeout(state, Duration::from_millis(10))
                .unwrap()
                .0;
        }
    }
    fn release(&self) {
        self.state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .1 = true;
        self.changed.notify_all();
    }
}
#[derive(Default)]
pub(super) struct Hooks {
    writing: Gate,
    join: Gate,
    gate_writing: AtomicBool,
    gate_join: AtomicBool,
    reject_first_ack: AtomicBool,
    freeze_pending: AtomicBool,
    request_repair: AtomicBool,
}
static HOOKS: OnceLock<Mutex<HashMap<String, Arc<Hooks>>>> = OnceLock::new();
fn key(path: &Path) -> String {
    path.to_string_lossy()
        .replace("\\\\?\\", "")
        .replace('/', "\\")
        .to_ascii_lowercase()
}
fn hooks(path: &Path) -> Option<Arc<Hooks>> {
    HOOKS
        .get_or_init(Default::default)
        .lock()
        .unwrap()
        .get(&key(path))
        .cloned()
}
pub(in crate::backend::runtime) fn after_writing_ack(
    profile: &Path,
    fact: &crate::repository::ImpulseImportFact,
    context: &ImpulseDecodeContext,
) -> Result<()> {
    if fact.phase == crate::repository::ImpulseImportPhase::Writing
        && let Some(hooks) = hooks(profile)
        && hooks.gate_writing.load(Ordering::Acquire)
    {
        hooks.writing.enter(Some(context))?;
    }
    Ok(())
}
pub(super) enum AckAction {
    Continue,
    Reject,
    Freeze,
}
pub(super) fn retirement_ack(profile: &Path, record: &ImpulseRetirementRecord) -> AckAction {
    let Some(hooks) = hooks(profile) else {
        return AckAction::Continue;
    };
    if !record.pending_zero_links() && hooks.reject_first_ack.swap(false, Ordering::AcqRel) {
        return AckAction::Reject;
    }
    if record.pending_zero_links()
        && !record.removed()
        && hooks.freeze_pending.swap(false, Ordering::AcqRel)
    {
        return AckAction::Freeze;
    }
    AckAction::Continue
}
pub(super) fn take_store_repair(profile: &Path) -> bool {
    hooks(profile).is_some_and(|hooks| hooks.request_repair.swap(false, Ordering::AcqRel))
}
pub(super) fn after_cleanup_result(profile: &Path) {
    if let Some(hooks) = hooks(profile)
        && hooks.gate_join.load(Ordering::Acquire)
    {
        let _ = hooks.join.enter(None);
    }
}

struct Case {
    temp: Option<tempfile::TempDir>,
    backend: Option<BackendHandle>,
    hooks: Arc<Hooks>,
}
impl Case {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let profile = temp.path().join("profile");
        let hooks = Arc::new(Hooks::default());
        HOOKS
            .get_or_init(Default::default)
            .lock()
            .unwrap()
            .insert(key(&profile), hooks.clone());
        let backend = BackendHandle::start(BackendConfig::headless(profile)).unwrap();
        Self {
            temp: Some(temp),
            backend: Some(backend),
            hooks,
        }
    }
    fn root(&self) -> &Path {
        self.temp.as_ref().unwrap().path()
    }
    fn profile(&self) -> PathBuf {
        self.root().join("profile")
    }
    fn backend(&self) -> &BackendHandle {
        self.backend.as_ref().unwrap()
    }
    fn execute(&self, id: &str, command: BackendCommand) -> crate::backend::CommandReceipt {
        self.backend()
            .execute(CommandEnvelope {
                request_id: id.into(),
                expected_revision: None,
                origin: "abort-actor-fixture".into(),
                command,
            })
            .unwrap()
    }
    fn prefs(&self) -> EffectsPreferences {
        let BackendQueryResult::ImpulseResources(preferences) = self
            .backend()
            .query(BackendQuery::ImpulseResources)
            .unwrap()
        else {
            panic!("Wrong query kind");
        };
        preferences
    }
    fn partial(&self, id: &str) -> crate::repository::ImpulseImportFact {
        self.hooks.gate_writing.store(true, Ordering::Release);
        let path = self.root().join(format!("{id}.wav"));
        std::fs::write(&path, wave()).unwrap();
        self.execute(
            id,
            BackendCommand::ImportImpulseResource {
                path,
                file_read_approved: true,
                timeout_ms: Some(30000),
            },
        );
        self.hooks.writing.wait_entered();
        let fact = self
            .prefs()
            .imports
            .into_iter()
            .find(|fact| fact.phase == crate::repository::ImpulseImportPhase::Writing)
            .unwrap();
        self.execute(
            &format!("{id}-cancel"),
            BackendCommand::CancelEffectsOperation {
                operation_id: id.into(),
            },
        );
        self.hooks.writing.release();
        self.hooks.gate_writing.store(false, Ordering::Release);
        let end = Instant::now() + WAIT;
        loop {
            let BackendQueryResult::EffectsOperations(operations) = self
                .backend()
                .query(BackendQuery::EffectsOperations)
                .unwrap()
            else {
                panic!();
            };
            if operations
                .iter()
                .all(|operation| operation.state != EffectsOperationState::Preparing)
            {
                break;
            }
            assert!(Instant::now() < end);
            std::thread::sleep(Duration::from_millis(10));
        }
        fact
    }
    fn body(&self, token: &str) -> PathBuf {
        self.profile()
            .join(".lx-impulse-vault")
            .join(format!(".lx-profile-{token}-impulse.bin"))
    }
    fn outcome(&self, id: &str, cursor: u64) -> (bool, Option<BackendError>) {
        let end = Instant::now() + WAIT;
        loop {
            for event in self.backend().events_after(cursor).unwrap() {
                if let BackendEvent::OperationFinished {
                    operation_id,
                    success,
                    error,
                } = event.payload
                    && operation_id == id
                {
                    return (success, error);
                }
            }
            assert!(
                Instant::now() < end,
                "Actual operation has no terminal event"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    fn restart(&mut self) {
        let backend = self.backend.take().unwrap();
        if let Err(error) = backend.shutdown(WAIT) {
            self.backend = Some(backend);
            panic!("Actual backend retirement failed: {error}");
        }
        let retired = backend.retirement_state();
        assert!(
            retired.actor_joined && retired.runtime_retired,
            "{retired:?}"
        );
        self.backend = Some(BackendHandle::start(BackendConfig::headless(self.profile())).unwrap());
    }
}
impl Drop for Case {
    fn drop(&mut self) {
        self.hooks.writing.release();
        self.hooks.join.release();
        self.hooks.request_repair.store(true, Ordering::Release);
        if let Some(backend) = self.backend.take()
            && let Err(error) = backend.shutdown(WAIT)
        {
            if let Some(directory) = self.temp.take() {
                let _ = directory.keep();
            }
            if !std::thread::panicking() {
                panic!("Actual backend retirement failed: {error}");
            }
            return;
        }
        HOOKS
            .get()
            .unwrap()
            .lock()
            .unwrap()
            .remove(&key(&self.profile()));
    }
}
fn wave() -> Vec<u8> {
    let mut bytes = b"RIFF".to_vec();
    bytes.extend_from_slice(&40u32.to_le_bytes());
    bytes.extend_from_slice(b"WAVEfmt ");
    bytes.extend_from_slice(&16u32.to_le_bytes());
    bytes.extend_from_slice(&1u16.to_le_bytes());
    bytes.extend_from_slice(&1u16.to_le_bytes());
    bytes.extend_from_slice(&8000u32.to_le_bytes());
    bytes.extend_from_slice(&16000u32.to_le_bytes());
    bytes.extend_from_slice(&2u16.to_le_bytes());
    bytes.extend_from_slice(&16u16.to_le_bytes());
    bytes.extend_from_slice(b"data");
    bytes.extend_from_slice(&4u32.to_le_bytes());
    bytes.extend_from_slice(&16384i16.to_le_bytes());
    bytes.extend_from_slice(&0i16.to_le_bytes());
    bytes
}

#[test]
fn public_abort_envelope_rejects_invalid_inventory_and_cold_consent_without_namespace_allocation() {
    let case = Case::new();
    let denied = serde_json::json!({"request_id":"cold-denied","command":"abort_impulse_import","token":"0".repeat(32),
        "selection":{"path":case.root().join("foreign.bin"),"file_read_approved":false,"file_write_approved":false}});
    let envelope: CommandEnvelope = serde_json::from_value(denied).unwrap();
    assert!(case.backend().execute(envelope).is_err());
    let invalid: CommandEnvelope =
        serde_json::from_value(serde_json::json!({"request_id":"invalid-token",
        "command":"abort_impulse_import","token":"../../foreign"}))
        .unwrap();
    assert!(case.backend().execute(invalid).is_err());
    assert!(!case.profile().join(".lx-impulse-vault").exists());
    assert!(case.prefs().imports.is_empty());
}

#[test]
fn abort_terminal_event_waits_for_durable_removed_ack_and_actual_worker_join() {
    let case = Case::new();
    let fact = case.partial("writing");
    assert_eq!(std::fs::metadata(case.body(&fact.token)).unwrap().len(), 0);
    case.hooks.gate_join.store(true, Ordering::Release);
    let cursor = case.backend().event_cursor();
    case.execute(
        "abort",
        BackendCommand::AbortImpulseImport {
            token: fact.token.clone(),
            selection: None,
        },
    );
    case.hooks.join.wait_entered();
    assert!(
        case.prefs()
            .retirements
            .iter()
            .any(|record| record.token == fact.token && record.removed())
    );
    assert!(
        case.prefs()
            .imports
            .iter()
            .any(|entry| entry.token == fact.token)
    );
    assert!(!case.backend().events_after(cursor).unwrap().iter().any(|event|
        matches!(&event.payload, BackendEvent::OperationFinished { operation_id, .. } if operation_id == "abort")));
    case.hooks.join.release();
    assert!(case.outcome("abort", cursor).0);
    assert!(case.prefs().imports.is_empty());
    assert!(!case.body(&fact.token).exists());
}
#[test]
fn abort_failed_actor_ack_preserves_original_live_descriptor_and_retry() {
    let case = Case::new();
    let fact = case.partial("failed-ack");
    case.hooks.reject_first_ack.store(true, Ordering::Release);
    let cursor = case.backend().event_cursor();
    case.execute(
        "abort-fail",
        BackendCommand::AbortImpulseImport {
            token: fact.token.clone(),
            selection: None,
        },
    );
    assert!(!case.outcome("abort-fail", cursor).0);
    assert_eq!(case.prefs().imports[0].body_identity, fact.body_identity);
    assert_eq!(std::fs::metadata(case.body(&fact.token)).unwrap().len(), 0);
    assert!(
        std::fs::rename(case.body(&fact.token), case.profile().join("displaced.bin")).is_err(),
        "Original native write descriptor was lost"
    );
    let cursor = case.backend().event_cursor();
    case.execute(
        "abort-retry",
        BackendCommand::AbortImpulseImport {
            token: fact.token.clone(),
            selection: None,
        },
    );
    assert!(case.outcome("abort-retry", cursor).0);
    assert!(case.prefs().imports.is_empty());
}
#[test]
fn cold_partial_restart_needs_fresh_actual_grant_and_rejects_foreign_fid() {
    let mut case = Case::new();
    let fact = case.partial("cold");
    case.restart();
    let path = case.body(&fact.token);
    let cursor = case.backend().event_cursor();
    case.execute(
        "cold-no-selection",
        BackendCommand::AbortImpulseImport {
            token: fact.token.clone(),
            selection: None,
        },
    );
    assert!(!case.outcome("cold-no-selection", cursor).0);
    assert!(path.exists());
    let foreign = case.root().join("foreign.bin");
    std::fs::write(&foreign, b"foreign bytes").unwrap();
    let cursor = case.backend().event_cursor();
    case.execute(
        "cold-foreign",
        BackendCommand::AbortImpulseImport {
            token: fact.token.clone(),
            selection: Some(ImpulseAbortSelection {
                path: foreign.clone(),
                file_read_approved: true,
                file_write_approved: true,
            }),
        },
    );
    assert!(!case.outcome("cold-foreign", cursor).0);
    assert_eq!(std::fs::read(&foreign).unwrap(), b"foreign bytes");
    let cursor = case.backend().event_cursor();
    case.execute(
        "cold-current",
        BackendCommand::AbortImpulseImport {
            token: fact.token.clone(),
            selection: Some(ImpulseAbortSelection {
                path: path.clone(),
                file_read_approved: true,
                file_write_approved: true,
            }),
        },
    );
    assert!(case.outcome("cold-current", cursor).0);
    assert!(!path.exists());
}
#[test]
fn disposition_ack_store_freeze_retains_owner_until_actual_gate_repair_and_retry() {
    let case = Case::new();
    let fact = case.partial("store-freeze");
    case.hooks.freeze_pending.store(true, Ordering::Release);
    let cursor = case.backend().event_cursor();
    case.execute(
        "freeze-abort",
        BackendCommand::AbortImpulseImport {
            token: fact.token.clone(),
            selection: None,
        },
    );
    let failed = case.outcome("freeze-abort", cursor);
    assert!(!failed.0);
    assert!(failed.1.is_some());
    assert!(
        case.prefs()
            .imports
            .iter()
            .any(|entry| entry.token == fact.token)
    );
    case.hooks.request_repair.store(true, Ordering::Release);
    let end = Instant::now() + WAIT;
    while case.hooks.request_repair.load(Ordering::Acquire) {
        assert!(Instant::now() < end);
        std::thread::sleep(Duration::from_millis(10));
    }
    case.execute(
        "repaired-abort",
        BackendCommand::AbortImpulseImport {
            token: fact.token.clone(),
            selection: None,
        },
    );
    assert!(case.outcome("repaired-abort", cursor).0);
    assert!(case.prefs().imports.is_empty());
}

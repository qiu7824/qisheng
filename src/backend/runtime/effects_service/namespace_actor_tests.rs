#![cfg(windows)]
use super::*;
use crate::backend::{
    BackendConfig, BackendEvent, BackendHandle, BackendQuery, BackendQueryResult, CommandEnvelope,
};
use crate::repository::{ImpulseVaultCreationFact, ImpulseVaultCreationPhase};
use std::{
    collections::HashMap,
    path::Path,
    sync::{Condvar, OnceLock},
    time::{Duration, Instant},
};

const WAIT: Duration = Duration::from_secs(30);
#[derive(Default)]
struct Hooks {
    reject_phase: Mutex<Option<ImpulseVaultCreationPhase>>,
    gate_operation: Mutex<Option<String>>,
    gate: Mutex<(bool, bool)>,
    changed: Condvar,
}
static HOOKS: OnceLock<Mutex<HashMap<String, Arc<Hooks>>>> = OnceLock::new();
fn key(path: &Path) -> String {
    path.to_string_lossy()
        .replace("\\\\?\\", "")
        .replace('/', "\\")
        .to_ascii_lowercase()
}
pub(super) fn reject_creation(profile: &Path, fact: &ImpulseVaultCreationFact) -> bool {
    let hooks = HOOKS
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .get(&key(profile))
        .cloned();
    let Some(hooks) = hooks else {
        return false;
    };
    let mut rejected = hooks
        .reject_phase
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    if *rejected == Some(fact.phase) {
        rejected.take();
        true
    } else {
        false
    }
}
pub(in crate::backend::runtime) fn after_result(operation: &str) {
    let hooks = HOOKS
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .values()
        .find(|hooks| {
            hooks
                .gate_operation
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .as_deref()
                == Some(operation)
        })
        .cloned();
    if let Some(hooks) = hooks {
        let mut state = hooks.gate.lock().unwrap_or_else(|error| error.into_inner());
        state.0 = true;
        hooks.changed.notify_all();
        while !state.1 {
            state = hooks
                .changed
                .wait(state)
                .unwrap_or_else(|error| error.into_inner());
        }
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
    fn namespace(&self) -> PathBuf {
        self.profile().join(".lx-impulse-vault")
    }
    fn owner(&self) -> PathBuf {
        self.namespace().join("owner.lock")
    }
    fn backend(&self) -> &BackendHandle {
        self.backend.as_ref().unwrap()
    }
    fn execute(&self, id: &str, command: BackendCommand) -> crate::backend::CommandReceipt {
        self.backend()
            .execute(CommandEnvelope {
                request_id: id.into(),
                origin: "namespace-lifecycle-test".into(),
                expected_revision: None,
                command,
            })
            .unwrap()
    }
    fn preferences(&self) -> EffectsPreferences {
        let BackendQueryResult::ImpulseResources(preferences) = self
            .backend()
            .query(BackendQuery::ImpulseResources)
            .unwrap()
        else {
            panic!("Wrong query kind");
        };
        preferences
    }
    fn outcome(&self, id: &str, cursor: u64) -> bool {
        let deadline = Instant::now() + WAIT;
        loop {
            for event in self.backend().events_after(cursor).unwrap() {
                if let BackendEvent::OperationFinished {
                    operation_id,
                    success,
                    ..
                } = event.payload
                    && operation_id == id
                {
                    return success;
                }
            }
            assert!(
                Instant::now() < deadline,
                "Namespace operation has no terminal event"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    fn selection(&self, owner: bool) -> ImpulseNamespaceSelection {
        ImpulseNamespaceSelection {
            directory_path: self.namespace(),
            directory_read_approved: true,
            directory_write_approved: true,
            owner: owner.then(|| ImpulseAbortSelection {
                path: self.owner(),
                file_read_approved: true,
                file_write_approved: true,
            }),
        }
    }
    fn import(&self, id: &str) -> bool {
        let path = self.root().join(format!("{id}.wav"));
        std::fs::write(&path, wave()).unwrap();
        let cursor = self.backend().event_cursor();
        self.execute(
            id,
            BackendCommand::ImportImpulseResource {
                path,
                file_read_approved: true,
                timeout_ms: Some(30000),
            },
        );
        self.outcome(id, cursor)
    }
    fn obligation(&self, phase: ImpulseVaultCreationPhase) -> ImpulseVaultCreationFact {
        *self.hooks.reject_phase.lock().unwrap() = Some(phase);
        assert!(!self.import("interrupted-create"));
        let preferences = self.preferences();
        assert!(preferences.vault.is_none());
        assert!(preferences.imports.is_empty());
        preferences.creation.unwrap()
    }
    fn recover(&self, id: &str, selection: ImpulseNamespaceSelection) -> bool {
        let cursor = self.backend().event_cursor();
        self.execute(
            id,
            BackendCommand::RecoverImpulseNamespace {
                selection,
                timeout_ms: Some(30000),
            },
        );
        self.outcome(id, cursor)
    }
    fn restart(&mut self) {
        let backend = self.backend.take().unwrap();
        if let Err(error) = backend.shutdown(WAIT) {
            self.backend = Some(backend);
            panic!("Actual namespace owner retirement failed: {error}");
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
        self.hooks
            .gate
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .1 = true;
        self.hooks.changed.notify_all();
        if let Some(backend) = self.backend.take()
            && let Err(error) = backend.shutdown(WAIT)
        {
            if let Some(temp) = self.temp.take() {
                let _ = temp.keep();
            }
            if !std::thread::panicking() {
                panic!("Actual namespace owner retirement failed: {error}");
            }
            return;
        }
        HOOKS
            .get()
            .unwrap()
            .lock()
            .unwrap_or_else(|error| error.into_inner())
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
fn cold_unknown_directory_public_recovery_needs_actual_selection_and_worker_join() {
    let mut case = Case::new();
    let fact = case.obligation(ImpulseVaultCreationPhase::DirectoryPinned);
    assert!(fact.directory_identity.is_none());
    assert!(!case.owner().exists());
    case.restart();
    assert!(!case.import("unapproved-cold-directory"));
    assert_eq!(case.preferences().creation.as_ref(), Some(&fact));
    let foreign = case.root().join("foreign-directory");
    std::fs::create_dir(&foreign).unwrap();
    let mut wrong = case.selection(false);
    wrong.directory_path = foreign.clone();
    assert!(!case.recover("foreign-directory-selection", wrong));
    assert!(std::fs::read_dir(&foreign).unwrap().next().is_none());
    assert!(!case.owner().exists());
    *case.hooks.gate_operation.lock().unwrap() = Some("recover-joined-namespace".into());
    let cursor = case.backend().event_cursor();
    case.execute(
        "recover-joined-namespace",
        BackendCommand::RecoverImpulseNamespace {
            selection: case.selection(false),
            timeout_ms: Some(30000),
        },
    );
    let deadline = Instant::now() + WAIT;
    while !case.hooks.gate.lock().unwrap().0 {
        assert!(
            Instant::now() < deadline,
            "Namespace producer did not reach actual join gate"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    let durable = case.preferences();
    assert!(durable.creation.is_none());
    assert!(durable.vault.is_some());
    assert!(!case.backend().events_after(cursor).unwrap().iter().any(|event|
        matches!(&event.payload, BackendEvent::OperationFinished {operation_id, ..} if operation_id == "recover-joined-namespace")));
    case.hooks.gate.lock().unwrap().1 = true;
    case.hooks.changed.notify_all();
    assert!(case.outcome("recover-joined-namespace", cursor));
    assert!(case.import("after-directory-recovery"));
    case.restart();
    assert!(case.preferences().vault.is_some());
    assert_eq!(case.preferences().resources.len(), 1);
}

#[test]
fn cold_unknown_owner_public_recovery_retains_selected_original_after_failed_ack() {
    let mut case = Case::new();
    let fact = case.obligation(ImpulseVaultCreationPhase::OwnerAllocated);
    assert!(fact.owner_identity.is_none());
    assert_eq!(std::fs::metadata(case.owner()).unwrap().len(), 0);
    case.restart();
    assert!(!case.import("unapproved-cold-owner"));
    assert!(!case.recover("missing-owner-selection", case.selection(false)));
    let foreign = case.root().join("foreign-owner.lock");
    std::fs::write(&foreign, []).unwrap();
    let mut wrong = case.selection(true);
    wrong.owner.as_mut().unwrap().path = foreign.clone();
    assert!(!case.recover("foreign-owner-selection", wrong));
    assert_eq!(std::fs::metadata(&foreign).unwrap().len(), 0);
    assert_eq!(std::fs::metadata(case.owner()).unwrap().len(), 0);
    assert_eq!(case.preferences().creation.as_ref(), Some(&fact));
    *case.hooks.reject_phase.lock().unwrap() = Some(ImpulseVaultCreationPhase::OwnerAllocated);
    assert!(!case.recover("recovery-ack-failed", case.selection(true)));
    assert!(
        std::fs::rename(case.owner(), case.root().join("displaced.lock")).is_err(),
        "Actual selected writer was dropped after failed Store ACK"
    );
    assert_eq!(case.preferences().creation.as_ref(), Some(&fact));
    assert!(case.recover("retained-owner-retry", case.selection(false)));
    assert!(case.preferences().creation.is_none());
    assert!(case.preferences().vault.is_some());
    assert!(case.import("after-owner-recovery"));
}

#[test]
fn public_namespace_recovery_rejects_same_fid_foreign_body_and_missing_approvals() {
    let mut case = Case::new();
    let fact = case.obligation(ImpulseVaultCreationPhase::OwnerAllocated);
    case.restart();
    let mut denied = case.selection(true);
    denied.directory_write_approved = false;
    assert!(
        case.backend()
            .execute(CommandEnvelope {
                request_id: "denied-dir".into(),
                origin: "test".into(),
                expected_revision: None,
                command: BackendCommand::RecoverImpulseNamespace {
                    selection: denied,
                    timeout_ms: None
                }
            })
            .is_err()
    );
    let mut denied = case.selection(true);
    denied.owner.as_mut().unwrap().file_write_approved = false;
    assert!(
        case.backend()
            .execute(CommandEnvelope {
                request_id: "denied-owner".into(),
                origin: "test".into(),
                expected_revision: None,
                command: BackendCommand::RecoverImpulseNamespace {
                    selection: denied,
                    timeout_ms: None
                }
            })
            .is_err()
    );
    std::fs::write(case.owner(), b"foreign owner representation").unwrap();
    assert!(!case.recover("same-fid-foreign-body", case.selection(true)));
    assert_eq!(
        std::fs::read(case.owner()).unwrap(),
        b"foreign owner representation"
    );
    assert_eq!(case.preferences().creation.as_ref(), Some(&fact));
    assert!(case.preferences().vault.is_none());
}

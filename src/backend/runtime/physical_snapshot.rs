//! Tracked physical snapshot workers retain native owners until verified retirement.

use super::{Completion, Runtime};
use crate::repository::{
    Repository,
    physical_snapshot_ledger::{
        PhysicalSnapshotLedgerContext, PhysicalSnapshotLedgerJournal, PhysicalSnapshotLedgerRecord,
        PhysicalSnapshotLedgerState,
    },
    snapshot_export::{
        MAX_PHYSICAL_SNAPSHOT_BYTES, PhysicalSnapshotContext, PhysicalSnapshotFailure,
        PhysicalSnapshotFailureKind, PhysicalSnapshotLimits, PhysicalSnapshotReceipt,
        PhysicalSnapshotRecoveryAction, PhysicalSnapshotRequest, PhysicalSnapshotSelection,
        PreparedPhysicalSnapshot,
    },
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

const MAX_PENDING: usize = 4;
const MAX_HISTORY: usize = 64;
const AGGREGATE_BYTES: u64 = 4 * 1024 * 1024 * 1024;
const WORK_TIMEOUT: Duration = Duration::from_secs(120);
const RETIRE_RETRY: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PhysicalSnapshotRecoveryMode {
    ResumePublication,
    CleanupPrivate,
}
impl From<PhysicalSnapshotRecoveryMode> for PhysicalSnapshotRecoveryAction {
    fn from(value: PhysicalSnapshotRecoveryMode) -> Self {
        match value {
            PhysicalSnapshotRecoveryMode::ResumePublication => Self::ResumePublication,
            PhysicalSnapshotRecoveryMode::CleanupPrivate => Self::CleanupPrivate,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PhysicalSnapshotFailureCategory {
    Cancelled,
    Deadline,
    Quota,
    Conflict,
    Unsupported,
    Changed,
    Io,
    PublishedUnconfirmed,
}
impl From<PhysicalSnapshotFailureKind> for PhysicalSnapshotFailureCategory {
    fn from(value: PhysicalSnapshotFailureKind) -> Self {
        match value {
            PhysicalSnapshotFailureKind::Cancelled => Self::Cancelled,
            PhysicalSnapshotFailureKind::Deadline => Self::Deadline,
            PhysicalSnapshotFailureKind::Quota => Self::Quota,
            PhysicalSnapshotFailureKind::Conflict => Self::Conflict,
            PhysicalSnapshotFailureKind::Unsupported => Self::Unsupported,
            PhysicalSnapshotFailureKind::Changed => Self::Changed,
            PhysicalSnapshotFailureKind::Io => Self::Io,
            PhysicalSnapshotFailureKind::PublishedUnconfirmed => Self::PublishedUnconfirmed,
        }
    }
}
#[derive(Clone, Debug, Serialize)]
pub struct PhysicalSnapshotErrorView {
    pub kind: PhysicalSnapshotFailureCategory,
    pub message: String,
}
#[derive(Clone, Debug, Serialize)]
pub struct PhysicalSnapshotView {
    pub operation_id: String,
    pub state: String,
    pub snapshot_revision: u64,
    pub maximum_image_bytes: u64,
    pub basename: String,
    pub path: Option<PathBuf>,
    pub receipt: Option<PhysicalSnapshotReceipt>,
    pub pending_recovery: bool,
    pub has_live_owner: bool,
    pub error: Option<PhysicalSnapshotErrorView>,
}

struct Entry {
    view: PhysicalSnapshotView,
    ledger_context: PhysicalSnapshotLedgerContext,
    owner: Option<Box<PreparedPhysicalSnapshot>>,
    context: Option<PhysicalSnapshotContext>,
    running_operation: String,
    completion: Arc<Mutex<Option<Settled>>>,
    retirement_retry: Option<Instant>,
    release_retry: Option<Instant>,
    shutdown_attempts: u32,
}
struct Settled {
    operation: Option<String>,
    outcome: Outcome,
}
enum Outcome {
    Completed {
        receipt: Option<PhysicalSnapshotReceipt>,
        cleaned: bool,
    },
    Failed(PhysicalSnapshotFailure),
}

pub(super) struct PhysicalSnapshotRuntime {
    entries: HashMap<String, Entry>,
    limits: PhysicalSnapshotLimits,
    closing: bool,
}
impl PhysicalSnapshotRuntime {
    pub(super) fn new() -> Result<Self> {
        Ok(Self {
            entries: HashMap::new(),
            limits: PhysicalSnapshotLimits::new(AGGREGATE_BYTES)?,
            closing: false,
        })
    }
    /// Restores quota and descriptive state without opening destination writers.
    pub(super) fn restore(repository: &Arc<Repository>) -> Result<Self> {
        let mut state = Self::new()?;
        let records = repository.physical_snapshot_records()?;
        ensure!(
            records.len() <= MAX_PENDING + MAX_HISTORY,
            "Snapshot ledger exceeds bounded history"
        );
        let pending = records
            .iter()
            .filter(|record| record.state != PhysicalSnapshotLedgerState::Finished)
            .collect::<Vec<_>>();
        ensure!(
            pending.len() <= MAX_PENDING,
            "Snapshot ledger exceeds pending capacity"
        );
        let locators = pending
            .iter()
            .map(|record| record.recovery.locator.clone())
            .collect::<Vec<_>>();
        repository.restore_pending_physical_snapshot_reservations(
            &locators,
            &state.limits,
            &|locator| {
                let record = pending
                    .iter()
                    .find(|record| record.context.operation_id == locator.operation_id)
                    .context("Pending snapshot has no trusted ledger context")?;
                PhysicalSnapshotLedgerJournal::new(repository.clone(), record.context.clone())?
                    .authorize_locator(locator)
            },
        )?;
        for record in records {
            let finished = record.state == PhysicalSnapshotLedgerState::Finished;
            let receipt = record.recovery.publication.clone();
            let operation_id = record.context.operation_id.clone();
            let view = view_from_record(&record, finished, receipt);
            ensure!(
                !state.entries.contains_key(&operation_id),
                "Duplicate snapshot ledger operation"
            );
            state.entries.insert(
                operation_id.clone(),
                Entry {
                    view,
                    ledger_context: record.context,
                    owner: None,
                    context: None,
                    running_operation: operation_id,
                    completion: Arc::new(Mutex::new(None)),
                    retirement_retry: None,
                    release_retry: None,
                    shutdown_attempts: 0,
                },
            );
        }
        Ok(state)
    }
    fn admit(&mut self) -> Result<()> {
        ensure!(!self.closing, "Physical snapshots are closing");
        ensure!(
            self.retained_count() < MAX_PENDING,
            "Four physical snapshots already own pending resources"
        );
        let finished = self
            .entries
            .values()
            .filter(|entry| !entry.view.pending_recovery)
            .count();
        if finished >= MAX_HISTORY {
            let removable = self
                .entries
                .iter()
                .filter(|(_, entry)| {
                    !entry.view.pending_recovery && entry.context.is_none() && entry.owner.is_none()
                })
                .min_by_key(|(_, entry)| entry.ledger_context.operation_epoch)
                .map(|(id, _)| id.clone())
                .context("Physical snapshot history has no settled entry")?;
            self.entries.remove(&removable);
        }
        Ok(())
    }
    pub(super) fn cancel(&mut self, operation: &str) {
        for entry in self.entries.values() {
            if (entry.running_operation == operation || entry.view.operation_id == operation)
                && let Some(context) = &entry.context
            {
                context.cancel();
            }
        }
    }
    pub(super) fn begin_shutdown(&mut self) {
        self.closing = true;
        for entry in self.entries.values() {
            if let Some(context) = &entry.context {
                context.cancel();
            }
        }
    }
    pub(super) fn has_running(&self) -> bool {
        self.entries.values().any(|entry| entry.context.is_some())
    }
    pub(super) fn has_live_owners(&self) -> bool {
        self.entries.values().any(|entry| entry.owner.is_some())
    }
    pub(super) fn retained_count(&self) -> usize {
        self.entries
            .values()
            .filter(|entry| entry.view.pending_recovery)
            .count()
    }
    /// A false result requires the actor to retain this entire runtime alive.
    pub(super) fn retire_handles_for_shutdown(&mut self) -> Result<bool> {
        ensure!(
            self.closing,
            "Snapshot shutdown retirement requires closed admission"
        );
        if self.has_running() {
            return Ok(false);
        }
        for entry in self.entries.values_mut() {
            if entry.release_retry.is_some_and(|at| Instant::now() < at) {
                continue;
            }
            let Some(owner) = entry.owner.take() else {
                continue;
            };
            match (*owner).release_handles_for_recovery() {
                Ok(_) => {
                    entry.view.has_live_owner = false;
                    entry.view.pending_recovery = true;
                    entry.view.state = "recovery_needed".into();
                    entry.retirement_retry = None;
                    entry.release_retry = None;
                }
                Err(failure) => {
                    entry.view.error = Some(error_view(failure.kind, failure.message));
                    entry.owner = failure.owner;
                    entry.view.has_live_owner = entry.owner.is_some();
                    entry.release_retry = Some(Instant::now() + RETIRE_RETRY);
                    ensure!(
                        entry.owner.is_some(),
                        "Native retirement rejected without preserving its actual owner"
                    );
                }
            }
        }
        Ok(!self.has_live_owners())
    }
}

fn view_from_record(
    record: &PhysicalSnapshotLedgerRecord,
    finished: bool,
    receipt: Option<PhysicalSnapshotReceipt>,
) -> PhysicalSnapshotView {
    PhysicalSnapshotView {
        operation_id: record.context.operation_id.clone(),
        state: if finished {
            if receipt.is_some() {
                "published"
            } else {
                "cleaned"
            }
        } else {
            "recovery_needed"
        }
        .into(),
        snapshot_revision: record.context.expected_snapshot_revision,
        maximum_image_bytes: record.context.maximum_image_bytes,
        basename: record.context.basename.clone(),
        path: receipt.as_ref().map(|receipt| receipt.path.clone()),
        receipt,
        pending_recovery: !finished,
        has_live_owner: false,
        error: None,
    }
}
fn error_view(kind: PhysicalSnapshotFailureKind, message: String) -> PhysicalSnapshotErrorView {
    PhysicalSnapshotErrorView {
        kind: kind.into(),
        message: message.chars().take(4096).collect(),
    }
}
fn failure(error: anyhow::Error, owner: Option<Box<PreparedPhysicalSnapshot>>) -> Outcome {
    Outcome::Failed(PhysicalSnapshotFailure {
        kind: if owner
            .as_ref()
            .is_some_and(|owner| owner.published_receipt().is_some())
        {
            PhysicalSnapshotFailureKind::PublishedUnconfirmed
        } else {
            PhysicalSnapshotFailureKind::Io
        },
        message: format!("{error:#}"),
        owner,
    })
}
fn preparation_boundary<T>(work: impl FnOnce() -> Result<T>) -> Result<T> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(work)).unwrap_or_else(|_| {
        Err(anyhow::anyhow!(
            "Physical snapshot worker preparation panicked"
        ))
    })
}
fn take_owner(
    slot: &Arc<Mutex<Option<Box<PreparedPhysicalSnapshot>>>>,
) -> Option<Box<PreparedPhysicalSnapshot>> {
    slot.lock()
        .unwrap_or_else(|error| error.into_inner())
        .take()
}
fn publish_and_acknowledge(
    owner: PreparedPhysicalSnapshot,
    context: &PhysicalSnapshotContext,
    journal: &PhysicalSnapshotLedgerJournal,
) -> Outcome {
    let result = if owner.published_receipt().is_some() {
        owner.acknowledge_published(context, &|receipt| journal.commit_receipt(receipt), journal)
    } else {
        match owner.publish(context, journal) {
            Ok(published) => {
                published.acknowledge(context, &|receipt| journal.commit_receipt(receipt), journal)
            }
            Err(error) => return Outcome::Failed(error),
        }
    };
    match result {
        Ok(receipt) => Outcome::Completed {
            receipt: Some(receipt),
            cleaned: false,
        },
        Err(error) => Outcome::Failed(error),
    }
}
struct SnapshotRecoveryRequest {
    ledger_context: PhysicalSnapshotLedgerContext,
    directory: PathBuf,
    selection_epoch: u64,
    mode: PhysicalSnapshotRecoveryMode,
}
fn run_recovery(
    repository: &Arc<Repository>,
    request: SnapshotRecoveryRequest,
    context: &PhysicalSnapshotContext,
    limits: &PhysicalSnapshotLimits,
    owner_slot: &Arc<Mutex<Option<Box<PreparedPhysicalSnapshot>>>>,
) -> Outcome {
    let SnapshotRecoveryRequest {
        ledger_context,
        directory,
        selection_epoch,
        mode,
    } = request;
    // Native owners stay in the admission slot while metadata/selection code can unwind.
    let (journal, selection) = match preparation_boundary(|| {
        let journal =
            PhysicalSnapshotLedgerJournal::new(repository.clone(), ledger_context.clone())?;
        let selection = PhysicalSnapshotSelection::open_selected(directory, selection_epoch)?;
        Ok((journal, selection))
    }) {
        Ok(prepared) => prepared,
        Err(error) => return failure(error, take_owner(owner_slot)),
    };
    let action = PhysicalSnapshotRecoveryAction::from(mode);
    let mut owner = match take_owner(owner_slot) {
        Some(owner) => owner,
        None => {
            let preview = match preparation_boundary(|| {
                let record = repository
                    .physical_snapshot_record(&ledger_context.operation_id)?
                    .context("Snapshot recovery ledger disappeared")?;
                repository.inspect_physical_snapshot_recovery(record.recovery, &selection, context)
            }) {
                Ok(preview) => preview,
                Err(error) => return failure(error, None),
            };
            match repository.recover_physical_snapshot(preview, context, limits, &|preview| {
                journal.authorize(preview, action)
            }) {
                Ok(owner) => Box::new(owner),
                Err(error) => return Outcome::Failed(error),
            }
        }
    };
    if owner.recovery_locator().is_some() {
        if let Err(error) =
            owner.ensure_recovery_registered(&|locator| journal.begin_registration(locator))
        {
            return failure(error, Some(owner));
        }
    } else if mode == PhysicalSnapshotRecoveryMode::CleanupPrivate {
        return match (*owner).cleanup(context, &journal) {
            Ok(()) => Outcome::Completed {
                receipt: None,
                cleaned: true,
            },
            Err(error) => Outcome::Failed(error),
        };
    }
    match mode {
        PhysicalSnapshotRecoveryMode::CleanupPrivate => {
            match (*owner).cleanup_in(&selection, context, &journal, &|locator| {
                journal.authorize_live_locator(locator, action)
            }) {
                Ok(()) => Outcome::Completed {
                    receipt: None,
                    cleaned: true,
                },
                Err(error) => Outcome::Failed(error),
            }
        }
        PhysicalSnapshotRecoveryMode::ResumePublication => {
            if owner.published_receipt().is_some() {
                match (*owner).acknowledge_published_in(
                    &selection,
                    context,
                    &|receipt| journal.commit_receipt(receipt),
                    &journal,
                    &|locator| journal.authorize_live_locator(locator, action),
                ) {
                    Ok(receipt) => Outcome::Completed {
                        receipt: Some(receipt),
                        cleaned: false,
                    },
                    Err(error) => Outcome::Failed(error),
                }
            } else {
                match (*owner).publish_in(&selection, context, &journal, &|locator| {
                    journal.authorize_live_locator(locator, action)
                }) {
                    Ok(published) => match published.acknowledge(
                        context,
                        &|receipt| journal.commit_receipt(receipt),
                        &journal,
                    ) {
                        Ok(receipt) => Outcome::Completed {
                            receipt: Some(receipt),
                            cleaned: false,
                        },
                        Err(error) => Outcome::Failed(error),
                    },
                    Err(error) => Outcome::Failed(error),
                }
            }
        }
    }
}
fn run_shutdown_retirement(
    repository: &Arc<Repository>,
    ledger_context: PhysicalSnapshotLedgerContext,
    context: &PhysicalSnapshotContext,
    owner_slot: &Arc<Mutex<Option<Box<PreparedPhysicalSnapshot>>>>,
) -> Outcome {
    let journal = match preparation_boundary(|| {
        PhysicalSnapshotLedgerJournal::new(repository.clone(), ledger_context)
    }) {
        Ok(journal) => journal,
        Err(error) => return failure(error, take_owner(owner_slot)),
    };
    let Some(mut owner) = take_owner(owner_slot) else {
        return failure(anyhow::anyhow!("Snapshot shutdown owner disappeared"), None);
    };
    if owner.recovery_locator().is_some()
        && let Err(error) =
            owner.ensure_recovery_registered(&|locator| journal.begin_registration(locator))
    {
        return failure(error, Some(owner));
    }
    if owner.published_receipt().is_some() {
        publish_and_acknowledge(*owner, context, &journal)
    } else {
        // The original continuously owned descriptors retain the existing cleanup scope.
        // This never opens a fresh selected directory or deletes a published final image.
        match (*owner).cleanup(context, &journal) {
            Ok(()) => Outcome::Completed {
                receipt: None,
                cleaned: true,
            },
            Err(error) => Outcome::Failed(error),
        }
    }
}

impl Runtime {
    pub(super) fn begin_physical_snapshot(
        &mut self,
        directory: PathBuf,
        basename: String,
        maximum_image_bytes: u64,
    ) -> Result<()> {
        self.physical_snapshots.admit()?;
        ensure!(
            self.tasks.len() < MAX_PENDING,
            "Too many pending file operations"
        );
        ensure!(
            (1..=MAX_PHYSICAL_SNAPSHOT_BYTES).contains(&maximum_image_bytes),
            "Physical snapshot image limit must be at most 1 GiB"
        );
        ensure!(
            !self
                .physical_snapshots
                .entries
                .contains_key(&self.request_id),
            "Physical snapshot operation already exists"
        );
        let (profile_id, profile_epoch) = self.repository.physical_snapshot_profile_scope()?;
        let operation_epoch = self.repository.physical_snapshot_next_operation_epoch()?;
        let selection_epoch = self.repository.physical_snapshot_next_selection_epoch()?;
        let operation = self.request_id.clone();
        let ledger_context = PhysicalSnapshotLedgerContext {
            operation_id: operation.clone(),
            profile_id,
            profile_epoch,
            operation_epoch,
            selection_epoch,
            expected_snapshot_revision: self.revision,
            maximum_image_bytes,
            basename: basename.clone(),
        };
        let context = PhysicalSnapshotContext::new(WORK_TIMEOUT)?;
        let completion = Arc::new(Mutex::new(None));
        // Create the retained entry before spawning, including its durable completion slot.
        self.physical_snapshots.entries.insert(
            operation.clone(),
            Entry {
                view: PhysicalSnapshotView {
                    operation_id: operation.clone(),
                    state: "running".into(),
                    snapshot_revision: self.revision,
                    maximum_image_bytes,
                    basename: basename.clone(),
                    path: Some(directory.join(&basename)),
                    receipt: None,
                    pending_recovery: true,
                    has_live_owner: false,
                    error: None,
                },
                ledger_context: ledger_context.clone(),
                owner: None,
                context: Some(context.clone()),
                running_operation: operation.clone(),
                completion: completion.clone(),
                retirement_retry: None,
                release_retry: None,
                shutdown_attempts: 0,
            },
        );
        let repository = self.repository.clone();
        let limits = self.physical_snapshots.limits.clone();
        let wake = self.completed_tx.clone();
        let worker_operation = operation.clone();
        let worker_context = context;
        let expected_revision = self.revision;
        let spawned = thread::Builder::new()
            .name("lx-physical-snapshot".into())
            .spawn(move || {
                let outcome = (|| {
                    let (selection, journal) = match preparation_boundary(|| {
                        let selection =
                            PhysicalSnapshotSelection::open_selected(directory, selection_epoch)?;
                        let journal =
                            PhysicalSnapshotLedgerJournal::new(repository.clone(), ledger_context)?;
                        Ok((selection, journal))
                    }) {
                        Ok(prepared) => prepared,
                        Err(error) => return failure(error, None),
                    };
                    let request = PhysicalSnapshotRequest {
                        operation_id: worker_operation.clone(),
                        expected_revision,
                        maximum_image_bytes,
                        basename,
                    };
                    match repository.prepare_physical_snapshot(
                        request,
                        &selection,
                        &worker_context,
                        &limits,
                        &|locator| journal.begin_registration(locator),
                    ) {
                        Ok(owner) => publish_and_acknowledge(owner, &worker_context, &journal),
                        Err(error) => Outcome::Failed(error),
                    }
                })();
                *completion.lock().unwrap_or_else(|error| error.into_inner()) = Some(Settled {
                    operation: Some(worker_operation),
                    outcome,
                });
                let _ = wake.try_send(Completion::PhysicalSnapshotWake);
            });
        match spawned {
            Ok(task) => {
                self.tasks.push(task);
                self.async_started = true;
                Ok(())
            }
            Err(error) => {
                // No worker ran, no native owner or reservation exists.
                self.physical_snapshots.entries.remove(&operation);
                Err(error.into())
            }
        }
    }
    pub(super) fn recover_physical_snapshot_operation(
        &mut self,
        snapshot_operation_id: String,
        directory: PathBuf,
        mode: PhysicalSnapshotRecoveryMode,
    ) -> Result<()> {
        ensure!(
            !self.physical_snapshots.closing && self.tasks.len() < MAX_PENDING,
            "Physical snapshot recovery is unavailable"
        );
        let entry = self
            .physical_snapshots
            .entries
            .get_mut(&snapshot_operation_id)
            .context("Unknown physical snapshot operation")?;
        ensure!(
            entry.context.is_none() && entry.view.pending_recovery,
            "Physical snapshot is running or already settled"
        );
        let selection_epoch = self.repository.physical_snapshot_next_selection_epoch()?;
        let context = PhysicalSnapshotContext::new(WORK_TIMEOUT)?;
        let operation = self.request_id.clone();
        let worker_operation = operation.clone();
        let slot = Arc::new(Mutex::new(entry.owner.take()));
        let worker_slot = slot.clone();
        let ledger_context = entry.ledger_context.clone();
        let completion = entry.completion.clone();
        let repository = self.repository.clone();
        let limits = self.physical_snapshots.limits.clone();
        let worker_context = context.clone();
        let wake = self.completed_tx.clone();
        let spawned = thread::Builder::new()
            .name("lx-physical-snapshot-recovery".into())
            .spawn(move || {
                let outcome = run_recovery(
                    &repository,
                    SnapshotRecoveryRequest {
                        ledger_context,
                        directory,
                        selection_epoch,
                        mode,
                    },
                    &worker_context,
                    &limits,
                    &worker_slot,
                );
                *completion.lock().unwrap_or_else(|error| error.into_inner()) = Some(Settled {
                    operation: Some(worker_operation),
                    outcome,
                });
                let _ = wake.try_send(Completion::PhysicalSnapshotWake);
            });
        match spawned {
            Ok(task) => {
                entry.context = Some(context);
                entry.running_operation = operation;
                entry.view.state = "running".into();
                entry.view.error = None;
                entry.view.has_live_owner = false;
                entry.retirement_retry = None;
                entry.release_retry = None;
                self.tasks.push(task);
                self.async_started = true;
                Ok(())
            }
            Err(error) => {
                entry.owner = slot
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .take();
                entry.view.has_live_owner = entry.owner.is_some();
                Err(error.into())
            }
        }
    }
    pub(super) fn physical_snapshot_view(
        &self,
        operation_id: &str,
    ) -> Option<PhysicalSnapshotView> {
        self.physical_snapshots
            .entries
            .get(operation_id)
            .map(|entry| entry.view.clone())
    }
    pub(super) fn physical_snapshot_views(&self) -> Vec<PhysicalSnapshotView> {
        let mut entries = self.physical_snapshots.entries.values().collect::<Vec<_>>();
        entries.sort_by_key(|entry| entry.ledger_context.operation_epoch);
        entries
            .into_iter()
            .map(|entry| entry.view.clone())
            .collect()
    }
    /// Shared task owners must be joined/reaped before another retirement task is admitted.
    pub(super) fn retire_physical_snapshot_owners(&mut self) -> Result<bool> {
        if self.physical_snapshots.retire_handles_for_shutdown()? {
            return Ok(true);
        }
        if self.tasks.len() >= MAX_PENDING {
            return Ok(false);
        }
        let now = Instant::now();
        let retry = self
            .physical_snapshots
            .entries
            .iter()
            .filter(|(_, entry)| entry.owner.is_some() && entry.context.is_none())
            .filter(|(_, entry)| {
                entry.shutdown_attempts == 0 || entry.retirement_retry.is_none_or(|at| now >= at)
            })
            .min_by_key(|(_, entry)| entry.ledger_context.operation_epoch)
            .map(|(id, _)| id.clone());
        let Some(id) = retry else { return Ok(false) };
        let context = PhysicalSnapshotContext::new(WORK_TIMEOUT)?;
        let entry = self
            .physical_snapshots
            .entries
            .get_mut(&id)
            .expect("Retirement entry remains owned");
        let slot = Arc::new(Mutex::new(entry.owner.take()));
        let worker_slot = slot.clone();
        let repository = self.repository.clone();
        let ledger_context = entry.ledger_context.clone();
        let worker_context = context.clone();
        let completion = entry.completion.clone();
        let wake = self.completed_tx.clone();
        let spawned = thread::Builder::new()
            .name("lx-physical-snapshot-retire".into())
            .spawn(move || {
                let outcome = run_shutdown_retirement(
                    &repository,
                    ledger_context,
                    &worker_context,
                    &worker_slot,
                );
                *completion.lock().unwrap_or_else(|error| error.into_inner()) = Some(Settled {
                    operation: None,
                    outcome,
                });
                let _ = wake.try_send(Completion::PhysicalSnapshotWake);
            });
        match spawned {
            Ok(task) => {
                entry.context = Some(context);
                entry.view.has_live_owner = false;
                entry.shutdown_attempts = entry.shutdown_attempts.saturating_add(1);
                entry.retirement_retry = None;
                self.tasks.push(task);
            }
            Err(error) => {
                entry.owner = take_owner(&slot);
                entry.view.has_live_owner = entry.owner.is_some();
                entry.shutdown_attempts = entry.shutdown_attempts.saturating_add(1);
                entry.retirement_retry = Some(now + RETIRE_RETRY);
                return Err(error.into());
            }
        }
        Ok(false)
    }
    pub(super) fn poll_physical_snapshots(&mut self) {
        let mut settled = Vec::with_capacity(MAX_PENDING);
        for (id, entry) in &self.physical_snapshots.entries {
            if let Some(outcome) = entry
                .completion
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .take()
            {
                settled.push((id.clone(), outcome));
            }
        }
        for (id, settled) in settled {
            let closing = self.physical_snapshots.closing;
            let entry = self
                .physical_snapshots
                .entries
                .get_mut(&id)
                .expect("Completion retains its entry");
            entry.context = None;
            let result = match settled.outcome {
                Outcome::Completed { receipt, cleaned } => {
                    entry.owner = None;
                    entry.view.has_live_owner = false;
                    entry.view.pending_recovery = false;
                    entry.view.state = if cleaned { "cleaned" } else { "published" }.into();
                    entry.view.path = receipt
                        .as_ref()
                        .map(|receipt| receipt.path.clone())
                        .or_else(|| entry.view.path.clone());
                    entry.view.receipt = receipt;
                    entry.view.error = None;
                    Ok(())
                }
                Outcome::Failed(failure) => {
                    entry.owner = failure.owner;
                    let record = self.repository.physical_snapshot_record(&id);
                    let durable_pending = match &record {
                        Ok(Some(record)) => record.state != PhysicalSnapshotLedgerState::Finished,
                        Ok(None) => false,
                        Err(_) => true,
                    };
                    if let Ok(Some(record)) = record {
                        entry.view.receipt = record.recovery.publication;
                    }
                    if let Some(receipt) = entry
                        .owner
                        .as_ref()
                        .and_then(|owner| owner.published_receipt())
                    {
                        entry.view.receipt = Some(receipt.clone());
                    }
                    entry.view.has_live_owner = entry.owner.is_some();
                    entry.view.pending_recovery = entry.owner.is_some() || durable_pending;
                    entry.view.state = if entry.view.receipt.is_some() {
                        "published_unconfirmed"
                    } else {
                        "failed"
                    }
                    .into();
                    entry.view.error = Some(error_view(failure.kind, failure.message.clone()));
                    if closing && entry.shutdown_attempts > 0 {
                        entry.retirement_retry = Some(Instant::now() + RETIRE_RETRY);
                    }
                    Err(anyhow::anyhow!(failure.message))
                }
            };
            if let Some(operation) = settled.operation {
                self.publish_finished(operation, &result);
            }
        }
    }
}

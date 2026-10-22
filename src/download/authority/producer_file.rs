//! One kept native object spans allocation, transfer and publication.

use super::*;
use crate::audio::worker_owner::TrackedWorker;
use std::{
    cell::{Cell, RefCell},
    fs::Metadata,
    sync::{
        OnceLock,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

const CAPACITY: usize = crate::download::MAX_DOWNLOAD_TASKS + crate::download::MAX_DOWNLOAD_WORKERS;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ProducerPhase {
    Reserved,
    IntentAcknowledged,
    NativeAttempted,
    NativeCaptured,
    CreationAcknowledged,
    AdoptionWriterClosed,
    HandleClosed,
    NamespaceRemoved,
    RecoveryRetired,
}

struct NativeState {
    plan: DownloadPlan,
    basename: String,
    native_name: String,
    nonce: String,
    maximum_bytes: u64,
    charged_bytes: u64,
    charge: Arc<AtomicU64>,
    quota: Option<crate::download::quota::PartialQuota>,
    context: Option<crate::download::DownloadContext>,
    kind: crate::download::artifact::OwnedArtifactKind,
    phase: ProducerPhase,
    creation_acknowledged: bool,
    file: Option<File>,
    record: Option<OwnedPartRecord>,
    removed_name: Option<String>,
    adoption_proof: Option<(u64, String)>,
}

struct KeptProducer {
    state: Arc<Mutex<NativeState>>,
    nonce: String,
    root_identity: FileIdentity,
    charge: Arc<AtomicU64>,
    workers: Vec<BoundWorker>,
}
#[derive(Clone)]
struct BoundWorker {
    worker: TrackedWorker,
    expected: std::thread::ThreadId,
}
impl BoundWorker {
    fn wait(&self, timeout: Duration) -> bool {
        self.worker.wait(timeout) && self.worker.thread_id() == Some(self.expected)
    }
}

thread_local! {
    static NATIVE_BORROW: Cell<bool> = const { Cell::new(false) };
    static PRODUCER_WORKER: RefCell<Option<TrackedWorker>> = const { RefCell::new(None) };
}
fn reject_reentry() -> Result<()> {
    ensure!(
        !NATIVE_BORROW.with(Cell::get),
        "Native producer callback cannot reenter file ownership"
    );
    Ok(())
}
struct NativeBorrow;
impl NativeBorrow {
    fn enter() -> Result<Self> {
        reject_reentry()?;
        NATIVE_BORROW.with(|held| held.set(true));
        Ok(Self)
    }
}
impl Drop for NativeBorrow {
    fn drop(&mut self) {
        NATIVE_BORROW.with(|held| held.set(false));
    }
}
pub(super) struct ProducerWorkerScope;
impl ProducerWorkerScope {
    pub(super) fn enter(worker: TrackedWorker) -> Result<Self> {
        reject_reentry()?;
        ensure!(
            !worker.wait(Duration::ZERO) && worker.thread_id() == Some(std::thread::current().id()),
            "Native producer scope does not match its actual current worker"
        );
        PRODUCER_WORKER.with(|current| {
            ensure!(
                current.borrow().is_none(),
                "Native producer worker scope already entered"
            );
            *current.borrow_mut() = Some(worker);
            Ok(Self)
        })
    }
}
impl Drop for ProducerWorkerScope {
    fn drop(&mut self) {
        PRODUCER_WORKER.with(|current| current.borrow_mut().take());
    }
}

static PRODUCERS: OnceLock<Mutex<Vec<Option<KeptProducer>>>> = OnceLock::new();
fn producers() -> &'static Mutex<Vec<Option<KeptProducer>>> {
    PRODUCERS.get_or_init(|| Mutex::new((0..CAPACITY).map(|_| None).collect()))
}

/// A borrow of the same native object. Dropping this value never closes the
/// original file: the bounded process keeper owns its state independently.
pub(super) struct ProducerFile {
    index: usize,
    state: Arc<Mutex<NativeState>>,
}
struct PublishedRepresentation<'a> {
    identity: &'a FileIdentity,
    path: &'a Path,
    bytes: u64,
    sha256: &'a str,
    sidecar: bool,
}

impl ProducerFile {
    pub(super) fn retained_profile_targets(profile: &str) -> Result<Vec<Self>> {
        reject_reentry()?;
        let states = producers()
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .iter()
            .enumerate()
            .filter_map(|(index, entry)| entry.as_ref().map(|entry| (index, entry.state.clone())))
            .collect::<Vec<_>>();
        let mut found = Vec::new();
        for (index, actual) in states {
            let state = actual.lock().unwrap_or_else(|error| error.into_inner());
            if state
                .context
                .as_ref()
                .is_some_and(|context| context.profile_id == profile)
            {
                drop(state);
                found.push(Self {
                    index,
                    state: actual,
                });
            }
        }
        Ok(found)
    }
    pub(super) fn retained_read_targets(
        profile: &str,
        task: &str,
        nonces: &[String],
    ) -> Result<Vec<Self>> {
        reject_reentry()?;
        ensure!(
            nonces.len() <= 2,
            "Download read retirement target count exceeds its bound"
        );
        let states = producers()
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .iter()
            .enumerate()
            .filter_map(|(index, entry)| {
                entry
                    .as_ref()
                    .filter(|entry| nonces.iter().any(|nonce| nonce == &entry.nonce))
                    .map(|entry| (index, entry.state.clone()))
            })
            .collect::<Vec<_>>();
        let mut found = Vec::new();
        for (index, actual) in states {
            let state = actual.lock().unwrap_or_else(|error| error.into_inner());
            if state
                .context
                .as_ref()
                .is_some_and(|context| context.profile_id == profile && context.task_id == task)
            {
                drop(state);
                found.push(Self {
                    index,
                    state: actual,
                });
            }
        }
        Ok(found)
    }
    pub(super) fn close_for_cold_recovery_after_join(
        &self,
        receiver: Option<&crate::library::authority::DownloadedPinReceipt>,
    ) -> Result<bool> {
        reject_reentry()?;
        {
            let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            if matches!(
                state.phase,
                ProducerPhase::RecoveryRetired | ProducerPhase::AdoptionWriterClosed
            ) && state.file.is_none()
                && state.charged_bytes == 0
            {
                return Ok(true);
            }
        }
        if !self.producers_joined()? {
            return Ok(false);
        }
        self.close_after_join()?;
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let mut kept = producers()
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let entry = kept[self.index]
            .as_ref()
            .context("Actual cold recovery keeper unavailable")?;
        ensure!(
            Arc::ptr_eq(&entry.state, &self.state)
                && state.file.is_none()
                && entry
                    .workers
                    .iter()
                    .all(|worker| worker.wait(Duration::ZERO)),
            "Actual cold read retirement acquired another active native loan"
        );
        if let Some(receiver) = receiver {
            receiver.matches_grant(&state.plan.grant)?;
            let closed = DestinationGrant(Arc::new(GrantInner {
                path: state.plan.grant.path().to_owned(),
                pins: Vec::new(),
                identities: Vec::new(),
                epoch: state.plan.epoch,
                selected_identity: state.plan.root_identity.clone(),
                parent: None,
                revoked: Arc::new(Mutex::new(true)),
            }));
            let original = std::mem::replace(&mut state.plan.grant, closed);
            // The opaque receiver holds this exact Arc's checked retirement
            // obligation before the source returns its native pin borrow.
            drop(original);
        } else {
            if Arc::strong_count(&state.plan.grant.0) != 1 {
                return Ok(false);
            }
            let mut parent = state.plan.grant.retained_parent();
            while let Some(grant) = parent {
                if Arc::strong_count(&grant.0) != 1 {
                    return Ok(false);
                }
                parent = grant.retained_parent();
            }
            close_retained_unique_grant(&mut state.plan.grant)?;
        }
        if let Some(quota) = state.quota.take() {
            if let Some(record) = &state.record {
                quota.remove(&record.identity);
            }
        }
        state.charged_bytes = 0;
        state.charge.store(0, Ordering::Release);
        state.phase = ProducerPhase::RecoveryRetired;
        drop(state);
        kept[self.index] = None;
        Ok(true)
    }
    pub(super) fn native_status(&self) -> Result<super::managed_native::DownloadNativeStatus> {
        use super::managed_native::DownloadNativeStatus as Status;
        reject_reentry()?;
        let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        Ok(if state.file.is_some() {
            Status::Captured
        } else {
            match state.phase {
                ProducerPhase::Reserved | ProducerPhase::IntentAcknowledged => Status::Reserved,
                ProducerPhase::NativeAttempted | ProducerPhase::NativeCaptured => {
                    Status::AttemptedNoHandle
                }
                ProducerPhase::CreationAcknowledged
                | ProducerPhase::AdoptionWriterClosed
                | ProducerPhase::HandleClosed
                | ProducerPhase::NamespaceRemoved
                | ProducerPhase::RecoveryRetired => Status::Closed,
            }
        })
    }
    pub(super) fn intent_binding(&self) -> Result<crate::download::artifact::OwnedArtifactIntent> {
        reject_reentry()?;
        let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let intent = crate::download::artifact::OwnedArtifactIntent {
            kind: state.kind,
            nonce: state.nonce.clone(),
            basename: state.basename.clone(),
            final_basename: state.plan.basename.clone(),
            working_directory_path: state.plan.grant.path().to_owned(),
            working_directory_identity: state.plan.grant.working_directory_identity().clone(),
            selected_root_identity: state.plan.root_identity.clone(),
            destination_epoch: state.plan.epoch,
            relative_group: state
                .plan
                .grant
                .0
                .parent
                .as_ref()
                .map(|_| {
                    state
                        .plan
                        .grant
                        .path()
                        .file_name()
                        .and_then(|name| name.to_str())
                        .context("Actual native group has no valid name")
                        .map(str::to_owned)
                })
                .transpose()?,
            reserved_max_bytes: state.maximum_bytes,
            context: state
                .context
                .clone()
                .context("Actual native producer has no original admitted context")?,
        };
        intent.validate()?;
        Ok(intent)
    }
    pub(super) fn allocation_binding(
        &self,
    ) -> Result<crate::download::artifact::OwnedArtifactAllocation> {
        let intent = self.intent_binding()?;
        let record = self.record_binding()?;
        let allocation = crate::download::artifact::OwnedArtifactAllocation {
            kind: intent.kind,
            record,
            working_directory_path: intent.working_directory_path,
            working_directory_identity: intent.working_directory_identity,
            selected_root_identity: intent.selected_root_identity,
            relative_group: intent.relative_group,
            reserved_max_bytes: intent.reserved_max_bytes,
            context: intent.context,
        };
        allocation.validate()?;
        Ok(allocation)
    }
    pub(super) fn inspect_io<R>(&self, inspect: impl FnOnce(&File) -> Result<R>) -> Result<R> {
        self.admit_current_worker()?;
        self.inspect(inspect)
    }
    pub(super) fn selected_plan(&self) -> Result<DownloadPlan> {
        reject_reentry()?;
        let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        state.plan.verify()?;
        Ok(state.plan.clone())
    }
    pub(super) fn context_binding(&self) -> Result<crate::download::DownloadContext> {
        reject_reentry()?;
        self.state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .context
            .clone()
            .context("Actual producer has no admitted original task context")
    }
    pub(super) fn record_binding(&self) -> Result<OwnedPartRecord> {
        reject_reentry()?;
        self.state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .record
            .clone()
            .context("Actual producer has no verified original object record")
    }
    pub(super) fn retained_for_profile(profile_id: &str) -> Result<bool> {
        reject_reentry()?;
        let states = producers()
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .iter()
            .flatten()
            .map(|entry| entry.state.clone())
            .collect::<Vec<_>>();
        for state in states {
            let state = state.lock().unwrap_or_else(|error| error.into_inner());
            if state
                .context
                .as_ref()
                .is_none_or(|context| context.profile_id == profile_id)
            {
                match state.kind {
                    crate::download::artifact::OwnedArtifactKind::Transfer
                    | crate::download::artifact::OwnedArtifactKind::TagCopy
                    | crate::download::artifact::OwnedArtifactKind::LyricSidecar
                    | crate::download::artifact::OwnedArtifactKind::RollbackOriginal => {
                        return Ok(true);
                    }
                }
            }
        }
        Ok(false)
    }
    pub(super) fn reserve(
        plan: DownloadPlan,
        nonce: String,
        kind: crate::download::artifact::OwnedArtifactKind,
        context: Option<crate::download::DownloadContext>,
        maximum_bytes: u64,
    ) -> Result<Self> {
        reject_reentry()?;
        let worker = PRODUCER_WORKER
            .with(|current| current.borrow().clone())
            .context("Native allocation requires a pre-admitted actual producer worker")?;
        plan.verify()?;
        if let Some(context) = &context {
            context.validate()?;
        }
        ensure!(
            nonce.len() == 32 && nonce.bytes().all(|byte| byte.is_ascii_hexdigit()),
            "Invalid actual producer nonce"
        );
        ensure!(
            maximum_bytes > 0 && maximum_bytes <= MAX_DOWNLOAD_BYTES,
            "Invalid actual producer reservation"
        );
        let mut kept = producers()
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let index = kept
            .iter()
            .position(Option::is_none)
            .context("Actual download object capacity reached")?;
        let mut charged = 0u64;
        for entry in kept.iter().flatten() {
            ensure!(
                entry.nonce != nonce || entry.root_identity != plan.root_identity,
                "Actual producer nonce already admitted"
            );
            charged = charged
                .checked_add(entry.charge.load(Ordering::Acquire))
                .context("Actual producer byte accounting overflow")?;
        }
        ensure!(
            charged
                .checked_add(0)
                .is_some_and(|bytes| bytes <= crate::download::MAX_OWNED_PART_BYTES),
            "Actual download byte reservation exhausted"
        );
        let charge = Arc::new(AtomicU64::new(0));
        let root_identity = plan.root_identity.clone();
        let kept_nonce = nonce.clone();
        let state = Arc::new(Mutex::new(NativeState {
            plan,
            basename: format!(".lx-download-{nonce}.part"),
            native_name: format!(".lx-download-{nonce}.part"),
            nonce,
            maximum_bytes,
            charged_bytes: 0,
            charge: charge.clone(),
            quota: None,
            context,
            kind,
            phase: ProducerPhase::Reserved,
            creation_acknowledged: false,
            file: None,
            record: None,
            removed_name: None,
            adoption_proof: None,
        }));
        kept[index] = Some(KeptProducer {
            state: state.clone(),
            nonce: kept_nonce,
            root_identity,
            charge,
            workers: vec![BoundWorker {
                worker,
                expected: std::thread::current().id(),
            }],
        });
        Ok(Self { index, state })
    }

    /// The durable callback precedes the first native namespace allocation.
    /// A panic leaves the attempted allocation and its reservation retained.
    pub(super) fn create(
        &self,
        acknowledge_intent: impl FnOnce(&DownloadPlan, &str, &str, u64) -> Result<()>,
    ) -> Result<()> {
        reject_reentry()?;
        self.admit_current_worker()?;
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        ensure!(
            state.phase == ProducerPhase::Reserved,
            "Actual producer already attempted"
        );
        state.plan.verify()?;
        {
            let _borrow = NativeBorrow::enter()?;
            acknowledge_intent(
                &state.plan,
                &state.basename,
                &state.nonce,
                state.maximum_bytes,
            )?;
        }
        state.phase = ProducerPhase::IntentAcknowledged;
        state.plan.verify()?;
        state.creation_acknowledged = false;
        state.phase = ProducerPhase::NativeAttempted;
        native_capture(&mut state, true)?;
        Ok(())
    }

    pub(super) fn reopen(&self) -> Result<()> {
        reject_reentry()?;
        self.admit_current_worker()?;
        // An existing file has not yet proved its size. Charge the whole
        // admitted bound before opening it, then reconcile the original FH.
        let maximum = self
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .maximum_bytes;
        self.reserve_length(maximum)?;
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        ensure!(
            state.phase == ProducerPhase::Reserved,
            "Actual reopen already attempted"
        );
        state.plan.verify()?;
        state.creation_acknowledged = false;
        state.phase = ProducerPhase::NativeAttempted;
        native_capture(&mut state, false)
    }

    pub(super) fn reopen_published(&self, record: &OwnedPartRecord) -> Result<()> {
        reject_reentry()?;
        self.admit_current_worker()?;
        {
            let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            ensure!(
                state.phase == ProducerPhase::Reserved
                    && record.nonce == state.nonce
                    && record.final_basename == state.plan.basename,
                "Actual published reopening differs from its selected native reservation"
            );
            state.native_name = state.plan.basename.clone();
        }
        self.reopen()
    }

    pub(super) fn retain_original(
        plan: &DownloadPlan,
        record: &OwnedPartRecord,
    ) -> Result<Option<Self>> {
        reject_reentry()?;
        let kept = producers()
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let found = kept.iter().enumerate().find_map(|(index, entry)| {
            entry
                .as_ref()
                .filter(|entry| {
                    entry.nonce == record.nonce
                        && entry.root_identity == record.destination_identity
                })
                .map(|entry| (index, entry.state.clone(), entry.workers.clone()))
        });
        drop(kept);
        if let Some((index, actual, workers)) = found {
            let state = actual.lock().unwrap_or_else(|error| error.into_inner());
            ensure!(
                state.plan.epoch == plan.epoch
                    && state.plan.basename == plan.basename
                    && state.basename == record.basename
                    && state.record.as_ref() == Some(record),
                "Retained actual producer differs from resumed ownership"
            );
            ensure!(
                !workers.is_empty() && workers.iter().all(|worker| worker.wait(Duration::ZERO)),
                "Retained producer has not actually joined"
            );
            drop(state);
            return Ok(Some(Self {
                index,
                state: actual,
            }));
        }
        Ok(None)
    }

    /// Reopening is an explicit current selection of a previously closed
    /// object. The same reservation remains charged throughout native capture.
    pub(super) fn reopen_closed(
        &self,
        plan: &DownloadPlan,
        record: &OwnedPartRecord,
    ) -> Result<()> {
        reject_reentry()?;
        plan.verify()?;
        ensure!(
            self.producers_joined()?,
            "Previous actual producer has not joined"
        );
        {
            let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            ensure!(
                state.file.is_none()
                    && matches!(
                        state.phase,
                        ProducerPhase::HandleClosed | ProducerPhase::NativeAttempted
                    )
                    && state.record.as_ref() == Some(record)
                    && state.plan.root_identity == plan.root_identity
                    && state.plan.epoch == plan.epoch
                    && state.plan.basename == plan.basename,
                "Closed actual producer cannot be rebound to another namespace"
            );
            state.plan = plan.clone();
            state.phase = ProducerPhase::Reserved;
        }
        self.reopen()
    }

    pub(super) fn reopen_closed_published(
        &self,
        plan: &DownloadPlan,
        record: &OwnedPartRecord,
    ) -> Result<()> {
        reject_reentry()?;
        plan.verify()?;
        ensure!(
            self.producers_joined()?,
            "Previous published native producer has not joined"
        );
        {
            let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            ensure!(
                state.file.is_none()
                    && matches!(
                        state.phase,
                        ProducerPhase::HandleClosed | ProducerPhase::NativeAttempted
                    )
                    && state.removed_name.is_none()
                    && state.record.as_ref() == Some(record)
                    && state.plan.root_identity == plan.root_identity
                    && state.plan.epoch == plan.epoch
                    && state.plan.basename == plan.basename,
                "Closed published object cannot be rebound or reopened after disposition"
            );
            state.plan = plan.clone();
            state.native_name = plan.basename.clone();
            state.phase = ProducerPhase::Reserved;
        }
        self.reopen()
    }

    /// Verification and acknowledgement borrow the original kept descriptor.
    pub(super) fn acknowledge_created(
        &self,
        acknowledge: impl FnOnce(&File, &DownloadPlan, &str, &str) -> Result<OwnedPartRecord>,
    ) -> Result<OwnedPartRecord> {
        reject_reentry()?;
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        ensure!(
            matches!(
                state.phase,
                ProducerPhase::NativeCaptured | ProducerPhase::CreationAcknowledged
            ),
            "Actual producer has no captured descriptor"
        );
        let record = {
            let _borrow = NativeBorrow::enter()?;
            acknowledge(
                state
                    .file
                    .as_ref()
                    .context("Actual producer descriptor unavailable")?,
                &state.plan,
                &state.basename,
                &state.nonce,
            )?
        };
        ensure!(
            record.nonce == state.nonce
                && record.basename == state.basename
                && record.destination_identity == state.plan.root_identity
                && record.destination_epoch == state.plan.epoch
                && record.final_basename == state.plan.basename,
            "Creation acknowledgement differs from its retained actual producer"
        );
        state.record = Some(record.clone());
        state.phase = ProducerPhase::CreationAcknowledged;
        state.creation_acknowledged = true;
        Ok(record)
    }

    pub(super) fn creation_acknowledged(&self) -> Result<bool> {
        reject_reentry()?;
        Ok(self
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .creation_acknowledged)
    }

    pub(super) fn remember_record(&self, record: OwnedPartRecord) -> Result<()> {
        reject_reentry()?;
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        ensure!(
            matches!(
                state.phase,
                ProducerPhase::NativeCaptured | ProducerPhase::CreationAcknowledged
            ) && state.file.is_some()
                && record.nonce == state.nonce
                && record.basename == state.basename
                && state.record.as_ref().is_none_or(|actual| actual == &record),
            "Actual producer record differs from its captured descriptor"
        );
        state.record = Some(record);
        Ok(())
    }

    pub(super) fn attach_quota(&self, quota: crate::download::quota::PartialQuota) -> Result<()> {
        reject_reentry()?;
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let identity = state
            .record
            .as_ref()
            .context("Actual quota attachment has no object record")?
            .identity
            .clone();
        quota.update(&identity, state.charged_bytes)?;
        state.quota = Some(quota);
        Ok(())
    }

    pub(super) fn request_owned_removal(&self) -> Result<()> {
        reject_reentry()?;
        ensure!(
            self.producers_joined()?,
            "Actual producer has not joined before native disposition"
        );
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let record = state
            .record
            .as_ref()
            .context("Actual disposition has no captured object record")?
            .clone();
        let native = state
            .file
            .as_ref()
            .context("Actual disposition has no retained native descriptor")?;
        platform::validate(native, false)?;
        ensure!(
            platform::identity(native)? == record.identity && platform::link_count(native)? == 1,
            "Actual native disposition object identity or link count changed"
        );
        let name = if platform::verify_file_name(
            state.plan.grant.parent(),
            &record.basename,
            &record.identity,
        )
        .is_ok()
        {
            record.basename.clone()
        } else {
            platform::verify_file_name(
                state.plan.grant.parent(),
                &state.plan.basename,
                &record.identity,
            )?;
            state.plan.basename.clone()
        };
        state.removed_name = Some(name.clone());
        platform::remove_owned(
            state
                .file
                .as_ref()
                .expect("Verified actual disposition descriptor"),
            state.plan.grant.parent(),
            &name,
            &record.identity,
        )
    }

    pub(super) fn inspect_published_after_join<R>(
        &self,
        facts: &CommittedAudio,
        inspect: impl FnOnce(&File, &DestinationGrant) -> Result<R>,
    ) -> Result<R> {
        ensure!(
            facts.checkpoint.completed_body,
            "Audio publication has no completed original body"
        );
        self.inspect_published_native(
            PublishedRepresentation {
                identity: &facts.identity,
                path: &facts.path,
                bytes: facts.checkpoint.bytes,
                sha256: &facts.checkpoint.sha256,
                sidecar: false,
            },
            inspect,
        )
    }
    pub(super) fn inspect_sidecar_after_join<R>(
        &self,
        facts: &CommittedSidecar,
        inspect: impl FnOnce(&File, &DestinationGrant) -> Result<R>,
    ) -> Result<R> {
        ensure!(
            facts.bytes <= crate::download::lyrics::MAX_DOWNLOAD_LYRIC_OUTPUT_BYTES as u64 + 3,
            "Sidecar publication exceeds its admitted body bound"
        );
        self.inspect_published_native(
            PublishedRepresentation {
                identity: &facts.identity,
                path: &facts.path,
                bytes: facts.bytes,
                sha256: &facts.sha256,
                sidecar: true,
            },
            inspect,
        )
    }
    fn inspect_published_native<R>(
        &self,
        facts: PublishedRepresentation<'_>,
        inspect: impl FnOnce(&File, &DestinationGrant) -> Result<R>,
    ) -> Result<R> {
        reject_reentry()?;
        ensure!(
            self.previous_producers_joined_for_current()?,
            "Published object still has an unjoined actual producer"
        );
        self.admit_current_worker()?;
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let record = state
            .record
            .as_ref()
            .context("Published native object has no original creation record")?
            .clone();
        ensure!(
            facts.identity == &record.identity
                && facts.path == state.plan.path()
                && facts.bytes > 0
                && facts.bytes <= state.maximum_bytes
                && (state.kind == crate::download::artifact::OwnedArtifactKind::LyricSidecar)
                    == facts.sidecar,
            "Published facts differ from the original retained native object"
        );
        let plan = state.plan.clone();
        let native = state
            .file
            .as_mut()
            .context("Published original native descriptor unavailable")?;
        platform::validate(native, false)?;
        ensure!(
            platform::identity(native)? == record.identity
                && platform::link_count(native)? == 1
                && native.metadata()?.len() == facts.bytes,
            "Published original identity, single link or length changed"
        );
        platform::verify_file_name(plan.grant.parent(), &plan.basename, &record.identity)?;
        let stamp = platform::stamp(native)?;
        native.rewind()?;
        let mut digest = Sha256::new();
        let mut count = 0u64;
        let mut buffer = [0u8; COPY_BUFFER_BYTES];
        loop {
            ensure!(
                platform::stamp(native)? == stamp,
                "Published original changed during native read loan"
            );
            let read = native.read(&mut buffer)?;
            count = count
                .checked_add(read as u64)
                .context("Published native read length overflow")?;
            ensure!(
                count <= facts.bytes,
                "Published native read exceeded its body proof"
            );
            if read == 0 {
                break;
            }
            digest.update(&buffer[..read]);
        }
        ensure!(
            count == facts.bytes
                && format!("{:x}", digest.finalize()) == facts.sha256
                && platform::stamp(native)? == stamp,
            "Published original body differs from its actual output proof"
        );
        let _borrow = NativeBorrow::enter()?;
        inspect(native, &plan.grant)
    }

    pub(super) fn release_adopted_after_join(
        &self,
        facts: &CommittedAudio,
        receipt: &crate::library::authority::DownloadedReadReceipt,
    ) -> Result<()> {
        ensure!(
            facts.checkpoint.completed_body,
            "Audio adoption has no completed original body"
        );
        receipt.matches_audio(facts)?;
        self.release_adopted_native(
            PublishedRepresentation {
                identity: &facts.identity,
                path: &facts.path,
                bytes: facts.checkpoint.bytes,
                sha256: &facts.checkpoint.sha256,
                sidecar: false,
            },
            receipt,
        )
    }
    pub(super) fn release_adopted_sidecar_after_join(
        &self,
        facts: &CommittedSidecar,
        receipt: &crate::library::authority::DownloadedReadReceipt,
    ) -> Result<()> {
        ensure!(
            facts.bytes <= crate::download::lyrics::MAX_DOWNLOAD_LYRIC_OUTPUT_BYTES as u64 + 3,
            "Sidecar adoption exceeds its admitted body bound"
        );
        receipt.matches_sidecar(facts)?;
        self.release_adopted_native(
            PublishedRepresentation {
                identity: &facts.identity,
                path: &facts.path,
                bytes: facts.bytes,
                sha256: &facts.sha256,
                sidecar: true,
            },
            receipt,
        )
    }
    fn release_adopted_native(
        &self,
        facts: PublishedRepresentation<'_>,
        receipt: &crate::library::authority::DownloadedReadReceipt,
    ) -> Result<()> {
        reject_reentry()?;
        receipt.verify()?;
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let mut kept = producers()
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let entry = kept[self.index]
            .as_ref()
            .context("Actual producer keeper unavailable")?;
        ensure!(
            Arc::ptr_eq(&entry.state, &self.state)
                && !entry.workers.is_empty()
                && entry
                    .workers
                    .iter()
                    .all(|worker| worker.wait(Duration::ZERO)),
            "Actual producer or Library preparation loan has not joined"
        );
        let record = state
            .record
            .as_ref()
            .context("Adoption has no original native creation record")?
            .clone();
        ensure!(
            facts.identity == &record.identity
                && facts.path == state.plan.path()
                && facts.bytes > 0
                && facts.bytes <= state.maximum_bytes
                && (state.kind == crate::download::artifact::OwnedArtifactKind::LyricSidecar)
                    == facts.sidecar,
            "Library adoption facts differ from the retained native source"
        );
        receipt.matches_source(&record.identity, &record.nonce, &state.plan.grant)?;
        if state.phase == ProducerPhase::AdoptionWriterClosed {
            ensure!(
                state.file.is_none()
                    && state.adoption_proof.as_ref()
                        == Some(&(facts.bytes, facts.sha256.to_owned())),
                "Retry of Library adoption differs from the actually closed writer"
            );
        } else {
            let native = state
                .file
                .as_ref()
                .context("Library adoption original writer unavailable")?;
            platform::validate(native, false)?;
            ensure!(
                platform::identity(native)? == record.identity
                    && platform::link_count(native)? == 1
                    && native.metadata()?.len() == facts.bytes,
                "Library adoption original native source changed"
            );
            platform::verify_file_name(
                state.plan.grant.parent(),
                &state.plan.basename,
                &record.identity,
            )?;
            close_native_in_place(&mut state)?;
            state.adoption_proof = Some((facts.bytes, facts.sha256.to_owned()));
            state.phase = ProducerPhase::AdoptionWriterClosed;
        }
        // The verified receiver keeps the same actual directory pins. Closing
        // the writer alone does not erase durable output artifact bytes.
        receipt.verify()?;
        if let Some(quota) = state.quota.take() {
            quota.remove(&record.identity);
        }
        state.charged_bytes = 0;
        state.charge.store(0, Ordering::Release);
        drop(state);
        kept[self.index] = None;
        Ok(())
    }

    pub(super) fn reserve_length(&self, bytes: u64) -> Result<()> {
        reject_reentry()?;
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let kept = producers()
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let entry = kept[self.index]
            .as_ref()
            .context("Actual producer keeper unavailable")?;
        ensure!(
            Arc::ptr_eq(&entry.state, &self.state),
            "Actual producer keeper changed"
        );
        let mut total = 0u64;
        for (index, entry) in kept.iter().enumerate() {
            if index == self.index {
                continue;
            }
            if let Some(entry) = entry {
                total = total
                    .checked_add(entry.charge.load(Ordering::Acquire))
                    .context("Actual producer byte accounting overflow")?;
            }
        }
        ensure!(
            bytes <= state.maximum_bytes,
            "Actual producer byte reservation exceeded"
        );
        ensure!(
            total
                .checked_add(bytes)
                .is_some_and(|bytes| bytes <= crate::download::MAX_OWNED_PART_BYTES),
            "Actual download byte quota exhausted"
        );
        state.charged_bytes = bytes;
        state.charge.store(bytes, Ordering::Release);
        Ok(())
    }

    /// A successful exact-object check may replace a provisional reopen/write
    /// charge with its actual length; closing an FH never calls this method.
    pub(super) fn reconcile_bytes(&self) -> Result<()> {
        let length = self.inspect(|file| Ok(file.metadata()?.len()))?;
        self.reserve_length(length)
    }

    pub(super) fn attach_worker(&self, worker: TrackedWorker) -> Result<()> {
        reject_reentry()?;
        let expected = worker
            .thread_id()
            .context("Producer worker has not been attached to its actual thread")?;
        let mut kept = producers()
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let entry = kept[self.index]
            .as_mut()
            .context("Actual producer keeper unavailable")?;
        ensure!(
            Arc::ptr_eq(&entry.state, &self.state),
            "Actual producer keeper changed"
        );
        entry.workers.retain(|worker| !worker.wait(Duration::ZERO));
        ensure!(
            entry.workers.len() < 8,
            "Actual producer exit bound reached"
        );
        if !entry
            .workers
            .iter()
            .any(|bound| bound.expected == expected && bound.worker.same_worker(&worker))
        {
            entry.workers.push(BoundWorker { worker, expected });
        }
        Ok(())
    }

    fn admit_current_worker(&self) -> Result<()> {
        reject_reentry()?;
        let worker = PRODUCER_WORKER
            .with(|current| current.borrow().clone())
            .context("Actual producer IO has no pre-admitted current worker")?;
        let expected = std::thread::current().id();
        ensure!(
            worker.thread_id() == Some(expected) && !worker.wait(Duration::ZERO),
            "Actual producer IO belongs to another worker"
        );
        let mut kept = producers()
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let entry = kept[self.index]
            .as_mut()
            .context("Actual producer keeper unavailable")?;
        ensure!(
            Arc::ptr_eq(&entry.state, &self.state),
            "Actual producer keeper changed"
        );
        entry.workers.retain(|worker| !worker.wait(Duration::ZERO));
        if !entry
            .workers
            .iter()
            .any(|bound| bound.expected == expected && bound.worker.same_worker(&worker))
        {
            ensure!(
                entry.workers.len() < 8,
                "Actual producer exit bound reached"
            );
            entry.workers.push(BoundWorker { worker, expected });
        }
        Ok(())
    }

    pub(super) fn producers_joined(&self) -> Result<bool> {
        reject_reentry()?;
        let kept = producers()
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let entry = kept[self.index]
            .as_ref()
            .context("Actual producer keeper unavailable")?;
        ensure!(
            Arc::ptr_eq(&entry.state, &self.state),
            "Actual producer keeper changed"
        );
        Ok(!entry.workers.is_empty()
            && entry
                .workers
                .iter()
                .all(|worker| worker.wait(Duration::ZERO)))
    }

    fn previous_producers_joined_for_current(&self) -> Result<bool> {
        reject_reentry()?;
        let current = PRODUCER_WORKER
            .with(|worker| worker.borrow().clone())
            .context("Published native inspection has no pre-admitted actual current worker")?;
        let id = std::thread::current().id();
        ensure!(
            current.thread_id() == Some(id) && !current.wait(Duration::ZERO),
            "Published native inspection does not match its actual current worker"
        );
        let kept = producers()
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let entry = kept[self.index]
            .as_ref()
            .context("Actual producer keeper unavailable")?;
        ensure!(
            Arc::ptr_eq(&entry.state, &self.state) && !entry.workers.is_empty(),
            "Published native inspection has no original actual producer admission"
        );
        ensure!(
            entry
                .workers
                .iter()
                .any(|bound| bound.expected == id && bound.worker.same_worker(&current)),
            "Current native inspection was not prebound to the original producer object"
        );
        Ok(entry.workers.iter().all(|bound| {
            (bound.expected == id && bound.worker.same_worker(&current))
                || bound.wait(Duration::ZERO)
        }))
    }

    pub(super) fn shared_borrow(&self) -> Self {
        Self {
            index: self.index,
            state: self.state.clone(),
        }
    }

    pub(super) fn inspect<R>(&self, inspect: impl FnOnce(&File) -> Result<R>) -> Result<R> {
        reject_reentry()?;
        let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let _borrow = NativeBorrow::enter()?;
        inspect(
            state
                .file
                .as_ref()
                .context("Actual producer descriptor unavailable")?,
        )
    }

    pub(super) fn inspect_namespace<R>(
        &self,
        inspect: impl FnOnce(&DestinationGrant) -> Result<R>,
    ) -> Result<R> {
        reject_reentry()?;
        let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let _borrow = NativeBorrow::enter()?;
        inspect(&state.plan.grant)
    }

    fn io<R>(&self, operation: impl FnOnce(&mut File) -> io::Result<R>) -> io::Result<R> {
        reject_reentry()
            .map_err(|error| io::Error::new(io::ErrorKind::WouldBlock, error.to_string()))?;
        self.admit_current_worker()
            .map_err(|error| io::Error::new(io::ErrorKind::PermissionDenied, error.to_string()))?;
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let file = state.file.as_mut().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotConnected,
                "Actual producer descriptor unavailable",
            )
        })?;
        operation(file)
    }
    fn write_io<R>(
        &self,
        required: impl FnOnce(&mut File) -> io::Result<u64>,
        operation: impl FnOnce(&mut File) -> io::Result<R>,
    ) -> io::Result<R> {
        reject_reentry()
            .map_err(|error| io::Error::new(io::ErrorKind::WouldBlock, error.to_string()))?;
        self.admit_current_worker()
            .map_err(|error| io::Error::new(io::ErrorKind::PermissionDenied, error.to_string()))?;
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let kept = producers()
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let mut total = 0u64;
        for (index, entry) in kept.iter().enumerate() {
            if index == self.index {
                continue;
            }
            if let Some(entry) = entry {
                total = total
                    .checked_add(entry.charge.load(Ordering::Acquire))
                    .ok_or_else(|| {
                        io::Error::new(
                            io::ErrorKind::StorageFull,
                            "Actual producer byte accounting overflow",
                        )
                    })?;
            }
        }
        if state.phase != ProducerPhase::CreationAcknowledged {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "Actual producer creation has not been acknowledged",
            ));
        }
        let requested = required(state.file.as_mut().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotConnected,
                "Actual producer descriptor unavailable",
            )
        })?)?;
        if requested > state.maximum_bytes
            || total
                .checked_add(requested)
                .is_none_or(|bytes| bytes > crate::download::MAX_OWNED_PART_BYTES)
        {
            return Err(io::Error::new(
                io::ErrorKind::StorageFull,
                "Actual producer byte reservation exceeded",
            ));
        }
        state.charged_bytes = requested;
        state.charge.store(requested, Ordering::Release);
        operation(
            state
                .file
                .as_mut()
                .expect("Preflight borrowed actual producer"),
        )
    }

    pub(super) fn metadata(&self) -> io::Result<Metadata> {
        self.inspect(|file| Ok(file.metadata()?))
            .map_err(|error| io::Error::new(io::ErrorKind::Other, error.to_string()))
    }
    pub(super) fn sync_all(&self) -> io::Result<()> {
        self.io(|file| file.sync_all())
    }
    pub(super) fn set_len(&self, bytes: u64) -> io::Result<()> {
        self.write_io(
            |file| Ok(file.metadata()?.len().max(bytes)),
            |file| file.set_len(bytes),
        )?;
        self.reconcile_bytes()
            .map_err(|error| io::Error::other(error.to_string()))
    }

    /// A successful close retains the pins and durable byte charge. Releasing
    /// an artifact reservation requires separate namespace or adoption proof.
    pub(super) fn close_after_join(&self) -> Result<()> {
        reject_reentry()?;
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let kept = producers()
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let entry = kept[self.index]
            .as_ref()
            .context("Actual producer keeper unavailable")?;
        ensure!(
            Arc::ptr_eq(&entry.state, &self.state)
                && !entry.workers.is_empty()
                && entry
                    .workers
                    .iter()
                    .all(|worker| worker.wait(Duration::ZERO)),
            "Actual producer has not joined before native close"
        );
        if state.phase == ProducerPhase::NamespaceRemoved {
            return Ok(());
        }
        close_native_in_place(&mut state)?;
        state.phase = ProducerPhase::HandleClosed;
        Ok(())
    }

    /// Called only after the original descriptor has actually closed. The
    /// original pins prove absence before the keeper releases its byte charge.
    pub(super) fn release_removed(&self) -> Result<OwnedPartRecord> {
        ensure!(
            self.producers_joined()?,
            "Actual producer has not joined before retirement"
        );
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let mut kept = producers()
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let entry = kept[self.index]
            .as_ref()
            .context("Actual producer keeper unavailable")?;
        ensure!(
            Arc::ptr_eq(&entry.state, &self.state),
            "Actual producer keeper changed"
        );
        ensure!(
            matches!(
                state.phase,
                ProducerPhase::HandleClosed | ProducerPhase::NamespaceRemoved
            ) && state.file.is_none(),
            "Actual artifact retirement still has an open native file"
        );
        let record = state
            .record
            .clone()
            .context("Actual retirement has no captured object record")?;
        if state.phase == ProducerPhase::HandleClosed {
            platform::verify_removed_owned(
                state.plan.grant.parent(),
                state.removed_name.as_deref().unwrap_or(&record.basename),
                &record.identity,
            )?;
            state.phase = ProducerPhase::NamespaceRemoved;
        }
        // Shared grant pins already have another actual DestinationGrant owner.
        // Sole-owned pins must actually close before this keeper can surrender.
        close_sole_owned_grant(&mut state.plan.grant)?;
        state.charged_bytes = 0;
        state.charge.store(0, Ordering::Release);
        if let Some(quota) = state.quota.take() {
            quota.remove(&record.identity);
        }
        drop(state);
        kept[self.index] = None;
        Ok(record)
    }

    pub(super) fn release_native_absence(&self, absence: &CleanupNativeAbsence) -> Result<()> {
        ensure!(
            self.producers_joined()?,
            "Actual producer has not joined before absence retirement"
        );
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let mut kept = producers()
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let entry = kept[self.index]
            .as_ref()
            .context("Actual producer keeper unavailable")?;
        ensure!(
            Arc::ptr_eq(&entry.state, &self.state),
            "Actual producer keeper changed"
        );
        let record = absence.record();
        ensure!(
            state.file.is_none()
                && state.nonce == record.nonce
                && state.basename == record.basename
                && state.plan.root_identity == record.destination_identity
                && state.plan.epoch == record.destination_epoch
                && state.plan.basename == record.final_basename
                && state.record.as_ref().is_none_or(|actual| actual == record),
            "Native absence receipt differs from its actual retained object or selected namespace"
        );
        state.phase = ProducerPhase::NamespaceRemoved;
        close_sole_owned_grant(&mut state.plan.grant)?;
        state.charged_bytes = 0;
        state.charge.store(0, Ordering::Release);
        if let Some(quota) = state.quota.take() {
            quota.remove(&record.identity);
        }
        drop(state);
        kept[self.index] = None;
        Ok(())
    }
}

impl Read for ProducerFile {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.io(|file| file.read(buffer))
    }
}
impl Write for ProducerFile {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.write_io(
            |file| {
                let requested = file
                    .stream_position()?
                    .checked_add(buffer.len() as u64)
                    .ok_or_else(|| {
                        io::Error::new(
                            io::ErrorKind::InvalidInput,
                            "Actual producer write offset overflow",
                        )
                    })?;
                Ok(requested.max(file.metadata()?.len()))
            },
            |file| file.write(buffer),
        )
    }
    fn flush(&mut self) -> io::Result<()> {
        self.io(|file| file.flush())
    }
}
impl Seek for ProducerFile {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        self.io(|file| file.seek(position))
    }
}

#[cfg(windows)]
fn native_capture(state: &mut NativeState, create: bool) -> Result<()> {
    use std::os::windows::io::{AsRawHandle, FromRawHandle};
    use windows_sys::{
        Wdk::{
            Foundation::OBJECT_ATTRIBUTES,
            Storage::FileSystem::{
                FILE_CREATE, FILE_NON_DIRECTORY_FILE, FILE_OPEN, FILE_OPEN_REPARSE_POINT,
                FILE_SYNCHRONOUS_IO_NONALERT, NtCreateFile,
            },
        },
        Win32::{
            Foundation::{
                GENERIC_READ, GENERIC_WRITE, INVALID_HANDLE_VALUE, OBJ_CASE_INSENSITIVE,
                RtlNtStatusToDosError, UNICODE_STRING,
            },
            Storage::FileSystem::{DELETE, FILE_SHARE_READ, SYNCHRONIZE},
            System::IO::IO_STATUS_BLOCK,
        },
    };
    let mut text: Vec<u16> = state.native_name.encode_utf16().collect();
    let name = UNICODE_STRING {
        Length: (text.len() * 2) as u16,
        MaximumLength: (text.len() * 2) as u16,
        Buffer: text.as_mut_ptr(),
    };
    let attributes = OBJECT_ATTRIBUTES {
        Length: std::mem::size_of::<OBJECT_ATTRIBUTES>() as u32,
        RootDirectory: state.plan.grant.parent().as_raw_handle(),
        ObjectName: &name,
        Attributes: OBJ_CASE_INSENSITIVE,
        SecurityDescriptor: std::ptr::null(),
        SecurityQualityOfService: std::ptr::null(),
    };
    let mut raw = std::ptr::null_mut();
    let mut status_block = IO_STATUS_BLOCK::default();
    let status = unsafe {
        NtCreateFile(
            &mut raw,
            GENERIC_READ | GENERIC_WRITE | DELETE | SYNCHRONIZE,
            &attributes,
            &mut status_block,
            std::ptr::null(),
            0,
            FILE_SHARE_READ,
            if create { FILE_CREATE } else { FILE_OPEN },
            FILE_OPEN_REPARSE_POINT | FILE_SYNCHRONOUS_IO_NONALERT | FILE_NON_DIRECTORY_FILE,
            std::ptr::null(),
            0,
        )
    };
    // No validation, FID query, allocation or fallible callback intervenes.
    if !raw.is_null() && raw != INVALID_HANDLE_VALUE {
        state.file = Some(unsafe { File::from_raw_handle(raw) });
        state.phase = ProducerPhase::NativeCaptured;
    }
    if status < 0 {
        return Err(
            io::Error::from_raw_os_error(unsafe { RtlNtStatusToDosError(status) } as i32).into(),
        );
    }
    ensure!(
        state.file.is_some(),
        "Native producer returned no descriptor"
    );
    Ok(())
}

#[cfg(unix)]
fn native_capture(state: &mut NativeState, create: bool) -> Result<()> {
    use std::{
        ffi::CString,
        os::fd::{AsRawFd, FromRawFd},
    };
    let name = CString::new(state.native_name.as_bytes())?;
    let descriptor = unsafe {
        libc::openat(
            state.plan.grant.parent().as_raw_fd(),
            name.as_ptr(),
            libc::O_RDWR
                | libc::O_CLOEXEC
                | libc::O_NOFOLLOW
                | if create {
                    libc::O_CREAT | libc::O_EXCL
                } else {
                    0
                },
            0o600,
        )
    };
    if descriptor >= 0 {
        state.file = Some(unsafe { File::from_raw_fd(descriptor) });
        state.phase = ProducerPhase::NativeCaptured;
        return Ok(());
    }
    Err(io::Error::last_os_error().into())
}

#[cfg(not(any(windows, unix)))]
fn native_capture(_: &mut NativeState, _: bool) -> Result<()> {
    bail!("UnsupportedCapability: native producer capture is unavailable")
}

#[cfg(windows)]
fn close_native_in_place(state: &mut NativeState) -> Result<()> {
    use std::os::windows::io::{FromRawHandle, IntoRawHandle};
    if let Some(file) = state.file.take() {
        let raw = file.into_raw_handle();
        if unsafe { windows_sys::Win32::Foundation::CloseHandle(raw) } == 0 {
            let error = io::Error::last_os_error();
            state.file = Some(unsafe { File::from_raw_handle(raw) });
            return Err(error.into());
        }
    }
    Ok(())
}

#[cfg(windows)]
fn close_sole_owned_grant(grant: &mut DestinationGrant) -> Result<()> {
    use std::os::windows::io::{FromRawHandle, IntoRawHandle};
    let Some(inner) = Arc::get_mut(&mut grant.0) else {
        return Ok(());
    };
    while let Some(file) = inner.pins.pop() {
        let raw = file.into_raw_handle();
        if unsafe { windows_sys::Win32::Foundation::CloseHandle(raw) } == 0 {
            let error = io::Error::last_os_error();
            inner.pins.push(unsafe { File::from_raw_handle(raw) });
            return Err(error.into());
        }
        inner.identities.pop();
    }
    if let Some(parent) = &mut inner.parent {
        close_sole_owned_grant(parent)?;
    }
    Ok(())
}

#[cfg(windows)]
pub(super) fn close_retained_unique_grant(grant: &mut DestinationGrant) -> Result<()> {
    let mut current = &*grant;
    loop {
        ensure!(
            Arc::strong_count(&current.0) == 1,
            "Destination native pins still have an outstanding strong borrow"
        );
        let Some(parent) = &current.0.parent else {
            break;
        };
        current = parent;
    }
    close_sole_owned_grant(grant)
}
#[cfg(windows)]
pub(super) fn close_retained_leaf_grant(
    grant: &mut DestinationGrant,
    parent_receiver: Option<&DestinationGrant>,
) -> Result<()> {
    use std::os::windows::io::{FromRawHandle, IntoRawHandle};
    let inner = Arc::get_mut(&mut grant.0)
        .context("Destination leaf still has outstanding strong borrows")?;
    match (&inner.parent, parent_receiver) {
        (None, None) => {}
        (Some(parent), Some(receiver)) => ensure!(
            parent.same_native_owner(receiver),
            "Destination parent receiver has a different actual native pin collection"
        ),
        _ => bail!("Destination parent native pins have no matching retained receiver"),
    }
    while let Some(file) = inner.pins.pop() {
        let raw = file.into_raw_handle();
        if unsafe { windows_sys::Win32::Foundation::CloseHandle(raw) } == 0 {
            let error = io::Error::last_os_error();
            inner.pins.push(unsafe { File::from_raw_handle(raw) });
            return Err(error.into());
        }
        inner.identities.pop();
    }
    // The independently retained exact parent receiver remains strong through
    // this transfer; neither its raw pins nor another native collection close.
    inner.parent.take();
    Ok(())
}
#[cfg(not(windows))]
pub(super) fn close_retained_leaf_grant(
    _: &mut DestinationGrant,
    _: Option<&DestinationGrant>,
) -> Result<()> {
    bail!("UnsupportedCapability: checked native leaf retirement requires Windows")
}
#[cfg(not(windows))]
pub(super) fn close_retained_unique_grant(_: &mut DestinationGrant) -> Result<()> {
    bail!("UnsupportedCapability: checked unique destination retirement requires Windows")
}

#[cfg(not(windows))]
fn close_sole_owned_grant(_: &mut DestinationGrant) -> Result<()> {
    bail!("UnsupportedCapability: checked grant retirement requires Windows")
}

#[cfg(not(windows))]
fn close_native_in_place(_: &mut NativeState) -> Result<()> {
    bail!("UnsupportedCapability: checked native producer close requires Windows")
}

#[cfg(all(test, windows))]
#[path = "producer_file_tests.rs"]
mod tests;

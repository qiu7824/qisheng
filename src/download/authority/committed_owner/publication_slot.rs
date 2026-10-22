//! Publication owners survive worker panic, lost notifications and actor drop.

use super::*;
use crate::audio::worker_owner::{TrackedWorker, WorkerKind, WorkerPermit};
use std::{sync::OnceLock, thread, time::Duration};

const MAX_RETAINED_PUBLICATIONS: usize =
    crate::download::MAX_DOWNLOAD_TASKS + crate::download::MAX_DOWNLOAD_WORKERS;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct DownloadPublicationKey {
    pub(crate) task_id: String,
    pub(crate) profile_id: String,
    pub(crate) operation_epoch: u64,
    pub(crate) candidate_nonce: String,
}

struct SlotState {
    owner: Option<PublicationOwner>,
    retirement: Option<PublishedAudioRetirement>,
    worker_admitted: bool,
    finished: bool,
    error: Option<String>,
    recovered: bool,
}
struct KeeperEntry {
    key: DownloadPublicationKey,
    state: Arc<Mutex<SlotState>>,
    worker: Option<TrackedWorker>,
}
static KEEPERS: OnceLock<Mutex<Vec<Option<KeeperEntry>>>> = OnceLock::new();
fn keepers() -> &'static Mutex<Vec<Option<KeeperEntry>>> {
    KEEPERS.get_or_init(|| Mutex::new((0..MAX_RETAINED_PUBLICATIONS).map(|_| None).collect()))
}

pub(crate) struct PublicationSlotAdmissionFailure {
    pub(crate) cause: anyhow::Error,
    pub(crate) audio: VerifiedOwnedAudio,
}

/// The process keeper owns the same slot before any native work starts. The
/// worker captures only slot state, with no reference to its tracked join handle.
pub(crate) struct DownloadPublicationSlot {
    key: DownloadPublicationKey,
    index: usize,
    state: Arc<Mutex<SlotState>>,
}

impl DownloadPublicationSlot {
    pub(crate) fn admit(
        audio: VerifiedOwnedAudio,
    ) -> std::result::Result<Self, PublicationSlotAdmissionFailure> {
        let key = DownloadPublicationKey {
            task_id: audio.context.task_id.clone(),
            profile_id: audio.context.profile_id.clone(),
            operation_epoch: audio.context.operation_epoch,
            candidate_nonce: audio.part.record.nonce.clone(),
        };
        let mut kept = keepers().lock().unwrap_or_else(|error| error.into_inner());
        let Some(index) = kept.iter().position(Option::is_none) else {
            return Err(PublicationSlotAdmissionFailure {
                cause: anyhow::anyhow!("Download native publication owner capacity reached"),
                audio,
            });
        };
        let state = Arc::new(Mutex::new(SlotState {
            owner: Some(PublicationOwner {
                audio,
                phase: DownloadPublicationPhase::OwnedPartial,
                published_stamp: None,
            }),
            retirement: None,
            worker_admitted: false,
            finished: false,
            error: None,
            recovered: false,
        }));
        kept[index] = Some(KeeperEntry {
            key: key.clone(),
            state: state.clone(),
            worker: None,
        });
        Ok(Self { key, index, state })
    }

    pub(crate) fn key(&self) -> &DownloadPublicationKey {
        &self.key
    }

    /// Reattaches actual kept slots after an interrupted actor. No stored path
    /// or serialized identity creates a native owner here.
    pub(crate) fn recover_retained(profile_id: &str) -> Vec<Self> {
        let kept = keepers().lock().unwrap_or_else(|error| error.into_inner());
        kept.iter()
            .enumerate()
            .filter_map(|(index, entry)| {
                entry
                    .as_ref()
                    .filter(|entry| entry.key.profile_id == profile_id)
                    .map(|entry| Self {
                        key: entry.key.clone(),
                        index,
                        state: entry.state.clone(),
                    })
            })
            .collect()
    }

    pub(crate) fn start(
        &mut self,
        notify: impl FnOnce(DownloadPublicationKey) + Send + 'static,
    ) -> Result<()> {
        self.start_guarded(Box::new(|| Ok(())), Box::new(|| Ok(())), notify)
    }

    fn start_guarded(
        &mut self,
        before_publication: Box<dyn FnOnce() -> Result<()> + Send>,
        after_native_publication: Box<dyn Fn() -> Result<()> + Send>,
        notify: impl FnOnce(DownloadPublicationKey) + Send + 'static,
    ) -> Result<()> {
        let permit = WorkerPermit::reserve(WorkerKind::Download)?;
        {
            let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            ensure!(
                !state.worker_admitted && state.owner.is_some(),
                "Publication worker already admitted or owner surrendered"
            );
            state.worker_admitted = true;
        }
        let state = self.state.clone();
        let key = self.key.clone();
        let spawned = thread::Builder::new()
            .name("lx-download-publication".into())
            .spawn(move || {
                {
                    let mut state = state.lock().unwrap_or_else(|error| error.into_inner());
                    // This guard borrows the native owner already stored in the
                    // keeper. Stack unwinding poisons the mutex, not its contents.
                    let result = (|| -> Result<()> {
                        before_publication()?;
                        state
                            .owner
                            .as_mut()
                            .context("Publication owner unavailable")?
                            .publish_in_place(after_native_publication.as_ref())
                    })();
                    state.error = result.as_ref().err().map(|error| format!("{error:#}"));
                    state.finished = true;
                }
                // Notifications carry no ownership and do not acknowledge a join.
                notify(key);
            });
        let handle = match spawned {
            Ok(handle) => handle,
            Err(error) => {
                self.state
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner())
                    .worker_admitted = false;
                return Err(error.into());
            }
        };
        let tracked = permit.attach(handle);
        let mut kept = keepers().lock().unwrap_or_else(|error| error.into_inner());
        let entry = kept[self.index]
            .as_mut()
            .context("Publication keeper unavailable")?;
        ensure!(
            Arc::ptr_eq(&entry.state, &self.state),
            "Publication keeper identity changed"
        );
        ensure!(
            entry.worker.is_none(),
            "Publication worker already attached"
        );
        entry.worker = Some(tracked);
        Ok(())
    }

    fn worker(&self) -> Result<Option<TrackedWorker>> {
        let kept = keepers().lock().unwrap_or_else(|error| error.into_inner());
        let entry = kept[self.index]
            .as_ref()
            .context("Publication keeper unavailable")?;
        ensure!(
            Arc::ptr_eq(&entry.state, &self.state),
            "Publication keeper identity changed"
        );
        Ok(entry.worker.clone())
    }
    pub(crate) fn is_joined(&self) -> Result<bool> {
        Ok(self
            .worker()?
            .is_some_and(|worker| worker.wait(Duration::ZERO)))
    }
    pub(crate) fn worker_panicked(&self) -> Result<bool> {
        Ok(self
            .worker()?
            .is_some_and(|worker| worker.wait(Duration::ZERO) && worker.panicked()))
    }
    pub(crate) fn error(&self) -> Option<String> {
        self.state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .error
            .clone()
    }

    /// Explicit recovery examines the same actual file after a real join. A
    /// failed worker or notification never manufactures a successful receipt.
    pub(crate) fn recover_published_after_join(&mut self) -> Result<()> {
        ensure!(
            self.is_joined()?,
            "Publication worker has not actually joined"
        );
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let owner = state
            .owner
            .as_mut()
            .context("Publication owner unavailable")?;
        owner.verify_published_body()?;
        if owner.phase != DownloadPublicationPhase::RepositoryAcknowledged {
            owner.phase = DownloadPublicationPhase::Published;
        }
        state.recovered = true;
        state.finished = true;
        state.error = None;
        Ok(())
    }

    /// The callback commits and reads back both task and artifact rows against
    /// this actual owner. Its Err or panic leaves the original slot and quota.
    pub(crate) fn acknowledge_repository_after_join(
        &mut self,
        acknowledge: impl FnOnce(&DownloadPublicationKey, &CommittedAudio) -> Result<()>,
    ) -> Result<()> {
        let worker = self.worker()?.context("Publication worker unavailable")?;
        ensure!(
            worker.wait(Duration::ZERO),
            "Publication worker has not actually joined"
        );
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        ensure!(
            state.finished && (state.recovered || (!worker.panicked() && state.error.is_none())),
            "Publication worker failed and its same-object recovery is incomplete"
        );
        let owner = state
            .owner
            .as_mut()
            .context("Publication owner unavailable")?;
        ensure!(
            matches!(
                owner.phase,
                DownloadPublicationPhase::Published
                    | DownloadPublicationPhase::RepositoryAcknowledged
            ),
            "Native publication has not been proved"
        );
        owner.verify_published()?;
        acknowledge(&self.key, &owner.facts())?;
        owner.verify_published()?;
        owner.phase = DownloadPublicationPhase::RepositoryAcknowledged;
        if let Some(quota) = &owner.audio.part.quota {
            quota.remove(&owner.audio.part.record.identity);
        }
        Ok(())
    }

    /// The recipient becomes the native owner after both durability and join.
    /// Library adoption or explicit retirement must consume the returned owner.
    pub(crate) fn handoff_acknowledged(&mut self) -> Result<PublishedDownloadAudio> {
        ensure!(
            self.is_joined()?,
            "Publication worker has not actually joined"
        );
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let owner = state
            .owner
            .as_ref()
            .context("Publication owner unavailable")?;
        ensure!(
            owner.phase == DownloadPublicationPhase::RepositoryAcknowledged,
            "Publication has no complete repository ACK"
        );
        owner.verify_published()?;
        let owner = state.owner.take().expect("checked publication owner");
        drop(state);
        self.release_keeper();
        Ok(PublishedDownloadAudio { owner })
    }

    /// Cancellation and shutdown close native handles after join. They neither
    /// delete output files nor release unacknowledged artifact accounting.
    pub(crate) fn retire_after_join(&mut self) -> Result<()> {
        let worker = self.worker()?;
        if let Some(worker) = &worker {
            ensure!(
                worker.wait(Duration::ZERO),
                "Publication worker has not actually joined"
            );
        }
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        ensure!(
            worker.is_some() || !state.worker_admitted,
            "Admitted publication worker has no actual join proof"
        );
        if let Some(owner) = state.owner.take() {
            state.retirement = Some(owner.into_handle_retirement());
        }
        if let Some(retirement) = &mut state.retirement {
            retirement.try_close()?;
            ensure!(
                retirement.is_closed(),
                "Actual publication file retirement is incomplete"
            );
        }
        state.retirement = None;
        drop(state);
        self.release_keeper();
        Ok(())
    }

    fn release_keeper(&self) {
        let mut kept = keepers().lock().unwrap_or_else(|error| error.into_inner());
        if kept[self.index]
            .as_ref()
            .is_some_and(|entry| Arc::ptr_eq(&entry.state, &self.state))
        {
            kept[self.index] = None;
        }
    }
}

impl PublicationOwner {
    /// Native work mutates only the owner borrowed from its admitted keeper.
    fn publish_in_place(
        &mut self,
        after_native_publication: &dyn Fn() -> Result<()>,
    ) -> Result<()> {
        ensure!(
            self.phase == DownloadPublicationPhase::OwnedPartial,
            "Publication already attempted"
        );
        let audio = &mut self.audio;
        audio.cancel.check()?;
        audio.execution_lease.check(&audio.context)?;
        audio.part.check_execution()?;
        audio.part.sync()?;
        audio.part.verify()?;
        let (bytes, sha256) = audio.part.hash(&audio.cancel)?;
        ensure!(
            bytes == audio.checkpoint.bytes && sha256 == audio.checkpoint.sha256,
            "Verified audio changed before native publication"
        );
        audio.cancel.check()?;
        audio.execution_lease.check(&audio.context)?;
        audio.part.check_execution()?;
        self.phase = DownloadPublicationPhase::PublicationAttempted;
        platform::rename_no_replace(
            &audio.part.file,
            audio.part.plan.grant.parent(),
            &audio.part.record.basename,
            &audio.part.plan.basename,
            &audio.part.record.identity,
        )?;
        after_native_publication()?;
        audio.cancel.check()?;
        audio.execution_lease.check(&audio.context)?;
        audio.part.check_execution()?;
        self.verify_published_body()?;
        self.phase = DownloadPublicationPhase::Published;
        Ok(())
    }
}

#[cfg(all(test, windows))]
#[path = "publication_slot_tests.rs"]
mod tests;

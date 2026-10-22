use super::*;
use crate::audio::worker_owner::{WorkerKind, WorkerPermit};

pub(super) type ProducerKey = (String, u64, &'static str);
pub(super) enum DeferredProducerCompletion {
    Transfer(String, u64, Result<Box<crate::download::TransferJob>>),
    Postprocess(String, u64, Result<Box<PreparedDownloadOutput>>),
    Recovery(
        String,
        String,
        u64,
        u64,
        Result<Box<RecoveredDownloadOutput>>,
    ),
    Publication {
        id: String,
        epoch: u64,
        original: crate::download::TransferCheckpoint,
        stages: DownloadStageRecords,
        original_audio: Option<Box<crate::download::VerifiedOwnedAudio>>,
        result: Result<(
            crate::download::CommittedAudio,
            Option<crate::download::CommittedSidecar>,
        )>,
    },
}
impl DeferredProducerCompletion {
    fn key(&self) -> ProducerKey {
        match self {
            Self::Transfer(id, epoch, _) => (id.clone(), *epoch, "prepare"),
            Self::Postprocess(id, epoch, _) => (id.clone(), *epoch, "postprocess"),
            Self::Recovery(_, id, _, epoch, _) => (id.clone(), *epoch, "recovery"),
            Self::Publication { id, epoch, .. } => (id.clone(), *epoch, "publication"),
        }
    }
}

impl DownloadRuntime {
    pub(in crate::backend::runtime) fn register_recovery_borrows(
        &mut self,
        id: &str,
        epoch: u64,
        objects: Vec<crate::download::DownloadProducerObject>,
    ) -> Result<()> {
        ensure!(
            self.producer_joined(id, epoch, "recovery"),
            "Actual recovery worker has not joined"
        );
        ensure!(
            !objects.is_empty(),
            "Actual recovery returned no native objects"
        );
        for object in &objects {
            ensure!(
                object.producers_joined()? && object.context_binding()?.task_id == id,
                "Recovery native objects have an unjoined loan or belong to another task"
            );
        }
        let key = (id.into(), epoch, "recovery");
        match self.producer_objects.entry(key) {
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert(objects);
            }
            std::collections::hash_map::Entry::Occupied(mut entry) => {
                ensure!(
                    entry.get().is_empty(),
                    "Actual recovery native objects were already registered"
                );
                entry.insert(objects);
            }
        }
        Ok(())
    }
    pub(in crate::backend::runtime) fn release_finished_stage(
        &mut self,
        id: &str,
        epoch: u64,
        stage: &'static str,
    ) -> Result<()> {
        let key = (id.into(), epoch, stage);
        if let Some(worker) = self.producer_workers.get(&key) {
            ensure!(
                worker.wait(Duration::ZERO),
                "Actual native stage has not joined before retry"
            );
        } else {
            ensure!(
                !self.producer_objects.contains_key(&key),
                "Native stage lost its actual worker tracker"
            );
        }
        self.producer_objects.remove(&key);
        self.producer_workers.remove(&key);
        Ok(())
    }
    pub(in crate::backend::runtime) fn spawn_producer(
        &mut self,
        name: &str,
        key: ProducerKey,
        objects: Vec<crate::download::DownloadProducerObject>,
        work: impl FnOnce() + Send + 'static,
    ) -> Result<()> {
        ensure!(
            !self.producer_workers.contains_key(&key),
            "Download producer operation already admitted"
        );
        let permit = WorkerPermit::reserve(WorkerKind::Download)?;
        let ticket = permit.tracker();
        let (start, ready) = std::sync::mpsc::sync_channel(1);
        let handle = thread::Builder::new().name(name.into()).spawn(move || {
            if ready.recv().is_err() {
                return;
            }
            let _scope = crate::download::ProducerWorkScope::enter(ticket)
                .expect("Attached download producer does not match its current thread");
            work();
        })?;
        let worker = permit.attach(handle);
        self.producer_workers.insert(key.clone(), worker.clone());
        self.producer_objects.insert(key, objects.clone());
        for object in objects {
            object.bind_worker(worker.clone())?;
        }
        start
            .send(())
            .context("Cannot start actual admitted download producer")?;
        Ok(())
    }
    pub(in crate::backend::runtime) fn producer_joined(
        &self,
        id: &str,
        epoch: u64,
        stage: &'static str,
    ) -> bool {
        self.producer_workers
            .get(&(id.into(), epoch, stage))
            .is_some_and(|worker| worker.wait(Duration::ZERO))
    }
    pub(in crate::backend::runtime) fn publication_objects(
        &self,
        id: &str,
        epoch: u64,
    ) -> Result<Vec<crate::download::DownloadProducerObject>> {
        let stage = if self.producer_joined(id, epoch, "publication") {
            "publication"
        } else {
            "recovery"
        };
        ensure!(
            self.producer_joined(id, epoch, stage),
            "Actual publisher or recovery worker has not joined"
        );
        self.producer_objects
            .get(&(id.into(), epoch, stage))
            .cloned()
            .context("Actual publisher native objects are unavailable")
    }
    pub(in crate::backend::runtime) fn release_publication_borrows(
        &mut self,
        id: &str,
        epoch: u64,
    ) -> Result<()> {
        ensure!(
            self.producer_joined(id, epoch, "publication")
                || self.producer_joined(id, epoch, "recovery"),
            "Actual publisher or recovery worker has not joined"
        );
        for stage in [
            "prepare",
            "postprocess",
            "publication",
            "recovery",
            "library",
        ] {
            let key = (id.into(), epoch, stage);
            if let Some(worker) = self.producer_workers.get(&key) {
                ensure!(
                    worker.wait(Duration::ZERO),
                    "Actual native loan has not joined before publisher borrows release"
                );
            }
            self.producer_objects.remove(&key);
            self.producer_workers.remove(&key);
        }
        self.auxiliary_tasks.remove(id);
        Ok(())
    }
}

impl Runtime {
    pub(super) fn poll_deferred_download_producers(&mut self) {
        let mut ready = Vec::new();
        let mut pending = VecDeque::new();
        while let Some(completion) = self.downloads.deferred_producers.pop_front() {
            let (id, epoch, stage) = completion.key();
            if self.downloads.producer_joined(&id, epoch, stage) {
                ready.push(completion);
            } else {
                pending.push_back(completion);
            }
        }
        self.downloads.deferred_producers = pending;
        for completion in ready {
            match completion {
                DeferredProducerCompletion::Transfer(id, epoch, result) => {
                    self.finish_download_transfer_preparation(id, epoch, result)
                }
                DeferredProducerCompletion::Postprocess(id, epoch, result) => {
                    self.finish_download_postprocess_preparation(id, epoch, result)
                }
                DeferredProducerCompletion::Recovery(operation, id, revision, epoch, result) => {
                    self.finish_download_recovery(operation, id, revision, epoch, result)
                }
                DeferredProducerCompletion::Publication {
                    id,
                    epoch,
                    original,
                    stages,
                    original_audio,
                    result,
                } => self.finish_download_audio_commit(
                    id,
                    epoch,
                    original,
                    stages,
                    original_audio,
                    result,
                ),
            }
        }
    }
}

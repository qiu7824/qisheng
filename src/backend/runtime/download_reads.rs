//! Native read adoption follows the complete download publication transaction.

use super::*;
use crate::download::{
    CommittedAudio, CommittedSidecar, DownloadProducerObject, task_model::DownloadTask,
};
use crate::library::authority::{DownloadedReadReceipt, FileAuthorization};
mod shutdown;

#[derive(Default)]
pub(super) struct DownloadReadRuntime {
    closing: bool,
    pub(super) jobs: HashMap<String, ReadAdoptionJob>,
    pub(super) recovering: HashMap<String, ColdReadJob>,
}
pub(super) struct ColdReadJob {
    task: DownloadTask,
    operation: String,
    result: Arc<Mutex<Option<Result<ColdReadResult>>>>,
    retained: Option<ColdReadResult>,
    error: Option<String>,
    borrows_registered: bool,
    shutdown_pins: Vec<crate::library::authority::DownloadedPinReceipt>,
}
struct ColdReadResult {
    facts: CommittedAudio,
    objects: Vec<DownloadProducerObject>,
}
pub(super) struct ReadAdoptionJob {
    task: DownloadTask,
    facts: CommittedAudio,
    producer: DownloadProducerObject,
    sidecar: Option<(CommittedSidecar, DownloadProducerObject)>,
    started: bool,
    result: Arc<Mutex<Option<Result<ReadPrepared>>>>,
    retained: Option<ReadPrepared>,
    error: Option<String>,
    play_request: Option<String>,
    shutdown_pins: Vec<crate::library::authority::DownloadedPinReceipt>,
}
struct ReadPrepared {
    local: Option<crate::catalog::PreparedLocalImport>,
    authorization: FileAuthorization,
    receipt: DownloadedReadReceipt,
    metadata_committed: bool,
    writer_released: bool,
    item: crate::domain::library::LibraryPreparedAsset,
    track_id: Option<String>,
    sidecar: Option<(FileAuthorization, DownloadedReadReceipt)>,
    sidecar_released: bool,
}

impl Runtime {
    pub(super) fn begin_download_read_adoption(
        &mut self,
        task: DownloadTask,
        facts: CommittedAudio,
    ) -> Result<()> {
        ensure!(
            !self.download_reads.closing,
            "Downloaded read admission is closing"
        );
        ensure!(
            !self.download_reads.jobs.contains_key(&task.id),
            "Download read preparation already exists"
        );
        ensure!(
            self.download_reads.jobs.len() < crate::download::MAX_DOWNLOAD_TASKS,
            "Downloaded read preparation capacity reached"
        );
        let objects = self
            .downloads
            .publication_objects(&task.id, task.operation_epoch)?;
        let producer = objects
            .iter()
            .cloned()
            .find(|object| {
                object
                    .record_binding()
                    .is_ok_and(|record| record.identity == facts.identity)
            })
            .context("Published output has no original native producer loan")?;
        let sidecar = task
            .stages
            .lyrics
            .artifact
            .as_ref()
            .map(|proof| -> Result<_> {
                let object = objects
                    .iter()
                    .find(|object| {
                        object
                            .record_binding()
                            .is_ok_and(|record| record.identity == proof.identity)
                    })
                    .context("Published sidecar has no original native producer loan")?
                    .clone();
                let sidecar = CommittedSidecar {
                    path: facts
                        .path
                        .parent()
                        .context("Downloaded file has no parent")?
                        .join(&proof.basename),
                    identity: proof.identity.clone(),
                    bytes: proof.bytes,
                    sha256: proof.sha256.clone(),
                    retained_part: None,
                };
                Ok((sidecar, object))
            })
            .transpose()?;
        ensure!(
            producer.context_binding()?.profile_id == task.profile_id,
            "Download read profile changed"
        );
        self.download_reads.jobs.insert(
            task.id.clone(),
            ReadAdoptionJob {
                task,
                facts,
                producer,
                sidecar,
                started: false,
                result: Arc::new(Mutex::new(None)),
                retained: None,
                error: None,
                play_request: None,
                shutdown_pins: Vec::new(),
            },
        );
        Ok(())
    }

    pub(super) fn poll_download_read_adoptions(&mut self) {
        if self.download_reads.closing {
            self.poll_download_read_shutdown_step();
            return;
        }
        self.poll_cold_download_reads();
        let active = self
            .download_reads
            .jobs
            .values()
            .filter(|job| {
                job.started
                    && !self.downloads.producer_joined(
                        &job.task.id,
                        job.task.operation_epoch,
                        "library",
                    )
            })
            .count();
        let pending = self
            .download_reads
            .jobs
            .iter()
            .filter(|(_, job)| !job.started && job.error.is_none())
            .map(|(id, _)| id.clone())
            .take(crate::download::MAX_DOWNLOAD_WORKERS.saturating_sub(active))
            .collect::<Vec<_>>();
        for id in pending {
            let job = self
                .download_reads
                .jobs
                .get(&id)
                .expect("Admitted download read job");
            let epoch = job.task.operation_epoch;
            let producer = job.producer.clone();
            let facts = job.facts.clone();
            let sidecar = job.sidecar.clone();
            let result = job.result.clone();
            let authority = self.library.authority.clone();
            let catalog = self.catalog.clone();
            let repository = self.repository.clone();
            #[cfg(test)]
            let read_task_id = id.clone();
            let mut loans = vec![producer.clone()];
            if let Some((_, object)) = &sidecar {
                loans.push(object.clone());
            }
            let started = self.downloads.spawn_producer(
                "download-library-read",
                (id.clone(), epoch, "library"),
                loans,
                move || {
                    let prepared = (|| -> Result<ReadPrepared> {
                        let nonce = producer.record_binding()?.nonce;
                        let (authorization, receipt) =
                            authority.stage_download_output(&producer, &facts, &nonce)?;
                        let sidecar = sidecar
                            .map(|(facts, object)| -> Result<_> {
                                let nonce = object.record_binding()?.nonce;
                                authority.stage_download_sidecar(&object, &facts, &nonce)
                            })
                            .transpose()?;
                        let mut local = catalog.prepare_authorized(authorization.open()?)?;
                        let item = repository
                            .prepare_library_identity(&local.library_asset(), &|| {
                                receipt.verify()
                            })?;
                        local.bind_library_identity(&item)?;
                        receipt.verify()?;
                        Ok(ReadPrepared {
                            local: Some(local),
                            authorization,
                            receipt,
                            metadata_committed: false,
                            writer_released: false,
                            item,
                            track_id: None,
                            sidecar,
                            sidecar_released: false,
                        })
                    })();
                    *result.lock().unwrap_or_else(|error| error.into_inner()) = Some(prepared);
                    #[cfg(test)]
                    shutdown::after_read_result(
                        repository.path().parent().expect("Profile directory"),
                        &read_task_id,
                        "library",
                    );
                },
            );
            let job = self
                .download_reads
                .jobs
                .get_mut(&id)
                .expect("Admitted download read job");
            match started {
                Ok(()) => job.started = true,
                Err(error) => {
                    job.error = Some(format!("{error:#}"));
                    let operation = job.play_request.take();
                    if let Some(operation) = operation {
                        self.publish_finished(operation, &Err(anyhow::anyhow!("{error:#}")));
                    }
                }
            }
        }
        let ready = self
            .download_reads
            .jobs
            .iter()
            .filter(|(_, job)| {
                job.started
                    && job.error.is_none()
                    && self.downloads.producer_joined(
                        &job.task.id,
                        job.task.operation_epoch,
                        "library",
                    )
            })
            .map(|(id, _)| id.clone())
            .take(crate::download::MAX_DOWNLOAD_WORKERS)
            .collect::<Vec<_>>();
        for id in ready {
            self.finish_download_read_adoption(&id);
        }
    }

    fn finish_download_read_adoption(&mut self, id: &str) {
        let Some(mut job) = self.download_reads.jobs.remove(id) else {
            return;
        };
        let finish = (|| -> Result<String> {
            ensure!(
                self.downloads
                    .producer_joined(id, job.task.operation_epoch, "library"),
                "Native read preparation has not actually joined"
            );
            if job.retained.is_none() {
                job.retained = Some(
                    job.result
                        .lock()
                        .unwrap_or_else(|error| error.into_inner())
                        .take()
                        .context("Joined native read worker returned no owner")??,
                );
            }
            let prepared = job
                .retained
                .as_mut()
                .context("Download read candidate missing")?;
            prepared.receipt.verify()?;
            let item = &prepared.item;
            let current = self
                .repository
                .download_task(id)?
                .context("Download read task disappeared")?;
            ensure!(
                current.revision >= job.task.revision
                    && current.output_audio == job.task.output_audio
                    && current.operation_epoch == job.task.operation_epoch,
                "Download changed before its read adoption"
            );
            if !prepared.metadata_committed {
                let commit = self.repository.commit_download_library_metadata(
                    &current,
                    item,
                    &prepared.receipt,
                    Some(self.revision),
                    &|| prepared.receipt.verify(),
                )?;
                self.revision = commit
                    .change
                    .as_ref()
                    .context("Downloaded metadata revision missing")?
                    .revision;
                prepared.receipt.acknowledge_repository_commit(&commit)?;
                prepared.metadata_committed = true;
            }
            let binding = self
                .repository
                .library_asset_binding(&item.asset.id)?
                .context("Downloaded metadata binding missing")?;
            let authorization = self.library.authority.install_download_output(
                &item.asset.id,
                binding.binding_revision,
                &prepared.authorization,
            )?;
            if !prepared.writer_released {
                job.producer
                    .release_adopted_after_join(&job.facts, &prepared.receipt)?;
                prepared.writer_released = true;
            }
            if !prepared.sidecar_released {
                if let Some((_, receipt)) = &prepared.sidecar {
                    let commit = self
                        .repository
                        .acknowledge_download_sidecar_read(&current, receipt)?;
                    receipt.acknowledge_repository_commit(&commit)?;
                    let (facts, object) = job
                        .sidecar
                        .as_ref()
                        .context("Sidecar native producer loan missing")?;
                    object.release_adopted_sidecar_after_join(facts, receipt)?;
                }
                prepared.sidecar_released = true;
                // The restricted sidecar reader and its original pins retire
                // through the existing bounded Library native owner.
                prepared.sidecar.take();
                job.sidecar.take();
            }
            if prepared.track_id.is_none() {
                let mut local = prepared
                    .local
                    .take()
                    .context("Prepared downloaded metadata missing")?;
                local.set_authorization(authorization);
                prepared.track_id = Some(self.catalog.commit_local(local).id);
            }
            self.downloads
                .release_publication_borrows(id, job.task.operation_epoch)?;
            Ok(prepared
                .track_id
                .clone()
                .expect("Committed downloaded catalog track"))
        })();
        match finish {
            Ok(track_id) => {
                if let Some(operation) = job.play_request {
                    let result = self.play(&track_id);
                    self.publish_finished(operation, &result);
                }
            }
            Err(error) => {
                job.error = Some(format!("{error:#}").chars().take(1024).collect());
                if let Some(operation) = job.play_request.take() {
                    self.publish_finished(operation, &Err(anyhow::anyhow!("{error:#}")));
                }
                self.error(format!(
                    "Download read adoption retained its owner: {error:#}"
                ));
                self.download_reads.jobs.insert(id.into(), job);
            }
        }
    }

    pub(super) fn play_download_task(&mut self, id: String) -> Result<bool> {
        ensure!(
            !self.download_reads.closing,
            "Downloaded read admission is closing"
        );
        if let Some(job) = self.download_reads.recovering.get_mut(&id) {
            ensure!(
                job.error.is_some(),
                "Downloaded native read recovery is already active"
            );
            if job.retained.is_some() {
                job.error = None;
                job.operation = self.request_id.clone();
                self.poll_cold_download_reads();
            } else {
                let epoch = job.task.operation_epoch;
                self.downloads
                    .release_finished_stage(&id, epoch, "recovery")?;
                self.download_reads.recovering.remove(&id);
                let task = self
                    .repository
                    .download_task(&id)?
                    .context("Downloaded recovery task disappeared")?;
                self.begin_cold_download_read(task, self.request_id.clone())?;
            }
            self.async_started = true;
            return Ok(true);
        }
        if let Some(job) = self.download_reads.jobs.get_mut(&id) {
            if job.error.is_some() {
                if job.retained.is_none() {
                    let epoch = job.task.operation_epoch;
                    self.downloads
                        .release_finished_stage(&id, epoch, "library")?;
                    job.started = false;
                    *job.result.lock().unwrap_or_else(|error| error.into_inner()) = None;
                }
                job.error = None;
            }
            ensure!(
                job.play_request.is_none(),
                "Download playback already awaits read adoption"
            );
            job.play_request = Some(self.request_id.clone());
            self.poll_download_read_adoptions();
            self.async_started = true;
            return Ok(true);
        }
        let task = self
            .repository
            .download_task(&id)?
            .context("Download task is unavailable")?;
        ensure!(task.audio_available, "Download task has no published audio");
        let stored = self
            .repository
            .download_destinations()?
            .into_iter()
            .find(|grant| grant.id == task.destination.grant_id)
            .context("Downloaded destination metadata is unavailable")?;
        let path = task
            .destination
            .relative_components
            .iter()
            .fold(stored.path, |path, part| path.join(part));
        let asset = self.repository.library_asset_at_path(&path)?;
        let Some(asset) = asset else {
            self.begin_cold_download_read(task, self.request_id.clone())?;
            self.async_started = true;
            return Ok(true);
        };
        let proof = task
            .output_audio
            .as_ref()
            .context("Downloaded output proof is unavailable")?;
        let identity = crate::domain::library::FileIdentity::from_native_parts(
            proof.identity.device,
            &proof.identity.object,
        )?;
        ensure!(
            self.repository
                .local_asset_file_identity(&asset.id)?
                .as_ref()
                == Some(&identity),
            "Downloaded output has been replaced by another physical file"
        );
        if !self.catalog.has_download_read_authority(&asset.track_id) {
            self.begin_cold_download_read(task, self.request_id.clone())?;
            self.async_started = true;
            return Ok(true);
        }
        self.play(&asset.track_id)?;
        Ok(false)
    }

    fn begin_cold_download_read(&mut self, task: DownloadTask, operation: String) -> Result<()> {
        ensure!(
            self.download_reads.recovering.len() + self.download_reads.jobs.len()
                < crate::download::MAX_DOWNLOAD_TASKS,
            "Downloaded native read recovery capacity reached"
        );
        task.validate()?;
        let stored = self
            .repository
            .download_destinations()?
            .into_iter()
            .find(|grant| grant.id == task.destination.grant_id)
            .context("Downloaded selected destination missing")?;
        task.require_current_destination(&stored)?;
        let expected_profile = crate::domain::music::stable_id(
            "download-profile",
            self.repository
                .path()
                .parent()
                .context("Profile directory missing")?
                .to_string_lossy()
                .as_bytes(),
        );
        ensure!(
            task.profile_id == expected_profile,
            "Downloaded read intent belongs to another profile"
        );
        let allocations = self.repository.download_owned_artifacts(Some(&task.id))?;
        let proof = task
            .output_audio
            .as_ref()
            .context("Downloaded output proof missing")?
            .clone();
        let audio_allocation = allocations
            .iter()
            .find(|artifact| {
                artifact.phase == "published"
                    && artifact.allocation.record.identity == proof.identity
                    && artifact.actual_bytes == proof.bytes
                    && artifact.sha256.as_deref() == Some(proof.sha256.as_str())
            })
            .context("Downloaded read intent has no complete local publication ACK")?
            .allocation
            .clone();
        let sidecar_allocation = task
            .stages
            .lyrics
            .artifact
            .as_ref()
            .map(|proof| -> Result<_> {
                let actual = allocations
                    .iter()
                    .find(|artifact| {
                        artifact.phase == "published"
                            && artifact.allocation.record.identity == proof.identity
                            && artifact.actual_bytes == proof.bytes
                            && artifact.sha256.as_deref() == Some(proof.sha256.as_str())
                    })
                    .context("Sidecar read intent has no complete local publication ACK")?;
                Ok((actual.allocation.clone(), proof.clone()))
            })
            .transpose()?;
        let result = Arc::new(Mutex::new(None));
        self.download_reads.recovering.insert(
            task.id.clone(),
            ColdReadJob {
                task: task.clone(),
                operation,
                result: result.clone(),
                retained: None,
                error: None,
                borrows_registered: false,
                shutdown_pins: Vec::new(),
            },
        );
        let repository = self.repository.clone();
        let id = task.id.clone();
        let epoch = task.operation_epoch;
        let started = self.downloads.spawn_producer(
            "download-completed-read-select",
            (id.clone(), epoch, "recovery"),
            vec![],
            move || {
                let recovered = (|| -> Result<ColdReadResult> {
                    let cancel = crate::download::DownloadCancel::default();
                    let mut selected = crate::download::DestinationGrant::open_selected(
                        &stored.path,
                        stored.epoch,
                    )?;
                    ensure!(
                        selected.identity() == &stored.identity,
                        "Selected download destination physical identity changed"
                    );
                    if task.destination.relative_components.len() == 2 {
                        selected = selected
                            .open_existing_group(&task.destination.relative_components[0])?;
                    }
                    let object = selected.recover_owned_artifact_retained(
                        &audio_allocation,
                        &proof,
                        &cancel,
                        &|actual, proof| {
                            repository.acknowledge_download_artifact_recovery(
                                actual,
                                proof,
                                &|| cancel.check(),
                            )
                        },
                    )?;
                    let facts = object.verify_published_audio(
                        &proof,
                        task.actual_quality.extension(),
                        &cancel,
                    )?;
                    let mut objects = vec![object];
                    if let Some((allocation, proof)) = sidecar_allocation {
                        objects.push(selected.recover_owned_artifact_retained(
                            &allocation,
                            &proof,
                            &cancel,
                            &|actual, proof| {
                                repository.acknowledge_download_artifact_recovery(
                                    actual,
                                    proof,
                                    &|| cancel.check(),
                                )
                            },
                        )?);
                    }
                    Ok(ColdReadResult { facts, objects })
                })();
                *result.lock().unwrap_or_else(|error| error.into_inner()) = Some(recovered);
                #[cfg(test)]
                shutdown::after_read_result(
                    repository.path().parent().expect("Profile directory"),
                    &task.id,
                    "recovery",
                );
            },
        );
        if let Err(error) = started {
            self.download_reads
                .recovering
                .get_mut(&id)
                .expect("Admitted cold read selection")
                .error = Some(format!("{error:#}"));
            return Err(error);
        }
        Ok(())
    }

    fn poll_cold_download_reads(&mut self) {
        let ready = self
            .download_reads
            .recovering
            .iter()
            .filter(|(_, job)| {
                job.error.is_none()
                    && self.downloads.producer_joined(
                        &job.task.id,
                        job.task.operation_epoch,
                        "recovery",
                    )
            })
            .map(|(id, _)| id.clone())
            .take(crate::download::MAX_DOWNLOAD_WORKERS)
            .collect::<Vec<_>>();
        for id in ready {
            let mut job = self
                .download_reads
                .recovering
                .remove(&id)
                .expect("Admitted cold read recovery");
            let outcome = (|| -> Result<()> {
                if job.retained.is_none() {
                    job.retained = Some(
                        job.result
                            .lock()
                            .unwrap_or_else(|error| error.into_inner())
                            .take()
                            .context("Joined cold read worker returned no result")??,
                    );
                }
                let recovered = job
                    .retained
                    .as_ref()
                    .context("Actual cold read owner missing")?;
                if !job.borrows_registered {
                    self.downloads.register_recovery_borrows(
                        &id,
                        job.task.operation_epoch,
                        recovered.objects.clone(),
                    )?;
                    job.borrows_registered = true;
                }
                let current = self
                    .repository
                    .download_task(&id)?
                    .context("Cold read task disappeared")?;
                ensure!(
                    current.output_audio == job.task.output_audio
                        && current.operation_epoch == job.task.operation_epoch,
                    "Downloaded read target changed during native selection"
                );
                self.begin_download_read_adoption(current, recovered.facts.clone())?;
                self.download_reads
                    .jobs
                    .get_mut(&id)
                    .context("Native downloaded read preparation missing")?
                    .play_request = Some(job.operation.clone());
                Ok(())
            })();
            if let Err(error) = outcome {
                job.error = Some(format!("{error:#}").chars().take(1024).collect());
                self.publish_finished(job.operation.clone(), &Err(anyhow::anyhow!("{error:#}")));
                self.download_reads.recovering.insert(id, job);
            }
        }
    }
}

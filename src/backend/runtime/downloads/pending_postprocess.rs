//! Auxiliary outputs consume at most four of the original 104 native slots.

use super::*;

impl Runtime {
    pub(super) fn poll_pending_download_postprocess(&mut self) {
        let mut pending = std::mem::take(&mut self.downloads.postprocess_queue);
        while let Some(mut job) = pending.pop_front() {
            let id = job.task.id.clone();
            let epoch = job.task.operation_epoch;
            let stopped = self.downloads.closing
                || self
                    .downloads
                    .staging
                    .get(&id)
                    .is_some_and(|control| control.cancel.reason().is_some());
            if stopped {
                let retired = self.retire_pending_download_postprocess(&mut job);
                match retired {
                    Ok(true) => continue,
                    Ok(false) => {}
                    Err(error) => self.note_pending_download_postprocess_error(&mut job, error),
                }
                self.downloads.postprocess_queue.push_back(job);
                continue;
            }
            if self.downloads.auxiliary_tasks.len() >= 2 || !self.downloads.retry_jobs.is_empty() {
                self.downloads.postprocess_queue.push_back(job);
                continue;
            }
            let lease = match self.downloads.leases.get(&id) {
                Some(lease) => lease.clone(),
                None => {
                    self.note_pending_download_postprocess_error(
                        &mut job,
                        anyhow::anyhow!("Pending verified download has no execution lease"),
                    );
                    self.downloads.postprocess_queue.push_back(job);
                    continue;
                }
            };
            let Some(control) = self.downloads.staging.get_mut(&id) else {
                self.note_pending_download_postprocess_error(
                    &mut job,
                    anyhow::anyhow!("Pending verified download lost its stage control"),
                );
                self.downloads.postprocess_queue.push_back(job);
                continue;
            };
            control.deadline = Instant::now() + Duration::from_secs(45);
            let cancel = control.cancel.clone();
            let metadata_cancel = control.metadata_cancel.clone();
            let objects = vec![job.audio.producer_object()];
            let slot = Arc::new(Mutex::new(Some(job)));
            let worker_slot = slot.clone();
            let completed = self.completed_tx.clone();
            let repository = self.repository.clone();
            let providers = self.provider_registry.clone();
            let task_id = id.clone();
            let spawned = self.downloads.spawn_producer(
                "download-postprocess",
                (id.clone(), epoch, "postprocess"),
                objects,
                move || {
                    let job = worker_slot
                        .lock()
                        .unwrap_or_else(|error| error.into_inner())
                        .take()
                        .expect("Owned pending download postprocessing");
                    let journal = DownloadArtifactJournal {
                        repository: repository.clone(),
                        cancel: cancel.clone(),
                        lease,
                    };
                    let result = prepare_download_output(
                        job.audio,
                        &job.task,
                        &repository,
                        &providers,
                        &cancel,
                        metadata_cancel,
                        &journal,
                    )
                    .map(Box::new);
                    let _ = completed.send(Completion::DownloadPostprocessPrepared {
                        task_id,
                        operation_epoch: epoch,
                        result,
                    });
                },
            );
            match spawned {
                Ok(()) => {
                    self.downloads.auxiliary_tasks.insert(id);
                }
                Err(error) => {
                    let retained = slot
                        .lock()
                        .unwrap_or_else(|error| error.into_inner())
                        .take();
                    if let Some(mut job) = retained {
                        self.note_pending_download_postprocess_error(&mut job, error);
                        self.downloads.postprocess_queue.push_back(job);
                    } else {
                        // Admission may have bound a real worker before its start gate failed.
                        // Its global native objects and tracker remain held for recovery.
                        self.downloads.auxiliary_tasks.insert(id);
                        self.error(format!(
                            "Download postprocessing admission retains its native owner: {error:#}"
                        ));
                    }
                }
            }
        }
    }

    fn note_pending_download_postprocess_error(
        &mut self,
        job: &mut PendingPostprocess,
        error: anyhow::Error,
    ) {
        let message = format!("{error:#}").chars().take(1024).collect::<String>();
        if job.error.as_deref() != Some(&message) {
            self.error(format!(
                "Download postprocessing remains pending: {message}"
            ));
            job.error = Some(message);
        }
    }

    fn retire_pending_download_postprocess(
        &mut self,
        job: &mut PendingPostprocess,
    ) -> Result<bool> {
        let id = &job.task.id;
        let epoch = job.task.operation_epoch;
        let mut task = self
            .repository
            .download_task(id)?
            .context("Pending original download task missing")?;
        ensure!(
            task.operation_epoch == epoch && !task.audio_available,
            "Pending original download moved to another operation"
        );
        let original = job.audio.producer_object();
        if !original.producers_joined()? {
            return Ok(false);
        }
        if job.retirement_pins.is_none() {
            job.retirement_pins = Some(
                self.library
                    .authority
                    .stage_download_pin_retirement(&original)?,
            );
        }
        if !original.close_for_cold_recovery_after_join(job.retirement_pins.as_ref())? {
            return Ok(false);
        }
        if matches!(
            task.state,
            DownloadState::PostProcessing | DownloadState::Pausing
        ) {
            let revision = task.revision;
            task.revision += 1;
            task.operation_epoch = task
                .operation_epoch
                .checked_add(1)
                .context("Pending download epoch overflow")?;
            task.state = DownloadState::Paused;
            task.pause_reason = Some(DownloadPauseReason::User);
            task.updated_at_ms = download_now()?;
            self.revision = self
                .repository
                .update_download_task(&task, revision, epoch, Some(self.revision), &|| Ok(()))?
                .revision;
        }
        self.downloads
            .release_finished_stage(id, epoch, "prepare")?;
        self.downloads.staging.remove(id);
        self.downloads.leases.remove(id);
        job.retirement_pins.take();
        Ok(true)
    }
}

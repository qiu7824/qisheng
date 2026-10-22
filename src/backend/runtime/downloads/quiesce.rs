//! One bounded polling step over the actual transfer and postprocessing owners.

use super::*;

impl Runtime {
    /// This runs after admission closes. Finished retry jobs are actually joined
    /// by the existing poller; incomplete handles and recovery owners stay owned.
    pub(in crate::backend::runtime) fn poll_download_quiesce_step(&mut self) -> Result<bool> {
        ensure!(
            self.downloads.closing,
            "Download handover requires closed admission"
        );
        self.poll_downloads();
        let pool_joined = match self.downloads.pool.as_ref() {
            None => true,
            Some(pool) => match pool.shutdown(Duration::ZERO) {
                Ok(()) => true,
                Err(crate::download::DownloadShutdownError::Timeout) => false,
                Err(crate::download::DownloadShutdownError::WorkerPanicked) => {
                    anyhow::bail!("Download transfer worker panicked during handover");
                }
            },
        };
        self.poll_download_postprocessing_retries();
        if !pool_joined
            || self
                .downloads
                .producer_workers
                .values()
                .any(|worker| !worker.wait(Duration::ZERO))
            || !self.downloads.deferred_producers.is_empty()
            || !self.downloads.postprocess_queue.is_empty()
            || !self.download_reads.jobs.is_empty()
            || !self.download_reads.recovering.is_empty()
            || !self.downloads.active.is_empty()
            || !self.downloads.preparing.is_empty()
            || !self.downloads.staging.is_empty()
            || !self.downloads.cleaning.is_empty()
            || self.downloads.resolving.is_some()
            || self
                .downloads
                .retry_jobs
                .values()
                .any(|job| job.worker.is_some())
        {
            return Ok(false);
        }
        ensure!(
            self.downloads.retry_jobs.is_empty(),
            "Download recovery retains actual postprocessing files or publication witnesses"
        );
        let profile_id = crate::domain::music::stable_id(
            "download-profile",
            self.repository
                .path()
                .parent()
                .context("Profile directory missing")?
                .to_string_lossy()
                .as_bytes(),
        );
        // A failed publication may never reach READ adoption. Its joined
        // producer objects still own the original native handles and pins;
        // merely waiting for the process keeper cannot retire those borrows.
        // Close those actual objects for cold recovery and preserve their
        // durable files/ledger. No serialized path is reopened or removed.
        let objects = crate::download::retained_download_profile_objects(&profile_id)?;
        for object in &objects {
            ensure!(
                object.context_binding()?.profile_id == profile_id,
                "Download retirement belongs to another profile"
            );
            if !object.producers_joined()? {
                return Ok(false);
            }
            if object.close_for_cold_recovery_after_join(None)? {
                continue;
            }
            let covered = object.inspect_retained_pins_after_join(|grant| {
                Ok(self
                    .downloads
                    .quiesce_pins
                    .iter()
                    .position(|receipt| receipt.matches_grant(grant).is_ok()))
            })?;
            let index = match covered {
                Some(index) => index,
                None => {
                    self.downloads.quiesce_pins.push(
                        self.library
                            .authority
                            .stage_download_pin_retirement(object)?,
                    );
                    self.downloads.quiesce_pins.len() - 1
                }
            };
            if !object
                .close_for_cold_recovery_after_join(Some(&self.downloads.quiesce_pins[index]))?
            {
                return Ok(false);
            }
        }
        self.downloads.producer_objects.clear();
        self.downloads.producer_workers.clear();
        self.downloads.auxiliary_tasks.clear();
        self.downloads.quiesce_pins.clear();
        if crate::download::has_retained_download_objects(&profile_id)? {
            return Ok(false);
        }
        Ok(self.downloads.active.is_empty()
            && self.downloads.preparing.is_empty()
            && self.downloads.staging.is_empty()
            && self.downloads.cleaning.is_empty()
            && self.downloads.resolving.is_none())
    }
}

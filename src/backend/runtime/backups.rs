use super::*;
use std::io::Read;

pub(super) fn recover_backup_preferences(
    repository: &Repository,
    store: &ProjectStore,
    data: &mut StoreData,
) -> Result<()> {
    for receipt in repository.pending_lx_backup_preferences()? {
        if !store.backup_preferences_applied(&receipt.operation_id, &receipt.input_digest)? {
            let candidate = receipt
                .settings
                .as_ref()
                .context("Pending backup settings are missing")?
                .project_store(data)?;
            store.apply_backup_preferences(
                &receipt.operation_id,
                &receipt.input_digest,
                &candidate,
            )?;
        }
        // Loading the saved state preserves changes made after an earlier application.
        *data = store.load()?;
        let digest = format!("{:x}", Sha256::digest(serde_json::to_vec(data)?));
        repository.record_profile_change(&digest)?;
        repository
            .acknowledge_lx_backup_preferences(&receipt.operation_id, &receipt.input_digest)?;
    }
    Ok(())
}

impl Runtime {
    fn recover_backup_preferences_runtime(&mut self) -> Result<()> {
        self.system_control.require_store_mutation()?;
        if self.repository.pending_lx_backup_preferences()?.is_empty() {
            return Ok(());
        }
        let result = recover_backup_preferences(&self.repository, &self.store, &mut self.data);
        self.revision = self.repository.revision()?;
        self.audio.set_volume(self.data.volume);
        self.audio
            .set_output_preferences(self.data.audio_preferences.clone())?;
        self.audio.set_equalizer(self.data.equalizer)?;
        self.audio.set_processing(self.data.playback_processing)?;
        self.audio
            .set_analysis_enabled(self.data.audio_analysis_enabled)?;
        self.audio.set_spatial(self.data.spatial)?;
        result
    }
    pub(super) fn begin_backup_preview(
        &mut self,
        path: PathBuf,
        replace_tracks: bool,
        copy_list_id: Option<String>,
    ) -> Result<()> {
        ensure!(
            path.is_absolute() && path.to_str().is_some(),
            "Choose an absolute backup file path"
        );
        ensure!(self.tasks.len() < 4, "Too many pending file operations");
        let operation = self.request_id.clone();
        let completed = self.completed_tx.clone();
        let repository = self.repository.clone();
        let authority = self.library.authority.clone();
        let cancel = Arc::new(AtomicBool::new(false));
        self.file_cancels.insert(operation.clone(), cancel.clone());
        self.tasks.push(
            thread::Builder::new()
                .name("lx-backup-preview".into())
                .spawn(move || {
                    let result = (|| -> Result<crate::repository::LxBackupPreview> {
                        ensure!(!cancel.load(Ordering::Acquire), "Backup preview cancelled");
                        let selected = authority.open_selection(&path)?;
                        let mut file = selected
                            .authorization()
                            .with_cancel(cancel.clone())?
                            .open()?;
                        let length = file.metadata()?.len();
                        ensure!(
                            length <= crate::backup::MAX_BACKUP_BYTES as u64,
                            "Backup exceeds its input budget"
                        );
                        let mut bytes = Vec::with_capacity(length as usize);
                        let mut buffer = [0u8; 64 * 1024];
                        loop {
                            ensure!(!cancel.load(Ordering::Acquire), "Backup preview cancelled");
                            let count = file.read(&mut buffer)?;
                            if count == 0 {
                                break;
                            }
                            ensure!(
                                count
                                    <= crate::backup::MAX_BACKUP_BYTES.saturating_sub(bytes.len()),
                                "Backup exceeds its input budget"
                            );
                            bytes.extend_from_slice(&buffer[..count]);
                        }
                        file.check()?;
                        let backup =
                            crate::backup::decode_lx(&bytes, crate::backup::BackupEncoding::Auto)?;
                        let policy = if let Some(id) = copy_list_id {
                            crate::backup::ConflictPolicy::Copy { new_list_id: id }
                        } else if replace_tracks {
                            crate::backup::ConflictPolicy::ReplaceTracks
                        } else {
                            crate::backup::ConflictPolicy::KeepExisting
                        };
                        let preview = repository.preview_lx_backup(&backup, policy)?;
                        ensure!(!cancel.load(Ordering::Acquire), "Backup preview cancelled");
                        Ok(preview)
                    })();
                    let _ = completed.send(Completion::BackupPreviewPrepared { operation, result });
                })?,
        );
        self.async_started = true;
        Ok(())
    }
    pub(super) fn finish_backup_preview(
        &mut self,
        operation: String,
        result: Result<crate::repository::LxBackupPreview>,
    ) {
        let cancel = self.file_cancels.remove(&operation);
        let outcome = result.and_then(|preview| {
            ensure!(
                !cancel
                    .as_ref()
                    .is_some_and(|cancel| cancel.load(Ordering::Acquire)),
                "Backup preview cancelled"
            );
            self.backup_preview = Some((operation.clone(), Arc::new(preview)));
            Ok(())
        });
        self.publish_finished(operation, &outcome);
    }
    pub(super) fn backup_preview_view(&self) -> Option<BackupPreviewView> {
        self.backup_preview
            .as_ref()
            .map(|(operation, preview)| BackupPreviewView {
                operation_id: operation.clone(),
                expected_revision: preview.expected_user_revision,
                input_digest: preview.input_digest.clone(),
                imported_lists: preview.plan.list_changes.len(),
                local_references: preview.plan.local_references.len(),
                settings_present: preview.plan.settings.is_some(),
                requires_activation: preview
                    .plan
                    .settings
                    .as_ref()
                    .map_or_else(Vec::new, |settings| settings.requires_activation.clone()),
            })
    }
    pub(super) fn commit_backup_preview(&mut self, operation: &str) -> Result<()> {
        self.system_control.require_store_mutation()?;
        self.invalidate_system_artwork()?;
        self.recover_backup_preferences_runtime()?;
        let (id, preview) = self
            .backup_preview
            .as_ref()
            .context("Prepare a backup preview first")?;
        ensure!(id == operation, "Backup preview operation changed");
        let candidate = preview
            .plan
            .settings
            .as_ref()
            .map(|settings| settings.project_store(&self.data))
            .transpose()?;
        if let Some(candidate) = &candidate {
            self.audio
                .validate_equalizer_admission(&candidate.equalizer)?;
            self.audio
                .validate_processing_admission(&candidate.playback_processing)?;
            self.audio.validate_spatial_admission(candidate.spatial)?;
        }
        let committed = self
            .repository
            .commit_lx_backup(&self.request_id, preview, &|| Ok(()))?;
        self.revision = committed.change.revision;
        self.reconcile_queue()?;
        self.queue.invalidate_plan();
        self.invalidate_preload(Invalidation::List);
        if candidate.is_some() {
            self.recover_backup_preferences_runtime()?;
        }
        self.events.lock().unwrap().publish(
            self.revision,
            &self.origin,
            Some(self.request_id.clone()),
            BackendEvent::ListsChanged {
                revision: self.revision,
                action: "lx_backup_import".into(),
            },
        );
        Ok(())
    }
}

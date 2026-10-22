//! Native archive restoration through one checked repository commit and a
//! recoverable, idempotent ProjectStore preference application.

use super::*;
use crate::backup::{BackupEncoding, ConflictPolicy, MAX_BACKUP_BYTES, decode_native};
use crate::repository::{NativeBackupCommit, NativeBackupCounts, NativeBackupPreview};
use serde::Serialize;
use std::io::Read;

#[derive(Default)]
pub(super) struct NativeRestoreRuntime {
    preview: Option<(String, Arc<NativeBackupPreview>)>,
}

#[derive(Clone, Debug, Serialize)]
pub struct NativeBackupPreviewView {
    pub operation_id: String,
    pub expected_revision: u64,
    pub input_digest: String,
    pub imported_lists: usize,
    pub local_references: usize,
    pub declared_local_paths: usize,
    pub source_archives: usize,
    pub download_plans: usize,
    pub downloads_included: bool,
    pub edited_lyrics: usize,
    pub edited_lyrics_included: bool,
    pub requires_activation: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct NativeBackupImportView {
    pub operation_id: String,
    pub input_digest: String,
    pub revision: u64,
    pub replayed: bool,
    pub counts: NativeBackupCounts,
    pub preferences_applied: bool,
}

/// Runs before runtime services start. A committed native import retains its
/// safe candidate until both files acknowledge the same operation and digest.
pub(super) fn recover_native_backup_preferences(
    repository: &Repository,
    store: &ProjectStore,
    data: &mut StoreData,
) -> Result<()> {
    for receipt in repository.pending_native_backup_preferences()? {
        if !store.backup_preferences_applied(&receipt.operation_id, &receipt.input_digest)? {
            let candidate = repository.native_backup_store_candidate(&receipt, data)?;
            store.apply_native_backup_preferences(
                &receipt.operation_id,
                &receipt.input_digest,
                &candidate,
            )?;
        }
        *data = store.load()?;
        let digest = format!("{:x}", Sha256::digest(serde_json::to_vec(data)?));
        repository.record_profile_change(&digest)?;
        repository
            .acknowledge_native_backup_preferences(&receipt.operation_id, &receipt.input_digest)?;
    }
    Ok(())
}

impl Runtime {
    pub(super) fn begin_native_backup_preview(
        &mut self,
        path: PathBuf,
        replace_tracks: bool,
    ) -> Result<()> {
        ensure!(
            path.is_absolute() && path.to_str().is_some(),
            "Choose an absolute native backup file path"
        );
        ensure!(self.tasks.len() < 4, "Too many pending file operations");
        let operation = self.request_id.clone();
        let completed = self.completed_tx.clone();
        let repository = self.repository.clone();
        let authority = self.library.authority.clone();
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_cancel = cancel.clone();
        let worker_operation = operation.clone();
        let worker = thread::Builder::new()
            .name("lx-native-backup-preview".into())
            .spawn(move || {
                let result = (|| -> Result<NativeBackupPreview> {
                    let check = || {
                        ensure!(
                            !worker_cancel.load(Ordering::Acquire),
                            "Native backup preview cancelled"
                        );
                        Ok(())
                    };
                    check()?;
                    let selected = authority.open_selection(&path)?;
                    let mut file = selected
                        .authorization()
                        .with_cancel(worker_cancel.clone())?
                        .open()?;
                    let length = file.metadata()?.len();
                    ensure!(
                        length <= MAX_BACKUP_BYTES as u64,
                        "Native backup exceeds its input budget"
                    );
                    let mut bytes = Vec::with_capacity(length as usize);
                    let mut buffer = [0u8; 64 * 1024];
                    loop {
                        check()?;
                        let count = file.read(&mut buffer)?;
                        if count == 0 {
                            break;
                        }
                        ensure!(
                            count <= MAX_BACKUP_BYTES.saturating_sub(bytes.len()),
                            "Native backup exceeds its input budget"
                        );
                        bytes.extend_from_slice(&buffer[..count]);
                    }
                    file.check()?;
                    check()?;
                    let backup = decode_native(&bytes, BackupEncoding::Auto)?;
                    let policy = if replace_tracks {
                        ConflictPolicy::ReplaceTracks
                    } else {
                        ConflictPolicy::KeepExisting
                    };
                    let preview = repository.preview_native_backup(&backup, policy)?;
                    check()?;
                    Ok(preview)
                })();
                let _ = completed.send(Completion::NativeBackupPreviewPrepared {
                    operation: worker_operation,
                    result: Box::new(result),
                });
            })?;
        self.file_cancels.insert(operation, cancel);
        self.tasks.push(worker);
        self.async_started = true;
        Ok(())
    }

    pub(super) fn finish_native_backup_preview(
        &mut self,
        operation: String,
        result: Result<NativeBackupPreview>,
    ) {
        let cancel = self.file_cancels.remove(&operation);
        let result = result.and_then(|preview| {
            ensure!(
                !cancel
                    .as_ref()
                    .is_some_and(|cancel| cancel.load(Ordering::Acquire)),
                "Native backup preview cancelled"
            );
            self.native_restore.preview = Some((operation.clone(), Arc::new(preview)));
            Ok(())
        });
        self.publish_finished(operation, &result);
    }

    pub(super) fn native_backup_preview_view(&self) -> Option<NativeBackupPreviewView> {
        self.native_restore
            .preview
            .as_ref()
            .map(|(operation, preview)| {
                let plan = &preview.plan;
                let mut requires_activation = Vec::new();
                if !plan.unapproved_sources.is_empty() {
                    requires_activation.push("source_scripts".into());
                }
                if !plan.declared_local_paths.is_empty()
                    || !plan.list_plan.local_references.is_empty()
                {
                    requires_activation.push("local_files".into());
                }
                if plan
                    .download_plans
                    .as_ref()
                    .is_some_and(|plans| !plans.is_empty())
                {
                    requires_activation.push("download_destinations".into());
                    requires_activation.push("download_audio_urls".into());
                }
                NativeBackupPreviewView {
                    operation_id: operation.clone(),
                    expected_revision: preview.expected_user_revision,
                    input_digest: preview.input_digest.clone(),
                    imported_lists: plan.list_plan.list_changes.len(),
                    local_references: plan.list_plan.local_references.len(),
                    declared_local_paths: plan.declared_local_paths.len(),
                    source_archives: plan.unapproved_sources.len(),
                    download_plans: plan.download_plans.as_ref().map_or(0, Vec::len),
                    downloads_included: plan.download_plans.is_some(),
                    edited_lyrics: plan.edited_lyrics.as_ref().map_or(0, Vec::len),
                    edited_lyrics_included: plan.edited_lyrics.is_some(),
                    requires_activation,
                }
            })
    }

    fn validate_native_restore_audio(&self, candidate: &StoreData) -> Result<()> {
        self.audio
            .validate_equalizer_admission(&candidate.equalizer)?;
        self.audio
            .validate_processing_admission(&candidate.playback_processing)?;
        self.audio.validate_spatial_admission(candidate.spatial)?;
        Ok(())
    }

    fn recover_native_backup_preferences_runtime(&mut self) -> Result<()> {
        self.system_control.require_store_mutation()?;
        if self
            .repository
            .pending_native_backup_preferences()?
            .is_empty()
        {
            return Ok(());
        }
        recover_native_backup_preferences(&self.repository, &self.store, &mut self.data)?;
        self.revision = self.repository.revision()?;
        self.dislike = self.repository.dislike_rules()?;
        self.audio.set_volume(self.data.volume);
        self.audio
            .set_output_preferences(self.data.audio_preferences.clone())?;
        self.audio.set_equalizer(self.data.equalizer)?;
        self.audio.set_processing(self.data.playback_processing)?;
        self.audio
            .set_analysis_enabled(self.data.audio_analysis_enabled)?;
        self.audio.set_spatial(self.data.spatial)?;
        Ok(())
    }

    pub(super) fn commit_native_backup_preview(
        &mut self,
        operation: &str,
        input_digest: &str,
    ) -> Result<()> {
        self.system_control.require_store_mutation()?;
        self.invalidate_system_artwork()?;
        self.recover_native_backup_preferences_runtime()?;
        let (preview_operation, preview) = self
            .native_restore
            .preview
            .as_ref()
            .context("Prepare a native backup preview first")?;
        ensure!(
            preview_operation == operation,
            "Native backup preview operation changed"
        );
        ensure!(
            preview.input_digest == input_digest,
            "Native backup preview digest changed"
        );
        let candidate = self
            .repository
            .native_backup_preview_store_candidate(preview, &self.data)?;
        self.validate_native_restore_audio(&candidate)?;
        self.store.validate_native_backup_preferences_candidate(
            &self.request_id,
            &preview.input_digest,
            &candidate,
        )?;
        let edited_current = self.audio.snapshot().track_id.filter(|id| {
            preview
                .plan
                .edited_lyrics
                .as_ref()
                .is_some_and(|lyrics| lyrics.iter().any(|lyric| &lyric.track_id == id))
        });
        let committed = self
            .repository
            .commit_native_backup(&self.request_id, preview, &|| Ok(()))?;
        self.revision = committed.change.revision;
        if !committed.replayed
            && let Some(id) = edited_current
        {
            self.lyrics_modified(&id);
        }
        self.reconcile_queue()?;
        self.queue.invalidate_plan();
        self.invalidate_preload(Invalidation::List);
        self.recover_native_backup_preferences_runtime()?;
        self.events.lock().unwrap().publish(
            self.revision,
            &self.origin,
            Some(self.request_id.clone()),
            BackendEvent::ListsChanged {
                revision: self.revision,
                action: "native_backup_import".into(),
            },
        );
        Ok(())
    }

    pub(super) fn native_backup_result(
        &self,
        operation: &str,
    ) -> Result<Option<NativeBackupImportView>> {
        Ok(self
            .repository
            .native_backup_result(operation)?
            .map(native_import_view))
    }
}

fn native_import_view(result: NativeBackupCommit) -> NativeBackupImportView {
    NativeBackupImportView {
        operation_id: result.operation_id,
        input_digest: result.input_digest,
        revision: result.change.revision,
        replayed: result.replayed,
        counts: result.counts,
        preferences_applied: result.preferences_applied,
    }
}

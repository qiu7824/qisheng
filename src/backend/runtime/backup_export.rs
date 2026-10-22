use super::*;
use crate::backup::file_export::{
    ExportContext, ExportDirectory, ExportFailure, ExportLimiter, ExportPart, ExportPartMetadata,
    ExportReceipt, MAX_EXPORT_BYTES, export_with,
};
use crate::repository::backup_export::LogicalBackupKind;
use serde::{Deserialize, Serialize};
use std::io::Write;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum BackupExportFormat {
    LxAllData,
    LxSettings,
    LxPlayList,
    LxPlayListPart { list_id: String },
    Native,
    Csv { list_id: String },
}
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackupExportEncoding {
    #[default]
    Json,
    Gzip,
}
impl BackupExportEncoding {
    fn codec(self) -> crate::backup::BackupEncoding {
        match self {
            Self::Json => crate::backup::BackupEncoding::Json,
            Self::Gzip => crate::backup::BackupEncoding::Gzip,
        }
    }
}
#[derive(Clone, Debug, Serialize)]
pub struct BackupExportView {
    pub operation_id: String,
    pub state: String,
    pub snapshot_revision: u64,
    pub path: PathBuf,
    pub receipt: Option<ExportReceipt>,
    pub owned_part: Option<ExportPartMetadata>,
    pub error: Option<String>,
}
struct ExportEntry {
    view: BackupExportView,
    context: Option<ExportContext>,
    directory: Option<ExportDirectory>,
    part: Option<Box<ExportPart>>,
    running_operation: String,
    completion: Arc<Mutex<Option<ExportSettled>>>,
}
struct ExportSettled {
    operation: String,
    export_operation: String,
    directory: Option<ExportDirectory>,
    outcome: ExportOutcome,
}
#[derive(Default)]
pub(super) struct BackupExportRuntime {
    entries: HashMap<String, ExportEntry>,
    limiter: ExportLimiter,
    epoch: u64,
    closing: bool,
}
pub(super) enum ExportOutcome {
    Published(std::result::Result<ExportReceipt, ExportFailure>),
    Cleaned(std::result::Result<ExportPartMetadata, ExportFailure>),
    PreparationFailed(anyhow::Error),
    RecoverySelectionFailed {
        error: anyhow::Error,
        part: Box<ExportPart>,
    },
}
impl BackupExportRuntime {
    pub(super) fn cancel(&mut self, operation: &str) {
        for entry in self.entries.values() {
            if entry.running_operation == operation
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
    pub(super) fn retained_count(&self) -> usize {
        self.entries
            .values()
            .filter(|entry| entry.part.is_some())
            .count()
    }
    fn admit(&mut self) -> Result<()> {
        ensure!(!self.closing, "Backup exports are closing");
        ensure!(
            self.entries
                .values()
                .filter(|entry| entry.context.is_some() || entry.part.is_some())
                .count()
                < 4,
            "Four backup exports already own resources"
        );
        if self.entries.len() >= 64 {
            let removable = self
                .entries
                .iter()
                .find(|(_, entry)| entry.context.is_none() && entry.part.is_none())
                .map(|(id, _)| id.clone())
                .context("Backup export history is full")?;
            self.entries.remove(&removable);
        }
        Ok(())
    }
}
impl Runtime {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn begin_backup_export(
        &mut self,
        directory: PathBuf,
        basename: String,
        format: BackupExportFormat,
        encoding: BackupExportEncoding,
        include_sources: bool,
        include_downloads: bool,
        include_edited_lyrics: bool,
    ) -> Result<()> {
        self.backup_exports.admit()?;
        ensure!(self.tasks.len() < 4, "Too many pending file operations");
        ensure!(
            matches!(format, BackupExportFormat::Native)
                || !(include_sources || include_downloads || include_edited_lyrics),
            "Archive options require the native backup format"
        );
        ensure!(
            !matches!(format, BackupExportFormat::Csv { .. })
                || matches!(encoding, BackupExportEncoding::Json),
            "CSV exports require plain encoding"
        );
        self.backup_exports.epoch = self
            .backup_exports
            .epoch
            .checked_add(1)
            .context("Export selection epoch overflow")?;
        let epoch = self.backup_exports.epoch;
        let operation = self.request_id.clone();
        let context = ExportContext::new(Duration::from_secs(120))?;
        let worker_context = context.clone();
        let completed = self.completed_tx.clone();
        let completion = Arc::new(Mutex::new(None));
        let worker_completion = completion.clone();
        let repository = self.repository.clone();
        let expected_revision = self.revision;
        let store = self.data.clone();
        let limiter = self.backup_exports.limiter.clone();
        let target = directory.join(&basename);
        let worker_operation = operation.clone();
        let kind = match format {
            BackupExportFormat::LxAllData => {
                LogicalBackupKind::Lx(crate::backup::LxBackupKind::AllData, None)
            }
            BackupExportFormat::LxSettings => {
                LogicalBackupKind::Lx(crate::backup::LxBackupKind::Settings, None)
            }
            BackupExportFormat::LxPlayList => {
                LogicalBackupKind::Lx(crate::backup::LxBackupKind::PlayList, None)
            }
            BackupExportFormat::LxPlayListPart { list_id } => {
                LogicalBackupKind::Lx(crate::backup::LxBackupKind::PlayListPart, Some(list_id))
            }
            BackupExportFormat::Native => {
                LogicalBackupKind::Native(crate::backup::NativeBackupOptions {
                    include_sources,
                    include_downloads,
                    include_edited_lyrics,
                })
            }
            BackupExportFormat::Csv { list_id } => LogicalBackupKind::Csv(list_id),
        };
        let task = thread::Builder::new()
            .name("lx-backup-export".into())
            .spawn(move || {
                let mut selected = None;
                let result = (|| -> Result<crate::backup::file_export::ExportPlan> {
                    worker_context.check()?;
                    let grant = ExportDirectory::open_selected(directory, epoch)?;
                    let plan = grant.plan(&basename)?;
                    selected = Some(grant);
                    Ok(plan)
                })();
                let outcome = match result {
                    Ok(plan) => ExportOutcome::Published(export_with(
                        plan,
                        &worker_operation,
                        MAX_EXPORT_BYTES,
                        &worker_context,
                        &limiter,
                        |writer| {
                            let bytes = repository.encode_backup_snapshot(
                                expected_revision,
                                &store,
                                &kind,
                                encoding.codec(),
                                &|| worker_context.check(),
                            )?;
                            writer.write_all(&bytes)?;
                            Ok(())
                        },
                    )),
                    Err(error) => ExportOutcome::PreparationFailed(error),
                };
                *worker_completion
                    .lock()
                    .unwrap_or_else(|error| error.into_inner()) = Some(ExportSettled {
                    operation: worker_operation.clone(),
                    export_operation: worker_operation,
                    directory: selected,
                    outcome,
                });
                let _ = completed.try_send(Completion::BackupExportWake);
            })?;
        self.tasks.push(task);
        self.backup_exports.entries.insert(
            operation.clone(),
            ExportEntry {
                completion,
                running_operation: operation.clone(),
                view: BackupExportView {
                    operation_id: operation,
                    state: "running".into(),
                    snapshot_revision: expected_revision,
                    path: target,
                    receipt: None,
                    owned_part: None,
                    error: None,
                },
                context: Some(context),
                directory: None,
                part: None,
            },
        );
        self.async_started = true;
        Ok(())
    }
    pub(super) fn retry_backup_export(
        &mut self,
        export_operation: String,
        cleanup_directory: Option<PathBuf>,
    ) -> Result<()> {
        let cleanup = cleanup_directory.is_some();
        ensure!(
            !self.backup_exports.closing && self.tasks.len() < 4,
            "Backup file operations are unavailable"
        );
        let entry = self
            .backup_exports
            .entries
            .get_mut(&export_operation)
            .context("Unknown backup export")?;
        ensure!(entry.context.is_none(), "Backup export is still running");
        let published = entry
            .part
            .as_ref()
            .context("Backup export has no retained file")?
            .metadata()
            .published;
        ensure!(
            published != cleanup,
            if cleanup {
                "A published export cannot be deleted"
            } else {
                "Only a published export supports verification retry"
            }
        );
        let context = ExportContext::new(Duration::from_secs(120))?;
        let worker_context = context.clone();
        let part = entry.part.take().expect("Retained part");
        let slot = Arc::new(Mutex::new(Some(part)));
        let worker_slot = slot.clone();
        let operation = self.request_id.clone();
        let worker_operation = operation.clone();
        let worker_export = export_operation.clone();
        let completed = self.completed_tx.clone();
        let worker_completion = entry.completion.clone();
        let spawned = thread::Builder::new()
            .name("lx-backup-export-recovery".into())
            .spawn(move || {
                let part = *worker_slot
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .take()
                    .expect("Export recovery owner");
                let outcome = if cleanup {
                    match ExportDirectory::open_selected(
                        cleanup_directory.expect("Explicit cleanup directory"),
                        part.metadata().selection_epoch,
                    ) {
                        Ok(fresh) => {
                            ExportOutcome::Cleaned(part.cleanup_in(&fresh, &worker_context))
                        }
                        Err(error) => ExportOutcome::RecoverySelectionFailed {
                            error,
                            part: Box::new(part),
                        },
                    }
                } else {
                    ExportOutcome::Published(part.verify_published(&worker_context))
                };
                *worker_completion
                    .lock()
                    .unwrap_or_else(|error| error.into_inner()) = Some(ExportSettled {
                    operation: worker_operation,
                    export_operation: worker_export,
                    directory: None,
                    outcome,
                });
                let _ = completed.try_send(Completion::BackupExportWake);
            });
        match spawned {
            Ok(task) => {
                entry.running_operation = operation;
                entry.context = Some(context);
                entry.view.state = "running".into();
                entry.view.error = None;
                self.tasks.push(task);
                self.async_started = true;
                Ok(())
            }
            Err(error) => {
                entry.part = slot
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .take();
                Err(error.into())
            }
        }
    }
    pub(super) fn finish_backup_export(
        &mut self,
        operation: String,
        export_operation: String,
        directory: Option<ExportDirectory>,
        outcome: ExportOutcome,
    ) {
        let result = (|| -> Result<()> {
            let entry = self
                .backup_exports
                .entries
                .get_mut(&export_operation)
                .context("Backup export ownership disappeared")?;
            entry.context = None;
            if directory.is_some() {
                entry.directory = directory;
            }
            match outcome {
                ExportOutcome::Published(Ok(receipt)) => {
                    entry.view.state = "published".into();
                    entry.view.receipt = Some(receipt);
                    entry.view.owned_part = None;
                    entry.part = None;
                    entry.directory = None;
                    entry.view.error = None;
                    Ok(())
                }
                ExportOutcome::Cleaned(Ok(_)) => {
                    entry.view.state = "cleaned".into();
                    entry.view.owned_part = None;
                    entry.part = None;
                    entry.directory = None;
                    entry.view.error = None;
                    Ok(())
                }
                ExportOutcome::RecoverySelectionFailed { error, part } => {
                    entry.directory = None;
                    entry.view.owned_part = Some(part.metadata());
                    entry.part = Some(part);
                    entry.view.state = "failed".into();
                    entry.view.error = Some(format!("{error:#}").chars().take(4096).collect());
                    Err(error)
                }
                ExportOutcome::PreparationFailed(error) => {
                    entry.view.state = "failed".into();
                    entry.view.error = Some(format!("{error:#}").chars().take(4096).collect());
                    entry.directory = None;
                    Err(error)
                }
                ExportOutcome::Published(Err(failure)) | ExportOutcome::Cleaned(Err(failure)) => {
                    entry.directory = None;
                    entry.part = failure.part;
                    entry.view.owned_part = entry.part.as_ref().map(|part| part.metadata());
                    entry.view.state = if entry
                        .view
                        .owned_part
                        .as_ref()
                        .is_some_and(|part| part.published)
                    {
                        "published_unconfirmed"
                    } else {
                        "failed"
                    }
                    .into();
                    entry.view.error = Some(failure.message.clone());
                    Err(anyhow::anyhow!(failure.message))
                }
            }
        })();
        self.publish_finished(operation, &result);
    }
    pub(super) fn backup_export_view(&self, operation: &str) -> Option<BackupExportView> {
        self.backup_exports
            .entries
            .get(operation)
            .map(|entry| entry.view.clone())
    }
    pub(super) fn poll_backup_exports(&mut self) {
        let mut settled = Vec::with_capacity(4);
        for entry in self.backup_exports.entries.values() {
            if let Some(result) = entry
                .completion
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .take()
            {
                settled.push(result);
            }
        }
        for result in settled {
            self.finish_backup_export(
                result.operation,
                result.export_operation,
                result.directory,
                result.outcome,
            );
        }
    }
}

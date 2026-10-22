//! Guard-owned, journaled replacement of a closed profile's file group.

use super::*;
use crate::repository::{DATABASE_NAME, ProfileGuard, RepositoryCloseOwner, RepositoryOpenOwner};
use std::{
    fs::File,
    io::{Read, Seek, SeekFrom, Write},
    os::windows::io::{FromRawHandle, IntoRawHandle},
};

const STORE_NAME: &str = "player-state.json";
const MAX_GROUP_JOURNAL: u64 = 1024 * 1024;
#[path = "cold.rs"]
mod cold;
pub use cold::{
    PhysicalProfileFileIdentity, PhysicalProfileRecoveryAdmission, PhysicalProfileRecoveryFailure,
    PhysicalProfileRecoveryFile, PhysicalProfileRecoveryOwner, PhysicalProfileRecoveryPreview,
    PhysicalProfileRecoverySelection, PhysicalProfileRecoverySelectionFailure,
    PhysicalProfileTerminalAdmission, PhysicalProfileTerminalRecovery,
    PhysicalProfileTerminalRecoveryFailure, PhysicalProfileTerminalRecoveryOwner,
    PhysicalProfileTerminalSelection, PhysicalProfileTerminalSelectionFailure,
};
#[cfg(test)]
thread_local! {
    static CRASH_POINT: std::cell::RefCell<Option<String>> = const { std::cell::RefCell::new(None) };
}
#[cfg(test)]
pub(super) fn crash_point(point: &str) {
    if CRASH_POINT.with(|selected| selected.borrow().as_deref() == Some(point)) {
        std::process::exit(77);
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileFact {
    role: String,
    live_name: String,
    private_name: String,
    identity: FileIdentity,
    bytes: u64,
    sha256: String,
    private: bool,
}

#[cfg(test)]
mod tests {
    use super::super::tests::{context, image, request};
    use super::*;
    use std::{fs, os::windows::io::AsRawHandle};

    fn installed() -> (
        PhysicalProfileInstallation,
        tempfile::TempDir,
        PhysicalRestoreLimits,
    ) {
        let folder = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
        let (source, _) = image(folder.path());
        let target = Arc::new(Repository::open(folder.path().join("target")).unwrap());
        target.record_profile_change(&"f".repeat(64)).unwrap();
        let limits = PhysicalRestoreLimits::new(128 * 1024 * 1024).unwrap();
        let context = context();
        let prepared = target
            .prepare_physical_restore(
                request(&target),
                PhysicalRestoreSelection::open_selected(source).unwrap(),
                &context,
                &limits,
                &|_| Ok(()),
            )
            .unwrap();
        let guard = target.close_into_guard().unwrap();
        (
            prepared.install_files(guard, &context).unwrap(),
            folder,
            limits,
        )
    }

    #[test]
    fn actual_close_failure_retains_same_main_descriptor_and_guard_for_reopen_retry() {
        use windows_sys::Win32::Foundation::{
            HANDLE_FLAG_PROTECT_FROM_CLOSE, SetHandleInformation,
        };
        let (owner, _folder, limits) = installed();
        let file = owner
            .replacements
            .iter()
            .find(|file| file.role == "main")
            .unwrap()
            .file
            .as_ref()
            .unwrap();
        let raw = file.as_raw_handle();
        assert_ne!(
            unsafe {
                SetHandleInformation(
                    raw,
                    HANDLE_FLAG_PROTECT_FROM_CLOSE,
                    HANDLE_FLAG_PROTECT_FROM_CLOSE,
                )
            },
            0
        );
        let failure = owner.reopen(&context()).unwrap_err();
        let same = failure
            .owner
            .replacements
            .iter()
            .find(|file| file.role == "main")
            .unwrap()
            .file
            .as_ref()
            .unwrap();
        assert_eq!(same.as_raw_handle(), raw);
        assert!(failure.owner.guard.as_ref().unwrap().verify().is_ok());
        assert_eq!(limits.stats().0, 1);
        assert_ne!(
            unsafe { SetHandleInformation(raw, HANDLE_FLAG_PROTECT_FROM_CLOSE, 0) },
            0
        );
        let opened = (*failure.owner).reopen(&context()).unwrap();
        opened.repository().unwrap().integrity_check().unwrap();
    }

    #[test]
    fn foreign_live_main_is_not_moved_or_overwritten_on_actual_rollback() {
        let (mut owner, folder, limits) = installed();
        let main = folder.path().join("target").join(DATABASE_NAME);
        let retained = folder
            .path()
            .join("target")
            .join("test-retained-candidate.sqlite3");
        let replacement = owner
            .replacements
            .iter_mut()
            .find(|file| file.role == "main")
            .unwrap();
        close_owned(&mut replacement.file).unwrap();
        fs::rename(&main, &retained).unwrap();
        let foreign = b"foreign fixture must survive rollback";
        fs::write(&main, foreign).unwrap();
        let failure = owner.rollback(&context()).unwrap_err();
        assert!(failure.message.contains("foreign"), "{}", failure.message);
        assert_eq!(fs::read(&main).unwrap(), foreign);
        assert!(retained.is_file());
        assert!(failure.owner.originals.iter().all(|file| file.private));
        assert_eq!(limits.stats().0, 1);
        let mut owner = *failure.owner;
        owner.release_obstructions().unwrap();
        fs::remove_file(&main).unwrap();
        fs::rename(&retained, &main).unwrap();
        let original = owner
            .rollback(&context())
            .unwrap()
            .reopen(&context())
            .unwrap();
        original.repository().unwrap().integrity_check().unwrap();
    }

    #[test]
    fn unowned_sidecar_blocks_installed_open_without_reading_or_changing_it() {
        let (owner, folder, _) = installed();
        let sidecar = folder
            .path()
            .join("target")
            .join("native-library.sqlite3-wal");
        let foreign = b"foreign WAL fixture";
        fs::write(&sidecar, foreign).unwrap();
        let failure = owner.reopen(&context()).unwrap_err();
        assert!(
            failure.message.contains("unowned sidecar"),
            "{}",
            failure.message
        );
        assert_eq!(fs::read(&sidecar).unwrap(), foreign);
        assert!(failure.owner.repository.is_none());
        let mut owner = *failure.owner;
        owner.release_obstructions().unwrap();
        fs::remove_file(&sidecar).unwrap();
        let opened = owner.reopen(&context()).unwrap();
        opened.repository().unwrap().integrity_check().unwrap();
    }
    #[test]
    fn committed_profile_retires_actual_backups_and_both_journals_before_quota_release() {
        let (owner, folder, limits) = installed();
        let group_journal = owner.journal_path();
        let staged = owner.private.path().to_owned();
        let terminal = owner
            .reopen(&context())
            .unwrap()
            .commit(&context())
            .unwrap();
        assert!(terminal.installed());
        assert!(!group_journal.exists());
        assert_eq!(fs::read_dir(&staged).unwrap().count(), 0);
        assert_eq!(limits.stats(), (0, 0));
        let repository = terminal.into_repository();
        repository.integrity_check().unwrap();
        assert!(Repository::open(folder.path().join("target")).is_err());
        let guard = repository.close_into_guard().unwrap();
        assert!(guard.verify().is_ok());
        drop(guard);
        Repository::open(folder.path().join("target"))
            .unwrap()
            .integrity_check()
            .unwrap();
    }
    #[test]
    fn rollback_terminal_retires_replacements_and_keeps_original_sqlite_change() {
        let (owner, _folder, limits) = installed();
        let staged = owner.private.path().to_owned();
        let terminal = owner
            .rollback(&context())
            .unwrap()
            .reopen(&context())
            .unwrap()
            .finish_rollback(&context())
            .unwrap();
        assert!(!terminal.installed());
        let repository = terminal.into_repository();
        repository.integrity_check().unwrap();
        assert_eq!(repository.revision().unwrap(), 1);
        assert_eq!(fs::read_dir(staged).unwrap().count(), 0);
        assert_eq!(limits.stats(), (0, 0));
    }
    #[test]
    fn actual_rollback_retries_guard_proof_close_before_moving_any_profile_file() {
        use windows_sys::Win32::Foundation::{
            HANDLE_FLAG_PROTECT_FROM_CLOSE, SetHandleInformation,
        };
        let (owner, folder, limits) = installed();
        let owner = owner.reopen(&context()).unwrap();
        let directory = folder.path().join("target");
        let main = fs::read(directory.join(DATABASE_NAME)).unwrap();
        let continuity = owner
            .repository
            .as_ref()
            .unwrap()
            .profile_guard
            .continuity_token()
            .unwrap();
        let _fault = super::super::super::profile_guard::protect_close_proof_for_profile_for_test(
            &directory,
        )
        .unwrap();
        let failure = owner.rollback(&context()).unwrap_err();
        assert!(failure.owner.repository.is_none() && failure.owner.opening.is_none());
        let guard = failure.owner.guard.as_ref().unwrap();
        let raw = guard.pending_close_handle_for_test().unwrap();
        guard.verify_continuity(&continuity).unwrap();
        assert!(failure.owner.originals.iter().all(|file| file.private));
        assert!(failure.owner.replacements.iter().all(|file| !file.private));
        assert!(
            fs::read(directory.join(DATABASE_NAME)).unwrap() == main,
            "Failed close moved or changed live main"
        );
        let failure = (*failure.owner).rollback(&context()).unwrap_err();
        assert_eq!(
            failure
                .owner
                .guard
                .as_ref()
                .unwrap()
                .pending_close_handle_for_test(),
            Some(raw)
        );
        assert!(failure.owner.originals.iter().all(|file| file.private));
        assert!(failure.owner.replacements.iter().all(|file| !file.private));
        assert!(
            fs::read(directory.join(DATABASE_NAME)).unwrap() == main,
            "Retry advanced rollback before native proof close"
        );
        assert_eq!(limits.stats().0, 1);
        assert!(ProfileGuard::acquire(&directory).is_err());
        assert_ne!(
            unsafe { SetHandleInformation(raw, HANDLE_FLAG_PROTECT_FROM_CLOSE, 0) },
            0
        );
        let owner = (*failure.owner).rollback(&context()).unwrap();
        assert!(
            owner
                .guard
                .as_ref()
                .unwrap()
                .pending_close_handle_for_test()
                .is_none()
        );
        owner
            .guard
            .as_ref()
            .unwrap()
            .verify_continuity(&continuity)
            .unwrap();
        let terminal = owner
            .reopen(&context())
            .unwrap()
            .finish_rollback(&context())
            .unwrap();
        assert!(!terminal.installed());
        terminal.into_repository().integrity_check().unwrap();
        assert_eq!(limits.stats(), (0, 0));
    }
}
struct OwnedGroupFile {
    file: Option<File>,
    retirement: Option<ActualFile>,
    cleanup: Option<DurableCleanupFact>,
    cleanup_acknowledged: bool,
    cold_cleanup: Option<crate::repository::snapshot_native::ColdCleanup>,
    fact: Option<FileFact>,
    role: String,
    live_name: String,
    private_name: String,
    private: bool,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
enum GroupPhase {
    Capturing,
    Captured,
    Staging,
    Installing,
    Installed,
    Reopening,
    Reopened,
    OriginalOpening,
    OriginalReopened,
    RollingBack,
    RolledBack,
    Committing,
    RestoringCleanup,
    Finalizing,
}
#[derive(Clone, Serialize, Deserialize)]
struct AllocationIntent {
    role: String,
    live_name: String,
    private_name: String,
}
#[derive(Clone, Serialize, Deserialize)]
struct GroupRecord {
    version: u32,
    sequence: u32,
    operation_id: String,
    request: PhysicalRestoreRequest,
    image: Option<PhysicalRestoreImageView>,
    manifest: ActualFileMetadata,
    prepared_main: Option<ActualFileMetadata>,
    target: FileIdentity,
    journal: FileIdentity,
    phase: GroupPhase,
    originals: Vec<FileFact>,
    replacements: Vec<FileFact>,
    allocations: Vec<AllocationIntent>,
    cleanup: Vec<DurableCleanupFact>,
    terminal_installed: Option<bool>,
}

pub struct PhysicalProfileInstallation {
    prepared: PreparedPhysicalRestore,
    guard: Option<ProfileGuard>,
    repository: Option<Arc<Repository>>,
    opening: Option<RepositoryOpenOwner>,
    target: SnapshotDirectory,
    private: SnapshotDirectory,
    originals: Vec<OwnedGroupFile>,
    replacements: Vec<OwnedGroupFile>,
    allocations: Vec<AllocationIntent>,
    obstructions: Vec<File>,
    journal: Option<File>,
    journal_name: String,
    journal_bytes: u64,
    journal_dirty: bool,
    sequence: u32,
    phase: GroupPhase,
    journal_retirement: Option<ActualFile>,
    journal_cleanup: Option<DurableCleanupFact>,
    terminal_installed: Option<bool>,
    last_record: Option<GroupRecord>,
}
pub struct PhysicalProfileTerminal {
    repository: Arc<Repository>,
    operation_id: String,
    installed: bool,
}
impl std::fmt::Debug for PhysicalProfileTerminal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PhysicalProfileTerminal")
            .field("operation_id", &self.operation_id)
            .field("installed", &self.installed)
            .finish()
    }
}
impl PhysicalProfileTerminal {
    pub fn into_repository(self) -> Arc<Repository> {
        self.repository
    }
    pub fn operation_id(&self) -> &str {
        &self.operation_id
    }
    pub fn installed(&self) -> bool {
        self.installed
    }
}
pub struct PhysicalProfileInstallFailure {
    pub message: String,
    pub owner: Box<PhysicalProfileInstallation>,
}
impl std::fmt::Debug for PhysicalProfileInstallation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PhysicalProfileInstallation")
            .field("operation_id", &self.prepared.request.operation_id)
            .field("phase", &self.phase)
            .field("has_guard", &self.guard.is_some())
            .field("has_repository", &self.repository.is_some())
            .finish()
    }
}
impl std::fmt::Debug for PhysicalProfileInstallFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PhysicalProfileInstallFailure")
            .field("message", &self.message)
            .field("owner", &self.owner)
            .finish()
    }
}
impl std::fmt::Display for PhysicalProfileInstallFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}
impl std::error::Error for PhysicalProfileInstallFailure {}

fn not_found(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            .is_some_and(|error| error.raw_os_error() == Some(2))
    })
}

pub(super) fn close_owned(file: &mut Option<File>) -> Result<()> {
    if let Some(owned) = file.take() {
        let raw = owned.into_raw_handle();
        if unsafe { windows_sys::Win32::Foundation::CloseHandle(raw) } == 0 {
            let error = std::io::Error::last_os_error();
            *file = Some(unsafe { File::from_raw_handle(raw) });
            return Err(error.into());
        }
    }
    Ok(())
}
fn hash_file(file: &mut File, maximum: u64, context: &SnapshotContext) -> Result<(u64, String)> {
    let bytes = file.metadata()?.len();
    ensure!(
        bytes <= maximum,
        "Profile file exceeds installation reservation"
    );
    file.seek(SeekFrom::Start(0))?;
    let mut remaining = bytes;
    let mut buffer = vec![0u8; IO_CHUNK];
    let mut hash = Sha256::new();
    while remaining > 0 {
        context.check()?;
        let wanted = remaining.min(IO_CHUNK as u64) as usize;
        let count = file.read(&mut buffer[..wanted])?;
        ensure!(count > 0, "Profile file truncated during capture");
        hash.update(&buffer[..count]);
        remaining -= count as u64;
    }
    ensure!(
        file.metadata()?.len() == bytes,
        "Profile file changed during capture"
    );
    Ok((bytes, format!("{:x}", hash.finalize())))
}
impl OwnedGroupFile {
    fn attest(
        &mut self,
        directory: &SnapshotDirectory,
        maximum: u64,
        context: &SnapshotContext,
    ) -> Result<()> {
        let file = self
            .file
            .as_mut()
            .context("Profile group descriptor missing")?;
        let identity = directory.group_file_identity(file)?;
        let (bytes, sha256) = hash_file(file, maximum, context)?;
        directory.verify_group_name(
            if self.private {
                &self.private_name
            } else {
                &self.live_name
            },
            &identity,
        )?;
        self.fact = Some(FileFact {
            role: self.role.clone(),
            live_name: self.live_name.clone(),
            private_name: self.private_name.clone(),
            identity,
            bytes,
            sha256,
            private: self.private,
        });
        Ok(())
    }
    fn move_to(&mut self, directory: &SnapshotDirectory, private: bool) -> Result<()> {
        let fact = self
            .fact
            .as_mut()
            .context("Unattested profile file cannot move")?;
        let name = if private {
            &self.private_name
        } else {
            &self.live_name
        };
        let file = self
            .file
            .as_ref()
            .context("Profile move lacks actual descriptor")?;
        let result = directory.rename_group_file(file, name, &fact.identity);
        // A failure may follow the actual rename. Reconcile its actual name,
        // never infer mutation state from the syscall's error alone.
        if directory.verify_group_name(name, &fact.identity).is_ok() {
            self.private = private;
            fact.private = private;
        }
        #[cfg(test)]
        if result.is_ok() && self.role == "main" {
            if private && self.private_name.ends_with(".old") {
                crash_point("original-main-moved");
            }
            if !private && self.private_name.ends_with(".new") {
                crash_point("replacement-main-moved");
            }
        }
        result
    }
}
impl PhysicalProfileInstallation {
    fn group_metadata(&self, file: &OwnedGroupFile) -> Result<ActualFileMetadata> {
        let fact = file
            .fact
            .as_ref()
            .context("Group retirement has no original attestation")?;
        ensure!(
            file.private && fact.private,
            "A live profile file cannot be retired as private"
        );
        let nonce = self
            .journal_name
            .strip_prefix(".lx-profile-")
            .and_then(|name| name.strip_suffix(".journal"))
            .context("Group journal nonce is invalid")?;
        Ok(ActualFileMetadata {
            operation_id: self.prepared.request.operation_id.clone(),
            nonce: nonce.to_owned(),
            temporary_name: Some(file.private_name.clone()),
            directory: self.private.path().to_owned(),
            directory_identity: self.private.identity().clone(),
            selection_epoch: self.private.epoch(),
            identity: Some(fact.identity.clone()),
            bytes: fact.bytes,
            published: false,
            final_basename: None,
            cleanup_probe: None,
        })
    }
    fn retire_group_file(
        &mut self,
        original: bool,
        index: usize,
        context: &SnapshotContext,
    ) -> Result<()> {
        let pending = if original {
            &mut self.originals[index]
        } else {
            &mut self.replacements[index]
        };
        if pending.file.is_none()
            && pending.retirement.is_none()
            && let Some(fact) = &pending.cleanup
        {
            if pending.cold_cleanup.is_none() {
                pending.cold_cleanup =
                    Some(crate::repository::snapshot_native::ColdCleanup::prepare(
                        self.private.clone(),
                        fact.clone(),
                    )?);
            }
            let confirmed = pending.cold_cleanup.as_mut().unwrap().confirm(context)?;
            pending.cleanup = Some(confirmed);
            pending.cleanup_acknowledged = false;
            self.checkpoint(context)?;
            let pending = if original {
                &mut self.originals[index]
            } else {
                &mut self.replacements[index]
            };
            pending.cleanup_acknowledged = true;
            return Ok(());
        }
        let file = if original {
            &self.originals[index]
        } else {
            &self.replacements[index]
        };
        if file.cleanup_acknowledged && file.cleanup.as_ref().is_some_and(|fact| fact.removed) {
            return Ok(());
        }
        let metadata = self.group_metadata(file)?;
        let sha256 = file.fact.as_ref().unwrap().sha256.clone();
        let file = if original {
            &mut self.originals[index]
        } else {
            &mut self.replacements[index]
        };
        if file.retirement.is_none() {
            file.retirement = Some(ActualFile::adopt_group_file(
                &mut file.file,
                &self.private,
                metadata,
                &sha256,
                context,
                &self.prepared.permit,
            )?);
        }
        let actual = file.retirement.take().unwrap();
        let retired = {
            let retained = std::cell::RefCell::new(&mut *self);
            let record = |fact: &DurableCleanupFact| -> Result<()> {
                let mut owner = retained
                    .try_borrow_mut()
                    .context("Group cleanup observer is reentrant")?;
                let file = if original {
                    &mut owner.originals[index]
                } else {
                    &mut owner.replacements[index]
                };
                file.cleanup = Some(fact.clone());
                file.cleanup_acknowledged = false;
                owner.checkpoint(context)?;
                let file = if original {
                    &mut owner.originals[index]
                } else {
                    &mut owner.replacements[index]
                };
                file.cleanup_acknowledged = true;
                Ok(())
            };
            actual.cleanup_journaled(context, &record)
        };
        if let Err(mut failure) = retired {
            let file = if original {
                &mut self.originals[index]
            } else {
                &mut self.replacements[index]
            };
            file.retirement = failure.file.take().map(|file| *file);
            anyhow::bail!("{}", failure.message);
        }
        Ok(())
    }
    fn retire_group_journal(&mut self, context: &SnapshotContext) -> Result<()> {
        if self
            .journal_cleanup
            .as_ref()
            .is_some_and(|fact| fact.removed)
        {
            return Ok(());
        }
        let manifest = self
            .prepared
            .manifest
            .as_ref()
            .context("Prepare manifest is absent")?
            .clone();
        if self.journal_retirement.is_none() {
            self.phase = GroupPhase::Finalizing;
            self.checkpoint(context)?;
            manifest.lock().unwrap_or_else(|e| e.into_inner()).append_checkpoint(
                &serde_json::to_vec(&serde_json::json!({"kind":"profile_group_final", "record":self.last_record}))?, context)?;
            // The surviving preparation manifest contains the final full group
            // record before the dedicated group journal's delete disposition.
            let file = self
                .journal
                .as_mut()
                .context("Group journal descriptor is absent")?;
            let (bytes, sha256) = hash_file(file, MAX_GROUP_JOURNAL, context)?;
            let identity = self.private.group_file_identity(file)?;
            let nonce = self
                .journal_name
                .strip_prefix(".lx-profile-")
                .unwrap()
                .strip_suffix(".journal")
                .unwrap()
                .to_owned();
            let metadata = ActualFileMetadata {
                operation_id: self.prepared.request.operation_id.clone(),
                nonce,
                temporary_name: Some(self.journal_name.clone()),
                directory: self.private.path().to_owned(),
                directory_identity: self.private.identity().clone(),
                selection_epoch: self.private.epoch(),
                identity: Some(identity),
                bytes,
                published: false,
                final_basename: None,
                cleanup_probe: None,
            };
            self.journal_retirement = Some(ActualFile::adopt_group_file(
                &mut self.journal,
                &self.private,
                metadata,
                &sha256,
                context,
                &self.prepared.permit,
            )?);
        }
        let actual = self.journal_retirement.take().unwrap();
        let result = {
            let fact_slot = std::cell::RefCell::new(&mut self.journal_cleanup);
            let record = |fact: &DurableCleanupFact| -> Result<()> {
                manifest.lock().unwrap_or_else(|e| e.into_inner()).append_checkpoint(
                    &serde_json::to_vec(&serde_json::json!({"kind":"profile_group_journal_cleanup","fact":fact}))?, context)?;
                **fact_slot.borrow_mut() = Some(fact.clone());
                #[cfg(test)]
                if fact.pending_zero_links() {
                    crash_point(if fact.removed {
                        "terminal-group-journal-removed"
                    } else {
                        "terminal-group-journal-pending"
                    });
                }
                Ok(())
            };
            actual.cleanup_journaled(context, &record)
        };
        if let Err(mut failure) = result {
            self.journal_retirement = failure.file.take().map(|file| *file);
            anyhow::bail!("{}", failure.message);
        }
        Ok(())
    }
    fn finish(
        mut self,
        installed: bool,
        context: &PhysicalRestoreContext,
    ) -> std::result::Result<PhysicalProfileTerminal, PhysicalProfileInstallFailure> {
        let result = (|| -> Result<()> {
            self.check(&context.inner)?;
            ensure!(
                self.repository.is_some() && self.opening.is_none(),
                "Terminal profile requires a successfully reopened actual repository"
            );
            ensure!(
                if installed {
                    matches!(
                        self.phase,
                        GroupPhase::Reopened | GroupPhase::Committing | GroupPhase::Finalizing
                    )
                } else {
                    matches!(
                        self.phase,
                        GroupPhase::OriginalReopened
                            | GroupPhase::RestoringCleanup
                            | GroupPhase::Finalizing
                    )
                },
                "Profile terminal action conflicts with its current disposition"
            );
            ensure!(
                self.terminal_installed
                    .is_none_or(|expected| expected == installed),
                "Profile terminal retry changed its commit/rollback disposition"
            );
            self.terminal_installed = Some(installed);
            if self.phase != GroupPhase::Finalizing {
                self.phase = if installed {
                    GroupPhase::Committing
                } else {
                    GroupPhase::RestoringCleanup
                };
                self.checkpoint(&context.inner)?;
                let count = if installed {
                    self.originals.len()
                } else {
                    self.replacements.len()
                };
                for index in 0..count {
                    self.retire_group_file(installed, index, &context.inner)?;
                }
            }
            self.release_obstructions()?;
            self.retire_group_journal(&context.inner)?;
            self.prepared.cleanup_private(context)?;
            Ok(())
        })();
        match result {
            Ok(()) => Ok(PhysicalProfileTerminal {
                repository: self.repository.take().unwrap(),
                operation_id: self.prepared.request.operation_id.clone(),
                installed,
            }),
            Err(error) => Err(self.fail(error)),
        }
    }
    /// Commits only a verified reopened profile and physically retires backups.
    pub fn commit(
        self,
        context: &PhysicalRestoreContext,
    ) -> std::result::Result<PhysicalProfileTerminal, PhysicalProfileInstallFailure> {
        self.finish(true, context)
    }
    /// Completes a successfully reopened rollback and retires replacement files.
    pub fn finish_rollback(
        self,
        context: &PhysicalRestoreContext,
    ) -> std::result::Result<PhysicalProfileTerminal, PhysicalProfileInstallFailure> {
        self.finish(false, context)
    }
    fn check(&self, context: &SnapshotContext) -> Result<()> {
        context.check()?;
        self.target.verify()?;
        self.private.verify()?;
        let guard = self
            .guard
            .as_ref()
            .or_else(|| {
                self.repository
                    .as_ref()
                    .map(|repository| &repository.profile_guard)
            })
            .context("Profile guard remains in an incomplete SQLite opening")?;
        self.prepared.verify_guard(guard)
    }
    fn checkpoint(&mut self, context: &SnapshotContext) -> Result<()> {
        self.check(context)?;
        let next = self
            .sequence
            .checked_add(1)
            .context("Profile journal sequence overflow")?;
        ensure!(next <= 64, "Profile group journal record bound exceeded");
        let journal_identity = self.private.group_file_identity(
            self.journal
                .as_ref()
                .context("Profile journal file missing")?,
        )?;
        let record = GroupRecord {
            version: 1,
            sequence: next,
            operation_id: self.prepared.request.operation_id.clone(),
            request: self.prepared.request.clone(),
            image: self.prepared.view.clone(),
            manifest: self
                .prepared
                .manifest
                .as_ref()
                .context("Profile prepare manifest is absent")?
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .metadata(),
            prepared_main: self.prepared.main.as_ref().map(ActualFile::metadata),
            target: self.target.identity().clone(),
            journal: journal_identity,
            phase: self.phase,
            originals: self
                .originals
                .iter()
                .filter_map(|file| file.fact.clone())
                .collect(),
            replacements: self
                .replacements
                .iter()
                .filter_map(|file| file.fact.clone())
                .collect(),
            allocations: self
                .allocations
                .iter()
                .map(|intent| AllocationIntent {
                    role: intent.role.clone(),
                    live_name: intent.live_name.clone(),
                    private_name: intent.private_name.clone(),
                })
                .collect(),
            cleanup: self
                .originals
                .iter()
                .chain(&self.replacements)
                .filter_map(|file| file.cleanup.clone())
                .chain(self.journal_cleanup.clone())
                .collect(),
            terminal_installed: self.terminal_installed,
        };
        let body = serde_json::to_vec(&record)?;
        ensure!(
            body.len() <= 64 * 1024,
            "Profile group journal record exceeds 64 KiB"
        );
        let added = body.len() as u64 + 36;
        ensure!(
            self.journal_bytes + added <= MAX_GROUP_JOURNAL,
            "Profile group journal exceeds 1 MiB"
        );
        let file = self
            .journal
            .as_mut()
            .context("Profile group journal descriptor missing")?;
        // Retry only this owner's unacknowledged suffix. An acknowledged
        // prefix is never rewritten, and the next record records the current
        // actual file identities again before another namespace mutation.
        if self.journal_dirty {
            file.set_len(self.journal_bytes)?;
            file.sync_all()?;
        }
        ensure!(
            file.metadata()?.len() == self.journal_bytes,
            "Profile journal length changed outside its owner"
        );
        self.journal_dirty = true;
        file.seek(SeekFrom::Start(self.journal_bytes))?;
        file.write_all(&(body.len() as u32).to_le_bytes())?;
        file.write_all(&Sha256::digest(&body))?;
        file.write_all(&body)?;
        file.sync_all()?;
        self.journal_bytes += added;
        self.sequence = next;
        self.journal_dirty = false;
        self.last_record = Some(record);
        #[cfg(test)]
        match self.phase {
            GroupPhase::Captured => crash_point("captured"),
            GroupPhase::Installed => crash_point("installed"),
            _ => (),
        }
        self.check(context)
    }
    fn fail(self, error: anyhow::Error) -> PhysicalProfileInstallFailure {
        PhysicalProfileInstallFailure {
            message: format!("{error:#}").chars().take(2048).collect(),
            owner: Box::new(self),
        }
    }
    pub fn journal_path(&self) -> PathBuf {
        self.private.path().join(&self.journal_name)
    }
    pub fn repository(&self) -> Option<&Arc<Repository>> {
        self.repository.as_ref()
    }
    /// Releases inspection descriptors without changing an obstructing file.
    pub fn release_obstructions(&mut self) -> Result<()> {
        while let Some(file) = self.obstructions.pop() {
            let mut owner = Some(file);
            if let Err(error) = close_owned(&mut owner) {
                self.obstructions.push(owner.unwrap());
                return Err(error);
            }
        }
        Ok(())
    }
    fn require_empty_sidecars(&mut self) -> Result<()> {
        for name in [
            "native-library.sqlite3-wal",
            "native-library.sqlite3-shm",
            "native-library.sqlite3-journal",
        ] {
            match self.target.open_group_file(name, false) {
                Ok(file) => {
                    self.obstructions.push(file);
                    anyhow::bail!("Profile reopen is blocked by an unowned sidecar: {name}");
                }
                Err(error) if not_found(&error) => (),
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }
    fn close_repository(&mut self) -> Result<()> {
        if let Some(opening) = self.opening.take() {
            match opening {
                RepositoryOpenOwner::Opening(mut opening) => {
                    if let Some(connection) = opening.connection.take()
                        && let Err((connection, error)) = connection.close()
                    {
                        opening.connection = Some(connection);
                        self.opening = Some(RepositoryOpenOwner::Opening(opening));
                        return Err(error.into());
                    }
                    self.guard = Some(opening.guard);
                }
                RepositoryOpenOwner::Repository(repository) => {
                    self.repository = Some(Arc::from(repository))
                }
            }
        }
        if let Some(repository) = self.repository.take() {
            match repository.close_into_guard() {
                Ok(guard) => self.guard = Some(guard),
                Err(failure) => {
                    match failure.owner {
                        RepositoryCloseOwner::Shared(repository) => {
                            self.repository = Some(repository)
                        }
                        RepositoryCloseOwner::Exclusive(repository) => {
                            self.repository = Some(Arc::from(repository))
                        }
                        RepositoryCloseOwner::Guard(guard) => self.guard = Some(*guard),
                    }
                    return Err(failure.error);
                }
            }
        }
        if let Some(guard) = self.guard.as_mut() {
            guard.finish_close()?;
        }
        Ok(())
    }
    /// The first writer must be the recorded installed main. No missing image
    /// is created and no pragma initializes a substituted filename.
    pub fn reopen(
        mut self,
        context: &PhysicalRestoreContext,
    ) -> std::result::Result<Self, PhysicalProfileInstallFailure> {
        let result = (|| -> Result<()> {
            if matches!(
                self.phase,
                GroupPhase::Reopening | GroupPhase::OriginalOpening
            ) {
                self.close_repository()?;
            }
            self.check(&context.inner)?;
            ensure!(
                matches!(
                    self.phase,
                    GroupPhase::Installed
                        | GroupPhase::RolledBack
                        | GroupPhase::Reopening
                        | GroupPhase::OriginalOpening
                ),
                "Profile file group has no verified main to reopen"
            );
            let original = matches!(
                self.phase,
                GroupPhase::RolledBack | GroupPhase::OriginalOpening
            );
            if !original {
                self.require_empty_sidecars()?;
            }
            let files = if original {
                &mut self.originals
            } else {
                &mut self.replacements
            };
            let main = files
                .iter_mut()
                .find(|file| file.role == "main")
                .context("Installed main owner missing")?;
            let identity = main
                .fact
                .as_ref()
                .context("Installed main identity missing")?
                .identity
                .clone();
            self.phase = if original {
                GroupPhase::OriginalOpening
            } else {
                GroupPhase::Reopening
            };
            self.checkpoint(&context.inner)?;
            let files = if original {
                &mut self.originals
            } else {
                &mut self.replacements
            };
            for file in files {
                close_owned(&mut file.file)?;
            }
            let guard = self.guard.take().unwrap();
            match Repository::open_installed_with_guard(guard, identity) {
                Ok(repository) => self.repository = Some(Arc::new(repository)),
                Err(failure) => {
                    self.opening = Some(failure.owner);
                    return Err(failure.error);
                }
            }
            self.repository.as_ref().unwrap().integrity_check()?;
            self.phase = if original {
                GroupPhase::OriginalReopened
            } else {
                GroupPhase::Reopened
            };
            self.checkpoint(&context.inner)
        })();
        match result {
            Ok(()) => Ok(self),
            Err(error) => Err(self.fail(error)),
        }
    }
    /// Rollback uses the retained original file objects and the same guard.
    /// Foreign occupants fail closed; they are never overwritten or deleted.
    pub fn rollback(
        mut self,
        context: &PhysicalRestoreContext,
    ) -> std::result::Result<Self, PhysicalProfileInstallFailure> {
        let result = (|| -> Result<()> {
            let installed_was_open = matches!(
                self.phase,
                GroupPhase::Reopened | GroupPhase::OriginalReopened
            ) || self.opening.is_some()
                || self.repository.is_some();
            self.close_repository()?;
            self.check(&context.inner)?;
            ensure!(
                self.journal.is_some(),
                "Profile installation has no durable group record"
            );
            if installed_was_open {
                self.require_empty_sidecars()?;
            }
            self.phase = GroupPhase::RollingBack;
            self.checkpoint(&context.inner)?;
            for replacement in &mut self.replacements {
                if !replacement.private {
                    if replacement.file.is_none() {
                        replacement.file =
                            Some(self.target.open_group_file(&replacement.live_name, false)?);
                        let actual = self
                            .target
                            .group_file_identity(replacement.file.as_ref().unwrap())?;
                        if actual != replacement.fact.as_ref().unwrap().identity {
                            self.obstructions.push(replacement.file.take().unwrap());
                            anyhow::bail!("Rollback live image is foreign");
                        }
                    }
                    if installed_was_open {
                        // This same actual main may contain trusted SQLite initialization
                        // writes. Attest it after actual close before private retirement.
                        replacement.attest(
                            &self.target,
                            self.prepared.request.maximum_image_bytes,
                            &context.inner,
                        )?;
                    }
                    replacement.move_to(&self.private, true)?;
                }
            }
            self.checkpoint(&context.inner)?;
            for original in &mut self.originals {
                if original.private {
                    original.move_to(&self.target, false)?;
                }
            }
            self.phase = GroupPhase::RolledBack;
            self.checkpoint(&context.inner)?;
            Ok(())
        })();
        match result {
            Ok(()) => Ok(self),
            Err(error) => Err(self.fail(error)),
        }
    }
}
impl PreparedPhysicalRestore {
    /// Installs a closed profile group while retaining originals for rollback.
    /// The current JSON bytes are copied, preserving source execution consent.
    pub fn install_files(
        self,
        guard: ProfileGuard,
        context: &PhysicalRestoreContext,
    ) -> std::result::Result<PhysicalProfileInstallation, PhysicalProfileInstallFailure> {
        let mut nonce = [0u8; 16];
        let random = getrandom::fill(&mut nonce);
        let nonce = nonce
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let target = self.target.clone();
        let private = self.private.clone();
        let mut owner = PhysicalProfileInstallation {
            prepared: self,
            guard: Some(guard),
            repository: None,
            opening: None,
            target,
            private,
            originals: Vec::new(),
            replacements: Vec::new(),
            allocations: Vec::new(),
            obstructions: Vec::new(),
            journal: None,
            journal_name: format!(".lx-profile-{nonce}.journal"),
            journal_bytes: 0,
            journal_dirty: false,
            sequence: 0,
            phase: GroupPhase::Capturing,
            journal_retirement: None,
            journal_cleanup: None,
            terminal_installed: None,
            last_record: None,
        };
        let installed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> Result<()> {
            random.map_err(|_| anyhow::anyhow!("Cannot allocate profile group nonce"))?;
            owner.guard.as_mut().unwrap().finish_close()?;
            owner.check(&context.inner)?;
            ensure!(owner.guard.as_ref().unwrap().closed_revision() == Some(owner.prepared.request.expected_revision), "Target revision changed before its actual SQLite close");
            ensure!(
                owner.prepared.view.is_some() && owner.prepared.main.is_some(),
                "Physical restore candidate is unsealed"
            );
            let expected = owner.prepared.authority.as_ref().context("Physical restore has no current-profile authority capture")?.digest();
            ensure!(owner.guard.as_ref().unwrap().closed_authority_digest() == Some(expected), "Current execution authority changed before its actual SQLite close");
            ensure!(owner.prepared.view.as_ref().unwrap().authority.as_ref().is_some_and(|report| report.source_sha256 == expected), "Sealed physical image differs from its actual current authority capture");
            // Retire unused private VACUUM objects before allocating another
            // image. Original profile bytes and replacement bytes share the
            // same retained operation reservation, never a second allowance.
            while let Some(file) = owner.prepared.auxiliary.pop() {
                let manifest = owner.prepared.manifest.as_ref().unwrap().clone();
                let record = |fact: &DurableCleanupFact| -> Result<()> {
                    manifest
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .append_checkpoint(
                            &serde_json::to_vec(
                                &serde_json::json!({"kind":"cleanup","fact":fact}),
                            )?,
                            &context.inner,
                        )
                };
                if let Err(mut failure) = file.cleanup_journaled(&context.inner, &record) {
                    if let Some(file) = failure.file.take() {
                        owner.prepared.auxiliary.push(*file);
                    }
                    return Err(anyhow::anyhow!(failure.message));
                }
            }
            owner.prepared.manifest.as_ref().unwrap().lock().unwrap_or_else(|e| e.into_inner()).append_checkpoint(
                &serde_json::to_vec(&serde_json::json!({"kind":"profile_group_allocation_intent", "name":owner.journal_name, "target":owner.target.identity()}))?,
                &context.inner,
            )?;
            owner.journal = Some(owner.private.open_group_file(&owner.journal_name, true)?);
            let journal_identity = owner.private.group_file_identity(owner.journal.as_ref().unwrap())?;
            owner.prepared.manifest.as_ref().unwrap().lock().unwrap_or_else(|e| e.into_inner()).append_checkpoint(
                &serde_json::to_vec(&serde_json::json!({"kind":"profile_group_allocated", "name":owner.journal_name, "identity":journal_identity}))?,
                &context.inner,
            )?;
            owner.checkpoint(&context.inner)?;
            for (role, name, required) in [
                ("main", DATABASE_NAME, true),
                ("wal", "native-library.sqlite3-wal", false),
                ("shm", "native-library.sqlite3-shm", false),
                ("rollback-journal", "native-library.sqlite3-journal", false),
                ("store", STORE_NAME, false),
            ] {
                if matches!(role, "wal" | "shm" | "rollback-journal") {
                    owner.target.require_group_absent(name)
                        .context("Closed profile acquired an unowned sidecar before installation")?;
                    continue;
                }
                let file = match owner.target.open_group_file(name, false) {
                    Ok(file) => file,
                    Err(error) if !required && not_found(&error) =>
                    {
                        continue;
                    }
                    Err(error) => return Err(error),
                };
                owner.originals.push(OwnedGroupFile {
                    file: Some(file),
                    retirement: None,
                    cleanup: None,
                    cleanup_acknowledged: false,
                    cold_cleanup: None,
                    fact: None,
                    role: role.into(),
                    live_name: name.into(),
                    private_name: format!(".lx-profile-{nonce}-{role}.old"),
                    private: false,
                });
                if role == "main" {
                    let captured = owner.target.group_file_identity(owner.originals.last().unwrap().file.as_ref().unwrap())?;
                    ensure!(owner.guard.as_ref().unwrap().closed_main_identity() == Some(&captured), "Target main identity changed after its actual SQLite close");
                    owner.guard.as_ref().unwrap().verify_closed_main(owner.originals.last().unwrap().file.as_ref().unwrap(), &||context.inner.check())?;
                    owner.guard.as_ref().unwrap().verify_closed_sidecars_absent()?;
                }
                let maximum = if role == "main" {
                    owner.prepared.request.maximum_image_bytes
                } else {
                    64 * 1024 * 1024
                };
                owner.originals.last_mut().unwrap().attest(
                    &owner.target,
                    maximum,
                    &context.inner,
                )?;
                owner.checkpoint(&context.inner)?;
            }
            let old_bytes = owner
                .originals
                .iter()
                .map(|file| file.fact.as_ref().unwrap().bytes)
                .try_fold(0u64, |sum, n| {
                    sum.checked_add(n)
                        .context("Original profile group size overflow")
                })?;
            ensure!(
                old_bytes <= owner.prepared.request.maximum_image_bytes + 64 * 1024 * 1024,
                "Original group exceeds its reserved image and sidecar allowance"
            );
            let store_bytes = owner
                .originals
                .iter()
                .find(|file| file.role == "store")
                .map(|file| file.fact.as_ref().unwrap().bytes)
                .unwrap_or(0);
            let image_bytes = owner.prepared.view.as_ref().unwrap().image_bytes;
            let manifest_bytes = owner
                .prepared
                .manifest
                .as_ref()
                .unwrap()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .metadata()
                .bytes;
            let required = old_bytes
                .checked_add(store_bytes)
                .and_then(|n| n.checked_add(image_bytes.checked_mul(2)?))
                .and_then(|n| n.checked_add(manifest_bytes))
                .and_then(|n| n.checked_add(MAX_GROUP_JOURNAL))
                .context("Profile installation workspace overflow")?;
            let reserved = owner
                .prepared
                .request
                .maximum_image_bytes
                .checked_mul(3)
                .and_then(|n| n.checked_add(65 * 1024 * 1024))
                .context("Profile reservation overflow")?;
            ensure!(
                required <= reserved,
                "Profile group has no remaining admitted workspace"
            );
            for (role, live_name) in [("main", DATABASE_NAME), ("store", STORE_NAME)] {
                if role == "store" && !owner.originals.iter().any(|file| file.role == "store") {
                    continue;
                }
                let private_name = format!(".lx-profile-{nonce}-{role}.new");
                owner.allocations.push(AllocationIntent { role: role.into(), live_name: live_name.into(), private_name: private_name.clone() });
                owner.checkpoint(&context.inner)?;
                let file = owner.private.open_group_file(&private_name, true)?;
                owner.replacements.push(OwnedGroupFile {
                    file: Some(file),
                    retirement: None,
                    cleanup: None,
                    cleanup_acknowledged: false,
                    cold_cleanup: None,
                    fact: None,
                    role: role.into(),
                    live_name: live_name.into(),
                    private_name,
                    private: true,
                });
                owner.replacements.last_mut().unwrap().attest(&owner.private, 0, &context.inner)?;
                owner.checkpoint(&context.inner)?;
                let output = owner
                    .replacements
                    .last_mut()
                    .unwrap()
                    .file
                    .as_mut()
                    .unwrap();
                let mut buffer = vec![0u8; IO_CHUNK];
                let mut at = 0u64;
                if role == "main" {
                    let main = owner.prepared.main.as_ref().unwrap();
                    let length = main.len(&context.inner)?;
                    while at < length {
                        context.inner.check()?;
                        let end = (length - at).min(IO_CHUNK as u64) as usize;
                        let n = main.read_at(at, &mut buffer[..end], &context.inner)?;
                        ensure!(n > 0, "Sealed restore image truncated");
                        output.write_all(&buffer[..n])?;
                        at += n as u64;
                    }
                } else {
                    let source = owner
                        .originals
                        .iter_mut()
                        .find(|file| file.role == "store")
                        .unwrap();
                    let input = source.file.as_mut().unwrap();
                    input.seek(SeekFrom::Start(0))?;
                    let length = source.fact.as_ref().unwrap().bytes;
                    while at < length {
                        context.inner.check()?;
                        let end = (length - at).min(IO_CHUNK as u64) as usize;
                        let n = input.read(&mut buffer[..end])?;
                        ensure!(n > 0, "Original configuration truncated");
                        output.write_all(&buffer[..n])?;
                        at += n as u64;
                    }
                }
                output.sync_all()?;
                owner.replacements.last_mut().unwrap().attest(
                    &owner.private,
                    if role == "main" {
                        owner.prepared.request.maximum_image_bytes
                    } else {
                        64 * 1024 * 1024
                    },
                    &context.inner,
                )?;
                let expected = if role == "main" {
                    &owner.prepared.view.as_ref().unwrap().image_sha256
                } else {
                    &owner
                        .originals
                        .iter()
                        .find(|file| file.role == "store")
                        .unwrap()
                        .fact
                        .as_ref()
                        .unwrap()
                        .sha256
                };
                ensure!(
                    &owner
                        .replacements
                        .last()
                        .unwrap()
                        .fact
                        .as_ref()
                        .unwrap()
                        .sha256
                        == expected,
                    "Replacement bytes differ from sealed source"
                );
                owner.checkpoint(&context.inner)?;
            }
            owner.phase = GroupPhase::Captured;
            owner.checkpoint(&context.inner)?;
            owner.phase = GroupPhase::Staging;
            owner.checkpoint(&context.inner)?;
            for index in 0..owner.originals.len() {
                owner.originals[index].move_to(&owner.private, true)?;
                owner.checkpoint(&context.inner)?;
            }
            owner.phase = GroupPhase::Installing;
            owner.checkpoint(&context.inner)?;
            for index in 0..owner.replacements.len() {
                owner.replacements[index].move_to(&owner.target, false)?;
                owner.checkpoint(&context.inner)?;
            }
            owner.phase = GroupPhase::Installed;
            owner.checkpoint(&context.inner)?;
            Ok(())
        }))
        .unwrap_or_else(|_| {
            Err(anyhow::anyhow!(
                "Profile group install panicked; native owners retained"
            ))
        });
        match installed {
            Ok(()) => Ok(owner),
            Err(error) => Err(owner.fail(error)),
        }
    }
}

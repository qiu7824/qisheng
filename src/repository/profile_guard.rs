//! Continuous native profile ownership across SQLite close and reopen.

use super::{
    DATABASE_NAME, Repository, SCHEMA_VERSION, schema,
    snapshot_native::{FileIdentity, SnapshotDirectory, SourceCloseFingerprint, SourceFileProof},
    sync, unique_backup_path,
};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OpenFlags};
use std::{
    fmt,
    fs::{self, File, OpenOptions},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, Weak},
    time::Duration,
};

const LOCK_NAME: &str = "native-library.lock";
#[cfg(all(test, windows))]
thread_local! {
    static PROTECT_NEXT_CLOSE_PROOF: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}
#[cfg(all(test, windows))]
static CLOSE_PROOF_FAULT_PROFILES: Mutex<std::collections::BTreeSet<PathBuf>> =
    Mutex::new(std::collections::BTreeSet::new());
#[cfg(all(test, windows))]
pub(crate) struct CloseProofFaultScope {
    directory: PathBuf,
}
#[cfg(all(test, windows))]
impl Drop for CloseProofFaultScope {
    fn drop(&mut self) {
        CLOSE_PROOF_FAULT_PROFILES
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .remove(&self.directory);
    }
}
#[cfg(all(test, windows))]
pub(crate) fn protect_close_proof_for_profile_for_test(
    directory: &Path,
) -> Result<CloseProofFaultScope> {
    let directory = fs::canonicalize(directory)?;
    ensure!(
        CLOSE_PROOF_FAULT_PROFILES
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .insert(directory.clone()),
        "A close proof fault already selects this actual profile"
    );
    Ok(CloseProofFaultScope { directory })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProfileGuardFailureKind {
    Acquire,
    NotLocked,
    Verify,
}

#[derive(Debug)]
pub struct ProfileGuardError {
    pub kind: ProfileGuardFailureKind,
    pub error: anyhow::Error,
}
impl fmt::Display for ProfileGuardError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "Profile guard {:?}: {:#}", self.kind, self.error)
    }
}
impl std::error::Error for ProfileGuardError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.error.as_ref())
    }
}

/// A failure after opening a native descriptor retains that actual owner.
/// `owner.locked == false` means no exclusive writer lock was acquired.
pub struct ProfileGuardFailure {
    pub error: ProfileGuardError,
    pub owner: Option<Box<ProfileGuard>>,
}
impl fmt::Debug for ProfileGuardFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProfileGuardFailure")
            .field("error", &self.error)
            .field("owner", &self.owner)
            .finish()
    }
}
impl fmt::Display for ProfileGuardFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.error.fmt(formatter)
    }
}
impl std::error::Error for ProfileGuardFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.error)
    }
}

/// This type has no Clone or serializable representation. The lock file, its
/// leaf proof and the continuous directory pins live until the owner is dropped.
pub struct ProfileGuard {
    continuity: Arc<()>,
    closed_revision: Option<u64>,
    closed_main_identity: Option<FileIdentity>,
    closed_authority_digest: Option<String>,
    closed_main_fingerprint: Option<SourceCloseFingerprint>,
    closing_main_proof: Option<SourceFileProof>,
    closing_main_before: Option<SourceCloseFingerprint>,
    closing_sql_transaction: bool,
    root: SnapshotDirectory,
    directory: PathBuf,
    lock: File,
    lock_identity: Option<FileIdentity>,
    lock_proof: Option<SourceFileProof>,
    locked: bool,
}
impl fmt::Debug for ProfileGuard {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProfileGuard")
            .field("directory", &self.directory)
            .field("locked", &self.locked)
            .field("has_lock_proof", &self.lock_proof.is_some())
            .field("has_pending_main_close", &self.closing_main_proof.is_some())
            .finish_non_exhaustive()
    }
}

impl ProfileGuard {
    pub fn acquire(directory: impl AsRef<Path>) -> std::result::Result<Self, ProfileGuardFailure> {
        let preparation = (|| -> Result<(SnapshotDirectory, PathBuf, File)> {
            fs::create_dir_all(directory.as_ref())
                .context("Cannot create native profile directory")?;
            let directory = fs::canonicalize(directory.as_ref())
                .context("Cannot identify native profile directory")?;
            // Read-only pins do not grant staging/publication authority and do
            // not impose the snapshot cleanup protocol's NTFS requirement.
            let root = SnapshotDirectory::pin_profile(&directory, 1)?;
            root.verify()?;
            let lock =
                open_lock(&directory.join(LOCK_NAME)).context("Cannot open native profile lock")?;
            Ok((root, directory, lock))
        })();
        let (root, directory, lock) = preparation.map_err(|error| ProfileGuardFailure {
            error: ProfileGuardError {
                kind: ProfileGuardFailureKind::Acquire,
                error,
            },
            owner: None,
        })?;
        // Place the descriptor under its returnable owner before inspection or
        // locking. Every later error returns this same file and its directory pins.
        let mut owner = Self {
            continuity: Arc::new(()),
            closed_revision: None,
            closed_main_identity: None,
            closed_authority_digest: None,
            closed_main_fingerprint: None,
            closing_main_proof: None,
            closing_main_before: None,
            closing_sql_transaction: false,
            root,
            directory,
            lock,
            lock_identity: None,
            lock_proof: None,
            locked: false,
        };
        let inspected = (|| -> Result<()> {
            owner.root.verify()?;
            owner.lock_identity = Some(lock_identity(&owner.lock)?);
            match SourceFileProof::pin(owner.directory.join(LOCK_NAME)) {
                Ok(proof) => owner.lock_proof = Some(proof),
                Err(mut failure) => {
                    owner.lock_proof = failure.source.take().map(|proof| *proof);
                    return Err(anyhow::anyhow!(failure.message));
                }
            }
            owner.verify_objects()?;
            owner
                .lock
                .try_lock()
                .context("Native profile already has an active writer")?;
            owner.locked = true;
            owner.verify().map_err(anyhow::Error::new)
        })();
        match inspected {
            Ok(()) => Ok(owner),
            Err(error) => Err(ProfileGuardFailure {
                error: ProfileGuardError {
                    kind: ProfileGuardFailureKind::Acquire,
                    error,
                },
                owner: Some(Box::new(owner)),
            }),
        }
    }

    pub fn directory(&self) -> &Path {
        &self.directory
    }
    pub fn is_locked(&self) -> bool {
        self.locked
    }

    pub(crate) fn continuity_token(&self) -> Result<ProfileContinuityToken> {
        self.verify()?;
        Ok(ProfileContinuityToken(Arc::downgrade(&self.continuity)))
    }
    pub(crate) fn closed_revision(&self) -> Option<u64> {
        self.closed_revision
    }
    pub(crate) fn closed_main_identity(&self) -> Option<&FileIdentity> {
        self.closed_main_identity.as_ref()
    }
    pub(crate) fn closed_authority_digest(&self) -> Option<&str> {
        self.closed_authority_digest.as_deref()
    }
    #[cfg(all(test, windows))]
    pub(crate) fn pending_close_handle_for_test(&self) -> Option<std::os::windows::io::RawHandle> {
        self.closing_main_proof
            .as_ref()
            .map(SourceFileProof::close_handle_for_test)
    }
    pub(crate) fn verify_closed_main(
        &self,
        file: &File,
        check: &dyn Fn() -> Result<()>,
    ) -> Result<()> {
        ensure!(
            self.closing_main_proof.is_none(),
            "Profile main close proof still has an actual native owner"
        );
        self.closed_main_fingerprint
            .as_ref()
            .context("Profile main has no completed actual close fingerprint")?
            .verify_file(file, check)
    }
    #[cfg(windows)]
    pub(crate) fn verify_closed_sidecars_absent(&self) -> Result<()> {
        for suffix in ["-wal", "-shm", "-journal"] {
            self.root
                .require_group_absent(&format!("{DATABASE_NAME}{suffix}"))
                .context("Closed profile acquired a new SQLite sidecar before installation")?;
        }
        Ok(())
    }
    fn retire_close_probe(&mut self) -> Result<()> {
        if let Some(proof) = self.closing_main_proof.take()
            && let Err(mut failure) = proof.close()
        {
            self.closing_main_proof = failure.source.take().map(|proof| *proof);
            anyhow::bail!("{}", failure.message);
        }
        self.closing_main_before = None;
        Ok(())
    }
    /// Finishes a failed native proof close after SQLite really closed. Retry
    /// retains this same guard, actual proof handle and full pre-close body.
    pub fn finish_close(&mut self) -> Result<()> {
        self.verify()?;
        let Some(proof) = self.closing_main_proof.as_ref() else {
            return Ok(());
        };
        ensure!(
            self.closed_revision.is_some() && self.closed_authority_digest.is_some(),
            "SQLite has not acknowledged actual connection close"
        );
        let before = self
            .closing_main_before
            .as_ref()
            .context("Profile main pre-close fingerprint is absent")?;
        let after = proof.fingerprint_close(&|| Ok(()))?;
        ensure!(
            before.same_body(&after),
            "Profile main body changed across actual SQLite close"
        );
        self.closed_main_fingerprint = Some(after);
        self.retire_close_probe()?;
        self.verify()?;
        Ok(())
    }

    pub(crate) fn verify_continuity(&self, token: &ProfileContinuityToken) -> Result<()> {
        self.verify()?;
        ensure!(
            Weak::ptr_eq(&Arc::downgrade(&self.continuity), &token.0),
            "Profile writer ownership was released and reacquired"
        );
        Ok(())
    }

    pub(crate) fn verified_lock_identity(
        &self,
    ) -> std::result::Result<super::ProfileLockIdentity, ProfileGuardError> {
        self.verify_objects().map_err(|error| ProfileGuardError {
            kind: ProfileGuardFailureKind::Verify,
            error,
        })?;
        Ok(self
            .lock_identity
            .as_ref()
            .expect("Verified profile lock identity")
            .clone())
    }

    /// No native object is opened, replaced, migrated or closed by verification.
    pub fn verify(&self) -> std::result::Result<(), ProfileGuardError> {
        if !self.locked {
            return Err(ProfileGuardError {
                kind: ProfileGuardFailureKind::NotLocked,
                error: anyhow::anyhow!("Profile writer lock has not been acquired"),
            });
        }
        self.verify_objects().map_err(|error| ProfileGuardError {
            kind: ProfileGuardFailureKind::Verify,
            error,
        })
    }

    fn verify_objects(&self) -> Result<()> {
        self.root.verify()?;
        let expected = self
            .lock_identity
            .as_ref()
            .context("Profile lock descriptor identity is incomplete")?;
        ensure!(
            lock_identity(&self.lock)? == *expected,
            "Profile lock descriptor changed actual object"
        );
        let proof = self
            .lock_proof
            .as_ref()
            .context("Profile lock leaf proof is incomplete")?;
        proof.verify()?;
        let leaf = proof.metadata()?;
        ensure!(
            leaf.identity == *expected
                && crate::domain::music::normalize_local_path(&leaf.path)
                    == crate::domain::music::normalize_local_path(&self.directory.join(LOCK_NAME)),
            "Profile lock name identifies another native object"
        );
        self.root.verify()?;
        Ok(())
    }
}

/// An observation of one acquisition, never a lock or a transferable writer.
/// A retained observation cannot keep a closed guard's descriptors alive.
pub(crate) struct ProfileContinuityToken(Weak<()>);

/// The connection remains owned here until initialization succeeds; a failed
/// SQLite initialization never returns a pathname in place of the real owner.
pub struct OpeningProfile {
    pub guard: ProfileGuard,
    pub connection: Option<Connection>,
}
impl fmt::Debug for OpeningProfile {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OpeningProfile")
            .field("guard", &self.guard)
            .field("has_connection", &self.connection.is_some())
            .finish()
    }
}

pub enum RepositoryOpenOwner {
    Opening(Box<OpeningProfile>),
    Repository(Box<Repository>),
}
pub struct RepositoryOpenFailure {
    pub error: anyhow::Error,
    pub owner: RepositoryOpenOwner,
}
impl fmt::Debug for RepositoryOpenFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RepositoryOpenFailure")
            .field("error", &self.error)
            .field(
                "owner",
                &match &self.owner {
                    RepositoryOpenOwner::Opening(_) => "opening",
                    RepositoryOpenOwner::Repository(_) => "repository",
                },
            )
            .finish()
    }
}
impl fmt::Display for RepositoryOpenFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "Cannot open guarded repository: {:#}",
            self.error
        )
    }
}
impl std::error::Error for RepositoryOpenFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.error.as_ref())
    }
}

pub enum RepositoryCloseOwner {
    Shared(Arc<Repository>),
    Exclusive(Box<Repository>),
    /// SQLite closed successfully, but the remaining guard failed verification.
    Guard(Box<ProfileGuard>),
}
pub struct RepositoryCloseFailure {
    pub error: anyhow::Error,
    pub owner: RepositoryCloseOwner,
}
impl fmt::Debug for RepositoryCloseFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RepositoryCloseFailure")
            .field("error", &self.error)
            .field(
                "owner",
                &match &self.owner {
                    RepositoryCloseOwner::Shared(_) => "shared",
                    RepositoryCloseOwner::Exclusive(_) => "exclusive",
                    RepositoryCloseOwner::Guard(_) => "guard",
                },
            )
            .finish()
    }
}
impl fmt::Display for RepositoryCloseFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "Cannot close guarded repository: {:#}",
            self.error
        )
    }
}
impl std::error::Error for RepositoryCloseFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.error.as_ref())
    }
}

impl Repository {
    #[cfg(windows)]
    pub(crate) fn ensure_tag_staging(&self) -> Result<SnapshotDirectory> {
        self.profile_guard.verify()?;
        let staging = SnapshotDirectory::open_tag_staging(&self.profile_guard.root)?;
        self.profile_guard.verify()?;
        Ok(staging)
    }
    /// Initialization is shared with ordinary open through its acquired guard.
    /// The caller retains the returned owner on every failure.
    pub fn open_with_guard(
        guard: ProfileGuard,
    ) -> std::result::Result<Self, RepositoryOpenFailure> {
        Self::open_with_guard_checked(guard, None)
    }
    pub(crate) fn open_installed_with_guard(
        guard: ProfileGuard,
        expected_main: FileIdentity,
    ) -> std::result::Result<Self, RepositoryOpenFailure> {
        Self::open_with_guard_checked(guard, Some(expected_main))
    }
    fn open_with_guard_checked(
        guard: ProfileGuard,
        expected_main: Option<FileIdentity>,
    ) -> std::result::Result<Self, RepositoryOpenFailure> {
        let mut opening = OpeningProfile {
            guard,
            connection: None,
        };
        if let Err(error) = initialize_open(&mut opening, expected_main.as_ref()) {
            return Err(RepositoryOpenFailure {
                error,
                owner: RepositoryOpenOwner::Opening(Box::new(opening)),
            });
        }
        let Some(connection) = opening.connection.take() else {
            return Err(RepositoryOpenFailure {
                error: anyhow::anyhow!("Initialized profile has no SQLite owner"),
                owner: RepositoryOpenOwner::Opening(Box::new(opening)),
            });
        };
        let repository = Self {
            path: opening.guard.directory.join(DATABASE_NAME),
            connection: Mutex::new(connection),
            sync_registry: Mutex::new(sync::SyncSessionRegistry::default()),
            tag_finished_validation: Mutex::new(Default::default()),
            profile_guard: opening.guard,
        };
        let recovered = (|| -> Result<()> {
            repository.recover_library_scans()?;
            repository.recover_download_tasks()?;
            repository.profile_guard.verify()?;
            Ok(())
        })();
        match recovered {
            Ok(()) => Ok(repository),
            Err(error) => Err(RepositoryOpenFailure {
                error,
                owner: RepositoryOpenOwner::Repository(Box::new(repository)),
            }),
        }
    }

    /// The actor must join its workers and drop all service owners before calling.
    /// A shared Arc or a busy SQLite connection is returned as the actual owner.
    pub fn close_into_guard(
        self: Arc<Self>,
    ) -> std::result::Result<ProfileGuard, RepositoryCloseFailure> {
        let mut repository = match Arc::try_unwrap(self) {
            Ok(repository) => repository,
            Err(owner) => {
                return Err(RepositoryCloseFailure {
                    error: anyhow::anyhow!("Repository still has shared owners"),
                    owner: RepositoryCloseOwner::Shared(owner),
                });
            }
        };
        let prepare = (|| -> Result<(u64, Option<FileIdentity>, Option<String>)> {
            repository.profile_guard.verify()?;
            repository.profile_guard.retire_close_probe()?;
            ensure!(
                !repository.sync_registry.is_poisoned(),
                "Repository session registry was poisoned"
            );
            let connection = repository
                .connection
                .get_mut()
                .map_err(|_| anyhow::anyhow!("Repository lock was poisoned"))?;
            if repository.profile_guard.closing_sql_transaction {
                if !connection.is_autocommit() {
                    connection.execute_batch("ROLLBACK")?;
                }
                repository.profile_guard.closing_sql_transaction = false;
            }
            schema::validate_current(connection)?;
            ensure!(
                connection.is_autocommit(),
                "Repository WAL checkpoint requires an idle connection"
            );
            connection.pragma_update(None, "locking_mode", "EXCLUSIVE")?;
            repository.profile_guard.closing_sql_transaction = true;
            let locked = connection.execute_batch(
                "BEGIN EXCLUSIVE; UPDATE repository_state SET revision=revision; COMMIT;",
            );
            if let Err(error) = locked {
                if !connection.is_autocommit() {
                    connection.execute_batch("ROLLBACK").context(
                        "Cannot roll back this close owner's interrupted lock transaction",
                    )?;
                }
                repository.profile_guard.closing_sql_transaction = false;
                return Err(error)
                    .context("Repository WAL checkpoint still has active readers or writers");
            }
            repository.profile_guard.closing_sql_transaction = false;
            let (busy, pages, checkpointed): (i64, i64, i64) =
                connection.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| {
                    Ok((row.get(0)?, row.get(1)?, row.get(2)?))
                })?;
            ensure!(
                busy == 0 && pages == checkpointed,
                "Repository WAL checkpoint still has active readers or writers"
            );
            repository.profile_guard.verify()?;
            #[cfg(windows)]
            let cut = super::profile_install::authority::CurrentAuthority::capture_close_cut(
                connection,
                &|| Ok(()),
            )?;
            #[cfg(windows)]
            let (closed_revision, digest) = (cut.revision, Some(cut.digest));
            #[cfg(not(windows))]
            let (closed_revision, digest) = {
                let revision: i64 = connection.query_row(
                    "SELECT revision FROM repository_state WHERE singleton=1",
                    [],
                    |row| row.get(0),
                )?;
                (
                    u64::try_from(revision).context("Closed repository revision is negative")?,
                    None,
                )
            };
            #[cfg(windows)]
            let identity = {
                let proof = unsafe {
                    SourceFileProof::from_sqlite_main(connection.handle(), &repository.path)
                };
                match proof {
                    Ok(proof) => repository.profile_guard.closing_main_proof = Some(proof),
                    Err(mut failure) => {
                        repository.profile_guard.closing_main_proof =
                            failure.source.take().map(|proof| *proof);
                        anyhow::bail!("{}", failure.message);
                    }
                }
                let proof = repository
                    .profile_guard
                    .closing_main_proof
                    .as_ref()
                    .unwrap();
                let before = proof.fingerprint_close(&|| Ok(()))?;
                let identity = before.identity.clone();
                repository.profile_guard.closing_main_before = Some(before);
                Some(identity)
            };
            #[cfg(not(windows))]
            let identity = None;
            Ok((closed_revision, identity, digest))
        })();
        let (closed_revision, closed_main_identity, closed_authority_digest) = match prepare {
            Ok(stamp) => stamp,
            Err(error) => {
                return Err(RepositoryCloseFailure {
                    error,
                    owner: RepositoryCloseOwner::Exclusive(Box::new(repository)),
                });
            }
        };
        let Self {
            connection,
            sync_registry,
            tag_finished_validation,
            path,
            mut profile_guard,
        } = repository;
        let connection = match connection.into_inner() {
            Ok(connection) => connection,
            Err(error) => {
                let repository = Self {
                    connection: Mutex::new(error.into_inner()),
                    sync_registry,
                    tag_finished_validation,
                    path,
                    profile_guard,
                };
                return Err(RepositoryCloseFailure {
                    error: anyhow::anyhow!("Repository lock was poisoned during close"),
                    owner: RepositoryCloseOwner::Exclusive(Box::new(repository)),
                });
            }
        };
        match connection.close() {
            Ok(()) => {
                drop(sync_registry);
                drop(tag_finished_validation);
                profile_guard.closed_revision = Some(closed_revision);
                profile_guard.closed_main_identity = closed_main_identity;
                profile_guard.closed_authority_digest = closed_authority_digest;
                #[cfg(all(test, windows))]
                if PROTECT_NEXT_CLOSE_PROOF.with(|selected| selected.replace(false))
                    || CLOSE_PROOF_FAULT_PROFILES
                        .lock()
                        .unwrap_or_else(|error| error.into_inner())
                        .remove(&profile_guard.directory)
                {
                    use windows_sys::Win32::Foundation::{
                        HANDLE_FLAG_PROTECT_FROM_CLOSE, SetHandleInformation,
                    };
                    let raw = profile_guard
                        .closing_main_proof
                        .as_ref()
                        .unwrap()
                        .close_handle_for_test();
                    if unsafe {
                        SetHandleInformation(
                            raw,
                            HANDLE_FLAG_PROTECT_FROM_CLOSE,
                            HANDLE_FLAG_PROTECT_FROM_CLOSE,
                        )
                    } == 0
                    {
                        return Err(RepositoryCloseFailure {
                            error: std::io::Error::last_os_error().into(),
                            owner: RepositoryCloseOwner::Guard(Box::new(profile_guard)),
                        });
                    }
                }
                match profile_guard.finish_close() {
                    Ok(()) => Ok(profile_guard),
                    Err(error) => Err(RepositoryCloseFailure {
                        error,
                        owner: RepositoryCloseOwner::Guard(Box::new(profile_guard)),
                    }),
                }
            }
            Err((connection, error)) => {
                let repository = Self {
                    connection: Mutex::new(connection),
                    sync_registry,
                    tag_finished_validation,
                    path,
                    profile_guard,
                };
                Err(RepositoryCloseFailure {
                    error: error.into(),
                    owner: RepositoryCloseOwner::Exclusive(Box::new(repository)),
                })
            }
        }
    }
}

/// Ordinary open and handover open use this one initializer. No profile lock is
/// acquired again and no error path drops an already-open SQLite descriptor.
fn initialize_open(
    opening: &mut OpeningProfile,
    expected_main: Option<&FileIdentity>,
) -> Result<()> {
    opening.guard.verify()?;
    opening.guard.finish_close()?;
    opening.guard.closed_revision = None;
    opening.guard.closed_main_identity = None;
    opening.guard.closed_authority_digest = None;
    opening.guard.closed_main_fingerprint = None;
    let path = opening.guard.directory.join(DATABASE_NAME);
    ensure!(!path.is_dir(), "Native database path is a directory");
    opening.connection = Some(
        Connection::open_with_flags(
            &path,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_NO_MUTEX
                | if expected_main.is_none() {
                    OpenFlags::SQLITE_OPEN_CREATE
                } else {
                    OpenFlags::empty()
                },
        )
        .context("Cannot open native repository")?,
    );
    let connection = opening
        .connection
        .as_mut()
        .context("SQLite opening owner is missing")?;
    if let Some(expected) = expected_main {
        // The actual default-VFS accessor is inspected before every pragma,
        // migration, recovery write or user-data transaction. A missing image
        // is not created and a substituted leaf is never initialized.
        let proof = unsafe { SourceFileProof::from_sqlite_main(connection.handle(), &path) }
            .map_err(|failure| anyhow::anyhow!(failure.message))?;
        ensure!(
            &proof.metadata()?.identity == expected,
            "Installed database filename identifies a different actual file"
        );
        proof.verify()?;
    }
    connection.busy_timeout(Duration::from_secs(5))?;
    let old_version = schema::version(connection)?;
    schema::validate_identity(connection, old_version)?;
    schema::integrity_check(connection)?;
    opening.guard.verify()?;
    if old_version > 0 && old_version < SCHEMA_VERSION {
        connection
            .backup("main", unique_backup_path(&path, old_version), None)
            .context("Cannot preserve database before migration")?;
    }
    connection.pragma_update(None, "foreign_keys", true)?;
    schema::migrate(connection, old_version)?;
    connection.pragma_update(None, "journal_mode", "WAL")?;
    connection.pragma_update(None, "synchronous", "FULL")?;
    opening.guard.verify()?;
    Ok(())
}

fn open_lock(path: &Path) -> Result<File> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ, FILE_SHARE_WRITE,
        };
        // Keeping SHARE_DELETE absent prevents replacement of the locked leaf.
        options
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    Ok(options.open(path)?)
}

#[cfg(windows)]
fn lock_identity(file: &File) -> Result<FileIdentity> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT, FILE_ATTRIBUTE_TAG_INFO,
        FILE_ID_INFO, FILE_STANDARD_INFO, FileAttributeTagInfo, FileIdInfo, FileStandardInfo,
        GetFileInformationByHandleEx,
    };
    let mut attributes = FILE_ATTRIBUTE_TAG_INFO::default();
    let mut identity = FILE_ID_INFO::default();
    let mut standard = FILE_STANDARD_INFO::default();
    ensure!(
        unsafe {
            GetFileInformationByHandleEx(
                file.as_raw_handle(),
                FileAttributeTagInfo,
                &mut attributes as *mut _ as *mut _,
                std::mem::size_of_val(&attributes) as u32,
            )
        } != 0,
        "Cannot inspect profile lock descriptor"
    );
    ensure!(
        attributes.FileAttributes & (FILE_ATTRIBUTE_DIRECTORY | FILE_ATTRIBUTE_REPARSE_POINT) == 0,
        "Profile lock is not a regular native file"
    );
    ensure!(
        unsafe {
            GetFileInformationByHandleEx(
                file.as_raw_handle(),
                FileStandardInfo,
                &mut standard as *mut _ as *mut _,
                std::mem::size_of_val(&standard) as u32,
            )
        } != 0
            && standard.NumberOfLinks == 1
            && !standard.DeletePending,
        "Profile lock link identity is invalid or pending deletion"
    );
    ensure!(
        unsafe {
            GetFileInformationByHandleEx(
                file.as_raw_handle(),
                FileIdInfo,
                &mut identity as *mut _ as *mut _,
                std::mem::size_of_val(&identity) as u32,
            )
        } != 0,
        "Cannot identify profile lock descriptor"
    );
    Ok(FileIdentity {
        device: identity.VolumeSerialNumber,
        object: identity
            .FileId
            .Identifier
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect(),
    })
}

#[cfg(target_os = "linux")]
fn lock_identity(file: &File) -> Result<FileIdentity> {
    use std::os::unix::fs::MetadataExt;
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file() && metadata.nlink() == 1,
        "Profile lock is not a unique regular native file"
    );
    Ok(FileIdentity {
        device: metadata.dev(),
        object: metadata.ino().to_string(),
    })
}

#[cfg(not(any(windows, target_os = "linux")))]
fn lock_identity(_: &File) -> Result<FileIdentity> {
    anyhow::bail!("Continuous native profile guards are unsupported on this platform")
}

#[cfg(all(test, any(windows, target_os = "linux")))]
mod tests {
    use super::*;
    #[cfg(windows)]
    #[test]
    fn actual_post_sqlite_close_proof_failure_retains_same_guard_and_native_handle_for_retry() {
        use windows_sys::Win32::Foundation::{
            HANDLE_FLAG_PROTECT_FROM_CLOSE, SetHandleInformation,
        };
        let directory = profile();
        let repo = Repository::open(directory.path()).unwrap();
        repo.record_search("actual-close-proof", None).unwrap();
        let revision = repo.revision().unwrap();
        let continuity = repo.profile_guard.continuity_token().unwrap();
        PROTECT_NEXT_CLOSE_PROOF.with(|selected| selected.set(true));
        let failure = Arc::new(repo).close_into_guard().unwrap_err();
        let RepositoryCloseOwner::Guard(mut guard) = failure.owner else {
            panic!("Successful SQLite close must retain its original guard and proof owner");
        };
        assert_eq!(guard.closed_revision(), Some(revision));
        assert!(guard.closed_authority_digest().is_some());
        let raw = guard
            .closing_main_proof
            .as_ref()
            .unwrap()
            .close_handle_for_test();
        guard.verify_continuity(&continuity).unwrap();
        second_writer_is_refused(directory.path());
        assert!(guard.finish_close().is_err());
        assert_eq!(
            guard
                .closing_main_proof
                .as_ref()
                .unwrap()
                .close_handle_for_test(),
            raw
        );
        assert_ne!(
            unsafe { SetHandleInformation(raw, HANDLE_FLAG_PROTECT_FROM_CLOSE, 0) },
            0
        );
        guard.finish_close().unwrap();
        assert!(guard.closing_main_proof.is_none());
        assert!(guard.closed_main_fingerprint.is_some());
        guard.verify_continuity(&continuity).unwrap();
        let reopened = Repository::open_with_guard(*guard).unwrap();
        assert_eq!(reopened.search_history().unwrap()[0], "actual-close-proof");
    }
    use rusqlite::ffi;

    fn profile() -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix("profile-guard-")
            .tempdir_in(std::env::temp_dir())
            .unwrap()
    }
    fn second_writer_is_refused(directory: &Path) {
        let failure = ProfileGuard::acquire(directory).unwrap_err();
        if let Some(owner) = &failure.owner {
            assert!(
                !owner.is_locked(),
                "Refused writer must not claim an exclusive lock"
            );
            assert_eq!(
                owner.verify().unwrap_err().kind,
                ProfileGuardFailureKind::NotLocked
            );
        }
        assert!(Repository::open(directory).is_err());
    }

    #[test]
    fn actual_os_lock_survives_sqlite_close_and_same_guard_reopen() {
        let directory = profile();
        let guard = ProfileGuard::acquire(directory.path()).unwrap();
        let identity = guard.lock_identity.clone();
        second_writer_is_refused(directory.path());
        let repo = Repository::open_with_guard(guard).unwrap();
        repo.record_search("saved-before-close", None).unwrap();
        second_writer_is_refused(directory.path());
        let guard = Arc::new(repo).close_into_guard().unwrap();
        guard.verify().unwrap();
        assert_eq!(guard.lock_identity, identity);
        second_writer_is_refused(directory.path());
        let repo = Repository::open_with_guard(guard).unwrap();
        assert_eq!(repo.search_history().unwrap(), ["saved-before-close"]);
        assert_eq!(repo.profile_guard.lock_identity, identity);
        second_writer_is_refused(directory.path());
        let guard = Arc::new(repo).close_into_guard().unwrap();
        second_writer_is_refused(directory.path());
        drop(guard);
        let next = ProfileGuard::acquire(directory.path()).unwrap();
        next.verify().unwrap();
    }

    #[test]
    fn shared_arc_close_returns_exact_live_repository_owner() {
        let directory = profile();
        let repo = Arc::new(Repository::open(directory.path()).unwrap());
        let shared = repo.clone();
        let failure = repo.close_into_guard().unwrap_err();
        let RepositoryCloseOwner::Shared(owner) = failure.owner else {
            panic!("Shared ownership must return its actual Arc");
        };
        assert!(Arc::ptr_eq(&owner, &shared));
        owner.integrity_check().unwrap();
        second_writer_is_refused(directory.path());
        drop(shared);
        let guard = owner.close_into_guard().unwrap();
        second_writer_is_refused(directory.path());
        let repo = Repository::open_with_guard(guard).unwrap();
        repo.integrity_check().unwrap();
    }

    #[test]
    fn actual_wal_reader_blocks_checkpoint_and_retains_exclusive_repository() {
        let directory = profile();
        let repo = Repository::open(directory.path()).unwrap();
        repo.record_search("initial-cut", None).unwrap();
        let reader =
            Connection::open_with_flags(repo.path(), OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        reader.execute_batch("BEGIN DEFERRED").unwrap();
        let old_revision: i64 = reader
            .query_row(
                "SELECT revision FROM repository_state WHERE singleton=1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        repo.record_search("committed-after-reader-cut", None)
            .unwrap();
        let current_revision = repo.revision().unwrap();
        assert!(current_revision > old_revision as u64);
        let failure = Arc::new(repo).close_into_guard().unwrap_err();
        assert!(failure.error.to_string().contains("checkpoint"));
        let RepositoryCloseOwner::Exclusive(owner) = failure.owner else {
            panic!("Busy WAL checkpoint must retain the actual repository");
        };
        assert_eq!(owner.revision().unwrap(), current_revision);
        owner.profile_guard.verify().unwrap();
        second_writer_is_refused(directory.path());
        reader.execute_batch("ROLLBACK").unwrap();
        drop(reader);
        let guard = Arc::new(*owner).close_into_guard().unwrap();
        second_writer_is_refused(directory.path());
        let repo = Repository::open_with_guard(guard).unwrap();
        assert_eq!(
            repo.search_history().unwrap()[0],
            "committed-after-reader-cut"
        );
        assert_eq!(repo.revision().unwrap(), current_revision);
    }

    #[test]
    fn genuine_unfinalized_statement_close_error_returns_same_sqlite_connection() {
        let directory = profile();
        let repo = Repository::open(directory.path()).unwrap();
        let mut statement: *mut ffi::sqlite3_stmt = std::ptr::null_mut();
        let database = {
            let connection = repo.connection.lock().unwrap();
            let database = unsafe { connection.handle() };
            assert_eq!(
                unsafe {
                    ffi::sqlite3_prepare_v2(
                        database,
                        c"SELECT 1".as_ptr(),
                        -1,
                        &mut statement,
                        std::ptr::null_mut(),
                    )
                },
                ffi::SQLITE_OK
            );
            assert!(!statement.is_null());
            database
        };
        // The outstanding real sqlite3_stmt makes sqlite3_close return BUSY.
        // Its database remains alive inside the returned exclusive repository.
        let failure = Arc::new(repo).close_into_guard().unwrap_err();
        let RepositoryCloseOwner::Exclusive(owner) = failure.owner else {
            panic!("Outstanding SQLite statement must retain its actual connection");
        };
        let returned_database = unsafe { owner.connection.lock().unwrap().handle() };
        assert_eq!(returned_database, database);
        assert_eq!(unsafe { ffi::sqlite3_db_handle(statement) }, database);
        second_writer_is_refused(directory.path());
        assert_eq!(unsafe { ffi::sqlite3_finalize(statement) }, ffi::SQLITE_OK);
        owner.integrity_check().unwrap();
        let guard = Arc::new(*owner).close_into_guard().unwrap();
        second_writer_is_refused(directory.path());
        Repository::open_with_guard(guard)
            .unwrap()
            .integrity_check()
            .unwrap();
    }

    #[test]
    fn failed_startup_retains_actual_connection_and_same_guard_can_reopen() {
        let directory = profile();
        let guard = ProfileGuard::acquire(directory.path()).unwrap();
        let identity = guard.lock_identity.clone();
        let database = directory.path().join(DATABASE_NAME);
        fs::write(&database, b"damaged synthetic SQLite fixture").unwrap();
        let failure = match Repository::open_with_guard(guard) {
            Err(failure) => failure,
            Ok(_) => panic!("Synthetic damaged SQLite bytes must fail startup"),
        };
        let RepositoryOpenOwner::Opening(mut opening) = failure.owner else {
            panic!("Database validation failure must retain its actual opening owner");
        };
        assert!(opening.connection.is_some());
        assert_eq!(opening.guard.lock_identity, identity);
        opening.guard.verify().unwrap();
        second_writer_is_refused(directory.path());
        let connection = opening.connection.take().unwrap();
        connection.close().unwrap();
        // Replace only this test's synthetic damaged bytes after genuine close.
        fs::write(&database, []).unwrap();
        let repo = Repository::open_with_guard(opening.guard).unwrap();
        assert_eq!(repo.profile_guard.lock_identity, identity);
        second_writer_is_refused(directory.path());
        repo.integrity_check().unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn installed_open_checks_actual_main_identity_before_initializing_foreign_bytes() {
        let directory = profile();
        let repo = Repository::open(directory.path()).unwrap();
        let proof = unsafe {
            SourceFileProof::from_sqlite_main(repo.connection.lock().unwrap().handle(), repo.path())
        }
        .unwrap();
        let expected = proof.metadata().unwrap().identity.clone();
        drop(proof);
        let guard = Arc::new(repo).close_into_guard().unwrap();
        let main = directory.path().join(DATABASE_NAME);
        fs::rename(&main, directory.path().join("retained-original.sqlite3")).unwrap();
        let foreign = b"unrelated bytes must not be initialized";
        fs::write(&main, foreign).unwrap();
        let failure = match Repository::open_installed_with_guard(guard, expected) {
            Err(failure) => failure,
            Ok(_) => panic!("Foreign main must not initialize"),
        };
        assert!(failure.error.to_string().contains("different actual file"));
        assert_eq!(fs::read(&main).unwrap(), foreign);
        let RepositoryOpenOwner::Opening(mut opening) = failure.owner else {
            panic!("Actual opening connection must remain owned")
        };
        assert!(opening.connection.is_some());
        second_writer_is_refused(directory.path());
        opening.connection.take().unwrap().close().unwrap();
        assert!(!directory.path().join("native-library.sqlite3-wal").exists());
        assert!(!directory.path().join("native-library.sqlite3-shm").exists());
        assert_eq!(fs::read(&main).unwrap(), foreign);
    }

    #[cfg(windows)]
    #[test]
    fn installed_open_does_not_create_a_missing_main_and_preserves_guard() {
        let directory = profile();
        let repo = Repository::open(directory.path()).unwrap();
        let proof = unsafe {
            SourceFileProof::from_sqlite_main(repo.connection.lock().unwrap().handle(), repo.path())
        }
        .unwrap();
        let expected = proof.metadata().unwrap().identity.clone();
        drop(proof);
        let guard = Arc::new(repo).close_into_guard().unwrap();
        let main = directory.path().join(DATABASE_NAME);
        fs::rename(&main, directory.path().join("retained-original.sqlite3")).unwrap();
        let failure = match Repository::open_installed_with_guard(guard, expected) {
            Err(failure) => failure,
            Ok(_) => panic!("Missing main must not be created"),
        };
        assert!(!main.exists());
        let RepositoryOpenOwner::Opening(opening) = failure.owner else {
            panic!("Missing main must retain the guard")
        };
        assert!(opening.connection.is_none());
        assert!(opening.guard.closed_revision().is_none());
        second_writer_is_refused(directory.path());
    }

    #[cfg(windows)]
    #[test]
    fn tag_staging_is_created_under_the_actual_profile_and_remains_pinned() {
        let directory = profile();
        let repo = Repository::open(directory.path()).unwrap();
        let path = directory.path().join("tag-edits");
        assert!(!path.exists());
        let staging = repo.ensure_tag_staging().unwrap();
        assert!(path.is_dir());
        staging.verify().unwrap();
        assert!(fs::rename(&path, directory.path().join("moved-tag-edits")).is_err());
        let same = repo.ensure_tag_staging().unwrap();
        assert_eq!(same.identity(), staging.identity());
    }

    #[cfg(windows)]
    #[test]
    fn a_foreign_file_at_tag_staging_is_preserved_and_never_repurposed() {
        let directory = profile();
        let repo = Repository::open(directory.path()).unwrap();
        let path = directory.path().join("tag-edits");
        let foreign = b"foreign directory occupant";
        fs::write(&path, foreign).unwrap();
        assert!(repo.ensure_tag_staging().is_err());
        assert_eq!(fs::read(&path).unwrap(), foreign);
        second_writer_is_refused(directory.path());
    }

    #[test]
    fn a_hard_linked_lock_leaf_cannot_become_an_exclusive_guard() {
        let directory = profile();
        let lock = directory.path().join(LOCK_NAME);
        let alias = directory.path().join("synthetic-lock-alias");
        fs::write(&lock, []).unwrap();
        fs::hard_link(&lock, &alias).unwrap();
        let failure = ProfileGuard::acquire(directory.path()).unwrap_err();
        let owner = failure.owner.as_ref().unwrap();
        assert!(!owner.is_locked());
        assert_eq!(
            owner.verify().unwrap_err().kind,
            ProfileGuardFailureKind::NotLocked
        );
        drop(failure);
        fs::remove_file(alias).unwrap();
        let guard = ProfileGuard::acquire(directory.path()).unwrap();
        guard.verify().unwrap();
    }

    #[test]
    fn native_lock_name_and_continuous_directory_pins_reject_replacement() {
        let directory = profile();
        let root = directory.path().join("profile");
        let guard = ProfileGuard::acquire(&root).unwrap();
        let lock = root.join(LOCK_NAME);
        let replacement = root.join("renamed-lock");
        #[cfg(windows)]
        {
            assert!(
                fs::rename(&lock, &replacement).is_err(),
                "The actual lock FD denies SHARE_DELETE"
            );
            assert!(
                fs::rename(&root, directory.path().join("renamed-profile")).is_err(),
                "Continuous actual directory pins deny renaming"
            );
            guard.verify().unwrap();
        }
        #[cfg(target_os = "linux")]
        {
            fs::rename(&lock, &replacement).unwrap();
            assert_eq!(
                guard.verify().unwrap_err().kind,
                ProfileGuardFailureKind::Verify
            );
            fs::rename(&replacement, &lock).unwrap();
            guard.verify().unwrap();
            let moved = directory.path().join("renamed-profile");
            fs::rename(&root, &moved).unwrap();
            assert_eq!(
                guard.verify().unwrap_err().kind,
                ProfileGuardFailureKind::Verify
            );
            fs::rename(moved, &root).unwrap();
            guard.verify().unwrap();
        }
    }
}

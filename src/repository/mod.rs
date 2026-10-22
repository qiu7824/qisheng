//! Native transactional storage. Source approvals remain in ProjectStore.

mod artwork;
mod backup;
pub(crate) mod backup_export;
pub use backup::{LxBackupCommit, LxBackupPreview};
pub mod dislike;
mod download;
mod download_artifacts;
pub use download_artifacts::DownloadOwnedArtifact;
mod library;
pub(crate) use library::DownloadReadCommit;
mod library_binding;
mod library_evidence_batch;
pub mod lists;
mod lyrics;
mod media;
mod native_backup;
pub use native_backup::{
    NativeBackupArchive, NativeBackupCommit, NativeBackupCounts, NativeBackupPreview,
};
pub mod physical_snapshot_ledger;
mod playback;
mod profile_guard;
#[cfg(windows)]
pub mod profile_install;
#[cfg(all(test, windows))]
pub(crate) use profile_guard::protect_close_proof_for_profile_for_test;
pub use profile_guard::{
    OpeningProfile, ProfileGuard, ProfileGuardError, ProfileGuardFailure, ProfileGuardFailureKind,
    RepositoryCloseFailure, RepositoryCloseOwner, RepositoryOpenFailure, RepositoryOpenOwner,
};
mod schema;
pub mod snapshot_export;
mod snapshot_native;
pub(crate) use snapshot_native::FileIdentity as ProfileLockIdentity;
pub(crate) use snapshot_native::impulse_vault::{
    ColdImpulseRetirement, ImpulseRetirementOwner, ImpulseVault, ImpulseVaultCreator,
};
pub use snapshot_native::impulse_vault::{
    ImpulseFileIdentity, ImpulseImportFact, ImpulseImportPhase, ImpulseRetirementRecord,
    ImpulseVaultCreationFact, ImpulseVaultCreationPhase, ImpulseVaultReceipt,
    MAX_IMPULSE_VAULT_BYTES, MAX_STORED_IMPULSES, StoredImpulseResource,
};
pub(crate) use snapshot_native::{
    SnapshotContext as ImpulseRetirementContext, SnapshotDirectory as ImpulseProfileDirectory,
};
mod snapshot_sanitize;
mod snapshot_vfs;
mod sync;
pub mod tag_edit;
pub use sync::SyncProfileState;
pub use tag_edit::{
    RepositoryTagEditCoordinator, TagEditArtifact, TagEditArtifactPhase, TagEditBudget,
    TagEditCommit, TagEditOperation, TagEditPhase, TagEditToken, TagWriteApproval,
};
mod tracks;

pub use library::{LibraryBatchCommit, LibraryScanFinish};
pub use library_binding::LibraryRelinkRepositoryCommit;
pub use library_evidence_batch::LibraryContentEvidenceCommit;
pub use media::{MAX_MEDIA_CACHE_BYTES, MAX_MEDIA_CACHE_ROWS, MediaCacheStats};
pub use playback::{MAX_PLAYBACK_CHECKPOINT_BYTES, PlaybackCheckpoint};
pub use schema::{APPLICATION_ID, SCHEMA_VERSION};
pub use tracks::{LegacyLocalImport, local_track_from_path};

use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior};
use serde::{Deserialize, Serialize};
use std::{
    path::{Path, PathBuf},
    sync::Mutex,
    time::{SystemTime, UNIX_EPOCH},
};

pub const DATABASE_NAME: &str = "native-library.sqlite3";
pub const MAX_DOCUMENT_BYTES: usize = 8 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeKind {
    TrackUpserted,
    LocalAssetUpserted,
    LocalAssetsUpserted,
    LyricsReplaced,
    EditedLyricsRemoved,
    RawLyricsCleared,
    LegacyLocalFilesImported,
    ProfileChanged,
    ListsChanged,
    ListPositionChanged,
    ListUpdateInfoChanged,
    DislikeChanged,
    SearchHistoryChanged,
    PlaybackCheckpointChanged,
    LibraryRootChanged,
    LibraryScanStarted,
    LibraryScanBatchCommitted,
    LibraryScanFinished,
    LibraryScansInterrupted,
    LibraryWatchChanged,
    LocalAssetRelinked,
    LibraryRootRelinked,
    DownloadTasksChanged,
    DownloadTasksInterrupted,
    DownloadSettingsChanged,
    DownloadDestinationChanged,
    TagWriteAuthorityChanged,
    TagEditOperationChanged,
    LocalTagsEdited,
    LocalTagsRestored,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RepositoryChange {
    pub revision: u64,
    pub kind: ChangeKind,
    pub track_id: Option<String>,
    pub details: serde_json::Value,
}

pub struct Repository {
    connection: Mutex<Connection>,
    sync_registry: Mutex<sync::SyncSessionRegistry>,
    tag_finished_validation: Mutex<tag_edit::FinishedValidationCache>,
    path: PathBuf,
    profile_guard: ProfileGuard,
}

impl Repository {
    pub fn open(profile_directory: impl AsRef<Path>) -> Result<Self> {
        let guard = ProfileGuard::acquire(profile_directory)
            .map_err(|failure| anyhow::Error::msg(failure.to_string()))?;
        Self::open_with_guard(guard).map_err(|failure| anyhow::Error::msg(failure.to_string()))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn integrity_check(&self) -> Result<()> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("Repository lock was poisoned"))?;
        schema::validate_current(&connection)?;
        schema::integrity_check(&connection)
    }

    pub fn revision(&self) -> Result<u64> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("Repository lock was poisoned"))?;
        Ok(connection.query_row(
            "SELECT revision FROM repository_state WHERE singleton=1",
            [],
            |row| row.get::<_, i64>(0),
        )? as u64)
    }

    /// Outbox records are visible only after their data transaction commits.
    pub fn changes_since(&self, revision: u64, limit: usize) -> Result<Vec<RepositoryChange>> {
        ensure!(limit <= 1000, "Outbox page exceeds 1000 records");
        let revision = checked_revision(revision)?;
        let connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("Repository lock was poisoned"))?;
        let mut statement = connection.prepare(
            "SELECT document FROM change_outbox WHERE revision>?1 ORDER BY revision LIMIT ?2",
        )?;
        let rows = statement.query_map(rusqlite::params![revision, limit as i64], |row| {
            row.get::<_, String>(0)
        })?;
        rows.map(|row| decode_document(&row?)).collect()
    }

    pub fn acknowledge_changes_through(&self, revision: u64) -> Result<()> {
        let revision = checked_revision(revision)?;
        let mut connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("Repository lock was poisoned"))?;
        schema::validate_current(&connection)?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current: i64 = transaction.query_row(
            "SELECT revision FROM repository_state WHERE singleton=1",
            [],
            |row| row.get(0),
        )?;
        ensure!(
            revision <= current,
            "Cannot acknowledge a future repository revision"
        );
        transaction.execute("DELETE FROM change_outbox WHERE revision<=?1", [revision])?;
        transaction.commit()?;
        Ok(())
    }

    pub fn backup_to(&self, destination: impl AsRef<Path>) -> Result<()> {
        let destination = destination.as_ref();
        ensure!(
            !destination.try_exists()?,
            "Repository backup destination already exists"
        );
        let connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("Repository lock was poisoned"))?;
        schema::validate_current(&connection)?;
        schema::integrity_check(&connection)?;
        connection
            .backup("main", destination, None)
            .context("Cannot back up native repository")
    }

    pub fn import_marker(&self, name: &str) -> Result<Option<String>> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("Repository lock was poisoned"))?;
        Ok(connection
            .query_row(
                "SELECT source_digest FROM import_markers WHERE name=?1",
                [name],
                |row| row.get(0),
            )
            .optional()?)
    }

    /// Records the digest of committed ProjectStore JSON without copying source permissions.
    /// Repeating the current digest returns its original change and publishes nothing new.
    pub fn record_profile_change(&self, digest: &str) -> Result<RepositoryChange> {
        ensure!(
            digest.len() == 64 && digest.bytes().all(|byte| byte.is_ascii_hexdigit()),
            "Profile metadata requires a SHA-256 digest"
        );
        let digest = digest.to_ascii_lowercase();
        let mut connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("Repository lock was poisoned"))?;
        schema::validate_current(&connection)?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let previous: Option<(String, String)> = transaction
            .query_row(
                "SELECT digest, change_document FROM profile_metadata WHERE singleton=1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        if let Some((previous_digest, document)) = previous
            && previous_digest == digest
        {
            let change: RepositoryChange = decode_document(&document)?;
            ensure!(
                change.kind == ChangeKind::ProfileChanged
                    && change
                        .details
                        .get("digest")
                        .and_then(serde_json::Value::as_str)
                        == Some(digest.as_str()),
                "Stored profile change is invalid"
            );
            transaction.commit()?;
            return Ok(change);
        }
        let current: i64 = transaction.query_row(
            "SELECT revision FROM repository_state WHERE singleton=1",
            [],
            |row| row.get(0),
        )?;
        let change = commit_change(
            &transaction,
            current,
            ChangeKind::ProfileChanged,
            None,
            serde_json::json!({"digest":digest}),
        )?;
        transaction.execute(
            "INSERT INTO profile_metadata(singleton,digest,change_document) VALUES(1,?1,?2) ON CONFLICT(singleton) DO UPDATE SET digest=excluded.digest,change_document=excluded.change_document",
            rusqlite::params![digest, encode_document(&change)?],
        )?;
        transaction
            .commit()
            .context("Cannot commit profile metadata change")?;
        Ok(change)
    }

    fn write<T>(
        &self,
        expected_revision: Option<u64>,
        operation: impl FnOnce(
            &Transaction<'_>,
        ) -> Result<(T, ChangeKind, Option<String>, serde_json::Value)>,
    ) -> Result<(T, RepositoryChange)> {
        self.write_checked(expected_revision, operation, &|| Ok(()))
    }

    fn write_checked<T>(
        &self,
        expected_revision: Option<u64>,
        operation: impl FnOnce(
            &Transaction<'_>,
        ) -> Result<(T, ChangeKind, Option<String>, serde_json::Value)>,
        check: &dyn Fn() -> Result<()>,
    ) -> Result<(T, RepositoryChange)> {
        check()?;
        let mut connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("Repository lock was poisoned"))?;
        schema::validate_current(&connection)?;
        check()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current: i64 = transaction.query_row(
            "SELECT revision FROM repository_state WHERE singleton=1",
            [],
            |row| row.get(0),
        )?;
        if let Some(expected) = expected_revision {
            ensure!(
                checked_revision(expected)? == current,
                "Repository revision conflict"
            );
        }
        let (result, kind, track_id, details) = operation(&transaction)?;
        let change = commit_change(&transaction, current, kind, track_id, details)?;
        check()?;
        transaction
            .commit()
            .context("Cannot commit repository transaction")?;
        Ok((result, change))
    }
}

fn commit_change(
    transaction: &Transaction<'_>,
    current: i64,
    kind: ChangeKind,
    track_id: Option<String>,
    details: serde_json::Value,
) -> Result<RepositoryChange> {
    let revision = current
        .checked_add(1)
        .context("Repository revision overflow")?;
    let change = RepositoryChange {
        revision: revision as u64,
        kind,
        track_id,
        details,
    };
    transaction.execute(
        "UPDATE repository_state SET revision=?1 WHERE singleton=1",
        [revision],
    )?;
    sync::record_change(transaction, &change.kind, &change.details, change.revision)?;
    transaction.execute(
        "INSERT INTO change_outbox(revision,document) VALUES(?1,?2)",
        rusqlite::params![revision, encode_document(&change)?],
    )?;
    Ok(change)
}

pub(crate) fn encode_document(value: &impl Serialize) -> Result<String> {
    let document = serde_json::to_string(value).context("Cannot encode repository document")?;
    ensure!(
        document.len() <= MAX_DOCUMENT_BYTES,
        "Repository document exceeds 8 MiB"
    );
    Ok(document)
}

pub(crate) fn decode_document<T: serde::de::DeserializeOwned>(document: &str) -> Result<T> {
    ensure!(
        document.len() <= MAX_DOCUMENT_BYTES,
        "Repository document exceeds 8 MiB"
    );
    serde_json::from_str(document).context("Repository document is invalid")
}

fn checked_revision(revision: u64) -> Result<i64> {
    i64::try_from(revision).context("Repository revision exceeds supported range")
}

fn unique_backup_path(path: &Path, version: u32) -> PathBuf {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    path.with_extension(format!(
        "schema-{version}-{}-{timestamp}.bak",
        std::process::id()
    ))
}

#[cfg(test)]
mod cancellation_tests {
    use super::*;
    use crate::domain::library::{LibraryError, LibraryErrorKind};
    use std::sync::atomic::{AtomicBool, Ordering};

    #[test]
    fn cancellation_after_data_writes_rolls_back_data_revision_and_outbox_before_commit() {
        let profile = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
        let repository = Repository::open(profile.path()).unwrap();
        let cancelled = AtomicBool::new(false);
        let check = || {
            if cancelled.load(Ordering::Acquire) {
                Err(LibraryError::new(
                    LibraryErrorKind::Cancelled,
                    "Cancelled after transaction writes",
                )
                .into())
            } else {
                Ok(())
            }
        };
        let result = repository.write_checked(
            None,
            |transaction| {
                transaction.execute(
                    "INSERT INTO tracks(id,document) VALUES('cancelled-fixture','{}')",
                    [],
                )?;
                cancelled.store(true, Ordering::Release);
                Ok((
                    (),
                    ChangeKind::LibraryScanBatchCommitted,
                    None,
                    serde_json::json!({"fixture":"cancelled"}),
                ))
            },
            &check,
        );
        assert!(result.is_err());
        assert_eq!(repository.revision().unwrap(), 0);
        assert!(repository.tracks().unwrap().is_empty());
        assert!(repository.changes_since(0, 10).unwrap().is_empty());
        repository.integrity_check().unwrap();
    }
}

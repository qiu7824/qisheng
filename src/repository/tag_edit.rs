//! Independent local-tag write consent and durable same-object installation CAS.
//! Native ownership is supplied by the installer; stored documents are not leases.

use super::{
    ChangeKind, Repository, RepositoryChange, commit_change, decode_document, encode_document,
    library, library_binding, schema, tracks,
};
use crate::{
    domain::{
        library::{FileIdentity, LibraryError, validate_library_id, validate_library_path},
        library_watch::validate_content_sha256,
        music::normalize_local_path,
    },
    library::tag_install::{
        TagArtifactAllocation, TagArtifactRetirementWitness, TagArtifactSeal, TagFileObservation,
        TagInstallArtifactFacts, TagInstallArtifactRole, TagInstallCoordinator, TagInstallPhase,
        TagInstallationReceipt, TagOriginalCloseWitness, TagOriginalRecoveryIntent,
        TagOriginalRetirementRecoveryWitness, TagOriginalRetirementSeal, TagPersistenceBudget,
        TagPreparedAsset, TagRecoveryDisposition, TagRetirementRecovery,
        TagRetirementRecoveryArtifact, TagUnallocatedArtifactWitness,
        TagUnopenedInstallationWitness, TagWriteSelection,
    },
};
use anyhow::{Context, Result, ensure};
use rusqlite::{
    Connection, OptionalExtension, Transaction, TransactionBehavior, params, types::ValueRef,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{Arc, Mutex},
};

pub const MAX_TAG_EDIT_OPERATIONS: usize = 64;
pub const MAX_TAG_RECOVERY_APPROVALS: usize = 64;
const MAX_RECOVERY_HISTORY_BYTES: usize = 65536;
const MAX_RECOVERY_HISTORY_DOCUMENT_BYTES: usize = MAX_RECOVERY_HISTORY_BYTES + 64;
pub const MAX_ACTIVE_TAG_EDITS: usize = 4;
pub const MAX_TAG_EDIT_LEDGER_BYTES: u64 = 16 * 1024 * 1024;
pub const MAX_TAG_EDIT_OWNED_BYTES: u64 = 4 * 1024 * 1024 * 1024;
const MAX_BODY: u64 = 1024 * 1024 * 1024 + 16 * 1024 * 1024;
const MAX_JOURNAL: u64 = 65 * 1024 * 1024;

/// Only a fully validated terminal operation can populate this cache. Each
/// lookup hashes every sealed SQL value again; it carries no grant or file lease.
#[derive(Default)]
pub(crate) struct FinishedValidationCache {
    entries: BTreeMap<String, [u8; 32]>,
}

fn hash_sql_value(hash: &mut Sha256, value: ValueRef<'_>) {
    match value {
        ValueRef::Null => hash.update([0]),
        ValueRef::Integer(value) => {
            hash.update([1]);
            hash.update(value.to_le_bytes());
        }
        ValueRef::Real(value) => {
            hash.update([2]);
            hash.update(value.to_bits().to_le_bytes());
        }
        ValueRef::Text(bytes) | ValueRef::Blob(bytes) => {
            hash.update([if matches!(value, ValueRef::Text(_)) {
                3
            } else {
                4
            }]);
            hash.update((bytes.len() as u64).to_le_bytes());
            hash.update(bytes);
        }
    }
}

fn operation_sql_fingerprint(connection: &Connection, id: &str) -> Result<[u8; 32]> {
    let mut hash = Sha256::new();
    hash.update(b"tag-finished-sql-v1");
    connection.query_row("SELECT operation_id,approval_id,asset_id,operation_epoch,ledger_revision,phase,document,byte_size FROM tag_edit_operations WHERE operation_id=?1", [id], |row| {
        for column in 0..8 {
            hash_sql_value(&mut hash, row.get_ref(column)?);
        }
        Ok(())
    })?;
    let mut statement = connection.prepare("SELECT operation_id,role,phase,actual_bytes,reserved_bytes,document,byte_size FROM tag_edit_artifacts WHERE operation_id=?1 ORDER BY role")?;
    let mut rows = statement.query([id])?;
    let mut count = 0u64;
    while let Some(row) = rows.next()? {
        hash.update(b"artifact");
        for column in 0..7 {
            hash_sql_value(&mut hash, row.get_ref(column)?);
        }
        count += 1;
    }
    hash.update(count.to_le_bytes());
    Ok(hash.finalize().into())
}

fn pending_operation_at(
    connection: &Connection,
    id: &str,
    cache: &Mutex<FinishedValidationCache>,
) -> Result<Option<TagEditOperation>> {
    let fingerprint = operation_sql_fingerprint(connection, id)?;
    let hit = cache
        .lock()
        .map_err(|_| anyhow::anyhow!("Tag validation cache was poisoned"))?
        .entries
        .get(id)
        .is_some_and(|known| *known == fingerprint);
    if hit {
        return Ok(None);
    }
    let operation = operation_at(connection, id)?.context("Tag operation disappeared")?;
    let mut cache = cache
        .lock()
        .map_err(|_| anyhow::anyhow!("Tag validation cache was poisoned"))?;
    cache.entries.remove(id);
    if operation.finished {
        // A transaction rollback cannot produce a false hit: the complete raw
        // operation and artifact rows must still equal the validated contents.
        if cache.entries.len() < MAX_TAG_EDIT_OPERATIONS {
            cache.entries.insert(id.into(), fingerprint);
        }
        Ok(None)
    } else {
        Ok(Some(operation))
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TagWriteApproval {
    pub approval_id: String,
    pub approval_epoch: u64,
    pub selection: TagWriteSelection,
    pub revoked: bool,
    #[serde(default)]
    pub purpose: TagWritePurpose,
}
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum TagWritePurpose {
    #[default]
    Install,
    Recovery {
        operation_id: String,
        operation_epoch: u64,
    },
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TagEditBudget {
    pub original_bytes: u64,
    pub journal_bytes: u64,
    pub backup_bytes: u64,
    pub candidate_bytes: u64,
}
impl TagEditBudget {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            (1..=MAX_JOURNAL).contains(&self.journal_bytes)
                && (1..=MAX_BODY).contains(&self.backup_bytes)
                && (1..=MAX_BODY).contains(&self.candidate_bytes)
                && self.original_bytes >= self.candidate_bytes
                && self.original_bytes <= MAX_BODY,
            LibraryError::budget("Tag installation reservation is invalid")
        );
        ensure!(
            self.total()? <= MAX_TAG_EDIT_OWNED_BYTES,
            LibraryError::budget("Tag installation exceeds 4 GiB")
        );
        Ok(())
    }
    pub fn total(&self) -> Result<u64> {
        [
            self.original_bytes,
            self.journal_bytes,
            self.backup_bytes,
            self.candidate_bytes,
        ]
        .into_iter()
        .try_fold(0u64, |sum, item| {
            sum.checked_add(item).context("Tag reservation overflow")
        })
    }
    pub fn artifact_limit(&self, role: TagInstallArtifactRole) -> u64 {
        match role {
            TagInstallArtifactRole::Journal => self.journal_bytes,
            TagInstallArtifactRole::Backup => self.backup_bytes,
            TagInstallArtifactRole::Candidate => self.candidate_bytes,
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TagEditPhase {
    Registered,
    Prepared,
    BackupSealed,
    CandidateVerified,
    Installing,
    DiskVerified,
    RepoCommitted,
    Published,
    Restoring,
    Restored,
}
impl TagEditPhase {
    fn name(self) -> &'static str {
        match self {
            Self::Registered => "registered",
            Self::Prepared => "prepared",
            Self::BackupSealed => "backup_sealed",
            Self::CandidateVerified => "candidate_verified",
            Self::Installing => "installing",
            Self::DiskVerified => "disk_verified",
            Self::RepoCommitted => "repo_committed",
            Self::Published => "published",
            Self::Restoring => "restoring",
            Self::Restored => "restored",
        }
    }
}
impl From<TagInstallPhase> for TagEditPhase {
    fn from(value: TagInstallPhase) -> Self {
        match value {
            TagInstallPhase::Prepared => Self::Prepared,
            TagInstallPhase::BackupSealed => Self::BackupSealed,
            TagInstallPhase::CandidateVerified => Self::CandidateVerified,
            TagInstallPhase::Installing => Self::Installing,
            TagInstallPhase::DiskVerified => Self::DiskVerified,
            TagInstallPhase::RepoCommitted => Self::RepoCommitted,
            TagInstallPhase::Published => Self::Published,
            TagInstallPhase::Restoring => Self::Restoring,
            TagInstallPhase::Restored => Self::Restored,
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TagEditArtifactPhase {
    Reserved,
    Allocated,
    Sealed,
    Retiring,
    Retired,
}
impl TagEditArtifactPhase {
    fn name(self) -> &'static str {
        match self {
            Self::Reserved => "reserved",
            Self::Allocated => "allocated",
            Self::Sealed => "sealed",
            Self::Retiring => "retiring",
            Self::Retired => "retired",
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TagEditArtifact {
    pub operation_id: String,
    pub operation_epoch: u64,
    pub role: String,
    pub path: PathBuf,
    pub phase: TagEditArtifactPhase,
    pub maximum_bytes: u64,
    pub actual_bytes: u64,
    pub identity: Option<FileIdentity>,
    pub directory_identity: Option<FileIdentity>,
    pub sha256: Option<String>,
    #[serde(default)]
    pub retired_size_bytes: Option<u64>,
    #[serde(default)]
    pub never_allocated: bool,
    #[serde(default)]
    pub allocation_attempted: Option<bool>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TagOriginalCloseRecord {
    pub observation: TagFileObservation,
    pub installed_facts_sha256: Option<String>,
    pub never_modified: bool,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TagOriginalRecoveryRecord {
    pub intent: TagOriginalCloseRecord,
    pub body_sha256: String,
    pub proof: String,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TagEditQuota {
    pub pending_operations: usize,
    pub charged_bytes: u64,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TagDocumentBudget {
    pub actual_bytes: u64,
    pub reserved_bytes: u64,
    pub charged_bytes: u64,
    pub maximum_bytes: u64,
}
fn document_reservation_absent(value: &bool) -> bool {
    !*value
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TagEditToken {
    pub operation_id: String,
    pub operation_epoch: u64,
    pub approval_id: String,
    pub approval_epoch: u64,
    pub request_digest: String,
    pub input_digest: String,
    pub selection: TagWriteSelection,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TagEditCommit {
    pub receipt: TagInstallationReceipt,
    pub change: RepositoryChange,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TagEditOperation {
    pub token: TagEditToken,
    pub budget: TagEditBudget,
    pub ledger_revision: u64,
    pub phase: TagEditPhase,
    #[serde(default, skip_serializing_if = "document_reservation_absent")]
    pub document_budget_reserved: bool,
    #[serde(default, skip_serializing_if = "document_reservation_absent")]
    pub recovery_budget_reserved: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub recovery_approval_history: Vec<TagWriteApproval>,
    pub commit: Option<TagEditCommit>,
    pub restoration: Option<TagEditCommit>,
    // Original write ownership is charged until an actual close witness exists.
    // Published/Restored alone must never release this reservation.
    pub original_owner_retired: bool,
    #[serde(default)]
    pub installation_attempted: Option<bool>,
    #[serde(default)]
    pub original_close: Option<TagOriginalCloseRecord>,
    #[serde(default)]
    pub original_retirement_intent: Option<TagOriginalCloseRecord>,
    #[serde(default)]
    pub original_recovery: Option<TagOriginalRecoveryRecord>,
    #[serde(default)]
    pub original_never_opened: bool,
    #[serde(default)]
    pub finished: bool,
    pub artifacts: Vec<TagEditArtifact>,
}
impl TagEditOperation {
    pub fn is_finished(&self) -> bool {
        self.finished
    }
}

fn complete_if_retired(value: &mut TagEditOperation) -> Result<()> {
    let no_objects = value.original_owner_retired
        && value
            .artifacts
            .iter()
            .all(|item| item.phase == TagEditArtifactPhase::Retired);
    if !no_objects {
        return Ok(());
    }
    let no_mutation = (value.original_never_opened
        || value
            .original_close
            .as_ref()
            .is_some_and(|closed| closed.never_modified))
        && value.commit.is_none()
        && value.restoration.is_none()
        && matches!(
            value.phase,
            TagEditPhase::Registered
                | TagEditPhase::Prepared
                | TagEditPhase::BackupSealed
                | TagEditPhase::CandidateVerified
        );
    ensure!(
        matches!(
            value.phase,
            TagEditPhase::Published | TagEditPhase::Restored
        ) || no_mutation,
        LibraryError::conflict(
            "Tag objects cannot finish before committed publication or verified rollback"
        )
    );
    value.finished = true;
    Ok(())
}
fn validate_original_retirement_record(
    operation: &TagEditOperation,
    record: &TagOriginalCloseRecord,
) -> Result<()> {
    ensure!(
        record.observation.identity == operation.token.selection.expected_identity
            && normalize_local_path(&record.observation.path)
                == normalize_local_path(&operation.token.selection.path),
        LibraryError::conflict("Original retirement intent changed the selected object")
    );
    if let Some(commit) = operation.commit.as_ref().or(operation.restoration.as_ref()) {
        ensure!(
            matches!(
                operation.phase,
                TagEditPhase::Published | TagEditPhase::Restored
            ) && commit
                .receipt
                .installed
                .as_ref()
                .is_some_and(|facts| facts.after == record.observation)
                && record.installed_facts_sha256
                    == commit.receipt.installed.as_ref().map(digest).transpose()?,
            LibraryError::conflict("Original retirement intent differs from committed facts")
        );
    } else {
        ensure!(
            record.never_modified
                && record.installed_facts_sha256.is_none()
                && matches!(
                    operation.phase,
                    TagEditPhase::Registered
                        | TagEditPhase::Prepared
                        | TagEditPhase::BackupSealed
                        | TagEditPhase::CandidateVerified
                ),
            LibraryError::conflict(
                "Mutated original cannot seal retirement before verified rollback"
            )
        );
    }
    Ok(())
}

/// Owns the durable operation token; the installer separately owns the actual
/// exclusive writer. Receipt-only calls cannot commit this local edit.
pub struct RepositoryTagEditCoordinator {
    repository: Arc<Repository>,
    token: TagEditToken,
    check: Arc<dyn Fn() -> Result<()> + Send + Sync>,
    owner_check: Arc<dyn Fn() -> Result<()> + Send + Sync>,
    recovery: Option<(String, u64)>,
}
impl RepositoryTagEditCoordinator {
    pub fn new(
        repository: Arc<Repository>,
        token: TagEditToken,
        check: Arc<dyn Fn() -> Result<()> + Send + Sync>,
        owner_check: Arc<dyn Fn() -> Result<()> + Send + Sync>,
    ) -> Result<Self> {
        owner_check()?;
        let stored = repository
            .tag_edit_operation(&token.operation_id)?
            .context("Tag coordinator operation is absent")?;
        ensure!(
            digest(&stored.token)? == digest(&token)?,
            LibraryError::conflict("Tag coordinator operation token changed")
        );
        repository.reserve_tag_documents(&token, owner_check.as_ref())?;
        Ok(Self {
            repository,
            token,
            check,
            owner_check,
            recovery: None,
        })
    }
    pub fn token(&self) -> &TagEditToken {
        &self.token
    }
    pub fn for_recovery(
        repository: Arc<Repository>,
        token: TagEditToken,
        approval_id: String,
        approval_epoch: u64,
        check: Arc<dyn Fn() -> Result<()> + Send + Sync>,
        owner_check: Arc<dyn Fn() -> Result<()> + Send + Sync>,
    ) -> Result<Self> {
        check()?;
        owner_check()?;
        repository.verify_tag_recovery_approval(&token, &approval_id, approval_epoch)?;
        repository.reserve_tag_documents(&token, &|| {
            owner_check()?;
            check()
        })?;
        Ok(Self {
            repository,
            token,
            check,
            owner_check,
            recovery: Some((approval_id, approval_epoch)),
        })
    }
    fn operation(&self) -> Result<TagEditOperation> {
        (self.owner_check)()?;
        let value = self
            .repository
            .tag_edit_operation(&self.token.operation_id)?
            .context("Tag operation is absent")?;
        ensure!(
            digest(&value.token)? == digest(&self.token)?,
            LibraryError::conflict("Tag coordinator token changed")
        );
        Ok(value)
    }
    fn selected(&self, selection: &TagWriteSelection) -> Result<()> {
        ensure!(
            same_selection(selection, &self.token.selection)?,
            LibraryError::conflict("Tag coordinator selection changed")
        );
        Ok(())
    }
}
impl TagInstallCoordinator for RepositoryTagEditCoordinator {
    fn check_persistence_budget(
        &self,
        selection: &TagWriteSelection,
        budget: &TagPersistenceBudget<'_>,
    ) -> Result<()> {
        self.selected(selection)?;
        self.repository
            .check_tag_persistence_budget(&self.token, budget, &|| {
                (self.owner_check)()?;
                (self.check)()
            })
    }
    fn begin_original_retirement(
        &mut self,
        selection: &TagWriteSelection,
        seal: &TagOriginalRetirementSeal<'_>,
    ) -> Result<()> {
        self.selected(selection)?;
        self.repository
            .begin_tag_original_retirement(&self.token, seal, self.owner_check.as_ref())
    }
    fn original_retirement_recovered(
        &mut self,
        selection: &TagWriteSelection,
        witness: &TagOriginalRetirementRecoveryWitness,
    ) -> Result<()> {
        self.selected(selection)?;
        self.repository.acknowledge_tag_original_recovery(
            &self.token,
            witness,
            self.owner_check.as_ref(),
        )
    }
    fn before_installation_open(&self, selection: &TagWriteSelection) -> Result<()> {
        self.selected(selection)?;
        self.repository
            .prepare_tag_installation_open(&self.token, &|| {
                (self.owner_check)()?;
                (self.check)()
            })
    }
    fn recovery_unopened(&self, selection: &TagWriteSelection) -> Result<bool> {
        self.authorize_recovery_selection(selection)?;
        self.repository.tag_recovery_unopened(&self.token)
    }
    fn recovery_retirement(
        &self,
        selection: &TagWriteSelection,
    ) -> Result<Option<TagRetirementRecovery>> {
        self.authorize_recovery_selection(selection)?;
        self.repository.tag_retirement_recovery(&self.token)
    }
    fn authorize_recovery_selection(&self, selection: &TagWriteSelection) -> Result<()> {
        self.selected(selection)?;
        (self.owner_check)()?;
        (self.check)()?;
        let (id, epoch) = self
            .recovery
            .as_ref()
            .context("Cold tag recovery requires current explicit recovery approval")?;
        self.repository
            .verify_tag_recovery_approval(&self.token, id, *epoch)
            .map(|_| ())
    }
    fn recovery_expected_body_sha256(
        &self,
        selection: &TagWriteSelection,
    ) -> Result<Option<String>> {
        self.authorize_recovery_selection(selection)?;
        let (id, epoch) = self.recovery.as_ref().unwrap();
        Ok(self
            .repository
            .verify_tag_recovery_approval(&self.token, id, *epoch)?
            .selection
            .expected_body_sha256)
    }
    fn authorize_explicit_selection(&self, selection: &TagWriteSelection) -> Result<()> {
        (self.check)()?;
        (self.owner_check)()?;
        self.selected(selection)?;
        self.repository.check_tag_edit_intent(&self.token)
    }
    fn check_current_intent(&self, selection: &TagWriteSelection) -> Result<()> {
        self.selected(selection)?;
        let operation = self.operation()?;
        if let Some(commit) = operation.commit {
            self.repository
                .verify_tag_edit_commit(&self.token, &commit.receipt)
        } else {
            if self.recovery.is_some() {
                return self.authorize_recovery_selection(selection);
            }
            (self.check)()?;
            self.repository.check_tag_edit_intent(&self.token)
        }
    }
    fn journal_directory(&self) -> Result<PathBuf> {
        Ok(self
            .repository
            .path()
            .parent()
            .context("Repository profile directory is absent")?
            .join("tag-edits"))
    }
    fn commit_installed(
        &mut self,
        _: &TagWriteSelection,
        _: &TagInstallationReceipt,
    ) -> Result<u64> {
        anyhow::bail!("Local-tag commit requires the actual same-handle prepared asset")
    }
    fn publish_installed(
        &mut self,
        _: &TagWriteSelection,
        _: &TagInstallationReceipt,
    ) -> Result<()> {
        anyhow::bail!("Local-tag publication requires the actual same-handle prepared asset")
    }
    fn commit_prepared(
        &mut self,
        selection: &TagWriteSelection,
        receipt: &TagInstallationReceipt,
        prepared: &TagPreparedAsset<'_>,
    ) -> Result<u64> {
        self.selected(selection)?;
        ensure!(
            receipt.new_body_sha256 == prepared.facts().new_body_sha256
                && receipt.old_body_sha256 == prepared.facts().old_body_sha256
                && receipt.audio_sha256 == prepared.facts().audio_sha256,
            LibraryError::conflict("Tag receipt differs from the actual prepared body")
        );
        let already_committed = self.operation()?.commit.is_some();
        let commit = self
            .repository
            .commit_tag_edit(&self.token, prepared, &|| {
                (self.owner_check)()?;
                if !already_committed {
                    (self.check)()?;
                }
                Ok(())
            })?;
        commit
            .receipt
            .committed_binding_revision
            .context("Tag committed binding revision is absent")
    }
    fn publish_prepared(
        &mut self,
        selection: &TagWriteSelection,
        receipt: &TagInstallationReceipt,
        prepared: &TagPreparedAsset<'_>,
    ) -> Result<()> {
        self.selected(selection)?;
        (self.owner_check)()?;
        prepared.check()?;
        self.repository
            .verify_tag_edit_commit(&self.token, receipt)?;
        prepared.check()
    }
    fn check_recovery(
        &self,
        selection: &TagWriteSelection,
        _: TagInstallPhase,
        receipt: Option<&TagInstallationReceipt>,
    ) -> Result<TagRecoveryDisposition> {
        self.selected(selection)?;
        let operation = self.operation()?;
        if operation.commit.is_some() {
            self.repository.verify_tag_edit_commit(
                &self.token,
                receipt.context("Committed recovery receipt is absent")?,
            )?;
            return Ok(if operation.phase == TagEditPhase::Published {
                TagRecoveryDisposition::Completed
            } else {
                TagRecoveryDisposition::ResumePublication
            });
        }
        self.repository.check_tag_restore_binding(&self.token)?;
        Ok(TagRecoveryDisposition::RestoreOriginal)
    }
    fn checkpoint(&mut self, phase: TagInstallPhase) -> Result<()> {
        let current = self.operation()?;
        let check = || {
            (self.owner_check)()?;
            if !matches!(
                phase,
                TagInstallPhase::Restoring
                    | TagInstallPhase::Restored
                    | TagInstallPhase::RepoCommitted
                    | TagInstallPhase::Published
            ) {
                (self.check)()?;
            }
            Ok(())
        };
        self.repository.checkpoint_tag_edit(
            &self.token,
            current.ledger_revision,
            phase.into(),
            &check,
        )?;
        Ok(())
    }
    fn restored_asset(
        &mut self,
        selection: &TagWriteSelection,
        prepared: &TagPreparedAsset<'_>,
    ) -> Result<()> {
        self.selected(selection)?;
        // Cancellation has already triggered rollback. Restoring this actual
        // owned original is checked against its SQL binding rather than the
        // cancelled operation's ordinary continuation callback.
        self.repository
            .commit_tag_restoration(&self.token, prepared, self.owner_check.as_ref())?;
        Ok(())
    }
    fn check_install_chunk(&mut self, restoring: bool, _: u64) -> Result<()> {
        (self.owner_check)()?;
        if restoring {
            // Rollback of this still-owned mutation is independent of a newly
            // revoked edit consent; a concurrent binding change still fences it.
            self.repository.check_tag_restore_binding(&self.token)
        } else {
            (self.check)()?;
            self.repository.check_tag_edit_intent(&self.token)
        }
    }
    fn artifact_created(
        &mut self,
        _: &TagWriteSelection,
        _: &TagInstallArtifactFacts,
    ) -> Result<()> {
        anyhow::bail!("Local-tag allocation requires the actual empty file proof")
    }
    fn artifact_allocated(
        &mut self,
        selection: &TagWriteSelection,
        allocation: &TagArtifactAllocation<'_>,
    ) -> Result<()> {
        self.selected(selection)?;
        self.repository
            .acknowledge_tag_allocation(&self.token, allocation, &|| {
                (self.owner_check)()?;
                (self.check)()
            })
    }
    fn artifact_sealed(
        &mut self,
        selection: &TagWriteSelection,
        seal: &TagArtifactSeal<'_>,
    ) -> Result<()> {
        self.selected(selection)?;
        self.repository.seal_tag_artifact(&self.token, seal, &|| {
            (self.owner_check)()?;
            if !matches!(
                seal.phase(),
                TagInstallPhase::Restoring
                    | TagInstallPhase::Restored
                    | TagInstallPhase::RepoCommitted
                    | TagInstallPhase::Published
            ) {
                (self.check)()?;
            }
            Ok(())
        })
    }
    fn artifact_limit(&self, role: TagInstallArtifactRole) -> u64 {
        self.operation()
            .map_or(0, |value| value.budget.artifact_limit(role))
    }
    fn before_artifact_allocation(
        &mut self,
        selection: &TagWriteSelection,
        role: TagInstallArtifactRole,
    ) -> Result<()> {
        self.selected(selection)?;
        self.repository
            .prepare_tag_artifact_allocation(&self.token, role, &|| {
                (self.owner_check)()?;
                (self.check)()
            })
    }
    fn recovery_artifact(
        &self,
        selection: &TagWriteSelection,
        role: TagInstallArtifactRole,
    ) -> Result<crate::library::tag_install::TagRecoveryArtifactState> {
        self.selected(selection)?;
        (self.owner_check)()?;
        self.repository.tag_recovery_artifact(&self.token, role)
    }
    fn begin_artifact_retirement(
        &mut self,
        selection: &TagWriteSelection,
        seal: &TagArtifactSeal<'_>,
    ) -> Result<()> {
        self.selected(selection)?;
        self.repository
            .begin_tag_artifact_retirement(&self.token, seal, self.owner_check.as_ref())
    }
    fn artifact_retired(
        &mut self,
        selection: &TagWriteSelection,
        witness: &TagArtifactRetirementWitness,
    ) -> Result<()> {
        self.selected(selection)?;
        self.repository.acknowledge_tag_artifact_retirement(
            &self.token,
            witness,
            self.owner_check.as_ref(),
        )
    }
    fn artifact_unallocated(
        &mut self,
        selection: &TagWriteSelection,
        witness: &TagUnallocatedArtifactWitness,
    ) -> Result<()> {
        self.selected(selection)?;
        self.repository.acknowledge_tag_unallocated_artifact(
            &self.token,
            witness,
            self.owner_check.as_ref(),
        )
    }
    fn original_closed(
        &mut self,
        selection: &TagWriteSelection,
        witness: &TagOriginalCloseWitness,
    ) -> Result<()> {
        self.selected(selection)?;
        self.repository.acknowledge_tag_original_close(
            &self.token,
            witness,
            self.owner_check.as_ref(),
        )
    }
    fn installation_unopened(
        &mut self,
        selection: &TagWriteSelection,
        witness: &TagUnopenedInstallationWitness,
    ) -> Result<()> {
        self.selected(selection)?;
        self.repository.acknowledge_tag_installation_unopened(
            &self.token,
            witness,
            self.owner_check.as_ref(),
        )
    }
    fn check_retirement_owner(&self, selection: &TagWriteSelection) -> Result<()> {
        self.selected(selection)?;
        (self.owner_check)()?;
        self.operation()?;
        Ok(())
    }
}

fn role_name(role: TagInstallArtifactRole) -> &'static str {
    match role {
        TagInstallArtifactRole::Journal => "journal",
        TagInstallArtifactRole::Backup => "backup",
        TagInstallArtifactRole::Candidate => "candidate",
    }
}
fn digest(value: &impl Serialize) -> Result<String> {
    Ok(format!("{:x}", Sha256::digest(serde_json::to_vec(value)?)))
}
fn same_selection(a: &TagWriteSelection, b: &TagWriteSelection) -> Result<bool> {
    Ok(serde_json::to_value(a)? == serde_json::to_value(b)?)
}
fn recovery_approvals(
    connection: &Connection,
    token: &TagEditToken,
) -> Result<Vec<TagWriteApproval>> {
    let mut query = connection.prepare("SELECT approval_id FROM tag_write_approvals WHERE json_extract(document,'$.purpose.kind')='recovery' AND json_extract(document,'$.purpose.operation_id')=?1 AND json_extract(document,'$.purpose.operation_epoch')=?2 ORDER BY approval_epoch")?;
    query
        .query_map(
            params![token.operation_id, token.operation_epoch as i64],
            |row| row.get::<_, String>(0),
        )?
        .map(|id| approval_at(connection, &id?)?.context("Recovery approval disappeared"))
        .collect()
}
fn historical_approval_exists(connection: &Connection, id: &str) -> Result<bool> {
    Ok(connection.query_row("SELECT EXISTS(SELECT 1 FROM tag_edit_operations, json_each(tag_edit_operations.document,'$.recovery_approval_history') AS history WHERE json_extract(history.value,'$.approval_id')=?1)", [id], |row| row.get(0))?)
}
fn archive_recovery_approvals(
    tx: &Transaction<'_>,
    operation: &mut TagEditOperation,
    approvals: Vec<TagWriteApproval>,
) -> Result<()> {
    for mut approval in approvals {
        ensure!(
            approval.purpose
                == TagWritePurpose::Recovery {
                    operation_id: operation.token.operation_id.clone(),
                    operation_epoch: operation.token.operation_epoch
                }
                && !operation
                    .recovery_approval_history
                    .iter()
                    .any(|old| old.approval_id == approval.approval_id),
            LibraryError::conflict("Recovery approval history changed operation or reused ID")
        );
        approval.revoked = true;
        operation.recovery_approval_history.push(approval.clone());
        ensure!(
            operation.recovery_approval_history.len() <= MAX_TAG_RECOVERY_APPROVALS
                && serde_json::to_vec(&operation.recovery_approval_history)?.len()
                    <= MAX_RECOVERY_HISTORY_BYTES,
            LibraryError::budget("Recovery approval history exceeds its reserved 64 KiB")
        );
        ensure!(
            tx.execute(
                "DELETE FROM tag_write_approvals WHERE approval_id=?1 AND approval_epoch=?2",
                params![approval.approval_id, approval.approval_epoch as i64]
            )? == 1,
            LibraryError::conflict("Recovery approval changed before archival")
        );
    }
    Ok(())
}
fn release_finished_recovery_approvals(
    tx: &Transaction<'_>,
    operation: &mut TagEditOperation,
) -> Result<()> {
    if operation.finished {
        let approvals = recovery_approvals(tx, &operation.token)?;
        archive_recovery_approvals(tx, operation, approvals)?;
    }
    Ok(())
}
fn recovery_history_bytes(operation: &TagEditOperation) -> Result<usize> {
    Ok(serde_json::to_vec(&operation.recovery_approval_history)?.len())
}
fn adopt_document_reservations(operation: &mut TagEditOperation) -> Result<bool> {
    if operation.finished
        || (operation.document_budget_reserved && operation.recovery_budget_reserved)
    {
        return Ok(false);
    }
    operation.document_budget_reserved = true;
    operation.recovery_budget_reserved = true;
    let mut stored = operation.clone();
    stored.artifacts.clear();
    ensure!(
        encode_document(&stored)?
            .len()
            .checked_add(
                MAX_RECOVERY_HISTORY_DOCUMENT_BYTES
                    .saturating_sub(recovery_history_bytes(operation)?)
            )
            .is_some_and(|bytes| bytes <= 1024 * 1024),
        LibraryError::budget("Legacy tag operation has no reserved recovery history space")
    );
    Ok(true)
}
fn validate_selection(selection: &TagWriteSelection) -> Result<()> {
    validate_library_id(&selection.operation_id)?;
    validate_library_id(&selection.asset_id)?;
    validate_library_path(&selection.path)?;
    selection.expected_identity.validate()?;
    if let Some(root) = &selection.root_id {
        validate_library_id(root)?;
    }
    ensure!(
        selection.grant_epoch > 0
            && selection.grant_epoch <= i64::MAX as u64
            && selection.expected_binding_revision > 0
            && selection.expected_binding_revision <= i64::MAX as u64,
        LibraryError::invalid("Tag write fence is invalid")
    );
    validate_content_sha256(
        selection
            .expected_body_sha256
            .as_deref()
            .context("Tag selection requires an actual original body hash")?,
    )
}
fn require_binding(connection: &Connection, selection: &TagWriteSelection) -> Result<()> {
    require_binding_current(connection, selection, true)
}
fn require_binding_current(
    connection: &Connection,
    selection: &TagWriteSelection,
    compare_known_body: bool,
) -> Result<()> {
    validate_selection(selection)?;
    let binding = library_binding::binding_at(connection, &selection.asset_id)?
        .context("Tag asset binding is absent")?;
    ensure!(
        binding.binding_revision == selection.expected_binding_revision
            && binding.file_identity.as_ref() == Some(&selection.expected_identity)
            && normalize_local_path(&binding.path) == normalize_local_path(&selection.path),
        LibraryError::conflict("Tag asset binding changed")
    );
    let asset =
        library::asset_at(connection, &selection.asset_id)?.context("Tag asset is absent")?;
    ensure!(
        asset.available
            && normalize_local_path(&asset.path) == normalize_local_path(&selection.path),
        LibraryError::conflict("Tag asset is unavailable or moved")
    );
    if let Some(root_id) = &selection.root_id {
        let root = library::root_at(connection, root_id)?.context("Tag read root was revoked")?;
        let membership:bool=connection.query_row("SELECT EXISTS(SELECT 1 FROM library_membership m JOIN library_roots r ON r.root_id=m.root_id AND r.active=1 WHERE m.root_id=?1 AND m.asset_id=?2 AND r.grant_epoch=?3)",params![root_id,selection.asset_id,selection.grant_epoch as i64],|row|row.get(0))?;
        ensure!(
            membership
                && root.grant_epoch == selection.grant_epoch
                && library::path_in_root(&selection.path, &root),
            LibraryError::conflict("Tag root authority changed")
        );
    } else {
        ensure!(
            library_binding::exact_file_epoch(connection, &selection.asset_id)?
                == Some(selection.grant_epoch),
            LibraryError::conflict("Tag exact-file read authority changed")
        );
    }
    if compare_known_body
        && binding.hash_size_bytes == Some(asset.size_bytes)
        && binding.hash_modified_ms == asset.modified_ms
        && let Some(hash) = binding.content_sha256
    {
        ensure!(
            selection
                .expected_body_sha256
                .as_ref()
                .is_some_and(|expected| expected.eq_ignore_ascii_case(&hash)),
            LibraryError::conflict("Tag selection body differs from current binding evidence")
        );
    }
    Ok(())
}
fn approval_at(connection: &Connection, id: &str) -> Result<Option<TagWriteApproval>> {
    let row:Option<(String,i64,String,i64,i64)>=connection.query_row("SELECT asset_id,approval_epoch,document,revoked,byte_size FROM tag_write_approvals WHERE approval_id=?1",[id],|row|Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?,row.get(4)?))).optional()?;
    row.map(|(asset, epoch, document, revoked, bytes)| {
        let value: TagWriteApproval = decode_document(&document)?;
        ensure!(
            value.approval_id == id
                && value.selection.asset_id == asset
                && value.approval_epoch == epoch as u64
                && value.revoked == (revoked != 0)
                && document.len() as i64 == bytes,
            LibraryError::conflict("Tag approval sealed fields disagree")
        );
        validate_selection(&value.selection)?;
        Ok(value)
    })
    .transpose()
}
fn operation_at(connection: &Connection, id: &str) -> Result<Option<TagEditOperation>> {
    let row:Option<(String,String,i64,i64,String,String,i64)>=connection.query_row("SELECT approval_id,asset_id,operation_epoch,ledger_revision,phase,document,byte_size FROM tag_edit_operations WHERE operation_id=?1",[id],|row|Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?,row.get(4)?,row.get(5)?,row.get(6)?))).optional()?;
    let Some((approval, asset, epoch, revision, phase, document, bytes)) = row else {
        return Ok(None);
    };
    let mut value: TagEditOperation = decode_document(&document)?;
    ensure!(
        value.token.operation_id == id
            && value.token.approval_id == approval
            && value.token.selection.asset_id == asset
            && value.token.operation_epoch == epoch as u64
            && value.ledger_revision == revision as u64
            && value.phase.name() == phase
            && document.len() as i64 == bytes,
        LibraryError::conflict("Tag operation sealed fields disagree")
    );
    value.budget.validate()?;
    let mut statement=connection.prepare("SELECT role,phase,actual_bytes,reserved_bytes,document,byte_size FROM tag_edit_artifacts WHERE operation_id=?1 ORDER BY role")?;
    value.artifacts = statement
        .query_map([id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, i64>(5)?,
            ))
        })?
        .map(|row| {
            let (role, phase, actual, reserved, document, bytes) = row?;
            let item: TagEditArtifact = decode_document(&document)?;
            ensure!(
                item.operation_id == id
                    && item.operation_epoch == value.token.operation_epoch
                    && item.role == role
                    && item.phase.name() == phase
                    && item.actual_bytes == actual as u64
                    && document.len() as i64 == bytes
                    && actual >= 0
                    && reserved >= 0
                    && (if item.phase == TagEditArtifactPhase::Retired {
                        actual + reserved == 0
                    } else {
                        actual as u64 + reserved as u64 == item.maximum_bytes
                    }),
                LibraryError::conflict("Tag artifact sealed fields disagree")
            );
            let expected_maximum = match role.as_str() {
                "journal" => value.budget.journal_bytes,
                "backup" => value.budget.backup_bytes,
                "candidate" => value.budget.candidate_bytes,
                _ => anyhow::bail!(LibraryError::conflict("Unknown tag artifact role")),
            };
            ensure!(
                item.maximum_bytes == expected_maximum && item.actual_bytes <= item.maximum_bytes,
                LibraryError::conflict(
                    "Tag artifact reservation differs from its operation budget"
                )
            );
            if item.phase == TagEditArtifactPhase::Retired {
                ensure!(
                    item.retired_size_bytes
                        .is_some_and(|bytes| bytes <= item.maximum_bytes),
                    LibraryError::conflict("Tag retired object lost its actual byte witness")
                );
                if item.never_allocated {
                    ensure!(
                        item.identity.is_none()
                            && item.directory_identity.is_none()
                            && item.sha256.is_none()
                            && item.retired_size_bytes == Some(0),
                        LibraryError::conflict(
                            "Tag unallocated slot contains actual ownership claims"
                        )
                    );
                } else {
                    ensure!(
                        item.identity.is_some()
                            && item.directory_identity.is_some()
                            && item.sha256.is_some(),
                        LibraryError::conflict(
                            "Tag retired object lost its actual identity or SHA"
                        )
                    );
                }
            } else {
                ensure!(
                    !item.never_allocated && item.retired_size_bytes.is_none(),
                    LibraryError::conflict("Tag live object contains a false retired witness")
                );
            }
            Ok(item)
        })
        .collect::<Result<Vec<_>>>()?;
    ensure!(
        value.artifacts.len() == 3,
        LibraryError::conflict("Tag operation lost an artifact reservation")
    );
    let original_receipts = usize::from(value.original_close.is_some())
        + usize::from(value.original_never_opened)
        + usize::from(value.original_recovery.is_some());
    ensure!(
        value.original_owner_retired == (original_receipts == 1) && original_receipts <= 1,
        LibraryError::conflict("Tag original retirement sealed fields disagree")
    );
    ensure!(
        value.recovery_approval_history.len() <= MAX_TAG_RECOVERY_APPROVALS
            && serde_json::to_vec(&value.recovery_approval_history)?.len()
                <= MAX_RECOVERY_HISTORY_BYTES,
        LibraryError::conflict("Recovery history exceeds its reserved bound")
    );
    let mut history_ids = std::collections::HashSet::new();
    for approval in &value.recovery_approval_history {
        ensure!(
            approval.revoked
                && approval.purpose
                    == TagWritePurpose::Recovery {
                        operation_id: value.token.operation_id.clone(),
                        operation_epoch: value.token.operation_epoch
                    }
                && history_ids.insert(approval.approval_id.clone()),
            LibraryError::conflict("Recovery history contains authority or duplicate IDs")
        );
    }
    if let Some(intent) = &value.original_retirement_intent {
        validate_original_retirement_record(&value, intent)?;
    }
    if let Some(recovery) = &value.original_recovery {
        ensure!(
            recovery.proof == "same_object_sharing_calibration"
                && value.original_retirement_intent.as_ref() == Some(&recovery.intent)
                && value
                    .commit
                    .as_ref()
                    .or(value.restoration.as_ref())
                    .is_some_and(
                        |commit| commit.receipt.new_body_sha256 == recovery.body_sha256
                            && commit
                                .receipt
                                .installed
                                .as_ref()
                                .is_some_and(|facts| facts.after == recovery.intent.observation)
                    ),
            LibraryError::conflict("Original recovery receipt differs from sealed intent and body")
        );
    }
    let mut completed = value.clone();
    if value.installation_attempted == Some(false) {
        ensure!(
            value.phase == TagEditPhase::Registered
                && value.commit.is_none()
                && value.restoration.is_none()
                && value.artifacts.iter().all(|item| item.identity.is_none()
                    && item.directory_identity.is_none()
                    && (item.phase == TagEditArtifactPhase::Reserved
                        && item.allocation_attempted != Some(true)
                        || item.phase == TagEditArtifactPhase::Retired && item.never_allocated)),
            LibraryError::conflict("Unstarted tag admission contains actual file activity")
        );
    }
    completed.finished = false;
    complete_if_retired(&mut completed)?;
    ensure!(
        completed.finished == value.finished,
        LibraryError::conflict("Tag finished flag differs from its actual retirement receipts")
    );
    Ok(Some(value))
}
fn require_token(connection: &Connection, token: &TagEditToken) -> Result<TagEditOperation> {
    let value =
        operation_at(connection, &token.operation_id)?.context("Tag operation is absent")?;
    ensure!(
        digest(&value.token)? == digest(token)?,
        LibraryError::conflict("Tag operation epoch or input changed")
    );
    Ok(value)
}
fn require_approval(connection: &Connection, token: &TagEditToken) -> Result<()> {
    let value =
        approval_at(connection, &token.approval_id)?.context("Tag write approval is absent")?;
    ensure!(
        !value.revoked
            && value.purpose == TagWritePurpose::Install
            && value.approval_epoch == token.approval_epoch
            && same_selection(&value.selection, &token.selection)?,
        LibraryError::conflict("Tag write approval was revoked or replaced")
    );
    require_binding(connection, &token.selection)
}
fn require_recovery_approval(
    connection: &Connection,
    token: &TagEditToken,
    id: &str,
    epoch: u64,
) -> Result<TagWriteApproval> {
    let operation = require_token(connection, token)?;
    ensure!(
        !operation.finished,
        LibraryError::conflict("Tag operation is already fully retired")
    );
    let approval =
        approval_at(connection, id)?.context("Current tag recovery approval is absent")?;
    ensure!(
        !approval.revoked
            && approval.approval_epoch == epoch
            && approval.purpose
                == TagWritePurpose::Recovery {
                    operation_id: token.operation_id.clone(),
                    operation_epoch: token.operation_epoch
                }
            && approval.selection.operation_id == token.operation_id
            && approval.selection.asset_id == token.selection.asset_id
            && approval.selection.expected_identity == token.selection.expected_identity
            && normalize_local_path(&approval.selection.path)
                == normalize_local_path(&token.selection.path),
        LibraryError::conflict(
            "Recovery approval does not bind this actual original and operation epoch"
        )
    );
    require_binding_current(connection, &approval.selection, false)?;
    if let Some(committed) = operation.commit.as_ref().or(operation.restoration.as_ref()) {
        ensure!(
            committed.receipt.committed_binding_revision
                == Some(approval.selection.expected_binding_revision)
                && approval
                    .selection
                    .expected_body_sha256
                    .as_ref()
                    .is_some_and(
                        |hash| hash.eq_ignore_ascii_case(&committed.receipt.new_body_sha256)
                    ),
            LibraryError::conflict("Recovery approval differs from the committed or restored body")
        );
    } else {
        ensure!(
            approval.selection.expected_binding_revision
                == token.selection.expected_binding_revision,
            LibraryError::conflict("Tag recovery old binding changed")
        );
    }
    Ok(approval)
}
fn save_operation(tx: &Transaction<'_>, value: &TagEditOperation) -> Result<()> {
    let mut stored = value.clone();
    stored.artifacts.clear();
    let document = encode_document(&stored)?;
    ensure!(
        document
            .len()
            .checked_add(if !value.finished && value.recovery_budget_reserved {
                MAX_RECOVERY_HISTORY_DOCUMENT_BYTES.saturating_sub(recovery_history_bytes(value)?)
            } else {
                0
            })
            .is_some_and(|bytes| bytes <= 1024 * 1024),
        LibraryError::budget("Tag operation exceeds 1 MiB")
    );
    tx.execute("UPDATE tag_edit_operations SET ledger_revision=?2,phase=?3,document=?4,byte_size=?5 WHERE operation_id=?1",params![value.token.operation_id,value.ledger_revision as i64,value.phase.name(),document,document.len() as i64])?;
    Ok(())
}
fn save_artifact(tx: &Transaction<'_>, value: &TagEditArtifact) -> Result<()> {
    let document = encode_document(value)?;
    ensure!(
        document.len() <= 65536,
        LibraryError::budget("Tag artifact exceeds 64 KiB")
    );
    let retired = value.phase == TagEditArtifactPhase::Retired;
    tx.execute("UPDATE tag_edit_artifacts SET phase=?3,actual_bytes=?4,reserved_bytes=?5,document=?6,byte_size=?7 WHERE operation_id=?1 AND role=?2",params![value.operation_id,value.role,value.phase.name(),if retired{0}else{value.actual_bytes as i64},if retired{0}else{(value.maximum_bytes-value.actual_bytes) as i64},document,document.len() as i64])?;
    Ok(())
}
fn next_ledger(value: &mut TagEditOperation) -> Result<()> {
    value.ledger_revision = value
        .ledger_revision
        .checked_add(1)
        .filter(|n| *n <= i64::MAX as u64)
        .context("Tag ledger revision overflow")?;
    Ok(())
}
fn storage_budget(
    connection: &Connection,
    cache: &Mutex<FinishedValidationCache>,
) -> Result<TagEditQuota> {
    let (install,recovery,operations,bytes):(i64,i64,i64,i64)=connection.query_row("SELECT (SELECT count(*) FROM tag_write_approvals WHERE coalesce(json_extract(document,'$.purpose.kind'),'install')='install'),(SELECT count(*) FROM tag_write_approvals WHERE json_extract(document,'$.purpose.kind')='recovery'),(SELECT count(*) FROM tag_edit_operations),(SELECT coalesce(sum(byte_size),0) FROM tag_write_approvals)+(SELECT coalesce(sum(byte_size),0) FROM tag_edit_operations)+(SELECT coalesce(sum(byte_size),0) FROM tag_edit_artifacts)",[],|row|Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?)))?;
    let approvals: i64 =
        connection.query_row("SELECT count(*) FROM tag_write_approvals", [], |row| {
            row.get(0)
        })?;
    ensure!(
        install <= MAX_TAG_EDIT_OPERATIONS as i64
            && recovery <= MAX_TAG_RECOVERY_APPROVALS as i64
            && approvals == install + recovery
            && operations <= MAX_TAG_EDIT_OPERATIONS as i64
            && bytes <= MAX_TAG_EDIT_LEDGER_BYTES as i64,
        LibraryError::budget("Tag ledger budget exceeded")
    );
    let mut total = 0u64;
    let mut active = 0;
    let mut unused_documents = 0u64;
    let mut reserved_recovery_slots = 0usize;
    let mut statement = connection
        .prepare("SELECT operation_id FROM tag_edit_operations ORDER BY operation_epoch")?;
    for row in statement.query_map([], |row| row.get::<_, String>(0))? {
        let Some(value) = pending_operation_at(connection, &row?, cache)? else {
            continue;
        };
        unused_documents = unused_documents
            .checked_add(unused_document_reservation(connection, &value)?)
            .context("Tag document reservation overflow")?;
        let pending = !value.finished;
        if pending {
            active += 1;
            if value.recovery_budget_reserved
                && recovery_approvals(connection, &value.token)?.is_empty()
            {
                reserved_recovery_slots += 1;
            }
        }
        if !value.original_owner_retired {
            total = total
                .checked_add(value.budget.original_bytes)
                .context("Tag quota overflow")?;
        }
        for item in value.artifacts {
            if item.phase != TagEditArtifactPhase::Retired {
                total = total
                    .checked_add(item.maximum_bytes)
                    .context("Tag quota overflow")?;
            }
        }
    }
    ensure!(
        active <= MAX_ACTIVE_TAG_EDITS && total <= MAX_TAG_EDIT_OWNED_BYTES,
        LibraryError::budget("Tag actual and reserved ownership budget exceeded")
    );
    ensure!(
        recovery as usize + reserved_recovery_slots <= MAX_TAG_RECOVERY_APPROVALS,
        LibraryError::budget("Tag recovery approval slots are fully reserved")
    );
    ensure!(
        u64::try_from(bytes)?
            .checked_add(unused_documents)
            .is_some_and(|charged| charged <= MAX_TAG_EDIT_LEDGER_BYTES),
        LibraryError::budget("Tag actual and reserved document budget exceeds 16 MiB")
    );
    Ok(TagEditQuota {
        pending_operations: active,
        charged_bytes: total,
    })
}
fn unused_document_reservation(
    connection: &Connection,
    operation: &TagEditOperation,
) -> Result<u64> {
    if operation.finished || !operation.document_budget_reserved {
        return Ok(0);
    }
    let bytes: i64 = connection.query_row(
        "SELECT byte_size FROM tag_edit_operations WHERE operation_id=?1",
        [&operation.token.operation_id],
        |row| row.get(0),
    )?;
    let mut reserved = (1024 * 1024u64)
        .checked_sub(u64::try_from(bytes)?)
        .context("Tag operation exceeded its document reservation")?;
    for artifact in &operation.artifacts {
        if artifact.phase != TagEditArtifactPhase::Retired {
            let bytes: i64 = connection.query_row(
                "SELECT byte_size FROM tag_edit_artifacts WHERE operation_id=?1 AND role=?2",
                params![operation.token.operation_id, artifact.role],
                |row| row.get(0),
            )?;
            reserved = reserved
                .checked_add(
                    65536u64
                        .checked_sub(u64::try_from(bytes)?)
                        .context("Tag artifact exceeded its document reservation")?,
                )
                .context("Tag artifact document reservation overflow")?;
        }
    }
    if operation.recovery_budget_reserved {
        let approvals = recovery_approvals(connection, &operation.token)?;
        ensure!(
            approvals.len() <= 1,
            LibraryError::budget("Tag operation has more than one current recovery approval")
        );
        let bytes = approvals
            .iter()
            .try_fold(0u64, |sum, approval| -> Result<u64> {
                sum.checked_add(encode_document(approval)?.len() as u64)
                    .context("Recovery approval document overflow")
            })?;
        reserved = reserved
            .checked_add(
                65536u64
                    .checked_sub(bytes)
                    .context("Recovery approval exceeded its document reservation")?,
            )
            .context("Recovery approval document reservation overflow")?;
    }
    Ok(reserved)
}
fn document_budget_at(
    connection: &Connection,
    cache: &Mutex<FinishedValidationCache>,
) -> Result<TagDocumentBudget> {
    let actual: i64 = connection.query_row("SELECT (SELECT coalesce(sum(byte_size),0) FROM tag_write_approvals)+(SELECT coalesce(sum(byte_size),0) FROM tag_edit_operations)+(SELECT coalesce(sum(byte_size),0) FROM tag_edit_artifacts)", [], |row| row.get(0))?;
    let mut reserved = 0u64;
    let mut statement = connection
        .prepare("SELECT operation_id FROM tag_edit_operations ORDER BY operation_epoch")?;
    for row in statement.query_map([], |row| row.get::<_, String>(0))? {
        let id = row?;
        let Some(operation) = pending_operation_at(connection, &id, cache)? else {
            continue;
        };
        reserved = reserved
            .checked_add(unused_document_reservation(connection, &operation)?)
            .context("Tag document reservation overflow")?;
    }
    let actual = u64::try_from(actual)?;
    let charged = actual
        .checked_add(reserved)
        .context("Tag total document budget overflow")?;
    ensure!(
        charged <= MAX_TAG_EDIT_LEDGER_BYTES,
        LibraryError::budget("Tag actual and reserved document budget exceeds 16 MiB")
    );
    Ok(TagDocumentBudget {
        actual_bytes: actual,
        reserved_bytes: reserved,
        charged_bytes: charged,
        maximum_bytes: MAX_TAG_EDIT_LEDGER_BYTES,
    })
}

impl Repository {
    pub fn tag_document_budget(&self) -> Result<TagDocumentBudget> {
        let mut connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("Repository lock was poisoned"))?;
        schema::validate_current(&connection)?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Deferred)?;
        let value = document_budget_at(&transaction, &self.tag_finished_validation)?;
        transaction.commit()?;
        Ok(value)
    }
    pub fn reserve_tag_documents(
        &self,
        token: &TagEditToken,
        check: &dyn Fn() -> Result<()>,
    ) -> Result<()> {
        self.tag_work(check, |tx| {
            let mut operation = require_token(tx, token)?;
            if !adopt_document_reservations(&mut operation)? {
                return Ok(());
            }
            next_ledger(&mut operation)?;
            save_operation(tx, &operation)
        })
    }
    pub fn verify_tag_recovery_approval(
        &self,
        token: &TagEditToken,
        id: &str,
        epoch: u64,
    ) -> Result<TagWriteApproval> {
        validate_library_id(id)?;
        let connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("Repository lock was poisoned"))?;
        schema::validate_current(&connection)?;
        require_recovery_approval(&connection, token, id, epoch)
    }
    pub fn approve_tag_recovery(
        &self,
        id: &str,
        token: &TagEditToken,
        current_selection: &TagWriteSelection,
        check: &dyn Fn() -> Result<()>,
    ) -> Result<TagWriteApproval> {
        validate_library_id(id)?;
        validate_selection(current_selection)?;
        self.tag_work(check, |tx| {
            let mut operation = require_token(tx, token)?;
            ensure!(
                !operation.finished
                    && current_selection.operation_id == token.operation_id
                    && current_selection.asset_id == token.selection.asset_id
                    && current_selection.expected_identity == token.selection.expected_identity
                    && normalize_local_path(&current_selection.path)
                        == normalize_local_path(&token.selection.path),
                LibraryError::conflict("Recovery selection is not this retained actual original")
            );
            require_binding_current(tx, current_selection, false)?;
            let purpose = TagWritePurpose::Recovery {
                operation_id: token.operation_id.clone(),
                operation_epoch: token.operation_epoch,
            };
            if let Some(existing) = approval_at(tx, id)? {
                ensure!(
                    !existing.revoked
                        && existing.purpose == purpose
                        && same_selection(&existing.selection, current_selection)?,
                    LibraryError::conflict("Recovery approval ID was already used")
                );
                require_recovery_approval(tx, token, id, existing.approval_epoch)?;
                return Ok(existing);
            }
            ensure!(!historical_approval_exists(tx, id)?, LibraryError::conflict("Recovery approval ID was already archived"));
            let changed = adopt_document_reservations(&mut operation)?;
            let previous = recovery_approvals(tx, token)?;
            for existing in &previous {
                ensure!(existing.revoked || !same_selection(&existing.selection, current_selection)?,
                    LibraryError::conflict("A current explicit recovery approval already exists"));
            }
            let archived = !previous.is_empty();
            archive_recovery_approvals(tx, &mut operation, previous)?;
            let count: i64 =
                tx.query_row("SELECT count(*) FROM tag_write_approvals WHERE json_extract(document,'$.purpose.kind')='recovery'", [], |row| {
                    row.get(0)
                })?;
            ensure!(
                count < MAX_TAG_RECOVERY_APPROVALS as i64,
                LibraryError::budget("Tag recovery approval ledger is full")
            );
            let value = TagWriteApproval {
                approval_id: id.into(),
                approval_epoch: library::next_revision(tx)?,
                selection: current_selection.clone(),
                revoked: false,
                purpose,
            };
            let document = encode_document(&value)?;
            ensure!(
                document.len() <= 65536,
                LibraryError::budget("Recovery approval exceeds 64 KiB")
            );
            let mut future_history = operation.recovery_approval_history.clone();
            let mut future_archived = value.clone();
            future_archived.revoked = true;
            future_history.push(future_archived);
            ensure!(future_history.len() <= MAX_TAG_RECOVERY_APPROVALS
                && serde_json::to_vec(&future_history)?.len() <= MAX_RECOVERY_HISTORY_BYTES,
                LibraryError::budget("Recovery approval has no reserved archival space"));
            if changed || archived {
                next_ledger(&mut operation)?;
                save_operation(tx, &operation)?;
            }
            tx.execute(
                "INSERT INTO tag_write_approvals VALUES(?1,?2,?3,0,?4,?5)",
                params![
                    id,
                    current_selection.asset_id,
                    value.approval_epoch as i64,
                    document,
                    document.len() as i64
                ],
            )?;
            require_recovery_approval(tx, token, id, value.approval_epoch)?;
            commit_change(
                tx,
                library::current_revision(tx)? as i64,
                ChangeKind::TagWriteAuthorityChanged,
                None,
                serde_json::json!({"approval_id":id,"recovery_operation_id":token.operation_id}),
            )?;
            Ok(value)
        })
    }
    pub fn check_tag_persistence_budget(
        &self,
        token: &TagEditToken,
        budget: &TagPersistenceBudget<'_>,
        check: &dyn Fn() -> Result<()>,
    ) -> Result<()> {
        ensure!(
            same_selection(budget.selection(), &token.selection)?,
            LibraryError::conflict("Persistence admission belongs to another actual writer")
        );
        self.tag_read_checked(token, &|| { check()?; budget.check() }, |tx| {
            let operation = require_token(tx, token)?;
            require_approval(tx, token)?;
            let asset = library::asset_at(tx, &token.selection.asset_id)?.context("Metadata admission asset is absent")?;
            for receipt in budget.estimates() {
                let facts = receipt.installed.as_ref().context("Metadata admission estimate is absent")?;
                let original = TagOriginalCloseRecord { observation: facts.after.clone(),
                    installed_facts_sha256: Some("f".repeat(64)), never_modified: false };
                let change = RepositoryChange { revision: i64::MAX as u64,
                    kind: ChangeKind::LocalTagsRestored, track_id: Some(asset.track_id.clone()),
                    details: serde_json::json!({"operation_id": token.operation_id,
                        "asset_id": token.selection.asset_id, "binding_revision":i64::MAX,
                        "audio_payload_preserved":true,"original_body_restored":true}) };
                for restoration in [false, true] {
                    let mut estimate = operation.clone();
                    estimate.artifacts.clear();
                    estimate.ledger_revision = i64::MAX as u64;
                    estimate.phase = if restoration { TagEditPhase::Restored } else { TagEditPhase::Published };
                    estimate.commit = None; estimate.restoration = None;
                    let commit = TagEditCommit { receipt: receipt.clone(), change: change.clone() };
                    if restoration { estimate.restoration = Some(commit); } else { estimate.commit = Some(commit); }
                    // These mutually exclusive receipt variants are combined
                    // only to reserve a conservative encoding upper bound.
                    estimate.original_retirement_intent = Some(original.clone());
                    estimate.original_close = Some(original.clone());
                    estimate.original_recovery = Some(TagOriginalRecoveryRecord { intent: original.clone(),
                        body_sha256: "f".repeat(64), proof: "same_object_sharing_calibration".into() });
                    estimate.original_owner_retired = true;
                    estimate.finished = true;
                    ensure!(encode_document(&estimate)?.len().checked_add(MAX_RECOVERY_HISTORY_DOCUMENT_BYTES.saturating_sub(recovery_history_bytes(&operation)?))
                        .is_some_and(|bytes| bytes <= 1024 * 1024),
                        LibraryError::budget("Complete tag commit/rollback/retirement document exceeds 1 MiB before original mutation"));
                }
            }
            Ok(())
        })
    }
    pub fn prepare_tag_installation_open(
        &self,
        token: &TagEditToken,
        check: &dyn Fn() -> Result<()>,
    ) -> Result<()> {
        self.tag_work(check, |tx| {
            let mut operation = require_token(tx, token)?;
            require_approval(tx, token)?;
            ensure!(
                operation.phase == TagEditPhase::Registered
                    && operation.document_budget_reserved
                    && operation.recovery_budget_reserved
                    && !operation.original_owner_retired
                    && operation.installation_attempted == Some(false),
                LibraryError::conflict(
                    "Tag admission was already attempted or has uncertain ownership"
                )
            );
            operation.installation_attempted = Some(true);
            next_ledger(&mut operation)?;
            save_operation(tx, &operation)
        })
    }
    pub fn tag_recovery_unopened(&self, token: &TagEditToken) -> Result<bool> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("Repository lock was poisoned"))?;
        schema::validate_current(&connection)?;
        let operation = require_token(&connection, token)?;
        Ok(operation.phase == TagEditPhase::Registered
            && operation.installation_attempted == Some(false)
            && !operation.original_owner_retired
            && operation.commit.is_none()
            && operation.restoration.is_none()
            && operation.artifacts.iter().all(|item| {
                item.phase == TagEditArtifactPhase::Reserved
                    && item.allocation_attempted == Some(false)
                    && item.identity.is_none()
            }))
    }
    pub fn tag_retirement_recovery(
        &self,
        token: &TagEditToken,
    ) -> Result<Option<TagRetirementRecovery>> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("Repository lock was poisoned"))?;
        schema::validate_current(&connection)?;
        let operation = require_token(&connection, token)?;
        if operation.finished
            || !matches!(
                operation.phase,
                TagEditPhase::Published | TagEditPhase::Restored
            )
        {
            return Ok(None);
        }
        let receipt = operation
            .commit
            .as_ref()
            .or(operation.restoration.as_ref())
            .context("Tag retirement has no committed or restored receipt")?
            .receipt
            .clone();
        let (observation, original) = if operation.original_owner_retired {
            let record = operation
                .original_close
                .as_ref()
                .or_else(|| {
                    operation
                        .original_recovery
                        .as_ref()
                        .map(|value| &value.intent)
                })
                .context("Tag retirement has no actual original retirement receipt")?;
            (record.observation.clone(), None)
        } else {
            let intent = operation.original_retirement_intent.as_ref()
                .context("Cold original retirement has no durable pre-close intent; its charge must remain retained")?;
            validate_original_retirement_record(&operation, intent)?;
            (
                intent.observation.clone(),
                Some(TagOriginalRecoveryIntent {
                    observation: intent.observation.clone(),
                    facts: receipt
                        .installed
                        .clone()
                        .context("Original recovery committed facts are absent")?,
                }),
            )
        };
        ensure!(
            receipt
                .installed
                .as_ref()
                .is_some_and(|facts| facts.after == observation),
            LibraryError::conflict(
                "Tag retirement original close differs from its committed facts"
            )
        );
        let mut artifacts = Vec::with_capacity(3);
        for role in [
            TagInstallArtifactRole::Backup,
            TagInstallArtifactRole::Candidate,
            TagInstallArtifactRole::Journal,
        ] {
            let item = operation
                .artifacts
                .iter()
                .find(|item| item.role == role_name(role))
                .context("Tag retirement slot is absent")?;
            if item.phase == TagEditArtifactPhase::Retired {
                artifacts.push(TagRetirementRecoveryArtifact::Retired {
                    role,
                    path: item.path.clone(),
                });
            } else {
                ensure!(
                    matches!(
                        item.phase,
                        TagEditArtifactPhase::Sealed | TagEditArtifactPhase::Retiring
                    ),
                    LibraryError::conflict("Cold tag retirement requires a sealed actual object")
                );
                let sha256 = item
                    .sha256
                    .clone()
                    .context("Cold tag retirement SHA is absent")?;
                validate_content_sha256(&sha256)?;
                artifacts.push(TagRetirementRecoveryArtifact::Owned {
                    facts: TagInstallArtifactFacts {
                        role,
                        path: item.path.clone(),
                        identity: item
                            .identity
                            .clone()
                            .context("Cold tag retirement FileID is absent")?,
                        directory_identity: item
                            .directory_identity
                            .clone()
                            .context("Cold tag retirement directory FileID is absent")?,
                    },
                    size_bytes: item.actual_bytes,
                    sha256,
                    retiring: item.phase == TagEditArtifactPhase::Retiring,
                });
            }
        }
        Ok(Some(TagRetirementRecovery {
            phase: if operation.phase == TagEditPhase::Published {
                TagInstallPhase::Published
            } else {
                TagInstallPhase::Restored
            },
            receipt: Some(receipt),
            observation,
            artifacts,
            original,
        }))
    }
    pub fn prepare_tag_artifact_allocation(
        &self,
        token: &TagEditToken,
        role: TagInstallArtifactRole,
        check: &dyn Fn() -> Result<()>,
    ) -> Result<()> {
        self.tag_work(check, |tx| {
            let mut operation = require_token(tx, token)?;
            require_approval(tx, token)?;
            ensure!(
                !operation.original_owner_retired && !operation.finished,
                LibraryError::conflict("Tag original admission has already retired")
            );
            let artifact = operation
                .artifacts
                .iter_mut()
                .find(|item| item.role == role_name(role))
                .context("Tag allocation slot is absent")?;
            ensure!(
                artifact.phase == TagEditArtifactPhase::Reserved
                    && artifact.identity.is_none()
                    && artifact.allocation_attempted == Some(false),
                LibraryError::conflict(
                    "Tag allocation was already attempted or has uncertain ownership"
                )
            );
            artifact.allocation_attempted = Some(true);
            save_artifact(tx, artifact)?;
            next_ledger(&mut operation)?;
            save_operation(tx, &operation)
        })
    }
    pub fn tag_recovery_artifact(
        &self,
        token: &TagEditToken,
        role: TagInstallArtifactRole,
    ) -> Result<crate::library::tag_install::TagRecoveryArtifactState> {
        use crate::library::tag_install::TagRecoveryArtifactState as State;
        let connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("Repository lock was poisoned"))?;
        schema::validate_current(&connection)?;
        let operation = require_token(&connection, token)?;
        let artifact = operation
            .artifacts
            .iter()
            .find(|item| item.role == role_name(role))
            .context("Tag recovery artifact slot is absent")?;
        Ok(if artifact.phase == TagEditArtifactPhase::Retired {
            State::Retired
        } else if let Some(identity) = &artifact.identity {
            State::Allocated(identity.clone())
        } else if artifact.phase == TagEditArtifactPhase::Reserved
            && artifact.allocation_attempted == Some(false)
        {
            State::NeverAllocated
        } else {
            State::Uncertain
        })
    }
    pub fn tag_edit_quota(&self) -> Result<TagEditQuota> {
        let mut connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("Repository lock was poisoned"))?;
        schema::validate_current(&connection)?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Deferred)?;
        let value = storage_budget(&transaction, &self.tag_finished_validation)?;
        transaction.commit()?;
        Ok(value)
    }
    pub fn begin_tag_artifact_retirement(
        &self,
        token: &TagEditToken,
        seal: &TagArtifactSeal<'_>,
        check: &dyn Fn() -> Result<()>,
    ) -> Result<()> {
        let check_all = || {
            check()?;
            seal.check()
        };
        self.tag_work(&check_all, |tx| {
            let mut operation = require_token(tx, token)?;
            ensure!(
                operation.original_owner_retired,
                LibraryError::conflict(
                    "Tag artifact retirement must follow actual original retirement"
                )
            );
            let facts = seal.facts();
            if facts.role == TagInstallArtifactRole::Journal {
                ensure!(
                    operation
                        .artifacts
                        .iter()
                        .filter(|item| item.role != "journal")
                        .all(|item| item.phase == TagEditArtifactPhase::Retired),
                    LibraryError::conflict("Tag journal must retire after backup and candidate")
                );
            }
            let artifact = operation
                .artifacts
                .iter_mut()
                .find(|item| item.role == role_name(facts.role))
                .context("Tag artifact slot is absent")?;
            ensure!(
                matches!(
                    artifact.phase,
                    TagEditArtifactPhase::Allocated
                        | TagEditArtifactPhase::Sealed
                        | TagEditArtifactPhase::Retiring
                ) && artifact.identity.as_ref() == Some(&facts.identity)
                    && artifact.directory_identity.as_ref() == Some(&facts.directory_identity)
                    && normalize_local_path(&artifact.path) == normalize_local_path(&facts.path),
                LibraryError::conflict("Tag retirement seal identifies another actual object")
            );
            ensure!(
                seal.size_bytes() <= artifact.maximum_bytes,
                LibraryError::budget("Tag retirement size exceeded its reservation")
            );
            validate_content_sha256(seal.sha256())?;
            if artifact.phase == TagEditArtifactPhase::Retiring {
                ensure!(
                    artifact.actual_bytes == seal.size_bytes()
                        && artifact.sha256.as_deref() == Some(seal.sha256()),
                    LibraryError::conflict("Tag retirement preparation changed bytes or SHA")
                );
                return Ok(());
            }
            artifact.actual_bytes = seal.size_bytes();
            artifact.sha256 = Some(seal.sha256().into());
            artifact.phase = TagEditArtifactPhase::Retiring;
            save_artifact(tx, artifact)?;
            next_ledger(&mut operation)?;
            save_operation(tx, &operation)
        })
    }
    pub fn acknowledge_tag_artifact_retirement(
        &self,
        token: &TagEditToken,
        witness: &TagArtifactRetirementWitness,
        check: &dyn Fn() -> Result<()>,
    ) -> Result<()> {
        ensure!(
            same_selection(witness.selection(), &token.selection)?,
            LibraryError::conflict("Tag artifact witness belongs to another selection")
        );
        let facts = witness.facts();
        validate_content_sha256(witness.sha256())?;
        self.tag_work(check, |tx| {
            let mut operation = require_token(tx, token)?;
            ensure!(
                operation.original_owner_retired,
                LibraryError::conflict("Tag original owner is not retired")
            );
            if facts.role == TagInstallArtifactRole::Journal {
                ensure!(
                    operation
                        .artifacts
                        .iter()
                        .filter(|item| item.role != "journal")
                        .all(|item| item.phase == TagEditArtifactPhase::Retired),
                    LibraryError::conflict("Tag journal retired before other work files")
                );
            }
            let artifact = operation
                .artifacts
                .iter_mut()
                .find(|item| item.role == role_name(facts.role))
                .context("Tag artifact witness slot is absent")?;
            ensure!(
                artifact.identity.as_ref() == Some(&facts.identity)
                    && artifact.directory_identity.as_ref() == Some(&facts.directory_identity)
                    && normalize_local_path(&artifact.path) == normalize_local_path(&facts.path)
                    && artifact.sha256.as_deref() == Some(witness.sha256())
                    && !artifact.never_allocated,
                LibraryError::conflict(
                    "Tag artifact retirement witness changed FID, role, namespace or SHA"
                )
            );
            if artifact.phase == TagEditArtifactPhase::Retired {
                ensure!(
                    artifact.retired_size_bytes == Some(witness.size_bytes()),
                    LibraryError::conflict("Tag retirement replay changed its actual bytes")
                );
                return Ok(());
            }
            ensure!(
                artifact.phase == TagEditArtifactPhase::Retiring
                    && artifact.actual_bytes == witness.size_bytes(),
                LibraryError::conflict("Tag retirement was not prepared for these bytes")
            );
            artifact.retired_size_bytes = Some(witness.size_bytes());
            artifact.actual_bytes = 0;
            artifact.phase = TagEditArtifactPhase::Retired;
            save_artifact(tx, artifact)?;
            next_ledger(&mut operation)?;
            complete_if_retired(&mut operation)?;
            release_finished_recovery_approvals(tx, &mut operation)?;
            save_operation(tx, &operation)
        })
    }
    pub fn acknowledge_tag_unallocated_artifact(
        &self,
        token: &TagEditToken,
        witness: &TagUnallocatedArtifactWitness,
        check: &dyn Fn() -> Result<()>,
    ) -> Result<()> {
        ensure!(
            same_selection(witness.selection(), &token.selection)?,
            LibraryError::conflict("Tag unused-slot witness belongs to another selection")
        );
        self.tag_work(check, |tx| {
            let mut operation = require_token(tx, token)?;
            ensure!(
                operation.original_owner_retired,
                LibraryError::conflict("Unused tag slots require actual original retirement")
            );
            if witness.role() == TagInstallArtifactRole::Journal {
                ensure!(
                    operation
                        .artifacts
                        .iter()
                        .filter(|item| item.role != "journal")
                        .all(|item| item.phase == TagEditArtifactPhase::Retired),
                    LibraryError::conflict("Unused tag journal slot must retire last")
                );
            }
            let artifact = operation
                .artifacts
                .iter_mut()
                .find(|item| item.role == role_name(witness.role()))
                .context("Unused tag slot is absent")?;
            ensure!(
                normalize_local_path(&artifact.path) == normalize_local_path(witness.path())
                    && artifact.identity.is_none()
                    && artifact.directory_identity.is_none()
                    && artifact.actual_bytes == 0
                    && artifact.sha256.is_none(),
                LibraryError::conflict("Unused tag slot already has actual allocation evidence")
            );
            if artifact.phase == TagEditArtifactPhase::Retired {
                ensure!(
                    artifact.never_allocated && artifact.retired_size_bytes == Some(0),
                    LibraryError::conflict("Unused tag slot replay differs")
                );
                return Ok(());
            }
            ensure!(
                artifact.phase == TagEditArtifactPhase::Reserved,
                LibraryError::conflict(
                    "Tag slot allocation cannot be retired without an actual witness"
                )
            );
            artifact.phase = TagEditArtifactPhase::Retired;
            artifact.never_allocated = true;
            artifact.retired_size_bytes = Some(0);
            save_artifact(tx, artifact)?;
            next_ledger(&mut operation)?;
            complete_if_retired(&mut operation)?;
            release_finished_recovery_approvals(tx, &mut operation)?;
            save_operation(tx, &operation)
        })
    }
    pub fn begin_tag_original_retirement(
        &self,
        token: &TagEditToken,
        seal: &TagOriginalRetirementSeal<'_>,
        check: &dyn Fn() -> Result<()>,
    ) -> Result<()> {
        ensure!(
            same_selection(seal.selection(), &token.selection)?,
            LibraryError::conflict("Original retirement seal belongs to another selection")
        );
        let record = TagOriginalCloseRecord {
            observation: seal.observation().clone(),
            installed_facts_sha256: seal.facts().map(digest).transpose()?,
            never_modified: seal.never_modified(),
        };
        self.tag_work(
            &|| {
                check()?;
                seal.check()
            },
            |tx| {
                let mut operation = require_token(tx, token)?;
                validate_original_retirement_record(&operation, &record)?;
                if let Some(previous) = &operation.original_retirement_intent {
                    ensure!(
                        *previous == record,
                        LibraryError::conflict("Original retirement intent replay differs")
                    );
                    return Ok(());
                }
                ensure!(
                    !operation.original_owner_retired,
                    LibraryError::conflict("Original writer is already retired")
                );
                operation.original_retirement_intent = Some(record);
                next_ledger(&mut operation)?;
                save_operation(tx, &operation)
            },
        )
    }
    pub fn acknowledge_tag_original_recovery(
        &self,
        token: &TagEditToken,
        witness: &TagOriginalRetirementRecoveryWitness,
        check: &dyn Fn() -> Result<()>,
    ) -> Result<()> {
        ensure!(
            same_selection(witness.selection(), &token.selection)?,
            LibraryError::conflict("Original recovery proof belongs to another selection")
        );
        let intent = witness.intent();
        let record = TagOriginalRecoveryRecord {
            intent: TagOriginalCloseRecord {
                observation: intent.observation.clone(),
                installed_facts_sha256: Some(digest(&intent.facts)?),
                never_modified: false,
            },
            body_sha256: witness.body_sha256().into(),
            proof: "same_object_sharing_calibration".into(),
        };
        self.tag_work(
            &|| {
                check()?;
                witness.check()
            },
            |tx| {
                let mut operation = require_token(tx, token)?;
                validate_original_retirement_record(&operation, &record.intent)?;
                ensure!(
                    operation.original_retirement_intent.as_ref() == Some(&record.intent),
                    LibraryError::conflict(
                        "Cold original recovery requires the exact durable pre-close intent"
                    )
                );
                if operation.original_owner_retired {
                    ensure!(
                        operation.original_recovery.as_ref() == Some(&record)
                            && operation.original_close.is_none()
                            && !operation.original_never_opened,
                        LibraryError::conflict("Original recovery acknowledgement replay differs")
                    );
                    return Ok(());
                }
                ensure!(
                    operation
                        .commit
                        .as_ref()
                        .or(operation.restoration.as_ref())
                        .is_some_and(|commit| commit.receipt.new_body_sha256 == record.body_sha256),
                    LibraryError::conflict("Original recovery body differs from committed content")
                );
                operation.original_owner_retired = true;
                operation.original_recovery = Some(record);
                next_ledger(&mut operation)?;
                complete_if_retired(&mut operation)?;
                release_finished_recovery_approvals(tx, &mut operation)?;
                save_operation(tx, &operation)
            },
        )
    }
    pub fn acknowledge_tag_original_close(
        &self,
        token: &TagEditToken,
        witness: &TagOriginalCloseWitness,
        check: &dyn Fn() -> Result<()>,
    ) -> Result<()> {
        ensure!(
            same_selection(witness.selection(), &token.selection)?,
            LibraryError::conflict("Tag original close witness belongs to another selection")
        );
        let observation = witness.observation();
        ensure!(
            observation.identity == token.selection.expected_identity
                && normalize_local_path(&observation.path)
                    == normalize_local_path(&token.selection.path),
            LibraryError::conflict("Tag original close witness changed actual object")
        );
        let record = TagOriginalCloseRecord {
            observation: observation.clone(),
            installed_facts_sha256: witness.facts().map(digest).transpose()?,
            never_modified: witness.never_modified(),
        };
        self.tag_work(check, |tx| {
            let mut operation = require_token(tx, token)?;
            if operation.original_owner_retired {
                ensure!(
                    operation.original_close.as_ref() == Some(&record)
                        && !operation.original_never_opened,
                    LibraryError::conflict("Tag original close replay differs")
                );
                return Ok(());
            }
            if let Some(intent) = &operation.original_retirement_intent {
                ensure!(
                    *intent == record,
                    LibraryError::conflict("Original close differs from durable pre-close intent")
                );
            }
            if let Some(expected) = operation.commit.as_ref().or(operation.restoration.as_ref()) {
                ensure!(
                    matches!(
                        operation.phase,
                        TagEditPhase::Published | TagEditPhase::Restored
                    ) && witness.facts() == expected.receipt.installed.as_ref()
                        && witness
                            .facts()
                            .is_some_and(|facts| facts.after == *observation),
                    LibraryError::conflict(
                        "Tag original close lacks the exact committed or restored facts"
                    )
                );
            } else {
                ensure!(
                    witness.never_modified()
                        && matches!(
                            operation.phase,
                            TagEditPhase::Registered
                                | TagEditPhase::Prepared
                                | TagEditPhase::BackupSealed
                                | TagEditPhase::CandidateVerified
                        ),
                    LibraryError::conflict(
                        "A mutated tag original cannot retire without verified rollback"
                    )
                );
            }
            operation.original_owner_retired = true;
            operation.original_close = Some(record);
            next_ledger(&mut operation)?;
            complete_if_retired(&mut operation)?;
            release_finished_recovery_approvals(tx, &mut operation)?;
            save_operation(tx, &operation)
        })
    }
    pub fn acknowledge_tag_installation_unopened(
        &self,
        token: &TagEditToken,
        witness: &TagUnopenedInstallationWitness,
        check: &dyn Fn() -> Result<()>,
    ) -> Result<()> {
        ensure!(
            same_selection(witness.selection(), &token.selection)?,
            LibraryError::conflict(
                "Unopened tag installation witness belongs to another selection"
            )
        );
        self.tag_work(check, |tx| {
            let mut operation = require_token(tx, token)?;
            if operation.original_owner_retired {
                ensure!(
                    operation.original_never_opened && operation.original_close.is_none(),
                    LibraryError::conflict(
                        "Tag installation already has a different original retirement"
                    )
                );
                return Ok(());
            }
            ensure!(
                operation.phase == TagEditPhase::Registered
                    && operation.commit.is_none()
                    && operation.restoration.is_none()
                    && operation
                        .artifacts
                        .iter()
                        .all(|item| item.phase == TagEditArtifactPhase::Reserved
                            && item.identity.is_none()),
                LibraryError::conflict(
                    "Unopened tag installation already has actual file activity"
                )
            );
            ensure!(
                operation.installation_attempted == Some(witness.admission_attempted()),
                LibraryError::conflict(
                    "Unopened tag witness differs from durable admission attempt"
                )
            );
            operation.original_owner_retired = true;
            operation.original_never_opened = true;
            next_ledger(&mut operation)?;
            save_operation(tx, &operation)
        })
    }
    fn check_tag_restore_binding(&self, token: &TagEditToken) -> Result<()> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("Repository lock was poisoned"))?;
        schema::validate_current(&connection)?;
        let operation = require_token(&connection, token)?;
        ensure!(
            operation.commit.is_none(),
            LibraryError::conflict("Committed tags cannot restore the old body")
        );
        let binding = library_binding::binding_at(&connection, &token.selection.asset_id)?
            .context("Tag restore binding is absent")?;
        let expected = operation
            .restoration
            .as_ref()
            .and_then(|value| value.receipt.committed_binding_revision)
            .unwrap_or(token.selection.expected_binding_revision);
        ensure!(
            binding.binding_revision == expected
                && binding.file_identity.as_ref() == Some(&token.selection.expected_identity)
                && normalize_local_path(&binding.path)
                    == normalize_local_path(&token.selection.path),
            LibraryError::conflict("Tag rollback binding changed")
        );
        Ok(())
    }
    pub fn commit_tag_restoration(
        &self,
        token: &TagEditToken,
        prepared: &TagPreparedAsset<'_>,
        check: &dyn Fn() -> Result<()>,
    ) -> Result<TagEditCommit> {
        let check_all = || {
            check()?;
            prepared.check()
        };
        self.tag_work(&check_all,|tx| {
            let mut operation=require_token(tx,token)?;
            ensure!(operation.commit.is_none(),LibraryError::conflict("Committed tags cannot restore their original body"));
            let facts=prepared.facts();
            ensure!(facts.before.identity==token.selection.expected_identity && facts.after.identity==token.selection.expected_identity && normalize_local_path(&facts.after.path)==normalize_local_path(&token.selection.path) && token.selection.expected_body_sha256.as_ref().is_some_and(|hash|hash.eq_ignore_ascii_case(&facts.old_body_sha256) && hash.eq_ignore_ascii_case(&facts.new_body_sha256)),LibraryError::conflict("Tag rollback has no actual original body proof"));
            if let Some(previous)=operation.restoration.as_ref() {ensure!(previous.receipt.installed.as_ref()==Some(facts),LibraryError::conflict("Tag rollback replay changed actual file metadata"));return Ok(previous.clone());}
            ensure!(operation.phase==TagEditPhase::Restoring,LibraryError::conflict("Tag rollback phase is missing"));
            let binding=library_binding::binding_at(tx,&token.selection.asset_id)?.context("Tag rollback binding is absent")?;
            ensure!(binding.binding_revision==token.selection.expected_binding_revision && binding.file_identity.as_ref()==Some(&token.selection.expected_identity) && normalize_local_path(&binding.path)==normalize_local_path(&token.selection.path),LibraryError::conflict("Tag rollback binding changed"));
            let mut asset=library::asset_at(tx,&token.selection.asset_id)?.context("Tag rollback asset is absent")?;
            ensure!(asset.size_bytes==facts.before.size_bytes && asset.modified_ms==facts.before.modified_ms && facts.after.size_bytes==facts.before.size_bytes,LibraryError::conflict("Tag rollback original representation changed"));
            asset.modified_ms=facts.after.modified_ms;asset.fingerprint=Some(facts.new_body_sha256.clone());
            let binding_revision=token.selection.expected_binding_revision.checked_add(1).filter(|value|*value<=i64::MAX as u64).context("Tag rollback binding revision overflow")?;
            tracks::write_asset(tx,&asset,&encode_document(&asset)?)?;
            let location=library::AssetLocation{path:asset.path.clone(),file_identity:Some(token.selection.expected_identity.clone()),content_sha256:Some(facts.new_body_sha256.clone()),hash_size_bytes:Some(asset.size_bytes),hash_modified_ms:asset.modified_ms,size_bytes:Some(asset.size_bytes),modified_ms:asset.modified_ms};
            ensure!(tx.execute("UPDATE local_asset_locations SET binding_revision=?2,document=?3 WHERE asset_id=?1 AND binding_revision=?4",params![asset.id,binding_revision as i64,encode_document(&location)?,token.selection.expected_binding_revision as i64])?==1,LibraryError::conflict("Tag rollback metadata CAS failed"));
            let change=commit_change(tx,library::current_revision(tx)? as i64,ChangeKind::LocalTagsRestored,Some(asset.track_id),serde_json::json!({"operation_id":token.operation_id,"asset_id":asset.id,"binding_revision":binding_revision,"original_body_restored":true}))?;
            let receipt=TagInstallationReceipt{operation_id:token.operation_id.clone(),asset_id:token.selection.asset_id.clone(),path:token.selection.path.clone(),file_identity:token.selection.expected_identity.clone(),old_body_sha256:facts.old_body_sha256.clone(),new_body_sha256:facts.new_body_sha256.clone(),audio_sha256:facts.audio_sha256.clone(),size_bytes:facts.after.size_bytes,committed_binding_revision:Some(binding_revision),installed:Some(facts.clone())};
            let restored=TagEditCommit{receipt,change};operation.restoration=Some(restored.clone());next_ledger(&mut operation)?;save_operation(tx,&operation)?;Ok(restored)
        })
    }
    pub fn verify_tag_edit_commit(
        &self,
        token: &TagEditToken,
        receipt: &TagInstallationReceipt,
    ) -> Result<()> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("Repository lock was poisoned"))?;
        schema::validate_current(&connection)?;
        let operation = require_token(&connection, token)?;
        let committed = operation
            .commit
            .context("Tag commit receipt is absent")?
            .receipt;
        ensure!(
            receipt.operation_id == token.operation_id
                && receipt.asset_id == token.selection.asset_id
                && receipt.file_identity == committed.file_identity
                && normalize_local_path(&receipt.path) == normalize_local_path(&committed.path)
                && receipt.new_body_sha256 == committed.new_body_sha256
                && receipt.old_body_sha256 == committed.old_body_sha256
                && receipt.audio_sha256 == committed.audio_sha256
                && receipt.size_bytes == committed.size_bytes
                && receipt
                    .committed_binding_revision
                    .is_none_or(|revision| Some(revision) == committed.committed_binding_revision),
            LibraryError::conflict(
                "Tag recovery receipt differs from the actual committed operation"
            )
        );
        let binding = library_binding::binding_at(&connection, &token.selection.asset_id)?
            .context("Committed tag binding is absent")?;
        ensure!(
            Some(binding.binding_revision) == committed.committed_binding_revision
                && binding.file_identity.as_ref() == Some(&committed.file_identity)
                && binding.content_sha256.as_ref() == Some(&committed.new_body_sha256)
                && binding.hash_size_bytes == Some(committed.size_bytes),
            LibraryError::conflict("Committed tag binding or body changed")
        );
        Ok(())
    }
    /// This explicit mutation records independent write consent. Scanning and
    /// read-grant acquisition never call it or construct its approval row.
    pub fn approve_tag_write(
        &self,
        approval_id: &str,
        selection: &TagWriteSelection,
        check: &dyn Fn() -> Result<()>,
    ) -> Result<TagWriteApproval> {
        validate_library_id(approval_id)?;
        validate_selection(selection)?;
        check()?;
        self.tag_work(check, |tx| {
            if let Some(existing) = approval_at(tx, approval_id)? {
                ensure!(
                    !existing.revoked
                        && existing.purpose == TagWritePurpose::Install
                        && same_selection(&existing.selection, selection)?,
                    LibraryError::conflict("Tag approval ID was already used")
                );
                return Ok(existing);
            }
            require_binding(tx, selection)?;
            ensure!(!historical_approval_exists(tx, approval_id)?, LibraryError::conflict("Tag approval ID was already archived"));
            let count: i64 =
                tx.query_row("SELECT count(*) FROM tag_write_approvals WHERE coalesce(json_extract(document,'$.purpose.kind'),'install')='install'", [], |row| {
                    row.get(0)
                })?;
            ensure!(
                count < MAX_TAG_EDIT_OPERATIONS as i64,
                LibraryError::budget("Tag approval ledger is full")
            );
            let value = TagWriteApproval {
                approval_id: approval_id.into(),
                approval_epoch: library::next_revision(tx)?,
                selection: selection.clone(),
                revoked: false,
                purpose: TagWritePurpose::Install,
            };
            let document = encode_document(&value)?;
            ensure!(
                document.len() <= 65536,
                LibraryError::budget("Tag approval exceeds 64 KiB")
            );
            tx.execute(
                "INSERT INTO tag_write_approvals VALUES(?1,?2,?3,0,?4,?5)",
                params![
                    approval_id,
                    selection.asset_id,
                    value.approval_epoch as i64,
                    document,
                    document.len() as i64
                ],
            )?;
            storage_budget(tx, &self.tag_finished_validation)?;
            commit_change(
                tx,
                library::current_revision(tx)? as i64,
                ChangeKind::TagWriteAuthorityChanged,
                None,
                serde_json::json!({"approval_id":approval_id,"write_consent":true}),
            )?;
            Ok(value)
        })
    }
    pub fn tag_write_approval(&self, id: &str) -> Result<Option<TagWriteApproval>> {
        validate_library_id(id)?;
        let connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("Repository lock was poisoned"))?;
        schema::validate_current(&connection)?;
        approval_at(&connection, id)
    }
    pub fn revoke_tag_write(
        &self,
        id: &str,
        expected_epoch: u64,
        check: &dyn Fn() -> Result<()>,
    ) -> Result<RepositoryChange> {
        validate_library_id(id)?;
        let (_,change)=self.write_checked(None,|tx| {let mut value=approval_at(tx,id)?.context("Tag approval is absent")?;ensure!(value.approval_epoch==expected_epoch,LibraryError::conflict("Tag approval epoch changed"));value.revoked=true;let document=encode_document(&value)?;tx.execute("UPDATE tag_write_approvals SET revoked=1,document=?2,byte_size=?3 WHERE approval_id=?1",params![id,document,document.len() as i64])?;Ok(((),ChangeKind::TagWriteAuthorityChanged,Some(value.selection.asset_id),serde_json::json!({"approval_id":id,"revoked":true})))},check)?;
        Ok(change)
    }
    pub fn begin_tag_edit(
        &self,
        approval_id: &str,
        expected_approval_epoch: u64,
        budget: TagEditBudget,
        input_digest: &str,
        check: &dyn Fn() -> Result<()>,
    ) -> Result<TagEditOperation> {
        validate_library_id(approval_id)?;
        budget.validate()?;
        validate_content_sha256(input_digest)?;
        check()?;
        self.tag_work(check,|tx| {
            let approval=approval_at(tx,approval_id)?.context("Explicit tag write approval is absent")?;
            ensure!(approval.purpose==TagWritePurpose::Install,LibraryError::conflict("Recovery approval cannot start a new tag edit"));
            ensure!(approval.approval_epoch==expected_approval_epoch,LibraryError::conflict("Tag approval epoch changed"));
            let request_digest=digest(&(&approval.approval_id,approval.approval_epoch,&approval.selection,budget,input_digest))?;
            if let Some(existing)=operation_at(tx,&approval.selection.operation_id)? {ensure!(existing.token.request_digest==request_digest,LibraryError::conflict("Tag operation ID was already used"));return Ok(existing);}
            ensure!(!approval.revoked,LibraryError::conflict("Tag write approval was revoked"));
            require_binding(tx,&approval.selection)?;
            let old=library::asset_at(tx,&approval.selection.asset_id)?.context("Tag asset disappeared")?;ensure!(budget.backup_bytes>=old.size_bytes && budget.candidate_bytes>=old.size_bytes,LibraryError::budget("Tag reservation cannot hold the original file"));
            let conflict:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM tag_edit_operations WHERE asset_id=?1 AND (json_extract(document,'$.original_owner_retired')=0 OR EXISTS(SELECT 1 FROM tag_edit_artifacts a WHERE a.operation_id=tag_edit_operations.operation_id AND a.phase!='retired')))",[&old.id],|row|row.get(0))?;ensure!(!conflict,LibraryError::conflict("The asset already has a retained tag writer"));
            let count:i64=tx.query_row("SELECT count(*) FROM tag_edit_operations",[],|row|row.get(0))?;ensure!(count<MAX_TAG_EDIT_OPERATIONS as i64,LibraryError::budget("Tag operation ledger is full"));
            let token=TagEditToken{operation_id:approval.selection.operation_id.clone(),operation_epoch:library::next_revision(tx)?,approval_id:approval.approval_id,approval_epoch:approval.approval_epoch,request_digest,input_digest:input_digest.into(),selection:approval.selection};
            let mut value=TagEditOperation{token,budget,ledger_revision:1,phase:TagEditPhase::Registered,document_budget_reserved:true,recovery_budget_reserved:true,recovery_approval_history:Vec::new(),commit:None,restoration:None,original_owner_retired:false,installation_attempted:Some(false),original_close:None,original_retirement_intent:None,original_recovery:None,original_never_opened:false,finished:false,artifacts:Vec::new()};
            let document=encode_document(&value)?;tx.execute("INSERT INTO tag_edit_operations VALUES(?1,?2,?3,?4,1,'registered',?5,?6)",params![value.token.operation_id,value.token.approval_id,old.id,value.token.operation_epoch as i64,document,document.len() as i64])?;
            let directory=self.path.parent().context("Repository has no profile directory")?.join("tag-edits");
            for (role,extension) in [(TagInstallArtifactRole::Journal,"journal"),(TagInstallArtifactRole::Backup,"backup"),(TagInstallArtifactRole::Candidate,"edit")] {
                let item=TagEditArtifact{operation_id:value.token.operation_id.clone(),operation_epoch:value.token.operation_epoch,role:role_name(role).into(),path:directory.join(format!("tag-install-{:x}.{extension}",Sha256::digest(value.token.operation_id.as_bytes()))),phase:TagEditArtifactPhase::Reserved,maximum_bytes:budget.artifact_limit(role),actual_bytes:0,identity:None,directory_identity:None,sha256:None,retired_size_bytes:None,never_allocated:false,allocation_attempted:Some(false)};
                let document=encode_document(&item)?;tx.execute("INSERT INTO tag_edit_artifacts VALUES(?1,?2,'reserved',0,?3,?4,?5)",params![item.operation_id,item.role,item.maximum_bytes as i64,document,document.len() as i64])?;value.artifacts.push(item);
            }
            storage_budget(tx, &self.tag_finished_validation)?;
            commit_change(tx,library::current_revision(tx)? as i64,ChangeKind::TagEditOperationChanged,Some(old.track_id),serde_json::json!({"operation_id":value.token.operation_id,"reserved_before_allocation":true}))?;
            Ok(value)
        })
    }
    pub fn tag_edit_operation(&self, id: &str) -> Result<Option<TagEditOperation>> {
        validate_library_id(id)?;
        let connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("Repository lock was poisoned"))?;
        schema::validate_current(&connection)?;
        operation_at(&connection, id)
    }
    pub fn pending_tag_edits(&self) -> Result<Vec<TagEditOperation>> {
        let mut connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("Repository lock was poisoned"))?;
        schema::validate_current(&connection)?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Deferred)?;
        storage_budget(&transaction, &self.tag_finished_validation)?;
        let pending = {
            let mut statement = transaction
                .prepare("SELECT operation_id FROM tag_edit_operations ORDER BY operation_epoch")?;
            let mut pending = Vec::new();
            for row in statement.query_map([], |row| row.get::<_, String>(0))? {
                if let Some(operation) =
                    pending_operation_at(&transaction, &row?, &self.tag_finished_validation)?
                {
                    pending.push(operation);
                }
            }
            pending
        };
        transaction.commit()?;
        Ok(pending)
    }
    pub fn check_tag_edit_intent(&self, token: &TagEditToken) -> Result<()> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("Repository lock was poisoned"))?;
        schema::validate_current(&connection)?;
        let operation = require_token(&connection, token)?;
        ensure!(
            operation.commit.is_none() && !operation.original_owner_retired && !operation.finished,
            LibraryError::conflict("Tag installation already committed")
        );
        require_approval(&connection, token)
    }
    fn tag_work<T>(
        &self,
        check: &dyn Fn() -> Result<()>,
        operation: impl FnOnce(&Transaction<'_>) -> Result<T>,
    ) -> Result<T> {
        check()?;
        self.profile_guard.verify()?;
        let mut connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("Repository lock was poisoned"))?;
        schema::validate_current(&connection)?;
        check()?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let value = operation(&tx)?;
        storage_budget(&tx, &self.tag_finished_validation)?;
        self.profile_guard.verify()?;
        check()?;
        tx.commit()?;
        Ok(value)
    }
    fn tag_read_checked<T>(
        &self,
        token: &TagEditToken,
        check: &dyn Fn() -> Result<()>,
        read: impl FnOnce(&Connection) -> Result<T>,
    ) -> Result<T> {
        check()?;
        self.profile_guard.verify()?;
        let mut connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("Repository lock was poisoned"))?;
        schema::validate_current(&connection)?;
        check()?;
        // Keep the original SQL admission fence while reading: this transaction
        // performs no updates, and a concurrent raw writer cannot alter the
        // current token/grant between the live-proof checks.
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        require_token(&transaction, token)?;
        require_approval(&transaction, token)?;
        let value = read(&transaction)?;
        self.profile_guard.verify()?;
        check()?;
        require_token(&transaction, token)?;
        require_approval(&transaction, token)?;
        transaction.commit()?;
        Ok(value)
    }
    pub fn acknowledge_tag_allocation(
        &self,
        token: &TagEditToken,
        allocation: &TagArtifactAllocation<'_>,
        check: &dyn Fn() -> Result<()>,
    ) -> Result<()> {
        let check_all = || {
            check()?;
            allocation.check_empty()
        };
        self.acknowledge_tag_artifact(token, allocation.facts(), &check_all)
    }
    pub fn seal_tag_artifact(
        &self,
        token: &TagEditToken,
        seal: &TagArtifactSeal<'_>,
        check: &dyn Fn() -> Result<()>,
    ) -> Result<()> {
        let check_all = || {
            check()?;
            seal.check()
        };
        self.tag_work(&check_all, |tx| {
            let mut operation = require_token(tx, token)?;
            let facts = seal.facts();
            let artifact = operation
                .artifacts
                .iter_mut()
                .find(|item| item.role == role_name(facts.role))
                .context("Tag seal reservation is absent")?;
            ensure!(
                matches!(
                    artifact.phase,
                    TagEditArtifactPhase::Allocated | TagEditArtifactPhase::Sealed
                ) && artifact.identity.as_ref() == Some(&facts.identity)
                    && artifact.directory_identity.as_ref() == Some(&facts.directory_identity)
                    && normalize_local_path(&artifact.path) == normalize_local_path(&facts.path),
                LibraryError::conflict("Tag seal actual owner differs from allocation")
            );
            ensure!(
                seal.size_bytes() <= artifact.maximum_bytes,
                LibraryError::budget("Tag seal exceeded its admitted reservation")
            );
            validate_content_sha256(seal.sha256())?;
            artifact.actual_bytes = seal.size_bytes();
            artifact.sha256 = Some(seal.sha256().into());
            artifact.phase = TagEditArtifactPhase::Sealed;
            save_artifact(tx, artifact)?;
            next_ledger(&mut operation)?;
            save_operation(tx, &operation)
        })
    }
    fn acknowledge_tag_artifact(
        &self,
        token: &TagEditToken,
        facts: &TagInstallArtifactFacts,
        check: &dyn Fn() -> Result<()>,
    ) -> Result<()> {
        validate_library_path(&facts.path)?;
        facts.identity.validate()?;
        facts.directory_identity.validate()?;
        self.tag_work(check, |tx| {
            let mut operation = require_token(tx, token)?;
            require_approval(tx, token)?;
            let artifact = operation
                .artifacts
                .iter_mut()
                .find(|a| a.role == role_name(facts.role))
                .context("Tag artifact slot is absent")?;
            ensure!(
                normalize_local_path(&artifact.path) == normalize_local_path(&facts.path),
                LibraryError::conflict("Tag artifact was allocated outside its reserved namespace")
            );
            if artifact.phase != TagEditArtifactPhase::Reserved {
                ensure!(
                    artifact.identity.as_ref() == Some(&facts.identity)
                        && artifact.directory_identity.as_ref() == Some(&facts.directory_identity)
                        && artifact.phase != TagEditArtifactPhase::Retired,
                    LibraryError::conflict("Tag artifact actual object changed")
                );
                return Ok(());
            }
            artifact.identity = Some(facts.identity.clone());
            artifact.directory_identity = Some(facts.directory_identity.clone());
            artifact.phase = TagEditArtifactPhase::Allocated;
            save_artifact(tx, artifact)?;
            next_ledger(&mut operation)?;
            save_operation(tx, &operation)
        })
    }
    pub fn checkpoint_tag_edit(
        &self,
        token: &TagEditToken,
        expected_ledger_revision: u64,
        phase: TagEditPhase,
        check: &dyn Fn() -> Result<()>,
    ) -> Result<u64> {
        self.tag_work(check, |tx| {
            let mut value = require_token(tx, token)?;
            ensure!(
                value.ledger_revision == expected_ledger_revision,
                LibraryError::conflict("Tag ledger CAS changed")
            );
            if value.phase == phase {
                return Ok(value.ledger_revision);
            };
            let allowed = matches!(
                (value.phase, phase),
                (TagEditPhase::Registered, TagEditPhase::Prepared)
                    | (TagEditPhase::Prepared, TagEditPhase::BackupSealed)
                    | (TagEditPhase::BackupSealed, TagEditPhase::CandidateVerified)
                    | (TagEditPhase::CandidateVerified, TagEditPhase::Installing)
                    | (TagEditPhase::Installing, TagEditPhase::DiskVerified)
                    | (TagEditPhase::RepoCommitted, TagEditPhase::Published)
                    | (
                        TagEditPhase::Registered
                            | TagEditPhase::Prepared
                            | TagEditPhase::BackupSealed
                            | TagEditPhase::CandidateVerified
                            | TagEditPhase::Installing
                            | TagEditPhase::DiskVerified,
                        TagEditPhase::Restoring
                    )
                    | (TagEditPhase::Restoring, TagEditPhase::Restored)
            );
            ensure!(
                allowed,
                LibraryError::conflict("Invalid tag phase transition")
            );
            if !matches!(
                phase,
                TagEditPhase::Restoring | TagEditPhase::Restored | TagEditPhase::Published
            ) {
                require_approval(tx, token)?;
            }
            if matches!(phase, TagEditPhase::Restoring | TagEditPhase::Restored) {
                ensure!(
                    value.commit.is_none(),
                    LibraryError::conflict("A committed tag edit cannot restore its old body")
                );
            }
            value.phase = phase;
            next_ledger(&mut value)?;
            save_operation(tx, &value)?;
            Ok(value.ledger_revision)
        })
    }
    pub fn commit_tag_edit(
        &self,
        token: &TagEditToken,
        prepared: &TagPreparedAsset<'_>,
        check: &dyn Fn() -> Result<()>,
    ) -> Result<TagEditCommit> {
        let check_all = || {
            check()?;
            prepared.check()
        };
        check_all()?;
        self.tag_work(&check_all,|tx| {
            let mut operation=require_token(tx,token)?;
            let facts=prepared.facts();
            ensure!(facts.before.identity==token.selection.expected_identity && facts.after.identity==token.selection.expected_identity && normalize_local_path(&facts.before.path)==normalize_local_path(&token.selection.path) && normalize_local_path(&facts.after.path)==normalize_local_path(&token.selection.path) && token.selection.expected_body_sha256.as_ref().is_some_and(|hash|hash.eq_ignore_ascii_case(&facts.old_body_sha256)),LibraryError::conflict("Tag prepared owner does not match its explicit selection"));
            if let Some(commit)=&operation.commit {ensure!(commit.receipt.installed.as_ref()==Some(facts),LibraryError::conflict("Tag commit replay differs from its retained receipt"));return Ok(commit.clone());}
            ensure!(operation.phase==TagEditPhase::DiskVerified,LibraryError::conflict("Tag disk verification phase is missing"));require_approval(tx,token)?;
            let backup=operation.artifacts.iter().find(|item|item.role=="backup").context("Tag backup ledger is absent")?;
            let candidate=operation.artifacts.iter().find(|item|item.role=="candidate").context("Tag candidate ledger is absent")?;
            let journal=operation.artifacts.iter().find(|item|item.role=="journal").context("Tag journal ledger is absent")?;
            ensure!([backup,candidate,journal].iter().all(|item|item.phase==TagEditArtifactPhase::Sealed && item.identity.is_some() && item.directory_identity.is_some()) && backup.sha256.as_ref()==Some(&facts.old_body_sha256) && backup.actual_bytes==facts.before.size_bytes && candidate.sha256.as_ref()==Some(&facts.new_body_sha256) && candidate.actual_bytes==facts.after.size_bytes,LibraryError::conflict("Tag commit lacks the sealed actual backup, candidate or journal"));
            ensure!(facts.after.size_bytes<=operation.budget.original_bytes && facts.after.size_bytes<=operation.budget.candidate_bytes,LibraryError::budget("Installed tag body exceeded its reservation"));
            let old=library::asset_at(tx,&token.selection.asset_id)?.context("Tag asset disappeared")?;
            ensure!(old.size_bytes==facts.before.size_bytes && old.modified_ms==facts.before.modified_ms,LibraryError::conflict("Tag original metadata representation changed"));
            let document:String=tx.query_row("SELECT document FROM tracks WHERE id=?1",[&old.track_id],|row|row.get(0))?;
            let original=decode_document(&document)?;let item=prepared.prepare_asset(&original,&old)?;
            ensure!(item.asset.id==old.id && item.asset.track_id==old.track_id && item.track.id==old.track_id && item.file_identity.as_ref()==Some(&token.selection.expected_identity),LibraryError::conflict("Tag prepared asset changed stable identity"));
            let binding_revision=token.selection.expected_binding_revision.checked_add(1).filter(|v|*v<=i64::MAX as u64).context("Tag binding revision overflow")?;
            tracks::write_track(tx,&item.track,&encode_document(&item.track)?)?;tracks::write_asset(tx,&item.asset,&encode_document(&item.asset)?)?;
            let location=library::AssetLocation{path:normalize_local_path(&item.asset.path),file_identity:item.file_identity.clone(),content_sha256:Some(facts.new_body_sha256.clone()),hash_size_bytes:Some(item.asset.size_bytes),hash_modified_ms:item.asset.modified_ms,size_bytes:Some(item.asset.size_bytes),modified_ms:item.asset.modified_ms};
            ensure!(tx.execute("UPDATE local_asset_locations SET binding_revision=?2,document=?3 WHERE asset_id=?1 AND binding_revision=?4",params![old.id,binding_revision as i64,encode_document(&location)?,token.selection.expected_binding_revision as i64])?==1,LibraryError::conflict("Tag binding CAS failed"));
            library::refresh_list_metadata(tx,&item.track,&check_all)?;
            tx.execute("DELETE FROM library_relink_candidates WHERE asset_id=?1",[&old.id])?;
            let current:i64=tx.query_row("SELECT revision FROM repository_state WHERE singleton=1",[],|row|row.get(0))?;
            let change=commit_change(tx,current,ChangeKind::LocalTagsEdited,Some(old.track_id),serde_json::json!({"operation_id":token.operation_id,"asset_id":old.id,"binding_revision":binding_revision,"audio_payload_preserved":true}))?;
            let receipt=TagInstallationReceipt{operation_id:token.operation_id.clone(),asset_id:token.selection.asset_id.clone(),path:token.selection.path.clone(),file_identity:token.selection.expected_identity.clone(),old_body_sha256:facts.old_body_sha256.clone(),new_body_sha256:facts.new_body_sha256.clone(),audio_sha256:facts.audio_sha256.clone(),size_bytes:facts.after.size_bytes,committed_binding_revision:Some(binding_revision),installed:Some(facts.clone())};
            let committed=TagEditCommit{receipt,change};operation.commit=Some(committed.clone());operation.phase=TagEditPhase::RepoCommitted;next_ledger(&mut operation)?;save_operation(tx,&operation)?;library::check_library_storage(tx)?;Ok(committed)
        })
    }
}

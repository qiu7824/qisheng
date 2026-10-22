//! Same-object tag installation with explicit mutation rights and durable recovery.
//! The file installation and repository transaction are separate commit points.
pub use super::tag_write::TagMetadataSnapshot;
mod budget;
mod managed_file;
mod owned;
mod retire_native;
use super::{
    scan::ScanContext,
    tag_write::{
        EditableTagFile, LocalTagPatch, TagWriteFormat, TagWriteVerification,
        read_local_tag_audio_sha256, read_local_tag_metadata, write_local_tags,
    },
};
use crate::domain::{
    library::{
        FileIdentity, LibraryError, LibraryErrorKind, validate_library_id, validate_library_path,
    },
    music::{LocalAsset, TrackRecord, normalize_local_path},
};
use anyhow::{Context, Result, ensure};
pub use budget::TagPersistenceBudget;
pub use managed_file::TagManagedArtifact;
use managed_file::{
    ArtifactAllocator, ManagedAllocationAck, NativeFileRef, TagNativeFile, actual_identity,
    actual_path, allocate_artifact, links, normal_file,
};
pub use owned::{
    TagArtifactRetirementWitness, TagOriginalCloseWitness, TagOriginalRetirementRecoveryWitness,
    TagOriginalRetirementSeal, TagOwnedInstallation, TagUnallocatedArtifactWitness,
    TagUnopenedInstallationWitness, install_selected_tags_owned, install_tags_owned,
    recover_selected_tags_owned,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::{File, OpenOptions},
    io::{self, Read, Seek, SeekFrom, Write},
    panic::{AssertUnwindSafe, catch_unwind},
    path::{Path, PathBuf},
    time::{Duration, UNIX_EPOCH},
};

const CHUNK: usize = 64 * 1024;
const MAX_BODY: u64 = 1024 * 1024 * 1024 + 16 * 1024 * 1024;
const MAX_RECORD: usize = 1024 * 1024;
const MAX_RECORDS: usize = 64;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TagWriteSelection {
    pub operation_id: String,
    pub asset_id: String,
    pub path: PathBuf,
    pub expected_identity: FileIdentity,
    pub root_id: Option<String>,
    pub grant_epoch: u64,
    pub expected_binding_revision: u64,
    pub expected_body_sha256: Option<String>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum TagInstallPhase {
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
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TagRecoveryDisposition {
    RestoreOriginal,
    ResumePublication,
    Completed,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TagInstallArtifactRole {
    Journal,
    Backup,
    Candidate,
}
#[derive(Clone, Debug)]
pub enum TagRecoveryArtifactState {
    NeverAllocated,
    Allocated(FileIdentity),
    Retired,
    Uncertain,
}

/// Stored retirement facts identify the obligation; they are not native proof.
#[derive(Clone, Debug)]
pub struct TagRetirementRecovery {
    pub phase: TagInstallPhase,
    pub receipt: Option<TagInstallationReceipt>,
    pub observation: TagFileObservation,
    pub artifacts: Vec<TagRetirementRecoveryArtifact>,
    pub original: Option<TagOriginalRecoveryIntent>,
}
#[derive(Clone, Debug)]
pub struct TagOriginalRecoveryIntent {
    pub observation: TagFileObservation,
    pub facts: TagInstalledFacts,
}
#[derive(Clone, Debug)]
pub enum TagRetirementRecoveryArtifact {
    Retired {
        role: TagInstallArtifactRole,
        path: PathBuf,
    },
    Owned {
        facts: TagInstallArtifactFacts,
        size_bytes: u64,
        sha256: String,
        retiring: bool,
    },
}
#[derive(Clone, Debug)]
pub struct TagInstallArtifactFacts {
    pub role: TagInstallArtifactRole,
    pub path: PathBuf,
    pub identity: FileIdentity,
    pub directory_identity: FileIdentity,
}

pub struct TagArtifactAllocation<'a> {
    file: &'a File,
    directory: &'a File,
    facts: &'a TagInstallArtifactFacts,
}
impl TagArtifactAllocation<'_> {
    pub fn facts(&self) -> &TagInstallArtifactFacts {
        self.facts
    }
    pub fn check_empty(&self) -> Result<()> {
        check_artifact_owner(self.file, self.directory, self.facts)?;
        ensure!(
            self.file.metadata()?.len() == 0,
            "Tag artifact is not an actual empty allocation"
        );
        Ok(())
    }
}

pub struct TagArtifactSeal<'a> {
    file: &'a File,
    directory: &'a File,
    facts: &'a TagInstallArtifactFacts,
    observation: TagFileObservation,
    sha256: String,
    phase: TagInstallPhase,
}
impl TagArtifactSeal<'_> {
    pub fn facts(&self) -> &TagInstallArtifactFacts {
        self.facts
    }
    pub fn size_bytes(&self) -> u64 {
        self.observation.size_bytes
    }
    pub fn sha256(&self) -> &str {
        &self.sha256
    }
    pub fn phase(&self) -> TagInstallPhase {
        self.phase
    }
    pub fn check(&self) -> Result<()> {
        check_artifact_owner(self.file, self.directory, self.facts)?;
        ensure!(
            observe_file(self.file)? == self.observation,
            "Exclusively held sealed tag artifact changed"
        );
        Ok(())
    }
}

fn check_artifact_owner(
    file: &impl NativeFileRef,
    directory: &File,
    facts: &TagInstallArtifactFacts,
) -> Result<()> {
    file.inspect_native(|file| check_native_artifact_owner(file, directory, facts))
}
fn check_native_artifact_owner(
    file: &File,
    directory: &File,
    facts: &TagInstallArtifactFacts,
) -> Result<()> {
    ensure!(
        normal_file(file)?
            && links(file)? == 1
            && actual_identity(file)? == facts.identity
            && normalize_local_path(&actual_path(file)?) == normalize_local_path(&facts.path)
            && directory.metadata()?.is_dir()
            && actual_identity(directory)? == facts.directory_identity
            && facts.path.parent().map(normalize_local_path).as_ref()
                == Some(&normalize_local_path(&actual_path(directory)?)),
        "Tag artifact differs from its actual selected file and directory owners"
    );
    Ok(())
}
pub struct UnacknowledgedTagArtifact {
    file: TagNativeFile,
    facts: TagInstallArtifactFacts,
    message: String,
}
impl std::fmt::Debug for UnacknowledgedTagArtifact {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UnacknowledgedTagArtifact")
            .field("facts", &self.facts)
            .finish()
    }
}
impl std::fmt::Display for UnacknowledgedTagArtifact {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Tag artifact allocation is not acknowledged: {}",
            self.message
        )
    }
}
impl std::error::Error for UnacknowledgedTagArtifact {}
impl UnacknowledgedTagArtifact {
    pub(crate) fn acknowledge(
        &self,
        selection: &TagWriteSelection,
        coordinator: &mut dyn TagInstallCoordinator,
    ) -> Result<()> {
        ensure!(
            self.file.metadata()?.len() == 0 && actual_identity(&self.file)? == self.facts.identity,
            "Unacknowledged tag artifact changed"
        );
        let pins = pin_directory(
            self.facts
                .path
                .parent()
                .context("Tag artifact has no directory")?,
        )?;
        self.file.inspect_native(|file| {
            let allocation = TagArtifactAllocation {
                file,
                directory: pins
                    .last()
                    .context("Tag allocation directory is unavailable")?,
                facts: &self.facts,
            };
            coordinator.artifact_allocated(selection, &allocation)
        })
    }
    pub(crate) fn facts(&self) -> &TagInstallArtifactFacts {
        &self.facts
    }
}
fn acknowledge_created(
    file: TagNativeFile,
    directory: &File,
    selection: &TagWriteSelection,
    facts: TagInstallArtifactFacts,
    coordinator: &mut dyn TagInstallCoordinator,
) -> Result<TagNativeFile> {
    let acknowledged = file.inspect_native(|file| {
        coordinator.artifact_allocated(
            selection,
            &TagArtifactAllocation {
                file,
                directory,
                facts: &facts,
            },
        )
    });
    match acknowledged {
        Ok(()) => Ok(file),
        Err(error) => Err(UnacknowledgedTagArtifact {
            file,
            facts,
            message: bounded(&error),
        }
        .into()),
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TagInstallationReceipt {
    pub operation_id: String,
    pub asset_id: String,
    pub path: PathBuf,
    pub file_identity: FileIdentity,
    pub old_body_sha256: String,
    pub new_body_sha256: String,
    pub audio_sha256: String,
    pub size_bytes: u64,
    pub committed_binding_revision: Option<u64>,
    #[serde(default)]
    pub installed: Option<TagInstalledFacts>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TagFileObservation {
    pub path: PathBuf,
    pub identity: FileIdentity,
    pub size_bytes: u64,
    pub modified_ms: Option<u64>,
    pub modified_nanos: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TagInstalledFacts {
    pub before: TagFileObservation,
    pub after: TagFileObservation,
    pub metadata: TagMetadataSnapshot,
    pub old_body_sha256: String,
    pub new_body_sha256: String,
    pub audio_sha256: String,
    pub format: TagWriteFormat,
}

/// Immutable borrowing of the actual exclusively opened installed writer.
/// Serialized receipt facts cannot construct this live proof.
pub struct TagPreparedAsset<'a> {
    writer: &'a NativeWriterToken,
    facts: &'a TagInstalledFacts,
}
impl TagPreparedAsset<'_> {
    pub fn facts(&self) -> &TagInstalledFacts {
        self.facts
    }
    pub fn check(&self) -> Result<()> {
        self.writer.check()?;
        ensure!(
            observe_file(&self.writer.file)? == self.facts.after,
            "Installed tag writer representation changed"
        );
        ensure!(
            self.facts.before.identity == self.writer.selection.expected_identity
                && self.facts.after.identity == self.writer.selection.expected_identity
                && self.facts.after.path == normalize_local_path(&self.writer.selection.path)
                && self.facts.metadata.format == self.facts.format,
            "Installed tag evidence differs from its selected actual owner"
        );
        Ok(())
    }
    pub fn prepare_asset(
        &self,
        previous_track: &TrackRecord,
        previous_asset: &LocalAsset,
    ) -> Result<crate::domain::library::LibraryPreparedAsset> {
        self.check()?;
        ensure!(
            previous_asset.id == self.writer.selection.asset_id
                && previous_asset.track_id == previous_track.id
                && previous_track.local_asset_id.as_deref() == Some(previous_asset.id.as_str())
                && previous_track.platform.source == "local"
                && normalize_local_path(&previous_asset.path) == self.facts.before.path,
            "Installed tag evidence does not belong to the previous local asset"
        );
        let mut track = previous_track.clone();
        track.title = self
            .facts
            .metadata
            .title
            .clone()
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| {
                self.facts
                    .after
                    .path
                    .file_stem()
                    .map(|value| value.to_string_lossy().into_owned())
                    .unwrap_or_default()
            });
        track.artists.clone_from(&self.facts.metadata.artists);
        track.album = self.facts.metadata.album.clone().unwrap_or_default();
        track
            .metadata
            .insert("albumName".into(), track.album.clone().into());
        let mut asset = previous_asset.clone();
        asset.path.clone_from(&self.facts.after.path);
        asset.size_bytes = self.facts.after.size_bytes;
        asset.modified_ms = self.facts.after.modified_ms;
        asset.fingerprint = Some(self.facts.new_body_sha256.clone());
        asset.available = true;
        Ok(crate::domain::library::LibraryPreparedAsset {
            track,
            asset,
            file_identity: Some(self.facts.after.identity.clone()),
        })
    }
}

fn observe_file(file: &impl NativeFileRef) -> Result<TagFileObservation> {
    file.inspect_native(observe_native_file)
}
fn observe_native_file(file: &File) -> Result<TagFileObservation> {
    let metadata = file.metadata()?;
    let modified_nanos = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| {
            u64::try_from(duration.as_nanos()).context("Tag timestamp exceeds supported precision")
        })
        .transpose()?;
    Ok(TagFileObservation {
        path: normalize_local_path(&actual_path(file)?),
        identity: actual_identity(file)?,
        size_bytes: metadata.len(),
        modified_ms: modified_nanos.map(|value| value / 1_000_000),
        modified_nanos,
    })
}
pub trait TagInstallCoordinator {
    /// Pre-admits the same download object before its native call. Explicit
    /// Library tag writes retain their existing native-owner path.
    fn prepare_managed_artifact(
        &mut self,
        _selection: &TagWriteSelection,
        _role: TagInstallArtifactRole,
        _path: &Path,
        _maximum: u64,
    ) -> Result<Option<TagManagedArtifact>> {
        Ok(None)
    }
    fn before_managed_artifact_allocation(
        &mut self,
        _selection: &TagWriteSelection,
        _role: TagInstallArtifactRole,
        _intent: &crate::download::artifact::OwnedArtifactIntent,
    ) -> Result<()> {
        anyhow::bail!("Managed tag creation requires its durable actual allocation intent")
    }
    fn check_persistence_budget(
        &self,
        _selection: &TagWriteSelection,
        budget: &TagPersistenceBudget<'_>,
    ) -> Result<()> {
        budget.check()
    }
    fn begin_original_retirement(
        &mut self,
        _selection: &TagWriteSelection,
        seal: &TagOriginalRetirementSeal<'_>,
    ) -> Result<()> {
        seal.check()
    }
    fn original_retirement_recovered(
        &mut self,
        _selection: &TagWriteSelection,
        witness: &TagOriginalRetirementRecoveryWitness,
    ) -> Result<()> {
        witness.check()
    }
    /// Persist the admission attempt before any selected-object native open.
    fn before_installation_open(&self, _selection: &TagWriteSelection) -> Result<()> {
        Ok(())
    }
    fn recovery_unopened(&self, _selection: &TagWriteSelection) -> Result<bool> {
        Ok(false)
    }
    fn recovery_retirement(
        &self,
        _selection: &TagWriteSelection,
    ) -> Result<Option<TagRetirementRecovery>> {
        Ok(None)
    }
    fn authorize_recovery_selection(&self, selection: &TagWriteSelection) -> Result<()> {
        self.authorize_explicit_selection(selection)
    }
    fn before_artifact_allocation(
        &mut self,
        _selection: &TagWriteSelection,
        _role: TagInstallArtifactRole,
    ) -> Result<()> {
        Ok(())
    }
    fn recovery_artifact(
        &self,
        _selection: &TagWriteSelection,
        _role: TagInstallArtifactRole,
    ) -> Result<TagRecoveryArtifactState> {
        Ok(TagRecoveryArtifactState::Uncertain)
    }
    fn recovery_expected_body_sha256(
        &self,
        _selection: &TagWriteSelection,
    ) -> Result<Option<String>> {
        Ok(None)
    }
    fn check_retirement_owner(&self, _selection: &TagWriteSelection) -> Result<()> {
        Ok(())
    }
    fn installation_unopened(
        &mut self,
        _selection: &TagWriteSelection,
        _witness: &TagUnopenedInstallationWitness,
    ) -> Result<()> {
        Ok(())
    }
    fn begin_artifact_retirement(
        &mut self,
        _selection: &TagWriteSelection,
        seal: &TagArtifactSeal<'_>,
    ) -> Result<()> {
        seal.check()
    }
    fn artifact_retired(
        &mut self,
        _selection: &TagWriteSelection,
        _witness: &TagArtifactRetirementWitness,
    ) -> Result<()> {
        Ok(())
    }
    fn artifact_unallocated(
        &mut self,
        _selection: &TagWriteSelection,
        _witness: &TagUnallocatedArtifactWitness,
    ) -> Result<()> {
        Ok(())
    }
    fn original_closed(
        &mut self,
        _selection: &TagWriteSelection,
        _witness: &TagOriginalCloseWitness,
    ) -> Result<()> {
        Ok(())
    }
    fn authorize_explicit_selection(&self, selection: &TagWriteSelection) -> Result<()>;
    fn check_current_intent(&self, selection: &TagWriteSelection) -> Result<()>;
    fn journal_directory(&self) -> Result<PathBuf>;
    fn commit_installed(
        &mut self,
        selection: &TagWriteSelection,
        receipt: &TagInstallationReceipt,
    ) -> Result<u64>;
    fn publish_installed(
        &mut self,
        selection: &TagWriteSelection,
        receipt: &TagInstallationReceipt,
    ) -> Result<()>;
    fn commit_prepared(
        &mut self,
        selection: &TagWriteSelection,
        receipt: &TagInstallationReceipt,
        prepared: &TagPreparedAsset<'_>,
    ) -> Result<u64> {
        prepared.check()?;
        self.commit_installed(selection, receipt)
    }
    fn publish_prepared(
        &mut self,
        selection: &TagWriteSelection,
        receipt: &TagInstallationReceipt,
        prepared: &TagPreparedAsset<'_>,
    ) -> Result<()> {
        prepared.check()?;
        self.publish_installed(selection, receipt)
    }
    fn restored_asset(
        &mut self,
        _selection: &TagWriteSelection,
        prepared: &TagPreparedAsset<'_>,
    ) -> Result<()> {
        prepared.check()
    }
    fn check_recovery(
        &self,
        selection: &TagWriteSelection,
        phase: TagInstallPhase,
        receipt: Option<&TagInstallationReceipt>,
    ) -> Result<TagRecoveryDisposition>;
    fn checkpoint(&mut self, _phase: TagInstallPhase) -> Result<()> {
        Ok(())
    }
    fn check_install_chunk(&mut self, _restoring: bool, _written: u64) -> Result<()> {
        Ok(())
    }
    fn artifact_basename(
        &self,
        _selection: &TagWriteSelection,
        _role: TagInstallArtifactRole,
    ) -> Result<Option<String>> {
        Ok(None)
    }
    /// The producer calls this while its original empty file remains exclusively open.
    fn artifact_created(
        &mut self,
        _selection: &TagWriteSelection,
        _facts: &TagInstallArtifactFacts,
    ) -> Result<()> {
        Ok(())
    }
    fn artifact_allocated(
        &mut self,
        selection: &TagWriteSelection,
        allocation: &TagArtifactAllocation<'_>,
    ) -> Result<()> {
        allocation.check_empty()?;
        self.artifact_created(selection, allocation.facts())
    }
    fn artifact_sealed(
        &mut self,
        _selection: &TagWriteSelection,
        seal: &TagArtifactSeal<'_>,
    ) -> Result<()> {
        seal.check()
    }
    fn artifact_limit(&self, _role: TagInstallArtifactRole) -> u64 {
        MAX_BODY
    }
}

/// An actual exclusive selected-object lease. It is neither serializable nor
/// cloneable and is unrelated to the Library's read authorization types.
pub struct NativeWriterToken {
    selection: TagWriteSelection,
    file: TagNativeFile,
    _parents: Vec<File>,
    authority: Option<Box<dyn Fn() -> Result<()> + Send + Sync>>,
}
impl std::fmt::Debug for NativeWriterToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NativeWriterToken")
            .field("operation_id", &self.selection.operation_id)
            .field("path", &self.selection.path)
            .finish()
    }
}
impl NativeWriterToken {
    pub(crate) fn from_download_native(
        selection: TagWriteSelection,
        file: crate::download::DownloadNativeFile,
        authority: Box<dyn Fn() -> Result<()> + Send + Sync>,
        context: &ScanContext,
        coordinator: &dyn TagInstallCoordinator,
    ) -> Result<Self> {
        // The producer keeper already owns the actual descriptor independently
        // of this returnable borrowing, including all constructor failures.
        let token = Self {
            selection,
            file: TagNativeFile::Download(file),
            _parents: Vec::new(),
            authority: Some(authority),
        };
        validate_selection(&token.selection)?;
        context.check()?;
        coordinator.check_current_intent(&token.selection)?;
        token
            .file
            .native_metadata_io(retire_native::freeze_write_time)?;
        token.check()?;
        Ok(token)
    }
    /// Consumes a live exclusively opened writer and its independently selected
    /// namespace owner. Serialized selections cannot construct this token.
    pub(crate) fn from_owned_file(
        selection: TagWriteSelection,
        file: File,
        authority: Box<dyn Fn() -> Result<()> + Send + Sync>,
        context: &ScanContext,
        coordinator: &dyn TagInstallCoordinator,
    ) -> Result<Self> {
        validate_selection(&selection)?;
        context.check()?;
        coordinator.check_current_intent(&selection)?;
        retire_native::freeze_write_time(&file)?;
        let token = Self {
            selection,
            file: TagNativeFile::Native(file),
            _parents: Vec::new(),
            authority: Some(authority),
        };
        token.check()?;
        Ok(token)
    }
    pub fn from_explicit_selection(
        selection: TagWriteSelection,
        context: &ScanContext,
        coordinator: &dyn TagInstallCoordinator,
    ) -> Result<Self> {
        validate_selection(&selection)?;
        context.check()?;
        coordinator.authorize_explicit_selection(&selection)?;
        coordinator.check_current_intent(&selection)?;
        coordinator.before_installation_open(&selection)?;
        open_selected(selection)
    }
    fn check(&self) -> Result<()> {
        if let Some(authority) = &self.authority {
            authority()?;
        }
        ensure!(
            actual_identity(&self.file)? == self.selection.expected_identity
                && normalize_local_path(&actual_path(&self.file)?)
                    == normalize_local_path(&self.selection.path)
                && normal_file(&self.file)?
                && links(&self.file)? == 1,
            LibraryError::conflict(
                "Selected tag writer object, namespace or hardlink count changed"
            )
        );
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    selection: TagWriteSelection,
    phase: TagInstallPhase,
    journal_directory_identity: FileIdentity,
    backup_identity: Option<FileIdentity>,
    candidate_identity: Option<FileIdentity>,
    old_sha256: Option<String>,
    old_size: Option<u64>,
    receipt: Option<TagInstallationReceipt>,
    #[serde(default)]
    backup_basename: Option<String>,
    #[serde(default)]
    candidate_basename: Option<String>,
    #[serde(default)]
    original_observation: Option<TagFileObservation>,
    #[serde(default)]
    restored_facts: Option<TagInstalledFacts>,
}
struct Journal {
    directory: PathBuf,
    path: PathBuf,
    _pins: Vec<File>,
    file: TagNativeFile,
    records: usize,
    maximum_bytes: u64,
    manifest: Manifest,
}
struct Session {
    token: NativeWriterToken,
    journal: Journal,
    backup: Option<TagNativeFile>,
    candidate: Option<TagNativeFile>,
    owned: bool,
}

pub struct UnrestoredTagInstallation {
    session: Session,
}
impl std::fmt::Debug for UnrestoredTagInstallation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UnrestoredTagInstallation")
            .field("writer", &self.session.token)
            .field("phase", &self.session.journal.manifest.phase)
            .finish()
    }
}
#[derive(Debug)]
pub enum TagInstallError {
    Owned {
        message: String,
        owner: Box<TagOwnedInstallation>,
    },
    Rejected {
        message: String,
        owner: Box<NativeWriterToken>,
    },
    Unrestored {
        message: String,
        owner: Box<UnrestoredTagInstallation>,
    },
    PendingPublication {
        message: String,
        journal_path: PathBuf,
    },
}
impl std::fmt::Display for TagInstallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Owned { message, .. } => {
                write!(f, "Tag operation retains its actual owner: {message}")
            }
            Self::Rejected { message, .. } => {
                write!(f, "Tag installation was not admitted: {message}")
            }
            Self::Unrestored { message, .. } => {
                write!(f, "Tag installation is not restored: {message}")
            }
            Self::PendingPublication { message, .. } => write!(
                f,
                "Tag installation committed but publication is pending: {message}"
            ),
        }
    }
}
impl std::error::Error for TagInstallError {}

pub fn install_tags(
    mut token: NativeWriterToken,
    journal_directory: &Path,
    patch: &LocalTagPatch,
    context: &ScanContext,
    coordinator: &mut dyn TagInstallCoordinator,
) -> Result<TagInstallationReceipt> {
    if token.file.is_download() {
        return Err(TagInstallError::Rejected {
            message: "Managed download tags require the owned installation lifecycle".into(),
            owner: Box::new(token),
        }
        .into());
    }
    context.check()?;
    token.check()?;
    let original = observe_file(&token.file)?;
    if original.size_bytes > coordinator.artifact_limit(TagInstallArtifactRole::Backup)
        || original.size_bytes > coordinator.artifact_limit(TagInstallArtifactRole::Candidate)
    {
        return Err(TagInstallError::Rejected {
            message:
                "Actual original tag body exceeds its admitted backup or candidate reservation"
                    .into(),
            owner: Box::new(token),
        }
        .into());
    }
    let preflight = (|| -> Result<()> {
        let metadata = read_writer_metadata(&mut token, context)?;
        let budget = TagPersistenceBudget::original(&token, &metadata)?;
        budget::check_original(&budget, coordinator)
    })();
    if let Err(error) = preflight {
        return Err(TagInstallError::Rejected {
            message: bounded(&error),
            owner: Box::new(token),
        }
        .into());
    }
    let mut journal = create_journal(&token.selection, journal_directory, coordinator)?;
    journal.manifest.original_observation = Some(original);
    let mut session = Session {
        token,
        journal,
        backup: None,
        candidate: None,
        owned: false,
    };
    let result = catch_unwind(AssertUnwindSafe(|| {
        run_install(&mut session, patch, context, coordinator)
    }))
    .unwrap_or_else(|_| Err(anyhow::anyhow!("Tag installation worker panicked")));
    match result {
        Ok(receipt) => Ok(receipt),
        Err(error) => {
            let phase = session.journal.manifest.phase;
            if matches!(
                phase,
                TagInstallPhase::RepoCommitted | TagInstallPhase::Published
            ) {
                return Err(TagInstallError::PendingPublication {
                    message: bounded(&error),
                    journal_path: session.journal.path.clone(),
                }
                .into());
            }
            if matches!(
                phase,
                TagInstallPhase::Installing
                    | TagInstallPhase::DiskVerified
                    | TagInstallPhase::Restoring
            ) {
                // A coordinator must also detect commit success followed by an
                // error before the RepoCommitted journal record became durable.
                let decision = coordinator.check_recovery(
                    &session.token.selection,
                    phase,
                    session.journal.manifest.receipt.as_ref(),
                );
                if matches!(
                    decision,
                    Ok(TagRecoveryDisposition::ResumePublication
                        | TagRecoveryDisposition::Completed)
                ) {
                    return Err(TagInstallError::PendingPublication {
                        message: bounded(&error),
                        journal_path: session.journal.path.clone(),
                    }
                    .into());
                }
                if let Err(recovery_error) = decision {
                    return Err(TagInstallError::Unrestored {
                        message: format!(
                            "{}; recovery intent: {}",
                            bounded(&error),
                            bounded(&recovery_error)
                        ),
                        owner: Box::new(UnrestoredTagInstallation { session }),
                    }
                    .into());
                }
                let restored = catch_unwind(AssertUnwindSafe(|| {
                    restore(
                        &mut session,
                        &ScanContext::new(Duration::from_secs(5)),
                        coordinator,
                    )
                }))
                .unwrap_or_else(|_| Err(anyhow::anyhow!("Tag restoration worker panicked")));
                if let Err(restore_error) = restored {
                    return Err(TagInstallError::Unrestored {
                        message: format!(
                            "{}; restore: {}",
                            bounded(&error),
                            bounded(&restore_error)
                        ),
                        owner: Box::new(UnrestoredTagInstallation { session }),
                    }
                    .into());
                }
            }
            Err(error)
        }
    }
}
impl UnrestoredTagInstallation {
    pub fn retry_restore(
        mut self,
        context: &ScanContext,
        coordinator: &mut dyn TagInstallCoordinator,
    ) -> Result<()> {
        let result = (|| -> Result<()> {
            context.check()?;
            ensure!(
                coordinator.check_recovery(
                    &self.session.token.selection,
                    self.session.journal.manifest.phase,
                    self.session.journal.manifest.receipt.as_ref()
                )? == TagRecoveryDisposition::RestoreOriginal,
                "Committed tag installation must resume publication instead of restoration"
            );
            restore(&mut self.session, context, coordinator)
        })();
        if let Err(error) = result {
            return Err(TagInstallError::Unrestored {
                message: bounded(&error),
                owner: Box::new(self),
            }
            .into());
        }
        Ok(())
    }
}

fn run_install(
    session: &mut Session,
    patch: &LocalTagPatch,
    context: &ScanContext,
    coordinator: &mut dyn TagInstallCoordinator,
) -> Result<TagInstallationReceipt> {
    let original_metadata = read_writer_metadata(&mut session.token, context)?;
    phase(session, TagInstallPhase::Prepared, coordinator)?;
    let operation = session.token.selection.operation_id.clone();
    let backup_path = selected_artifact_path(
        &session.journal.directory,
        &operation,
        "backup",
        session.journal.manifest.backup_basename.as_deref(),
    )?;
    let allocator = if session.token.file.is_download() {
        ArtifactAllocator::Managed
    } else if session.owned {
        ArtifactAllocator::OwnedNative
    } else {
        ArtifactAllocator::Native
    };
    allocate_artifact(
        &mut session.backup,
        &session.token.selection,
        TagInstallArtifactRole::Backup,
        &backup_path,
        session
            .journal
            ._pins
            .last()
            .context("Tag backup parent pin is absent")?,
        allocator,
        coordinator,
    )?;
    let facts = TagInstallArtifactFacts {
        role: TagInstallArtifactRole::Backup,
        path: backup_path,
        identity: actual_identity(session.backup.as_ref().unwrap())?,
        directory_identity: session.journal.manifest.journal_directory_identity.clone(),
    };
    acknowledge_stored_artifact(session, facts, coordinator)?;
    session.token.check()?;
    ensure!(
        session.token.file.metadata()?.len()
            <= coordinator.artifact_limit(TagInstallArtifactRole::Backup),
        "Tag backup exceeds its admitted reservation"
    );
    let (old_sha, old_size) = copy_body(
        &mut session.token.file,
        session.backup.as_mut().unwrap(),
        context,
        || Ok(()),
    )?;
    ensure!(
        old_size <= coordinator.artifact_limit(TagInstallArtifactRole::Backup),
        "Tag backup exceeds its admitted reservation"
    );
    session.backup.as_ref().unwrap().sync_all()?;
    if let Some(expected) = &session.token.selection.expected_body_sha256 {
        ensure!(
            expected.eq_ignore_ascii_case(&old_sha),
            "Selected tag writer body changed before backup"
        );
    }
    session.journal.manifest.old_sha256 = Some(old_sha.clone());
    session.journal.manifest.old_size = Some(old_size);
    session.journal.manifest.backup_identity =
        Some(actual_identity(session.backup.as_ref().unwrap())?);
    phase(session, TagInstallPhase::BackupSealed, coordinator)?;
    let candidate_path = selected_artifact_path(
        &session.journal.directory,
        &operation,
        "edit",
        session.journal.manifest.candidate_basename.as_deref(),
    )?;
    allocate_artifact(
        &mut session.candidate,
        &session.token.selection,
        TagInstallArtifactRole::Candidate,
        &candidate_path,
        session
            .journal
            ._pins
            .last()
            .context("Tag candidate parent pin is absent")?,
        allocator,
        coordinator,
    )?;
    let facts = TagInstallArtifactFacts {
        role: TagInstallArtifactRole::Candidate,
        path: candidate_path,
        identity: actual_identity(session.candidate.as_ref().unwrap())?,
        directory_identity: session.journal.manifest.journal_directory_identity.clone(),
    };
    acknowledge_stored_artifact(session, facts, coordinator)?;
    copy_body(
        session.backup.as_mut().unwrap(),
        session.candidate.as_mut().unwrap(),
        context,
        || Ok(()),
    )?;
    let identity = actual_identity(session.candidate.as_ref().unwrap())?;
    let path = actual_path(session.candidate.as_ref().unwrap())?;
    let (verified, candidate_metadata) = {
        let mut editable = OwnedEditable {
            file: session.candidate.as_mut().unwrap(),
            identity: identity.clone(),
            path,
            context,
            maximum_bytes: coordinator.artifact_limit(TagInstallArtifactRole::Candidate),
        };
        let verified = write_local_tags(&mut editable, patch, context)?;
        let metadata = read_local_tag_metadata(&mut editable, context)?;
        (verified, metadata)
    };
    let mut receipt = make_receipt(&session.token.selection, &old_sha, &verified);
    session.journal.manifest.candidate_identity = Some(identity);
    session.journal.manifest.receipt = Some(receipt.clone());
    let persistence = TagPersistenceBudget::candidate(
        &session.token,
        &original_metadata,
        session.candidate.as_ref().unwrap(),
        &candidate_metadata,
    )?;
    budget::check_candidate(&persistence, &session.journal, coordinator)?;
    phase(session, TagInstallPhase::CandidateVerified, coordinator)?;
    context.check()?;
    coordinator.check_current_intent(&session.token.selection)?;
    session.token.check()?;
    ensure!(
        hash_body(&mut session.token.file, context)? == old_sha,
        "Selected tag writer changed while preparing its copy"
    );
    phase(session, TagInstallPhase::Installing, coordinator)?;
    copy_into_original(session, false, context, coordinator)?;
    ensure!(
        hash_body(&mut session.token.file, context)? == receipt.new_body_sha256,
        "Installed tag body did not match the verified copy"
    );
    let metadata = read_writer_metadata(&mut session.token, context)?;
    ensure!(
        metadata == candidate_metadata && metadata.format == verified.format,
        "Installed actual writer metadata differs from its verified candidate"
    );
    let before = session
        .journal
        .manifest
        .original_observation
        .clone()
        .context("Original tag writer observation is unavailable")?;
    let after = observe_file(&session.token.file)?;
    ensure!(
        before.identity == after.identity && after.size_bytes == receipt.size_bytes,
        "Installed tag writer does not retain its original object and verified size"
    );
    receipt.installed = Some(TagInstalledFacts {
        before,
        after,
        metadata,
        old_body_sha256: receipt.old_body_sha256.clone(),
        new_body_sha256: receipt.new_body_sha256.clone(),
        audio_sha256: receipt.audio_sha256.clone(),
        format: verified.format,
    });
    session.journal.manifest.receipt = Some(receipt.clone());
    phase(session, TagInstallPhase::DiskVerified, coordinator)?;
    context.check()?;
    coordinator.check_current_intent(&session.token.selection)?;
    session.token.check()?;
    let prepared = TagPreparedAsset {
        writer: &session.token,
        facts: receipt.installed.as_ref().unwrap(),
    };
    let revision = coordinator.commit_prepared(&session.token.selection, &receipt, &prepared)?;
    receipt.committed_binding_revision = Some(revision);
    session.journal.manifest.receipt = Some(receipt.clone());
    phase(session, TagInstallPhase::RepoCommitted, coordinator)?;
    let prepared = TagPreparedAsset {
        writer: &session.token,
        facts: receipt.installed.as_ref().unwrap(),
    };
    coordinator.publish_prepared(&session.token.selection, &receipt, &prepared)?;
    phase(session, TagInstallPhase::Published, coordinator)?;
    Ok(receipt)
}
fn acknowledge_stored_artifact(
    session: &mut Session,
    facts: TagInstallArtifactFacts,
    coordinator: &mut dyn TagInstallCoordinator,
) -> Result<()> {
    let slot = match facts.role {
        TagInstallArtifactRole::Backup => &mut session.backup,
        TagInstallArtifactRole::Candidate => &mut session.candidate,
        TagInstallArtifactRole::Journal => anyhow::bail!("Journal has its separate retained owner"),
    };
    let directory = session
        .journal
        ._pins
        .last()
        .context("Tag journal directory pin missing")?;
    if session.owned {
        if slot.as_ref().unwrap().is_download() {
            return Ok(());
        }
        slot.as_ref().unwrap().inspect_native(|file| {
            coordinator.artifact_allocated(
                &session.token.selection,
                &TagArtifactAllocation {
                    file,
                    directory,
                    facts: &facts,
                },
            )
        })
    } else {
        *slot = Some(acknowledge_created(
            slot.take().unwrap(),
            directory,
            &session.token.selection,
            facts,
            coordinator,
        )?);
        Ok(())
    }
}
fn make_receipt(
    selection: &TagWriteSelection,
    old: &str,
    new: &TagWriteVerification,
) -> TagInstallationReceipt {
    TagInstallationReceipt {
        operation_id: selection.operation_id.clone(),
        asset_id: selection.asset_id.clone(),
        path: selection.path.clone(),
        file_identity: selection.expected_identity.clone(),
        old_body_sha256: old.into(),
        new_body_sha256: new.body_sha256.clone(),
        audio_sha256: new.audio_sha256.clone(),
        size_bytes: new.size_bytes,
        committed_binding_revision: None,
        installed: None,
    }
}
fn phase(
    session: &mut Session,
    phase: TagInstallPhase,
    coordinator: &mut dyn TagInstallCoordinator,
) -> Result<()> {
    let changed = session.journal.manifest.phase != phase || session.journal.records == 0;
    session.journal.manifest.phase = phase;
    if changed {
        append_record(&mut session.journal)?;
        seal_artifact(
            &mut session.journal.file,
            session
                .journal
                ._pins
                .last()
                .context("Tag journal directory pin missing")?,
            &session.token.selection,
            TagInstallArtifactRole::Journal,
            phase,
            coordinator,
        )?;
        if phase == TagInstallPhase::BackupSealed
            && let Some(backup) = session.backup.as_mut()
        {
            seal_artifact(
                backup,
                session
                    .journal
                    ._pins
                    .last()
                    .context("Tag journal directory pin missing")?,
                &session.token.selection,
                TagInstallArtifactRole::Backup,
                phase,
                coordinator,
            )?;
        }
        if phase == TagInstallPhase::CandidateVerified
            && let Some(candidate) = session.candidate.as_mut()
        {
            seal_artifact(
                candidate,
                session
                    .journal
                    ._pins
                    .last()
                    .context("Tag journal directory pin missing")?,
                &session.token.selection,
                TagInstallArtifactRole::Candidate,
                phase,
                coordinator,
            )?;
        }
    }
    coordinator.checkpoint(phase)
}

fn seal_artifact(
    file: &mut TagNativeFile,
    directory: &File,
    selection: &TagWriteSelection,
    role: TagInstallArtifactRole,
    phase: TagInstallPhase,
    coordinator: &mut dyn TagInstallCoordinator,
) -> Result<()> {
    let facts = TagInstallArtifactFacts {
        role,
        path: normalize_local_path(&actual_path(file)?),
        identity: actual_identity(file)?,
        directory_identity: actual_identity(directory)?,
    };
    let context = ScanContext::new(Duration::from_secs(30));
    let sha256 = hash_body(file, &context)?;
    let observation = observe_file(file)?;
    file.inspect_native(|file| {
        let seal = TagArtifactSeal {
            observation,
            file,
            directory,
            facts: &facts,
            sha256,
            phase,
        };
        seal.check()?;
        coordinator.artifact_sealed(selection, &seal)
    })
}

fn read_writer_metadata(
    token: &mut NativeWriterToken,
    context: &ScanContext,
) -> Result<TagMetadataSnapshot> {
    token.check()?;
    let metadata = {
        let mut editable = OwnedEditable {
            file: &mut token.file,
            identity: token.selection.expected_identity.clone(),
            path: token.selection.path.clone(),
            context,
            maximum_bytes: MAX_BODY,
        };
        read_local_tag_metadata(&mut editable, context)?
    };
    token.check()?;
    Ok(metadata)
}
fn read_writer_audio_sha256(
    token: &mut NativeWriterToken,
    context: &ScanContext,
) -> Result<String> {
    token.check()?;
    let mut editable = OwnedEditable {
        file: &mut token.file,
        identity: token.selection.expected_identity.clone(),
        path: token.selection.path.clone(),
        context,
        maximum_bytes: MAX_BODY,
    };
    let sha = read_local_tag_audio_sha256(&mut editable, context)?;
    token.check()?;
    Ok(sha)
}
fn verify_recovered_facts(
    token: &mut NativeWriterToken,
    facts: &TagInstalledFacts,
    context: &ScanContext,
) -> Result<()> {
    token.check()?;
    ensure!(
        observe_file(&token.file)? == facts.after,
        "Recovered actual tag writer stamp differs from its durable evidence"
    );
    ensure!(
        hash_body(&mut token.file, context)? == facts.new_body_sha256,
        "Recovered actual tag writer body differs from its durable evidence"
    );
    ensure!(
        read_writer_metadata(token, context)? == facts.metadata,
        "Recovered actual tag writer metadata differs from its durable evidence"
    );
    let audio_sha256 = {
        let mut editable = OwnedEditable {
            file: &mut token.file,
            identity: token.selection.expected_identity.clone(),
            path: token.selection.path.clone(),
            context,
            maximum_bytes: MAX_BODY,
        };
        read_local_tag_audio_sha256(&mut editable, context)?
    };
    ensure!(
        audio_sha256 == facts.audio_sha256,
        "Recovered actual tag writer audio differs from its durable evidence"
    );
    TagPreparedAsset {
        writer: token,
        facts,
    }
    .check()
}
fn copy_into_original(
    session: &mut Session,
    restoring: bool,
    context: &ScanContext,
    coordinator: &mut dyn TagInstallCoordinator,
) -> Result<()> {
    session.token.check()?;
    let source = if restoring {
        session.backup.as_mut()
    } else {
        session.candidate.as_mut()
    }
    .context("Tag installation source is unavailable")?;
    let length = source.metadata()?.len();
    ensure!(
        length <= MAX_BODY,
        "Tag installation body exceeds its limit"
    );
    source.rewind()?;
    session.token.file.rewind()?;
    let mut buffer = [0u8; CHUNK];
    let mut written = 0u64;
    while written < length {
        context.check()?;
        session.token.check()?;
        coordinator.check_install_chunk(restoring, written)?;
        let count = ((length - written).min(CHUNK as u64)) as usize;
        source.read_exact(&mut buffer[..count])?;
        session.token.file.write_all(&buffer[..count])?;
        written += count as u64;
        session.token.check()?;
        context.check()?;
    }
    coordinator.check_install_chunk(restoring, written)?;
    session.token.file.set_len(length)?;
    session.token.file.sync_all()?;
    session
        .token
        .file
        .native_metadata_io(retire_native::settle_write_time)?;
    session.token.check()?;
    Ok(())
}
fn restore(
    session: &mut Session,
    context: &ScanContext,
    coordinator: &mut dyn TagInstallCoordinator,
) -> Result<()> {
    session.token.check()?;
    phase(session, TagInstallPhase::Restoring, coordinator)?;
    let backup = session
        .backup
        .as_mut()
        .context("Original tag backup is unavailable")?;
    ensure!(
        Some(actual_identity(backup)?) == session.journal.manifest.backup_identity,
        "Original tag backup identity changed"
    );
    ensure!(
        Some(hash_body(backup, context)?) == session.journal.manifest.old_sha256,
        "Original tag backup SHA256 changed"
    );
    copy_into_original(session, true, context, coordinator)?;
    ensure!(
        Some(hash_body(&mut session.token.file, context)?) == session.journal.manifest.old_sha256,
        "Original tag file is not restored"
    );
    if let Some(before) = session.journal.manifest.original_observation.clone() {
        let metadata = read_writer_metadata(&mut session.token, context)?;
        let original_sha = session
            .journal
            .manifest
            .old_sha256
            .clone()
            .context("Restored original SHA is unavailable")?;
        let audio_sha = read_writer_audio_sha256(&mut session.token, context)?;
        let facts = TagInstalledFacts {
            before,
            after: observe_file(&session.token.file)?,
            format: metadata.format,
            metadata,
            old_body_sha256: original_sha.clone(),
            new_body_sha256: original_sha,
            audio_sha256: audio_sha,
        };
        let prepared = TagPreparedAsset {
            writer: &session.token,
            facts: &facts,
        };
        prepared.check()?;
        coordinator.restored_asset(&session.token.selection, &prepared)?;
        session.journal.manifest.restored_facts = Some(facts);
    }
    phase(session, TagInstallPhase::Restored, coordinator)
}

pub fn recover_tag_install(
    journal_file: &Path,
    context: &ScanContext,
    coordinator: &mut dyn TagInstallCoordinator,
) -> Result<Option<TagInstallationReceipt>> {
    recover_tag_install_inner(journal_file, context, coordinator, None)
}
pub(crate) fn recover_tag_install_owned(
    journal_file: &Path,
    context: &ScanContext,
    coordinator: &mut dyn TagInstallCoordinator,
    token: NativeWriterToken,
) -> Result<Option<TagInstallationReceipt>> {
    if token.file.is_download() {
        return Err(TagInstallError::Rejected {
            message: "Managed download recovery requires its retained native artifact allocator"
                .into(),
            owner: Box::new(token),
        }
        .into());
    }
    recover_tag_install_inner(journal_file, context, coordinator, Some(token))
}
fn recover_tag_install_inner(
    journal_file: &Path,
    context: &ScanContext,
    coordinator: &mut dyn TagInstallCoordinator,
    owned: Option<NativeWriterToken>,
) -> Result<Option<TagInstallationReceipt>> {
    context.check()?;
    let directory = normalize_local_path(&coordinator.journal_directory()?);
    ensure!(
        normalize_local_path(
            journal_file
                .parent()
                .context("Tag journal parent is unavailable")?
        ) == directory,
        "Tag journal is outside the owned profile directory"
    );
    let pins = pin_directory(&directory)?;
    let mut file = open_existing_owned(journal_file)?;
    let (manifest, records, valid_length) = read_records(&mut file)?;
    let journal_name =
        coordinator.artifact_basename(&manifest.selection, TagInstallArtifactRole::Journal)?;
    ensure!(
        normalize_local_path(journal_file)
            == selected_artifact_path(
                &directory,
                &manifest.selection.operation_id,
                "journal",
                journal_name.as_deref()
            )?
            && actual_identity(
                pins.last()
                    .context("Tag journal directory proof is unavailable")?
            )? == manifest.journal_directory_identity,
        "Tag journal name or actual directory identity changed"
    );
    validate_selection(&manifest.selection)?;
    ensure!(
        manifest.backup_basename
            == coordinator
                .artifact_basename(&manifest.selection, TagInstallArtifactRole::Backup)?
            && manifest.candidate_basename
                == coordinator
                    .artifact_basename(&manifest.selection, TagInstallArtifactRole::Candidate)?,
        "Tag recovery artifact names differ from current admitted reservations"
    );
    if owned.is_none() {
        coordinator.authorize_explicit_selection(&manifest.selection)?;
    }
    let disposition = coordinator.check_recovery(
        &manifest.selection,
        manifest.phase,
        manifest.receipt.as_ref(),
    )?;
    // The coordinator checks the phase-specific authoritative repository state
    // before any original writer is opened. Journal JSON is never permission.
    let token = if let Some(token) = owned {
        ensure!(
            token.selection.operation_id == manifest.selection.operation_id
                && token.selection.asset_id == manifest.selection.asset_id
                && token.selection.path == manifest.selection.path
                && token.selection.expected_identity == manifest.selection.expected_identity
                && token.selection.grant_epoch == manifest.selection.grant_epoch
                && token.selection.expected_binding_revision
                    == manifest.selection.expected_binding_revision
                && token.selection.expected_body_sha256 == manifest.selection.expected_body_sha256,
            "Owned recovery token differs from its durable selection"
        );
        token.check()?;
        token
    } else {
        open_selected(manifest.selection.clone())?
    };
    let mut session = Session {
        token,
        journal: Journal {
            directory,
            path: journal_file.to_owned(),
            _pins: pins,
            file,
            records,
            manifest,
            maximum_bytes: coordinator.artifact_limit(TagInstallArtifactRole::Journal),
        },
        backup: None,
        candidate: None,
        owned: false,
    };
    session.journal.file.set_len(valid_length)?;
    session.journal.file.sync_all()?;
    match disposition {
        TagRecoveryDisposition::ResumePublication | TagRecoveryDisposition::Completed => {
            let mut receipt = session
                .journal
                .manifest
                .receipt
                .clone()
                .context("Committed tag receipt is unavailable")?;
            ensure!(
                hash_body(&mut session.token.file, context)? == receipt.new_body_sha256,
                "Committed tag body changed before recovery"
            );
            if receipt.committed_binding_revision.is_none() {
                // An operation-idempotent repository call closes the durable
                // database-commit/journal-record gap without applying twice.
                let revision = if let Some(facts) = receipt.installed.as_ref() {
                    verify_recovered_facts(&mut session.token, facts, context)?;
                    let prepared = TagPreparedAsset {
                        writer: &session.token,
                        facts,
                    };
                    coordinator.commit_prepared(&session.token.selection, &receipt, &prepared)?
                } else {
                    coordinator.commit_installed(&session.token.selection, &receipt)?
                };
                receipt.committed_binding_revision = Some(revision);
                session.journal.manifest.receipt = Some(receipt.clone());
                phase(&mut session, TagInstallPhase::RepoCommitted, coordinator)?;
            }
            if disposition == TagRecoveryDisposition::ResumePublication {
                if let Some(facts) = receipt.installed.as_ref() {
                    verify_recovered_facts(&mut session.token, facts, context)?;
                    let prepared = TagPreparedAsset {
                        writer: &session.token,
                        facts,
                    };
                    coordinator.publish_prepared(&session.token.selection, &receipt, &prepared)?;
                } else {
                    coordinator.publish_installed(&session.token.selection, &receipt)?;
                }
                phase(&mut session, TagInstallPhase::Published, coordinator)?;
            }
            Ok(Some(receipt))
        }
        TagRecoveryDisposition::RestoreOriginal => {
            if matches!(
                session.journal.manifest.phase,
                TagInstallPhase::Prepared
                    | TagInstallPhase::BackupSealed
                    | TagInstallPhase::CandidateVerified
                    | TagInstallPhase::Restored
            ) {
                if let Some(old) = &session.journal.manifest.old_sha256 {
                    ensure!(
                        hash_body(&mut session.token.file, context)? == *old,
                        "Original tag file changed before non-mutating recovery"
                    );
                }
                if session.journal.manifest.phase == TagInstallPhase::Restored
                    && let Some(facts) = session.journal.manifest.restored_facts.clone()
                {
                    verify_recovered_facts(&mut session.token, &facts, context)?;
                    let prepared = TagPreparedAsset {
                        writer: &session.token,
                        facts: &facts,
                    };
                    coordinator.restored_asset(&session.token.selection, &prepared)?;
                }
                return Ok(None);
            }
            let path = selected_artifact_path(
                &session.journal.directory,
                &session.token.selection.operation_id,
                "backup",
                session.journal.manifest.backup_basename.as_deref(),
            )?;
            session.backup = Some(open_existing_owned(&path)?);
            if let Err(error) = restore(&mut session, context, coordinator) {
                return Err(TagInstallError::Unrestored {
                    message: bounded(&error),
                    owner: Box::new(UnrestoredTagInstallation { session }),
                }
                .into());
            }
            Ok(None)
        }
    }
}

fn create_journal(
    selection: &TagWriteSelection,
    directory: &Path,
    coordinator: &mut dyn TagInstallCoordinator,
) -> Result<Journal> {
    let directory = normalize_local_path(directory);
    ensure!(
        directory == normalize_local_path(&coordinator.journal_directory()?),
        "Tag journal directory is not owned by this profile"
    );
    let pins = pin_directory(&directory)?;
    let identity = actual_identity(
        pins.last()
            .context("Tag journal directory proof is unavailable")?,
    )?;
    let journal_name = coordinator.artifact_basename(selection, TagInstallArtifactRole::Journal)?;
    let path = selected_artifact_path(
        &directory,
        &selection.operation_id,
        "journal",
        journal_name.as_deref(),
    )?;
    coordinator.before_artifact_allocation(selection, TagInstallArtifactRole::Journal)?;
    let file = TagNativeFile::Native(create_owned_file(&path)?);
    let facts = TagInstallArtifactFacts {
        role: TagInstallArtifactRole::Journal,
        path: path.clone(),
        identity: actual_identity(&file)?,
        directory_identity: identity.clone(),
    };
    let file = acknowledge_created(
        file,
        pins.last().context("Tag journal directory pin missing")?,
        selection,
        facts,
        coordinator,
    )?;
    let backup_basename =
        coordinator.artifact_basename(selection, TagInstallArtifactRole::Backup)?;
    let candidate_basename =
        coordinator.artifact_basename(selection, TagInstallArtifactRole::Candidate)?;
    Ok(Journal {
        directory,
        path,
        _pins: pins,
        file,
        records: 0,
        maximum_bytes: coordinator.artifact_limit(TagInstallArtifactRole::Journal),
        manifest: Manifest {
            selection: selection.clone(),
            phase: TagInstallPhase::Prepared,
            journal_directory_identity: identity,
            backup_identity: None,
            candidate_identity: None,
            old_sha256: None,
            old_size: None,
            receipt: None,
            backup_basename,
            candidate_basename,
            original_observation: None,
            restored_facts: None,
        },
    })
}
pub fn journal_path(directory: &Path, operation_id: &str) -> PathBuf {
    artifact_path(directory, operation_id, "journal")
}
fn artifact_path(directory: &Path, operation_id: &str, extension: &str) -> PathBuf {
    directory.join(format!(
        "tag-install-{:x}.{extension}",
        Sha256::digest(operation_id.as_bytes())
    ))
}
fn selected_artifact_path(
    directory: &Path,
    operation_id: &str,
    extension: &str,
    basename: Option<&str>,
) -> Result<PathBuf> {
    if let Some(basename) = basename {
        crate::download::validate_basename(basename)?;
        ensure!(
            basename.len() == 50
                && basename.starts_with(".lx-download-")
                && basename.ends_with(".part")
                && basename[13..45].bytes().all(|b| b.is_ascii_hexdigit()),
            "Invalid selected tag artifact basename"
        );
        Ok(directory.join(basename))
    } else {
        Ok(artifact_path(directory, operation_id, extension))
    }
}
fn append_record(journal: &mut Journal) -> Result<()> {
    ensure!(
        journal.records < MAX_RECORDS,
        "Tag journal exceeds 64 durable records"
    );
    let body = serde_json::to_vec(&journal.manifest)?;
    ensure!(
        body.len() <= MAX_RECORD,
        "Tag journal manifest exceeds 1 MiB"
    );
    ensure!(
        journal
            .file
            .metadata()?
            .len()
            .checked_add(body.len() as u64 + 36)
            .is_some_and(|bytes| bytes <= journal.maximum_bytes),
        "Tag journal exceeds its admitted byte reservation"
    );
    journal.file.seek(SeekFrom::End(0))?;
    journal.file.write_all(&(body.len() as u32).to_le_bytes())?;
    journal.file.write_all(&Sha256::digest(&body))?;
    for chunk in body.chunks(CHUNK) {
        journal.file.write_all(chunk)?;
    }
    journal.file.sync_all()?;
    journal.records += 1;
    Ok(())
}
fn read_records(file: &mut TagNativeFile) -> Result<(Manifest, usize, u64)> {
    ensure!(
        file.metadata()?.len() <= (MAX_RECORD + 36) as u64 * MAX_RECORDS as u64,
        "Tag journal exceeds its physical byte limit"
    );
    file.rewind()?;
    let mut last = None;
    let mut count = 0usize;
    let mut valid = 0u64;
    loop {
        let mut length = [0u8; 4];
        if !read_complete_or_torn(file, &mut length)? {
            break;
        }
        let length = u32::from_le_bytes(length) as usize;
        ensure!(
            length <= MAX_RECORD && count < MAX_RECORDS,
            "Invalid tag journal record envelope"
        );
        let mut expected = [0u8; 32];
        if !read_complete_or_torn(file, &mut expected)? {
            break;
        }
        let mut body = vec![0; length];
        if !read_complete_or_torn(file, &mut body)? {
            break;
        }
        ensure!(
            Sha256::digest(&body).as_slice() == expected,
            "Tag journal record checksum changed"
        );
        let record: Manifest = serde_json::from_slice(&body)?;
        if let Some(previous) = &last {
            let previous: &Manifest = previous;
            ensure!(
                previous.selection.operation_id == record.selection.operation_id
                    && previous.selection.asset_id == record.selection.asset_id
                    && previous.selection.path == record.selection.path
                    && previous.selection.expected_identity == record.selection.expected_identity,
                "Tag journal changed its selected object"
            );
        }
        last = Some(record);
        count += 1;
        valid = file.stream_position()?;
    }
    Ok((
        last.context("Tag journal has no complete durable record")?,
        count,
        valid,
    ))
}
fn read_complete_or_torn(file: &mut TagNativeFile, buffer: &mut [u8]) -> Result<bool> {
    match file.read_exact(buffer) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => Ok(false),
        Err(error) => Err(error.into()),
    }
}
fn validate_selection(selection: &TagWriteSelection) -> Result<()> {
    validate_library_id(&selection.operation_id)?;
    validate_library_id(&selection.asset_id)?;
    validate_library_path(&selection.path)?;
    selection.expected_identity.validate()?;
    ensure!(
        selection.grant_epoch > 0 && selection.expected_binding_revision > 0,
        "Tag write selection has an invalid epoch or binding revision"
    );
    if let Some(root) = &selection.root_id {
        validate_library_id(root)?;
    }
    ensure!(
        selection.path.components().all(|part| match part {
            std::path::Component::Normal(name) =>
                name.to_str().is_some_and(|name| !name.contains(':')),
            std::path::Component::Prefix(_) | std::path::Component::RootDir => true,
            _ => false,
        }),
        "Selected tag path contains an unsafe component or alternate stream"
    );
    if let Some(hash) = &selection.expected_body_sha256 {
        ensure!(
            hash.len() == 64 && hash.bytes().all(|byte| byte.is_ascii_hexdigit()),
            "Invalid selected original SHA256"
        );
    }
    Ok(())
}
fn bounded(error: &anyhow::Error) -> String {
    format!("{error:#}").chars().take(1024).collect()
}
fn hash_body(
    file: &mut (impl Read + Seek + NativeFileRef),
    context: &ScanContext,
) -> Result<String> {
    let length = file.inspect_native(|file| Ok(file.metadata()?.len()))?;
    ensure!(length <= MAX_BODY, "Tag body exceeds its size limit");
    file.rewind()?;
    let mut digest = Sha256::new();
    let mut buffer = [0u8; CHUNK];
    let mut read = 0;
    while read < length {
        context.check()?;
        let count = ((length - read).min(CHUNK as u64)) as usize;
        file.read_exact(&mut buffer[..count])?;
        digest.update(&buffer[..count]);
        read += count as u64;
    }
    context.check()?;
    Ok(format!("{:x}", digest.finalize()))
}
fn copy_body(
    source: &mut TagNativeFile,
    target: &mut TagNativeFile,
    context: &ScanContext,
    mut check: impl FnMut() -> Result<()>,
) -> Result<(String, u64)> {
    let length = source.metadata()?.len();
    ensure!(
        length > 0 && length <= MAX_BODY,
        "Tag body exceeds its size limit"
    );
    source.rewind()?;
    target.rewind()?;
    let mut digest = Sha256::new();
    let mut buffer = [0u8; CHUNK];
    let mut copied = 0;
    while copied < length {
        context.check()?;
        check()?;
        let count = ((length - copied).min(CHUNK as u64)) as usize;
        source.read_exact(&mut buffer[..count])?;
        target.write_all(&buffer[..count])?;
        digest.update(&buffer[..count]);
        copied += count as u64;
    }
    target.set_len(length)?;
    target.sync_all()?;
    context.check()?;
    Ok((format!("{:x}", digest.finalize()), length))
}
struct OwnedEditable<'a> {
    file: &'a mut TagNativeFile,
    identity: FileIdentity,
    path: PathBuf,
    context: &'a ScanContext,
    maximum_bytes: u64,
}
impl EditableTagFile for OwnedEditable<'_> {
    fn check_write_authority(&self) -> io::Result<()> {
        self.context.check().map_err(io::Error::other)?;
        if actual_identity(self.file).map_err(io::Error::other)? != self.identity
            || normalize_local_path(&actual_path(self.file).map_err(io::Error::other)?)
                != normalize_local_path(&self.path)
        {
            return Err(io::Error::other("Owned editable tag copy changed"));
        }
        Ok(())
    }
    fn sync_data(&mut self) -> io::Result<()> {
        self.file.sync_all()
    }
}
impl Read for OwnedEditable<'_> {
    fn read(&mut self, b: &mut [u8]) -> io::Result<usize> {
        self.file.read(b)
    }
}
impl Write for OwnedEditable<'_> {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        if !self
            .file
            .stream_position()?
            .checked_add(b.len() as u64)
            .is_some_and(|bytes| bytes <= self.maximum_bytes)
        {
            return Err(io::Error::other(
                "Tag edit exceeds its admitted reservation",
            ));
        }
        self.file.write(b)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}
impl Seek for OwnedEditable<'_> {
    fn seek(&mut self, p: SeekFrom) -> io::Result<u64> {
        self.file.seek(p)
    }
}
impl lofty::io::Length for OwnedEditable<'_> {
    fn len(&self) -> io::Result<u64> {
        Ok(self.file.metadata()?.len())
    }
}
impl lofty::io::Truncate for OwnedEditable<'_> {
    fn truncate(&mut self, n: u64) -> io::Result<()> {
        if n > self.maximum_bytes {
            return Err(io::Error::other(
                "Tag edit exceeds its admitted reservation",
            ));
        }
        self.file.set_len(n)
    }
}

#[cfg(windows)]
fn open_selected(selection: TagWriteSelection) -> Result<NativeWriterToken> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE,
        SYNCHRONIZE,
    };
    let mut parents = pin_directory(
        selection
            .path
            .parent()
            .context("Selected tag file has no parent")?,
    )?;
    let name_pin = OpenOptions::new()
        .access_mode(FILE_READ_ATTRIBUTES | SYNCHRONIZE)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(&selection.path)?;
    ensure!(
        actual_identity(&name_pin)? == selection.expected_identity
            && normal_file(&name_pin)?
            && links(&name_pin)? == 1
            && normalize_local_path(&actual_path(&name_pin)?)
                == normalize_local_path(&selection.path),
        "Selected tag object changed before any write handle was opened"
    );
    parents.push(name_pin);
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .share_mode(0)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(&selection.path)
        .map_err(|error| {
            if matches!(error.raw_os_error(), Some(32 | 33)) {
                anyhow::Error::from(LibraryError::new(
                    LibraryErrorKind::Busy,
                    "Selected tag file has active readers or a competing writer",
                ))
            } else {
                error.into()
            }
        })?;
    retire_native::freeze_write_time(&file)?;
    let token = NativeWriterToken {
        selection,
        file: TagNativeFile::Native(file),
        _parents: parents,
        authority: None,
    };
    token.check()?;
    Ok(token)
}
#[cfg(not(windows))]
fn open_selected(_selection: TagWriteSelection) -> Result<NativeWriterToken> {
    anyhow::bail!(
        "UnsupportedCapability: exclusive same-object tag installation requires Windows actual namespace sharing"
    )
}
#[cfg(windows)]
fn pin_directory(path: &Path) -> Result<Vec<File>> {
    use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
        FILE_SHARE_READ, FILE_SHARE_WRITE,
    };
    validate_library_path(path)?;
    let normalized = normalize_local_path(path);
    ensure!(
        normalized.components().count() <= 256,
        "Tag namespace exceeds its component limit"
    );
    let mut chain = normalized
        .ancestors()
        .map(Path::to_path_buf)
        .collect::<Vec<_>>();
    chain.reverse();
    let mut pins = Vec::new();
    for directory in chain {
        let file = OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .open(&directory)?;
        let metadata = file.metadata()?;
        ensure!(
            metadata.is_dir() && metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT == 0,
            "Tag namespace contains a non-directory or reparse point"
        );
        ensure!(
            normalize_local_path(&actual_path(&file)?) == directory,
            "Selected tag directory differs from its actual namespace"
        );
        pins.push(file);
    }
    Ok(pins)
}
#[cfg(not(windows))]
fn pin_directory(_path: &Path) -> Result<Vec<File>> {
    anyhow::bail!("UnsupportedCapability: pinned tag namespace requires Windows")
}
#[cfg(windows)]
fn create_owned_file(path: &Path) -> Result<File> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .access_mode(
            windows_sys::Win32::Foundation::GENERIC_READ
                | windows_sys::Win32::Foundation::GENERIC_WRITE
                | windows_sys::Win32::Storage::FileSystem::DELETE,
        )
        .share_mode(0)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .create_new(true)
        .open(path)?;
    Ok(file)
}
#[cfg(not(windows))]
fn create_owned_file(_path: &Path) -> Result<File> {
    anyhow::bail!("UnsupportedCapability: owned tag artifacts require Windows")
}
#[cfg(windows)]
fn open_existing_owned(path: &Path) -> Result<TagNativeFile> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;
    let file = OpenOptions::new()
        .access_mode(
            windows_sys::Win32::Foundation::GENERIC_READ
                | windows_sys::Win32::Foundation::GENERIC_WRITE
                | windows_sys::Win32::Storage::FileSystem::DELETE,
        )
        .share_mode(0)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    ensure!(
        normal_file(&file)?
            && links(&file)? == 1
            && normalize_local_path(&actual_path(&file)?) == normalize_local_path(path),
        "Tag artifact namespace changed"
    );
    Ok(TagNativeFile::Native(file))
}
#[cfg(not(windows))]
fn open_existing_owned(_path: &Path) -> Result<TagNativeFile> {
    anyhow::bail!("UnsupportedCapability: tag recovery requires Windows")
}
#[cfg(windows)]
fn raw_normal_file(file: &File) -> Result<bool> {
    use std::os::windows::fs::MetadataExt;
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;
    let metadata = file.metadata()?;
    Ok(metadata.is_file() && metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT == 0)
}
#[cfg(not(windows))]
fn raw_normal_file(_file: &File) -> Result<bool> {
    anyhow::bail!("UnsupportedCapability: tag file identity")
}
#[cfg(windows)]
fn raw_actual_identity(file: &File) -> Result<FileIdentity> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_ID_INFO, FileIdInfo, GetFileInformationByHandleEx,
    };
    let mut info = FILE_ID_INFO::default();
    ensure!(
        unsafe {
            GetFileInformationByHandleEx(
                file.as_raw_handle(),
                FileIdInfo,
                &mut info as *mut _ as *mut _,
                std::mem::size_of_val(&info) as u32,
            )
        } != 0,
        "Cannot query tag writer actual file identity: {}",
        io::Error::last_os_error()
    );
    Ok(FileIdentity {
        kind: "windows_file_id".into(),
        volume: format!("{:016x}", info.VolumeSerialNumber),
        file_id: info
            .FileId
            .Identifier
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect(),
    })
}
#[cfg(not(windows))]
fn raw_actual_identity(_file: &File) -> Result<FileIdentity> {
    anyhow::bail!("UnsupportedCapability: tag file identity")
}
#[cfg(windows)]
fn raw_links(file: &File) -> Result<u32> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
    };
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    ensure!(
        unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) } != 0,
        "Cannot query tag hardlinks"
    );
    Ok(info.nNumberOfLinks)
}
#[cfg(not(windows))]
fn raw_links(_file: &File) -> Result<u32> {
    anyhow::bail!("UnsupportedCapability: tag hardlink identity")
}
#[cfg(windows)]
fn raw_actual_path(file: &File) -> Result<PathBuf> {
    use std::os::windows::{ffi::OsStringExt, io::AsRawHandle};
    use windows_sys::Win32::Storage::FileSystem::GetFinalPathNameByHandleW;
    let mut buffer = vec![0u16; 32768];
    let count = unsafe {
        GetFinalPathNameByHandleW(
            file.as_raw_handle(),
            buffer.as_mut_ptr(),
            buffer.len() as u32,
            0,
        )
    };
    ensure!(
        count > 0 && (count as usize) < buffer.len(),
        "Cannot query selected tag file actual path"
    );
    let path = PathBuf::from(std::ffi::OsString::from_wide(&buffer[..count as usize]));
    validate_library_path(&path)?;
    Ok(normalize_local_path(&path))
}
#[cfg(not(windows))]
fn raw_actual_path(_file: &File) -> Result<PathBuf> {
    anyhow::bail!("UnsupportedCapability: tag physical path")
}

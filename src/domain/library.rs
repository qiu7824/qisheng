//! Explicit local-library grants and bounded scan records.
use super::music::{LocalAsset, TrackRecord, normalize_local_path, stable_id};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

pub const MAX_LIBRARY_ROOTS: usize = 128;
pub const MAX_LIBRARY_ASSETS: usize = 50_000;
pub const MAX_LIBRARY_MEMBERSHIPS: usize = 200_000;
pub const MAX_LIBRARY_OPERATIONS: usize = 64;
pub const MAX_LIBRARY_BATCH_ITEMS: usize = 200;
pub const MAX_LIBRARY_BATCH_BYTES: usize = 1024 * 1024;
pub const MAX_LIBRARY_PAGE_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_LIBRARY_WARNINGS: usize = 256;
pub const MAX_LIBRARY_WARNING_BYTES: usize = 256 * 1024;
pub const MAX_LIBRARY_PATH_BYTES: usize = 16 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileIdentity {
    pub kind: String,
    pub volume: String,
    pub file_id: String,
}
impl FileIdentity {
    pub(crate) fn from_native_parts(device: u64, object: &str) -> Result<Self> {
        let (kind, file_id) = if cfg!(windows) {
            ensure!(
                object.len() == 32 && object.bytes().all(|byte| byte.is_ascii_hexdigit()),
                "Invalid Windows native file identity"
            );
            ("windows_file_id", object.to_ascii_lowercase())
        } else if cfg!(unix) {
            let inode = object
                .parse::<u64>()
                .map_err(|_| anyhow::anyhow!("Invalid Unix inode identity"))?;
            ("unix_inode", format!("{inode:016x}"))
        } else {
            anyhow::bail!("Native file identities are unavailable on this platform");
        };
        Ok(Self {
            kind: kind.into(),
            volume: format!("{device:016x}"),
            file_id,
        })
    }
    pub fn validate(&self) -> Result<()> {
        for (value, limit) in [(&self.kind, 64), (&self.volume, 128), (&self.file_id, 128)] {
            ensure!(
                !value.is_empty() && value.len() <= limit && !value.contains('\0'),
                LibraryError::invalid("Invalid physical file identity")
            );
        }
        Ok(())
    }
}
#[cfg(test)]
mod native_identity_tests {
    use super::*;
    #[test]
    fn native_download_identity_matches_library_encoding_and_rejects_malformed_objects() {
        let object = if cfg!(windows) {
            "0123456789abcdef0123456789abcdef"
        } else {
            "42"
        };
        let identity = FileIdentity::from_native_parts(24, object).unwrap();
        assert_eq!(identity.volume, "0000000000000018");
        assert_eq!(
            identity.file_id,
            if cfg!(windows) {
                object
            } else {
                "000000000000002a"
            }
        );
        assert_eq!(
            identity.kind,
            if cfg!(windows) {
                "windows_file_id"
            } else {
                "unix_inode"
            }
        );
        assert!(FileIdentity::from_native_parts(24, "invalid").is_err());
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LibraryPathPolicy {
    WindowsInsensitive,
    Sensitive,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LibraryRootDraft {
    pub path: PathBuf,
    pub recursive: bool,
    pub anchor: FileIdentity,
    pub path_policy: LibraryPathPolicy,
}
impl LibraryRootDraft {
    pub fn validate(&self) -> Result<()> {
        validate_library_path(&self.path)?;
        self.anchor.validate()
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LibraryRoot {
    pub root_id: String,
    pub path: PathBuf,
    pub recursive: bool,
    pub grant_epoch: u64,
    pub anchor: FileIdentity,
    pub path_policy: LibraryPathPolicy,
    pub watch_enabled: bool,
    pub generation: u64,
    pub last_complete_generation: u64,
}
impl LibraryRoot {
    pub fn validate(&self) -> Result<()> {
        validate_library_id(&self.root_id)?;
        validate_library_path(&self.path)?;
        self.anchor.validate()?;
        ensure!(
            self.grant_epoch > 0
                && self.grant_epoch <= i64::MAX as u64
                && self.generation <= i64::MAX as u64
                && self.last_complete_generation <= self.generation,
            LibraryError::invalid("Invalid library root generation")
        );
        Ok(())
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LibraryScanToken {
    pub root_id: String,
    pub grant_epoch: u64,
    pub generation: u64,
    pub operation_id: String,
}
impl LibraryScanToken {
    pub fn validate(&self) -> Result<()> {
        validate_library_id(&self.root_id)?;
        validate_library_id(&self.operation_id)?;
        ensure!(
            self.grant_epoch > 0
                && self.grant_epoch <= i64::MAX as u64
                && self.generation > 0
                && self.generation <= i64::MAX as u64,
            LibraryError::invalid("Invalid library scan token")
        );
        Ok(())
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LibraryScanOptions {
    pub max_files: usize,
    pub max_entries: usize,
    pub max_depth: usize,
    pub timeout_ms: u64,
}
impl Default for LibraryScanOptions {
    fn default() -> Self {
        Self {
            max_files: 50_000,
            max_entries: 250_000,
            max_depth: 64,
            timeout_ms: 120_000,
        }
    }
}
impl LibraryScanOptions {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            (1..=50_000).contains(&self.max_files)
                && (1..=250_000).contains(&self.max_entries)
                && self.max_depth <= 64
                && (1..=600_000).contains(&self.timeout_ms),
            LibraryError::invalid("Invalid library scan limits")
        );
        Ok(())
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LibraryOperationState {
    Running,
    Completed,
    Partial,
    Cancelled,
    Failed,
    Interrupted,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LibraryPhase {
    ValidatingRoot,
    Discovering,
    Preparing,
    Committing,
    Finalizing,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LibraryErrorKind {
    InvalidData,
    Conflict,
    Busy,
    Cancelled,
    Timeout,
    Storage,
    Unavailable,
    BudgetExceeded,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LibraryError {
    pub kind: LibraryErrorKind,
    pub message: String,
    pub retryable: bool,
}
impl LibraryError {
    pub fn new(kind: LibraryErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            retryable: matches!(
                kind,
                LibraryErrorKind::Busy | LibraryErrorKind::Timeout | LibraryErrorKind::Unavailable
            ),
        }
    }
    pub fn invalid(message: impl Into<String>) -> Self {
        Self::new(LibraryErrorKind::InvalidData, message)
    }
    pub fn conflict(message: impl Into<String>) -> Self {
        Self::new(LibraryErrorKind::Conflict, message)
    }
    pub fn budget(message: impl Into<String>) -> Self {
        Self::new(LibraryErrorKind::BudgetExceeded, message)
    }
}
impl std::fmt::Display for LibraryError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}
impl std::error::Error for LibraryError {}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LibraryWarning {
    pub path: PathBuf,
    pub reason: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LibraryOperationView {
    pub operation_id: String,
    pub root_id: Option<String>,
    pub generation: Option<u64>,
    pub state: LibraryOperationState,
    pub phase: LibraryPhase,
    pub entries_examined: usize,
    pub discovered_files: usize,
    pub prepared_files: usize,
    pub committed_files: usize,
    pub skipped_files: usize,
    pub changed_files: usize,
    pub missing_files: usize,
    pub warnings: Vec<LibraryWarning>,
    pub omitted_warnings: usize,
    pub error: Option<LibraryError>,
    pub repository_revision: u64,
}
impl LibraryOperationView {
    pub fn running(token: &LibraryScanToken) -> Self {
        Self {
            operation_id: token.operation_id.clone(),
            root_id: Some(token.root_id.clone()),
            generation: Some(token.generation),
            state: LibraryOperationState::Running,
            phase: LibraryPhase::Discovering,
            entries_examined: 0,
            discovered_files: 0,
            prepared_files: 0,
            committed_files: 0,
            skipped_files: 0,
            changed_files: 0,
            missing_files: 0,
            warnings: Vec::new(),
            omitted_warnings: 0,
            error: None,
            repository_revision: 0,
        }
    }
    pub fn validate(&self) -> Result<()> {
        validate_library_id(&self.operation_id)?;
        if let Some(root_id) = &self.root_id {
            validate_library_id(root_id)?;
        }
        ensure!(
            self.entries_examined <= 250_000
                && self.discovered_files <= MAX_LIBRARY_ASSETS
                && self.prepared_files <= MAX_LIBRARY_ASSETS
                && self.committed_files <= MAX_LIBRARY_ASSETS
                && self.skipped_files <= MAX_LIBRARY_ASSETS
                && self.changed_files <= MAX_LIBRARY_ASSETS
                && self.missing_files <= MAX_LIBRARY_ASSETS
                && self
                    .generation
                    .is_none_or(|value| value > 0 && value <= i64::MAX as u64)
                && self.repository_revision <= i64::MAX as u64,
            LibraryError::invalid("Invalid library operation counters")
        );
        ensure!(
            self.warnings.len() <= MAX_LIBRARY_WARNINGS,
            LibraryError::budget("Too many library scan warnings")
        );
        for warning in &self.warnings {
            validate_library_path(&warning.path)?;
            ensure!(
                warning.reason.chars().count() <= 512 && !warning.reason.contains('\0'),
                LibraryError::invalid("Invalid library warning text")
            );
        }
        ensure!(
            serde_json::to_vec(&self.warnings)?.len() <= MAX_LIBRARY_WARNING_BYTES,
            LibraryError::budget("Library scan warnings exceed 256 KiB")
        );
        if let Some(error) = &self.error {
            ensure!(
                error.message.len() <= 8192 && !error.message.contains('\0'),
                LibraryError::invalid("Invalid library error text")
            );
        }
        Ok(())
    }
}
#[derive(Clone, Debug, Serialize)]
pub struct LibraryPreparedAsset {
    pub track: TrackRecord,
    pub asset: LocalAsset,
    pub file_identity: Option<FileIdentity>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LibrarySeenFile {
    pub path: PathBuf,
    pub asset_id: Option<String>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocalAssetAuthority {
    pub asset_id: String,
    pub track_id: String,
    pub path: PathBuf,
    pub explicit_file_epoch: Option<u64>,
    pub roots: Vec<LibraryRoot>,
}
impl LocalAssetAuthority {
    pub fn permission_sha256(&self) -> Result<String> {
        validate_library_id(&self.asset_id)?;
        validate_library_id(&self.track_id)?;
        validate_library_path(&self.path)?;
        let mut roots = self.roots.clone();
        roots.sort_by(|a, b| a.root_id.cmp(&b.root_id));
        let root_grants = roots
            .iter()
            .map(|root| {
                (
                    &root.root_id,
                    root.grant_epoch,
                    &root.anchor,
                    root.recursive,
                )
            })
            .collect::<Vec<_>>();
        Ok(format!(
            "{:x}",
            Sha256::digest(serde_json::to_vec(&(
                "local-library-authority-v1",
                &self.asset_id,
                &self.track_id,
                &self.path,
                self.explicit_file_epoch,
                root_grants,
            ))?)
        ))
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LibraryAssetView {
    pub asset: LocalAsset,
    pub authorized: bool,
    pub root_ids: Vec<String>,
    pub explicit_file_grant: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LibraryRootPage {
    pub items: Vec<LibraryRoot>,
    pub total: usize,
    pub offset: usize,
    pub limit: usize,
    pub revision: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LibraryOperationPage {
    pub items: Vec<LibraryOperationView>,
    pub total: usize,
    pub offset: usize,
    pub limit: usize,
    pub revision: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LibraryAssetPage {
    pub items: Vec<LibraryAssetView>,
    pub total: usize,
    pub next_asset_id: Option<String>,
    pub revision: u64,
}
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LibrarySummary {
    pub roots: usize,
    pub authorized_assets: usize,
    pub running_operations: usize,
    pub latest_operation_id: Option<String>,
    pub revision: u64,
}

pub fn validate_library_id(value: &str) -> Result<()> {
    ensure!(
        !value.is_empty() && value.len() <= 1024 && !value.contains('\0'),
        LibraryError::invalid("Invalid library identity")
    );
    Ok(())
}
pub fn validate_library_path(path: &Path) -> Result<()> {
    let value = path
        .to_str()
        .ok_or_else(|| LibraryError::invalid("Library path is not valid Unicode"))?;
    ensure!(
        path.is_absolute() && value.len() <= MAX_LIBRARY_PATH_BYTES && !value.contains('\0'),
        LibraryError::invalid("Library path must be a bounded absolute path")
    );
    Ok(())
}
pub fn library_path_key(path: &Path, policy: LibraryPathPolicy) -> Result<String> {
    validate_library_path(path)?;
    let path = normalize_local_path(path);
    let text = path
        .to_str()
        .ok_or_else(|| LibraryError::invalid("Library path is not valid Unicode"))?;
    let identity = match policy {
        LibraryPathPolicy::WindowsInsensitive => text.to_lowercase(),
        LibraryPathPolicy::Sensitive => text.to_owned(),
    };
    Ok(stable_id("library-path", identity.as_bytes()))
}
pub fn native_library_path_policy() -> LibraryPathPolicy {
    if cfg!(windows) {
        LibraryPathPolicy::WindowsInsensitive
    } else {
        LibraryPathPolicy::Sensitive
    }
}

//! Watch intent, scoped discovery and stable local-file bindings.
use super::{library::*, music::normalize_local_path};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::path::{Component, PathBuf};

pub const MAX_RELINK_CANDIDATES: usize = 64;
pub const MAX_RELINK_DOCUMENT_BYTES: usize = 8 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RootMovePolicy {
    #[default]
    Pause,
    FollowSameObject,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LibraryWatchOptions {
    pub debounce_ms: u64,
    pub max_wait_ms: u64,
    pub root_move_policy: RootMovePolicy,
    pub auto_relink: bool,
}
impl Default for LibraryWatchOptions {
    fn default() -> Self {
        Self {
            debounce_ms: 250,
            max_wait_ms: 2000,
            root_move_policy: RootMovePolicy::Pause,
            auto_relink: false,
        }
    }
}
impl LibraryWatchOptions {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            (50..=2000).contains(&self.debounce_ms)
                && (self.debounce_ms..=10000).contains(&self.max_wait_ms),
            LibraryError::invalid("Invalid library watch timing")
        );
        Ok(())
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LibraryWatchPreference {
    pub root_id: String,
    pub enabled: bool,
    pub watch_generation: u64,
    pub options: LibraryWatchOptions,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LibraryWatchToken {
    pub root_id: String,
    pub grant_epoch: u64,
    pub watch_generation: u64,
    pub enabled: bool,
    pub options: LibraryWatchOptions,
}
impl LibraryWatchToken {
    pub fn validate(&self) -> Result<()> {
        validate_library_id(&self.root_id)?;
        self.options.validate()?;
        ensure!(
            self.grant_epoch > 0
                && self.grant_epoch <= i64::MAX as u64
                && self.watch_generation > 0
                && self.watch_generation <= i64::MAX as u64,
            LibraryError::invalid("Invalid library watch token")
        );
        Ok(())
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LibraryWatchState {
    Disabled,
    Starting,
    Watching,
    RescanPending,
    Degraded,
    Stopping,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LibraryWatchView {
    pub preference: LibraryWatchPreference,
    pub grant_epoch: u64,
    pub state: LibraryWatchState,
    pub received_sequence: u64,
    pub covered_sequence: u64,
    pub pending_paths: usize,
    pub full_rescan_required: bool,
    pub dropped_events: u64,
    pub last_scan_operation_id: Option<String>,
    pub error: Option<LibraryError>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LibraryWatchPage {
    pub items: Vec<LibraryWatchPreference>,
    pub total: usize,
    pub offset: usize,
    pub limit: usize,
    pub revision: u64,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LibraryDirtyState {
    pub root_id: String,
    pub watch_generation: u64,
    pub received_sequence: u64,
    pub covered_sequence: u64,
    pub full_rescan_required: bool,
}
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "scope", rename_all = "snake_case", deny_unknown_fields)]
pub enum LibraryScanScope {
    #[default]
    Root,
    Paths {
        paths: Vec<PathBuf>,
    },
    Subtree {
        path: PathBuf,
    },
}
impl LibraryScanScope {
    pub fn validate(&self) -> Result<()> {
        let paths: &[PathBuf] = match self {
            Self::Root => return Ok(()),
            Self::Paths { paths } => {
                ensure!(
                    !paths.is_empty() && paths.len() <= MAX_LIBRARY_BATCH_ITEMS,
                    LibraryError::invalid("Invalid incremental path count")
                );
                paths
            }
            Self::Subtree { path } => std::slice::from_ref(path),
        };
        for path in paths {
            let text = path
                .to_str()
                .ok_or_else(|| LibraryError::invalid("Incremental path is not Unicode"))?;
            ensure!(
                !text.is_empty()
                    && text.len() <= MAX_LIBRARY_PATH_BYTES
                    && !text.contains('\0')
                    && path
                        .components()
                        .all(|part| matches!(part, Component::Normal(_))),
                LibraryError::invalid("Incremental path must contain safe relative components")
            );
        }
        ensure!(
            serde_json::to_vec(self)?.len() <= MAX_LIBRARY_BATCH_BYTES,
            LibraryError::budget("Incremental path scope exceeds 1 MiB")
        );
        Ok(())
    }
    pub fn contains(&self, root: &LibraryRoot, path: &std::path::Path) -> bool {
        let normalize = |path: &std::path::Path| {
            let path = normalize_local_path(path);
            match root.path_policy {
                LibraryPathPolicy::Sensitive => path,
                LibraryPathPolicy::WindowsInsensitive => path
                    .to_str()
                    .map_or(path.clone(), |text| PathBuf::from(text.to_lowercase())),
            }
        };
        let path = normalize(path);
        match self {
            Self::Root => true,
            Self::Paths { paths } => paths
                .iter()
                .any(|item| normalize(&root.path.join(item)) == path),
            Self::Subtree { path: subtree } => {
                path.starts_with(normalize(&root.path.join(subtree)))
            }
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LibraryAssetBinding {
    pub asset_id: String,
    pub path: PathBuf,
    pub binding_revision: u64,
    pub file_identity: Option<FileIdentity>,
    pub content_sha256: Option<String>,
    pub hash_size_bytes: Option<u64>,
    pub hash_modified_ms: Option<u64>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LibraryRootBinding {
    pub root_id: String,
    pub path: PathBuf,
    pub location_revision: u64,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RelinkEvidence {
    SameOpenObject,
    SameFileAndContent,
    ManualSelection,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LibraryRelinkCandidate {
    pub candidate_id: String,
    pub asset_id: String,
    pub root_id: String,
    pub grant_epoch: u64,
    pub watch_generation: u64,
    pub old_path: PathBuf,
    pub new_path: PathBuf,
    pub new_identity: FileIdentity,
    pub content_sha256: Option<String>,
    pub binding_revision: u64,
    pub evidence: RelinkEvidence,
    pub requires_selection: bool,
    pub expires_at_ms: u64,
}
impl LibraryRelinkCandidate {
    pub fn validate(&self) -> Result<()> {
        for value in [&self.candidate_id, &self.asset_id, &self.root_id] {
            validate_library_id(value)?;
        }
        validate_library_path(&self.old_path)?;
        validate_library_path(&self.new_path)?;
        self.new_identity.validate()?;
        ensure!(
            self.grant_epoch > 0
                && self.grant_epoch <= i64::MAX as u64
                && self.watch_generation <= i64::MAX as u64
                && self.binding_revision > 0
                && self.binding_revision <= i64::MAX as u64
                && self.expires_at_ms > 0
                && self.expires_at_ms <= i64::MAX as u64,
            LibraryError::invalid("Invalid relink candidate token")
        );
        if let Some(hash) = &self.content_sha256 {
            validate_content_sha256(hash)?;
        }
        Ok(())
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LibraryRelinkCandidatePage {
    pub items: Vec<LibraryRelinkCandidate>,
    pub total: usize,
    pub next_candidate_id: Option<String>,
    pub revision: u64,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LibraryRelinkAuthorization {
    Manual,
    Root {
        root_id: String,
        grant_epoch: u64,
        watch_generation: u64,
    },
}
#[derive(Clone, Debug)]
pub struct LibraryRelinkCommit {
    pub operation_id: String,
    pub expected_binding_revision: u64,
    pub prepared: LibraryPreparedAsset,
    pub authorization: LibraryRelinkAuthorization,
    pub evidence: RelinkEvidence,
    pub content_sha256: Option<String>,
    pub allow_content_change: bool,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LibraryRelinkResult {
    pub operation_id: String,
    pub asset_id: String,
    pub track_id: String,
    pub binding_revision: u64,
    pub old_path: PathBuf,
    pub new_path: PathBuf,
    pub media_changed: bool,
    pub explicit_grant_renewed: bool,
    pub revision: u64,
}
pub fn validate_content_sha256(value: &str) -> Result<()> {
    ensure!(
        value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit()),
        LibraryError::invalid("Content evidence requires SHA-256")
    );
    Ok(())
}

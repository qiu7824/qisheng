//! Persistent artifact facts do not restore source or filesystem authority.

use super::{DownloadContext, FileIdentity, MAX_DOWNLOAD_BYTES, OwnedPartRecord};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::path::{Component, PathBuf};

mod creation_intent;
pub use creation_intent::{CreationJournalRequired, OwnedArtifactIntent};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OwnedArtifactKind {
    Transfer,
    TagCopy,
    LyricSidecar,
    RollbackOriginal,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OwnedArtifactAllocation {
    pub kind: OwnedArtifactKind,
    pub record: OwnedPartRecord,
    pub working_directory_path: PathBuf,
    pub working_directory_identity: FileIdentity,
    pub selected_root_identity: FileIdentity,
    pub relative_group: Option<String>,
    pub reserved_max_bytes: u64,
    pub context: DownloadContext,
}
impl OwnedArtifactAllocation {
    pub fn validate(&self) -> Result<()> {
        self.context.validate()?;
        ensure!(
            self.working_directory_path.is_absolute()
                && self
                    .working_directory_path
                    .components()
                    .all(|component| !matches!(
                        component,
                        Component::ParentDir | Component::CurDir
                    ))
                && self
                    .working_directory_path
                    .as_os_str()
                    .to_string_lossy()
                    .encode_utf16()
                    .count()
                    <= 32000,
            "Invalid owned artifact directory fact"
        );
        for identity in [
            &self.record.identity,
            &self.working_directory_identity,
            &self.selected_root_identity,
        ] {
            ensure!(
                !identity.object.is_empty()
                    && identity.object.len() <= 128
                    && identity.object.bytes().all(|byte| byte.is_ascii_hexdigit()),
                "Invalid owned artifact physical identity fact"
            );
        }
        ensure!(
            self.record.destination_identity == self.selected_root_identity
                && self.record.destination_epoch > 0,
            "Owned artifact selected-root fact differs from its record"
        );
        ensure!(
            self.record.nonce.len() == 32
                && self
                    .record
                    .nonce
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit())
                && self.record.basename == format!(".lx-download-{}.part", self.record.nonce),
            "Invalid owned artifact nonce fact"
        );
        super::validate_basename(&self.record.final_basename)?;
        if let Some(group) = &self.relative_group {
            super::validate_basename(group)?;
        }
        ensure!(
            self.reserved_max_bytes > 0 && self.reserved_max_bytes <= MAX_DOWNLOAD_BYTES,
            "Owned artifact reservation exceeds its bound"
        );
        if self.kind == OwnedArtifactKind::LyricSidecar {
            ensure!(
                self.record.final_basename.ends_with(".lrc")
                    && self.reserved_max_bytes
                        <= super::lyrics::MAX_DOWNLOAD_LYRIC_OUTPUT_BYTES as u64 + 3,
                "Lyric artifact reservation differs from its role"
            );
        }
        Ok(())
    }
}

/// Called by the actual worker with its original zero-length file still open.
/// A successful return acknowledges durable ownership before the first write.
pub trait OwnedArtifactJournal: Send + Sync {
    fn record_intent(&self, _intent: &OwnedArtifactIntent) -> Result<()> {
        Err(CreationJournalRequired.into())
    }
    fn record_reopened(&self, _allocation: &OwnedArtifactAllocation) -> Result<()> {
        Err(CreationJournalRequired.into())
    }
    fn record_created(&self, allocation: &OwnedArtifactAllocation) -> Result<()>;
}

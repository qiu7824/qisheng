//! Durable namespace intent contains no guessed file identity.

use super::*;

#[derive(Debug)]
pub struct CreationJournalRequired;
impl std::fmt::Display for CreationJournalRequired {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("Download allocation requires a durable task creation journal")
    }
}
impl std::error::Error for CreationJournalRequired {}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OwnedArtifactIntent {
    pub kind: OwnedArtifactKind,
    pub nonce: String,
    pub basename: String,
    pub final_basename: String,
    pub working_directory_path: PathBuf,
    pub working_directory_identity: FileIdentity,
    pub selected_root_identity: FileIdentity,
    pub destination_epoch: u64,
    pub relative_group: Option<String>,
    pub reserved_max_bytes: u64,
    pub context: DownloadContext,
}

impl OwnedArtifactIntent {
    pub fn validate(&self) -> Result<()> {
        self.context.validate()?;
        ensure!(
            self.nonce.len() == 32
                && self.nonce.bytes().all(|byte| byte.is_ascii_hexdigit())
                && self.basename == format!(".lx-download-{}.part", self.nonce),
            "Invalid download allocation intent nonce"
        );
        super::super::validate_basename(&self.final_basename)?;
        if let Some(group) = &self.relative_group {
            super::super::validate_basename(group)?;
        }
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
            "Invalid download allocation intent namespace"
        );
        for identity in [
            &self.working_directory_identity,
            &self.selected_root_identity,
        ] {
            ensure!(
                !identity.object.is_empty()
                    && identity.object.len() <= 128
                    && identity.object.bytes().all(|byte| byte.is_ascii_hexdigit()),
                "Invalid actual selected directory identity in allocation intent"
            );
        }
        ensure!(
            self.destination_epoch > 0
                && self.reserved_max_bytes > 0
                && self.reserved_max_bytes <= MAX_DOWNLOAD_BYTES,
            "Invalid download allocation intent reservation or epoch"
        );
        if self.kind == OwnedArtifactKind::LyricSidecar {
            ensure!(
                self.final_basename.ends_with(".lrc")
                    && self.reserved_max_bytes
                        <= super::super::lyrics::MAX_DOWNLOAD_LYRIC_OUTPUT_BYTES as u64 + 3,
                "Lyric allocation intent exceeds its role bound"
            );
        }
        Ok(())
    }

    pub fn require_created(&self, allocation: &OwnedArtifactAllocation) -> Result<()> {
        self.validate()?;
        allocation.validate()?;
        ensure!(
            self == &Self::from_created(allocation),
            "Actual creation acknowledgement differs from its durable namespace intent"
        );
        Ok(())
    }

    pub fn from_created(allocation: &OwnedArtifactAllocation) -> Self {
        Self {
            kind: allocation.kind,
            nonce: allocation.record.nonce.clone(),
            basename: allocation.record.basename.clone(),
            final_basename: allocation.record.final_basename.clone(),
            working_directory_path: allocation.working_directory_path.clone(),
            working_directory_identity: allocation.working_directory_identity.clone(),
            selected_root_identity: allocation.selected_root_identity.clone(),
            destination_epoch: allocation.record.destination_epoch,
            relative_group: allocation.relative_group.clone(),
            reserved_max_bytes: allocation.reserved_max_bytes,
            context: allocation.context.clone(),
        }
    }
}

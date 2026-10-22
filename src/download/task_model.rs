//! Persisted download documents never restore live execution or file authority.

use super::{
    CollisionPolicy, DownloadContext, DownloadError, DownloadState, FileIdentity,
    MAX_DOWNLOAD_BYTES, MAX_DOWNLOAD_TASKS, OwnedPartRecord, TransferCheckpoint,
    lyrics::{DownloadLyricOptions, LyricArtifactEncoding},
    planning::{DownloadQuality, FilenameFormat},
};
use crate::domain::music::TrackRecord;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{collections::BTreeSet, fmt, path::PathBuf};

pub const MAX_DOWNLOAD_TASK_DOCUMENT_BYTES: usize = 256 * 1024;
pub const MAX_DOWNLOAD_DOCUMENT_BYTES: usize = 16 * 1024 * 1024;
const MAX_ERROR_BYTES: usize = 4096;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DownloadSettings {
    pub enabled: bool,
    pub destination_grant_id: Option<String>,
    pub group_by_list: bool,
    pub filename_format: FilenameFormat,
    pub max_concurrent: usize,
    pub skip_existing: bool,
    pub download_lyrics: bool,
    pub sidecar_lyrics: DownloadLyricOptions,
    pub lyric_encoding: LyricArtifactEncoding,
    pub embed_cover: bool,
    pub embed_lyrics: bool,
    pub embedded_lyrics: DownloadLyricOptions,
    pub allow_other_source: bool,
}
impl Default for DownloadSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            destination_grant_id: None,
            group_by_list: false,
            filename_format: FilenameFormat::TitleArtist,
            max_concurrent: 3,
            skip_existing: true,
            download_lyrics: false,
            sidecar_lyrics: DownloadLyricOptions::default(),
            lyric_encoding: LyricArtifactEncoding::Utf8,
            embed_cover: true,
            embed_lyrics: false,
            embedded_lyrics: DownloadLyricOptions::default(),
            allow_other_source: false,
        }
    }
}
impl DownloadSettings {
    pub fn validate(&self) -> Result<(), DownloadDocumentError> {
        if !(1..=4).contains(&self.max_concurrent) {
            return Err(DownloadDocumentError::InvalidBounds);
        }
        if let Some(id) = &self.destination_grant_id {
            identifier(id)?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StoredDestinationGrant {
    pub id: String,
    pub profile_id: String,
    pub path: PathBuf,
    pub identity: FileIdentity,
    pub epoch: u64,
    pub revision: u64,
    pub revoked: bool,
}
impl StoredDestinationGrant {
    pub fn validate(&self) -> Result<(), DownloadDocumentError> {
        identifier(&self.id)?;
        identifier(&self.profile_id)?;
        identity(&self.identity)?;
        if !self.path.is_absolute()
            || self
                .path
                .as_os_str()
                .to_string_lossy()
                .encode_utf16()
                .count()
                > 32000
            || self.epoch == 0
            || self.revision == 0
        {
            return Err(DownloadDocumentError::InvalidBinding);
        }
        Ok(())
    }
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DownloadDestinationBinding {
    pub grant_id: String,
    pub grant_epoch: u64,
    pub root_identity: FileIdentity,
    pub relative_components: Vec<String>,
    pub collision: CollisionPolicy,
}
impl DownloadDestinationBinding {
    pub fn validate(&self) -> Result<(), DownloadDocumentError> {
        identifier(&self.grant_id)?;
        identity(&self.root_identity)?;
        if self.grant_epoch == 0 || !(1..=2).contains(&self.relative_components.len()) {
            return Err(DownloadDocumentError::InvalidBinding);
        }
        for component in &self.relative_components {
            super::validate_basename(component)
                .map_err(|_| DownloadDocumentError::InvalidBinding)?;
        }
        if let CollisionPolicy::Overwrite { observed } = &self.collision {
            identity(observed)?;
        }
        Ok(())
    }
}

pub type RepresentationProvenance = super::DownloadRepresentationIdentity;

/// Private repository state. Query views never serialize this document.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PrivateUrlBookmark {
    exact_url: String,
    pub sha256: String,
    pub context: DownloadContext,
}
impl PrivateUrlBookmark {
    pub fn new(exact_url: String, context: DownloadContext) -> Result<Self, DownloadDocumentError> {
        context
            .validate()
            .map_err(|_| DownloadDocumentError::InvalidBinding)?;
        if exact_url.len() > 2048
            || exact_url
                .chars()
                .any(|character| character.is_control() || character.is_whitespace())
        {
            return Err(DownloadDocumentError::InvalidUrl);
        }
        let url = reqwest::Url::parse(&exact_url).map_err(|_| DownloadDocumentError::InvalidUrl)?;
        if !matches!(url.scheme(), "http" | "https")
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
        {
            return Err(DownloadDocumentError::InvalidUrl);
        }
        let sha256 = format!("{:x}", Sha256::digest(exact_url.as_bytes()));
        Ok(Self {
            exact_url,
            sha256,
            context,
        })
    }
    pub fn validate(&self) -> Result<(), DownloadDocumentError> {
        let parsed = Self::new(self.exact_url.clone(), self.context.clone())?;
        if parsed.sha256 != self.sha256 {
            return Err(DownloadDocumentError::InvalidUrl);
        }
        Ok(())
    }
    pub fn exact_url_for_local_approval(&self) -> &str {
        &self.exact_url
    }
}
impl fmt::Debug for PrivateUrlBookmark {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PrivateUrlBookmark")
            .field("sha256", &self.sha256)
            .field("context", &self.context)
            .finish()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DownloadPauseReason {
    User,
    Restart,
    SourceReapproval,
    DestinationReapproval,
    UrlReapproval,
    Quota,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DownloadStageState {
    Disabled,
    Pending,
    Running,
    Succeeded,
    Failed,
    SkippedUnsupportedBaseline,
    SkippedUnavailable,
}
impl DownloadStageState {
    fn satisfied(self) -> bool {
        matches!(
            self,
            Self::Disabled
                | Self::Succeeded
                | Self::SkippedUnsupportedBaseline
                | Self::SkippedUnavailable
        )
    }
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DownloadStageRecord {
    pub state: DownloadStageState,
    pub attempts: u8,
    pub error: Option<DownloadError>,
    pub artifact: Option<DownloadArtifactProof>,
}
impl DownloadStageRecord {
    pub fn disabled() -> Self {
        Self {
            state: DownloadStageState::Disabled,
            attempts: 0,
            error: None,
            artifact: None,
        }
    }
    pub fn pending() -> Self {
        Self {
            state: DownloadStageState::Pending,
            ..Self::disabled()
        }
    }
    fn validate(&self) -> Result<(), DownloadDocumentError> {
        if self.attempts > 3
            || self
                .error
                .as_ref()
                .is_some_and(|error| error.message.len() > MAX_ERROR_BYTES)
        {
            return Err(DownloadDocumentError::InvalidBounds);
        }
        if let Some(artifact) = &self.artifact {
            artifact.validate()?;
        }
        if self.state == DownloadStageState::Failed && self.error.is_none() {
            return Err(DownloadDocumentError::InvalidState);
        }
        Ok(())
    }
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DownloadStageRecords {
    pub metadata: DownloadStageRecord,
    pub cover: DownloadStageRecord,
    pub embedded_lyrics: DownloadStageRecord,
    pub lyrics: DownloadStageRecord,
}
impl DownloadStageRecords {
    pub fn for_settings(settings: &DownloadSettings, quality: DownloadQuality) -> Self {
        let mut metadata = DownloadStageRecord::pending();
        if matches!(quality, DownloadQuality::Wav | DownloadQuality::Ape) {
            metadata.state = DownloadStageState::SkippedUnsupportedBaseline;
        }
        let embed_supported = !matches!(quality, DownloadQuality::Wav | DownloadQuality::Ape);
        let cover = if settings.embed_cover && embed_supported {
            DownloadStageRecord::pending()
        } else if settings.embed_cover {
            DownloadStageRecord {
                state: DownloadStageState::SkippedUnsupportedBaseline,
                ..DownloadStageRecord::disabled()
            }
        } else {
            DownloadStageRecord::disabled()
        };
        let embedded_lyrics = if settings.embed_lyrics && embed_supported {
            DownloadStageRecord::pending()
        } else if settings.embed_lyrics {
            DownloadStageRecord {
                state: DownloadStageState::SkippedUnsupportedBaseline,
                ..DownloadStageRecord::disabled()
            }
        } else {
            DownloadStageRecord::disabled()
        };
        let lyrics = if settings.download_lyrics {
            DownloadStageRecord::pending()
        } else {
            DownloadStageRecord::disabled()
        };
        Self {
            metadata,
            cover,
            embedded_lyrics,
            lyrics,
        }
    }
    pub fn all_satisfied(&self) -> bool {
        self.metadata.state.satisfied()
            && self.cover.state.satisfied()
            && self.embedded_lyrics.state.satisfied()
            && self.lyrics.state.satisfied()
    }
    pub(crate) fn validate(&self) -> Result<(), DownloadDocumentError> {
        self.metadata.validate()?;
        self.cover.validate()?;
        self.embedded_lyrics.validate()?;
        self.lyrics.validate()
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DownloadArtifactProof {
    pub basename: String,
    pub identity: FileIdentity,
    pub bytes: u64,
    pub sha256: String,
}
impl DownloadArtifactProof {
    pub fn validate(&self) -> Result<(), DownloadDocumentError> {
        super::validate_basename(&self.basename)
            .map_err(|_| DownloadDocumentError::InvalidBinding)?;
        identity(&self.identity)?;
        digest(&self.sha256)?;
        if self.bytes == 0 || self.bytes > MAX_DOWNLOAD_BYTES {
            return Err(DownloadDocumentError::InvalidBounds);
        }
        Ok(())
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DownloadCommitPhase {
    Prepared,
    Verified,
    Replacing,
    AudioPublished,
    RepositoryCommitted,
    PostprocessRetry,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DownloadCommitJournal {
    pub task_revision: u64,
    pub operation_epoch: u64,
    pub grant_epoch: u64,
    pub phase: DownloadCommitPhase,
    pub original_part: OwnedPartRecord,
    pub candidate: DownloadArtifactProof,
    pub observed_target: Option<DownloadArtifactProof>,
    pub rollback_original: Option<DownloadArtifactProof>,
    pub sidecar: Option<DownloadArtifactProof>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry: Option<super::postprocess_retry::DownloadPostprocessRetryIntent>,
}
impl DownloadCommitJournal {
    fn validate(&self) -> Result<(), DownloadDocumentError> {
        if self.task_revision == 0 || self.operation_epoch == 0 || self.grant_epoch == 0 {
            return Err(DownloadDocumentError::InvalidBinding);
        }
        owned_part(&self.original_part)?;
        self.candidate.validate()?;
        for artifact in [
            &self.observed_target,
            &self.rollback_original,
            &self.sidecar,
        ]
        .into_iter()
        .flatten()
        {
            artifact.validate()?;
        }
        if let Some(retry) = &self.retry {
            retry
                .validate()
                .map_err(|_| DownloadDocumentError::InvalidBinding)?;
            if self.phase != DownloadCommitPhase::PostprocessRetry
                || retry.task_revision != self.task_revision
                || retry.operation_epoch != self.operation_epoch
            {
                return Err(DownloadDocumentError::InvalidBinding);
            }
        } else if self.phase == DownloadCommitPhase::PostprocessRetry {
            return Err(DownloadDocumentError::InvalidBinding);
        }
        Ok(())
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DownloadTask {
    pub id: String,
    pub profile_id: String,
    pub original_record: TrackRecord,
    pub resolved_record: Option<TrackRecord>,
    pub requested_quality: DownloadQuality,
    pub actual_quality: DownloadQuality,
    pub list_id: Option<String>,
    pub list_name_snapshot: Option<String>,
    pub order: u32,
    pub revision: u64,
    pub operation_epoch: u64,
    pub settings: DownloadSettings,
    pub destination: DownloadDestinationBinding,
    pub state: DownloadState,
    pub pause_reason: Option<DownloadPauseReason>,
    pub owned_part: Option<OwnedPartRecord>,
    pub original_transfer: Option<TransferCheckpoint>,
    pub original_representation: Option<RepresentationProvenance>,
    pub url_bookmark: Option<PrivateUrlBookmark>,
    pub transfer_attempts: u8,
    pub retry_at_ms: Option<u64>,
    pub error: Option<DownloadError>,
    pub output_audio: Option<DownloadArtifactProof>,
    pub audio_available: bool,
    pub stages: DownloadStageRecords,
    pub commit_journal: Option<DownloadCommitJournal>,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
}
impl DownloadTask {
    pub fn deduplication_key(&self) -> String {
        format!(
            "{}_{}_{}",
            self.original_record.lx_id,
            self.actual_quality.as_str(),
            self.actual_quality.extension()
        )
    }
    pub fn require_current_destination(
        &self,
        grant: &StoredDestinationGrant,
    ) -> Result<(), DownloadDocumentError> {
        grant.validate()?;
        if grant.revoked
            || grant.id != self.destination.grant_id
            || grant.profile_id != self.profile_id
            || grant.epoch != self.destination.grant_epoch
            || grant.identity != self.destination.root_identity
        {
            return Err(DownloadDocumentError::InvalidBinding);
        }
        Ok(())
    }
    pub fn validate(&self) -> Result<(), DownloadDocumentError> {
        identifier(&self.id)?;
        identifier(&self.profile_id)?;
        identifier(&self.original_record.id)?;
        identifier(&self.original_record.lx_id)?;
        self.settings.validate()?;
        self.destination.validate()?;
        self.stages.validate()?;
        let planned_leaf = self
            .destination
            .relative_components
            .last()
            .ok_or(DownloadDocumentError::InvalidBinding)?;
        let selected = DownloadStageRecords::for_settings(&self.settings, self.actual_quality);
        for (actual, expected) in [
            (&self.stages.metadata, &selected.metadata),
            (&self.stages.cover, &selected.cover),
            (&self.stages.embedded_lyrics, &selected.embedded_lyrics),
            (&self.stages.lyrics, &selected.lyrics),
        ] {
            let valid = match expected.state {
                DownloadStageState::Disabled => actual.state == DownloadStageState::Disabled,
                DownloadStageState::SkippedUnsupportedBaseline => {
                    actual.state == DownloadStageState::SkippedUnsupportedBaseline
                }
                _ => !matches!(
                    actual.state,
                    DownloadStageState::Disabled | DownloadStageState::SkippedUnsupportedBaseline
                ),
            };
            if !valid {
                return Err(DownloadDocumentError::InvalidState);
            }
        }
        if self.revision == 0
            || self.operation_epoch == 0
            || self.order >= MAX_DOWNLOAD_TASKS as u32
            || self.transfer_attempts > 3
            || self.updated_at_ms < self.created_at_ms
            || self
                .error
                .as_ref()
                .is_some_and(|error| error.message.len() > MAX_ERROR_BYTES)
        {
            return Err(DownloadDocumentError::InvalidBounds);
        }
        if let Some(id) = &self.list_id {
            identifier(id)?;
        }
        if self
            .list_name_snapshot
            .as_ref()
            .is_some_and(|name| name.len() > 64 * 1024 || name.contains('\0'))
        {
            return Err(DownloadDocumentError::InvalidBounds);
        }
        if let Some(part) = &self.owned_part {
            owned_part(part)?;
            if part.destination_epoch != self.destination.grant_epoch
                || part.destination_identity != self.destination.root_identity
                || Some(&part.final_basename) != self.destination.relative_components.last()
            {
                return Err(DownloadDocumentError::InvalidBinding);
            }
        }
        if let Some(checkpoint) = &self.original_transfer {
            digest(&checkpoint.sha256)?;
            if checkpoint.bytes > MAX_DOWNLOAD_BYTES
                || checkpoint
                    .remote_total
                    .is_some_and(|total| total > MAX_DOWNLOAD_BYTES || total < checkpoint.bytes)
                || checkpoint.strong_etag.as_ref().is_some_and(|etag| {
                    etag.len() < 2
                        || etag.len() > 1024
                        || !etag.starts_with('"')
                        || !etag.ends_with('"')
                        || etag.chars().any(char::is_control)
                })
            {
                return Err(DownloadDocumentError::InvalidBounds);
            }
            if checkpoint.bytes > 0
                && (self.owned_part.is_none() && self.output_audio.is_none()
                    || self.original_representation.is_none())
            {
                return Err(DownloadDocumentError::InvalidBinding);
            }
            if checkpoint.representation != self.original_representation {
                return Err(DownloadDocumentError::InvalidBinding);
            }
        }
        if let Some(provenance) = &self.original_representation {
            provenance
                .validate()
                .map_err(|_| DownloadDocumentError::InvalidBinding)?;
            if provenance.task_id != self.id || provenance.profile_id != self.profile_id {
                return Err(DownloadDocumentError::InvalidBinding);
            }
        }
        if let Some(url) = &self.url_bookmark {
            url.validate()?;
            if url.context.task_id != self.id
                || url.context.profile_id != self.profile_id
                || url.context.operation_epoch > self.operation_epoch
            {
                return Err(DownloadDocumentError::InvalidBinding);
            }
        }
        if let Some(output) = &self.output_audio {
            output.validate()?;
            if &output.basename != planned_leaf {
                return Err(DownloadDocumentError::InvalidBinding);
            }
        }
        if self.audio_available && self.output_audio.is_none()
            || self.state == DownloadState::Completed
                && (self.output_audio.is_none() || !self.stages.all_satisfied())
        {
            return Err(DownloadDocumentError::InvalidState);
        }
        if let Some(journal) = &self.commit_journal {
            journal.validate()?;
            if journal.operation_epoch > self.operation_epoch
                || journal.grant_epoch != self.destination.grant_epoch
                || journal.original_part.destination_epoch != self.destination.grant_epoch
                || journal.original_part.destination_identity != self.destination.root_identity
                || &journal.original_part.final_basename != planned_leaf
            {
                return Err(DownloadDocumentError::InvalidBinding);
            }
            if self
                .owned_part
                .as_ref()
                .is_some_and(|part| part != &journal.original_part)
            {
                return Err(DownloadDocumentError::InvalidBinding);
            }
            if let Some(retry) = &journal.retry {
                if !self.audio_available
                    || self.output_audio.as_ref()
                        != Some(
                            retry
                                .installed_audio
                                .as_ref()
                                .unwrap_or(&retry.original_audio),
                        )
                    || &retry.original_audio.basename != planned_leaf
                    || journal.observed_target.as_ref() != Some(&retry.original_audio)
                    || journal.rollback_original.is_some()
                {
                    return Err(DownloadDocumentError::InvalidBinding);
                }
                for artifact in &retry.artifacts {
                    if let Some(allocation) = &artifact.allocation
                        && (allocation.context.profile_id != self.profile_id
                            || allocation.context.task_id != self.id
                            || allocation.context.operation_epoch != retry.operation_epoch
                            || allocation.selected_root_identity != self.destination.root_identity
                            || allocation.record.destination_epoch != self.destination.grant_epoch)
                    {
                        return Err(DownloadDocumentError::InvalidBinding);
                    }
                }
            } else {
                let published = matches!(
                    journal.phase,
                    DownloadCommitPhase::AudioPublished | DownloadCommitPhase::RepositoryCommitted
                );
                if published {
                    if &journal.candidate.basename != planned_leaf {
                        return Err(DownloadDocumentError::InvalidBinding);
                    }
                } else if !owned_candidate_name(&journal.candidate.basename) {
                    return Err(DownloadDocumentError::InvalidBinding);
                }
                match (&self.destination.collision, &journal.observed_target) {
                    (CollisionPolicy::Skip, None) => {
                        if journal.rollback_original.is_some() {
                            return Err(DownloadDocumentError::InvalidBinding);
                        }
                    }
                    (CollisionPolicy::Overwrite { observed }, Some(target))
                        if &target.identity == observed && &target.basename == planned_leaf => {}
                    _ => return Err(DownloadDocumentError::InvalidBinding),
                }
                if let Some(rollback) = &journal.rollback_original {
                    let target = journal
                        .observed_target
                        .as_ref()
                        .ok_or(DownloadDocumentError::InvalidBinding)?;
                    if !owned_rollback_name(&rollback.basename)
                        || rollback.sha256 != target.sha256
                        || rollback.bytes != target.bytes
                    {
                        return Err(DownloadDocumentError::InvalidBinding);
                    }
                }
            }
            if let Some(sidecar) = &journal.sidecar {
                let stem = planned_leaf
                    .rsplit_once('.')
                    .map_or(planned_leaf.as_str(), |(stem, _)| stem);
                if !self.settings.download_lyrics
                    || sidecar.basename != format!("{stem}.lrc")
                    || sidecar.bytes > super::lyrics::MAX_DOWNLOAD_LYRIC_OUTPUT_BYTES as u64 + 3
                {
                    return Err(DownloadDocumentError::InvalidBinding);
                }
            }
        }
        let bytes = serde_json::to_vec(self)
            .map_err(|_| DownloadDocumentError::InvalidDocument)?
            .len();
        if bytes > MAX_DOWNLOAD_TASK_DOCUMENT_BYTES {
            return Err(DownloadDocumentError::DocumentBound);
        }
        Ok(())
    }
    pub fn after_restart(&mut self, timestamp_ms: u64) -> Result<(), DownloadDocumentError> {
        self.validate()?;
        if !matches!(
            self.state,
            DownloadState::Completed
                | DownloadState::Failed
                | DownloadState::Cancelled
                | DownloadState::Paused
        ) {
            let epoch = self
                .operation_epoch
                .checked_add(1)
                .ok_or(DownloadDocumentError::EpochExhausted)?;
            let revision = self
                .revision
                .checked_add(1)
                .ok_or(DownloadDocumentError::EpochExhausted)?;
            self.operation_epoch = epoch;
            self.revision = revision;
            self.state = DownloadState::Paused;
            self.pause_reason = Some(DownloadPauseReason::Restart);
            self.retry_at_ms = None;
            for stage in [
                &mut self.stages.metadata,
                &mut self.stages.cover,
                &mut self.stages.embedded_lyrics,
                &mut self.stages.lyrics,
            ] {
                if stage.state == DownloadStageState::Running {
                    stage.state = DownloadStageState::Pending;
                }
            }
            self.updated_at_ms = self.updated_at_ms.max(timestamp_ms);
        }
        self.validate()
    }
    /// Foreign imports retain metadata only and require a fresh destination plan.
    pub fn discard_imported_authority(&mut self) {
        self.state = DownloadState::Paused;
        self.pause_reason = Some(DownloadPauseReason::DestinationReapproval);
        self.revision = 1;
        self.operation_epoch = 1;
        self.owned_part = None;
        self.original_transfer = None;
        self.original_representation = None;
        self.url_bookmark = None;
        self.output_audio = None;
        self.audio_available = false;
        self.commit_journal = None;
        self.error = None;
        self.retry_at_ms = None;
        self.transfer_attempts = 0;
        self.stages = DownloadStageRecords::for_settings(&self.settings, self.actual_quality);
    }
    pub fn compatibility_status(&self) -> &'static str {
        match self.state {
            DownloadState::Waiting => "waiting",
            DownloadState::Paused
            | DownloadState::AwaitingUrlApproval
            | DownloadState::Cancelled => "pause",
            DownloadState::Failed => "error",
            DownloadState::Completed => "completed",
            _ => "run",
        }
    }
    pub fn query_view(&self) -> DownloadTaskView {
        let bytes = self
            .original_transfer
            .as_ref()
            .map_or(0, |checkpoint| checkpoint.bytes);
        let total = self
            .original_transfer
            .as_ref()
            .and_then(|checkpoint| checkpoint.remote_total);
        DownloadTaskView {
            id: self.id.clone(),
            title: self.original_record.title.clone(),
            artists: self.original_record.artists.clone(),
            source: self.original_record.platform.source.clone(),
            requested_quality: self.requested_quality,
            actual_quality: self.actual_quality,
            state: self.state,
            compatibility_status: self.compatibility_status(),
            is_complate: self.state == DownloadState::Completed,
            bytes,
            total,
            progress_percent: total
                .filter(|total| *total > 0)
                .map(|total| (bytes as f64 / total as f64 * 100.0).min(100.0)),
            audio_available: self.audio_available,
            output_audio: self.output_audio.clone(),
            error_kind: self.error.as_ref().map(|error| error.kind),
            revision: self.revision,
            operation_epoch: self.operation_epoch,
        }
    }
}
impl fmt::Debug for DownloadTask {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DownloadTask")
            .field("id", &self.id)
            .field("state", &self.state)
            .field("revision", &self.revision)
            .field("operation_epoch", &self.operation_epoch)
            .field("audio_available", &self.audio_available)
            .finish()
    }
}
#[derive(Clone, Debug, Serialize)]
pub struct DownloadTaskView {
    pub id: String,
    pub title: String,
    pub artists: Vec<String>,
    pub source: String,
    pub requested_quality: DownloadQuality,
    pub actual_quality: DownloadQuality,
    pub state: DownloadState,
    pub compatibility_status: &'static str,
    #[serde(rename = "isComplate")]
    pub is_complate: bool,
    pub bytes: u64,
    pub total: Option<u64>,
    pub progress_percent: Option<f64>,
    pub audio_available: bool,
    pub output_audio: Option<DownloadArtifactProof>,
    pub error_kind: Option<super::DownloadErrorKind>,
    pub revision: u64,
    pub operation_epoch: u64,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DownloadDocumentError {
    InvalidBounds,
    InvalidBinding,
    InvalidUrl,
    InvalidState,
    InvalidDocument,
    DocumentBound,
    ProfileBound,
    DuplicateTask,
    EpochExhausted,
}
impl fmt::Display for DownloadDocumentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Invalid download document: {self:?}")
    }
}
impl std::error::Error for DownloadDocumentError {}
pub fn validate_profile_downloads(tasks: &[DownloadTask]) -> Result<(), DownloadDocumentError> {
    if tasks.len() > MAX_DOWNLOAD_TASKS {
        return Err(DownloadDocumentError::ProfileBound);
    }
    let mut ids = BTreeSet::new();
    let mut music_keys = BTreeSet::new();
    let mut bytes = 0usize;
    for task in tasks {
        task.validate()?;
        if tasks
            .first()
            .is_some_and(|first| first.profile_id != task.profile_id)
        {
            return Err(DownloadDocumentError::InvalidBinding);
        }
        if !ids.insert(&task.id) || !music_keys.insert(task.deduplication_key()) {
            return Err(DownloadDocumentError::DuplicateTask);
        }
        bytes = bytes
            .checked_add(
                serde_json::to_vec(task)
                    .map_err(|_| DownloadDocumentError::InvalidDocument)?
                    .len(),
            )
            .ok_or(DownloadDocumentError::DocumentBound)?;
        if bytes > MAX_DOWNLOAD_DOCUMENT_BYTES {
            return Err(DownloadDocumentError::DocumentBound);
        }
    }
    Ok(())
}
fn identifier(value: &str) -> Result<(), DownloadDocumentError> {
    if value.is_empty() || value.len() > 256 || value.chars().any(char::is_control) {
        Err(DownloadDocumentError::InvalidBinding)
    } else {
        Ok(())
    }
}
fn digest(value: &str) -> Result<(), DownloadDocumentError> {
    if value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        Ok(())
    } else {
        Err(DownloadDocumentError::InvalidBinding)
    }
}
fn identity(value: &FileIdentity) -> Result<(), DownloadDocumentError> {
    if value.object.is_empty()
        || value.object.len() > 128
        || !value.object.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        Err(DownloadDocumentError::InvalidBinding)
    } else {
        Ok(())
    }
}
fn owned_part(part: &OwnedPartRecord) -> Result<(), DownloadDocumentError> {
    identity(&part.identity)?;
    identity(&part.destination_identity)?;
    if part.nonce.len() != 32
        || !part.nonce.bytes().all(|byte| byte.is_ascii_hexdigit())
        || part.basename != format!(".lx-download-{}.part", part.nonce)
        || part.destination_epoch == 0
    {
        return Err(DownloadDocumentError::InvalidBinding);
    }
    super::validate_basename(&part.final_basename)
        .map_err(|_| DownloadDocumentError::InvalidBinding)
}
fn owned_candidate_name(value: &str) -> bool {
    value
        .strip_prefix(".lx-download-")
        .and_then(|name| name.strip_suffix(".part"))
        .is_some_and(|nonce| {
            nonce.len() == 32 && nonce.bytes().all(|byte| byte.is_ascii_hexdigit())
        })
}
fn owned_rollback_name(value: &str) -> bool {
    value
        .strip_prefix(".lx-download-")
        .and_then(|name| name.strip_suffix(".rollback"))
        .is_some_and(|nonce| {
            nonce.len() == 32 && nonce.bytes().all(|byte| byte.is_ascii_hexdigit())
        })
}

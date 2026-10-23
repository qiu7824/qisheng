//! Durable postprocessing intent never provides filesystem or source authority.

use super::{MAX_DOWNLOAD_BYTES, artifact::OwnedArtifactAllocation, task_model::*};
use crate::library::tag_install::{
    TagInstallArtifactFacts, TagInstallArtifactRole, TagInstallCoordinator, TagInstallPhase,
    TagInstallationReceipt, TagRecoveryDisposition, TagWriteSelection,
};
use crate::{
    domain::lyric::{LyricBundle, LyricOrigin},
    library::tag_write::{LocalTagPatch, TagField, VerifiedTagCover},
    platforms::{Platform, ProviderContext, ProviderRegistry, ProviderRequest, ProviderResponse},
    repository::Repository,
};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use sha2::Digest;
use std::{
    path::PathBuf,
    sync::{Arc, atomic::AtomicBool},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetryStages {
    pub metadata: bool,
    pub cover: bool,
    pub embedded_lyrics: bool,
    pub lyrics: bool,
}
impl RetryStages {
    pub fn select(stages: &DownloadStageRecords) -> Result<Self> {
        let select = |stage: &DownloadStageRecord| -> Result<bool> {
            let retry = matches!(
                stage.state,
                DownloadStageState::Pending
                    | DownloadStageState::Running
                    | DownloadStageState::Failed
            );
            ensure!(
                !retry || stage.attempts < 3,
                "Download postprocessing stage retry limit reached"
            );
            Ok(retry)
        };
        let selected = Self {
            metadata: select(&stages.metadata)?,
            cover: select(&stages.cover)?,
            embedded_lyrics: select(&stages.embedded_lyrics)?,
            lyrics: select(&stages.lyrics)?,
        };
        ensure!(
            selected.any(),
            "Download postprocessing is already complete"
        );
        Ok(selected)
    }
    pub fn any(self) -> bool {
        self.tags() || self.lyrics
    }
    pub fn tags(self) -> bool {
        self.metadata || self.cover || self.embedded_lyrics
    }
    pub fn begin(self, stages: &mut DownloadStageRecords) -> Result<()> {
        for (selected, stage) in [
            (self.metadata, &mut stages.metadata),
            (self.cover, &mut stages.cover),
            (self.embedded_lyrics, &mut stages.embedded_lyrics),
            (self.lyrics, &mut stages.lyrics),
        ] {
            if selected {
                ensure!(
                    stage.attempts < 3
                        && matches!(
                            stage.state,
                            DownloadStageState::Pending
                                | DownloadStageState::Running
                                | DownloadStageState::Failed
                        ),
                    "Download retry stage changed"
                );
                stage.attempts += 1;
                stage.state = DownloadStageState::Running;
                stage.error = None;
            }
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RetryArtifactRole {
    Backup,
    Candidate,
    Journal,
    Lyrics,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetryArtifactReservation {
    pub role: RetryArtifactRole,
    pub nonce: String,
    pub maximum_bytes: u64,
    pub allocation: Option<OwnedArtifactAllocation>,
    pub removed: bool,
    #[serde(default)]
    pub deletion: Option<RetryDeletionAttestation>,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetryDeletionAttestation {
    pub volume_guid: String,
    pub filesystem: String,
}
impl RetryDeletionAttestation {
    pub fn validate(&self) -> Result<()> {
        let value = &self.volume_guid;
        if self.filesystem == "POSIX-UNLINK" {
            ensure!(
                value
                    .strip_prefix("posix-dev-")
                    .is_some_and(|device| device.len() == 16
                        && device
                            .bytes()
                            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))),
                "Invalid canonical Unix retry device identity"
            );
            return Ok(());
        }
        ensure!(
            self.filesystem == "NTFS"
                && value.len() == 49
                && value.starts_with("\\\\?\\volume{")
                && value.ends_with("}\\"),
            "Invalid canonical retry volume GUID"
        );
        let guid = &value.as_bytes()[11..47];
        ensure!(
            guid.iter()
                .enumerate()
                .all(|(i, b)| if matches!(i, 8 | 13 | 18 | 23) {
                    *b == b'-'
                } else {
                    b.is_ascii_digit() || matches!(*b, b'a'..=b'f')
                }),
            "Invalid canonical retry volume identifier"
        );
        Ok(())
    }
}
impl RetryArtifactReservation {
    pub fn basename(&self) -> String {
        format!(".lx-download-{}.part", self.nonce)
    }
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.nonce.len() == 32 && self.nonce.bytes().all(|b| b.is_ascii_hexdigit()),
            "Invalid retry artifact nonce"
        );
        if let Some(deletion) = &self.deletion {
            ensure!(
                self.allocation.is_some(),
                "Unallocated retry artifact has no native deletion preparation"
            );
            deletion.validate()?;
        }
        ensure!(
            self.maximum_bytes > 0 && self.maximum_bytes <= MAX_DOWNLOAD_BYTES,
            "Invalid retry artifact reservation"
        );
        if let Some(allocation) = &self.allocation {
            allocation.validate()?;
            ensure!(
                allocation.record.nonce == self.nonce
                    && allocation.reserved_max_bytes == self.maximum_bytes,
                "Retry artifact differs from its original reservation"
            );
        }
        ensure!(
            !self.removed || self.allocation.is_some(),
            "Uncreated retry artifact cannot acknowledge removal"
        );
        Ok(())
    }
    pub fn unallocated_bytes(&self) -> u64 {
        if self.allocation.is_none() {
            self.maximum_bytes
        } else {
            0
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DownloadPostprocessRetryIntent {
    pub operation_id: String,
    pub task_revision: u64,
    pub operation_epoch: u64,
    pub original_audio: DownloadArtifactProof,
    pub selected: RetryStages,
    pub original_stages: DownloadStageRecords,
    pub artifacts: Vec<RetryArtifactReservation>,
    pub installed_audio: Option<DownloadArtifactProof>,
    pub repository_committed: bool,
}
impl DownloadPostprocessRetryIntent {
    pub(crate) fn new(task: &DownloadTask) -> Result<Self> {
        let selected = RetryStages::select(&task.stages)?;
        let original_audio = task
            .output_audio
            .clone()
            .ok_or_else(|| anyhow::anyhow!("Published download output is unavailable"))?;
        let reserve = |role, maximum_bytes| -> Result<_> {
            Ok(RetryArtifactReservation {
                role,
                nonce: nonce()?,
                maximum_bytes,
                allocation: None,
                removed: false,
                deletion: None,
            })
        };
        let mut artifacts = vec![reserve(RetryArtifactRole::Journal, 1024 * 1024)?];
        if selected.tags() {
            artifacts.push(reserve(RetryArtifactRole::Backup, original_audio.bytes)?);
            artifacts.push(reserve(
                RetryArtifactRole::Candidate,
                original_audio
                    .bytes
                    .saturating_add(crate::library::tag_write::MAX_TAG_METADATA_BYTES as u64)
                    .min(MAX_DOWNLOAD_BYTES),
            )?);
        }
        if selected.lyrics {
            artifacts.push(reserve(
                RetryArtifactRole::Lyrics,
                super::lyrics::MAX_DOWNLOAD_LYRIC_OUTPUT_BYTES as u64 + 3,
            )?);
        }
        let value = Self {
            operation_id: nonce()?,
            task_revision: task.revision,
            operation_epoch: task.operation_epoch,
            original_audio,
            selected,
            original_stages: task.stages.clone(),
            artifacts,
            installed_audio: None,
            repository_committed: false,
        };
        value.validate()?;
        Ok(value)
    }
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.operation_id.len() == 32
                && self.operation_id.bytes().all(|b| b.is_ascii_hexdigit()),
            "Invalid download retry operation"
        );
        ensure!(
            self.task_revision > 0 && self.operation_epoch > 0 && self.selected.any(),
            "Invalid download retry scope"
        );
        self.original_audio.validate()?;
        self.original_stages.validate()?;
        ensure!(
            (1..=4).contains(&self.artifacts.len()),
            "Download retry artifact bound exceeded"
        );
        for (index, artifact) in self.artifacts.iter().enumerate() {
            artifact.validate()?;
            ensure!(
                !self.artifacts[..index]
                    .iter()
                    .any(|old| old.role == artifact.role || old.nonce == artifact.nonce),
                "Duplicate download retry artifact"
            );
        }
        ensure!(
            self.artifacts
                .iter()
                .any(|a| a.role == RetryArtifactRole::Journal),
            "Download retry journal reservation is missing"
        );
        ensure!(
            !self.selected.tags()
                || [RetryArtifactRole::Backup, RetryArtifactRole::Candidate]
                    .into_iter()
                    .all(|role| self.artifacts.iter().any(|a| a.role == role)),
            "Download retry tag reservations are missing"
        );
        ensure!(
            !self.selected.lyrics
                || self
                    .artifacts
                    .iter()
                    .any(|a| a.role == RetryArtifactRole::Lyrics),
            "Download retry lyric reservation is missing"
        );
        if let Some(installed) = &self.installed_audio {
            installed.validate()?;
            ensure!(
                installed.basename == self.original_audio.basename
                    && installed.identity == self.original_audio.identity,
                "Retried tags cannot change the selected published object"
            );
        }
        ensure!(
            !self.repository_committed || self.installed_audio.is_some(),
            "Download retry acknowledgement has no verified installed output"
        );
        Ok(())
    }
    pub fn reservation(&self, role: RetryArtifactRole) -> Result<&RetryArtifactReservation> {
        self.artifacts
            .iter()
            .find(|a| a.role == role)
            .ok_or_else(|| anyhow::anyhow!("Download retry artifact reservation is unavailable"))
    }
    pub fn unallocated_bytes(&self) -> u64 {
        self.artifacts
            .iter()
            .map(RetryArtifactReservation::unallocated_bytes)
            .sum()
    }
    pub fn tag_operation_id(&self) -> String {
        format!("download-postprocess-{}", self.operation_id)
    }
}

fn nonce() -> Result<String> {
    let mut bytes = [0; 16];
    getrandom::fill(&mut bytes)
        .map_err(|_| anyhow::anyhow!("Cannot allocate download postprocessing identity"))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}
fn error(kind: super::DownloadErrorKind, message: impl Into<String>) -> super::DownloadError {
    super::DownloadError {
        kind,
        message: message.into(),
        http_status: None,
    }
}
fn fail(stage: &mut DownloadStageRecord, value: super::DownloadError) {
    stage.state = DownloadStageState::Failed;
    stage.error = Some(value);
}

struct RetryPreparation {
    stages: DownloadStageRecords,
    patch: LocalTagPatch,
    ready: RetryStages,
    lyric: Option<super::lyrics::LyricArtifact>,
}
fn prepare_stages(
    task: &DownloadTask,
    intent: &DownloadPostprocessRetryIntent,
    repository: &Repository,
    providers: &ProviderRegistry,
    cancel: &super::DownloadCancel,
    metadata_cancel: Arc<AtomicBool>,
) -> Result<RetryPreparation> {
    cancel.check()?;
    let selected = intent.selected;
    let mut stages = task.stages.clone();
    let mut patch = LocalTagPatch::default();
    let mut ready = RetryStages::default();
    if selected.metadata {
        let text = |v: &str| {
            if v.is_empty() {
                TagField::Remove
            } else {
                TagField::Set(v.to_owned())
            }
        };
        patch.title = text(&task.original_record.title);
        patch.album = text(&task.original_record.album);
        let artists = task.original_record.artists.join("、").replace('、', ";");
        patch.artists = if artists.is_empty() {
            TagField::Remove
        } else {
            TagField::Set(vec![artists])
        };
        ready.metadata = true;
    }
    if selected.cover {
        let lookup = (|| -> Result<_> {
            let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis() as u64;
            let key = crate::domain::artwork::ArtworkKey::new(
                &task.original_record,
                None,
                crate::domain::artwork::ArtworkAuthority::Native,
            )?;
            let crate::domain::artwork::ArtworkLookup::Hit(info) =
                repository.artwork_info(&key, now)?
            else {
                anyhow::bail!("Download cover requires a separately approved cached picture");
            };
            let blob = info
                .blob
                .ok_or_else(|| anyhow::anyhow!("Download cover is not cached"))?;
            let (_, bytes) = repository
                .stored_artwork_blob(&blob.sha256, now)?
                .ok_or_else(|| anyhow::anyhow!("Download cached cover body is unavailable"))?;
            ensure!(
                format!("{:x}", sha2::Sha256::digest(&bytes)) == blob.sha256,
                "Download cached cover changed"
            );
            VerifiedTagCover::from_bytes_checked(bytes, || cancel.check())
        })();
        match lookup {
            Ok(cover) => {
                patch.cover = TagField::Set(cover);
                ready.cover = true;
            }
            Err(cause) => fail(
                &mut stages.cover,
                error(
                    super::DownloadErrorKind::Verification,
                    format!("{cause:#}").chars().take(1024).collect::<String>(),
                ),
            ),
        }
    }
    let mut bundle = if selected.embedded_lyrics || selected.lyrics {
        repository
            .lyrics(&task.original_record.id, LyricOrigin::Edited)?
            .filter(|b| b.lyric.text().is_some())
            .or(repository.lyrics(&task.original_record.id, LyricOrigin::Raw)?)
    } else {
        None
    };
    if (selected.embedded_lyrics || selected.lyrics) && bundle.is_none() {
        let platform = match task.original_record.platform.source.as_str() {
            "kw" => Platform::Kw,
            "kg" => Platform::Kg,
            "tx" => Platform::Tx,
            "wy" => Platform::Wy,
            "mg" => Platform::Mg,
            _ => anyhow::bail!("Download lyric platform is unsupported"),
        };
        let context = ProviderContext {
            cancel: metadata_cancel,
            deadline: Instant::now() + Duration::from_secs(15),
        };
        if let Ok(ProviderResponse::Lyrics(lyrics)) = providers.execute(
            platform,
            ProviderRequest::Lyrics {
                track: task.original_record.clone(),
                word_by_word: true,
            },
            &context,
        ) {
            let field = |v: Option<String>| {
                v.map(crate::domain::lyric::LyricField::Text)
                    .unwrap_or(crate::domain::lyric::LyricField::Null)
            };
            bundle = Some(LyricBundle {
                lyric: crate::domain::lyric::LyricField::Text(lyrics.lyric),
                tlyric: field(lyrics.translated),
                rlyric: field(lyrics.romanized),
                lxlyric: field(lyrics.word_by_word),
            });
        }
    }
    cancel.check()?;
    let mut lyric = None;
    if selected.embedded_lyrics || selected.lyrics {
        match bundle {
            Some(bundle) if bundle.lyric.text().is_some_and(|v| !v.trim().is_empty()) => {
                bundle.validate()?;
                if selected.embedded_lyrics {
                    let text = super::lyrics::build_download_lyrics(
                        &bundle,
                        task.settings.embedded_lyrics,
                    )?;
                    patch.lyrics = TagField::Set(text.text);
                    ready.embedded_lyrics = true;
                }
                if selected.lyrics {
                    lyric = Some(super::lyrics::encode_lyric_artifact(
                        &bundle,
                        task.settings.sidecar_lyrics,
                        task.settings.lyric_encoding,
                        LyricOrigin::Raw,
                        task.revision,
                    )?);
                    ready.lyrics = true;
                }
            }
            Some(_) => {
                if selected.embedded_lyrics {
                    stages.embedded_lyrics.state = DownloadStageState::SkippedUnavailable;
                }
                if selected.lyrics {
                    stages.lyrics.state = DownloadStageState::SkippedUnavailable;
                }
            }
            None => {
                let cause = error(
                    super::DownloadErrorKind::Network,
                    "Download lyrics could not be fetched",
                );
                if selected.embedded_lyrics {
                    fail(&mut stages.embedded_lyrics, cause.clone());
                }
                if selected.lyrics {
                    fail(&mut stages.lyrics, cause);
                }
            }
        }
    }
    Ok(RetryPreparation {
        stages,
        patch,
        ready,
        lyric,
    })
}

struct RetryCoordinator {
    repository: Arc<Repository>,
    task: DownloadTask,
    intent: DownloadPostprocessRetryIntent,
    grant: super::DestinationGrant,
    cancel: super::DownloadCancel,
    stages: DownloadStageRecords,
    ready: RetryStages,
}
impl RetryCoordinator {
    fn record_allocation_intent(
        &self,
        role: RetryArtifactRole,
        actual: Option<&super::artifact::OwnedArtifactIntent>,
    ) -> Result<()> {
        self.current()?;
        self.grant.verify()?;
        let reservation = self.intent.reservation(role)?;
        let provenance =
            self.task.original_representation.as_ref().ok_or_else(|| {
                anyhow::anyhow!("Download retry original provenance is unavailable")
            })?;
        let basename = if role == RetryArtifactRole::Lyrics {
            let stem = std::path::Path::new(&self.intent.original_audio.basename)
                .file_stem()
                .and_then(|stem| stem.to_str())
                .ok_or_else(|| anyhow::anyhow!("Download retry leaf has no valid stem"))?;
            format!("{stem}.lrc")
        } else {
            self.intent.original_audio.basename.clone()
        };
        let intent = super::artifact::OwnedArtifactIntent {
            kind: match role {
                RetryArtifactRole::Candidate => super::artifact::OwnedArtifactKind::TagCopy,
                RetryArtifactRole::Lyrics => super::artifact::OwnedArtifactKind::LyricSidecar,
                RetryArtifactRole::Backup | RetryArtifactRole::Journal => {
                    super::artifact::OwnedArtifactKind::RollbackOriginal
                }
            },
            nonce: reservation.nonce.clone(),
            basename: reservation.basename(),
            final_basename: basename,
            working_directory_path: self.grant.path().to_owned(),
            working_directory_identity: self.grant.working_directory_identity().clone(),
            selected_root_identity: self.task.destination.root_identity.clone(),
            destination_epoch: self.task.destination.grant_epoch,
            relative_group: (self.task.destination.relative_components.len() == 2)
                .then(|| self.task.destination.relative_components[0].clone()),
            reserved_max_bytes: reservation.maximum_bytes,
            context: super::DownloadContext {
                profile_id: self.task.profile_id.clone(),
                task_id: self.task.id.clone(),
                operation_epoch: self.intent.operation_epoch,
                source_id: provenance.source_id.clone(),
                source_content_sha256: provenance.source_content_sha256.clone(),
                policy_epoch: provenance.policy_epoch,
            },
        };
        if let Some(actual) = actual {
            ensure!(
                *actual == intent,
                "Retry producer pending allocation differs from its actual native plan"
            );
        }
        self.repository.record_download_retry_allocation_intent(
            &self.task.id,
            &self.intent.operation_id,
            self.task.revision,
            self.task.operation_epoch,
            role,
            &intent,
            &|| self.cancel.check(),
        )
    }
    fn selection(&self) -> TagWriteSelection {
        TagWriteSelection {
            operation_id: self.intent.tag_operation_id(),
            asset_id: format!("download:{}", self.task.id),
            path: self.grant.path().join(&self.intent.original_audio.basename),
            expected_identity: library_identity(&self.intent.original_audio.identity),
            root_id: None,
            grant_epoch: self.task.destination.grant_epoch,
            expected_binding_revision: self.intent.task_revision,
            expected_body_sha256: Some(self.intent.original_audio.sha256.clone()),
        }
    }
    fn current(&self) -> Result<DownloadTask> {
        self.cancel.check()?;
        self.grant.verify()?;
        let task = self
            .repository
            .download_task(&self.task.id)?
            .ok_or_else(|| anyhow::anyhow!("Download retry task disappeared"))?;
        ensure!(
            task.revision == self.task.revision
                && task.operation_epoch == self.task.operation_epoch
                && task.state == super::DownloadState::PostProcessing
                && task
                    .commit_journal
                    .as_ref()
                    .and_then(|j| j.retry.as_ref())
                    .is_some_and(|i| i.operation_id == self.intent.operation_id),
            "Download retry current intent changed"
        );
        let stored = self
            .repository
            .download_destinations()?
            .into_iter()
            .find(|g| g.id == task.destination.grant_id)
            .ok_or_else(|| anyhow::anyhow!("Download retry destination disappeared"))?;
        task.require_current_destination(&stored)?;
        Ok(task)
    }
    fn complete_tag_stages(&mut self) {
        for (ready, stage) in [
            (self.ready.metadata, &mut self.stages.metadata),
            (self.ready.cover, &mut self.stages.cover),
            (self.ready.embedded_lyrics, &mut self.stages.embedded_lyrics),
        ] {
            if ready {
                stage.state = DownloadStageState::Succeeded;
                stage.error = None;
            }
        }
    }
}
fn library_identity(identity: &super::FileIdentity) -> crate::domain::library::FileIdentity {
    crate::domain::library::FileIdentity {
        kind: "windows_file_id".into(),
        volume: format!("{:016x}", identity.device),
        file_id: identity.object.clone(),
    }
}
fn download_identity(
    identity: &crate::domain::library::FileIdentity,
) -> Result<super::FileIdentity> {
    ensure!(
        identity.kind == "windows_file_id",
        "Download retry requires actual Windows file identity"
    );
    Ok(super::FileIdentity {
        device: u64::from_str_radix(&identity.volume, 16)?,
        object: identity.file_id.clone(),
    })
}
fn role(role: TagInstallArtifactRole) -> RetryArtifactRole {
    match role {
        TagInstallArtifactRole::Journal => RetryArtifactRole::Journal,
        TagInstallArtifactRole::Backup => RetryArtifactRole::Backup,
        TagInstallArtifactRole::Candidate => RetryArtifactRole::Candidate,
    }
}
impl TagInstallCoordinator for RetryCoordinator {
    fn before_artifact_allocation(
        &mut self,
        selection: &TagWriteSelection,
        kind: TagInstallArtifactRole,
    ) -> Result<()> {
        self.check_current_intent(selection)?;
        self.record_allocation_intent(role(kind), None)
    }
    fn authorize_explicit_selection(&self, _: &TagWriteSelection) -> Result<()> {
        anyhow::bail!("Download retry requires its actual published owner")
    }
    fn check_current_intent(&self, selection: &TagWriteSelection) -> Result<()> {
        self.current()?;
        let expected = self.selection();
        ensure!(
            selection.operation_id == expected.operation_id
                && selection.asset_id == expected.asset_id
                && selection.path == expected.path
                && selection.expected_identity == expected.expected_identity
                && selection.grant_epoch == expected.grant_epoch
                && selection.expected_binding_revision == expected.expected_binding_revision
                && selection.expected_body_sha256 == expected.expected_body_sha256,
            "Download tag writer selection changed"
        );
        Ok(())
    }
    fn journal_directory(&self) -> Result<PathBuf> {
        self.grant.verify()?;
        Ok(self.grant.path().to_owned())
    }
    fn artifact_basename(
        &self,
        _: &TagWriteSelection,
        kind: TagInstallArtifactRole,
    ) -> Result<Option<String>> {
        Ok(Some(self.intent.reservation(role(kind))?.basename()))
    }
    fn artifact_limit(&self, kind: TagInstallArtifactRole) -> u64 {
        self.intent
            .reservation(role(kind))
            .map_or(0, |r| r.maximum_bytes)
    }
    fn artifact_created(
        &mut self,
        selection: &TagWriteSelection,
        facts: &TagInstallArtifactFacts,
    ) -> Result<()> {
        self.check_current_intent(selection)?;
        let selected_role = role(facts.role);
        let reservation = self.intent.reservation(selected_role)?;
        ensure!(
            crate::domain::music::normalize_local_path(&facts.path)
                == crate::domain::music::normalize_local_path(
                    &self.grant.path().join(reservation.basename())
                )
                && download_identity(&facts.directory_identity)?
                    == *self.grant.working_directory_identity(),
            "Download retry producer namespace changed"
        );
        let provenance =
            self.task.original_representation.as_ref().ok_or_else(|| {
                anyhow::anyhow!("Download retry original provenance is unavailable")
            })?;
        let allocation = OwnedArtifactAllocation {
            kind: if selected_role == RetryArtifactRole::Candidate {
                super::artifact::OwnedArtifactKind::TagCopy
            } else {
                super::artifact::OwnedArtifactKind::RollbackOriginal
            },
            record: super::OwnedPartRecord {
                basename: reservation.basename(),
                nonce: reservation.nonce.clone(),
                identity: download_identity(&facts.identity)?,
                destination_identity: self.task.destination.root_identity.clone(),
                destination_epoch: self.task.destination.grant_epoch,
                final_basename: self.intent.original_audio.basename.clone(),
            },
            working_directory_path: self.grant.path().to_owned(),
            working_directory_identity: self.grant.working_directory_identity().clone(),
            selected_root_identity: self.task.destination.root_identity.clone(),
            relative_group: if self.task.destination.relative_components.len() == 2 {
                Some(self.task.destination.relative_components[0].clone())
            } else {
                None
            },
            reserved_max_bytes: reservation.maximum_bytes,
            context: super::DownloadContext {
                profile_id: self.task.profile_id.clone(),
                task_id: self.task.id.clone(),
                operation_epoch: self.intent.operation_epoch,
                source_id: provenance.source_id.clone(),
                source_content_sha256: provenance.source_content_sha256.clone(),
                policy_epoch: provenance.policy_epoch,
            },
        };
        self.repository.record_download_retry_artifact(
            &self.task.id,
            &self.intent.operation_id,
            self.task.revision,
            self.task.operation_epoch,
            selected_role,
            &allocation,
            &|| self.cancel.check(),
        )?;
        Ok(())
    }
    fn commit_installed(
        &mut self,
        selection: &TagWriteSelection,
        receipt: &TagInstallationReceipt,
    ) -> Result<u64> {
        if let Some(task) = self.repository.download_task(&self.task.id)?
            && task
                .commit_journal
                .as_ref()
                .and_then(|j| j.retry.as_ref())
                .is_some_and(|i| {
                    i.operation_id == self.intent.operation_id && i.repository_committed
                })
            && task.output_audio.as_ref().is_some_and(|p| {
                p.identity == self.intent.original_audio.identity
                    && p.sha256 == receipt.new_body_sha256
                    && p.bytes == receipt.size_bytes
            })
        {
            self.task = task;
            self.intent = self
                .task
                .commit_journal
                .as_ref()
                .unwrap()
                .retry
                .clone()
                .unwrap();
            return Ok(self.task.revision);
        }
        self.check_current_intent(selection)?;
        ensure!(
            receipt.operation_id == selection.operation_id
                && receipt.file_identity == selection.expected_identity
                && receipt.old_body_sha256 == self.intent.original_audio.sha256,
            "Download retry receipt differs from its actual selected object"
        );
        self.complete_tag_stages();
        let proof = DownloadArtifactProof {
            basename: self.intent.original_audio.basename.clone(),
            identity: self.intent.original_audio.identity.clone(),
            bytes: receipt.size_bytes,
            sha256: receipt.new_body_sha256.clone(),
        };
        self.task = self.repository.commit_download_retry_tags(
            &self.task.id,
            &self.intent.operation_id,
            self.task.revision,
            self.task.operation_epoch,
            &proof,
            &self.stages,
            &|| self.cancel.check(),
        )?;
        self.intent = self
            .task
            .commit_journal
            .as_ref()
            .unwrap()
            .retry
            .clone()
            .unwrap();
        Ok(self.task.revision)
    }
    fn publish_installed(
        &mut self,
        _: &TagWriteSelection,
        receipt: &TagInstallationReceipt,
    ) -> Result<()> {
        let task = self
            .repository
            .download_task(&self.task.id)?
            .ok_or_else(|| anyhow::anyhow!("Download retry task disappeared"))?;
        ensure!(
            task.output_audio
                .as_ref()
                .is_some_and(|p| p.identity == self.intent.original_audio.identity
                    && p.sha256 == receipt.new_body_sha256
                    && p.bytes == receipt.size_bytes)
                && task
                    .commit_journal
                    .as_ref()
                    .and_then(|j| j.retry.as_ref())
                    .is_some_and(
                        |i| i.operation_id == self.intent.operation_id && i.repository_committed
                    ),
            "Download retry publication is not committed"
        );
        self.task = task;
        Ok(())
    }
    fn check_recovery(
        &self,
        selection: &TagWriteSelection,
        _: TagInstallPhase,
        receipt: Option<&TagInstallationReceipt>,
    ) -> Result<TagRecoveryDisposition> {
        self.grant.verify()?;
        let expected = self.selection();
        ensure!(
            selection.operation_id == expected.operation_id
                && selection.expected_identity == expected.expected_identity
                && selection.expected_body_sha256 == expected.expected_body_sha256
                && selection.path == expected.path,
            "Download retry recovery selection changed"
        );
        let task = self
            .repository
            .download_task(&self.task.id)?
            .ok_or_else(|| anyhow::anyhow!("Download retry task disappeared"))?;
        let intent = task
            .commit_journal
            .as_ref()
            .and_then(|j| j.retry.as_ref())
            .ok_or_else(|| anyhow::anyhow!("Download retry recovery intent is unavailable"))?;
        ensure!(
            intent.operation_id == self.intent.operation_id
                && intent.original_audio == self.intent.original_audio,
            "Download retry recovery scope changed"
        );
        if intent.repository_committed {
            let receipt = receipt.ok_or_else(|| {
                anyhow::anyhow!("Committed download retry receipt is unavailable")
            })?;
            ensure!(
                task.output_audio
                    .as_ref()
                    .is_some_and(|p| p.identity == intent.original_audio.identity
                        && p.bytes == receipt.size_bytes
                        && p.sha256 == receipt.new_body_sha256),
                "Download retry committed body changed"
            );
            Ok(TagRecoveryDisposition::Completed)
        } else {
            ensure!(
                task.output_audio.as_ref() == Some(&intent.original_audio),
                "Download retry original output changed"
            );
            Ok(TagRecoveryDisposition::RestoreOriginal)
        }
    }
    fn check_install_chunk(&mut self, restoring: bool, _: u64) -> Result<()> {
        if restoring {
            let selection = self.selection();
            ensure!(
                self.check_recovery(&selection, TagInstallPhase::Restoring, None)?
                    == TagRecoveryDisposition::RestoreOriginal,
                "Committed download tags cannot be rolled back"
            );
            Ok(())
        } else {
            self.current().map(|_| ())
        }
    }
    fn checkpoint(&mut self, phase: TagInstallPhase) -> Result<()> {
        if matches!(
            phase,
            TagInstallPhase::Restoring
                | TagInstallPhase::Restored
                | TagInstallPhase::RepoCommitted
                | TagInstallPhase::Published
        ) {
            Ok(())
        } else {
            self.current().map(|_| ())
        }
    }
}

struct RetrySidecarJournal<'a>(&'a RetryCoordinator);
impl super::artifact::OwnedArtifactJournal for RetrySidecarJournal<'_> {
    fn record_intent(&self, intent: &super::artifact::OwnedArtifactIntent) -> Result<()> {
        self.0
            .record_allocation_intent(RetryArtifactRole::Lyrics, Some(intent))
    }
    fn record_created(&self, allocation: &OwnedArtifactAllocation) -> Result<()> {
        self.0.current()?;
        self.0.repository.record_download_retry_artifact(
            &self.0.task.id,
            &self.0.intent.operation_id,
            self.0.task.revision,
            self.0.task.operation_epoch,
            RetryArtifactRole::Lyrics,
            allocation,
            &|| self.0.cancel.check(),
        )
    }
}

pub(crate) struct RetryWorkResult {
    pub task: DownloadTask,
    pub stages: DownloadStageRecords,
    pub sidecar: Option<DownloadArtifactProof>,
    pub error: Option<anyhow::Error>,
    pub restart_required: bool,
    pub retirement_complete: bool,
    pub(crate) owner: Option<super::authority::PublishedAudioOwner>,
    retirements: Vec<super::authority::PreparedRetryArtifactRetirement>,
    retired_witnesses: Vec<super::authority::RetryArtifactRetirement>,
}
impl RetryWorkResult {
    pub(crate) fn finish_retirements_after_join(&mut self, repository: &Repository) -> Result<()> {
        let operation = self
            .task
            .commit_journal
            .as_ref()
            .and_then(|journal| journal.retry.as_ref())
            .ok_or_else(|| anyhow::anyhow!("Download retry retirement operation is unavailable"))?
            .operation_id
            .clone();
        while let Some(witness) = self.retired_witnesses.last() {
            repository.acknowledge_download_retry_retirement(&self.task.id, &operation, witness)?;
            self.retired_witnesses.pop();
        }
        while !self.retirements.is_empty() {
            let witness = self.retirements[0].finish()?;
            self.retirements.remove(0);
            self.retired_witnesses.push(witness);
            repository.acknowledge_download_retry_retirement(
                &self.task.id,
                &operation,
                self.retired_witnesses.last().unwrap(),
            )?;
            self.retired_witnesses.pop();
        }
        self.task = repository.download_task(&self.task.id)?.ok_or_else(|| {
            anyhow::anyhow!("Download retry task disappeared during actual retirement")
        })?;
        let intent = self
            .task
            .commit_journal
            .as_ref()
            .and_then(|journal| journal.retry.as_ref())
            .ok_or_else(|| anyhow::anyhow!("Download retry retirement intent disappeared"))?;
        self.retirement_complete =
            retirement_allocations(intent, self.task.stages.lyrics.artifact.is_some()).is_empty();
        Ok(())
    }
}
struct RetryRetirementAckFailure {
    witness: super::authority::RetryArtifactRetirement,
    message: String,
}
impl std::fmt::Debug for RetryRetirementAckFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RetryRetirementAckFailure")
            .field("record", self.witness.record())
            .finish()
    }
}
impl std::fmt::Display for RetryRetirementAckFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Download retry retirement acknowledgement failed: {}",
            self.message
        )
    }
}
impl std::error::Error for RetryRetirementAckFailure {}
fn retire_artifact(
    repository: &Repository,
    grant: &super::DestinationGrant,
    id: &str,
    operation: &str,
    allocation: &OwnedArtifactAllocation,
    prepared_retirements: &mut Vec<super::authority::PreparedRetryArtifactRetirement>,
) -> Result<()> {
    if prepared_retirements
        .iter()
        .any(|prepared| prepared.record() == &allocation.record)
    {
        return Ok(());
    }
    let witness = if let Some(prepared) = repository
        .download_task(id)?
        .and_then(|t| t.commit_journal.and_then(|j| j.retry))
        .and_then(|i| {
            i.artifacts
                .into_iter()
                .find(|a| a.allocation.as_ref() == Some(allocation))
        })
        .and_then(|a| a.deletion)
    {
        match super::authority::recover_retry_retirement(grant, &allocation.record, &prepared) {
            Ok(witness) => witness,
            Err(_) => {
                prepared_retirements.push(super::authority::prepare_retry_artifact_retirement(
                    grant,
                    allocation,
                    &|record, facts| {
                        repository.prepare_download_retry_retirement(id, operation, record, facts)
                    },
                )?);
                return Ok(());
            }
        }
    } else {
        prepared_retirements.push(super::authority::prepare_retry_artifact_retirement(
            grant,
            allocation,
            &|record, facts| {
                repository.prepare_download_retry_retirement(id, operation, record, facts)
            },
        )?);
        return Ok(());
    };
    match repository.acknowledge_download_retry_retirement(id, operation, &witness) {
        Ok(()) => Ok(()),
        Err(cause) => Err(RetryRetirementAckFailure {
            witness,
            message: format!("{cause:#}").chars().take(1024).collect(),
        }
        .into()),
    }
}
fn retirement_allocations(
    intent: &DownloadPostprocessRetryIntent,
    lyric_published: bool,
) -> Vec<&OwnedArtifactAllocation> {
    let mut roles = intent
        .artifacts
        .iter()
        .filter(|a| !a.removed && !(a.role == RetryArtifactRole::Lyrics && lyric_published))
        .collect::<Vec<_>>();
    roles.sort_by_key(|a| match a.role {
        RetryArtifactRole::Backup => 0,
        RetryArtifactRole::Candidate => 1,
        RetryArtifactRole::Lyrics => 2,
        RetryArtifactRole::Journal => 3,
    });
    roles
        .into_iter()
        .filter_map(|a| a.allocation.as_ref())
        .collect()
}
pub(crate) fn execute_retry(
    repository: Arc<Repository>,
    providers: Arc<ProviderRegistry>,
    task: DownloadTask,
    stored: StoredDestinationGrant,
    cancel: super::DownloadCancel,
    metadata_cancel: Arc<AtomicBool>,
    mut retained: Option<RetryWorkResult>,
) -> RetryWorkResult {
    let mut stages = task.stages.clone();
    let mut sidecar = None;
    let mut owner = retained.as_mut().and_then(|r| r.owner.take());
    let mut current = task.clone();
    let mut restart_required = false;
    let mut retirement_complete = false;
    let mut prepared_retirements = retained.as_mut().map_or_else(Vec::new, |previous| {
        std::mem::take(&mut previous.retirements)
    });
    let retired_witnesses = retained.as_mut().map_or_else(Vec::new, |previous| {
        std::mem::take(&mut previous.retired_witnesses)
    });
    let result = (|| -> Result<()> {
        let mut grant = super::DestinationGrant::open_selected(&stored.path, stored.epoch)?;
        ensure!(
            grant.identity() == &stored.identity,
            "Download retry selected destination changed"
        );
        task.require_current_destination(&stored)?;
        if task.destination.relative_components.len() == 2 {
            grant = grant.open_existing_group(&task.destination.relative_components[0])?;
        }
        let intent = task
            .commit_journal
            .as_ref()
            .and_then(|j| j.retry.clone())
            .ok_or_else(|| anyhow::anyhow!("Download retry intent missing"))?;
        let recovering = intent.artifacts.iter().any(|a| a.allocation.is_some());
        if let Some(owner) = &owner {
            ensure!(
                owner.grant().identity() == grant.identity()
                    && owner.grant().working_directory_identity()
                        == grant.working_directory_identity()
                    && owner.grant().epoch() == grant.epoch(),
                "Retained download retry owner no longer belongs to its selected namespace"
            );
        } else {
            owner = Some(super::authority::PublishedAudioOwner::open(
                grant.clone(),
                intent.original_audio.clone(),
                &cancel,
                recovering,
            )?);
        }
        let preparation = if recovering {
            RetryPreparation {
                stages: task.stages.clone(),
                patch: LocalTagPatch::default(),
                ready: RetryStages::default(),
                lyric: None,
            }
        } else {
            prepare_stages(
                &task,
                &intent,
                &repository,
                &providers,
                &cancel,
                metadata_cancel.clone(),
            )?
        };
        let mut coordinator = RetryCoordinator {
            repository: repository.clone(),
            task: task.clone(),
            intent: intent.clone(),
            grant: grant.clone(),
            cancel: cancel.clone(),
            stages: preparation.stages,
            ready: preparation.ready,
        };
        if let Some(previous) = &mut retained {
            if previous
                .error
                .as_ref()
                .is_some_and(|e| e.is::<super::authority::RetrySidecarPreparationFailure>())
            {
                let error = previous.error.take().unwrap();
                let mut sidecar = error
                    .downcast::<super::authority::RetrySidecarPreparationFailure>()
                    .unwrap();
                let latest = repository.download_task(&task.id)?.unwrap();
                if latest
                    .commit_journal
                    .as_ref()
                    .unwrap()
                    .retry
                    .as_ref()
                    .unwrap()
                    .reservation(RetryArtifactRole::Lyrics)?
                    .allocation
                    .is_none()
                    && let Err(cause) =
                        sidecar.acknowledge(&RetrySidecarJournal(&coordinator), &cancel)
                {
                    let _ = cause;
                    return Err(sidecar.into());
                }
                let latest = repository.download_task(&task.id)?.unwrap();
                let allocation = latest
                    .commit_journal
                    .as_ref()
                    .unwrap()
                    .retry
                    .as_ref()
                    .unwrap()
                    .reservation(RetryArtifactRole::Lyrics)?
                    .allocation
                    .as_ref()
                    .unwrap();
                let witness = sidecar.retire(&grant, allocation, &|record, facts| {
                    repository.prepare_download_retry_retirement(
                        &task.id,
                        &intent.operation_id,
                        record,
                        facts,
                    )
                })?;
                if let Err(cause) = repository.acknowledge_download_retry_retirement(
                    &task.id,
                    &intent.operation_id,
                    &witness,
                ) {
                    return Err(RetryRetirementAckFailure {
                        witness,
                        message: format!("{cause:#}"),
                    }
                    .into());
                }
                restart_required = true;
            } else if let Some(pending) = previous
                .error
                .as_ref()
                .and_then(|e| e.downcast_ref::<super::authority::RetryArtifactRemovalPending>())
            {
                let witness = pending.finish()?;
                if let Err(cause) = repository.acknowledge_download_retry_retirement(
                    &task.id,
                    &intent.operation_id,
                    &witness,
                ) {
                    return Err(RetryRetirementAckFailure {
                        witness,
                        message: format!("{cause:#}"),
                    }
                    .into());
                }
                previous.error.take();
            } else if let Some(unacknowledged) = previous
                .error
                .as_ref()
                .and_then(|e| e.downcast_ref::<RetryRetirementAckFailure>())
            {
                repository.acknowledge_download_retry_retirement(
                    &task.id,
                    &intent.operation_id,
                    &unacknowledged.witness,
                )?;
                previous.error.take();
            } else if let Some(unacknowledged) = previous.error.as_ref().and_then(|e| {
                e.downcast_ref::<crate::library::tag_install::UnacknowledgedTagArtifact>()
            }) {
                let role = role(unacknowledged.facts().role);
                unacknowledged.acknowledge(&coordinator.selection(), &mut coordinator)?;
                previous.error.take();
                let pending = repository
                    .download_task(&task.id)?
                    .ok_or_else(|| anyhow::anyhow!("Download retry task disappeared"))?;
                let allocation = pending
                    .commit_journal
                    .as_ref()
                    .unwrap()
                    .retry
                    .as_ref()
                    .unwrap()
                    .reservation(role)?
                    .allocation
                    .as_ref()
                    .unwrap();
                retire_artifact(
                    &repository,
                    &grant,
                    &task.id,
                    &intent.operation_id,
                    allocation,
                    &mut prepared_retirements,
                )?;
                if role == RetryArtifactRole::Journal {
                    owner.as_mut().unwrap().verify_body(&cancel)?;
                    restart_required = true;
                    current = repository.download_task(&task.id)?.unwrap();
                    owner.take();
                    retirement_complete = true;
                    return Ok(());
                }
            } else {
                previous.error.take();
            }
        }
        let context = crate::library::scan::ScanContext {
            cancel: metadata_cancel,
            deadline: Instant::now() + Duration::from_secs(30),
        };
        let actual = repository
            .download_task(&task.id)?
            .ok_or_else(|| anyhow::anyhow!("Download retry task disappeared"))?;
        let actual_intent = actual
            .commit_journal
            .as_ref()
            .and_then(|j| j.retry.as_ref())
            .ok_or_else(|| anyhow::anyhow!("Download retry intent disappeared"))?;
        if recovering
            && retirement_allocations(actual_intent, actual.stages.lyrics.artifact.is_some())
                .is_empty()
        {
            owner
                .as_mut()
                .unwrap()
                .accept_installed(actual.output_audio.clone().unwrap(), &cancel)?;
            restart_required =
                !actual_intent.repository_committed && !actual.stages.all_satisfied();
            stages = actual.stages.clone();
            current = actual;
            owner.take();
            retirement_complete = true;
            return Ok(());
        }
        if recovering
            && intent.selected.tags()
            && actual_intent
                .reservation(RetryArtifactRole::Journal)?
                .allocation
                .is_some()
            && !actual_intent
                .reservation(RetryArtifactRole::Journal)?
                .removed
        {
            let selection = coordinator.selection();
            let token = owner
                .as_ref()
                .unwrap()
                .tag_writer(selection, &context, &coordinator)?;
            let path = grant
                .path()
                .join(intent.reservation(RetryArtifactRole::Journal)?.basename());
            let receipt = crate::library::tag_install::recover_tag_install_owned(
                &path,
                &context,
                &mut coordinator,
                token,
            )?;
            let restored = repository
                .download_task(&task.id)?
                .ok_or_else(|| anyhow::anyhow!("Download retry task disappeared"))?;
            if let Some(proof) = restored.output_audio.clone() {
                owner.as_mut().unwrap().accept_installed(proof, &cancel)?;
            }
            // A restored interrupted operation retires its previous artifacts
            // before a new invocation creates another journal or candidate.
            current = restored;
            stages = current.stages.clone();
            restart_required = receipt.is_none();
        } else if preparation.ready.tags() {
            let selection = coordinator.selection();
            let token = owner
                .as_ref()
                .unwrap()
                .tag_writer(selection, &context, &coordinator)?;
            crate::library::tag_install::install_tags(
                token,
                grant.path(),
                &preparation.patch,
                &context,
                &mut coordinator,
            )?;
            current = coordinator.task.clone();
            owner
                .as_mut()
                .unwrap()
                .accept_installed(current.output_audio.clone().unwrap(), &cancel)?;
        }
        if !recovering {
            stages = coordinator.stages.clone();
        }
        if !recovering && let Some(artifact) = preparation.lyric {
            let reservation = intent.reservation(RetryArtifactRole::Lyrics)?;
            let provenance = task
                .original_representation
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("Download retry provenance missing"))?;
            let context = super::DownloadContext {
                profile_id: task.profile_id.clone(),
                task_id: task.id.clone(),
                operation_epoch: intent.operation_epoch,
                source_id: provenance.source_id.clone(),
                source_content_sha256: provenance.source_content_sha256.clone(),
                policy_epoch: provenance.policy_epoch,
            };
            let outcome = (|| -> Result<_> {
                let mut part = owner.as_ref().unwrap().prepare_sidecar(
                    &artifact,
                    reservation.nonce.clone(),
                    context,
                    &cancel,
                    &RetrySidecarJournal(&coordinator),
                )?;
                let checkpoint = part.checkpoint(&cancel, None, None, true)?;
                repository.verify_download_artifact(
                    &reservation.nonce,
                    intent.operation_epoch,
                    checkpoint.bytes,
                    &checkpoint.sha256,
                    &|| cancel.check(),
                )?;
                let committed =
                    part.commit_sidecar(checkpoint.bytes, &checkpoint.sha256, &cancel)?;
                Ok(DownloadArtifactProof {
                    basename: committed
                        .path
                        .file_name()
                        .ok_or_else(|| anyhow::anyhow!("Download lyric basename missing"))?
                        .to_string_lossy()
                        .into_owned(),
                    identity: committed.identity,
                    bytes: committed.bytes,
                    sha256: committed.sha256,
                })
            })();
            match outcome {
                Ok(proof) => {
                    stages.lyrics.state = DownloadStageState::Succeeded;
                    stages.lyrics.error = None;
                    coordinator.task = repository.commit_download_retry_lyrics(
                        &task.id,
                        &intent.operation_id,
                        coordinator.task.revision,
                        coordinator.task.operation_epoch,
                        &proof,
                        &stages,
                        &|| cancel.check(),
                    )?;
                    sidecar = Some(proof);
                    current = coordinator.task.clone();
                }
                Err(cause) if cause.is::<super::authority::RetrySidecarPreparationFailure>() => {
                    return Err(cause);
                }
                Err(cause) => fail(
                    &mut stages.lyrics,
                    error(
                        super::DownloadErrorKind::Disk,
                        format!("{cause:#}").chars().take(1024).collect::<String>(),
                    ),
                ),
            }
        }
        let latest = repository
            .download_task(&task.id)?
            .ok_or_else(|| anyhow::anyhow!("Download retry task disappeared"))?;
        let latest_intent = latest
            .commit_journal
            .as_ref()
            .and_then(|j| j.retry.as_ref())
            .ok_or_else(|| anyhow::anyhow!("Download retry intent disappeared"))?;
        if recovering
            && intent.selected.lyrics
            && latest.stages.lyrics.artifact.is_none()
            && let Some(allocation) = latest_intent
                .reservation(RetryArtifactRole::Lyrics)?
                .allocation
                .as_ref()
        {
            let record = repository
                .download_owned_artifacts(Some(&task.id))?
                .into_iter()
                .find(|a| a.allocation == *allocation)
                .ok_or_else(|| {
                    anyhow::anyhow!("Download lyric retry recovery accounting is unavailable")
                })?;
            if record.phase == "verified" {
                let sha = record
                    .sha256
                    .as_ref()
                    .ok_or_else(|| anyhow::anyhow!("Download lyric retry digest missing"))?;
                match grant.verify_owned_artifact(
                    &allocation.record.final_basename,
                    &allocation.record.identity,
                    record.actual_bytes,
                    sha,
                    &cancel,
                ) {
                    Ok(verified) => {
                        let proof = DownloadArtifactProof {
                            basename: verified.basename().into(),
                            identity: verified.identity().clone(),
                            bytes: verified.bytes(),
                            sha256: verified.sha256().into(),
                        };
                        stages.lyrics.state = DownloadStageState::Succeeded;
                        stages.lyrics.error = None;
                        current = repository.commit_download_retry_lyrics(
                            &task.id,
                            &intent.operation_id,
                            latest.revision,
                            latest.operation_epoch,
                            &proof,
                            &stages,
                            &|| cancel.check(),
                        )?;
                        sidecar = Some(proof);
                    }
                    Err(_) => {
                        restart_required = true;
                    }
                }
            } else {
                restart_required = true;
            }
        }
        let latest = repository
            .download_task(&task.id)?
            .ok_or_else(|| anyhow::anyhow!("Download retry task disappeared"))?;
        let latest_intent = latest
            .commit_journal
            .as_ref()
            .and_then(|j| j.retry.as_ref())
            .ok_or_else(|| anyhow::anyhow!("Download retry intent disappeared"))?;
        for artifact in
            retirement_allocations(latest_intent, latest.stages.lyrics.artifact.is_some())
        {
            retire_artifact(
                &repository,
                &grant,
                &task.id,
                &intent.operation_id,
                artifact,
                &mut prepared_retirements,
            )?;
        }
        current = repository
            .download_task(&task.id)?
            .ok_or_else(|| anyhow::anyhow!("Download retry task disappeared"))?;
        owner.take();
        retirement_complete = true;
        Ok(())
    })();
    let mut failure = result.err();
    if let Some(error) = &failure
        && !error.is::<crate::library::tag_install::UnacknowledgedTagArtifact>()
        && !error.is::<RetryRetirementAckFailure>()
        && !error.is::<super::authority::RetrySidecarPreparationFailure>()
        && !error.is::<super::authority::RetryArtifactRemovalPending>()
        && !matches!(
            error.downcast_ref::<crate::library::tag_install::TagInstallError>(),
            Some(crate::library::tag_install::TagInstallError::Unrestored { .. })
        )
    {
        let cleanup = (|| -> Result<()> {
            let latest = repository
                .download_task(&task.id)?
                .ok_or_else(|| anyhow::anyhow!("Download retry task disappeared"))?;
            let intent = latest
                .commit_journal
                .as_ref()
                .and_then(|j| j.retry.as_ref())
                .ok_or_else(|| anyhow::anyhow!("Download retry intent disappeared"))?;
            if let Some(owner) = &mut owner {
                owner.accept_installed(
                    latest
                        .output_audio
                        .clone()
                        .ok_or_else(|| anyhow::anyhow!("Download retry output disappeared"))?,
                    &super::DownloadCancel::default(),
                )?;
                let grant = owner.grant().clone();
                for allocation in
                    retirement_allocations(intent, latest.stages.lyrics.artifact.is_some())
                {
                    retire_artifact(
                        &repository,
                        &grant,
                        &task.id,
                        &intent.operation_id,
                        allocation,
                        &mut prepared_retirements,
                    )?;
                }
            } else {
                ensure!(
                    intent.artifacts.iter().all(|a| a.allocation.is_none()),
                    "Download retry private artifacts still need recovery"
                );
            }
            current = repository.download_task(&task.id)?.unwrap();
            stages = current.stages.clone();
            Ok(())
        })();
        match cleanup {
            Ok(()) => {
                owner.take();
                retirement_complete = true;
                if current.stages.all_satisfied() {
                    failure = None;
                }
            }
            Err(cause) if cause.is::<RetryRetirementAckFailure>() => {
                failure = Some(cause);
            }
            Err(_) => {}
        }
    }
    RetryWorkResult {
        task: current,
        stages,
        sidecar,
        error: failure,
        restart_required,
        retirement_complete: retirement_complete && prepared_retirements.is_empty(),
        owner,
        retirements: prepared_retirements,
        retired_witnesses,
    }
}

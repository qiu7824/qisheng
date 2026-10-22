//! Prepared tag and lyric outputs preserve their original transfer independently.

use super::{
    AudioVerification, COPY_BUFFER_BYTES, CollisionPolicy, CommittedAudio, CommittedSidecar,
    DownloadCancel, OwnedPart, OwnedPartRecord, TransferCheckpoint, VerifiedOwnedAudio,
    lyrics::{LyricArtifact, MAX_DOWNLOAD_LYRIC_OUTPUT_BYTES},
};
use crate::library::{
    scan::ScanContext,
    tag_write::{LocalTagPatch, TagWriteVerification, write_local_tags},
};
use anyhow::{Result, ensure};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{
    fmt,
    time::{Duration, Instant},
};

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct OutputDigest {
    pub bytes: u64,
    pub sha256: String,
}

pub struct PreparedTaggedOutput {
    candidate: VerifiedOwnedAudio,
    original_transfer: TransferCheckpoint,
    output_digest: OutputDigest,
    tag_verification: TagWriteVerification,
    deadline: Instant,
}
impl fmt::Debug for PreparedTaggedOutput {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PreparedTaggedOutput")
            .field("output_digest", &self.output_digest)
            .finish()
    }
}
impl PreparedTaggedOutput {
    pub(crate) fn producer_object(&self) -> super::DownloadProducerObject {
        self.candidate.producer_object()
    }
    pub fn original_transfer(&self) -> &TransferCheckpoint {
        &self.original_transfer
    }
    pub fn output_digest(&self) -> &OutputDigest {
        &self.output_digest
    }
    pub fn owned_record(&self) -> &OwnedPartRecord {
        self.candidate.owned_record()
    }
    pub fn verification(&self) -> &AudioVerification {
        self.candidate.verification()
    }
    pub fn tags(&self) -> &TagWriteVerification {
        &self.tag_verification
    }
    pub fn commit(self) -> Result<CommittedTaggedOutput> {
        ensure!(
            Instant::now() < self.deadline,
            "Prepared tag output deadline expired before commit"
        );
        let output = self.candidate.commit()?;
        ensure!(
            output.checkpoint.sha256 == self.output_digest.sha256
                && output.checkpoint.bytes == self.output_digest.bytes
                && output.checkpoint.representation.is_none(),
            "Committed tagged artifact differs from its independently prepared digest"
        );
        Ok(CommittedTaggedOutput {
            original_transfer: self.original_transfer,
            output,
            output_digest: self.output_digest,
            tags: self.tag_verification,
        })
    }
}
pub struct CommittedTaggedOutput {
    pub original_transfer: TransferCheckpoint,
    pub output: CommittedAudio,
    pub output_digest: OutputDigest,
    pub tags: TagWriteVerification,
}

pub fn prepare_tagged_output(
    _original: &VerifiedOwnedAudio,
    _patch: &LocalTagPatch,
    _cancel: &DownloadCancel,
    _timeout: Duration,
) -> Result<PreparedTaggedOutput> {
    Err(super::CreationJournalRequired.into())
}
pub fn prepare_tagged_output_journaled(
    original: &VerifiedOwnedAudio,
    patch: &LocalTagPatch,
    cancel: &DownloadCancel,
    timeout: Duration,
    journal: &dyn super::artifact::OwnedArtifactJournal,
) -> Result<PreparedTaggedOutput> {
    ensure!(
        !timeout.is_zero() && timeout <= Duration::from_secs(30),
        "Tag preparation deadline exceeds its bound"
    );
    let context = ScanContext::new(timeout);
    cancel.check()?;
    let extension = original
        .checkpoint()
        .representation
        .as_ref()
        .map(|identity| identity.actual_quality.extension())
        .unwrap_or_else(|| {
            original
                .owned_record()
                .final_basename
                .rsplit('.')
                .next()
                .unwrap_or("")
        });
    ensure!(
        matches!(extension, "mp3" | "flac"),
        "UnsupportedCapability: WAV and APE tag embedding is outside the baseline"
    );
    let original_transfer = original.checkpoint().clone();
    let mut copy = original.copy_for_postprocess_journaled(cancel, journal)?;
    context.check()?;
    let tags = {
        let mut file = copy.rewrite_io(cancel.clone())?;
        write_local_tags(&mut file, patch, &context)?
    };
    cancel.check()?;
    let candidate = original.seal_postprocessed_copy(copy, extension, cancel)?;
    context.check()?;
    let output_digest = OutputDigest {
        bytes: candidate.checkpoint().bytes,
        sha256: candidate.checkpoint().sha256.clone(),
    };
    ensure!(
        output_digest.sha256 == tags.body_sha256 && output_digest.bytes == tags.size_bytes,
        "Tag write and sealed file digests differ"
    );
    Ok(PreparedTaggedOutput {
        candidate,
        original_transfer,
        output_digest,
        tag_verification: tags,
        deadline: context.deadline,
    })
}

pub struct PreparedLyricSidecar {
    part: OwnedPart,
    digest: OutputDigest,
    cancel: DownloadCancel,
}
impl fmt::Debug for PreparedLyricSidecar {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PreparedLyricSidecar")
            .field("digest", &self.digest)
            .finish()
    }
}
impl PreparedLyricSidecar {
    pub(crate) fn producer_object(&self) -> super::DownloadProducerObject {
        self.part.producer_object()
    }
    pub fn output_digest(&self) -> &OutputDigest {
        &self.digest
    }
    pub fn owned_record(&self) -> &OwnedPartRecord {
        self.part.record()
    }
    pub fn commit(self) -> Result<CommittedSidecar> {
        self.part
            .commit_sidecar(self.digest.bytes, &self.digest.sha256, &self.cancel)
    }
}

/// This separate sidecar stage does not mutate, transfer or re-tag its audio.
pub fn prepare_lyric_sidecar(
    _original: &VerifiedOwnedAudio,
    _artifact: &LyricArtifact,
    _collision: CollisionPolicy,
    _cancel: &DownloadCancel,
) -> Result<PreparedLyricSidecar> {
    Err(super::CreationJournalRequired.into())
}
pub fn prepare_lyric_sidecar_journaled(
    original: &VerifiedOwnedAudio,
    artifact: &LyricArtifact,
    collision: CollisionPolicy,
    cancel: &DownloadCancel,
    journal: &dyn super::artifact::OwnedArtifactJournal,
) -> Result<PreparedLyricSidecar> {
    cancel.check()?;
    ensure!(
        artifact.has_ordinary_lyrics,
        "Unavailable: lyric sidecar requires ordinary lyric text"
    );
    ensure!(
        !artifact.bytes.is_empty() && artifact.bytes.len() <= MAX_DOWNLOAD_LYRIC_OUTPUT_BYTES + 3,
        "Lyric artifact exceeds its byte bound"
    );
    let plan = original.plan().lyric_sidecar_plan(collision)?;
    let mut part = original.create_sidecar_journaled(
        plan,
        MAX_DOWNLOAD_LYRIC_OUTPUT_BYTES as u64 + 3,
        journal,
        cancel,
    )?;
    for chunk in artifact.bytes.chunks(COPY_BUFFER_BYTES) {
        part.append(chunk, cancel, MAX_DOWNLOAD_LYRIC_OUTPUT_BYTES as u64 + 3)?;
    }
    part.sync()?;
    cancel.check()?;
    let checkpoint = part.checkpoint(cancel, None, None, true)?;
    let sha256 = format!("{:x}", Sha256::digest(&artifact.bytes));
    ensure!(
        checkpoint.sha256 == sha256 && checkpoint.bytes == artifact.bytes.len() as u64,
        "Owned lyric temporary differs from its selected encoded artifact"
    );
    Ok(PreparedLyricSidecar {
        part,
        digest: OutputDigest {
            bytes: checkpoint.bytes,
            sha256,
        },
        cancel: cancel.clone(),
    })
}

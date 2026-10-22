//! Admission estimates borrow actual unchanged file owners; estimates are not
//! installation or rollback evidence and cannot construct a prepared asset.
use super::*;

pub struct TagPersistenceBudget<'a> {
    writer: &'a NativeWriterToken,
    original_metadata: &'a TagMetadataSnapshot,
    before: TagFileObservation,
    candidate: Option<(
        &'a TagNativeFile,
        &'a TagMetadataSnapshot,
        TagFileObservation,
    )>,
}
impl<'a> TagPersistenceBudget<'a> {
    pub(super) fn original(
        writer: &'a NativeWriterToken,
        metadata: &'a TagMetadataSnapshot,
    ) -> Result<Self> {
        let value = Self {
            writer,
            original_metadata: metadata,
            before: observe_file(&writer.file)?,
            candidate: None,
        };
        value.check()?;
        Ok(value)
    }
    pub(super) fn candidate(
        writer: &'a NativeWriterToken,
        original: &'a TagMetadataSnapshot,
        file: &'a TagNativeFile,
        metadata: &'a TagMetadataSnapshot,
    ) -> Result<Self> {
        let mut value = Self::original(writer, original)?;
        value.candidate = Some((file, metadata, observe_file(file)?));
        value.check()?;
        Ok(value)
    }
    pub fn selection(&self) -> &TagWriteSelection {
        &self.writer.selection
    }
    pub fn check(&self) -> Result<()> {
        self.writer.check()?;
        ensure!(
            observe_file(&self.writer.file)? == self.before,
            LibraryError::conflict("Actual original changed during persistent metadata admission")
        );
        if let Some((file, _, observation)) = &self.candidate {
            ensure!(
                normal_file(file)? && links(file)? == 1 && observe_file(file)? == *observation,
                LibraryError::conflict(
                    "Actual candidate changed during persistent metadata admission"
                )
            );
        }
        Ok(())
    }
    pub(crate) fn estimates(&self) -> Vec<TagInstallationReceipt> {
        let mut values = vec![self.estimate(self.original_metadata, self.before.size_bytes)];
        if let Some((_, metadata, observation)) = &self.candidate {
            values.push(self.estimate(metadata, observation.size_bytes));
        }
        values
    }
    fn estimate(&self, metadata: &TagMetadataSnapshot, size: u64) -> TagInstallationReceipt {
        let mut after = self.before.clone();
        // Fixed numeric upper widths reserve future native timestamps/revisions
        // without describing them as actual observations or live evidence.
        after.size_bytes = size;
        after.modified_ms = Some(u64::MAX);
        after.modified_nanos = Some(u64::MAX);
        let hash = "f".repeat(64);
        TagInstallationReceipt {
            operation_id: self.selection().operation_id.clone(),
            asset_id: self.selection().asset_id.clone(),
            path: self.before.path.clone(),
            file_identity: self.before.identity.clone(),
            old_body_sha256: hash.clone(),
            new_body_sha256: hash.clone(),
            audio_sha256: hash.clone(),
            size_bytes: size,
            committed_binding_revision: Some(i64::MAX as u64),
            installed: Some(TagInstalledFacts {
                before: self.before.clone(),
                after,
                metadata: metadata.clone(),
                old_body_sha256: hash.clone(),
                new_body_sha256: hash.clone(),
                audio_sha256: hash,
                format: metadata.format,
            }),
        }
    }
}
fn maximum_identity() -> FileIdentity {
    FileIdentity {
        kind: "windows_file_id".into(),
        volume: u64::MAX.to_string(),
        file_id: "f".repeat(32),
    }
}
fn encoded(manifest: &Manifest) -> Result<u64> {
    let bytes = serde_json::to_vec(manifest)?.len();
    ensure!(
        bytes <= MAX_RECORD,
        LibraryError::budget(
            "Persistent tag metadata record exceeds 1 MiB before original mutation"
        )
    );
    Ok(bytes as u64 + 36)
}
pub(super) fn check_original(
    budget: &TagPersistenceBudget<'_>,
    coordinator: &dyn TagInstallCoordinator,
) -> Result<()> {
    budget.check()?;
    let original = budget.estimates().remove(0);
    let mut simple_receipt = original.clone();
    simple_receipt.installed = None;
    let identity = maximum_identity();
    let manifest = Manifest {
        selection: budget.selection().clone(),
        phase: TagInstallPhase::CandidateVerified,
        journal_directory_identity: identity.clone(),
        backup_identity: Some(identity.clone()),
        candidate_identity: Some(identity),
        old_sha256: Some("f".repeat(64)),
        old_size: Some(u64::MAX),
        receipt: Some(simple_receipt),
        backup_basename: coordinator
            .artifact_basename(budget.selection(), TagInstallArtifactRole::Backup)?,
        candidate_basename: coordinator
            .artifact_basename(budget.selection(), TagInstallArtifactRole::Candidate)?,
        original_observation: Some(budget.before.clone()),
        restored_facts: original.installed,
    };
    let restored = encoded(&manifest)?;
    let mut small = manifest.clone();
    small.restored_facts = None;
    let prefix = encoded(&small)?;
    ensure!(
        restored
            .checked_add(
                prefix
                    .checked_mul(4)
                    .context("Tag early journal admission overflow")?
            )
            .is_some_and(
                |bytes| bytes <= coordinator.artifact_limit(TagInstallArtifactRole::Journal)
            ),
        LibraryError::budget(
            "Tag journal cannot hold mandatory unchanged-original recovery before allocation"
        )
    );
    coordinator.check_persistence_budget(budget.selection(), budget)?;
    budget.check()
}
pub(super) fn check_candidate(
    budget: &TagPersistenceBudget<'_>,
    journal: &Journal,
    coordinator: &dyn TagInstallCoordinator,
) -> Result<()> {
    budget.check()?;
    let mut estimates = budget.estimates();
    let old = estimates.remove(0);
    let candidate = estimates
        .pop()
        .context("Actual candidate metadata admission is absent")?;
    let mut manifest = journal.manifest.clone();
    manifest.phase = TagInstallPhase::Installing;
    let installing = encoded(&manifest)?;
    manifest.receipt = Some(candidate);
    let mut success = installing;
    for phase in [
        TagInstallPhase::DiskVerified,
        TagInstallPhase::RepoCommitted,
        TagInstallPhase::Published,
    ] {
        manifest.phase = phase;
        success = success
            .checked_add(encoded(&manifest)?)
            .context("Tag journal admission size overflow")?;
    }
    manifest.phase = TagInstallPhase::DiskVerified;
    let mut rollback = installing + encoded(&manifest)?;
    manifest.phase = TagInstallPhase::Restoring;
    rollback = rollback
        .checked_add(encoded(&manifest)?)
        .context("Tag rollback journal size overflow")?;
    manifest.restored_facts = old.installed;
    manifest.phase = TagInstallPhase::Restored;
    rollback = rollback
        .checked_add(encoded(&manifest)?)
        .context("Tag rollback journal size overflow")?;
    // CandidateVerified is still appended before the original mutation.
    manifest = journal.manifest.clone();
    manifest.phase = TagInstallPhase::CandidateVerified;
    let before_mutation = encoded(&manifest)?;
    let required = before_mutation
        .checked_add(success.max(rollback))
        .context("Tag durable journal size overflow")?;
    ensure!(
        journal.records + 5 <= MAX_RECORDS
            && journal
                .file
                .metadata()?
                .len()
                .checked_add(required)
                .is_some_and(|bytes| bytes <= journal.maximum_bytes),
        LibraryError::budget(
            "Tag journal reservation cannot hold commit and mandatory rollback before original mutation"
        )
    );
    coordinator.check_persistence_budget(budget.selection(), budget)?;
    budget.check()
}

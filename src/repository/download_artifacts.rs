use super::*;
use crate::download::{
    MAX_OWNED_PART_BYTES, OwnedPartRecord, TransferCheckpoint,
    artifact::{OwnedArtifactAllocation, OwnedArtifactKind},
};
use rusqlite::params;

pub(super) mod creation_intent;

#[derive(Clone, Debug)]
pub struct DownloadOwnedArtifact {
    pub allocation: OwnedArtifactAllocation,
    pub phase: String,
    pub actual_bytes: u64,
    pub reserved_bytes: u64,
    pub sha256: Option<String>,
}
impl Repository {
    pub fn validate_download_cleanup_intent(
        &self,
        intent: &crate::download::cleanup::CleanupIntent,
        acknowledge: bool,
        check: &dyn Fn() -> Result<()>,
    ) -> Result<()> {
        intent.validate()?;
        check()?;
        let mut connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("Repository lock poisoned"))?;
        schema::validate_current(&connection)?;
        let tx = connection.transaction()?;
        let allocation = &intent.allocation;
        let artifact = artifact_at(&tx, &allocation.record.nonce)?
            .context("Cleanup original allocation is unavailable")?;
        ensure!(
            artifact.allocation == *allocation
                && (artifact.phase == "verified" || acknowledge && artifact.phase == "removed"),
            "Cleanup original allocation or accounting phase changed"
        );
        let document: String = tx.query_row(
            "SELECT document FROM download_tasks WHERE id=?1",
            [&allocation.context.task_id],
            |row| row.get(0),
        )?;
        let task: crate::download::task_model::DownloadTask = decode_document(&document)?;
        task.validate()?;
        ensure!(
            task.profile_id == allocation.context.profile_id
                && task.revision >= intent.task_revision
                && task.operation_epoch >= allocation.context.operation_epoch
                && task.audio_available
                && task.owned_part.is_none()
                && task.commit_journal.is_none()
                && task.output_audio.as_ref() == Some(&intent.final_output),
            "Cleanup task output or execution context changed"
        );
        let checkpoint = task
            .original_transfer
            .as_ref()
            .context("Cleanup transfer checkpoint is unavailable")?;
        ensure!(
            checkpoint.completed_body
                && checkpoint.bytes == intent.raw_bytes
                && checkpoint.sha256 == intent.raw_sha256
                && task.original_representation == checkpoint.representation,
            "Cleanup original checkpoint changed"
        );
        let provenance = checkpoint
            .representation
            .as_ref()
            .context("Cleanup source provenance is unavailable")?;
        ensure!(
            provenance.profile_id == allocation.context.profile_id
                && provenance.task_id == allocation.context.task_id
                && provenance.source_id == allocation.context.source_id
                && provenance.source_content_sha256 == allocation.context.source_content_sha256
                && provenance.policy_epoch == allocation.context.policy_epoch
                && provenance.actual_quality == task.actual_quality,
            "Cleanup representation differs from its original allocation"
        );
        if artifact.phase == "verified" {
            ensure!(
                artifact.actual_bytes == intent.raw_bytes
                    && artifact.sha256.as_deref() == Some(intent.raw_sha256.as_str()),
                "Cleanup accounting differs from its original body proof"
            );
        }
        let grant_document: String = tx.query_row(
            "SELECT document FROM download_destination_grants WHERE id=?1",
            [&task.destination.grant_id],
            |row| row.get(0),
        )?;
        let grant: crate::download::task_model::StoredDestinationGrant =
            decode_document(&grant_document)?;
        task.require_current_destination(&grant)?;
        let group = if task.destination.relative_components.len() == 2 {
            Some(task.destination.relative_components[0].clone())
        } else {
            None
        };
        let working = group
            .as_ref()
            .map_or_else(|| grant.path.clone(), |group| grant.path.join(group));
        ensure!(
            group == allocation.relative_group
                && task.destination.root_identity == allocation.selected_root_identity
                && task.destination.grant_epoch == allocation.record.destination_epoch
                && crate::domain::music::normalize_local_path(&working)
                    == crate::domain::music::normalize_local_path(
                        &allocation.working_directory_path
                    ),
            "Cleanup destination binding changed"
        );
        check()?;
        tx.commit()?;
        Ok(())
    }
    /// Records a restart reported by the producer holding the actual reset file.
    /// The producer must keep that zero-length handle unchanged through this
    /// transaction; neither the checkpoint nor this row establishes file authority.
    pub fn rebind_download_transfer_artifact(
        &self,
        allocation: &OwnedArtifactAllocation,
        empty_checkpoint: &TransferCheckpoint,
        check: &dyn Fn() -> Result<()>,
    ) -> Result<()> {
        allocation.validate()?;
        ensure!(
            allocation.kind == OwnedArtifactKind::Transfer,
            "Only an original transfer artifact can be rebound"
        );
        ensure!(
            empty_checkpoint.bytes == 0
                && empty_checkpoint.sha256
                    == "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
                && !empty_checkpoint.completed_body
                && empty_checkpoint.strong_etag.is_none()
                && empty_checkpoint.remote_total.is_none(),
            "Transfer rebind requires a fresh empty checkpoint"
        );
        let representation = empty_checkpoint
            .representation
            .as_ref()
            .context("Empty transfer checkpoint has no current representation")?;
        representation.validate()?;
        let document = encode_document(allocation)?;
        ensure!(
            document.len() <= 65536,
            "Owned artifact record exceeds its bound"
        );

        let mut connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("Repository lock poisoned"))?;
        schema::validate_current(&connection)?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        check()?;
        let old = artifact_at(&tx, &allocation.record.nonce)?
            .context("Original transfer allocation is unavailable")?;
        ensure!(
            matches!(old.phase.as_str(), "allocated" | "verified"),
            "A terminal transfer artifact cannot be rebound"
        );
        let previous = &old.allocation;
        ensure!(
            previous.kind == OwnedArtifactKind::Transfer
                && previous.record == allocation.record
                && previous.working_directory_path == allocation.working_directory_path
                && previous.working_directory_identity == allocation.working_directory_identity
                && previous.selected_root_identity == allocation.selected_root_identity
                && previous.relative_group == allocation.relative_group
                && previous.reserved_max_bytes == allocation.reserved_max_bytes
                && previous.context.profile_id == allocation.context.profile_id
                && previous.context.task_id == allocation.context.task_id,
            "Transfer rebind cannot change its original object, namespace, role or reservation"
        );

        let task_document: String = tx.query_row(
            "SELECT document FROM download_tasks WHERE id=?1",
            [&allocation.context.task_id],
            |row| row.get(0),
        )?;
        let task: crate::download::task_model::DownloadTask = decode_document(&task_document)?;
        task.validate()?;
        ensure!(
            task.id == allocation.context.task_id
                && task.profile_id == allocation.context.profile_id
                && task.operation_epoch == allocation.context.operation_epoch
                && !task.audio_available
                && task.output_audio.is_none()
                && task.commit_journal.is_none()
                && task.owned_part.as_ref() == Some(&allocation.record),
            "Transfer rebind differs from the current unpublished task ownership"
        );
        let grant_document: String = tx.query_row(
            "SELECT document FROM download_destination_grants WHERE id=?1",
            [&task.destination.grant_id],
            |row| row.get(0),
        )?;
        let grant: crate::download::task_model::StoredDestinationGrant =
            decode_document(&grant_document)?;
        task.require_current_destination(&grant)?;
        let group = if task.destination.relative_components.len() == 2 {
            Some(task.destination.relative_components[0].clone())
        } else {
            None
        };
        let directory = group
            .as_ref()
            .map_or_else(|| grant.path.clone(), |group| grant.path.join(group));
        ensure!(
            task.destination.root_identity == allocation.selected_root_identity
                && task.destination.grant_epoch == allocation.record.destination_epoch
                && task.destination.relative_components.last()
                    == Some(&allocation.record.final_basename)
                && group == allocation.relative_group
                && crate::domain::music::normalize_local_path(&directory)
                    == crate::domain::music::normalize_local_path(
                        &allocation.working_directory_path
                    )
                && (group.is_some() || allocation.working_directory_identity == grant.identity),
            "Transfer rebind destination differs from its current selected plan"
        );
        let bookmark = task
            .url_bookmark
            .as_ref()
            .context("Transfer rebind has no current URL context")?;
        ensure!(
            bookmark.context == allocation.context,
            "Transfer rebind source context differs from its current URL"
        );
        let record_digest = crate::domain::media::track_record_fingerprint(
            task.resolved_record
                .as_ref()
                .unwrap_or(&task.original_record),
        )?;
        ensure!(
            representation.profile_id == allocation.context.profile_id
                && representation.task_id == allocation.context.task_id
                && representation.source_id == allocation.context.source_id
                && representation.source_content_sha256 == allocation.context.source_content_sha256
                && representation.policy_epoch == allocation.context.policy_epoch
                && representation.exact_url_sha256 == bookmark.sha256
                && representation.resolved_track_sha256 == record_digest
                && representation.actual_quality == task.actual_quality,
            "Empty transfer checkpoint differs from the current source, URL, track or quality"
        );
        if previous.context == allocation.context {
            ensure!(
                old.phase == "allocated" && old.actual_bytes == 0 && old.sha256.is_none(),
                "A same-context rebind cannot erase verified transfer bytes"
            );
            check_artifact_storage(&tx)?;
            check()?;
            tx.commit()?;
            return Ok(());
        }
        ensure!(
            allocation.context.operation_epoch > previous.context.operation_epoch,
            "Transfer rebind requires a newer operation epoch"
        );
        let changed = tx.execute(
            "UPDATE download_owned_artifacts SET operation_epoch=?3,phase='allocated',actual_bytes=0,reserved_bytes=?4,sha256=NULL,document=?5,byte_size=?6 WHERE nonce=?1 AND task_id=?2 AND operation_epoch=?7 AND phase IN ('allocated','verified')",
            params![allocation.record.nonce, allocation.context.task_id,
                checked_revision(allocation.context.operation_epoch)?, i64::try_from(allocation.reserved_max_bytes)?,
                document, document.len() as i64, checked_revision(previous.context.operation_epoch)?],
        )?;
        ensure!(
            changed == 1,
            "Original transfer accounting changed during rebind"
        );
        check_artifact_storage(&tx)?;
        check()?;
        tx.commit()?;
        Ok(())
    }

    /// Durable accounting facts alone do not provide a file handle or write grant.
    pub fn record_download_artifact(
        &self,
        allocation: &OwnedArtifactAllocation,
        check: &dyn Fn() -> Result<()>,
    ) -> Result<()> {
        allocation.validate()?;
        check()?;
        let mut connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("Repository lock poisoned"))?;
        schema::validate_current(&connection)?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let task_document: String = tx.query_row(
            "SELECT document FROM download_tasks WHERE id=?1",
            [&allocation.context.task_id],
            |row| row.get(0),
        )?;
        let task: crate::download::task_model::DownloadTask = decode_document(&task_document)?;
        task.validate()?;
        ensure!(
            task.profile_id == allocation.context.profile_id
                && task.operation_epoch == allocation.context.operation_epoch,
            "Owned artifact operation context changed"
        );
        ensure!(
            task.destination.root_identity == allocation.selected_root_identity
                && task.destination.grant_epoch == allocation.record.destination_epoch,
            "Owned artifact destination context changed"
        );
        let grant_document: String = tx.query_row(
            "SELECT document FROM download_destination_grants WHERE id=?1",
            [&task.destination.grant_id],
            |row| row.get(0),
        )?;
        let grant: crate::download::task_model::StoredDestinationGrant =
            decode_document(&grant_document)?;
        task.require_current_destination(&grant)?;
        let group = if task.destination.relative_components.len() == 2 {
            Some(task.destination.relative_components[0].clone())
        } else {
            None
        };
        ensure!(
            group == allocation.relative_group,
            "Owned artifact group differs from task plan"
        );
        let directory = group
            .as_ref()
            .map_or_else(|| grant.path.clone(), |group| grant.path.join(group));
        ensure!(
            crate::domain::music::normalize_local_path(&directory)
                == crate::domain::music::normalize_local_path(&allocation.working_directory_path),
            "Owned artifact directory differs from its selected plan"
        );
        let leaf = task
            .destination
            .relative_components
            .last()
            .context("Download leaf missing")?;
        let expected_leaf = if allocation.kind == OwnedArtifactKind::LyricSidecar {
            ensure!(
                task.settings.download_lyrics,
                "Lyric artifact was not requested"
            );
            format!(
                "{}.lrc",
                leaf.rsplit_once('.').map(|(stem, _)| stem).unwrap_or(leaf)
            )
        } else {
            leaf.clone()
        };
        ensure!(
            allocation.record.final_basename == expected_leaf,
            "Owned artifact output differs from its planned role"
        );
        let bookmark = task
            .url_bookmark
            .as_ref()
            .context("Owned artifact has no current source context")?;
        ensure!(
            bookmark.context == allocation.context,
            "Owned artifact source execution context changed"
        );
        let document = encode_document(allocation)?;
        ensure!(
            document.len() <= 65536,
            "Owned artifact record exceeds its bound"
        );
        let existing: Option<(String, String)> = tx
            .query_row(
                "SELECT document,phase FROM download_owned_artifacts WHERE nonce=?1",
                [&allocation.record.nonce],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        if let Some((existing, phase)) = existing {
            ensure!(
                existing == document,
                "Owned artifact nonce belongs to another object"
            );
            ensure!(
                phase != "removed",
                "Removed owned artifact nonce cannot be reused"
            );
            check()?;
            tx.commit()?;
            return Ok(());
        }
        let pending = creation_intent::intent_at(&tx, &allocation.record.nonce)?
            .context("Actual creation has no durable namespace intent")?;
        pending.require_created(allocation)?;
        tx.execute("INSERT INTO download_owned_artifacts(nonce,task_id,operation_epoch,phase,actual_bytes,reserved_bytes,sha256,document,byte_size) VALUES(?1,?2,?3,'allocated',0,?4,NULL,?5,?6)",
            params![allocation.record.nonce,allocation.context.task_id,checked_revision(allocation.context.operation_epoch)?,i64::try_from(allocation.reserved_max_bytes)?,document,document.len() as i64])?;
        let consumed = tx.execute("DELETE FROM download_allocation_intents WHERE nonce=?1 AND task_id=?2 AND operation_epoch=?3",
            params![allocation.record.nonce, allocation.context.task_id, checked_revision(allocation.context.operation_epoch)?])?;
        ensure!(
            consumed == 1,
            "Actual creation acknowledgement lost its namespace intent"
        );
        check_artifact_storage(&tx)?;
        check()?;
        tx.commit()?;
        Ok(())
    }
    pub fn download_owned_artifacts(
        &self,
        task_id: Option<&str>,
    ) -> Result<Vec<DownloadOwnedArtifact>> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("Repository lock poisoned"))?;
        schema::validate_current(&connection)?;
        let mut stmt=connection.prepare("SELECT nonce,task_id,operation_epoch,phase,actual_bytes,reserved_bytes,sha256,document,byte_size FROM download_owned_artifacts WHERE (?1 IS NULL OR task_id=?1) ORDER BY nonce")?;
        let mut values = Vec::new();
        let mut bytes = 0usize;
        for row in stmt.query_map([task_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, i64>(5)?,
                row.get::<_, Option<String>>(6)?,
                row.get::<_, String>(7)?,
                row.get::<_, i64>(8)?,
            ))
        })? {
            let (nonce, owner, epoch, phase, actual, reserved, sha256, document, length) = row?;
            let allocation: OwnedArtifactAllocation = decode_document(&document)?;
            allocation.validate()?;
            ensure!(
                allocation.record.nonce == nonce
                    && allocation.context.task_id == owner
                    && checked_revision(allocation.context.operation_epoch)? == epoch
                    && document.len() as i64 == length,
                "Owned artifact SQL identity differs from document"
            );
            bytes = bytes.saturating_add(document.len());
            ensure!(
                values.len() < 400 && bytes <= 16 * 1024 * 1024,
                "Owned artifact page exceeds its bound"
            );
            let value = DownloadOwnedArtifact {
                allocation,
                phase,
                actual_bytes: u64::try_from(actual)?,
                reserved_bytes: u64::try_from(reserved)?,
                sha256,
            };
            validate_artifact_phase(&value)?;
            values.push(value);
        }
        Ok(values)
    }
    pub fn verify_download_artifact(
        &self,
        nonce: &str,
        epoch: u64,
        bytes: u64,
        sha256: &str,
        check: &dyn Fn() -> Result<()>,
    ) -> Result<()> {
        ensure!(
            sha256.len() == 64 && sha256.bytes().all(|byte| byte.is_ascii_hexdigit()),
            "Invalid artifact digest"
        );
        check()?;
        let mut connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("Repository lock poisoned"))?;
        schema::validate_current(&connection)?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let changed=tx.execute("UPDATE download_owned_artifacts SET phase='verified',actual_bytes=?3,reserved_bytes=?3,sha256=?4 WHERE nonce=?1 AND operation_epoch=?2 AND reserved_bytes>=?3 AND phase IN ('allocated','verified')",
            params![nonce,checked_revision(epoch)?,i64::try_from(bytes)?,sha256])?;
        ensure!(
            changed == 1,
            "Owned artifact revision or reservation changed"
        );
        check_artifact_storage(&tx)?;
        check()?;
        tx.commit()?;
        Ok(())
    }
    pub fn publish_download_artifact(
        &self,
        nonce: &str,
        epoch: u64,
        bytes: u64,
        sha256: &str,
    ) -> Result<()> {
        let mut connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("Repository lock poisoned"))?;
        schema::validate_current(&connection)?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        publish_download_artifact_in_transaction(&tx, nonce, epoch, bytes, sha256, "")?;
        tx.commit()?;
        Ok(())
    }

    /// Acknowledges a receipt produced after the actual original handle was
    /// deleted, closed and checked absent. This method performs no file I/O and
    /// cannot establish physical removal or grant deletion authority itself.
    pub fn acknowledge_removed_download_original(
        &self,
        task_id: &str,
        allocation_epoch: u64,
        removed: &OwnedPartRecord,
    ) -> Result<()> {
        self.acknowledge_removed_download_original_checked(
            task_id,
            allocation_epoch,
            removed,
            &|| Ok(()),
        )
    }

    pub fn acknowledge_removed_download_original_checked(
        &self,
        task_id: &str,
        allocation_epoch: u64,
        removed: &OwnedPartRecord,
        check: &dyn Fn() -> Result<()>,
    ) -> Result<()> {
        ensure!(
            !task_id.is_empty() && task_id.len() <= 256 && !task_id.chars().any(char::is_control),
            "Invalid cleanup task identity"
        );
        ensure!(
            allocation_epoch > 0,
            "Invalid original artifact allocation epoch"
        );
        check()?;
        let mut connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("Repository lock poisoned"))?;
        schema::validate_current(&connection)?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let artifact = artifact_at(&tx, &removed.nonce)?
            .context("Original artifact allocation is unavailable")?;
        ensure!(
            artifact.allocation.kind == OwnedArtifactKind::Transfer
                && artifact.allocation.record == *removed
                && artifact.allocation.context.task_id == task_id
                && artifact.allocation.context.operation_epoch == allocation_epoch,
            "Removal receipt differs from its original allocation"
        );
        if artifact.phase == "removed" {
            check()?;
            tx.commit()?;
            return Ok(());
        }
        ensure!(
            artifact.phase == "verified",
            "Original artifact must be verified and unpublished before cleanup ACK"
        );
        let task_document: String = tx.query_row(
            "SELECT document FROM download_tasks WHERE id=?1",
            [task_id],
            |row| row.get(0),
        )?;
        let task: crate::download::task_model::DownloadTask = decode_document(&task_document)?;
        task.validate()?;
        let allocation = &artifact.allocation;
        ensure!(
            task.id == task_id
                && task.profile_id == allocation.context.profile_id
                && task.operation_epoch >= allocation_epoch,
            "Removal receipt task context or epoch changed"
        );
        let group = if task.destination.relative_components.len() == 2 {
            Some(task.destination.relative_components[0].clone())
        } else {
            None
        };
        ensure!(
            task.destination.root_identity == allocation.selected_root_identity
                && task.destination.grant_epoch == removed.destination_epoch
                && group == allocation.relative_group
                && task.destination.relative_components.last() == Some(&removed.final_basename),
            "Removal receipt destination differs from its task output plan"
        );
        ensure!(
            task.audio_available && task.owned_part.is_none() && task.commit_journal.is_none(),
            "Original cleanup requires durable audio output without pending ownership or commit journal"
        );
        let output = task
            .output_audio
            .as_ref()
            .context("Original cleanup has no durable task output")?;
        ensure!(
            output.identity != removed.identity,
            "Published final file cannot be acknowledged as a removed original"
        );
        let checkpoint = task
            .original_transfer
            .as_ref()
            .context("Original cleanup has no retained transfer checkpoint")?;
        ensure!(
            checkpoint.completed_body
                && checkpoint.bytes > 0
                && checkpoint.bytes == artifact.actual_bytes
                && artifact.sha256.as_deref() == Some(checkpoint.sha256.as_str()),
            "Removal receipt differs from its verified original transfer bytes or digest"
        );
        let representation = checkpoint
            .representation
            .as_ref()
            .context("Original cleanup has no retained representation identity")?;
        ensure!(
            task.original_representation.as_ref() == Some(representation)
                && representation.profile_id == allocation.context.profile_id
                && representation.task_id == allocation.context.task_id
                && representation.source_id == allocation.context.source_id
                && representation.source_content_sha256 == allocation.context.source_content_sha256
                && representation.policy_epoch == allocation.context.policy_epoch
                && representation.actual_quality == task.actual_quality,
            "Removal receipt source provenance differs from its allocation context"
        );
        let changed=tx.execute("UPDATE download_owned_artifacts SET phase='removed',actual_bytes=0,reserved_bytes=0 WHERE nonce=?1 AND task_id=?2 AND operation_epoch=?3 AND phase='verified' AND actual_bytes=?4 AND sha256=?5",
            params![removed.nonce,task_id,checked_revision(allocation_epoch)?,i64::try_from(artifact.actual_bytes)?,checkpoint.sha256])?;
        ensure!(changed == 1, "Original artifact cleanup accounting changed");
        check_artifact_storage(&tx)?;
        check()?;
        tx.commit()?;
        Ok(())
    }
}

pub(super) fn publish_download_artifact_in_transaction(
    tx: &rusqlite::Transaction<'_>,
    nonce: &str,
    epoch: u64,
    bytes: u64,
    sha256: &str,
    expected_task_id: &str,
) -> Result<OwnedArtifactKind> {
    let (artifact_document, task_document): (String, String) = tx.query_row(
        "SELECT a.document,t.document FROM download_owned_artifacts a JOIN download_tasks t ON t.id=a.task_id WHERE a.nonce=?1 AND a.operation_epoch=?2",
        params![nonce, checked_revision(epoch)?], |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let allocation: OwnedArtifactAllocation = decode_document(&artifact_document)?;
    allocation.validate()?;
    let task: crate::download::task_model::DownloadTask = decode_document(&task_document)?;
    task.validate()?;
    ensure!(
        expected_task_id.is_empty() || task.id == expected_task_id,
        "Artifact belongs to another publication task"
    );
    ensure!(
        allocation.context.task_id == task.id && allocation.context.profile_id == task.profile_id,
        "Artifact producer belongs to another task or profile"
    );
    let output = if allocation.kind == OwnedArtifactKind::LyricSidecar {
        task.stages.lyrics.artifact.as_ref()
    } else {
        task.output_audio.as_ref()
    };
    ensure!(
        output.is_some_and(|output| output.identity == allocation.record.identity
            && output.bytes == bytes
            && output.sha256 == sha256
            && output.basename == allocation.record.final_basename),
        "Artifact publication is not acknowledged by its task output"
    );
    let changed = tx.execute("UPDATE download_owned_artifacts SET phase='published',reserved_bytes=0 WHERE nonce=?1 AND operation_epoch=?2 AND actual_bytes=?3 AND sha256=?4 AND phase IN ('verified','published')",
        params![nonce, checked_revision(epoch)?, i64::try_from(bytes)?, sha256])?;
    ensure!(
        changed == 1,
        "Published artifact does not match its verified ownership record"
    );
    Ok(allocation.kind)
}
fn check_artifact_storage(tx: &Transaction<'_>) -> Result<()> {
    let(count,documents,reserved):(i64,i64,i64)=tx.query_row("SELECT count(*),coalesce(sum(byte_size),0),coalesce(sum(reserved_bytes),0) FROM (SELECT byte_size,reserved_bytes FROM download_owned_artifacts UNION ALL SELECT byte_size,reserved_bytes FROM download_allocation_intents)",[],|row|Ok((row.get(0)?,row.get(1)?,row.get(2)?)))?;
    ensure!(
        count <= 400 && documents <= 16 * 1024 * 1024 && reserved <= MAX_OWNED_PART_BYTES as i64,
        "Owned artifact profile quota exhausted"
    );
    super::download::check_retry_storage(tx)?;
    Ok(())
}

type DownloadArtifactRow = (String, i64, String, i64, i64, Option<String>, String, i64);
pub(super) fn artifact_at(
    connection: &Connection,
    nonce: &str,
) -> Result<Option<DownloadOwnedArtifact>> {
    let row:Option<DownloadArtifactRow>=connection.query_row(
        "SELECT task_id,operation_epoch,phase,actual_bytes,reserved_bytes,sha256,document,byte_size FROM download_owned_artifacts WHERE nonce=?1",[nonce],
        |row|Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?,row.get(4)?,row.get(5)?,row.get(6)?,row.get(7)?))).optional()?;
    row.map(
        |(task, epoch, phase, actual, reserved, sha256, document, bytes)| {
            let allocation: OwnedArtifactAllocation = decode_document(&document)?;
            allocation.validate()?;
            ensure!(
                allocation.record.nonce == nonce
                    && allocation.context.task_id == task
                    && checked_revision(allocation.context.operation_epoch)? == epoch
                    && document.len() as i64 == bytes,
                "Owned artifact SQL identity differs from its document"
            );
            let value = DownloadOwnedArtifact {
                allocation,
                phase,
                actual_bytes: u64::try_from(actual)?,
                reserved_bytes: u64::try_from(reserved)?,
                sha256,
            };
            validate_artifact_phase(&value)?;
            Ok(value)
        },
    )
    .transpose()
}

fn validate_artifact_phase(value: &DownloadOwnedArtifact) -> Result<()> {
    ensure!(
        value.actual_bytes <= value.allocation.reserved_max_bytes
            && value.reserved_bytes <= value.allocation.reserved_max_bytes,
        "Owned artifact exceeds its original reservation"
    );
    let digest = value
        .sha256
        .as_ref()
        .is_some_and(|sha| sha.len() == 64 && sha.bytes().all(|byte| byte.is_ascii_hexdigit()));
    let valid = match value.phase.as_str() {
        "allocated" => {
            value.actual_bytes == 0
                && value.reserved_bytes == value.allocation.reserved_max_bytes
                && value.sha256.is_none()
        }
        "verified" => digest && value.reserved_bytes == value.actual_bytes,
        "published" => digest && value.actual_bytes > 0 && value.reserved_bytes == 0,
        "removed" => digest && value.actual_bytes == 0 && value.reserved_bytes == 0,
        _ => false,
    };
    ensure!(valid, "Owned artifact phase accounting is inconsistent");
    Ok(())
}

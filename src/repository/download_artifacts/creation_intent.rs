use super::*;
use crate::download::artifact::OwnedArtifactIntent;

impl Repository {
    pub(crate) fn acknowledge_download_artifact_recovery(
        &self,
        actual: &OwnedArtifactAllocation,
        proof: &crate::download::task_model::DownloadArtifactProof,
        check: &dyn Fn() -> Result<()>,
    ) -> Result<()> {
        actual.validate()?;
        proof.validate()?;
        let mut connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("Repository lock poisoned"))?;
        schema::validate_current(&connection)?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        check()?;
        let stored = artifact_at(&tx, &actual.record.nonce)?
            .context("Actual recovery has no durable original allocation")?;
        ensure!(
            stored.allocation == *actual
                && matches!(stored.phase.as_str(), "verified" | "published")
                && stored.actual_bytes == proof.bytes
                && stored.sha256.as_ref() == Some(&proof.sha256)
                && actual.record.identity == proof.identity
                && actual.record.final_basename == proof.basename,
            "Actual recovery differs from its verified original publication representation"
        );
        let document: String = tx.query_row(
            "SELECT document FROM download_tasks WHERE id=?1",
            [&actual.context.task_id],
            |row| row.get(0),
        )?;
        let task: crate::download::task_model::DownloadTask = decode_document(&document)?;
        task.validate()?;
        ensure!(
            task.profile_id == actual.context.profile_id,
            "Actual recovery belongs to another task profile"
        );
        let grant_document: String = tx.query_row(
            "SELECT document FROM download_destination_grants WHERE id=?1",
            [&task.destination.grant_id],
            |row| row.get(0),
        )?;
        let grant: crate::download::task_model::StoredDestinationGrant =
            decode_document(&grant_document)?;
        task.require_current_destination(&grant)?;
        let group = (task.destination.relative_components.len() == 2)
            .then(|| task.destination.relative_components[0].clone());
        let directory = group
            .as_ref()
            .map_or_else(|| grant.path.clone(), |group| grant.path.join(group));
        ensure!(
            actual.selected_root_identity == grant.identity
                && actual.record.destination_epoch == grant.epoch
                && actual.relative_group == group
                && crate::domain::music::normalize_local_path(&actual.working_directory_path)
                    == crate::domain::music::normalize_local_path(&directory)
                && (group.is_some() || actual.working_directory_identity == grant.identity),
            "Actual recovery selected namespace changed"
        );
        let expected = if actual.kind == OwnedArtifactKind::LyricSidecar {
            task.stages.lyrics.artifact.as_ref().or_else(|| {
                task.commit_journal
                    .as_ref()
                    .and_then(|journal| journal.sidecar.as_ref())
            })
        } else {
            task.output_audio.as_ref()
        };
        let candidate_matches = task.commit_journal.as_ref().is_some_and(|journal| {
            actual.kind != OwnedArtifactKind::LyricSidecar
                && journal.candidate.identity == proof.identity
                && journal.candidate.bytes == proof.bytes
                && journal.candidate.sha256 == proof.sha256
                && task.destination.relative_components.last() == Some(&proof.basename)
        });
        ensure!(
            expected == Some(proof) || candidate_matches,
            "Actual recovery representation is outside the current task publication acknowledgement"
        );
        check()?;
        tx.commit()?;
        Ok(())
    }

    pub fn acknowledge_download_artifact_reopen(
        &self,
        actual: &OwnedArtifactAllocation,
        check: &dyn Fn() -> Result<()>,
    ) -> Result<()> {
        actual.validate()?;
        ensure!(
            actual.kind == OwnedArtifactKind::Transfer,
            "Only an original transfer may resume body writes"
        );
        let mut connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("Repository lock poisoned"))?;
        schema::validate_current(&connection)?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        check()?;
        require_current_intent_task(&tx, &OwnedArtifactIntent::from_created(actual))?;
        let stored = artifact_at(&tx, &actual.record.nonce)?
            .context("Reopened partial has no durable creation acknowledgement")?;
        let old = &stored.allocation;
        ensure!(
            matches!(stored.phase.as_str(), "allocated" | "verified")
                && old.kind == actual.kind
                && old.record == actual.record
                && old.working_directory_path == actual.working_directory_path
                && old.working_directory_identity == actual.working_directory_identity
                && old.selected_root_identity == actual.selected_root_identity
                && old.relative_group == actual.relative_group
                && old.reserved_max_bytes == actual.reserved_max_bytes
                && old.context.profile_id == actual.context.profile_id
                && old.context.task_id == actual.context.task_id,
            "Reopen acknowledgement differs from the original durable object or namespace"
        );
        let task_document: String = tx.query_row(
            "SELECT document FROM download_tasks WHERE id=?1",
            [&actual.context.task_id],
            |row| row.get(0),
        )?;
        let task: crate::download::task_model::DownloadTask = decode_document(&task_document)?;
        ensure!(
            task.owned_part.as_ref() == Some(&actual.record)
                && !task.audio_available
                && task.output_audio.is_none()
                && task.commit_journal.is_none(),
            "Reopen is outside the current task's unpublished actual ownership"
        );
        check()?;
        tx.commit()?;
        Ok(())
    }

    pub fn record_download_allocation_intent(
        &self,
        intent: &OwnedArtifactIntent,
        check: &dyn Fn() -> Result<()>,
    ) -> Result<()> {
        intent.validate()?;
        let document = encode_document(intent)?;
        ensure!(
            document.len() <= 65536,
            "Download allocation intent exceeds its bound"
        );
        let mut connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("Repository lock poisoned"))?;
        schema::validate_current(&connection)?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        check()?;
        require_current_intent_task(&tx, intent)?;
        ensure!(
            !tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM download_owned_artifacts WHERE nonce=?1)",
                [&intent.nonce],
                |row| row.get::<_, bool>(0)
            )?,
            "An acknowledged native object cannot be allocated again"
        );
        if let Some(existing) = intent_at(&tx, &intent.nonce)? {
            ensure!(
                existing == *intent,
                "Download allocation intent nonce belongs to another operation"
            );
            check()?;
            tx.commit()?;
            return Ok(());
        }
        let pending_role: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM download_allocation_intents WHERE task_id=?1 AND json_extract(document,'$.kind')=?2)",
            params![intent.context.task_id, serde_json::to_value(intent.kind)?.as_str()
                .context("Artifact role is invalid")?], |row| row.get(0))?;
        ensure!(
            !pending_role,
            "The task already has an unresolved native allocation intent for this role"
        );
        tx.execute("INSERT INTO download_allocation_intents(nonce,task_id,operation_epoch,reserved_bytes,document,byte_size) VALUES(?1,?2,?3,?4,?5,?6)",
            params![intent.nonce, intent.context.task_id, checked_revision(intent.context.operation_epoch)?,
                i64::try_from(intent.reserved_max_bytes)?, document, i64::try_from(document.len())?])?;
        check_artifact_storage(&tx)?;
        check()?;
        tx.commit()?;
        Ok(())
    }

    /// These are pending facts. They never supply a file or deletion grant.
    pub fn download_allocation_intents(
        &self,
        task_id: Option<&str>,
    ) -> Result<Vec<OwnedArtifactIntent>> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("Repository lock poisoned"))?;
        schema::validate_current(&connection)?;
        let mut statement = connection.prepare(
            "SELECT nonce FROM download_allocation_intents WHERE (?1 IS NULL OR task_id=?1) ORDER BY nonce")?;
        let rows = statement.query_map([task_id], |row| row.get::<_, String>(0))?;
        let mut intents = Vec::new();
        for row in rows {
            ensure!(
                intents.len() < 400,
                "Download allocation intent count exceeds its bound"
            );
            intents.push(intent_at(&connection, &row?)?.context("Allocation intent disappeared")?);
        }
        Ok(intents)
    }
}

pub(in crate::repository) fn intent_at(
    connection: &Connection,
    nonce: &str,
) -> Result<Option<OwnedArtifactIntent>> {
    let row: Option<(String, i64, i64, String, i64)> = connection.query_row(
        "SELECT task_id,operation_epoch,reserved_bytes,document,byte_size FROM download_allocation_intents WHERE nonce=?1",
        [nonce], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)))
        .optional()?;
    let Some((task_id, epoch, reserved, document, bytes)) = row else {
        return Ok(None);
    };
    ensure!(
        document.len() <= 65536 && bytes == i64::try_from(document.len())?,
        "Corrupt download allocation intent document"
    );
    let intent: OwnedArtifactIntent = decode_document(&document)?;
    intent.validate()?;
    ensure!(
        intent.nonce == nonce
            && intent.context.task_id == task_id
            && checked_revision(intent.context.operation_epoch)? == epoch
            && i64::try_from(intent.reserved_max_bytes)? == reserved,
        "Download allocation intent row differs from its document"
    );
    Ok(Some(intent))
}

fn require_current_intent_task(
    connection: &Connection,
    intent: &OwnedArtifactIntent,
) -> Result<()> {
    let document: String = connection.query_row(
        "SELECT document FROM download_tasks WHERE id=?1",
        [&intent.context.task_id],
        |row| row.get(0),
    )?;
    let task: crate::download::task_model::DownloadTask = decode_document(&document)?;
    task.validate()?;
    ensure!(
        task.profile_id == intent.context.profile_id
            && task.operation_epoch == intent.context.operation_epoch,
        "Download allocation intent belongs to another task operation"
    );
    let grant_document: String = connection.query_row(
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
        task.destination.root_identity == intent.selected_root_identity
            && task.destination.grant_epoch == intent.destination_epoch
            && group == intent.relative_group
            && crate::domain::music::normalize_local_path(&directory)
                == crate::domain::music::normalize_local_path(&intent.working_directory_path)
            && (group.is_some() || intent.working_directory_identity == grant.identity),
        "Download allocation intent differs from its actual selected namespace"
    );
    let audio_basename = task
        .destination
        .relative_components
        .last()
        .context("Download task leaf is unavailable")?;
    if intent.kind == OwnedArtifactKind::LyricSidecar {
        ensure!(
            task.settings.download_lyrics,
            "Lyric allocation intent was not requested"
        );
        let stem = std::path::Path::new(audio_basename)
            .file_stem()
            .and_then(|stem| stem.to_str())
            .context("Download task leaf has no valid stem")?;
        ensure!(
            intent.final_basename == format!("{stem}.lrc"),
            "Lyric allocation intent is outside its task's selected stem"
        );
    } else {
        ensure!(
            &intent.final_basename == audio_basename,
            "Allocation intent is outside its task's selected leaf"
        );
    }
    let bookmark = task
        .url_bookmark
        .as_ref()
        .context("Download task has no approved execution context")?;
    ensure!(
        bookmark.context == intent.context,
        "Download allocation intent source context differs from its approved URL"
    );
    Ok(())
}

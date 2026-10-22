use super::*;
use crate::download::{MAX_DOWNLOAD_TASKS, task_model::*};
use rusqlite::params;

impl Repository {
    #[expect(
        clippy::too_many_arguments,
        reason = "The current retry CAS and selected producer role independently bind the durable pre-native intent"
    )]
    pub(crate) fn record_download_retry_allocation_intent(
        &self,
        id: &str,
        operation: &str,
        running_revision: u64,
        running_epoch: u64,
        role: crate::download::postprocess_retry::RetryArtifactRole,
        pending: &crate::download::artifact::OwnedArtifactIntent,
        check: &dyn Fn() -> Result<()>,
    ) -> Result<()> {
        use crate::download::{artifact::OwnedArtifactKind, postprocess_retry::RetryArtifactRole};
        pending.validate()?;
        self.download_retry_facts(id, operation, true, check, &mut |task, tx| {
            ensure!(task.revision == running_revision && task.operation_epoch == running_epoch
                && task.state == crate::download::DownloadState::PostProcessing,
                "Download retry native allocation admission changed");
            let retry = task.commit_journal.as_ref().and_then(|journal| journal.retry.as_ref())
                .context("Download retry intent is unavailable")?;
            let reservation = retry.reservation(role)?;
            let expected_kind = match role {
                RetryArtifactRole::Candidate => OwnedArtifactKind::TagCopy,
                RetryArtifactRole::Lyrics => OwnedArtifactKind::LyricSidecar,
                RetryArtifactRole::Backup | RetryArtifactRole::Journal => OwnedArtifactKind::RollbackOriginal,
            };
            let provenance = task.original_representation.as_ref().context("Download retry source provenance is unavailable")?;
            ensure!(!reservation.removed && reservation.allocation.is_none()
                && pending.kind == expected_kind && pending.nonce == reservation.nonce
                && pending.reserved_max_bytes == reservation.maximum_bytes
                && pending.context.task_id == task.id && pending.context.profile_id == task.profile_id
                && pending.context.operation_epoch == retry.operation_epoch
                && pending.context.source_id == provenance.source_id
                && pending.context.source_content_sha256 == provenance.source_content_sha256
                && pending.context.policy_epoch == provenance.policy_epoch,
                "Download retry pending native intent differs from its reserved producer");
            let group = (task.destination.relative_components.len() == 2)
                .then(|| task.destination.relative_components[0].clone());
            let grant = destination_at(tx, &task.destination.grant_id)?
                .context("Download retry destination is unavailable")?;
            let directory = group.as_ref().map_or_else(|| grant.path.clone(), |group| grant.path.join(group));
            let final_basename = if role == RetryArtifactRole::Lyrics {
                let stem = std::path::Path::new(&retry.original_audio.basename).file_stem()
                    .and_then(|stem| stem.to_str()).context("Download retry leaf has no valid stem")?;
                format!("{stem}.lrc")
            } else { retry.original_audio.basename.clone() };
            ensure!(pending.selected_root_identity == task.destination.root_identity
                && pending.destination_epoch == task.destination.grant_epoch && pending.relative_group == group
                && pending.final_basename == final_basename
                && crate::domain::music::normalize_local_path(&pending.working_directory_path)
                    == crate::domain::music::normalize_local_path(&directory)
                && (group.is_some() || pending.working_directory_identity == grant.identity),
                "Download retry pending native intent selected namespace changed");
            if let Some(previous) = super::download_artifacts::creation_intent::intent_at(tx, &pending.nonce)? {
                ensure!(previous == *pending, "Retry pending native nonce belongs to another allocation");
                return Ok(());
            }
            ensure!(super::download_artifacts::artifact_at(tx, &pending.nonce)?.is_none(),
                "Acknowledged retry object cannot be allocated again");
            let document = encode_document(pending)?;
            ensure!(document.len() <= 65536, "Retry pending native intent exceeds its document bound");
            tx.execute("INSERT INTO download_allocation_intents(nonce,task_id,operation_epoch,reserved_bytes,document,byte_size) VALUES(?1,?2,?3,?4,?5,?6)",
                params![pending.nonce, id, checked_revision(pending.context.operation_epoch)?,
                    i64::try_from(pending.reserved_max_bytes)?, document, i64::try_from(document.len())?])?;
            Ok(())
        })?;
        Ok(())
    }
    pub(crate) fn prepare_download_retry_retirement(
        &self,
        id: &str,
        operation: &str,
        record: &crate::download::OwnedPartRecord,
        attestation: &crate::download::postprocess_retry::RetryDeletionAttestation,
    ) -> Result<()> {
        self.download_retry_facts(id, operation, false, &|| Ok(()), &mut |task, _| {
            let intent = task
                .commit_journal
                .as_mut()
                .and_then(|j| j.retry.as_mut())
                .context("Download retry intent is unavailable")?;
            let reservation = intent
                .artifacts
                .iter_mut()
                .find(|a| a.allocation.as_ref().is_some_and(|a| a.record == *record))
                .context("Download retirement producer allocation is unavailable")?;
            if let Some(previous) = &reservation.deletion {
                ensure!(
                    previous == attestation,
                    "Download retry native deletion volume changed"
                );
            } else {
                reservation.deletion = Some(attestation.clone());
            }
            Ok(())
        })?;
        Ok(())
    }
    #[expect(
        clippy::too_many_arguments,
        reason = "The task CAS, operation scope, physical proof, stage state and final cancellation guard are independently validated"
    )]
    pub(crate) fn commit_download_retry_lyrics(
        &self,
        id: &str,
        operation: &str,
        expected_task_revision: u64,
        expected_epoch: u64,
        proof: &DownloadArtifactProof,
        stages: &DownloadStageRecords,
        check: &dyn Fn() -> Result<()>,
    ) -> Result<DownloadTask> {
        proof.validate()?;
        let (task,_)=self.write_checked(None,|tx| {
            let mut task=task_at(tx,id)?.context("Download retry task is unavailable")?;
            ensure!(task.revision==expected_task_revision&&task.operation_epoch==expected_epoch&&task.state==crate::download::DownloadState::PostProcessing,"Download lyric retry current intent changed");
            require_task_destination(tx,&task)?;
            let intent=task.commit_journal.as_ref().and_then(|j|j.retry.as_ref()).context("Download retry intent is unavailable")?;
            ensure!(intent.operation_id==operation,"Download lyric retry operation changed");
            let artifact=intent.reservation(crate::download::postprocess_retry::RetryArtifactRole::Lyrics)?.allocation.as_ref().context("Download lyric producer allocation is unavailable")?;
            ensure!(artifact.record.identity==proof.identity&&artifact.record.final_basename==proof.basename,"Download lyric retry physical output changed");
            let changed=tx.execute("UPDATE download_owned_artifacts SET phase='published',reserved_bytes=0 WHERE nonce=?1 AND task_id=?2 AND operation_epoch=?3 AND phase='verified' AND actual_bytes=?4 AND sha256=?5",params![artifact.record.nonce,id,checked_revision(intent.operation_epoch)?,i64::try_from(proof.bytes)?,proof.sha256])?;
            ensure!(changed==1,"Download lyric retry verified body changed");
            task.stages=stages.clone();task.stages.lyrics.state=DownloadStageState::Succeeded;task.stages.lyrics.error=None;task.stages.lyrics.artifact=Some(proof.clone());
            task.commit_journal.as_mut().unwrap().sidecar=Some(proof.clone());
            task.revision=task.revision.checked_add(1).context("Download retry revision overflow")?;task.updated_at_ms=task.updated_at_ms.max(SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis() as u64);
            task.validate()?;write_task(tx,&task)?;check_storage(tx)?;Ok((task,ChangeKind::DownloadTasksChanged,None,serde_json::json!({"id":id,"lyric_postprocessing":operation})))
        },check)?;
        Ok(task)
    }
    #[expect(
        clippy::too_many_arguments,
        reason = "Producer allocation identity, role, operation CAS and cancellation guard must remain distinct"
    )]
    pub(crate) fn record_download_retry_artifact(
        &self,
        id: &str,
        operation: &str,
        running_revision: u64,
        running_epoch: u64,
        role: crate::download::postprocess_retry::RetryArtifactRole,
        allocation: &crate::download::artifact::OwnedArtifactAllocation,
        check: &dyn Fn() -> Result<()>,
    ) -> Result<()> {
        allocation.validate()?;
        self.download_retry_facts(id,operation,true,check,&mut |task,tx| {
            ensure!(task.revision==running_revision&&task.operation_epoch==running_epoch&&task.state==crate::download::DownloadState::PostProcessing,"Download retry producer admission changed");
            let provenance=task.original_representation.as_ref().context("Download retry source provenance is unavailable")?;
            ensure!(allocation.context.source_id==provenance.source_id&&allocation.context.source_content_sha256==provenance.source_content_sha256&&allocation.context.policy_epoch==provenance.policy_epoch,"Download retry provenance changed");
            let intent=task.commit_journal.as_mut().and_then(|j|j.retry.as_mut()).context("Download retry intent is unavailable")?;
            let reservation=intent.artifacts.iter_mut().find(|a|a.role==role).context("Download retry reservation is unavailable")?;
            ensure!(!reservation.removed&&reservation.nonce==allocation.record.nonce&&reservation.maximum_bytes==allocation.reserved_max_bytes&&allocation.context.task_id==task.id&&allocation.context.profile_id==task.profile_id&&allocation.context.operation_epoch==intent.operation_epoch,"Download retry artifact scope changed");
            ensure!(allocation.selected_root_identity==task.destination.root_identity&&allocation.record.destination_epoch==task.destination.grant_epoch,"Download retry destination changed");
            let group=if task.destination.relative_components.len()==2 {Some(task.destination.relative_components[0].clone())} else {None};
            ensure!(allocation.relative_group==group,"Download retry group changed");
            if let Some(previous)=&reservation.allocation {ensure!(previous==allocation,"Download retry producer allocation changed");return Ok(());}
            let pending=super::download_artifacts::creation_intent::intent_at(tx,&allocation.record.nonce)?
                .context("Retry producer has no durable pre-native allocation intent")?;
            pending.require_created(allocation)?;
            let document=encode_document(allocation)?;
            ensure!(document.len()<=65536,"Owned retry producer document exceeds 65536 bytes");
            tx.execute("INSERT INTO download_owned_artifacts(nonce,task_id,operation_epoch,phase,actual_bytes,reserved_bytes,sha256,document,byte_size) VALUES(?1,?2,?3,'allocated',0,?4,NULL,?5,?6)",params![allocation.record.nonce,task.id,checked_revision(allocation.context.operation_epoch)?,i64::try_from(allocation.reserved_max_bytes)?,document,i64::try_from(document.len())?])?;
            let consumed=tx.execute("DELETE FROM download_allocation_intents WHERE nonce=?1 AND task_id=?2 AND operation_epoch=?3",
                params![allocation.record.nonce,task.id,checked_revision(allocation.context.operation_epoch)?])?;
            ensure!(consumed==1,"Retry native allocation intent consumption changed");
            reservation.allocation=Some(allocation.clone());Ok(())
        })?;
        Ok(())
    }
    pub(crate) fn acknowledge_download_retry_retirement(
        &self,
        id: &str,
        operation: &str,
        witness: &crate::download::RetryArtifactRetirement,
    ) -> Result<()> {
        let record = witness.record();
        self.download_retry_facts(id,operation,false,&||Ok(()),&mut |task,tx| {
            let intent=task.commit_journal.as_mut().and_then(|j|j.retry.as_mut()).context("Download retry intent is unavailable")?;
            let reservation=intent.artifacts.iter_mut().find(|a|a.allocation.as_ref().is_some_and(|a|a.record==*record)).context("Download retry retirement has no matching producer allocation")?;
            ensure!(reservation.role!=crate::download::postprocess_retry::RetryArtifactRole::Lyrics||task.stages.lyrics.artifact.as_ref().is_none_or(|p|p.identity!=record.identity),"Published lyric output cannot be retired as tag work");
            if reservation.removed {return Ok(());}
            let changed=tx.execute("UPDATE download_owned_artifacts SET phase='removed',actual_bytes=0,reserved_bytes=0,sha256=coalesce(sha256,'e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855') WHERE nonce=?1 AND task_id=?2 AND operation_epoch=?3 AND phase IN ('allocated','verified')",params![record.nonce,id,checked_revision(intent.operation_epoch)?])?;
            ensure!(changed==1,"Download retry retirement accounting changed");reservation.removed=true;Ok(())
        })?;
        Ok(())
    }
    #[expect(
        clippy::too_many_arguments,
        reason = "The task CAS, operation scope, same-object output proof, stage state and final cancellation guard are independent commit conditions"
    )]
    pub(crate) fn commit_download_retry_tags(
        &self,
        id: &str,
        operation: &str,
        expected_task_revision: u64,
        expected_epoch: u64,
        proof: &DownloadArtifactProof,
        stages: &DownloadStageRecords,
        check: &dyn Fn() -> Result<()>,
    ) -> Result<DownloadTask> {
        proof.validate()?;
        let (task,_)=self.write_checked(None,|tx| {
            let mut task=task_at(tx,id)?.context("Download retry task is unavailable")?;
            ensure!(task.revision==expected_task_revision&&task.operation_epoch==expected_epoch&&task.state==crate::download::DownloadState::PostProcessing,"Download retry installed intent changed");
            let intent=task.commit_journal.as_mut().and_then(|j|j.retry.as_mut()).context("Download retry intent is unavailable")?;
            ensure!(intent.operation_id==operation&&!intent.repository_committed&&proof.identity==intent.original_audio.identity&&proof.basename==intent.original_audio.basename&&task.output_audio.as_ref()==Some(&intent.original_audio),"Download retry installed output changed");
            require_task_destination(tx,&task)?;
            let old=task.output_audio.as_ref().unwrap();
            let changed=tx.execute("UPDATE download_owned_artifacts SET actual_bytes=?3,sha256=?4 WHERE task_id=?1 AND phase='published' AND json_extract(document,'$.record.identity')=?2 AND actual_bytes=?5 AND sha256=?6",params![id,serde_json::to_string(&proof.identity)?,i64::try_from(proof.bytes)?,proof.sha256,i64::try_from(old.bytes)?,old.sha256])?;
            ensure!(changed==1,"Published download accounting no longer matches its original physical output");
            let intent=task.commit_journal.as_mut().unwrap().retry.as_mut().unwrap();
            intent.installed_audio=Some(proof.clone());intent.repository_committed=true;
            task.output_audio=Some(proof.clone());task.stages=stages.clone();task.revision=task.revision.checked_add(1).context("Download retry revision overflow")?;
            task.updated_at_ms=task.updated_at_ms.max(SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis() as u64);
            task.validate()?;write_task(tx,&task)?;check_storage(tx)?;
            Ok((task,ChangeKind::DownloadTasksChanged,None,serde_json::json!({"id":id,"postprocessing":operation})))
        },check)?;
        Ok(task)
    }
    pub(crate) fn download_retry_facts(
        &self,
        id: &str,
        operation_id: &str,
        require_destination: bool,
        check: &dyn Fn() -> Result<()>,
        mutate: &mut dyn FnMut(&mut DownloadTask, &rusqlite::Transaction<'_>) -> Result<()>,
    ) -> Result<DownloadTask> {
        check()?;
        let mut connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("Repository lock poisoned"))?;
        schema::validate_current(&connection)?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut task = task_at(&tx, id)?.context("Download retry task is unavailable")?;
        let intent = task
            .commit_journal
            .as_ref()
            .and_then(|j| j.retry.as_ref())
            .context("Download retry intent is unavailable")?;
        ensure!(
            intent.operation_id == operation_id,
            "Download retry operation changed"
        );
        if require_destination {
            require_task_destination(&tx, &task)?;
        }
        mutate(&mut task, &tx)?;
        task.validate()?;
        write_task(&tx, &task)?;
        check_storage(&tx)?;
        check()?;
        tx.commit()?;
        Ok(task)
    }
    pub fn download_settings(&self) -> Result<DownloadSettings> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("Repository lock poisoned"))?;
        schema::validate_current(&connection)?;
        let document: Option<String> = connection
            .query_row(
                "SELECT document FROM download_settings WHERE singleton=1",
                [],
                |row| row.get(0),
            )
            .optional()?;
        let settings = document
            .map(|document| decode_document::<DownloadSettings>(&document))
            .transpose()?
            .unwrap_or_default();
        settings.validate()?;
        Ok(settings)
    }
    pub fn save_download_settings(
        &self,
        settings: &DownloadSettings,
        expected_revision: Option<u64>,
    ) -> Result<RepositoryChange> {
        settings.validate()?;
        let (_, change) = self.write_checked(expected_revision, |tx| {
            if let Some(id) = &settings.destination_grant_id { require_destination(tx, id, None)?; }
            tx.execute("INSERT INTO download_settings VALUES(1,?1) ON CONFLICT(singleton) DO UPDATE SET document=excluded.document", [encode_document(settings)?])?;
            Ok(((), ChangeKind::DownloadSettingsChanged, None, serde_json::json!({"enabled":settings.enabled})))
        }, &|| Ok(()))?;
        Ok(change)
    }
    pub fn download_destinations(&self) -> Result<Vec<StoredDestinationGrant>> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("Repository lock poisoned"))?;
        schema::validate_current(&connection)?;
        let mut statement =
            connection.prepare("SELECT document FROM download_destination_grants ORDER BY id")?;
        let mut values = Vec::new();
        for row in statement.query_map([], |row| row.get::<_, String>(0))? {
            let value: StoredDestinationGrant = decode_document(&row?)?;
            value.validate()?;
            ensure!(values.len() < 16, "Download destination budget exceeded");
            values.push(value);
        }
        Ok(values)
    }
    pub fn put_download_destination(
        &self,
        grant: &StoredDestinationGrant,
        expected_grant_revision: Option<u64>,
        expected_revision: Option<u64>,
        check: &dyn Fn() -> Result<()>,
    ) -> Result<RepositoryChange> {
        grant.validate()?;
        let (_, change) = self.write_checked(expected_revision, |tx| {
            let previous = destination_at(tx, &grant.id)?;
            match (&previous, expected_grant_revision) {
                (None, None) => ensure!(grant.epoch == 1 && grant.revision == 1, "New destination revision must start at one"),
                (Some(old), Some(expected)) => {
                    ensure!(old.revision == expected && old.profile_id == grant.profile_id,
                        "Download destination revision conflict");
                    ensure!(grant.revision == old.revision.checked_add(1).context("Destination revision overflow")?, "Invalid next destination revision");
                    let changed_authority = old.identity != grant.identity || old.path != grant.path || old.revoked != grant.revoked;
                    ensure!(grant.epoch == if changed_authority { old.epoch.checked_add(1).context("Destination epoch overflow")? } else { old.epoch }, "Invalid destination epoch");
                }
                _ => anyhow::bail!("Download destination revision conflict"),
            }
            tx.execute("INSERT INTO download_destination_grants VALUES(?1,?2,?3,?4,?5,?6) ON CONFLICT(id) DO UPDATE SET epoch=excluded.epoch,revision=excluded.revision,revoked=excluded.revoked,document=excluded.document",
                params![grant.id,grant.profile_id,checked_revision(grant.epoch)?,checked_revision(grant.revision)?,grant.revoked,encode_document(grant)?])?;
            let count: i64 = tx.query_row("SELECT count(*) FROM download_destination_grants", [], |row| row.get(0))?;
            ensure!(count <= 16, "Download destination budget exceeded");
            Ok(((), ChangeKind::DownloadDestinationChanged, None, serde_json::json!({"id":grant.id,"epoch":grant.epoch,"revoked":grant.revoked})))
        }, check)?;
        Ok(change)
    }
    pub fn download_tasks(&self) -> Result<Vec<DownloadTask>> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("Repository lock poisoned"))?;
        schema::validate_current(&connection)?;
        tasks_at(&connection)
    }
    pub fn download_task(&self, id: &str) -> Result<Option<DownloadTask>> {
        ensure!(
            !id.is_empty() && id.len() <= 256,
            "Invalid download task ID"
        );
        let connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("Repository lock poisoned"))?;
        schema::validate_current(&connection)?;
        task_at(&connection, id)
    }
    pub fn insert_download_tasks(
        &self,
        tasks: &[DownloadTask],
        expected_revision: Option<u64>,
        check: &dyn Fn() -> Result<()>,
    ) -> Result<RepositoryChange> {
        self.insert_download_tasks_at(tasks, false, expected_revision, check)
    }
    pub fn insert_download_tasks_at(
        &self,
        tasks: &[DownloadTask],
        top: bool,
        expected_revision: Option<u64>,
        check: &dyn Fn() -> Result<()>,
    ) -> Result<RepositoryChange> {
        validate_profile_downloads(tasks)?;
        ensure!(!tasks.is_empty(), "Download task batch is empty");
        let (_, change) = self.write_checked(expected_revision, |tx| {
            let mut combined = tasks_at(tx)?;
            for task in tasks {
                check()?;
                ensure!(task.revision == 1 && task.operation_epoch == 1,
                    "New download task revision must start at one");
                ensure!(task.state == crate::download::DownloadState::Waiting || task.state == crate::download::DownloadState::Paused,
                    "New download task must await explicit execution");
                ensure!(task.owned_part.is_none() && task.output_audio.is_none() && task.original_transfer.is_none()
                    && task.url_bookmark.is_none() && task.commit_journal.is_none(),
                    "Task insertion cannot restore execution or filesystem ownership");
                require_task_destination(tx, task)?;
                ensure!(combined.first().is_none_or(|old| old.profile_id == task.profile_id), "Download task belongs to another profile");
                ensure!(!combined.iter().any(|old| old.id == task.id || same_download_identity(old, task)), "Duplicate download task");
                combined.push(task.clone());
                write_task(tx, task)?;
            }
            validate_profile_downloads(&combined)?;
            let existing = combined.len() - tasks.len();
            if top { combined.rotate_right(tasks.len()); }
            for (position, task) in combined.iter_mut().enumerate() {
                if ((top && position >= tasks.len()) || (!top && position < existing))
                    && task.order != position as u32
                {
                    task.revision = task.revision.checked_add(1).context("Task order revision overflow")?;
                }
                task.order = position as u32;
                write_task(tx, task)?;
            }
            check_storage(tx)?;
            Ok(((), ChangeKind::DownloadTasksChanged, None, serde_json::json!({"inserted":tasks.iter().map(|task|&task.id).collect::<Vec<_>>()})))
        }, check)?;
        Ok(change)
    }
    pub fn update_download_task(
        &self,
        task: &DownloadTask,
        expected_task_revision: u64,
        expected_operation_epoch: u64,
        expected_revision: Option<u64>,
        check: &dyn Fn() -> Result<()>,
    ) -> Result<RepositoryChange> {
        self.update_download_task_and_artifacts(
            task,
            expected_task_revision,
            expected_operation_epoch,
            expected_revision,
            &[],
            check,
        )
    }

    /// Commits output facts and their verified artifact accounting in the same
    /// transaction. The caller retains native ownership across both checks.
    pub fn commit_download_publication(
        &self,
        task: &DownloadTask,
        expected_task_revision: u64,
        expected_operation_epoch: u64,
        expected_revision: Option<u64>,
        artifacts: &[(String, u64, u64, String)],
        check: &dyn Fn() -> Result<()>,
    ) -> Result<RepositoryChange> {
        ensure!(
            task.audio_available
                && task.output_audio.is_some()
                && !artifacts.is_empty()
                && artifacts.len() <= 2,
            "Download publication requires its audio and bounded verified artifacts"
        );
        self.update_download_task_and_artifacts(
            task,
            expected_task_revision,
            expected_operation_epoch,
            expected_revision,
            artifacts,
            check,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn update_download_task_and_artifacts(
        &self,
        task: &DownloadTask,
        expected_task_revision: u64,
        expected_operation_epoch: u64,
        expected_revision: Option<u64>,
        artifacts: &[(String, u64, u64, String)],
        check: &dyn Fn() -> Result<()>,
    ) -> Result<RepositoryChange> {
        task.validate()?;
        let (_, change) = self.write_checked(expected_revision, |tx| {
            let previous = task_at(tx, &task.id)?.context("Download task is unavailable")?;
            ensure!(previous.revision == expected_task_revision && previous.operation_epoch == expected_operation_epoch,
                "Download task revision or operation epoch conflict");
            ensure!(task.revision == previous.revision.checked_add(1).context("Task revision overflow")?
                && (task.operation_epoch == previous.operation_epoch
                    || task.operation_epoch == previous.operation_epoch.checked_add(1).context("Task epoch overflow")?),
                "Invalid next download task revision");
            ensure!(task.profile_id == previous.profile_id && task.original_record == previous.original_record
                && task.requested_quality == previous.requested_quality && task.created_at_ms == previous.created_at_ms,
                "Download task original identity is immutable");
            ensure!(task.updated_at_ms >= previous.updated_at_ms, "Download task timestamp moved backwards");
            if task.settings != previous.settings || task.resolved_record != previous.resolved_record {
                ensure!(matches!(previous.state, crate::download::DownloadState::Waiting | crate::download::DownloadState::Paused
                    | crate::download::DownloadState::Resolving | crate::download::DownloadState::AwaitingUrlApproval)
                    && task.operation_epoch > previous.operation_epoch && task.owned_part.is_none()
                    && task.original_transfer.is_none() && task.url_bookmark.is_none() && task.commit_journal.is_none(),
                    "Execution plan changes require a fresh operation without URL or file authority");
            }
            if task.destination != previous.destination {
                ensure!(matches!(previous.state, crate::download::DownloadState::Paused | crate::download::DownloadState::Resolving)
                    && task.operation_epoch > previous.operation_epoch && task.owned_part.is_none()
                    && task.original_transfer.is_none() && task.commit_journal.is_none(),
                    "Destination replanning requires a paused task without owned transfer work");
                require_task_destination(tx, task)?;
            }
            if task.actual_quality != previous.actual_quality {
                ensure!(matches!(previous.state, crate::download::DownloadState::Waiting | crate::download::DownloadState::Paused
                    | crate::download::DownloadState::Resolving | crate::download::DownloadState::AwaitingUrlApproval)
                    && task.operation_epoch > previous.operation_epoch && task.owned_part.is_none()
                    && task.original_transfer.is_none() && task.url_bookmark.is_none(),
                    "Quality replanning requires a fresh operation without prior URL or transfer authority");
            }
            if !matches!(task.state, crate::download::DownloadState::Paused | crate::download::DownloadState::Cancelled | crate::download::DownloadState::Failed) {
                require_task_destination(tx, task)?;
            }
            if !artifacts.is_empty() {
                let unresolved: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM download_allocation_intents WHERE task_id=?1)",
                    [&task.id], |row| row.get(0))?;
                ensure!(!unresolved, "Download publication has an unresolved native allocation intent");
            }
            write_task(tx, task)?;
            let mut published_audio = false;
            let mut published_sidecar = false;
            let mut seen_publications = std::collections::HashSet::new();
            for (nonce, epoch, bytes, sha256) in artifacts {
                ensure!(seen_publications.insert(nonce), "Duplicate artifact publication ACK");
                match super::download_artifacts::publish_download_artifact_in_transaction(tx, nonce, *epoch, *bytes, sha256, &task.id)? {
                    crate::download::artifact::OwnedArtifactKind::Transfer | crate::download::artifact::OwnedArtifactKind::TagCopy => published_audio = true,
                    crate::download::artifact::OwnedArtifactKind::LyricSidecar => published_sidecar = true,
                    crate::download::artifact::OwnedArtifactKind::RollbackOriginal => anyhow::bail!("A rollback artifact cannot acknowledge a published audio output"),
                }
            }
            if !artifacts.is_empty() {
                ensure!(published_audio && (task.stages.lyrics.artifact.is_none() || published_sidecar),
                    "Download publication ACK does not cover every output artifact");
            }
            validate_profile_downloads(&tasks_at(tx)?)?;
            check_storage(tx)?;
            Ok(((), ChangeKind::DownloadTasksChanged, None, serde_json::json!({"id":task.id,"task_revision":task.revision,"operation_epoch":task.operation_epoch,"state":task.state})))
        }, check)?;
        Ok(change)
    }
    pub fn remove_download_task(
        &self,
        id: &str,
        expected_task_revision: u64,
        expected_operation_epoch: u64,
        expected_revision: Option<u64>,
    ) -> Result<RepositoryChange> {
        let (_, change) = self.write_checked(expected_revision, |tx| {
            let task = task_at(tx, id)?.context("Download task is unavailable")?;
            ensure!(task.revision == expected_task_revision && task.operation_epoch == expected_operation_epoch, "Download task revision conflict");
            ensure!(matches!(task.state, crate::download::DownloadState::Paused | crate::download::DownloadState::Completed
                | crate::download::DownloadState::Failed | crate::download::DownloadState::Cancelled), "Download task still owns running work");
            ensure!(task.commit_journal.is_none(), "Download commit recovery must finish before removing its task");
            ensure!(task.owned_part.is_none(), "Owned download partial cleanup must finish before removing its task");
            let unresolved: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM download_owned_artifacts WHERE task_id=?1 AND phase NOT IN ('published','removed')) OR EXISTS(SELECT 1 FROM download_allocation_intents WHERE task_id=?1)", [id], |row| row.get(0))?;
            ensure!(!unresolved, "Owned download artifact cleanup must finish before removing its task");
            // Published output files belong to the user; only terminal accounting
            // metadata is removed in the same transaction as its task row.
            tx.execute("DELETE FROM download_owned_artifacts WHERE task_id=?1 AND phase IN ('published','removed')", [id])?;
            tx.execute("DELETE FROM download_tasks WHERE id=?1", [id])?;
            Ok(((), ChangeKind::DownloadTasksChanged, None, serde_json::json!({"removed":id})))
        }, &|| Ok(()))?;
        Ok(change)
    }
    pub(super) fn recover_download_tasks(&self) -> Result<()> {
        let tasks = self.download_tasks()?;
        let interrupted = tasks
            .into_iter()
            .filter(|task| {
                matches!(
                    task.state,
                    crate::download::DownloadState::Waiting
                        | crate::download::DownloadState::Resolving
                        | crate::download::DownloadState::AwaitingUrlApproval
                        | crate::download::DownloadState::Transferring
                        | crate::download::DownloadState::Retrying
                        | crate::download::DownloadState::Pausing
                        | crate::download::DownloadState::Verifying
                        | crate::download::DownloadState::CommittingAudio
                        | crate::download::DownloadState::PostProcessing
                        | crate::download::DownloadState::Cancelling
                )
            })
            .collect::<Vec<_>>();
        if interrupted.is_empty() {
            return Ok(());
        }
        let timestamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis() as u64;
        self.write_checked(
            None,
            |tx| {
                for mut task in interrupted {
                    task.after_restart(timestamp)?;
                    write_task(tx, &task)?;
                }
                check_storage(tx)?;
                Ok((
                    (),
                    ChangeKind::DownloadTasksInterrupted,
                    None,
                    serde_json::json!({"reason":"restart"}),
                ))
            },
            &|| Ok(()),
        )?;
        Ok(())
    }
}
fn same_download_identity(left: &DownloadTask, right: &DownloadTask) -> bool {
    left.deduplication_key() == right.deduplication_key()
}
fn destination_at(connection: &Connection, id: &str) -> Result<Option<StoredDestinationGrant>> {
    let document: Option<String> = connection
        .query_row(
            "SELECT document FROM download_destination_grants WHERE id=?1",
            [id],
            |row| row.get(0),
        )
        .optional()?;
    document
        .map(|document| {
            let value: StoredDestinationGrant = decode_document(&document)?;
            value.validate()?;
            Ok(value)
        })
        .transpose()
}
fn require_destination(
    connection: &Connection,
    id: &str,
    profile: Option<&str>,
) -> Result<StoredDestinationGrant> {
    let grant = destination_at(connection, id)?.context("Download destination is unavailable")?;
    ensure!(
        !grant.revoked && profile.is_none_or(|profile| profile == grant.profile_id),
        "Download destination requires reapproval"
    );
    Ok(grant)
}
fn require_task_destination(connection: &Connection, task: &DownloadTask) -> Result<()> {
    let grant = require_destination(
        connection,
        &task.destination.grant_id,
        Some(&task.profile_id),
    )?;
    ensure!(
        task.destination.grant_epoch == grant.epoch
            && task.destination.root_identity == grant.identity,
        "Download destination epoch or identity conflict"
    );
    Ok(())
}
type DownloadTaskRow = (String, String, String, i64, i64, i64, String, i64, String);
fn task_at(connection: &Connection, id: &str) -> Result<Option<DownloadTask>> {
    let row: Option<DownloadTaskRow> = connection.query_row(
        "SELECT id,profile_id,grant_id,position,task_revision,operation_epoch,state,byte_size,document FROM download_tasks WHERE id=?1", [id],
        |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?,row.get(4)?,row.get(5)?,row.get(6)?,row.get(7)?,row.get(8)?))).optional()?;
    row.map(
        |(id, profile, grant, position, revision, epoch, state, bytes, document)| {
            let task: DownloadTask = decode_document(&document)?;
            task.validate()?;
            ensure!(
                task.id == id
                    && task.profile_id == profile
                    && task.destination.grant_id == grant
                    && i64::from(task.order) == position
                    && checked_revision(task.revision)? == revision
                    && checked_revision(task.operation_epoch)? == epoch
                    && serde_json::to_value(task.state)?.as_str() == Some(state.as_str())
                    && i64::try_from(document.len())? == bytes,
                "Download task SQL identity differs from its document"
            );
            Ok(task)
        },
    )
    .transpose()
}
pub(super) fn tasks_at(connection: &Connection) -> Result<Vec<DownloadTask>> {
    let mut statement = connection.prepare("SELECT id FROM download_tasks ORDER BY position,id")?;
    let mut tasks = Vec::new();
    for row in statement.query_map([], |row| row.get::<_, String>(0))? {
        ensure!(
            tasks.len() < MAX_DOWNLOAD_TASKS,
            "Download task budget exceeded"
        );
        let task = task_at(connection, &row?)?.context("Download task disappeared")?;
        tasks.push(task);
    }
    validate_profile_downloads(&tasks)?;
    ensure!(
        tasks
            .first()
            .is_none_or(|first| tasks.iter().all(|task| task.profile_id == first.profile_id)),
        "Download profile identity is inconsistent"
    );
    Ok(tasks)
}
fn write_task(tx: &Transaction<'_>, task: &DownloadTask) -> Result<()> {
    let document = encode_document(task)?;
    let state = serde_json::to_value(task.state)?
        .as_str()
        .context("Download state is not a string")?
        .to_owned();
    tx.execute("INSERT INTO download_tasks VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9) ON CONFLICT(id) DO UPDATE SET grant_id=excluded.grant_id,position=excluded.position,task_revision=excluded.task_revision,operation_epoch=excluded.operation_epoch,state=excluded.state,document=excluded.document,byte_size=excluded.byte_size",
        params![task.id,task.profile_id,task.destination.grant_id,task.order,checked_revision(task.revision)?,checked_revision(task.operation_epoch)?,state,document,document.len() as i64])?;
    Ok(())
}
fn check_storage(tx: &Transaction<'_>) -> Result<()> {
    let (count, bytes): (i64, i64) = tx.query_row(
        "SELECT count(*),coalesce(sum(byte_size),0) FROM download_tasks",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    ensure!(
        count <= MAX_DOWNLOAD_TASKS as i64 && bytes <= MAX_DOWNLOAD_DOCUMENT_BYTES as i64,
        "Download profile storage budget exceeded"
    );
    check_retry_storage(tx)?;
    Ok(())
}
pub(super) fn check_retry_storage(tx: &Transaction<'_>) -> Result<()> {
    let (count, bytes): (i64, i64) = tx.query_row(
        "SELECT count(*),coalesce(sum(byte_size),0) FROM (SELECT byte_size FROM download_owned_artifacts UNION ALL SELECT byte_size FROM download_allocation_intents)",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    ensure!(
        count <= 400 && bytes <= 16 * 1024 * 1024,
        "Download retry artifact metadata budget exhausted"
    );
    let mut reserved: i64 = tx.query_row(
        "SELECT coalesce(sum(reserved_bytes),0) FROM (SELECT reserved_bytes FROM download_owned_artifacts UNION ALL SELECT reserved_bytes FROM download_allocation_intents)",
        [],
        |r| r.get(0),
    )?;
    let mut projected_count = count;
    let mut projected_bytes = bytes;
    for task in tasks_at(tx)? {
        if let Some(intent) = task.commit_journal.as_ref().and_then(|j| j.retry.as_ref()) {
            let mut missing = 0i64;
            let mut unreserved_bytes = 0i64;
            for artifact in intent.artifacts.iter().filter(|a| a.allocation.is_none()) {
                let pending: Option<(String, i64, i64)> = tx.query_row(
                    "SELECT task_id,operation_epoch,reserved_bytes FROM download_allocation_intents WHERE nonce=?1",
                    [&artifact.nonce], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?))).optional()?;
                if let Some((pending_task, epoch, bytes)) = pending {
                    ensure!(
                        pending_task == task.id
                            && epoch == checked_revision(intent.operation_epoch)?
                            && bytes == i64::try_from(artifact.maximum_bytes)?,
                        "Native allocation intent differs from its retry reservation"
                    );
                } else {
                    missing += 1;
                    unreserved_bytes = unreserved_bytes
                        .checked_add(i64::try_from(artifact.maximum_bytes)?)
                        .context("Download retry reservation overflow")?;
                }
            }
            projected_count = projected_count
                .checked_add(missing)
                .context("Download retry row reservation overflow")?;
            projected_bytes = projected_bytes
                .checked_add(missing * 65536)
                .context("Download retry document reservation overflow")?;
            reserved = reserved
                .checked_add(unreserved_bytes)
                .context("Download retry reservation overflow")?;
        }
    }
    ensure!(
        projected_count <= 400 && projected_bytes <= 16 * 1024 * 1024,
        "Download retry producer metadata reservations exhausted"
    );
    ensure!(
        reserved <= crate::download::MAX_OWNED_PART_BYTES as i64,
        "Download postprocessing profile quota exhausted"
    );
    Ok(())
}

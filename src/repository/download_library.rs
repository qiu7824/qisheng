//! Download metadata never manufactures an ordinary explicit-file grant.

use super::*;
use crate::{download::task_model::DownloadTask, library::authority::DownloadedReadReceipt};

pub(crate) struct DownloadReadCommit {
    pub(crate) change: Option<RepositoryChange>,
    reader: DownloadedReadReceipt,
}
impl DownloadReadCommit {
    pub(crate) fn verify_reader(&self, reader: &DownloadedReadReceipt) -> Result<()> {
        self.reader.verify()?;
        reader.verify()?;
        ensure!(
            self.reader.is_same_reader(reader),
            "Repository ACK belongs to another actual read recipient"
        );
        Ok(())
    }
}

impl Repository {
    pub(crate) fn commit_download_library_metadata(
        &self,
        task: &DownloadTask,
        item: &LibraryPreparedAsset,
        receipt: &DownloadedReadReceipt,
        expected_revision: Option<u64>,
        check: &dyn Fn() -> Result<()>,
    ) -> Result<DownloadReadCommit> {
        check()?;
        task.validate()?;
        validate_prepared(item)?;
        receipt.verify()?;
        let facts = receipt.audio_facts()?;
        let origin = receipt.origin();
        let identity = receipt.file_identity();
        ensure!(
            task.profile_id == origin.profile_id
                && task.id == origin.task_id
                && task.operation_epoch >= origin.operation_epoch
                && task.audio_available
                && task.commit_journal.is_none()
                && item.file_identity.as_ref() == Some(&identity)
                && normalize_local_path(&item.asset.path) == normalize_local_path(&facts.path)
                && item.asset.size_bytes == facts.checkpoint.bytes,
            "Download metadata does not describe its actual acknowledged read owner"
        );
        let proof = task
            .output_audio
            .as_ref()
            .context("Download output proof missing")?;
        ensure!(
            proof.identity == facts.identity
                && proof.bytes == facts.checkpoint.bytes
                && proof.sha256 == facts.checkpoint.sha256
                && facts.path.file_name().and_then(|name| name.to_str())
                    == Some(proof.basename.as_str()),
            "Download task output differs from its actual read recipient"
        );
        let track = encode_document(&item.track)?;
        let asset = encode_document(&item.asset)?;
        let (_, change) = self.write_checked(expected_revision, |transaction| {
            check()?;
            receipt.verify()?;
            let document: String = transaction.query_row("SELECT document FROM download_tasks WHERE id=?1", [&task.id], |row| row.get(0))?;
            let stored: DownloadTask = decode_document(&document)?;
            ensure!(serde_json::to_value(&stored)? == serde_json::to_value(task)?, "Download changed while preparing local metadata");
            let artifact = crate::repository::download_artifacts::artifact_at(transaction, receipt.nonce())?.context("Actual download publication ACK missing")?;
            ensure!(artifact.phase == "published" && artifact.reserved_bytes == 0
                && artifact.actual_bytes == proof.bytes && artifact.sha256.as_deref() == Some(proof.sha256.as_str())
                && artifact.allocation.context == *origin && artifact.allocation.record.identity == proof.identity,
                "Download metadata requires the complete original publication ACK");
            tracks::write_track(transaction, &item.track, &track)?;
            tracks::write_asset(transaction, &item.asset, &asset)?;
            write_location(transaction, &item.asset, Some(&identity))?;
            // An old path-only selection cannot authorize this new physical file.
            let old: Option<String> = transaction.query_row("SELECT document FROM local_file_grants WHERE asset_id=?1", [&item.asset.id], |row| row.get(0)).optional()?;
            if let Some(document) = old {
                let value: serde_json::Value = decode_document(&document)?;
                let prior = value.get("file_identity").filter(|value| !value.is_null()).map(|value| serde_json::from_value::<FileIdentity>(value.clone())).transpose()?;
                if prior.as_ref() != Some(&identity) {
                    transaction.execute("DELETE FROM local_file_grants WHERE asset_id=?1", [&item.asset.id])?;
                }
            }
            refresh_list_metadata(transaction, &item.track, check)?;
            check_library_storage(transaction)?;
            receipt.verify()?;
            Ok(((), ChangeKind::LocalAssetsUpserted, None, serde_json::json!({"count":1,"track_ids":[item.track.id],"download_task_id":task.id})))
        }, check)?;
        Ok(DownloadReadCommit {
            change: Some(change),
            reader: receipt.clone(),
        })
    }

    pub(crate) fn acknowledge_download_sidecar_read(
        &self,
        task: &DownloadTask,
        receipt: &DownloadedReadReceipt,
    ) -> Result<DownloadReadCommit> {
        task.validate()?;
        receipt.verify()?;
        let facts = receipt.sidecar_facts()?;
        let origin = receipt.origin();
        let proof = task
            .stages
            .lyrics
            .artifact
            .as_ref()
            .context("Published sidecar proof missing")?;
        ensure!(
            task.profile_id == origin.profile_id
                && task.id == origin.task_id
                && task.operation_epoch >= origin.operation_epoch
                && task.audio_available
                && task.commit_journal.is_none()
                && proof.identity == facts.identity
                && proof.bytes == facts.bytes
                && proof.sha256 == facts.sha256
                && facts.path.file_name().and_then(|name| name.to_str())
                    == Some(proof.basename.as_str()),
            "Sidecar read receipt differs from the actual task publication"
        );
        let connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("Repository lock poisoned"))?;
        schema::validate_current(&connection)?;
        let transaction = connection.unchecked_transaction()?;
        let document: String = transaction.query_row(
            "SELECT document FROM download_tasks WHERE id=?1",
            [&task.id],
            |row| row.get(0),
        )?;
        let stored: DownloadTask = decode_document(&document)?;
        ensure!(
            serde_json::to_value(&stored)? == serde_json::to_value(task)?,
            "Published sidecar task changed"
        );
        let artifact =
            crate::repository::download_artifacts::artifact_at(&transaction, receipt.nonce())?
                .context("Published sidecar allocation missing")?;
        ensure!(
            artifact.phase == "published"
                && artifact.reserved_bytes == 0
                && artifact.actual_bytes == proof.bytes
                && artifact.sha256.as_deref() == Some(proof.sha256.as_str())
                && artifact.allocation.context == *origin
                && artifact.allocation.record.identity == proof.identity
                && artifact.allocation.kind
                    == crate::download::artifact::OwnedArtifactKind::LyricSidecar,
            "Sidecar read adoption requires the complete original repository ACK"
        );
        receipt.verify()?;
        transaction.commit()?;
        Ok(DownloadReadCommit {
            change: None,
            reader: receipt.clone(),
        })
    }
}

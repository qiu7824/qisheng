use super::*;
use crate::download::artifact::{OwnedArtifactIntent, OwnedArtifactJournal, OwnedArtifactKind};

pub(crate) struct ProducerWorkScope {
    _scope: producer_file::ProducerWorkerScope,
}
impl ProducerWorkScope {
    pub(crate) fn enter(worker: crate::audio::worker_owner::TrackedWorker) -> Result<Self> {
        Ok(Self {
            _scope: producer_file::ProducerWorkerScope::enter(worker)?,
        })
    }
}

pub(crate) struct DownloadProducerObject {
    file: producer_file::ProducerFile,
}
impl Clone for DownloadProducerObject {
    fn clone(&self) -> Self {
        Self {
            file: self.file.shared_borrow(),
        }
    }
}
impl DownloadProducerObject {
    pub(crate) fn inspect_retained_pins_after_join<R>(
        &self,
        inspect: impl FnOnce(&DestinationGrant) -> Result<R>,
    ) -> Result<R> {
        ensure!(
            self.file.producers_joined()?,
            "Actual native loans have not joined before pin retirement"
        );
        self.file.inspect_namespace(inspect)
    }
    pub(crate) fn close_for_cold_recovery_after_join(
        &self,
        receiver: Option<&crate::library::authority::DownloadedPinReceipt>,
    ) -> Result<bool> {
        self.file.close_for_cold_recovery_after_join(receiver)
    }
    pub(super) fn from_native_file(file: ProducerFile) -> Self {
        Self { file }
    }
    pub(crate) fn native_file(&self) -> Result<super::managed_native::DownloadNativeFile> {
        super::managed_native::DownloadNativeFile::from_original(self.file.shared_borrow())
    }
    pub(crate) fn verify_published_audio(
        &self,
        proof: &crate::download::task_model::DownloadArtifactProof,
        extension: &str,
        cancel: &DownloadCancel,
    ) -> Result<CommittedAudio> {
        proof.validate()?;
        let record = self.file.record_binding()?;
        let plan = self.file.selected_plan()?;
        ensure!(
            proof.identity == record.identity
                && proof.basename == record.final_basename
                && plan.basename == proof.basename
                && proof.bytes > 0,
            "Published decoding differs from its actual original native object"
        );
        let stamp = self.file.inspect(platform::stamp)?;
        let check = || -> Result<()> {
            cancel.check()?;
            plan.verify()?;
            self.file.inspect(|native| {
                platform::validate(native, false)?;
                ensure!(
                    platform::identity(native)? == proof.identity
                        && platform::link_count(native)? == 1
                        && platform::stamp(native)? == stamp
                        && stamp.length == proof.bytes,
                    "Actual published decoding representation changed"
                );
                platform::verify_file_name(plan.grant.parent(), &proof.basename, &proof.identity)
            })
        };
        check()?;
        let mut file = self.file.shared_borrow();
        file.seek(SeekFrom::Start(0))?;
        let mut body = Sha256::new();
        let mut bytes = 0u64;
        let mut buffer = [0u8; COPY_BUFFER_BYTES];
        loop {
            check()?;
            let read = file.read(&mut buffer)?;
            bytes = bytes
                .checked_add(read as u64)
                .context("Published native digest length overflow")?;
            ensure!(
                bytes <= proof.bytes,
                "Published native digest exceeds its original body bound"
            );
            if read == 0 {
                break;
            }
            body.update(&buffer[..read]);
        }
        ensure!(
            bytes == proof.bytes && format!("{:x}", body.finalize()) == proof.sha256,
            "Actual published decoding body differs from its durable output proof"
        );
        check()?;
        file.seek(SeekFrom::Start(0))?;
        let reader = GuardedReader {
            file,
            plan: plan.clone(),
            identity: record.identity.clone(),
            basename: proof.basename.clone(),
            cancel: cancel.clone(),
            execution: None,
        };
        let verification = crate::download::verify::verify_owned_reader(
            Box::new(reader),
            extension,
            cancel,
            &check,
        )?;
        check()?;
        Ok(CommittedAudio {
            path: plan.path(),
            identity: record.identity,
            checkpoint: TransferCheckpoint {
                bytes: proof.bytes,
                sha256: proof.sha256.clone(),
                strong_etag: None,
                remote_total: Some(proof.bytes),
                completed_body: true,
                representation: None,
            },
            verification,
            retained_part: None,
        })
    }
    pub(crate) fn context_binding(&self) -> Result<crate::download::DownloadContext> {
        self.file.context_binding()
    }
    pub(crate) fn record_binding(&self) -> Result<OwnedPartRecord> {
        self.file.record_binding()
    }
    pub(crate) fn release_adopted_after_join(
        &self,
        facts: &CommittedAudio,
        receipt: &crate::library::authority::DownloadedReadReceipt,
    ) -> Result<()> {
        self.file.release_adopted_after_join(facts, receipt)
    }
    pub(crate) fn inspect_published_after_join<R>(
        &self,
        facts: &CommittedAudio,
        inspect: impl FnOnce(&File, &DestinationGrant) -> Result<R>,
    ) -> Result<R> {
        self.file.inspect_published_after_join(facts, inspect)
    }
    pub(crate) fn inspect_sidecar_after_join<R>(
        &self,
        facts: &CommittedSidecar,
        inspect: impl FnOnce(&File, &DestinationGrant) -> Result<R>,
    ) -> Result<R> {
        self.file.inspect_sidecar_after_join(facts, inspect)
    }
    pub(crate) fn release_adopted_sidecar_after_join(
        &self,
        facts: &CommittedSidecar,
        receipt: &crate::library::authority::DownloadedReadReceipt,
    ) -> Result<()> {
        self.file.release_adopted_sidecar_after_join(facts, receipt)
    }
    pub(crate) fn bind_worker(
        &self,
        worker: crate::audio::worker_owner::TrackedWorker,
    ) -> Result<()> {
        self.file.attach_worker(worker)
    }
    pub(crate) fn producers_joined(&self) -> Result<bool> {
        self.file.producers_joined()
    }
}

/// Runs actual artifact work behind a matching tracked start gate. Its result
/// is returned only after thread and native TLS teardown have really joined.
pub fn run_download_producer<R: Send + 'static>(
    operation: impl FnOnce() -> Result<R> + Send + 'static,
) -> Result<R> {
    use crate::audio::worker_owner::{WorkerKind, WorkerPermit};
    let permit = WorkerPermit::reserve(WorkerKind::Download)?;
    let ticket = permit.tracker();
    let (start, ready) = std::sync::mpsc::sync_channel(1);
    let (notify, result) = std::sync::mpsc::sync_channel(1);
    let handle = std::thread::Builder::new()
        .name("download-artifact-work".into())
        .spawn(move || {
            if ready.recv().is_err() {
                return;
            }
            let completed = (|| -> Result<R> {
                let _scope = ProducerWorkScope::enter(ticket)?;
                operation()
            })();
            let _ = notify.send(completed);
        })?;
    let worker = permit.attach(handle);
    start
        .send(())
        .context("Cannot start admitted download producer")?;
    ensure!(
        worker.wait(std::time::Duration::from_secs(30)),
        "Download producer shutdown deadline exceeded"
    );
    ensure!(!worker.panicked(), "Download producer operation panicked");
    result
        .try_recv()
        .context("Joined download producer returned no result")?
}

pub(crate) fn has_retained_download_objects(profile_id: &str) -> Result<bool> {
    producer_file::ProducerFile::retained_for_profile(profile_id)
}
pub(crate) fn retained_download_profile_objects(
    profile_id: &str,
) -> Result<Vec<DownloadProducerObject>> {
    Ok(
        producer_file::ProducerFile::retained_profile_targets(profile_id)?
            .into_iter()
            .map(DownloadProducerObject::from_native_file)
            .collect(),
    )
}
pub(crate) fn close_read_task_handles_after_join(
    profile: &str,
    task: &str,
    nonces: &[String],
) -> Result<bool> {
    let targets = producer_file::ProducerFile::retained_read_targets(profile, task, nonces)?;
    for file in &targets {
        if !file.producers_joined()? {
            return Ok(false);
        }
    }
    for file in targets {
        file.close_after_join()?;
    }
    Ok(true)
}
pub(crate) fn retained_read_task_objects_after_join(
    profile: &str,
    task: &str,
    nonces: &[String],
) -> Result<Vec<DownloadProducerObject>> {
    let objects = producer_file::ProducerFile::retained_read_targets(profile, task, nonces)?
        .into_iter()
        .map(DownloadProducerObject::from_native_file)
        .collect::<Vec<_>>();
    ensure!(
        objects
            .iter()
            .all(|object| object.producers_joined().unwrap_or(false)),
        "Retained read native loans have not actually joined"
    );
    Ok(objects)
}
pub(crate) fn retire_read_task_pins_for_recovery(
    profile: &str,
    task: &str,
    nonces: &[String],
    receivers: &[crate::library::authority::DownloadedPinReceipt],
) -> Result<bool> {
    let targets = producer_file::ProducerFile::retained_read_targets(profile, task, nonces)?;
    let mut complete = true;
    for file in targets {
        let receiver = receivers.iter().find(|receiver| {
            file.inspect_namespace(|grant| receiver.matches_grant(grant))
                .is_ok()
        });
        complete &= file.close_for_cold_recovery_after_join(receiver)?;
    }
    Ok(complete)
}

impl DestinationGrant {
    pub(crate) fn recover_owned_artifact_retained(
        &self,
        allocation: &crate::download::artifact::OwnedArtifactAllocation,
        proof: &crate::download::task_model::DownloadArtifactProof,
        cancel: &DownloadCancel,
        acknowledge: &dyn Fn(
            &crate::download::artifact::OwnedArtifactAllocation,
            &crate::download::task_model::DownloadArtifactProof,
        ) -> Result<()>,
    ) -> Result<DownloadProducerObject> {
        allocation.validate()?;
        proof.validate()?;
        self.verify()?;
        cancel.check()?;
        ensure!(
            allocation.record.identity == proof.identity
                && allocation.record.final_basename == proof.basename
                && allocation.record.destination_identity == *self.identity()
                && allocation.record.destination_epoch == self.epoch()
                && allocation.working_directory_identity == *self.working_directory_identity(),
            "Explicit recovery differs from its actual selected root and published object"
        );
        let plan = DownloadPlan {
            grant: self.clone(),
            basename: proof.basename.clone(),
            policy: CollisionPolicy::Skip,
            root_identity: self.identity().clone(),
            epoch: self.epoch(),
        };
        let file = if let Some(file) =
            producer_file::ProducerFile::retain_original(&plan, &allocation.record)?
        {
            if file.metadata().is_err() {
                file.reopen_closed_published(&plan, &allocation.record)?;
            }
            file
        } else {
            let file = producer_file::ProducerFile::reserve(
                plan,
                allocation.record.nonce.clone(),
                allocation.kind,
                Some(allocation.context.clone()),
                allocation.reserved_max_bytes,
            )?;
            file.reopen_published(&allocation.record)?;
            file.inspect(|native| {
                platform::validate(native, false)?;
                ensure!(
                    platform::identity(native)? == proof.identity
                        && platform::link_count(native)? == 1,
                    "Actual recovery descriptor identity or single link changed"
                );
                platform::verify_file_name(self.parent(), &proof.basename, &proof.identity)
            })?;
            file.remember_record(allocation.record.clone())?;
            file.reconcile_bytes()?;
            file
        };
        // The already admitted native object is verified, never reconstructed
        // into a read grant from the supplied output facts.
        let mut reader = file.shared_borrow();
        reader.seek(SeekFrom::Start(0))?;
        let initial = file.inspect(platform::stamp)?;
        let mut digest = Sha256::new();
        let mut bytes = 0u64;
        let mut buffer = [0u8; COPY_BUFFER_BYTES];
        loop {
            cancel.check()?;
            self.verify()?;
            ensure!(
                file.inspect(platform::stamp)? == initial,
                "Actual recovery body changed before read"
            );
            let read = reader.read(&mut buffer)?;
            bytes += read as u64;
            ensure!(
                bytes <= proof.bytes && bytes <= MAX_DOWNLOAD_BYTES,
                "Actual recovery body exceeds its bound"
            );
            if read == 0 {
                break;
            }
            digest.update(&buffer[..read]);
        }
        ensure!(
            bytes == proof.bytes
                && format!("{:x}", digest.finalize()) == proof.sha256
                && file.inspect(platform::stamp)? == initial,
            "Actual retained recovery body differs"
        );
        file.acknowledge_created(|native, actual_plan, _, _| {
            cancel.check()?;
            actual_plan.verify()?;
            ensure!(
                platform::identity(native)? == allocation.record.identity
                    && platform::link_count(native)? == 1
                    && platform::stamp(native)? == initial,
                "Actual recovery changed before its durable reopening acknowledgement"
            );
            acknowledge(allocation, proof)?;
            cancel.check()?;
            actual_plan.verify()?;
            ensure!(
                platform::stamp(native)? == initial,
                "Actual recovery changed during its durable reopening acknowledgement"
            );
            Ok(allocation.record.clone())
        })?;
        Ok(DownloadProducerObject { file })
    }
}

pub use crate::download::artifact::CreationJournalRequired;

impl OwnedPart {
    pub(crate) fn producer_object(&self) -> DownloadProducerObject {
        DownloadProducerObject {
            file: self.file.shared_borrow(),
        }
    }
    /// Missing task authority is rejected before reserving or creating a file.
    pub fn create(_plan: DownloadPlan) -> Result<Self> {
        Err(CreationJournalRequired.into())
    }

    pub fn reopen(_plan: DownloadPlan, _record: OwnedPartRecord) -> Result<Self> {
        Err(CreationJournalRequired.into())
    }

    pub fn reopen_journaled(
        plan: DownloadPlan,
        record: OwnedPartRecord,
        context: &crate::download::DownloadContext,
        maximum_bytes: u64,
        journal: &dyn OwnedArtifactJournal,
        cancel: &DownloadCancel,
    ) -> Result<Self> {
        cancel.check()?;
        context.validate()?;
        plan.verify()?;
        validate_basename(&record.basename)?;
        ensure!(
            record.nonce.len() == 32
                && record.nonce.bytes().all(|byte| byte.is_ascii_hexdigit())
                && record.basename == format!(".lx-download-{}.part", record.nonce)
                && record.destination_epoch == plan.epoch
                && record.destination_identity == plan.root_identity
                && record.final_basename == plan.basename,
            "Owned partial does not belong to its actual selected destination"
        );
        let file = if let Some(file) = producer_file::ProducerFile::retain_original(&plan, &record)?
        {
            if file.metadata().is_err() {
                file.reopen_closed(&plan, &record)?;
            }
            file
        } else {
            let file = producer_file::ProducerFile::reserve(
                plan.clone(),
                record.nonce.clone(),
                OwnedArtifactKind::Transfer,
                Some(context.clone()),
                maximum_bytes,
            )?;
            file.reopen()?;
            file
        };
        let group = plan
            .grant
            .0
            .parent
            .as_ref()
            .map(|_| {
                plan.grant
                    .path()
                    .file_name()
                    .and_then(|name| name.to_str())
                    .context("Actual reopen group is invalid")
                    .map(str::to_owned)
            })
            .transpose()?;
        let allocation = crate::download::artifact::OwnedArtifactAllocation {
            kind: OwnedArtifactKind::Transfer,
            record: record.clone(),
            working_directory_path: plan.grant.path().to_owned(),
            working_directory_identity: plan.grant.working_directory_identity().clone(),
            selected_root_identity: plan.root_identity.clone(),
            relative_group: group,
            reserved_max_bytes: maximum_bytes,
            context: context.clone(),
        };
        allocation.validate()?;
        file.acknowledge_created(|native, actual_plan, _, _| {
            actual_plan.verify()?;
            platform::validate(native, false)?;
            ensure!(
                platform::identity(native)? == record.identity
                    && platform::link_count(native)? == 1
                    && native.metadata()?.len() <= maximum_bytes,
                "Reopened actual partial identity, link count or length changed"
            );
            platform::verify_file_name(
                actual_plan.grant.parent(),
                &record.basename,
                &record.identity,
            )?;
            journal.record_reopened(&allocation)?;
            cancel.check()?;
            actual_plan.verify()?;
            ensure!(
                platform::identity(native)? == record.identity
                    && platform::link_count(native)? == 1
                    && native.metadata()?.len() <= maximum_bytes,
                "Actual reopened partial changed during its repository acknowledgement"
            );
            Ok(record.clone())
        })?;
        file.reconcile_bytes()?;
        Ok(Self {
            plan,
            record,
            file,
            quota: None,
            integrity: None,
            execution: None,
        })
    }

    pub fn create_journaled(
        plan: DownloadPlan,
        kind: OwnedArtifactKind,
        context: &crate::download::DownloadContext,
        maximum_bytes: u64,
        journal: &dyn OwnedArtifactJournal,
        cancel: &DownloadCancel,
    ) -> Result<Self> {
        let mut nonce = [0u8; 16];
        getrandom::fill(&mut nonce)
            .map_err(|_| anyhow::anyhow!("Cannot allocate owned download identity"))?;
        let nonce: String = nonce.iter().map(|byte| format!("{byte:02x}")).collect();
        Self::create_with_nonce_journaled(
            plan,
            nonce,
            kind,
            context,
            maximum_bytes,
            journal,
            cancel,
        )
    }

    pub(crate) fn create_with_nonce_journaled(
        plan: DownloadPlan,
        nonce: String,
        kind: OwnedArtifactKind,
        context: &crate::download::DownloadContext,
        maximum_bytes: u64,
        journal: &dyn OwnedArtifactJournal,
        cancel: &DownloadCancel,
    ) -> Result<Self> {
        cancel.check()?;
        context.validate()?;
        plan.verify()?;
        let relative_group = plan
            .grant
            .0
            .parent
            .as_ref()
            .map(|_| {
                plan.grant
                    .path()
                    .file_name()
                    .and_then(|name| name.to_str())
                    .context("Actual allocation group is invalid")
                    .map(str::to_owned)
            })
            .transpose()?;
        let intent = OwnedArtifactIntent {
            kind,
            nonce: nonce.clone(),
            basename: format!(".lx-download-{nonce}.part"),
            final_basename: plan.basename.clone(),
            working_directory_path: plan.grant.path().to_owned(),
            working_directory_identity: plan.grant.working_directory_identity().clone(),
            selected_root_identity: plan.root_identity.clone(),
            destination_epoch: plan.epoch,
            relative_group,
            reserved_max_bytes: maximum_bytes,
            context: context.clone(),
        };
        intent.validate()?;
        let file = producer_file::ProducerFile::reserve(
            plan.clone(),
            nonce,
            kind,
            Some(context.clone()),
            maximum_bytes,
        )?;
        file.create(|actual_plan, basename, nonce, maximum| {
            ensure!(
                actual_plan.root_identity == intent.selected_root_identity
                    && actual_plan.epoch == intent.destination_epoch
                    && basename == intent.basename
                    && nonce == intent.nonce
                    && maximum == intent.reserved_max_bytes,
                "Actual allocation slot differs from its namespace intent"
            );
            cancel.check()?;
            journal.record_intent(&intent)?;
            cancel.check()?;
            actual_plan.verify()
        })?;
        // Every error below leaves the captured original in the same keeper.
        let (record, stamp) = file.inspect(|native| {
            plan.verify()?;
            platform::validate(native, false)?;
            ensure!(
                native.metadata()?.len() == 0 && platform::link_count(native)? == 1,
                "Actual creation is not its original zero-length single-link object"
            );
            let identity = platform::identity(native)?;
            platform::verify_file_name(plan.grant.parent(), &intent.basename, &identity)?;
            Ok((
                OwnedPartRecord {
                    basename: intent.basename.clone(),
                    nonce: intent.nonce.clone(),
                    identity,
                    destination_identity: plan.root_identity.clone(),
                    destination_epoch: plan.epoch,
                    final_basename: plan.basename.clone(),
                },
                platform::stamp(native)?,
            ))
        })?;
        file.remember_record(record.clone())?;
        let part = Self {
            plan,
            record,
            file,
            quota: None,
            integrity: Some(Integrity {
                hasher: Sha256::new(),
                bytes: 0,
                stamp,
            }),
            execution: None,
        };
        part.acknowledge_artifact_allocation(kind, context, maximum_bytes, journal, cancel)?;
        Ok(part)
    }

    /// Explicit retirement uses the actual kept object, including after a
    /// discarded result. Facts alone cannot recreate a native deletion owner.
    pub fn retire_retained(plan: DownloadPlan, record: OwnedPartRecord) -> Result<OwnedPartRecord> {
        let file = producer_file::ProducerFile::retain_original(&plan, &record)?
            .context("No retained actual object is available for retirement")?;
        ensure!(
            file.producers_joined()?,
            "Actual producer has not joined before retirement"
        );
        file.request_owned_removal()?;
        file.close_after_join()?;
        drop(plan);
        file.release_removed()
    }

    /// This receipt proves only an actual close. It retains the artifact slot,
    /// selected pins and bytes until physical retirement or Library adoption.
    pub fn close_retained(plan: &DownloadPlan, record: &OwnedPartRecord) -> Result<()> {
        let file = producer_file::ProducerFile::retain_original(plan, record)?
            .context("No retained actual object is available for close")?;
        file.close_after_join()
    }
}

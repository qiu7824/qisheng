use super::*;
use crate::download::artifact::{OwnedArtifactAllocation, OwnedArtifactIntent, OwnedArtifactKind};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DownloadNativeStatus {
    Reserved,
    AttemptedNoHandle,
    Captured,
    Closed,
}

/// A loan of the original producer object. This value never owns a second FH
/// and never closes the original through ordinary Rust destruction.
pub(crate) struct DownloadNativeFile {
    file: ProducerFile,
    allocation: Option<OwnedArtifactAllocation>,
}
pub(crate) fn prepare_retry_native_file(
    plan: DownloadPlan,
    nonce: String,
    kind: OwnedArtifactKind,
    context: crate::download::DownloadContext,
    maximum_bytes: u64,
) -> Result<DownloadNativeFile> {
    let file = ProducerFile::reserve(plan, nonce, kind, Some(context), maximum_bytes)?;
    Ok(DownloadNativeFile {
        file,
        allocation: None,
    })
}
impl DownloadNativeFile {
    pub(super) fn from_original(file: ProducerFile) -> Result<Self> {
        let allocation = Some(file.allocation_binding()?);
        Ok(Self { file, allocation })
    }
    pub(crate) fn shared_borrow(&self) -> Self {
        Self {
            file: self.file.shared_borrow(),
            allocation: self.allocation.clone(),
        }
    }
    pub(crate) fn status(&self) -> Result<DownloadNativeStatus> {
        self.file.native_status()
    }
    pub(crate) fn allocation(&self) -> Result<OwnedArtifactAllocation> {
        self.allocation
            .clone()
            .map_or_else(|| self.file.allocation_binding(), Ok)
    }
    pub(crate) fn creation_acknowledged(&self) -> Result<bool> {
        self.file.creation_acknowledged()
    }
    pub(crate) fn acknowledge_existing_with_native_ack(
        &mut self,
        created_ack: impl FnOnce(&File, &File, &OwnedArtifactAllocation) -> Result<()>,
    ) -> Result<OwnedArtifactAllocation> {
        ensure!(
            self.file.producers_joined()?,
            "Actual managed allocation producer has not joined before acknowledgement retry"
        );
        ensure!(
            !self.creation_acknowledged()?,
            "Managed allocation already has its native creation acknowledgement"
        );
        let allocation = self.file.allocation_binding()?;
        let stamp = self.file.inspect(|native| {
            platform::validate(native, false)?;
            ensure!(native.metadata()?.len() == 0
                && platform::identity(native)? == allocation.record.identity
                && platform::link_count(native)? == 1,
                "Managed acknowledgement retry requires its original empty single-link native object");
            Ok(platform::stamp(native)?)
        })?;
        self.file
            .acknowledge_created(|native, plan, basename, nonce| {
                ensure!(
                    basename == allocation.record.basename
                        && nonce == allocation.record.nonce
                        && native.metadata()?.len() == 0
                        && platform::identity(native)? == allocation.record.identity
                        && platform::link_count(native)? == 1
                        && platform::stamp(native)? == stamp,
                    "Managed original changed before its creation acknowledgement retry"
                );
                plan.verify()?;
                platform::verify_file_name(
                    plan.grant.parent(),
                    basename,
                    &allocation.record.identity,
                )?;
                created_ack(native, plan.grant.parent(), &allocation)?;
                plan.verify()?;
                ensure!(
                    native.metadata()?.len() == 0 && platform::stamp(native)? == stamp,
                    "Managed original changed during its creation acknowledgement retry"
                );
                Ok(allocation.record.clone())
            })?;
        self.allocation = Some(allocation.clone());
        Ok(allocation)
    }
    pub(crate) fn create_with_native_ack(
        &mut self,
        intent_ack: impl FnOnce(&OwnedArtifactIntent) -> Result<()>,
        created_ack: impl FnOnce(&File, &File, &OwnedArtifactAllocation) -> Result<()>,
    ) -> Result<OwnedArtifactAllocation> {
        let intent = self.file.intent_binding()?;
        let plan = self.file_plan_for_callback(&intent)?;
        self.file.create(|_, _, _, _| intent_ack(&intent))?;
        let (record, stamp) = self.file.inspect(|native| {
            platform::validate(native, false)?;
            ensure!(
                native.metadata()?.len() == 0 && platform::link_count(native)? == 1,
                "Managed tag allocation is not its actual empty single-link object"
            );
            let identity = platform::identity(native)?;
            platform::verify_file_name(plan.grant.parent(), &intent.basename, &identity)?;
            Ok((
                OwnedPartRecord {
                    basename: intent.basename.clone(),
                    nonce: intent.nonce.clone(),
                    identity,
                    destination_identity: intent.selected_root_identity.clone(),
                    destination_epoch: intent.destination_epoch,
                    final_basename: intent.final_basename.clone(),
                },
                platform::stamp(native)?,
            ))
        })?;
        self.file.remember_record(record)?;
        let allocation = self.file.allocation_binding()?;
        self.allocation = Some(allocation.clone());
        self.file.acknowledge_created(|native, plan, _, _| {
            ensure!(
                native.metadata()?.len() == 0
                    && platform::identity(native)? == allocation.record.identity
                    && platform::link_count(native)? == 1
                    && platform::stamp(native)? == stamp,
                "Managed tag allocation changed before its actual empty-file acknowledgement"
            );
            plan.verify()?;
            created_ack(native, plan.grant.parent(), &allocation)?;
            plan.verify()?;
            ensure!(
                native.metadata()?.len() == 0 && platform::stamp(native)? == stamp,
                "Managed tag allocation changed during its actual empty-file acknowledgement"
            );
            Ok(allocation.record.clone())
        })?;
        Ok(allocation)
    }
    fn file_plan_for_callback(&self, intent: &OwnedArtifactIntent) -> Result<DownloadPlan> {
        // This helper is called before entering the native callback, below.
        let plan = self.file.selected_plan()?;
        ensure!(
            plan.basename == intent.final_basename
                && plan.root_identity == intent.selected_root_identity,
            "Managed tag allocation selected plan changed"
        );
        Ok(plan)
    }
    pub(crate) fn inspect_native<R>(&self, inspect: impl FnOnce(&File) -> Result<R>) -> Result<R> {
        self.file.inspect(inspect)
    }
    pub(crate) fn inspect_native_io<R>(
        &self,
        inspect: impl FnOnce(&File) -> Result<R>,
    ) -> Result<R> {
        self.file.inspect_io(inspect)
    }
    pub(crate) fn producer_object(&self) -> DownloadProducerObject {
        DownloadProducerObject::from_native_file(self.file.shared_borrow())
    }
    pub(crate) fn metadata(&self) -> io::Result<std::fs::Metadata> {
        self.file.metadata()
    }
    pub(crate) fn sync_all(&self) -> io::Result<()> {
        self.file.sync_all()
    }
    pub(crate) fn set_len(&self, length: u64) -> io::Result<()> {
        self.file.set_len(length)
    }
    pub(crate) fn close_after_join(&self) -> Result<()> {
        self.file.close_after_join()
    }
    pub(crate) fn request_owned_removal(&self) -> Result<()> {
        self.file.request_owned_removal()
    }
    pub(crate) fn release_retired(&self) -> Result<OwnedPartRecord> {
        self.file.release_removed()
    }
}
impl Read for DownloadNativeFile {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        self.file.read(bytes)
    }
}
impl Write for DownloadNativeFile {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.file.write(bytes)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}
impl Seek for DownloadNativeFile {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        self.file.seek(position)
    }
}

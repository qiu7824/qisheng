//! M06 borrows the M07 producer's same native object; no descriptor is copied.
use super::*;
use crate::download::DownloadNativeFile;

pub struct TagManagedArtifact {
    pub(crate) file: DownloadNativeFile,
}
impl TagManagedArtifact {
    pub(crate) fn from_download(file: DownloadNativeFile) -> Self {
        Self { file }
    }
}
pub(super) enum TagNativeFile {
    Native(File),
    Download(DownloadNativeFile),
}
#[derive(Clone, Copy)]
pub(super) enum ArtifactAllocator {
    Native,
    OwnedNative,
    Managed,
}
pub(super) trait NativeFileRef {
    fn inspect_native<R>(&self, inspect: impl FnOnce(&File) -> Result<R>) -> Result<R>;
}
impl<T: NativeFileRef> NativeFileRef for &T {
    fn inspect_native<R>(&self, inspect: impl FnOnce(&File) -> Result<R>) -> Result<R> {
        (**self).inspect_native(inspect)
    }
}
impl NativeFileRef for File {
    fn inspect_native<R>(&self, inspect: impl FnOnce(&File) -> Result<R>) -> Result<R> {
        inspect(self)
    }
}
impl NativeFileRef for TagNativeFile {
    fn inspect_native<R>(&self, inspect: impl FnOnce(&File) -> Result<R>) -> Result<R> {
        match self {
            Self::Native(file) => inspect(file),
            Self::Download(file) => file.inspect_native(inspect),
        }
    }
}
pub(super) fn actual_identity(file: &impl NativeFileRef) -> Result<FileIdentity> {
    file.inspect_native(raw_actual_identity)
}
pub(super) fn actual_path(file: &impl NativeFileRef) -> Result<PathBuf> {
    file.inspect_native(raw_actual_path)
}
pub(super) fn normal_file(file: &impl NativeFileRef) -> Result<bool> {
    file.inspect_native(raw_normal_file)
}
pub(super) fn links(file: &impl NativeFileRef) -> Result<u32> {
    file.inspect_native(raw_links)
}
impl TagNativeFile {
    pub(super) fn is_download(&self) -> bool {
        matches!(self, Self::Download(_))
    }
    pub(super) fn managed_borrow(&self) -> Option<DownloadNativeFile> {
        match self {
            Self::Native(_) => None,
            Self::Download(file) => Some(file.shared_borrow()),
        }
    }
    pub(super) fn managed_status(&self) -> Result<Option<crate::download::DownloadNativeStatus>> {
        match self {
            Self::Native(_) => Ok(None),
            Self::Download(file) => Ok(Some(file.status()?)),
        }
    }
    pub(super) fn metadata(&self) -> io::Result<std::fs::Metadata> {
        match self {
            Self::Native(file) => file.metadata(),
            Self::Download(file) => file.metadata(),
        }
    }
    pub(super) fn sync_all(&self) -> io::Result<()> {
        match self {
            Self::Native(file) => file.sync_all(),
            Self::Download(file) => file.sync_all(),
        }
    }
    pub(super) fn set_len(&self, bytes: u64) -> io::Result<()> {
        match self {
            Self::Native(file) => file.set_len(bytes),
            Self::Download(file) => file.set_len(bytes),
        }
    }
    pub(super) fn native_metadata_io<R>(
        &self,
        inspect: impl FnOnce(&File) -> Result<R>,
    ) -> Result<R> {
        match self {
            Self::Native(file) => inspect(file),
            Self::Download(file) => file.inspect_native_io(inspect),
        }
    }
    pub(super) fn close(slot: &mut Option<Self>) -> Result<()> {
        let Some(file) = slot.as_mut() else {
            return Ok(());
        };
        match file {
            Self::Native(_) => {
                let Self::Native(file) = slot.take().unwrap() else {
                    unreachable!()
                };
                let mut original = Some(file);
                if let Err(error) = retire_native::close(&mut original) {
                    *slot = original.map(Self::Native);
                    return Err(error);
                }
            }
            Self::Download(file) => {
                file.close_after_join()?;
                slot.take();
            }
        }
        Ok(())
    }
    pub(super) fn request_removal(&self) -> Result<()> {
        match self {
            Self::Native(file) => retire_native::disposition(file),
            Self::Download(file) => file.request_owned_removal(),
        }
    }
}
pub(super) struct ManagedAllocationAck<'a> {
    pub(super) selection: &'a TagWriteSelection,
    pub(super) role: TagInstallArtifactRole,
    pub(super) path: &'a Path,
    pub(super) directory: &'a File,
    pub(super) maximum: u64,
}
impl ManagedAllocationAck<'_> {
    pub(super) fn acknowledge(
        &self,
        file: &File,
        parent: &File,
        allocation: &crate::download::artifact::OwnedArtifactAllocation,
        coordinator: &mut dyn TagInstallCoordinator,
    ) -> Result<()> {
        let facts = TagInstallArtifactFacts {
            role: self.role,
            path: self.path.to_owned(),
            identity: raw_actual_identity(file)?,
            directory_identity: raw_actual_identity(parent)?,
        };
        ensure!(
            normalize_local_path(&raw_actual_path(parent)?)
                == normalize_local_path(&actual_path(self.directory)?)
                && facts.directory_identity == actual_identity(self.directory)?,
            "Managed tag allocator returned another actual parent"
        );
        allocation.validate()?;
        let expected_kind = if self.role == TagInstallArtifactRole::Candidate {
            crate::download::artifact::OwnedArtifactKind::TagCopy
        } else {
            crate::download::artifact::OwnedArtifactKind::RollbackOriginal
        };
        ensure!(
            self.path.file_name().and_then(|name| name.to_str())
                == Some(allocation.record.basename.as_str())
                && allocation.reserved_max_bytes == self.maximum
                && allocation.kind == expected_kind
                && FileIdentity::from_native_parts(
                    allocation.record.identity.device,
                    &allocation.record.identity.object,
                )? == facts.identity
                && FileIdentity::from_native_parts(
                    allocation.working_directory_identity.device,
                    &allocation.working_directory_identity.object,
                )? == facts.directory_identity
                && normalize_local_path(&allocation.working_directory_path)
                    == normalize_local_path(
                        self.path
                            .parent()
                            .context("Managed tag artifact parent is absent")?
                    ),
            "Managed tag allocation facts differ from its retained native namespace"
        );
        let allocation = TagArtifactAllocation {
            file,
            directory: parent,
            facts: &facts,
        };
        allocation.check_empty()?;
        coordinator.artifact_allocated(self.selection, &allocation)
    }
}
pub(super) fn allocate_artifact(
    slot: &mut Option<TagNativeFile>,
    selection: &TagWriteSelection,
    role: TagInstallArtifactRole,
    path: &Path,
    directory: &File,
    allocator: ArtifactAllocator,
    coordinator: &mut dyn TagInstallCoordinator,
) -> Result<()> {
    ensure!(
        slot.is_none(),
        "Tag artifact already has a native or pre-admitted owner"
    );
    let maximum = coordinator.artifact_limit(role);
    if matches!(allocator, ArtifactAllocator::Managed) {
        let managed = coordinator
            .prepare_managed_artifact(selection, role, path, maximum)?
            .context("Managed tag writer has no pre-admitted artifact allocator")?;
        // The shared 104-slot object is under M06 before any native attempt.
        *slot = Some(TagNativeFile::Download(managed.file));
        let TagNativeFile::Download(file) = slot.as_mut().unwrap() else {
            unreachable!()
        };
        let acknowledgment = ManagedAllocationAck {
            selection,
            role,
            path,
            directory,
            maximum,
        };
        let coordinator = std::cell::RefCell::new(coordinator);
        file.create_with_native_ack(
            |intent| {
                coordinator
                    .borrow_mut()
                    .before_managed_artifact_allocation(selection, role, intent)
            },
            |file, parent, allocation| {
                acknowledgment.acknowledge(
                    file,
                    parent,
                    allocation,
                    &mut **coordinator.borrow_mut(),
                )
            },
        )?;
        Ok(())
    } else {
        coordinator.before_artifact_allocation(selection, role)?;
        *slot = Some(TagNativeFile::Native(create_owned_file(path)?));
        Ok(())
    }
}
impl Read for TagNativeFile {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        match self {
            Self::Native(file) => file.read(buffer),
            Self::Download(file) => file.read(buffer),
        }
    }
}
impl Write for TagNativeFile {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        match self {
            Self::Native(file) => file.write(buffer),
            Self::Download(file) => file.write(buffer),
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        match self {
            Self::Native(file) => file.flush(),
            Self::Download(file) => file.flush(),
        }
    }
}
impl Seek for TagNativeFile {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        match self {
            Self::Native(file) => file.seek(position),
            Self::Download(file) => file.seek(position),
        }
    }
}

#[cfg(all(test, windows))]
mod tests;

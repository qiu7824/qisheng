//! Download publication keeps the original native file and destination pins.

use super::*;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DownloadPublicationPhase {
    OwnedPartial,
    PublicationAttempted,
    Published,
    RepositoryAcknowledged,
}

struct PublicationOwner {
    audio: VerifiedOwnedAudio,
    phase: DownloadPublicationPhase,
    published_stamp: Option<FileStamp>,
}

/// Every failed publication returns its original file, including failures after
/// the native rename succeeded. The caller must retain it for recovery.
pub(crate) struct PublicationFailure {
    cause: anyhow::Error,
    owner: Box<PublicationOwner>,
}

impl std::fmt::Debug for PublicationFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PublicationFailure")
            .field("phase", &self.owner.phase)
            .field("record", &self.owner.audio.part.record)
            .field("cause", &self.cause)
            .finish()
    }
}
impl std::fmt::Display for PublicationFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "Download publication retains its native owner: {}",
            self.cause
        )
    }
}
impl std::error::Error for PublicationFailure {}

impl PublicationFailure {
    pub(crate) fn phase(&self) -> DownloadPublicationPhase {
        self.owner.phase
    }
    pub(crate) fn cause(&self) -> &anyhow::Error {
        &self.cause
    }
    pub(crate) fn record(&self) -> &OwnedPartRecord {
        self.owner.audio.part.record()
    }
    /// Shutdown closes handles without deleting either possible namespace or
    /// releasing the durable artifact reservation.
    pub(crate) fn into_retirement(self) -> PublishedAudioRetirement {
        self.owner.into_handle_retirement()
    }

    /// Rechecks the original native object; neither an error message nor a
    /// stored phase establishes whether the destination was published.
    pub(crate) fn recover_published(self) -> std::result::Result<PublishedDownloadAudio, Self> {
        let Self { owner, cause: _ } = self;
        let mut owner = *owner;
        match owner.verify_published_body() {
            Ok(()) => {
                owner.phase = DownloadPublicationPhase::Published;
                Ok(PublishedDownloadAudio { owner })
            }
            Err(cause) => Err(Self {
                cause,
                owner: Box::new(owner),
            }),
        }
    }

    /// A pre-publication owner can be retried only while its original private
    /// name and file still agree with the original execution authority.
    pub(crate) fn recover_partial(self) -> std::result::Result<VerifiedOwnedAudio, Self> {
        let Self { owner, cause: _ } = self;
        let owner = *owner;
        let check = (|| -> Result<()> {
            ensure!(
                matches!(
                    owner.phase,
                    DownloadPublicationPhase::OwnedPartial
                        | DownloadPublicationPhase::PublicationAttempted
                ),
                "A proved published object cannot return to partial transfer"
            );
            owner.audio.cancel.check()?;
            owner.audio.execution_lease.check(&owner.audio.context)?;
            // On Windows the original single-link, no-delete-sharing file at
            // its private name proves that a failed no-replace rename did not
            // publish this object. A Unix linked publication has two links and
            // cannot pass the same partial-file proof.
            owner.audio.part.verify()
        })();
        match check {
            Ok(()) => Ok(owner.audio),
            Err(cause) => Err(Self {
                cause,
                owner: Box::new(owner),
            }),
        }
    }
}

/// This owner cannot be constructed from a path, task document or output DTO.
pub(crate) struct PublishedDownloadAudio {
    owner: PublicationOwner,
}
impl std::fmt::Debug for PublishedDownloadAudio {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PublishedDownloadAudio")
            .field("phase", &self.owner.phase)
            .field("record", &self.owner.audio.part.record)
            .finish()
    }
}

impl PublicationOwner {
    fn into_handle_retirement(self) -> PublishedAudioRetirement {
        let part = self.audio.part;
        PublishedAudioRetirement {
            files: vec![part.file],
            grant: Some(part.plan.grant),
        }
    }
    fn verify_published(&self) -> Result<()> {
        let part = &self.audio.part;
        part.plan.grant.verify()?;
        platform::validate(&part.file, false)?;
        ensure!(
            platform::identity(&part.file)? == part.record.identity
                && platform::link_count(&part.file)? == 1
                && part.file.metadata()?.len() == self.audio.checkpoint.bytes,
            "Published download physical identity, link count or length changed"
        );
        platform::verify_file_name(
            part.plan.grant.parent(),
            &part.plan.basename,
            &part.record.identity,
        )?;
        if let Some(stamp) = &self.published_stamp {
            ensure!(
                platform::stamp(&part.file)? == *stamp,
                "Published download changed"
            );
        }
        Ok(())
    }

    fn verify_published_body(&mut self) -> Result<()> {
        self.verify_published()?;
        let part = &mut self.audio.part;
        let stamp = platform::stamp(&part.file)?;
        part.file.rewind()?;
        let mut digest = Sha256::new();
        let mut bytes = 0u64;
        let mut buffer = [0u8; COPY_BUFFER_BYTES];
        loop {
            part.plan.grant.verify()?;
            ensure!(
                platform::stamp(&part.file)? == stamp,
                "Published download changed during hashing"
            );
            let count = part.file.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            bytes = bytes
                .checked_add(count as u64)
                .context("Published download length overflow")?;
            ensure!(
                bytes <= self.audio.checkpoint.bytes,
                "Published download exceeded its expected length"
            );
            digest.update(&buffer[..count]);
        }
        ensure!(
            bytes == self.audio.checkpoint.bytes
                && format!("{:x}", digest.finalize()) == self.audio.checkpoint.sha256
                && platform::stamp(&part.file)? == stamp,
            "Published download complete body differs"
        );
        self.published_stamp = Some(stamp);
        self.verify_published()
    }

    fn facts(&self) -> CommittedAudio {
        let part = &self.audio.part;
        CommittedAudio {
            path: part.plan.path(),
            identity: part.record.identity.clone(),
            checkpoint: self.audio.checkpoint.clone(),
            verification: self.audio.verification.clone(),
            retained_part: cfg!(unix).then(|| part.record.clone()),
        }
    }
}

impl VerifiedOwnedAudio {
    pub(crate) fn commit_owned(
        self,
    ) -> std::result::Result<PublishedDownloadAudio, PublicationFailure> {
        self.commit_owned_guarded(&|| Ok(()))
    }

    fn commit_owned_guarded(
        self,
        after_native_publication: &dyn Fn() -> Result<()>,
    ) -> std::result::Result<PublishedDownloadAudio, PublicationFailure> {
        let mut owner = PublicationOwner {
            audio: self,
            phase: DownloadPublicationPhase::OwnedPartial,
            published_stamp: None,
        };
        let result = (|| -> Result<()> {
            let audio = &mut owner.audio;
            audio.cancel.check()?;
            audio.execution_lease.check(&audio.context)?;
            audio.part.check_execution()?;
            audio.part.sync()?;
            audio.part.verify()?;
            let (bytes, sha256) = audio.part.hash(&audio.cancel)?;
            ensure!(
                bytes == audio.checkpoint.bytes && sha256 == audio.checkpoint.sha256,
                "Verified audio changed before native publication"
            );
            audio.cancel.check()?;
            audio.execution_lease.check(&audio.context)?;
            audio.part.check_execution()?;
            // rename_no_replace also flushes after the rename. Its error can
            // therefore follow a successful namespace mutation.
            owner.phase = DownloadPublicationPhase::PublicationAttempted;
            platform::rename_no_replace(
                &audio.part.file,
                audio.part.plan.grant.parent(),
                &audio.part.record.basename,
                &audio.part.plan.basename,
                &audio.part.record.identity,
            )?;
            after_native_publication()?;
            audio.cancel.check()?;
            audio.execution_lease.check(&audio.context)?;
            audio.part.check_execution()?;
            owner.verify_published_body()?;
            owner.phase = DownloadPublicationPhase::Published;
            Ok(())
        })();
        match result {
            Ok(()) => Ok(PublishedDownloadAudio { owner }),
            Err(cause) => Err(PublicationFailure {
                cause,
                owner: Box::new(owner),
            }),
        }
    }
}

impl PublishedDownloadAudio {
    pub(crate) fn facts(&self) -> CommittedAudio {
        self.owner.facts()
    }
    pub(crate) fn record(&self) -> &OwnedPartRecord {
        self.owner.audio.part.record()
    }
    pub(crate) fn grant(&self) -> &DestinationGrant {
        &self.owner.audio.part.plan.grant
    }
    pub(crate) fn into_retirement(self) -> PublishedAudioRetirement {
        self.owner.into_handle_retirement()
    }

    /// The controller checks the committed task and artifact rows while the
    /// original file is still retained. Failed ACK leaves this owner reusable.
    pub(crate) fn acknowledge_repository_commit(
        &mut self,
        acknowledge: impl FnOnce(&CommittedAudio) -> Result<()>,
    ) -> Result<()> {
        self.owner.verify_published()?;
        acknowledge(&self.owner.facts())?;
        self.owner.verify_published()?;
        self.owner.phase = DownloadPublicationPhase::RepositoryAcknowledged;
        if let Some(quota) = &self.owner.audio.part.quota {
            quota.remove(&self.owner.audio.part.record.identity);
        }
        Ok(())
    }

    pub(crate) fn into_read_anchor(
        self,
    ) -> std::result::Result<PublishedAudioReadAnchor, PublicationFailure> {
        let owner = self.owner;
        let result = (|| -> Result<File> {
            ensure!(
                owner.phase == DownloadPublicationPhase::RepositoryAcknowledged,
                "Completed download has not received its actual repository ACK"
            );
            owner.verify_published()?;
            duplicate_readonly(&owner.audio.part.file)
        })();
        match result {
            Ok(reader) => Ok(PublishedAudioReadAnchor { owner, reader }),
            Err(cause) => Err(PublicationFailure {
                cause,
                owner: Box::new(owner),
            }),
        }
    }
}

/// The read grant holds native ownership, never an Arc back to FileAuthorization.
/// Library permits retain this anchor and clone only its restricted reader.
pub(crate) struct PublishedAudioReadAnchor {
    owner: PublicationOwner,
    reader: File,
}
impl PublishedAudioReadAnchor {
    pub(crate) fn verify(&self) -> Result<()> {
        self.owner.verify_published()?;
        ensure!(
            platform::identity(&self.reader)? == self.owner.audio.part.record.identity
                && platform::stamp(&self.reader)?
                    == *self
                        .owner
                        .published_stamp
                        .as_ref()
                        .context("Missing published stamp")?,
            "Download reader differs from its original actual object"
        );
        Ok(())
    }
    pub(crate) fn facts(&self) -> CommittedAudio {
        self.owner.facts()
    }
    pub(crate) fn try_clone_reader(&self) -> Result<File> {
        self.verify()?;
        duplicate_readonly(&self.reader)
    }
    pub(crate) fn into_retirement(self) -> PublishedAudioRetirement {
        let part = self.owner.audio.part;
        PublishedAudioRetirement {
            files: vec![part.file, self.reader],
            grant: Some(part.plan.grant),
        }
    }
}

/// Owned by the Library retirement worker until every actual close succeeds.
/// A failed close retains that exact native handle and all destination pins.
pub(crate) struct PublishedAudioRetirement {
    files: Vec<File>,
    grant: Option<DestinationGrant>,
}
impl PublishedAudioRetirement {
    pub(crate) fn try_close(&mut self) -> Result<()> {
        while let Some(file) = self.files.pop() {
            if let Err((file, error)) = close_actual(file) {
                self.files.push(file);
                return Err(error.into());
            }
        }
        if let Some(grant) = &mut self.grant {
            close_unique_destination(grant)?;
        }
        self.grant.take();
        Ok(())
    }
    pub(crate) fn is_closed(&self) -> bool {
        self.files.is_empty() && self.grant.is_none()
    }
}

fn close_unique_destination(grant: &mut DestinationGrant) -> Result<()> {
    // Shared destination grants remain owned by their other live users. The
    // retirement worker retries after those users surrender their actual pins.
    let inner = Arc::get_mut(&mut grant.0)
        .context("Download destination still has another native owner")?;
    while let Some(file) = inner.pins.pop() {
        if let Err((file, error)) = close_actual(file) {
            inner.pins.push(file);
            return Err(error.into());
        }
        inner.identities.pop();
    }
    if let Some(parent) = &mut inner.parent {
        close_unique_destination(parent)?;
    }
    inner.parent.take();
    Ok(())
}

#[cfg(windows)]
fn duplicate_readonly(file: &File) -> Result<File> {
    use std::os::windows::io::{AsRawHandle, FromRawHandle};
    use windows_sys::Win32::{
        Foundation::{DuplicateHandle, INVALID_HANDLE_VALUE},
        Storage::FileSystem::FILE_GENERIC_READ,
        System::Threading::GetCurrentProcess,
    };
    let mut duplicate = std::ptr::null_mut();
    let process = unsafe { GetCurrentProcess() };
    // DuplicateHandle preserves the actual FILE_OBJECT and its original share
    // fence; zero options restrict access and never closes the source handle.
    ensure!(
        unsafe {
            DuplicateHandle(
                process,
                file.as_raw_handle(),
                process,
                &mut duplicate,
                FILE_GENERIC_READ,
                0,
                0,
            )
        } != 0,
        "Cannot duplicate published download as read-only: {}",
        io::Error::last_os_error()
    );
    ensure!(
        !duplicate.is_null() && duplicate != INVALID_HANDLE_VALUE,
        "Invalid published download duplicate"
    );
    let reader = unsafe { File::from_raw_handle(duplicate) };
    ensure!(
        platform::identity(&reader)? == platform::identity(file)?,
        "Restricted download duplicate changed actual identity"
    );
    Ok(reader)
}
#[cfg(not(windows))]
fn duplicate_readonly(_: &File) -> Result<File> {
    bail!(
        "UnsupportedCapability: same-object read-only download adoption requires native Windows descriptors"
    )
}

#[cfg(windows)]
fn close_actual(file: File) -> std::result::Result<(), (File, io::Error)> {
    use std::os::windows::io::{FromRawHandle, IntoRawHandle};
    let raw = file.into_raw_handle();
    if unsafe { windows_sys::Win32::Foundation::CloseHandle(raw) } == 0 {
        let error = io::Error::last_os_error();
        return Err((unsafe { File::from_raw_handle(raw) }, error));
    }
    Ok(())
}
#[cfg(not(windows))]
fn close_actual(file: File) -> std::result::Result<(), (File, io::Error)> {
    Err((
        file,
        io::Error::new(
            io::ErrorKind::Unsupported,
            "Actual download close acknowledgement requires Windows",
        ),
    ))
}

#[cfg(all(test, windows))]
#[path = "committed_owner_tests.rs"]
mod tests;

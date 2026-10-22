//! Explicit-directory, bounded no-replace backup publication.
//! Plans and receipts are metadata; only the live nonserializable directory and
//! part owners provide write authority. Windows pins namespace ancestors without
//! delete sharing. Linux uses anonymous O_TMPFILE storage and an FD-based link,
//! and checks descriptor/path relationships without claiming Windows share locks.

use anyhow::{Context, Result, bail, ensure};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{
    fs::File,
    io::{self, Read, Seek, SeekFrom, Write},
    path::{Component, Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

pub const MAX_EXPORT_BYTES: u64 = 16 * 1024 * 1024;
const CHUNK: usize = 64 * 1024;
const MAX_ACTIVE: usize = 4;

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ExportFileIdentity {
    pub device: u64,
    pub object: String,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExportErrorKind {
    Cancelled,
    Deadline,
    Quota,
    Conflict,
    Unsupported,
    Changed,
    Io,
    Producer,
    PublishedUnconfirmed,
}
#[derive(Debug)]
struct ExportControlError(ExportErrorKind);
impl std::fmt::Display for ExportControlError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Export operation: {:?}", self.0)
    }
}
impl std::error::Error for ExportControlError {}

fn require_unchanged(condition: bool, reason: &'static str) -> Result<()> {
    if condition {
        Ok(())
    } else {
        Err(ExportControlError(ExportErrorKind::Changed)).context(reason)
    }
}

#[derive(Debug)]
struct ExportProducerError(anyhow::Error);
impl std::fmt::Display for ExportProducerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Export producer failed: {}", self.0)
    }
}
impl std::error::Error for ExportProducerError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.0.as_ref())
    }
}

#[derive(Clone)]
pub struct ExportContext {
    cancel: Arc<AtomicBool>,
    deadline: Instant,
}
impl ExportContext {
    pub fn new(timeout: Duration) -> Result<Self> {
        ensure!(
            !timeout.is_zero() && timeout <= Duration::from_secs(120),
            "Export timeout must be positive and at most 120 seconds"
        );
        Ok(Self {
            cancel: Arc::new(AtomicBool::new(false)),
            deadline: Instant::now() + timeout,
        })
    }
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Release);
    }
    pub(crate) fn check(&self) -> Result<()> {
        if self.cancel.load(Ordering::Acquire) {
            return Err(ExportControlError(ExportErrorKind::Cancelled).into());
        }
        if Instant::now() >= self.deadline {
            return Err(ExportControlError(ExportErrorKind::Deadline).into());
        }
        Ok(())
    }
}
#[derive(Default)]
struct FlightState {
    active: usize,
    reserved: u64,
}
#[derive(Clone, Default)]
pub struct ExportLimiter(Arc<Mutex<FlightState>>);
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExportFlightStats {
    pub active: usize,
    pub reserved_bytes: u64,
}
impl ExportLimiter {
    pub fn stats(&self) -> ExportFlightStats {
        let state = self.0.lock().unwrap_or_else(|error| error.into_inner());
        ExportFlightStats {
            active: state.active,
            reserved_bytes: state.reserved,
        }
    }
    fn reserve(&self, bytes: u64) -> Result<ExportPermit> {
        if bytes > MAX_EXPORT_BYTES {
            return Err(ExportControlError(ExportErrorKind::Quota).into());
        }
        let mut state = self.0.lock().unwrap_or_else(|error| error.into_inner());
        if state.active >= MAX_ACTIVE || bytes > MAX_EXPORT_BYTES.saturating_sub(state.reserved) {
            return Err(ExportControlError(ExportErrorKind::Quota).into());
        }
        state.active += 1;
        state.reserved += bytes;
        Ok(ExportPermit {
            limiter: self.clone(),
            bytes,
        })
    }
}
struct ExportPermit {
    limiter: ExportLimiter,
    bytes: u64,
}
impl Drop for ExportPermit {
    fn drop(&mut self) {
        let mut state = self
            .limiter
            .0
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        state.active -= 1;
        state.reserved -= self.bytes;
    }
}

struct DirectoryInner {
    path: PathBuf,
    pins: Vec<File>,
    identities: Vec<ExportFileIdentity>,
    epoch: u64,
    revoked: AtomicBool,
}
#[derive(Clone)]
pub struct ExportDirectory(Arc<DirectoryInner>);
impl ExportDirectory {
    /// Represents a new explicit write selection, independent of library READ
    /// grants and uploaded backup paths. It does not create missing directories.
    pub fn open_selected(path: impl AsRef<Path>, epoch: u64) -> Result<Self> {
        let path = path.as_ref();
        ensure!(
            epoch > 0
                && path.is_absolute()
                && path.components().count() <= 64
                && path.as_os_str().to_string_lossy().encode_utf16().count() <= 32000
                && path.components().all(|component| !matches!(
                    component,
                    Component::CurDir | Component::ParentDir
                )),
            "Invalid selected export directory"
        );
        let (path, pins) = platform::pin_directory(path)?;
        let identities = pins
            .iter()
            .map(platform::identity)
            .collect::<Result<Vec<_>>>()?;
        let grant = Self(Arc::new(DirectoryInner {
            path,
            pins,
            identities,
            epoch,
            revoked: AtomicBool::new(false),
        }));
        grant.verify()?;
        Ok(grant)
    }
    pub fn path(&self) -> &Path {
        &self.0.path
    }
    pub fn identity(&self) -> &ExportFileIdentity {
        self.0.identities.last().expect("Selected directory")
    }
    pub fn epoch(&self) -> u64 {
        self.0.epoch
    }
    pub fn revoke(&self) {
        self.0.revoked.store(true, Ordering::Release);
    }
    pub fn plan(&self, basename: &str) -> Result<ExportPlan> {
        validate_basename(basename)?;
        self.verify()?;
        Ok(ExportPlan {
            directory: self.clone(),
            basename: basename.into(),
        })
    }
    fn parent(&self) -> &File {
        self.0.pins.last().expect("Selected directory")
    }
    fn verify(&self) -> Result<()> {
        require_unchanged(
            !self.0.revoked.load(Ordering::Acquire),
            "Export directory selection was revoked",
        )?;
        for (file, expected) in self.0.pins.iter().zip(&self.0.identities) {
            platform::validate(file, true)?;
            require_unchanged(
                platform::identity(file)? == *expected,
                "Selected export directory object changed",
            )?;
        }
        platform::verify_directory(&self.0.path, &self.0.pins)?;
        Ok(())
    }
}
pub struct ExportPlan {
    directory: ExportDirectory,
    basename: String,
}
#[derive(Clone, Debug, Serialize)]
pub struct ExportPlanMetadata {
    pub path: PathBuf,
    pub directory_identity: ExportFileIdentity,
    pub selection_epoch: u64,
}
impl ExportPlan {
    pub fn metadata(&self) -> ExportPlanMetadata {
        ExportPlanMetadata {
            path: self.directory.path().join(&self.basename),
            directory_identity: self.directory.identity().clone(),
            selection_epoch: self.directory.epoch(),
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct ExportReceipt {
    pub operation_id: String,
    pub path: PathBuf,
    pub identity: ExportFileIdentity,
    pub bytes: u64,
    pub sha256: String,
    pub directory_identity: ExportFileIdentity,
    pub selection_epoch: u64,
}
#[derive(Clone, Debug, Serialize)]
pub struct ExportPartMetadata {
    pub operation_id: String,
    pub nonce: String,
    pub temporary_name: Option<String>,
    pub directory: PathBuf,
    pub directory_identity: ExportFileIdentity,
    pub selection_epoch: u64,
    /// Absent only when initial inspection of the newly owned handle failed.
    pub identity: Option<ExportFileIdentity>,
    pub bytes: u64,
    pub published: bool,
    pub final_basename: String,
    /// Diagnostic recovery facts only; no native cleanup authority can be
    /// reconstructed from this record or its identity.
    pub cleanup_probe: Option<ExportCalibrationMetadata>,
}
#[derive(Clone, Debug, Serialize)]
pub struct ExportCalibrationMetadata {
    pub temporary_name: String,
    pub identity: Option<ExportFileIdentity>,
    pub owner_handle_open: bool,
    pub reader_handle_open: bool,
    pub disposition_requested: bool,
    pub zero_link_witness: bool,
}
#[derive(Clone, Debug, PartialEq, Eq)]
struct Stamp {
    changed: i128,
    written: i128,
    length: u64,
}
/// Owns the actual private or published object and its reservation. A controller
/// must retain failures until cleanup or publication verification finishes.
/// Dropping a Windows private owner closes its handle but does not remove its
/// recovery leaf; this module never deletes a pathname in Drop.
pub struct ExportPart {
    file: Option<File>,
    plan: ExportPlan,
    metadata: ExportPartMetadata,
    stamp: Option<Stamp>,
    maximum: u64,
    verified_sha: Option<String>,
    deletion_requested: bool,
    write_error: Option<anyhow::Error>,
    #[cfg(windows)]
    cleanup_state: platform::CleanupState,
    _permit: ExportPermit,
}
pub struct ExportFailure {
    pub kind: ExportErrorKind,
    pub message: String,
    pub part: Option<Box<ExportPart>>,
}
impl std::fmt::Debug for ExportFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExportFailure")
            .field("kind", &self.kind)
            .field("message", &self.message)
            .field("has_owned_part", &self.part.is_some())
            .finish()
    }
}
impl std::fmt::Display for ExportFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}
impl std::error::Error for ExportFailure {}
impl ExportFailure {
    fn new(error: anyhow::Error, part: Option<ExportPart>) -> Self {
        let published = part.as_ref().is_some_and(|part| part.metadata.published);
        let kind = if published {
            ExportErrorKind::PublishedUnconfirmed
        } else if let Some(error) = error
            .chain()
            .find_map(|error| error.downcast_ref::<ExportControlError>())
        {
            error.0
        } else if error.chain().any(|error| {
            error
                .downcast_ref::<io::Error>()
                .is_some_and(|error| error.kind() == io::ErrorKind::AlreadyExists)
        }) {
            ExportErrorKind::Conflict
        } else if error.to_string().contains("UnsupportedCapability") {
            ExportErrorKind::Unsupported
        } else if error.downcast_ref::<ExportProducerError>().is_some() {
            ExportErrorKind::Producer
        } else {
            ExportErrorKind::Io
        };
        Self {
            kind,
            message: format!("{error:#}").chars().take(2048).collect(),
            part: part.map(Box::new),
        }
    }
}

impl ExportPart {
    pub fn metadata(&self) -> ExportPartMetadata {
        #[cfg(windows)]
        {
            ExportPartMetadata {
                cleanup_probe: self.cleanup_state.probe_metadata(),
                ..self.metadata.clone()
            }
        }
        #[cfg(not(windows))]
        {
            self.metadata.clone()
        }
    }
    fn file(&self) -> &File {
        self.file.as_ref().expect("Live export part")
    }
    fn initialize(&mut self, context: &ExportContext) -> Result<()> {
        context.check()?;
        self.plan.directory.verify()?;
        platform::validate(self.file(), false)?;
        if self.metadata.identity.is_none() {
            self.metadata.identity = Some(platform::identity(self.file())?);
        }
        if self.stamp.is_none() {
            self.stamp = Some(platform::stamp(self.file())?);
        }
        self.verify(context)
    }
    fn identity(&self) -> Result<&ExportFileIdentity> {
        self.metadata
            .identity
            .as_ref()
            .context("Export object identity is not yet verified")
    }
    fn verify_object(&self, context: &ExportContext) -> Result<()> {
        self.verify_object_in(&self.plan.directory, context)
    }
    fn verify_object_in(&self, directory: &ExportDirectory, context: &ExportContext) -> Result<()> {
        context.check()?;
        directory.verify()?;
        require_unchanged(
            *directory.identity() == self.metadata.directory_identity,
            "Cleanup selection does not identify the original export directory",
        )?;
        ensure!(
            self.file.is_some() && !self.deletion_requested,
            "Export part is awaiting deletion confirmation"
        );
        platform::validate(self.file(), false)?;
        require_unchanged(
            platform::identity(self.file())? == *self.identity()?,
            "Export part identity changed",
        )?;
        let expected_links = if self.metadata.temporary_name.is_none() && !self.metadata.published {
            0
        } else {
            1
        };
        require_unchanged(
            platform::links(self.file())? == expected_links,
            "Export part has an unexpected hard link",
        )?;
        if self.metadata.published {
            platform::verify_name(directory.parent(), &self.plan.basename, self.identity()?)?;
        } else if let Some(name) = &self.metadata.temporary_name {
            platform::verify_name(directory.parent(), name, self.identity()?)?;
        }
        context.check()?;
        Ok(())
    }
    fn verify(&self, context: &ExportContext) -> Result<()> {
        self.verify_object(context)?;
        require_unchanged(
            Some(platform::stamp(self.file())?) == self.stamp,
            "Export part representation changed",
        )?;
        context.check()
    }
    fn hash(&mut self, context: &ExportContext) -> Result<String> {
        self.verify(context)?;
        self.file.as_mut().unwrap().seek(SeekFrom::Start(0))?;
        let size = self.metadata.bytes;
        let mut read_bytes = 0;
        let mut buffer = [0u8; CHUNK];
        let mut digest = Sha256::new();
        while read_bytes < size {
            self.verify(context)?;
            let wanted = (size - read_bytes).min(CHUNK as u64) as usize;
            let read = self.file.as_mut().unwrap().read(&mut buffer[..wanted])?;
            ensure!(read > 0, "Export part was truncated during verification");
            digest.update(&buffer[..read]);
            read_bytes += read as u64;
            self.verify(context)?;
        }
        ensure!(
            self.file().metadata()?.len() == size,
            "Export part grew during verification"
        );
        self.verify(context)?;
        Ok(format!("{:x}", digest.finalize()))
    }
    fn publish(&mut self, context: &ExportContext) -> Result<ExportReceipt> {
        self.verify(context)?;
        self.file().sync_all()?;
        let digest = self.hash(context)?;
        self.verified_sha = Some(digest.clone());
        self.verify(context)?;
        platform::publish(
            self.file(),
            self.plan.directory.parent(),
            &self.plan.basename,
            self.identity()?,
        )?;
        self.metadata.published = true;
        self.metadata.temporary_name = None;
        self.stamp = Some(platform::stamp(self.file())?);
        self.file().sync_all()?;
        platform::sync_directory(self.plan.directory.parent())?;
        self.verify(context)?;
        ensure!(
            self.hash(context)? == digest,
            "Published export whole digest changed"
        );
        Ok(self.receipt(digest))
    }
    fn receipt(&self, sha256: String) -> ExportReceipt {
        ExportReceipt {
            operation_id: self.metadata.operation_id.clone(),
            path: self.plan.directory.path().join(&self.plan.basename),
            identity: self
                .metadata
                .identity
                .clone()
                .expect("Verified export identity"),
            bytes: self.metadata.bytes,
            sha256,
            directory_identity: self.plan.directory.identity().clone(),
            selection_epoch: self.plan.directory.epoch(),
        }
    }
    /// Retries only post-publication verification. It never renames again or
    /// deletes an already published final file, including after cancellation.
    pub fn verify_published(
        mut self,
        context: &ExportContext,
    ) -> std::result::Result<ExportReceipt, ExportFailure> {
        let result = (|| -> Result<ExportReceipt> {
            ensure!(self.metadata.published, "Export part was not published");
            let expected = self
                .verified_sha
                .clone()
                .context("Published export digest is missing")?;
            self.verify_object(context)?;
            // Publication changes the namespace stamp. A failed stamp query
            // after the successful syscall is recoverable only through this
            // original handle, final-name proof, and the sealed whole digest.
            self.stamp = Some(platform::stamp(self.file())?);
            self.file().sync_all()?;
            platform::sync_directory(self.plan.directory.parent())?;
            ensure!(
                self.hash(context)? == expected,
                "Published export digest changed"
            );
            Ok(self.receipt(expected))
        })();
        match result {
            Ok(receipt) => Ok(receipt),
            Err(error) => Err(ExportFailure::new(error, Some(self))),
        }
    }
    /// Deletes only this unpublished private object through its actual handle
    /// (Windows), or closes the anonymous inode (Linux). No foreign leaf is used.
    pub fn cleanup(
        self,
        context: &ExportContext,
    ) -> std::result::Result<ExportPartMetadata, ExportFailure> {
        let directory = self.plan.directory.clone();
        self.cleanup_in(&directory, context)
    }
    /// Uses a fresh explicit selection for cleanup only. Its actual root object
    /// must match the original selection; it cannot re-arm the original grant,
    /// change an allocation epoch, or authorize publication.
    pub fn cleanup_in(
        mut self,
        directory: &ExportDirectory,
        context: &ExportContext,
    ) -> std::result::Result<ExportPartMetadata, ExportFailure> {
        let result = (|| -> Result<ExportPartMetadata> {
            ensure!(
                !self.metadata.published,
                "Published final export cannot be cleaned as a temporary part"
            );
            context.check()?;
            directory.verify()?;
            require_unchanged(
                *directory.identity() == self.metadata.directory_identity,
                "Cleanup selection does not identify the original export directory",
            )?;
            if !self.deletion_requested {
                // Cleanup requires object and private-name proof, rather than
                // a stale body stamp left by an interrupted write.
                if self.metadata.identity.is_none() {
                    self.metadata.identity = Some(platform::identity(self.file())?);
                }
                #[cfg(windows)]
                {
                    if !self.cleanup_state.disposition_started() {
                        self.verify_object_in(directory, context)?;
                        self.prepare_cleanup_hash(directory, context)?;
                    }
                    let expected = self.identity()?.clone();
                    let file = self
                        .file
                        .as_ref()
                        .context("Cleanup file handle is missing")?;
                    platform::prepare_cleanup(
                        &mut self.cleanup_state,
                        file,
                        &expected,
                        directory,
                        context,
                        self.stamp.as_ref().context("Cleanup stamp is missing")?,
                        self.verified_sha
                            .as_deref()
                            .context("Cleanup digest is missing")?,
                    )?;
                    platform::request_cleanup_disposition(
                        &mut self.cleanup_state,
                        file,
                        &expected,
                        directory,
                        context,
                    )?;
                }
                #[cfg(not(windows))]
                {
                    self.verify_object_in(directory, context)?;
                    platform::delete_private(self.file())?;
                }
                self.deletion_requested = true;
                drop(self.file.take());
            }
            #[cfg(windows)]
            platform::confirm_cleanup(&self.cleanup_state, self.identity()?, directory, context)?;
            #[cfg(not(windows))]
            if let Some(name) = &self.metadata.temporary_name {
                platform::verify_absent(directory.parent(), name, self.identity()?)?;
            }
            context.check()?;
            Ok(self.metadata.clone())
        })();
        match result {
            Ok(metadata) => Ok(metadata),
            Err(error) => Err(ExportFailure::new(error, Some(self))),
        }
    }
    #[cfg(windows)]
    fn prepare_cleanup_hash(
        &mut self,
        directory: &ExportDirectory,
        context: &ExportContext,
    ) -> Result<()> {
        self.verify_object_in(directory, context)?;
        let stamp = platform::stamp(self.file())?;
        ensure!(
            stamp.length <= self.maximum,
            "Cleanup object exceeds its original reservation"
        );
        self.file.as_mut().unwrap().seek(SeekFrom::Start(0))?;
        let mut digest = Sha256::new();
        let mut read_bytes = 0;
        let mut buffer = [0u8; CHUNK];
        while read_bytes < stamp.length {
            self.verify_object_in(directory, context)?;
            require_unchanged(
                platform::stamp(self.file())? == stamp,
                "Cleanup body changed before read",
            )?;
            let length = (stamp.length - read_bytes).min(CHUNK as u64) as usize;
            let read = self.file.as_mut().unwrap().read(&mut buffer[..length])?;
            ensure!(read > 0, "Cleanup body ended before its captured length");
            digest.update(&buffer[..read]);
            read_bytes += read as u64;
            self.verify_object_in(directory, context)?;
            require_unchanged(
                platform::stamp(self.file())? == stamp,
                "Cleanup body changed during read",
            )?;
        }
        self.verify_object_in(directory, context)?;
        require_unchanged(
            platform::stamp(self.file())? == stamp,
            "Cleanup body changed after read",
        )?;
        self.metadata.bytes = stamp.length;
        self.stamp = Some(stamp);
        self.verified_sha = Some(format!("{:x}", digest.finalize()));
        Ok(())
    }
}
pub struct CheckedExportWriter<'a> {
    part: &'a mut ExportPart,
    context: &'a ExportContext,
}
impl Write for CheckedExportWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let result = (|| -> Result<usize> {
            self.part.verify(self.context)?;
            let length = bytes.len().min(CHUNK);
            if length as u64 > self.part.maximum.saturating_sub(self.part.metadata.bytes) {
                return Err(ExportControlError(ExportErrorKind::Quota).into());
            }
            let file = self.part.file.as_mut().unwrap();
            file.seek(SeekFrom::End(0))?;
            let written = file.write(&bytes[..length])?;
            self.part.metadata.bytes += written as u64;
            self.part.stamp = Some(platform::stamp(self.part.file())?);
            self.part.verify(self.context)?;
            Ok(written)
        })();
        match result {
            Ok(written) => Ok(written),
            Err(error) => {
                let returned = export_io_error(&error);
                if self.part.write_error.is_none() {
                    self.part.write_error = Some(error);
                }
                Err(returned)
            }
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        let result = self.part.verify(self.context).and_then(|_| {
            self.part.file().sync_all()?;
            Ok(())
        });
        match result {
            Ok(()) => Ok(()),
            Err(error) => {
                let returned = export_io_error(&error);
                if self.part.write_error.is_none() {
                    self.part.write_error = Some(error);
                }
                Err(returned)
            }
        }
    }
}

fn export_io_error(error: &anyhow::Error) -> io::Error {
    if let Some(control) = error
        .chain()
        .find_map(|error| error.downcast_ref::<ExportControlError>())
    {
        io::Error::other(ExportControlError(control.0))
    } else {
        io::Error::other(error.to_string())
    }
}

pub fn export_bytes(
    plan: ExportPlan,
    operation_id: &str,
    bytes: &[u8],
    context: &ExportContext,
    limiter: &ExportLimiter,
) -> std::result::Result<ExportReceipt, ExportFailure> {
    export_with(
        plan,
        operation_id,
        bytes.len() as u64,
        context,
        limiter,
        |writer| {
            writer.write_all(bytes)?;
            Ok(())
        },
    )
}
pub fn export_with(
    plan: ExportPlan,
    operation_id: &str,
    maximum_bytes: u64,
    context: &ExportContext,
    limiter: &ExportLimiter,
    producer: impl FnOnce(&mut CheckedExportWriter<'_>) -> Result<()>,
) -> std::result::Result<ExportReceipt, ExportFailure> {
    let created = (|| -> Result<ExportPart> {
        ensure!(
            !operation_id.is_empty()
                && operation_id.len() <= 256
                && !operation_id.chars().any(char::is_control),
            "Invalid export operation identity"
        );
        context.check()?;
        plan.directory.verify()?;
        let permit = limiter.reserve(maximum_bytes)?;
        let mut nonce = [0u8; 16];
        getrandom::fill(&mut nonce)
            .map_err(|error| anyhow::anyhow!("Cannot create export nonce: {error}"))?;
        let nonce = nonce
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let temporary_name = if cfg!(windows) {
            Some(format!(".lx-export-{nonce}.part"))
        } else {
            None
        };
        let file = platform::create_private(plan.directory.parent(), temporary_name.as_deref())?;
        let metadata = ExportPartMetadata {
            operation_id: operation_id.into(),
            nonce,
            temporary_name,
            directory: plan.directory.path().to_path_buf(),
            directory_identity: plan.directory.identity().clone(),
            selection_epoch: plan.directory.epoch(),
            identity: None,
            bytes: 0,
            published: false,
            final_basename: plan.basename.clone(),
            cleanup_probe: None,
        };
        Ok(ExportPart {
            file: Some(file),
            plan,
            metadata,
            stamp: None,
            maximum: maximum_bytes,
            verified_sha: None,
            deletion_requested: false,
            write_error: None,
            #[cfg(windows)]
            cleanup_state: platform::CleanupState::default(),
            _permit: permit,
        })
    })();
    let mut part = match created {
        Ok(part) => part,
        Err(error) => return Err(ExportFailure::new(error, None)),
    };
    let result = (|| -> Result<ExportReceipt> {
        part.initialize(context)?;
        let produced = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            producer(&mut CheckedExportWriter {
                part: &mut part,
                context,
            })
        }));
        // A producer cannot turn an ignored I/O failure into a successful,
        // truncated backup by swallowing the writer's returned error.
        if let Some(error) = part.write_error.take() {
            return Err(error);
        }
        match produced {
            Ok(result) => result.map_err(ExportProducerError)?,
            Err(_) => return Err(ExportControlError(ExportErrorKind::Producer).into()),
        }
        part.publish(context)
    })();
    match result {
        Ok(receipt) => Ok(receipt),
        Err(error) => Err(ExportFailure::new(error, Some(part))),
    }
}
fn validate_basename(name: &str) -> Result<()> {
    ensure!(
        !name.is_empty()
            && name.encode_utf16().count() <= 240
            && !name.chars().any(char::is_control)
            && !name.contains(['\\', '/', ':', '*', '?', '"', '<', '>', '|'])
            && !name.ends_with(['.', ' '])
            && !matches!(name, "." | ".."),
        "Invalid export filename"
    );
    let stem = name.split('.').next().unwrap().to_ascii_uppercase();
    ensure!(
        !matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
            && !(stem.len() == 4
                && (stem.starts_with("COM") || stem.starts_with("LPT"))
                && stem.as_bytes()[3].is_ascii_digit()),
        "Reserved export filename"
    );
    Ok(())
}

#[cfg(windows)]
mod platform {
    use super::*;
    use std::os::windows::{
        ffi::OsStrExt,
        io::{AsRawHandle, FromRawHandle},
    };
    use windows_sys::{
        Wdk::{
            Foundation::OBJECT_ATTRIBUTES,
            Storage::FileSystem::{
                FILE_CREATE, FILE_DIRECTORY_FILE, FILE_NON_DIRECTORY_FILE, FILE_OPEN,
                FILE_OPEN_REPARSE_POINT, FILE_SYNCHRONOUS_IO_NONALERT, FileRenameInformation,
                NtCreateFile, NtSetInformationFile,
            },
        },
        Win32::{
            Foundation::{
                GENERIC_READ, GENERIC_WRITE, INVALID_HANDLE_VALUE, OBJ_CASE_INSENSITIVE,
                RtlNtStatusToDosError, UNICODE_STRING,
            },
            Storage::FileSystem::*,
            System::IO::IO_STATUS_BLOCK,
        },
    };
    const STATUS_DELETE_PENDING: i32 = 0xc0000056u32 as i32;
    const STATUS_INVALID_PARAMETER: i32 = 0xc000000du32 as i32;

    #[derive(Default)]
    pub(super) struct CleanupState {
        probe: Option<CalibrationProbe>,
        calibration: Option<CleanupCalibration>,
        prepared: Option<CleanupPrepared>,
        disposition_requested: bool,
        zero_link_witness: bool,
    }
    impl CleanupState {
        pub(super) fn disposition_started(&self) -> bool {
            self.disposition_requested
        }
        pub(super) fn probe_metadata(&self) -> Option<ExportCalibrationMetadata> {
            self.probe.as_ref().map(|probe| ExportCalibrationMetadata {
                temporary_name: probe.name.clone(),
                identity: probe.identity.clone(),
                owner_handle_open: probe.file.is_some(),
                reader_handle_open: probe.reader.is_some(),
                disposition_requested: probe.disposition_requested,
                zero_link_witness: probe.zero_link_witness,
            })
        }
    }
    struct CalibrationProbe {
        file: Option<File>,
        reader: Option<File>,
        name: String,
        identity: Option<ExportFileIdentity>,
        positive_control: bool,
        disposition_requested: bool,
        zero_link_witness: bool,
    }
    struct CleanupCalibration {
        volume: String,
        directory: ExportFileIdentity,
    }
    struct CleanupPrepared {
        identity: ExportFileIdentity,
        stamp: Stamp,
        sha256: String,
    }
    fn normalize(path: &Path) -> PathBuf {
        crate::domain::music::normalize_local_path(path)
    }
    pub(super) fn pin_directory(path: &Path) -> Result<(PathBuf, Vec<File>)> {
        let mut components = path.components();
        let prefix = components.next().context("Export volume prefix missing")?;
        ensure!(
            matches!(prefix, Component::Prefix(_))
                && matches!(components.next(), Some(Component::RootDir)),
            "Export must use an absolute native path"
        );
        let mut current = PathBuf::from(prefix.as_os_str());
        current.push("\\");
        let name = current
            .as_os_str()
            .encode_wide()
            .chain(Some(0))
            .collect::<Vec<_>>();
        let handle = unsafe {
            CreateFileW(
                name.as_ptr(),
                FILE_READ_ATTRIBUTES | FILE_LIST_DIRECTORY,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                std::ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
                std::ptr::null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error().into());
        }
        let mut pins = vec![unsafe { File::from_raw_handle(handle) }];
        validate(&pins[0], true)?;
        for component in components {
            let Component::Normal(name) = component else {
                bail!("Invalid selected export component");
            };
            pins.push(open_relative(
                pins.last().unwrap(),
                name.to_str().context("Export path is not UTF-8")?,
                true,
                false,
                false,
            )?);
        }
        let actual = current_path(pins.last().unwrap())?;
        ensure!(
            normalize(&actual) == normalize(path),
            "Selected export namespace differs from its path"
        );
        Ok((normalize(&actual), pins))
    }
    fn open_relative(
        parent: &File,
        name: &str,
        directory: bool,
        create: bool,
        readonly: bool,
    ) -> Result<File> {
        open_relative_with_sharing(parent, name, directory, create, readonly, None)
    }
    fn open_relative_with_sharing(
        parent: &File,
        name: &str,
        directory: bool,
        create: bool,
        readonly: bool,
        share_override: Option<u32>,
    ) -> Result<File> {
        let mut text = name.encode_utf16().collect::<Vec<_>>();
        let name = UNICODE_STRING {
            Length: (text.len() * 2) as u16,
            MaximumLength: (text.len() * 2) as u16,
            Buffer: text.as_mut_ptr(),
        };
        let attributes = OBJECT_ATTRIBUTES {
            Length: std::mem::size_of::<OBJECT_ATTRIBUTES>() as u32,
            RootDirectory: parent.as_raw_handle(),
            ObjectName: &name,
            Attributes: OBJ_CASE_INSENSITIVE,
            SecurityDescriptor: std::ptr::null(),
            SecurityQualityOfService: std::ptr::null(),
        };
        let mut handle = std::ptr::null_mut();
        let mut status_block = IO_STATUS_BLOCK::default();
        let access = if directory {
            FILE_READ_ATTRIBUTES | FILE_LIST_DIRECTORY
        } else if readonly {
            FILE_READ_ATTRIBUTES
        } else {
            GENERIC_READ | GENERIC_WRITE | DELETE
        };
        let sharing = share_override.unwrap_or(if directory {
            FILE_SHARE_READ | FILE_SHARE_WRITE
        } else if readonly {
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE
        } else {
            FILE_SHARE_READ
        });
        let status = unsafe {
            NtCreateFile(
                &mut handle,
                access | SYNCHRONIZE,
                &attributes,
                &mut status_block,
                std::ptr::null(),
                0,
                sharing,
                if create { FILE_CREATE } else { FILE_OPEN },
                FILE_OPEN_REPARSE_POINT
                    | FILE_SYNCHRONOUS_IO_NONALERT
                    | if directory {
                        FILE_DIRECTORY_FILE
                    } else {
                        FILE_NON_DIRECTORY_FILE
                    },
                std::ptr::null(),
                0,
            )
        };
        if status < 0 {
            return Err(io::Error::from_raw_os_error(
                unsafe { RtlNtStatusToDosError(status) } as i32
            )
            .into());
        }
        ensure!(
            !handle.is_null() && handle != INVALID_HANDLE_VALUE,
            "Invalid export handle"
        );
        let file = unsafe { File::from_raw_handle(handle) };
        // A newly created handle must reach its retained owner before any
        // fallible inspection; otherwise an inspection failure strands a leaf.
        if !create {
            validate(&file, directory)?;
        }
        Ok(file)
    }
    pub(super) fn create_private(parent: &File, name: Option<&str>) -> Result<File> {
        open_relative(
            parent,
            name.context("Private export name is missing")?,
            false,
            true,
            false,
        )
    }
    pub(super) fn validate(file: &File, directory: bool) -> Result<()> {
        let mut info = FILE_ATTRIBUTE_TAG_INFO::default();
        ensure!(
            unsafe {
                GetFileInformationByHandleEx(
                    file.as_raw_handle(),
                    FileAttributeTagInfo,
                    &mut info as *mut _ as *mut _,
                    std::mem::size_of_val(&info) as u32,
                )
            } != 0,
            "Cannot inspect export object"
        );
        ensure!(
            info.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT == 0
                && (info.FileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0) == directory,
            "Export object type or reparse traversal refused"
        );
        if directory {
            let mut case = FILE_CASE_SENSITIVE_INFO::default();
            ensure!(
                unsafe {
                    GetFileInformationByHandleEx(
                        file.as_raw_handle(),
                        FileCaseSensitiveInfo,
                        &mut case as *mut _ as *mut _,
                        std::mem::size_of_val(&case) as u32,
                    )
                } != 0
                    && case.Flags & 1 == 0,
                "UnsupportedCapability: export directory case policy cannot be supported"
            );
        }
        Ok(())
    }
    pub(super) fn identity(file: &File) -> Result<ExportFileIdentity> {
        let mut info = FILE_ID_INFO::default();
        ensure!(
            unsafe {
                GetFileInformationByHandleEx(
                    file.as_raw_handle(),
                    FileIdInfo,
                    &mut info as *mut _ as *mut _,
                    std::mem::size_of_val(&info) as u32,
                )
            } != 0,
            "Cannot identify export object"
        );
        Ok(ExportFileIdentity {
            device: info.VolumeSerialNumber,
            object: info
                .FileId
                .Identifier
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect(),
        })
    }
    pub(super) fn stamp(file: &File) -> Result<Stamp> {
        let mut info = FILE_BASIC_INFO::default();
        ensure!(
            unsafe {
                GetFileInformationByHandleEx(
                    file.as_raw_handle(),
                    FileBasicInfo,
                    &mut info as *mut _ as *mut _,
                    std::mem::size_of_val(&info) as u32,
                )
            } != 0,
            "Cannot inspect export change stamp"
        );
        Ok(Stamp {
            changed: info.ChangeTime as i128,
            written: info.LastWriteTime as i128,
            length: file.metadata()?.len(),
        })
    }
    pub(super) fn links(file: &File) -> Result<u64> {
        let mut info = FILE_STANDARD_INFO::default();
        ensure!(
            unsafe {
                GetFileInformationByHandleEx(
                    file.as_raw_handle(),
                    FileStandardInfo,
                    &mut info as *mut _ as *mut _,
                    std::mem::size_of_val(&info) as u32,
                )
            } != 0,
            "Cannot inspect export link count"
        );
        Ok(u64::from(info.NumberOfLinks))
    }
    fn current_path(file: &File) -> Result<PathBuf> {
        let mut text = [0u16; 32768];
        let length = unsafe {
            GetFinalPathNameByHandleW(
                file.as_raw_handle(),
                text.as_mut_ptr(),
                text.len() as u32,
                FILE_NAME_NORMALIZED | VOLUME_NAME_DOS,
            )
        };
        ensure!(
            length > 0 && (length as usize) < text.len(),
            "Cannot inspect bounded export namespace"
        );
        Ok(PathBuf::from(String::from_utf16(&text[..length as usize])?))
    }
    pub(super) fn verify_directory(path: &Path, pins: &[File]) -> Result<()> {
        ensure!(
            normalize(&current_path(
                pins.last().context("Export root handle missing")?
            )?) == normalize(path),
            "Selected export directory moved"
        );
        Ok(())
    }
    pub(super) fn verify_name(
        parent: &File,
        name: &str,
        expected: &ExportFileIdentity,
    ) -> Result<()> {
        let proof = open_relative(parent, name, false, false, true)?;
        ensure!(
            identity(&proof)? == *expected,
            "Export leaf now belongs to another object"
        );
        Ok(())
    }
    pub(super) fn publish(
        file: &File,
        parent: &File,
        name: &str,
        expected: &ExportFileIdentity,
    ) -> Result<()> {
        ensure!(
            identity(file)? == *expected,
            "Export object changed before publication"
        );
        let text = name.encode_utf16().collect::<Vec<_>>();
        let offset = std::mem::offset_of!(FILE_RENAME_INFO, FileName);
        let length = offset + text.len() * 2;
        let mut storage = vec![0u64; length.div_ceil(8)];
        let info = storage.as_mut_ptr() as *mut FILE_RENAME_INFO;
        unsafe {
            (*info).Anonymous.ReplaceIfExists = false;
            (*info).RootDirectory = parent.as_raw_handle();
            (*info).FileNameLength = (text.len() * 2) as u32;
            std::ptr::copy_nonoverlapping(
                text.as_ptr(),
                (info as *mut u8).add(offset) as *mut u16,
                text.len(),
            );
        }
        let mut status_block = IO_STATUS_BLOCK::default();
        let status = unsafe {
            NtSetInformationFile(
                file.as_raw_handle(),
                &mut status_block,
                info as *const _,
                length as u32,
                FileRenameInformation,
            )
        };
        if status < 0 {
            return Err(io::Error::from_raw_os_error(
                unsafe { RtlNtStatusToDosError(status) } as i32
            )
            .into());
        }
        Ok(())
    }
    pub(super) fn sync_directory(_directory: &File) -> Result<()> {
        Ok(())
    }
    pub(super) fn delete_private(file: &File) -> Result<()> {
        let info = FILE_DISPOSITION_INFO { DeleteFile: true };
        if unsafe {
            SetFileInformationByHandle(
                file.as_raw_handle(),
                FileDispositionInfo,
                &info as *const _ as *const _,
                std::mem::size_of_val(&info) as u32,
            )
        } == 0
        {
            return Err(io::Error::last_os_error().into());
        }
        Ok(())
    }

    fn cleanup_scope(directory: &ExportDirectory, context: &ExportContext) -> Result<()> {
        context.check()?;
        directory.verify()?;
        Ok(())
    }
    fn cleanup_volume(file: &File) -> Result<String> {
        let mut filesystem = [0u16; 32];
        let (mut serial, mut maximum, mut flags) = (0, 0, 0);
        ensure!(
            unsafe {
                GetVolumeInformationByHandleW(
                    file.as_raw_handle(),
                    std::ptr::null_mut(),
                    0,
                    &mut serial,
                    &mut maximum,
                    &mut flags,
                    filesystem.as_mut_ptr(),
                    filesystem.len() as u32,
                )
            } != 0,
            "UnsupportedCapability: export cleanup filesystem cannot be inspected"
        );
        let length = filesystem
            .iter()
            .position(|unit| *unit == 0)
            .context("Cleanup filesystem name is not bounded")?;
        ensure!(
            String::from_utf16(&filesystem[..length])? == "NTFS",
            "UnsupportedCapability: export cleanup calibration requires NTFS"
        );
        let mut path = [0u16; 32768];
        let length = unsafe {
            GetFinalPathNameByHandleW(
                file.as_raw_handle(),
                path.as_mut_ptr(),
                path.len() as u32,
                FILE_NAME_NORMALIZED | VOLUME_NAME_GUID,
            )
        } as usize;
        ensure!(
            length > 0 && length < path.len(),
            "UnsupportedCapability: export cleanup volume GUID is unavailable"
        );
        let path = String::from_utf16(&path[..length])?;
        ensure!(
            path.starts_with("\\\\?\\Volume{"),
            "UnsupportedCapability: export cleanup volume GUID is not native"
        );
        let end = path
            .find("}\\")
            .context("Cleanup volume GUID is malformed")?
            + 2;
        Ok(path[..end].to_ascii_lowercase())
    }
    fn cleanup_identifier(expected: &ExportFileIdentity) -> Result<[u8; 16]> {
        ensure!(
            expected.object.len() == 32
                && expected.object.bytes().all(|byte| byte.is_ascii_hexdigit()),
            "Cleanup requires the original actual 128-bit file identity"
        );
        let mut identifier = [0u8; 16];
        for (index, byte) in identifier.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&expected.object[index * 2..index * 2 + 2], 16)?;
        }
        ensure!(
            identifier[8..].iter().all(|byte| *byte == 0),
            "UnsupportedCapability: export cleanup ID layout is not calibrated"
        );
        Ok(identifier)
    }
    fn cleanup_lookup(
        parent: &File,
        identifier: [u8; 16],
        length: u16,
    ) -> std::result::Result<File, i32> {
        let mut buffer = [0u16; 8];
        for (index, unit) in buffer.iter_mut().enumerate() {
            *unit = u16::from_le_bytes([identifier[index * 2], identifier[index * 2 + 1]]);
        }
        let name = UNICODE_STRING {
            Length: length,
            MaximumLength: length,
            Buffer: buffer.as_mut_ptr(),
        };
        let attributes = OBJECT_ATTRIBUTES {
            Length: std::mem::size_of::<OBJECT_ATTRIBUTES>() as u32,
            RootDirectory: parent.as_raw_handle(),
            ObjectName: &name,
            Attributes: OBJ_CASE_INSENSITIVE,
            SecurityDescriptor: std::ptr::null(),
            SecurityQualityOfService: std::ptr::null(),
        };
        let mut handle = std::ptr::null_mut();
        let mut block = IO_STATUS_BLOCK::default();
        let status = unsafe {
            NtCreateFile(
                &mut handle,
                FILE_READ_ATTRIBUTES | SYNCHRONIZE,
                &attributes,
                &mut block,
                std::ptr::null(),
                0,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                FILE_OPEN,
                windows_sys::Wdk::Storage::FileSystem::FILE_OPEN_BY_FILE_ID
                    | FILE_OPEN_REPARSE_POINT
                    | FILE_SYNCHRONOUS_IO_NONALERT,
                std::ptr::null(),
                0,
            )
        };
        if status < 0 {
            return Err(status);
        }
        if handle.is_null() || handle == INVALID_HANDLE_VALUE {
            return Err(0xc0000008u32 as i32);
        }
        Ok(unsafe { File::from_raw_handle(handle) })
    }
    fn cleanup_present(
        directory: &ExportDirectory,
        expected: &ExportFileIdentity,
        is_directory: bool,
        volume: &str,
        context: &ExportContext,
    ) -> Result<()> {
        let identifier = cleanup_identifier(expected)?;
        for length in [8, 16] {
            cleanup_scope(directory, context)?;
            let file = cleanup_lookup(directory.parent(), identifier, length)
                .map_err(|status| {
                    io::Error::from_raw_os_error(unsafe { RtlNtStatusToDosError(status) } as i32)
                })
                .context("UnsupportedCapability: native export cleanup positive control failed")?;
            validate(&file, is_directory)?;
            require_unchanged(
                identity(&file)? == *expected && cleanup_volume(&file)? == volume,
                "Cleanup native positive control differs from the actual full file identity or volume",
            )?;
            cleanup_scope(directory, context)?;
        }
        Ok(())
    }
    fn cleanup_status(
        directory: &ExportDirectory,
        expected: &ExportFileIdentity,
        status: i32,
        context: &ExportContext,
    ) -> Result<()> {
        let identifier = cleanup_identifier(expected)?;
        for length in [8, 16] {
            cleanup_scope(directory, context)?;
            match cleanup_lookup(directory.parent(), identifier, length) {
                Err(actual) if actual == status => {}
                Err(actual) => bail!(
                    "UnsupportedCapability: native export cleanup signature differs: {actual:#010x}"
                ),
                Ok(file) => {
                    require_unchanged(
                        identity(&file)? == *expected,
                        "Cleanup native lookup returned an unexpected identity",
                    )?;
                    bail!(
                        "Original export file identity still exists, including aliases or renamed objects"
                    );
                }
            }
            cleanup_scope(directory, context)?;
        }
        Ok(())
    }
    fn disposition_state(file: &File) -> Result<(bool, u32)> {
        let mut info = FILE_STANDARD_INFO::default();
        ensure!(
            unsafe {
                GetFileInformationByHandleEx(
                    file.as_raw_handle(),
                    FileStandardInfo,
                    &mut info as *mut _ as *mut _,
                    std::mem::size_of_val(&info) as u32,
                )
            } != 0,
            "Cannot inspect export cleanup disposition"
        );
        Ok((info.DeletePending, info.NumberOfLinks))
    }
    fn calibrate_cleanup(
        state: &mut CleanupState,
        directory: &ExportDirectory,
        context: &ExportContext,
    ) -> Result<()> {
        cleanup_scope(directory, context)?;
        let volume = cleanup_volume(directory.parent())?;
        let root = identity(directory.parent())?;
        cleanup_present(directory, &root, true, &volume, context)?;
        if let Some(calibration) = &state.calibration {
            require_unchanged(
                calibration.volume == volume && calibration.directory == root,
                "Cleanup calibration belongs to a different actual selected volume",
            )?;
            return Ok(());
        }
        if state.probe.is_none() {
            let mut nonce = [0u8; 16];
            getrandom::fill(&mut nonce).map_err(|error| {
                anyhow::anyhow!("Cannot allocate cleanup calibration nonce: {error}")
            })?;
            let nonce: String = nonce.iter().map(|byte| format!("{byte:02x}")).collect();
            let name = format!(".lx-export-calibration-{nonce}.part");
            let file =
                open_relative_with_sharing(directory.parent(), &name, false, true, false, Some(0))?;
            // Retain the exclusive probe before its first fallible inspection.
            state.probe = Some(CalibrationProbe {
                file: Some(file),
                reader: None,
                name,
                identity: None,
                positive_control: false,
                disposition_requested: false,
                zero_link_witness: false,
            });
        }
        let probe = state.probe.as_mut().unwrap();
        if !probe.disposition_requested {
            let file = probe
                .file
                .as_ref()
                .context("Cleanup calibration probe handle is missing")?;
            validate(file, false)?;
            if probe.identity.is_none() {
                probe.identity = Some(identity(file)?);
            }
            let expected = probe.identity.as_ref().unwrap();
            require_unchanged(
                identity(file)? == *expected
                    && expected.device == root.device
                    && file.metadata()?.len() == 0
                    && links(file)? == 1,
                "Cleanup calibration probe is not its original zero-byte single-link object",
            )?;
            verify_name(directory.parent(), &probe.name, expected)?;
            cleanup_present(directory, expected, false, &volume, context)?;
            if probe.reader.is_none() {
                let reader =
                    cleanup_lookup(directory.parent(), cleanup_identifier(expected)?, 16).map_err(
                        |status| {
                            io::Error::from_raw_os_error(
                                unsafe { RtlNtStatusToDosError(status) } as i32
                            )
                        },
                    )?;
                require_unchanged(
                    identity(&reader)? == *expected && cleanup_volume(&reader)? == volume,
                    "Cleanup calibration reader does not identify the original probe",
                )?;
                probe.reader = Some(reader);
            }
            file.sync_all()?;
            probe.positive_control = true;
            cleanup_scope(directory, context)?;
            delete_private(file)?;
            probe.disposition_requested = true;
        }
        let expected = probe
            .identity
            .as_ref()
            .context("Cleanup calibration probe identity is missing")?;
        if let Some(file) = &probe.file {
            let (pending, count) = disposition_state(file)?;
            require_unchanged(
                probe.positive_control && pending && count == 0 && identity(file)? == *expected,
                "Cleanup calibration has no same-handle pending zero-link witness",
            )?;
            cleanup_status(directory, expected, STATUS_DELETE_PENDING, context)?;
            probe.zero_link_witness = true;
            drop(probe.file.take());
        }
        ensure!(
            probe.zero_link_witness,
            "Cleanup calibration closed without its physical witness"
        );
        if let Some(reader) = &probe.reader {
            let (pending, count) = disposition_state(reader)?;
            require_unchanged(
                pending && count == 0 && identity(reader)? == *expected,
                "Cleanup delayed-reader control has no same-object pending zero-link witness",
            )?;
            // Closing the original owner is not sufficient while another real
            // handle still exists. Calibrate that distinct state explicitly.
            cleanup_status(directory, expected, STATUS_DELETE_PENDING, context)?;
            drop(probe.reader.take());
        }
        cleanup_status(directory, expected, STATUS_INVALID_PARAMETER, context)?;
        cleanup_scope(directory, context)?;
        require_unchanged(
            cleanup_volume(directory.parent())? == volume && identity(directory.parent())? == root,
            "Actual export cleanup volume changed during calibration",
        )?;
        state.calibration = Some(CleanupCalibration {
            volume,
            directory: root,
        });
        state.probe = None;
        Ok(())
    }
    pub(super) fn prepare_cleanup(
        state: &mut CleanupState,
        file: &File,
        expected: &ExportFileIdentity,
        directory: &ExportDirectory,
        context: &ExportContext,
        captured: &Stamp,
        sha256: &str,
    ) -> Result<()> {
        cleanup_scope(directory, context)?;
        calibrate_cleanup(state, directory, context)?;
        if state.disposition_requested {
            return Ok(());
        }
        let calibration = state.calibration.as_ref().unwrap();
        require_unchanged(
            identity(file)? == *expected && stamp(file)? == *captured && links(file)? == 1,
            "Cleanup original changed after actual whole-body verification",
        )?;
        ensure!(
            sha256.len() == 64 && sha256.bytes().all(|byte| byte.is_ascii_hexdigit()),
            "Cleanup whole digest is missing"
        );
        cleanup_present(directory, expected, false, &calibration.volume, context)?;
        file.sync_all()?;
        require_unchanged(
            stamp(file)? == *captured,
            "Cleanup original changed during native preparation",
        )?;
        state.prepared = Some(CleanupPrepared {
            identity: expected.clone(),
            stamp: captured.clone(),
            sha256: sha256.into(),
        });
        cleanup_scope(directory, context)
    }
    pub(super) fn request_cleanup_disposition(
        state: &mut CleanupState,
        file: &File,
        expected: &ExportFileIdentity,
        directory: &ExportDirectory,
        context: &ExportContext,
    ) -> Result<()> {
        cleanup_scope(directory, context)?;
        let prepared = state
            .prepared
            .as_ref()
            .context("Cleanup original has no native-valid preparation")?;
        require_unchanged(
            prepared.identity == *expected && identity(file)? == *expected,
            "Cleanup disposition does not identify the actual prepared original",
        )?;
        if !state.disposition_requested {
            require_unchanged(
                stamp(file)? == prepared.stamp && links(file)? == 1,
                "Cleanup original changed before native disposition",
            )?;
            delete_private(file)?;
            state.disposition_requested = true;
        }
        let (pending, count) = disposition_state(file)?;
        require_unchanged(
            pending && count == 0 && identity(file)? == *expected,
            "Cleanup original has no same-handle pending zero-link witness",
        )?;
        cleanup_status(directory, expected, STATUS_DELETE_PENDING, context)?;
        state.zero_link_witness = true;
        cleanup_scope(directory, context)
    }
    pub(super) fn confirm_cleanup(
        state: &CleanupState,
        expected: &ExportFileIdentity,
        directory: &ExportDirectory,
        context: &ExportContext,
    ) -> Result<()> {
        cleanup_scope(directory, context)?;
        let prepared = state
            .prepared
            .as_ref()
            .context("Closed cleanup original has no native preparation")?;
        let calibration = state
            .calibration
            .as_ref()
            .context("Cleanup original has no native calibration")?;
        ensure!(
            state.disposition_requested
                && state.zero_link_witness
                && prepared.identity == *expected
                && prepared.sha256.len() == 64,
            "Cleanup original has no matching native disposition and body witness"
        );
        require_unchanged(
            cleanup_volume(directory.parent())? == calibration.volume
                && identity(directory.parent())? == calibration.directory,
            "Cleanup original is on a different actual selected volume",
        )?;
        cleanup_present(
            directory,
            &calibration.directory,
            true,
            &calibration.volume,
            context,
        )?;
        // Invalid-parameter is accepted only for this previously native-valid
        // exact ID and this fresh opaque pending/close calibration, never as a
        // generic OpenFileById failure or a private-name absence shortcut.
        cleanup_status(directory, expected, STATUS_INVALID_PARAMETER, context)?;
        require_unchanged(
            cleanup_volume(directory.parent())? == calibration.volume
                && identity(directory.parent())? == calibration.directory,
            "Cleanup selected volume changed during the exact absent-ID lookup",
        )?;
        cleanup_scope(directory, context)
    }
}

#[cfg(target_os = "linux")]
mod platform {
    use super::*;
    use std::{
        ffi::CString,
        os::unix::{
            ffi::OsStrExt,
            fs::MetadataExt,
            io::{AsRawFd, FromRawFd},
        },
    };
    pub(super) fn pin_directory(path: &Path) -> Result<(PathBuf, Vec<File>)> {
        let fd = unsafe {
            libc::open(
                c"/".as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error().into());
        }
        let mut pins = vec![unsafe { File::from_raw_fd(fd) }];
        for component in path.components() {
            if let Component::Normal(name) = component {
                let name = CString::new(name.as_bytes())?;
                let fd = unsafe {
                    libc::openat(
                        pins.last().unwrap().as_raw_fd(),
                        name.as_ptr(),
                        libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                    )
                };
                if fd < 0 {
                    return Err(io::Error::last_os_error().into());
                }
                let file = unsafe { File::from_raw_fd(fd) };
                validate(&file, true)?;
                pins.push(file);
            }
        }
        Ok((path.to_path_buf(), pins))
    }
    pub(super) fn validate(file: &File, directory: bool) -> Result<()> {
        let metadata = file.metadata()?;
        ensure!(
            if directory {
                metadata.is_dir()
            } else {
                metadata.is_file()
            },
            "Export object type refused"
        );
        Ok(())
    }
    pub(super) fn identity(file: &File) -> Result<ExportFileIdentity> {
        let metadata = file.metadata()?;
        Ok(ExportFileIdentity {
            device: metadata.dev(),
            object: metadata.ino().to_string(),
        })
    }
    pub(super) fn stamp(file: &File) -> Result<Stamp> {
        let metadata = file.metadata()?;
        Ok(Stamp {
            changed: metadata.ctime() as i128 * 1_000_000_000 + metadata.ctime_nsec() as i128,
            written: metadata.mtime() as i128 * 1_000_000_000 + metadata.mtime_nsec() as i128,
            length: metadata.len(),
        })
    }
    pub(super) fn links(file: &File) -> Result<u64> {
        Ok(file.metadata()?.nlink())
    }
    pub(super) fn verify_directory(path: &Path, pins: &[File]) -> Result<()> {
        let mut parent = &pins[0];
        let mut index = 1;
        for component in path.components() {
            if let Component::Normal(name) = component {
                let name = CString::new(name.as_bytes())?;
                let fd = unsafe {
                    libc::openat(
                        parent.as_raw_fd(),
                        name.as_ptr(),
                        libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                    )
                };
                if fd < 0 {
                    return Err(io::Error::last_os_error().into());
                }
                let observed = unsafe { File::from_raw_fd(fd) };
                ensure!(
                    identity(&observed)? == identity(&pins[index])?,
                    "Selected export ancestor namespace changed"
                );
                parent = &pins[index];
                index += 1;
            }
        }
        Ok(())
    }
    pub(super) fn create_private(parent: &File, _name: Option<&str>) -> Result<File> {
        let fd = unsafe {
            libc::openat(
                parent.as_raw_fd(),
                c".".as_ptr(),
                libc::O_TMPFILE | libc::O_RDWR | libc::O_CLOEXEC,
                0o600,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error()).context(
                "UnsupportedCapability: filesystem cannot create an anonymous export inode",
            );
        }
        Ok(unsafe { File::from_raw_fd(fd) })
    }
    pub(super) fn verify_name(
        parent: &File,
        name: &str,
        expected: &ExportFileIdentity,
    ) -> Result<()> {
        let name = CString::new(name)?;
        let fd = unsafe {
            libc::openat(
                parent.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error().into());
        }
        let file = unsafe { File::from_raw_fd(fd) };
        validate(&file, false)?;
        ensure!(
            identity(&file)? == *expected,
            "Published export leaf belongs to another object"
        );
        Ok(())
    }
    pub(super) fn publish(
        file: &File,
        parent: &File,
        name: &str,
        expected: &ExportFileIdentity,
    ) -> Result<()> {
        ensure!(
            identity(file)? == *expected && links(file)? == 0,
            "Anonymous export inode changed"
        );
        let name = CString::new(name)?;
        if unsafe {
            libc::linkat(
                file.as_raw_fd(),
                c"".as_ptr(),
                parent.as_raw_fd(),
                name.as_ptr(),
                libc::AT_EMPTY_PATH,
            )
        } != 0
        {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::AlreadyExists {
                return Err(error.into());
            }
            if !matches!(error.raw_os_error(), Some(libc::EPERM) | Some(libc::ENOENT)) {
                return Err(error.into());
            }
            let descriptor = CString::new(format!("/proc/self/fd/{}", file.as_raw_fd()))?;
            if unsafe {
                libc::linkat(
                    libc::AT_FDCWD,
                    descriptor.as_ptr(),
                    parent.as_raw_fd(),
                    name.as_ptr(),
                    libc::AT_SYMLINK_FOLLOW,
                )
            } != 0
            {
                return Err(io::Error::last_os_error())
                    .context("UnsupportedCapability: actual-FD no-replace export link failed");
            }
        }
        Ok(())
    }
    pub(super) fn sync_directory(directory: &File) -> Result<()> {
        directory.sync_all()?;
        Ok(())
    }
    pub(super) fn delete_private(file: &File) -> Result<()> {
        ensure!(
            links(file)? == 0,
            "Named Linux export cannot be cleaned as an anonymous temporary inode"
        );
        Ok(())
    }
    pub(super) fn verify_absent(
        _parent: &File,
        _name: &str,
        _expected: &ExportFileIdentity,
    ) -> Result<()> {
        bail!("UnsupportedCapability: Linux anonymous exports have no private leaf")
    }
}

#[cfg(not(any(windows, target_os = "linux")))]
mod platform {
    use super::*;
    pub(super) fn pin_directory(_path: &Path) -> Result<(PathBuf, Vec<File>)> {
        bail!("UnsupportedCapability: no verified export publication primitive for this platform")
    }
    pub(super) fn identity(_file: &File) -> Result<ExportFileIdentity> {
        bail!("UnsupportedCapability")
    }
    pub(super) fn validate(_file: &File, _directory: bool) -> Result<()> {
        bail!("UnsupportedCapability")
    }
    pub(super) fn stamp(_file: &File) -> Result<Stamp> {
        bail!("UnsupportedCapability")
    }
    pub(super) fn links(_file: &File) -> Result<u64> {
        bail!("UnsupportedCapability")
    }
    pub(super) fn verify_directory(_path: &Path, _pins: &[File]) -> Result<()> {
        bail!("UnsupportedCapability")
    }
    pub(super) fn create_private(_parent: &File, _name: Option<&str>) -> Result<File> {
        bail!("UnsupportedCapability")
    }
    pub(super) fn verify_name(
        _parent: &File,
        _name: &str,
        _expected: &ExportFileIdentity,
    ) -> Result<()> {
        bail!("UnsupportedCapability")
    }
    pub(super) fn publish(
        _file: &File,
        _parent: &File,
        _name: &str,
        _expected: &ExportFileIdentity,
    ) -> Result<()> {
        bail!("UnsupportedCapability")
    }
    pub(super) fn sync_directory(_directory: &File) -> Result<()> {
        bail!("UnsupportedCapability")
    }
    pub(super) fn delete_private(_file: &File) -> Result<()> {
        bail!("UnsupportedCapability")
    }
    pub(super) fn verify_absent(
        _parent: &File,
        _name: &str,
        _expected: &ExportFileIdentity,
    ) -> Result<()> {
        bail!("UnsupportedCapability")
    }
}

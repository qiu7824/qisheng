//! Actual creator ownership for the default SQLite WAL and shared-memory files.
//! Durable facts describe ownership; only the retained native creator can hand
//! an object to the default writer or to calibrated private retirement.
use super::*;
use rusqlite::ffi;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs::File,
    os::windows::io::{AsRawHandle, FromRawHandle},
};
use windows_sys::{
    Wdk::{
        Foundation::OBJECT_ATTRIBUTES,
        Storage::FileSystem::{
            FILE_CREATE, FILE_NON_DIRECTORY_FILE, FILE_OPEN, FILE_OPEN_IF, FILE_OPEN_REPARSE_POINT,
            FILE_SYNCHRONOUS_IO_NONALERT, NtCreateFile,
        },
    },
    Win32::{
        Foundation::{GENERIC_READ, GENERIC_WRITE, HANDLE, OBJ_CASE_INSENSITIVE, UNICODE_STRING},
        Storage::FileSystem::{FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE},
        System::IO::IO_STATUS_BLOCK,
    },
};
const SYNCHRONIZE: u32 = 0x00100000;
#[path = "writer_sidecars/journal.rs"]
mod journal;
pub(crate) use journal::{
    DEFAULT_WRITER_SIDECAR_BYTES, NativeSidecarJournal, WriterStartupFailure, reserve_writer_permit,
};
#[path = "writer_sidecars/vfs.rs"]
mod vfs;
pub(crate) use vfs::SidecarWriter;
#[path = "writer_sidecars/native.rs"]
mod native;
pub(crate) fn new_writer_operation() -> Result<String> {
    Ok(format!("sqlite-writer-{}", nonce()?))
}
pub(crate) fn retry_retained_resources(context: &SnapshotContext) -> Result<()> {
    vfs::retry_retained_writers(context)?;
    journal::retry_retained_startups(context)?;
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum WriterSidecarRole {
    Wal,
    Shm,
    Journal,
}
impl WriterSidecarRole {
    fn leaf(self) -> &'static str {
        match self {
            Self::Wal => "native-library.sqlite3-wal",
            Self::Shm => "native-library.sqlite3-shm",
            Self::Journal => "native-library.sqlite3-journal",
        }
    }
    fn retired(self, nonce: &str) -> String {
        format!(
            ".lx-profile-{nonce}-{}.owned",
            match self {
                Self::Wal => "wal",
                Self::Shm => "shm",
                Self::Journal => "journal",
            }
        )
    }
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum WriterSidecarOrigin {
    #[default]
    Pending,
    Created,
    ExistingLockedProfile,
}
/// Actual profile continuity, never a deserializable file grant. Ordinary cold
/// opens may admit legacy SQLite sidecars only under this same locked profile.
#[derive(Clone)]
pub(crate) struct WriterProfileAdmission {
    profile: SnapshotDirectory,
    lock_identity: FileIdentity,
    continuity: std::sync::Weak<()>,
}
impl WriterProfileAdmission {
    pub(crate) fn new(
        profile: SnapshotDirectory,
        lock: &File,
        continuity: std::sync::Weak<()>,
    ) -> Result<Self> {
        platform::validate(lock, false)?;
        let lock_identity = platform::identity(lock)?;
        ensure!(
            platform::links(lock)? == 1,
            "SQLite profile writer lock is aliased"
        );
        let admission = Self {
            profile,
            lock_identity,
            continuity,
        };
        admission.verify()?;
        Ok(admission)
    }
    fn verify(&self) -> Result<()> {
        ensure!(
            self.continuity.upgrade().is_some(),
            "SQLite actual profile writer continuity ended"
        );
        self.profile.verify()?;
        platform::verify_name(
            self.profile.parent(),
            "native-library.lock",
            &self.lock_identity,
        )
    }
    fn verify_profile(&self, profile: &SnapshotDirectory) -> Result<()> {
        self.verify()?;
        ensure!(
            self.profile.identity() == profile.identity(),
            "SQLite cold admission belongs to another actual profile"
        );
        Ok(())
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct WriterSidecarCreationFact {
    pub operation_id: String,
    pub role: WriterSidecarRole,
    pub nonce: String,
    pub profile: FileIdentity,
    pub main: FileIdentity,
    pub leaf: String,
    pub maximum_bytes: u64,
    pub identity: Option<FileIdentity>,
    #[serde(default)]
    pub origin: WriterSidecarOrigin,
    #[serde(default)]
    pub admitted_bytes: u64,
    #[serde(default)]
    pub admitted_sha256: Option<String>,
    #[serde(default)]
    pub native_failed_without_handle: bool,
    #[serde(default)]
    pub admission_rejected_and_closed: bool,
}
pub(crate) trait WriterSidecarJournal: Send + Sync {
    fn creation(&self, fact: &WriterSidecarCreationFact) -> Result<()>;
    fn retirement(&self, fact: &DurableCleanupFact) -> Result<()>;
    fn finish(&self, context: &SnapshotContext) -> Result<()>;
}
struct Entry {
    fact: WriterSidecarCreationFact,
    creator_pin: Option<File>,
    reader: Option<File>,
    writable: Option<File>,
    retirement: Option<ActualFile>,
    allocation_attempted: bool,
    created: Option<bool>,
    creation_acknowledged: bool,
    charge: Option<FileCharge>,
    maximum_observed: u64,
    private_name: Option<String>,
    cleanup: Option<DurableCleanupFact>,
    cleanup_acknowledged: bool,
}
pub(crate) struct WriterSidecars {
    profile: SnapshotDirectory,
    private: SnapshotDirectory,
    main: FileIdentity,
    operation_id: String,
    maximum: u64,
    permit: SnapshotPermit,
    journal: Arc<dyn WriterSidecarJournal>,
    entries: BTreeMap<WriterSidecarRole, Entry>,
    sqlite_closed: bool,
    admission: Option<WriterProfileAdmission>,
    admitted_existing_main: bool,
}
impl WriterSidecars {
    pub(crate) fn new(
        profile: SnapshotDirectory,
        main: FileIdentity,
        operation_id: String,
        maximum: u64,
        permit: SnapshotPermit,
        journal: Arc<dyn WriterSidecarJournal>,
    ) -> Result<Self> {
        profile.verify()?;
        validate_operation(&operation_id)?;
        ensure!(
            maximum > 0 && maximum <= permit.0.maximum,
            "SQLite sidecar exceeds its existing operation allowance"
        );
        let private = SnapshotDirectory::open_private(profile.path(), profile.epoch())?;
        Ok(Self {
            profile,
            private,
            main,
            operation_id,
            maximum,
            permit,
            journal,
            entries: BTreeMap::new(),
            sqlite_closed: false,
            admission: None,
            admitted_existing_main: false,
        })
    }
    pub(crate) fn allow_locked_profile_admission(
        &mut self,
        admission: WriterProfileAdmission,
        existing_main: bool,
    ) -> Result<()> {
        admission.verify_profile(&self.profile)?;
        ensure!(
            self.entries.is_empty(),
            "Cold profile admission cannot replace an active SQLite owner"
        );
        self.admission = Some(admission);
        self.admitted_existing_main = existing_main;
        Ok(())
    }
    /// No native allocation occurs before its durable creation intent. A failed
    /// FID or journal ACK keeps the original pin under this same entry.
    pub(crate) fn admit(&mut self, role: WriterSidecarRole) -> Result<()> {
        ensure!(
            !self.sqlite_closed,
            "Closed SQLite writer cannot create another sidecar"
        );
        self.profile.verify()?;
        if !self.entries.contains_key(&role) {
            let fact = WriterSidecarCreationFact {
                operation_id: self.operation_id.clone(),
                role,
                nonce: nonce()?,
                profile: self.profile.identity().clone(),
                main: self.main.clone(),
                leaf: role.leaf().into(),
                maximum_bytes: self.maximum,
                identity: None,
                origin: WriterSidecarOrigin::Pending,
                admitted_bytes: 0,
                admitted_sha256: None,
                native_failed_without_handle: false,
                admission_rejected_and_closed: false,
            };
            let charge = self.permit.subfile(self.maximum, false)?;
            self.journal.creation(&fact)?;
            self.entries.insert(
                role,
                Entry {
                    fact,
                    creator_pin: None,
                    reader: None,
                    writable: None,
                    retirement: None,
                    allocation_attempted: false,
                    created: None,
                    creation_acknowledged: false,
                    charge: Some(charge),
                    maximum_observed: 0,
                    private_name: None,
                    cleanup: None,
                    cleanup_acknowledged: false,
                },
            );
        }
        let entry = self.entries.get_mut(&role).unwrap();
        if entry.creation_acknowledged {
            return verify_entry(&self.profile, entry);
        }
        if entry.creator_pin.is_none() {
            ensure!(
                !entry.allocation_attempted,
                "Unacknowledged SQLite allocation cannot be reopened by name"
            );
            entry.allocation_attempted = true;
            // A native zero-object creator with no SHARE_DELETE permits the genuine default VFS's
            // READ_WRITE open and prevents replacement of this actual leaf.
            if let Some(admission) = &self.admission {
                admission.verify_profile(&self.profile)?;
            }
            let opened = open_native_pin(
                self.profile.parent(),
                role.leaf(),
                if self.admission.is_some() {
                    FILE_OPEN_IF
                } else {
                    FILE_CREATE
                },
                false,
            )?;
            entry.creator_pin = opened.file;
            entry.fact.native_failed_without_handle =
                opened.status < 0 && entry.creator_pin.is_none();
            native_status(opened.status)?;
            ensure!(
                entry.creator_pin.is_some() && (opened.information == 1 || opened.information == 2),
                "Native SQLite sidecar disposition or returned handle is unknown"
            );
            entry.created = Some(opened.information == 2);
        }
        let file = entry.creator_pin.as_ref().unwrap();
        platform::validate(file, false)?;
        ensure!(
            platform::links(file)? == 1 && file.metadata()?.len() <= entry.fact.maximum_bytes,
            "SQLite creator did not retain its bounded single-link object"
        );
        let identity = platform::identity(file)?;
        if let Some(expected) = &entry.fact.identity {
            ensure!(*expected == identity, "SQLite creator actual FID changed");
        }
        entry.fact.identity = Some(identity);
        if entry.created == Some(true) {
            ensure!(
                file.metadata()?.len() == 0,
                "New SQLite creator returned a nonempty object before zero-byte ACK"
            );
            entry.fact.origin = WriterSidecarOrigin::Created;
        } else {
            ensure!(
                self.admitted_existing_main,
                "Legacy SQLite sidecar cannot be admitted to a newly initialized main"
            );
            self.admission
                .as_ref()
                .context("Existing SQLite sidecar has no actual locked-profile admission")?
                .verify_profile(&self.profile)?;
            if role == WriterSidecarRole::Shm {
                ensure!(
                    file.metadata()?.len() % 32768 == 0,
                    "Existing SQLite SHM has an invalid physical region length"
                );
            }
            let fingerprint = fingerprint_source_file(file, &|| self.profile.verify())?;
            entry.fact.origin = WriterSidecarOrigin::ExistingLockedProfile;
            entry.fact.admitted_bytes = fingerprint.stamp.length;
            entry.fact.admitted_sha256 = Some(fingerprint.sha256);
            entry.maximum_observed = entry.fact.admitted_bytes;
            entry
                .charge
                .as_mut()
                .unwrap()
                .resize(entry.maximum_observed)?;
        }
        platform::verify_name(
            self.profile.parent(),
            role.leaf(),
            entry.fact.identity.as_ref().unwrap(),
        )?;
        file.sync_all()?;
        platform::sync_directory(self.profile.parent())?;
        self.journal.creation(&entry.fact)?;
        entry.creation_acknowledged = true;
        verify_entry(&self.profile, entry)
    }
    pub(crate) fn check(&self, role: WriterSidecarRole) -> Result<()> {
        let entry = self
            .entries
            .get(&role)
            .context("SQLite sidecar has no actual creator")?;
        ensure!(
            entry.creation_acknowledged,
            "SQLite creation lacks its durable actual FID ACK"
        );
        verify_entry(&self.profile, entry)
    }
    pub(crate) unsafe fn verify_accessor(
        &self,
        role: WriterSidecarRole,
        accessor: *mut ffi::sqlite3_file,
    ) -> Result<()> {
        self.check(role)?;
        ensure!(
            !accessor.is_null(),
            "SQLite sidecar native accessor is missing"
        );
        let control = unsafe { (*accessor).pMethods.as_ref() }
            .and_then(|methods| methods.xFileControl)
            .context("Default SQLite sidecar file-control is missing")?;
        let mut handle: HANDLE = std::ptr::null_mut();
        ensure!(
            unsafe {
                control(
                    accessor,
                    ffi::SQLITE_FCNTL_WIN32_GET_HANDLE,
                    (&mut handle as *mut HANDLE).cast(),
                )
            } == ffi::SQLITE_OK
                && !handle.is_null(),
            "SQLite sidecar actual native handle is unavailable"
        );
        unsafe { self.verify_native_handle(role, handle) }
    }
    pub(crate) unsafe fn verify_native_handle(
        &self,
        role: WriterSidecarRole,
        handle: HANDLE,
    ) -> Result<()> {
        self.check(role)?;
        ensure!(
            !handle.is_null(),
            "Default SQLite actual native handle is missing"
        );
        let borrowed = std::mem::ManuallyDrop::new(unsafe { File::from_raw_handle(handle) });
        platform::validate(&borrowed, false)?;
        ensure!(
            platform::identity(&borrowed)? == *self.entries[&role].fact.identity.as_ref().unwrap()
                && platform::links(&borrowed)? == 1,
            "Default SQLite opened another actual sidecar"
        );
        self.check(role)
    }
    /// Reservation precedes default xWrite/xTruncate. Failed writes retain the
    /// maximum liability until the actual object has retired, never refund it.
    pub(crate) fn reserve_write(&mut self, role: WriterSidecarRole, end: u64) -> Result<()> {
        self.check(role)?;
        let entry = self.entries.get_mut(&role).unwrap();
        ensure!(
            end <= entry.fact.maximum_bytes,
            "Default SQLite sidecar growth quota exceeded"
        );
        entry.maximum_observed = entry.maximum_observed.max(end);
        entry
            .charge
            .as_mut()
            .context("SQLite sidecar liability moved to retirement")?
            .resize(entry.maximum_observed)?;
        Ok(())
    }
    /// Called only after genuine sqlite3_close succeeded. An accessor xClose is
    /// insufficient: SHM mappings and other native handles may remain alive.
    pub(crate) fn acknowledge_sqlite_close(&mut self) {
        self.sqlite_closed = true;
    }
    pub(crate) fn wal_needs_checkpoint(&self) -> Result<bool> {
        self.entries
            .get(&WriterSidecarRole::Wal)
            .and_then(|entry| entry.creator_pin.as_ref())
            .map_or(Ok(false), |file| Ok(file.metadata()?.len() != 0))
    }
    pub(crate) fn reconcile_retained_creations(&mut self) -> Result<()> {
        let roles = self
            .entries
            .iter()
            .filter_map(|(role, entry)| {
                (!entry.creation_acknowledged
                    && entry.created == Some(true)
                    && entry.creator_pin.is_some())
                .then_some(*role)
            })
            .collect::<Vec<_>>();
        let was_closed = self.sqlite_closed;
        self.sqlite_closed = false;
        let result = roles.into_iter().try_for_each(|role| self.admit(role));
        self.sqlite_closed = was_closed;
        result
    }
    pub(crate) fn retire(&mut self, context: &SnapshotContext) -> Result<()> {
        ensure!(self.sqlite_closed, "SQLite writer has not actually closed");
        if let Some(wal) = self.entries.get(&WriterSidecarRole::Wal)
            && wal.creation_acknowledged
            && wal.private_name.is_none()
        {
            ensure!(
                wal.creator_pin
                    .as_ref()
                    .or(wal.reader.as_ref())
                    .or(wal.writable.as_ref())
                    .context("SQLite WAL retirement lost every actual retained handle")?
                    .metadata()?
                    .len()
                    == 0,
                "SQLite WAL still contains frames; actual checkpoint/truncation must precede retirement"
            );
        }
        self.retire_selected(context, None)
    }
    /// A rollback-journal xDelete follows its actual accessor xClose while the
    /// main connection remains open. The VFS verifies that witness first.
    pub(crate) fn retire_journal_after_accessor_close(
        &mut self,
        context: &SnapshotContext,
    ) -> Result<()> {
        ensure!(
            self.entries.contains_key(&WriterSidecarRole::Journal),
            "Rollback journal has no admitted native owner"
        );
        self.retire_selected(context, Some(WriterSidecarRole::Journal))?;
        self.entries.remove(&WriterSidecarRole::Journal);
        Ok(())
    }
    fn retire_selected(
        &mut self,
        context: &SnapshotContext,
        selected: Option<WriterSidecarRole>,
    ) -> Result<()> {
        for entry in self
            .entries
            .values_mut()
            .filter(|entry| selected.is_none_or(|role| entry.fact.role == role))
        {
            if entry.fact.native_failed_without_handle {
                ensure!(
                    entry.creator_pin.is_none(),
                    "Failed SQLite native return retained an actual handle"
                );
                self.journal.creation(&entry.fact)?;
                entry.charge.take();
                continue;
            }
            if !entry.creation_acknowledged && entry.created == Some(false) {
                // A rejected legacy admission never becomes a creator or gains
                // delete authority. Close this actual native pin and preserve
                // the pre-existing file's body/name in its original profile.
                platform::close_retained(&mut entry.creator_pin)?;
                entry.fact.admission_rejected_and_closed = true;
                self.journal.creation(&entry.fact)?;
                entry.charge.take();
                continue;
            }
            if entry.cleanup_acknowledged && entry.cleanup.as_ref().is_some_and(|fact| fact.removed)
            {
                continue;
            }
            ensure!(
                entry.creation_acknowledged,
                "SQLite sidecar lacks acknowledged creator FID"
            );
            self.profile.verify()?;
            self.private.verify()?;
            if entry.retirement.is_none() {
                if entry.reader.is_none() && entry.private_name.is_none() {
                    verify_entry(&self.profile, entry)?;
                    let opened = open_native_pin(
                        self.profile.parent(),
                        entry.fact.role.leaf(),
                        FILE_OPEN,
                        true,
                    )?;
                    entry.reader = opened.file;
                    native_status(opened.status)?;
                    ensure!(
                        entry.reader.is_some(),
                        "SQLite retirement returned no actual reader"
                    );
                    ensure!(
                        platform::identity(entry.reader.as_ref().unwrap())?
                            == *entry.fact.identity.as_ref().unwrap(),
                        "SQLite retirement reader identifies another actual object"
                    );
                }
                // The reader stays live throughout the actual creator-pin close
                // and subsequent exclusive retirement-handle acquisition.
                platform::close_retained(&mut entry.creator_pin)?;
                if entry.writable.is_none() {
                    let (directory, name) = match &entry.private_name {
                        Some(name) => (&self.private, name.as_str()),
                        None => (&self.profile, entry.fact.role.leaf()),
                    };
                    entry.writable = Some(directory.open_group_file(name, false)?);
                }
                let file = entry.writable.as_ref().unwrap();
                ensure!(
                    platform::identity(file)? == *entry.fact.identity.as_ref().unwrap()
                        && platform::links(file)? == 1,
                    "SQLite retirement ownership changed or is aliased"
                );
                let fingerprint = fingerprint_source_file(file, &|| context.check())?;
                ensure!(
                    fingerprint.stamp.length <= entry.fact.maximum_bytes,
                    "SQLite sidecar grew outside its admitted reservation"
                );
                if entry.private_name.is_none() {
                    let name = entry.fact.role.retired(&entry.fact.nonce);
                    let result = self
                        .private
                        .rename_group_file(file, &name, &fingerprint.identity);
                    if self
                        .private
                        .verify_group_name(&name, &fingerprint.identity)
                        .is_ok()
                    {
                        entry.private_name = Some(name);
                    }
                    result?;
                }
                platform::close_retained(&mut entry.reader)?;
                let metadata = ActualFileMetadata {
                    operation_id: entry.fact.operation_id.clone(),
                    nonce: entry.fact.nonce.clone(),
                    temporary_name: entry.private_name.clone(),
                    directory: self.private.path().to_owned(),
                    directory_identity: self.private.identity().clone(),
                    selection_epoch: self.private.epoch(),
                    identity: entry.fact.identity.clone(),
                    bytes: fingerprint.stamp.length,
                    published: false,
                    final_basename: None,
                    cleanup_probe: None,
                };
                entry.retirement = Some(ActualFile::adopt_group_file(
                    &mut entry.writable,
                    &self.private,
                    metadata,
                    &fingerprint.sha256,
                    context,
                    &self.permit,
                )?);
                entry.charge.take();
            }
            let actual = entry.retirement.take().unwrap();
            let slot =
                std::cell::RefCell::new((&mut entry.cleanup, &mut entry.cleanup_acknowledged));
            let result = actual.cleanup_journaled(context, &|fact| {
                let mut slot = slot.borrow_mut();
                *slot.0 = Some(fact.clone());
                *slot.1 = false;
                self.journal.retirement(fact)?;
                *slot.1 = true;
                Ok(())
            });
            if let Err(mut failure) = result {
                entry.retirement = failure.file.take().map(|file| *file);
                anyhow::bail!("{}", failure.message);
            }
        }
        Ok(())
    }
}
fn verify_entry(directory: &SnapshotDirectory, entry: &Entry) -> Result<()> {
    directory.verify()?;
    let file = entry
        .creator_pin
        .as_ref()
        .context("SQLite creator pin has already entered retirement")?;
    platform::validate(file, false)?;
    ensure!(
        Some(platform::identity(file)?) == entry.fact.identity && platform::links(file)? == 1,
        "SQLite creator object is foreign or aliased"
    );
    ensure!(
        file.metadata()?.len() <= entry.fact.maximum_bytes,
        "SQLite sidecar exceeds its admitted maximum"
    );
    platform::verify_name(
        directory.parent(),
        entry.fact.role.leaf(),
        entry.fact.identity.as_ref().unwrap(),
    )
}
struct NativePinReturn {
    file: Option<File>,
    status: i32,
    information: usize,
}
fn native_status(status: i32) -> Result<()> {
    if status < 0 {
        return Err(std::io::Error::from_raw_os_error(unsafe {
            windows_sys::Wdk::Foundation::RtlNtStatusToDosError(status)
        } as i32)
        .into());
    }
    Ok(())
}
fn open_native_pin(
    parent: &File,
    name: &str,
    disposition: u32,
    share_delete: bool,
) -> Result<NativePinReturn> {
    validate_basename(name)?;
    let mut text = name.encode_utf16().collect::<Vec<_>>();
    let mut unicode = UNICODE_STRING {
        Length: (text.len() * 2) as u16,
        MaximumLength: (text.len() * 2) as u16,
        Buffer: text.as_mut_ptr(),
    };
    let attributes = OBJECT_ATTRIBUTES {
        Length: std::mem::size_of::<OBJECT_ATTRIBUTES>() as u32,
        RootDirectory: parent.as_raw_handle(),
        ObjectName: &mut unicode,
        Attributes: OBJ_CASE_INSENSITIVE,
        SecurityDescriptor: std::ptr::null(),
        SecurityQualityOfService: std::ptr::null(),
    };
    let mut handle = std::ptr::null_mut();
    let mut status_block = IO_STATUS_BLOCK::default();
    let status = unsafe {
        NtCreateFile(
            &mut handle,
            GENERIC_READ
                | SYNCHRONIZE
                | if disposition != FILE_OPEN {
                    GENERIC_WRITE
                } else {
                    0
                },
            &attributes,
            &mut status_block,
            std::ptr::null(),
            0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | if share_delete { FILE_SHARE_DELETE } else { 0 },
            disposition,
            FILE_OPEN_REPARSE_POINT | FILE_SYNCHRONOUS_IO_NONALERT | FILE_NON_DIRECTORY_FILE,
            std::ptr::null(),
            0,
        )
    };
    // Transport every native return, including an unexpected failed status with
    // an actual handle. The caller places it into its entry before inspection.
    let file = (!handle.is_null()
        && handle != windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE)
        .then(|| unsafe { File::from_raw_handle(handle) });
    Ok(NativePinReturn {
        file,
        status,
        information: status_block.Information,
    })
}
#[cfg(test)]
#[path = "writer_sidecars/tests.rs"]
mod tests;

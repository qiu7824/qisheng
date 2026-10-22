//! Operation-scoped SQLite I/O over retained actual snapshot descriptors.
//! Destination filenames are private symbolic names; they never name OS files.
//! SQLite close/delete releases logical access only. The coordinator retains
//! actual objects until native cleanup or verified publication completes.

use super::snapshot_native::{
    ActualFile, DurableManifest, IO_CHUNK, NativeFailure, SnapshotContext, SnapshotDirectory,
    SnapshotPermit, SourceFileProof,
};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OpenFlags, ffi};
use std::{
    collections::HashMap,
    ffi::{CStr, CString, c_char, c_int, c_void},
    fmt,
    mem::size_of,
    ptr,
    sync::{Arc, Mutex},
};

// Manifest plus a possible selected publication candidate use two subfiles.
const MAX_FILES: usize = 14;
const MAX_NAME: usize = 4096;
pub(crate) const MAIN_NAME: &str = "snapshot-main";

struct FileSlot {
    file: Option<ActualFile>,
    open: bool,
    deleted: bool,
    lock: c_int,
    delete_on_close: bool,
}
pub(crate) struct OwnedFiles {
    manifest: Arc<Mutex<DurableManifest>>,
    directory: SnapshotDirectory,
    permit: SnapshotPermit,
    context: SnapshotContext,
    operation: String,
    maximum: u64,
    restore_main_rollback: bool,
    slots: Vec<FileSlot>,
    names: HashMap<String, usize>,
    #[expect(
        clippy::vec_box,
        reason = "NativeFailure transfers its existing boxed owner unchanged; unpack only after SQLite accessors retire"
    )]
    failed: Vec<Box<ActualFile>>,
    last_error: Option<String>,
    next_temporary: u64,
}
impl OwnedFiles {
    pub(crate) fn can_release_for_recovery(&self) -> bool {
        self.slots.iter().all(|s| {
            s.file
                .as_ref()
                .is_none_or(ActualFile::can_release_for_recovery)
        }) && self.failed.iter().all(|f| f.can_release_for_recovery())
    }
    pub(crate) fn take_main(&mut self) -> Result<ActualFile> {
        let index = *self
            .names
            .get(MAIN_NAME)
            .context("Snapshot main object is missing")?;
        ensure!(
            !self.slots[index].open,
            "SQLite still owns the snapshot main object"
        );
        self.slots[index]
            .file
            .take()
            .context("Snapshot main owner is missing")
    }
    pub(crate) fn take_remaining(&mut self) -> Result<Vec<ActualFile>> {
        ensure!(
            self.slots.iter().all(|slot| !slot.open),
            "SQLite file access is still active"
        );
        let mut files = self
            .slots
            .iter_mut()
            .filter_map(|slot| slot.file.take())
            .collect::<Vec<_>>();
        files.extend(self.failed.drain(..).map(|file| *file));
        Ok(files)
    }
    pub(crate) fn last_error(&self) -> Option<&str> {
        self.last_error.as_deref()
    }
    fn retain_failure(&mut self, mut failure: NativeFailure) -> anyhow::Error {
        if let Some(file) = failure.file.take() {
            self.failed.push(file);
        }
        anyhow::anyhow!(failure.message)
    }
}
pub(crate) struct OwnedSqlite {
    connection: Option<Connection>,
    registration: Registration,
    files: Arc<Mutex<OwnedFiles>>,
}
impl OwnedSqlite {
    pub(crate) fn seed_main(
        &mut self,
        main: ActualFile,
    ) -> std::result::Result<(), (anyhow::Error, Box<ActualFile>)> {
        let mut files = self.files.lock().unwrap_or_else(|e| e.into_inner());
        let checked = (|| -> Result<()> {
            ensure!(
                self.connection.is_none() && files.slots.is_empty() && files.names.is_empty(),
                "Owned SQLite already has file access"
            );
            ensure!(
                main.metadata().directory_identity == *files.directory.identity()
                    && main.metadata().operation_id == files.operation,
                "SQLite seed belongs to another operation"
            );
            Ok(())
        })();
        if let Err(error) = checked {
            return Err((error, Box::new(main)));
        }
        files.names.insert(MAIN_NAME.into(), 0);
        files.slots.push(FileSlot {
            file: Some(main),
            open: false,
            deleted: false,
            lock: ffi::SQLITE_LOCK_NONE,
            delete_on_close: false,
        });
        Ok(())
    }
    pub(crate) fn allocate(
        directory: SnapshotDirectory,
        operation: &str,
        maximum: u64,
        context: SnapshotContext,
        permit: SnapshotPermit,
        manifest: Arc<Mutex<DurableManifest>>,
    ) -> Result<Self> {
        let files = Arc::new(Mutex::new(OwnedFiles {
            manifest,
            directory,
            operation: operation.into(),
            maximum,
            restore_main_rollback: false,
            context,
            permit,
            slots: Vec::new(),
            names: HashMap::new(),
            failed: Vec::new(),
            last_error: None,
            next_temporary: 0,
        }));
        let registration = Registration::owned(files.clone())?;
        Ok(Self {
            connection: None,
            registration,
            files,
        })
    }
    pub(crate) fn open(&mut self) -> Result<()> {
        self.open_connection()?;
        self.initialize_private_pragmas()
    }
    pub(crate) fn open_connection(&mut self) -> Result<()> {
        ensure!(self.connection.is_none(), "Owned SQLite is already open");
        let connection = Connection::open_with_flags_and_vfs(
            MAIN_NAME,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_CREATE
                | OpenFlags::SQLITE_OPEN_NO_MUTEX,
            self.registration.name.as_c_str(),
        )
        .map_err(|error| {
            let native = self.files.lock().unwrap_or_else(|e| e.into_inner());
            match native.last_error() {
                Some(detail) => anyhow::anyhow!("{error}: {detail}"),
                None => error.into(),
            }
        })?;
        self.connection = Some(connection);
        Ok(())
    }
    pub(crate) fn initialize_private_pragmas(&self) -> Result<()> {
        // The unpublished private image is disposable on every pre-seal error.
        // Avoid a full rollback journal (and its per-page/sector expansion);
        // neither a partial transaction nor a partial VACUUM can be resumed.
        self.connection()?.execute_batch("PRAGMA journal_mode=OFF; PRAGMA synchronous=FULL; PRAGMA mmap_size=0; PRAGMA cache_size=-2048; PRAGMA temp_store=FILE; PRAGMA foreign_keys=ON;")?;
        Ok(())
    }
    /// Opts this owned restore candidate into a bounded main rollback journal.
    /// The existing operation permit charges the journal; no new reservation,
    /// temporary-journal allowance, or public writer authority is introduced.
    pub(crate) fn enable_restore_rollback_journal(&self) -> Result<()> {
        ensure!(
            self.connection()?.is_autocommit(),
            "Restore rollback journal requires an idle owned connection"
        );
        let mut state = self
            .files
            .lock()
            .map_err(|_| anyhow::anyhow!("Snapshot file registry poisoned"))?;
        state.context.check()?;
        ensure!(
            !state.names.contains_key("snapshot-main-journal") && state.failed.is_empty(),
            "Restore rollback journal was already allocated or failed"
        );
        let index = *state
            .names
            .get(MAIN_NAME)
            .context("Restore rollback main owner is missing")?;
        let slot = state
            .slots
            .get(index)
            .context("Restore rollback main slot is missing")?;
        let main = slot
            .file
            .as_ref()
            .context("Restore rollback actual main is missing")?;
        main.verify(&state.context)?;
        let metadata = main.metadata();
        ensure!(
            slot.open
                && !slot.deleted
                && !metadata.published
                && metadata.operation_id == state.operation
                && metadata.directory_identity == *state.directory.identity(),
            "Restore rollback journal scope does not identify the owned main"
        );
        state.restore_main_rollback = true;
        Ok(())
    }
    pub(crate) fn connection(&self) -> Result<&Connection> {
        self.connection
            .as_ref()
            .context("Snapshot SQLite connection is closed")
    }
    pub(crate) fn connection_mut(&mut self) -> Result<&mut Connection> {
        self.connection
            .as_mut()
            .context("Snapshot SQLite connection is closed")
    }
    pub(crate) fn close(&mut self) -> Result<()> {
        if let Some(connection) = self.connection.take()
            && let Err((connection, error)) = connection.close()
        {
            self.connection = Some(connection);
            return Err(error.into());
        }
        Ok(())
    }
    pub(crate) fn files(&self) -> Arc<Mutex<OwnedFiles>> {
        self.files.clone()
    }
    pub(crate) fn take_last_error(&self) -> Option<String> {
        self.files
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .last_error
            .take()
    }
    /// SQLite backup copies the source page-one WAL format bytes unchanged.
    /// This VFS owns a standalone private image and provides no WAL/SHM files.
    /// Close the real SQLite accessor, normalize only its two format bytes on
    /// the same retained descriptor, then reopen to discard the old page cache.
    pub(crate) fn normalize_copied_journal_mode(&mut self) -> Result<()> {
        self.close()?;
        {
            let mut state = self
                .files
                .lock()
                .map_err(|_| anyhow::anyhow!("Snapshot file registry poisoned"))?;
            let context = state.context.clone();
            context.check()?;
            let index = *state
                .names
                .get(MAIN_NAME)
                .context("Copied snapshot main owner is missing")?;
            let slot = state
                .slots
                .get_mut(index)
                .context("Copied snapshot main slot is missing")?;
            ensure!(
                !slot.open && !slot.deleted,
                "SQLite still owns or deleted the copied snapshot accessor"
            );
            let file = slot
                .file
                .as_mut()
                .context("Copied snapshot native descriptor is missing")?;
            ensure!(
                !file.metadata().published,
                "Published snapshot format cannot be modified"
            );
            let mut header = [0u8; 100];
            ensure!(
                file.read_at(0, &mut header, &context)? == header.len()
                    && &header[..16] == b"SQLite format 3\0"
                    && matches!(header[18], 1 | 2)
                    && matches!(header[19], 1 | 2),
                "Copied snapshot has an invalid SQLite format header"
            );
            if header[18] != 1 || header[19] != 1 {
                ensure!(
                    file.write_at(18, &[1, 1], &context)? == 2,
                    "Snapshot format normalization made incomplete progress"
                );
                file.sync_all(&context)?;
            }
        }
        self.open()
    }
}
impl Drop for OwnedSqlite {
    fn drop(&mut self) {
        drop(self.connection.take());
    }
}

struct Registration {
    name: CString,
    vfs: Box<ffi::sqlite3_vfs>,
    #[expect(
        clippy::redundant_allocation,
        reason = "sqlite3_vfs.pAppData points to this Arc cell; its address must survive Registration moves"
    )]
    _context: Box<Arc<Mutex<OwnedFiles>>>,
}
impl Registration {
    fn owned(files: Arc<Mutex<OwnedFiles>>) -> Result<Self> {
        let mut nonce = [0u8; 16];
        getrandom::fill(&mut nonce)
            .map_err(|error| anyhow::anyhow!("Cannot allocate snapshot VFS identity: {error}"))?;
        let nonce: String = nonce.iter().map(|byte| format!("{byte:02x}")).collect();
        let name = CString::new(format!("lx-snapshot-{nonce}"))?;
        let mut context = Box::new(files);
        let parent = unsafe { ffi::sqlite3_vfs_find(ptr::null()) };
        ensure!(!parent.is_null(), "SQLite default VFS is missing");
        let mut vfs = Box::new(ffi::sqlite3_vfs {
            iVersion: 1,
            szOsFile: size_of::<OwnedFile>() as c_int,
            mxPathname: MAX_NAME as c_int,
            pNext: ptr::null_mut(),
            zName: name.as_ptr(),
            pAppData: (&mut *context as *mut Arc<Mutex<OwnedFiles>>).cast(),
            xOpen: Some(owned_open),
            xDelete: Some(owned_delete),
            xAccess: Some(owned_access),
            xFullPathname: Some(owned_full_path),
            xDlOpen: None,
            xDlError: None,
            xDlSym: None,
            xDlClose: None,
            xRandomness: Some(owned_random),
            xSleep: Some(owned_sleep),
            xCurrentTime: Some(owned_time),
            xGetLastError: None,
            xCurrentTimeInt64: None,
            xSetSystemCall: None,
            xGetSystemCall: None,
            xNextSystemCall: None,
        });
        ensure!(
            unsafe { ffi::sqlite3_vfs_register(&mut *vfs, 0) } == ffi::SQLITE_OK,
            "Cannot register snapshot actual-FD VFS"
        );
        Ok(Self {
            name,
            vfs,
            _context: context,
        })
    }
}
impl Drop for Registration {
    fn drop(&mut self) {
        unsafe { ffi::sqlite3_vfs_unregister(&mut *self.vfs) };
    }
}

#[repr(C)]
struct OwnedFile {
    base: ffi::sqlite3_file,
    context: *const Mutex<OwnedFiles>,
    index: usize,
}
static OWNED_IO: ffi::sqlite3_io_methods = ffi::sqlite3_io_methods {
    iVersion: 1,
    xClose: Some(owned_close),
    xRead: Some(owned_read),
    xWrite: Some(owned_write),
    xTruncate: Some(owned_truncate),
    xSync: Some(owned_sync),
    xFileSize: Some(owned_size),
    xLock: Some(owned_lock),
    xUnlock: Some(owned_unlock),
    xCheckReservedLock: Some(owned_reserved),
    xFileControl: Some(owned_control),
    xSectorSize: Some(owned_sector),
    xDeviceCharacteristics: Some(owned_characteristics),
    xShmMap: None,
    xShmLock: None,
    xShmBarrier: None,
    xShmUnmap: None,
    xFetch: None,
    xUnfetch: None,
};
fn callback(fallback: c_int, body: impl FnOnce() -> Result<c_int>) -> c_int {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(body)) {
        Ok(Ok(code)) => code,
        _ => fallback,
    }
}
fn owned_vfs_callback(
    vfs: *mut ffi::sqlite3_vfs,
    stage: &'static str,
    fallback: c_int,
    body: impl FnOnce() -> Result<c_int>,
) -> c_int {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(body));
    let detail = match result {
        Ok(Ok(code)) => return code,
        Ok(Err(error)) => format!("{stage}: {error:#}"),
        Err(_) => format!("{stage}: Snapshot VFS callback panicked"),
    };
    if let Ok(state) = unsafe { session(vfs) } {
        state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .last_error = Some(detail.chars().take(2048).collect());
    }
    fallback
}
unsafe fn name(pointer: *const c_char) -> Result<String> {
    ensure!(!pointer.is_null(), "SQLite symbolic filename is missing");
    let bytes = unsafe { CStr::from_ptr(pointer) }.to_bytes();
    ensure!(
        bytes.len() <= MAX_NAME,
        "SQLite symbolic filename exceeds its bound"
    );
    Ok(std::str::from_utf8(bytes)?.to_owned())
}
unsafe fn session(vfs: *mut ffi::sqlite3_vfs) -> Result<&'static Mutex<OwnedFiles>> {
    ensure!(!vfs.is_null(), "Snapshot VFS is missing");
    let arc = unsafe { &*((*vfs).pAppData.cast::<Arc<Mutex<OwnedFiles>>>()) };
    Ok(arc.as_ref())
}
unsafe fn owned(file: *mut ffi::sqlite3_file) -> Result<&'static mut OwnedFile> {
    ensure!(!file.is_null(), "Snapshot SQLite file is missing");
    Ok(unsafe { &mut *file.cast::<OwnedFile>() })
}
fn file_operation(
    file: *mut ffi::sqlite3_file,
    fallback: c_int,
    body: impl FnOnce(&mut FileSlot, &SnapshotContext) -> Result<c_int>,
) -> c_int {
    callback(fallback, || {
        let file = unsafe { owned(file) }?;
        let mut state = unsafe { &*file.context }
            .lock()
            .map_err(|_| anyhow::anyhow!("Snapshot VFS owner poisoned"))?;
        let context = state.context.clone();
        let result = (|| {
            context.check()?;
            body(
                state
                    .slots
                    .get_mut(file.index)
                    .context("Snapshot file slot is missing")?,
                &context,
            )
        })();
        if let Err(error) = &result {
            let symbolic = state
                .names
                .iter()
                .find_map(|(name, index)| (*index == file.index).then_some(name.as_str()))
                .unwrap_or("unknown snapshot file");
            state.last_error = Some(
                format!("{symbolic}: {error:#}")
                    .chars()
                    .take(2048)
                    .collect(),
            );
        }
        result
    })
}
unsafe extern "C" fn owned_open(
    vfs: *mut ffi::sqlite3_vfs,
    zname: ffi::sqlite3_filename,
    file: *mut ffi::sqlite3_file,
    flags: c_int,
    out: *mut c_int,
) -> c_int {
    owned_vfs_callback(vfs, "owned xOpen", ffi::SQLITE_CANTOPEN, || {
        ensure!(!file.is_null(), "SQLite file allocation is missing");
        unsafe { (*file).pMethods = ptr::null() };
        let mutex = unsafe { session(vfs) }?;
        let mut state = mutex
            .lock()
            .map_err(|_| anyhow::anyhow!("Snapshot VFS owner poisoned"))?;
        state.context.check()?;
        let symbolic = if zname.is_null() {
            state.next_temporary = state
                .next_temporary
                .checked_add(1)
                .context("Snapshot temporary counter exhausted")?;
            format!("snapshot-temp-{}", state.next_temporary)
        } else {
            unsafe { name(zname) }?
        };
        ensure!(
            symbolic == MAIN_NAME
                || symbolic.starts_with("snapshot-main-")
                || symbolic.starts_with("snapshot-temp-"),
            "SQLite filename is outside snapshot operation"
        );
        ensure!(
            flags & ffi::SQLITE_OPEN_WAL == 0,
            "Snapshot destination cannot open WAL"
        );
        let index = if let Some(index) = state.names.get(&symbolic).copied() {
            ensure!(
                !state.slots[index].open,
                "Snapshot file already has an active SQLite accessor"
            );
            if state.slots[index].deleted {
                let context = state.context.clone();
                state.slots[index]
                    .file
                    .as_mut()
                    .context("Snapshot file owner is missing")?
                    .set_len(0, &context)?;
                state.slots[index].deleted = false;
            }
            index
        } else {
            ensure!(
                state.slots.len() + state.failed.len() < MAX_FILES,
                "Snapshot SQLite file count exceeded"
            );
            let manifest = state.manifest.clone();
            let role = if symbolic == MAIN_NAME {
                "main"
            } else {
                "auxiliary"
            };
            let created = ActualFile::create_journaled(
                &state.directory,
                &state.operation,
                if state.restore_main_rollback
                    && symbolic == "snapshot-main-journal"
                    && flags & ffi::SQLITE_OPEN_MAIN_JOURNAL != 0
                {
                    state.maximum
                } else if flags
                    & (ffi::SQLITE_OPEN_MAIN_JOURNAL
                        | ffi::SQLITE_OPEN_TEMP_JOURNAL
                        | ffi::SQLITE_OPEN_SUBJOURNAL)
                    != 0
                {
                    // SQLite's VACUUM attaches an initially empty database;
                    // its temporary rollback journal contains a header only.
                    state.maximum.min(IO_CHUNK as u64)
                } else {
                    state.maximum
                },
                &state.context,
                &state.permit,
                &|metadata| {
                    manifest.lock().map_err(|_| anyhow::anyhow!("Snapshot manifest poisoned"))?
                        .append_checkpoint(&serde_json::to_vec(&serde_json::json!({"kind":"allocation", "role":role,"file":metadata}))?, &state.context)
                },
            );
            let owner = match created {
                Ok(owner) => owner,
                Err(failure) => return Err(state.retain_failure(failure)),
            };
            let index = state.slots.len();
            state.slots.push(FileSlot {
                file: Some(owner),
                open: false,
                deleted: false,
                lock: ffi::SQLITE_LOCK_NONE,
                delete_on_close: flags & ffi::SQLITE_OPEN_DELETEONCLOSE != 0,
            });
            state.names.insert(symbolic, index);
            index
        };
        state.slots[index].open = true;
        unsafe {
            ptr::write(
                file.cast::<OwnedFile>(),
                OwnedFile {
                    base: ffi::sqlite3_file {
                        pMethods: &OWNED_IO,
                    },
                    context: mutex,
                    index,
                },
            );
            if !out.is_null() {
                *out = flags;
            }
        }
        Ok(ffi::SQLITE_OK)
    })
}
unsafe extern "C" fn owned_close(file: *mut ffi::sqlite3_file) -> c_int {
    // Closing a logical accessor is required even after cancellation; no
    // native object is dropped or deleted by this callback.
    callback(ffi::SQLITE_IOERR_CLOSE, || {
        let file = unsafe { owned(file) }?;
        let mut state = unsafe { &*file.context }
            .lock()
            .map_err(|_| anyhow::anyhow!("Snapshot VFS owner poisoned"))?;
        let slot = state
            .slots
            .get_mut(file.index)
            .context("Snapshot file slot is missing")?;
        slot.open = false;
        slot.lock = ffi::SQLITE_LOCK_NONE;
        slot.deleted |= slot.delete_on_close;
        Ok(ffi::SQLITE_OK)
    })
}
unsafe extern "C" fn owned_read(
    file: *mut ffi::sqlite3_file,
    buffer: *mut c_void,
    amount: c_int,
    offset: i64,
) -> c_int {
    file_operation(file, ffi::SQLITE_IOERR_READ, |slot, context| {
        ensure!(
            amount >= 0 && offset >= 0 && (amount == 0 || !buffer.is_null()),
            "Invalid SQLite read range"
        );
        if amount == 0 {
            return Ok(ffi::SQLITE_OK);
        }
        let bytes = unsafe { std::slice::from_raw_parts_mut(buffer.cast::<u8>(), amount as usize) };
        let owner = slot
            .file
            .as_ref()
            .context("Snapshot read owner is missing")?;
        let mut at = 0;
        while at < bytes.len() {
            let length = (bytes.len() - at).min(IO_CHUNK);
            let read = owner.read_at(
                offset as u64 + at as u64,
                &mut bytes[at..at + length],
                context,
            )?;
            if read < length {
                bytes[at + read..].fill(0);
                return Ok(ffi::SQLITE_IOERR_SHORT_READ);
            }
            at += read;
        }
        Ok(ffi::SQLITE_OK)
    })
}
unsafe extern "C" fn owned_write(
    file: *mut ffi::sqlite3_file,
    buffer: *const c_void,
    amount: c_int,
    offset: i64,
) -> c_int {
    file_operation(file, ffi::SQLITE_IOERR_WRITE, |slot, context| {
        ensure!(
            amount >= 0 && offset >= 0 && (amount == 0 || !buffer.is_null()),
            "Invalid SQLite write range"
        );
        if amount == 0 {
            return Ok(ffi::SQLITE_OK);
        }
        let bytes = unsafe { std::slice::from_raw_parts(buffer.cast::<u8>(), amount as usize) };
        let owner = slot
            .file
            .as_mut()
            .context("Snapshot write owner is missing")?;
        let mut at = 0;
        while at < bytes.len() {
            let length = (bytes.len() - at).min(IO_CHUNK);
            let written =
                owner.write_at(offset as u64 + at as u64, &bytes[at..at + length], context)?;
            ensure!(written > 0, "Snapshot write made no progress");
            at += written;
        }
        Ok(ffi::SQLITE_OK)
    })
}
unsafe extern "C" fn owned_truncate(file: *mut ffi::sqlite3_file, length: i64) -> c_int {
    file_operation(file, ffi::SQLITE_IOERR_TRUNCATE, |slot, context| {
        ensure!(length >= 0, "Invalid SQLite truncate length");
        slot.file
            .as_mut()
            .context("Snapshot truncate owner is missing")?
            .set_len(length as u64, context)?;
        Ok(ffi::SQLITE_OK)
    })
}
unsafe extern "C" fn owned_sync(file: *mut ffi::sqlite3_file, _: c_int) -> c_int {
    file_operation(file, ffi::SQLITE_IOERR_FSYNC, |slot, context| {
        slot.file
            .as_ref()
            .context("Snapshot sync owner is missing")?
            .sync_all(context)?;
        Ok(ffi::SQLITE_OK)
    })
}
unsafe extern "C" fn owned_size(file: *mut ffi::sqlite3_file, size: *mut i64) -> c_int {
    file_operation(file, ffi::SQLITE_IOERR_FSTAT, |slot, context| {
        ensure!(!size.is_null(), "SQLite size pointer is missing");
        unsafe {
            *size = i64::try_from(
                slot.file
                    .as_ref()
                    .context("Snapshot size owner is missing")?
                    .len(context)?,
            )?;
        }
        Ok(ffi::SQLITE_OK)
    })
}
unsafe extern "C" fn owned_lock(file: *mut ffi::sqlite3_file, level: c_int) -> c_int {
    file_operation(file, ffi::SQLITE_IOERR_LOCK, |slot, _| {
        slot.lock = level;
        Ok(ffi::SQLITE_OK)
    })
}
unsafe extern "C" fn owned_unlock(file: *mut ffi::sqlite3_file, level: c_int) -> c_int {
    callback(ffi::SQLITE_IOERR_UNLOCK, || {
        let file = unsafe { owned(file) }?;
        let mut state = unsafe { &*file.context }
            .lock()
            .map_err(|_| anyhow::anyhow!("Snapshot owner poisoned"))?;
        state
            .slots
            .get_mut(file.index)
            .context("Snapshot lock slot missing")?
            .lock = level;
        Ok(ffi::SQLITE_OK)
    })
}
unsafe extern "C" fn owned_reserved(file: *mut ffi::sqlite3_file, result: *mut c_int) -> c_int {
    file_operation(file, ffi::SQLITE_IOERR_CHECKRESERVEDLOCK, |slot, _| {
        ensure!(!result.is_null(), "SQLite reserved-lock pointer is missing");
        unsafe { *result = c_int::from(slot.lock >= ffi::SQLITE_LOCK_RESERVED) };
        Ok(ffi::SQLITE_OK)
    })
}
unsafe extern "C" fn owned_control(
    _: *mut ffi::sqlite3_file,
    operation: c_int,
    _: *mut c_void,
) -> c_int {
    if matches!(
        operation,
        ffi::SQLITE_FCNTL_SYNC | ffi::SQLITE_FCNTL_COMMIT_PHASETWO
    ) {
        ffi::SQLITE_OK
    } else {
        ffi::SQLITE_NOTFOUND
    }
}
unsafe extern "C" fn owned_sector(_: *mut ffi::sqlite3_file) -> c_int {
    4096
}
unsafe extern "C" fn owned_characteristics(_: *mut ffi::sqlite3_file) -> c_int {
    0
}
unsafe extern "C" fn owned_random(
    _: *mut ffi::sqlite3_vfs,
    amount: c_int,
    out: *mut c_char,
) -> c_int {
    callback(0, || {
        let parent = unsafe { ffi::sqlite3_vfs_find(ptr::null()) };
        ensure!(!parent.is_null(), "Native VFS missing");
        Ok(unsafe {
            (*parent).xRandomness.context("Native randomness missing")?(parent, amount, out)
        })
    })
}
unsafe extern "C" fn owned_sleep(_: *mut ffi::sqlite3_vfs, micros: c_int) -> c_int {
    callback(0, || {
        let parent = unsafe { ffi::sqlite3_vfs_find(ptr::null()) };
        ensure!(!parent.is_null(), "Native VFS missing");
        Ok(unsafe { (*parent).xSleep.context("Native sleep missing")?(parent, micros) })
    })
}
unsafe extern "C" fn owned_time(_: *mut ffi::sqlite3_vfs, out: *mut f64) -> c_int {
    callback(ffi::SQLITE_ERROR, || {
        let parent = unsafe { ffi::sqlite3_vfs_find(ptr::null()) };
        ensure!(!parent.is_null(), "Native VFS missing");
        Ok(unsafe { (*parent).xCurrentTime.context("Native time missing")?(parent, out) })
    })
}
unsafe extern "C" fn owned_delete(
    vfs: *mut ffi::sqlite3_vfs,
    zname: *const c_char,
    _: c_int,
) -> c_int {
    callback(ffi::SQLITE_IOERR_DELETE, || {
        let symbolic = unsafe { name(zname) }?;
        let mut state = unsafe { session(vfs) }?
            .lock()
            .map_err(|_| anyhow::anyhow!("Snapshot VFS owner poisoned"))?;
        state.context.check()?;
        if let Some(index) = state.names.get(&symbolic).copied() {
            ensure!(
                !state.slots[index].open,
                "SQLite deletes a still-open snapshot object"
            );
            state.slots[index].deleted = true;
        }
        Ok(ffi::SQLITE_OK)
    })
}
unsafe extern "C" fn owned_access(
    vfs: *mut ffi::sqlite3_vfs,
    zname: *const c_char,
    _: c_int,
    out: *mut c_int,
) -> c_int {
    callback(ffi::SQLITE_IOERR_ACCESS, || {
        ensure!(!out.is_null(), "SQLite access output is missing");
        let symbolic = unsafe { name(zname) }?;
        let state = unsafe { session(vfs) }?
            .lock()
            .map_err(|_| anyhow::anyhow!("Snapshot VFS owner poisoned"))?;
        state.context.check()?;
        let exists = state
            .names
            .get(&symbolic)
            .is_some_and(|index| !state.slots[*index].deleted);
        unsafe { *out = c_int::from(exists) };
        Ok(ffi::SQLITE_OK)
    })
}
unsafe extern "C" fn owned_full_path(
    vfs: *mut ffi::sqlite3_vfs,
    zname: *const c_char,
    capacity: c_int,
    out: *mut c_char,
) -> c_int {
    owned_vfs_callback(vfs, "owned xFullPathname", ffi::SQLITE_CANTOPEN, || {
        let symbolic = unsafe { name(zname) }?;
        ensure!(
            !out.is_null() && capacity > 0 && symbolic.len() < capacity as usize,
            "SQLite symbolic name output exceeds its bound"
        );
        unsafe {
            ptr::copy_nonoverlapping(symbolic.as_ptr().cast::<c_char>(), out, symbolic.len());
            *out.add(symbolic.len()) = 0;
        }
        Ok(ffi::SQLITE_OK)
    })
}

/// Independent default-VFS reader. It keeps SQLite's native WAL/SHM lock
/// implementation and read endmark, while proving every returned native file.
pub(crate) struct SourceSqlite {
    connection: Option<Connection>,
    registration: SourceRegistration,
}
pub(crate) struct SourceAnchors {
    pub main: SourceFileProof,
    pub wal: SourceFileProof,
    pub shm: SourceFileProof,
}
struct SourceState {
    anchors: SourceAnchors,
    context: SnapshotContext,
    parent: *mut ffi::sqlite3_vfs,
    baseline_wal_bytes: u64,
    last_error: Mutex<Option<String>>,
}
impl SourceState {
    fn diagnostic_error(&self, error: impl fmt::Display) -> anyhow::Error {
        match self
            .last_error
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .as_ref()
        {
            Some(detail) => anyhow::anyhow!("{error}: {detail}"),
            None => anyhow::anyhow!("{error}"),
        }
    }
    fn check(&self) -> Result<()> {
        self.context.check()?;
        self.anchors.main.verify()?;
        self.anchors.wal.verify()?;
        self.anchors.shm.verify()?;
        super::snapshot_native::enforce_wal_growth(
            self.anchors.wal.len()?,
            self.baseline_wal_bytes,
        )?;
        self.context.check()
    }
    fn proof(&self, path: &str) -> Result<&SourceFileProof> {
        let actual = crate::domain::music::normalize_local_path(std::path::Path::new(path));
        for proof in [&self.anchors.main, &self.anchors.wal] {
            if crate::domain::music::normalize_local_path(&proof.metadata()?.path) == actual {
                return Ok(proof);
            }
        }
        anyhow::bail!("SQLite source filename is outside its actual native anchors")
    }
}
impl SourceSqlite {
    pub(crate) fn open(anchors: SourceAnchors, context: SnapshotContext) -> Result<Self> {
        let path = anchors.main.metadata()?.path.clone();
        let registration = SourceRegistration::new(anchors, context)?;
        let connection = Connection::open_with_flags_and_vfs(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
            registration.name.as_c_str(),
        )
        .map_err(|error| registration.state.diagnostic_error(error))?;
        connection
            .execute_batch("PRAGMA query_only=ON; PRAGMA mmap_size=0; PRAGMA cache_size=-2048;")
            .map_err(|error| registration.state.diagnostic_error(error))?;
        Ok(Self {
            connection: Some(connection),
            registration,
        })
    }
    pub(crate) fn connection(&self) -> Result<&Connection> {
        self.connection
            .as_ref()
            .context("Snapshot read connection is closed")
    }
    pub(crate) fn check(&self) -> Result<()> {
        self.registration.state.check()
    }
    pub(crate) fn take_last_error(&self) -> Option<String> {
        self.registration
            .state
            .last_error
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take()
    }
    pub(crate) fn close(&mut self) -> Result<()> {
        if let Some(connection) = self.connection.take()
            && let Err((connection, error)) = connection.close()
        {
            self.connection = Some(connection);
            return Err(error.into());
        }
        Ok(())
    }
}
impl Drop for SourceSqlite {
    fn drop(&mut self) {
        drop(self.connection.take());
    }
}
struct SourceRegistration {
    name: CString,
    vfs: Box<ffi::sqlite3_vfs>,
    state: Box<SourceState>,
}
impl SourceRegistration {
    fn new(anchors: SourceAnchors, context: SnapshotContext) -> Result<Self> {
        let parent = unsafe { ffi::sqlite3_vfs_find(ptr::null()) };
        ensure!(!parent.is_null(), "SQLite default source VFS is missing");
        let mut nonce = [0u8; 16];
        getrandom::fill(&mut nonce)
            .map_err(|error| anyhow::anyhow!("Cannot allocate source VFS identity: {error}"))?;
        let nonce: String = nonce.iter().map(|byte| format!("{byte:02x}")).collect();
        let name = CString::new(format!("lx-snapshot-read-{nonce}"))?;
        let baseline_wal_bytes = anchors.wal.len()?;
        let mut state = Box::new(SourceState {
            anchors,
            context,
            parent,
            baseline_wal_bytes,
            last_error: Mutex::new(None),
        });
        state.check()?;
        let mut vfs = Box::new(unsafe { *parent });
        vfs.pNext = ptr::null_mut();
        vfs.zName = name.as_ptr();
        vfs.pAppData = (&mut *state as *mut SourceState).cast();
        vfs.szOsFile = vfs
            .szOsFile
            .checked_add(size_of::<SourceFile>() as c_int)
            .context("Source VFS file allocation overflow")?;
        vfs.xOpen = Some(source_open);
        vfs.xDelete = Some(source_delete);
        vfs.xAccess = Some(source_access);
        vfs.xFullPathname = Some(source_full_path);
        vfs.xDlOpen = None;
        vfs.xDlError = None;
        vfs.xDlSym = None;
        vfs.xDlClose = None;
        vfs.xRandomness = Some(source_random);
        vfs.xSleep = Some(source_sleep);
        vfs.xCurrentTime = Some(source_time);
        vfs.xCurrentTimeInt64 = Some(source_time64);
        vfs.xGetLastError = None;
        vfs.xSetSystemCall = None;
        vfs.xGetSystemCall = None;
        vfs.xNextSystemCall = None;
        ensure!(
            unsafe { ffi::sqlite3_vfs_register(&mut *vfs, 0) } == ffi::SQLITE_OK,
            "Cannot register native-proved source VFS"
        );
        Ok(Self { name, vfs, state })
    }
}
impl Drop for SourceRegistration {
    fn drop(&mut self) {
        unsafe { ffi::sqlite3_vfs_unregister(&mut *self.vfs) };
    }
}
#[repr(C)]
struct SourceFile {
    base: ffi::sqlite3_file,
    inner: *mut ffi::sqlite3_file,
    state: *const SourceState,
    main: bool,
}
unsafe fn source_state(vfs: *mut ffi::sqlite3_vfs) -> Result<&'static SourceState> {
    ensure!(!vfs.is_null(), "Source VFS is missing");
    Ok(unsafe { &*(*vfs).pAppData.cast::<SourceState>() })
}
fn source_vfs_callback(
    vfs: *mut ffi::sqlite3_vfs,
    stage: &'static str,
    fallback: c_int,
    body: impl FnOnce() -> Result<c_int>,
) -> c_int {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(body));
    let detail = match result {
        Ok(Ok(code)) if code == ffi::SQLITE_OK => return code,
        Ok(Ok(code)) => {
            if let Ok(state) = unsafe { source_state(vfs) } {
                let mut last = state
                    .last_error
                    .lock()
                    .unwrap_or_else(|error| error.into_inner());
                if last.is_none() {
                    *last = Some(format!("{stage}: native SQLite result {code}"));
                }
            }
            return code;
        }
        Ok(Err(error)) => format!("{stage}: {error:#}"),
        Err(_) => format!("{stage}: Source VFS callback panicked"),
    };
    if let Ok(state) = unsafe { source_state(vfs) } {
        *state
            .last_error
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(detail.chars().take(2048).collect());
    }
    fallback
}
unsafe fn source_file(file: *mut ffi::sqlite3_file) -> Result<&'static SourceFile> {
    ensure!(!file.is_null(), "Source SQLite file is missing");
    let file = unsafe { &*file.cast::<SourceFile>() };
    ensure!(
        !file.inner.is_null()
            && !file.state.is_null()
            && unsafe { !(*file.inner).pMethods.is_null() },
        "Source SQLite accessor has no initialized native owner"
    );
    Ok(file)
}
fn source_operation(
    file: *mut ffi::sqlite3_file,
    fallback: c_int,
    body: impl FnOnce(&SourceFile, &ffi::sqlite3_io_methods) -> Result<c_int>,
) -> c_int {
    callback(fallback, || {
        let file = unsafe { source_file(file) }?;
        let state = unsafe { &*file.state };
        state.check()?;
        let proof = if file.main {
            &state.anchors.main
        } else {
            &state.anchors.wal
        };
        unsafe { proof.verify_sqlite_file(file.inner) }?;
        let methods = unsafe {
            file.inner
                .as_ref()
                .and_then(|inner| inner.pMethods.as_ref())
        }
        .context("Source native I/O methods are missing")?;
        let result = body(file, methods)?;
        state.check()?;
        unsafe { proof.verify_sqlite_file(file.inner) }?;
        Ok(result)
    })
}
unsafe extern "C" fn source_open(
    vfs: *mut ffi::sqlite3_vfs,
    path: ffi::sqlite3_filename,
    file: *mut ffi::sqlite3_file,
    flags: c_int,
    out: *mut c_int,
) -> c_int {
    source_vfs_callback(vfs, "source xOpen", ffi::SQLITE_CANTOPEN, || {
        ensure!(!file.is_null(), "Source SQLite file allocation is missing");
        unsafe { (*file).pMethods = ptr::null() };
        let state = unsafe { source_state(vfs) }?;
        state.check()?;
        let path_text = unsafe { name(path) }?;
        let proof = state.proof(&path_text)?;
        let inner = unsafe {
            file.cast::<u8>()
                .add(size_of::<SourceFile>())
                .cast::<ffi::sqlite3_file>()
        };
        let open =
            unsafe { (*state.parent).xOpen }.context("Default source VFS cannot open a file")?;
        let flags = (flags & !ffi::SQLITE_OPEN_CREATE & !ffi::SQLITE_OPEN_READWRITE)
            | ffi::SQLITE_OPEN_READONLY;
        let code = unsafe { open(state.parent, path, inner, flags, out) };
        if code != ffi::SQLITE_OK {
            let native_error = std::io::Error::last_os_error();
            *state
                .last_error
                .lock()
                .unwrap_or_else(|error| error.into_inner()) = Some(format!(
                "source xOpen: default VFS result {code}, flags {flags:#x}, native error {native_error}"
            ));
            if let Some(close) = unsafe {
                inner
                    .as_ref()
                    .and_then(|f| f.pMethods.as_ref())
                    .and_then(|m| m.xClose)
            } {
                unsafe { close(inner) };
            }
            return Ok(code);
        }
        let checked = (|| -> Result<()> {
            unsafe { proof.verify_sqlite_file(inner) }?;
            state.check()?;
            Ok(())
        })();
        if let Err(error) = checked {
            if let Some(close) = unsafe {
                inner
                    .as_ref()
                    .and_then(|f| f.pMethods.as_ref())
                    .and_then(|m| m.xClose)
            } {
                unsafe { close(inner) };
            }
            return Err(error);
        }
        let is_main = std::ptr::eq(proof, &state.anchors.main);
        unsafe {
            ptr::write(
                file.cast::<SourceFile>(),
                SourceFile {
                    base: ffi::sqlite3_file {
                        pMethods: &SOURCE_IO,
                    },
                    inner,
                    state,
                    main: is_main,
                },
            );
        }
        Ok(ffi::SQLITE_OK)
    })
}
static SOURCE_IO: ffi::sqlite3_io_methods = ffi::sqlite3_io_methods {
    iVersion: 3,
    xClose: Some(source_close),
    xRead: Some(source_read),
    xWrite: Some(source_write),
    xTruncate: Some(source_truncate),
    xSync: Some(source_sync),
    xFileSize: Some(source_size),
    xLock: Some(source_lock),
    xUnlock: Some(source_unlock),
    xCheckReservedLock: Some(source_reserved),
    xFileControl: Some(source_control),
    xSectorSize: Some(source_sector),
    xDeviceCharacteristics: Some(source_characteristics),
    xShmMap: Some(source_shm_map),
    xShmLock: Some(source_shm_lock),
    xShmBarrier: Some(source_shm_barrier),
    xShmUnmap: Some(source_shm_unmap),
    xFetch: None,
    xUnfetch: None,
};
unsafe extern "C" fn source_close(file: *mut ffi::sqlite3_file) -> c_int {
    callback(ffi::SQLITE_IOERR_CLOSE, || {
        let file = unsafe { source_file(file) }?;
        let close =
            unsafe { (*(*file.inner).pMethods).xClose }.context("Source close method missing")?;
        Ok(unsafe { close(file.inner) })
    })
}
unsafe extern "C" fn source_read(
    file: *mut ffi::sqlite3_file,
    buffer: *mut c_void,
    amount: c_int,
    offset: i64,
) -> c_int {
    source_operation(file, ffi::SQLITE_IOERR_READ, |file, methods| {
        ensure!(
            amount >= 0
                && offset >= 0
                && offset.checked_add(i64::from(amount)).is_some()
                && (amount == 0 || !buffer.is_null()),
            "Invalid source read range"
        );
        let read = methods.xRead.context("Source read method missing")?;
        let mut at = 0;
        while at < amount as usize {
            let state = unsafe { &*file.state };
            state.check()?;
            let length = (amount as usize - at).min(IO_CHUNK);
            let code = unsafe {
                read(
                    file.inner,
                    buffer.cast::<u8>().add(at).cast(),
                    length as c_int,
                    offset + at as i64,
                )
            };
            if code != ffi::SQLITE_OK {
                if code == ffi::SQLITE_IOERR_SHORT_READ {
                    unsafe {
                        ptr::write_bytes(
                            buffer.cast::<u8>().add(at + length),
                            0,
                            amount as usize - at - length,
                        )
                    };
                }
                return Ok(code);
            }
            at += length;
            state.check()?;
        }
        Ok(ffi::SQLITE_OK)
    })
}
unsafe extern "C" fn source_write(
    _: *mut ffi::sqlite3_file,
    _: *const c_void,
    _: c_int,
    _: i64,
) -> c_int {
    ffi::SQLITE_READONLY
}
unsafe extern "C" fn source_truncate(_: *mut ffi::sqlite3_file, _: i64) -> c_int {
    ffi::SQLITE_READONLY
}
unsafe extern "C" fn source_sync(file: *mut ffi::sqlite3_file, flags: c_int) -> c_int {
    source_operation(file, ffi::SQLITE_IOERR_FSYNC, |file, m| {
        Ok(unsafe { m.xSync.context("Source sync missing")?(file.inner, flags) })
    })
}
unsafe extern "C" fn source_size(file: *mut ffi::sqlite3_file, size: *mut i64) -> c_int {
    source_operation(file, ffi::SQLITE_IOERR_FSTAT, |file, m| {
        Ok(unsafe { m.xFileSize.context("Source size missing")?(file.inner, size) })
    })
}
unsafe extern "C" fn source_lock(file: *mut ffi::sqlite3_file, level: c_int) -> c_int {
    source_operation(file, ffi::SQLITE_IOERR_LOCK, |file, m| {
        Ok(unsafe { m.xLock.context("Source lock missing")?(file.inner, level) })
    })
}
unsafe extern "C" fn source_unlock(file: *mut ffi::sqlite3_file, level: c_int) -> c_int {
    callback(ffi::SQLITE_IOERR_UNLOCK, || {
        let file = unsafe { source_file(file) }?;
        Ok(unsafe {
            (*(*file.inner).pMethods)
                .xUnlock
                .context("Source unlock missing")?(file.inner, level)
        })
    })
}
unsafe extern "C" fn source_reserved(file: *mut ffi::sqlite3_file, out: *mut c_int) -> c_int {
    source_operation(file, ffi::SQLITE_IOERR_CHECKRESERVEDLOCK, |file, m| {
        Ok(unsafe {
            m.xCheckReservedLock
                .context("Source reserved lock missing")?(file.inner, out)
        })
    })
}
unsafe extern "C" fn source_control(
    file: *mut ffi::sqlite3_file,
    operation: c_int,
    arg: *mut c_void,
) -> c_int {
    source_operation(file, ffi::SQLITE_IOERR, |file, m| {
        Ok(unsafe {
            m.xFileControl.context("Source file control missing")?(file.inner, operation, arg)
        })
    })
}
unsafe extern "C" fn source_sector(file: *mut ffi::sqlite3_file) -> c_int {
    source_operation(file, 4096, |file, m| {
        Ok(unsafe { m.xSectorSize.context("Source sector size missing")?(file.inner) })
    })
}
unsafe extern "C" fn source_characteristics(file: *mut ffi::sqlite3_file) -> c_int {
    source_operation(file, 0, |file, m| {
        Ok(unsafe {
            m.xDeviceCharacteristics
                .context("Source characteristics missing")?(file.inner)
        })
    })
}
unsafe extern "C" fn source_shm_map(
    file: *mut ffi::sqlite3_file,
    page: c_int,
    size: c_int,
    extend: c_int,
    out: *mut *mut c_void,
) -> c_int {
    source_operation(file, ffi::SQLITE_IOERR_SHMMAP, |file, m| {
        let state = unsafe { &*file.state };
        state.anchors.shm.verify()?;
        let result = unsafe {
            m.xShmMap
                .context("Default source VFS has no genuine WAL mapping")?(
                file.inner, page, size, extend, out,
            )
        };
        state.anchors.shm.verify()?;
        Ok(result)
    })
}
unsafe extern "C" fn source_shm_lock(
    file: *mut ffi::sqlite3_file,
    offset: c_int,
    count: c_int,
    flags: c_int,
) -> c_int {
    // WAL lock release remains available when an operation is cancelled.
    if flags & ffi::SQLITE_SHM_UNLOCK != 0 {
        return callback(ffi::SQLITE_IOERR_SHMLOCK, || {
            let file = unsafe { source_file(file) }?;
            Ok(unsafe {
                (*(*file.inner).pMethods)
                    .xShmLock
                    .context("Source WAL unlock missing")?(
                    file.inner, offset, count, flags
                )
            })
        });
    }
    source_operation(file, ffi::SQLITE_IOERR_SHMLOCK, |file, m| {
        Ok(unsafe {
            m.xShmLock
                .context("Default source VFS has no genuine WAL locks")?(
                file.inner, offset, count, flags,
            )
        })
    })
}
unsafe extern "C" fn source_shm_barrier(file: *mut ffi::sqlite3_file) {
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        if let Ok(file) = unsafe { source_file(file) }
            && let Some(barrier) = unsafe { (*(*file.inner).pMethods).xShmBarrier }
        {
            unsafe { barrier(file.inner) }
        }
    }));
}
unsafe extern "C" fn source_shm_unmap(file: *mut ffi::sqlite3_file, _: c_int) -> c_int {
    callback(ffi::SQLITE_IOERR_SHMMAP, || {
        let file = unsafe { source_file(file) }?;
        Ok(unsafe {
            (*(*file.inner).pMethods)
                .xShmUnmap
                .context("Source WAL unmap missing")?(file.inner, 0)
        })
    })
}
unsafe extern "C" fn source_delete(_: *mut ffi::sqlite3_vfs, _: *const c_char, _: c_int) -> c_int {
    ffi::SQLITE_READONLY
}
unsafe extern "C" fn source_access(
    vfs: *mut ffi::sqlite3_vfs,
    path: *const c_char,
    flags: c_int,
    out: *mut c_int,
) -> c_int {
    callback(ffi::SQLITE_IOERR_ACCESS, || {
        let state = unsafe { source_state(vfs) }?;
        state.check()?;
        let text = unsafe { name(path) }?;
        if text.ends_with("-journal") {
            ensure!(!out.is_null(), "Source access output missing");
            unsafe { *out = 0 };
            return Ok(ffi::SQLITE_OK);
        }
        let _ = state.proof(&text)?;
        Ok(unsafe {
            (*state.parent)
                .xAccess
                .context("Source access method missing")?(state.parent, path, flags, out)
        })
    })
}
unsafe extern "C" fn source_full_path(
    vfs: *mut ffi::sqlite3_vfs,
    path: *const c_char,
    size: c_int,
    out: *mut c_char,
) -> c_int {
    source_vfs_callback(vfs, "source xFullPathname", ffi::SQLITE_CANTOPEN, || {
        let state = unsafe { source_state(vfs) }?;
        state.check()?;
        let _ = state.proof(&unsafe { name(path) }?)?;
        Ok(unsafe {
            (*state.parent)
                .xFullPathname
                .context("Source full path missing")?(state.parent, path, size, out)
        })
    })
}
unsafe extern "C" fn source_random(
    vfs: *mut ffi::sqlite3_vfs,
    size: c_int,
    out: *mut c_char,
) -> c_int {
    callback(0, || {
        let state = unsafe { source_state(vfs) }?;
        Ok(unsafe {
            (*state.parent)
                .xRandomness
                .context("Source randomness missing")?(state.parent, size, out)
        })
    })
}
unsafe extern "C" fn source_sleep(vfs: *mut ffi::sqlite3_vfs, micros: c_int) -> c_int {
    callback(0, || {
        let state = unsafe { source_state(vfs) }?;
        Ok(
            unsafe {
                (*state.parent).xSleep.context("Source sleep missing")?(state.parent, micros)
            },
        )
    })
}
unsafe extern "C" fn source_time(vfs: *mut ffi::sqlite3_vfs, out: *mut f64) -> c_int {
    callback(ffi::SQLITE_ERROR, || {
        let state = unsafe { source_state(vfs) }?;
        Ok(unsafe {
            (*state.parent)
                .xCurrentTime
                .context("Source time missing")?(state.parent, out)
        })
    })
}
unsafe extern "C" fn source_time64(vfs: *mut ffi::sqlite3_vfs, out: *mut i64) -> c_int {
    callback(ffi::SQLITE_ERROR, || {
        let state = unsafe { source_state(vfs) }?;
        Ok(unsafe {
            (*state.parent)
                .xCurrentTimeInt64
                .context("Source high precision time missing")?(state.parent, out)
        })
    })
}
#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use crate::repository::snapshot_native::{SnapshotLimiter, SnapshotPhase};
    use std::time::Duration;
    fn owned_fixture() -> (tempfile::TempDir, OwnedSqlite, Arc<Mutex<DurableManifest>>) {
        owned_fixture_with_maximum(1024 * 1024)
    }
    fn owned_fixture_with_maximum(
        maximum: u64,
    ) -> (tempfile::TempDir, OwnedSqlite, Arc<Mutex<DurableManifest>>) {
        let folder = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
        let directory = SnapshotDirectory::open_private(folder.path(), 1).unwrap();
        let limiter = SnapshotLimiter::new(1024 * 1024 * 1024).unwrap();
        let permit = limiter
            .reserve_job(maximum, 3 * maximum + 65 * 1024 * 1024)
            .unwrap();
        let context = SnapshotContext::new(Duration::from_secs(120)).unwrap();
        let mut manifest =
            DurableManifest::create(&directory, "vfs-fixture", &context, &permit).unwrap();
        manifest
            .append_once(SnapshotPhase::Created, b"{}", &context)
            .unwrap();
        let manifest = Arc::new(Mutex::new(manifest));
        let owner = OwnedSqlite::allocate(
            directory,
            "vfs-fixture",
            maximum,
            context,
            permit,
            manifest.clone(),
        )
        .unwrap();
        (folder, owner, manifest)
    }
    #[test]
    fn actual_hot_wal_65_mib_cut_reopens_private_rollback_image_without_source_changes() {
        const PAYLOAD_BYTES: i64 = 65 * 1024 * 1024;
        let (folder, mut owner, _manifest) = owned_fixture_with_maximum(80 * 1024 * 1024);
        let source_path = folder.path().join("含 空格-hot-wal.sqlite3");
        let source = Connection::open(&source_path).unwrap();
        source.execute_batch("PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0; CREATE TABLE payload(value BLOB);").unwrap();
        source
            .execute("INSERT INTO payload VALUES(zeroblob(?1))", [PAYLOAD_BYTES])
            .unwrap();
        let source_main_bytes = std::fs::metadata(&source_path).unwrap().len();
        let wal_path = folder.path().join("含 空格-hot-wal.sqlite3-wal");
        let source_wal_bytes = std::fs::metadata(&wal_path).unwrap().len();
        assert!(source_main_bytes < PAYLOAD_BYTES as u64);
        assert!(source_wal_bytes > PAYLOAD_BYTES as u64);
        let main =
            unsafe { SourceFileProof::from_sqlite_main(source.handle(), &source_path) }.unwrap();
        let mut wal_handle: *mut ffi::sqlite3_file = ptr::null_mut();
        assert_eq!(
            unsafe {
                ffi::sqlite3_file_control(
                    source.handle(),
                    c"main".as_ptr(),
                    ffi::SQLITE_FCNTL_JOURNAL_POINTER,
                    (&mut wal_handle as *mut *mut ffi::sqlite3_file).cast(),
                )
            },
            ffi::SQLITE_OK
        );
        assert!(!wal_handle.is_null());
        let wal = unsafe { SourceFileProof::from_sqlite_file(wal_handle, &wal_path) }.unwrap();
        let shm =
            SourceFileProof::pin_checked_sidecar(&main, "含 空格-hot-wal.sqlite3-shm").unwrap();
        let context = owner.files.lock().unwrap().context.clone();
        let mut reader =
            SourceSqlite::open(SourceAnchors { main, wal, shm }, context.clone()).unwrap();
        reader
            .connection()
            .unwrap()
            .execute_batch("BEGIN DEFERRED")
            .unwrap();
        assert_eq!(
            reader
                .connection()
                .unwrap()
                .query_row("SELECT length(value) FROM payload", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            PAYLOAD_BYTES
        );
        owner.open().unwrap();
        {
            let backup = rusqlite::backup::Backup::new(
                reader.connection().unwrap(),
                owner.connection_mut().unwrap(),
            )
            .unwrap();
            loop {
                context.check().unwrap();
                reader.check().unwrap();
                match backup.step(16).unwrap() {
                    rusqlite::backup::StepResult::Done => break,
                    rusqlite::backup::StepResult::More => {}
                    other => panic!("Unexpected bounded backup result: {other:?}"),
                }
            }
        }
        reader
            .connection()
            .unwrap()
            .execute_batch("ROLLBACK")
            .unwrap();
        reader.close().unwrap();
        owner.normalize_copied_journal_mode().unwrap();
        let (length, first, last): (i64, Vec<u8>, Vec<u8>) = owner
            .connection()
            .unwrap()
            .query_row(
                "SELECT length(value),substr(value,1,65536),substr(value,-65536) FROM payload",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(length, PAYLOAD_BYTES);
        assert_eq!(first, vec![0u8; 65536]);
        assert_eq!(last, vec![0u8; 65536]);
        assert_eq!(
            owner
                .connection()
                .unwrap()
                .query_row("PRAGMA integrity_check", [], |row| row.get::<_, String>(0))
                .unwrap(),
            "ok"
        );
        owner.close().unwrap();
        assert_eq!(
            std::fs::metadata(&source_path).unwrap().len(),
            source_main_bytes
        );
        assert_eq!(
            std::fs::metadata(&wal_path).unwrap().len(),
            source_wal_bytes
        );
        assert_eq!(
            source
                .query_row("PRAGMA journal_mode", [], |row| row.get::<_, String>(0))
                .unwrap(),
            "wal"
        );
        let mut files = owner.files.lock().unwrap();
        let main = files.take_main().unwrap();
        let mut mode = [0u8; 2];
        assert_eq!(main.read_at(18, &mut mode, &files.context).unwrap(), 2);
        assert_eq!(mode, [1, 1]);
        assert!(main.len(&files.context).unwrap() > PAYLOAD_BYTES as u64);
        main.cleanup(&files.context).unwrap();
        for file in files.take_remaining().unwrap() {
            file.cleanup(&files.context).unwrap();
        }
    }
    #[test]
    fn copied_wal_database_is_normalized_on_same_owned_fd_before_sqlite_reopen() {
        let (folder, mut owner, _manifest) = owned_fixture();
        let source = Connection::open(folder.path().join("wal-source.sqlite3")).unwrap();
        source.execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE payload(value TEXT); INSERT INTO payload VALUES('copied-hot-wal');").unwrap();
        owner.open().unwrap();
        {
            let backup =
                rusqlite::backup::Backup::new(&source, owner.connection_mut().unwrap()).unwrap();
            assert_eq!(backup.step(-1).unwrap(), rusqlite::backup::StepResult::Done);
        }
        owner.close().unwrap();
        let original_identity = {
            let files = owner.files.lock().unwrap();
            let slot = &files.slots[*files.names.get(MAIN_NAME).unwrap()];
            let file = slot.file.as_ref().unwrap();
            let mut mode = [0u8; 2];
            file.read_at(18, &mut mode, &files.context).unwrap();
            assert_eq!(
                mode,
                [2, 2],
                "Actual SQLite backup retains the source WAL header"
            );
            file.metadata().identity.unwrap()
        };
        owner.normalize_copied_journal_mode().unwrap();
        assert_eq!(
            owner
                .connection()
                .unwrap()
                .query_row("SELECT value FROM payload", [], |row| row
                    .get::<_, String>(0))
                .unwrap(),
            "copied-hot-wal"
        );
        owner.connection().unwrap().execute_batch("VACUUM").unwrap();
        assert_eq!(
            owner
                .connection()
                .unwrap()
                .query_row("PRAGMA integrity_check", [], |row| row.get::<_, String>(0))
                .unwrap(),
            "ok"
        );
        owner.close().unwrap();
        let mut files = owner.files.lock().unwrap();
        let main = files.take_main().unwrap();
        assert_eq!(main.metadata().identity, Some(original_identity));
        let mut mode = [0u8; 2];
        main.read_at(18, &mut mode, &files.context).unwrap();
        assert_eq!(mode, [1, 1]);
        main.cleanup(&files.context).unwrap();
        for file in files.take_remaining().unwrap() {
            file.cleanup(&files.context).unwrap();
        }
    }
    #[test]
    fn sqlite_pages_use_same_actual_private_fh_and_inventory_before_body() {
        let (_folder, mut owner, manifest) = owned_fixture();
        owner.open().unwrap();
        assert_eq!(
            owner
                .connection()
                .unwrap()
                .pragma_query_value::<String, _>(None, "journal_mode", |r| r.get(0))
                .unwrap(),
            "off"
        );
        owner
            .connection()
            .unwrap()
            .execute_batch(
                "CREATE TABLE payload(data BLOB);INSERT INTO payload VALUES(zeroblob(98304));",
            )
            .unwrap();
        let (nonce, identity) = {
            let registry = owner.files.lock().unwrap();
            let index = registry.names[MAIN_NAME];
            let file = registry.slots[index].file.as_ref().unwrap().metadata();
            (file.nonce, file.identity.unwrap())
        };
        owner.close().unwrap();
        let files = owner.files();
        drop(owner);
        let mut registry = files.lock().unwrap();
        let main = registry.take_main().unwrap();
        assert_eq!(main.metadata().identity, Some(identity));
        let context = SnapshotContext::new(Duration::from_secs(120)).unwrap();
        let mut header = [0; 16];
        main.read_at(0, &mut header, &context).unwrap();
        assert_eq!(&header, b"SQLite format 3\0");
        let facts = manifest.lock().unwrap().read_facts(&context).unwrap();
        assert!(facts.iter().any(|fact| {
            fact.checkpoint
                && serde_json::from_slice::<serde_json::Value>(&fact.body).unwrap()["file"]["nonce"]
                    == nonce
        }));
        main.cleanup(&context).unwrap();
        for file in registry.take_remaining().unwrap() {
            file.cleanup(&context).unwrap();
        }
    }
    #[test]
    fn cancellation_still_releases_all_logical_sqlite_accessors_and_locks() {
        let (_folder, mut owner, _manifest) = owned_fixture();
        owner.open().unwrap();
        owner
            .connection()
            .unwrap()
            .execute_batch("CREATE TABLE payload(data BLOB)")
            .unwrap();
        owner.files.lock().unwrap().context.cancel();
        assert!(
            owner
                .connection()
                .unwrap()
                .execute_batch("INSERT INTO payload VALUES(zeroblob(4096))")
                .is_err()
        );
        assert!(
            owner
                .take_last_error()
                .is_some_and(|error| error.contains("Cancelled"))
        );
        owner.close().unwrap();
        let files = owner.files();
        drop(owner);
        let mut registry = files.lock().unwrap();
        assert!(
            registry
                .slots
                .iter()
                .all(|s| !s.open && s.lock == ffi::SQLITE_LOCK_NONE)
        );
        let context = SnapshotContext::new(Duration::from_secs(120)).unwrap();
        for file in registry.take_remaining().unwrap() {
            file.cleanup(&context).unwrap();
        }
    }
    #[test]
    fn near_image_limit_private_updates_and_vacuum_have_only_bounded_journal_overhead() {
        let (_folder, mut owner, _manifest) = owned_fixture();
        owner.open().unwrap();
        owner.connection().unwrap().execute_batch("CREATE TABLE payload(data BLOB);INSERT INTO payload VALUES(zeroblob(983040));UPDATE payload SET data=zeroblob(983040);VACUUM;").unwrap();
        owner.close().unwrap();
        let files = owner.files();
        drop(owner);
        let mut registry = files.lock().unwrap();
        let main = registry.take_main().unwrap();
        assert!(main.metadata().bytes <= 1024 * 1024);
        assert!(main.metadata().bytes > 983040);
        let context = SnapshotContext::new(Duration::from_secs(120)).unwrap();
        main.cleanup(&context).unwrap();
        for file in registry.take_remaining().unwrap() {
            let metadata = file.metadata();
            assert!(metadata.bytes <= 1024 * 1024);
            file.cleanup(&context).unwrap();
        }
    }
    #[test]
    fn restore_owned_main_rollback_journal_is_scoped_and_undoes_actual_spilled_pages() {
        let (_folder, mut owner, manifest) = owned_fixture();
        assert!(owner.enable_restore_rollback_journal().is_err());
        owner.open().unwrap();
        owner
            .connection()
            .unwrap()
            .execute_batch(
                "CREATE TABLE payload(data BLOB); INSERT INTO payload VALUES(zeroblob(786432));",
            )
            .unwrap();
        owner
            .connection()
            .unwrap()
            .execute_batch("BEGIN IMMEDIATE")
            .unwrap();
        assert!(owner.enable_restore_rollback_journal().is_err());
        owner
            .connection()
            .unwrap()
            .execute_batch("ROLLBACK")
            .unwrap();
        owner.enable_restore_rollback_journal().unwrap();
        owner.connection().unwrap().execute_batch("PRAGMA journal_mode=DELETE; PRAGMA cache_size=-64; BEGIN IMMEDIATE; UPDATE payload SET data=randomblob(786432);").unwrap();
        let (journal_bytes, journal_nonce) = {
            let registry = owner.files.lock().unwrap();
            let journal = registry.slots[registry.names["snapshot-main-journal"]]
                .file
                .as_ref()
                .unwrap()
                .metadata();
            assert!(journal.bytes > IO_CHUNK as u64 && journal.bytes <= registry.maximum);
            (journal.bytes, journal.nonce)
        };
        owner
            .connection()
            .unwrap()
            .execute_batch("ROLLBACK")
            .unwrap();
        let restored: bool = owner
            .connection()
            .unwrap()
            .query_row("SELECT data=zeroblob(786432) FROM payload", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert!(restored, "Actual owned main pages did not roll back");
        let integrity: String = owner
            .connection()
            .unwrap()
            .query_row("PRAGMA integrity_check", [], |row| row.get(0))
            .unwrap();
        assert_eq!(integrity, "ok");
        assert!(owner.enable_restore_rollback_journal().is_err());
        let facts = manifest
            .lock()
            .unwrap()
            .read_facts(&SnapshotContext::new(Duration::from_secs(120)).unwrap())
            .unwrap();
        assert!(facts.iter().any(|fact| {
            fact.checkpoint
                && serde_json::from_slice::<serde_json::Value>(&fact.body)
                    .is_ok_and(|value| value["file"]["nonce"] == journal_nonce)
        }));
        eprintln!(
            "Restore owned journal actual bytes={journal_bytes}; main quota={}",
            owner.files.lock().unwrap().maximum
        );
        owner.close().unwrap();
        let files = owner.files();
        drop(owner);
        let mut registry = files.lock().unwrap();
        let context = SnapshotContext::new(Duration::from_secs(120)).unwrap();
        for file in registry.take_remaining().unwrap() {
            file.cleanup(&context).unwrap();
        }
    }
    #[test]
    fn restore_scoped_main_journal_preserves_default_temporary_and_unscoped_caps() {
        let (_folder, mut owner, _manifest) = owned_fixture();
        owner.open().unwrap();
        owner.connection().unwrap().execute_batch("CREATE TABLE payload(data BLOB);INSERT INTO payload VALUES(zeroblob(786432));PRAGMA journal_mode=DELETE;PRAGMA cache_size=-64;").unwrap();
        assert!(
            owner
                .connection()
                .unwrap()
                .execute_batch("BEGIN IMMEDIATE;UPDATE payload SET data=randomblob(786432);")
                .is_err()
        );
        {
            let registry = owner.files.lock().unwrap();
            let journal = registry.slots[registry.names["snapshot-main-journal"]]
                .file
                .as_ref()
                .unwrap()
                .metadata();
            assert!(journal.bytes <= IO_CHUNK as u64);
            assert!(!registry.restore_main_rollback);
        }
        owner.close().unwrap();
        let files = owner.files();
        drop(owner);
        let mut registry = files.lock().unwrap();
        let context = SnapshotContext::new(Duration::from_secs(120)).unwrap();
        for file in registry.take_remaining().unwrap() {
            file.cleanup(&context).unwrap();
        }

        let (_folder, mut scoped, _manifest) = owned_fixture();
        scoped.open().unwrap();
        scoped.enable_restore_rollback_journal().unwrap();
        scoped.connection().unwrap().execute_batch("CREATE TABLE payload(data BLOB);INSERT INTO payload VALUES(zeroblob(786432));VACUUM;").unwrap();
        scoped.close().unwrap();
        let files = scoped.files();
        drop(scoped);
        let mut registry = files.lock().unwrap();
        assert!(registry.names.iter().filter(|(name, _)| name.starts_with("snapshot-temp-") && name.ends_with("-journal")).all(|(_, &index)| registry.slots[index].file.as_ref().unwrap().metadata().bytes <= IO_CHUNK as u64));
        for file in registry.take_remaining().unwrap() {
            file.cleanup(&context).unwrap();
        }
    }
    #[test]
    fn restore_owned_main_journal_near_image_boundary_fails_with_retained_bounded_files() {
        let (_folder, mut owner, _manifest) = owned_fixture();
        owner.open().unwrap();
        owner.connection().unwrap().execute_batch("PRAGMA page_size=4096;CREATE TABLE payload(data BLOB);INSERT INTO payload VALUES(zeroblob(1040000));").unwrap();
        let pages: i64 = owner
            .connection()
            .unwrap()
            .query_row("PRAGMA page_count", [], |row| row.get(0))
            .unwrap();
        assert_eq!(pages * 4096, 1024 * 1024);
        owner.enable_restore_rollback_journal().unwrap();
        owner
            .connection()
            .unwrap()
            .execute_batch("PRAGMA journal_mode=DELETE;PRAGMA cache_size=-64;")
            .unwrap();
        let error = owner
            .connection()
            .unwrap()
            .execute_batch("BEGIN IMMEDIATE;UPDATE payload SET data=randomblob(1040000);")
            .unwrap_err();
        eprintln!("Bounded near-image restore journal refused full update: {error}");
        assert!(owner.connection().unwrap().is_autocommit());
        let restored: bool = owner
            .connection()
            .unwrap()
            .query_row("SELECT data=zeroblob(1040000) FROM payload", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert!(restored);
        let integrity: String = owner
            .connection()
            .unwrap()
            .query_row("PRAGMA integrity_check", [], |row| row.get(0))
            .unwrap();
        assert_eq!(integrity, "ok");
        owner.close().unwrap();
        let files = owner.files();
        drop(owner);
        let mut registry = files.lock().unwrap();
        assert!(registry.names.contains_key("snapshot-main-journal"));
        let context = SnapshotContext::new(Duration::from_secs(120)).unwrap();
        for file in registry.take_remaining().unwrap() {
            assert!(file.metadata().bytes <= 1024 * 1024);
            file.cleanup(&context).unwrap();
        }
    }
}

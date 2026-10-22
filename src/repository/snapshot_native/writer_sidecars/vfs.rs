//! Default-VFS writer delegation with actual WAL/SHM creator gates.
use super::*;
use rusqlite::{Connection, OpenFlags};
use std::{
    ffi::{CStr, CString, c_char, c_int, c_void},
    mem::size_of,
    ptr,
};

struct NativeAccessor {
    memory: Box<[u64]>,
    close_started: bool,
    closed: bool,
    witnesses: Vec<native::HandleWitness>,
    views: Vec<native::ViewWitness>,
    role: Option<WriterSidecarRole>,
}
impl NativeAccessor {
    fn new(bytes: c_int, role: Option<WriterSidecarRole>) -> Result<Self> {
        ensure!(
            bytes > 0 && bytes <= 1024 * 1024,
            "Default SQLite native accessor size is invalid"
        );
        Ok(Self {
            memory: vec![0u64; (bytes as usize).div_ceil(size_of::<u64>())].into_boxed_slice(),
            close_started: false,
            closed: false,
            witnesses: Vec::new(),
            views: Vec::new(),
            role,
        })
    }
    fn file(&self) -> *mut ffi::sqlite3_file {
        self.memory.as_ptr().cast_mut().cast()
    }
    fn methods(&self) -> Result<&ffi::sqlite3_io_methods> {
        unsafe { (*self.file()).pMethods.as_ref() }
            .context("Default SQLite accessor has no methods")
    }
    fn close(&mut self) -> Result<()> {
        if self.closed {
            return Ok(());
        }
        if !self.close_started
            && let Some(methods) = unsafe { (*self.file()).pMethods.as_ref() }
        {
            let close = methods
                .xClose
                .context("Default SQLite native close is absent")?;
            unsafe { native::capture_main(self.file(), &mut self.witnesses) }?;
            let shm =
                unsafe { native::capture_shm(self.file(), &mut self.witnesses, &mut self.views) }?;
            if !shm.is_empty() {
                let unmap = methods
                    .xShmUnmap
                    .context("Default SQLite native SHM unmap is absent")?;
                let code = unsafe { unmap(self.file(), 0) };
                ensure!(
                    code == ffi::SQLITE_OK,
                    "Default SQLite SHM unmap did not complete"
                );
            }
            self.close_started = true;
            // winClose is invoked once. It unmaps and mutates internal state
            // even when its HANDLE close fails, so retries use only witnesses.
            let _ = unsafe { close(self.file()) };
        }
        for view in &mut self.views {
            view.finish()?;
        }
        for witness in &mut self.witnesses {
            witness.finish()?;
        }
        unsafe { (*self.file()).pMethods = ptr::null() };
        self.closed = true;
        Ok(())
    }
}
struct State {
    profile: SnapshotDirectory,
    expected_main: Option<FileIdentity>,
    main: Mutex<Option<SourceFileProof>>,
    sidecars: Mutex<Option<WriterSidecars>>,
    // Each allocation must keep its native sqlite3_file address stable while
    // later accessor slots are added to this bounded keeper.
    #[allow(clippy::vec_box)]
    accessors: Mutex<Vec<Box<NativeAccessor>>>,
    permit: SnapshotPermit,
    operation: String,
    maximum: u64,
    journal: Arc<dyn WriterSidecarJournal>,
    admission: Option<WriterProfileAdmission>,
    parent: *mut ffi::sqlite3_vfs,
    last_error: Mutex<Option<String>>,
}
impl State {
    fn check(&self) -> Result<()> {
        self.profile.verify()?;
        if let Some(admission) = &self.admission {
            admission.verify_profile(&self.profile)?;
        }
        if let Some(main) = self
            .main
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .as_ref()
        {
            main.verify()?;
        }
        Ok(())
    }
    fn admit(&self, role: WriterSidecarRole) -> Result<()> {
        self.check()?;
        self.sidecars
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .as_mut()
            .context("SQLite main did not establish actual sidecar ownership")?
            .admit(role)
    }
    fn sidecar_check(&self, role: WriterSidecarRole) -> Result<()> {
        self.check()?;
        self.sidecars
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .as_ref()
            .context("SQLite sidecar keeper is absent")?
            .check(role)
    }
    fn retain_native(&self, native: NativeAccessor) -> Result<*mut NativeAccessor> {
        let mut accessors = self
            .accessors
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        ensure!(
            accessors.len() < 16,
            "SQLite native accessor keeper quota exceeded"
        );
        accessors.try_reserve(1)?;
        accessors.push(Box::new(native));
        Ok(&mut **accessors.last_mut().unwrap() as *mut NativeAccessor)
    }
}
static ACTIVE_WRITERS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
struct WriterSlot;
impl WriterSlot {
    fn acquire() -> Result<Self> {
        ACTIVE_WRITERS
            .try_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                (count < 4).then_some(count + 1)
            })
            .map_err(|_| anyhow::anyhow!("SQLite writer owner slots exceeded"))?;
        Ok(Self)
    }
}
impl Drop for WriterSlot {
    fn drop(&mut self) {
        ACTIVE_WRITERS.fetch_sub(1, Ordering::AcqRel);
    }
}
struct Registration {
    name: CString,
    vfs: Box<ffi::sqlite3_vfs>,
    state: Box<State>,
    _slot: WriterSlot,
}
// SQLite owns one exclusively used NO_MUTEX connection. The VFS and State
// addresses remain fixed in their Boxes when the containing owner is moved.
unsafe impl Send for Registration {}
struct RetainedWriter {
    _connection: Option<Connection>,
    _registration: Registration,
    external_connection: bool,
}
static RETAINED_WRITERS: Mutex<Vec<RetainedWriter>> = Mutex::new(Vec::new());
pub(crate) fn retry_retained_writers(context: &SnapshotContext) -> Result<usize> {
    let retained = std::mem::take(
        &mut *RETAINED_WRITERS
            .lock()
            .unwrap_or_else(|error| error.into_inner()),
    );
    let mut remaining = 0;
    let mut first_error = None;
    for retained in retained {
        let mut owner = SidecarWriter {
            connection: retained._connection,
            registration: Some(retained._registration),
            fully_closed: false,
            external_connection: retained.external_connection,
        };
        if let Err(error) = owner.close(context) {
            remaining += 1;
            first_error.get_or_insert(error);
        }
        // Failed Drop returns the same native owners to the bounded keeper.
    }
    match first_error {
        Some(error) => Err(error),
        None => Ok(remaining),
    }
}
pub(crate) struct SidecarWriter {
    connection: Option<Connection>,
    registration: Option<Registration>,
    fully_closed: bool,
    external_connection: bool,
}
pub(crate) struct WriterOpenFailure {
    pub error: anyhow::Error,
    pub owner: Option<SidecarWriter>,
}
impl std::fmt::Debug for WriterOpenFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("WriterOpenFailure")
            .field("error", &self.error)
            .finish_non_exhaustive()
    }
}
impl SidecarWriter {
    #[cfg(test)]
    pub(super) fn sidecars_for_test(&self) -> std::sync::MutexGuard<'_, Option<WriterSidecars>> {
        self.registration
            .as_ref()
            .unwrap()
            .state
            .sidecars
            .lock()
            .unwrap_or_else(|error| error.into_inner())
    }
    #[cfg(test)]
    pub(super) fn actual_native_handle_for_test(&self, role: WriterSidecarRole) -> Result<HANDLE> {
        let registration = self.registration.as_ref().unwrap();
        let accessors = registration
            .state
            .accessors
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let accessor = accessors
            .iter()
            .find(|native| native.role == Some(role) && !native.closed)
            .context("Selected actual default accessor is absent")?;
        let mut raw = ptr::null_mut();
        ensure!(
            unsafe {
                accessor
                    .methods()?
                    .xFileControl
                    .context("Actual native getter is missing")?(
                    accessor.file(),
                    ffi::SQLITE_FCNTL_WIN32_GET_HANDLE,
                    (&mut raw as *mut HANDLE).cast(),
                )
            } == ffi::SQLITE_OK,
            "Actual native getter failed"
        );
        Ok(raw)
    }
    #[cfg(test)]
    pub(super) fn retains_native_close_for_test(
        &self,
        role: WriterSidecarRole,
        raw: HANDLE,
    ) -> bool {
        self.registration
            .as_ref()
            .unwrap()
            .state
            .accessors
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .iter()
            .any(|accessor| {
                accessor.role == Some(role)
                    && accessor.close_started
                    && !accessor.closed
                    && accessor
                        .witnesses
                        .iter()
                        .any(|witness| witness.original() == raw && witness.retained())
            })
    }
    #[cfg(test)]
    pub(super) fn actual_shared_handle_for_test(&self) -> Result<HANDLE> {
        let accessors = self
            .registration
            .as_ref()
            .unwrap()
            .state
            .accessors
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let main = accessors
            .iter()
            .find(|native| native.role.is_none())
            .context("Actual native main is absent")?;
        unsafe { native::actual_shared_handle_for_test(main.file()) }
    }
    #[cfg(test)]
    pub(super) fn actual_mapped_views_for_test(&self) -> usize {
        self.registration
            .as_ref()
            .unwrap()
            .state
            .accessors
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .iter()
            .map(|native| native.views.len())
            .sum()
    }
    #[cfg(test)]
    pub(crate) fn open(
        profile: SnapshotDirectory,
        expected_main: Option<FileIdentity>,
        operation: String,
        maximum: u64,
        permit: SnapshotPermit,
        journal: Arc<dyn WriterSidecarJournal>,
    ) -> std::result::Result<Self, WriterOpenFailure> {
        Self::open_admitted(
            profile,
            expected_main,
            operation,
            maximum,
            permit,
            journal,
            None,
        )
    }
    pub(crate) fn open_guarded(
        profile: SnapshotDirectory,
        expected_main: Option<FileIdentity>,
        operation: String,
        maximum: u64,
        permit: SnapshotPermit,
        journal: Arc<dyn WriterSidecarJournal>,
        admission: WriterProfileAdmission,
    ) -> std::result::Result<Self, WriterOpenFailure> {
        Self::open_admitted(
            profile,
            expected_main,
            operation,
            maximum,
            permit,
            journal,
            Some(admission),
        )
    }
    fn open_admitted(
        profile: SnapshotDirectory,
        expected_main: Option<FileIdentity>,
        operation: String,
        maximum: u64,
        permit: SnapshotPermit,
        journal: Arc<dyn WriterSidecarJournal>,
        admission: Option<WriterProfileAdmission>,
    ) -> std::result::Result<Self, WriterOpenFailure> {
        // Registration itself owns no SQLite file. Allocation validation occurs
        // before native xOpen and every later failure returns this same owner.
        let registration = Registration::new(
            profile,
            expected_main,
            operation,
            maximum,
            permit,
            journal,
            admission,
        );
        let registration = match registration {
            Ok(value) => value,
            Err(error) => return Err(WriterOpenFailure { error, owner: None }),
        };
        let mut owner = Self {
            connection: None,
            registration: Some(registration),
            fully_closed: false,
            external_connection: false,
        };
        let registration = owner.registration.as_ref().unwrap();
        let path = registration
            .state
            .profile
            .path()
            .join("native-library.sqlite3");
        let flags = OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_NO_MUTEX
            | if registration.state.expected_main.is_none() {
                OpenFlags::SQLITE_OPEN_CREATE
            } else {
                OpenFlags::empty()
            };
        match Connection::open_with_flags_and_vfs(&path, flags, registration.name.as_c_str()) {
            Ok(connection) => owner.connection = Some(connection),
            Err(error) => {
                return Err(WriterOpenFailure {
                    error: owner.registration.as_ref().unwrap().state.error(error),
                    owner: Some(owner),
                });
            }
        }
        if let Err(error) =
            owner
                .connection
                .as_ref()
                .unwrap()
                .pragma_update(None, "journal_size_limit", 0i64)
        {
            return Err(WriterOpenFailure {
                error: error.into(),
                owner: Some(owner),
            });
        }
        Ok(owner)
    }
    #[cfg(test)]
    pub(crate) fn connection(&self) -> Result<&Connection> {
        self.connection
            .as_ref()
            .context("SQLite writer is actually closed")
    }
    /// Transfers only the Connection into Repository's existing mutex. This
    /// owner retains registration, native accessors, witnesses and creator pins.
    pub(crate) fn take_connection(&mut self) -> Result<Connection> {
        ensure!(
            !self.external_connection,
            "SQLite writer already transferred its connection"
        );
        let connection = self
            .connection
            .take()
            .context("SQLite writer has no connection to transfer")?;
        self.external_connection = true;
        Ok(connection)
    }
    /// The production close path consumes this owner's actual wrapper accessor.
    /// A busy/error return gives the same Connection back to Repository.
    pub(crate) fn close_connection(
        &mut self,
        connection: Connection,
    ) -> std::result::Result<(), (Connection, anyhow::Error)> {
        let checked = (|| -> Result<()> {
            ensure!(
                self.external_connection && self.connection.is_none(),
                "SQLite close does not own a transferred connection"
            );
            let registration = self
                .registration
                .as_ref()
                .context("SQLite writer registration is absent")?;
            let mut pointer: *mut ffi::sqlite3_file = ptr::null_mut();
            ensure!(
                unsafe {
                    ffi::sqlite3_file_control(
                        connection.handle(),
                        c"main".as_ptr(),
                        ffi::SQLITE_FCNTL_FILE_POINTER,
                        (&mut pointer as *mut *mut ffi::sqlite3_file).cast(),
                    )
                } == ffi::SQLITE_OK
                    && !pointer.is_null(),
                "SQLite close lacks its actual main wrapper accessor"
            );
            let wrapper = unsafe { file(pointer) }?;
            ensure!(
                wrapper.state == &*registration.state as *const State && wrapper.role.is_none(),
                "SQLite close was offered another actual connection"
            );
            Ok(())
        })();
        if let Err(error) = checked {
            return Err((connection, error));
        }
        match connection.close() {
            Ok(()) => {
                self.external_connection = false;
                Ok(())
            }
            Err((connection, error)) => Err((connection, error.into())),
        }
    }
    pub(crate) fn close(&mut self, context: &SnapshotContext) -> Result<()> {
        if self.fully_closed {
            return Ok(());
        }
        if self.external_connection {
            let registration = self
                .registration
                .as_ref()
                .context("SQLite external close owner is absent")?;
            let accessors = registration
                .state
                .accessors
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            ensure!(
                accessors.iter().any(|native| native.role.is_none())
                    && accessors.iter().all(|native| native.closed),
                "Transferred SQLite connection still has actual native accessors"
            );
            // Covers Connection's ordinary Drop/failed-opening caller close.
            // Every actual default accessor and its witnesses already closed.
            self.external_connection = false;
        }
        let needs_checkpoint = self
            .registration
            .as_ref()
            .context("SQLite checkpoint owner is absent")?
            .state
            .sidecars
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .as_ref()
            .map_or(Ok(false), WriterSidecars::wal_needs_checkpoint)?;
        if let Some(connection) = self.connection.as_ref()
            && needs_checkpoint
        {
            ensure!(
                connection.is_autocommit(),
                "SQLite writer still has an active transaction"
            );
            let (busy, pages, checkpointed): (i64, i64, i64) =
                connection.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| {
                    Ok((row.get(0)?, row.get(1)?, row.get(2)?))
                })?;
            ensure!(
                busy == 0 && pages == checkpointed,
                "SQLite writer checkpoint still has active readers or writers"
            );
        }
        if let Some(connection) = self.connection.take()
            && let Err((connection, error)) = connection.close()
        {
            self.connection = Some(connection);
            return Err(error.into());
        }
        let registration = self.registration.as_ref().unwrap();
        let mut accessors = registration
            .state
            .accessors
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        while let Some(mut native) = accessors.pop() {
            if let Err(error) = native.close() {
                accessors.push(native);
                return Err(error);
            }
        }
        drop(accessors);
        if let Some(sidecars) = registration
            .state
            .sidecars
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .as_mut()
        {
            sidecars.reconcile_retained_creations()?;
            sidecars.acknowledge_sqlite_close();
            sidecars.retire(context)?;
        }
        let main = {
            registration
                .state
                .main
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .take()
        };
        if let Some(main) = main
            && let Err(mut failure) = main.close()
        {
            *registration
                .state
                .main
                .lock()
                .unwrap_or_else(|error| error.into_inner()) =
                failure.source.take().map(|value| *value);
            anyhow::bail!("{}", failure.message);
        }
        registration.state.journal.finish(context)?;
        self.fully_closed = true;
        Ok(())
    }
}
impl Drop for SidecarWriter {
    fn drop(&mut self) {
        if let Some(registration) = self.registration.take() {
            if !self.fully_closed {
                RETAINED_WRITERS
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .push(RetainedWriter {
                        _connection: self.connection.take(),
                        _registration: registration,
                        external_connection: self.external_connection,
                    });
            }
        }
    }
}
impl State {
    fn error(&self, error: impl std::fmt::Display) -> anyhow::Error {
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
}
impl Registration {
    fn new(
        profile: SnapshotDirectory,
        expected_main: Option<FileIdentity>,
        operation: String,
        maximum: u64,
        permit: SnapshotPermit,
        journal: Arc<dyn WriterSidecarJournal>,
        admission: Option<WriterProfileAdmission>,
    ) -> Result<Self> {
        let slot = WriterSlot::acquire()?;
        profile.verify()?;
        if let Some(admission) = &admission {
            admission.verify_profile(&profile)?;
        }
        validate_operation(&operation)?;
        ensure!(
            maximum > 0 && maximum <= permit.0.maximum,
            "Invalid SQLite writer sidecar allowance"
        );
        let parent = unsafe { ffi::sqlite3_vfs_find(ptr::null()) };
        ensure!(!parent.is_null(), "Actual default SQLite VFS is missing");
        native::verify_abi(parent)?;
        native::install_close_observers(parent)?;
        let name = CString::new(format!("lx-owned-writer-{}", nonce()?))?;
        let mut state = Box::new(State {
            profile,
            expected_main,
            main: Mutex::new(None),
            sidecars: Mutex::new(None),
            accessors: Mutex::new(Vec::new()),
            permit,
            operation,
            maximum,
            journal,
            admission,
            parent,
            last_error: Mutex::new(None),
        });
        let mut vfs = Box::new(unsafe { *parent });
        vfs.pNext = ptr::null_mut();
        vfs.zName = name.as_ptr();
        vfs.pAppData = (&mut *state as *mut State).cast();
        vfs.szOsFile = size_of::<WriterFile>() as c_int;
        vfs.xOpen = Some(writer_open);
        vfs.xDelete = Some(writer_delete);
        vfs.xAccess = Some(writer_access);
        vfs.xFullPathname = Some(writer_full_path);
        ensure!(
            unsafe { ffi::sqlite3_vfs_register(&mut *vfs, 0) } == ffi::SQLITE_OK,
            "Cannot register actual SQLite writer VFS"
        );
        Ok(Self {
            name,
            vfs,
            state,
            _slot: slot,
        })
    }
}
impl Drop for Registration {
    fn drop(&mut self) {
        unsafe { ffi::sqlite3_vfs_unregister(&mut *self.vfs) };
    }
}
#[repr(C)]
struct WriterFile {
    base: ffi::sqlite3_file,
    native: *mut NativeAccessor,
    state: *const State,
    role: Option<WriterSidecarRole>,
}
unsafe fn state(vfs: *mut ffi::sqlite3_vfs) -> Result<&'static State> {
    ensure!(!vfs.is_null(), "SQLite writer VFS is absent");
    Ok(unsafe { &*(*vfs).pAppData.cast::<State>() })
}
unsafe fn file(file: *mut ffi::sqlite3_file) -> Result<&'static WriterFile> {
    ensure!(!file.is_null(), "SQLite writer file is absent");
    let file = unsafe { &*file.cast::<WriterFile>() };
    ensure!(
        !file.native.is_null() && !file.state.is_null(),
        "SQLite writer file lost its actual native owner"
    );
    Ok(file)
}
fn callback(
    vfs: *mut ffi::sqlite3_vfs,
    fallback: c_int,
    body: impl FnOnce() -> Result<c_int>,
) -> c_int {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(body)) {
        Ok(Ok(code)) => code,
        result => {
            let error = match result {
                Ok(Err(error)) => format!("{error:#}"),
                _ => "SQLite writer callback panicked; actual owner retained".into(),
            };
            if let Ok(state) = unsafe { state(vfs) } {
                *state
                    .last_error
                    .lock()
                    .unwrap_or_else(|error| error.into_inner()) =
                    Some(error.chars().take(2048).collect());
            }
            fallback
        }
    }
}
fn io_callback(
    pointer: *mut ffi::sqlite3_file,
    fallback: c_int,
    body: impl FnOnce(&WriterFile, &mut NativeAccessor, &ffi::sqlite3_io_methods) -> Result<c_int>,
) -> c_int {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> Result<c_int> {
        let file = unsafe { file(pointer) }?;
        let state = unsafe { &*file.state };
        state.check()?;
        let native = unsafe { &mut *file.native };
        if let Some(role) = file.role {
            unsafe {
                state
                    .sidecars
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .as_ref()
                    .context("SQLite creator keeper missing")?
                    .verify_accessor(role, native.file())
            }?;
        }
        let methods = unsafe { (*native.file()).pMethods.as_ref() }
            .context("Default SQLite actual accessor methods are missing")?;
        body(file, native, methods)
    })) {
        Ok(Ok(code)) => code,
        _ => fallback,
    }
}
unsafe extern "C" fn writer_open(
    vfs: *mut ffi::sqlite3_vfs,
    path: ffi::sqlite3_filename,
    pointer: *mut ffi::sqlite3_file,
    flags: c_int,
    out: *mut c_int,
) -> c_int {
    callback(vfs, ffi::SQLITE_CANTOPEN, || {
        ensure!(
            !path.is_null() && !pointer.is_null(),
            "Anonymous SQLite writer allocation is refused"
        );
        unsafe { (*pointer).pMethods = ptr::null() };
        let state = unsafe { state(vfs) }?;
        state.check()?;
        let path_text = unsafe { CStr::from_ptr(path) }.to_str()?;
        let main_path = state.profile.path().join("native-library.sqlite3");
        let actual = crate::domain::music::normalize_local_path(Path::new(path_text));
        let role = if flags & ffi::SQLITE_OPEN_MAIN_DB != 0 {
            ensure!(
                actual == crate::domain::music::normalize_local_path(&main_path),
                "SQLite main left its actual profile"
            );
            ensure!(
                state
                    .main
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .is_none(),
                "SQLite writer cannot replace its already retained actual main owner"
            );
            None
        } else if flags & ffi::SQLITE_OPEN_WAL != 0 {
            ensure!(
                actual
                    == crate::domain::music::normalize_local_path(
                        &state.profile.path().join(WriterSidecarRole::Wal.leaf())
                    ),
                "SQLite WAL left its actual profile"
            );
            state.admit(WriterSidecarRole::Wal)?;
            Some(WriterSidecarRole::Wal)
        } else if flags & ffi::SQLITE_OPEN_MAIN_JOURNAL != 0 {
            ensure!(
                actual
                    == crate::domain::music::normalize_local_path(
                        &state.profile.path().join(WriterSidecarRole::Journal.leaf())
                    ),
                "SQLite rollback journal left its actual profile"
            );
            state.admit(WriterSidecarRole::Journal)?;
            Some(WriterSidecarRole::Journal)
        } else {
            anyhow::bail!("SQLite writer only delegates main and admitted persistent sidecars");
        };
        // Keeper admission precedes default xOpen; a returned default native
        // HANDLE is already under this same State before FID/body inspection.
        let native = state.retain_native(NativeAccessor::new(
            unsafe { (*state.parent).szOsFile },
            role,
        )?)?;
        let native = unsafe { &mut *native };
        let open =
            unsafe { (*state.parent).xOpen }.context("Default SQLite native open is missing")?;
        let native_file = native.file();
        let scope = native::factory_scope(&mut native.witnesses, &mut native.views)?;
        let code = unsafe {
            open(
                state.parent,
                path,
                native_file,
                flags & !ffi::SQLITE_OPEN_CREATE
                    | if role.is_none() && state.expected_main.is_none() {
                        ffi::SQLITE_OPEN_CREATE
                    } else {
                        0
                    },
                out,
            )
        };
        drop(scope);
        if code != ffi::SQLITE_OK {
            return Ok(code);
        }
        let checked = (|| -> Result<()> {
            if let Some(role) = role {
                unsafe {
                    state
                        .sidecars
                        .lock()
                        .unwrap_or_else(|error| error.into_inner())
                        .as_ref()
                        .unwrap()
                        .verify_accessor(role, native.file())
                }?;
            } else {
                let proof =
                    match unsafe { SourceFileProof::from_sqlite_file(native.file(), &main_path) } {
                        Ok(proof) => proof,
                        Err(mut failure) => {
                            *state.main.lock().unwrap_or_else(|error| error.into_inner()) =
                                failure.source.take().map(|proof| *proof);
                            anyhow::bail!("{}", failure.message);
                        }
                    };
                *state.main.lock().unwrap_or_else(|error| error.into_inner()) = Some(proof);
                let identity = state
                    .main
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .as_ref()
                    .unwrap()
                    .metadata()?
                    .identity
                    .clone();
                ensure!(
                    state
                        .expected_main
                        .as_ref()
                        .is_none_or(|expected| *expected == identity),
                    "Default SQLite opened another actual main"
                );
                let mut persist = 1;
                ensure!(
                    unsafe {
                        native
                            .methods()?
                            .xFileControl
                            .context("Default SQLite main control is absent")?(
                            native.file(),
                            ffi::SQLITE_FCNTL_PERSIST_WAL,
                            (&mut persist as *mut c_int).cast(),
                        )
                    } == ffi::SQLITE_OK,
                    "Default SQLite cannot retain owned WAL/SHM"
                );
                persist = -1;
                ensure!(
                    unsafe {
                        native.methods()?.xFileControl.unwrap()(
                            native.file(),
                            ffi::SQLITE_FCNTL_PERSIST_WAL,
                            (&mut persist as *mut c_int).cast(),
                        )
                    } == ffi::SQLITE_OK
                        && persist == 1,
                    "Owned WAL persistence was not applied before writer use"
                );
                let mut sidecars = WriterSidecars::new(
                    state.profile.clone(),
                    identity,
                    state.operation.clone(),
                    state.maximum,
                    state.permit.clone(),
                    state.journal.clone(),
                )?;
                if let Some(admission) = &state.admission {
                    sidecars.allow_locked_profile_admission(admission.clone(), unsafe {
                        native::main_has_sqlite_header(native.file())
                    }?)?;
                }
                *state
                    .sidecars
                    .lock()
                    .unwrap_or_else(|error| error.into_inner()) = Some(sidecars);
            }
            Ok(())
        })();
        if let Err(error) = checked {
            return Err(error);
        }
        unsafe {
            ptr::write(
                pointer.cast::<WriterFile>(),
                WriterFile {
                    base: ffi::sqlite3_file {
                        pMethods: &WRITER_IO,
                    },
                    native,
                    state,
                    role,
                },
            )
        };
        Ok(ffi::SQLITE_OK)
    })
}
unsafe extern "C" fn writer_close(pointer: *mut ffi::sqlite3_file) -> c_int {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> Result<c_int> {
        let file = unsafe { file(pointer) }?;
        let state = unsafe { &*file.state };
        let native = unsafe { &mut *file.native };
        let result = native.close();
        unsafe {
            (*pointer.cast::<WriterFile>()).native = ptr::null_mut();
            (*pointer).pMethods = ptr::null()
        };
        if let Err(error) = result {
            return Err(error);
        }
        Ok(ffi::SQLITE_OK)
    })) {
        Ok(Ok(code)) => code,
        _ => ffi::SQLITE_IOERR_CLOSE,
    }
}
unsafe extern "C" fn writer_read(
    p: *mut ffi::sqlite3_file,
    b: *mut c_void,
    n: c_int,
    o: i64,
) -> c_int {
    io_callback(p, ffi::SQLITE_IOERR_READ, |_, native, m| {
        Ok(unsafe { m.xRead.context("Default SQLite read missing")?(native.file(), b, n, o) })
    })
}
unsafe extern "C" fn writer_write(
    p: *mut ffi::sqlite3_file,
    b: *const c_void,
    n: c_int,
    o: i64,
) -> c_int {
    io_callback(p, ffi::SQLITE_IOERR_WRITE, |file, native, m| {
        ensure!(n >= 0 && o >= 0, "Invalid SQLite writer offset");
        if let Some(role) = file.role {
            unsafe { &*file.state }
                .sidecars
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .as_mut()
                .unwrap()
                .reserve_write(
                    role,
                    (o as u64)
                        .checked_add(n as u64)
                        .context("SQLite writer size overflow")?,
                )?;
        }
        Ok(unsafe { m.xWrite.context("Default SQLite write missing")?(native.file(), b, n, o) })
    })
}
unsafe extern "C" fn writer_truncate(p: *mut ffi::sqlite3_file, n: i64) -> c_int {
    io_callback(p, ffi::SQLITE_IOERR_TRUNCATE, |file, native, m| {
        ensure!(n >= 0, "Invalid SQLite writer truncation");
        if let Some(role) = file.role {
            unsafe { &*file.state }
                .sidecars
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .as_mut()
                .unwrap()
                .reserve_write(role, n as u64)?;
        }
        Ok(unsafe { m.xTruncate.context("Default SQLite truncate missing")?(native.file(), n) })
    })
}
unsafe extern "C" fn writer_sync(p: *mut ffi::sqlite3_file, f: c_int) -> c_int {
    io_callback(p, ffi::SQLITE_IOERR_FSYNC, |_, native, m| {
        Ok(unsafe { m.xSync.context("Default SQLite sync missing")?(native.file(), f) })
    })
}
unsafe extern "C" fn writer_size(p: *mut ffi::sqlite3_file, n: *mut i64) -> c_int {
    io_callback(p, ffi::SQLITE_IOERR_FSTAT, |_, native, m| {
        Ok(unsafe { m.xFileSize.context("Default SQLite size missing")?(native.file(), n) })
    })
}
unsafe extern "C" fn writer_lock(p: *mut ffi::sqlite3_file, f: c_int) -> c_int {
    io_callback(p, ffi::SQLITE_IOERR_LOCK, |_, native, m| {
        Ok(unsafe { m.xLock.context("Default SQLite lock missing")?(native.file(), f) })
    })
}
unsafe extern "C" fn writer_unlock(p: *mut ffi::sqlite3_file, f: c_int) -> c_int {
    let result = (|| -> Result<c_int> {
        let file = unsafe { file(p) }?;
        let native = unsafe { &*file.native };
        Ok(unsafe {
            native
                .methods()?
                .xUnlock
                .context("Default SQLite unlock missing")?(native.file(), f)
        })
    })();
    result.unwrap_or(ffi::SQLITE_IOERR_UNLOCK)
}
unsafe extern "C" fn writer_reserved(p: *mut ffi::sqlite3_file, n: *mut c_int) -> c_int {
    io_callback(p, ffi::SQLITE_IOERR_CHECKRESERVEDLOCK, |_, native, m| {
        Ok(unsafe {
            m.xCheckReservedLock
                .context("Default SQLite reserved lock check missing")?(native.file(), n)
        })
    })
}
unsafe extern "C" fn writer_control(p: *mut ffi::sqlite3_file, f: c_int, a: *mut c_void) -> c_int {
    io_callback(p, ffi::SQLITE_IOERR, |_, native, m| {
        if f == ffi::SQLITE_FCNTL_PERSIST_WAL {
            ensure!(
                !a.is_null() && unsafe { *a.cast::<c_int>() } != 0,
                "Owned WAL persistence cannot be disabled"
            );
        }
        Ok(unsafe {
            m.xFileControl.context("Default SQLite control missing")?(native.file(), f, a)
        })
    })
}
unsafe extern "C" fn writer_sector(p: *mut ffi::sqlite3_file) -> c_int {
    let Ok(file) = (unsafe { file(p) }) else {
        return 0;
    };
    let native = unsafe { &*file.native };
    native
        .methods()
        .ok()
        .and_then(|m| m.xSectorSize)
        .map_or(0, |f| unsafe { f(native.file()) })
}
unsafe extern "C" fn writer_characteristics(p: *mut ffi::sqlite3_file) -> c_int {
    let Ok(file) = (unsafe { file(p) }) else {
        return 0;
    };
    let native = unsafe { &*file.native };
    native
        .methods()
        .ok()
        .and_then(|m| m.xDeviceCharacteristics)
        .map_or(0, |f| unsafe { f(native.file()) })
}
unsafe extern "C" fn writer_shm_map(
    p: *mut ffi::sqlite3_file,
    page: c_int,
    size: c_int,
    extend: c_int,
    out: *mut *mut c_void,
) -> c_int {
    io_callback(p, ffi::SQLITE_IOERR_SHMMAP, |file, native, m| {
        let state = unsafe { &*file.state };
        state.admit(WriterSidecarRole::Shm)?;
        if extend != 0 {
            ensure!(page >= 0 && size > 0, "Invalid SQLite SHM mapping");
            state
                .sidecars
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .as_mut()
                .unwrap()
                .reserve_write(
                    WriterSidecarRole::Shm,
                    (page as u64 + 1)
                        .checked_mul(size as u64)
                        .context("SQLite SHM size overflow")?,
                )?;
        }
        let _ = unsafe {
            native::capture_shm(native.file(), &mut native.witnesses, &mut native.views)
        }?;
        let native_file = native.file();
        let scope = native::factory_scope(&mut native.witnesses, &mut native.views)?;
        let code = unsafe {
            m.xShmMap
                .context("Default SQLite real SHM mapping missing")?(
                native_file,
                page,
                size,
                extend,
                out,
            )
        };
        drop(scope);
        let handles = unsafe {
            native::capture_shm(native.file(), &mut native.witnesses, &mut native.views)
        }?;
        for handle in handles {
            unsafe {
                state
                    .sidecars
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .as_ref()
                    .context("SQLite actual SHM keeper is absent")?
                    .verify_native_handle(WriterSidecarRole::Shm, handle)
            }?;
        }
        state.sidecar_check(WriterSidecarRole::Shm)?;
        Ok(code)
    })
}
unsafe extern "C" fn writer_shm_lock(
    p: *mut ffi::sqlite3_file,
    o: c_int,
    n: c_int,
    f: c_int,
) -> c_int {
    let result = (|| -> Result<c_int> {
        let file = unsafe { file(p) }?;
        let native = unsafe { &*file.native };
        if f & ffi::SQLITE_SHM_UNLOCK == 0 {
            unsafe { &*file.state }.sidecar_check(WriterSidecarRole::Shm)?;
        }
        Ok(unsafe {
            native
                .methods()?
                .xShmLock
                .context("Default SQLite real SHM lock missing")?(native.file(), o, n, f)
        })
    })();
    result.unwrap_or(ffi::SQLITE_IOERR_SHMLOCK)
}
unsafe extern "C" fn writer_shm_barrier(p: *mut ffi::sqlite3_file) {
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        if let Ok(file) = unsafe { file(p) } {
            let native = unsafe { &*file.native };
            if let Some(barrier) = native.methods().ok().and_then(|m| m.xShmBarrier) {
                unsafe { barrier(native.file()) };
            }
        }
    }));
}
unsafe extern "C" fn writer_shm_unmap(p: *mut ffi::sqlite3_file, _delete: c_int) -> c_int {
    let result = (|| -> Result<c_int> {
        let file = unsafe { file(p) }?;
        let native = unsafe { &mut *file.native };
        let _ = unsafe {
            native::capture_shm(native.file(), &mut native.witnesses, &mut native.views)
        }?;
        Ok(unsafe {
            native
                .methods()?
                .xShmUnmap
                .context("Default SQLite SHM unmap missing")?(native.file(), 0)
        })
    })();
    result.unwrap_or(ffi::SQLITE_IOERR_SHMMAP)
}
static WRITER_IO: ffi::sqlite3_io_methods = ffi::sqlite3_io_methods {
    iVersion: 3,
    xClose: Some(writer_close),
    xRead: Some(writer_read),
    xWrite: Some(writer_write),
    xTruncate: Some(writer_truncate),
    xSync: Some(writer_sync),
    xFileSize: Some(writer_size),
    xLock: Some(writer_lock),
    xUnlock: Some(writer_unlock),
    xCheckReservedLock: Some(writer_reserved),
    xFileControl: Some(writer_control),
    xSectorSize: Some(writer_sector),
    xDeviceCharacteristics: Some(writer_characteristics),
    xShmMap: Some(writer_shm_map),
    xShmLock: Some(writer_shm_lock),
    xShmBarrier: Some(writer_shm_barrier),
    xShmUnmap: Some(writer_shm_unmap),
    xFetch: None,
    xUnfetch: None,
};
unsafe extern "C" fn writer_delete(
    vfs: *mut ffi::sqlite3_vfs,
    path: *const c_char,
    _sync: c_int,
) -> c_int {
    callback(vfs, ffi::SQLITE_IOERR_DELETE, || {
        ensure!(!path.is_null(), "SQLite delete path is missing");
        let state = unsafe { state(vfs) }?;
        state.check()?;
        let text = unsafe { CStr::from_ptr(path) }.to_str()?;
        ensure!(
            crate::domain::music::normalize_local_path(Path::new(text))
                == crate::domain::music::normalize_local_path(
                    &state.profile.path().join(WriterSidecarRole::Journal.leaf())
                ),
            "Default SQLite cannot delete an unregistered persistent sidecar"
        );
        {
            let accessors = state
                .accessors
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            let journals = accessors
                .iter()
                .filter(|native| native.role == Some(WriterSidecarRole::Journal))
                .collect::<Vec<_>>();
            ensure!(
                !journals.is_empty() && journals.iter().all(|native| native.closed),
                "Rollback journal lacks actual default-accessor close witnesses"
            );
        }
        state
            .sidecars
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .as_mut()
            .context("SQLite actual journal keeper is missing")?
            .retire_journal_after_accessor_close(&SnapshotContext::new(Duration::from_secs(
                120,
            ))?)?;
        Ok(ffi::SQLITE_OK)
    })
}
unsafe extern "C" fn writer_access(
    vfs: *mut ffi::sqlite3_vfs,
    path: *const c_char,
    flags: c_int,
    out: *mut c_int,
) -> c_int {
    callback(vfs, ffi::SQLITE_IOERR_ACCESS, || {
        let state = unsafe { state(vfs) }?;
        state.check()?;
        Ok(unsafe {
            (*state.parent)
                .xAccess
                .context("Default SQLite access missing")?(
                state.parent, path, flags, out
            )
        })
    })
}
unsafe extern "C" fn writer_full_path(
    vfs: *mut ffi::sqlite3_vfs,
    path: *const c_char,
    n: c_int,
    out: *mut c_char,
) -> c_int {
    callback(vfs, ffi::SQLITE_CANTOPEN, || {
        let state = unsafe { state(vfs) }?;
        state.check()?;
        Ok(unsafe {
            (*state.parent)
                .xFullPathname
                .context("Default SQLite pathname missing")?(state.parent, path, n, out)
        })
    })
}

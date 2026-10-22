//! Checked prefixes of the fixed bundled SQLite Windows VFS (3.53.2).
//! The definitions correspond to winFile, winShm, winShmNode and ShmRegion
//! in libsqlite3-sys 0.38.2's sqlite3/sqlite3.c. No other ABI is admitted.
use super::*;
use std::{ffi::CStr, mem::size_of};
use windows_sys::Win32::{
    Foundation::{
        CompareObjectHandles, DUPLICATE_SAME_ACCESS, DuplicateHandle, GetHandleInformation,
        INVALID_HANDLE_VALUE,
    },
    System::Threading::GetCurrentProcess,
};

const SOURCE_ID: &[u8] =
    b"2026-06-03 19:12:13 d6e03d8c777cfa2d35e3b60d8ec3e0187f3e9f99d8e2ee9cac695fd6fcdf1a24";
const MAX_REGIONS: usize = 4096;
const MAX_WITNESSES: usize = 8198;
#[path = "close_observer.rs"]
mod observer;
pub(super) fn install_close_observers(parent: *mut ffi::sqlite3_vfs) -> Result<()> {
    observer::install(parent)
}

#[repr(C)]
struct WinFilePrefix {
    methods: *const ffi::sqlite3_io_methods,
    vfs: *mut ffi::sqlite3_vfs,
    handle: HANDLE,
    lock_type: u8,
    shared_lock_byte: i16,
    control_flags: u8,
    last_errno: u32,
    shm: *mut WinShmPrefix,
}
#[repr(C)]
struct WinShmPrefix {
    node: *mut WinShmNodePrefix,
    shared_mask: u16,
    exclusive_mask: u16,
    handle: HANDLE,
    readonly: i32,
    // On Windows x64 the optional DEBUG/OS_TRACE byte occupies this otherwise
    // padded area. The following pointer has offset 32 in either configuration.
    debug_or_padding: [std::mem::MaybeUninit<u8>; 4],
    next: *mut WinShmPrefix,
}
#[repr(C)]
struct ShmRegion {
    mapping: HANDLE,
    view: *mut std::ffi::c_void,
}
#[repr(C)]
struct WinShmNodePrefix {
    mutex: *mut ffi::sqlite3_mutex,
    filename: *mut std::ffi::c_char,
    shared_handle: HANDLE,
    shared_lock_handle: i32,
    unlocked: i32,
    readonly: i32,
    region_size: i32,
    region_count: i32,
    regions: *mut ShmRegion,
    last_errno: u32,
    connections: *mut WinShmPrefix,
}

/// The duplicate is a retained witness of the same Kernel object. A recycled
/// HANDLE value alone cannot authorize closing another process-owned object.
pub(super) struct HandleWitness {
    original: HANDLE,
    witness: Option<File>,
    original_closed: bool,
    observation: Option<Arc<observer::HandleObservation>>,
    unexposed_owner: Option<File>,
}
unsafe impl Send for HandleWitness {}
impl HandleWitness {
    fn capture(original: HANDLE) -> Result<Self> {
        ensure!(
            valid_handle(original),
            "Default SQLite returned no actual native handle"
        );
        let mut duplicate = std::ptr::null_mut();
        let process = unsafe { GetCurrentProcess() };
        ensure!(
            unsafe {
                DuplicateHandle(
                    process,
                    original,
                    process,
                    &mut duplicate,
                    0,
                    0,
                    DUPLICATE_SAME_ACCESS,
                )
            } != 0,
            "Cannot retain the default SQLite handle's actual Kernel object"
        );
        // Store the native return before any fallible inspection of its object.
        let owner = Self {
            original,
            witness: Some(unsafe { File::from_raw_handle(duplicate) }),
            original_closed: false,
            observation: None,
            unexposed_owner: None,
        };
        Ok(owner)
    }
    fn register_observation(&mut self) -> Result<()> {
        if self.observation.is_none() {
            self.observation = Some(observer::handle(
                self.original,
                self.witness
                    .as_ref()
                    .context("Native object witness is absent")?
                    .as_raw_handle(),
            )?);
        }
        Ok(())
    }
    fn same_original(&self) -> Result<bool> {
        let witness = self
            .witness
            .as_ref()
            .context("Native close witness already retired")?;
        let mut flags = 0;
        if unsafe { GetHandleInformation(self.original, &mut flags) } == 0 {
            let error = std::io::Error::last_os_error();
            ensure!(
                error.raw_os_error() == Some(6),
                "Cannot inspect original SQLite native close: {error}"
            );
            return Ok(false);
        }
        Ok(unsafe { CompareObjectHandles(self.original, witness.as_raw_handle()) } != 0)
    }
    pub(super) fn finish(&mut self) -> Result<()> {
        if self.unexposed_owner.is_some() {
            // A failed hook never returned this newly created original to C.
            // Its exclusive native creator remains owned here without aliases.
            platform::close_retained(&mut self.unexposed_owner)?;
            self.original_closed = true;
        }
        if self.witness.is_none() {
            ensure!(
                self.original_closed,
                "Native object has no close acknowledgement"
            );
            return Ok(());
        }
        if self
            .observation
            .as_ref()
            .is_some_and(|watch| watch.succeeded())
        {
            self.original_closed = true;
        }
        if !self.original_closed {
            if self.same_original()? {
                ensure!(
                    self.observation
                        .as_ref()
                        .is_some_and(|watch| observer::actual_close_attempted(watch)),
                    "Default SQLite did not attempt closing this shared actual object"
                );
                // The default Windows accessor has already been called once.
                // Retry only its still-live original handle, never winClose.
                let mut original = Some(unsafe { File::from_raw_handle(self.original) });
                if let Err(error) = platform::close_retained(&mut original) {
                    if let Some(file) = original.take() {
                        use std::os::windows::io::IntoRawHandle;
                        let _ = file.into_raw_handle();
                    }
                    return Err(error);
                }
            }
            self.original_closed = true;
        }
        if let Some(watch) = &self.observation {
            watch.duplicate(std::ptr::null_mut());
        }
        let result = platform::close_retained(&mut self.witness);
        if let Some(watch) = &self.observation {
            watch.duplicate(
                self.witness
                    .as_ref()
                    .map_or(std::ptr::null_mut(), AsRawHandle::as_raw_handle),
            );
        }
        result
    }
    pub(super) fn original(&self) -> HANDLE {
        self.original
    }
    #[cfg(test)]
    pub(super) fn retained(&self) -> bool {
        self.witness.is_some() && !self.original_closed
    }
}
pub(super) struct ViewWitness {
    address: usize,
    observation: Option<Arc<observer::ViewObservation>>,
    unexposed: bool,
}
impl ViewWitness {
    pub(super) fn finish(&mut self) -> Result<()> {
        if self.unexposed {
            observer::retire_unexposed_view(self.address)?;
            self.unexposed = false;
            self.address = 0;
            return Ok(());
        }
        if self.address == 0 {
            return Ok(());
        }
        let observation = self
            .observation
            .as_ref()
            .context("Native mapped view has no close observation")?;
        if observation.succeeded() {
            return Ok(());
        }
        // An active shared node has not attempted unmapping this original view.
        // It remains retained until the default VFS really retires that node.
        ensure!(
            observation.failed(),
            "Default SQLite shared view still belongs to a live native node"
        );
        observer::retry_failed_view(self.address, observation)
    }
}
struct FactorySlots {
    handles: *mut Vec<HandleWitness>,
    views: *mut Vec<ViewWitness>,
}
thread_local! { static FACTORY: std::cell::RefCell<Option<FactorySlots>> = const { std::cell::RefCell::new(None) }; }
pub(super) struct FactoryScope<'a>(
    std::marker::PhantomData<(
        &'a mut Vec<HandleWitness>,
        &'a mut Vec<ViewWitness>,
        std::rc::Rc<()>,
    )>,
);
impl Drop for FactoryScope<'_> {
    fn drop(&mut self) {
        FACTORY.with(|scope| {
            scope.borrow_mut().take();
        });
    }
}
pub(super) fn factory_scope<'a>(
    handles: &'a mut Vec<HandleWitness>,
    views: &'a mut Vec<ViewWitness>,
) -> Result<FactoryScope<'a>> {
    FACTORY.with(|scope| -> Result<()> {
        let mut scope = scope.borrow_mut();
        ensure!(
            scope.is_none(),
            "SQLite native allocation scope cannot reenter"
        );
        *scope = Some(FactorySlots { handles, views });
        Ok(())
    })?;
    Ok(FactoryScope(std::marker::PhantomData))
}
pub(super) fn reserve_factory_slot(view: bool) -> Result<bool> {
    FACTORY.with(|scope| -> Result<bool> {
        let scope = scope.borrow();
        let Some(scope) = scope.as_ref() else {
            return Ok(false);
        };
        if view {
            let views = unsafe { &mut *scope.views };
            ensure!(
                views.len() < MAX_REGIONS,
                "SQLite view factory quota exceeded"
            );
            views.try_reserve(1)?;
        } else {
            let handles = unsafe { &mut *scope.handles };
            ensure!(
                handles.len() < MAX_WITNESSES,
                "SQLite native handle factory quota exceeded"
            );
            handles.try_reserve(1)?;
        }
        Ok(true)
    })
}
pub(super) fn capture_factory_handle(raw: HANDLE) -> Result<()> {
    FACTORY.with(|scope| -> Result<()> {
        let scope = scope.borrow();
        let scope = scope
            .as_ref()
            .context("SQLite native factory lost its active keeper")?;
        let handles = unsafe { &mut *scope.handles };
        // Capacity was reserved before the OS allocation. Store the real native
        // return under its exclusive creator before duplication or inspection.
        handles.push(HandleWitness {
            original: raw,
            witness: None,
            original_closed: false,
            observation: None,
            unexposed_owner: Some(unsafe { File::from_raw_handle(raw) }),
        });
        let slot = handles.last_mut().unwrap();
        let witness = HandleWitness::capture(raw)?;
        slot.witness = witness.witness;
        slot.register_observation()?;
        // Only a fully retained, registered original is exposed to default C.
        use std::os::windows::io::IntoRawHandle;
        let _ = slot.unexposed_owner.take().unwrap().into_raw_handle();
        Ok(())
    })
}
pub(super) fn capture_factory_view(address: usize) -> Result<()> {
    FACTORY.with(|scope| -> Result<()> {
        let scope = scope.borrow();
        let scope = scope
            .as_ref()
            .context("SQLite view factory lost its active keeper")?;
        let views = unsafe { &mut *scope.views };
        views.push(ViewWitness {
            address,
            observation: None,
            unexposed: true,
        });
        let slot = views.last_mut().unwrap();
        slot.observation = Some(observer::view(address)?);
        slot.unexposed = false;
        Ok(())
    })
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn audited_fixed_windows_x64_prefix_offsets_match_original_sqlite_structs() {
        assert_eq!(size_of::<WinFilePrefix>(), 48);
        assert_eq!(std::mem::offset_of!(WinFilePrefix, handle), 16);
        assert_eq!(std::mem::offset_of!(WinFilePrefix, shm), 40);
        assert_eq!(std::mem::offset_of!(WinShmPrefix, handle), 16);
        assert_eq!(std::mem::offset_of!(WinShmPrefix, next), 32);
        assert_eq!(std::mem::offset_of!(WinShmNodePrefix, shared_handle), 16);
        assert_eq!(std::mem::offset_of!(WinShmNodePrefix, region_count), 40);
        assert_eq!(std::mem::offset_of!(WinShmNodePrefix, regions), 48);
        assert_eq!(std::mem::offset_of!(WinShmNodePrefix, connections), 64);
        assert_eq!(size_of::<ShmRegion>(), 16);
        verify_abi(unsafe { ffi::sqlite3_vfs_find(std::ptr::null()) }).unwrap();
    }
}
fn valid_handle(handle: HANDLE) -> bool {
    !handle.is_null() && handle != INVALID_HANDLE_VALUE
}

pub(super) fn verify_abi(parent: *mut ffi::sqlite3_vfs) -> Result<()> {
    ensure!(
        !parent.is_null(),
        "Actual default Windows SQLite VFS is absent"
    );
    ensure!(
        size_of::<usize>() == 8,
        "UnsupportedCapability: SQLite close ownership requires the audited Windows x64 ABI"
    );
    let source = unsafe { ffi::sqlite3_sourceid() };
    ensure!(
        !source.is_null() && unsafe { CStr::from_ptr(source) }.to_bytes() == SOURCE_ID,
        "UnsupportedCapability: SQLite Windows native ownership ABI source ID changed"
    );
    ensure!(
        unsafe { ffi::sqlite3_libversion_number() } == 3_053_002,
        "UnsupportedCapability: SQLite native ownership ABI version changed"
    );
    let name = unsafe { (*parent).zName };
    ensure!(
        !name.is_null()
            && unsafe { CStr::from_ptr(name) }.to_bytes() == b"win32"
            && unsafe { (*parent).szOsFile } as usize >= size_of::<WinFilePrefix>(),
        "UnsupportedCapability: Actual default SQLite VFS has another native layout"
    );
    Ok(())
}

unsafe fn prefix(file: *mut ffi::sqlite3_file) -> Result<&'static WinFilePrefix> {
    ensure!(!file.is_null(), "SQLite actual accessor is missing");
    let prefix = unsafe { &*file.cast::<WinFilePrefix>() };
    verify_abi(prefix.vfs)?;
    ensure!(
        prefix.methods == unsafe { (*file).pMethods },
        "SQLite accessor method prefix changed"
    );
    Ok(prefix)
}

/// Called while the actual accessor is still live, before default unmap/close.
/// Every observed original and its duplicate is stored in its retained keeper.
pub(super) unsafe fn capture_main(
    file: *mut ffi::sqlite3_file,
    witnesses: &mut Vec<HandleWitness>,
) -> Result<()> {
    let prefix = unsafe { prefix(file) }?;
    let mut actual = std::ptr::null_mut();
    let control = unsafe { (*file).pMethods.as_ref() }
        .and_then(|m| m.xFileControl)
        .context("Default SQLite actual native file control is missing")?;
    ensure!(
        unsafe {
            control(
                file,
                ffi::SQLITE_FCNTL_WIN32_GET_HANDLE,
                (&mut actual as *mut HANDLE).cast(),
            )
        } == ffi::SQLITE_OK
            && actual == prefix.handle,
        "Default SQLite native prefix differs from actual GET_HANDLE accessor"
    );
    capture(witnesses, prefix.handle)
}
pub(super) unsafe fn main_has_sqlite_header(file: *mut ffi::sqlite3_file) -> Result<bool> {
    let prefix = unsafe { prefix(file) }?;
    let file = std::mem::ManuallyDrop::new(unsafe { File::from_raw_handle(prefix.handle) });
    platform::validate(&file, false)?;
    if file.metadata()?.len() < 100 {
        return Ok(false);
    }
    let mut header = [0u8; 16];
    ensure!(
        platform::read_at(&file, &mut header, 0)? == header.len(),
        "SQLite main header could not be read from its actual native accessor"
    );
    Ok(&header == b"SQLite format 3\0")
}
#[cfg(test)]
pub(super) unsafe fn actual_shared_handle_for_test(file: *mut ffi::sqlite3_file) -> Result<HANDLE> {
    let prefix = unsafe { prefix(file) }?;
    ensure!(!prefix.shm.is_null(), "Actual default SHM is not mapped");
    let shm = unsafe { &*prefix.shm };
    ensure!(!shm.node.is_null(), "Actual default SHM node is absent");
    Ok(unsafe { (*shm.node).shared_handle })
}
fn capture(witnesses: &mut Vec<HandleWitness>, raw: HANDLE) -> Result<()> {
    if !valid_handle(raw) {
        return Ok(());
    }
    for witness in witnesses
        .iter_mut()
        .filter(|w| w.original() == raw && !w.original_closed)
    {
        if witness.same_original()? {
            return witness.register_observation();
        }
        witness.original_closed = true;
    }
    ensure!(
        witnesses.len() < MAX_WITNESSES,
        "SQLite native close witness quota exceeded"
    );
    witnesses.try_reserve(1)?;
    witnesses.push(HandleWitness::capture(raw)?);
    witnesses.last_mut().unwrap().register_observation()
}
pub(super) unsafe fn capture_shm(
    file: *mut ffi::sqlite3_file,
    witnesses: &mut Vec<HandleWitness>,
    views: &mut Vec<ViewWitness>,
) -> Result<Vec<HANDLE>> {
    let prefix = unsafe { prefix(file) }?;
    if prefix.shm.is_null() {
        return Ok(Vec::new());
    }
    let shm = unsafe { &*prefix.shm };
    ensure!(
        !shm.node.is_null(),
        "Default SQLite SHM lost its actual node"
    );
    // This per-node mutex is the lock used by the original VFS to mutate its
    // connection list. It is released before calling any default VFS method.
    let node = unsafe { &*shm.node };
    unsafe { ffi::sqlite3_mutex_enter(node.mutex) };
    struct Unlock(*mut ffi::sqlite3_mutex);
    impl Drop for Unlock {
        fn drop(&mut self) {
            unsafe { ffi::sqlite3_mutex_leave(self.0) };
        }
    }
    let _unlock = Unlock(node.mutex);
    ensure!(
        node.region_count >= 0 && node.region_count as usize <= MAX_REGIONS,
        "Default SQLite SHM mapping count exceeded its bounded native keeper"
    );
    ensure!(
        node.region_count == 0 || !node.regions.is_null(),
        "SQLite mapping array is missing"
    );
    capture(witnesses, shm.handle)?;
    capture(witnesses, node.shared_handle)?;
    for region in 0..node.region_count as usize {
        let region = unsafe { &*node.regions.add(region) };
        capture(witnesses, region.mapping)?;
        let address = region.view as usize;
        if address != 0
            && !views.iter().any(|view| {
                view.address == address
                    && !view
                        .observation
                        .as_ref()
                        .is_some_and(|watch| watch.succeeded())
            })
        {
            ensure!(
                views.len() < MAX_REGIONS,
                "SQLite retained native mapped view quota exceeded"
            );
            views.try_reserve(1)?;
            let observation = observer::view(address)?;
            views.push(ViewWitness {
                address,
                observation: Some(observation),
                unexposed: false,
            });
        }
    }
    Ok([shm.handle, node.shared_handle]
        .into_iter()
        .filter(|raw| valid_handle(*raw))
        .collect())
}

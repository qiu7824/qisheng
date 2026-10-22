//! Observes the original Windows VFS's actual OS close/unmap return values.
//! The hooks forward once to the checked original SQLite syscall pointers.
use super::*;
use std::{
    collections::BTreeMap,
    sync::{
        Weak,
        atomic::{AtomicU8, AtomicUsize},
    },
};
use windows_sys::Win32::Foundation::{GetLastError, SetLastError};
use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};

const NEVER: u8 = 0;
const SUCCEEDED: u8 = 1;
const FAILED: u8 = 2;
const MAX_OBSERVATIONS: usize = 4 * 8198;
type CloseCall = unsafe extern "system" fn(HANDLE) -> i32;
type UnmapCall = unsafe extern "system" fn(*const std::ffi::c_void) -> i32;
type CreateFileCall = unsafe extern "system" fn(
    *const u16,
    u32,
    u32,
    *const std::ffi::c_void,
    u32,
    u32,
    HANDLE,
) -> HANDLE;
type CreateMappingCall =
    unsafe extern "system" fn(HANDLE, *const std::ffi::c_void, u32, u32, u32, *const u16) -> HANDLE;
type MapViewCall = unsafe extern "system" fn(HANDLE, u32, u32, u32, usize) -> *mut std::ffi::c_void;
static ORIGINAL_CLOSE: AtomicUsize = AtomicUsize::new(0);
static ORIGINAL_UNMAP: AtomicUsize = AtomicUsize::new(0);
static ORIGINAL_CREATE_FILE: AtomicUsize = AtomicUsize::new(0);
static ORIGINAL_CREATE_MAPPING: AtomicUsize = AtomicUsize::new(0);
static ORIGINAL_MAP_VIEW: AtomicUsize = AtomicUsize::new(0);
static INSTALL: Mutex<()> = Mutex::new(());

pub(super) struct HandleObservation {
    duplicate: AtomicUsize,
    result: AtomicU8,
}
impl HandleObservation {
    pub(super) fn result(&self) -> u8 {
        self.result.load(Ordering::Acquire)
    }
    pub(super) fn duplicate(&self, raw: HANDLE) {
        self.duplicate.store(raw as usize, Ordering::Release);
    }
    pub(super) fn succeeded(&self) -> bool {
        self.result() == SUCCEEDED
    }
}
pub(super) struct ViewObservation {
    result: AtomicU8,
}
impl ViewObservation {
    pub(super) fn succeeded(&self) -> bool {
        self.result.load(Ordering::Acquire) == SUCCEEDED
    }
    pub(super) fn failed(&self) -> bool {
        self.result.load(Ordering::Acquire) == FAILED
    }
}
static HANDLES: Mutex<BTreeMap<usize, Vec<Weak<HandleObservation>>>> = Mutex::new(BTreeMap::new());
static VIEWS: Mutex<BTreeMap<usize, Vec<Weak<ViewObservation>>>> = Mutex::new(BTreeMap::new());

pub(super) fn handle(original: HANDLE, duplicate: HANDLE) -> Result<Arc<HandleObservation>> {
    let mut handles = HANDLES.lock().unwrap_or_else(|error| error.into_inner());
    handles.retain(|_, list| {
        list.retain(|watch| watch.strong_count() != 0);
        !list.is_empty()
    });
    let count = handles.values().map(Vec::len).sum::<usize>();
    ensure!(
        count < MAX_OBSERVATIONS,
        "SQLite actual handle observation quota exceeded"
    );
    let watch = Arc::new(HandleObservation {
        duplicate: AtomicUsize::new(duplicate as usize),
        result: AtomicU8::new(NEVER),
    });
    handles
        .entry(original as usize)
        .or_default()
        .push(Arc::downgrade(&watch));
    Ok(watch)
}
pub(super) fn view(original: usize) -> Result<Arc<ViewObservation>> {
    ensure!(original != 0, "SQLite actual mapped view is absent");
    let mut views = VIEWS.lock().unwrap_or_else(|error| error.into_inner());
    views.retain(|_, list| {
        list.retain(|watch| watch.strong_count() != 0);
        !list.is_empty()
    });
    let count = views.values().map(Vec::len).sum::<usize>();
    ensure!(
        count < MAX_OBSERVATIONS,
        "SQLite actual mapped view observation quota exceeded"
    );
    let watch = Arc::new(ViewObservation {
        result: AtomicU8::new(NEVER),
    });
    views
        .entry(original)
        .or_default()
        .push(Arc::downgrade(&watch));
    Ok(watch)
}
pub(super) fn install(parent: *mut ffi::sqlite3_vfs) -> Result<()> {
    let _install = INSTALL.lock().unwrap_or_else(|error| error.into_inner());
    let get = unsafe { (*parent).xGetSystemCall }
        .context("Default SQLite native syscall getter is absent")?;
    let set = unsafe { (*parent).xSetSystemCall }
        .context("Default SQLite native syscall setter is absent")?;
    let kernel_name = "kernel32.dll\0".encode_utf16().collect::<Vec<_>>();
    let kernel = unsafe { GetModuleHandleW(kernel_name.as_ptr()) };
    ensure!(
        !kernel.is_null(),
        "Actual Windows kernel module is unavailable"
    );
    for (name, original, hook) in [
        (
            c"CloseHandle",
            &ORIGINAL_CLOSE,
            close_hook as *const () as usize,
        ),
        (
            c"UnmapViewOfFile",
            &ORIGINAL_UNMAP,
            unmap_hook as *const () as usize,
        ),
        (
            c"CreateFileW",
            &ORIGINAL_CREATE_FILE,
            create_file_hook as *const () as usize,
        ),
        (
            c"CreateFileMappingW",
            &ORIGINAL_CREATE_MAPPING,
            create_mapping_hook as *const () as usize,
        ),
        (
            c"MapViewOfFile",
            &ORIGINAL_MAP_VIEW,
            map_view_hook as *const () as usize,
        ),
    ] {
        let current = unsafe { get(parent, name.as_ptr()) }
            .context("Default SQLite required native syscall is absent")?
            as usize;
        let system = unsafe { GetProcAddress(kernel, name.as_ptr().cast()) }
            .context("Actual Windows kernel syscall is unavailable")? as usize;
        if original.load(Ordering::Acquire) == 0 {
            ensure!(
                current == system,
                "UnsupportedCapability: Default SQLite native syscall is already redirected"
            );
            original.store(current, Ordering::Release);
            let erased: unsafe extern "C" fn() = unsafe { std::mem::transmute(hook) };
            ensure!(
                unsafe { set(parent, name.as_ptr(), Some(erased)) } == ffi::SQLITE_OK,
                "Cannot install actual SQLite native close observation"
            );
        }
        ensure!(
            original.load(Ordering::Acquire) == system,
            "UnsupportedCapability: Original SQLite native syscall differs from the Windows kernel entry point"
        );
        ensure!(
            unsafe { get(parent, name.as_ptr()) }.is_some_and(|current| current as usize == hook),
            "UnsupportedCapability: SQLite close observation was replaced by another native syscall"
        );
    }
    Ok(())
}
unsafe extern "system" fn create_file_hook(
    name: *const u16,
    access: u32,
    sharing: u32,
    security: *const std::ffi::c_void,
    disposition: u32,
    flags: u32,
    template: HANDLE,
) -> HANDLE {
    let tracked = match super::reserve_factory_slot(false) {
        Ok(tracked) => tracked,
        Err(_) => {
            unsafe { SetLastError(8) };
            return INVALID_HANDLE_VALUE;
        }
    };
    let original: CreateFileCall =
        unsafe { std::mem::transmute(ORIGINAL_CREATE_FILE.load(Ordering::Acquire)) };
    let raw = unsafe {
        original(
            name,
            access,
            sharing,
            security,
            disposition,
            flags,
            template,
        )
    };
    let last_error = unsafe { GetLastError() };
    if tracked && valid_handle(raw) && super::capture_factory_handle(raw).is_err() {
        // The unexposed actual original remains in the pre-admitted keeper.
        unsafe { SetLastError(8) };
        return INVALID_HANDLE_VALUE;
    }
    unsafe { SetLastError(last_error) };
    raw
}
unsafe extern "system" fn create_mapping_hook(
    file: HANDLE,
    security: *const std::ffi::c_void,
    protection: u32,
    high: u32,
    low: u32,
    name: *const u16,
) -> HANDLE {
    let tracked = match super::reserve_factory_slot(false) {
        Ok(tracked) => tracked,
        Err(_) => {
            unsafe { SetLastError(8) };
            return std::ptr::null_mut();
        }
    };
    let original: CreateMappingCall =
        unsafe { std::mem::transmute(ORIGINAL_CREATE_MAPPING.load(Ordering::Acquire)) };
    let raw = unsafe { original(file, security, protection, high, low, name) };
    let last_error = unsafe { GetLastError() };
    if tracked && valid_handle(raw) && super::capture_factory_handle(raw).is_err() {
        unsafe { SetLastError(8) };
        return std::ptr::null_mut();
    }
    unsafe { SetLastError(last_error) };
    raw
}
unsafe extern "system" fn map_view_hook(
    mapping: HANDLE,
    access: u32,
    high: u32,
    low: u32,
    bytes: usize,
) -> *mut std::ffi::c_void {
    let tracked = match super::reserve_factory_slot(true) {
        Ok(tracked) => tracked,
        Err(_) => {
            unsafe { SetLastError(8) };
            return std::ptr::null_mut();
        }
    };
    let original: MapViewCall =
        unsafe { std::mem::transmute(ORIGINAL_MAP_VIEW.load(Ordering::Acquire)) };
    let address = unsafe { original(mapping, access, high, low, bytes) };
    let last_error = unsafe { GetLastError() };
    if tracked && !address.is_null() && super::capture_factory_view(address as usize).is_err() {
        unsafe { SetLastError(8) };
        return std::ptr::null_mut();
    }
    unsafe { SetLastError(last_error) };
    address
}
unsafe extern "system" fn close_hook(raw: HANDLE) -> i32 {
    let original: CloseCall =
        unsafe { std::mem::transmute(ORIGINAL_CLOSE.load(Ordering::Acquire)) };
    let watches = HANDLES
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .get(&(raw as usize))
        .map(|list| list.iter().filter_map(Weak::upgrade).collect::<Vec<_>>())
        .unwrap_or_default();
    // Bind observations to the same Kernel object before the OS call. A reused
    // HANDLE value does not inherit an older object's close acknowledgement.
    let bound = watches
        .into_iter()
        .filter(|watch| {
            let duplicate = watch.duplicate.load(Ordering::Acquire) as HANDLE;
            !watch.succeeded()
                && !duplicate.is_null()
                && unsafe { CompareObjectHandles(raw, duplicate) } != 0
        })
        .collect::<Vec<_>>();
    let result = unsafe { original(raw) };
    let last_error = unsafe { GetLastError() };
    for watch in bound {
        watch.result.store(
            if result != 0 { SUCCEEDED } else { FAILED },
            Ordering::Release,
        );
    }
    unsafe { SetLastError(last_error) };
    result
}
unsafe extern "system" fn unmap_hook(address: *const std::ffi::c_void) -> i32 {
    let original: UnmapCall =
        unsafe { std::mem::transmute(ORIGINAL_UNMAP.load(Ordering::Acquire)) };
    let watches = VIEWS
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .get(&(address as usize))
        .map(|list| {
            list.iter()
                .filter_map(Weak::upgrade)
                .filter(|watch| !watch.succeeded())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let result = unsafe { original(address) };
    let last_error = unsafe { GetLastError() };
    for watch in watches {
        watch.result.store(
            if result != 0 { SUCCEEDED } else { FAILED },
            Ordering::Release,
        );
    }
    unsafe { SetLastError(last_error) };
    result
}
/// A failed original unmap retains this exact view allocation; successful
/// observations remain immutable even if the virtual address is reused later.
pub(super) fn retry_failed_view(address: usize, watch: &ViewObservation) -> Result<()> {
    ensure!(
        watch.failed(),
        "SQLite shared view has no failed actual unmap to retry"
    );
    ensure!(
        unsafe { unmap_hook(address as *const std::ffi::c_void) } != 0,
        "Actual original SQLite view unmap retry failed"
    );
    Ok(())
}
pub(super) fn retire_unexposed_view(address: usize) -> Result<()> {
    ensure!(
        unsafe { unmap_hook(address as *const std::ffi::c_void) } != 0,
        "Native unexposed SQLite view unmap failed"
    );
    Ok(())
}
pub(super) fn actual_close_attempted(watch: &HandleObservation) -> bool {
    watch.result() != NEVER
}

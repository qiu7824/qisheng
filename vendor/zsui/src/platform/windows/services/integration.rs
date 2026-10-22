// Desktop integration on Win32: single-instance launches, pointer cursors,
// dropped files, system theme changes, window placement, tray notifications
// and the host declarations (file drop, taskbar, media session) a View makes.

const ZSUI_WIN32_SINGLE_INSTANCE_COPYDATA: usize = 0x5A53_4931;

static ZSUI_WIN32_SINGLE_INSTANCE_CLAIMS: OnceLock<Mutex<Vec<(String, isize)>>> = OnceLock::new();

thread_local! {
    /// Windows registered as the running instance, with their property name.
    static ZSUI_WIN32_SINGLE_INSTANCE_WINDOWS: std::cell::RefCell<Vec<(isize, Vec<u16>)>> =
        const { std::cell::RefCell::new(Vec::new()) };
    /// The host declarations last applied to each window.
    static ZSUI_WIN32_HOST_DECLARATIONS: std::cell::RefCell<Vec<(isize, crate::ZsHostDeclarations)>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

fn windows_win32_single_instance_key(id: &str) -> String {
    id.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn windows_win32_single_instance_property(id: &str) -> Vec<u16> {
    wide_null(&format!(
        "zsui.instance.{}",
        windows_win32_single_instance_key(id)
    ))
}

/// Claims application `id` for this process; `false` when another process
/// already holds it. The claim lasts until the process exits.
fn claim_windows_win32_single_instance(id: &str) -> bool {
    let key = windows_win32_single_instance_key(id);
    let claims = ZSUI_WIN32_SINGLE_INSTANCE_CLAIMS.get_or_init(|| Mutex::new(Vec::new()));
    let mut claims = claims.lock().unwrap_or_else(|error| error.into_inner());
    if claims.iter().any(|(claimed, _)| *claimed == key) {
        return true;
    }
    let name = wide_null(&format!("Local\\zsui.instance.{key}"));
    unsafe {
        let handle = windows_sys::Win32::System::Threading::CreateMutexW(null(), 0, name.as_ptr());
        if handle.is_null() {
            // Without a mutex the process cannot tell; run normally.
            return true;
        }
        if GetLastError() == windows_sys::Win32::Foundation::ERROR_ALREADY_EXISTS {
            windows_sys::Win32::Foundation::CloseHandle(handle);
            return false;
        }
        claims.push((key, handle as isize));
    }
    true
}

/// See [`crate::zs_forward_to_running_instance`].
pub(crate) fn zs_forward_to_running_instance_win32(id: &str) -> bool {
    if claim_windows_win32_single_instance(id) {
        return false;
    }
    let args: Vec<String> = std::env::args().skip(1).collect();
    let payload: Vec<u16> = args.join("\u{1f}").encode_utf16().collect();
    // The running instance may still be starting; give its window time.
    for _ in 0..60 {
        if let Some(hwnd) = find_windows_win32_single_instance_window(id) {
            unsafe {
                use windows_sys::Win32::System::DataExchange::COPYDATASTRUCT;
                use windows_sys::Win32::UI::WindowsAndMessaging::{
                    AllowSetForegroundWindow, GetWindowThreadProcessId, SMTO_ABORTIFHUNG,
                    SendMessageTimeoutW, WM_COPYDATA,
                };
                let mut process = 0u32;
                GetWindowThreadProcessId(hwnd, &mut process);
                AllowSetForegroundWindow(process);
                let data = COPYDATASTRUCT {
                    dwData: ZSUI_WIN32_SINGLE_INSTANCE_COPYDATA,
                    cbData: (payload.len() * 2) as u32,
                    lpData: payload.as_ptr() as *mut core::ffi::c_void,
                };
                let mut result = 0usize;
                SendMessageTimeoutW(
                    hwnd,
                    WM_COPYDATA,
                    0,
                    &data as *const COPYDATASTRUCT as LPARAM,
                    SMTO_ABORTIFHUNG,
                    5000,
                    &mut result,
                );
            }
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    // Another process owns the application; do not start a second copy.
    true
}

fn find_windows_win32_single_instance_window(id: &str) -> Option<HWND> {
    struct Search {
        property: Vec<u16>,
        found: HWND,
    }
    unsafe extern "system" fn visit(hwnd: HWND, lparam: LPARAM) -> i32 {
        let search = unsafe { &mut *(lparam as *mut Search) };
        let marked = unsafe {
            !windows_sys::Win32::UI::WindowsAndMessaging::GetPropW(hwnd, search.property.as_ptr())
                .is_null()
        };
        if marked {
            search.found = hwnd;
            0
        } else {
            1
        }
    }
    let mut search = Search {
        property: windows_win32_single_instance_property(id),
        found: null_mut(),
    };
    unsafe {
        windows_sys::Win32::UI::WindowsAndMessaging::EnumWindows(
            Some(visit),
            &mut search as *mut Search as LPARAM,
        );
    }
    (!search.found.is_null()).then_some(search.found)
}

/// Marks `hwnd` as the window of running application `id`.
pub(crate) fn register_windows_win32_single_instance_window(hwnd: HWND, id: &str) {
    claim_windows_win32_single_instance(id);
    let property = windows_win32_single_instance_property(id);
    unsafe {
        windows_sys::Win32::UI::WindowsAndMessaging::SetPropW(
            hwnd,
            property.as_ptr(),
            1usize as windows_sys::Win32::Foundation::HANDLE,
        );
    }
    ZSUI_WIN32_SINGLE_INSTANCE_WINDOWS.with(|windows| {
        windows.borrow_mut().push((hwnd as isize, property));
    });
}

fn clear_windows_win32_single_instance_window(hwnd: HWND) {
    let _ = ZSUI_WIN32_SINGLE_INSTANCE_WINDOWS.try_with(|windows| {
        windows.borrow_mut().retain(|(owner, property)| {
            if *owner != hwnd as isize {
                return true;
            }
            unsafe {
                windows_sys::Win32::UI::WindowsAndMessaging::RemovePropW(hwnd, property.as_ptr());
            }
            false
        });
    });
}

/// Handles the `WM_COPYDATA` a second launch sends: brings the window to
/// the front and reports the launch's arguments.
fn handle_windows_win32_single_instance_copydata(hwnd: HWND, lparam: LPARAM) -> bool {
    use windows_sys::Win32::System::DataExchange::COPYDATASTRUCT;
    if lparam == 0 {
        return false;
    }
    let data = unsafe { &*(lparam as *const COPYDATASTRUCT) };
    if data.dwData != ZSUI_WIN32_SINGLE_INSTANCE_COPYDATA {
        return false;
    }
    let units = data.cbData as usize / 2;
    let text = if data.lpData.is_null() || units == 0 {
        String::new()
    } else {
        String::from_utf16_lossy(unsafe {
            std::slice::from_raw_parts(data.lpData as *const u16, units)
        })
    };
    let args = if text.is_empty() {
        Vec::new()
    } else {
        text.split('\u{1f}').map(str::to_owned).collect()
    };
    unsafe {
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            IsIconic, SW_RESTORE, SW_SHOW, SetForegroundWindow, ShowWindow,
        };
        ShowWindow(hwnd, if IsIconic(hwnd) != 0 { SW_RESTORE } else { SW_SHOW });
        SetForegroundWindow(hwnd);
    }
    dispatch_windows_win32_host_event(hwnd, crate::ZsHostEvent::SecondInstance(args));
    true
}

/// Delivers a host event to the window's View.
pub(crate) fn dispatch_windows_win32_host_event(hwnd: HWND, event: crate::ZsHostEvent) -> bool {
    dispatch_windows_win32_window_view_input(hwnd, |route| route.dispatch_host_event(event))
        .is_some_and(|report| report.handled)
}

/// Whether a `WM_SETTINGCHANGE` announces a light/dark app theme change.
fn windows_win32_setting_change_is_theme(lparam: LPARAM) -> bool {
    if lparam == 0 {
        return false;
    }
    let expected: Vec<u16> = "ImmersiveColorSet".encode_utf16().collect();
    let text = lparam as *const u16;
    for (index, unit) in expected.iter().enumerate() {
        if unsafe { *text.add(index) } != *unit {
            return false;
        }
    }
    unsafe { *text.add(expected.len()) == 0 }
}

fn windows_win32_cursor_resource(cursor: crate::ZsCursor) -> windows_sys::core::PCWSTR {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        IDC_APPSTARTING, IDC_ARROW, IDC_HAND, IDC_IBEAM, IDC_NO, IDC_SIZEALL, IDC_SIZENS,
        IDC_SIZEWE,
    };
    match cursor {
        crate::ZsCursor::Arrow => IDC_ARROW,
        crate::ZsCursor::Pointer => IDC_HAND,
        crate::ZsCursor::Text => IDC_IBEAM,
        crate::ZsCursor::Move => IDC_SIZEALL,
        crate::ZsCursor::ResizeHorizontal => IDC_SIZEWE,
        crate::ZsCursor::ResizeVertical => IDC_SIZENS,
        crate::ZsCursor::NotAllowed => IDC_NO,
        crate::ZsCursor::Progress => IDC_APPSTARTING,
    }
}

/// Sets the cursor the View asks for under the pointer; `false` when the
/// window has no View route.
fn apply_windows_win32_view_cursor(hwnd: HWND) -> bool {
    let mut point = POINT { x: 0, y: 0 };
    unsafe {
        if windows_sys::Win32::UI::WindowsAndMessaging::GetCursorPos(&mut point) == 0
            || windows_sys::Win32::Graphics::Gdi::ScreenToClient(hwnd, &mut point) == 0
        {
            return false;
        }
    }
    let cursor = {
        let routes = window_view_input_routes()
            .lock()
            .expect("window view input route registry should not be poisoned");
        routes
            .iter()
            .find(|record| record.hwnd == hwnd as isize)
            .map(|record| {
                record.route.shared_runtime.cursor_at(crate::Point {
                    x: point.x,
                    y: point.y,
                })
            })
    };
    let Some(cursor) = cursor else {
        return false;
    };
    unsafe {
        let handle = LoadCursorW(null_mut(), windows_win32_cursor_resource(cursor));
        windows_sys::Win32::UI::WindowsAndMessaging::SetCursor(handle);
    }
    true
}

/// Reads and releases the files of a `WM_DROPFILES`.
fn take_windows_win32_dropped_files(drop: WPARAM) -> Vec<std::path::PathBuf> {
    use std::os::windows::ffi::OsStringExt;
    use windows_sys::Win32::UI::Shell::{DragFinish, DragQueryFileW, HDROP};
    let drop = drop as HDROP;
    let mut paths = Vec::new();
    unsafe {
        let count = DragQueryFileW(drop, u32::MAX, null_mut(), 0);
        for index in 0..count {
            let length = DragQueryFileW(drop, index, null_mut(), 0);
            if length == 0 {
                continue;
            }
            let mut buffer = vec![0u16; length as usize + 1];
            let copied = DragQueryFileW(drop, index, buffer.as_mut_ptr(), length + 1);
            buffer.truncate(copied as usize);
            paths.push(std::path::PathBuf::from(std::ffi::OsString::from_wide(&buffer)));
        }
        DragFinish(drop);
    }
    paths
}

/// The window's restored bounds and maximized state.
#[cfg(feature = "window-chrome")]
pub(crate) fn windows_win32_window_placement(hwnd: HWND) -> Option<crate::ZsWindowPlacement> {
    use windows_sys::Win32::UI::WindowsAndMessaging::{GetWindowPlacement, WINDOWPLACEMENT};
    let mut placement: WINDOWPLACEMENT = unsafe { zeroed() };
    placement.length = size_of::<WINDOWPLACEMENT>() as u32;
    if unsafe { GetWindowPlacement(hwnd, &mut placement) } == 0 {
        return None;
    }
    let rect = placement.rcNormalPosition;
    Some(crate::ZsWindowPlacement {
        x: rect.left,
        y: rect.top,
        width: rect.right - rect.left,
        height: rect.bottom - rect.top,
        maximized: unsafe { windows_sys::Win32::UI::WindowsAndMessaging::IsZoomed(hwnd) } != 0,
    })
}

/// Restores a saved placement when it is still on a connected monitor;
/// `show` also shows the window in that state.
pub(crate) fn apply_windows_win32_window_placement(
    hwnd: HWND,
    saved: crate::ZsWindowPlacement,
    show: bool,
) -> bool {
    use windows_sys::Win32::Graphics::Gdi::{MONITOR_DEFAULTTONULL, MonitorFromRect};
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetWindowPlacement, SW_HIDE, SW_SHOWMAXIMIZED, SW_SHOWNORMAL, SetWindowPlacement,
        WINDOWPLACEMENT, WPF_RESTORETOMAXIMIZED,
    };
    if saved.width < 64 || saved.height < 64 {
        return false;
    }
    let rect = RECT {
        left: saved.x,
        top: saved.y,
        right: saved.x.saturating_add(saved.width),
        bottom: saved.y.saturating_add(saved.height),
    };
    if unsafe { MonitorFromRect(&rect, MONITOR_DEFAULTTONULL) }.is_null() {
        return false;
    }
    let mut placement: WINDOWPLACEMENT = unsafe { zeroed() };
    placement.length = size_of::<WINDOWPLACEMENT>() as u32;
    if unsafe { GetWindowPlacement(hwnd, &mut placement) } == 0 {
        return false;
    }
    placement.rcNormalPosition = rect;
    placement.showCmd = if !show {
        SW_HIDE as u32
    } else if saved.maximized {
        SW_SHOWMAXIMIZED as u32
    } else {
        SW_SHOWNORMAL as u32
    };
    if saved.maximized {
        placement.flags |= WPF_RESTORETOMAXIMIZED;
    }
    unsafe { SetWindowPlacement(hwnd, &placement) != 0 }
}

/// Applies the View's host declarations after a rebuild.
pub(crate) fn sync_windows_win32_view_host_declarations(hwnd: HWND) {
    if hwnd.is_null() {
        return;
    }
    let declared = {
        let routes = window_view_input_routes()
            .lock()
            .expect("window view input route registry should not be poisoned");
        routes
            .iter()
            .find(|record| record.hwnd == hwnd as isize)
            .and_then(|record| record.route.shared_runtime.declared_host())
    };
    let Some(declared) = declared else {
        return;
    };
    let previous = ZSUI_WIN32_HOST_DECLARATIONS.with(|all| {
        all.borrow()
            .iter()
            .find(|(owner, _)| *owner == hwnd as isize)
            .map(|(_, declarations)| declarations.clone())
    });
    if previous.as_ref() == Some(&declared) {
        return;
    }
    let previous = previous.unwrap_or_default();
    if previous.accepts_files != declared.accepts_files {
        unsafe {
            windows_sys::Win32::UI::Shell::DragAcceptFiles(hwnd, i32::from(declared.accepts_files));
        }
    }
    #[cfg(feature = "tray")]
    if let Some(notification) = declared.new_notification(&previous) {
        notify_windows_win32_view_status_item(
            hwnd,
            &notification.title,
            &notification.body,
            notification.silent,
        );
    }
    #[cfg(feature = "shell-integration")]
    {
        if previous.taskbar != declared.taskbar {
            apply_windows_win32_taskbar(hwnd, declared.taskbar.as_ref());
        }
        if previous.media_session != declared.media_session {
            apply_windows_win32_media_session(hwnd, declared.media_session.as_ref());
        }
    }
    ZSUI_WIN32_HOST_DECLARATIONS.with(|all| {
        let mut all = all.borrow_mut();
        all.retain(|(owner, _)| *owner != hwnd as isize);
        all.push((hwnd as isize, declared));
    });
}

/// Forgets a closing window's declarations and integrations.
fn clear_windows_win32_view_host_integration(hwnd: HWND) {
    let _ = ZSUI_WIN32_HOST_DECLARATIONS.try_with(|all| {
        all.borrow_mut().retain(|(owner, _)| *owner != hwnd as isize);
    });
    clear_windows_win32_single_instance_window(hwnd);
    #[cfg(feature = "shell-integration")]
    clear_windows_win32_shell_integration(hwnd);
}

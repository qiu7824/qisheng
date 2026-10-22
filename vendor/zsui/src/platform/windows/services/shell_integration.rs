// Taskbar thumbnail buttons and progress (ITaskbarList3) and the system media
// session (SystemMediaTransportControls) for View declarations. Media key and
// headset presses arrive on a WinRT thread and are posted back to the window.

const ZSUI_WIN32_TASKBAR_BUTTON_BASE: u32 = 0x5B00;
const ZSUI_WIN32_TASKBAR_BUTTON_COUNT: usize = 7;
const ZSUI_WIN32_MEDIA_COMMAND_MESSAGE: u32 =
    windows_sys::Win32::UI::WindowsAndMessaging::WM_APP + 0x0A4D;

struct WindowsWin32TaskbarState {
    owner: isize,
    list: Option<::windows::Win32::UI::Shell::ITaskbarList3>,
    buttons_added: bool,
    spec: Option<crate::ZsTaskbarSpec>,
    icons: Vec<isize>,
}

struct WindowsWin32MediaSessionState {
    owner: isize,
    controls: ::windows::Media::SystemMediaTransportControls,
    token: i64,
    applied: Option<crate::ZsMediaSession>,
}

thread_local! {
    static ZSUI_WIN32_TASKBARS: std::cell::RefCell<Vec<WindowsWin32TaskbarState>> =
        const { std::cell::RefCell::new(Vec::new()) };
    static ZSUI_WIN32_MEDIA_SESSIONS: std::cell::RefCell<Vec<WindowsWin32MediaSessionState>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

fn windows_win32_com_hwnd(hwnd: HWND) -> ::windows::Win32::Foundation::HWND {
    ::windows::Win32::Foundation::HWND(hwnd)
}

fn windows_win32_ensure_com_apartment() {
    unsafe {
        let _ = ::windows::Win32::System::Com::CoInitializeEx(
            None,
            ::windows::Win32::System::Com::COINIT_APARTMENTTHREADED,
        );
    }
}

/// The registered message Explorer sends once the taskbar button exists;
/// thumbnail buttons can only be added after it.
pub(crate) fn windows_win32_taskbar_button_created_message() -> u32 {
    static MESSAGE: OnceLock<u32> = OnceLock::new();
    *MESSAGE.get_or_init(|| {
        let name = wide_null("TaskbarButtonCreated");
        unsafe { windows_sys::Win32::UI::WindowsAndMessaging::RegisterWindowMessageW(name.as_ptr()) }
    })
}

/// Draws an icon-font glyph into an icon: white on dark taskbar previews,
/// near-black on light ones.
fn windows_win32_glyph_icon(glyph: char, size: i32, light: bool) -> isize {
    use windows_sys::Win32::Graphics::Gdi::{
        ANTIALIASED_QUALITY, BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CLIP_DEFAULT_PRECIS,
        CreateBitmap, CreateCompatibleDC, CreateDIBSection, CreateFontW, DEFAULT_CHARSET,
        DEFAULT_PITCH, DIB_RGB_COLORS, DT_CENTER, DT_NOPREFIX, DT_SINGLELINE, DT_VCENTER,
        DeleteDC, DeleteObject, DrawTextW, FW_NORMAL, GdiFlush, OUT_DEFAULT_PRECIS, SelectObject,
        SetBkMode, SetTextColor, TRANSPARENT,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{CreateIconIndirect, ICONINFO};
    let size = size.clamp(12, 64);
    unsafe {
        let dc = CreateCompatibleDC(null_mut());
        if dc.is_null() {
            return 0;
        }
        let mut info: BITMAPINFO = zeroed();
        info.bmiHeader.biSize = size_of::<BITMAPINFOHEADER>() as u32;
        info.bmiHeader.biWidth = size;
        info.bmiHeader.biHeight = -size;
        info.bmiHeader.biPlanes = 1;
        info.bmiHeader.biBitCount = 32;
        info.bmiHeader.biCompression = BI_RGB;
        let mut bits: *mut core::ffi::c_void = null_mut();
        let color = CreateDIBSection(dc, &info, DIB_RGB_COLORS, &mut bits, null_mut(), 0);
        if color.is_null() || bits.is_null() {
            DeleteDC(dc);
            return 0;
        }
        let previous = SelectObject(dc, color);
        let pixels = std::slice::from_raw_parts_mut(bits as *mut u32, (size * size) as usize);
        pixels.fill(0);
        let family = wide_null("Segoe MDL2 Assets");
        let font = CreateFontW(
            -(size * 3 / 4),
            0,
            0,
            0,
            FW_NORMAL as i32,
            0,
            0,
            0,
            DEFAULT_CHARSET as u32,
            OUT_DEFAULT_PRECIS as u32,
            CLIP_DEFAULT_PRECIS as u32,
            ANTIALIASED_QUALITY as u32,
            DEFAULT_PITCH as u32,
            family.as_ptr(),
        );
        let previous_font = SelectObject(dc, font);
        SetBkMode(dc, TRANSPARENT as i32);
        SetTextColor(dc, 0x00FF_FFFF);
        let mut text: Vec<u16> = glyph.to_string().encode_utf16().collect();
        let mut rect = RECT {
            left: 0,
            top: 0,
            right: size,
            bottom: size,
        };
        DrawTextW(
            dc,
            text.as_mut_ptr(),
            text.len() as i32,
            &mut rect,
            DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX,
        );
        GdiFlush();
        // Grayscale coverage becomes premultiplied alpha of the icon color.
        let ink: u32 = if light { 0x1e } else { 0xff };
        for pixel in pixels.iter_mut() {
            let coverage = (*pixel >> 16 & 0xff).max(*pixel >> 8 & 0xff).max(*pixel & 0xff);
            let value = ink * coverage / 255;
            *pixel = (coverage << 24) | (value << 16) | (value << 8) | value;
        }
        SelectObject(dc, previous_font);
        DeleteObject(font);
        SelectObject(dc, previous);
        let mask = CreateBitmap(size, size, 1, 1, null());
        let icon_info = ICONINFO {
            fIcon: 1,
            xHotspot: 0,
            yHotspot: 0,
            hbmMask: mask,
            hbmColor: color,
        };
        let icon = CreateIconIndirect(&icon_info);
        DeleteObject(mask);
        DeleteObject(color);
        DeleteDC(dc);
        icon as isize
    }
}

fn windows_win32_taskbar_list() -> Option<::windows::Win32::UI::Shell::ITaskbarList3> {
    windows_win32_ensure_com_apartment();
    unsafe {
        let list: ::windows::Win32::UI::Shell::ITaskbarList3 = ::windows::Win32::System::Com::CoCreateInstance(
            &::windows::Win32::UI::Shell::TaskbarList,
            None,
            ::windows::Win32::System::Com::CLSCTX_INPROC_SERVER,
        )
        .ok()?;
        list.HrInit().ok()?;
        Some(list)
    }
}

fn windows_win32_render_taskbar(hwnd: HWND, state: &mut WindowsWin32TaskbarState) {
    use ::windows::Win32::UI::Shell::{
        TBPF_ERROR, TBPF_INDETERMINATE, TBPF_NOPROGRESS, TBPF_NORMAL, TBPF_PAUSED, THB_FLAGS,
        THB_ICON, THB_TOOLTIP, THBF_DISABLED, THBF_ENABLED, THBF_HIDDEN, THUMBBUTTON,
    };
    if state.list.is_none() {
        state.list = windows_win32_taskbar_list();
    }
    let Some(list) = state.list.clone() else {
        return;
    };
    let window = windows_win32_com_hwnd(hwnd);
    for icon in state.icons.drain(..) {
        unsafe {
            windows_sys::Win32::UI::WindowsAndMessaging::DestroyIcon(icon as _);
        }
    }
    let spec = state.spec.clone().unwrap_or_default();
    if !spec.buttons.is_empty() || state.buttons_added {
        let light = windows_system_theme_mode() == crate::ZsuiThemeMode::Light;
        let size = unsafe {
            windows_sys::Win32::UI::HiDpi::GetSystemMetricsForDpi(
                windows_sys::Win32::UI::WindowsAndMessaging::SM_CXSMICON as _,
                GetDpiForWindow(hwnd).max(96),
            )
        };
        let mut buttons: Vec<THUMBBUTTON> = Vec::with_capacity(ZSUI_WIN32_TASKBAR_BUTTON_COUNT);
        for index in 0..ZSUI_WIN32_TASKBAR_BUTTON_COUNT {
            let mut button = THUMBBUTTON {
                iId: ZSUI_WIN32_TASKBAR_BUTTON_BASE + index as u32,
                ..THUMBBUTTON::default()
            };
            match spec.buttons.get(index) {
                Some(declared) => {
                    let icon = windows_win32_glyph_icon(declared.glyph, size, light);
                    state.icons.push(icon);
                    button.dwMask = THB_ICON | THB_TOOLTIP | THB_FLAGS;
                    button.hIcon =
                        ::windows::Win32::UI::WindowsAndMessaging::HICON(icon as *mut core::ffi::c_void);
                    for (slot, unit) in button
                        .szTip
                        .iter_mut()
                        .zip(declared.tooltip.encode_utf16().take(259))
                    {
                        *slot = unit;
                    }
                    button.dwFlags = if declared.enabled {
                        THBF_ENABLED
                    } else {
                        THBF_DISABLED
                    };
                }
                None => {
                    button.dwMask = THB_FLAGS;
                    button.dwFlags = THBF_HIDDEN;
                }
            }
            buttons.push(button);
        }
        let result = unsafe {
            if state.buttons_added {
                list.ThumbBarUpdateButtons(window, &buttons)
            } else {
                list.ThumbBarAddButtons(window, &buttons)
            }
        };
        if result.is_ok() {
            state.buttons_added = true;
        }
    }
    unsafe {
        let _ = match spec.progress {
            crate::ZsTaskbarProgress::None => list.SetProgressState(window, TBPF_NOPROGRESS),
            crate::ZsTaskbarProgress::Indeterminate => {
                list.SetProgressState(window, TBPF_INDETERMINATE)
            }
            crate::ZsTaskbarProgress::Normal(value)
            | crate::ZsTaskbarProgress::Paused(value)
            | crate::ZsTaskbarProgress::Error(value) => {
                let flag = match spec.progress {
                    crate::ZsTaskbarProgress::Paused(_) => TBPF_PAUSED,
                    crate::ZsTaskbarProgress::Error(_) => TBPF_ERROR,
                    _ => TBPF_NORMAL,
                };
                let _ = list.SetProgressState(window, flag);
                let completed = (value.clamp(0.0, 1.0) * 1000.0).round() as u64;
                list.SetProgressValue(window, completed, 1000)
            }
        };
    }
}

/// Applies a declared taskbar spec; `None` removes buttons and progress.
fn apply_windows_win32_taskbar(hwnd: HWND, spec: Option<&crate::ZsTaskbarSpec>) {
    ZSUI_WIN32_TASKBARS.with(|all| {
        let mut all = all.borrow_mut();
        let index = match all.iter().position(|state| state.owner == hwnd as isize) {
            Some(index) => index,
            None => {
                if spec.is_none() {
                    return;
                }
                all.push(WindowsWin32TaskbarState {
                    owner: hwnd as isize,
                    list: None,
                    buttons_added: false,
                    spec: None,
                    icons: Vec::new(),
                });
                all.len() - 1
            }
        };
        let state = &mut all[index];
        state.spec = spec.cloned();
        windows_win32_render_taskbar(hwnd, state);
    });
}

/// Re-adds thumbnail buttons once Explorer has (re)created the button.
pub(crate) fn windows_win32_taskbar_button_created(hwnd: HWND) {
    ZSUI_WIN32_TASKBARS.with(|all| {
        let mut all = all.borrow_mut();
        if let Some(state) = all.iter_mut().find(|state| state.owner == hwnd as isize) {
            state.buttons_added = false;
            state.list = None;
            windows_win32_render_taskbar(hwnd, state);
        }
    });
}

/// The command of a clicked thumbnail button.
pub(crate) fn windows_win32_taskbar_command(hwnd: HWND, wparam: WPARAM) -> Option<Command> {
    if (wparam >> 16) as u32 & 0xffff != ::windows::Win32::UI::Shell::THBN_CLICKED {
        return None;
    }
    let id = (wparam & 0xffff) as u32;
    let index = id.checked_sub(ZSUI_WIN32_TASKBAR_BUTTON_BASE)? as usize;
    ZSUI_WIN32_TASKBARS.with(|all| {
        all.borrow()
            .iter()
            .find(|state| state.owner == hwnd as isize)
            .and_then(|state| state.spec.as_ref())
            .and_then(|spec| spec.buttons.get(index))
            .filter(|button| button.enabled)
            .map(|button| button.command.clone())
    })
}

fn windows_win32_media_controls(
    hwnd: HWND,
) -> Option<(::windows::Media::SystemMediaTransportControls, i64)> {
    use ::windows::Media::{
        SystemMediaTransportControls, SystemMediaTransportControlsButton,
        SystemMediaTransportControlsButtonPressedEventArgs,
    };
    use ::windows::Win32::System::WinRT::ISystemMediaTransportControlsInterop;
    windows_win32_ensure_com_apartment();
    let interop = ::windows::core::factory::<
        SystemMediaTransportControls,
        ISystemMediaTransportControlsInterop,
    >()
    .ok()?;
    let controls: SystemMediaTransportControls =
        unsafe { interop.GetForWindow(windows_win32_com_hwnd(hwnd)) }.ok()?;
    let owner = hwnd as isize;
    let handler = ::windows::Foundation::TypedEventHandler::<
        SystemMediaTransportControls,
        SystemMediaTransportControlsButtonPressedEventArgs,
    >::new(move |_, args| {
        if let Some(args) = args.as_ref() {
            let code = match args.Button()? {
                SystemMediaTransportControlsButton::Play => 1,
                SystemMediaTransportControlsButton::Pause => 2,
                SystemMediaTransportControlsButton::Stop => 3,
                SystemMediaTransportControlsButton::Next => 4,
                SystemMediaTransportControlsButton::Previous => 5,
                _ => 0,
            };
            if code != 0 {
                unsafe {
                    PostMessageW(owner as HWND, ZSUI_WIN32_MEDIA_COMMAND_MESSAGE, code, 0);
                }
            }
        }
        Ok(())
    });
    let token = controls.ButtonPressed(&handler).ok()?;
    Some((controls, token))
}

/// Maps a posted media button to a host event.
pub(crate) fn windows_win32_media_command(code: WPARAM) -> Option<crate::ZsMediaCommand> {
    Some(match code {
        1 => crate::ZsMediaCommand::Play,
        2 => crate::ZsMediaCommand::Pause,
        3 => crate::ZsMediaCommand::Stop,
        4 => crate::ZsMediaCommand::Next,
        5 => crate::ZsMediaCommand::Previous,
        _ => return None,
    })
}

fn windows_win32_time_span(seconds: f64) -> ::windows::Foundation::TimeSpan {
    ::windows::Foundation::TimeSpan {
        Duration: (seconds.max(0.0) * 10_000_000.0) as i64,
    }
}

fn windows_win32_update_media_display(
    controls: &::windows::Media::SystemMediaTransportControls,
    session: &crate::ZsMediaSession,
) -> ::windows::core::Result<()> {
    use ::windows::Media::MediaPlaybackType;
    let updater = controls.DisplayUpdater()?;
    updater.SetType(MediaPlaybackType::Music)?;
    let music = updater.MusicProperties()?;
    music.SetTitle(&::windows::core::HSTRING::from(session.title.as_str()))?;
    music.SetArtist(&::windows::core::HSTRING::from(session.artist.as_str()))?;
    music.SetAlbumTitle(&::windows::core::HSTRING::from(session.album.as_str()))?;
    if let Some(artwork) = session.artwork.as_ref().filter(|bytes| !bytes.is_empty()) {
        // A synchronous in-memory stream; WinRT's async writers would block
        // the UI thread.
        let stream = unsafe { ::windows::Win32::UI::Shell::SHCreateMemStream(Some(artwork)) };
        if let Some(stream) = stream {
            let random: ::windows::Storage::Streams::IRandomAccessStream = unsafe {
                ::windows::Win32::System::WinRT::CreateRandomAccessStreamOverStream(
                    &stream,
                    ::windows::Win32::System::WinRT::BSOS_DEFAULT,
                )?
            };
            let reference =
                ::windows::Storage::Streams::RandomAccessStreamReference::CreateFromStream(&random)?;
            updater.SetThumbnail(&reference)?;
        }
    } else {
        updater.SetThumbnail(None::<&::windows::Storage::Streams::RandomAccessStreamReference>)?;
    }
    updater.Update()
}

fn windows_win32_update_media_timeline(
    controls: &::windows::Media::SystemMediaTransportControls,
    position: f64,
    duration: f64,
) -> ::windows::core::Result<()> {
    let timeline = ::windows::Media::SystemMediaTransportControlsTimelineProperties::new()?;
    timeline.SetStartTime(windows_win32_time_span(0.0))?;
    timeline.SetMinSeekTime(windows_win32_time_span(0.0))?;
    timeline.SetEndTime(windows_win32_time_span(duration))?;
    timeline.SetMaxSeekTime(windows_win32_time_span(duration))?;
    timeline.SetPosition(windows_win32_time_span(position))?;
    controls.UpdateTimelineProperties(&timeline)
}

/// Applies a declared media session; `None` ends it.
fn apply_windows_win32_media_session(hwnd: HWND, session: Option<&crate::ZsMediaSession>) {
    use ::windows::Media::MediaPlaybackStatus;
    ZSUI_WIN32_MEDIA_SESSIONS.with(|all| {
        let mut all = all.borrow_mut();
        let position = all.iter().position(|state| state.owner == hwnd as isize);
        let Some(session) = session else {
            if let Some(index) = position {
                let state = all.remove(index);
                let _ = state.controls.SetIsEnabled(false);
                let _ = state.controls.RemoveButtonPressed(state.token);
            }
            return;
        };
        let index = match position {
            Some(index) => index,
            None => {
                let Some((controls, token)) = windows_win32_media_controls(hwnd) else {
                    return;
                };
                all.push(WindowsWin32MediaSessionState {
                    owner: hwnd as isize,
                    controls,
                    token,
                    applied: None,
                });
                all.len() - 1
            }
        };
        let state = &mut all[index];
        let previous = state.applied.clone();
        let controls = &state.controls;
        let first = previous.is_none();
        if first
            || previous.as_ref().is_some_and(|previous| {
                previous.can_next != session.can_next
                    || previous.can_previous != session.can_previous
            })
        {
            let _ = controls.SetIsEnabled(true);
            let _ = controls.SetIsPlayEnabled(true);
            let _ = controls.SetIsPauseEnabled(true);
            let _ = controls.SetIsStopEnabled(false);
            let _ = controls.SetIsNextEnabled(session.can_next);
            let _ = controls.SetIsPreviousEnabled(session.can_previous);
        }
        if first || previous.as_ref().is_some_and(|previous| previous.playing != session.playing) {
            let _ = controls.SetPlaybackStatus(if session.playing {
                MediaPlaybackStatus::Playing
            } else {
                MediaPlaybackStatus::Paused
            });
        }
        let display_changed = previous.as_ref().is_none_or(|previous| {
            previous.title != session.title
                || previous.artist != session.artist
                || previous.album != session.album
                || match (&previous.artwork, &session.artwork) {
                    (Some(a), Some(b)) => !std::sync::Arc::ptr_eq(a, b) && a != b,
                    (None, None) => false,
                    _ => true,
                }
        });
        if display_changed {
            let _ = windows_win32_update_media_display(controls, session);
        }
        if let (Some(position), Some(duration)) = (session.position, session.duration) {
            // The system extrapolates while playing; resend on jumps only.
            let stale = previous.as_ref().is_none_or(|previous| {
                display_changed
                    || previous.playing != session.playing
                    || previous.duration != session.duration
                    || previous
                        .position
                        .is_none_or(|last| (last - position).abs() > 2.5)
            });
            if stale {
                let _ = windows_win32_update_media_timeline(controls, position, duration);
                state.applied = Some(session.clone());
                return;
            }
            // Keep the last sent position so drift is measured against it.
            let mut applied = session.clone();
            applied.position = previous.as_ref().and_then(|previous| previous.position);
            state.applied = Some(applied);
            return;
        }
        state.applied = Some(session.clone());
    });
}

/// Ends a closing window's taskbar and media integrations.
fn clear_windows_win32_shell_integration(hwnd: HWND) {
    let _ = ZSUI_WIN32_TASKBARS.try_with(|all| {
        all.borrow_mut().retain(|state| {
            if state.owner != hwnd as isize {
                return true;
            }
            for icon in &state.icons {
                unsafe {
                    windows_sys::Win32::UI::WindowsAndMessaging::DestroyIcon(*icon as _);
                }
            }
            false
        });
    });
    let _ = ZSUI_WIN32_MEDIA_SESSIONS.try_with(|all| {
        all.borrow_mut().retain(|state| {
            if state.owner != hwnd as isize {
                return true;
            }
            let _ = state.controls.SetIsEnabled(false);
            let _ = state.controls.RemoveButtonPressed(state.token);
            false
        });
    });
}

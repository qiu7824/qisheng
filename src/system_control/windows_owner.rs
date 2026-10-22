use super::{
    HotkeyBinding, SystemConfigurationAck, SystemConfigurationOutcome, SystemConfigurationState,
    SystemInputSender, SystemPlaybackState, SystemPowerEvent, SystemProjection, SystemSettings,
    smtc::SmtcOwner,
    supervisor::ManagedWorker,
    types::{ConfigurationMailbox, ConfigurationRequest},
};
use anyhow::{Result, ensure};
use std::{
    cell::Cell,
    collections::HashMap,
    ffi::c_void,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
    },
    thread,
    time::{Duration, Instant},
};
use windows::Win32::System::WinRT::{RO_INIT_MULTITHREADED, RoInitialize, RoUninitialize};
use windows_sys::Win32::{
    Foundation::{
        CloseHandle, HANDLE, HINSTANCE, HWND, INVALID_HANDLE_VALUE, LPARAM, LRESULT, WPARAM,
    },
    System::{
        LibraryLoader::GetModuleHandleW,
        Power::{
            HPOWERNOTIFY, PowerClearRequest, PowerCreateRequest, PowerRequestSystemRequired,
            PowerSetRequest, RegisterSuspendResumeNotification,
            UnregisterSuspendResumeNotification,
        },
        Threading::{
            GetCurrentThreadId, POWER_REQUEST_CONTEXT_SIMPLE_STRING, REASON_CONTEXT,
            REASON_CONTEXT_0,
        },
    },
    UI::{
        Input::KeyboardAndMouse::{
            MOD_ALT, MOD_CONTROL, MOD_NOREPEAT, MOD_SHIFT, MOD_WIN, RegisterHotKey,
            UnregisterHotKey,
        },
        WindowsAndMessaging::*,
    },
};

#[derive(Clone, Debug, Default)]
pub struct WindowsOwnerState {
    pub owner_epoch: u64,
    pub ready: bool,
    pub thread_exited: bool,
    pub registered_hotkeys: usize,
    pub media_session_active: bool,
    pub power_request_active: bool,
    pub last_power_event: Option<SystemPowerEvent>,
    pub last_error: Option<String>,
    pub configuration: SystemConfigurationState,
}
struct Shared {
    closing: AtomicBool,
    thread_id: AtomicU32,
    configuration: Mutex<ConfigurationMailbox>,
    projection: Mutex<Option<SystemProjection>>,
    status: Arc<Mutex<WindowsOwnerState>>,
}
pub struct WindowsSystemOwner {
    shared: Arc<Shared>,
    worker: Option<ManagedWorker>,
    sender: SystemInputSender,
}
impl WindowsSystemOwner {
    /// Starts only an explicitly enabled native system-control service. The
    /// returned handle retains the actual worker even if initialization fails.
    pub fn start(
        settings: SystemSettings,
        projection: SystemProjection,
        sender: SystemInputSender,
    ) -> Result<Self> {
        settings.validate()?;
        projection.validate()?;
        ensure!(settings.enabled, "System controls are disabled");
        let configuration = ConfigurationMailbox::new(settings)?;
        sender.pause();
        let shared = Arc::new(Shared {
            closing: AtomicBool::new(false),
            thread_id: AtomicU32::new(0),
            configuration: Mutex::new(configuration),
            projection: Mutex::new(Some(projection)),
            status: Arc::new(Mutex::new(WindowsOwnerState {
                owner_epoch: sender.owner_epoch(),
                ..Default::default()
            })),
        });
        let owner = shared.clone();
        let input = sender.clone();
        let worker = ManagedWorker::spawn("lx-windows-system-owner", move || run(owner, input))?;
        Ok(Self {
            shared,
            worker: Some(worker),
            sender,
        })
    }
    pub fn state(&self) -> WindowsOwnerState {
        let mut state = self
            .shared
            .status
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        state.configuration = self
            .shared
            .configuration
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .state();
        state
    }
    pub fn publish(&self, projection: SystemProjection) -> Result<()> {
        projection.validate()?;
        ensure!(
            !self.shared.closing.load(Ordering::Acquire),
            "System owner is closing"
        );
        *self
            .shared
            .projection
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(projection);
        Ok(())
    }
    /// Admits one request. A returned revision is not a native success receipt.
    pub fn configure(&self, settings: SystemSettings) -> Result<u64> {
        settings.validate()?;
        ensure!(
            !self.shared.closing.load(Ordering::Acquire),
            "System owner is closing"
        );
        let mut configuration = self
            .shared
            .configuration
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let revision = configuration.admit(settings)?;
        self.sender.pause();
        drop(configuration);
        Ok(revision)
    }
    /// Commits an observed native acknowledgement after the backend's durable
    /// settings transaction succeeds. Disabled owners are actually joined here.
    pub fn commit_configuration(&mut self, revision: u64) -> Result<SystemConfigurationAck> {
        let state = self
            .shared
            .configuration
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .state();
        let ack = state
            .terminal
            .as_ref()
            .filter(|ack| ack.revision == revision)
            .ok_or_else(|| anyhow::anyhow!("Native configuration acknowledgement is not ready"))?;
        if !ack.applied.as_ref().is_some_and(|s| s.enabled) {
            ensure!(
                self.join_retired()?,
                "Native owner retirement has not joined"
            );
        }
        let ack = self
            .shared
            .configuration
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .acknowledge(revision)?;
        if ack.applied.as_ref().is_some_and(|s| s.enabled) {
            self.sender.resume();
        }
        Ok(ack)
    }
    /// A failed durable commit keeps admission paused while the same HWND
    /// restores the previously acknowledged native configuration.
    pub fn rollback_configuration(&self, revision: u64) -> Result<u64> {
        ensure!(
            !self.shared.closing.load(Ordering::Acquire),
            "System owner is closing"
        );
        let next = self
            .shared
            .configuration
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .rollback_uncommitted(revision)?;
        self.sender.pause();
        Ok(next)
    }
    pub fn begin_shutdown(&self) {
        self.sender.close();
        self.shared.closing.store(true, Ordering::Release);
        let id = self.shared.thread_id.load(Ordering::Acquire);
        if id != 0 {
            unsafe {
                PostThreadMessageW(id, WM_NULL, 0, 0);
            }
        }
    }
    /// Checks and joins only an already-exited worker. The backend uses this
    /// before durably committing a disabled configuration.
    pub fn join_retired(&mut self) -> Result<bool> {
        if self.worker.as_ref().is_some_and(|w| !w.is_finished()) {
            return Ok(false);
        }
        if let Some(mut worker) = self.worker.take() {
            worker.join_finished()?;
        }
        // A closed owner can still hold the last queued artwork lease in its
        // shared mailbox. Release it after the actual native thread joins.
        self.shared
            .projection
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        Ok(true)
    }
    pub fn shutdown(&mut self, timeout: Duration) -> Result<bool> {
        self.begin_shutdown();
        let deadline = Instant::now() + timeout;
        while self.worker.as_ref().is_some_and(|w| !w.is_finished()) {
            if Instant::now() >= deadline {
                return Ok(false);
            }
            thread::sleep(Duration::from_millis(2));
        }
        self.join_retired()
    }
}
impl Drop for WindowsSystemOwner {
    fn drop(&mut self) {
        self.begin_shutdown();
    }
}

struct WindowContext {
    sender: SystemInputSender,
    generation: AtomicU64,
    hotkeys: Mutex<HashMap<i32, super::SystemAction>>,
    status: Arc<Mutex<WindowsOwnerState>>,
}
thread_local! {static OWNED_WINDOW:Cell<(HWND,*const WindowContext)>=const {Cell::new((std::ptr::null_mut(),std::ptr::null()))};}
unsafe extern "system" fn window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if message == WM_NCCREATE {
        return OWNED_WINDOW.with(|slot| {
            let (old, pointer) = slot.get();
            if old.is_null() && !pointer.is_null() {
                slot.set((hwnd, pointer));
                1
            } else {
                0
            }
        });
    }
    let pointer = OWNED_WINDOW.with(|slot| {
        let (owned, pointer) = slot.get();
        if owned == hwnd {
            pointer
        } else {
            std::ptr::null()
        }
    });
    if !pointer.is_null() {
        let context = unsafe { &*pointer };
        if message == WM_HOTKEY {
            if let Some(action) = context
                .hotkeys
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(&(wparam as i32))
                .copied()
            {
                let generation = action
                    .requires_generation()
                    .then(|| context.generation.load(Ordering::Acquire));
                let _ = context.sender.try_submit(action, generation);
            }
            return 0;
        }
        if message == WM_POWERBROADCAST {
            let event = match wparam as u32 {
                PBT_APMSUSPEND => Some(SystemPowerEvent::Suspend),
                PBT_APMRESUMEAUTOMATIC => Some(SystemPowerEvent::ResumeAutomatic),
                PBT_APMRESUMESUSPEND => Some(SystemPowerEvent::ResumeUser),
                _ => None,
            };
            if let Some(event) = event {
                context
                    .status
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .last_power_event = Some(event);
                let _ = context
                    .sender
                    .try_submit(super::SystemAction::PowerEvent { event }, None);
            }
            return 1;
        }
        if message == WM_NCDESTROY {
            OWNED_WINDOW.with(|slot| slot.set((std::ptr::null_mut(), std::ptr::null())));
        }
    }
    unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
}

struct PowerLease {
    handle: HANDLE,
    active: bool,
    _reason: Vec<u16>,
}
impl PowerLease {
    fn create() -> Result<Self> {
        let mut reason = "LX Native audio playback"
            .encode_utf16()
            .chain(Some(0))
            .collect::<Vec<_>>();
        let context = REASON_CONTEXT {
            Version: 0,
            Flags: POWER_REQUEST_CONTEXT_SIMPLE_STRING,
            Reason: REASON_CONTEXT_0 {
                SimpleReasonString: reason.as_mut_ptr(),
            },
        };
        let handle = unsafe { PowerCreateRequest(&context) };
        ensure!(
            !handle.is_null() && handle != INVALID_HANDLE_VALUE,
            "Cannot create native playback power request: {}",
            std::io::Error::last_os_error()
        );
        Ok(Self {
            handle,
            active: false,
            _reason: reason,
        })
    }
    fn set(&mut self, wanted: bool) -> Result<()> {
        if wanted == self.active {
            return Ok(());
        }
        let success = unsafe {
            if wanted {
                PowerSetRequest(self.handle, PowerRequestSystemRequired)
            } else {
                PowerClearRequest(self.handle, PowerRequestSystemRequired)
            }
        };
        ensure!(
            success != 0,
            "Cannot change native playback power request: {}",
            std::io::Error::last_os_error()
        );
        self.active = wanted;
        Ok(())
    }
    fn close(&mut self) -> Result<()> {
        self.set(false)?;
        if !self.handle.is_null() {
            ensure!(
                unsafe { CloseHandle(self.handle) } != 0,
                "Cannot close native playback power request"
            );
            self.handle = std::ptr::null_mut();
        }
        Ok(())
    }
}
struct NativeOwner {
    instance: HINSTANCE,
    class_name: Vec<u16>,
    class_registered: bool,
    hwnd: HWND,
    context: Box<WindowContext>,
    hotkeys: Vec<NativeHotkey>,
    next_hotkey: i32,
    power: Option<PowerLease>,
    power_release: Option<Instant>,
    notification: HPOWERNOTIFY,
    smtc: Option<SmtcOwner>,
    ro_initialized: bool,
}
struct NativeHotkey {
    id: i32,
    binding: HotkeyBinding,
}
impl NativeOwner {
    fn empty(sender: SystemInputSender, status: Arc<Mutex<WindowsOwnerState>>) -> Self {
        let class_name = format!("LXNative.SystemControl.{}", sender.owner_epoch())
            .encode_utf16()
            .chain(Some(0))
            .collect();
        Self {
            instance: std::ptr::null_mut(),
            class_name,
            class_registered: false,
            hwnd: std::ptr::null_mut(),
            context: Box::new(WindowContext {
                sender,
                generation: AtomicU64::new(0),
                hotkeys: Mutex::new(HashMap::new()),
                status,
            }),
            hotkeys: Vec::with_capacity(super::MAX_HOTKEYS),
            next_hotkey: 1,
            power: None,
            power_release: None,
            notification: 0,
            smtc: None,
            ro_initialized: false,
        }
    }
    fn initialize(&mut self) -> Result<()> {
        self.instance = unsafe { GetModuleHandleW(std::ptr::null()) };
        ensure!(
            !self.instance.is_null(),
            "Cannot inspect native application instance"
        );
        let class = WNDCLASSW {
            style: 0,
            lpfnWndProc: Some(window_proc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: self.instance,
            hIcon: std::ptr::null_mut(),
            hCursor: std::ptr::null_mut(),
            hbrBackground: std::ptr::null_mut(),
            lpszMenuName: std::ptr::null(),
            lpszClassName: self.class_name.as_ptr(),
        };
        ensure!(
            unsafe { RegisterClassW(&class) } != 0,
            "Cannot register system-control window class: {}",
            std::io::Error::last_os_error()
        );
        self.class_registered = true;
        OWNED_WINDOW
            .with(|slot| slot.set((std::ptr::null_mut(), &*self.context as *const WindowContext)));
        self.hwnd = unsafe {
            CreateWindowExW(
                0,
                self.class_name.as_ptr(),
                self.class_name.as_ptr(),
                WS_OVERLAPPED,
                0,
                0,
                0,
                0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                self.instance,
                std::ptr::null::<c_void>(),
            )
        };
        ensure!(
            !self.hwnd.is_null(),
            "Cannot create hidden system-control window: {}",
            std::io::Error::last_os_error()
        );
        self.notification =
            unsafe { RegisterSuspendResumeNotification(self.hwnd, DEVICE_NOTIFY_WINDOW_HANDLE) };
        ensure!(
            self.notification != 0,
            "Cannot register native suspend/resume notifications: {}",
            std::io::Error::last_os_error()
        );
        Ok(())
    }
    fn unregister_hotkeys(&mut self) -> Result<()> {
        let mut failure = false;
        self.hotkeys.retain(|hotkey| {
            let removed = unsafe { UnregisterHotKey(self.hwnd, hotkey.id) } != 0;
            if removed {
                self.context
                    .hotkeys
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&hotkey.id);
            } else {
                failure = true;
            }
            !removed
        });
        ensure!(!failure, "Native hotkey retirement is incomplete");
        Ok(())
    }
    fn configure(&mut self, settings: &SystemSettings) -> Result<()> {
        // Prepare new chords while old registrations remain owned. Admission is
        // paused until the backend commits the resulting native acknowledgement.
        if settings.media_session_enabled {
            if !self.ro_initialized {
                unsafe {
                    RoInitialize(RO_INIT_MULTITHREADED)?;
                }
                self.ro_initialized = true;
            }
            if self.smtc.is_none() {
                self.smtc = Some(SmtcOwner::create(self.hwnd)?);
            }
        }
        let desired = if settings.global_hotkeys_enabled {
            settings.hotkeys.as_slice()
        } else {
            &[]
        };
        for binding in desired {
            if !self
                .hotkeys
                .iter()
                .any(|old| old.binding.chord == binding.chord)
            {
                let chord = &binding.chord;
                ensure!(
                    self.hotkeys.len() < super::MAX_HOTKEYS * 2,
                    "Native staged hotkey ownership limit exceeded"
                );
                ensure!(self.next_hotkey <= 0xbfff, "Native hotkey IDs exhausted");
                let id = self.next_hotkey;
                self.next_hotkey += 1;
                let modifiers = MOD_NOREPEAT
                    | if chord.control { MOD_CONTROL } else { 0 }
                    | if chord.alt { MOD_ALT } else { 0 }
                    | if chord.shift { MOD_SHIFT } else { 0 }
                    | if chord.windows { MOD_WIN } else { 0 };
                ensure!(
                    unsafe {
                        RegisterHotKey(self.hwnd, id, modifiers, u32::from(chord.virtual_key))
                    } != 0,
                    "Cannot register global hotkey: {}",
                    std::io::Error::last_os_error()
                );
                self.hotkeys.push(NativeHotkey {
                    id,
                    binding: binding.clone(),
                });
            }
        }
        if !settings.media_session_enabled
            && let Some(smtc) = &mut self.smtc
        {
            smtc.close()?;
            self.smtc = None;
        }
        if !settings.power_blocker_enabled {
            if let Some(power) = &mut self.power {
                power.set(false)?;
            }
            self.power_release = None;
        }
        let mut retirement_failed = false;
        self.hotkeys.retain(|old| {
            if desired.iter().any(|wanted| wanted == &old.binding) {
                return true;
            }
            if unsafe { UnregisterHotKey(self.hwnd, old.id) } == 0 {
                retirement_failed = true;
                return true;
            }
            self.context
                .hotkeys
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&old.id);
            false
        });
        ensure!(
            !retirement_failed,
            "Native old hotkey retirement is incomplete"
        );
        // Rebind changed actions under a fresh ID. Delayed WM_HOTKEY messages
        // carrying the retired ID can never dispatch a replacement action.
        for binding in desired {
            if self.hotkeys.iter().any(|old| old.binding == *binding) {
                continue;
            }
            ensure!(self.next_hotkey <= 0xbfff, "Native hotkey IDs exhausted");
            let id = self.next_hotkey;
            self.next_hotkey += 1;
            let chord = &binding.chord;
            let modifiers = MOD_NOREPEAT
                | if chord.control { MOD_CONTROL } else { 0 }
                | if chord.alt { MOD_ALT } else { 0 }
                | if chord.shift { MOD_SHIFT } else { 0 }
                | if chord.windows { MOD_WIN } else { 0 };
            ensure!(
                unsafe { RegisterHotKey(self.hwnd, id, modifiers, u32::from(chord.virtual_key)) }
                    != 0,
                "Cannot rebind global hotkey: {}",
                std::io::Error::last_os_error()
            );
            self.hotkeys.push(NativeHotkey {
                id,
                binding: binding.clone(),
            });
        }
        let mut actions = self
            .context
            .hotkeys
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        actions.clear();
        for hotkey in &mut self.hotkeys {
            let binding = desired
                .iter()
                .find(|wanted| wanted.chord == hotkey.binding.chord)
                .expect("retained desired native chord");
            hotkey.binding = binding.clone();
            actions.insert(hotkey.id, binding.action);
        }
        Ok(())
    }
    fn project(&mut self, value: &SystemProjection, settings: &SystemSettings) -> Result<()> {
        self.context
            .generation
            .store(value.generation, Ordering::Release);
        if let Some(smtc) = &mut self.smtc {
            smtc.update(value, &self.context.sender)?;
        }
        let playing = matches!(
            value.state,
            SystemPlaybackState::Playing | SystemPlaybackState::Buffering
        ) && value.media_id.is_some();
        if settings.power_blocker_enabled && playing {
            self.power_release = None;
            if self.power.is_none() {
                self.power = Some(PowerLease::create()?);
            }
            self.power.as_mut().unwrap().set(true)?;
        } else if self.power.as_ref().is_some_and(|p| p.active) && self.power_release.is_none() {
            self.power_release = Some(Instant::now() + Duration::from_secs(90));
        }
        Ok(())
    }
    fn poll_power(&mut self) -> Result<()> {
        if self.power_release.is_some_and(|t| Instant::now() >= t) {
            if let Some(power) = &mut self.power {
                power.set(false)?;
            }
            self.power_release = None;
        }
        Ok(())
    }
    fn close(&mut self) -> Result<()> {
        self.context.sender.close();
        if let Some(smtc) = &mut self.smtc {
            smtc.close()?;
            self.smtc = None;
        }
        self.unregister_hotkeys()?;
        if self.notification != 0 {
            ensure!(
                unsafe { UnregisterSuspendResumeNotification(self.notification) } != 0,
                "Native power notification retirement failed"
            );
            self.notification = 0;
        }
        if let Some(power) = &mut self.power {
            power.close()?;
            self.power = None;
        }
        if !self.hwnd.is_null() {
            ensure!(
                unsafe { DestroyWindow(self.hwnd) } != 0,
                "Hidden system-control window is still live"
            );
            self.hwnd = std::ptr::null_mut();
        }
        if self.class_registered {
            ensure!(
                unsafe { UnregisterClassW(self.class_name.as_ptr(), self.instance) } != 0,
                "System-control window class is still registered"
            );
            self.class_registered = false;
        }
        if self.ro_initialized {
            unsafe {
                RoUninitialize();
            }
            self.ro_initialized = false;
        }
        Ok(())
    }
}
struct PendingConfiguration {
    request: ConfigurationRequest,
    failure: Option<String>,
    next_attempt: Instant,
}
fn report_error(shared: &Shared, error: &str) {
    shared
        .status
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .last_error = Some(error.chars().take(1024).collect());
}
fn finish_configuration(
    shared: &Shared,
    request: ConfigurationRequest,
    outcome: SystemConfigurationOutcome,
    error: Option<String>,
) {
    if let Err(error) = shared
        .configuration
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .finish(request, outcome, error)
    {
        report_error(shared, &format!("{error:#}"));
        shared.closing.store(true, Ordering::Release);
    }
}
fn run(shared: Arc<Shared>, sender: SystemInputSender) {
    shared
        .thread_id
        .store(unsafe { GetCurrentThreadId() }, Ordering::Release);
    let mut resources = NativeOwner::empty(sender, shared.status.clone());
    let request = shared
        .configuration
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take_request()
        .expect("initial admitted configuration");
    let mut projection = shared
        .projection
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take()
        .expect("initial validated projection");
    let mut settings: Option<SystemSettings> = None;
    let initialization_failure = resources.initialize().err().map(|e| format!("{e:#}"));
    if let Some(error) = &initialization_failure {
        report_error(&shared, error);
        let _ = shared
            .configuration
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .failure(request.revision, error);
    }
    let mut pending = Some(PendingConfiguration {
        request,
        failure: initialization_failure,
        next_attempt: Instant::now(),
    });
    let mut projection_pending = false;
    loop {
        let mut message = MSG::default();
        let mut count = 0;
        while count < 64
            && unsafe { PeekMessageW(&mut message, std::ptr::null_mut(), 0, 0, PM_REMOVE) } != 0
        {
            if message.message == WM_QUIT {
                shared.closing.store(true, Ordering::Release);
            } else {
                unsafe {
                    TranslateMessage(&message);
                    DispatchMessageW(&message);
                }
            }
            count += 1;
        }
        if shared.closing.load(Ordering::Acquire) {
            match resources.close() {
                Ok(()) => {
                    if let Some(transaction) = pending.take() {
                        finish_configuration(
                            &shared,
                            transaction.request,
                            SystemConfigurationOutcome::Cancelled,
                            Some("Native system owner shutdown cancelled configuration".into()),
                        );
                    }
                    break;
                }
                Err(error) => {
                    report_error(&shared, &format!("{error:#}"));
                    thread::sleep(Duration::from_millis(50));
                    continue;
                }
            }
        }
        if pending.is_none() {
            pending = shared
                .configuration
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take_request()
                .map(|request| PendingConfiguration {
                    request,
                    failure: None,
                    next_attempt: Instant::now(),
                });
        }
        if let Some(mut transaction) = pending.take() {
            if Instant::now() < transaction.next_attempt {
                pending = Some(transaction);
            } else if transaction.failure.is_none() {
                if !transaction.request.settings.enabled {
                    shared
                        .status
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .ready = false;
                    match resources.close() {
                        Ok(()) => {
                            finish_configuration(
                                &shared,
                                transaction.request,
                                SystemConfigurationOutcome::Applied,
                                None,
                            );
                            break;
                        }
                        Err(error) => {
                            // A disabled owner remains the actual holder until all
                            // tokens/handles are retired and its worker is joined.
                            report_error(&shared, &format!("{error:#}"));
                            transaction.next_attempt = Instant::now() + Duration::from_millis(50);
                            pending = Some(transaction);
                        }
                    }
                } else {
                    let outcome =
                        resources
                            .configure(&transaction.request.settings)
                            .and_then(|_| {
                                resources.project(&projection, &transaction.request.settings)
                            });
                    match outcome {
                        Ok(()) => {
                            settings = Some(transaction.request.settings.clone());
                            finish_configuration(
                                &shared,
                                transaction.request,
                                SystemConfigurationOutcome::Applied,
                                None,
                            );
                            shared
                                .status
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .ready = true;
                        }
                        Err(error) => {
                            let error = format!("{error:#}");
                            report_error(&shared, &error);
                            let _ = shared
                                .configuration
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .failure(transaction.request.revision, &error);
                            if transaction.request.rollback {
                                // A failed durable commit requires the original
                                // target to be restored; returning to the rejected
                                // candidate would leave unsaved native settings.
                                transaction.next_attempt =
                                    Instant::now() + Duration::from_millis(50);
                            } else {
                                transaction.failure = Some(error);
                                transaction.next_attempt = Instant::now();
                            }
                            pending = Some(transaction);
                        }
                    }
                }
            } else {
                let previous = transaction.request.previous.as_ref().filter(|s| s.enabled);
                let rollback = match previous {
                    Some(previous) => resources
                        .configure(previous)
                        .and_then(|_| resources.project(&projection, previous)),
                    None => resources.close(),
                };
                match rollback {
                    Ok(()) => {
                        settings = previous.cloned();
                        let exiting = settings.is_none();
                        finish_configuration(
                            &shared,
                            transaction.request,
                            SystemConfigurationOutcome::Rejected,
                            transaction.failure,
                        );
                        shared
                            .status
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .ready = !exiting;
                        if exiting {
                            break;
                        }
                    }
                    Err(error) => {
                        report_error(
                            &shared,
                            &format!("Native rollback remains incomplete: {error:#}"),
                        );
                        transaction.next_attempt = Instant::now() + Duration::from_millis(50);
                        pending = Some(transaction);
                    }
                }
            }
        }
        if let Some(next) = shared
            .projection
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        {
            projection = next;
            projection_pending = true;
        }
        if projection.artwork.as_ref().is_some_and(|a| !a.is_current()) {
            projection.artwork = None;
            projection_pending = true;
        }
        if pending.is_none()
            && projection_pending
            && let Some(settings) = &settings
        {
            match resources.project(&projection, settings) {
                Ok(()) => {
                    projection_pending = false;
                    shared
                        .status
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .last_error = None;
                }
                Err(error) => report_error(&shared, &format!("{error:#}")),
            }
        }
        if let Err(error) = resources.poll_power() {
            report_error(&shared, &format!("{error:#}"));
        }
        {
            let mut status = shared.status.lock().unwrap_or_else(|e| e.into_inner());
            status.registered_hotkeys = resources.hotkeys.len();
            status.media_session_active = resources.smtc.is_some();
            status.power_request_active = resources.power.as_ref().is_some_and(|p| p.active);
        }
        thread::sleep(Duration::from_millis(8));
    }
    shared
        .configuration
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .closed();
    shared.thread_id.store(0, Ordering::Release);
    shared.closing.store(true, Ordering::Release);
    let mut status = shared.status.lock().unwrap_or_else(|e| e.into_inner());
    status.ready = false;
    status.thread_exited = true;
    status.registered_hotkeys = 0;
    status.media_session_active = false;
    status.power_request_active = false;
}

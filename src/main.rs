#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
#![allow(unsafe_op_in_unsafe_fn)]

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::ffi::c_void;
use std::mem::size_of;
use std::path::Path;
use std::ptr::{addr_of, addr_of_mut};
use std::sync::atomic::{AtomicI64, AtomicIsize, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::SystemTime;

use windows::Win32::Devices::Display::{
    DestroyPhysicalMonitor, GetMonitorBrightness, GetMonitorCapabilities,
    GetNumberOfPhysicalMonitorsFromHMONITOR, GetPhysicalMonitorsFromHMONITOR, MC_CAPS_BRIGHTNESS,
    PHYSICAL_MONITOR, SetMonitorBrightness,
};
use windows::Win32::Foundation::{
    CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HANDLE, HINSTANCE, HWND, LPARAM, LRESULT,
    POINT, RECT, WPARAM,
};
use windows::Win32::Graphics::Dwm::{DWMWA_CLOAKED, DwmGetWindowAttribute};
use windows::Win32::Graphics::Gdi::{
    EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFOEXW,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::{
    CreateMutexW, OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
    QueryFullProcessImageNameW,
};
use windows::Win32::UI::Accessibility::{HWINEVENTHOOK, SetWinEventHook, UnhookWinEvent};
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext,
};
use windows::Win32::UI::Shell::{
    NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY, NOTIFYICONDATAW,
    Shell_NotifyIconW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CHILDID_SELF, CW_USEDEFAULT, CreatePopupMenu, CreateWindowExW, DefWindowProcW,
    DestroyIcon, DestroyMenu, DestroyWindow, DispatchMessageW, EVENT_OBJECT_CLOAKED,
    EVENT_OBJECT_CREATE, EVENT_OBJECT_DESTROY, EVENT_OBJECT_HIDE, EVENT_OBJECT_LOCATIONCHANGE,
    EVENT_OBJECT_SHOW, EVENT_OBJECT_UNCLOAKED, EVENT_SYSTEM_DESKTOPSWITCH, EVENT_SYSTEM_FOREGROUND,
    EVENT_SYSTEM_MINIMIZEEND, EVENT_SYSTEM_MINIMIZESTART, EnumWindows, GWL_EXSTYLE, GWLP_USERDATA,
    GetClassNameW, GetCursorPos, GetMessageW, GetShellWindow, GetWindowLongPtrW, GetWindowRect,
    GetWindowTextLengthW, GetWindowTextW, GetWindowThreadProcessId, HICON, HMENU, IDI_APPLICATION,
    IMAGE_ICON, IsIconic, IsWindowVisible, KillTimer, LR_DEFAULTSIZE, LoadIconW, LoadImageW,
    MB_ICONINFORMATION, MB_OK, MF_CHECKED, MF_SEPARATOR, MF_STRING, MF_UNCHECKED, MSG, MessageBoxW,
    OBJID_WINDOW, PBT_APMRESUMEAUTOMATIC, PostMessageW, PostQuitMessage, RegisterClassExW,
    RegisterWindowMessageW, SW_HIDE, SetForegroundWindow, SetTimer, SetWindowLongPtrW, ShowWindow,
    TPM_NONOTIFY, TPM_RETURNCMD, TPM_RIGHTBUTTON, TrackPopupMenu, TranslateMessage,
    WINDOW_EX_STYLE, WINEVENT_OUTOFCONTEXT, WINEVENT_SKIPOWNPROCESS, WM_APP, WM_CLOSE, WM_COMMAND,
    WM_CONTEXTMENU, WM_DESTROY, WM_DISPLAYCHANGE, WM_LBUTTONUP, WM_NCCREATE, WM_NULL,
    WM_POWERBROADCAST, WM_RBUTTONUP, WM_TIMER, WNDCLASSEXW, WS_OVERLAPPEDWINDOW,
};
use windows::core::{BOOL, Error, PCWSTR, PWSTR, w};

const APP_NAME: &str = "MMD";
const CLASS_NAME: PCWSTR = w!("MmdRustWindow");
const SINGLE_INSTANCE_MUTEX_NAME: PCWSTR = w!("Local\\MMD.SingleInstance.v1");
const APP_ICON_RESOURCE_ID: PCWSTR = PCWSTR(1u16 as _);
const WM_TRAYICON: u32 = WM_APP + 1;
const WM_WINDOW_EVENT: u32 = WM_APP + 2;
const TIMER_DEBOUNCE: usize = 1;
const TRAY_UID: u32 = 1;
const WS_EX_TOOLWINDOW: isize = 0x00000080;
const MAX_TARGET_FAILURES: u8 = 3;

const ID_BRIGHTNESS_0: usize = 1000;
const ID_BRIGHTNESS_10: usize = 1010;
const ID_BRIGHTNESS_25: usize = 1025;
const ID_BRIGHTNESS_50: usize = 1050;
const ID_BRIGHTNESS_75: usize = 1075;
const ID_BRIGHTNESS_100: usize = 1100;
const ID_TOGGLE_DIMMING: usize = 1900;
const ID_REFRESH: usize = 2000;
const ID_DIAGNOSTICS: usize = 2001;
const ID_EXIT: usize = 2002;

static EVENT_TARGET_HWND: AtomicIsize = AtomicIsize::new(0);
static EVENT_COUNT: AtomicI64 = AtomicI64::new(0);
static LAST_EVENT: OnceLock<Mutex<Option<LastEvent>>> = OnceLock::new();
static TASKBAR_CREATED_MESSAGE: OnceLock<u32> = OnceLock::new();

fn main() -> windows::core::Result<()> {
    let Some(_single_instance) = SingleInstanceGuard::acquire()? else {
        return Ok(());
    };

    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }

    let taskbar_created_message = unsafe { RegisterWindowMessageW(w!("TaskbarCreated")) };
    if taskbar_created_message != 0 {
        let _ = TASKBAR_CREATED_MESSAGE.set(taskbar_created_message);
    }

    let module = unsafe { GetModuleHandleW(None)? };
    let hinstance = HINSTANCE(module.0);
    let hwnd = create_message_window(hinstance)?;

    let settings = AppSettings::default();
    let state = Box::new(RefCell::new(AppState::new(hwnd, settings)?));
    let state_ptr = Box::into_raw(state);

    unsafe {
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, state_ptr as isize);
        let _ = ShowWindow(hwnd, SW_HIDE);
    }

    let result = run_message_loop();

    unsafe {
        let state_ptr = SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0) as *mut RefCell<AppState>;
        if !state_ptr.is_null() {
            drop(Box::from_raw(state_ptr));
        }
        let _ = DestroyWindow(hwnd);
    }

    result
}

struct SingleInstanceGuard {
    handle: HANDLE,
}

impl SingleInstanceGuard {
    fn acquire() -> windows::core::Result<Option<Self>> {
        unsafe {
            let handle = CreateMutexW(None, false, SINGLE_INSTANCE_MUTEX_NAME)?;
            if GetLastError() == ERROR_ALREADY_EXISTS {
                let _ = CloseHandle(handle);
                Ok(None)
            } else {
                Ok(Some(Self { handle }))
            }
        }
    }
}

impl Drop for SingleInstanceGuard {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.handle);
        }
    }
}

fn create_message_window(hinstance: HINSTANCE) -> windows::core::Result<HWND> {
    let class = WNDCLASSEXW {
        cbSize: size_of::<WNDCLASSEXW>() as u32,
        lpfnWndProc: Some(window_proc),
        hInstance: hinstance,
        lpszClassName: CLASS_NAME,
        ..Default::default()
    };

    unsafe {
        let atom = RegisterClassExW(&class);
        if atom == 0 {
            return Err(Error::from_thread());
        }

        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            CLASS_NAME,
            w!("MMD"),
            WS_OVERLAPPEDWINDOW,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            None,
            None,
            Some(hinstance),
            None,
        )?;

        Ok(hwnd)
    }
}

fn run_message_loop() -> windows::core::Result<()> {
    unsafe {
        let mut message = MSG::default();
        loop {
            let result = GetMessageW(&mut message, None, 0, 0).0;
            if result == -1 {
                return Err(Error::from_thread());
            }
            if result == 0 {
                break;
            }

            let _ = TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }

    Ok(())
}

unsafe extern "system" fn window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if message == WM_NCCREATE {
        return DefWindowProcW(hwnd, message, wparam, lparam);
    }

    let state_ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut RefCell<AppState>;
    if state_ptr.is_null() {
        if message == WM_DESTROY {
            PostQuitMessage(0);
            return LRESULT(0);
        }

        return DefWindowProcW(hwnd, message, wparam, lparam);
    }

    let state = &*state_ptr;
    if TASKBAR_CREATED_MESSAGE
        .get()
        .is_some_and(|taskbar_created| message == *taskbar_created)
    {
        if let Ok(mut state) = state.try_borrow_mut() {
            state.restore_tray_icon();
        }
        return LRESULT(0);
    }

    match message {
        WM_TRAYICON => {
            let tray_event = low_word(lparam.0);
            match tray_event {
                WM_LBUTTONUP => {
                    if let Ok(mut state) = state.try_borrow_mut() {
                        state.toggle_tray_brightness();
                    }
                }
                WM_RBUTTONUP | WM_CONTEXTMENU => {
                    let manual_dimming_enabled = state
                        .try_borrow()
                        .map(|state| state.monitor_manager.manual_dimming_enabled())
                        .unwrap_or(false);
                    if let Some(command_id) = show_context_menu(hwnd, manual_dimming_enabled) {
                        dispatch_command(hwnd, state, command_id);
                    }
                }
                _ => {}
            }
            LRESULT(0)
        }
        WM_COMMAND => {
            dispatch_command(hwnd, state, wparam.0 & 0xffff);
            LRESULT(0)
        }
        WM_WINDOW_EVENT => {
            if let Ok(mut state) = state.try_borrow_mut() {
                state.schedule_update();
            }
            LRESULT(0)
        }
        WM_TIMER => {
            if wparam.0 == TIMER_DEBOUNCE
                && let Ok(mut state) = state.try_borrow_mut()
            {
                state.update_scheduled = false;
                state.retry_timer_scheduled = false;
                unsafe {
                    let _ = KillTimer(Some(hwnd), TIMER_DEBOUNCE);
                }
                state.update_brightness_state();
            }
            LRESULT(0)
        }
        WM_DISPLAYCHANGE => {
            if let Ok(mut state) = state.try_borrow_mut() {
                state.schedule_display_refresh();
            }
            LRESULT(0)
        }
        WM_POWERBROADCAST if wparam.0 == PBT_APMRESUMEAUTOMATIC as usize => {
            if let Ok(mut state) = state.try_borrow_mut() {
                state.schedule_display_refresh();
            }
            LRESULT(1)
        }
        WM_CLOSE => {
            PostQuitMessage(0);
            LRESULT(0)
        }
        WM_DESTROY => {
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, message, wparam, lparam),
    }
}

enum AppAction {
    None,
    Exit,
    ShowDiagnostics(String),
}

fn dispatch_command(hwnd: HWND, state: &RefCell<AppState>, command_id: usize) {
    let action = state
        .try_borrow_mut()
        .map(|mut state| state.handle_command(command_id))
        .unwrap_or(AppAction::None);

    match action {
        AppAction::None => {}
        AppAction::Exit => unsafe {
            PostQuitMessage(0);
        },
        AppAction::ShowDiagnostics(text) => show_diagnostics(hwnd, &text),
    }
}

#[derive(Clone)]
struct AppSettings {
    empty_brightness: u32,
    minimum_overlap_pixels: i32,
    minimum_overlap_width: i32,
    minimum_overlap_height: i32,
    event_debounce_milliseconds: u32,
    brightness_retry_milliseconds: u32,
    include_tool_windows: bool,
    excluded_process_names: HashSet<String>,
    excluded_class_names: HashSet<String>,
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            empty_brightness: 0,
            minimum_overlap_pixels: 1024,
            minimum_overlap_width: 32,
            minimum_overlap_height: 32,
            event_debounce_milliseconds: 1000,
            brightness_retry_milliseconds: 5000,
            include_tool_windows: false,
            excluded_process_names: [
                "LockApp",
                "SearchHost",
                "ShellExperienceHost",
                "StartMenuExperienceHost",
                "TextInputHost",
                "WidgetService",
                "Widgets",
            ]
            .into_iter()
            .map(normalize_name)
            .collect(),
            excluded_class_names: [
                "NotifyIconOverflowWindow",
                "Progman",
                "Shell_SecondaryTrayWnd",
                "Shell_TrayWnd",
                "WorkerW",
            ]
            .into_iter()
            .map(normalize_name)
            .collect(),
        }
    }
}

struct AppState {
    hwnd: HWND,
    settings: AppSettings,
    monitor_manager: MonitorManager,
    window_tracker: WindowTracker,
    event_watcher: WinEventWatcher,
    tray_icon: TrayIcon,
    update_scheduled: bool,
    retry_timer_scheduled: bool,
    display_refresh_needed: bool,
    last_error: Option<String>,
}

impl AppState {
    fn new(hwnd: HWND, settings: AppSettings) -> windows::core::Result<Self> {
        let monitor_manager = MonitorManager::new(settings.clone());
        let window_tracker = WindowTracker::new(settings.clone());
        let tray_icon = TrayIcon::new(hwnd)?;
        let event_watcher = WinEventWatcher::new(hwnd)?;

        let mut state = Self {
            hwnd,
            settings,
            monitor_manager,
            window_tracker,
            event_watcher,
            tray_icon,
            update_scheduled: false,
            retry_timer_scheduled: false,
            display_refresh_needed: false,
            last_error: None,
        };

        state.update_brightness_state();
        Ok(state)
    }

    fn handle_command(&mut self, command_id: usize) -> AppAction {
        match command_id {
            ID_BRIGHTNESS_0 => self.set_brightness_now(0),
            ID_BRIGHTNESS_10 => self.set_brightness_now(10),
            ID_BRIGHTNESS_25 => self.set_brightness_now(25),
            ID_BRIGHTNESS_50 => self.set_brightness_now(50),
            ID_BRIGHTNESS_75 => self.set_brightness_now(75),
            ID_BRIGHTNESS_100 => self.set_brightness_now(100),
            ID_TOGGLE_DIMMING => self.toggle_tray_brightness(),
            ID_REFRESH => self.refresh_now(),
            ID_DIAGNOSTICS => return AppAction::ShowDiagnostics(self.build_diagnostics()),
            ID_EXIT => return AppAction::Exit,
            _ => {}
        }

        AppAction::None
    }

    fn toggle_tray_brightness(&mut self) {
        self.cancel_scheduled_update();

        if self.monitor_manager.manual_dimming_enabled() {
            let windows = self.window_tracker.get_tracked_windows();
            self.monitor_manager.restore_all_from_user(&windows);
        } else {
            self.monitor_manager.dim_all();
        }

        self.update_tray_text();
        self.schedule_pending_retry();
    }

    fn set_brightness_now(&mut self, brightness: u32) {
        let windows = self.window_tracker.get_tracked_windows();
        self.monitor_manager
            .set_all_brightness(brightness, &windows);
        self.update_tray_text();
        self.schedule_pending_retry();
    }

    fn schedule_update(&mut self) {
        if self.update_scheduled && !self.retry_timer_scheduled {
            return;
        }

        self.set_update_timer(self.settings.event_debounce_milliseconds, false);
    }

    fn set_update_timer(&mut self, delay_milliseconds: u32, is_retry: bool) {
        self.update_scheduled = true;
        self.retry_timer_scheduled = is_retry;
        unsafe {
            let _ = KillTimer(Some(self.hwnd), TIMER_DEBOUNCE);
            let timer_id = SetTimer(
                Some(self.hwnd),
                TIMER_DEBOUNCE,
                delay_milliseconds.max(1),
                None,
            );
            if timer_id == 0 {
                self.update_scheduled = false;
                self.retry_timer_scheduled = false;
                self.last_error = Some("Failed to schedule brightness update".to_string());
                self.update_tray_text();
            }
        }
    }

    fn schedule_pending_retry(&mut self) {
        if self.monitor_manager.pending_operation_count() > 0 && !self.update_scheduled {
            self.set_update_timer(self.settings.brightness_retry_milliseconds, true);
        }
    }

    fn schedule_display_refresh(&mut self) {
        self.display_refresh_needed = true;
        self.schedule_update();
    }

    fn cancel_scheduled_update(&mut self) {
        self.update_scheduled = false;
        self.retry_timer_scheduled = false;
        unsafe {
            let _ = KillTimer(Some(self.hwnd), TIMER_DEBOUNCE);
        }
    }

    fn refresh_now(&mut self) {
        self.cancel_scheduled_update();
        self.display_refresh_needed = true;
        self.update_brightness_state();
    }

    fn update_brightness_state(&mut self) {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let windows = self.window_tracker.get_tracked_windows();
            if std::mem::take(&mut self.display_refresh_needed) {
                self.monitor_manager.reenumerate(&windows);
            } else {
                self.monitor_manager.update(&windows);
            }
        }));

        self.last_error = result.err().map(|_| "Unexpected update error".to_string());
        self.update_tray_text();
        self.schedule_pending_retry();
    }

    fn update_tray_text(&mut self) {
        let text = if let Some(error) = &self.last_error {
            format!("{APP_NAME} - Error: {error}")
        } else if self.monitor_manager.pending_operation_count() > 0 {
            format!(
                "{APP_NAME} - retrying {} brightness update(s)",
                self.monitor_manager.pending_operation_count()
            )
        } else {
            format!(
                "{APP_NAME} - {}/{} dimmed",
                self.monitor_manager.dimmed_display_count(),
                self.monitor_manager.controllable_display_count()
            )
        };

        self.tray_icon.set_tooltip(&text);
    }

    fn restore_tray_icon(&mut self) {
        match self.tray_icon.add() {
            Ok(()) => {
                if self
                    .last_error
                    .as_deref()
                    .is_some_and(|error| error.starts_with("Failed to restore tray icon:"))
                {
                    self.last_error = None;
                }
            }
            Err(error) => {
                self.last_error = Some(format!("Failed to restore tray icon: {error}"));
            }
        }
        self.update_tray_text();
    }

    fn build_diagnostics(&self) -> String {
        let mut text = String::new();
        text.push_str(&format!(
            "Dimmed displays: {}\r\n",
            self.monitor_manager.dimmed_display_count()
        ));
        text.push_str(&format!(
            "Debounce: {} ms\r\n",
            self.settings.event_debounce_milliseconds
        ));
        text.push_str(&format!(
            "Brightness retry: {} ms\r\n",
            self.settings.brightness_retry_milliseconds
        ));
        text.push_str(&format!(
            "Window events: {}\r\n",
            self.event_watcher.event_count()
        ));
        text.push_str(&format!(
            "Last window event: {}\r\n\r\n",
            self.event_watcher.last_event_description()
        ));
        text.push_str(&self.monitor_manager.build_diagnostic_text());

        if let Some(error) = &self.last_error {
            text.push_str("\r\n\r\nLast error: ");
            text.push_str(error);
        }

        text
    }
}

fn show_context_menu(hwnd: HWND, manual_dimming_enabled: bool) -> Option<usize> {
    unsafe {
        let Ok(menu) = CreatePopupMenu() else {
            return None;
        };

        append_checkable_menu_string(menu, ID_TOGGLE_DIMMING, "Dimming", manual_dimming_enabled);
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, None);

        for (id, label) in [
            (ID_BRIGHTNESS_0, "0%"),
            (ID_BRIGHTNESS_10, "10%"),
            (ID_BRIGHTNESS_25, "25%"),
            (ID_BRIGHTNESS_50, "50%"),
            (ID_BRIGHTNESS_75, "75%"),
            (ID_BRIGHTNESS_100, "100%"),
        ] {
            append_menu_string(menu, id, label);
        }

        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, None);
        append_menu_string(menu, ID_REFRESH, "Refresh now");
        append_menu_string(menu, ID_DIAGNOSTICS, "Diagnostics");
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, None);
        append_menu_string(menu, ID_EXIT, "Exit");

        let mut command_id = 0;
        let mut point = POINT::default();
        if GetCursorPos(&mut point).is_ok() {
            let _ = SetForegroundWindow(hwnd);
            command_id = TrackPopupMenu(
                menu,
                TPM_RIGHTBUTTON | TPM_RETURNCMD | TPM_NONOTIFY,
                point.x,
                point.y,
                Some(0),
                hwnd,
                None,
            )
            .0;
            let _ = PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0));
        }

        let _ = DestroyMenu(menu);
        (command_id > 0).then_some(command_id as usize)
    }
}

fn show_diagnostics(hwnd: HWND, text: &str) {
    let text = wide_null(text);
    unsafe {
        MessageBoxW(
            Some(hwnd),
            PCWSTR(text.as_ptr()),
            w!("MMD Diagnostics"),
            MB_OK | MB_ICONINFORMATION,
        );
    }
}

impl Drop for AppState {
    fn drop(&mut self) {
        self.monitor_manager.restore_all();
    }
}

struct TrayIcon {
    hwnd: HWND,
    icon: HICON,
    owns_icon: bool,
}

impl TrayIcon {
    fn new(hwnd: HWND) -> windows::core::Result<Self> {
        let (icon, owns_icon) = load_app_icon();
        let mut tray_icon = Self {
            hwnd,
            icon,
            owns_icon,
        };
        tray_icon.add()?;
        tray_icon.set_tooltip(APP_NAME);
        Ok(tray_icon)
    }

    fn add(&mut self) -> windows::core::Result<()> {
        let mut data = self.data();
        data.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
        data.uCallbackMessage = WM_TRAYICON;
        data.hIcon = self.icon;
        write_tip(&mut data, APP_NAME);

        unsafe {
            if !Shell_NotifyIconW(NIM_ADD, &data).as_bool() {
                return Err(Error::from_thread());
            }
        }

        Ok(())
    }

    fn set_tooltip(&mut self, text: &str) {
        let mut data = self.data();
        data.uFlags = NIF_TIP;
        write_tip(&mut data, text);

        unsafe {
            let _ = Shell_NotifyIconW(NIM_MODIFY, &data);
        }
    }

    fn data(&self) -> NOTIFYICONDATAW {
        NOTIFYICONDATAW {
            cbSize: size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: self.hwnd,
            uID: TRAY_UID,
            ..Default::default()
        }
    }
}

impl Drop for TrayIcon {
    fn drop(&mut self) {
        let data = self.data();
        unsafe {
            let _ = Shell_NotifyIconW(NIM_DELETE, &data);
            if self.owns_icon && !self.icon.0.is_null() {
                let _ = DestroyIcon(self.icon);
            }
        }
    }
}

struct WinEventWatcher {
    hooks: Vec<HWINEVENTHOOK>,
}

impl WinEventWatcher {
    fn new(hwnd: HWND) -> windows::core::Result<Self> {
        EVENT_TARGET_HWND.store(hwnd.0 as isize, Ordering::SeqCst);
        let mut watcher = Self { hooks: Vec::new() };

        unsafe {
            watcher.add_hook(EVENT_SYSTEM_FOREGROUND, EVENT_SYSTEM_FOREGROUND)?;
            watcher.add_hook(EVENT_SYSTEM_MINIMIZESTART, EVENT_SYSTEM_MINIMIZEEND)?;
            watcher.add_hook(EVENT_SYSTEM_DESKTOPSWITCH, EVENT_SYSTEM_DESKTOPSWITCH)?;
            watcher.add_hook(EVENT_OBJECT_CREATE, EVENT_OBJECT_LOCATIONCHANGE)?;
            watcher.add_hook(EVENT_OBJECT_CLOAKED, EVENT_OBJECT_UNCLOAKED)?;
        }

        Ok(watcher)
    }

    unsafe fn add_hook(&mut self, event_min: u32, event_max: u32) -> windows::core::Result<()> {
        let hook = SetWinEventHook(
            event_min,
            event_max,
            None,
            Some(win_event_proc),
            0,
            0,
            WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS,
        );

        if hook.0.is_null() {
            Err(Error::from_thread())
        } else {
            self.hooks.push(hook);
            Ok(())
        }
    }

    fn event_count(&self) -> i64 {
        EVENT_COUNT.load(Ordering::SeqCst)
    }

    fn last_event_description(&self) -> String {
        let Some(lock) = LAST_EVENT.get() else {
            return "(none)".to_string();
        };

        let Some(last_event) = lock.lock().ok().and_then(|guard| guard.clone()) else {
            return "(none)".to_string();
        };

        format!(
            "{} at {}",
            get_event_name(last_event.event_type),
            format_system_time(last_event.when)
        )
    }
}

impl Drop for WinEventWatcher {
    fn drop(&mut self) {
        EVENT_TARGET_HWND.store(0, Ordering::SeqCst);
        for hook in self.hooks.drain(..) {
            unsafe {
                let _ = UnhookWinEvent(hook);
            }
        }
    }
}

#[derive(Clone)]
struct LastEvent {
    event_type: u32,
    when: SystemTime,
}

unsafe extern "system" fn win_event_proc(
    _hook: HWINEVENTHOOK,
    event_type: u32,
    hwnd: HWND,
    id_object: i32,
    id_child: i32,
    _event_thread: u32,
    _event_time: u32,
) {
    if !is_relevant_window_event(event_type, !hwnd.0.is_null(), id_object, id_child) {
        return;
    }

    EVENT_COUNT.fetch_add(1, Ordering::SeqCst);
    let lock = LAST_EVENT.get_or_init(|| Mutex::new(None));
    if let Ok(mut last_event) = lock.lock() {
        *last_event = Some(LastEvent {
            event_type,
            when: SystemTime::now(),
        });
    }

    let target = EVENT_TARGET_HWND.load(Ordering::SeqCst);
    if target != 0 {
        let _ = PostMessageW(
            Some(HWND(target as *mut c_void)),
            WM_WINDOW_EVENT,
            WPARAM(event_type as usize),
            LPARAM(0),
        );
    }
}

fn is_relevant_window_event(
    event_type: u32,
    has_window: bool,
    id_object: i32,
    id_child: i32,
) -> bool {
    if matches!(
        event_type,
        EVENT_SYSTEM_FOREGROUND
            | EVENT_SYSTEM_MINIMIZESTART
            | EVENT_SYSTEM_MINIMIZEEND
            | EVENT_SYSTEM_DESKTOPSWITCH
    ) {
        return true;
    }

    has_window
        && id_object == OBJID_WINDOW.0
        && id_child == CHILDID_SELF as i32
        && matches!(
            event_type,
            EVENT_OBJECT_CREATE
                | EVENT_OBJECT_DESTROY
                | EVENT_OBJECT_SHOW
                | EVENT_OBJECT_HIDE
                | EVENT_OBJECT_LOCATIONCHANGE
                | EVENT_OBJECT_CLOAKED
                | EVENT_OBJECT_UNCLOAKED
        )
}

struct MonitorManager {
    settings: AppSettings,
    displays: Vec<DisplayState>,
    detached_snapshots: HashMap<String, DisplaySnapshot>,
    manual_dimming_enabled: bool,
}

impl MonitorManager {
    fn new(settings: AppSettings) -> Self {
        let mut manager = Self {
            settings,
            displays: Vec::new(),
            detached_snapshots: HashMap::new(),
            manual_dimming_enabled: false,
        };
        manager.enumerate_displays();
        manager
    }

    fn controllable_display_count(&self) -> usize {
        self.displays
            .iter()
            .filter(|display| {
                display
                    .targets
                    .iter()
                    .any(PhysicalMonitorTarget::can_attempt_operations)
            })
            .count()
    }

    fn dimmed_display_count(&self) -> usize {
        self.displays
            .iter()
            .filter(|display| display.targets.iter().any(|target| target.is_dimmed))
            .count()
    }

    fn pending_operation_count(&self) -> usize {
        self.displays
            .iter()
            .filter(|display| display.pending_operation.is_some())
            .count()
    }

    fn manual_dimming_enabled(&self) -> bool {
        self.manual_dimming_enabled
    }

    fn update(&mut self, windows: &[TrackedWindow]) {
        for display in &mut self.displays {
            let (has_window, blocking_window) =
                try_find_blocking_window(&self.settings, display, windows);
            display.last_blocking_window = blocking_window;
            update_display_state(
                &self.settings,
                display,
                has_window,
                self.manual_dimming_enabled,
            );
        }
    }

    fn reenumerate(&mut self, windows: &[TrackedWindow]) {
        self.detached_snapshots
            .extend(capture_display_snapshots(&self.displays));
        self.displays.clear();
        self.enumerate_displays();
        restore_display_snapshots(&mut self.displays, &mut self.detached_snapshots);
        self.update(windows);
    }

    fn restore_all(&mut self) {
        for display in &mut self.displays {
            display.pending_operation = None;
            let mut restored = true;
            for target in &mut display.targets {
                restored &= target.restore_for_shutdown();
            }
            if restored {
                display.dimming_state = DisplayDimmingState::Lit;
            }
        }
    }

    fn restore_all_from_user(&mut self, windows: &[TrackedWindow]) {
        self.manual_dimming_enabled = false;
        for display in &mut self.displays {
            let (has_window, blocking_window) =
                try_find_blocking_window(&self.settings, display, windows);
            display.last_blocking_window = blocking_window;
            request_operation(
                display,
                PendingBrightnessOperation::Restore {
                    next_state: occupancy_state(has_window),
                },
            );
        }
    }

    fn dim_all(&mut self) {
        self.manual_dimming_enabled = true;
        for display in &mut self.displays {
            request_operation(
                display,
                PendingBrightnessOperation::Dim {
                    brightness: self.settings.empty_brightness,
                    next_state: DisplayDimmingState::UserDimmed,
                },
            );
            display.last_blocking_window = Some("Tray icon click".to_string());
        }
    }

    fn set_all_brightness(&mut self, brightness: u32, windows: &[TrackedWindow]) {
        self.manual_dimming_enabled = false;
        for display in &mut self.displays {
            let (has_window, blocking_window) =
                try_find_blocking_window(&self.settings, display, windows);
            request_operation(
                display,
                PendingBrightnessOperation::Set {
                    brightness,
                    next_state: occupancy_state(has_window),
                },
            );
            display.last_blocking_window =
                blocking_window.or_else(|| Some(format!("Brightness set to {brightness}%")));
        }
    }

    fn build_diagnostic_text(&self) -> String {
        let mut lines = vec![
            "DPI mode: PerMonitorV2".to_string(),
            format!("Manual dimming: {}", self.manual_dimming_enabled),
            format!(
                "Controllable displays: {}/{}",
                self.controllable_display_count(),
                self.displays.len()
            ),
            format!("Dimmed displays: {}", self.dimmed_display_count()),
            format!(
                "Detached display snapshots: {}",
                self.detached_snapshots.len()
            ),
            String::new(),
        ];

        for display in &self.displays {
            lines.push(format!(
                "{} {}x{} targets={}",
                display.device_name,
                display.bounds.width(),
                display.bounds.height(),
                display.targets.len()
            ));
            lines.push(format!("  State: {:?}", display.dimming_state));
            lines.push(format!("  Pending: {:?}", display.pending_operation));
            lines.push(format!(
                "  Blocking window: {}",
                display.last_blocking_window.as_deref().unwrap_or("(none)")
            ));
            for target in &display.targets {
                lines.push(format!(
                    "  Monitor: {} dimmed={} availability={:?}",
                    target.description, target.is_dimmed, target.availability
                ));
            }
        }

        lines.join("\r\n")
    }

    fn enumerate_displays(&mut self) {
        for display_info in get_display_infos() {
            let mut display = DisplayState {
                device_name: display_info.device_name,
                bounds: display_info.bounds,
                targets: Vec::new(),
                dimming_state: DisplayDimmingState::Lit,
                pending_operation: None,
                last_blocking_window: None,
            };

            unsafe {
                let mut physical_monitor_count = 0;
                if GetNumberOfPhysicalMonitorsFromHMONITOR(
                    display_info.handle,
                    &mut physical_monitor_count,
                )
                .is_err()
                    || physical_monitor_count == 0
                {
                    self.displays.push(display);
                    continue;
                }

                let mut physical_monitors =
                    vec![PHYSICAL_MONITOR::default(); physical_monitor_count as usize];
                if GetPhysicalMonitorsFromHMONITOR(display_info.handle, &mut physical_monitors)
                    .is_err()
                {
                    self.displays.push(display);
                    continue;
                }

                for (physical_index, physical_monitor) in physical_monitors.into_iter().enumerate()
                {
                    let handle = addr_of!(physical_monitor.hPhysicalMonitor).read_unaligned();
                    let description =
                        addr_of!(physical_monitor.szPhysicalMonitorDescription).read_unaligned();
                    let description = string_from_wide_slice(&description);
                    let key = TargetKey {
                        display_device_name: display.device_name.clone(),
                        description: normalize_name(&description),
                        physical_index,
                    };
                    display
                        .targets
                        .push(PhysicalMonitorTarget::new(handle, key, description));
                }
            }

            self.displays.push(display);
        }
    }
}

struct DisplayInfo {
    handle: HMONITOR,
    device_name: String,
    bounds: SimpleRect,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DisplayDimmingState {
    Lit,
    AutoDimmed,
    UserDimmed,
    UserRestoredWhileEmpty,
}

struct DisplayState {
    device_name: String,
    bounds: SimpleRect,
    targets: Vec<PhysicalMonitorTarget>,
    dimming_state: DisplayDimmingState,
    pending_operation: Option<PendingBrightnessOperation>,
    last_blocking_window: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct TargetKey {
    display_device_name: String,
    description: String,
    physical_index: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TargetAvailability {
    Unknown,
    Available,
    TemporarilyUnavailable,
    Unavailable,
    Unsupported,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TargetSnapshot {
    restore_brightness: Option<u32>,
    is_dimmed: bool,
}

#[derive(Debug, Clone)]
struct DisplaySnapshot {
    dimming_state: DisplayDimmingState,
    pending_operation: Option<PendingBrightnessOperation>,
    targets: HashMap<TargetKey, TargetSnapshot>,
}

fn capture_display_snapshots(displays: &[DisplayState]) -> HashMap<String, DisplaySnapshot> {
    displays
        .iter()
        .map(|display| {
            let targets = display
                .targets
                .iter()
                .map(|target| {
                    (
                        target.key.clone(),
                        TargetSnapshot {
                            restore_brightness: target.restore_brightness,
                            is_dimmed: target.is_dimmed,
                        },
                    )
                })
                .collect();
            (
                display.device_name.clone(),
                DisplaySnapshot {
                    dimming_state: display.dimming_state,
                    pending_operation: display.pending_operation,
                    targets,
                },
            )
        })
        .collect()
}

fn restore_display_snapshots(
    displays: &mut [DisplayState],
    snapshots: &mut HashMap<String, DisplaySnapshot>,
) {
    for display in displays {
        let Some(snapshot) = snapshots.remove(&display.device_name) else {
            continue;
        };

        let force_dim_reapplication = snapshot.pending_operation.is_none()
            && matches!(
                snapshot.dimming_state,
                DisplayDimmingState::AutoDimmed | DisplayDimmingState::UserDimmed
            );
        let mut matched_target = false;

        for target in &mut display.targets {
            let Some(target_snapshot) = snapshot.targets.get(&target.key) else {
                continue;
            };

            matched_target = true;
            target.restore_brightness = target_snapshot.restore_brightness;
            target.is_dimmed = if force_dim_reapplication {
                false
            } else {
                target_snapshot.is_dimmed
            };
        }

        if matched_target || snapshot.targets.is_empty() {
            display.dimming_state = snapshot.dimming_state;
            display.pending_operation = snapshot.pending_operation;
        } else {
            snapshots.insert(display.device_name.clone(), snapshot);
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PendingBrightnessOperation {
    Dim {
        brightness: u32,
        next_state: DisplayDimmingState,
    },
    Restore {
        next_state: DisplayDimmingState,
    },
    Set {
        brightness: u32,
        next_state: DisplayDimmingState,
    },
}

impl PendingBrightnessOperation {
    fn next_state(self) -> DisplayDimmingState {
        match self {
            Self::Dim { next_state, .. }
            | Self::Restore { next_state }
            | Self::Set { next_state, .. } => next_state,
        }
    }
}

struct PhysicalMonitorTarget {
    handle: HANDLE,
    key: TargetKey,
    description: String,
    restore_brightness: Option<u32>,
    is_dimmed: bool,
    availability: TargetAvailability,
    consecutive_failures: u8,
    disposed: bool,
}

impl PhysicalMonitorTarget {
    fn new(handle: HANDLE, key: TargetKey, description: String) -> Self {
        let mut target = Self {
            handle,
            key,
            description: if description.trim().is_empty() {
                "Unknown monitor".to_string()
            } else {
                description
            },
            restore_brightness: None,
            is_dimmed: false,
            availability: TargetAvailability::Unknown,
            consecutive_failures: 0,
            disposed: false,
        };
        target.probe_availability();
        target
    }

    fn probe_availability(&mut self) {
        if self.disposed {
            return;
        }

        let mut capabilities = 0;
        let mut color_temperatures = 0;
        let capabilities_available = unsafe {
            GetMonitorCapabilities(self.handle, &mut capabilities, &mut color_temperatures) != 0
        };

        if capabilities_available && capabilities & MC_CAPS_BRIGHTNESS == 0 {
            self.availability = TargetAvailability::Unsupported;
            return;
        }

        if self.try_read_brightness_full().is_some() {
            self.record_success();
        } else {
            let _ = self.record_failure();
        }
    }

    fn can_attempt_operations(&self) -> bool {
        !self.disposed
            && matches!(
                self.availability,
                TargetAvailability::Unknown
                    | TargetAvailability::Available
                    | TargetAvailability::TemporarilyUnavailable
            )
    }

    fn record_success(&mut self) {
        self.availability = TargetAvailability::Available;
        self.consecutive_failures = 0;
    }

    fn record_failure(&mut self) -> bool {
        self.consecutive_failures = self.consecutive_failures.saturating_add(1);
        if self.consecutive_failures >= MAX_TARGET_FAILURES {
            self.availability = TargetAvailability::Unavailable;
            true
        } else {
            self.availability = TargetAvailability::TemporarilyUnavailable;
            false
        }
    }

    fn dim(&mut self, empty_brightness_percent: u32) -> bool {
        if self.disposed {
            return false;
        }
        if !self.can_attempt_operations() {
            return true;
        }

        let Some((minimum, current, maximum)) = self.try_read_brightness_full() else {
            return self.record_failure();
        };

        if !self.is_dimmed && self.restore_brightness.is_none() {
            self.restore_brightness = Some(current);
        }

        let brightness = percent_to_raw(minimum, maximum, empty_brightness_percent);
        if self.try_set_brightness(brightness) {
            self.record_success();
            self.is_dimmed = true;
            true
        } else {
            self.record_failure()
        }
    }

    fn set_brightness(&mut self, brightness_percent: u32) -> bool {
        if self.disposed {
            return false;
        }
        if !self.can_attempt_operations() {
            return true;
        }

        let Some((minimum, _, maximum)) = self.try_read_brightness_full() else {
            return self.record_failure();
        };

        let brightness = percent_to_raw(minimum, maximum, brightness_percent);
        if self.try_set_brightness(brightness) {
            self.record_success();
            self.restore_brightness = Some(brightness);
            self.is_dimmed = false;
            true
        } else {
            self.record_failure()
        }
    }

    fn restore(&mut self) -> bool {
        if self.disposed {
            return false;
        }
        if !self.is_dimmed {
            return true;
        }
        if !self.can_attempt_operations() {
            return true;
        }

        let Some(restore_brightness) = self.restore_brightness else {
            self.is_dimmed = false;
            return true;
        };

        if self.try_set_brightness(restore_brightness) {
            self.record_success();
            self.is_dimmed = false;
            true
        } else {
            self.record_failure()
        }
    }

    fn restore_for_shutdown(&mut self) -> bool {
        if self.disposed {
            return false;
        }
        if !self.is_dimmed {
            return true;
        }

        let Some(restore_brightness) = self.restore_brightness else {
            return false;
        };
        if self.try_set_brightness(restore_brightness) {
            self.is_dimmed = false;
            true
        } else {
            false
        }
    }

    fn try_read_brightness_full(&self) -> Option<(u32, u32, u32)> {
        unsafe {
            let mut minimum = 0;
            let mut current = 0;
            let mut maximum = 0;
            if GetMonitorBrightness(self.handle, &mut minimum, &mut current, &mut maximum) != 0 {
                Some((minimum, current, maximum))
            } else {
                None
            }
        }
    }

    fn try_set_brightness(&self, brightness: u32) -> bool {
        unsafe { SetMonitorBrightness(self.handle, brightness) != 0 }
    }
}

impl Drop for PhysicalMonitorTarget {
    fn drop(&mut self) {
        if !self.disposed {
            unsafe {
                let _ = DestroyPhysicalMonitor(self.handle);
            }
            self.disposed = true;
        }
    }
}

struct WindowTracker {
    settings: AppSettings,
}

impl WindowTracker {
    fn new(settings: AppSettings) -> Self {
        Self { settings }
    }

    fn get_tracked_windows(&self) -> Vec<TrackedWindow> {
        let mut context = EnumWindowContext {
            settings: &self.settings,
            shell_window: unsafe { GetShellWindow() },
            windows: Vec::new(),
        };

        unsafe {
            let _ = EnumWindows(
                Some(enum_windows_proc),
                LPARAM(addr_of_mut!(context) as isize),
            );
        }

        context.windows
    }
}

struct EnumWindowContext<'a> {
    settings: &'a AppSettings,
    shell_window: HWND,
    windows: Vec<TrackedWindow>,
}

#[derive(Clone)]
struct TrackedWindow {
    process_name: String,
    title: String,
    bounds: SimpleRect,
}

unsafe extern "system" fn enum_windows_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let context = &mut *(lparam.0 as *mut EnumWindowContext<'_>);
    if let Some(window) = try_get_tracked_window(hwnd, context.shell_window, context.settings) {
        context.windows.push(window);
    }

    BOOL(1)
}

fn try_get_tracked_window(
    hwnd: HWND,
    shell_window: HWND,
    settings: &AppSettings,
) -> Option<TrackedWindow> {
    unsafe {
        if hwnd.0.is_null()
            || hwnd == shell_window
            || !IsWindowVisible(hwnd).as_bool()
            || IsIconic(hwnd).as_bool()
        {
            return None;
        }

        let title_length = GetWindowTextLengthW(hwnd);

        let ex_style = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        if !settings.include_tool_windows && (ex_style & WS_EX_TOOLWINDOW) == WS_EX_TOOLWINDOW {
            return None;
        }

        let mut cloaked = 0u32;
        if DwmGetWindowAttribute(
            hwnd,
            DWMWA_CLOAKED,
            &mut cloaked as *mut _ as *mut c_void,
            size_of::<u32>() as u32,
        )
        .is_ok()
            && cloaked != 0
        {
            return None;
        }

        let mut rect = RECT::default();
        if GetWindowRect(hwnd, &mut rect).is_err()
            || rect.right <= rect.left
            || rect.bottom <= rect.top
        {
            return None;
        }

        let title = if title_length > 0 {
            window_title_or_placeholder(get_window_text(hwnd, title_length + 1))
        } else {
            window_title_or_placeholder(String::new())
        };

        let class_name = get_class_name(hwnd);
        if settings
            .excluded_class_names
            .contains(&normalize_name(&class_name))
        {
            return None;
        }

        let mut process_id = 0u32;
        GetWindowThreadProcessId(hwnd, Some(&mut process_id));
        let process_name = get_process_name(process_id);
        if !process_name.is_empty()
            && settings
                .excluded_process_names
                .contains(&normalize_name(&process_name))
        {
            return None;
        }

        Some(TrackedWindow {
            process_name,
            title,
            bounds: SimpleRect::from_rect(rect),
        })
    }
}

#[derive(Clone, Copy)]
struct SimpleRect {
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
}

impl SimpleRect {
    fn from_rect(rect: RECT) -> Self {
        Self {
            left: rect.left,
            top: rect.top,
            right: rect.right,
            bottom: rect.bottom,
        }
    }

    fn width(self) -> i32 {
        self.right - self.left
    }

    fn height(self) -> i32 {
        self.bottom - self.top
    }

    fn is_empty(self) -> bool {
        self.width() <= 0 || self.height() <= 0
    }

    fn intersect(self, other: Self) -> Self {
        Self {
            left: self.left.max(other.left),
            top: self.top.max(other.top),
            right: self.right.min(other.right),
            bottom: self.bottom.min(other.bottom),
        }
    }
}

fn try_find_blocking_window(
    settings: &AppSettings,
    display: &DisplayState,
    windows: &[TrackedWindow],
) -> (bool, Option<String>) {
    for window in windows {
        let intersection = display.bounds.intersect(window.bounds);
        if intersection.is_empty() {
            continue;
        }

        let area = intersection.width() * intersection.height();
        if area >= settings.minimum_overlap_pixels
            && intersection.width() >= settings.minimum_overlap_width
            && intersection.height() >= settings.minimum_overlap_height
        {
            return (
                true,
                Some(format!("{}: {}", window.process_name, window.title)),
            );
        }
    }

    (false, None)
}

fn update_display_state(
    settings: &AppSettings,
    display: &mut DisplayState,
    has_window: bool,
    manual_dimming_enabled: bool,
) {
    if display.targets.is_empty() {
        display.dimming_state = DisplayDimmingState::Lit;
        display.pending_operation = None;
        return;
    }

    if manual_dimming_enabled {
        if display.pending_operation.is_some() {
            display.pending_operation = Some(PendingBrightnessOperation::Dim {
                brightness: settings.empty_brightness,
                next_state: DisplayDimmingState::UserDimmed,
            });
            if !apply_pending_operation(display) {
                return;
            }
        }

        if display
            .targets
            .iter()
            .any(|target| target.can_attempt_operations() && !target.is_dimmed)
        {
            request_operation(
                display,
                PendingBrightnessOperation::Dim {
                    brightness: settings.empty_brightness,
                    next_state: DisplayDimmingState::UserDimmed,
                },
            );
        } else {
            display.dimming_state = DisplayDimmingState::UserDimmed;
        }
        return;
    }

    if let Some(operation) = display.pending_operation {
        display.pending_operation = Some(reconcile_pending_operation(
            operation,
            has_window,
            settings.empty_brightness,
        ));
        if !apply_pending_operation(display) {
            return;
        }
    }

    if has_window {
        match display.dimming_state {
            DisplayDimmingState::AutoDimmed => request_operation(
                display,
                PendingBrightnessOperation::Restore {
                    next_state: DisplayDimmingState::Lit,
                },
            ),
            DisplayDimmingState::UserRestoredWhileEmpty => {
                display.dimming_state = DisplayDimmingState::Lit;
            }
            DisplayDimmingState::Lit
                if display
                    .targets
                    .iter()
                    .any(|target| target.can_attempt_operations() && target.is_dimmed) =>
            {
                request_operation(
                    display,
                    PendingBrightnessOperation::Restore {
                        next_state: DisplayDimmingState::Lit,
                    },
                );
            }
            _ => {}
        }
        return;
    }

    match display.dimming_state {
        DisplayDimmingState::Lit => request_operation(
            display,
            PendingBrightnessOperation::Dim {
                brightness: settings.empty_brightness,
                next_state: DisplayDimmingState::AutoDimmed,
            },
        ),
        DisplayDimmingState::UserDimmed => {
            display.dimming_state = DisplayDimmingState::AutoDimmed;
        }
        DisplayDimmingState::AutoDimmed => {
            if display
                .targets
                .iter()
                .any(|target| target.can_attempt_operations() && !target.is_dimmed)
            {
                request_operation(
                    display,
                    PendingBrightnessOperation::Dim {
                        brightness: settings.empty_brightness,
                        next_state: display.dimming_state,
                    },
                );
            }
        }
        DisplayDimmingState::UserRestoredWhileEmpty => {
            if display
                .targets
                .iter()
                .any(|target| target.can_attempt_operations() && target.is_dimmed)
            {
                request_operation(
                    display,
                    PendingBrightnessOperation::Restore {
                        next_state: DisplayDimmingState::UserRestoredWhileEmpty,
                    },
                );
            }
        }
    }
}

fn occupancy_state(has_window: bool) -> DisplayDimmingState {
    if has_window {
        DisplayDimmingState::Lit
    } else {
        DisplayDimmingState::UserRestoredWhileEmpty
    }
}

fn percent_to_raw(minimum: u32, maximum: u32, percent: u32) -> u32 {
    if maximum <= minimum {
        return minimum;
    }

    let range = u64::from(maximum - minimum);
    let scaled = (range * u64::from(percent.min(100)) + 50) / 100;
    minimum + scaled as u32
}

fn reconcile_pending_operation(
    operation: PendingBrightnessOperation,
    has_window: bool,
    empty_brightness: u32,
) -> PendingBrightnessOperation {
    match operation {
        PendingBrightnessOperation::Dim {
            next_state: DisplayDimmingState::AutoDimmed,
            ..
        } if has_window => PendingBrightnessOperation::Restore {
            next_state: DisplayDimmingState::Lit,
        },
        PendingBrightnessOperation::Restore {
            next_state: DisplayDimmingState::Lit,
        } if !has_window => PendingBrightnessOperation::Dim {
            brightness: empty_brightness,
            next_state: DisplayDimmingState::AutoDimmed,
        },
        PendingBrightnessOperation::Restore {
            next_state: DisplayDimmingState::UserRestoredWhileEmpty,
        } if has_window => PendingBrightnessOperation::Restore {
            next_state: DisplayDimmingState::Lit,
        },
        PendingBrightnessOperation::Set { brightness, .. } => PendingBrightnessOperation::Set {
            brightness,
            next_state: occupancy_state(has_window),
        },
        operation => operation,
    }
}

fn request_operation(display: &mut DisplayState, operation: PendingBrightnessOperation) {
    if display.targets.is_empty() {
        display.dimming_state = DisplayDimmingState::Lit;
        display.pending_operation = None;
        return;
    }

    display.pending_operation = Some(operation);
    let _ = apply_pending_operation(display);
}

fn apply_pending_operation(display: &mut DisplayState) -> bool {
    let Some(operation) = display.pending_operation else {
        return true;
    };

    let mut succeeded = true;
    for target in &mut display.targets {
        succeeded &= match operation {
            PendingBrightnessOperation::Dim { brightness, .. } => target.dim(brightness),
            PendingBrightnessOperation::Restore { .. } => target.restore(),
            PendingBrightnessOperation::Set { brightness, .. } => target.set_brightness(brightness),
        };
    }

    if succeeded {
        display.dimming_state = operation.next_state();
        display.pending_operation = None;
    }

    succeeded
}

unsafe extern "system" fn enum_display_monitor_proc(
    monitor: HMONITOR,
    _hdc: HDC,
    _rect: *mut RECT,
    data: LPARAM,
) -> BOOL {
    let displays = &mut *(data.0 as *mut Vec<DisplayInfo>);
    let mut info = MONITORINFOEXW::default();
    info.monitorInfo.cbSize = size_of::<MONITORINFOEXW>() as u32;

    if GetMonitorInfoW(monitor, &mut info as *mut _ as *mut _).as_bool() {
        let device_name = string_from_wide_slice(&info.szDevice);
        if !device_name.trim().is_empty() {
            displays.push(DisplayInfo {
                handle: monitor,
                device_name,
                bounds: SimpleRect::from_rect(info.monitorInfo.rcMonitor),
            });
        }
    }

    BOOL(1)
}

fn get_display_infos() -> Vec<DisplayInfo> {
    let mut displays = Vec::new();
    unsafe {
        let _ = EnumDisplayMonitors(
            None,
            None,
            Some(enum_display_monitor_proc),
            LPARAM(addr_of_mut!(displays) as isize),
        );
    }
    displays
}

fn get_window_text(hwnd: HWND, capacity: i32) -> String {
    let mut buffer = vec![0u16; capacity.max(1) as usize];
    unsafe {
        let len = GetWindowTextW(hwnd, &mut buffer);
        String::from_utf16_lossy(&buffer[..len.max(0) as usize])
    }
}

fn window_title_or_placeholder(title: String) -> String {
    if title.trim().is_empty() {
        "(untitled)".to_string()
    } else {
        title
    }
}

fn get_class_name(hwnd: HWND) -> String {
    let mut buffer = vec![0u16; 256];
    unsafe {
        let len = GetClassNameW(hwnd, &mut buffer);
        String::from_utf16_lossy(&buffer[..len.max(0) as usize])
    }
}

fn get_process_name(process_id: u32) -> String {
    if process_id == 0 {
        return String::new();
    }

    unsafe {
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, process_id);
        let Ok(process) = process else {
            return String::new();
        };

        let mut buffer = vec![0u16; 32768];
        let mut size = buffer.len() as u32;
        let ok = QueryFullProcessImageNameW(
            process,
            PROCESS_NAME_WIN32,
            PWSTR(buffer.as_mut_ptr()),
            &mut size,
        );
        let _ = CloseHandle(process);

        if ok.is_err() || size == 0 {
            return String::new();
        }

        let path = String::from_utf16_lossy(&buffer[..size as usize]);
        Path::new(&path)
            .file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or_default()
            .to_string()
    }
}

fn append_menu_string(menu: HMENU, id: usize, text: &str) {
    let text = wide_null(text);
    unsafe {
        let _ = AppendMenuW(menu, MF_STRING, id, PCWSTR(text.as_ptr()));
    }
}

fn append_checkable_menu_string(menu: HMENU, id: usize, text: &str, checked: bool) {
    let text = wide_null(text);
    let checked_flag = if checked { MF_CHECKED } else { MF_UNCHECKED };
    unsafe {
        let _ = AppendMenuW(menu, MF_STRING | checked_flag, id, PCWSTR(text.as_ptr()));
    }
}

fn load_app_icon() -> (HICON, bool) {
    unsafe {
        if let Ok(module) = GetModuleHandleW(None)
            && let Ok(handle) = LoadImageW(
                Some(HINSTANCE(module.0)),
                APP_ICON_RESOURCE_ID,
                IMAGE_ICON,
                0,
                0,
                LR_DEFAULTSIZE,
            )
        {
            return (HICON(handle.0), true);
        }

        (LoadIconW(None, IDI_APPLICATION).unwrap_or_default(), false)
    }
}

fn write_tip(data: &mut NOTIFYICONDATAW, text: &str) {
    data.szTip.fill(0);
    let mut wide = text
        .encode_utf16()
        .take(data.szTip.len() - 1)
        .collect::<Vec<_>>();
    if wide.len() == data.szTip.len() - 1 && text.encode_utf16().count() > wide.len() {
        wide.truncate(data.szTip.len() - 4);
        wide.extend("...".encode_utf16());
    }

    for (index, value) in wide.into_iter().enumerate() {
        data.szTip[index] = value;
    }
}

fn wide_null(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(Some(0)).collect()
}

fn string_from_wide_slice(buffer: &[u16]) -> String {
    let len = buffer
        .iter()
        .position(|value| *value == 0)
        .unwrap_or(buffer.len());
    String::from_utf16_lossy(&buffer[..len])
}

fn normalize_name(text: &str) -> String {
    text.to_ascii_lowercase()
}

fn low_word(value: isize) -> u32 {
    (value as u32) & 0xffff
}

fn get_event_name(event_type: u32) -> String {
    match event_type {
        EVENT_SYSTEM_FOREGROUND => "Foreground".to_string(),
        EVENT_SYSTEM_MINIMIZESTART => "Minimize start".to_string(),
        EVENT_SYSTEM_MINIMIZEEND => "Minimize end".to_string(),
        EVENT_SYSTEM_DESKTOPSWITCH => "Desktop switch".to_string(),
        EVENT_OBJECT_CLOAKED => "Object cloaked".to_string(),
        EVENT_OBJECT_UNCLOAKED => "Object uncloaked".to_string(),
        EVENT_OBJECT_CREATE..=EVENT_OBJECT_LOCATIONCHANGE => "Object change".to_string(),
        _ => format!("0x{event_type:04X}"),
    }
}

fn format_system_time(time: SystemTime) -> String {
    match time.duration_since(SystemTime::UNIX_EPOCH) {
        Ok(duration) => format!(
            "unix+{}.{:03}s",
            duration.as_secs(),
            duration.subsec_millis()
        ),
        Err(_) => "(unknown time)".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_target(
        display_device_name: &str,
        description: &str,
        physical_index: usize,
        restore_brightness: Option<u32>,
        is_dimmed: bool,
    ) -> PhysicalMonitorTarget {
        PhysicalMonitorTarget {
            handle: HANDLE::default(),
            key: TargetKey {
                display_device_name: display_device_name.to_string(),
                description: normalize_name(description),
                physical_index,
            },
            description: description.to_string(),
            restore_brightness,
            is_dimmed,
            availability: TargetAvailability::Available,
            consecutive_failures: 0,
            disposed: true,
        }
    }

    fn test_display(
        device_name: &str,
        targets: Vec<PhysicalMonitorTarget>,
        dimming_state: DisplayDimmingState,
        pending_operation: Option<PendingBrightnessOperation>,
    ) -> DisplayState {
        DisplayState {
            device_name: device_name.to_string(),
            bounds: SimpleRect {
                left: 0,
                top: 0,
                right: 1,
                bottom: 1,
            },
            targets,
            dimming_state,
            pending_operation,
            last_blocking_window: None,
        }
    }

    #[test]
    fn rectangle_intersection_uses_physical_overlap() {
        let display = SimpleRect {
            left: 0,
            top: 0,
            right: 1920,
            bottom: 1080,
        };
        let window = SimpleRect {
            left: 1900,
            top: 100,
            right: 2100,
            bottom: 300,
        };

        let intersection = display.intersect(window);
        assert_eq!(intersection.width(), 20);
        assert_eq!(intersection.height(), 200);
    }

    #[test]
    fn pending_auto_dim_turns_into_restore_when_a_window_arrives() {
        let operation = PendingBrightnessOperation::Dim {
            brightness: 0,
            next_state: DisplayDimmingState::AutoDimmed,
        };

        assert_eq!(
            reconcile_pending_operation(operation, true, 0),
            PendingBrightnessOperation::Restore {
                next_state: DisplayDimmingState::Lit,
            }
        );
    }

    #[test]
    fn pending_auto_restore_turns_back_into_dim_when_window_leaves() {
        let operation = PendingBrightnessOperation::Restore {
            next_state: DisplayDimmingState::Lit,
        };

        assert_eq!(
            reconcile_pending_operation(operation, false, 10),
            PendingBrightnessOperation::Dim {
                brightness: 10,
                next_state: DisplayDimmingState::AutoDimmed,
            }
        );
    }

    #[test]
    fn pending_manual_brightness_tracks_current_occupancy() {
        let operation = PendingBrightnessOperation::Set {
            brightness: 50,
            next_state: DisplayDimmingState::UserRestoredWhileEmpty,
        };

        assert_eq!(
            reconcile_pending_operation(operation, true, 0),
            PendingBrightnessOperation::Set {
                brightness: 50,
                next_state: DisplayDimmingState::Lit,
            }
        );
    }

    #[test]
    fn failed_brightness_operation_remains_pending() {
        let operation = PendingBrightnessOperation::Restore {
            next_state: DisplayDimmingState::Lit,
        };
        let mut display = test_display(
            "test",
            vec![test_target("test", "test", 0, Some(50), true)],
            DisplayDimmingState::AutoDimmed,
            Some(operation),
        );

        assert!(!apply_pending_operation(&mut display));
        assert_eq!(display.dimming_state, DisplayDimmingState::AutoDimmed);
        assert_eq!(display.pending_operation, Some(operation));
    }

    #[test]
    fn accessibility_child_events_are_ignored() {
        assert!(is_relevant_window_event(
            EVENT_OBJECT_SHOW,
            true,
            OBJID_WINDOW.0,
            CHILDID_SELF as i32,
        ));
        assert!(!is_relevant_window_event(
            EVENT_OBJECT_SHOW,
            true,
            OBJID_WINDOW.0,
            1,
        ));
        assert!(!is_relevant_window_event(
            EVENT_OBJECT_SHOW,
            true,
            OBJID_WINDOW.0 + 1,
            CHILDID_SELF as i32,
        ));
    }

    #[test]
    fn system_events_do_not_require_a_window_handle() {
        assert!(is_relevant_window_event(
            EVENT_SYSTEM_DESKTOPSWITCH,
            false,
            0,
            0,
        ));
    }

    #[test]
    fn brightness_percent_is_scaled_to_the_monitor_range() {
        assert_eq!(percent_to_raw(20, 220, 0), 20);
        assert_eq!(percent_to_raw(20, 220, 25), 70);
        assert_eq!(percent_to_raw(20, 220, 50), 120);
        assert_eq!(percent_to_raw(20, 220, 100), 220);
        assert_eq!(percent_to_raw(20, 220, 150), 220);
        assert_eq!(percent_to_raw(80, 80, 50), 80);
    }

    #[test]
    fn failed_target_is_only_disabled_after_repeated_failures() {
        let mut target = test_target("display", "monitor", 0, None, false);
        target.availability = TargetAvailability::Unknown;

        assert!(!target.record_failure());
        assert_eq!(
            target.availability,
            TargetAvailability::TemporarilyUnavailable
        );
        assert!(!target.record_failure());
        assert!(target.record_failure());
        assert_eq!(target.availability, TargetAvailability::Unavailable);
    }

    #[test]
    fn reenumeration_preserves_restore_brightness_and_manual_state() {
        let old_display = test_display(
            "display",
            vec![test_target("display", "monitor", 0, Some(73), true)],
            DisplayDimmingState::UserDimmed,
            None,
        );
        let mut snapshots = capture_display_snapshots(&[old_display]);
        let mut new_displays = vec![test_display(
            "display",
            vec![test_target("display", "monitor", 0, None, false)],
            DisplayDimmingState::Lit,
            None,
        )];

        restore_display_snapshots(&mut new_displays, &mut snapshots);

        assert!(snapshots.is_empty());
        assert_eq!(
            new_displays[0].dimming_state,
            DisplayDimmingState::UserDimmed
        );
        assert_eq!(new_displays[0].targets[0].restore_brightness, Some(73));
        assert!(!new_displays[0].targets[0].is_dimmed);
    }

    #[test]
    fn reenumeration_preserves_pending_operation() {
        let operation = PendingBrightnessOperation::Restore {
            next_state: DisplayDimmingState::Lit,
        };
        let old_display = test_display(
            "display",
            vec![test_target("display", "monitor", 0, Some(73), true)],
            DisplayDimmingState::AutoDimmed,
            Some(operation),
        );
        let mut snapshots = capture_display_snapshots(&[old_display]);
        let mut new_displays = vec![test_display(
            "display",
            vec![test_target("display", "monitor", 0, None, false)],
            DisplayDimmingState::Lit,
            None,
        )];

        restore_display_snapshots(&mut new_displays, &mut snapshots);

        assert_eq!(new_displays[0].pending_operation, Some(operation));
        assert!(new_displays[0].targets[0].is_dimmed);
    }

    #[test]
    fn manual_dimming_check_state_tracks_user_intent_even_when_ddc_fails() {
        let mut manager = MonitorManager {
            settings: AppSettings::default(),
            displays: vec![test_display(
                "display",
                vec![test_target("display", "monitor", 0, Some(73), false)],
                DisplayDimmingState::Lit,
                None,
            )],
            detached_snapshots: HashMap::new(),
            manual_dimming_enabled: false,
        };

        manager.dim_all();

        assert!(manager.manual_dimming_enabled());
        assert!(manager.displays[0].pending_operation.is_some());
        assert!(!manager.displays[0].targets[0].is_dimmed);

        manager.restore_all_from_user(&[]);

        assert!(!manager.manual_dimming_enabled());
    }

    #[test]
    fn empty_window_title_uses_a_diagnostic_placeholder() {
        assert_eq!(window_title_or_placeholder(String::new()), "(untitled)");
        assert_eq!(window_title_or_placeholder("   ".to_string()), "(untitled)");
        assert_eq!(
            window_title_or_placeholder("Settings".to_string()),
            "Settings"
        );
    }
}

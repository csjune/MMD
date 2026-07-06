#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
#![allow(unsafe_op_in_unsafe_fn)]

use std::cell::RefCell;
use std::collections::HashSet;
use std::ffi::c_void;
use std::mem::size_of;
use std::path::Path;
use std::ptr::{addr_of, addr_of_mut};
use std::sync::atomic::{AtomicI64, AtomicIsize, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::SystemTime;

use windows::Win32::Devices::Display::{
    DestroyPhysicalMonitor, GetMonitorBrightness, GetNumberOfPhysicalMonitorsFromHMONITOR,
    GetPhysicalMonitorsFromHMONITOR, PHYSICAL_MONITOR, SetMonitorBrightness,
};
use windows::Win32::Foundation::{CloseHandle, HANDLE, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::{DWMWA_CLOAKED, DwmGetWindowAttribute};
use windows::Win32::Graphics::Gdi::{
    EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFOEXW,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::{
    OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};
use windows::Win32::UI::Accessibility::{
    HWINEVENTHOOK, SetWinEventHook, UnhookWinEvent,
};
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext,
};
use windows::Win32::UI::Shell::{
    NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY, NOTIFYICONDATAW,
    Shell_NotifyIconW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CW_USEDEFAULT, CreatePopupMenu, CreateWindowExW, DefWindowProcW,
    DestroyIcon, DestroyMenu, DestroyWindow, DispatchMessageW, EnumWindows, GWLP_USERDATA,
    GWL_EXSTYLE, GetClassNameW, GetCursorPos, GetMessageW, GetShellWindow, GetWindowLongPtrW,
    GetWindowRect, GetWindowTextLengthW, GetWindowTextW, GetWindowThreadProcessId, HMENU,
    HICON, IDI_APPLICATION, IMAGE_ICON, IsIconic, IsWindowVisible, KillTimer, LR_DEFAULTSIZE,
    LoadIconW, LoadImageW, MB_ICONINFORMATION, MB_OK, MF_SEPARATOR, MF_STRING, MSG, MessageBoxW,
    PostMessageW, PostQuitMessage, RegisterClassExW, SW_HIDE, SetForegroundWindow, SetTimer,
    SetWindowLongPtrW, ShowWindow, TPM_RIGHTBUTTON, TrackPopupMenu, TranslateMessage,
    WINDOW_EX_STYLE, WM_APP, WM_COMMAND, WM_CONTEXTMENU, WM_DESTROY, WM_LBUTTONUP, WM_NCCREATE,
    WM_RBUTTONUP, WM_TIMER, WNDCLASSEXW, WS_OVERLAPPEDWINDOW, WINEVENT_OUTOFCONTEXT,
    WINEVENT_SKIPOWNPROCESS, EVENT_OBJECT_CLOAKED, EVENT_OBJECT_CREATE,
    EVENT_OBJECT_LOCATIONCHANGE, EVENT_OBJECT_UNCLOAKED, EVENT_SYSTEM_DESKTOPSWITCH,
    EVENT_SYSTEM_FOREGROUND, EVENT_SYSTEM_MINIMIZEEND, EVENT_SYSTEM_MINIMIZESTART,
};
use windows::core::{w, BOOL, Error, PCWSTR, PWSTR};

const APP_NAME: &str = "MMD";
const CLASS_NAME: PCWSTR = w!("MmdRustWindow");
const APP_ICON_RESOURCE_ID: PCWSTR = PCWSTR(1u16 as _);
const WM_TRAYICON: u32 = WM_APP + 1;
const WM_WINDOW_EVENT: u32 = WM_APP + 2;
const TIMER_DEBOUNCE: usize = 1;
const TRAY_UID: u32 = 1;
const WS_EX_TOOLWINDOW: isize = 0x00000080;

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

fn main() -> windows::core::Result<()> {
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
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
    match message {
        WM_TRAYICON => {
            let tray_event = low_word(lparam.0);
            if let Ok(mut state) = state.try_borrow_mut() {
                match tray_event {
                    WM_LBUTTONUP => state.toggle_tray_brightness(),
                    WM_RBUTTONUP | WM_CONTEXTMENU => state.show_context_menu(),
                    _ => {}
                }
            }
            LRESULT(0)
        }
        WM_COMMAND => {
            if let Ok(mut state) = state.try_borrow_mut() {
                state.handle_command(wparam.0 & 0xffff);
            }
            LRESULT(0)
        }
        WM_WINDOW_EVENT => {
            if let Ok(mut state) = state.try_borrow_mut() {
                state.schedule_update();
            }
            LRESULT(0)
        }
        WM_TIMER => {
            if wparam.0 == TIMER_DEBOUNCE {
                if let Ok(mut state) = state.try_borrow_mut() {
                    state.update_scheduled = false;
                    unsafe {
                        let _ = KillTimer(Some(hwnd), TIMER_DEBOUNCE);
                    }
                    state.update_brightness_state();
                }
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, message, wparam, lparam),
    }
}

#[derive(Clone)]
struct AppSettings {
    empty_brightness: u32,
    minimum_overlap_pixels: i32,
    minimum_overlap_width: i32,
    minimum_overlap_height: i32,
    event_debounce_milliseconds: u32,
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
            last_error: None,
        };

        state.update_brightness_state();
        Ok(state)
    }

    fn handle_command(&mut self, command_id: usize) {
        match command_id {
            ID_BRIGHTNESS_0 => self.set_brightness_now(0),
            ID_BRIGHTNESS_10 => self.set_brightness_now(10),
            ID_BRIGHTNESS_25 => self.set_brightness_now(25),
            ID_BRIGHTNESS_50 => self.set_brightness_now(50),
            ID_BRIGHTNESS_75 => self.set_brightness_now(75),
            ID_BRIGHTNESS_100 => self.set_brightness_now(100),
            ID_TOGGLE_DIMMING => self.toggle_tray_brightness(),
            ID_REFRESH => self.refresh_now(),
            ID_DIAGNOSTICS => self.show_diagnostics(),
            ID_EXIT => unsafe {
                let _ = DestroyWindow(self.hwnd);
            },
            _ => {}
        }
    }

    fn toggle_tray_brightness(&mut self) {
        self.cancel_scheduled_update();

        if self.monitor_manager.all_controllable_monitors_dimmed() {
            let windows = self.window_tracker.get_tracked_windows();
            self.monitor_manager.restore_all_from_user(&windows);
        } else {
            self.monitor_manager.dim_all();
        }

        self.update_tray_text();
    }

    fn set_brightness_now(&mut self, brightness: u32) {
        let windows = self.window_tracker.get_tracked_windows();
        self.monitor_manager.set_all_brightness(brightness, &windows);
        self.update_tray_text();
    }

    fn schedule_update(&mut self) {
        if self.update_scheduled {
            return;
        }

        self.update_scheduled = true;
        unsafe {
            let _ = KillTimer(Some(self.hwnd), TIMER_DEBOUNCE);
            SetTimer(
                Some(self.hwnd),
                TIMER_DEBOUNCE,
                self.settings.event_debounce_milliseconds.max(1),
                None,
            );
        }
    }

    fn cancel_scheduled_update(&mut self) {
        self.update_scheduled = false;
        unsafe {
            let _ = KillTimer(Some(self.hwnd), TIMER_DEBOUNCE);
        }
    }

    fn refresh_now(&mut self) {
        self.cancel_scheduled_update();
        self.update_brightness_state();
    }

    fn update_brightness_state(&mut self) {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let windows = self.window_tracker.get_tracked_windows();
            self.monitor_manager.update(&windows);
        }));

        self.last_error = result.err().map(|_| "Unexpected update error".to_string());
        self.update_tray_text();
    }

    fn update_tray_text(&mut self) {
        let text = if let Some(error) = &self.last_error {
            format!("{APP_NAME} - Error: {error}")
        } else {
            format!(
                "{APP_NAME} - {}/{} dimmed",
                self.monitor_manager.dimmed_display_count(),
                self.monitor_manager.controllable_display_count()
            )
        };

        self.tray_icon.set_tooltip(&text);
    }

    fn show_context_menu(&mut self) {
        unsafe {
            let Ok(menu) = CreatePopupMenu() else {
                return;
            };

            let toggle_label = if self.monitor_manager.all_controllable_monitors_dimmed() {
                "Undimming"
            } else {
                "Dimming"
            };
            append_menu_string(menu, ID_TOGGLE_DIMMING, toggle_label);
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

            let mut point = POINT::default();
            if GetCursorPos(&mut point).is_ok() {
                let _ = SetForegroundWindow(self.hwnd);
                let _ = TrackPopupMenu(
                    menu,
                    TPM_RIGHTBUTTON,
                    point.x,
                    point.y,
                    Some(0),
                    self.hwnd,
                    None,
                );
            }

            let _ = DestroyMenu(menu);
        }
    }

    fn show_diagnostics(&self) {
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

        let text = wide_null(&text);
        unsafe {
            MessageBoxW(
                Some(self.hwnd),
                PCWSTR(text.as_ptr()),
                w!("MMD Diagnostics"),
                MB_OK | MB_ICONINFORMATION,
            );
        }
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
}

impl TrayIcon {
    fn new(hwnd: HWND) -> windows::core::Result<Self> {
        let icon = load_app_icon();
        let mut tray_icon = Self { hwnd, icon };
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
            if !Shell_NotifyIconW(NIM_ADD, &mut data).as_bool() {
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
            let _ = Shell_NotifyIconW(NIM_MODIFY, &mut data);
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
        let mut data = self.data();
        unsafe {
            let _ = Shell_NotifyIconW(NIM_DELETE, &mut data);
            if !self.icon.0.is_null() {
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
    _id_object: i32,
    _id_child: i32,
    _event_thread: u32,
    _event_time: u32,
) {
    if event_type >= EVENT_OBJECT_CREATE && hwnd.0.is_null() {
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

struct MonitorManager {
    settings: AppSettings,
    displays: Vec<DisplayState>,
}

impl MonitorManager {
    fn new(settings: AppSettings) -> Self {
        let mut manager = Self {
            settings,
            displays: Vec::new(),
        };
        manager.enumerate_displays();
        manager
    }

    fn controllable_display_count(&self) -> usize {
        self.displays
            .iter()
            .filter(|display| !display.targets.is_empty())
            .count()
    }

    fn dimmed_display_count(&self) -> usize {
        self.displays
            .iter()
            .filter(|display| display.targets.iter().any(|target| target.is_dimmed))
            .count()
    }

    fn all_controllable_monitors_dimmed(&self) -> bool {
        let mut has_controllable_monitor = false;

        for display in &self.displays {
            for target in &display.targets {
                has_controllable_monitor = true;
                if !target.is_dimmed {
                    return false;
                }
            }
        }

        has_controllable_monitor
    }

    fn update(&mut self, windows: &[TrackedWindow]) {
        for display in &mut self.displays {
            let (has_window, blocking_window) =
                try_find_blocking_window(&self.settings, display, windows);
            display.last_blocking_window = blocking_window;
            update_display_state(&self.settings, display, has_window);
        }
    }

    fn restore_all(&mut self) {
        for display in &mut self.displays {
            restore(display, DisplayDimmingState::Lit);
        }
    }

    fn restore_all_from_user(&mut self, windows: &[TrackedWindow]) {
        for display in &mut self.displays {
            let (has_window, blocking_window) =
                try_find_blocking_window(&self.settings, display, windows);
            display.last_blocking_window = blocking_window;
            restore(
                display,
                if has_window {
                    DisplayDimmingState::Lit
                } else {
                    DisplayDimmingState::UserRestoredWhileEmpty
                },
            );
        }
    }

    fn dim_all(&mut self) {
        for display in &mut self.displays {
            dim(&self.settings, display, DisplayDimmingState::UserDimmed);
            display.last_blocking_window = Some("Tray icon click".to_string());
        }
    }

    fn set_all_brightness(&mut self, brightness: u32, windows: &[TrackedWindow]) {
        for display in &mut self.displays {
            let (has_window, blocking_window) =
                try_find_blocking_window(&self.settings, display, windows);
            for target in &mut display.targets {
                target.set_brightness(brightness);
            }
            display.dimming_state = if has_window {
                DisplayDimmingState::Lit
            } else {
                DisplayDimmingState::UserRestoredWhileEmpty
            };
            display.last_blocking_window =
                blocking_window.or_else(|| Some(format!("Brightness set to {brightness}%")));
        }
    }

    fn build_diagnostic_text(&self) -> String {
        let mut lines = vec![
            "DPI mode: PerMonitorV2".to_string(),
            format!(
                "Controllable displays: {}/{}",
                self.controllable_display_count(),
                self.displays.len()
            ),
            format!("Dimmed displays: {}", self.dimmed_display_count()),
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
            lines.push(format!(
                "  Blocking window: {}",
                display.last_blocking_window.as_deref().unwrap_or("(none)")
            ));
            for target in &display.targets {
                lines.push(format!(
                    "  Monitor: {} dimmed={}",
                    target.description, target.is_dimmed
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

                for physical_monitor in physical_monitors {
                    let handle = addr_of!(physical_monitor.hPhysicalMonitor).read_unaligned();
                    let description =
                        addr_of!(physical_monitor.szPhysicalMonitorDescription).read_unaligned();
                    let target =
                        PhysicalMonitorTarget::new(handle, string_from_wide_slice(&description));
                    if target.try_read_brightness().is_some() {
                        display.targets.push(target);
                    }
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
    last_blocking_window: Option<String>,
}

struct PhysicalMonitorTarget {
    handle: HANDLE,
    description: String,
    restore_brightness: u32,
    is_dimmed: bool,
    disposed: bool,
}

impl PhysicalMonitorTarget {
    fn new(
        handle: HANDLE,
        description: String,
    ) -> Self {
        Self {
            handle,
            description: if description.trim().is_empty() {
                "Unknown monitor".to_string()
            } else {
                description
            },
            restore_brightness: 0,
            is_dimmed: false,
            disposed: false,
        }
    }

    fn dim(&mut self, empty_brightness: u32) {
        if self.disposed {
            return;
        }

        let Some((minimum, current, maximum)) = self.try_read_brightness_full() else {
            return;
        };

        if !self.is_dimmed {
            self.restore_brightness = current;
        }

        let brightness = empty_brightness.clamp(minimum, maximum);
        if self.try_set_brightness(brightness) {
            self.is_dimmed = true;
        }
    }

    fn set_brightness(&mut self, brightness: u32) {
        if self.disposed {
            return;
        }

        let Some((minimum, _, maximum)) = self.try_read_brightness_full() else {
            return;
        };

        let brightness = brightness.clamp(minimum, maximum);
        if self.try_set_brightness(brightness) {
            self.restore_brightness = brightness;
            self.is_dimmed = false;
        }
    }

    fn restore(&mut self) {
        if !self.is_dimmed || self.disposed {
            return;
        }

        if self.try_set_brightness(self.restore_brightness) {
            self.is_dimmed = false;
        }
    }

    fn try_read_brightness(&self) -> Option<u32> {
        self.try_read_brightness_full().map(|(_, current, _)| current)
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
        if title_length <= 0 {
            return None;
        }

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

        let title = get_window_text(hwnd, title_length + 1);
        if title.trim().is_empty() {
            return None;
        }

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
) {
    if display.targets.is_empty() {
        display.dimming_state = DisplayDimmingState::Lit;
        return;
    }

    if has_window {
        match display.dimming_state {
            DisplayDimmingState::AutoDimmed => restore(display, DisplayDimmingState::Lit),
            DisplayDimmingState::UserRestoredWhileEmpty => {
                display.dimming_state = DisplayDimmingState::Lit;
            }
            _ => {}
        }
        return;
    }

    match display.dimming_state {
        DisplayDimmingState::Lit => dim(settings, display, DisplayDimmingState::AutoDimmed),
        DisplayDimmingState::UserDimmed => {
            display.dimming_state = DisplayDimmingState::AutoDimmed;
        }
        DisplayDimmingState::AutoDimmed => {
            if display.targets.iter().any(|target| !target.is_dimmed) {
                dim(settings, display, display.dimming_state);
            }
        }
        DisplayDimmingState::UserRestoredWhileEmpty => {}
    }
}

fn dim(settings: &AppSettings, display: &mut DisplayState, state: DisplayDimmingState) {
    if display.targets.is_empty() {
        display.dimming_state = DisplayDimmingState::Lit;
        return;
    }

    for target in &mut display.targets {
        target.dim(settings.empty_brightness);
    }
    display.dimming_state = state;
}

fn restore(display: &mut DisplayState, state: DisplayDimmingState) {
    if display.targets.is_empty() {
        display.dimming_state = DisplayDimmingState::Lit;
        return;
    }

    if display.targets.iter().all(|target| !target.is_dimmed) {
        display.dimming_state = state;
        return;
    }

    for target in &mut display.targets {
        target.restore();
    }
    display.dimming_state = state;
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
        let ok =
            QueryFullProcessImageNameW(process, PROCESS_NAME_WIN32, PWSTR(buffer.as_mut_ptr()), &mut size);
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

fn load_app_icon() -> HICON {
    unsafe {
        if let Ok(module) = GetModuleHandleW(None) {
            if let Ok(handle) = LoadImageW(
                Some(HINSTANCE(module.0)),
                APP_ICON_RESOURCE_ID,
                IMAGE_ICON,
                0,
                0,
                LR_DEFAULTSIZE,
            ) {
                return HICON(handle.0);
            }
        }

        LoadIconW(None, IDI_APPLICATION).unwrap_or_default()
    }
}

fn write_tip(data: &mut NOTIFYICONDATAW, text: &str) {
    data.szTip.fill(0);
    let mut wide = text.encode_utf16().take(data.szTip.len() - 1).collect::<Vec<_>>();
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
    let len = buffer.iter().position(|value| *value == 0).unwrap_or(buffer.len());
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
        Ok(duration) => format!("unix+{}.{:03}s", duration.as_secs(), duration.subsec_millis()),
        Err(_) => "(unknown time)".to_string(),
    }
}

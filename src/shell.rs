//! The shell thread: one hidden top-level window that owns the tray icon,
//! global hotkeys, the dimmer overlays and gamma, and listens for display,
//! lid, power and session changes.
//!
//! It has to be a real (hidden) top-level window rather than a message-only
//! one, because WM_DISPLAYCHANGE and WM_SETTINGCHANGE are only broadcast to
//! top-level windows.

use std::cell::RefCell;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;

use windows_sys::core::GUID;
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Power::RegisterPowerSettingNotification;
use windows_sys::Win32::System::RemoteDesktop::WTSRegisterSessionNotification;
use windows_sys::Win32::UI::Accessibility::{SetWinEventHook, UnhookWinEvent, HWINEVENTHOOK};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{RegisterHotKey, UnregisterHotKey};
use windows_sys::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_TIP, NIIF_INFO, NIM_ADD, NIM_DELETE,
    NIM_MODIFY, NOTIFYICONDATAW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use crate::app::Core;
use crate::dimmer::Dimmer;
use crate::hotkeys::{self, MOD_NOREPEAT};
use crate::win::{self, wide, Hwnd};

pub const SHELL_CLASS: &str = "TransparencyAppShell";
pub const WM_SHELL_CMD: u32 = WM_APP + 1;
const WM_TRAY: u32 = WM_APP + 2;
pub const WM_SHOW_MAIN: u32 = WM_APP + 3;

const TIMER_TICK: usize = 1;
const TIMER_RESYNC_1: usize = 2;
const TIMER_RESYNC_2: usize = 3;
const TIMER_RESYNC_3: usize = 4;
const TIMER_CAPTURE: usize = 5;
const TIMER_MONITORS_UI: usize = 6;

const MENU_SHOW: usize = 100;
const MENU_PAUSE: usize = 101;
const MENU_FOCUS: usize = 102;
const MENU_DIMMER: usize = 103;
const MENU_RESTORE: usize = 104;
const MENU_INSTALL: usize = 105;
const MENU_QUIT: usize = 106;

const GUID_LIDSWITCH_STATE_CHANGE: GUID = GUID::from_u128(0xba3e0f4d_b817_4094_a2d1_d56379e6a0f3);
const GUID_CONSOLE_DISPLAY_STATE: GUID = GUID::from_u128(0x6fe69556_704a_47a0_8f24_c28d936fda47);
const PBT_APMRESUMESUSPEND: usize = 0x7;
const PBT_APMRESUMEAUTOMATIC: usize = 0x12;
const PBT_POWERSETTINGCHANGE: usize = 0x8013;

pub enum ShellCmd {
    SyncDimmer,
    RegisterHotkeys,
    RefreshTray,
    Notify(String, String),
    Quit,
}

#[derive(Clone)]
pub struct ShellHandle {
    hwnd: Hwnd,
    tx: Sender<ShellCmd>,
}

impl ShellHandle {
    pub fn send(&self, cmd: ShellCmd) {
        if self.tx.send(cmd).is_ok() {
            unsafe { PostMessageW(win::h(self.hwnd), WM_SHELL_CMD, 0, 0) };
        }
    }
}

struct State {
    core: Arc<Core>,
    rx: Receiver<ShellCmd>,
    hwnd: Hwnd,
    dimmer: Dimmer,
    icon: HICON,
    tray_added: bool,
    taskbar_created: u32,
    hotkeys: Vec<i32>,
    capture_hooks: Vec<HWINEVENTHOOK>,
    tick_running: bool,
}

thread_local! {
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
}

fn with_state<R>(f: impl FnOnce(&mut State) -> R) -> Option<R> {
    STATE.with(|s| s.try_borrow_mut().ok().and_then(|mut g| g.as_mut().map(f)))
}

/// Start the shell thread. Blocks until its window exists.
pub fn start(core: Arc<Core>) -> std::thread::JoinHandle<()> {
    let (ready_tx, ready_rx) = mpsc::channel::<ShellHandle>();
    let handle = std::thread::Builder::new()
        .name("Shell".into())
        .spawn(move || run(core, ready_tx))
        .expect("spawn shell");
    if let Ok(h) = ready_rx.recv() {
        let _ = h; // stored by run() into core.shell
    }
    handle
}

/// Find an already running instance's shell window.
pub fn find_existing() -> Option<Hwnd> {
    let class = wide(SHELL_CLASS);
    let hwnd = unsafe { FindWindowW(class.as_ptr(), std::ptr::null()) };
    (!hwnd.is_null()).then_some(hwnd as Hwnd)
}

fn run(core: Arc<Core>, ready: Sender<ShellHandle>) {
    let class = wide(SHELL_CLASS);
    let hinstance = unsafe { GetModuleHandleW(std::ptr::null()) };
    unsafe {
        let wc = WNDCLASSW {
            style: 0,
            lpfnWndProc: Some(wnd_proc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: hinstance,
            hIcon: std::ptr::null_mut(),
            hCursor: std::ptr::null_mut(),
            hbrBackground: std::ptr::null_mut(),
            lpszMenuName: std::ptr::null(),
            lpszClassName: class.as_ptr(),
        };
        RegisterClassW(&wc);
    }
    let title = wide("Transparency App");
    let hwnd = unsafe {
        CreateWindowExW(
            WS_EX_TOOLWINDOW,
            class.as_ptr(),
            title.as_ptr(),
            WS_POPUP,
            0,
            0,
            0,
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            hinstance,
            std::ptr::null(),
        )
    } as Hwnd;

    let (tx, rx) = mpsc::channel();
    let handle = ShellHandle { hwnd, tx };
    let _ = core.shell.set(handle.clone());

    let icon = unsafe {
        LoadImageW(
            hinstance,
            1 as *const u16, // MAKEINTRESOURCE(1): the app icon
            IMAGE_ICON,
            GetSystemMetrics(SM_CXSMICON),
            GetSystemMetrics(SM_CYSMICON),
            LR_DEFAULTCOLOR,
        )
    } as HICON;
    let taskbar_created = unsafe { RegisterWindowMessageW(wide("TaskbarCreated").as_ptr()) };

    unsafe {
        RegisterPowerSettingNotification(hwnd as _, &GUID_LIDSWITCH_STATE_CHANGE, 0);
        RegisterPowerSettingNotification(hwnd as _, &GUID_CONSOLE_DISPLAY_STATE, 0);
        WTSRegisterSessionNotification(win::h(hwnd), 0);
    }

    let dimmer = Dimmer::new();
    STATE.with(|s| {
        *s.borrow_mut() = Some(State {
            core: core.clone(),
            rx,
            hwnd,
            dimmer,
            icon,
            tray_added: false,
            taskbar_created,
            hotkeys: vec![],
            capture_hooks: vec![],
            tick_running: false,
        })
    });
    with_state(|st| {
        st.add_tray();
        st.register_hotkeys();
        st.sync(true);
    });
    let _ = ready.send(handle);

    unsafe {
        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    with_state(|st| st.teardown());
    STATE.with(|s| s.borrow_mut().take());
}

unsafe extern "system" fn capture_event(
    _hook: HWINEVENTHOOK,
    _event: u32,
    _hwnd: HWND,
    _obj: i32,
    _child: i32,
    _thread: u32,
    _time: u32,
) {
    with_state(|st| {
        SetTimer(win::h(st.hwnd), TIMER_CAPTURE, 60, None);
    });
}

impl State {
    fn add_tray(&mut self) {
        unsafe {
            let mut nid = self.nid();
            nid.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
            nid.uCallbackMessage = WM_TRAY;
            nid.hIcon = self.icon;
            copy_wide(&mut nid.szTip, "Transparency App");
            self.tray_added = Shell_NotifyIconW(NIM_ADD, &nid) != 0;
        }
    }

    fn nid(&self) -> NOTIFYICONDATAW {
        let mut nid: NOTIFYICONDATAW = unsafe { std::mem::zeroed() };
        nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        nid.hWnd = win::h(self.hwnd);
        nid.uID = 1;
        nid
    }

    fn notify(&self, title: &str, body: &str) {
        unsafe {
            let mut nid = self.nid();
            nid.uFlags = NIF_INFO;
            copy_wide(&mut nid.szInfoTitle, title);
            copy_wide(&mut nid.szInfo, body);
            nid.dwInfoFlags = NIIF_INFO;
            Shell_NotifyIconW(NIM_MODIFY, &nid);
        }
    }

    fn register_hotkeys(&mut self) {
        for id in self.hotkeys.drain(..) {
            unsafe { UnregisterHotKey(win::h(self.hwnd), id) };
        }
        let settings = self.core.config.settings();
        let mut failed = vec![];
        if settings.hotkeys_enabled && !self.core.capturing.load(std::sync::atomic::Ordering::SeqCst) {
            for (key, _label, id, default, _action) in hotkeys::DEFS {
                let combo = settings
                    .hotkeys
                    .get(*key)
                    .filter(|c| hotkeys::parse(c).is_some())
                    .map(String::as_str)
                    .unwrap_or(default);
                let Some((mods, vk)) = hotkeys::parse(combo) else { continue };
                if unsafe { RegisterHotKey(win::h(self.hwnd), *id, mods | MOD_NOREPEAT, vk) } != 0 {
                    self.hotkeys.push(*id);
                } else {
                    failed.push(key.to_string());
                }
            }
            if !failed.is_empty() {
                crate::log::warn(format!("hotkeys unavailable (in use by another app): {failed:?}"));
            }
        }
        *self.core.hotkey_failures.lock().unwrap() = failed;
    }

    fn sync(&mut self, repair: bool) {
        let settings = self.core.config.settings();
        self.dimmer.sync(&settings, repair);
        *self.core.dimmer_modes.lock().unwrap() = self.dimmer.modes.clone();

        // Keep a slow tick while dimming (keeps overlays on top and follows
        // screen changes) or while a gamma restore is still owed.
        let want_tick = self.dimmer.enabled() || self.dimmer.has_owed();
        unsafe {
            if want_tick && !self.tick_running {
                SetTimer(win::h(self.hwnd), TIMER_TICK, 2000, None);
                self.tick_running = true;
            } else if !want_tick && self.tick_running {
                KillTimer(win::h(self.hwnd), TIMER_TICK);
                self.tick_running = false;
            }
        }
        self.update_capture_hook();
    }

    fn update_capture_hook(&mut self) {
        let want = self.dimmer.enabled() && self.dimmer.has_overlays();
        if want && self.capture_hooks.is_empty() {
            for ev in [
                crate::engine::EVENT_SYSTEM_FOREGROUND,
                0x8002, /* EVENT_OBJECT_SHOW */
                0x8003, /* EVENT_OBJECT_HIDE */
            ] {
                let h = unsafe {
                    SetWinEventHook(ev, ev, std::ptr::null_mut(), Some(capture_event), 0, 0, 0x2 /* skip own */)
                };
                if !h.is_null() {
                    self.capture_hooks.push(h);
                }
            }
            self.dimmer.set_capture_active(crate::dimmer::capture_ui_active());
        } else if !want && !self.capture_hooks.is_empty() {
            for h in self.capture_hooks.drain(..) {
                unsafe { UnhookWinEvent(h) };
            }
            self.dimmer.set_capture_active(false);
        }
    }

    /// Screens changed (lid, plug, resume, unlock): resync now and a few
    /// more times while the driver settles.
    fn screens_changed(&mut self) {
        self.dimmer.display_changed();
        unsafe {
            let h = win::h(self.hwnd);
            SetTimer(h, TIMER_RESYNC_1, 300, None);
            SetTimer(h, TIMER_RESYNC_2, 1500, None);
            SetTimer(h, TIMER_RESYNC_3, 4000, None);
            SetTimer(h, TIMER_MONITORS_UI, 700, None);
        }
    }

    fn drain(&mut self) -> bool {
        while let Ok(cmd) = self.rx.try_recv() {
            match cmd {
                ShellCmd::SyncDimmer => self.sync(false),
                ShellCmd::RegisterHotkeys => self.register_hotkeys(),
                ShellCmd::RefreshTray => {}
                ShellCmd::Notify(t, b) => self.notify(&t, &b),
                ShellCmd::Quit => return false,
            }
        }
        true
    }

    fn show_menu(&mut self) {
        let settings = self.core.config.settings();
        let ready = self.core.update.lock().unwrap().state == "ready";
        unsafe {
            let menu = CreatePopupMenu();
            let add = |id: usize, text: &str, checked: bool| {
                let t = wide(text);
                let flags = MF_STRING | if checked { MF_CHECKED } else { MF_UNCHECKED };
                AppendMenuW(menu, flags, id, t.as_ptr());
            };
            add(MENU_SHOW, "Open Transparency App", false);
            SetMenuDefaultItem(menu, MENU_SHOW as u32, 0);
            AppendMenuW(menu, MF_SEPARATOR, 0, std::ptr::null());
            add(MENU_PAUSE, "Transparency rules", !self.core.engine.paused());
            add(MENU_FOCUS, "Focus mode", self.core.engine.focus_mode());
            add(MENU_DIMMER, "Screen dimmer", settings.dimmer_enabled);
            add(MENU_RESTORE, "Restore all windows", false);
            if ready {
                AppendMenuW(menu, MF_SEPARATOR, 0, std::ptr::null());
                add(MENU_INSTALL, "Install downloaded update", false);
            }
            AppendMenuW(menu, MF_SEPARATOR, 0, std::ptr::null());
            add(MENU_QUIT, "Quit", false);

            let mut pt = POINT { x: 0, y: 0 };
            GetCursorPos(&mut pt);
            SetForegroundWindow(win::h(self.hwnd));
            let cmd = TrackPopupMenu(
                menu,
                TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_BOTTOMALIGN,
                pt.x,
                pt.y,
                0,
                win::h(self.hwnd),
                std::ptr::null(),
            ) as usize;
            PostMessageW(win::h(self.hwnd), WM_NULL, 0, 0);
            DestroyMenu(menu);
            let core = self.core.clone();
            match cmd {
                MENU_SHOW => core.show_window(),
                MENU_PAUSE => core.set_paused(!core.engine.paused()),
                MENU_FOCUS => core.set_focus(!core.engine.focus_mode()),
                MENU_DIMMER => {
                    core.config.update_quiet(|d| d.settings.dimmer_enabled = !settings.dimmer_enabled);
                    self.sync(false);
                    core.refresh_ui();
                }
                MENU_RESTORE => core.restore_all(),
                MENU_INSTALL => {
                    let c = core.clone();
                    let _ = slint::invoke_from_event_loop(move || c.install_update());
                }
                MENU_QUIT => core.quit(),
                _ => {}
            }
        }
    }

    fn teardown(&mut self) {
        for h in self.capture_hooks.drain(..) {
            unsafe { UnhookWinEvent(h) };
        }
        for id in self.hotkeys.drain(..) {
            unsafe { UnregisterHotKey(win::h(self.hwnd), id) };
        }
        self.dimmer.shutdown();
        if self.tray_added {
            let nid = self.nid();
            unsafe { Shell_NotifyIconW(NIM_DELETE, &nid) };
            self.tray_added = false;
        }
    }
}

fn copy_wide(dst: &mut [u16], s: &str) {
    let src: Vec<u16> = s.encode_utf16().take(dst.len() - 1).collect();
    dst[..src.len()].copy_from_slice(&src);
    dst[src.len()] = 0;
}

unsafe extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_SHELL_CMD => {
            if with_state(|st| st.drain()) == Some(false) {
                DestroyWindow(hwnd);
            }
            0
        }
        WM_TRAY => {
            let event = (lp as u32) & 0xFFFF;
            match event {
                WM_LBUTTONUP | WM_LBUTTONDBLCLK => {
                    let core = with_state(|st| st.core.clone());
                    if let Some(c) = core {
                        c.show_window();
                    }
                }
                WM_RBUTTONUP | WM_CONTEXTMENU => {
                    with_state(|st| st.show_menu());
                }
                _ => {}
            }
            0
        }
        WM_SHOW_MAIN => {
            if let Some(c) = with_state(|st| st.core.clone()) {
                c.show_window();
            }
            0
        }
        WM_HOTKEY => {
            let id = wp as i32;
            if let Some(core) = with_state(|st| st.core.clone()) {
                if let Some((_, _, _, _, action)) = hotkeys::DEFS.iter().find(|d| d.2 == id) {
                    core.run_action(*action);
                }
            }
            0
        }
        WM_DISPLAYCHANGE => {
            with_state(|st| st.screens_changed());
            DefWindowProcW(hwnd, msg, wp, lp)
        }
        WM_SETTINGCHANGE => {
            // Taskbar auto-hide and work area changes move the overlay gap.
            with_state(|st| {
                if st.dimmer.enabled() {
                    SetTimer(win::h(st.hwnd), TIMER_RESYNC_1, 300, None);
                }
            });
            DefWindowProcW(hwnd, msg, wp, lp)
        }
        WM_POWERBROADCAST => {
            if matches!(wp, PBT_APMRESUMEAUTOMATIC | PBT_APMRESUMESUSPEND | PBT_POWERSETTINGCHANGE) {
                with_state(|st| st.screens_changed());
            }
            1
        }
        WM_WTSSESSION_CHANGE => {
            // Unlock and console reconnect can reload display calibration.
            if wp == 0x8 /* WTS_SESSION_UNLOCK */ || wp == 0x1 /* CONSOLE_CONNECT */ {
                with_state(|st| st.screens_changed());
            }
            0
        }
        WM_TIMER => {
            let id = wp;
            with_state(|st| match id {
                TIMER_TICK => {
                    if st.dimmer.enabled() && st.dimmer.has_overlays() {
                        st.dimmer.set_capture_active(crate::dimmer::capture_ui_active());
                    }
                    st.sync(false);
                }
                TIMER_RESYNC_1 | TIMER_RESYNC_2 | TIMER_RESYNC_3 => {
                    KillTimer(hwnd, id);
                    st.sync(true);
                    st.core.refresh_monitors();
                }
                TIMER_CAPTURE => {
                    KillTimer(hwnd, id);
                    st.dimmer.set_capture_active(crate::dimmer::capture_ui_active());
                }
                TIMER_MONITORS_UI => {
                    KillTimer(hwnd, id);
                    st.core.refresh_monitors();
                }
                _ => {}
            });
            0
        }
        WM_QUERYENDSESSION => 1,
        WM_ENDSESSION => {
            if wp != 0 {
                // Windows is signing out or shutting down: put gamma back now.
                with_state(|st| st.dimmer.shutdown());
            }
            0
        }
        WM_DESTROY => {
            PostQuitMessage(0);
            0
        }
        _ => {
            let reattach = with_state(|st| st.taskbar_created != 0 && msg == st.taskbar_created).unwrap_or(false);
            if reattach {
                with_state(|st| st.add_tray());
                return 0;
            }
            DefWindowProcW(hwnd, msg, wp, lp)
        }
    }
}

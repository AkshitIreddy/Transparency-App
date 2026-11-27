//! Thin wrappers around the Win32 calls the app needs.
//!
//! Window handles are passed around as `isize` so they can cross threads
//! freely; every call here is safe to make from any thread.

use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::Mutex;

use windows_sys::Win32::Devices::Display::{
    DisplayConfigGetDeviceInfo, GetDisplayConfigBufferSizes, QueryDisplayConfig,
    DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME, DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME,
    DISPLAYCONFIG_MODE_INFO, DISPLAYCONFIG_PATH_INFO, DISPLAYCONFIG_SOURCE_DEVICE_NAME,
    DISPLAYCONFIG_TARGET_DEVICE_NAME,
};
use windows_sys::Win32::Foundation::{CloseHandle, BOOL, HWND, LPARAM, RECT};
use windows_sys::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED};
use windows_sys::Win32::Graphics::Gdi::{
    EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFO, MONITORINFOEXW,
};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcessId, OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
    PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

pub type Hwnd = isize;

#[inline]
pub fn h(hwnd: Hwnd) -> HWND {
    hwnd as HWND
}

pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

pub fn from_wide(buf: &[u16]) -> String {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}

pub const EX_LAYERED: u32 = WS_EX_LAYERED;
pub const EX_TRANSPARENT: u32 = WS_EX_TRANSPARENT;
pub const EX_TOPMOST: u32 = WS_EX_TOPMOST;

#[derive(Clone, Debug, Default)]
pub struct WindowInfo {
    pub hwnd: Hwnd,
    pub title: String,
    pub class_name: String,
    pub pid: u32,
    /// Executable base name, lowercase, e.g. "code.exe".
    pub process: String,
}

pub fn is_window(hwnd: Hwnd) -> bool {
    hwnd != 0 && unsafe { IsWindow(h(hwnd)) != 0 }
}

pub fn window_title(hwnd: Hwnd) -> String {
    let mut buf = [0u16; 512];
    let n = unsafe { GetWindowTextW(h(hwnd), buf.as_mut_ptr(), buf.len() as i32) };
    if n <= 0 {
        return String::new();
    }
    String::from_utf16_lossy(&buf[..n as usize])
}

pub fn window_class(hwnd: Hwnd) -> String {
    let mut buf = [0u16; 256];
    let n = unsafe { GetClassNameW(h(hwnd), buf.as_mut_ptr(), buf.len() as i32) };
    if n <= 0 {
        return String::new();
    }
    String::from_utf16_lossy(&buf[..n as usize])
}

pub fn window_pid(hwnd: Hwnd) -> u32 {
    let mut pid = 0u32;
    unsafe { GetWindowThreadProcessId(h(hwnd), &mut pid) };
    pid
}

pub fn own_pid() -> u32 {
    unsafe { GetCurrentProcessId() }
}

static PROCESS_CACHE: Mutex<Option<HashMap<u32, String>>> = Mutex::new(None);

/// Executable base name for a pid, lowercase. Cached (pids are reused, so the
/// cache is kept small and cleared when it grows).
pub fn process_name(pid: u32) -> String {
    if pid == 0 {
        return String::new();
    }
    if let Some(name) = PROCESS_CACHE
        .lock()
        .ok()
        .and_then(|c| c.as_ref().and_then(|m| m.get(&pid).cloned()))
    {
        return name;
    }
    let mut name = String::new();
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if !handle.is_null() {
            let mut buf = [0u16; 1024];
            let mut len = buf.len() as u32;
            if QueryFullProcessImageNameW(handle, PROCESS_NAME_WIN32, buf.as_mut_ptr(), &mut len)
                != 0
            {
                let full = String::from_utf16_lossy(&buf[..len as usize]);
                name = full
                    .rsplit(['\\', '/'])
                    .next()
                    .unwrap_or("")
                    .to_lowercase();
            }
            CloseHandle(handle);
        }
    }
    if let Ok(mut cache) = PROCESS_CACHE.lock() {
        let map = cache.get_or_insert_with(HashMap::new);
        if map.len() > 512 {
            map.clear();
        }
        map.insert(pid, name.clone());
    }
    name
}

pub fn window_info(hwnd: Hwnd) -> WindowInfo {
    let pid = window_pid(hwnd);
    WindowInfo {
        hwnd,
        title: window_title(hwnd),
        class_name: window_class(hwnd),
        pid,
        process: process_name(pid),
    }
}

pub fn exstyle(hwnd: Hwnd) -> u32 {
    unsafe { GetWindowLongPtrW(h(hwnd), GWL_EXSTYLE) as u32 }
}

fn set_exstyle(hwnd: Hwnd, style: u32) {
    unsafe {
        SetWindowLongPtrW(h(hwnd), GWL_EXSTYLE, style as i32 as isize);
    }
}

pub fn is_cloaked(hwnd: Hwnd) -> bool {
    let mut cloaked: u32 = 0;
    let hr = unsafe {
        DwmGetWindowAttribute(
            h(hwnd),
            DWMWA_CLOAKED as u32,
            &mut cloaked as *mut u32 as *mut c_void,
            std::mem::size_of::<u32>() as u32,
        )
    };
    hr == 0 && cloaked != 0
}

/// Shell and system windows that must never be made transparent.
const EXCLUDED_CLASSES: &[&str] = &[
    "Progman",
    "WorkerW",
    "Shell_TrayWnd",
    "Shell_SecondaryTrayWnd",
    "Windows.UI.Core.CoreWindow",
    "XamlExplorerHostIslandWindow",
    crate::dimmer::OVERLAY_CLASS,
    crate::shell::SHELL_CLASS,
];
const EXCLUDED_TITLES: &[&str] = &["Windows Input Experience", "Program Manager"];

/// A window a user would recognise: visible, titled, not a tool window and
/// not part of the Windows shell.
pub fn is_app_window(hwnd: Hwnd) -> bool {
    unsafe {
        if IsWindow(h(hwnd)) == 0 || IsWindowVisible(h(hwnd)) == 0 {
            return false;
        }
    }
    if exstyle(hwnd) & WS_EX_TOOLWINDOW != 0 {
        return false;
    }
    let title = window_title(hwnd);
    if title.is_empty() || EXCLUDED_TITLES.contains(&title.as_str()) {
        return false;
    }
    let class = window_class(hwnd);
    if EXCLUDED_CLASSES.contains(&class.as_str()) {
        return false;
    }
    !is_cloaked(hwnd)
}

unsafe extern "system" fn collect_hwnds(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let list = &mut *(lparam as *mut Vec<Hwnd>);
    list.push(hwnd as Hwnd);
    1
}

pub fn top_level_windows() -> Vec<Hwnd> {
    let mut list: Vec<Hwnd> = Vec::with_capacity(256);
    unsafe {
        EnumWindows(Some(collect_hwnds), &mut list as *mut Vec<Hwnd> as LPARAM);
    }
    list
}

/// All current top-level app windows (minimised ones included).
pub fn enum_app_windows() -> Vec<WindowInfo> {
    top_level_windows()
        .into_iter()
        .filter(|&w| is_app_window(w))
        .map(window_info)
        .collect()
}

pub fn foreground_window() -> Hwnd {
    unsafe { GetForegroundWindow() as Hwnd }
}

pub fn window_rect(hwnd: Hwnd) -> (i32, i32, i32, i32) {
    let mut r = RECT { left: 0, top: 0, right: 0, bottom: 0 };
    unsafe { GetWindowRect(h(hwnd), &mut r) };
    (r.left, r.top, r.right, r.bottom)
}

// --- transparency and styles ------------------------------------------------

pub fn set_alpha(hwnd: Hwnd, alpha: u8) -> bool {
    if !is_window(hwnd) {
        return false;
    }
    let style = exstyle(hwnd);
    if style & WS_EX_LAYERED == 0 {
        set_exstyle(hwnd, style | WS_EX_LAYERED);
    }
    unsafe { SetLayeredWindowAttributes(h(hwnd), 0, alpha, LWA_ALPHA) != 0 }
}

/// Current layered alpha, or None when the window is not layered.
pub fn get_alpha(hwnd: Hwnd) -> Option<u8> {
    if exstyle(hwnd) & WS_EX_LAYERED == 0 {
        return None;
    }
    let mut alpha: u8 = 0;
    let mut flags: u32 = 0;
    let ok = unsafe {
        GetLayeredWindowAttributes(h(hwnd), std::ptr::null_mut(), &mut alpha, &mut flags)
    };
    if ok != 0 && flags & LWA_ALPHA != 0 {
        Some(alpha)
    } else {
        None
    }
}

/// Put a window back how it was before we touched it.
pub fn restore_window(hwnd: Hwnd, had_layered: bool, prev_alpha: Option<u8>) -> bool {
    if !is_window(hwnd) {
        return false;
    }
    if had_layered {
        return set_alpha(hwnd, prev_alpha.unwrap_or(255));
    }
    set_alpha(hwnd, 255);
    let style = exstyle(hwnd);
    if style & WS_EX_LAYERED != 0 {
        set_exstyle(hwnd, style & !WS_EX_LAYERED);
    }
    true
}

pub fn set_click_through(hwnd: Hwnd, enabled: bool) -> bool {
    if !is_window(hwnd) {
        return false;
    }
    let mut style = exstyle(hwnd);
    if enabled {
        style |= WS_EX_LAYERED | WS_EX_TRANSPARENT;
    } else {
        style &= !WS_EX_TRANSPARENT;
    }
    set_exstyle(hwnd, style);
    true
}


pub fn set_topmost(hwnd: Hwnd, enabled: bool) -> bool {
    if !is_window(hwnd) {
        return false;
    }
    let after = if enabled { HWND_TOPMOST } else { HWND_NOTOPMOST };
    unsafe {
        SetWindowPos(
            h(hwnd),
            after,
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
        ) != 0
    }
}

pub fn virtual_screen_rect() -> (i32, i32, i32, i32) {
    unsafe {
        (
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
            GetSystemMetrics(SM_CXVIRTUALSCREEN),
            GetSystemMetrics(SM_CYVIRTUALSCREEN),
        )
    }
}

// --- monitors ---------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MonitorKind {
    External,
    BuiltIn,
    Virtual,
}

#[derive(Clone, Debug)]
pub struct Monitor {
    /// Stable identity: the monitor's device interface path (lowercase). It
    /// survives lid close/open and replugging, unlike the GDI device name.
    pub id: String,
    /// Current GDI device name (`\\.\DISPLAY5`). Changes between sessions and
    /// topology changes; only used for the actual Win32 calls.
    pub device: String,
    pub friendly: String,
    pub kind: MonitorKind,
    pub rect: (i32, i32, i32, i32), // x, y, w, h
    pub primary: bool,
    pub number: u32,
}

impl Monitor {
    pub fn label(&self) -> String {
        match self.kind {
            MonitorKind::BuiltIn => "Built-in display".to_string(),
            _ if !self.friendly.trim().is_empty() => {
                let f = self.friendly.trim();
                if self.kind == MonitorKind::Virtual && !f.to_lowercase().contains("virtual") {
                    format!("{f} (virtual)")
                } else {
                    f.to_string()
                }
            }
            MonitorKind::Virtual => format!("Virtual display {}", self.number),
            MonitorKind::External => format!("Display {}", self.number),
        }
    }
}

const QDC_ONLY_ACTIVE_PATHS: u32 = 0x2;
const TECH_INTERNAL: i32 = 0x8000_0000u32 as i32;
const TECH_LVDS: i32 = 6;
const TECH_DISPLAYPORT_EMBEDDED: i32 = 11;
const TECH_UDI_EMBEDDED: i32 = 13;
const TECH_INDIRECT_VIRTUAL: i32 = 17;
const TECH_OTHER: i32 = -1;

struct TargetInfo {
    path: String,
    friendly: String,
    tech: i32,
}

/// GDI device name -> monitor target info, from the display configuration.
fn display_targets() -> HashMap<String, TargetInfo> {
    let mut out = HashMap::new();
    unsafe {
        let mut n_paths = 0u32;
        let mut n_modes = 0u32;
        if GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, &mut n_paths, &mut n_modes) != 0 {
            return out;
        }
        let mut paths: Vec<DISPLAYCONFIG_PATH_INFO> = vec![std::mem::zeroed(); n_paths as usize];
        let mut modes: Vec<DISPLAYCONFIG_MODE_INFO> = vec![std::mem::zeroed(); n_modes as usize];
        if QueryDisplayConfig(
            QDC_ONLY_ACTIVE_PATHS,
            &mut n_paths,
            paths.as_mut_ptr(),
            &mut n_modes,
            modes.as_mut_ptr(),
            std::ptr::null_mut(),
        ) != 0
        {
            return out;
        }
        paths.truncate(n_paths as usize);
        for p in &paths {
            let mut source: DISPLAYCONFIG_SOURCE_DEVICE_NAME = std::mem::zeroed();
            source.header.r#type = DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME;
            source.header.size = std::mem::size_of::<DISPLAYCONFIG_SOURCE_DEVICE_NAME>() as u32;
            source.header.adapterId = p.sourceInfo.adapterId;
            source.header.id = p.sourceInfo.id;
            if DisplayConfigGetDeviceInfo(&mut source.header) != 0 {
                continue;
            }
            let mut target: DISPLAYCONFIG_TARGET_DEVICE_NAME = std::mem::zeroed();
            target.header.r#type = DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME;
            target.header.size = std::mem::size_of::<DISPLAYCONFIG_TARGET_DEVICE_NAME>() as u32;
            target.header.adapterId = p.targetInfo.adapterId;
            target.header.id = p.targetInfo.id;
            if DisplayConfigGetDeviceInfo(&mut target.header) != 0 {
                continue;
            }
            let gdi = from_wide(&source.viewGdiDeviceName);
            out.insert(
                gdi.to_uppercase(),
                TargetInfo {
                    path: from_wide(&target.monitorDevicePath).to_lowercase(),
                    friendly: from_wide(&target.monitorFriendlyDeviceName),
                    tech: target.outputTechnology as i32,
                },
            );
        }
    }
    out
}

unsafe extern "system" fn collect_monitors(
    hmon: HMONITOR,
    _hdc: HDC,
    _rect: *mut RECT,
    lparam: LPARAM,
) -> BOOL {
    let list = &mut *(lparam as *mut Vec<(String, RECT, bool)>);
    let mut info: MONITORINFOEXW = std::mem::zeroed();
    info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
    if GetMonitorInfoW(hmon, &mut info as *mut MONITORINFOEXW as *mut MONITORINFO) != 0 {
        list.push((
            from_wide(&info.szDevice),
            info.monitorInfo.rcMonitor,
            info.monitorInfo.dwFlags & 1 != 0,
        ));
    }
    1
}

fn display_number(device: &str) -> Option<u32> {
    let digits: String = device
        .chars()
        .rev()
        .take_while(|c| c.is_ascii_digit())
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    digits.parse().ok()
}

/// Attached monitors, ordered by Windows' display number.
pub fn enum_monitors() -> Vec<Monitor> {
    let mut raw: Vec<(String, RECT, bool)> = Vec::new();
    unsafe {
        EnumDisplayMonitors(
            std::ptr::null_mut(),
            std::ptr::null(),
            Some(collect_monitors),
            &mut raw as *mut _ as LPARAM,
        );
    }
    let targets = display_targets();
    let mut monitors: Vec<Monitor> = raw
        .into_iter()
        .map(|(device, r, primary)| {
            let t = targets.get(&device.to_uppercase());
            let name_says_virtual = t
                .map(|t| {
                    let f = format!("{} {}", t.friendly, t.path).to_lowercase();
                    ["parsec", "virtual", "vda", "spacedesk", "dummy", "idd"].iter().any(|k| f.contains(k))
                })
                .unwrap_or(false);
            let kind = match t.map(|t| t.tech) {
                _ if name_says_virtual => MonitorKind::Virtual,
                Some(TECH_INTERNAL | TECH_LVDS | TECH_DISPLAYPORT_EMBEDDED | TECH_UDI_EMBEDDED) => {
                    MonitorKind::BuiltIn
                }
                Some(TECH_INDIRECT_VIRTUAL) => MonitorKind::Virtual,
                Some(TECH_OTHER) => MonitorKind::Virtual,
                _ => MonitorKind::External,
            };
            let id = t
                .map(|t| t.path.clone())
                .filter(|p| !p.is_empty())
                .unwrap_or_else(|| device.to_lowercase());
            Monitor {
                id,
                number: display_number(&device).unwrap_or(0),
                device,
                friendly: t.map(|t| t.friendly.clone()).unwrap_or_default(),
                kind,
                rect: (r.left, r.top, r.right - r.left, r.bottom - r.top),
                primary,
            }
        })
        .collect();
    monitors.sort_by_key(|m| (m.number, m.rect.0, m.rect.1));
    monitors
}

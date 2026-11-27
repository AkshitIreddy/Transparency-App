//! Screen dimming: hardware gamma where the screen supports it, topped up by
//! a click-through black overlay for darker levels or unsupported screens.
//!
//! Lives on the shell thread (which pumps the overlay windows' messages).
//! Only screens the user picked are dimmed; a screen that is connected later
//! is left alone until it is picked.

use std::collections::{HashMap, HashSet};

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{GetStockObject, BLACK_BRUSH, HBRUSH};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Shell::{SHAppBarMessage, APPBARDATA};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use crate::config::{Settings, MAX_DIM};
use crate::gamma::{GammaManager, GdiGamma, Screen};
use crate::win::{self, Hwnd, Monitor};

pub const OVERLAY_CLASS: &str = "TransparencyAppDimmer";
const TASKBAR_GAP: i32 = 2;
const WDA_NONE: u32 = 0;
const WDA_EXCLUDEFROMCAPTURE: u32 = 0x11;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Hardware,
    Overlay,
    Mixed,
}

impl Mode {
    pub fn label(self) -> &'static str {
        match self {
            Mode::Hardware => "Hardware dimming",
            Mode::Overlay => "Overlay dimming",
            Mode::Mixed => "Hardware + overlay",
        }
    }
}

unsafe extern "system" fn overlay_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if msg == WM_MOUSEACTIVATE {
        return MA_NOACTIVATE as LRESULT;
    }
    DefWindowProcW(hwnd, msg, wp, lp)
}

fn register_class() {
    let name = win::wide(OVERLAY_CLASS);
    unsafe {
        let wc = WNDCLASSW {
            style: 0,
            lpfnWndProc: Some(overlay_proc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: GetModuleHandleW(std::ptr::null()),
            hIcon: std::ptr::null_mut(),
            hCursor: std::ptr::null_mut(),
            hbrBackground: GetStockObject(BLACK_BRUSH) as HBRUSH,
            lpszMenuName: std::ptr::null(),
            lpszClassName: name.as_ptr(),
        };
        RegisterClassW(&wc);
    }
}

/// (edge, auto_hide, taskbar rect) of the primary taskbar.
fn taskbar_state() -> (u32, bool, Option<RECT>) {
    unsafe {
        let mut abd: APPBARDATA = std::mem::zeroed();
        abd.cbSize = std::mem::size_of::<APPBARDATA>() as u32;
        let state = SHAppBarMessage(0x4 /* ABM_GETSTATE */, &mut abd);
        let mut pos: APPBARDATA = std::mem::zeroed();
        pos.cbSize = std::mem::size_of::<APPBARDATA>() as u32;
        let ok = SHAppBarMessage(0x5 /* ABM_GETTASKBARPOS */, &mut pos) != 0;
        (pos.uEdge, state & 0x1 != 0, ok.then_some(pos.rc))
    }
}

fn overlay_rects(monitors: &[&Monitor]) -> HashMap<String, (i32, i32, i32, i32)> {
    let (edge, auto_hide, tb) = taskbar_state();
    let centre = tb.map(|r| ((r.left + r.right) / 2, (r.top + r.bottom) / 2));
    monitors
        .iter()
        .map(|m| {
            let (mut x, mut y, mut w, mut hgt) = m.rect;
            let holds_taskbar = centre
                .map(|(cx, cy)| cx >= x && cx < x + w && cy >= y && cy < y + hgt)
                .unwrap_or(false);
            // Leave a sliver at an auto-hide taskbar so it can still be summoned.
            if auto_hide && holds_taskbar {
                match edge {
                    3 => hgt -= TASKBAR_GAP,
                    1 => {
                        y += TASKBAR_GAP;
                        hgt -= TASKBAR_GAP
                    }
                    0 => {
                        x += TASKBAR_GAP;
                        w -= TASKBAR_GAP
                    }
                    2 => w -= TASKBAR_GAP,
                    _ => {}
                }
            }
            (m.id.clone(), (x, y, w, hgt))
        })
        .collect()
}

fn set_capture_excluded(hwnd: Hwnd, excluded: bool) {
    unsafe {
        SetWindowDisplayAffinity(win::h(hwnd), if excluded { WDA_EXCLUDEFROMCAPTURE } else { WDA_NONE });
    }
}

/// The transient screen-capture picker (never the normal Snipping Tool
/// editor): while it is up, the overlay is hidden from captures.
pub fn capture_ui_active() -> bool {
    let (_vx, _vy, vw, vh) = win::virtual_screen_rect();
    let desktop_area = (vw as i64 * vh as i64).max(1);
    for hwnd in win::top_level_windows() {
        unsafe {
            if IsWindowVisible(win::h(hwnd)) == 0 {
                continue;
            }
        }
        let info = win::window_info(hwnd);
        if info.process == "screenclippinghost.exe" {
            return true;
        }
        if info.process != "snippingtool.exe" {
            continue;
        }
        let class = info.class_name.to_lowercase();
        if class.contains("capture") || class.contains("snipper") {
            return true;
        }
        let (l, t, r, b) = win::window_rect(hwnd);
        let area = ((r - l).max(0) as i64) * ((b - t).max(0) as i64);
        let title = info.title.to_lowercase();
        if area * 100 >= desktop_area * 45 && title != "snipping tool" && title != "snipping tool settings" {
            return true;
        }
    }
    false
}

pub struct Dimmer {
    overlays: HashMap<String, Hwnd>,
    gamma: GammaManager<GdiGamma>,
    enabled: bool,
    capture_active: bool,
    pub modes: HashMap<String, Mode>,
}

impl Dimmer {
    pub fn new() -> Dimmer {
        register_class();
        let mut gamma = GammaManager::new(GdiGamma, Some(crate::paths::gamma_state_path()));
        let monitors = win::enum_monitors();
        let screens: Vec<Screen> = monitors.iter().map(|m| Screen { id: &m.id, device: &m.device }).collect();
        gamma.recover_legacy(&crate::paths::legacy_gamma_ledger_path(), &screens);
        Dimmer {
            overlays: HashMap::new(),
            gamma,
            enabled: false,
            capture_active: false,
            modes: HashMap::new(),
        }
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// True while a gamma restore is owed to a screen (keeps the shell's
    /// timer running so it is retried until done).
    pub fn has_owed(&self) -> bool {
        !self.gamma.owed().is_empty()
    }

    pub fn display_changed(&mut self) {
        self.gamma.display_changed();
    }

    /// Bring screens in line with the settings. `repair` also resets stray
    /// dims on screens we are not dimming (startup and display changes).
    pub fn sync(&mut self, settings: &Settings, repair: bool) {
        self.enabled = settings.dimmer_enabled;
        let monitors = win::enum_monitors();
        let screens: Vec<Screen> = monitors.iter().map(|m| Screen { id: &m.id, device: &m.device }).collect();

        let targets: Vec<&Monitor> = if self.enabled {
            monitors
                .iter()
                .filter(|m| settings.dimmer_screens.iter().any(|id| id == &m.id))
                .filter(|m| settings.level_for(&m.id) > 0)
                .collect()
        } else {
            vec![]
        };

        let mut gamma_ids: HashSet<String> = HashSet::new();
        let mut overlay_alpha: HashMap<String, u8> = HashMap::new();
        self.modes.clear();
        for m in &targets {
            let requested = settings.level_for(&m.id).min(MAX_DIM);
            let screen = Screen { id: &m.id, device: &m.device };
            match self.gamma.apply(&screen, requested) {
                Some(applied) => {
                    gamma_ids.insert(m.id.clone());
                    let requested_scale = (255.0 - requested as f64) / 255.0;
                    let output_scale = (255.0 - applied as f64) / 255.0;
                    let residual = requested_scale / output_scale;
                    let alpha = (255.0 * (1.0 - residual)).round().clamp(0.0, MAX_DIM as f64) as u8;
                    overlay_alpha.insert(m.id.clone(), alpha);
                    self.modes
                        .insert(m.id.clone(), if alpha > 0 { Mode::Mixed } else { Mode::Hardware });
                }
                None => {
                    overlay_alpha.insert(m.id.clone(), requested);
                    self.modes.insert(m.id.clone(), Mode::Overlay);
                }
            }
        }

        self.gamma.settle(&screens, &gamma_ids);
        if repair {
            // Virtual displays (remote desktop adapters) are never touched.
            let physical: Vec<Screen> = monitors
                .iter()
                .filter(|m| m.kind != win::MonitorKind::Virtual)
                .map(|m| Screen { id: &m.id, device: &m.device })
                .collect();
            self.gamma.repair_strays(&physical, &gamma_ids);
        }

        let rects = overlay_rects(&targets);
        let stale: Vec<String> = self
            .overlays
            .keys()
            .filter(|id| overlay_alpha.get(*id).copied().unwrap_or(0) == 0 || !rects.contains_key(*id))
            .cloned()
            .collect();
        for id in stale {
            self.destroy_one(&id);
        }
        for (id, rect) in &rects {
            let alpha = overlay_alpha.get(id).copied().unwrap_or(0);
            if alpha == 0 {
                continue;
            }
            match self.overlays.get(id).copied() {
                Some(hwnd) if win::is_window(hwnd) => self.reassert(hwnd, *rect, alpha),
                _ => {
                    if let Some(hwnd) = self.create(*rect, alpha) {
                        self.overlays.insert(id.clone(), hwnd);
                    }
                }
            }
        }
    }

    fn create(&self, rect: (i32, i32, i32, i32), alpha: u8) -> Option<Hwnd> {
        let class = win::wide(OVERLAY_CLASS);
        let title = win::wide("Screen Dimming Overlay");
        let (x, y, w, hgt) = rect;
        unsafe {
            let hwnd = CreateWindowExW(
                WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOPMOST | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
                class.as_ptr(),
                title.as_ptr(),
                WS_POPUP,
                x,
                y,
                w,
                hgt,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                GetModuleHandleW(std::ptr::null()),
                std::ptr::null(),
            );
            if hwnd.is_null() {
                return None;
            }
            let hwnd = hwnd as Hwnd;
            SetLayeredWindowAttributes(win::h(hwnd), 0, alpha, LWA_ALPHA);
            set_capture_excluded(hwnd, self.capture_active);
            SetWindowPos(win::h(hwnd), HWND_TOPMOST, x, y, w, hgt, SWP_SHOWWINDOW | SWP_NOACTIVATE);
            Some(hwnd)
        }
    }

    fn reassert(&self, hwnd: Hwnd, rect: (i32, i32, i32, i32), alpha: u8) {
        let (x, y, w, hgt) = rect;
        unsafe {
            SetWindowPos(win::h(hwnd), HWND_TOPMOST, x, y, w, hgt, SWP_SHOWWINDOW | SWP_NOACTIVATE);
            SetLayeredWindowAttributes(win::h(hwnd), 0, alpha, LWA_ALPHA);
        }
        set_capture_excluded(hwnd, self.capture_active);
    }

    fn destroy_one(&mut self, id: &str) {
        if let Some(hwnd) = self.overlays.remove(id) {
            if win::is_window(hwnd) {
                unsafe { DestroyWindow(win::h(hwnd)) };
            }
        }
    }

    pub fn set_capture_active(&mut self, active: bool) {
        if active == self.capture_active {
            return;
        }
        self.capture_active = active;
        for &hwnd in self.overlays.values() {
            if win::is_window(hwnd) {
                set_capture_excluded(hwnd, active);
            }
        }
    }

    pub fn has_overlays(&self) -> bool {
        !self.overlays.is_empty()
    }

    /// Undo everything we can reach (shutdown). Owed restores for screens
    /// that are off stay on disk for the next launch.
    pub fn shutdown(&mut self) {
        let ids: Vec<String> = self.overlays.keys().cloned().collect();
        for id in ids {
            self.destroy_one(&id);
        }
        let monitors = win::enum_monitors();
        let screens: Vec<Screen> = monitors.iter().map(|m| Screen { id: &m.id, device: &m.device }).collect();
        self.gamma.restore_all(&screens);
        self.enabled = false;
    }
}

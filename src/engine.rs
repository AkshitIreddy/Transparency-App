//! Event-driven transparency engine.
//!
//! - A WinEventHook thread reports windows being created, shown, renamed,
//!   uncloaked, destroyed or focused; nothing polls.
//! - One worker thread owns every Win32 mutation.
//! - Each touched window's original state is snapshotted, so restore puts it
//!   back exactly, and redundant writes are skipped.
//! - A slow safety sweep catches anything the hook missed.
//! - A crash ledger on disk lets the next launch undo a crashed session.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::Duration;

use serde_json::{json, Value};
use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::System::Threading::GetCurrentThreadId;
use windows_sys::Win32::UI::Accessibility::{SetWinEventHook, UnhookWinEvent, HWINEVENTHOOK};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, GetMessageW, PeekMessageW, PostThreadMessageW, TranslateMessage, MSG,
    WM_QUIT, WM_USER,
};

use crate::config::{FocusMode, Rule, Store, MIN_FOCUS_ALPHA};
use crate::win::{self, Hwnd, WindowInfo};

const SWEEP_INTERVAL: Duration = Duration::from_secs(5);

pub const EVENT_SYSTEM_FOREGROUND: u32 = 0x0003;
const EVENT_OBJECT_CREATE: u32 = 0x8000;
const EVENT_OBJECT_DESTROY: u32 = 0x8001;
const EVENT_OBJECT_SHOW: u32 = 0x8002;
const EVENT_OBJECT_NAMECHANGE: u32 = 0x800C;
const EVENT_OBJECT_UNCLOAKED: u32 = 0x8018;
const WINEVENT_OUTOFCONTEXT: u32 = 0;
const WINEVENT_SKIPOWNPROCESS: u32 = 2;

enum Cmd {
    Event(u32, Hwnd),
    Sweep,
    RestoreAll,
    Override(Hwnd, u8),
    Stop,
}

struct WindowState {
    had_layered: bool,
    prev_alpha: Option<u8>,
    was_topmost: bool,
    was_click: bool,
    alpha: Option<u8>,
    click: bool,
    topmost: bool,
    process: String,
    title: String,
}

impl WindowState {
    fn capture(info: &WindowInfo) -> WindowState {
        let style = win::exstyle(info.hwnd);
        let was_click = style & win::EX_TRANSPARENT != 0;
        let was_topmost = style & win::EX_TOPMOST != 0;
        WindowState {
            had_layered: style & win::EX_LAYERED != 0,
            prev_alpha: win::get_alpha(info.hwnd),
            was_topmost,
            was_click,
            alpha: None,
            click: was_click,
            topmost: was_topmost,
            process: info.process.clone(),
            title: info.title.clone(),
        }
    }
}

pub struct Engine {
    tx: Sender<Cmd>,
    rx: Mutex<Option<Receiver<Cmd>>>,
    paused: Arc<AtomicBool>,
    focus: Arc<AtomicBool>,
    affected: Arc<AtomicUsize>,
    overrides: Arc<Mutex<HashMap<Hwnd, u8>>>,
    hook_thread: AtomicU32,
    threads: Mutex<Vec<JoinHandle<()>>>,
    config: Arc<Store>,
    ledger: Option<PathBuf>,
}

static HOOK_TX: OnceLock<Mutex<Option<Sender<Cmd>>>> = OnceLock::new();

unsafe extern "system" fn win_event_proc(
    _hook: HWINEVENTHOOK,
    event: u32,
    hwnd: HWND,
    id_object: i32,
    id_child: i32,
    _thread: u32,
    _time: u32,
) {
    if id_object != 0 || id_child != 0 || hwnd.is_null() {
        return;
    }
    if let Some(lock) = HOOK_TX.get() {
        if let Ok(guard) = lock.lock() {
            if let Some(tx) = guard.as_ref() {
                let _ = tx.send(Cmd::Event(event, hwnd as Hwnd));
            }
        }
    }
}

impl Engine {
    pub fn new(config: Arc<Store>, ledger: Option<PathBuf>) -> Arc<Engine> {
        let (tx, rx) = mpsc::channel();
        let engine = Arc::new(Engine {
            tx,
            rx: Mutex::new(Some(rx)),
            paused: Arc::new(AtomicBool::new(false)),
            focus: Arc::new(AtomicBool::new(false)),
            affected: Arc::new(AtomicUsize::new(0)),
            overrides: Arc::new(Mutex::new(HashMap::new())),
            hook_thread: AtomicU32::new(0),
            threads: Mutex::new(vec![]),
            config: config.clone(),
            ledger,
        });
        let tx = engine.tx.clone();
        config.add_listener(move || {
            let _ = tx.send(Cmd::Sweep);
        });
        engine
    }

    pub fn start(&self) {
        let Some(rx) = self.rx.lock().unwrap().take() else { return };
        if let Some(path) = &self.ledger {
            recover_previous_session(path);
        }
        let mut worker = Worker {
            config: self.config.clone(),
            ledger: self.ledger.clone(),
            paused: self.paused.clone(),
            focus: self.focus.clone(),
            affected: self.affected.clone(),
            overrides: self.overrides.clone(),
            state: HashMap::new(),
            last_foreground: 0,
            ledger_dirty: false,
            own_pid: win::own_pid(),
            rules: vec![],
            focus_cfg: self.config.settings().focus,
        };
        let t = std::thread::Builder::new()
            .name("TransparencyEngine".into())
            .spawn(move || worker.run(rx))
            .expect("spawn engine");
        self.threads.lock().unwrap().push(t);

        HOOK_TX
            .get_or_init(|| Mutex::new(None))
            .lock()
            .unwrap()
            .replace(self.tx.clone());
        let (ready_tx, ready_rx) = mpsc::channel();
        let hook = std::thread::Builder::new()
            .name("WinEventHook".into())
            .spawn(move || unsafe {
                let mut msg: MSG = std::mem::zeroed();
                PeekMessageW(&mut msg, std::ptr::null_mut(), WM_USER, WM_USER, 0);
                let _ = ready_tx.send(GetCurrentThreadId());
                let mut hooks = vec![];
                for ev in [
                    EVENT_SYSTEM_FOREGROUND,
                    EVENT_OBJECT_CREATE,
                    EVENT_OBJECT_DESTROY,
                    EVENT_OBJECT_SHOW,
                    EVENT_OBJECT_NAMECHANGE,
                    EVENT_OBJECT_UNCLOAKED,
                ] {
                    let h = SetWinEventHook(
                        ev,
                        ev,
                        std::ptr::null_mut(),
                        Some(win_event_proc),
                        0,
                        0,
                        WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS,
                    );
                    if !h.is_null() {
                        hooks.push(h);
                    }
                }
                while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
                    TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
                for h in hooks {
                    UnhookWinEvent(h);
                }
            })
            .expect("spawn hook");
        if let Ok(id) = ready_rx.recv_timeout(Duration::from_secs(5)) {
            self.hook_thread.store(id, Ordering::SeqCst);
        }
        self.threads.lock().unwrap().push(hook);
        self.request_sweep();
    }

    /// Stop and restore every window we touched.
    pub fn stop(&self) {
        let id = self.hook_thread.swap(0, Ordering::SeqCst);
        if id != 0 {
            unsafe { PostThreadMessageW(id, WM_QUIT, 0, 0) };
        }
        if let Some(lock) = HOOK_TX.get() {
            lock.lock().unwrap().take();
        }
        let _ = self.tx.send(Cmd::Stop);
        let threads: Vec<_> = self.threads.lock().unwrap().drain(..).collect();
        for t in threads {
            let _ = t.join();
        }
    }

    pub fn request_sweep(&self) {
        let _ = self.tx.send(Cmd::Sweep);
    }

    pub fn paused(&self) -> bool {
        self.paused.load(Ordering::SeqCst)
    }

    pub fn focus_mode(&self) -> bool {
        self.focus.load(Ordering::SeqCst)
    }

    pub fn affected(&self) -> usize {
        self.affected.load(Ordering::SeqCst)
    }

    pub fn set_paused(&self, paused: bool) {
        self.paused.store(paused, Ordering::SeqCst);
        if paused {
            let _ = self.tx.send(Cmd::RestoreAll);
        }
        // Sweep either way: after a pause focus mode (which keeps working
        // while rules are paused) is re-applied; after resume everything is.
        self.request_sweep();
    }

    pub fn set_focus_mode(&self, enabled: bool) {
        self.focus.store(enabled, Ordering::SeqCst);
        if !enabled {
            let _ = self.tx.send(Cmd::RestoreAll);
        }
        self.request_sweep();
    }

    pub fn restore_all(&self) {
        let _ = self.tx.send(Cmd::RestoreAll);
        self.request_sweep();
    }

    /// Restore everything and pause: the escape hatch.
    pub fn panic_restore(&self) {
        self.paused.store(true, Ordering::SeqCst);
        self.focus.store(false, Ordering::SeqCst);
        let _ = self.tx.send(Cmd::RestoreAll);
    }

    pub fn set_override(&self, hwnd: Hwnd, alpha: u8) {
        let _ = self.tx.send(Cmd::Override(hwnd, alpha));
    }

    pub fn get_override(&self, hwnd: Hwnd) -> Option<u8> {
        self.overrides.lock().unwrap().get(&hwnd).copied()
    }
}

struct Worker {
    config: Arc<Store>,
    ledger: Option<PathBuf>,
    paused: Arc<AtomicBool>,
    focus: Arc<AtomicBool>,
    affected: Arc<AtomicUsize>,
    overrides: Arc<Mutex<HashMap<Hwnd, u8>>>,
    state: HashMap<Hwnd, WindowState>,
    last_foreground: Hwnd,
    ledger_dirty: bool,
    own_pid: u32,
    rules: Vec<Rule>,
    focus_cfg: FocusMode,
}

impl Worker {
    fn paused(&self) -> bool {
        self.paused.load(Ordering::SeqCst)
    }
    fn focus(&self) -> bool {
        self.focus.load(Ordering::SeqCst)
    }

    fn refresh_config(&mut self) {
        let d = self.config.read();
        self.rules = d.rules.clone();
        self.focus_cfg = d.settings.focus.clone();
    }

    fn find_rule(&self, title: &str, process: &str) -> Option<&Rule> {
        self.rules.iter().find(|r| r.enabled && r.matches(title, process))
    }

    fn run(&mut self, rx: Receiver<Cmd>) {
        self.refresh_config();
        loop {
            let cmd = match rx.recv_timeout(SWEEP_INTERVAL) {
                Ok(c) => c,
                Err(RecvTimeoutError::Timeout) => {
                    self.sweep();
                    self.flush_ledger();
                    continue;
                }
                Err(RecvTimeoutError::Disconnected) => {
                    self.restore_everything();
                    self.clear_ledger();
                    return;
                }
            };
            // Coalesce bursts: drain what is queued now, dropping duplicate
            // events and collapsing many sweeps (slider drags) into one.
            let mut batch = vec![cmd];
            while let Ok(next) = rx.try_recv() {
                batch.push(next);
            }
            let mut seen: HashSet<(u32, Hwnd)> = HashSet::new();
            let mut sweep = false;
            let mut refreshed = false;
            for cmd in batch {
                match cmd {
                    Cmd::Stop => {
                        self.restore_everything();
                        self.clear_ledger();
                        return;
                    }
                    Cmd::Sweep => sweep = true,
                    Cmd::Event(ev, hwnd) => {
                        if seen.insert((ev, hwnd)) {
                            if !refreshed {
                                self.refresh_config();
                                refreshed = true;
                            }
                            self.handle_event(ev, hwnd);
                        }
                    }
                    Cmd::RestoreAll => self.restore_everything(),
                    Cmd::Override(hwnd, alpha) => {
                        self.overrides
                            .lock()
                            .unwrap()
                            .insert(hwnd, alpha.max(MIN_FOCUS_ALPHA));
                        if !refreshed {
                            self.refresh_config();
                            refreshed = true;
                        }
                        self.apply_one(hwnd);
                    }
                }
            }
            if sweep {
                self.refresh_config();
                self.sweep();
            }
            self.update_affected();
            self.flush_ledger();
        }
    }

    fn update_affected(&self) {
        let n = self.state.values().filter(|s| s.alpha.is_some()).count();
        self.affected.store(n, Ordering::SeqCst);
    }

    fn handle_event(&mut self, event: u32, hwnd: Hwnd) {
        if event == EVENT_OBJECT_DESTROY {
            self.forget(hwnd);
            return;
        }
        let previous = self.last_foreground;
        if event == EVENT_SYSTEM_FOREGROUND {
            self.last_foreground = hwnd;
        }
        if self.paused() && !self.focus() {
            return;
        }
        if event == EVENT_SYSTEM_FOREGROUND && self.focus() {
            self.apply_one(hwnd);
            if previous != 0 && previous != hwnd {
                self.apply_one(previous);
            }
            return;
        }
        self.apply_one(hwnd);
    }

    fn target_alpha(&self, info: &WindowInfo, foreground: Hwnd) -> Option<u8> {
        let paused = self.paused();
        let focus = self.focus();
        if paused && !focus {
            return None;
        }
        if let Some(o) = self.overrides.lock().unwrap().get(&info.hwnd) {
            return Some(*o);
        }
        if focus {
            let fm = &self.focus_cfg;
            if fm.exclude.iter().any(|p| p == &info.process) {
                return None;
            }
            let rule = if paused { None } else { self.find_rule(&info.title, &info.process) };
            if info.hwnd == foreground {
                // Never let focus mode make the window in use invisible.
                let target = rule.map(|r| r.opacity).unwrap_or(fm.active_opacity);
                return Some(target.max(MIN_FOCUS_ALPHA));
            }
            return Some(fm.background_opacity.max(MIN_FOCUS_ALPHA));
        }
        self.find_rule(&info.title, &info.process).map(|r| r.opacity)
    }

    fn apply_one(&mut self, hwnd: Hwnd) {
        if !win::is_window(hwnd) {
            self.forget(hwnd);
            return;
        }
        if !win::is_app_window(hwnd) {
            return;
        }
        let info = win::window_info(hwnd);
        if info.pid == self.own_pid {
            return;
        }
        let foreground = if self.focus() && self.last_foreground != 0 {
            self.last_foreground
        } else {
            win::foreground_window()
        };
        self.apply_to(&info, foreground);
    }

    fn apply_to(&mut self, info: &WindowInfo, foreground: Hwnd) {
        let hwnd = info.hwnd;
        let target = self.target_alpha(info, foreground);
        let Some(target) = target else {
            if let Some(state) = self.state.get(&hwnd) {
                if state.alpha.is_some() {
                    self.restore_one(hwnd);
                }
            }
            return;
        };
        if !self.state.contains_key(&hwnd) {
            self.state.insert(hwnd, WindowState::capture(info));
            self.ledger_dirty = true;
        }
        let rule = if !self.focus()
            && !self.paused()
            && !self.overrides.lock().unwrap().contains_key(&hwnd)
        {
            self.find_rule(&info.title, &info.process).cloned()
        } else {
            None
        };
        let state = self.state.get_mut(&hwnd).unwrap();
        if state.alpha != Some(target) && win::set_alpha(hwnd, target) {
            state.alpha = Some(target);
        }
        let want_click = rule.as_ref().map(|r| r.click_through).unwrap_or(state.was_click);
        let want_top = rule.as_ref().map(|r| r.topmost).unwrap_or(state.was_topmost);
        if state.click != want_click && win::set_click_through(hwnd, want_click) {
            state.click = want_click;
        }
        if state.topmost != want_top && win::set_topmost(hwnd, want_top) {
            state.topmost = want_top;
        }
    }

    fn sweep(&mut self) {
        if self.paused() && !self.focus() {
            return;
        }
        let foreground = win::foreground_window();
        if self.focus() {
            self.last_foreground = foreground;
        }
        for info in win::enum_app_windows() {
            if info.pid == self.own_pid {
                continue;
            }
            self.apply_to(&info, foreground);
        }
        let gone: Vec<Hwnd> = self.state.keys().copied().filter(|&h| !win::is_window(h)).collect();
        for h in gone {
            self.forget(h);
        }
    }

    fn restore_one(&mut self, hwnd: Hwnd) {
        let Some(state) = self.state.get(&hwnd) else { return };
        let mut ok = win::restore_window(hwnd, state.had_layered, state.prev_alpha);
        if state.click != state.was_click {
            ok = win::set_click_through(hwnd, state.was_click) && ok;
        }
        if state.topmost != state.was_topmost {
            ok = win::set_topmost(hwnd, state.was_topmost) && ok;
        }
        // Keep a failed restore tracked so a later sweep can retry it.
        if ok || !win::is_window(hwnd) {
            self.forget(hwnd);
        }
    }

    fn restore_everything(&mut self) {
        let hwnds: Vec<Hwnd> = self.state.keys().copied().collect();
        for h in hwnds {
            if win::is_window(h) {
                self.restore_one(h);
            } else {
                self.forget(h);
            }
        }
        self.overrides.lock().unwrap().clear();
        self.ledger_dirty = true;
        self.update_affected();
        self.flush_ledger();
    }

    fn forget(&mut self, hwnd: Hwnd) {
        if self.state.remove(&hwnd).is_some() {
            self.ledger_dirty = true;
        }
        self.overrides.lock().unwrap().remove(&hwnd);
    }

    fn flush_ledger(&mut self) {
        let Some(path) = &self.ledger else { return };
        if !self.ledger_dirty {
            return;
        }
        self.ledger_dirty = false;
        let touched: serde_json::Map<String, Value> = self
            .state
            .iter()
            .filter(|(_, s)| s.alpha.is_some())
            .map(|(h, s)| {
                (
                    h.to_string(),
                    json!({
                        "had_layered": s.had_layered,
                        "prev_alpha": s.prev_alpha,
                        "was_topmost": s.was_topmost,
                        "was_click": s.was_click,
                        "process": s.process,
                        "title": s.title,
                    }),
                )
            })
            .collect();
        if touched.is_empty() {
            let _ = std::fs::remove_file(path);
        } else {
            let data = json!({"pid": self.own_pid, "windows": touched});
            let _ = crate::config::atomic_write(path, data.to_string().as_bytes());
        }
    }

    fn clear_ledger(&self) {
        if let Some(p) = &self.ledger {
            let _ = std::fs::remove_file(p);
        }
    }
}

/// If the last session crashed, undo the windows it left transparent.
/// HWNDs are recycled, so identity (process + title) must still match.
fn recover_previous_session(path: &PathBuf) {
    let Ok(text) = std::fs::read_to_string(path) else { return };
    let mut recovered = 0;
    if let Ok(v) = serde_json::from_str::<Value>(&text) {
        if let Some(windows) = v.get("windows").and_then(Value::as_object) {
            for (key, snap) in windows {
                let Ok(hwnd) = key.parse::<isize>() else { continue };
                if !win::is_app_window(hwnd) || win::get_alpha(hwnd).is_none() {
                    continue;
                }
                let info = win::window_info(hwnd);
                let proc_ = snap.get("process").and_then(Value::as_str).unwrap_or("");
                let title = snap.get("title").and_then(Value::as_str).unwrap_or("");
                if (!proc_.is_empty() && info.process != proc_) || (!title.is_empty() && info.title != title) {
                    continue;
                }
                let had_layered = snap.get("had_layered").and_then(Value::as_bool).unwrap_or(false);
                let prev = snap.get("prev_alpha").and_then(Value::as_u64).map(|a| a.min(255) as u8);
                win::restore_window(hwnd, had_layered, prev);
                if !snap.get("was_click").and_then(Value::as_bool).unwrap_or(false) {
                    win::set_click_through(hwnd, false);
                }
                if !snap.get("was_topmost").and_then(Value::as_bool).unwrap_or(false) {
                    win::set_topmost(hwnd, false);
                }
                recovered += 1;
            }
        }
    }
    if recovered > 0 {
        crate::log::info(format!("recovered {recovered} windows from previous session"));
    }
    let _ = std::fs::remove_file(path);
}

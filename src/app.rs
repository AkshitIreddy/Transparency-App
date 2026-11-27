//! Application core: owns the config and engine, and is the one place every
//! front end (window, tray, hotkeys) goes through. Safe to use from any
//! thread; UI updates are marshalled onto Slint's event loop.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use crate::config::Store;
use crate::dimmer::Mode;
use crate::engine::Engine;
use crate::hotkeys::Action;
use crate::shell::{ShellCmd, ShellHandle};
use crate::updater::{self, Release};
use crate::win;
use crate::AppWindow;

const NUDGE_STEP: i32 = 20;

#[derive(Clone)]
pub struct UpdateStatus {
    pub state: String,
    pub message: String,
    pub release: Option<Release>,
    pub staged: Option<PathBuf>,
}

pub struct Core {
    pub config: Arc<Store>,
    pub engine: Arc<Engine>,
    pub shell: OnceLock<ShellHandle>,
    pub ui: Mutex<Option<slint::Weak<AppWindow>>>,
    pub update: Mutex<UpdateStatus>,
    update_busy: AtomicBool,
    pub dimmer_modes: Mutex<HashMap<String, Mode>>,
    pub hotkey_failures: Mutex<Vec<String>>,
    /// While true, registered hotkeys are released (a shortcut is being
    /// recorded in the settings page).
    pub capturing: AtomicBool,
}

impl Core {
    pub fn new(config: Arc<Store>, engine: Arc<Engine>) -> Arc<Core> {
        Arc::new(Core {
            config,
            engine,
            shell: OnceLock::new(),
            ui: Mutex::new(None),
            update: Mutex::new(UpdateStatus {
                state: "idle".into(),
                message: "Updates are checked automatically.".into(),
                release: None,
                staged: None,
            }),
            update_busy: AtomicBool::new(false),
            dimmer_modes: Mutex::new(HashMap::new()),
            hotkey_failures: Mutex::new(vec![]),
            capturing: AtomicBool::new(false),
        })
    }

    pub fn send_shell(&self, cmd: ShellCmd) {
        if let Some(s) = self.shell.get() {
            s.send(cmd);
        }
    }

    /// Run `f` with the main window on the UI thread.
    pub fn with_ui(self: &Arc<Self>, f: impl FnOnce(&Arc<Core>, &AppWindow) + Send + 'static) {
        let weak = self.ui.lock().unwrap().clone();
        let Some(weak) = weak else { return };
        let core = self.clone();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(ui) = weak.upgrade() {
                f(&core, &ui);
            }
        });
    }

    pub fn refresh_ui(self: &Arc<Self>) {
        self.with_ui(|core, ui| crate::ui::push_state(core, ui));
        self.send_shell(ShellCmd::RefreshTray);
    }

    pub fn refresh_monitors(self: &Arc<Self>) {
        self.with_ui(|core, ui| crate::ui::push_monitors(core, ui));
    }

    // -- toggles ---------------------------------------------------------------

    pub fn set_paused(self: &Arc<Self>, paused: bool) {
        self.engine.set_paused(paused);
        self.config.update_quiet(|d| d.settings.transparency_on = !paused);
        self.refresh_ui();
    }

    pub fn set_focus(self: &Arc<Self>, on: bool) {
        self.engine.set_focus_mode(on);
        self.config.update_quiet(|d| d.settings.focus.enabled = on);
        self.refresh_ui();
    }

    pub fn set_dimmer(self: &Arc<Self>, on: bool) {
        self.config.update_quiet(|d| d.settings.dimmer_enabled = on);
        self.send_shell(ShellCmd::SyncDimmer);
        self.refresh_ui();
    }

    pub fn restore_all(self: &Arc<Self>) {
        self.engine.restore_all();
    }

    pub fn panic(self: &Arc<Self>) {
        self.engine.panic_restore();
        self.config.update_quiet(|d| {
            d.settings.transparency_on = false;
            d.settings.focus.enabled = false;
        });
        self.refresh_ui();
    }

    fn nudge(&self, delta: i32) {
        let hwnd = win::foreground_window();
        if hwnd == 0 || !win::is_app_window(hwnd) || win::window_pid(hwnd) == win::own_pid() {
            return;
        }
        let current = self
            .engine
            .get_override(hwnd)
            .or_else(|| win::get_alpha(hwnd))
            .unwrap_or(255) as i32;
        self.engine.set_override(hwnd, (current + delta).clamp(20, 255) as u8);
    }

    pub fn run_action(self: &Arc<Self>, action: Action) {
        match action {
            Action::TogglePaused => self.set_paused(!self.engine.paused()),
            Action::ToggleFocus => self.set_focus(!self.engine.focus_mode()),
            Action::NudgeUp => self.nudge(NUDGE_STEP),
            Action::NudgeDown => self.nudge(-NUDGE_STEP),
            Action::Panic => self.panic(),
            Action::ToggleDimmer => {
                let on = self.config.settings().dimmer_enabled;
                self.set_dimmer(!on);
            }
        }
    }

    // -- window ------------------------------------------------------------------

    pub fn show_window(self: &Arc<Self>) {
        self.with_ui(|core, ui| {
            crate::ui::push_state(core, ui);
            crate::ui::push_monitors(core, ui);
            let _ = slint::ComponentHandle::show(ui);
            crate::ui::bring_to_front(ui);
        });
    }

    pub fn quit(self: &Arc<Self>) {
        let _ = slint::invoke_from_event_loop(|| {
            let _ = slint::quit_event_loop();
        });
    }


    // -- updates -------------------------------------------------------------------

    fn set_update(self: &Arc<Self>, state: &str, message: impl Into<String>) {
        {
            let mut u = self.update.lock().unwrap();
            u.state = state.to_string();
            u.message = message.into();
        }
        self.refresh_ui();
    }

    pub fn check_for_updates(self: &Arc<Self>, manual: bool) {
        if self.update_busy.swap(true, Ordering::SeqCst) {
            return;
        }
        if self.update.lock().unwrap().state == "ready" {
            self.update_busy.store(false, Ordering::SeqCst);
            if manual {
                self.offer_update();
            }
            return;
        }
        self.set_update("checking", "Checking GitHub Releases...");
        let core = self.clone();
        std::thread::Builder::new()
            .name("Updater".into())
            .spawn(move || {
                let result = (|| -> Result<Option<Release>, String> {
                    let Some(release) = updater::check()? else { return Ok(None) };
                    core.set_update("downloading", format!("Downloading version {}...", release.version));
                    let dest = updater::staged_path(&release.version);
                    let mut last = 0;
                    let staged = updater::download(&release, &dest, |done, total| {
                        let pct = if total > 0 { done * 100 / total } else { 0 };
                        if pct >= last + 5 {
                            last = pct;
                            core.set_update(
                                "downloading",
                                format!("Downloading version {}... {}%", release.version, pct),
                            );
                        }
                    })?;
                    let mut u = core.update.lock().unwrap();
                    u.release = Some(release.clone());
                    u.staged = Some(staged);
                    Ok(Some(release))
                })();
                match result {
                    Ok(None) => core.set_update("current", "You are up to date."),
                    Ok(Some(release)) => {
                        core.set_update("ready", format!("Version {} is ready to install.", release.version));
                        core.send_shell(ShellCmd::Notify(
                            "Transparency App update ready".into(),
                            format!(
                                "Version {} is downloaded and verified. Open the app or its tray menu to install it.",
                                release.version
                            ),
                        ));
                        if manual {
                            core.offer_update();
                        }
                    }
                    Err(e) => {
                        crate::log::warn(format!("update check failed: {e}"));
                        let msg = if manual { e } else { "Automatic update check failed. Try again later.".into() };
                        core.set_update("error", msg);
                    }
                }
                core.update_busy.store(false, Ordering::SeqCst);
            })
            .ok();
    }

    fn offer_update(self: &Arc<Self>) {
        let version = self
            .update
            .lock()
            .unwrap()
            .release
            .as_ref()
            .map(|r| r.version.clone())
            .unwrap_or_default();
        self.with_ui(move |_, ui| {
            ui.set_update_offer_version(version.into());
            ui.set_update_offer_open(true);
            let _ = slint::ComponentHandle::show(ui);
        });
    }

    pub fn install_update(self: &Arc<Self>) {
        let (release, staged) = {
            let u = self.update.lock().unwrap();
            (u.release.clone(), u.staged.clone())
        };
        let (Some(release), Some(staged)) = (release, staged) else {
            self.check_for_updates(true);
            return;
        };
        let target = crate::paths::current_exe();
        match updater::launch_installer(&staged, &target, &release.sha256, &release.version) {
            Ok(()) => {
                self.set_update("installing", "Restarting to finish the update...");
                self.quit();
            }
            Err(e) => self.set_update("error", e),
        }
    }
}

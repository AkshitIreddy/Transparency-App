#![windows_subsystem = "windows"]
//! Transparency App: per-window transparency, focus mode and screen dimming
//! for Windows.

slint::include_modules!();

mod app;
mod config;
mod dimmer;
mod engine;
mod gamma;
mod hotkeys;
mod log;
mod paths;
#[cfg(debug_assertions)]
mod preview;
mod shell;
mod startup;
mod ui;
mod updater;
mod win;

use std::sync::Arc;
use std::time::Duration;

use slint::ComponentHandle;
use windows_sys::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS};
use windows_sys::Win32::System::Threading::CreateMutexW;
use windows_sys::Win32::UI::WindowsAndMessaging::PostMessageW;

fn arg_value(args: &[String], name: &str) -> Option<String> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned()
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--install-update") {
        std::process::exit(run_installer(&args));
    }
    #[cfg(debug_assertions)]
    if let Some(dir) = arg_value(&args, "--ui-preview") {
        preview::run(dir.into());
        return;
    }
    #[cfg(debug_assertions)]
    if let Some(name) = arg_value(&args, "--drive-test") {
        std::process::exit(preview::drive_test(name));
    }
    if args.iter().any(|a| a == "--self-test") {
        let ok = self_test();
        std::process::exit(if ok { 0 } else { 1 });
    }

    // One instance per user session: a second launch just opens the first.
    let name = win::wide("Local\\TransparencyApp.Instance");
    let _mutex = unsafe { CreateMutexW(std::ptr::null(), 0, name.as_ptr()) };
    if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        for _ in 0..20 {
            if let Some(h) = shell::find_existing() {
                unsafe { PostMessageW(win::h(h), shell::WM_SHOW_MAIN, 0, 0) };
                return;
            }
            std::thread::sleep(Duration::from_millis(150));
        }
        return;
    }

    let minimized = args.iter().any(|a| a == "--minimized");
    if let Err(e) = run(minimized) {
        log::warn(format!("fatal error: {e}"));
    }
}

/// Map the Python versions' device-name based dimmer settings onto stable
/// screen ids. Screens that were dimmed stay dimmed; nothing else is added.
fn migrate_dimmer(config: &config::Store) {
    if config.read().settings.dimmer_migrated {
        return;
    }
    let (selection, levels) = config.read().legacy_dimmer();
    let monitors = win::enum_monitors();
    config.update_quiet(|d| {
        let s = &mut d.settings;
        for m in &monitors {
            let key = m.device.to_uppercase();
            let legacy = levels.get(&key).copied();
            if let Some(level) = legacy {
                s.dimmer_levels.insert(m.id.clone(), level);
            }
            let picked = selection.as_ref().map(|l| l.contains(&key)).unwrap_or(true);
            if picked && legacy.unwrap_or(s.dimmer_intensity) > 0 && !s.dimmer_screens.contains(&m.id) {
                s.dimmer_screens.push(m.id.clone());
            }
        }
        s.dimmer_migrated = true;
    });
    log::info(format!(
        "migrated dimmer settings to stable screen ids: {:?}",
        config.read().settings.dimmer_screens
    ));
}

fn run(start_hidden: bool) -> Result<(), slint::PlatformError> {
    log::info(format!("starting version {}", updater::VERSION));
    let config = config::Store::open(paths::config_path());
    migrate_dimmer(&config);
    let engine = engine::Engine::new(config.clone(), Some(paths::session_path()));
    let core = app::Core::new(config.clone(), engine.clone());

    let settings = config.settings();
    if !settings.transparency_on {
        engine.set_paused(true);
    }
    if settings.focus.enabled {
        engine.set_focus_mode(true);
    }
    engine.start();
    startup::refresh_if_enabled();

    let window = AppWindow::new()?;
    *core.ui.lock().unwrap() = Some(window.as_weak());
    ui::wire(&core, &window);
    let shell_thread = shell::start(core.clone());

    if !start_hidden {
        window.show()?;
        ui::bring_to_front(&window);
    }
    let _update_timer = slint::Timer::default();
    if settings.check_updates_on_startup {
        let c = core.clone();
        _update_timer.start(slint::TimerMode::SingleShot, Duration::from_millis(2500), move || {
            c.check_for_updates(false)
        });
    }

    slint::run_event_loop_until_quit()?;

    log::info("shutting down");
    let _ = window.hide();
    core.send_shell(shell::ShellCmd::Quit);
    let _ = shell_thread.join();
    engine.stop();
    config.close();
    Ok(())
}

fn run_installer(args: &[String]) -> i32 {
    let (Some(target), Some(pid), Some(sha)) = (
        arg_value(args, "--target"),
        arg_value(args, "--wait-pid").and_then(|p| p.parse::<u32>().ok()),
        arg_value(args, "--expected-sha256"),
    ) else {
        return 2;
    };
    let version = arg_value(args, "--version").unwrap_or_default();
    let source = paths::current_exe();
    let Ok(window) = InstallerWindow::new() else { return 1 };
    // Match the app's look.
    if let Ok(text) = std::fs::read_to_string(paths::config_path()) {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
            let s = &v["settings"];
            ui::apply_theme_global(
                &window.global::<Theme>(),
                s["theme"].as_str().unwrap_or("dark"),
                s["accent"].as_str().unwrap_or("blue"),
            );
        }
    }
    window.set_version(version.into());
    window.on_close_clicked(|| {
        let _ = slint::quit_event_loop();
    });
    let failed = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let weak = window.as_weak();
    let failed2 = failed.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(200));
        let w = weak.clone();
        let result = updater::replace_and_restart(
            &source,
            std::path::Path::new(&target),
            pid,
            &sha,
            move |p, msg| {
                let msg = msg.to_string();
                let w = w.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(win) = w.upgrade() {
                        win.set_progress(p);
                        win.set_status(msg.into());
                    }
                });
            },
        );
        match result {
            Ok(()) => {
                std::thread::sleep(Duration::from_millis(1100));
                let _ = slint::invoke_from_event_loop(|| {
                    let _ = slint::quit_event_loop();
                });
            }
            Err(e) => {
                failed2.store(true, std::sync::atomic::Ordering::SeqCst);
                log::warn(format!("update install failed: {e}"));
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(win) = weak.upgrade() {
                        win.set_failed(true);
                        win.set_status("The update was not installed.".into());
                        win.set_detail(e.into());
                    }
                });
            }
        }
    });
    let _ = window.run();
    if failed.load(std::sync::atomic::Ordering::SeqCst) {
        1
    } else {
        0
    }
}

/// Bring the app up without a user, exercise it and tear it down (CI runs
/// this against the built exe to prove the shipped bytes start).
fn self_test() -> bool {
    let result = std::panic::catch_unwind(|| -> Result<(), String> {
        let dir = std::env::temp_dir().join(format!("ta-selftest-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let config = config::Store::open(dir.join("config.json"));
        let engine = engine::Engine::new(config.clone(), None);
        let core = app::Core::new(config.clone(), engine.clone());
        engine.start();
        let window = AppWindow::new().map_err(|e| e.to_string())?;
        *core.ui.lock().unwrap() = Some(window.as_weak());
        ui::wire(&core, &window);
        for page in 0..4 {
            window.set_page(page);
            window.invoke_page_shown(page);
        }
        let _ = win::enum_monitors();
        engine.stop();
        config.close();
        let _ = std::fs::remove_dir_all(&dir);
        Ok(())
    });
    matches!(result, Ok(Ok(())))
}

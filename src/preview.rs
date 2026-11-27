//! Debug builds only: render every page with sample data and save
//! screenshots (`TransparencyApp --ui-preview <dir>`). Touches nothing on the
//! system: no engine hooks, no tray, no hotkeys, no dimming.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use slint::ComponentHandle;
use windows_sys::Win32::Foundation::RECT;
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::Storage::Xps::PrintWindow;
use windows_sys::Win32::UI::WindowsAndMessaging::{GetClientRect, SetWindowPos, SWP_NOZORDER};

use crate::config::{MatchMode, Rule, Store};
use crate::{app, engine, ui, win, AppWindow};

fn capture(hwnd: win::Hwnd, path: &PathBuf) {
    unsafe {
        let mut r = RECT { left: 0, top: 0, right: 0, bottom: 0 };
        GetClientRect(win::h(hwnd), &mut r);
        let (w, h) = (r.right, r.bottom);
        let screen = GetDC(std::ptr::null_mut());
        let mem = CreateCompatibleDC(screen);
        let bmp = CreateCompatibleBitmap(screen, w, h);
        let old = SelectObject(mem, bmp);
        PrintWindow(win::h(hwnd), mem, 3);
        let mut info: BITMAPINFO = std::mem::zeroed();
        info.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
        info.bmiHeader.biWidth = w;
        info.bmiHeader.biHeight = -h;
        info.bmiHeader.biPlanes = 1;
        info.bmiHeader.biBitCount = 32;
        let mut buf = vec![0u8; (w * h * 4) as usize];
        GetDIBits(mem, bmp, 0, h as u32, buf.as_mut_ptr() as *mut _, &mut info, DIB_RGB_COLORS);
        SelectObject(mem, old);
        DeleteObject(bmp);
        DeleteDC(mem);
        ReleaseDC(std::ptr::null_mut(), screen);
        let mut out = Vec::with_capacity(54 + buf.len());
        let size = 54 + buf.len() as u32;
        out.extend_from_slice(b"BM");
        out.extend_from_slice(&size.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&54u32.to_le_bytes());
        out.extend_from_slice(&40u32.to_le_bytes());
        out.extend_from_slice(&w.to_le_bytes());
        out.extend_from_slice(&(-h).to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&32u16.to_le_bytes());
        out.extend_from_slice(&[0u8; 24]);
        out.extend_from_slice(&buf);
        let _ = std::fs::write(path, out);
    }
}

pub fn run(dir: PathBuf) {
    let _ = std::fs::create_dir_all(&dir);
    let cfg_dir = std::env::temp_dir().join(format!("ta-preview-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&cfg_dir);
    let config = Store::open(cfg_dir.join("config.json"));
    config.update_quiet(|d| {
        let mut a = Rule::new("Google Chrome", 128, MatchMode::Title);
        a.topmost = true;
        let mut b = Rule::new("code.exe", 217, MatchMode::Process);
        b.click_through = true;
        let mut c = Rule::new("Spotify Premium", 166, MatchMode::TitleExact);
        c.enabled = false;
        d.rules = vec![a, b, c];
        d.presets.insert("Work".into(), d.rules.clone());
        d.presets.insert("Movie night".into(), vec![]);
        d.settings.focus.active_opacity = 245;
        d.settings.focus.background_opacity = 96;
        d.settings.dimmer_enabled = true;
        d.settings.dimmer_migrated = true;
        if let Some(m) = win::enum_monitors().first() {
            d.settings.dimmer_screens = vec![m.id.clone()];
            d.settings.dimmer_levels.insert(m.id.clone(), 120);
        }
    });
    let engine = engine::Engine::new(config.clone(), None);
    let core = app::Core::new(config.clone(), engine);
    let window = AppWindow::new().unwrap();
    *core.ui.lock().unwrap() = Some(window.as_weak());
    ui::wire(&core, &window);
    window.set_affected(3);
    window.show().unwrap();

    let steps: Vec<(&str, Box<dyn Fn(&Arc<app::Core>, &AppWindow)>)> = vec![
        ("1-rules", Box::new(|_, w| w.set_page(0))),
        ("2-focus", Box::new(|c, w| { w.set_page(1); w.set_focus_on(true); let _ = c; })),
        ("3-dimmer", Box::new(|c, w| { w.set_page(2); w.invoke_page_shown(2); let _ = c; })),
        ("4-settings", Box::new(|_, w| { w.set_page(3); w.invoke_page_shown(3); })),
        ("5-picker", Box::new(|_, w| { w.set_page(0); w.invoke_open_picker(); })),
        ("6-light-rules", Box::new(|_, w| { w.set_picker_open(false); w.invoke_set_theme(1); w.set_page(0); })),
        ("7-light-dimmer", Box::new(|_, w| { w.set_page(2); w.invoke_set_accent(1); })),
        ("8-empty", Box::new(|c, w| {
            c.config.update_quiet(|d| d.rules.clear());
            w.invoke_set_theme(0);
            w.invoke_set_accent(0);
            ui::push_rules(c, w);
            w.set_page(0);
        })),
        ("9-capture", Box::new(|_, w| {
            w.set_page(3);
            w.set_capture_name("Toggle screen dimmer".into());
            w.set_capture_message("Hold Ctrl, Alt or Shift and press a key.".into());
            w.set_capture_open(true);
        })),
    ];
    let steps = std::rc::Rc::new(steps);
    let index = std::rc::Rc::new(std::cell::Cell::new(0usize));
    let timer = slint::Timer::default();
    let weak = window.as_weak();
    let c = core.clone();
    let placed = std::cell::Cell::new(false);
    timer.start(slint::TimerMode::Repeated, Duration::from_millis(700), move || {
        let Some(w) = weak.upgrade() else { return };
        let hwnd = win::top_level_windows().into_iter().find(|&h| {
            win::window_pid(h) == win::own_pid() && win::window_title(h) == "Transparency App"
        });
        let Some(hwnd) = hwnd else { return };
        if !placed.get() {
            unsafe { SetWindowPos(win::h(hwnd), std::ptr::null_mut(), 120, 80, 1180, 820, SWP_NOZORDER) };
            placed.set(true);
            return;
        }
        let i = index.get();
        if i > 0 {
            capture(hwnd, &dir.join(format!("{}.bmp", steps[i - 1].0)));
        }
        if i >= steps.len() {
            let _ = slint::quit_event_loop();
            return;
        }
        (steps[i].1)(&c, &w);
        index.set(i + 1);
    });
    let _ = slint::run_event_loop_until_quit();
    config.close();
    let _ = std::fs::remove_dir_all(&cfg_dir);
}

/// Debug builds only: drive the real core against one screen and check the
/// gamma ramp is dimmed, survives a display change without compounding, and
/// is restored exactly. `TransparencyApp --drive-test <name fragment>`
pub fn drive_test(fragment: String) -> i32 {
    use crate::gamma::{looks_dimmed, ramps_match, scaled, GammaIo, GdiGamma};
    use crate::shell::ShellCmd;
    let say = |m: &str| println!("{m}");
    let monitors = win::enum_monitors();
    let Some(target) = monitors
        .iter()
        .find(|m| m.label().to_lowercase().contains(&fragment.to_lowercase()) && m.kind != win::MonitorKind::Virtual)
        .cloned()
    else {
        say("FAIL: no matching physical screen");
        return 2;
    };
    say(&format!("target: {} {} id={}", target.label(), target.device, target.id));
    let Some(original) = GdiGamma.get(&target.device) else {
        say("FAIL: cannot read gamma");
        return 2;
    };
    say(&format!("original looks dimmed: {:?}", looks_dimmed(&original)));

    let cfg_dir = std::env::temp_dir().join(format!("ta-drive-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&cfg_dir);
    let config = Store::open(cfg_dir.join("config.json"));
    config.update_quiet(|d| {
        d.settings.hotkeys_enabled = false;
        d.settings.transparency_on = false;
        d.settings.dimmer_migrated = true;
        d.settings.dimmer_screens = vec![target.id.clone()];
        d.settings.dimmer_levels.insert(target.id.clone(), 100);
    });
    let engine = engine::Engine::new(config.clone(), None);
    let core = app::Core::new(config.clone(), engine);
    let shell_thread = crate::shell::start(core.clone());
    let wait = |ms| std::thread::sleep(Duration::from_millis(ms));
    let read = || GdiGamma.get(&target.device).unwrap();
    let mut ok = true;
    let mut check = |name: &str, cond: bool| {
        say(&format!("{} {name}", if cond { "PASS" } else { "FAIL" }));
        ok &= cond;
    };

    check("starts untouched", ramps_match(&read(), &original));
    config.update_quiet(|d| d.settings.dimmer_enabled = true);
    core.send_shell(ShellCmd::SyncDimmer);
    wait(800);
    let dimmed = scaled(&original, 100);
    check("dimmed to 100/255 by hardware", ramps_match(&read(), &dimmed));
    check(
        "only the chosen screen is dimmed",
        core.dimmer_modes.lock().unwrap().keys().all(|k| k == &target.id),
    );
    check("restore is recorded on disk", crate::paths::gamma_state_path().exists());

    // What a lid open / resume / replug delivers.
    let shell = crate::shell::find_existing().unwrap_or(0);
    unsafe {
        windows_sys::Win32::UI::WindowsAndMessaging::PostMessageW(
            win::h(shell),
            windows_sys::Win32::UI::WindowsAndMessaging::WM_DISPLAYCHANGE,
            32,
            0,
        );
    }
    wait(5000);
    check("display change does not compound the dim", ramps_match(&read(), &dimmed));

    config.update_quiet(|d| d.settings.dimmer_enabled = false);
    core.send_shell(ShellCmd::SyncDimmer);
    wait(800);
    check("switching off restores the exact original", ramps_match(&read(), &original));
    check("nothing left owed", !crate::paths::gamma_state_path().exists());

    // Crash safety: dim, then quit through the normal path.
    config.update_quiet(|d| d.settings.dimmer_enabled = true);
    core.send_shell(ShellCmd::SyncDimmer);
    wait(800);
    core.send_shell(ShellCmd::Quit);
    let _ = shell_thread.join();
    check("quitting while dimmed restores the original", ramps_match(&read(), &original));

    // Safety net: never leave this screen dim, whatever happened above.
    if !ramps_match(&read(), &original) {
        GdiGamma.set(&target.device, &original);
    }
    config.close();
    let _ = std::fs::remove_dir_all(&cfg_dir);
    say(if ok { "RESULT: PASS" } else { "RESULT: FAIL" });
    if ok { 0 } else { 1 }
}

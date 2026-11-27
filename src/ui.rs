//! Glue between the Slint window and the core. Runs on the UI thread only.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use slint::{Color, ComponentHandle, Model, ModelRc, SharedString, VecModel};

use crate::app::Core;
use crate::config::{MatchMode, Rule, ACCENTS, MAX_DIM, MIN_FOCUS_ALPHA};
use crate::shell::ShellCmd;
use crate::win::{self, MonitorKind, WindowInfo};
use crate::{hotkeys, startup, AppWindow, HotkeyItem, MonitorItem, PickItem, RuleItem, Theme};

const ACCENT_COLORS: [(u32, u32); 7] = [
    (0x5B8CFF, 0x8A6BFF), // blue
    (0xA06BFF, 0xE06BD8), // purple
    (0x34C47C, 0x2DBFB8), // green
    (0xF0883E, 0xF2C14E), // orange
    (0xF065B8, 0xA06BFF), // pink
    (0xE5534B, 0xF0883E), // red
    (0x2DBFB8, 0x5B8CFF), // teal
];

fn rgb(c: u32) -> Color {
    Color::from_rgb_u8((c >> 16) as u8, (c >> 8) as u8, c as u8)
}

#[derive(Default)]
struct UiState {
    rules: Option<Rc<VecModel<RuleItem>>>,
    monitors: Option<Rc<VecModel<MonitorItem>>>,
    picker_all: Vec<WindowInfo>,
    picker_shown: Vec<WindowInfo>,
    capture_action: Option<String>,
    toast_timer: Option<slint::Timer>,
    tick: Option<slint::Timer>,
}

thread_local! {
    static STATE: RefCell<UiState> = RefCell::new(UiState::default());
}

fn rule_item(r: &Rule) -> RuleItem {
    RuleItem {
        id: r.id.clone().into(),
        pattern: r.pattern.clone().into(),
        mode: r.match_mode.index(),
        opacity: r.opacity as i32,
        enabled: r.enabled,
        click: r.click_through,
        topmost: r.topmost,
    }
}

pub fn system_prefers_light() -> bool {
    use windows_sys::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_DWORD};
    let key = win::wide(r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize");
    let name = win::wide("AppsUseLightTheme");
    let mut value: u32 = 0;
    let mut size = 4u32;
    let rc = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            key.as_ptr(),
            name.as_ptr(),
            RRF_RT_REG_DWORD,
            std::ptr::null_mut(),
            &mut value as *mut u32 as *mut _,
            &mut size,
        )
    };
    rc == 0 && value == 1
}

pub fn apply_theme_global(theme: &Theme, mode: &str, accent: &str) {
    let dark = match mode {
        "light" => false,
        "system" => !system_prefers_light(),
        _ => true,
    };
    let idx = ACCENTS.iter().position(|a| *a == accent).unwrap_or(0);
    let (a, b) = ACCENT_COLORS[idx];
    theme.set_dark(dark);
    theme.set_accent(rgb(a));
    theme.set_accent_2(rgb(b));
}

fn apply_theme(core: &Arc<Core>, ui: &AppWindow) {
    let s = core.config.settings();
    apply_theme_global(&ui.global::<Theme>(), &s.theme, &s.accent);
    set_dark_titlebar(ui.global::<Theme>().get_dark());
}

/// Ask DWM for a dark (or light) title bar to match the theme.
fn set_dark_titlebar(dark: bool) {
    use windows_sys::Win32::Graphics::Dwm::DwmSetWindowAttribute;
    if let Some(hwnd) = main_hwnd() {
        let v: i32 = dark as i32;
        unsafe {
            DwmSetWindowAttribute(win::h(hwnd), 20, &v as *const i32 as *const _, 4);
        }
    }
}

/// The main window's HWND (our process, our title, not the shell window).
fn main_hwnd() -> Option<win::Hwnd> {
    let pid = win::own_pid();
    win::top_level_windows().into_iter().find(|&h| {
        win::window_pid(h) == pid
            && win::window_title(h) == "Transparency App"
            && win::window_class(h) != crate::shell::SHELL_CLASS
    })
}

pub fn bring_to_front(_ui: &AppWindow) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{SetForegroundWindow, ShowWindow, SW_RESTORE};
    if let Some(h) = main_hwnd() {
        unsafe {
            ShowWindow(win::h(h), SW_RESTORE);
            SetForegroundWindow(win::h(h));
        }
    }
    set_dark_titlebar(_ui.global::<Theme>().get_dark());
}

pub fn show_toast(ui: &AppWindow, text: &str) {
    ui.set_toast(text.into());
    ui.set_toast_visible(true);
    let weak = ui.as_weak();
    STATE.with(|s| {
        let mut s = s.borrow_mut();
        let timer = s.toast_timer.get_or_insert_with(slint::Timer::default);
        timer.start(slint::TimerMode::SingleShot, Duration::from_millis(2200), move || {
            if let Some(ui) = weak.upgrade() {
                ui.set_toast_visible(false);
            }
        });
    });
}

pub fn push_rules(core: &Arc<Core>, ui: &AppWindow) {
    let filter = ui.get_filter().to_lowercase();
    let d = core.config.read();
    let items: Vec<RuleItem> = d
        .rules
        .iter()
        .filter(|r| filter.is_empty() || r.pattern.to_lowercase().contains(&filter))
        .map(rule_item)
        .collect();
    let count = d.rules.len() as i32;
    let presets: Vec<SharedString> = d.presets.keys().map(|k| k.into()).collect();
    drop(d);
    STATE.with(|s| {
        let mut s = s.borrow_mut();
        let model = s.rules.get_or_insert_with(|| Rc::new(VecModel::default())).clone();
        model.set_vec(items);
        ui.set_rules(ModelRc::from(model));
    });
    ui.set_rule_count(count);
    ui.set_presets(ModelRc::new(VecModel::from(presets)));
}

/// Update one rule's row in place (keeps focus in its text field).
fn update_rule_row(core: &Arc<Core>, id: &str) {
    let rule = core.config.read().rules.iter().find(|r| r.id == id).cloned();
    STATE.with(|s| {
        let s = s.borrow();
        let (Some(model), Some(rule)) = (s.rules.as_ref(), rule) else { return };
        for i in 0..model.row_count() {
            if model.row_data(i).map(|r| r.id == id).unwrap_or(false) {
                model.set_row_data(i, rule_item(&rule));
                break;
            }
        }
    });
}

pub fn push_monitors(core: &Arc<Core>, ui: &AppWindow) {
    let s = core.config.settings();
    let modes = core.dimmer_modes.lock().unwrap().clone();
    let items: Vec<MonitorItem> = win::enum_monitors()
        .into_iter()
        .map(|m| {
            let (_, _, w, h) = m.rect;
            let selected = s.dimmer_screens.iter().any(|id| id == &m.id);
            MonitorItem {
                id: m.id.clone().into(),
                name: m.label().into(),
                detail: format!("{w} × {h}").into(),
                kind: match m.kind {
                    MonitorKind::External => 0,
                    MonitorKind::BuiltIn => 1,
                    MonitorKind::Virtual => 2,
                },
                primary: m.primary,
                selected,
                level: s.level_for(&m.id) as i32,
                mode: modes.get(&m.id).map(|x| x.label()).unwrap_or("").into(),
                aspect: if h > 0 { (w as f32 / h as f32).clamp(0.5, 4.0) } else { 1.6 },
            }
        })
        .collect();
    STATE.with(|st| {
        let mut st = st.borrow_mut();
        let model = st.monitors.get_or_insert_with(|| Rc::new(VecModel::default())).clone();
        // Update rows in place when the set of screens is unchanged so a
        // slider being dragged is not rebuilt under the pointer.
        let same = model.row_count() == items.len()
            && items
                .iter()
                .enumerate()
                .all(|(i, it)| model.row_data(i).map(|r| r.id == it.id).unwrap_or(false));
        if same {
            for (i, it) in items.into_iter().enumerate() {
                if model.row_data(i).as_ref() != Some(&it) {
                    model.set_row_data(i, it);
                }
            }
        } else {
            model.set_vec(items);
        }
        ui.set_monitors(ModelRc::from(model));
    });
}

fn push_hotkeys(core: &Arc<Core>, ui: &AppWindow) {
    let s = core.config.settings();
    let failed = core.hotkey_failures.lock().unwrap().clone();
    let mut dimmer_combo = String::new();
    let items: Vec<HotkeyItem> = hotkeys::DEFS
        .iter()
        .map(|(key, label, _id, default, action)| {
            let combo = s
                .hotkeys
                .get(*key)
                .filter(|c| hotkeys::parse(c).is_some())
                .cloned()
                .unwrap_or_else(|| default.to_string());
            if *action == hotkeys::Action::ToggleDimmer && s.hotkeys_enabled {
                dimmer_combo = hotkeys::format(&combo);
            }
            let keys: Vec<SharedString> = hotkeys::display_parts(&combo).into_iter().map(Into::into).collect();
            HotkeyItem {
                action: (*key).into(),
                name: (*label).into(),
                keys: ModelRc::new(VecModel::from(keys)),
                ok: !(s.hotkeys_enabled && failed.iter().any(|f| f == key)),
            }
        })
        .collect();
    ui.set_hotkeys(ModelRc::new(VecModel::from(items)));
    ui.set_dimmer_hotkey(dimmer_combo.into());
}

pub fn push_state(core: &Arc<Core>, ui: &AppWindow) {
    let s = core.config.settings();
    ui.set_transparency_on(!core.engine.paused());
    ui.set_focus_on(core.engine.focus_mode());
    ui.set_dimmer_on(s.dimmer_enabled);
    ui.set_affected(core.engine.affected() as i32);
    ui.set_focus_active(s.focus.active_opacity as i32);
    ui.set_focus_bg(s.focus.background_opacity as i32);
    ui.set_theme_mode(match s.theme.as_str() {
        "light" => 1,
        "system" => 2,
        _ => 0,
    });
    ui.set_accent_index(ACCENTS.iter().position(|a| *a == s.accent).unwrap_or(0) as i32);
    ui.set_startup(startup::is_enabled());
    ui.set_hotkeys_enabled(s.hotkeys_enabled);
    ui.set_auto_update(s.check_updates_on_startup);
    {
        let u = core.update.lock().unwrap();
        ui.set_update_state(u.state.clone().into());
        ui.set_update_message(u.message.clone().into());
    }
    push_hotkeys(core, ui);
    let rule_count = core.config.read().rules.len() as i32;
    if rule_count != ui.get_rule_count() {
        push_rules(core, ui);
    }
}

fn refresh_picker(ui: &AppWindow) {
    let own = win::own_pid();
    let mut seen = std::collections::HashSet::new();
    let mut all: Vec<WindowInfo> = win::enum_app_windows()
        .into_iter()
        .filter(|w| w.pid != own)
        .filter(|w| seen.insert((w.title.clone(), w.process.clone())))
        .collect();
    all.sort_by(|a, b| (a.process.as_str(), a.title.to_lowercase()).cmp(&(b.process.as_str(), b.title.to_lowercase())));
    STATE.with(|s| s.borrow_mut().picker_all = all);
    filter_picker(ui);
}

fn filter_picker(ui: &AppWindow) {
    let q = ui.get_picker_query().to_lowercase();
    STATE.with(|s| {
        let mut s = s.borrow_mut();
        let shown: Vec<WindowInfo> = s
            .picker_all
            .iter()
            .filter(|w| q.is_empty() || w.title.to_lowercase().contains(&q) || w.process.contains(&q))
            .cloned()
            .collect();
        let items: Vec<PickItem> = shown
            .iter()
            .map(|w| PickItem { title: w.title.clone().into(), process: w.process.clone().into() })
            .collect();
        s.picker_shown = shown;
        ui.set_picker_items(ModelRc::new(VecModel::from(items)));
    });
}

fn is_modifier_key(text: &str) -> bool {
    use slint::platform::Key;
    let Some(ch) = text.chars().next() else { return true };
    [Key::Control, Key::ControlR, Key::Shift, Key::ShiftR, Key::Alt, Key::AltGr, Key::Meta, Key::MetaR]
        .iter()
        .any(|k| char::from(k.clone()) == ch)
}

pub fn wire(core: &Arc<Core>, ui: &AppWindow) {
    ui.set_version(crate::updater::VERSION.into());
    ui.set_accent_colors(ModelRc::new(VecModel::from(
        ACCENT_COLORS.iter().map(|(a, _)| rgb(*a)).collect::<Vec<_>>(),
    )));
    apply_theme(core, ui);
    push_rules(core, ui);
    push_state(core, ui);
    push_monitors(core, ui);

    // Keep the "N windows affected" line live while the window is open.
    let weak = ui.as_weak();
    let c = core.clone();
    let tick = slint::Timer::default();
    tick.start(slint::TimerMode::Repeated, Duration::from_millis(1000), move || {
        if let Some(ui) = weak.upgrade() {
            if ui.window().is_visible() {
                ui.set_affected(c.engine.affected() as i32);
            }
        }
    });
    STATE.with(|s| s.borrow_mut().tick = Some(tick));

    macro_rules! on {
        ($setter:ident, |$core:ident, $ui:ident $(, $arg:ident : $ty:ty)*| $body:block) => {{
            let core = core.clone();
            let weak = ui.as_weak();
            ui.$setter(move |$($arg: $ty),*| {
                let $core = &core;
                let Some($ui) = weak.upgrade() else { return Default::default() };
                let $ui = &$ui;
                $body
            });
        }};
    }

    on!(on_toggle_transparency, |core, _ui, v: bool| { core.set_paused(!v); });
    on!(on_toggle_focus, |core, _ui, v: bool| { core.set_focus(v); });
    on!(on_toggle_dimmer, |core, ui, v: bool| {
        core.set_dimmer(v);
        ui.set_dimmer_on(v);
    });
    on!(on_focus_active_changed, |core, ui, v: i32| {
        let v = v.clamp(MIN_FOCUS_ALPHA as i32, 255);
        ui.set_focus_active(v);
        core.config.update(|d| d.settings.focus.active_opacity = v as u8);
    });
    on!(on_focus_bg_changed, |core, ui, v: i32| {
        let v = v.clamp(MIN_FOCUS_ALPHA as i32, 255);
        ui.set_focus_bg(v);
        core.config.update(|d| d.settings.focus.background_opacity = v as u8);
    });

    fn edit_rule(core: &Arc<Core>, id: &str, f: impl FnOnce(&mut Rule)) {
        core.config.update(|d| {
            if let Some(r) = d.rules.iter_mut().find(|r| r.id == id) {
                f(r);
            }
        });
        update_rule_row(core, id);
    }
    on!(on_rule_pattern, |core, _ui, id: SharedString, text: SharedString| {
        let t = text.trim().to_string();
        if !t.is_empty() {
            core.config.update(|d| {
                if let Some(r) = d.rules.iter_mut().find(|r| r.id == id.as_str()) {
                    r.pattern = t;
                }
            });
        }
    });
    on!(on_rule_mode, |core, _ui, id: SharedString, m: i32| {
        edit_rule(core, &id, |r| r.match_mode = MatchMode::from_index(m));
    });
    on!(on_rule_opacity, |core, _ui, id: SharedString, v: i32| {
        edit_rule(core, &id, |r| r.opacity = v.clamp(0, 255) as u8);
    });
    on!(on_rule_enabled, |core, _ui, id: SharedString, v: bool| {
        edit_rule(core, &id, |r| r.enabled = v);
    });
    on!(on_rule_click, |core, _ui, id: SharedString, v: bool| {
        edit_rule(core, &id, |r| r.click_through = v);
    });
    on!(on_rule_topmost, |core, _ui, id: SharedString, v: bool| {
        edit_rule(core, &id, |r| r.topmost = v);
    });
    on!(on_rule_delete, |core, ui, id: SharedString| {
        core.config.update(|d| d.rules.retain(|r| r.id != id.as_str()));
        push_rules(core, ui);
        show_toast(ui, "Rule deleted");
    });
    on!(on_add_manual, |core, ui| {
        core.config.update(|d| d.rules.insert(0, Rule::new("New rule", 220, MatchMode::Title)));
        ui.set_filter("".into());
        push_rules(core, ui);
    });
    on!(on_filter_changed, |core, ui, _t: SharedString| { push_rules(core, ui); });

    on!(on_open_picker, |_core, ui| {
        ui.set_picker_query("".into());
        refresh_picker(ui);
        ui.set_picker_open(true);
    });
    on!(on_picker_refresh, |_core, ui| { refresh_picker(ui); });
    on!(on_picker_search, |_core, ui, _q: SharedString| { filter_picker(ui); });
    on!(on_picker_pick, |core, ui, i: i32| {
        let info = STATE.with(|s| s.borrow().picker_shown.get(i as usize).cloned());
        let Some(info) = info else { return };
        let by_app = ui.get_picker_mode() == 1;
        let pattern = if by_app { info.process.clone() } else { info.title.clone() };
        if pattern.is_empty() {
            return;
        }
        let mode = if by_app { MatchMode::Process } else { MatchMode::Title };
        core.config.update(|d| d.rules.insert(0, Rule::new(&pattern, if by_app { 230 } else { 220 }, mode)));
        ui.set_picker_open(false);
        ui.set_filter("".into());
        push_rules(core, ui);
        show_toast(ui, &format!("Rule added for {pattern}"));
    });

    on!(on_preset_apply, |core, ui, name: SharedString| {
        let ok = core.config.update(|d| match d.presets.get(name.as_str()) {
            Some(rules) => {
                d.rules = rules.clone();
                true
            }
            None => false,
        });
        if ok {
            push_rules(core, ui);
            show_toast(ui, &format!("Applied preset {name}"));
        }
    });
    on!(on_preset_delete, |core, ui, name: SharedString| {
        core.config.update_quiet(|d| d.presets.remove(name.as_str()));
        push_rules(core, ui);
        show_toast(ui, &format!("Deleted preset {name}"));
    });
    on!(on_preset_save, |core, ui, name: SharedString| {
        let name = name.trim().to_string();
        if name.is_empty() {
            return;
        }
        core.config.update_quiet(|d| {
            let rules = d.rules.clone();
            d.presets.insert(name.clone(), rules);
        });
        push_rules(core, ui);
        show_toast(ui, &format!("Saved preset {name}"));
    });

    on!(on_screen_selected, |core, ui, id: SharedString, v: bool| {
        let id = id.to_string();
        core.config.update_quiet(|d| {
            let s = &mut d.settings;
            s.dimmer_migrated = true;
            s.dimmer_screens.retain(|x| x != &id);
            if v {
                s.dimmer_screens.push(id.clone());
                let level = s.level_for(&id);
                // A screen switched on at 0% would look broken: start it at
                // a gentle level instead.
                s.dimmer_levels.insert(id.clone(), if level == 0 { 80 } else { level });
            }
        });
        core.send_shell(ShellCmd::SyncDimmer);
        push_monitors(core, ui);
    });
    on!(on_screen_level, |core, ui, id: SharedString, v: i32| {
        let id = id.to_string();
        core.config.update_quiet(|d| {
            d.settings.dimmer_migrated = true;
            d.settings.dimmer_levels.insert(id.clone(), v.clamp(0, MAX_DIM as i32) as u8);
        });
        core.send_shell(ShellCmd::SyncDimmer);
        push_monitors(core, ui);
    });

    on!(on_set_theme, |core, ui, i: i32| {
        let mode = ["dark", "light", "system"][i.clamp(0, 2) as usize];
        core.config.update_quiet(|d| d.settings.theme = mode.into());
        ui.set_theme_mode(i);
        apply_theme(core, ui);
    });
    on!(on_set_accent, |core, ui, i: i32| {
        let name = ACCENTS[i.clamp(0, ACCENTS.len() as i32 - 1) as usize];
        core.config.update_quiet(|d| d.settings.accent = name.into());
        ui.set_accent_index(i);
        apply_theme(core, ui);
    });
    on!(on_set_startup, |core, ui, v: bool| {
        let ok = if v { startup::enable() } else { startup::disable() };
        core.config.update_quiet(|d| d.settings.start_minimized = v);
        ui.set_startup(startup::is_enabled());
        if !ok {
            show_toast(ui, "Could not change the sign-in setting");
        }
    });
    on!(on_set_hotkeys_enabled, |core, ui, v: bool| {
        core.config.update_quiet(|d| d.settings.hotkeys_enabled = v);
        ui.set_hotkeys_enabled(v);
        core.send_shell(ShellCmd::RegisterHotkeys);
    });
    on!(on_set_auto_update, |core, ui, v: bool| {
        core.config.update_quiet(|d| d.settings.check_updates_on_startup = v);
        ui.set_auto_update(v);
    });
    on!(on_check_updates, |core, _ui| { core.check_for_updates(true); });
    on!(on_install_update, |core, _ui| { core.install_update(); });

    on!(on_capture_begin, |core, _ui, action: SharedString| {
        STATE.with(|s| s.borrow_mut().capture_action = Some(action.to_string()));
        core.capturing.store(true, Ordering::SeqCst);
        core.send_shell(ShellCmd::RegisterHotkeys);
    });
    on!(on_capture_cancel, |core, _ui| {
        STATE.with(|s| s.borrow_mut().capture_action = None);
        core.capturing.store(false, Ordering::SeqCst);
        core.send_shell(ShellCmd::RegisterHotkeys);
    });
    on!(on_capture_key, |core, ui, text: SharedString, ctrl: bool, alt: bool, shift: bool, meta: bool| {
        if is_modifier_key(&text) {
            ui.set_capture_message("Now press a letter, number, F key or arrow.".into());
            ui.set_capture_error(false);
            return false;
        }
        let Some(combo) = hotkeys::from_key_event(&text, ctrl, alt, shift, meta) else {
            ui.set_capture_message("Use Ctrl, Alt or Shift together with a letter, number, F key or arrow.".into());
            ui.set_capture_error(true);
            return false;
        };
        let Some(action) = STATE.with(|s| s.borrow().capture_action.clone()) else { return true };
        let parsed = hotkeys::parse(&combo);
        let settings = core.config.settings();
        let taken = hotkeys::DEFS.iter().any(|(key, _, _, default, _)| {
            *key != action.as_str()
                && hotkeys::parse(settings.hotkeys.get(*key).map(String::as_str).unwrap_or(default)) == parsed
        });
        if taken {
            ui.set_capture_message(format!("{} is already used by another shortcut.", hotkeys::format(&combo)).into());
            ui.set_capture_error(true);
            return false;
        }
        core.config.update_quiet(|d| {
            d.settings.hotkeys.insert(action.clone(), combo.clone());
        });
        STATE.with(|s| s.borrow_mut().capture_action = None);
        core.capturing.store(false, Ordering::SeqCst);
        core.send_shell(ShellCmd::RegisterHotkeys);
        push_hotkeys(core, ui);
        show_toast(ui, &format!("Shortcut set to {}", hotkeys::format(&combo)));
        true
    });
    on!(on_reset_hotkeys, |core, ui| {
        core.config.update_quiet(|d| d.settings.hotkeys.clear());
        core.send_shell(ShellCmd::RegisterHotkeys);
        push_hotkeys(core, ui);
        show_toast(ui, "Shortcuts reset to defaults");
    });

    on!(on_export_settings, |core, ui| {
        let path = rfd::FileDialog::new()
            .set_title("Export settings")
            .set_file_name("transparency-settings.json")
            .add_filter("JSON", &["json"])
            .save_file();
        if let Some(p) = path {
            if core.config.export_to(&p) {
                show_toast(ui, "Settings exported");
            } else {
                show_toast(ui, "Could not write that file");
            }
        }
    });
    on!(on_import_settings, |core, ui| {
        let path = rfd::FileDialog::new()
            .set_title("Import settings")
            .add_filter("JSON", &["json"])
            .pick_file();
        if let Some(p) = path {
            if core.config.import_from(&p) {
                let s = core.config.settings();
                core.engine.set_paused(!s.transparency_on);
                core.engine.set_focus_mode(s.focus.enabled);
                apply_theme(core, ui);
                push_rules(core, ui);
                push_state(core, ui);
                push_monitors(core, ui);
                core.send_shell(ShellCmd::SyncDimmer);
                core.send_shell(ShellCmd::RegisterHotkeys);
                show_toast(ui, "Settings imported");
            } else {
                show_toast(ui, "That file is not a Transparency App settings file");
            }
        }
    });
    on!(on_restore_all, |core, ui| {
        core.restore_all();
        show_toast(ui, "All windows restored");
    });
    on!(on_page_shown, |core, ui, page: i32| {
        if page == 2 {
            push_monitors(core, ui);
        }
        if page == 3 {
            push_state(core, ui);
        }
    });
}

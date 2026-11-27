//! Configuration model and persistence.
//!
//! Lives in memory, notifies listeners on change and is written to disk
//! atomically on a short debounce. The file format is the same one the
//! Python versions used (`%APPDATA%\TransparencyApp\config.json`), so rules,
//! presets and settings carry over. Unknown keys are kept on save.

use std::collections::BTreeMap;
use std::hash::{BuildHasher, Hasher};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use serde_json::{json, Map, Value};

pub const CONFIG_VERSION: i64 = 2;
pub const MIN_FOCUS_ALPHA: u8 = 20;
pub const MAX_DIM: u8 = 200;
pub const ACCENTS: &[&str] = &["blue", "purple", "green", "orange", "pink", "red", "teal"];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MatchMode {
    Title,
    TitleExact,
    Process,
}

impl MatchMode {
    pub fn as_str(self) -> &'static str {
        match self {
            MatchMode::Title => "title",
            MatchMode::TitleExact => "title_exact",
            MatchMode::Process => "process",
        }
    }
    pub fn parse(s: &str) -> MatchMode {
        match s {
            "title_exact" => MatchMode::TitleExact,
            "process" => MatchMode::Process,
            _ => MatchMode::Title,
        }
    }
    pub fn index(self) -> i32 {
        match self {
            MatchMode::Title => 0,
            MatchMode::TitleExact => 1,
            MatchMode::Process => 2,
        }
    }
    pub fn from_index(i: i32) -> MatchMode {
        match i {
            1 => MatchMode::TitleExact,
            2 => MatchMode::Process,
            _ => MatchMode::Title,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Rule {
    pub id: String,
    pub pattern: String,
    pub opacity: u8,
    pub match_mode: MatchMode,
    pub enabled: bool,
    pub click_through: bool,
    pub topmost: bool,
}

pub fn new_id() -> String {
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    hasher.write_u128(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0),
    );
    format!("{:016x}", hasher.finish())[..12].to_string()
}

impl Rule {
    pub fn new(pattern: &str, opacity: u8, match_mode: MatchMode) -> Rule {
        Rule {
            id: new_id(),
            pattern: pattern.trim().to_string(),
            opacity,
            match_mode,
            enabled: true,
            click_through: false,
            topmost: false,
        }
    }

    pub fn matches(&self, title: &str, process: &str) -> bool {
        if self.pattern.is_empty() {
            return false;
        }
        match self.match_mode {
            MatchMode::Process => {
                let mut pat = self.pattern.to_lowercase();
                if !pat.ends_with(".exe") {
                    pat.push_str(".exe");
                }
                process == pat
            }
            MatchMode::TitleExact => title.to_lowercase() == self.pattern.to_lowercase(),
            MatchMode::Title => title.to_lowercase().contains(&self.pattern.to_lowercase()),
        }
    }

    pub fn to_json(&self) -> Value {
        json!({
            "id": self.id,
            "pattern": self.pattern,
            "match_mode": self.match_mode.as_str(),
            "opacity": self.opacity,
            "enabled": self.enabled,
            "click_through": self.click_through,
            "topmost": self.topmost,
        })
    }

    pub fn from_json(v: &Value) -> Option<Rule> {
        let o = v.as_object()?;
        let pattern = o.get("pattern")?.as_str()?.trim().to_string();
        if pattern.is_empty() {
            return None;
        }
        Some(Rule {
            id: o
                .get("id")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .unwrap_or_else(new_id),
            pattern,
            opacity: clamp(o.get("opacity"), 0, 255, 220) as u8,
            match_mode: MatchMode::parse(o.get("match_mode").and_then(Value::as_str).unwrap_or("")),
            enabled: boolean(o.get("enabled"), true),
            click_through: boolean(o.get("click_through"), false),
            topmost: boolean(o.get("topmost"), false),
        })
    }
}

fn clamp(v: Option<&Value>, lo: i64, hi: i64, default: i64) -> i64 {
    let n = match v {
        Some(Value::Number(n)) => n.as_f64().map(|f| f as i64),
        Some(Value::String(s)) => s.trim().parse::<i64>().ok(),
        Some(Value::Bool(b)) => Some(*b as i64),
        _ => None,
    };
    n.map(|n| n.clamp(lo, hi)).unwrap_or(default)
}

fn boolean(v: Option<&Value>, default: bool) -> bool {
    match v {
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().map(|f| f != 0.0).unwrap_or(default),
        Some(Value::Null) | None => default,
        Some(Value::String(s)) => !s.is_empty(),
        Some(_) => true,
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct FocusMode {
    pub enabled: bool,
    pub active_opacity: u8,
    pub background_opacity: u8,
    pub exclude: Vec<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Settings {
    pub theme: String,
    pub accent: String,
    pub transparency_on: bool,
    pub hotkeys_enabled: bool,
    pub start_minimized: bool,
    pub check_updates_on_startup: bool,
    pub focus: FocusMode,
    /// Level for a screen that has no level of its own yet.
    pub dimmer_intensity: u8,
    pub dimmer_enabled: bool,
    /// Screens to dim, by stable monitor id. Screens not listed (including
    /// any that are connected later) are never dimmed.
    pub dimmer_screens: Vec<String>,
    /// Per-screen level, by stable monitor id.
    pub dimmer_levels: BTreeMap<String, u8>,
    /// False until the old device-name based dimmer settings were migrated.
    pub dimmer_migrated: bool,
    pub hotkeys: BTreeMap<String, String>,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            theme: "dark".into(),
            accent: "blue".into(),
            transparency_on: true,
            hotkeys_enabled: true,
            start_minimized: false,
            check_updates_on_startup: true,
            focus: FocusMode {
                enabled: false,
                active_opacity: 255,
                background_opacity: 120,
                exclude: vec![],
            },
            dimmer_intensity: 160,
            dimmer_enabled: false,
            dimmer_screens: vec![],
            dimmer_levels: BTreeMap::new(),
            dimmer_migrated: false,
            hotkeys: BTreeMap::new(),
        }
    }
}

impl Settings {
    pub fn level_for(&self, monitor_id: &str) -> u8 {
        self.dimmer_levels
            .get(monitor_id)
            .copied()
            .unwrap_or(self.dimmer_intensity)
            .min(MAX_DIM)
    }

    fn from_json(raw: Option<&Value>) -> Settings {
        let mut s = Settings::default();
        let Some(o) = raw.and_then(Value::as_object) else {
            return s;
        };
        if let Some(t) = o.get("theme").and_then(Value::as_str) {
            if ["dark", "light", "system"].contains(&t) {
                s.theme = t.to_string();
            }
        }
        if let Some(a) = o.get("accent").and_then(Value::as_str) {
            if ACCENTS.contains(&a) {
                s.accent = a.to_string();
            }
        }
        s.transparency_on = boolean(o.get("transparency_on"), true);
        s.hotkeys_enabled = boolean(o.get("hotkeys_enabled"), true);
        s.start_minimized = boolean(o.get("start_minimized"), false);
        s.check_updates_on_startup = boolean(o.get("check_updates_on_startup"), true);
        s.dimmer_intensity = clamp(o.get("dimmer_intensity"), 0, MAX_DIM as i64, 160) as u8;
        s.dimmer_enabled = boolean(o.get("dimmer_enabled"), false);
        if let Some(list) = o.get("dimmer_screens").and_then(Value::as_array) {
            s.dimmer_migrated = true;
            s.dimmer_screens = list
                .iter()
                .filter_map(Value::as_str)
                .map(|x| x.trim().to_lowercase())
                .filter(|x| !x.is_empty())
                .collect();
            s.dimmer_screens.dedup();
        }
        if let Some(levels) = o.get("dimmer_levels").and_then(Value::as_object) {
            for (k, v) in levels {
                let k = k.trim().to_lowercase();
                if !k.is_empty() {
                    s.dimmer_levels
                        .insert(k, clamp(Some(v), 0, MAX_DIM as i64, s.dimmer_intensity as i64) as u8);
                }
            }
        }
        if let Some(hk) = o.get("hotkeys").and_then(Value::as_object) {
            for (k, v) in hk {
                if let Some(c) = v.as_str() {
                    let c = c.trim().to_lowercase();
                    if !c.is_empty() {
                        s.hotkeys.insert(k.clone(), c);
                    }
                }
            }
        }
        let fm = o.get("focus_mode").and_then(Value::as_object);
        let fmv = |k: &str| fm.and_then(|m| m.get(k));
        s.focus.enabled = boolean(fmv("enabled"), false);
        s.focus.active_opacity = clamp(fmv("active_opacity"), MIN_FOCUS_ALPHA as i64, 255, 255) as u8;
        s.focus.background_opacity =
            clamp(fmv("background_opacity"), MIN_FOCUS_ALPHA as i64, 255, 120) as u8;
        if let Some(ex) = fmv("exclude").and_then(Value::as_array) {
            s.focus.exclude = ex
                .iter()
                .filter_map(Value::as_str)
                .map(|p| p.trim().to_lowercase())
                .filter(|p| !p.is_empty())
                .collect();
        }
        s
    }

    fn write_json(&self, o: &mut Map<String, Value>) {
        o.insert("theme".into(), json!(self.theme));
        o.insert("accent".into(), json!(self.accent));
        o.insert("transparency_on".into(), json!(self.transparency_on));
        o.insert("hotkeys_enabled".into(), json!(self.hotkeys_enabled));
        o.insert("start_minimized".into(), json!(self.start_minimized));
        o.insert("check_updates_on_startup".into(), json!(self.check_updates_on_startup));
        o.insert(
            "focus_mode".into(),
            json!({
                "enabled": self.focus.enabled,
                "active_opacity": self.focus.active_opacity,
                "background_opacity": self.focus.background_opacity,
                "exclude": self.focus.exclude,
            }),
        );
        o.insert("dimmer_intensity".into(), json!(self.dimmer_intensity));
        o.insert("dimmer_enabled".into(), json!(self.dimmer_enabled));
        if self.dimmer_migrated {
            o.insert("dimmer_screens".into(), json!(self.dimmer_screens));
            o.insert("dimmer_levels".into(), json!(self.dimmer_levels));
        }
        o.insert("hotkeys".into(), json!(self.hotkeys));
    }
}

#[derive(Clone, Debug, Default)]
pub struct Data {
    pub rules: Vec<Rule>,
    pub presets: BTreeMap<String, Vec<Rule>>,
    pub settings: Settings,
    /// The settings object as read from disk, so keys this version does not
    /// know about (including the old device-name dimmer keys) survive a save.
    raw_settings: Map<String, Value>,
    raw_root: Map<String, Value>,
}

impl Data {
    pub fn from_json(v: &Value) -> Data {
        let mut v = v.clone();
        if v.get("windows").is_some() && v.get("rules").is_none() {
            v = migrate_v1(&v);
        }
        let rules = v
            .get("rules")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Rule::from_json).collect())
            .unwrap_or_default();
        let mut presets = BTreeMap::new();
        if let Some(p) = v.get("presets").and_then(Value::as_object) {
            for (name, list) in p {
                if let Some(list) = list.as_array() {
                    presets.insert(name.clone(), list.iter().filter_map(Rule::from_json).collect());
                }
            }
        }
        Data {
            rules,
            presets,
            settings: Settings::from_json(v.get("settings")),
            raw_settings: v
                .get("settings")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default(),
            raw_root: v.as_object().cloned().unwrap_or_default(),
        }
    }

    pub fn to_json(&self) -> Value {
        let mut root = self.raw_root.clone();
        let mut settings = self.raw_settings.clone();
        self.settings.write_json(&mut settings);
        root.insert("version".into(), json!(CONFIG_VERSION));
        root.insert(
            "rules".into(),
            Value::Array(self.rules.iter().map(Rule::to_json).collect()),
        );
        root.insert(
            "presets".into(),
            Value::Object(
                self.presets
                    .iter()
                    .map(|(k, v)| (k.clone(), Value::Array(v.iter().map(Rule::to_json).collect())))
                    .collect(),
            ),
        );
        root.insert("settings".into(), Value::Object(settings));
        Value::Object(root)
    }

    /// The old (Python) dimmer settings, keyed by GDI device name:
    /// (selection: None = "all", per-device levels).
    pub fn legacy_dimmer(&self) -> (Option<Vec<String>>, BTreeMap<String, u8>) {
        let sel = match self.raw_settings.get("dimmer_monitors") {
            Some(Value::Array(a)) => Some(
                a.iter()
                    .filter_map(Value::as_str)
                    .map(|s| s.to_uppercase())
                    .collect(),
            ),
            _ => None,
        };
        let mut levels = BTreeMap::new();
        if let Some(Value::Object(o)) = self.raw_settings.get("dimmer_intensities") {
            for (k, v) in o {
                levels.insert(
                    k.to_uppercase(),
                    clamp(Some(v), 0, MAX_DIM as i64, self.settings.dimmer_intensity as i64) as u8,
                );
            }
        }
        (sel, levels)
    }

}

fn migrate_v1(v: &Value) -> Value {
    let mut rules = vec![];
    if let Some(w) = v.get("windows").and_then(Value::as_object) {
        for (pattern, alpha) in w {
            if !pattern.trim().is_empty() {
                let opacity = clamp(Some(alpha), 0, 255, 220) as u8;
                rules.push(Rule::new(pattern, opacity, MatchMode::Title).to_json());
            }
        }
    }
    json!({"version": CONFIG_VERSION, "rules": rules})
}

type Listener = Box<dyn Fn() + Send + Sync>;

/// Thread-safe config with debounced atomic persistence.
pub struct Store {
    data: Mutex<Data>,
    path: PathBuf,
    saver: Mutex<Option<Sender<bool>>>,
    listeners: Mutex<Vec<Listener>>,
}

impl Store {
    pub fn open(path: PathBuf) -> Arc<Store> {
        let data = match std::fs::read_to_string(&path) {
            Ok(text) => match serde_json::from_str::<Value>(&text) {
                Ok(v) if v.is_object() => Data::from_json(&v),
                _ => {
                    let _ = std::fs::rename(&path, path.with_extension("json.corrupt"));
                    crate::log::warn("config was unreadable; kept it as config.json.corrupt");
                    Data::default()
                }
            },
            Err(_) => Data::default(),
        };
        let store = Arc::new(Store {
            data: Mutex::new(data),
            path,
            saver: Mutex::new(None),
            listeners: Mutex::new(vec![]),
        });
        let (tx, rx) = mpsc::channel::<bool>();
        *store.saver.lock().unwrap() = Some(tx);
        let weak = Arc::downgrade(&store);
        std::thread::Builder::new()
            .name("ConfigSaver".into())
            .spawn(move || loop {
                // Wait for a change, then until changes stop for 500 ms.
                match rx.recv() {
                    Ok(true) => {}
                    _ => return,
                }
                loop {
                    match rx.recv_timeout(Duration::from_millis(500)) {
                        Ok(true) => continue,
                        Ok(false) | Err(RecvTimeoutError::Disconnected) => {
                            if let Some(s) = weak.upgrade() {
                                s.save_now();
                            }
                            return;
                        }
                        Err(RecvTimeoutError::Timeout) => break,
                    }
                }
                match weak.upgrade() {
                    Some(s) => s.save_now(),
                    None => return,
                }
            })
            .expect("spawn config saver");
        store
    }

    pub fn add_listener(&self, f: impl Fn() + Send + Sync + 'static) {
        self.listeners.lock().unwrap().push(Box::new(f));
    }

    pub fn read(&self) -> MutexGuard<'_, Data> {
        self.data.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn settings(&self) -> Settings {
        self.read().settings.clone()
    }

    /// Mutate the config, then save soon and notify listeners.
    pub fn update<R>(&self, f: impl FnOnce(&mut Data) -> R) -> R {
        let r = {
            let mut d = self.read();
            f(&mut d)
        };
        self.changed();
        r
    }

    /// Mutate settings that the engine does not care about (no notify).
    pub fn update_quiet<R>(&self, f: impl FnOnce(&mut Data) -> R) -> R {
        let r = {
            let mut d = self.read();
            f(&mut d)
        };
        self.save_soon();
        r
    }

    fn changed(&self) {
        self.save_soon();
        for l in self.listeners.lock().unwrap().iter() {
            l();
        }
    }

    pub fn save_soon(&self) {
        if let Some(tx) = self.saver.lock().unwrap().as_ref() {
            let _ = tx.send(true);
        }
    }

    pub fn save_now(&self) {
        let text = {
            let d = self.read();
            serde_json::to_string_pretty(&d.to_json()).unwrap_or_default()
        };
        if text.is_empty() {
            return;
        }
        let _ = atomic_write(&self.path, text.as_bytes());
    }

    /// Flush any pending save (call on shutdown).
    pub fn close(&self) {
        if let Some(tx) = self.saver.lock().unwrap().take() {
            let _ = tx.send(false);
        }
        self.save_now();
    }

    pub fn export_to(&self, path: &Path) -> bool {
        let text = serde_json::to_string_pretty(&self.read().to_json()).unwrap_or_default();
        std::fs::write(path, text).is_ok()
    }

    pub fn import_from(&self, path: &Path) -> bool {
        let Ok(text) = std::fs::read_to_string(path) else {
            return false;
        };
        let Ok(mut v) = serde_json::from_str::<Value>(&text) else {
            return false;
        };
        if v.get("windows").is_some() && v.get("rules").is_none() {
            v = migrate_v1(&v);
        }
        if !v.get("rules").map(Value::is_array).unwrap_or(false) {
            return false;
        }
        let mut incoming = Data::from_json(&v);
        self.update(|d| {
            // Screens are machine specific: keep this machine's dimmer setup
            // unless the import carries one in the new format.
            if !incoming.settings.dimmer_migrated {
                incoming.settings.dimmer_screens = d.settings.dimmer_screens.clone();
                incoming.settings.dimmer_levels = d.settings.dimmer_levels.clone();
                incoming.settings.dimmer_migrated = d.settings.dimmer_migrated;
            }
            *d = incoming;
        });
        true
    }
}

pub fn atomic_write(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension(format!("{}.{}.tmp", std::process::id(), new_id()));
    let result = (|| {
        use std::io::Write;
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        drop(f);
        std::fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rule_matching_modes() {
        let mut r = Rule::new("Chrome", 128, MatchMode::Title);
        assert!(r.matches("Google Chrome - New tab", "chrome.exe"));
        r.match_mode = MatchMode::TitleExact;
        assert!(!r.matches("Google Chrome", "chrome.exe"));
        assert!(r.matches("chrome", "x.exe"));
        r.match_mode = MatchMode::Process;
        assert!(r.matches("anything", "chrome.exe"));
        r.pattern = "chrome.exe".into();
        assert!(r.matches("anything", "chrome.exe"));
    }

    #[test]
    fn python_config_round_trips_and_keeps_unknown_keys() {
        let raw = json!({
            "version": 2,
            "rules": [{"id": "abc", "pattern": "chrome", "match_mode": "title",
                       "opacity": 128, "enabled": true}],
            "presets": {},
            "settings": {
                "theme": "dark", "accent": "teal", "dimmer_intensity": 108,
                "dimmer_intensities": {"\\\\.\\DISPLAY5": 145},
                "dimmer_monitors": "all",
                "focus_mode": {"enabled": false, "active_opacity": 5, "background_opacity": 66},
                "some_future_key": 7
            }
        });
        let d = Data::from_json(&raw);
        assert_eq!(d.rules.len(), 1);
        assert_eq!(d.rules[0].opacity, 128);
        assert_eq!(d.settings.accent, "teal");
        assert_eq!(d.settings.focus.active_opacity, MIN_FOCUS_ALPHA);
        assert!(!d.settings.dimmer_migrated);
        let (sel, levels) = d.legacy_dimmer();
        assert!(sel.is_none());
        assert_eq!(levels.get("\\\\.\\DISPLAY5"), Some(&145));
        let out = d.to_json();
        assert_eq!(out["settings"]["some_future_key"], 7);
        assert_eq!(out["settings"]["dimmer_monitors"], "all");
        assert!(out["settings"].get("dimmer_screens").is_none());
    }

    #[test]
    fn v1_config_migrates() {
        let d = Data::from_json(&json!({"windows": {"Notepad": 100, "  ": 5}}));
        assert_eq!(d.rules.len(), 1);
        assert_eq!(d.rules[0].pattern, "Notepad");
        assert_eq!(d.rules[0].opacity, 100);
    }

    #[test]
    fn levels_fall_back_and_clamp() {
        let d = Data::from_json(&json!({"settings": {
            "dimmer_intensity": 90,
            "dimmer_screens": ["ID-A"],
            "dimmer_levels": {"id-a": 999}
        }}));
        assert!(d.settings.dimmer_migrated);
        assert_eq!(d.settings.dimmer_screens, vec!["id-a".to_string()]);
        assert_eq!(d.settings.level_for("id-a"), MAX_DIM);
        assert_eq!(d.settings.level_for("new-screen"), 90);
    }
}

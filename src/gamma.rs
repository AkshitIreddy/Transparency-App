//! Per-screen output dimming through hardware gamma ramps.
//!
//! Gamma dims the final monitor output (Start, taskbar and fullscreen apps
//! included) while screenshots keep their real pixels.
//!
//! The old implementation lost track of a screen whenever it disappeared
//! (closing a laptop lid turns the panel off). Its "restore" then failed
//! silently, the remembered original was thrown away, and when the lid opened
//! again the panel still carried the dimmed ramp, which was read back as the
//! new "original". Dimming compounded and switching off left the screen dim.
//!
//! This manager fixes that by:
//! - keying screens by their stable monitor id, never by `\\.\DISPLAYn`
//!   (those names move around when the topology changes);
//! - persisting the true original ramp for every screen it has changed,
//!   together with a "dirty" flag meaning a restore is still owed;
//! - only clearing that flag after a restore is confirmed, so a restore owed
//!   to a screen that is currently off is retried when it comes back (even
//!   in a later session);
//! - never accepting a ramp that looks like a uniformly dimmed identity
//!   ramp as an original (a leftover from older versions or a crash).

use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

pub type Ramp = [[u16; 256]; 3];

/// Keep hardware output at or above ~50%. Darker requests are finished by
/// the overlay (gamma below that is rejected by many drivers).
pub const MAX_GAMMA_INTENSITY: u8 = 127;
const VERIFY_TOLERANCE: i32 = 512;

pub trait GammaIo {
    fn get(&self, device: &str) -> Option<Ramp>;
    fn set(&self, device: &str, ramp: &Ramp) -> bool;
}

pub fn identity() -> Ramp {
    let mut r = [[0u16; 256]; 3];
    for c in r.iter_mut() {
        for (i, v) in c.iter_mut().enumerate() {
            *v = (i * 257) as u16;
        }
    }
    r
}

pub fn scaled(base: &Ramp, intensity: u8) -> Ramp {
    let scale = (255.0 - intensity.min(200) as f64) / 255.0;
    let mut r = [[0u16; 256]; 3];
    for c in 0..3 {
        for i in 0..256 {
            r[c][i] = (base[c][i] as f64 * scale).round().clamp(0.0, 65535.0) as u16;
        }
    }
    r
}

pub fn ramps_match(a: &Ramp, b: &Ramp) -> bool {
    (0..3).all(|c| (0..256).all(|i| (a[c][i] as i32 - b[c][i] as i32).abs() <= VERIFY_TOLERANCE))
}

/// Some(scale) when the ramp is an identity ramp scaled down uniformly on
/// all channels: the signature of a dimmer, never of a real calibration.
pub fn looks_dimmed(r: &Ramp) -> Option<f64> {
    let tops: Vec<f64> = (0..3).map(|c| r[c][255] as f64 / 65535.0).collect();
    let mean = tops.iter().sum::<f64>() / 3.0;
    if !(0.30..=0.97).contains(&mean) || tops.iter().any(|t| (t - mean).abs() > 0.02) {
        return None;
    }
    let tolerance = 0.02 * 65535.0;
    for c in 0..3 {
        for i in 0..256 {
            let expected = mean * (i * 257) as f64;
            if (r[c][i] as f64 - expected).abs() > tolerance {
                return None;
            }
        }
    }
    Some(mean)
}

#[derive(Clone, Serialize, Deserialize)]
struct StoredEntry {
    device: String,
    dirty: bool,
    red: Vec<u16>,
    green: Vec<u16>,
    blue: Vec<u16>,
}

#[derive(Default, Serialize, Deserialize)]
struct StoredState {
    version: u32,
    screens: BTreeMap<String, StoredEntry>,
}

#[derive(Clone)]
struct Entry {
    baseline: Ramp,
    /// We may have left a non-original ramp on this screen.
    dirty: bool,
    device: String,
}

fn to_ramp(e: &StoredEntry) -> Option<Ramp> {
    if e.red.len() != 256 || e.green.len() != 256 || e.blue.len() != 256 {
        return None;
    }
    let mut r = [[0u16; 256]; 3];
    r[0].copy_from_slice(&e.red);
    r[1].copy_from_slice(&e.green);
    r[2].copy_from_slice(&e.blue);
    Some(r)
}

pub struct Screen<'a> {
    pub id: &'a str,
    pub device: &'a str,
}

pub struct GammaManager<I: GammaIo> {
    io: I,
    path: Option<PathBuf>,
    entries: BTreeMap<String, Entry>,
    /// Screens whose driver refused a ramp this session (cleared on any
    /// display change, since a screen can come back on a different path).
    unsupported: HashSet<String>,
}

impl<I: GammaIo> GammaManager<I> {
    pub fn new(io: I, path: Option<PathBuf>) -> Self {
        let mut entries = BTreeMap::new();
        if let Some(p) = &path {
            if let Ok(text) = std::fs::read_to_string(p) {
                if let Ok(state) = serde_json::from_str::<StoredState>(&text) {
                    for (id, e) in state.screens {
                        if let Some(baseline) = to_ramp(&e) {
                            entries.insert(
                                id,
                                Entry { baseline, dirty: e.dirty, device: e.device },
                            );
                        }
                    }
                }
            }
        }
        GammaManager { io, path, entries, unsupported: HashSet::new() }
    }

    fn persist(&self) {
        let Some(path) = &self.path else { return };
        let dirty: BTreeMap<String, StoredEntry> = self
            .entries
            .iter()
            .filter(|(_, e)| e.dirty)
            .map(|(id, e)| {
                (
                    id.clone(),
                    StoredEntry {
                        device: e.device.clone(),
                        dirty: true,
                        red: e.baseline[0].to_vec(),
                        green: e.baseline[1].to_vec(),
                        blue: e.baseline[2].to_vec(),
                    },
                )
            })
            .collect();
        if dirty.is_empty() {
            let _ = std::fs::remove_file(path);
            return;
        }
        let state = StoredState { version: 1, screens: dirty };
        if let Ok(text) = serde_json::to_string(&state) {
            let _ = crate::config::atomic_write(path, text.as_bytes());
        }
    }

    pub fn display_changed(&mut self) {
        self.unsupported.clear();
    }

    pub fn is_dirty(&self, id: &str) -> bool {
        self.entries.get(id).map(|e| e.dirty).unwrap_or(false)
    }

    /// Ids of screens that still owe a restore.
    pub fn owed(&self) -> Vec<String> {
        self.entries
            .iter()
            .filter(|(_, e)| e.dirty)
            .map(|(id, _)| id.clone())
            .collect()
    }

    fn baseline_for(&mut self, s: &Screen) -> Option<Ramp> {
        if let Some(e) = self.entries.get_mut(s.id) {
            e.device = s.device.to_string();
            return Some(e.baseline);
        }
        let current = self.io.get(s.device)?;
        let baseline = if looks_dimmed(&current).is_some() {
            crate::log::warn(format!(
                "screen {} already carried a dimmed gamma ramp; using a neutral original",
                s.device
            ));
            identity()
        } else {
            current
        };
        self.entries.insert(
            s.id.to_string(),
            Entry { baseline, dirty: false, device: s.device.to_string() },
        );
        Some(baseline)
    }

    /// Dim a screen. Returns the gamma intensity that took effect, or None
    /// when the screen does not support it (the caller falls back to an
    /// overlay for the whole amount).
    pub fn apply(&mut self, s: &Screen, intensity: u8) -> Option<u8> {
        if intensity == 0 {
            self.restore(s);
            return None;
        }
        if self.unsupported.contains(s.id) {
            return None;
        }
        let applied = intensity.min(MAX_GAMMA_INTENSITY);
        let baseline = self.baseline_for(s)?;
        let target = scaled(&baseline, applied);
        if let Some(current) = self.io.get(s.device) {
            if ramps_match(&current, &target) {
                if !self.is_dirty(s.id) {
                    self.mark_dirty(s.id);
                }
                return Some(applied);
            }
        }
        let was_dirty = self.is_dirty(s.id);
        // Record the debt before touching the screen, so a crash or power
        // loss right after the write is still repaired next launch.
        self.mark_dirty(s.id);
        if !self.io.set(s.device, &target) {
            // Nothing changed on the screen.
            if !was_dirty {
                self.clear(s.id);
            }
            self.unsupported.insert(s.id.to_string());
            return None;
        }
        match self.io.get(s.device) {
            Some(r) if ramps_match(&r, &target) => Some(applied),
            _ => {
                // Silently rejected or altered: put the original back.
                self.restore(s);
                self.unsupported.insert(s.id.to_string());
                None
            }
        }
    }

    fn mark_dirty(&mut self, id: &str) {
        if let Some(e) = self.entries.get_mut(id) {
            e.dirty = true;
        }
        self.persist();
    }

    fn clear(&mut self, id: &str) {
        if self.entries.remove(id).is_some() {
            self.persist();
        }
    }

    /// Put a screen's original ramp back. Returns true once nothing is owed.
    pub fn restore(&mut self, s: &Screen) -> bool {
        let Some(e) = self.entries.get(s.id).cloned() else {
            return true;
        };
        if !e.dirty {
            self.entries.remove(s.id);
            return true;
        }
        let ok = self.io.set(s.device, &e.baseline)
            && self
                .io
                .get(s.device)
                .map(|r| ramps_match(&r, &e.baseline))
                .unwrap_or(true);
        if ok {
            self.clear(s.id);
        }
        ok
    }

    /// Restore every owed screen that is connected now and not in `keep`.
    /// Screens that are off (lid closed, unplugged) stay owed.
    pub fn settle(&mut self, present: &[Screen], keep: &HashSet<String>) {
        for id in self.owed() {
            if keep.contains(&id) {
                continue;
            }
            if let Some(s) = present.iter().find(|s| s.id == id) {
                if !self.restore(s) {
                    crate::log::warn(format!("could not restore gamma on {}", s.device));
                }
            }
        }
        // Drop clean, unused baselines so a later calibration change is seen.
        let stale: Vec<String> = self
            .entries
            .iter()
            .filter(|(id, e)| !e.dirty && !keep.contains(*id))
            .map(|(id, _)| id.clone())
            .collect();
        for id in stale {
            self.entries.remove(&id);
        }
    }

    /// Repair screens left dim by something we no longer have a record of
    /// (an older version, a crash before the record was written). Only
    /// touches connected screens we are not dimming whose ramp is a
    /// uniformly dimmed identity ramp, and resets them to neutral.
    pub fn repair_strays(&mut self, present: &[Screen], keep: &HashSet<String>) {
        for s in present {
            if keep.contains(s.id) || self.entries.contains_key(s.id) {
                continue;
            }
            if let Some(r) = self.io.get(s.device) {
                if let Some(scale) = looks_dimmed(&r) {
                    if self.io.set(s.device, &identity()) {
                        crate::log::info(format!(
                            "reset leftover dim ({:.0}%) on {}",
                            scale * 100.0,
                            s.device
                        ));
                    }
                }
            }
        }
    }

    /// Restore owed screens that are connected (used on shutdown and when
    /// dimming is switched off).
    pub fn restore_all(&mut self, present: &[Screen]) {
        self.settle(present, &HashSet::new());
    }

    /// One-time cleanup of the Python versions' crash ledger, which was keyed
    /// by device name. Only ramps that still look dimmed are touched.
    pub fn recover_legacy(&mut self, ledger: &std::path::Path, present: &[Screen]) {
        let Ok(text) = std::fs::read_to_string(ledger) else { return };
        if let Ok(serde_json::Value::Object(map)) = serde_json::from_str::<serde_json::Value>(&text) {
            for (device, values) in map {
                let Some(s) = present.iter().find(|s| s.device.eq_ignore_ascii_case(&device)) else {
                    continue;
                };
                let Some(current) = self.io.get(s.device) else { continue };
                if looks_dimmed(&current).is_none() {
                    continue;
                }
                let original = (|| {
                    let mut r = [[0u16; 256]; 3];
                    for (c, name) in ["red", "green", "blue"].iter().enumerate() {
                        let arr = values.get(*name)?.as_array()?;
                        if arr.len() != 256 {
                            return None;
                        }
                        for (i, v) in arr.iter().enumerate() {
                            r[c][i] = v.as_u64()?.min(65535) as u16;
                        }
                    }
                    Some(r)
                })()
                .filter(|r| looks_dimmed(r).is_none())
                .unwrap_or_else(identity);
                self.io.set(s.device, &original);
            }
        }
        let _ = std::fs::remove_file(ledger);
    }
}

pub struct GdiGamma;

#[cfg(windows)]
impl GammaIo for GdiGamma {
    fn get(&self, device: &str) -> Option<Ramp> {
        with_dc(device, |hdc| {
            let mut r: Ramp = [[0; 256]; 3];
            let ok = unsafe {
                windows_sys::Win32::UI::ColorSystem::GetDeviceGammaRamp(
                    hdc,
                    r.as_mut_ptr() as *mut std::ffi::c_void,
                )
            };
            (ok != 0).then_some(r)
        })
        .flatten()
    }

    fn set(&self, device: &str, ramp: &Ramp) -> bool {
        with_dc(device, |hdc| unsafe {
            windows_sys::Win32::UI::ColorSystem::SetDeviceGammaRamp(
                hdc,
                ramp.as_ptr() as *const std::ffi::c_void,
            ) != 0
        })
        .unwrap_or(false)
    }
}

fn with_dc<R>(device: &str, f: impl FnOnce(windows_sys::Win32::Graphics::Gdi::HDC) -> R) -> Option<R> {
    use windows_sys::Win32::Graphics::Gdi::{CreateDCW, DeleteDC};
    let driver = crate::win::wide("DISPLAY");
    let dev = crate::win::wide(device);
    unsafe {
        let hdc = CreateDCW(driver.as_ptr(), dev.as_ptr(), std::ptr::null(), std::ptr::null());
        if hdc.is_null() {
            return None;
        }
        let r = f(hdc);
        DeleteDC(hdc);
        Some(r)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::HashMap;

    /// Fake display hardware: device name -> ramp. A missing device behaves
    /// like a closed laptop lid (every call fails). `sticky` mimics a driver
    /// that keeps the last ramp across the screen turning off and on.
    #[derive(Default)]
    struct FakeIo {
        ramps: RefCell<HashMap<String, Ramp>>,
        off: RefCell<HashSet<String>>,
        reject: RefCell<HashSet<String>>,
    }

    impl GammaIo for &FakeIo {
        fn get(&self, device: &str) -> Option<Ramp> {
            if self.off.borrow().contains(device) {
                return None;
            }
            self.ramps.borrow().get(device).copied()
        }
        fn set(&self, device: &str, ramp: &Ramp) -> bool {
            if self.off.borrow().contains(device)
                || self.reject.borrow().contains(device)
                || !self.ramps.borrow().contains_key(device)
            {
                return false;
            }
            self.ramps.borrow_mut().insert(device.to_string(), *ramp);
            true
        }
    }

    fn calibrated() -> Ramp {
        // A real, non-linear calibration curve (warm white point).
        let mut r = identity();
        for i in 0..256 {
            let x = i as f64 / 255.0;
            r[0][i] = (x.powf(0.95) * 65535.0) as u16;
            r[1][i] = (x.powf(1.02) * 64000.0) as u16;
            r[2][i] = (x.powf(1.08) * 60000.0) as u16;
        }
        r
    }

    fn temp_path(name: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("ta-gamma-test-{}-{name}.json", std::process::id()));
        let _ = std::fs::remove_file(&p);
        p
    }

    #[test]
    fn dimmed_identity_is_detected_but_calibration_is_not() {
        assert!(looks_dimmed(&identity()).is_none());
        assert!(looks_dimmed(&scaled(&identity(), 100)).is_some());
        assert!(looks_dimmed(&calibrated()).is_none());
        assert!(looks_dimmed(&scaled(&calibrated(), 100)).is_none());
    }

    #[test]
    fn lid_close_while_dimmed_then_switch_off_restores_on_reopen() {
        let io = FakeIo::default();
        io.ramps.borrow_mut().insert("D1".into(), calibrated());
        let path = temp_path("lid");
        let mut g = GammaManager::new(&io, Some(path.clone()));
        let laptop = Screen { id: "laptop", device: "D1" };

        assert_eq!(g.apply(&laptop, 100), Some(100));
        assert!(ramps_match(&(&io).get("D1").unwrap(), &scaled(&calibrated(), 100)));

        // Lid closes: the panel is gone. Switching dimming off cannot reach it.
        io.off.borrow_mut().insert("D1".into());
        g.restore_all(&[]);
        assert!(g.is_dirty("laptop"), "restore must stay owed while the screen is off");

        // Lid opens. The driver kept the dimmed ramp, and the panel came back
        // under a different device name.
        io.off.borrow_mut().clear();
        let dimmed = io.ramps.borrow_mut().remove("D1").unwrap();
        io.ramps.borrow_mut().insert("D3".into(), dimmed);
        let laptop = Screen { id: "laptop", device: "D3" };
        g.display_changed();
        g.settle(&[laptop], &HashSet::new());
        assert!(ramps_match(&(&io).get("D3").unwrap(), &calibrated()));
        assert!(!g.is_dirty("laptop"));
        assert!(!path.exists(), "nothing owed, so no state file");
    }

    #[test]
    fn reopen_while_still_dimming_does_not_compound() {
        let io = FakeIo::default();
        io.ramps.borrow_mut().insert("D1".into(), identity());
        let mut g = GammaManager::new(&io, None);
        let s = Screen { id: "laptop", device: "D1" };
        g.apply(&s, 120);
        io.off.borrow_mut().insert("D1".into());
        assert_eq!(g.apply(&s, 120), None);
        io.off.borrow_mut().clear();
        g.display_changed();
        for _ in 0..5 {
            assert_eq!(g.apply(&s, 120), Some(120));
        }
        assert!(ramps_match(&(&io).get("D1").unwrap(), &scaled(&identity(), 120)));
        g.restore_all(&[Screen { id: "laptop", device: "D1" }]);
        assert!(ramps_match(&(&io).get("D1").unwrap(), &identity()));
    }

    #[test]
    fn owed_restore_survives_a_restart() {
        let io = FakeIo::default();
        io.ramps.borrow_mut().insert("D1".into(), calibrated());
        let path = temp_path("restart");
        {
            let mut g = GammaManager::new(&io, Some(path.clone()));
            g.apply(&Screen { id: "laptop", device: "D1" }, 90);
            io.off.borrow_mut().insert("D1".into());
            g.restore_all(&[]);
            // App quits (or crashes) with the lid still closed.
        }
        assert!(path.exists());
        io.off.borrow_mut().clear();
        let mut g = GammaManager::new(&io, Some(path.clone()));
        assert!(g.is_dirty("laptop"));
        g.settle(&[Screen { id: "laptop", device: "D2" }], &HashSet::new());
        // D2 does not exist in the fake, so this screen is still owed...
        assert!(g.is_dirty("laptop"));
        g.settle(&[Screen { id: "laptop", device: "D1" }], &HashSet::new());
        assert!(ramps_match(&(&io).get("D1").unwrap(), &calibrated()));
        assert!(!path.exists());
    }

    #[test]
    fn leftover_dim_is_never_taken_as_the_original() {
        let io = FakeIo::default();
        io.ramps.borrow_mut().insert("D1".into(), scaled(&identity(), 110));
        let mut g = GammaManager::new(&io, None);
        let s = Screen { id: "x", device: "D1" };
        g.apply(&s, 50);
        assert!(ramps_match(&(&io).get("D1").unwrap(), &scaled(&identity(), 50)));
        g.restore_all(&[Screen { id: "x", device: "D1" }]);
        assert!(ramps_match(&(&io).get("D1").unwrap(), &identity()));
    }

    #[test]
    fn stray_repair_only_touches_unmanaged_dimmed_screens() {
        let io = FakeIo::default();
        io.ramps.borrow_mut().insert("D1".into(), scaled(&identity(), 90));
        io.ramps.borrow_mut().insert("D2".into(), calibrated());
        io.ramps.borrow_mut().insert("D3".into(), scaled(&identity(), 90));
        let mut g = GammaManager::new(&io, None);
        let keep: HashSet<String> = ["c".to_string()].into();
        g.repair_strays(
            &[
                Screen { id: "a", device: "D1" },
                Screen { id: "b", device: "D2" },
                Screen { id: "c", device: "D3" },
            ],
            &keep,
        );
        assert!(ramps_match(&(&io).get("D1").unwrap(), &identity()));
        assert!(ramps_match(&(&io).get("D2").unwrap(), &calibrated()));
        assert!(looks_dimmed(&(&io).get("D3").unwrap()).is_some());
    }

    #[test]
    fn unsupported_screen_falls_back_without_owing_anything() {
        let io = FakeIo::default();
        io.ramps.borrow_mut().insert("V".into(), identity());
        io.reject.borrow_mut().insert("V".into());
        let mut g = GammaManager::new(&io, None);
        assert_eq!(g.apply(&Screen { id: "virt", device: "V" }, 100), None);
        assert!(g.owed().is_empty());
    }

    #[test]
    fn darker_requests_are_capped_for_hardware() {
        let io = FakeIo::default();
        io.ramps.borrow_mut().insert("D1".into(), identity());
        let mut g = GammaManager::new(&io, None);
        assert_eq!(g.apply(&Screen { id: "a", device: "D1" }, 190), Some(MAX_GAMMA_INTENSITY));
    }
}

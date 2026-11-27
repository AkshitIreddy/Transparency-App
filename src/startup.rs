//! Run-at-sign-in via the per-user Run registry key (no admin needed).

use windows_sys::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyExW, RegDeleteValueW, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW,
    HKEY, HKEY_CURRENT_USER, KEY_QUERY_VALUE, KEY_SET_VALUE, REG_OPTION_NON_VOLATILE, REG_SZ,
};

use crate::win::wide;

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const VALUE: &str = "TransparencyApp";

/// Command Windows runs at sign-in: start hidden in the tray.
pub fn launch_command() -> String {
    format!("\"{}\" --minimized", crate::paths::current_exe().display())
}

fn open(access: u32) -> Option<HKEY> {
    let key = wide(RUN_KEY);
    let mut h: HKEY = std::ptr::null_mut();
    let rc = unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, key.as_ptr(), 0, access, &mut h) };
    (rc == 0).then_some(h)
}

pub fn current_value() -> Option<String> {
    let h = open(KEY_QUERY_VALUE)?;
    let name = wide(VALUE);
    let mut buf = vec![0u16; 2048];
    let mut size = (buf.len() * 2) as u32;
    let mut kind = 0u32;
    let rc = unsafe {
        RegQueryValueExW(h, name.as_ptr(), std::ptr::null(), &mut kind, buf.as_mut_ptr() as *mut u8, &mut size)
    };
    unsafe { RegCloseKey(h) };
    (rc == 0).then(|| crate::win::from_wide(&buf))
}

pub fn is_enabled() -> bool {
    current_value().is_some()
}

pub fn enable() -> bool {
    let key = wide(RUN_KEY);
    let mut h: HKEY = std::ptr::null_mut();
    unsafe {
        if RegCreateKeyExW(
            HKEY_CURRENT_USER,
            key.as_ptr(),
            0,
            std::ptr::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_SET_VALUE,
            std::ptr::null(),
            &mut h,
            std::ptr::null_mut(),
        ) != 0
        {
            return false;
        }
        let name = wide(VALUE);
        let data = wide(&launch_command());
        let rc = RegSetValueExW(h, name.as_ptr(), 0, REG_SZ, data.as_ptr() as *const u8, (data.len() * 2) as u32);
        RegCloseKey(h);
        rc == 0
    }
}

pub fn disable() -> bool {
    let Some(h) = open(KEY_SET_VALUE) else { return true };
    let name = wide(VALUE);
    let rc = unsafe { RegDeleteValueW(h, name.as_ptr()) };
    unsafe { RegCloseKey(h) };
    rc == 0 || rc == 2
}

/// Keep an existing sign-in entry pointing at this exe with the current
/// arguments (older versions registered the exe without --minimized).
pub fn refresh_if_enabled() {
    if let Some(v) = current_value() {
        if v != launch_command() {
            enable();
        }
    }
}

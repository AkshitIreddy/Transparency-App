//! A tiny file logger (`%APPDATA%\TransparencyApp\app.log`, rotated at 512 KB).

use std::io::Write;
use std::sync::Mutex;

use windows_sys::Win32::System::SystemInformation::GetLocalTime;

static LOCK: Mutex<()> = Mutex::new(());

fn write(level: &str, msg: &str) {
    if cfg!(test) {
        return;
    }
    let _guard = LOCK.lock();
    let path = crate::paths::log_path();
    if let Ok(meta) = std::fs::metadata(&path) {
        if meta.len() > 512 * 1024 {
            let _ = std::fs::rename(&path, path.with_extension("log.1"));
        }
    }
    let t = unsafe {
        let mut t = std::mem::zeroed();
        GetLocalTime(&mut t);
        t
    };
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(
            f,
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02},{:03} {} transparency_app: {}",
            t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond, t.wMilliseconds, level, msg
        );
    }
}

pub fn info(msg: impl AsRef<str>) {
    write("INFO", msg.as_ref());
}

pub fn warn(msg: impl AsRef<str>) {
    write("WARNING", msg.as_ref());
}

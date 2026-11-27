//! Where the app keeps its files (`%APPDATA%\TransparencyApp`).

use std::path::PathBuf;

pub fn app_dir() -> PathBuf {
    let base = std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("USERPROFILE").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("."));
    let dir = base.join("TransparencyApp");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

pub fn config_path() -> PathBuf {
    app_dir().join("config.json")
}

/// Crash ledger for window transparency (same file the Python app used).
pub fn session_path() -> PathBuf {
    app_dir().join("session.json")
}

/// Gamma state: true originals per screen and screens we still owe a restore.
pub fn gamma_state_path() -> PathBuf {
    app_dir().join("gamma-state.json")
}

/// Crash ledger written by the Python versions, keyed by device name.
pub fn legacy_gamma_ledger_path() -> PathBuf {
    app_dir().join("gamma-session.json")
}

pub fn log_path() -> PathBuf {
    app_dir().join("app.log")
}

pub fn current_exe() -> PathBuf {
    std::env::current_exe().unwrap_or_else(|_| PathBuf::from("TransparencyApp.exe"))
}

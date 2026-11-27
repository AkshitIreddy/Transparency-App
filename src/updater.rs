//! Verified updates from the project's GitHub Releases.
//!
//! The running exe cannot overwrite itself, so the downloaded (SHA-256
//! verified) exe is started in installer mode: it waits for this process to
//! exit, swaps the file with a rollback copy and relaunches. The command line
//! is the same one the Python versions used, so they can update to this one.

use std::io::{Read, Write};
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::Value;
use sha2::{Digest, Sha256};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
const LATEST_RELEASE_API: &str =
    "https://api.github.com/repos/AkshitIreddy/TransparencyApp/releases/latest";
pub const ASSET_NAME: &str = "TransparencyApp.exe";
const MAX_UPDATE_BYTES: u64 = 100 * 1024 * 1024;
const DETACHED_PROCESS: u32 = 0x0000_0008;
const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;

#[derive(Clone, Debug)]
pub struct Release {
    pub version: String,
    pub url: String,
    pub size: u64,
    pub sha256: String,
}

pub fn version_tuple(v: &str) -> Option<(u64, u64, u64)> {
    let v = v.trim().strip_prefix('v').unwrap_or(v.trim());
    let mut it = v.split('.');
    let t = (
        it.next()?.parse().ok()?,
        it.next()?.parse().ok()?,
        it.next()?.parse().ok()?,
    );
    it.next().is_none().then_some(t)
}

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(15))
        .timeout_read(Duration::from_secs(60))
        .user_agent(&format!("TransparencyApp/{VERSION}"))
        .build()
}

/// The newest release if it is newer than this build.
pub fn check() -> Result<Option<Release>, String> {
    let resp = agent()
        .get(LATEST_RELEASE_API)
        .set("Accept", "application/vnd.github+json")
        .set("X-GitHub-Api-Version", "2022-11-28")
        .call()
        .map_err(|e| format!("Could not check GitHub Releases: {e}"))?;
    let payload: Value = resp
        .into_json()
        .map_err(|e| format!("Could not read the release information: {e}"))?;
    parse_release(&payload, VERSION)
}

pub fn parse_release(payload: &Value, current: &str) -> Result<Option<Release>, String> {
    let tag = payload.get("tag_name").and_then(Value::as_str).unwrap_or("");
    let (Some(latest), Some(cur)) = (version_tuple(tag), version_tuple(current)) else {
        return Err("The release has an unsupported version number.".into());
    };
    if latest <= cur {
        return Ok(None);
    }
    let asset = payload
        .get("assets")
        .and_then(Value::as_array)
        .and_then(|a| a.iter().find(|x| x.get("name").and_then(Value::as_str) == Some(ASSET_NAME)))
        .ok_or_else(|| format!("Release {tag} has no {ASSET_NAME} asset."))?;
    let url = asset.get("browser_download_url").and_then(Value::as_str).unwrap_or("");
    if !url.starts_with("https://github.com/") {
        return Err("The update download URL is not trusted.".into());
    }
    let size = asset.get("size").and_then(Value::as_u64).unwrap_or(0);
    if size == 0 || size > MAX_UPDATE_BYTES {
        return Err("The update has an invalid download size.".into());
    }
    let digest = asset.get("digest").and_then(Value::as_str).unwrap_or("");
    let sha = digest
        .strip_prefix("sha256:")
        .filter(|h| h.len() == 64 && h.chars().all(|c| c.is_ascii_hexdigit()))
        .ok_or("GitHub did not provide a SHA-256 digest for the update.")?;
    Ok(Some(Release {
        version: format!("{}.{}.{}", latest.0, latest.1, latest.2),
        url: url.to_string(),
        size,
        sha256: sha.to_lowercase(),
    }))
}

pub fn staged_path(version: &str) -> PathBuf {
    std::env::temp_dir()
        .join("TransparencyAppUpdates")
        .join(format!("TransparencyApp-v{version}.exe"))
}

pub fn download(release: &Release, dest: &Path, mut progress: impl FnMut(u64, u64)) -> Result<PathBuf, String> {
    if let Some(dir) = dest.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let partial = dest.with_extension("exe.part");
    let result = (|| -> Result<(), String> {
        let resp = agent()
            .get(&release.url)
            .call()
            .map_err(|e| format!("Could not download the update: {e}"))?;
        let mut reader = resp.into_reader().take(MAX_UPDATE_BYTES + 1);
        let mut out = std::fs::File::create(&partial).map_err(|e| e.to_string())?;
        let mut hasher = Sha256::new();
        let mut buf = vec![0u8; 256 * 1024];
        let mut received = 0u64;
        loop {
            let n = reader.read(&mut buf).map_err(|e| format!("Could not download the update: {e}"))?;
            if n == 0 {
                break;
            }
            received += n as u64;
            if received > release.size {
                return Err("The update download is larger than expected.".into());
            }
            out.write_all(&buf[..n]).map_err(|e| e.to_string())?;
            hasher.update(&buf[..n]);
            progress(received, release.size);
        }
        out.sync_all().map_err(|e| e.to_string())?;
        if received != release.size {
            return Err("The update download is incomplete.".into());
        }
        if hex(&hasher.finalize()) != release.sha256 {
            return Err("The update failed SHA-256 verification.".into());
        }
        Ok(())
    })();
    match result {
        Ok(()) => {
            std::fs::rename(&partial, dest).map_err(|e| e.to_string())?;
            Ok(dest.to_path_buf())
        }
        Err(e) => {
            let _ = std::fs::remove_file(&partial);
            Err(e)
        }
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn file_sha256(path: &Path) -> std::io::Result<String> {
    let mut f = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1024 * 1024];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex(&hasher.finalize()))
}

fn is_expected_name(target: &Path) -> bool {
    target
        .file_name()
        .map(|n| n.to_string_lossy().eq_ignore_ascii_case(ASSET_NAME))
        .unwrap_or(false)
}

/// Start the verified exe's installer mode.
pub fn launch_installer(staged: &Path, target: &Path, sha256: &str, version: &str) -> Result<(), String> {
    if !staged.is_file() {
        return Err("The verified update file is missing.".into());
    }
    if !is_expected_name(target) {
        return Err("Refusing to replace an unexpected executable name.".into());
    }
    if file_sha256(staged).map_err(|e| e.to_string())? != sha256.to_lowercase() {
        return Err("The update no longer matches its verified SHA-256.".into());
    }
    std::process::Command::new(staged)
        .args([
            "--install-update".to_string(),
            "--target".into(),
            target.display().to_string(),
            "--wait-pid".into(),
            std::process::id().to_string(),
            "--expected-sha256".into(),
            sha256.to_lowercase(),
            "--version".into(),
            version.to_string(),
        ])
        .current_dir(target.parent().unwrap_or(Path::new(".")))
        .creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP)
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("Could not start the update installer: {e}"))
}

pub fn wait_for_process(pid: u32, timeout_ms: u32) -> bool {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE};
    if pid == 0 {
        return true;
    }
    unsafe {
        let h = OpenProcess(PROCESS_SYNCHRONIZE, 0, pid);
        if h.is_null() {
            return true; // already gone
        }
        let r = WaitForSingleObject(h, timeout_ms);
        CloseHandle(h);
        r == 0
    }
}

/// Replace `target` with `source` transactionally and relaunch it. The old
/// exe stays at `<target>.previous` as a rollback copy.
pub fn replace_and_restart(
    source: &Path,
    target: &Path,
    pid: u32,
    sha256: &str,
    mut progress: impl FnMut(f32, &str),
) -> Result<(), String> {
    let sha256 = sha256.to_lowercase();
    if !is_expected_name(target) {
        return Err("Refusing to replace an unexpected executable name.".into());
    }
    if !source.is_file() {
        return Err("The verified update file is missing.".into());
    }
    if file_sha256(source).map_err(|e| e.to_string())? != sha256 {
        return Err("The update no longer matches its verified SHA-256.".into());
    }
    progress(0.12, "Waiting for Transparency App to close...");
    if !wait_for_process(pid, 120_000) {
        return Err("Transparency App did not close in time.".into());
    }
    // Give Windows a moment to release the file lock after exit.
    std::thread::sleep(Duration::from_millis(300));
    let backup = PathBuf::from(format!("{}.previous", target.display()));
    let incoming = PathBuf::from(format!("{}.updating", target.display()));
    let mut swapped = false;
    let result = (|| -> Result<(), String> {
        progress(0.38, "Preparing a safe rollback copy...");
        for stale in [&incoming, &backup] {
            if stale.exists() {
                std::fs::remove_file(stale).map_err(|e| e.to_string())?;
            }
        }
        if target.exists() {
            std::fs::rename(target, &backup).map_err(|e| e.to_string())?;
        }
        progress(0.62, "Installing the verified update...");
        std::fs::copy(source, &incoming).map_err(|e| e.to_string())?;
        if file_sha256(&incoming).map_err(|e| e.to_string())? != sha256 {
            return Err("The installed copy failed SHA-256 verification.".into());
        }
        std::fs::rename(&incoming, target).map_err(|e| e.to_string())?;
        swapped = true;
        progress(0.88, "Starting the updated app...");
        std::process::Command::new(target)
            .current_dir(target.parent().unwrap_or(Path::new(".")))
            .creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP)
            .spawn()
            .map_err(|e| e.to_string())?;
        progress(1.0, "Update complete. Transparency App is restarting.");
        Ok(())
    })();
    if let Err(e) = result {
        let _ = std::fs::remove_file(&incoming);
        if swapped {
            let _ = std::fs::remove_file(target);
        }
        if backup.exists() {
            let _ = std::fs::rename(&backup, target);
        }
        return Err(format!("Could not install the update: {e}"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn payload(tag: &str) -> Value {
        json!({"tag_name": tag, "assets": [{
            "name": ASSET_NAME,
            "browser_download_url": "https://github.com/x/y/releases/download/v9/TransparencyApp.exe",
            "size": 1234,
            "digest": format!("sha256:{}", "a".repeat(64)),
        }]})
    }

    #[test]
    fn newer_release_is_offered() {
        let r = parse_release(&payload("v3.1.0"), "3.0.0").unwrap().unwrap();
        assert_eq!(r.version, "3.1.0");
        assert_eq!(r.size, 1234);
        assert!(parse_release(&payload("v3.0.0"), "3.0.0").unwrap().is_none());
        assert!(parse_release(&payload("v2.9.9"), "3.0.0").unwrap().is_none());
        assert!(parse_release(&payload("nightly"), "3.0.0").is_err());
    }

    #[test]
    fn untrusted_url_is_rejected() {
        let mut p = payload("v9.0.0");
        p["assets"][0]["browser_download_url"] = json!("https://evil.example/x.exe");
        assert!(parse_release(&p, "3.0.0").is_err());
    }
}

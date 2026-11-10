//! Updates, on request only: nothing here runs until the button in Settings is pressed.
//!
//! "Check" asks GitHub for the newest release of `[update] repo` (one small request, then the
//! thread ends). "Install" downloads that release's setup program to the temp folder and starts
//! it quietly; the installer closes this app, replaces the files and starts it again.
//! A release that carries the bare `dynamic-notch.exe` is even simpler: it is downloaded next to
//! the running exe, swapped in (a running exe can be renamed on Windows) and started again.
//! There is no timer, no background service and no check at start-up.

use std::io::Write;
use std::sync::atomic::AtomicBool;

use crate::bus::Bus;
use crate::sys::http::{self, Timeouts};

/// The version this build was made as (`Cargo.toml`).
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Worker -> Settings.
#[derive(Debug)]
pub enum Msg {
    UpToDate,
    Available { tag: String, installer: Option<String>, page: String, notes: String },
    Installing,
    Failed(String),
}

/// Ask GitHub whether a newer release exists. The answer arrives as a `Msg`.
pub fn check(repo: &str, bus: &Bus) {
    let repo = canonical(repo);
    let bus = bus.clone();
    let _ = std::thread::Builder::new().name("update".into()).spawn(move || {
        bus.to_module("settings", run_check(&repo));
    });
}

/// Download the setup program and start it quietly.
pub fn install(url: &str, bus: &Bus) {
    let url = url.to_string();
    let bus = bus.clone();
    let _ = std::thread::Builder::new().name("update".into()).spawn(move || {
        bus.to_module("settings", run_install(&url));
    });
}

/// Older configs still name the repos this app was forked from; they all mean this project's.
fn canonical(repo: &str) -> String {
    let r = repo.trim();
    if r.eq_ignore_ascii_case("shreyasb1119-arch/Notch") || r.eq_ignore_ascii_case("eigenes/DynamicNotch") || r.is_empty() {
        "TonyB1119-arch/Notch".to_string()
    } else {
        r.to_string()
    }
}

/// Remove what a finished self-update left behind. Cheap, silent, and only touches our own files.
pub fn tidy() {
    if let Ok(exe) = std::env::current_exe() {
        let _ = std::fs::remove_file(exe.with_extension("exe.old"));
        let _ = std::fs::remove_file(exe.with_extension("exe.new"));
    }
}

fn headers() -> [(&'static str, String); 2] {
    [("Accept", "application/vnd.github+json".to_string()), ("User-Agent", "DynamicNotch".to_string())]
}

/// Headers for downloading the program file itself.
fn download_headers() -> [(&'static str, String); 2] {
    [("Accept", "application/octet-stream".to_string()), ("User-Agent", "DynamicNotch".to_string())]
}

fn run_check(repo: &str) -> Msg {
    if repo.split('/').count() != 2 || repo.contains(' ') || repo.starts_with('/') || repo.ends_with('/') {
        return Msg::Failed("Set [update] repo in the config (owner/name)".into());
    }
    let url = format!("https://api.github.com/repos/{repo}/releases/latest");
    let cancel = AtomicBool::new(false);
    let (status, body) = match http::fetch("GET", &url, &headers(), &[], Timeouts::DEFAULT, 512 * 1024, &cancel) {
        Ok(v) => v,
        Err(e) => return Msg::Failed(e),
    };
    match status {
        200 => {}
        404 => return Msg::Failed("No release published yet".into()),
        s => return Msg::Failed(format!("GitHub answered {s}")),
    }
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(&body) else {
        return Msg::Failed("Couldn't read GitHub's answer".into());
    };
    let tag = v["tag_name"].as_str().unwrap_or("").to_string();
    if tag.is_empty() {
        return Msg::Failed("The release has no version".into());
    }
    if !is_newer(&tag, VERSION) {
        return Msg::UpToDate;
    }
    // the bare exe wins (swapped in place); a setup program is the fallback
    let mut exe = None;
    let mut setup = None;
    for a in v["assets"].as_array().into_iter().flatten() {
        let (Some(name), Some(url)) = (a["name"].as_str(), a["browser_download_url"].as_str()) else { continue };
        let name = name.to_ascii_lowercase();
        if !name.ends_with(".exe") {
            continue;
        }
        if name.contains("setup") {
            setup.get_or_insert(url.to_string());
        } else {
            exe.get_or_insert(url.to_string());
        }
    }
    let installer = exe.or(setup);
    let notes = v["body"].as_str().unwrap_or("").to_string();
    let page = v["html_url"].as_str().unwrap_or("").to_string();
    Msg::Available { tag, installer, page, notes }
}

fn run_install(url: &str) -> Msg {
    if !url.starts_with("https://") {
        return Msg::Failed("The download link is not secure".into());
    }
    let setup = url.rsplit('/').next().unwrap_or("").to_ascii_lowercase().contains("setup");
    if setup {
        let path = std::env::temp_dir().join("DynamicNotch-Setup.exe");
        if let Err(e) = download(url, &path) {
            return Msg::Failed(e);
        }
        return match std::process::Command::new(&path)
            .args(["/SILENT", "/CLOSEAPPLICATIONS", "/RESTARTAPPLICATIONS"])
            .spawn()
        {
            Ok(_) => Msg::Installing,
            Err(e) => Msg::Failed(format!("Couldn't start the installer: {e}")),
        };
    }
    swap_in(url)
}

/// Download `url` to `path` and check it looks like a Windows program.
fn download(url: &str, path: &std::path::Path) -> Result<(), String> {
    let mut file = std::fs::File::create(path).map_err(|_| "Couldn't write next to the app".to_string())?;
    let cancel = AtomicBool::new(false);
    let mut total = 0usize;
    let mut first = [0u8; 2];
    let mut write_ok = true;
    let result =
        http::request("GET", url, &download_headers(), &[], Timeouts(10_000, 15_000, 30_000, 300_000), &cancel, |chunk| {
            if total == 0 && chunk.len() >= 2 {
                first = [chunk[0], chunk[1]];
            }
            total += chunk.len();
            write_ok = file.write_all(chunk).is_ok();
            write_ok
        });
    drop(file);
    let ok = matches!(result, Ok(200)) && write_ok && total >= 100_000 && &first == b"MZ";
    if !ok {
        let _ = std::fs::remove_file(path);
        return Err(match result {
            Err(e) => e,
            Ok(code) => format!("The download didn't look right (HTTP {code}, {total} bytes)"),
        });
    }
    Ok(())
}

/// Replace the running exe with the downloaded one, start it again and quit this copy.
fn swap_in(url: &str) -> Msg {
    use std::os::windows::process::CommandExt;
    let Ok(cur) = std::env::current_exe() else { return Msg::Failed("Couldn't find the running app".into()) };
    let new = cur.with_extension("exe.new");
    let old = cur.with_extension("exe.old");
    if let Err(e) = download(url, &new) {
        return Msg::Failed(e);
    }
    let _ = std::fs::remove_file(&old);
    if std::fs::rename(&cur, &old).is_err() {
        let _ = std::fs::remove_file(&new);
        return Msg::Failed("Couldn't replace the app (is it in a protected folder?)".into());
    }
    if std::fs::rename(&new, &cur).is_err() {
        let _ = std::fs::rename(&old, &cur);
        let _ = std::fs::remove_file(&new);
        return Msg::Failed("Couldn't put the new version in place".into());
    }
    // start the new copy a moment from now, once this one has let go of the single-instance lock
    let started = std::process::Command::new("cmd")
        .arg("/C")
        .raw_arg(format!("ping -n 3 127.0.0.1 >nul & start \"\" \"{}\"", cur.display()))
        .creation_flags(0x0800_0000)
        .spawn();
    if started.is_err() {
        return Msg::Failed("Updated, please start the notch again".into());
    }
    std::thread::spawn(|| {
        std::thread::sleep(std::time::Duration::from_millis(400));
        crate::ipc::send_to_running(&["quit".to_string()]);
    });
    Msg::Installing
}

/// Is release tag `tag` ("V3", "v1.2.0") newer than `current` ("3.0.0")? Compared number by number.
pub fn is_newer(tag: &str, current: &str) -> bool {
    fn nums(s: &str) -> Vec<u32> {
        s.trim_start_matches(['v', 'V'])
            .split(|c: char| !c.is_ascii_digit())
            .filter(|p| !p.is_empty())
            .map(|p| p.parse().unwrap_or(0))
            .collect()
    }
    let (t, c) = (nums(tag), nums(current));
    for i in 0..t.len().max(c.len()) {
        let (a, b) = (t.get(i).copied().unwrap_or(0), c.get(i).copied().unwrap_or(0));
        if a != b {
            return a > b;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_compare_by_number() {
        assert!(is_newer("V4", "3.0.0"));
        assert!(is_newer("v3.1", "3.0.9"));
        assert!(is_newer("V10", "9.0.0"));
        assert!(!is_newer("V3", "3.0.0"));
        assert!(!is_newer("V2", "3.0.0"));
        assert!(!is_newer("v3.0.0", "3"));
    }
}

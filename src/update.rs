//! Self-update: asks GitLab for the latest release, downloads the installer for this
//! platform and starts it. Windows runs the Inno Setup installer silently (it waits for
//! this process to exit, then relaunches the app); macOS replaces the app bundle with the
//! downloaded one from a helper script that waits for this process. Flatpak (Linux) is
//! updated by the software manager, so there the check only says that a version is out.
//!
//! Everything here is blocking and meant for a background thread; `app.rs` polls the result.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

const RELEASE_URL: &str = "https://gitlab.com/api/v4/projects/kestermckinney%2FSeeMyMac/releases/permalink/latest";
const USER_AGENT: &str = concat!("SeeMyMac-Updater/", env!("CARGO_PKG_VERSION"));

/// A newer release with the installer for this platform.
#[derive(Debug, Clone, PartialEq)]
pub struct Release {
    pub version: String,
    pub notes: String,
    pub asset_url: String,
    pub asset_name: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum CheckResult {
    UpToDate,
    Available(Release),
    /// A newer version exists but cannot be installed from here (Flatpak, or no installer
    /// published for this platform); the text tells the user what to do.
    Manual(String),
    Failed(String),
}

pub fn current_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// True when `candidate` is a strictly higher `X.Y.Z` than `current`.
pub fn is_newer(candidate: &str, current: &str) -> bool {
    let parse = |v: &str| -> Vec<u64> { v.split('.').map(|p| p.parse().unwrap_or(0)).collect() };
    let (c, r) = (parse(candidate), parse(current));
    for i in 0..c.len().max(r.len()) {
        let (a, b) = (c.get(i).copied().unwrap_or(0), r.get(i).copied().unwrap_or(0));
        if a != b {
            return a > b;
        }
    }
    false
}

/// Whether `name` is this platform's installer, given the file names of the release's links.
/// Windows: `SeeMyMac-<ver>-setup.exe`, one installer that picks the ARM64 or x86 build for
/// the PC it runs on (debug installers, `-debug-setup.exe`, are never offered).
/// macOS: `SeeMyMac-<ver>-macOS.zip` (a universal, signed and notarized app bundle).
fn is_platform_asset(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    if cfg!(target_os = "windows") {
        n.strip_prefix("seemymac-").and_then(|r| r.strip_suffix("-setup.exe")).map_or(false, |v| !v.is_empty() && v.bytes().all(|b| b.is_ascii_digit() || b == b'.'))
    } else if cfg!(target_os = "macos") {
        n.starts_with("seemymac-") && n.contains("macos") && n.ends_with(".zip")
    } else {
        false
    }
}

/// Interpret GitLab's release JSON for a program that is running `current`.
pub fn parse_release(json: &str, current: &str) -> CheckResult {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(json) else {
        return CheckResult::Failed("Unexpected response from the update server.".into());
    };
    if v["upcoming_release"].as_bool().unwrap_or(false) {
        return CheckResult::UpToDate;
    }
    let tag = v["tag_name"].as_str().unwrap_or("");
    let version = tag.strip_prefix(['v', 'V']).unwrap_or(tag);
    let stable = version.split('.').count() == 3 && version.split('.').all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()));
    if !stable {
        return CheckResult::Failed("The update server did not report a version.".into());
    }
    if !is_newer(version, current) {
        return CheckResult::UpToDate;
    }
    if cfg!(target_os = "linux") {
        return CheckResult::Manual(format!("Version {version} is available. Update SeeMyMac through your software manager, or run: flatpak update io.gitlab.kestermckinney.SeeMyMac"));
    }
    let links = v["assets"]["links"].as_array().cloned().unwrap_or_default();
    for link in &links {
        let url = link["direct_asset_url"].as_str().filter(|u| !u.is_empty()).or_else(|| link["url"].as_str()).unwrap_or("");
        // Only https downloads, never anything else a release page might list.
        if !url.starts_with("https://") {
            continue;
        }
        let file = url.rsplit('/').next().unwrap_or("");
        let label = link["name"].as_str().unwrap_or("");
        if is_platform_asset(file) || is_platform_asset(label) {
            return CheckResult::Available(Release { version: version.to_string(), notes: v["description"].as_str().unwrap_or("").to_string(), asset_url: url.to_string(), asset_name: if is_platform_asset(file) { file } else { label }.to_string() });
        }
    }
    CheckResult::Manual(format!("Version {version} is available, but no installer was found for this computer. Please download it from the project page."))
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder().timeout_global(Some(std::time::Duration::from_secs(30))).user_agent(USER_AGENT).build().into()
}

/// Ask GitLab whether a newer release exists.
pub fn check() -> CheckResult {
    match agent().get(RELEASE_URL).header("Accept", "application/json").call() {
        Ok(mut r) => match r.body_mut().read_to_string() {
            Ok(body) => parse_release(&body, current_version()),
            Err(e) => CheckResult::Failed(format!("Could not read the update server's answer: {e}")),
        },
        Err(e) => CheckResult::Failed(format!("Could not reach the update server: {e}")),
    }
}

/// Download `url` into the temp folder, reporting `(received, total)`. `cancel` aborts it.
pub fn download(url: &str, file_name: &str, cancel: &AtomicBool, mut progress: impl FnMut(u64, Option<u64>)) -> Result<PathBuf, String> {
    // A fresh folder per download, so a planted file of the same name is never reused.
    let dir = std::env::temp_dir().join(format!("seemymac-update-{}", std::process::id()));
    std::fs::create_dir_all(&dir).map_err(|e| format!("Could not create a download folder: {e}"))?;
    let name = Path::new(file_name).file_name().and_then(|n| n.to_str()).filter(|n| !n.is_empty()).ok_or("The update has no file name.")?;
    let path = dir.join(name);
    let agent = ureq::Agent::config_builder().timeout_connect(Some(std::time::Duration::from_secs(30))).timeout_recv_body(Some(std::time::Duration::from_secs(120))).user_agent(USER_AGENT).build().into();
    let agent: ureq::Agent = agent;
    let mut resp = agent.get(url).call().map_err(|e| format!("The update could not be downloaded: {e}"))?;
    let total = resp.body().content_length();
    let mut reader = resp.body_mut().as_reader();
    let mut out = std::fs::File::create(&path).map_err(|e| format!("The update could not be saved: {e}"))?;
    let (mut buf, mut got) = (vec![0u8; 64 * 1024], 0u64);
    loop {
        if cancel.load(Ordering::Relaxed) {
            let _ = std::fs::remove_file(&path);
            return Err("Cancelled".into());
        }
        let n = reader.read(&mut buf).map_err(|e| format!("The download was interrupted: {e}"))?;
        if n == 0 {
            break;
        }
        out.write_all(&buf[..n]).map_err(|e| format!("The update could not be saved: {e}"))?;
        got += n as u64;
        progress(got, total);
    }
    out.flush().map_err(|e| format!("The update could not be saved: {e}"))?;
    if total.map_or(false, |t| t != got) {
        return Err("The download ended early. Please try again.".into());
    }
    Ok(path)
}

/// Start installing the downloaded update. On success the caller must quit right away: the
/// installer waits for this process to exit before replacing the program, then reopens it.
pub fn install(path: &Path) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    {
        // Inno Setup: silent, waits for our process (see seemymac.iss), then relaunches.
        let child = std::process::Command::new(path)
            .args(["/VERYSILENT", "/SUPPRESSMSGBOXES", "/NORESTART", "/relaunch=1"])
            .arg(format!("/waitpid={}", std::process::id()))
            .spawn();
        return child.map(|_| ()).map_err(|e| format!("The update was downloaded but its installer could not be started: {e}"));
    }
    #[cfg(target_os = "macos")]
    {
        return install_macos(path);
    }
    #[allow(unreachable_code)]
    {
        let _ = path;
        Err("Updates are installed by your software manager on this system.".into())
    }
}

/// The `.app` this program runs from, if it runs from one.
#[cfg(target_os = "macos")]
fn running_bundle() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let app = exe.parent()?.parent()?.parent()?; // .../SeeMyMac.app/Contents/MacOS/see_my_mac
    (app.extension()? == "app").then(|| app.to_path_buf())
}

/// Where the new app goes: over the running one when its folder is writable (a copy in the
/// user's own Applications folder, say), otherwise into `~/Applications` in the user's profile.
#[cfg(target_os = "macos")]
fn install_target() -> Result<PathBuf, String> {
    if let Some(app) = running_bundle() {
        let writable = app.parent().map_or(false, |dir| {
            let probe = dir.join(format!(".seemymac-write-test-{}", std::process::id()));
            let ok = std::fs::File::create(&probe).is_ok();
            let _ = std::fs::remove_file(&probe);
            ok
        });
        if writable {
            return Ok(app);
        }
    }
    let home = std::env::var_os("HOME").map(PathBuf::from).ok_or("Could not find your home folder.")?;
    let dir = home.join("Applications");
    std::fs::create_dir_all(&dir).map_err(|e| format!("Could not create {}: {e}", dir.display()))?;
    Ok(dir.join("SeeMyMac.app"))
}

#[cfg(target_os = "macos")]
fn install_macos(zip: &Path) -> Result<(), String> {
    let target = install_target()?;
    let script = std::env::temp_dir().join(format!("seemymac-update-{}.sh", std::process::id()));
    // Waits for this process to exit, expands the app, swaps it in (the old copy is kept
    // until the new one is in place), clears the download quarantine flag and reopens it.
    let body = r#"#!/bin/bash
PID="$1"; ZIP="$2"; TARGET="$3"
STAGE="$(mktemp -d -t seemymac-update)"
while kill -0 "$PID" 2>/dev/null; do sleep 0.2; done
if ! /usr/bin/ditto -x -k "$ZIP" "$STAGE"; then /usr/bin/open "$STAGE"; exit 1; fi
NEW="$(/usr/bin/find "$STAGE" -maxdepth 2 -name '*.app' -type d | head -n 1)"
[ -n "$NEW" ] || exit 1
/usr/bin/xattr -dr com.apple.quarantine "$NEW" 2>/dev/null || true
OLD="$TARGET.old-$$"
if [ -e "$TARGET" ]; then /bin/mv "$TARGET" "$OLD" || exit 1; fi
if /usr/bin/ditto "$NEW" "$TARGET"; then
    /bin/rm -rf "$OLD"
else
    /bin/rm -rf "$TARGET"; [ -e "$OLD" ] && /bin/mv "$OLD" "$TARGET"
    exit 1
fi
/usr/bin/open "$TARGET"
/bin/rm -rf "$STAGE" "$ZIP" "$0"
"#;
    std::fs::write(&script, body).map_err(|e| format!("Could not prepare the installer: {e}"))?;
    std::process::Command::new("/bin/bash")
        .arg(&script)
        .arg(std::process::id().to_string())
        .arg(zip)
        .arg(&target)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("The update was downloaded but its installer could not be started: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_compare_numerically() {
        assert!(is_newer("0.1.5", "0.1.4"));
        assert!(is_newer("0.2.0", "0.1.9"));
        assert!(is_newer("1.0.0", "0.99.99"));
        assert!(is_newer("0.1.10", "0.1.9"), "10 is higher than 9, not a text comparison");
        assert!(!is_newer("0.1.4", "0.1.4"));
        assert!(!is_newer("0.1.3", "0.1.4"));
    }

    fn release(tag: &str, links: &str, upcoming: bool) -> String {
        format!(r#"{{"tag_name":"{tag}","description":"Notes","upcoming_release":{upcoming},"assets":{{"links":[{links}]}}}}"#)
    }

    /// Talks to GitLab: `cargo test live_check -- --ignored`.
    #[test]
    #[ignore]
    fn live_check_reaches_gitlab() {
        let r = check();
        println!("{r:?}");
        assert!(!matches!(r, CheckResult::Failed(_)), "{r:?}");
    }

    #[test]
    fn same_or_older_release_is_up_to_date() {
        assert_eq!(parse_release(&release("v0.1.4", "", false), "0.1.4"), CheckResult::UpToDate);
        assert_eq!(parse_release(&release("v0.1.3", "", false), "0.1.4"), CheckResult::UpToDate);
        assert_eq!(parse_release(&release("v9.9.9", "", true), "0.1.4"), CheckResult::UpToDate, "upcoming releases are not offered");
    }

    #[test]
    fn bad_tags_are_reported() {
        for tag in ["", "latest", "v1.2", "v1.2.x", "v1.2.3-beta"] {
            assert!(matches!(parse_release(&release(tag, "", false), "0.1.4"), CheckResult::Failed(_)), "{tag}");
        }
        assert!(matches!(parse_release("not json", "0.1.4"), CheckResult::Failed(_)));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_picks_the_zip_over_https_only() {
        let links = r#"{"name":"SeeMyMac.flatpak","direct_asset_url":"https://x/SeeMyMac.flatpak"},
            {"name":"mac","direct_asset_url":"http://x/SeeMyMac-0.2.0-macOS.zip"},
            {"name":"mac","direct_asset_url":"https://x/SeeMyMac-0.2.0-macOS.zip"}"#;
        match parse_release(&release("v0.2.0", links, false), "0.1.4") {
            CheckResult::Available(r) => assert_eq!((r.version.as_str(), r.asset_url.as_str(), r.asset_name.as_str()), ("0.2.0", "https://x/SeeMyMac-0.2.0-macOS.zip", "SeeMyMac-0.2.0-macOS.zip")),
            other => panic!("{other:?}"),
        }
        assert!(matches!(parse_release(&release("v0.2.0", r#"{"name":"f","direct_asset_url":"https://x/SeeMyMac.flatpak"}"#, false), "0.1.4"), CheckResult::Manual(_)));
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_picks_the_combined_installer_and_never_debug() {
        let links = r#"{"name":"d","direct_asset_url":"https://x/SeeMyMac-0.2.0-debug-setup.exe"},{"name":"o","direct_asset_url":"https://x/SeeMyMac-0.2.0-other-setup.exe"},{"name":"h","direct_asset_url":"http://x/SeeMyMac-0.2.0-setup.exe"},{"name":"r","direct_asset_url":"https://x/SeeMyMac-0.2.0-setup.exe"}"#;
        match parse_release(&release("v0.2.0", links, false), "0.1.4") {
            CheckResult::Available(r) => assert_eq!((r.asset_url.as_str(), r.asset_name.as_str()), ("https://x/SeeMyMac-0.2.0-setup.exe", "SeeMyMac-0.2.0-setup.exe")),
            other => panic!("{other:?}"),
        }
        let only_debug = r#"{"name":"d","direct_asset_url":"https://x/SeeMyMac-0.2.0-debug-setup.exe"}"#;
        assert!(matches!(parse_release(&release("v0.2.0", only_debug, false), "0.1.4"), CheckResult::Manual(_)));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_points_to_flatpak() {
        assert!(matches!(parse_release(&release("v0.2.0", "", false), "0.1.4"), CheckResult::Manual(m) if m.contains("flatpak update")));
    }
}

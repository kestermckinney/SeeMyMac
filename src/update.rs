//! Self-update: checks GitHub and GitLab for releases, downloads the installer for this
//! platform and starts it. Windows runs the Inno Setup installer silently (it waits for
//! this process to exit, then relaunches the app); macOS replaces the app bundle with the
//! downloaded one from a helper script that waits for this process. Linux builds show a link to GitHub when a newer release is published there.
//! Flatpak still handles installation on Linux.
//!
//! Everything here is blocking and meant for a background thread; `app.rs` polls the result.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

const GITHUB_RELEASE_URL: &str = "https://api.github.com/repos/kestermckinney/SeeMyMac/releases/latest";
const GITLAB_RELEASE_URL: &str = "https://gitlab.com/api/v4/projects/kestermckinney%2FSeeMyMac/releases/permalink/latest";
const USER_AGENT: &str = concat!("SeeMyMac-Updater/", env!("CARGO_PKG_VERSION"));

pub const GITHUB_PROJECT_URL: &str = "https://github.com/kestermckinney/SeeMyMac";

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
    GitHubNotice(String),
    /// A newer version exists but cannot be installed from here (no installer published for
    /// this platform); the text tells the user what to do.
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

#[derive(Clone, Copy)]
enum ReleaseSource {
    GitHub,
    GitLab,
}

impl ReleaseSource {
    fn endpoint(self) -> (&'static str, &'static str, &'static str) {
        match self {
            Self::GitHub => ("GitHub", GITHUB_RELEASE_URL, "application/vnd.github+json"),
            Self::GitLab => ("GitLab", GITLAB_RELEASE_URL, "application/json"),
        }
    }
}

/// Interpret a release response for a program that is running `current`.
fn parse_release(json: &str, current: &str, source: ReleaseSource) -> CheckResult {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(json) else {
        return CheckResult::Failed("Unexpected response from the update server.".into());
    };
    let unpublished = match source {
        ReleaseSource::GitHub => {
            v["draft"].as_bool() == Some(true) || v["prerelease"].as_bool() == Some(true)
        }
        ReleaseSource::GitLab => v["upcoming_release"].as_bool() == Some(true),
    };
    if unpublished {
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
    if cfg!(target_os = "linux") && matches!(source, ReleaseSource::GitHub) {
        return CheckResult::GitHubNotice(version.into());
    }
    let (assets, notes, project) = match source {
        ReleaseSource::GitHub => (&v["assets"], &v["body"], "https://github.com/kestermckinney/SeeMyMac/releases"),
        ReleaseSource::GitLab => (&v["assets"]["links"], &v["description"], "https://gitlab.com/kestermckinney/SeeMyMac/-/releases"),
    };
    for asset in assets.as_array().into_iter().flatten() {
        let url = match source {
            ReleaseSource::GitHub => asset["browser_download_url"].as_str(),
            ReleaseSource::GitLab => asset["direct_asset_url"].as_str()
                .filter(|url| !url.is_empty())
                .or_else(|| asset["url"].as_str()),
        }.unwrap_or("");
        // Only https downloads, never anything else a release page might list.
        if !url.starts_with("https://") {
            continue;
        }
        let file = url.rsplit('/').next().unwrap_or("");
        let label = asset["name"].as_str().unwrap_or("");
        if is_platform_asset(file) || is_platform_asset(label) {
            return CheckResult::Available(Release { version: version.to_string(), notes: notes.as_str().unwrap_or("").to_string(), asset_url: url.to_string(), asset_name: if is_platform_asset(file) { file } else { label }.to_string() });
        }
    }
    CheckResult::Manual(format!("Version {version} is available, but no installer was found for this computer. Please download it from {project}."))
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder().timeout_global(Some(std::time::Duration::from_secs(30))).user_agent(USER_AGENT).build().into()
}

fn check_source(source: ReleaseSource) -> Option<CheckResult> {
    let (name, url, accept) = source.endpoint();
    Some(match agent().get(url).header("Accept", accept).call() {
        Ok(mut response) => match response.body_mut().read_to_string() {
            Ok(body) => parse_release(&body, current_version(), source),
            Err(e) => CheckResult::Failed(format!("Could not read the update response from {name}: {e}")),
        },
        Err(ureq::Error::StatusCode(404)) => return None,
        Err(e) => CheckResult::Failed(format!("Could not reach {name} for updates: {e}")),
    })
}

fn select_update(github: Option<CheckResult>, gitlab: Option<CheckResult>) -> CheckResult {
    use CheckResult::*;
    match (github, gitlab) {
        (Some(Available(github)), Some(Available(gitlab))) => {
            if is_newer(&gitlab.version, &github.version) {
                Available(gitlab)
            } else {
                Available(github)
            }
        }
        (Some(result @ Available(_)), _) | (_, Some(result @ Available(_))) |
        (Some(result @ (Manual(_) | GitHubNotice(_))), _) | (_, Some(result @ (Manual(_) | GitHubNotice(_)))) => result,
        (Some(UpToDate), _) | (_, Some(UpToDate)) | (None, None) => UpToDate,
        (Some(Failed(github)), Some(Failed(gitlab))) => Failed(format!("{github}\n{gitlab}")),
        (Some(result @ Failed(_)), None) | (None, Some(result @ Failed(_))) => result,
    }
}

/// Check for installers on Windows and macOS, or a GitHub release notice on Linux.
pub fn check() -> CheckResult {
    if cfg!(target_os = "linux") {
        return check_source(ReleaseSource::GitHub).unwrap_or(CheckResult::UpToDate);
    }
    std::thread::scope(|scope| {
        let github = scope.spawn(|| check_source(ReleaseSource::GitHub));
        let gitlab = check_source(ReleaseSource::GitLab);
        select_update(github.join().expect("GitHub update check panicked"), gitlab)
    })
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

    fn parse_gitlab_release(json: &str, current: &str) -> CheckResult {
        super::parse_release(json, current, ReleaseSource::GitLab)
    }

    fn release(tag: &str, links: &str, upcoming: bool) -> String {
        format!(r#"{{"tag_name":"{tag}","description":"Notes","upcoming_release":{upcoming},"assets":{{"links":[{links}]}}}}"#)
    }

    /// Talks to the release feeds: `cargo test live_check -- --ignored`.
    #[test]
    #[ignore]
    fn live_check_reaches_release_feeds() {
        let r = check();
        println!("{r:?}");
        assert!(!matches!(r, CheckResult::Failed(_)), "{r:?}");
    }

    #[test]
    fn github_release_uses_uploaded_assets_and_notes() {
        let name = if cfg!(target_os = "windows") {
            "SeeMyMac-9.0.0-setup.exe"
        } else {
            "SeeMyMac-9.0.0-macOS.zip"
        };
        let url = format!("https://github.com/kestermckinney/SeeMyMac/releases/download/v9.0.0/{name}");
        let mut release = serde_json::json!({
            "tag_name": "v9.0.0",
            "body": "GitHub notes",
            "assets": [{"name": name, "browser_download_url": url}],
        });
        let result = super::parse_release(&release.to_string(), "8.0.0", ReleaseSource::GitHub);
        if cfg!(target_os = "linux") {
            assert_eq!(result, CheckResult::GitHubNotice("9.0.0".into()));
        } else {
            assert_eq!(result, CheckResult::Available(Release {
                version: "9.0.0".into(),
                notes: "GitHub notes".into(),
                asset_url: url,
                asset_name: name.into(),
            }));
        }
        for flag in ["draft", "prerelease"] {
            release[flag] = true.into();
            assert_eq!(
                super::parse_release(&release.to_string(), "8.0.0", ReleaseSource::GitHub),
                CheckResult::UpToDate
            );
            release[flag] = false.into();
        }
    }

    #[test]
    fn selects_newest_installer_and_prefers_github_on_ties() {
        use CheckResult::*;
        let available = |version: &str, source: &str| Available(Release {
            version: version.into(),
            notes: source.into(),
            asset_url: String::new(),
            asset_name: String::new(),
        });
        let github = available("2.0.0", "GitHub");
        let gitlab = available("2.0.0", "GitLab");
        let newer_gitlab = available("3.0.0", "GitLab");
        assert_eq!(select_update(Some(github.clone()), Some(gitlab.clone())), github);
        assert_eq!(select_update(Some(github), Some(newer_gitlab.clone())), newer_gitlab);
        assert_eq!(select_update(None, Some(gitlab.clone())), gitlab);
        assert_eq!(select_update(Some(Failed("offline".into())), Some(gitlab.clone())), gitlab);
        assert_eq!(select_update(Some(Manual("no installer".into())), Some(gitlab.clone())), gitlab);
        assert_eq!(select_update(None, Some(UpToDate)), UpToDate);
        assert_eq!(select_update(None, Some(Failed("offline".into()))), Failed("offline".into()));
    }

    #[test]
    fn same_or_older_release_is_up_to_date() {
        assert_eq!(parse_gitlab_release(&release("v0.1.4", "", false), "0.1.4"), CheckResult::UpToDate);
        assert_eq!(parse_gitlab_release(&release("v0.1.3", "", false), "0.1.4"), CheckResult::UpToDate);
        assert_eq!(parse_gitlab_release(&release("v9.9.9", "", true), "0.1.4"), CheckResult::UpToDate, "upcoming releases are not offered");
    }

    #[test]
    fn bad_tags_are_reported() {
        for tag in ["", "latest", "v1.2", "v1.2.x", "v1.2.3-beta"] {
            assert!(matches!(parse_gitlab_release(&release(tag, "", false), "0.1.4"), CheckResult::Failed(_)), "{tag}");
        }
        assert!(matches!(parse_gitlab_release("not json", "0.1.4"), CheckResult::Failed(_)));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_picks_the_zip_over_https_only() {
        let links = r#"{"name":"SeeMyMac.flatpak","direct_asset_url":"https://x/SeeMyMac.flatpak"},
            {"name":"mac","direct_asset_url":"http://x/SeeMyMac-0.2.0-macOS.zip"},
            {"name":"mac","direct_asset_url":"https://x/SeeMyMac-0.2.0-macOS.zip"}"#;
        match parse_gitlab_release(&release("v0.2.0", links, false), "0.1.4") {
            CheckResult::Available(r) => assert_eq!((r.version.as_str(), r.asset_url.as_str(), r.asset_name.as_str()), ("0.2.0", "https://x/SeeMyMac-0.2.0-macOS.zip", "SeeMyMac-0.2.0-macOS.zip")),
            other => panic!("{other:?}"),
        }
        assert!(matches!(parse_gitlab_release(&release("v0.2.0", r#"{"name":"f","direct_asset_url":"https://x/SeeMyMac.flatpak"}"#, false), "0.1.4"), CheckResult::Manual(_)));
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_picks_the_combined_installer_and_never_debug() {
        let links = r#"{"name":"d","direct_asset_url":"https://x/SeeMyMac-0.2.0-debug-setup.exe"},{"name":"o","direct_asset_url":"https://x/SeeMyMac-0.2.0-other-setup.exe"},{"name":"h","direct_asset_url":"http://x/SeeMyMac-0.2.0-setup.exe"},{"name":"r","direct_asset_url":"https://x/SeeMyMac-0.2.0-setup.exe"}"#;
        match parse_gitlab_release(&release("v0.2.0", links, false), "0.1.4") {
            CheckResult::Available(r) => assert_eq!((r.asset_url.as_str(), r.asset_name.as_str()), ("https://x/SeeMyMac-0.2.0-setup.exe", "SeeMyMac-0.2.0-setup.exe")),
            other => panic!("{other:?}"),
        }
        let only_debug = r#"{"name":"d","direct_asset_url":"https://x/SeeMyMac-0.2.0-debug-setup.exe"}"#;
        assert!(matches!(parse_gitlab_release(&release("v0.2.0", only_debug, false), "0.1.4"), CheckResult::Manual(_)));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_notice_requires_a_newer_github_release() {
        let release = r#"{"tag_name":"v2.0.0","assets":[]}"#;
        assert_eq!(super::parse_release(release, "1.9.0", ReleaseSource::GitHub), CheckResult::GitHubNotice("2.0.0".into()));
        assert_eq!(super::parse_release(release, "2.0.0", ReleaseSource::GitHub), CheckResult::UpToDate);
        assert_eq!(super::parse_release(release, "2.1.0", ReleaseSource::GitHub), CheckResult::UpToDate);
    }
}

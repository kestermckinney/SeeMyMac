//! Records when the program was built, for the About screen (UTC, `SOURCE_DATE_EPOCH` honoured
//! so reproducible builds stay reproducible).

fn main() {
    let secs = std::env::var("SOURCE_DATE_EPOCH")
        .ok()
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or_else(|| std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64));
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + (m <= 2) as i64;
    println!("cargo:rustc-env=SEEMYMAC_BUILD_TIME={y:04}-{m:02}-{d:02} {:02}:{:02}:{:02} UTC", rem / 3600, rem % 3600 / 60, rem % 60);
    embed_macos_name();
    embed_windows_icon();
}

/// Run outside an .app bundle (`cargo run`), macOS names the application menu after the
/// executable, `see_my_mac`. An Info.plist embedded in the binary's `__TEXT,__info_plist`
/// section gives it the bundle's name instead. (The .app's own Info.plist takes precedence.)
fn embed_macos_name() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos") {
        return;
    }
    let out = std::path::PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR is set by cargo")).join("Info.plist");
    let plist = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>CFBundleInfoDictionaryVersion</key><string>6.0</string>
	<key>CFBundleName</key><string>SeeMyMac</string>
	<key>CFBundleDisplayName</key><string>SeeMyMac</string>
</dict>
</plist>
"#;
    std::fs::write(&out, plist).expect("write the embedded Info.plist");
    println!("cargo:rustc-link-arg-bin=see_my_mac=-Wl,-sectcreate,__TEXT,__info_plist,{}", out.display());
}

/// On Windows the executable carries its icon (`packaging/windows/seemymac.rc`), which Explorer,
/// shortcuts and the taskbar read from the file itself.
fn embed_windows_icon() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    println!("cargo:rerun-if-changed=packaging/windows/seemymac.rc");
    println!("cargo:rerun-if-changed=assets/icons/seemac.ico");
    embed_resource::compile("packaging/windows/seemymac.rc", embed_resource::NONE)
        .manifest_optional()
        .expect("embed the Windows icon resource");
}

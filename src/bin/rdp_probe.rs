//! Drive an RDP connection from the command line and report each stage.
//!
//!   SEEMYMAC_USER=me SEEMYMAC_PASS=secret cargo run --bin rdp_probe -- HOST[:PORT] [seconds]
//!   SEEMYMAC_USER=me cargo run --bin rdp_probe -- HOST   (asks for the password, hidden)
//!   cargo run --bin rdp_probe -- @CONNECTION_ID   (a connection saved in SeeMyMac)
//!
//! Against the bundled test server (tools/rdp-test-server) it also checks the
//! picture (a known background pixel) and sends a click, a key and clipboard text,
//! which the server prints as it receives them. SEEMYMAC_PROBE_SHARE=/some/dir
//! offers that folder as a drive; SEEMYMAC_PROBE_PRINTER=1 offers the default
//! printer; SEEMYMAC_PROBE_NO_AUDIO=1 turns audio off; SEEMYMAC_PROBE_MONITORS=WxH,WxH
//! asks for that monitor layout; SEEMYMAC_PROBE_NO_INPUT=1 sends no input at all (use on a live session);
//! SEEMYMAC_PROBE_SNAPSHOT=file.png saves a picture of the remote desktop; SEEMYMAC_PROBE_SMARTCARD=1 offers the local card readers; SEEMYMAC_PROBE_SENDFILE=path copies that file to the server
//! (instead of clipboard text) and SEEMYMAC_PROBE_DOWNLOADS=dir is where files from the server go.

use see_my_mac::rdp;
use see_my_mac::rfb::{Event, Input};
use std::time::{Duration, Instant};

/// Ask for the password without echoing it (needs a terminal).
fn prompt_password(user: &str, host: &str) -> String {
    use std::io::{BufRead, Write};
    eprint!("Password for {user}@{host}: ");
    let _ = std::io::stderr().flush();
    let off = std::process::Command::new("stty").arg("-echo").status().map(|s| s.success()).unwrap_or(false);
    let mut line = String::new();
    let _ = std::io::stdin().lock().read_line(&mut line);
    if off {
        let _ = std::process::Command::new("stty").arg("echo").status();
        eprintln!();
    }
    line.trim_end_matches(['\r', '\n']).to_string()
}

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("see_my_mac=debug")).init();
    let mut args = std::env::args().skip(1);
    let target = args.next().expect("usage: rdp_probe HOST[:PORT] [seconds]");
    let secs: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(10);
    // @ID uses a connection saved in SeeMyMac (its keyring password); otherwise the password comes
    // from SEEMYMAC_PASS, or is typed at a hidden prompt.
    let (mut host, mut port, mut user, mut pass) = (target.clone(), 3389u16, std::env::var("SEEMYMAC_USER").unwrap_or_default(), std::env::var("SEEMYMAC_PASS").ok());
    if let Some(id) = target.strip_prefix('@') {
        let store = see_my_mac::model::Store::load(see_my_mac::model::Store::default_path());
        let c = store.connections.iter().find(|c| c.id == id).unwrap_or_else(|| panic!("no saved connection {id}")).clone();
        host = c.host.clone();
        port = c.port_number();
        if user.is_empty() {
            user = c.user.clone();
        }
        if pass.is_none() {
            pass = see_my_mac::vault::get(id).expect("keyring");
        }
    } else if let Some((h, p)) = target.rsplit_once(':').filter(|(_, p)| p.parse::<u16>().is_ok()) {
        host = h.to_string();
        port = p.parse().unwrap();
    }
    let pass = pass.unwrap_or_else(|| prompt_password(&user, &host));
    let mut cfg = rdp::Config::new(host, port, user, pass);
    cfg.share_folder = std::env::var_os("SEEMYMAC_PROBE_SHARE").map(Into::into);
    cfg.printer = std::env::var_os("SEEMYMAC_PROBE_PRINTER").is_some();
    cfg.smartcard = std::env::var_os("SEEMYMAC_PROBE_SMARTCARD").is_some();
    if let Ok(d) = std::env::var("SEEMYMAC_PROBE_PRINTER_DRIVER") {
        cfg.printer_driver = d;
    }
    cfg.remote_app = std::env::var("SEEMYMAC_PROBE_REMOTEAPP").ok();
    cfg.audio = std::env::var_os("SEEMYMAC_PROBE_NO_AUDIO").is_none();
    if let Some(d) = std::env::var_os("SEEMYMAC_PROBE_DOWNLOADS") {
        cfg.download_dir = d.into();
    }
    // SEEMYMAC_PROBE_MONITORS=1920x1080,1280x720 asks for two monitors side by side.
    if let Ok(list) = std::env::var("SEEMYMAC_PROBE_MONITORS") {
        let mut x = 0;
        for m in list.split(',') {
            let (w, h) = m.split_once('x').expect("WxH");
            let (w, h): (u32, u32) = (w.parse().unwrap(), h.parse().unwrap());
            cfg.monitors.push(rdp::MonitorRect { x, y: 0, w, h });
            x += w as i32;
        }
    }

    let t0 = Instant::now();
    let h = rdp::connect(cfg);
    let end = t0 + Duration::from_secs(secs);
    // SEEMYMAC_PROBE_NO_INPUT=1 only watches: no clicks, keys, clipboard or resize (for live sessions).
    let watch_only = std::env::var_os("SEEMYMAC_PROBE_NO_INPUT").is_some();
    // SEEMYMAC_PROBE_RESIZE=WxH asks for that desktop size even when only watching (resizes the live session).
    let resize_to: (u32, u32) = std::env::var("SEEMYMAC_PROBE_RESIZE").ok().and_then(|s| s.split_once('x').and_then(|(w, h)| Some((w.parse().ok()?, h.parse().ok()?)))).unwrap_or((800, 600));
    let ask_resize = !watch_only || std::env::var_os("SEEMYMAC_PROBE_RESIZE").is_some();
    let (mut connected, mut sent, mut first_frame, mut resized, mut asked) = (false, watch_only, None, false, !ask_resize);
    let mut disconnected = None;
    while Instant::now() < end && disconnected.is_none() {
        while let Ok(ev) = h.events.try_recv() {
            match ev {
                Event::Stage(s) => println!("[{:>5} ms] stage: {s}", t0.elapsed().as_millis()),
                Event::Connected { width, height, name } => {
                    connected = true;
                    println!("[{:>5} ms] CONNECTED {width}x{height} {name}", t0.elapsed().as_millis());
                }
                Event::ServerClipboard(t) => println!("[{:>5} ms] server clipboard: {t:?}", t0.elapsed().as_millis()),
                Event::Disconnected(r) => {
                    println!("[{:>5} ms] DISCONNECTED {r:?}", t0.elapsed().as_millis());
                    disconnected = Some(r);
                }
                Event::SessionEnded(reason) => {
                    println!("[{:>5} ms] SESSION ENDED {reason}", t0.elapsed().as_millis());
                    disconnected = Some(Some(reason));
                }
                other => println!("[{:>5} ms] event: {other:?}", t0.elapsed().as_millis()),
            }
        }
        if connected && first_frame.is_none() {
            let f = h.fb.lock().unwrap();
            if f.version > 1 && f.bgra.len() >= 4 && f.bgra[..4] != [0, 0, 0, 0] {
                first_frame = Some(Instant::now());
                println!("[{:>5} ms] FIRST PICTURE {}x{} pixel(5,5)={:02x?}", t0.elapsed().as_millis(), f.width, f.height, &f.bgra[(5 * f.width as usize + 5) * 4..][..4]);
            }
        }
        if let (Some(t), false) = (first_frame, sent) {
            if t.elapsed() > Duration::from_millis(500) {
                sent = true;
                h.send(Input::Pointer { x: 100, y: 120, buttons: 0 });
                h.send(Input::Pointer { x: 100, y: 120, buttons: 1 });
                h.send(Input::Pointer { x: 100, y: 120, buttons: 0 });
                h.send(Input::Pointer { x: 100, y: 120, buttons: 8 }); // wheel up
                h.send(Input::Pointer { x: 100, y: 120, buttons: 0 });
                for (k, d) in [('a' as u32, true), ('a' as u32, false), (0xff0d, true), (0xff0d, false), (0xffe3, true), ('c' as u32, true), ('c' as u32, false), (0xffe3, false)] {
                    h.send(Input::Key { keysym: k, down: d });
                }
                match std::env::var_os("SEEMYMAC_PROBE_SENDFILE") {
                    Some(f) => h.send(Input::SendFiles(vec![f.into()])),
                    None => h.send(Input::Clipboard("hello from SeeMyMac".into())),
                }
                println!("[{:>5} ms] sent click, wheel, keys a / Enter / Ctrl+C and clipboard text", t0.elapsed().as_millis());
            }
        }
        // The resize comes later, on its own: the server redoes its channel setup for it.
        if let (Some(t), true, false) = (first_frame, sent, asked) {
            if t.elapsed() > Duration::from_millis(2000) {
                asked = true;
                h.send(Input::UseVirtualDisplay { width: resize_to.0, height: resize_to.1 });
                println!("[{:>5} ms] asked for a resize to {}x{}", t0.elapsed().as_millis(), resize_to.0, resize_to.1);
            }
        }
        if asked && !resized {
            let f = h.fb.lock().unwrap();
            if (f.width, f.height) == resize_to {
                resized = true;
                println!("[{:>5} ms] RESIZED to {}x{}", t0.elapsed().as_millis(), f.width, f.height);
            }
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let f = h.fb.lock().unwrap();
    // SEEMYMAC_PROBE_SNAPSHOT=file.png saves what the remote desktop looked like at the end.
    if let Some(path) = std::env::var_os("SEEMYMAC_PROBE_SNAPSHOT") {
        let mut rgba = Vec::with_capacity(f.bgra.len());
        for p in f.bgra.chunks_exact(4) {
            rgba.extend_from_slice(&[p[2], p[1], p[0], 255]);
        }
        match image::RgbaImage::from_raw(f.width, f.height, rgba).map(|img| img.save(&path)) {
            Some(Ok(())) => println!("snapshot saved to {}", std::path::Path::new(&path).display()),
            Some(Err(e)) => eprintln!("snapshot failed: {e}"),
            None => eprintln!("snapshot failed: no picture"),
        }
    }
    println!("final: version {} size {}x{} screens {:?}", f.version, f.width, f.height, f.screens);
    drop(f);
    h.close();
    std::thread::sleep(Duration::from_millis(300));
    match disconnected {
        Some(Some(e)) => {
            eprintln!("FAILED: {e}");
            std::process::exit(1);
        }
        _ if !connected => {
            eprintln!("FAILED: never connected");
            std::process::exit(1);
        }
        _ if std::env::var_os("SEEMYMAC_PROBE_EXPECT_RESIZE").is_some() && !resized => {
            eprintln!("FAILED: the desktop was not resized");
            std::process::exit(1);
        }
        _ => println!("OK"),
    }
}

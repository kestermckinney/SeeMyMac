//! Drive an Apple High Performance connection from the command line and report
//! each stage with timings. Credentials come from the environment:
//!
//!   SEEMYMAC_USER=me SEEMYMAC_PASS=secret cargo run --release --bin hp_probe -- 192.168.1.10 [seconds]
//!
//! or, using a connection saved in SeeMyMac with its password in the keyring:
//!
//!   cargo run --release --bin hp_probe -- @c4 [seconds]

use see_my_mac::hp;
use see_my_mac::rfb::Event;
use std::time::{Duration, Instant};

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("see_my_mac=debug")).init();
    let mut args = std::env::args().skip(1);
    if std::env::args().nth(1).as_deref() == Some("--decode") {
        return replay(&std::env::args().nth(2).expect("--decode FILE"));
    }
    let target = args.next().expect("usage: hp_probe HOST|@CONNECTION_ID [seconds]");
    let secs: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(30);
    let (host, mut username, mut password) = (target.clone(), std::env::var("SEEMYMAC_USER").unwrap_or_default(), std::env::var("SEEMYMAC_PASS").unwrap_or_default());
    let host = match target.strip_prefix('@') {
        Some(id) => {
            let store = see_my_mac::model::Store::load(see_my_mac::model::Store::default_path());
            let c = store.connections.iter().find(|c| c.id == id).unwrap_or_else(|| panic!("no saved connection {id}")).clone();
            if username.is_empty() {
                username = c.user.clone();
            }
            if password.is_empty() {
                password = see_my_mac::vault::get(id).expect("keyring").unwrap_or_else(|| panic!("no saved password for {id}"));
            }
            c.host
        }
        None => host,
    };
    println!("connecting to {host} as {username:?} (password {} chars)", password.chars().count());
    let cfg = hp::Config {
        host,
        port: 5900,
        username,
        password,
        width: 1920,
        height: 1080,
        virtual_display: std::env::var_os("SEEMYMAC_NO_VIRTUAL").is_none(),
        demo: false,
    };
    let t0 = Instant::now();
    let h = hp::connect(cfg);
    let end = t0 + Duration::from_secs(secs);
    // SEEMYMAC_PROBE_POINTER="x,y;x,y": after the first picture, move the pointer to
    // each framebuffer position in turn (2 s apart) and snapshot just before the next.
    // "x,y" moves; "x,y,c" also clicks; "esc" taps Escape.
    let moves: Vec<(u16, u16, &str)> = std::env::var("SEEMYMAC_PROBE_POINTER")
        .unwrap_or_default()
        .leak()
        .split(';')
        .filter_map(|p| {
            if p.trim() == "esc" {
                return Some((0, 0, "esc"));
            }
            if let Some(k) = p.trim().strip_prefix("kd:") {
                return Some((u16::from_str_radix(k, 16).ok()?, 0, "keydown"));
            }
            if let Some(k) = p.trim().strip_prefix("ku:") {
                return Some((u16::from_str_radix(k, 16).ok()?, 0, "keyup"));
            }
            if let Some(k) = p.trim().strip_prefix("k:") {
                // "k:HEX" taps one X11 keysym.
                return Some((u16::from_str_radix(k, 16).ok()?, 0, "key"));
            }
            let mut it = p.split(',');
            Some((it.next()?.trim().parse().ok()?, it.next()?.trim().parse().ok()?, it.next().unwrap_or("").trim()))
        })
        .collect();
    let mut first_frame_at: Option<Instant> = None;
    let mut move_idx = 0;
    let mut last_version = 0;
    let mut frames = 0u64;
    while Instant::now() < end {
        match h.events.recv_timeout(Duration::from_millis(200)) {
            Ok(Event::Disconnected(r)) => {
                println!("[{:6.2}s] DISCONNECTED: {r:?}", t0.elapsed().as_secs_f32());
                return;
            }
            Ok(e) => println!("[{:6.2}s] {e:?}", t0.elapsed().as_secs_f32()),
            Err(_) => {}
        }
        if let Some(t1) = first_frame_at {
            let due = t1 + Duration::from_secs(2 * (move_idx as u64 + 1));
            if Instant::now() >= due && move_idx <= moves.len() {
                if move_idx > 0 {
                    if let Some(dir) = std::env::var_os("SEEMYMAC_PROBE_SHOTS") {
                        let f = h.fb.lock().unwrap();
                        let mut ppm = format!("P6\n{} {}\n255\n", f.width, f.height).into_bytes();
                        for px in f.bgra.chunks_exact(4) {
                            ppm.extend_from_slice(&[px[2], px[1], px[0]]);
                        }
                        let (x, y, _) = moves[move_idx - 1];
                        let _ = std::fs::write(std::path::Path::new(&dir).join(format!("step{move_idx}_{x}_{y}.ppm")), ppm);
                    }
                }
                if move_idx == 0 && std::env::var_os("SEEMYMAC_PROBE_TAP_SHIFT").is_some() {
                    println!("[{:6.2}s] tap Shift", t0.elapsed().as_secs_f32());
                    h.send(see_my_mac::rfb::Input::Key { keysym: 0xffe1, down: true });
                    h.send(see_my_mac::rfb::Input::Key { keysym: 0xffe1, down: false });
                }
                if let Some(&(x, y, what)) = moves.get(move_idx) {
                    use see_my_mac::rfb::Input;
                    if what == "keydown" || what == "keyup" {
                        println!("[{:6.2}s] {what} {x:#x}", t0.elapsed().as_secs_f32());
                        h.send(Input::Key { keysym: x as u32, down: what == "keydown" });
                    } else if what == "key" {
                        println!("[{:6.2}s] tap keysym {x:#x}", t0.elapsed().as_secs_f32());
                        h.send(Input::Key { keysym: x as u32, down: true });
                        h.send(Input::Key { keysym: x as u32, down: false });
                    } else if what == "esc" {
                        println!("[{:6.2}s] tap Escape", t0.elapsed().as_secs_f32());
                        h.send(Input::Key { keysym: 0xff1b, down: true });
                        h.send(Input::Key { keysym: 0xff1b, down: false });
                    } else {
                        println!("[{:6.2}s] pointer -> {x},{y} {what}", t0.elapsed().as_secs_f32());
                        // A small wiggle so the Mac redraws the cursor at the final spot.
                        for d in [6u16, 3, 0] {
                            h.send(Input::Pointer { x: x.saturating_sub(d), y: y.saturating_sub(d), buttons: 0 });
                            std::thread::sleep(Duration::from_millis(30));
                        }
                        if what == "c" {
                            h.send(Input::Pointer { x, y, buttons: 1 });
                            std::thread::sleep(Duration::from_millis(60));
                            h.send(Input::Pointer { x, y, buttons: 0 });
                        }
                    }
                }
                move_idx += 1;
            }
        }
        let f = h.fb.lock().unwrap();
        if f.version != last_version {
            frames += f.version - last_version;
            first_frame_at.get_or_insert_with(Instant::now);
            last_version = f.version;
            if let Some(dir) = std::env::var_os("SEEMYMAC_PROBE_SHOTS").filter(|_| frames == 1 || frames % 150 == 0) {
                // Binary PPM (RGB) snapshot of the framebuffer.
                let mut ppm = format!("P6\n{} {}\n255\n", f.width, f.height).into_bytes();
                for px in f.bgra.chunks_exact(4) {
                    ppm.extend_from_slice(&[px[2], px[1], px[0]]);
                }
                let _ = std::fs::write(std::path::Path::new(&dir).join(format!("frame{frames}.ppm")), ppm);
            }
            if frames == 1 || frames % 60 == 0 {
                let mean = f.bgra.iter().step_by(97).map(|&b| b as u64).sum::<u64>() / (f.bgra.len() as u64 / 97).max(1);
                println!("[{:6.2}s] frame #{frames} {}x{} logical {:?} mean {mean}", t0.elapsed().as_secs_f32(), f.width, f.height, f.logical);
            }
        }
    }
    println!("[{:6.2}s] done: {frames} frames", t0.elapsed().as_secs_f32());
    h.close();
    std::thread::sleep(Duration::from_secs(1));
}

/// Feed an Annex-B dump (SEEMYMAC_HP_DUMP; H.264 or HEVC) through the app's decoder.
fn replay(path: &str) {
    let data = std::fs::read(path).expect("read dump");
    let mut starts: Vec<usize> = data.windows(4).enumerate().filter(|(_, w)| *w == [0, 0, 0, 1]).map(|(i, _)| i + 4).collect();
    starts.push(data.len() + 4);
    let nals: Vec<&[u8]> = starts.windows(2).map(|w| &data[w[0]..w[1] - 4]).collect();
    if nals.first().map_or(false, |n| (32..=34).contains(&hp::decode::hevc_type(n))) {
        return replay_hevc(&nals);
    }
    let sps = nals.iter().find(|n| n[0] & 0x1f == 7).expect("no SPS").to_vec();
    let pps = nals.iter().find(|n| n[0] & 0x1f == 8).expect("no PPS").to_vec();
    let mut dec = hp::decode::AvcDecoder::new(sps, pps);
    let mut bgra = vec![];
    let (mut pictures, mut errors, mut total) = (0, 0, 0.0);
    for (i, nal) in nals.iter().enumerate().filter(|(_, n)| matches!(n[0] & 0x1f, 1 | 5)) {
        let t0 = Instant::now();
        let r = dec.feed(nal, &mut bgra);
        let ms = t0.elapsed().as_secs_f64() * 1000.0;
        match r {
            Ok(Some(p)) => {
                pictures += 1;
                total += ms;
                if std::env::var_os("VERBOSE").is_some() {
                    println!("#{i:4} type {:2} {:7} bytes  {}x{} {ms:.1} ms", nal[0] & 0x1f, nal.len(), p.width, p.height);
                }
            }
            Ok(None) => {}
            Err(e) => {
                errors += 1;
                println!("#{i:4} type {:2} {:7} bytes  {e}", nal[0] & 0x1f, nal.len());
            }
        }
    }
    let t0 = Instant::now();
    if let Ok(Some(_)) = dec.finish(&mut bgra) {
        pictures += 1;
    }
    total += t0.elapsed().as_secs_f64() * 1000.0;
    println!("{pictures} pictures returned, {errors} errors, {:.1} ms of feeding per returned picture (pictures decode in parallel)", total / pictures.max(1) as f64);
}

/// HEVC dumps: parameter sets travel with the next picture; one slice per picture.
fn replay_hevc(nals: &[&[u8]]) {
    let mut dec = hp::decode::HevcDecoder::new(&[]);
    let mut bgra = vec![];
    let (mut pictures, mut errors, mut au) = (0, 0, vec![]);
    let t0 = Instant::now();
    for nal in nals {
        au.push(nal.to_vec());
        if hp::decode::hevc_type(nal) > 31 {
            continue;
        }
        match dec.feed_au(&std::mem::take(&mut au), &mut bgra) {
            Ok(Some(_)) => pictures += 1,
            Ok(None) => {}
            Err(e) => {
                errors += 1;
                println!("{e}");
            }
        }
    }
    if let Ok(Some(_)) = dec.finish(&mut bgra) {
        pictures += 1;
    }
    let el = t0.elapsed().as_secs_f64();
    println!("{pictures} pictures returned, {errors} errors, {:.2} s ({:.1} pictures/s; only the newest finished picture is returned)", el, pictures as f64 / el);
}

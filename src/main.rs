// Release builds on Windows are GUI-subsystem apps so no console window opens with them.
// Debug builds keep the console for log output.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

use gpui::{prelude::*, px, size, Application, Bounds, SharedString, TitlebarOptions, WindowBackgroundAppearance, WindowBounds, WindowDecorations, WindowOptions};
use see_my_mac::{app::App, model::Store, settings::Settings};

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("see_my_mac=info")).format_timestamp_millis().init();
    Application::new().with_assets(see_my_mac::view::Assets).run(|cx: &mut gpui::App| {
        let store = Store::load(Store::default_path());
        let settings = Settings::load(Settings::default_path());
        // Prefer the design's Inter font when installed; otherwise the system default.
        let font = cx
            .text_system()
            .all_font_names()
            .into_iter()
            .find(|n| n == "Inter")
            .map(SharedString::from);
        let bounds = Bounds::centered(None, size(px(1280.0), px(800.0)), cx);
        let native_frame = see_my_mac::view::NATIVE_FRAME;
        let opened = cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                // macOS and Windows use the native frame; Linux draws a GNOME-style client-side header bar.
                titlebar: Some(TitlebarOptions { title: Some("SeeMyMac".into()), appears_transparent: !native_frame, ..Default::default() }),
                window_decorations: Some(if native_frame { WindowDecorations::Server } else { WindowDecorations::Client }),
                window_min_size: Some(size(px(640.0), px(400.0))),
                window_background: if native_frame { WindowBackgroundAppearance::Opaque } else { WindowBackgroundAppearance::Transparent },
                app_id: Some("io.gitlab.kestermckinney.SeeMyMac".into()),
                ..Default::default()
            },
            |window, cx| {
                set_window_icon(window);
                cx.new(|cx| {
                    let mut app = App::with_settings(store, settings, font, window, cx);
                    app.shortcuts = Some(see_my_mac::inhibit::Shortcuts::for_window(window));
                    app.auto_update_check(cx);
                    app
                })
            },
        );
        if let Err(e) = opened {
            eprintln!("SeeMyMac could not open its window: {e:#}");
            std::process::exit(1);
        }
        cx.activate(true);
    });
}

#[cfg(target_os = "windows")]
fn set_window_icon(window: &gpui::Window) {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use windows::Win32::{Foundation::{HWND, LPARAM, WPARAM}, UI::WindowsAndMessaging::{CreateIconFromResourceEx, SendMessageW, IMAGE_FLAGS, WM_SETICON}};
    let Ok(handle) = HasWindowHandle::window_handle(window) else { return };
    let RawWindowHandle::Win32(handle) = handle.as_raw() else { return };
    // PNG-compressed icon resources are supported by Windows. These handles
    // stay alive until process exit so the window can use them throughout its life.
    for (kind, size) in [(0, 16), (1, 32)] {
        unsafe {
            match CreateIconFromResourceEx(include_bytes!("../assets/icons/seemac.png"), true, 0x00030000, size, size, IMAGE_FLAGS(0)) {
                Ok(icon) => { SendMessageW(HWND(handle.hwnd.get() as *mut _), WM_SETICON, Some(WPARAM(kind)), Some(LPARAM(icon.0 as isize))); }
                Err(error) => log::warn!("could not load window icon: {error}"),
            }
        }
    }
}

#[cfg(not(target_os = "windows"))]
fn set_window_icon(_: &gpui::Window) {}

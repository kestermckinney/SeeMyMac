use gpui::{prelude::*, px, size, Application, Bounds, SharedString, TitlebarOptions, WindowBounds, WindowDecorations, WindowOptions};
use see_my_mac::{app::App, model::Store, settings::Settings};

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("see_my_mac=info")).format_timestamp_millis().init();
    Application::new().run(|cx: &mut gpui::App| {
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
        let opened = cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions { title: Some("SeeMyMac".into()), appears_transparent: true, ..Default::default() }),
                window_decorations: Some(WindowDecorations::Client),
                window_min_size: Some(size(px(640.0), px(400.0))),
                app_id: Some("seemymac".into()),
                ..Default::default()
            },
            |window, cx| cx.new(|cx| App::with_settings(store, settings, font, window, cx)),
        );
        if let Err(e) = opened {
            eprintln!("SeeMyMac could not open its window: {e:#}");
            std::process::exit(1);
        }
        cx.activate(true);
    });
}

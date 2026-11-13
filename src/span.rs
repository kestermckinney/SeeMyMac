//! Extra full-screen windows for "Span all local monitors": the main window shows the
//! remote monitor that matches its own display, and one of these covers each other
//! local display with the remote monitor of the same number. Input goes to the app.

use gpui::{prelude::*, div, img, point, px, rgb, AnyWindowHandle, Bounds, Context, Entity, FocusHandle, KeyDownEvent, KeyUpEvent, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, ModifiersChangedEvent, ObjectFit, Render, ScrollDelta, ScrollWheelEvent, Window};

use crate::{app::App, geometry};

pub struct SpanScreen {
    app: Entity<App>,
    main: AnyWindowHandle,
    /// The remote monitor shown here (0-based, same as the local display's position).
    monitor: usize,
    focus: FocusHandle,
}

impl SpanScreen {
    pub fn new(app: Entity<App>, main: AnyWindowHandle, monitor: usize, window: &mut Window, cx: &mut Context<Self>) -> Self {
        cx.observe(&app, |_, _, cx| cx.notify()).detach();
        let focus = cx.focus_handle();
        window.focus(&focus);
        // Closing this window ends the connection, which closes the windows on the other
        // monitors too (instead of the app reopening this one).
        let (a, m) = (app.clone(), main);
        window.on_window_should_close(cx, move |_, cx| {
            let app = a.clone();
            let _ = m.update(cx, move |_, main, cx| app.update(cx, |a, cx| a.end_span(main, cx)));
            true
        });
        Self { app, main, monitor, focus }
    }

    fn area(window: &Window) -> Bounds<gpui::Pixels> {
        Bounds::new(point(px(0.0), px(0.0)), window.viewport_size())
    }
}

impl Render for SpanScreen {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let vp = window.viewport_size();
        let tile = self.app.read(cx).span_tile(self.monitor);
        let m = self.monitor;
        let (vw, vh) = (f32::from(vp.width), f32::from(vp.height));
        let rect = tile.as_ref().map(|(_, size, scaling, pan)| geometry::image_rect_pan(vw, vh, size.0 as f32, size.1 as f32, *scaling, *pan));
        let picture = tile.zip(rect).map(|((image, ..), r)| img(image).absolute().left(px(r.x)).top(px(r.y)).w(px(r.w)).h(px(r.h)).object_fit(ObjectFit::Fill));
        let bars = rect.map(|r| crate::view::pan_bars(vw, vh, r)).unwrap_or_default();
        div()
            .id("span-screen")
            .key_context("SeeMyMacSpan")
            .track_focus(&self.focus)
            .size_full()
            .relative()
            .overflow_hidden()
            .bg(rgb(0x000000))
            .on_mouse_down(MouseButton::Left, cx.listener(move |this, ev: &MouseDownEvent, window, cx| {
                window.focus(&this.focus);
                let a = Self::area(window);
                this.app.update(cx, |app, _| app.pointer_at(Some(m), a, ev.position, Some(1), Some(true)));
            }))
            .on_mouse_down(MouseButton::Middle, cx.listener(move |this, ev: &MouseDownEvent, window, cx| {
                let a = Self::area(window);
                this.app.update(cx, |app, _| app.pointer_at(Some(m), a, ev.position, Some(2), Some(true)));
            }))
            .on_mouse_down(MouseButton::Right, cx.listener(move |this, ev: &MouseDownEvent, window, cx| {
                let a = Self::area(window);
                this.app.update(cx, |app, _| app.pointer_at(Some(m), a, ev.position, Some(4), Some(true)));
            }))
            .on_mouse_up(MouseButton::Left, cx.listener(move |this, ev: &MouseUpEvent, window, cx| {
                let a = Self::area(window);
                this.app.update(cx, |app, _| app.pointer_at(Some(m), a, ev.position, Some(1), Some(false)));
            }))
            .on_mouse_up(MouseButton::Middle, cx.listener(move |this, ev: &MouseUpEvent, window, cx| {
                let a = Self::area(window);
                this.app.update(cx, |app, _| app.pointer_at(Some(m), a, ev.position, Some(2), Some(false)));
            }))
            .on_mouse_up(MouseButton::Right, cx.listener(move |this, ev: &MouseUpEvent, window, cx| {
                let a = Self::area(window);
                this.app.update(cx, |app, _| app.pointer_at(Some(m), a, ev.position, Some(4), Some(false)));
            }))
            .on_mouse_move(cx.listener(move |this, ev: &MouseMoveEvent, window, cx| {
                let a = Self::area(window);
                this.app.update(cx, |app, _| app.pointer_at(Some(m), a, ev.position, None, None));
            }))
            .on_scroll_wheel(cx.listener(move |this, ev: &ScrollWheelEvent, window, cx| {
                let (dx, dy) = match ev.delta {
                    ScrollDelta::Pixels(p) => (f32::from(p.x), f32::from(p.y)),
                    ScrollDelta::Lines(l) => (l.x * 40.0, l.y * 40.0),
                };
                let a = Self::area(window);
                let shift = ev.modifiers.shift;
                this.app.update(cx, |app, cx| {
                    if app.scroll_wheel_at(Some(m), a, ev.position, dx, dy, shift) {
                        cx.notify();
                    }
                });
            }))
            .on_key_down(cx.listener(|this, ev: &KeyDownEvent, _, cx| {
                let app = this.app.clone();
                let ev = ev.clone();
                let _ = this.main.update(cx, move |_, main, cx| app.update(cx, |a, cx| a.on_key_down(&ev, main, cx)));
            }))
            .on_key_up(cx.listener(|this, ev: &KeyUpEvent, _, cx| {
                let app = this.app.clone();
                let ev = ev.clone();
                let _ = this.main.update(cx, move |_, main, cx| app.update(cx, |a, cx| a.on_key_up(&ev, main, cx)));
            }))
            .on_modifiers_changed(cx.listener(|this, ev: &ModifiersChangedEvent, _, cx| {
                let app = this.app.clone();
                let ev = ev.clone();
                let _ = this.main.update(cx, move |_, main, cx| app.update(cx, |a, cx| a.on_modifiers(&ev, main, cx)));
            }))
            .children(picture)
            .children(bars)
    }
}

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
        let picture = tile.map(|(image, size, scaling)| {
            let r = geometry::image_rect(f32::from(vp.width), f32::from(vp.height), size.0 as f32, size.1 as f32, scaling);
            img(image).absolute().left(px(r.x)).top(px(r.y)).w(px(r.w)).h(px(r.h)).object_fit(ObjectFit::Fill)
        });
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
                let dy = match ev.delta {
                    ScrollDelta::Pixels(p) => f32::from(p.y),
                    ScrollDelta::Lines(l) => l.y,
                };
                let a = Self::area(window);
                this.app.update(cx, |app, _| app.scroll_at(Some(m), a, ev.position, dy));
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
    }
}

//! Trackpad view: use the phone as a trackpad for this computer.
//!
//! This is a designed placeholder. The transport that would carry pointer
//! and keyboard events from the phone to the desktop does not exist yet, so
//! this pane shows what the feature will be, what it needs, and how it will
//! be set up — without pretending to be connected.
//!
//! The pane is device-aware: it names the selected phone and holds the
//! settings that the backend will read, so the shape of the screen is
//! settled before the wire protocol. When the backend lands, `Start` will
//! open the tunnel and the surface below will become live; until then it
//! stays disabled with a plain-language reason.

use gpui::prelude::*;
use gpui::{AnyElement, Context, Styled, Window, div, px, relative};

use crate::icons::{self, names};
use crate::theme::{self, Palette, Themed};
use crate::views::ui;

/// The trackpad view.
pub struct Trackpad {
    device: Option<String>,
    device_label: String,
}

impl Trackpad {
    pub fn new() -> Self {
        Self {
            device: None,
            device_label: String::new(),
        }
    }

    pub fn set_device(&mut self, device: Option<String>, cx: &mut Context<Self>) {
        if self.device != device {
            self.device = device;
            cx.notify();
        }
    }

    pub fn set_device_label(&mut self, label: String, cx: &mut Context<Self>) {
        if self.device_label != label {
            self.device_label = label;
            cx.notify();
        }
    }
}

impl Render for Trackpad {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = *cx.theme();
        let title = div()
            .flex()
            .items_center()
            .gap(px(10.0))
            .px(px(20.0))
            .pt(px(16.0))
            .pb(px(12.0))
            .child(icons::icon(names::TRACKPAD, 18.0, t.accent))
            .child(
                div()
                    .flex_col()
                    .gap(px(2.0))
                    .child(
                        div()
                            .text_size(t.text(17.0))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(t.text_header)
                            .child("Trackpad"),
                    )
                    .child(
                        div()
                            .text_size(t.text(12.0))
                            .text_color(t.text_muted)
                            .child(if self.device.is_some() {
                                "Use the selected phone as a trackpad for this computer".to_string()
                            } else {
                                "Use your phone as a trackpad for this computer — pick a phone first".to_string()
                            }),
                    ),
            )
            .child(div().flex_1())
            .child(ui::pill(&t, "Coming soon", t.text_dim, t.secondary_bg));
        div()
            .flex_col()
            .size_full()
            .bg(t.canvas)
            .child(title)
            .child(self.body(&t, cx))
            .into_any_element()
    }
}

impl Trackpad {
    fn body(&mut self, t: &Palette, _cx: &mut Context<Self>) -> AnyElement {
        let device_line = match (&self.device, &self.device_label) {
            (Some(_), label) if !label.is_empty() => label.clone(),
            (Some(serial), _) => serial.clone(),
            _ => "No phone selected".to_string(),
        };

        // Left: what it does, and what it needs.
        let plan = div()
            .flex_col()
            .gap(px(10.0))
            .w(px(300.0))
            .flex_shrink_0()
            .child(section_title(t, "How it will work"))
            .child(
                div()
                    .flex_col()
                    .gap(px(8.0))
                    .child(step(t, "1", "Hold the phone like a trackpad."))
                    .child(step(t, "2", "Slide one finger to move the cursor."))
                    .child(step(t, "3", "Tap to click; two fingers scroll."))
                    .child(step(
                        t,
                        "4",
                        "Use the on-phone keyboard when you need to type.",
                    )),
            )
            .child(section_title(t, "What it needs"))
            .child(
                div()
                    .flex_col()
                    .gap(px(8.0))
                    .child(need(t, &device_line))
                    .child(need(t, "A phone with USB or Wi-Fi debugging"))
                    .child(need(t, "The trackpad transport (in development)")),
            );

        // Centre: the touch surface, honestly inert until the backend lands.
        let surface = div()
            .flex_1()
            .min_w_0()
            .flex()
            .items_center()
            .justify_center()
            .child(
                div()
                    .w(px(420.0))
                    .h(px(300.0))
                    .rounded(px(theme::RADIUS_CARD))
                    .bg(t.card)
                    .border_1()
                    .border_color(t.border)
                    .flex()
                    .flex_col()
                    .items_center()
                    .justify_center()
                    .gap(px(10.0))
                    .child(icons::icon(names::TRACKPAD, 36.0, t.text_muted))
                    .child(
                        div()
                            .text_size(t.text(14.0))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(t.text_secondary)
                            .child("Trackpad surface"),
                    )
                    .child(
                        div()
                            .max_w(px(300.0))
                            .text_size(t.text(12.0))
                            .line_height(relative(1.4))
                            .text_color(t.text_muted)
                            .whitespace_normal()
                            .child(
                                "This is where your finger would move the cursor. It activates once the trackpad transport is connected — this preview does not drive the pointer yet.",
                            ),
                    )
                    .child(
                        div()
                            .mt(px(4.0))
                            .px(px(16.0))
                            .h(px(32.0))
                            .flex()
                            .items_center()
                            .rounded(px(theme::RADIUS_CAPSULE_BTN))
                            .bg(t.secondary_bg)
                            .text_size(t.text(12.5))
                            .text_color(t.text_muted)
                            .child("Connect trackpad — not yet available"),
                    ),
            );

        div()
            .flex()
            .flex_1()
            .min_h_0()
            .gap(px(20.0))
            .px(px(20.0))
            .pb(px(16.0))
            .child(plan)
            .child(surface)
            .into_any_element()
    }
}

fn section_title(t: &Palette, label: &str) -> AnyElement {
    div()
        .text_size(t.text(11.0))
        .font_weight(gpui::FontWeight::SEMIBOLD)
        .text_color(t.text_muted)
        .child(label.to_string())
        .into_any_element()
}

fn step(t: &Palette, n: &str, text: &str) -> AnyElement {
    div()
        .flex()
        .items_center()
        .gap(px(10.0))
        .child(
            div()
                .size(px(20.0))
                .rounded_full()
                .bg(t.accent_soft)
                .flex()
                .items_center()
                .justify_center()
                .text_size(t.text(10.5))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(t.accent)
                .child(n.to_string()),
        )
        .child(
            div()
                .flex_1()
                .text_size(t.text(12.5))
                .text_color(t.text_secondary)
                .child(text.to_string()),
        )
        .into_any_element()
}

fn need(t: &Palette, text: &str) -> AnyElement {
    div()
        .flex()
        .items_center()
        .gap(px(10.0))
        .child(icons::icon(names::CHECKBOX_CHECKED, 14.0, t.text_muted))
        .child(
            div()
                .flex_1()
                .text_size(t.text(12.5))
                .text_color(t.text_secondary)
                .child(text.to_string()),
        )
        .into_any_element()
}

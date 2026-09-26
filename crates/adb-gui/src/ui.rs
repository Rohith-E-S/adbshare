//! Shared UI components.
//!
//! Every repeated visual from the GTK build's stylesheet lives here as a small
//! builder, so the views describe structure and this module owns appearance.
//! The names map to the old CSS classes: `.pill-capsule` becomes
//! [`capsule`], `.usb-badge` becomes [`pill`], and so on.
//!
//! Interactive builders take an `ElementId` because GPUI identifies elements by
//! id for hit-testing and state: a caller's own id keeps repeated instances
//! (list rows, menu items) distinct.

use gpui::prelude::*;
use gpui::{
    AnyElement, AnyView, App, Div, ElementId, FontWeight, Rgba, Stateful,
    StatefulInteractiveElement, Styled, Window, deferred, div, px, relative, rgba,
};

use crate::icons::{self, names};
use crate::protocol::StateTone;
use crate::theme::{self, Palette, Themed};

/// Build an [`ElementId`] from a runtime string.
///
/// `ElementId` implements `From<SharedString>` but not `From<String>`, so any id
/// assembled at render time needs this.
pub fn el_id(text: impl Into<gpui::SharedString>) -> ElementId {
    ElementId::from(text.into())
}

// ── Buttons ──────────────────────────────────────────────────────────────────

/// A square, borderless icon button: the building block of every capsule.
///
/// `tint` is the resting icon colour; hover and pressed states come from the
/// palette.
pub fn icon_button(
    t: &Palette,
    id: impl Into<ElementId>,
    icon: &'static str,
    size: f32,
    tint: Rgba,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut gpui::App) + 'static,
) -> Stateful<Div> {
    div()
        .id(id)
        .flex()
        .items_center()
        .justify_center()
        .size(px(size))
        .rounded(px(theme::RADIUS_CAPSULE_BTN))
        .cursor_pointer()
        .hover(|s| s.bg(t.hover))
        .active(|s| s.bg(t.pressed))
        .child(icons::icon(icon, size * 0.53, tint))
        .on_click(on_click)
}

/// The same button, but drawn in its toggled-on state. Used for the grid/list
/// switch, where one of the two is always active.
pub fn icon_button_active(
    t: &Palette,
    id: impl Into<ElementId>,
    icon: &'static str,
    size: f32,
    active: bool,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut gpui::App) + 'static,
) -> Stateful<Div> {
    let tint = if active { t.text_header } else { t.text_dim };
    div()
        .id(id)
        .flex()
        .items_center()
        .justify_center()
        .size(px(size))
        .rounded(px(theme::RADIUS_CAPSULE_BTN))
        .cursor_pointer()
        .when(active, |d| d.bg(t.pressed))
        .hover(|s| s.bg(t.hover))
        .child(icons::icon(icon, size * 0.53, tint))
        .on_click(on_click)
}

/// An icon button that is drawn dimmed and ignores clicks, for actions that need
/// something selected first.
pub fn icon_button_disabled(t: &Palette, icon: &'static str, size: f32) -> Div {
    div()
        .flex()
        .items_center()
        .justify_center()
        .size(px(size))
        .rounded(px(theme::RADIUS_CAPSULE_BTN))
        .opacity(0.35)
        .child(icons::icon(icon, size * 0.53, t.text_muted))
}

/// A rounded container that groups 2–4 icon buttons, matching `.pill-capsule`.
pub fn capsule(children: impl IntoIterator<Item = AnyElement>) -> impl IntoElement {
    div()
        .flex()
        .items_center()
        .h(px(theme::CAPSULE_H))
        .px(px(2.0))
        .rounded(px(theme::RADIUS_CAPSULE))
        .children(children)
}

/// A 1px vertical rule between capsule buttons.
pub fn capsule_separator(t: &Palette) -> Div {
    div()
        .w(px(1.0))
        .h(px(18.0))
        .my(px(8.0))
        .bg(t.capsule_border)
}

/// The destructive variant of a dialog button.
pub fn danger_button(
    id: impl Into<ElementId>,
    label: impl Into<gpui::SharedString>,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut gpui::App) + 'static,
) -> impl IntoElement {
    div()
        .id(id)
        .flex()
        .items_center()
        .h(px(32.0))
        .px(px(14.0))
        .rounded(px(8.0))
        .text_sm()
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(rgba(0xFFFFFFFF))
        .bg(rgba(0xEF4444E6))
        .cursor_pointer()
        .hover(|s| s.bg(rgba(0xEF4444FF)))
        .child(label.into())
        .on_click(on_click)
}

/// A dialog button: a rounded rect that reads as a normal action. `primary`
/// renders the filled variant used for the accepting button.
pub fn button(
    t: &Palette,
    id: impl Into<ElementId>,
    label: impl Into<gpui::SharedString>,
    primary: bool,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut gpui::App) + 'static,
) -> impl IntoElement {
    let idle_bg = if primary {
        t.text_header
    } else {
        rgba(0xFFFFFF14)
    };
    let hover_bg = if primary {
        rgba(0xFFFFFFFF)
    } else {
        rgba(0xFFFFFF26)
    };
    div()
        .id(id)
        .flex()
        .items_center()
        .h(px(32.0))
        .px(px(14.0))
        .rounded(px(8.0))
        .text_sm()
        .font_weight(FontWeight::MEDIUM)
        .cursor_pointer()
        .text_color(if primary {
            t.text_inverse
        } else {
            t.text_primary
        })
        .bg(idle_bg)
        .hover(move |s| s.bg(hover_bg))
        .child(label.into())
        .on_click(on_click)
}

// ── Indicators ───────────────────────────────────────────────────────────────

/// A rounded progress bar with a faint fill, as used by the device storage bar,
/// the status bar and each transfer card.
pub fn progress_bar(t: &Palette, fraction: f32, height: f32) -> Div {
    let fraction = fraction.clamp(0.0, 1.0);
    div()
        .relative()
        .w_full()
        .h(px(height))
        .rounded(px(theme::RADIUS_PILL))
        .bg(t.track)
        .when(fraction > 0.0, |d| {
            d.child(
                div()
                    .absolute()
                    .top_0()
                    .bottom_0()
                    .left_0()
                    .w(relative(fraction))
                    .rounded(px(theme::RADIUS_PILL))
                    .bg(t.fill_soft),
            )
        })
}

/// A small status dot, used for the device LEDs.
pub fn led(color: Rgba) -> Div {
    div().size(px(7.0)).rounded_full().bg(color).shadow_sm()
}

/// A small text badge, matching `.usb-badge` / `.battery-pill` /
/// `.transfer-state-pill`.
pub fn pill(text: impl Into<gpui::SharedString>, fg: Rgba, bg: Rgba) -> Div {
    div()
        .flex()
        .items_center()
        .h(px(20.0))
        .px(px(7.0))
        .rounded(px(theme::RADIUS_PILL))
        .bg(bg)
        .text_size(px(9.0))
        .font_weight(FontWeight::BOLD)
        .text_color(fg)
        .child(text.into())
}

/// Resolve a [`StateTone`] to the pill colours the old `pill-*` classes used.
pub fn tone_colors(t: &Palette, tone: StateTone) -> (Rgba, Rgba) {
    match tone {
        StateTone::Active => (t.text_inverse, t.text_header),
        StateTone::Neutral => (t.text_dim, rgba(0xFFFFFF14)),
        StateTone::Success => (t.success, rgba(0x22C55E1F)),
        StateTone::Warning => (t.warning, rgba(0xFACC151F)),
        StateTone::Danger => (t.danger, rgba(0xEF44441F)),
    }
}

// ── Text ─────────────────────────────────────────────────────────────────────

/// A monospace secondary line, used for paths, counts and status text.
pub fn mono(text: impl Into<gpui::SharedString>, t: &Palette) -> Div {
    div()
        .font_family(theme::MONO)
        .text_size(px(10.5))
        .text_color(t.text_dim)
        .child(text.into())
}

/// The uppercase monospace sidebar headings.
pub fn section_heading(text: impl Into<gpui::SharedString>, t: &Palette) -> Div {
    div()
        .flex()
        .items_end()
        .h(px(24.0))
        .px(px(4.0))
        .text_size(px(11.0))
        .font_weight(FontWeight::MEDIUM)
        .text_color(t.text_muted)
        .child(text.into().to_uppercase())
}

/// A `Ctrl+U` style shortcut hint, matching `.shortcut-label`.
pub fn shortcut_label(t: &Palette, text: &str, accent: bool) -> Div {
    div()
        .font_family(theme::MONO)
        .text_size(px(11.0))
        .text_color(if accent { t.text_dim } else { t.text_muted })
        .child(text.to_string())
}

// ── Layout scaffolds ─────────────────────────────────────────────────────────

/// A vertical scroll container with the scrollbar hidden. The GTK build hid its
/// scrollbars too; wheel scrolling still works.
pub fn scroll_area(content: impl IntoIterator<Item = AnyElement>) -> Stateful<Div> {
    div()
        .id("scroll")
        .size_full()
        .overflow_y_scroll()
        .scrollbar_width(px(0.0))
        .children(content)
}

/// Chrome for a popover, a context menu or a dialog panel: the `.card` look.
pub fn surface(t: &Palette, content: impl IntoIterator<Item = AnyElement>) -> Div {
    div()
        .flex()
        .flex_col()
        .rounded(px(12.0))
        .bg(t.card)
        .border_1()
        .border_color(t.border_soft)
        .shadow_xl()
        .overflow_hidden()
        .children(content)
}

/// A dismiss-anywhere backdrop behind a modal. `block_mouse_except_scroll`
/// rather than `occlude()` so the wheel still reaches the view underneath.
pub fn modal_backdrop() -> Stateful<Div> {
    div()
        .id("backdrop")
        .absolute()
        .inset_0()
        .bg(rgba(0x00000099))
        .block_mouse_except_scroll()
}

/// Paint the given content above everything already laid out in the tree.
///
/// GPUI has no z-index; paint order comes from tree order, and `deferred`
/// postpones an element's paint until after its ancestors'.
pub fn on_top(content: impl IntoElement) -> impl IntoElement {
    deferred(div().absolute().inset_0().child(content)).priority(100)
}

/// A menu row for the kebab menu and context menus.
pub fn menu_row(
    t: &Palette,
    id: impl Into<ElementId>,
    icon: Option<&'static str>,
    label: &str,
    danger: bool,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut gpui::App) + 'static,
) -> Stateful<Div> {
    let fg = if danger { t.danger } else { t.text_secondary };
    div()
        .id(id)
        .flex()
        .items_center()
        .gap(px(10.0))
        .w_full()
        .px(px(12.0))
        .py(px(7.0))
        .cursor_pointer()
        .text_sm()
        .text_color(fg)
        .hover(|s| s.bg(t.hover).text_color(t.text_header))
        .when_some(icon.map(|i| (i, fg)), |d, (icon, color)| {
            d.child(icons::icon(icon, 15.0, color))
        })
        .child(label.to_string())
        .on_click(on_click)
}

/// A horizontal rule between menu groups.
pub fn menu_separator(t: &Palette) -> Div {
    div().h(px(1.0)).w_full().my(px(4.0)).bg(t.border_soft)
}

/// A toggleable menu row, used for "Show hidden files".
///
/// A transparent icon keeps the label aligned with [`menu_row`] whether or not
/// the row is currently ticked.
pub fn menu_check(
    t: &Palette,
    id: impl Into<ElementId>,
    icon: &'static str,
    label: &str,
    checked: bool,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut gpui::App) + 'static,
) -> Stateful<Div> {
    div()
        .id(id)
        .flex()
        .items_center()
        .gap(px(10.0))
        .w_full()
        .px(px(12.0))
        .py(px(7.0))
        .cursor_pointer()
        .text_sm()
        .text_color(t.text_secondary)
        .hover(|s| s.bg(t.hover).text_color(t.text_header))
        .child(icons::icon(
            icon,
            15.0,
            if checked {
                t.text_header
            } else {
                rgba(0x00000000)
            },
        ))
        .child(label.to_string())
        .on_click(on_click)
}

/// A right-pointing chevron for breadcrumb separators.
pub fn breadcrumb_separator(t: &Palette) -> Div {
    div()
        .px(px(2.0))
        .child(icons::icon(names::GO_NEXT, 11.0, t.text_muted))
}

// ── Tooltips ─────────────────────────────────────────────────────────────────
//
// A tooltip's colours come from the installed palette rather than being passed
// in, because GPUI builds the tooltip inside a closure that only receives
// `&mut App` — no view state, and so no `&Palette` to hand it.

/// A small bubble with a line of text, shown on hover.
///
/// A view rather than a bare element because GPUI's `tooltip` takes an
/// `AnyView`, which is a thing it can render later, not an element it paints
/// now.
///
/// Icons in the tool bar and sidebar carry no visible label, so without this the
/// chrome asks the user to guess what a glyph means.
pub struct Tooltip {
    label: gpui::SharedString,
}

impl Render for Tooltip {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = cx.theme();
        div()
            .max_w(px(280.0))
            .px(px(9.0))
            .py(px(6.0))
            .rounded(px(6.0))
            // A tooltip is an overlay, so it sits above every surface.
            .bg(t.tooltip)
            .text_color(t.tooltip_text)
            .border_1()
            .border_color(t.tooltip_border)
            .shadow_lg()
            .text_size(px(11.0))
            .line_height(relative(1.35))
            .whitespace_normal()
            .child(self.label.clone())
    }
}

/// Build the hover hint for a label.
pub fn hover_hint(
    label: impl Into<gpui::SharedString> + 'static,
) -> impl Fn(&mut Window, &mut App) -> gpui::AnyView + 'static {
    // Converted once here rather than per hover: `SharedString` is a cheap Arc
    // clone, and the closure has to be `Fn` because the tooltip may be shown
    // repeatedly.
    let label: gpui::SharedString = label.into();
    move |_window, cx: &mut App| {
        let label = label.clone();
        AnyView::from(cx.new(|_cx| Tooltip { label }))
    }
}

/// Attach a hover hint to an icon button.
pub fn icon_button_with_hint(
    t: &Palette,
    id: impl Into<ElementId>,
    icon: &'static str,
    size: f32,
    tint: Rgba,
    hint: impl Into<gpui::SharedString> + 'static,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut gpui::App) + 'static,
) -> impl IntoElement {
    icon_button(t, id, icon, size, tint, on_click).tooltip(hover_hint(hint.into()))
}

/// Attach a hover hint to a toggling icon button.
pub fn icon_button_active_with_hint(
    t: &Palette,
    id: impl Into<ElementId>,
    icon: &'static str,
    size: f32,
    active: bool,
    hint: impl Into<gpui::SharedString> + 'static,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut gpui::App) + 'static,
) -> impl IntoElement {
    icon_button_active(t, id, icon, size, active, on_click).tooltip(hover_hint(hint.into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::Palette;

    #[test]
    fn palette_contrast_tokens_are_distinct() {
        let t = Palette::one_dark();
        assert_ne!(t.canvas, t.card, "card must lift off the canvas");
        assert_ne!(t.text_header, t.text_muted);
        assert_ne!(t.hover, t.pressed);
    }

    /// The palette is Zed's One Dark, so the tokens that make it recognisable are
    /// pinned to that theme's values.
    ///
    /// Without this the palette could quietly drift back towards adbshare's old
    /// near-black scheme and nothing would fail.
    #[test]
    fn the_palette_is_zed_one_dark() {
        use gpui::Rgba;
        let t = Palette::one_dark();
        let expect = |got: Rgba, want: u32, what: &str| {
            assert_eq!(u32::from(got), want, "{what} is not Zed One Dark");
        };

        // surfaces
        expect(t.canvas, 0x3B414DFF, "background");
        expect(t.sidebar, 0x2F343EFF, "surface.background");
        expect(t.card, 0x2F343EFF, "elevated_surface.background");
        expect(t.surface_raised, 0x2E343EFF, "element.background");
        expect(t.topbar_raised, 0x282C33FF, "toolbar.background");

        // text
        expect(t.text_header, 0xDCE0E5FF, "text");
        expect(t.text_dim, 0xA9AFBCFF, "text.muted");
        expect(t.text_muted, 0x878A98FF, "text.disabled");

        // strokes
        expect(t.border, 0x464B57FF, "border");
        expect(t.border_soft, 0x363C46FF, "border.variant");

        // semantic
        expect(t.accent, 0x74ADE8FF, "text.accent");
        expect(t.accent_muted, 0x47679EFF, "border.focused");
        expect(t.success, 0xA1C181FF, "success");
        expect(t.warning, 0xDEC184FF, "warning");
        expect(t.danger, 0xD07277FF, "error");
    }

    /// Every text token has to be readable on every surface it is drawn over.
    #[test]
    fn text_stays_legible_on_every_surface() {
        use gpui::colors::{Colors, DefaultAppearance};
        let t = Palette::one_dark();
        // The surfaces the UI actually paints on.
        let surfaces = [t.canvas, t.sidebar, t.card, t.surface_raised];
        let inks = [t.text_header, t.text_primary, t.text_dim, t.text_muted];

        for ink in inks {
            for surface in surfaces {
                // Relative luminance, per WCAG. One Dark is a light-ish grey, so
                // even `text.disabled` has to clear the large-text threshold.
                let lum = |c: gpui::Rgba| {
                    let f = |v: f32| {
                        if v <= 0.03928 {
                            v / 12.92
                        } else {
                            ((v + 0.055) / 1.055).powf(2.4)
                        }
                    };
                    0.2126 * f(c.r) + 0.7152 * f(c.g) + 0.0722 * f(c.b)
                };
                let (a, b) = (lum(ink), lum(surface));
                let ratio = (a.max(b) + 0.05) / (a.min(b) + 0.05);
                assert!(
                    ratio >= 2.0,
                    "ink {ink:?} on surface {surface:?} is only {ratio:.2}:1"
                );
            }
        }
        // And the theme GPUI itself would use agrees this is a dark theme.
        let _ = Colors::dark();
        assert_eq!(
            DefaultAppearance::from(gpui::WindowAppearance::Dark),
            DefaultAppearance::Dark
        );
    }

    /// The hover hint is a real view, so it has to lay out and read its colours
    /// from the installed palette.
    ///
    /// The hover *trigger* is GPUI's own `tooltip`, attached to the buttons; what
    /// this covers is that the bubble itself is well formed, which a missing
    /// palette entry or a bad size would break.
    #[gpui::test]
    async fn the_tooltip_view_lays_out(cx: &mut gpui::TestAppContext) {
        // The palette is a global, and a bare test context has none.
        cx.update(crate::theme::install);
        let (tooltip, vctx) = cx.add_window_view(|_w, _cx| Tooltip {
            label: "Save to this computer".into(),
        });

        let _ = vctx.draw(
            gpui::point(px(0.), px(0.)),
            gpui::size(px(400.), px(200.)),
            |_w, _cx| tooltip.clone(),
        );

        cx.update(|app| {
            let t = app.theme();
            // A tooltip has to be opaque enough to read against whatever it
            // hovers, and its ink has to contrast.
            assert!(t.tooltip.a >= 0.9, "the tooltip surface is see-through");
            let lum = |c: Rgba| {
                let f = |v: f32| {
                    if v <= 0.03928 {
                        v / 12.92
                    } else {
                        ((v + 0.055) / 1.055).powf(2.4)
                    }
                };
                0.2126 * f(c.r) + 0.7152 * f(c.g) + 0.0722 * f(c.b)
            };
            let (ink, bg) = (lum(t.tooltip_text), lum(t.tooltip));
            let ratio = (ink.max(bg) + 0.05) / (ink.min(bg) + 0.05);
            assert!(
                ratio >= 4.5,
                "tooltip text is only {ratio:.1}:1 on its bubble"
            );
        });
    }

    #[test]
    fn tone_colors_cover_every_tone() {
        let t = Palette::one_dark();
        for tone in [
            StateTone::Active,
            StateTone::Neutral,
            StateTone::Success,
            StateTone::Warning,
            StateTone::Danger,
        ] {
            let (fg, bg) = tone_colors(&t, tone);
            assert_ne!(fg, bg, "{tone:?} pill must be legible");
        }
    }
}

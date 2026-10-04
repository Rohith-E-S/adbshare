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

/// The colour a chrome icon wears for its meaning.
///
/// Every icon in the UI is asked for by name, so the colour travels with it:
/// `ui::icon_tint(names::TRASH, t)` is red without the call site saying so. Pass
/// the result to [`icons::icon`] or [`icons::icon_or_art`], or override it where
/// a state needs the icon to follow its label instead — a selected row, say.
pub fn icon_tint(key: &'static str, t: &Palette) -> Rgba {
    t.hue(icons::hue(key))
}

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
/// `tint` is the icon's resting colour; pass [`icon_tint`] for the colour the
/// icon's meaning calls for. Hover and pressed states come from the palette.
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
    // A toggle shows which half is on by giving that half its colour and
    // leaving the other in body text, so a pair reads as one control.
    let tint = if active {
        t.hue(icons::hue(icon))
    } else {
        t.text_muted
    };
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
///
/// The icon keeps its own hue and the whole button is faded, so a disabled
/// delete still reads as delete rather than as an anonymous grey square.
pub fn icon_button_disabled(t: &Palette, icon: &'static str, size: f32) -> Div {
    div()
        .flex()
        .items_center()
        .justify_center()
        .size(px(size))
        .rounded(px(theme::RADIUS_CAPSULE_BTN))
        .opacity(0.35)
        .child(icons::icon(icon, size * 0.53, icon_tint(icon, t)))
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
    t: &Palette,
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
        .text_color(t.text_inverse)
        .bg(t.danger_border)
        .cursor_pointer()
        .hover(|s| s.bg(t.danger))
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
        t.secondary_bg
    };
    let hover_bg = if primary {
        t.text_secondary
    } else {
        t.secondary_bg_hover
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
            t.text_on_light
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

/// Resolve a [`StateTone`] to the pill colours.
///
/// `Active` is the one filled pill, so it is the one that needs dark ink;
/// every other tone is a 16% wash of its own hue, which is the step T3 Code
/// uses for every tone surface.
pub fn tone_colors(t: &Palette, tone: StateTone) -> (Rgba, Rgba) {
    match tone {
        StateTone::Active => (t.text_on_light, t.text_header),
        StateTone::Neutral => (t.text_dim, t.secondary_bg),
        StateTone::Success => (t.success, t.success_soft),
        StateTone::Warning => (t.warning, t.warning_soft),
        StateTone::Danger => (t.danger, t.danger_soft),
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
///
/// The icon keeps its own colour so a row of them can be scanned by hue, and
/// only the label follows the row's hover and selection state.
pub fn menu_row(
    t: &Palette,
    id: impl Into<ElementId>,
    icon: Option<&'static str>,
    label: &str,
    danger: bool,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut gpui::App) + 'static,
) -> Stateful<Div> {
    let fg = if danger {
        t.text_primary
    } else {
        t.text_secondary
    };
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
        .text_color(if danger { t.danger } else { fg })
        .hover(|s| s.bg(t.hover).text_color(t.text_header))
        .when_some(icon, |d, icon| {
            d.child(icons::icon_or_art(icon, 15.0, icon_tint(icon, t)))
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
        .child(icons::icon_or_art(
            icon,
            15.0,
            if checked {
                icon_tint(icon, t)
            } else {
                rgba(0x00000000)
            },
        ))
        .child(label.to_string())
        .on_click(on_click)
}

/// A right-pointing chevron for breadcrumb separators.
pub fn breadcrumb_separator(t: &Palette) -> Div {
    div().px(px(2.0)).child(icons::icon(
        names::GO_NEXT,
        11.0,
        icon_tint(names::GO_NEXT, t),
    ))
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

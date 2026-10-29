//! Design tokens for the adbshare frontend.
//!
//! GPUI has no CSS cascade and no theme type of its own, so the whole visual
//! system lives here as tokens, installed as an [`gpui::Global`] that views read
//! without threading it through every call.
//!
//! # A pure-black OLED theme with an elevated chrome
//!
//! The file canvas stays true black (`#000000`) so an OLED panel switches those
//! pixels off. Everything framing it — the sidebar, top bar and status bar —
//! sits one step up at `#09090B`, and controls on that chrome rise again to
//! `#121216`. Structure therefore reads from fill as well as from hairlines,
//! and the hairlines themselves are `#222228` rather than the near-invisible
//! borders the previous palette used.
//!
//! Text avoids pure white. At OLED contrast a `#FFFFFF` glyph blooms, so the
//! strongest ink is `#F4F4F6`, body copy is `#EDEDEF`, metadata `#A1A1AA`, and
//! section headers `#71717A`. That ladder keeps sizes, dates and storage stats
//! scannable without letting them compete with file names.
//!
//! Surfaces are *opaque* steps, because near-black greys a browser cannot
//! round-trip would band. Interaction fills are percentages of white instead:
//! one value stays legible over the black canvas, the `#09090B` chrome and a
//! `#121216` card alike.
//!
//! One token is deliberately absent: `scrollbar.thumb.background`. GPUI 0.2.2
//! has no scrollbar colour API at all, so a palette entry for it would be a
//! value nothing could read.
//!
//! Dark only, as the previous GTK build was. A light palette is one more
//! constructor here plus a branch in [`install`].

use gpui::{App, Global, Rgba, px, rgba};

/// The full set of colour tokens used across the UI.
///
/// Every field carries the T3 Code token it came from, so the mapping is
/// auditable rather than a matter of taste.
#[derive(Clone, Copy)]
pub struct Palette {
    /// The file area and the window behind everything. True black, so an OLED
    /// panel can switch it off.
    pub canvas: Rgba,
    /// The top navigation bar, one step above the canvas.
    pub topbar: Rgba,
    /// The raised step every control and strip on the toolbar sits on.
    pub topbar_raised: Rgba,
    /// The chrome behind the bottom status strip.
    pub statusbar: Rgba,
    /// The sidebar pane, elevated from the canvas so the layout reads as panes
    /// rather than one flat field.
    pub sidebar: Rgba,
    /// Dialogs, popovers, cards and the panels inside them.
    pub card: Rgba,
    /// A card or control under the pointer.
    pub card_hover: Rgba,
    /// The selected row in the sidebar.
    pub sidebar_active: Rgba,
    /// A file row or grid tile under the pointer.
    pub row_hover: Rgba,
    /// A structural divider: column header underline, status bar edge.
    pub divider: Rgba,
    /// A between-row hairline in the list view, dimmer than [`Self::divider`].
    pub row_divider: Rgba,
    /// The tint behind the connected device card while it is the active one.
    pub device_active_bg: Rgba,
    /// Its accent border, at 60% so the edge stays crisp.
    pub device_active_border: Rgba,

    // ── Interaction ────────────────────────────────────────────────────────
    // Percentages of white rather than fixed greys, so one value works over
    // the canvas, the `#09090B` chrome and a `#121216` card alike.
    /// A resting element under the pointer.
    pub hover: Rgba,
    /// A selected element.
    pub selected: Rgba,
    /// A selected element under the pointer, one step past `hover`.
    pub selected_strong: Rgba,
    /// A pressed element.
    pub pressed: Rgba,
    /// A group of icon buttons: the raised `#121216` control fill.
    pub capsule_bg: Rgba,
    /// The outline around a capsule.
    pub capsule_border: Rgba,
    /// A resting raised control: a secondary button, a neutral badge. The same
    /// `--input` step a capsule is outlined with.
    pub secondary_bg: Rgba,
    /// One step past `secondary_bg`, for that control under the pointer.
    pub secondary_bg_hover: Rgba,
    /// The groove behind a progress bar, `--input`.
    pub track: Rgba,
    /// The fill of a progress bar.
    pub fill_soft: Rgba,

    // ── Text ───────────────────────────────────────────────────────────────
    /// Headlines, the current breadcrumb and an active label. Off-white rather
    /// than pure white so the glyph does not bloom on an OLED panel.
    pub text_header: Rgba,
    /// Body copy and file names.
    pub text_primary: Rgba,
    /// A label at body weight rather than heading weight.
    pub text_secondary: Rgba,
    /// Metadata and secondary labels: file sizes, dates, inactive breadcrumbs,
    /// storage stats.
    pub text_dim: Rgba,
    /// Section headings, column headers and placeholders.
    pub text_muted: Rgba,
    /// Text drawn on an accent fill.
    pub text_inverse: Rgba,
    /// Ink for a light *fill*: a filled badge or a primary button.
    pub text_on_light: Rgba,

    // ── Strokes ─────────────────────────────────────────────────────────────
    /// Structural borders and card outlines.
    pub border: Rgba,
    /// Hairlines and dividers.
    pub border_soft: Rgba,

    // ── Semantic ───────────────────────────────────────────────────────────
    /// `--primary`. Accent text and icons.
    pub accent: Rgba,
    /// `--success-foreground`.
    pub success: Rgba,
    /// `--warning-foreground`.
    pub warning: Rgba,
    /// `--error`.
    pub danger: Rgba,
    /// `--success` at 16%, the badge fill the theme builds for success.
    pub success_soft: Rgba,
    /// `--warning-surface`: the warning at 16%.
    pub warning_soft: Rgba,
    /// `--error-surface`: the error at 16%, for a warning panel.
    pub danger_soft: Rgba,
    /// The same error, denser, for that panel's border.
    pub danger_border: Rgba,

    /// A tooltip is an overlay, so it sits above every surface: above the
    /// raised step rather than on it, with `--foreground` on top.
    pub tooltip: Rgba,
    pub tooltip_text: Rgba,
    /// The border around a tooltip, `--input`.
    pub tooltip_border: Rgba,

    /// How much text is scaled by, from [`set_font_scale`].
    ///
    /// A field rather than a constant because every size in the app is laid
    /// out in absolute pixels — GPUI has no cascade to hang a text size on —
    /// so the only place a scale can be applied is where a size is chosen.
    /// That is [`Palette::text`], and it has to read this value.
    pub font_scale: f32,
}

impl Palette {
    /// The size a run of text is drawn at.
    ///
    /// Every text size in the app goes through this one accessor — the
    /// `px(11.0)` that used to be written at each of ~45 call sites — so the
    /// overflow menu's text-size rows have a single lever, and none of those
    /// sites can quietly opt out of it. A `Palette` is copied into every
    /// renderer at the top of its render, so the scale is read fresh each
    /// frame rather than being captured once.
    pub fn text(&self, size: f32) -> gpui::Pixels {
        px(size * self.font_scale)
    }

    /// The colour an icon of this hue wears.
    ///
    /// Four of the six families already exist in the palette as its semantic
    /// colours, so a red icon and a red error are the same red. `Violet` and
    /// `Cyan` have no semantic token — nothing in the UI is a violet warning —
    /// and come from the app theme's own accent families, at the same lightness
    /// as the rest so a row of icons reads as one set.
    ///
    /// `Neutral` is body text rather than a hue: the direction and editing verbs
    /// are grey because a coloured back-arrow would be noise, not because grey
    /// is a colour they chose.
    pub fn hue(&self, hue: crate::icons::IconHue) -> Rgba {
        use crate::icons::IconHue;
        match hue {
            IconHue::Accent => self.accent,
            IconHue::Success => self.success,
            IconHue::Warning => self.warning,
            IconHue::Danger => self.danger,
            IconHue::Violet => rgba(0x8063C4FF),
            IconHue::Cyan => rgba(0x4288ACFF),
            IconHue::Neutral => self.text_dim,
        }
    }

    /// The app's dark theme: pure-black canvas, elevated chrome, one blue
    /// accent.
    pub fn t3_dark() -> Self {
        Self {
            // Surfaces. The canvas is true black; every piece of chrome is one
            // deliberately visible step above it. Controls sit on `card`, so a
            // capsule or a device card separates from the bar it lives on.
            canvas: rgba(0x000000FF),
            topbar: rgba(0x09090BFF),
            topbar_raised: rgba(0x121216FF),
            statusbar: rgba(0x09090BFF),
            sidebar: rgba(0x09090BFF),
            card: rgba(0x121216FF),
            card_hover: rgba(0x1A1A22FF),
            sidebar_active: rgba(0x1C1C24FF),
            row_hover: rgba(0x141419FF),
            divider: rgba(0x1F1F24FF),
            row_divider: rgba(0x141418FF),
            // The active device keeps its blue identity: a 6% tint under a 60%
            // accent border, crisp enough to pick out at a glance.
            device_active_bg: rgba(0x3B82F60F),
            device_active_border: rgba(0x3B82F699),

            // Interaction, as percentages of white. 8% clears the old 4% hover
            // step while staying under the 11% selection, and it brightens on
            // a card instead of disappearing into it.
            hover: rgba(0xFFFFFF14),
            selected: rgba(0xFFFFFF1C),
            selected_strong: rgba(0xFFFFFF29),
            pressed: rgba(0xFFFFFF24),
            // A capsule is a raised control on the top bar.
            capsule_bg: rgba(0x121216FF),
            capsule_border: rgba(0x222228FF),
            // A resting control, and one step denser under the pointer.
            secondary_bg: rgba(0xFFFFFF14),
            secondary_bg_hover: rgba(0xFFFFFF26),
            track: rgba(0xFFFFFF14),
            fill_soft: rgba(0xFFFFFFD9),

            // Text. Off-white at the top of the ladder rather than #FFFFFF,
            // which haloes on OLED; metadata and headers walk down through the
            // zinc greys so filenames stay dominant.
            text_header: rgba(0xF4F4F6FF),
            text_primary: rgba(0xEDEDEFFF),
            text_secondary: rgba(0xEDEDEFFF),
            text_dim: rgba(0xA1A1AAFF),
            text_muted: rgba(0x71717AFF),
            text_inverse: rgba(0xFFFFFFFF),
            text_on_light: rgba(0x000000FF),

            // Strokes. A visible structural grey: #222228 is the line between
            // the sidebar and the canvas, and around every card.
            border: rgba(0x222228FF),
            border_soft: rgba(0x222228FF),

            // Semantic. Focus and accent are the same blue, so they cannot
            // disagree. The status hues stay legible on the black canvas.
            accent: rgba(0x3B82F6FF),
            success: rgba(0x00D492FF),
            warning: rgba(0xFFB900FF),
            danger: rgba(0xFB414AFF),
            // Every tone surface is its hue at 16%, so a badge of any tone
            // carries the same weight against the same dark background.
            success_soft: rgba(0x00D49229),
            warning_soft: rgba(0xFFB90029),
            danger_soft: rgba(0xFB414A29),
            // The panel border is one step denser than its fill, so a warning
            // panel keeps a visible edge.
            danger_border: rgba(0xFB414A73),

            // A tooltip is an overlay, so it sits above every surface.
            tooltip: rgba(0x171717F2),
            tooltip_text: rgba(0xF4F4F6FF),
            tooltip_border: rgba(0xFFFFFF14),

            // No scale until a preference says otherwise; see [`set_font_scale`].
            font_scale: TEXT_SCALES[TEXT_DEFAULT_STEP],
        }
    }
}

/// Wraps [`Palette`] so it can live in GPUI's global registry.
pub struct Theme(pub Palette);

impl Global for Theme {}

/// Convenience accessor so views can write `cx.theme().canvas`.
pub trait Themed {
    fn theme(&self) -> &Palette;
}

impl Themed for App {
    fn theme(&self) -> &Palette {
        &self.global::<Theme>().0
    }
}

/// Install the palette as the app-wide theme. Called once from `main`.
pub fn install(app: &mut App) {
    app.set_global(Theme(Palette::t3_dark()));
}

/// Set the text scale [`Palette::text`] sizes by.
///
/// Writes the global rather than threading the scale through every renderer,
/// for the same reason the palette itself is a global: a view deep in the tree
/// needs it and has no `Context` of its own to ask.
pub fn set_font_scale(app: &mut App, scale: f32) {
    app.global_mut::<Theme>().0.font_scale = scale;
}

// ── Text size steps ───────────────────────────────────────────────────────────
//
// Discrete steps rather than a slider: the sizes are chosen so a step changes
// what is legible without rearranging the layout, which a free value cannot
// promise. Step 1 is the default and the palette is built at it, so a value
// outside this table is a corrupted preference and gets clamped back to the
// nearest end rather than trusted.

/// The scales the user can pick, as an index into [`TEXT_SCALES`].
pub const TEXT_DEFAULT_STEP: usize = 1;

/// Every text-size step, in menu order.
pub const TEXT_SCALES: &[f32] = &[0.9, 1.0, 1.15, 1.3];

/// The scale for a stored step, clamped to the table.
pub fn text_scale(step: usize) -> f32 {
    TEXT_SCALES[step.min(TEXT_SCALES.len() - 1)]
}

/// The row label for a step, for the overflow menu.
pub fn text_scale_label(step: usize) -> &'static str {
    match step.min(TEXT_SCALES.len() - 1) {
        0 => "Small",
        2 => "Large",
        3 => "Extra large",
        _ => "Default",
    }
}

/// The element id of a step's menu row, as `app::on_menu_select` matches it.
pub fn text_scale_id(step: usize) -> &'static str {
    match step.min(TEXT_SCALES.len() - 1) {
        0 => "text-small",
        2 => "text-large",
        3 => "text-xl",
        _ => "text-default",
    }
}

/// The step a scale names, for restoring a row's tick.
#[cfg(test)]
pub fn text_scale_step(scale: f32) -> usize {
    // Within a hair of a step is that step; anything else is the default,
    // which is also what a hand-edited preference gets.
    TEXT_SCALES
        .iter()
        .position(|s| (*s - scale).abs() < 0.001)
        .unwrap_or(TEXT_DEFAULT_STEP)
}

// ── Geometry tokens ──────────────────────────────────────────────────────────
// The GTK stylesheet expressed these as literal `min-height` / `padding` /
// `border-radius` values per rule; hoisting them keeps the builders below
// consistent with each other.

/// The window the app opens at, and the basis for the benchmarks' geometry.
pub const WINDOW_W: f32 = 1000.0;
pub const WINDOW_H: f32 = 680.0;
/// The smallest useful window.
pub const WINDOW_MIN_W: f32 = 360.0;
pub const WINDOW_MIN_H: f32 = 400.0;
/// Height of the top bar.
pub const TOPBAR_H: f32 = 45.0;
/// Height of the breadcrumb / path capsule.
pub const CAPSULE_H: f32 = 34.0;
/// Edge length of a square icon button inside a capsule.
pub const CAPSULE_BTN: f32 = 30.0;
/// Corner radius of a capsule container.
pub const RADIUS_CAPSULE: f32 = 10.0;
/// Corner radius of a button inside a capsule.
pub const RADIUS_CAPSULE_BTN: f32 = 7.0;
/// Corner radius of a sidebar row.
pub const RADIUS_ROW: f32 = 8.0;
/// Corner radius of a device card.
pub const RADIUS_CARD: f32 = 12.0;
/// Height of the bottom status strip.
pub const STATUSBAR_H: f32 = 30.0;
/// Full-pill radius, used for badges and LEDs.
pub const RADIUS_PILL: f32 = 9999.0;
/// Monospace stack used for paths, counts and status lines.
pub const MONO: &str = "JetBrains Mono, DejaVu Sans Mono, monospace";

#[cfg(test)]
mod tests {
    use super::*;

    /// One row per step, and no two rows alike — a duplicated or missing row is
    /// how a menu ends up with two ticked entries or none.
    #[test]
    fn every_step_has_its_own_row() {
        let mut ids = Vec::new();
        let mut labels = Vec::new();
        for step in 0..TEXT_SCALES.len() {
            ids.push(text_scale_id(step));
            labels.push(text_scale_label(step));
        }
        let ids: std::collections::HashSet<&str> = ids.into_iter().collect();
        let labels: std::collections::HashSet<&str> = labels.into_iter().collect();
        assert_eq!(ids.len(), TEXT_SCALES.len(), "duplicate row id");
        assert_eq!(labels.len(), TEXT_SCALES.len(), "duplicate row label");
    }

    /// The scales climb and stay inside what the layout can absorb: text is
    /// sized through `Palette::text`, but every height and padding in the app
    /// is absolute, so a step too large would put a label outside its own row.
    #[test]
    fn steps_climb_and_stay_small() {
        let mut previous = 0.0;
        for &scale in TEXT_SCALES {
            assert!(scale > previous, "{scale} does not follow");
            assert!(scale <= 1.5, "{scale} would not fit the fixed layout");
            previous = scale;
        }
    }

    /// A scale round-trips to the step it came from, so a stored step and a
    /// shown tick can never disagree.
    #[test]
    fn a_scale_names_its_step() {
        for step in 0..TEXT_SCALES.len() {
            assert_eq!(text_scale_step(text_scale(step)), step);
        }
    }

    /// An out-of-range step — a hand-edited preferences file, or one written by
    /// a build with more steps than this one — lands on a real step rather
    /// than scaling text by an invented number.
    #[test]
    fn an_unknown_step_clamps() {
        let last = TEXT_SCALES.len() - 1;
        assert_eq!(text_scale(usize::MAX), TEXT_SCALES[last]);
        assert_eq!(text_scale(0), TEXT_SCALES[0]);
    }

    /// The scale reaches the sizes the views ask for.
    #[test]
    fn text_is_scaled_by_the_palette() {
        let mut t = Palette::t3_dark();
        assert_eq!(f32::from(t.text(10.0)), 10.0);
        t.font_scale = 1.3;
        assert_eq!(f32::from(t.text(10.0)), 13.0);
    }
}

//! Design tokens for the adbshare frontend.
//!
//! GPUI has no CSS cascade and no theme type of its own, so the whole visual
//! system lives here as tokens, installed as an [`gpui::Global`] that views read
//! without threading it through every call.
//!
//! # The palette is T3 Code's dark theme
//!
//! adbshare is styled as a sibling of the **T3 Code** desktop app, so its theme
//! is copied from that app rather than invented. The values below were read out
//! of T3 Code's own stylesheet, `apps/server/dist/client/assets/main-*.css`
//! inside its Electron `app.asar`, by resolving the CSS custom properties its
//! dark mode declares. T3 Code is itself built on Vercel's design system —
//! Tailwind v4's `neutral`/`zinc` greys on near-black, one blue accent, and
//! every interaction expressed as a percentage of white — so this is also
//! simply what "the Vercel theme" looks like as an application.
//!
//! The source declares its greys in OKLCH. Those are converted to sRGB here
//! (GPUI has no OKLCH) and the results are checked against the stylesheet's own
//! hex fallbacks, which agree exactly:
//!
//! ```text
//! --background   neutral-950  #0a0a0a     --foreground     neutral-100  #f5f5f5
//! --card         bg + 3% wht  #111111     --popover        card          #111111
//! --primary      oklch 57% .21 264  #346bf1  --muted          white 3%
//! --accent       white 4%               --border         white 6%
//! --input        white 8%               --error          red-500 + 10% white #fb414a
//! ```
//!
//! Two places T3 Code overrides the shared tokens. Its sidebar is *purer* black
//! than the canvas (`#000` against `#0a0a0a`) and carries its own text greys,
//! and that is reproduced in [`Palette::sidebar`] and [`Palette::text_dim`]
//! rather than lost to the root values.
//!
//! One token is deliberately absent: `scrollbar.thumb.background`. GPUI 0.2.2
//! has no scrollbar colour API at all, so a palette entry for it would be a
//! value nothing could read.
//!
//! Surfaces are the theme's own *opaque* steps, because near-black greys a
//! browser cannot round-trip would band. For hover, selection and every other
//! interaction, which have to work on top of four different surfaces, the
//! theme's translucent white percentages are used instead: unlike a fixed
//! opaque value those stay legible on whichever surface is underneath.
//!
//! Dark only, as the previous GTK build was. A light palette is one more
//! constructor here plus a branch in [`install`].

use gpui::{App, Global, Rgba, rgba};

/// The full set of colour tokens used across the UI.
///
/// Every field carries the T3 Code token it came from, so the mapping is
/// auditable rather than a matter of taste.
#[derive(Clone, Copy)]
pub struct Palette {
    // ── Surfaces ───────────────────────────────────────────────────────────
    /// `--background`. The file area, and the window behind everything.
    pub canvas: Rgba,
    /// `--toolbar-background`, which the theme points back at `background`.
    pub topbar: Rgba,
    /// `--popover`: the raised step every control and strip on the toolbar
    /// sits on.
    pub topbar_raised: Rgba,
    /// The chrome behind the bottom status strip, `background`.
    pub statusbar: Rgba,
    /// The sidebar pane, which T3 Code pins to pure black.
    pub sidebar: Rgba,
    /// `--card`. Dialogs, popovers and the panels inside them.
    pub card: Rgba,
    /// `--surface-raised`. A resting row or tile.
    pub surface_raised: Rgba,

    // ── Interaction ────────────────────────────────────────────────────────
    // Percentages of white rather than fixed greys, so one value works over
    // the canvas, the pure-black sidebar and a card alike.
    /// A resting element under the pointer, `--accent`.
    pub hover: Rgba,
    /// A selected element, `--sidebar-row-active`.
    pub selected: Rgba,
    /// A selected element under the pointer, one step past `--accent`.
    pub selected_strong: Rgba,
    /// A pressed element.
    pub pressed: Rgba,
    /// A group of icon buttons: `--toolbar-control`.
    pub capsule_bg: Rgba,
    /// `--input`, the outline around a capsule.
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
    /// `--foreground`. Headlines and anything that should read as emphasised.
    pub text_header: Rgba,
    /// `--foreground`. Body copy.
    pub text_primary: Rgba,
    /// `--foreground`, at the weight a label uses rather than a heading.
    pub text_secondary: Rgba,
    /// The sidebar's own `--muted-foreground`. Secondary labels and paths.
    pub text_dim: Rgba,
    /// `--muted-foreground`. Placeholders and dimmed icons.
    pub text_muted: Rgba,
    /// `--primary-foreground`: text drawn on an accent fill.
    pub text_inverse: Rgba,
    /// Ink for a `--foreground` *fill*: a filled badge or a primary button.
    /// The canvas colour, because that is what the theme puts on its own
    /// foreground — which is the one fill light enough to need dark text.
    pub text_on_light: Rgba,

    // ── Strokes ─────────────────────────────────────────────────────────────
    /// `--border`.
    pub border: Rgba,
    /// `--border`. Hairlines and dividers; the theme defines only this step.
    pub border_soft: Rgba,

    // ── Semantic ───────────────────────────────────────────────────────────
    /// `--primary`. Accent text and icons.
    pub accent: Rgba,
    /// `--ring`, which the theme points at `--primary`: a focused outline.
    pub accent_muted: Rgba,
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
}

impl Palette {
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

    /// T3 Code's dark theme: Vercel's greys on near-black, one blue accent.
    pub fn t3_dark() -> Self {
        Self {
            // Surfaces. T3 Code's canvas is `neutral-950`, and every raised
            // surface is that same colour mixed 3% towards white — so the
            // ladder is 7 levels of 255 wide rather than a set of greys.
            canvas: rgba(0x0A0A0AFF),
            topbar: rgba(0x0A0A0AFF),
            topbar_raised: rgba(0x111111FF),
            statusbar: rgba(0x0A0A0AFF),
            // The sidebar is the one surface T3 Code overrides to pure black,
            // which is what makes it read as a distinct pane on a near-black
            // canvas rather than blending into it.
            sidebar: rgba(0x000000FF),
            card: rgba(0x111111FF),
            surface_raised: rgba(0x111111FF),

            // Interaction, as the theme's own percentages of white. `--accent`
            // is white 4% and `--sidebar-row-active` white 11%, so hover sits
            // at 6% to clear both while staying under the selection step.
            hover: rgba(0xFFFFFF0F),
            selected: rgba(0xFFFFFF1C),
            selected_strong: rgba(0xFFFFFF29),
            pressed: rgba(0xFFFFFF24),
            // A capsule is a `--toolbar-control`, and T3 Code points that at
            // `--popover`; it is outlined with `--input`, white 8%.
            capsule_bg: rgba(0x111111FF),
            capsule_border: rgba(0xFFFFFF14),
            // A resting control, and one step denser under the pointer, so a
            // secondary button tracks the same ladder a hovered row does.
            secondary_bg: rgba(0xFFFFFF14),
            secondary_bg_hover: rgba(0xFFFFFF26),
            track: rgba(0xFFFFFF14),
            fill_soft: rgba(0xFFFFFFD9),

            // Text. `--foreground` is `neutral-100`. The two greys below it are
            // the theme's own pair: `#a3a3a3` from the sidebar, `#818181` from
            // `neutral-500` mixed 10% towards white.
            text_header: rgba(0xF5F5F5FF),
            text_primary: rgba(0xF5F5F5FF),
            text_secondary: rgba(0xF5F5F5FF),
            text_dim: rgba(0xA3A3A3FF),
            text_muted: rgba(0x818181FF),
            // T3 Code draws accent-filled labels in white, not in a dark ink.
            text_inverse: rgba(0xFFFFFFFF),
            // ...and puts the canvas colour back on its own foreground fill.
            text_on_light: rgba(0x0A0A0AFF),

            // Strokes. `--border` is white 6%; the soft variant is the same
            // step, which is the only hairline the theme defines.
            border: rgba(0xFFFFFF0F),
            border_soft: rgba(0xFFFFFF0F),

            // Semantic. `--primary` and `--ring` are the same blue, so focus and
            // accent cannot disagree. The three status hues are the theme's
            // `*-400` foregrounds, which are what it uses for status text on a
            // dark surface; `--error` is `red-500` lifted 10% towards white.
            accent: rgba(0x346BF1FF),
            accent_muted: rgba(0x346BF1FF),
            success: rgba(0x00D492FF),
            warning: rgba(0xFFB900FF),
            danger: rgba(0xFB414AFF),
            // Every tone surface in T3 Code is its hue at 16%, so a badge of any
            // tone carries the same weight against the same dark background.
            success_soft: rgba(0x00D49229),
            warning_soft: rgba(0xFFB90029),
            danger_soft: rgba(0xFB414A29),
            // The panel border is one step denser than its fill, so a warning
            // panel keeps a visible edge.
            danger_border: rgba(0xFB414A73),

            // A tooltip is an overlay, so it sits above every surface: above
            // `#111111` rather than on it, and opaque enough to read against
            // whatever it hovers.
            tooltip: rgba(0x171717F2),
            tooltip_text: rgba(0xF5F5F5FF),
            tooltip_border: rgba(0xFFFFFF14),
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
/// Corner radius of an omnibar / context bar.
pub const RADIUS_BAR: f32 = 10.0;
/// Height of the bottom status strip.
pub const STATUSBAR_H: f32 = 30.0;
/// Full-pill radius, used for badges and LEDs.
pub const RADIUS_PILL: f32 = 9999.0;
/// Monospace stack used for paths, counts and status lines.
pub const MONO: &str = "JetBrains Mono, DejaVu Sans Mono, monospace";

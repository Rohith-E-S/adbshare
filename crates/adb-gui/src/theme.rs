//! Design tokens for the adbshare frontend.
//!
//! GPUI has no CSS cascade and no theme type of its own, so the whole visual
//! system lives here as tokens, installed as an [`gpui::Global`] that views read
//! without threading it through every call.
//!
//! The palette is Zed's **One Dark**, the default theme of the editor this
//! frontend is now visually a sibling of. The values below are that theme's own
//! tokens, read from `zed-industries/zed` at `assets/themes/one/one.json`, so the
//! two applications agree on what a surface, a border or a warning looks like:
//!
//! ```text
//! background            #3b414d   text                #dce0e5
//! surface.background    #2f343e   text.muted          #a9afbc
//! element.background    #2e343e   text.disabled       #878a98
//! element.hover         #363c46   text.accent         #74ade8
//! element.active        #454a56   border              #464b57
//! border.variant        #363c46   border.focused      #47679e
//! error                 #d07277   success             #a1c181
//! warning               #dec184
//! ```
//!
//! One token is deliberately absent: `scrollbar.thumb.background`. GPUI 0.2.2
//! has no scrollbar colour API at all, so a palette entry for it would be a
//! value nothing could read.
//!
//! Zed steps between *opaque* surface colours rather than layering translucent
//! white. Those steps are used directly for the surfaces. For hover and
//! selection, which have to work on top of three different surfaces, the
//! equivalent translucent step is used instead: the numbers reproduce Zed's own
//! ladder, and unlike a fixed opaque value they stay legible on whichever
//! surface happens to be underneath.
//!
//! Dark only, as the previous GTK build was. A light palette is one more
//! constructor here plus a branch in [`install`].

use gpui::{App, Global, Rgba, rgba};

/// The full set of colour tokens used across the UI.
///
/// Every field carries the Zed token it came from, so the mapping is auditable
/// rather than a matter of taste.
#[derive(Clone, Copy)]
pub struct Palette {
    // ── Surfaces ───────────────────────────────────────────────────────────
    /// `background`. The file area, and the window behind everything.
    pub canvas: Rgba,
    /// `title_bar.background`, same as `background` in One Dark.
    pub topbar: Rgba,
    /// `toolbar.background`: the darker step Zed uses for its tool strips.
    pub topbar_raised: Rgba,
    /// `status_bar.background`.
    pub statusbar: Rgba,
    /// `surface.background`. The sidebar.
    pub sidebar: Rgba,
    /// `elevated_surface.background`. Cards, popovers and dialogs.
    pub card: Rgba,
    /// `element.background`. A resting row or tile.
    pub surface_raised: Rgba,

    // ── Interaction ────────────────────────────────────────────────────────
    // Translucent equivalents of Zed's `element.hover` and `element.active`
    // steps, so they work over the canvas, the sidebar and a card alike.
    /// A resting element under the pointer, `element.hover`.
    pub hover: Rgba,
    /// A selected element, `element.active`.
    pub selected: Rgba,
    /// A selected element under the pointer, one step past `element.active`.
    pub selected_strong: Rgba,
    /// A pressed element.
    pub pressed: Rgba,
    /// A group of icon buttons, `ghost_element.background`.
    pub capsule_bg: Rgba,
    /// `border.variant`, used for the outline around a capsule.
    pub capsule_border: Rgba,
    /// The groove behind a progress bar.
    pub track: Rgba,
    /// The fill of a progress bar.
    pub fill_soft: Rgba,

    // ── Text ───────────────────────────────────────────────────────────────
    /// `text`. Headlines and anything that should read as emphasised.
    pub text_header: Rgba,
    /// `text`. Body copy.
    pub text_primary: Rgba,
    /// `text`, at the weight a label uses rather than a heading.
    pub text_secondary: Rgba,
    /// `text.muted`. Secondary labels and paths.
    pub text_dim: Rgba,
    /// `text.disabled`. Placeholders and dimmed icons.
    pub text_muted: Rgba,
    /// For text drawn on an accent fill.
    pub text_inverse: Rgba,

    // ── Strokes ─────────────────────────────────────────────────────────────
    /// `border`.
    pub border: Rgba,
    /// `border.variant`. Hairlines and dividers.
    pub border_soft: Rgba,

    // ── Semantic ───────────────────────────────────────────────────────────
    /// `text.accent`.
    pub accent: Rgba,
    /// `border.focused`. A focused or active outline.
    pub accent_muted: Rgba,
    /// `success`.
    pub success: Rgba,
    /// `warning`.
    pub warning: Rgba,
    /// `error`.
    pub danger: Rgba,
    /// `error` at low opacity, for a warning panel.
    pub danger_soft: Rgba,
    /// `error` at medium opacity, for that panel's border.
    pub danger_border: Rgba,
}

impl Palette {
    /// Zed's One Dark, the editor's default dark theme.
    pub fn one_dark() -> Self {
        Self {
            // Surfaces, straight from the theme file.
            canvas: rgba(0x3B414DFF),
            topbar: rgba(0x3B414DFF),
            topbar_raised: rgba(0x282C33FF),
            statusbar: rgba(0x3B414DFF),
            sidebar: rgba(0x2F343EFF),
            card: rgba(0x2F343EFF),
            surface_raised: rgba(0x2E343EFF),

            // Interaction. Each is the translucent form of the opaque step Zed
            // uses: #ffffff at 6% lifts `element.background` (#2e343e) to about
            // #3c4049, and at 11% to about #454a56, which is exactly
            // `element.active`.
            hover: rgba(0xFFFFFF0F),
            selected: rgba(0xFFFFFF1C),
            selected_strong: rgba(0xFFFFFF28),
            pressed: rgba(0x0000002E),
            capsule_bg: rgba(0x00000026),
            capsule_border: rgba(0xFFFFFF1A),
            track: rgba(0x00000040),
            fill_soft: rgba(0xFFFFFFB3),

            // Text.
            text_header: rgba(0xDCE0E5FF),
            text_primary: rgba(0xDCE0E5FF),
            text_secondary: rgba(0xDCE0E5FF),
            text_dim: rgba(0xA9AFBCFF),
            text_muted: rgba(0x878A98FF),
            text_inverse: rgba(0x1B1F26FF),

            // Strokes.
            border: rgba(0x464B57FF),
            border_soft: rgba(0x363C46FF),

            // Semantic.
            accent: rgba(0x74ADE8FF),
            accent_muted: rgba(0x47679EFF),
            success: rgba(0xA1C181FF),
            warning: rgba(0xDEC184FF),
            danger: rgba(0xD07277FF),
            danger_soft: rgba(0xD0727726),
            danger_border: rgba(0xD0727773),
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
    app.set_global(Theme(Palette::one_dark()));
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

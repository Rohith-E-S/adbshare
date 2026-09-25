//! Design tokens for the adbshare frontend.
//!
//! This replaces the 975-line GTK stylesheet. GPUI has no CSS cascade and no
//! `Theme` type of its own, so every colour, radius and spacing value from the
//! old `style.css` is expressed here as a token, and the palette is installed
//! as an [`gpui::Global`] so views can read it without threading it through
//! every call.
//!
//! The app is dark-only, matching the GTK build, which forced a dark stylesheet
//! and never consulted the desktop's colour scheme. Swapping in a light palette
//! later means adding one constructor here and installing it in `main`.

use gpui::{App, Global, Rgba, rgba};

/// The full set of colour tokens used across the UI.
#[derive(Clone, Copy)]
pub struct Palette {
    // ── Surfaces ───────────────────────────────────────────────────────────
    /// Window / file-canvas base.
    pub canvas: Rgba,
    /// Top bar gradient start.
    pub topbar_top: Rgba,
    /// Sidebar background.
    pub sidebar: Rgba,
    /// Cards, popovers and dialogs.
    pub card: Rgba,
    /// Hovered rows and list surfaces.
    pub surface_raised: Rgba,
    /// Rounded container behind groups of icon buttons.
    pub capsule_bg: Rgba,
    pub capsule_border: Rgba,

    // ── Text ───────────────────────────────────────────────────────────────
    /// Headlines and active states.
    pub text_header: Rgba,
    /// Body text and descriptions.
    pub text_primary: Rgba,
    /// Secondary labels.
    pub text_secondary: Rgba,
    /// Dim labels and hints.
    pub text_dim: Rgba,
    /// Placeholders and disabled icons.
    pub text_muted: Rgba,
    /// Text drawn on a light fill.
    pub text_inverse: Rgba,

    // ── Strokes ────────────────────────────────────────────────────────────
    pub border: Rgba,
    pub border_soft: Rgba,

    // ── Interaction ────────────────────────────────────────────────────────
    pub hover: Rgba,
    pub hover_strong: Rgba,
    pub pressed: Rgba,
    /// Progress-bar groove.
    pub track: Rgba,
    /// Progress-bar fill, used as a gradient's faint end.
    pub fill_soft: Rgba,

    // ── Semantic ───────────────────────────────────────────────────────────
    pub success: Rgba,
    pub danger: Rgba,
    pub danger_text: Rgba,
    pub warning: Rgba,
}

impl Palette {
    /// The dark palette, ported 1:1 from the GTK stylesheet's `@define-color`
    /// block and the individual rule colours.
    pub fn dark() -> Self {
        Self {
            canvas: rgba(0x0A0E15FF),
            topbar_top: rgba(0x10141CFF),
            sidebar: rgba(0x12161EFF),
            card: rgba(0x181D27FF),

            surface_raised: rgba(0xFFFFFF08),
            capsule_bg: rgba(0xFFFFFF0A),
            capsule_border: rgba(0xFFFFFF0F),

            text_header: rgba(0xFFFFFFFF),
            text_primary: rgba(0xE0E4EBFF),
            text_secondary: rgba(0xD1D6E0FF),
            text_dim: rgba(0xBFC6D4FF),
            text_muted: rgba(0x667085FF),
            text_inverse: rgba(0x0A0E15FF),

            border: rgba(0x373F4E73),
            border_soft: rgba(0x373F4E66),

            hover: rgba(0xFFFFFF0A),
            hover_strong: rgba(0xFFFFFF14),
            pressed: rgba(0xFFFFFF1F),
            track: rgba(0xFFFFFF0F),
            fill_soft: rgba(0xFFFFFF80),

            success: rgba(0x22C55EFF),
            danger: rgba(0xEF4444FF),
            danger_text: rgba(0xFCA5A5FF),
            warning: rgba(0xFACC15FF),
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
    app.set_global(Theme(Palette::dark()));
}

// ── Geometry tokens ──────────────────────────────────────────────────────────
// The GTK stylesheet expressed these as literal `min-height` / `padding` /
// `border-radius` values per rule; hoisting them keeps the builders below
// consistent with each other.

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
/// Monospace stack used wherever the old CSS said `font-family: monospace`.
pub const MONO: &str = "JetBrains Mono, DejaVu Sans Mono, monospace";

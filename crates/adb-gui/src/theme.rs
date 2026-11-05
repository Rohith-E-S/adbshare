//! Design tokens for the adbshare frontend.
//!
//! GPUI has no CSS cascade and no theme type of its own, so the whole visual
//! system lives here as tokens, installed as an [`gpui::Global`] that views read
//! without threading it through every call.
//!
//! # A deep slate workbench
//!
//! The file canvas is a deep ink-slate (`#0B0E12`) rather than pure black, so
//! the whole window reads as one graphite instrument instead of a void with
//! floating controls. Chrome steps up through `#10141A` and `#161C24`, cards
//! and popovers sit at `#171D26`, and hairlines at `#232C37` give every pane a
//! precise edge. Structure reads from fill first, border second.
//!
//! The accent is a restrained cool cyan-blue (`#5CB6D9`): it marks the active
//! device, the current location and live transfers, and nowhere else. States
//! are kept off the accent ramp, so a selected row, a hovered control and a
//! running job cannot be mistaken for one another.
//!
//! Text avoids pure white. The strongest ink is `#F3F6F8`, body copy
//! `#E6EBF0`, secondary labels `#D3DAE1`, metadata `#97A3B0`, and section
//! headers `#66717E`. That ladder keeps sizes, dates and storage stats
//! scannable without letting them compete with file names.
//!
//! Surfaces are *opaque* steps, because near-black greys a browser cannot
//! round-trip would band. Interaction fills are percentages of white instead:
//! one value stays legible over the canvas, the chrome and a card alike.
//!
//! One token is deliberately absent: `scrollbar.thumb.background`. GPUI 0.2.2
//! has no scrollbar colour API at all, so a palette entry for it would be a
//! value nothing could read.
//!
//! Both appearances exist ([`Palette::t3_dark`] and [`Palette::t3_light`]),
//! following the system via GPUI's window appearance, with a forced choice
//! from the `ADBSHARE_APPEARANCE` environment variable. The GTK build was
//! dark only; this one is not.

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
    /// A neutral accent wash for active chrome: the current breadcrumb, an
    /// open toolbar button, the active device card.
    pub accent_soft: Rgba,
    /// The accent as an outline, one step quieter than [`Self::accent`].
    pub accent_border: Rgba,

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
    /// A resting raised control: a secondary button, a neutral badge.
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
    /// Chrome is monochrome (see [`crate::icons::IconHue`]), so this is mostly
    /// one answer: `Neutral` is the dim ink every resting glyph wears, and the
    /// two semantic hues reuse the palette's own warning and error colours,
    /// so a red icon and a red error are the same red. Accent is not a hue
    /// here at all — it is state, and the call sites that know the state
    /// (a toggled button, a live transfer, a focal empty-state glyph) ask
    /// for [`Self::accent`] directly.
    pub fn hue(&self, hue: crate::icons::IconHue) -> Rgba {
        use crate::icons::IconHue;
        match hue {
            IconHue::Warning => self.warning,
            IconHue::Danger => self.danger,
            IconHue::Neutral => self.text_dim,
        }
    }

    /// The app's dark theme: a deep slate workbench with one cool cyan-blue
    /// accent.
    pub fn t3_dark() -> Self {
        Self {
            // Surfaces. A deep ink-slate canvas; chrome and cards climb a
            // visible step so the window reads as panes on one instrument.
            canvas: rgba(0x0B0E12FF),
            topbar: rgba(0x10141AFF),
            statusbar: rgba(0x0E1217FF),
            sidebar: rgba(0x0E1217FF),
            card: rgba(0x171D26FF),
            card_hover: rgba(0x1E2631FF),
            sidebar_active: rgba(0x1B2530FF),
            row_hover: rgba(0x151B22FF),
            divider: rgba(0x232C37FF),
            row_divider: rgba(0x171E26FF),
            // The active device keeps its cyan identity: a 10% tint under a
            // 70% accent border, crisp enough to pick out at a glance.
            device_active_bg: rgba(0x5CB6D91A),
            device_active_border: rgba(0x5CB6D9B3),
            accent_soft: rgba(0x5CB6D91F),
            accent_border: rgba(0x5CB6D98C),

            // Interaction, as percentages of white. 5% is a hover step that
            // clears the slate canvas without bleaching a card; selection and
            // its hover step stay brighter than any resting control.
            hover: rgba(0xFFFFFF0D),
            selected: rgba(0xFFFFFF14),
            selected_strong: rgba(0xFFFFFF21),
            pressed: rgba(0xFFFFFF1C),
            // A resting control, and one step denser under the pointer.
            secondary_bg: rgba(0xFFFFFF12),
            secondary_bg_hover: rgba(0xFFFFFF22),
            track: rgba(0xFFFFFF14),
            fill_soft: rgba(0xFFFFFFD9),

            // Text. Off-white at the top of the ladder rather than #FFFFFF,
            // which blooms on a dark panel; metadata and headers walk down
            // through the cool greys so filenames stay dominant.
            text_header: rgba(0xF3F6F8FF),
            text_primary: rgba(0xE6EBF0FF),
            text_secondary: rgba(0xD3DAE1FF),
            text_dim: rgba(0x97A3B0FF),
            text_muted: rgba(0x66717EFF),
            text_inverse: rgba(0xFFFFFFFF),
            text_on_light: rgba(0x081016FF),

            // Strokes. A visible cool grey: #232C37 is the line between the
            // sidebar and the canvas, and around every popover.
            border: rgba(0x26303BFF),
            border_soft: rgba(0x1E262FFF),

            // Semantic. The accent is one restrained cyan-blue, used only for
            // identity and live work. Status hues stay legible on the slate.
            accent: rgba(0x5CB6D9FF),
            success: rgba(0x41D6A3FF),
            warning: rgba(0xE9B45AFF),
            danger: rgba(0xEE6673FF),
            // Every tone surface is its hue at 16%, so a badge of any tone
            // carries the same weight against the same dark background.
            success_soft: rgba(0x41D6A329),
            warning_soft: rgba(0xE9B45A29),
            danger_soft: rgba(0xEE667329),
            // The panel border is one step denser than its fill, so a warning
            // panel keeps a visible edge.
            danger_border: rgba(0xEE667380),

            // A tooltip is an overlay, so it sits above every surface.
            tooltip: rgba(0x1A222CF2),
            tooltip_text: rgba(0xF3F6F8FF),
            tooltip_border: rgba(0x2A3642FF),

            // No scale until a preference says otherwise; see [`set_font_scale`].
            font_scale: TEXT_SCALES[TEXT_DEFAULT_STEP],
        }
    }

    /// The light theme: the same slate instrument, inverted.
    ///
    /// Surfaces climb from a cool paper canvas to white cards; text walks
    /// down from near-black ink. The accent is the dark palette's cyan
    /// darkened until it passes contrast on white, and the semantic hues
    /// darken with it. Interaction fills are percentages of *black* here,
    /// where the dark palette's are percentages of white — one value that
    /// works over every surface in either mode. Tooltips stay dark in both
    /// modes, as GNOME's do.
    pub fn t3_light() -> Self {
        Self {
            canvas: rgba(0xF4F6F9FF),
            topbar: rgba(0xE9EDF2FF),
            statusbar: rgba(0xEDF0F4FF),
            sidebar: rgba(0xEDF0F4FF),
            card: rgba(0xFFFFFFFF),
            card_hover: rgba(0xF0F3F7FF),
            sidebar_active: rgba(0xDCE5EEFF),
            row_hover: rgba(0xEEF1F5FF),
            divider: rgba(0xD6DDE5FF),
            row_divider: rgba(0xE3E8EEFF),
            device_active_bg: rgba(0x1F7FA81A),
            device_active_border: rgba(0x1F7FA8B3),
            accent_soft: rgba(0x1F7FA81F),
            accent_border: rgba(0x1F7FA88C),

            hover: rgba(0x0000000D),
            selected: rgba(0x00000014),
            selected_strong: rgba(0x00000021),
            pressed: rgba(0x0000001C),
            secondary_bg: rgba(0x0000000F),
            secondary_bg_hover: rgba(0x0000001F),
            track: rgba(0x00000014),
            fill_soft: rgba(0x1F2933D9),

            text_header: rgba(0x11161CFF),
            text_primary: rgba(0x1C242EFF),
            text_secondary: rgba(0x3A4552FF),
            text_dim: rgba(0x5C6875FF),
            text_muted: rgba(0x8A95A1FF),
            text_inverse: rgba(0xFFFFFFFF),
            text_on_light: rgba(0xFFFFFFFF),

            border: rgba(0xC3CDD7FF),
            border_soft: rgba(0xDDE3EAFF),

            accent: rgba(0x1F7FA8FF),
            success: rgba(0x0E9F6EFF),
            warning: rgba(0xB7791FFF),
            danger: rgba(0xD64550FF),
            success_soft: rgba(0x0E9F6E29),
            warning_soft: rgba(0xB7791F29),
            danger_soft: rgba(0xD6455029),
            danger_border: rgba(0xD6455080),

            tooltip: rgba(0x1A222CF2),
            tooltip_text: rgba(0xF3F6F8FF),
            tooltip_border: rgba(0x2A3642FF),

            font_scale: TEXT_SCALES[TEXT_DEFAULT_STEP],
        }
    }
}

/// Wraps [`Palette`] so it can live in GPUI's global registry, along with
/// which appearance it was built for.
pub struct Theme {
    pub palette: Palette,
    /// Whether [`Self::palette`] is the dark one. Compared against the
    /// window appearance on every render, so a system switch to light (or
    /// back) swaps the palette live rather than on the next launch.
    pub dark: bool,
    /// A forced appearance from `ADBSHARE_APPEARANCE` (`light`/`dark`), so
    /// the light palette can be reached — and screenshotted — on a dark
    /// system. `None` follows the system.
    pub forced: Option<bool>,
}

impl Global for Theme {}

/// Convenience accessor so views can write `cx.theme().canvas`.
pub trait Themed {
    fn theme(&self) -> &Palette;
}

impl Themed for App {
    fn theme(&self) -> &Palette {
        &self.global::<Theme>().palette
    }
}

/// Whether the system currently wants dark windows, per GPUI's appearance
/// (the XDG portal's `color-scheme` on Linux).
fn system_dark(app: &App) -> bool {
    matches!(
        app.window_appearance(),
        gpui::WindowAppearance::Dark | gpui::WindowAppearance::VibrantDark
    )
}

/// Install the palette as the app-wide theme. Called once from `main`.
///
/// The initial choice is only a first guess: on Linux the portal's
/// appearance report can arrive after startup, so [`sync_appearance`]
/// re-checks on every render and swaps if the system has since spoken.
pub fn install(app: &mut App) {
    let forced = std::env::var("ADBSHARE_APPEARANCE").ok().and_then(|v| {
        match v.trim().to_ascii_lowercase().as_str() {
            "light" => Some(false),
            "dark" => Some(true),
            _ => None,
        }
    });
    let dark = forced.unwrap_or_else(|| system_dark(app));
    app.set_global(Theme {
        palette: if dark {
            Palette::t3_dark()
        } else {
            Palette::t3_light()
        },
        dark,
        forced,
    });
    app.set_global(UiFont(resolve_system_font(app)));
}

/// Swap the palette if the appearance has changed since it was built.
///
/// Called from the app root's render, before the palette is read. The text
/// scale lives in the palette, so it is carried across the swap — a theme
/// change must not silently reset the user's text size.
pub fn sync_appearance(app: &mut App) {
    let want_dark = app
        .global::<Theme>()
        .forced
        .unwrap_or_else(|| system_dark(app));
    let theme = app.global_mut::<Theme>();
    if theme.dark != want_dark {
        let scale = theme.palette.font_scale;
        theme.palette = if want_dark {
            Palette::t3_dark()
        } else {
            Palette::t3_light()
        };
        theme.palette.font_scale = scale;
        theme.dark = want_dark;
    }
}

/// Set the text scale [`Palette::text`] sizes by.
///
/// Writes the global rather than threading the scale through every renderer,
/// for the same reason the palette itself is a global: a view deep in the tree
/// needs it and has no `Context` of its own to ask.
pub fn set_font_scale(app: &mut App, scale: f32) {
    app.global_mut::<Theme>().palette.font_scale = scale;
}

/// The UI font family, resolved once at install.
pub struct UiFont(pub gpui::SharedString);

impl Global for UiFont {}

/// The resolved UI font, for the app root to set on its element tree.
pub fn font_family(app: &App) -> gpui::SharedString {
    app.global::<UiFont>().0.clone()
}

/// The system sans-serif family, as a name GPUI's text system can load.
///
/// GPUI on Linux maps its `.SystemUIFont` to "IBM Plex Sans" (its own TODO
/// says the proper system font is undetermined), so the app would render in
/// a bundled-looking font on every desktop. Ask fontconfig which family its
/// sans-serif alias resolves to — Cantarell on GNOME, Noto Sans on KDE —
/// and fall back to the first well-known system sans the text system
/// actually has, then to GPUI's default.
fn resolve_system_font(app: &App) -> gpui::SharedString {
    let available: std::collections::HashSet<String> =
        app.text_system().all_font_names().into_iter().collect();
    let known = |name: &str| available.contains(name);

    // The desktop's own interface font first: GNOME records it in
    // `org.gnome.desktop.interface font-name` ("Adwaita Sans 11", family
    // plus size), which is what every GTK app renders in — fontconfig's
    // bare "sans-serif" alias resolves to Liberation Sans here, which is
    // nobody's interface font.
    let gsettings_family = std::process::Command::new("gsettings")
        .args(["get", "org.gnome.desktop.interface", "font-name"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|value| {
            let value = value.trim().trim_matches('\'').to_string();
            match value.rsplit_once(' ') {
                Some((name, size)) if size.chars().all(|c| c.is_ascii_digit()) => name.to_string(),
                _ => value,
            }
        });
    if let Some(family) = gsettings_family.filter(|name| known(name)) {
        return family.into();
    }
    let fc_family = std::process::Command::new("fc-match")
        .args(["-f", "%{family}", "sans-serif"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .and_then(|family| family.split(',').next().map(str::trim).map(str::to_string));
    if let Some(first) = fc_family.filter(|name| known(name)) {
        return first.into();
    }
    for candidate in [
        "Cantarell",
        "Adwaita Sans",
        "Noto Sans",
        "DejaVu Sans",
        "Inter",
    ] {
        if known(candidate) {
            return candidate.into();
        }
    }
    ".SystemUIFont".into()
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
/// Height of the top-bar controls: the path bar and the button groups.
pub const CAPSULE_H: f32 = 34.0;
/// Edge length of a square icon button in the top bar.
pub const CAPSULE_BTN: f32 = 30.0;
/// Corner radius of a button group container. Groups are flat now (no fill,
/// no outline — see `ui::capsule`), so this only rounds the group's box.
pub const RADIUS_CAPSULE: f32 = 9.0;
/// Corner radius of a control: a button, a menu row, a text field. One
/// radius for every control in the app.
pub const RADIUS_CAPSULE_BTN: f32 = 6.0;
/// Corner radius of a dialog, one step past a card: a modal sits above the
/// popovers and reads as the topmost surface by its radius as well as its
/// backdrop.
pub const RADIUS_DIALOG: f32 = 12.0;
/// Corner radius of a sidebar row.
pub const RADIUS_ROW: f32 = 6.0;
/// Corner radius of a device card.
pub const RADIUS_CARD: f32 = 10.0;
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

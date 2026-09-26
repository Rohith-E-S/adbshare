//! Menu model and rendering, shared by the overflow menu and context menus.
//!
//! GPUI has no menu widget, so a menu is a list of [`MenuItem`]s rendered into
//! a card. Callers build the list for the current state and get back the id of
//! whatever was clicked, which keeps the "what is on offer" decision in the
//! views and this module responsible only for how it looks.

use gpui::prelude::*;
use gpui::{AnyElement, ElementId, IntoElement, SharedString, Window, div, px};

use crate::theme::Palette;
use crate::ui;

/// One row of a menu.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MenuItem {
    /// A command. `shortcut` is shown right-aligned, e.g. `"Ctrl+U"`.
    Action {
        id: &'static str,
        icon: Option<&'static str>,
        label: String,
        shortcut: Option<String>,
        enabled: bool,
        danger: bool,
        /// Drawn with a highlight, for the most likely choice.
        prominent: bool,
    },
    /// An independent on/off toggle, shown with a tick.
    Check {
        id: &'static str,
        icon: &'static str,
        label: String,
        checked: bool,
    },
    /// A non-interactive section title.
    Heading(String),
    Separator,
}

impl MenuItem {
    /// A plain command with no icon, shortcut or styling.
    pub fn action(id: &'static str, label: impl Into<String>) -> Self {
        MenuItem::Action {
            id,
            icon: None,
            label: label.into(),
            shortcut: None,
            enabled: true,
            danger: false,
            prominent: false,
        }
    }

    /// A command with a leading icon.
    pub fn with_icon(
        id: &'static str,
        icon: &'static str,
        label: impl Into<String>,
        shortcut: Option<&str>,
    ) -> Self {
        MenuItem::Action {
            id,
            icon: Some(icon),
            label: label.into(),
            shortcut: shortcut.map(str::to_string),
            enabled: true,
            danger: false,
            prominent: false,
        }
    }

    /// The same command, disabled because its precondition is unmet.
    pub fn disabled(
        id: &'static str,
        icon: &'static str,
        label: impl Into<String>,
        shortcut: Option<&str>,
    ) -> Self {
        match MenuItem::with_icon(id, icon, label, shortcut) {
            MenuItem::Action {
                id,
                icon,
                label,
                shortcut,
                ..
            } => MenuItem::Action {
                id,
                icon,
                label,
                shortcut,
                enabled: false,
                danger: false,
                prominent: false,
            },
            other => other,
        }
    }

    /// The same command, styled as destructive.
    pub fn danger(id: &'static str, icon: &'static str, label: impl Into<String>) -> Self {
        MenuItem::Action {
            id,
            icon: Some(icon),
            label: label.into(),
            shortcut: None,
            enabled: true,
            danger: true,
            prominent: false,
        }
    }

    /// Set the shortcut hint on a command.
    pub fn shortcut(mut self, hint: &str) -> Self {
        if let MenuItem::Action { shortcut, .. } = &mut self {
            *shortcut = Some(hint.to_string());
        }
        self
    }

    /// The id this item reports when chosen.
    #[cfg(test)]
    pub fn id(&self) -> Option<&'static str> {
        match self {
            MenuItem::Action { id, .. } | MenuItem::Check { id, .. } => Some(id),
            MenuItem::Heading(_) | MenuItem::Separator => None,
        }
    }

    /// Whether the item can be chosen.
    #[cfg(test)]
    pub fn is_enabled(&self) -> bool {
        match self {
            MenuItem::Action { enabled, .. } => *enabled,
            MenuItem::Check { .. } => true,
            _ => false,
        }
    }
}

/// Render a menu into a card of the given minimum width.
///
/// `on_select` is called with the chosen item's id. Callers close the menu
/// themselves, which keeps dismissal in one place.
pub fn render<F>(t: &Palette, items: &[MenuItem], min_width: f32, on_select: F) -> AnyElement
where
    // `Clone` rather than `Copy` so each row can own its own handle to the same
    // closure; the alternative is boxing, which callers would have to unwind.
    F: Fn(&'static str, &mut Window, &mut gpui::App) + Clone + 'static,
{
    let mut rows: Vec<AnyElement> = Vec::with_capacity(items.len());

    for item in items {
        rows.push(match item {
            MenuItem::Separator => ui::menu_separator(t).into_any_element(),
            MenuItem::Heading(text) => div()
                .px(px(12.0))
                .pt(px(8.0))
                .pb(px(2.0))
                .text_size(px(10.0))
                .font_weight(gpui::FontWeight::BOLD)
                .text_color(t.text_muted)
                .child(text.clone())
                .into_any_element(),
            MenuItem::Check {
                id,
                icon,
                label,
                checked,
            } => {
                let on_select = on_select.clone();
                let id = *id;
                ui::menu_check(
                    t,
                    ElementId::from(id),
                    icon,
                    label,
                    *checked,
                    move |_, w, cx| on_select(id, w, cx),
                )
                .into_any_element()
            }
            MenuItem::Action {
                id,
                icon,
                label,
                shortcut,
                enabled,
                danger,
                prominent,
            } => {
                let on_select = on_select.clone();
                let id = *id;
                let row = if *enabled {
                    ui::menu_row(
                        t,
                        ElementId::from(id),
                        *icon,
                        label,
                        *danger,
                        move |_, w, cx| on_select(id, w, cx),
                    )
                } else {
                    // Same chrome and same width, but inert and dimmed, so a
                    // disabled row occupies exactly the space an enabled one
                    // would and cannot be clicked.
                    ui::menu_row(
                        t,
                        concat_id(id, "-off"),
                        *icon,
                        label,
                        *danger,
                        |_, _, _| {},
                    )
                    .opacity(0.35)
                    .cursor_not_allowed()
                };
                row.w(px(min_width))
                    .when(*prominent, |d| d.bg(t.hover))
                    .when_some(shortcut.clone(), |d, hint: String| {
                        // A hairline leader pushes the hint to the right edge,
                        // the way a native menu lays out accelerator keys.
                        d.child(
                            div()
                                .flex_1()
                                .min_w(px(8.0))
                                .mx(px(6.0))
                                .h(px(1.0))
                                .bg(t.border_soft),
                        )
                        .child(ui::shortcut_label(t, &hint, false))
                    })
                    .into_any_element()
            }
        });
    }

    ui::surface(t, rows)
        .min_w(px(min_width))
        .py(px(6.0))
        .into_any_element()
}

/// Build a distinct id for the inert copy of a disabled row, so it does not
/// collide with the real item's element id.
fn concat_id(id: &'static str, suffix: &str) -> ElementId {
    ElementId::from(SharedString::from(format!("{id}{suffix}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builders_set_the_fields_they_name() {
        let plain = MenuItem::action("refresh", "Refresh");
        assert_eq!(plain.id(), Some("refresh"));
        assert!(plain.is_enabled());
        assert!(matches!(
            plain,
            MenuItem::Action {
                icon: None,
                shortcut: None,
                danger: false,
                prominent: false,
                ..
            }
        ));

        let shortcut =
            MenuItem::with_icon("upload", "send-to", "Upload", Some("Ctrl+U")).shortcut("Ctrl+U");
        assert!(matches!(
            shortcut,
            MenuItem::Action {
                icon: Some("send-to"),
                ..
            }
        ));
        assert_eq!(shortcut.id(), Some("upload"));

        let off = MenuItem::disabled("apk", "system-software-install", "Install APK", None);
        assert!(!off.is_enabled(), "a disabled item must not be selectable");

        let del = MenuItem::danger("delete", "edit-delete", "Delete");
        assert!(del.is_enabled());
        assert!(matches!(del, MenuItem::Action { danger: true, .. }));
    }

    #[test]
    fn decorative_items_have_no_id() {
        assert_eq!(MenuItem::Separator.id(), None);
        assert_eq!(MenuItem::Heading("Group".into()).id(), None);
        assert!(!MenuItem::Separator.is_enabled());
    }

    #[test]
    fn disabled_ids_do_not_collide_with_enabled_ones() {
        let on = ElementId::from("apk");
        let off = concat_id("apk", "-off");
        assert_ne!(format!("{on:?}"), format!("{off:?}"));
    }
}

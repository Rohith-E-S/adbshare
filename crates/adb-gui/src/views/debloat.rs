//! The debloat view: the phone's packages, what the curated list says about
//! them, and the four ways a package's state can be changed.
//!
//! The view is a pane in its own right, not a dialog: a phone has several
//! hundred packages on it, and the useful view is a filtered list of the ones
//! the curated list knows about, with the ones this phone's brand put there
//! at the top.
//!
//! The view does not change anything itself. It collects a selection and
//! asks the app to act on it, so the confirmation card, the work on the
//! daemon and the ledger all live in one place.

use gpui::prelude::*;
use gpui::{
    AnyElement, Entity, EventEmitter, Styled, UniformListScrollHandle, Window, div, px,
    uniform_list,
};

use crate::icons::{self, names};
use crate::protocol::{PackageActionDto, PackageDto, PackageRequestDto};
use crate::state::daemon;
use crate::state::debloat::{Counts, DebloatList, Removal, Vendor, vendor_of};
use crate::theme::{self, Palette, Themed};
use crate::views::textinput::TextField;
use crate::views::ui;

/// Height of one row in the list.
const ROW_H: f32 = 46.0;

/// What the view is showing about a package's state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StateFilter {
    /// Everything.
    All,
    /// The app is on the phone and can be used.
    Installed,
    /// The app is on the phone but switched off.
    Disabled,
    /// The app is not on the phone for this user.
    Uninstalled,
}

impl StateFilter {
    pub fn label(self) -> &'static str {
        match self {
            StateFilter::All => "All states",
            StateFilter::Installed => "Installed",
            StateFilter::Disabled => "Disabled",
            StateFilter::Uninstalled => "Not installed",
        }
    }

    /// The next one, for a button that cycles through them.
    pub fn next(self) -> StateFilter {
        match self {
            StateFilter::All => StateFilter::Installed,
            StateFilter::Installed => StateFilter::Disabled,
            StateFilter::Disabled => StateFilter::Uninstalled,
            StateFilter::Uninstalled => StateFilter::All,
        }
    }
}

/// What the view is filtering by.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Filters {
    /// None means every vendor.
    pub vendor: Option<Vendor>,
    /// None means every risk tier.
    pub removal: Option<Removal>,
    pub state: StateFilter,
    /// Whether packages the curated list does not know about are shown.
    pub show_unlisted: bool,
}

impl Default for Filters {
    fn default() -> Self {
        Self {
            vendor: None,
            removal: None,
            state: StateFilter::All,
            show_unlisted: false,
        }
    }
}

/// One package in the list, with the curated list joined in.
#[derive(Debug, Clone)]
pub struct Row {
    pub id: String,
    /// `enabled` | `disabled` | `uninstalled`.
    pub state: String,
    /// True when the package lives on the system's read-only partition.
    pub system: bool,
    /// The curated list's description of the app.
    pub description: Option<String>,
    pub vendor: Vendor,
    /// The curated list's risk assessment, when it has one.
    pub removal: Option<Removal>,
    /// True when the curated list knows about this package.
    pub listed: bool,
    /// Packages that have to go with this one.
    pub dependencies: Vec<String>,
    /// Packages that stop working if this one goes.
    pub needed_by: Vec<String>,
}

impl Row {
    fn is_enabled(&self) -> bool {
        self.state == "enabled"
    }
    fn is_disabled(&self) -> bool {
        self.state == "disabled"
    }
    fn is_uninstalled(&self) -> bool {
        self.state == "uninstalled"
    }
}

/// What the view asks the app to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DebloatEvent {
    /// Change the state of these packages.
    Apply {
        action: PackageActionDto,
        packages: Vec<String>,
    },
    /// Put back everything the ledger says was changed on this phone.
    Undo,
}

/// The debloat view.
pub struct Debloat {
    /// The phone being looked at.
    device: Option<String>,
    /// The phone's friendly name, for the title.
    device_label: String,
    /// The Android user the changes apply to.
    user: u16,
    /// Everything the phone reported, joined with the curated list.
    rows: Vec<Row>,
    /// The rows the filters leave visible.
    visible: Vec<usize>,
    /// The packages the user has picked.
    selected: std::collections::BTreeSet<String>,
    filters: Filters,
    /// The vendors to offer in the filter, in the order they should appear.
    vendors: Vec<Vendor>,
    /// A search over the package name and the description.
    query: String,
    loading: bool,
    error: Option<String>,
    /// A message about the last thing that happened.
    status: Option<String>,
    scroll: UniformListScrollHandle,
    /// The search box in the header.
    search: Entity<TextField>,
}

impl Debloat {
    pub fn new(cx: &mut Context<Self>) -> Self {
        Self {
            device: None,
            device_label: String::new(),
            user: 0,
            rows: Vec::new(),
            visible: Vec::new(),
            selected: std::collections::BTreeSet::new(),
            filters: Filters::default(),
            vendors: Vec::new(),
            query: String::new(),
            loading: false,
            error: None,
            status: None,
            scroll: UniformListScrollHandle::default(),
            search: cx.new(|cx| TextField::new(cx, "Search packages")),
        }
    }

    /// The search box, so the app can watch it and hand over the caret.
    pub fn search_field(&self) -> &Entity<TextField> {
        &self.search
    }

    /// The rows the filters leave visible.
    fn visible_rows(&self) -> Vec<&Row> {
        self.visible
            .iter()
            .filter_map(|ix| self.rows.get(*ix))
            .collect()
    }

    /// The packages the user has picked.
    pub fn selection(&self) -> Vec<String> {
        self.selected.iter().cloned().collect()
    }

    /// The rows for these package ids, for a confirmation card.
    pub fn rows_for(&self, ids: &[String]) -> Vec<&Row> {
        ids.iter()
            .filter_map(|id| self.rows.iter().find(|row| &row.id == id))
            .collect()
    }

    /// The state each of these packages is in now.
    pub fn states_for(&self, ids: &[String]) -> Vec<(String, String)> {
        ids.iter()
            .filter_map(|id| {
                self.rows
                    .iter()
                    .find(|row| &row.id == id)
                    .map(|row| (row.id.clone(), row.state.clone()))
            })
            .collect()
    }

    /// Note that some packages are in a new state, so the list does not have
    /// to be read from the phone again to show what happened.
    pub fn note_states(&mut self, states: &[(String, String)], cx: &mut Context<Self>) {
        for (id, state) in states {
            if let Some(row) = self.rows.iter_mut().find(|row| &row.id == id) {
                row.state = state.clone();
            }
        }
        self.recompute();
        cx.notify();
    }

    /// The packages that would be left broken by removing these packages.
    ///
    /// The curated list records the dependency graph from both sides: a
    /// package names what it needs in `dependencies`, and names what needs it
    /// in `needed_by`. Removing a package breaks the things that depend on
    /// it, so both directions have to be followed to find them.
    pub fn broken_by_removing(&self, ids: &[String]) -> Vec<String> {
        broken_by_removing(&self.rows, ids)
    }

    /// Look at a phone, and read its packages.
    pub fn set_device(&mut self, device: Option<String>, cx: &mut Context<Self>) {
        if self.device == device {
            return;
        }
        let has_device = device.is_some();
        self.device = device;
        self.rows.clear();
        self.visible.clear();
        self.selected.clear();
        self.error = None;
        self.status = None;
        if has_device {
            self.reload(cx);
        }
    }

    /// The phone's friendly name, for the title of the view.
    pub fn set_device_label(&mut self, label: String, cx: &mut Context<Self>) {
        if self.device_label == label {
            return;
        }
        self.device_label = label;
        cx.notify();
    }

    /// Read the phone's packages again.
    pub fn reload(&mut self, cx: &mut Context<Self>) {
        let Some(device) = self.device.clone() else {
            return;
        };
        self.loading = true;
        self.error = None;
        cx.notify();

        let user = self.user;
        let me = cx.entity();
        cx.spawn(async move |_this, cx| {
            let result = daemon::list_packages(&device, user).await;
            me.update(cx, |this, cx| {
                this.loading = false;
                match result {
                    Ok(packages) => {
                        this.rows = join(&packages);
                        this.vendors = collect_vendors(&this.rows);
                        this.recompute();
                        cx.notify();
                    }
                    Err(err) => {
                        this.error = Some(err);
                        cx.notify();
                    }
                }
            })
            .ok();
        })
        .detach();
    }

    /// Apply a search term.
    pub fn set_query(&mut self, query: String, cx: &mut Context<Self>) {
        self.query = query;
        self.recompute();
        cx.notify();
    }

    /// Change one of the filters.
    pub fn set_filters(&mut self, filters: Filters, cx: &mut Context<Self>) {
        self.filters = filters;
        self.recompute();
        cx.notify();
    }

    /// Pick a package, or put it back.
    pub fn toggle(&mut self, id: &str, cx: &mut Context<Self>) {
        if !self.selected.remove(id) {
            self.selected.insert(id.to_string());
        }
        cx.notify();
    }

    /// Pick every visible package, or none of them.
    pub fn toggle_all(&mut self, cx: &mut Context<Self>) {
        let ids: Vec<String> = self
            .visible_rows()
            .iter()
            .map(|row| row.id.clone())
            .collect();
        let all_selected = !ids.is_empty() && ids.iter().all(|id| self.selected.contains(id));
        if all_selected {
            for id in ids {
                self.selected.remove(&id);
            }
        } else {
            for id in ids {
                self.selected.insert(id);
            }
        }
        cx.notify();
    }

    /// Clear the selection.
    pub fn clear_selection(&mut self, cx: &mut Context<Self>) {
        self.selected.clear();
        cx.notify();
    }

    /// Say something about what just happened.
    pub fn set_status(&mut self, status: Option<String>, cx: &mut Context<Self>) {
        self.status = status;
        cx.notify();
    }

    /// The actions that make sense for the current selection.
    ///
    /// Only the ones that would do something are offered: disabling a
    /// package that is already disabled is noise, and so is restoring one
    /// that was never removed.
    pub fn applicable_actions(&self) -> Vec<PackageActionDto> {
        let picked: Vec<&Row> = self
            .selected
            .iter()
            .filter_map(|id| self.rows.iter().find(|row| &row.id == id))
            .collect();
        if picked.is_empty() {
            return Vec::new();
        }
        let mut actions = Vec::new();
        if picked.iter().any(|row| row.is_enabled()) {
            actions.push(PackageActionDto::Disable);
        }
        if picked.iter().any(|row| row.is_disabled()) {
            actions.push(PackageActionDto::Enable);
        }
        if picked
            .iter()
            .any(|row| row.is_enabled() || row.is_disabled())
        {
            actions.push(PackageActionDto::Uninstall);
        }
        if picked.iter().any(|row| row.is_uninstalled()) {
            actions.push(PackageActionDto::Restore);
        }
        actions
    }

    /// The rows the filters leave visible, in order.
    fn recompute(&mut self) {
        let needle = self.query.to_lowercase();
        let filters = self.filters;
        self.visible = self
            .rows
            .iter()
            .enumerate()
            .filter(|(_, row)| {
                if !filters.show_unlisted && !row.listed {
                    return false;
                }
                if let Some(vendor) = filters.vendor
                    && row.vendor != vendor
                {
                    return false;
                }
                if let Some(removal) = filters.removal
                    && row.removal != Some(removal)
                {
                    return false;
                }
                match filters.state {
                    StateFilter::All => true,
                    StateFilter::Installed => row.is_enabled(),
                    StateFilter::Disabled => row.is_disabled(),
                    StateFilter::Uninstalled => row.is_uninstalled(),
                }
            })
            .filter(|(_, row)| {
                needle.is_empty()
                    || row.id.to_lowercase().contains(&needle)
                    || row
                        .description
                        .as_deref()
                        .is_some_and(|d| d.to_lowercase().contains(&needle))
            })
            .map(|(ix, _)| ix)
            .collect();
    }
}

/// The packages that would be left broken by removing these packages.
///
/// The curated list records the dependency graph from both sides: a package
/// names what it needs in `dependencies`, and names what needs it in
/// `needed_by`. Removing a package breaks the things that depend on it, so
/// both directions have to be followed to find them.
///
/// A package that is in the selection does not count: it is going too.
pub fn broken_by_removing(rows: &[Row], ids: &[String]) -> Vec<String> {
    let chosen: std::collections::HashSet<&str> = ids.iter().map(|id| id.as_str()).collect();
    let on_phone: std::collections::HashSet<&str> =
        rows.iter().map(|row| row.id.as_str()).collect();
    let mut broken = std::collections::BTreeSet::new();
    for row in rows {
        if !chosen.contains(row.id.as_str()) {
            continue;
        }
        for dependent in row.dependencies.iter().chain(row.needed_by.iter()) {
            if !chosen.contains(dependent.as_str()) && on_phone.contains(dependent.as_str()) {
                broken.insert(dependent.clone());
            }
        }
    }
    broken.into_iter().collect()
}

/// Join the device's packages with the curated list.
fn join(packages: &[PackageDto]) -> Vec<Row> {
    let list = DebloatList::bundled();
    let mut rows = Vec::with_capacity(packages.len());
    for package in packages {
        let entry = list.get(&package.id);
        let (description, removal, dependencies, needed_by, listed) = match entry {
            Some(entry) => (
                Some(entry.description.clone()),
                Some(entry.removal),
                entry.dependencies.clone(),
                entry.needed_by.clone(),
                true,
            ),
            None => (None, None, Vec::new(), Vec::new(), false),
        };
        let vendor = vendor_of(&package.id, list_kind(list, &package.id));
        rows.push(Row {
            id: package.id.clone(),
            state: package.state.clone(),
            system: package.system,
            description,
            vendor,
            removal,
            listed,
            dependencies,
            needed_by,
        });
    }
    rows.sort_by(|a, b| {
        // The packages the curated list knows about come first, then the
        // safest to remove, then the vendor's own, then the name.
        b.listed
            .cmp(&a.listed)
            .then_with(|| a.removal.cmp(&b.removal))
            .then_with(|| a.vendor.cmp(&b.vendor))
            .then_with(|| a.id.cmp(&b.id))
    });
    rows
}

/// The curated list a package belongs to.
fn list_kind(list: &DebloatList, id: &str) -> crate::state::debloat::ListKind {
    list.get(id)
        .map(|entry| entry.list)
        .unwrap_or(crate::state::debloat::ListKind::Misc)
}

/// The vendors that appear in the rows, in the order they should be offered.
fn collect_vendors(rows: &[Row]) -> Vec<Vendor> {
    let mut vendors: Vec<Vendor> = rows.iter().map(|row| row.vendor).collect();
    vendors.sort_unstable();
    vendors.dedup();
    vendors
}

/// The requests that carry out an action on a selection.
pub fn requests(action: PackageActionDto, packages: &[String]) -> Vec<PackageRequestDto> {
    packages
        .iter()
        .map(|package| PackageRequestDto {
            package: package.clone(),
            action,
        })
        .collect()
}

// ── Rendering ────────────────────────────────────────────────────────────────

impl EventEmitter<DebloatEvent> for Debloat {}

impl Render for Debloat {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = *cx.theme();
        let title = self.title(&t);
        let header = self.header(&t, cx);
        let body = self.body(&t, cx);
        let footer = self.footer(&t, cx);

        div()
            .flex()
            .flex_col()
            .size_full()
            .bg(t.canvas)
            .child(title)
            .child(header)
            .child(body)
            .child(footer)
            .into_any_element()
    }
}

impl Debloat {
    /// The title of the view: what it is, and what it found.
    ///
    /// A pane that opens without saying what it is leaves the reader
    /// guessing, and a pane that opens without saying what it found leaves
    /// them with nothing to act on.
    fn title(&self, t: &Palette) -> AnyElement {
        let counts = self.counts();
        let subtitle = if counts.listed == 0 {
            format!(
                "{} package(s) on the phone; the debloat list knows none of them.",
                counts.total
            )
        } else {
            format!(
                "{} of {} package(s) on the phone are in the debloat list · {} safe to remove",
                counts.listed, counts.total, counts.recommended
            )
        };

        div()
            .flex()
            .items_center()
            .gap(px(10.0))
            .px(px(14.0))
            .pt(px(12.0))
            .pb(px(10.0))
            .child(icons::icon(
                names::PACKAGE,
                18.0,
                ui::icon_tint(names::PACKAGE, t),
            ))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(1.0))
                    .min_w_0()
                    .child(
                        div()
                            .text_size(t.text(15.0))
                            .text_color(t.text_header)
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .child(match self.device_label.as_str() {
                                label if !label.is_empty() => format!("Debloat {label}"),
                                _ => "Debloat".to_string(),
                            }),
                    )
                    .child(
                        div()
                            .text_size(t.text(11.5))
                            .text_color(t.text_muted)
                            .child(subtitle),
                    ),
            )
            .into_any_element()
    }

    /// What the list says about this phone, in numbers.
    fn counts(&self) -> Counts {
        Counts {
            total: self.rows.len(),
            listed: self.rows.iter().filter(|row| row.listed).count(),
            recommended: self
                .rows
                .iter()
                .filter(|row| row.removal == Some(Removal::Recommended))
                .count(),
        }
    }

    /// The strip above the list: the filters and the search.
    fn header(&mut self, t: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let me = cx.entity();
        let vendor_label = match self.filters.vendor {
            Some(vendor) => vendor.label().to_string(),
            None => "All vendors".to_string(),
        };
        let removal_label = match self.filters.removal {
            Some(removal) => removal.label().to_string(),
            None => "All risks".to_string(),
        };

        let vendor_cycle = {
            let me = me.clone();
            let vendors = self.vendors.clone();
            move |_e: &gpui::ClickEvent, _w: &mut Window, app: &mut gpui::App| {
                me.update(app, |this, cx| {
                    let next = match this.filters.vendor {
                        None => vendors.first().copied(),
                        Some(current) => {
                            let at = vendors.iter().position(|v| *v == current);
                            match at {
                                Some(at) if at + 1 < vendors.len() => Some(vendors[at + 1]),
                                Some(_) => None,
                                None => vendors.first().copied(),
                            }
                        }
                    };
                    this.set_filters(
                        Filters {
                            vendor: next,
                            ..this.filters
                        },
                        cx,
                    );
                });
            }
        };

        let removal_cycle = {
            let me = me.clone();
            move |_e: &gpui::ClickEvent, _w: &mut Window, app: &mut gpui::App| {
                me.update(app, |this, cx| {
                    let next = match this.filters.removal {
                        None => Some(Removal::Recommended),
                        Some(Removal::Recommended) => Some(Removal::Advanced),
                        Some(Removal::Advanced) => Some(Removal::Expert),
                        Some(Removal::Expert) => Some(Removal::Unsafe),
                        Some(Removal::Unsafe) => None,
                    };
                    this.set_filters(
                        Filters {
                            removal: next,
                            ..this.filters
                        },
                        cx,
                    );
                });
            }
        };

        let state_cycle = {
            let me = me.clone();
            move |_e: &gpui::ClickEvent, _w: &mut Window, app: &mut gpui::App| {
                me.update(app, |this, cx| {
                    this.set_filters(
                        Filters {
                            state: this.filters.state.next(),
                            ..this.filters
                        },
                        cx,
                    );
                });
            }
        };

        let unlisted_cycle = {
            let me = me.clone();
            move |_e: &gpui::ClickEvent, _w: &mut Window, app: &mut gpui::App| {
                me.update(app, |this, cx| {
                    this.set_filters(
                        Filters {
                            show_unlisted: !this.filters.show_unlisted,
                            ..this.filters
                        },
                        cx,
                    );
                });
            }
        };

        let refresh = {
            let me = me.clone();
            move |_e: &gpui::ClickEvent, _w: &mut Window, app: &mut gpui::App| {
                me.update(app, |this, cx| this.reload(cx));
            }
        };

        let select_all = {
            let me = me.clone();
            move |_e: &gpui::ClickEvent, _w: &mut Window, app: &mut gpui::App| {
                me.update(app, |this, cx| this.toggle_all(cx));
            }
        };

        div()
            .id("debloat-filters")
            .flex()
            .items_center()
            .gap(px(8.0))
            .px(px(14.0))
            .py(px(8.0))
            .border_b_1()
            .border_color(t.border_soft)
            // A narrow pane scrolls the strip rather than pushing the search
            // box off the edge of the window.
            .overflow_x_scroll()
            .child(ui::button(
                t,
                "debloat-vendor",
                vendor_label,
                false,
                vendor_cycle,
            ))
            .child(ui::button(
                t,
                "debloat-risk",
                removal_label,
                false,
                removal_cycle,
            ))
            .child(ui::button(
                t,
                "debloat-state",
                self.filters.state.label(),
                false,
                state_cycle,
            ))
            .child(ui::button(
                t,
                "debloat-unlisted",
                if self.filters.show_unlisted {
                    "All"
                } else {
                    "Known"
                },
                false,
                unlisted_cycle,
            ))
            .child(div().flex_1())
            .child(ui::button(
                t,
                "debloat-select-all",
                "Select all",
                false,
                select_all,
            ))
            .child(icons::icon(names::EDIT_FIND, 14.0, t.text_muted))
            // The field is given a width of its own: it has no natural
            // maximum, so without one it takes the room it can get and
            // pushes the rest of the row off the edge.
            .child(div().w(px(200.0)).child(self.search.clone()))
            .child(ui::icon_button(
                t,
                "debloat-refresh",
                names::REFRESH,
                16.0,
                t.text_muted,
                refresh,
            ))
            .into_any_element()
    }

    /// The list itself.
    fn body(&mut self, t: &Palette, cx: &mut Context<Self>) -> AnyElement {
        if let Some(error) = &self.error {
            return div()
                .flex()
                .flex_1()
                .items_center()
                .justify_center()
                .text_size(t.text(13.0))
                .text_color(t.danger)
                .child(error.clone())
                .into_any_element();
        }
        if self.loading && self.rows.is_empty() {
            return div()
                .flex()
                .flex_1()
                .items_center()
                .justify_center()
                .text_size(t.text(13.0))
                .text_color(t.text_muted)
                .child("Reading the phone's packages…")
                .into_any_element();
        }
        if self.rows.is_empty() {
            return div()
                .flex()
                .flex_1()
                .items_center()
                .justify_center()
                .flex_col()
                .gap(px(8.0))
                .child(
                    div()
                        .text_size(t.text(14.0))
                        .text_color(t.text_secondary)
                        .child("No packages to show"),
                )
                .child(
                    div()
                        .text_size(t.text(12.0))
                        .text_color(t.text_muted)
                        .child("Pick a phone in the sidebar to see what is on it."),
                )
                .into_any_element();
        }

        let count = self.visible.len();
        if count == 0 {
            return div()
                .flex()
                .flex_1()
                .items_center()
                .justify_center()
                .flex_col()
                .gap(px(8.0))
                .child(
                    div()
                        .text_size(t.text(14.0))
                        .text_color(t.text_secondary)
                        .child("Nothing matches the filters"),
                )
                .child(
                    div()
                        .text_size(t.text(12.0))
                        .text_color(t.text_muted)
                        .child(format!(
                            "{} package(s) on the phone are not shown. The list knows {} \
                             packages.",
                            self.rows.len(),
                            DebloatList::bundled().len()
                        )),
                )
                .into_any_element();
        }

        // The processor already hands over the view's own state; reaching for
        // the entity from inside it would re-enter an update that is already
        // in progress, which GPUI treats as a double lease and panics on.
        let palette = *t;
        uniform_list(
            "debloat-list",
            count,
            cx.processor(move |this, range: std::ops::Range<usize>, _w, cx| {
                range
                    .filter_map(|slot| this.row_element(slot, &palette, cx))
                    .collect::<Vec<_>>()
            }),
        )
        .flex_1()
        .min_h_0()
        .track_scroll(self.scroll.clone())
        .into_any_element()
    }

    /// One row of the list.
    fn row_element(&self, slot: usize, t: &Palette, cx: &mut Context<Self>) -> Option<AnyElement> {
        let index = *self.visible.get(slot)?;
        let row = self.rows.get(index)?;
        let selected = self.selected.contains(&row.id);
        let me = cx.entity();
        let id = row.id.clone();

        let resting_bg = if selected { Some(t.selected) } else { None };
        let hover_bg = if selected { t.selected_strong } else { t.hover };

        // The risk pill. The tiers are the curated list's own assessment, so
        // the colours follow the palette's warning and danger steps.
        let risk = row.removal.map(|removal| {
            let (fg, bg) = match removal {
                Removal::Recommended => ui::tone_colors(t, crate::protocol::StateTone::Success),
                Removal::Advanced => ui::tone_colors(t, crate::protocol::StateTone::Active),
                Removal::Expert => ui::tone_colors(t, crate::protocol::StateTone::Warning),
                Removal::Unsafe => ui::tone_colors(t, crate::protocol::StateTone::Danger),
            };
            ui::pill(t, removal.label(), fg, bg)
        });

        let state_colour = match row.state.as_str() {
            "enabled" => t.text_dim,
            "disabled" => t.warning,
            _ => t.text_muted,
        };

        let description = row
            .description
            .as_deref()
            .map(first_line)
            .unwrap_or_else(|| "Not in the debloat list".to_string());

        Some(
            div()
                .id(("debloat-row", index))
                .flex()
                .items_center()
                .gap(px(10.0))
                .w_full()
                .h(px(ROW_H))
                .mx(px(10.0))
                .px(px(8.0))
                .rounded(px(6.0))
                .cursor_pointer()
                .when_some(resting_bg, |d, bg| d.bg(bg))
                .hover(move |d| d.bg(hover_bg))
                .on_click(move |_e, _w, app| {
                    me.update(app, |this, cx| this.toggle(&id, cx));
                })
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .flex_1()
                        .min_w_0()
                        .gap(px(1.0))
                        .child(
                            div()
                                .text_size(t.text(12.0))
                                .text_color(t.text_primary)
                                .font_family(theme::MONO)
                                .truncate()
                                .child(row.id.clone()),
                        )
                        .child(
                            div()
                                .text_size(t.text(11.0))
                                .text_color(t.text_muted)
                                .truncate()
                                .child(description),
                        ),
                )
                .child(
                    div()
                        .flex_shrink_0()
                        .w(px(78.0))
                        .text_size(t.text(11.0))
                        .text_color(t.text_dim)
                        .child(row.vendor.label()),
                )
                .when_some(risk, |d, pill| d.child(div().flex_shrink_0().child(pill)))
                .child(
                    div()
                        .flex_shrink_0()
                        .w(px(8.0))
                        .justify_center()
                        .child(ui::led(state_colour)),
                )
                .into_any_element(),
        )
    }

    /// The strip below the list: what is picked, and what can be done.
    fn footer(&mut self, t: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let me = cx.entity();
        let picked = self.selected.len();
        let total = self.visible.len();
        let actions = self.applicable_actions();

        let mut buttons: Vec<AnyElement> = Vec::new();
        for action in actions {
            let me = me.clone();
            let packages = self.selection();
            let primary = matches!(
                action,
                PackageActionDto::Disable | PackageActionDto::Enable | PackageActionDto::Restore
            );
            buttons.push(
                ui::button(
                    t,
                    action_id(action),
                    action.label(),
                    primary,
                    move |_e, _w, app| {
                        me.update(app, |this, cx| {
                            cx.emit(DebloatEvent::Apply {
                                action,
                                packages: packages.clone(),
                            });
                            this.clear_selection(cx);
                        });
                    },
                )
                .into_any_element(),
            );
        }

        let undo = {
            let me = me.clone();
            move |_e: &gpui::ClickEvent, _w: &mut Window, app: &mut gpui::App| {
                me.update(app, |this, cx| {
                    cx.emit(DebloatEvent::Undo);
                    let _ = this;
                });
            }
        };

        div()
            .flex()
            .items_center()
            .gap(px(10.0))
            .h(px(theme::STATUSBAR_H))
            .px(px(14.0))
            .bg(t.statusbar)
            .border_t_1()
            .border_color(t.divider)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_size(t.text(11.5))
                    .text_color(t.text_dim)
                    .child(match &self.status {
                        Some(status) => status.clone(),
                        None if picked > 0 => {
                            format!("{picked} selected, of {total} shown")
                        }
                        None => format!("{total} package(s) shown"),
                    }),
            )
            .child(ui::icon_button(
                t,
                "debloat-undo",
                names::EDIT_UNDO,
                16.0,
                t.text_muted,
                undo,
            ))
            .children(buttons)
            .into_any_element()
    }
}

/// The first line of a description.
///
/// The curated list writes its descriptions as a title and then an
/// explanation on the next line. A row has room for one line, and a
/// two-line description would run into the row below it.
fn first_line(text: &str) -> String {
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default()
        .to_string()
}

/// The element id of an action's button.
fn action_id(action: PackageActionDto) -> &'static str {
    match action {
        PackageActionDto::Disable => "debloat-disable",
        PackageActionDto::Enable => "debloat-enable",
        PackageActionDto::Uninstall => "debloat-uninstall",
        PackageActionDto::Restore => "debloat-restore",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::debloat::ListKind;

    fn package(id: &str, state: &str) -> PackageDto {
        PackageDto {
            id: id.to_string(),
            state: state.to_string(),
            system: true,
            apk_path: None,
        }
    }

    #[test]
    fn a_known_package_gets_its_description_and_risk() {
        let rows = join(&[package("com.samsung.android.bixby.service", "enabled")]);
        assert_eq!(rows.len(), 1);
        assert!(rows[0].listed);
        assert_eq!(rows[0].vendor, Vendor::Samsung);
        assert_eq!(rows[0].removal, Some(Removal::Recommended));
        assert!(rows[0].description.is_some());
    }

    #[test]
    fn an_unknown_package_is_still_listed_but_flagged() {
        let rows = join(&[package("com.example.unknown", "enabled")]);
        assert!(!rows[0].listed);
        assert_eq!(rows[0].removal, None);
        assert_eq!(rows[0].vendor, Vendor::Other);
    }

    #[test]
    fn known_packages_come_first() {
        let rows = join(&[
            package("com.example.unknown", "enabled"),
            package("com.samsung.android.bixby.service", "enabled"),
        ]);
        assert!(rows[0].listed, "the known package sorts first");
    }

    #[test]
    fn the_vendor_follows_the_package_name() {
        let rows = join(&[
            package("com.miui.securitycenter", "enabled"),
            package("com.huawei.hwid", "enabled"),
        ]);
        let vendors: Vec<Vendor> = rows.iter().map(|r| r.vendor).collect();
        assert!(vendors.contains(&Vendor::Xiaomi));
        assert!(vendors.contains(&Vendor::Huawei));
    }

    #[test]
    fn the_state_filter_cycles() {
        assert_eq!(StateFilter::All.next(), StateFilter::Installed);
        assert_eq!(StateFilter::Installed.next(), StateFilter::Disabled);
        assert_eq!(StateFilter::Disabled.next(), StateFilter::Uninstalled);
        assert_eq!(StateFilter::Uninstalled.next(), StateFilter::All);
    }

    #[test]
    fn a_request_carries_the_action() {
        let requests = requests(
            PackageActionDto::Disable,
            &["com.example.one".to_string(), "com.example.two".to_string()],
        );
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].package, "com.example.one");
        assert_eq!(requests[0].action, PackageActionDto::Disable);
    }

    #[test]
    fn removing_a_package_says_which_ones_would_break() {
        let mut rows = join(&[
            package("com.huawei.hwid", "enabled"),
            package("com.huawei.fastapp", "enabled"),
        ]);
        // The curated list records the edge from both sides.
        for row in rows.iter_mut() {
            if row.id == "com.huawei.hwid" {
                row.needed_by = vec!["com.huawei.fastapp".into()];
            }
        }
        let broken = broken_by_removing(&rows, &["com.huawei.hwid".to_string()]);
        assert_eq!(broken, vec!["com.huawei.fastapp".to_string()]);

        // Taking both at once breaks nothing, because the one that needed it
        // is going too.
        let both = broken_by_removing(
            &rows,
            &[
                "com.huawei.hwid".to_string(),
                "com.huawei.fastapp".to_string(),
            ],
        );
        assert!(both.is_empty(), "{both:?}");
    }

    #[test]
    fn a_carrier_package_is_a_carriers() {
        // The curated list says so, and that has to beat the name.
        let list = DebloatList::bundled();
        let entry = list
            .get("com.nttdocomo.android.mymagazine")
            .expect("a carrier package in the list");
        assert_eq!(entry.list, ListKind::Carrier);
    }
}

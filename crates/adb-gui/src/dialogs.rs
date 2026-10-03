//! Modal dialogs.
//!
//! GPUI has no dialog, popover or overlay primitive, so a dialog here is a
//! backdrop plus a centred card drawn with [`ui::on_top`], which postpones its
//! paint until after the rest of the window. That gets correct layering without
//! a z-index concept.
//!
//! Dialogs carry no behaviour of their own: they render, collect input, and
//! return a [`DialogResult`] for the app to act on. Keeping the async work
//! (D-Bus, trash, portals) in one place also makes the validation in this module
//! directly unit-testable.

use std::path::{Path, PathBuf};

use gpui::prelude::*;
use gpui::{AnyElement, Entity, SharedString, Styled, Window, div, px, relative};

use crate::icons::{self, names};
use crate::protocol::{DiagnosticReportDto, DirEntry, format_diagnostics, human_size};
use crate::textinput::TextField;
use crate::theme::{self, Palette};
use crate::ui;

/// Width of an ordinary dialog card.
const CARD_W: f32 = 420.0;
/// Width of a dialog with a wide body, such as the diagnostics report.
const WIDE_W: f32 = 560.0;
/// The largest image preview edge, so a huge photo cannot fill the window.
const PREVIEW_MAX: f32 = 520.0;

/// Severity of a plain message dialog.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageTone {
    Info,
    Success,
    Warning,
    Error,
}

impl MessageTone {
    fn icon(self) -> &'static str {
        match self {
            MessageTone::Info => names::DIALOG_INFORMATION,
            MessageTone::Success => names::OBJECT_SELECT,
            MessageTone::Warning => names::DIALOG_WARNING,
            MessageTone::Error => names::DIALOG_ERROR,
        }
    }

    fn accent(self, t: &Palette) -> gpui::Rgba {
        match self {
            MessageTone::Info => t.text_dim,
            MessageTone::Success => t.success,
            MessageTone::Warning => t.warning,
            MessageTone::Error => t.danger,
        }
    }
}

/// How far the two-step wireless pairing flow has got.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WifiStep {
    /// Waiting for an `ip:port`.
    Address,
    /// Address accepted; waiting for the six-digit code shown on the phone.
    Pairing { address: String },
    /// `adb pair` succeeded; the user can now connect.
    Paired { address: String },
    /// A step failed; `message` says why.
    Failed { message: String },
}

/// The dialog currently on screen.
pub enum Dialog {
    /// No dialog. [`render`] returns `None` for this.
    None,
    NewFolder {
        field: Entity<TextField>,
    },
    Rename {
        entry: DirEntry,
        field: Entity<TextField>,
    },
    /// Confirm a destructive delete. `trash` is set for local paths, where the
    /// plain Delete key is recoverable from the desktop's trash.
    ConfirmDelete {
        label: String,
        entries: Vec<DirEntry>,
        trash: bool,
    },
    Properties {
        entry: DirEntry,
        full_path: PathBuf,
        device: String,
    },
    /// The two-field entities live here rather than in the app so the dialog can
    /// be rendered from a single value, and so opening a new pairing flow always
    /// starts with empty fields.
    ConnectWifi {
        step: WifiStep,
        address_field: Entity<TextField>,
        code_field: Entity<TextField>,
    },
    Diagnostics {
        report: DiagnosticReportDto,
    },
    /// A picked or dropped `.apk`: install it, or just copy it across.
    SideloadApk {
        name: String,
        local: PathBuf,
    },
    Message {
        title: String,
        body: String,
        tone: MessageTone,
        /// A second, quieter button, e.g. "Copy report" beside "Close".
        extra: Option<(String, DialogResult)>,
    },
    /// The keyboard shortcut reference.
    Shortcuts,
    /// A still image from a device, read through its FUSE mount.
    ImagePreview {
        name: String,
        local: PathBuf,
    },
    About,
}

impl Dialog {
    /// Whether Escape and a backdrop click should dismiss this dialog.
    ///
    /// A pairing already in flight must not be dismissed by a stray click, or
    /// the user loses track of an operation that is still running.
    pub fn is_dismissible(&self) -> bool {
        match self {
            Dialog::None => false,
            Dialog::ConnectWifi { step, .. } => step_is_dismissible(step),
            _ => true,
        }
    }

    /// The dialog title, also used as the accessible name and by tests.
    pub fn heading(&self) -> &str {
        match self {
            Dialog::None => "",
            Dialog::NewFolder { .. } => "New folder",
            Dialog::Rename { .. } => "Rename",
            Dialog::ConfirmDelete { .. } => "Delete",
            Dialog::Properties { .. } => "Properties",
            Dialog::ConnectWifi { step, .. } => step_heading(step),
            Dialog::Diagnostics { .. } => "Connection diagnostics",
            Dialog::SideloadApk { .. } => "Install this APK?",
            Dialog::Message { title, .. } => title,
            Dialog::ImagePreview { name, .. } => name,
            Dialog::Shortcuts => "Keyboard shortcuts",
            Dialog::About => "About ADBShare",
        }
    }
}

/// Whether a pairing step can be backed out of.
fn step_is_dismissible(step: &WifiStep) -> bool {
    !matches!(step, WifiStep::Pairing { .. })
}

/// The title for a pairing step.
fn step_heading(step: &WifiStep) -> &'static str {
    match step {
        WifiStep::Address => "Connect ADB via IP",
        WifiStep::Pairing { .. } => "Pair with this phone",
        WifiStep::Paired { .. } => "Paired",
        WifiStep::Failed { .. } => "Could not connect",
    }
}

/// What a dialog asks the app to do once the user has decided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DialogResult {
    Dismiss,
    /// Create a folder with this name in the directory being browsed.
    CreateFolder(String),
    /// Rename an entry to this name.
    Rename(DirEntry, String),
    /// Move to the local trash, which the desktop can restore.
    Trash(Vec<DirEntry>),
    /// Delete for good. On a device this is the only option.
    DeletePermanently(Vec<DirEntry>),
    /// Copy the rendered report to the clipboard.
    CopyDiagnostics(String),
    /// Step 1: pair with this address.
    PairAddress(String),
    /// Step 2: finish pairing with the code shown on the phone.
    PairCode {
        address: String,
        code: String,
    },
    /// The device is paired; connect over TCP.
    ConnectWireless(String),
    /// Open a path with the desktop's default handler.
    OpenExternal(PathBuf),
    /// Run `adb install` on the device.
    InstallApk(PathBuf),
    /// Copy the APK across without installing.
    CopyApk(PathBuf),
}

// ── Validation ───────────────────────────────────────────────────────────────

/// Reject a name a filesystem would not accept.
///
/// Android's `/sdcard` is a FAT-derived FUSE mount: no permission bits, and it
/// refuses the same characters a desktop filesystem does plus a few of its own.
/// Blocking them here means the daemon never sees a doomed request.
pub fn validate_name(name: &str) -> Result<String, String> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err("Please enter a name.".into());
    }
    if trimmed == "." || trimmed == ".." {
        return Err("That name is reserved.".into());
    }
    if trimmed.chars().count() > 255 {
        return Err("Names must be 255 characters or fewer.".into());
    }
    if let Some(bad) = trimmed.chars().find(|c| {
        matches!(
            c,
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' | '\0'
        )
    }) {
        return Err(format!("A name cannot contain “{bad}”."));
    }
    // A trailing dot survives trimming and breaks path lookups on FAT, so it
    // has to be rejected. A trailing *space* does not: the name is trimmed
    // first, which is both friendlier and satisfies the FAT rule.
    if trimmed.ends_with('.') {
        return Err("Names cannot end with a dot.".into());
    }
    Ok(trimmed.to_string())
}

/// Check a wireless pairing address and code before spending a D-Bus round trip.
///
/// `address` is `host:port`, where a bracketed IPv6 literal is allowed. The port
/// must be a real, non-zero TCP port.
pub fn validate_pair_input(address: &str, code: &str) -> Result<(), String> {
    if !address_is_well_formed(address) {
        return Err("Pairing address must look like 192.168.1.20:37001.".into());
    }
    if code.len() != 6 || !code.bytes().all(|b| b.is_ascii_digit()) {
        return Err("Pairing code must be the six digits shown on the phone.".into());
    }
    Ok(())
}

/// The host/port shape check from [`validate_pair_input`], split out so the
/// address step can be validated before a code has been typed.
pub fn address_is_well_formed(address: &str) -> bool {
    let Some((host, port)) = address.rsplit_once(':') else {
        return false;
    };
    if port.is_empty() || !port.bytes().all(|b| b.is_ascii_digit()) {
        return false;
    }
    let Ok(port) = port.parse::<u16>() else {
        return false;
    };
    if port == 0 {
        return false;
    }
    if host.is_empty() {
        return false;
    }

    // A bracketed literal is taken at face value, as long as it is bracketed on
    // both sides and has no stray brackets inside.
    if host.starts_with('[') || host.ends_with(']') {
        let Some(inner) = host.strip_prefix('[').and_then(|h| h.strip_suffix(']')) else {
            return false;
        };
        return !inner.is_empty() && !inner.contains(['[', ']']);
    }

    // An unbracketed host may still be a bare IPv6 literal, which is why the
    // port is split off the last colon rather than the first. But a host with a
    // colon in it must then look like hex-and-colons: that rejects a mistyped
    // "192.168.1.20:5555:66", which is indistinguishable from a bare IPv6
    // address until something downstream tries to resolve it.
    if host.contains(':') {
        let colons = host.matches(':').count();
        return colons <= 4
            && host
                .chars()
                .all(|c| c.is_ascii_hexdigit() || matches!(c, ':' | '%'));
    }

    // `%` is a zone separator, which only ever appears in an IPv6 scope id, but
    // accepting it here costs nothing and keeps a scoped address usable even if
    // the daemon parses it more strictly than we do.
    host.chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | '%'))
}

/// The directory "save to computer" offers first.
pub fn default_save_dir() -> PathBuf {
    dirs::download_dir()
        .or_else(dirs::home_dir)
        .unwrap_or_else(|| PathBuf::from("/"))
}

// ── Rendering ────────────────────────────────────────────────────────────────

/// Draw the active dialog, or `None` when there is not one.
pub fn render<F>(dialog: &Dialog, t: &Palette, on_result: F) -> Option<AnyElement>
where
    F: Fn(DialogResult, &mut Window, &mut gpui::App) + Clone + 'static,
{
    if matches!(dialog, Dialog::None) {
        return None;
    }

    let card: AnyElement = match dialog {
        Dialog::None => return None,

        Dialog::NewFolder { field } => form(
            t,
            FormSpec {
                icon: names::FOLDER_NEW,
                title: "New folder",
                hint: "It will be created in the folder you are browsing.",
                submit_label: "Create",
                submit_id: "create-folder",
            },
            field.clone(),
            on_result.clone(),
            Box::new(DialogResult::CreateFolder),
        ),

        Dialog::Rename { entry, field } => {
            let entry = entry.clone();
            let hint = format!("Rename “{}”.", entry.name);
            form(
                t,
                FormSpec {
                    icon: names::DOCUMENT_EDIT,
                    title: "Rename",
                    hint: &hint,
                    submit_label: "Rename",
                    submit_id: "rename-entry",
                },
                field.clone(),
                on_result.clone(),
                Box::new(move |value: String| DialogResult::Rename(entry.clone(), value)),
            )
        }

        Dialog::ConfirmDelete {
            label,
            entries,
            trash,
        } => {
            let count = entries.len();
            let (title, verb, icon) = if *trash {
                ("Move to Trash", "Move to Trash", names::TRASH)
            } else {
                ("Delete", "Delete", names::EDIT_DELETE)
            };
            let body = if count == 1 {
                format!("{verb} “{label}”? This cannot be undone from here.")
            } else {
                format!("{verb} {count} items? This cannot be undone from here.")
            };
            let result = if *trash {
                DialogResult::Trash(entries.clone())
            } else {
                DialogResult::DeletePermanently(entries.clone())
            };
            let accent = if *trash { t.text_dim } else { t.danger };
            card(
                t,
                CARD_W,
                vec![
                    header(t, icon, accent, title).into_any_element(),
                    body_text(t, &body).into_any_element(),
                    buttons(
                        t,
                        vec![
                            Button::normal("cancel", DialogResult::Dismiss),
                            if *trash {
                                Button::normal("move-to-trash", result)
                            } else {
                                Button::danger("delete", result)
                            },
                        ],
                        on_result.clone(),
                    )
                    .into_any_element(),
                ],
            )
        }

        Dialog::Properties {
            entry,
            full_path,
            device,
        } => properties(t, entry, full_path, device, on_result.clone()),

        Dialog::ConnectWifi {
            step,
            address_field,
            code_field,
        } => connect_wifi(
            t,
            step,
            address_field.clone(),
            code_field.clone(),
            on_result.clone(),
        ),

        Dialog::Diagnostics { report } => diagnostics(t, report, on_result.clone()),

        Dialog::SideloadApk { name, local } => card(
            t,
            CARD_W,
            vec![
                header(t, names::SOFTWARE_INSTALL, t.text_dim, "Install this APK?").into_any_element(),
                body_text(
                    t,
                    &format!(
                        "Install {name} on the connected phone, or copy it across without installing?"
                    ),
                )
                .into_any_element(),
                buttons(
                    t,
                    vec![
                        Button::normal("cancel", DialogResult::Dismiss),
                        Button::normal("copy", DialogResult::CopyApk(local.clone())),
                        Button::normal("install", DialogResult::InstallApk(local.clone())),
                    ],
                    on_result.clone(),
                )
                .into_any_element(),
            ],
        ),

        Dialog::Message {
            title,
            body,
            tone,
            extra,
        } => {
            let mut row = vec![Button::normal("close", DialogResult::Dismiss)];
            if let Some((label, result)) = extra {
                row.push(Button::normal_owned(label.clone(), result.clone()));
            }
            card(
                t,
                CARD_W,
                vec![
                    header(t, tone.icon(), tone.accent(t), title).into_any_element(),
                    body_text(t, body).into_any_element(),
                    buttons(t, row, on_result.clone()).into_any_element(),
                ],
            )
        }

        Dialog::Shortcuts => shortcuts(t, on_result.clone()),

        Dialog::ImagePreview { name, local } => {
            image_preview(t, name, local, on_result.clone())
        }

        Dialog::About => about(t, on_result.clone()),
    };

    // A dismissible dialog's backdrop is also its dismiss target; a pairing in
    // flight gets an inert one so a stray click cannot abandon it.
    let dismissible = dialog.is_dismissible();
    let backdrop: AnyElement = if dismissible {
        ui::modal_backdrop()
            .on_click({
                let on_result = on_result.clone();
                move |_, w, cx| on_result(DialogResult::Dismiss, w, cx)
            })
            .into_any_element()
    } else {
        ui::modal_backdrop().into_any_element()
    };

    Some(
        ui::on_top(
            div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .child(backdrop)
                .child(card),
        )
        .into_any_element(),
    )
}

// ── Card scaffolds ───────────────────────────────────────────────────────────

/// A dialog button.
///
/// The label is kept separate from the element id so a label built at runtime
/// never has to be leaked to obtain a `'static` id.
enum Button {
    Normal {
        id: &'static str,
        label: SharedString,
        result: DialogResult,
    },
    Danger {
        id: &'static str,
        label: SharedString,
        result: DialogResult,
    },
    /// Reads a text field when clicked and reports whatever is in it.
    ///
    /// The field is read at click time rather than mirrored into app state, so
    /// the two can never disagree.
    Submit {
        id: &'static str,
        label: &'static str,
        field: Entity<TextField>,
        build: Box<dyn Fn(String) -> DialogResult>,
    },
}

impl Button {
    /// A secondary or accepting button.
    fn normal(id: &'static str, result: DialogResult) -> Self {
        Button::Normal {
            id,
            label: label_for(id),
            result,
        }
    }

    /// A secondary button whose label is only known at runtime.
    fn normal_owned(label: String, result: DialogResult) -> Self {
        // Only one such button is ever on screen, so a fixed id cannot collide.
        Button::Normal {
            id: "extra",
            label: label.into(),
            result,
        }
    }

    /// A destructive button.
    fn danger(id: &'static str, result: DialogResult) -> Self {
        Button::Danger {
            id,
            label: label_for(id),
            result,
        }
    }

    /// An accepting button that submits the contents of `field`.
    fn submit(
        id: &'static str,
        label: &'static str,
        field: Entity<TextField>,
        build: impl Fn(String) -> DialogResult + 'static,
    ) -> Self {
        Button::Submit {
            id,
            label,
            field,
            build: Box::new(build),
        }
    }
}

/// Human label for a button, derived from its id.
fn label_for(id: &str) -> SharedString {
    id.split(['-', ' ', '_'])
        .filter(|w| !w.is_empty())
        .map(|w| {
            let mut chars = w.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
        .into()
}

/// A dialog card: rounded, with a shadow, sized to `width`.
fn card(t: &Palette, width: f32, rows: Vec<AnyElement>) -> AnyElement {
    ui::surface(t, rows)
        .w(px(width))
        .max_w(relative(0.94))
        .rounded(px(14.0))
        .shadow_2xl()
        .into_any_element()
}

/// The icon plus title at the top of a dialog.
fn header(t: &Palette, icon: &'static str, accent: gpui::Rgba, title: &str) -> gpui::Div {
    div()
        .flex()
        .items_center()
        .gap(px(10.0))
        .px(px(18.0))
        .pt(px(18.0))
        .pb(px(12.0))
        .child(icons::icon(icon, 18.0, accent))
        .child(
            div()
                .text_lg()
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(t.text_header)
                .child(title.to_string()),
        )
}

/// A paragraph of dialog body copy.
fn body_text(t: &Palette, text: &str) -> gpui::Div {
    div()
        .px(px(18.0))
        .pb(px(16.0))
        .text_sm()
        .text_color(t.text_primary)
        .line_height(relative(1.45))
        .whitespace_normal()
        .child(text.to_string())
}

/// A right-aligned row of buttons, separated from the body by a hairline.
fn buttons<F>(t: &Palette, row: Vec<Button>, on_result: F) -> gpui::Div
where
    F: Fn(DialogResult, &mut Window, &mut gpui::App) + Clone + 'static,
{
    // Cloned once here so the per-button closures can each own a handle.
    let on_result = on_result;
    let children: Vec<AnyElement> = row
        .into_iter()
        .enumerate()
        .map(|(index, button)| {
            // The index keeps ids unique when a dialog offers two buttons built
            // from the same id.
            match button {
                Button::Normal { id, label, result } => {
                    ui::button(t, ui::el_id(format!("{id}-{index}")), label, false, {
                        let on_result = on_result.clone();
                        move |_, w, cx| on_result(result.clone(), w, cx)
                    })
                    .into_any_element()
                }
                Button::Danger { id, label, result } => {
                    ui::danger_button(ui::el_id(format!("{id}-{index}")), label, {
                        let on_result = on_result.clone();
                        move |_, w, cx| on_result(result.clone(), w, cx)
                    })
                    .into_any_element()
                }
                Button::Submit {
                    id,
                    label,
                    field,
                    build,
                } => {
                    let on_result = on_result.clone();
                    ui::button(
                        t,
                        ui::el_id(format!("{id}-{index}")),
                        label,
                        true,
                        move |_, w, cx| {
                            let value = field.read(cx).value().to_string();
                            on_result(build(value), w, cx);
                        },
                    )
                    .into_any_element()
                }
            }
        })
        .collect();

    div()
        .flex()
        .items_center()
        .justify_end()
        .gap(px(8.0))
        .px(px(18.0))
        .py(px(14.0))
        .border_t_1()
        .border_color(t.border_soft)
        .children(children)
}

/// A dialog with one text field and an accepting button.
///
/// The button reads the field's current value when clicked, so the field stays
/// the single source of truth and the caller never has to mirror it.
/// The static parts of a single-field dialog, gathered so [`form`] stays under
/// clippy's argument limit.
struct FormSpec<'a> {
    /// An asset key, so `'static`; the rest are borrowed from the call site.
    icon: &'static str,
    title: &'a str,
    hint: &'a str,
    submit_label: &'static str,
    submit_id: &'static str,
}

fn form<F>(
    t: &Palette,
    spec: FormSpec<'_>,
    field: Entity<TextField>,
    on_result: F,
    submit_result: Box<dyn Fn(String) -> DialogResult>,
) -> AnyElement
where
    F: Fn(DialogResult, &mut Window, &mut gpui::App) + Clone + 'static,
{
    let FormSpec {
        icon,
        title,
        hint,
        submit_label,
        submit_id,
    } = spec;

    let cancel = ui::button(t, "cancel", "Cancel", false, {
        let on_result = on_result.clone();
        move |_, w, cx| on_result(DialogResult::Dismiss, w, cx)
    });
    let confirm = ui::button(t, submit_id, submit_label, true, {
        let on_result = on_result.clone();
        let field = field.clone();
        let submit_result = submit_result;
        move |_, w, cx| {
            let value = field.read(cx).value().to_string();
            on_result(submit_result(value), w, cx);
        }
    });

    card(
        t,
        CARD_W,
        vec![
            header(t, icon, t.text_dim, title).into_any_element(),
            div()
                .px(px(18.0))
                .pb(px(12.0))
                .text_sm()
                .text_color(t.text_dim)
                .whitespace_normal()
                .child(hint.to_string())
                .into_any_element(),
            div()
                .px(px(18.0))
                .pb(px(16.0))
                .child(field)
                .into_any_element(),
            div()
                .flex()
                .items_center()
                .justify_end()
                .gap(px(8.0))
                .px(px(18.0))
                .py(px(14.0))
                .border_t_1()
                .border_color(t.border_soft)
                .children([cancel.into_any_element(), confirm.into_any_element()])
                .into_any_element(),
        ],
    )
}

/// A label/value table, as used by Properties and About.
fn detail_table(t: &Palette, rows: Vec<(&str, String)>, label_w: f32) -> AnyElement {
    div()
        .px(px(18.0))
        .pb(px(10.0))
        .children(rows.into_iter().map(|(label, value)| {
            div()
                .flex()
                .items_start()
                .gap(px(14.0))
                .py(px(5.0))
                .child(
                    div()
                        .w(px(label_w))
                        .flex_shrink_0()
                        .text_sm()
                        .text_color(t.text_muted)
                        .child(label.to_string()),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_sm()
                        .text_color(t.text_primary)
                        .whitespace_normal()
                        .child(value),
                )
                .into_any_element()
        }))
        .into_any_element()
}

// ── Specific dialogs ─────────────────────────────────────────────────────────

/// Name, type, size, location, modified and permissions.
/// A human description of what a file is, from its extension.
///
/// The GTK build had a table for this and said "Image", "Android Package",
/// "PDF Document" and so on; the rewrite collapsed it to "File", which is the
/// one answer nobody needs. Only extensions are consulted — there is no MIME
/// database here — so an unknown extension falls back to "File".
fn file_kind(entry: &DirEntry) -> String {
    if entry.is_symlink && !entry.is_dir {
        return "Symbolic link".to_string();
    }
    if entry.is_dir {
        return "Folder".to_string();
    }
    let ext = entry.ext();
    if ext.is_empty() {
        return "File".to_string();
    }
    let label = match ext.as_str() {
        "apk" => "Android package",
        "zip" | "7z" | "rar" | "xz" | "zst" => "Archive",
        "tar" | "gz" | "tgz" | "bz2" => "Tar archive",
        "pdf" => "PDF document",
        "doc" | "docx" | "odt" | "rtf" => "Word processor document",
        "xls" | "xlsx" | "ods" | "csv" => "Spreadsheet",
        "ppt" | "pptx" | "odp" => "Presentation",
        "epub" => "E-book",
        "txt" | "log" | "md" => "Text document",
        "json" | "xml" | "yaml" | "yml" | "toml" | "ini" => "Structured data",
        "mp3" | "flac" | "ogg" | "wav" | "m4a" | "aac" | "opus" => "Audio",
        "mp4" | "mkv" | "avi" | "webm" | "mov" | "m4v" => "Video",
        "png" | "jpg" | "jpeg" | "webp" | "gif" | "bmp" | "svg" | "heic" => "Image",
        _ => return "File".to_string(),
    };
    // "Image" reads better than "image file", but "PDF document" does not want
    // a trailing noun, so the label carries its own shape.
    label.to_string()
}

fn properties<F>(
    t: &Palette,
    entry: &DirEntry,
    full_path: &Path,
    device: &str,
    on_result: F,
) -> AnyElement
where
    F: Fn(DialogResult, &mut Window, &mut gpui::App) + Clone + 'static,
{
    let kind = file_kind(entry);
    let size = if entry.is_dir {
        "—".to_string()
    } else {
        human_size(entry.size)
    };

    let rows = vec![
        ("Name", entry.name.clone()),
        ("Type", kind.to_string()),
        ("Size", size),
        ("Location", full_path.display().to_string()),
        ("Modified", entry.mtime_string()),
        ("Permissions", entry.mode_string()),
        ("Device", device.to_string()),
    ];

    card(
        t,
        WIDE_W,
        vec![
            header(t, names::DIALOG_INFORMATION, t.text_dim, "Properties").into_any_element(),
            detail_table(t, rows, 96.0),
            buttons(
                t,
                vec![Button::normal("close", DialogResult::Dismiss)],
                on_result,
            )
            .into_any_element(),
        ],
    )
}

/// The two-step wireless pairing flow.
fn connect_wifi<F>(
    t: &Palette,
    step: &WifiStep,
    address_field: Entity<TextField>,
    code_field: Entity<TextField>,
    on_result: F,
) -> AnyElement
where
    F: Fn(DialogResult, &mut Window, &mut gpui::App) + Clone + 'static,
{
    let (icon, accent, title, body, field, row) = match step {
        WifiStep::Address => (
            names::WIRELESS,
            t.text_dim,
            "Connect ADB via IP",
            "On the phone, open Settings \u{2192} About phone and tap Build number seven \
             times, then Developer options \u{2192} Wireless debugging. Pair once, then \
             connect by IP address."
                .to_string(),
            Some(address_field),
            vec![Button::normal("cancel", DialogResult::Dismiss)],
        ),
        WifiStep::Pairing { address } => (
            names::WIRELESS,
            t.warning,
            "Pair with this phone",
            format!(
                "On the phone, tap Pair device with pairing code, then enter this address \
                 and the six-digit code it shows.\n\n{address}"
            ),
            Some(code_field),
            vec![Button::normal("cancel", DialogResult::Dismiss)],
        ),
        WifiStep::Paired { address } => (
            names::OBJECT_SELECT,
            t.success,
            "Paired",
            format!(
                "The phone is paired. Connect to {address} now? You only need to do this \
                 once per network."
            ),
            None,
            vec![
                Button::normal("not now", DialogResult::Dismiss),
                Button::normal("connect", DialogResult::ConnectWireless(address.clone())),
            ],
        ),
        WifiStep::Failed { message } => (
            names::DIALOG_ERROR,
            t.danger,
            "Could not connect",
            message.clone(),
            None,
            vec![Button::normal("close", DialogResult::Dismiss)],
        ),
    };

    // The forward buttons must read a text field, so they are built here rather
    // than in the match above, where only the `field` in play is known.
    let field_for_submit = field.clone();
    let row: Vec<Button> = match step {
        WifiStep::Address => vec![
            Button::normal("cancel", DialogResult::Dismiss),
            Button::submit(
                "next",
                "Next",
                field_for_submit.expect("the address step always has a field"),
                DialogResult::PairAddress,
            ),
        ],
        WifiStep::Pairing { address } => {
            let address = address.clone();
            vec![
                Button::normal("cancel", DialogResult::Dismiss),
                Button::submit(
                    "pair",
                    "Pair",
                    field_for_submit.expect("the pairing step always has a field"),
                    move |value| DialogResult::PairCode {
                        address: address.clone(),
                        code: value,
                    },
                ),
            ]
        }
        _ => row,
    };

    let mut rows = vec![
        header(t, icon, accent, title).into_any_element(),
        body_text(t, &body).into_any_element(),
    ];
    if let Some(field) = field {
        rows.push(
            div()
                .px(px(18.0))
                .pb(px(16.0))
                .child(field)
                .into_any_element(),
        );
    }
    rows.push(buttons(t, row, on_result).into_any_element());

    card(t, CARD_W, rows)
}

/// The rendered diagnostics report, with a "Copy report" button.
fn diagnostics<F>(t: &Palette, report: &DiagnosticReportDto, on_result: F) -> AnyElement
where
    F: Fn(DialogResult, &mut Window, &mut gpui::App) + Clone + 'static,
{
    let text = format_diagnostics(report);
    let accent = if report.adb_ok { t.success } else { t.danger };

    card(
        t,
        WIDE_W,
        vec![
            header(
                t,
                names::DIALOG_INFORMATION,
                accent,
                "Connection diagnostics",
            )
            .into_any_element(),
            div()
                .px(px(18.0))
                .pb(px(16.0))
                .font_family(theme::MONO)
                .text_size(px(11.0))
                .line_height(relative(1.5))
                .text_color(t.text_primary)
                .whitespace_normal()
                .child(text.clone())
                .into_any_element(),
            buttons(
                t,
                vec![
                    Button::normal_owned("Copy report".into(), DialogResult::CopyDiagnostics(text)),
                    Button::normal("close", DialogResult::Dismiss),
                ],
                on_result,
            )
            .into_any_element(),
        ],
    )
}

/// The keyboard shortcuts, in one place.
///
/// The app has always had this many bindings and they were only documented in
/// the README, which nobody has open while using the thing. Listing them is
/// cheap and removes the need to guess.
/// Every shortcut the app binds, grouped for display.
///
/// A test asserts no two rows claim the same keys, because a duplicate row is
/// how "Alt+Up" ended up listed twice.
const SHORTCUT_GROUPS: &[(&str, &[(&str, &str)])] = &[
    (
        "Navigation",
        &[
            ("Back / Forward", "Alt+Left / Alt+Right"),
            ("Up one folder", "Alt+Up"),
            ("Edit the path directly", "Ctrl+L"),
            ("Refresh", "F5"),
        ],
    ),
    (
        "Selection",
        &[
            ("Move focus", "Up / Down"),
            ("First / last item", "Home / End"),
            ("Select all", "Ctrl+A"),
            ("Extend selection", "Shift or Ctrl + click"),
            ("Select a band", "Drag in the file area"),
            ("Close a menu, field or dialog; else clear", "Esc"),
        ],
    ),
    (
        "Files",
        &[
            ("Open", "Enter or double click"),
            ("New folder", "Ctrl+N"),
            ("Rename", "F2"),
            ("Move to Trash", "Delete"),
            ("Delete permanently", "Shift+Delete"),
            ("Copy / Paste", "Ctrl+C / Ctrl+V"),
            ("Send to phone", "Ctrl+U"),
            ("Save to computer", "Ctrl+Shift+C"),
            ("Open in terminal", "Context menu"),
        ],
    ),
    (
        "View",
        &[
            ("Toggle sidebar", "F9"),
            ("Switch grid / list", "Alt+T"),
            ("Zoom out / in", "Ctrl+minus / Ctrl+plus"),
            ("Search this folder", "Ctrl+F"),
            ("Show hidden files", "Overflow menu"),
        ],
    ),
];

fn shortcuts<F>(t: &Palette, on_result: F) -> AnyElement
where
    F: Fn(DialogResult, &mut Window, &mut gpui::App) + Clone + 'static,
{
    let mut body: Vec<AnyElement> = Vec::new();
    for (group, rows) in SHORTCUT_GROUPS {
        body.push(
            div()
                .px(px(18.0))
                .pt(px(12.0))
                .text_size(px(10.0))
                .font_weight(gpui::FontWeight::BOLD)
                .text_color(t.text_muted)
                .child(group.to_uppercase())
                .into_any_element(),
        );
        for (label, keys) in rows.iter() {
            body.push(
                div()
                    .flex()
                    .items_center()
                    .gap(px(12.0))
                    .px(px(18.0))
                    .py(px(3.0))
                    .child(
                        div()
                            .flex_1()
                            .text_sm()
                            .text_color(t.text_primary)
                            .child(*label),
                    )
                    .child(
                        div()
                            .font_family(theme::MONO)
                            .text_size(px(11.0))
                            .text_color(t.text_dim)
                            .child(*keys),
                    )
                    .into_any_element(),
            );
        }
    }

    card(
        t,
        WIDE_W,
        vec![
            header(
                t,
                names::DIALOG_INFORMATION,
                t.text_dim,
                "Keyboard shortcuts",
            )
            .into_any_element(),
            div()
                .id("shortcut-list")
                .pb(px(12.0))
                .max_h(px(460.0))
                .overflow_y_scroll()
                .scrollbar_width(px(6.0))
                .children(body)
                .into_any_element(),
            buttons(
                t,
                vec![Button::normal("close", DialogResult::Dismiss)],
                on_result,
            )
            .into_any_element(),
        ],
    )
}

/// A still image, read through the device's FUSE mount.
fn image_preview<F>(t: &Palette, name: &str, local: &Path, on_result: F) -> AnyElement
where
    F: Fn(DialogResult, &mut Window, &mut gpui::App) + Clone + 'static,
{
    card(
        t,
        WIDE_W,
        vec![
            header(t, names::IMAGE_GENERIC, t.text_dim, name).into_any_element(),
            div()
                .flex()
                .items_center()
                .justify_center()
                .px(px(18.0))
                .pb(px(16.0))
                .child(
                    gpui::img(gpui::ImageSource::from(local.to_path_buf()))
                        .max_w(px(PREVIEW_MAX))
                        .max_h(px(PREVIEW_MAX))
                        .object_fit(gpui::ObjectFit::Contain)
                        .rounded(px(8.0)),
                )
                .into_any_element(),
            buttons(
                t,
                vec![
                    Button::normal(
                        "open externally",
                        DialogResult::OpenExternal(local.to_path_buf()),
                    ),
                    Button::normal("close", DialogResult::Dismiss),
                ],
                on_result,
            )
            .into_any_element(),
        ],
    )
}

/// Version and licence information.
fn about<F>(t: &Palette, on_result: F) -> AnyElement
where
    F: Fn(DialogResult, &mut Window, &mut gpui::App) + Clone + 'static,
{
    let rows = vec![
        ("Version", env!("CARGO_PKG_VERSION").to_string()),
        ("Bus", "org.adbshare.Manager (session D-Bus)".to_string()),
        (
            "Licence",
            "GPL-3.0-or-later. The transfer queue lives in memory and is lost on exit.".to_string(),
        ),
        // The Lucide SVGs are embedded in this binary under the ISC licence,
        // which requires the notice to travel with them.
        ("Icons", "Lucide (ISC)".to_string()),
    ];

    card(
        t,
        CARD_W,
        vec![
            div()
                .flex()
                .flex_col()
                .items_center()
                .gap(px(6.0))
                .px(px(18.0))
                .pt(px(22.0))
                .pb(px(18.0))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .justify_center()
                        .size(px(48.0))
                        .rounded(px(14.0))
                        .bg(t.hover)
                        .child(icons::icon(names::PHONE, 24.0, t.text_header)),
                )
                .child(
                    div()
                        .text_xl()
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(t.text_header)
                        .child("ADBShare"),
                )
                .child(
                    div()
                        .text_sm()
                        .text_color(t.text_dim)
                        .child("Browse and transfer files over ADB"),
                )
                .into_any_element(),
            detail_table(t, rows, 76.0),
            // The ISC notice for the embedded Lucide icons, rendered rather than
            // just named: the licence requires the notice to accompany the
            // work, and a binary that embeds 61 SVGs is a copy of them. The
            // Yaru folders are CC-BY-SA-4.0, which asks for the credit; the
            // licence text itself is 400-odd lines and stays in the repo.
            div()
                .px(px(18.0))
                .pb(px(12.0))
                .text_size(px(9.0))
                .font_family(theme::MONO)
                .text_color(t.text_muted)
                .child(icons::YARU_ATTRIBUTION)
                .child("\n\n")
                .child(icons::LUCIDE_LICENSE.trim())
                .into_any_element(),
            buttons(
                t,
                vec![Button::normal("close", DialogResult::Dismiss)],
                on_result,
            )
            .into_any_element(),
        ],
    )
}

#[cfg(test)]
mod tests {
    use crate::protocol::DirEntry;

    fn named(name: &str) -> DirEntry {
        DirEntry {
            name: name.into(),
            is_dir: false,
            is_symlink: false,
            size: 0,
            mode: 0o644,
            mtime: 0,
        }
    }

    #[test]
    fn file_kind_names_the_common_types() {
        assert_eq!(file_kind(&named("report.pdf")), "PDF document");
        assert_eq!(file_kind(&named("app.apk")), "Android package");
        assert_eq!(file_kind(&named("holiday.jpg")), "Image");
        assert_eq!(file_kind(&named("song.flac")), "Audio");
        assert_eq!(file_kind(&named("bundle.tar.gz")), "Tar archive");
    }

    #[test]
    fn file_kind_falls_back_for_anything_it_does_not_know() {
        assert_eq!(file_kind(&named("mystery")), "File");
        assert_eq!(file_kind(&named("libfoo.so.7")), "File");
        let mut dir = named("photos");
        dir.is_dir = true;
        assert_eq!(file_kind(&dir), "Folder");
        let mut link = named("shortcut");
        link.is_symlink = true;
        assert_eq!(file_kind(&link), "Symbolic link");
    }

    use super::*;

    #[test]
    fn names_reject_path_separators_and_reserved_words() {
        for bad in ["", "   ", ".", ".."] {
            assert!(validate_name(bad).is_err(), "{bad:?} should be rejected");
        }
        for bad in [
            "a/b", "a\\b", "a:b", "a*b", "a?b", "a\"b", "a<b", "a>b", "a|b",
        ] {
            assert!(validate_name(bad).is_err(), "{bad:?} should be rejected");
        }
        assert!(
            validate_name("with\u{0}nul").is_err(),
            "a NUL byte must be rejected"
        );
    }

    #[test]
    fn names_reject_invisible_trailing_characters() {
        assert!(validate_name("folder.").is_err(), "trailing dot");
        assert!(validate_name("a b").is_ok(), "an inner space is fine");
        // A trailing space is trimmed rather than rejected, which is both
        // friendlier and what keeps the name FAT-safe.
        assert_eq!(validate_name("folder ").expect("trimmed"), "folder");
        assert!(
            validate_name("folder. ").is_err(),
            "trim, then reject the dot"
        );
    }

    #[test]
    fn names_are_trimmed_and_length_checked() {
        assert_eq!(validate_name("  Photos  ").expect("ok"), "Photos");
        assert!(validate_name(&"x".repeat(255)).is_ok());
        assert!(validate_name(&"x".repeat(256)).is_err());
    }

    #[test]
    fn pairing_addresses_cover_ipv4_hostnames_and_ipv6() {
        for good in [
            "192.168.1.20:37001",
            "10.0.0.5:5555",
            "phone.local:5037",
            "my_phone-2:1",
            "[fe80::1ff:fe23:4567:890a]:37001",
            "fe80::1:37001",
            "host%eth0:5555",
        ] {
            assert!(address_is_well_formed(good), "{good} should be accepted");
        }
    }

    #[test]
    fn pairing_addresses_reject_malformed_input() {
        for bad in [
            "",
            "192.168.1.20",
            "192.168.1.20:",
            "192.168.1.20:abc",
            "192.168.1.20:0",
            "192.168.1.20:70000",
            ":37001",
            "192.168.1.20:5555:66",
            "[fe80::1:37001",
            "fe80::1]:37001",
            "[fe80::1]37001",
            "host name:5555",
        ] {
            assert!(!address_is_well_formed(bad), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn pair_input_validation_checks_address_then_code() {
        assert!(validate_pair_input("192.168.1.20:37001", "123456").is_ok());

        let err = validate_pair_input("nonsense", "123456").expect_err("bad address");
        assert!(err.contains("192.168.1.20:37001"), "{err}");

        for bad in ["12345", "1234567", "12345a", "", "   "] {
            let err = validate_pair_input("192.168.1.20:37001", bad).expect_err("bad code");
            assert!(err.contains("six digits"), "{bad:?}: {err}");
        }
    }

    #[test]
    fn a_pairing_in_flight_is_not_dismissible() {
        assert!(
            !step_is_dismissible(&WifiStep::Pairing {
                address: "192.168.1.20:37001".into()
            }),
            "a stray click must not abandon a pairing that is already running"
        );
    }

    #[test]
    fn ordinary_pairing_steps_are_dismissible() {
        for step in [
            WifiStep::Address,
            WifiStep::Paired {
                address: "192.168.1.20:5555".into(),
            },
            WifiStep::Failed {
                message: "nope".into(),
            },
        ] {
            assert!(step_is_dismissible(&step), "{step:?} should be dismissible");
        }
    }

    /// Map a human key label to the spelling the bindings table uses.
    ///
    /// GPUI's grammar is `[secondary-][ctrl-][alt-][shift-]key`, where
    /// `secondary` is Ctrl on Linux. The dialog writes the labels a person
    /// would read, so they have to be translated before they can be checked.
    fn normalise(keys: &str) -> Vec<String> {
        keys.split(" / ")
            .map(|combo| combo.trim())
            .filter(|combo| !combo.is_empty())
            .map(|combo| {
                let mut out = String::new();
                for part in combo.split('+') {
                    match part.trim().to_lowercase().as_str() {
                        "ctrl" => out.push_str("secondary-"),
                        "cmd" | "super" => out.push_str("cmd-"),
                        "alt" | "option" => out.push_str("alt-"),
                        "shift" => out.push_str("shift-"),
                        "ctrlcmd" => out.push_str("secondary-"),
                        key => {
                            out.push_str(key);
                            return out;
                        }
                    }
                }
                out
            })
            .collect()
    }

    /// Human labels that describe a gesture or a menu rather than a key.
    const NOT_KEYS: &[&str] = &[
        "context menu",
        "overflow menu",
        "enter or double click",
        "esc",
        "drag in the file area",
        "shift or ctrl + click",
    ];

    #[test]
    fn every_key_the_shortcuts_dialog_claims_is_actually_bound() {
        // Regression: the dialog promised Alt+T for "open in terminal" when
        // Alt+T switches the view and the terminal has no binding at all.
        for (_, rows) in SHORTCUT_GROUPS {
            for (label, keys) in rows.iter() {
                if NOT_KEYS.contains(&keys.trim().to_lowercase().as_str()) {
                    continue;
                }
                for key in normalise(keys) {
                    assert!(
                        crate::browser::is_bound(&key),
                        "{label:?} claims {key:?}, which nothing binds"
                    );
                }
            }
        }
    }

    #[test]
    fn no_two_shortcut_rows_claim_the_same_keys() {
        // A duplicate row is how "Alt+Up" came to be listed twice.
        let mut seen: Vec<&str> = Vec::new();
        for (_, rows) in SHORTCUT_GROUPS {
            for (label, keys) in rows.iter() {
                for key in keys.split(" / ") {
                    assert!(
                        !seen.contains(&key),
                        "{key:?} is claimed by more than one row ({label:?})"
                    );
                    seen.push(key);
                }
            }
        }
    }

    #[test]
    fn every_shortcut_row_has_a_label_and_keys() {
        for (group, rows) in SHORTCUT_GROUPS {
            assert!(!group.is_empty());
            for (label, keys) in rows.iter() {
                assert!(!label.trim().is_empty(), "{group}: a row has no label");
                assert!(!keys.trim().is_empty(), "{label}: a row has no keys");
            }
        }
    }

    #[test]
    fn headings_name_every_pairing_step() {
        assert_eq!(step_heading(&WifiStep::Address), "Connect ADB via IP");
        assert_eq!(
            step_heading(&WifiStep::Pairing {
                address: "a:1".into()
            }),
            "Pair with this phone"
        );
        assert_eq!(
            step_heading(&WifiStep::Paired {
                address: "a:1".into()
            }),
            "Paired"
        );
        assert_eq!(
            step_heading(&WifiStep::Failed {
                message: "x".into()
            }),
            "Could not connect"
        );
    }

    #[test]
    fn headings_name_every_dialog() {
        assert_eq!(Dialog::None.heading(), "");
        assert_eq!(Dialog::About.heading(), "About ADBShare");
        assert_eq!(
            Dialog::Diagnostics {
                report: Default::default()
            }
            .heading(),
            "Connection diagnostics"
        );
        assert_eq!(
            Dialog::Message {
                title: "Transfer failed".into(),
                body: String::new(),
                tone: MessageTone::Error,
                extra: None,
            }
            .heading(),
            "Transfer failed"
        );
        assert_eq!(
            Dialog::ImagePreview {
                name: "cat.png".into(),
                local: PathBuf::from("/tmp/cat.png"),
            }
            .heading(),
            "cat.png"
        );
    }

    #[test]
    fn no_dialog_is_dismissible() {
        assert!(!Dialog::None.is_dismissible());
    }

    #[test]
    fn button_labels_are_derived_from_ids() {
        assert_eq!(label_for("cancel"), "Cancel");
        assert_eq!(label_for("move-to-trash"), "Move To Trash");
        assert_eq!(label_for("open externally"), "Open Externally");
    }

    #[test]
    fn default_save_dir_is_absolute() {
        assert!(
            default_save_dir().is_absolute(),
            "a relative save directory would resolve against the process cwd"
        );
    }
}

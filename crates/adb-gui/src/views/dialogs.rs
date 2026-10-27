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
use crate::protocol::{
    DiagnosticReportDto, DirEntry, InstallStatusDto, PackageActionDto, PackageInfoDto,
    PackageRequestDto, StateTone, format_diagnostics, human_size,
};
use crate::state::apk::ApkInfo;
use crate::state::debloat::Removal;
use crate::theme::{self, Palette};
use crate::views::textinput::TextField;
use crate::views::ui;

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
    ///
    /// The manifest is read while the dialog is open, in the background; the
    /// fields the card shows arrive through `info` once it lands.
    SideloadApk {
        local: PathBuf,
        /// Parsed manifest. None until the background read finishes, or when
        /// it failed — see `parse_error`.
        info: Option<ApkInfo>,
        /// Why the manifest could not be read, when it could not.
        parse_error: Option<String>,
        /// What the device has installed for this package, if anything.
        installed: Option<PackageInfoDto>,
        /// Device the install would go to.
        device: String,
        /// The device's primary ABI, for the native-libs warning.
        device_abi: Option<String>,
        /// Whether "Copy across" applies (host-local files only).
        can_copy: bool,
    },
    /// An install in flight, with live progress from the daemon. The daemon's
    /// install id lives in the app's `install_watch`; the dialog only renders.
    Installing {
        label: String,
        status: Option<InstallStatusDto>,
    },
    /// An APK already on the device, opened from its browser. Installing from
    /// there is one click, so a plain confirm replaces the rich card.
    ConfirmDeviceInstall {
        name: String,
        device_path: String,
    },
    /// Several APKs dropped or picked at once.
    SideloadBatch {
        /// The dialog owns this list; the app reads it back through
        /// `self.dialog` when "Install all" or "Copy all" is pressed.
        items: Vec<BatchItem>,
        device: String,
    },
    /// An install failed, with classified advice and recovery actions.
    InstallFailed {
        label: String,
        code: Option<String>,
        message: Option<String>,
        raw: Option<String>,
        device: String,
        /// Package name, when known — the uninstall-and-retry action needs it.
        package: Option<String>,
        path: String,
        from_device: bool,
    },
    /// What is about to happen to a batch of packages.
    DebloatConfirm {
        device: String,
        /// The headline, e.g. "Disable".
        action: PackageActionDto,
        /// What will actually run, one entry per package.
        actions: Vec<PackageRequestDto>,
        /// The packages the action applies to.
        packages: Vec<DebloatPackage>,
        /// Apps that would be left broken by this, and are not in the
        /// selection.
        breaks: Vec<String>,
        /// True when this is putting back what a previous session changed.
        undo: bool,
    },
    /// A batch of package changes in flight.
    DebloatWorking {
        count: usize,
    },
    /// What a batch of package changes actually did.
    DebloatResult {
        action: PackageActionDto,
        /// The packages that changed.
        done: Vec<String>,
        /// The packages that did not, with the reason.
        failed: Vec<DebloatFailure>,
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

/// One row of the batch install dialog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BatchItem {
    pub name: String,
    pub local: PathBuf,
    pub state: ItemState,
}

/// One package in a batch of changes, as the confirmation card shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DebloatPackage {
    pub id: String,
    /// The curated list's description of the app.
    pub description: Option<String>,
    /// The curated list's risk tier, when it has one.
    pub risk: Option<Removal>,
    /// True when the app is on the system's read-only partition.
    pub system: bool,
    /// Other packages that need this one.
    pub needed_by: Vec<String>,
}

/// A package that could not be changed, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DebloatFailure {
    pub id: String,
    /// A classified reason, e.g. `system_uninstall_blocked`.
    pub code: Option<String>,
    /// A sentence a person can act on.
    pub message: Option<String>,
}

/// Where one batch row stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ItemState {
    Queued,
    Running {
        percent: u8,
        state: String,
    },
    Succeeded,
    Failed {
        code: Option<String>,
        message: Option<String>,
    },
}

impl ItemState {
    /// `(pill text, semantic tone)`.
    pub fn pill(&self) -> (&'static str, StateTone) {
        use StateTone as T;
        match self {
            ItemState::Queued => ("Queued", T::Neutral),
            ItemState::Running { .. } => ("Running", T::Active),
            ItemState::Succeeded => ("Installed", T::Success),
            ItemState::Failed { .. } => ("Failed", T::Danger),
        }
    }
}

// StateTone comes from protocol.rs, shared with the job pills in ui.rs.

impl Dialog {
    /// Whether Escape and a backdrop click should dismiss this dialog.
    ///
    /// A pairing already in flight must not be dismissed by a stray click, or
    /// the user loses track of an operation that is still running. A batch
    /// with anything still running stays put for the same reason.
    pub fn is_dismissible(&self) -> bool {
        match self {
            Dialog::None => false,
            Dialog::ConnectWifi { step, .. } => step_is_dismissible(step),
            Dialog::SideloadBatch { items, .. } => !items
                .iter()
                .any(|i| matches!(i.state, ItemState::Running { .. })),
            // A batch of package changes is in flight: the phone is being
            // changed, and a stray click should not hide that.
            Dialog::DebloatWorking { .. } => false,
            _ => true,
        }
    }

    /// The dialog title, also used as the accessible name and by tests.
    pub fn heading(&self) -> String {
        match self {
            Dialog::None => String::new(),
            Dialog::NewFolder { .. } => "New folder".into(),
            Dialog::Rename { .. } => "Rename".into(),
            Dialog::ConfirmDelete { .. } => "Delete".into(),
            Dialog::Properties { .. } => "Properties".into(),
            Dialog::ConnectWifi { step, .. } => step_heading(step).into(),
            Dialog::Diagnostics { .. } => "Connection diagnostics".into(),
            Dialog::SideloadApk { .. } => "Install this APK?".into(),
            Dialog::Installing { .. } => "Installing".into(),
            Dialog::ConfirmDeviceInstall { .. } => "Install this APK?".into(),
            Dialog::SideloadBatch { .. } => "Install APKs".into(),
            Dialog::InstallFailed { .. } => "Could not install".into(),
            Dialog::DebloatConfirm { .. } => "Change these packages?".into(),
            Dialog::DebloatWorking { .. } => "Working".into(),
            Dialog::DebloatResult { .. } => "Packages changed".into(),
            Dialog::Message { title, .. } => title.clone(),
            Dialog::ImagePreview { name, .. } => name.clone(),
            Dialog::Shortcuts => "Keyboard shortcuts".into(),
            Dialog::About => "About ADBShare".into(),
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
    /// Install an APK that is already on the device.
    InstallFromDevice(String),
    /// Recovery: remove the installed package, then install again.
    UninstallAndRetry {
        device: String,
        package: String,
        path: String,
        from_device: bool,
    },
    /// Batch: install every queued item, one after another.
    InstallAll,
    /// Batch: copy every queued item across without installing.
    CopyAll,
    /// Carry out a change to a batch of packages.
    DebloatApply {
        /// What to do to each package.
        actions: Vec<PackageRequestDto>,
    },
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

        Dialog::SideloadApk {
            local,
            info,
            parse_error,
            installed,
            device,
            device_abi,
            can_copy,
        } => sideload_apk(
            t,
            local,
            info.as_ref(),
            parse_error.as_deref(),
            installed.as_ref(),
            device,
            device_abi.as_deref(),
            *can_copy,
            on_result.clone(),
        ),

        Dialog::Installing { label, status, .. } => {
            installing(t, label, status.as_ref(), on_result.clone())
        }

        Dialog::ConfirmDeviceInstall { name, device_path } => card(
            t,
            CARD_W,
            vec![
                header(
                    t,
                    names::SOFTWARE_INSTALL,
                    ui::icon_tint(names::SOFTWARE_INSTALL, t),
                    "Install this APK?",
                )
                .into_any_element(),
                body_text(
                    t,
                    &format!("Install {name} from the phone's storage on this device?"),
                )
                .into_any_element(),
                detail_table(t, vec![("Location", device_path.clone())], 90.0),
                buttons(
                    t,
                    vec![
                        Button::normal("cancel", DialogResult::Dismiss),
                        Button::primary(
                            "install",
                            DialogResult::InstallFromDevice(device_path.clone()),
                        ),
                    ],
                    on_result.clone(),
                )
                .into_any_element(),
            ],
        ),

        Dialog::SideloadBatch { items, device } => {
            sideload_batch(t, items, device, on_result.clone())
        }

        Dialog::InstallFailed {
            label,
            code,
            message,
            raw,
            device,
            package,
            path,
            from_device,
        } => install_failed(
            t,
            label,
            code.as_deref(),
            message.as_deref(),
            raw.as_deref(),
            device,
            package.as_deref(),
            path,
            *from_device,
            on_result.clone(),
        ),

        Dialog::DebloatConfirm {
            device,
            action,
            actions,
            packages,
            breaks,
            undo,
        } => debloat_confirm(
            t,
            device,
            *action,
            actions,
            packages,
            breaks,
            *undo,
            on_result.clone(),
        ),

        Dialog::DebloatWorking { count } => debloat_working(t, *count, on_result.clone()),

        Dialog::DebloatResult {
            action,
            done,
            failed,
        } => debloat_result(t, *action, done, failed, on_result.clone()),

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

        Dialog::ImagePreview { name, local } => image_preview(t, name, local, on_result.clone()),

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

/// The pre-install card for a host-local APK: what it is, what the phone has
/// now, what installing would do.
#[allow(clippy::too_many_arguments)]
fn sideload_apk<F>(
    t: &Palette,
    local: &Path,
    info: Option<&ApkInfo>,
    parse_error: Option<&str>,
    installed: Option<&PackageInfoDto>,
    device: &str,
    device_abi: Option<&str>,
    can_copy: bool,
    on_result: F,
) -> AnyElement
where
    F: Fn(DialogResult, &mut Window, &mut gpui::App) + Clone + 'static,
{
    let name = local
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();

    // ── what the card knows so far ──────────────────────────────────────
    let mut rows: Vec<AnyElement> = Vec::new();
    match (info, parse_error) {
        (Some(info), _) => {
            if let Some(label) = &info.label {
                rows.push(
                    div()
                        .px(px(18.0))
                        .pb(px(4.0))
                        .text_size(t.text(15.0))
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(t.text_header)
                        .child(label.clone())
                        .into_any_element(),
                );
            }
            let mut table: Vec<(&str, String)> = vec![
                ("Package", info.package.clone()),
                (
                    "Version",
                    match (&info.version_name, info.version_code) {
                        (Some(v), Some(c)) => format!("{v} ({c})"),
                        (Some(v), None) => v.clone(),
                        (None, Some(c)) => c.to_string(),
                        (None, None) => "—".into(),
                    },
                ),
                ("Size", human_size(info.size.unwrap_or(0))),
            ];
            if let Some(min) = info.min_sdk {
                table.push(("Min Android", pretty_api(min)));
            }
            if !info.permissions.is_empty() {
                let short: Vec<String> = info
                    .permissions
                    .iter()
                    .map(|p| p.rsplit('.').next().unwrap_or(p).to_string())
                    .collect();
                table.push(("Permissions", short.join(", ")));
            }
            // ABI warning: native libs that cannot run on this phone.
            if !info.native_abis.is_empty()
                && let Some(abi) = device_abi
                && !info.native_abis.iter().any(|a| a == abi)
            {
                table.push((
                    "Architecture",
                    format!(
                        "{abi} device — this APK ships {:?} and may not install",
                        info.native_abis
                    ),
                ));
            }
            rows.push(detail_table(t, table, 110.0));
        }
        (None, Some(err)) => {
            rows.push(
                body_text(t, &format!("{name} could not be read as an APK: {err}"))
                    .into_any_element(),
            );
        }
        (None, None) => {
            rows.push(body_text(t, "Reading the package…").into_any_element());
        }
    }

    // ── the upgrade / downgrade / replace banner ─────────────────────────
    if let (Some(info), Some(installed)) = (info, installed)
        && installed.installed
    {
        let verdict = match (info.version_code, installed.version_code) {
            (Some(new), Some(old)) if new > old => ("Upgrade", StateTone::Active),
            (Some(new), Some(old)) if new < old => (
                "Downgrade — the device will refuse unless the old version is removed first",
                StateTone::Warning,
            ),
            (Some(_), Some(_)) => ("Replace", StateTone::Neutral),
            _ => ("Already installed", StateTone::Neutral),
        };
        let (fg, bg) = ui::tone_colors(t, verdict.1);
        let installed_text = match &installed.version_name {
            Some(v) => format!(" v{v}"),
            _ => String::new(),
        };
        rows.push(
            div()
                .px(px(18.0))
                .pb(px(12.0))
                .flex()
                .items_center()
                .gap(px(8.0))
                .child(ui::pill(t, verdict.0, fg, bg))
                .child(
                    div()
                        .text_size(t.text(12.0))
                        .text_color(t.text_muted)
                        .child(format!(
                            "{}{installed_text} is on {device}",
                            installed.package
                        )),
                )
                .into_any_element(),
        );
    }

    // ── actions ─────────────────────────────────────────────────────────
    let mut row = vec![Button::normal("cancel", DialogResult::Dismiss)];
    if can_copy {
        row.push(Button::normal(
            "copy",
            DialogResult::CopyApk(local.to_path_buf()),
        ));
    }
    row.push(Button::primary(
        "install",
        DialogResult::InstallApk(local.to_path_buf()),
    ));
    rows.push(buttons(t, row, on_result).into_any_element());

    card(t, CARD_W, rows)
}

/// 21 → "Android 5.0"; beyond the map, "API 34".
fn pretty_api(sdk: u32) -> String {
    let api = match sdk {
        21 => "5.0",
        22 => "5.1",
        23 => "6.0",
        24 => "7.0",
        25 => "7.1",
        26 => "8.0",
        27 => "8.1",
        28 => "9",
        29 => "10",
        30 => "11",
        31 => "12",
        32 => "12L",
        33 => "13",
        34 => "14",
        35 => "15",
        36 => "16",
        _ => "",
    };
    if api.is_empty() {
        format!("API {sdk}")
    } else {
        format!("{api} (API {sdk})")
    }
}

/// Live install progress, polled from the daemon's install status.
fn installing<F>(
    t: &Palette,
    label: &str,
    status: Option<&InstallStatusDto>,
    on_result: F,
) -> AnyElement
where
    F: Fn(DialogResult, &mut Window, &mut gpui::App) + Clone + 'static,
{
    let (state, percent, bytes_done, bytes_total) = status
        .map(|s| (s.state.clone(), s.percent, s.bytes_done, s.bytes_total))
        .unwrap_or(("Pending".into(), 0, 0, 0));
    let stage = match state.as_str() {
        "Done" | "Failed" => state.clone(),
        _ => state.clone(),
    };
    let detail = if bytes_total > 0 {
        format!(
            "{} / {} — {}%",
            human_size(bytes_done),
            human_size(bytes_total),
            percent
        )
    } else {
        format!("{percent}%")
    };

    card(
        t,
        CARD_W,
        vec![
            header(
                t,
                names::SOFTWARE_INSTALL,
                ui::icon_tint(names::SOFTWARE_INSTALL, t),
                "Installing",
            )
            .into_any_element(),
            detail_table(
                t,
                vec![("Package", label.to_string()), ("Stage", stage.clone())],
                110.0,
            ),
            div()
                .px(px(18.0))
                .pb(px(6.0))
                .child(ui::progress_bar(t, percent as f32 / 100.0, 6.0))
                .into_any_element(),
            div()
                .px(px(18.0))
                .pb(px(16.0))
                .text_size(t.text(12.0))
                .text_color(t.text_muted)
                .child(detail)
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

/// The batch card: one row per APK, with per-item state pills.
fn sideload_batch<F>(t: &Palette, items: &[BatchItem], device: &str, on_result: F) -> AnyElement
where
    F: Fn(DialogResult, &mut Window, &mut gpui::App) + Clone + 'static,
{
    let queued = items
        .iter()
        .filter(|i| matches!(i.state, ItemState::Queued))
        .count();
    let running = items
        .iter()
        .any(|i| matches!(i.state, ItemState::Running { .. }));

    let mut rows: Vec<AnyElement> = vec![
        header(
            t,
            names::SOFTWARE_INSTALL,
            ui::icon_tint(names::SOFTWARE_INSTALL, t),
            "Install APKs",
        )
        .into_any_element(),
        body_text(
            t,
            &format!("{} dropped — install {queued} on {device}?", items.len()),
        )
        .into_any_element(),
    ];

    // One row per item: name, size, state pill. Bigger batches scroll.
    rows.push(
        div()
            .id("batch-list")
            .px(px(18.0))
            .pb(px(12.0))
            .max_h(px(260.0))
            .overflow_y_scroll()
            .children(items.iter().map(|item| {
                let (pill_text, tone) = item.state.pill();
                let (fg, bg) = ui::tone_colors(t, tone);
                let sub = match &item.state {
                    ItemState::Failed { message, .. } => message.clone().unwrap_or_default(),
                    _ => String::new(),
                };
                div()
                    .flex()
                    .items_center()
                    .gap(px(10.0))
                    .py(px(3.0))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_size(t.text(13.0))
                            .text_color(t.text_primary)
                            .overflow_hidden()
                            .text_ellipsis()
                            .child(item.name.clone()),
                    )
                    .child(
                        div()
                            .text_size(t.text(11.0))
                            .text_color(t.text_muted)
                            .child(human_size(
                                std::fs::metadata(&item.local).map(|m| m.len()).unwrap_or(0),
                            )),
                    )
                    .child(ui::pill(t, pill_text, fg, bg))
                    .when(!sub.is_empty(), |d| {
                        d.child(
                            div()
                                .text_size(t.text(11.0))
                                .text_color(t.text_muted)
                                .child(sub),
                        )
                    })
            }))
            .into_any_element(),
    );

    let mut row = vec![Button::normal("cancel", DialogResult::Dismiss)];
    if !running {
        if queued > 0 {
            row.push(Button::normal("copy-all", DialogResult::CopyAll));
            row.push(Button::primary("install-all", DialogResult::InstallAll));
        } else {
            // Everything finished or failed; closing is the only action.
        }
    }
    rows.push(buttons(t, row, on_result).into_any_element());

    card(t, WIDE_W, rows)
}

/// The recovery card: classified advice plus the actions that can fix it.
#[allow(clippy::too_many_arguments)]
fn install_failed<F>(
    t: &Palette,
    label: &str,
    code: Option<&str>,
    message: Option<&str>,
    raw: Option<&str>,
    device: &str,
    package: Option<&str>,
    path: &str,
    from_device: bool,
    on_result: F,
) -> AnyElement
where
    F: Fn(DialogResult, &mut Window, &mut gpui::App) + Clone + 'static,
{
    let mut rows: Vec<AnyElement> = vec![
        header(t, names::DIALOG_ERROR, t.danger, "Could not install").into_any_element(),
        body_text(t, &format!("{label} on {device}")).into_any_element(),
    ];
    if let Some(code) = code {
        rows.push(
            div()
                .px(px(18.0))
                .pb(px(8.0))
                .text_size(t.text(11.0))
                .text_color(t.text_muted)
                .child(format!("Error kind: {code}"))
                .into_any_element(),
        );
    }
    if let Some(message) = message
        && !message.is_empty()
    {
        rows.push(body_text(t, message).into_any_element());
    }
    if let Some(raw) = raw {
        rows.push(
            div()
                .px(px(18.0))
                .pb(px(12.0))
                .text_size(t.text(11.0))
                .text_color(t.text_muted)
                .whitespace_normal()
                .child(raw.to_string())
                .into_any_element(),
        );
    }

    let mut row = vec![Button::normal("close", DialogResult::Dismiss)];
    // Recovery actions, most specific first.
    if let Some(package) = package {
        row.push(Button::normal_owned(
            "Uninstall and retry".into(),
            DialogResult::UninstallAndRetry {
                device: device.to_string(),
                package: package.to_string(),
                path: path.to_string(),
                from_device,
            },
        ));
    }
    if !from_device {
        row.push(Button::normal_owned(
            "Copy instead".into(),
            DialogResult::CopyApk(PathBuf::from(path)),
        ));
    }
    rows.push(buttons(t, row, on_result).into_any_element());

    card(t, WIDE_W, rows)
}

// ── Debloat ──────────────────────────────────────────────────────────────────

/// The card that says what is about to happen to a batch of packages.
///
/// This is the last stop before the phone changes, so it says what will
/// change, what the worst of it is, and what cannot be taken back. It does
/// not pretend that all of it can.
#[allow(clippy::too_many_arguments)]
fn debloat_confirm<F>(
    t: &Palette,
    device: &str,
    action: PackageActionDto,
    actions: &[PackageRequestDto],
    packages: &[DebloatPackage],
    breaks: &[String],
    undo: bool,
    on_result: F,
) -> AnyElement
where
    F: Fn(DialogResult, &mut Window, &mut gpui::App) + Clone + 'static,
{
    let count = packages.len();
    let verb = action.label().to_lowercase();

    let mut rows: Vec<AnyElement> = vec![
        header(
            t,
            names::PACKAGE,
            ui::icon_tint(names::PACKAGE, t),
            if undo {
                "Put these back"
            } else {
                "Change these packages?"
            },
        )
        .into_any_element(),
        body_text(
            t,
            &if undo {
                format!("{count} package(s) will be put back on {device}.")
            } else {
                format!("{count} package(s) will be {verb}d on {device}.")
            },
        )
        .into_any_element(),
    ];

    // The risks, worst first. The tier words are the curated list's own
    // assessment, and they are the reason the card exists.
    let unsafe_count = packages
        .iter()
        .filter(|p| p.risk == Some(Removal::Unsafe))
        .count();
    let expert_count = packages
        .iter()
        .filter(|p| p.risk == Some(Removal::Expert))
        .count();
    if unsafe_count > 0 {
        rows.push(
            div()
                .mx(px(18.0))
                .mb(px(8.0))
                .px(px(10.0))
                .py(px(8.0))
                .rounded(px(6.0))
                .bg(t.danger_soft)
                .border_1()
                .border_color(t.danger_border)
                .text_size(t.text(11.5))
                .text_color(t.danger)
                .child(format!(
                    "{unsafe_count} of these can break parts of the phone. A factory reset \
                     may be needed to fix it. {}",
                    Removal::Unsafe.explanation()
                ))
                .into_any_element(),
        );
    }
    if expert_count > 0 {
        rows.push(
            div()
                .mx(px(18.0))
                .mb(px(8.0))
                .px(px(10.0))
                .py(px(8.0))
                .rounded(px(6.0))
                .bg(t.warning_soft)
                .text_size(t.text(11.5))
                .text_color(t.warning)
                .child(format!(
                    "{expert_count} of these break functionality you probably use, though \
                     the phone should still boot. {}",
                    Removal::Expert.explanation()
                ))
                .into_any_element(),
        );
    }
    // Apps that need one of these and are not in the selection: they would
    // be left broken.
    if !breaks.is_empty() {
        rows.push(
            div()
                .mx(px(18.0))
                .mb(px(8.0))
                .text_size(t.text(11.5))
                .text_color(t.text_muted)
                .child(format!(
                    "{} other app(s) need one of these and will stop working: {}.",
                    breaks.len(),
                    breaks.join(", ")
                ))
                .into_any_element(),
        );
    }
    // The one thing that cannot be done at all on a recent Android.
    if action == PackageActionDto::Uninstall {
        rows.push(
            div()
                .mx(px(18.0))
                .mb(px(8.0))
                .text_size(t.text(11.5))
                .text_color(t.text_muted)
                .child(
                    "Android 17 and later do not let ADB remove a system app. Removing it \
                     needs root.",
                )
                .into_any_element(),
        );
    }
    if action == PackageActionDto::Disable {
        rows.push(
            div()
                .mx(px(18.0))
                .mb(px(8.0))
                .text_size(t.text(11.5))
                .text_color(t.text_muted)
                .child(
                    "Android 15 and later do not let ADB switch a system app off. If the \
                     phone refuses, removing it for this user is the way round it.",
                )
                .into_any_element(),
        );
    }

    // The packages themselves, with the risk pill beside each.
    rows.push(
        div()
            .id("debloat-list")
            .px(px(18.0))
            .pb(px(12.0))
            .max_h(px(220.0))
            .overflow_y_scroll()
            .children(packages.iter().map(|package| {
                let (fg, bg) = match package.risk {
                    Some(Removal::Recommended) => ui::tone_colors(t, StateTone::Success),
                    Some(Removal::Advanced) => ui::tone_colors(t, StateTone::Active),
                    Some(Removal::Expert) => ui::tone_colors(t, StateTone::Warning),
                    Some(Removal::Unsafe) => ui::tone_colors(t, StateTone::Danger),
                    None => ui::tone_colors(t, StateTone::Neutral),
                };
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .py(px(2.0))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .child(
                                div()
                                    .text_size(t.text(12.0))
                                    .text_color(t.text_primary)
                                    .font_family(theme::MONO)
                                    .truncate()
                                    .child(package.id.clone()),
                            )
                            .when_some(package.description.clone(), |d, description| {
                                d.child(
                                    div()
                                        .text_size(t.text(10.5))
                                        .text_color(t.text_muted)
                                        .truncate()
                                        .child(description),
                                )
                            }),
                    )
                    .when_some(package.risk, |d, risk| {
                        d.child(ui::pill(t, risk.label(), fg, bg))
                    })
            }))
            .into_any_element(),
    );

    let mut row = vec![Button::normal("cancel", DialogResult::Dismiss)];
    let result = DialogResult::DebloatApply {
        actions: actions.to_vec(),
    };
    // A selection that can break the phone gets a button that says so.
    let label = format!("{} {count}", action.label());
    if packages
        .iter()
        .any(|p| p.risk.is_some_and(|risk| risk.needs_confirmation()))
    {
        row.push(Button::danger_owned(label, result));
    } else {
        row.push(Button::primary_owned(label, result));
    }
    rows.push(buttons(t, row, on_result).into_any_element());

    card(t, WIDE_W, rows)
}

/// The card shown while a batch of changes is in flight.
fn debloat_working<F>(t: &Palette, count: usize, _on_result: F) -> AnyElement
where
    F: Fn(DialogResult, &mut Window, &mut gpui::App) + Clone + 'static,
{
    card(
        t,
        CARD_W,
        vec![
            header(
                t,
                names::PACKAGE,
                ui::icon_tint(names::PACKAGE, t),
                "Working",
            )
            .into_any_element(),
            body_text(t, &format!("Changing {count} package(s)…")).into_any_element(),
            div()
                .px(px(18.0))
                .pb(px(16.0))
                .child(ui::progress_bar(t, 0.5, 6.0))
                .into_any_element(),
        ],
    )
}

/// The card that says what a batch of changes actually did.
fn debloat_result<F>(
    t: &Palette,
    action: PackageActionDto,
    done: &[String],
    failed: &[DebloatFailure],
    on_result: F,
) -> AnyElement
where
    F: Fn(DialogResult, &mut Window, &mut gpui::App) + Clone + 'static,
{
    let verb = action.past();
    let mut rows: Vec<AnyElement> = vec![
        header(
            t,
            if failed.is_empty() {
                names::CHECKBOX_CHECKED
            } else {
                names::DIALOG_WARNING
            },
            if failed.is_empty() {
                t.success
            } else {
                t.warning
            },
            if failed.is_empty() {
                "Done"
            } else {
                "Partly done"
            },
        )
        .into_any_element(),
        body_text(
            t,
            &if failed.is_empty() {
                format!("{verb} {} package(s).", done.len())
            } else {
                format!(
                    "{verb} {} package(s); {} could not be changed.",
                    done.len(),
                    failed.len()
                )
            },
        )
        .into_any_element(),
    ];

    if !failed.is_empty() {
        rows.push(
            div()
                .id("debloat-failures")
                .px(px(18.0))
                .pb(px(10.0))
                .max_h(px(200.0))
                .overflow_y_scroll()
                .children(failed.iter().map(|failure| {
                    div()
                        .flex()
                        .flex_col()
                        .py(px(3.0))
                        .gap(px(1.0))
                        .child(
                            div()
                                .text_size(t.text(12.0))
                                .text_color(t.text_primary)
                                .font_family(theme::MONO)
                                .child(failure.id.clone()),
                        )
                        .when_some(failure.message.clone(), |d, message| {
                            d.child(
                                div()
                                    .text_size(t.text(11.0))
                                    .text_color(t.text_muted)
                                    .child(message),
                            )
                        })
                }))
                .into_any_element(),
        );
    }

    rows.push(
        buttons(
            t,
            vec![Button::normal("close", DialogResult::Dismiss)],
            on_result,
        )
        .into_any_element(),
    );

    card(t, WIDE_W, rows)
}

/// A dialog button.
///
/// The label is kept separate from the element id so a label built at runtime
/// never has to be leaked to obtain a `'static` id.
enum Button {
    Normal {
        id: &'static str,
        label: SharedString,
        result: DialogResult,
        /// The filled variant, for the action the dialog is asking about.
        primary: bool,
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
    /// A secondary button, or an accepting one with `primary` set.
    fn normal(id: &'static str, result: DialogResult) -> Self {
        Button::Normal {
            id,
            label: label_for(id),
            result,
            primary: false,
        }
    }

    /// The filled variant, for the action this dialog is asking about.
    ///
    /// Every dialog that has an accepting action states it this way and puts
    /// it rightmost: "Create", "Delete", "Next", "Pair". The three that did
    /// not — the APK prompt's Install, the paired prompt's Connect, the image
    /// preview's Open externally — offered no filled button at all, so the one
    /// choice the dialog exists for looked like any of its alternatives.
    fn primary(id: &'static str, result: DialogResult) -> Self {
        Button::Normal {
            id,
            label: label_for(id),
            result,
            primary: true,
        }
    }

    /// A secondary button whose label is only known at runtime.
    fn normal_owned(label: String, result: DialogResult) -> Self {
        // Only one such button is ever on screen, so a fixed id cannot collide.
        Button::Normal {
            id: "extra",
            label: label.into(),
            result,
            primary: false,
        }
    }

    /// The filled variant, for a runtime label.
    fn primary_owned(label: String, result: DialogResult) -> Self {
        Button::Normal {
            id: "extra",
            label: label.into(),
            result,
            primary: true,
        }
    }

    /// A destructive button.
    fn danger_owned(label: String, result: DialogResult) -> Self {
        Self::Danger {
            id: "extra",
            label: label.into(),
            result,
        }
    }

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
                .text_size(t.text(18.0))
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
        .text_size(t.text(14.0))
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
                Button::Normal {
                    id,
                    label,
                    result,
                    primary,
                } => ui::button(t, ui::el_id(format!("{id}-{index}")), label, primary, {
                    let on_result = on_result.clone();
                    move |_, w, cx| on_result(result.clone(), w, cx)
                })
                .into_any_element(),
                Button::Danger { id, label, result } => {
                    ui::danger_button(t, ui::el_id(format!("{id}-{index}")), label, {
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
            header(t, icon, ui::icon_tint(icon, t), title).into_any_element(),
            div()
                .px(px(18.0))
                .pb(px(12.0))
                .text_size(t.text(14.0))
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
                        .text_size(t.text(14.0))
                        .text_color(t.text_muted)
                        .child(label.to_string()),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_size(t.text(14.0))
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
            header(
                t,
                names::DIALOG_INFORMATION,
                ui::icon_tint(names::DIALOG_INFORMATION, t),
                "Properties",
            )
            .into_any_element(),
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
            // Placeholder: the row below is rebuilt per step, because only it
            // knows which text field the forward button has to read.
            Vec::new(),
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
                // The step the user drove this flow to reach.
                Button::primary("connect", DialogResult::ConnectWireless(address.clone())),
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
                .text_size(t.text(11.0))
                .line_height(relative(1.5))
                .text_color(t.text_primary)
                .whitespace_normal()
                .child(text.clone())
                .into_any_element(),
            buttons(
                t,
                vec![
                    Button::normal("close", DialogResult::Dismiss),
                    // Not `normal_owned`: the rule is that the accepting action
                    // is the filled, rightmost button, and copying the report is
                    // what this dialog is for.
                    Button::primary_owned(
                        "Copy report".into(),
                        DialogResult::CopyDiagnostics(text),
                    ),
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
///
/// Kept honest against the binding tables by construction rather than by
/// memory: the rows here name the keys `crate::views::browser::BINDINGS` and
/// `crate::BINDINGS` actually register, so a new binding that is not added
/// here is a shortcut the user cannot discover — which is how "Open in
/// terminal" came to promise Alt+T while Alt+T switched the view.
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
            ("Install APK", "Ctrl+I"),
            ("Debloat phone", "Ctrl+Shift+D"),
            ("Save to computer", "Ctrl+Shift+C"),
            ("Open in terminal", "Context menu"),
        ],
    ),
    (
        "View",
        &[
            ("Toggle sidebar", "F9 / Ctrl+B"),
            ("Switch grid / list", "Alt+T"),
            ("Zoom out / in", "Ctrl+minus / Ctrl+plus"),
            ("Search this folder", "Ctrl+F"),
            ("Show hidden files", "Overflow menu"),
        ],
    ),
    (
        // Bound in `app::BINDINGS`, and previously missing from this list
        // entirely, which left two working shortcuts undocumentable.
        "Connection",
        &[
            ("Connect via Wi-Fi", "Shift+F10"),
            ("Connection diagnostics", "Ctrl+,"),
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
                .text_size(t.text(10.0))
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
                            .text_size(t.text(14.0))
                            .text_color(t.text_primary)
                            .child(*label),
                    )
                    .child(
                        div()
                            .font_family(theme::MONO)
                            .text_size(t.text(11.0))
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
                ui::icon_tint(names::DIALOG_INFORMATION, t),
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
            header(
                t,
                names::IMAGE_GENERIC,
                ui::icon_tint(names::IMAGE_GENERIC, t),
                name,
            )
            .into_any_element(),
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
                    Button::normal("close", DialogResult::Dismiss),
                    Button::primary(
                        "open externally",
                        DialogResult::OpenExternal(local.to_path_buf()),
                    ),
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
        // The Material Symbols are embedded in this binary under Apache-2.0,
        // which requires the notice to travel with them.
        ("Icons", "Material Symbols (Apache-2.0)".to_string()),
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
                        .child(icons::icon(
                            names::PHONE,
                            24.0,
                            ui::icon_tint(names::PHONE, t),
                        )),
                )
                .child(
                    div()
                        .text_size(t.text(20.0))
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(t.text_header)
                        .child("ADBShare"),
                )
                .child(
                    div()
                        .text_size(t.text(14.0))
                        .text_color(t.text_dim)
                        .child("Browse and transfer files over ADB"),
                )
                .into_any_element(),
            detail_table(t, rows, 76.0),
            // The Apache-2.0 notice for the embedded Material Symbols, rendered
            // rather than just named: §4 of the licence requires the notice to
            // accompany the work, and a binary that embeds 53 SVGs is a copy of
            // them. The Yaru folders are CC-BY-SA-4.0, which asks for the
            // credit; both licence texts stay in the repo.
            div()
                .px(px(18.0))
                .pb(px(12.0))
                .text_size(t.text(9.0))
                .font_family(theme::MONO)
                .text_color(t.text_muted)
                .child(icons::YARU_ATTRIBUTION)
                .child("\n\n")
                .child(icons::MATERIAL_ATTRIBUTION.trim())
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

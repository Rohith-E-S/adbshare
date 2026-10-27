//! The device's package inventory, and the four ways a package's state can be
//! changed.
//!
//! Everything here goes through `pm`, the package manager, over `adb shell`.
//! The verbs are the same ones the community debloat tools use:
//!
//! | Action     | Command                                    | Reversible by            |
//! |------------|--------------------------------------------|--------------------------|
//! | `Disable`  | `pm disable-user --user N <pkg>`          | `pm enable`              |
//! | `Enable`   | `pm enable --user N <pkg>`                | —                        |
//! | `Uninstall`| `pm uninstall -k --user N <pkg>`          | `cmd package install-existing` |
//! | `Restore`  | `cmd package install-existing <pkg>`      | —                        |
//!
//! Two of those deserve a note.
//!
//! `-k` on the uninstall keeps the app's data and the APK itself. Without it
//! the data is deleted and there is no way back, which is not what a user
//! asking to "remove an app" means when they can undo it.
//!
//! A system app cannot really be removed by an unprivileged caller: `pm
//! uninstall --user N` only unlinks it for that user, and the APK stays on
//! the read-only partition. Android 17 tightened this further and now
//! refuses outright, with `only root can delete system app for a particular
//! user`. That is why `Disable` is the action the GUI offers first: it does
//! what a user wants, always works, and can be undone.

use anyhow::{Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::time::Duration;

use super::{adb_command, adb_shell_with, is_package_name};

/// How long one `pm` call may take.
const PM_TIMEOUT: Duration = Duration::from_secs(20);

/// How many `pm` calls run at once. Each one is a subprocess, so this is
/// about not flooding the adb server as much as about speed.
const PM_CONCURRENCY: usize = 8;

/// How long a package inventory stays usable before it is read again.
pub const INVENTORY_TTL: Duration = Duration::from_secs(15);

/// Where a package stands, as the device reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PackageState {
    /// Installed and running.
    Enabled,
    /// Installed but switched off for this user.
    Disabled,
    /// Known to the system but not installed for this user.
    Uninstalled,
}

/// The four things that can be done to a package.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PackageAction {
    /// Take the app out of the launcher and stop it running. Reversible.
    Disable,
    /// Put a disabled app back. Reversible.
    Enable,
    /// Unlink the app for this user, keeping its data. Reversible.
    Uninstall,
    /// Re-link an app that was uninstalled for this user.
    Restore,
}

impl PackageAction {
    /// What this action does to a package's state.
    pub fn target_state(self) -> PackageState {
        match self {
            PackageAction::Disable => PackageState::Disabled,
            PackageAction::Enable | PackageAction::Restore => PackageState::Enabled,
            PackageAction::Uninstall => PackageState::Uninstalled,
        }
    }

    /// The `pm` command that carries this action out.
    ///
    /// The package name is single-quoted. It has already been checked
    /// against the platform's package-name grammar, so it cannot contain a
    /// quote or a shell metacharacter; the quotes are there so that a future
    /// relaxation of that grammar cannot turn into an injection.
    fn command(self, package: &str, user: u16) -> String {
        match self {
            PackageAction::Disable => {
                format!("pm disable-user --user {user} '{package}'")
            }
            PackageAction::Enable => format!("pm enable --user {user} '{package}'"),
            PackageAction::Uninstall => {
                format!("pm uninstall -k --user {user} '{package}'")
            }
            PackageAction::Restore => {
                format!("cmd package install-existing '{package}'")
            }
        }
    }
}

/// One package as the device reports it.
#[derive(Debug, Clone, Serialize)]
pub struct PackageRow {
    pub id: String,
    /// `enabled` | `disabled` | `uninstalled`
    pub state: PackageState,
    /// True when the package lives on the system's read-only partition.
    pub system: bool,
    /// Where the APK is, when it is installed at all.
    pub apk_path: Option<String>,
}

/// One request in a batch: a package and what to do to it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PackageRequest {
    pub package: String,
    pub action: PackageAction,
}

/// What happened to one package.
#[derive(Debug, Clone, Serialize)]
pub struct PackageResult {
    pub package: String,
    pub action: PackageAction,
    pub ok: bool,
    /// The state the package should now be in. Only set when the action
    /// worked; the caller can apply it without reading the device again.
    pub state: Option<PackageState>,
    /// A classified reason, e.g. `system_uninstall_blocked`.
    pub code: Option<String>,
    /// A sentence a person can act on.
    pub message: Option<String>,
    /// The untranslated `pm` output.
    pub raw: Option<String>,
}

// ── Reading the inventory ────────────────────────────────────────────────────

/// Every package the user can see, with its state.
pub async fn inventory(device: &str, user: u16) -> Result<Vec<PackageRow>> {
    // Four calls, because the package manager will not report the enabled
    // and the disabled set in one go. The result is cached by the caller.
    let all = shell(device, &format!("pm list packages -f -u --user {user}")).await?;
    let enabled = shell(device, &format!("pm list packages -e --user {user}")).await?;
    let disabled = shell(device, &format!("pm list packages -d --user {user}")).await?;
    let system = shell(device, &format!("pm list packages -s --user {user}")).await?;

    let enabled = ids(&enabled);
    let disabled = ids(&disabled);
    let system = ids(&system);

    let mut rows = Vec::new();
    for listed in parse_package_list(&all) {
        let state = if enabled.contains(&listed.id) {
            PackageState::Enabled
        } else if disabled.contains(&listed.id) {
            PackageState::Disabled
        } else {
            PackageState::Uninstalled
        };
        rows.push(PackageRow {
            system: system.contains(&listed.id),
            id: listed.id,
            state,
            apk_path: listed.path,
        });
    }
    rows.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(rows)
}

/// One line of `pm list packages`.
struct ListedPackage {
    id: String,
    path: Option<String>,
}

/// Parse the output of `pm list packages`.
///
/// With `-f` each line is `package:<apk path>=<package id>`; without it,
/// `package:<package id>`.
///
/// The split is on the *last* `=`, not the first. An installed app's path
/// is a directory under `/data/app`, and those directories have a hash in
/// their name that ends in `==`, so the first `=` in the line is usually in
/// the path rather than between the path and the package name. A package
/// name cannot contain an `=`, which is what makes the last one the right
/// one to split on.
fn parse_package_list(output: &str) -> Vec<ListedPackage> {
    output
        .lines()
        .filter_map(|line| {
            let rest = line.trim().strip_prefix("package:")?.trim();
            if let Some((path, id)) = rest.rsplit_once('=') {
                let (path, id) = (path.trim(), id.trim());
                if path.starts_with('/') && !id.is_empty() {
                    return Some(ListedPackage {
                        id: id.to_string(),
                        path: Some(path.to_string()),
                    });
                }
                return None;
            }
            Some(ListedPackage {
                id: rest.to_string(),
                path: None,
            })
        })
        .filter(|p| !p.id.is_empty())
        .collect()
}

/// Just the ids from a `pm list packages` output.
fn ids(output: &str) -> HashSet<String> {
    parse_package_list(output)
        .into_iter()
        .map(|p| p.id)
        .collect()
}

// ── Changing the state ───────────────────────────────────────────────────────

/// Apply a batch of actions, at most [`PM_CONCURRENCY`] at a time.
///
/// The results come back in the order the requests were given, whatever
/// order the calls finished in.
pub async fn apply_all(
    device: &str,
    user: u16,
    requests: Vec<PackageRequest>,
) -> Vec<PackageResult> {
    let mut slots: Vec<Option<PackageResult>> = (0..requests.len()).map(|_| None).collect();
    let mut running = tokio::task::JoinSet::new();
    let mut next = requests.into_iter().enumerate();

    // Keep at most PM_CONCURRENCY calls in flight: each one is an `adb
    // shell` subprocess, and the adb server does not like being flooded.
    for _ in 0..PM_CONCURRENCY {
        let Some((index, request)) = next.next() else {
            break;
        };
        running.spawn(one(device.to_string(), user, index, request));
    }
    while let Some(finished) = running.join_next().await {
        match finished {
            Ok((index, result)) => slots[index] = Some(result),
            Err(err) => tracing::warn!(error = %err, "a package action task ended badly"),
        }
        let Some((index, request)) = next.next() else {
            continue;
        };
        running.spawn(one(device.to_string(), user, index, request));
    }

    slots
        .into_iter()
        .map(|slot| {
            slot.unwrap_or_else(|| PackageResult {
                package: String::new(),
                action: PackageAction::Disable,
                ok: false,
                state: None,
                code: Some("not_run".into()),
                message: Some("the action did not run".into()),
                raw: None,
            })
        })
        .collect()
}

/// Run one action and describe what happened.
async fn one(
    device: String,
    user: u16,
    index: usize,
    request: PackageRequest,
) -> (usize, PackageResult) {
    let result = apply_one(&device, user, &request).await;
    (index, result)
}

/// Run one action.
async fn apply_one(device: &str, user: u16, request: &PackageRequest) -> PackageResult {
    let package = request.package.trim();
    if !is_package_name(package) {
        return PackageResult {
            package: package.to_string(),
            action: request.action,
            ok: false,
            state: None,
            code: Some("not_a_package_name".into()),
            message: Some(format!("{package} is not a package name.")),
            raw: None,
        };
    }
    let command = request.action.command(package, user);
    let output = match pm(device, &command).await {
        Ok(out) => out,
        Err(err) => {
            return PackageResult {
                package: package.to_string(),
                action: request.action,
                ok: false,
                state: None,
                code: Some("device_unreachable".into()),
                message: Some(err.to_string()),
                raw: None,
            };
        }
    };
    let trimmed = output.trim();
    if trimmed.is_empty() {
        // `pm` says nothing when it has nothing to complain about.
        return PackageResult {
            package: package.to_string(),
            action: request.action,
            ok: true,
            state: Some(request.action.target_state()),
            code: None,
            message: None,
            raw: None,
        };
    }
    if is_failure(trimmed) {
        let (code, message) = classify(trimmed);
        return PackageResult {
            package: package.to_string(),
            action: request.action,
            ok: false,
            state: None,
            code,
            message: Some(message),
            raw: Some(trimmed.to_string()),
        };
    }
    PackageResult {
        package: package.to_string(),
        action: request.action,
        ok: true,
        state: Some(request.action.target_state()),
        code: None,
        message: None,
        raw: Some(trimmed.to_string()),
    }
}

/// Whether `pm` reported a failure.
///
/// `pm` is inconsistent about exit codes: an old device returns success for
/// a failed uninstall, and some failures come back on stdout with a zero
/// status. The words are what has to be looked for.
fn is_failure(output: &str) -> bool {
    output.contains("Failure")
        || output.contains("Error")
        || output.contains("Exception")
        || output.contains("not installed for")
        || output.contains("Unknown package")
}

/// What a failed `pm` call meant, in code and in person.
///
/// The raw output is never thrown away; the ledger keeps it next to the
/// translation, so the card can quote it. The message is the first line of
/// the output at most, because the rest is a Java stack trace and nobody
/// wants to read one.
fn classify(output: &str) -> (Option<String>, String) {
    if output.contains("only root can delete system app") {
        return (
            Some("system_uninstall_blocked".into()),
            "This Android version does not let ADB remove a system app. Removing it needs \
             root."
                .into(),
        );
    }
    if output.contains("Cannot disable system packages") {
        // Android 15 and later stop the shell from switching a system app
        // off. The app stays exactly where it is, so there is nothing to
        // disable and nothing to undo.
        return (
            Some("system_disable_blocked".into()),
            "This Android version does not let ADB switch a system app off. Only apps you \
             installed yourself can be changed."
                .into(),
        );
    }
    if output.contains("DELETE_FAILED_DEVICE_POLICY_MANAGER") {
        return (
            Some("blocked_by_policy".into()),
            "A device administrator or work profile blocks removing this app.".into(),
        );
    }
    if output.contains("DELETE_FAILED_OWNER_BLOCKED") {
        return (
            Some("blocked_by_owner".into()),
            "The device owner blocks removing this app.".into(),
        );
    }
    if output.contains("DELETE_FAILED_INTERNAL_ERROR") {
        return (
            Some("internal_error".into()),
            "The package manager refused without saying why. The app is probably in use \
             or is depended on by another app."
                .into(),
        );
    }
    if output.contains("not installed for") || output.contains("Unknown package") {
        return (
            Some("not_installed".into()),
            "The app is not installed for this user, so there was nothing to change.".into(),
        );
    }
    if output.contains("INSTALL_FAILED_VERSION_DOWNGRADE") {
        return (
            Some("version_downgrade".into()),
            "Restoring the app failed: a newer version is already installed.".into(),
        );
    }
    if output.contains("INSTALL_FAILED_UPDATE_INCOMPATIBLE") {
        return (
            Some("signature_mismatch".into()),
            "Restoring the app failed: its signature does not match the installed one.".into(),
        );
    }
    if output.contains("Unknown command") || output.contains("not found") {
        return (
            Some("unsupported".into()),
            "This Android version does not support that command.".into(),
        );
    }
    if output.contains("SecurityException") || output.contains("Exception occurred") {
        return (
            Some("refused".into()),
            format!("The package manager refused: {}", first_line(output)),
        );
    }
    (Some("unknown".into()), first_line(output))
}

/// The first meaningful line of a `pm` failure.
///
/// A refused action comes back as a Java exception with a stack trace
/// under it. The trace is kept in the raw output; the message a person
/// reads stops at the first line.
fn first_line(output: &str) -> String {
    output
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty() && !line.starts_with("at "))
        .unwrap_or("the package manager refused")
        .to_string()
}

// ── The shell ────────────────────────────────────────────────────────────────

/// Run one `pm` command and hand back everything it said.
///
/// The exit status is deliberately ignored. `pm` reports a refused action
/// by printing the reason and exiting non-zero, so the output is the answer
/// and the status is only noise. A phone that did not answer at all is
/// different: there is no output, and that is the one case that is an error.
async fn pm(device: &str, command: &str) -> Result<String> {
    let out = tokio::time::timeout(
        PM_TIMEOUT,
        adb_command()
            .args(["-s", device, "shell", command])
            .kill_on_drop(true)
            .output(),
    )
    .await
    .map_err(|_| anyhow!("the phone did not answer in time"))?
    .map_err(|err| anyhow!("adb shell failed: {err}"))?;
    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&out.stderr));
    if text.trim().is_empty() && !out.status.success() {
        bail!("the phone did not answer");
    }
    Ok(text)
}

/// Run one `adb shell` command and hand back its output.
async fn shell(device: &str, command: &str) -> Result<String> {
    adb_shell_with(device, command, PM_TIMEOUT)
        .await
        .map_err(|err| anyhow!("{err}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plain_listing_gives_ids() {
        let out = "package:com.example.one\npackage:com.example.two\n";
        let parsed = parse_package_list(out);
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].id, "com.example.one");
        assert!(parsed[0].path.is_none());
    }

    #[test]
    fn a_listing_with_paths_gives_both_halves() {
        // The order of the two halves is not worth relying on, so both are
        // accepted.
        let out = "package:/system/app/Foo.apk=com.example.foo\n";
        let parsed = parse_package_list(out);
        assert_eq!(parsed[0].id, "com.example.foo");
        assert_eq!(parsed[0].path.as_deref(), Some("/system/app/Foo.apk"));
    }

    #[test]
    fn a_path_with_an_equals_sign_in_it_still_splits() {
        // An installed app's directory is named with a hash that ends in
        // `==`, so the first `=` in the line is usually in the path. Splitting
        // on it gives a package name that is really a path.
        let out = "package:/data/app/~~X_NZadkdycyNk3u-O-Iv3Q==/\
                   android.autoinstalls.config.Xiaomi.model-hcFJP5AflM4N4mP6WzUkGg==/base.apk=\
                   android.autoinstalls.config.Xiaomi.model\n";
        let parsed = parse_package_list(out);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].id, "android.autoinstalls.config.Xiaomi.model");
        assert_eq!(
            parsed[0].path.as_deref(),
            Some(
                "/data/app/~~X_NZadkdycyNk3u-O-Iv3Q==/\
                 android.autoinstalls.config.Xiaomi.model-hcFJP5AflM4N4mP6WzUkGg==/base.apk"
            )
        );
    }

    #[test]
    fn lines_that_are_not_packages_are_skipped() {
        let out = "Error: unknown command\npackage:com.example.one\n";
        let parsed = parse_package_list(out);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].id, "com.example.one");
    }

    #[test]
    fn android_17_is_recognised_by_its_own_words() {
        let (code, message) =
            classify("Failure [only root can delete system app for a particular user]");
        assert_eq!(code.as_deref(), Some("system_uninstall_blocked"));
        assert!(message.contains("root"), "{message}");
    }

    #[test]
    fn a_policy_block_is_told_apart_from_a_generic_failure() {
        let (code, _) = classify("Failure [DELETE_FAILED_DEVICE_POLICY_MANAGER]");
        assert_eq!(code.as_deref(), Some("blocked_by_policy"));
    }

    #[test]
    fn a_phone_that_will_not_let_a_system_app_be_disabled_is_recognised() {
        // Android 15 and later refuse this from the shell, and say so in a
        // Java exception rather than a `Failure [...]` line.
        let output = "Exception occurred while executing 'disable-user':\n\
                      java.lang.SecurityException: Cannot disable system packages.\n\
                      \tat com.android.server.pm.PackageManagerServiceImpl\
                      .shouldRestrictEnabledSettingsChange(PackageManagerServiceImpl.java:1064)";
        let (code, message) = classify(output);
        assert_eq!(code.as_deref(), Some("system_disable_blocked"));
        assert!(message.contains("system app"), "{message}");
    }

    #[test]
    fn a_message_never_carries_a_java_stack_trace() {
        let output = "Exception occurred while executing 'disable-user':\n\
                      java.lang.SecurityException: Cannot disable system packages.\n\
                      \tat com.android.server.pm.PackageManagerServiceImpl\
                      .shouldRestrictEnabledSettingsChange(PackageManagerServiceImpl.java:1064)\n\
                      \tat com.android.server.pm.PackageManagerService.setEnabledSettings";
        let (_, message) = classify(output);
        assert!(
            !message.contains("\n"),
            "the message is one line: {message}"
        );
        assert!(!message.contains("at com.android"), "{message}");
    }

    #[test]
    fn pm_saying_nothing_means_it_worked() {
        // `pm disable-user` prints nothing when it has nothing to complain
        // about, and returns a zero status even when it does complain.
        assert!(!is_failure(""));
        assert!(is_failure("Failure [DELETE_FAILED_INTERNAL_ERROR]"));
        assert!(is_failure("Package com.example.one is not installed for 0"));
    }

    #[test]
    fn the_commands_are_the_reversible_ones() {
        // Every verb here has to be undoable, because the GUI offers an undo
        // and because an uninstall without `-k` cannot be taken back.
        let disable = PackageAction::Disable.command("com.example.app", 0);
        assert!(disable.contains("disable-user"));
        assert!(disable.contains("--user 0"));

        let uninstall = PackageAction::Uninstall.command("com.example.app", 0);
        assert!(uninstall.contains("uninstall"));
        assert!(uninstall.contains("-k"), "the uninstall must keep the data");

        let restore = PackageAction::Restore.command("com.example.app", 0);
        assert!(restore.contains("install-existing"));
    }
}

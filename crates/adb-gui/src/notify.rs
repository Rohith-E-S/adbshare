//! Desktop notifications for device arrivals and departures.
//!
//! The GTK build used `gio::Notification`. Dropping GLib took that with it, so
//! connect and disconnect only ever produced an in-app toast — which is exactly
//! what a user misses, because the window they are looking at is usually not
//! the window the phone was plugged into.
//!
//! The XDG notification portal is the portable answer and needs no new native
//! dependency: `ashpd` is already a dependency for the file chooser, and the
//! portal works the same on Wayland and X11 under a plain desktop session. With
//! no portal running this is a silent no-op, which is the right failure mode for
//! a notification.
//!
//! The portal answers over D-Bus, so every send goes to a worker thread and
//! nothing waits on the result.

use ashpd::desktop::Icon;
use ashpd::desktop::notification::{Notification, NotificationProxy};

/// The identifier the portal groups notifications under. It has to match the
/// caller's app id, which is what the desktop file declares.
const NOTIFICATION_ID: &str = "org.adbshare.Gui";

/// The desktop entry's icon name, so a notification carries the app's identity
/// rather than a generic placeholder. Resolved by the desktop from the theme,
/// which also keeps this working from a source checkout.
const ICON_NAME: &str = "org.adbshare.Gui";

/// Whether a desktop session is even present to show notifications.
pub fn available() -> bool {
    std::env::var_os("WAYLAND_DISPLAY").is_some() || std::env::var_os("DISPLAY").is_some()
}

/// Show a notification titled `title` with `body` as its text.
pub fn send(title: &str, body: &str) {
    if !available() {
        return;
    }
    let title = title.to_string();
    let body = body.to_string();
    std::thread::spawn(move || {
        let _ = send_blocking(&title, &body);
    });
}

/// Do the portal round trip on the calling thread.
///
/// Split out from [`send`] so it can be tested directly: the spawn above exists
/// because this blocks, and a test that only ever went through the spawn could
/// not tell a working notification from a thread that hung on the first reply.
fn send_blocking(title: &str, body: &str) -> bool {
    // Resolving the proxy is itself a D-Bus round trip. The portal backend is
    // driven by zbus's own reactor thread, so parking here is fine — `block_on`
    // parks until that reactor wakes us.
    let Ok(proxy) = futures::executor::block_on(NotificationProxy::new()) else {
        return false;
    };
    let notification = Notification::new(title)
        .body(body)
        .icon(Icon::with_names([ICON_NAME]));
    futures::executor::block_on(proxy.add_notification(NOTIFICATION_ID, notification)).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn availability_is_a_plain_environment_check() {
        // A missing portal has to stay a cheap boolean with no I/O, so a headless
        // test run costs nothing here.
        let _ = available();
    }

    /// Ignored by default because it needs a session bus with a notification
    /// portal. Run it with `--ignored` on a desktop:
    ///
    /// ```sh
    /// cargo test -p adb-gui -- --ignored a_notification_reaches_the_portal
    /// ```
    ///
    /// It asserts the call *completes*, which is the failure mode worth catching:
    /// a `block_on` on a future zbus's reactor never wakes would hang here
    /// instead of silently leaking a thread per connect and disconnect.
    #[test]
    #[ignore = "needs a session bus with a notification portal"]
    fn a_notification_reaches_the_portal() {
        if !available() {
            return;
        }
        assert!(
            send_blocking("ADBShare", "notification smoke test"),
            "the portal refused the notification"
        );
    }
}

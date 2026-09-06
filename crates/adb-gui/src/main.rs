//! GTK4 + libadwaita frontend for adbshare.
//!
//! Layout:
//! - Main window: AdwApplicationWindow with a NavigationSplitView.
//! - Sidebar: list of devices (D-Bus-discovered).
//! - Content: per-device view — transfer queue, file browser, settings.
//! - Header bar: refresh, about, settings.

mod app;
mod device_list;
mod file_browser;
mod transfer_dock;
mod transfer_view;

use clap::Parser;
use app::AdbshareApp;

/// Kept only so `adb-gui --version` / `--help` work before GTK loads.
/// (The old, advertised-but-ignored `--pair` flag was removed: no pairing
/// wizard is implemented and silently discarding it was misleading.)
#[derive(Parser, Debug)]
#[command(name = "adb-gui", version, about = "GUI frontend for adbshare")]
struct Cli {}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"))
        )
        .init();

    // Parse CLI before initializing GTK so --version/--help don't pull in GLib.
    let _cli = Cli::parse();

    let app = AdbshareApp::new();
    app.run()
}

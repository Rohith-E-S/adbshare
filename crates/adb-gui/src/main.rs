//! GPUI frontend for adbshare.
//!
//! Layout:
//! - One window: a top bar over a resizable sidebar/content split.
//! - Sidebar: devices discovered over the session D-Bus, plus place shortcuts.
//! - Content: a file browser that can point at a phone or at the local disk.
//!
//! All backend work goes to `adb-daemon` over the session bus; see [`daemon`].
//! The GUI never talks to ADB directly.

mod app;
mod browser;
mod clipboard;
mod daemon;
mod dialogs;
mod filechooser;
mod icons;
mod localfs;
mod menu;
mod prefs;
mod protocol;
mod textinput;
mod theme;
mod toast;
mod ui;

use clap::Parser;
use gpui::{App, Application, Bounds, WindowBounds, WindowOptions, px, size};

use icons::AdbShareAssets;
use protocol::TransferPolicy;

/// Kept so `adb-gui --version` / `--help` work before the window opens.
#[derive(Parser, Debug)]
#[command(name = "adb-gui", version, about = "GPUI frontend for adbshare")]
struct Cli {}

/// The initial window size, matching the GTK build.
const WINDOW_W: f32 = 1000.0;
const WINDOW_H: f32 = 680.0;
/// The smallest useful window, also from the GTK build.
const WINDOW_MIN_W: f32 = 360.0;
const WINDOW_MIN_H: f32 = 400.0;

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let _cli = Cli::parse();

    Application::new()
        .with_assets(AdbShareAssets)
        .run(|cx: &mut App| {
            theme::install(cx);
            cx.set_global(TransferPolicy::default());
            daemon::start(cx);

            // Key bindings are app-global, so they are registered once here
            // rather than per view.
            app::install_key_bindings(cx);
            browser::install_key_bindings(cx);
            textinput::install_key_bindings(cx);

            let bounds = Bounds::centered(None, size(px(WINDOW_W), px(WINDOW_H)), cx);
            cx.open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    titlebar: Some(gpui::TitlebarOptions {
                        title: Some("ADBShare Files".into()),
                        ..Default::default()
                    }),
                    window_min_size: Some(size(px(WINDOW_MIN_W), px(WINDOW_MIN_H))),
                    ..Default::default()
                },
                app::AdbShareApp::build,
            )
            .expect("failed to open the main window");

            // `ADB_GUI_AUTOCLOSE_MS` is a smoke-test hook: quit after N
            // milliseconds, so a build can be launched and watched for panics.
            if let Some(ms) = autoclose_delay() {
                cx.spawn(async move |cx| {
                    gpui::Timer::after(ms).await;
                    cx.update(|app| app.quit()).ok();
                })
                .detach();
            }

            cx.activate(true);
        });

    Ok(())
}

/// Read the auto-close hook, if it is set to a valid duration.
fn autoclose_delay() -> Option<std::time::Duration> {
    let ms = std::env::var("ADB_GUI_AUTOCLOSE_MS").ok()?;
    let ms = ms.parse::<u64>().ok()?;
    Some(std::time::Duration::from_millis(ms))
}

//! The device tools view: everything adbshare can ask a phone to do that is
//! not file transfer.
//!
//! A pane, like the debloat view: device health, screenshots, the app
//! manager, quick settings, display scaling, logcat, port forwards,
//! recording, and input. The view does its own daemon calls (through
//! [`crate::state::daemon`]) and shows a confirm strip before anything
//! destructive — reboot, force-stop, clear data — because a personal phone
//! is one tap away from losing app data.
//!
//! Read-only operations (health, logcat, package list, screenshot) need no
//! confirmation. Nothing here debloats or uninstalls; that stays in the
//! debloat view with its ledger.

use gpui::prelude::*;
use gpui::{AnyElement, Entity, Styled, Window, div, px};

use crate::protocol::{DeviceInfoDto, PackageDto};
use crate::state::daemon;
use crate::theme::{self, Palette, Themed};
use crate::views::textinput::TextField;
use crate::views::ui;

/// One thing the user asked for, waiting for a confirmation tap.
#[derive(Debug, Clone)]
enum Pending {
    Reboot(String),
    ForceStop(String),
    ClearData(String),
}

/// The tools view.
pub struct Tools {
    device: Option<String>,
    device_label: String,
    info: Option<DeviceInfoDto>,
    apps: Vec<PackageDto>,
    selected_app: Option<String>,
    apps_query: String,
    apps_search: Entity<TextField>,
    loading: bool,
    error: Option<String>,
    status: Option<String>,
    /// Last shell output, for the output section.
    output: String,
    pending: Option<Pending>,
    // Quick settings.
    wifi: bool,
    data: bool,
    bluetooth: bool,
    airplane: bool,
    // Display.
    display_size: String,
    display_density: String,
    // Logcat.
    logcat_running: bool,
    logcat_cursor: u64,
    logcat_lines: Vec<daemon::LogcatLine>,
    // Forwards / input fields.
    forwards: String,
    tmp_port: Entity<TextField>,
    tmp_remote: Entity<TextField>,
    tmp_perm: Entity<TextField>,
    // Capture.
    recording: bool,
    // Input coordinates.
    tap_x: Entity<TextField>,
    tap_y: Entity<TextField>,
    swipe: Entity<TextField>,
}

impl Tools {
    pub fn new(cx: &mut Context<Self>) -> Self {
        Self {
            device: None,
            device_label: String::new(),
            info: None,
            apps: Vec::new(),
            selected_app: None,
            apps_query: String::new(),
            apps_search: cx.new(|cx| TextField::new(cx, "Filter packages")),
            loading: false,
            error: None,
            status: None,
            output: String::new(),
            pending: None,
            wifi: false,
            data: false,
            bluetooth: false,
            airplane: false,
            display_size: String::new(),
            display_density: String::new(),
            logcat_running: false,
            logcat_cursor: 0,
            logcat_lines: Vec::new(),
            forwards: String::new(),
            tmp_port: cx.new(|cx| TextField::new(cx, "8080")),
            tmp_remote: cx.new(|cx| TextField::new(cx, "localdevice:8080")),
            tmp_perm: cx.new(|cx| TextField::new(cx, "android.permission.CAMERA")),
            recording: false,
            tap_x: cx.new(|cx| TextField::new(cx, "540")),
            tap_y: cx.new(|cx| TextField::new(cx, "1200")),
            swipe: cx.new(|cx| TextField::new(cx, "540,1800 → 540,600")),
        }
    }

    pub fn set_device(&mut self, device: Option<String>, cx: &mut Context<Self>) {
        if self.device == device {
            return;
        }
        self.device = device;
        self.info = None;
        self.apps.clear();
        self.selected_app = None;
        self.error = None;
        self.pending = None;
        self.logcat_lines.clear();
        self.logcat_cursor = 0;
        self.logcat_running = false;
        if self.device.is_some() {
            self.reload(cx);
        }
    }

    pub fn set_device_label(&mut self, label: String, cx: &mut Context<Self>) {
        if self.device_label != label {
            self.device_label = label;
            cx.notify();
        }
    }

    /// The package filter box, so the app can watch it.
    pub fn apps_search_field(&self) -> &Entity<TextField> {
        &self.apps_search
    }

    /// Apply a package filter.
    pub fn set_apps_query(&mut self, query: String, cx: &mut Context<Self>) {
        if self.apps_query != query {
            self.apps_query = query;
            cx.notify();
        }
    }

    fn reload(&mut self, cx: &mut Context<Self>) {
        let Some(device) = self.device.clone() else {
            return;
        };
        self.loading = true;
        self.error = None;
        cx.notify();
        let me = cx.entity();
        cx.spawn(async move |_this, cx| {
            let info = daemon::device_info(&device).await;
            let apps = daemon::list_packages(&device, 0).await;
            let forwards = daemon::list_forwards().await;
            let toggles = read_toggles(&device).await;
            let display = read_display(&device).await;
            let _ = me.update(cx, |this, cx| {
                this.loading = false;
                match info {
                    Ok(dto) => this.info = Some(dto),
                    Err(e) => this.error = Some(e),
                }
                if let Ok(list) = apps {
                    this.apps = list;
                }
                if let Ok(text) = forwards {
                    this.forwards = text;
                }
                if let Ok((wifi, data, bt, airplane)) = toggles {
                    this.wifi = wifi;
                    this.data = data;
                    this.bluetooth = bt;
                    this.airplane = airplane;
                }
                if let Ok((size, density)) = display {
                    this.display_size = size;
                    this.display_density = density;
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn note(&mut self, msg: impl Into<String>, cx: &mut Context<Self>) {
        self.status = Some(msg.into());
        self.error = None;
        cx.notify();
    }

    fn fail(&mut self, err: impl Into<String>, cx: &mut Context<Self>) {
        self.error = Some(err.into());
        cx.notify();
    }

    /// Run the daemon call behind `action`, reporting the outcome.
    fn run<F, Fut>(&mut self, cx: &mut Context<Self>, done: &str, action: F)
    where
        F: FnOnce(String) -> Fut + 'static,
        Fut: Future<Output = Result<String, String>> + 'static,
    {
        let Some(device) = self.device.clone() else {
            return;
        };
        let done = done.to_string();
        let me = cx.entity();
        cx.spawn(async move |_this, cx| {
            let result = action(device).await;
            let _ = me.update(cx, |this, cx| match result {
                Ok(out) => {
                    this.output = out;
                    this.note(done, cx);
                }
                Err(e) => this.fail(e, cx),
            });
        })
        .detach();
    }

    fn shell(&mut self, cx: &mut Context<Self>, done: &str, words: Vec<&'static str>) {
        self.run(cx, done, move |device| async move {
            daemon::run_shell(&device, &words).await
        });
    }

    fn start_logcat(&mut self, cx: &mut Context<Self>) {
        let Some(device) = self.device.clone() else {
            return;
        };
        self.logcat_running = true;
        self.logcat_cursor = 0;
        self.logcat_lines.clear();
        cx.notify();
        let me = cx.entity();
        cx.spawn(async move |_this, cx| {
            if let Err(e) = daemon::start_logcat(&device).await {
                let _ = me.update(cx, |this, cx| {
                    this.logcat_running = false;
                    this.fail(e, cx);
                });
                return;
            }
            // Poll until the view stops asking. `gpui::Timer`, not tokio's:
            // this task runs on GPUI's executor, which has no tokio reactor,
            // and `tokio::time::sleep` panics without one.
            loop {
                gpui::Timer::after(std::time::Duration::from_millis(600)).await;
                let running = me.read_with(cx, |this, _cx| this.logcat_running);
                if !running.unwrap_or(false) {
                    break;
                }
                let cursor = me
                    .read_with(cx, |this, _cx| this.logcat_cursor)
                    .unwrap_or(0);
                let (next, lines) = match daemon::logcat_lines(&device, cursor, 500).await {
                    Ok(v) => v,
                    Err(_) => break,
                };
                let _ = me.update(cx, |this, cx| {
                    this.logcat_cursor = next;
                    this.logcat_lines.extend(lines);
                    if this.logcat_lines.len() > 2000 {
                        let drain = this.logcat_lines.len() - 2000;
                        this.logcat_lines.drain(..drain);
                    }
                    cx.notify();
                });
            }
        })
        .detach();
    }

    fn stop_logcat(&mut self, cx: &mut Context<Self>) {
        self.logcat_running = false;
        if let Some(device) = self.device.clone() {
            let me = cx.entity();
            cx.spawn(async move |_this, cx| {
                let _ = daemon::stop_logcat(&device).await;
                let _ = me.update(cx, |this, cx| this.note("Logcat stopped", cx));
            })
            .detach();
        }
        cx.notify();
    }
}

async fn read_toggles(device: &str) -> Result<(bool, bool, bool, bool), String> {
    let wifi = daemon::run_shell(device, &["settings", "get", "global", "wifi_on"]).await?;
    let data = daemon::run_shell(device, &["settings", "get", "global", "mobile_data"]).await?;
    let bt = daemon::run_shell(device, &["settings", "get", "global", "bluetooth_on"]).await?;
    let airplane =
        daemon::run_shell(device, &["settings", "get", "global", "airplane_mode_on"]).await?;
    let on = |s: &str| s.trim() == "1";
    Ok((on(&wifi), on(&data), on(&bt), on(&airplane)))
}

async fn read_display(device: &str) -> Result<(String, String), String> {
    let size = daemon::run_shell(device, &["wm", "size"]).await?;
    let density = daemon::run_shell(device, &["wm", "density"]).await?;
    Ok((size.trim().to_string(), density.trim().to_string()))
}

impl Render for Tools {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = *cx.theme();
        if self.device.is_none() {
            return div()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .size_full()
                .bg(t.canvas)
                .child(
                    div()
                        .text_size(t.text(15.0))
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(t.text_secondary)
                        .child("Pick a phone to see its tools"),
                )
                .child(
                    div()
                        .mt(px(6.0))
                        .text_size(t.text(12.0))
                        .text_color(t.text_muted)
                        .child(
                            "Health, apps, logcat, forwards and input live here once a device is selected.",
                        ),
                )
                .into_any_element();
        }

        let mut rows: Vec<AnyElement> = Vec::new();
        rows.push(self.header(&t, cx));
        // Fixed-height slots, always present: inserting a banner above the
        // sections as they appear would shift every row below it mid-task,
        // which is how a second tap lands on the wrong toggle.
        let banner: AnyElement = if let Some(err) = &self.error {
            status_row(&t, err, true)
        } else if let Some(status) = &self.status {
            status_row(&t, status, false)
        } else {
            status_row(&t, "", false)
        };
        rows.push(div().h(px(34.0)).child(banner).into_any_element());
        if let Some(pending) = self.pending.clone() {
            rows.push(confirm_row(&t, &pending, cx));
        }
        rows.push(section(&t, "Device", self.device_section(&t, cx)));
        rows.push(section(&t, "Apps", self.apps_section(&t, cx)));
        rows.push(section(&t, "Logcat", self.logcat_section(&t, cx)));
        rows.push(section(&t, "Capture & input", self.capture_section(&t, cx)));
        rows.push(section(&t, "Shell output", self.output_section(&t, cx)));

        div()
            .flex()
            .flex_col()
            .size_full()
            .bg(t.canvas)
            .child(ui::scroll_area(rows))
            .into_any_element()
    }
}

fn status_row(t: &Palette, text: &str, is_error: bool) -> AnyElement {
    div()
        .px(px(14.0))
        .py(px(7.0))
        .rounded(px(theme::RADIUS_CAPSULE_BTN))
        .when(!text.is_empty(), |d| {
            d.bg(if is_error {
                t.danger_soft
            } else {
                t.success_soft
            })
        })
        .text_size(t.text(12.0))
        .text_color(if is_error { t.danger } else { t.success })
        .child(text.to_string())
        .into_any_element()
}

fn section(t: &Palette, title: &str, body: Vec<AnyElement>) -> AnyElement {
    let mut col = vec![
        div()
            .text_size(t.text(13.0))
            .font_weight(gpui::FontWeight::SEMIBOLD)
            .text_color(t.text_header)
            .child(title.to_string())
            .into_any_element(),
    ];
    col.extend(body);
    div()
        .flex()
        .flex_col()
        .gap(px(8.0))
        .px(px(14.0))
        .py(px(12.0))
        .children(col)
        .into_any_element()
}

fn confirm_row(t: &Palette, pending: &Pending, cx: &mut Context<Tools>) -> AnyElement {
    let (label, detail) = match pending {
        Pending::Reboot(mode) => ("Reboot the phone?".to_string(), format!("Mode: {mode}")),
        Pending::ForceStop(pkg) => (
            "Force-stop this app?".to_string(),
            format!("{pkg} — unsaved work in the app may be lost."),
        ),
        Pending::ClearData(pkg) => (
            "Clear this app's data?".to_string(),
            format!("{pkg} — all of the app's data on the phone will be deleted."),
        ),
    };
    let me = cx.entity();
    div()
        .flex()
        .items_center()
        .gap(px(10.0))
        .px(px(12.0))
        .py(px(9.0))
        .rounded(px(theme::RADIUS_CAPSULE_BTN))
        .bg(t.warning_soft)
        .border_1()
        .border_color(t.danger_border)
        .child(
            div()
                .flex_1()
                .text_size(t.text(12.0))
                .text_color(t.text_primary)
                .child(format!("{label} {detail}")),
        )
        .child(ui::danger_button(t, "confirm-yes", "Do it", {
            let me = me.clone();
            move |_, _w, cx| {
                me.update(cx, |this, cx| this.do_pending(cx));
            }
        }))
        .child(ui::button(t, "confirm-no", "Cancel", false, {
            let me = me.clone();
            move |_, _w, cx| {
                me.update(cx, |this, cx| {
                    this.pending = None;
                    cx.notify();
                });
            }
        }))
        .into_any_element()
}

fn kv(t: &Palette, key: &str, value: String) -> AnyElement {
    div()
        .flex()
        .items_center()
        .gap(px(10.0))
        .child(
            div()
                .w(px(110.0))
                .text_size(t.text(11.5))
                .text_color(t.text_muted)
                .child(key.to_string()),
        )
        .child(
            div()
                .flex_1()
                .text_size(t.text(12.0))
                .text_color(t.text_primary)
                .child(value),
        )
        .into_any_element()
}

impl Tools {
    fn do_pending(&mut self, cx: &mut Context<Self>) {
        let Some(pending) = self.pending.take() else {
            return;
        };
        match pending {
            Pending::Reboot(mode) => {
                self.run(cx, "Rebooting…", move |device| async move {
                    daemon::reboot(&device, &mode).await.map(|_| String::new())
                });
            }
            Pending::ForceStop(pkg) => {
                self.run(
                    cx,
                    &format!("Force-stopped {pkg}"),
                    move |device| async move {
                        daemon::run_shell(&device, &["am", "force-stop", &pkg]).await
                    },
                );
            }
            Pending::ClearData(pkg) => {
                self.run(cx, &format!("Cleared {pkg}"), move |device| async move {
                    daemon::run_shell(&device, &["pm", "clear", &pkg]).await
                });
            }
        }
        cx.notify();
    }

    fn header(&mut self, t: &Palette, cx: &mut Context<Self>) -> AnyElement {
        div()
            .flex()
            .items_center()
            .gap(px(10.0))
            .px(px(14.0))
            .py(px(10.0))
            .child(
                div()
                    .text_size(t.text(15.0))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(t.text_header)
                    .child(format!("Tools — {}", self.device_label)),
            )
            .child(
                div()
                    .flex_1()
                    .text_size(t.text(11.0))
                    .text_color(t.text_muted)
                    .child(if self.loading {
                        "Reading the phone…".to_string()
                    } else {
                        String::new()
                    }),
            )
            .child(ui::button(t, "tools-refresh", "Refresh", false, {
                let me = cx.entity();
                move |_, _w, cx| {
                    me.update(cx, |this, cx| this.reload(cx));
                }
            }))
            .into_any_element()
    }

    fn device_section(&mut self, t: &Palette, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let mut rows: Vec<AnyElement> = Vec::new();
        if let Some(info) = &self.info {
            let mut pct = info
                .battery_pct
                .map(|v| format!("{v}%"))
                .unwrap_or_default();
            if let Some(health) = &info.battery_health {
                // `dumpsys battery` reports a number: 1 unknown, 2 good,
                // 3 overheat, 4 dead, 5 over voltage, 6 failure, 7 cold.
                let word = match health.as_str() {
                    "1" => "unknown",
                    "2" => "good",
                    "3" => "overheat",
                    "4" => "dead",
                    "5" => "over voltage",
                    "6" => "failure",
                    "7" => "cold",
                    other => other,
                };
                pct.push_str(&format!(" ({word})"));
            }
            if let Some(temp) = info.battery_temp_c {
                pct.push_str(&format!(", {temp}°C"));
            }
            if let Some(mv) = info.battery_voltage_mv {
                pct.push_str(&format!(", {:.1}V", mv as f32 / 1000.0));
            }
            rows.push(kv(t, "Battery", pct));
            if let (Some(used), Some(total)) = (info.storage_used, info.storage_total) {
                rows.push(kv(
                    t,
                    "Storage",
                    format!("{} of {} used", human(used), human(total)),
                ));
            }
            rows.push(kv(
                t,
                "Android",
                format!(
                    "{} (API {})",
                    info.android_version.as_deref().unwrap_or("?"),
                    info.api_level.map(|v| v.to_string()).unwrap_or_default()
                ),
            ));
            if let (Some(w), Some(h)) = (info.display_width, info.display_height) {
                rows.push(kv(t, "Display", format!("{w}×{h}")));
            }
        }
        // Quick settings toggles.
        rows.push(toggle_row(t, "Wi-Fi", self.wifi, cx, |on| {
            vec!["svc", "wifi", if on { "enable" } else { "disable" }]
        }));
        rows.push(toggle_row(t, "Mobile data", self.data, cx, |on| {
            vec!["svc", "data", if on { "enable" } else { "disable" }]
        }));
        rows.push(toggle_row(t, "Bluetooth", self.bluetooth, cx, |on| {
            vec!["svc", "bluetooth", if on { "enable" } else { "disable" }]
        }));
        rows.push(toggle_row(t, "Airplane mode", self.airplane, cx, |on| {
            vec![
                "settings",
                "put",
                "global",
                "airplane_mode_on",
                if on { "1" } else { "0" },
            ]
        }));
        // Display scale.
        rows.push(kv(t, "wm size", self.display_size.clone()));
        rows.push(kv(t, "wm density", self.display_density.clone()));
        // Actions.
        let me = cx.entity();
        rows.push(
            div()
                .flex()
                .flex_wrap()
                .gap(px(8.0))
                .child(ui::button(t, "tools-shot", "Screenshot", false, {
                    let me = me.clone();
                    move |_, _w, cx| {
                        me.update(cx, |this, cx| {
                            this.run(cx, "Screenshot saved", move |device| async move {
                                daemon::screenshot(&device).await
                            });
                        });
                    }
                }))
                .child(ui::button(t, "tools-bug", "Bug report", false, {
                    let me = me.clone();
                    move |_, _w, cx| {
                        me.update(cx, |this, cx| {
                            this.run(cx, "Bug report saved", move |device| async move {
                                daemon::bugreport(&device).await
                            });
                        });
                    }
                }))
                .child(ui::button(t, "tools-reboot", "Reboot…", false, {
                    let me = me.clone();
                    move |_, _w, cx| {
                        me.update(cx, |this, cx| {
                            this.pending = Some(Pending::Reboot(String::new()));
                            cx.notify();
                        });
                    }
                }))
                .into_any_element(),
        );
        rows
    }

    fn apps_section(&mut self, t: &Palette, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let mut rows: Vec<AnyElement> = Vec::new();
        // Filtered, capped list: 453 packages is not a scroll region, it is
        // a search box. The query narrows to matches; the cap keeps the
        // pane cheap when it is broad.
        let query = self.apps_query.to_ascii_lowercase();
        let visible: Vec<&PackageDto> = self
            .apps
            .iter()
            .filter(|p| p.state == "enabled" || p.state == "disabled")
            .filter(|p| query.is_empty() || p.id.to_ascii_lowercase().contains(&query))
            .take(50)
            .collect();
        rows.push(
            div()
                .flex()
                .items_center()
                .gap(px(10.0))
                .child(div().w(px(220.0)).child(self.apps_search.clone()))
                .child(
                    div()
                        .text_size(t.text(11.5))
                        .text_color(t.text_muted)
                        .child(format!(
                            "{} packages — select one to launch, stop, clear, or grant.",
                            self.apps.len()
                        )),
                )
                .into_any_element(),
        );
        for pkg in visible {
            let selected = self.selected_app.as_deref() == Some(pkg.id.as_str());
            let me = cx.entity();
            let id = pkg.id.clone();
            rows.push(
                div()
                    .id(ui::el_id(format!("tool-pkg:{}", pkg.id)))
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .px(px(8.0))
                    .py(px(4.0))
                    .rounded(px(theme::RADIUS_CAPSULE_BTN))
                    .cursor_pointer()
                    .when(selected, |d| d.bg(t.selected))
                    .hover(|s| s.bg(t.hover))
                    .child(
                        div()
                            .flex_1()
                            .text_size(t.text(12.0))
                            .text_color(t.text_primary)
                            .child(pkg.id.clone()),
                    )
                    .child(
                        div()
                            .text_size(t.text(10.5))
                            .text_color(t.text_muted)
                            .child(pkg.state.clone()),
                    )
                    .on_click(move |_, _w, cx| {
                        me.update(cx, |this, cx| {
                            this.selected_app = Some(id.clone());
                            cx.notify();
                        });
                    })
                    .into_any_element(),
            );
        }
        if let Some(pkg) = self.selected_app.clone() {
            let me = cx.entity();
            rows.push(
                div()
                    .flex()
                    .flex_wrap()
                    .gap(px(8.0))
                    .child(ui::button(t, "app-launch", "Launch", true, {
                        let me = me.clone();
                        let pkg = pkg.clone();
                        move |_, _w, cx| {
                            let pkg = pkg.clone();
                            me.update(cx, |this, cx| {
                                this.run(
                                    cx,
                                    &format!("Launched {pkg}"),
                                    move |device| async move {
                                        daemon::run_shell(
                                            &device,
                                            &[
                                                "monkey",
                                                "-p",
                                                &pkg,
                                                "-c",
                                                "android.intent.category.LAUNCHER",
                                                "1",
                                            ],
                                        )
                                        .await
                                    },
                                );
                            });
                        }
                    }))
                    .child(ui::button(t, "app-stop", "Force-stop…", false, {
                        let me = me.clone();
                        let pkg = pkg.clone();
                        move |_, _w, cx| {
                            let pkg = pkg.clone();
                            me.update(cx, |this, cx| {
                                this.pending = Some(Pending::ForceStop(pkg));
                                cx.notify();
                            });
                        }
                    }))
                    .child(ui::button(t, "app-clear", "Clear data…", false, {
                        let me = me.clone();
                        let pkg = pkg.clone();
                        move |_, _w, cx| {
                            let pkg = pkg.clone();
                            me.update(cx, |this, cx| {
                                this.pending = Some(Pending::ClearData(pkg));
                                cx.notify();
                            });
                        }
                    }))
                    .child(div().w(px(220.0)).child(self.tmp_perm.clone()))
                    .child(ui::button(t, "app-grant", "Grant", false, {
                        let me = me.clone();
                        let pkg = pkg.clone();
                        move |_, _w, cx| {
                            let pkg = pkg.clone();
                            me.update(cx, |this, cx| {
                                let perm = this.tmp_perm.read(cx).value().to_string();
                                this.run(
                                    cx,
                                    &format!("Granted {perm}"),
                                    move |device| async move {
                                        daemon::run_shell(&device, &["pm", "grant", &pkg, &perm])
                                            .await
                                    },
                                );
                            });
                        }
                    }))
                    .child(ui::button(t, "app-revoke", "Revoke", false, {
                        let me = me.clone();
                        let pkg = pkg.clone();
                        move |_, _w, cx| {
                            let pkg = pkg.clone();
                            me.update(cx, |this, cx| {
                                let perm = this.tmp_perm.read(cx).value().to_string();
                                this.run(
                                    cx,
                                    &format!("Revoked {perm}"),
                                    move |device| async move {
                                        daemon::run_shell(&device, &["pm", "revoke", &pkg, &perm])
                                            .await
                                    },
                                );
                            });
                        }
                    }))
                    .into_any_element(),
            );
        }
        rows
    }

    fn logcat_section(&mut self, t: &Palette, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let mut rows: Vec<AnyElement> = Vec::new();
        let me = cx.entity();
        rows.push(
            div()
                .flex()
                .items_center()
                .gap(px(8.0))
                .child(if self.logcat_running {
                    ui::danger_button(t, "logcat-stop", "Stop logcat", {
                        let me = me.clone();
                        move |_, _w, cx| {
                            me.update(cx, |this, cx| this.stop_logcat(cx));
                        }
                    })
                    .into_any_element()
                } else {
                    ui::button(t, "logcat-start", "Start logcat", true, {
                        let me = me.clone();
                        move |_, _w, cx| {
                            me.update(cx, |this, cx| this.start_logcat(cx));
                        }
                    })
                    .into_any_element()
                })
                .child(
                    div()
                        .text_size(t.text(11.0))
                        .text_color(t.text_muted)
                        .child(format!("{} lines buffered", self.logcat_lines.len())),
                )
                .into_any_element(),
        );
        // Last 200 lines, newest last, mono.
        let lines: Vec<AnyElement> = self
            .logcat_lines
            .iter()
            .rev()
            .take(200)
            .rev()
            .map(|(_, text)| {
                div()
                    .font_family(theme::MONO)
                    .text_size(t.text(10.0))
                    .text_color(t.text_dim)
                    .child(text.clone())
                    .into_any_element()
            })
            .collect();
        rows.push(
            div()
                .id("logcat-body")
                .flex()
                .flex_col()
                .gap(px(2.0))
                .max_h(px(260.0))
                .overflow_y_scroll()
                .children(lines)
                .into_any_element(),
        );
        rows
    }

    fn capture_section(&mut self, t: &Palette, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let mut rows: Vec<AnyElement> = Vec::new();
        let me = cx.entity();
        // Recording.
        rows.push(
            div()
                .flex()
                .items_center()
                .gap(px(8.0))
                .child(if self.recording {
                    ui::danger_button(t, "rec-stop", "Stop recording", {
                        let me = me.clone();
                        move |_, _w, cx| {
                            me.update(cx, |this, cx| {
                                this.run(cx, "Recording saved", move |device| async move {
                                    daemon::stop_record(&device).await
                                });
                                this.recording = false;
                            });
                        }
                    })
                    .into_any_element()
                } else {
                    ui::button(t, "rec-start", "Record screen (3 min)", false, {
                        let me = me.clone();
                        move |_, _w, cx| {
                            me.update(cx, |this, cx| {
                                this.run(cx, "Recording started", move |device| async move {
                                    daemon::start_record(&device).await.map(|_| String::new())
                                });
                                this.recording = true;
                            });
                        }
                    })
                    .into_any_element()
                })
                .into_any_element(),
        );
        // Forwards.
        rows.push(
            div()
                .flex()
                .items_center()
                .gap(px(8.0))
                .child(div().w(px(70.0)).child(self.tmp_port.clone()))
                .child(div().w(px(180.0)).child(self.tmp_remote.clone()))
                .child(ui::button(t, "fwd-add", "Add forward", false, {
                    let me = me.clone();
                    move |_, _w, cx| {
                        me.update(cx, |this, cx| {
                            let port: u16 = this.tmp_port.read(cx).value().parse().unwrap_or(0);
                            let remote = this.tmp_remote.read(cx).value().to_string();
                            this.run(cx, "Forward added", move |device| async move {
                                daemon::add_forward(&device, port, &remote)
                                    .await
                                    .map(|_| String::new())
                            });
                        });
                    }
                }))
                .child(ui::button(t, "fwd-remove", "Remove", false, {
                    let me = me.clone();
                    move |_, _w, cx| {
                        me.update(cx, |this, cx| {
                            let port: u16 = this.tmp_port.read(cx).value().parse().unwrap_or(0);
                            this.run(cx, "Forward removed", move |device| async move {
                                daemon::remove_forward(&device, port)
                                    .await
                                    .map(|_| String::new())
                            });
                        });
                    }
                }))
                .child(ui::button(t, "fwd-refresh", "Refresh list", false, {
                    let me = me.clone();
                    move |_, _w, cx| {
                        me.update(cx, |this, cx| {
                            this.run(cx, "Forwards refreshed", move |_device| async move {
                                daemon::list_forwards().await
                            });
                        });
                    }
                }))
                .into_any_element(),
        );
        rows.push(
            div()
                .font_family(theme::MONO)
                .text_size(t.text(10.5))
                .text_color(t.text_dim)
                .whitespace_normal()
                .child(self.forwards.clone())
                .into_any_element(),
        );
        // Input.
        rows.push(
            div()
                .flex()
                .items_center()
                .gap(px(8.0))
                .child(div().w(px(70.0)).child(self.tap_x.clone()))
                .child(div().w(px(70.0)).child(self.tap_y.clone()))
                .child(ui::button(t, "input-tap", "Tap", false, {
                    let me = me.clone();
                    move |_, _w, cx| {
                        me.update(cx, |this, cx| {
                            let x = this.tap_x.read(cx).value().to_string();
                            let y = this.tap_y.read(cx).value().to_string();
                            this.run(cx, "Tapped", move |device| async move {
                                daemon::run_shell(&device, &["input", "tap", &x, &y]).await
                            });
                        });
                    }
                }))
                .into_any_element(),
        );
        rows.push(
            div()
                .flex()
                .items_center()
                .gap(px(8.0))
                .child(div().w(px(220.0)).child(self.swipe.clone()))
                .child(ui::button(t, "input-swipe", "Swipe", false, {
                    let me = me.clone();
                    move |_, _w, cx| {
                        me.update(cx, |this, cx| {
                            let v = this.swipe.read(cx).value().to_string();
                            this.run(cx, "Swiped", move |device| async move {
                                let parts: Vec<&str> = v.split_whitespace().collect();
                                let coords: Vec<&str> = parts
                                    .iter()
                                    .flat_map(|p| p.split([',', '→']))
                                    .filter(|p| !p.trim().is_empty())
                                    .collect();
                                if coords.len() != 4 {
                                    return Err("swipe needs: x1,y1 x2,y2".to_string());
                                }
                                daemon::run_shell(
                                    &device,
                                    &[
                                        "input",
                                        "swipe",
                                        coords[0].trim(),
                                        coords[1].trim(),
                                        coords[2].trim(),
                                        coords[3].trim(),
                                    ],
                                )
                                .await
                            });
                        });
                    }
                }))
                .into_any_element(),
        );
        // All key buttons in one row instead:
        let key_row = div()
            .flex()
            .flex_wrap()
            .gap(px(6.0))
            .children(
                [
                    ("key-home2", "Home", "3"),
                    ("key-back2", "Back", "4"),
                    ("key-switch2", "Recents", "187"),
                    ("key-volup2", "Vol +", "24"),
                    ("key-voldn2", "Vol −", "25"),
                ]
                .iter()
                .map(|(id, label, key)| {
                    let me = me.clone();
                    let key = key.to_string();
                    ui::button(t, *id, *label, false, move |_, _w, cx| {
                        let key = key.clone();
                        me.update(cx, |this, cx| {
                            this.run(cx, "Key sent", move |device| async move {
                                daemon::run_shell(&device, &["input", "keyevent", &key]).await
                            });
                        });
                    })
                    .into_any_element()
                })
                .collect::<Vec<_>>(),
            )
            .into_any_element();
        rows.push(key_row);
        rows
    }

    fn output_section(&mut self, t: &Palette, _cx: &mut Context<Self>) -> Vec<AnyElement> {
        vec![
            div()
                .font_family(theme::MONO)
                .text_size(t.text(10.5))
                .text_color(t.text_dim)
                .whitespace_normal()
                .child(if self.output.is_empty() {
                    "Command output appears here.".to_string()
                } else {
                    self.output.clone()
                })
                .into_any_element(),
        ]
    }
}

fn toggle_row(
    t: &Palette,
    label: &'static str,
    on: bool,
    cx: &mut Context<Tools>,
    words: impl Fn(bool) -> Vec<&'static str> + 'static,
) -> AnyElement {
    let me = cx.entity();
    div()
        .flex()
        .items_center()
        .gap(px(10.0))
        .child(
            div()
                .w(px(110.0))
                .text_size(t.text(11.5))
                .text_color(t.text_muted)
                .child(label.to_string()),
        )
        .child(ui::button(
            t,
            ui::el_id(format!("toggle-{label}")),
            if on { "On" } else { "Off" },
            on,
            move |_, _w, cx| {
                me.update(cx, |this, cx| {
                    let next = !on;
                    let words = words(next);
                    this.shell(cx, &format!("{label} toggled"), words);
                    match label {
                        "Wi-Fi" => this.wifi = next,
                        "Mobile data" => this.data = next,
                        "Bluetooth" => this.bluetooth = next,
                        _ => this.airplane = next,
                    }
                });
            },
        ))
        .into_any_element()
}

fn human(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < 3 {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

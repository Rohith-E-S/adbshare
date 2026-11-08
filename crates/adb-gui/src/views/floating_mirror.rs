//! Floating phone screen mirror window.
//!
//! Opens in a standalone floating window matching the phone's native aspect ratio,
//! allowing seamless edge-to-edge mirroring across all Linux desktop environments
//! (GNOME, KDE Plasma, XFCE, Sway, Hyprland, etc.).

use std::path::PathBuf;
use std::sync::Arc;

use adb_remote::input::{ControlMessage, POINTER_ID_FINGER, key_action, motion_action};
use gpui::prelude::*;
use gpui::{
    AnyElement, App, Bounds, Context, ExternalPaths, FocusHandle, ImageSource, KeyDownEvent,
    MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels, Point, RenderImage,
    ScrollWheelEvent, TitlebarOptions, Window, WindowBounds, WindowKind, WindowOptions, div, px,
    size,
};

use crate::icons::{self, names};
use crate::state::daemon::{self, RemoteControlHandle, RemoteEvent};
use crate::theme::Themed;
use crate::views::ui;

/// Sizing constants for standard floating phone window
const DEFAULT_PHONE_W: f32 = 1080.0;
const DEFAULT_PHONE_H: f32 = 2400.0;
const PORTRAIT_TARGET_H: f32 = 760.0;
const LANDSCAPE_TARGET_W: f32 = 800.0;

/// Open a new floating window dedicated to mirroring the specified phone.
pub fn open_floating_mirror(
    device: String,
    device_label: String,
    preset: String,
    init_w: u32,
    init_h: u32,
    app: &mut App,
) {
    let (dw, dh) = if init_w > 0 && init_h > 0 {
        (init_w as f32, init_h as f32)
    } else {
        (DEFAULT_PHONE_W, DEFAULT_PHONE_H)
    };
    let aspect = dw / dh;
    let (target_w, target_h) = if aspect > 1.0 {
        let w = LANDSCAPE_TARGET_W;
        let h = (w / aspect).round();
        (w, h)
    } else {
        let h = PORTRAIT_TARGET_H;
        let w = (h * aspect).round();
        (w, h)
    };

    let bounds = Bounds::centered(None, size(px(target_w), px(target_h)), app);

    let title = if device_label.is_empty() {
        "Phone Mirror".to_string()
    } else {
        format!("{device_label} — Mirror")
    };

    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(bounds)),
        titlebar: Some(TitlebarOptions {
            title: Some(title.into()),
            appears_transparent: true,
            ..Default::default()
        }),
        window_min_size: Some(size(px(180.0), px(320.0))),
        kind: WindowKind::Floating,
        app_id: Some("org.adbshare.Adbshare.Mirror".to_string()),
        ..Default::default()
    };

    let dev_clone = device.clone();
    let lbl_clone = device_label.clone();
    let _ = app.open_window(options, |_window, cx| {
        cx.new(|cx| FloatingMirror::new(dev_clone, lbl_clone, preset, init_w, init_h, cx))
    });
}

/// Standalone floating phone mirror view.
pub struct FloatingMirror {
    device: Option<String>,
    device_label: String,
    preset: String,
    running: bool,
    polling: bool,
    error: Option<String>,
    status: Option<String>,
    socket_path: Option<PathBuf>,
    shm_path: Option<PathBuf>,
    shm_reader: Option<adb_remote::shm::ShmReader>,
    control_tx: Option<async_channel::Sender<ControlMessage>>,
    is_dragging: bool,
    last_drag: Option<(u32, u32)>,
    current_frame: Option<Arc<RenderImage>>,
    previous_frame: Option<Arc<RenderImage>>,
    frame_file: Option<PathBuf>,
    width: u32,
    height: u32,
    codec: String,
    frames: u64,
    fps_x10: u32,
    bytes: u64,
    controls_expanded: bool,
    last_device_dims: Option<(u32, u32)>,
    screen_off: bool,
    focus_handle: FocusHandle,
}

impl FloatingMirror {
    pub fn new(
        device: String,
        device_label: String,
        preset: String,
        init_w: u32,
        init_h: u32,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut this = Self {
            device: Some(device),
            device_label,
            preset,
            running: false,
            polling: false,
            error: None,
            status: None,
            socket_path: None,
            shm_path: None,
            shm_reader: None,
            control_tx: None,
            is_dragging: false,
            last_drag: None,
            current_frame: None,
            previous_frame: None,
            frame_file: None,
            width: init_w,
            height: init_h,
            codec: String::new(),
            frames: 0,
            fps_x10: 0,
            bytes: 0,
            controls_expanded: true,
            last_device_dims: if init_w > 0 && init_h > 0 {
                Some((init_w, init_h))
            } else {
                None
            },
            screen_off: false,
            focus_handle: cx.focus_handle(),
        };
        this.start(cx);
        this
    }

    fn aspect(&self) -> f32 {
        if self.width > 0 && self.height > 0 {
            self.width as f32 / self.height as f32
        } else {
            DEFAULT_PHONE_W / DEFAULT_PHONE_H
        }
    }

    fn window_to_device(&self, pos: Point<Pixels>, win_w: f32, win_h: f32) -> Option<(u32, u32)> {
        let (dw, dh) = match (self.width, self.height) {
            (w, h) if w > 0 && h > 0 => (w as f32, h as f32),
            _ => (DEFAULT_PHONE_W, DEFAULT_PHONE_H),
        };
        let aspect = dw / dh;
        let (screen_w, screen_h) = if (win_w / win_h) > aspect {
            ((win_h * aspect).round(), win_h)
        } else {
            (win_w, (win_w / aspect).round())
        };
        let ox = ((win_w - screen_w) / 2.0).max(0.0);
        let oy = ((win_h - screen_h) / 2.0).max(0.0);

        let px = f32::from(pos.x);
        let py = f32::from(pos.y);
        if px < ox || px > ox + screen_w || py < oy || py > oy + screen_h {
            return None;
        }
        let dx = (px - ox).clamp(0.0, screen_w);
        let dy = (py - oy).clamp(0.0, screen_h);
        let tx = ((dx / screen_w) * dw).round() as u32;
        let ty = ((dy / screen_h) * dh).round() as u32;
        Some((tx.min(dw as u32), ty.min(dh as u32)))
    }

    fn setup_control_channel(&mut self, sock: PathBuf, cx: &mut Context<Self>) {
        self.socket_path = Some(sock.clone());
        match daemon::open_remote_control(sock) {
            Ok(RemoteControlHandle { tx, rx }) => {
                self.control_tx = Some(tx);
                let me = cx.entity();
                cx.spawn(async move |_this, cx| {
                    while let Ok(event) = rx.recv().await {
                        let res = me.update(cx, |this, cx| {
                            this.handle_remote_event(event, cx);
                        });
                        if res.is_err() {
                            break;
                        }
                    }
                })
                .detach();
            }
            Err(e) => {
                self.error = Some(format!("Control channel error: {e}"));
            }
        }
    }

    fn handle_remote_event(&mut self, event: RemoteEvent, cx: &mut Context<Self>) {
        match event {
            RemoteEvent::Frame(notif) => {
                self.on_frame_notification(notif, cx);
            }
            RemoteEvent::Clipboard(text) => {
                cx.write_to_clipboard(gpui::ClipboardItem::new_string(text));
                self.status = Some("Clipboard synced from phone".to_string());
                cx.notify();
            }
        }
    }

    fn on_frame_notification(
        &mut self,
        notif: adb_remote::proxy::FrameNotification,
        cx: &mut Context<Self>,
    ) {
        if self.shm_reader.is_none()
            && let Some(ref p) = self.shm_path
        {
            self.shm_reader = adb_remote::shm::ShmReader::open(p).ok();
        }

        let Some(ref reader) = self.shm_reader else {
            return;
        };
        let w = notif.width;
        let h = notif.height;
        if w == 0 || h == 0 {
            return;
        }

        let frame_len = (w as usize).saturating_mul(h as usize).saturating_mul(4);
        let Ok(slot_bytes) = reader.read_slot_exact(notif.slot, frame_len) else {
            return;
        };

        // Construct image::RgbaImage from BGRA bytes (which GPUI Vulkan texture expects)
        if let Some(rgba) = image::RgbaImage::from_raw(w, h, slot_bytes.to_vec()) {
            let frame = image::Frame::new(rgba);
            let img = Arc::new(RenderImage::new(vec![frame]));
            if self.width != w || self.height != h {
                self.width = w;
                self.height = h;
            }
            self.previous_frame = self.current_frame.take();
            self.current_frame = Some(img);
            self.frames = self.frames.saturating_add(1);
            cx.notify();
        }
    }

    fn toggle_screen_off(&mut self, cx: &mut Context<Self>) {
        if !self.running {
            return;
        }
        let new_state = !self.screen_off;
        self.screen_off = new_state;
        let power_on = !new_state;
        if let Some(ref tx) = self.control_tx {
            let _ = tx.try_send(ControlMessage::SetDisplayPower { on: power_on });
        }
        self.status = Some(if new_state {
            "Phone display off (mirroring active)".to_string()
        } else {
            "Phone display on".to_string()
        });
        cx.notify();
    }

    fn send_touch(&mut self, action: i32, x: u32, y: u32, pressure: f32) {
        if !self.running {
            return;
        }
        let (dw, dh) = match (self.width, self.height) {
            (w, h) if w > 0 && h > 0 => (w as f32, h as f32),
            _ => (DEFAULT_PHONE_W, DEFAULT_PHONE_H),
        };
        let msg = ControlMessage::InjectTouchEvent {
            action,
            pointer_id: POINTER_ID_FINGER,
            x: x as i32,
            y: y as i32,
            screen_w: dw as u16,
            screen_h: dh as u16,
            pressure,
            action_button: 0,
            buttons: 0,
        };
        if let Some(ref tx) = self.control_tx {
            let _ = tx.try_send(msg);
        }
    }

    fn send_scroll(&mut self, x: u32, y: u32, hscroll: f32, vscroll: f32) {
        if !self.running {
            return;
        }
        let (dw, dh) = match (self.width, self.height) {
            (w, h) if w > 0 && h > 0 => (w as f32, h as f32),
            _ => (DEFAULT_PHONE_W, DEFAULT_PHONE_H),
        };
        let msg = ControlMessage::InjectScrollEvent {
            x: x as i32,
            y: y as i32,
            screen_w: dw as u16,
            screen_h: dh as u16,
            hscroll,
            vscroll,
            buttons: 0,
        };
        if let Some(ref tx) = self.control_tx {
            let _ = tx.try_send(msg);
        }
    }

    fn send_key(&mut self, label: &str, keycode: i32) {
        if !self.running {
            return;
        }
        if let Some(ref tx) = self.control_tx {
            let down = ControlMessage::InjectKeycode {
                action: key_action::DOWN,
                keycode,
                repeat: 0,
                metastate: 0,
            };
            let up = ControlMessage::InjectKeycode {
                action: key_action::UP,
                keycode,
                repeat: 0,
                metastate: 0,
            };
            let _ = tx.try_send(down);
            let _ = tx.try_send(up);
            self.status = Some(format!("Sent {label}"));
        } else {
            self.error = Some("control channel unavailable".to_string());
        }
    }

    fn rotate_device(&mut self) {
        if !self.running {
            return;
        }
        if let Some(ref tx) = self.control_tx {
            let _ = tx.try_send(ControlMessage::RotateDevice);
        }
    }

    fn take_screenshot(&mut self, cx: &mut Context<Self>) {
        if !self.running {
            return;
        }
        let Some(device) = self.device.clone() else {
            return;
        };
        let me = cx.entity();
        cx.spawn(async move |_this, cx| {
            let result = daemon::screenshot(&device).await;
            let _ = me.update(cx, |this, cx| match result {
                Ok(_) => {
                    this.status = Some("Screenshot saved to Pictures".to_string());
                    cx.notify();
                }
                Err(e) => {
                    this.error = Some(daemon::explain(&e));
                    cx.notify();
                }
            });
        })
        .detach();
    }

    pub fn start(&mut self, cx: &mut Context<Self>) {
        let Some(device) = self.device.clone() else {
            return;
        };
        let preset = self.preset.clone();
        let options = serde_json::json!({ "preset": preset, "audio": false }).to_string();
        let me = cx.entity();

        cx.spawn(async move |_this, cx| {
            // Check if remote session is already running
            if let Ok(st) = daemon::remote_status(&device).await
                && st.get("running").and_then(|r| r.as_bool()) == Some(true)
            {
                let _ = me.update(cx, |this, cx| {
                    apply_status(this, &st, cx);
                    this.start_poll(cx);
                    cx.notify();
                });
                return;
            }

            let result = daemon::start_remote(&device, &options).await;
            let _ = me.update(cx, |this, cx| match result {
                Ok(info) => {
                    this.running = true;
                    if let Some(shm) = info.get("shm_path").and_then(|s| s.as_str()) {
                        let p = PathBuf::from(shm);
                        this.shm_path = Some(p.clone());
                        this.shm_reader = adb_remote::shm::ShmReader::open(&p).ok();
                    }
                    if let Some(frame) = info.get("frame").and_then(|f| f.as_str()) {
                        this.frame_file = Some(PathBuf::from(frame));
                    }
                    if let Some(sock) = info.get("socket").and_then(|s| s.as_str()) {
                        let p = PathBuf::from(sock);
                        this.setup_control_channel(p, cx);
                    }
                    if let Some(w) = info.get("width").and_then(|w| w.as_u64()) {
                        this.width = w as u32;
                    }
                    if let Some(h) = info.get("height").and_then(|h| h.as_u64()) {
                        this.height = h as u32;
                    }
                    this.start_poll(cx);
                    cx.notify();
                }
                Err(e) => {
                    this.error = Some(daemon::explain(&e));
                    cx.notify();
                }
            });
        })
        .detach();
    }

    #[allow(dead_code)]
    pub fn stop(&mut self, cx: &mut Context<Self>) {
        if self.screen_off {
            if let Some(ref tx) = self.control_tx {
                let _ = tx.try_send(ControlMessage::SetDisplayPower { on: true });
            }
            self.screen_off = false;
        }
        self.running = false;
        self.polling = false;
        self.control_tx = None;
        self.shm_reader = None;
        self.is_dragging = false;
        self.last_drag = None;
        self.current_frame = None;
        self.previous_frame = None;
        self.frame_file = None;
        cx.notify();

        let Some(device) = self.device.clone() else {
            return;
        };
        let me = cx.entity();
        cx.spawn(async move |_this, cx| {
            let result = daemon::stop_remote(&device).await;
            let _ = me.update(cx, |this, cx| match result {
                Ok(_) => {
                    this.socket_path = None;
                    cx.notify();
                }
                Err(e) => {
                    this.error = Some(daemon::explain(&e));
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn start_poll(&mut self, cx: &mut Context<Self>) {
        if self.polling {
            return;
        }
        let Some(device) = self.device.clone() else {
            return;
        };
        self.polling = true;
        let me = cx.entity();

        cx.spawn(async move |_this, cx| {
            loop {
                // Heartbeat status poll every 1s (minimal CPU footprint, zero lag)
                gpui::Timer::after(std::time::Duration::from_millis(1000)).await;
                let still = me.read_with(cx, |this, _c| this.polling).unwrap_or(false);
                if !still {
                    break;
                }

                let status = daemon::remote_status(&device).await.ok();
                let _ = me.update(cx, |this, cx| {
                    if let Some(v) = status {
                        apply_status(this, &v, cx);
                        if !this.running && v.get("running").and_then(|r| r.as_bool()) == Some(true)
                        {
                            this.running = true;
                        }
                        if this.running && v.get("running").and_then(|r| r.as_bool()) == Some(false)
                        {
                            this.running = false;
                            this.polling = false;
                            this.current_frame = None;
                            this.previous_frame = None;
                        }
                        cx.notify();
                    }
                });
            }
        })
        .detach();
    }

    fn on_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Take focus so keyboard shortcuts and text input land on phone
        self.focus_handle.focus(window);

        if !self.running {
            return;
        }
        let win_w = f32::from(window.bounds().size.width);
        let win_h = f32::from(window.bounds().size.height);

        // Right click = Android Back button (scrcpy convention)
        if event.button == MouseButton::Right {
            self.send_key("Back", 4);
            cx.notify();
            return;
        }

        if event.button != MouseButton::Left {
            return;
        }

        let Some((tx, ty)) = self.window_to_device(event.position, win_w, win_h) else {
            return;
        };
        self.is_dragging = true;
        self.last_drag = Some((tx, ty));
        self.send_touch(motion_action::DOWN, tx, ty, 1.0);
        cx.notify();
    }

    fn on_mouse_move(
        &mut self,
        event: &MouseMoveEvent,
        window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
        if !self.running || !self.is_dragging {
            return;
        }
        let win_w = f32::from(window.bounds().size.width);
        let win_h = f32::from(window.bounds().size.height);

        let Some((tx, ty)) = self.window_to_device(event.position, win_w, win_h) else {
            return;
        };
        if self.last_drag == Some((tx, ty)) {
            return;
        }
        self.last_drag = Some((tx, ty));
        self.send_touch(motion_action::MOVE, tx, ty, 1.0);
    }

    fn on_mouse_up(&mut self, event: &MouseUpEvent, window: &mut Window, cx: &mut Context<Self>) {
        if !self.running {
            self.is_dragging = false;
            return;
        }
        if event.button != MouseButton::Left || !self.is_dragging {
            return;
        }
        self.is_dragging = false;
        let win_w = f32::from(window.bounds().size.width);
        let win_h = f32::from(window.bounds().size.height);

        let (tx, ty) = self
            .window_to_device(event.position, win_w, win_h)
            .or(self.last_drag)
            .unwrap_or((0, 0));
        self.last_drag = None;
        self.send_touch(motion_action::UP, tx, ty, 0.0);
        cx.notify();
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        if !self.running {
            return;
        }
        let key = &event.keystroke.key;
        let mods = &event.keystroke.modifiers;

        // Clipboard shortcut handling: Ctrl+V (paste) & Ctrl+C (copy) & Ctrl+A
        if mods.control && !mods.alt && !mods.platform {
            if key.eq_ignore_ascii_case("v") {
                if let Some(item) = cx.read_from_clipboard()
                    && let Some(text) = item.text()
                    && let Some(ref tx) = self.control_tx
                {
                    let _ = tx.try_send(ControlMessage::SetClipboard {
                        sequence: 0,
                        paste: true,
                        text: text.to_string(),
                    });
                    self.status = Some("Pasted clipboard to phone".to_string());
                    cx.notify();
                }
                return;
            } else if key.eq_ignore_ascii_case("c") {
                if let Some(ref tx) = self.control_tx {
                    let _ = tx.try_send(ControlMessage::GetClipboard { copy: true });
                    self.status = Some("Requested phone clipboard".to_string());
                    cx.notify();
                }
                return;
            } else if key.eq_ignore_ascii_case("a") {
                if let Some(ref tx) = self.control_tx {
                    let _ = tx.try_send(ControlMessage::InjectKeycode {
                        action: key_action::DOWN,
                        keycode: 29, // AKEYCODE_A
                        repeat: 0,
                        metastate: 0x1000, // META_CTRL_ON
                    });
                    let _ = tx.try_send(ControlMessage::InjectKeycode {
                        action: key_action::UP,
                        keycode: 29,
                        repeat: 0,
                        metastate: 0,
                    });
                }
                return;
            }
        }

        // Standard Android navigation and action keys
        let keycode = match key.as_str() {
            "backspace" => Some(67), // AKEYCODE_DEL
            "enter" => Some(66),     // AKEYCODE_ENTER
            "tab" => Some(61),       // AKEYCODE_TAB
            "escape" => Some(4),     // AKEYCODE_BACK
            "delete" => Some(112),   // AKEYCODE_FORWARD_DEL
            "up" => Some(19),        // AKEYCODE_DPAD_UP
            "down" => Some(20),      // AKEYCODE_DPAD_DOWN
            "left" => Some(21),      // AKEYCODE_DPAD_LEFT
            "right" => Some(22),     // AKEYCODE_DPAD_RIGHT
            "space" => Some(62),     // AKEYCODE_SPACE
            "home" => Some(3),       // AKEYCODE_HOME
            "pageup" => Some(92),    // AKEYCODE_PAGE_UP
            "pagedown" => Some(93),  // AKEYCODE_PAGE_DOWN
            _ => None,
        };

        if let Some(code) = keycode {
            if let Some(ref tx) = self.control_tx {
                let _ = tx.try_send(ControlMessage::InjectKeycode {
                    action: key_action::DOWN,
                    keycode: code,
                    repeat: 0,
                    metastate: 0,
                });
                let _ = tx.try_send(ControlMessage::InjectKeycode {
                    action: key_action::UP,
                    keycode: code,
                    repeat: 0,
                    metastate: 0,
                });
            }
            return;
        }

        // Text injection for printable characters
        if !mods.control && !mods.alt && !mods.platform {
            let text_to_inject = if let Some(ref text) = event.keystroke.key_char {
                if !text.is_empty() && text.chars().all(|c| !c.is_control()) {
                    Some(text.clone())
                } else {
                    None
                }
            } else if key.chars().count() == 1 {
                let ch = key.chars().next().unwrap();
                if !ch.is_control() {
                    Some(ch.to_string())
                } else {
                    None
                }
            } else {
                None
            };

            if let Some(text) = text_to_inject
                && let Some(ref tx) = self.control_tx
            {
                let _ = tx.try_send(ControlMessage::InjectText(text));
            }
        }
    }

    fn on_scroll_wheel(
        &mut self,
        event: &ScrollWheelEvent,
        window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
        if !self.running {
            return;
        }
        let win_w = f32::from(window.bounds().size.width);
        let win_h = f32::from(window.bounds().size.height);

        let Some((tx, ty)) = self.window_to_device(event.position, win_w, win_h) else {
            return;
        };
        let delta = event.delta.pixel_delta(px(20.0));
        let hscroll = (f32::from(delta.x) / 10.0).clamp(-16.0, 16.0);
        let vscroll = (f32::from(delta.y) / 10.0).clamp(-16.0, 16.0);
        if hscroll.abs() > 0.01 || vscroll.abs() > 0.01 {
            self.send_scroll(tx, ty, hscroll, vscroll);
        }
    }

    fn on_external_drop(
        &mut self,
        dropped: &ExternalPaths,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(device) = self.device.clone() else {
            return;
        };
        let paths = dropped.paths().to_vec();
        if paths.is_empty() {
            return;
        }
        let me = cx.entity();
        cx.spawn(async move |_this, cx| {
            for path in paths {
                let is_apk = path
                    .extension()
                    .and_then(|e| e.to_str())
                    .map(|e| e.eq_ignore_ascii_case("apk"))
                    .unwrap_or(false);
                let name = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("file")
                    .to_string();

                if is_apk {
                    let _ = me.update(cx, |this, cx| {
                        this.status = Some(format!("Installing {name}…"));
                        cx.notify();
                    });
                    let res = daemon::start_install(&device, &path.to_string_lossy()).await;
                    let _ = me.update(cx, |this, cx| {
                        match res {
                            Ok(_) => this.status = Some(format!("Installed {name}")),
                            Err(e) => this.error = Some(format!("Install failed: {e}")),
                        }
                        cx.notify();
                    });
                } else {
                    let _ = me.update(cx, |this, cx| {
                        this.status = Some(format!("Pushing {name} to /sdcard/Download/…"));
                        cx.notify();
                    });
                    let remote_dest = format!("/sdcard/Download/{name}");
                    let policy = ("skip".to_string(), false);
                    let res = daemon::enqueue_push(
                        &device,
                        &path.to_string_lossy(),
                        &remote_dest,
                        policy,
                    )
                    .await;
                    let _ = me.update(cx, |this, cx| {
                        match res {
                            Ok(_) => this.status = Some(format!("Pushed {name} to Download")),
                            Err(e) => this.error = Some(format!("Push failed: {e}")),
                        }
                        cx.notify();
                    });
                }
            }
        })
        .detach();
    }
}

impl Drop for FloatingMirror {
    fn drop(&mut self) {
        if self.screen_off
            && let Some(ref tx) = self.control_tx
        {
            let _ = tx.try_send(ControlMessage::SetDisplayPower { on: true });
        }
        if let Some(device) = self.device.take() {
            daemon::spawn_stop_remote(device);
        }
    }
}

fn apply_status(
    this: &mut FloatingMirror,
    v: &serde_json::Value,
    cx: &mut Context<FloatingMirror>,
) {
    let running = v.get("running").and_then(|r| r.as_bool()).unwrap_or(false);
    if running {
        this.running = true;
        if let Some(shm) = v.get("shm_path").and_then(|s| s.as_str()) {
            let p = PathBuf::from(shm);
            if this.shm_path.as_ref() != Some(&p) {
                this.shm_path = Some(p.clone());
                this.shm_reader = adb_remote::shm::ShmReader::open(&p).ok();
            }
        }
        if let Some(frame) = v.get("frame").and_then(|f| f.as_str()) {
            this.frame_file = Some(PathBuf::from(frame));
        }
        if let Some(sock) = v.get("socket").and_then(|s| s.as_str()) {
            let p = PathBuf::from(sock);
            if this.socket_path.as_ref() != Some(&p) || this.control_tx.is_none() {
                this.setup_control_channel(p, cx);
            }
        }
        this.codec = v
            .get("codec")
            .and_then(|c| c.as_str())
            .unwrap_or_default()
            .to_string();
        this.frames = v
            .get("frames")
            .and_then(|n| n.as_u64())
            .unwrap_or(this.frames);
        this.bytes = v.get("bytes").and_then(|n| n.as_u64()).unwrap_or(0);
        let w = v.get("width").and_then(|n| n.as_u64()).unwrap_or(0) as u32;
        let h = v.get("height").and_then(|n| n.as_u64()).unwrap_or(0) as u32;
        if w > 0 && h > 0 {
            this.width = w;
            this.height = h;
        }
        this.fps_x10 = v.get("fps_x10").and_then(|n| n.as_u64()).unwrap_or(0) as u32;
        this.error = None;
    } else {
        this.running = false;
        this.socket_path = None;
        this.control_tx = None;
        this.shm_reader = None;
        this.current_frame = None;
        this.previous_frame = None;
        this.frame_file = None;
        if let Some(err) = v.get("error").and_then(|e| e.as_str()) {
            this.error = Some(err.to_string());
        }
    }
}

fn overlay_btn(
    id: impl Into<gpui::ElementId>,
    icon_name: &'static str,
    hint: &'static str,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut gpui::App) + 'static,
) -> impl IntoElement {
    div()
        .id(id)
        .size(px(30.0))
        .flex()
        .items_center()
        .justify_center()
        .rounded_full()
        .bg(gpui::rgba(0xffffff15))
        .text_color(gpui::rgb(0xffffff))
        .cursor_pointer()
        .hover(|s| s.bg(gpui::rgba(0xffffff30)))
        .active(|s| s.bg(gpui::rgba(0xffffff45)))
        .tooltip(ui::hover_hint(hint))
        .child(icons::icon(icon_name, 15.0, gpui::rgb(0xffffff)))
        .on_mouse_down(MouseButton::Left, |_, _, _| {})
        .on_mouse_down(MouseButton::Right, |_, _, _| {})
        .on_click(on_click)
}

impl Render for FloatingMirror {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if let Some(prev) = self.previous_frame.take() {
            let _ = window.drop_image(prev);
        }

        let aspect = self.aspect();
        let win_w = f32::from(window.bounds().size.width);
        let win_h = f32::from(window.bounds().size.height);

        // Auto-adapt window dimensions when phone resolution or orientation changes
        if self.width > 0
            && self.height > 0
            && self.last_device_dims != Some((self.width, self.height))
            && win_w > 50.0
            && win_h > 50.0
        {
            self.last_device_dims = Some((self.width, self.height));
            let (target_w, target_h) = if aspect > 1.0 {
                let w = LANDSCAPE_TARGET_W;
                let h = (w / aspect).round();
                (w, h)
            } else {
                let h = PORTRAIT_TARGET_H;
                let w = (h * aspect).round();
                (w, h)
            };
            window.resize(size(px(target_w), px(target_h)));
        }

        // Calculate seamless fitted screen bounds
        let (screen_w, screen_h) = if (win_w / win_h) > aspect {
            ((win_h * aspect).round(), win_h)
        } else {
            (win_w, (win_w / aspect).round())
        };

        let me = cx.entity();
        let frame: AnyElement = if let Some(img) = &self.current_frame {
            gpui::img(ImageSource::Render(img.clone()))
                .w(px(screen_w))
                .h(px(screen_h))
                .object_fit(gpui::ObjectFit::Fill)
                .into_any_element()
        } else {
            let t = *cx.theme();
            let me_start = me.clone();
            div()
                .size_full()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap(px(12.0))
                .bg(gpui::rgb(0x0e1013))
                .child(
                    div()
                        .size(px(44.0))
                        .rounded_full()
                        .bg(t.secondary_bg)
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(icons::icon(names::PHONE, 22.0, t.text_secondary)),
                )
                .child(
                    div()
                        .text_size(t.text(13.0))
                        .text_color(t.text_header)
                        .child(if let Some(ref err) = self.error {
                            err.clone()
                        } else if self.running {
                            "Waiting for frames…".to_string()
                        } else if self.device_label.is_empty() {
                            "Mirror is stopped".to_string()
                        } else {
                            format!("{} — Mirror is stopped", self.device_label)
                        }),
                )
                .when(!self.running, |d| {
                    d.child(ui::button(
                        &t,
                        "float-start-btn",
                        "Start mirror",
                        true,
                        move |_, _w, cx| {
                            me_start.update(cx, |this, cx| this.start(cx));
                        },
                    ))
                })
                .into_any_element()
        };

        // Front-facing camera hole punch cutout
        let is_landscape = screen_w > screen_h;
        let camera_cutout = if is_landscape {
            div()
                .absolute()
                .left(px(10.0))
                .top(px((screen_h - 11.0) / 2.0))
                .size(px(11.0))
                .rounded_full()
                .bg(gpui::rgb(0x0a0a0c))
                .border_1()
                .border_color(gpui::rgba(0xffffff20))
                .flex()
                .items_center()
                .justify_center()
                .child(
                    div()
                        .size(px(3.5))
                        .rounded_full()
                        .bg(gpui::rgba(0xffffff25)),
                )
        } else {
            div()
                .absolute()
                .top(px(10.0))
                .left(px((screen_w - 11.0) / 2.0))
                .size(px(11.0))
                .rounded_full()
                .bg(gpui::rgb(0x0a0a0c))
                .border_1()
                .border_color(gpui::rgba(0xffffff20))
                .flex()
                .items_center()
                .justify_center()
                .child(
                    div()
                        .size(px(3.5))
                        .rounded_full()
                        .bg(gpui::rgba(0xffffff25)),
                )
        };

        let screen_container = div()
            .relative()
            .w(px(screen_w))
            .h(px(screen_h))
            .rounded(px(16.0))
            .overflow_hidden()
            .child(frame)
            .child(camera_cutout);

        // Floating action pill docked at bottom of the screen
        let me_ctl = me.clone();
        let screen_off = self.screen_off;
        let controls: AnyElement = if self.controls_expanded {
            div()
                .absolute()
                .bottom(px(14.0))
                .left_0()
                .right_0()
                .flex()
                .justify_center()
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(5.0))
                        .px(px(8.0))
                        .py(px(5.0))
                        .rounded_full()
                        .bg(gpui::rgba(0x111317d0))
                        .border_1()
                        .border_color(gpui::rgba(0xffffff1a))
                        .shadow_lg()
                        .on_mouse_down(MouseButton::Left, |_, _, _| {})
                        .on_mouse_down(MouseButton::Right, |_, _, _| {})
                        .child(overlay_btn(
                            "flt-back",
                            names::GO_PREVIOUS,
                            "Back (Right-click)",
                            {
                                let me = me_ctl.clone();
                                move |_, _w, cx| {
                                    me.update(cx, |this, _cx| this.send_key("Back", 4));
                                }
                            },
                        ))
                        .child(overlay_btn("flt-home", names::HOME, "Home", {
                            let me = me_ctl.clone();
                            move |_, _w, cx| {
                                me.update(cx, |this, _cx| this.send_key("Home", 3));
                            }
                        }))
                        .child(overlay_btn("flt-recents", names::VIEW_GRID, "Recents", {
                            let me = me_ctl.clone();
                            move |_, _w, cx| {
                                me.update(cx, |this, _cx| this.send_key("Recents", 187));
                            }
                        }))
                        .child(overlay_btn(
                            "flt-screen-off",
                            names::PHONE,
                            if screen_off {
                                "Turn phone screen on"
                            } else {
                                "Turn phone screen off (mirror stays active)"
                            },
                            {
                                let me = me_ctl.clone();
                                move |_, _w, cx| {
                                    me.update(cx, |this, cx| this.toggle_screen_off(cx));
                                }
                            },
                        ))
                        .child(overlay_btn(
                            "flt-voldn",
                            names::TEXT_SMALLER,
                            "Volume down",
                            {
                                let me = me_ctl.clone();
                                move |_, _w, cx| {
                                    me.update(cx, |this, _cx| this.send_key("Volume down", 25));
                                }
                            },
                        ))
                        .child(overlay_btn("flt-volup", names::TEXT_LARGER, "Volume up", {
                            let me = me_ctl.clone();
                            move |_, _w, cx| {
                                me.update(cx, |this, _cx| this.send_key("Volume up", 24));
                            }
                        }))
                        .child(overlay_btn("flt-power", names::STOP, "Power", {
                            let me = me_ctl.clone();
                            move |_, _w, cx| {
                                me.update(cx, |this, _cx| this.send_key("Power", 26));
                            }
                        }))
                        .child(overlay_btn("flt-rot", names::REFRESH, "Rotate", {
                            let me = me_ctl.clone();
                            move |_, _w, cx| {
                                me.update(cx, |this, _cx| this.rotate_device());
                            }
                        }))
                        .child(overlay_btn(
                            "flt-shot",
                            names::CAMERA_PHOTO,
                            "Screenshot",
                            {
                                let me = me_ctl.clone();
                                move |_, _w, cx| {
                                    me.update(cx, |this, cx| this.take_screenshot(cx));
                                }
                            },
                        ))
                        .child(
                            div()
                                .id("flt-collapse")
                                .size(px(24.0))
                                .flex()
                                .items_center()
                                .justify_center()
                                .rounded_full()
                                .cursor_pointer()
                                .hover(|s| s.bg(gpui::rgba(0xffffff20)))
                                .tooltip(ui::hover_hint("Hide controls"))
                                .on_mouse_down(MouseButton::Left, |_, _, _| {})
                                .on_mouse_down(MouseButton::Right, |_, _, _| {})
                                .child(
                                    div()
                                        .w(px(10.0))
                                        .h(px(2.0))
                                        .rounded_full()
                                        .bg(gpui::rgba(0xffffffaa)),
                                )
                                .on_click({
                                    let me = me_ctl.clone();
                                    move |_, _w, cx| {
                                        me.update(cx, |this, cx| {
                                            this.controls_expanded = false;
                                            cx.notify();
                                        });
                                    }
                                }),
                        ),
                )
                .into_any_element()
        } else {
            div()
                .absolute()
                .bottom(px(12.0))
                .right(px(12.0))
                .child(
                    div()
                        .id("flt-expand")
                        .size(px(28.0))
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded_full()
                        .bg(gpui::rgba(0x111317aa))
                        .border_1()
                        .border_color(gpui::rgba(0xffffff1a))
                        .cursor_pointer()
                        .hover(|s| s.bg(gpui::rgba(0x111317dd)))
                        .tooltip(ui::hover_hint("Show phone controls"))
                        .on_mouse_down(MouseButton::Left, |_, _, _| {})
                        .on_mouse_down(MouseButton::Right, |_, _, _| {})
                        .child(icons::icon(names::PHONE, 14.0, gpui::rgb(0xffffff)))
                        .on_click({
                            let me = me_ctl.clone();
                            move |_, _w, cx| {
                                me.update(cx, |this, cx| {
                                    this.controls_expanded = true;
                                    cx.notify();
                                });
                            }
                        }),
                )
                .into_any_element()
        };

        div()
            .id("floating-mirror-root")
            .size_full()
            .bg(gpui::rgb(0x0a0a0c))
            .relative()
            .overflow_hidden()
            .flex()
            .items_center()
            .justify_center()
            .track_focus(&self.focus_handle)
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_down(MouseButton::Right, cx.listener(Self::on_mouse_down))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_scroll_wheel(cx.listener(Self::on_scroll_wheel))
            .on_key_down(cx.listener(Self::on_key_down))
            .on_drop::<ExternalPaths>(cx.listener(Self::on_external_drop))
            .child(screen_container)
            .child(controls)
    }
}

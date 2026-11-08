//! Screen mirror view: live phone screen + mouse/touch control.
//!
//! The daemon owns the scrcpy server, tunnels, FFmpeg pipeline and Unix socket;
//! this view starts/stops the session over D-Bus and renders the live decoded
//! frames into an atlas-friendly [`RenderImage`].
//!
//! The phone is displayed inside a sleek, realistic smartphone chassis with
//! a camera punch-hole cutout, side hardware button notches, and a vertical
//! action rail docked to its right. Sizing is dynamic, maximizing screen area
//! within the available viewport while maintaining device aspect ratio.
//! Mouse click-and-drag and trackpad swipe gestures stream low-latency
//! touch events over a persistent Unix control socket.

use std::path::PathBuf;
use std::sync::Arc;

use adb_remote::input::{ControlMessage, POINTER_ID_FINGER, key_action, motion_action};
use gpui::prelude::*;
use gpui::{
    AnyElement, Context, ExternalPaths, FocusHandle, ImageSource, KeyDownEvent, MouseButton,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels, Point, RenderImage, ScrollWheelEvent,
    Window, div, px,
};

use crate::icons::{self, names};
use crate::state::daemon::{self, RemoteControlHandle, RemoteEvent};
use crate::theme::{self, Palette, Themed};
use crate::views::ui;

/// Sizing and layout constants for the phone frame and docked rail.
const HEADER_H: f32 = 44.0;
const BEZEL: f32 = 10.0;
const RAIL_W: f32 = 44.0;
const GAP: f32 = 16.0;
const PADDING_Y: f32 = 16.0;
const PADDING_X: f32 = 24.0;

/// Quality presets offered by the toolbar, spelled as the daemon expects.
const PRESETS: &[&str] = &["performance", "balanced", "quality"];

/// The mirror view.
pub struct Mirror {
    device: Option<String>,
    device_label: String,
    running: bool,
    polling: bool,
    preset: String,
    error: Option<String>,
    status: Option<String>,
    /// Path to the Unix control socket provided by the daemon.
    socket_path: Option<PathBuf>,
    /// Backing path to the shared memory frame pool.
    shm_path: Option<PathBuf>,
    /// Reader handle mapping the shared memory frame pool.
    shm_reader: Option<adb_remote::shm::ShmReader>,
    /// Persistent sender to the remote control socket task.
    control_tx: Option<async_channel::Sender<ControlMessage>>,
    /// Whether the device physical screen has been turned off to save power.
    screen_off: bool,
    /// Active drag / swipe state.
    is_dragging: bool,
    last_drag: Option<(u32, u32)>,
    /// Current rendered frame in memory, cache-safe across rapid updates.
    current_frame: Option<Arc<RenderImage>>,
    /// Previous frame dropped from atlas during next paint.
    previous_frame: Option<Arc<RenderImage>>,
    /// Latest screenshot path for the `img()` frame; `None` until one lands.
    screenshot_path: Option<String>,
    /// Direct path to the current frame JPEG written by the daemon.
    frame_file: Option<PathBuf>,
    /// Video dimensions from `remote_status` (zeros until known).
    width: u32,
    height: u32,
    /// Session stats shown in the header.
    codec: String,
    frames: u64,
    fps_x10: u32,
    bytes: u64,
    /// Content origin reported by the app root; the frame origin is derived.
    viewport_x: f32,
    viewport_y: f32,
    viewport_w: f32,
    viewport_h: f32,
    /// Focus handle for keyboard input passthrough.
    focus_handle: FocusHandle,
}

impl Mirror {
    pub fn new(cx: &mut Context<Self>) -> Self {
        Self {
            device: None,
            device_label: String::new(),
            running: false,
            polling: false,
            preset: "balanced".to_string(),
            error: None,
            status: None,
            socket_path: None,
            shm_path: None,
            shm_reader: None,
            control_tx: None,
            screen_off: false,
            is_dragging: false,
            last_drag: None,
            current_frame: None,
            previous_frame: None,
            screenshot_path: None,
            frame_file: None,
            width: 0,
            height: 0,
            codec: String::new(),
            frames: 0,
            fps_x10: 0,
            bytes: 0,
            viewport_x: 0.0,
            viewport_y: 0.0,
            viewport_w: 0.0,
            viewport_h: 0.0,
            focus_handle: cx.focus_handle(),
        }
    }

    pub fn set_device(&mut self, device: Option<String>, cx: &mut Context<Self>) {
        if self.device == device {
            if self.device.is_some() {
                self.refresh_status(cx);
            }
            return;
        }
        if self.screen_off {
            if let Some(ref tx) = self.control_tx {
                let _ = tx.try_send(ControlMessage::SetDisplayPower { on: true });
            }
            self.screen_off = false;
        }
        self.device = device;
        self.running = false;
        self.polling = false;
        self.error = None;
        self.socket_path = None;
        self.shm_path = None;
        self.shm_reader = None;
        self.control_tx = None;
        self.is_dragging = false;
        self.last_drag = None;
        self.current_frame = None;
        self.previous_frame = None;
        self.screenshot_path = None;
        self.frame_file = None;
        self.frames = 0;
        self.width = 0;
        self.height = 0;
        if self.device.is_some() {
            self.refresh_status(cx);
        }
        cx.notify();
    }

    pub fn set_device_label(&mut self, label: String, cx: &mut Context<Self>) {
        if self.device_label != label {
            self.device_label = label;
            cx.notify();
        }
    }

    /// Content origin from the app root (sidebar + topbar offset), used to
    /// turn a window-relative click into frame-relative coordinates.
    pub fn set_viewport(&mut self, x: f32, y: f32, w: f32, h: f32, _cx: &mut Context<Self>) {
        self.viewport_x = x;
        self.viewport_y = y;
        self.viewport_w = w;
        self.viewport_h = h;
    }

    fn aspect(&self) -> f32 {
        if self.width > 0 && self.height > 0 {
            self.width as f32 / self.height as f32
        } else {
            1080.0 / 2400.0
        }
    }

    /// (inner_w, inner_h, outer_w, outer_h)
    fn phone_dims(&self) -> (f32, f32, f32, f32) {
        let avail_w = (self.viewport_w - PADDING_X * 2.0 - RAIL_W - GAP).max(100.0);
        let avail_h = (self.viewport_h - HEADER_H - PADDING_Y * 2.0).max(100.0);

        let aspect = self.aspect();
        let max_inner_w = (avail_w - BEZEL * 2.0).max(50.0);
        let max_inner_h = (avail_h - BEZEL * 2.0).max(50.0);

        let (inner_w, inner_h) = if max_inner_w / max_inner_h > aspect {
            ((max_inner_h * aspect).round(), max_inner_h.round())
        } else {
            (max_inner_w.round(), (max_inner_w / aspect).round())
        };

        let outer_w = inner_w + BEZEL * 2.0;
        let outer_h = inner_h + BEZEL * 2.0;
        (inner_w, inner_h, outer_w, outer_h)
    }

    fn inner_screen_origin(&self) -> (f32, f32) {
        let (_, _, outer_w, outer_h) = self.phone_dims();
        let total_w = outer_w + GAP + RAIL_W;
        let ox = self.viewport_x + (self.viewport_w - total_w) / 2.0 + BEZEL;
        let stage_avail_h = self.viewport_h - HEADER_H;
        let oy = self.viewport_y + HEADER_H + (stage_avail_h - outer_h) / 2.0 + BEZEL;
        (ox, oy)
    }

    fn window_to_device(&self, pos: Point<Pixels>) -> Option<(u32, u32)> {
        let (inner_w, inner_h, _, _) = self.phone_dims();
        if inner_w <= 0.0 || inner_h <= 0.0 {
            return None;
        }
        let (ox, oy) = self.inner_screen_origin();
        let px = f32::from(pos.x);
        let py = f32::from(pos.y);
        let dx = (px - ox).clamp(0.0, inner_w);
        let dy = (py - oy).clamp(0.0, inner_h);
        let (dw, dh) = match (self.width, self.height) {
            (w, h) if w > 0 && h > 0 => (w as f32, h as f32),
            _ => (1080.0, 2400.0),
        };
        let tx = ((dx / inner_w) * dw).round() as u32;
        let ty = ((dy / inner_h) * dh).round() as u32;
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
                self.fail(format!("Control channel error: {e}"), cx);
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
                self.note("Clipboard synced from phone", cx);
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
        // Tear check: skip the frame if the writer is mid-publish (the next
        // notification will carry a consistent frame).
        if let Some(snap) = reader.try_snapshot()
            && (snap.seq != notif.seq || snap.slot != notif.slot)
        {
            return;
        }
        let Ok(slot_bytes) = reader.read_slot_exact(notif.slot, frame_len) else {
            return;
        };
        // Copy out (must happen before the writer overwrites the slot), then
        // decode off the GPUI thread.
        // No R/B swap: ffmpeg emits BGRA (-pix_fmt bgra) for GPUI's Bgra8Unorm.
        let pixels = slot_bytes.to_vec();
        let me = cx.entity();
        cx.spawn(async move |_this, cx| {
            let img = cx
                .background_executor()
                .spawn(async move {
                    image::RgbaImage::from_raw(w, h, pixels)
                        .map(|rgba| Arc::new(RenderImage::new(vec![image::Frame::new(rgba)])))
                })
                .await;
            let _ = me.update(cx, |this, cx| {
                if let Some(img) = img {
                    if this.width != w || this.height != h {
                        this.width = w;
                        this.height = h;
                    }
                    this.previous_frame = this.current_frame.take();
                    this.current_frame = Some(img);
                    this.frames = this.frames.saturating_add(1);
                    cx.notify();
                }
            });
        })
        .detach();
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
        if new_state {
            self.note("Phone display off (mirror active)", cx);
        } else {
            self.note("Phone display on", cx);
        }
    }

    fn send_touch(&mut self, action: i32, x: u32, y: u32, pressure: f32) {
        if !self.running {
            return;
        }
        let (dw, dh) = match (self.width, self.height) {
            (w, h) if w > 0 && h > 0 => (w as f32, h as f32),
            _ => (1080.0, 2400.0),
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
            _ => (1080.0, 2400.0),
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

    fn rotate_device(&mut self) {
        if !self.running {
            return;
        }
        if let Some(ref tx) = self.control_tx {
            let _ = tx.try_send(ControlMessage::RotateDevice);
        }
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

    fn start(&mut self, cx: &mut Context<Self>) {
        let Some(device) = self.device.clone() else {
            return;
        };
        // Audio defaults on (daemon default); no toggle is exposed yet.
        let options = serde_json::json!({ "preset": self.preset, "audio": true }).to_string();
        let me = cx.entity();
        cx.spawn(async move |_this, cx| {
            // Attach to an already-running session instead of killing it
            // (e.g. a floating window on the same device).
            if let Ok(st) = daemon::remote_status(&device).await
                && st.get("running").and_then(|r| r.as_bool()) == Some(true)
            {
                let _ = me.update(cx, |this, cx| {
                    apply_status(this, &st, cx);
                    this.note("Mirror attached to running session", cx);
                    this.start_poll(cx);
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
                    this.note("Mirror started", cx);
                    this.start_poll(cx);
                }
                Err(e) => this.fail(daemon::explain(&e), cx),
            });
        })
        .detach();
    }

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
        self.screenshot_path = None;
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
                    this.note("Mirror stopped", cx);
                }
                Err(e) => this.fail(daemon::explain(&e), cx),
            });
        })
        .detach();
    }

    fn refresh_status(&mut self, cx: &mut Context<Self>) {
        let Some(device) = self.device.clone() else {
            return;
        };
        let me = cx.entity();
        cx.spawn(async move |_this, cx| {
            let result = daemon::remote_status(&device).await;
            let _ = me.update(cx, |this, cx| {
                if let Ok(v) = result {
                    apply_status(this, &v, cx);
                    if this.running && !this.polling {
                        this.start_poll(cx);
                    }
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
                Ok(path) => {
                    this.screenshot_path = Some(path);
                    this.note("Screenshot saved", cx);
                }
                Err(e) => this.fail(daemon::explain(&e), cx),
            });
        })
        .detach();
    }

    fn on_frame_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Give keyboard focus to mirror view
        self.focus_handle.focus(window);

        if !self.running {
            return;
        }
        if event.button != MouseButton::Left {
            return;
        }
        let Some((tx, ty)) = self.window_to_device(event.position) else {
            return;
        };
        self.is_dragging = true;
        self.last_drag = Some((tx, ty));
        self.send_touch(motion_action::DOWN, tx, ty, 1.0);
        cx.notify();
    }

    fn on_frame_mouse_move(
        &mut self,
        event: &MouseMoveEvent,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
        if !self.running || !self.is_dragging {
            return;
        }
        let Some((tx, ty)) = self.window_to_device(event.position) else {
            return;
        };
        if self.last_drag == Some((tx, ty)) {
            return;
        }
        self.last_drag = Some((tx, ty));
        self.send_touch(motion_action::MOVE, tx, ty, 1.0);
    }

    fn on_frame_mouse_up(
        &mut self,
        event: &MouseUpEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.running {
            self.is_dragging = false;
            return;
        }
        if event.button != MouseButton::Left || !self.is_dragging {
            return;
        }
        self.is_dragging = false;
        let (tx, ty) = self
            .window_to_device(event.position)
            .or(self.last_drag)
            .unwrap_or((0, 0));
        self.last_drag = None;
        self.send_touch(motion_action::UP, tx, ty, 0.0);
        self.note(format!("Touch at {tx}, {ty}"), cx);
    }

    fn on_frame_mouse_up_out(
        &mut self,
        event: &MouseUpEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.running {
            self.is_dragging = false;
            return;
        }
        if event.button != MouseButton::Left || !self.is_dragging {
            return;
        }
        self.is_dragging = false;
        let (tx, ty) = self.last_drag.take().unwrap_or((0, 0));
        self.send_touch(motion_action::UP, tx, ty, 0.0);
        self.note(format!("Release at {tx}, {ty}"), cx);
    }

    fn on_frame_scroll(
        &mut self,
        event: &ScrollWheelEvent,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
        if !self.running {
            return;
        }
        let Some((tx, ty)) = self.window_to_device(event.position) else {
            return;
        };
        let delta = event.delta.pixel_delta(px(20.0));
        let hscroll = (f32::from(delta.x) / 10.0).clamp(-16.0, 16.0);
        let vscroll = (f32::from(delta.y) / 10.0).clamp(-16.0, 16.0);
        if hscroll.abs() > 0.01 || vscroll.abs() > 0.01 {
            self.send_scroll(tx, ty, hscroll, vscroll);
        }
    }

    fn send_key(&mut self, cx: &mut Context<Self>, label: &str, keycode: &str) {
        if !self.running {
            return;
        }
        let numeric_keycode = keycode.parse::<i32>().ok();
        if let (Some(tx), Some(code)) = (&self.control_tx, numeric_keycode) {
            let down = ControlMessage::InjectKeycode {
                action: key_action::DOWN,
                keycode: code,
                repeat: 0,
                metastate: 0,
            };
            let up = ControlMessage::InjectKeycode {
                action: key_action::UP,
                keycode: code,
                repeat: 0,
                metastate: 0,
            };
            let _ = tx.try_send(down);
            let _ = tx.try_send(up);
            self.note(format!("Sent {label}"), cx);
        } else {
            self.fail("control channel unavailable", cx);
        }
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
                    self.note("Pasted clipboard to phone", cx);
                }
                return;
            } else if key.eq_ignore_ascii_case("c") {
                if let Some(ref tx) = self.control_tx {
                    let _ = tx.try_send(ControlMessage::GetClipboard { copy: true });
                    self.note("Requested phone clipboard", cx);
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
                        this.note(format!("Installing {name}…"), cx);
                    });
                    let res = daemon::start_install(&device, &path.to_string_lossy()).await;
                    let _ = me.update(cx, |this, cx| match res {
                        Ok(_) => this.note(format!("Installed {name}"), cx),
                        Err(e) => this.fail(format!("Install failed: {e}"), cx),
                    });
                } else {
                    let _ = me.update(cx, |this, cx| {
                        this.note(format!("Pushing {name} to /sdcard/Download/…"), cx);
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
                    let _ = me.update(cx, |this, cx| match res {
                        Ok(_) => this.note(format!("Pushed {name} to Download"), cx),
                        Err(e) => this.fail(format!("Push failed: {e}"), cx),
                    });
                }
            }
        })
        .detach();
    }
}

impl Drop for Mirror {
    fn drop(&mut self) {
        if self.screen_off
            && let Some(ref tx) = self.control_tx
        {
            let _ = tx.try_send(ControlMessage::SetDisplayPower { on: true });
        }
    }
}

fn apply_status(this: &mut Mirror, v: &serde_json::Value, cx: &mut Context<Mirror>) {
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

fn rail_btn(
    t: &Palette,
    id: impl Into<gpui::ElementId>,
    icon_name: &'static str,
    hint: &'static str,
    enabled: bool,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut gpui::App) + 'static,
) -> impl IntoElement {
    let mut btn = div()
        .id(id)
        .size(px(34.0))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(theme::RADIUS_CAPSULE_BTN));

    if enabled {
        btn = btn
            .cursor_pointer()
            .text_color(t.text_secondary)
            .hover(|s| s.bg(t.secondary_bg_hover).text_color(t.text_header))
            .active(|s| s.bg(t.secondary_bg))
            .tooltip(ui::hover_hint(hint))
            .child(icons::icon(icon_name, 16.0, t.text_secondary))
            .on_click(on_click);
    } else {
        btn = btn
            .opacity(0.35)
            .cursor_not_allowed()
            .text_color(t.text_muted)
            .child(icons::icon(icon_name, 16.0, t.text_muted));
    }
    btn
}

impl Render for Mirror {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if let Some(prev) = self.previous_frame.take() {
            let _ = window.drop_image(prev);
        }
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
                        .child("Pick a phone to mirror its screen"),
                )
                .child(
                    div()
                        .mt(px(6.0))
                        .text_size(t.text(12.0))
                        .text_color(t.text_muted)
                        .child("The mirror starts from the selected device — server, tunnel and socket stay in the daemon."),
                )
                .into_any_element();
        }

        let (inner_w, inner_h, outer_w, outer_h) = self.phone_dims();

        // Phone frame: prefer in-memory RenderImage, fallback to screenshot file path
        let me_start = cx.entity();
        let frame: AnyElement = if let Some(img) = &self.current_frame {
            gpui::img(ImageSource::Render(img.clone()))
                .size_full()
                .object_fit(gpui::ObjectFit::Fill)
                .into_any_element()
        } else if let Some(path) = &self.screenshot_path {
            gpui::img(ImageSource::from(PathBuf::from(path)))
                .size_full()
                .object_fit(gpui::ObjectFit::Fill)
                .into_any_element()
        } else {
            div()
                .size_full()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap(px(14.0))
                .bg(gpui::rgb(0x0e1013))
                .child(
                    div()
                        .size(px(48.0))
                        .rounded_full()
                        .bg(t.secondary_bg)
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(icons::icon(names::PHONE, 24.0, t.text_secondary)),
                )
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .items_center()
                        .gap(px(4.0))
                        .child(
                            div()
                                .text_size(t.text(14.0))
                                .text_color(t.text_header)
                                .child(if self.running {
                                    "Waiting for frames…"
                                } else {
                                    "Mirror is stopped"
                                }),
                        )
                        .child(
                            div()
                                .text_size(t.text(11.0))
                                .text_color(t.text_muted)
                                .child(if self.running {
                                    "Streaming screen over adb tunnel…"
                                } else {
                                    "Click Start mirror to begin streaming"
                                }),
                        ),
                )
                .when(!self.running, |d| {
                    d.child(ui::button(
                        &t,
                        "placeholder-start-btn",
                        "Start mirror",
                        true,
                        {
                            let me = me_start.clone();
                            move |_, _w, cx| {
                                me.update(cx, |this, cx| this.start(cx));
                            }
                        },
                    ))
                })
                .into_any_element()
        };

        // Front-facing camera hole punch cutout
        let is_landscape = inner_w > inner_h;
        let camera_cutout = if is_landscape {
            div()
                .absolute()
                .left(px(8.0))
                .top(px((inner_h - 12.0) / 2.0))
                .size(px(12.0))
                .rounded_full()
                .bg(gpui::rgb(0x0a0a0c))
                .border_1()
                .border_color(gpui::rgba(0xffffff18))
                .flex()
                .items_center()
                .justify_center()
                .child(
                    div()
                        .size(px(4.0))
                        .rounded_full()
                        .bg(gpui::rgba(0xffffff22)),
                )
        } else {
            div()
                .absolute()
                .top(px(8.0))
                .left(px((inner_w - 12.0) / 2.0))
                .size(px(12.0))
                .rounded_full()
                .bg(gpui::rgb(0x0a0a0c))
                .border_1()
                .border_color(gpui::rgba(0xffffff18))
                .flex()
                .items_center()
                .justify_center()
                .child(
                    div()
                        .size(px(4.0))
                        .rounded_full()
                        .bg(gpui::rgba(0xffffff22)),
                )
        };

        // Inner screen element where gestures are captured
        let mut inner_screen = div()
            .id("mirror-inner-screen")
            .w(px(inner_w))
            .h(px(inner_h))
            .rounded(px(26.0))
            .border_1()
            .border_color(gpui::rgba(0x00000088))
            .bg(gpui::rgb(0x000000))
            .overflow_hidden()
            .relative();

        if self.running {
            let me_bk = cx.entity();
            inner_screen = inner_screen
                .cursor_pointer()
                .on_mouse_down(MouseButton::Left, cx.listener(Mirror::on_frame_mouse_down))
                .on_mouse_down(MouseButton::Right, {
                    let me = me_bk;
                    move |_, _w, cx| {
                        me.update(cx, |this, cx| this.send_key(cx, "Back", "4"));
                    }
                })
                .on_mouse_move(cx.listener(Mirror::on_frame_mouse_move))
                .on_mouse_up(MouseButton::Left, cx.listener(Mirror::on_frame_mouse_up))
                .on_mouse_up_out(
                    MouseButton::Left,
                    cx.listener(Mirror::on_frame_mouse_up_out),
                )
                .on_scroll_wheel(cx.listener(Mirror::on_frame_scroll));
        }

        let inner_screen = inner_screen.child(frame).child(camera_cutout);

        // Hardware button notches on phone outer shell
        let volume_bump = div()
            .absolute()
            .top(px(outer_h * 0.22))
            .right(px(-2.5))
            .w(px(3.0))
            .h(px((outer_h * 0.12).clamp(40.0, 70.0)))
            .rounded_r(px(2.0))
            .bg(gpui::rgb(0x3a3e48));

        let power_bump = div()
            .absolute()
            .top(px(outer_h * 0.38))
            .right(px(-2.5))
            .w(px(3.0))
            .h(px((outer_h * 0.07).clamp(24.0, 44.0)))
            .rounded_r(px(2.0))
            .bg(gpui::rgb(0x3a3e48));

        // Phone chassis
        let chassis = div()
            .id("phone-chassis")
            .w(px(outer_w))
            .h(px(outer_h))
            .rounded(px(36.0))
            .bg(gpui::rgb(0x181a1f))
            .border_2()
            .border_color(gpui::rgb(0x323640))
            .shadow_lg()
            .p(px(BEZEL))
            .relative()
            .child(volume_bump)
            .child(power_bump)
            .child(inner_screen);

        let me = cx.entity();
        let preset = self.preset.clone();
        let preset_row = div()
            .flex()
            .items_center()
            .gap(px(4.0))
            .children(PRESETS.iter().map(|p| {
                let p = (*p).to_string();
                let active = preset == p;
                let me = me.clone();
                let id = ui::el_id(format!("mirror-preset:{p}"));
                div()
                    .id(id)
                    .px(px(8.0))
                    .py(px(4.0))
                    .rounded(px(theme::RADIUS_CAPSULE_BTN))
                    .text_size(t.text(11.0))
                    .text_color(if active {
                        t.text_on_light
                    } else {
                        t.text_secondary
                    })
                    .bg(if active {
                        t.text_header
                    } else {
                        t.secondary_bg
                    })
                    .cursor_pointer()
                    .hover(|s| s.bg(t.secondary_bg_hover))
                    .child(preset_label(&p))
                    .on_click({
                        let me = me.clone();
                        move |_, _w, cx| {
                            me.update(cx, |this, cx| {
                                this.preset = p.clone();
                                cx.notify();
                            });
                        }
                    })
                    .into_any_element()
            }))
            .into_any_element();

        let me2 = cx.entity();
        let me_flt = me2.clone();
        let dev_opt = self.device.clone();
        let lbl_flt = self.device_label.clone();
        let preset_flt = self.preset.clone();
        let (init_w, init_h) = (self.width, self.height);
        let float_btn = ui::button(
            &t,
            "mirror-float",
            "Float window",
            false,
            move |_, _w, cx| {
                if let Some(dev) = dev_opt.clone() {
                    me_flt.update(cx, |this, cx| {
                        if this.running {
                            this.stop(cx);
                        }
                    });
                    crate::views::floating_mirror::open_floating_mirror(
                        dev,
                        lbl_flt.clone(),
                        preset_flt.clone(),
                        init_w,
                        init_h,
                        cx,
                    );
                }
            },
        );

        let start_stop: AnyElement = if self.running {
            ui::danger_button(&t, "mirror-stop", "Stop mirror", {
                let me = me2.clone();
                move |_, _w, cx| {
                    me.update(cx, |this, cx| this.stop(cx));
                }
            })
            .into_any_element()
        } else {
            ui::button(&t, "mirror-start", "Start mirror", true, {
                let me = me2.clone();
                move |_, _w, cx| {
                    me.update(cx, |this, cx| this.start(cx));
                }
            })
            .into_any_element()
        };

        let stats = if self.running {
            format!(
                "{} • {}×{} • {:.1} fps • {}",
                if self.codec.is_empty() {
                    "streaming"
                } else {
                    self.codec.as_str()
                },
                self.width,
                self.height,
                self.fps_x10 as f32 / 10.0,
                human(self.bytes),
            )
        } else {
            "Stopped".to_string()
        };

        let status_badge: Option<AnyElement> = if let Some(err) = &self.error {
            Some(
                div()
                    .px(px(8.0))
                    .py(px(3.0))
                    .rounded(px(theme::RADIUS_CAPSULE_BTN))
                    .bg(t.danger_soft)
                    .text_size(t.text(11.0))
                    .text_color(t.danger)
                    .child(err.clone())
                    .into_any_element(),
            )
        } else {
            self.status.as_ref().map(|note| {
                div()
                    .px(px(8.0))
                    .py(px(3.0))
                    .rounded(px(theme::RADIUS_CAPSULE_BTN))
                    .bg(t.success_soft)
                    .text_size(t.text(11.0))
                    .text_color(t.success)
                    .child(note.clone())
                    .into_any_element()
            })
        };

        let rail_enabled = self.running;
        let screen_off = self.screen_off;
        // Vertical action rail docked to the right of the phone chassis
        let rail = div()
            .flex()
            .flex_col()
            .items_center()
            .gap(px(6.0))
            .p(px(5.0))
            .rounded(px(theme::RADIUS_CARD))
            .bg(t.track)
            .border_1()
            .border_color(t.border)
            .shadow_sm()
            .child(rail_btn(
                &t,
                "rail-home",
                names::HOME,
                "Home",
                rail_enabled,
                {
                    let me = me2.clone();
                    move |_, _w, cx| {
                        me.update(cx, |this, cx| this.send_key(cx, "Home", "3"));
                    }
                },
            ))
            .child(rail_btn(
                &t,
                "rail-back",
                names::GO_PREVIOUS,
                "Back",
                rail_enabled,
                {
                    let me = me2.clone();
                    move |_, _w, cx| {
                        me.update(cx, |this, cx| this.send_key(cx, "Back", "4"));
                    }
                },
            ))
            .child(rail_btn(
                &t,
                "rail-recents",
                names::VIEW_GRID,
                "Overview / Recents",
                rail_enabled,
                {
                    let me = me2.clone();
                    move |_, _w, cx| {
                        me.update(cx, |this, cx| this.send_key(cx, "Recents", "187"));
                    }
                },
            ))
            .child(rail_btn(
                &t,
                "rail-screen-off",
                names::PHONE,
                if screen_off {
                    "Turn phone screen on"
                } else {
                    "Turn phone screen off (mirror stays active)"
                },
                rail_enabled,
                {
                    let me = me2.clone();
                    move |_, _w, cx| {
                        me.update(cx, |this, cx| this.toggle_screen_off(cx));
                    }
                },
            ))
            .child(div().w(px(20.0)).h(px(1.0)).bg(t.border).my(px(2.0)))
            .child(rail_btn(
                &t,
                "rail-volup",
                names::TEXT_LARGER,
                "Volume Up",
                rail_enabled,
                {
                    let me = me2.clone();
                    move |_, _w, cx| {
                        me.update(cx, |this, cx| this.send_key(cx, "Volume up", "24"));
                    }
                },
            ))
            .child(rail_btn(
                &t,
                "rail-voldn",
                names::TEXT_SMALLER,
                "Volume Down",
                rail_enabled,
                {
                    let me = me2.clone();
                    move |_, _w, cx| {
                        me.update(cx, |this, cx| this.send_key(cx, "Volume down", "25"));
                    }
                },
            ))
            .child(div().w(px(20.0)).h(px(1.0)).bg(t.border).my(px(2.0)))
            .child(rail_btn(
                &t,
                "rail-power",
                names::STOP,
                "Power / Lock",
                rail_enabled,
                {
                    let me = me2.clone();
                    move |_, _w, cx| {
                        me.update(cx, |this, cx| this.send_key(cx, "Power", "26"));
                    }
                },
            ))
            .child(rail_btn(
                &t,
                "rail-shot",
                names::CAMERA_PHOTO,
                "Take Screenshot",
                rail_enabled,
                {
                    let me = me2.clone();
                    move |_, _w, cx| {
                        me.update(cx, |this, cx| this.take_screenshot(cx));
                    }
                },
            ))
            .child(rail_btn(
                &t,
                "rail-rotate",
                names::REFRESH,
                "Rotate Screen",
                rail_enabled,
                {
                    let me = me2.clone();
                    move |_, _w, cx| {
                        me.update(cx, |this, _cx| {
                            this.rotate_device();
                        });
                    }
                },
            ));

        div()
            .flex()
            .flex_col()
            .size_full()
            .bg(t.canvas)
            .track_focus(&self.focus_handle)
            .on_mouse_move(cx.listener(Mirror::on_frame_mouse_move))
            .on_mouse_up(MouseButton::Left, cx.listener(Mirror::on_frame_mouse_up))
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(Mirror::on_frame_mouse_up_out),
            )
            .on_key_down(cx.listener(Mirror::on_key_down))
            .on_drop::<ExternalPaths>(cx.listener(Mirror::on_external_drop))
            // Header bar
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(10.0))
                    .h(px(HEADER_H))
                    .px(px(14.0))
                    .border_b_1()
                    .border_color(t.border)
                    .child(icons::icon(names::PHONE, 16.0, t.text_secondary))
                    .child(
                        div()
                            .text_size(t.text(14.0))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(t.text_header)
                            .child(if self.device_label.is_empty() {
                                "Phone Mirror".to_string()
                            } else {
                                self.device_label.clone()
                            }),
                    )
                    .child(
                        div()
                            .px(px(8.0))
                            .py(px(2.5))
                            .rounded(px(theme::RADIUS_CAPSULE_BTN))
                            .bg(t.track)
                            .border_1()
                            .border_color(t.border)
                            .text_size(t.text(11.0))
                            .text_color(t.text_muted)
                            .child(stats),
                    )
                    .when_some(status_badge, |d, badge| d.child(badge))
                    .child(div().flex_1())
                    .child(preset_row)
                    .child(float_btn)
                    .child(start_stop)
                    .into_any_element(),
            )
            // Centered stage with phone chassis and docked action rail
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(GAP))
                            .child(chassis)
                            .child(rail),
                    )
                    .into_any_element(),
            )
            .into_any_element()
    }
}

fn preset_label(p: &str) -> &'static str {
    match p {
        "performance" => "Performance",
        "quality" => "Quality",
        _ => "Balanced",
    }
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

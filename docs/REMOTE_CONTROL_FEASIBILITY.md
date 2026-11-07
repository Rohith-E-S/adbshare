# Real-time phone remote control — feasibility study and plan

## Goal

Apple-like “iPhone Mirroring” for Android in adbshare: run the mobile apps on the phone, show the phone screen in the Linux desktop GUI, and let the user control it with mouse/keyboard/audio. This is screen *mirroring + input*, not moving app execution to the PC.

## TL;DR feasibility

**Feasible as a core feature on Android via ADB. Not feasible as a pixel-perfect Apple clone without OS/OEM integration.**

- Best technical route: **scrcpy-style server + Rust client**. A small Java server is pushed to `/data/local/tmp`, run via `app_process` as `shell`, captures the display with `MediaCodec`, and injects input via hidden/system APIs. The desktop client decodes H.264/H.265 + Opus and renders in GPUI.
- Expected performance on USB: **30–120 fps, ~35–70 ms latency** class, similar to scrcpy’s reported range. Wireless will be worse and depends on ADB-over-Wi-Fi stability.
- Hard limits to set expectations honestly:
  - Apps must still run on the phone; adbshare can mirror/control them, not virtualize them away.
  - Apple’s locked-phone, trusted-device, seamless Continuity UX cannot be reproduced on stock Android via ADB. scrcpy can turn the phone screen off while mirroring, but the device remains “unlocked” in Android terms.
  - Secure/DRM windows (banking, Netflix/Hulu-style content) may be blanked by Android.
  - Android 15+ virtual-display and MediaProjection rules are stricter; primary-display mirroring is the safe baseline.

## What Apple does and what it implies for Android

Apple’s iPhone Mirroring is an OS feature: a trusted device pairing, live video/audio, mouse/keyboard/trackpad input, auto-rotation, drag-and-drop, notifications/Live Activities, inactivity pause, and the iPhone staying locked. It even refuses DRM video in some apps. The lesson for adbshare: the *user-visible* target is a single “phone window” in the desktop, but under the hood Android will not give us the same OS-level guarantees. We should copy the UX shape, not promise the same security model.

Useful references:
- Apple: “iPhone Mirroring lets you see and use your iPhone right from your Mac” and requires the same Apple ID / trusted setup: https://www.apple.com/macos/continuity ; support overview: https://support.apple.com/en-us/120421
- scrcpy: mirrors Android video/audio over USB/TCP-IP, keyboard/mouse control, no root/no installed app, 30–120 fps, 35–70 ms latency focus, Apache-2.0: https://github.com/Genymobile/scrcpy
- scrcpy developer doc: server pushed to device, separate video/audio/control sockets, MediaCodec encode, hidden `InputManager.injectInputEvent()`, FFmpeg decode on client: https://github.com/Genymobile/scrcpy/blob/master/doc/develop.md
- Android MediaProjection docs: consent per session, virtual display, Android 14 foreground-service/single-use token rules, Android 15 status chip/auto-stop on lock: https://developer.android.com/media/grow/media-projection

## Options considered

| Approach | How it works | Pros | Cons | Verdict |
|---|---|---|---|---|
| **A. scrcpy-style ADB server** | Push Java server JAR to `/data/local/tmp`; run with `app_process`; `MediaCodec` encodes screen; separate video/audio/control sockets over `adb forward/reverse`; inject input via `InputManager` | Proven, lowest latency, no root, no installed app, fits adbshare’s helper-push model; audio on Android 11+; UHID options | Hidden APIs may change; vendor quirks (e.g. Xiaomi `INJECT_EVENTS` needs “USB debugging (Security Settings)”); virtual display permissions on Android 15+; secure windows blank | **Recommended baseline** |
| **B. MediaProjection companion app** | Install an Android app; user grants screen-capture consent; app captures via `VirtualDisplay` + `MediaCodec`/WebRTC | Official API, clearer consent, per-app sharing on Android 14+, status chip, better Play-policy posture | Requires app install + consent every session; one-time token on Android 14+; input still needs AccessibilityService or ADB; less seamless, more friction | Good as a later “private/share” mode, not the Apple-like default |
| **C. WebRTC over MediaProjection** | Companion app captures screen/audio and sends to desktop via WebRTC; desktop sends input back | Great for NAT/WAN, congestion control, encryption, future cloud/remote use; 0.2–0.5s class latency | Most components; consent friction; still needs input channel; higher battery/complexity | Phase 3, after local ADB path works |
| **D. ADB `screencap` + `input` shell** | Repeated PNG screenshots + `adb shell input tap/swipe` | Already partly present (`screencap` in daemon, tap/swipe in tools UI) | ~seconds latency, no smooth scrolling/drag, no audio, no continuous video | Only a fallback/debug tool |

## Recommended architecture for adbshare

### 1. New crate: `adb-remote`

Responsibilities:
- Session protocol types: video/audio/control channel framing, PTS/flags, codec IDs, dimensions, clipboard/device messages.
- Quality presets and capability negotiation.
- Input message encoding: key, text, pointer motion/button/scroll, touch, clipboard, screen on/off/wake, resize.
- Demuxing for H.264/H.265/AV1 and Opus/AAC/FLAC/raw per scrcpy-compatible protocol.

Keep it independent of GPUI so it can be unit-tested without a display.

### 2. Device server: reuse scrcpy server first, own server later

Phase 1 should **bundle a pinned scrcpy-server JAR** (Apache-2.0) and push it to `/data/local/tmp/adbshare-scrcpy-server.jar`, mirroring how the daemon already pushes `/data/local/tmp/adbshare-proxy`. The daemon starts it with `app_process` and a random `scid`.

Long-term, replace it with an adbshare-owned Java/Kotlin server implementing the same internal protocol so we are not locked to scrcpy’s version churn. That is a significant Android project; do not block the MVP on it.

### 3. Transport

Reuse ADB tunnels rather than D-Bus for media:
- `adb reverse localabstract:adbshare_<scid> tcp:<host-port>` by default (scrcpy’s preferred direction), with `adb forward` fallback.
- Three logical sockets: **video**, **audio**, **control**. Control is bidirectional (input client→device, clipboard device→client).
- Allocate one ephemeral host port per device/session; store in daemon state next to `logcat` streams and FUSE mounts.

Do **not** send video frames or per-event input over D-Bus. D-Bus is for `start_remote_session`, `stop_remote_session`, `remote_status`, quality/settings. Media and input need a local Unix socket or TCP channel owned by the daemon/GUI.

### 4. Daemon integration

Extend `ManagerInterface` with:
- `start_remote(device, options_json) -> session_id`
- `stop_remote(device) -> bool`
- `remote_status(device) -> json` (running, fps, bitrate, latency, size, codec, errors)
- `remote_event(device, input_json)` only for low-frequency commands; high-frequency pointer/key events go over the session socket.

Daemon owns: tunnel setup, server push/start, process supervision, reconnect, stats, and cleanup on `device Offline` like existing FUSE/logcat handling.

### 5. GUI integration

New “Mirror” / “Remote control” view, and a device-tools entry:
- A phone-shaped window with aspect-ratio letterboxing, rotation handling, scaling to fit, fullscreen toggle.
- Mouse: left = touch tap/drag, right = Back, middle = Home (scrcpy convention), wheel = scroll, pinch via modifiers where practical.
- Keyboard: map host keys to Android keycodes; optional UHID physical keyboard mode.
- Toolbar: quality preset, bitrate/fps, audio on/off, clipboard sync, screen on/off, wake, lock-stop, stop mirroring, recording.
- Status bar inside the view: codec, resolution, fps, latency estimate, bitrate, dropped frames.

GPUI needs a video-frame path: decode with FFmpeg (`ffmpeg-next` or FFI to system libav) → upload frames as GPU textures → draw with letterboxing. This is the biggest GUI unknown; spike it early.

### 6. Audio

- Capture playback with `AudioRecord` + `AudioPlaybackCaptureConfiguration` equivalents on the server side; encode **Opus 48 kHz** by default, AAC fallback.
- Client plays through the desktop audio device with a small jitter buffer; add volume/mute in the mirror toolbar.
- Expect some apps/devices to block capture; degrade to video-only, not an error.

### 7. Security and UX consent

Because the ADB path has no Android status chip, adbshare must make consent explicit:
- Mirror only starts from an explicit user action; show a persistent “Mirroring <model> — Stop” banner.
- Auto-stop after inactivity (Apple does this), on device disconnect, and when the phone locks/screens off unless “keep screen off while mirroring” is chosen.
- Log session start/stop in the daemon log; never send the feed off the local machine.
- Warn that the phone is not in Apple’s locked/trusted-device mode.

## Performance targets

| Mode | Preset | Target |
|---|---|---|
| USB “Performance” | 720p, 30 fps, 2–4 Mb/s | <80 ms click-to-photon |
| USB “Balanced” | 1080p, 60 fps, 8 Mb/s | 16.7 ms frame cadence, <100 ms |
| USB “Quality” | native/1440p, 60 fps, 16 Mb/s | best text clarity, more heat |
| Wi-Fi | 720p/1080p adaptive | accept 100–300 ms, prioritize smoothness over resolution |

Key rules from scrcpy practice: decode ASAP, avoid extra buffering, use hardware decode where available, request keyframes on resize/rotate, and let the server restart the encode session on rotation/fold.

## Risks and mitigations

| Risk | Impact | Mitigation |
|---|---|---|
| Hidden API changes across Android versions | Server breaks after OS updates | Pin server version; CI matrix Android 10–16; fallback to screenshot/input tool |
| Vendor input permission quirks | Taps/keyboard rejected (e.g. Xiaomi) | Detect `INJECT_EVENTS` failure; show exact device setting to enable |
| Android 15 virtual display permission | “New display” mode fails | Default to primary display; treat virtual display as opt-in |
| DRM/secure windows | Black screen for banking/video apps | Show “content hidden by Android” state; never try to bypass |
| FFmpeg/GPUI texture pipeline | Schedule risk | Time-box a spike; if GPUI textures are painful, render via a native video widget first |
| Packaging deps | FFmpeg/libav, VAAPI, Opus not present everywhere | Keep video-only MVP working with software decode; document optional packages |

## Phased plan

### Phase 0 — Spike (1–2 weeks)
- Bundle/run a pinned scrcpy server manually with `app_process`; prove H.264 video + tap/swipe + keyboard from a Rust test client.
- Spike GPUI texture display of decoded frames; measure fps/latency on USB and Wi-Fi.
- Exit criteria: 1080p30 playable, <150 ms input latency, rotation resize handled.

### Phase 1 — Integrated MVP (4–6 weeks)
- Add `adb-remote` crate, daemon session manager, D-Bus methods, GUI mirror view, quality presets, clipboard sync, screen on/off/wake, error states.
- Tests: protocol framing, daemon state cleanup, D-Bus validation.
- Exit criteria: one-device mirror/control in the app without terminal commands.

### Phase 2 — Polish (6–10 weeks)
- Audio playback, recording, UHID keyboard/mouse, gamepad basics, drag-and-drop via existing transfer queue, multi-device isolation (one session per device), packaging/dependency docs.
- Exit criteria: usable for daily phone interaction, not just demos.

### Phase 3 — Apple-like extras (3–6 months)
- WebRTC/WAN mode, stronger pairing/auth, notification/Live-Activity style surfacing where Android permits, own Java server to replace scrcpy protocol, virtual-display improvements.
- Exit criteria: decide whether “remote over internet” is a product goal or local-only stays the scope.

## Effort estimate

- MVP integrated remote control: **8–12 weeks** for one experienced Rust developer.
- Full Apple-comparable experience (audio, polish, virtual display, own server, WebRTC): **6–12 months**.

## Open questions to decide early

1. License strategy: ship pinned scrcpy server (Apache-2.0) vs. fund an owned Java server from the start?
2. Decode stack: system FFmpeg + VAAPI NVDEC/VAAPI, or pure Rust (`openh264` etc.) with lower codec coverage?
3. Target Android floor: API 21 (scrcpy parity) or API 30+ (simpler audio/display handling)?
4. Is Wi-Fi-only/remote-over-network in scope, or USB/ADB-local only?
5. Should “keep phone locked while mirroring” be a goal, knowing stock Android cannot match Apple’s model?

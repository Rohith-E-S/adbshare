# Remote control — agent TODO list

Feasibility background: `docs/REMOTE_CONTROL_FEASIBILITY.md`.
Goal: Apple-like phone window in adbshare — apps run on the phone, desktop shows live video/audio, mouse/keyboard control it.

## Agent rules (apply to every task)

- One task = one PR-sized change. Do not mix protocol, daemon, GUI, and packaging in one task.
- Run before pushing:
  ```sh
  cargo fmt --all
  cargo clippy --workspace --all-targets -- -D warnings
  cargo test -p <crate> <test_name>
  ```
- Keep `adb-gui` free of ADB logic. It calls `org.adbshare.Manager` over D-Bus; daemon owns ADB/video/control streams.
- Never send video frames or high-frequency input over D-Bus. D-Bus is session control only. Media/input use local sockets.
- One remote session per device. Store session state in daemon `State` next to `logcat` and FUSE mounts.
- No root required for MVP. If a device needs extra settings (e.g. Xiaomi `INJECT_EVENTS`), surface that as an error message, not a workaround.
- Scrcpy is Apache-2.0; adbshare is GPL-3.0. If bundling `scrcpy-server`, keep it a separate artifact with attribution; do not paste its source into GPL files without review.
- Update `docs/ARCHITECTURE.md` when a new crate, D-Bus method, or protocol is added.

## Key files

- Workspace: `Cargo.toml`
- Daemon: `crates/adb-daemon/src/main.rs` (`org.adbshare.Manager`, device setup, `adb forward`, screenshot/logcat)
- GUI D-Bus client: `crates/adb-gui/src/state/daemon.rs`
- GUI views: `crates/adb-gui/src/views/tools.rs` (device tools entry point), new `crates/adb-gui/src/views/mirror.rs`
- Existing helper-deploy pattern: `adb-daemon` pushes proxy to phone; mirror server follows the same pattern to `/data/local/tmp/`
- Wire protocol reference: scrcpy `doc/develop.md` (video/audio/control sockets, codec IDs, session/media packets, dummy byte/metadata)

---

## Milestone 0 — Spikes (do these first, in parallel where noted)

### RC-001 — Prove phone server can stream to host
- **Owner:** daemon/agent
- **Files:** `crates/adb-daemon/src/main.rs` (temporary), new `crates/adb-remote/` skeleton
- **Do:**
  - Add `adb-remote` crate to workspace with protocol types only: `CodecId`, `SessionPacket`, `MediaPacket` header, quality presets.
  - Push pinned `scrcpy-server` JAR to `/data/local/tmp/adbshare-scrcpy-server.jar`.
  - Start it with `app_process` using a random `scid`; set up `adb reverse` + fallback `adb forward`.
  - Read the first video socket: dummy byte, device metadata, codec ID, session dimensions, media packets.
  - Unit tests: header encode/decode, codec IDs (`h264`, `h265`, `av1`, `vp8`, `vp9`), session vs media packet split.
- **Verify:**
  ```sh
  cargo test -p adb-remote
  cargo clippy -p adb-remote --all-targets -- -D warnings
  ```
- **Done when:** Rust test client receives H.264 packets from a real phone and prints resolution/fps.

### RC-002 — Prove control channel works
- **Owner:** daemon/agent (depends on RC-001)
- **Files:** `crates/adb-remote/src/input.rs`, tests
- **Do:** Implement control message encode/decode for tap, swipe/drag, key, text, clipboard, screen on/off/wake. Use scrcpy control-message tests as the reference shape.
- **Verify:** unit tests for each message type; manual tap/swipe on a phone from the test client.
- **Done when:** tap, swipe, Home/Back, text entry, and clipboard round-trip work.

### RC-003 — Prove GPUI can render decoded video
- **Owner:** GUI/agent (parallel with RC-001/002)
- **Files:** new `crates/adb-gui/src/views/mirror.rs` spike only
- **Do:** Decode H.264 with FFmpeg (`ffmpeg-next` or direct libav FFI), upload frames to GPUI textures, draw letterboxed with rotation support. If GPUI textures are painful, use a temporary native video widget and note the blocker.
- **Verify:** `cargo build --release --bin adb-gui`; launch with `./run.sh`; play a 1080p30 stream.
- **Done when:** 1080p30 renders without re-buffering; fps/latency numbers recorded in the PR.

### RC-004 — Measure latency and pick defaults
- **Owner:** any (depends on RC-001..003)
- **Do:** Test USB and Wi-Fi at Performance/Balanced/Quality presets. Record fps, bitrate, click-to-photon estimate, CPU, battery/heat notes.
- **Done when:** default preset chosen and documented in `docs/REMOTE_CONTROL_FEASIBILITY.md`.

---

## Milestone 1 — Integrated MVP

### RC-101 — `adb-remote` protocol crate (full)
- **Depends on:** RC-001, RC-002
- **Files:** `crates/adb-remote/src/{lib,protocol,demux,input,codec,presets}.rs`
- **Do:** Complete demuxing for H.264/H.265/AV1 and Opus/AAC/FLAC/raw; resize/rotate session handling; keyframe-on-resize; stats (fps, bitrate, drops).
- **Verify:** `cargo test -p adb-remote`, property/round-trip tests for headers.
- **Done when:** all protocol tests pass; no GPUI/zbus dependency in this crate.

### RC-102 — Daemon session manager
- **Depends on:** RC-101
- **Files:** `crates/adb-daemon/src/main.rs`
- **Do:** Add `RemoteSession` to daemon `State`; implement server push/start, tunnel setup (`adb reverse` + forward fallback), process supervision, reconnect, cleanup on device Offline.
- **Verify:** start/stop from a small Rust test harness; logs show tunnel + server lifecycle; stale sessions cleaned after unplug.
- **Done when:** one session per device; second start returns a clear error.

### RC-103 — D-Bus control API
- **Depends on:** RC-102
- **Files:** `crates/adb-daemon/src/main.rs`, `crates/adb-gui/src/state/daemon.rs`
- **Do:** Add `start_remote(device, options_json) -> session_id`, `stop_remote(device)`, `remote_status(device) -> json`. Validate all args; cap output; return plain-language errors.
- **Verify:** `busctl --user call org.adbshare.Manager /org/adbshare/Manager org.adbshare.Manager remote_status s <serial>`; clippy clean.
- **Done when:** GUI can start/stop/query without any ADB logic.

### RC-104 — GUI mirror view (video + mouse)
- **Depends on:** RC-003, RC-103
- **Files:** `crates/adb-gui/src/views/mirror.rs`, `tools.rs` entry point, `state/daemon.rs`
- **Do:** Phone-shaped window, aspect-fit letterboxing, rotation handling, mouse tap/drag/scroll mapping, right=Back / middle=Home, toolbar with preset/bitrate/fps/audio toggle/screen on-off/wake/stop.
- **Verify:** `./run.sh`; mirror from device tools; stop banner visible; GUI stays responsive during stream.
- **Done when:** user can open mirror, interact, and stop without leaving the app.

### RC-105 — Keyboard + clipboard + errors
- **Depends on:** RC-104
- **Files:** `mirror.rs`, `adb-remote/src/input.rs`
- **Do:** Map host keyboard to Android keycodes; text input; bidirectional clipboard; error states for `INJECT_EVENTS` failure, black/DRM content, server crash, Wi-Fi drop.
- **Verify:** type in phone app; copy/paste both directions; unplug mid-session recovers cleanly.
- **Done when:** keyboard and clipboard work; failures explain themselves in the UI.

### RC-106 — Docs and packaging notes
- **Depends on:** RC-101..105
- **Files:** `README.md`, `docs/ARCHITECTURE.md`, `docs/DEVELOPMENT.md`, `packaging/*` if needed
- **Do:** Document FFmpeg/libav/Opus deps, quality presets, device quirks (Xiaomi setting, Android 15 virtual display, DRM black screens), and that mirroring is local-only.
- **Done when:** a fresh clone can build and mirror following the docs.

---

## Milestone 2 — Polish

### RC-201 — Audio playback
- Capture via server `AudioRecord`/playback capture, Opus 48 kHz default; desktop playback with jitter buffer and mute/volume. Degrade to video-only when capture is blocked.

### RC-202 — Recording
- Mux video+audio to MKV/MP4 on the client via FFmpeg; reuse existing “Record screen (3 min)” UX in tools.

### RC-203 — UHID keyboard/mouse + gamepad
- Add physical HID modes as advanced options; keep SDK injection as default.

### RC-204 — Drag-and-drop files
- Drop local files onto mirror → existing transfer queue → phone; drag phone files out via FUSE mount.

### RC-205 — Multi-device and reconnect
- Isolate sessions per serial; auto-reconnect with backoff; keep file browser usable while mirroring.

---

## Milestone 3 — Apple-like extras (only after MVP is stable)

### RC-301 — WebRTC/WAN mode
- MediaProjection capture + WebRTC transport for remote access beyond ADB; keep local ADB path as default.

### RC-302 — Owned device server
- Replace pinned scrcpy server with adbshare-owned Java/Kotlin server behind the same protocol; remove version-lock risk.

### RC-303 — Notifications / Live-Activity style surfacing
- Surface selected phone notifications in desktop UI where Android permits; respect user opt-in per app.

### RC-304 — Virtual display mode
- Opt-in new-display mirroring (Android 15+ permission aware); primary display remains default.

---

## Test matrix (run for each milestone)

- Devices: Pixel-class Android 14/15/16; Xiaomi (input-permission quirk); Samsung fold/rotation if available.
- Transports: USB, ADB-over-Wi-Fi.
- Presets: Performance, Balanced, Quality.
- Scenarios: rotation, fold, lock/screen-off, unplug mid-session, Wi-Fi drop, DRM app (expect black), banking app (expect black/secure), text entry, clipboard, audio on/off.
- Commands:
  ```sh
  adb devices -l
  adb -s SERIAL shell getprop ro.build.version.release
  adb -s SERIAL shell wm size
  ./run.sh
  tail -f /tmp/adb-daemon.log /tmp/adb-gui.log
  cargo test --workspace
  cargo clippy --workspace --all-targets -- -D warnings
  cargo fmt --all --check
  ```

## Definition of done (MVP)

- [ ] Mirror starts/stops from the GUI with no terminal commands
- [ ] Video at 1080p30-class on USB with acceptable input latency
- [ ] Tap/drag/scroll/keyboard/text/clipboard work
- [ ] Screen on/off/wake and stop banner work
- [ ] Errors are plain-language (input permission, DRM black screen, disconnect)
- [ ] `cargo fmt`, `cargo clippy`, `cargo test --workspace` pass
- [ ] Docs updated (`README`, `ARCHITECTURE`, feasibility note)

## Suggested fan-out order

1. RC-001, RC-002, RC-003 in parallel (spikes).
2. RC-004 after spikes.
3. RC-101 → RC-102 → RC-103 → RC-104 → RC-105 → RC-106 sequentially.
4. Milestone 2/3 only after MVP definition-of-done passes on real hardware.

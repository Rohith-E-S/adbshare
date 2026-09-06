# Architecture

adbshare is split into six crates in a single Cargo workspace. Each crate
has a single responsibility; the dependency graph is intentionally
acyclic.

## Crate map

```
                   adb-gui  ────►  adb-daemon (over D-Bus)
                      │                 │
                      └──────┐  ┌──────┘
                             ▼  ▼
                          transfer-engine
                              │
                              ▼
                            adbfs
                          ╱      ╲
                         ▼        ▼
                     adb-proxy  adb-device
                                   │
                                   ▼
                              (libusb, rusb)
```

### `adb-device`
The transport and protocol layer. Knows how to talk to `adbd` over USB
(libusb) and TCP, generate RSA auth keys, and watch for device
plug/unplug events. No filesystem semantics.

### `adb-proxy`
Two parts: a host-side client (`ProxyClient`) and a device-side server
(`adbshare-proxy` binary). They speak a tiny length-prefixed binary RPC
over `adb forward`'d TCP connections. Used for fast random-access file
I/O and stat lookups — replaces the slow `adb shell` / `adb push` /
`adb pull` cycle.

### `adbfs`
A FUSE filesystem (`fuser` crate) that exposes a device as a POSIX mount
at `/run/user/$UID/adbshare/<serial>/`. Implements `lookup`, `getattr`,
`readdir`, `open`, `read`, `write`, `create`, `mkdir`, `unlink`, `rmdir`,
`rename`, `release`, `statfs`. Uses an ino→path map and TTL stat cache.

### `transfer-engine`
A queue with `DEFAULT_PARALLELISM = 4` workers for one-off transfers
(outside FUSE, for "download all" and similar). Each job streams via
`ProxyFile::read_at/write_at`. Tracks progress with a sliding window
for current speed, average, and ETA. Optional SHA-256 verification.

### `adb-daemon`
The background service. Discovers devices, pushes the proxy binary,
sets up `adb forward`, mounts the FUSE FS, and exposes a D-Bus interface
(`org.adbshare.Manager`) for the GUI.

### `adb-gui`
GTK4 + libadwaita. NavigationSplitView with devices on the left and
transfers on the right. Pairing wizard for first-run. Talks to the
daemon over D-Bus.

## Wire protocol summary

```
Client (host)                                    adbshare-proxy (device)
   │                                                       │
   │── TCP connect (via `adb forward`) ───────────────────►│
   │                                                       │
   │── [op u8][len u32 LE][args] ─────────────────────────►│
   │                                                       │
   │                                       (syscall + reply)
   │                                                       │
   │◄── [status u8][len u32 LE][data] ─────────────────────│
```

Op codes: OPEN, CLOSE, READ, WRITE, STAT, LSTAT, LISTDIR, MKDIR,
UNLINK, RMDIR, RENAME, TRUNCATE, REALPATH, READLINK, SYMLINK, UTIME,
DISKUSAGE.

## Concurrency model

- `adb-daemon` runs a tokio multi-thread runtime. Each mount lives in
  its own dedicated blocking thread (FUSE `BackgroundSession` is `!Send`).
- `adbfs::Adbfs` uses `tokio::Handle::block_on` to bridge sync FUSE
  callbacks into async proxy calls. Cache mitigates the per-call cost.
- The proxy client pools `DEFAULT_PROXY_CONNS = 4` TCP connections per
  device, protected by a `Semaphore`. Connections are reusable.
- The transfer engine spawns one task per in-flight job.

## Failure handling

- USB unplug → `Stream::AsyncRead` returns EOF, daemon transitions
  device to `Offline` in the state map, FUSE calls return `ENOTCONN`.
- `adb forward` drop → `ProxyClient::request` returns `Closed`,
  surfaced as `EIO` to the FUSE layer, then reconnection attempt after
  5s.
- Phone sleep → reads block until the device wakes (kernel-level USB
  suspend). No special handling needed; the user sees a hang and
  unplug-replug recovers.
- adbd restart → same as USB unplug. Auto-reconnect kicks in.

## What's NOT in v0.1

- Transfer resumption across crashes (state for the partial-file
  markers is sketched in `transfer-engine` but not wired in the daemon).
- Checksum verification on push (pull path is there).
- Wireless pairing QR code (wizard is text-only).
- Photo auto-import.
- MTP fallback for non-ADB devices.
- inotify on the device side (impossible; we poll).

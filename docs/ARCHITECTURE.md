# Architecture

adbshare is split into eight crates in a single Cargo workspace. Each crate
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
                      adb-proxy  adb-device ──► (libusb, rusb)
                          │
                          │  speaks the same wire protocol,
                          │  reimplemented rather than shared
                          ▼
              adbshare-proxy-device ──► (runs on the phone)

              test-adbfs  (manual harness, no dependents)
```

`adbshare-proxy-device` is the server half of the protocol `adb-proxy`
documents below. It cannot depend on that crate: it cross-compiles to
Android, so it carries `libc` and a trimmed tokio and nothing else, and the
op codes are declared twice. `test-adbfs` is a local harness that mounts
`adbfs` against a proxy on the same machine; nothing depends on it.

### `adb-device`
The transport and protocol layer. Knows how to talk to `adbd` over USB
(libusb) and TCP, generate RSA auth keys, and watch for device
plug/unplug events. No filesystem semantics.

### `adb-proxy`
The host-side client (`ProxyClient`), pooling TCP connections to
`127.0.0.1:<port>` (which `adb forward` maps to the phone). Speaks a tiny
length-prefixed binary RPC. Used for fast random-access file I/O and stat
lookups — replaces the slow `adb shell` / `adb push` / `adb pull` cycle.

Its server half is the separate `adbshare-proxy-device` crate, pushed to the
phone as `/data/local/tmp/adbshare-proxy`.

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
GPUI (the framework behind Zed), rendering with Vulkan. A top bar over a
resizable sidebar/content split: devices and place shortcuts on the left,
a file browser on the right, transfers in a popover. A two-step dialog
handles wireless pairing. Talks to the daemon over D-Bus.

The GUI owns no ADB logic. The file browser is a separate entity that
emits events; the app root turns those into D-Bus calls or local
filesystem work, and polls the daemon for the device list and the
transfer queue. Because zbus needs a tokio reactor and GPUI runs on its
own executor, a single-threaded tokio runtime is pinned to a dedicated
thread and every daemon call is dispatched onto it.

The old GTK build needed a 975-line stylesheet. GPUI has no CSS cascade
and no theme type of its own, so that stylesheet's palette and geometry
now live in `theme.rs` as tokens, its repeated components as small
builders in `ui.rs`, and its icons as an embedded SVG set in `icons.rs`.

The chrome icons are Google's Material Symbols in the Outlined style —
one path per glyph on a 24x24 grid — fetched by
`crates/adb-gui/tools/fetch_icons.py` under the GTK build's semantic
names, so call sites still read as intent. They are tinted per call site,
and `icons::hue` is what decides which colour each one wears. The
full-colour artwork (Yaru folders, Adwaita file types) is PNG, because
`gpui::img` cannot decode an SVG at all. The file chooser goes through
`ashpd` — the XDG desktop portal — rather than GTK4's `FileChooserNative`,
which under Wayland was itself a portal client.

Directories are the exception: they use the Yaru icon theme's full-colour
folders, the same artwork the desktop file manager draws, vendored as PNGs
under `assets/folders/`. They have to be raster. GPUI's `svg()` renders
through `usvg` and then keeps only the alpha channel, tinting the silhouette
with one colour — correct for line art, but it turns a filled purple folder
into a flat blob. `img()` keeps the pixels. `icons::icon_or_art` picks
between the two so no call site can hand a PNG to the SVG path and get a
blank square.

Two sets of third-party assets therefore travel with the binary, under
different licences. `assets/icons/` holds Material Symbols under Apache-2.0,
whose notice is compiled into the binary and shown by the About dialog, and
`assets/folders/` the Yaru notice for the CC-BY-SA-4.0 folder icons. Both are
aggregated alongside the code rather than merged into it.

## File previews

Previews are not a decode pipeline adbshare owns. GNOME Files already has one,
and it is not a pipeline either: `libgnome-desktop`'s `ThumbnailFactory` looks
up a `.thumbnailer` keyfile in `$XDG_DATA_DIRS/thumbnailers` whose `MimeType`
list covers the file and *runs the program it names* — `evince-thumbnailer` for
PDF, `totem-video-thumbnailer` for video — falling back to in-process
gdk-pixbuf decoding when nothing matches. adbshare does the same thing in
`thumbnails.rs`, so PDF and video previews are the pixels Nautilus draws, and
cost no dependency that was not already in the tree.

Results go in the freedesktop thumbnail repository,
`$XDG_CACHE_HOME/thumbnails/{normal,large}/<md5-of-URI>.png`, with the source
URI and mtime as PNG `tEXt` keys. That is not a private cache: a preview
adbshare generates is the one Nautilus reuses, and one Nautilus generated is
the one adbshare reuses. Freshness is the mtime comparison, so an edited file
regenerates. Failures are cached per-program under `fail/`, so a format with no
renderer installed is attempted once rather than on every repaint.

Three details are load-bearing and were each found by running it:

- **The cache is PNG whether or not the thumbnailer wrote one.** The video
  thumbnailer does not pass `-c png`, so ffmpeg returns a JPEG, which would be
  stored under a `.png` name with no `Thumb::` keys and then never validate.
  Every result is decoded and re-encoded, which is what libgnome-desktop does
  for the same reason.
- **The `Thumb::` keys have to be spliced in.** `image`'s PNG encoder will not
  write arbitrary chunks, so they are inserted ahead of the first `IDAT`; the
  chunk CRC is recomputed and nothing else moves.
- **Matching is by MIME type and the extension is not one.** ffmpeg's video
  thumbnailer claims `video/matroska`, GNOME's audio one lists six spellings of
  MP3, and the canonical names disagree between them. `mimes_for` therefore
  returns the known aliases per extension rather than one guess, because a
  single wrong guess matches nothing and the preview silently never appears.

Phone files have no local path of their own; a preview is read through the
device's FUSE mount, which the browser already tracks for "Open with". With FUSE
off there is no byte source and no D-Bus read, so phone previews are skipped and
local ones still work.

## File-type icons

A file with no preview still needs a type icon, and for a long time every one
that was not in the eight vendored artworks got the same blue page: a
spreadsheet, a Markdown file, a web page, a disc image and an executable all
looked alike, and none matched the file manager in the next window.

GNOME Files does not own those icons either. It asks the freedesktop icon theme,
and every desktop installs one — 981 full-colour `mimetypes/*.png` across Yaru,
Adwaita and hicolor on this machine alone. `sysicons.rs` reads them, which gives
complete coverage and exactly the icons the rest of the desktop draws, with
nothing vendored into the binary.

Three details each turned out to matter:

- **The theme name and its `Inherits` chain decide the answer.** This desktop runs
  `Yaru-purple`, which ships no artwork of its own and inherits `Yaru`. A lookup
  that enumerated theme directories picked up a badge out of an unrelated theme
  instead, which does not fail — it silently draws the wrong icon in the wrong
  colour. So the configured theme is read (GTK settings, then `gsettings`) and
  its inheritance walked breadth-first.
- **Every subdirectory at a size is searched, not `mimetypes/`.** Themes keep
  disc and flash artwork in `devices/`, folders in `places/`, and a hardcoded
  list misses half the theme.
- **Only PNG.** The `scalable/` copies are SVG, and GPUI's SVG path keeps just
  the alpha channel and tints the silhouette, which flattens a coloured
  spreadsheet logo into a green blob. The raster directories are correct.

Identity comes from the extension, then the executable bit, then — for a file
with neither — an eight-byte content sniff, which is what GIO does and what
catches an extensionless ELF binary that mode 644 would not. A test asserts
every icon name the table can return actually resolves in an installed theme,
because a misspelled name does not fail, it just falls through to the generic
page.

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
   │◄── [len u32 LE][status u8][data] ─────────────────────│
```

The two directions are not symmetric: a request leads with its one-byte op
code, a reply leads with the length, so the reader knows the frame size
before it knows whether the call succeeded. Both are little-endian.

Op codes: OPEN, CLOSE, READ, WRITE, STAT, LSTAT, LISTDIR, MKDIR, UNLINK,
RMDIR, RENAME, TRUNCATE, READLINK, SYMLINK, UTIME, DISKUSAGE, COPYFILE,
FSTAT. `0x0C` is unassigned; the numbering skips it.

## Concurrency model

- `adb-daemon` runs a tokio multi-thread runtime. Each mount lives in
  its own dedicated blocking thread (FUSE `BackgroundSession` is `!Send`).
- `adbfs::Adbfs` uses `tokio::Handle::block_on` to bridge sync FUSE
  callbacks into async proxy calls. Cache mitigates the per-call cost.
- The proxy client pools one connection per permit up to `--proxy-conns`
  (default 4) per device, protected by a `Semaphore`. Connections are
  reusable.
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

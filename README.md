# adbshare

A fast, reliable file manager for Linux that talks to Android devices over ADB.

> **Why?** MTP is slow, fragile, and locked into vendor quirks. KDE Connect is WiFi-only. We use ADB over USB (or wireless) and expose the device as a real filesystem via FUSE — with a polished GTK4 frontend on top.

## Highlights

- **Fast**: `adb forward` proxy protocol streams data without the per-chunk ADB framing overhead. Parallel transfers (4 concurrent per device).
- **Reliable**: auto-reconnect on USB unplug/replug, adbd restart, phone sleep. Transfer resumption for large files (configurable chunk size).
- **Visible**: per-file progress, current/avg speed, ETA, throughput history, checksum verification (opt-in).
- **Polished**: GTK4 + libadwaita, GNOME-style, but works on KDE too.
- **First-run friendly**: USB portal integration in Flatpak, no terminal required for the 90% case. Wireless pairing via QR.

## Architecture

```
+----------------+        D-Bus         +-----------------+
|   adb-gui      |  <----------------> |   adb-daemon    |
|  (GTK4 UI)     |                     |  (headless)     |
+----------------+                     +-----------------+
                                               |
                                      manages  | mounts + IPC
                                               v
                                     +-------------------+
                                     |     adbfs         |  (FUSE)
                                     | (per-device mount)|
                                     +-------------------+
                                               |
                                  proxy-over-  | adb forward
                                  adb forward  v
                                     +-------------------+
                                     |  adb-proxy server |
                                     | (pushed to device)|
                                     +-------------------+
```

- **`adb-device`**: ADB protocol (USB + wireless), device discovery, key management.
- **`adb-proxy`**: client+server for the fast streaming protocol over `adb forward`.
- **`adbfs`**: the FUSE filesystem using `fuser` (low-level).
- **`transfer-engine`**: queue, parallelism, resumption, checksums.
- **`adb-daemon`**: background service, mounts, IPC.
- **`adb-gui`**: the GTK4 frontend.

## Status

Pre-alpha. API is unstable. Don't ship it yet.

## Building

```sh
cargo build --release
```

GTK4 + libadwaita + fuse3 development headers required:

```sh
# Arch
sudo pacman -S gtk4 libadwaita fuse3 base-devel

# Debian/Ubuntu
sudo apt install libgtk-4-dev libadwaita-1-dev libfuse3-dev build-essential

# Fedora
sudo dnf install gtk4-devel libadwaita-devel fuse3-devel gcc
```

## License

GPL-3.0-or-later.

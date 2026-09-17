# adbshare

Browse your Android phone and move files between it and your Linux computer over USB or Wi-Fi using ADB. adbshare has a GTK4 + libadwaita interface, a transfer queue, and optional FUSE mounts for opening phone files in other desktop applications.

**Status: pre-alpha.** Keep backups of important files. Some operations are incomplete; read [Current limitations](#current-limitations) before moving or deleting data.

## What you can do

- Browse phone storage and local folders in grid or list view.
- Navigate with breadcrumbs or type a path; filter the current folder by name.
- Send files to your phone and save phone files to your computer.
- Watch transfer progress, speed, and ETA; pause, resume, or cancel queued work.
- Create folders, rename items, view properties, and delete items.
- Install APKs from the computer or the phone.
- Connect to an already paired phone over Wi-Fi.
- Open phone files in external applications through FUSE.

No companion Android app is needed. The daemon automatically deploys a small executable helper to the phone. Access is limited to what Android's ADB shell can read or write; this does not unlock protected app data.

## Before you start

You need:

- A Linux desktop with a graphical session and user/session D-Bus.
- Rust and Cargo. The workspace declares Rust 1.85 or newer; current stable is recommended.
- **GTK 4.18+**, **libadwaita 1.5+**, FUSE 3, a C build toolchain, and `pkg-config`.
- Android platform tools: the `adb` command must be on your `PATH`.
- An Android phone with USB debugging enabled and a data-capable USB cable.
- A helper binary built for your **phone's architecture**, as explained below.

### Install Linux dependencies

On Arch Linux:

```sh
sudo pacman -S --needed base-devel pkgconf gtk4 libadwaita fuse3 libusb android-tools
```

On Fedora (CI currently builds on Fedora 44):

```sh
sudo dnf install gcc pkgconf-pkg-config gtk4-devel libadwaita-devel fuse3-devel libusbx-devel android-tools
```

On Debian/Ubuntu, development packages are named `build-essential`, `pkg-config`, `libgtk-4-dev`, `libadwaita-1-dev`, `libfuse3-dev`, and `libusb-1.0-0-dev`; also install `adb` and `fuse3`. **Check native library versions first:** older distribution releases do not provide GTK 4.18.

These packages do not install the Rust toolchain. Check your environment before building:

```sh
cargo --version
adb version
pkg-config --modversion gtk4 libadwaita-1 fuse3
```

External file opening uses `xdg-open`; local Trash operations use `gio`. Terminal opening requires `gnome-terminal` or `x-terminal-emulator`.

### Prepare your phone

1. Enable **Developer options**, then **USB debugging** in Android settings. The location varies by manufacturer.
2. Connect the phone to the computer and unlock it.
3. Accept the **Allow USB debugging?** authorization prompt on the phone.
4. Check the connection:

   ```sh
   adb devices -l
   ```

Your phone should appear with the state `device`. If it says `unauthorized`, accept the authorization prompt. If no phone appears, check your cable and Linux USB permissions before proceeding.

## Build and run from source

Run these commands from the repository root. Source builds are the recommended starting point while packaging is experimental.

### 1. Build the desktop programs

```sh
git clone https://github.com/Rohith-E-S/adbshare.git
cd adbshare
cargo build --release --workspace --locked
```

This produces `target/release/adb-daemon` and `target/release/adb-gui`. It also builds a host-side proxy executable, which **is not suitable for an ARM phone when built on an x86-64 computer**.

### 2. Build the phone helper

Check your phone's ABI, replacing `SERIAL` with its identifier from `adb devices -l`:

```sh
adb -s SERIAL shell getprop ro.product.cpu.abi
```

For an **ARM64 phone** (`arm64-v8a`), the repository provides a static musl build configuration:

```sh
rustup target add aarch64-unknown-linux-musl
cargo build --release --locked -p adbshare-proxy-device --bin adbshare-proxy --target aarch64-unknown-linux-musl
export ADBSHARE_PROXY_BIN="$PWD/target/aarch64-unknown-linux-musl/release/adbshare-proxy"
```

Run the build from this repository so Cargo picks up `.cargo/config.toml`. Compatibility with every Android version/device is not guaranteed. Do not use the ARM64 executable on a 32-bit ARM or x86 phone.

<details>
<summary>Alternative: build an ARM64 helper with the Android NDK</summary>

The release workflow uses Android NDK r26d and an API 24 ARM64 compiler. With that NDK already installed on an x86-64 Linux host:

```sh
rustup target add aarch64-linux-android
export ANDROID_NDK=/absolute/path/to/android-ndk-r26d
export PATH="$ANDROID_NDK/toolchains/llvm/prebuilt/linux-x86_64/bin:$PATH"
export CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER=aarch64-linux-android24-clang
export CC_aarch64_linux_android=aarch64-linux-android24-clang
export CXX_aarch64_linux_android=aarch64-linux-android24-clang++
export AR_aarch64_linux_android=llvm-ar
cargo build --release --locked -p adbshare-proxy-device --bin adbshare-proxy --target aarch64-linux-android
export ADBSHARE_PROXY_BIN="$PWD/target/aarch64-linux-android/release/adbshare-proxy"
```

</details>

`ADBSHARE_PROXY_BIN` must be available in the **daemon's environment**. Set it again in a new terminal if needed. The daemon can also discover the ARM64 build outputs automatically when running from this source tree.

### 3. Launch adbshare

From the same terminal:

```sh
./run.sh
```

This rebuilds the release workspace, stops existing adbshare instances, attempts to unmount stale adbshare FUSE mounts, and starts the daemon and GUI. **Let transfers finish before rerunning it.** It does not cross-build the phone helper or install a system service.

Logs are written to:

- `/tmp/adb-daemon.log`
- `/tmp/adb-gui.log`

For foreground diagnostics, run the programs in separate terminals instead:

```sh
./target/release/adb-daemon
```

```sh
./target/release/adb-gui
```

Run both as your normal desktop user, not with `sudo`. The GUI communicates with the daemon over the session bus.

## Your first transfer

1. Select your phone in the sidebar. Devices appear after the daemon has connected and deployed the helper successfully.
2. Open **Download** or another phone folder. The initial phone location is `/sdcard/Download`.
3. Choose **Send to phone** (`Ctrl+U`) and select a local file. You can also drop local files onto the phone folder.
4. Open **Transfers** in the top bar to watch progress.
5. To copy a file back, select it on the phone and use **Save to computer** (`Ctrl+Shift+C`). One file opens a Save As dialog; multiple files open a destination-folder chooser.
6. Press `F5` if the folder listing has not updated after the transfer finishes.

Queued phone transfers currently **skip existing destination files**. Folder trees cannot be uploaded or downloaded through this queue yet. Use distinct destination names when testing.

### Finding your way around

- **Sidebar:** switch between connected phones, phone storage shortcuts, Home, Downloads, and local Trash.
- **Top bar:** back/forward/up navigation, current path, new folder, send/save controls, view switch, search, transfers, and more options.
- **Path bar:** click breadcrumbs or press `Ctrl+L` to enter a path.
- **Search:** filters names in the current folder; it is not a recursive device-wide search.
- **More options:** includes hidden files, Wi-Fi connection, and APK installation.

### Keyboard shortcuts

| Shortcut | Action |
| --- | --- |
| `Alt+Left` / `Alt+Right` | Back / forward |
| `Alt+Up` | Parent folder |
| `Ctrl+L` | Edit the path |
| `Ctrl+F` | Search the current folder |
| `F5` | Refresh |
| `F9` | Toggle the sidebar |
| `Ctrl+U` | Send a file to the phone |
| `Ctrl+Shift+C` | Save selected phone files to the computer |
| `Ctrl+C` / `Ctrl+V` | Copy / paste files; see the same-phone limitation below |
| `F2` | Rename |
| `Alt+Return` | Properties |
| `Shift+Delete` | Permanent-delete action |

### Connect over Wi-Fi

For Android's Wireless debugging feature, pair using the ADB CLI first:

```sh
adb pair PHONE_IP:PAIRING_PORT
```

Enter the pairing code shown on the phone. Then choose **Connect via Wi-Fi…** in adbshare and enter the phone's **connection address and port**. The connection port can differ from the pairing port. The computer and phone need network connectivity to each other.

adbshare currently offers connection, not a QR-code or pairing wizard. USB remains the simplest first setup.

### Install an APK

Use **Install APK…** in the more-options menu, or the APK context action. Dropping an APK onto a phone folder offers an install-versus-copy choice. Installation uses Android's package manager and may replace an existing installation; only install APKs you trust.

## Current limitations

- **Phone deletion is permanent.** Only local files offer “Move to Trash”; there is no phone trash/recovery feature.
- Same-phone paste copies regular files without replacing existing destinations. It requires the updated phone helper and does not support copying symlinks or directories. If a copy loses its connection or times out, completion is unknown: the destination may be incomplete or still copying. Cross-phone paste is unsupported, and queued copy operations handle files rather than directory trees.
- The transfer queue is in memory and is lost when the daemon exits. Reconnecting can retry work, but reliable byte-offset resumption is not guaranteed.
- The GUI does not expose overwrite policies, checksum-verification settings, or automatic folder synchronization.
- FUSE enables external file opening and dragging phone files out. Without it, use in-app browsing and queued transfers instead.
- Custom/remote ADB server support is incomplete: discovery and subprocess ADB commands do not consistently use the same server options.
- AUR, Flatpak, and `.deb` packaging are experimental. Package layouts and dependencies are not yet consistently wired up; a phone-compatible proxy may need to be supplied separately. There is no working Flatpak USB-portal setup.

## Troubleshooting

| Problem | What to check |
| --- | --- |
| Phone does not appear | Run `adb devices -l`, unlock/authorize the phone, and check USB permissions. Then inspect the daemon log: a device appears only after helper setup succeeds. |
| Helper missing or fails to start | Check the phone ABI and `ADBSHARE_PROXY_BIN`. An x86-64 host executable will not run on an ARM64 phone. |
| GTK build fails | Check `pkg-config --modversion gtk4 libadwaita-1 fuse3`; installing headers alone does not ensure a new enough GTK version. |
| Transfer says skipped | The destination already exists. Save under another name or handle the existing file yourself. |
| File not visible after upload | Wait for completion and press `F5`. |
| External opening or drag-out fails | Check FUSE, `fusermount3`, `/dev/fuse`, and `xdg-open`. In-app browsing may work even when mounting fails. |
| GUI cannot reach the daemon | Start `adb-daemon` in a terminal in the same desktop session and inspect its output. |

To isolate FUSE problems, start the daemon without mounts (stop an already running instance first):

```sh
./target/release/adb-daemon --no-fuse
```

For more logging:

```sh
RUST_LOG=debug ./target/release/adb-daemon
```

The phone helper's log is available through ADB:

```sh
adb -s SERIAL shell cat /data/local/tmp/adbshare-proxy.log
```

If you have installed the repository's systemd **user** service, inspect it with:

```sh
journalctl --user -u adbshare-daemon.service -b
```

For a bug report, include your Linux distribution, GTK version, phone model/Android version, reproduction steps, and relevant errors. Review logs for private filenames, device identifiers, and other sensitive information before sharing.

## For contributors

The Rust workspace separates the GTK GUI (`adb-gui`), session D-Bus daemon (`adb-daemon`), ADB discovery/transport (`adb-device`), proxy client (`adb-proxy`), phone helper (`adbshare-proxy-device`), FUSE filesystem (`adbfs`), and transfer queue (`transfer-engine`).

See [development notes](docs/DEVELOPMENT.md) and [architecture notes](docs/ARCHITECTURE.md) for background. Some details there are historical; current source and CI configuration are authoritative.

## License

GPL-3.0-or-later. See [LICENSE](LICENSE).

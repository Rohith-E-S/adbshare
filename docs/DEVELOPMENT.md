# Development setup

## Prerequisites

### Linux host

```sh
# Arch
sudo pacman -S vulkan-icd-loader libxkbcommon wayland libx11 libxext fontconfig fuse3 libusb base-devel

# Debian/Ubuntu
sudo apt install libvulkan-dev libxkbcommon-dev libwayland-dev libx11-dev libxext-dev libfontconfig1-dev libfuse3-dev libusb-1.0-0-dev build-essential

# Fedora
sudo dnf install vulkan-loader-devel libxkbcommon-devel libxkbcommon-x11-devel wayland-devel libX11-devel libXext-devel fontconfig-devel freetype-devel fuse3-devel libusb1-devel gcc
```

`libxkbcommon-x11-devel` is not optional on Fedora: `gpui` enables
xkbcommon's `x11` feature, and the link fails with
`unable to find library -lxkbcommon-x11` without it. The Arch and Debian
packages ship that library in the main `libxkbcommon` package.

### Rust

Rust 1.88 or newer (`rust-version` in the root `Cargo.toml`). The backend
and GUI use `if let … &&` chains, which only became stable in 1.88.

```sh
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

### XDG desktop portal

The GUI's file chooser goes through `ashpd`, i.e.
`org.freedesktop.portal.FileChooser` on the session bus. Anything using the
pickers — send to phone, save to computer, install APK — needs a portal
backend in the session: `xdg-desktop-portal-gnome` on GNOME,
`xdg-desktop-portal-kde` on KDE, or the portal-gtk fallback elsewhere.
Without one the chooser call fails and the rest of the app still works.

### Android NDK (for the device-side proxy)

Required only for `make adb-proxy-device`. Install via Android Studio's SDK
Manager or directly. Use **r26d**, the version `release.yml` builds with:

```sh
wget https://dl.google.com/android/repository/android-ndk-r26d-linux.zip
unzip android-ndk-r26d-linux.zip -d $HOME/Android/Sdk/ndk/
export ANDROID_NDK=$HOME/Android/Sdk/ndk/android-ndk-r26d
```

## Build

```sh
# Host binaries (daemon + GUI), same invocation as CI and `make build`
cargo build --release --workspace --locked

# Device-side proxy (Android ELF, ~500KB)
rustup target add aarch64-linux-android
make adb-proxy-device
```

`--workspace` matters: `adb-gui`, `adb-daemon` and the host `adbshare-proxy`
are separate members, and `cargo build --release` without it only builds the
current directory's default members.

## Run

The GUI renders with Vulkan, so it needs a GPU and driver that `vulkaninfo`
lists, on a Wayland or X11 session. GPUI selects Vulkan unconditionally —
there is no OpenGL or Cairo fallback, unlike the old GTK build's
`GSK_RENDERER=cairo`.

Without a hardware GPU, install Mesa's **lavapipe** software Vulkan driver
and point the loader at it if the hardware ICDs get in the way:

```sh
sudo dnf install mesa-vulkan-drivers            # Fedora
sudo apt install mesa-vulkan-drivers            # Debian/Ubuntu

vulkaninfo --summary                            # expect an llvmpipe device
export VK_ICD_FILENAMES=/usr/share/vulkan/icd.d/lvp_icd.x86_64.json
```

Arch splits its Vulkan drivers into per-driver `vulkan-*` packages and has
no single `mesa-vulkan-drivers`, so install the one matching your GPU
(`vulkan-radeon`, `vulkan-nouveau`, `vulkan-intel`, …) there.

lavapipe advertises itself as `llvmpipe` with
`deviceType = PHYSICAL_DEVICE_TYPE_CPU`, which `blade-graphics` records as
`is_software_emulated` rather than rejecting, so startup works; it is slow,
but it is the documented path for llvmpipe-only machines.

```sh
# 1. Start the daemon (auto-discovers devices, mounts them under
#    $XDG_RUNTIME_DIR/adbshare/<serial>/)
./target/release/adb-daemon

# 2. Launch the GUI
./target/release/adb-gui
```

`./run.sh` does both and writes logs to `/tmp/adb-daemon.log` and
`/tmp/adb-gui.log`.

## Verifying

There is no test suite. Verification is the smoke test below, plus
`cargo clippy` and `cargo fmt`, all of which CI runs.

Smoke-testing the *binary* is a different matter. The end-to-end path —
`Application::new()`, Vulkan device selection, asset resolution, window
creation — needs both a display and a Vulkan ICD, so CI runs it under Xvfb
with lavapipe:

```sh
sudo dnf install xorg-x11-server-Xvfb mesa-vulkan-drivers vulkan-tools
export VK_ICD_FILENAMES=/usr/share/vulkan/icd.d/lvp_icd.x86_64.json
ADB_GUI_AUTOCLOSE_MS=1500 xvfb-run -a ./target/release/adb-gui
```

`ADB_GUI_AUTOCLOSE_MS=<ms>` quits the app after N milliseconds. Note that
`adb-gui --version` exits inside clap's `Cli::parse()`, before
`Application::new()` runs, so it only proves the binary links and its
argument parser works — it does not touch Vulkan, assets, or the window.

## Architecture

See `README.md` for the high-level layout. The interesting design
decisions are documented inline in each crate's `lib.rs`.

## Code style

```sh
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
```

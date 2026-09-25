# Development setup

## Prerequisites

### Linux host

```sh
# Arch
sudo pacman -S vulkan-icd-loader libxkbcommon wayland libx11 libxext fontconfig fuse3 libusb base-devel

# Debian/Ubuntu
sudo apt install libvulkan-dev libxkbcommon-dev libwayland-dev libx11-dev libxext-dev libfontconfig1-dev libfuse3-dev libusb-1.0-0-dev build-essential

# Fedora
sudo dnf install vulkan-loader-devel libxkbcommon-devel wayland-devel libX11-devel libXext-devel fontconfig-devel freetype-devel fuse3-devel libusb1-devel gcc
```

### Rust

```sh
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

### Android NDK (for the device-side proxy)

Required only for `make adb-proxy-device`. Install via Android Studio's SDK
Manager or directly:

```sh
wget https://dl.google.com/android/repository/android-ndk-r26b-linux.zip
unzip android-ndk-r26b-linux.zip -d $HOME/Android/Sdk/ndk/
export ANDROID_NDK=$HOME/Android/Sdk/ndk/android-ndk-r26b
```

## Build

```sh
# Host binaries (daemon + GUI)
cargo build --release

# Device-side proxy (Android ELF, ~500KB)
rustup target add aarch64-linux-android
make adb-proxy-device
```

## Run

The GUI renders with Vulkan, so it needs a GPU and driver that `vulkaninfo`
lists, on a Wayland or X11 session.

```sh
# 1. Start the daemon (auto-discovers devices, mounts them under
#    $XDG_RUNTIME_DIR/adbshare/<serial>/)
./target/release/adb-daemon

# 2. Launch the GUI
./target/release/adb-gui
```

`./run.sh` does both and writes logs to `/tmp/adb-daemon.log` and
`/tmp/adb-gui.log`.

## Test

```sh
# Some daemon tests serve a mock interface on the session bus.
dbus-run-session -- cargo test --workspace
```

The GUI's tests need neither a display server nor a GPU: they drive
`gpui`'s `TestAppContext`, which supplies a fake platform window and
lays out in software. There is no Xvfb step any more.

`ADB_GUI_AUTOCLOSE_MS=<ms> ./target/release/adb-gui` quits after N
milliseconds, which is a cheap smoke test that the real binary gets
through startup.

## Architecture

See `README.md` for the high-level layout. The interesting design
decisions are documented inline in each crate's `lib.rs`.

## Code style

```sh
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
```

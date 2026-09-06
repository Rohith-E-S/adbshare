# Development setup

## Prerequisites

### Linux host

```sh
# Arch
sudo pacman -S gtk4 libadwaita fuse3 libusb base-devel

# Debian/Ubuntu
sudo apt install libgtk-4-dev libadwaita-1-dev libfuse3-dev libusb-1.0-0-dev build-essential

# Fedora
sudo dnf install gtk4-devel libadwaita-devel fuse3-devel libusb1-devel gcc
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

```sh
# 1. Start the daemon (auto-discovers devices, mounts them under
#    $XDG_RUNTIME_DIR/adbshare/<serial>/)
./target/release/adb-daemon

# 2. Launch the GUI
./target/release/adb-gui
```

## Test

```sh
cargo test --workspace
```

## Architecture

See `README.md` for the high-level layout. The interesting design
decisions are documented inline in each crate's `lib.rs`.

## Code style

```sh
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
```

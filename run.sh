#!/usr/bin/env bash
# Build adbshare in release mode and (re)start the daemon and GUI.
set -euo pipefail

cd "$(dirname "$0")"

# Resolve cargo: rustup install first, then whatever is on PATH
if [ -x "$HOME/.cargo/bin/cargo" ]; then
    CARGO="$HOME/.cargo/bin/cargo"
elif command -v cargo > /dev/null 2>&1; then
    CARGO="$(command -v cargo)"
else
    echo "ERROR: cargo not found (looked in \$HOME/.cargo/bin and PATH)" >&2
    exit 1
fi
echo "==> Using cargo: $CARGO ($("$CARGO" --version))"

echo "==> Building (cargo build --release --workspace)"
"$CARGO" build --release --workspace

echo "==> Stopping existing instances"
pkill -x adb-daemon 2>/dev/null || true
pkill -x adb-gui 2>/dev/null || true
sleep 1
# Unmount any stale FUSE mounts left behind
for m in /run/user/"$(id -u)"/adbshare/*; do
    [ -d "$m" ] && fusermount3 -u "$m" 2>/dev/null || true
done

echo "==> Starting adb-daemon"
nohup ./target/release/adb-daemon > /tmp/adb-daemon.log 2>&1 &
echo "    daemon pid: $!"
sleep 3

echo "==> Starting adb-gui"
nohup ./target/release/adb-gui > /tmp/adb-gui.log 2>&1 &
echo "    gui pid: $!"
sleep 2

if pgrep -x adb-daemon > /dev/null && pgrep -x adb-gui > /dev/null; then
    echo "==> Done. Logs: /tmp/adb-daemon.log, /tmp/adb-gui.log"
else
    echo "==> ERROR: a process failed to start; check /tmp/adb-daemon.log and /tmp/adb-gui.log" >&2
    exit 1
fi

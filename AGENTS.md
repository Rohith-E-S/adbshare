# AGENTS.md

## Architecture

Rust workspace (edition 2024, MSRV 1.88) — Android file manager for Linux, pre-alpha:

```
adb-gui ──D-Bus──► adb-daemon ──► transfer-engine ──► adbfs (FUSE)
                                                  ╱           ╲
                                           adb-proxy      adb-device (USB/TCP, rusb)
                                               │  wire protocol, reimplemented
                                               ▼
                                   adbshare-proxy-device (runs on phone)
test-adbfs: manual FUSE harness, no dependents, publish=false
```

- `adb-gui`: GPUI (Zed's framework, Vulkan-only — no OpenGL fallback). Owns no ADB logic; all device work is D-Bus calls to `org.adbshare.Manager` (`/org/adbshare/Manager`). zbus needs tokio but GPUI has its own executor, so a single-threaded tokio runtime is pinned to a dedicated thread in `adb-gui/src/state/daemon.rs` — don't call zbus directly from GPUI tasks.
- `adb-daemon`: tokio multi-thread; discovers devices, deploys helper, sets up `adb forward`, mounts FUSE at `$XDG_RUNTIME_DIR/adbshare/<serial>/`, exposes D-Bus. Each mount's FUSE session (`!Send`) lives on its own blocking thread.
- Wire protocol (`adb-proxy` ↔ `adbshare-proxy-device`): `[op u8][len u32 LE][args]` → `[len u32 LE][status u8][data]`. Op codes are declared twice, once per crate — the device crate must stay minimal (`libc` + trimmed tokio) to cross-compile to Android, so it cannot depend on `adb-proxy`. Change an opcode in both. `0x0C` is unassigned.

## Commands

```sh
cargo build --release --workspace --locked   # host binaries; bare `cargo build` from root also works (all 8 are default members)
make build / make test / make lint / make fmt # wrappers: test = cargo test --workspace, lint = clippy --workspace --all-targets -- -D warnings
cargo test -p <crate> <test_name>            # focused test, e.g. -p adb-daemon
cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings   # before pushing; CI fails otherwise
./run.sh                                     # rebuilds release, kills existing instances, unmounts stale FUSE, starts daemon+GUI; logs /tmp/adb-daemon.log, /tmp/adb-gui.log
```

- Run manually: `./target/release/adb-daemon` then `./target/release/adb-gui` (same session bus, as normal user — never `sudo`). `--no-fuse` on the daemon skips mounts (phone previews then stop working; local ones don't).
- Toolchain is pinned to exact `1.98.1` in `rust-toolchain.toml` for fmt/clippy parity with CI — don't bump casually. It overrides any installed default, so MSRV checks need the explicit form CI uses: `cargo +1.88.0 check --workspace --locked`.

## Phone helper (the usual trap)

- The host-built `adbshare-proxy` will **not** run on an ARM phone. For an ARM64 phone, cross-compile and point the daemon at it:
  ```sh
  rustup target add aarch64-unknown-linux-musl
  cargo build --release --locked -p adbshare-proxy-device --bin adbshare-proxy --target aarch64-unknown-linux-musl
  export ADBSHARE_PROXY_BIN="$PWD/target/aarch64-unknown-linux-musl/release/adbshare-proxy"  # must be in the daemon's env
  ```
  `make adb-proxy-device ANDROID_NDK=~/Android/Sdk/ndk/android-ndk-r26d` is the NDK (`aarch64-linux-android`) alternative release CI uses; run it from the repo root so `.cargo/config.toml` applies. The daemon also auto-discovers ARM64 outputs when run from the source tree.
- `adb -s SERIAL shell getprop ro.product.cpu.abi` tells you the phone's ABI — don't use the ARM64 binary on 32-bit ARM/x86 phones.

## Environment quirks

- **Vulkan is mandatory** for the GUI. No GPU? Mesa lavapipe works: `vulkaninfo --summary` should list `llvmpipe`; if hardware ICDs interfere, `VK_ICD_FILENAMES=/usr/share/vulkan/icd.d/lvp_icd.x86_64.json`. Arch has per-driver `vulkan-*` packages, not a single mesa one.
- **Fedora only**: `libxkbcommon-x11-devel` is a separate package and the link fails without it (`unable to find library -lxkbcommon-x11`).
- File pickers need an XDG desktop portal backend (`xdg-desktop-portal-gnome`/`-kde`); without one, browsing/transfers work but chooser calls fail.
- Previews come from system thumbnailers (freedesktop shared cache `~/.cache/thumbnails`), not a bundled decoder — PDF/video/office need poppler/ffmpeg/libreoffice present. Phone previews additionally need FUSE.

## Testing

- There is no broad suite. `cargo test --workspace` is mostly pure helpers in `adb-daemon` (tree-op caps, path checks — both had real off-by-ones). Don't expect GUI/integration coverage.
- Two tests are environment-sensitive and fail on a bare machine, not because of your change: `adb-gui`'s sysicons test needs an installed icon theme (`yaru-icon-theme` + `adwaita-icon-theme-legacy` in CI), and the thumbnail test needs the system's thumbnailers present.
- GUI startup smoke test needs display + Vulkan: `VK_ICD_FILENAMES=.../lvp_icd.x86_64.json ADB_GUI_AUTOCLOSE_MS=1500 xvfb-run -a ./target/release/adb-gui` (as in `ci.yml`). `--version` proves nothing — it exits in clap before `Application::new()`.
- Never run `adbshare-proxy` with `--help`/no args in scripts: it takes a port as `$1` and binds, hanging the job.

## Assets & data

- Icons: Material Symbols (Apache-2.0) under `crates/adb-gui/assets/icons/` (compiled in via `asset_table!` in `icons.rs`; NOTICEs ship in packages). Folders are vendored Yaru PNGs (CC-BY-SA-4.0) — GPUI's `svg()` keeps only the alpha channel and tints it, so any filled artwork must be PNG via `img()`, never SVG.
- Debloat list is bundled, gzipped (`crates/adb-gui/assets/debloat/uad_lists.json.gz`) — the app never fetches it at runtime by design. Refresh with `make debloat-list`; keep the GPL `NOTICE` in place.
- User state lives in `~/.config/adbshare/` (`gui.json`, `install_history.json`, `debloat_ledger.json`) — corrupt/unparseable `gui.json` falls back to defaults by design, don't "fix" that by failing hard.

## CI / release

- CI (`.github/workflows/ci.yml`): Fedora 44 container; jobs are release build + Xvfb smoke, clippy+test, fmt `--check`, MSRV `cargo +1.88.0 check`, and `cargo audit`. Keep `--locked` on every cargo invocation — the lockfile is what's audited.
- `cargo audit` is blocking: `.cargo/audit.toml` denies `unsound`, and its two ignored advisories (rsa, crypto-bigint) are triaged there with reasoning. Re-run `cargo audit` after any dependency change; don't add ignores without recording why, and don't "fix" the rsa one by upgrading — ADB auth requires RSA-2048 PKCS#1 v1.5.
- Every push to `main` auto-publishes a GitHub Release (`release.yml`, version `v<workspace-version>-r<RUN>-g<sha>` from root `Cargo.toml`). Doc-only changes are skipped via `paths-ignore`.
- Packaging (`packaging/`: systemd user unit, D-Bus service, desktop file, flatpak, AUR) is experimental — see README "Current limitations" before assuming it's wired up.

## Docs

- `README.md` (usage/troubleshooting), `docs/DEVELOPMENT.md` (setup, smoke test), `docs/ARCHITECTURE.md` (crate map, thumbnail/icon design). Where docs conflict with source or CI config, source/CI wins — some doc details are historical.

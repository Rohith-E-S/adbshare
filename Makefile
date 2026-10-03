PROJECT := adbshare
CARGO := cargo
TARGET_PROXY := aarch64-linux-android
ANDROID_NDK ?= $(HOME)/Android/Sdk/ndk/$(NDK_VERSION)
PROXY_BINDIR := target/$(TARGET_PROXY)/release

# Cross-compile the device-side proxy binary. Requires the Android NDK
# standalone toolchain at $ANDROID_NDK or a Rust target installed via
# `rustup target add aarch64-linux-android`.
.PHONY: adb-proxy-device
adb-proxy-device:
	@if [ ! -d "$(ANDROID_NDK)" ]; then \
		echo "Set ANDROID_NDK to your NDK install path (e.g. ~/Android/Sdk/ndk/26.3.11579264)"; \
		exit 1; \
	fi
	@rustup target list --installed | grep -q $(TARGET_PROXY) || rustup target add $(TARGET_PROXY)
	CC_aarch64_linux_android=$(ANDROID_NDK)/toolchains/llvm/prebuilt/linux-x86_64/bin/aarch64-linux-android24-clang \
	CXX_aarch64_linux_android=$(ANDROID_NDK)/toolchains/llvm/prebuilt/linux-x86_64/bin/aarch64-linux-android24-clang++ \
	AR_aarch64_linux_android=$(ANDROID_NDK)/toolchains/llvm/prebuilt/linux-x86_64/bin/llvm-ar \
	$(CARGO) build --release --target $(TARGET_PROXY) --bin adbshare-proxy
	@mkdir -p $(PROXY_BINDIR)
	@cp target/$(TARGET_PROXY)/release/adbshare-proxy $(PROXY_BINDIR)/adbshare-proxy

.PHONY: build
build:
	$(CARGO) build --release --workspace

.PHONY: test
# Under a session bus, so the adb-daemon test that serves a mock
# org.adbshare.Manager interface over the session D-Bus works when it is
# enabled with --ignored. The default run passes with or without the wrapper;
# the wrapper is what makes `make test --ignored` usable.
test:
	dbus-run-session -- $(CARGO) test --workspace

.PHONY: clean
clean:
	$(CARGO) clean

.PHONY: fmt
fmt:
	$(CARGO) fmt --all

.PHONY: lint
lint:
	$(CARGO) clippy --workspace --all-targets -- -D warnings

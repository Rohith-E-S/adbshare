CARGO := cargo
TARGET_PROXY := aarch64-linux-android

# Cross-compile the device-side proxy binary. Requires the Android NDK
# standalone toolchain, because the aarch64-linux-android target needs its
# clang as the linker — `rustup target add` alone is not enough. Set
# ANDROID_NDK to the install directory:
#
#   make adb-proxy-device ANDROID_NDK=~/Android/Sdk/ndk/android-ndk-r26d
#
# See docs/DEVELOPMENT.md for how to install it.
.PHONY: adb-proxy-device
adb-proxy-device:
	@if [ -z "$(ANDROID_NDK)" ] || [ ! -d "$(ANDROID_NDK)" ]; then \
		echo "ANDROID_NDK is unset or not a directory."; \
		echo "Pass it explicitly: make adb-proxy-device ANDROID_NDK=~/Android/Sdk/ndk/android-ndk-r26d"; \
		exit 1; \
	fi
	@rustup target list --installed | grep -q $(TARGET_PROXY) || rustup target add $(TARGET_PROXY)
	CC_aarch64_linux_android=$(ANDROID_NDK)/toolchains/llvm/prebuilt/linux-x86_64/bin/aarch64-linux-android24-clang \
	CXX_aarch64_linux_android=$(ANDROID_NDK)/toolchains/llvm/prebuilt/linux-x86_64/bin/aarch64-linux-android24-clang++ \
	AR_aarch64_linux_android=$(ANDROID_NDK)/toolchains/llvm/prebuilt/linux-x86_64/bin/llvm-ar \
	$(CARGO) build --release --target $(TARGET_PROXY) --bin adbshare-proxy

.PHONY: build
build:
	$(CARGO) build --release --workspace

.PHONY: clean
clean:
	$(CARGO) clean

.PHONY: fmt
fmt:
	$(CARGO) fmt --all

.PHONY: lint
lint:
	$(CARGO) clippy --workspace --all-targets -- -D warnings

.PHONY: test
test:
	$(CARGO) test --workspace

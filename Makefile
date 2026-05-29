# Top-level convenience targets.
#
# Two flavours of work:
#   * Local build      — `make build` (Rust CLI/host binary) and
#                         `make android` (Android APK, which itself
#                         cross-compiles the JNI shim via cargo-ndk).
#   * Sync to remote   — `make sync REMOTE=user@host:/path` rsyncs the
#                         repo while excluding generated artefacts the
#                         remote will regenerate on its own build.

REMOTE  ?=
SSH_KEY ?=
# rsync's ssh command — overridable so callers can pass `SSH_KEY=…`
# without remembering rsync's `-e` syntax. `StrictHostKeyChecking=accept-new`
# trusts a never-seen-before host but rejects key changes (TOFU); drop the
# flag if your fleet rotates keys often. -o IdentitiesOnly=yes prevents the
# ssh agent from trying other keys first when SSH_KEY is set.
SSH_CMD = ssh $(if $(SSH_KEY),-i $(SSH_KEY) -o IdentitiesOnly=yes) \
          -o StrictHostKeyChecking=accept-new

# `-r` recursive, `-l` preserve symlinks, `-t` mtimes (lets cargo's
# fingerprinting skip rebuilds when source didn't actually change),
# `-z` compress in transit. `--delete` removes files on the remote
# that aren't local — keeps the remote tree from accumulating cruft.
# `-v` for one line per file — `--info=progress2` would be nicer but
# macOS ships rsync 2.6.9 which doesn't know that flag. `-e "$(SSH_CMD)"`
# threads the optional SSH key through.
RSYNC_FLAGS = -rltzv --delete -e "$(SSH_CMD)" \
    --exclude='.git/' \
    --exclude='target/' \
    --exclude='**/target/' \
    --exclude='node_modules/' \
    --exclude='**/node_modules/' \
    --exclude='bale-vpn-android/**/build/' \
    --exclude='bale-vpn-android/rust/jniLibs/' \
    --exclude='bale-vpn-android/.gradle/' \
    --exclude='bale-vpn-android/.idea/' \
    --exclude='bale-vpn-android/local.properties' \
    --exclude='**/.cxx/' \
    --exclude='*.iml' \
    --exclude='*.jks' \
    --exclude='*.keystore' \
    --exclude='.DS_Store' \
    --exclude='*.swp' \
    --exclude='*.swo'

GRADLEW = ./bale-vpn-android/gradlew -p bale-vpn-android

.PHONY: help \
        build build-headless \
        android android-debug install-android install-android-debug \
        clean clean-android clean-rust \
        sync sync-dry

help:
	@echo "Build targets (run from anywhere):"
	@echo "  make build                   — build the Rust CLI/host binary (release)"
	@echo "  make build-headless          — same, --no-default-features (no tao/wry)"
	@echo "  make android                 — build the release APK (assembleRelease)"
	@echo "  make android-debug           — build the debug APK (assembleDebug)"
	@echo "  make install-android         — adb-install the release APK"
	@echo "  make install-android-debug   — adb-install the debug APK"
	@echo ""
	@echo "Clean targets:"
	@echo "  make clean                   — cargo clean + gradle clean + jniLibs"
	@echo "  make clean-rust              — cargo clean across every Rust workspace"
	@echo "  make clean-android           — gradle clean + drop bale-vpn-android/rust/jniLibs"
	@echo ""
	@echo "Sync targets:"
	@echo "  make sync REMOTE=user@host:/path [SSH_KEY=~/.ssh/id_ed25519]"
	@echo "      rsync to remote (deletes extras on the far side)"
	@echo "  make sync-dry REMOTE=user@host:/path [SSH_KEY=…]"
	@echo "      dry-run, show what would change"
	@echo ""
	@echo "Generated artefacts (target/, build/, jniLibs/) are excluded from"
	@echo "sync — the remote regenerates them via cargo build + gradle build."

# ── Rust host/CLI binary (bale-vpn-rust) ──────────────────────────

build:
	cd bale-vpn-rust && cargo build --release

build-headless:
	cd bale-vpn-rust && cargo build --release --no-default-features

# ── Android (Gradle task chain auto-runs cargo-ndk for the JNI .so) ──

android:
	$(GRADLEW) :androidApp:assembleRelease

android-debug:
	$(GRADLEW) :androidApp:assembleDebug

install-android:
	$(GRADLEW) :androidApp:installRelease

install-android-debug:
	$(GRADLEW) :androidApp:installDebug

# ── Clean ──────────────────────────────────────────────────────────

clean: clean-rust clean-android

clean-rust:
	cd lk-signaling-rust    && cargo clean
	cd bale-signaling-rust  && cargo clean
	cd lktunnel-rust        && cargo clean
	cd bale-vpn-rust        && cargo clean
	cd bale-vpn-android/rust && cargo clean

clean-android:
	$(GRADLEW) clean
	rm -rf bale-vpn-android/rust/jniLibs

# ── Sync (rsync to remote build host) ──────────────────────────────

sync:
ifeq ($(REMOTE),)
	@echo "ERROR: REMOTE not set. Example: make sync REMOTE=user@host:/path/to/BaleVPN"
	@exit 1
endif
	rsync $(RSYNC_FLAGS) ./ $(REMOTE)/

sync-dry:
ifeq ($(REMOTE),)
	@echo "ERROR: REMOTE not set. Example: make sync-dry REMOTE=user@host:/path/to/BaleVPN"
	@exit 1
endif
	rsync $(RSYNC_FLAGS) --dry-run --itemize-changes ./ $(REMOTE)/

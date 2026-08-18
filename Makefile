# Lanyard — build, install, and set everything up with a single `make`.
#
#   make            build the app, install to /Applications, clear the stale
#                   Accessibility grant, set up the shell profile, and launch
#   make build      just compile and bundle the .app
#   make install    kill + replace /Applications/Lanyard.app, reset the grant
#   make update     stop the running app, replace it with a fresh build, relaunch
#   make reset-ax   only clear the (stale) Accessibility entry
#   make test       run the Rust test suite
#   make doctor     print exactly what Lanyard sees
#   make dev        hot-reloading dev build

APP       := /Applications/Lanyard.app
BUNDLE    := src-tauri/target/release/bundle/macos/Lanyard.app
BUNDLE_ID := dev.justin06lee.lanyard

.PHONY: all build install update reset-ax shell launch test doctor dev

all: build install shell launch
	@echo ""
	@echo "Done. If macOS shows an Accessibility prompt, grant it — the pill"
	@echo "appears as soon as a Claude Code terminal has focus. (Lanyard also"
	@echo "re-prompts itself at launch whenever access is missing.)"

# Updater artifacts must be minisign-signed; local builds pick the key up
# from ~/.tauri automatically when it exists (CI uses the repo secret).
# The key is encrypted with an empty password; the password variable must
# exist (empty) or the CLI prompts for it and dies without a tty.
ifneq (,$(wildcard $(HOME)/.tauri/lanyard.key))
build: export TAURI_SIGNING_PRIVATE_KEY := $(HOME)/.tauri/lanyard.key
build: export TAURI_SIGNING_PRIVATE_KEY_PASSWORD :=
endif

build:
	bun install
	bun run build -- --bundles app

install:
	-pkill -x lanyard
	rm -rf $(APP)
	cp -R $(BUNDLE) $(APP)
	@# Re-register with LaunchServices: notification consent is keyed to the
	@# app's sealed identity, and a stale registration from a previous build
	@# makes usernoted refuse ("not allowed for this application").
	-/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister -f $(APP)
	@# The old grant is keyed to the replaced binary's code hash; clearing it
	@# lets the fresh binary's request actually show the consent prompt.
	$(MAKE) reset-ax

update: build install launch

reset-ax:
	@# System Settings caches the TCC table; a stale open pane hides the reset.
	-osascript -e 'quit app "System Settings"'
	-tccutil reset Accessibility $(BUNDLE_ID)

shell:
	./scripts/setup-shell.sh

launch:
	open $(APP)

test:
	cargo test --manifest-path src-tauri/Cargo.toml

doctor:
	cargo run --manifest-path src-tauri/Cargo.toml --bin lanyard-doctor

dev:
	bun run dev

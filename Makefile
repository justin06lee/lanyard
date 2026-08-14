# Lanyard — build, install, and set everything up with a single `make`.
#
#   make            build the app, install to /Applications, clear the stale
#                   Accessibility grant, set up the shell profile, and launch
#   make build      just compile and bundle the .app
#   make install    kill + replace /Applications/Lanyard.app, reset the grant
#   make reset-ax   only clear the (stale) Accessibility entry
#   make test       run the Rust test suite
#   make doctor     print exactly what Lanyard sees
#   make dev        hot-reloading dev build

APP       := /Applications/Lanyard.app
BUNDLE    := src-tauri/target/release/bundle/macos/Lanyard.app
BUNDLE_ID := dev.justin06lee.lanyard

.PHONY: all build install reset-ax shell launch test doctor dev

all: build install shell launch
	@echo ""
	@echo "Done. If macOS shows an Accessibility prompt, grant it — the pill"
	@echo "appears as soon as a Claude Code terminal has focus. (Lanyard also"
	@echo "re-prompts itself at launch whenever access is missing.)"

build:
	npm install
	npm run build -- --bundles app

install:
	-pkill -x lanyard
	rm -rf $(APP)
	cp -R $(BUNDLE) $(APP)
	@# The old grant is keyed to the replaced binary's code hash; clearing it
	@# lets the fresh binary's request actually show the consent prompt.
	$(MAKE) reset-ax

reset-ax:
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
	npm run dev

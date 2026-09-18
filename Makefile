# ccs — build, test, install.
#
# Installs the CLI through cargo rather than copying the binary, so there is one
# install record and one copy on PATH. PREFIX follows CARGO_HOME by default,
# which is where `cargo install` would have put it anyway; override it for a
# system-wide CLI install (`sudo make install-cli PREFIX=/usr/local`).

PREFIX ?= $(or $(CARGO_HOME),$(HOME)/.cargo)
BIN := ccs

# The app: a Rust crate under app/, written with GPUI Kit. On macOS it is wrapped
# into a bundle, because notifications and launch-at-login want one, and
# ad-hoc signed so it runs on the machine that built it; elsewhere it is the
# binary and a desktop entry.
APP := build/ccs.app
APPS ?= /Applications
BIN_DIR ?= $(HOME)/.local/bin
UNAME := $(shell uname -s)

.PHONY: all build install install-cli uninstall test fmt lint check clean help \
	app install-app uninstall-app test-app lint-app

all: build

build: ## compile the release binary
	cargo build --release

install: install-cli install-app ## install both the CLI and app

install-cli: ## build and put ccs on PATH (PREFIX overrides where)
	cargo install --path . --root '$(PREFIX)' --force

uninstall: ## remove an installed ccs
	cargo uninstall --root '$(PREFIX)' $(BIN)

test: ## run the test suite
	cargo test

fmt: ## verify formatting
	cargo fmt --check

lint: ## clippy over the workspace, with warnings as errors
	cargo clippy --workspace --all-targets -- -D warnings

check: fmt lint test lint-app test-app ## fmt, lint and test — everything before a commit

lint-app: ## lint the native app
	cargo clippy -p ccs-app --all-targets -- -D warnings

test-app: ## run the app's tests
	cargo test -p ccs-app

app: ## build the app into build/ (a bundle on macOS)
	cargo build --release -p ccs-app
	rm -rf build
ifeq ($(UNAME),Darwin)
	mkdir -p '$(APP)/Contents/MacOS'
	cp target/release/ccs-app '$(APP)/Contents/MacOS/ccs-app'
	cp app/Info.plist '$(APP)/Contents/Info.plist'
	codesign --force --sign - '$(APP)'
else
	mkdir -p build
	cp target/release/ccs-app build/ccs-app
	cp app/assets/ccs.desktop build/ccs.desktop
endif

install-app: app ## install the app (APPS or BIN_DIR overrides where)
ifeq ($(UNAME),Darwin)
	rm -rf '$(APPS)/ccs.app'
	cp -R '$(APP)' '$(APPS)/ccs.app'
else
	mkdir -p '$(BIN_DIR)' '$(HOME)/.local/share/applications'
	cp build/ccs-app '$(BIN_DIR)/ccs-app'
	sed 's|^Exec=.*|Exec=$(BIN_DIR)/ccs-app|' build/ccs.desktop > '$(HOME)/.local/share/applications/ccs.desktop'
endif

uninstall-app: ## remove the installed app
	rm -rf '$(APPS)/ccs.app' '$(BIN_DIR)/ccs-app' '$(HOME)/.local/share/applications/ccs.desktop'

clean: ## remove build artefacts
	cargo clean
	rm -rf build

help: ## list targets
	@grep -hE '^[a-z][a-z-]*:.*##' $(MAKEFILE_LIST) \
		| sed -e 's/:[^#]*## /|/' \
		| awk -F'|' '{ printf "  %-14s %s\n", $$1, $$2 }'

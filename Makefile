.PHONY: help build install uninstall install-units uninstall-units du test lint check docs-check clean clean-debug clean-release clean-stale

# root installs to /usr/local, anyone else to ~/.local; override with PREFIX=
PREFIX ?= $(if $(filter 0,$(shell id -u)),/usr/local,$(HOME)/.local)
BINDIR ?= $(PREFIX)/bin
SYSTEMD_UNIT_DIR ?= $(PREFIX)/lib/systemd/system
TARGET_DIR ?= $(or $(CARGO_TARGET_DIR),target)

# Units are only installed for a root install (or a root packaging run with DESTDIR).
ifeq ($(shell id -u),0)
INSTALL_UNITS = install-units
UNINSTALL_UNITS = uninstall-units
else
INSTALL_UNITS =
UNINSTALL_UNITS =
endif


help:  ## list targets
	@grep -E '^[a-z-]+:.*##' Makefile | sed 's/:.*##/ -/'

build:  ## release build of lu (with the tui feature) and ting
	cargo build --release --locked -p lu --features tui
	cargo build --release --locked -p tuishou-ting

test:  ## workspace tests, with and without the tui feature
	cargo test --locked --workspace
	cargo test --locked --workspace --features tui

lint:  ## format check and clippy with warnings denied, with and without the tui feature
	cargo fmt --all --check
	cargo clippy --workspace --all-targets --locked -- -D warnings
	cargo clippy --workspace --all-targets --locked --features tui -- -D warnings

check: lint test  ## run lint and test

docs-check:  ## check README sections, doc links, command names and config examples
	./scripts/check-docs.sh

install: build $(INSTALL_UNITS)  ## build and install lu and ting into PREFIX/bin (~/.local, or /usr/local as root; PREFIX= and DESTDIR= override)
	install -d '$(DESTDIR)$(BINDIR)'
	install -m 0755 '$(TARGET_DIR)/release/lu' '$(DESTDIR)$(BINDIR)/lu'
	install -m 0755 '$(TARGET_DIR)/release/ting' '$(DESTDIR)$(BINDIR)/ting'
	@echo "installed $(DESTDIR)$(BINDIR)/lu and $(DESTDIR)$(BINDIR)/ting"

install-units:  ## install systemd units into PREFIX/lib/systemd/system (root only)
	install -d '$(DESTDIR)$(SYSTEMD_UNIT_DIR)'
	install -m 0644 contrib/lu-controller.service contrib/ting.service '$(DESTDIR)$(SYSTEMD_UNIT_DIR)/'
	@echo "installed units in $(DESTDIR)$(SYSTEMD_UNIT_DIR)"

uninstall: $(UNINSTALL_UNITS)  ## remove the installed binaries and, as root, the units
	rm -f '$(DESTDIR)$(BINDIR)/lu' '$(DESTDIR)$(BINDIR)/ting'

uninstall-units:  ## remove the systemd units from PREFIX/lib/systemd/system (root only)
	rm -f '$(DESTDIR)$(SYSTEMD_UNIT_DIR)/lu-controller.service' \
	      '$(DESTDIR)$(SYSTEMD_UNIT_DIR)/ting.service'
	@echo "removed units from $(DESTDIR)$(SYSTEMD_UNIT_DIR)"

du:  ## show target/ size breakdown (debug/release/musl), biggest first
	@du -sh target/* 2>/dev/null | sort -rh || echo "no target/ yet"

clean-debug:  ## cargo clean, debug profile only -- debug/ is almost always the bulk of target/ (stale incremental artifacts, never GC'd by cargo itself)
	cargo clean --profile dev

clean-release:  ## cargo clean, release profile only
	cargo clean --release

clean-stale:  ## remove build artifacts untouched for 14+ days, without a full clean (needs: cargo install cargo-sweep)
	cargo sweep --time 14 .

clean:  ## full cargo clean -- next build recompiles everything from scratch
	cargo clean

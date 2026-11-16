.PHONY: help build install uninstall du clean clean-debug clean-release clean-stale

# root installs to /usr/local, anyone else to ~/.local; override with PREFIX=
PREFIX ?= $(if $(filter 0,$(shell id -u)),/usr/local,$(HOME)/.local)
BINDIR ?= $(PREFIX)/bin
TARGET_DIR ?= $(or $(CARGO_TARGET_DIR),target)


help:  ## list targets
	@grep -E '^[a-z-]+:.*##' Makefile | sed 's/:.*##/ -/'

build:  ## release build of lu with the tui feature
	cargo build --release --locked -p lu --features tui

install: build  ## build and install lu into PREFIX/bin (~/.local, or /usr/local as root; PREFIX= and DESTDIR= override)
	install -d '$(DESTDIR)$(BINDIR)'
	install -m 0755 '$(TARGET_DIR)/release/lu' '$(DESTDIR)$(BINDIR)/lu'
	@echo "installed $(DESTDIR)$(BINDIR)/lu"

uninstall:  ## remove the installed binary
	rm -f '$(DESTDIR)$(BINDIR)/lu'

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

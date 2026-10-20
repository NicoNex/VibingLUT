# VibingLUT — build, run and package.
#   make run FILE=photo.RW2     debug run, optionally opening a photo
#   make release / run-release  optimised build
#   make install PREFIX=~/.local
#   make arch                   build an Arch Linux package with makepkg

PREFIX  ?= /usr/local
DESTDIR ?=
CARGO   ?= cargo
FILE    ?=

APP_ID  := io.github.vibinglut.VibingLUT
BIN     := vibinglut
VERSION := $(shell sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n1)
ARCHDIR := packaging/arch

.PHONY: all build run release run-release test check fmt clean install uninstall arch

all: release

build:
	$(CARGO) build

run:
	$(CARGO) run -p $(BIN) -- $(FILE)

release:
	$(CARGO) build --release

run-release:
	$(CARGO) run --release -p $(BIN) -- $(FILE)

test:
	$(CARGO) test --workspace

check:
	$(CARGO) fmt --all --check
	$(CARGO) clippy --workspace --all-targets -- -D warnings
	$(CARGO) test --workspace

fmt:
	$(CARGO) fmt --all

target/release/$(BIN):
	$(CARGO) build --release

install: target/release/$(BIN)
	install -Dm755 target/release/$(BIN) $(DESTDIR)$(PREFIX)/bin/$(BIN)
	install -Dm644 data/$(APP_ID).desktop $(DESTDIR)$(PREFIX)/share/applications/$(APP_ID).desktop
	install -Dm644 data/$(APP_ID).svg $(DESTDIR)$(PREFIX)/share/icons/hicolor/scalable/apps/$(APP_ID).svg
	install -Dm644 LICENSE $(DESTDIR)$(PREFIX)/share/licenses/$(BIN)/LICENSE

uninstall:
	rm -f $(DESTDIR)$(PREFIX)/bin/$(BIN) \
	      $(DESTDIR)$(PREFIX)/share/applications/$(APP_ID).desktop \
	      $(DESTDIR)$(PREFIX)/share/icons/hicolor/scalable/apps/$(APP_ID).svg
	rm -rf $(DESTDIR)$(PREFIX)/share/licenses/$(BIN)

# Packages the committed HEAD (commit first!).
arch:
	git archive --format=tar.gz --prefix=$(BIN)-$(VERSION)/ -o $(ARCHDIR)/$(BIN)-$(VERSION).tar.gz HEAD
	sed -i 's/^pkgver=.*/pkgver=$(VERSION)/' $(ARCHDIR)/PKGBUILD
	cd $(ARCHDIR) && makepkg -f
	@echo "Install with: sudo pacman -U $(ARCHDIR)/$(BIN)-$(VERSION)-*.pkg.tar.zst"

clean:
	$(CARGO) clean
	rm -rf $(ARCHDIR)/src $(ARCHDIR)/pkg $(ARCHDIR)/*.tar.gz $(ARCHDIR)/*.pkg.tar.zst

# VibingLUT — build, run and package.
#   make run FILE=photo.RW2     debug run, optionally opening a photo
#   make release / run-release  optimised build
#   make install PREFIX=~/.local
#   make arch                   build an Arch Linux package with makepkg
#   make pot                    refresh po/vibinglut.pot and merge it into po/*.po

PREFIX  ?= /usr/local
DESTDIR ?=
CARGO   ?= cargo
FILE    ?=

APP_ID  := io.github.vibinglut.VibingLUT
BIN     := vibinglut
VERSION := $(shell sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n1)
ARCHDIR := packaging/arch
LINGUAS := $(basename $(notdir $(wildcard po/*.po)))
MOS     := $(foreach l,$(LINGUAS),target/locale/$(l)/LC_MESSAGES/$(BIN).mo)

.PHONY: all build run release run-release test check fmt clean install uninstall arch pot locale

all: release

build: locale
	$(CARGO) build

run: locale
	$(CARGO) run -p $(BIN) -- $(FILE)

release: locale
	$(CARGO) build --release

run-release: locale
	$(CARGO) run --release -p $(BIN) -- $(FILE)

test:
	$(CARGO) test --workspace

check:
	$(CARGO) fmt --all --check
	$(CARGO) clippy --workspace --all-targets -- -D warnings
	$(CARGO) test --workspace

fmt:
	$(CARGO) fmt --all

locale: $(MOS)

target/locale/%/LC_MESSAGES/$(BIN).mo: po/%.po
	@mkdir -p $(dir $@)
	msgfmt --check -o $@ $<

pot:
	xgettext -L Rust --from-code=UTF-8 -k --keyword=gettext --keyword='gettext!' \
	  --package-name=VibingLUT -o po/$(BIN).pot gtk/src/*.rs
	for l in $(LINGUAS); do msgmerge -U --backup=none po/$$l.po po/$(BIN).pot; done

target/release/$(BIN):
	$(CARGO) build --release

install: target/release/$(BIN) locale
	install -Dm755 target/release/$(BIN) $(DESTDIR)$(PREFIX)/bin/$(BIN)
	for l in $(LINGUAS); do install -Dm644 target/locale/$$l/LC_MESSAGES/$(BIN).mo \
	  $(DESTDIR)$(PREFIX)/share/locale/$$l/LC_MESSAGES/$(BIN).mo; done
	install -Dm644 data/$(APP_ID).desktop $(DESTDIR)$(PREFIX)/share/applications/$(APP_ID).desktop
	install -Dm644 data/$(APP_ID).svg $(DESTDIR)$(PREFIX)/share/icons/hicolor/scalable/apps/$(APP_ID).svg
	install -Dm644 LICENSE $(DESTDIR)$(PREFIX)/share/licenses/$(BIN)/LICENSE

uninstall:
	rm -f $(DESTDIR)$(PREFIX)/bin/$(BIN) \
	      $(DESTDIR)$(PREFIX)/share/applications/$(APP_ID).desktop \
	      $(DESTDIR)$(PREFIX)/share/icons/hicolor/scalable/apps/$(APP_ID).svg \
	      $(foreach l,$(LINGUAS),$(DESTDIR)$(PREFIX)/share/locale/$(l)/LC_MESSAGES/$(BIN).mo)
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

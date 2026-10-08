# Per-user install for development; Flatpak packaging comes later.
PREFIX ?= $(HOME)/.local
BINDIR = $(PREFIX)/bin
DATADIR = $(PREFIX)/share
AUTOSTART = $(HOME)/.config/autostart
ID = io.github.selfref.Todav

install:
	cargo build --release -p todav-linux
	install -Dm755 ../target/release/todav-linux $(BINDIR)/todav-linux
	install -Dm644 data/$(ID).desktop $(DATADIR)/applications/$(ID).desktop
	mkdir -p $(DATADIR)/dbus-1/services
	sed 's|@bindir@|$(BINDIR)|' data/$(ID).service.in > $(DATADIR)/dbus-1/services/$(ID).service
	install -Dm644 data/$(ID)-autostart.desktop $(AUTOSTART)/$(ID).desktop

uninstall:
	rm -f $(BINDIR)/todav-linux $(DATADIR)/applications/$(ID).desktop \
	  $(DATADIR)/dbus-1/services/$(ID).service $(AUTOSTART)/$(ID).desktop

.PHONY: install uninstall

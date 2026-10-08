UUID = todav@selfref.dev
DEST = $(HOME)/.local/share/gnome-shell/extensions/$(UUID)

install:
	glib-compile-schemas schemas
	mkdir -p $(DEST)
	cp -r metadata.json extension.js dbus.js stylesheet.css schemas $(DEST)/

uninstall:
	rm -rf $(DEST)

.PHONY: install uninstall

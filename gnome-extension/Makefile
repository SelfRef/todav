UUID = todav@selfref.dev
DEST = $(HOME)/.local/share/gnome-shell/extensions/$(UUID)

install:
	glib-compile-schemas schemas
	mkdir -p $(DEST)
	cp -r metadata.json extension.js dbus.js stylesheet.css schemas $(DEST)/

# Development: symlink the source dir so edits apply on the next shell start.
link:
	glib-compile-schemas schemas
	@if [ -e $(DEST) ] && [ ! -L $(DEST) ]; then \
	  echo "$(DEST) is an installed copy; run 'make uninstall' first"; exit 1; fi
	mkdir -p $(dir $(DEST))
	ln -sfn $(CURDIR) $(DEST)

uninstall:
	rm -rf $(DEST)

# Nested GNOME Shell with the extension and the dev build of the app (needs mutter-devkit).
nested: link
	./run-nested.sh

.PHONY: install link uninstall nested

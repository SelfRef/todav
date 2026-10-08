#!/bin/sh
# Runs a nested GNOME Shell on its own session bus, with the extension enabled and
# the workspace build of the app as its D-Bus backend. Expects `make link` and
# `cargo build -p todav-linux` to have been run.
set -eu

uuid=todav@selfref.dev
app=$(dirname "$0")/../target/debug/todav-linux
display=todav-nested

if [ -z "${DBUS_RUN_SESSION:-}" ]; then
  # Own bus: the host's app instance and installed D-Bus service stay out of the way.
  DBUS_RUN_SESSION=1 exec dbus-run-session -- "$0" "$@"
fi

MUTTER_DEBUG_DUMMY_MODE_SPECS=${MUTTER_DEBUG_DUMMY_MODE_SPECS:-1600x900} \
SHELL_DEBUG=${SHELL_DEBUG:-all} \
  gnome-shell --devkit --wayland --wayland-display=$display &
shell=$!

while [ ! -S "$XDG_RUNTIME_DIR/$display" ]; do
  kill -0 $shell 2>/dev/null || exit 1
  sleep 0.2
done

gnome-extensions enable $uuid || true
WAYLAND_DISPLAY=$display "$app" --gapplication-service &
app_pid=$!

wait $shell || true
kill $app_pid 2>/dev/null || true

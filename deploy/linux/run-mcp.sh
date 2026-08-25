#!/bin/sh
set -eu

if [ -z "${DBUS_SESSION_BUS_ADDRESS:-}" ]; then
    exec dbus-run-session -- "$0"
fi

printf '\n' | gnome-keyring-daemon --unlock --components=secrets >/dev/null 2>&1 || true
exec /opt/mc-feedback-viewer/mc-feedback-viewer --mcp

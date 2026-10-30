#!/usr/bin/env bash
# USB over Net: remove the server service and program installed by install.sh.
#
#   ./uninstall.sh            remove the service and the program, keep your settings
#   ./uninstall.sh --purge    also delete the settings file (/etc/default/usbip-server)
#
# Rust, the build packages and this project folder are left alone.

set -euo pipefail

BIN_DIR="${BIN_DIR:-/usr/local/bin}"
UNIT_DIR="${UNIT_DIR:-/etc/systemd/system}"
DEFAULTS_FILE="${DEFAULTS_FILE:-/etc/default/usbip-server}"
SERVICE=usbip-server

PURGE=0
for arg in "$@"; do
    case "$arg" in
        --purge) PURGE=1 ;;
        -h|--help)
            sed -n '2,8p' "$0" | sed 's/^# \{0,1\}//'
            exit 0 ;;
        *) echo "Unknown option: $arg (try --help)" >&2; exit 1 ;;
    esac
done

if [ "$(id -u)" -eq 0 ]; then
    SUDO=""
else
    command -v sudo >/dev/null 2>&1 || { echo "Error: sudo is needed (or run this as root)" >&2; exit 1; }
    SUDO="sudo"
fi

have_systemd() { command -v systemctl >/dev/null 2>&1 && [ -d /run/systemd/system ]; }

echo "==> Removing the $SERVICE service and program"

if have_systemd; then
    # Stop it first: a running server holds the USB devices and port 3240.
    $SUDO systemctl disable --now "$SERVICE" 2>/dev/null || true
fi

removed=0
for f in "$UNIT_DIR/$SERVICE.service" "$BIN_DIR/usbip-server"; do
    if [ -e "$f" ]; then
        $SUDO rm -f "$f"
        echo "Removed $f"
        removed=1
    fi
done

if have_systemd; then
    $SUDO systemctl daemon-reload
    $SUDO systemctl reset-failed "$SERVICE" 2>/dev/null || true
fi

if [ "$PURGE" -eq 1 ]; then
    if [ -e "$DEFAULTS_FILE" ]; then
        $SUDO rm -f "$DEFAULTS_FILE"
        echo "Removed $DEFAULTS_FILE"
        removed=1
    fi
elif [ -e "$DEFAULTS_FILE" ]; then
    echo "Kept your settings in $DEFAULTS_FILE (use --purge to delete them too)."
fi

if [ "$removed" -eq 0 ]; then
    echo "Nothing to remove: the service and program were not installed."
else
    echo "Done. Rust and the build packages were left installed."
fi

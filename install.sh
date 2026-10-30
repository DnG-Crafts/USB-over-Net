#!/usr/bin/env bash
# USB over Net: one-step installer for the server on a Raspberry Pi (or any Debian/Ubuntu-style Linux).
#
#   git clone https://github.com/DnG-Crafts/USB-over-Net.git
#   cd USB-over-Net
#   ./install.sh
#
# It installs the build packages and Rust, builds the server, installs it as a systemd service
# that starts at boot, and starts it. Run it again at any time to update after a `git pull`.
# Run it as your normal user; it asks for sudo only for the steps that need it.

set -euo pipefail

BIN_DIR="${BIN_DIR:-/usr/local/bin}"
UNIT_DIR="${UNIT_DIR:-/etc/systemd/system}"
DEFAULTS_FILE="${DEFAULTS_FILE:-/etc/default/usbip-server}"
SERVICE=usbip-server
MIN_RUST_MINOR=85 # Rust 1.85 or newer

usage() {
    cat <<'EOF'
Usage: ./install.sh [options]

Installs the packages and Rust, builds usbip-server, and sets it up as a service that runs at boot.

Options:
  --allow VID:PID   Share only this device (repeat for several). Find IDs with `lsusb`.
                    Without any --allow, every USB device except hubs is shared.
  --name NAME       The name shown in the app (default: this computer's host name).
  --no-mdns         Do not announce the server on the network.
  --listen ADDR     Address and port to listen on (default 0.0.0.0:3240).
  --no-service      Build and install the program only; do not create or start a service.
  --uninstall       Stop and remove the service and the installed program.
  -h, --help        Show this help.

The service's options are kept in /etc/default/usbip-server. Re-running without --allow, --name,
--no-mdns or --listen keeps what is there; edit that file and run
`sudo systemctl restart usbip-server` to change them later.
EOF
}

say() { printf '\n==> %s\n' "$*"; }
die() { printf '\nError: %s\n' "$*" >&2; exit 1; }

ALLOW=()
NAME=""
NO_MDNS=0
LISTEN=""
NO_SERVICE=0
UNINSTALL=0
CUSTOM_ARGS=0

while [ $# -gt 0 ]; do
    case "$1" in
        --allow)
            [ $# -ge 2 ] || die "--allow needs a VID:PID value, for example --allow 046d:08e3"
            [[ "$2" =~ ^[0-9a-fA-F]{4}:[0-9a-fA-F]{4}$ ]] || die "'$2' is not a VID:PID like 046d:08e3 (see lsusb)"
            ALLOW+=("$2"); CUSTOM_ARGS=1; shift 2 ;;
        --name)
            [ $# -ge 2 ] || die "--name needs a value"
            [[ "$2" =~ ^[A-Za-z0-9._-]+$ ]] || die "--name may only contain letters, digits, dots, dashes and underscores (no spaces)"
            NAME="$2"; CUSTOM_ARGS=1; shift 2 ;;
        --no-mdns) NO_MDNS=1; CUSTOM_ARGS=1; shift ;;
        --listen)
            [ $# -ge 2 ] || die "--listen needs a value like 0.0.0.0:3240"
            [[ "$2" =~ ^[0-9A-Za-z.:-]+$ ]] || die "'$2' does not look like an address:port"
            LISTEN="$2"; CUSTOM_ARGS=1; shift 2 ;;
        --no-service) NO_SERVICE=1; shift ;;
        --uninstall) UNINSTALL=1; shift ;;
        -h|--help) usage; exit 0 ;;
        *) usage >&2; die "unknown option: $1" ;;
    esac
done

# Started with sudo? Build as the person who ran it, so the project folder and Rust stay theirs.
if [ "$(id -u)" -eq 0 ] && [ -n "${SUDO_USER:-}" ] && [ "$SUDO_USER" != root ]; then
    exec sudo -u "$SUDO_USER" -H env BIN_DIR="$BIN_DIR" UNIT_DIR="$UNIT_DIR" DEFAULTS_FILE="$DEFAULTS_FILE" "$0" "$@"
fi

if [ "$(id -u)" -eq 0 ]; then
    SUDO=""
else
    command -v sudo >/dev/null 2>&1 || die "sudo is needed (or run this as root)"
    SUDO="sudo"
fi

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$HERE"
[ -f Cargo.toml ] && [ -d crates/server ] || die "run this from the USB-over-Net folder (the one with Cargo.toml)"

have_systemd() { command -v systemctl >/dev/null 2>&1 && [ -d /run/systemd/system ]; }

# ---------------------------------------------------------------- uninstall
if [ "$UNINSTALL" -eq 1 ]; then
    say "Removing the $SERVICE service and program"
    if have_systemd; then
        $SUDO systemctl disable --now "$SERVICE" 2>/dev/null || true
    fi
    $SUDO rm -f "$UNIT_DIR/$SERVICE.service" "$BIN_DIR/usbip-server"
    have_systemd && $SUDO systemctl daemon-reload || true
    echo "Removed. Settings in $DEFAULTS_FILE were kept; delete that file too if you want them gone."
    echo "Rust and the packages were left installed."
    exit 0
fi

# ---------------------------------------------------------------- checks
[ "$(uname -s)" = Linux ] || die "the server runs on Linux (this is $(uname -s))"

# ---------------------------------------------------------------- packages
say "Installing build tools and libusb"
if command -v apt-get >/dev/null 2>&1; then
    $SUDO apt-get update
    $SUDO apt-get install -y build-essential pkg-config libusb-1.0-0-dev git curl ca-certificates
else
    die "this script needs apt (Raspberry Pi OS, Debian, Ubuntu). On another system install a C compiler, pkg-config,
libusb-1.0 development files and Rust (https://rustup.rs) yourself, then run:
  cargo build --release -p usbip-server"
fi

# ---------------------------------------------------------------- rust
# Use rustup's compiler first: the one from apt is usually too old.
export PATH="$HOME/.cargo/bin:$PATH"

rust_minor() { rustc --version 2>/dev/null | sed -n 's/^rustc 1\.\([0-9]*\)\..*/\1/p'; }

say "Checking Rust"
minor="$(rust_minor || true)"
if [ -z "$minor" ] || [ "$minor" -lt "$MIN_RUST_MINOR" ]; then
    if command -v rustup >/dev/null 2>&1; then
        echo "Updating Rust (found 1.${minor:-none}, need 1.$MIN_RUST_MINOR or newer)"
        rustup update stable
        rustup default stable
    else
        echo "Installing Rust with rustup"
        curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
    fi
    # shellcheck disable=SC1091
    [ -f "$HOME/.cargo/env" ] && . "$HOME/.cargo/env"
    minor="$(rust_minor || true)"
    [ -n "$minor" ] && [ "$minor" -ge "$MIN_RUST_MINOR" ] || die "Rust 1.$MIN_RUST_MINOR or newer is required, but found: $(rustc --version 2>&1 || echo none)"
fi
echo "Using $(rustc --version)"

# ---------------------------------------------------------------- build
say "Building the server (the first build takes several minutes on a Pi)"
cargo build --release -p usbip-server
BUILT="$HERE/target/release/usbip-server"
[ -x "$BUILT" ] || die "the build finished but $BUILT is missing"

say "Installing the program to $BIN_DIR"
$SUDO install -d "$BIN_DIR"
# Replace via a temporary name so a running server can be swapped out cleanly.
$SUDO install -m 0755 "$BUILT" "$BIN_DIR/usbip-server.new"
$SUDO mv -f "$BIN_DIR/usbip-server.new" "$BIN_DIR/usbip-server"

if [ "$NO_SERVICE" -eq 1 ]; then
    say "Done (no service requested)"
    echo "Run it with:  sudo $BIN_DIR/usbip-server --allow VID:PID"
    exit 0
fi

# ---------------------------------------------------------------- service
have_systemd || die "systemd was not found, so no service was created. The program is installed: sudo $BIN_DIR/usbip-server"

say "Setting up the $SERVICE service"

# The server's options live in a small file so the service survives re-installs unchanged.
if [ "$CUSTOM_ARGS" -eq 1 ] || [ ! -f "$DEFAULTS_FILE" ]; then
    args=()
    [ -n "$LISTEN" ] && args+=(--listen "$LISTEN")
    [ -n "$NAME" ] && args+=(--name "$NAME")
    [ "$NO_MDNS" -eq 1 ] && args+=(--no-mdns)
    for id in ${ALLOW[@]+"${ALLOW[@]}"}; do args+=(--allow "$id"); done

    tmp="$(mktemp)"
    {
        echo "# Options for the usbip-server service. After editing: sudo systemctl restart usbip-server"
        echo "# Share only specific devices with --allow VID:PID (repeat it). Find IDs with lsusb."
        echo "# Other options: --name NAME, --no-mdns, --listen ADDR:PORT"
        echo "USBIP_ARGS=\"${args[*]+${args[*]}}\""
    } > "$tmp"
    $SUDO install -m 0644 "$tmp" "$DEFAULTS_FILE"
    rm -f "$tmp"
    echo "Wrote $DEFAULTS_FILE"
else
    echo "Keeping the existing options in $DEFAULTS_FILE"
fi

unit="$(mktemp)"
cat > "$unit" <<EOF
[Unit]
Description=USB over Net server
After=network-online.target
Wants=network-online.target

[Service]
EnvironmentFile=-$DEFAULTS_FILE
ExecStart=$BIN_DIR/usbip-server \$USBIP_ARGS
Restart=on-failure
RestartSec=3
# Runs as root so libusb can claim devices and detach their kernel drivers.

[Install]
WantedBy=multi-user.target
EOF
$SUDO install -m 0644 "$unit" "$UNIT_DIR/$SERVICE.service"
rm -f "$unit"

$SUDO systemctl daemon-reload
$SUDO systemctl enable "$SERVICE"
$SUDO systemctl restart "$SERVICE"
sleep 2

# ---------------------------------------------------------------- result
if $SUDO systemctl is-active --quiet "$SERVICE"; then
    say "The server is running and will start at every boot"
else
    $SUDO journalctl -u "$SERVICE" -n 20 --no-pager || true
    die "the service did not start. See the log above, or run: sudo journalctl -u $SERVICE -e"
fi

ip="$(hostname -I 2>/dev/null | awk '{print $1}')"
echo "Address : ${ip:-<pi-ip-address>}:3240 (the Windows app finds it by itself)"
echo "Status  : sudo systemctl status $SERVICE"
echo "Log     : sudo journalctl -u $SERVICE -f"
echo "Options : $DEFAULTS_FILE (edit, then: sudo systemctl restart $SERVICE)"
echo "Update  : git pull && ./install.sh"
if ! grep -q -- '--allow' "$DEFAULTS_FILE" 2>/dev/null; then
    echo
    echo "Note: every USB device on this Pi is shared, and there is no password. Anyone on your network"
    echo "can attach them. To share only some, run:  ./install.sh --allow VID:PID   (IDs from lsusb)"
fi

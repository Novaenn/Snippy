#!/bin/sh
# Installs Snippy for the current user and adds it to the application menu.
#
#   ./install.sh
#
# Builds release binaries and copies them into ~/.local/bin (or $XDG_BIN_HOME),
# installs the icon and a desktop entry under ~/.local/share (or $XDG_DATA_HOME).
# Nothing is installed system-wide, autostart is not enabled, and no vault,
# settings, or clipboard data is created or touched. Rerun to update; remove
# with ./uninstall.sh.
set -eu

ROOT=$(cd "$(dirname "$0")" && pwd)
cd "$ROOT"

BIN_DIR=${XDG_BIN_HOME:-$HOME/.local/bin}
DATA_DIR=${XDG_DATA_HOME:-$HOME/.local/share}
APP_DIR=$DATA_DIR/applications
ICON_DIR=$DATA_DIR/icons/hicolor/scalable/apps
DESKTOP=$APP_DIR/org.snippy.Snippy.desktop

# The desktop entry quotes this path; refuse characters that would need escaping.
case $BIN_DIR in
    *[\"\`\$\\%]* | *"
"*)
        echo "install.sh: unsupported characters in $BIN_DIR" >&2
        exit 1
        ;;
esac

if ! command -v cargo >/dev/null 2>&1; then
    echo "install.sh: cargo was not found. Install Rust and the build prerequisites listed in README.md." >&2
    exit 1
fi
offline=
if [ -z "${CARGO_HOME:-}" ] && [ -d .cache/cargo ]; then
    CARGO_HOME=$ROOT/.cache/cargo
    export CARGO_HOME
    offline=--offline
fi

echo "Building release binaries…"
cargo build --release --locked $offline -p snippy -p snippy-gui

# A daemon running from the installed binary would keep using the old files.
# Stop it gracefully (this locks the vault); the launcher restarts it on demand.
stopped=
pids=
for proc in /proc/[0-9]*; do
    exe=$(readlink "$proc/exe" 2>/dev/null) || continue
    case $exe in
        "$BIN_DIR/snippy" | "$BIN_DIR/snippy (deleted)") pids="$pids ${proc#/proc/}" ;;
    esac
done
if [ -n "$pids" ]; then
    echo "Stopping the installed snippy daemon (the vault locks; the served clip is released)…"
    # shellcheck disable=SC2086
    kill -TERM $pids 2>/dev/null || true
    waited=0
    for pid in $pids; do
        while kill -0 "$pid" 2>/dev/null && [ "$waited" -lt 100 ]; do
            sleep 0.1
            waited=$((waited + 1))
        done
    done
    stopped=1
fi

mkdir -p "$BIN_DIR" "$APP_DIR" "$ICON_DIR"

# Copy then rename, so a running GUI never sees a half-written file.
place() {
    install -m 0755 "$1" "$2.install-tmp"
    mv -f "$2.install-tmp" "$2"
}
place target/release/snippy "$BIN_DIR/snippy"
place target/release/snippy-gui "$BIN_DIR/snippy-gui"
place packaging/snippy-launch "$BIN_DIR/snippy-launch"
install -m 0644 packaging/snippy.svg "$ICON_DIR/snippy.svg.install-tmp"
mv -f "$ICON_DIR/snippy.svg.install-tmp" "$ICON_DIR/snippy.svg"

cat >"$DESKTOP.install-tmp" <<EOF
[Desktop Entry]
Type=Application
Version=1.5
Name=Snippy
GenericName=Clipboard Manager
Comment=Save chosen clipboard items to an encrypted vault and reuse them
Exec="$BIN_DIR/snippy-launch" manager
Icon=snippy
Terminal=false
Categories=Utility;
Keywords=clipboard;history;paste;copy;vault;
StartupNotify=false
StartupWMClass=snippy-gui
Actions=save;lock;
X-snippy-Launcher=true

[Desktop Action save]
Name=Save Current Clipboard
Exec="$BIN_DIR/snippy-launch" save

[Desktop Action lock]
Name=Lock Vault
Exec="$BIN_DIR/snippy-launch" lock
EOF
chmod 0644 "$DESKTOP.install-tmp"
mv -f "$DESKTOP.install-tmp" "$DESKTOP"

if command -v update-desktop-database >/dev/null 2>&1; then
    update-desktop-database "$APP_DIR" >/dev/null 2>&1 || true
fi
if command -v gtk-update-icon-cache >/dev/null 2>&1; then
    gtk-update-icon-cache -f -t "$DATA_DIR/icons/hicolor" >/dev/null 2>&1 || true
fi

echo
echo "Installed Snippy."
echo "  Programs: $BIN_DIR/snippy, snippy-gui, snippy-launch"
echo "  Menu:     $DESKTOP"
echo "  Icon:     $ICON_DIR/snippy.svg"
echo
echo "Open \"Snippy\" from your application menu; it starts the background service"
echo "(locked) when needed. To start it with your session, enable"
echo "\"Start with the desktop session\" in Snippy's Settings."
if [ -n "$stopped" ]; then
    echo "The previously running daemon was stopped; opening snippy starts the new one."
fi
case ":$PATH:" in
    *":$BIN_DIR:"*) ;;
    *) echo "Note: $BIN_DIR is not on your PATH; the menu entry works regardless." ;;
esac

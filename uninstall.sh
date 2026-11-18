#!/bin/sh
# Removes what install.sh installed. Your encrypted vault and settings are kept:
#   vault:    ${XDG_DATA_HOME:-~/.local/share}/snippy/
#   settings: ${XDG_CONFIG_HOME:-~/.config}/snippy/
set -eu

BIN_DIR=${XDG_BIN_HOME:-$HOME/.local/bin}
DATA_DIR=${XDG_DATA_HOME:-$HOME/.local/share}
CONFIG_DIR=${XDG_CONFIG_HOME:-$HOME/.config}
DESKTOP=$DATA_DIR/applications/org.snippy.Snippy.desktop
ICON=$DATA_DIR/icons/hicolor/scalable/apps/snippy.svg
AUTOSTART=$CONFIG_DIR/autostart/snippy.desktop

# Stop a daemon running from the installed binary (this locks the vault).
pids=
for proc in /proc/[0-9]*; do
    exe=$(readlink "$proc/exe" 2>/dev/null) || continue
    case $exe in
        "$BIN_DIR/snippy" | "$BIN_DIR/snippy (deleted)") pids="$pids ${proc#/proc/}" ;;
    esac
done
if [ -n "$pids" ]; then
    echo "Stopping the installed snippy daemon…"
    # shellcheck disable=SC2086
    kill -TERM $pids 2>/dev/null || true
    waited=0
    for pid in $pids; do
        while kill -0 "$pid" 2>/dev/null && [ "$waited" -lt 100 ]; do
            sleep 0.1
            waited=$((waited + 1))
        done
    done
fi

# Remove the autostart entry only if snippy wrote it for the installed binary.
if [ -f "$AUTOSTART" ] && [ ! -L "$AUTOSTART" ] &&
    grep -qx 'X-snippy-Autostart=true' "$AUTOSTART" &&
    grep -qF "$BIN_DIR/snippy" "$AUTOSTART"; then
    rm -f "$AUTOSTART"
    echo "Removed the snippy autostart entry."
fi

rm -f "$BIN_DIR/snippy" "$BIN_DIR/snippy-gui" "$BIN_DIR/snippy-launch"
if [ -f "$DESKTOP" ] && grep -qx 'X-snippy-Launcher=true' "$DESKTOP"; then
    rm -f "$DESKTOP"
fi
rm -f "$ICON"

if command -v update-desktop-database >/dev/null 2>&1; then
    update-desktop-database "$DATA_DIR/applications" >/dev/null 2>&1 || true
fi
if command -v gtk-update-icon-cache >/dev/null 2>&1; then
    gtk-update-icon-cache -f -t "$DATA_DIR/icons/hicolor" >/dev/null 2>&1 || true
fi

echo "Uninstalled Snippy. Your vault and settings were kept:"
echo "  ${DATA_DIR}/snippy/"
echo "  ${CONFIG_DIR}/snippy/"

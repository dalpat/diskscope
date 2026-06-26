#!/usr/bin/env sh
# Install a prebuilt DiskScope (from a release tarball) for the current user.
#
# No build tools and no -dev packages are required — only the GTK4/libadwaita
# *runtime*, which ships with modern GNOME (GTK 4.12+ / libadwaita 1.5+).
#
#   ./install.sh              install for the current user (under ~/.local)
#   ./install.sh --uninstall  remove it again
#
# You can also skip installing entirely and just run ./diskscope from here.
set -eu

APP_ID="dev.diskscope.DiskScope"
DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)

DATA_HOME="${XDG_DATA_HOME:-$HOME/.local/share}"
BIN_DIR="$HOME/.local/bin"
APP_DIR="$DATA_HOME/applications"
ICON_DIR="$DATA_HOME/icons/hicolor/scalable/apps"

DESKTOP_DEST="$APP_DIR/$APP_ID.desktop"
ICON_DEST="$ICON_DIR/$APP_ID.svg"
BIN_DEST="$BIN_DIR/diskscope"

refresh_caches() {
    if command -v update-desktop-database >/dev/null 2>&1; then
        update-desktop-database "$APP_DIR" >/dev/null 2>&1 || true
    fi
    if command -v gtk-update-icon-cache >/dev/null 2>&1; then
        gtk-update-icon-cache -q -t -f "$DATA_HOME/icons/hicolor" >/dev/null 2>&1 || true
    fi
}

if [ "${1:-}" = "--uninstall" ] || [ "${1:-}" = "-u" ]; then
    rm -f "$DESKTOP_DEST" "$ICON_DEST" "$BIN_DEST"
    refresh_caches
    echo "Removed DiskScope."
    exit 0
fi

[ -x "$DIR/diskscope" ] || { echo "error: diskscope binary not found next to this script" >&2; exit 1; }

mkdir -p "$BIN_DIR" "$APP_DIR" "$ICON_DIR"
install -m 0755 "$DIR/diskscope" "$BIN_DEST"
install -m 0644 "$DIR/$APP_ID.svg" "$ICON_DEST"
# Point the launcher at the absolute binary path so it works regardless of PATH.
sed "s|^Exec=diskscope |Exec=$BIN_DEST |" "$DIR/$APP_ID.desktop" > "$DESKTOP_DEST"
chmod 0644 "$DESKTOP_DEST"
refresh_caches

echo "DiskScope installed:"
echo "  binary   $BIN_DEST"
echo "  launcher $DESKTOP_DEST"
echo
echo "Launch it from the Activities Overview (search \"DiskScope\"), or run: diskscope"
case ":${PATH:-}:" in
    *":$BIN_DIR:"*) : ;;
    *) echo "Note: $BIN_DIR is not on your PATH — add it to run \"diskscope\" from a terminal." ;;
esac
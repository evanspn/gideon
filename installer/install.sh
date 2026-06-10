#!/bin/sh
# gideon installer for Kobo devices.
#
# Works in two modes:
#   * from a computer with the Kobo mounted over USB (auto-detects the mount)
#   * directly on the device (over SSH/telnet), where the root is /mnt/onboard
#
# Data safety rules (the whole point of this script):
#   * <root>/.adds/gideon/data/ is NEVER written, modified or deleted by an
#     install or upgrade — that's where settings and reading progress live.
#   * Before every upgrade, data/ is archived to backups/ (last 3 kept).
#   * Library progress stored next to your manga (.gideon/ dirs) is never
#     touched either — the installer only ever writes inside .adds/gideon/
#     and the NickelMenu config.
#   * Uninstall keeps data/ and backups/ unless you pass --purge.
#
# Usage:
#   install.sh [--binary PATH] [--root PATH] [--no-backup]
#   install.sh --uninstall [--root PATH] [--purge]
set -eu

BINARY=""
ROOT=""
UNINSTALL=0
PURGE=0
NO_BACKUP=0

usage() {
    sed -n '2,21p' "$0" | sed 's/^# \{0,1\}//'
    exit "${1:-0}"
}

while [ $# -gt 0 ]; do
    case "$1" in
        --binary) BINARY=$2; shift 2 ;;
        --root) ROOT=$2; shift 2 ;;
        --uninstall) UNINSTALL=1; shift ;;
        --purge) PURGE=1; shift ;;
        --no-backup) NO_BACKUP=1; shift ;;
        -h|--help) usage 0 ;;
        *) echo "unknown option: $1" >&2; usage 1 ;;
    esac
done

# --- locate the Kobo root ---

detect_root() {
    # On-device?
    if [ -d /mnt/onboard/.kobo ]; then
        echo /mnt/onboard
        return 0
    fi
    # USB-mounted on a computer.
    for candidate in /media/*/KOBOeReader /run/media/*/KOBOeReader /Volumes/KOBOeReader; do
        if [ -d "$candidate/.kobo" ]; then
            echo "$candidate"
            return 0
        fi
    done
    return 1
}

if [ -z "$ROOT" ]; then
    ROOT=$(detect_root) || {
        echo "error: couldn't find a connected Kobo (no .kobo directory found)." >&2
        echo "Pass the mount point explicitly with --root /path/to/KOBOeReader" >&2
        exit 1
    }
fi

if [ ! -d "$ROOT/.kobo" ]; then
    echo "error: $ROOT doesn't look like a Kobo (missing .kobo directory)." >&2
    echo "If you're sure, create that directory or point --root at the right place." >&2
    exit 1
fi

APP_DIR="$ROOT/.adds/gideon"
DATA_DIR="$APP_DIR/data"
BACKUP_DIR="$APP_DIR/backups"
NM_DIR="$ROOT/.adds/nm"

# --- uninstall ---

if [ "$UNINSTALL" -eq 1 ]; then
    echo "Uninstalling gideon from $APP_DIR"
    rm -rf "${APP_DIR:?}/bin" "${APP_DIR:?}/VERSION"
    rm -f "$NM_DIR/gideon"
    if [ "$PURGE" -eq 1 ]; then
        echo "Purging user data and backups (--purge)"
        rm -rf "$APP_DIR"
    else
        echo "User data preserved at $DATA_DIR (use --purge to remove it)"
    fi
    echo "Done."
    exit 0
fi

# --- install / upgrade ---

if [ -z "$BINARY" ]; then
    # Default: the gideon binary shipped next to this script.
    BINARY="$(dirname "$0")/gideon"
fi
if [ ! -f "$BINARY" ]; then
    echo "error: binary not found at $BINARY (use --binary PATH)" >&2
    exit 1
fi

mkdir -p "$APP_DIR/bin" "$DATA_DIR"

# Back up user data before touching anything else, so even a failed upgrade
# can't lose progress. Only when there's data worth backing up.
if [ "$NO_BACKUP" -eq 0 ] && [ -n "$(ls -A "$DATA_DIR" 2>/dev/null)" ]; then
    mkdir -p "$BACKUP_DIR"
    STAMP=$(date +%Y%m%d-%H%M%S)
    BACKUP_FILE="$BACKUP_DIR/data-$STAMP.tar.gz"
    echo "Backing up user data to $BACKUP_FILE"
    (cd "$APP_DIR" && tar czf "$BACKUP_FILE" data)
    # Keep only the 3 most recent backups.
    find "$BACKUP_DIR" -name 'data-*.tar.gz' 2>/dev/null | sort | head -n -3 | while read -r old; do
        rm -f "$old"
    done
fi

# Install the binary atomically: write next to the target, then rename, so a
# yanked cable mid-copy can't leave a half-written executable in place.
echo "Installing binary to $APP_DIR/bin/gideon"
cp "$BINARY" "$APP_DIR/bin/.gideon.new"
chmod 755 "$APP_DIR/bin/.gideon.new"
mv "$APP_DIR/bin/.gideon.new" "$APP_DIR/bin/gideon"

# Version: prefer the VERSION file shipped in the bundle (the binary itself
# usually can't execute on the installing computer — it's ARM code).
SCRIPT_DIR=$(dirname "$0")
if [ -f "$SCRIPT_DIR/VERSION" ]; then
    VERSION=$(cat "$SCRIPT_DIR/VERSION")
else
    VERSION=$("$APP_DIR/bin/gideon" --version 2>/dev/null || true)
fi
[ -n "$VERSION" ] || VERSION="gideon (version unknown)"
echo "$VERSION" > "$APP_DIR/VERSION"

# NickelMenu launcher entry — only when NickelMenu is installed, and only
# our own config file (we never modify the user's other entries).
if [ -d "$NM_DIR" ]; then
    echo "Installing NickelMenu entry"
    if [ -f "$SCRIPT_DIR/nickelmenu-gideon" ]; then
        cp "$SCRIPT_DIR/nickelmenu-gideon" "$NM_DIR/gideon"
    else
        printf 'menu_item :main :gideon :cmd_output :500:env GIDEON_DATA_DIR=/mnt/onboard/.adds/gideon/data /mnt/onboard/.adds/gideon/bin/gideon library /mnt/onboard/Manga 2>&1\n' > "$NM_DIR/gideon"
    fi
else
    echo ""
    echo "WARNING: NickelMenu is not installed on this device."
    echo "gideon is installed, but you won't be able to launch it from the"
    echo "Kobo home screen without a launcher. To fix this:"
    echo "  1. Install NickelMenu: https://pgaskin.net/NickelMenu/"
    echo "     (download KoboRoot.tgz into the device's .kobo folder and eject)"
    echo "  2. Re-run this installer — it will add the gideon menu entry."
    echo "Until then you can only run gideon over SSH/telnet."
    echo ""
fi

echo
echo "Installed: $VERSION"
echo "  app:     $APP_DIR/bin/gideon"
echo "  data:    $DATA_DIR (untouched by upgrades)"
[ -d "$BACKUP_DIR" ] && echo "  backups: $BACKUP_DIR"
echo "Done. Eject the device safely before unplugging."

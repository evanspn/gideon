#!/bin/sh
# Check on / restage NickelMenu on a USB-mounted Kobo.
#
#   nickelmenu.sh status  [--root PATH]
#   nickelmenu.sh install [--root PATH] [--tgz FILE] [--force] [--dry-run]
#
# Why this exists: after Kobo firmware updates the gideon menu entry can be
# gone from Home. NickelMenu's hook (libnm.so) lives on the device's system
# partition, which is NOT visible over USB, so a computer cannot see whether
# the hook is alive. Restaging is idempotent, so "install" is the one-step
# recovery: it drops a checksum-verified KoboRoot.tgz that the Kobo unpacks
# on its next boot.
#
# Safety: writes ONLY <root>/.kobo/KoboRoot.tgz. Never touches .adds/ (so
# gideon data and menu configs are left byte-identical). Refuses to overwrite
# a pending KoboRoot.tgz (e.g. a firmware update) unless --force.
set -eu

NM_VERSION="v0.6.0"
NM_SHA256="322ff9aa863860e8f5f7e0b55cae561c54bf95983b9bce1d19819d1225d064af"
NM_URL="https://github.com/pgaskin/NickelMenu/releases/download/$NM_VERSION/KoboRoot.tgz"

die() { echo "error: $*" >&2; exit 1; }

sha256_of() {
    if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1" | cut -d' ' -f1
    else shasum -a 256 "$1" | cut -d' ' -f1; fi
}

CMD="${1:-}"; [ $# -gt 0 ] && shift
ROOT=""; TGZ=""; FORCE=0; DRY=0
while [ $# -gt 0 ]; do
    case "$1" in
        --root) [ $# -ge 2 ] || die "--root needs a path"; ROOT="$2"; shift 2 ;;
        --tgz) [ $# -ge 2 ] || die "--tgz needs a file"; TGZ="$2"; shift 2 ;;
        --force) FORCE=1; shift ;;
        --dry-run) DRY=1; shift ;;
        *) die "unknown option: $1" ;;
    esac
done
[ "$CMD" = status ] || [ "$CMD" = install ] || die "usage: nickelmenu.sh status|install [--root PATH] [--tgz FILE] [--force] [--dry-run]"

if [ -z "$ROOT" ]; then
    for c in /Volumes/KOBOeReader /media/*/KOBOeReader /run/media/*/KOBOeReader; do
        [ -d "$c/.kobo" ] && ROOT="$c" && break
    done
fi
[ -n "$ROOT" ] || die "no Kobo found; plug it in or pass --root"
[ -d "$ROOT/.kobo" ] || die "$ROOT is not a Kobo (no .kobo directory)"

PENDING="$ROOT/.kobo/KoboRoot.tgz"

if [ "$CMD" = status ]; then
    echo "Kobo:      $ROOT"
    [ -f "$ROOT/.kobo/version" ] && echo "Firmware:  $(cut -d, -f3 "$ROOT/.kobo/version")"
    if [ -d "$ROOT/.adds/nm" ]; then echo "Menu dir:  .adds/nm present ($(ls "$ROOT/.adds/nm" | tr '\n' ' '))"
    else echo "Menu dir:  .adds/nm MISSING"; fi
    if [ -f "$PENDING" ]; then echo "Pending:   KoboRoot.tgz waiting (installs on next reboot)"
    else echo "Pending:   none"; fi
    echo "Note:      the NickelMenu hook is on the system partition and cannot be"
    echo "           checked over USB. If Home lacks the menu, run: nickelmenu.sh install"
    exit 0
fi

# install
USER_TGZ=1
if [ -z "$TGZ" ]; then
    USER_TGZ=0
    CACHE="${XDG_CACHE_HOME:-$HOME/.cache}/gideon"
    TGZ="$CACHE/NickelMenu-$NM_VERSION-KoboRoot.tgz"
    if [ ! -f "$TGZ" ]; then
        [ "$DRY" -eq 1 ] && { echo "would download $NM_URL"; echo "would verify sha256 $NM_SHA256"; echo "would write $PENDING"; exit 0; }
        mkdir -p "$CACHE"
        command -v curl >/dev/null 2>&1 || die "curl not found; pass --tgz FILE"
        curl --proto =https --tlsv1.2 -fsSL -o "$TGZ.part" "$NM_URL" || { rm -f "$TGZ.part"; die "download failed"; }
        mv "$TGZ.part" "$TGZ"
    fi
fi
[ -f "$TGZ" ] || die "no such file: $TGZ"
GOT=$(sha256_of "$TGZ")
if [ "$GOT" != "$NM_SHA256" ]; then
    # Only ever delete our own cache file, never a file the user passed in.
    [ "$USER_TGZ" -eq 1 ] || rm -f "$TGZ" 2>/dev/null || true
    die "checksum mismatch for NickelMenu $NM_VERSION (got $GOT); refusing to install"
fi

if [ -f "$PENDING" ] && [ "$FORCE" -ne 1 ]; then
    if [ "$(sha256_of "$PENDING")" = "$NM_SHA256" ]; then echo "NickelMenu is already staged; eject and reboot the Kobo."; exit 0; fi
    die "a different KoboRoot.tgz is pending (maybe a firmware update); not overwriting. Re-run with --force to replace it."
fi

if [ "$DRY" -eq 1 ]; then echo "would write $PENDING (verified $NM_VERSION)"; exit 0; fi
cp "$TGZ" "$PENDING.part" || { rm -f "$PENDING.part"; die "copy to the Kobo failed"; }
sync
if [ "$(sha256_of "$PENDING.part")" != "$NM_SHA256" ]; then
    rm -f "$PENDING.part"
    die "copy on the Kobo does not match the checksum; nothing staged"
fi
mv "$PENDING.part" "$PENDING"
sync
echo "Staged NickelMenu $NM_VERSION. Eject the Kobo; it installs on the next boot."

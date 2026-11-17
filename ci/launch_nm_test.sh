#!/usr/bin/env bash
# Tests for the NickelMenu protection in installer/gideon-launch.sh, on a
# fake Kobo root: the failsafe heal, and the KoboRoot.tgz rescue that keeps
# NickelMenu installed if the device goes down while nickel's failsafe has
# its library parked.
set -euo pipefail
LAUNCH="$(cd "$(dirname "$0")/.." && pwd)/installer/gideon-launch.sh"
W=$(mktemp -d); trap 'rm -rf "$W"' EXIT
fail() { echo "FAIL: $1" >&2; exit 1; }

R="$W/root"
LIB_DIR="$R/usr/local/Kobo/imageformats"
mkdir -p "$LIB_DIR" "$R/mnt/onboard/.kobo" "$R/mnt/onboard/.adds/gideon"
export GIDEON_NM_LIB="$LIB_DIR/libnm.so"
export GIDEON_NM_RESCUE="$R/mnt/onboard/.kobo/KoboRoot.tgz"
export GIDEON_NM_RESCUE_MARK="$R/mnt/onboard/.adds/gideon/nm-rescue.md5"
export GIDEON_LAUNCH_LOG="$R/mnt/onboard/.adds/gideon/launch.log"
export GIDEON_LAUNCH_FUNCTIONS_ONLY=1
# shellcheck source=/dev/null
. "$LAUNCH"

reset() { rm -rf "${LIB_DIR:?}"/* "$GIDEON_NM_RESCUE" "$GIDEON_NM_RESCUE_MARK"; echo "nickelmenu v0.6" > "$GIDEON_NM_LIB"; }
# The firmware's boot-time install: unpack KoboRoot.tgz over / and delete it.
boot_unpacks_koboroot() {
    [ -e "$GIDEON_NM_RESCUE" ] || return 0
    tar xzf "$GIDEON_NM_RESCUE" -C /
    rm -f "$GIDEON_NM_RESCUE"
}

echo "==> a reboot inside nickel's failsafe window no longer loses NickelMenu"
reset
nm_stage_rescue
[ -e "$GIDEON_NM_RESCUE" ] || fail "nothing staged"
mv "$GIDEON_NM_LIB" "$GIDEON_NM_LIB.failsafe"   # nickel's failsafe parks it…
# …and the device goes down before the 3 s disarm: nothing in the launcher
# survives that. The next boot unpacks the rescue before nickel starts.
boot_unpacks_koboroot
[ -e "$GIDEON_NM_LIB" ] || fail "NickelMenu lost across the reboot"
grep -q "nickelmenu v0.6" "$GIDEON_NM_LIB" || fail "wrong library restored"

echo "==> a safe window takes the rescue back, so a later boot installs nothing"
reset
nm_stage_rescue
nm_clear_rescue
[ ! -e "$GIDEON_NM_RESCUE" ] || fail "rescue left behind"
[ ! -e "$GIDEON_NM_RESCUE_MARK" ] || fail "marker left behind"

echo "==> someone else's KoboRoot.tgz (a firmware or NickelMenu update) is never touched"
reset
echo "firmware update" > "$GIDEON_NM_RESCUE"
nm_stage_rescue
grep -q "firmware update" "$GIDEON_NM_RESCUE" || fail "overwrote a pending update"
nm_clear_rescue
grep -q "firmware update" "$GIDEON_NM_RESCUE" || fail "deleted a pending update"

echo "==> our own stale rescue is refreshed, not kept"
reset
nm_stage_rescue
echo "nickelmenu v0.7" > "$GIDEON_NM_LIB"
nm_stage_rescue
rm -f "$GIDEON_NM_LIB"
boot_unpacks_koboroot
grep -q "nickelmenu v0.7" "$GIDEON_NM_LIB" || fail "staged an old library"

echo "==> no NickelMenu installed: nothing staged"
reset
rm -f "$GIDEON_NM_LIB"
nm_stage_rescue
[ ! -e "$GIDEON_NM_RESCUE" ] || fail "staged a rescue with nothing to rescue"

echo "==> a parked library is put back (nickel died inside the window)"
reset
mv "$GIDEON_NM_LIB" "$GIDEON_NM_LIB.failsafe"
nm_failsafe_heal
[ -e "$GIDEON_NM_LIB" ] && [ ! -e "$GIDEON_NM_LIB.failsafe" ] || fail "not healed"
nm_failsafe_heal   # twice is harmless
[ -e "$GIDEON_NM_LIB" ] || fail "second heal broke it"

echo "==> the rescue holds one file, like NickelHook's own KoboRoot.tgz"
reset
chmod 0600 "$GIDEON_NM_LIB"
nm_stage_rescue
entries=$(tar tzvf "$GIDEON_NM_RESCUE")
[ "$(echo "$entries" | wc -l)" -eq 1 ] || fail "extra entries: $entries"
echo "$entries" | grep -q '^-rw-r--r-- .*libnm.so$' || fail "not a 0644 file: $entries"

echo "==> a leftover .failsafe beside a reinstalled library is cleared"
reset
echo stale > "$GIDEON_NM_LIB.failsafe"
nm_failsafe_heal
[ ! -e "$GIDEON_NM_LIB.failsafe" ] || fail "stale .failsafe kept"
grep -q "nickelmenu v0.6" "$GIDEON_NM_LIB" || fail "heal touched the live library"

echo "==> a freshly started nickel that hasn't parked the library yet keeps the window open"
reset
# A "nickel" started a moment ago. (pidof is called from the sourced
# launcher functions, which shellcheck cannot see.)
sleep 30 &
young=$!
# shellcheck disable=SC2329
pidof() { [ "$1" = -s ] && shift; [ "$1" = nickel ] && echo "$young"; }
nm_window_open || fail "window closed before nickel loaded NickelMenu (rescue would be cleared too early)"
mv "$GIDEON_NM_LIB" "$GIDEON_NM_LIB.failsafe"
nm_window_open || fail "parked library not seen as an open window"
mv "$GIDEON_NM_LIB.failsafe" "$GIDEON_NM_LIB"
# shellcheck disable=SC2329
pidof() { return 1; }
nm_window_open || fail "nickel gone must still count as needing the rescue"
unset -f pidof
kill "$young" 2>/dev/null || true

echo "==> each step is logged"
reset
nm_log exit
grep -q "exit: libnm=present parked=no rescue=none" "$GIDEON_LAUNCH_LOG" || fail "log line: $(cat "$GIDEON_LAUNCH_LOG")"

echo "OK"

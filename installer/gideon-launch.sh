#!/bin/sh
# Launch the gideon browse UI full-screen on a Kobo device (from NickelMenu).
#
# Nickel (the stock Kobo UI) owns the framebuffer and the touch screen, so it
# must be stopped before gideon can draw. When gideon exits we restart nickel
# IN PLACE (the same dance KOReader's nickel.sh does) instead of rebooting:
# a reboot reliably brought nickel back, but cost the user a full boot every
# time they left the app. The reboot is kept as a fallback for when nickel
# refuses to come up.
set -u

# --- NickelMenu protection ------------------------------------------------
# Verified against the source (NickelHook nh.c + NickelMenu nickelmenu.cc):
#   * nh_init is a shared-library constructor, so it runs when nickel's Qt
#     plugin loader dlopens /usr/local/Kobo/imageformats/libnm.so during
#     startup (install path: NickelHook.mk, KOBOROOT rule).
#   * nh_failsafe_create renames libnm.so -> libnm.so.failsafe ("parks" it).
#   * At the END of nh_init — on the success AND the error path — a detached
#     thread is scheduled that sleeps failsafe_delay (NickelMenu: 3 s) and
#     renames the library back.
#   * If nickel dies before that thread fires, the library stays parked and
#     NickelMenu is simply absent on the next boot ("uninstalled itself").
#     Nothing but that thread — or us — ever restores it.
#   * The one case nothing in this script can heal: the DEVICE goes down
#     inside that window (the reboot on exit with a Bluetooth device
#     connected, koreader/koreader#12739, lands exactly there, while nickel
#     brings its stack up). For that, the library is staged as a KoboRoot.tgz
#     before nickel restarts — the firmware unpacks it early in the next
#     boot, before nickel, which is precisely how NickelMenu is installed in
#     the first place — and the staged copy is removed once the window has
#     passed safely. A KoboRoot.tgz that is not ours (a firmware or
#     NickelMenu update waiting to install) is never touched.
# Paths are overridable only so ci/launch_nm_test.sh can run these on a
# fake root; on the device they are always the defaults.
NM_LIB=${GIDEON_NM_LIB:-/usr/local/Kobo/imageformats/libnm.so}
NM_RESCUE=${GIDEON_NM_RESCUE:-/mnt/onboard/.kobo/KoboRoot.tgz}
NM_RESCUE_MARK=${GIDEON_NM_RESCUE_MARK:-/mnt/onboard/.adds/gideon/nm-rescue.md5}
LAUNCH_LOG=${GIDEON_LAUNCH_LOG:-/mnt/onboard/.adds/gideon/launch.log}

# One line of evidence per step, so a NickelMenu that vanished leaves a
# trail: what the library looked like at each point. Kept small.
nm_log() {
    if [ -f "$LAUNCH_LOG" ] && [ "$(wc -c <"$LAUNCH_LOG" 2>/dev/null || echo 0)" -gt 65536 ]; then
        tail -n 200 "$LAUNCH_LOG" >"$LAUNCH_LOG.tmp" 2>/dev/null && mv "$LAUNCH_LOG.tmp" "$LAUNCH_LOG"
    fi
    lib=absent
    [ -e "$NM_LIB" ] && lib=present
    parked=no
    [ -e "$NM_LIB.failsafe" ] && parked=yes
    rescue=none
    [ -e "$NM_RESCUE" ] && rescue=staged
    echo "$(date '+%Y-%m-%d %H:%M:%S' 2>/dev/null) $1: libnm=$lib parked=$parked rescue=$rescue" \
        >>"$LAUNCH_LOG" 2>/dev/null
}

# Restore a library the failsafe left parked (nickel died inside the
# window, so NickelMenu never got to rename it back). Without this,
# NickelMenu has silently uninstalled itself come the next boot.
nm_failsafe_heal() {
    if [ -e "$NM_LIB.failsafe" ] && [ ! -e "$NM_LIB" ]; then
        mv "$NM_LIB.failsafe" "$NM_LIB" 2>/dev/null
        sync
    fi
}

# Whether the staged KoboRoot.tgz is the one we wrote.
nm_rescue_is_ours() {
    [ -e "$NM_RESCUE" ] && [ -e "$NM_RESCUE_MARK" ] &&
        [ "$(md5sum <"$NM_RESCUE" | cut -d' ' -f1)" = "$(cat "$NM_RESCUE_MARK")" ]
}

# Stage NickelMenu's library to be reinstalled by the next boot, in case
# the device goes down while nickel's failsafe has it parked. Does nothing
# when NickelMenu is not installed, or when someone else's KoboRoot.tgz is
# already waiting.
nm_stage_rescue() {
    [ -e "$NM_LIB" ] || return 0
    if [ -e "$NM_RESCUE" ] && ! nm_rescue_is_ours; then
        return 0
    fi
    work=$(mktemp -d 2>/dev/null) || return 0
    lib_dir=$(dirname "$NM_LIB")
    mkdir -p "$work/${lib_dir#/}" &&
        cp "$NM_LIB" "$work/${lib_dir#/}/" &&
        mkdir -p "$(dirname "$NM_RESCUE")" "$(dirname "$NM_RESCUE_MARK")" &&
        (cd "$work" && tar czf "$NM_RESCUE.part" "${lib_dir#/}") &&
        mv "$NM_RESCUE.part" "$NM_RESCUE" &&
        md5sum <"$NM_RESCUE" | cut -d' ' -f1 >"$NM_RESCUE_MARK"
    rm -rf "$work" "$NM_RESCUE.part" 2>/dev/null
    sync
}

# The window passed: take back our staged copy (never anyone else's).
nm_clear_rescue() {
    if nm_rescue_is_ours; then
        rm -f "$NM_RESCUE"
    fi
    rm -f "$NM_RESCUE_MARK"
    sync
}

# Sourced by the test suite for the functions above only.
if [ "${GIDEON_LAUNCH_FUNCTIONS_ONLY:-}" = 1 ]; then
    # shellcheck disable=SC2317 # return when sourced, exit when run
    return 0 2>/dev/null || exit 0
fi

GIDEON_DATA_DIR=/mnt/onboard/.adds/gideon/data
export GIDEON_DATA_DIR

# Flush pending writes before we take over (KOReader does the same).
sync

# The per-device touch profile keys off the Kobo PRODUCT codename; if
# the environment didn't carry it, re-derive it the way KOReader does.
if [ -z "${PRODUCT:-}" ]; then
    PRODUCT="$(/bin/kobo_config.sh 2>/dev/null)"
    export PRODUCT
fi

# Re-derive the env nickel needs BEFORE stopping it, from the same sources
# the boot scripts (rcS) and KOReader's koreader.sh use. Launched from
# NickelMenu we normally inherit all of this from nickel itself; the
# fallbacks cover launchers with a scrubbed environment.
#
# PLATFORM picks the relaunch behavior (udevadm trigger) below.
if [ -z "${PLATFORM:-}" ]; then
    # shellcheck disable=SC2046 # word-splitting the VAR=value is the point
    export $(grep -s -e '^PLATFORM=' "/proc/$(pidof -s udevd)/environ" 2>/dev/null)
fi
if [ -z "${PLATFORM:-}" ]; then
    PLATFORM="freescale"
    if dd if="/dev/mmcblk0" bs=512 skip=1024 count=1 2>/dev/null | grep -q "HW CONFIG"; then
        CPU="$(ntx_hwconfig -s -p /dev/mmcblk0 CPU 2>/dev/null)"
        PLATFORM="${CPU}-ntx"
    fi
    if [ "${PLATFORM}" != "freescale" ] && [ ! -e "/etc/u-boot/${PLATFORM}/u-boot.mmc" ]; then
        PLATFORM="ntx508"
    fi
    export PLATFORM
fi
# INTERFACE: nickel's Wi-Fi handling expects it (eth0 is what rcS hardcoded
# for years; the fallback matches KOReader's).
if [ -z "${INTERFACE:-}" ]; then
    INTERFACE="eth0"
    export INTERFACE
fi

# Restart nickel in place, ported from KOReader's platform/kobo/nickel.sh:
# recreate the hardware-status FIFO, hand the sdcard back, then relaunch
# the stock stack. We deliberately do NOT tear down Wi-Fi here: KOReader
# unloads the module with per-chipset power-off dances that are riskier
# than letting nickel reconcile the interface state itself.
restart_nickel() {
    export LD_LIBRARY_PATH="/usr/local/Kobo"
    # Qt audio sinks, exported by rcS on FW 4.28+ (harmless earlier).
    export QT_GSTREAMER_PLAYBIN_AUDIOSINK=alsasink
    export QT_GSTREAMER_PLAYBIN_AUDIOSINK_DEVICE_PARAMETER=bluealsa:DEV=00:00:00:00:00:00
    cd / || return 1

    # Recreate Nickel's FIFO ourselves, like rcS does: udev *will* write
    # to it, and nickel must process what lands there.
    rm -f /tmp/nickel-hardware-status
    mkfifo /tmp/nickel-hardware-status

    sync

    # Hand the sdcard back: unmount it ourselves or nickel shows an
    # "unrecognized FS" popup; the udevadm trigger below enqueues the add
    # event that makes nickel re-detect it (no-op on slotless devices).
    if [ -e "/dev/mmcblk1p1" ]; then
        umount /mnt/sd 2>/dev/null
    fi

    # Relaunch the stock stack exactly like the reference implementation,
    # KOReader's platform/kobo/nickel.sh: hindenburg + nickel + udevadm
    # trigger, and NOTHING else. In particular do NOT relaunch sickel (the
    # FW watchdog): KOReader kills it on entry (koreader.sh) but never
    # restarts it, and that recipe is what ships to every KOReader user on
    # this hardware. Relaunching it ourselves was an unsourced deviation —
    # and a watchdog restarted outside init is a plausible culler of the
    # freshly started nickel (which would strand NickelMenu's failsafe).
    /usr/local/Kobo/hindenburg &
    LIBC_FATAL_STDERR_=1 /usr/local/Kobo/nickel -platform kobo -skipFontLoad &
    [ "${PLATFORM}" != "freescale" ] && udevadm trigger &

    return 0
}

# --- NickelMenu failsafe guard -------------------------------------------
# Never kill nickel while its failsafe has the library parked (see the
# NickelMenu protection notes at the top), and never walk away from a nickel
# we started while it is still parked.
nm_failsafe_armed() {
    # The parked library is the authoritative marker when present…
    [ -e "$NM_LIB.failsafe" ] && return 0
    # …and nickel's process age covers NickelMenu versions whose failsafe
    # works differently: field 22 of /proc/<pid>/stat is the start time in
    # clock ticks (100 Hz on these kernels).
    pid=$(pidof -s nickel) || return 1
    age=$(awk -v up="$(cut -d' ' -f1 /proc/uptime)" \
        '{print int(up - $22 / 100)}' "/proc/$pid/stat" 2>/dev/null)
    [ -n "$age" ] && [ "$age" -lt 25 ]
}

i=0
while nm_failsafe_armed; do
    i=$((i + 1))
    [ "$i" -ge 100 ] && break # ~25 s upper bound; never hang the launch
    usleep 250000 2>/dev/null || sleep 1
done
# Still parked once the window is long past: nickel's disarm never came.
# NickelMenu is still loaded in this nickel (its code stays mapped), so put
# the file back now, while there is a chance — and drop any rescue copy a
# previous exit left behind (the window it guarded is over).
nm_failsafe_heal
nm_clear_rescue
nm_log launch

# Stop nickel and its watchdog/helper daemons so the screen is ours, and
# wait for nickel to actually exit (up to ~4s) instead of guessing — both
# processes fighting over the framebuffer stomps gideon's first paint.
killall -TERM nickel hindenburg sickel fickel 2>/dev/null
i=0
while pkill -0 nickel 2>/dev/null; do
    i=$((i + 1))
    [ "$i" -ge 16 ] && break
    usleep 250000 2>/dev/null || sleep 1
done

# Remove Nickel's hardware-status FIFO: with nickel gone, udev/udhcpc
# scripts can hang forever on open() against it (KOReader's koreader.sh
# does exactly this).
rm -f /tmp/nickel-hardware-status

# gideon turns the panel upright for itself and puts it back when it
# closes; remember Nickel's rotation here too, so a gideon that died
# without cleaning up (a crash, a kill) still hands Nickel a framebuffer it
# can draw on. Nickel restarted in place on a rotated panel comes up with
# every menu sideways or torn — a reboot used to hide this.
FB_ROTATE=/sys/class/graphics/fb0/rotate
NICKEL_ROTATE=$(cat "$FB_ROTATE" 2>/dev/null)

/mnt/onboard/.adds/gideon/bin/gideon browse --library /mnt/onboard/Manga \
    >>/mnt/onboard/.adds/gideon/browse.log 2>&1

if [ -n "$NICKEL_ROTATE" ] && [ "$(cat "$FB_ROTATE" 2>/dev/null)" != "$NICKEL_ROTATE" ]; then
    echo "$NICKEL_ROTATE" >"$FB_ROTATE" 2>/dev/null
fi

# Bluetooth is deliberately NOT touched here. The rfkill soft-block that
# briefly lived at this spot (PR #121, for the exit-with-BT-connected
# reboot in koreader/koreader#12739 / issue #120) ran on EVERY exit and
# made things worse in the field — constant crashes on exit, reported on
# the Libra Colour 2 — so radio manipulation is off the table. The
# platform reboot risk when a BT device is connected at exit is upstream
# (it reproduces in KOReader too, without any rfkill writes); NickelMenu
# survival is instead protected by the failsafe guard/heal/babysit in
# this script, which are plain file checks and renames.

# Recover the stock UI in place; flush writes first. If the failsafe
# tripped anyway (a race, or an older gideon killed nickel inside the
# window), put NickelMenu's library back before nickel comes up.
sync
nm_failsafe_heal
# From here until nickel's failsafe disarms, a reboot would strand
# NickelMenu: stage its library for the next boot to reinstall.
nm_stage_rescue
nm_log exit
restart_nickel

# Fallback: if nickel didn't appear within ~10s, reboot — that reliably
# brings the stock UI back, exactly like the old behavior.
i=0
while ! pidof nickel >/dev/null 2>&1; do
    i=$((i + 1))
    if [ "$i" -ge 40 ]; then
        # Rebooting can also catch a half-started nickel inside its
        # failsafe window — restore the library first.
        nm_failsafe_heal
        sync
        sleep 1
        reboot
        exit 0
    fi
    usleep 250000 2>/dev/null || sleep 1
done

# Nickel is up — but NickelMenu's failsafe is armed again: the library was
# parked when nickel's plugin loader pulled libnm.so in, and only the
# disarm thread (fires 3 s after nh_init returns — see the guard comment
# above for the sourced details) puts it back. If nickel dies before that,
# the library stays parked and NickelMenu has "uninstalled itself" come
# the next boot, with nobody left to notice. So don't exit yet: babysit
# the window until the failsafe disarms, and restore the library ourselves
# if nickel dies first (or the disarm never comes within ~30 s).
i=0
while [ -e "$NM_LIB.failsafe" ]; do
    i=$((i + 1))
    if [ "$i" -ge 120 ]; then
        # Init hung with the failsafe still armed: keep NickelMenu alive
        # (a double restore is harmless — NM's own rename just no-ops).
        nm_failsafe_heal
        nm_log heal-after-hang
        break
    fi
    if ! pidof nickel >/dev/null 2>&1; then
        # Nickel died inside its window: restore the library, then reboot —
        # a clean boot reliably brings the stock UI back with NickelMenu
        # intact.
        nm_failsafe_heal
        nm_log nickel-died-in-window
        sync
        sleep 1
        reboot
        exit 0
    fi
    usleep 250000 2>/dev/null || sleep 1
done

# The window has passed with NickelMenu back in place: the rescue copy is
# no longer needed, and must not reinstall anything on a later boot.
nm_clear_rescue
nm_log disarmed
exit 0

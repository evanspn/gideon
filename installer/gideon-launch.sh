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

    # Relaunch the stock stack: hindenburg, sickel (the watchdog newer
    # firmwares — FW5 / Libra Colour — ship) and nickel itself.
    /usr/local/Kobo/hindenburg &
    if [ -x /usr/local/Kobo/sickel ]; then
        /usr/local/Kobo/sickel &
    fi
    LIBC_FATAL_STDERR_=1 /usr/local/Kobo/nickel -platform kobo -skipFontLoad &
    [ "${PLATFORM}" != "freescale" ] && udevadm trigger &

    return 0
}

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

/mnt/onboard/.adds/gideon/bin/gideon browse --library /mnt/onboard/Manga \
    >>/mnt/onboard/.adds/gideon/browse.log 2>&1

# Recover the stock UI in place; flush writes first.
sync
restart_nickel

# Fallback: if nickel didn't appear within ~10s, reboot — that reliably
# brings the stock UI back, exactly like the old behavior.
i=0
while ! pidof nickel >/dev/null 2>&1; do
    i=$((i + 1))
    if [ "$i" -ge 40 ]; then
        sync
        sleep 1
        reboot
        exit 0
    fi
    usleep 250000 2>/dev/null || sleep 1
done

exit 0

#!/bin/sh
# Launch the gideon browse UI full-screen on a Kobo device (from NickelMenu).
#
# Nickel (the stock Kobo UI) owns the framebuffer and the touch screen, so it
# must be stopped before gideon can draw. Restarting nickel in-place is
# fragile (it needs a carefully reconstructed environment), so for now we
# reboot the device when gideon exits — that reliably brings nickel back.
# A soft nickel restart is a future improvement.
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

# Recover the stock UI: flush writes, then reboot back into nickel.
sync
sleep 1
reboot

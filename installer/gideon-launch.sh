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

# Stop nickel and its watchdog/helper daemons so the screen is ours.
killall -TERM nickel hindenburg sickel fickel 2>/dev/null
sleep 1

/mnt/onboard/.adds/gideon/bin/gideon browse --library /mnt/onboard/Manga \
    >>/mnt/onboard/.adds/gideon/browse.log 2>&1

# Recover the stock UI: flush writes, then reboot back into nickel.
sync
sleep 1
reboot

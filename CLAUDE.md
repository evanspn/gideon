# gideon — agent notes

## Bluetooth

gideon does NOT touch the Bluetooth radio at the kernel level — no rfkill, no
bluetoothctl, no hci*, no sysfs writes. Pairing is Nickel's job; gideon reads
already-connected devices, and manages BT power ONLY around suspend, ONLY via
the stack's own D-Bus interface (`com.kobo.mtk.bluedroid`,
`crates/gideon-device/src/bluetooth.rs`) — the mechanism the field-tested
kobo.koplugin uses. Power down before suspend, power up + `Device1.Connect`
paired devices after wake (`GIDEON_SUSPEND_BT=0` opts out). The MTK BT stack
needs the shared Wi-Fi radio up to start, so the wake path ties the two
restores together. On exit, `bluetooth::restore_for_exit` (main.rs + the
panic hook) powers the stack back up if a suspend left it down, so Nickel
gets the radios back in the state it left them — a state handover via the
same D-Bus interface, still never rfkill/sysfs.

Crashes: panics and fatal errors draw a smiley crash screen and append
details to `/mnt/onboard/.adds/gideon/crash.log` (`install_panic_screen`
/ `show_fatal_on_display` in gideon-app/src/main.rs).

How it works:
- A paired BT page-turn remote shows up as a Linux evdev node. Device discovery
  and key mapping live in `crates/gideon-device/src/kobo_input.rs`
  (`remote_key_to_page` maps standard HID keys to page turns; KEY_POWER stays a
  sleep key).
- The set of open remote nodes drives the Home screen Bluetooth indicator
  (`crates/gideon-app/src/ui/mod.rs`).
- Page direction from the remote is fixed across screen rotation.

Hard rule: never add radio manipulation (especially not on the exit path in
`installer/gideon-launch.sh`). PR #121 tried an rfkill soft-block on exit and it
crashed the Kobo on every exit in the field; reverted in PR #122. The
exit-with-BT-connected reboot is an upstream platform issue
(koreader/koreader#12739) and is accepted, not worked around.

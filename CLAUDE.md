# gideon — agent notes

## Bluetooth

gideon does NOT manage the Bluetooth radio at all — no rfkill, no bluetoothctl,
no hci*, no sysfs writes. Pairing and radio power are Nickel's job; gideon only
*reads* already-connected devices.

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

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
restores together.

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

## E-ink refresh

Which refresh a repaint asks for is a user-visible design decision, not an
implementation detail. **Read `docs/REFRESH.md` before changing any
`render_current` / `flush` call.**

The short version: the panel has non-flashing partial waveforms for BOTH
grayscale and colour (GLR16 / GLRC16), and callers select
`RefreshMode::{Full, Partial}` while `kobo.rs` picks the waveform from
`last_blit_color`.

- **Partial** when a bounded region changes and the rest of the frame is
  byte-identical to what the panel already shows — a value, a row, a page
  turn, a sheet sliding over content that is not moving.
- **Full** when a whole screen changes, or when a region the panel has been
  holding stale behind something opaque is revealed again.

Hard rules: never flash for a one-line change; never flash to reveal a modal;
any surface that repaints partially in a loop must force a Full every N
repaints so ghosting has somewhere to go; and pin the mode in a test —
`MemoryDisplay.flushes` records every flush, and this codebase has already
shipped a comment claiming "this does not flash" that was false.

REAGL waveforms are paired with `UPDATE_MODE_FULL` on purpose. That flag is
about the update region, not the flash. "Fixing" that pairing breaks partial
refresh everywhere.

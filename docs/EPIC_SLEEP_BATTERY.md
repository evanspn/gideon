# Epic: intelligent sleep & battery life

The goal: closing the cover (or walking away) always ends with the device
asleep, and nothing burns battery while nobody is reading. The device owes
the user zero babysitting — a sleep request is a promise, not an attempt.

Findings come from a code audit of `gideon-device/src/power.rs`,
`gideon-device/src/kobo_input.rs` and the sleep/wake arms of
`gideon-app/src/ui/mod.rs` (see `docs/KOREADER_LESSONS.md` §Power for the
reference behavior).

## Shipped in this epic

1. **Cover closed while charging no longer strands the device awake.**
   `suspend()` must refuse while plugged in (an MTK suspend with the
   charger in hangs the kernel), but the refusal used to be final: a device
   closed in its cover, unplugged later and tossed in a bag stayed awake
   until the battery died. Now the UI waits out the charger (5 s probes)
   and finishes the nap the moment the cable is pulled; any tap or button
   aborts the wait because the user is clearly using the device. Both the
   menu and reader paths share the same `sleep_once_unplugged` helper.

2. **Idle auto-suspend (15 minutes), menus and reader.** There was no
   inactivity timeout at all — the main loops blocked on input forever, CPU
   scheduled and Wi-Fi fully up. Nickel and KOReader both auto-suspend;
   now gideon does too: 15 idle minutes behave exactly like a cover close
   (the reader saves progress first through the same Sleep path). Only
   active with a suspend hook installed, so tests/dev runs are unaffected.

3. **A cover close during "Connecting to Wi-Fi…" now sleeps.** The
   cancellable connect waits treated *any* input as "cancel" and dropped
   it — a cover close mid-connect was swallowed and the device stayed
   awake. Sleep is now recognized there and honored after the wait exits.

4. **A failed suspend restores Wi-Fi even with auto-connect off.**
   `suspend()` takes the radio fully down before trying; on a hard failure
   the radio was only restored when `wifi_auto_connect` was on. The user
   turned off auto-connect, not the radio.

5. **Unknown battery drivers can't fake "unplugged".** The charging probe
   knew two supply names (`battery`, `bd71827_bat`); any other board name
   read as "not plugged in" — the exact charger-in kernel-hang case. A
   fallback scan now finds any supply of `type: Battery`.

## Backlog (next iterations)

- **Low-battery handling**: warn at a threshold, auto-shutdown at critical
  (KOREADER_LESSONS §Power TODO). Battery percent is already probed for the
  Home title; the missing piece is a check on wake/idle ticks.
- **Frontlight off before suspend**: needs one hardware check whether
  `gSleep_Mode_Suspend` already cuts the lm3630a under a closed cover
  (KOREADER_LESSONS §Power TODO). `reapply()` on wake already exists.
- **Deduplicate the sleep paths**: `sleep_now()` and the reader Sleep arm
  are parallel implementations (the reader saves progress and skips the
  status screen). They already diverged once; fold into one helper the way
  `sleep_once_unplugged` was.
- **Pre-download vs. suspend**: a suspend tears Wi-Fi out from under an
  in-flight pre-download; the job fails silently and is not retried after
  wake. Either drain the worker before suspending or re-queue on wake.
- **Wake latency**: the input-node reopen can cost ~2.5 s of blocking
  sleeps on the UI thread (6 × 500 ms probes on MTK); could poll faster
  with the same ceiling.

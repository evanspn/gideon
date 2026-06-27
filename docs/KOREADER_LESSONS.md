# KOReader lessons: the Kobo device audit

KOReader carries a decade of accumulated Kobo device knowledge. This is the
systematic comparison of what their Kobo support does against what gideon
does, focused on the critical path: **launch → UI draws → browse → download
→ read**. Sources: `koreader/koreader-base` (`ffi/framebuffer_mxcfb.lua`,
`ffi-cdecl/include/mxcfb-kobo.h`, `mtk-kobo.h`) and `koreader/koreader`
(`platform/kobo/koreader.sh`, `frontend/device/kobo/device.lua`).

Status legend: ✅ implemented · 📋 documented/deferred · ⚠️ known risk.

## Display

| KOReader behavior | gideon status |
| --- | --- |
| Per-generation refresh ioctl: mxcfb v1 (pre-Mark 7), v2 (Mark 7+), HWTCON (MTK: Libra Colour, Clara BW/Colour, Elipsa 2E) | ✅ probes MTK → v2 → v1 on first refresh, caches the winner; ioctl numbers test-pinned to the device ABI struct sizes |
| Maps the fb with the driver-advertised memory size (char devices have file length 0) | ✅ exact `smem_len` mapping, validated to cover the screen |
| Never msyncs the fb mapping (device memory; EINVAL) | ✅ removed |
| Adapts to the panel depth; normalizes to 8bpp via `fbdepth` on NXP, keeps 32bpp on MTK/colour | ✅ converts grayscale to the reported depth (8/16/24/32bpp) instead of switching |
| Normalizes fb rotation to upright at startup (`fbdepth -R UR`); touch tables assume that orientation. "Upright" is a per-device **native** rotate value (FBInk's rotationMap): monza/condor → 1, spa\* → 3 — NOT 0 | ✅ best-effort `FBIOPUT_VSCREENINFO` to the per-`PRODUCT` upright value (monza/condor=1, spa\*=3, else 0; `GIDEON_FB_ROTATE` overrides), re-reads geometry, adapts if refused |
| Waveforms: GC16 flashes; partial = AUTO/REAGL per gen; MTK partial = GLR16 (REAGL always available) | ✅ GC16 full everywhere; MTK partials GLR16, mxcfb partials AUTO |
| MTK: wait for update *submission* after every send; completion-wait after every REAGL/Kaleido send (their partials are promoted to UPDATE_MODE_FULL, which triggers their wait) | ✅ submission-wait on every send; completion fenced LAZILY — the previous partial's marker is waited on at the top of the next flush (≈0ms in normal reading, serializes rapid taps), flashes wait immediately. Same ordering guarantee as KOReader without their ~500ms synchronous per-turn cost |
| MTK driver off-by-one: refresh regions must be expanded/aligned to 16px | 📋 gideon only refreshes the full screen today (1264 and 1680 are 16-aligned); enforce alignment when partial-rect refreshes are added |
| Night mode inversion, A2/DU fast waveforms, hardware dithering, NXP EPDC power knob | 📋 quality features, not blockers |
| Kaleido colour: CFA handling for color content | ✅ color manga pages render in color end-to-end: `page_is_color` samples each decoded page (every 8th pixel, channel spread > 20, > 1% colorful) and `render_page` keeps detected pages RGB through the resize (`PageBuf::Rgb`, no software dithering — the hardware Y8→Y4 dither handles it); the reader caches/prefetches either depth and dispatches to `blit_rgb`, where `last_blit_color` picks KOReader's Kaleido waveforms (GCC16 fulls, GLRC16 partials, CFA flag 0x600 \| dither 1, dither_mode 0x102). B/W pages stay byte-identical on the gray fast path (parity-pinned), so ordinary manga pays nothing. The library shelf's covers already used the same blit |

## Touch input

| KOReader behavior | gideon status |
| --- | --- |
| Per-device mirroring from the device table (`PRODUCT` codename); Libra Colour (`monza`) is `touch_mirrored_y` | ✅ codename-based transform defaults (monza → SwapXYMirrorX in our raw-axis convention — KOReader mirrors after the swap; Clara BW/Colour mapped); `GIDEON_TOUCH_TRANSFORM` overrides |
| `pressure_event = ABS_MT_PRESSURE` on monza: contact tracked via pressure | ✅ zero pressure registers a release |
| "snow" protocol quirks on some models (Clara BW) | 📋 tracker handles MT tracking-id + BTN_TOUCH + pressure; snow-specific quirks unverified on hardware |
| Multi-touch gestures (pinch etc.) | 📋 taps only for now |

## Launcher / system integration (koreader.sh)

| KOReader behavior | gideon status |
| --- | --- |
| `sync` before takeover | ✅ |
| Kills the full Nickel stack | ✅ nickel + hindenburg + sickel + fickel (we deliberately leave `dhcpcd` alive so the wifi lease survives — KOReader kills it because it manages wifi itself) |
| Removes `/tmp/nickel-hardware-status` FIFO (udev/udhcpc scripts hang on open() once nickel is gone) | ✅ |
| Restarts Nickel in place on exit (carefully reconstructed env) | 📋 we reboot — reliable, slower; soft restart is roadmap |
| Wifi: full management (module load, wpa_supplicant, udhcpc) since they kill dhcpcd | ⚠️ we rely on Nickel's wifi staying configured after the kill (interface, route and resolv.conf persist; dhcpcd keeps the lease). If downloads fail in-app with network errors, this is the first suspect — the fix is porting their enable-wifi/obtain-ip scripts |
| Frontlight control (sysfs per device) | ✅ `gideon-device/src/light.rs`, paths from KOReader's monza table: brightness = percent straight into `mxc_msp430.0/brightness`; warmth = `lm3630a_led/color` on the inverted native 0–10 scale (`nl_inverted`: 0 is warm). Reader edge slides drive it (right edge = brightness, left = night light), levels persist in settings.json and are restored at startup. The before-suspend frontlight-off check is folded into the Power row |
| Power: suspend/standby management, low-battery handling | ✅ power button and magnetic sleep cover suspend to RAM (`gideon-device/src/power.rs`), following KOReader's `Kobo:_doSuspend`/`resume`: skip while plugged in using their polarity (anything but exactly `Discharging` blocks — `Not charging` is the trap; an MTK suspend with the charger in hangs the kernel), Wi-Fi down first (KOReader "murders" it; wpa_supplicant/dhcpcd stay alive so link-up on wake reassociates — `GIDEON_SUSPEND_WIFI=0` opts out), `1 > /sys/power/state-extended`, 2 s settle, `sync`, `mem > /sys/power/state` (blocks until wake), then `0 > state-extended` + 100 ms + Wi-Fi up; `EBUSY` retries reset state-extended like Nickel's own 1→mem→0 loop. Buttons come from a capability scan of `/dev/input/event0..9` for `EV_KEY` 116 (power) / 59 / 35 (covers), `poll(2)`-merged with the touch stream; dead nodes are dropped instead of busy-looped on. Reading progress is flushed to disk *before* suspending; wake repaints in full, drops the wake key press, re-snaps the orientation in auto mode (the gsensor reports only on *change*, so reopening the input nodes preserves the last observed pose and a resync applies it — otherwise the screen stays at the pre-sleep angle until physically moved), and debounces sleep for 1 s so a late-delivered wake press can't re-suspend (their #12325). A cover closed mid-download survives the post-download input drain. Still 📋: low-battery auto-shutdown; frontlight off before suspend (KOReader ramps it down explicitly — needs one hardware check whether `gSleep_Mode_Suspend` already cuts the lm3630a under the closed cover) |
| USB plug events, sdcard, charging LED | 📋 not handled; plugging USB mid-session is untested |

## Why the v0.1.x crashes happened (the short history)

1. `mmap` used the file length — `/dev/fb0` is a char device, length 0
2. The fb depth was asserted to be 8bpp — stock Nickel leaves 16/32bpp
3. `msync` on the mapping — EINVAL on device memory
4. The refresh ioctl was mxcfb-v1-only — Libra Colour is HWTCON (MTK)

Every one of these is something KOReader's code already handled; the audit
above exists so the rest get adopted deliberately instead of discovered as
crashes.

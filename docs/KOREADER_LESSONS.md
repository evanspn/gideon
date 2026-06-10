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
| MTK: wait for update *submission* after every send; wait for *completion* around flashes | ✅ both, best-effort |
| MTK driver off-by-one: refresh regions must be expanded/aligned to 16px | 📋 gideon only refreshes the full screen today (1264 and 1680 are 16-aligned); enforce alignment when partial-rect refreshes are added |
| Night mode inversion, A2/DU fast waveforms, hardware dithering, NXP EPDC power knob | 📋 quality features, not blockers |
| Kaleido colour: CFA handling for color content | 📋 we render grayscale, which the colour panel displays fine; colour rendering is future work |

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
| Frontlight control (sysfs per device) | 📋 brightness stays at the level Nickel left; in-app control is roadmap |
| Power: suspend/standby management, low-battery handling | 📋 nothing suspends while gideon runs (Nickel's the one that sleeps the device); battery drains at active-use rate during sessions |
| USB plug events, sdcard, charging LED | 📋 not handled; plugging USB mid-session is untested |

## Why the v0.1.x crashes happened (the short history)

1. `mmap` used the file length — `/dev/fb0` is a char device, length 0
2. The fb depth was asserted to be 8bpp — stock Nickel leaves 16/32bpp
3. `msync` on the mapping — EINVAL on device memory
4. The refresh ioctl was mxcfb-v1-only — Libra Colour is HWTCON (MTK)

Every one of these is something KOReader's code already handled; the audit
above exists so the rest get adopted deliberately instead of discovered as
crashes.

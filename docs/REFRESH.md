# E-ink refresh policy

The panel is slow and it flashes. Which refresh a repaint asks for is a
**user-visible design decision**, not an implementation detail — get it wrong
and a good layout still feels cheap. This file records what the hardware
offers and the rules gideon follows, so the policy survives future changes.

## What the hardware gives us

The Libra Colour (MTK/HWTCON, `monza`) has four waveforms, all wired in
`crates/gideon-device/src/kobo.rs`:

| Waveform | Constant | Use | Flashes? |
| --- | --- | --- | --- |
| GC16 | `WAVEFORM_MODE_GC16` = 2 | grayscale full | **yes** |
| GLR16 (REAGL) | `HWTCON_WAVEFORM_MODE_GLR16` = 4 | grayscale partial | no |
| GCC16 | `HWTCON_WAVEFORM_MODE_GCC16` = 10 | Kaleido colour full | **yes** |
| GLRC16 | `HWTCON_WAVEFORM_MODE_GLRC16` = 11 | Kaleido colour partial | no |

Two things about this are easy to get wrong:

1. **There is a non-flashing COLOUR partial.** GLRC16 is not a compromise or a
   grayscale fallback — colour content can be repainted without a flash. Do not
   assume colour implies a full refresh.
2. **REAGL waveforms must be paired with `UPDATE_MODE_FULL`.** That flag is
   about the update *region*, not the flash. GLR16 and GLRC16 do not flash
   despite it. Anyone reading the ioctl code and "fixing" this pairing will
   break partial refresh across the whole app.

Callers never pick a waveform. They pick `RefreshMode::{Full, Partial}`, and
`kobo.rs` resolves it against `last_blit_color` — so a colour frame
automatically gets GCC16/GLRC16 and a grayscale one GC16/GLR16.

## The rule

> **Partial** when a bounded region changes and everything else in the frame is
> byte-identical to what the panel is already showing.
>
> **Full** when a whole screen changes, or when a region the panel has been
> holding stale is revealed again.

The second half of the Full case is the subtle one. A partial refresh asks the
panel to reason from the image it believes it is showing. When something opaque
has been covering a region, that belief is stale, and reconstructing it with
REAGL is exactly where residue shows.

## What that means in practice

| Interaction | Mode | Why |
| --- | --- | --- |
| Reader page turn | Partial | one page replaces another; flashes every `reader_full_refresh_interval` turns (default 8, clamped 4–24, cycled through `FULL_REFRESH_STEPS = [6, 8, 12, 16]`) to discharge ghosting |
| Screen push / pop | Full | the whole content area changes |
| Switching top-level destination (nav bar) | Full | same reason — it is a screen change |
| Opening a modal sheet | **Partial** | only the strip the sheet covers changes; nothing above it moves |
| Cycling a value inside a sheet | **Partial** | repaints one tile, or one line |
| Replacing one sheet with another (book → delete confirmation) | **Partial** | the same strip is redrawn; what is above it never moved |
| Closing a modal sheet | Full | restores a region held stale behind an opaque panel |
| Settings page flip | Full | the whole content area is replaced, like any other list page (the grid fits on one page at the sizes gideon runs on, so this is the small-panel path) |
| Keyboard keypress | Partial | one character; forced Full every `KEYBOARD_FULL_REFRESH_INTERVAL` = 8 repaints so typing does not accumulate ghosting |
| Library view toggle (shelf ⇄ list) | Full | the entire content area is replaced |
| Status overlay ("Downloading… page 3/20") | Partial | a transient line |

## Rules that are not negotiable

- **Never flash for a one-line change.** If a repaint changes a value, a label,
  a progress bar or a single row, it is Partial.
- **Never flash to reveal a modal.** Sheets slide over content that is not
  changing.
- **Always give ghosting somewhere to go.** Any surface that repaints partially
  in a loop (the reader, the keyboard) must force a Full every N repaints. A
  new surface with that shape needs its own interval, not an exemption.
- **Assert the mode, do not describe it.** `MemoryDisplay` records every flush
  in `flushes`, so refresh policy is testable. New interactions get a test that
  pins their mode — see `quick_settings_opens_and_cycles_without_flashing` in
  `crates/gideon-app/src/ui/tests.rs`. A comment claiming "this does not flash"
  is worth nothing; the codebase has already shipped one that was false.

## History

This file exists because the first version of the quick-settings sheet asked
for a Full refresh on open and on every value cycle — flashing the whole panel
to slide a sheet over the bottom third, and flashing again to repaint one line
— while its own commit message claimed nothing beneath the sheet repainted.
The hardware had supported the right thing the whole time.

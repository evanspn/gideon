//! Touch input abstraction.
//!
//! [`InputSource`] is the blocking event stream the UI consumes. Backends:
//!
//! * [`FakeInput`] — scripted events for tests.
//! * `KoboTouch` (feature `kobo`, Linux) — evdev touch screen reader, see
//!   [`crate::kobo_input`].
//!
//! [`TouchTransform`] maps raw touch-panel coordinates to screen
//! coordinates: most Kobo panels are mounted rotated/mirrored relative to
//! the framebuffer.

use std::str::FromStr;

/// A user-interface event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UiEvent {
    /// A finger tapped the screen at (x, y) in screen coordinates.
    Tap { x: u32, y: u32 },
    /// Physical page-forward button (Kobo Libra family: code 194, RPgFwd).
    PageForward,
    /// Physical page-back button (Kobo Libra family: code 193, RPgBack).
    PageBack,
    /// A drag from (x0, y0) to (x1, y1) in screen coordinates — emitted on
    /// finger release when the travel exceeds the tap slop. Edge slides
    /// adjust the frontlight in the reader.
    Swipe { x0: u32, y0: u32, x1: u32, y1: u32 },
    /// A press held in place (≥600 ms without travel) — context actions,
    /// e.g. a library book's chapter list.
    LongPress { x: u32, y: u32 },
    /// The user asked the device to sleep: power button pressed or the
    /// magnetic sleep cover closed (KOReader: KEY_POWER=116 press, sleep
    /// cover codes 59/35 press). Waking is *not* an event: the suspend call
    /// blocks until wakeup, and the key that woke the device is consumed by
    /// the input drain that follows it.
    Sleep,
    /// The accelerometer reported a new physical orientation: an *absolute*
    /// reading rotation in degrees clockwise (0/90/180/270). Emitted by the
    /// Kobo gyro (KOReader's `EV_MSC`/`MSC_RAW` gsensor codes). The UI only
    /// acts on it when the orientation is unlocked ("auto"); a locked
    /// orientation ignores it. Face-up / face-down report no rotation.
    Rotate { rotation: u32 },
}

/// A blocking source of UI events.
pub trait InputSource {
    /// Block until the next event arrives.
    fn next_event(&mut self) -> crate::Result<UiEvent>;

    /// Drop any events that queued up while the UI was busy (e.g. taps made
    /// during a long download) so they don't fire stale actions. Default:
    /// no-op — test inputs replay their script unaffected.
    fn discard_queued(&mut self) {}

    /// Like [`Self::discard_queued`], but sleep requests survive the drain:
    /// taps made during a long download are stale, a sleep cover closed
    /// during it is not — the device must still go to sleep afterwards.
    /// Default: same as `discard_queued`.
    fn discard_taps(&mut self) {
        self.discard_queued();
    }

    /// Re-open the underlying devices after a suspend/resume cycle:
    /// kernels can re-register input nodes across suspend, leaving old
    /// fds dead. Default: no-op (fakes have nothing to reopen).
    fn refresh_devices(&mut self) {}
}

/// Test input source: replays a fixed list of events, then errors.
pub struct FakeInput {
    events: std::vec::IntoIter<UiEvent>,
    /// How often `refresh_devices` ran (the post-wake reopen), for tests.
    pub refreshes: usize,
}

impl FakeInput {
    pub fn new(events: Vec<UiEvent>) -> Self {
        Self {
            events: events.into_iter(),
            refreshes: 0,
        }
    }
}

impl InputSource for FakeInput {
    fn next_event(&mut self) -> crate::Result<UiEvent> {
        self.events
            .next()
            .ok_or_else(|| crate::Error::Display("fake input exhausted".to_string()))
    }

    fn refresh_devices(&mut self) {
        self.refreshes += 1;
    }
}

/// How raw touch-panel coordinates map onto the screen.
///
/// Mirrors apply to the *raw* axes first, then `SwapXY` exchanges them. The
/// common Kobo mounting is [`TouchTransform::SwapXYMirrorX`]:
/// `screen_x = raw_y`, `screen_y = max_x - raw_x`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TouchTransform {
    Identity,
    SwapXY,
    MirrorX,
    MirrorY,
    SwapXYMirrorX,
    /// KOReader's base Kobo mapping (touch_mirrored_x after swap):
    /// screen_x = max - raw_y, screen_y = raw_x.
    #[default]
    SwapXYMirrorY,
    MirrorBoth,
    SwapXYMirrorBoth,
}

/// Scale `v` from `0..=max` to `0..=out - 1`.
fn scale(v: u32, max: u32, out: u32) -> u32 {
    if max == 0 || out == 0 {
        return 0;
    }
    ((v.min(max) as u64 * (out as u64 - 1) + (max as u64 / 2)) / max as u64) as u32
}

impl TouchTransform {
    /// Map a raw panel coordinate to screen coordinates. `max_x`/`max_y`
    /// are the raw axis maxima reported by the device; the result is scaled
    /// to `screen_w`/`screen_h`.
    pub fn apply(
        self,
        raw_x: u32,
        raw_y: u32,
        max_x: u32,
        max_y: u32,
        screen_w: u32,
        screen_h: u32,
    ) -> (u32, u32) {
        use TouchTransform::*;
        let (mx, my) = match self {
            Identity | SwapXY => (raw_x, raw_y),
            MirrorX | SwapXYMirrorX => (max_x.saturating_sub(raw_x.min(max_x)), raw_y),
            MirrorY | SwapXYMirrorY => (raw_x, max_y.saturating_sub(raw_y.min(max_y))),
            MirrorBoth | SwapXYMirrorBoth => (
                max_x.saturating_sub(raw_x.min(max_x)),
                max_y.saturating_sub(raw_y.min(max_y)),
            ),
        };
        if self.swaps() {
            // Raw Y becomes screen X; raw X becomes screen Y.
            (scale(my, max_y, screen_w), scale(mx, max_x, screen_h))
        } else {
            (scale(mx, max_x, screen_w), scale(my, max_y, screen_h))
        }
    }

    fn swaps(self) -> bool {
        matches!(
            self,
            TouchTransform::SwapXY
                | TouchTransform::SwapXYMirrorX
                | TouchTransform::SwapXYMirrorY
                | TouchTransform::SwapXYMirrorBoth
        )
    }

    /// Decompose into the (swap, mirror_x, mirror_y) algebra: mirrors apply
    /// to the raw axes first, then `swap` exchanges them (see [`Self::apply`]).
    fn decompose(self) -> (bool, bool, bool) {
        use TouchTransform::*;
        match self {
            Identity => (false, false, false),
            SwapXY => (true, false, false),
            MirrorX => (false, true, false),
            MirrorY => (false, false, true),
            SwapXYMirrorX => (true, true, false),
            SwapXYMirrorY => (true, false, true),
            MirrorBoth => (false, true, true),
            SwapXYMirrorBoth => (true, true, true),
        }
    }

    fn compose(swap: bool, mirror_x: bool, mirror_y: bool) -> Self {
        use TouchTransform::*;
        match (swap, mirror_x, mirror_y) {
            (false, false, false) => Identity,
            (true, false, false) => SwapXY,
            (false, true, false) => MirrorX,
            (false, false, true) => MirrorY,
            (true, true, false) => SwapXYMirrorX,
            (true, false, true) => SwapXYMirrorY,
            (false, true, true) => MirrorBoth,
            (true, true, true) => SwapXYMirrorBoth,
        }
    }

    /// Compose `quarter_turns` extra *clockwise* quarter-turn rotations onto
    /// this transform: the result maps raw panel coordinates to a screen
    /// whose content is rotated `quarter_turns × 90°` CW relative to what
    /// `self` targets (e.g. when the kernel refused the upright rotation
    /// and the framebuffer settled elsewhere).
    ///
    /// A CW quarter turn sends screen `(x, y)` to `(max_y - y, x)`; in the
    /// (swap, mirror) algebra that is "mirror the new screen-x source, then
    /// swap": a swapped transform un-swaps with mirror_x flipped, an
    /// un-swapped one swaps with mirror_y flipped.
    pub fn rotated_quarter_turns(self, quarter_turns: u32) -> TouchTransform {
        let (mut swap, mut mirror_x, mut mirror_y) = self.decompose();
        for _ in 0..(quarter_turns % 4) {
            if swap {
                swap = false;
                mirror_x = !mirror_x;
            } else {
                swap = true;
                mirror_y = !mirror_y;
            }
        }
        Self::compose(swap, mirror_x, mirror_y)
    }

    /// Read the transform from `GIDEON_TOUCH_TRANSFORM`, falling back to
    /// the per-device default for the Kobo `PRODUCT` codename (set by the
    /// stock system and inherited by our launcher), then the generic
    /// default.
    pub fn from_env() -> Self {
        if let Some(transform) = std::env::var("GIDEON_TOUCH_TRANSFORM")
            .ok()
            .and_then(|raw| raw.parse().ok())
        {
            return transform;
        }
        Self::default_for_product(std::env::var("PRODUCT").ok().as_deref())
    }

    /// Per-device defaults, taken from KOReader's Kobo device table
    /// (`touch_mirrored_x/y` per codename; Kobo panels report swapped axes).
    pub fn default_for_product(product: Option<&str>) -> Self {
        match product.map(|p| p.trim().to_ascii_lowercase()).as_deref() {
            // KOReader mirrors AFTER the axis swap; our variants mirror the
            // raw axes BEFORE swapping. Their monza mapping (mirrored_y):
            // screen_x = raw_y, screen_y = max - raw_x == our SwapXYMirrorX.
            Some("monza") | Some("monzakobo") | Some("monzatolino") => {
                TouchTransform::SwapXYMirrorX
            }
            // Clara BW / Clara Colour and the rest of the spa* family
            // (incl. Tolino variants): KOReader's base mapping (mirrored_x):
            // screen_x = max - raw_y == our SwapXYMirrorY.
            Some(p) if p.starts_with("spa") => TouchTransform::SwapXYMirrorY,
            _ => TouchTransform::default(),
        }
    }
}

impl FromStr for TouchTransform {
    type Err = String;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        let normalized: String = raw
            .chars()
            .filter(|c| c.is_ascii_alphanumeric())
            .collect::<String>()
            .to_ascii_lowercase();
        match normalized.as_str() {
            "identity" => Ok(Self::Identity),
            "swapxy" => Ok(Self::SwapXY),
            "mirrorx" => Ok(Self::MirrorX),
            "mirrory" => Ok(Self::MirrorY),
            "swapxymirrorx" => Ok(Self::SwapXYMirrorX),
            "swapxymirrory" => Ok(Self::SwapXYMirrorY),
            "mirrorboth" => Ok(Self::MirrorBoth),
            "swapxymirrorboth" => Ok(Self::SwapXYMirrorBoth),
            other => Err(format!("unknown touch transform '{other}'")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fake_input_replays_then_errors() {
        let mut input = FakeInput::new(vec![
            UiEvent::Tap { x: 1, y: 2 },
            UiEvent::Tap { x: 3, y: 4 },
        ]);
        assert_eq!(input.next_event().unwrap(), UiEvent::Tap { x: 1, y: 2 });
        assert_eq!(input.next_event().unwrap(), UiEvent::Tap { x: 3, y: 4 });
        assert!(input.next_event().is_err());
    }

    // Raw panel: 0..=1000 both axes. Screen: 101x201 so scaled coordinates
    // are exact (raw/10 and raw/5).
    const MAX: u32 = 1000;
    const W: u32 = 101;
    const H: u32 = 201;

    fn apply(t: TouchTransform, x: u32, y: u32) -> (u32, u32) {
        t.apply(x, y, MAX, MAX, W, H)
    }

    #[test]
    fn identity_scales_axes() {
        assert_eq!(apply(TouchTransform::Identity, 0, 0), (0, 0));
        assert_eq!(apply(TouchTransform::Identity, 1000, 1000), (100, 200));
        assert_eq!(apply(TouchTransform::Identity, 500, 250), (50, 50));
    }

    #[test]
    fn swap_xy_exchanges_axes() {
        // raw (1000, 0): raw_y → screen x, raw_x → screen y.
        assert_eq!(apply(TouchTransform::SwapXY, 1000, 0), (0, 200));
        assert_eq!(apply(TouchTransform::SwapXY, 250, 500), (50, 50));
    }

    #[test]
    fn mirror_x_flips_horizontal() {
        assert_eq!(apply(TouchTransform::MirrorX, 0, 0), (100, 0));
        assert_eq!(apply(TouchTransform::MirrorX, 1000, 1000), (0, 200));
        assert_eq!(apply(TouchTransform::MirrorX, 250, 500), (75, 100));
    }

    #[test]
    fn mirror_y_flips_vertical() {
        assert_eq!(apply(TouchTransform::MirrorY, 0, 0), (0, 200));
        assert_eq!(apply(TouchTransform::MirrorY, 1000, 1000), (100, 0));
        assert_eq!(apply(TouchTransform::MirrorY, 500, 250), (50, 150));
    }

    #[test]
    fn mirror_both_flips_both() {
        assert_eq!(apply(TouchTransform::MirrorBoth, 0, 0), (100, 200));
        assert_eq!(apply(TouchTransform::MirrorBoth, 1000, 1000), (0, 0));
        assert_eq!(apply(TouchTransform::MirrorBoth, 250, 250), (75, 150));
    }

    #[test]
    fn swap_xy_mirror_x_matches_kobo_mounting() {
        // screen_x = raw_y, screen_y = max_x - raw_x.
        assert_eq!(apply(TouchTransform::SwapXYMirrorX, 0, 0), (0, 200));
        assert_eq!(apply(TouchTransform::SwapXYMirrorX, 1000, 1000), (100, 0));
        assert_eq!(apply(TouchTransform::SwapXYMirrorX, 250, 500), (50, 150));
    }

    #[test]
    fn swap_xy_mirror_y() {
        // screen_x = max_y - raw_y, screen_y = raw_x.
        assert_eq!(apply(TouchTransform::SwapXYMirrorY, 0, 0), (100, 0));
        assert_eq!(apply(TouchTransform::SwapXYMirrorY, 1000, 1000), (0, 200));
        assert_eq!(apply(TouchTransform::SwapXYMirrorY, 500, 250), (75, 100));
    }

    #[test]
    fn swap_xy_mirror_both() {
        // screen_x = max_y - raw_y, screen_y = max_x - raw_x.
        assert_eq!(apply(TouchTransform::SwapXYMirrorBoth, 0, 0), (100, 200));
        assert_eq!(apply(TouchTransform::SwapXYMirrorBoth, 1000, 1000), (0, 0));
        assert_eq!(apply(TouchTransform::SwapXYMirrorBoth, 250, 500), (50, 150));
    }

    #[test]
    fn degenerate_ranges_do_not_panic() {
        let t = TouchTransform::Identity;
        assert_eq!(t.apply(5, 5, 0, 0, 100, 100), (0, 0));
        assert_eq!(t.apply(5000, 5000, 100, 100, 100, 100), (99, 99));
        assert_eq!(t.apply(5, 5, 100, 100, 0, 0), (0, 0));
    }

    #[test]
    fn parses_from_str() {
        assert_eq!(
            "identity".parse::<TouchTransform>().unwrap(),
            TouchTransform::Identity
        );
        assert_eq!(
            "swap_xy_mirror_x".parse::<TouchTransform>().unwrap(),
            TouchTransform::SwapXYMirrorX
        );
        assert_eq!(
            "SwapXYMirrorBoth".parse::<TouchTransform>().unwrap(),
            TouchTransform::SwapXYMirrorBoth
        );
        assert_eq!(
            "mirror-both".parse::<TouchTransform>().unwrap(),
            TouchTransform::MirrorBoth
        );
        assert!("bogus".parse::<TouchTransform>().is_err());
        assert_eq!(TouchTransform::default(), TouchTransform::SwapXYMirrorY);
    }

    #[test]
    fn product_codename_selects_device_transform() {
        assert_eq!(
            TouchTransform::default_for_product(Some("monza")),
            TouchTransform::SwapXYMirrorX
        );
        assert_eq!(
            TouchTransform::default_for_product(Some(" MonzaKobo ")),
            TouchTransform::SwapXYMirrorX
        );
        // Any spa* codename (Clara family, incl. Tolino variants).
        assert_eq!(
            TouchTransform::default_for_product(Some("spaBW")),
            TouchTransform::SwapXYMirrorY
        );
        assert_eq!(
            TouchTransform::default_for_product(Some("spaColour")),
            TouchTransform::SwapXYMirrorY
        );
        assert_eq!(
            TouchTransform::default_for_product(Some(" spaTolinoBW ")),
            TouchTransform::SwapXYMirrorY
        );
        assert_eq!(
            TouchTransform::default_for_product(Some("frost")),
            TouchTransform::default()
        );
        assert_eq!(
            TouchTransform::default_for_product(None),
            TouchTransform::default()
        );
    }

    /// Rotate a screen point one quarter turn clockwise on a square screen
    /// of side `side`: (x, y) -> (side - 1 - y, x).
    fn rotate_point_cw(p: (u32, u32), side: u32) -> (u32, u32) {
        (side - 1 - p.1, p.0)
    }

    // Square geometry so screen dimensions survive rotation unchanged and
    // the scaled mapping is exact (raw 0..=1000 onto 101 px = raw/10).
    const SQ: u32 = 101;

    fn apply_sq(t: TouchTransform, x: u32, y: u32) -> (u32, u32) {
        t.apply(x, y, MAX, MAX, SQ, SQ)
    }

    #[test]
    fn rotated_quarter_turns_composes_with_apply() {
        // For every transform, every delta and a few sample points:
        // rotated transform == rotate the output of the original.
        let all = [
            TouchTransform::Identity,
            TouchTransform::SwapXY,
            TouchTransform::MirrorX,
            TouchTransform::MirrorY,
            TouchTransform::SwapXYMirrorX,
            TouchTransform::SwapXYMirrorY,
            TouchTransform::MirrorBoth,
            TouchTransform::SwapXYMirrorBoth,
        ];
        let samples = [(0, 0), (1000, 0), (0, 1000), (250, 500), (730, 90)];
        for t in all {
            for delta in 0..4u32 {
                for (rx, ry) in samples {
                    let mut expected = apply_sq(t, rx, ry);
                    for _ in 0..delta {
                        expected = rotate_point_cw(expected, SQ);
                    }
                    assert_eq!(
                        apply_sq(t.rotated_quarter_turns(delta), rx, ry),
                        expected,
                        "{t:?} rotated by {delta} quarter turns at raw ({rx}, {ry})"
                    );
                }
            }
        }
    }

    #[test]
    fn delta_zero_is_identity_composition() {
        assert_eq!(
            TouchTransform::SwapXYMirrorX.rotated_quarter_turns(0),
            TouchTransform::SwapXYMirrorX
        );
        // raw (250, 500) on monza: screen (50, 75) on the square screen.
        assert_eq!(apply_sq(TouchTransform::SwapXYMirrorX, 250, 500), (50, 75));
    }

    #[test]
    fn delta_one_maps_one_quarter_turn_clockwise() {
        // Identity + 1 CW turn: (x, y) -> (max_y - y, x) == SwapXYMirrorY.
        assert_eq!(
            TouchTransform::Identity.rotated_quarter_turns(1),
            TouchTransform::SwapXYMirrorY
        );
        let t = TouchTransform::Identity.rotated_quarter_turns(1);
        assert_eq!(apply_sq(t, 0, 0), (100, 0));
        assert_eq!(apply_sq(t, 1000, 0), (100, 100));
        assert_eq!(apply_sq(t, 250, 500), (50, 25));
    }

    #[test]
    fn delta_two_maps_a_half_turn() {
        // Identity + 180°: (x, y) -> (max - x, max - y) == MirrorBoth.
        assert_eq!(
            TouchTransform::Identity.rotated_quarter_turns(2),
            TouchTransform::MirrorBoth
        );
        let t = TouchTransform::Identity.rotated_quarter_turns(2);
        assert_eq!(apply_sq(t, 0, 0), (100, 100));
        assert_eq!(apply_sq(t, 1000, 0), (0, 100));
        assert_eq!(apply_sq(t, 250, 500), (75, 50));
    }

    #[test]
    fn delta_three_maps_one_quarter_turn_counterclockwise() {
        // Identity + 270° CW (= 90° CCW): (x, y) -> (y, max_x - x).
        assert_eq!(
            TouchTransform::Identity.rotated_quarter_turns(3),
            TouchTransform::SwapXYMirrorX
        );
        let t = TouchTransform::Identity.rotated_quarter_turns(3);
        assert_eq!(apply_sq(t, 0, 0), (0, 100));
        assert_eq!(apply_sq(t, 1000, 0), (0, 0));
        assert_eq!(apply_sq(t, 250, 500), (50, 75));
    }

    #[test]
    fn four_quarter_turns_round_trip() {
        for t in [
            TouchTransform::Identity,
            TouchTransform::SwapXYMirrorX,
            TouchTransform::MirrorBoth,
        ] {
            assert_eq!(t.rotated_quarter_turns(4), t);
            assert_eq!(
                t.rotated_quarter_turns(1).rotated_quarter_turns(3),
                t,
                "1 + 3 quarter turns must cancel for {t:?}"
            );
        }
    }
}

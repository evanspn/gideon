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
}

/// A blocking source of UI events.
pub trait InputSource {
    /// Block until the next event arrives.
    fn next_event(&mut self) -> crate::Result<UiEvent>;
}

/// Test input source: replays a fixed list of events, then errors.
pub struct FakeInput {
    events: std::vec::IntoIter<UiEvent>,
}

impl FakeInput {
    pub fn new(events: Vec<UiEvent>) -> Self {
        Self {
            events: events.into_iter(),
        }
    }
}

impl InputSource for FakeInput {
    fn next_event(&mut self) -> crate::Result<UiEvent> {
        self.events
            .next()
            .ok_or_else(|| crate::Error::Display("fake input exhausted".to_string()))
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
            // Clara BW / Clara Colour: KOReader's base mapping (mirrored_x):
            // screen_x = max - raw_y == our SwapXYMirrorY.
            Some("spabw") | Some("spacolour") | Some("spacolor") => TouchTransform::SwapXYMirrorY,
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
        assert_eq!(
            TouchTransform::default_for_product(Some("spaBW")),
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
}

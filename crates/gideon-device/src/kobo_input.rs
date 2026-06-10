//! Kobo touch screen input (Linux evdev).
//!
//! Kobo devices expose the touch panel as `/dev/input/eventN`. We pick the
//! first device that advertises an absolute X axis (multitouch
//! `ABS_MT_POSITION_X` or single-touch `ABS_X`), read its axis ranges, and
//! turn the raw event stream into [`UiEvent::Tap`]s: track the latest
//! position and emit on finger release. Raw panel coordinates are mapped to
//! screen coordinates with a [`TouchTransform`] (configurable through
//! `GIDEON_TOUCH_TRANSFORM`; the default fits most Kobo models).
//!
//! Only compiled with the `kobo` feature on Linux. The event-stream state
//! machine ([`TouchTracker`]) is pure and unit-tested with synthetic
//! `libc::input_event` values.

#![cfg(target_os = "linux")]

use std::fs::File;
use std::io::Read;
use std::os::fd::AsRawFd;

use crate::input::{InputSource, TouchTransform, UiEvent};
use crate::{Error, Result};

// --- evdev ABI (from linux/input.h, linux/input-event-codes.h) ---

const EV_SYN: u16 = 0x00;
const EV_KEY: u16 = 0x01;
const EV_ABS: u16 = 0x03;

const SYN_REPORT: u16 = 0x00;

const ABS_X: u16 = 0x00;
const ABS_Y: u16 = 0x01;
const ABS_MT_POSITION_X: u16 = 0x35;
const ABS_MT_POSITION_Y: u16 = 0x36;
const ABS_MT_TRACKING_ID: u16 = 0x39;

const BTN_TOUCH: u16 = 0x14a;

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct input_absinfo {
    value: i32,
    minimum: i32,
    maximum: i32,
    fuzz: i32,
    flat: i32,
    resolution: i32,
}

/// `EVIOCGBIT(EV_ABS, len)`: read the absolute-axis capability bitmask.
/// _IOC(_IOC_READ, 'E', 0x20 + EV_ABS, len)
fn eviocgbit_abs(len: usize) -> u32 {
    (2u32 << 30) | ((len as u32) << 16) | (b'E' as u32) << 8 | (0x20 + EV_ABS as u32)
}

/// `EVIOCGABS(abs)`: read an axis's `input_absinfo`.
fn eviocgabs(abs: u16) -> u32 {
    (2u32 << 30)
        | ((std::mem::size_of::<input_absinfo>() as u32) << 16)
        | (b'E' as u32) << 8
        | (0x40 + abs as u32)
}

/// See `crate::kobo::ioctl` — same request-type portability shim.
///
/// # Safety
/// Same contract as `libc::ioctl`: `arg` must match what `request` expects.
unsafe fn ioctl<T>(fd: libc::c_int, request: u32, arg: *mut T) -> libc::c_int {
    libc::ioctl(fd, request as _, arg)
}

fn abs_bit_set(bits: &[u8], code: u16) -> bool {
    let byte = (code / 8) as usize;
    byte < bits.len() && bits[byte] & (1 << (code % 8)) != 0
}

/// Pure state machine: feed raw `input_event`s, get a raw-coordinate tap on
/// finger release. Tracks both the multitouch protocol (type B:
/// `ABS_MT_POSITION_*` + `ABS_MT_TRACKING_ID`) and the legacy single-touch
/// one (`ABS_X/Y` + `BTN_TOUCH`).
#[derive(Debug, Default)]
pub struct TouchTracker {
    last_x: Option<i32>,
    last_y: Option<i32>,
    touching: bool,
    release_seen: bool,
}

impl TouchTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Process one event. Returns the raw `(x, y)` of a completed tap when
    /// the finger lifts.
    pub fn push(&mut self, ev: &libc::input_event) -> Option<(u32, u32)> {
        match (ev.type_, ev.code) {
            (EV_ABS, ABS_MT_POSITION_X) | (EV_ABS, ABS_X) => {
                self.last_x = Some(ev.value);
                self.touching = true;
            }
            (EV_ABS, ABS_MT_POSITION_Y) | (EV_ABS, ABS_Y) => {
                self.last_y = Some(ev.value);
                self.touching = true;
            }
            (EV_ABS, ABS_MT_TRACKING_ID) => {
                if ev.value == -1 {
                    self.release_seen = true;
                } else {
                    self.touching = true;
                }
            }
            (EV_KEY, BTN_TOUCH) => {
                if ev.value == 0 {
                    // Finger lifted: emit immediately.
                    return self.finish_tap();
                }
                self.touching = true;
            }
            (EV_SYN, SYN_REPORT) if self.release_seen => {
                return self.finish_tap();
            }
            _ => {}
        }
        None
    }

    fn finish_tap(&mut self) -> Option<(u32, u32)> {
        self.release_seen = false;
        if !self.touching {
            return None;
        }
        self.touching = false;
        match (self.last_x, self.last_y) {
            (Some(x), Some(y)) => Some((x.max(0) as u32, y.max(0) as u32)),
            _ => None,
        }
    }
}

/// Touch screen input on Kobo hardware.
pub struct KoboTouch {
    file: File,
    tracker: TouchTracker,
    transform: TouchTransform,
    max_x: u32,
    max_y: u32,
    screen_w: u32,
    screen_h: u32,
}

impl KoboTouch {
    /// Scan `/dev/input/event0..event5` for the first device advertising an
    /// absolute X axis, and configure raw-to-screen mapping for a
    /// `screen_w x screen_h` display.
    pub fn open(screen_w: u32, screen_h: u32) -> Result<Self> {
        for n in 0..=5 {
            let path = format!("/dev/input/event{n}");
            let Ok(file) = File::open(&path) else {
                continue;
            };
            let fd = file.as_raw_fd();

            let mut bits = [0u8; 8];
            // SAFETY: EVIOCGBIT with a buffer of the size encoded in the request.
            let ret = unsafe { ioctl(fd, eviocgbit_abs(bits.len()), bits.as_mut_ptr()) };
            if ret < 0 {
                continue;
            }
            let mt = abs_bit_set(&bits, ABS_MT_POSITION_X);
            if !mt && !abs_bit_set(&bits, ABS_X) {
                continue;
            }

            let (x_axis, y_axis) = if mt {
                (ABS_MT_POSITION_X, ABS_MT_POSITION_Y)
            } else {
                (ABS_X, ABS_Y)
            };
            // Some devices advertise ABS bits but reject EVIOCGABS; fall
            // back to screen dimensions rather than aborting, and keep
            // scanning if this device is unusable.
            let (max_x, max_y) = match (read_axis_max(fd, x_axis), read_axis_max(fd, y_axis)) {
                (Ok(x), Ok(y)) => (x, y),
                (x, y) => {
                    eprintln!(
                        "gideon touch: {path}: EVIOCGABS failed (x: {x:?}, y: {y:?}); using screen dims"
                    );
                    (
                        x.unwrap_or(screen_w.max(1) - 1),
                        y.unwrap_or(screen_h.max(1) - 1),
                    )
                }
            };
            eprintln!(
                "gideon touch: using {path} (mt={mt}) max_x={max_x} max_y={max_y} transform={:?}",
                TouchTransform::from_env()
            );

            return Ok(Self {
                file,
                tracker: TouchTracker::new(),
                transform: TouchTransform::from_env(),
                max_x,
                max_y,
                screen_w,
                screen_h,
            });
        }
        Err(Error::Display(
            "no touch screen found on /dev/input/event0..5".to_string(),
        ))
    }
}

fn read_axis_max(fd: libc::c_int, axis: u16) -> Result<u32> {
    let mut info = input_absinfo::default();
    // SAFETY: EVIOCGABS with a properly sized zero-initialized out-struct.
    let ret = unsafe { ioctl(fd, eviocgabs(axis), &mut info) };
    if ret < 0 {
        return Err(Error::Display(format!(
            "EVIOCGABS(axis {axis:#x}) failed: {}",
            std::io::Error::last_os_error()
        )));
    }
    Ok(info.maximum.max(1) as u32)
}

impl InputSource for KoboTouch {
    fn next_event(&mut self) -> Result<UiEvent> {
        loop {
            let mut ev: libc::input_event = unsafe { std::mem::zeroed() };
            let size = std::mem::size_of::<libc::input_event>();
            // SAFETY: input_event is plain-old-data; reading exactly one
            // struct's worth of bytes into it is sound.
            let buf = unsafe { std::slice::from_raw_parts_mut(&mut ev as *mut _ as *mut u8, size) };
            self.file.read_exact(buf)?;

            if let Some((raw_x, raw_y)) = self.tracker.push(&ev) {
                let (x, y) = self.transform.apply(
                    raw_x,
                    raw_y,
                    self.max_x,
                    self.max_y,
                    self.screen_w,
                    self.screen_h,
                );
                return Ok(UiEvent::Tap { x, y });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(type_: u16, code: u16, value: i32) -> libc::input_event {
        let mut e: libc::input_event = unsafe { std::mem::zeroed() };
        e.type_ = type_;
        e.code = code;
        e.value = value;
        e
    }

    #[test]
    fn multitouch_tap_emits_on_tracking_release() {
        let mut t = TouchTracker::new();
        assert_eq!(t.push(&ev(EV_ABS, ABS_MT_TRACKING_ID, 7)), None);
        assert_eq!(t.push(&ev(EV_ABS, ABS_MT_POSITION_X, 320)), None);
        assert_eq!(t.push(&ev(EV_ABS, ABS_MT_POSITION_Y, 540)), None);
        assert_eq!(t.push(&ev(EV_SYN, SYN_REPORT, 0)), None);
        assert_eq!(t.push(&ev(EV_ABS, ABS_MT_TRACKING_ID, -1)), None);
        assert_eq!(t.push(&ev(EV_SYN, SYN_REPORT, 0)), Some((320, 540)));
        // Nothing further without a new touch.
        assert_eq!(t.push(&ev(EV_SYN, SYN_REPORT, 0)), None);
    }

    #[test]
    fn btn_touch_release_emits_immediately() {
        let mut t = TouchTracker::new();
        assert_eq!(t.push(&ev(EV_KEY, BTN_TOUCH, 1)), None);
        assert_eq!(t.push(&ev(EV_ABS, ABS_X, 10)), None);
        assert_eq!(t.push(&ev(EV_ABS, ABS_Y, 20)), None);
        assert_eq!(t.push(&ev(EV_SYN, SYN_REPORT, 0)), None);
        assert_eq!(t.push(&ev(EV_KEY, BTN_TOUCH, 0)), Some((10, 20)));
        // The trailing SYN_REPORT must not double-emit.
        assert_eq!(t.push(&ev(EV_SYN, SYN_REPORT, 0)), None);
    }

    #[test]
    fn drag_reports_last_position() {
        let mut t = TouchTracker::new();
        t.push(&ev(EV_ABS, ABS_MT_POSITION_X, 100));
        t.push(&ev(EV_ABS, ABS_MT_POSITION_Y, 100));
        t.push(&ev(EV_SYN, SYN_REPORT, 0));
        t.push(&ev(EV_ABS, ABS_MT_POSITION_X, 250));
        t.push(&ev(EV_ABS, ABS_MT_POSITION_Y, 300));
        t.push(&ev(EV_SYN, SYN_REPORT, 0));
        t.push(&ev(EV_ABS, ABS_MT_TRACKING_ID, -1));
        assert_eq!(t.push(&ev(EV_SYN, SYN_REPORT, 0)), Some((250, 300)));
    }

    #[test]
    fn release_without_position_is_ignored() {
        let mut t = TouchTracker::new();
        assert_eq!(t.push(&ev(EV_KEY, BTN_TOUCH, 1)), None);
        assert_eq!(t.push(&ev(EV_KEY, BTN_TOUCH, 0)), None);
    }

    #[test]
    fn negative_coordinates_clamp_to_zero() {
        let mut t = TouchTracker::new();
        t.push(&ev(EV_ABS, ABS_MT_POSITION_X, -5));
        t.push(&ev(EV_ABS, ABS_MT_POSITION_Y, 7));
        t.push(&ev(EV_ABS, ABS_MT_TRACKING_ID, -1));
        assert_eq!(t.push(&ev(EV_SYN, SYN_REPORT, 0)), Some((0, 7)));
    }

    #[test]
    fn second_tap_reuses_tracker() {
        let mut t = TouchTracker::new();
        t.push(&ev(EV_ABS, ABS_MT_POSITION_X, 1));
        t.push(&ev(EV_ABS, ABS_MT_POSITION_Y, 2));
        t.push(&ev(EV_ABS, ABS_MT_TRACKING_ID, -1));
        assert_eq!(t.push(&ev(EV_SYN, SYN_REPORT, 0)), Some((1, 2)));

        t.push(&ev(EV_ABS, ABS_MT_TRACKING_ID, 3));
        t.push(&ev(EV_ABS, ABS_MT_POSITION_X, 9));
        t.push(&ev(EV_ABS, ABS_MT_POSITION_Y, 8));
        t.push(&ev(EV_ABS, ABS_MT_TRACKING_ID, -1));
        assert_eq!(t.push(&ev(EV_SYN, SYN_REPORT, 0)), Some((9, 8)));
    }

    #[test]
    fn ioctl_request_constants_match_kernel_encoding() {
        // EVIOCGBIT(EV_ABS, 8) == _IOC(read, 'E', 0x23, 8)
        assert_eq!(eviocgbit_abs(8), 0x8008_4523);
        // EVIOCGABS(ABS_MT_POSITION_X) == _IOC(read, 'E', 0x75, 24)
        assert_eq!(eviocgabs(ABS_MT_POSITION_X), 0x8018_4575);
    }
}

//! Kobo input (Linux evdev): touch screen + power button + sleep cover.
//!
//! Kobo devices expose input as several `/dev/input/eventN` nodes. The
//! touch panel advertises absolute axes (multitouch `ABS_MT_POSITION_X` or
//! single-touch `ABS_X`); the power button and the magnetic sleep cover
//! live on *different* nodes (on NTX boards usually `event0`, on MTK boards
//! like the Libra Colour they may be split across nodes) that advertise
//! `EV_KEY` with `KEY_POWER` (116) and/or the sleep-cover codes (59 =
//! `KEY_F1`, 35 = `KEY_H` on the Elipsa power cover) — the same capability
//! probe FBInk's `fbink_input_scan` uses for KOReader.
//!
//! [`KoboTouch`] opens every matching node, `poll(2)`s across them, and
//! merges the streams into [`UiEvent`]s: taps from the touch tracker
//! (emit raw position on finger release, mapped to screen coordinates with
//! a [`TouchTransform`]), [`UiEvent::Sleep`] from the button tracker (power
//! button press or cover closed).
//!
//! Only compiled with the `kobo` feature on Linux. The event-stream state
//! machines ([`TouchTracker`], [`ButtonTracker`]) are pure and unit-tested
//! with synthetic `libc::input_event` values.

#![cfg(target_os = "linux")]

use std::collections::VecDeque;
use std::fs::File;
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;

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
const ABS_MT_PRESSURE: u16 = 0x3a;

const BTN_TOUCH: u16 = 0x14a;

/// Power button (KOReader's Kobo event_map: `[116] = "Power"`).
const KEY_POWER: u16 = 116;
/// Magnetic sleep cover (KOReader: `[59] = "SleepCover"`; 59 is KEY_F1).
const KEY_SLEEP_COVER: u16 = 59;
/// Elipsa-style power cover (KOReader: `[35] = "SleepCover"`; 35 is KEY_H).
const KEY_POWER_COVER: u16 = 35;
/// Physical page-back button (KOReader: `[193] = "RPgBack"`).
const KEY_PAGE_BACK: u16 = 193;
/// Physical page-forward button (KOReader: `[194] = "RPgFwd"`).
const KEY_PAGE_FWD: u16 = 194;

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

/// `EVIOCGBIT(ev, len)`: read a capability bitmask for event type `ev`.
/// _IOC(_IOC_READ, 'E', 0x20 + ev, len)
fn eviocgbit(ev: u16, len: usize) -> u32 {
    (2u32 << 30) | ((len as u32) << 16) | (b'E' as u32) << 8 | (0x20 + ev as u32)
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

fn bit_set(bits: &[u8], code: u16) -> bool {
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
            (EV_ABS, ABS_MT_PRESSURE) => {
                // Libra Colour-class panels report contact via pressure
                // (KOReader: pressure_event = ABS_MT_PRESSURE); zero
                // pressure means the finger lifted.
                if ev.value == 0 {
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

/// Pure state machine for the power button and the magnetic sleep cover.
///
/// KOReader's Kobo event map: code 116 (`KEY_POWER`) is the power button,
/// codes 59/35 are the sleep cover — value 1 = press / cover closed,
/// value 0 = release / cover opened, value 2 = key repeat. We sleep on
/// power *press* and cover *close*; releases and cover-open are ignored
/// (waking is the kernel's job — the suspend write blocks until then).
#[derive(Debug, Default)]
pub struct ButtonTracker;

impl ButtonTracker {
    pub fn new() -> Self {
        Self
    }

    /// Process one event; returns a [`UiEvent`] when one completes.
    pub fn push(&mut self, ev: &libc::input_event) -> Option<UiEvent> {
        if ev.type_ != EV_KEY || ev.value != 1 {
            return None;
        }
        match ev.code {
            KEY_POWER | KEY_SLEEP_COVER | KEY_POWER_COVER => Some(UiEvent::Sleep),
            KEY_PAGE_FWD => Some(UiEvent::PageForward),
            KEY_PAGE_BACK => Some(UiEvent::PageBack),
            _ => None,
        }
    }
}

/// Merged evdev input on Kobo hardware: the touch panel plus any nodes
/// carrying the power button / sleep cover.
pub struct KoboTouch {
    /// All opened devices, polled together. Index `touch_idx` feeds the
    /// touch tracker; every device feeds the button tracker (touch nodes
    /// never emit the power/cover codes, so this is harmless).
    devices: Vec<File>,
    touch_idx: usize,
    tracker: TouchTracker,
    buttons: ButtonTracker,
    pending: VecDeque<UiEvent>,
    transform: TouchTransform,
    max_x: u32,
    max_y: u32,
    screen_w: u32,
    screen_h: u32,
}

impl KoboTouch {
    /// Scan `/dev/input/event0..event9` for the touch panel (first device
    /// advertising an absolute X axis) and any button devices (`EV_KEY`
    /// with `KEY_POWER` or a sleep-cover code), and configure raw-to-screen
    /// mapping for a `screen_w x screen_h` display, with the transform
    /// taken from the environment (`GIDEON_TOUCH_TRANSFORM` / `PRODUCT`).
    pub fn open(screen_w: u32, screen_h: u32) -> Result<Self> {
        Self::open_with_transform(screen_w, screen_h, TouchTransform::from_env())
    }

    /// Like [`Self::open`], but with an explicit raw-to-screen transform
    /// (e.g. the env default composed with the framebuffer's settled
    /// rotation delta).
    pub fn open_with_transform(
        screen_w: u32,
        screen_h: u32,
        transform: TouchTransform,
    ) -> Result<Self> {
        let scan = scan_devices(screen_w, screen_h, transform)?;
        Ok(Self {
            devices: scan.devices,
            touch_idx: scan.touch_idx,
            tracker: TouchTracker::new(),
            buttons: ButtonTracker::new(),
            pending: VecDeque::new(),
            transform,
            max_x: scan.max_x,
            max_y: scan.max_y,
            screen_w,
            screen_h,
        })
    }

    /// Close and re-scan every input device. MTK kernels (Libra Colour)
    /// can re-register the evdev nodes across a suspend/resume cycle,
    /// leaving our fds dead — without this, the first cover-close sleep
    /// would kill input (and with it the app) on wake. Retries briefly:
    /// the nodes take a moment to come back after resume. Keeps the old
    /// devices when every retry fails (they may still be alive).
    pub fn reopen(&mut self) {
        for attempt in 0..6 {
            if attempt > 0 {
                std::thread::sleep(std::time::Duration::from_millis(500));
            }
            match scan_devices(self.screen_w, self.screen_h, self.transform) {
                Ok(scan) => {
                    self.devices = scan.devices;
                    self.touch_idx = scan.touch_idx;
                    self.max_x = scan.max_x;
                    self.max_y = scan.max_y;
                    self.tracker = TouchTracker::new();
                    self.buttons = ButtonTracker::new();
                    self.pending.clear();
                    return;
                }
                Err(e) => {
                    eprintln!("gideon input: rescan attempt {} failed: {e}", attempt + 1);
                }
            }
        }
        eprintln!("gideon input: rescan failed; keeping the previous devices");
    }

    /// Drain any already-queued evdev events without blocking and reset the
    /// trackers: the key press that woke the device from suspend must not
    /// fire an action in whatever screen comes next.
    pub fn discard_queued(&mut self) {
        for file in &self.devices {
            let _ = drain_events(file.as_raw_fd(), |_| {});
        }
        self.tracker = TouchTracker::new();
        self.buttons = ButtonTracker::new();
        self.pending.clear();
    }
}

/// What a device scan found.
struct Scan {
    devices: Vec<File>,
    touch_idx: usize,
    max_x: u32,
    max_y: u32,
}

/// Scan `/dev/input/event0..event9` for the touch panel and button devices
/// (see [`KoboTouch::open_with_transform`]).
fn scan_devices(screen_w: u32, screen_h: u32, transform: TouchTransform) -> Result<Scan> {
    let mut devices: Vec<File> = Vec::new();
    let mut touch_idx: Option<usize> = None;
    let mut axes: Option<(u32, u32)> = None;

    for n in 0..=9 {
        let path = format!("/dev/input/event{n}");
        // Non-blocking: next_event poll(2)s before reading, and
        // discard_queued drains without fcntl gymnastics.
        let Ok(file) = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(&path)
        else {
            continue;
        };
        let fd = file.as_raw_fd();

        let mut abs_bits = [0u8; 8];
        // SAFETY: EVIOCGBIT with a buffer of the size encoded in the request.
        let abs_ok =
            unsafe { ioctl(fd, eviocgbit(EV_ABS, abs_bits.len()), abs_bits.as_mut_ptr()) } >= 0;
        let mt = abs_ok && bit_set(&abs_bits, ABS_MT_POSITION_X);
        let is_touch = touch_idx.is_none() && abs_ok && (mt || bit_set(&abs_bits, ABS_X));

        // Key capabilities: 32 bytes cover codes 0..=255 — the power
        // button (116), the cover codes (59/35) and the physical
        // page-turn buttons (193/194).
        let mut key_bits = [0u8; 32];
        // SAFETY: EVIOCGBIT with a buffer of the size encoded in the request.
        let key_ok =
            unsafe { ioctl(fd, eviocgbit(EV_KEY, key_bits.len()), key_bits.as_mut_ptr()) } >= 0;
        let has_power = key_ok && bit_set(&key_bits, KEY_POWER);
        let has_cover =
            key_ok && (bit_set(&key_bits, KEY_SLEEP_COVER) || bit_set(&key_bits, KEY_POWER_COVER));
        let has_pages =
            key_ok && (bit_set(&key_bits, KEY_PAGE_FWD) || bit_set(&key_bits, KEY_PAGE_BACK));

        if !is_touch && !has_power && !has_cover && !has_pages {
            continue;
        }

        if is_touch {
            let (x_axis, y_axis) = if mt {
                (ABS_MT_POSITION_X, ABS_MT_POSITION_Y)
            } else {
                (ABS_X, ABS_Y)
            };
            // Some devices advertise ABS bits but reject EVIOCGABS; fall
            // back to screen dimensions rather than aborting.
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
                    "gideon touch: using {path} (mt={mt}) max_x={max_x} max_y={max_y} transform={transform:?}"
                );
            touch_idx = Some(devices.len());
            axes = Some((max_x, max_y));
        } else {
            eprintln!(
                    "gideon input: using {path} for buttons (power={has_power} cover={has_cover} pages={has_pages})"
                );
        }
        devices.push(file);
    }

    let Some(touch_idx) = touch_idx else {
        return Err(Error::Display(
            "no touch screen found on /dev/input/event0..9".to_string(),
        ));
    };
    let (max_x, max_y) = axes.expect("axes recorded with touch_idx");

    Ok(Scan {
        devices,
        touch_idx,
        max_x,
        max_y,
    })
}

impl KoboTouch {
    /// Drain queued events but keep sleep requests: taps made while a
    /// chapter downloaded are stale, a sleep cover closed during it is not
    /// — the device must still go to sleep once the download finishes.
    pub fn discard_taps(&mut self) {
        let mut buttons = ButtonTracker::new();
        let mut slept = false;
        for file in &self.devices {
            let _ = drain_events(file.as_raw_fd(), |ev| {
                slept |= buttons.push(ev).is_some();
            });
        }
        self.tracker = TouchTracker::new();
        self.buttons = ButtonTracker::new();
        self.pending.retain(|e| matches!(e, UiEvent::Sleep));
        if slept && self.pending.is_empty() {
            // Multiple presses/closes collapse to a single suspend.
            self.pending.push_back(UiEvent::Sleep);
        }
    }

    /// Block in `poll(2)` until any device is readable, then drain it
    /// through the trackers into `pending`. Devices that die (EOF or a
    /// fatal read error — drivers can re-register nodes across a
    /// suspend/resume cycle) are dropped; losing the touch panel is fatal,
    /// so the app exits to the launcher instead of spinning on a dead fd.
    fn poll_and_read(&mut self) -> Result<()> {
        let mut fds: Vec<libc::pollfd> = self
            .devices
            .iter()
            .map(|f| libc::pollfd {
                fd: f.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            })
            .collect();
        // SAFETY: fds points at a valid pollfd array of the given length.
        let ret = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, -1) };
        if ret < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::Interrupted {
                return Ok(());
            }
            return Err(err.into());
        }

        let mut dead: Vec<usize> = Vec::new();
        for (i, pfd) in fds.iter().enumerate() {
            if pfd.revents & (libc::POLLIN | libc::POLLERR | libc::POLLHUP) == 0 {
                continue;
            }
            let is_touch = i == self.touch_idx;
            let tracker = &mut self.tracker;
            let buttons = &mut self.buttons;
            let pending = &mut self.pending;
            let (transform, max_x, max_y) = (self.transform, self.max_x, self.max_y);
            let (screen_w, screen_h) = (self.screen_w, self.screen_h);
            let drain = drain_events(pfd.fd, |ev| {
                if is_touch {
                    if let Some((raw_x, raw_y)) = tracker.push(ev) {
                        let (x, y) =
                            transform.apply(raw_x, raw_y, max_x, max_y, screen_w, screen_h);
                        pending.push_back(UiEvent::Tap { x, y });
                    }
                }
                if let Some(event) = buttons.push(ev) {
                    pending.push_back(event);
                }
            });
            if matches!(drain, Drain::Dead) {
                dead.push(i);
            }
        }

        let mut lost_touch = false;
        for &i in dead.iter().rev() {
            eprintln!("gideon input: device {i} went away, dropping it");
            self.devices.remove(i);
            if i == self.touch_idx {
                lost_touch = true;
            } else if i < self.touch_idx {
                self.touch_idx -= 1;
            }
        }
        if lost_touch {
            // Kernels re-register input nodes (e.g. across suspend/resume);
            // rescan rather than dying — without a touch screen the app is
            // unusable and the launcher would reboot the device.
            eprintln!("gideon input: touch device disappeared, rescanning");
            self.reopen();
            if self.devices.is_empty() {
                return Err(Error::Display(
                    "the touch input device disappeared and did not come back".to_string(),
                ));
            }
        }
        Ok(())
    }
}

/// Outcome of draining one evdev fd.
enum Drain {
    /// Read until `EAGAIN`; the device is healthy.
    Drained,
    /// EOF or a fatal read error; the node is gone.
    Dead,
}

/// Read every queued event off a non-blocking evdev fd, passing each to
/// `f`. The kernel only ever returns whole `input_event`s.
fn drain_events(fd: libc::c_int, mut f: impl FnMut(&libc::input_event)) -> Drain {
    const EVENT_SIZE: usize = std::mem::size_of::<libc::input_event>();
    loop {
        let mut events: [libc::input_event; 64] = unsafe { std::mem::zeroed() };
        // SAFETY: reading whole input_event structs (plain old data) into a
        // correctly sized local buffer on a fd we own.
        let n = unsafe { libc::read(fd, events.as_mut_ptr().cast(), EVENT_SIZE * events.len()) };
        if n > 0 {
            for ev in &events[..(n as usize / EVENT_SIZE)] {
                f(ev);
            }
            continue;
        }
        if n == 0 {
            return Drain::Dead;
        }
        match std::io::Error::last_os_error().raw_os_error() {
            Some(libc::EAGAIN) => return Drain::Drained,
            Some(libc::EINTR) => continue,
            _ => return Drain::Dead,
        }
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
            if let Some(event) = self.pending.pop_front() {
                return Ok(event);
            }
            self.poll_and_read()?;
        }
    }

    fn discard_queued(&mut self) {
        KoboTouch::discard_queued(self);
    }

    fn discard_taps(&mut self) {
        KoboTouch::discard_taps(self);
    }

    fn refresh_devices(&mut self) {
        KoboTouch::reopen(self);
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
        assert_eq!(eviocgbit(EV_ABS, 8), 0x8008_4523);
        // EVIOCGBIT(EV_KEY, 16) == _IOC(read, 'E', 0x21, 16)
        assert_eq!(eviocgbit(EV_KEY, 16), 0x8010_4521);
        // EVIOCGABS(ABS_MT_POSITION_X) == _IOC(read, 'E', 0x75, 24)
        assert_eq!(eviocgabs(ABS_MT_POSITION_X), 0x8018_4575);
    }

    #[test]
    fn pressure_zero_releases_like_libra_colour() {
        let mut t = TouchTracker::new();
        assert_eq!(t.push(&ev(EV_ABS, ABS_MT_POSITION_X, 100)), None);
        assert_eq!(t.push(&ev(EV_ABS, ABS_MT_POSITION_Y, 200)), None);
        assert_eq!(t.push(&ev(EV_ABS, ABS_MT_PRESSURE, 30)), None);
        assert_eq!(t.push(&ev(EV_SYN, SYN_REPORT, 0)), None, "still touching");
        assert_eq!(t.push(&ev(EV_ABS, ABS_MT_PRESSURE, 0)), None);
        assert_eq!(
            t.push(&ev(EV_SYN, SYN_REPORT, 0)),
            Some((100, 200)),
            "pressure 0 + SYN should tap"
        );
    }

    // --- ButtonTracker (power button / sleep cover) ---

    #[test]
    fn power_button_press_sleeps_release_does_not() {
        let mut b = ButtonTracker::new();
        assert_eq!(b.push(&ev(EV_KEY, KEY_POWER, 1)), Some(UiEvent::Sleep));
        assert_eq!(b.push(&ev(EV_KEY, KEY_POWER, 0)), None, "release ignored");
        assert_eq!(b.push(&ev(EV_KEY, KEY_POWER, 2)), None, "repeat ignored");
    }

    #[test]
    fn sleep_cover_close_sleeps_open_does_not() {
        let mut b = ButtonTracker::new();
        // 59 (KEY_F1): the classic Kobo sleep cover, incl. the Libra Colour.
        assert_eq!(
            b.push(&ev(EV_KEY, KEY_SLEEP_COVER, 1)),
            Some(UiEvent::Sleep),
            "cover closed must sleep"
        );
        assert_eq!(
            b.push(&ev(EV_KEY, KEY_SLEEP_COVER, 0)),
            None,
            "cover opened is consumed by wakeup, not an event"
        );
        // 35 (KEY_H): the Elipsa-style power cover code.
        assert_eq!(
            b.push(&ev(EV_KEY, KEY_POWER_COVER, 1)),
            Some(UiEvent::Sleep)
        );
    }

    #[test]
    fn page_buttons_emit_page_events_on_press_only() {
        let mut b = ButtonTracker::new();
        assert_eq!(
            b.push(&ev(EV_KEY, KEY_PAGE_FWD, 1)),
            Some(UiEvent::PageForward)
        );
        assert_eq!(b.push(&ev(EV_KEY, KEY_PAGE_FWD, 0)), None, "release");
        assert_eq!(
            b.push(&ev(EV_KEY, KEY_PAGE_BACK, 1)),
            Some(UiEvent::PageBack)
        );
        assert_eq!(b.push(&ev(EV_KEY, KEY_PAGE_BACK, 2)), None, "repeat");
    }

    #[test]
    fn key_bitmask_covers_the_page_button_codes() {
        // 193/194 live above the old 16-byte (0..=127) probe window; the
        // scan must use a 32-byte mask or the buttons are invisible.
        let mut bits = [0u8; 32];
        bits[(KEY_PAGE_FWD / 8) as usize] |= 1 << (KEY_PAGE_FWD % 8);
        assert!(bit_set(&bits, KEY_PAGE_FWD));
        assert!(!bit_set(&bits[..16], KEY_PAGE_FWD), "16 bytes is too small");
    }

    #[test]
    fn unrelated_keys_and_touch_events_do_not_sleep() {
        let mut b = ButtonTracker::new();
        assert_eq!(b.push(&ev(EV_KEY, BTN_TOUCH, 1)), None);
        assert_eq!(b.push(&ev(EV_KEY, 102, 1)), None, "Home key is not sleep");
        assert_eq!(b.push(&ev(EV_ABS, ABS_MT_POSITION_X, 116)), None);
        assert_eq!(b.push(&ev(EV_SYN, SYN_REPORT, 0)), None);
    }

    #[test]
    fn drain_events_classifies_and_detects_dead_fds() {
        let mut fds = [0i32; 2];
        // SAFETY: pipe2 with a valid 2-int out array.
        assert_eq!(
            unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_NONBLOCK) },
            0
        );
        let (r, w) = (fds[0], fds[1]);

        let events = [ev(EV_KEY, KEY_POWER, 1), ev(EV_SYN, SYN_REPORT, 0)];
        // SAFETY: input_event is plain old data; viewing the array as bytes
        // for a pipe write is sound.
        let bytes = unsafe {
            std::slice::from_raw_parts(events.as_ptr().cast::<u8>(), std::mem::size_of_val(&events))
        };
        // SAFETY: writing a valid buffer to a pipe fd we own.
        assert_eq!(
            unsafe { libc::write(w, bytes.as_ptr().cast(), bytes.len()) },
            bytes.len() as isize
        );

        let mut seen = Vec::new();
        let drain = drain_events(r, |e| seen.push((e.type_, e.code, e.value)));
        assert!(matches!(drain, Drain::Drained), "EAGAIN means healthy");
        assert_eq!(seen, vec![(EV_KEY, KEY_POWER, 1), (EV_SYN, SYN_REPORT, 0)]);

        // Writer gone: the next drain sees EOF and reports the fd dead —
        // poll_and_read drops such devices instead of busy-looping on them.
        // SAFETY: closing fds we own.
        unsafe { libc::close(w) };
        assert!(matches!(drain_events(r, |_| {}), Drain::Dead));
        unsafe { libc::close(r) };
    }

    #[test]
    fn merged_stream_interleaves_taps_and_sleep() {
        // The poll loop feeds every event through both trackers; a power
        // press in the middle of a touch sequence must not corrupt the tap.
        let mut touch = TouchTracker::new();
        let mut buttons = ButtonTracker::new();
        let mut out: Vec<UiEvent> = Vec::new();
        let stream = [
            ev(EV_ABS, ABS_MT_POSITION_X, 100),
            ev(EV_ABS, ABS_MT_POSITION_Y, 200),
            ev(EV_KEY, KEY_POWER, 1), // power pressed mid-touch (other fd)
            ev(EV_KEY, KEY_POWER, 0),
            ev(EV_ABS, ABS_MT_TRACKING_ID, -1),
            ev(EV_SYN, SYN_REPORT, 0),
        ];
        for e in &stream {
            if let Some((x, y)) = touch.push(e) {
                out.push(UiEvent::Tap { x, y });
            }
            if let Some(event) = buttons.push(e) {
                out.push(event);
            }
        }
        assert_eq!(out, vec![UiEvent::Sleep, UiEvent::Tap { x: 100, y: 200 }]);
    }
}

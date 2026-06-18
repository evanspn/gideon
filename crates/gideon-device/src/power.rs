//! Kobo suspend-to-RAM, following Nickel's (and KOReader's) exact dance.
//!
//! The sequence — verified against KOReader `frontend/device/kobo/device.lua`
//! (`Kobo:suspend` / `Kobo:_doSuspend` / `Kobo:resume`) and Nickel's own
//! strace (`platform/kobo/nickel_suspend_strace.txt`) — is:
//!
//! 1. Skip entirely while plugged in: on MTK (Libra Colour) a suspend
//!    attempt with the charger plugged in **hangs the kernel**. The check
//!    matches KOReader's polarity (`powerd.lua`: charging means status
//!    `~= "Discharging"`): anything but exactly `Discharging` — including
//!    `Not charging`, `Full` and `Unknown`, all of which a plugged-in
//!    charger can report — blocks the suspend. Only a missing status file
//!    (tests, dev machines) falls through to suspending.
//! 2. Take Wi-Fi down. KOReader "murders" Wi-Fi before every suspend and
//!    Nickel powers it down too; suspending with the SDIO radio associated
//!    risks `EBUSY` loops and a broken association after resume. We
//!    best-effort `ifconfig wlan0 down` (wpa_supplicant and dhcpcd stay
//!    alive — only Nickel was killed — so link-up on wake reassociates and
//!    renews the lease). `GIDEON_SUSPEND_WIFI=0` disables both halves.
//! 3. `echo 1 > /sys/power/state-extended` — sets the kernel-global
//!    `gSleep_Mode_Suspend` flag that NTX/MTK kernels use to put peripheral
//!    subsystems to sleep and arm the wakeup pins. Abort on failure.
//! 4. Wait ~2 s. KOReader: "I have traumatic memories of things going awry
//!    if we don't sleep between the two writes".
//! 5. `sync` the filesystems.
//! 6. `echo mem > /sys/power/state` — the write blocks until wakeup. On
//!    failure (typically `EBUSY` from the EPDC or touch controller) reset
//!    `state-extended` to 0 — Nickel's observed `1 → mem(EBUSY) → 0` loop —
//!    and retry the whole sequence a couple of times before giving up.
//! 7. On wake: `echo 0 > /sys/power/state-extended` to resume subsystems,
//!    wait 100 ms for the kernel to catch up (KOReader's resume HACK), and
//!    bring Wi-Fi back up.
//!
//! MTK notes (Libra Colour, `monza`): there is no `"standby"` power state —
//! Nickel and KOReader both use `"mem"` — and the plugged-in guard in step 1
//! is mandatory there. The sequence itself is identical to NXP devices.
//!
//! Everything is rooted at a configurable directory so tests can run the
//! exact sequence against a tempdir and assert the writes and their order.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::{Error, Result};

/// How many full `1 → mem → 0` attempts to make before giving up.
const MAX_ATTEMPTS: u32 = 3;

/// What a [`KoboSuspend::suspend`] call did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SuspendOutcome {
    /// The device suspended and has since woken up.
    Suspended,
    /// The device is charging; suspending was skipped (on MTK kernels a
    /// suspend attempt while plugged in hangs the kernel).
    SkippedCharging,
}

/// Suspend-to-RAM driver. Construct with [`KoboSuspend::new`] on hardware;
/// tests use [`KoboSuspend::with_root`] plus [`KoboSuspend::settle`] to point
/// it at a tempdir with zero waits.
pub struct KoboSuspend {
    root: PathBuf,
    settle: Duration,
    /// Ordered log of every step taken, for tests and the device log.
    log: Vec<String>,
}

impl KoboSuspend {
    /// The real thing: rooted at `/`, with the 2 s settle wait.
    pub fn new() -> Self {
        Self::with_root("/")
    }

    /// Rooted at `root` (tests pass a tempdir laid out like `/`).
    pub fn with_root(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            settle: Duration::from_secs(2),
            log: Vec::new(),
        }
    }

    /// Override the settle wait between flagging `state-extended` and
    /// writing `mem` (tests use zero).
    pub fn settle(mut self, settle: Duration) -> Self {
        self.settle = settle;
        self
    }

    /// Every step taken so far, in order (`"path <- value"` for writes).
    pub fn log(&self) -> &[String] {
        &self.log
    }

    fn step(&mut self, message: String) {
        eprintln!("gideon power: {message}");
        self.log.push(message);
    }

    /// Suspend to RAM. Blocks until the device wakes up (power button or
    /// sleep cover opening). Returns [`SuspendOutcome::SkippedCharging`]
    /// without touching `/sys/power` when the charger is plugged in.
    pub fn suspend(&mut self) -> Result<SuspendOutcome> {
        if self.is_plugged_in() {
            self.step("plugged in — skipping suspend (MTK kernels hang otherwise)".to_string());
            return Ok(SuspendOutcome::SkippedCharging);
        }

        // KOReader kills Wi-Fi before every suspend; a live SDIO radio is
        // the classic source of EBUSY and post-resume breakage.
        self.wifi("down");

        let mut last_err = None;
        for attempt in 1..=MAX_ATTEMPTS {
            match self.try_suspend() {
                Ok(()) => {
                    // Awake again: resume subsystems and give the kernel a
                    // moment (KOReader waits 0.1 s after the write).
                    self.write_sysfs("sys/power/state-extended", "0")?;
                    if !self.settle.is_zero() {
                        std::thread::sleep(Duration::from_millis(100));
                    }
                    self.wifi("up");
                    self.step("woke up".to_string());
                    return Ok(SuspendOutcome::Suspended);
                }
                Err(e) => {
                    // Nickel's own failure loop is 1 → mem (EBUSY) → 0,
                    // retried; reset state-extended and try again.
                    self.step(format!("suspend attempt {attempt} failed: {e}"));
                    let _ = self.write_sysfs("sys/power/state-extended", "0");
                    last_err = Some(e);
                }
            }
        }
        // Don't leave the radio down after a failed suspend.
        self.wifi("up");
        Err(last_err.unwrap_or_else(|| Error::Display("suspend failed".to_string())))
    }

    /// Best-effort `ifconfig <iface> up|down` on hardware, on the **real**
    /// Wi-Fi interface (the Kobo's is `eth0`, NOT `wlan0` — using the wrong
    /// name silently no-ops, which left the radio "up" with a stale address
    /// across suspend so nothing reconnected on wake). This is only the cheap
    /// link toggle around suspend; the actual reconnection is owned by
    /// `network::reconnect_after_wake()`, which the app fires after we return
    /// and which power-cycles the radio and restarts wpa_supplicant from
    /// scratch (a warm re-associate doesn't recover the MTK chip after sleep).
    /// `GIDEON_SUSPEND_WIFI=0` opts out entirely.
    fn wifi(&mut self, direction: &str) {
        if std::env::var("GIDEON_SUSPEND_WIFI").as_deref() == Ok("0") {
            return;
        }
        if self.root == Path::new("/") {
            let iface = crate::network::interface();
            let status = std::process::Command::new("ifconfig")
                .args([iface.as_str(), direction])
                .status();
            if !matches!(status, Ok(s) if s.success()) {
                self.step(format!("ifconfig {iface} {direction} failed (ignored)"));
                return;
            }
        }
        self.step(format!("wifi {direction}"));
    }

    /// One `1 → settle → sync → mem` attempt. Returns once the device has
    /// woken up (or immediately on failure).
    fn try_suspend(&mut self) -> Result<()> {
        self.write_sysfs("sys/power/state-extended", "1")?;
        if !self.settle.is_zero() {
            self.step(format!("settling {:?} before suspend", self.settle));
            std::thread::sleep(self.settle);
        }
        self.sync_fs();
        // This write blocks until wakeup.
        self.write_sysfs("sys/power/state", "mem")
    }

    fn write_sysfs(&mut self, relative: &str, value: &str) -> Result<()> {
        let path = self.root.join(relative);
        // Nickel opens these O_WRONLY|O_CREAT|O_TRUNC, so do the same.
        let result = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .and_then(|mut f| f.write_all(value.as_bytes()));
        match result {
            Ok(()) => {
                self.step(format!("{relative} <- {value}"));
                Ok(())
            }
            Err(e) => {
                self.step(format!("{relative} <- {value} FAILED: {e}"));
                Err(Error::Display(format!(
                    "writing '{value}' to {} failed: {e}",
                    path.display()
                )))
            }
        }
    }

    fn sync_fs(&mut self) {
        // Only sync the real filesystem on hardware; tests don't need it.
        if self.root == Path::new("/") {
            let _ = std::process::Command::new("sync").status();
        }
        self.step("sync".to_string());
    }

    /// `true` unless every known battery reports exactly `Discharging` —
    /// KOReader's polarity (`powerd.lua`: charging means status
    /// `~= "Discharging"`). A plugged-in charger can report `Charging`,
    /// `Full`, `Not charging` or `Unknown`; all of those must block the
    /// suspend, because an MTK suspend with the charger in hangs the
    /// kernel. Only a missing status file (tests, dev machines) suspends.
    fn is_plugged_in(&self) -> bool {
        // Libra Colour / Clara family use bd71827_bat; older NTX boards
        // (and KOReader's generic fallback) use "battery".
        for name in ["battery", "bd71827_bat"] {
            let path = self
                .root
                .join("sys/class/power_supply")
                .join(name)
                .join("status");
            if let Ok(status) = std::fs::read_to_string(&path) {
                return !status.trim().eq_ignore_ascii_case("discharging");
            }
        }
        false
    }
}

impl Default for KoboSuspend {
    fn default() -> Self {
        Self::new()
    }
}

/// Battery charge percent from sysfs, `None` when no battery reports one
/// (tests, dev machines). Reads `capacity` from the same power-supply
/// directories as the charging probe.
pub fn battery_percent() -> Option<u8> {
    battery_percent_at(Path::new("/"))
}

/// [`battery_percent`] rooted at `root`, so tests can point it at a
/// tempdir laid out like `/`. Lenient: an unparsable capacity file is
/// treated like a missing one, and values are clamped to 0–100.
pub fn battery_percent_at(root: &Path) -> Option<u8> {
    for name in ["battery", "bd71827_bat"] {
        let path = root
            .join("sys/class/power_supply")
            .join(name)
            .join("capacity");
        if let Some(percent) = std::fs::read_to_string(&path)
            .ok()
            .and_then(|raw| raw.trim().parse::<u8>().ok())
        {
            return Some(percent.min(100));
        }
    }
    None
}

/// Drop the **calling thread** to idle CPU and IO priority, so a background
/// worker (e.g. chapter pre-download) physically cannot steal cycles or flash
/// bandwidth from the reader on the device's modest CPU — it runs only when
/// nothing else wants the core, and its writes yield to the reader's page
/// reads. Off-device (no `kobo` feature) this is a no-op.
#[cfg(feature = "kobo")]
pub fn lower_current_thread_to_idle() {
    // SAFETY: all three are plain scheduler syscalls on the *current* thread
    // (pid/tid 0); failures are ignored (best-effort niceness).
    unsafe {
        // CPU: SCHED_IDLE — scheduled only when no other task is runnable.
        // `sched_param` carries extra POSIX sporadic-server fields on musl, so
        // zero it (priority 0 is exactly what SCHED_IDLE wants) instead of a
        // field-by-field literal that wouldn't compile across libcs.
        let param: libc::sched_param = std::mem::zeroed();
        libc::sched_setscheduler(0, libc::SCHED_IDLE, &param);
        // Belt and suspenders for schedulers that still time-slice IDLE tasks.
        libc::setpriority(libc::PRIO_PROCESS, 0, 19);
        // IO: idle class, so large CBZ writes defer to the reader's reads.
        const IOPRIO_WHO_PROCESS: libc::c_long = 1;
        const IOPRIO_CLASS_IDLE: libc::c_long = 3;
        const IOPRIO_CLASS_SHIFT: libc::c_long = 13;
        let ioprio = IOPRIO_CLASS_IDLE << IOPRIO_CLASS_SHIFT;
        libc::syscall(libc::SYS_ioprio_set, IOPRIO_WHO_PROCESS, 0, ioprio);
    }
}

/// Off-device stub — there's no scheduler to tune.
#[cfg(not(feature = "kobo"))]
pub fn lower_current_thread_to_idle() {}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (tempfile::TempDir, KoboSuspend) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("sys/power")).unwrap();
        std::fs::write(dir.path().join("sys/power/state-extended"), "0").unwrap();
        std::fs::write(dir.path().join("sys/power/state"), "").unwrap();
        let suspend = KoboSuspend::with_root(dir.path()).settle(Duration::ZERO);
        (dir, suspend)
    }

    #[test]
    fn suspend_runs_the_kobo_sequence_in_order() {
        let (dir, mut suspend) = fixture();
        assert_eq!(suspend.suspend().unwrap(), SuspendOutcome::Suspended);

        // The exact write/step order of the Nickel/KOReader dance:
        // Wi-Fi dies first, comes back only after the kernel resumed.
        assert_eq!(
            suspend.log(),
            &[
                "wifi down",
                "sys/power/state-extended <- 1",
                "sync",
                "sys/power/state <- mem",
                "sys/power/state-extended <- 0",
                "wifi up",
                "woke up",
            ]
        );
        // Final file states: mem requested, subsystems resumed.
        assert_eq!(
            std::fs::read_to_string(dir.path().join("sys/power/state")).unwrap(),
            "mem"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("sys/power/state-extended")).unwrap(),
            "0"
        );
    }

    #[test]
    fn failed_state_write_resets_state_extended_and_errors() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("sys/power")).unwrap();
        std::fs::write(dir.path().join("sys/power/state-extended"), "0").unwrap();
        // Make the `mem` write fail: state is a directory.
        std::fs::create_dir(dir.path().join("sys/power/state")).unwrap();

        let mut suspend = KoboSuspend::with_root(dir.path()).settle(Duration::ZERO);
        assert!(suspend.suspend().is_err());

        // Nickel's failure loop: each attempt ends with state-extended <- 0.
        assert_eq!(
            std::fs::read_to_string(dir.path().join("sys/power/state-extended")).unwrap(),
            "0"
        );
        let resets = suspend
            .log()
            .iter()
            .filter(|l| l.as_str() == "sys/power/state-extended <- 0")
            .count();
        assert_eq!(resets, MAX_ATTEMPTS as usize, "one reset per attempt");
        // And every attempt starts by flagging subsystems again.
        let flags = suspend
            .log()
            .iter()
            .filter(|l| l.as_str() == "sys/power/state-extended <- 1")
            .count();
        assert_eq!(flags, MAX_ATTEMPTS as usize);
    }

    #[test]
    fn missing_state_extended_aborts_before_touching_state() {
        let dir = tempfile::tempdir().unwrap();
        // No sys/power at all: the very first write must fail and nothing
        // else may happen.
        let mut suspend = KoboSuspend::with_root(dir.path()).settle(Duration::ZERO);
        assert!(suspend.suspend().is_err());
        assert!(!suspend.log().iter().any(|l| l.starts_with("sync")));
        assert!(!suspend
            .log()
            .iter()
            .any(|l| l.starts_with("sys/power/state <-")));
    }

    #[test]
    fn any_plugged_in_status_skips_suspend_entirely() {
        let (dir, _) = fixture();
        let battery = dir.path().join("sys/class/power_supply/bd71827_bat");
        std::fs::create_dir_all(&battery).unwrap();

        // KOReader polarity: everything except exactly "Discharging" means
        // a charger may be attached — and an MTK suspend with the charger
        // in hangs the kernel. "Not charging" (topped-off battery, charger
        // still plugged) is the trap case.
        for status in ["Charging\n", "Full", "Not charging\n", "Unknown"] {
            std::fs::write(battery.join("status"), status).unwrap();
            let mut suspend = KoboSuspend::with_root(dir.path()).settle(Duration::ZERO);
            assert_eq!(
                suspend.suspend().unwrap(),
                SuspendOutcome::SkippedCharging,
                "status {status:?} must block suspend"
            );
            assert!(
                !suspend.log().iter().any(|l| l.contains("<-")),
                "no sysfs writes while plugged in (status {status:?})"
            );
        }
    }

    #[test]
    fn battery_percent_reads_capacity_from_either_supply() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(battery_percent_at(dir.path()), None, "no sysfs yet");

        let battery = dir.path().join("sys/class/power_supply/bd71827_bat");
        std::fs::create_dir_all(&battery).unwrap();
        std::fs::write(battery.join("capacity"), "47\n").unwrap();
        assert_eq!(battery_percent_at(dir.path()), Some(47));

        // Lenient: clamp overshoot, treat garbage as missing.
        std::fs::write(battery.join("capacity"), "147").unwrap();
        assert_eq!(battery_percent_at(dir.path()), Some(100), "clamped");
        std::fs::write(battery.join("capacity"), "wat").unwrap();
        assert_eq!(battery_percent_at(dir.path()), None);

        // The older NTX name works too.
        let ntx = dir.path().join("sys/class/power_supply/battery");
        std::fs::create_dir_all(&ntx).unwrap();
        std::fs::write(ntx.join("capacity"), "12").unwrap();
        assert_eq!(battery_percent_at(dir.path()), Some(12));
    }

    #[test]
    fn discharging_battery_does_not_block_suspend() {
        let (dir, _) = fixture();
        let battery = dir.path().join("sys/class/power_supply/battery");
        std::fs::create_dir_all(&battery).unwrap();
        std::fs::write(battery.join("status"), "Discharging\n").unwrap();

        let mut suspend = KoboSuspend::with_root(dir.path()).settle(Duration::ZERO);
        assert_eq!(suspend.suspend().unwrap(), SuspendOutcome::Suspended);
    }
}

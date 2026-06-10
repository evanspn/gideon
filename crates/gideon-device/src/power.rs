//! Kobo suspend-to-RAM, following Nickel's (and KOReader's) exact dance.
//!
//! The sequence — verified against KOReader `frontend/device/kobo/device.lua`
//! (`Kobo:suspend` / `Kobo:_doSuspend` / `Kobo:resume`) and Nickel's own
//! strace (`platform/kobo/nickel_suspend_strace.txt`) — is:
//!
//! 1. Skip entirely while charging: on MTK (Libra Colour) a suspend attempt
//!    with the charger plugged in **hangs the kernel** (KOReader guards this
//!    with `isMTK() and powerd:isCharging()`; we guard unconditionally —
//!    staying awake while plugged in is harmless on every Kobo).
//! 2. `echo 1 > /sys/power/state-extended` — sets the kernel-global
//!    `gSleep_Mode_Suspend` flag that NTX/MTK kernels use to put peripheral
//!    subsystems to sleep and arm the wakeup pins. Abort on failure.
//! 3. Wait ~2 s. KOReader: "I have traumatic memories of things going awry
//!    if we don't sleep between the two writes".
//! 4. `sync` the filesystems.
//! 5. `echo mem > /sys/power/state` — the write blocks until wakeup. On
//!    failure (typically `EBUSY` from the EPDC or touch controller) reset
//!    `state-extended` to 0 — Nickel's observed `1 → mem(EBUSY) → 0` loop —
//!    and retry the whole sequence a couple of times before giving up.
//! 6. On wake: `echo 0 > /sys/power/state-extended` to resume subsystems,
//!    then wait 100 ms for the kernel to catch up (KOReader's resume HACK).
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
        if self.is_charging() {
            self.step("charging — skipping suspend (MTK kernels hang otherwise)".to_string());
            return Ok(SuspendOutcome::SkippedCharging);
        }

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
        Err(last_err.unwrap_or_else(|| Error::Display("suspend failed".to_string())))
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

    /// `true` when any known battery reports Charging/Full. KOReader treats
    /// "charged but still plugged in" as charging too — so do we.
    fn is_charging(&self) -> bool {
        // Libra Colour / Clara family use bd71827_bat; older NTX boards
        // (and KOReader's generic fallback) use "battery".
        for name in ["battery", "bd71827_bat"] {
            let path = self
                .root
                .join("sys/class/power_supply")
                .join(name)
                .join("status");
            if let Ok(status) = std::fs::read_to_string(&path) {
                let status = status.trim();
                if status.eq_ignore_ascii_case("charging") || status.eq_ignore_ascii_case("full") {
                    return true;
                }
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

        // The exact write/step order of the Nickel/KOReader dance.
        assert_eq!(
            suspend.log(),
            &[
                "sys/power/state-extended <- 1",
                "sync",
                "sys/power/state <- mem",
                "sys/power/state-extended <- 0",
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
    fn charging_skips_suspend_entirely() {
        let (dir, _) = fixture();
        let battery = dir.path().join("sys/class/power_supply/bd71827_bat");
        std::fs::create_dir_all(&battery).unwrap();
        std::fs::write(battery.join("status"), "Charging\n").unwrap();

        let mut suspend = KoboSuspend::with_root(dir.path()).settle(Duration::ZERO);
        assert_eq!(suspend.suspend().unwrap(), SuspendOutcome::SkippedCharging);
        assert!(
            !suspend.log().iter().any(|l| l.contains("<-")),
            "no sysfs writes while charging"
        );
        // Full (charged but plugged in) also counts, like KOReader.
        std::fs::write(battery.join("status"), "Full").unwrap();
        let mut suspend = KoboSuspend::with_root(dir.path()).settle(Duration::ZERO);
        assert_eq!(suspend.suspend().unwrap(), SuspendOutcome::SkippedCharging);
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

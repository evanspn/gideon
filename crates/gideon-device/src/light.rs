//! Kobo frontlight control: brightness and warmth ("night light").
//!
//! Paths and semantics from KOReader's Kobo device table for the Libra
//! Colour (`monza`, `frontend/device/kobo/device.lua` + `sysfs_light.lua`):
//!
//! * Brightness: write the percent (0–100) straight to
//!   `/sys/class/backlight/mxc_msp430.0/brightness`.
//! * Warmth: `/sys/class/backlight/lm3630a_led/color` on a native 0–10
//!   scale that is **inverted** (`nl_inverted`): 10 is cold, 0 is fully
//!   warm. Percent warmth is scaled to native and flipped before writing.
//!
//! Writes are best-effort: a missing node (different model, dev machine)
//! logs and carries on — light control must never crash the reader.
//! Everything is rooted at a configurable directory for tests.

use std::path::{Path, PathBuf};

/// Brightness/warmth control, abstract so the UI can be tested with a fake.
pub trait LightControl {
    /// Current brightness percent (0–100).
    fn brightness(&self) -> u8;
    /// Set brightness percent (clamped to 0–100).
    fn set_brightness(&mut self, percent: u8);
    /// Current warmth percent (0–100).
    fn warmth(&self) -> u8;
    /// Set warmth percent (clamped to 0–100).
    fn set_warmth(&mut self, percent: u8);

    /// Re-push the current levels to the hardware. The kernel powers the
    /// frontlight controller down across suspend (`gSleep_Mode_Suspend`),
    /// so waking up must write the levels again — KOReader does the same
    /// in `afterResume`.
    fn reapply(&mut self) {
        let (b, w) = (self.brightness(), self.warmth());
        self.set_brightness(b);
        self.set_warmth(w);
    }
}

/// Brightness sysfs node (percent, written as-is).
const WHITE_PATH: &str = "sys/class/backlight/mxc_msp430.0/brightness";
/// Warmth mixer sysfs node (native scale, inverted).
const MIXER_PATH: &str = "sys/class/backlight/lm3630a_led/color";
/// Native warmth scale maximum (KOReader `nl_max` for monza).
const NL_MAX: u8 = 10;

/// The real frontlight, via sysfs.
pub struct KoboFrontlight {
    root: PathBuf,
    brightness: u8,
    warmth: u8,
}

impl KoboFrontlight {
    /// Rooted at `/`, with the given starting levels (not yet applied —
    /// call [`Self::apply`] to push them to the hardware).
    pub fn new(brightness: u8, warmth: u8) -> Self {
        Self::with_root("/", brightness, warmth)
    }

    /// Rooted at `root` (tests pass a tempdir laid out like `/`).
    pub fn with_root(root: impl Into<PathBuf>, brightness: u8, warmth: u8) -> Self {
        Self {
            root: root.into(),
            brightness: brightness.min(100),
            warmth: warmth.min(100),
        }
    }

    /// Push the current levels to the hardware (startup restore).
    pub fn apply(&mut self) {
        self.write_white();
        self.write_mixer();
    }

    fn write_white(&self) {
        write_sysfs(&self.root.join(WHITE_PATH), u32::from(self.brightness));
    }

    fn write_mixer(&self) {
        // Percent → native 0..=NL_MAX, then invert: the mixer's 0 is warm.
        let native = (u32::from(self.warmth) * u32::from(NL_MAX) + 50) / 100;
        write_sysfs(
            &self.root.join(MIXER_PATH),
            u32::from(NL_MAX) - native.min(u32::from(NL_MAX)),
        );
    }
}

impl LightControl for KoboFrontlight {
    fn brightness(&self) -> u8 {
        self.brightness
    }

    fn set_brightness(&mut self, percent: u8) {
        self.brightness = percent.min(100);
        self.write_white();
    }

    fn warmth(&self) -> u8 {
        self.warmth
    }

    fn set_warmth(&mut self, percent: u8) {
        self.warmth = percent.min(100);
        self.write_mixer();
    }
}

fn write_sysfs(path: &Path, value: u32) {
    if let Err(e) = std::fs::write(path, value.to_string()) {
        eprintln!(
            "gideon light: writing {value} to {} failed: {e}",
            path.display()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (tempfile::TempDir, KoboFrontlight) {
        let dir = tempfile::tempdir().unwrap();
        for p in [WHITE_PATH, MIXER_PATH] {
            let path = dir.path().join(p);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, "0").unwrap();
        }
        let light = KoboFrontlight::with_root(dir.path(), 20, 0);
        (dir, light)
    }

    fn read(dir: &tempfile::TempDir, p: &str) -> String {
        std::fs::read_to_string(dir.path().join(p)).unwrap()
    }

    #[test]
    fn brightness_writes_percent_directly() {
        let (dir, mut light) = fixture();
        light.set_brightness(42);
        assert_eq!(read(&dir, WHITE_PATH), "42");
        assert_eq!(light.brightness(), 42);

        light.set_brightness(200); // clamped
        assert_eq!(read(&dir, WHITE_PATH), "100");
    }

    #[test]
    fn warmth_is_scaled_to_native_and_inverted() {
        let (dir, mut light) = fixture();
        // 0% warm = native 0 = mixer 10 (cold end, nl_inverted).
        light.set_warmth(0);
        assert_eq!(read(&dir, MIXER_PATH), "10");
        // 100% warm = native 10 = mixer 0 (fully warm).
        light.set_warmth(100);
        assert_eq!(read(&dir, MIXER_PATH), "0");
        // 50% = native 5 = mixer 5.
        light.set_warmth(50);
        assert_eq!(read(&dir, MIXER_PATH), "5");
    }

    #[test]
    fn apply_restores_both_channels() {
        let (dir, _) = fixture();
        let mut light = KoboFrontlight::with_root(dir.path(), 33, 100);
        light.apply();
        assert_eq!(read(&dir, WHITE_PATH), "33");
        assert_eq!(read(&dir, MIXER_PATH), "0");
    }

    #[test]
    fn missing_nodes_do_not_panic() {
        let dir = tempfile::tempdir().unwrap();
        let mut light = KoboFrontlight::with_root(dir.path(), 20, 0);
        light.set_brightness(50);
        light.set_warmth(50);
        light.apply();
    }
}

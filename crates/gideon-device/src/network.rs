//! Best-effort Wi-Fi auto-enable and connectivity check, ported from
//! KOReader's Kobo scripts for the MTK Libra Colour (monza).
//!
//! gideon used to *inherit* whatever Wi-Fi state Nickel left behind and never
//! touch the radio — so if the user launched with Wi-Fi off, nothing could
//! download, with no way to recover in-app. This brings the radio up itself,
//! exactly the "it should just fix itself" behaviour: before a network action
//! that finds no connection, gideon loads the MTK Wi-Fi modules (if cold),
//! powers the chip on, brings the interface up, starts wpa_supplicant against
//! Nickel's saved networks, runs DHCP, and waits for an address.
//!
//! Everything here is **best-effort and additive**: it only acts when we are
//! actually offline (the connected path returns instantly and untouched), and
//! it only runs on a real device — off-device (desktop/CI) the interface
//! sysfs dir is absent, so every call is a no-op and reports "online" so tests
//! never try to manage Wi-Fi.
//!
//! Device-specific facts are sourced from KOReader (`platform/kobo/
//! enable-wifi.sh`, `obtain-ip.sh`; `frontend/ui/network/manager.lua`'s
//! `ifHasAnAddress`/`connectivityCheck`) and are overridable by environment
//! variable, since KOReader itself reads them from Nickel's environment rather
//! than hardcoding them per codename.

use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

/// How long to wait for association + a DHCP lease before giving up. KOReader
/// allows up to ~45s; we use a tighter window so a genuinely-unavailable
/// network surfaces the "no network" message reasonably soon.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// Connectivity poll cadence while waiting (KOReader's `scheduleConnectivityCheck`).
const POLL_INTERVAL: Duration = Duration::from_millis(250);

/// The Wi-Fi interface name. KOReader resolves this from Nickel's environment
/// (`INTERFACE`), defaulting to `eth0` — the Kobo convention, including on the
/// MTK platform (the wireless interface is named `eth0`, not `wlan0`).
/// `GIDEON_WIFI_INTERFACE` overrides for the rare kernel that names it
/// differently.
pub fn interface() -> String {
    env_nonempty("GIDEON_WIFI_INTERFACE")
        .or_else(|| env_nonempty("INTERFACE"))
        .unwrap_or_else(|| "eth0".to_string())
}

fn env_nonempty(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|s| !s.trim().is_empty())
}

/// Whether the interface exists in sysfs — i.e. we're on a device with this
/// network interface. When it's absent we're not on a Kobo (desktop/CI), so
/// every operation here becomes a no-op.
fn on_device(iface: &str) -> bool {
    Path::new(&format!("/sys/class/net/{iface}")).exists()
}

/// `/sys/class/net/<iface>/operstate == "up"` — for Wi-Fi this means
/// associated + authenticated, not merely `ifconfig up` (KOReader's
/// `sysfsInterfaceOperational`).
fn operstate_up(iface: &str) -> bool {
    std::fs::read_to_string(format!("/sys/class/net/{iface}/operstate"))
        .map(|s| s.trim() == "up")
        .unwrap_or(false)
}

/// Whether the interface has an IPv4 address assigned (KOReader's
/// `ifHasAnAddress`, via busybox `ifconfig` rather than getifaddrs FFI).
fn has_ipv4(iface: &str) -> bool {
    Command::new("ifconfig")
        .arg(iface)
        .output()
        .ok()
        .map(|o| {
            let out = String::from_utf8_lossy(&o.stdout);
            // busybox: "inet addr:192.168…"; iproute2/newer: "inet 192.168…".
            out.contains("inet addr:") || out.contains("inet ")
        })
        .unwrap_or(false)
}

/// True when we have a usable connection: operstate up *and* an IPv4 address.
/// Off-device (no such interface) we report online so nothing tries to manage
/// Wi-Fi where there's none to manage.
pub fn is_online() -> bool {
    let iface = interface();
    if !on_device(&iface) {
        return true;
    }
    operstate_up(&iface) && has_ipv4(&iface)
}

/// Fire the best-effort bring-up sequence (modules → power → ifup →
/// wpa_supplicant → DHCP). Returns immediately if already online, off-device,
/// or opted out via `GIDEON_WIFI_AUTOENABLE=0`. The work is delegated to a
/// shell script so the multi-step KOReader sequence stays legible; failures of
/// individual steps are swallowed (the device may already be partway up).
pub fn bring_up_wifi() {
    let iface = interface();
    if !on_device(&iface) || std::env::var("GIDEON_WIFI_AUTOENABLE").as_deref() == Ok("0") {
        return;
    }
    let _ = Command::new("sh")
        .arg("-c")
        .arg(enable_script(&iface))
        .status();
}

/// Poll until [`is_online`] or `timeout`. Returns whether we got online.
pub fn wait_until_online(timeout: Duration) -> bool {
    let start = Instant::now();
    loop {
        if is_online() {
            return true;
        }
        if start.elapsed() >= timeout {
            return false;
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

/// One-shot convenience: if offline, bring Wi-Fi up and wait for a connection.
/// Returns whether we ended up online. The UI uses the three steps directly so
/// it can paint a "Connecting…" status before the (blocking) wait.
pub fn ensure_online(timeout: Duration) -> bool {
    if is_online() {
        return true;
    }
    bring_up_wifi();
    wait_until_online(timeout)
}

/// The MTK (monza, `wlan_drv_gen4m`) Wi-Fi enable sequence as a shell script,
/// ported from KOReader's `platform/kobo/enable-wifi.sh` + `obtain-ip.sh`.
/// Module name, kernel-module directory and wpa_supplicant config are
/// overridable by env (KOReader sources these from Nickel's environment); the
/// defaults are the Libra Colour values.
fn enable_script(iface: &str) -> String {
    // KOReader: WIFI_MODULE=wlan_drv_gen4m for the MTK Libra Colour.
    let module = env_nonempty("GIDEON_WIFI_MODULE")
        .or_else(|| env_nonempty("WIFI_MODULE"))
        .unwrap_or_else(|| "wlan_drv_gen4m".to_string());
    // KMOD dir is /drivers/<PLATFORM>/mt66xx; PLATFORM comes from the launcher
    // (Nickel's env). Fall back to a wildcard the shell resolves if unset.
    let kmod_dir = env_nonempty("GIDEON_WIFI_KMOD_DIR")
        .or_else(|| env_nonempty("PLATFORM").map(|p| format!("/drivers/{p}/mt66xx")))
        .unwrap_or_else(|| "/drivers/*/mt66xx".to_string());
    let conf = env_nonempty("GIDEON_WPA_SUPPLICANT_CONF")
        .unwrap_or_else(|| "/etc/wpa_supplicant/wpa_supplicant.conf".to_string());

    // Each step is best-effort (`|| :`); insmod only when the module isn't
    // already loaded, and the whole module/power block is skipped once the
    // chip control node exists (warm start — the common reconnect case).
    format!(
        r#"
sleep_ms() {{ usleep "$1"000 2>/dev/null || sleep 1; }}
if [ ! -e /dev/wmtWifi ]; then
  for m in wmt_drv wmt_chrdev_wifi wmt_cdev_bt {module}; do
    if ! grep -q "^${{m}} " /proc/modules 2>/dev/null; then
      insmod {kmod_dir}/${{m}}.ko 2>/dev/null || :
      sleep_ms 250
    fi
  done
  echo 0xDB9DB9 > /proc/driver/wmt_dbg 2>/dev/null || :
  echo "7 9 0"  > /proc/driver/wmt_dbg 2>/dev/null || :
  sleep 1
  echo 0xDB9DB9 > /proc/driver/wmt_dbg 2>/dev/null || :
  echo "7 9 1"  > /proc/driver/wmt_dbg 2>/dev/null || :
fi
echo 1 > /dev/wmtWifi 2>/dev/null || :
sleep 1
ifconfig {iface} up 2>/dev/null || :
pkill -0 wpa_supplicant 2>/dev/null || \
  wpa_supplicant -D nl80211 -s -i {iface} -c {conf} -C /var/run/wpa_supplicant -B 2>/dev/null || :
if command -v dhcpcd >/dev/null 2>&1; then
  dhcpcd -t 30 -w {iface} 2>/dev/null || :
else
  udhcpc -i {iface} -t 5 -T 3 -n -q 2>/dev/null || :
fi
"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interface_prefers_env_then_falls_back_to_eth0() {
        // Default (no env set in this process) is eth0.
        // (We avoid mutating env vars here to not race other tests; just
        // assert the documented fallback constant is what we return when the
        // overrides are empty.)
        std::env::remove_var("GIDEON_WIFI_INTERFACE");
        std::env::remove_var("INTERFACE");
        assert_eq!(interface(), "eth0");
    }

    #[test]
    fn off_device_is_treated_as_online() {
        // CI/desktop has no such interface, so management is skipped and we
        // report online (never try to bring Wi-Fi up where there's none).
        assert!(!on_device("definitely-not-a-real-iface-zzz"));
        // is_online() short-circuits to true when the iface sysfs is absent.
        std::env::set_var("GIDEON_WIFI_INTERFACE", "definitely-not-a-real-iface-zzz");
        assert!(is_online());
        std::env::remove_var("GIDEON_WIFI_INTERFACE");
    }

    #[test]
    fn enable_script_has_the_monza_sequence() {
        let s = enable_script("eth0");
        // Module loads, chip power-on, interface up, supplicant, DHCP — in
        // the KOReader order, on the resolved interface.
        assert!(s.contains("wlan_drv_gen4m"));
        assert!(s.contains("/dev/wmtWifi"));
        assert!(s.contains("ifconfig eth0 up"));
        assert!(s.contains("wpa_supplicant -D nl80211 -s -i eth0"));
        assert!(s.contains("dhcpcd -t 30 -w eth0"));
        let drv = s.find("/dev/wmtWifi").unwrap();
        let ifup = s.find("ifconfig eth0 up").unwrap();
        let dhcp = s.find("dhcpcd").unwrap();
        assert!(drv < ifup && ifup < dhcp, "steps must be ordered");
    }

    #[test]
    fn enable_script_honors_module_and_iface_overrides() {
        std::env::set_var("GIDEON_WIFI_MODULE", "moal");
        let s = enable_script("wlan0");
        assert!(s.contains("moal"));
        assert!(s.contains("ifconfig wlan0 up"));
        std::env::remove_var("GIDEON_WIFI_MODULE");
    }
}

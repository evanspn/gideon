//! Wi-Fi management, a faithful port of KOReader's Kobo scripts for the MTK
//! Libra Colour (monza, `wlan_drv_gen4m`).
//!
//! We deliberately do **exactly what KOReader does**, in the same places, after
//! many attempts at "cleverer" variants regressed reconnection. KOReader splits
//! Wi-Fi across the suspend boundary into four scripts; we mirror each one:
//!
//! * [`enable_wifi_script`]  ← `platform/kobo/enable-wifi.sh`
//! * [`obtain_ip_script`]    ← `platform/kobo/obtain-ip.sh` (+ `release-ip.sh`)
//! * [`disable_wifi_script`] ← `platform/kobo/disable-wifi.sh`
//! * [`restore_wifi_script`] ← `platform/kobo/restore-wifi-async.sh`
//!
//! The lifecycle KOReader uses, which we follow exactly:
//!   - **Before suspend** (`Kobo:suspend` → `disableWifi`): release the lease,
//!     terminate wpa_supplicant, drop the interface and power the chip **off**.
//!     The chip is off for the whole sleep; the modules stay loaded.
//!   - **After resume** (`restoreWifiAsync`): run `enable-wifi.sh` (which on the
//!     MTK chip re-runs the firmware power-on dance **every time** — the chip
//!     loses its state across a power-off, so this is never gated — and starts a
//!     fresh wpa_supplicant), wait for `wpa_state=COMPLETED`, then `obtain-ip`.
//!     If association doesn't complete in ~15s, tear Wi-Fi back down.
//!
//! What we are NOT doing any more (these were gideon-only embellishments that
//! left the radio — and Nickel, after we exit — in a wedged state): power-cycling
//! the chip at *resume* instead of suspend, `pkill`-ing and `rm`-ing the
//! supplicant control socket on every bring-up, and kicking explicit
//! `wpa_cli scan`/`reconnect`/`reassociate`. KOReader does none of that; a fresh
//! `-s` supplicant re-associates on its own and we just wait for it.
//!
//! Everything is best-effort and only runs on a real device: off-device
//! (desktop/CI) the Kobo marker `/mnt/onboard` is absent, so every call no-ops
//! and [`is_online`] reports "online" so tests never try to manage Wi-Fi.
//! Device facts (module name, kmod dir, supplicant config) are overridable by
//! env, since KOReader reads them from Nickel's environment too.

use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// How long to wait for association + a DHCP lease before giving up. KOReader
/// allows up to ~45s; we use a tighter window so a genuinely-unavailable
/// network surfaces the "no network" message reasonably soon.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// Connectivity poll cadence while waiting (KOReader's `scheduleConnectivityCheck`).
const POLL_INTERVAL: Duration = Duration::from_millis(250);

/// Guards against stacking restore campaigns when several wakes land close
/// together — only one background restore runs at a time.
static RECONNECTING: AtomicBool = AtomicBool::new(false);

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

/// `GIDEON_WIFI_AUTOENABLE=0` opts out of all automatic Wi-Fi bring-up.
fn autoenable_off() -> bool {
    std::env::var("GIDEON_WIFI_AUTOENABLE").as_deref() == Ok("0")
}

/// Whether we're on a Kobo, independent of Wi-Fi state. We can't key this on
/// the interface existing: with the radio fully off the MTK module may be
/// unloaded and the interface absent — which is exactly the cold case we must
/// still bring up. `/mnt/onboard` (Kobo's user partition) is present whatever
/// the radio is doing; off-device (desktop/CI) it's absent and every op here
/// no-ops. `GIDEON_WIFI_FORCE=1` forces on (for on-desktop testing).
fn on_device() -> bool {
    std::env::var("GIDEON_WIFI_FORCE").as_deref() == Ok("1") || Path::new("/mnt/onboard").is_dir()
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
/// `ifHasAnAddress`). `Some(false)` = ran the check and there's no address;
/// `None` = couldn't determine (the `ifconfig` subprocess failed to run).
/// The distinction matters: a fork failure must NOT be read as "offline" and
/// trigger a disruptive bring-up on a working link.
fn has_ipv4(iface: &str) -> Option<bool> {
    let out = Command::new("ifconfig").arg(iface).output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    // busybox: "inet addr:192.168…"; iproute2/newer: "inet 192.168…".
    Some(text.contains("inet addr:") || text.contains("inet "))
}

/// True when we have a usable connection. Off-device we report online so
/// nothing tries to manage Wi-Fi where there's none. On-device: operstate
/// must be `up` (a reliable sysfs read), AND the interface must have an IPv4
/// — but if we *can't determine* the address (subprocess failed), we trust
/// operstate and stay "online" rather than disturb a possibly-working link.
pub fn is_online() -> bool {
    if !on_device() {
        return true;
    }
    let iface = interface();
    operstate_up(&iface) && has_ipv4(&iface).unwrap_or(true)
}

/// Bring Wi-Fi up on demand (a network action found no connection): KOReader's
/// `turnOnWifi` → enable + wait-for-association + obtain-ip, run detached so the
/// UI stays responsive and can poll [`is_online`]. No-op off-device or when
/// auto-enable is opted out.
pub fn bring_up_wifi() {
    if !on_device() || autoenable_off() {
        return;
    }
    run_detached(&restore_wifi_script(&interface()));
}

/// Rejoin the network after waking from sleep — KOReader's `restoreWifiAsync`.
/// Suspend left the chip powered off and wpa_supplicant terminated (see
/// [`disable_wifi`]); this runs the full enable + wait-for-`COMPLETED` +
/// obtain-ip, exactly like `restore-wifi-async.sh`. Runs in a guarded
/// background thread so only one restore runs at a time and the UI is
/// responsive immediately. No-op off-device or when auto-enable is opted out.
pub fn reconnect_after_wake() {
    if !on_device() || autoenable_off() {
        return;
    }
    // One restore at a time (back-to-back wakes / debounce).
    if RECONNECTING.swap(true, Ordering::SeqCst) {
        return;
    }
    let spawned = std::thread::Builder::new()
        .name("gideon-wifi-restore".into())
        .spawn(|| {
            run_blocking(&restore_wifi_script(&interface()));
            RECONNECTING.store(false, Ordering::SeqCst);
        });
    if spawned.is_err() {
        // Couldn't spawn — fall back to a detached one-shot.
        RECONNECTING.store(false, Ordering::SeqCst);
        run_detached(&restore_wifi_script(&interface()));
    }
}

/// Take Wi-Fi fully down — KOReader's `disable-wifi.sh` for the wmt chip:
/// release the lease, terminate wpa_supplicant, drop the interface and power
/// the chip off (modules stay loaded). Used both by the in-app "turn Wi-Fi
/// off" control and by the suspend path (the chip is off for the whole sleep).
/// Blocks briefly; no-op off-device.
pub fn disable_wifi() {
    if !on_device() {
        return;
    }
    run_blocking(&disable_wifi_script(&interface()));
}

/// Run a script **detached in the background** (`( … ) &`) so `sh` returns at
/// once and slow steps (chip wake, a DHCP wait) continue reparented to init.
fn run_detached(script: &str) {
    let detached = format!("( {script} ) </dev/null >/dev/null 2>&1 &");
    let _ = Command::new("sh").arg("-c").arg(detached).status();
}

/// Run a script and wait for it to finish (used from background threads that
/// want to know when the work is done, e.g. to release the restore guard).
fn run_blocking(script: &str) {
    let _ = Command::new("sh").arg("-c").arg(script).status();
}

/// A nearby Wi-Fi network discovered by a scan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WifiNetwork {
    pub ssid: String,
    /// Signal strength in dBm (closer to 0 = stronger).
    pub signal: i32,
    /// Needs a password (WPA/WPA2/WEP).
    pub secured: bool,
    /// wpa_supplicant already has saved credentials for it.
    pub saved: bool,
    /// Currently associated with it.
    pub connected: bool,
}

impl WifiNetwork {
    /// Signal as 0–4 bars, for the UI.
    pub fn bars(&self) -> u8 {
        match self.signal {
            s if s >= -55 => 4,
            s if s >= -65 => 3,
            s if s >= -75 => 2,
            s if s >= -85 => 1,
            _ => 0,
        }
    }
}

/// The wpa_supplicant control socket directory (matches the bring-up scripts).
const WPA_CTRL: &str = "/var/run/wpa_supplicant";

/// Run `wpa_cli` against the supplicant on `iface`; stdout, or `None` if it
/// couldn't run.
fn wpa_cli(iface: &str, args: &[&str]) -> Option<String> {
    let out = Command::new("wpa_cli")
        .args(["-i", iface, "-p", WPA_CTRL])
        .args(args)
        .output()
        .ok()?;
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Parse `wpa_cli scan_results` (tab-separated `bssid / freq / signal / flags
/// / ssid`, with a header line) into `(ssid, signal_dbm, secured)`. Header,
/// blank and hidden (SSID-less) rows are dropped. Pure, for testing.
fn parse_scan_results(output: &str) -> Vec<(String, i32, bool)> {
    output
        .lines()
        .skip(1)
        .filter_map(|line| {
            let mut f = line.split('\t');
            let _bssid = f.next()?;
            let _freq = f.next()?;
            let signal = f.next()?.trim().parse::<i32>().ok()?;
            let flags = f.next()?;
            let ssid = f.next()?.trim();
            if ssid.is_empty() {
                return None;
            }
            let secured = flags.contains("WPA") || flags.contains("WEP") || flags.contains("RSN");
            Some((ssid.to_string(), signal, secured))
        })
        .collect()
}

/// The currently-associated SSID, if any (`wpa_cli status`).
fn current_ssid(iface: &str) -> Option<String> {
    wpa_cli(iface, &["status"])?
        .lines()
        .find_map(|l| l.strip_prefix("ssid=").map(str::to_string))
}

/// SSIDs wpa_supplicant has saved (`wpa_cli list_networks`, tab `id / ssid`).
fn saved_ssids(iface: &str) -> Vec<String> {
    wpa_cli(iface, &["list_networks"])
        .into_iter()
        .flat_map(|out| {
            out.lines()
                .skip(1)
                .filter_map(|l| l.split('\t').nth(1).map(|s| s.trim().to_string()))
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
        })
        .collect()
}

/// The saved-network id for `ssid`, if wpa_supplicant has it.
fn saved_network_id(iface: &str, ssid: &str) -> Option<String> {
    wpa_cli(iface, &["list_networks"])?
        .lines()
        .skip(1)
        .find_map(|l| {
            let mut f = l.split('\t');
            let id = f.next()?.trim();
            let s = f.next()?.trim();
            (s == ssid).then(|| id.to_string())
        })
}

/// Scan for nearby networks: trigger a scan, wait briefly, parse the results
/// and annotate each with saved/connected state — strongest signal per SSID,
/// connected one first. Best-effort: empty off-device or when wpa_cli is
/// unavailable.
pub fn scan_networks() -> Vec<WifiNetwork> {
    if !on_device() {
        return Vec::new();
    }
    let iface = interface();
    let _ = wpa_cli(&iface, &["scan"]);
    std::thread::sleep(Duration::from_secs(3));
    let results = wpa_cli(&iface, &["scan_results"]).unwrap_or_default();
    let saved = saved_ssids(&iface);
    let connected = current_ssid(&iface);

    let mut best: std::collections::BTreeMap<String, (i32, bool)> =
        std::collections::BTreeMap::new();
    for (ssid, signal, secured) in parse_scan_results(&results) {
        let e = best.entry(ssid).or_insert((i32::MIN, secured));
        if signal > e.0 {
            *e = (signal, secured);
        }
    }
    let mut nets: Vec<WifiNetwork> = best
        .into_iter()
        .map(|(ssid, (signal, secured))| WifiNetwork {
            saved: saved.iter().any(|s| s == &ssid),
            connected: connected.as_deref() == Some(ssid.as_str()),
            ssid,
            signal,
            secured,
        })
        .collect();
    nets.sort_by(|a, b| b.connected.cmp(&a.connected).then(b.signal.cmp(&a.signal)));
    nets
}

/// Connect to `ssid` (`password = None` for an open or already-saved network):
/// reuse the saved network or add a fresh one, save it, and kick DHCP in the
/// background. The caller polls [`is_online`] for the result. No-op off-device.
pub fn connect_network(ssid: &str, password: Option<&str>) -> bool {
    if !on_device() {
        return false;
    }
    let iface = interface();
    // wpa_cli wants string values in their quoted form ("ssid"); args are
    // passed literally (no shell), so the quotes are part of the value.
    let id = match saved_network_id(&iface, ssid) {
        Some(id) => id,
        None => {
            let Some(id) = wpa_cli(&iface, &["add_network"]).map(|s| s.trim().to_string()) else {
                return false;
            };
            wpa_cli(
                &iface,
                &["set_network", &id, "ssid", &format!("\"{ssid}\"")],
            );
            match password {
                Some(p) => {
                    wpa_cli(&iface, &["set_network", &id, "psk", &format!("\"{p}\"")]);
                }
                None => {
                    wpa_cli(&iface, &["set_network", &id, "key_mgmt", "NONE"]);
                }
            }
            id
        }
    };
    wpa_cli(&iface, &["enable_network", &id]);
    wpa_cli(&iface, &["select_network", &id]);
    wpa_cli(&iface, &["save_config"]);
    // Background DHCP so the UI stays responsive and can poll for the lease.
    let dhcp = format!(
        "( dhcpcd -k {iface} 2>/dev/null || : ; dhcpcd -t 30 -w {iface} 2>/dev/null \
         || udhcpc -i {iface} -t 5 -n -q 2>/dev/null || : ) </dev/null >/dev/null 2>&1 &"
    );
    Command::new("sh").arg("-c").arg(dhcp).status().is_ok()
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

/// Resolve the MTK module name, kernel-module dir and wpa_supplicant config.
/// KOReader sources these from Nickel's environment; the defaults are the
/// Libra Colour (monza) values. Each is overridable by env.
fn wifi_env() -> (String, String, String) {
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
    (module, kmod_dir, conf)
}

/// KOReader's `enable-wifi.sh` for the wmt (Libra Colour) chip: load the
/// modules if cold (they're loaded once and never unloaded), run the `wmt_dbg`
/// firmware power-on dance **unconditionally** (the chip loses this state across
/// a power-off, which is why KOReader never gates it), power the chip on, bring
/// the interface up, and start a wpa_supplicant if one isn't already running.
/// We do NOT kick an explicit scan/reconnect — a fresh `-s` supplicant
/// re-associates against the saved networks on its own; the caller waits for it.
fn enable_wifi_script(iface: &str) -> String {
    let (module, kmod_dir, conf) = wifi_env();
    format!(
        r#"sleep_ms() {{ usleep "$1"000 2>/dev/null || sleep 1; }}
for m in wmt_drv wmt_chrdev_wifi wmt_cdev_bt {module}; do
  grep -q "^${{m}} " /proc/modules 2>/dev/null || {{ insmod {kmod_dir}/${{m}}.ko 2>/dev/null || :; sleep_ms 250; }}
done
echo 0xDB9DB9 > /proc/driver/wmt_dbg 2>/dev/null || :
echo "7 9 0"  > /proc/driver/wmt_dbg 2>/dev/null || :
sleep 1
echo 0xDB9DB9 > /proc/driver/wmt_dbg 2>/dev/null || :
echo "7 9 1"  > /proc/driver/wmt_dbg 2>/dev/null || :
echo 1 > /dev/wmtWifi 2>/dev/null || :
sleep_ms 250
sleep 1
ifconfig {iface} up 2>/dev/null || :
pkill -0 wpa_supplicant 2>/dev/null || \
  wpa_supplicant -D nl80211 -s -i {iface} -c {conf} -C /var/run/wpa_supplicant -B 2>/dev/null || :
"#
    )
}

/// KOReader's `obtain-ip.sh` (with `release-ip.sh` inlined): drop any existing
/// lease first, then acquire a fresh one with dhcpcd (Nickel's choice; udhcpc
/// fallback). Releasing first mirrors KOReader and avoids a second dhcpcd
/// fighting a stale lease.
fn obtain_ip_script(iface: &str) -> String {
    format!(
        r#"dhcpcd -d -k {iface} 2>/dev/null || :
killall -q -TERM udhcpc 2>/dev/null || :
if command -v dhcpcd >/dev/null 2>&1; then
  dhcpcd -d -t 30 -w {iface} 2>/dev/null || :
else
  udhcpc -i {iface} -t 5 -T 3 -n -q 2>/dev/null || :
fi
"#
    )
}

/// KOReader's `disable-wifi.sh` for the wmt chip: release the lease, terminate
/// wpa_supplicant, drop the interface, and power the chip off. The modules are
/// left loaded (KOReader's `SKIP_UNLOAD` for `wlan_drv_gen4m`).
fn disable_wifi_script(iface: &str) -> String {
    format!(
        r#"dhcpcd -d -k {iface} 2>/dev/null || :
killall -q -TERM udhcpc 2>/dev/null || :
wpa_cli -i {iface} -p /var/run/wpa_supplicant terminate 2>/dev/null || :
ifconfig {iface} down 2>/dev/null || :
echo 0 > /dev/wmtWifi 2>/dev/null || :
"#
    )
}

/// KOReader's `restore-wifi-async.sh`: enable Wi-Fi, then wait for
/// wpa_supplicant to actually reach `wpa_state=COMPLETED` before asking for a
/// lease (so dhcpcd doesn't broadcast into an un-associated link). If
/// association doesn't complete within ~15s (60 × 0.25s), tear Wi-Fi back down
/// and bail — the next on-demand bring-up will try again from a clean state.
fn restore_wifi_script(iface: &str) -> String {
    let enable = enable_wifi_script(iface);
    let obtain = obtain_ip_script(iface);
    let disable = disable_wifi_script(iface);
    format!(
        r#"{enable}
wpac_timeout=0
while ! wpa_cli -i {iface} -p /var/run/wpa_supplicant status 2>/dev/null | grep -q "wpa_state=COMPLETED"; do
  if [ $wpac_timeout -ge 60 ]; then
{disable}
    exit 1
  fi
  usleep 250000 2>/dev/null || sleep 1
  wpac_timeout=$((wpac_timeout + 1))
done
{obtain}"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interface_prefers_env_then_falls_back_to_eth0() {
        // Default (no env set in this process) is eth0.
        std::env::remove_var("GIDEON_WIFI_INTERFACE");
        std::env::remove_var("INTERFACE");
        assert_eq!(interface(), "eth0");
    }

    #[test]
    fn off_device_is_treated_as_online() {
        // CI/desktop has no /mnt/onboard, so Wi-Fi management is skipped and
        // we report online (never try to bring Wi-Fi up where there's none).
        assert!(!on_device(), "CI/desktop must not be detected as a Kobo");
        assert!(is_online(), "off-device short-circuits to online");
    }

    #[test]
    fn has_ipv4_never_claims_an_address_for_a_bogus_interface() {
        // Either Some(false) (ifconfig ran, no inet) or None (ifconfig absent
        // in CI) — but never Some(true). The None case is precisely why
        // is_online() trusts operstate instead of declaring offline.
        assert_ne!(has_ipv4("definitely-not-a-real-iface-zzz"), Some(true));
    }

    #[test]
    fn enable_runs_the_firmware_init_unconditionally_like_koreader() {
        let s = enable_wifi_script("eth0");
        // Module load, the wmt_dbg firmware dance, chip power-on, interface up,
        // and a wpa_supplicant — KOReader's enable-wifi.sh, in order.
        assert!(s.contains("wlan_drv_gen4m"));
        assert!(s.contains("0xDB9DB9"), "runs the wmt_dbg firmware dance");
        assert!(s.contains("echo 1 > /dev/wmtWifi"), "powers the chip on");
        assert!(s.contains("ifconfig eth0 up"));
        assert!(s.contains("wpa_supplicant -D nl80211 -s -i eth0"));
        // The firmware dance is UNGATED — KOReader re-runs it on every enable,
        // because the chip loses its state across a power-off/suspend. This is
        // the bug the old `[ ! -e /dev/wmtWifi ]` gate caused.
        assert!(
            !s.contains("! -e /dev/wmtWifi"),
            "firmware init must not be gated on the control node"
        );
        // KOReader does NOT kick an explicit scan/reconnect/reassociate, nor
        // kill/rm the supplicant socket — a fresh -s supplicant reconnects.
        assert!(!s.contains("reassociate"));
        assert!(!s.contains("pkill wpa_supplicant"));
        assert!(
            !s.contains("echo 0 > /dev/wmtWifi"),
            "enable never powers off"
        );
        let dance = s.find("0xDB9DB9").unwrap();
        let power_on = s.find("echo 1 > /dev/wmtWifi").unwrap();
        let ifup = s.find("ifconfig eth0 up").unwrap();
        assert!(
            dance < power_on && power_on < ifup,
            "order: firmware dance, power on, ifup"
        );
    }

    #[test]
    fn disable_terminates_powers_off_and_keeps_modules() {
        // KOReader disable-wifi.sh for wmt: release, terminate, ifdown, power
        // off — and crucially never rmmod (SKIP_UNLOAD for wlan_drv_gen4m).
        let s = disable_wifi_script("eth0");
        assert!(s.contains("dhcpcd -d -k eth0"), "releases the lease");
        assert!(s.contains("terminate"), "terminates wpa_supplicant");
        assert!(s.contains("ifconfig eth0 down"));
        assert!(s.contains("echo 0 > /dev/wmtWifi"), "powers the chip off");
        assert!(!s.contains("rmmod"), "modules stay loaded on the wmt chip");
    }

    #[test]
    fn obtain_ip_releases_before_acquiring() {
        let s = obtain_ip_script("eth0");
        let release = s.find("dhcpcd -d -k eth0").unwrap();
        let acquire = s.find("dhcpcd -d -t 30 -w eth0").unwrap();
        assert!(
            release < acquire,
            "release the stale lease before acquiring"
        );
    }

    #[test]
    fn restore_enables_then_waits_for_completed_then_obtains_ip() {
        let s = restore_wifi_script("eth0");
        // enable (firmware dance) → wait for COMPLETED → DHCP, in that order.
        let dance = s.find("0xDB9DB9").unwrap();
        let completed = s.find("wpa_state=COMPLETED").unwrap();
        let acquire = s.find("dhcpcd -d -t 30 -w eth0").unwrap();
        assert!(
            dance < completed && completed < acquire,
            "order: enable, wait for association, then DHCP"
        );
        // On a failed association it tears Wi-Fi down (KOReader's behaviour):
        // the disable's power-off appears (in the timeout branch).
        assert!(
            s.contains("echo 0 > /dev/wmtWifi"),
            "tears Wi-Fi down if association never completes"
        );
    }

    #[test]
    fn scripts_honor_module_and_iface_overrides() {
        std::env::set_var("GIDEON_WIFI_MODULE", "moal");
        let s = enable_wifi_script("wlan0");
        assert!(s.contains("moal"));
        assert!(s.contains("ifconfig wlan0 up"));
        std::env::remove_var("GIDEON_WIFI_MODULE");
    }

    #[test]
    fn parse_scan_results_extracts_networks() {
        // Real `wpa_cli scan_results` shape: header + tab-separated rows.
        let out = "bssid / frequency / signal level / flags / ssid\n\
            00:11:22:33:44:55\t2412\t-45\t[WPA2-PSK-CCMP][ESS]\tHomeNet\n\
            66:77:88:99:aa:bb\t5180\t-72\t[ESS]\tCoffeeShop\n\
            cc:dd:ee:ff:00:11\t2437\t-60\t[WEP][ESS]\tOldRouter\n\
            22:33:44:55:66:77\t2462\t-80\t[WPA2-PSK-CCMP][ESS]\t\n";
        let nets = parse_scan_results(out);
        assert_eq!(nets.len(), 3, "the hidden (SSID-less) row is dropped");
        assert_eq!(nets[0], ("HomeNet".to_string(), -45, true));
        assert_eq!(
            nets[1],
            ("CoffeeShop".to_string(), -72, false),
            "open network"
        );
        assert_eq!(
            nets[2],
            ("OldRouter".to_string(), -60, true),
            "WEP counts as secured"
        );
    }

    #[test]
    fn signal_maps_to_bars() {
        let net = |dbm| WifiNetwork {
            ssid: "x".into(),
            signal: dbm,
            secured: false,
            saved: false,
            connected: false,
        };
        assert_eq!(net(-40).bars(), 4);
        assert_eq!(net(-60).bars(), 3);
        assert_eq!(net(-70).bars(), 2);
        assert_eq!(net(-80).bars(), 1);
        assert_eq!(net(-95).bars(), 0);
    }
}

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
//! it only runs on a real device — off-device (desktop/CI) the Kobo marker
//! `/mnt/onboard` is absent, so every call is a no-op and reports "online" so
//! tests never try to manage Wi-Fi.
//!
//! Device-specific facts are sourced from KOReader (`platform/kobo/
//! enable-wifi.sh`, `obtain-ip.sh`; `frontend/ui/network/manager.lua`'s
//! `ifHasAnAddress`/`connectivityCheck`) and are overridable by environment
//! variable, since KOReader itself reads them from Nickel's environment rather
//! than hardcoding them per codename.

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

/// Post-wake reconnect: how many bring-up attempts, and how long to give each
/// one to associate + get a lease before re-kicking. The chip can take a few
/// seconds to come back after suspend and the first associate often misses, so
/// a single shot isn't enough — keep trying for roughly a minute. The first
/// attempt is a full power-cycle (see [`Mode::Cold`]) which needs more headroom
/// than a warm re-associate.
const WAKE_RECONNECT_ATTEMPTS: u32 = 4;
const WAKE_RECONNECT_ATTEMPT: Duration = Duration::from_secs(20);

/// How hard a bring-up tries. The on-demand path reuses whatever Nickel/our
/// previous bring-up left running; the post-wake path power-cycles from scratch.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// Reuse a loaded module, a powered chip and a live wpa_supplicant: just
    /// re-scan / re-associate / renew the lease. Cheap; the on-demand path.
    Warm,
    /// A genuine radio power-cycle — interface down, chip *off*, supplicant
    /// killed, lease released, then the full firmware init and a fresh
    /// supplicant on the way back up. This is exactly the off→on that Nickel's
    /// Wi-Fi toggle performs, and the only thing that reliably recovers the
    /// link after a suspend: a warm re-associate leaves the MTK chip powered
    /// but with stale firmware/association state (the control node persists
    /// across suspend, so the warm path's `[ ! -e /dev/wmtWifi ]` guard skips
    /// the re-init the chip actually needs). Used as the first post-wake try.
    Cold,
}

/// Guards against stacking reconnect campaigns when several wakes land close
/// together — only one background reconnect runs at a time.
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

/// Fire the best-effort bring-up sequence (modules → power → ifup →
/// wpa_supplicant → scan/re-associate → DHCP) and return **immediately**.
/// Returns without doing anything if off-device or opted out via
/// `GIDEON_WIFI_AUTOENABLE=0`. The sequence is run **detached in the
/// background** (`( … ) &`) — its slow steps (the chip waking up, a DHCP
/// wait of up to 30s) must NOT block the caller, so the UI can show a
/// cancellable "Connecting…" status and poll [`is_online`] for the result.
/// Failures of individual steps are swallowed (the device may already be
/// partway up).
pub fn bring_up_wifi() {
    if !on_device() || std::env::var("GIDEON_WIFI_AUTOENABLE").as_deref() == Ok("0") {
        return;
    }
    run_enable_detached(&interface(), Mode::Warm);
}

/// Run a bring-up [`enable_script`] **detached in the background** so `sh`
/// returns at once and the slow steps (chip wake, a DHCP wait of up to 30s)
/// continue reparented to init. A lease that lands after the UI stops waiting
/// still leaves the device online for the next action.
fn run_enable_detached(iface: &str, mode: Mode) {
    let detached = format!(
        "( {} ) </dev/null >/dev/null 2>&1 &",
        enable_script(iface, mode)
    );
    let _ = Command::new("sh").arg("-c").arg(detached).status();
}

/// Rejoin a known network after the device wakes from sleep. A suspend leaves
/// the MTK radio dead-but-powered, so the first thing we do is a full cold
/// power-cycle ([`Mode::Cold`]) — the same off→on as Nickel's Wi-Fi toggle,
/// the one recovery the user can see working — rather than a warm re-associate
/// against a half-dead chip. Runs in a detached background thread (the campaign
/// can span ~a minute) so the UI is responsive immediately; subsequent attempts
/// are warm and stop the instant the link is genuinely back. No-op off-device
/// or when auto-enable is opted out (`GIDEON_WIFI_AUTOENABLE=0`).
pub fn reconnect_after_wake() {
    if !on_device() || std::env::var("GIDEON_WIFI_AUTOENABLE").as_deref() == Ok("0") {
        return;
    }
    // Only one reconnect campaign at a time (back-to-back wakes, debounce).
    if RECONNECTING.swap(true, Ordering::SeqCst) {
        return;
    }
    let spawned = std::thread::Builder::new()
        .name("gideon-wifi-reconnect".into())
        .spawn(|| {
            let iface = interface();
            // The first attempt is ALWAYS a cold power-cycle, run
            // unconditionally — we must not trust is_online() before it. After
            // a suspend the radio is dead, yet the pre-sleep IPv4 address
            // survives the interface down/up, so is_online() reports a stale
            // "connected" and a warm-only campaign would no-op exactly when a
            // real reset is needed. Later attempts are warm re-associates and
            // bail the instant the link is genuinely back.
            'campaign: for attempt in 0..WAKE_RECONNECT_ATTEMPTS {
                if attempt > 0 && is_online() {
                    break;
                }
                let mode = if attempt == 0 { Mode::Cold } else { Mode::Warm };
                run_enable_detached(&iface, mode);
                let start = Instant::now();
                while start.elapsed() < WAKE_RECONNECT_ATTEMPT {
                    std::thread::sleep(POLL_INTERVAL);
                    if is_online() {
                        break 'campaign;
                    }
                }
            }
            RECONNECTING.store(false, Ordering::SeqCst);
        });
    if spawned.is_err() {
        // Couldn't spawn — fall back to a single inline cold reset.
        RECONNECTING.store(false, Ordering::SeqCst);
        run_enable_detached(&interface(), Mode::Cold);
    }
}

/// Turn Wi-Fi off to save battery: drop the interface and power the chip
/// down. Best-effort and no-op off-device. The module is left loaded (a warm
/// re-enable is then fast). The user re-enables from the Wi-Fi controls.
pub fn disable_wifi() {
    if !on_device() {
        return;
    }
    let iface = interface();
    let script =
        format!("ifconfig {iface} down 2>/dev/null || :; echo 0 > /dev/wmtWifi 2>/dev/null || :");
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

/// The wpa_supplicant control socket path (matches [`enable_script`]).
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

/// The MTK (monza, `wlan_drv_gen4m`) Wi-Fi enable sequence as a shell script,
/// ported from KOReader's `platform/kobo/enable-wifi.sh` + `obtain-ip.sh`.
/// Module name, kernel-module directory and wpa_supplicant config are
/// overridable by env (KOReader sources these from Nickel's environment); the
/// defaults are the Libra Colour values.
fn enable_script(iface: &str, mode: Mode) -> String {
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

    let cold = mode == Mode::Cold;
    // Cold start = a genuine radio power-cycle (Nickel's Wi-Fi toggle), the
    // only thing that recovers the link after suspend. Tear everything down
    // first — interface, chip power, supplicant and its stale control socket,
    // and any half-acquired lease — so the bring-up below rebuilds from a clean
    // slate instead of poking a half-dead chip. Warm start skips all of this.
    let teardown = if cold {
        format!(
            "ifconfig {iface} down 2>/dev/null || :\n\
             echo 0 > /dev/wmtWifi 2>/dev/null || :\n\
             pkill wpa_supplicant 2>/dev/null || :\n\
             rm -f /var/run/wpa_supplicant/{iface} 2>/dev/null || :\n\
             dhcpcd -k {iface} 2>/dev/null || :\n\
             sleep 1\n"
        )
    } else {
        String::new()
    };
    // The control node persists across suspend even after the chip lost its
    // firmware, so `[ ! -e /dev/wmtWifi ]` alone won't re-init a woken chip.
    // Cold forces the firmware (wmt_dbg) init regardless; warm only inits a
    // truly cold node (first-ever bring-up). `true`/`false` are shell builtins.
    let force_init = if cold { "true" } else { "false" };

    // Each step is best-effort (`|| :`); insmod only when the module isn't
    // already loaded, and the module block is skipped once the chip control
    // node exists (warm start — the common on-demand reconnect case).
    format!(
        r#"
sleep_ms() {{ usleep "$1"000 2>/dev/null || sleep 1; }}
{teardown}if [ ! -e /dev/wmtWifi ]; then
  for m in wmt_drv wmt_chrdev_wifi wmt_cdev_bt {module}; do
    if ! grep -q "^${{m}} " /proc/modules 2>/dev/null; then
      insmod {kmod_dir}/${{m}}.ko 2>/dev/null || :
      sleep_ms 250
    fi
  done
fi
if [ ! -e /dev/wmtWifi ] || {force_init}; then
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
# Actively kick a scan + (re)association instead of passively hoping the
# radio reconnects on its own — the chip needs a moment to come up after
# sleep, and a supplicant that's already running may be sitting idle. Give it
# a beat to scan, then re-associate against the saved networks.
if command -v wpa_cli >/dev/null 2>&1; then
  wpa_cli -i {iface} -p /var/run/wpa_supplicant scan 2>/dev/null || :
  sleep 2
  # `reconnect` re-enables every saved network and connects to the best one
  # (the disconnected case); `reassociate` re-associates if it's idling on one
  # already. Run both, like KOReader, so either state recovers.
  wpa_cli -i {iface} -p /var/run/wpa_supplicant reconnect 2>/dev/null || :
  wpa_cli -i {iface} -p /var/run/wpa_supplicant reassociate 2>/dev/null || :
  # Wait for association to actually COMPLETE before asking for a lease —
  # KOReader's restore-wifi-async.sh does the same. Otherwise dhcpcd broadcasts
  # DISCOVERs into a still-unassociated link and burns its timeout before the
  # radio is even up. Bounded (~10s) so the detached campaign can't wedge; an
  # attempt that never associates falls through to DHCP (which then times out)
  # and the outer retry re-kicks.
  i=0
  while [ $i -lt 40 ]; do
    wpa_cli -i {iface} -p /var/run/wpa_supplicant status 2>/dev/null \
      | grep -q "wpa_state=COMPLETED" && break
    sleep_ms 250
    i=$((i + 1))
  done
fi
if command -v dhcpcd >/dev/null 2>&1; then
  # Release any stale/contending lease first (Nickel's dhcpcd is left alive
  # by the launcher, so a bare second dhcpcd would fight it and hang) — this
  # mirrors KOReader's release-ip.sh before obtain-ip.sh. Only reached when
  # we're already offline, so there's no good lease to protect.
  dhcpcd -k {iface} 2>/dev/null || :
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
    fn enable_script_has_the_monza_sequence() {
        let s = enable_script("eth0", Mode::Warm);
        // Module loads, chip power-on, interface up, supplicant, DHCP — in
        // the KOReader order, on the resolved interface.
        assert!(s.contains("wlan_drv_gen4m"));
        assert!(s.contains("/dev/wmtWifi"));
        assert!(s.contains("ifconfig eth0 up"));
        assert!(s.contains("wpa_supplicant -D nl80211 -s -i eth0"));
        assert!(s.contains("dhcpcd -t 30 -w eth0"));
        // Actively kicks a scan + reconnect + re-association rather than
        // passively waiting.
        assert!(s.contains("wpa_cli -i eth0"));
        assert!(s.contains("reconnect"));
        assert!(s.contains("reassociate"));
        // Release a stale/contending lease before re-acquiring (KOReader's
        // release-ip.sh before obtain-ip.sh).
        assert!(s.contains("dhcpcd -k eth0"));
        // Wait for association to COMPLETE before DHCP (KOReader's
        // restore-wifi-async.sh), so dhcpcd doesn't broadcast into a dead link.
        assert!(s.contains("wpa_state=COMPLETED"));
        let drv = s.find("/dev/wmtWifi").unwrap();
        let ifup = s.find("ifconfig eth0 up").unwrap();
        let release = s.find("dhcpcd -k eth0").unwrap();
        let acquire = s.find("dhcpcd -t 30 -w eth0").unwrap();
        let associated = s.find("wpa_state=COMPLETED").unwrap();
        assert!(
            drv < ifup && ifup < release && release < acquire,
            "steps must be ordered: power, ifup, release, acquire"
        );
        assert!(
            associated < acquire,
            "must wait for association before acquiring a lease"
        );
    }

    #[test]
    fn warm_start_does_not_power_cycle_or_force_init() {
        // The on-demand path reuses a running radio: no chip power-off, no
        // supplicant kill, and the firmware init stays gated on a cold node.
        let s = enable_script("eth0", Mode::Warm);
        assert!(!s.contains("echo 0 > /dev/wmtWifi"), "warm must not power off");
        assert!(!s.contains("pkill wpa_supplicant"), "warm must not kill supplicant");
        assert!(
            s.contains("[ ! -e /dev/wmtWifi ] || false"),
            "warm only inits a truly cold node"
        );
    }

    #[test]
    fn cold_start_power_cycles_the_radio_and_forces_init() {
        // The post-wake path is a full off→on (Nickel's toggle): tear the
        // interface, chip power, supplicant, its control socket and the lease
        // down first, then force the firmware re-init on the way back up.
        let s = enable_script("eth0", Mode::Cold);
        assert!(s.contains("ifconfig eth0 down"));
        assert!(s.contains("echo 0 > /dev/wmtWifi"), "powers the chip off");
        assert!(s.contains("pkill wpa_supplicant"), "kills the stale supplicant");
        assert!(
            s.contains("rm -f /var/run/wpa_supplicant/eth0"),
            "clears the stale control socket"
        );
        assert!(
            s.contains("[ ! -e /dev/wmtWifi ] || true"),
            "cold forces the firmware re-init"
        );
        // Teardown must come before the bring-up powers the chip back on.
        let power_off = s.find("echo 0 > /dev/wmtWifi").unwrap();
        let power_on = s.find("echo 1 > /dev/wmtWifi").unwrap();
        assert!(power_off < power_on, "power off, then back on");
    }

    #[test]
    fn enable_script_honors_module_and_iface_overrides() {
        std::env::set_var("GIDEON_WIFI_MODULE", "moal");
        let s = enable_script("wlan0", Mode::Warm);
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

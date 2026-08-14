//! Bluetooth power across suspend, for the MTK family (Libra Colour).
//!
//! A suspend powers the BT subsystem down with no clean shutdown, and on MTK
//! the chip does not come back usable on resume — the paired page-turn remote
//! never reconnects until the whole stack is restarted. So we mirror the
//! Wi-Fi suspend dance at the Bluetooth layer, using the same mechanism the
//! field-tested kobo.koplugin uses on these devices
//! (`src/lib/bluetooth/adapters/mtk_adapter.lua`): D-Bus calls to Kobo's own
//! `com.kobo.mtk.bluedroid` service, which owns the MTK Bluetooth stack and
//! keeps running while Nickel is stopped.
//!
//! - **Before suspend** ([`power_down_for_suspend`]): if the adapter is
//!   powered, shut it down cleanly (`Adapter1.Powered=false`, then
//!   `BluedroidManager1.Off`) and remember that it was on.
//! - **After wake** ([`reconnect_after_wake`]): if it was on, bring it back
//!   (`BluedroidManager1.On`, `Powered=true`) and ask BlueZ to reconnect
//!   every paired device (`Device1.Connect`), in a background thread so the
//!   UI is responsive immediately. The remote's evdev node then reappears
//!   and the input layer's inotify hotplug picks it up.
//!
//! The MTK Bluetooth stack shares the radio with Wi-Fi and needs the Wi-Fi
//! chip powered to come up (kobo.koplugin gates its BT enable on Wi-Fi for
//! this reason), so the caller kicks `network::reconnect_after_wake` whenever
//! a Bluetooth resume is pending, and the enable below retries while that
//! bring-up completes.
//!
//! This is deliberately NOT radio manipulation in the PR #121/#122 sense: no
//! rfkill, no sysfs power writes, no hci tools — only the stack's own D-Bus
//! management interface, and only around *suspend* (never on the exit path).
//! Everything is best-effort: off-device, with `GIDEON_SUSPEND_BT=0`, or on
//! a device without the bluedroid service (non-MTK), every call no-ops.

use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// The Kobo MTK Bluetooth management service (bus name it owns on the
/// system bus). Absent on non-MTK devices — every query then just fails,
/// which reads as "Bluetooth off" and disables this whole module.
const DEST: &str = "com.kobo.mtk.bluedroid";

/// Bluetooth was powered when the last suspend started, so the next wake
/// must power it back up and reconnect.
static RESUME_PENDING: AtomicBool = AtomicBool::new(false);

/// A resume is already running in a background thread (debounced wakes).
static RECONNECTING: AtomicBool = AtomicBool::new(false);

/// A suspend is in progress: the restore thread must stop powering the
/// stack up (a quick sleep–wake–sleep could otherwise race a lingering
/// restore into re-powering Bluetooth right as the kernel suspends —
/// exactly the unclean state this module exists to prevent). Set by
/// [`power_down_for_suspend`], cleared by [`reconnect_after_wake`].
static SUSPENDING: AtomicBool = AtomicBool::new(false);

/// How long the post-wake enable keeps retrying. Wi-Fi restore (which the
/// MTK BT stack piggybacks on) can itself take ~15s.
const RESUME_RETRY_FOR: Duration = Duration::from_secs(20);

/// The Kobo user-partition marker: present on every device, absent on
/// desktops/CI — same probe as the network module.
fn on_device() -> bool {
    std::path::Path::new("/mnt/onboard").exists()
}

fn opted_out() -> bool {
    std::env::var("GIDEON_SUSPEND_BT").as_deref() == Ok("0")
}

/// Run one `dbus-send` against the bluedroid service, capturing output.
/// `Err(())` covers every failure mode — no dbus-send binary, no service,
/// call rejected — and callers treat them all as "Bluetooth unavailable".
fn dbus_call(args: &[&str]) -> Result<String, ()> {
    let output = Command::new("dbus-send")
        .args(["--system", "--print-reply", "--reply-timeout=5000"])
        .arg(format!("--dest={DEST}"))
        .args(args)
        .output()
        .map_err(|_| ())?;
    if !output.status.success() {
        return Err(());
    }
    String::from_utf8(output.stdout).map_err(|_| ())
}

/// Whether the adapter reports `Powered=true`. Any failure (no service,
/// no adapter yet) reads as "off".
fn is_powered() -> bool {
    dbus_call(&[
        "/org/bluez/hci0",
        "org.freedesktop.DBus.Properties.Get",
        "string:org.bluez.Adapter1",
        "string:Powered",
    ])
    .is_ok_and(|reply| reply.contains("boolean true"))
}

/// The stack's own power-up sequence: start the bluedroid manager, then
/// power the BlueZ adapter.
fn power_on() {
    let _ = dbus_call(&["/", "com.kobo.bluetooth.BluedroidManager1.On"]);
    let _ = dbus_call(&[
        "/org/bluez/hci0",
        "org.freedesktop.DBus.Properties.Set",
        "string:org.bluez.Adapter1",
        "string:Powered",
        "variant:boolean:true",
    ]);
}

/// The matching power-down: unpower the adapter, then stop the manager.
fn power_off() {
    let _ = dbus_call(&[
        "/org/bluez/hci0",
        "org.freedesktop.DBus.Properties.Set",
        "string:org.bluez.Adapter1",
        "string:Powered",
        "variant:boolean:false",
    ]);
    let _ = dbus_call(&["/", "com.kobo.bluetooth.BluedroidManager1.Off"]);
}

/// Cleanly power Bluetooth down before a suspend. Returns `true` when the
/// adapter was on and is now shut down (the caller logs the step); the next
/// [`reconnect_after_wake`] will then power it back up. No-op (and `false`)
/// off-device, when opted out, or when Bluetooth is already off.
pub fn power_down_for_suspend() -> bool {
    if !on_device() || opted_out() {
        return false;
    }
    // Stop any in-flight restore thread first, even when the adapter reads
    // as off right now (a restore mid-retry keeps calling power-on and must
    // not do so into the suspend). A pending-but-unfinished restore keeps
    // its RESUME_PENDING, so the next wake picks it back up.
    SUSPENDING.store(true, Ordering::SeqCst);
    if !is_powered() {
        return false;
    }
    power_off();
    RESUME_PENDING.store(true, Ordering::SeqCst);
    true
}

/// Whether the next wake needs to restore Bluetooth — the UI uses this to
/// kick the Wi-Fi restore too (the MTK BT stack needs the shared radio up)
/// even when Wi-Fi auto-connect is off.
pub fn resume_pending() -> bool {
    RESUME_PENDING.load(Ordering::SeqCst)
}

/// Bring Bluetooth back after a wake, if the last suspend powered it down:
/// re-enable the stack (retrying while the shared radio comes up) and ask it
/// to reconnect every paired device, so a page-turn remote resumes on its
/// own instead of needing a trip to Nickel. Runs in a background thread;
/// returns immediately. No-op when nothing is pending.
pub fn reconnect_after_wake() {
    SUSPENDING.store(false, Ordering::SeqCst);
    if !RESUME_PENDING.swap(false, Ordering::SeqCst) {
        return;
    }
    if RECONNECTING.swap(true, Ordering::SeqCst) {
        // A previous wake's restore is still running; re-arm so this
        // wake's restore isn't lost if that thread's window has expired —
        // the next wake (or the running thread) will service it.
        RESUME_PENDING.store(true, Ordering::SeqCst);
        return;
    }
    let spawned = std::thread::Builder::new()
        .name("gideon-bt-restore".into())
        .spawn(|| {
            restore_blocking();
            RECONNECTING.store(false, Ordering::SeqCst);
        });
    if spawned.is_err() {
        // Never restore inline — this is the UI thread and the retry loop
        // can block for many seconds. Re-arm and let the next wake try.
        RECONNECTING.store(false, Ordering::SeqCst);
        RESUME_PENDING.store(true, Ordering::SeqCst);
    }
}

/// The blocking restore: retry the power-up until the adapter reports
/// powered (the Wi-Fi bring-up it depends on can take a while), then
/// connect the paired devices. Best-effort throughout.
fn restore_blocking() {
    let deadline = std::time::Instant::now() + RESUME_RETRY_FOR;
    loop {
        if SUSPENDING.load(Ordering::SeqCst) {
            // A new suspend started while we were restoring: stop powering
            // the stack up into it, and leave the restore pending so the
            // wake that follows finishes the job.
            RESUME_PENDING.store(true, Ordering::SeqCst);
            eprintln!("gideon bluetooth: restore paused by a new suspend");
            return;
        }
        power_on();
        if is_powered() {
            break;
        }
        if std::time::Instant::now() >= deadline {
            // Give up for now but re-arm: the next wake retries instead of
            // leaving Bluetooth off forever. A few dbus calls per wake
            // against a genuinely dead chip is the cheap side of that trade.
            RESUME_PENDING.store(true, Ordering::SeqCst);
            eprintln!("gideon bluetooth: adapter did not power up; will retry on next wake");
            return;
        }
        std::thread::sleep(Duration::from_secs(1));
    }
    eprintln!("gideon bluetooth: adapter powered up after wake");
    let Ok(reply) = dbus_call(&["/", "org.freedesktop.DBus.ObjectManager.GetManagedObjects"])
    else {
        return;
    };
    for path in paired_device_paths(&reply) {
        eprintln!("gideon bluetooth: reconnecting {path}");
        let _ = dbus_call(&[&path, "org.bluez.Device1.Connect"]);
    }
}

/// Parse a `dbus-send --print-reply` `GetManagedObjects` reply into the
/// object paths of the *paired* devices. The reply lists each device as
/// `object path "/org/bluez/hci0/dev_..."` followed by its properties as
/// `string "Paired"` / `variant boolean true` line pairs.
fn paired_device_paths(reply: &str) -> Vec<String> {
    let mut paths = Vec::new();
    let mut current: Option<String> = None;
    let mut awaiting_paired_value = false;
    for line in reply.lines() {
        if let Some(rest) = line.split("object path \"").nth(1) {
            if let Some(path) = rest.split('"').next() {
                if path.starts_with("/org/bluez/hci0/dev_") {
                    current = Some(path.to_string());
                    awaiting_paired_value = false;
                }
            }
            continue;
        }
        if line.contains("string \"Paired\"") {
            awaiting_paired_value = current.is_some();
            continue;
        }
        if awaiting_paired_value {
            awaiting_paired_value = false;
            if line.contains("boolean true") {
                if let Some(path) = current.take() {
                    paths.push(path);
                }
            }
        }
    }
    paths
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"method return time=1.2 sender=:1.4 -> destination=:1.9 serial=11 reply_serial=2
   array [
      dict entry(
         object path "/org/bluez/hci0"
         array [
         ]
      )
      dict entry(
         object path "/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF"
         array [
            dict entry(
               string "org.bluez.Device1"
               array [
                  dict entry(
                     string "Paired"
                     variant                boolean true
                  )
                  dict entry(
                     string "Connected"
                     variant                boolean false
                  )
               ]
            )
         ]
      )
      dict entry(
         object path "/org/bluez/hci0/dev_11_22_33_44_55_66"
         array [
            dict entry(
               string "Paired"
               variant                boolean false
            )
         ]
      )
   ]
"#;

    #[test]
    fn parses_only_paired_device_paths() {
        assert_eq!(
            paired_device_paths(SAMPLE),
            vec!["/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF".to_string()]
        );
    }

    #[test]
    fn adapter_and_garbage_lines_are_ignored() {
        assert!(paired_device_paths("").is_empty());
        // A stray Paired=true with no device path in scope must not panic
        // or emit anything.
        let stray = "string \"Paired\"\nvariant boolean true\n";
        assert!(paired_device_paths(stray).is_empty());
        // The adapter's own path is not a device.
        let adapter = "object path \"/org/bluez/hci0\"\nstring \"Paired\"\nvariant boolean true\n";
        assert!(paired_device_paths(adapter).is_empty());
    }

    #[test]
    fn resume_is_a_noop_without_a_pending_suspend() {
        // Off-device there is never a pending resume; the call must return
        // instantly without spawning anything or touching dbus.
        RESUME_PENDING.store(false, Ordering::SeqCst);
        reconnect_after_wake();
        assert!(!resume_pending());
    }
}

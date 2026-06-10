//! Small helpers vendored from bobo-koreader's `util.rs`. Only the
//! connectivity check used by the `net` WASM imports is kept.

use std::io::ErrorKind;
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

/// How long each probe connection may take before we stop waiting.
const PROBE_TIMEOUT: Duration = Duration::from_secs(3);

/// Best-effort connectivity probe (the point is to fail *fast* when musl's
/// `getaddrinfo()` would otherwise stall for seconds with no network).
///
/// Fails OPEN: a timeout or any other uncertain outcome returns `true`, so
/// the real request runs and surfaces the actual error — only a quick,
/// definitive connection failure (e.g. network unreachable) returns `false`.
pub async fn has_internet_connection() -> bool {
    let addrs = [
        SocketAddr::from(([1, 0, 0, 1], 80)),
        SocketAddr::from(([1, 1, 1, 1], 80)),
    ];

    for addr in &addrs {
        match TcpStream::connect_timeout(addr, PROBE_TIMEOUT) {
            Ok(_) => return true,
            // Timeout means "uncertain", not "offline": fail open and let
            // the real (also time-limited) request report what's wrong.
            Err(e) if matches!(e.kind(), ErrorKind::TimedOut | ErrorKind::WouldBlock) => {
                return true
            }
            Err(_) => {}
        }
    }
    false
}

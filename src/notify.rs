//! `sd_notify(3)` without libsystemd: the protocol is a datagram of
//! `KEY=VALUE` lines to the socket in `NOTIFY_SOCKET` (ADR-14).
//!
//! Reference: `docs/16-deployment.md`. Outside systemd there is no socket and
//! every call is a no-op.

use std::os::unix::net::{SocketAddr, UnixDatagram};
use std::time::Duration;

use tracing::{debug, warn};

/// `NOTIFY_SOCKET`, resolved once at startup.
#[derive(Clone, Debug)]
pub struct Notifier {
    address: Option<SocketAddr>,
}

impl Notifier {
    /// Reads `NOTIFY_SOCKET` and takes it out of the environment, the way
    /// `sd_notify(unset_environment = 1)` does: the helpers we spawn
    /// (`systemctl`, `sh -c`) would otherwise report to systemd as us.
    ///
    /// Must run before any other thread exists — `main` calls it before the
    /// tokio runtime is built.
    pub fn from_env() -> Notifier {
        let socket = std::env::var_os("NOTIFY_SOCKET");
        if socket.is_some() {
            // SAFETY: called from `main` before the runtime starts, so no
            // other thread can be reading the environment concurrently.
            unsafe { std::env::remove_var("NOTIFY_SOCKET") };
        }
        let address = socket.and_then(|path| {
            let path = path.to_string_lossy().into_owned();
            match parse_socket(&path) {
                Ok(address) => Some(address),
                Err(err) => {
                    warn!(path, %err, "NOTIFY_SOCKET is unusable");
                    None
                }
            }
        });
        if address.is_some() {
            debug!("systemd notify socket found");
        }
        Notifier { address }
    }

    pub fn is_active(&self) -> bool {
        self.address.is_some()
    }

    pub fn ready(&self) {
        self.send("READY=1");
    }

    pub fn reloading(&self) {
        self.send("RELOADING=1");
    }

    pub fn stopping(&self) {
        self.send("STOPPING=1");
    }

    /// One line for `systemctl status`.
    pub fn status(&self, text: &str) {
        self.send(&format!("STATUS={}", text.replace('\n', " ")));
    }

    /// `WATCHDOG_USEC` from the unit, if the watchdog is on.
    pub fn watchdog_interval() -> Option<Duration> {
        let usec: u64 = std::env::var("WATCHDOG_USEC").ok()?.parse().ok()?;
        (usec > 0).then(|| Duration::from_micros(usec))
    }

    /// Pings the watchdog at half its interval for as long as the runtime
    /// is alive — which is what the watchdog is there to check.
    pub fn spawn_watchdog(&self) {
        let Some(interval) = Notifier::watchdog_interval() else {
            return;
        };
        let notifier = self.clone();
        debug!(?interval, "systemd watchdog on");
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(interval / 2).await;
                notifier.send("WATCHDOG=1");
            }
        });
    }

    fn send(&self, message: &str) {
        let Some(address) = &self.address else {
            return;
        };
        let result = UnixDatagram::unbound()
            .and_then(|socket| socket.send_to_addr(message.as_bytes(), address));
        if let Err(err) = result {
            warn!(%err, message, "sd_notify failed");
        }
    }
}

/// A leading `@` means an abstract socket, as in `sd_notify(3)`.
fn parse_socket(path: &str) -> std::io::Result<SocketAddr> {
    if let Some(name) = path.strip_prefix('@') {
        use std::os::linux::net::SocketAddrExt;
        SocketAddr::from_abstract_name(name)
    } else {
        SocketAddr::from_pathname(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_reach_the_socket() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notify");
        let server = UnixDatagram::bind(&path).unwrap();
        let notifier = Notifier {
            address: Some(parse_socket(path.to_str().unwrap()).unwrap()),
        };
        assert!(notifier.is_active());

        notifier.ready();
        notifier.status("Active, mode balanced\nsecond line");
        notifier.stopping();

        let mut buffer = [0u8; 256];
        let read = |buffer: &mut [u8]| {
            let n = server.recv(buffer).unwrap();
            String::from_utf8_lossy(&buffer[..n]).into_owned()
        };
        assert_eq!(read(&mut buffer), "READY=1");
        assert_eq!(
            read(&mut buffer),
            "STATUS=Active, mode balanced second line"
        );
        assert_eq!(read(&mut buffer), "STOPPING=1");
    }

    #[test]
    fn no_socket_is_a_no_op() {
        let notifier = Notifier { address: None };
        assert!(!notifier.is_active());
        notifier.ready();
        notifier.stopping();
    }

    #[test]
    fn abstract_names_parse() {
        use std::os::linux::net::SocketAddrExt;
        let address = parse_socket("@/org/freedesktop/systemd1/notify/123").unwrap();
        assert_eq!(
            address.as_abstract_name(),
            Some(b"/org/freedesktop/systemd1/notify/123".as_slice())
        );
        let address = parse_socket("/run/systemd/notify").unwrap();
        assert!(address.as_pathname().is_some());
    }
}

//! AC and battery from `/sys/class/power_supply`.
//!
//! Reference: `docs/07-power-supply.md`. No UPower (ADR-7): two numbers are
//! not worth a D-Bus dependency.

use std::fs;
use std::io;
use std::os::fd::{AsRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::unix::AsyncFd;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use super::{PowerSnapshot, PowerSource};
use crate::core::Event;

pub const SYSFS_ROOT: &str = "/sys/class/power_supply";

/// The safety net for embedded controllers that never send a uevent.
pub const POLL_INTERVAL: Duration = Duration::from_secs(60);

/// Right after resume the driver can still report the values from before it
/// (`docs/07-power-supply.md`).
const RESUME_RECHECKS: [Duration; 2] = [Duration::from_secs(2), Duration::from_secs(10)];

pub struct SysfsPowerSource {
    root: PathBuf,
}

impl SysfsPowerSource {
    pub fn new() -> SysfsPowerSource {
        SysfsPowerSource::with_root(SYSFS_ROOT)
    }

    pub fn with_root(root: impl Into<PathBuf>) -> SysfsPowerSource {
        SysfsPowerSource { root: root.into() }
    }
}

impl Default for SysfsPowerSource {
    fn default() -> Self {
        SysfsPowerSource::new()
    }
}

impl PowerSource for SysfsPowerSource {
    fn snapshot(&self) -> io::Result<PowerSnapshot> {
        let mut ac = false;
        // name, capacity, design capacity used as the averaging weight
        let mut batteries: Vec<(String, u8, u64)> = Vec::new();

        for entry in fs::read_dir(&self.root)? {
            let path = entry?.path();
            let name = match path.file_name().and_then(|n| n.to_str()) {
                Some(name) => name.to_string(),
                None => continue,
            };

            match read_field(&path, "type").as_deref() {
                // A dock and a charger can both be present; either one counts.
                Some("Mains") | Some("USB") => {
                    if read_field(&path, "online").as_deref() == Some("1") {
                        ac = true;
                    }
                }
                Some("Battery") => {
                    // Mice, headsets and other peripherals with a battery.
                    if read_field(&path, "scope").as_deref() == Some("Device") {
                        continue;
                    }
                    let Some(capacity) = read_number(&path, "capacity") else {
                        continue;
                    };
                    let full = read_number(&path, "energy_full")
                        .or_else(|| read_number(&path, "charge_full"))
                        .unwrap_or(1);
                    batteries.push((name, capacity.min(100) as u8, full.max(1)));
                }
                _ => {}
            }
        }

        batteries.sort_by(|a, b| a.0.cmp(&b.0));
        let battery = weighted_capacity(&batteries);
        Ok(PowerSnapshot {
            ac,
            battery,
            batteries: batteries.into_iter().map(|(name, _, _)| name).collect(),
        })
    }
}

/// Several batteries average by their design capacity — a 20% top-up battery
/// must not count as much as the main one.
fn weighted_capacity(batteries: &[(String, u8, u64)]) -> Option<u8> {
    if batteries.is_empty() {
        return None;
    }
    let total: u128 = batteries.iter().map(|(_, _, full)| *full as u128).sum();
    if total == 0 {
        return None;
    }
    let sum: u128 = batteries
        .iter()
        .map(|(_, capacity, full)| *capacity as u128 * *full as u128)
        .sum();
    // Round to nearest so 49.6% does not read as 49%.
    Some(((sum + total / 2) / total).min(100) as u8)
}

fn read_field(dir: &Path, name: &str) -> Option<String> {
    fs::read_to_string(dir.join(name))
        .ok()
        .map(|text| text.trim().to_string())
}

fn read_number(dir: &Path, name: &str) -> Option<u64> {
    read_field(dir, name)?.parse().ok()
}

/// A fake source for tests and `--fake-power`.
#[derive(Debug, Clone)]
pub struct FakePowerSource {
    snapshot: Arc<Mutex<PowerSnapshot>>,
}

impl FakePowerSource {
    pub fn new(snapshot: PowerSnapshot) -> FakePowerSource {
        FakePowerSource {
            snapshot: Arc::new(Mutex::new(snapshot)),
        }
    }

    /// Parses the `--fake-power ac | bat:NN` argument from `docs/15-testing.md`.
    pub fn parse(spec: &str) -> Result<FakePowerSource, String> {
        let snapshot = match spec.split_once(':') {
            Some(("bat", percent)) => PowerSnapshot {
                ac: false,
                battery: Some(percent.parse().map_err(|_| "expected bat:NN")?),
                batteries: vec!["BAT0".into()],
            },
            Some(("ac", percent)) => PowerSnapshot {
                ac: true,
                battery: Some(percent.parse().map_err(|_| "expected ac:NN")?),
                batteries: vec!["BAT0".into()],
            },
            None if spec == "ac" => PowerSnapshot::on_ac(),
            None if spec == "bat" => PowerSnapshot {
                ac: false,
                battery: Some(50),
                batteries: vec!["BAT0".into()],
            },
            _ => return Err(format!("cannot parse {spec:?}, expected ac, bat or bat:NN")),
        };
        Ok(FakePowerSource::new(snapshot))
    }

    pub fn set(&self, snapshot: PowerSnapshot) {
        *crate::locked(&self.snapshot) = snapshot;
    }
}

impl PowerSource for FakePowerSource {
    fn snapshot(&self) -> io::Result<PowerSnapshot> {
        Ok(crate::locked(&self.snapshot).clone())
    }
}

// ------------------------------------------------------------------- actor

/// Lets `main` ask for an extra read and see the latest raw snapshot.
#[derive(Clone)]
pub struct SupplyHandle {
    refresh: mpsc::Sender<()>,
    latest: Arc<Mutex<PowerSnapshot>>,
}

impl SupplyHandle {
    /// After a resume, re-read twice with a delay (`docs/07-power-supply.md`).
    pub fn recheck_after_resume(&self) {
        for delay in RESUME_RECHECKS {
            let refresh = self.refresh.clone();
            tokio::spawn(async move {
                tokio::time::sleep(delay).await;
                let _ = refresh.send(()).await;
            });
        }
    }

    pub fn latest(&self) -> PowerSnapshot {
        crate::locked(&self.latest).clone()
    }
}

/// Watches the power supply and turns changes into events.
///
/// Both sources from `docs/07-power-supply.md` are used: udev netlink for an
/// immediate reaction, polling as the safety net. Losing netlink leaves
/// polling in place rather than the daemon without power events.
pub fn spawn(
    source: Arc<dyn PowerSource + Send + Sync>,
    initial: PowerSnapshot,
    events: mpsc::Sender<Event>,
) -> SupplyHandle {
    let (refresh_tx, mut refresh_rx) = mpsc::channel(4);
    let latest = Arc::new(Mutex::new(initial.clone()));
    let handle = SupplyHandle {
        refresh: refresh_tx,
        latest: latest.clone(),
    };

    tokio::spawn(async move {
        let monitor = match UdevMonitor::open() {
            Ok(monitor) => {
                info!("listening for power_supply uevents");
                Some(monitor)
            }
            Err(err) => {
                warn!(%err, "no udev netlink, falling back to polling only");
                None
            }
        };

        let mut previous = initial;
        loop {
            tokio::select! {
                _ = tokio::time::sleep(POLL_INTERVAL) => {}
                _ = wait_for_uevent(monitor.as_ref()) => {}
                message = refresh_rx.recv() => {
                    if message.is_none() {
                        return;
                    }
                }
            }

            let snapshot = match source.snapshot() {
                Ok(snapshot) => snapshot,
                Err(err) => {
                    warn!(%err, "cannot read the power supply");
                    continue;
                }
            };
            *crate::locked(&latest) = snapshot.clone();

            for event in diff(&previous, &snapshot) {
                if events.send(event).await.is_err() {
                    return;
                }
            }
            previous = snapshot;
        }
    });

    handle
}

/// Only real changes become events: AC always, battery from one percent up.
fn diff(previous: &PowerSnapshot, current: &PowerSnapshot) -> Vec<Event> {
    let mut events = Vec::new();
    if previous.ac != current.ac {
        events.push(Event::AcChanged(current.ac));
    }
    if let Some(percent) = current.battery
        && previous.battery != Some(percent)
    {
        events.push(Event::Battery(percent));
    }
    events
}

async fn wait_for_uevent(monitor: Option<&UdevMonitor>) {
    match monitor {
        Some(monitor) => {
            if let Err(err) = monitor.next_power_supply_event().await {
                warn!(%err, "udev netlink failed, polling continues");
                std::future::pending::<()>().await;
            }
        }
        None => std::future::pending().await,
    }
}

/// Raw `NETLINK_KOBJECT_UEVENT` (ADR-12): the kernel's uevent format is a
/// NUL-separated list of `KEY=VALUE`, which needs no libudev.
struct UdevMonitor {
    socket: AsyncFd<OwnedFd>,
}

impl UdevMonitor {
    fn open() -> io::Result<UdevMonitor> {
        use nix::sys::socket::{AddressFamily, NetlinkAddr, SockFlag, SockType, bind, socket};

        let fd = socket(
            AddressFamily::Netlink,
            SockType::Datagram,
            SockFlag::SOCK_CLOEXEC | SockFlag::SOCK_NONBLOCK,
            nix::sys::socket::SockProtocol::NetlinkKObjectUEvent,
        )?;
        // Group 1 is the kernel's own uevent multicast group; it needs
        // CAP_NET_ADMIN, which the root unit has.
        bind(fd.as_raw_fd(), &NetlinkAddr::new(0, 1))?;
        Ok(UdevMonitor {
            socket: AsyncFd::new(fd)?,
        })
    }

    async fn next_power_supply_event(&self) -> io::Result<()> {
        let mut buffer = [0u8; 8192];
        loop {
            let mut guard = self.socket.readable().await?;
            let read = guard.try_io(|inner| {
                nix::sys::socket::recv(
                    inner.get_ref().as_raw_fd(),
                    &mut buffer,
                    nix::sys::socket::MsgFlags::empty(),
                )
                .map_err(io::Error::from)
            });
            let length = match read {
                Ok(Ok(length)) => length,
                Ok(Err(err)) => return Err(err),
                // Another reader won the race; wait again.
                Err(_would_block) => continue,
            };
            if is_power_supply_change(&buffer[..length]) {
                debug!("power_supply uevent");
                return Ok(());
            }
        }
    }
}

fn is_power_supply_change(message: &[u8]) -> bool {
    let mut subsystem = false;
    let mut action = false;
    for field in message.split(|byte| *byte == 0) {
        match std::str::from_utf8(field) {
            Ok("SUBSYSTEM=power_supply") => subsystem = true,
            Ok("ACTION=change") | Ok("ACTION=add") | Ok("ACTION=remove") => action = true,
            _ => {}
        }
    }
    subsystem && action
}

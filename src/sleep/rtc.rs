//! The RTC wake alarm: `wakealarm` in sysfs, or `rtcwake -m no`.
//!
//! Reference: `docs/10-long-sleep-rtc.md`, ADR-5. Only the alarm is written
//! here; the suspend itself always goes through logind (ADR-4), so both
//! backends do the same job and differ only in how the alarm reaches the RTC.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tracing::{debug, info, warn};

use crate::config::{RtcBackend, Server};

pub const SYSFS_ROOT: &str = "/sys/class/rtc";

/// The kernel rejects an alarm in the past or a couple of seconds away with
/// `EINVAL`; a full minute keeps us clear of that with time to suspend.
pub const MIN_HORIZON: Duration = Duration::from_secs(60);

pub trait RtcAlarm: Send + Sync {
    /// Arms a wake at `at`, replacing whatever alarm was set. Fails before
    /// touching the RTC when `at` is closer than [`MIN_HORIZON`].
    fn set(&self, at: SystemTime) -> io::Result<()>;
    fn clear(&self) -> io::Result<()>;
    /// The armed alarm, if any. The kernel clears it once it has fired.
    fn pending(&self) -> io::Result<Option<SystemTime>>;
}

/// The backend `[server]` asks for, or `None` when the RTC cannot wake the
/// machine — the caller marks `rtc` as degraded.
pub fn from_config(server: &Server) -> Option<Box<dyn RtcAlarm>> {
    let root = Path::new(SYSFS_ROOT);
    match server.rtc_backend {
        RtcBackend::Wakealarm => Wakealarm::discover(root, &server.rtc_device)
            .map(|alarm| Box::new(alarm) as Box<dyn RtcAlarm>),
        RtcBackend::Rtcwake => Rtcwake::discover(root, &server.rtc_device)
            .map(|alarm| Box::new(alarm) as Box<dyn RtcAlarm>),
    }
}

/// The seconds to write, or the error to return before any write happens.
fn epoch_for(at: SystemTime, now: SystemTime) -> io::Result<u64> {
    let horizon = now + MIN_HORIZON;
    if at < horizon {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("alarm must be at least {}s away", MIN_HORIZON.as_secs()),
        ));
    }
    at.duration_since(UNIX_EPOCH)
        .map(|since| since.as_secs())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "alarm before the epoch"))
}

/// `/sys/class/rtc/<dev>/wakealarm` is one line of `<epoch>`, or empty.
fn parse_pending(text: &str) -> io::Result<Option<SystemTime>> {
    let text = text.trim();
    if text.is_empty() {
        return Ok(None);
    }
    let secs: u64 = text
        .parse()
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "wakealarm"))?;
    Ok(Some(UNIX_EPOCH + Duration::from_secs(secs)))
}

/// Says so once at startup when the RTC cannot wake the machine at all
/// (`docs/16-deployment.md`, checklist).
fn check_wakeup_enabled(root: &Path, device: &str) {
    let path = root.join(device).join("device/power/wakeup");
    match fs::read_to_string(&path) {
        Ok(text) if text.trim() == "enabled" => {}
        Ok(text) => warn!(
            device,
            wakeup = text.trim(),
            "the RTC is not a wakeup source; the alarm will not wake the machine"
        ),
        Err(err) => debug!(device, %err, "cannot read the RTC wakeup attribute"),
    }
}

// --------------------------------------------------------------- wakealarm

/// The `wakealarm` attribute as the kernel exposes it. Behind a trait so the
/// tests can watch the order of writes, which the kernel makes mandatory.
trait AlarmFile: Send + Sync {
    fn read(&self) -> io::Result<String>;
    fn write(&self, value: &str) -> io::Result<()>;
}

struct SysfsFile(PathBuf);

impl AlarmFile for SysfsFile {
    fn read(&self) -> io::Result<String> {
        fs::read_to_string(&self.0)
    }

    fn write(&self, value: &str) -> io::Result<()> {
        // Opened per write, like the backlight: sysfs descriptors do not
        // like surviving a suspend.
        fs::write(&self.0, value)
    }
}

/// The default backend: two writes to sysfs.
pub struct Wakealarm {
    file: Box<dyn AlarmFile>,
}

impl Wakealarm {
    pub fn discover(root: &Path, device: &str) -> Option<Wakealarm> {
        let path = root.join(device).join("wakealarm");
        if !path.is_file() {
            warn!(
                device,
                "no wakealarm attribute; the RTC cannot wake the machine"
            );
            return None;
        }
        check_wakeup_enabled(root, device);
        info!(device, "rtc alarm via wakealarm");
        Some(Wakealarm {
            file: Box::new(SysfsFile(path)),
        })
    }
}

impl RtcAlarm for Wakealarm {
    fn set(&self, at: SystemTime) -> io::Result<()> {
        let epoch = epoch_for(at, SystemTime::now())?;
        // Writing over an armed alarm fails; it has to be cleared first.
        self.file.write("0")?;
        self.file.write(&epoch.to_string())?;
        debug!(epoch, "rtc alarm armed");
        Ok(())
    }

    fn clear(&self) -> io::Result<()> {
        self.file.write("0")
    }

    fn pending(&self) -> io::Result<Option<SystemTime>> {
        parse_pending(&self.file.read()?)
    }
}

// ----------------------------------------------------------------- rtcwake

/// `rtcwake -m no` from util-linux writes the alarm and nothing else; the
/// sysfs attribute is still what `pending` reads.
pub struct Rtcwake {
    device: String,
    file: SysfsFile,
}

impl Rtcwake {
    pub fn discover(root: &Path, device: &str) -> Option<Rtcwake> {
        let path = root.join(device).join("wakealarm");
        if !path.is_file() {
            warn!(
                device,
                "no wakealarm attribute; the RTC cannot wake the machine"
            );
            return None;
        }
        check_wakeup_enabled(root, device);
        info!(device, "rtc alarm via rtcwake");
        Some(Rtcwake {
            device: device.to_string(),
            file: SysfsFile(path),
        })
    }

    /// `rtcwake` is a short-lived process; the few milliseconds it takes are
    /// not worth an async boundary in a trait that is otherwise two writes.
    fn run(&self, args: &[&str]) -> io::Result<()> {
        let output = process::Command::new("rtcwake")
            .args(["-d", &self.device])
            .args(args)
            .output()?;
        if output.status.success() {
            return Ok(());
        }
        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(io::Error::other(format!(
            "rtcwake exited with {}: {}",
            output.status,
            stderr.trim()
        )))
    }
}

impl RtcAlarm for Rtcwake {
    fn set(&self, at: SystemTime) -> io::Result<()> {
        let epoch = epoch_for(at, SystemTime::now())?;
        // Never `-m mem`: that would suspend behind logind's back (ADR-4).
        self.run(&["-m", "no", "-t", &epoch.to_string()])?;
        debug!(epoch, "rtc alarm armed");
        Ok(())
    }

    fn clear(&self) -> io::Result<()> {
        self.run(&["-m", "disable"])
    }

    fn pending(&self) -> io::Result<Option<SystemTime>> {
        parse_pending(&self.file.read()?)
    }
}

#[cfg(test)]
mod tests {
    //! `docs/15-testing.md`, "Sleep/RTC". The kernel's `wakealarm` is
    //! emulated in memory; nothing here arms a real alarm.

    use std::sync::Mutex;

    use super::*;

    /// The kernel's rules: an armed alarm has to be cleared with `0` before
    /// a new one can be written.
    #[derive(Default)]
    struct FakeAlarmFile {
        armed: Mutex<Option<u64>>,
        writes: Mutex<Vec<String>>,
    }

    impl AlarmFile for FakeAlarmFile {
        fn read(&self) -> io::Result<String> {
            Ok(match *crate::locked(&self.armed) {
                Some(epoch) => format!("{epoch}\n"),
                None => String::new(),
            })
        }

        fn write(&self, value: &str) -> io::Result<()> {
            crate::locked(&self.writes).push(value.to_string());
            let mut armed = crate::locked(&self.armed);
            if value == "0" {
                *armed = None;
                return Ok(());
            }
            if armed.is_some() {
                return Err(io::Error::new(io::ErrorKind::ResourceBusy, "EBUSY"));
            }
            let epoch: u64 = value
                .parse()
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "EINVAL"))?;
            *armed = Some(epoch);
            Ok(())
        }
    }

    fn wakealarm() -> (Wakealarm, std::sync::Arc<FakeAlarmFile>) {
        // The trait object owns one handle; the test keeps the other.
        struct Shared(std::sync::Arc<FakeAlarmFile>);
        impl AlarmFile for Shared {
            fn read(&self) -> io::Result<String> {
                self.0.read()
            }
            fn write(&self, value: &str) -> io::Result<()> {
                self.0.write(value)
            }
        }
        let file = std::sync::Arc::new(FakeAlarmFile::default());
        let alarm = Wakealarm {
            file: Box::new(Shared(file.clone())),
        };
        (alarm, file)
    }

    fn epoch(at: SystemTime) -> u64 {
        at.duration_since(UNIX_EPOCH).unwrap().as_secs()
    }

    #[test]
    fn set_clears_first_then_writes_the_epoch() {
        let (alarm, file) = wakealarm();
        let at = SystemTime::now() + Duration::from_secs(20 * 60);
        alarm.set(at).unwrap();
        assert_eq!(
            *crate::locked(&file.writes),
            vec!["0".to_string(), epoch(at).to_string()]
        );
        assert_eq!(
            alarm.pending().unwrap(),
            Some(UNIX_EPOCH + Duration::from_secs(epoch(at)))
        );
    }

    #[test]
    fn set_replaces_an_armed_alarm() {
        let (alarm, file) = wakealarm();
        let first = SystemTime::now() + Duration::from_secs(600);
        let second = SystemTime::now() + Duration::from_secs(1200);
        alarm.set(first).unwrap();
        alarm.set(second).unwrap();
        assert_eq!(*crate::locked(&file.armed), Some(epoch(second)));
        assert_eq!(crate::locked(&file.writes).len(), 4);
    }

    #[test]
    fn too_close_fails_before_any_write() {
        let (alarm, file) = wakealarm();
        let err = alarm
            .set(SystemTime::now() + Duration::from_secs(30))
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        let err = alarm
            .set(SystemTime::now() - Duration::from_secs(30))
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        assert!(crate::locked(&file.writes).is_empty());
        assert_eq!(alarm.pending().unwrap(), None);
    }

    #[test]
    fn clear_writes_zero() {
        let (alarm, file) = wakealarm();
        alarm
            .set(SystemTime::now() + Duration::from_secs(600))
            .unwrap();
        alarm.clear().unwrap();
        assert_eq!(alarm.pending().unwrap(), None);
        assert_eq!(
            crate::locked(&file.writes).last().map(String::as_str),
            Some("0")
        );
    }

    #[test]
    fn horizon_is_exactly_a_minute() {
        let now = UNIX_EPOCH + Duration::from_secs(1_000_000);
        assert!(epoch_for(now + Duration::from_secs(59), now).is_err());
        assert_eq!(
            epoch_for(now + Duration::from_secs(60), now).unwrap(),
            1_000_060
        );
        assert!(epoch_for(now - Duration::from_secs(1), now).is_err());
    }

    #[test]
    fn pending_parses_the_attribute() {
        assert_eq!(parse_pending("").unwrap(), None);
        assert_eq!(parse_pending("\n").unwrap(), None);
        assert_eq!(
            parse_pending("1700000000\n").unwrap(),
            Some(UNIX_EPOCH + Duration::from_secs(1_700_000_000))
        );
        assert!(parse_pending("soon").is_err());
    }

    // ------------------------------------------------------------- discovery

    fn rtc(root: &Path, device: &str, wakeup: Option<&str>) {
        let dir = root.join(device);
        fs::create_dir_all(dir.join("device/power")).unwrap();
        fs::write(dir.join("wakealarm"), "").unwrap();
        if let Some(wakeup) = wakeup {
            fs::write(dir.join("device/power/wakeup"), wakeup).unwrap();
        }
    }

    #[test]
    fn discover_needs_the_wakealarm_attribute() {
        let root = tempfile::tempdir().unwrap();
        assert!(Wakealarm::discover(root.path(), "rtc0").is_none());
        assert!(Rtcwake::discover(root.path(), "rtc0").is_none());

        // An RTC without an alarm has the directory but not the attribute.
        fs::create_dir_all(root.path().join("rtc0")).unwrap();
        assert!(Wakealarm::discover(root.path(), "rtc0").is_none());

        rtc(root.path(), "rtc0", Some("enabled"));
        rtc(root.path(), "rtc1", Some("disabled"));
        assert!(Wakealarm::discover(root.path(), "rtc0").is_some());
        // Not a wakeup source is only a warning: the attribute is there.
        assert!(Wakealarm::discover(root.path(), "rtc1").is_some());
        assert!(Rtcwake::discover(root.path(), "rtc0").is_some());
    }

    /// The sysfs backend against a real file, end to end.
    #[test]
    fn wakealarm_on_a_file() {
        let root = tempfile::tempdir().unwrap();
        rtc(root.path(), "rtc0", None);
        let alarm = Wakealarm::discover(root.path(), "rtc0").unwrap();
        assert_eq!(alarm.pending().unwrap(), None);

        let at = SystemTime::now() + Duration::from_secs(600);
        alarm.set(at).unwrap();
        assert_eq!(
            fs::read_to_string(root.path().join("rtc0/wakealarm")).unwrap(),
            epoch(at).to_string()
        );
        assert_eq!(alarm.pending().unwrap().map(epoch), Some(epoch(at)));

        alarm.clear().unwrap();
        // A real kernel reads back empty; a plain file keeps the "0".
        assert_eq!(
            fs::read_to_string(root.path().join("rtc0/wakealarm")).unwrap(),
            "0"
        );
    }

    #[test]
    fn rtcwake_reads_pending_from_sysfs() {
        let root = tempfile::tempdir().unwrap();
        rtc(root.path(), "rtc0", None);
        fs::write(root.path().join("rtc0/wakealarm"), "1700000000\n").unwrap();
        let alarm = Rtcwake::discover(root.path(), "rtc0").unwrap();
        assert_eq!(alarm.pending().unwrap().map(epoch), Some(1_700_000_000));
        // The horizon check comes before the subprocess.
        let err = alarm.set(SystemTime::now()).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }
}

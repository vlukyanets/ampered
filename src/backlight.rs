//! Screen brightness through `/sys/class/backlight`.
//!
//! Reference: `docs/05-backlight.md`. There is no Wayland protocol for this,
//! and no backlight device is a degradation, not an error.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

use crate::config::Backlight as BacklightConfig;
use crate::ipc::BacklightInfo;

pub const SYSFS_ROOT: &str = "/sys/class/backlight";

/// One write per frame is smooth enough and keeps sysfs traffic sane.
const STEP: Duration = Duration::from_millis(16);

pub trait BacklightSink: Send + Sync {
    fn read(&self) -> io::Result<u32>;
    fn write(&self, value: u32) -> io::Result<()>;
    fn max(&self) -> u32;
    fn name(&self) -> &str;
}

pub struct SysfsBacklight {
    name: String,
    dir: PathBuf,
    max: u32,
}

impl SysfsBacklight {
    /// Picks a device the way `docs/05-backlight.md` describes it.
    pub fn discover(root: &Path, device: &str) -> Option<SysfsBacklight> {
        if device != "auto" {
            return match SysfsBacklight::open(root.join(device)) {
                Ok(backlight) => Some(backlight),
                Err(err) => {
                    warn!(device, %err, "configured backlight device is unusable");
                    None
                }
            };
        }

        let mut candidates: Vec<(u8, String, SysfsBacklight)> = Vec::new();
        for entry in fs::read_dir(root).ok()?.flatten() {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            let Ok(backlight) = SysfsBacklight::open(path.clone()) else {
                continue;
            };
            // `raw` (intel_backlight, amdgpu_bl0) is usually the only one that
            // really moves the panel; acpi_video0 is often a dummy.
            let rank = match fs::read_to_string(path.join("type"))
                .unwrap_or_default()
                .trim()
            {
                "raw" => 0,
                "firmware" => 1,
                _ => 2,
            };
            candidates.push((rank, name.to_string(), backlight));
        }

        candidates.sort_by(|a, b| (a.0, &a.1).cmp(&(b.0, &b.1)));
        let best_rank = candidates.first()?.0;
        if candidates.iter().filter(|c| c.0 == best_rank).count() > 1 {
            warn!(
                chosen = candidates[0].1,
                "several backlight devices of the same type, taking the first"
            );
        }
        let (_, name, backlight) = candidates.into_iter().next()?;
        info!(device = name, max = backlight.max, "backlight");
        Some(backlight)
    }

    fn open(dir: PathBuf) -> io::Result<SysfsBacklight> {
        let max: u32 = fs::read_to_string(dir.join("max_brightness"))?
            .trim()
            .parse()
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "max_brightness"))?;
        if max == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "max_brightness is 0",
            ));
        }
        let name = dir
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default()
            .to_string();
        Ok(SysfsBacklight { name, dir, max })
    }
}

impl BacklightSink for SysfsBacklight {
    fn read(&self) -> io::Result<u32> {
        fs::read_to_string(self.dir.join("brightness"))?
            .trim()
            .parse()
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "brightness"))
    }

    fn write(&self, value: u32) -> io::Result<()> {
        // Opened per write: sysfs descriptors do not like surviving a suspend.
        fs::write(self.dir.join("brightness"), value.to_string())
    }

    fn max(&self) -> u32 {
        self.max
    }

    fn name(&self) -> &str {
        &self.name
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Active,
    Dimmed,
}

pub struct Controller {
    sink: Option<Arc<dyn BacklightSink>>,
    /// The configured device name, kept so a reload can tell it changed.
    device: String,
    state: State,
    /// Brightness before the dim, in raw units.
    pre_dim: Option<u32>,
    /// The last value we wrote ourselves — during a fade, the most recent step.
    written: Arc<AtomicU32>,
    min_percent: u8,
    transition: Duration,
    fade: Option<Fade>,
    /// `unavailable` is said once, not on every timeout.
    complained: bool,
}

struct Fade {
    cancel: Arc<AtomicBool>,
    task: JoinHandle<()>,
}

impl Controller {
    pub fn new(config: &BacklightConfig, sink: Option<Arc<dyn BacklightSink>>) -> Controller {
        Controller {
            sink,
            device: config.device.clone(),
            state: State::Active,
            pre_dim: None,
            written: Arc::new(AtomicU32::new(0)),
            min_percent: config.min_percent,
            transition: config.transition,
            fade: None,
            complained: false,
        }
    }

    /// From the config, discovering the device under the real sysfs.
    pub fn from_config(config: &BacklightConfig) -> Controller {
        let sink = SysfsBacklight::discover(Path::new(SYSFS_ROOT), &config.device)
            .map(|found| Arc::new(found) as Arc<dyn BacklightSink>);
        if sink.is_none() {
            warn!("no usable backlight device; dim and undim will do nothing");
        }
        Controller::new(config, sink)
    }

    pub fn is_available(&self) -> bool {
        self.sink.is_some()
    }

    /// Applies a reloaded `[backlight]` section. Only a changed `device` costs
    /// the dim state; the rest is picked up in place, so a reload in the middle
    /// of a dim still restores the right brightness afterwards.
    pub async fn reconfigure(&mut self, config: &BacklightConfig) {
        self.min_percent = config.min_percent;
        self.transition = config.transition;
        if config.device == self.device {
            return;
        }
        self.stop_fade().await;
        info!(
            from = self.device,
            to = config.device,
            "backlight device changed"
        );
        self.device = config.device.clone();
        self.sink = SysfsBacklight::discover(Path::new(SYSFS_ROOT), &config.device)
            .map(|found| Arc::new(found) as Arc<dyn BacklightSink>);
        self.pre_dim = None;
        self.state = State::Active;
        self.complained = false;
    }

    /// Dim to `percent` of `max_brightness`, never below `min_percent`.
    pub async fn dim_to(&mut self, percent: u8) {
        let Some(sink) = self.sink.clone() else {
            self.complain();
            return;
        };
        // Idempotent: a second Dim while already dimmed must not overwrite
        // `pre_dim` with the dimmed value (`docs/02-state-machine.md`).
        if self.state == State::Dimmed {
            return;
        }
        // A restore fade may still be ramping; `pre_dim` must be the
        // brightness the user had, not a step of that ramp.
        self.stop_fade().await;

        let current = match sink.read() {
            Ok(value) => value,
            Err(err) => {
                warn!(%err, "cannot read brightness, not dimming");
                return;
            }
        };
        let target = self.target_for(sink.as_ref(), percent);
        debug!(from = current, to = target, "dim");

        self.pre_dim = Some(current);
        self.state = State::Dimmed;
        self.fade_to(sink, current, target).await;
    }

    /// Back to `pre_dim`, unless somebody else changed the brightness while
    /// we were dimmed (ADR-9).
    pub async fn restore(&mut self) {
        let Some(sink) = self.sink.clone() else {
            return;
        };
        if self.state != State::Dimmed {
            return;
        }
        self.stop_fade().await;

        let target = self.pre_dim.take();
        self.state = State::Active;
        let Some(target) = target else {
            return;
        };

        let current = match sink.read() {
            Ok(value) => value,
            Err(err) => {
                warn!(%err, "cannot read brightness, not restoring");
                return;
            }
        };
        let ours = self.written.load(Ordering::Relaxed);
        if current != ours {
            debug!(
                current,
                ours, "brightness changed externally while dimmed, not restoring"
            );
            return;
        }

        debug!(from = current, to = target, "restore");
        self.fade_to(sink, current, target).await;
    }

    /// Waits for a running fade to finish. The shutdown path needs it: the
    /// process must not exit halfway through undimming.
    pub async fn settle(&mut self) {
        if let Some(fade) = self.fade.take() {
            let _ = fade.task.await;
        }
    }

    /// What `amperedctl status` shows, in percent.
    pub fn info(&self) -> BacklightInfo {
        let Some(sink) = &self.sink else {
            return BacklightInfo::default();
        };
        let max = sink.max();
        BacklightInfo {
            device: Some(sink.name().to_string()),
            percent: sink.read().ok().map(|raw| to_percent(raw, max)),
            pre_dim: self.pre_dim.map(|raw| to_percent(raw, max)),
        }
    }

    fn target_for(&self, sink: &dyn BacklightSink, percent: u8) -> u32 {
        let max = sink.max();
        let wanted = raw_from_percent(max, percent);
        // Never reach 0 on panels where that switches the backlight off.
        let floor = raw_from_percent(max, self.min_percent);
        wanted.max(floor).min(max)
    }

    async fn fade_to(&mut self, sink: Arc<dyn BacklightSink>, from: u32, to: u32) {
        self.stop_fade().await;
        if from == to {
            return;
        }

        let steps = if self.transition.is_zero() {
            1
        } else {
            (self.transition.as_millis() / STEP.as_millis()).max(1) as u32
        };
        if steps == 1 {
            self.write(sink.as_ref(), to);
            return;
        }

        self.written.store(to, Ordering::Relaxed);
        let cancel = Arc::new(AtomicBool::new(false));
        let task = {
            let cancel = cancel.clone();
            let written = self.written.clone();
            tokio::spawn(async move {
                for step in 1..=steps {
                    if cancel.load(Ordering::Relaxed) {
                        return;
                    }
                    let value = interpolate(from, to, step, steps);
                    written.store(value, Ordering::Relaxed);
                    if let Err(err) = sink.write(value) {
                        warn!(%err, value, "backlight write failed");
                        return;
                    }
                    tokio::time::sleep(STEP).await;
                }
            })
        };
        self.fade = Some(Fade { cancel, task });
    }

    /// Cancels a running fade and waits for it, so the value we compare
    /// against cannot change under our feet.
    async fn stop_fade(&mut self) {
        if let Some(fade) = self.fade.take() {
            fade.cancel.store(true, Ordering::Relaxed);
            let _ = fade.task.await;
        }
    }

    fn write(&self, sink: &dyn BacklightSink, value: u32) {
        self.written.store(value, Ordering::Relaxed);
        if let Err(err) = sink.write(value) {
            warn!(%err, value, "backlight write failed");
        }
    }

    fn complain(&mut self) {
        if !self.complained {
            warn!("no backlight device, dim is a no-op");
            self.complained = true;
        }
    }
}

fn raw_from_percent(max: u32, percent: u8) -> u32 {
    ((max as u64 * percent as u64) / 100) as u32
}

fn to_percent(raw: u32, max: u32) -> u8 {
    if max == 0 {
        return 0;
    }
    (((raw as u64 * 100) + max as u64 / 2) / max as u64).min(100) as u8
}

fn interpolate(from: u32, to: u32, step: u32, steps: u32) -> u32 {
    let from = from as i64;
    let to = to as i64;
    (from + (to - from) * step as i64 / steps as i64) as u32
}

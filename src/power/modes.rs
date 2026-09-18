//! Applying a mode: platform profile, cpufreq governor, EPP, custom sysfs.
//!
//! Reference: `docs/08-power-modes.md`. A missing knob is not an error — a
//! `warn!` per path, and the FSM carries on (`CLAUDE.md`, rule 5).

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use tracing::{debug, info, warn};

use crate::config::Mode;

pub const PLATFORM_PROFILE: &str = "/sys/firmware/acpi/platform_profile";
pub const PLATFORM_PROFILE_CHOICES: &str = "/sys/firmware/acpi/platform_profile_choices";
pub const CPU_ROOT: &str = "/sys/devices/system/cpu";

/// Everything that writes to the system sits behind this (`CLAUDE.md`, rule 2).
pub trait ModeSink: Send + Sync {
    fn write(&self, path: &Path, value: &str) -> io::Result<()>;
    fn read(&self, path: &Path) -> io::Result<String>;
    /// Per-CPU cpufreq knobs, e.g. `scaling_governor`.
    fn cpufreq(&self, leaf: &str) -> Vec<PathBuf>;
}

/// Writes to a real sysfs. `root` is `/` outside tests.
pub struct SysfsModeSink {
    root: PathBuf,
}

impl SysfsModeSink {
    pub fn new() -> SysfsModeSink {
        SysfsModeSink::with_root("/")
    }

    pub fn with_root(root: impl Into<PathBuf>) -> SysfsModeSink {
        SysfsModeSink { root: root.into() }
    }

    fn resolve(&self, path: &Path) -> PathBuf {
        match path.strip_prefix("/") {
            Ok(relative) => self.root.join(relative),
            Err(_) => self.root.join(path),
        }
    }
}

impl Default for SysfsModeSink {
    fn default() -> Self {
        SysfsModeSink::new()
    }
}

impl ModeSink for SysfsModeSink {
    fn write(&self, path: &Path, value: &str) -> io::Result<()> {
        // sysfs wants the bare value; the file is opened per write because
        // long-lived descriptors do not survive suspend well.
        fs::write(self.resolve(path), value)
    }

    fn read(&self, path: &Path) -> io::Result<String> {
        Ok(fs::read_to_string(self.resolve(path))?.trim().to_string())
    }

    fn cpufreq(&self, leaf: &str) -> Vec<PathBuf> {
        let mut paths = Vec::new();
        let Ok(entries) = fs::read_dir(self.resolve(Path::new(CPU_ROOT))) else {
            return paths;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            // cpu0, cpu1 … but not cpufreq or cpuidle.
            if !name.starts_with("cpu") || !name[3..].chars().all(|c| c.is_ascii_digit()) {
                continue;
            }
            if name.len() == 3 {
                continue;
            }
            let path = entry.path().join("cpufreq").join(leaf);
            if path.exists() {
                // Report the unprefixed path so `write` can resolve it again.
                paths.push(Path::new(CPU_ROOT).join(name).join("cpufreq").join(leaf));
            }
        }
        paths.sort();
        paths
    }
}

pub struct ModeApplier<S: ModeSink> {
    sink: S,
}

impl<S: ModeSink> ModeApplier<S> {
    pub fn new(sink: S) -> ModeApplier<S> {
        ModeApplier { sink }
    }

    /// Custom `sysfs` first, so knobs like `no_turbo` are in place before the
    /// governor changes (`docs/08-power-modes.md`).
    pub fn apply(&self, name: &str, mode: &Mode) {
        info!(mode = name, "applying mode");
        for (path, value) in mode.sysfs_writes() {
            self.write(Path::new(path), &value);
        }
        if let Some(profile) = &mode.platform_profile {
            self.platform_profile(profile);
        }
        if let Some(governor) = &mode.cpu_governor {
            self.cpufreq("scaling_governor", governor);
        }
        if let Some(epp) = &mode.epp {
            self.cpufreq("energy_performance_preference", epp);
        }
    }

    fn write(&self, path: &Path, value: &str) {
        match self.sink.write(path, value) {
            Ok(()) => debug!(path = %path.display(), value, "written"),
            Err(err) => warn!(path = %path.display(), value, %err, "sysfs write failed"),
        }
    }

    /// The firmware advertises what it accepts; anything else would be `EINVAL`.
    fn platform_profile(&self, profile: &str) {
        let choices = match self.sink.read(Path::new(PLATFORM_PROFILE_CHOICES)) {
            Ok(choices) => choices,
            Err(err) => {
                debug!(%err, "no platform_profile on this machine, skipping");
                return;
            }
        };
        if !choices.split_whitespace().any(|choice| choice == profile) {
            warn!(profile, choices, "platform_profile not supported, skipping");
            return;
        }
        self.write(Path::new(PLATFORM_PROFILE), profile);
    }

    /// On `intel_pstate` in active mode only some values exist. Failures are
    /// summed up in one warning: every core would otherwise report the same
    /// thing, and a mode change would fill the journal.
    fn cpufreq(&self, leaf: &str, value: &str) {
        let paths = self.sink.cpufreq(leaf);
        if paths.is_empty() {
            debug!(leaf, "no cpufreq knob on this machine, skipping");
            return;
        }
        let mut failed = 0usize;
        let mut first_error = None;
        for path in &paths {
            match self.sink.write(path, value) {
                Ok(()) => {}
                Err(err) => {
                    failed += 1;
                    first_error.get_or_insert(err.to_string());
                }
            }
        }
        match first_error {
            Some(err) => warn!(
                leaf,
                value,
                failed,
                of = paths.len(),
                %err,
                "cpufreq write failed"
            ),
            None => debug!(leaf, value, cpus = paths.len(), "written"),
        }
    }
}

/// Daemons that would fight us over the same knobs (`docs/08-power-modes.md`).
pub const CONFLICTING_UNITS: [&str; 3] = [
    "power-profiles-daemon.service",
    "tlp.service",
    "tuned.service",
];

/// Returns `conflict:<unit>` entries for `status.degraded`.
pub async fn detect_conflicts() -> Vec<String> {
    let mut found = Vec::new();
    for unit in CONFLICTING_UNITS {
        let output = tokio::process::Command::new("systemctl")
            .args(["is-active", "--quiet", unit])
            .status()
            .await;
        if matches!(output, Ok(status) if status.success()) {
            let short = unit.trim_end_matches(".service");
            warn!(
                unit,
                "another power daemon is running; it will fight over the same knobs"
            );
            found.push(format!("conflict:{short}"));
        }
    }
    found
}

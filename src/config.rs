//! Configuration file parsing and validation.
//!
//! Reference: `docs/12-configuration.md`, example: `docs/13-config-example.md`
//! (kept in sync with `examples/ampered.toml`, which a test enforces).

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Deserializer};

/// Upper bound for any idle timeout: a notification timeout is passed to the
/// compositor in milliseconds as a `u32` (`docs/04-idle-wayland.md`).
pub const MAX_TIMEOUT: Duration = Duration::from_secs(24 * 60 * 60);

/// The server cycle must not wake up more often than this (`docs/10-long-sleep-rtc.md`).
pub const MIN_CHECK_INTERVAL: Duration = Duration::from_secs(120);

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("cannot read {path}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("cannot parse {path}")]
    Parse {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
    #[error("invalid config: {0}")]
    Invalid(String),
}

fn invalid<T>(msg: impl Into<String>) -> Result<T, ConfigError> {
    Err(ConfigError::Invalid(msg.into()))
}

// ---------------------------------------------------------------- durations

/// Parses a `humantime` duration, with `"0"` meaning "disabled".
pub fn parse_duration(s: &str) -> Result<Duration, String> {
    let trimmed = s.trim();
    if trimmed == "0" {
        return Ok(Duration::ZERO);
    }
    humantime::parse_duration(trimmed).map_err(|e| format!("invalid duration {s:?}: {e}"))
}

/// Renders a duration the way the config and the IPC protocol spell it.
pub fn format_duration(d: Duration) -> String {
    if d.is_zero() {
        "0".to_string()
    } else {
        humantime::format_duration(d).to_string()
    }
}

fn de_duration<'de, D: Deserializer<'de>>(d: D) -> Result<Duration, D::Error> {
    let s = String::deserialize(d)?;
    parse_duration(&s).map_err(serde::de::Error::custom)
}

/// `#[serde(with = "crate::config::duration_str")]` for durations that travel
/// over IPC as the same strings the config uses.
pub mod duration_str {
    use super::{format_duration, parse_duration};
    use serde::{Deserialize, Deserializer, Serializer};
    use std::time::Duration;

    pub fn serialize<S: Serializer>(d: &Duration, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&format_duration(*d))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Duration, D::Error> {
        let s = String::deserialize(d)?;
        parse_duration(&s).map_err(serde::de::Error::custom)
    }
}

// ------------------------------------------------------------------- config

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct Config {
    pub general: General,
    pub wayland: Wayland,
    pub idle: Idle,
    pub backlight: Backlight,
    pub display: Display,
    /// Mode name → mode. Sorted by name so `amperedctl modes` is stable.
    pub modes: BTreeMap<String, Mode>,
    pub auto_mode: AutoMode,
    pub sleep: Sleep,
    pub server: Server,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields, default)]
pub struct General {
    pub log_level: String,
    pub socket: PathBuf,
    pub socket_group: String,
}

impl Default for General {
    fn default() -> Self {
        Self {
            log_level: "info".into(),
            socket: "/run/ampered/ampered.sock".into(),
            socket_group: "users".into(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields, default)]
pub struct Wayland {
    pub runtime_dir: PathBuf,
    pub display: String,
    #[serde(deserialize_with = "de_duration")]
    pub reconnect_max_backoff: Duration,
}

impl Default for Wayland {
    fn default() -> Self {
        Self {
            runtime_dir: "/run/user/1000".into(),
            display: "wayland-1".into(),
            reconnect_max_backoff: Duration::from_secs(60),
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields, default)]
pub struct Idle {
    pub fallback: IdleFallback,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum IdleFallback {
    #[default]
    None,
    Logind,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields, default)]
pub struct Backlight {
    /// Device name under `/sys/class/backlight`, or `"auto"`.
    pub device: String,
    pub dim_percent: u8,
    #[serde(deserialize_with = "de_duration")]
    pub transition: Duration,
    pub min_percent: u8,
}

impl Default for Backlight {
    fn default() -> Self {
        Self {
            device: "auto".into(),
            dim_percent: 10,
            transition: Duration::from_millis(400),
            min_percent: 1,
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields, default)]
pub struct Display {
    pub backend: DisplayBackend,
    pub off_command: Option<String>,
    pub on_command: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum DisplayBackend {
    #[default]
    Wlr,
    Command,
    None,
}

/// A named bundle of hardware knobs and idle timeouts (`docs/08-power-modes.md`).
///
/// A missing hardware key means "don't touch it"; a timeout of `"0"` (or a
/// missing timeout) means the stage is disabled.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct Mode {
    pub platform_profile: Option<String>,
    pub cpu_governor: Option<String>,
    pub epp: Option<String>,
    #[serde(deserialize_with = "de_duration")]
    pub dim_after: Duration,
    #[serde(deserialize_with = "de_duration")]
    pub screen_off_after: Duration,
    #[serde(deserialize_with = "de_duration")]
    pub sleep_after: Duration,
    /// Arbitrary `path = value` writes, applied in declaration order — hence
    /// `toml::Table` (the `preserve_order` feature) rather than a `BTreeMap`.
    pub sysfs: toml::Table,
}

impl Mode {
    /// The `sysfs` table as plain string pairs, in declaration order.
    pub fn sysfs_writes(&self) -> Vec<(&str, String)> {
        self.sysfs
            .iter()
            .filter_map(|(k, v)| scalar_to_string(v).map(|s| (k.as_str(), s)))
            .collect()
    }
}

fn scalar_to_string(v: &toml::Value) -> Option<String> {
    match v {
        toml::Value::String(s) => Some(s.clone()),
        toml::Value::Integer(i) => Some(i.to_string()),
        _ => None,
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields, default)]
pub struct AutoMode {
    pub enabled: bool,
    pub on_ac: String,
    pub on_battery: String,
    pub low_battery_percent: u8,
    pub on_low_battery: String,
}

impl Default for AutoMode {
    fn default() -> Self {
        Self {
            enabled: true,
            on_ac: "balanced".into(),
            on_battery: "powersave".into(),
            low_battery_percent: 20,
            on_low_battery: "powersave".into(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields, default)]
pub struct Sleep {
    pub method: SleepMethod,
    pub respect_inhibitors: bool,
    #[serde(deserialize_with = "de_duration")]
    pub sleep_retry: Duration,
}

impl Default for Sleep {
    fn default() -> Self {
        Self {
            method: SleepMethod::Suspend,
            respect_inhibitors: true,
            sleep_retry: Duration::from_secs(120),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum SleepMethod {
    #[default]
    Suspend,
    Hibernate,
    SuspendThenHibernate,
}

impl SleepMethod {
    /// The `org.freedesktop.login1.Manager` method that performs it.
    pub fn logind_method(self) -> &'static str {
        match self {
            SleepMethod::Suspend => "Suspend",
            SleepMethod::Hibernate => "Hibernate",
            SleepMethod::SuspendThenHibernate => "SuspendThenHibernate",
        }
    }

    pub fn needs_hibernate(self) -> bool {
        matches!(
            self,
            SleepMethod::Hibernate | SleepMethod::SuspendThenHibernate
        )
    }
}

impl fmt::Display for SleepMethod {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            SleepMethod::Suspend => "suspend",
            SleepMethod::Hibernate => "hibernate",
            SleepMethod::SuspendThenHibernate => "suspend-then-hibernate",
        };
        f.write_str(s)
    }
}

/// The long-sleep server cycle (`docs/10-long-sleep-rtc.md`). Parsed and
/// validated here, driven by `core` and `sleep`.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields, default)]
pub struct Server {
    pub enabled: bool,
    pub trigger: ServerTrigger,
    #[serde(deserialize_with = "de_duration")]
    pub grace_period: Duration,
    #[serde(deserialize_with = "de_duration")]
    pub check_interval: Duration,
    #[serde(deserialize_with = "de_duration")]
    pub awake_window: Duration,
    #[serde(deserialize_with = "de_duration")]
    pub alarm_slack: Duration,
    pub battery_critical_percent: u8,
    pub critical_action: CriticalAction,
    pub rtc_backend: RtcBackend,
    pub rtc_device: String,
    pub resume_hook: Option<String>,
}

impl Default for Server {
    fn default() -> Self {
        Self {
            enabled: false,
            trigger: ServerTrigger::AcLost,
            grace_period: Duration::from_secs(180),
            check_interval: Duration::from_secs(20 * 60),
            awake_window: Duration::from_secs(45),
            alarm_slack: Duration::from_secs(90),
            battery_critical_percent: 10,
            critical_action: CriticalAction::Hibernate,
            rtc_backend: RtcBackend::Wakealarm,
            rtc_device: "rtc0".into(),
            resume_hook: None,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ServerTrigger {
    #[default]
    AcLost,
    Manual,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum CriticalAction {
    #[default]
    Hibernate,
    Poweroff,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum RtcBackend {
    #[default]
    Wakealarm,
    Rtcwake,
}

// --------------------------------------------------------------- validation

impl Config {
    /// Reads and validates a config file.
    pub fn load(path: &Path) -> Result<Config, ConfigError> {
        let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.to_path_buf(),
            source,
        })?;
        let config = Config::parse(&text).map_err(|source| ConfigError::Parse {
            path: path.to_path_buf(),
            source,
        })?;
        config.validate()?;
        Ok(config)
    }

    pub fn parse(text: &str) -> Result<Config, toml::de::Error> {
        toml::from_str(text)
    }

    /// Everything that cannot be expressed in the type system.
    ///
    /// An invalid config is the only thing the daemon refuses to start on
    /// (`CLAUDE.md`, rule 3), so this is deliberately strict.
    pub fn validate(&self) -> Result<(), ConfigError> {
        for (name, mode) in &self.modes {
            mode.validate(name)?;
        }

        if self.auto_mode.enabled {
            for (key, name) in [
                ("on_ac", &self.auto_mode.on_ac),
                ("on_battery", &self.auto_mode.on_battery),
                ("on_low_battery", &self.auto_mode.on_low_battery),
            ] {
                if !self.modes.contains_key(name) {
                    return invalid(format!(
                        "[auto_mode] {key} = {name:?}: no such mode in [modes]"
                    ));
                }
            }
        }
        if self.auto_mode.low_battery_percent > 100 {
            return invalid("[auto_mode] low_battery_percent must be 0..=100");
        }

        if self.backlight.dim_percent > 100 {
            return invalid("[backlight] dim_percent must be 0..=100");
        }
        if self.backlight.min_percent > 100 {
            return invalid("[backlight] min_percent must be 0..=100");
        }

        if self.display.backend == DisplayBackend::Command
            && (self.display.off_command.is_none() || self.display.on_command.is_none())
        {
            return invalid(
                "[display] backend = \"command\" requires both off_command and on_command",
            );
        }

        if self.server.check_interval < MIN_CHECK_INTERVAL {
            return invalid(format!(
                "[server] check_interval must be at least {}",
                format_duration(MIN_CHECK_INTERVAL)
            ));
        }
        if self.server.battery_critical_percent > 100 {
            return invalid("[server] battery_critical_percent must be 0..=100");
        }

        Ok(())
    }
}

impl Mode {
    fn validate(&self, name: &str) -> Result<(), ConfigError> {
        for (key, value) in [
            ("dim_after", self.dim_after),
            ("screen_off_after", self.screen_off_after),
            ("sleep_after", self.sleep_after),
        ] {
            if value > MAX_TIMEOUT {
                return invalid(format!(
                    "[modes.{name}] {key} must not exceed {}",
                    format_duration(MAX_TIMEOUT)
                ));
            }
        }

        // Only enabled ("0" means disabled) timeouts take part in the ordering.
        let enabled = [
            ("dim_after", self.dim_after),
            ("screen_off_after", self.screen_off_after),
            ("sleep_after", self.sleep_after),
        ];
        let enabled: Vec<_> = enabled.iter().filter(|(_, v)| !v.is_zero()).collect();
        for pair in enabled.windows(2) {
            let ((a_key, a), (b_key, b)) = (pair[0], pair[1]);
            if a > b {
                return invalid(format!(
                    "[modes.{name}] {a_key} ({}) must not be later than {b_key} ({})",
                    format_duration(*a),
                    format_duration(*b)
                ));
            }
        }

        for (path, value) in self.sysfs.iter() {
            if scalar_to_string(value).is_none() {
                return invalid(format!(
                    "[modes.{name}.sysfs] {path:?}: value must be a string or an integer"
                ));
            }
            if !Path::new(path).is_absolute() {
                return invalid(format!(
                    "[modes.{name}.sysfs] {path:?}: path must be absolute"
                ));
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXAMPLE: &str = include_str!("../examples/ampered.toml");
    const DOC: &str = include_str!("../docs/13-config-example.md");

    fn toml_block(doc: &str, index: usize) -> String {
        doc.split("```toml")
            .nth(index + 1)
            .and_then(|rest| rest.split("```").next())
            .expect("fenced toml block")
            .trim_start_matches('\n')
            .to_string()
    }

    /// `examples/ampered.toml` is generated from the docs; they must not drift.
    #[test]
    fn example_matches_the_documented_one() {
        assert_eq!(EXAMPLE, toml_block(DOC, 0));
    }

    #[test]
    fn example_parses_and_validates() {
        let config = Config::parse(EXAMPLE).expect("parse");
        config.validate().expect("validate");

        assert_eq!(config.general.socket_group, "users");
        assert_eq!(config.backlight.dim_percent, 10);
        assert_eq!(config.backlight.transition, Duration::from_millis(400));
        assert_eq!(config.modes.len(), 4);
        assert_eq!(config.modes["balanced"].dim_after, Duration::from_secs(300));
        assert_eq!(config.modes["server"].sleep_after, Duration::ZERO);
        assert_eq!(
            config.modes["server"].sysfs_writes(),
            vec![(
                "/sys/class/leds/platform::kbd_backlight/brightness",
                "0".to_string()
            )]
        );
        assert_eq!(config.sleep.method, SleepMethod::Suspend);
        assert!(!config.server.enabled);
        assert_eq!(config.server.resume_hook, None);
    }

    /// The second block in the doc is a fragment of the server variant.
    #[test]
    fn server_variant_fragment_parses() {
        let fragment = toml_block(DOC, 1);
        let config = Config::parse(&fragment).expect("parse");
        assert!(config.server.enabled);
        assert_eq!(config.server.check_interval, Duration::from_secs(30 * 60));
        assert_eq!(
            config.server.resume_hook.as_deref(),
            Some("systemctl start docker.service")
        );
    }

    #[test]
    fn defaults_match_the_reference() {
        let config = Config::default();
        assert_eq!(config.general.log_level, "info");
        assert_eq!(
            config.general.socket,
            PathBuf::from("/run/ampered/ampered.sock")
        );
        assert_eq!(config.wayland.display, "wayland-1");
        assert_eq!(
            config.wayland.reconnect_max_backoff,
            Duration::from_secs(60)
        );
        assert_eq!(config.idle.fallback, IdleFallback::None);
        assert_eq!(config.backlight.device, "auto");
        assert_eq!(config.backlight.min_percent, 1);
        assert_eq!(config.display.backend, DisplayBackend::Wlr);
        assert_eq!(config.auto_mode.low_battery_percent, 20);
        assert_eq!(config.sleep.sleep_retry, Duration::from_secs(120));
        assert_eq!(config.server.grace_period, Duration::from_secs(180));
        assert_eq!(config.server.rtc_device, "rtc0");
    }

    #[test]
    fn zero_means_disabled() {
        assert_eq!(parse_duration("0").unwrap(), Duration::ZERO);
        assert_eq!(parse_duration(" 0 ").unwrap(), Duration::ZERO);
        assert_eq!(parse_duration("1h30m").unwrap(), Duration::from_secs(5400));
        assert!(parse_duration("").is_err());
        assert!(parse_duration("soon").is_err());
        assert_eq!(format_duration(Duration::ZERO), "0");
        assert_eq!(format_duration(Duration::from_secs(300)), "5m");
    }

    #[test]
    fn unknown_keys_are_rejected() {
        let err = Config::parse("[general]\nlog_levle = \"info\"\n").unwrap_err();
        assert!(err.to_string().contains("log_levle"), "{err}");
    }

    #[test]
    fn timeouts_must_not_decrease() {
        let text = r#"
            [modes.x]
            dim_after = "10m"
            screen_off_after = "5m"
        "#;
        let err = Config::parse(text).unwrap().validate().unwrap_err();
        assert!(err.to_string().contains("dim_after"), "{err}");
    }

    #[test]
    fn disabled_stages_are_skipped_by_the_ordering_check() {
        let text = r#"
            [auto_mode]
            enabled = false
            [modes.x]
            dim_after = "0"
            screen_off_after = "5m"
            sleep_after = "0"
        "#;
        Config::parse(text).unwrap().validate().expect("valid");
    }

    #[test]
    fn timeouts_are_capped_at_24h() {
        let text = "[auto_mode]\nenabled = false\n[modes.x]\nsleep_after = \"25h\"\n";
        let err = Config::parse(text).unwrap().validate().unwrap_err();
        assert!(
            err.to_string().contains("sleep_after must not exceed"),
            "{err}"
        );
    }

    #[test]
    fn auto_mode_must_reference_existing_modes() {
        let text = r#"
            [modes.balanced]
            [auto_mode]
            on_ac = "balanced"
            on_battery = "nope"
            on_low_battery = "balanced"
        "#;
        let err = Config::parse(text).unwrap().validate().unwrap_err();
        assert!(err.to_string().contains("no such mode"), "{err}");
    }

    #[test]
    fn disabled_auto_mode_needs_no_modes() {
        Config::parse("[auto_mode]\nenabled = false\n")
            .unwrap()
            .validate()
            .expect("valid");
    }

    #[test]
    fn command_backend_needs_both_commands() {
        let text = "[auto_mode]\nenabled = false\n[display]\nbackend = \"command\"\noff_command = \"true\"\n";
        let err = Config::parse(text).unwrap().validate().unwrap_err();
        assert!(err.to_string().contains("on_command"), "{err}");
    }

    #[test]
    fn check_interval_has_a_floor() {
        let text = "[auto_mode]\nenabled = false\n[server]\ncheck_interval = \"1m\"\n";
        let err = Config::parse(text).unwrap().validate().unwrap_err();
        assert!(err.to_string().contains("check_interval"), "{err}");
    }

    #[test]
    fn sysfs_paths_must_be_absolute() {
        let text = "[auto_mode]\nenabled = false\n[modes.x.sysfs]\n\"relative/path\" = \"1\"\n";
        let err = Config::parse(text).unwrap().validate().unwrap_err();
        assert!(err.to_string().contains("absolute"), "{err}");
    }

    #[test]
    fn sysfs_keeps_declaration_order() {
        let text = r#"
            [modes.x.sysfs]
            "/b" = "2"
            "/a" = 1
            "/c" = "3"
        "#;
        let config = Config::parse(text).unwrap();
        let writes = config.modes["x"].sysfs_writes();
        assert_eq!(
            writes,
            vec![
                ("/b", "2".to_string()),
                ("/a", "1".to_string()),
                ("/c", "3".to_string())
            ]
        );
    }

    #[test]
    fn percentages_are_range_checked() {
        for (text, key) in [
            (
                "[auto_mode]\nenabled = false\n[backlight]\ndim_percent = 120\n",
                "dim_percent",
            ),
            (
                "[auto_mode]\nenabled = false\n[backlight]\nmin_percent = 101\n",
                "min_percent",
            ),
            (
                "[auto_mode]\nenabled = false\nlow_battery_percent = 200\n",
                "low_battery_percent",
            ),
            (
                "[auto_mode]\nenabled = false\n[server]\nbattery_critical_percent = 150\n",
                "battery_critical_percent",
            ),
        ] {
            let config = Config::parse(text).expect("parse");
            let err = config.validate().unwrap_err();
            assert!(err.to_string().contains(key), "{key}: {err}");
        }
    }

    /// A percentage of exactly 100 is a legal value, not an off-by-one victim.
    #[test]
    fn a_hundred_percent_is_allowed() {
        Config::parse(
            "[auto_mode]\nenabled = false\n[backlight]\ndim_percent = 100\nmin_percent = 100\n",
        )
        .unwrap()
        .validate()
        .expect("valid");
    }
}

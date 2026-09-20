//! `logind.conf` and its drop-ins, read for the lid-switch and `IdleAction`
//! checks (`docs/09-sleep-logind.md`).
//!
//! logind does not expose these settings on the bus, so the files are read
//! the way logind reads them. Nothing here writes anything.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use tracing::{debug, warn};

/// In order of precedence for the main file; drop-ins are taken from all of them.
const DIRS: [&str; 4] = [
    "etc/systemd",
    "run/systemd",
    "usr/local/lib/systemd",
    "usr/lib/systemd",
];
const MAIN: &str = "logind.conf";
const DROP_IN_DIR: &str = "logind.conf.d";

const LID_KEYS: [&str; 3] = [
    "HandleLidSwitch",
    "HandleLidSwitchExternalPower",
    "HandleLidSwitchDocked",
];

/// The `[Login]` keys we care about, with logind's defaults filled in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoginSettings {
    pub lid_switch: String,
    pub lid_switch_external_power: String,
    pub lid_switch_docked: String,
    pub idle_action: String,
}

impl LoginSettings {
    fn from_keys(keys: &BTreeMap<String, String>) -> LoginSettings {
        let get = |key: &str, default: &str| {
            keys.get(key)
                .cloned()
                .unwrap_or_else(|| default.to_string())
        };
        let lid_switch = get("HandleLidSwitch", "suspend");
        LoginSettings {
            // Unset means "same as HandleLidSwitch" (logind.conf(5)).
            lid_switch_external_power: get("HandleLidSwitchExternalPower", &lid_switch),
            lid_switch_docked: get("HandleLidSwitchDocked", "ignore"),
            idle_action: get("IdleAction", "ignore"),
            lid_switch,
        }
    }

    /// The lid settings that would put the machine to sleep behind the
    /// server cycle's back, as `Key=value`.
    pub fn lid_actions(&self) -> Vec<String> {
        LID_KEYS
            .iter()
            .zip([
                &self.lid_switch,
                &self.lid_switch_external_power,
                &self.lid_switch_docked,
            ])
            .filter(|(_, value)| *value != "ignore")
            .map(|(key, value)| format!("{key}={value}"))
            .collect()
    }
}

/// Reads the settings under `root` (`/` on a live system).
pub fn read(root: &Path) -> LoginSettings {
    LoginSettings::from_keys(&effective_keys(&collect_files(root)))
}

/// Everything the check has to say, as `status.degraded` entries; each one
/// is also logged. `server` is `[server] enabled`.
pub fn check(root: &Path, server: bool) -> Vec<String> {
    let settings = read(root);
    debug!(?settings, "logind.conf");
    let mut degraded = Vec::new();
    if server {
        let actions = settings.lid_actions();
        if !actions.is_empty() {
            warn!(
                settings = actions.join(", "),
                "logind handles the lid itself; closing it would sleep without an RTC alarm \
                 (set HandleLidSwitch*=ignore in logind.conf)"
            );
            degraded.push("lid-switch".to_string());
        }
    }
    if settings.idle_action != "ignore" {
        warn!(
            idle_action = settings.idle_action,
            "logind acts on idle as well; set IdleAction=ignore in logind.conf"
        );
        degraded.push("idle-action".to_string());
    }
    degraded
}

/// The main file plus the drop-ins, in the order they are applied: the
/// main file first, then the drop-ins by file name, a same-named drop-in in
/// an earlier directory of `DIRS` shadowing the later ones.
fn collect_files(root: &Path) -> Vec<(PathBuf, String)> {
    let mut files = Vec::new();
    if let Some(main) = DIRS
        .iter()
        .map(|dir| root.join(dir).join(MAIN))
        .find(|path| path.is_file())
        && let Ok(text) = fs::read_to_string(&main)
    {
        files.push((main, text));
    }

    let mut drop_ins: BTreeMap<String, PathBuf> = BTreeMap::new();
    for dir in DIRS {
        let Ok(entries) = fs::read_dir(root.join(dir).join(DROP_IN_DIR)) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if !name.ends_with(".conf") || !path.is_file() {
                continue;
            }
            // First directory wins for a given name.
            drop_ins.entry(name.to_string()).or_insert(path);
        }
    }
    for (_, path) in drop_ins {
        if let Ok(text) = fs::read_to_string(&path) {
            files.push((path, text));
        }
    }
    files
}

/// Later files override earlier ones, key by key; only `[Login]` counts.
fn effective_keys(files: &[(PathBuf, String)]) -> BTreeMap<String, String> {
    let mut keys = BTreeMap::new();
    for (_, text) in files {
        for (key, value) in login_section(text) {
            if value.is_empty() {
                keys.remove(&key);
            } else {
                keys.insert(key, value);
            }
        }
    }
    keys
}

/// `Key=value` pairs from the `[Login]` section. An empty value resets the
/// key to its default, as in every systemd config file.
fn login_section(text: &str) -> Vec<(String, String)> {
    let mut pairs = Vec::new();
    let mut in_login = false;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if let Some(section) = line.strip_prefix('[') {
            in_login = section.trim_end_matches(']').trim() == "Login";
            continue;
        }
        if !in_login {
            continue;
        }
        if let Some((key, value)) = line.split_once('=') {
            pairs.push((key.trim().to_string(), value.trim().to_string()));
        }
    }
    pairs
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(root: &Path, rel: &str, text: &str) {
        let path = root.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    #[test]
    fn defaults_without_any_file() {
        let root = tempfile::tempdir().unwrap();
        let settings = read(root.path());
        assert_eq!(settings.lid_switch, "suspend");
        assert_eq!(settings.lid_switch_external_power, "suspend");
        assert_eq!(settings.lid_switch_docked, "ignore");
        assert_eq!(settings.idle_action, "ignore");
        assert_eq!(
            settings.lid_actions(),
            vec![
                "HandleLidSwitch=suspend",
                "HandleLidSwitchExternalPower=suspend"
            ]
        );
        assert_eq!(check(root.path(), true), vec!["lid-switch"]);
        assert!(check(root.path(), false).is_empty());
    }

    #[test]
    fn the_shipped_file_is_all_comments() {
        let root = tempfile::tempdir().unwrap();
        write(
            root.path(),
            "etc/systemd/logind.conf",
            "[Login]\n#HandleLidSwitch=suspend\n#IdleAction=ignore\n",
        );
        assert_eq!(read(root.path()).lid_switch, "suspend");
    }

    #[test]
    fn drop_ins_override_the_main_file_in_name_order() {
        let root = tempfile::tempdir().unwrap();
        write(
            root.path(),
            "etc/systemd/logind.conf",
            "[Login]\nHandleLidSwitch=hibernate\nIdleAction=suspend\n",
        );
        write(
            root.path(),
            "usr/lib/systemd/logind.conf.d/10-vendor.conf",
            "[Login]\nHandleLidSwitch=suspend\n",
        );
        write(
            root.path(),
            "etc/systemd/logind.conf.d/50-ampered.conf",
            "[Login]\nHandleLidSwitch=ignore\nHandleLidSwitchExternalPower=ignore\n\
             HandleLidSwitchDocked=ignore\nIdleAction=ignore\n",
        );
        let settings = read(root.path());
        assert_eq!(settings.lid_switch, "ignore");
        assert!(settings.lid_actions().is_empty());
        assert!(check(root.path(), true).is_empty());
    }

    #[test]
    fn a_same_named_drop_in_in_etc_shadows_usr() {
        let root = tempfile::tempdir().unwrap();
        write(
            root.path(),
            "usr/lib/systemd/logind.conf.d/lid.conf",
            "[Login]\nHandleLidSwitch=suspend\n",
        );
        write(
            root.path(),
            "etc/systemd/logind.conf.d/lid.conf",
            "[Login]\nHandleLidSwitch=lock\n",
        );
        // Not a `.conf`: ignored.
        write(
            root.path(),
            "etc/systemd/logind.conf.d/lid.conf.bak",
            "[Login]\nHandleLidSwitch=poweroff\n",
        );
        assert_eq!(read(root.path()).lid_switch, "lock");
        assert_eq!(check(root.path(), true), vec!["lid-switch"]);
    }

    #[test]
    fn external_power_follows_the_lid_unless_set() {
        let root = tempfile::tempdir().unwrap();
        write(
            root.path(),
            "etc/systemd/logind.conf",
            "[Login]\nHandleLidSwitch=ignore\n",
        );
        let settings = read(root.path());
        assert_eq!(settings.lid_switch_external_power, "ignore");
        write(
            root.path(),
            "etc/systemd/logind.conf",
            "[Login]\nHandleLidSwitch=ignore\nHandleLidSwitchExternalPower=suspend\n",
        );
        assert_eq!(
            read(root.path()).lid_actions(),
            vec!["HandleLidSwitchExternalPower=suspend"]
        );
    }

    #[test]
    fn idle_action_is_checked_in_every_mode() {
        let root = tempfile::tempdir().unwrap();
        write(
            root.path(),
            "etc/systemd/logind.conf",
            "[Login]\nHandleLidSwitch=ignore\nIdleAction=suspend\n",
        );
        assert_eq!(check(root.path(), false), vec!["idle-action"]);
        assert_eq!(check(root.path(), true), vec!["idle-action"]);
    }

    #[test]
    fn only_the_login_section_counts() {
        let pairs = login_section(
            "[Sleep]\nHandleLidSwitch=ignore\n[Login]\n  IdleAction = lock  \n; comment\n\
             [Other]\nIdleAction=suspend\n[Login]\nHandleLidSwitchDocked=suspend\n",
        );
        assert_eq!(
            pairs,
            vec![
                ("IdleAction".to_string(), "lock".to_string()),
                ("HandleLidSwitchDocked".to_string(), "suspend".to_string()),
            ]
        );
    }

    #[test]
    fn an_empty_value_resets_to_the_default() {
        let root = tempfile::tempdir().unwrap();
        write(
            root.path(),
            "etc/systemd/logind.conf",
            "[Login]\nIdleAction=suspend\n",
        );
        write(
            root.path(),
            "etc/systemd/logind.conf.d/reset.conf",
            "[Login]\nIdleAction=\n",
        );
        assert_eq!(read(root.path()).idle_action, "ignore");
    }
}

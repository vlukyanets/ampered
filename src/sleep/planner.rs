//! The daemon's side of the server cycle: which wake was this, and the
//! `resume_hook`.
//!
//! Reference: `docs/10-long-sleep-rtc.md`, ADR-13. The decisions stay in the
//! engine; this module supplies the two things the engine cannot have — a
//! clock and a subprocess.

use std::fmt;
use std::process::Stdio;
use std::time::{Duration, SystemTime};

use tokio::process::Command;
use tracing::{error, info, warn};

/// `resume_hook` gets this long before it is killed.
pub const HOOK_TIMEOUT: Duration = Duration::from_secs(5 * 60);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wake {
    /// The RTC alarm fired.
    Scheduled,
    /// Lid, keyboard, power button — anything that was not the alarm.
    User,
}

impl fmt::Display for Wake {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Wake::Scheduled => "scheduled",
            Wake::User => "user",
        })
    }
}

/// `now` is taken after the resume; `scheduled` is the alarm the daemon
/// armed. Within `slack` of it the wake is the alarm's; anything else, and a
/// wake with no alarm armed at all, is the user's.
pub fn classify_wake(now: SystemTime, scheduled: Option<SystemTime>, slack: Duration) -> Wake {
    let Some(scheduled) = scheduled else {
        return Wake::User;
    };
    let off_by = match now.duration_since(scheduled) {
        Ok(late) => late,
        Err(early) => early.duration(),
    };
    if off_by <= slack {
        Wake::Scheduled
    } else {
        Wake::User
    }
}

/// What the daemon remembers between `ScheduleWake` and the next `Resumed`.
#[derive(Debug, Default)]
pub struct Planner {
    scheduled: Option<SystemTime>,
}

impl Planner {
    pub fn armed(&mut self, at: SystemTime) {
        self.scheduled = Some(at);
    }

    pub fn disarmed(&mut self) {
        self.scheduled = None;
    }

    pub fn scheduled(&self) -> Option<SystemTime> {
        self.scheduled
    }

    /// Classifies the wake that just happened and forgets the alarm: the
    /// kernel has cleared it too.
    pub fn classify(&mut self, now: SystemTime, slack: Duration) -> Wake {
        let wake = classify_wake(now, self.scheduled.take(), slack);
        info!(%wake, "woke up in the long-sleep cycle");
        wake
    }
}

/// Runs `resume_hook` as `sh -c` in the background; the outcome goes to the
/// log and nowhere else (`docs/10-long-sleep-rtc.md`).
pub fn spawn_hook(command: String) {
    tokio::spawn(async move {
        info!(command, "running resume_hook");
        let child = Command::new("sh")
            .arg("-c")
            .arg(&command)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn();
        let child = match child {
            Ok(child) => child,
            Err(err) => {
                error!(%err, "cannot start resume_hook");
                return;
            }
        };
        match tokio::time::timeout(HOOK_TIMEOUT, child.wait_with_output()).await {
            Ok(Ok(output)) => {
                let stdout = String::from_utf8_lossy(&output.stdout);
                let stderr = String::from_utf8_lossy(&output.stderr);
                if output.status.success() {
                    info!(stdout = %stdout.trim(), stderr = %stderr.trim(), "resume_hook finished");
                } else {
                    warn!(
                        status = %output.status,
                        stdout = %stdout.trim(),
                        stderr = %stderr.trim(),
                        "resume_hook failed"
                    );
                }
            }
            Ok(Err(err)) => error!(%err, "resume_hook could not be waited for"),
            // Dropping the future kills the child (`kill_on_drop`).
            Err(_) => error!(timeout = ?HOOK_TIMEOUT, "resume_hook timed out, killed"),
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    const SLACK: Duration = Duration::from_secs(90);

    fn at(secs: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(secs)
    }

    #[test]
    fn within_the_slack_is_the_alarm() {
        assert_eq!(
            classify_wake(at(1000), Some(at(1000)), SLACK),
            Wake::Scheduled
        );
        assert_eq!(
            classify_wake(at(1090), Some(at(1000)), SLACK),
            Wake::Scheduled
        );
        assert_eq!(
            classify_wake(at(910), Some(at(1000)), SLACK),
            Wake::Scheduled
        );
    }

    #[test]
    fn beyond_the_slack_is_the_user() {
        assert_eq!(classify_wake(at(1091), Some(at(1000)), SLACK), Wake::User);
        assert_eq!(classify_wake(at(909), Some(at(1000)), SLACK), Wake::User);
        assert_eq!(classify_wake(at(5000), Some(at(1000)), SLACK), Wake::User);
    }

    #[test]
    fn no_alarm_is_the_user() {
        assert_eq!(classify_wake(at(1000), None, SLACK), Wake::User);
    }

    #[test]
    fn planner_forgets_the_alarm_after_classifying() {
        let mut planner = Planner::default();
        assert_eq!(planner.classify(at(1000), SLACK), Wake::User);

        planner.armed(at(2000));
        assert_eq!(planner.scheduled(), Some(at(2000)));
        assert_eq!(planner.classify(at(2010), SLACK), Wake::Scheduled);
        assert_eq!(planner.scheduled(), None);

        planner.armed(at(3000));
        planner.disarmed();
        assert_eq!(planner.classify(at(3000), SLACK), Wake::User);
    }

    #[tokio::test]
    async fn hook_runs_in_the_background() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("ran");
        spawn_hook(format!("echo hi > {}", marker.display()));
        for _ in 0..50 {
            if marker.exists() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("the hook did not run");
    }
}

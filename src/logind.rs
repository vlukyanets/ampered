//! Sleep and inhibitors through `org.freedesktop.login1`.
//!
//! Reference: `docs/09-sleep-logind.md`. We never write to `/sys/power/state`
//! (ADR-4): going through logind is what makes hooks, inhibitors and
//! `PrepareForSleep` work for everybody else on the system.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};
use zbus::export::futures_core::Stream;
use zbus::zvariant::OwnedFd;

use crate::config::{Config, SleepMethod};
use crate::core::{Event, Inhibitor, Stage};

/// logind has no "inhibitors changed" signal, so the cache is refreshed on a
/// timer and re-checked at the moment of the suspend (ADR-11).
pub const INHIBITOR_POLL: Duration = Duration::from_secs(30);

/// `IdleHint` is coarse anyway; more often would only load the bus
/// (`docs/04-idle-wayland.md`).
pub const IDLE_HINT_POLL: Duration = Duration::from_secs(30);

const STATE_FILE: &str = "state.json";
const DEFAULT_STATE_DIR: &str = "/var/lib/ampered";

#[zbus::proxy(
    interface = "org.freedesktop.login1.Manager",
    default_service = "org.freedesktop.login1",
    default_path = "/org/freedesktop/login1"
)]
trait Manager {
    fn suspend(&self, interactive: bool) -> zbus::Result<()>;
    fn hibernate(&self, interactive: bool) -> zbus::Result<()>;
    fn suspend_then_hibernate(&self, interactive: bool) -> zbus::Result<()>;
    fn power_off(&self, interactive: bool) -> zbus::Result<()>;
    fn can_hibernate(&self) -> zbus::Result<String>;
    fn inhibit(&self, what: &str, who: &str, why: &str, mode: &str) -> zbus::Result<OwnedFd>;
    /// `(what, who, why, mode, uid, pid)`
    #[allow(clippy::type_complexity)]
    fn list_inhibitors(&self) -> zbus::Result<Vec<(String, String, String, String, u32, u32)>>;

    #[zbus(property)]
    fn idle_hint(&self) -> zbus::Result<bool>;
    /// Microseconds since the epoch, `CLOCK_REALTIME`; `0` when not idle.
    #[zbus(property)]
    fn idle_since_hint(&self) -> zbus::Result<u64>;

    #[zbus(signal)]
    fn prepare_for_sleep(&self, start: bool) -> zbus::Result<()>;
}

/// What the `IdleHint` poller needs from the daemon (`[idle] fallback`).
#[derive(Debug, Clone, Default)]
pub struct IdleFallback {
    /// `[idle] fallback = "logind"`, live across reloads.
    pub enabled: bool,
    /// The compositor backend is connected, so the fallback stays out of it.
    pub compositor: bool,
    /// The current mode's `sleep_after`; `None` disables the stage.
    pub sleep_after: Option<Duration>,
}

impl IdleFallback {
    pub fn engaged(&self) -> bool {
        self.enabled && !self.compositor
    }
}

pub type SharedFallback = Arc<Mutex<IdleFallback>>;

/// What the daemon writes before going to sleep, and reads back at startup
/// to pick up an interrupted server cycle (`docs/10-long-sleep-rtc.md`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SavedState {
    pub state: String,
    pub mode: String,
    /// `regular` or `long-sleep`.
    pub reason: String,
    /// RFC 3339, the alarm armed for the long sleep.
    pub scheduled_wake: Option<String>,
    pub saved_at: Option<String>,
}

impl SavedState {
    pub fn in_long_sleep(&self) -> bool {
        self.state.starts_with("LongSleep")
    }
}

/// The state saved before the last sleep, if the file is there and readable.
pub fn load_saved_state() -> Option<SavedState> {
    let path = state_path();
    let text = std::fs::read_to_string(&path).ok()?;
    match serde_json::from_str(&text) {
        Ok(state) => Some(state),
        Err(err) => {
            warn!(path = %path.display(), %err, "ignoring an unreadable state file");
            None
        }
    }
}

pub type SharedState = Arc<Mutex<SavedState>>;

#[derive(Debug)]
enum Request {
    Sleep { method: SleepMethod, force: bool },
    PowerOff,
}

#[derive(Clone)]
pub struct LogindHandle {
    requests: mpsc::Sender<Request>,
    hibernate_available: bool,
}

impl LogindHandle {
    /// `false` means the request never reached the bus — the caller has to
    /// undo the FSM's move into `Suspending`, or it would wait there forever.
    #[must_use]
    pub fn suspend(&self, method: SleepMethod, force: bool) -> bool {
        let method = self.usable(method);
        match self.requests.try_send(Request::Sleep { method, force }) {
            Ok(()) => true,
            Err(err) => {
                warn!(%err, "logind actor is busy, suspend request dropped");
                false
            }
        }
    }

    pub fn power_off(&self) {
        let _ = self.requests.try_send(Request::PowerOff);
    }

    pub fn hibernate_available(&self) -> bool {
        self.hibernate_available
    }

    /// Better to sleep than to fail outright (`docs/09-sleep-logind.md`).
    fn usable(&self, method: SleepMethod) -> SleepMethod {
        if method.needs_hibernate() && !self.hibernate_available {
            warn!(%method, "hibernate is unavailable, falling back to suspend");
            return SleepMethod::Suspend;
        }
        method
    }
}

/// Connects to the system bus and starts the three logind tasks: the
/// `PrepareForSleep` listener with our delay lock, the inhibitor poll, and the
/// executor of sleep requests. `None` means no D-Bus — the daemon runs on
/// without the ability to sleep.
pub async fn connect(
    config: Arc<Config>,
    events: mpsc::Sender<Event>,
    saved: SharedState,
    fallback: SharedFallback,
) -> Option<LogindHandle> {
    let connection = match zbus::Connection::system().await {
        Ok(connection) => connection,
        Err(err) => {
            warn!(%err, "no system bus; sleep is unavailable");
            return None;
        }
    };
    let manager = match ManagerProxy::new(&connection).await {
        Ok(manager) => manager,
        Err(err) => {
            warn!(%err, "no logind on the bus; sleep is unavailable");
            return None;
        }
    };

    let hibernate_available = hibernate_available(&manager).await;
    if !hibernate_available {
        warn!("hibernate is not configured; it will degrade to suspend");
    }

    let (requests_tx, requests_rx) = mpsc::channel(4);
    tokio::spawn(sleep_listener(
        manager.clone(),
        events.clone(),
        saved,
        state_path(),
    ));
    tokio::spawn(inhibitor_poll(manager.clone(), events.clone()));
    tokio::spawn(idle_hint_poll(manager.clone(), events.clone(), fallback));
    tokio::spawn(executor(manager, config, events, requests_rx));

    info!("connected to logind");
    Some(LogindHandle {
        requests: requests_tx,
        hibernate_available,
    })
}

/// Both checks from `docs/09-sleep-logind.md`: logind's opinion and the kernel's.
async fn hibernate_available(manager: &ManagerProxy<'_>) -> bool {
    match manager.can_hibernate().await {
        Ok(answer) if answer == "yes" => {}
        Ok(answer) => {
            debug!(answer, "logind cannot hibernate");
            return false;
        }
        Err(err) => {
            debug!(%err, "CanHibernate failed");
            return false;
        }
    }
    match std::fs::read_to_string("/sys/power/disk") {
        Ok(modes) => !modes.trim().is_empty() && modes.trim() != "[disabled]",
        Err(err) => {
            debug!(%err, "cannot read /sys/power/disk");
            false
        }
    }
}

/// Holds a `delay` inhibitor for the daemon's lifetime and releases it only
/// for the few moments around an actual suspend.
async fn sleep_listener(
    manager: ManagerProxy<'static>,
    events: mpsc::Sender<Event>,
    saved: SharedState,
    state_path: PathBuf,
) {
    let mut lock = take_delay_lock(&manager).await;
    let mut signals = match manager.receive_prepare_for_sleep().await {
        Ok(signals) => signals,
        Err(err) => {
            error!(%err, "cannot subscribe to PrepareForSleep");
            return;
        }
    };

    while let Some(signal) = next(&mut signals).await {
        let starting = match signal.args() {
            Ok(args) => args.start,
            Err(err) => {
                warn!(%err, "malformed PrepareForSleep");
                continue;
            }
        };

        if starting {
            info!("the system is going to sleep");
            write_state(&state_path, &saved);
            if events.send(Event::Suspending).await.is_err() {
                return;
            }
            // Releasing the fd is what lets logind proceed.
            lock.take();
        } else {
            info!("the system woke up");
            lock = take_delay_lock(&manager).await;
            if events.send(Event::Resumed).await.is_err() {
                return;
            }
        }
    }
}

/// zbus does not re-export `StreamExt`, and one `next` is not worth a
/// dependency on `futures-util`.
async fn next<S: Stream + Unpin>(stream: &mut S) -> Option<S::Item> {
    std::future::poll_fn(|cx| std::pin::Pin::new(&mut *stream).poll_next(cx)).await
}

async fn take_delay_lock(manager: &ManagerProxy<'_>) -> Option<OwnedFd> {
    match manager
        .inhibit("sleep", "ampered", "save state before sleep", "delay")
        .await
    {
        Ok(fd) => Some(fd),
        Err(err) => {
            warn!(%err, "no delay lock; state will not be saved before sleep");
            None
        }
    }
}

fn state_path() -> PathBuf {
    let dir = std::env::var_os("STATE_DIRECTORY")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_STATE_DIR));
    dir.join(STATE_FILE)
}

fn write_state(path: &PathBuf, saved: &SharedState) {
    let mut state = crate::locked(saved).clone();
    state.saved_at = Some(humantime::format_rfc3339_seconds(SystemTime::now()).to_string());
    let Ok(text) = serde_json::to_string(&state) else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    match std::fs::write(path, text) {
        Ok(()) => debug!(path = %path.display(), state = state.state, "state saved"),
        // Not fatal: only the v0.2 wake classifier needs this file.
        Err(err) => debug!(path = %path.display(), %err, "cannot save state"),
    }
}

async fn inhibitor_poll(manager: ManagerProxy<'static>, events: mpsc::Sender<Event>) {
    loop {
        let blockers = blocking_inhibitors(&manager).await;
        if events.send(Event::Inhibitors(blockers)).await.is_err() {
            return;
        }
        tokio::time::sleep(INHIBITOR_POLL).await;
    }
}

/// Only `block` mode counts: logind waits for `delay` inhibitors by itself.
async fn blocking_inhibitors(manager: &ManagerProxy<'_>) -> Vec<Inhibitor> {
    let listed = match manager.list_inhibitors().await {
        Ok(listed) => listed,
        Err(err) => {
            debug!(%err, "ListInhibitors failed");
            return Vec::new();
        }
    };
    listed
        .into_iter()
        .filter(|(what, _, _, mode, _, _)| mode == "block" && what.split(':').any(|w| w == "sleep"))
        .map(|(what, who, why, _, _, _)| Inhibitor { what, who, why })
        .collect()
}

/// The coarse idle source for compositors without `ext-idle-notify-v1`
/// (`docs/04-idle-wayland.md`). Only the `sleep` stage; `IdleHint` is not
/// fine enough for a dim.
async fn idle_hint_poll(
    manager: ManagerProxy<'static>,
    events: mpsc::Sender<Event>,
    fallback: SharedFallback,
) {
    let mut fired = false;
    loop {
        tokio::time::sleep(IDLE_HINT_POLL).await;
        let control = crate::locked(&fallback).clone();
        if !control.engaged() {
            fired = false;
            continue;
        }
        let (hint, since) = match (manager.idle_hint().await, manager.idle_since_hint().await) {
            (Ok(hint), Ok(since)) => (hint, since),
            (Err(err), _) | (_, Err(err)) => {
                debug!(%err, "cannot read IdleHint");
                continue;
            }
        };
        let idle_for = if hint && since > 0 {
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH + Duration::from_micros(since))
                .unwrap_or(Duration::ZERO)
        } else {
            Duration::ZERO
        };
        if let Some(event) = fallback_event(hint, idle_for, control.sleep_after, &mut fired) {
            info!(?event, idle_for = ?idle_for, "logind idle fallback");
            if events.send(event).await.is_err() {
                return;
            }
        }
    }
}

/// `fired` remembers that the stage was delivered, so it goes out once per
/// stretch of idleness and `Activity` once when it ends.
fn fallback_event(
    hint: bool,
    idle_for: Duration,
    sleep_after: Option<Duration>,
    fired: &mut bool,
) -> Option<Event> {
    match sleep_after {
        Some(after) if hint && idle_for >= after => {
            if *fired {
                return None;
            }
            *fired = true;
            Some(Event::Idle(Stage::Sleep))
        }
        _ if *fired && !hint => {
            *fired = false;
            Some(Event::Activity)
        }
        _ => None,
    }
}

async fn executor(
    manager: ManagerProxy<'static>,
    config: Arc<Config>,
    events: mpsc::Sender<Event>,
    mut requests: mpsc::Receiver<Request>,
) {
    while let Some(request) = requests.recv().await {
        match request {
            Request::Sleep { method, force } => {
                // The cached list the engine used can be seconds old; check
                // again now that we are about to act (ADR-11).
                if !force && config.sleep.respect_inhibitors {
                    let blockers = blocking_inhibitors(&manager).await;
                    if !blockers.is_empty() {
                        info!(?blockers, "suspend refused by inhibitors");
                        if events.send(Event::SleepBlocked(blockers)).await.is_err() {
                            return;
                        }
                        continue;
                    }
                }

                info!(%method, force, "asking logind to sleep");
                let result = match method {
                    SleepMethod::Suspend => manager.suspend(false).await,
                    SleepMethod::Hibernate => manager.hibernate(false).await,
                    SleepMethod::SuspendThenHibernate => {
                        manager.suspend_then_hibernate(false).await
                    }
                };
                if let Err(err) = result {
                    error!(%err, %method, "logind refused to sleep");
                    // Nothing is going to happen, so let the FSM back out.
                    let _ = events.send(Event::Activity).await;
                }
            }
            Request::PowerOff => {
                info!("asking logind to power off");
                if let Err(err) = manager.power_off(false).await {
                    error!(%err, "logind refused to power off");
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SLEEP_AFTER: Option<Duration> = Some(Duration::from_secs(600));

    #[test]
    fn fallback_fires_once_and_resets_on_activity() {
        let mut fired = false;
        let short = Duration::from_secs(60);
        let long = Duration::from_secs(900);

        assert_eq!(
            fallback_event(false, Duration::ZERO, SLEEP_AFTER, &mut fired),
            None
        );
        assert_eq!(fallback_event(true, short, SLEEP_AFTER, &mut fired), None);
        assert_eq!(
            fallback_event(true, long, SLEEP_AFTER, &mut fired),
            Some(Event::Idle(Stage::Sleep))
        );
        assert!(fired);
        assert_eq!(fallback_event(true, long, SLEEP_AFTER, &mut fired), None);
        assert_eq!(
            fallback_event(false, Duration::ZERO, SLEEP_AFTER, &mut fired),
            Some(Event::Activity)
        );
        assert!(!fired);
        assert_eq!(
            fallback_event(false, Duration::ZERO, SLEEP_AFTER, &mut fired),
            None
        );
    }

    #[test]
    fn fallback_with_the_stage_disabled() {
        let mut fired = false;
        assert_eq!(
            fallback_event(true, Duration::from_secs(9000), None, &mut fired),
            None
        );
        // Disabled while fired (a mode change): Activity when the idle ends.
        fired = true;
        assert_eq!(
            fallback_event(true, Duration::from_secs(9000), None, &mut fired),
            None
        );
        assert_eq!(
            fallback_event(false, Duration::ZERO, None, &mut fired),
            Some(Event::Activity)
        );
    }

    #[test]
    fn fallback_control() {
        let mut control = IdleFallback::default();
        assert!(!control.engaged());
        control.enabled = true;
        assert!(control.engaged());
        control.compositor = true;
        assert!(!control.engaged());
    }
}

//! The state machine — the only place where decisions are made.
//!
//! Reference: `docs/02-state-machine.md`. `Engine::handle` is pure: no I/O, no
//! clock, no sysfs. Timers are commands going out and events coming back, so
//! the documented transition table can be exercised without real time or a
//! real machine (ADR-2).

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use tracing::{debug, info, warn};

use crate::config::{Config, CriticalAction, Mode, ServerTrigger, SleepMethod, format_duration};
use crate::ipc::{
    IdleInfo, InhibitWhat, InhibitorInfo, MAX_INHIBIT_TTL, ModeInfo, ModesData, PowerInfo, Request,
    RequestId, Response, ServerInfo, SleepInfo, StagesInfo, StateEvent, StatusData,
};
use crate::power::PowerSnapshot;

// -------------------------------------------------------------------- state

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Active,
    Dimmed,
    ScreenOff,
    /// `Command::Suspend` sent, waiting for `PrepareForSleep(true)`.
    Suspending,
    /// Between `PrepareForSleep(true)` and `PrepareForSleep(false)`.
    Sleeping,
    /// The server cycle (`docs/10-long-sleep-rtc.md`).
    LongSleep(Phase),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Grace,
    Armed,
    Sleeping,
    Checking,
}

impl State {
    /// States in which the user is away but the machine is still running.
    fn is_awake(self) -> bool {
        matches!(self, State::Active | State::Dimmed | State::ScreenOff)
    }
}

impl fmt::Display for State {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            State::Active => f.write_str("Active"),
            State::Dimmed => f.write_str("Dimmed"),
            State::ScreenOff => f.write_str("ScreenOff"),
            State::Suspending => f.write_str("Suspending"),
            State::Sleeping => f.write_str("Sleeping"),
            State::LongSleep(phase) => write!(f, "LongSleep({phase})"),
        }
    }
}

impl fmt::Display for Phase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Phase::Grace => "grace",
            Phase::Armed => "armed",
            Phase::Sleeping => "sleeping",
            Phase::Checking => "checking",
        })
    }
}

/// Which mode is in effect and why (`docs/08-power-modes.md`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModeSelection {
    Auto(String),
    Manual(String),
}

impl ModeSelection {
    pub fn name(&self) -> &str {
        match self {
            ModeSelection::Auto(name) | ModeSelection::Manual(name) => name,
        }
    }

    pub fn source(&self) -> &'static str {
        match self {
            ModeSelection::Auto(_) => "auto",
            ModeSelection::Manual(_) => "manual",
        }
    }
}

/// The three idle stages, absolute from the start of idleness.
/// `None` means the stage is disabled and no notification is created.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Stages {
    pub dim: Option<Duration>,
    pub screen_off: Option<Duration>,
    pub sleep: Option<Duration>,
}

impl Stages {
    /// Idle has no effect at all — `ReplaceIdleStages(Stages::NONE)`.
    pub const NONE: Stages = Stages {
        dim: None,
        screen_off: None,
        sleep: None,
    };

    pub fn from_mode(mode: &Mode) -> Stages {
        let enabled = |d: Duration| (!d.is_zero()).then_some(d);
        Stages {
            dim: enabled(mode.dim_after),
            screen_off: enabled(mode.screen_off_after),
            sleep: enabled(mode.sleep_after),
        }
    }

    pub fn is_empty(&self) -> bool {
        *self == Stages::NONE
    }

    pub fn get(&self, stage: Stage) -> Option<Duration> {
        match stage {
            Stage::Dim => self.dim,
            Stage::ScreenOff => self.screen_off,
            Stage::Sleep => self.sleep,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Stage {
    Dim,
    ScreenOff,
    Sleep,
}

impl fmt::Display for Stage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Stage::Dim => "dim",
            Stage::ScreenOff => "screen_off",
            Stage::Sleep => "sleep",
        })
    }
}

/// A blocking inhibitor taken from `org.freedesktop.login1` (`docs/09-sleep-logind.md`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Inhibitor {
    pub what: String,
    pub who: String,
    pub why: String,
}

impl fmt::Display for Inhibitor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.who, self.why)
    }
}

/// An inhibitor created over IPC (`docs/11-ipc-cli.md`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Inhibit {
    pub id: u64,
    pub what: InhibitWhat,
    pub why: String,
    pub ttl: Duration,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TimerId {
    Grace,
    SleepRetry,
    AwakeWindow,
    InhibitExpiry(u64),
}

// ------------------------------------------------------------ events/commands

#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    Idle(Stage),
    /// `resumed` from the compositor.
    Activity,
    AcChanged(bool),
    Battery(u8),
    /// `PrepareForSleep(true)`.
    Suspending,
    /// `PrepareForSleep(false)`.
    Resumed,
    Timer(TimerId),
    Ipc(RequestId, Request),
    /// The compositor connected or was lost.
    IdleBackendChanged(bool),
    /// Whether logind and the kernel can hibernate (`docs/09-sleep-logind.md`).
    HibernateAvailable(bool),
    /// Refreshed view of logind's blocking inhibitors (ADR-11).
    Inhibitors(Vec<Inhibitor>),
    /// A suspend attempt was refused because of inhibitors (ADR-11).
    SleepBlocked(Vec<Inhibitor>),
    /// The RTC alarm asked for by `ScheduleWake` is armed — or could not be (ADR-13).
    WakeScheduled(bool),
    ReloadRequested,
    ShutdownRequested,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    Dim(u8),
    Undim,
    Screen(bool),
    ApplyMode(String),
    ReplaceIdleStages(Stages),
    Suspend {
        method: SleepMethod,
        force: bool,
    },
    PowerOff,
    /// Wake the machine this long from now; the FSM has no clock (ADR-13).
    ScheduleWake(Duration),
    CancelWake,
    RunHook(String),
    StartTimer(TimerId, Duration),
    CancelTimer(TimerId),
    Reply(RequestId, Response),
    Broadcast(StateEvent),
    Reload {
        reply_to: Option<RequestId>,
    },
    Shutdown,
}

// ------------------------------------------------------------------- engine

pub struct Engine {
    config: Arc<Config>,
    state: State,
    mode: ModeSelection,
    power: PowerSnapshot,
    /// Low battery, with the hysteresis from `docs/07-power-supply.md`.
    low: bool,
    inhibits: Vec<Inhibit>,
    next_inhibit_id: u64,
    /// Last known blocking inhibitors from logind.
    external: Vec<Inhibitor>,
    /// A sleep attempt was refused and a retry timer is armed.
    pending_sleep: bool,
    /// Where to go back to if the suspend we asked for does not happen.
    suspend_from: State,
    idle_connected: bool,
    /// Until told otherwise, assume the critical action can hibernate.
    hibernate_available: bool,
    /// Attempts of the server cycle that did not end in a suspend.
    sleep_failures: u8,
    /// The last cycle ended because the user woke the machine; shown in
    /// status until the next cycle starts (`docs/10-long-sleep-rtc.md`).
    interrupted: bool,
}

/// Give up on the server cycle after this many attempts that did not sleep.
pub const MAX_SLEEP_FAILURES: u8 = 3;

impl Engine {
    pub fn new(config: Arc<Config>, power: PowerSnapshot) -> Engine {
        let mut engine = Engine {
            mode: ModeSelection::Auto(String::new()),
            config,
            state: State::Active,
            power,
            low: false,
            inhibits: Vec::new(),
            next_inhibit_id: 1,
            external: Vec::new(),
            pending_sleep: false,
            suspend_from: State::Active,
            idle_connected: false,
            hibernate_available: true,
            sleep_failures: 0,
            interrupted: false,
        };
        engine.low = engine.compute_low(engine.power.battery);
        engine.mode = ModeSelection::Auto(engine.auto_mode_name());
        engine
    }

    /// Commands to run once at startup: put the machine into the current mode.
    pub fn start(&mut self) -> Vec<Command> {
        self.mode_commands()
    }

    /// The manual mode saved by a previous run (ADR-15), before `start`.
    /// A name the config no longer has is refused, and auto stays in charge.
    pub fn restore_manual_mode(&mut self, name: &str) -> bool {
        if !self.config.modes.contains_key(name) {
            warn!(
                mode = name,
                "saved manual mode is not in the config, ignoring it"
            );
            return false;
        }
        info!(mode = name, "manual mode restored");
        self.mode = ModeSelection::Manual(name.to_string());
        true
    }

    pub fn state(&self) -> State {
        self.state
    }

    pub fn mode(&self) -> &ModeSelection {
        &self.mode
    }

    pub fn stages(&self) -> Stages {
        match self.config.modes.get(self.mode.name()) {
            Some(mode) => Stages::from_mode(mode),
            None => Stages::NONE,
        }
    }

    /// A validated replacement config (SIGHUP, `amperedctl reload`).
    ///
    /// `[general] socket` and `[wayland]` are not re-read — see
    /// `docs/12-configuration.md`; `main` warns about that.
    pub fn set_config(&mut self, config: Arc<Config>) -> Vec<Command> {
        self.config = config;
        self.low = self.compute_low(self.power.battery);
        if let ModeSelection::Auto(_) = self.mode {
            self.mode = ModeSelection::Auto(self.auto_mode_name());
        }
        let mut commands = self.mode_commands();
        // Past Grace the stages are off for the cycle; a reload must not
        // switch them back on (`docs/10-long-sleep-rtc.md`).
        if matches!(
            self.state,
            State::LongSleep(Phase::Armed | Phase::Sleeping | Phase::Checking)
        ) {
            for command in &mut commands {
                if let Command::ReplaceIdleStages(stages) = command {
                    *stages = Stages::NONE;
                }
            }
        }
        commands
    }

    /// Put the current mode into effect. A config with no `[modes]` at all has
    /// nothing to apply, and `ApplyMode("")` would only make `main` log an
    /// error, so the command is left out rather than sent empty.
    fn mode_commands(&self) -> Vec<Command> {
        let name = self.mode.name().to_string();
        let mut commands = Vec::new();
        if !name.is_empty() {
            commands.push(Command::ApplyMode(name));
        }
        commands.push(Command::ReplaceIdleStages(self.stages()));
        commands
    }

    pub fn handle(&mut self, event: Event) -> Vec<Command> {
        let from = self.state;
        let mut commands = self.dispatch(event);
        if self.state != from {
            debug!(%from, to = %self.state, "state change");
            commands.push(Command::Broadcast(StateEvent::State {
                from: from.to_string(),
                to: self.state.to_string(),
            }));
            // `next_wake` is filled in by `main`, which armed the alarm.
            let phase = match (from, self.state) {
                (_, State::LongSleep(phase)) => Some(phase.to_string()),
                (State::LongSleep(_), _) => Some(self.server_phase()),
                _ => None,
            };
            if let Some(phase) = phase {
                commands.push(Command::Broadcast(StateEvent::LongSleep {
                    phase,
                    next_wake: None,
                }));
            }
        }
        commands
    }

    /// The daemon came back in the middle of a cycle (`state.json`, no AC):
    /// pick it up at `Checking`, as if the alarm had just fired.
    pub fn resume_cycle(&mut self) -> Vec<Command> {
        info!("resuming the long-sleep cycle from a previous run");
        self.state = State::LongSleep(Phase::Checking);
        vec![
            Command::ReplaceIdleStages(Stages::NONE),
            Command::StartTimer(TimerId::AwakeWindow, self.config.server.awake_window),
        ]
    }

    fn server_phase(&self) -> String {
        match self.state {
            State::LongSleep(phase) => phase.to_string(),
            _ if self.interrupted => "interrupted".to_string(),
            _ => "idle".to_string(),
        }
    }

    fn dispatch(&mut self, event: Event) -> Vec<Command> {
        match event {
            Event::Idle(stage) => self.on_idle(stage),
            Event::Activity => self.on_activity(),
            Event::AcChanged(ac) => self.on_ac_changed(ac),
            Event::Battery(percent) => self.on_battery(percent),
            Event::Suspending => self.on_suspending(),
            Event::Resumed => self.on_resumed(),
            Event::Timer(id) => self.on_timer(id),
            Event::Ipc(id, request) => self.on_ipc(id, request),
            Event::IdleBackendChanged(connected) => {
                self.idle_connected = connected;
                Vec::new()
            }
            Event::HibernateAvailable(available) => {
                self.hibernate_available = available;
                if !available && self.config.server.critical_action == CriticalAction::Hibernate {
                    warn!("hibernate is unavailable; the critical action degrades to poweroff");
                }
                Vec::new()
            }
            Event::Inhibitors(list) => {
                self.external = list;
                Vec::new()
            }
            Event::SleepBlocked(list) => self.on_sleep_blocked(list),
            Event::WakeScheduled(armed) => self.on_wake_scheduled(armed),
            Event::ReloadRequested => vec![Command::Reload { reply_to: None }],
            Event::ShutdownRequested => {
                // Never leave the user with a dark screen (`docs/02-state-machine.md`).
                self.state = State::Active;
                vec![Command::Undim, Command::Screen(true), Command::Shutdown]
            }
        }
    }

    // ------------------------------------------------------------ idle stages

    fn on_idle(&mut self, stage: Stage) -> Vec<Command> {
        if let Some(inhibit) = self.idle_inhibit() {
            debug!(stage = %stage, why = %inhibit.why, "idle stage ignored, inhibited");
            return Vec::new();
        }
        match stage {
            Stage::Dim if self.state == State::Active => {
                self.state = State::Dimmed;
                vec![Command::Dim(self.config.backlight.dim_percent)]
            }
            // Reached from Active directly when the dim stage is disabled.
            Stage::ScreenOff if matches!(self.state, State::Active | State::Dimmed) => {
                self.state = State::ScreenOff;
                vec![Command::Screen(false)]
            }
            // A compositor restart can deliver `sleep` before `dim`; that is
            // correct as-is (`docs/04-idle-wayland.md`).
            Stage::Sleep if self.state.is_awake() => self.request_sleep(false, true),
            _ => Vec::new(),
        }
    }

    fn on_activity(&mut self) -> Vec<Command> {
        let mut commands = self.cancel_pending_sleep();
        match self.state {
            State::Dimmed => {
                self.state = State::Active;
                commands.push(Command::Undim);
            }
            // `Suspending` here means logind refused or raced with us.
            State::ScreenOff | State::Suspending => {
                self.state = State::Active;
                commands.push(Command::Undim);
                commands.push(Command::Screen(true));
            }
            // The suspend we asked for did not happen.
            State::LongSleep(Phase::Armed) => commands.extend(self.cycle_failure()),
            // The user woke the machine, or opened the lid during the window.
            State::LongSleep(Phase::Checking) => {
                self.interrupted = true;
                info!("long sleep interrupted by the user");
                commands.push(Command::CancelTimer(TimerId::AwakeWindow));
                commands.extend(self.leave_cycle());
            }
            State::Active | State::Sleeping | State::LongSleep(_) => {}
        }
        commands
    }

    // ----------------------------------------------------------------- sleep

    /// `arm_retry` distinguishes an idle-driven attempt (retried later) from an
    /// explicit `amperedctl sleep` (reported back to the caller instead).
    fn request_sleep(&mut self, force: bool, arm_retry: bool) -> Vec<Command> {
        let blockers = self.sleep_blockers();
        if !force && !blockers.is_empty() {
            info!(blocked_by = ?blockers, "not sleeping");
            if !arm_retry {
                return Vec::new();
            }
            return self.arm_sleep_retry();
        }

        self.suspend_from = self.state;
        self.state = State::Suspending;
        self.pending_sleep = false;
        vec![Command::Suspend {
            method: self.config.sleep.method,
            force,
        }]
    }

    fn arm_sleep_retry(&mut self) -> Vec<Command> {
        let retry = self.config.sleep.sleep_retry;
        if retry.is_zero() {
            return Vec::new();
        }
        self.pending_sleep = true;
        vec![Command::StartTimer(TimerId::SleepRetry, retry)]
    }

    fn cancel_pending_sleep(&mut self) -> Vec<Command> {
        if std::mem::take(&mut self.pending_sleep) {
            vec![Command::CancelTimer(TimerId::SleepRetry)]
        } else {
            Vec::new()
        }
    }

    /// Inhibitors that stand between us and a suspend right now.
    fn sleep_blockers(&self) -> Vec<String> {
        if !self.config.sleep.respect_inhibitors {
            return Vec::new();
        }
        let internal = self
            .inhibits
            .iter()
            .filter(|i| i.what == InhibitWhat::Sleep)
            .map(|i| format!("ampered#{}: {}", i.id, i.why));
        self.external
            .iter()
            .map(|i| i.to_string())
            .chain(internal)
            .collect()
    }

    fn idle_inhibit(&self) -> Option<&Inhibit> {
        self.inhibits.iter().find(|i| i.what == InhibitWhat::Idle)
    }

    fn on_sleep_blocked(&mut self, blockers: Vec<Inhibitor>) -> Vec<Command> {
        self.external = blockers;
        match self.state {
            State::Suspending => {
                self.state = self.suspend_from;
                self.arm_sleep_retry()
            }
            State::LongSleep(Phase::Armed) => self.cycle_failure(),
            _ => Vec::new(),
        }
    }

    fn on_suspending(&mut self) -> Vec<Command> {
        // Also reached when logind suspends for a reason of its own (lid, a
        // `systemctl suspend` from elsewhere) — the machine is going down
        // either way.
        self.pending_sleep = false;
        self.state = match self.state {
            State::LongSleep(_) => State::LongSleep(Phase::Sleeping),
            _ => State::Sleeping,
        };
        Vec::new()
    }

    fn on_resumed(&mut self) -> Vec<Command> {
        self.pending_sleep = false;
        if let State::LongSleep(_) = self.state {
            return self.check_power();
        }
        self.state = State::Active;
        // DPMS state after S3 is non-deterministic, and the compositor may
        // have dropped our notifications (`docs/06-display-dpms.md`).
        vec![
            Command::Undim,
            Command::Screen(true),
            Command::ReplaceIdleStages(self.stages()),
        ]
    }

    fn on_timer(&mut self, id: TimerId) -> Vec<Command> {
        match id {
            TimerId::SleepRetry if self.pending_sleep => {
                self.pending_sleep = false;
                self.request_sleep(false, true)
            }
            TimerId::InhibitExpiry(inhibit) => {
                if let Some(pos) = self.inhibits.iter().position(|i| i.id == inhibit) {
                    let expired = self.inhibits.remove(pos);
                    info!(id = expired.id, why = %expired.why, "inhibitor expired");
                }
                Vec::new()
            }
            TimerId::Grace if self.state == State::LongSleep(Phase::Grace) => self.arm(true),
            TimerId::AwakeWindow => match self.state {
                // A retry after an attempt that did not sleep — unless the
                // power came back in the meantime.
                State::LongSleep(Phase::Armed) if self.power.ac => self.power_is_back(),
                State::LongSleep(Phase::Armed) => self.arm(false),
                State::LongSleep(Phase::Checking) => {
                    if self.power.ac || self.battery_critical() {
                        self.check_power()
                    } else {
                        self.arm(false)
                    }
                }
                _ => Vec::new(),
            },
            _ => Vec::new(),
        }
    }

    // ---------------------------------------------------------- server cycle

    fn enter_grace(&mut self, delay: Duration) -> Vec<Command> {
        info!(grace = %format_duration(delay), "entering the long-sleep cycle");
        self.state = State::LongSleep(Phase::Grace);
        self.sleep_failures = 0;
        self.interrupted = false;
        vec![Command::StartTimer(TimerId::Grace, delay)]
    }

    /// Schedule the wake; the suspend follows `WakeScheduled(true)`.
    fn arm(&mut self, first: bool) -> Vec<Command> {
        self.state = State::LongSleep(Phase::Armed);
        let mut commands = Vec::new();
        if first {
            commands.push(Command::ReplaceIdleStages(Stages::NONE));
        }
        commands.push(Command::ScheduleWake(self.config.server.check_interval));
        commands
    }

    fn on_wake_scheduled(&mut self, armed: bool) -> Vec<Command> {
        if self.state != State::LongSleep(Phase::Armed) {
            return Vec::new();
        }
        if !armed {
            warn!("no wake alarm; not sleeping in the cycle without one");
            return self.cycle_failure();
        }
        // Power returned while the alarm was being armed: no point sleeping.
        if self.power.ac {
            return self.power_is_back();
        }
        let blockers = self.sleep_blockers();
        if !blockers.is_empty() {
            info!(blocked_by = ?blockers, "long sleep blocked");
            return self.cycle_failure();
        }
        vec![Command::Suspend {
            method: self.config.sleep.method,
            force: false,
        }]
    }

    /// An attempt that did not end in a suspend: wait out the window and
    /// try again, up to `MAX_SLEEP_FAILURES`.
    fn cycle_failure(&mut self) -> Vec<Command> {
        self.sleep_failures += 1;
        if self.sleep_failures >= MAX_SLEEP_FAILURES {
            warn!(
                attempts = self.sleep_failures,
                "long sleep keeps failing, giving up on the cycle"
            );
            return self.leave_cycle();
        }
        warn!(
            attempt = self.sleep_failures,
            "long sleep did not happen, retrying after the awake window"
        );
        vec![Command::StartTimer(
            TimerId::AwakeWindow,
            self.config.server.awake_window,
        )]
    }

    /// After a wake in the cycle, and again when the window closes: is the
    /// power back, is the battery critical, or do we go on?
    fn check_power(&mut self) -> Vec<Command> {
        if self.power.ac {
            return self.power_is_back();
        }
        self.state = State::LongSleep(Phase::Checking);
        let mut commands = Vec::new();
        if self.battery_critical() {
            warn!(
                battery = ?self.power.battery,
                action = ?self.config.server.critical_action,
                "battery critical"
            );
            commands.push(Command::Broadcast(StateEvent::LongSleep {
                phase: "critical".to_string(),
                next_wake: None,
            }));
            commands.push(match self.config.server.critical_action {
                CriticalAction::Hibernate if self.hibernate_available => Command::Suspend {
                    method: SleepMethod::Hibernate,
                    force: true,
                },
                // A plain suspend would keep draining the battery.
                CriticalAction::Hibernate | CriticalAction::Poweroff => Command::PowerOff,
            });
        }
        // The window also covers a critical action that does not happen:
        // the next check tries again instead of leaving a dead state.
        commands.push(Command::StartTimer(
            TimerId::AwakeWindow,
            self.config.server.awake_window,
        ));
        commands
    }

    fn battery_critical(&self) -> bool {
        self.power
            .battery
            .is_some_and(|percent| percent <= self.config.server.battery_critical_percent)
    }

    /// The cycle ends because AC is back: the hook, then the mode and the
    /// stages that `Armed` took away.
    fn power_is_back(&mut self) -> Vec<Command> {
        info!("power is back, leaving the long-sleep cycle");
        let mut commands = Vec::new();
        if let Some(hook) = &self.config.server.resume_hook {
            commands.push(Command::RunHook(hook.clone()));
        }
        commands.extend(self.leave_cycle());
        commands
    }

    /// Back to `Active` from any phase, with the alarm and timers gone.
    fn leave_cycle(&mut self) -> Vec<Command> {
        self.state = State::Active;
        let changed = self.select_auto_mode();
        let mut commands = vec![Command::CancelWake];
        commands.extend(self.mode_commands());
        if changed {
            commands.push(Command::Broadcast(StateEvent::Mode {
                name: self.mode.name().to_string(),
            }));
        }
        commands.push(Command::Undim);
        commands.push(Command::Screen(true));
        commands
    }

    fn cancel_cycle(&mut self, phase: Phase) -> Vec<Command> {
        info!(%phase, "long-sleep cycle cancelled");
        let mut commands = match phase {
            Phase::Grace => vec![Command::CancelTimer(TimerId::Grace)],
            Phase::Armed | Phase::Checking => vec![Command::CancelTimer(TimerId::AwakeWindow)],
            Phase::Sleeping => Vec::new(),
        };
        self.interrupted = false;
        commands.extend(self.leave_cycle());
        commands
    }

    // ----------------------------------------------------------------- power

    fn on_ac_changed(&mut self, ac: bool) -> Vec<Command> {
        if self.power.ac == ac {
            return Vec::new();
        }
        self.power.ac = ac;
        info!(ac, "power source changed");
        let mut commands = vec![Command::Broadcast(StateEvent::Power { ac })];
        match self.state {
            State::LongSleep(Phase::Grace) if ac => {
                self.state = State::Active;
                commands.push(Command::CancelTimer(TimerId::Grace));
                commands.extend(self.reapply_auto_mode());
            }
            State::LongSleep(Phase::Checking) if ac => {
                commands.push(Command::CancelTimer(TimerId::AwakeWindow));
                commands.extend(self.power_is_back());
            }
            _ => {
                commands.extend(self.reapply_auto_mode());
                let server = &self.config.server;
                if !ac
                    && self.state.is_awake()
                    && server.enabled
                    && server.trigger == ServerTrigger::AcLost
                {
                    commands.extend(self.enter_grace(server.grace_period));
                }
            }
        }
        commands
    }

    fn on_battery(&mut self, percent: u8) -> Vec<Command> {
        self.power.battery = Some(percent);
        let low = self.compute_low(Some(percent));
        if low == self.low {
            return Vec::new();
        }
        self.low = low;
        info!(percent, low, "low battery threshold crossed");
        self.reapply_auto_mode()
    }

    /// `low` at `≤ threshold`, back to normal only at `≥ threshold + 5`.
    fn compute_low(&self, battery: Option<u8>) -> bool {
        let Some(percent) = battery else {
            return false;
        };
        let threshold = self.config.auto_mode.low_battery_percent;
        // Capped at 100: with a threshold above 95 the band would otherwise
        // never clear, and `low` would be a one-way trip.
        let clears_at = threshold.saturating_add(5).min(100);
        if percent <= threshold {
            true
        } else if percent >= clears_at {
            false
        } else {
            self.low
        }
    }

    /// The mode auto-switching wants right now.
    ///
    /// With `enabled = false` there is nothing to compute and whatever is in
    /// effect stays in effect — except at startup, where nothing is in effect
    /// yet. Falling back to the first declared mode there keeps the daemon from
    /// coming up with an empty mode name and, with it, no idle stages at all
    /// (`docs/08-power-modes.md`).
    fn auto_mode_name(&self) -> String {
        let auto = &self.config.auto_mode;
        if !auto.enabled {
            let current = self.mode.name();
            if !current.is_empty() {
                return current.to_string();
            }
            return self.config.modes.keys().next().cloned().unwrap_or_default();
        }
        if self.power.ac {
            auto.on_ac.clone()
        } else if self.low {
            auto.on_low_battery.clone()
        } else {
            auto.on_battery.clone()
        }
    }

    /// Re-evaluates an auto mode; `true` when a different one was picked.
    fn select_auto_mode(&mut self) -> bool {
        if !matches!(self.mode, ModeSelection::Auto(_)) {
            return false;
        }
        let wanted = self.auto_mode_name();
        if wanted == self.mode.name() {
            return false;
        }
        self.mode = ModeSelection::Auto(wanted);
        true
    }

    fn reapply_auto_mode(&mut self) -> Vec<Command> {
        if self.select_auto_mode() {
            self.mode_changed()
        } else {
            Vec::new()
        }
    }

    fn mode_changed(&self) -> Vec<Command> {
        let name = self.mode.name().to_string();
        info!(mode = %name, source = self.mode.source(), "mode applied");
        let mut commands = self.mode_commands();
        commands.push(Command::Broadcast(StateEvent::Mode { name }));
        commands
    }

    // ------------------------------------------------------------------- ipc

    fn on_ipc(&mut self, id: RequestId, request: Request) -> Vec<Command> {
        match request {
            Request::Status => vec![Command::Reply(
                id,
                Response::Status(Box::new(self.status())),
            )],
            Request::Modes => {
                let data = ModesData {
                    modes: self.config.modes.keys().cloned().collect(),
                    current: self.mode.name().to_string(),
                    source: self.mode.source().to_string(),
                };
                vec![Command::Reply(id, Response::Modes(data))]
            }
            Request::Mode { name } => self.on_set_mode(id, &name),
            Request::Dim => {
                let mut commands = if self.state == State::Active {
                    self.state = State::Dimmed;
                    vec![Command::Dim(self.config.backlight.dim_percent)]
                } else {
                    Vec::new()
                };
                commands.push(Command::Reply(id, Response::Ok));
                commands
            }
            Request::Undim => {
                let mut commands = self.on_activity();
                commands.push(Command::Reply(id, Response::Ok));
                commands
            }
            Request::Screen { state } => {
                let mut commands = if state.is_on() {
                    self.on_activity()
                } else if matches!(self.state, State::Active | State::Dimmed) {
                    self.state = State::ScreenOff;
                    vec![Command::Screen(false)]
                } else {
                    Vec::new()
                };
                commands.push(Command::Reply(id, Response::Ok));
                commands
            }
            Request::Sleep { force } => {
                let blockers = self.sleep_blockers();
                if !force && !blockers.is_empty() {
                    return vec![Command::Reply(
                        id,
                        Response::error(format!("blocked by inhibitors: {}", blockers.join(", "))),
                    )];
                }
                let mut commands = self.request_sleep(force, false);
                commands.push(Command::Reply(id, Response::Ok));
                commands
            }
            Request::LongSleep { cancel } => self.on_long_sleep(id, cancel),
            Request::Inhibit { what, why, ttl } => self.on_inhibit(id, what, why, ttl),
            Request::Uninhibit { id: inhibit } => self.on_uninhibit(id, inhibit),
            Request::Reload => vec![Command::Reload { reply_to: Some(id) }],
            // `subscribe` never reaches the engine; the IPC server keeps those
            // connections attached to the broadcast channel itself.
            Request::Subscribe => vec![Command::Reply(
                id,
                Response::error("subscribe is handled by the IPC server"),
            )],
        }
    }

    fn on_long_sleep(&mut self, id: RequestId, cancel: bool) -> Vec<Command> {
        let reply = |commands: &mut Vec<Command>| commands.push(Command::Reply(id, Response::Ok));
        if cancel {
            let State::LongSleep(phase) = self.state else {
                return vec![Command::Reply(
                    id,
                    Response::error("not in the long-sleep cycle"),
                )];
            };
            let mut commands = self.cancel_cycle(phase);
            reply(&mut commands);
            return commands;
        }
        if !self.config.server.enabled {
            return vec![Command::Reply(
                id,
                Response::error("[server] is not enabled"),
            )];
        }
        if let State::LongSleep(phase) = self.state {
            return vec![Command::Reply(
                id,
                Response::error(format!("already in the long-sleep cycle ({phase})")),
            )];
        }
        if !self.state.is_awake() {
            return vec![Command::Reply(
                id,
                Response::error("cannot start the cycle while going to sleep"),
            )];
        }
        let mut commands = self.enter_grace(Duration::ZERO);
        reply(&mut commands);
        commands
    }

    fn on_set_mode(&mut self, id: RequestId, name: &str) -> Vec<Command> {
        if name == "auto" {
            self.mode = ModeSelection::Auto(self.auto_mode_name());
            let mut commands = self.mode_changed();
            commands.push(Command::Reply(id, Response::Ok));
            return commands;
        }
        if !self.config.modes.contains_key(name) {
            return vec![Command::Reply(
                id,
                Response::error(format!("no such mode: {name}")),
            )];
        }
        self.mode = ModeSelection::Manual(name.to_string());
        let mut commands = self.mode_changed();
        commands.push(Command::Reply(id, Response::Ok));
        commands
    }

    fn on_inhibit(
        &mut self,
        request: RequestId,
        what: InhibitWhat,
        why: String,
        ttl: Duration,
    ) -> Vec<Command> {
        let ttl = ttl.min(MAX_INHIBIT_TTL);
        if ttl.is_zero() {
            return vec![Command::Reply(
                request,
                Response::error("ttl must be greater than zero"),
            )];
        }
        let id = self.next_inhibit_id;
        self.next_inhibit_id += 1;
        info!(id, %what, %why, ttl = %format_duration(ttl), "inhibitor added");
        self.inhibits.push(Inhibit { id, what, why, ttl });
        vec![
            Command::StartTimer(TimerId::InhibitExpiry(id), ttl),
            Command::Reply(request, Response::Inhibited { id }),
        ]
    }

    fn on_uninhibit(&mut self, request: RequestId, inhibit: u64) -> Vec<Command> {
        match self.inhibits.iter().position(|i| i.id == inhibit) {
            Some(pos) => {
                self.inhibits.remove(pos);
                vec![
                    Command::CancelTimer(TimerId::InhibitExpiry(inhibit)),
                    Command::Reply(request, Response::Ok),
                ]
            }
            None => vec![Command::Reply(
                request,
                Response::error(format!("no such inhibitor: {inhibit}")),
            )],
        }
    }

    /// Everything the engine knows; `main` merges in the actor-owned parts
    /// (backlight, idle backend name, `degraded`, inhibitor expiry times).
    pub fn status(&self) -> StatusData {
        let stages = self.stages();
        let stage_string = |d: Option<Duration>| format_duration(d.unwrap_or(Duration::ZERO));
        StatusData {
            state: self.state.to_string(),
            mode: ModeInfo {
                name: self.mode.name().to_string(),
                source: self.mode.source().to_string(),
            },
            power: PowerInfo {
                ac: self.power.ac,
                battery_percent: self.power.battery,
                low: self.low,
                batteries: self.power.batteries.clone(),
            },
            backlight: Default::default(),
            idle: IdleInfo {
                backend: "ext-idle-notify".to_string(),
                connected: self.idle_connected,
                stages: StagesInfo {
                    dim: stage_string(stages.dim),
                    screen_off: stage_string(stages.screen_off),
                    sleep: stage_string(stages.sleep),
                },
            },
            sleep: SleepInfo {
                method: self.config.sleep.method.to_string(),
                blocked_by: self.sleep_blockers(),
            },
            server: ServerInfo {
                enabled: self.config.server.enabled,
                phase: self.server_phase(),
                next_wake: None,
            },
            inhibitors: self
                .inhibits
                .iter()
                .map(|i| InhibitorInfo {
                    id: i.id,
                    what: i.what.to_string(),
                    why: i.why.clone(),
                    expires: None,
                })
                .collect(),
            degraded: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    //! The transition table from `docs/02-state-machine.md` and the scenarios
    //! from `docs/15-testing.md`. Nothing here touches the machine: a
    //! `Command::Suspend` is a value to compare, not a suspend.

    use super::*;
    use crate::ipc::ScreenState;

    const CONFIG: &str = r#"
        [backlight]
        dim_percent = 10

        [modes.ac]
        dim_after = "5m"
        screen_off_after = "10m"
        sleep_after = "30m"

        [modes.bat]
        dim_after = "2m"
        screen_off_after = "4m"
        sleep_after = "10m"

        [modes.low]
        dim_after = "1m"
        screen_off_after = "2m"
        sleep_after = "5m"

        [modes.nodim]
        dim_after = "0"
        screen_off_after = "4m"
        sleep_after = "10m"

        [auto_mode]
        enabled = true
        on_ac = "ac"
        on_battery = "bat"
        low_battery_percent = 20
        on_low_battery = "low"

        [sleep]
        method = "suspend"
        respect_inhibitors = true
        sleep_retry = "2m"
    "#;

    const SUSPEND: Command = Command::Suspend {
        method: SleepMethod::Suspend,
        force: false,
    };
    const SLEEP_RETRY: Duration = Duration::from_secs(120);

    fn config_from(text: &str) -> Arc<Config> {
        let config = Config::parse(text).expect("parse");
        config.validate().expect("valid");
        Arc::new(config)
    }

    fn config() -> Arc<Config> {
        config_from(CONFIG)
    }

    fn stages(name: &str) -> Stages {
        Stages::from_mode(&config().modes[name])
    }

    fn on_battery(percent: u8) -> PowerSnapshot {
        PowerSnapshot {
            ac: false,
            battery: Some(percent),
            batteries: vec!["BAT0".into()],
        }
    }

    fn fresh() -> Engine {
        Engine::new(config(), PowerSnapshot::on_ac())
    }

    /// An engine driven into `state` along the documented idle path.
    fn engine_in(state: State) -> Engine {
        let mut engine = fresh();
        let path: &[Event] = match state {
            State::Active => &[],
            State::Dimmed => &[Event::Idle(Stage::Dim)],
            State::ScreenOff => &[Event::Idle(Stage::Dim), Event::Idle(Stage::ScreenOff)],
            State::Suspending => &[
                Event::Idle(Stage::Dim),
                Event::Idle(Stage::ScreenOff),
                Event::Idle(Stage::Sleep),
            ],
            State::Sleeping => &[
                Event::Idle(Stage::Dim),
                Event::Idle(Stage::ScreenOff),
                Event::Idle(Stage::Sleep),
                Event::Suspending,
            ],
            State::LongSleep(_) => unreachable!("use server_in for the cycle"),
        };
        for event in path {
            engine.handle(event.clone());
        }
        assert_eq!(engine.state(), state);
        engine
    }

    /// Everything except the `Broadcast`s, which mirror a state or mode change
    /// and are checked once on their own in `broadcasts_mirror_the_changes`.
    fn effects(commands: Vec<Command>) -> Vec<Command> {
        commands
            .into_iter()
            .filter(|c| !matches!(c, Command::Broadcast(_)))
            .collect()
    }

    fn inhibitor(who: &str) -> Inhibitor {
        Inhibitor {
            what: "sleep".into(),
            who: who.into(),
            why: "busy".into(),
        }
    }

    fn ipc(request: Request) -> Event {
        Event::Ipc(7, request)
    }

    fn set_mode(engine: &mut Engine, name: &str) {
        let commands = engine.handle(ipc(Request::Mode { name: name.into() }));
        assert!(
            commands.contains(&Command::Reply(7, Response::Ok)),
            "{commands:?}"
        );
    }

    // ------------------------------------------------------------- the table

    /// One row per line of the transition table: from, event, to, commands.
    /// Rows that need a condition (inhibitors, a disabled stage, a manual
    /// mode) live in the scenario tests below.
    fn transitions() -> Vec<(State, Event, State, Vec<Command>)> {
        use Command::*;
        use State::*;
        vec![
            // The main idle chain.
            (Active, Event::Idle(Stage::Dim), Dimmed, vec![Dim(10)]),
            (
                Dimmed,
                Event::Idle(Stage::ScreenOff),
                ScreenOff,
                vec![Screen(false)],
            ),
            (
                ScreenOff,
                Event::Idle(Stage::Sleep),
                Suspending,
                vec![SUSPEND],
            ),
            (Suspending, Event::Suspending, Sleeping, vec![]),
            (
                Sleeping,
                Event::Resumed,
                Active,
                vec![Undim, Screen(true), ReplaceIdleStages(stages("ac"))],
            ),
            // Stages before `sleep` disabled, or a compositor restart that
            // delivers the later stage first.
            (
                Active,
                Event::Idle(Stage::ScreenOff),
                ScreenOff,
                vec![Screen(false)],
            ),
            (Active, Event::Idle(Stage::Sleep), Suspending, vec![SUSPEND]),
            (Dimmed, Event::Idle(Stage::Sleep), Suspending, vec![SUSPEND]),
            // Activity from every intermediate state.
            (Active, Event::Activity, Active, vec![]),
            (Dimmed, Event::Activity, Active, vec![Undim]),
            (
                ScreenOff,
                Event::Activity,
                Active,
                vec![Undim, Screen(true)],
            ),
            (
                Suspending,
                Event::Activity,
                Active,
                vec![Undim, Screen(true)],
            ),
            (Sleeping, Event::Activity, Sleeping, vec![]),
            // Idempotency: no second Dim, no stage while the machine is going down.
            (Dimmed, Event::Idle(Stage::Dim), Dimmed, vec![]),
            (ScreenOff, Event::Idle(Stage::Dim), ScreenOff, vec![]),
            (ScreenOff, Event::Idle(Stage::ScreenOff), ScreenOff, vec![]),
            (Suspending, Event::Idle(Stage::Sleep), Suspending, vec![]),
            (Sleeping, Event::Idle(Stage::Dim), Sleeping, vec![]),
            // logind suspending for a reason of its own (lid, `systemctl suspend`).
            (Active, Event::Suspending, Sleeping, vec![]),
            (ScreenOff, Event::Suspending, Sleeping, vec![]),
            // Reload never touches the state.
            (
                Active,
                Event::ReloadRequested,
                Active,
                vec![Reload { reply_to: None }],
            ),
            (
                ScreenOff,
                Event::ReloadRequested,
                ScreenOff,
                vec![Reload { reply_to: None }],
            ),
            // Shutdown restores the screen from wherever we are.
            (
                Active,
                Event::ShutdownRequested,
                Active,
                vec![Undim, Screen(true), Shutdown],
            ),
            (
                Dimmed,
                Event::ShutdownRequested,
                Active,
                vec![Undim, Screen(true), Shutdown],
            ),
            (
                ScreenOff,
                Event::ShutdownRequested,
                Active,
                vec![Undim, Screen(true), Shutdown],
            ),
            // Bookkeeping events produce nothing on their own.
            (Active, Event::IdleBackendChanged(false), Active, vec![]),
            (Active, Event::HibernateAvailable(false), Active, vec![]),
            (
                Active,
                Event::Inhibitors(vec![inhibitor("x")]),
                Active,
                vec![],
            ),
            (
                Active,
                Event::SleepBlocked(vec![inhibitor("x")]),
                Active,
                vec![],
            ),
            (Active, Event::Timer(TimerId::SleepRetry), Active, vec![]),
            (Active, Event::Timer(TimerId::Grace), Active, vec![]),
            (Active, Event::Timer(TimerId::AwakeWindow), Active, vec![]),
            (
                Active,
                Event::Timer(TimerId::InhibitExpiry(99)),
                Active,
                vec![],
            ),
            (Active, Event::AcChanged(true), Active, vec![]),
        ]
    }

    #[test]
    fn transition_table() {
        for (from, event, to, expected) in transitions() {
            let mut engine = engine_in(from);
            let commands = effects(engine.handle(event.clone()));
            assert_eq!(commands, expected, "{from} + {event:?}");
            assert_eq!(engine.state(), to, "{from} + {event:?}");
        }
    }

    // ------------------------------------------------------------- scenarios

    /// Scenario 1: the whole chain, with the broadcasts this time.
    #[test]
    fn broadcasts_mirror_the_changes() {
        let mut engine = fresh();
        let state = |from: &str, to: &str| {
            Command::Broadcast(StateEvent::State {
                from: from.into(),
                to: to.into(),
            })
        };

        assert_eq!(
            engine.handle(Event::Idle(Stage::Dim)),
            vec![Command::Dim(10), state("Active", "Dimmed")]
        );
        assert_eq!(
            engine.handle(Event::Idle(Stage::ScreenOff)),
            vec![Command::Screen(false), state("Dimmed", "ScreenOff")]
        );
        assert_eq!(
            engine.handle(Event::Idle(Stage::Sleep)),
            vec![SUSPEND, state("ScreenOff", "Suspending")]
        );
        assert_eq!(
            engine.handle(Event::Suspending),
            vec![state("Suspending", "Sleeping")]
        );
        assert_eq!(
            engine.handle(Event::Resumed),
            vec![
                Command::Undim,
                Command::Screen(true),
                Command::ReplaceIdleStages(stages("ac")),
                state("Sleeping", "Active"),
            ]
        );
        // No change, no broadcast.
        assert_eq!(engine.handle(Event::Activity), vec![]);
    }

    #[test]
    fn restore_manual_mode_before_start() {
        let mut engine = fresh();
        assert!(engine.restore_manual_mode("bat"));
        assert_eq!(engine.mode(), &ModeSelection::Manual("bat".into()));
        assert_eq!(engine.start()[0], Command::ApplyMode("bat".into()));
        // Manual: the power source no longer matters.
        assert_eq!(
            engine.handle(Event::AcChanged(false)),
            vec![Command::Broadcast(StateEvent::Power { ac: false })]
        );

        let mut engine = fresh();
        assert!(!engine.restore_manual_mode("turbo"));
        assert_eq!(engine.mode(), &ModeSelection::Auto("ac".into()));
    }

    #[test]
    fn start_applies_the_auto_mode() {
        let mut engine = fresh();
        assert_eq!(engine.mode(), &ModeSelection::Auto("ac".into()));
        assert_eq!(
            engine.start(),
            vec![
                Command::ApplyMode("ac".into()),
                Command::ReplaceIdleStages(stages("ac")),
            ]
        );

        let mut engine = Engine::new(config(), on_battery(10));
        assert_eq!(engine.mode(), &ModeSelection::Auto("low".into()));
        assert!(engine.status().power.low);
        assert_eq!(engine.start()[0], Command::ApplyMode("low".into()));
    }

    /// Scenario 3: an inhibitor defers the idle sleep until it is gone.
    #[test]
    fn external_inhibitor_arms_a_retry() {
        let mut engine = engine_in(State::ScreenOff);
        engine.handle(Event::Inhibitors(vec![inhibitor("firefox")]));

        assert_eq!(
            engine.handle(Event::Idle(Stage::Sleep)),
            vec![Command::StartTimer(TimerId::SleepRetry, SLEEP_RETRY)]
        );
        assert_eq!(engine.state(), State::ScreenOff);
        assert_eq!(engine.status().sleep.blocked_by, vec!["firefox: busy"]);

        // Still blocked: the timer is re-armed, nothing else.
        assert_eq!(
            engine.handle(Event::Timer(TimerId::SleepRetry)),
            vec![Command::StartTimer(TimerId::SleepRetry, SLEEP_RETRY)]
        );

        engine.handle(Event::Inhibitors(vec![]));
        assert_eq!(
            effects(engine.handle(Event::Timer(TimerId::SleepRetry))),
            vec![SUSPEND]
        );
        assert_eq!(engine.state(), State::Suspending);
    }

    #[test]
    fn activity_cancels_the_pending_retry() {
        let mut engine = engine_in(State::ScreenOff);
        engine.handle(Event::Inhibitors(vec![inhibitor("firefox")]));
        engine.handle(Event::Idle(Stage::Sleep));

        assert_eq!(
            effects(engine.handle(Event::Activity)),
            vec![
                Command::CancelTimer(TimerId::SleepRetry),
                Command::Undim,
                Command::Screen(true),
            ]
        );
        // The timer may still fire from the channel; it must be ignored now.
        assert_eq!(engine.handle(Event::Timer(TimerId::SleepRetry)), vec![]);
    }

    /// ADR-11: logind refused the suspend after we asked for it.
    #[test]
    fn sleep_blocked_goes_back_and_retries() {
        let mut engine = engine_in(State::Suspending);
        assert_eq!(
            effects(engine.handle(Event::SleepBlocked(vec![inhibitor("firefox")]))),
            vec![Command::StartTimer(TimerId::SleepRetry, SLEEP_RETRY)]
        );
        assert_eq!(engine.state(), State::ScreenOff);
        assert_eq!(engine.status().sleep.blocked_by, vec!["firefox: busy"]);

        // A suspend from Active goes back to Active.
        let mut engine = fresh();
        engine.handle(Event::Idle(Stage::Sleep));
        engine.handle(Event::SleepBlocked(vec![inhibitor("firefox")]));
        assert_eq!(engine.state(), State::Active);
    }

    #[test]
    fn sleep_retry_zero_means_no_retry() {
        let text = CONFIG.replace("sleep_retry = \"2m\"", "sleep_retry = \"0\"");
        let mut engine = Engine::new(config_from(&text), PowerSnapshot::on_ac());
        engine.handle(Event::Inhibitors(vec![inhibitor("firefox")]));
        engine.handle(Event::Idle(Stage::ScreenOff));
        assert_eq!(engine.handle(Event::Idle(Stage::Sleep)), vec![]);
        assert_eq!(engine.state(), State::ScreenOff);
    }

    #[test]
    fn respect_inhibitors_false_ignores_them() {
        let text = CONFIG.replace("respect_inhibitors = true", "respect_inhibitors = false");
        let mut engine = Engine::new(config_from(&text), PowerSnapshot::on_ac());
        engine.handle(Event::Inhibitors(vec![inhibitor("firefox")]));
        engine.handle(Event::Idle(Stage::ScreenOff));
        assert_eq!(
            effects(engine.handle(Event::Idle(Stage::Sleep))),
            vec![SUSPEND]
        );
        assert!(engine.status().sleep.blocked_by.is_empty());
    }

    /// Scenario 4: auto-switching follows the power source, manual does not.
    #[test]
    fn ac_change_reapplies_the_auto_mode() {
        let mut engine = fresh();
        assert_eq!(
            engine.handle(Event::AcChanged(false)),
            vec![
                Command::Broadcast(StateEvent::Power { ac: false }),
                Command::ApplyMode("bat".into()),
                Command::ReplaceIdleStages(stages("bat")),
                Command::Broadcast(StateEvent::Mode { name: "bat".into() }),
            ]
        );
        assert_eq!(engine.mode(), &ModeSelection::Auto("bat".into()));
        // The same value again is not a change.
        assert_eq!(engine.handle(Event::AcChanged(false)), vec![]);

        // Manual: the power event is still broadcast, the mode stays.
        set_mode(&mut engine, "ac");
        assert_eq!(engine.mode(), &ModeSelection::Manual("ac".into()));
        assert_eq!(
            engine.handle(Event::AcChanged(true)),
            vec![Command::Broadcast(StateEvent::Power { ac: true })]
        );
        assert_eq!(
            engine.handle(Event::AcChanged(false)),
            vec![Command::Broadcast(StateEvent::Power { ac: false })]
        );
        assert_eq!(engine.mode(), &ModeSelection::Manual("ac".into()));

        // Back to auto: recomputed from the current power source.
        assert_eq!(
            effects(engine.handle(ipc(Request::Mode {
                name: "auto".into()
            }))),
            vec![
                Command::ApplyMode("bat".into()),
                Command::ReplaceIdleStages(stages("bat")),
                Command::Reply(7, Response::Ok),
            ]
        );
        assert_eq!(engine.mode(), &ModeSelection::Auto("bat".into()));
    }

    #[test]
    fn mode_change_is_a_no_op_when_nothing_changes() {
        let mut engine = fresh();
        // AC and on_ac already in effect: a "change" to the same value.
        assert_eq!(engine.handle(Event::AcChanged(true)), vec![]);

        let text = CONFIG.replace("on_battery = \"bat\"", "on_battery = \"ac\"");
        let mut engine = Engine::new(config_from(&text), PowerSnapshot::on_ac());
        // The wanted mode is the same one: only the power broadcast goes out.
        assert_eq!(
            engine.handle(Event::AcChanged(false)),
            vec![Command::Broadcast(StateEvent::Power { ac: false })]
        );
    }

    /// Scenario 5: `low` at ≤ 20, clears only at ≥ 25.
    #[test]
    fn low_battery_hysteresis() {
        let mut engine = Engine::new(config(), on_battery(50));
        assert_eq!(engine.mode(), &ModeSelection::Auto("bat".into()));

        assert_eq!(engine.handle(Event::Battery(21)), vec![]);
        assert!(!engine.status().power.low);

        assert_eq!(
            effects(engine.handle(Event::Battery(20))),
            vec![
                Command::ApplyMode("low".into()),
                Command::ReplaceIdleStages(stages("low")),
            ]
        );
        assert!(engine.status().power.low);

        assert_eq!(engine.handle(Event::Battery(23)), vec![]);
        assert!(engine.status().power.low);
        assert_eq!(engine.handle(Event::Battery(24)), vec![]);
        assert!(engine.status().power.low);

        assert_eq!(
            effects(engine.handle(Event::Battery(25))),
            vec![
                Command::ApplyMode("bat".into()),
                Command::ReplaceIdleStages(stages("bat")),
            ]
        );
        assert!(!engine.status().power.low);
        assert_eq!(engine.status().power.battery_percent, Some(25));
    }

    #[test]
    fn low_battery_does_not_matter_on_ac() {
        let mut engine = fresh();
        assert_eq!(engine.handle(Event::Battery(5)), vec![]);
        assert!(engine.status().power.low);
        assert_eq!(engine.mode(), &ModeSelection::Auto("ac".into()));

        // Unplugging with a low battery goes straight to on_low_battery.
        let commands = engine.handle(Event::AcChanged(false));
        assert!(
            commands.contains(&Command::ApplyMode("low".into())),
            "{commands:?}"
        );
    }

    #[test]
    fn low_band_is_capped_at_100() {
        let text = CONFIG.replace("low_battery_percent = 20", "low_battery_percent = 98");
        let mut engine = Engine::new(config_from(&text), on_battery(50));
        assert!(engine.status().power.low);
        assert_eq!(engine.handle(Event::Battery(99)), vec![]);
        assert!(engine.status().power.low);
        engine.handle(Event::Battery(100));
        assert!(!engine.status().power.low);
    }

    /// Scenario 9: a reload is a command for `main`; the engine keeps its state
    /// and mode until a validated config comes back through `set_config`.
    #[test]
    fn reload_keeps_state_and_mode() {
        let mut engine = engine_in(State::Dimmed);
        set_mode(&mut engine, "bat");

        assert_eq!(
            engine.handle(Event::ReloadRequested),
            vec![Command::Reload { reply_to: None }]
        );
        assert_eq!(
            engine.handle(ipc(Request::Reload)),
            vec![Command::Reload { reply_to: Some(7) }]
        );
        assert_eq!(engine.state(), State::Dimmed);
        assert_eq!(engine.mode(), &ModeSelection::Manual("bat".into()));

        // A manual mode survives a new config; the stages are re-read from it.
        let text = CONFIG.replace("sleep_after = \"10m\"", "sleep_after = \"20m\"");
        let new = config_from(&text);
        assert_eq!(
            engine.set_config(new.clone()),
            vec![
                Command::ApplyMode("bat".into()),
                Command::ReplaceIdleStages(Stages::from_mode(&new.modes["bat"])),
            ]
        );
        assert_eq!(engine.state(), State::Dimmed);
        assert_eq!(engine.mode(), &ModeSelection::Manual("bat".into()));
    }

    #[test]
    fn set_config_recomputes_an_auto_mode() {
        let mut engine = fresh();
        let text = CONFIG.replace("on_ac = \"ac\"", "on_ac = \"nodim\"");
        assert_eq!(
            engine.set_config(config_from(&text)),
            vec![
                Command::ApplyMode("nodim".into()),
                Command::ReplaceIdleStages(stages("nodim")),
            ]
        );
        assert_eq!(engine.mode(), &ModeSelection::Auto("nodim".into()));
    }

    /// Scenario 10: with `dim` disabled the first stage is `screen_off`.
    #[test]
    fn disabled_dim_stage_skips_to_screen_off() {
        let mut engine = fresh();
        set_mode(&mut engine, "nodim");
        assert_eq!(engine.stages().dim, None);
        assert_eq!(
            effects(engine.handle(Event::Idle(Stage::ScreenOff))),
            vec![Command::Screen(false)]
        );
        assert_eq!(engine.state(), State::ScreenOff);
        assert_eq!(
            effects(engine.handle(Event::Activity)),
            vec![Command::Undim, Command::Screen(true)]
        );
    }

    #[test]
    fn auto_mode_disabled_falls_back_to_the_first_mode() {
        let text = CONFIG.replace("enabled = true", "enabled = false");
        let mut engine = Engine::new(config_from(&text), PowerSnapshot::on_ac());
        // BTreeMap order: "ac" < "bat" < "low" < "nodim".
        assert_eq!(engine.mode(), &ModeSelection::Auto("ac".into()));
        assert_eq!(
            engine.handle(Event::AcChanged(false)),
            vec![Command::Broadcast(StateEvent::Power { ac: false })]
        );
        assert_eq!(engine.mode(), &ModeSelection::Auto("ac".into()));
    }

    #[test]
    fn no_modes_at_all_means_no_apply_and_no_stages() {
        let mut engine = Engine::new(
            config_from("[auto_mode]\nenabled = false\n"),
            PowerSnapshot::on_ac(),
        );
        assert_eq!(
            engine.start(),
            vec![Command::ReplaceIdleStages(Stages::NONE)]
        );
        assert!(engine.stages().is_empty());
    }

    // ------------------------------------------------------------------- ipc

    #[test]
    fn ipc_mode() {
        let mut engine = fresh();
        assert_eq!(
            engine.handle(ipc(Request::Mode { name: "bat".into() })),
            vec![
                Command::ApplyMode("bat".into()),
                Command::ReplaceIdleStages(stages("bat")),
                Command::Broadcast(StateEvent::Mode { name: "bat".into() }),
                Command::Reply(7, Response::Ok),
            ]
        );
        assert_eq!(engine.mode(), &ModeSelection::Manual("bat".into()));

        assert_eq!(
            engine.handle(ipc(Request::Mode {
                name: "turbo".into()
            })),
            vec![Command::Reply(7, Response::error("no such mode: turbo"))]
        );
        assert_eq!(engine.mode(), &ModeSelection::Manual("bat".into()));

        let commands = engine.handle(ipc(Request::Modes));
        let Command::Reply(7, Response::Modes(data)) = &commands[0] else {
            panic!("{commands:?}");
        };
        assert_eq!(data.modes, vec!["ac", "bat", "low", "nodim"]);
        assert_eq!(data.current, "bat");
        assert_eq!(data.source, "manual");
    }

    #[test]
    fn ipc_status_reflects_the_engine() {
        let mut engine = engine_in(State::ScreenOff);
        let commands = engine.handle(ipc(Request::Status));
        let Command::Reply(7, Response::Status(status)) = &commands[0] else {
            panic!("{commands:?}");
        };
        assert_eq!(status.state, "ScreenOff");
        assert_eq!(status.mode.name, "ac");
        assert_eq!(status.mode.source, "auto");
        assert!(status.power.ac);
        assert_eq!(status.idle.stages.dim, "5m");
        assert_eq!(status.idle.stages.sleep, "30m");
        assert_eq!(status.sleep.method, "suspend");
        assert_eq!(status.server.phase, "idle");
        assert!(status.inhibitors.is_empty());
    }

    #[test]
    fn ipc_dim_undim_and_screen() {
        let mut engine = fresh();
        assert_eq!(
            effects(engine.handle(ipc(Request::Dim))),
            vec![Command::Dim(10), Command::Reply(7, Response::Ok)]
        );
        assert_eq!(engine.state(), State::Dimmed);
        // Idempotent.
        assert_eq!(
            engine.handle(ipc(Request::Dim)),
            vec![Command::Reply(7, Response::Ok)]
        );

        assert_eq!(
            effects(engine.handle(ipc(Request::Undim))),
            vec![Command::Undim, Command::Reply(7, Response::Ok)]
        );
        assert_eq!(engine.state(), State::Active);

        assert_eq!(
            effects(engine.handle(ipc(Request::Screen {
                state: ScreenState::Off
            }))),
            vec![Command::Screen(false), Command::Reply(7, Response::Ok)]
        );
        assert_eq!(engine.state(), State::ScreenOff);
        assert_eq!(
            effects(engine.handle(ipc(Request::Screen {
                state: ScreenState::On
            }))),
            vec![
                Command::Undim,
                Command::Screen(true),
                Command::Reply(7, Response::Ok),
            ]
        );
        assert_eq!(engine.state(), State::Active);
    }

    #[test]
    fn ipc_sleep_reports_blockers_instead_of_retrying() {
        let mut engine = fresh();
        engine.handle(Event::Inhibitors(vec![inhibitor("firefox")]));

        assert_eq!(
            engine.handle(ipc(Request::Sleep { force: false })),
            vec![Command::Reply(
                7,
                Response::error("blocked by inhibitors: firefox: busy")
            )]
        );
        assert_eq!(engine.state(), State::Active);
        assert_eq!(engine.handle(Event::Timer(TimerId::SleepRetry)), vec![]);

        assert_eq!(
            effects(engine.handle(ipc(Request::Sleep { force: true }))),
            vec![
                Command::Suspend {
                    method: SleepMethod::Suspend,
                    force: true,
                },
                Command::Reply(7, Response::Ok),
            ]
        );
        assert_eq!(engine.state(), State::Suspending);
    }

    #[test]
    fn ipc_inhibit_sleep_blocks_and_expires() {
        let mut engine = engine_in(State::ScreenOff);
        let ttl = Duration::from_secs(600);
        assert_eq!(
            engine.handle(ipc(Request::Inhibit {
                what: InhibitWhat::Sleep,
                why: "backup".into(),
                ttl,
            })),
            vec![
                Command::StartTimer(TimerId::InhibitExpiry(1), ttl),
                Command::Reply(7, Response::Inhibited { id: 1 }),
            ]
        );
        assert_eq!(engine.status().sleep.blocked_by, vec!["ampered#1: backup"]);
        assert_eq!(engine.status().inhibitors.len(), 1);
        assert_eq!(
            engine.handle(Event::Idle(Stage::Sleep)),
            vec![Command::StartTimer(TimerId::SleepRetry, SLEEP_RETRY)]
        );

        // The next one gets the next id.
        let commands = engine.handle(ipc(Request::Inhibit {
            what: InhibitWhat::Sleep,
            why: "more".into(),
            ttl,
        }));
        assert_eq!(
            commands[1],
            Command::Reply(7, Response::Inhibited { id: 2 })
        );

        assert_eq!(
            engine.handle(ipc(Request::Uninhibit { id: 2 })),
            vec![
                Command::CancelTimer(TimerId::InhibitExpiry(2)),
                Command::Reply(7, Response::Ok),
            ]
        );
        assert_eq!(
            engine.handle(ipc(Request::Uninhibit { id: 2 })),
            vec![Command::Reply(7, Response::error("no such inhibitor: 2"))]
        );

        assert_eq!(
            engine.handle(Event::Timer(TimerId::InhibitExpiry(1))),
            vec![]
        );
        assert!(engine.status().inhibitors.is_empty());
        assert_eq!(
            effects(engine.handle(Event::Timer(TimerId::SleepRetry))),
            vec![SUSPEND]
        );
    }

    #[test]
    fn ipc_inhibit_idle_ignores_the_stages() {
        let mut engine = fresh();
        engine.handle(ipc(Request::Inhibit {
            what: InhibitWhat::Idle,
            why: "movie".into(),
            ttl: Duration::from_secs(60),
        }));
        assert_eq!(engine.handle(Event::Idle(Stage::Dim)), vec![]);
        assert_eq!(engine.handle(Event::Idle(Stage::Sleep)), vec![]);
        assert_eq!(engine.state(), State::Active);
        // An idle inhibit does not block an explicit sleep.
        assert!(engine.status().sleep.blocked_by.is_empty());

        engine.handle(Event::Timer(TimerId::InhibitExpiry(1)));
        assert_eq!(
            effects(engine.handle(Event::Idle(Stage::Dim))),
            vec![Command::Dim(10)]
        );
    }

    #[test]
    fn ipc_inhibit_ttl_bounds() {
        let mut engine = fresh();
        assert_eq!(
            engine.handle(ipc(Request::Inhibit {
                what: InhibitWhat::Sleep,
                why: "x".into(),
                ttl: Duration::ZERO,
            })),
            vec![Command::Reply(
                7,
                Response::error("ttl must be greater than zero")
            )]
        );
        let commands = engine.handle(ipc(Request::Inhibit {
            what: InhibitWhat::Sleep,
            why: "x".into(),
            ttl: MAX_INHIBIT_TTL * 10,
        }));
        assert_eq!(
            commands[0],
            Command::StartTimer(TimerId::InhibitExpiry(1), MAX_INHIBIT_TTL)
        );
    }

    #[test]
    fn ipc_not_ours() {
        let mut engine = fresh();
        let commands = engine.handle(ipc(Request::Subscribe));
        assert!(matches!(
            &commands[0],
            Command::Reply(7, Response::Error(_))
        ));
        assert_eq!(engine.state(), State::Active);
    }

    // ---------------------------------------------------------- server cycle

    const SERVER: &str = r#"
        [server]
        enabled = true
        trigger = "ac_lost"
        grace_period = "3m"
        check_interval = "20m"
        awake_window = "45s"
        alarm_slack = "90s"
        battery_critical_percent = 10
        critical_action = "hibernate"
        resume_hook = "systemctl start my-services.target"
    "#;
    const GRACE: Duration = Duration::from_secs(180);
    const CHECK_INTERVAL: Duration = Duration::from_secs(20 * 60);
    const AWAKE_WINDOW: Duration = Duration::from_secs(45);
    const HIBERNATE: Command = Command::Suspend {
        method: SleepMethod::Hibernate,
        force: true,
    };

    fn server_config(extra: &str) -> Arc<Config> {
        config_from(&format!("{CONFIG}\n{SERVER}\n{extra}"))
    }

    /// A server on AC, in the `ac` mode.
    fn server() -> Engine {
        Engine::new(
            server_config(""),
            PowerSnapshot {
                ac: true,
                battery: Some(50),
                batteries: vec!["BAT0".into()],
            },
        )
    }

    /// The server driven into `phase` along the documented cycle.
    fn server_in(phase: Phase) -> Engine {
        let mut engine = server();
        engine.handle(Event::AcChanged(false));
        assert_eq!(engine.state(), State::LongSleep(Phase::Grace));
        let path: &[Event] = match phase {
            Phase::Grace => &[],
            Phase::Armed => &[Event::Timer(TimerId::Grace)],
            Phase::Sleeping => &[
                Event::Timer(TimerId::Grace),
                Event::WakeScheduled(true),
                Event::Suspending,
            ],
            Phase::Checking => &[
                Event::Timer(TimerId::Grace),
                Event::WakeScheduled(true),
                Event::Suspending,
                Event::Resumed,
            ],
        };
        for event in path {
            engine.handle(event.clone());
        }
        assert_eq!(engine.state(), State::LongSleep(phase));
        engine
    }

    fn phase(name: &str) -> Command {
        Command::Broadcast(StateEvent::LongSleep {
            phase: name.into(),
            next_wake: None,
        })
    }

    /// Leaving the cycle: alarm gone, mode and stages back, screen on.
    fn back_to_active(mode: &str) -> Vec<Command> {
        vec![
            Command::CancelWake,
            Command::ApplyMode(mode.into()),
            Command::ReplaceIdleStages(stages(mode)),
            Command::Undim,
            Command::Screen(true),
        ]
    }

    #[test]
    fn ac_lost_enters_grace_and_ac_back_leaves_it() {
        let mut engine = server();
        assert_eq!(
            engine.handle(Event::AcChanged(false)),
            vec![
                Command::Broadcast(StateEvent::Power { ac: false }),
                Command::ApplyMode("bat".into()),
                Command::ReplaceIdleStages(stages("bat")),
                Command::Broadcast(StateEvent::Mode { name: "bat".into() }),
                Command::StartTimer(TimerId::Grace, GRACE),
                Command::Broadcast(StateEvent::State {
                    from: "Active".into(),
                    to: "LongSleep(grace)".into(),
                }),
                phase("grace"),
            ]
        );
        assert_eq!(engine.status().server.phase, "grace");

        // Power back within the grace period: nothing else happened.
        assert_eq!(
            effects(engine.handle(Event::AcChanged(true))),
            vec![
                Command::CancelTimer(TimerId::Grace),
                Command::ApplyMode("ac".into()),
                Command::ReplaceIdleStages(stages("ac")),
            ]
        );
        assert_eq!(engine.state(), State::Active);
        assert_eq!(engine.status().server.phase, "idle");
    }

    #[test]
    fn grace_starts_from_every_awake_state() {
        for state in [State::Active, State::Dimmed, State::ScreenOff] {
            let mut engine = server();
            match state {
                State::Dimmed => {
                    engine.handle(Event::Idle(Stage::Dim));
                }
                State::ScreenOff => {
                    engine.handle(Event::Idle(Stage::Dim));
                    engine.handle(Event::Idle(Stage::ScreenOff));
                }
                _ => {}
            }
            engine.handle(Event::AcChanged(false));
            assert_eq!(engine.state(), State::LongSleep(Phase::Grace), "{state}");
        }
    }

    #[test]
    fn idle_stages_are_ignored_in_the_cycle() {
        let mut engine = server_in(Phase::Grace);
        assert_eq!(engine.handle(Event::Idle(Stage::Dim)), vec![]);
        assert_eq!(engine.handle(Event::Idle(Stage::Sleep)), vec![]);
        assert_eq!(engine.handle(Event::Activity), vec![]);
        assert_eq!(engine.state(), State::LongSleep(Phase::Grace));
    }

    /// Scenario 6: Grace → Armed → Sleeping → Checking → Armed, exit by AC.
    #[test]
    fn full_cycle_exits_when_power_is_back() {
        let mut engine = server_in(Phase::Grace);

        assert_eq!(
            engine.handle(Event::Timer(TimerId::Grace)),
            vec![
                Command::ReplaceIdleStages(Stages::NONE),
                Command::ScheduleWake(CHECK_INTERVAL),
                Command::Broadcast(StateEvent::State {
                    from: "LongSleep(grace)".into(),
                    to: "LongSleep(armed)".into(),
                }),
                phase("armed"),
            ]
        );
        // No suspend before the alarm is confirmed.
        assert_eq!(engine.handle(Event::WakeScheduled(true)), vec![SUSPEND]);
        assert_eq!(engine.state(), State::LongSleep(Phase::Armed));

        assert_eq!(effects(engine.handle(Event::Suspending)), vec![]);
        assert_eq!(engine.state(), State::LongSleep(Phase::Sleeping));

        // The alarm fired, still on battery: wait out the window.
        assert_eq!(
            effects(engine.handle(Event::Resumed)),
            vec![Command::StartTimer(TimerId::AwakeWindow, AWAKE_WINDOW)]
        );
        assert_eq!(engine.state(), State::LongSleep(Phase::Checking));
        assert_eq!(engine.status().server.phase, "checking");

        // Stages stay off from here on: no second ReplaceIdleStages.
        assert_eq!(
            effects(engine.handle(Event::Timer(TimerId::AwakeWindow))),
            vec![Command::ScheduleWake(CHECK_INTERVAL)]
        );
        assert_eq!(engine.state(), State::LongSleep(Phase::Armed));
        assert_eq!(engine.handle(Event::WakeScheduled(true)), vec![SUSPEND]);
        engine.handle(Event::Suspending);

        // Power returned while asleep; the next alarm sees it.
        engine.handle(Event::AcChanged(true));
        assert_eq!(engine.state(), State::LongSleep(Phase::Sleeping));
        assert_eq!(
            engine.handle(Event::Resumed),
            vec![
                Command::RunHook("systemctl start my-services.target".into()),
                Command::CancelWake,
                Command::ApplyMode("ac".into()),
                Command::ReplaceIdleStages(stages("ac")),
                Command::Undim,
                Command::Screen(true),
                Command::Broadcast(StateEvent::State {
                    from: "LongSleep(sleeping)".into(),
                    to: "Active".into(),
                }),
                phase("idle"),
            ]
        );
        assert_eq!(engine.mode(), &ModeSelection::Auto("ac".into()));
    }

    #[test]
    fn ac_back_during_the_window_ends_the_cycle() {
        let mut engine = server_in(Phase::Checking);
        assert_eq!(
            effects(engine.handle(Event::AcChanged(true))),
            vec![
                Command::CancelTimer(TimerId::AwakeWindow),
                Command::RunHook("systemctl start my-services.target".into()),
                Command::CancelWake,
                Command::ApplyMode("ac".into()),
                Command::ReplaceIdleStages(stages("ac")),
                Command::Undim,
                Command::Screen(true),
            ]
        );
        assert_eq!(engine.state(), State::Active);
    }

    #[test]
    fn manual_trigger_needs_the_request() {
        let text =
            format!("{CONFIG}\n{SERVER}").replace("trigger = \"ac_lost\"", "trigger = \"manual\"");
        let mut engine = Engine::new(config_from(&text), PowerSnapshot::on_ac());
        engine.handle(Event::AcChanged(false));
        assert_eq!(engine.state(), State::Active);

        assert_eq!(
            effects(engine.handle(ipc(Request::LongSleep { cancel: false }))),
            vec![
                Command::StartTimer(TimerId::Grace, Duration::ZERO),
                Command::Reply(7, Response::Ok),
            ]
        );
        assert_eq!(engine.state(), State::LongSleep(Phase::Grace));
    }

    /// Scenario 6, exit by the user: the daemon turns a wake by the user
    /// into `Activity` (ADR-13).
    #[test]
    fn user_wake_interrupts_the_cycle() {
        let mut engine = server_in(Phase::Checking);
        let mut expected = vec![Command::CancelTimer(TimerId::AwakeWindow)];
        expected.extend(back_to_active("bat"));
        expected.push(Command::Broadcast(StateEvent::State {
            from: "LongSleep(checking)".into(),
            to: "Active".into(),
        }));
        expected.push(phase("interrupted"));
        assert_eq!(engine.handle(Event::Activity), expected);
        assert_eq!(engine.status().server.phase, "interrupted");

        // Still on battery: only an explicit request resumes the cycle.
        assert_eq!(engine.handle(Event::AcChanged(false)), vec![]);
        let commands = engine.handle(ipc(Request::LongSleep { cancel: false }));
        assert_eq!(
            commands[0],
            Command::StartTimer(TimerId::Grace, Duration::ZERO)
        );
        assert_eq!(engine.status().server.phase, "grace");
    }

    /// Scenario 6, exit by critical battery.
    #[test]
    fn critical_battery_hibernates_and_keeps_checking() {
        let mut engine = server_in(Phase::Sleeping);
        engine.handle(Event::Battery(9));
        assert_eq!(
            engine.handle(Event::Resumed),
            vec![
                phase("critical"),
                HIBERNATE,
                Command::StartTimer(TimerId::AwakeWindow, AWAKE_WINDOW),
                Command::Broadcast(StateEvent::State {
                    from: "LongSleep(sleeping)".into(),
                    to: "LongSleep(checking)".into(),
                }),
                phase("checking"),
            ]
        );

        // The hibernate did not happen: the window tries again.
        assert_eq!(
            engine.handle(Event::Timer(TimerId::AwakeWindow)),
            vec![
                phase("critical"),
                HIBERNATE,
                Command::StartTimer(TimerId::AwakeWindow, AWAKE_WINDOW),
            ]
        );

        // Charged a little above the threshold: back to the cycle.
        engine.handle(Event::Battery(11));
        assert_eq!(
            effects(engine.handle(Event::Timer(TimerId::AwakeWindow))),
            vec![Command::ScheduleWake(CHECK_INTERVAL)]
        );
    }

    #[test]
    fn critical_hibernate_degrades_to_poweroff() {
        let mut engine = server_in(Phase::Sleeping);
        engine.handle(Event::HibernateAvailable(false));
        engine.handle(Event::Battery(5));
        let commands = engine.handle(Event::Resumed);
        assert_eq!(commands[0], phase("critical"));
        assert_eq!(commands[1], Command::PowerOff);
    }

    #[test]
    fn critical_action_poweroff() {
        let text = format!("{CONFIG}\n{SERVER}").replace(
            "critical_action = \"hibernate\"",
            "critical_action = \"poweroff\"",
        );
        let mut engine = Engine::new(config_from(&text), PowerSnapshot::on_ac());
        engine.handle(Event::AcChanged(false));
        engine.handle(Event::Timer(TimerId::Grace));
        engine.handle(Event::WakeScheduled(true));
        engine.handle(Event::Suspending);
        engine.handle(Event::Battery(10));
        let commands = engine.handle(Event::Resumed);
        assert_eq!(commands[0], phase("critical"));
        assert_eq!(commands[1], Command::PowerOff);
    }

    /// Scenario 7: three attempts that do not sleep end the cycle.
    #[test]
    fn three_failures_abandon_the_cycle() {
        let mut engine = server_in(Phase::Armed);

        // No RTC: never sleep without an alarm.
        assert_eq!(
            engine.handle(Event::WakeScheduled(false)),
            vec![Command::StartTimer(TimerId::AwakeWindow, AWAKE_WINDOW)]
        );
        assert_eq!(engine.state(), State::LongSleep(Phase::Armed));
        assert_eq!(
            engine.handle(Event::Timer(TimerId::AwakeWindow)),
            vec![Command::ScheduleWake(CHECK_INTERVAL)]
        );

        // logind refused the suspend.
        engine.handle(Event::WakeScheduled(true));
        assert_eq!(
            engine.handle(Event::SleepBlocked(vec![inhibitor("backup")])),
            vec![Command::StartTimer(TimerId::AwakeWindow, AWAKE_WINDOW)]
        );
        engine.handle(Event::Timer(TimerId::AwakeWindow));

        // The suspend request never reached logind (`Activity` from main).
        let mut expected = back_to_active("bat");
        expected.push(Command::Broadcast(StateEvent::State {
            from: "LongSleep(armed)".into(),
            to: "Active".into(),
        }));
        expected.push(phase("idle"));
        assert_eq!(engine.handle(Event::Activity), expected);
    }

    #[test]
    fn power_back_while_armed_ends_the_cycle_without_sleeping() {
        let mut engine = server_in(Phase::Armed);
        engine.handle(Event::AcChanged(true));
        let commands = effects(engine.handle(Event::WakeScheduled(true)));
        assert_eq!(
            commands[0],
            Command::RunHook("systemctl start my-services.target".into())
        );
        assert!(!commands.contains(&SUSPEND));
        assert_eq!(engine.state(), State::Active);

        // The same while waiting out the window after a failed attempt.
        let mut engine = server_in(Phase::Armed);
        engine.handle(Event::WakeScheduled(false));
        engine.handle(Event::AcChanged(true));
        let commands = effects(engine.handle(Event::Timer(TimerId::AwakeWindow)));
        assert!(commands.contains(&Command::CancelWake));
        assert_eq!(engine.state(), State::Active);
    }

    #[test]
    fn reload_in_the_cycle_keeps_the_stages_off() {
        let mut engine = server_in(Phase::Checking);
        let commands = engine.set_config(server_config(""));
        assert!(commands.contains(&Command::ReplaceIdleStages(Stages::NONE)));
        assert_eq!(engine.state(), State::LongSleep(Phase::Checking));

        // In Grace the stages are still the mode's.
        let mut engine = server_in(Phase::Grace);
        let commands = engine.set_config(server_config(""));
        assert!(commands.contains(&Command::ReplaceIdleStages(stages("bat"))));
    }

    #[test]
    fn cached_inhibitors_count_as_a_failure() {
        let mut engine = server_in(Phase::Grace);
        engine.handle(Event::Inhibitors(vec![inhibitor("backup")]));
        engine.handle(Event::Timer(TimerId::Grace));
        assert_eq!(
            engine.handle(Event::WakeScheduled(true)),
            vec![Command::StartTimer(TimerId::AwakeWindow, AWAKE_WINDOW)]
        );
        engine.handle(Event::Inhibitors(vec![]));
        engine.handle(Event::Timer(TimerId::AwakeWindow));
        assert_eq!(engine.handle(Event::WakeScheduled(true)), vec![SUSPEND]);
    }

    #[test]
    fn a_late_wake_confirmation_is_ignored() {
        let mut engine = server_in(Phase::Grace);
        assert_eq!(engine.handle(Event::WakeScheduled(true)), vec![]);
        let mut engine = fresh();
        assert_eq!(engine.handle(Event::WakeScheduled(false)), vec![]);
    }

    #[test]
    fn someone_else_suspends_during_the_cycle() {
        let mut engine = server_in(Phase::Grace);
        engine.handle(Event::Suspending);
        assert_eq!(engine.state(), State::LongSleep(Phase::Sleeping));
        engine.handle(Event::Resumed);
        assert_eq!(engine.state(), State::LongSleep(Phase::Checking));
    }

    #[test]
    fn ipc_long_sleep_cancel() {
        let mut engine = server_in(Phase::Grace);
        let mut expected = vec![Command::CancelTimer(TimerId::Grace)];
        expected.extend(back_to_active("bat"));
        expected.push(Command::Reply(7, Response::Ok));
        assert_eq!(
            effects(engine.handle(ipc(Request::LongSleep { cancel: true }))),
            expected
        );
        assert_eq!(engine.state(), State::Active);
        assert_eq!(engine.status().server.phase, "idle");

        assert_eq!(
            engine.handle(ipc(Request::LongSleep { cancel: true })),
            vec![Command::Reply(
                7,
                Response::error("not in the long-sleep cycle")
            )]
        );

        let mut engine = server_in(Phase::Checking);
        let commands = effects(engine.handle(ipc(Request::LongSleep { cancel: true })));
        assert_eq!(commands[0], Command::CancelTimer(TimerId::AwakeWindow));
        assert_eq!(engine.state(), State::Active);
    }

    #[test]
    fn ipc_long_sleep_refusals() {
        let mut engine = fresh();
        assert_eq!(
            engine.handle(ipc(Request::LongSleep { cancel: false })),
            vec![Command::Reply(
                7,
                Response::error("[server] is not enabled")
            )]
        );

        let mut engine = server_in(Phase::Grace);
        assert_eq!(
            engine.handle(ipc(Request::LongSleep { cancel: false })),
            vec![Command::Reply(
                7,
                Response::error("already in the long-sleep cycle (grace)")
            )]
        );

        let mut engine = server();
        engine.handle(Event::Idle(Stage::Sleep));
        assert_eq!(engine.state(), State::Suspending);
        let commands = engine.handle(ipc(Request::LongSleep { cancel: false }));
        assert!(matches!(
            &commands[0],
            Command::Reply(7, Response::Error(_))
        ));
    }

    #[test]
    fn resume_cycle_after_a_restart() {
        let mut engine = Engine::new(server_config(""), on_battery(40));
        assert_eq!(
            engine.resume_cycle(),
            vec![
                Command::ReplaceIdleStages(Stages::NONE),
                Command::StartTimer(TimerId::AwakeWindow, AWAKE_WINDOW),
            ]
        );
        assert_eq!(engine.state(), State::LongSleep(Phase::Checking));
        assert_eq!(
            effects(engine.handle(Event::Timer(TimerId::AwakeWindow))),
            vec![Command::ScheduleWake(CHECK_INTERVAL)]
        );
    }

    #[test]
    fn shutdown_in_the_cycle_restores_the_screen() {
        let mut engine = server_in(Phase::Checking);
        assert_eq!(
            effects(engine.handle(Event::ShutdownRequested)),
            vec![Command::Undim, Command::Screen(true), Command::Shutdown]
        );
    }
}

//! Daemon entry point: CLI, logging, actors, the command loop.
//!
//! See `docs/01-architecture.md`. This is the only "dirty" place in the
//! crate (ADR-2): everything the engine decides is executed here, and
//! nothing here decides anything.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::Parser;
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use ampered::backlight::Controller as Backlight;
use ampered::config::{Config, IdleFallback};
use ampered::core::{Command, Engine, Event, Phase, State, TimerId};
use ampered::display::Display;
use ampered::idle::{self, IdleHandle};
use ampered::ipc::{self, RequestId, Response, StateEvent, StatusData};
use ampered::logind::{self, LogindHandle, SavedState, SharedState};
use ampered::power::modes::{self, ModeApplier, SysfsModeSink};
use ampered::power::supply::{self, FakePowerSource, SupplyHandle, SysfsPowerSource};
use ampered::power::{PowerSnapshot, PowerSource};
use ampered::sleep::planner::{self, Planner, Wake};
use ampered::sleep::rtc::{self, RtcAlarm};
use ampered::timers::Timers;

const DEFAULT_CONFIG: &str = "/etc/ampered/ampered.toml";

#[derive(Debug, Parser)]
#[command(
    name = "ampered",
    version,
    about = "Power management daemon for Wayland laptops"
)]
struct Cli {
    /// Path to ampered.toml.
    #[arg(long, default_value = DEFAULT_CONFIG, value_name = "PATH")]
    config: PathBuf,

    /// Override [general] socket.
    #[arg(long, value_name = "PATH")]
    socket: Option<PathBuf>,

    /// Validate the config and exit.
    #[arg(long)]
    check: bool,

    /// Pretend the power supply reads `ac`, `bat` or `bat:NN` (development).
    #[arg(long, value_name = "SPEC")]
    fake_power: Option<String>,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    let config =
        Config::load(&cli.config).with_context(|| format!("config {}", cli.config.display()))?;

    if cli.check {
        println!("{}: ok", cli.config.display());
        return Ok(());
    }

    init_logging(&config.general.log_level);

    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(run(cli, config))
}

/// `RUST_LOG` takes priority over `[general] log_level` (`docs/12-configuration.md`).
fn init_logging(log_level: &str) {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(format!("ampered={log_level}")));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();
}

async fn run(cli: Cli, config: Config) -> Result<()> {
    let config = Arc::new(config);
    let socket = cli
        .socket
        .clone()
        .unwrap_or_else(|| config.general.socket.clone());

    let (events_tx, mut events_rx) = mpsc::channel::<Event>(64);
    let server = ipc::listen(&socket, &config.general.socket_group, events_tx.clone())
        .await
        .with_context(|| format!("ipc socket {}", socket.display()))?;
    spawn_signal_handlers(events_tx.clone())?;

    let mut degraded = modes::detect_conflicts().await;

    let backlight = Backlight::from_config(&config.backlight);
    if !backlight.is_available() {
        degraded.push("backlight".into());
    }
    let display = Display::from_config(&config);
    if !display.is_available() {
        degraded.push("display".into());
    }

    let source: Arc<dyn PowerSource + Send + Sync> = match &cli.fake_power {
        Some(spec) => {
            warn!(spec, "using a fake power source");
            Arc::new(FakePowerSource::parse(spec).map_err(anyhow::Error::msg)?)
        }
        None => Arc::new(SysfsPowerSource::new()),
    };
    let initial = match source.snapshot() {
        Ok(snapshot) => snapshot,
        Err(err) => {
            warn!(%err, "cannot read the power supply, assuming AC");
            degraded.push("power".into());
            PowerSnapshot::on_ac()
        }
    };
    info!(ac = initial.ac, battery = ?initial.battery, "power at startup");
    let supply = supply::spawn(source, initial.clone(), events_tx.clone());
    let idle = idle::spawn(config.wayland.clone(), events_tx.clone());
    if config.idle.fallback == IdleFallback::Logind {
        warn!("[idle] fallback = \"logind\" is not implemented in v0.1; ext-idle-notify only");
    }

    let saved: SharedState = Arc::new(std::sync::Mutex::new(SavedState::default()));
    let logind = logind::connect(config.clone(), events_tx.clone(), saved.clone()).await;
    match &logind {
        Some(handle) if !handle.hibernate_available() => degraded.push("hibernate".into()),
        Some(_) => {}
        None => degraded.push("logind".into()),
    }

    let rtc = rtc_for(&config);
    if config.server.enabled && rtc.is_none() {
        error!("no usable RTC alarm; long sleep is disabled");
        degraded.push("rtc".into());
    }

    let mut daemon = Daemon {
        config_path: cli.config.clone(),
        config: config.clone(),
        socket,
        server,
        timers: Timers::new(events_tx.clone()),
        modes: ModeApplier::new(SysfsModeSink::new()),
        backlight,
        display,
        idle,
        logind,
        saved,
        supply,
        rtc,
        planner: Planner::default(),
        degraded,
    };

    let mut engine = Engine::new(config.clone(), initial.clone());
    let mut commands = engine.start();
    // Restarted in the middle of the cycle, still without power: carry on
    // from `Checking` (`docs/10-long-sleep-rtc.md`).
    if config.server.enabled
        && !initial.ac
        && logind::load_saved_state().is_some_and(|saved| saved.in_long_sleep())
    {
        commands.extend(engine.resume_cycle());
    }
    daemon.remember(&engine);

    info!(version = env!("CARGO_PKG_VERSION"), "ampered started");

    loop {
        let mut queue: std::collections::VecDeque<_> = commands.drain(..).collect();
        while let Some(command) = queue.pop_front() {
            match daemon.execute(command, &mut engine).await {
                Outcome::Continue(more) => queue.extend(more),
                Outcome::Shutdown => {
                    // The undim must finish before the process does.
                    daemon.backlight.settle().await;
                    daemon.cleanup();
                    info!("ampered stopped");
                    return Ok(());
                }
            }
        }

        let Some(event) = events_rx.recv().await else {
            warn!("every event source is gone");
            daemon.cleanup();
            return Ok(());
        };
        let mut woke_in_cycle = false;
        match &event {
            Event::Timer(id) => daemon.timers.forget(*id),
            // Values read straight after resume can still be the old ones.
            Event::Resumed => {
                daemon.supply.recheck_after_resume();
                woke_in_cycle = engine.state() == State::LongSleep(Phase::Sleeping);
            }
            _ => {}
        }
        debug!(?event, "event");
        commands = engine.handle(event);
        if woke_in_cycle {
            commands.extend(daemon.classify_wake(&mut engine));
        }
        daemon.remember(&engine);
    }
}

enum Outcome {
    Continue(Vec<Command>),
    Shutdown,
}

struct Daemon {
    config_path: PathBuf,
    config: Arc<Config>,
    socket: PathBuf,
    server: ipc::Server,
    timers: Timers,
    modes: ModeApplier<SysfsModeSink>,
    backlight: Backlight,
    display: Display,
    idle: IdleHandle,
    logind: Option<LogindHandle>,
    saved: SharedState,
    supply: SupplyHandle,
    rtc: Option<Box<dyn RtcAlarm>>,
    planner: Planner,
    degraded: Vec<String>,
}

/// The RTC is only touched when the server cycle can use it.
fn rtc_for(config: &Config) -> Option<Box<dyn RtcAlarm>> {
    if !config.server.enabled {
        return None;
    }
    rtc::from_config(&config.server)
}

impl Daemon {
    async fn execute(&mut self, command: Command, engine: &mut Engine) -> Outcome {
        match command {
            Command::StartTimer(id, after) => self.timers.start(id, after),
            Command::CancelTimer(id) => self.timers.cancel(id),
            Command::ReplaceIdleStages(stages) => self.idle.replace_stages(stages),
            Command::Suspend { method, force } => {
                let delivered = match &self.logind {
                    Some(logind) => logind.suspend(method, force),
                    None => {
                        warn!("no logind, cannot sleep");
                        false
                    }
                };
                if !delivered {
                    // Nothing is going to happen, so do not leave the FSM
                    // sitting in Suspending until the user touches the machine.
                    return Outcome::Continue(engine.handle(Event::Activity));
                }
            }
            // The engine sleeps only once it hears the alarm is armed (ADR-13).
            Command::ScheduleWake(after) => {
                let at = std::time::SystemTime::now() + after;
                let armed = match &self.rtc {
                    Some(rtc) => match rtc.set(at) {
                        Ok(()) => true,
                        Err(err) => {
                            error!(%err, at = %humantime::format_rfc3339_seconds(at), "cannot arm the RTC alarm");
                            false
                        }
                    },
                    None => {
                        error!("no RTC alarm, the wake cannot be scheduled");
                        false
                    }
                };
                if armed {
                    info!(at = %humantime::format_rfc3339_seconds(at), "wake scheduled");
                    self.planner.armed(at);
                } else {
                    self.planner.disarmed();
                }
                let commands = engine.handle(Event::WakeScheduled(armed));
                // The suspend that follows writes `state.json` before the
                // next event gets here; it must carry the alarm.
                self.remember(engine);
                return Outcome::Continue(commands);
            }
            Command::CancelWake => {
                self.planner.disarmed();
                if let Some(rtc) = &self.rtc
                    && let Err(err) = rtc.clear()
                {
                    warn!(%err, "cannot clear the RTC alarm");
                }
            }
            Command::PowerOff => match &self.logind {
                Some(logind) => logind.power_off(),
                None => error!("no logind, cannot power off"),
            },
            Command::RunHook(command) => planner::spawn_hook(command),
            Command::Screen(on) => self.display.set(on).await,
            Command::Dim(percent) => self.backlight.dim_to(percent).await,
            Command::Undim => self.backlight.restore().await,
            Command::ApplyMode(name) => match self.config.modes.get(&name) {
                Some(mode) => self.modes.apply(&name, mode),
                None => error!(
                    mode = name,
                    "the engine asked for a mode that is not configured"
                ),
            },
            Command::Reply(id, response) => self.reply(id, response),
            Command::Broadcast(StateEvent::LongSleep { phase, .. }) => {
                let next_wake = self.next_wake();
                self.server
                    .broadcast(StateEvent::LongSleep { phase, next_wake });
            }
            Command::Broadcast(event) => self.server.broadcast(event),
            Command::Reload { reply_to } => {
                return Outcome::Continue(self.reload(reply_to, engine).await);
            }
            Command::Shutdown => return Outcome::Shutdown,
        }
        Outcome::Continue(Vec::new())
    }

    /// The alarm we armed, or whatever the RTC reports when we did not.
    fn next_wake(&self) -> Option<String> {
        self.planner
            .scheduled()
            .or_else(|| {
                self.rtc
                    .as_ref()
                    .and_then(|rtc| rtc.pending().ok().flatten())
            })
            .map(|at| humantime::format_rfc3339_seconds(at).to_string())
    }

    /// Right after `Resumed` in the cycle: was it the alarm, or the user?
    /// A wake by the user is `Activity` to the engine (ADR-13).
    fn classify_wake(&mut self, engine: &mut Engine) -> Vec<Command> {
        let wake = self
            .planner
            .classify(std::time::SystemTime::now(), self.config.server.alarm_slack);
        match wake {
            Wake::Scheduled => Vec::new(),
            Wake::User => engine.handle(Event::Activity),
        }
    }

    fn reply(&self, id: RequestId, response: Response) {
        let response = match response {
            Response::Status(mut status) => {
                self.enrich_status(&mut status);
                Response::Status(status)
            }
            other => other,
        };
        self.server.reply(id, response);
    }

    /// The engine only knows what it decided; the rest of `status` is ours.
    /// Inhibitor expiry lives in the timer registry, not in the FSM, which
    /// has no clock at all.
    fn enrich_status(&self, status: &mut StatusData) {
        status.degraded = self.degraded.clone();
        status.power.batteries = self.supply.latest().batteries;
        status.backlight = self.backlight.info();
        status.idle.backend = idle::BACKEND.to_string();
        if !status.idle.connected {
            status.degraded.push("wayland".into());
        }
        status.server.next_wake = self.next_wake();
        for inhibitor in &mut status.inhibitors {
            inhibitor.expires = self
                .timers
                .deadline(TimerId::InhibitExpiry(inhibitor.id))
                .map(|at| humantime::format_rfc3339_seconds(at).to_string());
        }
    }

    /// A rejected config leaves everything as it was (`docs/12-configuration.md`).
    async fn reload(&mut self, reply_to: Option<RequestId>, engine: &mut Engine) -> Vec<Command> {
        info!(path = %self.config_path.display(), "reloading config");
        let config = match Config::load(&self.config_path) {
            Ok(config) => config,
            Err(err) => {
                error!(%err, "config rejected, keeping the previous one");
                if let Some(id) = reply_to {
                    self.server.reply(id, Response::error(err.to_string()));
                }
                return Vec::new();
            }
        };

        if config.general.socket != self.config.general.socket {
            warn!("[general] socket changed; it takes effect after a restart");
        }
        if config.wayland != self.config.wayland {
            warn!("[wayland] changed; it takes effect after a restart");
        }

        // Everything except [general].socket and [wayland] is live
        // (`docs/12-configuration.md`), which includes the two sections the
        // engine knows nothing about.
        if config.backlight != self.config.backlight {
            self.backlight.reconfigure(&config.backlight).await;
            self.mark_degraded("backlight", !self.backlight.is_available());
        }
        if config.display != self.config.display {
            self.display = Display::from_config(&config);
            self.mark_degraded("display", !self.display.is_available());
        }
        if config.server != self.config.server {
            self.rtc = rtc_for(&config);
            self.mark_degraded("rtc", config.server.enabled && self.rtc.is_none());
        }

        let config = Arc::new(config);
        self.config = config.clone();
        let mut commands = engine.set_config(config);
        if let Some(id) = reply_to {
            commands.push(Command::Reply(id, Response::Ok));
        }
        commands
    }

    /// Keeps `state.json` ready: the delay lock gives us only a few seconds
    /// before a suspend (`docs/09-sleep-logind.md`).
    fn remember(&self, engine: &Engine) {
        let long_sleep = matches!(
            engine.state(),
            State::LongSleep(Phase::Armed | Phase::Sleeping)
        );
        *ampered::locked(&self.saved) = SavedState {
            state: engine.state().to_string(),
            mode: engine.mode().name().to_string(),
            reason: if long_sleep { "long-sleep" } else { "regular" }.into(),
            scheduled_wake: self
                .planner
                .scheduled()
                .map(|at| humantime::format_rfc3339_seconds(at).to_string()),
            saved_at: None,
        };
    }

    /// Keeps `status.degraded` honest after a reload changed a subsystem.
    fn mark_degraded(&mut self, subsystem: &str, degraded: bool) {
        self.degraded.retain(|entry| entry != subsystem);
        if degraded {
            self.degraded.push(subsystem.to_string());
        }
    }

    fn cleanup(&self) {
        if let Err(err) = std::fs::remove_file(&self.socket) {
            debug!(%err, socket = %self.socket.display(), "could not remove the socket");
        }
    }
}

fn spawn_signal_handlers(events: mpsc::Sender<Event>) -> Result<()> {
    let mut hangup = signal(SignalKind::hangup())?;
    let mut terminate = signal(SignalKind::terminate())?;
    let mut interrupt = signal(SignalKind::interrupt())?;

    tokio::spawn(async move {
        loop {
            let event = tokio::select! {
                _ = hangup.recv() => Event::ReloadRequested,
                _ = terminate.recv() => Event::ShutdownRequested,
                _ = interrupt.recv() => Event::ShutdownRequested,
            };
            info!(?event, "signal");
            if events.send(event).await.is_err() {
                return;
            }
        }
    });
    Ok(())
}

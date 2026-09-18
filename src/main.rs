//! Daemon entry point: CLI, logging, actors, the command loop.
//!
//! See `docs/01-architecture.md`. This is the only "dirty" place in the
//! crate (ADR-2): everything the engine decides is executed here, and
//! nothing here decides anything.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::Parser;
use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use ampered::config::Config;
use ampered::core::{Command, Engine, Event, TimerId};
use ampered::ipc::{self, RequestId, Response, StatusData};
use ampered::power::PowerSnapshot;
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

    let mut daemon = Daemon {
        config_path: cli.config.clone(),
        config: config.clone(),
        socket,
        server,
        timers: Timers::new(events_tx.clone()),
        degraded: Vec::new(),
    };

    // The real power source arrives with `power::supply`; until then the
    // engine starts from "we are on AC" and corrects itself on the first event.
    let mut engine = Engine::new(config, PowerSnapshot::on_ac());
    let mut commands = engine.start();

    info!(version = env!("CARGO_PKG_VERSION"), "ampered started");

    loop {
        let mut queue: std::collections::VecDeque<_> = commands.drain(..).collect();
        while let Some(command) = queue.pop_front() {
            match daemon.execute(command, &mut engine).await {
                Outcome::Continue(more) => queue.extend(more),
                Outcome::Shutdown => {
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
        if let Event::Timer(id) = &event {
            daemon.timers.forget(*id);
        }
        debug!(?event, "event");
        commands = engine.handle(event);
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
    degraded: Vec<String>,
}

impl Daemon {
    async fn execute(&mut self, command: Command, engine: &mut Engine) -> Outcome {
        match command {
            Command::StartTimer(id, after) => self.timers.start(id, after),
            Command::CancelTimer(id) => self.timers.cancel(id),
            Command::Reply(id, response) => self.reply(id, response),
            Command::Broadcast(event) => self.server.broadcast(event),
            Command::Reload { reply_to } => {
                return Outcome::Continue(self.reload(reply_to, engine))
            }
            Command::Shutdown => return Outcome::Shutdown,
            // Wired up in the steps that follow (`CLAUDE.md`, implementation order).
            other => debug!(?other, "command has no executor yet"),
        }
        Outcome::Continue(Vec::new())
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
        for inhibitor in &mut status.inhibitors {
            inhibitor.expires = self
                .timers
                .deadline(TimerId::InhibitExpiry(inhibitor.id))
                .map(|at| humantime::format_rfc3339_seconds(at).to_string());
        }
    }

    /// A rejected config leaves everything as it was (`docs/12-configuration.md`).
    fn reload(&mut self, reply_to: Option<RequestId>, engine: &mut Engine) -> Vec<Command> {
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

        let config = Arc::new(config);
        self.config = config.clone();
        let mut commands = engine.set_config(config);
        if let Some(id) = reply_to {
            commands.push(Command::Reply(id, Response::Ok));
        }
        commands
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

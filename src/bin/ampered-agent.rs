//! The session agent for split mode: `idle` and `display` running as the
//! user, over the daemon's IPC socket.
//!
//! See `docs/03-privileges.md` and ADR-16. The agent has no config file:
//! the daemon pushes `[display]` and the idle stages down the stream, and
//! the compositor comes from the session's own environment.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use clap::Parser;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use ampered::agent::{AgentEvent, AgentOp, parse_stages};
use ampered::config::Wayland as WaylandConfig;
use ampered::core::Event;
use ampered::display::Display;
use ampered::idle::{self, IdleHandle};

const DEFAULT_SOCKET: &str = "/run/ampered/ampered.sock";
const MAX_BACKOFF: Duration = Duration::from_secs(30);

#[derive(Debug, Parser)]
#[command(
    name = "ampered-agent",
    version,
    about = "Session agent for ampered in split mode"
)]
struct Cli {
    /// The daemon's socket.
    #[arg(long, default_value = DEFAULT_SOCKET, value_name = "PATH")]
    socket: PathBuf,

    /// Where the compositor socket lives; defaults to $XDG_RUNTIME_DIR.
    #[arg(long, value_name = "DIR")]
    runtime_dir: Option<PathBuf>,

    /// The compositor socket name; defaults to $WAYLAND_DISPLAY.
    #[arg(long, value_name = "NAME")]
    display: Option<String>,

    /// Log level, unless RUST_LOG is set.
    #[arg(long, default_value = "info")]
    log_level: String,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let filter = tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        tracing_subscriber::EnvFilter::new(format!("ampered={}", cli.log_level))
    });
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();

    let wayland = WaylandConfig {
        runtime_dir: cli
            .runtime_dir
            .clone()
            .or_else(|| std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from))
            .ok_or_else(|| anyhow!("no --runtime-dir and no XDG_RUNTIME_DIR"))?,
        display: cli
            .display
            .clone()
            .or_else(|| std::env::var("WAYLAND_DISPLAY").ok())
            .ok_or_else(|| anyhow!("no --display and no WAYLAND_DISPLAY"))?,
        ..WaylandConfig::default()
    };

    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(run(cli, wayland))
}

async fn run(cli: Cli, wayland: WaylandConfig) -> Result<()> {
    let (events_tx, mut events_rx) = mpsc::channel::<Event>(64);
    let idle = idle::spawn(wayland.clone(), events_tx);
    let mut agent = Agent {
        wayland,
        idle,
        display: None,
        backend_connected: false,
    };
    info!(
        version = env!("CARGO_PKG_VERSION"),
        socket = %cli.socket.display(),
        "ampered-agent started"
    );

    let mut backoff = Duration::from_secs(1);
    loop {
        match connect(&cli.socket).await {
            Ok((lines, write)) => {
                info!("connected to the daemon");
                backoff = Duration::from_secs(1);
                match agent.serve(lines, write, &mut events_rx).await {
                    Ok(()) => info!("the daemon hung up"),
                    Err(err) => warn!(%err, "the daemon connection failed"),
                }
            }
            Err(err) => debug!(%err, "no daemon yet"),
        }
        // Whatever the compositor says while we are disconnected cannot be
        // delivered; the daemon re-sends the stages when we are back. The
        // connection flag is kept, so the next registration reports it.
        while let Ok(event) = events_rx.try_recv() {
            if let Event::IdleBackendChanged(connected) = event {
                agent.backend_connected = connected;
            }
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(MAX_BACKOFF);
    }
}

type Lines = tokio::io::Lines<BufReader<OwnedReadHalf>>;

async fn connect(socket: &PathBuf) -> Result<(Lines, OwnedWriteHalf)> {
    let stream = UnixStream::connect(socket)
        .await
        .with_context(|| format!("connect {}", socket.display()))?;
    let (read, mut write) = stream.into_split();
    let mut lines = BufReader::new(read).lines();
    write.write_all(b"{\"cmd\":\"agent\"}\n").await?;
    write.flush().await?;
    let reply = lines
        .next_line()
        .await?
        .ok_or_else(|| anyhow!("the daemon closed the connection"))?;
    let reply: serde_json::Value = serde_json::from_str(&reply).context("registration reply")?;
    if reply["ok"] != serde_json::Value::Bool(true) {
        return Err(anyhow!(
            "registration refused: {}",
            reply["error"].as_str().unwrap_or("unknown error")
        ));
    }
    Ok((lines, write))
}

struct Agent {
    wayland: WaylandConfig,
    idle: IdleHandle,
    /// Built from the `display` op; none until the daemon sent one.
    display: Option<Display>,
    backend_connected: bool,
}

impl Agent {
    /// One connection to the daemon: ops in, events out.
    async fn serve(
        &mut self,
        mut lines: Lines,
        mut write: OwnedWriteHalf,
        events: &mut mpsc::Receiver<Event>,
    ) -> Result<()> {
        // The daemon starts from "disconnected" for every new agent.
        self.report_backend(&mut write).await?;
        loop {
            tokio::select! {
                line = lines.next_line() => match line? {
                    Some(line) => {
                        if line.trim().is_empty() {
                            continue;
                        }
                        match serde_json::from_str::<AgentOp>(&line) {
                            Ok(op) => self.apply(op, &mut write).await?,
                            Err(err) => warn!(%err, line, "bad line from the daemon"),
                        }
                    }
                    None => return Ok(()),
                },
                event = events.recv() => match event {
                    Some(event) => self.forward(event, &mut write).await?,
                    None => return Err(anyhow!("the idle watcher is gone")),
                },
            }
        }
    }

    async fn apply(&mut self, op: AgentOp, write: &mut OwnedWriteHalf) -> Result<()> {
        match op {
            AgentOp::Display(config) => {
                self.display = Some(Display::new(&config, &self.wayland, self.idle.clone()));
                self.report_display(write).await?;
            }
            AgentOp::Stages {
                dim,
                screen_off,
                sleep,
            } => self
                .idle
                .replace_stages(parse_stages(&dim, &screen_off, &sleep)),
            AgentOp::Screen { on } => match &mut self.display {
                Some(display) => display.set(on).await,
                None => warn!(on, "screen request before the display config"),
            },
        }
        Ok(())
    }

    async fn forward(&mut self, event: Event, write: &mut OwnedWriteHalf) -> Result<()> {
        let agent_event = match event {
            Event::Idle(stage) => AgentEvent::Idle { stage },
            Event::Activity => AgentEvent::Activity,
            Event::IdleBackendChanged(connected) => {
                self.backend_connected = connected;
                // The `wlr` display comes and goes with the same connection.
                self.report_display(write).await?;
                AgentEvent::Backend {
                    name: idle::BACKEND.to_string(),
                    connected,
                }
            }
            other => {
                debug!(?other, "not an event for the daemon");
                return Ok(());
            }
        };
        send(write, &agent_event).await
    }

    async fn report_backend(&self, write: &mut OwnedWriteHalf) -> Result<()> {
        send(
            write,
            &AgentEvent::Backend {
                name: idle::BACKEND.to_string(),
                connected: self.backend_connected,
            },
        )
        .await?;
        self.report_display(write).await
    }

    async fn report_display(&self, write: &mut OwnedWriteHalf) -> Result<()> {
        let available = self.display.as_ref().is_some_and(Display::is_available);
        send(write, &AgentEvent::Display { available }).await
    }
}

async fn send(write: &mut OwnedWriteHalf, event: &AgentEvent) -> Result<()> {
    let mut line = serde_json::to_string(event)?;
    line.push('\n');
    write.write_all(line.as_bytes()).await?;
    write.flush().await?;
    Ok(())
}

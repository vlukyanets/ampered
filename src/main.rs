//! Daemon entry point: CLI, logging, config.
//!
//! See `docs/01-architecture.md`. The actors and the command loop are wired
//! up here and nowhere else — every decision belongs to `core::Engine`.

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;

use ampered::config::Config;

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
    let _socket = cli
        .socket
        .clone()
        .unwrap_or_else(|| config.general.socket.clone());

    anyhow::bail!("the daemon is not wired up yet; use --check")
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

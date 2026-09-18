//! `amperedctl` — the command line client.
//!
//! Reference: `docs/11-ipc-cli.md`. Exit codes: 0 ok, 1 daemon error,
//! 2 failed to connect.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use clap::{Parser, Subcommand, ValueEnum};

use ampered::config::parse_duration;
use ampered::ipc::{InhibitWhat, Request, ScreenState, StatusData, Wire};

const DEFAULT_SOCKET: &str = "/run/ampered/ampered.sock";

#[derive(Debug, Parser)]
#[command(name = "amperedctl", version, about = "Talk to the ampered daemon")]
struct Cli {
    /// Path to the daemon socket.
    #[arg(long, default_value = DEFAULT_SOCKET, value_name = "PATH")]
    socket: PathBuf,

    /// Print the raw response instead of the human-readable form.
    #[arg(long, global = true)]
    json: bool,

    #[command(subcommand)]
    command: Cmd,
}

#[derive(Debug, Subcommand)]
enum Cmd {
    /// Show what the daemon is doing.
    Status,
    /// List the configured modes.
    Modes,
    /// Switch to a mode, or back to `auto`.
    Mode { name: String },
    /// Dim the screen now.
    Dim,
    /// Undo a dim.
    Undim,
    /// Turn the screen on or off.
    Screen { state: Screen },
    /// Sleep now.
    Sleep {
        /// Ignore inhibitors.
        #[arg(long)]
        force: bool,
    },
    /// Enter the long-sleep server cycle.
    LongSleep {
        /// Leave the cycle instead.
        #[arg(long)]
        cancel: bool,
    },
    /// Hold off idle or sleep for a while.
    Inhibit {
        what: What,
        #[arg(long, value_name = "TEXT")]
        why: String,
        #[arg(long, default_value = "1h", value_name = "DURATION")]
        ttl: String,
    },
    /// Drop an inhibitor by id.
    Uninhibit { id: u64 },
    /// Re-read the config file.
    Reload,
    /// Follow state changes until interrupted.
    Watch,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum Screen {
    On,
    Off,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum What {
    Idle,
    Sleep,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(&cli) {
        Ok(code) => code,
        Err(err) => {
            eprintln!("amperedctl: {err}");
            ExitCode::from(1)
        }
    }
}

fn run(cli: &Cli) -> Result<ExitCode, String> {
    let request = match &cli.command {
        Cmd::Status => Request::Status,
        Cmd::Modes => Request::Modes,
        Cmd::Mode { name } => Request::Mode { name: name.clone() },
        Cmd::Dim => Request::Dim,
        Cmd::Undim => Request::Undim,
        Cmd::Screen { state } => Request::Screen {
            state: match state {
                Screen::On => ScreenState::On,
                Screen::Off => ScreenState::Off,
            },
        },
        Cmd::Sleep { force } => Request::Sleep { force: *force },
        Cmd::LongSleep { cancel } => Request::LongSleep { cancel: *cancel },
        Cmd::Inhibit { what, why, ttl } => Request::Inhibit {
            what: match what {
                What::Idle => InhibitWhat::Idle,
                What::Sleep => InhibitWhat::Sleep,
            },
            why: why.clone(),
            ttl: ttl_of(ttl)?,
        },
        Cmd::Uninhibit { id } => Request::Uninhibit { id: *id },
        Cmd::Reload => Request::Reload,
        Cmd::Watch => Request::Subscribe,
    };

    let stream = match UnixStream::connect(&cli.socket) {
        Ok(stream) => stream,
        Err(err) => {
            eprintln!(
                "amperedctl: cannot connect to {}: {err}",
                cli.socket.display()
            );
            return Ok(ExitCode::from(2));
        }
    };

    let mut writer = stream.try_clone().map_err(|e| e.to_string())?;
    let mut reader = BufReader::new(stream);

    let mut line = serde_json::to_string(&request).map_err(|e| e.to_string())?;
    line.push('\n');
    writer
        .write_all(line.as_bytes())
        .map_err(|e| e.to_string())?;
    writer.flush().map_err(|e| e.to_string())?;

    let mut response = String::new();
    if reader.read_line(&mut response).map_err(|e| e.to_string())? == 0 {
        return Err("the daemon closed the connection without answering".into());
    }
    let wire: Wire = serde_json::from_str(&response).map_err(|e| e.to_string())?;

    if !wire.ok {
        let message = wire.error.unwrap_or_else(|| "unknown error".into());
        if cli.json {
            println!("{}", response.trim_end());
        } else {
            eprintln!("amperedctl: {message}");
        }
        return Ok(ExitCode::from(1));
    }

    if matches!(cli.command, Cmd::Watch) {
        return follow(reader, cli.json);
    }

    if cli.json {
        println!("{}", response.trim_end());
        return Ok(ExitCode::SUCCESS);
    }

    match (&cli.command, wire.data) {
        (Cmd::Status, Some(data)) => print_status(data),
        (Cmd::Modes, Some(data)) => print_modes(data),
        (Cmd::Inhibit { .. }, Some(data)) => {
            println!("inhibitor {} created", data["id"]);
        }
        _ => println!("ok"),
    }
    Ok(ExitCode::SUCCESS)
}

fn ttl_of(text: &str) -> Result<Duration, String> {
    let ttl = parse_duration(text)?;
    if ttl.is_zero() {
        return Err("--ttl must be greater than zero".into());
    }
    Ok(ttl)
}

fn follow(reader: BufReader<UnixStream>, json: bool) -> Result<ExitCode, String> {
    for line in reader.lines() {
        let line = line.map_err(|e| e.to_string())?;
        if line.trim().is_empty() {
            continue;
        }
        if json {
            println!("{line}");
            continue;
        }
        match serde_json::from_str::<ampered::ipc::StateEvent>(&line) {
            Ok(event) => println!("{}", describe(&event)),
            Err(_) => println!("{line}"),
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn describe(event: &ampered::ipc::StateEvent) -> String {
    use ampered::ipc::StateEvent::*;
    match event {
        State { from, to } => format!("state    {from} -> {to}"),
        Mode { name } => format!("mode     {name}"),
        Power { ac } => format!("power    {}", if *ac { "ac" } else { "battery" }),
        LongSleep { phase, next_wake } => match next_wake {
            Some(at) => format!("server   {phase}, next wake {at}"),
            None => format!("server   {phase}"),
        },
    }
}

fn print_status(data: serde_json::Value) {
    let status: StatusData = match serde_json::from_value(data) {
        Ok(status) => status,
        Err(err) => {
            eprintln!("amperedctl: unexpected status payload: {err}");
            return;
        }
    };

    println!("state      {}", status.state);
    println!("mode       {} ({})", status.mode.name, status.mode.source);

    let power = &status.power;
    let source = if power.ac { "ac" } else { "battery" };
    match power.battery_percent {
        Some(percent) => {
            let low = if power.low { ", low" } else { "" };
            println!("power      {source}, {percent}%{low}");
        }
        None => println!("power      {source}"),
    }

    match (&status.backlight.device, status.backlight.percent) {
        (Some(device), Some(percent)) => println!("backlight  {device}, {percent}%"),
        (Some(device), None) => println!("backlight  {device}"),
        _ => println!("backlight  unavailable"),
    }

    let stages = &status.idle.stages;
    println!(
        "idle       {} ({}), dim {} / screen off {} / sleep {}",
        status.idle.backend,
        if status.idle.connected {
            "connected"
        } else {
            "disconnected"
        },
        stages.dim,
        stages.screen_off,
        stages.sleep
    );

    print!("sleep      {}", status.sleep.method);
    if status.sleep.blocked_by.is_empty() {
        println!();
    } else {
        println!(", blocked by {}", status.sleep.blocked_by.join(", "));
    }

    if status.server.enabled {
        match &status.server.next_wake {
            Some(at) => println!("server     {}, next wake {at}", status.server.phase),
            None => println!("server     {}", status.server.phase),
        }
    }

    for inhibitor in &status.inhibitors {
        let expires = inhibitor.expires.as_deref().unwrap_or("?");
        println!(
            "inhibitor  #{} {} \"{}\" until {expires}",
            inhibitor.id, inhibitor.what, inhibitor.why
        );
    }

    if !status.degraded.is_empty() {
        println!("degraded   {}", status.degraded.join(", "));
    }
}

fn print_modes(data: serde_json::Value) {
    let modes = data["modes"].as_array().cloned().unwrap_or_default();
    let current = data["current"].as_str().unwrap_or("");
    let source = data["source"].as_str().unwrap_or("");
    for mode in modes {
        let name = mode.as_str().unwrap_or_default();
        if name == current {
            println!("* {name} ({source})");
        } else {
            println!("  {name}");
        }
    }
}

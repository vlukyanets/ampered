//! Turning the screen off and on.
//!
//! Reference: `docs/06-display-dpms.md`. The `wlr` backend talks to the
//! compositor through the idle watcher's connection (`idle::IdleHandle`);
//! `command` runs a helper. Like `idle`, this runs in `ampered-agent` as
//! the session user (`docs/03-privileges.md`), so the helper needs no
//! `setuid` — only the compositor's environment.

use std::process::Stdio;
use std::time::Duration;

use tokio::process::Command;
use tracing::{debug, info, warn};

use crate::config::{Display as DisplayConfig, DisplayBackend};
use crate::idle::{Compositor, IdleHandle};

/// A compositor helper that hangs must not hang the daemon with it.
const TIMEOUT: Duration = Duration::from_secs(10);

pub struct Display {
    backend: Backend,
    compositor: Compositor,
    /// Idempotency lives here, not in the FSM (`docs/06-display-dpms.md`).
    /// `None` means "unknown", which is also where a failed command leaves it.
    last: Option<bool>,
}

enum Backend {
    /// The protocol when the compositor has it, the commands when it does not.
    Wlr {
        idle: IdleHandle,
        fallback: Option<(String, String)>,
    },
    Command {
        off: String,
        on: String,
    },
    None,
}

impl Display {
    /// `compositor` says where the compositor is, for the `command` helper.
    pub fn new(display: &DisplayConfig, compositor: &Compositor, idle: IdleHandle) -> Display {
        let backend = match display.backend {
            DisplayBackend::Command => {
                // Validation guarantees both commands are present.
                let off = display.off_command.clone().unwrap_or_default();
                let on = display.on_command.clone().unwrap_or_default();
                info!("display backend: command");
                Backend::Command { off, on }
            }
            DisplayBackend::None => {
                info!("display backend: none");
                Backend::None
            }
            DisplayBackend::Wlr => {
                // Validation guarantees the pair is complete or absent.
                let fallback = display.off_command.clone().zip(display.on_command.clone());
                info!(
                    fallback = fallback.is_some(),
                    "display backend: wlr-output-power-management"
                );
                Backend::Wlr { idle, fallback }
            }
        };
        Display {
            backend,
            compositor: compositor.clone(),
            last: None,
        }
    }

    /// For `wlr` this follows the compositor: gone with the connection,
    /// back with it — unless the commands stand in.
    pub fn is_available(&self) -> bool {
        match &self.backend {
            Backend::Wlr { idle, fallback } => idle.output_power_available() || fallback.is_some(),
            Backend::Command { .. } => true,
            Backend::None => false,
        }
    }

    /// `Screen(true)` is sent on every resume even if we never turned it off,
    /// so the repeat check matters.
    pub async fn set(&mut self, on: bool) {
        if self.last == Some(on) {
            debug!(on, "screen already in that state");
            return;
        }

        let command = match &self.backend {
            Backend::Wlr { idle, fallback } => {
                if idle.output_power_available() {
                    idle.set_screen(on);
                    self.last = Some(on);
                    return;
                }
                match fallback {
                    Some((off, on_cmd)) => {
                        debug!("no output power protocol, using the command fallback");
                        Some(if on { on_cmd.clone() } else { off.clone() })
                    }
                    None => {
                        idle.set_screen(on);
                        self.last = Some(on);
                        return;
                    }
                }
            }
            Backend::None => None,
            Backend::Command { off, on: on_cmd } => {
                let command = if on { on_cmd } else { off };
                (!command.is_empty()).then(|| command.clone())
            }
        };
        let Some(command) = command else {
            self.last = Some(on);
            return;
        };

        // A helper that failed leaves the panel in an unknown state, so the
        // repeat check must not latch: `Screen(true)` on resume and on
        // shutdown has to be free to try again.
        self.last = self.run(&command).await.then_some(on);
    }

    /// Runs the helper with the compositor's environment: `swaymsg` and
    /// friends find their socket through it, and `--runtime-dir`/`--display`
    /// on the agent must reach them too.
    async fn run(&self, command: &str) -> bool {
        let mut child = Command::new("sh");
        child
            .arg("-c")
            .arg(command)
            .env("XDG_RUNTIME_DIR", &self.compositor.runtime_dir)
            .env("WAYLAND_DISPLAY", &self.compositor.display)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        debug!(command, "running display command");
        let output = match tokio::time::timeout(TIMEOUT, child.output()).await {
            Ok(Ok(output)) => output,
            Ok(Err(err)) => {
                warn!(command, %err, "display command failed to start");
                return false;
            }
            Err(_) => {
                warn!(command, "display command timed out");
                return false;
            }
        };

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            warn!(
                command,
                code = output.status.code(),
                stderr = stderr.trim(),
                "display command failed"
            );
            return false;
        }
        true
    }
}

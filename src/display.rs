//! Turning the screen off and on.
//!
//! Reference: `docs/06-display-dpms.md`. The `wlr` backend talks to the
//! compositor through the idle watcher's connection (`idle::IdleHandle`);
//! `command` runs a helper as the session user.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use tokio::process::Command;
use tracing::{debug, info, warn};

use crate::config::{Config, DisplayBackend};
use crate::idle::IdleHandle;

/// A compositor helper that hangs must not hang the daemon with it.
const TIMEOUT: Duration = Duration::from_secs(10);

pub struct Display {
    backend: Backend,
    session: Session,
    /// Idempotency lives here, not in the FSM (`docs/06-display-dpms.md`).
    /// `None` means "unknown", which is also where a failed command leaves it.
    last: Option<bool>,
}

enum Backend {
    Wlr(IdleHandle),
    Command { off: String, on: String },
    None,
}

/// What a helper needs in order to find the compositor it should talk to.
#[derive(Clone, Debug)]
struct Session {
    runtime_dir: PathBuf,
    display: String,
    user: Option<SessionUser>,
}

#[derive(Clone, Debug)]
struct SessionUser {
    uid: u32,
    gid: u32,
    home: PathBuf,
}

impl Display {
    pub fn from_config(config: &Config, idle: IdleHandle) -> Display {
        let backend = match config.display.backend {
            DisplayBackend::Command => {
                // Validation guarantees both commands are present.
                let off = config.display.off_command.clone().unwrap_or_default();
                let on = config.display.on_command.clone().unwrap_or_default();
                info!("display backend: command");
                Backend::Command { off, on }
            }
            DisplayBackend::None => {
                info!("display backend: none");
                Backend::None
            }
            DisplayBackend::Wlr => {
                info!("display backend: wlr-output-power-management");
                Backend::Wlr(idle)
            }
        };
        Display {
            backend,
            session: Session::discover(&config.wayland.runtime_dir, &config.wayland.display),
            last: None,
        }
    }

    /// For `wlr` this follows the compositor: gone with the connection,
    /// back with it.
    pub fn is_available(&self) -> bool {
        match &self.backend {
            Backend::Wlr(idle) => idle.output_power_available(),
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
            Backend::Wlr(idle) => {
                idle.set_screen(on);
                self.last = Some(on);
                return;
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
        self.last = self.session.run(&command).await.then_some(on);
    }
}

impl Session {
    fn discover(runtime_dir: &Path, display: &str) -> Session {
        Session {
            runtime_dir: runtime_dir.to_path_buf(),
            display: display.to_string(),
            user: SessionUser::owning(runtime_dir),
        }
    }

    /// Runs the helper as the owner of `runtime_dir`: `swaymsg` and friends
    /// need that uid to reach the compositor's socket (`docs/03-privileges.md`).
    async fn run(&self, command: &str) -> bool {
        let mut child = Command::new("sh");
        child
            .arg("-c")
            .arg(command)
            .env("XDG_RUNTIME_DIR", &self.runtime_dir)
            .env("WAYLAND_DISPLAY", &self.display)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        if let Some(user) = &self.user {
            // Only root can change uid; as a normal user we are already the
            // one who owns the session, or we would not have got this far.
            if nix::unistd::Uid::effective().is_root() {
                child.uid(user.uid).gid(user.gid);
            }
            child.env("HOME", &user.home);
        }

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

impl SessionUser {
    fn owning(runtime_dir: &Path) -> Option<SessionUser> {
        use std::os::unix::fs::MetadataExt;

        let uid = match std::fs::metadata(runtime_dir) {
            Ok(meta) => meta.uid(),
            Err(err) => {
                warn!(dir = %runtime_dir.display(), %err, "cannot stat the runtime directory");
                return None;
            }
        };
        match nix::unistd::User::from_uid(nix::unistd::Uid::from_raw(uid)) {
            Ok(Some(user)) => Some(SessionUser {
                uid,
                gid: user.gid.as_raw(),
                home: user.dir,
            }),
            Ok(None) => {
                warn!(uid, "the runtime directory belongs to an unknown user");
                None
            }
            Err(err) => {
                warn!(uid, %err, "cannot look up the session user");
                None
            }
        }
    }
}

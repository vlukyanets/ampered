//! The daemon's end of the `ampered-agent` stream, and the types both ends
//! speak.
//!
//! Reference: `docs/03-privileges.md`, `docs/11-ipc-cli.md` ("The agent
//! stream"), ADR-16, ADR-17. `AgentLink` is how the daemon reaches `idle`
//! and `display`, which only ever run in the agent, as the user.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tracing::{debug, info};

use crate::config::{Display as DisplayConfig, format_duration, parse_duration};
use crate::core::{Stage, Stages};

/// Daemon → agent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum AgentOp {
    /// The `[display]` section, on registration and on reload.
    Display(DisplayConfig),
    /// `Command::ReplaceIdleStages`; `"0"` is a disabled stage.
    Stages {
        dim: String,
        screen_off: String,
        sleep: String,
    },
    /// `Command::Screen`.
    Screen { on: bool },
}

impl AgentOp {
    pub fn stages(stages: Stages) -> AgentOp {
        let text = |d: Option<Duration>| format_duration(d.unwrap_or(Duration::ZERO));
        AgentOp::Stages {
            dim: text(stages.dim),
            screen_off: text(stages.screen_off),
            sleep: text(stages.sleep),
        }
    }
}

/// `"0"` and anything unparsable both mean "disabled".
pub fn parse_stages(dim: &str, screen_off: &str, sleep: &str) -> Stages {
    let stage = |text: &str| parse_duration(text).ok().filter(|d| !d.is_zero());
    Stages {
        dim: stage(dim),
        screen_off: stage(screen_off),
        sleep: stage(sleep),
    }
}

/// Agent → daemon. No replies.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum AgentEvent {
    Idle {
        stage: Stage,
    },
    Activity,
    /// The agent's idle backend and whether it is connected.
    Backend {
        name: String,
        connected: bool,
    },
    /// Whether the agent can turn the screen off right now.
    Display {
        available: bool,
    },
}

/// Which registration a stream belongs to, so a replaced stream does not
/// unregister its successor.
pub type AgentId = u64;

#[derive(Debug)]
struct AgentState {
    stream: Option<(AgentId, mpsc::UnboundedSender<AgentOp>)>,
    next_id: AgentId,
    /// Re-sent to a new agent, so a restart on either side loses nothing.
    display: DisplayConfig,
    stages: Stages,
    backend: String,
    backend_connected: bool,
    display_available: bool,
}

/// Shared between `main` (commands in) and the IPC server (the stream).
#[derive(Clone, Debug)]
pub struct AgentLink {
    state: Arc<Mutex<AgentState>>,
}

impl AgentLink {
    pub fn new(display: &DisplayConfig) -> AgentLink {
        AgentLink {
            state: Arc::new(Mutex::new(AgentState {
                stream: None,
                next_id: 1,
                display: display.clone(),
                stages: Stages::NONE,
                backend: "agent".to_string(),
                backend_connected: false,
                display_available: false,
            })),
        }
    }

    // ------------------------------------------------------- the daemon side

    pub fn replace_stages(&self, stages: Stages) {
        let mut state = crate::locked(&self.state);
        state.stages = stages;
        send(&state, AgentOp::stages(stages));
    }

    pub fn set_screen(&self, on: bool) {
        send(&crate::locked(&self.state), AgentOp::Screen { on });
    }

    /// A reloaded `[display]` section.
    pub fn set_display(&self, display: &DisplayConfig) {
        let mut state = crate::locked(&self.state);
        if state.display == *display {
            return;
        }
        state.display = display.clone();
        send(&state, AgentOp::Display(display.clone()));
    }

    pub fn is_connected(&self) -> bool {
        crate::locked(&self.state).stream.is_some()
    }

    /// The idle backend the agent reports, for `status.idle.backend`.
    pub fn backend(&self) -> String {
        crate::locked(&self.state).backend.clone()
    }

    pub fn display_available(&self) -> bool {
        let state = crate::locked(&self.state);
        state.stream.is_some() && state.display_available
    }

    // ------------------------------------------------------- the stream side

    /// A new agent takes over; the previous stream, if any, is closed by
    /// dropping its sender. The current `[display]` and stages go out first.
    pub fn register(&self, uid: Option<u32>) -> (AgentId, mpsc::UnboundedReceiver<AgentOp>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let mut state = crate::locked(&self.state);
        let id = state.next_id;
        state.next_id += 1;
        if state.stream.is_some() {
            info!("a new agent replaces the previous one");
        }
        state.stream = Some((id, tx));
        state.backend_connected = false;
        state.display_available = false;
        info!(id, ?uid, "agent registered");
        send(&state, AgentOp::Display(state.display.clone()));
        send(&state, AgentOp::stages(state.stages));
        (id, rx)
    }

    /// `true` if that stream was the current one — its loss then counts.
    pub fn unregister(&self, id: AgentId) -> bool {
        let mut state = crate::locked(&self.state);
        if state
            .stream
            .as_ref()
            .is_some_and(|(current, _)| *current == id)
        {
            state.stream = None;
            state.backend_connected = false;
            state.display_available = false;
            info!(id, "agent gone");
            return true;
        }
        false
    }

    /// Keeps the status fields in step with what the agent says.
    pub fn note(&self, id: AgentId, event: &AgentEvent) {
        let mut state = crate::locked(&self.state);
        if !state
            .stream
            .as_ref()
            .is_some_and(|(current, _)| *current == id)
        {
            return;
        }
        match event {
            AgentEvent::Backend { name, connected } => {
                state.backend = name.clone();
                state.backend_connected = *connected;
            }
            AgentEvent::Display { available } => state.display_available = *available,
            AgentEvent::Idle { .. } | AgentEvent::Activity => {}
        }
    }
}

fn send(state: &AgentState, op: AgentOp) {
    match &state.stream {
        Some((_, tx)) => {
            let _ = tx.send(op);
        }
        None => debug!(?op, "no agent to send to"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::DisplayBackend;

    #[test]
    fn ops_and_events_on_the_wire() {
        let stages = Stages {
            dim: Some(Duration::from_secs(300)),
            screen_off: None,
            sleep: Some(Duration::from_secs(1800)),
        };
        let line = serde_json::to_string(&AgentOp::stages(stages)).unwrap();
        assert_eq!(
            line,
            r#"{"op":"stages","dim":"5m","screen_off":"0","sleep":"30m"}"#
        );
        let AgentOp::Stages {
            dim,
            screen_off,
            sleep,
        } = serde_json::from_str(&line).unwrap()
        else {
            panic!("stages");
        };
        assert_eq!(parse_stages(&dim, &screen_off, &sleep), stages);

        assert_eq!(
            serde_json::to_string(&AgentOp::Screen { on: false }).unwrap(),
            r#"{"op":"screen","on":false}"#
        );
        let display: AgentOp = serde_json::from_str(
            r#"{"op":"display","backend":"command","off_command":"x","on_command":"y"}"#,
        )
        .unwrap();
        assert_eq!(
            display,
            AgentOp::Display(DisplayConfig {
                backend: DisplayBackend::Command,
                off_command: Some("x".into()),
                on_command: Some("y".into()),
            })
        );

        let event: AgentEvent =
            serde_json::from_str(r#"{"event":"idle","stage":"screen_off"}"#).unwrap();
        assert_eq!(
            event,
            AgentEvent::Idle {
                stage: Stage::ScreenOff
            }
        );
        assert_eq!(
            serde_json::to_string(&AgentEvent::Backend {
                name: "ext-idle-notify".into(),
                connected: true
            })
            .unwrap(),
            r#"{"event":"backend","name":"ext-idle-notify","connected":true}"#
        );
    }

    #[test]
    fn registration_replays_display_and_stages() {
        let link = AgentLink::new(&DisplayConfig::default());
        assert!(!link.is_connected());
        let stages = Stages {
            dim: Some(Duration::from_secs(60)),
            ..Stages::NONE
        };
        // Nothing to send to yet; the cache remembers.
        link.replace_stages(stages);

        let (id, mut rx) = link.register(Some(1000));
        assert!(link.is_connected());
        assert_eq!(
            rx.try_recv().unwrap(),
            AgentOp::Display(DisplayConfig::default())
        );
        assert_eq!(rx.try_recv().unwrap(), AgentOp::stages(stages));

        link.set_screen(false);
        assert_eq!(rx.try_recv().unwrap(), AgentOp::Screen { on: false });

        link.note(
            id,
            &AgentEvent::Backend {
                name: "ext-idle-notify".into(),
                connected: true,
            },
        );
        link.note(id, &AgentEvent::Display { available: true });
        assert_eq!(link.backend(), "ext-idle-notify");
        assert!(link.display_available());

        // A reload with the same section sends nothing.
        link.set_display(&DisplayConfig::default());
        assert!(rx.try_recv().is_err());

        assert!(link.unregister(id));
        assert!(!link.is_connected());
        assert!(!link.display_available());
    }

    #[test]
    fn a_new_agent_replaces_the_old_stream() {
        let link = AgentLink::new(&DisplayConfig::default());
        let (first, mut first_rx) = link.register(None);
        let (second, mut second_rx) = link.register(None);
        // The first sender was dropped: its receiver ends after the replay.
        first_rx.try_recv().unwrap();
        first_rx.try_recv().unwrap();
        assert!(first_rx.try_recv().is_err());
        assert!(first_rx.is_closed());

        // The old stream going away does not count against the new one.
        assert!(!link.unregister(first));
        assert!(link.is_connected());
        link.note(first, &AgentEvent::Display { available: true });
        assert!(!link.display_available());

        link.set_screen(true);
        second_rx.try_recv().unwrap();
        second_rx.try_recv().unwrap();
        assert_eq!(second_rx.try_recv().unwrap(), AgentOp::Screen { on: true });
        assert!(link.unregister(second));
    }
}

//! IPC protocol types: NDJSON over a Unix socket.
//!
//! Reference: `docs/11-ipc-cli.md`. The server itself lives further down this
//! module; `amperedctl` speaks the same types from the other side.

use std::collections::HashMap;
use std::fmt;
use std::io;
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use nix::unistd::Group;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{broadcast, mpsc, oneshot};
use tracing::{debug, error, info, warn};

use crate::agent::{AgentEvent, AgentLink};
use crate::core::Event;

/// Identifies an in-flight request so `Command::Reply` can find its connection.
pub type RequestId = u64;

/// A forgotten inhibitor must not live forever (`docs/11-ipc-cli.md`).
pub const MAX_INHIBIT_TTL: Duration = Duration::from_secs(24 * 60 * 60);

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "kebab-case")]
pub enum Request {
    Status,
    Modes,
    Mode {
        name: String,
    },
    Dim,
    Undim,
    Screen {
        state: ScreenState,
    },
    Sleep {
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        force: bool,
    },
    LongSleep {
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        cancel: bool,
    },
    Inhibit {
        what: InhibitWhat,
        why: String,
        #[serde(with = "crate::config::duration_str")]
        ttl: Duration,
    },
    Uninhibit {
        id: u64,
    },
    Reload,
    Subscribe,
    /// `ampered-agent` registering (`docs/11-ipc-cli.md`, "The agent stream").
    Agent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ScreenState {
    On,
    Off,
}

impl ScreenState {
    pub fn is_on(self) -> bool {
        matches!(self, ScreenState::On)
    }
}

/// `idle` ignores every idle stage, `sleep` blocks sleep only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum InhibitWhat {
    Idle,
    Sleep,
}

impl fmt::Display for InhibitWhat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            InhibitWhat::Idle => "idle",
            InhibitWhat::Sleep => "sleep",
        })
    }
}

/// What the engine decided to send back; serialized as `{"ok":…}`.
#[derive(Debug, Clone, PartialEq)]
pub enum Response {
    Ok,
    Status(Box<StatusData>),
    Modes(ModesData),
    Inhibited { id: u64 },
    Error(String),
}

impl Response {
    pub fn error(msg: impl Into<String>) -> Response {
        Response::Error(msg.into())
    }

    pub fn is_ok(&self) -> bool {
        !matches!(self, Response::Error(_))
    }

    /// The wire form: `{"ok":true,"data":{…}}` or `{"ok":false,"error":"…"}`.
    pub fn to_wire(&self) -> Wire {
        match self {
            Response::Ok => Wire {
                ok: true,
                data: None,
                error: None,
            },
            Response::Status(s) => Wire::data(serde_json::to_value(s).unwrap_or_default()),
            Response::Modes(m) => Wire::data(serde_json::to_value(m).unwrap_or_default()),
            Response::Inhibited { id } => Wire::data(serde_json::json!({ "id": id })),
            Response::Error(e) => Wire {
                ok: false,
                data: None,
                error: Some(e.clone()),
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Wire {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl Wire {
    fn data(value: serde_json::Value) -> Wire {
        Wire {
            ok: true,
            data: Some(value),
            error: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModesData {
    pub modes: Vec<String>,
    pub current: String,
    /// `"auto"` or `"manual"`.
    pub source: String,
}

/// `amperedctl status`. The engine fills in what it decides; `main` merges in
/// the parts owned by the actors (backlight, idle backend, `degraded`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StatusData {
    pub state: String,
    pub mode: ModeInfo,
    pub power: PowerInfo,
    pub backlight: BacklightInfo,
    pub idle: IdleInfo,
    pub sleep: SleepInfo,
    pub server: ServerInfo,
    pub inhibitors: Vec<InhibitorInfo>,
    pub degraded: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModeInfo {
    pub name: String,
    pub source: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PowerInfo {
    pub ac: bool,
    pub battery_percent: Option<u8>,
    pub low: bool,
    pub batteries: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct BacklightInfo {
    pub device: Option<String>,
    pub percent: Option<u8>,
    pub pre_dim: Option<u8>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IdleInfo {
    pub backend: String,
    pub connected: bool,
    pub stages: StagesInfo,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StagesInfo {
    pub dim: String,
    pub screen_off: String,
    pub sleep: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SleepInfo {
    pub method: String,
    pub blocked_by: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ServerInfo {
    pub enabled: bool,
    pub phase: String,
    pub next_wake: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InhibitorInfo {
    pub id: u64,
    pub what: String,
    pub why: String,
    /// RFC 3339; filled in by `main`, which owns the expiry timers.
    pub expires: Option<String>,
}

/// One line of an `amperedctl watch` stream.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "kebab-case")]
pub enum StateEvent {
    State {
        from: String,
        to: String,
    },
    Mode {
        name: String,
    },
    Power {
        ac: bool,
    },
    LongSleep {
        phase: String,
        next_wake: Option<String>,
    },
}

// ------------------------------------------------------------------ server

/// Hands replies and broadcasts back to the connections waiting for them.
#[derive(Clone)]
pub struct Server {
    pending: Arc<Mutex<HashMap<RequestId, oneshot::Sender<Response>>>>,
    broadcast: broadcast::Sender<StateEvent>,
    next_id: Arc<AtomicU64>,
}

impl Server {
    /// Answers the request `Command::Reply` names. A vanished client is not an
    /// error — it simply hung up before we got there.
    pub fn reply(&self, id: RequestId, response: Response) {
        match crate::locked(&self.pending).remove(&id) {
            Some(channel) => {
                let _ = channel.send(response);
            }
            None => debug!(id, "reply for a request nobody is waiting for"),
        }
    }

    /// Feeds one line to every `subscribe` connection.
    pub fn broadcast(&self, event: StateEvent) {
        // `Err` only means there are no subscribers right now.
        let _ = self.broadcast.send(event);
    }

    fn take_id(&self) -> RequestId {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }
}

/// Binds the socket and starts accepting connections.
///
/// Permissions are `0660 root:<socket_group>` (`docs/11-ipc-cli.md`); an
/// unknown group leaves the socket owned by our own group and warns, which
/// degrades access rather than the daemon.
pub async fn listen(
    path: &Path,
    group: &str,
    events: mpsc::Sender<Event>,
    agent: Option<AgentLink>,
) -> io::Result<Server> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // A socket left behind by a killed daemon would make bind() fail.
    match std::fs::metadata(path) {
        Ok(meta) if meta.file_type().is_socket() => std::fs::remove_file(path)?,
        _ => {}
    }

    let listener = UnixListener::bind(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o660))?;
    // Not being able to hand the socket to the group is a degradation (only
    // root can chown), never a reason to refuse to start — that rule is
    // reserved for an invalid config (`CLAUDE.md`, rule 3).
    match Group::from_name(group) {
        Ok(Some(found)) => {
            if let Err(err) = nix::unistd::chown(path, None, Some(found.gid)) {
                warn!(group, %err, "cannot set the socket group; only our own group can talk to it");
            }
        }
        Ok(None) => warn!(group, "no such group, leaving the socket group as is"),
        Err(err) => warn!(group, %err, "cannot look up the socket group"),
    }
    info!(socket = %path.display(), "listening");

    let (broadcast_tx, _) = broadcast::channel(64);
    let server = Server {
        pending: Arc::new(Mutex::new(HashMap::new())),
        broadcast: broadcast_tx,
        next_id: Arc::new(AtomicU64::new(1)),
    };

    let accepting = server.clone();
    tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((stream, _)) => {
                    let server = accepting.clone();
                    let events = events.clone();
                    let agent = agent.clone();
                    tokio::spawn(async move {
                        if let Err(err) = serve_connection(stream, server, events, agent).await {
                            debug!(%err, "ipc connection closed");
                        }
                    });
                }
                Err(err) => {
                    error!(%err, "ipc accept failed");
                    return;
                }
            }
        }
    });

    Ok(server)
}

async fn serve_connection(
    stream: UnixStream,
    server: Server,
    events: mpsc::Sender<Event>,
    agent: Option<AgentLink>,
) -> io::Result<()> {
    let uid = stream.peer_cred().ok().map(|cred| cred.uid());
    let (read_half, mut write_half) = stream.into_split();
    let mut lines = BufReader::new(read_half).lines();

    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let request: Request = match serde_json::from_str(&line) {
            Ok(request) => request,
            Err(err) => {
                write_line(
                    &mut write_half,
                    &Response::error(format!("bad request: {err}")),
                )
                .await?;
                continue;
            }
        };

        if matches!(request, Request::Subscribe) {
            write_line(&mut write_half, &Response::Ok).await?;
            return stream_events(lines, write_half, server.broadcast.subscribe()).await;
        }
        if matches!(request, Request::Agent) {
            let Some(link) = agent else {
                write_line(
                    &mut write_half,
                    &Response::error("[general] privilege is not \"split\""),
                )
                .await?;
                return Ok(());
            };
            write_line(&mut write_half, &Response::Ok).await?;
            return serve_agent(lines, write_half, link, uid, events).await;
        }

        let id = server.take_id();
        let (reply_tx, reply_rx) = oneshot::channel();
        crate::locked(&server.pending).insert(id, reply_tx);

        if events.send(Event::Ipc(id, request)).await.is_err() {
            crate::locked(&server.pending).remove(&id);
            write_line(&mut write_half, &Response::error("daemon is shutting down")).await?;
            return Ok(());
        }

        let response = reply_rx
            .await
            .unwrap_or_else(|_| Response::error("the daemon dropped the request"));
        write_line(&mut write_half, &response).await?;
    }
    Ok(())
}

/// `subscribe`: one event per line until the client goes away.
async fn stream_events(
    mut lines: tokio::io::Lines<BufReader<tokio::net::unix::OwnedReadHalf>>,
    mut write_half: tokio::net::unix::OwnedWriteHalf,
    mut updates: broadcast::Receiver<StateEvent>,
) -> io::Result<()> {
    loop {
        tokio::select! {
            // The client is not expected to say anything else; this arm is
            // here to notice the disconnect.
            line = lines.next_line() => match line {
                Ok(Some(_)) => continue,
                _ => return Ok(()),
            },
            update = updates.recv() => match update {
                Ok(event) => {
                    let mut line = serde_json::to_string(&event).unwrap_or_default();
                    line.push('\n');
                    write_half.write_all(line.as_bytes()).await?;
                    write_half.flush().await?;
                }
                Err(broadcast::error::RecvError::Lagged(missed)) => {
                    warn!(missed, "subscriber fell behind");
                }
                Err(broadcast::error::RecvError::Closed) => return Ok(()),
            },
        }
    }
}

/// The agent stream: ops out, events in, until either side hangs up
/// (`docs/11-ipc-cli.md`). Losing the current agent is the compositor
/// going away as far as the engine is concerned.
async fn serve_agent(
    mut lines: tokio::io::Lines<BufReader<tokio::net::unix::OwnedReadHalf>>,
    mut write_half: tokio::net::unix::OwnedWriteHalf,
    link: AgentLink,
    uid: Option<u32>,
    events: mpsc::Sender<Event>,
) -> io::Result<()> {
    let (id, mut ops) = link.register(uid);
    let result = loop {
        tokio::select! {
            line = lines.next_line() => match line {
                Ok(Some(line)) => {
                    if line.trim().is_empty() {
                        continue;
                    }
                    let event: AgentEvent = match serde_json::from_str(&line) {
                        Ok(event) => event,
                        Err(err) => {
                            warn!(%err, line, "bad line from the agent");
                            continue;
                        }
                    };
                    link.note(id, &event);
                    let event = match event {
                        AgentEvent::Idle { stage } => Some(Event::Idle(stage)),
                        AgentEvent::Activity => Some(Event::Activity),
                        AgentEvent::Backend { connected, .. } => {
                            Some(Event::IdleBackendChanged(connected))
                        }
                        AgentEvent::Display { .. } => None,
                    };
                    if let Some(event) = event
                        && events.send(event).await.is_err()
                    {
                        break Ok(());
                    }
                }
                Ok(None) => break Ok(()),
                Err(err) => break Err(err),
            },
            op = ops.recv() => match op {
                Some(op) => {
                    let mut line = serde_json::to_string(&op).unwrap_or_default();
                    line.push('\n');
                    if let Err(err) = write_half.write_all(line.as_bytes()).await {
                        break Err(err);
                    }
                    if let Err(err) = write_half.flush().await {
                        break Err(err);
                    }
                }
                // Replaced by a newer agent.
                None => break Ok(()),
            },
        }
    };
    if link.unregister(id) {
        let _ = events.send(Event::IdleBackendChanged(false)).await;
        let _ = events.send(Event::Activity).await;
    }
    result
}

async fn write_line(
    write_half: &mut tokio::net::unix::OwnedWriteHalf,
    response: &Response,
) -> io::Result<()> {
    let mut line = serde_json::to_string(&response.to_wire()).unwrap_or_default();
    line.push('\n');
    write_half.write_all(line.as_bytes()).await?;
    write_half.flush().await
}

#[cfg(test)]
mod tests {
    //! The server on a real socket in a tempdir; the group lookup warns as a
    //! non-root user and that is fine.

    use super::*;
    use crate::agent::AgentOp;
    use crate::config::Display as DisplayConfig;
    use crate::core::{Stage, Stages};
    use tokio::io::{AsyncBufReadExt, BufReader};

    async fn client(
        path: &Path,
    ) -> (
        tokio::io::Lines<BufReader<tokio::net::unix::OwnedReadHalf>>,
        tokio::net::unix::OwnedWriteHalf,
    ) {
        let stream = UnixStream::connect(path).await.unwrap();
        let (read, write) = stream.into_split();
        (BufReader::new(read).lines(), write)
    }

    async fn send(write: &mut tokio::net::unix::OwnedWriteHalf, line: &str) {
        write
            .write_all(format!("{line}\n").as_bytes())
            .await
            .unwrap();
        write.flush().await.unwrap();
    }

    #[tokio::test]
    async fn request_reply_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ampered.sock");
        let (events_tx, mut events_rx) = mpsc::channel(8);
        let server = listen(&path, "", events_tx, None).await.unwrap();

        let (mut lines, mut write) = client(&path).await;
        send(&mut write, r#"{"cmd":"status"}"#).await;
        let Some(Event::Ipc(id, Request::Status)) = events_rx.recv().await else {
            panic!("expected a status request");
        };
        server.reply(id, Response::Ok);
        assert_eq!(lines.next_line().await.unwrap().unwrap(), r#"{"ok":true}"#);

        send(&mut write, "not json").await;
        let line = lines.next_line().await.unwrap().unwrap();
        assert!(line.contains("bad request"), "{line}");

        // No split mode: the agent is turned away.
        send(&mut write, r#"{"cmd":"agent"}"#).await;
        let line = lines.next_line().await.unwrap().unwrap();
        assert!(line.contains("privilege"), "{line}");
    }

    #[tokio::test]
    async fn the_agent_stream() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ampered.sock");
        let (events_tx, mut events_rx) = mpsc::channel(8);
        let link = AgentLink::new(&DisplayConfig::default());
        let _server = listen(&path, "", events_tx, Some(link.clone()))
            .await
            .unwrap();
        let stages = Stages {
            dim: Some(Duration::from_secs(120)),
            ..Stages::NONE
        };
        link.replace_stages(stages);

        let (mut lines, mut write) = client(&path).await;
        send(&mut write, r#"{"cmd":"agent"}"#).await;
        assert_eq!(lines.next_line().await.unwrap().unwrap(), r#"{"ok":true}"#);
        // The replay: display, then the stages cached before the agent came.
        let display: AgentOp =
            serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
        assert_eq!(display, AgentOp::Display(DisplayConfig::default()));
        let replayed: AgentOp =
            serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
        assert_eq!(replayed, AgentOp::stages(stages));

        // Events go up; the status side notes them.
        send(
            &mut write,
            r#"{"event":"backend","name":"ext-idle-notify","connected":true}"#,
        )
        .await;
        assert_eq!(
            events_rx.recv().await,
            Some(Event::IdleBackendChanged(true))
        );
        send(&mut write, r#"{"event":"display","available":true}"#).await;
        send(&mut write, r#"{"event":"idle","stage":"dim"}"#).await;
        assert_eq!(events_rx.recv().await, Some(Event::Idle(Stage::Dim)));
        assert!(link.is_connected());
        assert!(link.display_available());
        assert_eq!(link.backend(), "ext-idle-notify");

        // Commands go down.
        link.set_screen(false);
        let op: AgentOp = serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
        assert_eq!(op, AgentOp::Screen { on: false });

        // A bad line is skipped, the stream lives on.
        send(&mut write, "garbage").await;
        send(&mut write, r#"{"event":"activity"}"#).await;
        assert_eq!(events_rx.recv().await, Some(Event::Activity));

        // The agent hangs up: the compositor is gone as far as the engine knows.
        drop(write);
        drop(lines);
        assert_eq!(
            events_rx.recv().await,
            Some(Event::IdleBackendChanged(false))
        );
        assert_eq!(events_rx.recv().await, Some(Event::Activity));
        assert!(!link.is_connected());
    }

    #[tokio::test]
    async fn a_second_agent_takes_over() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ampered.sock");
        let (events_tx, mut events_rx) = mpsc::channel(8);
        let link = AgentLink::new(&DisplayConfig::default());
        let _server = listen(&path, "", events_tx, Some(link.clone()))
            .await
            .unwrap();

        let (mut first_lines, mut first_write) = client(&path).await;
        send(&mut first_write, r#"{"cmd":"agent"}"#).await;
        for _ in 0..3 {
            first_lines.next_line().await.unwrap().unwrap();
        }

        let (mut second_lines, mut second_write) = client(&path).await;
        send(&mut second_write, r#"{"cmd":"agent"}"#).await;
        for _ in 0..3 {
            second_lines.next_line().await.unwrap().unwrap();
        }
        // The first stream is closed by the daemon, with no event: the new
        // agent is in charge.
        assert_eq!(first_lines.next_line().await.unwrap(), None);
        assert!(link.is_connected());

        link.set_screen(true);
        let op: AgentOp =
            serde_json::from_str(&second_lines.next_line().await.unwrap().unwrap()).unwrap();
        assert_eq!(op, AgentOp::Screen { on: true });
        send(&mut second_write, r#"{"event":"activity"}"#).await;
        assert_eq!(events_rx.recv().await, Some(Event::Activity));
    }
}

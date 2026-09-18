//! IPC protocol types: NDJSON over a Unix socket.
//!
//! Reference: `docs/11-ipc-cli.md`. The server itself lives further down this
//! module; `amperedctl` speaks the same types from the other side.

use std::fmt;
use std::time::Duration;

use serde::{Deserialize, Serialize};

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

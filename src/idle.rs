//! Idle notifications from the compositor via `ext-idle-notify-v1`.
//!
//! Reference: `docs/04-idle-wayland.md`. We keep no idle timers of our own
//! (ADR-3): one notification per stage, the compositor does the counting and
//! honours `idle-inhibit-unstable-v1` for us.

use std::os::fd::{AsFd, AsRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::time::Duration;

use tokio::io::Interest;
use tokio::io::unix::AsyncFd;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};
use wayland_client::protocol::{wl_registry, wl_seat};
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle};
use wayland_protocols::ext::idle_notify::v1::client::ext_idle_notification_v1::{
    self, ExtIdleNotificationV1,
};
use wayland_protocols::ext::idle_notify::v1::client::ext_idle_notifier_v1::ExtIdleNotifierV1;

use crate::config::Wayland as WaylandConfig;
use crate::core::{Event, Stage, Stages};

pub const BACKEND: &str = "ext-idle-notify";

/// Where `main` sends `Command::ReplaceIdleStages`.
#[derive(Clone)]
pub struct IdleHandle {
    stages: mpsc::UnboundedSender<Stages>,
}

impl IdleHandle {
    pub fn replace_stages(&self, stages: Stages) {
        let _ = self.stages.send(stages);
    }
}

/// Connects to the compositor and keeps reconnecting for the daemon's lifetime.
pub fn spawn(config: WaylandConfig, events: mpsc::Sender<Event>) -> IdleHandle {
    let (stages_tx, stages_rx) = mpsc::unbounded_channel();
    tokio::spawn(reconnect_loop(config, events, stages_rx));
    IdleHandle { stages: stages_tx }
}

async fn reconnect_loop(
    config: WaylandConfig,
    events: mpsc::Sender<Event>,
    mut stages_rx: mpsc::UnboundedReceiver<Stages>,
) {
    let socket = config.runtime_dir.join(&config.display);
    let mut backoff = Duration::from_secs(1);
    // Survives reconnects: the compositor gets the stages back as they were.
    let mut stages = Stages::NONE;

    loop {
        while let Ok(update) = stages_rx.try_recv() {
            stages = update;
        }

        // True only if the backend had been announced as available, which is
        // the case for every way of losing a live connection — a clean
        // disconnect and a protocol error alike.
        if session(&socket, &events, &mut stages_rx, &mut stages).await {
            info!("compositor connection lost");
            let _ = events.send(Event::IdleBackendChanged(false)).await;
            // The stages are gone with it; do not leave the FSM waiting for a
            // `resumed` that can no longer arrive.
            let _ = events.send(Event::Activity).await;
            backoff = Duration::from_secs(1);
        }

        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(config.reconnect_max_backoff);
    }
}

/// One connection, from the first roundtrip until the socket dies.
///
/// Returns whether the idle backend had been announced as available — that is,
/// whether the caller has a live connection to mourn. Errors are logged here so
/// that every way out of the loop, clean or not, goes through the same answer.
async fn session(
    socket: &std::path::Path,
    events: &mpsc::Sender<Event>,
    stages_rx: &mut mpsc::UnboundedReceiver<Stages>,
    stages: &mut Stages,
) -> bool {
    // The system unit does not see `graphical-session.target`, so the socket
    // path is configured explicitly (`docs/03-privileges.md`).
    let connection = match UnixStream::connect(socket)
        .map_err(|err| err.to_string())
        .and_then(|stream| Connection::from_socket(stream).map_err(|err| err.to_string()))
    {
        Ok(connection) => connection,
        Err(err) => {
            debug!(socket = %socket.display(), err, "no compositor yet");
            return false;
        }
    };
    let mut queue = connection.new_event_queue();
    let handle = queue.handle();
    connection.display().get_registry(&handle, ());

    let mut watcher = Watcher {
        wanted: *stages,
        ..Watcher::default()
    };
    if let Err(err) = queue.roundtrip(&mut watcher) {
        debug!(%err, "wayland handshake failed");
        return false;
    }

    if watcher.notifier.is_none() {
        // The global may still show up later; keep the connection and wait.
        warn!("no ext_idle_notifier_v1; idle is unavailable on this compositor");
        let _ = events.send(Event::IdleBackendChanged(false)).await;
    }

    let async_fd =
        match AsyncFd::with_interest(FdOf(connection.as_fd().as_raw_fd()), Interest::READABLE) {
            Ok(async_fd) => async_fd,
            Err(err) => {
                warn!(%err, "cannot watch the wayland socket");
                return watcher.announced;
            }
        };

    loop {
        if let Err(err) = queue.dispatch_pending(&mut watcher) {
            debug!(%err, "wayland dispatch failed");
            return watcher.announced;
        }
        for event in watcher.outgoing.drain(..) {
            if events.send(event).await.is_err() {
                return watcher.announced;
            }
        }
        if let Err(err) = connection.flush() {
            debug!(%err, "wayland flush failed");
            return watcher.announced;
        }

        let Some(guard) = connection.prepare_read() else {
            // More events arrived while we were dispatching.
            continue;
        };

        tokio::select! {
            update = stages_rx.recv() => {
                drop(guard);
                match update {
                    Some(update) => {
                        *stages = update;
                        watcher.set_stages(&handle, update);
                    }
                    // The daemon is going away.
                    None => return watcher.announced,
                }
            }
            ready = async_fd.readable() => {
                let mut ready = match ready {
                    Ok(ready) => ready,
                    Err(err) => {
                        warn!(%err, "cannot wait on the wayland socket");
                        return watcher.announced;
                    }
                };
                match guard.read() {
                    // Readiness stays set on purpose: tokio's epoll is
                    // edge-triggered, and clearing it with bytes still in the
                    // socket would park us with unread events. The next
                    // iteration reads again until WouldBlock.
                    Ok(_) => {}
                    Err(wayland_client::backend::WaylandError::Io(err))
                        if err.kind() == std::io::ErrorKind::WouldBlock =>
                    {
                        ready.clear_ready();
                    }
                    // Anything else means the compositor is gone.
                    Err(err) => {
                        debug!(%err, "wayland read failed");
                        return watcher.announced;
                    }
                }
            }
        }
    }
}

/// `AsyncFd` needs an owner; the connection keeps the descriptor alive.
struct FdOf(RawFd);

impl AsRawFd for FdOf {
    fn as_raw_fd(&self) -> RawFd {
        self.0
    }
}

#[derive(Default)]
struct Watcher {
    seat: Option<wl_seat::WlSeat>,
    notifier: Option<ExtIdleNotifierV1>,
    notifications: Vec<(Stage, ExtIdleNotificationV1)>,
    wanted: Stages,
    /// The stage whose `resumed` stands for "the user is back"; without it a
    /// single keypress would send three `Activity` events.
    primary: Option<Stage>,
    announced: bool,
    outgoing: Vec<Event>,
}

impl Watcher {
    fn set_stages(&mut self, handle: &QueueHandle<Watcher>, stages: Stages) {
        self.wanted = stages;
        for (_, notification) in self.notifications.drain(..) {
            notification.destroy();
        }
        self.primary = None;

        let (Some(notifier), Some(seat)) = (&self.notifier, &self.seat) else {
            return;
        };
        for stage in [Stage::Dim, Stage::ScreenOff, Stage::Sleep] {
            let Some(after) = stages.get(stage) else {
                continue;
            };
            // Config validation keeps every timeout under 24h, so the
            // milliseconds fit in the u32 the protocol asks for.
            let timeout = after.as_millis().min(u32::MAX as u128) as u32;
            let notification = notifier.get_idle_notification(timeout, seat, handle, stage);
            self.primary.get_or_insert(stage);
            self.notifications.push((stage, notification));
        }
        debug!(
            stages = self.notifications.len(),
            "idle notifications created"
        );
    }
}

impl Dispatch<wl_registry::WlRegistry, ()> for Watcher {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        handle: &QueueHandle<Watcher>,
    ) {
        match event {
            wl_registry::Event::Global {
                name,
                interface,
                version,
            } => {
                if interface == wl_seat::WlSeat::interface().name && state.seat.is_none() {
                    // One seat; multi-seat is out of scope (`docs/04-idle-wayland.md`).
                    state.seat = Some(registry.bind(name, version.min(7), handle, ()));
                } else if interface == ExtIdleNotifierV1::interface().name {
                    state.notifier = Some(registry.bind(name, 1, handle, ()));
                    info!("ext_idle_notifier_v1 available");
                    state.announced = true;
                    state.outgoing.push(Event::IdleBackendChanged(true));
                } else {
                    return;
                }
                if state.seat.is_some() && state.notifier.is_some() {
                    state.set_stages(handle, state.wanted);
                }
            }
            wl_registry::Event::GlobalRemove { .. } => {}
            _ => {}
        }
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for Watcher {
    fn event(
        _: &mut Self,
        _: &wl_seat::WlSeat,
        _: wl_seat::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Watcher>,
    ) {
    }
}

impl Dispatch<ExtIdleNotifierV1, ()> for Watcher {
    fn event(
        _: &mut Self,
        _: &ExtIdleNotifierV1,
        _: <ExtIdleNotifierV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Watcher>,
    ) {
    }
}

impl Dispatch<ExtIdleNotificationV1, Stage> for Watcher {
    fn event(
        state: &mut Self,
        _: &ExtIdleNotificationV1,
        event: ext_idle_notification_v1::Event,
        stage: &Stage,
        _: &Connection,
        _: &QueueHandle<Watcher>,
    ) {
        match event {
            ext_idle_notification_v1::Event::Idled => {
                debug!(%stage, "idled");
                state.outgoing.push(Event::Idle(*stage));
            }
            // Every stage resumes at once; only the first one speaks.
            ext_idle_notification_v1::Event::Resumed if state.primary == Some(*stage) => {
                debug!(%stage, "resumed");
                state.outgoing.push(Event::Activity);
            }
            _ => {}
        }
    }
}

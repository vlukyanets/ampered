//! Idle notifications from the compositor via `ext-idle-notify-v1`, and the
//! `wlr-output-power-management` client that shares the connection.
//!
//! Reference: `docs/04-idle-wayland.md`, `docs/06-display-dpms.md`. We keep
//! no idle timers of our own (ADR-3): one notification per stage, the
//! compositor does the counting and honours `idle-inhibit-unstable-v1` for
//! us. The output power objects live here because the connection does; the
//! `display` module decides when to use them.
//!
//! This runs in `ampered-agent`, as the session user (`docs/03-privileges.md`);
//! the daemon reaches it through the agent stream.

use std::os::fd::{AsFd, AsRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tokio::io::Interest;
use tokio::io::unix::AsyncFd;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};
use wayland_client::protocol::{wl_output, wl_registry, wl_seat};
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle};
use wayland_protocols::ext::idle_notify::v1::client::ext_idle_notification_v1::{
    self, ExtIdleNotificationV1,
};
use wayland_protocols::ext::idle_notify::v1::client::ext_idle_notifier_v1::ExtIdleNotifierV1;
use wayland_protocols_wlr::output_power_management::v1::client::zwlr_output_power_manager_v1::ZwlrOutputPowerManagerV1;
use wayland_protocols_wlr::output_power_management::v1::client::zwlr_output_power_v1::{
    self, ZwlrOutputPowerV1,
};

use crate::core::{Event, Stage, Stages};

pub const BACKEND: &str = "ext-idle-notify";

/// The reconnect backoff ceiling (`docs/04-idle-wayland.md`).
const RECONNECT_MAX_BACKOFF: Duration = Duration::from_secs(60);

/// Where the compositor is: the session's `XDG_RUNTIME_DIR` and
/// `WAYLAND_DISPLAY`, which the helpers of the `command` display backend
/// inherit too.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Compositor {
    pub runtime_dir: PathBuf,
    pub display: String,
}

impl Compositor {
    pub fn socket(&self) -> PathBuf {
        self.runtime_dir.join(&self.display)
    }
}

/// What the daemon asks of the connection.
#[derive(Debug, Clone, Copy)]
enum Request {
    Stages(Stages),
    Screen(bool),
}

/// Where `main` sends `Command::ReplaceIdleStages`, and where the `wlr`
/// display backend sends `Screen`.
#[derive(Clone)]
pub struct IdleHandle {
    requests: mpsc::UnboundedSender<Request>,
    output_power: Arc<AtomicBool>,
}

impl IdleHandle {
    pub fn replace_stages(&self, stages: Stages) {
        let _ = self.requests.send(Request::Stages(stages));
    }

    /// `wlr-output-power-management` on every output the compositor has.
    pub fn set_screen(&self, on: bool) {
        let _ = self.requests.send(Request::Screen(on));
    }

    /// Whether the compositor offers `zwlr_output_power_manager_v1` right now.
    pub fn output_power_available(&self) -> bool {
        self.output_power.load(Ordering::Relaxed)
    }
}

/// Connects to the compositor and keeps reconnecting for the agent's lifetime.
pub fn spawn(compositor: Compositor, events: mpsc::Sender<Event>) -> IdleHandle {
    let (requests_tx, requests_rx) = mpsc::unbounded_channel();
    let output_power = Arc::new(AtomicBool::new(false));
    tokio::spawn(reconnect_loop(
        compositor,
        events,
        requests_rx,
        output_power.clone(),
    ));
    IdleHandle {
        requests: requests_tx,
        output_power,
    }
}

async fn reconnect_loop(
    compositor: Compositor,
    events: mpsc::Sender<Event>,
    mut requests_rx: mpsc::UnboundedReceiver<Request>,
    output_power: Arc<AtomicBool>,
) {
    let socket = compositor.socket();
    let mut backoff = Duration::from_secs(1);
    // Survives reconnects: the compositor gets the stages back as they were.
    // The screen state does not: a fresh compositor comes up with it on.
    let mut stages = Stages::NONE;

    loop {
        while let Ok(request) = requests_rx.try_recv() {
            if let Request::Stages(update) = request {
                stages = update;
            }
        }

        // True only if the backend had been announced as available, which is
        // the case for every way of losing a live connection — a clean
        // disconnect and a protocol error alike.
        let lost = session(
            &socket,
            &events,
            &mut requests_rx,
            &mut stages,
            &output_power,
        )
        .await;
        output_power.store(false, Ordering::Relaxed);
        if lost {
            info!("compositor connection lost");
            let _ = events.send(Event::IdleBackendChanged(false)).await;
            // The stages are gone with it; do not leave the FSM waiting for a
            // `resumed` that can no longer arrive.
            let _ = events.send(Event::Activity).await;
            backoff = Duration::from_secs(1);
        }

        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(RECONNECT_MAX_BACKOFF);
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
    requests_rx: &mut mpsc::UnboundedReceiver<Request>,
    stages: &mut Stages,
    output_power: &AtomicBool,
) -> bool {
    // The agent is started by `graphical-session.target`, but a compositor
    // can still restart under it — hence the retry rather than an error.
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
    output_power.store(watcher.power_manager.is_some(), Ordering::Relaxed);

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
            request = requests_rx.recv() => {
                drop(guard);
                match request {
                    Some(Request::Stages(update)) => {
                        *stages = update;
                        watcher.set_stages(&handle, update);
                    }
                    Some(Request::Screen(on)) => watcher.set_screen(&handle, on),
                    // The daemon is going away.
                    None => return watcher.announced,
                }
                output_power.store(watcher.power_manager.is_some(), Ordering::Relaxed);
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
    power_manager: Option<ZwlrOutputPowerManagerV1>,
    /// Every `wl_output` in the registry, with its power object once one
    /// was needed (`docs/06-display-dpms.md`).
    outputs: Vec<Output>,
}

struct Output {
    /// The registry name, for `global_remove`.
    name: u32,
    output: wl_output::WlOutput,
    power: Option<ZwlrOutputPowerV1>,
}

impl Watcher {
    /// Creates the power objects that are missing and sets the mode on all
    /// of them. A `failed` object is only recreated here, on the next request.
    fn set_screen(&mut self, handle: &QueueHandle<Watcher>, on: bool) {
        let Some(manager) = &self.power_manager else {
            warn!("no zwlr_output_power_manager_v1; the screen stays as it is");
            return;
        };
        let mode = if on {
            zwlr_output_power_v1::Mode::On
        } else {
            zwlr_output_power_v1::Mode::Off
        };
        for entry in &mut self.outputs {
            let power = entry
                .power
                .get_or_insert_with(|| manager.get_output_power(&entry.output, handle, entry.name));
            power.set_mode(mode);
        }
        debug!(on, outputs = self.outputs.len(), "output power set");
    }

    fn remove_output(&mut self, name: u32) {
        if let Some(pos) = self.outputs.iter().position(|entry| entry.name == name) {
            let entry = self.outputs.remove(pos);
            if let Some(power) = entry.power {
                power.destroy();
            }
            entry.output.release();
            debug!(name, "output gone");
        }
    }

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
                debug!(interface, version, "global");
                if interface == wl_seat::WlSeat::interface().name && state.seat.is_none() {
                    // One seat; multi-seat is out of scope (`docs/04-idle-wayland.md`).
                    state.seat = Some(registry.bind(name, version.min(7), handle, ()));
                } else if interface == ExtIdleNotifierV1::interface().name {
                    state.notifier = Some(registry.bind(name, 1, handle, ()));
                    info!("ext_idle_notifier_v1 available");
                    state.announced = true;
                    state.outgoing.push(Event::IdleBackendChanged(true));
                } else if interface == ZwlrOutputPowerManagerV1::interface().name {
                    state.power_manager = Some(registry.bind(name, 1, handle, ()));
                    info!("zwlr_output_power_manager_v1 available");
                    return;
                } else if interface == wl_output::WlOutput::interface().name {
                    let output = registry.bind(name, version.min(4), handle, ());
                    state.outputs.push(Output {
                        name,
                        output,
                        power: None,
                    });
                    return;
                } else {
                    return;
                }
                if state.seat.is_some() && state.notifier.is_some() {
                    state.set_stages(handle, state.wanted);
                }
            }
            wl_registry::Event::GlobalRemove { name } => state.remove_output(name),
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

impl Dispatch<wl_output::WlOutput, ()> for Watcher {
    fn event(
        _: &mut Self,
        _: &wl_output::WlOutput,
        _: wl_output::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Watcher>,
    ) {
    }
}

impl Dispatch<ZwlrOutputPowerManagerV1, ()> for Watcher {
    fn event(
        _: &mut Self,
        _: &ZwlrOutputPowerManagerV1,
        _: <ZwlrOutputPowerManagerV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Watcher>,
    ) {
    }
}

impl Dispatch<ZwlrOutputPowerV1, u32> for Watcher {
    fn event(
        state: &mut Self,
        power: &ZwlrOutputPowerV1,
        event: zwlr_output_power_v1::Event,
        name: &u32,
        _: &Connection,
        _: &QueueHandle<Watcher>,
    ) {
        match event {
            zwlr_output_power_v1::Event::Mode { mode } => {
                debug!(output = name, ?mode, "output power mode");
            }
            // The compositor gave up on this output (another client took
            // it, or it went away); a new object is made on the next request.
            zwlr_output_power_v1::Event::Failed => {
                warn!(output = name, "output power control failed");
                power.destroy();
                if let Some(entry) = state.outputs.iter_mut().find(|entry| entry.name == *name) {
                    entry.power = None;
                }
            }
            _ => {}
        }
    }
}

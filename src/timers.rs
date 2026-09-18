//! Timers: `Command::StartTimer` in, `Event::Timer` out.
//!
//! The FSM has no clock of its own (`docs/02-state-machine.md`), so every
//! deadline it asks for becomes a task here and comes back as an event.

use std::collections::HashMap;
use std::time::{Duration, SystemTime};

use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::debug;

use crate::core::{Event, TimerId};

pub struct Timers {
    events: mpsc::Sender<Event>,
    running: HashMap<TimerId, Running>,
}

struct Running {
    task: JoinHandle<()>,
    deadline: SystemTime,
}

impl Timers {
    pub fn new(events: mpsc::Sender<Event>) -> Timers {
        Timers {
            events,
            running: HashMap::new(),
        }
    }

    /// Starting a timer that is already running replaces it.
    pub fn start(&mut self, id: TimerId, after: Duration) {
        self.cancel(id);
        debug!(?id, ?after, "timer armed");
        let events = self.events.clone();
        let task = tokio::spawn(async move {
            tokio::time::sleep(after).await;
            let _ = events.send(Event::Timer(id)).await;
        });
        self.running.insert(
            id,
            Running {
                task,
                deadline: SystemTime::now() + after,
            },
        );
    }

    pub fn cancel(&mut self, id: TimerId) {
        if let Some(running) = self.running.remove(&id) {
            running.task.abort();
            debug!(?id, "timer cancelled");
        }
    }

    /// When a timer will fire — `status` reports inhibitor expiry from here.
    pub fn deadline(&self, id: TimerId) -> Option<SystemTime> {
        self.running.get(&id).map(|running| running.deadline)
    }

    /// A fired timer leaves its entry behind; drop it once the event is handled.
    pub fn forget(&mut self, id: TimerId) {
        self.running.remove(&id);
    }
}

//! Timers: `Command::StartTimer` in, `Event::Timer` out.
//!
//! The FSM has no clock of its own (`docs/02-state-machine.md`), so every
//! deadline it asks for becomes a task here and comes back as an event.
//! Each start gets a generation: a fire that was already queued when its
//! timer was restarted or cancelled is dropped here, before the FSM sees it.

use std::collections::HashMap;
use std::time::{Duration, SystemTime};

use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::debug;

use crate::core::TimerId;

pub struct Timers {
    fired_tx: mpsc::UnboundedSender<(TimerId, u64)>,
    fired_rx: mpsc::UnboundedReceiver<(TimerId, u64)>,
    next_generation: u64,
    running: HashMap<TimerId, Running>,
}

struct Running {
    task: JoinHandle<()>,
    deadline: SystemTime,
    generation: u64,
}

impl Default for Timers {
    fn default() -> Timers {
        Timers::new()
    }
}

impl Timers {
    pub fn new() -> Timers {
        let (fired_tx, fired_rx) = mpsc::unbounded_channel();
        Timers {
            fired_tx,
            fired_rx,
            next_generation: 0,
            running: HashMap::new(),
        }
    }

    /// Starting a timer that is already running replaces it.
    pub fn start(&mut self, id: TimerId, after: Duration) {
        self.cancel(id);
        debug!(?id, ?after, "timer armed");
        self.next_generation += 1;
        let generation = self.next_generation;
        let fired = self.fired_tx.clone();
        let task = tokio::spawn(async move {
            tokio::time::sleep(after).await;
            let _ = fired.send((id, generation));
        });
        self.running.insert(
            id,
            Running {
                task,
                deadline: SystemTime::now() + after,
                generation,
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

    /// The next timer that fired and is still current. Cancel-safe, for
    /// `select!` in the main loop.
    pub async fn fired(&mut self) -> TimerId {
        loop {
            // `self` holds a sender, so the channel never closes.
            let Some((id, generation)) = self.fired_rx.recv().await else {
                return std::future::pending().await;
            };
            if self
                .running
                .get(&id)
                .is_some_and(|running| running.generation == generation)
            {
                self.running.remove(&id);
                return id;
            }
            debug!(?id, "stale timer fire dropped");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SETTLE: Duration = Duration::from_millis(50);

    #[tokio::test]
    async fn restart_discards_the_stale_fire() {
        let mut timers = Timers::new();
        timers.start(TimerId::Grace, Duration::from_millis(1));
        // The first timer fires and its event is queued...
        tokio::time::sleep(SETTLE).await;
        // ...before the engine restarts it.
        timers.start(TimerId::Grace, Duration::from_secs(3600));

        assert!(tokio::time::timeout(SETTLE, timers.fired()).await.is_err());
        assert!(timers.deadline(TimerId::Grace).is_some());
        timers.cancel(TimerId::Grace);
        assert!(timers.deadline(TimerId::Grace).is_none());
    }

    #[tokio::test]
    async fn restarted_timer_fires_once() {
        let mut timers = Timers::new();
        timers.start(TimerId::AwakeWindow, Duration::from_millis(1));
        assert_eq!(timers.fired().await, TimerId::AwakeWindow);
        timers.start(TimerId::AwakeWindow, Duration::from_millis(1));
        assert_eq!(timers.fired().await, TimerId::AwakeWindow);
        assert!(timers.deadline(TimerId::AwakeWindow).is_none());
        assert!(tokio::time::timeout(SETTLE, timers.fired()).await.is_err());
    }
}

# 02 — State machine

`core::Engine` is a pure function `handle(&mut self, Event) -> Vec<Command>`.
No I/O inside. Timers are also commands (`Command::StartTimer`) and events
(`Event::Timer`), so the FSM can be tested without `tokio::time`.

## States

```rust
enum State {
    Active,
    Dimmed,
    ScreenOff,
    Suspending,          // Command::Suspend sent, waiting for PrepareForSleep(true)
    Sleeping,            // between PrepareForSleep(true) and (false)
    LongSleep(Phase),    // server cycle, see 10-long-sleep-rtc.md
}

enum Phase { Grace, Armed, Sleeping, Checking }
```

Besides the state, `Engine` also holds:

- `mode: ModeSelection` — `Auto(name)` | `Manual(name)`
- `power: PowerSnapshot` — `ac: bool`, `battery: Option<u8>`, `low: bool` (with hysteresis)
- `inhibits: Vec<Inhibit>` — internal (from IPC) with a TTL
- `sleep_failures: u8` — failure counter for the long-sleep cycle

## Events

```rust
enum Event {
    Idle(Stage),                  // Stage::Dim | ScreenOff | Sleep
    Activity,                     // resumed from the compositor
    AcChanged(bool),
    Battery(u8),
    Suspending,                   // PrepareForSleep(true)
    Resumed,                      // PrepareForSleep(false)
    Timer(TimerId),               // Grace | SleepRetry | AwakeWindow | InhibitExpiry(id)
    Ipc(RequestId, Request),
    IdleBackendChanged(bool),     // compositor connected/lost
    ReloadRequested,
    ShutdownRequested,
}
```

## Commands

```rust
enum Command {
    Dim(u8), Undim,
    Screen(bool),
    ApplyMode(String),
    ReplaceIdleStages(Stages),    // None = disable all
    Suspend { method, force },
    ScheduleWake(SystemTime),
    RunHook(String),
    StartTimer(TimerId, Duration), CancelTimer(TimerId),
    Reply(RequestId, Response),
    Broadcast(StateEvent),        // for subscribe
    Reload, Shutdown,
}
```

## Transition table (main)

| State | Event | Condition | New state | Commands |
|---|---|---|---|---|
| Active | Idle(Dim) | — | Dimmed | Dim(pct) |
| Active | Idle(ScreenOff) | dim disabled | ScreenOff | Screen(false) |
| Dimmed | Idle(ScreenOff) | — | ScreenOff | Screen(false) |
| Dimmed | Activity | — | Active | Undim |
| ScreenOff | Idle(Sleep) | no inhibitors | Suspending | Suspend |
| ScreenOff | Idle(Sleep) | has inhibitors | ScreenOff | StartTimer(SleepRetry) |
| ScreenOff | Timer(SleepRetry) | no inhibitors | Suspending | Suspend |
| ScreenOff | Activity | — | Active | Undim, Screen(true) |
| Suspending | Suspending | — | Sleeping | — |
| Suspending | Activity | logind refused / race | Active | Undim, Screen(true) |
| Sleeping | Resumed | — | Active | Undim, Screen(true), ReplaceIdleStages |
| * | AcChanged(x) | auto mode | — | ApplyMode, ReplaceIdleStages |
| Active/Dimmed/ScreenOff | AcChanged(false) | server.enabled | LongSleep(Grace) | StartTimer(Grace) |
| LongSleep(Grace) | AcChanged(true) | — | Active | CancelTimer(Grace) |
| LongSleep(Grace) | Timer(Grace) | — | LongSleep(Armed) | ScheduleWake, Suspend |
| LongSleep(*) | Ipc(LongSleep{cancel}) | — | Active | CancelTimer(*) |
| * | Ipc(Mode(name)) | — | — | ApplyMode, ReplaceIdleStages, Reply |
| * | ReloadRequested | — | — | Reload |
| * | ShutdownRequested | — | — | Undim, Screen(true), Shutdown |

The full set of `LongSleep` transitions is in `10-long-sleep-rtc.md`.

## Invariants

- `Dim` is never sent while already in `Dimmed`/`ScreenOff` (idempotency).
- `Undim` is always sent when leaving `Dimmed`/`ScreenOff`/`Sleeping`, even
  if `pre_dim` is empty — `backlight` decides what to do on its own
  (`05-backlight.md`).
- `Suspend` is only sent from `ScreenOff`, `LongSleep(Armed)`, or via
  `Ipc(Sleep)` from any state.
- On `ShutdownRequested` the FSM must restore the screen and brightness —
  otherwise `systemctl stop ampered` would leave the user with a dark screen.

## Testing

Table-driven tests: each row of the table above is one `#[test]` or one
row in `TRANSITIONS: &[(State, Event, State, &[Command])]`. See `15-testing.md`.

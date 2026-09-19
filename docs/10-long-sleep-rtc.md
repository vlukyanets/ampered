# 10 — Long sleep: server cycle and RTC

## The problem

The laptop runs as a server. Power is lost; there's no UPS — the laptop's
battery *is* the UPS. We must not drain to zero, and must not miss power
coming back.

## The approach

Sleep with an RTC alarm, wake up periodically, check AC.

When AC is lost, the FSM enters `LongSleep(Grace)` and starts a
`grace_period` timer. If AC returns before it expires, nothing else
happens and the machine goes back to `Active`. If the grace period
expires with AC still gone, the cycle begins:

- **`LongSleep(Armed)`**: schedule a wake at `now + check_interval`
  (`Command::ScheduleWake(check_interval)` — the FSM has no clock, the
  daemon adds `now`), and suspend once the daemon answers
  `Event::WakeScheduled(true)`. A machine must never go to sleep in the
  cycle without an armed alarm, so `WakeScheduled(false)` (no RTC, a
  write error) counts as a failed attempt instead.
- **`LongSleep(Sleeping)`**: wait for `PrepareForSleep(false)`.
- **`LongSleep(Checking)`**: classify the wake-up reason. If it was the
  `User` (lid opened, input), go to `Active`. If AC is online, run
  `resume_hook` and go to `Active`. If battery is at or below
  `critical_action`'s threshold, perform the critical action (hibernate or
  poweroff). Otherwise, start an `AwakeWindow` timer.
- **`Timer(AwakeWindow)`**: re-read the power snapshot; if AC is present,
  go to `Active`, otherwise go back to `Armed` and repeat the cycle.

While in `LongSleep`, idle stages are not created
(`ReplaceIdleStages(None)`) — the screen is already closed, and
`Idle(Sleep)` would only cause confusion.

## Transition table

| State | Event | Condition | New | Commands |
|---|---|---|---|---|
| Active* | AcChanged(false) | server.enabled, trigger=ac_lost | Grace | StartTimer(Grace) |
| Any | Ipc(LongSleep) | server.enabled | Grace | StartTimer(Grace, 0) |
| Grace | AcChanged(true) | | Active | CancelTimer(Grace) |
| Grace | Timer(Grace) | | Armed | ReplaceIdleStages(None), ScheduleWake |
| Armed | WakeScheduled(true) | no inhibitors | Armed | Suspend |
| Armed | WakeScheduled(false) / SleepBlocked / Activity | the alarm or the suspend didn't happen | Armed | sleep_failures += 1; < 3 → StartTimer(AwakeWindow), ≥ 3 → Active |
| Armed | Suspending | | Sleeping | |
| Armed | Timer(AwakeWindow) | retry | Armed | ScheduleWake |
| Sleeping | Resumed | ac | Active | RunHook, ApplyMode, ReplaceIdleStages |
| Sleeping | Resumed | battery ≤ critical | Checking | Broadcast(critical), Suspend{Hibernate, force} or PowerOff, StartTimer(AwakeWindow) |
| Sleeping | Resumed | otherwise | Checking | StartTimer(AwakeWindow) |
| Checking | Activity | classify = User, lid, input | Active | CancelTimer(AwakeWindow), Broadcast(interrupted) |
| Checking | AcChanged(true) | | Active | CancelTimer(AwakeWindow), RunHook, ApplyMode, ReplaceIdleStages |
| Checking | Timer(AwakeWindow) | ac | Active | RunHook, ApplyMode, ReplaceIdleStages |
| Checking | Timer(AwakeWindow) | battery ≤ critical | Checking | Broadcast(critical), the critical action, StartTimer(AwakeWindow) |
| Checking | Timer(AwakeWindow) | otherwise | Armed | ScheduleWake |
| Any LongSleep | Ipc(LongSleep{cancel}) | | Active | CancelTimer(*), CancelWake |

`*` — Active, Dimmed, ScreenOff.

A wake by the user does not have its own event: the daemon classifies the
wake right after `Resumed` and feeds `Activity` (ADR-13), which is also what
the compositor sends when the lid opens during `Checking`. Every exit from
the cycle to `Active` after a sleep also sends `Undim` and `Screen(true)`,
for the same reason a regular `Resumed` does.

The critical action leaves the FSM in `Checking` with the window running:
if the hibernate does not happen (refused, unavailable), the next window
re-evaluates the battery and tries again, instead of sitting in a dead
state at 10%.

The daemon restarted in the middle of the cycle (`state.json` says
`LongSleep`, no AC at startup) starts in `Checking` with the window running.

## Why `awake_window`

Right after resume, `power_supply/*/online` can be stale (the EC hasn't
been polled yet), `capacity` can reflect the past. 30–60s is enough. It's
also a window during which an admin can SSH in, if the network came up.

## Classifying the wake-up

Did we wake up on the alarm, or did someone open the lid? If it's the
latter, we must not go back to sleep.

The classifier compares `now` (taken **after** resume) against
`scheduled` (the alarm the daemon armed for `Command::ScheduleWake`, also
written to `state.json` before sleep). If the absolute difference is
within `alarm_slack`, the wake-up is classified as `Scheduled`; otherwise
as `User`. With no alarm armed, every wake is `User`.

Extra hints (logged only, never used as the criterion — they're
platform-dependent): an empty `wakealarm` (the kernel clears a fired
alarm), `/sys/power/pm_wakeup_irq`, `/proc/acpi/wakeup`.

A clock jump (NTP after resume) larger than `alarm_slack`: if `wakealarm`
is empty we call it `Scheduled`, otherwise `User`. An extremely rare case;
documented and not special-cased further.

An interrupted cycle shows up as `server.phase = "interrupted"`; to
resume it, use `amperedctl long-sleep`.

## RTC backend

### `wakealarm` (default)

```
echo 0        > /sys/class/rtc/rtc0/wakealarm   # clear the old one first — mandatory
echo <epoch>  > /sys/class/rtc/rtc0/wakealarm
logind.Suspend()
```

- Accepts a **UTC epoch** regardless of the RTC's clock mode
  (`hwclock --localtime`); the kernel converts it itself. Don't repeat
  `rtcwake`'s mistake of subtracting the offset under `--localtime`.
- Writing over an existing alarm doesn't work — clear it with `0` first.
- An alarm in the past, or less than ~2s away → `EINVAL`. The minimum
  horizon is `now + 60s`; `check_interval` is validated as `≥ 2m`.
- No `wakealarm` file → the RTC has no alarm → long sleep is disabled,
  `error!` at startup and `degraded: ["rtc"]`.
- **Known limitation:** `echo 0` clears someone else's alarm (cron, a
  user's `rtcwake`). We don't attempt to merge.

### `rtcwake` (exec)

```
rtcwake -m no -d rtc0 -t <epoch>      # only sets the alarm
logind.Suspend()
```

Only writes the alarm; we do **not** use `-m mem` — it sleeps the machine
bypassing logind (hooks wouldn't fire). Both backends are functionally
equivalent, differing only in how the alarm gets written. Requires `util-linux`.

## Critical action

- `hibernate` requires configured swap and `resume=`; checks are in
  `09-sleep-logind.md`. If unavailable, it degrades to `poweroff` with a
  `warn!` at startup (better to find out at startup than at 10% battery).
  The daemon tells the FSM with `Event::HibernateAvailable(false)`, and the
  critical action becomes `PowerOff` — never the plain suspend that
  `[sleep] method` falls back to, which would keep draining the battery.
- `poweroff` — `logind.PowerOff(false)`.

Before the critical action, `RunHook(resume_hook)` is not called, but
`Broadcast(critical)` is.

## `resume_hook`

Runs once power has returned and the cycle has ended. Root, `sh -c`, 5min
timeout, stdout/stderr go to the log. Typically:
`systemctl start my-services.target`.

## Edge cases

| Situation | Behavior |
|---|---|
| AC lost and back within 10s | `grace_period` hasn't expired — nothing happens |
| AC returned during sleep | Wakes on the alarm, sees AC, exits. Delay ≤ `check_interval` |
| Battery drained faster than expected | The next alarm sees `critical` — or the machine has already shut down. Hence a conservative `check_interval` |
| `Suspend` refused by an inhibitor in the cycle | `sleep_failures++`, wait for `awake_window`, retry; after 3 attempts → `Active` |
| Lid opened during `Checking` | Input → compositor `resumed` → `Activity` → treated as `User` in `Checking` → `Active` |
| Daemon restarted during `LongSleep` | `state.json` holds `phase`; if started with no AC, resume from `Checking` |
| `rtc_device` isn't `rtc0` (USB RTC, ARM) | Configure `rtc_device`; check `/sys/class/rtc/<dev>/device/power/wakeup == enabled` |

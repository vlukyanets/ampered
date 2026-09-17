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

- **`LongSleep(Armed)`**: schedule a wake at `now + check_interval`, then suspend.
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
| Grace | Timer(Grace) | | Armed | ReplaceIdleStages(None), ScheduleWake, Suspend |
| Armed | Suspending | | Sleeping | |
| Armed | Activity / Timer(AwakeWindow) | suspend didn't happen | Armed | sleep_failures += 1; ≥ 3 → Active |
| Sleeping | Resumed | classify = User | Active | Broadcast(interrupted) |
| Sleeping | Resumed | ac | Active | RunHook, ReplaceIdleStages, ApplyMode |
| Sleeping | Resumed | battery ≤ critical | — | Suspend{Hibernate} or Poweroff |
| Sleeping | Resumed | otherwise | Checking | StartTimer(AwakeWindow) |
| Checking | AcChanged(true) | | Active | CancelTimer, RunHook |
| Checking | Timer(AwakeWindow) | | Armed | ScheduleWake, Suspend |
| Any LongSleep | Ipc(LongSleep{cancel}) | | Active | CancelTimer(*) |

`*` — Active, Dimmed, ScreenOff.

## Why `awake_window`

Right after resume, `power_supply/*/online` can be stale (the EC hasn't
been polled yet), `capacity` can reflect the past. 30–60s is enough. It's
also a window during which an admin can SSH in, if the network came up.

## Classifying the wake-up

Did we wake up on the alarm, or did someone open the lid? If it's the
latter, we must not go back to sleep.

The classifier compares `now` (taken **after** resume) against
`scheduled` (read from `state.json`, written before sleep). If the
absolute difference is within `alarm_slack`, the wake-up is classified as
`Scheduled`; otherwise as `User`.

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

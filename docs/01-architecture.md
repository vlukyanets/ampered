# 01 — Architecture

## Overview

A single `tokio` process. Actors communicate over channels; the only
decision-making center is `core::Engine`. Four sources feed events into
it — Wayland idle notifications, sysfs/udev power-supply changes, logind
D-Bus signals, and the IPC Unix socket — and it dispatches commands out to
four sinks — backlight, display power, power modes, and the sleep
planner. See the component table below for exactly what each side does.

## Components

| Module | Role | Input | Output |
|---|---|---|---|
| `config` | Loading/validation of `ampered.toml`, hot-reload on SIGHUP | file | `Config` |
| `core` | FSM; the only place decisions are made | all `Event` | all `Command` |
| `idle` | `ext-idle-notify-v1` client; one notification per stage | Wayland | `Event::Idle(stage)` / `Event::Activity` |
| `backlight` | sysfs backlight, smooth transitions, `pre_dim` memory | `Command` | sysfs |
| `display` | DPMS via `wlr-output-power-management` or a command | `Command` | Wayland / `sh -c` |
| `power::supply` | AC and charge from `/sys/class/power_supply` | sysfs, udev | `Event::AcChanged`, `Event::Battery` |
| `power::modes` | Applying a mode | `Command::ApplyMode` | sysfs |
| `logind` | `Suspend`, `Hibernate`, `PrepareForSleep`, `ListInhibitors`, delay lock | D-Bus | `Event::Suspending`, `Event::Resumed` |
| `sleep::rtc` | RTC alarm (`wakealarm` / `rtcwake`) | `Command::ScheduleWake` | RTC |
| `sleep::planner` | Wake classification, `resume_hook` | `Command::ScheduleWake`, `Event::Resumed` | `Event::Activity` for a wake by the user, `sh -c` |
| `ipc` | NDJSON server over a Unix socket; `amperedctl` — a separate binary | socket | `Event::Ipc(req)` |

## Key flows

### Idle → dim → sleep

1. The compositor sends `idled` for the `dim` stage notification.
2. `Engine`: `Active → Dimmed`, `Command::Dim(pct)`.
3. `idled` for `screen_off` → `ScreenOff`, `Command::Screen(false)`.
4. `idled` for `sleep` → checks inhibitors (`09-sleep-logind.md`) → `Command::Suspend`.
5. Any `resumed` → `Active`, `Command::Undim`, `Command::Screen(true)`.

### Power source change

1. `power::supply` notices a change in `online` → `Event::AcChanged(bool)`.
2. `Engine` picks a mode from `[auto_mode]` → `Command::ApplyMode`,
   `Command::ReplaceIdleStages` (notifications are recreated with new timeouts).
3. If `server.enabled` and AC is lost → `Command::StartTimer(Grace)`; if AC
   hasn't returned once it expires → `LongSleep` (`10-long-sleep-rtc.md`).

### Resume from sleep

1. logind sends `PrepareForSleep(false)` → `Event::Resumed`.
2. `Engine` → `Active`, `Command::Undim`, `Command::Screen(true)`,
   `Command::ReplaceIdleStages` (the compositor may have lost notifications).
3. If it was in `LongSleep`, the FSM goes to `LongSleep(Checking)` instead,
   and the planner classifies the wake-up: a wake by the user becomes
   `Event::Activity`, which ends the cycle (`10-long-sleep-rtc.md`).

## Failure handling

| Failure | Behavior |
|---|---|
| No Wayland / compositor crashed | `idle` reconnects with backoff; stages reset to `Active` |
| No `ext_idle_notifier_v1` in the registry | `idle: unavailable`; everything else works |
| No backlight device | dim — a no-op with a single `warn!` |
| No D-Bus / logind | sleep disabled, `degraded: ["logind"]` in status |
| No `wakealarm` | long sleep disabled with `error!` at startup |
| sysfs write error | `warn!`, the FSM transition happens anyway |
| Panic in an actor | the process crashes; systemd `Restart=on-failure` |

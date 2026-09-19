# 04 — Idle: Wayland integration

## Protocol

`ext-idle-notify-v1` (staging, `wayland-protocols` ≥ 1.27):

```
ext_idle_notifier_v1.get_idle_notification(timeout_ms, seat)
    → ext_idle_notification_v1 { events: idled, resumed }
```

We create **one notification per stage** — `dim`, `screen_off`, `sleep`.
The compositor tracks idleness itself and sends `idled` once the threshold
is crossed, and `resumed` on any input. `ampered` has no idle timers of its own.

What the compositor does for us:

- honors `idle-inhibit-unstable-v1` (video playing in a browser) — `idled`
  simply won't arrive;
- decides what counts as input (mouse, keyboard, touch, gamepad);
- doesn't interfere with its own `swayidle`-like timeouts — notifications are independent.

One `wl_seat` — we take the first one from the registry. Multi-seat is not supported.

## `idle::Watcher` lifecycle

On `connect(runtime_dir/display)`: a failure triggers backoff (1s, 2s, 4s
… up to `reconnect_max_backoff`) and a retry. On success, it walks the
registry looking for `ext_idle_notifier_v1`: if it's missing, emit
`Event::IdleBackendChanged(false)` and keep the connection open, waiting
for the global to appear; if it's present, emit
`Event::IdleBackendChanged(true)` and call `create_stages(current)`.

Once connected, the dispatch loop handles three cases: `idled(stage)`
becomes `Event::Idle(stage)`; `resumed(stage)` becomes `Event::Activity`
(only from the `dim` stage, or the first enabled one, so we don't send
`Activity` three times); and the socket closing emits
`Event::IdleBackendChanged(false)` and `Event::Activity`, then reconnects.

`Command::ReplaceIdleStages(stages)` → destroys all notifications, creates
new ones. Called on mode change, reload, resume from sleep.

Timeouts are **absolute** from the start of idleness (`dim_after = 5m`,
`sleep_after = 30m` → sleep after 30 minutes, not 35).

## Stages

Three stages fire in order as idleness grows: `dim` moves `Active` to
`Dimmed`, `screen_off` moves `Dimmed` to `ScreenOff`, and `sleep` moves
`ScreenOff` to `Sleeping`.

Any stage can be disabled (`"0"`). Then no notification is created and the
FSM skips over it (`Active → ScreenOff` directly). All three set to `0` —
idle has no effect at all (`server` mode).

## Compositor support

| Compositor | `ext-idle-notify` | Note |
|---|---|---|
| Sway ≥ 1.8 | yes | |
| Hyprland | yes | |
| river, niri, labwc | yes | wlroots-based |
| KWin ≥ 5.27 | yes | DPMS via `kscreen-doctor` |
| Mutter / GNOME | **no** | roadmap: `mutter` backend via `org.gnome.Mutter.IdleMonitor` |
| Weston | no | not planned |

## Fallback: logind IdleHint

`[idle] fallback = "logind"` — use logind's `IdleHint`/`IdleSinceHint`
(the manager-level aggregate over sessions). Coarse (updated rarely by
the compositor and not by all of them), only good enough for `sleep`, not
for dim. Disabled by default.

Implemented in the `logind` actor as polling once every 30s, and only while
the fallback is enabled and the compositor backend is not connected
(`Event::IdleBackendChanged(false)`): with `IdleHint = true` for at least
the current mode's `sleep_after`, it emits `Event::Idle(Sleep)` once; when
`IdleHint` drops back to `false`, `Event::Activity`. The stages come from
the same `Command::ReplaceIdleStages` the Wayland watcher gets, so a mode
change or a reload (including `[idle] fallback` itself) is picked up
without a restart. `amperedctl status` shows `idle.backend = "logind"`
while the fallback is in charge.

## Edge cases

| Situation | Behavior |
|---|---|
| Compositor restarted | reconnect, all stages → `Active`, `Event::Activity` |
| `idled` arrives for `sleep` before `dim` (restart with already-large idle) | FSM handles it as-is: `Active → Suspending`, skipping dim — this is correct |
| Resume from sleep — does the compositor send `resumed`? | Not guaranteed; `Engine` itself does `Undim`/`Screen(true)` on `Resumed` from logind |
| A notification with a timeout > `u32::MAX` ms (~49 days) | Config validation rejects `> 24h` |
| A second `Watcher` on the same compositor (dev run alongside prod) | Works: notifications are independent; but dim happens twice — a warning is noted in `15-testing.md` |

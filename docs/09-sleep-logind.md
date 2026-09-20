# 09 — Regular sleep via logind

`sleep_after` expires → inhibitors are checked → `org.freedesktop.login1.Manager`:
`Suspend(false)` / `Hibernate(false)` / `SuspendThenHibernate(false)`
depending on `[sleep] method`.

## Why logind and not `/sys/power/state`

- `PrepareForSleep` fires — browsers, NetworkManager, and services prepare
  and wake up correctly.
- `/usr/lib/systemd/system-sleep/*` hooks and `sleep.target` run.
- Inhibitors are respected (`systemd-inhibit`, downloads, etc.).
- polkit/audit work like for everyone else.

## Inhibitors

`ListInhibitors()` → we take entries where `what` contains `sleep` and
`mode == "block"`. If there's at least one, we don't sleep: `info!` with
`who`/`why`, `Command::StartTimer(SleepRetry, sleep.sleep_retry)`. The
timer triggers a recheck. `delay` inhibitors don't block — logind itself
waits for them to be released.

Plus internal inhibitors from IPC (`11-ipc-cli.md`): `what == sleep`
blocks the same way; `what == idle` blocks all stages (the FSM ignores
`Event::Idle`).

`respect_inhibitors = false` or `amperedctl sleep --force` skips the check.

## Our own delay lock

At startup: `Inhibit("sleep", "ampered", "save state before sleep", "delay")`
— we get an fd and hold it for the daemon's lifetime.

On `PrepareForSleep(true)`:

1. Save `state.json` to `$STATE_DIRECTORY` (`/var/lib/ampered`; the same
   directory holds the `mode` file from `08-power-modes.md`): FSM
   state, mode, sleep reason (`regular` / `long-sleep`), `scheduled_wake`
   (RFC 3339, for long sleep), `saved_at`.
2. If this is a long sleep, make sure the alarm has been written
   (`10-long-sleep-rtc.md`).
3. Close the fd → logind proceeds. Take a new fd right after
   `PrepareForSleep(false)`.

`InhibitDelayMaxSec` defaults to 5s — we finish with plenty of margin.

On `PrepareForSleep(false)`: read `state.json`, emit `Event::Resumed`.

## Hibernate

Before using `hibernate` / `suspend-then-hibernate` / `critical_action`:

- `CanHibernate()` must return `"yes"`;
- `/sys/power/disk` must not be `[disabled]`.

Otherwise, at startup: `warn!` and `degraded: ["hibernate"]`; an attempt
falls back to `Command::Suspend` with `method = Suspend` instead of
hibernate (better to sleep than to fail outright).

## Recommended `logind.conf`

```ini
[Login]
IdleAction=ignore                     # idle is handled by ampered
# For the server scenario:
HandleLidSwitch=ignore
HandleLidSwitchExternalPower=ignore
HandleLidSwitchDocked=ignore
```

If `HandleLidSwitch=suspend`, logind will sleep on lid close **without**
an RTC alarm — the server cycle won't trigger. These settings are not
exposed on the bus, so `logind_conf` reads them the way logind does:
the main file (`/etc/systemd/logind.conf`, else `/run`, `/usr/local/lib`,
`/usr/lib`), then the drop-ins from all `logind.conf.d` directories
merged and applied in file-name order, a same-named file in a
higher-priority directory shadowing the others, and the `[Login]` section
only. Defaults when a key is missing or commented out:
`HandleLidSwitch=suspend`, `HandleLidSwitchExternalPower` = the value of
`HandleLidSwitch`, `HandleLidSwitchDocked=ignore`, `IdleAction=ignore`.

At startup and on every reload:

- `server.enabled` and any of the three lid settings other than `ignore` →
  `warn!` naming the key and `degraded: ["lid-switch"]`;
- `IdleAction` other than `ignore`, in any mode → `warn!` and
  `degraded: ["idle-action"]`: logind and ampered would both act on idle.

Nothing is changed on disk; the fix is the drop-in above and
`systemctl restart systemd-logind` (or a reboot).

# 17 — Decision log (ADR)

Format: number, date, decision, reason, consequences. A new decision gets
a new entry; old ones are never edited (only marked "superseded by ADR-N").

## ADR-1 — Single tokio process, actors over channels
**2026-09.** Not one thread per module, not separate processes.
Reason: simplicity, a single binary, all I/O async.
Consequences: a panic in one actor brings the whole process down (accepted, `Restart=on-failure`).
**Superseded in part by ADR-17:** the compositor side is a second process, `ampered-agent`.

## ADR-2 — FSM as a pure function
**2026-09.** `Engine::handle(Event) -> Vec<Command>`, timers are also
events. Reason: table-driven tests with no real time and no sysfs.
Consequences: `main.rs` is the only "dirty" spot; a bit more boilerplate.

## ADR-3 — Idle only through `ext-idle-notify-v1`
**2026-09.** Not a `swayidle` wrapper, not a homegrown `evdev` timer.
Reason: the compositor already knows everything (inhibit, input devices);
`evdev` needs root on `/dev/input` and doesn't see virtual input.
Consequences: GNOME isn't supported (roadmap: a `mutter` backend).

## ADR-4 — Sleep only through logind
**2026-09.** Never write to `/sys/power/state`, never `rtcwake -m mem`.
Reason: hooks, inhibitors, `PrepareForSleep`.
Consequences: a dependency on D-Bus; without it, degraded with no sleep support.

## ADR-5 — RTC via `wakealarm`, `rtcwake` only with `-m no`
**2026-09.** sysfs is the default; `rtcwake` is a fallback, used only to
write the alarm. Reason: `-m mem` bypasses logind (ADR-4); `wakealarm`
accepts UTC regardless of the clock mode.
Consequences: `echo 0` clears someone else's alarm — a known limitation.

## ADR-6 — Root unit with an explicit Wayland socket path (v0.1)
**2026-09.** No split mode, no polkit. Reason: minimal amount of code for
a working v0.1. Consequences: a root process parses the user's Wayland
protocol; a deliberate debt, closed by split mode in v0.3 (`03-privileges.md`).
**Superseded by ADR-17.**

## ADR-7 — Our own `power_supply` parser, no UPower
**2026-09.** Reason: two numbers aren't worth a D-Bus dependency and an
extra daemon. Consequences: we handle multiple batteries, `scope=Device`,
and stale values after resume ourselves.

## ADR-8 — A mode is hardware + timeouts, no inheritance
**2026-09.** Reason: "on battery" means both CPU and dim settings;
inheritance would obscure what actually gets applied.
Consequences: the config is a bit longer.

## ADR-9 — Restore brightness only if the value hasn't changed
**2026-09.** Compared against `written_target`, not `pre_dim`. Reason:
the classic "the daemon stomped on my brightness" bug. Consequences: if
an external write happens to match `written_target`, we restore anyway —
a negligible probability.

## ADR-10 — Manual mode doesn't survive a restart
**2026-09.** Reason: no persistence store in v0.1; `state.json` is only
for sleep. Consequences: after `systemctl restart`, it's back to auto.
Revisit in v0.2. *Superseded by ADR-15.*

## ADR-11 — Inhibitors reach the FSM as events, and are re-checked on the way out
**2026-09.** logind has no "inhibitors changed" signal, and `ListInhibitors()`
is I/O, which `Engine::handle` must not do (ADR-2). So the `logind` actor polls
the list and pushes `Event::Inhibitors`, which the FSM keeps as a cache for the
documented `Idle(Sleep)` pre-check and for `status.blocked_by`. Because a cache
can be stale by seconds, the actor checks again when it executes
`Command::Suspend` and answers a refusal with `Event::SleepBlocked`, which
returns the FSM to the state the suspend was requested from and arms
`SleepRetry`. Reason: a fresh inhibitor (`systemd-inhibit make`) must never lose
a race with a cached list. Consequences: two events instead of one, and a sleep
attempt can be rejected after `Command::Suspend` was already sent.

## ADR-12 — udev over raw netlink, not libudev
**2026-09.** The open question in `docs/07-power-supply.md` is settled: we bind
`NETLINK_KOBJECT_UEVENT` group 1 with `nix` and match `SUBSYSTEM=power_supply`
in the NUL-separated payload. Reason: no C dependency, and the kernel's uevent
format is a dozen lines of parsing. Consequences: group 1 needs CAP_NET_ADMIN,
which the root unit has; without it the actor warns once and lives on the 60s
poll, so a non-root development run still works.

## ADR-13 — The clock stays out of the FSM: relative wakes, classification in the daemon
**2026-09.** `Command::ScheduleWake` carries a `Duration` (`check_interval`),
and `main` adds `now`, arms the RTC, remembers the absolute time and answers
`Event::WakeScheduled(bool)`; only `true` lets the FSM send `Suspend`. On
`Resumed` the FSM moves from `LongSleep(Sleeping)` to `LongSleep(Checking)`
unconditionally; the daemon then compares `now` with the remembered alarm
(`sleep::planner::classify_wake`) and, for a wake by the user, feeds
`Event::Activity` — the same event the compositor sends when the lid opens
during `Checking`. Reason: ADR-2 — the engine has no clock, and a
`SystemTime` in a command or a classification inside the FSM would need one.
Consequences: a wake by the user passes through `Checking` for one event,
with a `StartTimer`/`CancelTimer` pair; one round trip between
`ScheduleWake` and `Suspend`, which is what keeps a machine from sleeping in
the cycle with no alarm armed.

## ADR-14 — `sd_notify` by hand, no `sd-notify` crate
**2026-09.** `notify.rs` sends the `KEY=VALUE` datagram to `NOTIFY_SOCKET`
itself (path or abstract name) with `std::os::unix::net`. Reason: the whole
protocol is one `sendto`, and the crate would be the only dependency in the
tree that exists for a single function. Consequences: `READY`, `RELOADING`,
`STOPPING`, `STATUS` and `WATCHDOG` are the entire vocabulary we speak;
`MONOTONIC_USEC` for `Type=notify-reload` is not sent, so the unit stays
`Type=notify` with `ExecReload`.

## ADR-15 — Manual mode is persisted as one file
**2026-09.** Supersedes ADR-10. `$STATE_DIRECTORY/mode` holds the name of
the manual mode; it is written on `amperedctl mode <name>`, removed on
`amperedctl mode auto`, and read once at startup. Reason: a user who chose
`performance` before a reboot did not choose `balanced` after it, and one
name in one file needs no store. Consequences: `state.json` stays the
sleep-only file it was; a name missing from the reloaded config is dropped
with a `warn!`, never applied blindly.

## ADR-16 — Split mode over the IPC socket, not D-Bus + polkit
**2026-09.** `ampered-agent` talks to the daemon through the existing
NDJSON socket (`{"cmd":"agent"}` turns a connection into a two-way stream),
and is authorized by `[general] socket_group` like every other client.
Reason: the D-Bus API and polkit rule sketched in `03-privileges.md` would
add a second bus role (we are a logind client only — `CLAUDE.md`), a policy
file and a name to register, all to carry four messages between two
processes we ship together; and the socket group already answers "who may
tell the daemon to sleep". The agent lives in the same crate as a third
binary because it reuses `idle` and `display` whole. Consequences: any
member of the group can register as the agent and feed idle events — the
same trust the group already has through `amperedctl sleep`; a per-session
check against logind's active session can be added later without changing
the protocol. Supersedes the D-Bus sketch in `03-privileges.md`.

## ADR-17 — Daemon + agent is the only privilege model
**2026-09.** The root daemon never opens a Wayland connection and never
runs a compositor helper; `ampered-agent` in the session always does.
`[general] privilege` and `[wayland]` are gone, and so is the "no root at
all" deployment sketch. Reason: two code paths for the same job — one of
them a root process parsing the session's protocol and `setuid`-ing for
`swaymsg` (ADR-6) — doubled the surface to test and document for a
convenience that split mode already provides without the debt; the udev
`video` route could not reach `platform_profile` or cpufreq anyway, so it
was never a real mode. Consequences: a laptop needs both units; the daemon
drops `CAP_SETUID/SETGID`; `idle` and `display` are linked only into the
agent; a machine with no session (a headless server) runs the daemon
alone, exactly as an agent-less daemon did before. Supersedes ADR-6.

# 17 — Decision log (ADR)

Format: number, date, decision, reason, consequences. A new decision gets
a new entry; old ones are never edited (only marked "superseded by ADR-N").

## ADR-1 — Single tokio process, actors over channels
**2026-09.** Not one thread per module, not separate processes.
Reason: simplicity, a single binary, all I/O async.
Consequences: a panic in one actor brings the whole process down (accepted, `Restart=on-failure`).

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
Revisit in v0.2.

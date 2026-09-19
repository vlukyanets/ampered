# CLAUDE.md — ampered

Context for the agent/developer who will write code in this repository.
Read it in full before your first commit. Anything not covered here is in
`docs/`.

## What this is

**ampered** is a power management daemon for Linux laptops under Wayland,
written in Rust. It closes the gap between `logind`/`power-profiles-daemon`
(which don't know about compositor idle) and `swayidle`/`hypridle` (which
don't know about battery and power).

Key scenario: **laptop as a home server**. Lid closed, power lost → the
machine sleeps with an RTC alarm, wakes up periodically, checks whether
power is back, and either keeps running, sleeps again, or goes to
hibernate at critical battery.

## Stack and constraints

- Rust, edition 2024, MSRV 1.98. Async — `tokio`. Single process, actors on channels.
- Wayland: `wayland-client` 0.31, `ext-idle-notify-v1` (staging), `wlr-output-power-management`.
- D-Bus: `zbus` (logind only). No UPower — we read `/sys/class/power_supply` ourselves.
- Config: TOML (`serde` + `toml` + `humantime-serde`). CLI: `clap`.
- Logs: `tracing`. Errors: `thiserror` in modules, `anyhow` only in binaries.
- No `unsafe` without a "why" comment. No `unwrap()` outside tests.

## Documentation map

| File | What's there |
|---|---|
| `docs/00-overview.md` | Goals, scenarios, what we do NOT do |
| `docs/01-architecture.md` | Components, event flows |
| `docs/02-state-machine.md` | FSM: states, events, commands, transition table |
| `docs/03-privileges.md` | Root vs. user Wayland socket |
| `docs/04-idle-wayland.md` | `ext-idle-notify-v1`, fallbacks |
| `docs/05-backlight.md` | sysfs backlight, dim/restore, edge cases |
| `docs/06-display-dpms.md` | Turning the screen off |
| `docs/07-power-supply.md` | AC/battery from sysfs, udev, hysteresis |
| `docs/08-power-modes.md` | Modes: what's written where, auto-switching, conflicts |
| `docs/09-sleep-logind.md` | Regular sleep, inhibitors, delay lock |
| `docs/10-long-sleep-rtc.md` | Server cycle, `wakealarm`/`rtcwake`, wake classification |
| `docs/11-ipc-cli.md` | Socket protocol, `amperedctl` |
| `docs/12-configuration.md` | `ampered.toml` reference |
| `docs/13-config-example.md` | Full config example |
| `docs/14-code-layout.md` | Module tree, traits, dependencies |
| `docs/15-testing.md` | What and how we test |
| `docs/16-deployment.md` | systemd unit, udev, installation |
| `docs/17-decisions.md` | Architecture decision log (ADR) |
| `docs/18-roadmap.md` | Version plan |

## Rules for code

1. **Only `core::Engine` makes decisions.** `Engine::handle(Event) -> Vec<Command>`
   — a pure function with no I/O. Modules don't call each other directly.
2. **Anything that touches the system sits behind a trait** with a `Fake*`
   implementation for tests (`BacklightSink`, `RtcAlarm`, `PowerSource`,
   `SleepBackend`, `IdleSource`).
3. **A missing subsystem is not an error.** No compositor, no backlight, no
   RTC — the module is `unavailable`, the daemon runs degraded and shows
   this in `amperedctl status`. The only thing allowed to crash on is an
   invalid config.
4. **Transitions are idempotent.** `dim` while already `Dimmed` is a no-op.
5. **A sysfs write error does not block the FSM transition** — `warn!` and move on.
6. **Documentation is the source of truth.** Changing behavior means
   updating `docs/` first, then the code. A new architectural decision
   gets an entry in `docs/17-decisions.md`.
7. **Do not add dependencies** without an entry in `docs/14-code-layout.md`.

## Implementation order (v0.1 and v0.2, both done)

1. `config` — structs + validation + a test against `docs/13-config-example.md`.
2. `core` — FSM with table-driven tests (see `docs/02-state-machine.md`).
3. `ipc` + `amperedctl status` — so state can be inspected from day one.
4. `power::supply` → `power::modes` → auto-switching.
5. `backlight`.
6. `idle` (Wayland) — the trickiest part, do it after the FSM is already
   verified with fakes.
7. `logind` — suspend + inhibitors.
8. `display`.
9. v0.2: `sleep::rtc` + `sleep::planner`, then the rest of the v0.2 list
   in `docs/18-roadmap.md`. Next up is v0.3.

## Commands

```sh
cargo build
cargo test
cargo run -- --config examples/ampered.toml --check   # examples/ is generated from docs/13-config-example.md
RUST_LOG=ampered=debug cargo run -- --socket /tmp/ampered.sock
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

## What not to do

- Don't replace logind: sleep only through `org.freedesktop.login1`.
- Don't implement idle-inhibit — that's the compositor's job.
- Don't pull in UPower, don't pull in GTK/Qt, no tray icon.
- Don't write to sysfs knobs that aren't documented in the mode.

## Documentation language

- All documentation (`README.md`, `CLAUDE.md`, `docs/`, code comments,
  commit messages) is written in English only. No exceptions.

## Commits

- No AI attribution (`Co-Authored-By`, "Generated with", tool signatures,
  etc.) in commit messages.
- First line — a concise summary of what was done.
- Followed, if needed, by a blank line and paragraphs describing the
  implementation.

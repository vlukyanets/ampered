# 03 — Privilege model

## The conflict

Power management needs two things that no single process should have at
once:

- **root:** writing to `/sys/class/backlight`, `/sys/firmware/acpi/platform_profile`,
  `/sys/devices/system/cpu/*/cpufreq`, `/sys/class/rtc/*/wakealarm`.
- **the user's session:** connecting to the Wayland socket in
  `$XDG_RUNTIME_DIR`, running `swaymsg`/`niri msg` (which need the
  compositor's IPC socket).

## Two processes: `ampered` (root) + `ampered-agent` (user)

This is the only model; there is no single-process mode and no config key
for it (ADR-17).

- **`ampered`** is the root daemon, `ampered.service` in
  `multi-user.target`: FSM, IPC, sysfs writes, logind, RTC, power supply.
  It opens **no** Wayland connection and runs no compositor helper — it
  never parses a byte from the session and needs no `CAP_SETUID`.
- **`ampered-agent`** is a small service in the user's
  `graphical-session.target`. It owns everything that needs the session:
  the `ext-idle-notify-v1` watcher, the `wlr` output-power client and the
  `command` DPMS helper, running as the user with the session's own
  `XDG_RUNTIME_DIR` and `WAYLAND_DISPLAY`. Nothing about the compositor
  is configured on the daemon's side.
- The two talk over the daemon's IPC socket, not D-Bus (ADR-16): the agent
  connects like any client, sends `{"cmd":"agent"}` and the connection
  turns into a two-way stream — idle events up, stages and screen requests
  down (`11-ipc-cli.md`, "The agent stream"). The daemon pushes the
  `[display]` section to the agent on registration and on every reload,
  so the agent needs no config file of its own.
- Authorization is the socket's: membership in `[general] socket_group`,
  the same right that lets a user run `amperedctl sleep`. One agent at a
  time; a new registration replaces the old stream (a compositor restart
  starts a new agent), and the uid is logged from `SO_PEERCRED`.
- Without an agent the daemon runs as it does without a compositor: no
  idle stages, `degraded: ["agent"]`, the logind `IdleHint` fallback if
  enabled. The agent reconnects with backoff when the daemon restarts, and
  the daemon re-sends the stages when it does. A headless server with no
  session at all simply never has an agent, and everything else — modes,
  power supply, the long-sleep cycle — works the same.

## Boundaries

| Concern | Where |
|---|---|
| Decisions (`core::Engine`) | daemon |
| sysfs, logind, RTC, `state.json`, `resume_hook` | daemon (root) |
| Wayland: idle notifications, output power | agent (user) |
| `off_command` / `on_command` | agent (user, `sh -c`, the session's environment) |
| `amperedctl` | any member of `socket_group` |

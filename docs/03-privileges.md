# 03 — Privilege model

## The conflict

The daemon does two things that require incompatible permissions:

- **root:** writing to `/sys/class/backlight`, `/sys/firmware/acpi/platform_profile`,
  `/sys/devices/system/cpu/*/cpufreq`, `/sys/class/rtc/*/wakealarm`.
- **user uid:** connecting to the Wayland socket in `$XDG_RUNTIME_DIR`,
  running `swaymsg`/`hyprctl` (which need the compositor's IPC socket).

## `privilege = "root"` (default): system unit running as root

- `ampered.service` in `multi-user.target`, `User=root`.
- The path to the compositor is given explicitly: `[wayland] runtime_dir`,
  `display`. Root reaches the socket via the owner's `0700` permissions.
- The `display.off_command` and `resume_hook` commands run through `sh -c`;
  for `off_command`/`on_command` the child process does a `setuid/setgid`
  to the owner of `runtime_dir` and sets `XDG_RUNTIME_DIR`,
  `WAYLAND_DISPLAY`, `HOME` — otherwise `swaymsg` won't find its socket.
- The system unit **does not see** `graphical-session.target`, so `idle`
  connects to Wayland with backoff (1s → `reconnect_max_backoff`) and does
  not treat a missing compositor as an error.

Trade-off: a root process parses the Wayland protocol from a user session.
The attack surface is small (we're only a client, only two protocols), but
it is a piece of debt — see ADR-6 — that split mode pays off.

## `privilege = "split"`: `ampered` (root) + `ampered-agent` (user)

- `ampered` stays the root daemon: FSM, IPC, sysfs writes, logind, RTC,
  power supply. It opens **no** Wayland connection and runs no compositor
  helper.
- `ampered-agent` is a small service in the user's
  `graphical-session.target`. It owns everything that needs the session:
  the `ext-idle-notify-v1` watcher, the `wlr` output-power client and the
  `command` DPMS helper, all of them the same code as in root mode, now
  simply running as the user with the session's own `XDG_RUNTIME_DIR` and
  `WAYLAND_DISPLAY`. No `setuid` anywhere.
- The two talk over the daemon's existing IPC socket, not D-Bus (ADR-16):
  the agent connects like any client, sends `{"cmd":"agent"}` and the
  connection turns into a two-way stream — idle events up, stages and
  screen requests down (`11-ipc-cli.md`, "The agent stream"). The daemon
  pushes the `[display]` section to the agent on registration and on every
  reload, so the agent needs no config file of its own.
- Authorization is the socket's: membership in `[general] socket_group`,
  the same right that lets a user run `amperedctl sleep`. One agent at a
  time; a new registration replaces the old stream (a compositor restart
  starts a new agent), and the uid is logged from `SO_PEERCRED`.
- Without an agent the daemon runs exactly as it does without a compositor:
  no idle stages, `degraded: ["agent"]`, the logind `IdleHint` fallback if
  enabled. The agent reconnects with backoff when the daemon restarts, and
  the daemon re-sends the stages when it does.

What the daemon loses in split mode: nothing — `[wayland]` is simply unused.
What it gains: a root process that never parses a byte from the session.

## Alternative: no root at all

- udev gives the `video` group write access to `backlight` (`16-deployment.md`).
- Sleep goes through logind; polkit authorizes the active session by default.
- RTC — `rtcwake` via passwordless `sudoers`.
- But `platform_profile` and cpufreq stay unreachable → modes degrade to
  "timeouts only". Acceptable for some users; not a mode of its own.

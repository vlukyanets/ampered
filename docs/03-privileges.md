# 03 — Privilege model

## The conflict

The daemon does two things that require incompatible permissions:

- **root:** writing to `/sys/class/backlight`, `/sys/firmware/acpi/platform_profile`,
  `/sys/devices/system/cpu/*/cpufreq`, `/sys/class/rtc/*/wakealarm`.
- **user uid:** connecting to the Wayland socket in `$XDG_RUNTIME_DIR`,
  running `swaymsg`/`hyprctl` (which need the compositor's IPC socket).

## v0.1 decision: system unit running as root

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
this is a deliberate piece of debt — see `17-decisions.md`.

## Alternatives (roadmap)

### Split: `ampered` (root) + `ampered-agent` (user)

- `ampered` — stays the root daemon: FSM, IPC, sysfs writes, logind, RTC.
  It exposes a D-Bus API `org.ampered.Priv` (`SetBacklight`, `ApplyMode`,
  `SetWakeAlarm`, …) for the parts that need the user session.
- `ampered-agent` — a tiny user-session service in `graphical-session.target`
  that owns the Wayland connection (idle notifications, DPMS) and forwards
  events/commands to `ampered` over that D-Bus API. A polkit rule
  authorizes the active local session to talk to it.
- Cleaner from a security standpoint, but two binaries, a D-Bus interface, polkit.

### No root at all

- udev gives the `video` group write access to `backlight` (`16-deployment.md`).
- Sleep goes through logind; polkit authorizes the active session by default.
- RTC — `rtcwake` via passwordless `sudoers`.
- But `platform_profile` and cpufreq stay unreachable → modes degrade to
  "timeouts only". Acceptable for some users.

The privilege backend could be made configurable
(`[general] privilege = "root" | "split"`), but not in v0.1.

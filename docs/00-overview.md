# 00 — Overview

## The problem

On Wayland, only the compositor knows "time since last input". Existing
tools fall into two camps:

- **`swayidle`, `hypridle`** — know idle, but don't know about battery,
  CPU modes, RTC. These are scripted triggers, not a power daemon.
- **`logind`, `power-profiles-daemon`, `TLP`** — know about power, but for
  them idle is a crude `IdleHint`, or nothing at all.

Neither solves the "laptop-as-a-server without a UPS" scenario.

## What ampered does

| Area | Capabilities |
|---|---|
| Idle | `ext-idle-notify-v1` timeouts: dim → screen off → sleep |
| Brightness | Smooth dim via `/sys/class/backlight`, restore, accounting for manual changes |
| Modes | `performance` / `balanced` / `powersave` / custom: platform_profile, cpufreq, EPP, timeouts |
| Auto-switching | Mode by AC/battery and charge threshold, with hysteresis |
| Sleep | suspend / hibernate / suspend-then-hibernate via logind, with inhibitors |
| Long sleep | RTC alarm, "sleep → wake → check AC → decide" cycle |
| IPC | Unix socket + `amperedctl` |

## Target scenarios

1. **Regular laptop.** Sway/Hyprland/KDE, more economical and aggressive
   dim on battery, calmer on AC. A manual `presentation` mode with no sleep.
2. **Laptop as a server.** Lid closed, idle disabled, `server.enabled = true`.
   Power is lost → after `grace_period` it enters the long-sleep cycle.
   Power comes back → it wakes at the nearest alarm and brings services up.
3. **Laptop as a server, sometimes used directly.** The lid is opened
   mid-cycle — the cycle is interrupted (`classify_wake == User`), the
   machine stays active.

## What it deliberately does NOT do

- Doesn't handle the lid or power button — that's `logind` (`HandleLidSwitch=`).
- Doesn't implement idle-inhibit — the compositor honors `idle-inhibit-unstable-v1` itself.
- Doesn't fully replace TLP/PPD — only writes the knobs described in the mode.
  Running alongside PPD at the same time is not supported (see `08-power-modes.md`).
- Doesn't manage external monitors over DDC/CI (roadmap).
- Doesn't support GNOME/Mutter in v0.1 (no `ext-idle-notify`; roadmap).
- No GUI/tray.

## Requirements

- Linux ≥ 5.x, systemd-logind.
- A compositor with `ext-idle-notify-v1`: Sway ≥ 1.8, Hyprland, river, niri, KWin ≥ 5.27.
- For DPMS without commands: `wlr-output-power-management-unstable-v1`.
- For long sleep: an RTC with `wakealarm` (`/sys/class/rtc/rtc0/wakealarm`).

# 06 — Screen power (DPMS)

## `wlr` backend (default)

**v0.1 ships `command` and `none` only.** With `backend = "wlr"` the daemon
warns at startup and behaves as `none`; the protocol client arrives in v0.2
(`docs/18-roadmap.md`).

`wlr-output-power-management-unstable-v1`:

```
zwlr_output_power_manager_v1.get_output_power(wl_output) → zwlr_output_power_v1
zwlr_output_power_v1.set_mode(off | on)
events: mode(current), failed
```

- One `zwlr_output_power_v1` per `wl_output` from the registry; new
  outputs (hotplug) are picked up via `wl_registry.global`.
- `failed` from the compositor → `warn!`, the object is destroyed and not
  recreated until the next `Screen(*)`.
- Shares the Wayland connection with `idle::Watcher` (one `Connection`, one `EventQueue`).

Support: wlroots compositors, Hyprland, niri. **Not** KWin, not Mutter.

## `command` backend

Arbitrary commands. Run as the uid that owns `runtime_dir` with
`XDG_RUNTIME_DIR`/`WAYLAND_DISPLAY` set (`03-privileges.md`), 10s timeout.

```toml
[display]
backend = "command"
# Sway
off_command = "swaymsg 'output * power off'"
on_command  = "swaymsg 'output * power on'"
# Hyprland
# off_command = "hyprctl dispatch dpms off"
# on_command  = "hyprctl dispatch dpms on"
# KDE
# off_command = "kscreen-doctor --dpms off"
# on_command  = "kscreen-doctor --dpms on"
```

## `none` backend

The `screen_off` stage exists in the FSM but does nothing. Useful when the
compositor turns the screen off on its own.

## `Engine` behavior

- `Screen(true)` is **always** sent on `Resumed` from sleep, even if the
  screen wasn't turned off: DPMS state after S3 is non-deterministic.
- `Screen(true)` is sent on `ShutdownRequested`.
- Idempotency is handled by `display`: it remembers the last state sent
  and doesn't resend `set_mode(on)`.

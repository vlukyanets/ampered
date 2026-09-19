# 06 — Screen power (DPMS)

## `wlr` backend (default)

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
- Shares the Wayland connection with `idle::Watcher` (one `Connection`, one
  `EventQueue`): the client lives in `idle.rs`, `display.rs` sends it
  `Screen` through `IdleHandle`.
- No `zwlr_output_power_manager_v1` in the registry → `warn!` on the first
  `Screen(*)` and `degraded: ["display"]` for as long as the compositor
  lacks it; the screen state is left alone.

Support: wlroots compositors (Sway, river, labwc), Hyprland. **Not** niri
(it offers `wlr-output-management`, a different protocol — use `command`
with `niri msg action power-off-monitors` / `power-on-monitors`), not
KWin, not Mutter.

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
# niri
# off_command = "niri msg action power-off-monitors"
# on_command  = "niri msg action power-on-monitors"
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

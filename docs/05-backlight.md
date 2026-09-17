# 05 — Backlight

There is no Wayland protocol for backlight. The only portable path is
`/sys/class/backlight/<dev>/{brightness,max_brightness,actual_brightness}`.

## Device selection

`device = "auto"`:

1. Enumerate `/sys/class/backlight/*`.
2. Prefer by `type`: `raw` > `firmware` > `platform`.
   `raw` (`intel_backlight`, `amdgpu_bl0`, `nvidia_0`) is usually the only
   one that actually changes the panel; `acpi_video0` is often a dummy.
3. Several `raw` devices — pick the first alphabetically + `warn!`.
4. None — `backlight: unavailable`; `Dim`/`Undim` are no-ops, `warn!` once.

`device = "intel_backlight"` — explicit.

All values are in **percent of `max_brightness`**. Internally we store raw
units and convert at the boundary.

## Dim

```
dim_to(pct):
  if state == Dimmed: return                     // idempotent
  pre_dim = read(brightness)
  target = max(max_brightness * pct / 100, max_brightness * min_percent / 100)
  written_target = target
  transition(pre_dim → target, duration = transition, step ≈ 16ms)
  state = Dimmed
```

`min_percent` protects against `0` on panels where 0 turns the backlight
off entirely (the user wouldn't even see the cursor).

## Restore

```
restore():
  if state != Dimmed: return
  cur = read(brightness)
  if cur == written_target:                      // nobody touched it
      transition(cur → pre_dim)
  else:
      debug!("brightness changed externally while dimmed, not restoring")
  pre_dim = None; state = Active
```

This is the key point: if the user pressed the brightness keys while the
screen was dimmed, the compositor/brightnessctl already wrote a new
value — restoring `pre_dim` on top of it would be wrong. We compare
against `written_target`, not `actual_brightness`, because some drivers round.

## Transition

- A cancellable task: `resumed` during the fade interrupts it, and
  `restore` proceeds from the current value.
- `transition = "0"` — a single write.
- Writing to sysfs is a `write(2)` of a string with no `\n` issues; the
  file is opened on every write (sysfs files don't like long-lived
  descriptors across suspend).

## Permissions

v0.1 — root, which is sufficient. For split/user mode, see the udev rule
in `16-deployment.md` (the `video` group).

## Edge cases

| Situation | Behavior |
|---|---|
| Lid closed, external monitor | The compositor disabled the built-in output; writing to backlight is harmless |
| Resume from S3, panel at a different brightness (firmware reset it) | `Undim` on `Resumed`: `state != Dimmed` after resetting to `Active` → no-op. Correct: we don't touch what we didn't dim |
| `brightness` read → `EIO` (panel disconnected) | `warn!`, treat as `unavailable` until the next successful read |
| `max_brightness == 0` | the device is rejected during selection |
| Keyboard backlight | not here — via `[modes.*.sysfs]` in `08-power-modes.md` |

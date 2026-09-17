# 08 — Power modes

A mode is a named bundle of **hardware settings** and **idle timeouts**.
The two are combined on purpose: "on battery" means both a frugal CPU
**and** aggressive dim.

## Knobs

| Mode key | Path | Note |
|---|---|---|
| `platform_profile` | `/sys/firmware/acpi/platform_profile` | Check `platform_profile_choices`; missing file → skip with `debug!` |
| `cpu_governor` | `/sys/devices/system/cpu/cpu*/cpufreq/scaling_governor` | On `intel_pstate` (active) only `powersave` / `performance` are available |
| `epp` | `.../cpufreq/energy_performance_preference` | Only `intel_pstate` / `amd_pstate` in active/guided mode; otherwise skip |
| `sysfs` | arbitrary `path = value` pairs | Written in declaration order |

Application order: `sysfs` → `platform_profile` → `cpu_governor` → `epp`.
Custom values go first so that, e.g., `intel_pstate/no_turbo` can be set
before the governor changes.

A missing key means **don't touch it**. There is no inheritance between
modes — this keeps behavior predictable.

Each write error is a separate `warn!` with the path; the mode is still
considered applied (partially) — the FSM is not blocked.

## Built-in modes (from `13-config-example.md`)

| | performance | balanced | powersave | server |
|---|---|---|---|---|
| `platform_profile` | performance | balanced | low-power | low-power |
| `cpu_governor` | performance | schedutil | powersave | powersave |
| `epp` | performance | balance_performance | power | power |
| `dim_after` | 10m | 5m | 2m | 0 |
| `screen_off_after` | 15m | 10m | 4m | 0 |
| `sleep_after` | 0 | 30m | 10m | 0 |

## Auto-switching

The mode is picked from `[auto_mode]` based on the power snapshot: `on_ac`
while AC is online, `on_battery` while on battery and not low, and
`on_low_battery` while on battery and low (per the hysteresis in
`07-power-supply.md`).

- `amperedctl mode <name>` → `Manual(name)`: auto-switching is disabled.
- `amperedctl mode auto` → `Auto`, immediate recalculation.
- `Manual` **does not survive** a daemon restart (no persisted state in v0.1).
- Changing mode means `Command::ApplyMode` + `Command::ReplaceIdleStages`.

## Conflicts with other daemons

| Daemon | Conflict | Recommendation |
|---|---|---|
| `power-profiles-daemon` | both write `platform_profile` | Disable PPD, or don't set `platform_profile` in ampered |
| `TLP` | governor/EPP by AC/BAT | Disable it, or leave only timeouts to ampered |
| `tuned` | same as above | don't combine |
| `swayidle` / `hypridle` | both react to idle | remove them — otherwise dim happens twice |

At startup: check `systemctl is-active` for these units → `warn!` and an
entry in `status.degraded`. The `ampered.service` unit declares
`Conflicts=power-profiles-daemon.service tlp.service`.

## Custom modes

```toml
[modes.presentation]
platform_profile = "performance"
dim_after = "0"
screen_off_after = "0"
sleep_after = "0"

[modes.night]
dim_after = "1m"
screen_off_after = "2m"
sleep_after = "5m"
[modes.night.sysfs]
"/sys/class/leds/platform::kbd_backlight/brightness" = "0"
```

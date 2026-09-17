# 12 — Configuration reference

File: `/etc/ampered/ampered.toml` (`--config` overrides it).
Durations use `humantime`: `30s`, `5m`, `1h30m`, `"0"` = disabled.
Unknown keys are an error (`deny_unknown_fields`).

## `[general]`

| Key | Default | Description |
|---|---|---|
| `log_level` | `"info"` | `error`…`trace`; `RUST_LOG` takes priority |
| `socket` | `"/run/ampered/ampered.sock"` | IPC socket (not re-read on reload) |
| `socket_group` | `"users"` | Group allowed to write to the socket |

## `[wayland]`

| Key | Default | Description |
|---|---|---|
| `runtime_dir` | `"/run/user/1000"` | User's `XDG_RUNTIME_DIR` |
| `display` | `"wayland-1"` | Compositor socket name |
| `reconnect_max_backoff` | `"60s"` | Backoff ceiling |

## `[idle]`

| Key | Default | Description |
|---|---|---|
| `fallback` | `"none"` | `none` / `logind` (coarse IdleHint, sleep only) |

## `[backlight]`

| Key | Default | Description |
|---|---|---|
| `device` | `"auto"` | Name in `/sys/class/backlight/` or `auto` |
| `dim_percent` | `10` | Brightness in `Dimmed`, % of `max_brightness` |
| `transition` | `"400ms"` | Smoothness; `"0"` — instant |
| `min_percent` | `1` | Never go below this |

## `[display]`

| Key | Default | Description |
|---|---|---|
| `backend` | `"wlr"` | `wlr` / `command` / `none` |
| `off_command`, `on_command` | — | For `command` |

## `[modes.<name>]`

Any number of modes. All keys are optional; a missing key means "don't touch it".

| Key | Description |
|---|---|
| `platform_profile` | `low-power` / `balanced` / `performance` (whatever's in `platform_profile_choices`) |
| `cpu_governor` | `schedutil` / `powersave` / `performance` / … |
| `epp` | `power` / `balance_power` / `balance_performance` / `performance` |
| `dim_after`, `screen_off_after`, `sleep_after` | Absolute timeouts from the start of idleness |
| `sysfs` | A table of `"path" = "value"` |

Validation: enabled timeouts must be non-decreasing (`dim ≤ screen_off ≤ sleep`);
each one `≤ 24h`.

## `[auto_mode]`

| Key | Default | Description |
|---|---|---|
| `enabled` | `true` | |
| `on_ac` | `"balanced"` | Must exist in `[modes]` |
| `on_battery` | `"powersave"` | |
| `low_battery_percent` | `20` | Hysteresis +5 |
| `on_low_battery` | `"powersave"` | |

## `[sleep]`

| Key | Default | Description |
|---|---|---|
| `method` | `"suspend"` | `suspend` / `hibernate` / `suspend-then-hibernate` |
| `respect_inhibitors` | `true` | |
| `sleep_retry` | `"2m"` | Recheck after being blocked by an inhibitor |

## `[server]`

| Key | Default | Description |
|---|---|---|
| `enabled` | `false` | |
| `trigger` | `"ac_lost"` | `ac_lost` / `manual` |
| `grace_period` | `"3m"` | Filters out brief power blips |
| `check_interval` | `"20m"` | Wake-up period; validated as `≥ 2m` |
| `awake_window` | `"45s"` | How long to stay awake to check |
| `alarm_slack` | `"90s"` | Tolerance for `classify_wake` |
| `battery_critical_percent` | `10` | |
| `critical_action` | `"hibernate"` | `hibernate` / `poweroff` |
| `rtc_backend` | `"wakealarm"` | `wakealarm` / `rtcwake` |
| `rtc_device` | `"rtc0"` | |
| `resume_hook` | — | Command to run once power returns |

## Hot reload

`SIGHUP` / `amperedctl reload`: the file is re-read and validated in
full. On error, the old config stays in effect, and the error is logged
and returned to the CLI. On success: idle stages are recreated, the
current mode is reapplied. `[general].socket` and `[wayland]` require a restart.

# 13 — Full config example

At implementation time, move this into `examples/ampered.toml` and cover
it with an `include_str!` → parse → validate test.

```toml
[general]
log_level = "info"
socket = "/run/ampered/ampered.sock"
socket_group = "users"

[wayland]
runtime_dir = "/run/user/1000"
display = "wayland-1"

[idle]
fallback = "none"

[backlight]
device = "auto"
dim_percent = 10
transition = "400ms"
min_percent = 1

[display]
backend = "wlr"
# With "wlr" the commands are the fallback for a compositor that lacks the
# protocol; with backend = "command" they are the only way.
# off_command = "swaymsg 'output * power off'"
# on_command  = "swaymsg 'output * power on'"

# ------------------------------------------------------------ modes

[modes.performance]
platform_profile = "performance"
cpu_governor = "performance"
epp = "performance"
dim_after = "10m"
screen_off_after = "15m"
sleep_after = "0"

[modes.balanced]
platform_profile = "balanced"
cpu_governor = "schedutil"
epp = "balance_performance"
dim_after = "5m"
screen_off_after = "10m"
sleep_after = "30m"

[modes.powersave]
platform_profile = "low-power"
cpu_governor = "powersave"
epp = "power"
dim_after = "2m"
screen_off_after = "4m"
sleep_after = "10m"

# Laptop-as-server with the lid closed: idle has no effect.
[modes.server]
platform_profile = "low-power"
cpu_governor = "powersave"
epp = "power"
dim_after = "0"
screen_off_after = "0"
sleep_after = "0"
[modes.server.sysfs]
"/sys/class/leds/platform::kbd_backlight/brightness" = "0"

[auto_mode]
enabled = true
on_ac = "balanced"
on_battery = "powersave"
low_battery_percent = 20
on_low_battery = "powersave"

# ------------------------------------------------------------ sleep

[sleep]
method = "suspend"
respect_inhibitors = true
sleep_retry = "2m"

[server]
enabled = false
trigger = "ac_lost"
grace_period = "3m"
check_interval = "20m"
awake_window = "45s"
alarm_slack = "90s"
battery_critical_percent = 10
critical_action = "hibernate"
rtc_backend = "wakealarm"
rtc_device = "rtc0"
# resume_hook = "systemctl start my-services.target"
```

## Server variant

```toml
[auto_mode]
on_ac = "server"
on_battery = "server"
on_low_battery = "server"

[server]
enabled = true
check_interval = "30m"
critical_action = "hibernate"
resume_hook = "systemctl start docker.service"
```

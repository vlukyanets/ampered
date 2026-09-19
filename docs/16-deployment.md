# 16 — Installation and deployment

## Files

| What | Where |
|---|---|
| `ampered`, `amperedctl` | `/usr/local/bin/` |
| `examples/ampered.toml` | `/etc/ampered/ampered.toml` |
| `contrib/ampered.service` | `/etc/systemd/system/` |
| `contrib/90-ampered-backlight.rules` | `/etc/udev/rules.d/` (user/split mode only) |

```sh
sudo systemctl enable --now ampered
amperedctl status
```

## `contrib/ampered.service`

```ini
[Unit]
Description=ampered power daemon
After=systemd-logind.service dbus.service
Wants=systemd-logind.service
Conflicts=power-profiles-daemon.service tlp.service

[Service]
Type=notify
WatchdogSec=60
ExecStart=/usr/local/bin/ampered --config /etc/ampered/ampered.toml
ExecReload=/bin/kill -HUP $MAINPID
Restart=on-failure
RestartSec=2
RuntimeDirectory=ampered
RuntimeDirectoryMode=0755
StateDirectory=ampered

ProtectSystem=strict
ProtectHome=read-only
ReadWritePaths=/sys/class/backlight /sys/firmware/acpi /sys/devices/system/cpu /sys/class/rtc /sys/class/leds
PrivateTmp=yes
NoNewPrivileges=yes
RestrictAddressFamilies=AF_UNIX AF_NETLINK
SystemCallFilter=@system-service
CapabilityBoundingSet=CAP_SYS_ADMIN CAP_SYS_TIME CAP_SETUID CAP_SETGID CAP_NET_ADMIN

[Install]
WantedBy=multi-user.target
```

`ProtectHome=read-only` — the Wayland socket lives in `/run/user/*`, not
in `$HOME`; but `off_command` might run something from `~/.local/bin` —
read-only is enough for that. `CAP_SETUID/SETGID` is for running commands
as the user (`03-privileges.md`), `CAP_NET_ADMIN` for the `power_supply`
uevent socket (ADR-12). `Type=notify`: the daemon sends `READY=1` once the
socket is listening and the mode is applied, `RELOADING=1`/`READY=1`
around a reload, `STOPPING=1` on the way out, `STATUS=<state>, mode <name>`
on every change, and `WATCHDOG=1` at half of `WatchdogSec` (ADR-14).
Outside systemd there is no `NOTIFY_SOCKET` and all of it is a no-op.

## `contrib/90-ampered-backlight.rules`

Only for running without root:

```
ACTION=="add", SUBSYSTEM=="backlight", RUN+="/bin/chgrp video /sys/class/backlight/%k/brightness", RUN+="/bin/chmod g+w /sys/class/backlight/%k/brightness"
ACTION=="add", SUBSYSTEM=="leds", KERNEL=="*::kbd_backlight", RUN+="/bin/chgrp video /sys/class/leds/%k/brightness", RUN+="/bin/chmod g+w /sys/class/leds/%k/brightness"
```

## Checklist for the server scenario

- [ ] `logind.conf`: `HandleLidSwitch*=ignore`, `IdleAction=ignore`
- [ ] PPD/TLP disabled
- [ ] `cat /sys/class/rtc/rtc0/wakealarm` exists; `.../device/power/wakeup` = `enabled`
- [ ] Hibernate configured, if `critical_action = "hibernate"` (`systemctl hibernate` works manually)
- [ ] `server.enabled = true`, `server` modes set in `[auto_mode]`
- [ ] `resume_hook` brings back up what the sleep hooks stopped
- [ ] Verified manually with `check_interval = "2m"` and `amperedctl watch`

## Distribution packages

Packaging (AUR, nix, deb) comes after v0.2. For now, `cargo install --path .`.

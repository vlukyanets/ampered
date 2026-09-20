# 16 — Installation and deployment

## Files

| What | Where |
|---|---|
| `ampered`, `ampered-agent`, `amperedctl` | `/usr/local/bin/` |
| `examples/ampered.toml` | `/etc/ampered/ampered.toml` |
| `contrib/ampered.service` | `/etc/systemd/system/` |
| `contrib/ampered-agent.service` | `~/.config/systemd/user/` (or `/etc/systemd/user/`) |

```sh
sudo systemctl enable --now ampered
systemctl --user enable --now ampered-agent
amperedctl status
```

Both units are needed on a laptop with a session: the daemon never talks
to the compositor itself (`03-privileges.md`). A headless server runs the
daemon alone.

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
CapabilityBoundingSet=CAP_SYS_ADMIN CAP_SYS_TIME CAP_NET_ADMIN

[Install]
WantedBy=multi-user.target
```

The daemon opens nothing in `/run/user/*` or `$HOME` — the compositor
side is the agent's (`03-privileges.md`) — hence no `CAP_SETUID` and
`ProtectHome=read-only`. `CAP_NET_ADMIN` is for the `power_supply`
uevent socket (ADR-12). `Type=notify`: the daemon sends `READY=1` once the
socket is listening and the mode is applied, `RELOADING=1`/`READY=1`
around a reload, `STOPPING=1` on the way out, `STATUS=<state>, mode <name>`
on every change, and `WATCHDOG=1` at half of `WatchdogSec` (ADR-14).
Outside systemd there is no `NOTIFY_SOCKET` and all of it is a no-op.

## `contrib/ampered-agent.service`

A user unit, `systemctl --user enable --now ampered-agent`; the user must
be in `[general] socket_group`. It carries the idle watcher and the DPMS
backend for the session, and reconnects with backoff when the daemon is
restarted.

```ini
[Unit]
Description=ampered session agent
PartOf=graphical-session.target
After=graphical-session.target

[Service]
ExecStart=/usr/local/bin/ampered-agent
Restart=on-failure
RestartSec=2

[Install]
WantedBy=graphical-session.target
```

`XDG_RUNTIME_DIR` and `WAYLAND_DISPLAY` come from the session; the socket
path is `--socket` (default `/run/ampered/ampered.sock`).

## Checklist for the server scenario

- [ ] `logind.conf`: `HandleLidSwitch*=ignore`, `IdleAction=ignore` — `amperedctl status` reports `lid-switch` / `idle-action` in `degraded` otherwise
- [ ] PPD/TLP disabled
- [ ] `cat /sys/class/rtc/rtc0/wakealarm` exists; `.../device/power/wakeup` = `enabled`
- [ ] Hibernate configured, if `critical_action = "hibernate"` (`systemctl hibernate` works manually)
- [ ] `server.enabled = true`, `server` modes set in `[auto_mode]`
- [ ] `resume_hook` brings back up what the sleep hooks stopped
- [ ] Verified manually with `check_interval = "2m"` and `amperedctl watch`

## Distribution packages

Packaging (AUR, nix, deb) is v0.3. For now, `cargo install --path .`.

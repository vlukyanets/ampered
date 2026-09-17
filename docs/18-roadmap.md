# 18 — Roadmap

## v0.1 — "works on my laptop"
- [ ] `config`: parsing, validation, a test against the example
- [ ] `core`: FSM + table-driven tests (no `LongSleep`)
- [ ] `ipc` + `amperedctl status|mode|modes|watch|reload`
- [ ] `power::supply` (sysfs + polling; udev if there's time)
- [ ] `power::modes` + auto-switching with hysteresis
- [ ] `backlight` with dim/restore
- [ ] `idle`: `ext-idle-notify-v1` with reconnect
- [ ] `logind`: suspend, inhibitors, delay lock, `state.json`
- [ ] `display`: `command` backend (`wlr` in v0.2)
- [ ] `contrib/ampered.service` with `Type=simple`

## v0.2 — "server"
- [ ] `sleep::rtc` (`wakealarm`, `rtcwake`)
- [ ] `sleep::planner`: the full cycle, `classify_wake`, `resume_hook`
- [ ] Hibernate checks at startup
- [ ] udev netlink for `power_supply`
- [ ] `display` `wlr` backend
- [ ] `sd_notify` → `Type=notify`
- [ ] Persist manual mode (ADR-10)

## v0.3 — "security and reach"
- [ ] Split mode: `ampered-agent` + polkit
- [ ] `mutter` idle backend (GNOME)
- [ ] Check `logind.conf` for `HandleLidSwitch` in server mode
- [ ] Packages: AUR, nix flake

## Later / undecided
- DDC/CI for external monitors
- A schedule ("don't sleep 9–18") as an auto-switching input
- Multi-seat
- User notifications before sleep (via `notify-send` as the uid) — or is that out of scope?
- `amperedctl` shell completion, man pages

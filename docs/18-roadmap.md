# 18 — Roadmap

## v0.1 — "works on my laptop"

The checked items are implemented. Tests: `config`, `core`, `backlight` and
`power::supply` are in the tree; the rest is listed in `docs/15-testing.md`.
- [x] `config`: parsing, validation, a test against the example
- [x] `core`: FSM + table-driven tests (no `LongSleep`)
- [x] `ipc` + `amperedctl status|mode|modes|watch|reload`
- [x] `power::supply` (sysfs + polling + udev netlink) with tests on a fake sysfs
- [x] `power::modes` + auto-switching with hysteresis
- [x] `backlight` with dim/restore and `FakeBacklightSink` tests
- [x] `idle`: `ext-idle-notify-v1` with reconnect
- [x] `logind`: suspend, inhibitors, delay lock, `state.json`
- [x] `display`: `command` backend (`wlr` in v0.2)
- [x] `contrib/ampered.service` with `Type=simple`

## v0.2 — "server"
- [ ] `[idle] fallback = "logind"` (coarse `IdleHint` polling)
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

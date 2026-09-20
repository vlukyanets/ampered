# ampered

A power management daemon for Linux laptops under Wayland: idle timeouts
from the compositor, dim/DPMS/sleep, power modes, and long sleep via RTC
for a laptop-as-server that needs to survive a power outage.

It fills the gap between `logind`/`power-profiles-daemon`, which know about
power but not about compositor idle, and `swayidle`/`hypridle`, which know
about idle but not about batteries.

## Status

v0.2 is implemented: config, the state machine, IPC with `amperedctl`,
power source and modes with auto-switching (the manual choice survives a
restart), backlight, Wayland idle with a logind `IdleHint` fallback, sleep
through logind, the `wlr` and `command` DPMS backends, `Type=notify` with
a watchdog, and the long-sleep server cycle: lid closed, power lost, the
machine sleeps with an RTC alarm, wakes to check for power, hibernates or
powers off at a critical battery, and runs `resume_hook` once power is
back. Split mode (`privilege = "split"`) moves the compositor side into
`ampered-agent` in the user session, so the root daemon never touches
Wayland ([`docs/03-privileges.md`](docs/03-privileges.md)). What comes
next is in [`docs/18-roadmap.md`](docs/18-roadmap.md).

A missing subsystem is never fatal: no compositor, no backlight or no D-Bus
leaves the daemon running in a degraded mode, which `amperedctl status`
reports.

## Build and try it

```sh
cargo build --release
cargo run -- --config examples/ampered.toml --check     # validate a config

# a development instance on its own socket, with a pretend battery
RUST_LOG=ampered=debug cargo run -- --config examples/ampered.toml \
    --socket /tmp/ampered.sock --fake-power bat:15
cargo run --bin amperedctl -- --socket /tmp/ampered.sock status
cargo run --bin amperedctl -- --socket /tmp/ampered.sock watch
```

Do not run a development instance next to the system service on the same
compositor — both would dim.

## Install

```sh
cargo install --path .
sudo install -Dm644 examples/ampered.toml /etc/ampered/ampered.toml
sudo install -Dm644 contrib/ampered.service /etc/systemd/system/ampered.service
sudo systemctl enable --now ampered
amperedctl status

# split mode: privilege = "split" in the config, plus the agent in the session
install -Dm644 contrib/ampered-agent.service ~/.config/systemd/user/ampered-agent.service
systemctl --user enable --now ampered-agent
```

Details, including the checklist for the server scenario, are in
[`docs/16-deployment.md`](docs/16-deployment.md).

## Documentation

Start with [`docs/00-overview.md`](docs/00-overview.md) for the goals and
scenarios, [`docs/01-architecture.md`](docs/01-architecture.md) for how the
pieces fit together, and [`docs/12-configuration.md`](docs/12-configuration.md)
for every config key. [`CLAUDE.md`](CLAUDE.md) is the map for anyone writing
code here; [`docs/17-decisions.md`](docs/17-decisions.md) records why things
are the way they are.

License: Unlicense (public domain), see [`LICENSE`](LICENSE).

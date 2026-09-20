# 14 — Code layout

## Tree (target)

- `ampered/`
  - `Cargo.toml`
  - `CLAUDE.md`
  - `README.md`
  - `docs/`
  - `examples/ampered.toml`
  - `contrib/`
    - `ampered.service`
    - `ampered-agent.service`
  - `src/`
    - `main.rs` — bootstrap: config, logging, spawning actors, the Command loop; `idle` and `display` are reached through `AgentLink`
    - `lib.rs` — `pub mod *`, for tests and `amperedctl`
    - `config.rs` — structs + `validate()`
    - `core.rs` — `State`, `Event`, `Command`, `Engine::handle`
    - `idle.rs` — the `ext-idle-notify-v1` Watcher, plus the `wlr-output-power-management` client on the same connection (runs in the agent)
    - `display.rs` — DPMS backends; `wlr` goes through `idle::IdleHandle` (runs in the agent)
    - `backlight.rs` — the sysfs backlight Controller
    - `logind.rs` — the zbus login1 Client
    - `logind_conf.rs` — `logind.conf` + drop-ins, read for the lid-switch and IdleAction checks
    - `notify.rs` — `sd_notify` over `NOTIFY_SOCKET`, no libsystemd
    - `ipc.rs` — the NDJSON server + `Request`/`Response` types
    - `power/`
      - `mod.rs`
      - `supply.rs` — `PowerSource`: sysfs + udev
      - `modes.rs` — the Applier
    - `sleep/`
      - `mod.rs`
      - `rtc.rs` — `RtcAlarm`: Wakealarm, Rtcwake
      - `planner.rs` — server-cycle timers, hooks, `state.json`
    - `timers.rs` — `TimerId` → tokio task, emits `Event::Timer`
    - `agent.rs` — the daemon's end of the agent stream (`AgentLink`), and the stream types
    - `bin/`
      - `amperedctl.rs`
      - `ampered-agent.rs` — the session agent: `idle` + `display` as the user, over the IPC socket

One crate, three binaries. Only the agent links `idle` and `display` into
a running process; the daemon reaches them through `agent::AgentLink`.
They stay in the one library with the IPC types because a workspace would
only split a crate that all three binaries want whole (ADR-16).

## Traits for testability

```rust
trait BacklightSink { fn read(&self) -> Result<u32>; fn write(&self, v: u32) -> Result<()>; fn max(&self) -> u32; }
trait PowerSource   { fn snapshot(&self) -> Result<PowerSnapshot>; }
trait SleepBackend  { async fn suspend(&self, m: SleepMethod) -> Result<()>; async fn inhibitors(&self) -> Result<Vec<Inhibitor>>; }
trait RtcAlarm      { fn set(&self, at: SystemTime) -> Result<()>; fn clear(&self) -> Result<()>; fn pending(&self) -> Result<Option<SystemTime>>; }
trait ModeSink      { fn write(&self, path: &Path, v: &str) -> Result<()>; }
```

Each one has a `Fake*` under `#[cfg(test)]` (or `tests/common/`). `Engine`
depends on none of them — only `main.rs` wires commands to their executors.

## Dependencies

| Crate | Why | Rejected alternative |
|---|---|---|
| `tokio` | runtime, signals, timers, unix sockets | `async-std` — smaller ecosystem |
| `wayland-client` 0.31, `wayland-protocols` (staging), `wayland-protocols-wlr` | idle, DPMS | `smithay-client-toolkit` — overkill for two protocols |
| `zbus` 5 (tokio) | logind | `dbus-rs` — C dependency |
| `serde`, `toml` (`preserve_order`), `serde_json`, `humantime` | config, IPC | `humantime-serde` — `"0"` = disabled needs a custom deserializer anyway |
| `clap` (derive) | CLI | |
| `tracing`, `tracing-subscriber` (env-filter) | logging | `log` — no spans |
| `thiserror`, `anyhow` | errors | |
| `nix` (user, signal, fs, socket, net) | socket group lookup and chown, netlink udev, signals | raw `libc` — less type safety |
| `tempfile` (dev) | sysfs fake tests | |

`udev` (libudev) was rejected in favour of raw netlink — see ADR-12, and
`sd-notify` in favour of a hand-written datagram — see ADR-14.

## Style

- `cargo fmt`, `cargo clippy -D warnings` — in CI.
- One module per file while it stays under ~600 lines; a directory beyond that.
- Only what `main.rs`/tests/`amperedctl` need is public.
- A `//!` doc comment at the top of each module linking to the relevant `docs/NN-*.md`.

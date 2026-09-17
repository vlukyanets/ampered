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
    - `90-ampered-backlight.rules`
  - `src/`
    - `main.rs` — bootstrap: config, logging, spawning actors, the Command loop
    - `lib.rs` — `pub mod *`, for tests and `amperedctl`
    - `config.rs` — structs + `validate()`
    - `core.rs` — `State`, `Event`, `Command`, `Engine::handle`
    - `idle.rs` — the `ext-idle-notify-v1` Watcher
    - `display.rs` — DPMS backends
    - `backlight.rs` — the sysfs backlight Controller
    - `logind.rs` — the zbus login1 Client
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
    - `bin/`
      - `amperedctl.rs`

One crate, two binaries. No workspace is needed until `ampered-agent` shows up.

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
| `serde`, `toml`, `serde_json`, `humantime-serde` | config, IPC | |
| `clap` (derive) | CLI | |
| `tracing`, `tracing-subscriber` (env-filter) | logging | `log` — no spans |
| `thiserror`, `anyhow` | errors | |
| `nix` (user, signal, fs, socket) | setuid for commands, netlink udev | raw `libc` — less type safety |
| `tempfile` (dev) | sysfs fake tests | |

Optional, to be decided at implementation time: `udev` (if raw netlink
turns out to be painful), `sd-notify` (for `Type=notify`).

## Style

- `cargo fmt`, `cargo clippy -D warnings` — in CI.
- One module per file while it stays under ~600 lines; a directory beyond that.
- Only what `main.rs`/tests/`amperedctl` need is public.
- A `//!` doc comment at the top of each module linking to the relevant `docs/NN-*.md`.

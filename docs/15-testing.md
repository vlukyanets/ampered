# 15 — Testing

> **Status:** only the `config` tests are in the tree — parsing, every rule in
> `validate()`, and the check that keeps `examples/ampered.toml` identical to
> the fenced block in `13-config-example.md`. They touch nothing outside the
> process: no sysfs, no sockets, no D-Bus, no subprocesses. The suites for
> `core`, `backlight`, `power`, `display`, `ipc` and `timers` are not in the
> tree. What follows describes the full intended suite, and stands as the
> specification a restored one should work from.

## Levels

| Level | What | How | When |
|---|---|---|---|
| Unit | config parsing/validation, brightness conversion, `classify_wake`, hysteresis | plain `#[test]` | always |
| FSM | the transition table from `02-state-machine.md` and `10-long-sleep-rtc.md` | `TRANSITIONS: &[(State, Event, State, &[Command])]` + scenario tests | always |
| Module tests with fakes | `backlight` with `FakeBacklightSink` (a tempdir with `brightness`/`max_brightness`), `supply` on a tempdir copy of `/sys/class/power_supply` | `tempfile` | always |
| Integration | real Wayland, D-Bus | `--features integration`, `#[ignore]` without the feature | manual / on a dev machine |

## FSM: required scenarios

1. `Active → Dimmed → ScreenOff → Suspending → Sleeping → Resumed → Active`,
   check the commands at each step.
2. Activity from every intermediate state → `Active` with `Undim`/`Screen(true)`.
3. `Idle(Sleep)` with an inhibitor → `StartTimer(SleepRetry)`, then `Timer(SleepRetry)` with no inhibitor → `Suspend`.
4. `AcChanged(false)` in auto → `ApplyMode(on_battery)`; in manual → no `ApplyMode`.
5. Hysteresis: `Battery(20)` → low, `Battery(23)` → still low, `Battery(25)` → not low.
6. The full server cycle: `Grace → Armed → Sleeping → Checking → Armed`, exits by AC, by User, by critical.
7. `sleep_failures ≥ 3` → exit to `Active`.
8. `ShutdownRequested` from `Dimmed`/`ScreenOff` → `Undim`, `Screen(true)`, `Shutdown`.
9. `Reload` with an invalid config → state and mode don't change.
10. Idle stages with `dim` disabled: `Idle(ScreenOff)` from `Active` → `ScreenOff`.

## Backlight

- `dim_to` → `restore` with an unchanged value → brightness is restored.
- `dim_to` → an external write → `restore` → brightness is **not** touched.
- `min_percent` prevents going to 0.
- `restore` while `Active` is a no-op.

## Sleep/RTC

- `Wakealarm::set` writes `0`, then the epoch (verify the order against a fake file).
- `set` with `at < now + 60s` → an error before any write.
- `classify_wake`: within `±slack` → Scheduled, beyond it → User.

## Manual verification on a live machine

```sh
# 1. Idle without sysfs permissions (dim will warn!, but the FSM is visible in watch)
RUST_LOG=ampered=debug cargo run -- --config examples/ampered.toml --socket /tmp/a.sock &
cargo run --bin amperedctl -- --socket /tmp/a.sock watch

# 2. A fake power source to check auto-switching
cargo run -- --socket /tmp/a.sock --fake-power bat:15

# 3. Long sleep on a short interval (careful: this will really put the machine to sleep)
#    check_interval = "2m", awake_window = "20s"; keep `watch` running in another terminal
```

**Warning:** do not run a dev instance alongside the system `ampered.service`
on the same compositor — both will dim.

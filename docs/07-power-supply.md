# 07 — Power source and battery

We read `/sys/class/power_supply/*` directly. UPower is not used: an extra
dependency and an extra D-Bus hop for just two numbers.

## What we read

| Device | Criterion | Fields |
|---|---|---|
| AC | `type == "Mains"` (or `USB` with `online`) | `online` |
| Battery | `type == "Battery"`, `scope != "Device"` | `capacity`, `status`, `energy_full`/`charge_full` |

- Multiple Mains (dock + adapter): `ac = any(online == 1)`.
- Multiple batteries: `capacity` is a weighted average by `energy_full`
  (or `charge_full` if `energy_*` isn't available).
- `scope == "Device"` — mice/headsets, ignored.
- No batteries at all (desktop) → `battery = None`, auto-switching is by AC only.

## Events

Two sources, both required:

1. **udev netlink** (`subsystem == "power_supply"`, `action == "change"`) —
   an immediate reaction to plugging/unplugging the adapter.
2. **Polling every 60s** — a safety net: some ECs don't send a uevent when
   `online` changes, and `capacity` changes without events.

After any event, a full re-read, then a diff against the previous
snapshot → `Event::AcChanged(bool)` only on a real change,
`Event::Battery(pct)` on a change of ≥ 1%.

After `Resumed` — a forced re-read after 2s, then again after 10s: right
after resume the driver can return stale values.

## "Low battery" hysteresis

`low = true` at `capacity ≤ low_battery_percent`,
`low = false` at `capacity ≥ low_battery_percent + 5`.
Without this the mode would flap back and forth right at the boundary.

## Implementation

A `PowerSource { fn snapshot(&self) -> PowerSnapshot }` trait with
`SysfsPowerSource` and `FakePowerSource` (for tests and `--fake-power
ac|bat:NN` in dev mode).

For udev — either the `udev` crate (libudev) **or** raw netlink via `nix`.
Netlink is preferred: no C dependency, and the uevent format is trivial.
The decision gets recorded in `17-decisions.md` at implementation time.

# 11 — IPC and `amperedctl`

## Transport

A Unix stream socket at `[general] socket` (default
`/run/ampered/ampered.sock`), permissions `0660 root:<socket_group>`.
Protocol — **NDJSON**: request line → response line, connection closes.
Exception — `subscribe`: a stream of events until the client disconnects.

## Requests

```json
{"cmd":"status"}
{"cmd":"modes"}
{"cmd":"mode","name":"powersave"}          // or "auto"
{"cmd":"dim"}
{"cmd":"undim"}
{"cmd":"screen","state":"off"}             // "on" | "off"
{"cmd":"sleep"}                            // with inhibitor checks
{"cmd":"sleep","force":true}
{"cmd":"long-sleep"}                       // enter the server cycle
{"cmd":"long-sleep","cancel":true}
{"cmd":"inhibit","what":"idle","why":"build running","ttl":"2h"}
{"cmd":"uninhibit","id":3}
{"cmd":"reload"}
{"cmd":"subscribe"}
```

## Responses

```json
{"ok":true,"data":{...}}
{"ok":false,"error":"no such mode: foo"}
```

### `status`

```json
{
  "state": "Active",
  "mode": {"name": "balanced", "source": "auto"},
  "power": {"ac": true, "battery_percent": 87, "low": false, "batteries": ["BAT0"]},
  "backlight": {"device": "intel_backlight", "percent": 60, "pre_dim": null},
  "idle": {"backend": "ext-idle-notify", "connected": true,
           "stages": {"dim": "5m", "screen_off": "10m", "sleep": "30m"}},
  "sleep": {"method": "suspend", "blocked_by": []},
  "server": {"enabled": true, "phase": "idle", "next_wake": null},
  "inhibitors": [{"id": 3, "what": "idle", "why": "build running", "expires": "2026-09-17T14:00:00Z"}],
  "degraded": []
}
```

`degraded` — a list of subsystems that are `unavailable`: `"wayland"`,
`"backlight"`, `"logind"`, `"hibernate"`, `"rtc"`, `"conflict:tlp"`.

### `subscribe`

Each line is one of:
`{"event":"state","from":"Active","to":"Dimmed"}`,
`{"event":"mode","name":"powersave"}`, `{"event":"power","ac":false}`,
`{"event":"long-sleep","phase":"armed","next_wake":"..."}`.

## Internal inhibitors

`what`: `idle` (ignore all stages) | `sleep` (sleep only). `ttl` is
mandatory, capped at `24h` — a forgotten inhibitor shouldn't live forever.
These are honored **in addition to** logind and the compositor.

## CLI

```
amperedctl status [--json]
amperedctl modes
amperedctl mode <name|auto>
amperedctl dim | undim
amperedctl screen on|off
amperedctl sleep [--force]
amperedctl long-sleep [--cancel]
amperedctl inhibit <idle|sleep> --why "..." [--ttl 1h]
amperedctl uninhibit <id>
amperedctl reload
amperedctl watch                    # = subscribe
amperedctl --socket <path> ...
```

Exit codes: `0` ok, `1` daemon error, `2` failed to connect.
Human-readable output by default, `--json` for the raw response.

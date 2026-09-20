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
{"cmd":"agent"}                            // ampered-agent only, see below
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

`degraded` — a list of subsystems that are `unavailable`, and of
configuration that works against ampered: `"wayland"`, `"backlight"`,
`"display"`, `"logind"`, `"hibernate"`, `"rtc"`, `"conflict:tlp"`,
`"lid-switch"`, `"idle-action"` (the last two from `logind.conf`,
`09-sleep-logind.md`).

### `subscribe`

The first line is the usual `{"ok":true}`, confirming the client is attached.
After that each line is one of:
`{"event":"state","from":"Active","to":"Dimmed"}`,
`{"event":"mode","name":"powersave"}`, `{"event":"power","ac":false}`,
`{"event":"long-sleep","phase":"armed","next_wake":"..."}`.

### The agent stream

`{"cmd":"agent"}` registers the connection as the session agent
(`03-privileges.md`). After the `{"ok":true}` the connection
is a stream in both directions, one JSON object per line.

Daemon → agent (`op`):

```json
{"op":"display","backend":"wlr","off_command":null,"on_command":null}   // on registration and reload
{"op":"stages","dim":"5m","screen_off":"10m","sleep":"30m"}             // "0" = disabled
{"op":"screen","on":false}
```

Agent → daemon (`event`), no replies:

```json
{"event":"idle","stage":"dim"}          // dim | screen_off | sleep
{"event":"activity"}
{"event":"backend","name":"ext-idle-notify","connected":true}
{"event":"display","available":true}
```

`backend` and `display` feed `status.idle` and `status.degraded`
(`"wayland"`, `"display"`); with no agent at all `status.degraded` has
`"agent"`. A second `agent` registration closes the first stream. When the
stream closes the daemon behaves as if the compositor went away:
`Event::IdleBackendChanged(false)`, `Event::Activity`.

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

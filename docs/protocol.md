# The korid protocol

Version 10. This is what `korid` and its clients (the window, `korictl`, and
anything an agent writes) say to each other. The machine-readable form is
[`protocol.schema.json`](protocol.schema.json), generated from the wire types
in `crates/core/src/ipc.rs` and printed by `korictl schema`. Where this page and
the schema disagree, the schema is right and this page is a bug.

Every `json` block below is a sequence of real frames, one per line, and
`crates/cli/tests/schema.rs` validates each line against the schema.

## Transport

- **A Unix socket, nothing else.** The daemon listens on
  `$KORI_SOCKET`, or `kori/kori.sock` under `$KORI_RUNTIME_DIR`, or under
  `$XDG_RUNTIME_DIR` (normally `/run/user/UID/kori/kori.sock`). It opens no
  network socket of any kind.
- **Same user only.** The socket is mode `0600`, and the daemon reads the
  peer's credentials from the kernel (`SO_PEERCRED`) on every connection. A
  peer whose user ID is not the daemon's is answered `peer_rejected` and
  closed.
- **Newline-delimited JSON.** One JSON object per frame, one frame per line,
  UTF-8. A frame is at most **262,144 bytes (256 KiB)** including its newline,
  in both directions.
- **One request, one response, in order.** After the handshake, a connection
  sends a request and reads exactly one response before sending the next. The
  connection stays open for as many requests as the client likes.
- **Three tags, never mixed.** A request carries `"request"`, a response
  `"response"`, a pushed frame `"push"`. A reader can always tell which it
  holds from the first key it checks.

A frame the daemon cannot read (not JSON, an unknown request, a field of the
wrong type, an unknown field in a strict struct, or too long) is answered once
with `malformed` or `frame_too_large` and the connection is closed. Nothing
reaches the hardware.

## Handshake

The first frame on a connection must be `hello`. Anything else is answered
`handshake_required` and closed.

```json
{"request":"hello","protocol_version":10,"client":"claude-code"}
{"response":"hello","protocol_version":10,"daemon_version":"0.1.1","connection":7}
```

- `protocol_version` must equal the daemon's exactly. There is no negotiation
  downward: a mismatch is refused with both numbers and the connection closes.
- `client` is optional. It is 1 to 32 characters of `a-z`, `0-9`, `.`, `_` and
  `-`, and it labels this connection in `status.clients`, in events and in the
  `changed_by` of every revision it causes. It is a label, not proof: the
  process ID next to it comes from the kernel, the name from the client.
- `connection` is this connection's number, unique for the life of the daemon.

```json
{"response":"error","error":"unsupported_protocol","requested":9,"supported":10}
```

## Requests

Thirteen requests besides `hello` and `subscribe`. Writes are the six
requests that change hardware or stored configuration: `save_profile`,
`activate_profile`, `delete_profile`, `apply_program`, `apply_lighting` and
`apply_display`. The daemon revalidates every value of a write against its
safety floors before anything is sent to a device; a client is never trusted.

### `status`

The daemon, every connected client, the access mode, the devices, and the
committed state of each write domain with its revision.

```json
{"request":"status"}
```

The reply is `{"response":"status", ...DaemonStatus}`. The fields a client
acts on most:

```text
access            {"mode":"read_write"} or {"mode":"read_only","conflicts":[...]}
cooling           committed program, revision, changed_by
lighting[]        one entry per channel: committed program, revision, changed_by
display           panel geometry, committed preset, streaming, revision, changed_by,
                  frames_sent, failed_transfers, faulted
clients[]         connection, client, pid, connected_at_unix_ms
started_at_unix_ms  every revision, connection number and event seq restarts with it
```

### `telemetry`

The latest sampling pass. Each section (`kraken`, `system`, `gpu`) carries the
time it was sampled; a value is `{"reading":"valid","value":...}` or
`{"reading":"unavailable","cause":{...}}`, never a made-up zero.

```json
{"request":"telemetry"}
```

### `capabilities`

The versioned capability record of every allowlisted device: what was probed,
from where, and what is unknown and why.

```json
{"request":"capabilities"}
```

### `profiles`

```json
{"request":"profiles"}
{"response":"profiles","active":"Onboard safe","profiles":[]}
```

### `save_profile`

Stores a profile, replacing one with the same name. `Profile` refuses unknown
fields.

```json
{"request":"save_profile","profile":{"name":"Quiet","program":{"mode":"fixed","pump":120,"fan":60},"device":null,"lighting":[],"display":null}}
{"response":"saved","name":"Quiet"}
```

### `activate_profile`

```json
{"request":"activate_profile","name":"Quiet"}
```

The reply is `activated`, with the hardware state and the write it performed.

### `delete_profile`

When the deleted profile is the active one, the safe profile is activated
first and named in `activated_instead`.

```json
{"request":"delete_profile","name":"Quiet"}
{"response":"deleted","name":"Quiet","activated_instead":null}
```

### `apply_program`

Applies a cooling program without saving it. Three modes:

- `{"mode":"onboard"}` writes nothing and leaves the firmware in charge.
- `{"mode":"fixed","pump":P,"fan":F}` holds a duty per channel, 0 to 255.
  The pump is never commanded under **51** (`MIN_PUMP_DUTY`, 20%).
- `{"mode":"curve","pump":{"points":[...]},"fan":{"points":[...]}}` writes 40
  duties per channel, one per degree Celsius of liquid temperature from 20 to
  59, never decreasing, the pump's never under 51.

`expect_revision` is optional: with it, the write is refused with `conflict`
unless the cooling revision is still that one, and nothing is written.

```json
{"request":"apply_program","program":{"mode":"fixed","pump":153,"fan":102},"expect_revision":3}
{"response":"applied","hardware":{"state":"confirmed"},"writes":4,"deduplicated":false,"readback":[{"channel":"pump","mode":"fixed","duty":153,"curve_points_confirmed":null,"mismatch":null},{"channel":"fan","mode":"fixed","duty":102,"curve_points_confirmed":null,"mismatch":null}]}
```

`writes: 0` with `deduplicated: true` means the program was already the one
running.

### `apply_lighting`

One program on one channel of the RGB controller, `off`, `fixed` or `effect`.
`expect_revision` is the revision of that channel.

```json
{"request":"apply_lighting","command":{"channel":1,"program":{"mode":"fixed","color":{"r":255,"g":96,"b":0},"brightness":80}}}
{"response":"lit","channel":1,"program":{"mode":"fixed","color":{"r":255,"g":96,"b":0},"brightness":80},"hardware":{"state":"confirmed"},"writes":1,"deduplicated":false}
```

### `apply_display`

A preset for the Kraken's panel. The preset travels, never the pixels: the
daemon renders it, and keeps streaming it after the client is gone.

```json
{"request":"apply_display","preset":{"mode":"solid","readings":[{"metric":"liquid_temperature","reading":{"r":0,"g":180,"b":255},"text":{"r":255,"g":255,"b":255}},{"metric":"cpu_load","reading":{"r":255,"g":96,"b":0},"text":{"r":255,"g":255,"b":255}}],"background":{"r":0,"g":0,"b":0},"orientation":"deg0","brightness":60,"image":null},"expect_revision":1}
```

The reply is `shown`: the preset, the hardware state, the frames sent, and
`brightness_sent` when the brightness report went out.

### `diagnostics`

A redacted export for an issue report: the capability record with serial
numbers removed, and the event log with secrets scrubbed from free text.

```json
{"request":"diagnostics"}
```

### `events`

A page of the event log, a ring of the newest 512 events. Every event has a
`seq` (1 for the daemon's first, then one more per event) and a `severity`.
With `after_seq`, the events after it, oldest first; without, the newest.
`limit` defaults to 100, at most 512. When the ring no longer holds events after
the cursor, `missed` says how many are gone.

```json
{"request":"events","after_seq":41,"limit":2}
{"response":"events","oldest_seq":1,"latest_seq":43,"missed":0,"events":[{"seq":42,"at_unix_ms":1763460000000,"severity":"info","event":"profile_saved","name":"Quiet","by":{"origin":"client","connection":7,"client":"claude-code","pid":4242}},{"seq":43,"at_unix_ms":1763460001000,"severity":"info","event":"program_applied","hardware":{"state":"confirmed"},"writes":4,"by":{"origin":"client","connection":7,"client":"claude-code","pid":4242}}]}
```

Only state changes are events: a connection is recorded only when it wrote,
subscribed or failed, and the panel stream when its state changes, never once
per frame (`status.display.frames_sent` counts those).

### `history`

Telemetry the daemon kept in memory, in two tiers: **900 one-second slots**
(15 minutes) and **1,440 one-minute slots** (24 hours), for ten metrics. Nothing
is written to disk; it restarts with the daemon.

- `metrics`: any of `cpu_load`, `cpu_temperature`, `gpu_load`,
  `gpu_temperature`, `memory_used`, `liquid_temperature`, `pump_speed`,
  `fan_speed`, `pump_duty`, `fan_duty`. Default: all.
- `window_s`: default 900, at most 86,400.
- `step_s`: under 60 reads the second tier (window at most 900), a whole number
  of minutes reads the minute tier. Default 1 up to 900 s, then 60. The window
  must be a multiple of the step.
- `points`: false by default, which returns only `min`, `avg`, `max`, `last`
  and `samples` per metric. With true, each metric also carries `avg` (and
  `min` and `max` when a point folds several slots), oldest first, `null` where
  nothing was sampled.

A reply carries at most **20,000 numbers** in its point arrays; a larger
request is refused with `history_out_of_range` naming the limit it crossed.
Units: `percent` (loads and duties), `celsius`, `mebibytes` (memory), `rpm`.

```json
{"request":"history","metrics":["liquid_temperature"],"window_s":3,"step_s":1,"points":true}
{"response":"history","end_unix_ms":1763460003000,"step_s":1,"slots":3,"available_s":3,"metrics":{"liquid_temperature":{"unit":"celsius","summary":{"min":31.0,"avg":31.2,"max":31.5,"last":31.5,"samples":3},"avg":[31.0,31.1,31.5]}}}
```

### `subscribe`

Turns the connection into a stream. After the `subscribed` reply, the daemon
only writes push frames and reads nothing; the client unsubscribes by closing
the connection.

- `topics`: any of `telemetry` (a snapshot every `interval_s` seconds),
  `status` (once right away, then whenever it differs from the last one pushed),
  `events` (every event recorded after the cursor).
- `interval_s`: 1 to 60, default 1.
- `after_seq`: push the events after this sequence number first, so a client
  that reconnects misses nothing the ring still holds.

```json
{"request":"subscribe","topics":["status","events"],"interval_s":1,"after_seq":43}
{"response":"subscribed","topics":["status","events"],"interval_s":1}
```

At most **8 subscriptions** are served at once; a ninth is refused with
`subscription_limit` and that connection stays usable for ordinary requests.

## Push frames

```json
{"push":"heartbeat","at_unix_ms":1763460010000}
{"push":"lagged","missed":3}
{"push":"closing","reason":"received SIGTERM"}
{"push":"event","seq":44,"at_unix_ms":1763460011000,"severity":"warning","event":"collector_failed","collector":"gpu","detail":"NVML is not loaded"}
```

- `telemetry` and `status` carry the same body as the `telemetry` and `status`
  replies, under `"push"` instead of `"response"`.
- `event` is one entry of the event log, exactly as an `events` page holds it.
- `heartbeat` fills every 10 seconds without another frame. A subscriber that
  sees nothing for longer knows the daemon is gone.
- `lagged` counts events this subscriber will never see. Each subscriber has a
  queue of 64 frames; when it falls behind, the newest telemetry and status are
  kept and events are counted here instead.
- `closing` is the last frame before the daemon shuts down.

A subscriber that cannot accept a frame within 2 seconds is disconnected. The
daemon never waits on a subscriber while holding its state, so a slow reader
cannot delay a hardware write or a panel frame.

## Revisions and attribution

Each write domain has a revision: cooling, each lighting channel, the display.
It is 1 for the state the daemon restored at startup (attributed to the daemon
itself) and grows by one with each accepted write command; the panel's own
stream does not count. `status` reports it with `changed_by`, who made the last
change:

```text
{"origin":"client","connection":7,"client":"claude-code","pid":4242}
{"origin":"daemon"}
```

A write with `expect_revision` that no longer matches is refused and writes
nothing:

```json
{"response":"error","error":"conflict","domain":"cooling","expected":3,"current":4,"changed_by":{"origin":"client","connection":2,"client":"kori","pid":3100}}
```

Revisions live in memory and restart with the daemon; a changed
`started_at_unix_ms` means every number a client held is void.

## Errors

A refusal is a `response` of `error`, with the code in `error`. `korictl`
prints the same object without the `response` member. Codes:

| `error` | Meaning |
|---|---|
| `unsupported_protocol` | `hello` named another version; `requested` and `supported` are both given. Closes. |
| `malformed` | The frame was not a request this daemon reads, or `hello` named an invalid client. Closes. |
| `frame_too_large` | The frame passed `max_bytes`. Closes. |
| `handshake_required` | A request arrived before `hello`. Closes. |
| `peer_rejected` | The peer is not the daemon's user. Closes. |
| `validation` | A cooling value or a profile was refused; `kind` says which rule. |
| `lighting` | A lighting command was refused; `kind` says which rule. |
| `display` | A panel preset was refused; `kind` says which rule. |
| `read_only` | Writes are refused on this machine right now; `reason` says why. |
| `incompatible` | The profile needs capabilities the hardware does not offer. |
| `profile_not_found` | No profile has that name. |
| `profile_name_unavailable` | The name belongs to the built-in safe profile. |
| `no_device` | No supported device is connected. |
| `io` | A local operation failed; `detail` says which. |
| `conflict` | `expect_revision` no longer matched; nothing was written. |
| `subscription_limit` | Eight subscriptions are already served. |
| `history_out_of_range` | A history request crossed `limit`. |

The `kind` values of `validation`, `lighting` and `display`, and every field of
every error, are in the schema under `$defs.ValidationError`,
`$defs.LightingError` and `$defs.DisplayError`.

```json
{"response":"error","error":"validation","kind":"duty_out_of_range","channel":"pump","value":20,"min":51,"max":255}
{"response":"error","error":"history_out_of_range","limit":"max_values","detail":"864000 values in all; the limit is 20000"}
{"response":"error","error":"subscription_limit","max":8}
```

## Limits

| Limit | Value |
|---|---|
| Frame size, each direction | 262,144 bytes (256 KiB) |
| Concurrent subscriptions | 8 |
| Frames queued per subscriber | 64 |
| Blocked write before a subscriber is closed | 2 s |
| Heartbeat | every 10 s without another frame |
| `subscribe.interval_s` | 1 to 60 s |
| Numbers in one `history` reply | 20,000 |
| Points per metric in one `history` reply | 1,500 |
| History tiers | 900 one-second slots, 1,440 one-minute slots |
| Events held, and most per `events` page | 512 |
| Client name | 32 characters |
| Pump duty floor | 51 of 255 (20%) |

## Changing this protocol

A breaking change bumps `PROTOCOL_VERSION` and is recorded in
[`schema-history.md`](schema-history.md) with the disagreement the bump
prevents. Any change to a wire type regenerates the schema:

```sh
cargo run --quiet -p kori-core --features schema --example protocol_schema > docs/protocol.schema.json
```

`cargo test --workspace` fails with that command in its message when the
committed schema and the types disagree.

[PRD]
# PRD: Agent-First Control

## Changelog

| Version | Date | Author | Summary |
|---------|------|--------|---------|
| 1.0 | 2026-11-18 | Arthur Jean | Initial draft |

## Problem Statement

Kori is driven today by one client, the GPUI window. Everything a program other than that window would need either does not exist or lives inside the window. Measured and read on 2026-11-18 against the working tree at `145462c`:

1. **No scriptable client exists.** `korid` speaks newline-delimited JSON on a 0600 Unix socket (protocol 9), and the only clients are the window and a Rust library (`crates/core/src/client.rs`). An AI agent that runs shell commands has to hand-write socket code to read a temperature.
2. **Telemetry history exists only inside the window.** The daemon keeps the latest snapshot and nothing else (`crates/daemon/src/telemetry.rs:36-45`). The 15-minute series, their averages and their peaks are computed in `crates/app/src/metrics.rs` and never cross the socket. A client that connects has no past, and the window itself starts with an empty chart every time it opens.
3. **Everything is polled.** The protocol is strict request and reply. The window asks for `status` and `telemetry` every second; there is no way to wait for a change.
4. **The event log drowns in one event.** `display_applied` is recorded for every frame the panel stream sends, about once a second. In a live sample it was 421 of 462 entries in a 512-slot ring, so startup events are evicted after about 8.5 minutes. A one-shot command-line client would add two more events per invocation (`client_accepted`, `client_disconnected`) and finish the job.
5. **The protocol is described nowhere but its source.** There is no schema file and no message catalogue, so a client cannot discover the surface.
6. **Two writers cannot see each other.** Any same-user client may write, no event says who wrote, and the window overwrites an external change with its own pending edit (`crates/app/src/cooling.rs:348`). Lighting never reads another client's values and the panel preset is adopted on the first status only (`crates/app/src/shell.rs:247-254`).
7. **The Monitoring screen shows each metric three to four times** (dial, chart, legend, radar). Each render walks the full history about 20 times, the whole window repaints on every sample whichever screen is shown (`crates/app/src/shell.rs:281`), and every pointer move over a chart repaints the window (`crates/app/src/shell/screen/monitoring.rs:272`).
8. **Idle cost is higher than the work requires.** `korid` wakes about 114 times a second because four loops sleep in 50 ms slices. The panel is rendered in full every second and compared afterwards. The window rebuilds the panel preview on every sample even when the Lighting screen is not shown.
9. **A control that does nothing ships in Settings.** The "Export" button has no click handler and the window never sends `diagnostics`.
10. **The memory and cold-start budgets are checked by nobody.** They are written in `docs/from-source.md` and enforced by no script, test or CI step.

**Why now:** the operator wants Kori driven mainly by an agent (Claude Code style) that monitors, collects data and controls the hardware, with the window reduced to a minimal view. The protocol is at version 9 with a single shipped client pair, so this is the cheapest moment to add identity, history and a stream: every later client would otherwise be built on polling and on a history that lives in the wrong process. The open panel-freeze investigation also needs exactly this: a durable, attributable record of stream state that does not depend on a window being open.

## Overview

The daemon becomes the single source of truth for everything observable, and a new command-line client, `korictl`, becomes a first-class way to reach it. `korictl` prints JSON only, has stable exit codes, separates read commands from write commands by name so an agent permission rule can allow one and prompt on the other, and describes itself (`korictl describe`, `korictl schema`).

The protocol moves to version 10 and stays newline-delimited JSON. It gains a client name in `hello`, attribution on every write, a per-domain revision with an optional precondition, a `history` request served from two fixed in-memory tiers (15 minutes at 1 s, 24 hours at 1 min), an `events` request with sequence numbers, and a `subscribe` request that turns a connection into a pushed stream of telemetry, status changes and events. The event log records state changes only.

The window gets thinner. Monitoring keeps the four dials and one temperature chart whose points come from the daemon, so the window stores no history and shows the full 15 minutes the moment it opens. It adopts changes made by other clients, stops working for screens that are not shown, and consumes the stream instead of polling. The daemon waits until its next deadline instead of polling for shutdown, and skips the panel render when the inputs did not change. A script measures the budgets and CI enforces the ones it can measure without a display.

Key decisions: keep NDJSON rather than migrate to Varlink or JSON-RPC (the serde types and the socket tests already exist; only Varlink's "close to unsubscribe" stream semantics are borrowed); optimistic revisions rather than write leases (no lifetime to manage, no stuck lease after a crash); no telemetry on disk (the daemon writes `config.toml` and nothing else; a client that wants a longer record pipes `korictl watch` to a file); no MCP server in this version (a CLI costs an agent no context and composes with `jq`).

## Goals

| Goal | Month-1 Target | Month-6 Target |
|------|---------------|----------------|
| An agent can read, record and control through `korictl` alone | 100% of protocol requests reachable from `korictl`; 0 hand-written socket code in agent workflows | Agent sessions run with the window closed by default |
| The window holds no telemetry history | 0 history buffers in `crates/app`; chart fed by `history` | Unchanged |
| Startup events survive in the event log | Startup events present after 24 h of panel streaming | Unchanged with 1 window and 1 agent connected |
| Daemon idle wakeups | At most 6 per second with no client and no panel stream | Unchanged |
| Window idle cost (release) | CPU at most 1.0% of one core, `RssAnon` at most 81.3 MiB | CPU at most 0.5% with a screen other than Monitoring shown |
| Protocol is discoverable | `docs/protocol.schema.json` committed and checked by a test | Schema covers every later protocol version |

## Target Users

### The agent
- **Role:** an AI coding agent (Claude Code style) running shell commands as the operator's user on the machine that holds the hardware.
- **Behaviors:** runs short commands, parses JSON, verifies a write with a follow-up read, works within command timeouts, and can be restricted by command-prefix permission rules.
- **Pain points:** no command exists; history is unreachable; no schema to learn the payload shapes from; no way to wait for a change without a polling loop; nothing records which client wrote.
- **Current workaround:** a hand-written Python script over the raw socket (the panel-freeze probe at `~/.local/state/kori-probe/watch.py`, which hard-codes protocol 9).
- **Success looks like:** `korictl describe` and `korictl schema` are enough to learn the tool; `korictl history` answers "what happened in the last hour" in one call; `korictl watch --count 10` returns and exits.

### The operator
- **Role:** the owner of the machine, who delegates monitoring and tuning to the agent and opens the window occasionally.
- **Behaviors:** glances at temperatures, changes a curve or the panel a few times a week, wants the machine quiet and the software unnoticeable.
- **Pain points:** a dense Monitoring screen that repeats itself; a window that costs 6.7% of a core in a debug session and repaints when nothing is visible; no way to see that the agent changed something; a dead button.
- **Current workaround:** closing the window to save resources, which also throws away the history.
- **Success looks like:** one glance gives the current state and the temperature trend; an external change shows up with the name of who made it; the window costs nothing when it is not looked at.

## Research Findings

Key findings that informed this PRD:

### Competitive Context
- **liquidctl:** stateless CLI; `--json` exists only on `list`, `initialize` and `status`, so a write returns nothing machine-readable. No history, no events, no published schema. Users bolt on Prometheus exporters for history. Kori differs by keeping history and write results in the daemon.
- **CoolerControl:** daemon with REST and gRPC, token authentication, a `cctv` CLI recommended for scripts. Closest in scope, but HTTP with tokens and a far larger surface. Kori stays on a same-user Unix socket with zero network sockets.
- **OpenRGB:** binary TCP protocol on port 6742 with version negotiation at connect. No JSON, no history.
- **Market gap:** no surveyed tool offers history, pushed events and a published schema together over a local socket.

### Best Practices Applied
- JSON only on stdout, prose on stderr, one JSON object per line for streams; stable documented exit codes; errors carry a machine-readable code; the tool describes itself (community guidance from Arcjet, InfoQ and others; no official standard exists).
- Varlink's streaming model: a call that keeps replying owns its connection, and the client unsubscribes by closing it. The Varlink spec leaves slow readers to the server, so each subscriber gets a bounded queue and a write timeout.
- Fixed-size history tiers indexed by tick rather than pruned by wall-clock time, so a clock step cannot corrupt ordering or drop samples.
- Additive protocol changes only, gated by the existing versioned `hello`.
- `schemars` 1.2.2 (MIT) maps `deny_unknown_fields` to `additionalProperties: false` and supports internally tagged enums. The exact output for nested tagged enums was not confirmed by documentation and is settled by a snapshot test.
- Identity declared in a handshake is a label, not proof: `SO_PEERCRED` proves the user and the process ID only. The safety floors in the daemon remain the real guard.

*Full research sources available in project documentation.*

## Assumptions & Constraints

### Assumptions (to validate)
- GPUI 0.2.2 stops drawing a window that is hidden. Verified in source for X11 (`platform/linux/x11/client.rs:1698-1726` removes the refresh loop when the window is unmapped or fully obscured); inferred for Wayland from frame callbacks. GPUI exposes focus only (`Window::is_window_active`), no "minimized" or "hidden" state. Validated by US-019.
- `schemars` 1.2.2 emits a usable schema for nested internally tagged enums combined with `deny_unknown_fields`. Settled inside US-014, with a hand-written fragment as the fallback.
- A release build of `korid` stays under 32 MiB of `RssAnon`. Only a debug measurement exists (23.9 MiB). Confirmed by the first run of the budget script (US-018).
- Dropping the per-frame `display_applied` event loses no diagnostic value once `status` carries frame counters and stream transitions are still recorded. The panel-freeze investigation depends on the `frames: 0` and fault events, which are kept.
- An agent prefers protocol-shaped JSON payloads for writes over per-field flags, because one schema then describes both the wire and the command line.
- 24 hours at one-minute resolution in memory is enough for agent data collection; anything longer is recorded by the agent itself from `korictl watch`.

### Hard Constraints
- Every rule under "Hardware boundaries" in `AGENTS.md` holds unchanged: the two allowlisted devices, the three write-module pairs, both firmware gates, no kernel driver detached.
- The daemon revalidates every client value. `korictl` is not a trusted input and performs no safety validation of its own.
- Normal operation opens zero network sockets. Neither `korid`, `kori` nor `korictl` requires root.
- The frame rules in `LcdLink::send_frame` (wait for `0x37`, one bulk transfer, every frame twice) are not touched.
- A protocol or config schema bump is recorded in `docs/schema-history.md` with the reason a silent mismatch would be wrong.
- Toolchain 1.97.1 and MSRV 1.90 do not change. A new dependency is checked against GPL-3.0-or-later, recorded next to the dependency, and passes `deny.toml`.
- Every Rust source starts with the two SPDX header lines; a new crate root carries the `cfg_attr(test, allow(...))` lint line.
- UI changes pass the interface checklist in `AGENTS.md` and replace the captures of the screens they change.
- `korid` is stopped before `cargo test --workspace`.

## Quality Gates

These commands must pass for every user story:
- `cargo fmt --all -- --check` - formatting
- `cargo check --workspace --all-targets` - compilation of every target
- `cargo clippy --workspace --all-targets -- -D warnings` - lint policy, including the denied panic-like escapes
- `cargo test --workspace` - unit and socket-level integration tests, run with `korid` stopped

For stories that add or change a dependency:
- `cargo deny check` - licence and advisory policy
- `reuse lint` - every file carries a copyright

For UI stories, additional gates:
- Wayland and X11, 920x640 and 1280x720, 100% and 200% scale, the screen completed by keyboard alone, and a capture committed under `docs/screenshots/` showing the screen as the change leaves it. Screen capture is not available to an agent session, so the capture and the visual matrix are certified in US-020.

## Epics & User Stories

### EP-001: `korictl`, the command-line client

A JSON command-line client that reaches every request the daemon answers today, with an output and exit-code contract an agent can rely on, shipped in the same packages as the other two binaries.

**Definition of Done:** every protocol 9 request is reachable from `korictl`; output, error and exit-code contracts are covered by tests that run the client entry point against a daemon over a real socket; the released packages install `korictl`.

#### US-001: CLI scaffold, output contract and read commands
**Description:** As the agent, I want read commands that print one JSON document and exit with a stable code so that I can monitor the hardware without writing socket code.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** None

**Acceptance Criteria:**
- [ ] A new crate `crates/cli` builds a binary named `korictl` whose only workspace dependency is `kori-core`
- [ ] `korictl status`, `telemetry`, `capabilities`, `profiles` and `diagnostics` each print the daemon's reply as exactly one JSON document on stdout, unchanged in shape, and exit 0
- [ ] `korictl version` prints `{"korictl": <version>, "protocol_version": <n>, "daemon_version": <version or null>}` and exits 0 even when the daemon is unreachable
- [ ] Global options `--socket PATH`, `--timeout SECONDS` (default 5) and `--pretty` (indented JSON) are accepted before the command; `KORI_SOCKET` is honored as today
- [ ] `kori_core::client::Client` accepts a caller-chosen timeout and gains typed wrappers for `diagnostics`, `save_profile`, `activate_profile` and `delete_profile`; the window's behavior is unchanged
- [ ] stdout never carries anything but JSON; every prose diagnostic goes to stderr
- [ ] Given the daemon is not running, when any read command runs, then stdout holds `{"error":"unreachable","path":...}` and the exit code is 3
- [ ] Given a daemon that answers `unsupported_protocol`, then stdout holds that error object and the exit code is 4
- [ ] Given an unknown command or option, then stdout holds `{"error":"usage",...}`, stderr names the offending token, and the exit code is 2
- [ ] Given the daemon stops answering mid-command, then stdout holds `{"error":"timeout"}` or `{"error":"disconnected"}` and the exit code is 5
- [ ] The integration tests invoke the client entry point against a daemon started on a `FakeSysfs` tree over a real socket

#### US-002: Write commands with protocol-shaped payloads
**Description:** As the agent, I want write commands that take the protocol's own JSON payload and print the daemon's outcome so that I can control cooling, lighting and the panel and see whether the hardware confirmed it.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-001

**Acceptance Criteria:**
- [ ] `korictl set cooling <JSON>`, `set lighting <JSON>` and `set display <JSON>` send `apply_program`, `apply_lighting` and `apply_display` and print the daemon's reply, including the hardware outcome
- [ ] `korictl profile activate <NAME>`, `profile save <JSON>` and `profile delete <NAME>` send the matching requests and print the reply
- [ ] A payload argument of `-` reads the JSON from stdin
- [ ] Every command that can change hardware or configuration starts with `set` or `profile`; no other command sends a write request
- [ ] `korictl` performs no validation of duty, curve, color or preset values: a pump duty under the floor reaches the daemon and its typed refusal is printed
- [ ] Given the daemon refuses a write (`validation`, `read_only`, `incompatible`, `no_device`, `profile_not_found`), then stdout holds the daemon's error object unchanged and the exit code is 1
- [ ] Given a payload that is not valid JSON, then stdout holds `{"error":"invalid_payload",...}`, no request is sent, and the exit code is 2
- [ ] Given a daemon in read-only mode, when `korictl set cooling` runs, then the reason from the daemon is printed and no attribute file in the fake tree changes

#### US-003: Package and release `korictl`
**Description:** As the operator, I want `korictl` installed by the same package as `kori` and `korid` so that the agent finds it on every machine Kori is installed on.

**Priority:** P1
**Size:** S (2 pts)
**Dependencies:** Blocked by US-001

**Acceptance Criteria:**
- [ ] `packaging/stage.sh`, `packaging/nfpm.yaml`, `packaging/aur/PKGBUILD`, `packaging/install.sh` and `.github/workflows/release.yml` stage and ship `korictl` next to the other two binaries
- [ ] `packaging/linked-libraries.txt` lists the shared libraries `korictl` names, and `packaging/check-linked-libraries.sh` checks the third binary
- [ ] `scripts/dev.sh` builds `korictl` with the other two binaries
- [ ] `docs/from-source.md` and `docs/releasing.md` name the third binary wherever they enumerate the first two
- [ ] Given a release build, when the ELF `NEEDED` list of `korictl` is read, then it names no GUI, Vulkan, font or NVIDIA library
- [ ] Given `korictl` is missing from the staged layout, when `packaging/stage.sh` runs, then it fails with a message naming the missing binary

---

### EP-002: Protocol 10: identity, events, history and a stream

The daemon gains what a second client needs: a name for each client, attribution and revisions on writes, an event log that keeps what matters, telemetry history and a pushed stream.

**Definition of Done:** `PROTOCOL_VERSION` is 10 with one entry in `docs/schema-history.md` covering every addition; each new request is reachable from `korictl` and covered by a socket-level test; a connection that names itself is visible in `status` and on every write event.

#### US-004: Client identity in `hello` and write attribution
**Description:** As the operator, I want every connection to carry a name and every write to record who made it so that I can tell what the agent changed.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-001

**Acceptance Criteria:**
- [ ] `PROTOCOL_VERSION` becomes 10 and `docs/schema-history.md` gains the entry for 10 stating what a silent mismatch would get wrong
- [ ] `hello` accepts an optional `client` string of 1 to 32 characters from `[a-z0-9._-]`; the reply carries a `connection` number unique for the life of the daemon
- [ ] `status` gains `started_at_unix_ms` and a `clients` list with `connection`, `client`, `pid` and `connected_at_unix_ms` for every open connection
- [ ] `program_applied`, `lighting_applied`, `display_applied` (when a command caused it) and the three profile events carry a `by` field: `{"origin":"client","connection":n,"client":name or null,"pid":n}` or `{"origin":"daemon"}` for a restore at startup
- [ ] The window sends `client: "kori"`; `korictl` sends `korictl` by default and the value of `--client NAME` or `KORI_CLIENT` when given
- [ ] `docs/` states that the name is a self-declared label and that the process ID and user come from the kernel
- [ ] Given a `client` value that is empty, longer than 32 characters or outside the allowed characters, then the daemon answers `malformed` and closes the connection
- [ ] Given a client that sends protocol 9, then the daemon answers `unsupported_protocol` naming 9 and 10, as it does today for any mismatch

#### US-005: An event log that records state changes, with sequence numbers
**Description:** As the agent, I want the event log to keep state changes and number them so that I can read what happened since my last look without the log having been flushed by periodic noise.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-004

**Acceptance Criteria:**
- [ ] Every recorded event carries a `seq` that starts at 1 when the daemon starts and increases by 1 per event
- [ ] A confirmed panel frame that follows a confirmed frame records no event; `display_applied` is recorded when a command or a startup restore applies a preset, when a stream transfer fails for a reason not already recorded, and when the first frame lands after a fault
- [ ] `status.display` gains `frames_sent` and `failed_transfers` (counts since the daemon started) and `last_frame_at_unix_ms`
- [ ] `client_accepted` is recorded when a connection first writes or subscribes; `client_disconnected` is recorded only for such a connection or for a connection closed on a framing error
- [ ] `daemon_stopping` is recorded when the orderly shutdown starts
- [ ] A new request `events` with optional `after_seq` and `limit` (default 100, maximum 512) answers `{"response":"events","oldest_seq":n,"latest_seq":n,"missed":n,"events":[...]}`; `korictl events [--after SEQ] [--limit N]` prints it
- [ ] Given 600 seconds of panel streaming at one frame per second in a test, when `events` is read, then the `daemon_started` event is still present
- [ ] Given 200 one-shot read connections, when `events` is read, then none of them added an event
- [ ] Given `after_seq` older than the oldest retained event, then `missed` holds the number of events that are gone and the reply still succeeds

#### US-006: Telemetry history in the daemon
**Description:** As the agent, I want the daemon to keep recent telemetry and answer with summaries or points so that one call tells me what happened without my having been connected.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** Blocked by US-004

**Acceptance Criteria:**
- [ ] The daemon records one slot per one-second tick for 10 metrics: `cpu_load`, `cpu_temperature`, `gpu_load`, `gpu_temperature`, `memory_used`, `liquid_temperature`, `pump_speed`, `fan_speed`, `pump_duty`, `fan_duty`
- [ ] A slot holds the reading when its section's timestamp advanced since the previous tick and the reading is valid, and null otherwise
- [ ] The second tier keeps 900 slots; every 60 ticks the last 60 slots fold into one slot of a minute tier holding minimum, average, maximum and sample count; the minute tier keeps 1,440 slots
- [ ] Retention is by slot count, never by comparing wall-clock timestamps
- [ ] The request `history` accepts optional `metrics`, `window_s` (default 900), `step_s` (default 1 when `window_s` is at most 900, otherwise 60) and `points` (default false)
- [ ] The reply carries `end_unix_ms`, `step_s`, `slots`, `available_s`, and per metric a `unit` and a `summary` of `min`, `avg`, `max`, `last` and `samples` over the window, computed by the daemon; `summary` is null when the window holds no sample
- [ ] With `points: true`, each metric carries an `avg` array of `slots` numbers or nulls, oldest first, plus `min` and `max` arrays when `step_s` is greater than 1
- [ ] A `step_s` under 60 is served from the second tier and requires `window_s` at most 900; a `step_s` that is a multiple of 60 is served from the minute tier and requires `window_s` at most 86,400
- [ ] `korictl history [--metric M]... [--window DURATION] [--step DURATION] [--points]` prints the reply; durations accept `90s`, `15m`, `24h` or a bare number of seconds
- [ ] The history never reaches disk and `docs/` states that it restarts empty with the daemon
- [ ] Given a request whose `window_s` is not a multiple of `step_s`, whose slot count exceeds 1,500, or whose total number of values exceeds 20,000, then the daemon answers the typed error `history_out_of_range` naming the limit that was crossed
- [ ] Given the Kraken is unplugged for 30 ticks, then the three Kraken metrics hold 30 null slots and the other metrics are unaffected
- [ ] Given the wall clock steps back one hour, then no slot is dropped and the tier still holds one slot per tick
- [ ] Given a daemon that started 40 seconds ago, then `available_s` is 40 and the older slots are null

#### US-007: A pushed stream with `subscribe`
**Description:** As the agent, I want to subscribe to telemetry, status changes and events so that I can wait for a change instead of polling.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** Blocked by US-005

**Acceptance Criteria:**
- [ ] The request `subscribe` accepts `topics` (any of `telemetry`, `status`, `events`), `interval_s` (1 to 60, default 1) and `after_seq`; the reply is `{"response":"subscribed","topics":[...],"interval_s":n}`
- [ ] After that reply the connection carries only frames tagged `"push"`: `telemetry` every `interval_s`; `status` once immediately and then whenever the status differs from the last one pushed on that connection, checked once per tick; `event` for each event recorded after `after_seq`
- [ ] A `heartbeat` push is sent when nothing was written on the connection for 10 seconds, and a `closing` push is sent before the daemon closes the connection on shutdown
- [ ] The client unsubscribes by closing the connection; nothing sent by the client after `subscribed` is processed
- [ ] No push is written while the daemon's state lock is held, and the ticker never waits on a subscriber
- [ ] Each subscriber has a queue of at most 64 frames with only the newest `telemetry` and `status` kept; an overflow of events produces one `{"push":"lagged","missed":n}` frame
- [ ] `korictl watch [--topic T]... [--interval DURATION] [--count N] [--for DURATION]` prints one push per line, flushes each line, exits 0 when the count or duration is reached or when stdout is closed by the reader, and exits 5 after printing `closing` when the daemon stops
- [ ] Given a subscriber that stops reading, when a frame cannot be written within 2 seconds, then the daemon closes that connection, records `client_disconnected` with the reason, and the panel stream and the other subscribers miss no tick
- [ ] Given 8 open subscriptions, when a ninth `subscribe` arrives, then the daemon answers the typed error `subscription_limit` with `max: 8` and keeps the connection usable for other requests
- [ ] Given `after_seq` older than the oldest retained event, then the first push is `lagged` with the number of events that are gone
- [ ] Given `topics` is empty or names an unknown topic, then the daemon answers `malformed`

#### US-008: Write revisions and a conflict precondition
**Description:** As the agent, I want each write domain to carry a revision that I can require so that I never overwrite a change the operator made after my last read.

**Priority:** P1
**Size:** M (3 pts)
**Dependencies:** Blocked by US-004

**Acceptance Criteria:**
- [ ] `status.cooling`, each entry of `status.lighting` and `status.display` carry a `revision` and a `changed_by` in the same shape as the `by` field of events
- [ ] A revision starts at 1 for the state restored at startup and increases by 1 for every accepted write command in its domain; panel frames sent by the tick do not change it
- [ ] `apply_program`, `apply_display` and each lighting command accept an optional `expect_revision`
- [ ] A write without `expect_revision` behaves as today: the last writer wins and is attributed
- [ ] `korictl set cooling|lighting|display` accept `--if-revision N`
- [ ] Activating a profile increases the revision of every domain it changes
- [ ] Given `expect_revision` differs from the current revision, then the daemon answers the typed error `conflict` with `domain`, `expected`, `current` and `changed_by`, and writes nothing to hardware or to `config.toml`
- [ ] Given two clients that read revision 4 and both write with `expect_revision: 4`, then exactly one succeeds and the other receives `conflict` with `current: 5`

---

### EP-003: A minimal Monitoring screen and a thin window

The window stops owning data and stops working when nothing it shows has changed. Monitoring keeps the current state and one trend.

**Definition of Done:** `crates/app` holds no telemetry history and no code for the removed charts; the window adopts changes made by other clients; a sample that changes nothing visible causes no repaint; the window reads the stream instead of polling.

#### US-009: Monitoring reduced to four dials and one temperature chart
**Description:** As the operator, I want Monitoring to show the current state and the temperature trend only so that one glance is enough and the window stores no history.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** Blocked by US-006

**Acceptance Criteria:**
- [ ] Monitoring shows the four dials (CPU, GPU, Memory, Coolant), one line chart of CPU, GPU and coolant temperature over 15 minutes with its legend (current, average, peak), and the device strip for devices that are not ready
- [ ] The chart draws the `avg` array of a `history` reply for the three temperature metrics with `window_s: 900`, `step_s: 5`, and the legend shows the daemon's `summary` values
- [ ] The window requests that history when Monitoring becomes the visible screen, on reconnection, and every 5 seconds while Monitoring is visible; it sends no `history` request while another screen is shown
- [ ] The load area chart, the cooling bars, the headroom radar and the chart hover layer are removed; `components/chart/bars.rs`, `components/chart/radar.rs`, the bucketing and summary helpers, the histories in `crates/app/src/metrics.rs` and the history types in `crates/core/src/telemetry/view.rs` are deleted when nothing else uses them
- [ ] Theme tokens used only by removed charts are deleted with the contrast tests that named them; every remaining color, size and font comes from `crates/app/src/theme.rs`
- [ ] Moving the pointer over Monitoring causes no repaint
- [ ] Rendering Monitoring does not iterate over more than the 180 points per series it draws
- [ ] Given the daemon started less than 5 seconds ago, then the chart area shows the grid and the text "History starts with the background service." with no line drawn
- [ ] Given the `history` request is refused or the daemon is unreachable, then the dials follow the existing disconnected state and the chart area states the reason in operator language
- [ ] Given a window opened against a daemon that has run for an hour, then the chart shows 15 minutes of data on its first render after the reply

#### US-010: The window adopts changes made by other clients
**Description:** As the operator, I want the window to show a change the agent made and never write over it so that two clients cannot fight for the hardware.

**Priority:** P1
**Size:** M (3 pts)
**Dependencies:** Blocked by US-008

**Acceptance Criteria:**
- [ ] When the revision of cooling, of a lighting channel or of the panel advances and `changed_by` is another connection, the window replaces its editor state for that domain with the committed state
- [ ] The panel preset and the lighting programs are adopted on every such change, not only on the first status
- [ ] Every write the window sends carries `expect_revision` set to the revision its editor state was built from
- [ ] After adopting an external change, the affected control shows "Changed by <client name>" until the operator next edits it, using a theme token for its color
- [ ] Given the operator is dragging a curve point when another client changes the cooling program, when the drag ends and the window's write is refused with `conflict`, then the window adopts the committed program, shows who changed it, and sends no further write for the abandoned edit
- [ ] Given `changed_by` has no client name, then the notice reads "Changed by another client"

#### US-011: Remove the dead Export control from Settings
**Description:** As the operator, I want Settings to contain only controls that act so that nothing on screen pretends to work.

**Priority:** P1
**Size:** XS (1 pt)
**Dependencies:** Blocked by US-001

**Acceptance Criteria:**
- [ ] The "Export" button is removed from Settings
- [ ] The Diagnostics block shows one text row: "Run `korictl diagnostics` to export. Serial numbers are redacted."
- [ ] Settings remains completable by keyboard alone and its first tab stop is a control that exists
- [ ] Given the daemon is not running, then the Diagnostics row is unchanged and no disabled control appears in its place

#### US-012: Repaint only when the visible screen changed
**Description:** As the operator, I want the window to do nothing for screens I am not looking at so that leaving it open costs no measurable CPU.

**Priority:** P1
**Size:** M (3 pts)
**Dependencies:** Blocked by US-009

**Acceptance Criteria:**
- [ ] A new sample triggers a repaint only when a value shown on the visible screen changed
- [ ] The write flush triggers a repaint only when it sent a write or changed a control state
- [ ] The panel preview is rebuilt only while the Lighting screen is visible with the LCD row open, and once when that becomes true
- [ ] Animated picture timers stop when the Lighting screen is left and restart on return at the frame the daemon's status implies
- [ ] A test counts repaint requests: with Settings visible, 10 consecutive samples whose status is unchanged cause 0 repaints
- [ ] Given the Cooling screen is visible and only GPU load changes between two samples, then no repaint is requested
- [ ] Given an edit is pending when the visible screen changes, then the write is still flushed within the existing quiet period

#### US-013: The window consumes the stream instead of polling
**Description:** As the operator, I want the window to wait for pushed changes so that it wakes when there is something to show and not otherwise.

**Priority:** P2
**Size:** L (5 pts)
**Dependencies:** Blocked by US-007, US-012

**Acceptance Criteria:**
- [ ] The feed worker holds one subscribed connection for `telemetry` and `status` and one connection for commands
- [ ] The worker no longer sends `status` and `telemetry` every second and no longer waits in 25 ms slices; it blocks until a push or a command arrives
- [ ] Capabilities and profiles are still read once per connection and profiles are re-read after a command that touched them
- [ ] A command's outcome reaches the window in the same order and with the same sequence numbering as today
- [ ] The staleness rule that disables a cooling write on old Kraken telemetry still holds
- [ ] Given the daemon restarts, then the window shows the existing unavailable state within 11 seconds (one missed heartbeat) and both connections are re-established without a restart of the window
- [ ] Given the stream delivers `lagged`, then the window re-reads `status` once and continues
- [ ] Given a `closing` push, then the window shows the unavailable state immediately

---

### EP-004: A published protocol contract

The protocol becomes something a client can learn without reading Rust.

**Definition of Done:** a committed JSON Schema is proven current by a test; `korictl` can print the schema and a description of itself; a protocol document and an agent guide exist under `docs/`.

#### US-014: JSON Schema generated from the wire types
**Description:** As the agent, I want a JSON Schema of every request, reply, push and error so that I can build a valid payload without trial and error.

**Priority:** P1
**Size:** M (3 pts)
**Dependencies:** Blocked by US-007, US-008

**Acceptance Criteria:**
- [ ] `kori-core` gains an optional feature `schema` that derives a JSON Schema for every type that crosses the socket; no shipped binary enables it
- [ ] `docs/protocol.schema.json` is committed and contains the request, reply, push and error types under one document
- [ ] Every numeric field with a unit or a bound states it in its schema description or constraints (duty 0 to 255, temperatures in Celsius, speeds in RPM, the pump floor)
- [ ] `korictl schema` prints the committed document and exits 0 without contacting the daemon
- [ ] The dependency and its licence finding are recorded next to it in `Cargo.toml`
- [ ] A test serializes one example of every request, reply, push and error variant and validates each against the schema
- [ ] Given a wire type changes without the document being regenerated, then `cargo test --workspace` fails with a message that names the command to regenerate it
- [ ] Given a payload with an unknown field in a struct that denies unknown fields, then validation against the schema rejects it

#### US-015: Self-description and protocol documentation
**Description:** As the agent, I want the tool to describe its commands, exit codes and error codes so that I can use it correctly from a cold start.

**Priority:** P1
**Size:** S (2 pts)
**Dependencies:** Blocked by US-014

**Acceptance Criteria:**
- [ ] `korictl describe` prints one JSON document listing every command with its arguments and whether it is a read or a write, the exit codes with their meaning, the error codes, and the environment variables
- [ ] `docs/protocol.md` documents the transport, the handshake, every request with one wire example, the push frames, the error codes and the limits (256 KiB frame, 8 subscriptions, 20,000 history values)
- [ ] `docs/agent-guide.md` states how an agent should drive Kori: read before writing, verify a write with `status`, use `--if-revision`, bound `watch` with `--count` or `--for`, and which command prefixes write
- [ ] `AGENTS.md` references both documents in its Reference section
- [ ] A test fails when a command exists in the parser and is missing from `describe`, or the reverse
- [ ] Given `korictl describe` runs with no daemon, then it still exits 0

---

### EP-005: Idle cost and budgets

The daemon and the tools do no periodic work that produces nothing, and the budgets are measured by a command rather than remembered.

**Definition of Done:** the daemon's idle wakeups are asserted by a test; the panel is not rendered when its inputs did not change; one script measures every budget and CI fails on the ones it can measure.

#### US-016: Deadline-based waits and named threads in the daemon
**Description:** As the operator, I want the daemon to sleep until it has work so that it does not wake the processor a hundred times a second.

**Priority:** P1
**Size:** M (3 pts)
**Dependencies:** None

**Acceptance Criteria:**
- [ ] The three collector loops and the ticker wait until their next deadline in one wait that a stop request interrupts, instead of sleeping in 50 ms slices
- [ ] Every daemon thread has a name starting with `korid-`
- [ ] A test starts the daemon on a fake tree at the real one-second interval with no client and no panel preset, and asserts at most 6 voluntary context switches per second summed over the `korid-` threads across 5 seconds
- [ ] An animation frame still lands within 50 ms of its due time
- [ ] No sleep, poll interval or timeout inside `crates/hardware-linux` changes
- [ ] Given a stop request during a wait, then the collector threads and the ticker exit within 100 ms
- [ ] Given a collector pass that takes longer than the interval, then the next pass starts immediately and no wait is negative or unbounded

#### US-017: Skip the panel render when its inputs did not change
**Description:** As the operator, I want the daemon to render the panel only when what it shows changed so that a steady reading costs nothing.

**Priority:** P1
**Size:** S (2 pts)
**Dependencies:** None

**Acceptance Criteria:**
- [ ] The tick renders a frame only when the preset, the two samples (compared bit for bit) or the panel geometry differ from those of the frame the panel holds
- [ ] The frames sent for a recorded sequence of snapshots are byte-identical to those the previous implementation sends for the same sequence, proven by a test
- [ ] A test counts renders: 10 consecutive ticks with identical samples cause exactly 1 render
- [ ] Brightness changes, the fault backoff (5 s doubling to 60 s) and the animated-picture path behave as before
- [ ] `LcdLink::send_frame` and `crates/hardware-linux/src/lcd.rs` are not modified
- [ ] Given a transfer failed and the stream is in backoff, when the samples have not changed, then the retry still renders and sends once the backoff expires
- [ ] Given the Kraken returns after an absence, then the first tick renders and sends even when the samples equal the last sent ones

#### US-018: A budget script and the CI checks it can run
**Description:** As the operator, I want one command that measures every budget so that a regression is a failed check instead of a number in a document.

**Priority:** P1
**Size:** M (3 pts)
**Dependencies:** Blocked by US-003, US-016

**Acceptance Criteria:**
- [ ] `scripts/budgets.sh` measures on release builds: `korid` `RssAnon`, CPU share and wakeups per second over 60 seconds; `kori` `RssAnon`, CPU share and cold start as the median of 5 launches using `KORI_STARTUP_TRACE` and `KORI_EXIT_AFTER_FIRST_FRAME`; `korictl` binary size and the 95th percentile wall time of `korictl telemetry` over 100 runs
- [ ] The script prints one JSON document with each measurement, its budget and a pass flag, and exits non-zero when any measured budget is exceeded
- [ ] Budgets enforced: `kori` `RssAnon` at most 110 MiB, cold start at most 700 ms, `kori` CPU at most 1.5%; `korid` `RssAnon` at most 32 MiB and at most 6 wakeups per second with no panel stream; `korictl` at most 3 MiB and at most 50 ms
- [ ] The script only reads `/proc` and launches `kori` and `korictl`; it sends no write request and never runs as root
- [ ] `docs/from-source.md` replaces its hand-recorded table with the script's output and the command that produced it, and `docs/releasing.md` lists the script as a step before tagging
- [ ] CI runs the measurements that need no display (the `korictl` size and latency against a daemon on a fake tree) and parses the script with `bash -n`
- [ ] Given no display is available, then every `kori` measurement is reported as `skipped` with the reason, and the exit code reflects only what was measured
- [ ] Given `korid` is not running, then the script exits non-zero with a message naming what to start and measures nothing

---

### EP-006: Operator validation

The claims only a person at the machine can prove: what a hidden window costs, what the screens look like, and that the whole path works against the real Kraken.

**Definition of Done:** each validation below is recorded with its measured values in the repository, and any failed assumption has a follow-up story.

#### US-019: Validate assumption: a hidden window draws nothing
**Description:** As the operator, I want proof that a minimized or covered window costs no drawing on Wayland and on X11 so that the decision not to add a hidden-window mode is grounded.

**Priority:** P1
**Size:** S (2 pts)
**Dependencies:** Blocked by US-013

**Acceptance Criteria:**
- [ ] On a Wayland session and on an X11 session, the release window's CPU share is measured over 5 minutes in three states: visible on Monitoring, minimized, and fully covered by another window
- [ ] The six values and the compositor names are recorded in `docs/from-source.md`
- [ ] The assumption holds when the minimized and covered values are at most 0.2% of one core on both sessions
- [ ] Given the assumption fails on either session, then a follow-up story is added to this PRD for a reduced cadence driven by `observe_window_activation`, and this story is still closed with the measurements recorded

#### US-020: Hardware and interface validation on the owned Kraken
**Description:** As the operator, I want the agent path and the changed screens exercised on the real hardware so that what ships is what was observed.

**Priority:** P1
**Size:** M (3 pts)
**Dependencies:** Blocked by US-013, US-017, US-018

**Acceptance Criteria:**
- [ ] The firmware of both devices is recorded before any write, and the session stays inside the allowlist
- [ ] An agent session using only `korictl` reads status, records 10 minutes with `watch`, reads `history`, applies a fixed cooling program with `--if-revision`, verifies it with `status`, and restores the previous program
- [ ] The window, open during that session, shows the agent's change with the agent's client name
- [ ] After 24 hours of panel streaming, `korictl events` still returns the startup events, and `status.display.frames_sent` matches the panel having updated
- [ ] `scripts/budgets.sh` passes on the machine and its output is committed in `docs/from-source.md`
- [ ] Monitoring and Settings pass the interface matrix: Wayland and X11, 920x640 and 1280x720, 100% and 200% scale, keyboard alone
- [ ] `docs/screenshots/monitoring.png` is replaced by a capture of the new screen at the size the AppStream metadata declares, and no capture shows a removed chart
- [ ] Given a pump duty under the floor is sent with `korictl set cooling`, then the daemon refuses it and the pump speed read from the hardware does not change
- [ ] Given the session ends for any reason, then the cooling program, the lighting and the panel preset in place before it are restored

---

## Functional Requirements

- FR-01: The system must provide a binary `korictl` that reaches every request of the protocol and prints the daemon's reply as JSON on stdout.
- FR-02: `korictl` must exit 0 on success, 1 when the daemon refuses the request, 2 on a usage or payload error, 3 when the daemon is unreachable, 4 on a protocol version mismatch and 5 when the connection is lost or times out.
- FR-03: On any failure `korictl` must print one JSON object with an `error` field on stdout, using the daemon's own error object when the daemon produced it.
- FR-04: Only commands starting with `set` or `profile` may send a request that changes hardware or configuration.
- FR-05: The system must NOT validate safety limits in `korictl`; the daemon revalidates every value.
- FR-06: The protocol must be version 10, must stay newline-delimited JSON with a 256 KiB frame limit, and must refuse any other version at `hello` with both versions named.
- FR-07: When a client sends a `client` name in `hello`, the daemon must expose it in `status.clients` and on every event and revision caused by that connection.
- FR-08: The daemon must keep telemetry history in memory in two tiers (900 one-second slots, 1,440 one-minute slots) for 10 metrics, and must answer `history` with summaries and, on request, points.
- FR-09: The daemon must NOT write telemetry history to disk.
- FR-10: The daemon must number events, record display events on state change only, record connection events only for connections that wrote, subscribed or failed, and answer `events` from a given sequence number.
- FR-11: When a client sends `subscribe`, the daemon must push telemetry at the requested interval, status on change and events as they are recorded, until the client closes the connection.
- FR-12: The daemon must NOT hold its state lock while writing to a subscriber and must close a subscriber that cannot accept a frame within 2 seconds.
- FR-13: The daemon must refuse a ninth concurrent subscription with a typed error.
- FR-14: Each write domain must carry a revision; when a write carries `expect_revision` and it differs, the daemon must refuse with `conflict` and write nothing.
- FR-15: The window must hold no telemetry history and must draw its temperature chart from `history`.
- FR-16: The window must adopt a change made by another connection and must NOT resend an edit refused with `conflict`.
- FR-17: The window must NOT request a repaint for a sample that changes nothing on the visible screen.
- FR-18: The system must publish `docs/protocol.schema.json`, generated from the wire types, and a test must fail when it is out of date.
- FR-19: `korictl describe` and `korictl schema` must work with no daemon running.
- FR-20: The daemon must render a panel frame only when the preset, the samples or the panel geometry changed, and the frames sent must be identical to those sent before this change.
- FR-21: The system must provide `scripts/budgets.sh`, which measures every budget, prints JSON and fails when a budget is exceeded.

## Non-Functional Requirements

- **Performance:** `korictl telemetry` completes in at most 50 ms at the 95th percentile on a release build; the daemon answers a `history` request of 20,000 values in at most 5 ms at the 95th percentile; a push is written within 100 ms of the tick that produced it; `korid` makes at most 6 voluntary context switches per second with no client and no panel stream; the window uses at most 1.0% of one core with Monitoring visible and at most 1.5% in any state; window cold start is at most 700 ms as the median of 5 launches.
- **Memory:** telemetry history uses at most 1 MiB; `korid` `RssAnon` is at most 32 MiB; `kori` `RssAnon` is at most 110 MiB and does not exceed the 81.3 MiB measured before this work; `korictl` is at most 3 MiB on disk.
- **Security:** the socket stays mode 0600 with the peer user checked on every connection; 0 network sockets are opened by any of the three binaries; 0 binaries require root; 100% of write requests are revalidated by the daemon; 100% of accepted writes are attributed to a connection, a process ID and, when given, a client name.
- **Accessibility:** Monitoring and Settings are completable by keyboard alone; every color on the changed screens comes from a theme token covered by the existing contrast tests; layouts hold at 920x640 and 1280x720 at 100% and 200% scale.
- **Scalability:** at most 8 concurrent subscriptions, each with a queue of at most 64 frames; at least 16 concurrent one-shot clients complete with 0 lost or reordered commands; the event ring holds 512 events.
- **Reliability:** a stalled subscriber is disconnected within 2 seconds and causes 0 missed ticks for the panel stream; a subscriber detects a dead daemon within 11 seconds through the 10-second heartbeat; the collector threads and the ticker stop within 100 ms of a stop request; history survives a wall-clock step of any size with 0 dropped slots.

## Edge Cases & Error States

| # | Scenario | Trigger | Expected Behavior | User Message |
|---|----------|---------|-------------------|--------------|
| 1 | Empty history | The daemon started seconds ago | `available_s` reports the real depth, older slots are null, the chart shows its grid | "History starts with the background service." |
| 2 | Chart waiting for its first reply | Monitoring opened, `history` not yet answered | Dials render from telemetry; chart area shows the grid with no line | None |
| 3 | Daemon unreachable from the CLI | `korid` stopped or wrong socket | One error object on stdout, exit code 3 | `{"error":"unreachable","path":"..."}` |
| 4 | Version mismatch | A protocol 9 client (including the old probe script) meets a protocol 10 daemon | Refused at `hello` with both versions; `korictl` exits 4 | `{"error":"unsupported_protocol","requested":9,"supported":10}` |
| 5 | Slow subscriber | A reader stops consuming `watch` output | Newest telemetry and status kept, events counted as missed, connection closed after a 2 s blocked write | `{"push":"lagged","missed":n}` |
| 6 | Too many subscribers | A ninth `subscribe` | Typed refusal, connection stays usable | `{"error":"subscription_limit","max":8}` |
| 7 | Concurrent writers | The window and the agent change cooling within the same second | With a precondition, one wins and the other gets `conflict`; without, the last writer wins and is attributed | "Changed by <client name>" |
| 8 | History request too large | 10 metrics over 24 h with points | Typed refusal naming the crossed limit | `{"error":"history_out_of_range",...}` |
| 9 | Device unplugged | The Kraken disappears | Its history slots are null, the status push reports it absent, the panel stream stops without a per-tick event | Existing device-absent state |
| 10 | Read-only access | No write permission on the hwmon attributes | Reads work; writes are refused with the daemon's reason; `korictl` exits 1 | The daemon's `read_only` reason |
| 11 | Wall-clock step | NTP correction or resume from suspend | No slot dropped; `end_unix_ms` follows the new clock; older slot times are approximate | None |
| 12 | Interrupted stream | `korictl watch \| head -1`, or Ctrl+C | Exit 0 on a closed stdout, no broken-pipe message, the daemon frees the subscription | None |
| 13 | Daemon restart during a stream | `systemctl --user restart korid` | `closing` push, `korictl watch` exits 5, the window reconnects and refetches history | Existing unavailable state |
| 14 | Invalid write payload | Malformed JSON or an unknown field | Malformed JSON is refused by `korictl` (exit 2); an unknown field is refused by the daemon (exit 1) | `{"error":"invalid_payload",...}` or the daemon's error |
| 15 | Event cursor too old | `after_seq` precedes the ring | The reply succeeds and reports how many events are gone | `"missed": n` |
| 16 | Conflict during a drag | The agent writes while the operator drags a curve | The window's write is refused, the committed program is adopted, the edit is dropped | "Changed by <client name>" |

## Risks & Mitigations

| # | Risk | Probability | Impact | Mitigation |
|---|------|------------|--------|------------|
| 1 | A subscriber blocks the state lock and delays a hardware write or a panel frame | Med | High | Pushes are written by the subscriber's own thread outside the lock, with a bounded queue and a 2 s write timeout; a test stalls a reader and asserts no missed tick (US-007) |
| 2 | Removing the per-frame display event hides a frozen panel | Low | High | Fault and recovery transitions stay recorded; `status.display` gains frame and failure counters; validated over 24 h in US-020 |
| 3 | An agent steered by hostile input writes to the hardware | Med | Med | Safety floors are enforced in the daemon; every write is attributed; write commands share a prefix so a permission rule can require approval; cooling and lighting already enforce a minimum interval between writes |
| 4 | `schemars` output is wrong for nested tagged enums | Med | Low | Every example frame is validated against the schema in a test; a hand-written schema fragment replaces a derive that fails (US-014) |
| 5 | Moving history changes what the chart shows | Med | Low | The slot rule reuses the "section timestamp advanced" rule from `crates/app/src/metrics.rs`; a test feeds both a recorded snapshot sequence |
| 6 | GPUI keeps drawing a hidden window on Wayland | Low | Med | Measured in US-019; fallback story for a reduced cadence when the window is inactive |
| 7 | The protocol bump breaks the existing probe script and any third-party client | High | Low | The mismatch is refused loudly at `hello`; `korictl watch` replaces the script; `docs/schema-history.md` records the bump |
| 8 | Idle-wakeup and latency tests are flaky on shared CI runners | Med | Med | Thresholds leave margin (6 per second against 4 expected); the timing budgets that need quiet hardware run from the script, not from `cargo test` |
| 9 | Scope: 20 stories and 64 points across protocol, CLI, window and packaging | Med | Med | Epics are ordered so EP-001 and EP-002 deliver the agent path alone; EP-003 to EP-005 are independent of each other after EP-002 |
| 10 | Budgets cannot be measured in CI for the window | High | Low | CI enforces the daemon and CLI budgets; the script is a release step and its output is committed |

## Non-Goals

What this version explicitly does NOT include:

- **An MCP server.** A CLI costs an agent no context and composes with shell tools. An MCP wrapper over `korictl` can be considered once the CLI contract has been used for a while.
- **Write leases, per-client authorization or a "window-only writes" switch.** A handshake name is a label, not proof, so authorization built on it would be decorative. Revisions and attribution are the v1 answer.
- **Telemetry or events on disk.** The daemon writes `config.toml` and nothing else. A longer record is the client's job (`korictl watch >> file`).
- **Remote access.** No TCP, HTTP or network socket of any kind.
- **A dry-run or validate request.** An agent verifies with a read after the write, and the daemon's floors make a refused write harmless. Revisit if agents ask for it.
- **Human-formatted output from `korictl`** (tables, colors). `--pretty` indents JSON and nothing more; the window is the human interface.
- **Compatibility with protocol 9 clients.** The handshake stays a strict equality check.
- **Changing the HID or USB transfer path:** the 2 ms answer polling in `hid.rs`, the timeouts in `usbfs.rs` and the frame rules in `lcd.rs` are untouched, even where they cost wakeups during a panel stream.
- **Comparing quantized readings to skip more panel renders.** Only bit-identical inputs skip a render, because that is provably equivalent.
- **Window budgets in CI.** The runners have no display and no GPU; a software renderer would measure a different process.
- **New hardware, new firmware entries or new capabilities.**

## Files NOT to Modify

- `crates/hardware-linux/src/control.rs`, `rgb.rs`, `lcd.rs`: the only modules that write to hardware. No story adds a write path, and `LcdLink::send_frame` keeps its three frame rules.
- `crates/hardware-linux/src/gates.rs`, `rgb::VALIDATED_FIRMWARE`, `lcd::VALIDATED_FIRMWARE`: filled only from an operator-watched write probe.
- `crates/hardware-linux/src/usbfs.rs` and `hid.rs`: `Usbfs::claim` must keep refusing a driver-held interface; timeouts and poll intervals stay as measured.
- `ALLOWLIST` in `crates/core/src/lib.rs`: extended only with capability evidence from real hardware.
- The safety floors in `crates/core/src/profile.rs` (`MIN_PUMP_DUTY` and the rest): enforced before the write, never relaxed.
- The root refusal in `crates/daemon/src/main.rs`.
- `crates/daemon/src/rgb_probe.rs`, `lcd_probe.rs` and the probe records under `docs/*-write-probe-*.json`: evidence, not code under change.
- `docs/capability-record.json`: re-recorded only by `--probe` on the machine.
- `rust-toolchain.toml` and `rust-version` in `Cargo.toml`: deliberate decisions.
- `[workspace.lints.clippy]` in `Cargo.toml` and `clippy.toml`: the lint policy is carried into the new crate, not changed.
- `deny.toml`: a licence is added only after it has been read; none is expected, since `schemars` (MIT) is already in the graph.

`crates/daemon/src/cooling.rs`, `lighting.rs` and `display.rs` are modified only where a story says so (attribution, revisions, the render skip), and remain the only callers that serialize writes.

## Technical Considerations

Frame as questions for engineering input, not mandates:

- **Architecture:** Where does the CLI logic live so the socket tests can call it? Recommended: `crates/cli` exposes `run(args, stdout, stderr) -> exit code` as a library with a thin `main`, and the daemon's integration tests call it in-process through the existing `Harness`. Alternative: move the harness behind a `testing` feature of `kori-daemon`. Engineering to confirm which keeps the dependency direction clean.
- **Architecture:** How do subscriber threads learn of new data without the ticker waiting on them? Recommended: a small hub (generation counter and condition variable) the ticker and the event recorder notify after releasing the state lock; each client thread that subscribed waits on it and serializes outside the lock. Engineering to confirm the lock ordering.
- **Data Model:** History as a fixed array of `Option<f32>` per metric per tier (about 72 KiB for the second tier, about 230 KiB for the minute tier) versus reusing `History` from `crates/core/src/telemetry/view.rs`. Trade-off: the existing type prunes by timestamp and drops a point stamped before the newest, which is the clock-step failure this PRD removes.
- **Data Model:** Where do revisions and `changed_by` live? Recommended: in the daemon state next to each executor's committed value, in memory only, reset at start (`status.started_at_unix_ms` disambiguates).
- **API Design:** Push frames use a third tag, `"push"`, beside `"request"` and `"response"`. Should `subscribed` connections reject later bytes with an error instead of ignoring them? Recommended: ignore, as Varlink does, so the server thread never has to read and write at once.
- **API Design:** Is one protocol bump enough for the whole PRD? Recommended: US-004 moves to 10 and later stories extend the same `docs/schema-history.md` entry as long as no tag has shipped version 10; a wire change after such a tag bumps again.
- **Dependencies:** Argument parsing: `lexopt` (MIT, no dependencies, not yet in the graph) recommended for strict handling of `--opt=value` and `--`. Alternatives: `pico-args` 0.5.0 (MIT, no dependencies, already in `Cargo.lock`) or a hand-written matcher like the one in `korid`. `clap` is rejected for its size in a binary meant to stay under 3 MiB.
- **Dependencies:** `schemars` 1.2.2 as an optional dependency of `kori-core` behind `schema`, enabled only by a dev-dependency so `cargo build --release --workspace` never compiles it into `korid` or `korictl`. Validating examples needs a JSON Schema validator as a dev-dependency (`jsonschema`, MIT, default features off): engineering to confirm its licence graph passes `deny.toml`, or to validate with a hand-written structural check instead.
- **Migration:** No data migration. `config.toml` gains no field, so `CONFIG_SCHEMA_VERSION` stays at 4. Rollback is reverting to the previous tag; a protocol 9 window against a protocol 10 daemon is refused at `hello` rather than misbehaving. The probe script under `~/.local/state/kori-probe/` hard-codes protocol 9 and is replaced by `korictl watch --topic status --topic events`.
- **Testing:** The idle-wakeup test reads `voluntary_ctxt_switches` per thread from `/proc/self/task/*/status`, filtered by the `korid-` name. Is 5 seconds enough for a stable count on a shared runner, or should the threshold be a ratio against the expected 4 per second?

## Success Metrics

| Metric | Baseline (current) | Target | Timeframe | How Measured |
|--------|-------------------|--------|-----------|-------------|
| Protocol requests reachable from a shipped CLI | 0 of 12 | 100% of protocol 10 requests | Month-1 | `korictl describe` compared with the request enum in a test |
| `korid` wakeups per second, no client, no panel stream | about 114 (debug build, window connected, panel streaming, 2026-11-18) | at most 6 | Month-1 | The idle test in `cargo test`; `scripts/budgets.sh` |
| Share of the event ring taken by `display_applied` | 91% (421 of 462) | under 5% after 24 h of streaming | Month-1 | `korictl events` in US-020 |
| Telemetry history available to a new client | 0 s | 15 min at 1 s and 24 h at 1 min | Month-1 | `korictl history` after 24 h of uptime |
| History buffers held by the window | 8 series of about 900 points | 0 | Month-1 | Code review of `crates/app`; `metrics.rs` histories deleted |
| Window CPU at idle (release) | 1.10% (documented); 6.7% in a debug session | at most 1.0% on Monitoring, at most 0.5% elsewhere | Month-1 / Month-6 | `scripts/budgets.sh` |
| Window `RssAnon` at idle (release) | 81.3 MiB | at most 81.3 MiB | Month-1 | `scripts/budgets.sh` |
| `korid` `RssAnon` | 23.9 MiB (debug build) | at most 32 MiB (release), tightened after the first measurement | Month-1 | `scripts/budgets.sh` |
| Budgets enforced by a command | 0 of 4 | 100% by the script, daemon and CLI budgets also in CI | Month-1 | `scripts/budgets.sh` exit code; CI job |
| Chart modules in the window | 5 files, 1,657 lines | 3 files (`mod`, `radial`, `series`) | Month-1 | Line count |

## Open Questions

- **Is removing the chart hover acceptable?** Operator to answer before US-009 starts. Keeping it means isolating the chart in its own cached view so a pointer move repaints the chart alone.
- **Should the agent be lockable out of writes from the window?** Operator, before any second version. It would need identity stronger than a declared name, for example the executable path read from `/proc/<pid>/exe`.
- **Is 24 hours at one-minute resolution the right second tier?** Operator, before US-006. It costs about 230 KiB; 7 days at 5 minutes would cost about 320 KiB.
- **What is the right `korid` memory ceiling?** Engineering, after the first `scripts/budgets.sh` run in US-018. The 32 MiB ceiling comes from a debug measurement.
- **Should `korictl watch` be unbounded by default?** Engineering, before US-007. An agent's command has a timeout, so a default bound (for example 60 seconds) would be safer for agents and surprising for people used to `tail -f`.
- **If US-019 fails, is a slower cadence for an unfocused window acceptable?** Operator, after US-019. A visible but unfocused window would then update every few seconds instead of every second.
[/PRD]

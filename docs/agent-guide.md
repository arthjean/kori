# Driving Kori from an agent

How a program that runs shell commands (an AI coding agent, a script, a cron
job) should monitor and control Kori through `korictl`. The wire protocol
underneath is in [`protocol.md`](protocol.md); this page is about using the
command line well.

## Start cold

```sh
korictl describe            # every command, its arguments, read or write, exit and error codes
korictl schema              # the JSON Schema of every payload and reply
korictl version             # this client, the protocol, and the daemon if it answers
```

The first two need no daemon. A payload operand's entry in `describe` carries a
`schema` pointer such as `#/$defs/CoolingProgram`; read that definition with
`korictl schema | jq '."$defs".CoolingProgram'` before building the payload.

Name yourself, so the operator can see who changed what. Every write you make
is then shown with this name in `status`, in the event log and in the window:

```sh
export KORI_CLIENT=claude-code      # or --client claude-code before the command
```

## Which commands write

**Only commands starting with `set` or `profile` can change the hardware or the
stored configuration.** Every other command is a read:

| Writes | Reads |
|---|---|
| `korictl set cooling` | `korictl status` |
| `korictl set lighting` | `korictl telemetry` |
| `korictl set display` | `korictl history` |
| `korictl profile save` | `korictl events` |
| `korictl profile activate` | `korictl watch` |
| `korictl profile delete` | `korictl capabilities`, `profiles`, `diagnostics`, `version`, `describe`, `schema` |

So a permission rule that allows `korictl` but asks before any command
beginning `korictl set` or `korictl profile` lets an agent observe freely and
never write unseen. Global options come before the command
(`korictl --pretty status`), so they never hide the prefix.

## Read before you write

1. `korictl status` first. Check `access.mode`: `read_only` means every write
   will be refused, and `access.conflicts` says why. Report it; do not try to
   work around it, and never run anything as root.
2. Note the revision of the domain you are about to change:
   `korictl status | jq .cooling.revision`, `.lighting[N].revision` for a
   lighting channel, `.display.revision` for the panel.
3. Keep the current value (`.cooling.committed`, `.lighting[N].committed`,
   `.display.committed`) if you intend to restore it afterwards.

## Write with `--if-revision`

Pass the revision you read, so a change someone else made in between (the
operator in the window, another agent) is never silently overwritten:

```sh
korictl set cooling --if-revision 3 '{"mode":"fixed","pump":153,"fan":102}'
echo '{"channel":1,"program":{"mode":"off"}}' | korictl set lighting --if-revision 2 -
```

The payload is the protocol's own JSON for that request, inline or from stdin
with `-`. If the domain moved on, the command exits 1 with
`{"error":"conflict","expected":3,"current":4,"changed_by":{...}}` and nothing
was written. Read `status` again, find out who changed it and why, and decide
again; do not loop on the same write.

## Verify every write with `status`

A write's reply says what the daemon did (`hardware.state`, `writes`,
`readback`), but the proof is a fresh read:

```sh
korictl status | jq '.cooling | {committed, revision, changed_by}'
```

The write landed when `committed` is what you sent, `revision` moved past the
one you read, and `changed_by.client` is your name. For cooling, `telemetry`
then shows the duty the hardware reports (`.kraken.pump.duty`).

The daemon enforces the safety floors itself: a pump duty under 51 of 255, a
curve that decreases, a lighting channel the controller does not have are
refused with exit 1 and a typed `validation`, `lighting` or `display` error. `korictl` checks
nothing but that the payload is JSON, so a refusal always comes from the
daemon and always names the rule.

## Bound every `watch`

`korictl watch` streams one JSON object per line until something stops it.
Without a bound it runs until it is killed, which an agent's command timeout
turns into a failure. Always give one:

```sh
korictl watch --topic status --count 1           # wait for the next status change
korictl watch --topic events --for 30s           # what happens in the next 30 s
korictl watch --topic telemetry --interval 5s --for 10m > session.ndjson
```

`--count N` exits 0 after N pushes (heartbeats are not counted), `--for` after
the duration. A watch exits 5 if the daemon shuts down (`closing`) or goes
silent past a heartbeat. Closing its stdout (`| head -1`) exits 0.

## Collect data

- `korictl history` answers "what happened" in one call: 15 minutes at one
  second, or up to 24 hours at one minute. `korictl history --window 1h
  --metric liquid_temperature --metric pump_speed` gives min, avg, max and last
  per metric; add `--points` for the series.
- `korictl events --after SEQ` reads the log from a sequence number you kept,
  and `missed` tells you if the ring dropped anything since.
- Nothing is kept on disk by the daemon. For a record longer than 24 hours,
  keep `korictl watch` output yourself.

## Exit codes

| Code | Meaning | What to do |
|---|---|---|
| 0 | Success | Parse stdout. |
| 1 | The daemon refused | Read `error` on stdout; nothing was written. |
| 2 | Usage or payload error | Fix the command line; nothing was sent. |
| 3 | No daemon | `korid` is not running; tell the operator rather than starting it yourself. |
| 4 | Protocol mismatch | `korictl` and `korid` are different versions. |
| 5 | Connection lost or timed out | Retry once; a stream ended when the daemon stopped. |

On any failure, stdout holds exactly one object with an `error` member, and
stderr one sentence for a person. Branch on the exit code and the `error`
value, never on the sentence.

## Leave the machine as you found it

After an experiment, restore what you saved in step 3 of "Read before you
write", with `--if-revision` set to the revision your own last write produced,
and verify it with `status`. The window shows every change with the name that
made it, so an operator who opens it sees what the agent did.

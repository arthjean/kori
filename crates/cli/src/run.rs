// SPDX-FileCopyrightText: 2026 Arthur Jean
// SPDX-License-Identifier: GPL-3.0-or-later

//! One invocation, from a command line to a JSON document and an exit code.
//!
//! The output contract lives here and nowhere else:
//!
//! - stdout carries exactly one JSON document, the daemon's reply on success
//!   and an object with an `error` member on failure. Nothing else is ever
//!   written to it, so `korictl status | jq` cannot be broken by a warning.
//! - stderr carries one sentence on failure, for a person reading a terminal.
//! - the exit code says which kind of failure it was, see [`Exit`].

use std::ffi::OsString;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use kori_core::client::{Client, ClientError};
use kori_core::ipc::{
    FrameError, HEARTBEAT_INTERVAL, IpcError, MAX_FRAME_BYTES, PROTOCOL_VERSION, Push, Response,
};
use serde::Serialize;
use serde_json::{Value, json};

use crate::command::{
    self, Access, Action, COMMANDS, GLOBAL_OPTIONS, Invocation, Operand, UsageError,
};

/// The exit codes, stable across versions because scripts branch on them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exit {
    /// The daemon answered and did what was asked.
    Success = 0,
    /// The daemon refused the request with a typed error.
    Refused = 1,
    /// The command line or the payload was wrong; nothing was sent.
    Usage = 2,
    /// No daemon answered at the socket.
    Unreachable = 3,
    /// The daemon speaks another protocol version, or answered out of turn.
    Protocol = 4,
    /// The connection was lost or timed out after it was opened.
    Lost = 5,
}

impl Exit {
    pub fn code(self) -> u8 {
        self as u8
    }
}

/// What the process inherits that changes where it connects.
///
/// Read once in `main` and passed in, so the tests drive the same path with a
/// value of their own rather than mutating the process environment.
#[derive(Debug, Clone)]
pub struct Environment {
    /// `KORI_SOCKET` when set, otherwise the per-user runtime directory.
    pub socket: PathBuf,
    /// `KORI_CLIENT` when set and not empty.
    pub client: Option<String>,
}

/// Environment variable the handshake name is read from.
pub const CLIENT_NAME_ENV: &str = "KORI_CLIENT";

/// The handshake name when neither `--client` nor `KORI_CLIENT` gives one.
pub const DEFAULT_CLIENT_NAME: &str = "korictl";

impl Environment {
    pub fn from_process() -> Self {
        Self {
            socket: kori_core::ipc::socket_path_from_env(),
            // An empty variable reads as unset, as it does for the socket: a
            // shell that exported it blank meant no override, not a name the
            // daemon is certain to refuse.
            client: std::env::var(CLIENT_NAME_ENV)
                .ok()
                .filter(|name| !name.is_empty()),
        }
    }
}

/// Every way an invocation can fail, before it becomes an output.
#[derive(Debug)]
enum Failure {
    Usage(UsageError),
    InvalidPayload {
        operand: &'static str,
        detail: String,
    },
    Unreachable {
        path: String,
        detail: String,
    },
    Timeout {
        after: Duration,
    },
    Disconnected {
        detail: String,
    },
    Daemon(IpcError),
    Unexpected {
        expected: &'static str,
        actual: &'static str,
    },
}

impl Failure {
    fn from_client(error: ClientError, timeout: Duration) -> Self {
        match error {
            ClientError::Unreachable { path, source } => Self::Unreachable {
                path,
                detail: source.to_string(),
            },
            // A socket timeout surfaces as `WouldBlock` on Linux and as
            // `TimedOut` elsewhere; both mean the daemon stopped answering.
            ClientError::Disconnected(FrameError::Io(source))
                if matches!(
                    source.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                Self::Timeout { after: timeout }
            }
            ClientError::Disconnected(error) => Self::Disconnected {
                detail: error.to_string(),
            },
            ClientError::Refused(error) => Self::Daemon(error),
            ClientError::Unexpected { expected, actual } => Self::Unexpected { expected, actual },
        }
    }

    fn exit(&self) -> Exit {
        match self {
            Self::Usage(_) | Self::InvalidPayload { .. } => Exit::Usage,
            Self::Unreachable { .. } => Exit::Unreachable,
            Self::Daemon(IpcError::UnsupportedProtocol { .. }) | Self::Unexpected { .. } => {
                Exit::Protocol
            }
            Self::Daemon(_) => Exit::Refused,
            Self::Timeout { .. } | Self::Disconnected { .. } => Exit::Lost,
        }
    }

    /// The object printed on stdout. A refusal is the daemon's own error object,
    /// exactly as it travelled, so a script matches the codes the protocol
    /// defines rather than a second vocabulary invented here.
    fn document(&self) -> Value {
        match self {
            Self::Usage(usage) => {
                let mut document = json!({ "error": "usage", "detail": usage.detail });
                if let Some(token) = &usage.token {
                    document["token"] = json!(token);
                }
                document
            }
            Self::InvalidPayload { operand, detail } => {
                json!({ "error": "invalid_payload", "operand": operand, "detail": detail })
            }
            Self::Unreachable { path, detail } => {
                json!({ "error": "unreachable", "path": path, "detail": detail })
            }
            Self::Timeout { after } => {
                json!({ "error": "timeout", "timeout_s": after.as_secs_f64() })
            }
            Self::Disconnected { detail } => json!({ "error": "disconnected", "detail": detail }),
            Self::Daemon(error) => serde_json::to_value(error)
                .unwrap_or_else(|encode| json!({ "error": "io", "detail": encode.to_string() })),
            Self::Unexpected { expected, actual } => {
                json!({ "error": "unexpected_response", "expected": expected, "actual": actual })
            }
        }
    }

    /// The sentence printed on stderr.
    fn sentence(&self) -> String {
        match self {
            Self::Usage(usage) => match &usage.token {
                Some(token) => format!("{} (at '{token}')\n\n{}", usage.detail, usage_text()),
                None => format!("{}\n\n{}", usage.detail, usage_text()),
            },
            Self::InvalidPayload { operand, detail } => {
                format!("{operand} is not valid JSON, nothing was sent: {detail}")
            }
            Self::Unreachable { path, detail } => {
                format!("no daemon answered at {path} ({detail}); is korid running?")
            }
            Self::Timeout { after } => {
                format!("the daemon did not answer within {} s", after.as_secs_f64())
            }
            Self::Disconnected { detail } => {
                format!("the connection to the daemon was lost: {detail}")
            }
            Self::Daemon(error) => format!("the daemon refused: {error}"),
            Self::Unexpected { expected, actual } => {
                format!("the daemon answered {actual} where {expected} was expected")
            }
        }
    }
}

/// The usage summary, generated from the command table.
fn usage_text() -> String {
    let mut text = String::from("Usage: korictl [OPTIONS] COMMAND\n\nCommands:\n");
    let width = COMMANDS
        .iter()
        .map(|spec| spec.synopsis().len())
        .max()
        .unwrap_or(0);
    for spec in COMMANDS {
        let marker = match spec.access() {
            Access::Read => "     ",
            Access::Write => "write",
        };
        text.push_str(&format!(
            "  {:width$}  {marker}  {}\n",
            spec.synopsis(),
            spec.summary
        ));
        for option in spec.options {
            text.push_str(&format!(
                "      {:width$}  {}\n",
                option.synopsis(),
                option.summary,
                width = width.saturating_sub(4) + 7
            ));
        }
    }
    text.push_str("\nOptions:\n");
    let width = GLOBAL_OPTIONS
        .iter()
        .map(|(name, _)| name.len())
        .max()
        .unwrap_or(0);
    for (name, summary) in GLOBAL_OPTIONS {
        text.push_str(&format!("  {name:width$}  {summary}\n"));
    }
    text.push_str(
        "\nA payload is the protocol's own JSON for that request, or - to read it from stdin.\n\
         Exit codes: 0 success, 1 refused by the daemon, 2 usage or payload, 3 unreachable,\n\
         4 protocol mismatch, 5 connection lost or timed out.",
    );
    text
}

/// A request frame carrying a payload this client does not interpret.
///
/// `expect_revision` is the `--if-revision` precondition, added beside the
/// payload rather than inside it because it belongs to the request, not to the
/// program, command or preset it carries.
pub(crate) fn forward_frame(
    tag: &str,
    field: &str,
    payload: Value,
    expect_revision: Option<u64>,
) -> Value {
    let mut frame = serde_json::Map::new();
    frame.insert("request".into(), json!(tag));
    frame.insert(field.into(), payload);
    if let Some(revision) = expect_revision {
        frame.insert("expect_revision".into(), json!(revision));
    }
    Value::Object(frame)
}

/// A usage failure naming the token responsible.
fn usage(detail: String, token: impl ToString) -> Failure {
    Failure::Usage(UsageError {
        detail,
        token: Some(token.to_string()),
    })
}

/// A count, a sequence number or a revision: a non-negative integer.
fn parse_count(option: &str, value: &str) -> Result<u64, Failure> {
    value.parse::<u64>().map_err(|_| {
        usage(
            format!("{option} expects a non-negative integer, got {value}"),
            value,
        )
    })
}

/// The integer an option was given, when it was given.
fn optional_count(invocation: &Invocation, option: &str) -> Result<Option<u64>, Failure> {
    invocation
        .value(option)
        .map(|value| parse_count(option, value))
        .transpose()
}

/// Run one invocation. Returns the exit code; everything else is written.
pub fn run(
    arguments: impl IntoIterator<Item = OsString>,
    environment: &Environment,
    stdin: &mut dyn Read,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> Exit {
    let invocation = match command::parse(arguments) {
        Ok(invocation) => invocation,
        Err(usage) => return fail(&Failure::Usage(usage), false, stdout, stderr),
    };
    if invocation.spec.action == Action::Watch {
        return watch(&invocation, environment, stdout, stderr);
    }
    let pretty = invocation.globals.pretty;
    match execute(&invocation, environment, stdin, stderr) {
        Ok(document) => {
            emit(stdout, &document, pretty);
            Exit::Success
        }
        Err(failure) => fail(&failure, pretty, stdout, stderr),
    }
}

fn fail(failure: &Failure, pretty: bool, stdout: &mut dyn Write, stderr: &mut dyn Write) -> Exit {
    emit(stdout, &failure.document(), pretty);
    // A closed stderr loses a sentence for a person; the document and the exit
    // code a program reads are already out.
    let _ = writeln!(stderr, "korictl: {}", failure.sentence());
    failure.exit()
}

/// Write one document and its newline.
///
/// A failed write is not reported anywhere: stdout is the only channel that
/// could carry the report, and the exit code still says how the command went.
fn emit(stdout: &mut dyn Write, document: &impl Serialize, pretty: bool) {
    let written = if pretty {
        serde_json::to_writer_pretty(&mut *stdout, document)
    } else {
        serde_json::to_writer(&mut *stdout, document)
    };
    if written.is_ok() {
        let _ = stdout.write_all(b"\n");
    }
    let _ = stdout.flush();
}

/// Subscribe and print every push on its own line until a bound is reached.
///
/// The line format is the contract, so `--pretty` does not apply here: one
/// compact JSON object per line, flushed as it arrives, is what `jq` and an
/// agent reading a pipe both rely on.
///
/// Exits 0 when `--count` or `--for` is reached, and when the reader closes
/// stdout (`korictl watch | head -1`), which is a reader that has what it
/// wanted rather than a failure. Exits 5 after printing `closing` when the
/// daemon shuts down, and 5 when the daemon goes silent past a heartbeat.
fn watch(
    invocation: &Invocation,
    environment: &Environment,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> Exit {
    match start_watch(invocation, environment) {
        Ok((subscription, bounds)) => stream_pushes(subscription, &bounds, stdout, stderr),
        Err(failure) => fail(&failure, false, stdout, stderr),
    }
}

/// When a watch stops on its own.
struct Bounds {
    count: Option<u64>,
    until: Option<Instant>,
}

fn start_watch(
    invocation: &Invocation,
    environment: &Environment,
) -> Result<(kori_core::client::Subscription, Bounds), Failure> {
    let socket = invocation
        .globals
        .socket
        .as_deref()
        .unwrap_or(&environment.socket);
    let timeout = invocation.globals.timeout;
    let client = invocation
        .globals
        .client
        .as_deref()
        .or(environment.client.as_deref())
        .unwrap_or(DEFAULT_CLIENT_NAME);

    // Topics travel as typed and the daemon judges them, as it judges every
    // other name this client forwards.
    let mut topics: Vec<&str> = invocation.values("--topic");
    if topics.is_empty() {
        topics = vec!["telemetry", "status", "events"];
    }
    let mut frame = serde_json::Map::new();
    frame.insert("request".into(), json!("subscribe"));
    frame.insert("topics".into(), json!(topics));
    if let Some(value) = invocation.value("--interval") {
        let seconds = command::parse_duration("--interval", value).map_err(Failure::Usage)?;
        frame.insert("interval_s".into(), json!(seconds));
    }
    let count = optional_count(invocation, "--count")?;
    let until = match invocation.value("--for") {
        Some(value) => {
            let seconds = command::parse_duration("--for", value).map_err(Failure::Usage)?;
            Some(Instant::now() + Duration::from_secs(u64::from(seconds)))
        }
        None => None,
    };

    let lost = |error| Failure::from_client(error, timeout);
    let subscription = Client::connect_as(socket, timeout, Some(client))
        .map_err(lost)?
        .subscribe_frame(&Value::Object(frame))
        .map_err(lost)?;
    Ok((subscription, Bounds { count, until }))
}

fn stream_pushes(
    mut subscription: kori_core::client::Subscription,
    bounds: &Bounds,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> Exit {
    // What a dead daemon looks like: no heartbeat when one was due.
    let silence = HEARTBEAT_INTERVAL + Duration::from_secs(1);
    let mut printed = 0u64;
    loop {
        if bounds.count.is_some_and(|count| printed >= count) {
            return Exit::Success;
        }
        let wait = match bounds.until {
            Some(until) => {
                let remaining = until.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return Exit::Success;
                }
                remaining.min(silence)
            }
            None => silence,
        };
        if let Err(error) = subscription.set_read_timeout(wait) {
            return fail(&Failure::from_client(error, wait), false, stdout, stderr);
        }
        let push = match subscription.next_push() {
            Ok(push) => push,
            Err(error) => {
                if bounds.until.is_some_and(|until| Instant::now() >= until) {
                    return Exit::Success;
                }
                return fail(&Failure::from_client(error, wait), false, stdout, stderr);
            }
        };
        // A reader that closed its end has what it wanted.
        if !emit_line(stdout, &push) {
            return Exit::Success;
        }
        match push {
            Push::Closing { reason } => {
                let _ = writeln!(stderr, "korictl: the daemon closed the stream: {reason}");
                return Exit::Lost;
            }
            Push::Heartbeat { .. } => {}
            _ => printed += 1,
        }
    }
}

/// Write one push as one line and flush it. False when stdout is gone.
fn emit_line(stdout: &mut dyn Write, push: &Push) -> bool {
    serde_json::to_writer(&mut *stdout, push).is_ok()
        && stdout.write_all(b"\n").is_ok()
        && stdout.flush().is_ok()
}

/// What an invocation printed on success: the daemon's reply, or a document
/// this client composed.
#[derive(Serialize)]
#[serde(untagged)]
enum Document {
    Reply(Response),
    Composed(Value),
}

fn execute(
    invocation: &Invocation,
    environment: &Environment,
    stdin: &mut dyn Read,
    stderr: &mut dyn Write,
) -> Result<Document, Failure> {
    let socket = invocation
        .globals
        .socket
        .as_deref()
        .unwrap_or(&environment.socket);
    let timeout = invocation.globals.timeout;
    let action = invocation.spec.action;
    let client = invocation
        .globals
        .client
        .as_deref()
        .or(environment.client.as_deref())
        .unwrap_or(DEFAULT_CLIENT_NAME);

    // A payload is read and checked to be JSON before anything is connected,
    // so a typo costs no request. Its contents are the daemon's to judge.
    let payload = match (invocation.spec.operand, invocation.operand.as_deref()) {
        (Some(Operand::Payload(label)), Some(text)) => Some(read_payload(label, text, stdin)?),
        _ => None,
    };
    let name = || invocation.operand.clone().unwrap_or_default();

    let lost = |error| Failure::from_client(error, timeout);
    let connect = || Client::connect_as(socket, timeout, Some(client)).map_err(lost);
    let reply = match action {
        // Diverted in `run`, because a stream is many documents rather than
        // one. Reaching here would be a wiring fault, and it is reported as
        // one rather than streamed into the one-document contract.
        Action::Watch => {
            return Err(Failure::Usage(UsageError {
                detail: "watch is a stream and cannot run as a single command".to_string(),
                token: None,
            }));
        }
        Action::Version => {
            return Ok(Document::Composed(version(socket, timeout, client, stderr)));
        }
        Action::Status => Response::Status(Box::new(connect()?.status().map_err(lost)?)),
        Action::Telemetry => Response::Telemetry(Box::new(connect()?.telemetry().map_err(lost)?)),
        Action::Capabilities => {
            Response::Capabilities(Box::new(connect()?.capabilities().map_err(lost)?))
        }
        Action::Profiles => {
            let (active, profiles) = connect()?.profiles().map_err(lost)?;
            Response::Profiles { active, profiles }
        }
        Action::Diagnostics => Response::Diagnostics(connect()?.diagnostics().map_err(lost)?),
        Action::Events => {
            let after = optional_count(invocation, "--after")?;
            // Range-checked by the daemon, which owns the ceiling; a value too
            // large for the wire type is the one thing refused here.
            let limit = match optional_count(invocation, "--limit")? {
                Some(limit) => Some(
                    u32::try_from(limit)
                        .map_err(|_| usage(format!("--limit {limit} is out of range"), limit))?,
                ),
                None => None,
            };
            Response::Events(connect()?.events(after, limit).map_err(lost)?)
        }
        Action::History => {
            // The metric names travel as typed. An unknown one is the daemon's
            // to refuse, so the list of metrics lives in one place.
            let mut frame = serde_json::Map::new();
            frame.insert("request".into(), json!("history"));
            let metrics = invocation.values("--metric");
            if !metrics.is_empty() {
                frame.insert("metrics".into(), json!(metrics));
            }
            for (option, field) in [("--window", "window_s"), ("--step", "step_s")] {
                if let Some(value) = invocation.value(option) {
                    let seconds = command::parse_duration(option, value).map_err(Failure::Usage)?;
                    frame.insert(field.into(), json!(seconds));
                }
            }
            if invocation.flag("--points") {
                frame.insert("points".into(), json!(true));
            }
            connect()?
                .forward(&Value::Object(frame), "history")
                .map_err(lost)?
        }
        Action::ProfileActivate => {
            Response::Activated(connect()?.activate_profile(name()).map_err(lost)?)
        }
        Action::ProfileDelete => {
            let (name, activated_instead) = connect()?.delete_profile(name()).map_err(lost)?;
            Response::Deleted {
                name,
                activated_instead,
            }
        }
        Action::Forward {
            tag,
            field,
            expected,
        } => {
            let expect_revision = optional_count(invocation, "--if-revision")?;
            let frame = forward_frame(tag, field, payload.unwrap_or(Value::Null), expect_revision);
            connect()?.forward(&frame, expected).map_err(lost)?
        }
    };
    Ok(Document::Reply(reply))
}

/// This client's version, and the daemon's when one completes the handshake.
///
/// Always succeeds: the point of the command is to report what is installed,
/// and "no daemon" is one of the answers.
fn version(socket: &Path, timeout: Duration, client: &str, stderr: &mut dyn Write) -> Value {
    let daemon_version = match Client::connect_as(socket, timeout, Some(client)) {
        Ok(client) => Some(client.daemon_version().to_string()),
        Err(error) => {
            let _ = writeln!(
                stderr,
                "korictl: {}",
                Failure::from_client(error, timeout).sentence()
            );
            None
        }
    };
    json!({
        "korictl": env!("CARGO_PKG_VERSION"),
        "protocol_version": PROTOCOL_VERSION,
        "daemon_version": daemon_version,
    })
}

/// The operand as JSON, read from stdin when it is `-`.
fn read_payload(label: &'static str, text: &str, stdin: &mut dyn Read) -> Result<Value, Failure> {
    let invalid = |detail: String| Failure::InvalidPayload {
        operand: label,
        detail,
    };
    let owned;
    let source = if text == "-" {
        // One byte past the frame ceiling is enough to know it cannot be sent.
        let mut buffer = String::new();
        stdin
            .take(MAX_FRAME_BYTES as u64 + 1)
            .read_to_string(&mut buffer)
            .map_err(|error| invalid(format!("reading stdin failed: {error}")))?;
        owned = buffer;
        owned.as_str()
    } else {
        text
    };
    serde_json::from_str(source).map_err(|error| invalid(error.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn invoke(line: &[&str], stdin: &str) -> (Exit, String, String) {
        let environment = Environment {
            socket: PathBuf::from("/nonexistent/kori-test/kori.sock"),
            client: None,
        };
        let mut out = Vec::new();
        let mut err = Vec::new();
        let exit = run(
            line.iter().map(OsString::from),
            &environment,
            &mut stdin.as_bytes(),
            &mut out,
            &mut err,
        );
        (
            exit,
            String::from_utf8(out).unwrap(),
            String::from_utf8(err).unwrap(),
        )
    }

    fn parsed(stdout: &str) -> Value {
        assert_eq!(stdout.matches('\n').count(), 1, "one line: {stdout:?}");
        serde_json::from_str(stdout).unwrap()
    }

    #[test]
    fn exit_codes_are_the_documented_numbers() {
        assert_eq!(Exit::Success.code(), 0);
        assert_eq!(Exit::Refused.code(), 1);
        assert_eq!(Exit::Usage.code(), 2);
        assert_eq!(Exit::Unreachable.code(), 3);
        assert_eq!(Exit::Protocol.code(), 4);
        assert_eq!(Exit::Lost.code(), 5);
    }

    #[test]
    fn a_payload_that_is_not_json_is_refused_before_connecting() {
        // The socket does not exist: reaching it would be exit 3, so exit 2
        // proves the refusal happened first.
        let (exit, out, err) = invoke(&["set", "cooling", "{mode: fixed"], "");
        assert_eq!(exit, Exit::Usage);
        let document = parsed(&out);
        assert_eq!(document["error"], "invalid_payload");
        assert_eq!(document["operand"], "PROGRAM");
        assert!(err.contains("nothing was sent"), "{err}");

        let (exit, out, _) = invoke(&["profile", "save", "-"], "not json");
        assert_eq!(exit, Exit::Usage);
        assert_eq!(parsed(&out)["error"], "invalid_payload");
    }

    #[test]
    fn a_usage_error_prints_json_and_names_the_token_on_stderr() {
        let (exit, out, err) = invoke(&["frobnicate"], "");
        assert_eq!(exit, Exit::Usage);
        let document = parsed(&out);
        assert_eq!(document["error"], "usage");
        assert_eq!(document["token"], "frobnicate");
        assert!(err.contains("'frobnicate'"), "{err}");
        // The command list on stderr comes from the same table the parser reads.
        for spec in COMMANDS {
            assert!(err.contains(&spec.synopsis()), "{}", spec.synopsis());
        }
    }

    #[test]
    fn pretty_output_is_still_one_document() {
        let (exit, out, _) = invoke(&["--pretty", "version"], "");
        assert_eq!(exit, Exit::Success);
        assert!(out.lines().count() > 1);
        let parsed: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(parsed["daemon_version"], Value::Null);
    }

    #[test]
    fn a_daemon_refusal_is_printed_as_the_daemon_sent_it() {
        let refusal = Failure::Daemon(IpcError::ReadOnly {
            reason: "no udev rule".into(),
        });
        assert_eq!(refusal.exit(), Exit::Refused);
        assert_eq!(
            refusal.document(),
            serde_json::to_value(IpcError::ReadOnly {
                reason: "no udev rule".into()
            })
            .unwrap()
        );
        let mismatch = Failure::Daemon(IpcError::UnsupportedProtocol {
            requested: 9,
            supported: 10,
        });
        assert_eq!(mismatch.exit(), Exit::Protocol);
        assert_eq!(
            mismatch.document(),
            json!({ "error": "unsupported_protocol", "requested": 9, "supported": 10 })
        );
    }
}

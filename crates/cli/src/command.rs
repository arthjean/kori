// SPDX-FileCopyrightText: 2026 Arthur Jean
// SPDX-License-Identifier: GPL-3.0-or-later

//! Every command `korictl` accepts, and the parser that reads them.
//!
//! One table, [`COMMANDS`], is both what the parser matches and what a usage
//! error lists, so a command cannot exist in one and be missing from the other.
//!
//! The grammar is deliberately strict: global options come before the command,
//! nothing is abbreviated, and an unknown token is a usage error that names it.
//! A caller that is a program learns nothing from a parser that guesses.

use std::ffi::OsString;
use std::path::PathBuf;
use std::time::Duration;

use kori_core::client::DEFAULT_TIMEOUT;

/// Whether a command can change the hardware or the stored configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    /// Reads only. Safe to allow without a prompt.
    Read,
    /// Sends a request that may change hardware or configuration. Every such
    /// command starts with `set` or `profile`, so one prefix rule in an agent's
    /// permissions covers all of them.
    Write,
}

/// What a command does once parsed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Version,
    Status,
    Telemetry,
    Capabilities,
    Profiles,
    Diagnostics,
    Events,
    History,
    /// Subscribe and print each push on its own line.
    Watch,
    /// Hand a payload the operator wrote to the daemon inside one request
    /// frame, without decoding it here. `tag` is the request's wire name,
    /// `field` the member the payload travels in, `expected` the reply's kind.
    Forward {
        tag: &'static str,
        field: &'static str,
        expected: &'static str,
    },
    ProfileActivate,
    ProfileDelete,
}

impl Action {
    /// Derived from what the action sends, not declared beside it, so the
    /// classification cannot disagree with the request.
    pub fn access(self) -> Access {
        match self {
            Self::Version
            | Self::Status
            | Self::Telemetry
            | Self::Capabilities
            | Self::Profiles
            | Self::Diagnostics
            | Self::Events
            | Self::History
            | Self::Watch => Access::Read,
            Self::Forward { .. } | Self::ProfileActivate | Self::ProfileDelete => Access::Write,
        }
    }
}

/// The one positional argument a command may take.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Operand {
    /// A JSON document in the protocol's own shape, or `-` to read it from
    /// standard input.
    Payload(&'static str),
    /// A profile name.
    Name,
}

impl Operand {
    pub fn label(self) -> &'static str {
        match self {
            Self::Payload(label) => label,
            Self::Name => "NAME",
        }
    }
}

/// An option a command takes after its words.
#[derive(Debug)]
pub struct Opt {
    pub name: &'static str,
    /// The label of the value it takes, or `None` for a flag.
    pub value: Option<&'static str>,
    /// Whether it may be given more than once.
    pub repeat: bool,
    pub summary: &'static str,
}

impl Opt {
    /// The option as a usage line shows it.
    pub fn synopsis(&self) -> String {
        let mut text = self.name.to_string();
        if let Some(value) = self.value {
            text.push(' ');
            text.push_str(value);
        }
        if self.repeat {
            text.push_str("...");
        }
        text
    }
}

/// The precondition every `set` command takes.
const IF_REVISION: Opt = Opt {
    name: "--if-revision",
    value: Some("N"),
    repeat: false,
    summary: "Refuse with `conflict` unless the domain is still at revision N.",
};

/// One command: the words that select it, what it takes, what it does.
#[derive(Debug)]
pub struct Spec {
    pub words: &'static [&'static str],
    pub operand: Option<Operand>,
    /// Options accepted after the words, in the order a usage line lists them.
    pub options: &'static [Opt],
    pub summary: &'static str,
    pub action: Action,
}

impl Spec {
    pub fn access(&self) -> Access {
        self.action.access()
    }

    /// The words and the operand, as a usage line shows them.
    pub fn synopsis(&self) -> String {
        let mut line = self.words.join(" ");
        if let Some(operand) = self.operand {
            line.push(' ');
            line.push_str(operand.label());
        }
        line
    }
}

/// Every command, in the order a usage message lists them.
pub const COMMANDS: &[Spec] = &[
    Spec {
        words: &["version"],
        operand: None,
        options: &[],
        summary: "This client's version and protocol, and the daemon's version when it answers.",
        action: Action::Version,
    },
    Spec {
        words: &["status"],
        operand: None,
        options: &[],
        summary: "Daemon, device, ownership and committed state.",
        action: Action::Status,
    },
    Spec {
        words: &["telemetry"],
        operand: None,
        options: &[],
        summary: "The most recent sampling pass.",
        action: Action::Telemetry,
    },
    Spec {
        words: &["capabilities"],
        operand: None,
        options: &[],
        summary: "The versioned capability record of every allowlisted device.",
        action: Action::Capabilities,
    },
    Spec {
        words: &["profiles"],
        operand: None,
        options: &[],
        summary: "Every stored profile and the active one.",
        action: Action::Profiles,
    },
    Spec {
        words: &["diagnostics"],
        operand: None,
        options: &[],
        summary: "The redacted diagnostics export for an issue report.",
        action: Action::Diagnostics,
    },
    Spec {
        words: &["events"],
        operand: None,
        options: &[
            Opt {
                name: "--after",
                value: Some("SEQ"),
                repeat: false,
                summary: "Only events recorded after SEQ, oldest first. Without it, the newest.",
            },
            Opt {
                name: "--limit",
                value: Some("N"),
                repeat: false,
                summary: "At most N events (default 100, at most 512).",
            },
        ],
        summary: "A page of the event log, numbered by seq.",
        action: Action::Events,
    },
    Spec {
        words: &["history"],
        operand: None,
        options: &[
            Opt {
                name: "--metric",
                value: Some("METRIC"),
                repeat: true,
                summary: "Only this metric (cpu_load, cpu_temperature, gpu_load, gpu_temperature, \
                          memory_used, liquid_temperature, pump_speed, fan_speed, pump_duty, \
                          fan_duty). Default: all.",
            },
            Opt {
                name: "--window",
                value: Some("DURATION"),
                repeat: false,
                summary: "How far back: 90s, 15m, 24h or seconds. Default 15m.",
            },
            Opt {
                name: "--step",
                value: Some("DURATION"),
                repeat: false,
                summary: "Seconds per point: under 60 or whole minutes. Default 1s up to 15m, then 1m.",
            },
            Opt {
                name: "--points",
                value: None,
                repeat: false,
                summary: "Add the points, oldest first, beside each summary.",
            },
        ],
        summary: "Recorded telemetry: min, avg, max and last per metric over a window.",
        action: Action::History,
    },
    Spec {
        words: &["watch"],
        operand: None,
        options: &[
            Opt {
                name: "--topic",
                value: Some("TOPIC"),
                repeat: true,
                summary: "telemetry, status or events. Default: all three.",
            },
            Opt {
                name: "--interval",
                value: Some("DURATION"),
                repeat: false,
                summary: "Telemetry every DURATION, 1s to 60s. Default 1s.",
            },
            Opt {
                name: "--count",
                value: Some("N"),
                repeat: false,
                summary: "Exit 0 after N pushes, heartbeats not counted.",
            },
            Opt {
                name: "--for",
                value: Some("DURATION"),
                repeat: false,
                summary: "Exit 0 after DURATION. Without --count or --for, runs until stopped.",
            },
        ],
        summary: "Subscribe and print one push per line, flushed as it arrives.",
        action: Action::Watch,
    },
    Spec {
        words: &["set", "cooling"],
        operand: Some(Operand::Payload("PROGRAM")),
        options: &[IF_REVISION],
        summary: "Apply a cooling program (the `program` of apply_program).",
        action: Action::Forward {
            tag: "apply_program",
            field: "program",
            expected: "applied",
        },
    },
    Spec {
        words: &["set", "lighting"],
        operand: Some(Operand::Payload("COMMAND")),
        options: &[IF_REVISION],
        summary: "Apply a lighting program to one channel (the `command` of apply_lighting).",
        action: Action::Forward {
            tag: "apply_lighting",
            field: "command",
            expected: "lit",
        },
    },
    Spec {
        words: &["set", "display"],
        operand: Some(Operand::Payload("PRESET")),
        options: &[IF_REVISION],
        summary: "Show a preset on the panel (the `preset` of apply_display).",
        action: Action::Forward {
            tag: "apply_display",
            field: "preset",
            expected: "shown",
        },
    },
    Spec {
        words: &["profile", "activate"],
        operand: Some(Operand::Name),
        options: &[],
        summary: "Make a stored profile active.",
        action: Action::ProfileActivate,
    },
    Spec {
        words: &["profile", "save"],
        operand: Some(Operand::Payload("PROFILE")),
        options: &[],
        summary: "Store a profile, replacing one with the same name (the `profile` of save_profile).",
        action: Action::Forward {
            tag: "save_profile",
            field: "profile",
            expected: "saved",
        },
    },
    Spec {
        words: &["profile", "delete"],
        operand: Some(Operand::Name),
        options: &[],
        summary: "Remove a stored profile.",
        action: Action::ProfileDelete,
    },
];

/// Options that apply to every command, given before it.
#[derive(Debug, Clone, PartialEq)]
pub struct Globals {
    /// Overrides `KORI_SOCKET` and the runtime-directory default.
    pub socket: Option<PathBuf>,
    /// Bound on each read and write on the socket.
    pub timeout: Duration,
    /// Indent the JSON document. Nothing else about the output changes.
    pub pretty: bool,
    /// Overrides `KORI_CLIENT` and the `korictl` default.
    pub client: Option<String>,
}

impl Default for Globals {
    fn default() -> Self {
        Self {
            socket: None,
            timeout: DEFAULT_TIMEOUT,
            pretty: false,
            client: None,
        }
    }
}

/// The global options a usage message lists.
pub const GLOBAL_OPTIONS: &[(&str, &str)] = &[
    (
        "--socket PATH",
        "Daemon socket. Defaults to $KORI_SOCKET, then the runtime directory.",
    ),
    (
        "--timeout SECONDS",
        "Longest wait on any one read or write. Default 5.",
    ),
    ("--pretty", "Indent the JSON output."),
    (
        "--client NAME",
        "Name sent in the handshake. Defaults to $KORI_CLIENT, then korictl.",
    ),
];

/// A parsed command line.
#[derive(Debug)]
pub struct Invocation {
    pub globals: Globals,
    pub spec: &'static Spec,
    pub operand: Option<String>,
    /// The command's own options, in the order given. A flag carries `None`.
    pub options: Vec<(&'static str, Option<String>)>,
}

impl Invocation {
    /// Every value given for `name`, in order.
    pub fn values(&self, name: &str) -> Vec<&str> {
        self.options
            .iter()
            .filter(|(given, _)| *given == name)
            .filter_map(|(_, value)| value.as_deref())
            .collect()
    }

    /// The value of an option given at most once.
    pub fn value(&self, name: &str) -> Option<&str> {
        self.values(name).into_iter().next()
    }

    /// Whether a flag was given.
    pub fn flag(&self, name: &str) -> bool {
        self.options.iter().any(|(given, _)| *given == name)
    }
}

/// Why a command line was refused, and the token responsible when there is one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsageError {
    pub detail: String,
    pub token: Option<String>,
}

impl UsageError {
    fn new(detail: impl Into<String>, token: Option<&str>) -> Self {
        Self {
            detail: detail.into(),
            token: token.map(str::to_string),
        }
    }
}

/// A token that reads as an option rather than a value. A lone `-` is the
/// standard-input marker, not an option.
fn looks_like_option(token: &str) -> bool {
    token.len() > 1 && token.starts_with('-')
}

/// Read a command line, without the program name.
pub fn parse(arguments: impl IntoIterator<Item = OsString>) -> Result<Invocation, UsageError> {
    let mut tokens = Vec::new();
    for argument in arguments {
        match argument.into_string() {
            Ok(token) => tokens.push(token),
            Err(raw) => {
                let lossy = raw.to_string_lossy().into_owned();
                return Err(UsageError::new(
                    "argument is not valid UTF-8",
                    Some(lossy.as_str()),
                ));
            }
        }
    }
    let mut tokens = tokens.into_iter().peekable();

    let mut globals = Globals::default();
    while let Some(token) = tokens.next_if(|token| looks_like_option(token)) {
        if token == "--" {
            break;
        }
        let (name, inline) = match token.split_once('=') {
            Some((name, value)) => (name, Some(value.to_string())),
            None => (token.as_str(), None),
        };
        match name {
            "--pretty" => {
                if inline.is_some() {
                    return Err(UsageError::new("--pretty takes no value", Some(&token)));
                }
                globals.pretty = true;
            }
            "--socket" | "--timeout" | "--client" => {
                let value = match inline {
                    Some(value) => value,
                    None => tokens.next().ok_or_else(|| {
                        UsageError::new(format!("{name} needs a value"), Some(name))
                    })?,
                };
                match name {
                    "--socket" => {
                        if value.is_empty() {
                            return Err(UsageError::new("--socket needs a path", Some(name)));
                        }
                        globals.socket = Some(PathBuf::from(value));
                    }
                    // Sent as given: the daemon is the one place a name is
                    // judged, and its refusal names the rule.
                    "--client" => globals.client = Some(value),
                    _ => globals.timeout = parse_timeout(&value)?,
                }
            }
            _ => {
                return Err(UsageError::new(
                    format!("unknown option {name}"),
                    Some(name),
                ));
            }
        }
    }

    let first = tokens
        .next()
        .ok_or_else(|| UsageError::new("no command given", None))?;
    let family: Vec<&'static Spec> = COMMANDS
        .iter()
        .filter(|spec| spec.words[0] == first)
        .collect();
    let spec = match family.as_slice() {
        [] => {
            return Err(UsageError::new(
                format!("unknown command {first}"),
                Some(&first),
            ));
        }
        [single] if single.words.len() == 1 => *single,
        _ => {
            let choices = family
                .iter()
                .filter_map(|spec| spec.words.get(1).copied())
                .collect::<Vec<_>>()
                .join(", ");
            let Some(second) = tokens.next() else {
                return Err(UsageError::new(
                    format!("{first} needs one of: {choices}"),
                    Some(&first),
                ));
            };
            family
                .iter()
                .find(|spec| spec.words.get(1) == Some(&second.as_str()))
                .copied()
                .ok_or_else(|| {
                    UsageError::new(
                        format!("unknown command {first} {second}; expected one of: {choices}"),
                        Some(&second),
                    )
                })?
        }
    };

    let mut positionals = Vec::new();
    let mut options = Vec::new();
    let mut options_ended = false;
    while let Some(token) = tokens.next() {
        if !options_ended && token == "--" {
            options_ended = true;
            continue;
        }
        if options_ended || !looks_like_option(&token) {
            positionals.push(token);
            continue;
        }
        let (name, inline) = match token.split_once('=') {
            Some((name, value)) => (name, Some(value.to_string())),
            None => (token.as_str(), None),
        };
        let Some(option) = spec.options.iter().find(|option| option.name == name) else {
            return Err(UsageError::new(
                format!(
                    "unknown option {name} for {}; global options go before the command",
                    spec.words.join(" ")
                ),
                Some(&token),
            ));
        };
        if !option.repeat && options.iter().any(|(given, _)| *given == option.name) {
            return Err(UsageError::new(
                format!("{name} is given more than once"),
                Some(&token),
            ));
        }
        let value = match (option.value, inline) {
            (None, None) => None,
            (None, Some(_)) => {
                return Err(UsageError::new(
                    format!("{name} takes no value"),
                    Some(&token),
                ));
            }
            (Some(_), Some(value)) => Some(value),
            (Some(label), None) => Some(
                tokens
                    .next()
                    .ok_or_else(|| UsageError::new(format!("{name} needs {label}"), Some(name)))?,
            ),
        };
        options.push((option.name, value));
    }

    let mut positionals = positionals.into_iter();
    let operand = match spec.operand {
        Some(kind) => Some(positionals.next().ok_or_else(|| {
            UsageError::new(
                format!("{} needs {}", spec.words.join(" "), kind.label()),
                Some(spec.words[spec.words.len() - 1]),
            )
        })?),
        None => None,
    };
    if let Some(extra) = positionals.next() {
        return Err(UsageError::new(
            format!("unexpected argument {extra}"),
            Some(&extra),
        ));
    }

    Ok(Invocation {
        globals,
        spec,
        operand,
        options,
    })
}

/// A whole number of seconds, written `90s`, `15m`, `24h` or as a bare number.
///
/// Nothing finer than a second: the history has no finer slot, and a fraction
/// accepted here would be rounded somewhere the caller cannot see.
pub fn parse_duration(option: &str, value: &str) -> Result<u32, UsageError> {
    let refuse = || {
        UsageError::new(
            format!(
                "{option} expects a duration such as 90s, 15m, 24h or a number of seconds, got {value}"
            ),
            Some(value),
        )
    };
    let (digits, scale) = match value.char_indices().last() {
        Some((index, 's')) => (&value[..index], 1),
        Some((index, 'm')) => (&value[..index], 60),
        Some((index, 'h')) => (&value[..index], 3_600),
        _ => (value, 1),
    };
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(refuse());
    }
    digits
        .parse::<u32>()
        .ok()
        .and_then(|count| count.checked_mul(scale))
        .ok_or_else(refuse)
}

/// A positive number of seconds, fractions allowed.
///
/// Zero is refused rather than read as "no limit": the kernel rejects a zero
/// socket timeout, and an unbounded wait is the one thing this option exists to
/// prevent.
fn parse_timeout(value: &str) -> Result<Duration, UsageError> {
    let refuse = || {
        UsageError::new(
            format!("--timeout expects a positive number of seconds, got {value}"),
            Some(value),
        )
    };
    let seconds: f64 = value.parse().map_err(|_| refuse())?;
    if !seconds.is_finite() || seconds <= 0.0 {
        return Err(refuse());
    }
    Duration::try_from_secs_f64(seconds).map_err(|_| refuse())
}

#[cfg(test)]
mod tests {
    use super::*;
    use kori_core::display::DisplayPreset;
    use kori_core::ipc::Request;
    use kori_core::lighting::{LightingCommand, LightingProgram};
    use kori_core::profile::{CoolingProgram, Profile};

    fn parse_str(line: &[&str]) -> Result<Invocation, UsageError> {
        parse(line.iter().map(OsString::from))
    }

    /// The contract an agent's permission rules are written against: allowing
    /// every command that does not start with `set` or `profile` can never
    /// allow a write.
    #[test]
    fn only_set_and_profile_commands_write() {
        for spec in COMMANDS {
            let prefixed = matches!(spec.words[0], "set" | "profile");
            assert_eq!(
                spec.access() == Access::Write,
                prefixed,
                "{} is classified {:?}",
                spec.synopsis(),
                spec.access()
            );
        }
    }

    /// The frames `korictl` assembles from a forwarded payload are requests the
    /// daemon decodes into the variant the command names. The tags are written
    /// out in the table, so this is what keeps them the wire's own.
    #[test]
    fn every_forwarded_payload_travels_in_the_request_it_names() {
        let sample = |tag: &str| -> serde_json::Value {
            match tag {
                "apply_program" => serde_json::to_value(CoolingProgram::Onboard),
                "apply_lighting" => serde_json::to_value(LightingCommand {
                    channel: 1,
                    program: LightingProgram::Off,
                }),
                "apply_display" => serde_json::to_value(DisplayPreset::default_infographic()),
                "save_profile" => serde_json::to_value(Profile {
                    name: "Quiet".into(),
                    program: CoolingProgram::Onboard,
                    device: None,
                    lighting: Vec::new(),
                    display: None,
                }),
                other => panic!("no sample payload for {other}"),
            }
            .unwrap()
        };

        let mut forwarded = 0;
        for spec in COMMANDS {
            let Action::Forward { tag, field, .. } = spec.action else {
                continue;
            };
            forwarded += 1;
            let frame = crate::run::forward_frame(tag, field, sample(tag), None);
            let request: Request = serde_json::from_value(frame).unwrap();
            let decoded_tag = serde_json::to_value(&request).unwrap()["request"].clone();
            assert_eq!(decoded_tag, tag, "{}", spec.synopsis());
            assert!(
                matches!(
                    request,
                    Request::ApplyProgram { .. }
                        | Request::ApplyLighting { .. }
                        | Request::ApplyDisplay { .. }
                        | Request::SaveProfile { .. }
                ),
                "{} forwards a request that is not a write",
                spec.synopsis()
            );
        }
        assert_eq!(forwarded, 4);
    }

    #[test]
    fn no_two_commands_share_their_words() {
        for (index, spec) in COMMANDS.iter().enumerate() {
            for other in &COMMANDS[index + 1..] {
                assert_ne!(spec.words, other.words);
            }
            // A family is either one bare word or all two-word commands, so the
            // parser never has to guess whether a second token is an operand.
            let family = COMMANDS.iter().filter(|o| o.words[0] == spec.words[0]);
            assert!(family.clone().all(|o| o.words.len() == spec.words.len()));
        }
    }

    #[test]
    fn global_options_are_read_before_the_command() {
        let parsed =
            parse_str(&["--socket", "/x.sock", "--timeout=0.5", "--pretty", "status"]).unwrap();
        assert_eq!(parsed.spec.action, Action::Status);
        assert_eq!(parsed.globals.socket, Some(PathBuf::from("/x.sock")));
        assert_eq!(parsed.globals.timeout, Duration::from_millis(500));
        assert!(parsed.globals.pretty);

        let defaults = parse_str(&["telemetry"]).unwrap();
        assert_eq!(defaults.globals, Globals::default());
        assert_eq!(defaults.globals.timeout, Duration::from_secs(5));
    }

    #[test]
    fn a_global_option_after_the_command_is_refused_by_name() {
        let error = parse_str(&["status", "--pretty"]).unwrap_err();
        assert_eq!(error.token.as_deref(), Some("--pretty"));
    }

    #[test]
    fn unknown_and_incomplete_command_lines_name_the_token() {
        let cases: &[(&[&str], Option<&str>)] = &[
            (&["frobnicate"], Some("frobnicate")),
            (&["--verbose", "status"], Some("--verbose")),
            (&["set"], Some("set")),
            (&["set", "fans", "{}"], Some("fans")),
            (&["set", "cooling"], Some("cooling")),
            (&["status", "extra"], Some("extra")),
            (&["--timeout", "0", "status"], Some("0")),
            (&["--timeout", "soon", "status"], Some("soon")),
            (&["--timeout"], Some("--timeout")),
            (&["--pretty=yes", "status"], Some("--pretty=yes")),
            (&[], None),
        ];
        for (line, token) in cases {
            let error = parse_str(line).unwrap_err();
            assert_eq!(error.token.as_deref(), *token, "{line:?}: {error:?}");
        }
    }

    #[test]
    fn durations_are_whole_seconds_with_an_optional_unit() {
        for (text, seconds) in [("90s", 90), ("15m", 900), ("24h", 86_400), ("45", 45)] {
            assert_eq!(parse_duration("--window", text), Ok(seconds), "{text}");
        }
        for text in ["", "m", "1.5m", "-3s", "10d", "s15", "99999999999h"] {
            let error = parse_duration("--window", text).unwrap_err();
            assert_eq!(error.token.as_deref(), Some(text), "{text}");
        }
    }

    #[test]
    fn command_options_are_checked_against_the_command() {
        let parsed = parse_str(&[
            "history",
            "--metric",
            "cpu_load",
            "--metric=fan_speed",
            "--points",
            "--window",
            "1h",
        ])
        .unwrap();
        assert_eq!(parsed.values("--metric"), vec!["cpu_load", "fan_speed"]);
        assert!(parsed.flag("--points"));
        assert_eq!(parsed.value("--window"), Some("1h"));

        for (line, token) in [
            (&["history", "--points=yes"][..], "--points=yes"),
            (
                &["history", "--window", "1m", "--window", "2m"][..],
                "--window",
            ),
            (&["history", "--window"][..], "--window"),
            (&["events", "--points"][..], "--points"),
        ] {
            let error = parse_str(line).unwrap_err();
            assert_eq!(error.token.as_deref(), Some(token), "{line:?}: {error:?}");
        }
    }

    #[test]
    fn a_payload_operand_may_be_stdin_or_follow_a_double_dash() {
        let stdin = parse_str(&["set", "cooling", "-"]).unwrap();
        assert_eq!(stdin.operand.as_deref(), Some("-"));

        let dashed = parse_str(&["profile", "delete", "--", "-odd-name"]).unwrap();
        assert_eq!(dashed.operand.as_deref(), Some("-odd-name"));
        assert_eq!(dashed.spec.action, Action::ProfileDelete);
    }
}

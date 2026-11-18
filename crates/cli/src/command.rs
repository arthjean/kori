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
            | Self::Diagnostics => Access::Read,
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

/// One command: the words that select it, what it takes, what it does.
#[derive(Debug)]
pub struct Spec {
    pub words: &'static [&'static str],
    pub operand: Option<Operand>,
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
        summary: "This client's version and protocol, and the daemon's version when it answers.",
        action: Action::Version,
    },
    Spec {
        words: &["status"],
        operand: None,
        summary: "Daemon, device, ownership and committed state.",
        action: Action::Status,
    },
    Spec {
        words: &["telemetry"],
        operand: None,
        summary: "The most recent sampling pass.",
        action: Action::Telemetry,
    },
    Spec {
        words: &["capabilities"],
        operand: None,
        summary: "The versioned capability record of every allowlisted device.",
        action: Action::Capabilities,
    },
    Spec {
        words: &["profiles"],
        operand: None,
        summary: "Every stored profile and the active one.",
        action: Action::Profiles,
    },
    Spec {
        words: &["diagnostics"],
        operand: None,
        summary: "The redacted diagnostics export for an issue report.",
        action: Action::Diagnostics,
    },
    Spec {
        words: &["set", "cooling"],
        operand: Some(Operand::Payload("PROGRAM")),
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
        summary: "Make a stored profile active.",
        action: Action::ProfileActivate,
    },
    Spec {
        words: &["profile", "save"],
        operand: Some(Operand::Payload("PROFILE")),
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
}

impl Default for Globals {
    fn default() -> Self {
        Self {
            socket: None,
            timeout: DEFAULT_TIMEOUT,
            pretty: false,
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
];

/// A parsed command line.
#[derive(Debug)]
pub struct Invocation {
    pub globals: Globals,
    pub spec: &'static Spec,
    pub operand: Option<String>,
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
            "--socket" | "--timeout" => {
                let value = match inline {
                    Some(value) => value,
                    None => tokens.next().ok_or_else(|| {
                        UsageError::new(format!("{name} needs a value"), Some(name))
                    })?,
                };
                if name == "--socket" {
                    if value.is_empty() {
                        return Err(UsageError::new("--socket needs a path", Some(name)));
                    }
                    globals.socket = Some(PathBuf::from(value));
                } else {
                    globals.timeout = parse_timeout(&value)?;
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
    let mut options_ended = false;
    for token in tokens {
        if !options_ended && token == "--" {
            options_ended = true;
        } else if !options_ended && looks_like_option(&token) {
            return Err(UsageError::new(
                format!(
                    "unknown option {token} for {}; global options go before the command",
                    spec.words.join(" ")
                ),
                Some(&token),
            ));
        } else {
            positionals.push(token);
        }
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
    })
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
            let frame = crate::run::forward_frame(tag, field, sample(tag));
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
    fn a_payload_operand_may_be_stdin_or_follow_a_double_dash() {
        let stdin = parse_str(&["set", "cooling", "-"]).unwrap();
        assert_eq!(stdin.operand.as_deref(), Some("-"));

        let dashed = parse_str(&["profile", "delete", "--", "-odd-name"]).unwrap();
        assert_eq!(dashed.operand.as_deref(), Some("-odd-name"));
        assert_eq!(dashed.spec.action, Action::ProfileDelete);
    }
}

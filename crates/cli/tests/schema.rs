// SPDX-FileCopyrightText: 2026 Arthur Jean
// SPDX-License-Identifier: GPL-3.0-or-later

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

//! The published protocol schema, held to the types it is generated from and
//! to the frames a real daemon writes.
//!
//! `docs/protocol.schema.json` is what `korictl schema` prints, so an agent
//! builds its payloads from it. Four things make it worth trusting:
//!
//! - it is byte-identical to what the wire types generate today;
//! - one example of every request, response, push and error, serialized by the
//!   types themselves, validates against it, and the examples cover every tag
//!   the schema lists;
//! - what a daemon on a fake machine actually answers validates too, which is
//!   the check a hand-written example cannot make;
//! - a struct that refuses unknown fields is refused by the schema as well.
//!
//! The validator is `jsonschema`, an implementation written independently of
//! the generator, so a mistake in one is not repeated by the other.

#[path = "../../daemon/tests/common/mod.rs"]
mod common;
mod support;

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use kori_core::DeviceId;
use kori_core::capability::{
    CAPABILITY_SCHEMA_VERSION, CapabilityId, CapabilityRecord, Evidenced, ProbeContext,
};
use kori_core::diagnostics::{DiagnosticsLog, EventKind};
use kori_core::display::{DisplayError, DisplayPreset};
use kori_core::history::{HistoryLimit, HistoryMetric, HistoryReply, MetricHistory, Summary};
use kori_core::ipc::{
    AccessMode, ActivationOutcome, ApplyOutcome, Attribution, BlockedCapability, ChannelReadback,
    ChannelState, ConfigState, ConnectedClient, CoolingState, DaemonStatus, DeviceStatus,
    DisplayOutcome, DisplayState, HardwareState, IpcError, LightingOutcome, OwnershipConflict,
    PROTOCOL_VERSION, Push, Request, Response, Revision, Topic, WriteDomain,
};
use kori_core::lighting::{
    Brightness, EffectDirection, EffectSpeed, LightingCommand, LightingEffect, LightingError,
    LightingProgram, Rgb,
};
use kori_core::profile::{
    Channel, CoolingProgram, Incompatibility, MIN_PUMP_DUTY, Profile, TemperatureCurve,
    ValidationError,
};
use kori_core::schema::{REGENERATE_COMMAND, ROOTS, SCHEMA_PATH, protocol_schema_text};
use kori_core::telemetry::{PwmMode, SafetyAlert, TelemetrySnapshot, Unavailable};
use serde::Serialize;
use serde_json::{Value, json};

use common::Harness;
use support::{dead_socket, korictl};

fn committed_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(SCHEMA_PATH)
}

fn committed() -> Value {
    let text = std::fs::read_to_string(committed_path()).unwrap();
    serde_json::from_str(&text).unwrap()
}

/// A validator for one definition of the committed document.
fn validator(definition: &str) -> jsonschema::Validator {
    let document = committed();
    let schema = json!({
        "$schema": document["$schema"],
        "$defs": document["$defs"],
        "$ref": format!("#/$defs/{definition}"),
    });
    jsonschema::validator_for(&schema).unwrap()
}

fn assert_valid(validator: &jsonschema::Validator, definition: &str, instance: &Value) {
    let errors: Vec<String> = validator
        .iter_errors(instance)
        .map(|error| format!("{} at {}", error, error.instance_path()))
        .collect();
    assert!(
        errors.is_empty(),
        "{definition} refuses {instance}:\n  {}",
        errors.join("\n  ")
    );
}

/// The tags a tagged enum's definition lists, read from the schema itself.
fn tags(definition: &str, tag: &str) -> BTreeSet<String> {
    let document = committed();
    document["$defs"][definition]["oneOf"]
        .as_array()
        .unwrap_or_else(|| panic!("{definition} is not a oneOf"))
        .iter()
        .map(|variant| {
            variant["properties"][tag]["const"]
                .as_str()
                .unwrap_or_else(|| panic!("a {definition} variant has no {tag} const"))
                .to_string()
        })
        .collect()
}

fn encode(value: &impl Serialize) -> Value {
    serde_json::to_value(value).unwrap()
}

#[test]
fn the_committed_schema_is_what_the_wire_types_generate() {
    let generated = protocol_schema_text();
    let committed = std::fs::read_to_string(committed_path()).unwrap_or_default();
    assert!(
        generated == committed,
        "{SCHEMA_PATH} is out of date with the wire types in kori-core. \
         Regenerate it with:\n\n    {REGENERATE_COMMAND}\n\n\
         then review the diff and commit it with the change that caused it."
    );
}

#[test]
fn the_document_holds_every_root_under_one_schema() {
    let document = committed();
    assert_eq!(
        document["$schema"],
        "https://json-schema.org/draft/2020-12/schema"
    );
    assert_eq!(document["x-protocol-version"], PROTOCOL_VERSION);
    for root in ROOTS {
        assert!(document["$defs"][root].is_object(), "{root} is missing");
    }
    let frames: Vec<&str> = document["oneOf"]
        .as_array()
        .unwrap()
        .iter()
        .map(|frame| frame["$ref"].as_str().unwrap())
        .collect();
    assert_eq!(
        frames,
        ["#/$defs/Request", "#/$defs/Response", "#/$defs/Push"]
    );
    // The document is itself a valid draft 2020-12 schema.
    assert!(jsonschema::meta::is_valid(&document));
}

fn curve(duty: u8) -> TemperatureCurve {
    TemperatureCurve::from_points([duty; kori_core::profile::CURVE_POINT_COUNT])
}

fn client(connection: u64) -> Attribution {
    Attribution::Client {
        connection,
        client: Some("korictl".into()),
        pid: 4242,
    }
}

fn lit() -> LightingCommand {
    LightingCommand {
        channel: 1,
        program: LightingProgram::Effect {
            effect: LightingEffect::Breathing,
            colors: vec![Rgb {
                r: 255,
                g: 64,
                b: 0,
            }],
            brightness: Brightness::FULL,
            speed: EffectSpeed::Normal,
            direction: EffectDirection::Forward,
        },
    }
}

fn status() -> DaemonStatus {
    let kraken = DeviceId::new(0x1e71, 0x300e);
    DaemonStatus {
        daemon_version: "0.1.1".into(),
        protocol_version: PROTOCOL_VERSION,
        started_at_unix_ms: 1_700_000_000_000,
        clients: vec![ConnectedClient {
            connection: 3,
            client: Some("claude-code".into()),
            pid: 4242,
            connected_at_unix_ms: 1_700_000_000_500,
        }],
        access: AccessMode::ReadOnly {
            conflicts: vec![OwnershipConflict {
                device: Some(kraken),
                resource: "/sys/class/hwmon/hwmon3/pwm1".into(),
                detail: "permission denied".into(),
            }],
        },
        devices: vec![DeviceStatus {
            id: kraken,
            present: true,
            owned: false,
            writable: vec![CapabilityId::PumpDuty],
            blocked: vec![BlockedCapability {
                capability: CapabilityId::FanCurve,
                reason: "read-only".into(),
            }],
        }],
        active_profile: "Quiet".into(),
        config: ConfigState::Recovered {
            detail: "expected schema 4, found 9".into(),
            preserved_path: "/home/a/.config/kori/config.toml.corrupt".into(),
            recovery_action: "Re-save a profile to replace it.".into(),
        },
        cooling: CoolingState {
            committed: Some(CoolingProgram::Curve {
                pump: curve(128),
                fan: curve(64),
            }),
            revision: Revision {
                revision: 4,
                changed_by: client(3),
            },
        },
        lighting: vec![ChannelState {
            channel: 1,
            accessories: vec!["F120 RGB Core".into()],
            committed: Some(LightingProgram::Off),
            revision: Revision::initial(),
        }],
        display: DisplayState {
            panel: None,
            committed: Some(DisplayPreset::default_infographic()),
            streaming: true,
            revision: Revision::initial(),
            faulted: Some("transfer timed out".into()),
            dropped_frames: 1,
            frames_sent: 42,
            failed_transfers: 1,
            last_frame_at_unix_ms: Some(1_700_000_001_000),
            animation_started_at_unix_ms: None,
        },
        socket_path: "/run/user/1000/kori/kori.sock".into(),
    }
}

fn history() -> HistoryReply {
    let mut metrics = BTreeMap::new();
    metrics.insert(
        HistoryMetric::LiquidTemperature,
        MetricHistory {
            unit: "celsius".into(),
            summary: Some(Summary {
                min: 30.5,
                avg: 31.0,
                max: 31.5,
                last: 31.2,
                samples: 3,
            }),
            avg: Some(vec![Some(30.5), None, Some(31.5)]),
            min: Some(vec![Some(30.5), None, Some(31.5)]),
            max: Some(vec![Some(30.5), None, Some(31.5)]),
        },
    );
    metrics.insert(
        HistoryMetric::FanSpeed,
        MetricHistory {
            unit: "rpm".into(),
            summary: None,
            avg: None,
            min: None,
            max: None,
        },
    );
    HistoryReply {
        end_unix_ms: 1_700_000_000_000,
        step_s: 1,
        slots: 3,
        available_s: 2,
        metrics,
    }
}

fn event() -> kori_core::diagnostics::DiagnosticEvent {
    let mut log = DiagnosticsLog::default();
    log.record(
        7,
        EventKind::ProfileSaved {
            name: "Quiet".into(),
            by: client(3),
        },
    );
    log.events().next().unwrap().clone()
}

fn requests() -> Vec<Request> {
    vec![
        Request::Hello {
            protocol_version: PROTOCOL_VERSION,
            client: Some("claude-code".into()),
        },
        Request::Status,
        Request::Capabilities,
        Request::Profiles,
        Request::SaveProfile {
            profile: Profile {
                name: "Quiet".into(),
                program: CoolingProgram::Fixed {
                    pump: MIN_PUMP_DUTY,
                    fan: 0,
                },
                device: Some(DeviceId::new(0x1e71, 0x300e)),
                lighting: vec![lit()],
                display: Some(DisplayPreset::default_infographic()),
            },
        },
        Request::ActivateProfile {
            name: "Quiet".into(),
        },
        Request::DeleteProfile {
            name: "Quiet".into(),
        },
        Request::Telemetry,
        Request::ApplyProgram {
            program: CoolingProgram::Curve {
                pump: curve(128),
                fan: curve(0),
            },
            expect_revision: Some(4),
        },
        Request::ApplyLighting {
            command: lit(),
            expect_revision: None,
        },
        Request::ApplyDisplay {
            preset: DisplayPreset::default_infographic(),
            expect_revision: Some(1),
        },
        Request::Diagnostics,
        Request::Events {
            after_seq: Some(10),
            limit: Some(50),
        },
        Request::History {
            metrics: Some(vec![HistoryMetric::CpuLoad, HistoryMetric::PumpDuty]),
            window_s: Some(3_600),
            step_s: Some(60),
            points: Some(true),
        },
        Request::Subscribe {
            topics: vec![Topic::Telemetry, Topic::Status, Topic::Events],
            interval_s: Some(5),
            after_seq: Some(3),
        },
    ]
}

fn responses() -> Vec<Response> {
    let record = CapabilityRecord {
        schema_version: CAPABILITY_SCHEMA_VERSION,
        context: ProbeContext {
            kernel_release: Evidenced::unknown("not read", "test"),
            probed_at_unix_ms: 0,
        },
        devices: Vec::new(),
        rejected: Vec::new(),
    };
    let readback = ApplyOutcome {
        hardware: HardwareState::Confirmed,
        writes: 2,
        deduplicated: false,
        readback: vec![ChannelReadback {
            channel: Channel::Pump,
            mode: Some(PwmMode::Fixed),
            duty: Some(128),
            curve_points_confirmed: None,
            mismatch: None,
        }],
    };
    let mut log = DiagnosticsLog::default();
    log.record(
        1,
        EventKind::ProfileSaved {
            name: "Quiet".into(),
            by: Attribution::Daemon,
        },
    );
    vec![
        Response::Hello {
            protocol_version: PROTOCOL_VERSION,
            daemon_version: "0.1.1".into(),
            connection: 1,
        },
        Response::Status(Box::new(status())),
        Response::Capabilities(Box::new(record)),
        Response::Profiles {
            active: "Onboard safe".into(),
            profiles: Vec::new(),
        },
        Response::Activated(ActivationOutcome {
            name: "Quiet".into(),
            hardware: HardwareState::Uncertain {
                reason: "readback differs".into(),
            },
            applied: Some(readback.clone()),
        }),
        Response::Saved {
            name: "Quiet".into(),
        },
        Response::Deleted {
            name: "Quiet".into(),
            activated_instead: Some("Onboard safe".into()),
        },
        Response::Telemetry(Box::new(TelemetrySnapshot::unavailable(
            0,
            Unavailable::absent("no hwmon"),
        ))),
        Response::Applied(Box::new(readback)),
        Response::Lit(LightingOutcome {
            channel: 1,
            program: LightingProgram::Off,
            hardware: HardwareState::NotApplied {
                reason: "read-only".into(),
            },
            writes: 0,
            deduplicated: true,
        }),
        Response::Shown(Box::new(DisplayOutcome {
            preset: DisplayPreset::default_infographic(),
            hardware: HardwareState::Confirmed,
            frames: 2,
            deduplicated: false,
            brightness_sent: true,
        })),
        Response::Diagnostics(log.export(0, "0.1.1", None)),
        Response::Events(log.page(None, 10)),
        Response::History(Box::new(history())),
        Response::Subscribed {
            topics: vec![Topic::Events],
            interval_s: 1,
        },
        Response::Error(IpcError::NoDevice),
    ]
}

fn pushes() -> Vec<Push> {
    let mut snapshot = TelemetrySnapshot::unavailable(0, Unavailable::unreadable("EACCES"));
    snapshot.alerts = vec![
        SafetyAlert::LiquidCritical {
            temperature_c: 61.0,
            threshold_c: 60.0,
        },
        SafetyAlert::ChannelStalled {
            channel: Channel::Fan,
            commanded_duty: 128,
            samples: 3,
            rpm: 0,
        },
    ];
    vec![
        Push::Telemetry(Box::new(snapshot)),
        Push::Status(Box::new(status())),
        Push::Event(event()),
        Push::Heartbeat { at_unix_ms: 1 },
        Push::Lagged { missed: 3 },
        Push::Closing {
            reason: "received SIGTERM".into(),
        },
    ]
}

fn lighting_errors() -> Vec<LightingError> {
    vec![
        LightingError::ColorDigits {
            expected: 6,
            actual: 5,
        },
        LightingError::ColorAlphabet {
            value: "zz0000".into(),
        },
        LightingError::BrightnessOutOfRange {
            value: 120,
            max: 100,
        },
        LightingError::ChannelUnknown {
            channel: 4,
            available: 3,
        },
        LightingError::ColorCount {
            effect: "Breathing".into(),
            min: 1,
            max: 8,
            actual: 0,
        },
        LightingError::DirectionUnsupported {
            effect: "Breathing".into(),
        },
        LightingError::CadenceTooFast {
            channel: 1,
            minimum_interval_ms: 100,
            elapsed_ms: 20,
        },
    ]
}

fn display_errors() -> Vec<DisplayError> {
    vec![
        DisplayError::Color {
            field: "background".into(),
            source: LightingError::ColorAlphabet {
                value: "zz0000".into(),
            },
        },
        DisplayError::ImagePathMissing,
        DisplayError::ImagePathTooLong {
            bytes: 5_000,
            max_bytes: 4_096,
        },
        DisplayError::ImageUndecodable {
            path: "/tmp/a.png".into(),
            detail: "truncated".into(),
        },
        DisplayError::ImageTooLarge {
            width: 9_000,
            height: 10,
            max: 8_192,
        },
        DisplayError::ImageTooManyPixels {
            pixels: 40_000_000,
            max: 32_000_000,
        },
        DisplayError::AnimationTooLong { max: 1_000 },
        DisplayError::PanelUnknown,
    ]
}

fn validation_errors() -> Vec<ValidationError> {
    vec![
        ValidationError::NameLength { min: 1, max: 48 },
        ValidationError::NameBlank,
        ValidationError::DutyOutOfRange {
            channel: Channel::Pump,
            value: 10,
            min: MIN_PUMP_DUTY,
            max: 255,
        },
        ValidationError::CurvePointCount {
            expected: 40,
            actual: 39,
        },
        ValidationError::CurveDutyOutOfRange {
            channel: Channel::Pump,
            temperature_c: 30,
            value: 10,
            min: MIN_PUMP_DUTY,
            max: 255,
        },
        ValidationError::Lighting {
            channel: 1,
            source: LightingError::ChannelUnknown {
                channel: 4,
                available: 3,
            },
        },
        ValidationError::Display {
            source: DisplayError::PanelUnknown,
        },
        ValidationError::CurveNotMonotonic {
            channel: Channel::Fan,
            previous: 100,
            previous_temperature_c: 30,
            value: 90,
            temperature_c: 31,
        },
    ]
}

fn errors() -> Vec<IpcError> {
    let mut errors = vec![
        IpcError::UnsupportedProtocol {
            requested: 9,
            supported: PROTOCOL_VERSION,
        },
        IpcError::Malformed {
            detail: "expected value at line 1".into(),
        },
        IpcError::FrameTooLarge {
            max_bytes: kori_core::ipc::MAX_FRAME_BYTES,
        },
        IpcError::HandshakeRequired,
        IpcError::PeerRejected {
            reason: "uid 1001 is not the daemon's user".into(),
        },
        IpcError::ReadOnly {
            reason: "no udev rule".into(),
        },
        IpcError::Incompatible {
            details: vec![Incompatibility {
                capability: CapabilityId::PumpCurve,
                reason: "not exposed".into(),
            }],
        },
        IpcError::ProfileNotFound {
            name: "Quiet".into(),
        },
        IpcError::ProfileNameUnavailable {
            name: "Onboard safe".into(),
        },
        IpcError::NoDevice,
        IpcError::Io {
            detail: "disk full".into(),
        },
        IpcError::Conflict {
            domain: WriteDomain::Lighting,
            channel: Some(1),
            expected: 3,
            current: 4,
            changed_by: client(2),
        },
        IpcError::Conflict {
            domain: WriteDomain::Cooling,
            channel: None,
            expected: 3,
            current: 4,
            changed_by: Attribution::Daemon,
        },
        IpcError::SubscriptionLimit {
            max: kori_core::ipc::MAX_SUBSCRIPTIONS,
        },
        IpcError::HistoryOutOfRange {
            limit: HistoryLimit::MaxValues,
            detail: "24 h of 10 metrics is 864000 values".into(),
        },
    ];
    errors.extend(validation_errors().into_iter().map(IpcError::Validation));
    errors.extend(lighting_errors().into_iter().map(IpcError::Lighting));
    errors.extend(display_errors().into_iter().map(IpcError::Display));
    errors
}

/// Validate every example against its definition and against the root, and
/// return the tags they covered.
fn check(definition: &str, tag: &str, examples: &[Value]) -> BTreeSet<String> {
    let specific = validator(definition);
    let root = jsonschema::validator_for(&committed()).unwrap();
    let mut covered = BTreeSet::new();
    for example in examples {
        assert_valid(&specific, definition, example);
        if definition != "IpcError" {
            assert_valid(&root, "the root", example);
        }
        covered.insert(example[tag].as_str().unwrap().to_string());
    }
    covered
}

#[test]
fn one_example_of_every_variant_validates_and_every_tag_is_covered() {
    let requests: Vec<Value> = requests().iter().map(encode).collect();
    let responses: Vec<Value> = responses().iter().map(encode).collect();
    let pushes: Vec<Value> = pushes().iter().map(encode).collect();
    // A refusal travels inside a response frame as well as on its own.
    let refusals: Vec<Value> = errors()
        .into_iter()
        .map(|error| encode(&Response::Error(error)))
        .collect();
    let errors: Vec<Value> = errors().iter().map(encode).collect();

    for (definition, tag, examples) in [
        ("Request", "request", &requests),
        ("Response", "response", &responses),
        ("Push", "push", &pushes),
        ("IpcError", "error", &errors),
    ] {
        let covered = check(definition, tag, examples);
        assert_eq!(
            covered,
            tags(definition, tag),
            "the {definition} examples do not cover every variant the schema lists"
        );
    }
    check("Response", "response", &refusals);

    for (definition, examples) in [
        ("ValidationError", encode(&validation_errors())),
        ("LightingError", encode(&lighting_errors())),
        ("DisplayError", encode(&display_errors())),
    ] {
        let covered: BTreeSet<String> = examples
            .as_array()
            .unwrap()
            .iter()
            .map(|example| example["kind"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(covered, tags(definition, "kind"), "{definition}");
    }
}

/// What a daemon on a fake machine writes, not what a test constructed.
#[test]
fn what_a_real_daemon_answers_validates_against_the_schema() {
    let harness = Harness::start_lcd("cli-schema", "2.0.0");
    harness.wait_for_telemetry(
        &mut harness.client(),
        std::time::Duration::from_secs(5),
        |snapshot| snapshot.kraken.present,
    );
    let responses = validator("Response");
    let refusals = validator("IpcError");
    let pushes = validator("Push");

    for command in [
        &["status"][..],
        &["telemetry"],
        &["capabilities"],
        &["profiles"],
        &["diagnostics"],
        &["events"],
        &["history", "--points"],
        &["history", "--window", "1h", "--points"],
        &["profile", "activate", "Nope"],
        &["set", "cooling", r#"{"mode":"fixed","pump":10,"fan":0}"#],
    ] {
        let run = korictl(harness.socket(), command);
        let document = run.document();
        let label = format!("korictl {}", command.join(" "));
        // A reply is printed as the frame it travelled in, a refusal as the
        // bare error object inside it.
        if document.get("response").is_some() {
            assert_eq!(run.code, 0, "{run:?}");
            assert_valid(&responses, &label, &document);
        } else {
            assert_eq!(run.code, 1, "{run:?}");
            assert_valid(&refusals, &label, &document);
        }
    }

    let run = korictl(harness.socket(), &["watch", "--count", "4"]);
    assert_eq!(run.code, 0, "{run:?}");
    let lines: Vec<&str> = run.stdout.lines().collect();
    assert!(lines.len() >= 4, "{run:?}");
    for line in lines {
        let push: Value = serde_json::from_str(line).unwrap();
        assert_valid(&pushes, "korictl watch", &push);
    }
}

/// Each struct that denies unknown fields, reached through a request that
/// carries it, with one key it does not know.
#[test]
fn an_unknown_field_in_a_strict_struct_is_refused_by_the_schema() {
    let examples = requests();
    let save = encode(&examples[4]);
    let apply = encode(&examples[8]);
    let light = encode(&examples[9]);
    let show = encode(&examples[10]);
    let requests = validator("Request");

    let cases: [(&str, Value, &str); 6] = [
        ("Profile", save.clone(), "/profile"),
        ("LightingCommand", light, "/command"),
        ("Rgb", save.clone(), "/profile/lighting/0/program/colors/0"),
        ("TemperatureCurve", apply, "/program/pump"),
        ("DisplayPreset", show.clone(), "/preset"),
        ("ReadingSlot", show, "/preset/readings/0"),
    ];
    for (name, valid, pointer) in cases {
        assert_valid(&requests, name, &valid);
        let mut invalid = valid.clone();
        invalid
            .pointer_mut(pointer)
            .unwrap_or_else(|| panic!("{pointer} is not in {valid}"))
            .as_object_mut()
            .unwrap()
            .insert("unexpected".into(), json!(1));
        assert!(
            !requests.is_valid(&invalid),
            "{name} accepted an unknown field at {pointer}"
        );
    }

    // The schema says what the daemon does: the same payload is refused there.
    let harness = Harness::start("cli-schema-strict");
    let run = korictl(
        harness.socket(),
        &[
            "profile",
            "save",
            r#"{"name":"Quiet","program":{"mode":"onboard"},"unexpected":1}"#,
        ],
    );
    assert_eq!(run.code, 1, "{run:?}");
    assert_eq!(run.document()["error"], "malformed");
}

/// Units the name of a field promises, which its description must state.
const UNITS: &[(&str, &str)] = &[
    ("temperature", "celsius"),
    ("_c", "celsius"),
    ("rpm", "rpm"),
    ("duty", "0 to 255"),
    ("percent", "percent"),
    ("_ms", "milliseconds"),
    ("_s", "seconds"),
    ("bytes", "bytes"),
];

fn is_numeric(schema: &Value, definitions: &Value) -> bool {
    if let Some(reference) = schema["$ref"].as_str() {
        let name = reference.trim_start_matches("#/$defs/");
        // A reading is numeric when what it carries is.
        return ["Reading_", "Evidenced_"]
            .iter()
            .any(|prefix| name.starts_with(prefix))
            && definitions[name]["oneOf"]
                .as_array()
                .unwrap()
                .iter()
                .any(|variant| is_numeric(&variant["properties"]["value"], definitions));
    }
    let numeric = |kind: &Value| matches!(kind.as_str(), Some("integer" | "number"));
    match &schema["type"] {
        Value::Array(kinds) => kinds.iter().any(numeric),
        Value::String(_) if schema["type"] == "array" => is_numeric(&schema["items"], definitions),
        kind @ Value::String(_) => numeric(kind),
        _ => ["anyOf", "oneOf"].iter().any(|key| {
            schema[key]
                .as_array()
                .is_some_and(|options| options.iter().any(|o| is_numeric(o, definitions)))
        }),
    }
}

fn walk(at: &str, schema: &Value, definitions: &Value, missing: &mut Vec<String>) {
    match schema {
        Value::Object(map) => {
            if let Some(Value::Object(properties)) = map.get("properties") {
                for (name, property) in properties {
                    if !is_numeric(property, definitions) {
                        continue;
                    }
                    let description = property["description"]
                        .as_str()
                        .unwrap_or_default()
                        .to_lowercase();
                    for (marker, unit) in UNITS {
                        let named = if marker.starts_with('_') {
                            name.ends_with(marker)
                        } else {
                            name.contains(marker)
                        };
                        if named && !description.contains(unit) {
                            missing.push(format!("{at}.{name} does not say {unit:?}"));
                        }
                    }
                }
            }
            for (key, value) in map {
                walk(&format!("{at}/{key}"), value, definitions, missing);
            }
        }
        Value::Array(items) => {
            for item in items {
                walk(at, item, definitions, missing);
            }
        }
        _ => {}
    }
}

#[test]
fn every_numeric_field_states_its_unit_and_bound() {
    let document = committed();
    let definitions = &document["$defs"];
    let mut missing = Vec::new();
    walk("", definitions, definitions, &mut missing);
    assert!(missing.is_empty(), "{}", missing.join("\n"));

    // The bounds a client most needs are constraints, not only prose.
    let fixed = &definitions["CoolingProgram"]["oneOf"][1]["properties"];
    assert_eq!(fixed["pump"]["minimum"], MIN_PUMP_DUTY);
    assert!(
        fixed["pump"]["description"]
            .as_str()
            .unwrap()
            .contains(&MIN_PUMP_DUTY.to_string())
    );
    assert_eq!(fixed["fan"]["maximum"], 255);
    let points = &definitions["TemperatureCurve"]["properties"]["points"];
    assert_eq!(points["minItems"], kori_core::profile::CURVE_POINT_COUNT);
    assert_eq!(points["maxItems"], kori_core::profile::CURVE_POINT_COUNT);
    assert_eq!(points["items"]["maximum"], 255);
    assert_eq!(
        definitions["Brightness"]["maximum"],
        kori_core::lighting::MAX_BRIGHTNESS
    );
}

/// The frames `docs/protocol.md` shows, one per line in each `json` block.
fn documented_frames() -> Vec<Value> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../docs/protocol.md");
    let text = std::fs::read_to_string(path).unwrap();
    let mut frames = Vec::new();
    let mut in_json = false;
    for line in text.lines() {
        match line.trim() {
            "```json" => in_json = true,
            "```" => in_json = false,
            frame if in_json && !frame.is_empty() => frames.push(
                serde_json::from_str(frame)
                    .unwrap_or_else(|error| panic!("{error}: protocol.md shows {frame}")),
            ),
            _ => {}
        }
    }
    frames
}

/// The protocol document cannot show a frame the daemon would not send or
/// accept, and it shows one of every request.
#[test]
fn every_frame_in_the_protocol_document_validates() {
    let root = jsonschema::validator_for(&committed()).unwrap();
    let frames = documented_frames();
    for frame in &frames {
        assert_valid(&root, "docs/protocol.md", frame);
    }
    let shown: BTreeSet<String> = frames
        .iter()
        .filter_map(|frame| frame["request"].as_str().map(str::to_string))
        .collect();
    assert_eq!(
        shown,
        tags("Request", "request"),
        "docs/protocol.md lacks a wire example for a request, or shows one that does not exist"
    );
    let text = std::fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../docs/protocol.md"),
    )
    .unwrap();
    for tag in tags("Push", "push")
        .iter()
        .chain(&tags("IpcError", "error"))
    {
        assert!(
            text.contains(&format!("`{tag}`")),
            "docs/protocol.md never names `{tag}`"
        );
    }
}

#[test]
fn korictl_schema_prints_the_committed_document_without_a_daemon() {
    let run = korictl(&dead_socket("schema"), &["schema"]);
    assert_eq!(run.code, 0, "{run:?}");
    assert_eq!(run.stderr, "");
    assert_eq!(
        run.stdout,
        std::fs::read_to_string(committed_path()).unwrap()
    );
}

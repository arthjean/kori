// SPDX-FileCopyrightText: 2026 Arthur Jean
// SPDX-License-Identifier: GPL-3.0-or-later

//! The protocol as one JSON Schema document, generated from the wire types.
//!
//! Compiled only with the `schema` feature, which no shipped binary enables.
//! The generated document is committed as `docs/protocol.schema.json` and that
//! file is what `korictl schema` prints, so a client learns the surface without
//! the derives ever reaching a release build. A test in `crates/cli` fails when
//! the file and these types disagree, and names [`REGENERATE_COMMAND`].
//!
//! The schema describes what a reader accepts (schemars' deserialize contract):
//! a field the daemon leaves out when empty is optional here, which is what a
//! client building a request needs to know.

use schemars::generate::SchemaSettings;
use serde_json::{Map, Value, json};

use crate::ipc::{IpcError, PROTOCOL_VERSION, Push, Request, Response};

/// Where the committed document lives, relative to the workspace root.
pub const SCHEMA_PATH: &str = "docs/protocol.schema.json";

/// The command that rewrites [`SCHEMA_PATH`] from the current types.
pub const REGENERATE_COMMAND: &str = "cargo run --quiet -p kori-core --features schema \
     --example protocol_schema > docs/protocol.schema.json";

/// The four top-level definitions, in the order the document lists them.
pub const ROOTS: [&str; 4] = ["Request", "Response", "Push", "IpcError"];

/// Every request, reply, push and error in one draft 2020-12 document.
///
/// The root accepts any frame a socket carries (a request, a response or a
/// push); `IpcError` sits beside them under `$defs` because it is also what
/// `korictl` prints on a refusal, outside any response frame.
pub fn protocol_schema() -> Value {
    let mut generator = SchemaSettings::draft2020_12().into_generator();
    let frames = [
        generator.subschema_for::<Request>().to_value(),
        generator.subschema_for::<Response>().to_value(),
        generator.subschema_for::<Push>().to_value(),
    ];
    // Generated for its definition alone: a response embeds it, and a refusal
    // printed by the command line is this object on its own.
    let _ = generator.subschema_for::<IpcError>();
    let definitions = generator.take_definitions(true);

    let document = json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "title": format!("Kori protocol {PROTOCOL_VERSION}"),
        "description": "Every frame korid and its clients exchange over the Unix socket: \
            newline-delimited JSON, one object per line, at most 262144 bytes per frame. \
            A request carries a `request` tag, a reply a `response` tag and a pushed \
            frame a `push` tag. A refusal is a `response` of `error` whose body is an \
            `IpcError`. See docs/protocol.md.",
        "x-protocol-version": PROTOCOL_VERSION,
        "x-regenerate": REGENERATE_COMMAND,
        "oneOf": frames,
        "$defs": definitions,
    });
    canonical(document)
}

/// The committed form: keys sorted at every level, two-space indent, one
/// trailing newline.
///
/// Sorted by hand rather than left to the map type, because whether
/// `serde_json` keeps insertion order depends on a feature another crate in the
/// build may switch on, and the committed bytes must not depend on which crate
/// was built alongside.
pub fn protocol_schema_text() -> String {
    let mut text = serde_json::to_string_pretty(&protocol_schema()).unwrap_or_default();
    text.push('\n');
    text
}

fn canonical(value: Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut entries: Vec<(String, Value)> = map.into_iter().collect();
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            let mut sorted = Map::new();
            for (key, value) in entries {
                sorted.insert(key, canonical(value));
            }
            Value::Object(sorted)
        }
        Value::Array(items) => Value::Array(items.into_iter().map(canonical).collect()),
        other => other,
    }
}

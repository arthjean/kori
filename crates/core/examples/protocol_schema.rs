// SPDX-FileCopyrightText: 2026 Arthur Jean
// SPDX-License-Identifier: GPL-3.0-or-later

//! Print the protocol schema to stdout.
//!
//! ```text
//! cargo run --quiet -p kori-core --features schema --example protocol_schema \
//!     > docs/protocol.schema.json
//! ```

use std::io::Write;

fn main() -> std::io::Result<()> {
    std::io::stdout().write_all(kori_core::schema::protocol_schema_text().as_bytes())
}

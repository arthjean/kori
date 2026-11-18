// SPDX-FileCopyrightText: 2026 Arthur Jean
// SPDX-License-Identifier: GPL-3.0-or-later

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

//! `korictl set ... --if-revision`, against a daemon serving a fake machine.

#[path = "../../daemon/tests/common/mod.rs"]
mod common;
mod support;

use std::time::Duration;

use kori_core::profile::MIN_PROGRAM_INTERVAL_MS;

use common::{Harness, read_attribute};
use support::korictl;

#[test]
fn a_set_with_a_stale_revision_is_refused_with_the_conflict() {
    let harness = Harness::start("cli-if-revision");
    let hwmon = harness.hwmon_path();

    let run = korictl(
        harness.socket(),
        &[
            "set",
            "cooling",
            "--if-revision",
            "1",
            r#"{"mode":"fixed","pump":180,"fan":90}"#,
        ],
    );
    assert_eq!(run.code, 0, "{run:?}");
    std::thread::sleep(Duration::from_millis(MIN_PROGRAM_INTERVAL_MS + 20));

    // The option may also follow the payload.
    let run = korictl(
        harness.socket(),
        &[
            "set",
            "cooling",
            r#"{"mode":"fixed","pump":220,"fan":60}"#,
            "--if-revision=1",
        ],
    );
    assert_eq!(run.code, 1, "{run:?}");
    let conflict = run.document();
    assert_eq!(conflict["error"], "conflict");
    assert_eq!(conflict["domain"], "cooling");
    assert_eq!(conflict["expected"], 1);
    assert_eq!(conflict["current"], 2);
    assert_eq!(conflict["changed_by"]["client"], "korictl");
    assert_eq!(read_attribute(&hwmon, "pwm1"), "180", "nothing was written");

    let status = korictl(harness.socket(), &["status"]).document();
    assert_eq!(status["cooling"]["revision"], 2);

    let run = korictl(
        harness.socket(),
        &["set", "cooling", "--if-revision", "two", "{}"],
    );
    assert_eq!(run.code, 2, "{run:?}");
    assert_eq!(run.document()["token"], "two");
}

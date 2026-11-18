// SPDX-FileCopyrightText: 2026 Arthur Jean
// SPDX-License-Identifier: GPL-3.0-or-later

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

//! `korictl`, the command-line client of the Kori daemon.
//!
//! Built for a program on the other end, an agent or a script, before a person:
//! JSON only on stdout, one sentence on stderr when something failed, and an
//! exit code that says which kind of failure it was. Read commands and write
//! commands are told apart by their first word (`set` and `profile` write,
//! nothing else does), so a permission rule can allow one family and prompt on
//! the other.
//!
//! It validates nothing a payload says. The daemon revalidates every value
//! against its safety floors before any write, and a second, weaker copy of
//! those rules here would only be a second place for them to disagree.

mod command;
mod run;

use std::process::ExitCode;

fn main() -> ExitCode {
    let environment = run::Environment::from_process();
    let exit = run::run(
        std::env::args_os().skip(1),
        &environment,
        &mut std::io::stdin().lock(),
        &mut std::io::stdout().lock(),
        &mut std::io::stderr().lock(),
    );
    ExitCode::from(exit.code())
}

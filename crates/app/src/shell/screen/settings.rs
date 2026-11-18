// SPDX-FileCopyrightText: 2026 Arthur Jean
// SPDX-License-Identifier: GPL-3.0-or-later

//! The Settings screen: local paths, versions and diagnostics.
//!
//! The permanent home of the provenance the monitoring strip does not carry.
//! One row per fact, in a label-and-value shape, because that is what these
//! are: static facts an operator reads once, or quotes in a report.

use gpui::{Div, SharedString, div, prelude::*};

use crate::components::{eyebrow, section_title};
use crate::shell::Shell;
use crate::shell::visible::configuration_fact;
use crate::theme::{META_SEPARATOR, UNOFFICIAL_NOTICE, color, text};

use super::{block, screen, setting_card, setting_row};

/// How an operator exports diagnostics.
///
/// A sentence rather than a button. The window never sent `diagnostics`, so
/// the button it used to carry did nothing; the command line does exactly
/// this, writes JSON a report can quote, and works whether or not the window
/// is open. The row is the same with the daemon running or not: it states
/// where the export lives, which is true either way, and puts no disabled
/// control in front of the operator.
pub const DIAGNOSTICS_HINT: &str =
    "Run korictl diagnostics to export. Serial numbers are redacted.";

impl Shell {
    /// Every fact the screen states, as label and value: the Service card,
    /// then the Devices card.
    ///
    /// Built for the screen and for the key that decides whether a sample
    /// changed it, so the two cannot drift: a fact the screen shows is a fact
    /// the key compares. Nothing here moves with every sample. The sampling
    /// row used to add how long ago the last pass completed, which changed
    /// every second and was the only reason Settings ever needed to repaint.
    pub(crate) fn settings_facts(&self) -> Vec<(SharedString, SharedString)> {
        let mut facts = self.service_facts();
        facts.extend(self.device_settings());
        facts
    }

    fn service_facts(&self) -> Vec<(SharedString, SharedString)> {
        let (socket, version, config) = match self.link.status() {
            Some(status) => (
                status.socket_path.clone(),
                status.daemon_version.clone(),
                configuration_fact(&status.config),
            ),
            None => (
                "not connected".to_string(),
                "unknown".to_string(),
                "not available".to_string(),
            ),
        };
        let sampling = match self.link.telemetry() {
            Some(snapshot) => format!("Every {} ms", snapshot.interval_ms),
            None => "no telemetry".to_string(),
        };

        let mut facts: Vec<(SharedString, SharedString)> = vec![
            ("Socket".into(), socket.into()),
            ("Daemon version".into(), version.into()),
            ("Configuration".into(), config.into()),
            ("Sampling".into(), sampling.into()),
            (
                "Network".into(),
                "No network request, no listening TCP or UDP socket.".into(),
            ),
        ];
        for failure in self.link.failed_collectors() {
            facts.push((
                format!("{} collector", failure.collector.label()).into(),
                failure.detail.clone().into(),
            ));
        }
        facts
    }

    pub(crate) fn settings(&self) -> Div {
        let rows = |facts: Vec<(SharedString, SharedString)>| {
            facts
                .into_iter()
                .map(|(label, value)| setting_row(label, value))
                .collect::<Vec<_>>()
        };
        let service = rows(self.service_facts());
        let devices = rows(self.device_settings());

        screen("Settings", "Local paths, versions and diagnostics.")
            .child(block(eyebrow("Service"), setting_card(service)))
            .child(block(
                section_title(
                    "Devices",
                    Some("Only the two allowlisted devices are ever opened.".into()),
                ),
                setting_card(devices),
            ))
            .child(block(
                eyebrow("Diagnostics"),
                setting_card([div()
                    .debug_selector(|| "settings-diagnostics".to_string())
                    .child(setting_row("Export", DIAGNOSTICS_HINT))]),
            ))
            .child(
                div()
                    .px(gpui::px(2.0))
                    .text_size(text::LABEL_SM)
                    .text_color(color::TEXT_MUTED.hsla())
                    .child(UNOFFICIAL_NOTICE),
            )
    }

    /// Every device and everything known about it, for the diagnostics screen.
    ///
    /// The permanent home of the provenance the monitoring strip no longer
    /// carries. One row per device, in the same label-and-value shape as the
    /// service rows beside it, because that is what these are: static facts an
    /// operator reads once, or quotes in a report.
    ///
    /// The state is the first fragment rather than a colored word at the end.
    /// On this screen it is one more recorded fact, not a signal to act on; the
    /// screen that asks for action is the one that colors it.
    fn device_settings(&self) -> Vec<(SharedString, SharedString)> {
        let rows = self.link.device_rows();
        if rows.is_empty() {
            return vec![(
                "Devices".into(),
                "No supported NZXT device detected.".into(),
            )];
        }

        rows.into_iter()
            .map(|summary| {
                (
                    summary.name.clone().into(),
                    format!(
                        "{} {META_SEPARATOR} {} {META_SEPARATOR} {}",
                        summary.health.label(),
                        summary.id,
                        summary.detail()
                    )
                    .into(),
                )
            })
            .collect()
    }
}

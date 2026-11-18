// SPDX-FileCopyrightText: 2026 Arthur Jean
// SPDX-License-Identifier: GPL-3.0-or-later

//! The Monitoring screen: the current state, and the temperature trend.
//!
//! Readouts and their provenance, and nothing operable. Four dials for the
//! values that have a limit to be read against, and one line chart for the
//! three temperatures that run close together. That is all: one glance gives
//! where the machine is and where it has been, and every number appears once.
//! The load area, the cooling columns and the headroom radar each repeated a
//! value a dial or the legend already stated.
//!
//! The chart's points are the daemon's. It records the series by tick whether
//! or not a window is open, so the chart shows the full fifteen minutes the
//! moment the screen opens, and the window keeps no series of its own. The
//! screen asks for them when it becomes visible, on reconnection, and every
//! five seconds while it stays visible; never while another screen is shown.
//!
//! A device that is ready says so by the readings under it moving; the strip
//! at the top speaks only for a device that is not.

use gpui::{Div, FontWeight, Hsla, SharedString, div, prelude::*, px};

use kori_core::history::{HistoryMetric, HistoryReply, Summary};
use kori_core::telemetry::{
    LIQUID_CRITICAL_C, MetricView, format_binary_bytes, format_temperature,
};

use crate::components::chart::{self, Plotted, RadialGauge, Scale, SeriesChart};
use crate::components::{
    DeviceHealth, DeviceRow, Note, NoteLevel, eyebrow, panel_surface, section_title,
};
use crate::feed::{HistoryState, TREND_METRICS, TREND_STEP_S, TREND_WINDOW_S};
use crate::link::DeviceSummary;
use crate::shell::Shell;
use crate::theme::{Color, DEGREE_C, DEVICE_LINE_HEIGHT, color, numeric_font, space, text};

use super::{block, screen};

/// Points the chart plots per series: one per five seconds over the window.
pub const TREND_POINTS: usize = (TREND_WINDOW_S / TREND_STEP_S) as usize;
/// What the chart says while the daemon has recorded less than one point.
pub const HISTORY_STARTING: &str = "History starts with the background service.";
/// What the chart says while no daemon answers.
pub const HISTORY_UNREACHABLE: &str =
    "No temperature history while the background service is not running.";
/// Coolant temperature at the bottom of its dial: room temperature, about.
const LIQUID_FLOOR_C: f32 = 20.0;
/// Coolant temperature from which its dial turns to the warning color, ten
/// degrees under the alert.
const LIQUID_WARM_C: f32 = LIQUID_CRITICAL_C - 10.0;
/// Memory occupancy from which its dial turns to the warning color.
const MEMORY_HIGH_PERCENT: f32 = 90.0;

/// The temperature chart, as the last `history` answer left it.
///
/// Built once per answer rather than per repaint: the reply is reduced here to
/// the points the chart draws and their scale, so a render walks those points
/// and nothing else.
#[derive(Debug, Clone, PartialEq)]
pub enum Trend {
    /// Nothing answered yet. The grid is drawn, with no line and no sentence.
    Waiting,
    /// The daemon has recorded less than one point.
    Starting,
    /// Points to draw: one series per [`TREND_METRICS`] entry, oldest first,
    /// with the daemon's summary of the window beside each.
    Drawn {
        series: [Vec<Option<f32>>; 3],
        summaries: [Option<Summary>; 3],
        scale: Scale,
    },
    /// Why there is nothing to draw, in operator language.
    Refused(String),
}

impl Trend {
    pub fn from_state(state: &HistoryState) -> Self {
        match state {
            HistoryState::Answered(reply) => Self::from_reply(reply),
            HistoryState::Refused(reason) => Self::Refused(reason.clone()),
        }
    }

    /// Reduce one reply to what the chart draws.
    ///
    /// The `avg` array of each metric, and at most [`TREND_POINTS`] of it, the
    /// newest: a reply carrying more than it was asked for is not a reason to
    /// draw more than the chart has room for.
    pub fn from_reply(reply: &HistoryReply) -> Self {
        if reply.available_s < reply.step_s.max(1) {
            return Self::Starting;
        }
        let points = |metric: HistoryMetric| -> Vec<Option<f32>> {
            let Some(avg) = reply
                .metrics
                .get(&metric)
                .and_then(|history| history.avg.as_ref())
            else {
                return vec![None; TREND_POINTS];
            };
            let skip = avg.len().saturating_sub(TREND_POINTS);
            avg[skip..].to_vec()
        };
        let summary = |metric: HistoryMetric| {
            reply
                .metrics
                .get(&metric)
                .and_then(|history| history.summary)
        };
        let series = TREND_METRICS.map(points);
        let summaries = TREND_METRICS.map(summary);
        let readings: Vec<f32> = series.iter().flatten().flatten().copied().collect();
        let scale = if readings.is_empty() {
            default_scale()
        } else {
            Scale::nice(readings, 4, None)
        };
        Self::Drawn {
            series,
            summaries,
            scale,
        }
    }

    /// The daemon's summary of one metric over the window, when it has one.
    fn summary(&self, index: usize) -> Option<&Summary> {
        match self {
            Self::Drawn { summaries, .. } => summaries.get(index)?.as_ref(),
            _ => None,
        }
    }
}

/// The axis an empty chart is drawn on.
fn default_scale() -> Scale {
    Scale::fixed(20.0, 80.0, 3)
}

/// The devices the monitoring strip still has to speak for.
///
/// A ready device has nothing left to say there: everything it can do is
/// already offered, and everything it cannot is refused at the control that
/// tried, by [`crate::link::LinkState::control_state`]. A device in any other
/// state is the case the strip exists for, so the filter is written once and
/// tested, rather
/// than being an inline predicate that could quietly widen to `true` and take
/// the screen back to a permanent block, or narrow and hide a device that is
/// not answering.
fn degraded_devices(rows: Vec<DeviceSummary>) -> Vec<DeviceSummary> {
    rows.into_iter()
        .filter(|summary| summary.health != DeviceHealth::Ready)
        .collect()
}

fn percent(value: f32) -> String {
    format!("{value:.0}%")
}

fn celsius(value: f32) -> String {
    format!("{}{DEGREE_C}", format_temperature(value))
}

fn rpm(value: f32) -> String {
    format!("{} RPM", chart::grouped(value))
}

/// Share of the coolant dial a temperature fills.
fn liquid_share(value: f32) -> f32 {
    (value - LIQUID_FLOOR_C) / (LIQUID_CRITICAL_C - LIQUID_FLOOR_C)
}

/// A value as a readout writes it, or the marker for none.
fn readout(view: &MetricView<f32>, format: impl Fn(f32) -> String) -> String {
    view.copied().map_or_else(|| "--".to_string(), format)
}

/// One series of the legend: its key, its name, where it is now, and what the
/// daemon says about the window.
///
/// The value is in the text color and the series color stays on the key: a
/// number never wears the color of its line.
fn legend_item(
    name: &'static str,
    key: Hsla,
    view: &MetricView<f32>,
    summary: Option<&Summary>,
    format: impl Fn(f32) -> String,
) -> Div {
    let now = match (view.copied(), view.qualifier()) {
        (Some(value), Some(word)) => format!("{} {word}", format(value)),
        (None, Some(word)) => word.to_string(),
        (Some(value), None) => format(value),
        (None, None) => "--".to_string(),
    };
    let window = summary.map_or_else(
        || "No reading in this window".to_string(),
        |summary| format!("Avg {}  Peak {}", format(summary.avg), format(summary.max)),
    );
    div()
        .flex()
        .flex_col()
        .gap(px(2.0))
        .min_w(px(132.0))
        .child(
            div()
                .flex()
                .items_center()
                .gap(space::SM)
                .child(
                    div()
                        .flex_none()
                        .w(px(12.0))
                        .h(px(2.0))
                        .rounded(px(1.0))
                        .bg(key),
                )
                .child(
                    div()
                        .text_size(text::LABEL_SM)
                        .text_color(color::TEXT_MUTED.hsla())
                        .child(name),
                ),
        )
        .child(
            div()
                .pl(px(20.0))
                .text_size(text::TITLE)
                .font_weight(FontWeight::MEDIUM)
                .font(numeric_font())
                .text_color(if view.copied().is_some() && !view.is_stale() {
                    color::TEXT.hsla()
                } else {
                    color::TEXT_MUTED.hsla()
                })
                .child(now),
        )
        .child(
            div()
                .pl(px(20.0))
                .text_size(text::LABEL_SM)
                .font(numeric_font())
                .text_color(color::TEXT_MUTED.hsla())
                .child(window),
        )
}

fn legend(items: impl IntoIterator<Item = Div>) -> Div {
    div()
        .flex()
        .flex_wrap()
        .gap_x(space::XL)
        .gap_y(space::MD)
        .w_full()
        .min_w_0()
        .children(items)
}

/// One dial of the overview, with what it measures and a line of detail.
fn overview_cell(label: &'static str, gauge: RadialGauge, detail: String) -> Div {
    div()
        .flex()
        .flex_col()
        .items_center()
        .flex_1()
        .min_w(px(128.0))
        .gap(space::XS)
        .child(gauge.render())
        .child(
            div()
                .text_size(text::BODY_EMPHASIS)
                .font_weight(FontWeight::MEDIUM)
                .text_color(color::TEXT.hsla())
                .child(label),
        )
        .child(
            div()
                .text_size(text::LABEL_SM)
                .font(numeric_font())
                .text_color(color::TEXT_MUTED.hsla())
                .child(detail),
        )
}

/// A dial for one view: the value and its state, in words as well as color.
///
/// `severity` names the state of a fresh value past its threshold, with the
/// color that goes with it; it is never consulted for a value that is stale
/// or missing, whose own word wins.
fn gauge(
    view: &MetricView<f32>,
    share: impl Fn(f32) -> f32,
    format: impl Fn(f32) -> String,
    caption: &'static str,
    severity: impl Fn(f32) -> Option<(&'static str, Hsla)>,
) -> RadialGauge {
    let value = view.copied();
    let gauge = RadialGauge::new(value.map(&share), readout(view, &format), {
        match (view.qualifier(), value.and_then(&severity)) {
            (Some(word), _) => word,
            (None, Some((word, _))) => word,
            (None, None) => caption,
        }
    });
    match (view.qualifier(), value.and_then(&severity)) {
        (Some(_), _) => gauge.fill(color::TEXT_DISABLED.hsla()).muted(),
        (None, Some((_, fill))) => gauge.fill(fill),
        (None, None) => gauge,
    }
}

/// The series of the chart, in [`TREND_METRICS`] order: legend name and color.
const TREND_SERIES: [(&str, Color); 3] = [
    ("CPU package", color::SERIES_CPU),
    ("GPU core", color::SERIES_GPU),
    ("Coolant", color::SERIES_COOLANT),
];

impl Shell {
    pub(crate) fn monitoring(&self) -> Div {
        let now = self.now_unix_ms;
        let book = &self.metrics;

        let cpu_load = book.cpu_load.view(now);
        let cpu_temperature = book.cpu_temperature.view(now);
        let gpu_load = book.gpu_load.view(now);
        let gpu_temperature = book.gpu_temperature.view(now);
        let memory_percent = book.memory_percent.view(now);
        let liquid = book.liquid.view(now);
        let pump = book.pump.rpm.view(now);
        let fan = book.fan.rpm.view(now);

        // Overview: one dial per quantity that has a limit to be read against.
        let memory_detail = match book.memory.view(now).copied() {
            Some(usage) => format!(
                "{} of {}",
                format_binary_bytes(usage.used_bytes),
                format_binary_bytes(usage.total_bytes)
            ),
            None => "Size not readable".to_string(),
        };
        let overview = panel_surface().child(
            div()
                .flex()
                .flex_wrap()
                .gap_y(space::LG)
                .w_full()
                .child(overview_cell(
                    "CPU",
                    gauge(&cpu_load, |v| v / 100.0, percent, "load", |_| None),
                    format!("Package {}", readout(&cpu_temperature, celsius)),
                ))
                .child(overview_cell(
                    "GPU",
                    gauge(&gpu_load, |v| v / 100.0, percent, "load", |_| None),
                    format!("Core {}", readout(&gpu_temperature, celsius)),
                ))
                .child(overview_cell(
                    "Memory",
                    gauge(
                        &memory_percent,
                        |v| v / 100.0,
                        percent,
                        "in use",
                        |v| (v >= MEMORY_HIGH_PERCENT).then(|| ("High", color::WARNING.hsla())),
                    ),
                    memory_detail,
                ))
                .child(overview_cell(
                    "Coolant",
                    gauge(&liquid, liquid_share, celsius, "of 60 \u{00b0}C", |v| {
                        if v >= LIQUID_CRITICAL_C {
                            Some(("Critical", color::DANGER.hsla()))
                        } else if v >= LIQUID_WARM_C {
                            Some(("Warm", color::WARNING.hsla()))
                        } else {
                            None
                        }
                    }),
                    format!(
                        "Pump {}  Fan {}",
                        readout(&pump, chart::grouped),
                        readout(&fan, rpm)
                    ),
                )),
        );

        let temperatures = panel_surface()
            .child(self.temperature_chart().render())
            .child(legend(
                [&cpu_temperature, &gpu_temperature, &liquid]
                    .into_iter()
                    .zip(TREND_SERIES)
                    .enumerate()
                    .map(|(index, (view, (name, key)))| {
                        legend_item(name, key.hsla(), view, self.trend.summary(index), celsius)
                    }),
            ));

        screen(
            "Monitoring",
            "The current state, and the temperatures over the last 15 minutes.",
        )
        .children(self.device_strip())
        .children(self.link.failed_collectors().iter().map(|failure| {
            Note::new(
                NoteLevel::Warning,
                format!("{}: {}", failure.collector.label(), failure.detail),
            )
            .render()
        }))
        .child(block(eyebrow("Overview"), overview))
        .child(block(
            section_title(
                "Temperatures",
                Some("CPU package, GPU core and the coolant in the loop.".into()),
            ),
            temperatures,
        ))
        .child(
            div()
                .px(px(2.0))
                .text_size(text::LABEL_SM)
                .text_color(color::TEXT_MUTED.hsla())
                .child(
                    "Recorded by the background service in memory only. \
                     It restarts empty with the service.",
                ),
        )
    }

    /// What the chart area says instead of a line, when it says anything.
    ///
    /// Nothing while the first answer is on its way: the grid alone is the
    /// honest picture of a question not answered yet.
    pub(crate) fn chart_message(&self) -> Option<SharedString> {
        if self.link.status().is_none() {
            return Some(HISTORY_UNREACHABLE.into());
        }
        match &self.trend {
            Trend::Starting => Some(HISTORY_STARTING.into()),
            Trend::Refused(reason) => Some(reason.clone().into()),
            Trend::Waiting | Trend::Drawn { .. } => None,
        }
    }

    /// The chart as the last answer left it, or the reason there is none.
    ///
    /// A daemon that is not answering outranks whatever the last reply drew:
    /// the dials go to their disconnected state, and the chart says why it
    /// stopped rather than holding a trend nothing is extending.
    fn temperature_chart(&self) -> SeriesChart {
        let chart = |scale| {
            SeriesChart::new("monitoring-temperatures", scale, u64::from(TREND_WINDOW_S))
                .format(|v| format!("{v:.0}\u{00b0}"))
        };
        if let Some(message) = self.chart_message() {
            return chart(default_scale()).empty(message);
        }
        match &self.trend {
            Trend::Waiting | Trend::Starting | Trend::Refused(_) => chart(default_scale()),
            Trend::Drawn { series, scale, .. } => {
                let mut drawn = chart(scale.clone());
                for (values, (name, key)) in series.iter().zip(TREND_SERIES) {
                    drawn = drawn.series(Plotted {
                        name: SharedString::new_static(name),
                        color: key.hsla(),
                        values: values.clone(),
                    });
                }
                drawn
            }
        }
    }

    /// Which hardware answered, as a caption under the screen heading.
    ///
    /// Not a panel, and not always drawn. This used to be a titled card at the
    /// head of the screen, which gave the two devices the same weight as the CPU
    /// and GPU sections and spent a heading, a line of policy and four lines of
    /// prose to say "both devices are ready". Every fact on it was already
    /// carried better somewhere else:
    ///
    /// - The state word duplicates [`crate::link::LinkState::control_state`],
    ///   which is the single
    ///   gate every write passes through and which names the refusal on the
    ///   control the operator just tried to use, in language about that control.
    /// - The Kraken's presence is proven by its own readings further down this
    ///   screen. A device that stopped answering shows it in Liquid, Pump and
    ///   Fan, not in a line of provenance above them.
    /// - Firmware, kernel binding and the USB identity are static for a session
    ///   and are follow-up questions rather than glances, so they live on the
    ///   Settings screen, which is the diagnostics list.
    ///
    /// What is left is the exception, and only the exception: a device that is
    /// not [`DeviceHealth::Ready`] gets its full line here, because that is
    /// exactly when its firmware and its kernel binding are the evidence that
    /// explains the degradation. A machine with both devices ready draws
    /// nothing, and the screen opens on the readings it is named for.
    ///
    /// The dropped sentence about the allowlist is not a loss of meaning: it
    /// described a property of the process, not a state of the hardware. That
    /// policy is stated where it is enforced, in `ALLOWLIST`.
    fn device_strip(&self) -> Option<Div> {
        let strip = div().flex().flex_col().w_full().min_w_0().gap(space::XS);
        let rows = self.link.device_rows();

        // Nothing supported at all is itself the exception, and the one case
        // where no device line can carry the news.
        if rows.is_empty() {
            return Some(
                strip.child(
                    div()
                        .text_size(text::BODY)
                        .min_h(DEVICE_LINE_HEIGHT)
                        .text_color(color::TEXT_MUTED.hsla())
                        .child("No supported NZXT device detected."),
                ),
            );
        }

        let degraded = degraded_devices(rows);
        if degraded.is_empty() {
            return None;
        }

        Some(strip.children(degraded.into_iter().map(|summary| {
            DeviceRow::new(summary.name.clone(), summary.id.to_string(), summary.health)
                .detail(summary.detail())
                .render()
        })))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kori_core::{DeviceId, KRAKEN_BASE, RGB_CONTROLLER};

    fn device(id: DeviceId, health: DeviceHealth) -> DeviceSummary {
        DeviceSummary {
            id,
            name: "Device".to_string(),
            firmware: Some("0200".to_string()),
            driver: "kraken2023".to_string(),
            health,
        }
    }

    #[test]
    fn a_missing_value_reads_as_unavailable_never_as_zero() {
        let missing: MetricView<f32> = MetricView::Unavailable { cause: None };
        assert_eq!(readout(&missing, celsius), "--");
        assert_eq!(
            readout(&MetricView::Fresh { value: 51.0 }, celsius),
            "51.0 \u{00b0}C"
        );
        // An empty dial, not a dial at zero, and the state in a word.
        let dial = gauge(&missing, |v| v / 100.0, percent, "load", |_| None);
        assert_eq!(dial.fraction(), None);
        assert_eq!(dial.caption().as_ref(), "N/A");
    }

    #[test]
    fn a_dial_names_its_state_in_a_word_and_freshness_outranks_severity() {
        let critical =
            |v: f32| (v >= LIQUID_CRITICAL_C).then(|| ("Critical", color::DANGER.hsla()));
        let hot = MetricView::Fresh { value: 61.0 };
        assert_eq!(
            gauge(&hot, liquid_share, celsius, "of 60", critical)
                .caption()
                .as_ref(),
            "Critical"
        );
        // A stale reading is first of all stale: its alarm word would claim a
        // present state the reading no longer proves.
        let aging = MetricView::Stale {
            value: 61.0,
            age_ms: 3_000,
        };
        assert_eq!(
            gauge(&aging, liquid_share, celsius, "of 60", critical)
                .caption()
                .as_ref(),
            "Stale"
        );
        let calm = MetricView::Fresh { value: 31.0 };
        assert_eq!(
            gauge(&calm, liquid_share, celsius, "of 60", critical)
                .caption()
                .as_ref(),
            "of 60"
        );
    }

    #[test]
    fn the_coolant_dial_is_full_at_the_alert_threshold() {
        assert_eq!(liquid_share(LIQUID_FLOOR_C), 0.0);
        assert_eq!(liquid_share(LIQUID_CRITICAL_C), 1.0);
    }

    #[test]
    fn the_monitoring_strip_speaks_for_a_device_only_while_it_is_not_ready() {
        // A machine where both devices answered and both are writable draws no
        // strip at all: the screen opens on the readings it is named for, and
        // the provenance is on the diagnostics screen.
        assert!(
            degraded_devices(vec![
                device(KRAKEN_BASE, DeviceHealth::Ready),
                device(RGB_CONTROLLER, DeviceHealth::Ready),
            ])
            .is_empty()
        );

        // Every other state is the exception the strip exists for, and it is
        // named per device rather than collapsed into one line: a read-only
        // controller beside a ready Kraken is a different machine from one
        // where neither answered.
        for health in [DeviceHealth::ReadOnly, DeviceHealth::Unavailable] {
            let shown = degraded_devices(vec![
                device(KRAKEN_BASE, DeviceHealth::Ready),
                device(RGB_CONTROLLER, health),
            ]);
            assert_eq!(shown.len(), 1, "{health:?} was not reported");
            assert_eq!(shown[0].id, RGB_CONTROLLER);
            assert_eq!(shown[0].health, health);
        }
    }
}

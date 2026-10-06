// SPDX-FileCopyrightText: 2026 Arthur Jean
// SPDX-License-Identifier: GPL-3.0-or-later

//! The Monitoring screen: system and cooling state at a glance.
//!
//! Readouts and their provenance, and nothing operable. Each block is the
//! chart its data asks for: a dial for a value against its limit, an area for
//! a load with a floor, lines for temperatures that run close together,
//! columns for a speed averaged per minute, and a radar for every share of
//! capacity read as one shape. Every value a chart shows on hover is also
//! written in its legend, so the pointer is a shortcut and never the only way
//! to a number.
//!
//! A device that is ready says so by the readings under it moving; the strip
//! at the top speaks only for a device that is not.

use gpui::{Context, Div, FontWeight, Hsla, SharedString, Stateful, div, prelude::*, px};

use kori_core::telemetry::{
    HISTORY_WINDOW_MS, History, LIQUID_CRITICAL_C, MetricView, format_binary_bytes,
    format_temperature,
};

use crate::components::chart::{
    self, BarChart, Holes, Plotted, RadarAxis, RadarChart, RadarSeries, RadialGauge, Scale,
    SeriesChart, SeriesStyle,
};
use crate::components::{
    DeviceHealth, DeviceRow, Note, NoteLevel, eyebrow, panel_surface, section_title,
};
use crate::link::DeviceSummary;
use crate::shell::Shell;
use crate::theme::{Color, DEGREE_C, DEVICE_LINE_HEIGHT, color, numeric_font, space, text};

use super::{block, screen};

/// Points a trend chart plots over the window: one per five seconds.
const TREND_POINTS: usize = 180;
/// Columns of the cooling chart: one per minute.
const MINUTE_COLUMNS: usize = (HISTORY_WINDOW_MS / 60_000) as usize;
/// Coolant temperature at the bottom of its dial: room temperature, about.
const LIQUID_FLOOR_C: f32 = 20.0;
/// Coolant temperature from which its dial turns to the warning color, ten
/// degrees under the alert.
const LIQUID_WARM_C: f32 = LIQUID_CRITICAL_C - 10.0;
/// Chip temperature a radar spoke reads as its whole length.
const CHIP_SPAN_C: f32 = 100.0;
/// Memory occupancy from which its dial turns to the warning color.
const MEMORY_HIGH_PERCENT: f32 = 90.0;

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

/// One series of a legend: its key, its name, where it is now, and what the
/// window says about it.
///
/// The value is in the text color and the series color stays on the key: a
/// number never wears the color of its line.
fn legend_item(
    name: &'static str,
    key: Hsla,
    view: &MetricView<f32>,
    history: &History,
    format: impl Fn(f32) -> String,
) -> Div {
    let now = match (view.copied(), view.qualifier()) {
        (Some(value), Some(word)) => format!("{} {word}", format(value)),
        (None, Some(word)) => word.to_string(),
        (Some(value), None) => format(value),
        (None, None) => "--".to_string(),
    };
    let window = chart::summarize(history).map_or_else(
        || "No reading in this window".to_string(),
        |summary| {
            format!(
                "Avg {}  Peak {}",
                format(summary.average),
                format(summary.peak)
            )
        },
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

/// A radar point: a share of its limit, and its display value.
type SpokeValue = Option<(f32, String)>;

/// One spoke of the headroom radar: where its quantity is now, and the
/// highest it went in the window, each as a share of its limit.
fn spoke(
    view: &MetricView<f32>,
    history: &History,
    share: fn(f32) -> f32,
    format: fn(f32) -> String,
) -> (SpokeValue, SpokeValue) {
    let at = |value: f32| (share(value), format(value));
    (
        view.copied().map(at),
        chart::summarize(history).map(|summary| at(summary.peak)),
    )
}

/// The key of a radar series: no values of its own, since the axis labels and
/// the tooltip carry them.
fn radar_key(name: &'static str, key: Hsla) -> Div {
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
        )
}

/// Repaint a chart while the pointer moves over it and once as it leaves, so
/// its hover layer follows the pointer and goes away with it.
fn hoverable(chart: Stateful<Div>, cx: &mut Context<Shell>) -> Stateful<Div> {
    chart
        .on_mouse_move(cx.listener(|_, _, _, cx| cx.notify()))
        .on_hover(cx.listener(|_, _, _, cx| cx.notify()))
}

impl Shell {
    pub(crate) fn monitoring(&self, cx: &mut Context<Self>) -> Div {
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

        let trend = |history: &History| {
            chart::bucket(history, now, HISTORY_WINDOW_MS, TREND_POINTS, Holes::Keep)
        };
        let window_s = HISTORY_WINDOW_MS / 1_000;
        let plotted = |name: &'static str, key: Color, values| Plotted {
            name: SharedString::new_static(name),
            color: key.hsla(),
            values,
        };

        // Load: a share of capacity with a floor, so the area under the line
        // is part of the reading.
        let load_chart = SeriesChart::new(
            "monitoring-load",
            SeriesStyle::Area,
            Scale::fixed(0.0, 100.0, 4),
            window_s,
        )
        .series(plotted(
            "CPU",
            color::SERIES_CPU,
            trend(book.cpu_load.history()),
        ))
        .series(plotted(
            "GPU",
            color::SERIES_GPU,
            trend(book.gpu_load.history()),
        ))
        .format(percent, percent);
        let load = panel_surface()
            .child(hoverable(load_chart.render(), cx))
            .child(legend([
                legend_item(
                    "CPU",
                    color::SERIES_CPU.hsla(),
                    &cpu_load,
                    book.cpu_load.history(),
                    percent,
                ),
                legend_item(
                    "GPU",
                    color::SERIES_GPU.hsla(),
                    &gpu_load,
                    book.gpu_load.history(),
                    percent,
                ),
            ]));

        // Temperatures: three lines close together on one scale, where washes
        // would hide the one being followed.
        let cpu_trend = trend(book.cpu_temperature.history());
        let gpu_trend = trend(book.gpu_temperature.history());
        let liquid_trend = trend(book.liquid.history());
        let readings: Vec<f32> = [&cpu_trend, &gpu_trend, &liquid_trend]
            .into_iter()
            .flatten()
            .flatten()
            .copied()
            .collect();
        let temperature_scale = if readings.is_empty() {
            Scale::fixed(20.0, 80.0, 3)
        } else {
            Scale::nice(readings, 4, None)
        };
        let temperature_chart = SeriesChart::new(
            "monitoring-temperatures",
            SeriesStyle::Line,
            temperature_scale,
            window_s,
        )
        .series(plotted("CPU", color::SERIES_CPU, cpu_trend))
        .series(plotted("GPU", color::SERIES_GPU, gpu_trend))
        .series(plotted("Coolant", color::SERIES_COOLANT, liquid_trend))
        .format(|v| format!("{v:.0}\u{00b0}"), celsius);
        let temperatures = panel_surface()
            .child(hoverable(temperature_chart.render(), cx))
            .child(legend([
                legend_item(
                    "CPU package",
                    color::SERIES_CPU.hsla(),
                    &cpu_temperature,
                    book.cpu_temperature.history(),
                    celsius,
                ),
                legend_item(
                    "GPU core",
                    color::SERIES_GPU.hsla(),
                    &gpu_temperature,
                    book.gpu_temperature.history(),
                    celsius,
                ),
                legend_item(
                    "Coolant",
                    color::SERIES_COOLANT.hsla(),
                    &liquid,
                    book.liquid.history(),
                    celsius,
                ),
            ]));

        // Cooling response: a speed averaged per minute, compared minute by
        // minute between the two channels.
        let minutes = |history: &History| {
            chart::bucket(
                history,
                now,
                HISTORY_WINDOW_MS,
                MINUTE_COLUMNS,
                Holes::Average,
            )
        };
        let pump_minutes = minutes(book.pump.rpm.history());
        let fan_minutes = minutes(book.fan.rpm.history());
        let speeds: Vec<f32> = pump_minutes
            .iter()
            .chain(&fan_minutes)
            .flatten()
            .copied()
            .collect();
        let speed_scale = if speeds.is_empty() {
            Scale::fixed(0.0, 3_000.0, 3)
        } else {
            Scale::nice(speeds, 4, Some(0.0))
        };
        let cooling_chart = BarChart::new("monitoring-cooling", speed_scale, 60)
            .series(plotted("Pump", color::SERIES_PUMP, pump_minutes))
            .series(plotted("Fan", color::SERIES_FAN, fan_minutes))
            .format(rpm);
        let cooling = panel_surface()
            .child(hoverable(cooling_chart.render(), cx))
            .child(legend([
                legend_item(
                    "Pump",
                    color::SERIES_PUMP.hsla(),
                    &pump,
                    book.pump.rpm.history(),
                    rpm,
                ),
                legend_item(
                    "Fan",
                    color::SERIES_FAN.hsla(),
                    &fan,
                    book.fan.rpm.history(),
                    rpm,
                ),
            ]));

        // Headroom: every share of capacity at once, now against the peak of
        // the window, so the one closest to its limit stands out as a shape.
        let spokes = [
            (
                "CPU load",
                spoke(&cpu_load, book.cpu_load.history(), |v| v / 100.0, percent),
            ),
            (
                "CPU temp",
                spoke(
                    &cpu_temperature,
                    book.cpu_temperature.history(),
                    |v| v / CHIP_SPAN_C,
                    celsius,
                ),
            ),
            (
                "GPU load",
                spoke(&gpu_load, book.gpu_load.history(), |v| v / 100.0, percent),
            ),
            (
                "GPU temp",
                spoke(
                    &gpu_temperature,
                    book.gpu_temperature.history(),
                    |v| v / CHIP_SPAN_C,
                    celsius,
                ),
            ),
            (
                "Memory",
                spoke(
                    &memory_percent,
                    book.memory_percent.history(),
                    |v| v / 100.0,
                    percent,
                ),
            ),
            (
                "Coolant",
                spoke(&liquid, book.liquid.history(), liquid_share, celsius),
            ),
        ];
        let radar = RadarChart::new(
            "monitoring-headroom",
            spokes
                .iter()
                .map(|(label, ..)| RadarAxis {
                    label: SharedString::new_static(label),
                })
                .collect(),
        )
        .series(RadarSeries {
            name: "Now".into(),
            color: color::ACCENT.hsla(),
            wash: 0.15,
            stroke: px(2.0),
            markers: true,
            points: spokes.iter().map(|(_, (now, _))| now.clone()).collect(),
        })
        .series(RadarSeries {
            name: "Peak, 15 min".into(),
            color: color::TEXT_MUTED.hsla(),
            wash: 0.08,
            stroke: px(1.5),
            markers: false,
            points: spokes.iter().map(|(_, (_, peak))| peak.clone()).collect(),
        });
        let headroom = panel_surface().child(hoverable(radar.render(), cx)).child(
            div()
                .flex()
                .flex_wrap()
                .gap_x(space::XL)
                .gap_y(space::XS)
                .child(radar_key("Now", color::ACCENT.hsla()))
                .child(radar_key(
                    "Peak in the last 15 minutes",
                    color::TEXT_MUTED.hsla(),
                )),
        );

        let gpu_description = match book.gpu_name.view(now).value() {
            Some(name) => format!(
                "CPU and GPU utilization. GPU: {name}, read through {}.",
                self.gpu_source()
            ),
            None => "CPU and GPU utilization. No GPU management interface answered.".to_string(),
        };

        screen("Monitoring", "Load, temperatures and cooling over the last 15 minutes.")
            .children(self.device_strip())
            .children(self.link.failed_collectors().iter().map(|failure| {
                Note::new(
                    NoteLevel::Warning,
                    format!("{}: {}", failure.collector.label(), failure.detail),
                )
                .render()
            }))
            .child(block(eyebrow("Overview"), overview))
            .child(block(section_title("Load", Some(gpu_description.into())), load))
            .child(block(
                section_title(
                    "Temperatures",
                    Some("CPU package, GPU core and the coolant in the loop.".into()),
                ),
                temperatures,
            ))
            .child(block(
                section_title(
                    "Cooling response",
                    Some("Average pump and fan speed per minute.".into()),
                ),
                cooling,
            ))
            .child(block(
                section_title(
                    "Headroom",
                    Some(
                        "Each spoke is a share of its limit: 100% load, 100 \u{00b0}C on a chip, 60 \u{00b0}C on the coolant."
                            .into(),
                    ),
                ),
                headroom,
            ))
            .child(
                div()
                    .px(px(2.0))
                    .text_size(text::LABEL_SM)
                    .text_color(color::TEXT_MUTED.hsla())
                    .child(format!(
                        "Rolling {} minute window, held in memory only.",
                        HISTORY_WINDOW_MS / 60_000
                    )),
            )
    }

    /// The interface NVML or its absence is reported under.
    fn gpu_source(&self) -> String {
        self.link
            .telemetry()
            .map(|snapshot| snapshot.gpu.source.clone())
            .unwrap_or_else(|| "no interface".to_string())
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

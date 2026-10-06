// SPDX-FileCopyrightText: 2026 Arthur Jean
// SPDX-License-Identifier: GPL-3.0-or-later

//! A trend over time: shadcn's area and line charts.

use std::rc::Rc;

use gpui::{
    Bounds, Div, FontWeight, HitboxBehavior, PathBuilder, Pixels, SharedString, Stateful, Window,
    canvas, div, point, prelude::*, px,
};

use crate::theme::{color, numeric_font, text};

use super::{
    Plotted, Scale, TooltipRow, ago, hairline, line_height, marker, measure, paint_text,
    paint_tooltip, ui_font,
};

/// How the series of a chart are drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SeriesStyle {
    /// A line over a wash of its own color: for a quantity with a floor, where
    /// the area under the line is part of what it says, such as a load.
    Area,
    /// Lines alone: for series that sit close together on one scale, where
    /// overlapping washes would muddy the one the reader is following.
    Line,
}

/// Opacity of the wash under a line: the dataviz tenth.
const AREA_WASH: f32 = 0.10;
/// Height of the plot, axis bands included.
const CHART_HEIGHT: Pixels = px(184.0);
/// Height of the band under the plot the time labels sit in.
const X_AXIS_BAND: Pixels = px(22.0);
/// Gap between the tick labels and the plot.
const Y_AXIS_GAP: Pixels = px(8.0);

/// Several series over the same window, on one value axis.
///
/// One axis only: two quantities of different units never share a plot, they
/// get a chart each. The values are already bucketed by time, oldest first,
/// with the last bucket ending now.
pub struct SeriesChart {
    id: SharedString,
    style: SeriesStyle,
    series: Vec<Plotted>,
    scale: Scale,
    window_s: u64,
    tick: Rc<dyn Fn(f32) -> String>,
    value: Rc<dyn Fn(f32) -> String>,
}

impl SeriesChart {
    pub fn new(
        id: impl Into<SharedString>,
        style: SeriesStyle,
        scale: Scale,
        window_s: u64,
    ) -> Self {
        Self {
            id: id.into(),
            style,
            series: Vec::new(),
            scale,
            window_s,
            tick: Rc::new(super::grouped),
            value: Rc::new(super::grouped),
        }
    }

    pub fn series(mut self, series: Plotted) -> Self {
        self.series.push(series);
        self
    }

    /// How an axis tick and a tooltip value are written.
    pub fn format(
        mut self,
        tick: impl Fn(f32) -> String + 'static,
        value: impl Fn(f32) -> String + 'static,
    ) -> Self {
        self.tick = Rc::new(tick);
        self.value = Rc::new(value);
        self
    }

    pub fn render(self) -> Stateful<Div> {
        let chart = Rc::new(self);
        let id = chart.id.clone();
        div().id(id).w_full().h(CHART_HEIGHT).child(
            canvas(
                |bounds, window, _| window.insert_hitbox(bounds, HitboxBehavior::Normal),
                move |bounds, hitbox, window, cx| {
                    let pointer = hitbox.is_hovered(window).then(|| window.mouse_position());
                    chart.paint(bounds, pointer, window, cx);
                },
            )
            .size_full(),
        )
    }

    fn paint(
        &self,
        bounds: Bounds<Pixels>,
        pointer: Option<gpui::Point<Pixels>>,
        window: &mut Window,
        cx: &mut gpui::App,
    ) {
        let tick_font = numeric_font();
        let gutter = self
            .scale
            .ticks
            .iter()
            .map(|tick| {
                measure(
                    window,
                    &(self.tick)(*tick),
                    text::LABEL_SM,
                    tick_font.clone(),
                )
            })
            .fold(px(0.0), |widest, width| widest.max(width))
            + Y_AXIS_GAP;
        // Half a line of room above the top tick, so its label is not cut.
        let top = bounds.origin.y + line_height(text::LABEL_SM) / 2.0;
        let plot = Bounds::new(
            point(bounds.origin.x + gutter, top),
            gpui::size(
                bounds.size.width - gutter,
                bounds.size.height - X_AXIS_BAND - (top - bounds.origin.y),
            ),
        );
        if plot.size.width <= px(0.0) || plot.size.height <= px(0.0) {
            return;
        }
        let y_of =
            |value: f32| plot.origin.y + plot.size.height * (1.0 - self.scale.fraction(value));

        // Gridlines and their labels, solid hairlines one step off the card.
        for tick in &self.scale.ticks {
            let y = y_of(*tick);
            hairline(
                window,
                point(plot.origin.x, y),
                point(plot.origin.x + plot.size.width, y),
                color::GRID.hsla(),
            );
            let label = (self.tick)(*tick);
            let width = measure(window, &label, text::LABEL_SM, tick_font.clone());
            paint_text(
                window,
                cx,
                &label,
                point(
                    plot.origin.x - Y_AXIS_GAP - width,
                    y - line_height(text::LABEL_SM) / 2.0,
                ),
                text::LABEL_SM,
                color::TEXT_MUTED.hsla(),
                tick_font.clone(),
            );
        }

        let count = self
            .series
            .iter()
            .map(|series| series.values.len())
            .max()
            .unwrap_or(0);
        if count < 2 {
            return;
        }
        let x_of =
            |index: usize| plot.origin.x + plot.size.width * (index as f32 / (count - 1) as f32);
        let baseline = plot.origin.y + plot.size.height;

        // Every wash before any line, so no line is dimmed by a neighbor's fill.
        if self.style == SeriesStyle::Area {
            for series in &self.series {
                for (first, last) in runs(&series.values) {
                    if first == last {
                        continue;
                    }
                    let mut builder = PathBuilder::fill();
                    builder.move_to(point(x_of(first), baseline));
                    for index in first..=last {
                        if let Some(value) = series.values[index] {
                            builder.line_to(point(x_of(index), y_of(value)));
                        }
                    }
                    builder.line_to(point(x_of(last), baseline));
                    builder.close();
                    if let Ok(path) = builder.build() {
                        window.paint_path(path, series.color.opacity(AREA_WASH));
                    }
                }
            }
        }
        for series in &self.series {
            for (first, last) in runs(&series.values) {
                if first == last {
                    // A lone reading between two holes is still a reading.
                    if let Some(value) = series.values[first] {
                        marker(window, point(x_of(first), y_of(value)), series.color);
                    }
                    continue;
                }
                let mut builder = PathBuilder::stroke(px(2.0));
                for index in first..=last {
                    if let Some(value) = series.values[index] {
                        let at = point(x_of(index), y_of(value));
                        if index == first {
                            builder.move_to(at);
                        } else {
                            builder.line_to(at);
                        }
                    }
                }
                if let Ok(path) = builder.build() {
                    window.paint_path(path, series.color);
                }
            }
        }

        // The time axis: the window's start, two steps between, and now.
        let label_font = ui_font(FontWeight::NORMAL);
        let minutes = self.window_s / 60;
        for step in 0..=3u64 {
            let fraction = step as f32 / 3.0;
            let label = if step == 3 {
                "Now".to_string()
            } else {
                format!("\u{2212}{} min", minutes - minutes * step / 3)
            };
            let width = measure(window, &label, text::LABEL_SM, label_font.clone());
            let x = plot.origin.x + plot.size.width * fraction;
            let x = (x - width / 2.0)
                .max(plot.origin.x)
                .min(plot.origin.x + plot.size.width - width);
            paint_text(
                window,
                cx,
                &label,
                point(x, baseline + px(6.0)),
                text::LABEL_SM,
                color::TEXT_MUTED.hsla(),
                label_font.clone(),
            );
        }

        // The hover layer: the crosshair snaps to the nearest bucket, and one
        // tooltip reads every series there.
        let Some(pointer) = pointer.filter(|pointer| plot.contains(pointer)) else {
            return;
        };
        let across = (pointer.x - plot.origin.x) / plot.size.width;
        let index = (across * (count - 1) as f32).round() as usize;
        let x = x_of(index);
        hairline(
            window,
            point(x, plot.origin.y),
            point(x, baseline),
            color::TEXT.alpha(0.3),
        );
        let mut rows = Vec::with_capacity(self.series.len());
        for series in &self.series {
            let value = series.values.get(index).copied().flatten();
            if let Some(value) = value {
                marker(window, point(x, y_of(value)), series.color);
            }
            rows.push(TooltipRow {
                key: series.color,
                value: value.map_or_else(|| "No reading".to_string(), |value| (self.value)(value)),
                label: series.name.to_string(),
            });
        }
        let seconds_ago = self.window_s * (count - 1 - index) as u64 / (count - 1) as u64;
        paint_tooltip(
            window,
            cx,
            plot,
            point(x, pointer.y),
            &ago(seconds_ago),
            &rows,
        );
    }
}

/// Runs of consecutive readings, as inclusive index ranges.
///
/// Each run becomes its own path, which is what leaves every hole visible.
fn runs(values: &[Option<f32>]) -> Vec<(usize, usize)> {
    let mut runs = Vec::new();
    let mut start: Option<usize> = None;
    for (index, value) in values.iter().enumerate() {
        match (value, start) {
            (Some(_), None) => start = Some(index),
            (None, Some(first)) => {
                runs.push((first, index - 1));
                start = None;
            }
            _ => {}
        }
    }
    if let Some(first) = start {
        runs.push((first, values.len() - 1));
    }
    runs
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_line_breaks_at_every_hole() {
        assert_eq!(
            runs(&[Some(1.0), Some(2.0), None, Some(4.0), Some(5.0)]),
            vec![(0, 1), (3, 4)]
        );
        assert_eq!(runs(&[None, Some(1.0), None]), vec![(1, 1)]);
        assert!(runs(&[None, None]).is_empty());
        assert!(runs(&[]).is_empty());
    }
}

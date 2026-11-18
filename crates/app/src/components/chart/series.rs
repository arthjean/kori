// SPDX-FileCopyrightText: 2026 Arthur Jean
// SPDX-License-Identifier: GPL-3.0-or-later

//! A trend over time: shadcn's line chart.

use std::rc::Rc;

use gpui::{
    Bounds, Div, FontWeight, PathBuilder, Pixels, SharedString, Stateful, Window, canvas, div,
    point, prelude::*, px,
};

use crate::theme::{color, numeric_font, text};

use super::{Plotted, Scale, hairline, line_height, marker, measure, paint_text, ui_font};

/// Height of the plot, axis bands included.
const CHART_HEIGHT: Pixels = px(184.0);
/// Height of the band under the plot the time labels sit in.
const X_AXIS_BAND: Pixels = px(22.0);
/// Gap between the tick labels and the plot.
const Y_AXIS_GAP: Pixels = px(8.0);

/// Several series over the same window, on one value axis, as lines.
///
/// One axis only: two quantities of different units never share a plot, they
/// get a chart each. The values are already one per point, oldest first, with
/// the last one ending now. Lines alone, without washes: the series sit close
/// together on one scale, and overlapping washes would muddy the one the reader
/// is following.
///
/// The canvas takes no hitbox and reads no pointer, so moving the pointer over
/// a chart changes nothing it paints and asks for no repaint.
pub struct SeriesChart {
    id: SharedString,
    series: Vec<Plotted>,
    scale: Scale,
    window_s: u64,
    tick: Rc<dyn Fn(f32) -> String>,
    /// A sentence painted over an empty plot, when there is something to say
    /// about why it is empty.
    empty: Option<SharedString>,
}

impl SeriesChart {
    pub fn new(id: impl Into<SharedString>, scale: Scale, window_s: u64) -> Self {
        Self {
            id: id.into(),
            series: Vec::new(),
            scale,
            window_s,
            tick: Rc::new(super::grouped),
            empty: None,
        }
    }

    pub fn series(mut self, series: Plotted) -> Self {
        self.series.push(series);
        self
    }

    /// How an axis tick is written.
    pub fn format(mut self, tick: impl Fn(f32) -> String + 'static) -> Self {
        self.tick = Rc::new(tick);
        self
    }

    /// Say why the plot holds no line. The grid is still drawn under it, so
    /// the chart keeps its place and its scale while it waits.
    pub fn empty(mut self, message: impl Into<SharedString>) -> Self {
        self.empty = Some(message.into());
        self
    }

    pub fn render(self) -> Stateful<Div> {
        let chart = Rc::new(self);
        let id = chart.id.clone();
        let selector = id.to_string();
        div()
            .id(id)
            .debug_selector(move || selector)
            .w_full()
            .h(CHART_HEIGHT)
            .child(
                canvas(
                    |_, _, _| {},
                    move |bounds, _, window, cx| chart.paint(bounds, window, cx),
                )
                .size_full(),
            )
    }

    fn paint(&self, bounds: Bounds<Pixels>, window: &mut Window, cx: &mut gpui::App) {
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
        let baseline = plot.origin.y + plot.size.height;

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

        if let Some(message) = &self.empty {
            let width = measure(window, message, text::BODY, label_font.clone());
            paint_text(
                window,
                cx,
                message,
                point(
                    plot.origin.x + (plot.size.width - width).max(px(0.0)) / 2.0,
                    plot.origin.y + (plot.size.height - line_height(text::BODY)) / 2.0,
                ),
                text::BODY,
                color::TEXT_MUTED.hsla(),
                label_font,
            );
            return;
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

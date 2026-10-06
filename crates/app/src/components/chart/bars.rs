// SPDX-FileCopyrightText: 2026 Arthur Jean
// SPDX-License-Identifier: GPL-3.0-or-later

//! A value per period: shadcn's grouped bar chart, in columns.

use std::rc::Rc;

use gpui::{
    Bounds, Corners, Div, FontWeight, HitboxBehavior, Pixels, SharedString, Stateful, Window,
    canvas, div, point, prelude::*, px, size,
};

use crate::theme::{color, numeric_font, text};

use super::{
    Plotted, Scale, TooltipRow, hairline, line_height, measure, paint_text, paint_tooltip, ui_font,
};

const CHART_HEIGHT: Pixels = px(184.0);
const X_AXIS_BAND: Pixels = px(22.0);
const Y_AXIS_GAP: Pixels = px(8.0);
/// The widest a column grows: a thin mark with air around it, never a block.
const BAR_MAX: Pixels = px(24.0);
/// The surface gap between two columns of one group.
const BAR_GAP: Pixels = px(2.0);
/// The rounded data end. The baseline end stays square, where the column grows
/// from.
const BAR_RADIUS: Pixels = px(4.0);
/// Share of a group's band its columns may take; the rest is air.
const BAND_FILL: f32 = 0.7;

/// One column per series in each period, side by side, on one axis.
///
/// Periods run oldest first and the last one ends now. A period a series has
/// no reading for draws no column at all, rather than a column of zero.
pub struct BarChart {
    id: SharedString,
    series: Vec<Plotted>,
    scale: Scale,
    period_s: u64,
    value: Rc<dyn Fn(f32) -> String>,
}

impl BarChart {
    pub fn new(id: impl Into<SharedString>, scale: Scale, period_s: u64) -> Self {
        Self {
            id: id.into(),
            series: Vec::new(),
            scale,
            period_s,
            value: Rc::new(super::grouped),
        }
    }

    pub fn series(mut self, series: Plotted) -> Self {
        self.series.push(series);
        self
    }

    /// How a tooltip value is written.
    pub fn format(mut self, value: impl Fn(f32) -> String + 'static) -> Self {
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
                    &super::grouped(*tick),
                    text::LABEL_SM,
                    tick_font.clone(),
                )
            })
            .fold(px(0.0), |widest, width| widest.max(width))
            + Y_AXIS_GAP;
        let top = bounds.origin.y + line_height(text::LABEL_SM) / 2.0;
        let plot = Bounds::new(
            point(bounds.origin.x + gutter, top),
            size(
                bounds.size.width - gutter,
                bounds.size.height - X_AXIS_BAND - (top - bounds.origin.y),
            ),
        );
        let groups = self
            .series
            .iter()
            .map(|series| series.values.len())
            .max()
            .unwrap_or(0);
        if plot.size.width <= px(0.0) || plot.size.height <= px(0.0) || groups == 0 {
            return;
        }
        let baseline = plot.origin.y + plot.size.height;
        let y_of =
            |value: f32| plot.origin.y + plot.size.height * (1.0 - self.scale.fraction(value));
        let band = plot.size.width / groups as f32;

        // The group under the pointer lifts first, under its columns, the way
        // shadcn's cursor does: the whole period is the target, not a 6-pixel
        // column.
        let hovered = pointer
            .filter(|pointer| plot.contains(pointer))
            .map(|pointer| (((pointer.x - plot.origin.x) / band) as usize).min(groups - 1));
        if let Some(group) = hovered {
            let lit = Bounds::new(
                point(plot.origin.x + band * group as f32, plot.origin.y),
                size(band, plot.size.height),
            );
            window.paint_quad(
                gpui::fill(lit, color::TEXT.alpha(color::WASH_HOVER)).corner_radii(px(6.0)),
            );
        }

        for tick in &self.scale.ticks {
            let y = y_of(*tick);
            hairline(
                window,
                point(plot.origin.x, y),
                point(plot.origin.x + plot.size.width, y),
                color::GRID.hsla(),
            );
            let label = super::grouped(*tick);
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

        let columns = self.series.len().max(1) as f32;
        let bar = ((band * BAND_FILL - BAR_GAP * (columns - 1.0)) / columns)
            .min(BAR_MAX)
            .max(px(1.0));
        let group_width = bar * columns + BAR_GAP * (columns - 1.0);
        for group in 0..groups {
            let left = plot.origin.x + band * group as f32 + (band - group_width) / 2.0;
            for (offset, series) in self.series.iter().enumerate() {
                let Some(value) = series.values.get(group).copied().flatten() else {
                    continue;
                };
                let y = y_of(value);
                let height = baseline - y;
                if height <= px(0.0) {
                    continue;
                }
                let x = left + (bar + BAR_GAP) * offset as f32;
                let radius = BAR_RADIUS.min(bar / 2.0).min(height);
                window.paint_quad(
                    gpui::fill(Bounds::new(point(x, y), size(bar, height)), series.color)
                        .corner_radii(Corners {
                            top_left: radius,
                            top_right: radius,
                            bottom_right: px(0.0),
                            bottom_left: px(0.0),
                        }),
                );
            }
        }

        // A label every five periods, and on the last one, which is now.
        let label_font = ui_font(FontWeight::NORMAL);
        for group in (0..groups).filter(|group| (groups - 1 - group) % 5 == 0) {
            let periods_ago = (groups - 1 - group) as u64;
            let label = if periods_ago == 0 {
                "Now".to_string()
            } else {
                format!("\u{2212}{} min", periods_ago * self.period_s / 60)
            };
            let width = measure(window, &label, text::LABEL_SM, label_font.clone());
            let center = plot.origin.x + band * (group as f32 + 0.5);
            let x = (center - width / 2.0)
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

        let Some(group) = hovered else {
            return;
        };
        let rows: Vec<TooltipRow> = self
            .series
            .iter()
            .map(|series| TooltipRow {
                key: series.color,
                value: series
                    .values
                    .get(group)
                    .copied()
                    .flatten()
                    .map_or_else(|| "No reading".to_string(), |value| (self.value)(value)),
                label: series.name.to_string(),
            })
            .collect();
        let periods_ago = (groups - 1 - group) as u64;
        let title = match periods_ago {
            0 => "This minute".to_string(),
            1 => "1 min ago".to_string(),
            ago => format!("{} min ago", ago * self.period_s / 60),
        };
        let anchor_x = plot.origin.x + band * (group as f32 + 0.5) + group_width / 2.0;
        let anchor_y = pointer.map_or(plot.origin.y, |pointer| pointer.y);
        paint_tooltip(window, cx, plot, point(anchor_x, anchor_y), &title, &rows);
    }
}

// SPDX-FileCopyrightText: 2026 Arthur Jean
// SPDX-License-Identifier: GPL-3.0-or-later

//! Several shares of capacity read as one shape: shadcn's radar chart.

use std::f32::consts::PI;
use std::rc::Rc;

use gpui::{
    Bounds, Div, FontWeight, HitboxBehavior, Hsla, PathBuilder, Pixels, Point, SharedString,
    Stateful, Window, canvas, div, point, prelude::*, px,
};

use crate::theme::{color, numeric_font, text};

use super::{
    TooltipRow, hairline, line_height, marker, measure, paint_text, paint_tooltip, ui_font,
};

const CHART_HEIGHT: Pixels = px(260.0);
/// Room kept around the web for the axis labels.
const LABEL_BAND_X: Pixels = px(96.0);
const LABEL_BAND_Y: Pixels = px(40.0);
/// Gap between the outer ring and an axis label.
const LABEL_GAP: Pixels = px(10.0);
/// Rings at a quarter, a half, three quarters and the whole.
const RINGS: [f32; 4] = [0.25, 0.5, 0.75, 1.0];

/// One spoke of the web.
#[derive(Debug, Clone)]
pub struct RadarAxis {
    pub label: SharedString,
}

/// One shape on the web: a share of capacity per axis, with the value the
/// tooltip and the axis label write for it.
#[derive(Debug, Clone)]
pub struct RadarSeries {
    pub name: SharedString,
    pub color: Hsla,
    /// Opacity of the fill inside the shape.
    pub wash: f32,
    /// Line width of its outline.
    pub stroke: Pixels,
    /// Whether its vertices carry markers: the series the reader is meant to
    /// follow, never the reference behind it.
    pub markers: bool,
    /// Per axis, the share of capacity (0.0 to 1.0) and its display value.
    pub points: Vec<Option<(f32, String)>>,
}

/// Several series on shared spokes, each spoke a share of its own limit.
///
/// The first series is the one the axis labels quote. A series missing a
/// reading on any axis draws no shape, only the vertices it has: closing the
/// outline through a hole would invent the value on that spoke.
pub struct RadarChart {
    id: SharedString,
    axes: Vec<RadarAxis>,
    series: Vec<RadarSeries>,
}

impl RadarChart {
    pub fn new(id: impl Into<SharedString>, axes: Vec<RadarAxis>) -> Self {
        Self {
            id: id.into(),
            axes,
            series: Vec::new(),
        }
    }

    pub fn series(mut self, series: RadarSeries) -> Self {
        self.series.push(series);
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
        pointer: Option<Point<Pixels>>,
        window: &mut Window,
        cx: &mut gpui::App,
    ) {
        let count = self.axes.len();
        let center = point(
            bounds.origin.x + bounds.size.width / 2.0,
            bounds.origin.y + bounds.size.height / 2.0,
        );
        let radius =
            (bounds.size.width / 2.0 - LABEL_BAND_X).min(bounds.size.height / 2.0 - LABEL_BAND_Y);
        if count < 3 || radius <= px(0.0) {
            return;
        }
        let at = |axis: usize, share: f32| {
            spoke_point(center, radius * share.clamp(0.0, 1.0), axis, count)
        };

        // The web: rings as polygons, so a ring and the outline of a shape at
        // the same share coincide, and the spokes from the center.
        let grid = color::GRID.hsla();
        for ring in RINGS {
            for axis in 0..count {
                hairline(window, at(axis, ring), at((axis + 1) % count, ring), grid);
            }
        }
        for axis in 0..count {
            hairline(window, center, at(axis, 1.0), grid);
        }

        // Reference shapes first, so the series being followed is on top.
        for series in self.series.iter().rev() {
            let vertices: Vec<Option<Point<Pixels>>> = (0..count)
                .map(|axis| {
                    series
                        .points
                        .get(axis)
                        .and_then(|point| point.as_ref())
                        .map(|(share, _)| at(axis, *share))
                })
                .collect();
            if vertices.iter().all(Option::is_some) {
                let vertices: Vec<Point<Pixels>> = vertices.iter().flatten().copied().collect();
                let mut fill = PathBuilder::fill();
                let mut outline = PathBuilder::stroke(series.stroke);
                for (index, vertex) in vertices.iter().enumerate() {
                    if index == 0 {
                        fill.move_to(*vertex);
                        outline.move_to(*vertex);
                    } else {
                        fill.line_to(*vertex);
                        outline.line_to(*vertex);
                    }
                }
                fill.close();
                outline.line_to(vertices[0]);
                if let Ok(path) = fill.build() {
                    window.paint_path(path, series.color.opacity(series.wash));
                }
                if let Ok(path) = outline.build() {
                    window.paint_path(path, series.color);
                }
            }
            if series.markers || vertices.iter().any(Option::is_none) {
                for vertex in vertices.iter().flatten() {
                    marker(window, *vertex, series.color);
                }
            }
        }

        // Axis labels outside the web, each quoting the first series: the name
        // muted, the value under it in the text color.
        let label_font = ui_font(FontWeight::NORMAL);
        let value_font = numeric_font();
        let quoted = self.series.first();
        for (axis, spec) in self.axes.iter().enumerate() {
            let value = quoted
                .and_then(|series| series.points.get(axis))
                .and_then(|point| point.as_ref())
                .map_or("No reading", |(_, display)| display.as_str());
            let label_width = measure(window, &spec.label, text::LABEL_SM, label_font.clone());
            let value_width = measure(window, value, text::BODY, value_font.clone());
            let block_height = line_height(text::LABEL_SM) + line_height(text::BODY);
            let anchor = spoke_point(center, radius + LABEL_GAP, axis, count);
            let (dx, dy) = direction(axis, count);
            let place = |width: Pixels| {
                // Left of a spoke pointing left, right of one pointing right,
                // centered on one pointing straight up or down.
                if dx > 0.2 {
                    anchor.x
                } else if dx < -0.2 {
                    anchor.x - width
                } else {
                    anchor.x - width / 2.0
                }
            };
            let top = if dy < -0.5 {
                anchor.y - block_height
            } else if dy > 0.5 {
                anchor.y
            } else {
                anchor.y - block_height / 2.0
            };
            paint_text(
                window,
                cx,
                &spec.label,
                point(place(label_width), top),
                text::LABEL_SM,
                color::TEXT_MUTED.hsla(),
                label_font.clone(),
            );
            paint_text(
                window,
                cx,
                value,
                point(place(value_width), top + line_height(text::LABEL_SM)),
                text::BODY,
                color::TEXT.hsla(),
                value_font.clone(),
            );
        }

        // The hover layer: the spoke nearest the pointer by angle lights up,
        // and the tooltip reads every series on it.
        let Some(pointer) = pointer else {
            return;
        };
        let (px_, py_) = (
            f32::from(pointer.x - center.x),
            f32::from(pointer.y - center.y),
        );
        let reach = f32::from(radius + LABEL_BAND_X);
        if px_.hypot(py_) > reach {
            return;
        }
        let axis = nearest_axis(px_, py_, count);
        hairline(window, center, at(axis, 1.0), color::TEXT.alpha(0.3));
        let rows: Vec<TooltipRow> = self
            .series
            .iter()
            .map(|series| TooltipRow {
                key: series.color,
                value: series
                    .points
                    .get(axis)
                    .and_then(|point| point.as_ref())
                    .map_or_else(|| "No reading".to_string(), |(_, display)| display.clone()),
                label: series.name.to_string(),
            })
            .collect();
        paint_tooltip(window, cx, bounds, pointer, &self.axes[axis].label, &rows);
    }
}

/// Unit direction of a spoke on screen, the first one pointing straight up
/// and the rest following clockwise.
fn direction(axis: usize, count: usize) -> (f32, f32) {
    let angle = -PI / 2.0 + 2.0 * PI * axis as f32 / count.max(1) as f32;
    (angle.cos(), angle.sin())
}

fn spoke_point(
    center: Point<Pixels>,
    distance: Pixels,
    axis: usize,
    count: usize,
) -> Point<Pixels> {
    let (dx, dy) = direction(axis, count);
    point(center.x + distance * dx, center.y + distance * dy)
}

/// The spoke closest in angle to an offset from the center.
fn nearest_axis(dx: f32, dy: f32, count: usize) -> usize {
    let count = count.max(1);
    // Angle clockwise from straight up, in turns.
    let turns = (dx.atan2(-dy) / (2.0 * PI)).rem_euclid(1.0);
    ((turns * count as f32).round() as usize) % count
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_first_spoke_points_up_and_the_rest_follow_clockwise() {
        let (dx, dy) = direction(0, 6);
        assert!(dx.abs() < 1e-6 && (dy + 1.0).abs() < 1e-6);
        // The second of six sits up and to the right on screen.
        let (dx, dy) = direction(1, 6);
        assert!(dx > 0.0 && dy < 0.0);
        let (dx, dy) = direction(3, 6);
        assert!(dx.abs() < 1e-6 && (dy - 1.0).abs() < 1e-6);
    }

    #[test]
    fn the_pointer_selects_the_spoke_nearest_in_angle() {
        assert_eq!(nearest_axis(0.0, -10.0, 6), 0);
        assert_eq!(nearest_axis(0.0, 10.0, 6), 3);
        // Just left of straight up wraps to the first spoke, not the last.
        assert_eq!(nearest_axis(-0.5, -10.0, 6), 0);
        let (dx, dy) = direction(4, 6);
        assert_eq!(nearest_axis(dx * 50.0, dy * 50.0, 6), 4);
    }
}

// SPDX-FileCopyrightText: 2026 Arthur Jean
// SPDX-License-Identifier: GPL-3.0-or-later

//! The charts the Monitoring screen is made of.
//!
//! Four forms, each picked by what its data has to say rather than by variety:
//! a radial gauge for one value against its limit, a series chart for a trend
//! over time, a column chart for a value per minute, and a radar for several
//! shares of capacity read as one shape. The vocabulary is shadcn's charts, the
//! marks are the dataviz specs: 2-pixel lines, washes at about a tenth, solid
//! hairline grids, columns at most 24 wide with a 4-pixel rounded end.
//!
//! Every chart paints into one canvas, axis labels included, so a label and the
//! gridline it names are placed by the same arithmetic. The hover layer is
//! painted there too: the canvas records its hitbox, reads the pointer while
//! painting, and draws the crosshair and the tooltip on top of the marks. The
//! screen only has to repaint when the pointer moves over a chart.
//!
//! A gap is drawn as a gap throughout. Joining across a hole would invent a
//! value and flattening it to zero would invent a plunge, and this product
//! refuses both.

use gpui::{
    App, Bounds, Font, FontFeatures, FontStyle, FontWeight, Hsla, Pixels, Point, SharedString,
    TextRun, Window, point, px, size,
};

use kori_core::telemetry::History;

use crate::theme::{UI_FONT, color, numeric_font, text};

use super::squircle;

mod bars;
mod radar;
mod radial;
mod series;

pub use bars::BarChart;
pub use radar::{RadarAxis, RadarChart, RadarSeries};
pub use radial::RadialGauge;
pub use series::{SeriesChart, SeriesStyle};

/// One named, colored series, already reduced to the points a chart plots.
#[derive(Debug, Clone)]
pub struct Plotted {
    pub name: SharedString,
    pub color: Hsla,
    pub values: Vec<Option<f32>>,
}

/// What a whole history says in three numbers: where it is, where it sat on
/// average, and the highest it went.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Summary {
    pub latest: f32,
    pub average: f32,
    pub peak: f32,
}

/// Summarize the readings a history holds, skipping its holes.
///
/// `None` when it holds no reading at all, so a legend says nothing rather
/// than a zero nobody measured.
pub fn summarize(history: &History) -> Option<Summary> {
    let mut latest = None;
    let mut sum = 0.0;
    let mut count = 0usize;
    let mut peak = f32::MIN;
    for value in history.points().filter_map(|point| point.value) {
        latest = Some(value);
        sum += value;
        count += 1;
        peak = peak.max(value);
    }
    latest.map(|latest| Summary {
        latest,
        average: sum / count as f32,
        peak,
    })
}

/// Whether a bucket that lost one of its samples is drawn at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Holes {
    /// One missing sample makes the whole bucket a hole. Right for a line,
    /// where a hole is the evidence and must never be averaged away.
    Keep,
    /// The bucket averages what it has. Right for a column standing for a
    /// minute, which is still that minute with one second missing.
    Average,
}

/// Reduce a history to `count` buckets over the last `window_ms` before `now`.
///
/// Placed by time rather than by index, so the right edge is always now and a
/// session that started two minutes ago fills only the last two minutes of the
/// axis instead of stretching them over all fifteen. A bucket nothing was
/// recorded in is `None`, like a hole: the line does not cross it either.
pub fn bucket(
    history: &History,
    now_unix_ms: u64,
    window_ms: u64,
    count: usize,
    holes: Holes,
) -> Vec<Option<f32>> {
    if count == 0 || window_ms == 0 {
        return Vec::new();
    }
    let start = now_unix_ms.saturating_sub(window_ms);
    let width = window_ms as f64 / count as f64;
    let mut sums = vec![0.0f32; count];
    let mut seen = vec![0u32; count];
    let mut lost = vec![false; count];
    for point in history.points() {
        if point.at_unix_ms < start || point.at_unix_ms > now_unix_ms {
            continue;
        }
        let index = (((point.at_unix_ms - start) as f64 / width) as usize).min(count - 1);
        match point.value {
            Some(value) => {
                sums[index] += value;
                seen[index] += 1;
            }
            None => lost[index] = true,
        }
    }
    (0..count)
        .map(|index| {
            if seen[index] == 0 || (holes == Holes::Keep && lost[index]) {
                None
            } else {
                Some(sums[index] / seen[index] as f32)
            }
        })
        .collect()
}

/// A value axis: where it starts, where it ends, and the gridlines between.
#[derive(Debug, Clone, PartialEq)]
pub struct Scale {
    pub min: f32,
    pub max: f32,
    pub ticks: Vec<f32>,
}

impl Scale {
    /// A fixed axis, for a quantity with a known range such as a percentage.
    pub fn fixed(min: f32, max: f32, steps: usize) -> Self {
        let steps = steps.max(1);
        let ticks = (0..=steps)
            .map(|index| min + (max - min) * index as f32 / steps as f32)
            .collect();
        Self { min, max, ticks }
    }

    /// The smallest round axis that holds every value, with about `target`
    /// intervals.
    ///
    /// Round means a step of 1, 2, 2.5 or 5 times a power of ten, so a tick
    /// reads 0, 1,000, 2,000 rather than 0, 937, 1,874. `floor` keeps the axis
    /// from starting above a value that is meaningful at zero, such as a speed.
    pub fn nice(values: impl IntoIterator<Item = f32>, target: usize, floor: Option<f32>) -> Self {
        let mut low = f32::INFINITY;
        let mut high = f32::NEG_INFINITY;
        for value in values {
            low = low.min(value);
            high = high.max(value);
        }
        if let Some(floor) = floor {
            low = low.min(floor);
        }
        if !low.is_finite() || !high.is_finite() {
            return Self::fixed(0.0, 1.0, 1);
        }
        if (high - low).abs() < f32::EPSILON {
            high = low + 1.0;
        }
        let step = nice_step((high - low) / target.max(1) as f32);
        let min = (low / step).floor() * step;
        let max = (high / step).ceil() * step;
        let steps = ((max - min) / step).round().max(1.0) as usize;
        Self::fixed(min, max, steps)
    }

    /// Where a value sits on the axis, from 0.0 at the bottom to 1.0 at the top.
    pub fn fraction(&self, value: f32) -> f32 {
        if self.max <= self.min {
            return 0.0;
        }
        ((value - self.min) / (self.max - self.min)).clamp(0.0, 1.0)
    }
}

fn nice_step(raw: f32) -> f32 {
    if raw <= 0.0 || !raw.is_finite() {
        return 1.0;
    }
    let magnitude = 10f32.powf(raw.log10().floor());
    let normalized = raw / magnitude;
    let nice = if normalized <= 1.0 {
        1.0
    } else if normalized <= 2.0 {
        2.0
    } else if normalized <= 2.5 {
        2.5
    } else if normalized <= 5.0 {
        5.0
    } else {
        10.0
    };
    nice * magnitude
}

/// A number with thousands grouped, as an axis tick reads it.
pub fn grouped(value: f32) -> String {
    let rounded = value.round() as i64;
    let digits = rounded.unsigned_abs().to_string();
    let mut out = String::new();
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            out.push(',');
        }
        out.push(digit);
    }
    if rounded < 0 {
        out.insert(0, '-');
    }
    out
}

/// How long ago a bucket ended, as a tooltip title.
pub fn ago(seconds: u64) -> String {
    match seconds {
        0..=4 => "Now".to_string(),
        5..=59 => format!("{seconds} s ago"),
        _ if seconds.is_multiple_of(60) => format!("{} min ago", seconds / 60),
        _ => format!("{} min {} s ago", seconds / 60, seconds % 60),
    }
}

/// The interface face at one weight, for text painted into a canvas.
pub(crate) fn ui_font(weight: FontWeight) -> Font {
    Font {
        family: UI_FONT.into(),
        features: FontFeatures::default(),
        fallbacks: None,
        weight,
        style: FontStyle::Normal,
    }
}

fn shape(window: &Window, text: &str, size: Pixels, color: Hsla, font: Font) -> gpui::ShapedLine {
    let run = TextRun {
        len: text.len(),
        font,
        color,
        background_color: None,
        underline: None,
        strikethrough: None,
    };
    window
        .text_system()
        .shape_line(SharedString::from(text.to_string()), size, &[run], None)
}

/// Width a run of text takes, without painting it.
pub(crate) fn measure(window: &Window, text: &str, size: Pixels, font: Font) -> Pixels {
    shape(window, text, size, color::TEXT.hsla(), font).width
}

/// Paint one line of text with its top-left corner at `origin`.
pub(crate) fn paint_text(
    window: &mut Window,
    cx: &mut App,
    text: &str,
    origin: Point<Pixels>,
    size: Pixels,
    color: Hsla,
    font: Font,
) -> Pixels {
    let line = shape(window, text, size, color, font);
    let width = line.width;
    let _ = line.paint(origin, line_height(size), window, cx);
    width
}

/// Line box of painted text: the face's own proportion, rounded to a pixel.
pub(crate) fn line_height(size: Pixels) -> Pixels {
    (size * 1.4).round()
}

/// A straight hairline.
pub(crate) fn hairline(window: &mut Window, from: Point<Pixels>, to: Point<Pixels>, color: Hsla) {
    super::stroke_line(window, from, to, px(1.0), color);
}

/// A marker: a filled dot inside a ring of the card color, so it stays
/// legible where it sits on a line or on another marker.
pub(crate) fn marker(window: &mut Window, center: Point<Pixels>, fill: Hsla) {
    let disc = |radius: Pixels, color: Hsla, window: &mut Window| {
        let bounds = Bounds::new(
            point(center.x - radius, center.y - radius),
            size(radius * 2.0, radius * 2.0),
        );
        window.paint_quad(gpui::fill(bounds, color).corner_radii(radius));
    };
    disc(px(6.0), color::PANEL.hsla(), window);
    disc(px(4.0), fill, window);
}

/// One line of a tooltip: the series key, its value, and what it is.
pub(crate) struct TooltipRow {
    pub key: Hsla,
    pub value: String,
    pub label: String,
}

const TOOLTIP_PADDING_X: Pixels = px(10.0);
const TOOLTIP_PADDING_Y: Pixels = px(8.0);
const TOOLTIP_ROW: Pixels = px(18.0);
const TOOLTIP_KEY: Pixels = px(12.0);
const TOOLTIP_GAP: Pixels = px(8.0);
const TOOLTIP_RADIUS: Pixels = px(14.0);
/// Distance between the crosshair and the tooltip beside it.
const TOOLTIP_OFFSET: Pixels = px(12.0);

/// Paneflow's tooltip, painted beside `anchor` and kept inside `area`.
///
/// The title says where on the axis the pointer is, and each row leads with
/// the value in the full text color, the series name muted after it: the
/// reader already knows which series, and wants the number. Each row is keyed
/// by a short stroke of the series color, never a filled box.
pub(crate) fn paint_tooltip(
    window: &mut Window,
    cx: &mut App,
    area: Bounds<Pixels>,
    anchor: Point<Pixels>,
    title: &str,
    rows: &[TooltipRow],
) {
    let title_font = ui_font(FontWeight::NORMAL);
    let value_font = numeric_font();
    let label_font = ui_font(FontWeight::NORMAL);

    let mut width = measure(window, title, text::LABEL_SM, title_font.clone());
    for row in rows {
        let row_width = TOOLTIP_KEY
            + TOOLTIP_GAP
            + measure(window, &row.value, text::BODY, value_font.clone())
            + px(6.0)
            + measure(window, &row.label, text::LABEL_SM, label_font.clone());
        width = width.max(row_width);
    }
    let width = width + TOOLTIP_PADDING_X * 2.0;
    let height = TOOLTIP_PADDING_Y * 2.0 + TOOLTIP_ROW * (rows.len() as f32 + 1.0);

    // Right of the pointer, or left of it once the right side would overflow,
    // and never above or below the plot it describes.
    let mut x = anchor.x + TOOLTIP_OFFSET;
    if x + width > area.origin.x + area.size.width {
        x = anchor.x - TOOLTIP_OFFSET - width;
    }
    let x = x.max(area.origin.x);
    let y = (anchor.y - height / 2.0)
        .max(area.origin.y)
        .min((area.origin.y + area.size.height - height).max(area.origin.y));
    let bounds = Bounds::new(point(x, y), size(width, height));

    if let Some(path) = squircle::fill_path(bounds, TOOLTIP_RADIUS) {
        window.paint_path(path, color::RAIL.hsla());
    }
    if let Some(path) = squircle::stroke_path(bounds, TOOLTIP_RADIUS, px(1.0)) {
        window.paint_path(path, color::CONTROL_HOVER.hsla());
    }

    let left = x + TOOLTIP_PADDING_X;
    let mut top = y + TOOLTIP_PADDING_Y;
    paint_text(
        window,
        cx,
        title,
        point(left, top + px(2.0)),
        text::LABEL_SM,
        color::TEXT_MUTED.hsla(),
        title_font,
    );
    for row in rows {
        top += TOOLTIP_ROW;
        let middle = top + TOOLTIP_ROW / 2.0;
        window.paint_quad(
            gpui::fill(
                Bounds::new(point(left, middle - px(1.0)), size(TOOLTIP_KEY, px(2.0))),
                row.key,
            )
            .corner_radii(px(1.0)),
        );
        let value_x = left + TOOLTIP_KEY + TOOLTIP_GAP;
        let value_width = paint_text(
            window,
            cx,
            &row.value,
            point(value_x, middle - line_height(text::BODY) / 2.0),
            text::BODY,
            color::TEXT.hsla(),
            value_font.clone(),
        );
        paint_text(
            window,
            cx,
            &row.label,
            point(
                value_x + value_width + px(6.0),
                middle - line_height(text::LABEL_SM) / 2.0,
            ),
            text::LABEL_SM,
            color::TEXT_MUTED.hsla(),
            label_font.clone(),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn history(values: &[Option<f32>]) -> History {
        let mut history = History::new(kori_core::telemetry::HISTORY_WINDOW_MS);
        for (step, value) in values.iter().enumerate() {
            history.push(1_000_000 + step as u64 * 1_000, *value);
        }
        history
    }

    #[test]
    fn a_bucket_with_a_hole_stays_a_hole_on_a_line_and_averages_on_a_column() {
        let history = history(&[Some(2.0), None, Some(4.0), Some(6.0)]);
        let now = 1_000_000 + 3_000;
        // Two buckets over the last four seconds: the first holds the first
        // sample, the second the hole and the two after it.
        let line = bucket(&history, now, 4_000, 2, Holes::Keep);
        let column = bucket(&history, now, 4_000, 2, Holes::Average);
        assert_eq!(line[0], Some(2.0));
        assert_eq!(line[1], None, "the lost sample must survive as a hole");
        assert_eq!(column[1], Some(5.0));
    }

    #[test]
    fn a_short_session_fills_only_the_end_of_the_axis() {
        let history = history(&[Some(1.0); 10]);
        let now = 1_000_000 + 9_000;
        let buckets = bucket(&history, now, 60_000, 60, Holes::Keep);
        assert_eq!(buckets.len(), 60);
        let filled = buckets.iter().filter(|value| value.is_some()).count();
        assert!((9..=11).contains(&filled), "{filled} buckets filled");
        assert!(
            buckets[0].is_none(),
            "the start of the window was never read"
        );
        assert!(buckets[59].is_some(), "the right edge is now");
    }

    #[test]
    fn a_summary_skips_holes_and_says_nothing_about_an_empty_history() {
        let summary = summarize(&history(&[Some(2.0), None, Some(8.0), Some(5.0)]));
        assert_eq!(
            summary,
            Some(Summary {
                latest: 5.0,
                average: 5.0,
                peak: 8.0
            })
        );
        assert_eq!(summarize(&history(&[None, None])), None);
    }

    #[test]
    fn a_nice_scale_holds_every_value_on_round_ticks() {
        let scale = Scale::nice([937.0, 2_840.0], 4, Some(0.0));
        assert_eq!(scale.min, 0.0);
        assert_eq!(scale.max, 3_000.0);
        assert!(scale.ticks.contains(&1_000.0));
        let temperatures = Scale::nice([31.2, 58.4], 4, None);
        assert!(temperatures.min <= 31.2 && temperatures.max >= 58.4);
        for tick in &temperatures.ticks {
            assert_eq!(tick % 5.0, 0.0, "{tick} is not a round tick");
        }
        // A flat series still gets an axis rather than a division by zero.
        let flat = Scale::nice([40.0, 40.0], 4, None);
        assert!(flat.max > flat.min);
        assert_eq!(Scale::nice([], 4, None), Scale::fixed(0.0, 1.0, 1));
    }

    #[test]
    fn a_fraction_stays_on_the_axis() {
        let scale = Scale::fixed(0.0, 100.0, 4);
        assert_eq!(scale.ticks, vec![0.0, 25.0, 50.0, 75.0, 100.0]);
        assert_eq!(scale.fraction(50.0), 0.5);
        assert_eq!(scale.fraction(-10.0), 0.0);
        assert_eq!(scale.fraction(140.0), 1.0);
    }

    #[test]
    fn ticks_group_thousands_and_titles_read_as_elapsed_time() {
        assert_eq!(grouped(2_000.0), "2,000");
        assert_eq!(grouped(950.0), "950");
        assert_eq!(grouped(1_234_567.0), "1,234,567");
        assert_eq!(ago(2), "Now");
        assert_eq!(ago(35), "35 s ago");
        assert_eq!(ago(120), "2 min ago");
        assert_eq!(ago(135), "2 min 15 s ago");
    }
}

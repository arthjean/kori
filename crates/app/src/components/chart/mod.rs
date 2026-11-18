// SPDX-FileCopyrightText: 2026 Arthur Jean
// SPDX-License-Identifier: GPL-3.0-or-later

//! The charts the Monitoring screen is made of.
//!
//! Two forms, each picked by what its data has to say: a radial gauge for one
//! value against its limit, and a line chart for a trend over time. The marks
//! are the dataviz specs: 2-pixel lines and solid hairline grids.
//!
//! Every chart paints into one canvas, axis labels included, so a label and the
//! gridline it names are placed by the same arithmetic. No chart has a hover
//! layer: every value one would show is written in the legend beside it, and a
//! layer that followed the pointer cost a repaint of the whole window on every
//! pointer move for a number the screen already states.
//!
//! A gap is drawn as a gap throughout. Joining across a hole would invent a
//! value and flattening it to zero would invent a plunge, and this product
//! refuses both.

use gpui::{
    App, Bounds, Font, FontFeatures, FontStyle, FontWeight, Hsla, Pixels, Point, SharedString,
    TextRun, Window, point, px, size,
};

use crate::theme::{UI_FONT, color};

mod radial;
mod series;

pub use radial::RadialGauge;
pub use series::SeriesChart;

/// One named, colored series, already reduced to the points a chart plots.
#[derive(Debug, Clone)]
pub struct Plotted {
    pub name: SharedString,
    pub color: Hsla,
    pub values: Vec<Option<f32>>,
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

#[cfg(test)]
mod tests {
    use super::*;

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
    fn ticks_group_thousands() {
        assert_eq!(grouped(2_000.0), "2,000");
        assert_eq!(grouped(950.0), "950");
        assert_eq!(grouped(1_234_567.0), "1,234,567");
    }
}

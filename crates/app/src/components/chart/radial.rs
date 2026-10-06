// SPDX-FileCopyrightText: 2026 Arthur Jean
// SPDX-License-Identifier: GPL-3.0-or-later

//! One value against its limit: shadcn's radial chart with text, as a meter.

use std::f32::consts::PI;

use gpui::{
    Bounds, Div, Hsla, PathBuilder, Pixels, Point, Rgba, SharedString, Window, canvas, div, point,
    prelude::*, px, size,
};

use crate::theme::{color, text};

/// Side of the square a gauge is drawn in.
const GAUGE_SIDE: Pixels = px(112.0);
/// Thickness of the ring.
const RING: Pixels = px(10.0);
/// Where the arc starts, clockwise from three o'clock: the lower left.
const START: f32 = 0.75 * PI;
/// How far it sweeps: three quarters of a turn, open at the bottom so the eye
/// reads it as a dial with a beginning and an end rather than as a ring.
const SWEEP: f32 = 1.5 * PI;

/// A meter: how far a value has gone toward its limit, with the value written
/// in the middle.
///
/// The track is the same arc in the subtle fill, so the share that is left
/// reads as much as the share that is used. At rest the fill shades from the
/// foot of the sweep to the head of what it reached, as the panel's ring does;
/// a severity replaces it with the one color that state calls for, and the
/// caption under the value says the same thing in a word, so the state is
/// never carried by color alone.
pub struct RadialGauge {
    fraction: Option<f32>,
    /// The fill's color at the start of the sweep and at the end of what it
    /// reached. The same color twice is a solid fill.
    fill: (Hsla, Hsla),
    value: SharedString,
    caption: SharedString,
    value_color: Hsla,
}

impl RadialGauge {
    /// `fraction` is the share of the limit reached, `None` when there is no
    /// reading, which leaves the track empty rather than at zero.
    pub fn new(
        fraction: Option<f32>,
        value: impl Into<SharedString>,
        caption: impl Into<SharedString>,
    ) -> Self {
        Self {
            fraction: fraction.map(|fraction| fraction.clamp(0.0, 1.0)),
            fill: (color::GAUGE_FOOT.hsla(), color::GAUGE_HEAD.hsla()),
            value: value.into(),
            caption: caption.into(),
            value_color: color::TEXT.hsla(),
        }
    }

    /// Fill solid in `fill`, in place of the resting gradient.
    pub fn fill(mut self, fill: Hsla) -> Self {
        self.fill = (fill, fill);
        self
    }

    /// Dim the value, for a reading that is stale or missing.
    pub fn muted(mut self) -> Self {
        self.value_color = color::TEXT_MUTED.hsla();
        self
    }

    #[cfg(test)]
    pub(crate) fn fraction(&self) -> Option<f32> {
        self.fraction
    }

    #[cfg(test)]
    pub(crate) fn caption(&self) -> &SharedString {
        &self.caption
    }

    pub fn render(self) -> Div {
        let fraction = self.fraction;
        let fill = self.fill;
        div()
            .relative()
            .flex_none()
            .size(GAUGE_SIDE)
            .child(
                canvas(
                    |_, _, _| {},
                    move |bounds, _, window, _| paint_gauge(window, bounds, fraction, fill),
                )
                .absolute()
                .size_full(),
            )
            .child(
                // Proportional figures: a standalone value at this size reads
                // loose with every digit as wide as a zero.
                div()
                    .absolute()
                    .inset_0()
                    .flex()
                    .flex_col()
                    .items_center()
                    .justify_center()
                    .child(
                        div()
                            .text_size(text::READING)
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(self.value_color)
                            .child(self.value),
                    )
                    .child(
                        div()
                            .text_size(text::LABEL_SM)
                            .text_color(color::TEXT_MUTED.hsla())
                            .child(self.caption),
                    ),
            )
    }
}

/// The point on the arc at `share` of the sweep.
fn arc_point(center: Point<Pixels>, radius: Pixels, share: f32) -> Point<Pixels> {
    let angle = START + SWEEP * share.clamp(0.0, 1.0);
    point(
        center.x + radius * angle.cos(),
        center.y + radius * angle.sin(),
    )
}

/// `foot` and `head` mixed at `at`, from 0 to 1, in RGB as the panel mixes
/// them.
fn shade(foot: Hsla, head: Hsla, at: f32) -> Hsla {
    let (foot, head) = (foot.to_rgb(), head.to_rgb());
    let mix = |from: f32, to: f32| from + (to - from) * at;
    Rgba {
        r: mix(foot.r, head.r),
        g: mix(foot.g, head.g),
        b: mix(foot.b, head.b),
        a: mix(foot.a, head.a),
    }
    .into()
}

fn stroke_arc(
    window: &mut Window,
    center: Point<Pixels>,
    radius: Pixels,
    to: f32,
    (foot, head): (Hsla, Hsla),
) {
    // Three degrees a segment: smooth at this size, and still a few dozen
    // vertices for a whole dial.
    let segments = ((SWEEP * to) / (3.0f32.to_radians())).ceil().max(1.0) as usize;
    let at_step = |step: usize| arc_point(center, radius, to * step as f32 / segments as f32);
    let stroke = |window: &mut Window, steps: std::ops::RangeInclusive<usize>, color: Hsla| {
        let mut builder = PathBuilder::stroke(RING);
        for step in steps.clone() {
            if step == *steps.start() {
                builder.move_to(at_step(step));
            } else {
                builder.line_to(at_step(step));
            }
        }
        if let Ok(path) = builder.build() {
            window.paint_path(path, color);
        }
    };
    if foot == head {
        stroke(window, 0..=segments, foot);
    } else {
        // One path per segment, each in its own step of the shade. Every one
        // runs a segment past its end so its neighbor overlaps it: two
        // antialiased edges laid end to end leave a hairline between them.
        for step in 0..segments {
            let color = shade(foot, head, (step as f32 + 0.5) / segments as f32);
            stroke(window, step..=(step + 2).min(segments), color);
        }
    }
    // Round ends, painted rather than asked of the stroker, each in the color
    // of the end it closes.
    for (share, color) in [(0.0, foot), (to, head)] {
        let at = arc_point(center, radius, share);
        let cap = RING / 2.0;
        window.paint_quad(
            gpui::fill(
                Bounds::new(point(at.x - cap, at.y - cap), size(RING, RING)),
                color,
            )
            .corner_radii(cap),
        );
    }
}

fn paint_gauge(
    window: &mut Window,
    bounds: Bounds<Pixels>,
    fraction: Option<f32>,
    fill: (Hsla, Hsla),
) {
    let side = bounds.size.width.min(bounds.size.height);
    let center = point(
        bounds.origin.x + bounds.size.width / 2.0,
        bounds.origin.y + bounds.size.height / 2.0,
    );
    let radius = side / 2.0 - RING / 2.0 - px(1.0);
    if radius <= px(0.0) {
        return;
    }
    let track = color::CONTROL_HOVER.hsla();
    stroke_arc(window, center, radius, 1.0, (track, track));
    if let Some(fraction) = fraction.filter(|fraction| *fraction > 0.0) {
        stroke_arc(window, center, radius, fraction, fill);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_dial_opens_at_the_bottom_and_fills_clockwise_over_the_top() {
        let center = point(px(0.0), px(0.0));
        let radius = px(10.0);
        let start = arc_point(center, radius, 0.0);
        let middle = arc_point(center, radius, 0.5);
        let end = arc_point(center, radius, 1.0);
        // Screen coordinates grow downward: the ends sit below the center and
        // the halfway point sits straight above it.
        assert!(start.y > px(0.0) && end.y > px(0.0));
        assert!(start.x < px(0.0) && end.x > px(0.0));
        assert!((middle.x).abs() < px(0.001));
        assert!(middle.y < px(-9.9));
        // Past either end the dial holds at its end rather than wrapping.
        assert_eq!(arc_point(center, radius, 2.0), end);
    }

    #[test]
    fn the_shade_runs_from_the_foot_to_the_head_and_no_further() {
        let (foot, head) = (color::GAUGE_FOOT.hsla(), color::GAUGE_HEAD.hsla());
        let close = |a: Hsla, b: Hsla| {
            let (a, b) = (a.to_rgb(), b.to_rgb());
            (a.r - b.r).abs() + (a.g - b.g).abs() + (a.b - b.b).abs() < 0.01
        };
        assert!(close(shade(foot, head, 0.0), foot));
        assert!(close(shade(foot, head, 1.0), head));
        // Halfway is a mix of the two, not a third hue.
        let middle = shade(foot, head, 0.5).to_rgb();
        let (foot, head) = (foot.to_rgb(), head.to_rgb());
        assert!((middle.r - (foot.r + head.r) / 2.0).abs() < 0.01);
        assert!((middle.b - (foot.b + head.b) / 2.0).abs() < 0.01);
    }

    #[test]
    fn a_severity_fills_solid_and_the_resting_dial_shades() {
        let resting = RadialGauge::new(Some(0.5), "", "");
        assert_ne!(resting.fill.0, resting.fill.1);
        let warm = RadialGauge::new(Some(0.5), "", "").fill(color::WARNING.hsla());
        assert_eq!(warm.fill, (color::WARNING.hsla(), color::WARNING.hsla()));
    }

    #[test]
    fn a_missing_reading_is_an_empty_track_and_a_value_is_held_to_the_dial() {
        assert_eq!(RadialGauge::new(None, "--", "no reading").fraction, None);
        assert_eq!(RadialGauge::new(Some(1.4), "", "").fraction, Some(1.0));
        assert_eq!(RadialGauge::new(Some(-0.2), "", "").fraction, Some(0.0));
    }
}

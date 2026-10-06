// SPDX-FileCopyrightText: 2026 Arthur Jean
// SPDX-License-Identifier: GPL-3.0-or-later

//! The continuous corner every card, menu, row and button is drawn with.
//!
//! Paneflow's `ui_primitives::squircle`: three cubic segments per corner, from
//! the normalized UIKit control points Liam Rosenfeld documented
//! (<https://liamrosenfeld.com/posts/apple_icon_quest/>). A rounded rectangle
//! joins its arc to the straight edge with a step in curvature the eye reads as
//! a corner; this one eases into the edge, which is what makes a lit row and
//! the card around it read as one material.
//!
//! GPUI clips to rectangles and rounds a quad with circular arcs, so the shape
//! is painted as a path on a canvas laid under the element's content. The
//! element itself carries no fill: [`skin`] is how a surface gets one.
//!
//! Paneflow caches the tessellated paths. This window paints a few dozen of
//! them at most, so it builds them per paint and keeps nothing between frames.

use gpui::{
    Bounds, Div, Hsla, PathBuilder, Pixels, SharedString, Stateful, canvas, div, point, prelude::*,
    px, size,
};

/// How far one corner reaches along each edge, in units of the radius.
const CORNER_EXTENT: f32 = 1.528_665;
/// One corner as three cubic segments: two control points, then the end.
const CORNER_CURVES: [[(f32, f32); 3]; 3] = [
    [(1.088_493, 0.), (0.868_407, 0.), (0.631_494, 0.074_911)],
    [
        (0.372_824, 0.169_060),
        (0.169_060, 0.372_824),
        (0.074_911, 0.631_494),
    ],
    [(0., 0.868_407), (0., 1.088_493), (0., CORNER_EXTENT)],
];

/// The radius a box of this size can actually carry.
///
/// A corner reaches further than its radius, so two of them meet sooner than
/// two arcs would: the radius is clamped until they no longer overlap, which
/// is what turns a short row into a capsule rather than a knot.
fn limited_radius(bounds: Bounds<Pixels>, radius: Pixels) -> Pixels {
    radius
        .min(bounds.size.height / (2. * CORNER_EXTENT))
        .min(bounds.size.width / (2. * CORNER_EXTENT))
        .max(px(0.))
}

fn trace(builder: &mut PathBuilder, bounds: Bounds<Pixels>, radius: Pixels) {
    let radius = limited_radius(bounds, radius);
    let (left, right) = (bounds.left(), bounds.right());
    let (top, bottom) = (bounds.top(), bounds.bottom());
    builder.move_to(point(left + radius * CORNER_EXTENT, top));
    for corner in 0..4 {
        let transform = |(x, y): (f32, f32)| match corner {
            0 => point(right - radius * x, top + radius * y),
            1 => point(right - radius * y, bottom - radius * x),
            2 => point(left + radius * x, bottom - radius * y),
            _ => point(left + radius * y, top + radius * x),
        };
        builder.line_to(transform((CORNER_EXTENT, 0.)));
        for [control_a, control_b, end] in CORNER_CURVES {
            builder.cubic_bezier_to(transform(end), transform(control_a), transform(control_b));
        }
    }
    builder.close();
}

pub(crate) fn fill_path(bounds: Bounds<Pixels>, radius: Pixels) -> Option<gpui::Path<Pixels>> {
    if bounds.size.width <= px(0.) || bounds.size.height <= px(0.) {
        return None;
    }
    let mut builder = PathBuilder::fill();
    trace(&mut builder, bounds, radius);
    builder.build().ok()
}

pub(crate) fn stroke_path(
    bounds: Bounds<Pixels>,
    radius: Pixels,
    width: Pixels,
) -> Option<gpui::Path<Pixels>> {
    if bounds.size.width <= px(0.) || bounds.size.height <= px(0.) {
        return None;
    }
    let half = width / 2.;
    let radius = limited_radius(bounds, radius);
    let inner = Bounds {
        origin: bounds.origin + point(half, half),
        size: size(
            (bounds.size.width - width).max(px(0.)),
            (bounds.size.height - width).max(px(0.)),
        ),
    };
    let mut builder = PathBuilder::stroke(width);
    trace(&mut builder, inner, (radius - half).max(px(0.)));
    builder.build().ok()
}

/// A fill of `color` in the shape of the element it is laid in.
///
/// Absolute and first among its parent's children, so everything the parent
/// holds paints over it. The parent has to be `relative`.
pub fn fill(radius: Pixels, color: Hsla) -> Div {
    div().absolute().inset_0().child(
        canvas(
            |_, _, _| {},
            move |bounds, _, window, _| {
                if color.a <= f32::EPSILON {
                    return;
                }
                if let Some(path) = fill_path(bounds, radius) {
                    window.paint_path(path, color);
                }
            },
        )
        .size_full(),
    )
}

/// A hairline of `color` along the same outline.
pub fn border(radius: Pixels, width: Pixels, color: Hsla) -> Div {
    div().absolute().inset_0().child(
        canvas(
            |_, _, _| {},
            move |bounds, _, window, _| {
                if color.a <= f32::EPSILON {
                    return;
                }
                if let Some(path) = stroke_path(bounds, radius, width) {
                    window.paint_path(path, color);
                }
            },
        )
        .size_full(),
    )
}

/// Give an element a resting fill and a fill under the pointer, both
/// continuous.
///
/// Paneflow's `squircle_skin`. The hover layer is always laid out and only
/// made visible while the group is hovered, so the element's own `hover` style
/// stays free: GPUI asserts it is set once, and a skin that took it would make
/// every caller choose between a lit row and a lit label.
///
/// Must be called before the element gets its content, so the fills paint
/// under it.
pub fn skin(
    element: Stateful<Div>,
    group: impl Into<SharedString>,
    radius: Pixels,
    resting: Option<Hsla>,
    hovered: Option<Hsla>,
) -> Stateful<Div> {
    let group: SharedString = group.into();
    let mut element = element.relative().group(group.clone());
    if let Some(resting) = resting {
        element = element.child(fill(radius, resting));
    }
    if let Some(hovered) = hovered {
        element = element.child(
            div()
                .absolute()
                .inset_0()
                .invisible()
                .group_hover(group, |style| style.visible())
                .child(fill(radius, hovered)),
        );
    }
    element
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_component_radii_fit_their_components_without_clamping() {
        // Row, nav entry, card and menu, each at the smallest size it is drawn
        // at. A radius that had to be clamped here would be a corner the theme
        // promises and the screen does not paint.
        for (width, height, radius) in [
            (180., 32., 9.),
            (300., 100., 20.),
            (200., 80., 18.),
            (120., 44., 14.),
        ] {
            let bounds = Bounds::new(point(px(0.), px(0.)), size(px(width), px(height)));
            assert_eq!(limited_radius(bounds, px(radius)), px(radius));
            assert!(fill_path(bounds, px(radius)).is_some());
            assert!(stroke_path(bounds, px(radius), px(1.)).is_some());
        }
    }

    #[test]
    fn corners_cannot_overlap_in_small_bounds() {
        for (width, height) in [(1., 28.), (284., 1.), (0., 0.)] {
            let bounds = Bounds::new(point(px(0.), px(0.)), size(px(width), px(height)));
            let extent = limited_radius(bounds, px(9.)) * CORNER_EXTENT;
            assert!(extent <= px(width / 2.));
            assert!(extent <= px(height / 2.));
        }
    }

    #[test]
    fn an_empty_box_paints_nothing() {
        for bounds in [
            Bounds::new(point(px(3.), px(3.)), size(px(0.), px(18.))),
            Bounds::new(point(px(3.), px(3.)), size(px(24.), px(0.))),
        ] {
            assert!(fill_path(bounds, px(5.)).is_none());
            assert!(stroke_path(bounds, px(5.), px(1.)).is_none());
        }
    }
}

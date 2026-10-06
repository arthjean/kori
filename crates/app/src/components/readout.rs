// SPDX-FileCopyrightText: 2026 Arthur Jean
// SPDX-License-Identifier: GPL-3.0-or-later

//! The readout of what a device is and how far it can be trusted.
//!
//! Nothing here is operable. Each of them states a value and how much it can be
//! trusted, and states the second in a word rather than in a color alone.

use gpui::{Div, Hsla, SharedString, div, prelude::*};

use crate::theme::{DEVICE_LINE_HEIGHT, META_SEPARATOR, color, numeric_font, space, text};

/// How a device presents in a [`DeviceRow`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceHealth {
    /// Present and writable.
    Ready,
    /// Present but read-only.
    ReadOnly,
    /// Not present, or ownership was refused.
    Unavailable,
}

impl DeviceHealth {
    /// The status color, shared with any screen that names a device's state.
    pub fn color(self) -> Hsla {
        match self {
            Self::Ready => color::SUCCESS.hsla(),
            Self::ReadOnly => color::WARNING.hsla(),
            Self::Unavailable => color::DANGER.hsla(),
        }
    }

    /// Text label, so status is never carried by color alone.
    ///
    /// The word is the whole cue. A glyph used to sit beside it and nothing has
    /// drawn one since the device line became a caption: [`DeviceRow::render`]
    /// carries no leading status mark, and the diagnostics screen writes the
    /// label into a text row. A second vocabulary nothing paints is three
    /// assets in a binary whose point is to be self-contained.
    pub fn label(self) -> &'static str {
        match self {
            Self::Ready => "Ready",
            Self::ReadOnly => "Read-only",
            Self::Unavailable => "Unavailable",
        }
    }
}

/// One hardware device and its current state.
pub struct DeviceRow {
    name: SharedString,
    identifier: SharedString,
    health: DeviceHealth,
    detail: Option<SharedString>,
}

impl DeviceRow {
    pub fn new(
        name: impl Into<SharedString>,
        identifier: impl Into<SharedString>,
        health: DeviceHealth,
    ) -> Self {
        Self {
            name: name.into(),
            identifier: identifier.into(),
            health,
            detail: None,
        }
    }

    pub fn detail(mut self, detail: impl Into<SharedString>) -> Self {
        self.detail = Some(detail.into());
        self
    }

    /// One line: what the device is, how it was identified, and what state it
    /// is in.
    ///
    /// It used to be two lines inside a titled panel, and on the monitoring
    /// screen that block outweighed the readouts under it: a card, a heading, a
    /// sentence of policy and four lines of prose, all of it answering a
    /// question the operator asks once a session. Provenance is a caption, so it
    /// is set like one. The whole line is body size and hierarchy
    /// is carried by color alone: the name in ink, the identity muted, the state
    /// in its own color at the far right.
    ///
    /// The row carries no separator, no fill and no leading status glyph. State
    /// is still named in words, never by color alone; the colored label is the
    /// word. The identity block takes the slack between the two, so the state
    /// column lands on the same right edge on every line whatever the name in
    /// front of it measures, and truncates rather than wrapping: a device that
    /// reports a long firmware string must not push its own state off the line.
    ///
    /// `min_w_0` belongs on the flex containers, never on the element that
    /// holds the text. On a text element it removes the intrinsic minimum a
    /// line needs, and GPUI then wraps the name one glyph per line rather than
    /// letting the row be as wide as its content.
    pub fn render(self) -> Div {
        div()
            .flex()
            // Wrapping is the escape hatch for a window narrow enough that the
            // name and the state alone no longer fit. The identity block shrinks
            // first and reaches zero before that happens, so in practice this
            // only fires on a window narrower than the layout targets.
            .flex_wrap()
            .items_center()
            .w_full()
            .min_w_0()
            .min_h(DEVICE_LINE_HEIGHT)
            .gap(space::SM)
            .text_size(text::BODY)
            .child(
                div()
                    .flex_none()
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(color::TEXT.hsla())
                    .child(self.name),
            )
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_w_0()
                    .items_center()
                    .gap(space::XS)
                    .text_color(color::TEXT_MUTED.hsla())
                    .child(
                        div()
                            .flex_none()
                            .font(numeric_font())
                            .child(self.identifier),
                    )
                    .children(self.detail.map(|detail| {
                        div()
                            .flex()
                            .min_w_0()
                            .items_center()
                            .gap(space::XS)
                            .child(
                                div()
                                    .flex_none()
                                    .text_color(color::TEXT_DISABLED.hsla())
                                    .child(META_SEPARATOR),
                            )
                            .child(div().min_w_0().truncate().child(detail))
                    })),
            )
            .child(
                div()
                    .flex_none()
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(self.health.color())
                    .child(self.health.label()),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_health_is_named_in_words_and_never_by_color_alone() {
        let labels: Vec<&str> = [
            DeviceHealth::Ready,
            DeviceHealth::ReadOnly,
            DeviceHealth::Unavailable,
        ]
        .map(DeviceHealth::label)
        .to_vec();

        assert!(labels.iter().all(|label| !label.is_empty()));
        let mut unique = labels.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(
            unique.len(),
            labels.len(),
            "two states share a word: {labels:?}"
        );
    }
}

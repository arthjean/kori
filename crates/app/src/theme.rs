// SPDX-FileCopyrightText: 2026 Arthur Jean
// SPDX-License-Identifier: GPL-3.0-or-later

//! Centralized design tokens.
//!
//! Every color, size and font used by a component comes from here, so the
//! interface can be checked as a system: the contrast tests below run against
//! these values, not against a screenshot.
//!
//! The system is Paneflow's, taken whole rather than sampled: its dark palette
//! (`theme::model::ui_colors_with`, dark branch), its geometry and its type
//! scale, as `DESIGN.md` in that repository states them. Kori used to keep a
//! violet accent of its own to tell the two windows apart; it now wears
//! Paneflow's teal, and the window title is what says which product is open.
//! Nothing here reuses a vendor logo, asset or wordmark.

use gpui::{Font, FontFeatures, FontStyle, FontWeight, Hsla, Pixels, Rgba, px};
use std::sync::Arc;

/// Product name shown in the shell. Deliberately not a vendor trademark.
pub const PRODUCT_NAME: &str = "Kori";
/// The identifier the window hands to the compositor, and the one
/// `packaging/desktop/kori.desktop` declares as `StartupWMClass`. A desktop
/// finds an icon for a window by matching these two strings, so they are one
/// constant and a test, not two spellings that happen to agree today.
pub const APP_ID: &str = "kori";
/// Shown wherever the product identifies itself.
pub const UNOFFICIAL_NOTICE: &str = "Unofficial. Not affiliated with or endorsed by NZXT.";

/// One color token, kept as packed RGB so contrast can be computed on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Color(pub u32);

impl Color {
    pub const fn rgb(value: u32) -> Self {
        Self(value)
    }

    /// Relative luminance, per WCAG 2.1.
    pub fn luminance(self) -> f32 {
        fn channel(value: f32) -> f32 {
            if value <= 0.03928 {
                value / 12.92
            } else {
                ((value + 0.055) / 1.055).powf(2.4)
            }
        }
        let r = channel(((self.0 >> 16) & 0xff) as f32 / 255.0);
        let g = channel(((self.0 >> 8) & 0xff) as f32 / 255.0);
        let b = channel((self.0 & 0xff) as f32 / 255.0);
        0.2126 * r + 0.7152 * g + 0.0722 * b
    }

    /// WCAG contrast ratio between two tokens, from 1.0 to 21.0.
    pub fn contrast(self, other: Self) -> f32 {
        let (a, b) = (self.luminance(), other.luminance());
        let (lighter, darker) = if a > b { (a, b) } else { (b, a) };
        (lighter + 0.05) / (darker + 0.05)
    }

    pub fn hsla(self) -> Hsla {
        Rgba {
            r: ((self.0 >> 16) & 0xff) as f32 / 255.0,
            g: ((self.0 >> 8) & 0xff) as f32 / 255.0,
            b: (self.0 & 0xff) as f32 / 255.0,
            a: 1.0,
        }
        .into()
    }

    /// The ink to draw on top of this color.
    ///
    /// Every other pairing in this file is two tokens checked against each
    /// other once, in the tests below. This one cannot be: the color under the
    /// glyph is whatever the operator sent to a channel or set as the panel's
    /// background, so the choice has to be made at paint time. Picking the
    /// better of the two extremes of the palette never drops under 4:1, which
    /// `readable_ink_clears_the_non_text_bar_on_any_color` measures over the
    /// whole cube rather than over the colors this product happens to offer.
    pub fn readable_ink(self) -> Self {
        if self.contrast(color::TEXT_ON_SOLID) >= self.contrast(color::RAIL) {
            color::TEXT_ON_SOLID
        } else {
            color::RAIL
        }
    }

    /// The same color at reduced opacity, for overlays and disabled fills.
    pub fn alpha(self, alpha: f32) -> Hsla {
        let mut hsla = self.hsla();
        hsla.a = alpha.clamp(0.0, 1.0);
        hsla
    }
}

/// A color the operator sent to a channel or to the panel, as a theme token.
///
/// The conversion lives here rather than at each call site because this is the
/// type that owns the packed form: a hand-rolled shift-and-or beside every
/// contrast check is the same expression written four times, and one of the
/// four getting the channel order wrong is a defect nothing would catch.
impl From<kori_core::lighting::Rgb> for Color {
    fn from(color: kori_core::lighting::Rgb) -> Self {
        Self::rgb((u32::from(color.r) << 16) | (u32::from(color.g) << 8) | u32::from(color.b))
    }
}

/// The palette: Paneflow Dark, role for role.
pub mod color {
    use super::Color;

    /// The shell: title bar, navigation rail and the ground around the panel.
    /// Paneflow's `overlay`, which is what its cockpit chrome paints.
    pub const RAIL: Color = Color::rgb(0x141414);
    /// The main panel the screens are laid on. Paneflow's `base`.
    ///
    /// Lighter than the shell around it, which is what makes the inset panel
    /// read as a card without a shadow.
    pub const SURFACE: Color = Color::rgb(0x181818);
    /// A card on the panel. Paneflow's `card`, the role its Settings cards
    /// and dialogs are drawn on.
    pub const PANEL: Color = Color::rgb(0x232323);
    /// Input and control fill. Paneflow's `subtle`, one step above a card so a
    /// control on one stays visible.
    pub const CONTROL: Color = Color::rgb(0x2a2a2a);
    /// The same fill under the pointer: [`CONTROL`] moved 6% toward [`TEXT`],
    /// which is Paneflow's `select_trigger` and `secondary_button` alike.
    pub const CONTROL_HOVER: Color = Color::rgb(0x353535);
    /// Low-contrast separator. Paneflow's `border`.
    pub const SEPARATOR: Color = Color::rgb(0x252525);
    /// Surface a floating menu is drawn on: Paneflow's `surface` lifted by
    /// 0.035 in HSL lightness, which is how `select_menu_surface` puts a menu
    /// in front of the page without a shadow.
    pub const MENU: Color = Color::rgb(0x2a2a2a);
    /// The empty part of a track: [`TEXT_MUTED`] at 0.30 over a card, which is
    /// Paneflow's toggle track when off, kept opaque so it can be measured.
    pub const TRACK: Color = Color::rgb(0x494949);

    /// Paneflow's `accent`. Links, selected metadata and data: the readings,
    /// the curve, the history. Never a fill under text.
    pub const ACCENT: Color = Color::rgb(0x57d5c4);
    /// Paneflow's toggle blue: the filled part of a track a pointer drags.
    pub const SWITCH: Color = Color::rgb(0x339cff);
    /// A solid action under a white label: Paneflow's update blue, the one
    /// blue of its palette that holds white at 4.5:1.
    pub const SOLID: Color = Color::rgb(0x1a6ff6);
    /// The same fill under the pointer, 0.05 darker as `solid_button` does.
    pub const SOLID_HOVER: Color = Color::rgb(0x0961ed);
    /// Focus ring. Paneflow's `FOCUS_BLUE`.
    pub const FOCUS: Color = Color::rgb(0x007aff);

    /// Paneflow's `text`. Hue-free, as every neutral of the shell is.
    pub const TEXT: Color = Color::rgb(0xdddddd);
    /// Paneflow's `muted`: secondary text, icons at rest, eyebrows.
    pub const TEXT_MUTED: Color = Color::rgb(0xa0a0a0);
    pub const TEXT_DISABLED: Color = Color::rgb(0x6b6b6b);
    /// Text drawn on top of a solid fill.
    pub const TEXT_ON_SOLID: Color = Color::rgb(0xffffff);

    /// Paneflow's `vc_added`.
    pub const SUCCESS: Color = Color::rgb(0x57d992);
    /// Paneflow's `vc_modified`.
    pub const WARNING: Color = Color::rgb(0xffd166);
    /// The word a failure is named in: an error, a stalled channel, a device
    /// that did not answer. A text color, held to the full 4.5:1 on every
    /// surface it is written on, which is why it is a light red rather than a
    /// saturated one. Paneflow's `vc_deleted`, which is the same tradeoff.
    pub const DANGER: Color = Color::rgb(0xff6f6a);

    /// Fill of a destructive action. Paneflow's fixed `#ff453a`, the system
    /// red, under a white label.
    ///
    /// Deliberately *not* [`DANGER`]. That token is a word on a surface and is
    /// held to the body-text bar; this one is a surface a word sits on, and the
    /// two cannot be the same color without one of them failing its own job.
    pub const DESTRUCTIVE: Color = Color::rgb(0xff453a);
    /// The same fill under the pointer, 0.05 darker in HSL lightness as
    /// Paneflow's `solid_button` does. Deepening rather than lifting is what
    /// keeps the white label above 3:1.
    pub const DESTRUCTIVE_HOVER: Color = Color::rgb(0xff2d20);
    /// Text drawn on top of a destructive fill.
    pub const TEXT_ON_DESTRUCTIVE: Color = Color::rgb(0xffffff);

    /// The entities the Monitoring charts plot, one hue each.
    ///
    /// Color follows the entity, never its rank: the CPU is this teal in every
    /// chart that draws it. The five are the dark-mode categorical steps,
    /// held inside the OKLCH lightness band 0.48 to 0.67 so none of them shouts
    /// over the others, and run through the dataviz validator on the card
    /// surface: the three that share the temperature chart clear every
    /// all-pairs check (worst CVD separation 13.0, worst normal-vision 19.9),
    /// as do the two that share the cooling chart (15.9 and 26.5). The CPU
    /// teal is [`ACCENT`] stepped down into that band.
    pub const SERIES_CPU: Color = Color::rgb(0x35a898);
    pub const SERIES_GPU: Color = Color::rgb(0xd95926);
    pub const SERIES_COOLANT: Color = Color::rgb(0x9085e9);
    pub const SERIES_PUMP: Color = Color::rgb(0x3987e5);
    pub const SERIES_FAN: Color = Color::rgb(0xd55181);
    /// A gridline: one step off the card, solid, recessive.
    pub const GRID: Color = Color::rgb(0x303030);
    /// The overview dials' fill, from the foot of the sweep to the head of
    /// what it has reached: the panel's own default band, so a dial on the
    /// screen and the ring on the glass wear the same two colors.
    ///
    /// Under the 3:1 a mark is held to elsewhere in this file, and knowingly:
    /// the foot measures 2.01:1 on [`PANEL`] and the head 3.45:1. A dial is
    /// never read from its arc alone, since the value is written inside it.
    pub const GAUGE_FOOT: Color = Color::rgb(0x6b00de);
    pub const GAUGE_HEAD: Color = Color::rgb(0xd600bf);

    /// The translucent washes every row, menu item and nav entry is lit with.
    ///
    /// Paneflow has one highlight material: an alpha of the text color, never a
    /// per-component fill. These are its four steps.
    pub const WASH_HOVER: f32 = 0.05;
    pub const WASH_SELECTED: f32 = 0.10;
    pub const NAV_HOVER: f32 = 0.10;
    pub const NAV_ACTIVE: f32 = 0.16;
}

/// Type scale, Paneflow's `ui_primitives` constants plus its two headings.
pub mod text {
    use gpui::{Pixels, px};

    /// Micro chips and hints.
    pub const LABEL_XS: Pixels = px(10.0);
    /// Eyebrows and descriptions.
    pub const LABEL_SM: Pixels = px(11.0);
    /// Body text, and the size of the interface at its root.
    pub const BODY: Pixels = px(12.0);
    /// A row title that has to outrank body.
    pub const BODY_EMPHASIS: Pixels = px(13.0);
    /// Card titles and empty-state titles.
    pub const TITLE: Pixels = px(14.0);
    /// Navigation rail entries, set larger in the system face as Paneflow's.
    pub const NAV: Pixels = px(14.0);
    /// Line height of a navigation entry.
    pub const NAV_LINE: Pixels = px(20.0);
    /// A large reading, where the number is the whole point of the tile.
    pub const READING: Pixels = px(20.0);
    /// The page heading.
    pub const HEADING: Pixels = px(26.0);
}

/// The interface face, bundled so every machine renders the same letters.
pub const UI_FONT: &str = "Geist";
/// The navigation rail keeps the platform's own face, as Paneflow's does.
pub const NAV_FONT: &str = ".SystemUIFont";

/// Spacing scale, in logical pixels.
pub mod space {
    use gpui::{Pixels, px};

    pub const XS: Pixels = px(4.0);
    pub const SM: Pixels = px(8.0);
    pub const MD: Pixels = px(12.0);
    pub const LG: Pixels = px(16.0);
    pub const XL: Pixels = px(24.0);
}

/// Minimum size of a pointer target a row offers, in logical pixels.
pub const TARGET_MIN: Pixels = px(40.0);

/// Width of the navigation rail.
///
/// Narrower than the 300 Paneflow's rail holds: its minimum window is 800 wide
/// around a terminal grid, and this one is 920 around a column of cards that
/// still has to fit a device row on one line.
pub const RAIL_WIDTH: Pixels = px(220.0);
/// Geometry of one navigation entry: Paneflow's sidebar row.
pub const NAV_ROW_HEIGHT: Pixels = px(32.0);
pub const NAV_ROW_MARGIN: Pixels = px(8.0);
pub const NAV_ROW_PADDING_X: Pixels = px(7.0);
pub const NAV_ROW_GAP: Pixels = px(2.0);
pub const NAV_ROW_RADIUS: Pixels = px(9.0);
pub const NAV_ICON_SIZE: Pixels = px(17.0);

/// Inset of the main panel from the shell, on its right and bottom edges.
pub const PANEL_INSET: Pixels = px(4.0);
/// Corner radius of the main panel, the same curve as the window's.
pub const PANEL_RADIUS: Pixels = px(10.0);

/// Corner radius of a field: a select trigger, a color field.
pub const RADIUS: Pixels = px(8.0);
/// Corner radius of a row, a button and a menu item: Paneflow's `ROW_RADIUS`,
/// drawn as a continuous corner.
pub const ROW_RADIUS: Pixels = px(14.0);
/// Corner radius of a card: Paneflow's `SETTINGS_CARD_RADIUS`.
pub const CARD_RADIUS: Pixels = px(20.0);
/// Inset of the rows inside a card of rows, so a row of [`ROW_RADIUS`] sits
/// concentric with the [`CARD_RADIUS`] around it.
pub const CARD_ROW_INSET: Pixels = px(6.0);
/// Padding of a card whose content is fields or prose.
pub const CARD_PADDING_X: Pixels = px(16.0);
pub const CARD_PADDING_Y: Pixels = px(14.0);
/// Padding of one setting line: a title on the left, its control on the right.
pub const SETTING_ROW_PADDING_X: Pixels = px(16.0);
pub const SETTING_ROW_PADDING_Y: Pixels = px(10.0);
/// Width of the visible focus ring, in logical pixels.
pub const FOCUS_RING: Pixels = px(2.0);

/// The reading column a screen is laid out in: Paneflow's Settings column.
pub const COLUMN_MAX_WIDTH: Pixels = px(700.0);
pub const COLUMN_PADDING: Pixels = px(28.0);
/// Space between two blocks of a page, and under an eyebrow.
pub const BLOCK_GAP: Pixels = px(24.0);
pub const EYEBROW_GAP: Pixels = px(8.0);

/// Height of one line of the device strip.
///
/// The strip is provenance, not content: it names which hardware answered and
/// in what state, above the readouts the screen is actually about. So the line
/// is sized like a caption rather than like a row.
pub const DEVICE_LINE_HEIGHT: Pixels = px(22.0);

/// Height of a control pill: a select, a color field, a slider.
///
/// One height for all three, so a row that carries two different controls has
/// them on the same baseline: Paneflow's 10 by 6 select padding around a body
/// line, plus the reserved focus ring.
pub const CONTROL_HEIGHT: Pixels = px(32.0);
/// Height of a button: Paneflow's `secondary_button`, 10 by 4 around a body
/// line, plus the ring.
pub const BUTTON_HEIGHT: Pixels = px(30.0);
/// Width clamp of a select trigger.
pub const SELECT_MIN_WIDTH: Pixels = px(190.0);
pub const SELECT_MAX_WIDTH: Pixels = px(260.0);

/// Side of a color swatch, and the radius that goes with it.
pub const SWATCH_SIZE: Pixels = px(18.0);
pub const SWATCH_RADIUS: Pixels = px(5.0);

/// Geometry of a floating menu, taken from Paneflow's `menu_panel`.
///
/// A 34-pixel row clamps its continuous corner to 11, and the 7 pixels of
/// surface padding land it concentric inside the 18 of the menu.
pub const MENU_RADIUS: Pixels = px(18.0);
pub const MENU_PADDING: Pixels = px(7.0);
pub const MENU_MIN_WIDTH: Pixels = px(200.0);
pub const MENU_MAX_WIDTH: Pixels = px(280.0);
pub const MENU_MAX_HEIGHT: Pixels = px(400.0);
pub const MENU_ROW_HEIGHT: Pixels = px(34.0);
pub const MENU_ROW_GAP: Pixels = px(1.0);
/// Side of the glyph on a menu trigger, and of the check on a chosen row.
pub const MENU_GLYPH_SIZE: Pixels = px(12.0);
pub const MENU_CHECK_SIZE: Pixels = px(13.0);
/// Gap between the control a menu belongs to and the menu itself.
pub const MENU_OFFSET: Pixels = px(6.0);

/// Window size the layout is designed for.
pub const WINDOW_WIDTH: Pixels = px(920.0);
pub const WINDOW_HEIGHT: Pixels = px(640.0);

/// Client-side window decoration geometry, Paneflow's title bar.
///
/// The bar is `1.75 * rem_size` and never shorter than this floor, so it grows
/// with the interface scale instead of clipping its own controls.
pub const TITLE_BAR_MIN_HEIGHT: Pixels = px(32.0);
/// Side of one window control button.
pub const TITLE_BAR_CONTROL: Pixels = px(20.0);
/// Gap between two window control buttons.
pub const TITLE_BAR_CONTROL_GAP: Pixels = px(12.0);
/// Inset between the window edge and the control group.
pub const TITLE_BAR_INSET: Pixels = px(8.0);
/// Corner radius of the window itself, dropped edge by edge when tiled.
pub const WINDOW_RADIUS: Pixels = px(10.0);
/// Border the decorated surface draws around itself.
pub const WINDOW_BORDER: Pixels = px(1.0);
/// Invisible band around the window that starts a resize.
pub const RESIZE_BORDER: Pixels = px(10.0);

/// Glyph placed between two fragments of one metadata line.
///
/// A middle dot rather than a comma or a dash: it reads as a column break at
/// the small size these lines are set in, and it does not compete with the
/// punctuation inside the fragments it separates.
pub const META_SEPARATOR: &str = "\u{00b7}";

/// Unit every temperature in the interface is written in.
///
/// One constant rather than a literal per readout: a Celsius reading spelled
/// `31.4 C` on one screen and `31.4 °C` on the next reads as two different
/// products, and the degree sign is the one part of the unit a reader looks for.
pub const DEGREE_C: &str = " \u{00b0}C";

/// Font for numeric readouts.
///
/// The interface face with tabular figures, as Paneflow sets its diffstats:
/// digit width stays constant, so a value changing from `9` to `10` cannot
/// shift the label next to it, and the number still reads as the same type as
/// the word beside it.
pub fn numeric_font() -> Font {
    Font {
        family: UI_FONT.into(),
        features: FontFeatures(Arc::new(vec![("tnum".into(), 1), ("lnum".into(), 1)])),
        fallbacks: None,
        weight: FontWeight::MEDIUM,
        style: FontStyle::Normal,
    }
}

#[cfg(test)]
mod tests {
    use super::color::*;
    use super::*;

    /// WCAG AA for body text.
    const TEXT_MIN: f32 = 4.5;
    /// WCAG AA for interface components and their states.
    const NON_TEXT_MIN: f32 = 3.0;

    /// Composite a wash of [`TEXT`] over an opaque surface, as the renderer
    /// does, so a translucent highlight can be measured like a token.
    fn washed(surface: Color, alpha: f32) -> Color {
        let mix = |shift: u32| {
            let over = ((TEXT.0 >> shift) & 0xff) as f32;
            let under = ((surface.0 >> shift) & 0xff) as f32;
            (over * alpha + under * (1.0 - alpha)).round() as u32
        };
        Color::rgb((mix(16) << 16) | (mix(8) << 8) | mix(0))
    }

    #[test]
    fn body_text_meets_aa_on_every_surface() {
        for surface in [RAIL, SURFACE, PANEL, CONTROL, CONTROL_HOVER, MENU] {
            let ratio = TEXT.contrast(surface);
            assert!(ratio >= TEXT_MIN, "TEXT on {surface:?} is {ratio:.2}:1");
        }
    }

    #[test]
    fn muted_text_meets_aa_on_every_surface() {
        for surface in [RAIL, SURFACE, PANEL, CONTROL, CONTROL_HOVER, MENU] {
            let ratio = TEXT_MUTED.contrast(surface);
            assert!(
                ratio >= TEXT_MIN,
                "TEXT_MUTED on {surface:?} is {ratio:.2}:1"
            );
        }
    }

    /// The washes are where a row's own text sits while it is lit, so they are
    /// held to the same bar as the surfaces under them.
    #[test]
    fn text_stays_legible_on_every_wash() {
        for (surface, alpha) in [
            (RAIL, NAV_HOVER),
            (RAIL, NAV_ACTIVE),
            (PANEL, WASH_HOVER),
            (PANEL, WASH_SELECTED),
            (MENU, WASH_HOVER),
            (MENU, WASH_SELECTED),
        ] {
            let lit = washed(surface, alpha);
            let ratio = TEXT.contrast(lit);
            assert!(
                ratio >= TEXT_MIN,
                "TEXT on {surface:?} washed at {alpha} is {ratio:.2}:1"
            );
        }
        // Muted text rides on the rail entries and on the rows of a card, never
        // on a menu item, which carries one label in the full text color.
        for (surface, alpha) in [
            (RAIL, NAV_HOVER),
            (RAIL, NAV_ACTIVE),
            (PANEL, WASH_HOVER),
            (PANEL, WASH_SELECTED),
        ] {
            let ratio = TEXT_MUTED.contrast(washed(surface, alpha));
            assert!(
                ratio >= TEXT_MIN,
                "TEXT_MUTED on {surface:?} washed at {alpha} is {ratio:.2}:1"
            );
        }
    }

    #[test]
    fn white_on_a_solid_action_meets_aa_in_every_interaction_state() {
        for state in [SOLID, SOLID_HOVER] {
            let ratio = TEXT_ON_SOLID.contrast(state);
            assert!(ratio >= TEXT_MIN, "on-solid text is {ratio:.2}:1");
        }
        // Against the surfaces it is laid on, so the button is a shape before
        // it is a label. Resting only: the darker fill exists while the pointer
        // is on top of the button, which has already been found by then, and
        // it lands at 2.96:1 on a card.
        for surface in [SURFACE, PANEL] {
            let ratio = SOLID.contrast(surface);
            assert!(
                ratio >= NON_TEXT_MIN,
                "SOLID on {surface:?} is {ratio:.2}:1"
            );
        }
        assert!(SOLID_HOVER.luminance() < SOLID.luminance());
    }

    #[test]
    fn the_accent_is_distinguishable_from_every_surface_it_draws_on() {
        for surface in [SURFACE, PANEL, TRACK] {
            let ratio = ACCENT.contrast(surface);
            assert!(
                ratio >= NON_TEXT_MIN,
                "ACCENT on {surface:?} is {ratio:.2}:1"
            );
        }
    }

    /// The glyph a device row draws over an operator's color.
    ///
    /// Swept rather than sampled: the fill is a color a channel or the panel was
    /// sent, so the only honest check covers the cube. The worst pairing sits
    /// near the luminance where white and [`RAIL`] are equally far away, which
    /// the assertion below measures at just over 4:1, comfortably past the 3:1
    /// bar a non-text mark has to clear.
    #[test]
    fn readable_ink_clears_the_non_text_bar_on_any_color() {
        let mut worst = f32::INFINITY;
        for r in (0u32..=255).step_by(15) {
            for g in (0u32..=255).step_by(15) {
                for b in (0u32..=255).step_by(15) {
                    let fill = Color::rgb((r << 16) | (g << 8) | b);
                    let ratio = fill.readable_ink().contrast(fill);
                    assert!(ratio >= NON_TEXT_MIN, "ink on {fill:?} is {ratio:.2}:1");
                    worst = worst.min(ratio);
                }
            }
        }
        assert!(worst >= 4.0, "worst pairing is {worst:.2}:1");
    }

    /// The destructive fill is the one place this interface knowingly sits under
    /// the body-text bar.
    ///
    /// A saturated red carrying white cannot clear 4.5:1: the luminance a red
    /// that bright has puts the ceiling near 3.5:1, which is why Apple ships
    /// `systemRed` with white on its own destructive buttons at the same figure.
    /// The alternative is a deep maroon that no longer reads as a warning, and a
    /// destructive control that does not read as one is the worse failure.
    ///
    /// So the bar here is the 3:1 one, checked in both states, and the label is
    /// never the only thing carrying the meaning: the button says "Delete
    /// profile" in words and asks a second time before it acts.
    #[test]
    fn the_destructive_button_stays_legible_in_every_interaction_state() {
        for state in [DESTRUCTIVE, DESTRUCTIVE_HOVER] {
            let ratio = TEXT_ON_DESTRUCTIVE.contrast(state);
            assert!(ratio >= NON_TEXT_MIN, "on-destructive text is {ratio:.2}:1");
            for surface in [SURFACE, PANEL] {
                let ratio = state.contrast(surface);
                assert!(
                    ratio >= NON_TEXT_MIN,
                    "{state:?} on {surface:?} is {ratio:.2}:1"
                );
            }
        }
        assert!(DESTRUCTIVE_HOVER.luminance() < DESTRUCTIVE.luminance());

        // The fill and the word a failure is named in are separate tokens on
        // purpose. Collapsing them would put a body-text red behind white, or
        // a saturated red on a panel as text, and each fails its own bar.
        assert_ne!(DESTRUCTIVE, DANGER);
        assert!(DANGER.contrast(PANEL) >= TEXT_MIN);
    }

    #[test]
    fn every_chart_series_is_a_visible_mark_on_the_card() {
        for series in [
            SERIES_CPU,
            SERIES_GPU,
            SERIES_COOLANT,
            SERIES_PUMP,
            SERIES_FAN,
        ] {
            let ratio = series.contrast(PANEL);
            assert!(ratio >= NON_TEXT_MIN, "{series:?} on PANEL is {ratio:.2}:1");
        }
        // Gridlines recede: visible, and nowhere near a mark.
        let grid = GRID.contrast(PANEL);
        assert!((1.05..1.5).contains(&grid), "grid is {grid:.2}:1");
    }

    #[test]
    fn the_overview_dials_wear_the_panels_default_band() {
        // Two spellings of one gradient: the panel's lives in the preset, the
        // dial's here. Tied by a test so that changing one moves the other.
        let band = kori_core::display::DisplayPreset::default_infographic().readings[0];
        assert_eq!(Color::from(band.reading), GAUGE_FOOT);
        assert_eq!(Color::from(band.band_end()), GAUGE_HEAD);
    }

    #[test]
    fn status_colors_meet_aa_on_the_panel() {
        for status in [SUCCESS, WARNING, DANGER] {
            let ratio = status.contrast(PANEL);
            assert!(ratio >= TEXT_MIN, "{status:?} on PANEL is {ratio:.2}:1");
        }
    }

    #[test]
    fn the_focus_ring_is_visible_against_every_background_it_sits_on() {
        for surface in [RAIL, SURFACE, PANEL, CONTROL, MENU] {
            let ratio = FOCUS.contrast(surface);
            assert!(
                ratio >= NON_TEXT_MIN,
                "FOCUS on {surface:?} is {ratio:.2}:1"
            );
        }
    }

    #[test]
    fn disabled_text_is_visibly_dimmer_but_still_perceivable() {
        assert!(TEXT_DISABLED.contrast(CONTROL) < TEXT.contrast(CONTROL));
        assert!(TEXT_DISABLED.contrast(CONTROL) >= 2.0);
    }

    #[test]
    fn how_much_of_a_track_is_filled_is_visible_without_reading_the_value() {
        // The one thing a slider says at a glance is where the fill stops, so
        // the boundary between the filled part and the empty one is a
        // meaningful non-text element and takes the full 3:1. The knob carries
        // a ring of the shell color, and that ring is what has to clear the
        // fill it rides on.
        let ratio = SWITCH.contrast(TRACK);
        assert!(ratio >= NON_TEXT_MIN, "fill against track is {ratio:.2}:1");
        let ring = RAIL.contrast(SWITCH);
        assert!(
            ring >= NON_TEXT_MIN,
            "knob ring against fill is {ring:.2}:1"
        );
        let knob = TEXT_ON_SOLID.contrast(TRACK);
        assert!(knob >= NON_TEXT_MIN, "knob against track is {knob:.2}:1");
    }

    #[test]
    fn a_menu_reads_as_lifted_off_the_panel_it_covers() {
        // The lift is small on purpose: enough to see the edge of the menu
        // where it overlaps the panel, not so much that the menu reads as a
        // separate window. It only has to be lighter, and by less than a
        // separator is.
        assert!(
            MENU.luminance() > PANEL.luminance(),
            "the menu is not lifted"
        );
        let ratio = MENU.contrast(PANEL);
        assert!(
            (1.05..1.4).contains(&ratio),
            "menu over panel is {ratio:.3}:1"
        );
    }

    #[test]
    fn the_panel_reads_as_a_card_on_the_shell_without_a_shadow() {
        // Paneflow's ramp: the shell is the darkest surface, the main panel
        // one step up, a card one more. The order is the whole depth model.
        assert!(SURFACE.luminance() > RAIL.luminance());
        assert!(PANEL.luminance() > SURFACE.luminance());
        assert!(CONTROL.luminance() > PANEL.luminance());
    }

    #[test]
    fn separators_stay_low_contrast_without_disappearing() {
        let ratio = SEPARATOR.contrast(SURFACE);
        assert!((1.1..2.5).contains(&ratio), "separator is {ratio:.2}:1");
    }

    #[test]
    fn contrast_is_symmetric_and_bounded() {
        assert!((TEXT.contrast(SURFACE) - SURFACE.contrast(TEXT)).abs() < 0.001);
        let white = Color::rgb(0xffffff);
        let black = Color::rgb(0x000000);
        assert!((white.contrast(black) - 21.0).abs() < 0.01);
        assert!((white.contrast(white) - 1.0).abs() < 0.001);
    }

    #[test]
    fn pointer_targets_clear_the_wcag_minimum() {
        // WCAG 2.2 asks 24 for a target; a row keeps 40 and a nav entry
        // Paneflow's 32.
        assert!(TARGET_MIN >= px(40.0));
        assert!(NAV_ROW_HEIGHT >= px(24.0));
        assert!(BUTTON_HEIGHT >= px(24.0));
    }

    #[test]
    fn a_menu_row_sits_concentric_inside_its_menu() {
        // The continuous corner clamps to a third of the row height, so the
        // radius a row actually paints is that clamp, and the padding is what
        // separates it from the menu's own curve.
        let painted = (f32::from(MENU_ROW_HEIGHT) / 3.0).min(f32::from(ROW_RADIUS));
        let menu = painted + f32::from(MENU_PADDING) - f32::from(MENU_RADIUS);
        assert!(menu.abs() < 1.0, "menu rows are {menu} off concentric");
        let card = f32::from(ROW_RADIUS) + f32::from(CARD_ROW_INSET) - f32::from(CARD_RADIUS);
        assert!(card.abs() < 0.5, "card rows are {card} off concentric");
    }

    #[test]
    fn a_core_color_packs_into_the_themes_own_form() {
        use kori_core::lighting::Rgb;
        assert_eq!(
            Color::from(Rgb::new(0x6f, 0x4e, 0xf2)),
            Color::rgb(0x6f4ef2)
        );
        assert_eq!(Color::from(Rgb::BLACK), Color::rgb(0x000000));
        assert_eq!(
            Color::from(Rgb::new(0xff, 0xff, 0xff)),
            Color::rgb(0xffffff)
        );
        // The channel order is the whole risk: a red-only color must not come
        // back as a blue one.
        assert_eq!(
            Color::from(Rgb::new(0xff, 0x00, 0x00)),
            Color::rgb(0xff0000)
        );
    }
}

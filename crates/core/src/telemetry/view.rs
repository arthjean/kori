// SPDX-FileCopyrightText: 2026 Arthur Jean
// SPDX-License-Identifier: GPL-3.0-or-later

//! How a sampled metric presents to whoever is looking at it.
//!
//! Everything here is downstream of the readings rather than part of them. A
//! [`Reading`] is what one collector produced in one pass; a [`MetricView`] is
//! what a readout shows now, which depends on how long ago that pass was.
//! [`Tracked`] holds the difference. The series a chart plots are the daemon's,
//! recorded by tick in `kori_core::history` and read with a `history` request.
//!
//! Nothing in this module derives `Serialize`, and that is the seam it was
//! split along rather than a detail: none of it crosses the socket. The daemon
//! stamps a snapshot and sends it; deciding when a retained value becomes stale
//! and when it disappears is the reader's own work, done against the reader's
//! own clock. The formatters are here for the same reason: they turn a value
//! into the characters drawn next to it, which is presentation and not
//! measurement.

use super::{Reading, Unavailable};

/// Age at which a retained value is presented as stale.
pub const STALE_AFTER_MS: u64 = 2_000;

/// Age at which a retained value is dropped rather than shown stale.
pub const DROP_AFTER_MS: u64 = 10_000;

/// How a retained metric currently presents.
#[derive(Debug, Clone, PartialEq)]
pub enum MetricView<T> {
    /// Sampled recently enough to be shown as current.
    Fresh { value: T },
    /// The last valid value, kept visible and marked as aging.
    Stale { value: T, age_ms: u64 },
    /// Nothing valid is available, and `cause` says why when one is known.
    Unavailable { cause: Option<Unavailable> },
}

impl<T> MetricView<T> {
    pub fn value(&self) -> Option<&T> {
        match self {
            Self::Fresh { value } | Self::Stale { value, .. } => Some(value),
            Self::Unavailable { .. } => None,
        }
    }

    /// The same view with its value converted, keeping the freshness intact.
    ///
    /// Freshness is a state rather than a label, so widening a duty byte into
    /// the number a readout formats must not lose the fact that it is stale.
    pub fn map<U>(&self, convert: impl FnOnce(&T) -> U) -> MetricView<U> {
        match self {
            Self::Fresh { value } => MetricView::Fresh {
                value: convert(value),
            },
            Self::Stale { value, age_ms } => MetricView::Stale {
                value: convert(value),
                age_ms: *age_ms,
            },
            Self::Unavailable { cause } => MetricView::Unavailable {
                cause: cause.clone(),
            },
        }
    }

    pub fn is_stale(&self) -> bool {
        matches!(self, Self::Stale { .. })
    }

    pub fn is_unavailable(&self) -> bool {
        matches!(self, Self::Unavailable { .. })
    }

    /// A word shown next to the value, so state never rests on color alone.
    pub fn qualifier(&self) -> Option<&'static str> {
        match self {
            Self::Fresh { .. } => None,
            Self::Stale { .. } => Some("Stale"),
            Self::Unavailable { .. } => Some("N/A"),
        }
    }

    /// The sentence explaining an unavailable metric, when there is one.
    pub fn detail(&self) -> Option<&str> {
        match self {
            Self::Fresh { .. } | Self::Stale { .. } => None,
            Self::Unavailable { cause } => cause.as_ref().map(Unavailable::detail),
        }
    }
}

impl<T: Copy> MetricView<T> {
    pub fn copied(&self) -> Option<T> {
        self.value().copied()
    }
}

/// The last valid value of one metric, with its age.
///
/// A temporary read failure does not blank a readout: the previous value stays
/// visible and ages out. [`STALE_AFTER_MS`] and [`DROP_AFTER_MS`] are the two
/// thresholds that decide when.
#[derive(Debug, Clone, PartialEq)]
pub struct Tracked<T> {
    last_valid: Option<(T, u64)>,
    cause: Option<Unavailable>,
}

// Written out rather than derived: a derived `Default` would demand
// `T: Default`, and a metric that has never been sampled has no default value
// to fall back on. That is the whole point of the type.
impl<T> Default for Tracked<T> {
    fn default() -> Self {
        Self {
            last_valid: None,
            cause: None,
        }
    }
}

impl<T: Clone> Tracked<T> {
    pub fn new() -> Self {
        Self::default()
    }

    /// Fold one reading in, stamped with the time it was sampled.
    ///
    /// A sample older than the one already held is ignored: the stamps come
    /// from a wall clock, and a clock that steps backwards must not be read as
    /// a fresher measurement. The stamp is what [`Tracked::view`] ages
    /// against, so one sample landing far enough ahead of the clock would hold
    /// the metric at [`MetricView::Fresh`] for as long as it took real time to
    /// catch up, and a frozen readout that never says `Stale` is exactly the
    /// reading this module refuses to show.
    pub fn observe(&mut self, reading: &Reading<T>, at_unix_ms: u64) {
        match reading {
            Reading::Valid { value } => {
                if self
                    .last_valid
                    .as_ref()
                    .is_some_and(|(_, at)| at_unix_ms < *at)
                {
                    return;
                }
                self.last_valid = Some((value.clone(), at_unix_ms));
                self.cause = None;
            }
            Reading::Unavailable { cause } => self.cause = Some(cause.clone()),
        }
    }

    /// How the metric presents at `now_unix_ms`.
    pub fn view(&self, now_unix_ms: u64) -> MetricView<T> {
        let Some((value, at)) = &self.last_valid else {
            return MetricView::Unavailable {
                cause: self.cause.clone(),
            };
        };
        let age_ms = now_unix_ms.saturating_sub(*at);
        if age_ms >= DROP_AFTER_MS {
            MetricView::Unavailable {
                cause: self.cause.clone(),
            }
        } else if age_ms >= STALE_AFTER_MS {
            MetricView::Stale {
                value: value.clone(),
                age_ms,
            }
        } else {
            MetricView::Fresh {
                value: value.clone(),
            }
        }
    }
}

/// Format a temperature with exactly one decimal place.
pub fn format_temperature(value: f32) -> String {
    format!("{value:.1}")
}

/// Format a byte count with an explicit binary unit.
///
/// Binary rather than decimal, and spelled out, so `GiB` can never be read as
/// `GB` by whoever compares the figure with another tool.
pub fn format_binary_bytes(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    let value = bytes as f64;
    if value >= KIB * KIB * KIB {
        format!("{:.1} GiB", value / (KIB * KIB * KIB))
    } else if value >= KIB * KIB {
        format!("{:.0} MiB", value / (KIB * KIB))
    } else if value >= KIB {
        format!("{:.0} KiB", value / KIB)
    } else {
        format!("{bytes} B")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::telemetry::Unavailable;

    fn valid(value: f32) -> Reading<f32> {
        Reading::valid(value)
    }

    fn missing() -> Reading<f32> {
        Reading::unavailable(Unavailable::unreadable("/sys/.../temp1_input: EIO"))
    }

    #[test]
    fn converting_a_view_keeps_how_old_it_is() {
        let fresh: MetricView<u8> = MetricView::Fresh { value: 180 };
        assert_eq!(fresh.map(|duty| f32::from(*duty)).copied(), Some(180.0));

        let stale: MetricView<u8> = MetricView::Stale {
            value: 180,
            age_ms: 3_000,
        };
        let widened = stale.map(|duty| f32::from(*duty));
        assert!(widened.is_stale(), "freshness is a state, not a label");
        assert_eq!(widened.copied(), Some(180.0));

        let missing: MetricView<u8> = MetricView::Unavailable {
            cause: Some(Unavailable::unreadable("pwm1")),
        };
        let widened = missing.map(|duty| f32::from(*duty));
        assert!(widened.is_unavailable());
        assert_eq!(widened.copied(), None);
    }

    #[test]
    fn a_retained_value_becomes_stale_then_disappears() {
        let mut tracked = Tracked::new();
        tracked.observe(&valid(29.8), 10_000);

        assert_eq!(tracked.view(10_500), MetricView::Fresh { value: 29.8 });

        // A read failure does not blank the readout.
        tracked.observe(&missing(), 11_000);
        assert_eq!(tracked.view(11_500), MetricView::Fresh { value: 29.8 });

        let stale = tracked.view(10_000 + STALE_AFTER_MS);
        assert!(stale.is_stale(), "{stale:?}");
        assert_eq!(stale.copied(), Some(29.8));
        assert_eq!(stale.qualifier(), Some("Stale"));

        let dropped = tracked.view(10_000 + DROP_AFTER_MS);
        assert!(dropped.is_unavailable(), "{dropped:?}");
        assert_eq!(dropped.copied(), None);
        assert!(dropped.detail().unwrap().contains("EIO"));
    }

    #[test]
    fn a_fresh_sample_clears_a_previous_failure() {
        let mut tracked = Tracked::new();
        tracked.observe(&valid(29.8), 0);
        tracked.observe(&missing(), 1_000);
        tracked.observe(&valid(31.2), 2_000);
        assert_eq!(tracked.view(2_100), MetricView::Fresh { value: 31.2 });
        assert_eq!(tracked.view(2_100).detail(), None);
    }

    #[test]
    fn a_metric_never_sampled_is_unavailable_from_the_start() {
        let tracked: Tracked<f32> = Tracked::new();
        assert!(tracked.view(0).is_unavailable());
        assert_eq!(tracked.view(0).qualifier(), Some("N/A"));
    }

    /// A sample stamped in the past must not replace a newer one, because the
    /// stamp is what the staleness thresholds are measured from.
    #[test]
    fn a_retained_metric_ignores_a_sample_that_travels_backwards_in_time() {
        let mut tracked = Tracked::new();
        tracked.observe(&valid(29.8), 10_000);
        tracked.observe(&valid(50.0), 9_000);

        assert_eq!(tracked.view(10_100), MetricView::Fresh { value: 29.8 });
        // And the older sample did not become the stamp everything ages from:
        // the metric still goes stale on the schedule the newer one set.
        assert!(tracked.view(10_000 + STALE_AFTER_MS).is_stale());
        assert!(!tracked.view(9_000 + STALE_AFTER_MS).is_stale());
    }

    #[test]
    fn temperatures_keep_exactly_one_decimal() {
        assert_eq!(format_temperature(46.75), "46.8");
        assert_eq!(format_temperature(29.0), "29.0");
    }

    #[test]
    fn memory_is_reported_in_explicit_binary_units() {
        assert_eq!(format_binary_bytes(0), "0 B");
        assert_eq!(format_binary_bytes(2048), "2 KiB");
        assert_eq!(format_binary_bytes(12 * 1024 * 1024), "12 MiB");
        assert_eq!(format_binary_bytes(32 * 1024 * 1024 * 1024), "32.0 GiB");
    }
}

// SPDX-FileCopyrightText: 2026 Arthur Jean
// SPDX-License-Identifier: GPL-3.0-or-later

//! The telemetry history the daemon keeps, as the protocol describes it.
//!
//! Two fixed tiers in memory: 900 one-second slots, and 1,440 one-minute slots
//! folded from them. Both are indexed by tick, never pruned by comparing clock
//! readings, so a wall clock that steps back an hour or forward a day drops no
//! slot and reorders none. Nothing here reaches disk: the history restarts
//! empty with the daemon.
//!
//! This module is the vocabulary and the rules a request is held to. The rings
//! themselves live in the daemon, which is the only process that records.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::telemetry::TelemetrySnapshot;

/// One-second slots kept: fifteen minutes.
pub const SECOND_SLOTS: usize = 900;

/// One-minute slots kept: twenty-four hours.
pub const MINUTE_SLOTS: usize = 1_440;

/// Second slots folded into one minute slot.
pub const SLOTS_PER_MINUTE: usize = 60;

/// The longest window the second tier can answer, in seconds.
pub const SECOND_TIER_WINDOW_S: u32 = SECOND_SLOTS as u32;

/// The longest window the minute tier can answer, in seconds.
pub const MINUTE_TIER_WINDOW_S: u32 = (MINUTE_SLOTS * SLOTS_PER_MINUTE) as u32;

/// Most points one reply may carry per metric.
pub const MAX_POINTS: u32 = 1_500;

/// Most numbers one reply may carry in all its point arrays together.
///
/// Sized so the largest reply stays inside the protocol's 256 KiB frame with
/// room to spare, whatever the readings are.
pub const MAX_VALUES: u64 = 20_000;

/// Window a request gets when it names none: the whole second tier.
pub const DEFAULT_WINDOW_S: u32 = SECOND_TIER_WINDOW_S;

/// Every series the daemon records, in the order a reply lists them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum HistoryMetric {
    /// CPU utilization, in percent.
    CpuLoad,
    /// CPU package temperature, in degrees Celsius.
    CpuTemperature,
    /// GPU utilization, in percent.
    GpuLoad,
    /// GPU core temperature, in degrees Celsius.
    GpuTemperature,
    /// Memory in use, in mebibytes.
    MemoryUsed,
    /// Coolant temperature, in degrees Celsius.
    LiquidTemperature,
    /// Pump speed, in RPM.
    PumpSpeed,
    /// Fan speed, in RPM.
    FanSpeed,
    /// Pump duty, in percent of full scale.
    PumpDuty,
    /// Fan duty, in percent of full scale.
    FanDuty,
}

/// The section of a snapshot a metric is read from.
///
/// Each section carries its own timestamp, and a slot holds a reading only when
/// that section produced a new sample since the previous tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Section {
    Kraken,
    System,
    Gpu,
}

impl HistoryMetric {
    pub const ALL: [Self; 10] = [
        Self::CpuLoad,
        Self::CpuTemperature,
        Self::GpuLoad,
        Self::GpuTemperature,
        Self::MemoryUsed,
        Self::LiquidTemperature,
        Self::PumpSpeed,
        Self::FanSpeed,
        Self::PumpDuty,
        Self::FanDuty,
    ];

    /// Position in [`Self::ALL`], which is the column the daemon stores it in.
    pub fn index(self) -> usize {
        self as usize
    }

    /// The unit every number of this series is in.
    pub fn unit(self) -> &'static str {
        match self {
            Self::CpuLoad | Self::GpuLoad | Self::PumpDuty | Self::FanDuty => "percent",
            Self::CpuTemperature | Self::GpuTemperature | Self::LiquidTemperature => "celsius",
            Self::MemoryUsed => "mebibytes",
            Self::PumpSpeed | Self::FanSpeed => "rpm",
        }
    }

    pub fn section(self) -> Section {
        match self {
            Self::CpuLoad | Self::CpuTemperature | Self::MemoryUsed => Section::System,
            Self::GpuLoad | Self::GpuTemperature => Section::Gpu,
            Self::LiquidTemperature
            | Self::PumpSpeed
            | Self::FanSpeed
            | Self::PumpDuty
            | Self::FanDuty => Section::Kraken,
        }
    }

    /// The reading this metric takes from `snapshot`, when it is valid.
    ///
    /// Memory is recorded in mebibytes rather than bytes so every value fits
    /// an `f32` exactly: a byte count of a 64 GiB machine does not.
    pub fn read(self, snapshot: &TelemetrySnapshot) -> Option<f32> {
        let kraken = &snapshot.kraken;
        let value = match self {
            Self::CpuLoad => snapshot.system.cpu_load_percent.copied(),
            Self::CpuTemperature => snapshot.system.cpu_temperature_c.copied(),
            Self::GpuLoad => snapshot.gpu.load_percent.copied(),
            Self::GpuTemperature => snapshot.gpu.temperature_c.copied(),
            Self::MemoryUsed => snapshot
                .system
                .memory
                .copied()
                .map(|memory| (memory.used_bytes / (1024 * 1024)) as f32),
            Self::LiquidTemperature => kraken.liquid_temperature_c.copied(),
            Self::PumpSpeed => kraken.pump.rpm.copied().map(|rpm| rpm as f32),
            Self::FanSpeed => kraken.fan.rpm.copied().map(|rpm| rpm as f32),
            Self::PumpDuty => kraken.pump.duty_percent(),
            Self::FanDuty => kraken.fan.duty_percent(),
        };
        value.filter(|value| value.is_finite())
    }
}

/// Which tier a resolved request reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    Second,
    Minute,
}

/// A request checked against every limit, with its defaults filled in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryPlan {
    pub metrics: Vec<HistoryMetric>,
    pub window_s: u32,
    pub step_s: u32,
    /// Points per metric: `window_s / step_s`.
    pub slots: u32,
    pub tier: Tier,
    pub points: bool,
}

impl HistoryPlan {
    /// Tier slots each point folds together.
    pub fn slots_per_point(&self) -> usize {
        match self.tier {
            Tier::Second => self.step_s as usize,
            Tier::Minute => self.step_s as usize / SLOTS_PER_MINUTE,
        }
    }

    /// Whether the reply carries `min` and `max` arrays beside `avg`.
    pub fn carries_extremes(&self) -> bool {
        self.step_s > 1
    }

    /// Check a request against every limit, defaults filled in.
    ///
    /// The limit a refusal names is the first one crossed, in the order they
    /// are listed on [`HistoryLimit`].
    pub fn resolve(
        metrics: Option<&[HistoryMetric]>,
        window_s: Option<u32>,
        step_s: Option<u32>,
        points: Option<bool>,
    ) -> Result<Self, HistoryRefusal> {
        let metrics: Vec<HistoryMetric> = match metrics {
            None => HistoryMetric::ALL.to_vec(),
            Some([]) => {
                return Err(HistoryRefusal::new(
                    HistoryLimit::Metrics,
                    "metrics is empty; leave it out to ask for every metric",
                ));
            }
            Some(listed) => {
                let mut unique = listed.to_vec();
                unique.sort();
                unique.dedup();
                unique
            }
        };
        let window_s = window_s.unwrap_or(DEFAULT_WINDOW_S);
        let step_s = step_s.unwrap_or(if window_s <= SECOND_TIER_WINDOW_S {
            1
        } else {
            SLOTS_PER_MINUTE as u32
        });
        if window_s == 0 {
            return Err(HistoryRefusal::new(
                HistoryLimit::Window,
                "window_s must be at least one second",
            ));
        }
        if step_s == 0 {
            return Err(HistoryRefusal::new(
                HistoryLimit::Step,
                "step_s must be at least one second",
            ));
        }
        if !window_s.is_multiple_of(step_s) {
            return Err(HistoryRefusal::new(
                HistoryLimit::WindowMultipleOfStep,
                format!("window_s {window_s} is not a multiple of step_s {step_s}"),
            ));
        }
        let tier = if step_s < SLOTS_PER_MINUTE as u32 {
            if window_s > SECOND_TIER_WINDOW_S {
                return Err(HistoryRefusal::new(
                    HistoryLimit::SecondTierWindow,
                    format!(
                        "a step under 60 s reads the second tier, which holds \
                         {SECOND_TIER_WINDOW_S} s; window_s is {window_s}"
                    ),
                ));
            }
            Tier::Second
        } else if step_s.is_multiple_of(SLOTS_PER_MINUTE as u32) {
            if window_s > MINUTE_TIER_WINDOW_S {
                return Err(HistoryRefusal::new(
                    HistoryLimit::MinuteTierWindow,
                    format!(
                        "a step in whole minutes reads the minute tier, which holds \
                         {MINUTE_TIER_WINDOW_S} s; window_s is {window_s}"
                    ),
                ));
            }
            Tier::Minute
        } else {
            return Err(HistoryRefusal::new(
                HistoryLimit::Step,
                format!("step_s {step_s} is neither under 60 nor a whole number of minutes"),
            ));
        };
        // Unreachable with today's tiers, which hold 900 and 1,440 slots, so
        // no window they accept can ask for more. Kept because it is the bound
        // a reply's length is sized against, and a third tier must not be able
        // to slip past it.
        let slots = window_s / step_s;
        if slots > MAX_POINTS {
            return Err(HistoryRefusal::new(
                HistoryLimit::MaxSlots,
                format!("{slots} points per metric; the limit is {MAX_POINTS}"),
            ));
        }
        let points = points.unwrap_or(false);
        let plan = Self {
            metrics,
            window_s,
            step_s,
            slots,
            tier,
            points,
        };
        let values = plan.value_count();
        if values > MAX_VALUES {
            return Err(HistoryRefusal::new(
                HistoryLimit::MaxValues,
                format!("{values} values in all; the limit is {MAX_VALUES}"),
            ));
        }
        Ok(plan)
    }

    /// Numbers the point arrays of the reply carry, zero without points.
    pub fn value_count(&self) -> u64 {
        if !self.points {
            return 0;
        }
        let arrays = if self.carries_extremes() { 3 } else { 1 };
        self.metrics.len() as u64 * u64::from(self.slots) * arrays
    }
}

/// Which limit a history request crossed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum HistoryLimit {
    /// `metrics` was given and empty.
    Metrics,
    /// `window_s` was zero.
    Window,
    /// `step_s` was zero, or between one minute and two without being whole
    /// minutes.
    Step,
    WindowMultipleOfStep,
    /// A step under 60 s with a window past the second tier.
    SecondTierWindow,
    /// A step in whole minutes with a window past the minute tier.
    MinuteTierWindow,
    /// More than [`MAX_POINTS`] points per metric.
    MaxSlots,
    /// More than [`MAX_VALUES`] numbers in all.
    MaxValues,
}

/// Why a history request was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryRefusal {
    pub limit: HistoryLimit,
    pub detail: String,
}

impl HistoryRefusal {
    fn new(limit: HistoryLimit, detail: impl Into<String>) -> Self {
        Self {
            limit,
            detail: detail.into(),
        }
    }
}

/// What a window held for one metric, computed by the daemon.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Summary {
    /// Smallest reading in the window, in the metric's unit (`unit`).
    pub min: f32,
    /// Mean of the readings in the window, in the metric's unit.
    pub avg: f32,
    /// Largest reading in the window, in the metric's unit.
    pub max: f32,
    /// The newest value in the window: the newest reading for the second
    /// tier, the newest minute's average for the minute tier.
    pub last: f32,
    /// Readings the window is made of.
    pub samples: u32,
}

/// One metric of a history reply.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct MetricHistory {
    /// The unit every number of this metric is in: `percent`, `celsius`,
    /// `mebibytes` or `rpm`.
    pub unit: String,
    /// `None` when the window holds no reading at all.
    pub summary: Option<Summary>,
    /// One number or null per point, oldest first, when points were asked for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub avg: Option<Vec<Option<f32>>>,
    /// The smallest reading behind each point, when a point folds several.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min: Option<Vec<Option<f32>>>,
    /// The largest reading behind each point, when a point folds several.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max: Option<Vec<Option<f32>>>,
}

/// The answer to a `history` request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct HistoryReply {
    /// Wall-clock time of the newest slot of the tier read. It follows the
    /// clock, so after a clock step the older slots' times are approximate:
    /// a slot is one tick, not one timestamp.
    /// In milliseconds since the Unix epoch.
    pub end_unix_ms: u64,
    /// Seconds each point covers.
    pub step_s: u32,
    /// Points per metric.
    pub slots: u32,
    /// Seconds of history the tier read actually holds, at most its capacity.
    /// Slots older than this are null.
    pub available_s: u32,
    pub metrics: BTreeMap<HistoryMetric, MetricHistory>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resolve(
        window: Option<u32>,
        step: Option<u32>,
        points: bool,
    ) -> Result<HistoryPlan, HistoryLimit> {
        HistoryPlan::resolve(None, window, step, Some(points)).map_err(|refusal| refusal.limit)
    }

    #[test]
    fn defaults_read_fifteen_minutes_at_one_second() {
        let plan = resolve(None, None, false).unwrap();
        assert_eq!((plan.window_s, plan.step_s, plan.slots), (900, 1, 900));
        assert_eq!(plan.tier, Tier::Second);
        assert_eq!(plan.metrics.len(), 10);

        let plan = resolve(Some(3_600), None, false).unwrap();
        assert_eq!((plan.step_s, plan.slots, plan.tier), (60, 60, Tier::Minute));
    }

    #[test]
    fn each_limit_is_named_when_crossed() {
        assert_eq!(
            resolve(Some(900), Some(7), false),
            Err(HistoryLimit::WindowMultipleOfStep)
        );
        assert_eq!(
            resolve(Some(1_800), Some(30), false),
            Err(HistoryLimit::SecondTierWindow)
        );
        assert_eq!(
            resolve(Some(172_800), Some(120), false),
            Err(HistoryLimit::MinuteTierWindow)
        );
        assert_eq!(resolve(Some(900), Some(90), false), Err(HistoryLimit::Step));
        assert_eq!(resolve(Some(900), Some(0), false), Err(HistoryLimit::Step));
        assert_eq!(resolve(Some(0), Some(1), false), Err(HistoryLimit::Window));
        // 86,400 s at one minute is 1,440 points: inside the slot limit, but
        // ten metrics with three arrays each is 43,200 values.
        assert!(resolve(Some(86_400), Some(60), false).is_ok());
        assert_eq!(
            resolve(Some(86_400), Some(60), true),
            Err(HistoryLimit::MaxValues)
        );
        assert_eq!(
            HistoryPlan::resolve(Some(&[]), None, None, None).map_err(|r| r.limit),
            Err(HistoryLimit::Metrics)
        );
    }

    #[test]
    fn a_metric_named_twice_is_answered_once() {
        let plan = HistoryPlan::resolve(
            Some(&[
                HistoryMetric::FanSpeed,
                HistoryMetric::CpuLoad,
                HistoryMetric::FanSpeed,
            ]),
            None,
            None,
            None,
        )
        .unwrap();
        assert_eq!(
            plan.metrics,
            vec![HistoryMetric::CpuLoad, HistoryMetric::FanSpeed]
        );
    }

    #[test]
    fn the_metric_list_is_the_declared_order_and_every_unit_is_named() {
        for (index, metric) in HistoryMetric::ALL.iter().enumerate() {
            assert_eq!(metric.index(), index);
            assert!(!metric.unit().is_empty());
        }
        let encoded = serde_json::to_value(HistoryMetric::LiquidTemperature).unwrap();
        assert_eq!(encoded, "liquid_temperature");
    }
}

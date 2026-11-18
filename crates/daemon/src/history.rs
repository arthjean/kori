// SPDX-FileCopyrightText: 2026 Arthur Jean
// SPDX-License-Identifier: GPL-3.0-or-later

//! The two rings the telemetry history lives in.
//!
//! One slot per tick of the daemon's own clock, written by [`History::record`]
//! from the snapshot the tick already took. A ring holds a fixed number of
//! slots and the newest overwrites the oldest, so retention is a count and
//! never a comparison of timestamps: a wall clock that steps back an hour drops
//! nothing and reorders nothing, it only moves the `end_unix_ms` of the next
//! reply. The wire types and the limits a request is held to are in
//! [`kori_core::history`].
//!
//! Nothing here touches the disk. The history restarts empty with the daemon,
//! which is said in `docs/` rather than discovered.

use std::collections::{BTreeMap, VecDeque};

use kori_core::history::{
    HistoryMetric, HistoryPlan, HistoryReply, MINUTE_SLOTS, MetricHistory, SECOND_SLOTS,
    SLOTS_PER_MINUTE, Section, Summary, Tier,
};
use kori_core::telemetry::TelemetrySnapshot;

const METRICS: usize = HistoryMetric::ALL.len();

/// One second of every metric.
type SecondSlot = [Option<f32>; METRICS];

/// One minute of one metric, folded from up to sixty seconds.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Folded {
    min: f32,
    avg: f32,
    max: f32,
    samples: u16,
}

type MinuteSlot = [Option<Folded>; METRICS];

/// A fixed-capacity ring, newest at the back.
#[derive(Debug, Clone)]
struct Ring<T> {
    slots: VecDeque<T>,
    capacity: usize,
}

impl<T> Ring<T> {
    fn new(capacity: usize) -> Self {
        Self {
            slots: VecDeque::with_capacity(capacity),
            capacity,
        }
    }

    fn push(&mut self, slot: T) {
        if self.slots.len() == self.capacity {
            self.slots.pop_front();
        }
        self.slots.push_back(slot);
    }

    fn len(&self) -> usize {
        self.slots.len()
    }

    /// The newest `count` slots, oldest first, each `None` where the ring
    /// does not reach that far back.
    fn newest(&self, count: usize) -> impl Iterator<Item = Option<&T>> {
        let missing = count.saturating_sub(self.slots.len());
        let skip = self.slots.len().saturating_sub(count);
        std::iter::repeat_n(None, missing).chain(self.slots.iter().skip(skip).map(Some))
    }
}

/// Section timestamps seen at the previous tick.
#[derive(Debug, Clone, Copy, Default)]
struct Seen {
    kraken: Option<u64>,
    system: Option<u64>,
    gpu: Option<u64>,
}

impl Seen {
    fn slot(&mut self, section: Section) -> &mut Option<u64> {
        match section {
            Section::Kraken => &mut self.kraken,
            Section::System => &mut self.system,
            Section::Gpu => &mut self.gpu,
        }
    }
}

/// The telemetry history, both tiers.
#[derive(Debug, Clone)]
pub struct History {
    seconds: Ring<SecondSlot>,
    minutes: Ring<MinuteSlot>,
    /// Ticks recorded since the daemon started, gaps included.
    ticks: u64,
    /// Wall-clock time of the newest second slot.
    second_end_unix_ms: Option<u64>,
    /// Wall-clock time of the newest minute slot.
    minute_end_unix_ms: Option<u64>,
    seen: Seen,
}

impl Default for History {
    fn default() -> Self {
        Self::new()
    }
}

impl History {
    pub fn new() -> Self {
        Self {
            seconds: Ring::new(SECOND_SLOTS),
            minutes: Ring::new(MINUTE_SLOTS),
            ticks: 0,
            second_end_unix_ms: None,
            minute_end_unix_ms: None,
            seen: Seen::default(),
        }
    }

    /// Record one tick from the snapshot it took.
    ///
    /// A metric's slot holds its reading when the section it comes from
    /// produced a new sample since the previous tick and the reading is valid,
    /// and null otherwise: a collector that wedged must not be recorded as a
    /// steady line, and polling faster than the sampler must not record one
    /// sample twice. "New" is a timestamp that changed, not one that grew,
    /// because a clock stepped back makes every new sample look older than the
    /// last and would otherwise blank the history for as long as the step.
    pub fn record(&mut self, snapshot: &TelemetrySnapshot, now_unix_ms: u64) {
        let mut fresh = [false; 3];
        for (index, (section, at)) in [
            (Section::Kraken, snapshot.kraken.at_unix_ms),
            (Section::System, snapshot.system.at_unix_ms),
            (Section::Gpu, snapshot.gpu.at_unix_ms),
        ]
        .into_iter()
        .enumerate()
        {
            let last = self.seen.slot(section);
            if let Some(index) = fresh.get_mut(index) {
                *index = *last != Some(at);
            }
            *last = Some(at);
        }
        let is_fresh = |section: Section| match section {
            Section::Kraken => fresh[0],
            Section::System => fresh[1],
            Section::Gpu => fresh[2],
        };

        let mut slot: SecondSlot = [None; METRICS];
        for metric in HistoryMetric::ALL {
            if is_fresh(metric.section())
                && let Some(entry) = slot.get_mut(metric.index())
            {
                *entry = metric.read(snapshot);
            }
        }
        self.push(slot, now_unix_ms);
    }

    /// Record `count` ticks the clock skipped, as null slots.
    ///
    /// A tick that ran late does not catch up on readings, but it must not
    /// shorten the history either: each slot stands for one second, and a
    /// window of 900 slots has to mean fifteen minutes.
    pub fn record_gap(&mut self, count: u32, now_unix_ms: u64) {
        for _ in 0..count {
            self.push([None; METRICS], now_unix_ms);
        }
    }

    fn push(&mut self, slot: SecondSlot, now_unix_ms: u64) {
        self.seconds.push(slot);
        self.ticks += 1;
        self.second_end_unix_ms = Some(now_unix_ms);
        if self.ticks.is_multiple_of(SLOTS_PER_MINUTE as u64) {
            self.fold(now_unix_ms);
        }
    }

    /// Fold the newest sixty second slots into one minute slot.
    fn fold(&mut self, now_unix_ms: u64) {
        let mut minute: MinuteSlot = [None; METRICS];
        for metric in HistoryMetric::ALL {
            let column = metric.index();
            let values = self
                .seconds
                .newest(SLOTS_PER_MINUTE)
                .filter_map(|slot| slot.and_then(|slot| slot.get(column).copied().flatten()));
            if let (Some(stats), Some(entry)) = (Stats::of(values), minute.get_mut(column)) {
                *entry = Some(Folded {
                    min: stats.min,
                    avg: stats.avg(),
                    max: stats.max,
                    samples: stats.samples as u16,
                });
            }
        }
        self.minutes.push(minute);
        self.minute_end_unix_ms = Some(now_unix_ms);
    }

    /// Answer a request already checked against every limit.
    pub fn answer(&self, plan: &HistoryPlan, now_unix_ms: u64) -> HistoryReply {
        let mut metrics = BTreeMap::new();
        for metric in &plan.metrics {
            let history = match plan.tier {
                Tier::Second => self.second_series(*metric, plan),
                Tier::Minute => self.minute_series(*metric, plan),
            };
            metrics.insert(*metric, history);
        }
        let (end, available_s) = match plan.tier {
            Tier::Second => (self.second_end_unix_ms, self.seconds.len()),
            Tier::Minute => (
                self.minute_end_unix_ms,
                self.minutes.len() * SLOTS_PER_MINUTE,
            ),
        };
        HistoryReply {
            end_unix_ms: end.unwrap_or(now_unix_ms),
            step_s: plan.step_s,
            slots: plan.slots,
            available_s: available_s as u32,
            metrics,
        }
    }

    fn second_series(&self, metric: HistoryMetric, plan: &HistoryPlan) -> MetricHistory {
        let column = metric.index();
        let values: Vec<Option<f32>> = self
            .seconds
            .newest(plan.window_s as usize)
            .map(|slot| slot.and_then(|slot| slot.get(column).copied().flatten()))
            .collect();

        let summary = Stats::of(values.iter().flatten().copied()).map(|stats| Summary {
            min: stats.min,
            avg: stats.avg(),
            max: stats.max,
            last: values
                .iter()
                .rev()
                .flatten()
                .next()
                .copied()
                .unwrap_or(stats.max),
            samples: stats.samples,
        });

        let mut history = MetricHistory {
            unit: metric.unit().to_string(),
            summary,
            avg: None,
            min: None,
            max: None,
        };
        if plan.points {
            let points: Vec<Option<Stats>> = values
                .chunks(plan.slots_per_point())
                .map(|chunk| Stats::of(chunk.iter().flatten().copied()))
                .collect();
            fill_points(&mut history, &points, plan.carries_extremes());
        }
        history
    }

    fn minute_series(&self, metric: HistoryMetric, plan: &HistoryPlan) -> MetricHistory {
        let column = metric.index();
        let minutes = plan.window_s as usize / SLOTS_PER_MINUTE;
        let folded: Vec<Option<Folded>> = self
            .minutes
            .newest(minutes)
            .map(|slot| slot.and_then(|slot| slot.get(column).copied().flatten()))
            .collect();

        let summary = Stats::merge(folded.iter().flatten()).map(|stats| Summary {
            min: stats.min,
            avg: stats.avg(),
            max: stats.max,
            last: folded
                .iter()
                .rev()
                .flatten()
                .next()
                .map_or(stats.max, |minute| minute.avg),
            samples: stats.samples,
        });

        let mut history = MetricHistory {
            unit: metric.unit().to_string(),
            summary,
            avg: None,
            min: None,
            max: None,
        };
        if plan.points {
            let points: Vec<Option<Stats>> = folded
                .chunks(plan.slots_per_point())
                .map(|chunk| Stats::merge(chunk.iter().flatten()))
                .collect();
            fill_points(&mut history, &points, plan.carries_extremes());
        }
        history
    }
}

/// The point arrays of one metric.
fn fill_points(history: &mut MetricHistory, points: &[Option<Stats>], extremes: bool) {
    history.avg = Some(points.iter().map(|p| p.map(|s| s.avg())).collect());
    if extremes {
        history.min = Some(points.iter().map(|p| p.map(|s| s.min)).collect());
        history.max = Some(points.iter().map(|p| p.map(|s| s.max)).collect());
    }
}

/// Minimum, maximum, sum and count of a run of readings.
#[derive(Debug, Clone, Copy)]
struct Stats {
    min: f32,
    max: f32,
    /// In `f64`, so 86,400 readings sum without losing the last digits.
    sum: f64,
    samples: u32,
}

impl Stats {
    fn of(values: impl Iterator<Item = f32>) -> Option<Self> {
        let mut stats: Option<Self> = None;
        for value in values {
            let entry = stats.get_or_insert(Self {
                min: value,
                max: value,
                sum: 0.0,
                samples: 0,
            });
            entry.min = entry.min.min(value);
            entry.max = entry.max.max(value);
            entry.sum += f64::from(value);
            entry.samples += 1;
        }
        stats
    }

    /// Fold minute slots together, weighting each average by its samples.
    fn merge<'a>(minutes: impl Iterator<Item = &'a Folded>) -> Option<Self> {
        let mut stats: Option<Self> = None;
        for minute in minutes {
            let entry = stats.get_or_insert(Self {
                min: minute.min,
                max: minute.max,
                sum: 0.0,
                samples: 0,
            });
            entry.min = entry.min.min(minute.min);
            entry.max = entry.max.max(minute.max);
            entry.sum += f64::from(minute.avg) * f64::from(minute.samples);
            entry.samples += u32::from(minute.samples);
        }
        stats
    }

    fn avg(&self) -> f32 {
        (self.sum / f64::from(self.samples.max(1))) as f32
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kori_core::history::{HistoryPlan, MAX_VALUES};
    use kori_core::ipc::MAX_FRAME_BYTES;
    use kori_core::telemetry::{
        GpuTelemetry, KrakenTelemetry, Reading, SystemTelemetry, TelemetrySnapshot, Unavailable,
    };

    /// A snapshot whose every section is stamped `at` and reads `value`.
    fn snapshot(at: u64, value: f32) -> TelemetrySnapshot {
        let mut snapshot = TelemetrySnapshot::unavailable(at, Unavailable::absent("test"));
        snapshot.system = SystemTelemetry {
            at_unix_ms: at,
            cpu_load_percent: Reading::valid(value),
            cpu_temperature_c: Reading::valid(value),
            memory: Reading::unavailable(Unavailable::absent("test")),
        };
        snapshot.gpu = GpuTelemetry::unavailable(at, Unavailable::absent("test"));
        snapshot.kraken = KrakenTelemetry::absent(at, Unavailable::absent("test"));
        snapshot.kraken.present = true;
        snapshot.kraken.liquid_temperature_c = Reading::valid(value);
        snapshot
    }

    fn plan(window: u32, step: u32, points: bool) -> HistoryPlan {
        HistoryPlan::resolve(None, Some(window), Some(step), Some(points)).unwrap()
    }

    fn avg(reply: &HistoryReply, metric: HistoryMetric) -> Vec<Option<f32>> {
        reply.metrics[&metric].avg.clone().unwrap()
    }

    #[test]
    fn a_young_history_reports_its_real_depth_and_null_older_slots() {
        let mut history = History::new();
        for tick in 0..40u64 {
            history.record(
                &snapshot(1_000 + tick * 1_000, tick as f32),
                1_000 + tick * 1_000,
            );
        }
        let reply = history.answer(&plan(900, 1, true), 0);
        assert_eq!(reply.available_s, 40);
        assert_eq!(reply.slots, 900);
        assert_eq!(reply.end_unix_ms, 40_000);
        let points = avg(&reply, HistoryMetric::CpuLoad);
        assert_eq!(points.len(), 900);
        assert!(points[..860].iter().all(Option::is_none));
        assert_eq!(points[899], Some(39.0));
        let summary = reply.metrics[&HistoryMetric::CpuLoad].summary.unwrap();
        assert_eq!((summary.min, summary.max, summary.last), (0.0, 39.0, 39.0));
        assert_eq!(summary.samples, 40);
        assert!(reply.metrics[&HistoryMetric::GpuLoad].summary.is_none());
    }

    #[test]
    fn a_section_that_did_not_advance_records_null() {
        let mut history = History::new();
        history.record(&snapshot(5_000, 1.0), 5_000);
        // The same sample again: the collector did not run between ticks.
        history.record(&snapshot(5_000, 1.0), 6_000);
        history.record(&snapshot(7_000, 2.0), 7_000);
        let points = avg(
            &history.answer(&plan(3, 1, true), 0),
            HistoryMetric::CpuLoad,
        );
        assert_eq!(points, vec![Some(1.0), None, Some(2.0)]);
    }

    #[test]
    fn a_wall_clock_step_back_drops_no_slot() {
        let mut history = History::new();
        let hour = 3_600_000;
        let start = 10 * hour;
        for tick in 0..100u64 {
            // Halfway through, the clock steps back one hour.
            let at = if tick < 50 {
                start + tick * 1_000
            } else {
                start - hour + tick * 1_000
            };
            history.record(&snapshot(at, tick as f32), at);
        }
        let reply = history.answer(&plan(100, 1, true), 0);
        assert_eq!(reply.available_s, 100);
        let points = avg(&reply, HistoryMetric::CpuLoad);
        assert!(
            points.iter().all(Option::is_some),
            "every tick after the step still holds its reading"
        );
        assert_eq!(points[50], Some(50.0));
        assert_eq!(reply.end_unix_ms, start - hour + 99_000);
    }

    #[test]
    fn an_unplugged_kraken_leaves_null_kraken_slots_and_the_rest_intact() {
        let mut history = History::new();
        for tick in 0..30u64 {
            let at = 1_000 + tick * 1_000;
            let mut gone = snapshot(at, 5.0);
            gone.kraken = KrakenTelemetry::absent(at, Unavailable::no_device("unplugged"));
            history.record(&gone, at);
        }
        let reply = history.answer(&plan(30, 1, true), 0);
        for metric in HistoryMetric::ALL {
            let points = avg(&reply, metric);
            match metric.section() {
                Section::Kraken => assert!(points.iter().all(Option::is_none), "{metric:?}"),
                Section::System if metric != HistoryMetric::MemoryUsed => {
                    assert!(points.iter().all(Option::is_some), "{metric:?}")
                }
                _ => {}
            }
        }
    }

    #[test]
    fn every_sixty_ticks_fold_into_one_minute() {
        let mut history = History::new();
        for tick in 0..180u64 {
            let at = 1_000 + tick * 1_000;
            // Minute 0 reads 0..59, minute 1 reads 60..119, minute 2 nothing.
            if tick < 120 {
                history.record(&snapshot(at, tick as f32), at);
            } else {
                history.record_gap(1, at);
            }
        }
        let reply = history.answer(&plan(180, 60, true), 0);
        assert_eq!(reply.available_s, 180);
        assert_eq!(
            avg(&reply, HistoryMetric::CpuLoad),
            vec![Some(29.5), Some(89.5), None]
        );
        let metric = &reply.metrics[&HistoryMetric::CpuLoad];
        assert_eq!(
            metric.min.clone().unwrap(),
            vec![Some(0.0), Some(60.0), None]
        );
        assert_eq!(
            metric.max.clone().unwrap(),
            vec![Some(59.0), Some(119.0), None]
        );
        let summary = metric.summary.unwrap();
        assert_eq!(
            (summary.min, summary.max, summary.samples),
            (0.0, 119.0, 120)
        );
        assert_eq!(summary.avg, 59.5);
        assert_eq!(summary.last, 89.5);

        // Two minutes per point, weighted by samples.
        let reply = history.answer(&plan(240, 120, true), 0);
        assert_eq!(
            avg(&reply, HistoryMetric::CpuLoad),
            vec![Some(29.5), Some(89.5)]
        );
    }

    #[test]
    fn a_coarser_second_step_folds_its_slots() {
        let mut history = History::new();
        for tick in 0..10u64 {
            history.record(&snapshot(tick + 1, tick as f32), tick + 1);
        }
        let reply = history.answer(&plan(10, 5, true), 0);
        let metric = &reply.metrics[&HistoryMetric::CpuLoad];
        assert_eq!(metric.avg.clone().unwrap(), vec![Some(2.0), Some(7.0)]);
        assert_eq!(metric.min.clone().unwrap(), vec![Some(0.0), Some(5.0)]);
        assert_eq!(metric.max.clone().unwrap(), vec![Some(4.0), Some(9.0)]);
    }

    #[test]
    fn both_tiers_hold_their_capacity_and_no_more() {
        let mut history = History::new();
        let ticks = (MINUTE_SLOTS * SLOTS_PER_MINUTE + 120) as u64;
        for tick in 0..ticks {
            history.record_gap(1, tick);
        }
        assert_eq!(history.seconds.len(), SECOND_SLOTS);
        assert_eq!(history.minutes.len(), MINUTE_SLOTS);
    }

    /// The history budget is 1 MiB for both tiers.
    #[test]
    fn the_rings_fit_the_memory_budget() {
        let bytes = SECOND_SLOTS * std::mem::size_of::<SecondSlot>()
            + MINUTE_SLOTS * std::mem::size_of::<MinuteSlot>();
        assert!(bytes <= 1024 * 1024, "{bytes} bytes");
    }

    /// The largest reply the limits allow still fits one frame, with readings
    /// chosen for the longest text an `f32` prints as.
    #[test]
    fn the_largest_reply_fits_a_frame() {
        let mut history = History::new();
        for tick in 0..SECOND_SLOTS as u64 {
            history.record(
                &snapshot(tick + 1, -1234.5677 - tick as f32 * 1e-3),
                tick + 1,
            );
        }
        for metrics in 1..=HistoryMetric::ALL.len() {
            for step in [1u32, 2, 3, 5] {
                let Ok(plan) = HistoryPlan::resolve(
                    Some(&HistoryMetric::ALL[..metrics]),
                    Some(900),
                    Some(step),
                    Some(true),
                ) else {
                    continue;
                };
                assert!(plan.value_count() <= MAX_VALUES);
                let reply = history.answer(&plan, 0);
                let encoded =
                    serde_json::to_vec(&kori_core::ipc::Response::History(Box::new(reply)))
                        .unwrap();
                assert!(
                    encoded.len() < MAX_FRAME_BYTES,
                    "{metrics} metrics at {step} s: {} bytes",
                    encoded.len()
                );
            }
        }
    }
}

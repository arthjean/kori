// SPDX-FileCopyrightText: 2026 Arthur Jean
// SPDX-License-Identifier: GPL-3.0-or-later

//! The client's view of the current value of every metric.
//!
//! The daemon pushes one snapshot at a time. A readout needs one thing a
//! snapshot does not carry: the last valid value, so a momentary read failure
//! does not blank it. That is all this holds. The recent series live in the
//! daemon, which records them whether or not a window is open, and the chart
//! asks for them with `history`: a window that kept its own copy started empty
//! every time it opened and duplicated what the daemon already knows.

use kori_core::profile::Channel;
use kori_core::telemetry::{
    KrakenTelemetry, MemoryUsage, PwmMode, Reading, TelemetrySnapshot, Tracked, Unavailable,
};

/// The readings of one cooling channel.
#[derive(Debug, Clone, Default)]
pub struct ChannelMetrics {
    pub rpm: Tracked<f32>,
    pub duty: Tracked<u8>,
    pub mode: Tracked<PwmMode>,
}

/// Every metric the interface shows.
#[derive(Debug, Clone, Default)]
pub struct MetricBook {
    pub cpu_load: Tracked<f32>,
    pub cpu_temperature: Tracked<f32>,
    pub gpu_load: Tracked<f32>,
    pub gpu_temperature: Tracked<f32>,
    pub memory_percent: Tracked<f32>,
    pub memory: Tracked<MemoryUsage>,
    pub liquid: Tracked<f32>,
    pub pump: ChannelMetrics,
    pub fan: ChannelMetrics,
}

impl MetricBook {
    pub fn new() -> Self {
        Self::default()
    }

    /// Fold one snapshot in.
    ///
    /// Each section ages on its own timestamp: a wedged GPU collector must not
    /// be kept looking fresh by a Kraken that is still answering.
    pub fn observe(&mut self, snapshot: &TelemetrySnapshot) {
        self.observe_kraken(&snapshot.kraken);

        let system_at = snapshot.system.at_unix_ms;
        self.cpu_load
            .observe(&snapshot.system.cpu_load_percent, system_at);
        self.cpu_temperature
            .observe(&snapshot.system.cpu_temperature_c, system_at);
        self.memory.observe(&snapshot.system.memory, system_at);
        // Occupancy is derived, so it can be missing for a reason the memory
        // reading itself does not carry: a total of zero is a figure that was
        // never read, and 0% would draw an empty bar rather than no bar.
        let occupancy = match &snapshot.system.memory {
            Reading::Valid { value } => match value.percent() {
                Some(percent) => Reading::valid(percent),
                None => Reading::unavailable(Unavailable::unparsable(
                    "The memory collector reported no total, so occupancy has no percentage.",
                )),
            },
            Reading::Unavailable { cause } => Reading::unavailable(cause.clone()),
        };
        self.memory_percent.observe(&occupancy, system_at);

        let gpu_at = snapshot.gpu.at_unix_ms;
        self.gpu_load.observe(&snapshot.gpu.load_percent, gpu_at);
        self.gpu_temperature
            .observe(&snapshot.gpu.temperature_c, gpu_at);
    }

    fn observe_kraken(&mut self, kraken: &KrakenTelemetry) {
        let at = kraken.at_unix_ms;
        self.liquid.observe(&kraken.liquid_temperature_c, at);

        for channel in [Channel::Pump, Channel::Fan] {
            let source = kraken.channel(channel);
            let metrics = match channel {
                Channel::Pump => &mut self.pump,
                Channel::Fan => &mut self.fan,
            };
            let rpm = source.rpm.map(|value| *value as f32);
            metrics.rpm.observe(&rpm, at);
            metrics.duty.observe(&source.duty, at);
            metrics.mode.observe(&source.mode, at);
        }
    }

    pub fn channel(&self, channel: Channel) -> &ChannelMetrics {
        match channel {
            Channel::Pump => &self.pump,
            Channel::Fan => &self.fan,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kori_core::telemetry::{
        ChannelTelemetry, DROP_AFTER_MS, GpuTelemetry, STALE_AFTER_MS, SystemTelemetry, Unavailable,
    };

    fn snapshot(at: u64, liquid: Option<f32>) -> TelemetrySnapshot {
        let reading = |value: Option<f32>| match value {
            Some(value) => Reading::valid(value),
            None => Reading::unavailable(Unavailable::unreadable("temp1_input: EIO")),
        };

        TelemetrySnapshot {
            sequence: at,
            at_unix_ms: at,
            interval_ms: 1_000,
            kraken: KrakenTelemetry {
                at_unix_ms: at,
                present: true,
                liquid_temperature_c: reading(liquid),
                pump: ChannelTelemetry {
                    channel: Channel::Pump,
                    rpm: Reading::valid(2_970),
                    duty: Reading::valid(255),
                    mode: Reading::valid(PwmMode::FullSpeed),
                },
                fan: ChannelTelemetry {
                    channel: Channel::Fan,
                    rpm: Reading::valid(1_764),
                    duty: Reading::valid(255),
                    mode: Reading::valid(PwmMode::FullSpeed),
                },
            },
            system: SystemTelemetry {
                at_unix_ms: at,
                cpu_load_percent: Reading::valid(12.5),
                cpu_temperature_c: Reading::valid(46.8),
                memory: Reading::valid(MemoryUsage {
                    used_bytes: 512,
                    total_bytes: 1_024,
                }),
            },
            gpu: GpuTelemetry {
                at_unix_ms: at,
                source: "NVML".into(),
                name: Reading::valid("Test GPU".into()),
                load_percent: Reading::valid(30.0),
                temperature_c: Reading::valid(51.0),
            },
            alerts: Vec::new(),
            failed: Vec::new(),
        }
    }

    #[test]
    fn a_read_failure_leaves_the_last_value_visible_then_drops_it() {
        let mut book = MetricBook::new();
        book.observe(&snapshot(1_000, Some(29.8)));
        book.observe(&snapshot(2_000, None));

        assert_eq!(book.liquid.view(2_100).copied(), Some(29.8));
        assert!(book.liquid.view(1_000 + STALE_AFTER_MS).is_stale());
        assert!(book.liquid.view(1_000 + DROP_AFTER_MS).is_unavailable());
    }

    #[test]
    fn memory_occupancy_is_derived_alongside_the_raw_figures() {
        let mut book = MetricBook::new();
        book.observe(&snapshot(1_000, Some(29.8)));

        assert!((book.memory_percent.view(1_100).copied().unwrap() - 50.0).abs() < 0.001);
        let usage = book.memory.view(1_100).copied().unwrap();
        assert_eq!(usage.total_bytes, 1_024);
    }

    #[test]
    fn a_section_that_stops_updating_ages_alone() {
        let mut book = MetricBook::new();
        let mut first = snapshot(1_000, Some(29.8));
        book.observe(&first);

        // Only the kraken and system sections advance; the GPU collector is
        // wedged and its timestamp stays where it was.
        for step in 2..=6u64 {
            let mut next = snapshot(step * 1_000, Some(29.8));
            next.gpu = first.gpu.clone();
            book.observe(&next);
            first = next;
        }

        let now = 6_000;
        assert!(book.cpu_load.view(now).copied().is_some());
        assert!(!book.cpu_load.view(now).is_stale());
        assert!(
            book.gpu_load.view(now).is_stale(),
            "the GPU should have aged"
        );
    }

    #[test]
    fn channel_metrics_are_addressable_by_channel() {
        let mut book = MetricBook::new();
        book.observe(&snapshot(1_000, Some(29.8)));
        assert_eq!(
            book.channel(Channel::Pump).rpm.view(1_100).copied(),
            Some(2_970.0)
        );
        assert_eq!(
            book.channel(Channel::Fan).mode.view(1_100).copied(),
            Some(PwmMode::FullSpeed)
        );
    }
}

// SPDX-FileCopyrightText: 2026 Arthur Jean
// SPDX-License-Identifier: GPL-3.0-or-later

//! What the visible screen shows, so a sample that changes none of it costs
//! no repaint.
//!
//! A repaint rebuilds the whole element tree of the shown screen and walks
//! the layout, which was once a second, every second, whatever the screen and
//! whether or not anything on it had moved. Settings shows nothing a sample
//! changes; Cooling shows the Kraken and nothing about the GPU. So each sample
//! is folded in, the visible screen's key is built from what that screen
//! actually reads, and it is compared with the key of the last frame painted.
//! Equal keys mean an identical picture, and no repaint is asked for.
//!
//! The key errs toward repainting. It carries the exact values a screen reads
//! rather than the rounded text it prints, because a dial's arc moves with the
//! value even where its label does not; and every value is compared in the
//! words its freshness is shown in, so a reading going stale repaints once.

use kori_core::capability::CapabilityId;
use kori_core::capability::LcdPanel;
use kori_core::display::DisplayPreset;
use kori_core::ipc::{ChannelState, ConfigState};
use kori_core::profile::Channel;
use kori_core::telemetry::PwmMode;
use kori_core::telemetry::{CollectorFailure, MemoryUsage, MetricView, SafetyAlert};
use kori_core::{KRAKEN_BASE, RGB_CONTROLLER};

use crate::components::ControlState;
use crate::link::DeviceSummary;

use super::screen::monitoring::Trend;
use super::sync::Domain;
use super::{Destination, Shell};

/// One reading as a screen presents it: the value, the word for its
/// freshness, and the sentence an unavailable one carries.
type Shown = (Option<u64>, Option<&'static str>, Option<String>);

fn shown<T: Copy + Into<f64>>(view: &MetricView<T>) -> Shown {
    (
        view.copied().map(|value| value.into().to_bits()),
        view.qualifier(),
        view.detail().map(str::to_string),
    )
}

/// Everything one frame of the shell paints that a sample can change.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Visible {
    destination: Destination,
    banner: Option<String>,
    outcome: Option<u64>,
    screen: Screen,
}

#[derive(Debug, Clone, PartialEq)]
enum Screen {
    Monitoring {
        readings: Vec<Shown>,
        memory: Option<MemoryUsage>,
        failed: Vec<CollectorFailure>,
        devices: Vec<DeviceSummary>,
        reachable: bool,
        trend: Trend,
    },
    Cooling {
        controls: Vec<ControlState>,
        active: Option<String>,
        profiles: Vec<String>,
        alerts: Vec<SafetyAlert>,
        channels: Vec<ChannelShown>,
        liquid: Shown,
        /// The coolant marker on the curve, which is the latest raw reading
        /// rather than the retained one: a failed read drops the marker while
        /// the readout above still holds its last value.
        marker: Option<u64>,
        pending: bool,
        notice: Option<String>,
    },
    Lighting {
        controls: Vec<ControlState>,
        channels: Vec<ChannelState>,
        panel: Option<LcdPanel>,
        committed: Option<DisplayPreset>,
        faulted: Option<String>,
        picture: u64,
        notices: Vec<Option<String>>,
    },
    Settings {
        facts: Vec<(String, String)>,
    },
}

/// One cooling channel as its row shows it.
#[derive(Debug, Clone, PartialEq)]
struct ChannelShown {
    rpm: Shown,
    duty: Shown,
    mode: Option<PwmMode>,
    confirmed_percent: Option<u64>,
    alerts: Vec<SafetyAlert>,
}

impl Shell {
    /// The key of the screen as it would paint now.
    pub(crate) fn visible(&self) -> Visible {
        Visible {
            destination: self.destination,
            banner: self.link.banner(),
            outcome: self.outcome.as_ref().map(|outcome| outcome.sequence),
            screen: match self.destination {
                Destination::Monitoring => self.monitoring_key(),
                Destination::Cooling => self.cooling_key(),
                Destination::Lighting => self.lighting_key(),
                Destination::Settings => Screen::Settings {
                    facts: self
                        .settings_facts()
                        .into_iter()
                        .map(|(label, value)| (label.to_string(), value.to_string()))
                        .collect(),
                },
            },
        }
    }

    fn monitoring_key(&self) -> Screen {
        let now = self.now_unix_ms;
        let book = &self.metrics;
        Screen::Monitoring {
            readings: [
                &book.cpu_load,
                &book.cpu_temperature,
                &book.gpu_load,
                &book.gpu_temperature,
                &book.memory_percent,
                &book.liquid,
                &book.pump.rpm,
                &book.fan.rpm,
            ]
            .into_iter()
            .map(|tracked| shown(&tracked.view(now)))
            .collect(),
            memory: book.memory.view(now).copied(),
            failed: self.link.failed_collectors().to_vec(),
            devices: self.link.device_rows(),
            reachable: self.link.status().is_some(),
            trend: self.trend.clone(),
        }
    }

    fn cooling_key(&self) -> Screen {
        let now = self.now_unix_ms;
        let program = self.cooling.program();
        let mut controls = vec![
            self.link.write_state(),
            self.link
                .program_state(KRAKEN_BASE, &program.required_capabilities(), now),
        ];
        let mut channels = Vec::new();
        for channel in [Channel::Pump, Channel::Fan] {
            controls.push(
                self.link
                    .cooling_state(KRAKEN_BASE, channel.duty_capability(), now),
            );
            controls.push(
                self.link
                    .cooling_state(KRAKEN_BASE, channel.curve_capability(), now),
            );
            let metrics = self.metrics.channel(channel);
            channels.push(ChannelShown {
                rpm: shown(&metrics.rpm.view(now)),
                duty: shown(&metrics.duty.view(now)),
                mode: metrics.mode.view(now).copied(),
                confirmed_percent: self
                    .kraken()
                    .and_then(|kraken| kraken.channel(channel).duty_percent())
                    .map(|percent| f64::from(percent).to_bits()),
                alerts: self
                    .link
                    .channel_alerts(channel)
                    .into_iter()
                    .cloned()
                    .collect(),
            });
        }
        Screen::Cooling {
            controls,
            active: self.link.active_profile().map(str::to_string),
            profiles: self
                .link
                .profiles()
                .iter()
                .map(|profile| profile.name.clone())
                .collect(),
            alerts: self.link.alerts().to_vec(),
            channels,
            liquid: shown(&self.metrics.liquid.view(now)),
            marker: self
                .kraken()
                .and_then(|kraken| kraken.liquid_temperature_c.copied())
                .map(|value| f64::from(value).to_bits()),
            pending: self.cooling.pending(self.kraken()),
            notice: self.revisions.notice(Domain::Cooling).map(str::to_string),
        }
    }

    fn lighting_key(&self) -> Screen {
        let status = self.link.status();
        let channels = self.link.lighting_channels().to_vec();
        let mut notices: Vec<Option<String>> = channels
            .iter()
            .map(|state| {
                self.revisions
                    .notice(Domain::Lighting(state.channel))
                    .map(str::to_string)
            })
            .collect();
        notices.push(self.revisions.notice(Domain::Panel).map(str::to_string));
        Screen::Lighting {
            controls: vec![
                self.link
                    .control_state(RGB_CONTROLLER, CapabilityId::RgbEffects),
                self.link
                    .control_state(RGB_CONTROLLER, CapabilityId::RgbFixedColor),
                self.link.control_state(KRAKEN_BASE, CapabilityId::LcdFrame),
            ],
            channels,
            panel: status.and_then(|status| status.display.panel.clone()),
            committed: status.and_then(|status| status.display.committed.clone()),
            faulted: status.and_then(|status| status.display.faulted.clone()),
            picture: self.picture_version,
            notices,
        }
    }
}

/// What the Settings screen states about the configuration, in its words.
pub(crate) fn configuration_fact(config: &ConfigState) -> String {
    config
        .recovery_message()
        .unwrap_or_else(|| "Loaded".to_string())
}

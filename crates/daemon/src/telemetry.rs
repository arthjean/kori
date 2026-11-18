// SPDX-FileCopyrightText: 2026 Arthur Jean
// SPDX-License-Identifier: GPL-3.0-or-later

//! The sampling loops.
//!
//! Three collectors run independently, one thread each, publishing into one
//! shared state. That shape is what makes isolation real: a collector that
//! panics is caught and reported without touching the others, and a collector
//! that blocks simply stops refreshing its own section, which the client then
//! ages out. A single loop would have turned either failure into a dead
//! dashboard.
//!
//! Nothing here writes. Every read goes through `std::fs::read`, so the
//! "telemetry performs zero writes to `hwmon`" property is structural rather
//! than a promise about call sites.

use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use kori_core::telemetry::{
    AlertTracker, Collector, CollectorFailure, GpuTelemetry, KrakenTelemetry, SAMPLE_INTERVAL_MS,
    SafetyAlert, SystemTelemetry, TelemetrySnapshot, Unavailable,
};
use kori_hardware_linux::SysfsRoot;
use kori_hardware_linux::gpu::GpuSensor;
use kori_hardware_linux::hwmon::KrakenHwmon;
use kori_hardware_linux::sensors::SystemSensors;

use crate::wake::{Alarm, Wake};

/// The shared, always-latest section of every collector.
#[derive(Debug)]
struct SamplerState {
    sequence: u64,
    kraken: KrakenTelemetry,
    alerts: Vec<SafetyAlert>,
    system: SystemTelemetry,
    gpu: GpuTelemetry,
    failures: Vec<CollectorFailure>,
}

impl SamplerState {
    fn new(at_unix_ms: u64) -> Self {
        let waiting = Unavailable::unreadable("The first sample has not completed yet.");
        Self {
            sequence: 0,
            kraken: KrakenTelemetry::absent(at_unix_ms, waiting.clone()),
            alerts: Vec::new(),
            system: SystemTelemetry::unavailable(at_unix_ms, waiting.clone()),
            gpu: GpuTelemetry::unavailable(at_unix_ms, waiting),
            failures: Vec::new(),
        }
    }

    /// Record or clear a collector's failure, keeping one entry per collector.
    fn set_failure(&mut self, collector: Collector, detail: Option<String>) {
        self.failures
            .retain(|failure| failure.collector != collector);
        if let Some(detail) = detail {
            self.failures.push(CollectorFailure { collector, detail });
        }
    }
}

/// Runs the collectors and hands out the latest snapshot.
pub struct Sampler {
    state: Arc<Mutex<SamplerState>>,
    stop: Arc<Alarm>,
    threads: Vec<JoinHandle<()>>,
    interval_ms: u64,
}

impl Sampler {
    /// Start every collector against `sysfs`.
    ///
    /// Source resolution happens on the collector thread, not here, so a
    /// missing sensor or an absent NVML never delays daemon startup.
    pub fn start(sysfs: &SysfsRoot, proc_root: &Path, interval: Duration) -> Self {
        let state = Arc::new(Mutex::new(SamplerState::new(crate::now_unix_ms())));
        let stop = Arc::new(Alarm::new());
        let interval_ms = interval.as_millis() as u64;

        let started = [
            (
                Collector::Kraken,
                spawn_kraken(sysfs.clone(), interval, &state, &stop),
            ),
            (
                Collector::Cpu,
                spawn_system(
                    sysfs.clone(),
                    proc_root.to_path_buf(),
                    interval,
                    &state,
                    &stop,
                ),
            ),
            (Collector::Gpu, spawn_gpu(interval, &state, &stop)),
        ];
        let mut threads = Vec::with_capacity(started.len());
        for (collector, thread) in started {
            match thread {
                Ok(thread) => threads.push(thread),
                // A collector with no thread is a collector that failed, and is
                // reported like one: its section keeps saying no sample has
                // completed, and the failure says why.
                Err(error) => {
                    if let Ok(mut state) = state.lock() {
                        state.set_failure(
                            collector,
                            Some(format!(
                                "The {} collector could not start a thread: {error}.",
                                collector.label()
                            )),
                        );
                    }
                }
            }
        }

        Self {
            state,
            stop,
            threads,
            interval_ms,
        }
    }

    /// Assemble the current snapshot from whatever each collector last
    /// published.
    pub fn snapshot(&self) -> TelemetrySnapshot {
        let now = crate::now_unix_ms();
        let Ok(state) = self.state.lock() else {
            return TelemetrySnapshot::unavailable(
                now,
                Unavailable::unreadable("The sampler state is unavailable."),
            );
        };

        TelemetrySnapshot {
            sequence: state.sequence,
            at_unix_ms: now,
            interval_ms: self.interval_ms,
            kraken: state.kraken.clone(),
            system: state.system.clone(),
            gpu: state.gpu.clone(),
            alerts: state.alerts.clone(),
            failed: state.failures.clone(),
        }
    }

    /// Ask every collector to finish its current pass and exit.
    pub fn stop(&mut self) {
        self.stop.stop();
        for thread in self.threads.drain(..) {
            let _ = thread.join();
        }
    }
}

impl Drop for Sampler {
    fn drop(&mut self) {
        self.stop();
    }
}

impl std::fmt::Debug for Sampler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Sampler")
            .field("interval_ms", &self.interval_ms)
            .field("collectors", &self.threads.len())
            .finish()
    }
}

/// The Kraken collector: `hwmon` readings plus the safety alerts they imply.
fn spawn_kraken(
    sysfs: SysfsRoot,
    interval: Duration,
    state: &Arc<Mutex<SamplerState>>,
    stop: &Arc<Alarm>,
) -> std::io::Result<JoinHandle<()>> {
    let state = Arc::clone(state);
    let stop = Arc::clone(stop);

    crate::wake::spawn("kraken", move || {
        let mut hwmon = KrakenHwmon::locate(&sysfs);
        let mut tracker = AlertTracker::new();

        run(Collector::Kraken, interval, &stop, &state, move |now| {
            // Re-resolving costs one directory scan and is what lets a
            // reconnected device come back without restarting the daemon.
            if hwmon.is_none() {
                hwmon = KrakenHwmon::locate(&sysfs);
            }

            let Some(instance) = &hwmon else {
                return Section::Kraken {
                    telemetry: KrakenTelemetry::absent(
                        now,
                        Unavailable::no_device(
                            "No kraken2023 hwmon instance is bound to an allowlisted device.",
                        ),
                    ),
                    alerts: Vec::new(),
                };
            };

            let telemetry = instance.telemetry(now);
            // Every reading unavailable means the device went away mid-session.
            if !telemetry.liquid_temperature_c.is_valid()
                && !telemetry.pump.rpm.is_valid()
                && !telemetry.fan.rpm.is_valid()
            {
                hwmon = None;
            }
            let alerts = tracker.observe(&telemetry);
            Section::Kraken { telemetry, alerts }
        })
    })
}

/// The CPU and memory collector.
fn spawn_system(
    sysfs: SysfsRoot,
    proc_root: PathBuf,
    interval: Duration,
    state: &Arc<Mutex<SamplerState>>,
    stop: &Arc<Alarm>,
) -> std::io::Result<JoinHandle<()>> {
    let state = Arc::clone(state);
    let stop = Arc::clone(stop);

    crate::wake::spawn("system", move || {
        let mut sensors = SystemSensors::open(&sysfs, proc_root);
        run(Collector::Cpu, interval, &stop, &state, move |now| {
            Section::System(sensors.telemetry(now))
        })
    })
}

/// The GPU collector, on whatever vendor interface resolved.
fn spawn_gpu(
    interval: Duration,
    state: &Arc<Mutex<SamplerState>>,
    stop: &Arc<Alarm>,
) -> std::io::Result<JoinHandle<()>> {
    let state = Arc::clone(state);
    let stop = Arc::clone(stop);

    crate::wake::spawn("gpu", move || {
        let sensor = GpuSensor::open();
        run(Collector::Gpu, interval, &stop, &state, move |now| {
            Section::Gpu(sensor.sample(now))
        })
    })
}

/// What one collector produces.
enum Section {
    Kraken {
        telemetry: KrakenTelemetry,
        alerts: Vec<SafetyAlert>,
    },
    System(SystemTelemetry),
    Gpu(GpuTelemetry),
}

/// The loop every collector runs.
///
/// `sample` is called outside the shared lock and inside `catch_unwind`, so a
/// panicking collector can neither poison the state nor take the daemon with
/// it. Its failure is published like any other reading.
fn run(
    collector: Collector,
    interval: Duration,
    stop: &Arc<Alarm>,
    state: &Arc<Mutex<SamplerState>>,
    mut sample: impl FnMut(u64) -> Section,
) {
    while !stop.is_stopped() {
        let started = Instant::now();
        let now = crate::now_unix_ms();

        // `AssertUnwindSafe` is the honest annotation here: the closure owns
        // mutable collector state, and a panic may leave it half-updated. The
        // next pass re-reads every source from scratch, so a stale field
        // cannot outlive one interval.
        let outcome = std::panic::catch_unwind(AssertUnwindSafe(|| sample(now)));

        if let Ok(mut state) = state.lock() {
            match outcome {
                Ok(section) => {
                    state.set_failure(collector, None);
                    match section {
                        Section::Kraken { telemetry, alerts } => {
                            state.kraken = telemetry;
                            state.alerts = alerts;
                        }
                        Section::System(system) => state.system = system,
                        Section::Gpu(gpu) => state.gpu = gpu,
                    }
                    state.sequence += 1;
                }
                Err(payload) => {
                    let detail = panic_detail(&payload);
                    state.set_failure(
                        collector,
                        Some(format!(
                            "The {} collector panicked and was isolated: {detail}. \
                             Its readings age out while every other collector keeps running.",
                            collector.label()
                        )),
                    );
                }
            }
        }

        // One wait, to the next pass, that a stop ends at once.
        if stop.sleep_until(next_pass(started, interval)) == Wake::Stopped {
            break;
        }
    }
}

/// When the pass after the one that started at `started` is due.
///
/// Paced against the pass start, so a slow sample shortens the wait rather
/// than adding to it, and a pass that took longer than the interval is
/// followed at once: its deadline is already behind the clock, and a deadline
/// in the past is not waited for. The wait is never longer than `interval`.
/// An interval too large for the clock to represent is not one the daemon is
/// ever configured with; it degrades to passes back to back rather than to a
/// wait with no end.
fn next_pass(started: Instant, interval: Duration) -> Instant {
    started.checked_add(interval).unwrap_or(started)
}

fn panic_detail(payload: &Box<dyn std::any::Any + Send>) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_string()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "no message".to_string()
    }
}

/// The interval the product ships with.
pub fn default_interval() -> Duration {
    Duration::from_millis(SAMPLE_INTERVAL_MS)
}

#[cfg(test)]
mod tests {
    use super::*;
    use kori_hardware_linux::testing::FakeSysfs;

    /// Wait until `condition` holds, or give up after `limit`.
    fn wait_for(limit: Duration, mut condition: impl FnMut() -> bool) -> bool {
        let deadline = Instant::now() + limit;
        while Instant::now() < deadline {
            if condition() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        condition()
    }

    #[test]
    fn every_section_is_sampled_and_carries_its_own_timestamp() {
        let fake = FakeSysfs::new("sampler-sections");
        fake.add_kraken();
        fake.add_kraken_hwmon();
        fake.add_cpu_hwmon();
        let proc_root = fake.add_proc();

        let sampler = Sampler::start(
            &SysfsRoot::new(fake.root_path()),
            &proc_root,
            Duration::from_millis(40),
        );
        assert!(wait_for(Duration::from_secs(2), || {
            let snapshot = sampler.snapshot();
            snapshot.kraken.liquid_temperature_c.is_valid() && snapshot.system.memory.is_valid()
        }));

        let snapshot = sampler.snapshot();
        assert_eq!(snapshot.kraken.liquid_temperature_c.copied(), Some(27.9));
        assert_eq!(snapshot.kraken.pump.rpm.copied(), Some(2970));
        assert_eq!(snapshot.kraken.fan.rpm.copied(), Some(1764));
        assert!(snapshot.system.memory.is_valid());
        assert!(snapshot.kraken.at_unix_ms > 0);
        assert!(snapshot.system.at_unix_ms > 0);
        assert!(snapshot.gpu.at_unix_ms > 0);
        assert!(snapshot.sequence > 0);
    }

    #[test]
    fn a_machine_without_a_kraken_reports_no_device_and_keeps_sampling_the_rest() {
        let fake = FakeSysfs::new("sampler-no-kraken");
        let proc_root = fake.add_proc();
        let sampler = Sampler::start(
            &SysfsRoot::new(fake.root_path()),
            &proc_root,
            Duration::from_millis(40),
        );

        assert!(wait_for(Duration::from_secs(2), || sampler
            .snapshot()
            .sequence
            > 2));
        let snapshot = sampler.snapshot();
        assert!(!snapshot.kraken.present);
        let cause = snapshot
            .kraken
            .liquid_temperature_c
            .cause()
            .unwrap()
            .detail()
            .to_string();
        assert!(cause.contains("kraken2023"), "{cause}");
        // The GPU section still published, whatever it found.
        assert!(snapshot.gpu.at_unix_ms > 0);
    }

    #[test]
    fn sampling_writes_nothing_to_hwmon() {
        let fake = FakeSysfs::new("sampler-read-only");
        fake.add_kraken();
        let hwmon = fake.add_kraken_hwmon();
        let proc_root = fake.add_proc();
        let before = tree_state(&hwmon);

        let sampler = Sampler::start(
            &SysfsRoot::new(fake.root_path()),
            &proc_root,
            Duration::from_millis(20),
        );
        assert!(wait_for(Duration::from_secs(2), || {
            sampler.snapshot().kraken.liquid_temperature_c.is_valid()
        }));
        std::thread::sleep(Duration::from_millis(200));
        drop(sampler);

        assert_eq!(before, tree_state(&hwmon), "telemetry wrote to hwmon");
    }

    #[test]
    fn a_stop_during_the_wait_ends_every_collector_within_100_ms() {
        let fake = FakeSysfs::new("sampler-stop");
        fake.add_kraken();
        fake.add_kraken_hwmon();
        let proc_root = fake.add_proc();
        // The shipped interval: the collectors spend nearly all of it waiting.
        let mut sampler = Sampler::start(
            &SysfsRoot::new(fake.root_path()),
            &proc_root,
            default_interval(),
        );
        assert!(wait_for(Duration::from_secs(3), || {
            sampler.snapshot().sequence >= 3
        }));
        // Every collector finished its first pass and is inside its wait.
        std::thread::sleep(Duration::from_millis(100));

        let asked = Instant::now();
        sampler.stop();
        let elapsed = asked.elapsed();
        assert!(
            elapsed < Duration::from_millis(100),
            "the collectors took {elapsed:?} to stop"
        );
    }

    #[test]
    fn a_pass_longer_than_the_interval_is_followed_at_once() {
        let state = Arc::new(Mutex::new(SamplerState::new(0)));
        let stop = Arc::new(Alarm::new());
        let starts = Arc::new(Mutex::new(Vec::new()));
        let pass = Duration::from_millis(40);
        let interval = Duration::from_millis(10);

        let thread = {
            let (state, stop, starts) =
                (Arc::clone(&state), Arc::clone(&stop), Arc::clone(&starts));
            crate::wake::spawn("test-slow", move || {
                run(Collector::Cpu, interval, &stop, &state, |now| {
                    starts.lock().unwrap().push(Instant::now());
                    std::thread::sleep(pass);
                    Section::System(SystemTelemetry::unavailable(
                        now,
                        Unavailable::unreadable("test"),
                    ))
                })
            })
            .unwrap()
        };
        assert!(wait_for(Duration::from_secs(3), || starts
            .lock()
            .unwrap()
            .len()
            >= 6));
        stop.stop();
        thread.join().unwrap();

        // No wait was added after a pass that already overran: each pass
        // starts where the previous one ended, not an interval later.
        let starts = starts.lock().unwrap();
        for pair in starts.windows(2) {
            let gap = pair[1] - pair[0];
            assert!(gap >= pass, "{gap:?}");
            assert!(
                gap < pass + interval,
                "a wait followed an overrun pass: {gap:?}"
            );
        }
    }

    #[test]
    fn the_next_pass_is_one_interval_after_the_last_one_started() {
        let started = Instant::now();
        let interval = Duration::from_secs(1);
        assert_eq!(next_pass(started, interval), started + interval);
        // An overrun pass has a deadline already behind the clock, which is not
        // waited for; an interval the clock cannot hold is not waited for either.
        assert!(next_pass(started, Duration::ZERO) <= Instant::now());
        assert_eq!(next_pass(started, Duration::MAX), started);
    }

    fn tree_state(root: &std::path::Path) -> Vec<(std::path::PathBuf, std::time::SystemTime, u64)> {
        let mut entries = Vec::new();
        for child in std::fs::read_dir(root).into_iter().flatten().flatten() {
            let Ok(metadata) = child.metadata() else {
                continue;
            };
            if metadata.is_file() {
                entries.push((
                    child.path(),
                    metadata
                        .modified()
                        .unwrap_or(std::time::SystemTime::UNIX_EPOCH),
                    metadata.len(),
                ));
            }
        }
        entries.sort();
        entries
    }
}

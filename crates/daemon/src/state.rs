// SPDX-FileCopyrightText: 2026 Arthur Jean
// SPDX-License-Identifier: GPL-3.0-or-later

//! The daemon's authoritative state and the handler for every typed request.
//!
//! One instance owns the capability record, the per-device locks and the
//! configuration. The server wraps it in a mutex, so requests are serialized:
//! two clients can never interleave a command.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use kori_core::capability::{CapabilityId, CapabilityRecord, DeviceRecord};
use kori_core::diagnostics::{DiagnosticsLog, EVENTS_DEFAULT_LIMIT, EVENTS_MAX_LIMIT, EventKind};
use kori_core::display::DisplayPreset;
use kori_core::history::HistoryPlan;
use kori_core::ipc::{
    AccessMode, ActivationOutcome, ApplyOutcome, Attribution, BlockedCapability, ConnectedClient,
    CoolingState, DaemonStatus, DeviceStatus, DisplayOutcome, HardwareState, IpcError,
    LightingOutcome, MAX_SUBSCRIPTIONS, OwnershipConflict, PROTOCOL_VERSION, Request, Response,
    Revision, SUBSCRIBE_INTERVAL_S, Topic, WriteDomain, validate_client_name,
};
use kori_core::lighting::{LightingCommand, validate_command};
use kori_core::profile::{
    CoolingProgram, Incompatibility, Profile, incompatibilities, program_incompatibilities,
    validate_program,
};
use kori_core::telemetry::{Collector, TelemetrySnapshot};
use kori_core::{DeviceId, KRAKEN_BASE, RGB_CONTROLLER};
use kori_hardware_linux::SysfsRoot;
use kori_hardware_linux::probe::probe;

use crate::config::{ConfigError, Configuration, ProgramToRestore};
use crate::cooling::CoolingExecutor;
use crate::display::DisplayExecutor;
use crate::history::History;
use crate::lighting::LightingExecutor;
use crate::ownership::DeviceLock;
use crate::paths::Paths;
use crate::startup::{
    LcdBackend, RgbBackend, connect_display, connect_lighting, load_configuration,
    record_discovery, take_ownership,
};
use crate::telemetry::{Sampler, default_interval};

/// Version reported to clients and diagnostics.
pub const DAEMON_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Everything one daemon instance owns.
pub struct Daemon {
    paths: Paths,
    capabilities: CapabilityRecord,
    /// Held for the daemon's lifetime; dropping releases the device.
    locks: Vec<DeviceLock>,
    conflicts: Vec<OwnershipConflict>,
    config: Configuration,
    diagnostics: DiagnosticsLog,
    /// The 1 Hz collectors. Dropping the daemon stops them.
    sampler: Sampler,
    /// The sole writer of the thermal path.
    cooling: CoolingExecutor,
    /// The sole writer of the lighting path.
    lighting: LightingExecutor,
    /// The sole writer of the panel.
    display: DisplayExecutor,
    /// The failure each collector is currently reported under, so one fault is
    /// logged once and a recovery is logged exactly when it stops.
    ///
    /// Keyed by collector rather than held as a list of whole failures: the
    /// question asked in both directions is "is this collector failing, and with
    /// what detail", and a list forced one direction to compare whole records
    /// and the other to compare only the name.
    reported_failures: BTreeMap<Collector, String>,
    /// When this instance started, as `status` reports it.
    started_at_unix_ms: u64,
    /// Every open connection, by the number its handshake was answered with.
    connections: BTreeMap<u64, Connection>,
    /// The number the next connection is given. Starts at 1 and never
    /// repeats for the life of the process.
    next_connection: u64,
    /// Who the request being handled came from. [`Attribution::Daemon`]
    /// outside [`Self::handle`], which is what a restore at startup is
    /// attributed to.
    actor: Attribution,
    /// The stream fault the event log last recorded, so a fault is recorded
    /// once per reason and its end exactly once, when a frame lands again.
    stream_fault: Option<String>,
    /// Recorded telemetry, one slot per tick, in memory only.
    history: History,
    /// Where each write domain stands, in memory only and reset at start:
    /// `started_at_unix_ms` is what tells a client a revision is from an
    /// earlier daemon.
    revisions: Revisions,
    /// Connections currently subscribed, at most [`MAX_SUBSCRIPTIONS`].
    subscribers: u32,
}

/// The revision of every write domain.
#[derive(Debug, Clone)]
struct Revisions {
    cooling: Revision,
    /// One per lighting channel, created at revision 1 the first time it is
    /// asked for.
    lighting: BTreeMap<u8, Revision>,
    display: Revision,
}

impl Revisions {
    fn new() -> Self {
        Self {
            cooling: Revision::initial(),
            lighting: BTreeMap::new(),
            display: Revision::initial(),
        }
    }

    fn lighting(&self, channel: u8) -> Revision {
        self.lighting
            .get(&channel)
            .cloned()
            .unwrap_or_else(Revision::initial)
    }

    fn get(&self, domain: WriteDomain, channel: Option<u8>) -> Revision {
        match domain {
            WriteDomain::Cooling => self.cooling.clone(),
            WriteDomain::Lighting => self.lighting(channel.unwrap_or_default()),
            WriteDomain::Display => self.display.clone(),
        }
    }

    fn slot(&mut self, domain: WriteDomain, channel: Option<u8>) -> &mut Revision {
        match domain {
            WriteDomain::Cooling => &mut self.cooling,
            WriteDomain::Lighting => self
                .lighting
                .entry(channel.unwrap_or_default())
                .or_insert_with(Revision::initial),
            WriteDomain::Display => &mut self.display,
        }
    }
}

/// What the daemon knows about one open connection.
#[derive(Debug, Clone)]
struct Connection {
    uid: u32,
    /// From `SO_PEERCRED`, so the kernel's answer rather than the client's.
    pid: i32,
    /// The label the connection gave itself in its handshake.
    client: Option<String>,
    connected_at_unix_ms: u64,
    /// True once the connection wrote or subscribed, which is what earns it a
    /// `client_accepted` event and, later, a `client_disconnected` one.
    announced: bool,
}

impl Daemon {
    /// Probe the machine, take what ownership is available and load config.
    ///
    /// Ownership failure is not startup failure: the daemon stays up in
    /// read-only mode and reports the conflict.
    pub fn start(paths: Paths, sysfs: &SysfsRoot) -> std::io::Result<Self> {
        Self::start_with(
            paths,
            sysfs,
            &kori_hardware_linux::sensors::proc_root_from_env(),
            default_interval(),
            RgbBackend::Hidraw,
            LcdBackend::Nodes,
        )
    }

    /// Start against explicit sources and an explicit sampling interval.
    ///
    /// Only the sources and the cadence change; every other behavior is
    /// identical, so a test exercises the same paths without waiting on real
    /// seconds or mutating a process-global variable.
    pub fn start_with(
        paths: Paths,
        sysfs: &SysfsRoot,
        proc_root: &std::path::Path,
        interval: Duration,
        rgb: RgbBackend,
        lcd: LcdBackend,
    ) -> std::io::Result<Self> {
        paths.ensure()?;

        let started_at_unix_ms = crate::now_unix_ms();
        let mut diagnostics = DiagnosticsLog::default();
        diagnostics.record(
            started_at_unix_ms,
            EventKind::DaemonStarted {
                version: DAEMON_VERSION.to_string(),
                socket_path: paths.socket.display().to_string(),
            },
        );

        let mut capabilities = probe(sysfs);
        // The controller is asked what it is before ownership and before the
        // capability states are logged, so the record the rest of this function
        // reads is the final one. The two requests carry no color and no mode,
        // so asking changes nothing the operator can see.
        let lighting = connect_lighting(&mut capabilities, rgb);
        // The panel is asked the same way and for the same reason: two query
        // reports that carry no picture, no brightness and no orientation.
        let display = connect_display(&mut capabilities, lcd);

        record_discovery(&mut diagnostics, &capabilities);
        let (locks, conflicts) = take_ownership(&paths, &capabilities, &mut diagnostics);
        let config = load_configuration(&paths, &mut diagnostics);

        let mut daemon = Self {
            paths,
            capabilities,
            locks,
            conflicts,
            config,
            diagnostics,
            sampler: Sampler::start(sysfs, proc_root, interval),
            cooling: CoolingExecutor::open(sysfs),
            lighting,
            display,
            reported_failures: BTreeMap::new(),
            started_at_unix_ms,
            connections: BTreeMap::new(),
            next_connection: 1,
            actor: Attribution::Daemon,
            stream_fault: None,
            history: History::new(),
            revisions: Revisions::new(),
            subscribers: 0,
        };

        daemon.record_access_mode();

        // The active profile is put back on the hardware immediately, so a
        // restart resumes the program the operator chose instead of leaving
        // the cooler on whatever the firmware defaulted to.
        daemon.restore_active_profile();

        Ok(daemon)
    }

    /// Log the mode this daemon came up in, and why.
    fn record_access_mode(&mut self) {
        let read_only = self.access_mode().is_read_only();
        let reason = self
            .read_only_reason()
            .unwrap_or_else(|| "every supported capability is writable".to_string());
        self.diagnostics.record(
            crate::now_unix_ms(),
            EventKind::AccessModeChanged { read_only, reason },
        );
    }

    /// Re-apply what the operator left running, after a start.
    ///
    /// The three writes happen in the order an activation performs them, and
    /// each goes through the same function that activation uses, so a start and
    /// an activation cannot drift apart in what they write or in what they
    /// record. What differs is only where each of the three comes from, and that
    /// question belongs to [`Configuration`]: what the operator last put on the
    /// hardware outranks what the profile was saved with, since every Lighting
    /// edit, every drawn curve and every picked picture writes without being
    /// saved under a name. The profile still owns anything the session never
    /// committed, so a machine that comes back cold is the one the operator left
    /// rather than the one the last Save happened to capture.
    ///
    /// A refusal is recorded and the daemon carries on: an incompatible or
    /// unwritable profile must never keep the service from coming up.
    fn restore_active_profile(&mut self) {
        self.apply_lighting(&self.config.lighting_to_restore());
        self.apply_display(self.config.display_to_restore().as_ref());
        if let Some(held) = self.config.program_to_restore() {
            self.apply_program(held);
        }
    }

    /// Put a replayed program on the thermal path, and report the act that asked
    /// for it.
    ///
    /// [`Self::execute`] already records what every program that reached the
    /// executor did, so what is left here is the act. A program a profile named
    /// is reported under that name whatever the outcome, which is what puts a
    /// refused profile on the operator's timeline as the profile they chose
    /// rather than as an anonymous program. A program the session holds answers
    /// to no name, so only a refusal has anything left to say: a refusal never
    /// reached `execute`, and nothing else would mention it.
    fn apply_program(&mut self, held: ProgramToRestore) -> ApplyOutcome {
        let (outcome, unreported) = match self.execute(&held.program) {
            Ok(outcome) => (outcome, None),
            Err(error) => {
                let hardware = HardwareState::NotApplied {
                    reason: error.to_string(),
                };
                (ApplyOutcome::untouched(hardware.clone()), Some(hardware))
            }
        };

        let now = crate::now_unix_ms();
        match held.profile {
            Some(name) => self.diagnostics.record(
                now,
                EventKind::ProfileActivated {
                    name,
                    hardware: outcome.hardware.clone(),
                    by: self.actor.clone(),
                },
            ),
            None => {
                if let Some(hardware) = unreported {
                    self.diagnostics.record(
                        now,
                        EventKind::ProgramApplied {
                            hardware,
                            writes: 0,
                            by: self.actor.clone(),
                        },
                    );
                }
            }
        }

        outcome
    }

    /// Put the panel where a profile wants it, when the profile sets one.
    ///
    /// A refusal is recorded and the daemon carries on, for the same reason a
    /// refused lighting channel does not stop a cooling profile.
    fn apply_display(&mut self, preset: Option<&DisplayPreset>) {
        let Some(preset) = preset.cloned() else {
            return;
        };
        if let Err(error) = self.show(&preset) {
            self.diagnostics.record(
                crate::now_unix_ms(),
                EventKind::DisplayApplied {
                    mode: preset.mode.key().to_string(),
                    hardware: HardwareState::NotApplied {
                        reason: error.to_string(),
                    },
                    frames: 0,
                    by: Some(self.actor.clone()),
                },
            );
        }
    }

    /// Put every lighting channel a restore names where it wants it.
    ///
    /// A refusal is recorded and the next channel is still attempted: a
    /// controller that is read-only must not keep a cooling profile from being
    /// restored, and a channel that is gone must not hide the ones that remain.
    fn apply_lighting(&mut self, commands: &[LightingCommand]) {
        for command in commands {
            if let Err(error) = self.illuminate(command) {
                self.diagnostics.record(
                    crate::now_unix_ms(),
                    EventKind::LightingApplied {
                        channel: command.channel,
                        program: command.program.summary(),
                        hardware: HardwareState::NotApplied {
                            reason: error.to_string(),
                        },
                        writes: 0,
                        by: self.actor.clone(),
                    },
                );
            }
        }
    }

    /// Devices this daemon holds the exclusive lock for.
    pub fn locked_devices(&self) -> Vec<DeviceId> {
        self.locks.iter().map(DeviceLock::device).collect()
    }

    /// Read-only unless ownership is complete and something is writable.
    pub fn access_mode(&self) -> AccessMode {
        match self.read_only() {
            None => AccessMode::ReadWrite,
            Some(cause) => AccessMode::ReadOnly {
                conflicts: cause.into_conflicts(),
            },
        }
    }

    /// What stands between this daemon and a write, or `None` when nothing does.
    ///
    /// The one place that decides the question. [`Self::access_mode`] dresses it
    /// for the status response and [`Self::require_write_access`] turns it into a
    /// refusal, so the two can never answer differently.
    fn read_only(&self) -> Option<ReadOnly> {
        if !self.conflicts.is_empty() {
            return Some(ReadOnly::Held(self.conflicts.clone()));
        }
        if self.has_writable_capability() {
            return None;
        }
        Some(ReadOnly::NothingWritable)
    }

    fn read_only_reason(&self) -> Option<String> {
        self.read_only().map(ReadOnly::reason)
    }

    /// The refusal every write path returns when this daemon is read-only.
    ///
    /// One gate rather than one per command: an ownership conflict is a fact
    /// about the daemon, not about the thing being written, and four copies of
    /// the same check are four chances for one of them to drift.
    fn require_write_access(&self) -> Result<(), IpcError> {
        match self.read_only_reason() {
            None => Ok(()),
            Some(reason) => Err(IpcError::ReadOnly { reason }),
        }
    }

    fn has_writable_capability(&self) -> bool {
        self.capabilities
            .supported()
            .any(|device| device.capabilities.iter().any(|c| c.state.is_writable()))
    }

    /// The record for an allowlisted device that is actually present.
    ///
    /// Every write path starts here, so "the device is not on this machine" is
    /// one refusal written once rather than three lookups that could disagree
    /// about what counts as present.
    fn supported_device(&self, id: DeviceId) -> Result<&DeviceRecord, IpcError> {
        self.capabilities
            .device(id)
            .filter(|device| device.is_supported())
            .ok_or(IpcError::NoDevice)
    }

    fn device_status(&self, device: &DeviceRecord) -> DeviceStatus {
        let mut writable = Vec::new();
        let mut blocked = Vec::new();
        for capability in &device.capabilities {
            match capability.state.blocked_reason() {
                None => writable.push(capability.id),
                Some(reason) => blocked.push(BlockedCapability {
                    capability: capability.id,
                    reason,
                }),
            }
        }

        DeviceStatus {
            id: device.id(),
            present: true,
            owned: self
                .conflicts
                .iter()
                .all(|conflict| conflict.device != Some(device.id())),
            writable,
            blocked,
        }
    }

    pub fn status(&self) -> DaemonStatus {
        DaemonStatus {
            daemon_version: DAEMON_VERSION.to_string(),
            protocol_version: PROTOCOL_VERSION,
            started_at_unix_ms: self.started_at_unix_ms,
            clients: self
                .connections
                .iter()
                .map(|(number, connection)| ConnectedClient {
                    connection: *number,
                    client: connection.client.clone(),
                    pid: connection.pid,
                    connected_at_unix_ms: connection.connected_at_unix_ms,
                })
                .collect(),
            access: self.access_mode(),
            devices: self
                .capabilities
                .supported()
                .map(|device| self.device_status(device))
                .collect(),
            active_profile: self.config.active_profile_name().to_string(),
            config: self.config.state().clone(),
            // What this process wrote and has not since uncommitted, not what
            // the file holds: a restore the hardware refused must not be
            // reported as a program the machine is running.
            cooling: CoolingState {
                committed: self.cooling.committed_program(),
                revision: self.revisions.cooling.clone(),
            },
            lighting: self
                .lighting
                .state(|channel| self.revisions.lighting(channel)),
            display: self.display.state(self.revisions.display.clone()),
            socket_path: self.paths.socket.display().to_string(),
        }
    }

    /// Handle one typed request.
    ///
    /// Every rejection path returns before anything is written, so an invalid
    /// request cannot reach the hardware or the configuration file.
    ///
    /// `connection` is the number [`Self::open_connection`] gave the caller.
    /// Everything this request changes is attributed to it.
    pub fn handle(&mut self, connection: u64, request: Request) -> Response {
        // A subscription is announced like a write: it is a connection that
        // stays, and its departure is worth recording.
        if request.writes() || matches!(request, Request::Subscribe { .. }) {
            self.announce(connection);
        }
        self.actor = self.attribution(connection);
        let response = self.dispatch(connection, request);
        self.actor = Attribution::Daemon;
        if let Response::Error(error) = &response {
            self.diagnostics.record(
                crate::now_unix_ms(),
                EventKind::RequestRejected {
                    error: error.clone(),
                },
            );
        }
        response
    }

    fn dispatch(&mut self, connection: u64, request: Request) -> Response {
        match request {
            Request::Hello {
                protocol_version,
                client,
            } => self.hello(connection, protocol_version, client),
            Request::Status => Response::Status(Box::new(self.status())),
            Request::Capabilities => Response::Capabilities(Box::new(self.capabilities.clone())),
            Request::Profiles => Response::Profiles {
                active: self.config.active_profile_name().to_string(),
                profiles: self.config.profiles(),
            },
            Request::SaveProfile { profile } => self.save_profile(profile),
            Request::ActivateProfile { name } => self.activate_profile(&name),
            Request::DeleteProfile { name } => self.delete_profile(&name),
            Request::Telemetry => Response::Telemetry(Box::new(self.sampler.snapshot())),
            Request::ApplyProgram {
                program,
                expect_revision,
            } => match self
                .require_revision(WriteDomain::Cooling, None, expect_revision)
                .and_then(|()| self.execute(&program))
            {
                Ok(outcome) => Response::Applied(Box::new(outcome)),
                Err(error) => Response::Error(error),
            },
            Request::ApplyLighting {
                command,
                expect_revision,
            } => match self
                .require_revision(
                    WriteDomain::Lighting,
                    Some(command.channel),
                    expect_revision,
                )
                .and_then(|()| self.illuminate(&command))
            {
                Ok(outcome) => Response::Lit(outcome),
                Err(error) => Response::Error(error),
            },
            Request::ApplyDisplay {
                preset,
                expect_revision,
            } => match self
                .require_revision(WriteDomain::Display, None, expect_revision)
                .and_then(|()| self.show(&preset))
            {
                Ok(outcome) => Response::Shown(Box::new(outcome)),
                Err(error) => Response::Error(error),
            },
            Request::Diagnostics => Response::Diagnostics(self.diagnostics.export(
                crate::now_unix_ms(),
                DAEMON_VERSION,
                Some(self.capabilities.clone()),
            )),
            Request::Events { after_seq, limit } => {
                let limit = limit.unwrap_or(EVENTS_DEFAULT_LIMIT);
                if limit > EVENTS_MAX_LIMIT {
                    return Response::Error(IpcError::Malformed {
                        detail: format!(
                            "limit {limit} exceeds the {EVENTS_MAX_LIMIT} events the log holds"
                        ),
                    });
                }
                Response::Events(self.diagnostics.page(after_seq, limit))
            }
            Request::History {
                metrics,
                window_s,
                step_s,
                points,
            } => match HistoryPlan::resolve(metrics.as_deref(), window_s, step_s, points) {
                Ok(plan) => {
                    Response::History(Box::new(self.history.answer(&plan, crate::now_unix_ms())))
                }
                Err(refusal) => Response::Error(IpcError::HistoryOutOfRange {
                    limit: refusal.limit,
                    detail: refusal.detail,
                }),
            },
            Request::Subscribe {
                topics,
                interval_s,
                after_seq: _,
            } => self.subscribe(topics, interval_s),
        }
    }

    /// Bring the daemon's record of the hardware back in line with the latest
    /// sample, and log what changed.
    ///
    /// Runs on the daemon's own clock rather than on a client request. Every
    /// fact it settles is one the hardware owns: whether the device is still
    /// there, whether the curve it was told to run is the one it is running,
    /// whether a collector is failing. A daemon serving nobody has to answer
    /// them just as often as one with a window open, because the writes it is
    /// protecting happened either way.
    fn reconcile(&mut self, snapshot: &TelemetrySnapshot) {
        // A device that has gone away invalidates the executor's record of
        // what it committed. Without this, an Apply after a reconnect could
        // deduplicate against a program the hardware no longer holds and
        // silently write nothing.
        if !snapshot.kraken.present {
            self.cooling.forget();
            // The panel lives on the same device, so it went away too. This
            // also rearms the priming rule: the panel swaps buffers on the
            // transfer after the one that filled it, so a link that came back
            // believing it is already primed would lose its first frame into
            // the buffer nothing is showing.
            self.display.forget();
        } else {
            // A curve write is accepted on the mode alone, because the points
            // cannot be read back. This is the only place the device gets to
            // contradict it, so a divergence is recorded and the channel is
            // uncommitted, which makes the next Apply write rather than
            // deduplicate against a program the device never took.
            let now = crate::now_unix_ms();
            for divergence in self.cooling.verify_curves(&snapshot.kraken, now) {
                self.diagnostics.record(
                    now,
                    EventKind::CurveDiverged {
                        channel: divergence.channel,
                        liquid_temperature_mc: divergence.liquid_temperature_mc,
                        expected: divergence.expected,
                        reported: divergence.reported,
                    },
                );
            }
        }

        self.record_collector_health(snapshot);
    }

    /// Log every collector that started failing and every one that stopped.
    ///
    /// Both directions ask the same question of the same map, so a fault is
    /// logged once, a changed detail is logged again, and a recovery is logged
    /// exactly when the collector leaves the failing set.
    fn record_collector_health(&mut self, snapshot: &TelemetrySnapshot) {
        let current: BTreeMap<Collector, String> = snapshot
            .failed
            .iter()
            .map(|failure| (failure.collector, failure.detail.clone()))
            .collect();

        for (collector, detail) in &current {
            if self.reported_failures.get(collector) != Some(detail) {
                self.diagnostics.record(
                    crate::now_unix_ms(),
                    EventKind::CollectorFailed {
                        collector: *collector,
                        detail: detail.clone(),
                    },
                );
            }
        }
        for collector in self.reported_failures.keys() {
            if !current.contains_key(collector) {
                self.diagnostics.record(
                    crate::now_unix_ms(),
                    EventKind::CollectorRecovered {
                        collector: *collector,
                    },
                );
            }
        }
        self.reported_failures = current;
    }

    /// Gate a cooling program, then write it.
    ///
    /// Every refusal happens before the executor is reached, so a rejected
    /// request cannot produce a partial write. The order matters: values are
    /// validated first because an out-of-range duty is wrong whatever the
    /// device reports, then the capability record, then ownership.
    fn execute(&mut self, program: &CoolingProgram) -> Result<ApplyOutcome, IpcError> {
        validate_program(program)?;

        if *program == CoolingProgram::Onboard {
            // Nothing is written, so no capability is required. This is the
            // program the daemon falls back to, and it must never be refusable.
            let outcome = self.cooling.apply(Instant::now(), program);
            self.advance_revision(WriteDomain::Cooling, None, &outcome.hardware);
            return Ok(outcome);
        }

        let details = program_incompatibilities(program, self.supported_device(KRAKEN_BASE)?);
        if !details.is_empty() {
            return Err(IpcError::Incompatible { details });
        }

        self.require_write_access()?;

        let outcome = self.cooling.apply(Instant::now(), program);
        self.advance_revision(WriteDomain::Cooling, None, &outcome.hardware);
        self.diagnostics.record(
            crate::now_unix_ms(),
            EventKind::ProgramApplied {
                hardware: outcome.hardware.clone(),
                writes: outcome.writes,
                by: self.actor.clone(),
            },
        );
        if outcome.hardware == HardwareState::Confirmed {
            self.remember(|config| config.record_program(program));
        }
        Ok(outcome)
    }

    /// Gate a lighting command, then send it.
    ///
    /// The order mirrors [`Daemon::execute`] and matters for the same reason.
    /// Values are checked first because a malformed color is wrong whatever the
    /// controller reports. The topology is checked next, because a channel that
    /// does not exist must be refused before any capability question. Then the
    /// capability record, which is what carries the "this firmware was never
    /// validated" refusal. Then ownership. The executor is reached last, so a
    /// rejected request cannot produce a partial write.
    fn illuminate(&mut self, command: &LightingCommand) -> Result<LightingOutcome, IpcError> {
        validate_command(command, self.lighting.channel_count())?;

        require_writable(
            self.supported_device(RGB_CONTROLLER)?,
            command.program.required_capability(),
        )?;

        self.require_write_access()?;

        let outcome = self.lighting.apply(Instant::now(), command);
        self.advance_revision(
            WriteDomain::Lighting,
            Some(command.channel),
            &outcome.hardware,
        );
        self.diagnostics.record(
            crate::now_unix_ms(),
            EventKind::LightingApplied {
                channel: outcome.channel,
                program: outcome.program.summary(),
                hardware: outcome.hardware.clone(),
                writes: outcome.writes,
                by: self.actor.clone(),
            },
        );
        if outcome.hardware == HardwareState::Confirmed {
            self.remember(|config| config.record_lighting(command));
        }
        Ok(outcome)
    }

    /// Gate a panel preset, then render and send it.
    ///
    /// The order mirrors [`Daemon::illuminate`] and matters for the same
    /// reason. The preset is validated first because a mode with no file is
    /// wrong whatever the panel reports. Then the capability record, which is
    /// what carries the "this firmware was never validated" refusal and the
    /// "there may be no panel behind this cap" one. Then ownership. The
    /// executor is reached last, so a rejected request renders nothing and
    /// sends nothing.
    fn show(&mut self, preset: &DisplayPreset) -> Result<DisplayOutcome, IpcError> {
        preset.validate()?;

        {
            let device = self.supported_device(KRAKEN_BASE)?;
            for capability in [CapabilityId::LcdFrame, CapabilityId::LcdDisplayControl] {
                require_writable(device, capability)?;
            }
        }

        self.require_write_access()?;

        let samples = preset.samples(&self.sampler.snapshot());
        let outcome = self.display.apply(preset, &samples)?;
        self.advance_revision(WriteDomain::Display, None, &outcome.hardware);
        // A command restarts the stream and records its own event, so a fault
        // the log was holding is settled by that event rather than by a
        // recovery the stream did not make.
        self.stream_fault = None;
        self.diagnostics.record(
            crate::now_unix_ms(),
            EventKind::DisplayApplied {
                mode: preset.mode.key().to_string(),
                hardware: outcome.hardware.clone(),
                frames: outcome.frames,
                by: Some(self.actor.clone()),
            },
        );
        if outcome.hardware == HardwareState::Confirmed {
            self.remember(|config| config.record_display(preset));
        }
        Ok(outcome)
    }

    /// Refuse a write whose precondition no longer holds.
    ///
    /// Asked before anything else about the write, so a stale client is told
    /// so before it learns anything about its payload, and nothing reaches the
    /// hardware or the configuration file. Without `expected`, the last writer
    /// wins, which is how every client wrote before revisions existed.
    fn require_revision(
        &self,
        domain: WriteDomain,
        channel: Option<u8>,
        expected: Option<u64>,
    ) -> Result<(), IpcError> {
        let Some(expected) = expected else {
            return Ok(());
        };
        let current = self.revisions.get(domain, channel);
        if current.revision == expected {
            return Ok(());
        }
        Err(IpcError::Conflict {
            domain,
            channel: channel.filter(|_| domain == WriteDomain::Lighting),
            expected,
            current: current.revision,
            changed_by: current.changed_by,
        })
    }

    /// Count one accepted write command in `domain`.
    ///
    /// The restore at startup is what revision 1 describes, so the daemon's
    /// own writes leave the number where it is; every write a connection
    /// asked for, a profile activation included, moves it. A command answered
    /// `not_applied` changed nothing (a lighting command inside the cadence
    /// floor, a panel that is not there), so it moves nothing either: a
    /// revision that advanced without a change would refuse the next client
    /// over a write that never happened.
    fn advance_revision(
        &mut self,
        domain: WriteDomain,
        channel: Option<u8>,
        hardware: &HardwareState,
    ) {
        if self.actor == Attribution::Daemon || matches!(hardware, HardwareState::NotApplied { .. })
        {
            return;
        }
        let changed_by = self.actor.clone();
        let slot = self.revisions.slot(domain, channel);
        slot.revision += 1;
        slot.changed_by = changed_by;
    }

    /// Persist part of the committed state, without letting the disk speak for
    /// the hardware.
    ///
    /// The write has already reached the device by the time this runs, so a
    /// configuration that cannot be written must not turn a confirmed command
    /// into a refusal. It is recorded instead, because what the operator loses
    /// is real: the state stops surviving a restart, and nothing else on screen
    /// would say so.
    fn remember(&mut self, record: impl FnOnce(&mut Configuration) -> Result<(), ConfigError>) {
        if let Err(error) = record(&mut self.config) {
            self.diagnostics.record(
                crate::now_unix_ms(),
                EventKind::SessionNotRecorded {
                    detail: error.to_string(),
                },
            );
        }
    }

    /// One pass of the daemon's own clock: settle what the hardware says, then
    /// redraw the panel from it.
    ///
    /// Called once a second by the server's ticker, never by a client, so both
    /// halves keep happening with the window closed. That is the point of doing
    /// it here: the reconciliation guards writes this daemon made, and a write
    /// does not stop needing a guard because nobody is looking at it.
    ///
    /// A tick that finds nothing to do costs one sample and one render and no
    /// transfer, because the executor compares the picture it produced against
    /// the one the panel already holds.
    pub fn tick(&mut self, now: Instant) {
        let snapshot = self.sampler.snapshot();
        self.history.record(&snapshot, crate::now_unix_ms());
        self.reconcile(&snapshot);

        // The panel is on the device the reconciliation may have just declared
        // gone, and redrawing would put a frame straight back into the record
        // that pass dropped. Nothing is sent until the device answers again,
        // which is also what keeps a disconnected Kraken from being handed a
        // frame a second forever.
        if !snapshot.kraken.present {
            return;
        }

        let Some(preset) = self.display.active().cloned() else {
            return;
        };
        let samples = preset.samples(&snapshot);
        // Only a change of the stream's state is recorded: the first failure
        // for each reason, and the first frame that lands after one. A frame
        // that lands after a frame that landed is the steady state, which the
        // counters in `status.display` carry. Recording each one made the
        // panel nine events in ten of the ring and pushed out the startup
        // within minutes, while the failure that matters most was the one a
        // stream stopping with `frames: 0` used to leave unrecorded.
        let outcome = self.display.refresh(now, &samples);
        let landed = outcome.as_ref().is_some_and(|outcome| {
            outcome.frames > 0 && outcome.hardware == HardwareState::Confirmed
        });
        // A refresh that faulted without an outcome is a preset that stopped
        // rendering, which reached no hardware at all.
        let hardware = outcome.map(|outcome| outcome.hardware);
        self.record_stream_transition(landed, hardware, |reason| HardwareState::NotApplied {
            reason,
        });
    }

    /// Record the stream stopping for a reason not yet recorded, or landing its
    /// first frame after a stop.
    ///
    /// `hardware` is what the pass reported when it reported anything, and
    /// `otherwise` builds the state for a fault the pass did not describe.
    fn record_stream_transition(
        &mut self,
        landed: bool,
        hardware: Option<HardwareState>,
        otherwise: impl FnOnce(String) -> HardwareState,
    ) {
        let mode = || {
            self.display
                .active()
                .map_or_else(String::new, |preset| preset.mode.key().to_string())
        };
        match self.display.fault_reason() {
            Some(reason) if self.stream_fault.as_deref() != Some(reason) => {
                let reason = reason.to_string();
                let hardware = match hardware {
                    Some(state) if state != HardwareState::Confirmed => state,
                    _ => otherwise(reason.clone()),
                };
                let event = EventKind::DisplayApplied {
                    mode: mode(),
                    hardware,
                    frames: 0,
                    by: None,
                };
                self.diagnostics.record(crate::now_unix_ms(), event);
                self.stream_fault = Some(reason);
            }
            Some(_) => {}
            None => {
                if landed && self.stream_fault.is_some() {
                    let event = EventKind::DisplayApplied {
                        mode: mode(),
                        hardware: HardwareState::Confirmed,
                        frames: 1,
                        by: None,
                    };
                    self.diagnostics.record(crate::now_unix_ms(), event);
                    self.stream_fault = None;
                }
            }
        }
    }

    /// Record the start of an orderly shutdown.
    pub fn record_stopping(&mut self, reason: String) {
        self.diagnostics
            .record(crate::now_unix_ms(), EventKind::DaemonStopping { reason });
    }

    /// Account for ticks the clock slept through.
    ///
    /// Each is a frame the panel will never get and a second the history
    /// records as null, so the history keeps one slot per second whatever the
    /// ticker managed.
    pub fn skip_ticks(&mut self, count: u32) {
        for _ in 0..count {
            self.display.drop_frame();
        }
        self.history.record_gap(count, crate::now_unix_ms());
    }

    /// Advance a playing animation, and say when its next frame is due.
    ///
    /// `None` means nothing is playing, so the caller has only the telemetry
    /// cadence to wake for.
    ///
    /// No diagnostic is recorded per frame. An animation runs at up to a dozen
    /// frames a second, which would fill the event ring in a couple of minutes
    /// and push out the events that say what the hardware actually did. What is
    /// recorded is the same as for the telemetry stream: a transfer that stops
    /// the animation, once per reason, and the first frame that lands after it.
    pub fn tick_animation(&mut self, now: Instant) -> Option<Instant> {
        let was_faulted = self.display.fault_reason().is_some();
        let due = self.display.advance_animation(now);
        // The executor clears a fault only when a frame lands, so a fault that
        // was standing before this pass and is gone after it is one.
        let landed = was_faulted && self.display.fault_reason().is_none();
        if was_faulted || self.display.fault_reason().is_some() {
            self.record_stream_transition(landed, None, |reason| HardwareState::Uncertain {
                reason,
            });
        }
        due
    }

    fn save_profile(&mut self, profile: Profile) -> Response {
        let name = profile.name.clone();
        match self.config.save_profile(profile) {
            Ok(()) => {
                self.diagnostics.record(
                    crate::now_unix_ms(),
                    EventKind::ProfileSaved {
                        name: name.clone(),
                        by: self.actor.clone(),
                    },
                );
                Response::Saved { name }
            }
            Err(error) => Response::Error(config_error(error)),
        }
    }

    /// Devices a profile applies to.
    ///
    /// A profile pinned to one device matches only that device. An unpinned
    /// profile matches every device that exposes at least one capability its
    /// program needs, so a cooling profile is never checked against the RGB
    /// controller.
    fn targets_for(&self, profile: &Profile) -> Vec<&DeviceRecord> {
        let required = profile.program.required_capabilities();
        self.capabilities
            .supported()
            .filter(|device| match profile.device {
                Some(id) => device.id() == id,
                None => required
                    .iter()
                    .any(|capability| device.capability(*capability).is_some()),
            })
            .collect()
    }

    fn activate_profile(&mut self, name: &str) -> Response {
        let Some(profile) = self.config.profile(name) else {
            return Response::Error(IpcError::ProfileNotFound {
                name: name.to_string(),
            });
        };

        // Compatibility is checked against every supported device before the
        // selection is persisted, so an impossible profile never becomes
        // active.
        let onboard = profile.program == CoolingProgram::Onboard;
        if !onboard {
            let targets = self.targets_for(&profile);
            if targets.is_empty() {
                return Response::Error(IpcError::NoDevice);
            }

            let details: Vec<_> = targets
                .iter()
                .flat_map(|device| incompatibilities(&profile, device))
                .collect();
            if !details.is_empty() {
                return Response::Error(IpcError::Incompatible { details });
            }
            if let Err(error) = self.require_write_access() {
                return Response::Error(error);
            }
        }

        match self.config.activate(name) {
            Ok(profile) => {
                // An activation is the operator naming what they want, so the
                // profile wins here rather than the session: a profile that
                // sets no lighting and no picture leaves both alone, exactly as
                // it did before the session record existed.
                //
                // The selection is persisted first, then written. A write that
                // fails leaves the profile selected and its hardware state
                // reported honestly, rather than silently reverting a choice
                // the operator made.
                let name = profile.name.clone();
                self.apply_lighting(&profile.lighting);
                self.apply_display(profile.display.as_ref());
                let applied = self.apply_program(ProgramToRestore {
                    program: profile.program,
                    profile: Some(name.clone()),
                });
                Response::Activated(ActivationOutcome {
                    name,
                    hardware: applied.hardware.clone(),
                    applied: Some(applied),
                })
            }
            Err(error) => Response::Error(config_error(error)),
        }
    }

    fn delete_profile(&mut self, name: &str) -> Response {
        match self.config.delete_profile(name) {
            Ok(activated_instead) => {
                self.diagnostics.record(
                    crate::now_unix_ms(),
                    EventKind::ProfileDeleted {
                        name: name.to_string(),
                        by: self.actor.clone(),
                    },
                );
                Response::Deleted {
                    name: name.to_string(),
                    activated_instead,
                }
            }
            Err(error) => Response::Error(config_error(error)),
        }
    }

    /// Answer a handshake, and take the name the connection gave itself.
    ///
    /// The version is asked first, so a client of another protocol is told
    /// the two versions whatever else its handshake carried.
    fn hello(
        &mut self,
        connection: u64,
        protocol_version: u32,
        client: Option<String>,
    ) -> Response {
        if protocol_version != PROTOCOL_VERSION {
            return Response::Error(IpcError::UnsupportedProtocol {
                requested: protocol_version,
                supported: PROTOCOL_VERSION,
            });
        }
        if let Some(name) = &client
            && let Err(detail) = validate_client_name(name)
        {
            return Response::Error(IpcError::Malformed { detail });
        }
        if let Some(entry) = self.connections.get_mut(&connection) {
            entry.client = client;
        }
        Response::Hello {
            protocol_version: PROTOCOL_VERSION,
            daemon_version: DAEMON_VERSION.to_string(),
            connection,
        }
    }

    /// Accept a subscription, or say why not.
    ///
    /// Taking the slot here, under the same lock every other request takes,
    /// is what makes the ceiling exact: two connections subscribing at once
    /// cannot both be the eighth. The server gives the slot back through
    /// [`Self::unsubscribe`] when the stream ends, however it ends.
    fn subscribe(&mut self, topics: Vec<Topic>, interval_s: Option<u32>) -> Response {
        let mut topics = topics;
        topics.sort();
        topics.dedup();
        if topics.is_empty() {
            return Response::Error(IpcError::Malformed {
                detail: "topics is empty; name telemetry, status or events".to_string(),
            });
        }
        let interval_s = interval_s.unwrap_or(1);
        if !SUBSCRIBE_INTERVAL_S.contains(&interval_s) {
            return Response::Error(IpcError::Malformed {
                detail: format!(
                    "interval_s {interval_s} is outside {} to {} seconds",
                    SUBSCRIBE_INTERVAL_S.start(),
                    SUBSCRIBE_INTERVAL_S.end()
                ),
            });
        }
        if self.subscribers >= MAX_SUBSCRIPTIONS {
            return Response::Error(IpcError::SubscriptionLimit {
                max: MAX_SUBSCRIPTIONS,
            });
        }
        self.subscribers += 1;
        Response::Subscribed { topics, interval_s }
    }

    /// Give back the slot a subscription took.
    pub fn unsubscribe(&mut self) {
        self.subscribers = self.subscribers.saturating_sub(1);
    }

    /// Subscriptions currently served.
    pub fn subscribers(&self) -> u32 {
        self.subscribers
    }

    /// The newest sampling pass, as the `telemetry` request answers it.
    pub fn telemetry(&self) -> TelemetrySnapshot {
        self.sampler.snapshot()
    }

    /// Sequence number of the newest event recorded.
    pub fn latest_event_seq(&self) -> u64 {
        self.diagnostics.latest_seq()
    }

    /// The events after `cursor`, swept of secrets, at most `limit`.
    pub fn events_after(&self, cursor: u64, limit: u32) -> kori_core::diagnostics::EventPage {
        self.diagnostics.page(Some(cursor), limit)
    }

    /// Register a connection that passed the peer check, and number it.
    pub fn open_connection(&mut self, uid: u32, pid: i32) -> u64 {
        let number = self.next_connection;
        self.next_connection += 1;
        self.connections.insert(
            number,
            Connection {
                uid,
                pid,
                client: None,
                connected_at_unix_ms: crate::now_unix_ms(),
                announced: false,
            },
        );
        number
    }

    /// Drop a connection from the registry.
    ///
    /// The close is recorded only for a connection that earned a
    /// `client_accepted`, or when the connection was ended by a framing error,
    /// which is a fault worth keeping whoever made it. A read that came and
    /// went leaves the log as it found it.
    pub fn close_connection(&mut self, connection: u64, detail: String, framing_error: bool) {
        let Some(entry) = self.connections.remove(&connection) else {
            return;
        };
        if entry.announced || framing_error {
            self.diagnostics.record(
                crate::now_unix_ms(),
                EventKind::ClientDisconnected {
                    connection,
                    client: entry.client,
                    detail,
                },
            );
        }
    }

    /// Record a connection's first write, once.
    fn announce(&mut self, connection: u64) {
        let Some(entry) = self.connections.get_mut(&connection) else {
            return;
        };
        if entry.announced {
            return;
        }
        entry.announced = true;
        let event = EventKind::ClientAccepted {
            connection,
            client: entry.client.clone(),
            uid: entry.uid,
            pid: entry.pid,
        };
        self.diagnostics.record(crate::now_unix_ms(), event);
    }

    /// The attribution of what `connection` changes.
    ///
    /// A number the registry does not hold is attributed to the daemon rather
    /// than to an invented client: it only happens for a request handled
    /// outside a connection, which is the daemon acting for itself.
    fn attribution(&self, connection: u64) -> Attribution {
        match self.connections.get(&connection) {
            Some(entry) => Attribution::Client {
                connection,
                client: entry.client.clone(),
                pid: entry.pid,
            },
            None => Attribution::Daemon,
        }
    }

    /// Record a client that failed the local peer check.
    pub fn record_client_rejected(&mut self, reason: String) {
        self.diagnostics
            .record(crate::now_unix_ms(), EventKind::ClientRejected { reason });
    }
}

/// Why this daemon cannot write, when it cannot.
///
/// Two facts, kept apart: someone else holds the device, or ownership is
/// complete and nothing this user can write was found. They read the same on the
/// wire, because [`AccessMode::ReadOnly`] carries a list of conflicts and there
/// is no second shape for "the permission was never granted". That compromise is
/// made here, at the boundary, rather than everywhere the question is asked: the
/// daemon's own code never has to treat a missing udev rule as a process holding
/// a node.
enum ReadOnly {
    /// Another process holds a node, or a lock could not be taken.
    Held(Vec<OwnershipConflict>),
    /// Nothing is in the way and nothing is writable either, which is what an
    /// uninstalled udev rule looks like from in here.
    NothingWritable,
}

impl ReadOnly {
    /// The sentence every refusal carries.
    ///
    /// Built from the wire form so the reason a client is refused with and the
    /// detail its status shows cannot drift apart.
    fn reason(self) -> String {
        self.into_conflicts()
            .iter()
            .map(|conflict| conflict.detail.clone())
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// The wire form, where a missing permission has to borrow the conflict
    /// vocabulary. Its `device` is `None` because nobody holds it, and the
    /// resource it names is the interface an operator can actually act on.
    fn into_conflicts(self) -> Vec<OwnershipConflict> {
        match self {
            Self::Held(conflicts) => conflicts,
            Self::NothingWritable => vec![OwnershipConflict {
                device: None,
                resource: "hwmon".to_string(),
                detail: "No writable control attribute is available to this user. \
                         Check the installed udev rule."
                    .to_string(),
            }],
        }
    }
}

/// Refuse a capability the record does not carry as writable.
///
/// The reason comes from the record when it has one, because that is where the
/// "this firmware was never validated" refusal is written. A capability the
/// record does not mention at all is refused in its own words rather than
/// quietly treated as available.
fn require_writable(device: &DeviceRecord, capability: CapabilityId) -> Result<(), IpcError> {
    if device.can_write(capability) {
        return Ok(());
    }
    Err(IpcError::Incompatible {
        details: vec![Incompatibility {
            capability,
            reason: device
                .capability(capability)
                .and_then(|entry| entry.state.blocked_reason())
                .unwrap_or_else(|| {
                    format!(
                        "{} is absent from the capability record.",
                        capability.label()
                    )
                }),
        }],
    })
}

fn config_error(error: ConfigError) -> IpcError {
    match error {
        ConfigError::InvalidProfile { source, .. } => IpcError::Validation(source),
        ConfigError::MissingActiveProfile(name) => IpcError::ProfileNotFound { name },
        ConfigError::DuplicateProfile(name) => IpcError::ProfileNameUnavailable { name },
        other => IpcError::Io {
            detail: other.to_string(),
        },
    }
}

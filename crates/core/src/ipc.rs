// SPDX-FileCopyrightText: 2026 Arthur Jean
// SPDX-License-Identifier: GPL-3.0-or-later

//! The typed protocol spoken over the daemon's Unix socket.
//!
//! This file is the vocabulary: what either side may say and what every answer
//! carries. Anything that does not parse into [`Request`] is answered with a
//! typed [`IpcError`] and produces zero hardware writes.
//!
//! Two jobs done underneath it live in their own modules. `frame` is the
//! transport, newline-delimited JSON with a hard length ceiling on both ends,
//! and it knows nothing about what it carries. `paths` is where the socket
//! sits on this machine, which is filesystem layout rather than protocol. Both
//! are re-exported here, so every caller still reaches them through
//! `kori_core::ipc`.

use serde::{Deserialize, Serialize};

mod frame;
mod paths;

pub use frame::{FrameError, MAX_FRAME_BYTES, read_frame, write_frame};
pub use paths::{
    RUNTIME_DIR_ENV, SOCKET_FILE_NAME, SOCKET_PATH_ENV, runtime_dir, runtime_dir_from_env,
    socket_path_from_env,
};

use crate::DeviceId;
use crate::capability::{CapabilityId, CapabilityRecord, LcdPanel};
use crate::display::{DisplayError, DisplayPreset};
use crate::lighting::{LightingCommand, LightingError, LightingProgram};
use crate::profile::{Channel, CoolingProgram, Incompatibility, Profile, ValidationError};
use crate::telemetry::{PwmMode, TelemetrySnapshot};

/// Incremented on any breaking change to [`Request`] or [`Response`].
///
/// Negotiated by [`Request::Hello`] before anything else is sent, so a mismatch
/// is a refusal naming both versions rather than a frame one side misreads.
/// What each version changed, and which quiet disagreement the bump exists to
/// prevent, is recorded in `docs/schema-history.md`.
pub const PROTOCOL_VERSION: u32 = 10;

/// A request from the GPUI client to the daemon.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
// `deny_unknown_fields` is not supported on internally tagged enums, so strict
// field checking lives on the payload structs (`Profile`, `TemperatureCurve`).
#[serde(tag = "request", rename_all = "snake_case")]
pub enum Request {
    /// Negotiate the protocol version, and optionally name the client. Sent
    /// before anything else.
    ///
    /// `client` is a label the connection chooses for itself, checked by
    /// [`validate_client_name`]. It is not proof of anything: the process ID
    /// and the user a write is attributed to come from the kernel, and the name
    /// only makes the attribution readable.
    Hello {
        /// The protocol this client speaks. Any other than the daemon's own
        /// is refused with `unsupported_protocol`.
        protocol_version: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        client: Option<String>,
    },
    /// Current daemon, device and ownership state.
    Status,
    /// The versioned capability record for every allowlisted device.
    Capabilities,
    /// Every stored profile plus the active one.
    Profiles,
    /// Store a profile, replacing one with the same name.
    SaveProfile { profile: Profile },
    /// Make a stored profile active.
    ActivateProfile { name: String },
    /// Remove a stored profile. The safe profile is activated first when the
    /// deleted profile is the active one.
    DeleteProfile { name: String },
    /// The most recent sampling pass.
    Telemetry,
    /// Apply a cooling program directly, without saving it as a profile.
    ///
    /// This is what the Cooling screen's Apply activates. The daemon
    /// revalidates every value before the first write.
    ///
    /// With `expect_revision`, the write is refused with
    /// [`IpcError::Conflict`] unless the cooling revision is still that one.
    ApplyProgram {
        program: CoolingProgram,
        /// Refuse with `conflict` unless the domain is still at this revision.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        expect_revision: Option<u64>,
    },
    /// Apply a lighting program to one channel of the RGB controller.
    ///
    /// The daemon revalidates the program, checks it against the topology the
    /// controller reported, and enforces the command cadence before any byte
    /// is sent.
    ///
    /// `expect_revision` is the revision of the channel the command names.
    ApplyLighting {
        command: LightingCommand,
        /// Refuse with `conflict` unless the domain is still at this revision.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        expect_revision: Option<u64>,
    },
    /// Show a preset on the Kraken's panel.
    ///
    /// The preset travels, never the pixels: the daemon renders it with the
    /// same crate the client previews it with, so the two cannot drift, and a
    /// panel keeps updating after the window closes.
    ApplyDisplay {
        preset: DisplayPreset,
        /// Refuse with `conflict` unless the domain is still at this revision.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        expect_revision: Option<u64>,
    },
    /// Redacted diagnostics for an issue report.
    Diagnostics,
    /// A page of the event log.
    ///
    /// With `after_seq`, the events recorded after it, oldest first; without,
    /// the newest ones. `limit` defaults to
    /// [`crate::diagnostics::EVENTS_DEFAULT_LIMIT`] and may not exceed
    /// [`crate::diagnostics::EVENTS_MAX_LIMIT`].
    Events {
        /// Only the events recorded after this sequence number, oldest first.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        after_seq: Option<u64>,
        /// Most events in the page: default 100, at most 512.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        limit: Option<u32>,
    },
    /// Recorded telemetry, summarized and optionally as points.
    ///
    /// Every field is optional: `metrics` defaults to all of them, `window_s`
    /// to 900, `step_s` to 1 for a window up to 900 s and to 60 beyond, and
    /// `points` to false. The limits are in [`crate::history`].
    History {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        metrics: Option<Vec<crate::history::HistoryMetric>>,
        /// How far back, in seconds: default 900, at most 86400.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        window_s: Option<u32>,
        /// Seconds each point covers: 1 to 59 within the second tier, a
        /// whole number of minutes beyond.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        step_s: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        points: Option<bool>,
    },
    /// Turn this connection into a pushed stream.
    ///
    /// Answered with [`Response::Subscribed`]; from then on the connection
    /// carries only [`Push`] frames, and nothing the client sends is read. The
    /// client unsubscribes by closing the connection. `interval_s` (1 to 60,
    /// default 1) paces the telemetry push in ticks of the daemon's one-second
    /// clock. With `after_seq`, the events after it are pushed first.
    Subscribe {
        topics: Vec<Topic>,
        /// Seconds between two telemetry pushes, 1 to 60. Default 1.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        interval_s: Option<u32>,
        /// Push the events recorded after this sequence number first.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        after_seq: Option<u64>,
    },
}

/// What a subscription carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum Topic {
    /// A snapshot every `interval_s` ticks.
    Telemetry,
    /// The status once at once, then whenever it differs from the last one
    /// pushed on this connection.
    Status,
    /// Every event recorded after the cursor.
    Events,
}

/// Most subscriptions the daemon serves at once.
pub const MAX_SUBSCRIPTIONS: u32 = 8;

/// Frames one subscriber may have waiting before events are dropped.
pub const SUBSCRIBER_QUEUE: usize = 64;

/// Longest a push may wait on a subscriber that stopped reading.
pub const SUBSCRIBER_WRITE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// Longest a subscription goes without a frame: a heartbeat fills the gap.
pub const HEARTBEAT_INTERVAL: std::time::Duration = std::time::Duration::from_secs(10);

/// The range `interval_s` may take.
pub const SUBSCRIBE_INTERVAL_S: std::ops::RangeInclusive<u32> = 1..=60;

/// A frame the daemon pushes on a subscribed connection, tagged `push`.
///
/// A third tag beside `request` and `response`, so a reader can never take a
/// push for the answer to something it asked.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(tag = "push", rename_all = "snake_case")]
pub enum Push {
    Telemetry(Box<TelemetrySnapshot>),
    Status(Box<DaemonStatus>),
    Event(crate::diagnostics::DiagnosticEvent),
    /// Nothing else was written for [`HEARTBEAT_INTERVAL`]. A subscriber that
    /// sees neither this nor anything else for longer knows the daemon is gone.
    Heartbeat {
        /// When the heartbeat was sent, in milliseconds since the Unix epoch.
        at_unix_ms: u64,
    },
    /// Events this subscriber will never see: they left the ring before it
    /// could be sent them, or it fell [`SUBSCRIBER_QUEUE`] frames behind.
    Lagged {
        missed: u64,
    },
    /// The daemon is shutting down and closes this connection next.
    Closing {
        reason: String,
    },
}

impl Push {
    /// The name this frame travels under.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Telemetry(_) => "telemetry",
            Self::Status(_) => "status",
            Self::Event(_) => "event",
            Self::Heartbeat { .. } => "heartbeat",
            Self::Lagged { .. } => "lagged",
            Self::Closing { .. } => "closing",
        }
    }
}

impl Request {
    /// True for a request that may change the hardware or the stored
    /// configuration.
    ///
    /// Matched variant by variant rather than defaulted, so a request added
    /// later has to be classified by whoever adds it: a write that slipped into
    /// the read column would go unattributed and unannounced.
    pub fn writes(&self) -> bool {
        match self {
            Self::SaveProfile { .. }
            | Self::ActivateProfile { .. }
            | Self::DeleteProfile { .. }
            | Self::ApplyProgram { .. }
            | Self::ApplyLighting { .. }
            | Self::ApplyDisplay { .. } => true,
            Self::Hello { .. }
            | Self::Status
            | Self::Capabilities
            | Self::Profiles
            | Self::Telemetry
            | Self::Diagnostics
            | Self::Events { .. }
            | Self::History { .. }
            | Self::Subscribe { .. } => false,
        }
    }
}

/// A response from the daemon.
///
/// Not `Eq`: a telemetry snapshot carries floating-point readings, and an
/// equality that silently compared them would invite exactly the wrong kind of
/// assertion.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(tag = "response", rename_all = "snake_case")]
pub enum Response {
    Hello {
        /// The protocol the daemon speaks.
        protocol_version: u32,
        daemon_version: String,
        /// This connection's number, unique for the life of the daemon. It is
        /// what the daemon's events and revisions name this connection by.
        connection: u64,
    },
    Status(Box<DaemonStatus>),
    Capabilities(Box<CapabilityRecord>),
    Profiles {
        active: String,
        profiles: Vec<Profile>,
    },
    Activated(ActivationOutcome),
    Saved {
        name: String,
    },
    Deleted {
        name: String,
        activated_instead: Option<String>,
    },
    Telemetry(Box<TelemetrySnapshot>),
    Applied(Box<ApplyOutcome>),
    Lit(LightingOutcome),
    Shown(Box<DisplayOutcome>),
    Diagnostics(crate::diagnostics::DiagnosticsExport),
    Events(crate::diagnostics::EventPage),
    History(Box<crate::history::HistoryReply>),
    Subscribed {
        topics: Vec<Topic>,
        /// Seconds between two telemetry pushes.
        interval_s: u32,
    },
    Error(IpcError),
}

impl Response {
    /// The name this variant travels under, for an error that has to say what
    /// arrived instead of what was expected.
    ///
    /// The strings are the wire tags, so a mismatch reported to an operator
    /// names something they can find in a frame rather than a label invented
    /// for the message.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Hello { .. } => "hello",
            Self::Status(_) => "status",
            Self::Capabilities(_) => "capabilities",
            Self::Profiles { .. } => "profiles",
            Self::Activated(_) => "activated",
            Self::Saved { .. } => "saved",
            Self::Deleted { .. } => "deleted",
            Self::Telemetry(_) => "telemetry",
            Self::Applied(_) => "applied",
            Self::Lit(_) => "lit",
            Self::Shown(_) => "shown",
            Self::Diagnostics(_) => "diagnostics",
            Self::Events(_) => "events",
            Self::History(_) => "history",
            Self::Subscribed { .. } => "subscribed",
            Self::Error(_) => "error",
        }
    }
}

/// The result of one lighting command.
///
/// The controller acknowledges no state: there is no report that reads a
/// channel's current color back. The daemon is the sole writer, so its record
/// of what it committed is the only evidence of what the channel is showing,
/// which is the same reasoning the write-only curve attributes already follow.
/// `Confirmed` therefore means the controller accepted the report, and nothing
/// stronger is claimed anywhere.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct LightingOutcome {
    pub channel: u8,
    pub program: LightingProgram,
    pub hardware: HardwareState,
    /// Reports actually sent. Zero means the request matched the committed
    /// state and was deduplicated.
    pub writes: u32,
    pub deduplicated: bool,
}

/// The result of one panel command.
///
/// The panel acknowledges no picture: there is no report that reads back what
/// it is showing. The daemon is the sole writer, so its record of the last
/// preset it committed is the only evidence, exactly as for lighting.
/// `Confirmed` means the transfer completed, and nothing stronger.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct DisplayOutcome {
    pub preset: DisplayPreset,
    pub hardware: HardwareState,
    /// Frames actually sent. Zero means the request matched what the panel is
    /// already showing and was deduplicated.
    pub frames: u32,
    /// True when the picture was already on the glass and no frame went out.
    ///
    /// This is about the picture alone. A command can be deduplicated here and
    /// still have changed the panel, because the brightness is not a picture:
    /// see `brightness_sent`.
    pub deduplicated: bool,
    /// True when the display-control report went out for this command.
    ///
    /// The brightness is a panel setting rather than a pixel, so it survives
    /// the frame comparison. Without this field a preset whose only edit was
    /// the brightness would be reported as "nothing was sent" by a client that
    /// had just changed what the operator is looking at.
    pub brightness_sent: bool,
}

/// What the panel is currently showing, as far as the daemon knows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct DisplayState {
    /// Geometry a frame must match, when the panel answered at all.
    pub panel: Option<LcdPanel>,
    /// The last preset the daemon committed, when it has committed one.
    pub committed: Option<DisplayPreset>,
    /// True while a preset that reads telemetry is being streamed.
    pub streaming: bool,
    #[serde(flatten)]
    pub revision: Revision,
    /// Why the stream stopped, once a transfer failed.
    ///
    /// [`DisplayState::streaming`] cannot say this on its own: it is equally
    /// false for a preset that reads no telemetry and for a panel that was
    /// never written. The daemon retries a faulted stream on a widening wait
    /// and clears this only once a frame lands, so a fault still standing is a
    /// panel still frozen, and a screen that cannot see it cannot offer the
    /// operator the faster way back.
    pub faulted: Option<String>,
    /// Frames dropped because a transfer was still in flight when the next
    /// sample arrived. The streamer keeps at most one frame pending, so this
    /// counts what that ceiling discarded.
    pub dropped_frames: u64,
    /// Frames the panel took since the daemon started.
    ///
    /// The steady state of the stream lives here rather than in the event log:
    /// a healthy frame a second is not an event, and recording each one used to
    /// push every other event out of the ring within minutes.
    pub frames_sent: u64,
    /// Transfers that failed since the daemon started.
    pub failed_transfers: u64,
    /// When the panel last took a frame, if it ever did.
    /// In milliseconds since the Unix epoch.
    pub last_frame_at_unix_ms: Option<u64>,
    /// When the animation the panel is playing started, while one plays.
    ///
    /// The daemon walks an animation on the wall clock from this instant, a
    /// late tick skipping the frames it slept through, so the frame on the
    /// glass at any moment is this time's distance into the picture's own
    /// delays. A window that stopped its preview while hidden resumes it there
    /// instead of at the first frame. `None` for a still picture and for every
    /// other mode.
    /// In milliseconds since the Unix epoch.
    pub animation_started_at_unix_ms: Option<u64>,
}

/// What one lighting channel is currently showing, as far as the daemon knows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ChannelState {
    pub channel: u8,
    /// Accessory names the controller reported, in slot order.
    pub accessories: Vec<String>,
    /// The last program the daemon committed, when it has committed one.
    pub committed: Option<LightingProgram>,
    #[serde(flatten)]
    pub revision: Revision,
}

/// What actually happened to the hardware when a profile was activated.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ActivationOutcome {
    pub name: String,
    pub hardware: HardwareState,
    /// The write the activation performed, when it performed one.
    pub applied: Option<ApplyOutcome>,
}

/// The result of one cooling write, with the evidence behind it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ApplyOutcome {
    pub hardware: HardwareState,
    /// Number of kernel attributes actually written.
    ///
    /// Zero means the request matched the confirmed state and was
    /// deduplicated, which is what a repeated Apply must do.
    pub writes: u32,
    /// True when nothing was written because the state already matched.
    pub deduplicated: bool,
    /// What the kernel reported after the write, per channel.
    pub readback: Vec<ChannelReadback>,
}

impl ApplyOutcome {
    /// The outcome of a program that touches no hardware.
    pub fn untouched(hardware: HardwareState) -> Self {
        Self {
            hardware,
            writes: 0,
            deduplicated: false,
            readback: Vec::new(),
        }
    }

    pub fn readback_for(&self, channel: Channel) -> Option<&ChannelReadback> {
        self.readback.iter().find(|entry| entry.channel == channel)
    }
}

/// What one channel reported after a write.
///
/// Every field is optional because a readback the kernel does not provide is
/// recorded as missing rather than assumed to match what was written.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ChannelReadback {
    pub channel: Channel,
    pub mode: Option<PwmMode>,
    /// Duty the kernel reports after the write, 0 to 255.
    pub duty: Option<u8>,
    /// Curve points that read back identical to what was written.
    pub curve_points_confirmed: Option<u16>,
    /// Set when the readback contradicts the value that was written.
    pub mismatch: Option<String>,
}

impl ChannelReadback {
    pub fn new(channel: Channel) -> Self {
        Self {
            channel,
            mode: None,
            duty: None,
            curve_points_confirmed: None,
            mismatch: None,
        }
    }

    pub fn is_confirmed(&self) -> bool {
        self.mismatch.is_none() && (self.mode.is_some() || self.duty.is_some())
    }
}

/// How much of the hardware state the daemon can vouch for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum HardwareState {
    /// The device is running its own firmware program and nothing was written.
    Onboard,
    /// The daemon wrote the program and read it back.
    Confirmed,
    /// The profile was selected but not written, and `reason` says why.
    NotApplied { reason: String },
    /// A write started and could not be confirmed. No further write is sent
    /// until a readback succeeds.
    Uncertain { reason: String },
}

/// Longest name a client may give itself in [`Request::Hello`].
pub const MAX_CLIENT_NAME_LEN: usize = 32;

/// Check a client name: 1 to [`MAX_CLIENT_NAME_LEN`] characters, each a
/// lowercase ASCII letter, a digit, `.`, `_` or `-`.
///
/// Narrow on purpose. The name is printed in events, in `status` and on the
/// window's "Changed by" line, so it must not be able to carry a newline, a
/// control character or a look-alike of another client's name.
pub fn validate_client_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("client name is empty".to_string());
    }
    if name.len() > MAX_CLIENT_NAME_LEN {
        return Err(format!(
            "client name is {} characters long; the limit is {MAX_CLIENT_NAME_LEN}",
            name.len()
        ));
    }
    if let Some(refused) = name
        .chars()
        .find(|c| !(c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-')))
    {
        return Err(format!(
            "client name contains {refused:?}; only a-z, 0-9, '.', '_' and '-' are allowed"
        ));
    }
    Ok(())
}

/// Who caused a change: a connection, or the daemon itself.
///
/// The process ID is the kernel's answer (`SO_PEERCRED`), taken when the
/// connection was accepted. The client name is whatever that connection said
/// in its handshake, so it labels the write rather than proving who made it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(tag = "origin", rename_all = "snake_case")]
pub enum Attribution {
    Client {
        connection: u64,
        client: Option<String>,
        pid: i32,
    },
    /// The daemon acting on its own, such as restoring the operator's last
    /// state at startup.
    Daemon,
}

/// Where a write domain stands: how many accepted writes it has taken since
/// the daemon started, and who made the last one.
///
/// 1 is the state the daemon restored at startup, attributed to the daemon.
/// Each accepted write command adds one; the frames the stream sends on its own
/// do not. A client that read revision `n` and writes with
/// `expect_revision: n` is refused if anything wrote in between, rather than
/// silently overwriting it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Revision {
    pub revision: u64,
    pub changed_by: Attribution,
}

impl Revision {
    /// The revision of a domain the daemon has not been asked to change.
    pub fn initial() -> Self {
        Self {
            revision: 1,
            changed_by: Attribution::Daemon,
        }
    }
}

/// A write domain a revision belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum WriteDomain {
    Cooling,
    /// One lighting channel. Each has its own revision.
    Lighting,
    Display,
}

/// The thermal program, as `status` reports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct CoolingState {
    /// The program the daemon last committed, when it committed one.
    ///
    /// A duty can be read back from the driver, but a curve cannot: the device
    /// publishes no attribute that returns the shape it was given. The
    /// daemon's own record is therefore the only place a curve exists, and a
    /// client that could not see it would open on a plot the machine is not
    /// running and make the operator draw theirs again.
    pub committed: Option<CoolingProgram>,
    #[serde(flatten)]
    pub revision: Revision,
}

/// One open connection, as `status` lists it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ConnectedClient {
    pub connection: u64,
    /// The name the connection gave itself, when it gave one.
    pub client: Option<String>,
    /// From the kernel, not from the client.
    pub pid: i32,
    /// When the connection was accepted, in milliseconds since the Unix epoch.
    pub connected_at_unix_ms: u64,
}

/// Whether the daemon may write at all.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum AccessMode {
    ReadWrite,
    /// Writes are refused. `conflicts` lists the evidence.
    ReadOnly {
        conflicts: Vec<OwnershipConflict>,
    },
}

impl AccessMode {
    pub fn is_read_only(&self) -> bool {
        matches!(self, Self::ReadOnly { .. })
    }
}

/// Another process or a permission problem preventing ownership.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct OwnershipConflict {
    pub device: Option<DeviceId>,
    /// Path or resource the conflict was observed on.
    pub resource: String,
    pub detail: String,
}

/// Per-device state the client renders.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct DeviceStatus {
    pub id: DeviceId,
    pub present: bool,
    /// The daemon holds the exclusive per-device lock.
    pub owned: bool,
    /// Capabilities that may be written right now.
    pub writable: Vec<CapabilityId>,
    /// Operator-facing reason each unavailable capability is blocked.
    pub blocked: Vec<BlockedCapability>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct BlockedCapability {
    pub capability: CapabilityId,
    pub reason: String,
}

/// How the on-disk configuration was resolved at startup.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ConfigState {
    /// No configuration existed; built-in defaults are active.
    Defaults,
    /// A valid configuration was loaded.
    Loaded,
    /// The configuration could not be loaded. The unreadable file was kept at
    /// `preserved_path` and `recovery_action` names what the operator can do.
    Recovered {
        detail: String,
        preserved_path: String,
        recovery_action: String,
    },
}

impl ConfigState {
    /// One actionable sentence, or `None` when nothing needs attention.
    pub fn recovery_message(&self) -> Option<String> {
        match self {
            Self::Defaults | Self::Loaded => None,
            Self::Recovered {
                detail,
                preserved_path,
                recovery_action,
            } => Some(format!(
                "Configuration could not be loaded ({detail}). Safe defaults are active. \
                 The previous file was kept at {preserved_path}. {recovery_action}"
            )),
        }
    }
}

/// Everything the client needs to decide what to enable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct DaemonStatus {
    pub daemon_version: String,
    pub protocol_version: u32,
    /// When this daemon started. Revisions, connection numbers, event
    /// sequence numbers and the telemetry history all restart with it, so a
    /// client that sees this change knows every number it held is void.
    /// In milliseconds since the Unix epoch.
    pub started_at_unix_ms: u64,
    /// Every open connection, the asking one included.
    pub clients: Vec<ConnectedClient>,
    pub access: AccessMode,
    pub devices: Vec<DeviceStatus>,
    pub active_profile: String,
    pub config: ConfigState,
    /// The thermal program the daemon last committed, and its revision.
    pub cooling: CoolingState,
    /// Lighting channels the controller reported, empty when it reported none.
    pub lighting: Vec<ChannelState>,
    /// The panel's geometry and what it was last told to show.
    pub display: DisplayState,
    /// Path of the Unix socket. The daemon opens no other listening endpoint.
    pub socket_path: String,
}

/// Every way a request can be refused, with enough detail to render it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(tag = "error", rename_all = "snake_case")]
pub enum IpcError {
    #[error("Client protocol {requested} is not supported. This daemon speaks {supported}.")]
    UnsupportedProtocol { requested: u32, supported: u32 },
    #[error("Request could not be parsed: {detail}")]
    Malformed { detail: String },
    #[error("Request exceeds the {max_bytes} byte frame limit.")]
    FrameTooLarge {
        /// The frame ceiling, in bytes.
        max_bytes: usize,
    },
    #[error("Request rejected before the protocol handshake completed.")]
    HandshakeRequired,
    #[error("Local peer rejected: {reason}")]
    PeerRejected { reason: String },
    #[error("{0}")]
    Validation(#[from] ValidationError),
    #[error("{0}")]
    Lighting(#[from] LightingError),
    #[error("{0}")]
    Display(#[from] DisplayError),
    #[error("Controls are read-only: {reason}")]
    ReadOnly { reason: String },
    #[error("Profile is incompatible with the connected hardware.")]
    Incompatible { details: Vec<Incompatibility> },
    #[error("No profile named {name}.")]
    ProfileNotFound { name: String },
    #[error("The name {name} is reserved for the built-in safe profile.")]
    ProfileNameUnavailable { name: String },
    #[error("No supported device is present.")]
    NoDevice,
    #[error("Local operation failed: {detail}")]
    Io { detail: String },
    #[error("The {domain:?} state changed since revision {expected}; it is at {current} now.")]
    Conflict {
        domain: WriteDomain,
        /// The lighting channel, for a lighting conflict.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        channel: Option<u8>,
        expected: u64,
        current: u64,
        changed_by: Attribution,
    },
    #[error("The daemon already serves {max} subscriptions.")]
    SubscriptionLimit { max: u32 },
    #[error("History request is out of range: {detail}")]
    HistoryOutOfRange {
        limit: crate::history::HistoryLimit,
        detail: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    /// something the client did not ask for, so it has to be the tag the frame
    /// actually carries. The two are separate hand-written mirrors of the same
    /// variant list, and only this makes them agree.
    #[test]
    fn every_response_kind_is_the_tag_it_travels_under() {
        let status = DaemonStatus {
            daemon_version: "0.1.0".into(),
            protocol_version: PROTOCOL_VERSION,
            started_at_unix_ms: 0,
            clients: Vec::new(),
            access: AccessMode::ReadWrite,
            devices: Vec::new(),
            active_profile: "Onboard safe".into(),
            config: ConfigState::Defaults,
            cooling: CoolingState {
                committed: None,
                revision: Revision::initial(),
            },
            lighting: Vec::new(),
            display: DisplayState {
                panel: None,
                committed: None,
                streaming: false,
                revision: Revision::initial(),
                faulted: None,
                dropped_frames: 0,
                frames_sent: 0,
                failed_transfers: 0,
                last_frame_at_unix_ms: None,
                animation_started_at_unix_ms: None,
            },
            socket_path: "/run/user/1000/kori/kori.sock".into(),
        };
        let record = CapabilityRecord {
            schema_version: crate::capability::CAPABILITY_SCHEMA_VERSION,
            context: crate::capability::ProbeContext {
                kernel_release: crate::capability::Evidenced::unknown("not read", "test"),
                probed_at_unix_ms: 0,
            },
            devices: Vec::new(),
            rejected: Vec::new(),
        };
        let preset = DisplayPreset::default_infographic();

        let responses = vec![
            Response::Hello {
                protocol_version: PROTOCOL_VERSION,
                daemon_version: "0.1.0".into(),
                connection: 1,
            },
            Response::Status(Box::new(status)),
            Response::Capabilities(Box::new(record)),
            Response::Profiles {
                active: "Onboard safe".into(),
                profiles: Vec::new(),
            },
            Response::Activated(ActivationOutcome {
                name: "Onboard safe".into(),
                hardware: HardwareState::Onboard,
                applied: None,
            }),
            Response::Saved {
                name: "Silent".into(),
            },
            Response::Deleted {
                name: "Silent".into(),
                activated_instead: None,
            },
            Response::Telemetry(Box::new(TelemetrySnapshot::unavailable(
                0,
                crate::telemetry::Unavailable::absent("no hwmon"),
            ))),
            Response::Applied(Box::new(ApplyOutcome::untouched(HardwareState::Onboard))),
            Response::Lit(LightingOutcome {
                channel: 1,
                program: LightingProgram::Off,
                hardware: HardwareState::Confirmed,
                writes: 1,
                deduplicated: false,
            }),
            Response::Shown(Box::new(DisplayOutcome {
                preset,
                hardware: HardwareState::Confirmed,
                frames: 1,
                deduplicated: false,
                brightness_sent: true,
            })),
            Response::Diagnostics(
                crate::diagnostics::DiagnosticsLog::default().export(0, "0.1.0", None),
            ),
            Response::Events(crate::diagnostics::DiagnosticsLog::default().page(None, 10)),
            Response::Subscribed {
                topics: vec![Topic::Events],
                interval_s: 1,
            },
            Response::History(Box::new(crate::history::HistoryReply {
                end_unix_ms: 0,
                step_s: 1,
                slots: 0,
                available_s: 0,
                metrics: std::collections::BTreeMap::new(),
            })),
            Response::Error(IpcError::NoDevice),
        ];

        let mut seen: Vec<&'static str> = Vec::with_capacity(responses.len());
        for response in &responses {
            let encoded = serde_json::to_value(response).unwrap();
            assert_eq!(
                encoded.get("response").and_then(serde_json::Value::as_str),
                Some(response.kind()),
                "{} does not travel under its own kind",
                response.kind()
            );
            assert!(
                !seen.contains(&response.kind()),
                "{} is claimed by two variants",
                response.kind()
            );
            seen.push(response.kind());
        }
    }

    #[test]
    fn a_client_name_is_short_lowercase_and_printable() {
        for good in ["kori", "korictl", "claude-code", "agent_1.2", "a"] {
            assert_eq!(validate_client_name(good), Ok(()), "{good}");
        }
        let long = "a".repeat(MAX_CLIENT_NAME_LEN + 1);
        assert!(validate_client_name(&"a".repeat(MAX_CLIENT_NAME_LEN)).is_ok());
        for bad in [
            "",
            long.as_str(),
            "Kori",
            "two words",
            "line\nbreak",
            "é",
            "a/b",
        ] {
            assert!(validate_client_name(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn attribution_travels_in_the_documented_shape() {
        let by = Attribution::Client {
            connection: 7,
            client: Some("korictl".into()),
            pid: 4242,
        };
        assert_eq!(
            serde_json::to_value(&by).unwrap(),
            serde_json::json!({"origin":"client","connection":7,"client":"korictl","pid":4242})
        );
        assert_eq!(
            serde_json::to_value(Attribution::Daemon).unwrap(),
            serde_json::json!({"origin":"daemon"})
        );
    }

    #[test]
    fn a_push_travels_under_its_own_tag_and_round_trips() {
        let mut log = crate::diagnostics::DiagnosticsLog::default();
        log.record(
            7,
            crate::diagnostics::EventKind::ProfileSaved {
                name: "Silent".into(),
                by: Attribution::Daemon,
            },
        );
        let event = log.events().next().unwrap().clone();
        let pushes = vec![
            Push::Telemetry(Box::new(TelemetrySnapshot::unavailable(
                0,
                crate::telemetry::Unavailable::absent("no hwmon"),
            ))),
            Push::Event(event),
            Push::Heartbeat { at_unix_ms: 1 },
            Push::Lagged { missed: 3 },
            Push::Closing {
                reason: "received SIGTERM".into(),
            },
        ];
        for push in pushes {
            let encoded = serde_json::to_value(&push).unwrap();
            assert_eq!(encoded["push"], push.kind());
            assert!(encoded.get("response").is_none());
            let decoded: Push = serde_json::from_value(encoded).unwrap();
            assert_eq!(decoded, push);
        }
    }

    #[test]
    fn config_recovery_message_names_the_preserved_file() {
        let state = ConfigState::Recovered {
            detail: "expected schema 1, found 9".into(),
            preserved_path: "/home/a/.config/kori/config.toml.corrupt".into(),
            recovery_action: "Re-save a profile to replace it.".into(),
        };
        let message = state.recovery_message().unwrap();
        assert!(message.contains(".corrupt"), "{message}");
        assert!(message.contains("Safe defaults are active"), "{message}");
        assert!(ConfigState::Loaded.recovery_message().is_none());
    }
}

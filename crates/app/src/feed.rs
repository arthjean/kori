// SPDX-FileCopyrightText: 2026 Arthur Jean
// SPDX-License-Identifier: GPL-3.0-or-later

//! The window's two connections to the daemon.
//!
//! Every request runs on a worker thread, never on the UI thread. A local
//! socket is fast, but "fast" is not "bounded": a daemon that stalls would
//! otherwise freeze the window for the length of the client timeout, and a
//! frozen window cannot show the operator that anything is wrong.
//!
//! One connection is subscribed to `telemetry` and `status`, and the daemon
//! pushes on it: a sample every tick, the status whenever it changed. A reader
//! thread blocks on it and hands each push to the worker. The other connection
//! carries commands, the history the Monitoring chart asks for, and the reads a
//! connection needs once. The worker owns both and blocks on a single queue
//! until a push or a command arrives, so it wakes when there is something to
//! do and not otherwise: no `status` and `telemetry` every second, no 25 ms
//! slices spent looking at an empty queue.
//!
//! Commands keep their order and their numbering: they run one at a time on
//! the command connection, in the order the screen queued them, and each one's
//! outcome carries the next sequence number. Each is followed by a `status`
//! read on that same connection, published with the outcome, so the window
//! never holds an outcome without the revision it produced.
//!
//! A dead daemon is noticed within a heartbeat: the daemon writes on a quiet
//! subscription every ten seconds, and the read on it gives up a second after
//! that. A daemon stopping in order says so with a `closing` push, which is
//! noticed at once.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded};
use kori_core::client::{Client, ClientError, DEFAULT_TIMEOUT, SubscriptionCloser};
use kori_core::display::DisplayPreset;
use kori_core::history::{HistoryMetric, HistoryReply};
use kori_core::ipc::{
    ApplyOutcome, Attribution, DaemonStatus, HardwareState, IpcError, LightingOutcome, Push, Topic,
};
use kori_core::lighting::LightingCommand;
use kori_core::profile::{CoolingProgram, Profile};
use kori_core::telemetry::TelemetrySnapshot;

use crate::link::LinkState;

/// Something the operator asked the hardware to do.
///
/// Each write carries the revision the screen's editor was built from. The
/// daemon refuses it with `conflict` if another client wrote in between, which
/// is what stops the window from silently writing over an agent's change.
#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    /// Write a cooling program without saving it as a profile.
    Apply {
        program: CoolingProgram,
        expect_revision: Option<u64>,
    },
    /// Write one channel's lighting without saving it as a profile.
    ApplyLighting {
        command: LightingCommand,
        expect_revision: Option<u64>,
    },
    /// Show a preset on the panel without saving it as a profile.
    ApplyDisplay {
        preset: DisplayPreset,
        expect_revision: Option<u64>,
    },
    SaveProfile(Profile),
    ActivateProfile(String),
    DeleteProfile(String),
}

impl Command {
    /// Whether running this can change the stored profile list.
    ///
    /// A property of what was asked rather than of how it ended. Reading the
    /// list back after a refused save costs one request nothing is waiting on,
    /// where threading the answer out of every execution path meant a positional
    /// flag on fourteen returns, most of them a constant `false`.
    fn touches_profiles(&self) -> bool {
        matches!(
            self,
            Self::SaveProfile(_) | Self::ActivateProfile(_) | Self::DeleteProfile(_)
        )
    }

    /// What this command is addressed to.
    fn subject(&self) -> CommandSubject {
        match self {
            Self::Apply { .. } => CommandSubject::Cooling,
            Self::ApplyLighting { command, .. } => CommandSubject::Lighting(command.channel),
            Self::ApplyDisplay { .. } => CommandSubject::Panel,
            Self::SaveProfile(_) | Self::ActivateProfile(_) | Self::DeleteProfile(_) => {
                CommandSubject::Profiles
            }
        }
    }
}

/// What a command was addressed to.
///
/// One slot publishes every outcome, and without this the window had no way to
/// tell a refused lighting command from a refused cooling one. Two defects came
/// out of that: the Cooling screen showed the Lighting screen's refusals and
/// the other way round, and a lighting outcome cleared `sent`, the guard that
/// keeps two cooling writes from being in flight at once, so the next cooling
/// outcome could land against the wrong program.
///
/// It also carries the device's name. Three message builders used to prepend it
/// by hand, in four different shapes, because the type had lost the fact.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandSubject {
    Cooling,
    Lighting(u8),
    Panel,
    /// A stored profile: saved, activated or deleted.
    Profiles,
}

impl CommandSubject {
    /// What names the device in front of the message.
    ///
    /// `None` where the screen showing the note has one device and the message
    /// is already about it: the Cooling screen writes to one cooler, and a
    /// profile names itself in the sentence the daemon answered with.
    pub fn device(self) -> Option<String> {
        match self {
            Self::Cooling | Self::Profiles => None,
            Self::Lighting(channel) => Some(format!("Channel {channel}")),
            Self::Panel => Some("Panel".to_string()),
        }
    }

    /// Whether the Cooling screen is where this outcome belongs.
    pub fn is_cooling(self) -> bool {
        matches!(self, Self::Cooling | Self::Profiles)
    }

    /// Whether the Lighting screen is, which carries the controller's channels
    /// and the panel.
    pub fn is_appearance(self) -> bool {
        matches!(self, Self::Lighting(_) | Self::Panel)
    }
}

/// How a command ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutcomeSeverity {
    /// The hardware confirmed it.
    Confirmed,
    /// It was accepted but the hardware state is not confirmed.
    Unconfirmed,
    /// It was refused, and nothing was written.
    Refused,
}

/// How urgent a reported hardware state is.
///
/// One mapping for the three executors rather than one per executor. The three
/// used to repeat these five arms verbatim and only the sentence differed, which
/// meant a sixth [`HardwareState`] could be given three different severities by
/// being added to two of the three matches.
impl From<&HardwareState> for OutcomeSeverity {
    fn from(hardware: &HardwareState) -> Self {
        match hardware {
            // Onboard is a success: the device is running its own program
            // because that is what was asked for, and nothing was written
            // because nothing needed to be.
            HardwareState::Confirmed | HardwareState::Onboard => Self::Confirmed,
            HardwareState::Uncertain { .. } => Self::Unconfirmed,
            HardwareState::NotApplied { .. } => Self::Refused,
        }
    }
}

/// How one command ended, before it is told apart from any other.
///
/// No device name and no identity: the sentence says what happened, the
/// [`CommandSubject`] says to what, and the worker stamps the sequence. The
/// three executors used to fold all three into the message, which is why two of
/// them prepended `"Panel: "` and `"Channel 2: "` by hand on the failure paths
/// and inside the sentence on the success ones.
#[derive(Debug, Clone, PartialEq)]
struct Report {
    message: String,
    severity: OutcomeSeverity,
    /// Who wrote first, when the daemon refused the command with `conflict`.
    conflict: Option<Attribution>,
}

impl Report {
    fn refused(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            severity: OutcomeSeverity::Refused,
            conflict: None,
        }
    }

    fn confirmed(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            severity: OutcomeSeverity::Confirmed,
            conflict: None,
        }
    }

    /// A write that failed, told apart from a conflict.
    ///
    /// A conflict is not an error the operator caused: another client wrote
    /// first, and the window is about to show what it wrote. The sentence says
    /// that rather than quoting revision numbers, and the attribution travels
    /// with it so the shell can name who it was.
    fn failed(error: ClientError) -> Self {
        match error {
            ClientError::Refused(IpcError::Conflict { changed_by, .. }) => Self {
                message: format!(
                    "{} before this edit was sent, so the edit was dropped.",
                    changed_by_sentence(&changed_by)
                ),
                severity: OutcomeSeverity::Refused,
                conflict: Some(changed_by),
            },
            other => Self::refused(other.to_string()),
        }
    }

    /// One sentence about a state the hardware reported, at that state's own
    /// severity.
    ///
    /// The severity is never passed in: it is a property of the state, and
    /// [`OutcomeSeverity::from`] is the one place it is decided.
    fn from_hardware(hardware: &HardwareState, message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            severity: OutcomeSeverity::from(hardware),
            conflict: None,
        }
    }

    /// The outcome of one panel command.
    ///
    /// The panel answers no report that reads its picture back, so a confirmed
    /// command says the transfer completed and claims nothing about the glass.
    ///
    /// A deduplicated frame is not the same as an untouched panel: the
    /// brightness is a setting rather than a picture and travels whatever the
    /// pixels do, so the two are reported separately instead of collapsed into
    /// one "nothing was sent" that would be false exactly when the operator had
    /// just changed the brightness.
    fn from_display(outcome: &kori_core::ipc::DisplayOutcome) -> Self {
        let message = match &outcome.hardware {
            HardwareState::Confirmed if outcome.deduplicated && outcome.brightness_sent => format!(
                "Brightness set to {}%. The picture was already correct, so no frame was sent.",
                outcome.preset.brightness.percent()
            ),
            HardwareState::Confirmed if outcome.deduplicated => {
                "Already showing this. Nothing was sent.".to_string()
            }
            HardwareState::Confirmed => format!(
                "Set to {}. The transfer completed; the panel reports no picture to read back.",
                outcome.preset.mode.label()
            ),
            HardwareState::Onboard => "Keeping its own picture. Nothing was sent.".to_string(),
            HardwareState::NotApplied { reason } => reason.clone(),
            HardwareState::Uncertain { reason } => format!("Uncertain: {reason}"),
        };
        Self::from_hardware(&outcome.hardware, message)
    }

    fn from_lighting(outcome: &LightingOutcome) -> Self {
        let message = match &outcome.hardware {
            HardwareState::Confirmed if outcome.deduplicated => {
                "Already showing this. Nothing was sent.".to_string()
            }
            HardwareState::Confirmed => format!(
                "Set to {}. The controller accepted the command; it reports no state to read back.",
                outcome.program.summary()
            ),
            HardwareState::Onboard => {
                "The controller keeps its own program. Nothing was sent.".to_string()
            }
            HardwareState::NotApplied { reason } => reason.clone(),
            HardwareState::Uncertain { reason } => format!("Uncertain: {reason}"),
        };
        Self::from_hardware(&outcome.hardware, message)
    }

    fn from_apply(outcome: &ApplyOutcome) -> Self {
        let message = match &outcome.hardware {
            HardwareState::Confirmed if outcome.deduplicated => {
                "Already applied. The hardware already held this program, so nothing was written."
                    .to_string()
            }
            HardwareState::Confirmed => format!(
                "Applied and confirmed by readback. {} kernel attributes written.",
                outcome.writes
            ),
            HardwareState::Onboard => {
                "The device keeps running its own program. Nothing was written.".to_string()
            }
            HardwareState::NotApplied { reason } | HardwareState::Uncertain { reason } => {
                reason.clone()
            }
        };
        Self::from_hardware(&outcome.hardware, message)
    }
}

/// The result of the last command, shown on the screen the command came from.
#[derive(Debug, Clone, PartialEq)]
pub struct CommandOutcome {
    /// Which command produced this, counted by the worker.
    ///
    /// What tells one outcome from the next. Comparing the whole value would
    /// hide a repeat: two refusals of the same edit for the same reason are
    /// equal, and treating the second as "already seen" leaves the in-flight
    /// guard set on a write that will never be answered again.
    pub sequence: u64,
    pub subject: CommandSubject,
    pub message: String,
    pub severity: OutcomeSeverity,
    /// Who wrote first, when the daemon refused the command with `conflict`.
    pub conflict: Option<Attribution>,
}

/// "Changed by" and who, as the window writes it under a control.
///
/// The name is the label the client gave itself; a connection that gave none
/// is "another client" rather than a number the operator cannot place.
pub fn changed_by_sentence(by: &Attribution) -> String {
    match by {
        Attribution::Client {
            client: Some(name), ..
        } => format!("Changed by {name}"),
        Attribution::Client { client: None, .. } => "Changed by another client".to_string(),
        Attribution::Daemon => "Changed by the background service".to_string(),
    }
}

impl CommandOutcome {
    /// The message with the device it is about in front of it, when the screen
    /// showing it carries more than one.
    pub fn sentence(&self) -> String {
        match self.subject.device() {
            Some(device) => format!("{device}: {}", self.message),
            None => self.message.clone(),
        }
    }
}

/// The temperatures the Monitoring chart draws, in legend order.
pub const TREND_METRICS: [HistoryMetric; 3] = [
    HistoryMetric::CpuTemperature,
    HistoryMetric::GpuTemperature,
    HistoryMetric::LiquidTemperature,
];

/// The window the chart covers, in seconds: the whole second tier.
pub const TREND_WINDOW_S: u32 = 900;

/// The width of one chart point, in seconds.
pub const TREND_STEP_S: u32 = 5;

/// What the last `history` request came back with.
#[derive(Debug, Clone, PartialEq)]
pub enum HistoryState {
    Answered(Arc<HistoryReply>),
    /// Refused or unreachable, in operator language.
    Refused(String),
}

/// What the worker has published.
#[derive(Debug, Clone, Default)]
struct Shared {
    link: Option<LinkState>,
    outcome: Option<CommandOutcome>,
    /// The latest history answer, and how many have been published.
    history: Option<(u64, HistoryState)>,
    /// Telemetry pushes received since the worker started.
    samples: u64,
    /// `history` requests sent to the daemon since the worker started.
    history_requests: u64,
}

/// What the worker waits for. One queue carries all of it, so a single
/// blocking receive is the whole of waiting.
enum Event {
    Command(Command),
    History,
    /// A push from the reader of subscription `generation`.
    Push {
        generation: u64,
        push: Push,
    },
    /// The reader of subscription `generation` lost its connection.
    Lost {
        generation: u64,
        error: ClientError,
    },
    Stop,
}

/// A running worker.
pub struct Feed {
    shared: Arc<Mutex<Shared>>,
    events: Sender<Event>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl Feed {
    /// Start the worker and return it with the notifier the view awaits.
    ///
    /// `interval` is how often a sample is pushed, rounded to the whole seconds
    /// the protocol counts in, and how long the worker waits between two
    /// attempts to reach a daemon that is not answering.
    pub fn spawn(socket: PathBuf, interval: Duration) -> (Self, UnboundedReceiver<()>) {
        let shared = Arc::new(Mutex::new(Shared::default()));
        let (events, inbox) = channel();
        let (notifier, notifications) = unbounded();

        let worker = std::thread::Builder::new()
            .name("kori-feed".to_string())
            .spawn({
                let shared = Arc::clone(&shared);
                let events = events.clone();
                move || {
                    Worker {
                        socket,
                        interval,
                        shared,
                        events,
                        notifier,
                        session: None,
                        queued: VecDeque::new(),
                        sequence: 0,
                        generation: 0,
                        history_answers: 0,
                    }
                    .run(inbox)
                }
            })
            .ok();

        (
            Self {
                shared,
                events,
                worker,
            },
            notifications,
        )
    }

    /// The latest published state, or `None` before the first cycle.
    pub fn link(&self) -> Option<LinkState> {
        self.shared.lock().ok()?.link.clone()
    }

    /// The result of the most recent command.
    pub fn outcome(&self) -> Option<CommandOutcome> {
        self.shared.lock().ok()?.outcome.clone()
    }

    /// The latest history answer, numbered so a repeat can be told from a
    /// new one.
    pub fn history(&self) -> Option<(u64, HistoryState)> {
        self.shared.lock().ok()?.history.clone()
    }

    /// Telemetry pushes received since the worker started.
    pub fn samples(&self) -> u64 {
        self.shared.lock().map(|shared| shared.samples).unwrap_or(0)
    }

    /// `history` requests the worker sent since it started.
    pub fn history_requests(&self) -> u64 {
        self.shared
            .lock()
            .map(|shared| shared.history_requests)
            .unwrap_or(0)
    }

    /// Queue a command. It runs on the worker, in order.
    pub fn send(&self, command: Command) {
        let _ = self.events.send(Event::Command(command));
    }

    /// Ask for the temperatures the Monitoring chart draws.
    ///
    /// Answered on the command connection and published beside the link,
    /// never as a command outcome: it is the window reading, not the operator
    /// acting, and it must not take a sequence number from one.
    pub fn request_history(&self) {
        let _ = self.events.send(Event::History);
    }
}

impl Drop for Feed {
    fn drop(&mut self) {
        let _ = self.events.send(Event::Stop);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl std::fmt::Debug for Feed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Feed")
            .field("connected", &self.link().is_some())
            .finish()
    }
}

/// The name the window gives itself in the handshake.
pub const WINDOW_CLIENT_NAME: &str = "kori";

/// Everything the worker holds while a daemon is answering.
struct Session {
    /// The command connection.
    client: Client,
    capabilities: Arc<kori_core::capability::CapabilityRecord>,
    profiles: Arc<[Profile]>,
    status: Arc<DaemonStatus>,
    telemetry: Option<Arc<TelemetrySnapshot>>,
    /// Which subscription the pushes of this session come from.
    generation: u64,
    closer: SubscriptionCloser,
    reader: Option<std::thread::JoinHandle<()>>,
}

impl Session {
    fn link(&self) -> LinkState {
        LinkState::Connected {
            status: Arc::clone(&self.status),
            capabilities: Arc::clone(&self.capabilities),
            profiles: Arc::clone(&self.profiles),
            telemetry: self.telemetry.clone(),
            connection: self.client.connection(),
        }
    }
}

impl Drop for Session {
    /// Closing the subscription is unsubscribing; it also returns the reader
    /// from the read it is blocked in, so it can be joined at once.
    fn drop(&mut self) {
        self.closer.close();
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

struct Worker {
    socket: PathBuf,
    interval: Duration,
    shared: Arc<Mutex<Shared>>,
    /// A sender into the worker's own queue, handed to each reader thread.
    events: Sender<Event>,
    notifier: UnboundedSender<()>,
    session: Option<Session>,
    /// Commands that arrived while no daemon answered, run on reconnection in
    /// the order they were queued.
    queued: VecDeque<Command>,
    /// What tells one published outcome from the next.
    sequence: u64,
    /// Bumped for every subscription, so a push from a reader that belonged
    /// to a connection already dropped is recognized and ignored.
    generation: u64,
    history_answers: u64,
}

impl Worker {
    fn run(mut self, inbox: Receiver<Event>) {
        loop {
            if self.session.is_none() {
                self.connect();
            }
            if self.session.is_some() {
                while let Some(command) = self.queued.pop_front() {
                    self.execute(command);
                }
            }

            // With a session, nothing happens until a push or a command
            // arrives. Without one, the wait is also the retry delay.
            let event = if self.session.is_some() {
                inbox.recv().map_err(|_| RecvTimeoutError::Disconnected)
            } else {
                inbox.recv_timeout(self.interval)
            };
            match event {
                Ok(Event::Stop) | Err(RecvTimeoutError::Disconnected) => break,
                Err(RecvTimeoutError::Timeout) => {}
                Ok(Event::Command(command)) => {
                    if self.session.is_some() {
                        self.execute(command);
                    } else {
                        self.queued.push_back(command);
                    }
                }
                Ok(Event::History) => self.history(),
                Ok(Event::Push { generation, push }) => {
                    if self.is_current(generation) {
                        self.push(push);
                    }
                }
                Ok(Event::Lost { generation, error }) => {
                    if self.is_current(generation) {
                        self.lose(&error);
                    }
                }
            }
        }
        // Dropped before the worker returns, so the reader is joined while the
        // window is still waiting on this thread.
        self.session = None;
    }

    fn is_current(&self, generation: u64) -> bool {
        self.session
            .as_ref()
            .is_some_and(|session| session.generation == generation)
    }

    fn notify(&self) {
        let _ = self.notifier.unbounded_send(());
    }

    fn publish(&self, change: impl FnOnce(&mut Shared)) {
        if let Ok(mut shared) = self.shared.lock() {
            change(&mut shared);
        }
        self.notify();
    }

    fn publish_link(&self) {
        let link = self.session.as_ref().map(Session::link);
        self.publish(|shared| shared.link = link);
    }

    /// Drop the session and show the operator that the daemon is gone.
    fn lose(&mut self, error: &ClientError) {
        self.session = None;
        let message = error.operator_message();
        self.publish(|shared| shared.link = Some(LinkState::Unavailable { message }));
    }

    fn connect(&mut self) {
        self.generation += 1;
        match open_session(&self.socket, self.interval, self.generation, &self.events) {
            Ok(session) => {
                self.session = Some(session);
                self.publish_link();
            }
            Err(error) => {
                let message = error.operator_message();
                self.publish(|shared| shared.link = Some(LinkState::Unavailable { message }));
            }
        }
    }

    fn push(&mut self, push: Push) {
        let Some(session) = self.session.as_mut() else {
            return;
        };
        match push {
            Push::Telemetry(snapshot) => {
                session.telemetry = Some(Arc::new(*snapshot));
                let link = session.link();
                self.publish(|shared| {
                    shared.link = Some(link);
                    shared.samples += 1;
                });
            }
            Push::Status(status) => {
                session.status = Arc::new(*status);
                self.publish_link();
            }
            // The stream dropped frames for this reader. Telemetry and status
            // are kept newest-first by the daemon, so one fresh status read is
            // all it takes to be current again.
            Push::Lagged { .. } => match session.client.status() {
                Ok(status) => {
                    session.status = Arc::new(status);
                    self.publish_link();
                }
                Err(error) => self.lose(&error),
            },
            Push::Closing { .. } => {
                self.session = None;
                self.publish(|shared| {
                    shared.link = Some(LinkState::Unavailable {
                        message: "The background service stopped. Controls return when it is \
                                  running again."
                            .to_string(),
                    })
                });
            }
            // A heartbeat only proves the daemon is alive, which the read
            // that returned it already did. Events are not subscribed to.
            Push::Heartbeat { .. } | Push::Event(_) => {}
        }
    }

    fn history(&mut self) {
        let answer = match self.session.as_mut() {
            Some(session) => {
                if let Ok(mut shared) = self.shared.lock() {
                    shared.history_requests += 1;
                }
                match session.client.history(
                    Some(TREND_METRICS.to_vec()),
                    Some(TREND_WINDOW_S),
                    Some(TREND_STEP_S),
                    true,
                ) {
                    Ok(reply) => Ok(HistoryState::Answered(Arc::new(reply))),
                    Err(ClientError::Refused(error)) => Ok(HistoryState::Refused(format!(
                        "The background service refused the temperature history: {error}"
                    ))),
                    Err(error) => Err(error),
                }
            }
            None => Ok(HistoryState::Refused(
                "No temperature history while the background service is not running.".to_string(),
            )),
        };
        match answer {
            Ok(state) => {
                self.history_answers += 1;
                let numbered = (self.history_answers, state);
                self.publish(|shared| shared.history = Some(numbered));
            }
            Err(error) => self.lose(&error),
        }
    }

    /// Run one command, then read the status it left behind, and publish both.
    fn execute(&mut self, command: Command) {
        let Some(session) = self.session.as_mut() else {
            self.queued.push_back(command);
            return;
        };
        let refresh_profiles = command.touches_profiles();
        let subject = command.subject();
        let report = execute(session, command);
        self.sequence += 1;
        let outcome = CommandOutcome {
            sequence: self.sequence,
            subject,
            message: report.message,
            severity: report.severity,
            conflict: report.conflict,
        };

        if refresh_profiles && let Ok(profiles) = session.client.profiles() {
            session.profiles = profiles.1.into();
        }
        let status = session.client.status();
        match status {
            Ok(status) => {
                session.status = Arc::new(status);
                let link = session.link();
                self.publish(|shared| {
                    shared.link = Some(link);
                    shared.outcome = Some(outcome);
                });
            }
            Err(error) => {
                self.publish(|shared| shared.outcome = Some(outcome));
                self.lose(&error);
            }
        }
    }
}

/// Open both connections, read what a connection reads once, and start the
/// reader of the subscription.
///
/// The capability record is not re-read afterwards. It changes on hotplug, and
/// a hotplug drops the connection, which is what triggers a fresh read.
fn open_session(
    socket: &std::path::Path,
    interval: Duration,
    generation: u64,
    events: &Sender<Event>,
) -> Result<Session, ClientError> {
    // Both connections name themselves, so what the window writes reads
    // "kori" in the events and in `status`, beside the process ID the kernel
    // vouches for.
    let mut client = Client::connect_as(socket, DEFAULT_TIMEOUT, Some(WINDOW_CLIENT_NAME))?;
    let capabilities = Arc::new(client.capabilities()?);
    let profiles: Arc<[Profile]> = client.profiles()?.1.into();
    let status = Arc::new(client.status()?);
    // One sample now, so the dials do not wait a tick for their first push.
    let telemetry = Some(Arc::new(client.telemetry()?));

    let interval_s = u32::try_from(interval.as_secs())
        .unwrap_or(u32::MAX)
        .clamp(1, 60);
    let mut subscription = Client::connect_as(socket, DEFAULT_TIMEOUT, Some(WINDOW_CLIENT_NAME))?
        .subscribe(
        vec![Topic::Telemetry, Topic::Status],
        Some(interval_s),
        None,
    )?;
    let closer = subscription.closer()?;
    let events = events.clone();
    let reader = std::thread::Builder::new()
        .name("kori-feed-stream".to_string())
        .spawn(move || {
            loop {
                match subscription.next_push() {
                    Ok(push) => {
                        let closing = matches!(push, Push::Closing { .. });
                        if events.send(Event::Push { generation, push }).is_err() || closing {
                            return;
                        }
                    }
                    Err(error) => {
                        let _ = events.send(Event::Lost { generation, error });
                        return;
                    }
                }
            }
        })
        .map_err(|error| ClientError::Disconnected(kori_core::ipc::FrameError::Io(error)))?;

    Ok(Session {
        client,
        capabilities,
        profiles,
        status,
        telemetry,
        generation,
        closer,
        reader: Some(reader),
    })
}

/// Run one command and report how it ended.
///
/// Nothing here names the device. That is what [`Command::subject`] already
/// answered, and the window prints it in front of the sentence: two of these
/// arms used to prepend it by hand on the failure path only, so the same fact
/// was stated in four shapes and missing from three.
fn execute(session: &mut Session, command: Command) -> Report {
    match command {
        Command::Apply {
            program,
            expect_revision,
        } => match session.client.apply_expecting(program, expect_revision) {
            Ok(outcome) => Report::from_apply(&outcome),
            Err(error) => Report::failed(error),
        },
        Command::ApplyLighting {
            command,
            expect_revision,
        } => match session
            .client
            .apply_lighting_expecting(command, expect_revision)
        {
            Ok(outcome) => Report::from_lighting(&outcome),
            Err(error) => Report::failed(error),
        },
        Command::ApplyDisplay {
            preset,
            expect_revision,
        } => match session
            .client
            .apply_display_expecting(preset, expect_revision)
        {
            Ok(outcome) => Report::from_display(&outcome),
            Err(error) => Report::failed(error),
        },
        Command::SaveProfile(profile) => {
            let name = profile.name.clone();
            match session.client.save_profile(profile) {
                Ok(_) => Report::confirmed(format!("Profile {name} saved.")),
                Err(error) => refused(error),
            }
        }
        Command::ActivateProfile(name) => match session.client.activate_profile(name) {
            Ok(activation) => {
                let mut report = match &activation.applied {
                    Some(applied) => Report::from_apply(applied),
                    None => Report::from_hardware(
                        &activation.hardware,
                        "Profile activated.".to_string(),
                    ),
                };
                // The profile is named in the sentence rather than by the
                // subject: which of several stored arrangements ran is the
                // whole content of the message, not the device it reached.
                report.message = format!("{}: {}", activation.name, report.message);
                report
            }
            Err(error) => refused(error),
        },
        Command::DeleteProfile(name) => match session.client.delete_profile(name) {
            Ok((name, activated_instead)) => Report::confirmed(match activated_instead {
                Some(safe) => format!("{safe} activated before {name} was deleted."),
                None => format!("Profile {name} deleted."),
            }),
            Err(error) => refused(error),
        },
    }
}

/// How a profile command that did not succeed is reported.
///
/// An answer of the wrong kind keeps the sentence the window has always shown
/// for it: the wording `ClientError` uses names wire tags, which is right for a
/// command line and means nothing to an operator reading a status line.
fn refused(error: ClientError) -> Report {
    match error {
        ClientError::Unexpected { .. } => Report::refused(
            "The background service answered something this build does not understand.",
        ),
        other => Report::refused(other.to_string()),
    }
}

/// Milliseconds since the Unix epoch.
pub fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use kori_core::ipc::{ChannelReadback, DisplayOutcome};
    use kori_core::lighting::Brightness;
    use kori_core::profile::Channel;
    use std::time::Instant;

    /// The window's writes read "kori" in the daemon's events because this is
    /// the handshake the running feed sends. A listener stands in for the
    /// daemon and reads the first frame the worker writes.
    #[test]
    fn the_running_feed_names_itself_kori_in_its_handshake() {
        use std::io::BufRead;

        let socket =
            std::env::temp_dir().join(format!("kori-app-feed-name-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&socket);
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let (feed, _notifications) = Feed::spawn(socket.clone(), Duration::from_secs(5));

        let (stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut line = String::new();
        std::io::BufReader::new(stream)
            .read_line(&mut line)
            .unwrap();
        let hello: serde_json::Value = serde_json::from_str(&line).unwrap();
        drop(feed);
        let _ = std::fs::remove_file(&socket);

        assert_eq!(hello["request"], "hello");
        assert_eq!(hello["protocol_version"], kori_core::ipc::PROTOCOL_VERSION);
        assert_eq!(hello["client"], WINDOW_CLIENT_NAME);
        assert_eq!(WINDOW_CLIENT_NAME, "kori");
    }

    use crate::testing::harness::{Harness, fixed};
    use crate::testing::{Proxy, wait_until};

    const PATIENCE: Duration = Duration::from_secs(10);

    fn kinds(frames: &[serde_json::Value]) -> Vec<String> {
        frames
            .iter()
            .map(|frame| frame["request"].as_str().unwrap_or("?").to_string())
            .collect()
    }

    fn connected(feed: &Feed) -> bool {
        matches!(feed.link(), Some(LinkState::Connected { .. }))
    }

    /// One subscribed connection and one for commands, and no polling: over a
    /// second of pushes the command connection sends nothing beyond what a
    /// connection reads once, and the samples still arrive.
    #[test]
    fn the_worker_reads_each_connection_once_and_then_only_listens() {
        let harness = Harness::start("feed-listens");
        let proxy = Proxy::start("listens", harness.socket());
        let (feed, _notifications) =
            Feed::spawn(proxy.path().to_path_buf(), Duration::from_secs(1));

        // A push per daemon tick: forty milliseconds on the fake machine.
        wait_until("a second of pushed samples", PATIENCE, || {
            feed.samples() >= 25
        });

        assert_eq!(
            proxy.connections(),
            2,
            "one command and one subscribed connection"
        );
        assert_eq!(
            kinds(&proxy.sent(0)),
            ["hello", "capabilities", "profiles", "status", "telemetry"],
            "the command connection reads what a connection reads once, and stops"
        );
        let stream = proxy.sent(1);
        assert_eq!(kinds(&stream), ["hello", "subscribe"]);
        assert_eq!(
            stream[1]["topics"],
            serde_json::json!(["telemetry", "status"])
        );
        for hello in [&proxy.sent(0)[0], &stream[0]] {
            assert_eq!(hello["client"], WINDOW_CLIENT_NAME);
        }

        // Profiles are read again after a command that touched them, and the
        // capability record is not.
        feed.send(Command::SaveProfile(fixed("Feed listens", 180, 120)));
        wait_until("the save to answer", PATIENCE, || {
            feed.outcome().is_some_and(|outcome| outcome.sequence == 1)
        });
        assert_eq!(
            kinds(&proxy.sent(0))[5..],
            ["save_profile", "profiles", "status"]
        );
        assert_eq!(proxy.requests("capabilities").len(), 1);
        assert!(connected(&feed));
    }

    /// Outcomes arrive in the order the commands were queued, numbered from
    /// one, and each write carries the revision it expects.
    #[test]
    fn commands_keep_their_order_numbering_and_expected_revision() {
        let harness = Harness::start("feed-order");
        let proxy = Proxy::start("order", harness.socket());
        let (feed, _notifications) =
            Feed::spawn(proxy.path().to_path_buf(), Duration::from_secs(1));
        wait_until("the window to connect", PATIENCE, || connected(&feed));

        for name in ["First", "Second", "Third"] {
            feed.send(Command::SaveProfile(fixed(name, 180, 120)));
        }
        feed.send(Command::Apply {
            program: CoolingProgram::Fixed {
                pump: 200,
                fan: 120,
            },
            expect_revision: Some(1),
        });
        wait_until("four outcomes", PATIENCE, || {
            feed.outcome().is_some_and(|outcome| outcome.sequence == 4)
        });

        let saved: Vec<String> = proxy
            .requests("save_profile")
            .iter()
            .map(|frame| frame["profile"]["name"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(saved, ["First", "Second", "Third"]);
        let applied = proxy.requests("apply_program");
        assert_eq!(applied.len(), 1);
        assert_eq!(applied[0]["expect_revision"], 1);
        let outcome = feed.outcome().unwrap();
        assert_eq!(outcome.subject, CommandSubject::Cooling);
        assert_eq!(outcome.severity, OutcomeSeverity::Confirmed, "{outcome:?}");
        // The status read after the write is published with its outcome, so
        // the window never holds an outcome without the revision it produced.
        let status = feed.link().unwrap().status().unwrap().clone();
        assert_eq!(status.cooling.revision.revision, 2);
    }

    /// A write that expected a revision another client already moved is
    /// refused with `conflict`, and the outcome names who wrote first.
    #[test]
    fn a_conflict_is_reported_with_who_wrote_first() {
        let harness = Harness::start("feed-conflict");
        let (feed, _notifications) =
            Feed::spawn(harness.socket().to_path_buf(), Duration::from_secs(1));
        wait_until("the window to connect", PATIENCE, || connected(&feed));

        let mut agent =
            Client::connect_as(harness.socket(), DEFAULT_TIMEOUT, Some("korictl")).unwrap();
        agent
            .apply(CoolingProgram::Fixed {
                pump: 190,
                fan: 110,
            })
            .unwrap();
        feed.send(Command::Apply {
            program: CoolingProgram::Fixed {
                pump: 220,
                fan: 130,
            },
            expect_revision: Some(1),
        });
        wait_until("the refusal", PATIENCE, || feed.outcome().is_some());

        let outcome = feed.outcome().unwrap();
        assert_eq!(outcome.severity, OutcomeSeverity::Refused);
        assert!(
            matches!(&outcome.conflict, Some(Attribution::Client { client: Some(name), .. }) if name == "korictl"),
            "{outcome:?}"
        );
        assert!(
            outcome.message.starts_with("Changed by korictl"),
            "{}",
            outcome.message
        );
    }

    /// A `lagged` push costs one status read on the command connection, and
    /// the stream carries on.
    #[test]
    fn a_lagged_stream_reads_status_once_and_carries_on() {
        let harness = Harness::start("feed-lagged");
        let proxy = Proxy::start("lagged", harness.socket());
        let (feed, _notifications) =
            Feed::spawn(proxy.path().to_path_buf(), Duration::from_secs(1));
        wait_until("pushed samples", PATIENCE, || feed.samples() >= 3);
        let before = proxy.sent(0).len();

        proxy.inject(1, r#"{"push":"lagged","missed":3}"#);
        wait_until("the status read", PATIENCE, || {
            proxy.sent(0).len() == before + 1
        });
        assert_eq!(kinds(&proxy.sent(0)[before..]), ["status"]);

        let samples = feed.samples();
        wait_until("the stream to carry on", PATIENCE, || {
            feed.samples() > samples + 3
        });
        assert_eq!(
            proxy.sent(0).len(),
            before + 1,
            "one read, not one per sample"
        );
        assert!(connected(&feed));
    }

    /// A daemon stopping in order says so, and the window shows it at once;
    /// the daemon that replaces it is found again without restarting anything.
    #[test]
    fn a_closing_daemon_is_unavailable_at_once_and_its_successor_is_reconnected() {
        let mut first = Harness::start("feed-closing-first");
        let proxy = Proxy::start("closing", first.socket());
        let (feed, _notifications) =
            Feed::spawn(proxy.path().to_path_buf(), Duration::from_secs(1));
        wait_until("the window to connect", PATIENCE, || connected(&feed));

        let stopped = Instant::now();
        first.stop();
        wait_until("the unavailable state", PATIENCE, || {
            matches!(feed.link(), Some(LinkState::Unavailable { .. }))
        });
        assert!(
            stopped.elapsed() < Duration::from_secs(2),
            "the closing push took {:?} to show",
            stopped.elapsed()
        );

        let second = Harness::start("feed-closing-second");
        proxy.retarget(second.socket());
        let started_at = Client::connect(second.socket())
            .unwrap()
            .status()
            .unwrap()
            .started_at_unix_ms;
        wait_until("the successor", PATIENCE, || {
            feed.link()
                .and_then(|link| link.status().map(|status| status.started_at_unix_ms))
                == Some(started_at)
        });
        let samples = feed.samples();
        wait_until("its stream", PATIENCE, || feed.samples() > samples + 3);
        // Both connections were opened again: a command connection that read
        // the successor's capabilities, and a new subscription.
        let reopened: Vec<Vec<String>> = (2..proxy.connections())
            .map(|index| kinds(&proxy.sent(index)))
            .collect();
        assert!(
            reopened
                .iter()
                .any(|frames| frames.starts_with(&["hello".into(), "capabilities".into()])),
            "{reopened:?}"
        );
        assert!(
            reopened
                .iter()
                .any(|frames| frames == &["hello", "subscribe"]),
            "{reopened:?}"
        );
    }

    /// A daemon that stops answering without a word is noticed within one
    /// missed heartbeat: the subscription's read gives up a second after the
    /// ten seconds the daemon lets a quiet stream go without one.
    #[test]
    fn a_silent_daemon_is_unavailable_within_one_missed_heartbeat() {
        let harness = Harness::start("feed-silent");
        let proxy = Proxy::start("silent", harness.socket());
        let (feed, _notifications) =
            Feed::spawn(proxy.path().to_path_buf(), Duration::from_secs(1));
        wait_until("pushed samples", PATIENCE, || feed.samples() >= 3);

        let frozen = Instant::now();
        proxy.freeze();
        wait_until("the unavailable state", Duration::from_secs(15), || {
            matches!(feed.link(), Some(LinkState::Unavailable { .. }))
        });
        let waited = frozen.elapsed();
        assert!(
            waited <= Duration::from_millis(11_500),
            "a silent daemon took {waited:?} to be noticed"
        );
        assert!(
            waited >= Duration::from_secs(9),
            "noticed before any heartbeat was due"
        );
    }

    /// Every command says what it was addressed to, which is what routes its
    /// outcome to a screen and keeps it away from another screen's in-flight
    /// guard.
    #[test]
    fn every_command_names_the_screen_its_outcome_belongs_to() {
        let cooling = Command::Apply {
            program: CoolingProgram::Onboard,
            expect_revision: None,
        };
        assert_eq!(cooling.subject(), CommandSubject::Cooling);
        assert!(cooling.subject().is_cooling());
        assert!(!cooling.subject().is_appearance());

        let lighting = Command::ApplyLighting {
            command: LightingCommand {
                channel: 2,
                program: kori_core::lighting::LightingProgram::Off,
            },
            expect_revision: None,
        };
        assert_eq!(lighting.subject(), CommandSubject::Lighting(2));
        assert!(lighting.subject().is_appearance());
        assert!(!lighting.subject().is_cooling());

        let panel = Command::ApplyDisplay {
            preset: kori_core::display::DisplayPreset::default_infographic(),
            expect_revision: None,
        };
        assert_eq!(panel.subject(), CommandSubject::Panel);
        assert!(panel.subject().is_appearance());

        // A profile writes cooling among other things, so its outcome belongs
        // on the Cooling screen; it is still not the cooling write whose reply
        // the shell is holding a program for.
        let profile = Command::ActivateProfile("Silent".to_string());
        assert_eq!(profile.subject(), CommandSubject::Profiles);
        assert!(profile.subject().is_cooling());
        assert!(!profile.subject().is_appearance());
    }

    /// The Lighting screen shows one note for four rows that all write on their
    /// own, so a sentence that does not say which device it is about would be
    /// read against whichever row the operator is looking at.
    #[test]
    fn a_note_names_its_device_exactly_once() {
        let outcome = |subject, message: &str| CommandOutcome {
            sequence: 1,
            subject,
            message: message.to_string(),
            severity: OutcomeSeverity::Refused,
            conflict: None,
        };

        assert_eq!(
            outcome(CommandSubject::Lighting(2), "Already showing this.").sentence(),
            "Channel 2: Already showing this."
        );
        assert_eq!(
            outcome(CommandSubject::Panel, "Keeping its own picture.").sentence(),
            "Panel: Keeping its own picture."
        );
        // The Cooling screen writes to one cooler, so nothing is prepended.
        assert_eq!(
            outcome(CommandSubject::Cooling, "Applied.").sentence(),
            "Applied."
        );
        assert_eq!(
            outcome(CommandSubject::Profiles, "Profile Silent saved.").sentence(),
            "Profile Silent saved."
        );

        // And the executors leave the naming to the subject: no message may
        // carry the device itself, or the note would say it twice.
        for report in [
            Report::from_lighting(&LightingOutcome {
                channel: 2,
                program: kori_core::lighting::LightingProgram::Off,
                hardware: HardwareState::Confirmed,
                writes: 1,
                deduplicated: true,
            }),
            Report::from_display(&DisplayOutcome {
                preset: kori_core::display::DisplayPreset::default_infographic(),
                hardware: HardwareState::Confirmed,
                frames: 1,
                deduplicated: false,
                brightness_sent: false,
            }),
        ] {
            assert!(
                !report.message.starts_with("Channel ") && !report.message.starts_with("Panel"),
                "the message names its own device: {}",
                report.message
            );
        }
    }

    #[test]
    fn a_brightness_only_change_is_reported_as_applied_rather_than_as_nothing() {
        let preset = kori_core::display::DisplayPreset {
            brightness: Brightness::new(20).unwrap(),
            ..kori_core::display::DisplayPreset::default_infographic()
        };
        let dimmed = DisplayOutcome {
            preset,
            hardware: HardwareState::Confirmed,
            frames: 0,
            deduplicated: true,
            brightness_sent: true,
        };

        let reported = Report::from_display(&dimmed);
        assert_eq!(reported.severity, OutcomeSeverity::Confirmed);
        assert!(reported.message.contains("20%"), "{}", reported.message);
        assert!(
            !reported.message.contains("Nothing was sent"),
            "a panel that was just dimmed was told nothing happened: {}",
            reported.message
        );

        // A command that changed neither the picture nor the setting still says
        // so, which is the case the message was written for.
        let untouched = DisplayOutcome {
            brightness_sent: false,
            ..dimmed
        };
        assert!(
            Report::from_display(&untouched)
                .message
                .contains("Nothing was sent")
        );
    }

    fn report(hardware: HardwareState, writes: u32, deduplicated: bool) -> Report {
        Report::from_apply(&ApplyOutcome {
            hardware,
            writes,
            deduplicated,
            readback: vec![ChannelReadback::new(Channel::Pump)],
        })
    }

    #[test]
    fn a_confirmed_write_reports_how_many_attributes_it_touched() {
        let reported = report(HardwareState::Confirmed, 4, false);
        assert_eq!(reported.severity, OutcomeSeverity::Confirmed);
        assert!(reported.message.contains('4'), "{}", reported.message);
    }

    #[test]
    fn a_deduplicated_write_says_nothing_was_written() {
        let reported = report(HardwareState::Confirmed, 0, true);
        assert_eq!(reported.severity, OutcomeSeverity::Confirmed);
        assert!(
            reported.message.contains("nothing was written"),
            "{}",
            reported.message
        );
    }

    #[test]
    fn an_uncertain_state_is_neither_a_success_nor_a_plain_refusal() {
        let reported = report(
            HardwareState::Uncertain {
                reason: "pwm2 could not be read back".into(),
            },
            2,
            false,
        );
        assert_eq!(reported.severity, OutcomeSeverity::Unconfirmed);
        assert!(reported.message.contains("read back"));
    }

    #[test]
    fn a_refusal_carries_the_reason_the_daemon_gave() {
        let reported = report(
            HardwareState::NotApplied {
                reason: "Pump duty 3 is outside the accepted range 51-255.".into(),
            },
            0,
            false,
        );
        assert_eq!(reported.severity, OutcomeSeverity::Refused);
        assert!(reported.message.contains("51-255"));
    }

    #[test]
    fn the_onboard_program_is_reported_as_untouched_hardware() {
        let reported = report(HardwareState::Onboard, 0, false);
        assert_eq!(reported.severity, OutcomeSeverity::Confirmed);
        assert!(reported.message.contains("Nothing was written"));
    }
}

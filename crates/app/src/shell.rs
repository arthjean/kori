// SPDX-FileCopyrightText: 2026 Arthur Jean
// SPDX-License-Identifier: GPL-3.0-or-later

//! The application shell: Paneflow's cockpit frame around one work surface.
//!
//! A title bar across the whole window, a navigation rail on the shell color
//! under it, and the main panel inset beside the rail as a rounded card. Three
//! primary destinations, one secondary Settings entry, and nothing else. The
//! rail never scrolls and never changes width, so the panel has a known width
//! to lay out against at the 920x640 target size.
//!
//! The window holds no hardware handle and no telemetry history. It folds in
//! what the worker publishes and repaints only when the visible screen would
//! paint something different, and every write control is gated on what the
//! daemon reported.
//!
//! What the shell owns is deliberately grouped rather than spread flat. Every
//! screen module writes into this one value, so a field it can reach is a field
//! any of them can put in a state the others do not expect: the open popover
//! was closed by hand in eleven places across seven files before it had an
//! owner. What is left is the frame, the three editors, and the bookkeeping of
//! edits leaving the process.

use std::time::Instant;

use gpui::{
    App, Context, Div, FocusHandle, Focusable, KeyBinding, MouseButton, SharedString, Stateful,
    Window, actions, div, prelude::*, px,
};
use kori_core::profile::CoolingProgram;
use kori_core::telemetry::KrakenTelemetry;

use crate::assets::Icon;
use crate::components::{
    Note, NoteLevel, focus_ring, focus_visible, icon, set_focus_visible, squircle,
};
use crate::cooling::CoolingEditor;
use crate::display::DisplayScreen;
use crate::feed::{
    CommandOutcome, CommandSubject, Feed, HistoryState, OutcomeSeverity, now_unix_ms,
};
use crate::lighting::LightingEditor;
use crate::link::LinkState;
use crate::metrics::MetricBook;
use crate::shell::screen::drag::{Drag, Interaction};
use crate::shell::screen::monitoring::Trend;
use crate::shell::screen::row::LightingRow;
use crate::shell::screen::write::{WriteSchedule, WriteTarget, write_is_held_back};
use crate::shell::screen::{Disclosure, Popover};
use crate::shell::sync::{Domain, Revisions};
use crate::shell::visible::Visible;
use crate::theme::{
    BLOCK_GAP, COLUMN_MAX_WIDTH, COLUMN_PADDING, FOCUS_RING, NAV_FONT, NAV_ICON_SIZE, NAV_ROW_GAP,
    NAV_ROW_HEIGHT, NAV_ROW_MARGIN, NAV_ROW_PADDING_X, NAV_ROW_RADIUS, PANEL_INSET, PANEL_RADIUS,
    RAIL_WIDTH, UI_FONT, color, space, text,
};
use crate::window_chrome::{self, DragLatch};

pub mod screen;
pub(crate) mod sync;
pub(crate) mod visible;

/// How often the Monitoring chart asks for the daemon's history while shown.
///
/// The chart's points are five seconds wide, so asking more often would fetch
/// the same points again, and asking less often would leave the newest one
/// missing for longer than it takes to record it.
pub const HISTORY_REFRESH: std::time::Duration = std::time::Duration::from_secs(5);

actions!(
    shell,
    [
        FocusNext,
        FocusPrevious,
        GoMonitoring,
        GoCooling,
        GoLighting,
        GoSettings,
        ClosePopover,
    ]
);

/// Key bindings, registered once at startup.
pub fn key_bindings() -> Vec<KeyBinding> {
    vec![
        KeyBinding::new("tab", FocusNext, None),
        KeyBinding::new("shift-tab", FocusPrevious, None),
        KeyBinding::new("ctrl-1", GoMonitoring, None),
        KeyBinding::new("ctrl-2", GoCooling, None),
        KeyBinding::new("ctrl-3", GoLighting, None),
        KeyBinding::new("ctrl-comma", GoSettings, None),
        KeyBinding::new("escape", ClosePopover, None),
    ]
}

/// The only destinations this product has.
///
/// The panel has no destination of its own. It is one device's appearance
/// among the others, so it lives on Lighting next to the channels of the
/// controller, which is also where an operator looking for "what my hardware
/// shows" goes first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Destination {
    Monitoring,
    Cooling,
    Lighting,
    Settings,
}

impl Destination {
    /// Primary destinations, in rail order.
    pub const PRIMARY: [Destination; 3] = [
        Destination::Monitoring,
        Destination::Cooling,
        Destination::Lighting,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Monitoring => "Monitoring",
            Self::Cooling => "Cooling",
            Self::Lighting => "Lighting",
            Self::Settings => "Settings",
        }
    }

    /// Icon shown in the rail, so an entry is not identified by color alone.
    pub fn icon(self) -> Icon {
        match self {
            Self::Monitoring => Icon::ChartLine,
            Self::Cooling => Icon::Snowflake,
            Self::Lighting => Icon::Bulb,
            Self::Settings => Icon::Settings,
        }
    }

    /// Tab index of this rail entry. Rail entries come before screen controls.
    pub fn tab_index(self) -> isize {
        match self {
            Self::Monitoring => 1,
            Self::Cooling => 2,
            Self::Lighting => 3,
            Self::Settings => 4,
        }
    }
}

/// The root view.
pub struct Shell {
    focus: FocusHandle,
    feed: Feed,
    link: LinkState,
    metrics: MetricBook,
    cooling: CoolingEditor,
    lighting: LightingEditor,
    /// The LCD editor and the last preset that rendered, held together so an
    /// unfinished field keeps the previous picture on screen instead of
    /// blanking it, and so no control can move one without the other.
    lcd: DisplayScreen,
    /// What the pointer is doing, and where this frame painted what it can grab.
    interaction: Interaction,
    /// Which rows each screen has open, and where the keyboard sits on a plot.
    rows: Disclosure,
    destination: Destination,
    /// When each edited target should be written, once it settles.
    due: WriteSchedule,
    /// The program the last cooling write sent, held until its outcome arrives.
    sent: Option<CoolingProgram>,
    /// How the last command ended, on whichever screen issued it.
    outcome: Option<CommandOutcome>,
    /// Set once the operator has armed the profile deletion.
    ///
    /// Deleting a profile is the one destructive action this screen offers, so
    /// it takes two deliberate activations rather than one stray click.
    confirm_delete: bool,
    /// Set while the pointer holds the title bar, before a move begins.
    window_drag: DragLatch,
    /// The revision each editor was built from, and who changed a domain
    /// the operator has not edited since.
    revisions: Revisions,
    /// The temperature chart, as the last `history` answer left it.
    trend: Trend,
    /// Which history answer [`Self::trend`] was built from.
    history_seen: u64,
    /// The generation of the timer asking for history while Monitoring is
    /// shown. Leaving the screen takes a new one, which stops the old timer.
    history_watch: u64,
    /// `history` requests this window asked for, for the tests that hold it to
    /// asking only while Monitoring is shown.
    history_asked: u64,
    /// The daemon and the connection the link was last connected through.
    /// A change is a reconnection, which voids the chart.
    link_identity: Option<(u64, u64)>,
    /// Bumped whenever the panel preview was replaced.
    picture_version: u64,
    /// The key of the frame last painted.
    painted: Option<Visible>,
    /// Wall clock of the last refresh, used to age every reading.
    now_unix_ms: u64,
}

impl Shell {
    pub fn new(
        feed: Feed,
        notifications: futures::channel::mpsc::UnboundedReceiver<()>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus = cx.focus_handle();
        window.focus(&focus);

        // The view repaints when the worker publishes, not on a timer of its
        // own: one repaint per sample, and no lag between the two.
        cx.spawn(async move |shell, cx| {
            let mut notifications = notifications;
            use futures::StreamExt;
            while notifications.next().await.is_some() {
                if shell.update(cx, |shell, cx| shell.refresh(cx)).is_err() {
                    break;
                }
            }
        })
        .detach();

        // Monitoring is the screen the window opens on, so the chart's history
        // is asked for from the start.
        cx.defer_in(window, |shell, _, cx| shell.watch_history(cx));

        let mut rows = Disclosure::default();
        // The panel opens first: it is the row with something to look at, and a
        // screen where every row is shut hides the editor the last arrangement
        // put on its own destination.
        rows.lighting.insert(LightingRow::Lcd);

        Self {
            focus,
            feed,
            link: LinkState::connecting(),
            metrics: MetricBook::new(),
            cooling: CoolingEditor::new(),
            lighting: LightingEditor::default(),
            lcd: DisplayScreen::default(),
            interaction: Interaction::default(),
            rows,
            destination: Destination::Monitoring,
            due: WriteSchedule::default(),
            sent: None,
            outcome: None,
            confirm_delete: false,
            window_drag: DragLatch::default(),
            revisions: Revisions::default(),
            trend: Trend::Waiting,
            history_seen: 0,
            history_watch: 0,
            history_asked: 0,
            link_identity: None,
            picture_version: 0,
            painted: None,
            now_unix_ms: now_unix_ms(),
        }
    }

    /// Take whatever the worker published, and repaint if it shows.
    fn refresh(&mut self, cx: &mut Context<Self>) {
        let link = self.feed.link();
        let outcome = self.feed.outcome();
        let history = self.feed.history();
        self.absorb(link, outcome, history, cx);
    }

    /// Fold one publication in, and ask for a repaint only if the visible
    /// screen would paint something different.
    ///
    /// Everything a sample can change runs here, never in a repaint: the
    /// readings, the editors following another client, the chart, the panel
    /// preview and the writes that came due. Then the key of the visible
    /// screen is compared with the key of the last frame painted. A sample
    /// that moved only what another screen shows costs nothing.
    pub(crate) fn absorb(
        &mut self,
        link: Option<LinkState>,
        outcome: Option<CommandOutcome>,
        history: Option<(u64, HistoryState)>,
        cx: &mut Context<Self>,
    ) {
        self.now_unix_ms = now_unix_ms();
        let mut changed = false;

        if let Some(link) = link {
            if let Some(snapshot) = link.telemetry() {
                self.metrics.observe(snapshot);
            }
            let identity = link.status().map(|status| {
                (
                    status.started_at_unix_ms,
                    link.connection().unwrap_or_default(),
                )
            });
            let reconnected = identity.is_some() && identity != self.link_identity;
            self.link_identity = identity;
            self.link = link;
            // The controller's channel list is the only source of what can be
            // addressed, so the editor follows it rather than assuming three.
            let channels: Vec<u8> = self
                .link
                .lighting_channels()
                .iter()
                .map(|state| state.channel)
                .collect();
            self.lighting.sync(&channels);
            // A new connection is a new daemon or a lost one, and the trend the
            // last one answered says nothing about where this one stands.
            if reconnected {
                self.trend = Trend::Waiting;
                if self.destination == Destination::Monitoring {
                    self.ask_history();
                }
                changed = true;
            }
        }

        changed |= self.adopt_outcome(outcome);
        changed |= self.follow_revisions();

        if let Some((number, state)) = history
            && number != self.history_seen
        {
            self.history_seen = number;
            self.trend = Trend::from_state(&state);
            changed |= self.destination == Destination::Monitoring;
        }

        // The preview carries readings, so it follows the sample that just
        // landed, but only while it is on screen.
        if self.preview_visible() {
            changed |= self.refresh_picture(cx);
        }

        // The timers are what make an edit land promptly. This is what makes
        // sure it lands at all: a write held back because another was in
        // flight, or a timer that fired while a drag was still running, is
        // picked up here on the next sample rather than waiting for the
        // operator to touch the screen again.
        changed |= self.flush_writes();

        if changed || self.painted.as_ref() != Some(&self.visible()) {
            cx.notify();
        }
    }

    /// Ask the daemon for the chart's points.
    fn ask_history(&mut self) {
        self.history_asked += 1;
        self.feed.request_history();
    }

    /// Ask for history now, and every [`HISTORY_REFRESH`] while Monitoring
    /// stays the visible screen.
    ///
    /// The timer carries the generation it was started under, so leaving the
    /// screen, which takes a new one, leaves it firing into nothing: no task is
    /// cancelled, as with every other timer in the shell.
    fn watch_history(&mut self, cx: &mut Context<Self>) {
        self.history_watch = self.history_watch.wrapping_add(1);
        let generation = self.history_watch;
        self.ask_history();
        cx.spawn(async move |shell, cx| {
            loop {
                cx.background_executor().timer(HISTORY_REFRESH).await;
                let watching = shell.update(cx, |shell, _| {
                    let watching = shell.history_watch == generation
                        && shell.destination == Destination::Monitoring;
                    if watching {
                        shell.ask_history();
                    }
                    watching
                });
                if !matches!(watching, Ok(true)) {
                    break;
                }
            }
        })
        .detach();
    }

    /// Take the worker's latest outcome, and resolve what it settles.
    ///
    /// Told apart from the last one by its sequence rather than by comparing
    /// the whole value: two refusals of the same edit for the same reason are
    /// equal, and treating the second as already seen would leave [`Self::sent`]
    /// set on a write that will never be answered again.
    ///
    /// A write refused with `conflict` drops its pending edit here, before the
    /// status that came with it is followed: another client wrote first, the
    /// editor is about to take what it wrote, and the edit the operator
    /// abandoned must not be sent again over it.
    fn adopt_outcome(&mut self, outcome: Option<CommandOutcome>) -> bool {
        let Some(outcome) = outcome else {
            return false;
        };
        if self.outcome.as_ref().map(|seen| seen.sequence) == Some(outcome.sequence) {
            return false;
        }

        // Only a cooling write's own outcome resolves the cooling write in
        // flight. A refused lighting command used to clear it, which released
        // the guard that keeps two cooling writes from overlapping and left a
        // confirmed curve unrecorded.
        if outcome.subject == CommandSubject::Cooling {
            match outcome.severity {
                // A confirmed write is what turns a pending curve into the
                // client's record of what the hardware is running. Curve points
                // cannot be read back, so this record is the only evidence
                // there is, and it is only ever set from a confirmation.
                OutcomeSeverity::Confirmed => {
                    if let Some(program) = self.sent.take() {
                        self.cooling.record_applied(program);
                    }
                }
                OutcomeSeverity::Unconfirmed | OutcomeSeverity::Refused => self.sent = None,
            }
        }
        if outcome.conflict.is_some()
            && let Some(domain) = Domain::of_subject(outcome.subject)
        {
            self.due.cancel(domain.target());
        }
        self.outcome = Some(outcome);
        true
    }

    fn kraken(&self) -> Option<&KrakenTelemetry> {
        self.link.telemetry().map(|snapshot| &snapshot.kraken)
    }

    fn go(&mut self, destination: Destination, cx: &mut Context<Self>) {
        let left = self.destination;
        self.destination = destination;
        self.interaction.dismiss();
        // An armed deletion does not survive leaving the screen: coming back to
        // a button that already says "Confirm" would delete on one press.
        self.confirm_delete = false;
        if left != destination {
            if destination == Destination::Monitoring {
                self.watch_history(cx);
            }
            // An animation only runs while it is on screen: leaving stops its
            // timer, and coming back rebuilds the preview once and puts it on
            // the frame the glass is showing.
            if left == Destination::Lighting {
                self.lcd.next_film_generation();
            }
            if self.preview_visible() {
                self.refresh_picture(cx);
                self.resume_film(cx);
            }
        }
        cx.notify();
    }

    /// Open one row of the Lighting screen, or close it if it is open.
    ///
    /// Every control the detail renders is built for the channel whose line it
    /// sits under and carries that channel with it, so opening a row reveals
    /// controls rather than selecting anything.
    fn toggle_lighting_row(&mut self, row: LightingRow, cx: &mut Context<Self>) {
        self.rows.lighting.toggle(row);
        // A popover anchored to a control that just moved would be left
        // pointing at nothing.
        self.interaction.dismiss();
        // An animation only runs while it is on screen. A closed row repaints
        // nothing, so a timer firing into it would be ten wake-ups a second
        // spent moving a cursor nobody can see. Opening it builds the preview
        // once, since no sample rebuilt it while it was shut.
        if row == LightingRow::Lcd {
            if self.preview_visible() {
                self.refresh_picture(cx);
                self.resume_film(cx);
            } else {
                self.lcd.next_film_generation();
            }
        }
        cx.notify();
    }

    fn toggle_popover(&mut self, popover: Popover, cx: &mut Context<Self>) {
        self.interaction.toggle(popover);
        cx.notify();
    }

    fn on_focus_next(&mut self, _: &FocusNext, window: &mut Window, cx: &mut Context<Self>) {
        set_focus_visible(true);
        window.focus_next();
        cx.notify();
    }

    fn on_focus_previous(
        &mut self,
        _: &FocusPrevious,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        set_focus_visible(true);
        window.focus_prev();
        cx.notify();
    }

    fn on_close_popover(&mut self, _: &ClosePopover, _: &mut Window, cx: &mut Context<Self>) {
        if self.interaction.dismiss() {
            cx.notify();
        }
    }

    /// The navigation rail: Paneflow's Settings navigation.
    ///
    /// No card and no divider. The rail is part of the shell, painted in the
    /// shell color, and the main panel beside it is what is raised: the frame
    /// recedes and the content is the thing laid on top. The destinations run
    /// down from the title bar and the utility entry is pinned to a footer,
    /// set apart by the empty space above it.
    ///
    /// Nothing names the product here. The window title carries the name, the
    /// Settings screen carries the qualifier, and a heading repeating either one
    /// would only push the first destination down the column.
    fn rail(&self, cx: &mut Context<Self>) -> Div {
        let current = self.destination;
        // A `map` closure would have to hold the mutable context borrow across
        // calls, which the 2024 edition's capture rules reject. A plain loop
        // borrows it once per entry and releases it.
        let mut primary_entries = Vec::with_capacity(Destination::PRIMARY.len());
        for destination in Destination::PRIMARY {
            primary_entries.push(self.rail_entry(destination, current, cx));
        }

        div()
            .flex()
            .flex_col()
            .flex_none()
            .w(RAIL_WIDTH)
            .h_full()
            .font_family(NAV_FONT)
            .text_size(text::NAV)
            .line_height(text::NAV_LINE)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(NAV_ROW_GAP)
                    .pt(space::XS)
                    .children(primary_entries),
            )
            .child(div().flex_1())
            .child(div().flex().flex_col().pb(px(9.5)).child(self.rail_entry(
                Destination::Settings,
                current,
                cx,
            )))
    }

    /// Returns a concrete element rather than `impl IntoElement`.
    ///
    /// Under the 2024 edition an opaque return type captures every input
    /// lifetime, so an `impl IntoElement` here would keep borrowing the context
    /// and only one entry could be built at a time.
    ///
    /// Paneflow's sidebar row: 32 tall, a continuous corner of 9, a muted glyph
    /// and the label in the full text color. The destination the operator is
    /// standing in takes the active wash and every other one the hover wash
    /// under the pointer: the same material as a lit row anywhere else, never a
    /// colored fill. The glyph and the wash are what mark it, so selection is
    /// not carried by color alone either.
    fn rail_entry(
        &self,
        destination: Destination,
        current: Destination,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let selected = destination == current;
        let label = destination.label();

        focus_ring(
            squircle::skin(
                div().id(SharedString::from(label)),
                SharedString::from(format!("rail-{label}")),
                NAV_ROW_RADIUS,
                selected.then(|| color::TEXT.alpha(color::NAV_ACTIVE)),
                (!selected).then(|| color::TEXT.alpha(color::NAV_HOVER)),
            )
            .tab_index(destination.tab_index())
            .tab_stop(true)
            .flex()
            .flex_none()
            .items_center()
            .gap(space::SM)
            .mx(NAV_ROW_MARGIN)
            .min_h(NAV_ROW_HEIGHT)
            // The ring is reserved inside the row's own padding, so focusing
            // an entry moves nothing.
            .px(NAV_ROW_PADDING_X - FOCUS_RING)
            .py(px(6.0) - FOCUS_RING)
            .rounded(NAV_ROW_RADIUS)
            .cursor_pointer()
            .text_color(color::TEXT.hsla()),
            true,
        )
        .child(
            icon(destination.icon(), NAV_ICON_SIZE, color::TEXT_MUTED.hsla())
                .relative()
                .flex_none(),
        )
        .child(div().relative().flex_1().min_w_0().truncate().child(label))
        .on_click(cx.listener(move |this, _, _, cx| this.go(destination, cx)))
    }

    fn banner(&self) -> Option<Div> {
        let message = self.link.banner()?;
        Some(Note::new(NoteLevel::Warning, message).render())
    }

    /// Note an edit and arrange for it to be written once things go quiet.
    ///
    /// Every edit spawns its own timer and every timer drains whatever has come
    /// due, so a later edit on the same target pushes that target's deadline
    /// out and the earlier timers fire into nothing. No task has to be
    /// cancelled and no edit can be lost by cancelling the wrong one.
    fn schedule_write(&mut self, target: WriteTarget, cx: &mut Context<Self>) {
        // An edit is the operator reading the control: whoever changed it
        // before is no longer news.
        self.revisions.edited(Domain::of_target(target));
        // The picture is built at the edit rather than at the repaint, and this
        // is where every panel edit passes.
        if target == WriteTarget::Lighting(LightingRow::Lcd) {
            self.refresh_picture(cx);
        }
        let quiet = target.quiet();
        self.due.touch(target, Instant::now() + quiet);
        cx.notify();
        cx.spawn(async move |shell, cx| {
            cx.background_executor().timer(quiet).await;
            let _ = shell.update(cx, |shell, cx| {
                if shell.flush_writes() {
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// Write everything that has been still long enough.
    ///
    /// The quiet period is not cosmetic. The daemon refuses a command arriving
    /// inside its cadence floor rather than queueing it, and it keeps no
    /// last-value-wins. A screen that wrote on every pointer move would have
    /// most of its writes refused, and the value the operator actually stopped
    /// on could be one of them.
    ///
    /// A target that is due but held back keeps its place in the book rather
    /// than losing its write: this also runs on every sample, so a write held
    /// back by a gesture or by an outcome that had not landed is picked up on
    /// the next one instead of waiting for the operator to touch the screen
    /// again.
    ///
    /// Returns whether a write left, which is the only way a flush changes
    /// what the screen shows: a target held back keeps its place and looks
    /// exactly as it did.
    fn flush_writes(&mut self) -> bool {
        let mut sent = false;
        for target in self.due.take_due(Instant::now()) {
            if write_is_held_back(target, self.interaction.drag(), self.sent.is_some()) {
                self.due.touch(target, Instant::now() + target.quiet());
                continue;
            }
            sent |= match target {
                WriteTarget::Cooling => self.send_cooling(),
                WriteTarget::Lighting(LightingRow::Channel(channel)) => self.send_lighting(channel),
                WriteTarget::Lighting(LightingRow::Lcd) => self.send_display(),
            };
        }
        sent
    }

    /// Whether the panel preview is on screen: Lighting, with its row open.
    pub(crate) fn preview_visible(&self) -> bool {
        self.destination == Destination::Lighting && self.is_open(LightingRow::Lcd)
    }
}

impl Focusable for Shell {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for Shell {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Each frame republishes the tracks it paints. Clearing here is what
        // keeps a row that has gone away, or a screen that no longer shows one,
        // from leaving a rectangle behind that a press could still grab.
        self.interaction.clear_tracks();
        // What this frame paints, for the next sample to compare against.
        self.painted = Some(self.visible());

        let content = match self.destination {
            Destination::Monitoring => self.monitoring(),
            Destination::Cooling => self.cooling(cx),
            Destination::Lighting => self.lighting(cx),
            Destination::Settings => self.settings(),
        };

        let title_bar = window_chrome::title_bar(window, &self.window_drag);

        let shell = div()
            .id("shell")
            .track_focus(&self.focus)
            // Any press anywhere in the window means focus is about to move by
            // pointer, so the ring is dropped before the control under the
            // cursor renders with it. Capture phase, so it runs ahead of that
            // control's own handler rather than after it. Enter and Space reach
            // a control through the click listeners without a mouse event, so
            // keyboard activation leaves the ring alone.
            .capture_any_mouse_down(cx.listener(|this, event: &gpui::MouseDownEvent, _, cx| {
                if focus_visible() {
                    set_focus_visible(false);
                    cx.notify();
                }
                // An open popover owns the press: it is being dismissed, and a
                // press that lands over a slider or a curve on the way out must
                // not also move a value the operator was not aiming at. This is
                // also why `begin_drag` never has a list to close.
                if this.interaction.showing_any() {
                    return;
                }
                // A press starts at most one gesture, decided here from where
                // the controls were painted rather than by a listener on each
                // of them: an event that has to reach a control nested under
                // its own interactive state never arrived, and the capture
                // phase on the window is the one place it always does.
                if event.button == MouseButton::Left {
                    this.begin_drag(event.position, cx);
                }
            }))
            // A brightness drag is tracked by the window rather than by the
            // slider: the pointer routinely leaves a 200-pixel control while
            // still holding it, and a drag that stopped at the edge would leave
            // the value wherever the cursor crossed it.
            .on_mouse_move(cx.listener(|this, event: &gpui::MouseMoveEvent, _, cx| {
                this.drag_to(event.position, cx);
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _: &gpui::MouseUpEvent, _, cx| {
                    // Nothing is written while the pointer holds a control: a
                    // drag names dozens of values and only the one it stops on
                    // is worth a command. Releasing ends the gesture wherever
                    // the pointer is, including outside the control that
                    // started it.
                    match this.interaction.end_drag() {
                        Some(Drag::Brightness { row, .. }) => {
                            this.schedule_write(WriteTarget::Lighting(row), cx)
                        }
                        Some(Drag::Duty { .. } | Drag::Curve { .. }) => {
                            this.schedule_write(WriteTarget::Cooling, cx)
                        }
                        None => {}
                    }
                }),
            )
            .on_action(cx.listener(Self::on_focus_next))
            .on_action(cx.listener(Self::on_focus_previous))
            .on_action(cx.listener(Self::on_close_popover))
            .on_action(
                cx.listener(|this, _: &GoMonitoring, _, cx| this.go(Destination::Monitoring, cx)),
            )
            .on_action(cx.listener(|this, _: &GoCooling, _, cx| this.go(Destination::Cooling, cx)))
            .on_action(
                cx.listener(|this, _: &GoLighting, _, cx| this.go(Destination::Lighting, cx)),
            )
            .on_action(
                cx.listener(|this, _: &GoSettings, _, cx| this.go(Destination::Settings, cx)),
            )
            .size_full()
            .flex()
            .flex_col()
            // Paneflow sets its face on the root element and every surface
            // inherits it; only the rail asks for the platform's own.
            .font_family(UI_FONT)
            .text_size(text::BODY)
            .text_color(color::TEXT.hsla())
            // The title bar is a row of its own, across the whole window, so
            // every pixel of it above the rail and the panel is a place to
            // drag the window from.
            .child(title_bar)
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .child(self.rail(cx))
                    .child(
                        // The main panel: the inset card the screens are laid
                        // on, lighter than the shell around it, which is what
                        // makes it read as a card without a shadow. Inset on
                        // the right and the bottom only, as Paneflow's is
                        // while its sidebar is shown: the rail is the inset
                        // on the left.
                        div()
                            .flex_1()
                            // Without this the rail plus an unwrapped sentence
                            // can exceed the window width instead of wrapping.
                            .min_w_0()
                            .h_full()
                            .pr(PANEL_INSET)
                            .pb(PANEL_INSET)
                            .child(
                                div()
                                    .id("work-surface")
                                    .size_full()
                                    .rounded(PANEL_RADIUS)
                                    .bg(color::SURFACE.hsla())
                                    .overflow_y_scroll()
                                    .child(
                                        // Paneflow's Settings column: centered,
                                        // capped, and padded so the corners of
                                        // the panel never reach a card.
                                        div()
                                            .w_full()
                                            .max_w(COLUMN_MAX_WIDTH)
                                            .mx_auto()
                                            .px(COLUMN_PADDING)
                                            .pt(COLUMN_PADDING)
                                            .pb(px(72.0))
                                            .flex()
                                            .flex_col()
                                            .gap(BLOCK_GAP)
                                            .children(self.banner())
                                            .child(content),
                                    ),
                            ),
                    ),
            );

        // The shell color fills the window, so the title bar, the rail and all
        // four corners read as one ground and only the panel is raised off it.
        window_chrome::window_shell(
            div()
                .relative()
                .size_full()
                .child(shell)
                // While a list is open, the rest of the window is a way out of
                // it. Painted under the popover and over everything else, so a
                // press anywhere else closes the list and stops there: the
                // control underneath is not also operated by the press that
                // dismissed the list, which is how a menu behaves everywhere
                // else on the desktop. Escape does the same from the keyboard.
                .children(self.interaction.showing_any().then(|| {
                    div()
                        .id("popover-dismiss")
                        // Takes the press rather than letting it through, so
                        // the control that opened the list does not reopen it
                        // on the release and no other control is operated by
                        // the press that closed it.
                        .occlude()
                        .absolute()
                        .top_0()
                        .left_0()
                        .size_full()
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, _, _, cx| {
                                this.interaction.dismiss();
                                cx.notify();
                            }),
                        )
                })),
            window,
            color::RAIL.hsla(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::{WINDOW_HEIGHT, WINDOW_WIDTH};
    use kori_core::profile::Channel;
    use std::time::Duration;

    /// Lay the shell out and paint it at the size the layout is designed for.
    ///
    /// The view itself, not a placeholder: `draw` paints whatever the closure
    /// returns, so handing it an empty element would assert nothing at all.
    fn draw(shell: &gpui::Entity<Shell>, cx: &mut gpui::VisualTestContext) {
        let view = shell.clone();
        cx.draw(
            gpui::point(gpui::px(0.0), gpui::px(0.0)),
            gpui::size(WINDOW_WIDTH, WINDOW_HEIGHT),
            |_, _| gpui::AnyView::from(view),
        );
    }

    /// A shell with no daemon behind it, which is the state every write control
    /// is disabled in and every readout has to describe without inventing one.
    fn shell(cx: &mut gpui::TestAppContext) -> (gpui::Entity<Shell>, &mut gpui::VisualTestContext) {
        shell_on(
            std::path::PathBuf::from("/nonexistent/kori/absent.sock"),
            Duration::from_secs(3_600),
            cx,
        )
    }

    /// A shell with no daemon, once its worker has said so and answered the
    /// first history request. From then on the worker publishes nothing, so
    /// what a test hands [`Shell::absorb`] is the only thing that arrives.
    fn idle_shell(
        cx: &mut gpui::TestAppContext,
    ) -> (gpui::Entity<Shell>, &mut gpui::VisualTestContext) {
        let (shell, cx) = shell(cx);
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            cx.run_until_parked();
            let settled = shell.read_with(cx, |shell, _| {
                shell.history_seen > 0 && shell.feed.link().is_some()
            });
            if settled {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the worker never answered"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        (shell, cx)
    }

    /// A shell whose worker talks to the socket at `socket`.
    fn shell_on(
        socket: std::path::PathBuf,
        interval: Duration,
        cx: &mut gpui::TestAppContext,
    ) -> (gpui::Entity<Shell>, &mut gpui::VisualTestContext) {
        cx.update(|cx| cx.bind_keys(key_bindings()));
        let (feed, notifications) = Feed::spawn(socket, interval);
        cx.add_window_view(|window, cx| Shell::new(feed, notifications, window, cx))
    }

    /// Every destination builds its whole element tree.
    ///
    /// The unit tests around this one exercise the rules a screen applies; none
    /// of them builds one. That gap is not theoretical: GPUI asserts that a
    /// hover style is set exactly once, and a primitive that layers its own
    /// accent over a shared one only trips that assertion when the screen
    /// holding it is actually rendered. Splitting this module into one file per
    /// screen would otherwise have been checked by the compiler and by nothing
    /// else.
    #[gpui::test]
    fn every_destination_renders_without_a_daemon(cx: &mut gpui::TestAppContext) {
        let (shell, cx) = shell(cx);

        for destination in Destination::PRIMARY
            .into_iter()
            .chain([Destination::Settings])
        {
            shell.update(cx, |shell, cx| shell.go(destination, cx));
            draw(&shell, cx);
            cx.run_until_parked();
            shell.read_with(cx, |shell, _| {
                assert_eq!(shell.destination, destination);
            });
        }
    }

    /// The open rows are where the screens carry most of their controls, and a
    /// closed row renders none of them. This walks the two openable screens
    /// with every row revealed.
    #[gpui::test]
    fn every_open_row_renders_its_controls(cx: &mut gpui::TestAppContext) {
        let (shell, cx) = shell(cx);

        shell.update(cx, |shell, cx| {
            shell.rows.toggle_cooling(Channel::Pump);
            shell.rows.toggle_cooling(Channel::Fan);
            for row in [
                LightingRow::Channel(1),
                LightingRow::Channel(2),
                LightingRow::Channel(3),
            ] {
                shell.rows.lighting.insert(row);
            }
            cx.notify();
        });

        for destination in [Destination::Cooling, Destination::Lighting] {
            shell.update(cx, |shell, cx| shell.go(destination, cx));
            draw(&shell, cx);
            cx.run_until_parked();
        }

        shell.read_with(cx, |shell, _| {
            assert!(shell.rows.cooling.contains(Channel::Pump));
            assert!(
                shell.is_open(LightingRow::Lcd),
                "the panel opens by default"
            );
        });
    }

    /// Leaving a screen closes whatever list was open on it: a menu anchored to
    /// a control that is no longer drawn would be left pointing at nothing.
    #[gpui::test]
    fn navigating_away_closes_the_open_list_and_disarms_the_deletion(
        cx: &mut gpui::TestAppContext,
    ) {
        let (shell, cx) = shell(cx);

        shell.update(cx, |shell, cx| {
            shell.toggle_popover(
                Popover::Options {
                    select: screen::SelectId::CoolingMode,
                },
                cx,
            );
            shell.confirm_delete = true;
        });
        shell.read_with(cx, |shell, _| assert!(shell.interaction.showing_any()));

        shell.update(cx, |shell, cx| shell.go(Destination::Lighting, cx));
        shell.read_with(cx, |shell, _| {
            assert!(!shell.interaction.showing_any());
            assert!(
                !shell.confirm_delete,
                "a primed delete must not survive the trip back"
            );
        });
    }

    #[test]
    fn the_shell_exposes_three_primary_destinations_and_one_secondary() {
        // The panel is not one of them. It is a device's appearance, so it is a
        // row on Lighting rather than a destination that would put the two
        // halves of the same question two clicks apart.
        assert_eq!(Destination::PRIMARY.len(), 3);
        assert_eq!(
            Destination::PRIMARY.map(Destination::label),
            ["Monitoring", "Cooling", "Lighting"]
        );
        assert!(!Destination::PRIMARY.contains(&Destination::Settings));
    }

    #[test]
    fn every_destination_has_a_label_and_its_own_icon() {
        let mut icons = Vec::new();
        for destination in Destination::PRIMARY
            .into_iter()
            .chain([Destination::Settings])
        {
            assert!(!destination.label().is_empty());
            icons.push(destination.icon());
        }
        // An entry identified by a shared icon is identified by its label
        // alone, which is what the rail's icons exist to avoid. Compared
        // pairwise rather than sorted: an icon is a name, and giving it an
        // order would invent a ranking the interface never uses.
        for (index, icon) in icons.iter().enumerate() {
            assert!(
                !icons[index + 1..].contains(icon),
                "two rail entries share {icon:?}"
            );
        }
    }

    // ---- The window against a real daemon -------------------------------

    use std::cell::Cell;
    use std::rc::Rc;
    use std::sync::Arc;

    use kori_core::capability::{CapabilityId, LcdPanel, LcdPanelShape};
    use kori_core::client::{Client, DEFAULT_TIMEOUT};
    use kori_core::display::{DisplayMode, DisplayPreset};
    use kori_core::history::{HistoryMetric, HistoryReply, MetricHistory, Summary};
    use kori_core::ipc::AccessMode;
    use kori_core::lighting::{Brightness, Rgb};
    use kori_daemon::{LcdBackend, RgbBackend};
    use kori_hardware_linux::testing::{FakeController, FakeKraken};

    use crate::feed::{HistoryState, TREND_METRICS};
    use crate::link::fixture;
    use crate::shell::screen::monitoring::{HISTORY_STARTING, HISTORY_UNREACHABLE, TREND_POINTS};
    use crate::shell::screen::settings::DIAGNOSTICS_HINT;
    use crate::shell::screen::write::COOLING_QUIET;
    use crate::testing::harness::{Harness, Machine, lighting, preset};
    use crate::testing::{Proxy, wait_until};

    const PATIENCE: Duration = Duration::from_secs(10);

    /// Run the window's tasks until `condition` holds, in real time, since the
    /// worker publishing to it is a real thread talking to a real socket.
    fn pump(
        cx: &mut gpui::VisualTestContext,
        what: &str,
        mut condition: impl FnMut(&mut gpui::VisualTestContext) -> bool,
    ) {
        let deadline = std::time::Instant::now() + PATIENCE;
        loop {
            cx.run_until_parked();
            if condition(cx) {
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "timed out waiting for {what}"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Paint until the window is steady. The first frame starts loading the
    /// icons, and a finished load asks for one more frame; a count of repaints
    /// starts after that one.
    fn settle_frame(shell: &gpui::Entity<Shell>, cx: &mut gpui::VisualTestContext) {
        draw(shell, cx);
        cx.run_until_parked();
        draw(shell, cx);
        cx.run_until_parked();
    }

    /// Count every repaint the shell asks for from here on.
    fn repaints(shell: &gpui::Entity<Shell>, cx: &mut gpui::VisualTestContext) -> Rc<Cell<usize>> {
        let count = Rc::new(Cell::new(0));
        let seen = Rc::clone(&count);
        cx.update(|_, cx| {
            cx.observe(shell, move |_, _| seen.set(seen.get() + 1))
                .detach()
        });
        count
    }

    /// Let a pending write's quiet period run out: in real time for the
    /// deadline the schedule compares against, and on the test clock for the
    /// timer that wakes the flush.
    fn settle_writes(cx: &mut gpui::VisualTestContext) {
        let quiet = COOLING_QUIET + Duration::from_millis(60);
        std::thread::sleep(quiet);
        cx.executor().advance_clock(quiet);
        cx.run_until_parked();
    }

    fn connected(shell: &gpui::Entity<Shell>, cx: &mut gpui::VisualTestContext) -> bool {
        shell.read_with(cx, |shell, _| shell.link.status().is_some())
    }

    /// Both devices answering, so all three write domains exist.
    fn full_machine(name: &str) -> Harness {
        Harness::start_on(
            name,
            Machine {
                rgb: RgbBackend::Transport(Box::new(FakeController::new("1.5.0", 3))),
                lcd: LcdBackend::Link(Box::new(FakeKraken::new("2.0.0").link())),
                ..Machine::default()
            },
            |_, _| {},
        )
    }

    fn agent(harness: &Harness, name: Option<&str>) -> Client {
        Client::connect_as(harness.socket(), DEFAULT_TIMEOUT, name).unwrap()
    }

    /// A connected link with a fresh sample and a panel that answered.
    fn fresh_link() -> LinkState {
        let mut link = fixture::connected(
            AccessMode::ReadWrite,
            vec![fixture::writable(CapabilityId::PumpDuty)],
        );
        if let LinkState::Connected {
            status, telemetry, ..
        } = &mut link
        {
            Arc::make_mut(status).display.panel = Some(LcdPanel {
                width: 240,
                height: 240,
                shape: LcdPanelShape::Square,
                pixel_format: "RGB565 big-endian".to_string(),
                frame_bytes: 240 * 240 * 2,
                bulk_endpoint: 0x02,
                bulk_interface: 0,
            });
            *telemetry = Some(Arc::new(fixture::telemetry(now_unix_ms(), true)));
        }
        link
    }

    fn with_snapshot(
        link: &LinkState,
        edit: impl FnOnce(&mut kori_core::telemetry::TelemetrySnapshot),
    ) -> LinkState {
        let mut link = link.clone();
        if let LinkState::Connected { telemetry, .. } = &mut link {
            let mut snapshot = telemetry.as_deref().cloned().unwrap();
            edit(&mut snapshot);
            *telemetry = Some(Arc::new(snapshot));
        }
        link
    }

    fn with_status(
        link: &LinkState,
        edit: impl FnOnce(&mut kori_core::ipc::DaemonStatus),
    ) -> LinkState {
        let mut link = link.clone();
        if let LinkState::Connected { status, .. } = &mut link {
            edit(Arc::make_mut(status));
        }
        link
    }

    /// A reply the daemon could give for the chart: `points` per metric, every
    /// one a reading, `available_s` of recorded history.
    fn reply(points: usize, available_s: u32) -> HistoryReply {
        let metrics = TREND_METRICS
            .into_iter()
            .enumerate()
            .map(|(index, metric)| {
                let avg: Vec<Option<f32>> = (0..points)
                    .map(|step| Some(30.0 + index as f32 + step as f32 / 100.0))
                    .collect();
                (
                    metric,
                    MetricHistory {
                        unit: "celsius".to_string(),
                        summary: Some(Summary {
                            min: 30.0,
                            avg: 40.0 + index as f32,
                            max: 50.0 + index as f32,
                            last: 45.0,
                            samples: 900,
                        }),
                        min: Some(avg.clone()),
                        max: Some(avg.clone()),
                        avg: Some(avg),
                    },
                )
            })
            .collect();
        HistoryReply {
            end_unix_ms: now_unix_ms(),
            step_s: 5,
            slots: points as u32,
            available_s,
            metrics,
        }
    }

    // ---- US-009: Monitoring -------------------------------------------------

    /// The chart is the daemon's: the window asks for the three temperatures
    /// over fifteen minutes in five-second points, draws the `avg` arrays, and
    /// asks again on the five-second cadence only while Monitoring is shown.
    #[gpui::test]
    fn monitoring_draws_the_daemons_history_and_asks_only_while_shown(
        cx: &mut gpui::TestAppContext,
    ) {
        let harness = Harness::start("shell-history");
        let proxy = Proxy::start("shell-history", harness.socket());
        let (shell, cx) = shell_on(proxy.path().to_path_buf(), Duration::from_secs(1), cx);
        // The fake machine records a slot per 40 ms tick, so a few of its
        // "seconds" pass in a fraction of a real one; the periodic request is
        // what picks them up.
        pump(cx, "a drawn chart", |cx| {
            let drawn = shell.read_with(cx, |shell, _| matches!(shell.trend, Trend::Drawn { .. }));
            if !drawn {
                cx.executor().advance_clock(HISTORY_REFRESH);
            }
            drawn
        });

        let asked = proxy.requests("history");
        assert!(!asked.is_empty());
        for frame in &asked {
            assert_eq!(
                frame["metrics"],
                serde_json::json!(["cpu_temperature", "gpu_temperature", "liquid_temperature"])
            );
            assert_eq!(frame["window_s"], 900);
            assert_eq!(frame["step_s"], 5);
            assert_eq!(frame["points"], true);
        }
        shell.read_with(cx, |shell, _| {
            let Trend::Drawn { series, .. } = &shell.trend else {
                unreachable!()
            };
            for values in series {
                assert_eq!(values.len(), TREND_POINTS, "180 points per series");
            }
            assert!(
                series[0].iter().any(Option::is_some),
                "the fake CPU sensor's readings are drawn"
            );
            assert_eq!(shell.chart_message(), None);
        });

        // Another screen: the timer keeps firing into nothing.
        shell.update(cx, |shell, cx| shell.go(Destination::Settings, cx));
        let before = (
            proxy.requests("history").len(),
            shell.read_with(cx, |s, _| s.history_asked),
        );
        for _ in 0..4 {
            cx.executor().advance_clock(HISTORY_REFRESH);
            cx.run_until_parked();
        }
        std::thread::sleep(Duration::from_millis(200));
        cx.run_until_parked();
        assert_eq!(
            (
                proxy.requests("history").len(),
                shell.read_with(cx, |s, _| s.history_asked)
            ),
            before,
            "no history request while another screen is shown"
        );

        // Back on Monitoring: one request now, one per period after.
        shell.update(cx, |shell, cx| shell.go(Destination::Monitoring, cx));
        wait_until("the request on becoming visible", PATIENCE, || {
            proxy.requests("history").len() == before.0 + 1
        });
        cx.executor().advance_clock(HISTORY_REFRESH);
        cx.run_until_parked();
        wait_until("the periodic request", PATIENCE, || {
            proxy.requests("history").len() == before.0 + 2
        });
    }

    /// A daemon that started seconds ago has less than one point to give: the
    /// grid is drawn with the sentence and no line.
    #[gpui::test]
    fn a_young_daemon_draws_the_grid_and_says_history_starts_with_it(
        cx: &mut gpui::TestAppContext,
    ) {
        // A one-second tick, so the daemon really is under five seconds old
        // when the first answer comes back.
        let harness = Harness::start_on(
            "shell-young",
            Machine {
                interval: Duration::from_secs(1),
                ..Machine::default()
            },
            |_, _| {},
        );
        let (shell, cx) = shell_on(harness.socket().to_path_buf(), Duration::from_secs(1), cx);
        pump(cx, "the first answer", |cx| {
            shell.read_with(cx, |shell, _| {
                shell.link.status().is_some() && shell.history_seen > 0
            })
        });
        shell.read_with(cx, |shell, _| {
            assert_eq!(shell.trend, Trend::Starting);
            assert_eq!(
                shell.chart_message().as_deref().map(|m| &**m),
                Some(HISTORY_STARTING)
            );
        });
        draw(&shell, cx);

        // And before any answer, the grid alone: no line and no sentence.
        let (waiting, cx) = idle_shell(&mut cx.cx);
        waiting.update(cx, |shell, cx| {
            shell.absorb(Some(fresh_link()), None, None, cx);
            assert_eq!(shell.trend, Trend::Waiting);
            assert_eq!(shell.chart_message(), None);
        });
    }

    /// With no daemon the dials go to their disconnected state and the chart
    /// says why; a daemon that refuses the request is quoted.
    #[gpui::test]
    fn the_chart_states_why_it_is_empty_when_history_cannot_be_had(cx: &mut gpui::TestAppContext) {
        let (shell, cx) = shell(cx);
        pump(cx, "the worker's answer", |cx| {
            shell.read_with(cx, |shell, _| shell.history_seen > 0)
        });
        draw(&shell, cx);
        shell.read_with(cx, |shell, _| {
            assert!(matches!(shell.trend, Trend::Refused(_)));
            assert_eq!(
                shell.chart_message().as_deref().map(|m| &**m),
                Some(HISTORY_UNREACHABLE)
            );
            assert!(
                shell
                    .metrics
                    .cpu_load
                    .view(shell.now_unix_ms)
                    .is_unavailable(),
                "the dials read as disconnected"
            );
        });

        shell.update(cx, |shell, cx| {
            shell.absorb(
                Some(fresh_link()),
                None,
                Some((
                    99,
                    HistoryState::Refused("The background service refused it.".into()),
                )),
                cx,
            );
            assert_eq!(
                shell.chart_message().as_deref().map(|m| &**m),
                Some("The background service refused it.")
            );
        });
    }

    /// A window opened on a daemon that has run for an hour draws the whole
    /// fifteen minutes on the first frame after the answer, with the daemon's
    /// own averages and peaks in the legend.
    #[gpui::test]
    fn an_hour_old_daemon_fills_the_chart_on_the_first_render_after_its_answer(
        cx: &mut gpui::TestAppContext,
    ) {
        let (shell, cx) = idle_shell(cx);
        shell.update(cx, |shell, cx| {
            shell.absorb(
                Some(fresh_link()),
                None,
                Some((
                    1_000,
                    HistoryState::Answered(Arc::new(reply(TREND_POINTS, 3_600))),
                )),
                cx,
            );
        });
        draw(&shell, cx);
        shell.read_with(cx, |shell, _| {
            let Trend::Drawn {
                series, summaries, ..
            } = &shell.trend
            else {
                panic!("{:?}", shell.trend)
            };
            for values in series {
                assert_eq!(values.iter().flatten().count(), TREND_POINTS);
            }
            assert_eq!(summaries[2].map(|summary| summary.max), Some(52.0));
            assert!(shell.painted.is_some(), "the frame was painted from it");
        });
    }

    /// A render walks the points it draws and no more: a reply carrying more
    /// than 180 per series is cut to its newest 180 when it lands, once.
    #[test]
    fn the_chart_keeps_at_most_the_points_it_draws() {
        let oversized = reply(900, 900);
        let Trend::Drawn { series, .. } = Trend::from_reply(&oversized) else {
            panic!("a full reply draws")
        };
        let newest = oversized.metrics[&HistoryMetric::CpuTemperature]
            .avg
            .as_ref()
            .unwrap()[899];
        for values in &series {
            assert_eq!(values.len(), TREND_POINTS);
        }
        assert_eq!(
            series[0][TREND_POINTS - 1],
            newest,
            "the newest points are kept"
        );
        assert_eq!(Trend::from_reply(&reply(180, 4)), Trend::Starting);
    }

    /// The pointer moving over Monitoring asks for nothing: the chart has no
    /// hover layer and the shell's move handler only follows a drag. A press
    /// at the same spot does reach the shell, so the moves were delivered.
    #[gpui::test]
    fn moving_the_pointer_over_monitoring_asks_for_no_repaint(cx: &mut gpui::TestAppContext) {
        let (shell, cx) = idle_shell(cx);
        shell.update(cx, |shell, cx| {
            shell.absorb(
                Some(fresh_link()),
                None,
                Some((
                    1_000,
                    HistoryState::Answered(Arc::new(reply(TREND_POINTS, 3_600))),
                )),
                cx,
            );
        });
        draw(&shell, cx);
        let chart = cx
            .debug_bounds("monitoring-temperatures")
            .expect("the chart was painted");
        settle_frame(&shell, cx);
        let count = repaints(&shell, cx);
        cx.run_until_parked();
        assert_eq!(count.get(), 0, "the window is not settled");
        for step in 0..20 {
            let x = chart.origin.x + chart.size.width * (step as f32 / 20.0);
            let y = chart.origin.y + chart.size.height / 2.0;
            cx.simulate_mouse_move(gpui::point(x, y), None, gpui::Modifiers::none());
        }
        assert_eq!(count.get(), 0, "a pointer move asked for a repaint");

        set_focus_visible(true);
        cx.simulate_mouse_down(chart.center(), MouseButton::Right, gpui::Modifiers::none());
        assert_eq!(
            count.get(),
            1,
            "the press at the same spot reached the shell"
        );
    }

    // ---- US-010: other clients ----------------------------------------------

    /// What an agent writes lands in the window's editors, for cooling, a
    /// lighting channel and the panel alike, named under the control it
    /// changed until the operator edits it.
    #[gpui::test]
    fn the_window_adopts_what_another_client_changed_and_names_it(cx: &mut gpui::TestAppContext) {
        let harness = full_machine("shell-adopt");
        let (shell, cx) = shell_on(harness.socket().to_path_buf(), Duration::from_secs(1), cx);
        pump(cx, "the window to connect", |cx| connected(&shell, cx));

        let mut korictl = agent(&harness, Some("korictl"));
        let program = CoolingProgram::Fixed {
            pump: 190,
            fan: 110,
        };
        korictl.apply(program.clone()).unwrap();
        korictl.apply_lighting(lighting(1, "ff8800")).unwrap();
        let mut picture = preset(DisplayMode::DualInfographic);
        picture.brightness = Brightness::new(40).unwrap();
        korictl.apply_display(picture.clone()).unwrap();

        pump(cx, "all three domains adopted", |cx| {
            shell.read_with(cx, |shell, _| {
                shell.cooling.program() == program
                    && shell.lighting.channel(1).and_then(|row| row.program().ok())
                        == Some(lighting(1, "ff8800").program)
                    && shell.lcd.preset().ok().as_ref() == Some(&picture)
            })
        });
        shell.read_with(cx, |shell, _| {
            for domain in [Domain::Cooling, Domain::Lighting(1), Domain::Panel] {
                assert_eq!(shell.revisions.notice(domain), Some("Changed by korictl"));
            }
            assert_eq!(shell.revisions.notice(Domain::Lighting(2)), None);
        });
        for destination in [Destination::Cooling, Destination::Lighting] {
            shell.update(cx, |shell, cx| shell.go(destination, cx));
            draw(&shell, cx);
        }

        // An edit is the operator reading the notice.
        shell.update(cx, |shell, cx| {
            shell.schedule_write(WriteTarget::Cooling, cx)
        });
        shell.read_with(cx, |shell, _| {
            assert_eq!(shell.revisions.notice(Domain::Cooling), None)
        });

        // A connection that gave no name is another client.
        std::thread::sleep(Duration::from_millis(250));
        let mut anonymous = agent(&harness, None);
        anonymous.apply_lighting(lighting(2, "00ff00")).unwrap();
        pump(cx, "the anonymous change", |cx| {
            shell.read_with(cx, |shell, _| {
                shell.revisions.notice(Domain::Lighting(2)) == Some("Changed by another client")
            })
        });
    }

    /// Every write the window sends expects the revision its editor was built
    /// from: the agent moved cooling to revision 2, the window adopted it, and
    /// the window's own edit goes out expecting 2.
    #[gpui::test]
    fn every_write_carries_the_revision_its_editor_was_built_from(cx: &mut gpui::TestAppContext) {
        let harness = full_machine("shell-expect");
        let proxy = Proxy::start("shell-expect", harness.socket());
        let (shell, cx) = shell_on(proxy.path().to_path_buf(), Duration::from_secs(1), cx);
        pump(cx, "the window to connect", |cx| connected(&shell, cx));

        let mut korictl = agent(&harness, Some("korictl"));
        korictl
            .apply(CoolingProgram::Fixed {
                pump: 190,
                fan: 110,
            })
            .unwrap();
        korictl.apply_lighting(lighting(1, "ff8800")).unwrap();
        pump(cx, "the adoption", |cx| {
            shell.read_with(cx, |shell, _| {
                shell.expected_revision(Domain::Cooling) == Some(2)
                    && shell.expected_revision(Domain::Lighting(1)) == Some(2)
            })
        });

        shell.update(cx, |shell, cx| {
            shell.cooling.set_duty(Channel::Pump, 200);
            shell.schedule_write(WriteTarget::Cooling, cx);
            if let Some(row) = shell.lighting.channel_mut(1) {
                row.color = "0088FF".to_string();
            }
            shell.schedule_write(WriteTarget::Lighting(LightingRow::Channel(1)), cx);
            if let Ok(mut picture) = shell.lcd.preset() {
                picture.mode = DisplayMode::SingleReading;
                shell.lcd.adopt(&picture);
            }
            shell.schedule_write(WriteTarget::Lighting(LightingRow::Lcd), cx);
        });
        settle_writes(cx);
        wait_until("the three writes", PATIENCE, || {
            proxy.requests("apply_program").len() == 1
                && proxy.requests("apply_lighting").len() == 1
                && proxy.requests("apply_display").len() == 1
        });
        assert_eq!(proxy.requests("apply_program")[0]["expect_revision"], 2);
        assert_eq!(proxy.requests("apply_lighting")[0]["expect_revision"], 2);
        assert_eq!(proxy.requests("apply_display")[0]["expect_revision"], 1);
    }

    /// The operator drags a duty while the agent changes cooling. Nothing moves
    /// under the pointer; the drag's write goes out expecting the old revision,
    /// is refused with `conflict`, and the window then takes the agent's
    /// program, names the agent, and never sends the abandoned edit again.
    #[gpui::test]
    fn a_drag_that_loses_to_another_client_is_dropped_rather_than_resent(
        cx: &mut gpui::TestAppContext,
    ) {
        let harness = full_machine("shell-conflict");
        let proxy = Proxy::start("shell-conflict", harness.socket());
        let (shell, cx) = shell_on(proxy.path().to_path_buf(), Duration::from_secs(1), cx);
        pump(cx, "the window to connect", |cx| connected(&shell, cx));

        // The drag starts and moves the pump.
        shell.update(cx, |shell, _| {
            shell.cooling.set_mode(crate::cooling::CoolingMode::Fixed);
            shell.interaction.hold(Drag::Duty {
                channel: Channel::Pump,
                track: gpui::Bounds::default(),
            });
            shell.cooling.set_duty(Channel::Pump, 230);
            shell.touch_cooling();
        });
        let dragged = shell.read_with(cx, |shell, _| shell.cooling.program());

        let mut korictl = agent(&harness, Some("korictl"));
        let theirs = CoolingProgram::Fixed {
            pump: 160,
            fan: 100,
        };
        korictl.apply(theirs.clone()).unwrap();
        pump(cx, "the agent's status to arrive", |cx| {
            shell.read_with(cx, |shell, _| {
                shell
                    .link
                    .status()
                    .is_some_and(|status| status.cooling.revision.revision == 2)
            })
        });
        shell.read_with(cx, |shell, _| {
            assert_eq!(
                shell.cooling.program(),
                dragged,
                "nothing moved under the pointer"
            );
            assert_eq!(shell.expected_revision(Domain::Cooling), Some(1));
        });

        // The drag ends: its write goes out, and loses.
        shell.update(cx, |shell, cx| {
            shell.interaction.end_drag();
            shell.schedule_write(WriteTarget::Cooling, cx);
        });
        settle_writes(cx);
        pump(cx, "the conflict to be adopted", |cx| {
            shell.read_with(cx, |shell, _| shell.cooling.program() == theirs)
        });
        shell.read_with(cx, |shell, _| {
            assert_eq!(
                shell.revisions.notice(Domain::Cooling),
                Some("Changed by korictl")
            );
            let outcome = shell.outcome.as_ref().unwrap();
            assert!(outcome.conflict.is_some(), "{outcome:?}");
            assert!(!shell.due.is_pending(WriteTarget::Cooling));
        });
        let sent = proxy.requests("apply_program");
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0]["expect_revision"], 1);

        // Nothing more goes out for the abandoned edit, however long it waits.
        for _ in 0..3 {
            settle_writes(cx);
        }
        assert_eq!(proxy.requests("apply_program").len(), 1);
    }

    // ---- US-011: Settings ---------------------------------------------------

    /// The Diagnostics block is one sentence, with or without a daemon, and the
    /// screen's tab stops are the four rail entries: the dead Export button was
    /// the only control of its own, and it is gone.
    #[gpui::test]
    fn settings_says_where_diagnostics_are_exported_and_offers_no_dead_control(
        cx: &mut gpui::TestAppContext,
    ) {
        let harness = Harness::start("shell-settings");
        let (with_daemon, cx) =
            shell_on(harness.socket().to_path_buf(), Duration::from_secs(1), cx);
        pump(cx, "the window to connect", |cx| {
            connected(&with_daemon, cx)
        });
        with_daemon.update(cx, |shell, cx| shell.go(Destination::Settings, cx));
        draw(&with_daemon, cx);
        let connected_row = cx
            .debug_bounds("settings-diagnostics")
            .expect("the row is drawn");

        let (without, cx) = super::tests::shell(&mut cx.cx);
        without.update(cx, |shell, cx| shell.go(Destination::Settings, cx));
        draw(&without, cx);
        let disconnected_row = cx
            .debug_bounds("settings-diagnostics")
            .expect("the row is drawn");
        // The same row either way, to the half pixel layout rounding moves a
        // row by when the rows above it differ.
        assert_eq!(connected_row.size.width, disconnected_row.size.width);
        assert!(
            (connected_row.size.height - disconnected_row.size.height).abs() < gpui::px(1.0),
            "{connected_row:?} against {disconnected_row:?}"
        );
        assert!(DIAGNOSTICS_HINT.contains("korictl diagnostics"));

        // Keyboard alone: Tab walks every stop and comes back round, and every
        // one of them is a control that exists.
        let root = without.read_with(cx, |shell, _| shell.focus.clone());
        let mut visited = Vec::new();
        for _ in 0..8 {
            cx.dispatch_action(FocusNext);
            let focused = cx
                .update(|window, cx| window.focused(cx))
                .expect("a stop has focus");
            assert_ne!(focused, root, "Tab landed on nothing");
            if !visited.contains(&focused) {
                visited.push(focused);
            }
        }
        assert_eq!(visited.len(), 4, "the four rail entries and nothing else");
    }

    // ---- US-012: repaint only what shows ------------------------------------

    /// With Settings shown and a status that does not change, ten pushed
    /// samples cost no repaint at all.
    #[gpui::test]
    fn settings_takes_ten_unchanged_samples_without_a_repaint(cx: &mut gpui::TestAppContext) {
        let harness = Harness::start("shell-quiet");
        let (shell, cx) = shell_on(harness.socket().to_path_buf(), Duration::from_secs(1), cx);
        pump(cx, "the window to connect", |cx| connected(&shell, cx));
        shell.update(cx, |shell, cx| shell.go(Destination::Settings, cx));
        // The window's own connections settle into the status first.
        std::thread::sleep(Duration::from_millis(300));
        settle_frame(&shell, cx);

        let count = repaints(&shell, cx);
        let first = shell.read_with(cx, |shell, _| shell.link.telemetry().unwrap().sequence);
        pump(cx, "ten samples", |cx| {
            shell.read_with(cx, |shell, _| {
                shell.link.telemetry().unwrap().sequence >= first + 10
            })
        });
        assert_eq!(count.get(), 0, "an unchanged Settings screen was repainted");

        // A change Settings shows still repaints it.
        shell.update(cx, |shell, cx| {
            let link = with_status(&shell.link, |status| status.daemon_version = "9.9.9".into());
            shell.absorb(Some(link), None, None, cx);
        });
        assert_eq!(count.get(), 1);
    }

    /// Cooling shows the Kraken and nothing about the GPU.
    #[gpui::test]
    fn cooling_ignores_a_sample_that_moved_only_the_gpu_load(cx: &mut gpui::TestAppContext) {
        let (shell, cx) = idle_shell(cx);
        let link = fresh_link();
        shell.update(cx, |shell, cx| {
            shell.go(Destination::Cooling, cx);
            shell.absorb(Some(link.clone()), None, None, cx);
        });
        settle_frame(&shell, cx);
        let count = repaints(&shell, cx);

        let gpu = with_snapshot(&link, |snapshot| {
            snapshot.sequence += 1;
            snapshot.gpu.load_percent = kori_core::telemetry::Reading::valid(87.0);
        });
        shell.update(cx, |shell, cx| {
            shell.absorb(Some(gpu.clone()), None, None, cx)
        });
        assert_eq!(count.get(), 0, "a GPU load change repainted Cooling");

        let pump_speed = with_snapshot(&gpu, |snapshot| {
            snapshot.kraken.pump.rpm = kori_core::telemetry::Reading::valid(2_400);
        });
        shell.update(cx, |shell, cx| {
            shell.absorb(Some(pump_speed), None, None, cx)
        });
        assert_eq!(
            count.get(),
            1,
            "the pump speed Cooling shows did repaint it"
        );
    }

    /// The coolant marker on the curve follows the latest raw reading, so a
    /// failed read that drops it repaints Cooling even though the readout
    /// above still holds its last value.
    #[gpui::test]
    fn a_failed_coolant_read_repaints_the_curve_marker(cx: &mut gpui::TestAppContext) {
        let (shell, cx) = idle_shell(cx);
        let link = fresh_link();
        shell.update(cx, |shell, cx| {
            shell.go(Destination::Cooling, cx);
            shell.absorb(Some(link.clone()), None, None, cx);
        });
        settle_frame(&shell, cx);
        let count = repaints(&shell, cx);

        let failed = with_snapshot(&link, |snapshot| {
            snapshot.sequence += 1;
            snapshot.kraken.liquid_temperature_c = kori_core::telemetry::Reading::unavailable(
                kori_core::telemetry::Unavailable::Unreadable {
                    detail: "read failed".to_string(),
                },
            );
        });
        shell.update(cx, |shell, cx| shell.absorb(Some(failed), None, None, cx));
        assert_eq!(
            count.get(),
            1,
            "the marker the curve dropped stayed painted"
        );
    }

    /// A flush that sends nothing changes nothing on screen.
    #[gpui::test]
    fn a_flush_repaints_only_when_a_write_left(cx: &mut gpui::TestAppContext) {
        let (shell, cx) = shell(cx);
        shell.update(cx, |shell, cx| {
            assert!(!shell.flush_writes(), "nothing was due");
            shell.cooling.set_mode(crate::cooling::CoolingMode::Fixed);
            shell.schedule_write(WriteTarget::Cooling, cx);
        });
        std::thread::sleep(COOLING_QUIET + Duration::from_millis(60));
        shell.update(cx, |shell, _| {
            assert!(shell.flush_writes(), "the due write left")
        });
    }

    /// The panel preview is built while Lighting shows the panel row, once when
    /// it comes into view, and never for a screen nobody is looking at.
    #[gpui::test]
    fn the_panel_preview_is_built_only_while_it_is_on_screen(cx: &mut gpui::TestAppContext) {
        let (shell, cx) = idle_shell(cx);
        let link = fresh_link();
        // The default preset reads the CPU and the GPU.
        let warmer = with_snapshot(&link, |snapshot| {
            snapshot.system.cpu_temperature_c = kori_core::telemetry::Reading::valid(71.0);
        });
        shell.update(cx, |shell, cx| {
            shell.absorb(Some(link.clone()), None, None, cx);
            assert_eq!(shell.picture_version, 0, "Monitoring builds no preview");
            assert!(shell.lcd.picture().is_none());

            shell.go(Destination::Lighting, cx);
            assert_eq!(shell.picture_version, 1, "built once on coming into view");
            assert!(shell.lcd.picture().is_some());
            shell.absorb(Some(link.clone()), None, None, cx);
            assert_eq!(
                shell.picture_version, 1,
                "an unchanged sample rebuilds nothing"
            );
            shell.absorb(Some(warmer.clone()), None, None, cx);
            assert_eq!(shell.picture_version, 2, "a reading the panel shows moved");

            shell.go(Destination::Settings, cx);
            shell.absorb(Some(link.clone()), None, None, cx);
            assert_eq!(shell.picture_version, 2, "Settings rebuilds no preview");

            shell.go(Destination::Lighting, cx);
            shell.toggle_lighting_row(LightingRow::Lcd, cx);
            shell.absorb(Some(warmer.clone()), None, None, cx);
            assert_eq!(shell.picture_version, 3, "the return built it once");
            shell.absorb(Some(link.clone()), None, None, cx);
            assert_eq!(shell.picture_version, 3, "a shut row rebuilds nothing");
        });
    }

    /// An animated picture stops when Lighting is left and starts again on the
    /// frame the daemon's clock implies.
    #[gpui::test]
    fn an_animation_stops_off_screen_and_resumes_on_the_frame_the_glass_shows(
        cx: &mut gpui::TestAppContext,
    ) {
        let directory = kori_lcd_renderer::testing::scratch("shell-film").unwrap();
        let path = directory.join("loop.gif");
        let frames: Vec<(Rgb, u16)> = (0..3u8)
            .map(|index| (Rgb::new(index * 80, 0x40, 0x80), 20))
            .collect();
        kori_lcd_renderer::testing::write_gif(&path, 16, &frames).unwrap();
        let picture = DisplayPreset {
            mode: DisplayMode::Image,
            image: Some(path.clone()),
            ..DisplayPreset::default_infographic()
        };
        // Started a frame and a half ago, at 200 ms a frame.
        let started = now_unix_ms() - 300;
        let link = with_status(&fresh_link(), |status| {
            status.display.committed = Some(picture.clone());
            status.display.animation_started_at_unix_ms = Some(started);
        });

        let (shell, cx) = idle_shell(cx);
        shell.update(cx, |shell, cx| {
            shell.absorb(Some(link.clone()), None, None, cx);
            shell.go(Destination::Lighting, cx);
        });
        let (frame_one, generation) = shell.read_with(cx, |shell, _| {
            let mut film = shell.lcd.film().expect("the picture compiled").clone();
            assert_eq!(film.frame_count(), 3);
            let delay = film.delay().unwrap();
            film.seek(delay + delay / 2);
            assert_eq!(
                shell.lcd.picture(),
                film.frame(),
                "the preview resumed on the frame the glass shows"
            );
            (film.frame().unwrap().to_vec(), shell.lcd.film_generation())
        });

        shell.update(cx, |shell, cx| shell.go(Destination::Settings, cx));
        cx.executor().advance_clock(Duration::from_secs(2));
        cx.run_until_parked();
        shell.read_with(cx, |shell, _| {
            assert_ne!(
                shell.lcd.film_generation(),
                generation,
                "leaving stopped the timer"
            );
            assert_eq!(
                shell.lcd.picture(),
                Some(frame_one.as_slice()),
                "nothing advanced off screen"
            );
        });
        let _ = std::fs::remove_file(path);
    }

    /// An edit pending when the screen changes is still written once its quiet
    /// period is over.
    #[gpui::test]
    fn an_edit_pending_when_the_screen_changes_is_still_written(cx: &mut gpui::TestAppContext) {
        let harness = Harness::start("shell-pending");
        let proxy = Proxy::start("shell-pending", harness.socket());
        let (shell, cx) = shell_on(proxy.path().to_path_buf(), Duration::from_secs(1), cx);
        pump(cx, "the window to connect", |cx| connected(&shell, cx));
        shell.update(cx, |shell, cx| {
            shell.go(Destination::Cooling, cx);
            shell.cooling.set_mode(crate::cooling::CoolingMode::Fixed);
            shell.cooling.set_duty(Channel::Pump, 210);
            shell.schedule_write(WriteTarget::Cooling, cx);
            shell.go(Destination::Settings, cx);
        });
        settle_writes(cx);
        wait_until("the write", PATIENCE, || {
            proxy.requests("apply_program").len() == 1
        });
    }

    // ---- US-013: the stream -------------------------------------------------

    /// Kraken readings that stopped advancing still disable the cooling writes,
    /// whatever else keeps arriving on the stream.
    #[gpui::test]
    fn stale_kraken_telemetry_still_disables_cooling_writes(cx: &mut gpui::TestAppContext) {
        let (shell, cx) = idle_shell(cx);
        let stale = with_snapshot(&fresh_link(), |snapshot| {
            snapshot.kraken.at_unix_ms = now_unix_ms() - 5_000;
        });
        shell.update(cx, |shell, cx| {
            shell.absorb(Some(stale), None, None, cx);
            let state = shell.link.cooling_state(
                kori_core::KRAKEN_BASE,
                CapabilityId::PumpDuty,
                shell.now_unix_ms,
            );
            assert!(!state.is_enabled(), "{state:?}");
        });
    }

    /// The window and the stream end to end: pushed samples move the dials,
    /// and the window re-establishes both connections after a restart with
    /// the chart asked for again.
    #[gpui::test]
    fn the_window_follows_the_stream_and_a_restart(cx: &mut gpui::TestAppContext) {
        let mut first = Harness::start("shell-restart-first");
        let proxy = Proxy::start("shell-restart", first.socket());
        let (shell, cx) = shell_on(proxy.path().to_path_buf(), Duration::from_secs(1), cx);
        pump(cx, "the window to connect", |cx| connected(&shell, cx));
        let sequence = shell.read_with(cx, |shell, _| shell.link.telemetry().unwrap().sequence);
        pump(cx, "pushed samples", |cx| {
            shell.read_with(cx, |shell, _| {
                shell.link.telemetry().unwrap().sequence > sequence + 3
            })
        });
        assert!(
            proxy.requests("status").len() <= 1,
            "the window polled status"
        );

        first.stop();
        pump(cx, "the unavailable state", |cx| !connected(&shell, cx));
        let asked = proxy.requests("history").len();
        let second = Harness::start("shell-restart-second");
        proxy.retarget(second.socket());
        pump(cx, "the reconnection", |cx| connected(&shell, cx));
        wait_until("the chart to be asked for again", PATIENCE, || {
            proxy.requests("history").len() > asked
        });
    }
}

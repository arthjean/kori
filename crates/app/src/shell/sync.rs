// SPDX-FileCopyrightText: 2026 Arthur Jean
// SPDX-License-Identifier: GPL-3.0-or-later

//! Following what other clients change.
//!
//! The window is no longer the only client. An agent driving `korictl` writes
//! the same three domains the editors hold, and a window that kept its own
//! edit over that change would write it back on the next settle. So each
//! domain remembers the revision its editor was built from, every write
//! carries it as `expect_revision`, and a status whose revision moved past it
//! is adopted: the editor takes the committed state, and a change another
//! connection made is named under the control until the operator edits it.
//!
//! An edit in progress is not overwritten. It waits, its write goes out with
//! the old revision, the daemon refuses it with `conflict`, and only then is
//! the committed state adopted. That ordering is what the operator sees as
//! "my drag lost to the agent's write", rather than the curve jumping under
//! the pointer mid-gesture.

use std::collections::BTreeMap;

use gpui::{Div, div, prelude::*};
use kori_core::ipc::{Attribution, Revision};

use crate::feed::{CommandSubject, changed_by_sentence};
use crate::theme::{color, space, text};

use super::Shell;
use super::screen::drag::Drag;
use super::screen::row::LightingRow;
use super::screen::write::WriteTarget;

/// A write domain the window keeps an editor for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Domain {
    Cooling,
    Lighting(u8),
    Panel,
}

impl Domain {
    /// The pending write that edits this domain.
    pub fn target(self) -> WriteTarget {
        match self {
            Self::Cooling => WriteTarget::Cooling,
            Self::Lighting(channel) => WriteTarget::Lighting(LightingRow::Channel(channel)),
            Self::Panel => WriteTarget::Lighting(LightingRow::Lcd),
        }
    }

    /// The domain a write target edits.
    pub fn of_target(target: WriteTarget) -> Self {
        match target {
            WriteTarget::Cooling => Self::Cooling,
            WriteTarget::Lighting(LightingRow::Channel(channel)) => Self::Lighting(channel),
            WriteTarget::Lighting(LightingRow::Lcd) => Self::Panel,
        }
    }

    /// The domain a command outcome is about, when it is a write to one.
    pub fn of_subject(subject: CommandSubject) -> Option<Self> {
        match subject {
            CommandSubject::Cooling => Some(Self::Cooling),
            CommandSubject::Lighting(channel) => Some(Self::Lighting(channel)),
            CommandSubject::Panel => Some(Self::Panel),
            CommandSubject::Profiles => None,
        }
    }
}

/// What one status means for one domain's editor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Take the committed state. `notice` names who changed it, when that was
    /// another connection.
    Adopt { notice: Option<String> },
    /// The window's own write landed while the operator kept editing: the
    /// revision moves on and the editor keeps the newer edit.
    Advance,
    /// Another client changed it while the operator is editing. Nothing moves
    /// until that edit's write is refused with `conflict`.
    Wait,
    /// Nothing new.
    Keep,
}

/// Decide, without a window: free and pure for the reason
/// [`super::screen::write::write_is_held_back`] is.
///
/// `basis` is the revision the editor was built from, `None` before the first
/// status. `own` is the connection this window writes on.
pub fn decide(basis: Option<u64>, current: &Revision, own: Option<u64>, editing: bool) -> Decision {
    let Some(basis) = basis else {
        // The first status: whatever is committed is what the machine runs,
        // and the window opens on it rather than on its own defaults.
        return Decision::Adopt { notice: None };
    };
    if current.revision <= basis {
        return Decision::Keep;
    }
    let mine = matches!(
        current.changed_by,
        Attribution::Client { connection, .. } if Some(connection) == own
    );
    match (mine, editing) {
        (true, true) => Decision::Advance,
        (true, false) => Decision::Adopt { notice: None },
        (false, true) => Decision::Wait,
        (false, false) => Decision::Adopt {
            notice: Some(changed_by_sentence(&current.changed_by)),
        },
    }
}

/// The revision each editor was built from, and who to name under it.
#[derive(Debug, Default)]
pub struct Revisions {
    /// The daemon these revisions belong to. Revisions restart with it.
    started_at: Option<u64>,
    basis: BTreeMap<Domain, u64>,
    notices: BTreeMap<Domain, String>,
}

impl Revisions {
    /// Forget every revision when the daemon is a different one.
    pub fn follow_daemon(&mut self, started_at_unix_ms: u64) {
        if self.started_at != Some(started_at_unix_ms) {
            self.started_at = Some(started_at_unix_ms);
            self.basis.clear();
            self.notices.clear();
        }
    }

    /// The revision a write to `domain` must expect.
    pub fn basis(&self, domain: Domain) -> Option<u64> {
        self.basis.get(&domain).copied()
    }

    pub fn set_basis(&mut self, domain: Domain, revision: u64) {
        self.basis.insert(domain, revision);
    }

    /// "Changed by ..." for `domain`, while it stands.
    pub fn notice(&self, domain: Domain) -> Option<&str> {
        self.notices.get(&domain).map(String::as_str)
    }

    pub fn set_notice(&mut self, domain: Domain, notice: Option<String>) {
        match notice {
            Some(notice) => {
                self.notices.insert(domain, notice);
            }
            None => {
                self.notices.remove(&domain);
            }
        }
    }

    /// The operator edited `domain`: the notice has been read.
    pub fn edited(&mut self, domain: Domain) {
        self.notices.remove(&domain);
    }
}

impl Shell {
    /// Follow every domain the last status reported, and say whether an
    /// editor changed.
    pub(crate) fn follow_revisions(&mut self) -> bool {
        // A clone of the link, which is a handful of reference counts, so the
        // status can be read while the editors it feeds are written.
        let link = self.link.clone();
        let Some(status) = link.status() else {
            return false;
        };
        self.revisions.follow_daemon(status.started_at_unix_ms);
        let own = link.connection();
        let mut changed = false;

        let editing = self.is_editing(Domain::Cooling);
        changed |= self.settle(
            Domain::Cooling,
            &status.cooling.revision,
            own,
            editing,
            |shell| {
                if let Some(program) = &status.cooling.committed {
                    shell.cooling.adopt(program);
                }
            },
        );

        for state in &status.lighting {
            let domain = Domain::Lighting(state.channel);
            let editing = self.is_editing(domain);
            changed |= self.settle(domain, &state.revision, own, editing, |shell| {
                if let Some(program) = &state.committed {
                    shell.lighting.adopt(state.channel, program);
                }
            });
        }

        let editing = self.is_editing(Domain::Panel);
        changed |= self.settle(
            Domain::Panel,
            &status.display.revision,
            own,
            editing,
            |shell| {
                if let Some(preset) = &status.display.committed {
                    shell.lcd.adopt(preset);
                }
            },
        );
        changed
    }

    /// Apply one decision to one domain.
    fn settle(
        &mut self,
        domain: Domain,
        current: &Revision,
        own: Option<u64>,
        editing: bool,
        adopt: impl FnOnce(&mut Self),
    ) -> bool {
        match decide(self.revisions.basis(domain), current, own, editing) {
            Decision::Adopt { notice } => {
                adopt(self);
                self.revisions.set_basis(domain, current.revision);
                self.revisions.set_notice(domain, notice);
                true
            }
            Decision::Advance => {
                self.revisions.set_basis(domain, current.revision);
                false
            }
            Decision::Wait | Decision::Keep => false,
        }
    }

    /// Whether the operator has an edit of `domain` the daemon has not taken
    /// yet: a write waiting out its quiet period, a gesture still running, or
    /// a cooling write whose outcome has not come back.
    pub(crate) fn is_editing(&self, domain: Domain) -> bool {
        let drag = self.interaction.drag();
        let pending = self.due.is_pending(domain.target());
        match domain {
            Domain::Cooling => pending || self.sent.is_some() || drag.is_some_and(Drag::is_cooling),
            Domain::Lighting(channel) => {
                pending
                    || matches!(drag, Some(Drag::Brightness { row: LightingRow::Channel(held), .. }) if held == channel)
            }
            Domain::Panel => {
                pending
                    || matches!(
                        drag,
                        Some(Drag::Brightness {
                            row: LightingRow::Lcd,
                            ..
                        })
                    )
            }
        }
    }

    /// The revision a write to `domain` must expect.
    pub(crate) fn expected_revision(&self, domain: Domain) -> Option<u64> {
        self.revisions.basis(domain)
    }

    /// "Changed by ..." under the control another client changed, until the
    /// operator edits it.
    pub(crate) fn change_notice(&self, domain: Domain) -> Option<Div> {
        let notice = self.revisions.notice(domain)?;
        Some(
            div()
                .px(space::MD)
                .pb(space::XS)
                .text_size(text::LABEL_SM)
                .text_color(color::EXTERNAL_CHANGE.hsla())
                .child(notice.to_string()),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn by(connection: u64, client: Option<&str>) -> Revision {
        Revision {
            revision: 5,
            changed_by: Attribution::Client {
                connection,
                client: client.map(str::to_string),
                pid: 42,
            },
        }
    }

    #[test]
    fn the_first_status_is_adopted_silently() {
        assert_eq!(
            decide(None, &by(9, Some("korictl")), Some(1), false),
            Decision::Adopt { notice: None }
        );
    }

    #[test]
    fn another_clients_change_is_adopted_and_named() {
        assert_eq!(
            decide(Some(4), &by(9, Some("korictl")), Some(1), false),
            Decision::Adopt {
                notice: Some("Changed by korictl".to_string())
            }
        );
        assert_eq!(
            decide(Some(4), &by(9, None), Some(1), false),
            Decision::Adopt {
                notice: Some("Changed by another client".to_string())
            },
            "a connection with no name is still another client"
        );
    }

    #[test]
    fn an_edit_in_progress_waits_for_its_conflict_rather_than_being_overwritten() {
        assert_eq!(
            decide(Some(4), &by(9, Some("korictl")), Some(1), true),
            Decision::Wait
        );
    }

    #[test]
    fn the_windows_own_write_moves_the_basis_without_naming_anyone() {
        assert_eq!(
            decide(Some(4), &by(1, Some("kori")), Some(1), false),
            Decision::Adopt { notice: None }
        );
        assert_eq!(
            decide(Some(4), &by(1, Some("kori")), Some(1), true),
            Decision::Advance,
            "a newer edit outranks the write it follows, and the next write \
             must expect the revision that write produced"
        );
    }

    #[test]
    fn a_revision_already_seen_changes_nothing() {
        assert_eq!(
            decide(Some(5), &by(9, Some("korictl")), Some(1), false),
            Decision::Keep
        );
        // A push computed before the window's own write and delivered after
        // the status read that followed it carries an older revision; it must
        // not be taken for news.
        assert_eq!(
            decide(Some(6), &by(9, Some("korictl")), Some(1), false),
            Decision::Keep
        );
    }

    #[test]
    fn a_restarted_daemon_voids_every_revision_and_notice() {
        let mut revisions = Revisions::default();
        revisions.follow_daemon(100);
        revisions.set_basis(Domain::Cooling, 7);
        revisions.set_notice(Domain::Panel, Some("Changed by korictl".into()));
        revisions.follow_daemon(100);
        assert_eq!(revisions.basis(Domain::Cooling), Some(7));
        revisions.follow_daemon(200);
        assert_eq!(revisions.basis(Domain::Cooling), None);
        assert_eq!(revisions.notice(Domain::Panel), None);
    }

    #[test]
    fn every_domain_maps_to_the_write_that_edits_it_and_back() {
        for domain in [Domain::Cooling, Domain::Lighting(2), Domain::Panel] {
            assert_eq!(Domain::of_target(domain.target()), domain);
        }
        assert_eq!(Domain::of_subject(CommandSubject::Profiles), None);
        assert_eq!(
            Domain::of_subject(CommandSubject::Lighting(3)),
            Some(Domain::Lighting(3))
        );
    }
}

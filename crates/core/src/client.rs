// SPDX-FileCopyrightText: 2026 Arthur Jean
// SPDX-License-Identifier: GPL-3.0-or-later

//! The client side of the local socket.
//!
//! Lives next to the protocol so the GPUI process and the integration tests
//! speak it through the same code, including the frame ceiling.

use std::io::{BufReader, BufWriter};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

use crate::ipc::{
    DaemonStatus, FrameError, HEARTBEAT_INTERVAL, IpcError, PROTOCOL_VERSION, Push, Request,
    Response, Topic, read_frame, write_frame,
};

/// Why talking to the daemon failed.
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("the daemon is not running, or its socket is not reachable at {path}: {source}")]
    Unreachable {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("the connection to the daemon was lost: {0}")]
    Disconnected(FrameError),
    #[error("{0}")]
    Refused(IpcError),
    #[error("the daemon answered {actual} where {expected} was expected")]
    Unexpected {
        expected: &'static str,
        actual: &'static str,
    },
}

impl ClientError {
    /// One sentence a control surface can display verbatim.
    pub fn operator_message(&self) -> String {
        match self {
            Self::Unreachable { .. } => {
                "The background service is not running. Start korid to enable controls.".to_string()
            }
            other => other.to_string(),
        }
    }

    /// A framing failure on the way out.
    ///
    /// A request refused for its size never left this process, so it is a typed
    /// refusal and not a lost connection. Reporting it as a disconnection would
    /// describe something that did not happen and send the operator looking for
    /// a daemon that is still there.
    fn from_write(error: FrameError) -> Self {
        match error {
            FrameError::TooLarge { max_bytes } => {
                Self::Refused(IpcError::FrameTooLarge { max_bytes })
            }
            other => Self::Disconnected(other),
        }
    }
}

/// How long a read or a write on the socket may block before it fails.
///
/// What [`Client::connect`] uses, and what a caller that has no reason to pick
/// another value should use too.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(5);

/// A connected client that has completed the version handshake.
pub struct Client {
    reader: BufReader<UnixStream>,
    writer: BufWriter<UnixStream>,
    daemon_version: String,
    connection: u64,
}

impl Client {
    /// Connect and handshake, with [`DEFAULT_TIMEOUT`] on every read and write.
    pub fn connect(path: &Path) -> Result<Self, ClientError> {
        Self::connect_with_timeout(path, DEFAULT_TIMEOUT)
    }

    /// Connect and handshake, failing any read or write that blocks longer
    /// than `timeout`.
    ///
    /// The bound applies to each operation, the handshake included, not to the
    /// conversation as a whole. A zero duration is refused by the kernel and
    /// reported as an unreachable socket rather than silently meaning "forever".
    pub fn connect_with_timeout(path: &Path, timeout: Duration) -> Result<Self, ClientError> {
        Self::connect_as(path, timeout, None)
    }

    /// Connect and handshake under a client name.
    ///
    /// The name is sent as given and judged by the daemon, which refuses one
    /// outside [`crate::ipc::validate_client_name`] with `malformed` and closes
    /// the connection. It labels this connection's writes in events and in
    /// `status`; the process ID beside it comes from the kernel.
    pub fn connect_as(
        path: &Path,
        timeout: Duration,
        client: Option<&str>,
    ) -> Result<Self, ClientError> {
        let unreachable = |source| ClientError::Unreachable {
            path: path.display().to_string(),
            source,
        };

        let stream = UnixStream::connect(path).map_err(unreachable)?;
        // A hung daemon must not freeze the caller's thread forever. The
        // failure is reported rather than discarded: a connection whose
        // timeouts were refused does not carry that guarantee, and a caller
        // handed one anyway would wait on it without end.
        let timeout = Some(timeout);
        stream.set_read_timeout(timeout).map_err(unreachable)?;
        stream.set_write_timeout(timeout).map_err(unreachable)?;

        let write_half = stream.try_clone().map_err(unreachable)?;
        let mut reader = BufReader::new(stream);
        let mut writer = BufWriter::new(write_half);

        // The handshake runs on the streams themselves, before a `Client`
        // exists, so the type is never built around a daemon version nobody
        // has read yet.
        let hello = Request::Hello {
            protocol_version: PROTOCOL_VERSION,
            client: client.map(str::to_string),
        };
        write_frame(&mut writer, &hello).map_err(ClientError::from_write)?;
        let (daemon_version, connection) =
            match read_frame(&mut reader).map_err(ClientError::Disconnected)? {
                Response::Hello {
                    daemon_version,
                    connection,
                    ..
                } => (daemon_version, connection),
                Response::Error(error) => return Err(ClientError::Refused(error)),
                other => {
                    return Err(ClientError::Unexpected {
                        expected: "hello",
                        actual: other.kind(),
                    });
                }
            };

        Ok(Self {
            reader,
            writer,
            daemon_version,
            connection,
        })
    }

    pub fn daemon_version(&self) -> &str {
        &self.daemon_version
    }

    /// The number the daemon gave this connection in its handshake.
    pub fn connection(&self) -> u64 {
        self.connection
    }

    /// Send one request and read its response.
    pub fn request(&mut self, request: Request) -> Result<Response, ClientError> {
        write_frame(&mut self.writer, &request).map_err(ClientError::from_write)?;
        read_frame(&mut self.reader).map_err(ClientError::Disconnected)
    }

    /// Send a request frame the caller assembled, and take the one response
    /// shape it accepts.
    ///
    /// For a client that carries a payload it does not interpret, such as a
    /// command line handing on what an operator typed. The payload is not
    /// decoded into [`Request`] on this side, so the daemon is the one place an
    /// unknown field or an out-of-range value is judged, and its typed refusal
    /// reaches the caller unchanged. The answer is still decoded and checked
    /// against `expected`, so a reply of the wrong kind is reported as such.
    pub fn forward(
        &mut self,
        frame: &serde_json::Value,
        expected: &'static str,
    ) -> Result<Response, ClientError> {
        write_frame(&mut self.writer, frame).map_err(ClientError::from_write)?;
        match read_frame(&mut self.reader).map_err(ClientError::Disconnected)? {
            Response::Error(error) => Err(ClientError::Refused(error)),
            response if response.kind() == expected => Ok(response),
            other => Err(ClientError::Unexpected {
                expected,
                actual: other.kind(),
            }),
        }
    }

    /// Send one request and take the single response shape it accepts.
    ///
    /// A typed refusal comes back as one. Anything else names both what was
    /// expected and what arrived: an error that can only say "other response"
    /// is one an operator cannot act on and a maintainer cannot trace.
    fn expect<T>(
        &mut self,
        request: Request,
        expected: &'static str,
        take: impl FnOnce(Response) -> Option<T>,
    ) -> Result<T, ClientError> {
        match self.request(request)? {
            Response::Error(error) => Err(ClientError::Refused(error)),
            other => {
                let actual = other.kind();
                take(other).ok_or(ClientError::Unexpected { expected, actual })
            }
        }
    }

    /// Current daemon status.
    pub fn status(&mut self) -> Result<DaemonStatus, ClientError> {
        self.expect(Request::Status, "status", |response| match response {
            Response::Status(status) => Some(*status),
            _ => None,
        })
    }

    /// The versioned capability record.
    pub fn capabilities(&mut self) -> Result<crate::capability::CapabilityRecord, ClientError> {
        self.expect(
            Request::Capabilities,
            "capabilities",
            |response| match response {
                Response::Capabilities(record) => Some(*record),
                _ => None,
            },
        )
    }

    /// The most recent sampling pass.
    pub fn telemetry(&mut self) -> Result<crate::telemetry::TelemetrySnapshot, ClientError> {
        self.expect(Request::Telemetry, "telemetry", |response| match response {
            Response::Telemetry(snapshot) => Some(*snapshot),
            _ => None,
        })
    }

    /// Apply a cooling program and read what the hardware confirmed.
    pub fn apply(
        &mut self,
        program: crate::profile::CoolingProgram,
    ) -> Result<crate::ipc::ApplyOutcome, ClientError> {
        self.apply_expecting(program, None)
    }

    /// The same, refused with [`IpcError::Conflict`] unless the cooling
    /// revision is still `expect_revision` when the daemon takes the request.
    pub fn apply_expecting(
        &mut self,
        program: crate::profile::CoolingProgram,
        expect_revision: Option<u64>,
    ) -> Result<crate::ipc::ApplyOutcome, ClientError> {
        self.expect(
            Request::ApplyProgram {
                program,
                expect_revision,
            },
            "applied",
            |response| match response {
                Response::Applied(outcome) => Some(*outcome),
                _ => None,
            },
        )
    }

    /// Apply a lighting program to one channel.
    pub fn apply_lighting(
        &mut self,
        command: crate::lighting::LightingCommand,
    ) -> Result<crate::ipc::LightingOutcome, ClientError> {
        self.apply_lighting_expecting(command, None)
    }

    /// The same, conditional on the revision of the channel it names.
    pub fn apply_lighting_expecting(
        &mut self,
        command: crate::lighting::LightingCommand,
        expect_revision: Option<u64>,
    ) -> Result<crate::ipc::LightingOutcome, ClientError> {
        self.expect(
            Request::ApplyLighting {
                command,
                expect_revision,
            },
            "lit",
            |response| match response {
                Response::Lit(outcome) => Some(outcome),
                _ => None,
            },
        )
    }

    /// Show a preset on the Kraken's panel.
    pub fn apply_display(
        &mut self,
        preset: crate::display::DisplayPreset,
    ) -> Result<crate::ipc::DisplayOutcome, ClientError> {
        self.apply_display_expecting(preset, None)
    }

    /// The same, conditional on the panel's revision.
    pub fn apply_display_expecting(
        &mut self,
        preset: crate::display::DisplayPreset,
        expect_revision: Option<u64>,
    ) -> Result<crate::ipc::DisplayOutcome, ClientError> {
        self.expect(
            Request::ApplyDisplay {
                preset,
                expect_revision,
            },
            "shown",
            |response| match response {
                Response::Shown(outcome) => Some(*outcome),
                _ => None,
            },
        )
    }

    /// Store a profile, replacing one with the same name. Answers the name the
    /// daemon stored it under.
    pub fn save_profile(
        &mut self,
        profile: crate::profile::Profile,
    ) -> Result<String, ClientError> {
        self.expect(
            Request::SaveProfile { profile },
            "saved",
            |response| match response {
                Response::Saved { name } => Some(name),
                _ => None,
            },
        )
    }

    /// Make a stored profile active and read what the hardware did.
    pub fn activate_profile(
        &mut self,
        name: String,
    ) -> Result<crate::ipc::ActivationOutcome, ClientError> {
        self.expect(
            Request::ActivateProfile { name },
            "activated",
            |response| match response {
                Response::Activated(outcome) => Some(outcome),
                _ => None,
            },
        )
    }

    /// Remove a stored profile. Answers the removed name and, when it was the
    /// active one, the profile the daemon activated in its place.
    pub fn delete_profile(
        &mut self,
        name: String,
    ) -> Result<(String, Option<String>), ClientError> {
        self.expect(
            Request::DeleteProfile { name },
            "deleted",
            |response| match response {
                Response::Deleted {
                    name,
                    activated_instead,
                } => Some((name, activated_instead)),
                _ => None,
            },
        )
    }

    /// The redacted diagnostics export.
    pub fn diagnostics(&mut self) -> Result<crate::diagnostics::DiagnosticsExport, ClientError> {
        self.expect(
            Request::Diagnostics,
            "diagnostics",
            |response| match response {
                Response::Diagnostics(export) => Some(export),
                _ => None,
            },
        )
    }

    /// A page of the event log: after `after_seq` when given, the newest
    /// events otherwise.
    pub fn events(
        &mut self,
        after_seq: Option<u64>,
        limit: Option<u32>,
    ) -> Result<crate::diagnostics::EventPage, ClientError> {
        self.expect(
            Request::Events { after_seq, limit },
            "events",
            |response| match response {
                Response::Events(page) => Some(page),
                _ => None,
            },
        )
    }

    /// Recorded telemetry. Every argument left `None` takes the daemon's
    /// default.
    pub fn history(
        &mut self,
        metrics: Option<Vec<crate::history::HistoryMetric>>,
        window_s: Option<u32>,
        step_s: Option<u32>,
        points: bool,
    ) -> Result<crate::history::HistoryReply, ClientError> {
        let request = Request::History {
            metrics,
            window_s,
            step_s,
            points: Some(points),
        };
        self.expect(request, "history", |response| match response {
            Response::History(reply) => Some(*reply),
            _ => None,
        })
    }

    /// Turn this connection into a stream of pushes.
    ///
    /// Consumes the client: once subscribed, the connection carries nothing
    /// but [`Push`] frames, and closing it is how a subscriber unsubscribes.
    pub fn subscribe(
        self,
        topics: Vec<Topic>,
        interval_s: Option<u32>,
        after_seq: Option<u64>,
    ) -> Result<Subscription, ClientError> {
        let request = Request::Subscribe {
            topics,
            interval_s,
            after_seq,
        };
        let frame = serde_json::to_value(&request).map_err(|error| {
            ClientError::Refused(IpcError::Malformed {
                detail: error.to_string(),
            })
        })?;
        self.subscribe_frame(&frame)
    }

    /// The same, from a frame the caller assembled and does not interpret.
    ///
    /// For a command line handing on topics an operator typed: an unknown
    /// topic is the daemon's to refuse.
    pub fn subscribe_frame(
        mut self,
        frame: &serde_json::Value,
    ) -> Result<Subscription, ClientError> {
        let (topics, interval_s) = match self.forward(frame, "subscribed")? {
            Response::Subscribed { topics, interval_s } => (topics, interval_s),
            other => {
                return Err(ClientError::Unexpected {
                    expected: "subscribed",
                    actual: other.kind(),
                });
            }
        };
        let mut subscription = Subscription {
            reader: self.reader,
            _writer: self.writer,
            topics,
            interval_s,
        };
        // A read that waits for a heartbeat must outlast it, whatever bound
        // the caller set for single requests.
        subscription.set_read_timeout(HEARTBEAT_INTERVAL + std::time::Duration::from_secs(1))?;
        Ok(subscription)
    }

    /// Every stored profile plus the active one.
    pub fn profiles(&mut self) -> Result<(String, Vec<crate::profile::Profile>), ClientError> {
        self.expect(Request::Profiles, "profiles", |response| match response {
            Response::Profiles { active, profiles } => Some((active, profiles)),
            _ => None,
        })
    }
}

/// A connection the daemon pushes frames on.
///
/// Dropping it closes the connection, which is the whole of unsubscribing.
pub struct Subscription {
    reader: BufReader<UnixStream>,
    /// Held so the connection stays open in both directions until the
    /// subscription is dropped. Nothing is written on it.
    _writer: BufWriter<UnixStream>,
    topics: Vec<Topic>,
    interval_s: u32,
}

impl Subscription {
    /// The topics the daemon confirmed, deduplicated.
    pub fn topics(&self) -> &[Topic] {
        &self.topics
    }

    pub fn interval_s(&self) -> u32 {
        self.interval_s
    }

    /// Bound the wait for the next push.
    ///
    /// Defaults to one second past [`HEARTBEAT_INTERVAL`], which is how a dead
    /// daemon is told from a quiet one.
    pub fn set_read_timeout(&mut self, timeout: Duration) -> Result<(), ClientError> {
        self.reader
            .get_ref()
            .set_read_timeout(Some(timeout))
            .map_err(|error| ClientError::Disconnected(FrameError::Io(error)))
    }

    /// The next frame the daemon pushed.
    pub fn next_push(&mut self) -> Result<Push, ClientError> {
        read_frame(&mut self.reader).map_err(ClientError::Disconnected)
    }

    /// A handle that ends this subscription from another thread.
    ///
    /// A reader blocked in [`Self::next_push`] waits up to a heartbeat and a
    /// second for the next frame. A client shutting down cannot wait that
    /// long, so it closes the socket under the reader instead, which returns
    /// the blocked read at once.
    pub fn closer(&self) -> Result<SubscriptionCloser, ClientError> {
        self.reader
            .get_ref()
            .try_clone()
            .map(|stream| SubscriptionCloser { stream })
            .map_err(|error| ClientError::Disconnected(FrameError::Io(error)))
    }
}

/// Ends a [`Subscription`] from outside the thread reading it.
pub struct SubscriptionCloser {
    stream: UnixStream,
}

impl SubscriptionCloser {
    /// Shut the connection in both directions. The daemon sees the client
    /// leave, which is the whole of unsubscribing, and the reader's next read
    /// fails rather than waiting out its timeout.
    pub fn close(&self) {
        let _ = self.stream.shutdown(std::net::Shutdown::Both);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The message an operator reads has to name what actually arrived. The
    /// field used to be the constant "other response" on every site, which
    /// produced a sentence that was both ungrammatical and empty.
    #[test]
    fn an_unexpected_answer_names_what_arrived_and_what_was_wanted() {
        let error = ClientError::Unexpected {
            expected: "status",
            actual: Response::Saved {
                name: "Silent".into(),
            }
            .kind(),
        };
        let message = error.to_string();
        assert!(message.contains("saved"), "{message}");
        assert!(message.contains("status"), "{message}");
        assert_eq!(error.operator_message(), message);
    }

    /// A request refused for its size never reached the socket. Calling that a
    /// lost connection would send the operator to restart a daemon that is
    /// running and would have answered.
    #[test]
    fn an_oversized_request_is_a_refusal_rather_than_a_disconnection() {
        let refused = ClientError::from_write(FrameError::TooLarge {
            max_bytes: crate::ipc::MAX_FRAME_BYTES,
        });
        match refused {
            ClientError::Refused(IpcError::FrameTooLarge { max_bytes }) => {
                assert_eq!(max_bytes, crate::ipc::MAX_FRAME_BYTES);
            }
            other => panic!("expected a typed refusal, got {other:?}"),
        }

        // A genuine transport failure still reads as a lost connection.
        let lost = ClientError::from_write(FrameError::Closed);
        assert!(matches!(
            lost,
            ClientError::Disconnected(FrameError::Closed)
        ));
    }

    /// The one message the client rewrites, because a raw connection error
    /// tells an operator nothing about what to do next.
    #[test]
    fn an_unreachable_daemon_is_explained_in_operator_language() {
        let error = ClientError::Unreachable {
            path: "/run/user/1000/kori/kori.sock".into(),
            source: std::io::Error::from(std::io::ErrorKind::NotFound),
        };
        let message = error.operator_message();
        assert!(message.contains("korid"), "{message}");
        assert!(!message.contains("/run/user"), "{message}");
    }
}

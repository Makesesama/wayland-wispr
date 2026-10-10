//! Session state machine.
//!
//! The [`Controller`] is pure: it consumes commands/internal events and returns
//! a reply plus [`Effect`]s for the runtime to execute. This keeps it free of
//! GTK, threads, and I/O so transitions can be unit-tested.

use serde::{Deserialize, Serialize};

use crate::cleanup::{CleanupStatus, Processed};

pub type SessionId = u64;

/// What happens to a ready result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    /// Put the result on the clipboard; the user pastes.
    Copy,
    /// Put the result into the focused application.
    Insert,
    /// Pass the result to the user's `note.hook` on stdin.
    Note,
}

/// Commands accepted from the CLI over IPC.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Command {
    Toggle,
    Start,
    Stop,
    Cancel,
    Status,
    Copy,
    Insert,
    /// Like `Copy`, but the raw transcript instead of the cleaned text.
    CopyRaw,
    /// Like `Insert`, but the raw transcript instead of the cleaned text.
    InsertRaw,
    /// Like `Toggle`, but the result goes to the note hook; with a ready
    /// result, pass that to the hook.
    Note,
}

/// Events produced by background work owned by the runtime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Internal {
    ProcessingFinished {
        session: SessionId,
        result: Result<Processed, String>,
    },
    RecordingLimitReached {
        session: SessionId,
    },
    /// The microphone failed while recording.
    CaptureFailed {
        session: SessionId,
        message: String,
    },
    DeliveryFinished {
        session: SessionId,
        result: Result<(), Failure>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum State {
    Idle,
    Recording {
        session: SessionId,
        /// Started automatically once the result is ready.
        on_ready: Option<Action>,
    },
    Processing {
        session: SessionId,
        on_ready: Option<Action>,
    },
    Ready {
        session: SessionId,
        result: Processed,
        /// Why the last copy/insert/note attempt failed, if it did.
        notice: Option<Notice>,
    },
    /// Copying, inserting, or running the note hook.
    Inserting {
        session: SessionId,
        /// Kept so a failed insertion returns to `Ready` with everything.
        result: Processed,
        action: Action,
        raw: bool,
    },
    Error {
        session: Option<SessionId>,
        message: String,
    },
}

/// Why a copy/insert/note failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    /// Written by us; safe for `status`, replies, and logs.
    pub message: String,
    /// Written by the note hook (its last stderr line). May contain anything,
    /// including the transcript, so it is only shown in the overlay.
    pub detail: Option<String>,
    pub kind: FailureKind,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum FailureKind {
    #[default]
    Failed,
    /// Nothing to insert into: no text field was focused. The text was
    /// copied instead; `toggle`/`insert` try again once one is.
    NoTextField,
}

impl From<String> for Failure {
    fn from(message: String) -> Self {
        Self {
            message,
            detail: None,
            kind: FailureKind::Failed,
        }
    }
}

impl From<&str> for Failure {
    fn from(message: &str) -> Self {
        message.to_owned().into()
    }
}

/// A failed delivery of a ready result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notice {
    pub action: Action,
    /// The raw transcript was delivered (`--raw`).
    pub raw: bool,
    pub failure: Failure,
}

/// Side effects the runtime must perform after a transition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    /// Open the microphone and start recording for a session.
    StartCapture { session: SessionId },
    /// Stop recording and throw the audio away.
    DiscardCapture { session: SessionId },
    /// Arm the maximum-duration timer for a recording session.
    ScheduleRecordingLimit { session: SessionId },
    /// Stop recording and process the captured audio of a session.
    StartProcessing { session: SessionId },
    /// Stop processing a session (e.g. abort native inference). Its result,
    /// if one still arrives, is ignored.
    AbortProcessing { session: SessionId },
    /// Copy, insert, or note a ready result. Must be followed by exactly
    /// one [`Internal::DeliveryFinished`]; it is never retried automatically.
    Deliver {
        session: SessionId,
        action: Action,
        text: String,
    },
    /// Stop the note hook of a session (kill it). Its result is ignored.
    AbortDelivery { session: SessionId },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StateKind {
    Idle,
    Recording,
    Processing,
    Ready,
    Inserting,
    Error,
}

/// Non-sensitive state summary (never contains transcript text).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Status {
    pub state: StateKind,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub session: Option<SessionId>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub result_chars: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub error: Option<String>,
    /// What LLM cleanup did to the ready result.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub cleanup: Option<CleanupStatus>,
    /// Why cleanup failed and the raw transcript is used instead.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub cleanup_error: Option<String>,
    /// The session's result goes (or went) to the note hook.
    #[serde(skip_serializing_if = "std::ops::Not::not", default)]
    pub note: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reply {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub message: Option<String>,
    pub status: Status,
}

#[derive(Debug, Default)]
pub struct Outcome {
    pub effects: Vec<Effect>,
    pub changed: bool,
    /// Completes a command for which `handle_command` returned no reply.
    pub deferred_reply: Option<Reply>,
}

#[derive(Debug)]
pub struct Controller {
    state: State,
    last_session: SessionId,
    /// Copy/insert started automatically when a result becomes ready.
    on_ready: Option<Action>,
    /// Whether a note hook is configured.
    notes: bool,
}

impl Default for Controller {
    fn default() -> Self {
        Self::new()
    }
}

impl Controller {
    pub fn new() -> Self {
        Self::with_on_ready(None)
    }

    /// A controller that starts `on_ready` itself whenever processing
    /// produces a result (no deferred reply; nobody is waiting for one).
    pub fn with_on_ready(on_ready: Option<Action>) -> Self {
        Self {
            state: State::Idle,
            last_session: 0,
            on_ready,
            notes: false,
        }
    }

    /// Accept `note` (a note hook is configured).
    pub fn with_notes(mut self, notes: bool) -> Self {
        self.notes = notes;
        self
    }

    pub fn state(&self) -> &State {
        &self.state
    }

    pub fn status(&self) -> Status {
        let (state, session, result, error) = match &self.state {
            State::Idle => (StateKind::Idle, None, None, None),
            State::Recording { session, .. } => (StateKind::Recording, Some(*session), None, None),
            State::Processing { session, .. } => {
                (StateKind::Processing, Some(*session), None, None)
            }
            State::Ready {
                session,
                result,
                notice,
            } => (
                StateKind::Ready,
                Some(*session),
                Some(result),
                notice.as_ref().map(|n| n.failure.message.clone()),
            ),
            State::Inserting {
                session, result, ..
            } => (StateKind::Inserting, Some(*session), Some(result), None),
            State::Error { session, message } => {
                (StateKind::Error, *session, None, Some(message.clone()))
            }
        };
        // Length of the text being (or about to be) copied/inserted.
        let result_chars = match &self.state {
            State::Inserting {
                result, raw: true, ..
            } => Some(result.raw_text().chars().count()),
            _ => result.map(|r| r.text.chars().count()),
        };
        Status {
            state,
            session,
            result_chars,
            error,
            cleanup: result.map(|r| r.cleanup),
            cleanup_error: result.and_then(|r| r.cleanup_error.clone()),
            note: self.is_note(),
        }
    }

    /// The current session is a note (recorded with or delivered to the hook).
    fn is_note(&self) -> bool {
        match &self.state {
            State::Recording { on_ready, .. } | State::Processing { on_ready, .. } => {
                *on_ready == Some(Action::Note)
            }
            State::Inserting { action, .. } => *action == Action::Note,
            _ => false,
        }
    }

    /// Returns `None` as the reply when completion is deferred until an
    /// internal event (see [`Outcome::deferred_reply`]).
    pub fn handle_command(&mut self, cmd: Command) -> (Option<Reply>, Outcome) {
        let mut out = Outcome::default();
        let result: Result<Option<&'static str>, &'static str> = match (cmd, &self.state) {
            (Command::Status, _) => Ok(None),

            (Command::Note, _) if !self.notes => {
                Err("no note hook configured; set `note.hook` in the config")
            }

            (Command::Toggle | Command::Start, State::Idle | State::Error { .. }) => {
                self.start_recording(self.on_ready, &mut out);
                Ok(None)
            }
            (Command::Note, State::Idle | State::Error { .. }) => {
                self.start_recording(Some(Action::Note), &mut out);
                Ok(None)
            }
            (Command::Toggle | Command::Stop, State::Recording { .. }) => {
                self.stop_recording(&mut out);
                Ok(None)
            }
            // Whatever started it, this recording becomes a note.
            (Command::Note, State::Recording { session, .. }) => {
                let session = *session;
                self.set(
                    State::Recording {
                        session,
                        on_ready: Some(Action::Note),
                    },
                    &mut out,
                );
                self.stop_recording(&mut out);
                Ok(None)
            }
            (Command::Start, State::Recording { .. }) => Ok(Some("already recording")),
            (Command::Stop, _) => Ok(Some("not recording")),

            // Copy/insert finish quickly and must not be cut off mid-typing;
            // a note hook may hang on the network, so it can be cancelled.
            (
                Command::Cancel,
                State::Inserting {
                    session,
                    action: Action::Note,
                    ..
                },
            ) => {
                out.effects
                    .push(Effect::AbortDelivery { session: *session });
                self.set(State::Idle, &mut out);
                Ok(Some("cancelled"))
            }
            (_, State::Inserting { .. }) => Err("insertion in progress"),
            (Command::Toggle | Command::Start | Command::Note, State::Processing { .. }) => {
                Err("still processing; use `cancel` to discard")
            }
            // After a failed insertion (typically: no text field was
            // focused), the dictation key tries again.
            (
                Command::Toggle,
                State::Ready {
                    notice:
                        Some(Notice {
                            action: Action::Insert,
                            raw,
                            ..
                        }),
                    ..
                },
            ) => {
                let raw = *raw;
                self.deliver(Action::Insert, raw, &mut out);
                return (None, out);
            }
            (Command::Toggle | Command::Start, State::Ready { .. }) => {
                Err("a result is ready; use `copy`/`insert`, or `cancel` to discard it")
            }

            (Command::Cancel, State::Idle) => Ok(Some("nothing to cancel")),
            (Command::Cancel, state) => {
                match state {
                    State::Recording { session, .. } => out
                        .effects
                        .push(Effect::DiscardCapture { session: *session }),
                    State::Processing { session, .. } => out
                        .effects
                        .push(Effect::AbortProcessing { session: *session }),
                    _ => {}
                }
                self.set(State::Idle, &mut out);
                Ok(Some("cancelled"))
            }

            (
                Command::Copy
                | Command::Insert
                | Command::CopyRaw
                | Command::InsertRaw
                | Command::Note,
                State::Ready { .. },
            ) => {
                let action = match cmd {
                    Command::Copy | Command::CopyRaw => Action::Copy,
                    Command::Note => Action::Note,
                    _ => Action::Insert,
                };
                let raw = matches!(cmd, Command::CopyRaw | Command::InsertRaw);
                self.deliver(action, raw, &mut out);
                if action != Action::Note {
                    return (None, out);
                }
                // A hook may run long; replying when it is done would block
                // the control socket (and `cancel`) meanwhile. Its outcome
                // shows in the overlay and `status`.
                Ok(Some("saving note"))
            }
            (Command::Copy | Command::Insert | Command::CopyRaw | Command::InsertRaw, _) => {
                Err("no ready result")
            }
        };

        let (ok, message) = match result {
            Ok(msg) => (true, msg),
            Err(msg) => (false, Some(msg)),
        };
        (Some(self.reply(ok, message.map(str::to_owned))), out)
    }

    pub fn handle_internal(&mut self, event: Internal) -> Outcome {
        let mut out = Outcome::default();
        match event {
            Internal::ProcessingFinished { session, result } => {
                let State::Processing {
                    session: current,
                    on_ready,
                } = self.state
                else {
                    tracing::debug!(session, "ignoring stale processing result");
                    return out;
                };
                if current != session {
                    tracing::debug!(session, "ignoring stale processing result");
                    return out;
                }
                let next = match (result, on_ready) {
                    (Ok(result), Some(action)) => {
                        out.effects.push(Effect::Deliver {
                            session,
                            action,
                            text: result.text.clone(),
                        });
                        State::Inserting {
                            session,
                            result,
                            action,
                            raw: false,
                        }
                    }
                    (Ok(result), None) => State::Ready {
                        session,
                        result,
                        notice: None,
                    },
                    (Err(message), _) => State::Error {
                        session: Some(session),
                        message,
                    },
                };
                self.set(next, &mut out);
            }
            Internal::RecordingLimitReached { session } => {
                if !self.is_recording(session) {
                    return out;
                }
                tracing::info!(session, "maximum recording duration reached");
                self.stop_recording(&mut out);
            }
            Internal::CaptureFailed { session, message } => {
                if !self.is_recording(session) {
                    return out;
                }
                out.effects.push(Effect::DiscardCapture { session });
                self.set(
                    State::Error {
                        session: Some(session),
                        message,
                    },
                    &mut out,
                );
            }
            Internal::DeliveryFinished { session, result } => {
                let State::Inserting {
                    session: current,
                    result: processed,
                    action,
                    raw,
                } = &self.state
                else {
                    return out;
                };
                if *current != session {
                    return out;
                }
                let (action, raw, processed) = (*action, *raw, processed.clone());
                let (ok, message) = match result {
                    Ok(()) => {
                        self.set(State::Idle, &mut out);
                        let msg = match (action, raw) {
                            (Action::Copy, false) => "copied to clipboard",
                            (Action::Copy, true) => "copied raw transcript to clipboard",
                            (Action::Insert, false) => "inserted",
                            (Action::Insert, true) => "inserted raw transcript",
                            (Action::Note, _) => "note saved",
                        };
                        (true, msg.to_owned())
                    }
                    Err(failure) => {
                        let message = failure.message.clone();
                        // Keep the result so the user can retry or copy manually.
                        self.set(
                            State::Ready {
                                session,
                                result: processed,
                                notice: Some(Notice {
                                    action,
                                    raw,
                                    failure,
                                }),
                            },
                            &mut out,
                        );
                        (false, message)
                    }
                };
                out.deferred_reply = Some(self.reply(ok, Some(message)));
            }
        }
        out
    }

    fn reply(&self, ok: bool, message: Option<String>) -> Reply {
        Reply {
            ok,
            message,
            status: self.status(),
        }
    }

    fn is_recording(&self, session: SessionId) -> bool {
        matches!(self.state, State::Recording { session: s, .. } if s == session)
    }

    /// Start delivering the ready result. The caller replies (deferred for
    /// copy/insert).
    fn deliver(&mut self, action: Action, raw: bool, out: &mut Outcome) {
        let State::Ready {
            session, result, ..
        } = &self.state
        else {
            return;
        };
        let text = if raw {
            result.raw_text().to_owned()
        } else {
            result.text.clone()
        };
        let (session, result) = (*session, result.clone());
        out.effects.push(Effect::Deliver {
            session,
            action,
            text,
        });
        self.set(
            State::Inserting {
                session,
                result,
                action,
                raw,
            },
            out,
        );
    }

    fn start_recording(&mut self, on_ready: Option<Action>, out: &mut Outcome) {
        self.last_session += 1;
        let session = self.last_session;
        self.set(State::Recording { session, on_ready }, out);
        out.effects.push(Effect::StartCapture { session });
        out.effects.push(Effect::ScheduleRecordingLimit { session });
    }

    fn stop_recording(&mut self, out: &mut Outcome) {
        let State::Recording { session, on_ready } = self.state else {
            return;
        };
        self.set(State::Processing { session, on_ready }, out);
        out.effects.push(Effect::StartProcessing { session });
    }

    fn set(&mut self, next: State, out: &mut Outcome) {
        if self.state != next {
            self.state = next;
            out.changed = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(text: &str) -> Processed {
        Processed::uncleaned(text.into(), CleanupStatus::Disabled)
    }

    fn kind(c: &Controller) -> StateKind {
        c.status().state
    }

    /// Run a command that must reply immediately.
    fn run(c: &mut Controller, cmd: Command) -> (Reply, Outcome) {
        let (reply, out) = c.handle_command(cmd);
        (reply.expect("immediate reply"), out)
    }

    fn ready(c: &mut Controller, text: &str) -> SessionId {
        run(c, Command::Start);
        run(c, Command::Stop);
        let session = c.status().session.unwrap();
        c.handle_internal(Internal::ProcessingFinished {
            session,
            result: Ok(plain(text)),
        });
        assert_eq!(kind(c), StateKind::Ready);
        session
    }

    #[test]
    fn toggle_cycle_reaches_ready() {
        let mut c = Controller::new();
        let (r, o) = run(&mut c, Command::Toggle);
        assert!(r.ok);
        assert_eq!(kind(&c), StateKind::Recording);
        assert_eq!(
            o.effects,
            vec![
                Effect::StartCapture { session: 1 },
                Effect::ScheduleRecordingLimit { session: 1 }
            ]
        );

        let (r, o) = run(&mut c, Command::Toggle);
        assert!(r.ok);
        assert_eq!(kind(&c), StateKind::Processing);
        assert_eq!(o.effects, vec![Effect::StartProcessing { session: 1 }]);

        let o = c.handle_internal(Internal::ProcessingFinished {
            session: 1,
            result: Ok(plain("hello")),
        });
        assert!(o.changed);
        assert_eq!(c.status().result_chars, Some(5));
        assert_eq!(kind(&c), StateKind::Ready);
    }

    #[test]
    fn toggle_does_not_discard_processing_or_ready_work() {
        let mut c = Controller::new();
        run(&mut c, Command::Start);
        run(&mut c, Command::Stop);
        let (r, o) = run(&mut c, Command::Toggle);
        assert!(!r.ok);
        assert!(!o.changed);
        assert_eq!(kind(&c), StateKind::Processing);

        c.handle_internal(Internal::ProcessingFinished {
            session: 1,
            result: Ok(plain("x")),
        });
        let (r, _) = run(&mut c, Command::Toggle);
        assert!(!r.ok);
        assert_eq!(kind(&c), StateKind::Ready);
    }

    #[test]
    fn repeated_start_and_stop_are_safe() {
        let mut c = Controller::new();
        let (r, _) = run(&mut c, Command::Stop);
        assert!(r.ok);
        assert_eq!(kind(&c), StateKind::Idle);

        run(&mut c, Command::Start);
        let (r, o) = run(&mut c, Command::Start);
        assert!(r.ok);
        assert!(o.effects.is_empty());
        assert_eq!(c.status().session, Some(1));

        run(&mut c, Command::Stop);
        let (r, o) = run(&mut c, Command::Stop);
        assert!(r.ok);
        assert!(o.effects.is_empty());
    }

    #[test]
    fn stale_results_are_ignored() {
        let mut c = Controller::new();
        run(&mut c, Command::Start);
        run(&mut c, Command::Stop);
        run(&mut c, Command::Cancel);
        run(&mut c, Command::Start); // session 2
        let o = c.handle_internal(Internal::ProcessingFinished {
            session: 1,
            result: Ok(plain("old")),
        });
        assert!(!o.changed);
        assert_eq!(
            c.state(),
            &State::Recording {
                session: 2,
                on_ready: None
            }
        );

        // Old recording-limit timers must not stop the new session.
        let o = c.handle_internal(Internal::RecordingLimitReached { session: 1 });
        assert!(!o.changed);
        assert_eq!(kind(&c), StateKind::Recording);
    }

    #[test]
    fn cancel_while_recording_discards_capture() {
        let mut c = Controller::new();
        run(&mut c, Command::Start);
        let (r, o) = run(&mut c, Command::Cancel);
        assert!(r.ok);
        assert_eq!(o.effects, vec![Effect::DiscardCapture { session: 1 }]);

        // Cancelling while processing aborts the transcription instead.
        run(&mut c, Command::Start);
        run(&mut c, Command::Stop);
        let (_, o) = run(&mut c, Command::Cancel);
        assert_eq!(o.effects, vec![Effect::AbortProcessing { session: 2 }]);
        assert_eq!(kind(&c), StateKind::Idle);

        // Cancelling a ready result has nothing to stop.
        ready(&mut c, "x");
        let (_, o) = run(&mut c, Command::Cancel);
        assert!(o.effects.is_empty());
    }

    #[test]
    fn capture_failure_is_an_error_only_for_current_recording() {
        let mut c = Controller::new();
        run(&mut c, Command::Start);
        let o = c.handle_internal(Internal::CaptureFailed {
            session: 1,
            message: "mic unplugged".into(),
        });
        assert_eq!(o.effects, vec![Effect::DiscardCapture { session: 1 }]);
        assert_eq!(c.status().error.as_deref(), Some("mic unplugged"));

        run(&mut c, Command::Start); // session 2
        run(&mut c, Command::Stop);
        let o = c.handle_internal(Internal::CaptureFailed {
            session: 2,
            message: "late".into(),
        });
        assert!(!o.changed, "processing reports capture errors itself");
        assert_eq!(kind(&c), StateKind::Processing);
    }

    #[test]
    fn recording_limit_stops_current_session() {
        let mut c = Controller::new();
        run(&mut c, Command::Start);
        let o = c.handle_internal(Internal::RecordingLimitReached { session: 1 });
        assert_eq!(o.effects, vec![Effect::StartProcessing { session: 1 }]);
        assert_eq!(kind(&c), StateKind::Processing);
    }

    #[test]
    fn errors_are_recoverable() {
        let mut c = Controller::new();
        run(&mut c, Command::Start);
        run(&mut c, Command::Stop);
        c.handle_internal(Internal::ProcessingFinished {
            session: 1,
            result: Err("boom".into()),
        });
        assert_eq!(c.status().error.as_deref(), Some("boom"));
        let (r, _) = run(&mut c, Command::Toggle);
        assert!(r.ok);
        assert_eq!(
            c.state(),
            &State::Recording {
                session: 2,
                on_ready: None
            }
        );
    }

    #[test]
    fn copy_requires_ready_result() {
        let mut c = Controller::new();
        let (r, o) = run(&mut c, Command::Copy);
        assert!(!r.ok);
        assert!(o.effects.is_empty());
    }

    #[test]
    fn insert_is_deferred_and_finishes_idle() {
        let mut c = Controller::new();
        let s = ready(&mut c, "hi");
        let (reply, o) = c.handle_command(Command::Insert);
        assert!(reply.is_none());
        assert_eq!(
            o.effects,
            vec![Effect::Deliver {
                session: s,
                action: Action::Insert,
                text: "hi".into()
            }]
        );
        assert_eq!(kind(&c), StateKind::Inserting);

        let o = c.handle_internal(Internal::DeliveryFinished {
            session: s,
            result: Ok(()),
        });
        let r = o.deferred_reply.unwrap();
        assert!(r.ok);
        assert_eq!(r.message.as_deref(), Some("inserted"));
        assert_eq!(kind(&c), StateKind::Idle);
    }

    #[test]
    fn duplicate_commands_during_insertion_are_rejected() {
        let mut c = Controller::new();
        ready(&mut c, "hi");
        c.handle_command(Command::Insert);
        for cmd in [
            Command::Insert,
            Command::Copy,
            Command::Toggle,
            Command::Start,
            Command::Cancel,
        ] {
            let (r, o) = run(&mut c, cmd);
            assert!(!r.ok, "{cmd:?} should be rejected");
            assert!(o.effects.is_empty());
        }
        let (r, _) = run(&mut c, Command::Stop);
        assert!(r.ok);
        assert_eq!(kind(&c), StateKind::Inserting);
    }

    #[test]
    fn failed_insertion_keeps_result_for_retry() {
        let mut c = Controller::new();
        let s = ready(&mut c, "keep me");
        c.handle_command(Command::Insert);
        let o = c.handle_internal(Internal::DeliveryFinished {
            session: s,
            result: Err("no virtual keyboard".into()),
        });
        let r = o.deferred_reply.unwrap();
        assert!(!r.ok);
        assert_eq!(r.status.state, StateKind::Ready);
        assert_eq!(r.status.error.as_deref(), Some("no virtual keyboard"));
        assert_eq!(
            c.state(),
            &State::Ready {
                session: s,
                result: plain("keep me"),
                notice: Some(Notice {
                    action: Action::Insert,
                    raw: false,
                    failure: "no virtual keyboard".into()
                })
            }
        );

        // Retry is an explicit new command; copy works from the same result.
        let (reply, o) = c.handle_command(Command::Copy);
        assert!(reply.is_none());
        assert!(matches!(
            o.effects[..],
            [Effect::Deliver {
                action: Action::Copy,
                ..
            }]
        ));
    }

    #[test]
    fn stale_injection_results_are_ignored() {
        let mut c = Controller::new();
        let s = ready(&mut c, "x");
        c.handle_command(Command::Copy);
        let o = c.handle_internal(Internal::DeliveryFinished {
            session: s + 1,
            result: Ok(()),
        });
        assert!(o.deferred_reply.is_none());
        assert_eq!(kind(&c), StateKind::Inserting);
    }

    #[test]
    fn raw_commands_use_the_raw_transcript() {
        let mut c = Controller::new();
        run(&mut c, Command::Start);
        run(&mut c, Command::Stop);
        c.handle_internal(Internal::ProcessingFinished {
            session: 1,
            result: Ok(Processed {
                text: "Clean.".into(),
                raw: Some("um clean".into()),
                cleanup: CleanupStatus::Cleaned,
                cleanup_error: None,
            }),
        });
        assert_eq!(c.status().cleanup, Some(CleanupStatus::Cleaned));

        let (_, o) = c.handle_command(Command::InsertRaw);
        assert!(
            matches!(&o.effects[..], [Effect::Deliver { text, action: Action::Insert, .. }] if text == "um clean")
        );
        assert_eq!(c.status().result_chars, Some("um clean".len()));
        let o = c.handle_internal(Internal::DeliveryFinished {
            session: 1,
            result: Err("nope".into()),
        });
        assert!(!o.deferred_reply.unwrap().ok);

        // Back in Ready with both texts; the normal command uses the clean one.
        let (_, o) = c.handle_command(Command::Copy);
        assert!(matches!(&o.effects[..], [Effect::Deliver { text, .. }] if text == "Clean."));
        let o = c.handle_internal(Internal::DeliveryFinished {
            session: 1,
            result: Ok(()),
        });
        assert_eq!(
            o.deferred_reply.unwrap().message.as_deref(),
            Some("copied to clipboard")
        );

        // Without a separate raw text, raw commands use the result.
        ready(&mut c, "same");
        let (_, o) = c.handle_command(Command::CopyRaw);
        assert!(matches!(&o.effects[..], [Effect::Deliver { text, .. }] if text == "same"));
    }

    #[test]
    fn toggle_retries_insert_without_text_field() {
        let no_field = || Failure {
            message: "no text field is focused; copied to the clipboard".into(),
            detail: None,
            kind: FailureKind::NoTextField,
        };
        let mut c = Controller::with_on_ready(Some(Action::Insert));
        run(&mut c, Command::Toggle);
        run(&mut c, Command::Toggle);
        c.handle_internal(Internal::ProcessingFinished {
            session: 1,
            result: Ok(plain("hello")),
        });
        c.handle_internal(Internal::DeliveryFinished {
            session: 1,
            result: Err(no_field()),
        });
        assert_eq!(kind(&c), StateKind::Ready);

        // Still no field: back to Ready, the text is kept.
        let (reply, o) = c.handle_command(Command::Toggle);
        assert!(reply.is_none(), "deferred like insert");
        assert_eq!(
            o.effects,
            vec![Effect::Deliver {
                session: 1,
                action: Action::Insert,
                text: "hello".into()
            }]
        );
        let o = c.handle_internal(Internal::DeliveryFinished {
            session: 1,
            result: Err(no_field()),
        });
        assert!(!o.deferred_reply.unwrap().ok);
        assert_eq!(kind(&c), StateKind::Ready);

        // A field is focused now.
        c.handle_command(Command::Toggle);
        let o = c.handle_internal(Internal::DeliveryFinished {
            session: 1,
            result: Ok(()),
        });
        assert!(o.deferred_reply.unwrap().ok);
        assert_eq!(kind(&c), StateKind::Idle);
        // The next toggle records again.
        let (_, o) = run(&mut c, Command::Toggle);
        assert!(o.effects.contains(&Effect::StartCapture { session: 2 }));
    }

    #[test]
    fn toggle_retry_keeps_raw_and_cancel_discards() {
        let mut c = Controller::new();
        let s = ready(&mut c, "clean");
        c.handle_command(Command::InsertRaw);
        c.handle_internal(Internal::DeliveryFinished {
            session: s,
            result: Err("no text field".into()),
        });
        let (_, o) = c.handle_command(Command::Toggle);
        assert!(matches!(
            &o.effects[..],
            [Effect::Deliver {
                action: Action::Insert,
                ..
            }]
        ));
        assert!(matches!(
            c.state(),
            State::Inserting {
                action: Action::Insert,
                raw: true,
                ..
            }
        ));
        c.handle_internal(Internal::DeliveryFinished {
            session: s,
            result: Err("no text field".into()),
        });
        let (reply, _) = run(&mut c, Command::Cancel);
        assert!(reply.ok);
        assert_eq!(kind(&c), StateKind::Idle);

        // Only a failed insert is retried by toggle; a plain ready result
        // or a failed copy still needs an explicit command.
        let s = ready(&mut c, "x");
        let (reply, o) = run(&mut c, Command::Toggle);
        assert!(!reply.ok);
        assert!(o.effects.is_empty());
        c.handle_command(Command::Copy);
        c.handle_internal(Internal::DeliveryFinished {
            session: s,
            result: Err("no clipboard".into()),
        });
        let (reply, _) = run(&mut c, Command::Toggle);
        assert!(!reply.ok);
    }

    #[test]
    fn on_ready_inserts_automatically() {
        let mut c = Controller::with_on_ready(Some(Action::Insert));
        run(&mut c, Command::Toggle);
        run(&mut c, Command::Toggle);
        let o = c.handle_internal(Internal::ProcessingFinished {
            session: 1,
            result: Ok(plain("hello")),
        });
        assert_eq!(
            o.effects,
            vec![Effect::Deliver {
                session: 1,
                action: Action::Insert,
                text: "hello".into()
            }]
        );
        assert_eq!(kind(&c), StateKind::Inserting);

        let o = c.handle_internal(Internal::DeliveryFinished {
            session: 1,
            result: Ok(()),
        });
        assert_eq!(kind(&c), StateKind::Idle);
        assert!(o.deferred_reply.unwrap().ok);

        // A failed automatic insertion leaves the result ready for retry.
        run(&mut c, Command::Toggle);
        run(&mut c, Command::Toggle);
        c.handle_internal(Internal::ProcessingFinished {
            session: 2,
            result: Ok(plain("again")),
        });
        c.handle_internal(Internal::DeliveryFinished {
            session: 2,
            result: Err("no virtual keyboard".into()),
        });
        assert_eq!(kind(&c), StateKind::Ready);
        let (reply, o) = c.handle_command(Command::Insert);
        assert!(reply.is_none());
        assert_eq!(o.effects.len(), 1);

        // Errors are not inserted.
        let mut c = Controller::with_on_ready(Some(Action::Copy));
        run(&mut c, Command::Toggle);
        run(&mut c, Command::Toggle);
        let o = c.handle_internal(Internal::ProcessingFinished {
            session: 1,
            result: Err("no speech".into()),
        });
        assert!(o.effects.is_empty());
        assert_eq!(kind(&c), StateKind::Error);
    }

    #[test]
    fn failed_cleanup_is_reported_in_status() {
        let mut c = Controller::new();
        run(&mut c, Command::Start);
        run(&mut c, Command::Stop);
        c.handle_internal(Internal::ProcessingFinished {
            session: 1,
            result: Ok(Processed {
                text: "raw words".into(),
                raw: None,
                cleanup: CleanupStatus::Failed,
                cleanup_error: Some("cannot reach server".into()),
            }),
        });
        let status = c.status();
        assert_eq!(status.state, StateKind::Ready);
        assert_eq!(status.cleanup, Some(CleanupStatus::Failed));
        assert_eq!(status.cleanup_error.as_deref(), Some("cannot reach server"));
        assert_eq!(status.error, None);
    }

    #[test]
    fn note_requires_a_hook() {
        let mut c = Controller::new();
        let (r, o) = run(&mut c, Command::Note);
        assert!(!r.ok);
        assert!(o.effects.is_empty());
        assert_eq!(kind(&c), StateKind::Idle);
    }

    #[test]
    fn note_records_and_delivers_to_the_hook() {
        // The note goes to the hook even if dictation would insert.
        let mut c = Controller::with_on_ready(Some(Action::Insert)).with_notes(true);
        let (r, o) = run(&mut c, Command::Note);
        assert!(r.ok);
        assert!(r.status.note);
        assert_eq!(
            o.effects,
            vec![
                Effect::StartCapture { session: 1 },
                Effect::ScheduleRecordingLimit { session: 1 }
            ]
        );
        // `toggle`/`stop` end a note recording without changing where it goes.
        let (_, o) = run(&mut c, Command::Stop);
        assert_eq!(o.effects, vec![Effect::StartProcessing { session: 1 }]);
        let (r, _) = run(&mut c, Command::Note);
        assert!(!r.ok, "still processing");
        let o = c.handle_internal(Internal::ProcessingFinished {
            session: 1,
            result: Ok(plain("buy milk")),
        });
        assert_eq!(
            o.effects,
            vec![Effect::Deliver {
                session: 1,
                action: Action::Note,
                text: "buy milk".into()
            }]
        );
        assert!(c.status().note);
        let o = c.handle_internal(Internal::DeliveryFinished {
            session: 1,
            result: Ok(()),
        });
        assert_eq!(
            o.deferred_reply.unwrap().message.as_deref(),
            Some("note saved")
        );
        assert_eq!(kind(&c), StateKind::Idle);

        // Normal dictation afterwards still inserts.
        run(&mut c, Command::Toggle);
        assert!(!c.status().note);
        run(&mut c, Command::Toggle);
        let o = c.handle_internal(Internal::ProcessingFinished {
            session: 2,
            result: Ok(plain("x")),
        });
        assert!(matches!(
            o.effects[..],
            [Effect::Deliver {
                action: Action::Insert,
                ..
            }]
        ));
    }

    #[test]
    fn note_turns_a_dictation_into_a_note() {
        let mut c = Controller::with_on_ready(Some(Action::Insert)).with_notes(true);
        run(&mut c, Command::Toggle);
        let (r, o) = run(&mut c, Command::Note);
        assert!(r.ok);
        assert_eq!(o.effects, vec![Effect::StartProcessing { session: 1 }]);
        let o = c.handle_internal(Internal::ProcessingFinished {
            session: 1,
            result: Ok(plain("idea")),
        });
        assert!(matches!(
            o.effects[..],
            [Effect::Deliver {
                action: Action::Note,
                ..
            }]
        ));
    }

    #[test]
    fn failed_note_keeps_result_and_note_retries() {
        let mut c = Controller::new().with_notes(true);
        let s = ready(&mut c, "keep me");
        // Replies at once: a slow hook must not block the control socket.
        let (r, o) = run(&mut c, Command::Note);
        assert!(r.ok);
        assert!(matches!(
            o.effects[..],
            [Effect::Deliver {
                action: Action::Note,
                ..
            }]
        ));
        let o = c.handle_internal(Internal::DeliveryFinished {
            session: s,
            result: Err(Failure {
                message: "note hook exited with status 1".into(),
                detail: Some("cannot append secret words".into()),
                kind: FailureKind::Failed,
            }),
        });
        let reply = o.deferred_reply.unwrap();
        assert!(!reply.ok);
        // The hook's stderr may contain the note; it stays out of status.
        assert!(!serde_json::to_string(&reply).unwrap().contains("secret"));
        assert_eq!(
            c.status().error.as_deref(),
            Some("note hook exited with status 1")
        );
        assert!(matches!(
            c.state(),
            State::Ready {
                notice: Some(Notice {
                    action: Action::Note,
                    ..
                }),
                ..
            }
        ));
        let (_, o) = run(&mut c, Command::Note);
        assert!(matches!(
            &o.effects[..],
            [Effect::Deliver { action: Action::Note, text, .. }] if text == "keep me"
        ));
    }

    #[test]
    fn running_note_hook_can_be_cancelled_but_not_typing() {
        let mut c = Controller::new().with_notes(true);
        let s = ready(&mut c, "x");
        run(&mut c, Command::Note);
        let (r, o) = run(&mut c, Command::Cancel);
        assert!(r.ok);
        assert_eq!(o.effects, vec![Effect::AbortDelivery { session: s }]);
        assert_eq!(kind(&c), StateKind::Idle);
        // The killed hook's result is ignored.
        let o = c.handle_internal(Internal::DeliveryFinished {
            session: s,
            result: Err("killed".into()),
        });
        assert!(!o.changed);
        assert_eq!(kind(&c), StateKind::Idle);

        ready(&mut c, "y");
        c.handle_command(Command::Insert);
        let (r, _) = run(&mut c, Command::Cancel);
        assert!(!r.ok);
    }

    #[test]
    fn status_never_contains_text() {
        let mut c = Controller::new();
        run(&mut c, Command::Start);
        run(&mut c, Command::Stop);
        c.handle_internal(Internal::ProcessingFinished {
            session: 1,
            result: Ok(plain("secret words")),
        });
        let json = serde_json::to_string(&c.status()).unwrap();
        assert!(!json.contains("secret"));
    }
}

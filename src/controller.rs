//! Session state machine.
//!
//! The [`Controller`] is pure: it consumes commands/internal events and returns
//! a reply plus [`Effect`]s for the runtime to execute. This keeps it free of
//! GTK, threads, and I/O so transitions can be unit-tested.

use serde::{Deserialize, Serialize};

use crate::cleanup::{CleanupStatus, Processed};
use crate::injection::Action;

pub type SessionId = u64;

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
    InjectionFinished {
        session: SessionId,
        result: Result<(), String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum State {
    Idle,
    Recording {
        session: SessionId,
    },
    Processing {
        session: SessionId,
    },
    Ready {
        session: SessionId,
        result: Processed,
        /// Why the last copy/insert attempt failed, if it did.
        notice: Option<String>,
    },
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
    /// Copy or insert a ready result. Must be followed by exactly one
    /// [`Internal::InjectionFinished`]; it is never retried automatically.
    Inject {
        session: SessionId,
        action: Action,
        text: String,
    },
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
        }
    }

    pub fn state(&self) -> &State {
        &self.state
    }

    pub fn status(&self) -> Status {
        let (state, session, result, error) = match &self.state {
            State::Idle => (StateKind::Idle, None, None, None),
            State::Recording { session } => (StateKind::Recording, Some(*session), None, None),
            State::Processing { session } => (StateKind::Processing, Some(*session), None, None),
            State::Ready {
                session,
                result,
                notice,
            } => (
                StateKind::Ready,
                Some(*session),
                Some(result),
                notice.clone(),
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
        }
    }

    /// Returns `None` as the reply when completion is deferred until an
    /// internal event (see [`Outcome::deferred_reply`]).
    pub fn handle_command(&mut self, cmd: Command) -> (Option<Reply>, Outcome) {
        let mut out = Outcome::default();
        let result: Result<Option<&'static str>, &'static str> = match (cmd, &self.state) {
            (Command::Status, _) => Ok(None),

            (Command::Toggle | Command::Start, State::Idle | State::Error { .. }) => {
                self.start_recording(&mut out);
                Ok(None)
            }
            (Command::Toggle | Command::Stop, State::Recording { .. }) => {
                self.stop_recording(&mut out);
                Ok(None)
            }
            (Command::Start, State::Recording { .. }) => Ok(Some("already recording")),
            (Command::Stop, _) => Ok(Some("not recording")),

            (_, State::Inserting { .. }) => Err("insertion in progress"),
            (Command::Toggle | Command::Start, State::Processing { .. }) => {
                Err("still processing; use `cancel` to discard")
            }
            (Command::Toggle | Command::Start, State::Ready { .. }) => {
                Err("a result is ready; use `copy`/`insert`, or `cancel` to discard it")
            }

            (Command::Cancel, State::Idle) => Ok(Some("nothing to cancel")),
            (Command::Cancel, state) => {
                match state {
                    State::Recording { session } => out
                        .effects
                        .push(Effect::DiscardCapture { session: *session }),
                    State::Processing { session } => out
                        .effects
                        .push(Effect::AbortProcessing { session: *session }),
                    _ => {}
                }
                self.set(State::Idle, &mut out);
                Ok(Some("cancelled"))
            }

            (
                Command::Copy | Command::Insert | Command::CopyRaw | Command::InsertRaw,
                State::Ready {
                    session, result, ..
                },
            ) => {
                let action = match cmd {
                    Command::Copy | Command::CopyRaw => Action::Copy,
                    _ => Action::Insert,
                };
                let raw = matches!(cmd, Command::CopyRaw | Command::InsertRaw);
                let text = if raw {
                    result.raw_text().to_owned()
                } else {
                    result.text.clone()
                };
                let (session, result) = (*session, result.clone());
                out.effects.push(Effect::Inject {
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
                    &mut out,
                );
                return (None, out);
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
                if self.state != (State::Processing { session }) {
                    tracing::debug!(session, "ignoring stale processing result");
                    return out;
                }
                let next = match (result, self.on_ready) {
                    (Ok(result), Some(action)) => {
                        out.effects.push(Effect::Inject {
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
                if self.state != (State::Recording { session }) {
                    return out;
                }
                tracing::info!(session, "maximum recording duration reached");
                self.stop_recording(&mut out);
            }
            Internal::CaptureFailed { session, message } => {
                if self.state != (State::Recording { session }) {
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
            Internal::InjectionFinished { session, result } => {
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
                        };
                        (true, msg.to_owned())
                    }
                    Err(message) => {
                        // Keep the result so the user can retry or copy manually.
                        self.set(
                            State::Ready {
                                session,
                                result: processed,
                                notice: Some(message.clone()),
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

    fn start_recording(&mut self, out: &mut Outcome) {
        self.last_session += 1;
        let session = self.last_session;
        self.set(State::Recording { session }, out);
        out.effects.push(Effect::StartCapture { session });
        out.effects.push(Effect::ScheduleRecordingLimit { session });
    }

    fn stop_recording(&mut self, out: &mut Outcome) {
        let State::Recording { session } = self.state else {
            return;
        };
        self.set(State::Processing { session }, out);
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
        assert_eq!(c.state(), &State::Recording { session: 2 });

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
        assert_eq!(c.state(), &State::Recording { session: 2 });
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
            vec![Effect::Inject {
                session: s,
                action: Action::Insert,
                text: "hi".into()
            }]
        );
        assert_eq!(kind(&c), StateKind::Inserting);

        let o = c.handle_internal(Internal::InjectionFinished {
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
        let o = c.handle_internal(Internal::InjectionFinished {
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
                notice: Some("no virtual keyboard".into())
            }
        );

        // Retry is an explicit new command; copy works from the same result.
        let (reply, o) = c.handle_command(Command::Copy);
        assert!(reply.is_none());
        assert!(matches!(
            o.effects[..],
            [Effect::Inject {
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
        let o = c.handle_internal(Internal::InjectionFinished {
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
            matches!(&o.effects[..], [Effect::Inject { text, action: Action::Insert, .. }] if text == "um clean")
        );
        assert_eq!(c.status().result_chars, Some("um clean".len()));
        let o = c.handle_internal(Internal::InjectionFinished {
            session: 1,
            result: Err("nope".into()),
        });
        assert!(!o.deferred_reply.unwrap().ok);

        // Back in Ready with both texts; the normal command uses the clean one.
        let (_, o) = c.handle_command(Command::Copy);
        assert!(matches!(&o.effects[..], [Effect::Inject { text, .. }] if text == "Clean."));
        let o = c.handle_internal(Internal::InjectionFinished {
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
        assert!(matches!(&o.effects[..], [Effect::Inject { text, .. }] if text == "same"));
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
            vec![Effect::Inject {
                session: 1,
                action: Action::Insert,
                text: "hello".into()
            }]
        );
        assert_eq!(kind(&c), StateKind::Inserting);

        let o = c.handle_internal(Internal::InjectionFinished {
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
        c.handle_internal(Internal::InjectionFinished {
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

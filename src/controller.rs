//! Session state machine.
//!
//! The [`Controller`] is pure: it consumes commands/internal events and returns
//! a reply plus [`Effect`]s for the runtime to execute. This keeps it free of
//! GTK, threads, and I/O so transitions can be unit-tested.

use serde::{Deserialize, Serialize};

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
}

/// Events produced by background work owned by the runtime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Internal {
    ProcessingFinished {
        session: SessionId,
        result: Result<String, String>,
    },
    RecordingLimitReached {
        session: SessionId,
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
        text: String,
        /// Why the last copy/insert attempt failed, if it did.
        notice: Option<String>,
    },
    Inserting {
        session: SessionId,
        text: String,
        action: Action,
    },
    Error {
        session: Option<SessionId>,
        message: String,
    },
}

/// Side effects the runtime must perform after a transition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    /// Arm the maximum-duration timer for a recording session.
    ScheduleRecordingLimit { session: SessionId },
    /// Begin processing the captured audio of a session.
    StartProcessing { session: SessionId },
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
}

impl Default for Controller {
    fn default() -> Self {
        Self::new()
    }
}

impl Controller {
    pub fn new() -> Self {
        Self {
            state: State::Idle,
            last_session: 0,
        }
    }

    pub fn state(&self) -> &State {
        &self.state
    }

    pub fn status(&self) -> Status {
        let (state, session, result_chars, error) = match &self.state {
            State::Idle => (StateKind::Idle, None, None, None),
            State::Recording { session } => (StateKind::Recording, Some(*session), None, None),
            State::Processing { session } => (StateKind::Processing, Some(*session), None, None),
            State::Ready {
                session,
                text,
                notice,
            } => (
                StateKind::Ready,
                Some(*session),
                Some(text.chars().count()),
                notice.clone(),
            ),
            State::Inserting { session, text, .. } => (
                StateKind::Inserting,
                Some(*session),
                Some(text.chars().count()),
                None,
            ),
            State::Error { session, message } => {
                (StateKind::Error, *session, None, Some(message.clone()))
            }
        };
        Status {
            state,
            session,
            result_chars,
            error,
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
            (Command::Cancel, _) => {
                self.set(State::Idle, &mut out);
                Ok(Some("cancelled"))
            }

            (Command::Copy | Command::Insert, State::Ready { session, text, .. }) => {
                let action = if cmd == Command::Copy {
                    Action::Copy
                } else {
                    Action::Insert
                };
                let (session, text) = (*session, text.clone());
                out.effects.push(Effect::Inject {
                    session,
                    action,
                    text: text.clone(),
                });
                self.set(
                    State::Inserting {
                        session,
                        text,
                        action,
                    },
                    &mut out,
                );
                return (None, out);
            }
            (Command::Copy | Command::Insert, _) => Err("no ready result"),
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
                let next = match result {
                    Ok(text) => State::Ready {
                        session,
                        text,
                        notice: None,
                    },
                    Err(message) => State::Error {
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
            Internal::InjectionFinished { session, result } => {
                let State::Inserting {
                    session: current,
                    text,
                    action,
                } = &self.state
                else {
                    return out;
                };
                if *current != session {
                    return out;
                }
                let (action, text) = (*action, text.clone());
                let (ok, message) = match result {
                    Ok(()) => {
                        self.set(State::Idle, &mut out);
                        let msg = match action {
                            Action::Copy => "copied to clipboard",
                            Action::Insert => "inserted",
                        };
                        (true, msg.to_owned())
                    }
                    Err(message) => {
                        // Keep the result so the user can retry or copy manually.
                        self.set(
                            State::Ready {
                                session,
                                text,
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
            result: Ok(text.into()),
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
            vec![Effect::ScheduleRecordingLimit { session: 1 }]
        );

        let (r, o) = run(&mut c, Command::Toggle);
        assert!(r.ok);
        assert_eq!(kind(&c), StateKind::Processing);
        assert_eq!(o.effects, vec![Effect::StartProcessing { session: 1 }]);

        let o = c.handle_internal(Internal::ProcessingFinished {
            session: 1,
            result: Ok("hello".into()),
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
            result: Ok("x".into()),
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
            result: Ok("old".into()),
        });
        assert!(!o.changed);
        assert_eq!(c.state(), &State::Recording { session: 2 });

        // Old recording-limit timers must not stop the new session.
        let o = c.handle_internal(Internal::RecordingLimitReached { session: 1 });
        assert!(!o.changed);
        assert_eq!(kind(&c), StateKind::Recording);
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
                text: "keep me".into(),
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
    fn status_never_contains_text() {
        let mut c = Controller::new();
        run(&mut c, Command::Start);
        run(&mut c, Command::Stop);
        c.handle_internal(Internal::ProcessingFinished {
            session: 1,
            result: Ok("secret words".into()),
        });
        let json = serde_json::to_string(&c.status()).unwrap();
        assert!(!json.contains("secret"));
    }
}

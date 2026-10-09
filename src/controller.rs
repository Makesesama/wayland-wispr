//! Session state machine.
//!
//! The [`Controller`] is pure: it consumes commands/internal events and returns
//! a reply plus [`Effect`]s for the runtime to execute. This keeps it free of
//! GTK, threads, and I/O so transitions can be unit-tested.

use serde::{Deserialize, Serialize};

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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StateKind {
    Idle,
    Recording,
    Processing,
    Ready,
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
            State::Ready { session, text } => (
                StateKind::Ready,
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

    pub fn handle_command(&mut self, cmd: Command) -> (Reply, Outcome) {
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
            (Command::Stop, State::Idle | State::Error { .. }) => Ok(Some("not recording")),
            (Command::Stop, State::Processing { .. }) => Ok(Some("already processing")),
            (Command::Stop, State::Ready { .. }) => Ok(Some("result already ready")),
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

            (Command::Copy | Command::Insert, State::Ready { .. }) => {
                Err("clipboard/insertion is not implemented yet")
            }
            (Command::Copy | Command::Insert, _) => Err("no ready result"),
        };

        let (ok, message) = match result {
            Ok(msg) => (true, msg),
            Err(msg) => (false, Some(msg)),
        };
        let reply = Reply {
            ok,
            message: message.map(str::to_owned),
            status: self.status(),
        };
        (reply, out)
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
                    Ok(text) => State::Ready { session, text },
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
        }
        out
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

    #[test]
    fn toggle_cycle_reaches_ready() {
        let mut c = Controller::new();
        let (r, o) = c.handle_command(Command::Toggle);
        assert!(r.ok);
        assert_eq!(kind(&c), StateKind::Recording);
        assert_eq!(
            o.effects,
            vec![Effect::ScheduleRecordingLimit { session: 1 }]
        );

        let (r, o) = c.handle_command(Command::Toggle);
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
        c.handle_command(Command::Start);
        c.handle_command(Command::Stop);
        let (r, o) = c.handle_command(Command::Toggle);
        assert!(!r.ok);
        assert!(!o.changed);
        assert_eq!(kind(&c), StateKind::Processing);

        c.handle_internal(Internal::ProcessingFinished {
            session: 1,
            result: Ok("x".into()),
        });
        let (r, _) = c.handle_command(Command::Toggle);
        assert!(!r.ok);
        assert_eq!(kind(&c), StateKind::Ready);
    }

    #[test]
    fn repeated_start_and_stop_are_safe() {
        let mut c = Controller::new();
        let (r, _) = c.handle_command(Command::Stop);
        assert!(r.ok);
        assert_eq!(kind(&c), StateKind::Idle);

        c.handle_command(Command::Start);
        let (r, o) = c.handle_command(Command::Start);
        assert!(r.ok);
        assert!(o.effects.is_empty());
        assert_eq!(c.status().session, Some(1));

        c.handle_command(Command::Stop);
        let (r, o) = c.handle_command(Command::Stop);
        assert!(r.ok);
        assert!(o.effects.is_empty());
    }

    #[test]
    fn stale_results_are_ignored() {
        let mut c = Controller::new();
        c.handle_command(Command::Start);
        c.handle_command(Command::Stop);
        c.handle_command(Command::Cancel);
        c.handle_command(Command::Start); // session 2
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
        c.handle_command(Command::Start);
        let o = c.handle_internal(Internal::RecordingLimitReached { session: 1 });
        assert_eq!(o.effects, vec![Effect::StartProcessing { session: 1 }]);
        assert_eq!(kind(&c), StateKind::Processing);
    }

    #[test]
    fn errors_are_recoverable() {
        let mut c = Controller::new();
        c.handle_command(Command::Start);
        c.handle_command(Command::Stop);
        c.handle_internal(Internal::ProcessingFinished {
            session: 1,
            result: Err("boom".into()),
        });
        assert_eq!(c.status().error.as_deref(), Some("boom"));
        let (r, _) = c.handle_command(Command::Toggle);
        assert!(r.ok);
        assert_eq!(c.state(), &State::Recording { session: 2 });
    }

    #[test]
    fn copy_requires_ready_result() {
        let mut c = Controller::new();
        let (r, _) = c.handle_command(Command::Copy);
        assert!(!r.ok);
        let (r, _) = c.handle_command(Command::Cancel);
        assert!(r.ok);
    }

    #[test]
    fn status_never_contains_text() {
        let mut c = Controller::new();
        c.handle_command(Command::Start);
        c.handle_command(Command::Stop);
        c.handle_internal(Internal::ProcessingFinished {
            session: 1,
            result: Ok("secret words".into()),
        });
        let json = serde_json::to_string(&c.status()).unwrap();
        assert!(!json.contains("secret"));
    }
}

//! Resident process: wires the controller thread, IPC server, and GTK overlay.

use std::sync::{Arc, mpsc};
use std::thread;
use std::time::Duration;

use anyhow::{Context, bail};

use crate::audio::{self, Capture, CaptureEvent};
use crate::cleanup::{Cleaner, Processed};
use crate::config::Config;
use crate::controller::{Command, Controller, Effect, Internal, Reply, SessionId};
use crate::injection::Injector;
use crate::transcription::{
    CancelFlag, Progress, TranscribeError, Transcriber, strip_hallucinations,
};
use crate::ui::{UiEvent, View};
use crate::{ipc, probe, ui};

/// Covers copy/insert, which reply only after the paste chord was sent.
const REPLY_TIMEOUT: Duration = Duration::from_secs(15);

enum Msg {
    Command(Command, mpsc::SyncSender<Reply>),
    Internal(Internal),
}

pub fn run(config: Config) -> anyhow::Result<()> {
    check_capabilities()?;
    let injector = Arc::new(Injector::new(&config.injection).context("preparing paste chord")?);
    let transcriber = Arc::new(Transcriber::new(&config).context("setting up transcription")?);
    let cleaner = Arc::new(Cleaner::new(&config.cleanup).context("setting up cleanup")?);

    let socket = ipc::socket_path()?;
    let (listener, socket_guard) = ipc::bind(&socket)?;
    tracing::info!(socket = %socket.display(), "control socket ready");

    let (msg_tx, msg_rx) = mpsc::channel::<Msg>();
    let (ui_tx, ui_rx) = async_channel::unbounded::<UiEvent>();

    thread::Builder::new()
        .name("controller".into())
        .spawn({
            let runtime = Runtime {
                tx: msg_tx.clone(),
                ui: ui_tx,
                config: Arc::new(config.clone()),
                injector,
                transcriber,
                cleaner,
                capture: None,
                processing: None,
            };
            move || runtime.run(msg_rx)
        })
        .context("spawning controller thread")?;

    thread::Builder::new()
        .name("ipc".into())
        .spawn(move || {
            ipc::serve(listener, |cmd| {
                let (reply_tx, reply_rx) = mpsc::sync_channel(1);
                msg_tx.send(Msg::Command(cmd, reply_tx)).ok()?;
                reply_rx.recv_timeout(REPLY_TIMEOUT).ok()
            })
        })
        .context("spawning IPC thread")?;

    let shown = ui::run(config, ui_rx);

    // Remove only the socket this process created. Worker threads are
    // detached and end with the process; they hold no persistent resources.
    drop(socket_guard);

    if !shown {
        bail!("overlay could not be shown");
    }
    tracing::info!("shut down");
    Ok(())
}

fn check_capabilities() -> anyhow::Result<()> {
    let caps = probe::probe().context("probing Wayland capabilities")?;
    if caps.layer_shell.is_none() {
        bail!(
            "compositor does not advertise {}; the overlay requires wlr layer-shell",
            probe::LAYER_SHELL
        );
    }
    if !caps.can_insert() {
        tracing::warn!(
            virtual_keyboard = caps.virtual_keyboard.is_some(),
            data_control = caps.data_control(),
            seats = caps.seats,
            "native clipboard/virtual-keyboard insertion will be unavailable"
        );
    } else {
        tracing::info!("layer-shell, data-control, and virtual-keyboard available");
    }
    Ok(())
}

/// Owns everything the controller's effects act on. Runs on the
/// controller thread; blocking work goes to worker threads.
struct Runtime {
    tx: mpsc::Sender<Msg>,
    ui: async_channel::Sender<UiEvent>,
    config: Arc<Config>,
    injector: Arc<Injector>,
    transcriber: Arc<Transcriber>,
    cleaner: Arc<Cleaner>,
    /// The microphone capture of the current recording session, if any.
    capture: Option<(SessionId, Capture)>,
    /// Cancellation handle of the session being processed, if any.
    processing: Option<(SessionId, CancelFlag)>,
}

impl Runtime {
    fn run(mut self, rx: mpsc::Receiver<Msg>) {
        let mut controller = Controller::new();
        // Reply channel of the one copy/insert command awaiting completion.
        // The controller rejects further copy/insert commands until then.
        let mut pending: Option<mpsc::SyncSender<Reply>> = None;

        for msg in rx {
            let outcome = match msg {
                Msg::Command(cmd, reply_tx) => {
                    let (reply, outcome) = controller.handle_command(cmd);
                    match reply {
                        Some(reply) => {
                            log_reply(&format!("{cmd:?}"), &reply);
                            let _ = reply_tx.send(reply);
                        }
                        None => pending = Some(reply_tx),
                    }
                    outcome
                }
                Msg::Internal(event) => controller.handle_internal(event),
            };

            if let Some(reply) = outcome.deferred_reply.as_ref() {
                log_reply("deferred", reply);
                if let Some(reply_tx) = pending.take() {
                    let _ = reply_tx.send(reply.clone());
                }
            }

            if outcome.changed {
                let view = View::from(controller.state());
                if self.ui.send_blocking(UiEvent::State(view)).is_err() {
                    break; // UI is gone.
                }
            }

            for effect in outcome.effects {
                self.execute(effect);
            }
        }
    }

    fn execute(&mut self, effect: Effect) {
        match effect {
            Effect::StartCapture { session } => {
                let tx = self.tx.clone();
                let ui = self.ui.clone();
                let capture = Capture::start(
                    &self.config.audio,
                    self.config.max_recording(),
                    move |event| match event {
                        CaptureEvent::Level(dbfs) => {
                            let _ = ui.try_send(UiEvent::Level { session, dbfs });
                        }
                        CaptureEvent::Failed(message) => {
                            let _ = tx
                                .send(Msg::Internal(Internal::CaptureFailed { session, message }));
                        }
                    },
                );
                // Replacing an older capture cancels it.
                self.capture = Some((session, capture));
                // Load the models while the user speaks.
                self.transcriber.prepare();
                self.cleaner.prepare();
            }
            Effect::DiscardCapture { session } => {
                if self.capture.as_ref().is_some_and(|(s, _)| *s == session) {
                    self.capture = None; // Drop cancels without blocking.
                }
            }
            Effect::ScheduleRecordingLimit { session } => {
                let delay = self.config.max_recording();
                self.spawn("limit", move || {
                    thread::sleep(delay);
                    Internal::RecordingLimitReached { session }
                });
            }
            Effect::StartProcessing { session } => {
                let capture = match self.capture.take() {
                    Some((s, capture)) if s == session => Some(capture),
                    other => {
                        self.capture = other;
                        None
                    }
                };
                let config = self.config.clone();
                let transcriber = self.transcriber.clone();
                let cleaner = self.cleaner.clone();
                let cancel = CancelFlag::default();
                self.processing = Some((session, cancel.clone()));
                let ui = self.ui.clone();
                self.spawn("process", move || Internal::ProcessingFinished {
                    session,
                    result: process(
                        session,
                        capture,
                        &config,
                        &transcriber,
                        &cleaner,
                        &cancel,
                        ui,
                    ),
                });
            }
            Effect::AbortProcessing { session } => {
                if let Some((s, cancel)) = self.processing.take() {
                    if s == session {
                        cancel.cancel();
                    } else {
                        self.processing = Some((s, cancel));
                    }
                }
            }
            Effect::Inject {
                session,
                action,
                text,
            } => {
                let injector = self.injector.clone();
                self.spawn("inject", move || {
                    let result = injector.run(action, &text);
                    if let Err(e) = &result {
                        tracing::warn!(session, ?action, error = %e, "injection failed");
                    }
                    Internal::InjectionFinished { session, result }
                });
            }
        }
    }

    /// Run `work` on a worker thread and feed its event back to the
    /// controller.
    fn spawn<F>(&self, name: &str, work: F)
    where
        F: FnOnce() -> Internal + Send + 'static,
    {
        let tx = self.tx.clone();
        let spawned = thread::Builder::new().name(name.into()).spawn(move || {
            let _ = tx.send(Msg::Internal(work()));
        });
        if let Err(e) = spawned {
            // Without the worker the controller would wait forever; there is
            // no sensible recovery from thread exhaustion here.
            tracing::error!(error = %e, worker = name, "failed to spawn worker");
        }
    }
}

/// Stop the capture, normalize the audio, transcribe it, and clean up the
/// transcript. Blocking; runs on a worker thread.
fn process(
    session: SessionId,
    capture: Option<Capture>,
    config: &Config,
    transcriber: &Transcriber,
    cleaner: &Cleaner,
    cancel: &CancelFlag,
    ui: async_channel::Sender<UiEvent>,
) -> Result<Processed, String> {
    let capture = capture.ok_or("no recording for this session")?;
    let raw = capture.finish()?;

    let clip = audio::prepare(&raw, &config.audio, |clip| {
        if let Some(dir) = &config.debug.save_recordings_dir {
            match audio::save_debug_wav(dir, session, clip) {
                Ok(path) => tracing::info!(path = %path.display(), "saved debug recording"),
                Err(e) => tracing::warn!(error = %e, "could not save debug recording"),
            }
        }
    })?;

    let progress = Box::new({
        let ui = ui.clone();
        move |stage| {
            let label = match stage {
                Progress::LoadingModel => "Loading model…",
                Progress::Transcribing => "Transcribing…",
            };
            let _ = ui.try_send(UiEvent::Stage { session, label });
        }
    });
    let transcript = match transcriber.transcribe(&clip, cancel, progress) {
        Ok(transcript) => transcript,
        Err(e @ TranscribeError::Cancelled) => {
            // The controller already moved on; this result is ignored.
            tracing::debug!(session, "transcription cancelled");
            return Err(e.to_string());
        }
        Err(e) => {
            tracing::warn!(session, error = %e, "transcription failed");
            return Err(e.to_string());
        }
    };

    let raw = strip_hallucinations(&transcript.text);
    if raw.len() != transcript.text.trim().len() {
        tracing::info!(session, "dropped hallucinated subtitle credits");
    }
    if raw.is_empty() {
        return Err(TranscribeError::NoSpeech.to_string());
    }

    cleaner
        .run(raw, cancel, || {
            let _ = ui.try_send(UiEvent::Stage {
                session,
                label: "Polishing…",
            });
        })
        .map_err(|_| {
            tracing::debug!(session, "cleanup cancelled");
            "cancelled".to_owned()
        })
}

fn log_reply(command: &str, reply: &Reply) {
    // Messages may carry error text but never transcript content.
    tracing::info!(
        command,
        ok = reply.ok,
        state = ?reply.status.state,
        session = reply.status.session,
        message = reply.message.as_deref().unwrap_or(""),
        "command handled"
    );
}

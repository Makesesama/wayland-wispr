//! Resident process: wires the controller thread, IPC server, and GTK overlay.

use std::sync::{Arc, mpsc};
use std::thread;
use std::time::Duration;

use anyhow::{Context, bail};

use crate::config::Config;
use crate::controller::{Command, Controller, Effect, Internal, Reply};
use crate::injection::Injector;
use crate::ui::View;
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

    let socket = ipc::socket_path()?;
    let (listener, socket_guard) = ipc::bind(&socket)?;
    tracing::info!(socket = %socket.display(), "control socket ready");

    let (msg_tx, msg_rx) = mpsc::channel::<Msg>();
    let (view_tx, view_rx) = async_channel::unbounded::<View>();

    thread::Builder::new()
        .name("controller".into())
        .spawn({
            let msg_tx = msg_tx.clone();
            let config = config.clone();
            move || controller_loop(msg_rx, msg_tx, view_tx, config, injector)
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

    let shown = ui::run(config, view_rx);

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

fn controller_loop(
    rx: mpsc::Receiver<Msg>,
    tx: mpsc::Sender<Msg>,
    views: async_channel::Sender<View>,
    config: Config,
    injector: Arc<Injector>,
) {
    let mut controller = Controller::new();
    // Reply channel of the one copy/insert command awaiting completion. The
    // controller rejects further copy/insert commands until it completes.
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

        if outcome.changed && views.send_blocking(View::from(controller.state())).is_err() {
            break; // UI is gone.
        }

        for effect in outcome.effects {
            execute(effect, &tx, &config, &injector);
        }
    }
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

/// Effect execution on worker threads. Audio capture and transcription are
/// not implemented yet; processing produces a fixed text after a delay.
fn execute(effect: Effect, tx: &mpsc::Sender<Msg>, config: &Config, injector: &Arc<Injector>) {
    let (delay, event) = match effect {
        Effect::Inject {
            session,
            action,
            text,
        } => {
            let worker_tx = tx.clone();
            let injector = injector.clone();
            let spawned = thread::Builder::new().name("inject".into()).spawn(move || {
                let result = injector.run(action, &text);
                if let Err(e) = &result {
                    tracing::warn!(session, ?action, error = %e, "injection failed");
                }
                let _ = worker_tx.send(Msg::Internal(Internal::InjectionFinished {
                    session,
                    result,
                }));
            });
            if let Err(e) = spawned {
                tracing::error!(error = %e, "failed to spawn injection worker");
                let _ = tx.send(Msg::Internal(Internal::InjectionFinished {
                    session,
                    result: Err("could not start injection worker".into()),
                }));
            }
            return;
        }
        Effect::ScheduleRecordingLimit { session } => (
            config.max_recording(),
            Internal::RecordingLimitReached { session },
        ),
        Effect::StartProcessing { session } => (
            config.fake_processing(),
            Internal::ProcessingFinished {
                session,
                result: Ok(config.debug.fake_transcript.clone().unwrap_or_else(|| {
                    format!(
                        "Placeholder transcript for session {session}; \
                         audio capture and transcription are not implemented yet."
                    )
                })),
            },
        ),
    };

    let tx = tx.clone();
    let spawned = thread::Builder::new().name("effect".into()).spawn(move || {
        thread::sleep(delay);
        let _ = tx.send(Msg::Internal(event));
    });
    if let Err(e) = spawned {
        tracing::error!(error = %e, "failed to spawn effect worker");
    }
}

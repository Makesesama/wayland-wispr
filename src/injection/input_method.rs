//! Text insertion as the seat's input method (`zwp_input_method_v2`).
//!
//! The compositor relays `commit_string` to the focused client's
//! `zwp_text_input_v3`, so text arrives as one atomic edit instead of key
//! events: no keymaps, no held modifiers, no paste chord, and nothing is
//! inserted unless a text field is actually active.
//!
//! The input method has to stay bound for the whole process: compositors
//! (river, sway) only send text-input `enter` events while an input method
//! exists, and clients only enable text input after `enter`. A dedicated
//! thread owns a private connection (never GTK's) and tracks activation;
//! insertions are sent to it over a channel.
//!
//! Only one input method can be bound per seat. If another one (fcitx5,
//! ibus, …) holds it, the compositor answers `unavailable`; insertion then
//! fails and the input method is requested again on the next attempt.

use std::io::{self, Read, Write};
use std::os::fd::AsFd;
use std::os::unix::net::UnixStream;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use rustix::event::{PollFd, PollFlags, poll};
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::{wl_registry, wl_seat};
use wayland_client::{Connection, Dispatch, EventQueue, QueueHandle, delegate_noop};
use wayland_protocols_misc::zwp_input_method_v2::client::{
    zwp_input_method_manager_v2::ZwpInputMethodManagerV2,
    zwp_input_method_v2::{self, ZwpInputMethodV2},
};

/// Longest `commit_string` payload in bytes. Wayland messages are limited to
/// 4096 bytes (header, length prefix, NUL, and padding included), on our
/// side and when the compositor relays the text to the client.
const MAX_CHUNK: usize = 3900;
/// How long to wait for the insertion thread to answer.
const REPLY_TIMEOUT: Duration = Duration::from_secs(10);
/// After re-requesting the input method, how long a focused client gets to
/// re-enable its text input.
const REACTIVATE_WAIT: Duration = Duration::from_millis(250);
/// Between chunks: wait until the client's reaction (a `done` from the
/// compositor) has settled, so the next commit carries the current serial.
const CHUNK_QUIET: Duration = Duration::from_millis(40);
const CHUNK_MAX_WAIT: Duration = Duration::from_millis(400);

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ImError {
    #[error("no text field is focused")]
    NoTextField,
    #[error("another input method (e.g. fcitx5 or ibus) is active on this seat")]
    Unavailable,
    #[error("input method: {0}")]
    Failed(String),
}

enum Request {
    Commit {
        text: String,
        reply: mpsc::SyncSender<Result<(), ImError>>,
    },
}

/// Handle to the input-method thread.
#[derive(Debug)]
pub struct InputMethod {
    tx: mpsc::Sender<Request>,
    /// Wakes the thread from `poll` when a request is queued.
    wake: UnixStream,
}

impl std::fmt::Debug for Request {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Request")
    }
}

impl InputMethod {
    /// Connect, bind the input method on the first seat, and start the
    /// thread. Fails if the compositor lacks `zwp_input_method_manager_v2`;
    /// an input method already held by another program is not an error.
    pub fn start() -> anyhow::Result<Self> {
        let conn = Connection::connect_to_env().map_err(|e| err("connect", e))?;
        let (globals, mut queue) =
            registry_queue_init::<State>(&conn).map_err(|e| err("registry", e))?;
        let qh = queue.handle();

        let seats = globals
            .contents()
            .with_list(|l| l.iter().filter(|g| g.interface == "wl_seat").count());
        if seats > 1 {
            tracing::warn!(seats, "multiple seats; using the first advertised one");
        }
        let seat: wl_seat::WlSeat = globals
            .bind(&qh, 1..=1, ())
            .map_err(|e| err("wl_seat", e))?;
        let manager: ZwpInputMethodManagerV2 = globals.bind(&qh, 1..=1, ()).map_err(|_| {
            anyhow::anyhow!(
                "the compositor does not support zwp_input_method_manager_v2; \
                 set injection.method = \"type\" or \"paste\""
            )
        })?;

        let mut state = State::default();
        let im = manager.get_input_method(&seat, &qh, ());
        queue
            .roundtrip(&mut state)
            .map_err(|e| err("roundtrip", e))?;
        if state.unavailable {
            tracing::warn!(
                "another input method is active on this seat; \
                 insertion will fail until it exits"
            );
        } else {
            tracing::info!("bound as the seat's input method");
        }

        let (wake_rx, wake) = UnixStream::pair()?;
        wake_rx.set_nonblocking(true)?;
        wake.set_nonblocking(true)?;
        let (tx, rx) = mpsc::channel();
        let worker = Worker {
            conn,
            queue,
            qh,
            seat,
            manager,
            im,
            state,
        };
        thread::Builder::new()
            .name("input-method".into())
            .spawn(move || {
                if let Err(e) = worker.run(rx, wake_rx) {
                    tracing::error!(error = %e, "input method connection lost");
                }
            })?;
        Ok(Self { tx, wake })
    }

    /// Commit `text` to the focused text field. Blocking.
    pub fn commit(&self, text: &str) -> Result<(), ImError> {
        let lost = || ImError::Failed("connection to the compositor was lost".into());
        let (reply, result) = mpsc::sync_channel(1);
        self.tx
            .send(Request::Commit {
                text: text.to_owned(),
                reply,
            })
            .map_err(|_| lost())?;
        // A full socket buffer already guarantees a wakeup.
        let _ = (&self.wake).write(&[1]);
        result.recv_timeout(REPLY_TIMEOUT).map_err(|_| lost())?
    }
}

fn err(context: &str, e: impl std::fmt::Display) -> anyhow::Error {
    anyhow::anyhow!("input method: {context}: {e}")
}

/// Input-method state as of the last `done`.
#[derive(Debug, Default)]
struct State {
    /// A text field is active (applied on `done`).
    active: bool,
    /// `activate`/`deactivate` received since the last `done`.
    pending_active: Option<bool>,
    /// Number of `done` events: the serial `commit` must carry.
    serial: u32,
    /// The compositor refused (or withdrew) the input method.
    unavailable: bool,
}

impl Dispatch<ZwpInputMethodV2, ()> for State {
    fn event(
        state: &mut Self,
        _: &ZwpInputMethodV2,
        event: zwp_input_method_v2::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwp_input_method_v2::Event::Activate => state.pending_active = Some(true),
            zwp_input_method_v2::Event::Deactivate => state.pending_active = Some(false),
            // Activation is double-buffered like the rest of the state.
            zwp_input_method_v2::Event::Done => {
                if let Some(active) = state.pending_active.take() {
                    state.active = active;
                    tracing::debug!(active, "text field activation changed");
                }
                state.serial = state.serial.wrapping_add(1);
            }
            zwp_input_method_v2::Event::Unavailable => {
                state.unavailable = true;
                state.active = false;
                state.pending_active = None;
            }
            // Surrounding text, change cause, content type: not needed.
            _ => {}
        }
    }
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

delegate_noop!(State: ignore wl_seat::WlSeat);
delegate_noop!(State: ZwpInputMethodManagerV2);

struct Worker {
    conn: Connection,
    queue: EventQueue<State>,
    qh: QueueHandle<State>,
    seat: wl_seat::WlSeat,
    manager: ZwpInputMethodManagerV2,
    im: ZwpInputMethodV2,
    state: State,
}

impl Worker {
    fn run(mut self, rx: mpsc::Receiver<Request>, mut wake: UnixStream) -> anyhow::Result<()> {
        loop {
            self.queue.dispatch_pending(&mut self.state)?;
            loop {
                match rx.try_recv() {
                    Ok(Request::Commit { text, reply }) => {
                        let result = self.commit(&text);
                        let _ = reply.send(result);
                    }
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => return Ok(()),
                }
            }
            self.conn.flush()?;

            let Some(guard) = self.queue.prepare_read() else {
                continue; // Events are already queued.
            };
            let (wayland_ready, wake_ready) = {
                let (wayland_fd, wake_fd) = (guard.connection_fd(), wake.as_fd());
                let mut fds = [
                    PollFd::new(&wayland_fd, PollFlags::IN),
                    PollFd::new(&wake_fd, PollFlags::IN),
                ];
                match poll(&mut fds, None) {
                    Ok(_) => {}
                    Err(rustix::io::Errno::INTR) => continue,
                    Err(e) => return Err(e.into()),
                }
                let ready = |fd: &PollFd| {
                    fd.revents()
                        .intersects(PollFlags::IN | PollFlags::ERR | PollFlags::HUP)
                };
                (ready(&fds[0]), ready(&fds[1]))
            };
            if wayland_ready {
                match guard.read() {
                    Ok(_) => {}
                    Err(wayland_client::backend::WaylandError::Io(e))
                        if e.kind() == io::ErrorKind::WouldBlock => {}
                    Err(e) => return Err(e.into()),
                }
            } else {
                drop(guard);
            }
            if wake_ready {
                let mut buf = [0u8; 64];
                while matches!(wake.read(&mut buf), Ok(n) if n > 0) {}
            }
        }
    }

    fn roundtrip(&mut self) -> Result<(), ImError> {
        self.queue
            .roundtrip(&mut self.state)
            .map(drop)
            .map_err(|e| ImError::Failed(e.to_string()))
    }

    fn commit(&mut self, text: &str) -> Result<(), ImError> {
        // Catch up on activation changes and the latest serial.
        self.roundtrip()?;
        if self.state.unavailable {
            self.reacquire()?;
        }
        if !self.state.active {
            return Err(ImError::NoTextField);
        }

        let chunks = chunks(text, MAX_CHUNK);
        for (i, chunk) in chunks.iter().enumerate() {
            if i > 0 {
                self.settle()?;
                if !self.state.active {
                    // Focus moved mid-insertion; never type into whatever
                    // is focused now.
                    return Err(ImError::Failed(format!(
                        "text field lost focus after {i} of {} parts",
                        chunks.len()
                    )));
                }
            }
            self.im.commit_string((*chunk).to_owned());
            self.im.commit(self.state.serial);
            // Surfaces protocol errors before reporting success.
            self.roundtrip()?;
        }
        Ok(())
    }

    /// Let the client's reaction to the last commit arrive: wait until no
    /// `done` came in for `CHUNK_QUIET`.
    fn settle(&mut self) -> Result<(), ImError> {
        let start = Instant::now();
        let mut serial = self.state.serial;
        let mut quiet_since = Instant::now();
        while start.elapsed() < CHUNK_MAX_WAIT {
            thread::sleep(Duration::from_millis(10));
            self.roundtrip()?;
            if self.state.serial != serial {
                serial = self.state.serial;
                quiet_since = Instant::now();
            } else if quiet_since.elapsed() >= CHUNK_QUIET {
                break;
            }
        }
        Ok(())
    }

    /// Request the input method again after `unavailable` (another input
    /// method may have exited since).
    fn reacquire(&mut self) -> Result<(), ImError> {
        self.im.destroy();
        self.state = State::default();
        self.im = self.manager.get_input_method(&self.seat, &self.qh, ());
        self.roundtrip()?;
        if self.state.unavailable {
            return Err(ImError::Unavailable);
        }
        tracing::info!("bound as the seat's input method");
        // The focused client re-enables its text input only now.
        let start = Instant::now();
        while !self.state.active && start.elapsed() < REACTIVATE_WAIT {
            thread::sleep(Duration::from_millis(10));
            self.roundtrip()?;
        }
        Ok(())
    }
}

/// Split `text` into pieces of at most `max` bytes at char boundaries.
fn chunks(text: &str, max: usize) -> Vec<&str> {
    let mut out = Vec::new();
    let mut rest = text;
    while rest.len() > max {
        let mut end = max;
        while !rest.is_char_boundary(end) {
            end -= 1;
        }
        let (head, tail) = rest.split_at(end);
        out.push(head);
        rest = tail;
    }
    if !rest.is_empty() {
        out.push(rest);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunks_respect_limit_and_char_boundaries() {
        assert!(chunks("", 10).is_empty());
        assert_eq!(chunks("abc", 10), vec!["abc"]);
        assert_eq!(chunks("abcdef", 3), vec!["abc", "def"]);
        // "ü" is two bytes; never split inside it.
        let parts = chunks("aüüü", 2);
        assert_eq!(parts.concat(), "aüüü");
        assert!(parts.iter().all(|p| p.len() <= 2));

        let long = "Grüße ".repeat(2000);
        let parts = chunks(&long, MAX_CHUNK);
        assert!(parts.len() > 1);
        assert!(parts.iter().all(|p| p.len() <= MAX_CHUNK));
        assert_eq!(parts.concat(), long);
    }
}

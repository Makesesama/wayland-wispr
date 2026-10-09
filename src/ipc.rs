//! Local control socket: one JSON request line, one JSON response line.
//!
//! The socket lives in a private `$XDG_RUNTIME_DIR/wayland-wispr/` directory.
//! Peers must have the same UID; messages are size-bounded and versioned.

use std::fs::{self, DirBuilder};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::controller::{Command, Reply, Status};

pub const PROTOCOL_VERSION: u32 = 1;
const MAX_MESSAGE_BYTES: u64 = 4096;
const SERVER_IO_TIMEOUT: Duration = Duration::from_secs(2);
const CLIENT_IO_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Debug, thiserror::Error)]
pub enum IpcError {
    #[error(
        "XDG_RUNTIME_DIR is not set to an absolute path; refusing to create a socket elsewhere"
    )]
    NoRuntimeDir,
    #[error("runtime directory {0} is not a private directory owned by this user")]
    InsecureDir(PathBuf),
    #[error("another wayland-wispr instance is already listening on {0}")]
    AlreadyRunning(PathBuf),
    #[error("{0} exists and is not a stale socket owned by this user; not removing it")]
    PathOccupied(PathBuf),
    #[error("wayland-wispr is not running (no socket at {0}); start it with `wayland-wispr run`")]
    NotRunning(PathBuf),
    #[error("empty message")]
    Empty,
    #[error("message exceeds {MAX_MESSAGE_BYTES} bytes")]
    TooLarge,
    #[error("invalid message: {0}")]
    Protocol(String),
    #[error(transparent)]
    Io(#[from] io::Error),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub version: u32,
    pub command: Command,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    pub version: u32,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub status: Option<Status>,
}

impl Response {
    fn error(message: impl Into<String>) -> Self {
        Self {
            version: PROTOCOL_VERSION,
            ok: false,
            message: Some(message.into()),
            status: None,
        }
    }
}

impl From<Reply> for Response {
    fn from(r: Reply) -> Self {
        Self {
            version: PROTOCOL_VERSION,
            ok: r.ok,
            message: r.message,
            status: Some(r.status),
        }
    }
}

pub fn socket_path() -> Result<PathBuf, IpcError> {
    let dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .ok_or(IpcError::NoRuntimeDir)?;
    Ok(dir.join("wayland-wispr").join("control.sock"))
}

fn ensure_private_dir(dir: &Path) -> Result<(), IpcError> {
    match DirBuilder::new().mode(0o700).create(dir) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e.into()),
    }
    let meta = fs::symlink_metadata(dir)?;
    let private = meta.file_type().is_dir()
        && meta.uid() == rustix::process::getuid().as_raw()
        && meta.mode() & 0o077 == 0;
    if !private {
        return Err(IpcError::InsecureDir(dir.to_path_buf()));
    }
    Ok(())
}

/// Removes the socket on drop, but only if the path still refers to the
/// socket this process created.
#[derive(Debug)]
pub struct SocketGuard {
    path: PathBuf,
    dev: u64,
    ino: u64,
}

impl Drop for SocketGuard {
    fn drop(&mut self) {
        if let Ok(meta) = fs::symlink_metadata(&self.path)
            && meta.dev() == self.dev
            && meta.ino() == self.ino
        {
            let _ = fs::remove_file(&self.path);
        }
    }
}

/// Bind the control socket, detecting a live instance and replacing only a
/// stale socket owned by this user.
pub fn bind(path: &Path) -> Result<(UnixListener, SocketGuard), IpcError> {
    if let Some(parent) = path.parent() {
        ensure_private_dir(parent)?;
    }

    let listener = match UnixListener::bind(path) {
        Ok(l) => l,
        Err(e) if e.kind() == io::ErrorKind::AddrInUse => {
            if UnixStream::connect(path).is_ok() {
                return Err(IpcError::AlreadyRunning(path.to_path_buf()));
            }
            let meta = fs::symlink_metadata(path)?;
            if !meta.file_type().is_socket() || meta.uid() != rustix::process::getuid().as_raw() {
                return Err(IpcError::PathOccupied(path.to_path_buf()));
            }
            tracing::info!(path = %path.display(), "removing stale socket");
            fs::remove_file(path)?;
            UnixListener::bind(path)?
        }
        Err(e) => return Err(e.into()),
    };

    let meta = fs::symlink_metadata(path)?;
    let guard = SocketGuard {
        path: path.to_path_buf(),
        dev: meta.dev(),
        ino: meta.ino(),
    };
    Ok((listener, guard))
}

/// Accept connections forever, handling them sequentially. `handler` returns
/// `None` if the controller is gone.
pub fn serve<F>(listener: UnixListener, handler: F)
where
    F: Fn(Command) -> Option<Reply>,
{
    for conn in listener.incoming() {
        match conn {
            Ok(stream) => {
                if let Err(e) = handle_connection(stream, &handler) {
                    tracing::warn!(error = %e, "control connection failed");
                }
            }
            Err(e) => tracing::warn!(error = %e, "accept failed"),
        }
    }
}

fn handle_connection<F>(stream: UnixStream, handler: &F) -> Result<(), IpcError>
where
    F: Fn(Command) -> Option<Reply>,
{
    stream.set_read_timeout(Some(SERVER_IO_TIMEOUT))?;
    stream.set_write_timeout(Some(SERVER_IO_TIMEOUT))?;

    let peer = rustix::net::sockopt::socket_peercred(&stream).map_err(io::Error::from)?;
    if peer.uid != rustix::process::getuid() {
        tracing::warn!(
            uid = peer.uid.as_raw(),
            "rejecting control connection from other user"
        );
        return Ok(());
    }

    let response = match read_message(&stream) {
        // Connect-and-close is how a second instance checks for liveness.
        Err(IpcError::Empty) => return Ok(()),
        Err(IpcError::TooLarge) => Response::error("message too large"),
        Err(e) => return Err(e),
        Ok(line) => match serde_json::from_slice::<Request>(&line) {
            Err(e) => Response::error(format!("invalid request: {e}")),
            Ok(req) if req.version != PROTOCOL_VERSION => Response::error(format!(
                "unsupported protocol version {} (expected {PROTOCOL_VERSION})",
                req.version
            )),
            Ok(req) => match handler(req.command) {
                Some(reply) => reply.into(),
                None => Response::error("no reply from controller (timed out or shutting down)"),
            },
        },
    };

    write_message(&stream, &response)
}

fn read_message(stream: &UnixStream) -> Result<Vec<u8>, IpcError> {
    let mut reader = BufReader::new(stream.take(MAX_MESSAGE_BYTES + 1));
    let mut line = Vec::new();
    reader.read_until(b'\n', &mut line)?;
    if line.len() as u64 > MAX_MESSAGE_BYTES {
        return Err(IpcError::TooLarge);
    }
    if line.last() == Some(&b'\n') {
        line.pop();
    }
    if line.is_empty() {
        return Err(IpcError::Empty);
    }
    Ok(line)
}

fn write_message<T: Serialize>(mut stream: &UnixStream, msg: &T) -> Result<(), IpcError> {
    let mut buf = serde_json::to_vec(msg).map_err(|e| IpcError::Protocol(e.to_string()))?;
    buf.push(b'\n');
    stream.write_all(&buf)?;
    stream.flush()?;
    Ok(())
}

/// Send one command to the running instance.
pub fn send(path: &Path, command: Command) -> Result<Response, IpcError> {
    let stream = UnixStream::connect(path).map_err(|e| match e.kind() {
        io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused => {
            IpcError::NotRunning(path.to_path_buf())
        }
        _ => e.into(),
    })?;
    stream.set_read_timeout(Some(CLIENT_IO_TIMEOUT))?;
    stream.set_write_timeout(Some(CLIENT_IO_TIMEOUT))?;

    write_message(
        &stream,
        &Request {
            version: PROTOCOL_VERSION,
            command,
        },
    )?;
    let line = read_message(&stream)?;
    serde_json::from_slice(&line).map_err(|e| IpcError::Protocol(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::controller::Controller;
    use std::sync::Mutex;

    fn private_tempdir() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    fn spawn_server(path: &Path) -> SocketGuard {
        let (listener, guard) = bind(path).unwrap();
        std::thread::spawn(move || {
            let controller = Mutex::new(Controller::new());
            serve(listener, |cmd| {
                controller.lock().unwrap().handle_command(cmd).0
            });
        });
        guard
    }

    #[test]
    fn round_trip_and_single_instance() {
        let dir = private_tempdir();
        let path = dir.path().join("wispr").join("control.sock");
        let guard = spawn_server(&path);

        let r = send(&path, Command::Toggle).unwrap();
        assert!(r.ok);
        assert_eq!(
            r.status.unwrap().state,
            crate::controller::StateKind::Recording
        );

        assert!(matches!(bind(&path), Err(IpcError::AlreadyRunning(_))));

        drop(guard);
        assert!(!path.exists());
    }

    #[test]
    fn rejects_invalid_requests() {
        let dir = private_tempdir();
        let path = dir.path().join("wispr").join("control.sock");
        let _guard = spawn_server(&path);

        for msg in [
            &b"{\"version\":1,\"command\":\"explode\"}\n"[..],
            &b"{\"version\":99,\"command\":\"status\"}\n"[..],
            &b"not json\n"[..],
        ] {
            let mut s = UnixStream::connect(&path).unwrap();
            s.write_all(msg).unwrap();
            let line = read_message(&s).unwrap();
            let r: Response = serde_json::from_slice(&line).unwrap();
            assert!(!r.ok, "{msg:?} should be rejected");
        }

        let mut s = UnixStream::connect(&path).unwrap();
        let _ = s.write_all(&vec![b'x'; 10_000]);
        let line = read_message(&s).unwrap();
        let r: Response = serde_json::from_slice(&line).unwrap();
        assert_eq!(r.message.as_deref(), Some("message too large"));
    }

    #[test]
    fn replaces_stale_socket_but_not_regular_files() {
        let dir = private_tempdir();
        let sock_dir = dir.path().join("wispr");
        let path = sock_dir.join("control.sock");

        // Stale socket: bound then listener dropped without unlinking.
        ensure_private_dir(&sock_dir).unwrap();
        drop(UnixListener::bind(&path).unwrap());
        let (_l, _g) = bind(&path).unwrap();
        drop((_l, _g));

        fs::write(&path, b"not a socket").unwrap();
        assert!(matches!(bind(&path), Err(IpcError::PathOccupied(_))));
        assert!(path.exists());
    }

    #[test]
    fn send_reports_not_running() {
        let dir = private_tempdir();
        let path = dir.path().join("control.sock");
        assert!(matches!(
            send(&path, Command::Status),
            Err(IpcError::NotRunning(_))
        ));
    }
}

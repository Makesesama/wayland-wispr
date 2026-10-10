//! Note hook: a user executable that gets a result on stdin and puts it
//! wherever the user wants (a file, a notes app, a server, …).
//!
//! The hook is run directly (no shell) in its own process group, with the
//! transcript on stdin and `WAYLAND_WISPR_SESSION` in its environment. Exit
//! status 0 means saved. On timeout or cancel the whole group is killed, so
//! a half-finished upload cannot complete after the user saw a failure.
//! stdout is ignored; stderr is logged, and its last line is shown in the
//! overlay (never in `status`: it may contain the note).

use std::io::{self, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use rustix::process::{Pid, Signal, kill_process_group};

use crate::config::Config;
use crate::controller::{Failure, FailureKind, SessionId};
use crate::transcription::CancelFlag;

/// stderr kept for logging and the error detail; the rest is drained.
const MAX_STDERR_BYTES: usize = 16 * 1024;
/// Longest stderr line shown in the overlay.
const MAX_DETAIL_CHARS: usize = 200;
const POLL: Duration = Duration::from_millis(20);
/// How long to wait for stderr after the hook exited (a background child
/// may keep it open).
const DRAIN_GRACE: Duration = Duration::from_millis(500);

#[derive(Debug, Clone)]
pub struct NoteHook {
    path: PathBuf,
    timeout: Duration,
    /// Not passed on to the hook: `cleanup.api_key_env`.
    hidden_env: Option<String>,
}

impl NoteHook {
    /// `None` if no hook is configured.
    pub fn new(config: &Config) -> Option<Self> {
        Some(Self {
            path: config.note.hook.clone()?,
            timeout: Duration::from_secs(config.note.timeout_secs),
            hidden_env: config.cleanup.api_key_env.clone(),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Run the hook with `text` on stdin until it exits, times out, or
    /// `cancel` is set. Blocking; run off the GTK and controller threads.
    pub fn run(&self, session: SessionId, text: &str, cancel: &CancelFlag) -> Result<(), Failure> {
        let started = Instant::now();
        let mut child = self.spawn(session).map_err(|e| {
            Failure::from(format!("cannot run note hook {}: {e}", self.path.display()))
        })?;

        // Write and read on threads so neither a hook that ignores stdin nor
        // one that writes a lot of stderr can block us past the timeout.
        let stdin = child.stdin.take().expect("stdin is piped");
        let input = text.as_bytes().to_vec();
        thread::spawn(move || write_stdin(stdin, &input));
        let stderr = child.stderr.take().expect("stderr is piped");
        let (stderr_tx, stderr_rx) = mpsc::channel();
        thread::spawn(move || {
            let _ = stderr_tx.send(read_bounded(stderr));
        });

        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break Ok(status),
                Ok(None) if cancel.is_cancelled() => {
                    kill_group(&mut child);
                    break Err("note hook cancelled".to_owned());
                }
                Ok(None) if started.elapsed() >= self.timeout => {
                    kill_group(&mut child);
                    break Err(format!(
                        "note hook timed out after {} s",
                        self.timeout.as_secs()
                    ));
                }
                Ok(None) => thread::sleep(POLL),
                Err(e) => {
                    kill_group(&mut child);
                    break Err(format!("waiting for note hook failed: {e}"));
                }
            }
        };
        let stderr = stderr_rx.recv_timeout(DRAIN_GRACE).unwrap_or_default();
        let stderr = String::from_utf8_lossy(&stderr);
        let stderr = stderr.trim();
        if !stderr.is_empty() {
            // The hook's own diagnostics; what it prints is up to the user.
            tracing::info!(session, stderr, "note hook output");
        }

        let message = match status {
            Ok(status) if status.success() => {
                tracing::info!(session, elapsed = ?started.elapsed(), "note hook finished");
                return Ok(());
            }
            Ok(status) => describe_status(status),
            Err(message) => message,
        };
        tracing::warn!(session, hook = %self.path.display(), error = %message, "note hook failed");
        Err(Failure {
            message,
            detail: last_line(stderr),
            kind: FailureKind::Failed,
        })
    }

    fn spawn(&self, session: SessionId) -> io::Result<Child> {
        let mut attempts = 0;
        loop {
            let mut command = Command::new(&self.path);
            command
                .env("WAYLAND_WISPR_SESSION", session.to_string())
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                // Own group, so a timeout kills what the hook started too.
                .process_group(0);
            if let Some(var) = &self.hidden_env {
                command.env_remove(var);
            }
            match command.spawn() {
                // A script that was just written (e.g. saved by an editor)
                // may still be open for writing in another process.
                Err(e) if e.kind() == io::ErrorKind::ExecutableFileBusy && attempts < 5 => {
                    attempts += 1;
                    thread::sleep(POLL);
                }
                other => return other,
            }
        }
    }
}

/// Kill the hook and everything in its process group, then reap it.
fn kill_group(child: &mut Child) {
    let pid = Pid::from_child(child);
    if kill_process_group(pid, Signal::KILL).is_err() {
        let _ = child.kill();
    }
    let _ = child.wait();
}

fn write_stdin(mut stdin: impl Write, input: &[u8]) {
    let result = stdin.write_all(input).and_then(|()| stdin.flush());
    // A hook may legitimately not read its input.
    if let Err(e) = result
        && e.kind() != io::ErrorKind::BrokenPipe
    {
        tracing::debug!(error = %e, "writing to note hook failed");
    }
    // Dropping stdin sends EOF.
}

fn read_bounded(mut reader: impl Read) -> Vec<u8> {
    let mut kept = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        match reader.read(&mut buf) {
            Ok(0) | Err(_) => return kept,
            Ok(n) => {
                let room = MAX_STDERR_BYTES.saturating_sub(kept.len());
                kept.extend_from_slice(&buf[..n.min(room)]);
            }
        }
    }
}

fn describe_status(status: ExitStatus) -> String {
    match status.code() {
        Some(code) => format!("note hook exited with status {code}"),
        None => "note hook was killed by a signal".to_owned(),
    }
}

/// Last non-empty line of the hook's stderr, shortened for the overlay.
fn last_line(stderr: &str) -> Option<String> {
    let line = stderr
        .lines()
        .rev()
        .map(str::trim)
        .find(|l| !l.is_empty())?;
    Some(if line.chars().count() > MAX_DETAIL_CHARS {
        let cut: String = line.chars().take(MAX_DETAIL_CHARS - 1).collect();
        format!("{cut}…")
    } else {
        line.to_owned()
    })
}

/// For `check-config`: whether the hook can be run.
pub fn check(path: &Path) -> &'static str {
    match std::fs::metadata(path) {
        Err(_) => "missing",
        Ok(m) if !m.is_file() => "not a file",
        Ok(m) if m.permissions().mode() & 0o111 == 0 => "not executable",
        Ok(_) => "ok",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(hook: PathBuf, timeout_secs: u64) -> Config {
        let mut config = Config::default();
        config.note.hook = Some(hook);
        config.note.timeout_secs = timeout_secs;
        config
    }

    fn hook(dir: &Path, script: &str, timeout_secs: u64) -> NoteHook {
        let path = dir.join("hook.sh");
        std::fs::write(&path, format!("#!/bin/sh\n{script}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        NoteHook::new(&config(path, timeout_secs)).unwrap()
    }

    fn run(h: &NoteHook, text: &str) -> Result<(), Failure> {
        h.run(1, text, &CancelFlag::default())
    }

    #[test]
    fn unset_hook_disables_notes() {
        assert!(NoteHook::new(&Config::default()).is_none());
    }

    #[test]
    fn passes_text_on_stdin_and_session_in_env() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("out");
        let h = hook(
            dir.path(),
            &format!(
                "cat > '{}'; echo \"$WAYLAND_WISPR_SESSION\" >> '{0}'",
                out.display()
            ),
            5,
        );
        assert_eq!(check(h.path()), "ok");
        h.run(7, "Grüße\nzweite Zeile", &CancelFlag::default())
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(&out).unwrap(),
            "Grüße\nzweite Zeile7\n"
        );
    }

    #[test]
    fn hides_the_cleanup_api_key() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("out");
        let mut h = hook(
            dir.path(),
            &format!("echo \"${{HOME-unset}}\" > '{}'", out.display()),
            5,
        );
        // HOME stands in for the key variable (sh would invent a PATH).
        h.hidden_env = Some("HOME".into());
        run(&h, "x").unwrap();
        assert_eq!(std::fs::read_to_string(&out).unwrap(), "unset\n");
    }

    #[test]
    fn reports_exit_status_and_last_stderr_line_separately() {
        let dir = tempfile::tempdir().unwrap();
        let h = hook(
            dir.path(),
            "echo progress >&2; echo 'no inbox' >&2; exit 3",
            5,
        );
        assert_eq!(
            run(&h, "x").unwrap_err(),
            Failure {
                message: "note hook exited with status 3".into(),
                detail: Some("no inbox".into()),
                kind: FailureKind::Failed,
            }
        );
        let h = hook(dir.path(), "exit 1", 5);
        assert_eq!(
            run(&h, "x").unwrap_err(),
            "note hook exited with status 1".into()
        );
    }

    #[test]
    fn hook_may_ignore_stdin() {
        let dir = tempfile::tempdir().unwrap();
        let h = hook(dir.path(), "exit 0", 5);
        run(&h, &"x".repeat(1 << 20)).unwrap();
    }

    #[test]
    fn slow_hooks_are_killed_with_their_children() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("marker");
        // Not exec'd: `sleep` is a child of the shell, as in most scripts.
        let h = hook(
            dir.path(),
            &format!("sleep 2; touch '{}'", marker.display()),
            1,
        );
        let started = Instant::now();
        assert_eq!(
            run(&h, "x").unwrap_err(),
            "note hook timed out after 1 s".into()
        );
        assert!(started.elapsed() < Duration::from_secs(2));
        thread::sleep(Duration::from_millis(1500));
        assert!(!marker.exists(), "the hook's children must be killed too");
    }

    #[test]
    fn cancel_kills_the_hook() {
        let dir = tempfile::tempdir().unwrap();
        let h = hook(dir.path(), "sleep 10", 30);
        let cancel = CancelFlag::default();
        thread::spawn({
            let cancel = cancel.clone();
            move || {
                thread::sleep(Duration::from_millis(100));
                cancel.cancel();
            }
        });
        let started = Instant::now();
        assert_eq!(
            h.run(1, "x", &cancel).unwrap_err(),
            "note hook cancelled".into()
        );
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn missing_or_non_executable_hooks_fail() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nope");
        assert_eq!(check(&missing), "missing");
        let h = NoteHook::new(&config(missing, 5)).unwrap();
        assert!(
            run(&h, "x")
                .unwrap_err()
                .message
                .starts_with("cannot run note hook")
        );

        let plain = dir.path().join("plain");
        std::fs::write(&plain, "x").unwrap();
        assert_eq!(check(&plain), "not executable");
    }
}

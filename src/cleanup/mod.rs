//! LLM cleanup of raw transcripts through an OpenAI-compatible chat
//! completions server that the user runs (Ollama, llama-server, …).
//!
//! Cleanup never loses a transcript: if the server is unreachable, slow,
//! or replies with something unusable, the raw transcript is used and the
//! reason is reported alongside it.

mod client;
pub mod eval;
mod prompt;
mod sanitize;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::config::{CleanupConfig, is_loopback_host, split_url};
use crate::transcription::CancelFlag;
use client::Client;
pub use client::RequestError;
use prompt::Prompt;

/// Skip the preload request if the model was used this recently; servers
/// keep models loaded for minutes (Ollama: 5 min by default).
const WARM_WINDOW: Duration = Duration::from_secs(120);
/// Transcript used for the preload request.
const PRELOAD_TEXT: &str = "Hallo.";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CleanupStatus {
    /// `cleanup.enabled = false`.
    Disabled,
    /// Too short to be worth it (`cleanup.min_words`).
    Skipped,
    /// The model's reply is the result (possibly identical to the raw text).
    Cleaned,
    /// Cleanup failed; the result is the raw transcript.
    Failed,
}

/// Final text of a session plus what cleanup did to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Processed {
    pub text: String,
    /// The raw transcript, if cleanup changed it.
    pub raw: Option<String>,
    pub cleanup: CleanupStatus,
    /// Why cleanup failed (user-facing; never contains transcript text).
    pub cleanup_error: Option<String>,
}

impl Processed {
    pub fn uncleaned(text: String, cleanup: CleanupStatus) -> Self {
        Self {
            text,
            raw: None,
            cleanup,
            cleanup_error: None,
        }
    }

    pub fn raw_text(&self) -> &str {
        self.raw.as_deref().unwrap_or(&self.text)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cancelled;

pub struct Cleaner {
    inner: Option<Arc<Inner>>,
}

struct Inner {
    client: Client,
    prompt: Prompt,
    min_words: usize,
    preload: bool,
    preloading: AtomicBool,
    last_used: Mutex<Option<Instant>>,
}

impl Cleaner {
    pub fn new(config: &CleanupConfig) -> anyhow::Result<Self> {
        if !config.enabled {
            tracing::info!("LLM cleanup disabled");
            return Ok(Self { inner: None });
        }
        let prompt = Prompt::load(config.prompt_file.as_deref())?;
        if let Some(var) = &config.api_key_env
            && std::env::var(var).map_or(true, |k| k.trim().is_empty())
        {
            // Not fatal: dictation still works, cleanup reports the problem
            // per request and the raw transcript is used.
            tracing::warn!(var = %var, "cleanup.api_key_env is not set; cleanup will fail");
        }
        let local = split_url(config.base_url.trim()).is_some_and(|(_, h)| is_loopback_host(h));
        if config.preload && !local {
            tracing::info!("cleanup preload is only done for local servers");
        }
        if local {
            tracing::info!(base_url = %config.base_url, model = %config.model, "LLM cleanup enabled");
        } else {
            tracing::warn!(
                base_url = %config.base_url,
                model = %config.model,
                "LLM cleanup sends transcripts to a remote server"
            );
        }
        Ok(Self {
            inner: Some(Arc::new(Inner {
                client: Client::new(config),
                prompt,
                min_words: config.min_words,
                // Preloading a remote (paid) API would bill the whole prompt
                // on every recording start.
                preload: config.preload && local,
                preloading: AtomicBool::new(false),
                last_used: Mutex::new(None),
            })),
        })
    }

    /// Recording started: ask the server to load the model in the
    /// background, unless it was used recently. Errors are only logged; the
    /// real request reports them.
    pub fn prepare(&self) {
        let Some(inner) = &self.inner else { return };
        if !inner.preload || inner.recently_used() || inner.preloading.swap(true, Ordering::SeqCst)
        {
            return;
        }
        let worker = inner.clone();
        let spawned = thread::Builder::new()
            .name("cleanup-preload".into())
            .spawn(move || {
                match worker.preload_blocking() {
                    Ok(took) => {
                        tracing::debug!(ms = took.as_millis() as u64, "cleanup model ready")
                    }
                    Err(e) => tracing::debug!(error = %e, "cleanup preload failed"),
                }
                worker.preloading.store(false, Ordering::SeqCst);
            });
        if spawned.is_err() {
            inner.preloading.store(false, Ordering::SeqCst);
        }
    }

    /// Load the model and process the static prompt prefix; returns how
    /// long that took.
    pub fn preload_blocking(&self) -> Result<Duration, RequestError> {
        match &self.inner {
            Some(inner) => inner.preload_blocking(),
            None => Ok(Duration::ZERO),
        }
    }

    /// Clean up `raw`. Blocking; `on_start` runs right before the model is
    /// asked (not when cleanup is disabled or skipped). Only cancellation
    /// is an error: every other failure yields the raw transcript.
    pub fn run(
        &self,
        raw: String,
        cancel: &CancelFlag,
        on_start: impl FnOnce(),
    ) -> Result<Processed, Cancelled> {
        let Some(inner) = &self.inner else {
            return Ok(Processed::uncleaned(raw, CleanupStatus::Disabled));
        };
        if raw.split_whitespace().count() < inner.min_words {
            return Ok(Processed::uncleaned(raw, CleanupStatus::Skipped));
        }
        on_start();
        let started = Instant::now();
        match Inner::clean(inner, &raw, cancel) {
            Ok(text) => {
                tracing::info!(
                    ms = started.elapsed().as_millis() as u64,
                    changed = text != raw,
                    "cleanup finished"
                );
                Ok(Processed {
                    raw: (text != raw).then_some(raw),
                    text,
                    cleanup: CleanupStatus::Cleaned,
                    cleanup_error: None,
                })
            }
            Err(RequestError::Cancelled) => Err(Cancelled),
            Err(e) => {
                tracing::warn!(error = %e, "cleanup failed; using the raw transcript");
                Ok(Processed {
                    text: raw,
                    raw: None,
                    cleanup: CleanupStatus::Failed,
                    cleanup_error: Some(e.to_string()),
                })
            }
        }
    }
}

impl Inner {
    fn recently_used(&self) -> bool {
        self.last_used
            .lock()
            .ok()
            .and_then(|t| *t)
            .is_some_and(|t| t.elapsed() < WARM_WINDOW)
    }

    fn mark_used(&self) {
        if let Ok(mut t) = self.last_used.lock() {
            *t = Some(Instant::now());
        }
    }

    fn preload_blocking(&self) -> Result<Duration, RequestError> {
        let started = Instant::now();
        self.client.complete(
            self.prompt.messages(PRELOAD_TEXT),
            1,
            usize::MAX,
            &CancelFlag::default(),
        )?;
        self.mark_used();
        Ok(started.elapsed())
    }

    /// Request on a helper thread so cancellation is immediate even while
    /// the server is still loading the model; the helper notices the flag
    /// at its next chunk and closes the connection.
    fn clean(this: &Arc<Self>, raw: &str, cancel: &CancelFlag) -> Result<String, RequestError> {
        let chars = raw.chars().count();
        let max_tokens = u32::try_from(chars / 2 + 64).unwrap_or(u32::MAX);
        // Slightly above what sanitizing accepts, so runaway replies (poems,
        // answers) are cut off early instead of generated in full.
        let max_chars = chars * 3 / 2 + 60;
        let messages = this.prompt.messages(raw);

        let (tx, rx) = mpsc::channel();
        let worker = this.clone();
        let worker_cancel = cancel.clone();
        thread::Builder::new()
            .name("cleanup".into())
            .spawn(move || {
                let _ = tx.send(worker.client.complete(
                    messages,
                    max_tokens,
                    max_chars,
                    &worker_cancel,
                ));
            })
            .map_err(|e| RequestError::Other(format!("spawning worker: {e}")))?;
        let completion = loop {
            // (The helper ends by itself at the ureq global timeout.)
            match rx.recv_timeout(Duration::from_millis(50)) {
                Ok(result) => break result?,
                Err(mpsc::RecvTimeoutError::Timeout) if cancel.is_cancelled() => {
                    return Err(RequestError::Cancelled);
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(RequestError::Other("cleanup worker stopped".into()));
                }
            }
        };
        this.mark_used();

        let thinking = || {
            RequestError::Invalid(
                "the model is thinking; set cleanup.reasoning_effort = \"none\"".into(),
            )
        };
        if completion.finish_reason.as_deref() == Some("length") {
            return Err(if completion.reasoned {
                thinking()
            } else {
                RequestError::Invalid("reply was cut off".into())
            });
        }
        if completion.content.trim().is_empty() && completion.reasoned {
            return Err(thinking());
        }
        sanitize::sanitize(raw, &completion.content).map_err(RequestError::Invalid)
    }
}

#[cfg(test)]
mod tests {
    use super::client::tests::{config, http, serve_once, sse};
    use super::*;

    fn cleaner(url: &str) -> Cleaner {
        let mut cfg = config(url);
        cfg.preload = false;
        Cleaner::new(&cfg).unwrap()
    }

    const RAW: &str = "ähm also das ist ein Test";

    #[test]
    fn disabled_and_short_transcripts_pass_through() {
        let off = Cleaner::new(&CleanupConfig {
            enabled: false,
            ..CleanupConfig::default()
        })
        .unwrap();
        let p = off
            .run(RAW.into(), &CancelFlag::default(), || panic!("not started"))
            .unwrap();
        assert_eq!(p, Processed::uncleaned(RAW.into(), CleanupStatus::Disabled));

        let on = cleaner("http://127.0.0.1:9/v1");
        let p = on
            .run("Ja.".into(), &CancelFlag::default(), || {
                panic!("not started")
            })
            .unwrap();
        assert_eq!(p.cleanup, CleanupStatus::Skipped);
        assert_eq!(p.text, "Ja.");
    }

    #[test]
    fn cleaned_text_keeps_raw() {
        let (url, _server) = serve_once(sse(&["Das ist ", "ein Test."]));
        let mut started = false;
        let p = cleaner(&url)
            .run(RAW.into(), &CancelFlag::default(), || started = true)
            .unwrap();
        assert!(started);
        assert_eq!(p.cleanup, CleanupStatus::Cleaned);
        assert_eq!(p.text, "Das ist ein Test.");
        assert_eq!(p.raw_text(), RAW);
    }

    #[test]
    fn unchanged_text_has_no_separate_raw() {
        let (url, _server) = serve_once(sse(&[RAW]));
        let p = cleaner(&url)
            .run(RAW.into(), &CancelFlag::default(), || {})
            .unwrap();
        assert_eq!(p.cleanup, CleanupStatus::Cleaned);
        assert_eq!(p.raw, None);
    }

    #[test]
    fn failures_fall_back_to_raw() {
        // Server down.
        let port = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        };
        let p = cleaner(&format!("http://127.0.0.1:{port}/v1"))
            .run(RAW.into(), &CancelFlag::default(), || {})
            .unwrap();
        assert_eq!(p.cleanup, CleanupStatus::Failed);
        assert_eq!(p.text, RAW);
        assert!(p.cleanup_error.unwrap().contains("cannot reach"));

        // Refusal.
        let (url, _server) = serve_once(sse(&["I'm sorry, I can't help with that."]));
        let p = cleaner(&url)
            .run(RAW.into(), &CancelFlag::default(), || {})
            .unwrap();
        assert_eq!(p.cleanup, CleanupStatus::Failed);
        assert_eq!(p.text, RAW);

        // HTTP error.
        let (url, _server) = serve_once(http("500 Internal Server Error", "text/plain", "boom"));
        let p = cleaner(&url)
            .run(RAW.into(), &CancelFlag::default(), || {})
            .unwrap();
        assert_eq!(p.cleanup_error.as_deref(), Some("server replied 500: boom"));
    }

    #[test]
    fn thinking_models_get_a_hint() {
        let event = serde_json::json!({ "choices": [{ "delta": { "reasoning": "hmm" }, "finish_reason": "length" }] });
        let (url, _server) = serve_once(format!(
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\ndata: {event}\n\ndata: [DONE]\n\n"
        ));
        let p = cleaner(&url)
            .run(RAW.into(), &CancelFlag::default(), || {})
            .unwrap();
        assert!(p.cleanup_error.unwrap().contains("reasoning_effort"));
    }

    #[test]
    fn cancel_returns_promptly_while_server_is_silent() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/v1", listener.local_addr().unwrap());
        // Accept but never answer (like a server loading a model).
        let _server = thread::spawn(move || {
            let conn = listener.accept();
            thread::sleep(Duration::from_secs(3));
            drop(conn);
        });
        let cancel = CancelFlag::default();
        let c = cancel.clone();
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(100));
            c.cancel();
        });
        let started = Instant::now();
        let r = cleaner(&url).run(RAW.into(), &cancel, || {});
        assert_eq!(r, Err(Cancelled));
        assert!(started.elapsed() < Duration::from_secs(1));
    }
}

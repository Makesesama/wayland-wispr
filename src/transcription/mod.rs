//! Speech-to-text backends behind one blocking interface.
//!
//! [`Transcriber::transcribe`] blocks and must run on a worker thread. It takes
//! normalized mono 16 kHz audio and a [`CancelFlag`] that native inference
//! polls, so `cancel` stops work instead of only discarding its result.

#[cfg(feature = "local-whisper")]
pub mod model;
#[cfg(feature = "local-whisper")]
pub mod whisper;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::audio::Clip;
use crate::config::{Config, TranscriptionBackend};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transcript {
    pub text: String,
    /// Language code the backend transcribed in, if known.
    pub language: Option<String>,
}

/// Errors are user-facing: they end up in the overlay and `status`.
#[cfg_attr(not(feature = "local-whisper"), allow(dead_code))]
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TranscribeError {
    #[error("cancelled")]
    Cancelled,
    #[error("no speech recognized")]
    NoSpeech,
    #[cfg(feature = "local-whisper")]
    #[error("{0}")]
    Model(#[from] model::ModelError),
    #[error("loading the model failed: {0}")]
    Load(String),
    #[error("transcription failed: {0}")]
    Inference(String),
}

/// Shared cancellation flag for one transcription.
#[derive(Debug, Clone, Default)]
pub struct CancelFlag(Arc<AtomicBool>);

impl CancelFlag {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Relaxed);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }

    /// Stable pointer to the flag for native callbacks; valid while any
    /// clone of this flag is alive.
    #[cfg(feature = "local-whisper")]
    pub fn as_ptr(&self) -> *const AtomicBool {
        Arc::as_ptr(&self.0)
    }
}

/// Stage reports for the overlay.
#[cfg_attr(not(feature = "local-whisper"), allow(dead_code))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Progress {
    LoadingModel,
    Transcribing,
}

pub type ProgressFn = Box<dyn Fn(Progress) + Send>;

pub struct Transcriber {
    backend: Backend,
}

enum Backend {
    Fake {
        delay: Duration,
        text: Option<String>,
    },
    #[cfg(feature = "local-whisper")]
    Whisper(whisper::Engine),
}

impl Transcriber {
    /// Validate settings and the model file, and start backend workers.
    /// Does not load the model (see [`Transcriber::prepare`]).
    pub fn new(config: &Config) -> anyhow::Result<Self> {
        let backend = match config.transcription.backend {
            TranscriptionBackend::Fake => {
                tracing::warn!("transcription backend is `fake`: results are placeholder text");
                Backend::Fake {
                    delay: config.fake_processing(),
                    text: config.debug.fake_transcript.clone(),
                }
            }
            #[cfg(feature = "local-whisper")]
            TranscriptionBackend::Whisper => {
                Backend::Whisper(whisper::Engine::start(&config.transcription.whisper)?)
            }
            #[cfg(not(feature = "local-whisper"))]
            TranscriptionBackend::Whisper => anyhow::bail!(
                "transcription.backend = \"whisper\" but this build lacks the `local-whisper` feature"
            ),
        };
        Ok(Self { backend })
    }

    /// Hint that a transcription is coming (recording started): load the
    /// model in the background if configured to.
    pub fn prepare(&self) {
        match &self.backend {
            Backend::Fake { .. } => {}
            #[cfg(feature = "local-whisper")]
            Backend::Whisper(engine) => engine.prepare(),
        }
    }

    pub fn transcribe(
        &self,
        clip: &Clip,
        cancel: &CancelFlag,
        progress: ProgressFn,
    ) -> Result<Transcript, TranscribeError> {
        match &self.backend {
            Backend::Fake { delay, text } => {
                progress(Progress::Transcribing);
                let until = Instant::now() + *delay;
                while Instant::now() < until {
                    if cancel.is_cancelled() {
                        return Err(TranscribeError::Cancelled);
                    }
                    std::thread::sleep(Duration::from_millis(20).min(*delay));
                }
                Ok(Transcript {
                    text: text.clone().unwrap_or_else(|| {
                        format!(
                            "Placeholder transcript: recorded {:.1} s of audio (fake backend).",
                            clip.duration_secs()
                        )
                    }),
                    language: None,
                })
            }
            #[cfg(feature = "local-whisper")]
            Backend::Whisper(engine) => engine.transcribe(clip, cancel, progress),
        }
    }
}

/// Join segment texts into one clean string: trimmed, single-spaced lines.
#[cfg_attr(not(feature = "local-whisper"), allow(dead_code))]
pub fn join_segments<'a>(segments: impl IntoIterator<Item = &'a str>) -> String {
    let mut out = String::new();
    for seg in segments {
        let seg = seg.split_whitespace().collect::<Vec<_>>().join(" ");
        if seg.is_empty() {
            continue;
        }
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(&seg);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake(delay_ms: u64) -> Transcriber {
        let mut config = Config::default();
        config.transcription.backend = TranscriptionBackend::Fake;
        config.debug.fake_processing_ms = delay_ms;
        config.debug.fake_transcript = Some("hi".into());
        Transcriber::new(&config).unwrap()
    }

    fn clip() -> Clip {
        Clip {
            samples: vec![0.0; 16_000],
            sample_rate: 16_000,
        }
    }

    #[test]
    fn fake_backend_returns_text() {
        let t = fake(0);
        let r = t.transcribe(&clip(), &CancelFlag::default(), Box::new(|_| {}));
        assert_eq!(r.unwrap().text, "hi");
    }

    #[test]
    fn fake_backend_honours_cancel() {
        let t = fake(10_000);
        let cancel = CancelFlag::default();
        cancel.cancel();
        let started = Instant::now();
        let r = t.transcribe(&clip(), &cancel, Box::new(|_| {}));
        assert_eq!(r, Err(TranscribeError::Cancelled));
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn joins_segments() {
        assert_eq!(
            join_segments([" Hallo Welt.", "  ", " Zweiter\n Satz. "]),
            "Hallo Welt. Zweiter Satz."
        );
        assert_eq!(join_segments([]), "");
    }
}

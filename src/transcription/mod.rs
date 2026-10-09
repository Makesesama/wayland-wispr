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

/// Subtitle credits Whisper hallucinates from its training data, typically
/// during trailing silence. Distinctive enough to match anywhere in the
/// sentence (case-insensitive).
const HALLUCINATION_MARKERS: &[&str] = &[
    "amara.org",
    "untertitel im auftrag des",
    "untertitelung im auftrag des",
    "copyright wdr",
];

/// Sign-offs that are also ordinary speech; only removed when they are the
/// whole sentence (lowercase, punctuation ignored).
const HALLUCINATION_SENTENCES: &[&str] = &[
    "thanks for watching",
    "thank you for watching",
    "thank you so much for watching",
    "danke fürs zuschauen",
    "vielen dank fürs zuschauen",
];

/// Remove trailing sentences that are known Whisper hallucinations. Only
/// the end of the text is checked: these phrases appear where the speech
/// stopped, and genuine uses mid-text are left alone.
pub fn strip_hallucinations(text: &str) -> String {
    let mut rest = text.trim();
    while !rest.is_empty() {
        let start = last_sentence_start(rest);
        let sentence = rest[start..].to_lowercase();
        let words = sentence
            .split(|c: char| !c.is_alphanumeric())
            .filter(|w| !w.is_empty())
            .collect::<Vec<_>>()
            .join(" ");
        let hallucinated = HALLUCINATION_MARKERS.iter().any(|h| sentence.contains(h))
            || HALLUCINATION_SENTENCES.contains(&words.as_str());
        if !hallucinated {
            break;
        }
        rest = rest[..start].trim_end();
    }
    rest.to_owned()
}

/// Byte index where the last sentence of `s` starts: after the last `.`,
/// `!`, `?`, `…`, or newline that is followed by whitespace (so `Amara.org`
/// is one word), ignoring the final punctuation.
fn last_sentence_start(s: &str) -> usize {
    let body = s.trim_end_matches(['.', '!', '?', '…', ' ']);
    let mut start = 0;
    let mut chars = body.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        let boundary = c == '\n'
            || (matches!(c, '.' | '!' | '?' | '…')
                && chars.peek().is_some_and(|(_, next)| next.is_whitespace()));
        if boundary {
            start = i + c.len_utf8();
        }
    }
    start
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_trailing_hallucinations() {
        assert_eq!(
            strip_hallucinations("Hallo zusammen. Untertitel der Amara.org-Community"),
            "Hallo zusammen."
        );
        assert_eq!(
            strip_hallucinations(
                "Bis morgen! Untertitel im Auftrag des ZDF, 2017. Untertitel im Auftrag des ZDF, 2017"
            ),
            "Bis morgen!"
        );
        assert_eq!(strip_hallucinations("Thanks for watching!"), "");
        assert_eq!(
            strip_hallucinations("Bis dann. Vielen Dank fürs Zuschauen."),
            "Bis dann."
        );
        // Ordinary speech containing the phrases survives, also as the only
        // or the last sentence.
        for real in [
            "Thanks for watching the kids.",
            "I said thanks for watching the kids. See you.",
            "We need the subtitles by Friday.",
            "Kannst du die Untertitel der Präsentation prüfen?",
            "Danke fürs Zuschauen bei meinem Vortrag gestern.",
        ] {
            assert_eq!(strip_hallucinations(real), real);
        }
        assert_eq!(strip_hallucinations("  Ganz normal.  "), "Ganz normal.");
        assert_eq!(strip_hallucinations(""), "");
    }

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

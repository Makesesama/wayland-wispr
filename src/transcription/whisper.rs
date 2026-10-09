//! Local whisper.cpp inference via `whisper-rs`.
//!
//! A dedicated worker thread owns the model so loading and inference never
//! block the controller or GTK, jobs are serialized, and the model can be
//! loaded ahead of time (when recording starts) and unloaded when idle to
//! give (V)RAM back to other applications such as a local LLM.

use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use whisper_rs::{
    FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters, WhisperState,
};

use super::model::{self, ModelFile};
use super::{CancelFlag, Progress, ProgressFn, TranscribeError, Transcript, join_segments};
use crate::audio::Clip;
use crate::audio::convert::TARGET_RATE;
use crate::config::{KeepLoaded, WhisperConfig, WhisperDevice};

/// Whether this binary was built with a GPU backend.
pub const GPU_BUILD: bool = cfg!(feature = "whisper-vulkan");

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Language {
    Fixed(String),
    Auto,
    /// Detect, but only choose among these codes.
    AutoAmong(Vec<String>),
}

/// Validated settings derived from [`WhisperConfig`].
#[derive(Debug, Clone)]
pub struct Settings {
    pub model: ModelFile,
    pub use_gpu: bool,
    pub gpu_device: u32,
    pub flash_attn: bool,
    pub threads: u16,
    pub language: Language,
    pub initial_prompt: Option<String>,
    pub beam_size: u8,
    pub keep_loaded: KeepLoaded,
    pub idle_unload: Duration,
    pub preload: bool,
}

impl Settings {
    pub fn from_config(config: &WhisperConfig) -> anyhow::Result<Self> {
        let use_gpu = config.device == WhisperDevice::Gpu;
        if use_gpu && !GPU_BUILD {
            bail!(
                "transcription.whisper.device = \"gpu\" but this binary has no GPU backend; \
                 rebuild with `--features whisper-vulkan` or set device = \"cpu\""
            );
        }
        let check = |code: &str| -> anyhow::Result<String> {
            let code = code.trim().to_lowercase();
            if whisper_rs::get_lang_id(&code).is_none() {
                bail!("unknown whisper language code {code:?}");
            }
            Ok(code)
        };
        let language = match config.language.trim() {
            "auto" if config.languages.len() == 1 => Language::Fixed(check(&config.languages[0])?),
            "auto" if config.languages.is_empty() => Language::Auto,
            "auto" => Language::AutoAmong(
                config
                    .languages
                    .iter()
                    .map(|c| check(c))
                    .collect::<anyhow::Result<_>>()?,
            ),
            code => Language::Fixed(check(code)?),
        };
        let initial_prompt = config
            .initial_prompt
            .as_deref()
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .map(str::to_owned);
        if initial_prompt.as_ref().is_some_and(|p| p.contains('\0')) {
            bail!("transcription.whisper.initial_prompt must not contain NUL bytes");
        }
        Ok(Self {
            model: model::locate(config)?,
            use_gpu,
            gpu_device: config.gpu_device,
            flash_attn: config.flash_attn,
            threads: config.threads(),
            language,
            initial_prompt,
            beam_size: config.beam_size,
            keep_loaded: config.keep_loaded,
            idle_unload: Duration::from_secs(config.idle_unload_secs),
            preload: config.preload,
        })
    }

    pub fn device_label(&self) -> String {
        if self.use_gpu {
            format!("gpu{}", self.gpu_device)
        } else {
            "cpu".into()
        }
    }
}

/// A loaded model plus its reusable inference state (KV cache and compute
/// buffers live in the state, so keeping it avoids re-allocating per job).
pub struct Model {
    // `state` keeps the context alive internally; field order is irrelevant.
    _ctx: WhisperContext,
    state: WhisperState,
    multilingual: bool,
}

impl Model {
    pub fn load(settings: &Settings) -> Result<Self, TranscribeError> {
        if settings.use_gpu {
            check_gpu(settings.gpu_device)?;
        }
        // Re-validate: the file may have changed since startup.
        model::validate(&settings.model.path)?;

        let mut params = WhisperContextParameters::default();
        params
            .use_gpu(settings.use_gpu)
            .flash_attn(settings.flash_attn)
            .gpu_device(settings.gpu_device as i32);
        let ctx = WhisperContext::new_with_params(&settings.model.path, params).map_err(|e| {
            TranscribeError::Load(format!(
                "{} could not be loaded on {} ({e}); out of memory or incompatible model? \
                 Try a smaller model or device = \"cpu\"",
                settings.model.path.display(),
                settings.device_label()
            ))
        })?;
        let state = ctx.create_state().map_err(|e| {
            TranscribeError::Load(format!(
                "allocating inference buffers on {} failed ({e}); out of memory? \
                 Try a smaller model or device = \"cpu\"",
                settings.device_label()
            ))
        })?;
        let multilingual = ctx.is_multilingual();
        if !multilingual && !matches!(&settings.language, Language::Fixed(l) if l == "en") {
            tracing::warn!("model is English-only (.en); language settings are ignored");
        }
        Ok(Self {
            _ctx: ctx,
            state,
            multilingual,
        })
    }

    /// Blocking inference on mono 16 kHz samples.
    pub fn transcribe(
        &mut self,
        settings: &Settings,
        samples: &[f32],
        cancel: &CancelFlag,
    ) -> Result<Transcript, TranscribeError> {
        if samples.is_empty() {
            return Err(TranscribeError::NoSpeech);
        }
        let threads = i32::from(settings.threads.max(1));
        let language = if !self.multilingual {
            "en".to_owned()
        } else {
            match &settings.language {
                Language::Fixed(code) => code.clone(),
                Language::Auto => "auto".to_owned(),
                Language::AutoAmong(codes) => self.detect_among(codes, samples, threads)?,
            }
        };
        if cancel.is_cancelled() {
            return Err(TranscribeError::Cancelled);
        }

        let strategy = if settings.beam_size > 1 {
            SamplingStrategy::BeamSearch {
                beam_size: i32::from(settings.beam_size),
                patience: -1.0,
            }
        } else {
            SamplingStrategy::Greedy { best_of: 1 }
        };
        let mut params = FullParams::new(strategy);
        params.set_n_threads(threads);
        params.set_language(Some(&language));
        params.set_translate(false);
        params.set_no_timestamps(true);
        params.set_print_special(false);
        params.set_print_progress(false);
        params.set_print_realtime(false);
        params.set_print_timestamps(false);
        // Drop non-speech tokens such as "[Music]" / "(Applaus)".
        params.set_suppress_nst(true);
        if let Some(prompt) = &settings.initial_prompt {
            params.set_initial_prompt(prompt);
        }
        let abort = cancel.clone();
        // whisper-rs 0.16.0 bug: `set_abort_callback_safe` stores a
        // `Box<dyn FnMut() -> bool>` but its trampoline casts the pointer to
        // `F`. Passing `F = Box<dyn FnMut() -> bool>` makes that cast correct;
        // a plain closure would be read from the wrong memory and abort
        // inference at random. (The box is leaked by whisper-rs: 16 bytes.)
        let abort: Box<dyn FnMut() -> bool> = Box::new(move || abort.is_cancelled());
        params.set_abort_callback_safe(abort);
        // The abort callback is only polled between graph runs, never inside
        // the (long, on CPU) encoder pass; also stop before each encoder pass.
        // SAFETY: the pointer targets the AtomicBool inside `cancel`'s Arc,
        // which outlives the `full()` call below.
        unsafe {
            params.set_start_encoder_callback(Some(continue_unless_cancelled));
            params.set_start_encoder_callback_user_data(cancel.as_ptr().cast_mut().cast());
        }

        let result = self.state.full(params, samples);
        if cancel.is_cancelled() {
            return Err(TranscribeError::Cancelled);
        }
        result.map_err(|e| TranscribeError::Inference(e.to_string()))?;

        let segments: Vec<String> = self
            .state
            .as_iter()
            .filter_map(|seg| seg.to_str_lossy().ok().map(|s| s.into_owned()))
            .collect();
        let text = join_segments(segments.iter().map(String::as_str));
        if text.is_empty() {
            return Err(TranscribeError::NoSpeech);
        }
        let detected = whisper_rs::get_lang_str(self.state.full_lang_id_from_state())
            .map(str::to_owned)
            .or_else(|| (language != "auto").then_some(language));
        Ok(Transcript {
            text,
            language: detected,
        })
    }

    /// Language detection restricted to `codes`: one extra encoder pass on
    /// the first 30 s, then the most probable allowed language wins.
    fn detect_among(
        &mut self,
        codes: &[String],
        samples: &[f32],
        threads: i32,
    ) -> Result<String, TranscribeError> {
        let threads = threads as usize;
        let detect = |e: whisper_rs::WhisperError| {
            TranscribeError::Inference(format!("language detection: {e}"))
        };
        self.state.pcm_to_mel(samples, threads).map_err(detect)?;
        let (_, probs) = self.state.lang_detect(0, threads).map_err(detect)?;
        let best = codes
            .iter()
            .filter_map(|code| {
                let id = whisper_rs::get_lang_id(code)?;
                Some((code, probs.get(usize::try_from(id).ok()?).copied()?))
            })
            .max_by(|a, b| a.1.total_cmp(&b.1));
        let (code, p) = best.ok_or_else(|| {
            TranscribeError::Inference("language detection returned no probabilities".into())
        })?;
        tracing::debug!(language = %code, probability = p, "restricted language detection");
        Ok(code.clone())
    }
}

unsafe extern "C" fn continue_unless_cancelled(
    _ctx: *mut whisper_rs::whisper_rs_sys::whisper_context,
    _state: *mut whisper_rs::whisper_rs_sys::whisper_state,
    user_data: *mut std::ffi::c_void,
) -> bool {
    // SAFETY: set to `CancelFlag::as_ptr()` for the duration of `full()`.
    let cancelled = unsafe { &*user_data.cast::<std::sync::atomic::AtomicBool>() };
    !cancelled.load(std::sync::atomic::Ordering::Relaxed)
}

/// A GPU as whisper.cpp indexes it for `gpu_device`.
#[derive(Debug, Clone)]
pub struct GpuDevice {
    pub index: u32,
    pub name: String,
    pub description: String,
    pub free: usize,
    pub total: usize,
}

/// GPUs visible to the compiled ggml backends (none in CPU-only builds).
///
/// Uses the generic ggml backend registry, which catches backend init
/// failures (e.g. no Vulkan driver), unlike `whisper_rs::vulkan`, whose C++
/// exceptions would abort the process. Indices match whisper.cpp's
/// `whisper_backend_init_gpu`: GPU and iGPU devices in registry order.
pub fn gpu_devices() -> Vec<GpuDevice> {
    use std::ffi::CStr;
    use whisper_rs::whisper_rs_sys as sys;

    let text = |p: *const std::os::raw::c_char| {
        if p.is_null() {
            String::new()
        } else {
            // SAFETY: ggml returns NUL-terminated strings owned by the device.
            unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
        }
    };
    let mut out = Vec::new();
    // SAFETY: plain queries on ggml's process-wide, lazily initialized
    // backend registry; device handles stay valid for the process lifetime.
    unsafe {
        for i in 0..sys::ggml_backend_dev_count() {
            let dev = sys::ggml_backend_dev_get(i);
            if dev.is_null() {
                continue;
            }
            let kind = sys::ggml_backend_dev_type(dev);
            if kind != sys::ggml_backend_dev_type_GGML_BACKEND_DEVICE_TYPE_GPU
                && kind != sys::ggml_backend_dev_type_GGML_BACKEND_DEVICE_TYPE_IGPU
            {
                continue;
            }
            let (mut free, mut total) = (0usize, 0usize);
            sys::ggml_backend_dev_memory(dev, &mut free, &mut total);
            out.push(GpuDevice {
                index: out.len() as u32,
                name: text(sys::ggml_backend_dev_name(dev)),
                description: text(sys::ggml_backend_dev_description(dev)),
                free,
                total,
            });
        }
    }
    out
}

/// whisper.cpp silently uses the CPU when the requested GPU is missing;
/// make that an error instead.
fn check_gpu(index: u32) -> Result<(), TranscribeError> {
    let devices = gpu_devices();
    let Some(dev) = devices.iter().find(|d| d.index == index) else {
        let names: Vec<_> = devices
            .iter()
            .map(|d| format!("{}: {}", d.index, d.description))
            .collect();
        return Err(TranscribeError::Load(format!(
            "GPU {index} not found (available: {}); is a Vulkan driver installed?",
            if names.is_empty() {
                "none".into()
            } else {
                names.join(", ")
            }
        )));
    };
    tracing::info!(
        gpu = index,
        backend = %dev.name,
        name = %dev.description,
        vram_free_mib = dev.free / (1 << 20),
        vram_total_mib = dev.total / (1 << 20),
        "using GPU"
    );
    Ok(())
}

/// Route whisper.cpp/ggml logging into `tracing` (targets `whisper_rs::*`).
pub fn install_logging() {
    whisper_rs::install_logging_hooks();
}

enum Job {
    Preload,
    Transcribe {
        samples: Vec<f32>,
        cancel: CancelFlag,
        progress: ProgressFn,
        reply: mpsc::SyncSender<Result<Transcript, TranscribeError>>,
    },
}

/// Handle to the whisper worker thread.
pub struct Engine {
    tx: mpsc::Sender<Job>,
    preload: bool,
}

impl Engine {
    pub fn start(config: &WhisperConfig) -> anyhow::Result<Self> {
        install_logging();
        let settings = Settings::from_config(config).context("local whisper transcription")?;
        tracing::info!(
            model = %settings.model.path.display(),
            size_mib = settings.model.bytes / (1 << 20),
            device = %settings.device_label(),
            language = ?settings.language,
            beam_size = settings.beam_size,
            threads = settings.threads,
            keep_loaded = ?settings.keep_loaded,
            "whisper backend ready (model loads on first use)"
        );
        let preload = settings.preload;
        let (tx, rx) = mpsc::channel();
        thread::Builder::new()
            .name("whisper".into())
            .spawn(move || {
                Worker {
                    settings,
                    model: None,
                    last_used: Instant::now(),
                }
                .run(rx)
            })
            .context("spawning whisper worker")?;
        Ok(Self { tx, preload })
    }

    pub fn prepare(&self) {
        if self.preload {
            let _ = self.tx.send(Job::Preload);
        }
    }

    pub fn transcribe(
        &self,
        clip: &Clip,
        cancel: &CancelFlag,
        progress: ProgressFn,
    ) -> Result<Transcript, TranscribeError> {
        if clip.sample_rate != TARGET_RATE {
            return Err(TranscribeError::Inference(format!(
                "expected {TARGET_RATE} Hz audio, got {} Hz",
                clip.sample_rate
            )));
        }
        let (reply, result) = mpsc::sync_channel(1);
        self.tx
            .send(Job::Transcribe {
                samples: clip.samples.clone(),
                cancel: cancel.clone(),
                progress,
                reply,
            })
            .map_err(|_| TranscribeError::Inference("whisper worker stopped".into()))?;
        result
            .recv()
            .map_err(|_| TranscribeError::Inference("whisper worker stopped".into()))?
    }
}

struct Worker {
    settings: Settings,
    model: Option<Model>,
    last_used: Instant,
}

impl Worker {
    fn run(mut self, rx: mpsc::Receiver<Job>) {
        loop {
            let job = if self.model.is_some() && self.settings.keep_loaded != KeepLoaded::Always {
                // `never` only gets here after a preload that was not used.
                let idle = self.settings.idle_unload;
                let wait = (self.last_used + idle).saturating_duration_since(Instant::now());
                match rx.recv_timeout(wait) {
                    Ok(job) => job,
                    Err(RecvTimeoutError::Timeout) => {
                        self.unload("idle");
                        continue;
                    }
                    Err(RecvTimeoutError::Disconnected) => break,
                }
            } else {
                match rx.recv() {
                    Ok(job) => job,
                    Err(_) => break,
                }
            };

            match job {
                Job::Preload => {
                    if let Err(e) = self.ensure_loaded() {
                        tracing::warn!(error = %e, "preloading whisper model failed");
                    }
                    self.last_used = Instant::now();
                }
                Job::Transcribe {
                    samples,
                    cancel,
                    progress,
                    reply,
                } => {
                    let result = self.transcribe(&samples, &cancel, &progress);
                    self.last_used = Instant::now();
                    if self.settings.keep_loaded == KeepLoaded::Never {
                        self.unload("keep_loaded = never");
                    }
                    let _ = reply.send(result);
                }
            }
        }
        tracing::debug!("whisper worker stopped");
    }

    fn transcribe(
        &mut self,
        samples: &[f32],
        cancel: &CancelFlag,
        progress: &ProgressFn,
    ) -> Result<Transcript, TranscribeError> {
        if cancel.is_cancelled() {
            return Err(TranscribeError::Cancelled);
        }
        if self.model.is_none() {
            progress(Progress::LoadingModel);
        }
        self.ensure_loaded()?;
        // Loading cannot be interrupted; honour a cancel that arrived meanwhile.
        if cancel.is_cancelled() {
            return Err(TranscribeError::Cancelled);
        }
        progress(Progress::Transcribing);
        let started = Instant::now();
        let model = self.model.as_mut().expect("loaded above");
        let result = model.transcribe(&self.settings, samples, cancel);
        let elapsed = started.elapsed();
        let audio_secs = samples.len() as f32 / TARGET_RATE as f32;
        match &result {
            Ok(t) => tracing::info!(
                ms = elapsed.as_millis() as u64,
                audio_secs,
                rtf = elapsed.as_secs_f32() / audio_secs.max(0.01),
                language = t.language.as_deref().unwrap_or("?"),
                chars = t.text.chars().count(),
                "transcribed"
            ),
            Err(e) => {
                tracing::info!(ms = elapsed.as_millis() as u64, error = %e, "transcription ended")
            }
        }
        result
    }

    fn ensure_loaded(&mut self) -> Result<(), TranscribeError> {
        if self.model.is_some() {
            return Ok(());
        }
        let started = Instant::now();
        let model = Model::load(&self.settings)?;
        tracing::info!(
            ms = started.elapsed().as_millis() as u64,
            device = %self.settings.device_label(),
            "whisper model loaded"
        );
        self.model = Some(model);
        Ok(())
    }

    fn unload(&mut self, reason: &str) {
        if self.model.take().is_some() {
            tracing::info!(reason, "whisper model unloaded");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Real-model tests are opt-in: `WAYLAND_WISPR_TEST_MODEL=/path/ggml-*.bin cargo test`.
    fn test_model() -> Option<WhisperConfig> {
        let path = std::env::var_os("WAYLAND_WISPR_TEST_MODEL")?;
        Some(WhisperConfig {
            model_path: Some(path.into()),
            threads: Some(2),
            keep_loaded: KeepLoaded::Always,
            ..WhisperConfig::default()
        })
    }

    fn noise(secs: usize) -> Clip {
        // Deterministic pseudo-noise so the decoder has something to chew on.
        let mut x = 1u32;
        let samples = (0..secs * TARGET_RATE as usize)
            .map(|_| {
                x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                (x >> 8) as f32 / (1 << 24) as f32 * 0.2 - 0.1
            })
            .collect();
        Clip {
            samples,
            sample_rate: TARGET_RATE,
        }
    }

    #[test]
    fn rejects_gpu_without_gpu_build_and_bad_languages() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("m.bin"), b"lmgg").unwrap();
        let base = WhisperConfig {
            model_path: Some(dir.path().join("m.bin")),
            ..WhisperConfig::default()
        };
        let gpu = WhisperConfig {
            device: WhisperDevice::Gpu,
            ..base.clone()
        };
        assert_eq!(Settings::from_config(&gpu).is_ok(), GPU_BUILD);

        let bad = WhisperConfig {
            language: "klingon".into(),
            ..base.clone()
        };
        assert!(Settings::from_config(&bad).is_err());

        let among = WhisperConfig {
            languages: vec!["DE".into(), "en".into()],
            ..base.clone()
        };
        assert_eq!(
            Settings::from_config(&among).unwrap().language,
            Language::AutoAmong(vec!["de".into(), "en".into()])
        );
        let single = WhisperConfig {
            languages: vec!["de".into()],
            ..base
        };
        assert_eq!(
            Settings::from_config(&single).unwrap().language,
            Language::Fixed("de".into())
        );
    }

    #[test]
    fn cancel_aborts_running_inference() {
        let Some(config) = test_model() else {
            eprintln!("skipped: WAYLAND_WISPR_TEST_MODEL not set");
            return;
        };
        let engine = Engine::start(&config).unwrap();
        let clip = noise(30);

        // Warm up (load) so the timing below measures inference only.
        let warm = engine.transcribe(&noise(1), &CancelFlag::default(), Box::new(|_| {}));
        assert!(!matches!(warm, Err(TranscribeError::Cancelled)), "{warm:?}");

        let uncancelled = Instant::now();
        let _ = engine.transcribe(&clip, &CancelFlag::default(), Box::new(|_| {}));
        let full = uncancelled.elapsed();

        let cancel = CancelFlag::default();
        let canceller = {
            let cancel = cancel.clone();
            thread::spawn(move || {
                thread::sleep(full / 10);
                cancel.cancel();
            })
        };
        let started = Instant::now();
        let result = engine.transcribe(&clip, &cancel, Box::new(|_| {}));
        canceller.join().unwrap();
        assert_eq!(result, Err(TranscribeError::Cancelled));
        // whisper.cpp cannot interrupt a running encoder pass, so the bound is
        // "noticeably before the end", not "immediately". With auto language
        // a 30 s clip needs two encoder passes; the second must not start.
        assert!(
            started.elapsed() < full * 3 / 4,
            "abort took {:?} (full run {full:?})",
            started.elapsed()
        );
    }
}

//! Typed settings loaded from `$XDG_CONFIG_HOME/wayland-wispr/config.toml`.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("could not read config {path}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("invalid config {path}")]
    Parse {
        path: PathBuf,
        source: toml::de::Error,
    },
    #[error("invalid config value: {0}")]
    Invalid(String),
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub overlay: OverlayConfig,
    pub recording: RecordingConfig,
    pub audio: AudioConfig,
    pub transcription: TranscriptionConfig,
    pub cleanup: CleanupConfig,
    pub injection: InjectionConfig,
    pub debug: DebugConfig,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct OverlayConfig {
    /// Layer-shell layer for the indicator.
    pub layer: OverlayLayer,
    /// Distance from the bottom screen edge in logical pixels.
    pub margin_bottom: i32,
}

impl Default for OverlayConfig {
    fn default() -> Self {
        Self {
            layer: OverlayLayer::Overlay,
            margin_bottom: 48,
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum OverlayLayer {
    Overlay,
    Top,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct RecordingConfig {
    /// Recording is stopped automatically after this many seconds.
    pub max_seconds: u64,
}

impl Default for RecordingConfig {
    fn default() -> Self {
        Self { max_seconds: 120 }
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct AudioConfig {
    /// cpal host: `auto` (PipeWire if available, else ALSA), `pipewire`, `alsa`.
    pub host: String,
    /// Input device name or ID from `wayland-wispr devices`; default device if unset.
    pub device: Option<String>,
    /// Recordings shorter than this are rejected.
    pub min_seconds: f32,
    /// Recordings whose loudest 30 ms window is below this are rejected as silent.
    pub silence_threshold_dbfs: f32,
}

impl Default for AudioConfig {
    fn default() -> Self {
        Self {
            host: "auto".into(),
            device: None,
            min_seconds: 0.3,
            silence_threshold_dbfs: -50.0,
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct TranscriptionConfig {
    pub backend: TranscriptionBackend,
    pub whisper: WhisperConfig,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum TranscriptionBackend {
    /// Local whisper.cpp inference (`local-whisper` build feature).
    #[default]
    Whisper,
    /// Placeholder text after `debug.fake_processing_ms`; for UI/insertion tests.
    Fake,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct WhisperConfig {
    /// Model name, resolved to `<models_dir>/ggml-<model>.bin`,
    /// e.g. `large-v3-turbo-q5_0`, `small-q5_1`, `base-q5_1`.
    pub model: String,
    /// Explicit model file; overrides `model` / `models_dir`.
    pub model_path: Option<PathBuf>,
    /// Directory for named models. Default: `$XDG_DATA_HOME/wayland-wispr/models`.
    /// Relative paths are resolved against the config file's directory.
    pub models_dir: Option<PathBuf>,
    /// `cpu`, or `gpu` (requires a GPU build feature such as `whisper-vulkan`).
    /// Never falls back silently: a missing GPU is an error.
    pub device: WhisperDevice,
    /// GPU index as listed by `wayland-wispr bench`.
    pub gpu_device: u32,
    pub flash_attn: bool,
    /// CPU threads; default: available cores, at most 8.
    pub threads: Option<u16>,
    /// Spoken language code (`de`, `en`, …) or `auto`.
    pub language: String,
    /// With `language = "auto"`: only pick among these (e.g. `["de", "en"]`).
    /// Empty: any language Whisper knows.
    pub languages: Vec<String>,
    /// Vocabulary/style hint, e.g. names and jargon you use often.
    pub initial_prompt: Option<String>,
    /// 1 = greedy decoding (fastest); >1 = beam search with this width.
    pub beam_size: u8,
    /// When the model stays in (V)RAM between recordings.
    pub keep_loaded: KeepLoaded,
    /// With `keep_loaded = "idle"`: unload after this many seconds unused.
    pub idle_unload_secs: u64,
    /// Start loading the model as soon as recording starts.
    pub preload: bool,
}

impl Default for WhisperConfig {
    fn default() -> Self {
        Self {
            model: "base-q5_1".into(),
            model_path: None,
            models_dir: None,
            device: WhisperDevice::Cpu,
            gpu_device: 0,
            flash_attn: true,
            threads: None,
            language: "auto".into(),
            languages: Vec::new(),
            initial_prompt: None,
            beam_size: 1,
            keep_loaded: KeepLoaded::Idle,
            idle_unload_secs: 300,
            preload: true,
        }
    }
}

#[cfg_attr(not(feature = "local-whisper"), allow(dead_code))]
impl WhisperConfig {
    /// The model file to load.
    pub fn resolved_model_path(&self) -> Option<PathBuf> {
        if let Some(path) = &self.model_path {
            return Some(path.clone());
        }
        let dir = self.models_dir.clone().or_else(default_models_dir)?;
        let file = if self.model.ends_with(".bin") {
            self.model.clone()
        } else {
            format!("ggml-{}.bin", self.model)
        };
        Some(dir.join(file))
    }

    pub fn threads(&self) -> u16 {
        self.threads.unwrap_or_else(|| {
            std::thread::available_parallelism()
                .map(|n| n.get().min(8) as u16)
                .unwrap_or(4)
        })
    }
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum WhisperDevice {
    Cpu,
    Gpu,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum KeepLoaded {
    /// Load once, never unload.
    Always,
    /// Unload after `idle_unload_secs` without use.
    Idle,
    /// Unload right after each transcription (frees VRAM, slower start).
    Never,
}

/// LLM cleanup of the raw transcript through an OpenAI-compatible chat
/// completions endpoint (Ollama, llama-server, a remote provider, …). The
/// server is run by the user; if it is unreachable, the raw transcript is used.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct CleanupConfig {
    pub enabled: bool,
    /// API base including the version path; `/chat/completions` is appended.
    pub base_url: String,
    /// Model name as the server knows it, e.g. `qwen2.5:7b-instruct` (Ollama).
    pub model: String,
    /// Environment variable holding a bearer token; unset = no auth header.
    pub api_key_env: Option<String>,
    /// Sent as `reasoning_effort`; `"none"` disables thinking on Qwen3.x and
    /// other reasoning models (Ollama and llama-server understand it).
    pub reasoning_effort: Option<String>,
    pub temperature: f32,
    pub connect_timeout_ms: u64,
    /// Deadline for one cleanup request, including a cold model load.
    pub timeout_secs: u64,
    /// Transcripts with fewer words are used as-is.
    pub min_words: usize,
    /// Send a tiny request when recording starts so the server loads the
    /// model (and caches the prompt) while the user speaks.
    pub preload: bool,
    /// TOML file replacing the built-in system prompt and examples.
    /// Relative paths are resolved against the config file's directory.
    pub prompt_file: Option<PathBuf>,
    /// Allow plain `http://` to non-loopback hosts (transcripts are sent
    /// unencrypted).
    pub allow_insecure_http: bool,
}

impl Default for CleanupConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            base_url: "http://127.0.0.1:11434/v1".into(),
            model: "qwen2.5:7b-instruct".into(),
            api_key_env: None,
            reasoning_effort: None,
            temperature: 0.0,
            connect_timeout_ms: 1000,
            timeout_secs: 30,
            min_words: 4,
            preload: true,
            prompt_file: None,
            allow_insecure_http: false,
        }
    }
}

/// Scheme and host of an `http(s)://host[:port]/path` URL.
pub fn split_url(url: &str) -> Option<(&str, &str)> {
    let (scheme, rest) = url.split_once("://")?;
    if !matches!(scheme, "http" | "https") {
        return None;
    }
    let authority = rest.split('/').next()?;
    if authority.contains(['@', '?', '#']) || authority.chars().any(char::is_whitespace) {
        return None;
    }
    let host = if let Some(v6) = authority.strip_prefix('[') {
        v6.split(']').next()?
    } else {
        authority.split(':').next()?
    };
    (!host.is_empty()).then_some((scheme, host))
}

pub fn is_loopback_host(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct InjectionConfig {
    /// Chord sent after copying, e.g. `ctrl+v`, `ctrl+shift+v`, `shift+Insert`.
    pub paste_chord: String,
    /// Delay before the paste chord, so the triggering keybinding is released.
    pub pre_insert_delay_ms: u64,
    /// Delay between synthetic key events.
    pub key_delay_ms: u64,
}

impl Default for InjectionConfig {
    fn default() -> Self {
        Self {
            paste_chord: "ctrl+v".into(),
            pre_insert_delay_ms: 150,
            key_delay_ms: 10,
        }
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct DebugConfig {
    /// Duration of the `fake` transcription backend.
    pub fake_processing_ms: u64,
    /// Result text of the `fake` backend, e.g. to test Unicode/multiline insertion.
    pub fake_transcript: Option<String>,
    /// If set, every normalized (16 kHz mono) recording is written here as a
    /// private WAV. Relative paths are resolved against the config file's
    /// directory. Off by default: recordings are otherwise never persisted.
    pub save_recordings_dir: Option<PathBuf>,
}

impl Default for DebugConfig {
    fn default() -> Self {
        Self {
            fake_processing_ms: 1500,
            fake_transcript: None,
            save_recordings_dir: None,
        }
    }
}

impl Config {
    /// Load from an explicit path (must exist) or the default XDG path
    /// (missing file means defaults).
    pub fn load(explicit: Option<&Path>) -> Result<Self, ConfigError> {
        let (path, required) = match explicit {
            Some(p) => (p.to_path_buf(), true),
            None => match default_path() {
                Some(p) => (p, false),
                None => return Ok(Self::default()),
            },
        };

        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if !required && e.kind() == std::io::ErrorKind::NotFound => {
                tracing::debug!(path = %path.display(), "no config file, using defaults");
                return Ok(Self::default());
            }
            Err(source) => return Err(ConfigError::Read { path, source }),
        };

        let mut config: Self = toml::from_str(&text).map_err(|source| ConfigError::Parse {
            path: path.clone(),
            source,
        })?;
        config.validate()?;
        let base = path.parent().unwrap_or(Path::new("."));
        let base = std::path::absolute(base).unwrap_or_else(|_| base.to_path_buf());
        for dir in [
            &mut config.debug.save_recordings_dir,
            &mut config.transcription.whisper.models_dir,
            &mut config.transcription.whisper.model_path,
            &mut config.cleanup.prompt_file,
        ] {
            if let Some(p) = dir
                && p.is_relative()
            {
                *p = base.join(&*p);
            }
        }
        if let Some(dir) = &config.debug.save_recordings_dir {
            tracing::warn!(dir = %dir.display(), "debug: recordings will be saved to disk");
        }
        tracing::info!(path = %path.display(), "loaded config");
        Ok(config)
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        if !(1..=3600).contains(&self.recording.max_seconds) {
            return Err(ConfigError::Invalid(
                "recording.max_seconds must be between 1 and 3600".into(),
            ));
        }
        if !(0..=2000).contains(&self.overlay.margin_bottom) {
            return Err(ConfigError::Invalid(
                "overlay.margin_bottom must be between 0 and 2000".into(),
            ));
        }
        if let Err(e) = self
            .injection
            .paste_chord
            .parse::<crate::injection::chord::Chord>()
        {
            return Err(ConfigError::Invalid(format!("injection.paste_chord: {e}")));
        }
        if self.injection.pre_insert_delay_ms > 5000 || self.injection.key_delay_ms > 500 {
            return Err(ConfigError::Invalid(
                "injection delays must be at most 5000 ms (pre-insert) and 500 ms (per key)".into(),
            ));
        }
        if !self.audio.host.eq_ignore_ascii_case("auto")
            && let Err(e) = self.audio.host.parse::<cpal::HostId>()
        {
            return Err(ConfigError::Invalid(format!("audio.host: {e}")));
        }
        if !(0.0..=10.0).contains(&self.audio.min_seconds) {
            return Err(ConfigError::Invalid(
                "audio.min_seconds must be between 0 and 10".into(),
            ));
        }
        if !(-100.0..=0.0).contains(&self.audio.silence_threshold_dbfs) {
            return Err(ConfigError::Invalid(
                "audio.silence_threshold_dbfs must be between -100 and 0".into(),
            ));
        }
        let w = &self.transcription.whisper;
        if w.model.trim().is_empty() || w.model.contains('/') {
            return Err(ConfigError::Invalid(
                "transcription.whisper.model must be a model name like \"large-v3-turbo-q5_0\"; use model_path for files".into(),
            ));
        }
        if !(1..=16).contains(&w.beam_size) {
            return Err(ConfigError::Invalid(
                "transcription.whisper.beam_size must be between 1 and 16".into(),
            ));
        }
        if w.threads.is_some_and(|t| !(1..=256).contains(&t)) {
            return Err(ConfigError::Invalid(
                "transcription.whisper.threads must be between 1 and 256".into(),
            ));
        }
        if !(1..=86_400).contains(&w.idle_unload_secs) {
            return Err(ConfigError::Invalid(
                "transcription.whisper.idle_unload_secs must be between 1 and 86400".into(),
            ));
        }
        if w.language.trim().is_empty() {
            return Err(ConfigError::Invalid(
                "transcription.whisper.language must be a code like \"de\" or \"auto\"".into(),
            ));
        }
        if !w.languages.is_empty() && w.language != "auto" {
            return Err(ConfigError::Invalid(
                "transcription.whisper.languages only applies with language = \"auto\"".into(),
            ));
        }
        if w.initial_prompt.as_ref().is_some_and(|p| p.len() > 2000) {
            return Err(ConfigError::Invalid(
                "transcription.whisper.initial_prompt must be at most 2000 bytes".into(),
            ));
        }
        self.validate_cleanup()?;
        if self.debug.fake_processing_ms > 60_000 {
            return Err(ConfigError::Invalid(
                "debug.fake_processing_ms must be at most 60000".into(),
            ));
        }
        Ok(())
    }

    fn validate_cleanup(&self) -> Result<(), ConfigError> {
        let c = &self.cleanup;
        let invalid = |msg: &str| Err(ConfigError::Invalid(format!("cleanup.{msg}")));
        if c.base_url.contains(['?', '#']) {
            return invalid(
                "base_url must not contain a query or fragment (put keys in api_key_env)",
            );
        }
        let Some((scheme, host)) = split_url(c.base_url.trim()) else {
            return invalid("base_url must look like \"http://127.0.0.1:11434/v1\"");
        };
        if scheme == "http" && !is_loopback_host(host) && !c.allow_insecure_http {
            return invalid(
                "base_url sends transcripts over plain http to a non-local host; \
                 use https or set allow_insecure_http = true",
            );
        }
        if c.model.trim().is_empty() || c.model.len() > 256 {
            return invalid("model must be a model name such as \"qwen2.5:7b-instruct\"");
        }
        if c.api_key_env.as_ref().is_some_and(|v| {
            v.is_empty() || !v.chars().all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
        }) {
            return invalid("api_key_env must be an environment variable name");
        }
        if c.reasoning_effort
            .as_ref()
            .is_some_and(|r| r.trim().is_empty() || r.len() > 32)
        {
            return invalid("reasoning_effort must be a level such as \"none\" or \"low\"");
        }
        if !(0.0..=2.0).contains(&c.temperature) {
            return invalid("temperature must be between 0 and 2");
        }
        if !(50..=60_000).contains(&c.connect_timeout_ms) {
            return invalid("connect_timeout_ms must be between 50 and 60000");
        }
        if !(1..=600).contains(&c.timeout_secs) {
            return invalid("timeout_secs must be between 1 and 600");
        }
        if c.min_words > 1000 {
            return invalid("min_words must be at most 1000");
        }
        Ok(())
    }

    pub fn max_recording(&self) -> Duration {
        Duration::from_secs(self.recording.max_seconds)
    }

    pub fn fake_processing(&self) -> Duration {
        Duration::from_millis(self.debug.fake_processing_ms)
    }
}

fn default_path() -> Option<PathBuf> {
    Some(
        xdg_dir("XDG_CONFIG_HOME", ".config")?
            .join("wayland-wispr")
            .join("config.toml"),
    )
}

/// `$XDG_DATA_HOME/wayland-wispr/models` (fallback `~/.local/share`).
#[cfg_attr(not(feature = "local-whisper"), allow(dead_code))]
pub fn default_models_dir() -> Option<PathBuf> {
    Some(
        xdg_dir("XDG_DATA_HOME", ".local/share")?
            .join("wayland-wispr")
            .join("models"),
    )
}

fn xdg_dir(var: &str, home_fallback: &str) -> Option<PathBuf> {
    std::env::var_os(var)
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .filter(|p| p.is_absolute())
                .map(|h| h.join(home_fallback))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_config_uses_defaults() {
        let c: Config = toml::from_str("").unwrap();
        assert_eq!(c, Config::default());
        c.validate().unwrap();
    }

    #[test]
    fn parses_partial_config() {
        let c: Config = toml::from_str(
            r#"
            [overlay]
            layer = "top"
            [recording]
            max_seconds = 30
            "#,
        )
        .unwrap();
        assert_eq!(c.overlay.layer, OverlayLayer::Top);
        assert_eq!(c.overlay.margin_bottom, 48);
        assert_eq!(c.max_recording(), Duration::from_secs(30));
    }

    #[test]
    fn rejects_unknown_keys_and_bad_values() {
        assert!(toml::from_str::<Config>("[overlay]\nfoo = 1").is_err());
        let c: Config = toml::from_str("[recording]\nmax_seconds = 0").unwrap();
        assert!(c.validate().is_err());
        let c: Config = toml::from_str("[injection]\npaste_chord = \"meta+v\"").unwrap();
        assert!(c.validate().is_err());
        let c: Config = toml::from_str("[injection]\npaste_chord = \"ctrl+shift+v\"").unwrap();
        c.validate().unwrap();
    }

    #[test]
    fn example_and_dev_configs_are_valid() {
        for text in [
            include_str!("../config.example.toml"),
            include_str!("../dev/config.toml"),
        ] {
            let c: Config = toml::from_str(text).unwrap();
            c.validate().unwrap();
        }
    }

    #[test]
    fn validates_audio_settings() {
        for bad in [
            "[audio]\nhost = \"coreaudio\"",
            "[audio]\nsilence_threshold_dbfs = 3.0",
            "[audio]\nmin_seconds = -1.0",
        ] {
            let c: Config = toml::from_str(bad).unwrap();
            assert!(c.validate().is_err(), "{bad}");
        }
        for good in ["[audio]\nhost = \"pipewire\"", "[audio]\nhost = \"ALSA\""] {
            let c: Config = toml::from_str(good).unwrap();
            c.validate().unwrap();
        }
    }

    #[test]
    fn resolves_recordings_dir_relative_to_config() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[debug]\nsave_recordings_dir = \"rec\"").unwrap();
        let c = Config::load(Some(&path)).unwrap();
        assert_eq!(c.debug.save_recordings_dir, Some(dir.path().join("rec")));
    }

    #[test]
    fn resolves_whisper_model_paths() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "[transcription.whisper]\nmodels_dir = \"models\"\nmodel = \"small-q5_1\"",
        )
        .unwrap();
        let c = Config::load(Some(&path)).unwrap();
        assert_eq!(
            c.transcription.whisper.resolved_model_path(),
            Some(dir.path().join("models/ggml-small-q5_1.bin"))
        );

        let mut w = WhisperConfig {
            model_path: Some("/m/x.bin".into()),
            ..WhisperConfig::default()
        };
        assert_eq!(w.resolved_model_path(), Some(PathBuf::from("/m/x.bin")));
        w.model_path = None;
        w.models_dir = Some("/m".into());
        w.model = "ggml-custom.bin".into();
        assert_eq!(
            w.resolved_model_path(),
            Some(PathBuf::from("/m/ggml-custom.bin"))
        );
    }

    #[test]
    fn validates_whisper_settings() {
        for bad in [
            "[transcription.whisper]\nbeam_size = 0",
            "[transcription.whisper]\nmodel = \"../x\"",
            "[transcription.whisper]\nlanguage = \"de\"\nlanguages = [\"de\", \"en\"]",
            "[transcription.whisper]\nidle_unload_secs = 0",
        ] {
            let c: Config = toml::from_str(bad).unwrap();
            assert!(c.validate().is_err(), "{bad}");
        }
        assert!(toml::from_str::<Config>("[transcription.whisper]\ndevice = \"tpu\"").is_err());
        let c: Config = toml::from_str(
            "[transcription]\nbackend = \"whisper\"\n[transcription.whisper]\ndevice = \"gpu\"\nlanguages = [\"de\", \"en\"]\nkeep_loaded = \"always\"",
        )
        .unwrap();
        c.validate().unwrap();
        assert_eq!(c.transcription.whisper.device, WhisperDevice::Gpu);
    }

    #[test]
    fn validates_cleanup_settings() {
        for bad in [
            "[cleanup]\nbase_url = \"localhost:11434\"",
            "[cleanup]\nbase_url = \"ftp://127.0.0.1/v1\"",
            "[cleanup]\nbase_url = \"http://192.168.1.5:11434/v1\"",
            "[cleanup]\nbase_url = \"http://user@127.0.0.1/v1\"",
            "[cleanup]\nbase_url = \"https://api.example.com/v1?key=secret\"",
            "[cleanup]\nmodel = \" \"",
            "[cleanup]\napi_key_env = \"MY-KEY\"",
            "[cleanup]\ntemperature = 3.0",
            "[cleanup]\ntimeout_secs = 0",
            "[cleanup]\nreasoning_effort = \"\"",
        ] {
            let c: Config = toml::from_str(bad).unwrap();
            assert!(c.validate().is_err(), "{bad}");
        }
        for good in [
            "[cleanup]\nbase_url = \"http://localhost:11434/v1\"",
            "[cleanup]\nbase_url = \"http://[::1]:8080/v1\"",
            "[cleanup]\nbase_url = \"https://api.example.com/v1\"\napi_key_env = \"LLM_KEY\"",
            "[cleanup]\nbase_url = \"http://192.168.1.5:11434/v1\"\nallow_insecure_http = true",
            "[cleanup]\nenabled = false\nreasoning_effort = \"none\"",
        ] {
            let c: Config = toml::from_str(good).unwrap();
            c.validate().unwrap();
        }
    }

    #[test]
    fn splits_urls() {
        assert_eq!(
            split_url("http://127.0.0.1:11434/v1"),
            Some(("http", "127.0.0.1"))
        );
        assert_eq!(split_url("https://[::1]/v1"), Some(("https", "::1")));
        assert_eq!(
            split_url("https://example.com"),
            Some(("https", "example.com"))
        );
        assert_eq!(split_url("http:///v1"), None);
        assert!(is_loopback_host("127.0.0.2"));
        assert!(is_loopback_host("LOCALHOST"));
        assert!(!is_loopback_host("10.0.0.1"));
    }

    #[test]
    fn missing_explicit_file_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        assert!(Config::load(Some(&dir.path().join("nope.toml"))).is_err());
    }
}

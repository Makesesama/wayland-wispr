//! Typed settings loaded from `$XDG_CONFIG_HOME/wayland-wispr/config.toml`.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("could not read config {path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("invalid config {path}: {source}")]
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
pub struct DebugConfig {
    /// Duration of the placeholder processing stage until real backends exist.
    pub fake_processing_ms: u64,
}

impl Default for DebugConfig {
    fn default() -> Self {
        Self {
            fake_processing_ms: 1500,
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

        let config: Self =
            toml::from_str(&text).map_err(|source| ConfigError::Parse { path, source })?;
        config.validate()?;
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
        if self.debug.fake_processing_ms > 60_000 {
            return Err(ConfigError::Invalid(
                "debug.fake_processing_ms must be at most 60000".into(),
            ));
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
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .filter(|p| p.is_absolute())
                .map(|h| h.join(".config"))
        })?;
    Some(base.join("wayland-wispr").join("config.toml"))
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
    }

    #[test]
    fn missing_explicit_file_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        assert!(Config::load(Some(&dir.path().join("nope.toml"))).is_err());
    }
}

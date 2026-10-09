//! Microphone capture and audio normalization.

pub mod capture;
pub mod convert;

use std::fs::{self, DirBuilder, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub use capture::{Capture, CaptureEvent};
pub use convert::Clip;

use crate::config::AudioConfig;

/// Turn a raw device recording into mono 16 kHz audio ready for
/// transcription, rejecting recordings that are too short or silent.
///
/// `save` is called with the normalized clip before the silence check so
/// rejected recordings can still be inspected when debugging.
pub fn prepare(raw: &Clip, config: &AudioConfig, save: impl FnOnce(&Clip)) -> Result<Clip, String> {
    convert::check_duration(raw, config.min_seconds).map_err(|e| e.to_string())?;
    let clip = convert::resample_to_target(raw)?;
    save(&clip);
    convert::check_not_silent(&clip, config.silence_threshold_dbfs).map_err(|e| e.to_string())?;
    Ok(clip)
}

/// Write a private (0600) WAV for debugging into a private (0700) directory.
pub fn save_debug_wav(dir: &Path, session: u64, clip: &Clip) -> Result<PathBuf, String> {
    let bytes = convert::encode_wav(clip)?;
    match DirBuilder::new().recursive(true).mode(0o700).create(dir) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(format!("creating {}: {e}", dir.display())),
    }
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let path = dir.join(format!("recording-{stamp}-s{session}.wav"));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .map_err(|e| format!("creating {}: {e}", path.display()))?;
    file.write_all(&bytes)
        .and_then(|()| file.sync_all())
        .map_err(|e| {
            let _ = fs::remove_file(&path);
            format!("writing {}: {e}", path.display())
        })?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> AudioConfig {
        AudioConfig::default()
    }

    #[test]
    fn prepare_resamples_and_saves_before_silence_check() {
        let raw = Clip {
            samples: vec![0.0; 48_000],
            sample_rate: 48_000,
        };
        let mut saved = None;
        let result = prepare(&raw, &cfg(), |c| saved = Some(c.sample_rate));
        assert!(result.unwrap_err().contains("no speech"));
        assert_eq!(saved, Some(convert::TARGET_RATE));
    }

    #[test]
    fn prepare_rejects_short_without_saving() {
        let raw = Clip {
            samples: vec![0.3; 100],
            sample_rate: 48_000,
        };
        let mut saved = false;
        assert!(prepare(&raw, &cfg(), |_| saved = true).is_err());
        assert!(!saved);
    }

    #[test]
    fn saves_private_wav() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("recordings");
        let clip = Clip {
            samples: vec![0.1; 160],
            sample_rate: convert::TARGET_RATE,
        };
        let path = save_debug_wav(&target, 7, &clip).unwrap();
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let dmode = fs::metadata(&target).unwrap().permissions().mode() & 0o777;
        assert_eq!(dmode, 0o700);
        assert!(hound::WavReader::open(&path).is_ok());
    }
}

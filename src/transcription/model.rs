//! Model file resolution and cheap validation before handing it to whisper.cpp.

use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

use crate::config::WhisperConfig;

/// whisper.cpp `GGML_FILE_MAGIC` (0x67676d6c, "ggml") as stored little-endian.
const GGML_MAGIC: [u8; 4] = *b"lmgg";

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ModelError {
    #[error(
        "no whisper model directory: set transcription.whisper.model_path or models_dir (HOME/XDG_DATA_HOME unset)"
    )]
    NoModelsDir,
    #[error(
        "whisper model not found: {path}. Download a ggml model (e.g. from huggingface.co/ggerganov/whisper.cpp) or set transcription.whisper.model / model_path"
    )]
    Missing { path: PathBuf },
    #[error("cannot read whisper model {path}: {reason}")]
    Unreadable { path: PathBuf, reason: String },
    #[error("{path} is not a whisper.cpp ggml model ({reason})")]
    Invalid { path: PathBuf, reason: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelFile {
    pub path: PathBuf,
    pub bytes: u64,
}

/// Resolve the configured model and check that it looks like a ggml file.
pub fn locate(config: &WhisperConfig) -> Result<ModelFile, ModelError> {
    let path = config
        .resolved_model_path()
        .ok_or(ModelError::NoModelsDir)?;
    validate(&path)
}

pub fn validate(path: &Path) -> Result<ModelFile, ModelError> {
    let unreadable = |e: std::io::Error| ModelError::Unreadable {
        path: path.to_path_buf(),
        reason: e.to_string(),
    };
    let meta = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(ModelError::Missing {
                path: path.to_path_buf(),
            });
        }
        Err(e) => return Err(unreadable(e)),
    };
    if !meta.is_file() {
        return Err(ModelError::Invalid {
            path: path.to_path_buf(),
            reason: "not a regular file".into(),
        });
    }
    let mut magic = [0u8; 4];
    File::open(path)
        .and_then(|mut f| f.read_exact(&mut magic))
        .map_err(unreadable)?;
    if magic != GGML_MAGIC {
        return Err(ModelError::Invalid {
            path: path.to_path_buf(),
            reason: "wrong file magic; GGUF/safetensors/PyTorch files are not supported".into(),
        });
    }
    Ok(ModelFile {
        path: path.to_path_buf(),
        bytes: meta.len(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_magic_and_existence() {
        let dir = tempfile::tempdir().unwrap();
        let good = dir.path().join("ggml-x.bin");
        std::fs::write(&good, b"lmgg rest").unwrap();
        assert_eq!(validate(&good).unwrap().bytes, 9);

        let bad = dir.path().join("bad.bin");
        std::fs::write(&bad, b"GGUF....").unwrap();
        assert!(matches!(validate(&bad), Err(ModelError::Invalid { .. })));

        let short = dir.path().join("short.bin");
        std::fs::write(&short, b"lm").unwrap();
        assert!(matches!(
            validate(&short),
            Err(ModelError::Unreadable { .. })
        ));

        assert!(matches!(
            validate(&dir.path().join("nope.bin")),
            Err(ModelError::Missing { .. })
        ));
        assert!(matches!(
            validate(dir.path()),
            Err(ModelError::Invalid { .. })
        ));
    }

    #[test]
    fn locates_named_model() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("ggml-small-q5_1.bin"), b"lmgg").unwrap();
        let config = WhisperConfig {
            model: "small-q5_1".into(),
            models_dir: Some(dir.path().to_path_buf()),
            ..WhisperConfig::default()
        };
        assert_eq!(
            locate(&config).unwrap().path,
            dir.path().join("ggml-small-q5_1.bin")
        );
    }
}

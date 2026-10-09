//! Pure audio conversion: downmixing, levels, resampling, speech checks, WAV.
//! Free of devices and threads so it can be tested with synthetic signals.

use std::io::Cursor;

use rubato::audioadapter_buffers::direct::InterleavedSlice;
use rubato::{Fft, FixedSync, Resampler};

/// Sample rate expected by Whisper models and used for uploads.
pub const TARGET_RATE: u32 = 16_000;
/// Window for speech detection (30 ms at 16 kHz).
const SPEECH_WINDOW: usize = 480;
/// Floor for dBFS values so silence is finite.
pub const MIN_DBFS: f32 = -100.0;

/// Mono audio at a given sample rate.
#[derive(Debug, Clone, PartialEq)]
pub struct Clip {
    pub samples: Vec<f32>,
    pub sample_rate: u32,
}

impl Clip {
    pub fn duration_secs(&self) -> f32 {
        self.samples.len() as f32 / self.sample_rate as f32
    }
}

/// Streaming interleaved → mono downmix that tolerates frames split across
/// chunk boundaries.
#[derive(Debug)]
pub struct Downmix {
    channels: usize,
    acc: f32,
    idx: usize,
}

impl Downmix {
    pub fn new(channels: u16) -> Self {
        Self {
            channels: usize::from(channels.max(1)),
            acc: 0.0,
            idx: 0,
        }
    }

    /// Append mono frames for `interleaved` to `out`, stopping at `cap`
    /// total samples. Returns `false` if samples were dropped due to the cap.
    pub fn push(&mut self, interleaved: &[f32], out: &mut Vec<f32>, cap: usize) -> bool {
        let mut kept_all = true;
        for &s in interleaved {
            self.acc += s;
            self.idx += 1;
            if self.idx == self.channels {
                if out.len() < cap {
                    out.push(self.acc / self.channels as f32);
                } else {
                    kept_all = false;
                }
                self.acc = 0.0;
                self.idx = 0;
            }
        }
        kept_all
    }
}

pub fn rms(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    let sum: f64 = samples.iter().map(|&s| f64::from(s) * f64::from(s)).sum();
    (sum / samples.len() as f64).sqrt() as f32
}

pub fn to_dbfs(amplitude: f32) -> f32 {
    if amplitude <= 0.0 {
        MIN_DBFS
    } else {
        (20.0 * amplitude.log10()).max(MIN_DBFS)
    }
}

/// Resample mono audio to [`TARGET_RATE`].
pub fn resample_to_target(clip: &Clip) -> Result<Clip, String> {
    if clip.sample_rate == TARGET_RATE || clip.samples.is_empty() {
        return Ok(Clip {
            samples: clip.samples.clone(),
            sample_rate: TARGET_RATE,
        });
    }
    let len = clip.samples.len();
    let mut resampler = Fft::<f32>::new(
        clip.sample_rate as usize,
        TARGET_RATE as usize,
        1024,
        1,
        FixedSync::Input,
    )
    .map_err(|e| format!("resampler setup failed: {e}"))?;
    let input = InterleavedSlice::new(&clip.samples[..], 1, len)
        .map_err(|e| format!("resampler input: {e}"))?;
    let output = resampler
        .process_all(&input, len, None)
        .map_err(|e| format!("resampling failed: {e}"))?;
    Ok(Clip {
        samples: output.take_data(),
        sample_rate: TARGET_RATE,
    })
}

/// Loudest short-window level in dBFS; robust to long silent stretches
/// around brief speech.
pub fn peak_window_dbfs(samples: &[f32]) -> f32 {
    samples
        .chunks(SPEECH_WINDOW)
        .map(|w| to_dbfs(rms(w)))
        .fold(MIN_DBFS, f32::max)
}

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum SpeechError {
    #[error("recording too short ({secs:.2} s, minimum {min:.2} s)")]
    TooShort { secs: f32, min: f32 },
    #[error(
        "no speech detected (loudest {peak:.0} dBFS, threshold {threshold:.0} dBFS); check the microphone"
    )]
    Silent { peak: f32, threshold: f32 },
}

pub fn check_duration(clip: &Clip, min_secs: f32) -> Result<(), SpeechError> {
    let secs = clip.duration_secs();
    if secs < min_secs {
        return Err(SpeechError::TooShort {
            secs,
            min: min_secs,
        });
    }
    Ok(())
}

pub fn check_not_silent(clip: &Clip, threshold_dbfs: f32) -> Result<(), SpeechError> {
    let peak = peak_window_dbfs(&clip.samples);
    if peak < threshold_dbfs {
        return Err(SpeechError::Silent {
            peak,
            threshold: threshold_dbfs,
        });
    }
    Ok(())
}

/// Encode as 16-bit PCM mono WAV in memory.
pub fn encode_wav(clip: &Clip) -> Result<Vec<u8>, String> {
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: clip.sample_rate,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut bytes = Vec::with_capacity(44 + clip.samples.len() * 2);
    let mut writer = hound::WavWriter::new(Cursor::new(&mut bytes), spec)
        .map_err(|e| format!("wav encode: {e}"))?;
    for &s in &clip.samples {
        let v = (s.clamp(-1.0, 1.0) * f32::from(i16::MAX)).round() as i16;
        writer
            .write_sample(v)
            .map_err(|e| format!("wav encode: {e}"))?;
    }
    writer.finalize().map_err(|e| format!("wav encode: {e}"))?;
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(freq: f32, rate: u32, secs: f32, amp: f32) -> Vec<f32> {
        let n = (rate as f32 * secs) as usize;
        (0..n)
            .map(|i| amp * (2.0 * std::f32::consts::PI * freq * i as f32 / rate as f32).sin())
            .collect()
    }

    #[test]
    fn downmix_averages_and_handles_split_frames() {
        let mut d = Downmix::new(2);
        let mut out = Vec::new();
        assert!(d.push(&[1.0, 0.0, 0.5], &mut out, 10));
        assert_eq!(out, vec![0.5]);
        assert!(d.push(&[0.5, -1.0, -1.0], &mut out, 10));
        assert_eq!(out, vec![0.5, 0.5, -1.0]);
    }

    #[test]
    fn downmix_mono_passthrough_and_cap() {
        let mut d = Downmix::new(1);
        let mut out = Vec::new();
        assert!(!d.push(&[0.1, 0.2, 0.3], &mut out, 2));
        assert_eq!(out, vec![0.1, 0.2]);
    }

    #[test]
    fn levels() {
        assert_eq!(rms(&[]), 0.0);
        assert!((rms(&[0.5, -0.5]) - 0.5).abs() < 1e-6);
        assert!((to_dbfs(1.0)).abs() < 1e-6);
        assert!((to_dbfs(0.1) + 20.0).abs() < 1e-4);
        assert_eq!(to_dbfs(0.0), MIN_DBFS);
    }

    #[test]
    fn resamples_48k_to_16k_preserving_duration_and_level() {
        let clip = Clip {
            samples: sine(440.0, 48_000, 1.0, 0.5),
            sample_rate: 48_000,
        };
        let out = resample_to_target(&clip).unwrap();
        assert_eq!(out.sample_rate, TARGET_RATE);
        assert!(
            (out.samples.len() as i64 - 16_000).abs() <= 2,
            "{}",
            out.samples.len()
        );
        // RMS of a 0.5-amplitude sine is ~0.354; ignore edges.
        let mid = &out.samples[1000..15_000];
        assert!((rms(mid) - 0.3536).abs() < 0.01, "{}", rms(mid));
    }

    #[test]
    fn resamples_44k1_and_passes_through_16k() {
        let clip = Clip {
            samples: sine(440.0, 44_100, 0.5, 0.5),
            sample_rate: 44_100,
        };
        let out = resample_to_target(&clip).unwrap();
        assert!((out.samples.len() as i64 - 8_000).abs() <= 2);

        let same = Clip {
            samples: vec![0.1; 100],
            sample_rate: TARGET_RATE,
        };
        assert_eq!(resample_to_target(&same).unwrap(), same);
    }

    #[test]
    fn detects_silence_and_short_speech_in_silence() {
        let silent = Clip {
            samples: vec![0.0005; 16_000],
            sample_rate: TARGET_RATE,
        };
        assert!(matches!(
            check_not_silent(&silent, -45.0),
            Err(SpeechError::Silent { .. })
        ));

        // 2 s of near-silence with 100 ms of quiet speech-level tone.
        let mut samples = vec![0.0; 32_000];
        samples.splice(16_000..17_600, sine(300.0, 16_000, 0.1, 0.05));
        let clip = Clip {
            samples,
            sample_rate: TARGET_RATE,
        };
        check_not_silent(&clip, -45.0).unwrap();
    }

    #[test]
    fn rejects_short_recordings() {
        let clip = Clip {
            samples: vec![0.0; 1_600],
            sample_rate: TARGET_RATE,
        };
        assert!(check_duration(&clip, 0.3).is_err());
        check_duration(&clip, 0.1).unwrap();
    }

    #[test]
    fn wav_round_trip() {
        let clip = Clip {
            samples: vec![0.0, 0.5, -0.5, 1.5],
            sample_rate: TARGET_RATE,
        };
        let bytes = encode_wav(&clip).unwrap();
        let reader = hound::WavReader::new(Cursor::new(bytes)).unwrap();
        assert_eq!(reader.spec().sample_rate, TARGET_RATE);
        assert_eq!(reader.spec().channels, 1);
        let s: Vec<i16> = reader.into_samples().map(Result::unwrap).collect();
        assert_eq!(s, vec![0, 16384, -16384, i16::MAX]);
    }
}

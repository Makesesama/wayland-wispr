//! `wayland-wispr bench`: measure a local whisper model on this machine.
//!
//! Model file size says little about actual memory use (KV cache, compute
//! buffers, driver allocations), so pick per-host models from measurements.

use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Context, bail};

use crate::audio::Clip;
use crate::audio::convert::{self, Downmix};
use crate::config::{WhisperConfig, WhisperDevice};
use crate::transcription::CancelFlag;
use crate::transcription::whisper::{self, Model, Settings};

#[derive(Debug, Clone, Default)]
pub struct Overrides {
    pub model: Option<String>,
    pub model_path: Option<PathBuf>,
    pub device: Option<WhisperDevice>,
    pub beam_size: Option<u8>,
    pub language: Option<String>,
    pub threads: Option<u16>,
}

pub fn run(base: &WhisperConfig, wav: &Path, runs: u32, o: Overrides) -> anyhow::Result<()> {
    let mut config = base.clone();
    if let Some(model) = o.model {
        config.model = model;
        config.model_path = None;
    }
    if let Some(path) = o.model_path {
        config.model_path = Some(path);
    }
    if let Some(device) = o.device {
        config.device = device;
    }
    if let Some(beam) = o.beam_size {
        config.beam_size = beam;
    }
    if let Some(language) = o.language {
        config.language = language;
        config.languages.clear();
    }
    if o.threads.is_some() {
        config.threads = o.threads;
    }

    whisper::install_logging();
    let settings = Settings::from_config(&config)?;
    let clip = load_wav(wav)?;

    println!("whisper.cpp {}", whisper_rs::get_whisper_version());
    println!("system:   {}", whisper_rs::print_system_info().trim());
    let gpus = whisper::gpu_devices();
    if whisper::GPU_BUILD {
        if gpus.is_empty() {
            println!("gpus:     none found (no Vulkan driver/device?)");
        }
        for g in &gpus {
            println!(
                "gpu {}:    {} ({}): {} free / {}",
                g.index,
                g.description,
                g.name,
                mib(g.free),
                mib(g.total)
            );
        }
    } else {
        println!("gpus:     (CPU-only build)");
    }
    println!(
        "model:    {} ({})",
        settings.model.path.display(),
        mib(settings.model.bytes as usize)
    );
    println!(
        "settings: device={} flash_attn={} beam_size={} threads={} language={:?}",
        settings.device_label(),
        settings.flash_attn,
        settings.beam_size,
        settings.threads,
        settings.language
    );
    println!(
        "audio:    {} ({:.1} s)",
        wav.display(),
        clip.duration_secs()
    );
    println!();

    let vram_free = || settings.use_gpu.then(|| gpu_free(&settings)).flatten();
    let vram_before = vram_free();
    let rss_before = proc_status_kib("VmRSS:");

    let started = Instant::now();
    let mut model = Model::load(&settings)?;
    let load = started.elapsed();
    let vram_loaded = vram_free();
    println!(
        "load:     {} ms, RSS +{}{}",
        load.as_millis(),
        kib_delta(rss_before, proc_status_kib("VmRSS:")),
        vram_line(vram_before, vram_loaded)
    );

    let cancel = CancelFlag::default();
    for i in 1..=runs.max(1) {
        let started = Instant::now();
        let result = model.transcribe(&settings, &clip.samples, &cancel);
        let took = started.elapsed();
        let rtf = took.as_secs_f32() / clip.duration_secs().max(0.01);
        match result {
            Ok(t) => {
                println!(
                    "run {i}:    {} ms, RTF {rtf:.3}, language {}",
                    took.as_millis(),
                    t.language.as_deref().unwrap_or("?")
                );
                if i == 1 {
                    println!("text:     {}", t.text);
                }
            }
            Err(e) => println!("run {i}:    failed after {} ms: {e}", took.as_millis()),
        }
    }

    println!();
    println!(
        "peak RSS: {}{}",
        proc_status_kib("VmHWM:").map_or("?".into(), |k| mib(k * 1024)),
        vram_line(vram_before, vram_free())
    );
    if settings.use_gpu {
        println!(
            "note:     VRAM figures are device-wide free-memory deltas; close other GPU apps for clean numbers."
        );
    }
    println!("hint:     RUST_LOG=whisper_rs=info shows whisper.cpp's own buffer sizes.");
    Ok(())
}

fn gpu_free(settings: &Settings) -> Option<usize> {
    whisper::gpu_devices()
        .into_iter()
        .find(|g| g.index == settings.gpu_device)
        .map(|g| g.free)
}

fn vram_line(before: Option<usize>, after: Option<usize>) -> String {
    match (before, after) {
        (Some(b), Some(a)) => format!(", VRAM used +{}", mib(b.saturating_sub(a))),
        _ => String::new(),
    }
}

fn mib(bytes: usize) -> String {
    format!("{:.0} MiB", bytes as f64 / f64::from(1 << 20))
}

fn kib_delta(before: Option<usize>, after: Option<usize>) -> String {
    match (before, after) {
        (Some(b), Some(a)) => mib(a.saturating_sub(b) * 1024),
        _ => "?".into(),
    }
}

/// A `kB` field from /proc/self/status, e.g. `VmRSS:` or `VmHWM:` (peak).
fn proc_status_kib(field: &str) -> Option<usize> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    status
        .lines()
        .find_map(|l| l.strip_prefix(field))?
        .split_whitespace()
        .next()?
        .parse()
        .ok()
}

/// Read any PCM WAV and normalize it to mono 16 kHz like a recording.
pub fn load_wav(path: &Path) -> anyhow::Result<Clip> {
    let mut reader =
        hound::WavReader::open(path).with_context(|| format!("opening {}", path.display()))?;
    let spec = reader.spec();
    let interleaved: Vec<f32> = match spec.sample_format {
        hound::SampleFormat::Float => reader.samples::<f32>().collect::<Result<_, _>>()?,
        hound::SampleFormat::Int => {
            let scale = 1.0 / (1u64 << (spec.bits_per_sample - 1)) as f32;
            reader
                .samples::<i32>()
                .map(|s| s.map(|s| s as f32 * scale))
                .collect::<Result<_, _>>()?
        }
    };
    let mut mono = Vec::with_capacity(interleaved.len() / usize::from(spec.channels.max(1)));
    Downmix::new(spec.channels).push(&interleaved, &mut mono, usize::MAX);
    if mono.is_empty() {
        bail!("{} contains no audio", path.display());
    }
    convert::resample_to_target(&Clip {
        samples: mono,
        sample_rate: spec.sample_rate,
    })
    .map_err(anyhow::Error::msg)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_and_normalizes_stereo_wav() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.wav");
        let spec = hound::WavSpec {
            channels: 2,
            sample_rate: 48_000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut w = hound::WavWriter::create(&path, spec).unwrap();
        for _ in 0..48_000 {
            w.write_sample(i16::MAX / 2).unwrap();
            w.write_sample(i16::MAX / 2).unwrap();
        }
        w.finalize().unwrap();
        let clip = load_wav(&path).unwrap();
        assert_eq!(clip.sample_rate, convert::TARGET_RATE);
        assert!((clip.duration_secs() - 1.0).abs() < 0.05);
        let mid = clip.samples[clip.samples.len() / 2];
        assert!((mid - 0.5).abs() < 0.01, "{mid}");
    }
}

//! Microphone capture on a dedicated thread.
//!
//! The cpal stream is created and owned by the capture thread (streams are
//! not `Send` on every backend). The realtime callback only converts samples
//! to f32 and pushes them into a lock-free SPSC ring buffer; it never blocks
//! or allocates. The capture thread drains the ring every few milliseconds,
//! downmixes to mono into a bounded buffer, and reports input levels.

use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, Sample, SampleFormat, SizedSample};

use super::convert::{Clip, Downmix, rms, to_dbfs};
use crate::config::AudioConfig;

const DRAIN_INTERVAL: Duration = Duration::from_millis(20);
const LEVEL_INTERVAL: Duration = Duration::from_millis(50);
/// Ring capacity in seconds of device audio; far above the drain interval.
const RING_SECONDS: usize = 2;

/// Notifications from the capture thread (called on that thread).
#[derive(Debug, Clone, PartialEq)]
pub enum CaptureEvent {
    /// Input level of the most recent audio, in dBFS.
    Level(f32),
    /// Capture stopped with an error; `finish` will return it as well.
    Failed(String),
}

enum Control {
    Stop,
    Cancel,
}

/// A running capture. Dropping it without [`Capture::finish`] cancels
/// recording and releases the device without blocking.
pub struct Capture {
    control: mpsc::Sender<Control>,
    thread: Option<JoinHandle<Result<Clip, String>>>,
}

impl Capture {
    pub fn start<F>(config: &AudioConfig, max: Duration, on_event: F) -> Self
    where
        F: Fn(CaptureEvent) + Send + 'static,
    {
        let (control, control_rx) = mpsc::channel();
        let config = config.clone();
        let thread = thread::Builder::new()
            .name("capture".into())
            .spawn(move || {
                let result = run(&config, max, control_rx, &on_event);
                if let Err(e) = &result {
                    on_event(CaptureEvent::Failed(e.clone()));
                }
                result
            });

        let thread = match thread {
            Ok(t) => Some(t),
            Err(e) => {
                tracing::error!(error = %e, "failed to spawn capture thread");
                None
            }
        };
        Self { control, thread }
    }

    /// Stop recording and return the captured mono audio. Blocks briefly
    /// while the device is released.
    pub fn finish(mut self) -> Result<Clip, String> {
        let thread = self
            .thread
            .take()
            .ok_or("capture thread could not be started")?;
        let _ = self.control.send(Control::Stop);
        thread
            .join()
            .map_err(|_| "capture thread panicked".to_owned())?
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        if self.thread.is_some() {
            let _ = self.control.send(Control::Cancel);
        }
    }
}

pub fn host(config: &AudioConfig) -> Result<cpal::Host, String> {
    if config.host.eq_ignore_ascii_case("auto") {
        // Like cpal::default_host(), but say why PipeWire was skipped instead
        // of silently falling back to ALSA.
        match cpal::host_from_id(cpal::HostId::PipeWire) {
            Ok(host) => return Ok(host),
            Err(e) => tracing::warn!(error = %e, "PipeWire unavailable; falling back to ALSA"),
        }
        return cpal::host_from_id(cpal::HostId::Alsa).map_err(|e| format!("audio host ALSA: {e}"));
    }
    let id = cpal::HostId::from_str(&config.host).map_err(|e| e.to_string())?;
    cpal::host_from_id(id).map_err(|e| format!("audio host {}: {e}", id.name()))
}

/// Human-readable name of a device (falls back to its ID).
pub fn device_name(device: &cpal::Device) -> String {
    device
        .description()
        .map(|d| d.name().to_owned())
        .unwrap_or_else(|_| device.to_string())
}

pub fn device_id(device: &cpal::Device) -> Option<String> {
    device.id().ok().map(|id| id.to_string())
}

fn select_device(host: &cpal::Host, config: &AudioConfig) -> Result<cpal::Device, String> {
    match &config.device {
        None => host
            .default_input_device()
            .ok_or_else(|| format!("no default input device on {}", host.id().name())),
        Some(wanted) => host
            .input_devices()
            .map_err(|e| format!("listing input devices: {e}"))?
            .find(|d| {
                device_id(d).as_deref() == Some(wanted.as_str())
                    || device_name(d).eq_ignore_ascii_case(wanted)
            })
            .ok_or_else(|| {
                format!("input device `{wanted}` not found; see `wayland-wispr devices`")
            }),
    }
}

fn run<F>(
    config: &AudioConfig,
    max: Duration,
    control: mpsc::Receiver<Control>,
    on_event: &F,
) -> Result<Clip, String>
where
    F: Fn(CaptureEvent),
{
    let host = host(config)?;
    let device = select_device(&host, config)?;
    let supported = device
        .default_input_config()
        .map_err(|e| format!("input config for {}: {e}", device_name(&device)))?;
    let stream_config = supported.config();
    let rate = stream_config.sample_rate;
    let channels = stream_config.channels;

    tracing::info!(
        host = host.id().name(),
        device = %device_name(&device),
        rate,
        channels,
        format = ?supported.sample_format(),
        "capture started"
    );

    let ring_len = rate as usize * usize::from(channels) * RING_SECONDS;
    let (producer, mut consumer) = rtrb::RingBuffer::<f32>::new(ring_len);
    let overflow = Arc::new(AtomicBool::new(false));
    let (err_tx, err_rx) = mpsc::channel::<String>();

    let stream = build_stream(
        &device,
        stream_config,
        supported.sample_format(),
        producer,
        overflow.clone(),
        err_tx,
    )?;
    stream
        .play()
        .map_err(|e| format!("starting capture on {}: {e}", device_name(&device)))?;

    let cap = (rate as f64 * (max.as_secs_f64() + 1.0)) as usize;
    let mut mono = Vec::with_capacity(rate as usize * 10);
    let mut downmix = Downmix::new(channels);
    let mut level_from = 0usize;
    let mut last_level = Instant::now();
    let mut warned_cap = false;

    let mut drain = |mono: &mut Vec<f32>| {
        let available = consumer.slots();
        if available == 0 {
            return;
        }
        if let Ok(chunk) = consumer.read_chunk(available) {
            let (a, b) = chunk.as_slices();
            let kept = downmix.push(a, mono, cap) & downmix.push(b, mono, cap);
            chunk.commit_all();
            if !kept && !warned_cap {
                warned_cap = true;
                tracing::warn!("recording buffer full; further audio is dropped");
            }
        }
    };

    loop {
        match control.recv_timeout(DRAIN_INTERVAL) {
            Ok(Control::Stop) => {
                drain(&mut mono);
                break;
            }
            Ok(Control::Cancel) | Err(RecvTimeoutError::Disconnected) => {
                tracing::info!("capture cancelled");
                return Err("capture cancelled".into());
            }
            Err(RecvTimeoutError::Timeout) => {}
        }

        drain(&mut mono);
        if let Ok(e) = err_rx.try_recv() {
            return Err(format!("microphone error: {e}"));
        }
        if overflow.load(Ordering::Relaxed) {
            return Err("audio buffer overflow; capture thread fell behind".into());
        }
        if last_level.elapsed() >= LEVEL_INTERVAL {
            on_event(CaptureEvent::Level(to_dbfs(rms(&mono[level_from..]))));
            level_from = mono.len();
            last_level = Instant::now();
        }
    }

    drop(stream);
    let clip = Clip {
        samples: mono,
        sample_rate: rate,
    };
    tracing::info!(seconds = clip.duration_secs(), "capture stopped");
    Ok(clip)
}

fn build_stream(
    device: &cpal::Device,
    config: cpal::StreamConfig,
    format: SampleFormat,
    producer: rtrb::Producer<f32>,
    overflow: Arc<AtomicBool>,
    err_tx: mpsc::Sender<String>,
) -> Result<cpal::Stream, String> {
    macro_rules! build {
        ($($fmt:ident => $t:ty),* $(,)?) => {
            match format {
                $(SampleFormat::$fmt => typed::<$t>(device, config, producer, overflow, err_tx),)*
                other => Err(format!("unsupported sample format {other:?}")),
            }
        };
    }
    build!(
        F32 => f32, F64 => f64,
        I8 => i8, I16 => i16, I24 => cpal::I24, I32 => i32, I64 => i64,
        U8 => u8, U16 => u16, U24 => cpal::U24, U32 => u32, U64 => u64,
    )
}

fn typed<T>(
    device: &cpal::Device,
    config: cpal::StreamConfig,
    mut producer: rtrb::Producer<f32>,
    overflow: Arc<AtomicBool>,
    err_tx: mpsc::Sender<String>,
) -> Result<cpal::Stream, String>
where
    T: SizedSample,
    f32: FromSample<T>,
{
    device
        .build_input_stream::<T, _, _>(
            config,
            move |data: &[T], _| {
                for &s in data {
                    if producer.push(f32::from_sample(s)).is_err() {
                        overflow.store(true, Ordering::Relaxed);
                        return;
                    }
                }
            },
            move |e| {
                let _ = err_tx.send(e.to_string());
            },
            None,
        )
        .map_err(|e| format!("opening {}: {e}", device_name(device)))
}

/// One input device as shown by `wayland-wispr devices`.
#[derive(Debug, serde::Serialize)]
pub struct DeviceInfo {
    pub name: String,
    pub id: Option<String>,
    pub default: bool,
    pub config: Option<String>,
}

pub fn list_devices(config: &AudioConfig) -> Result<(String, Vec<DeviceInfo>), String> {
    let host = host(config)?;
    let default_id = host.default_input_device().and_then(|d| device_id(&d));
    let devices = host
        .input_devices()
        .map_err(|e| format!("listing input devices: {e}"))?
        .map(|d| {
            let id = device_id(&d);
            DeviceInfo {
                name: device_name(&d),
                default: id.is_some() && id == default_id,
                config: d.default_input_config().ok().map(|c| {
                    format!(
                        "{} Hz, {} ch, {:?}",
                        c.sample_rate(),
                        c.channels(),
                        c.sample_format()
                    )
                }),
                id,
            }
        })
        .collect();
    Ok((host.id().name().to_owned(), devices))
}

mod app;
mod audio;
#[cfg(feature = "local-whisper")]
mod bench;
mod cleanup;
mod config;
mod controller;
mod injection;
mod ipc;
mod probe;
mod transcription;
mod ui;

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

use crate::config::{Config, TranscriptionBackend, WhisperDevice};
use crate::controller::Command;

/// Wayland-native dictation overlay.
#[derive(Debug, Parser)]
#[command(version, about)]
struct Cli {
    /// Config file (default: $XDG_CONFIG_HOME/wayland-wispr/config.toml).
    /// The dev shell points this at the repo's dev/config.toml.
    #[arg(long, global = true, env = "WAYLAND_WISPR_CONFIG")]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: Cmd,
}

#[derive(Debug, Subcommand)]
enum Cmd {
    /// Run the resident process with the status overlay.
    Run,
    /// Start recording, or stop and process the current recording.
    Toggle,
    /// Start recording (e.g. push-to-talk key press).
    Start,
    /// Stop recording and process it (e.g. push-to-talk key release).
    Stop,
    /// Discard the active recording, processing, or ready result.
    Cancel,
    /// Print machine-readable state of the running instance as JSON.
    Status,
    /// Copy the ready result to the clipboard (native data-control).
    Copy {
        /// Use the raw transcript instead of the LLM-cleaned text.
        #[arg(long)]
        raw: bool,
    },
    /// Copy the ready result and type it (or send the paste chord) into the focused application.
    Insert {
        /// Use the raw transcript instead of the LLM-cleaned text.
        #[arg(long)]
        raw: bool,
    },
    /// Clean up a text with the configured LLM (argument or stdin) and print
    /// the result; timing and outcome go to stderr. Works with cleanup disabled.
    Cleanup {
        text: Option<String>,
        #[command(flatten)]
        overrides: CleanupArgs,
    },
    /// Run cleanup on every case of a fixtures file (e.g.
    /// dev/cleanup-fixtures.toml) and report results and latency.
    CleanupEval {
        fixtures: PathBuf,
        #[command(flatten)]
        overrides: CleanupArgs,
    },
    /// List microphones (input devices) of the configured audio host.
    Devices {
        /// Print JSON instead of a list.
        #[arg(long)]
        json: bool,
    },
    /// Validate the config file and report which whisper model it resolves
    /// to. Fails only on config errors; a missing model is reported.
    CheckConfig,
    /// Report native Wayland protocol support of the current compositor.
    Probe {
        /// Print JSON instead of a table.
        #[arg(long)]
        json: bool,
    },
    /// Measure the configured local whisper model on a WAV file: load time,
    /// latency, RAM and VRAM use. Prints the transcript to stdout.
    Bench {
        /// Any PCM WAV file (e.g. from debug.save_recordings_dir).
        wav: PathBuf,
        /// Number of transcription runs after loading.
        #[arg(long, default_value_t = 3)]
        runs: u32,
        /// Override transcription.whisper.model (a name such as `small-q5_1`).
        #[arg(long)]
        model: Option<String>,
        /// Override the model with an explicit file.
        #[arg(long, conflicts_with = "model")]
        model_path: Option<PathBuf>,
        /// Override transcription.whisper.device.
        #[arg(long, value_parser = ["cpu", "gpu"])]
        device: Option<String>,
        /// Override transcription.whisper.beam_size.
        #[arg(long)]
        beam_size: Option<u8>,
        /// Override the language (code or `auto`; clears `languages`).
        #[arg(long)]
        language: Option<String>,
        /// Override the CPU thread count.
        #[arg(long)]
        threads: Option<u16>,
    },
}

/// Overrides of `[cleanup]` for the cleanup commands.
#[derive(Debug, clap::Args)]
struct CleanupArgs {
    /// Override cleanup.model (e.g. `qwen2.5:7b-instruct`).
    #[arg(long)]
    model: Option<String>,
    /// Override cleanup.base_url.
    #[arg(long)]
    base_url: Option<String>,
    /// Override cleanup.reasoning_effort (`none` for Qwen3.x).
    #[arg(long)]
    reasoning_effort: Option<String>,
    /// Override cleanup.prompt_file.
    #[arg(long)]
    prompt_file: Option<PathBuf>,
}

impl From<CleanupArgs> for cleanup::eval::Overrides {
    fn from(a: CleanupArgs) -> Self {
        Self {
            model: a.model,
            base_url: a.base_url,
            reasoning_effort: a.reasoning_effort,
            prompt_file: a.prompt_file,
        }
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    // whisper.cpp logs every buffer allocation at info level; keep that
    // available via RUST_LOG=whisper_rs=info but quiet by default.
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("info,whisper_rs=warn")),
        )
        .with_writer(std::io::stderr)
        .init();

    match run(cli) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> anyhow::Result<ExitCode> {
    let command = match cli.command {
        Cmd::Run => {
            let config = Config::load(cli.config.as_deref())?;
            app::run(config)?;
            return Ok(ExitCode::SUCCESS);
        }
        Cmd::Probe { json } => return probe(json),
        Cmd::CheckConfig => {
            let config = Config::load(cli.config.as_deref())?;
            check_config(&config);
            return Ok(ExitCode::SUCCESS);
        }
        Cmd::Bench {
            wav,
            runs,
            model,
            model_path,
            device,
            beam_size,
            language,
            threads,
        } => {
            let config = Config::load(cli.config.as_deref())?;
            let device = device.map(|d| match d.as_str() {
                "gpu" => WhisperDevice::Gpu,
                _ => WhisperDevice::Cpu,
            });
            #[cfg(feature = "local-whisper")]
            {
                bench::run(
                    &config.transcription.whisper,
                    &wav,
                    runs,
                    bench::Overrides {
                        model,
                        model_path,
                        device,
                        beam_size,
                        language,
                        threads,
                    },
                )?;
                return Ok(ExitCode::SUCCESS);
            }
            #[cfg(not(feature = "local-whisper"))]
            {
                let _ = (
                    config, wav, runs, model, model_path, device, beam_size, language, threads,
                );
                anyhow::bail!("this build lacks the `local-whisper` feature");
            }
        }
        Cmd::Cleanup { text, overrides } => {
            let config = Config::load(cli.config.as_deref())?;
            return cleanup::eval::once(&config, text, overrides.into());
        }
        Cmd::CleanupEval {
            fixtures,
            overrides,
        } => {
            let config = Config::load(cli.config.as_deref())?;
            return cleanup::eval::eval(&config, &fixtures, overrides.into());
        }
        Cmd::Devices { json } => {
            let config = Config::load(cli.config.as_deref())?;
            return devices(&config, json);
        }
        Cmd::Toggle => Command::Toggle,
        Cmd::Start => Command::Start,
        Cmd::Stop => Command::Stop,
        Cmd::Cancel => Command::Cancel,
        Cmd::Status => Command::Status,
        Cmd::Copy { raw: false } => Command::Copy,
        Cmd::Copy { raw: true } => Command::CopyRaw,
        Cmd::Insert { raw: false } => Command::Insert,
        Cmd::Insert { raw: true } => Command::InsertRaw,
    };

    let response = ipc::send(&ipc::socket_path()?, command)?;
    if command == Command::Status {
        println!("{}", serde_json::to_string(&response)?);
    } else if let Some(msg) = &response.message {
        if response.ok {
            println!("{msg}");
        } else {
            eprintln!("error: {msg}");
        }
    }
    Ok(if response.ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

fn devices(config: &Config, json: bool) -> anyhow::Result<ExitCode> {
    let (host, devices) =
        audio::capture::list_devices(&config.audio).map_err(anyhow::Error::msg)?;
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({ "host": host, "devices": devices }))?
        );
        return Ok(ExitCode::SUCCESS);
    }
    println!("audio host: {host}");
    if devices.is_empty() {
        println!("  (no input devices)");
    }
    for d in &devices {
        println!("{} {}", if d.default { "*" } else { " " }, d.name);
        if let Some(id) = &d.id {
            println!("    id:     {id}");
        }
        if let Some(cfg) = &d.config {
            println!("    format: {cfg}");
        }
    }
    println!();
    println!("* = default. Select with `[audio] device = \"<name or id>\"`.");
    Ok(ExitCode::SUCCESS)
}

fn check_config(config: &Config) {
    println!("config ok");
    if config.transcription.backend != TranscriptionBackend::Whisper {
        return;
    }
    #[cfg(feature = "local-whisper")]
    match transcription::model::locate(&config.transcription.whisper) {
        Ok(m) => println!(
            "whisper model: {} ({} MiB)",
            m.path.display(),
            m.bytes / (1 << 20)
        ),
        Err(e) => println!("whisper model: {e}"),
    }
    #[cfg(not(feature = "local-whisper"))]
    println!("whisper model: this build lacks the `local-whisper` feature");
}

fn probe(json: bool) -> anyhow::Result<ExitCode> {
    let caps = probe::probe()?;
    if json {
        println!("{}", serde_json::to_string_pretty(&caps)?);
    } else {
        for (name, version) in caps.rows() {
            match version {
                Some(v) => println!("  ok       {name} (v{v})"),
                None => println!("  missing  {name}"),
            }
        }
        println!("  {:<8} wl_seat x{}", "", caps.seats);
        println!();
        println!(
            "overlay:   {}",
            if caps.layer_shell.is_some() {
                "supported"
            } else {
                "unsupported"
            }
        );
        println!(
            "insertion: {}",
            if caps.can_insert() {
                "native clipboard + virtual-keyboard paste possible"
            } else {
                "unavailable (needs data-control, virtual-keyboard, and a seat)"
            }
        );
    }
    Ok(if caps.layer_shell.is_some() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

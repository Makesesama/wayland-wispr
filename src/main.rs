mod app;
mod config;
mod controller;
mod ipc;
mod probe;
mod ui;

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

use crate::config::Config;
use crate::controller::Command;

/// Wayland-native dictation overlay.
#[derive(Debug, Parser)]
#[command(version, about)]
struct Cli {
    /// Config file (default: $XDG_CONFIG_HOME/wayland-wispr/config.toml).
    #[arg(long, global = true)]
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
    /// Copy the ready result to the clipboard (not implemented yet).
    Copy,
    /// Insert the ready result into the focused application (not implemented yet).
    Insert,
    /// Report native Wayland protocol support of the current compositor.
    Probe {
        /// Print JSON instead of a table.
        #[arg(long)]
        json: bool,
    },
}

fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();
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
        Cmd::Toggle => Command::Toggle,
        Cmd::Start => Command::Start,
        Cmd::Stop => Command::Stop,
        Cmd::Cancel => Command::Cancel,
        Cmd::Status => Command::Status,
        Cmd::Copy => Command::Copy,
        Cmd::Insert => Command::Insert,
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

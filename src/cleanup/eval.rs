//! `cleanup` and `cleanup-eval` commands: run the configured cleanup outside
//! the resident process to compare models and tune the prompt without a
//! microphone.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use anyhow::Context;
use serde::Deserialize;

use super::{Cleaner, CleanupStatus, Processed};
use crate::config::Config;
use crate::transcription::CancelFlag;

/// Command-line overrides of `[cleanup]`.
#[derive(Debug, Default)]
pub struct Overrides {
    pub model: Option<String>,
    pub base_url: Option<String>,
    pub reasoning_effort: Option<String>,
    pub prompt_file: Option<PathBuf>,
}

fn cleaner(config: &Config, o: Overrides) -> anyhow::Result<Cleaner> {
    let mut config = config.clone();
    let c = &mut config.cleanup;
    c.enabled = true;
    c.preload = false;
    if let Some(v) = o.model {
        c.model = v;
    }
    if let Some(v) = o.base_url {
        c.base_url = v;
    }
    if let Some(v) = o.reasoning_effort {
        c.reasoning_effort = Some(v);
    }
    if let Some(v) = o.prompt_file {
        c.prompt_file = Some(v);
    }
    config.validate()?;
    eprintln!(
        "model {} at {}",
        config.cleanup.model, config.cleanup.base_url
    );
    Cleaner::new(&config.cleanup)
}

/// Clean one text (argument or stdin) and print the result to stdout.
pub fn once(config: &Config, text: Option<String>, o: Overrides) -> anyhow::Result<ExitCode> {
    let text = match text {
        Some(t) => t,
        None => {
            let mut t = String::new();
            std::io::stdin()
                .take(1 << 20)
                .read_to_string(&mut t)
                .context("reading stdin")?;
            t
        }
    };
    let text = text.trim().to_owned();
    anyhow::ensure!(!text.is_empty(), "no text to clean up");
    let cleaner = cleaner(config, o)?;
    let started = Instant::now();
    let p = cleaner
        .run(text, &CancelFlag::default(), || {})
        .map_err(|_| anyhow::anyhow!("cancelled"))?;
    println!("{}", p.text);
    eprintln!("{} in {} ms", outcome(&p), started.elapsed().as_millis());
    Ok(if p.cleanup == CleanupStatus::Failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Fixtures {
    #[serde(rename = "case")]
    cases: Vec<Case>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Case {
    input: String,
    /// Expected result; optional because several outputs can be right.
    expect: Option<String>,
    note: Option<String>,
}

/// Run every case of a fixtures file and print results plus a summary.
pub fn eval(config: &Config, path: &Path, o: Overrides) -> anyhow::Result<ExitCode> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let fixtures: Fixtures =
        toml::from_str(&text).with_context(|| format!("invalid fixtures {}", path.display()))?;
    let cleaner = cleaner(config, o)?;

    match cleaner.preload_blocking() {
        Ok(took) => println!("model load + prompt prefix: {} ms\n", took.as_millis()),
        Err(e) => anyhow::bail!("the cleanup server is not usable: {e}"),
    }

    let (mut latencies, mut failed, mut matched, mut expected) = (Vec::new(), 0, 0, 0);
    for (i, case) in fixtures.cases.iter().enumerate() {
        let started = Instant::now();
        let p = cleaner
            .run(case.input.trim().to_owned(), &CancelFlag::default(), || {})
            .map_err(|_| anyhow::anyhow!("cancelled"))?;
        let took = started.elapsed();
        if matches!(p.cleanup, CleanupStatus::Cleaned | CleanupStatus::Failed) {
            latencies.push(took);
        }
        if p.cleanup == CleanupStatus::Failed {
            failed += 1;
        }
        let verdict = match &case.expect {
            Some(want) => {
                expected += 1;
                if p.text == want.trim() {
                    matched += 1;
                    "  ✓ expected"
                } else {
                    "  ✗ differs"
                }
            }
            None => "",
        };
        println!(
            "#{} {}{} ms  {}{verdict}",
            i + 1,
            case.note
                .as_deref()
                .map(|n| format!("{n}  "))
                .unwrap_or_default(),
            took.as_millis(),
            outcome(&p),
        );
        print_field("in", &case.input);
        print_field("out", &p.text);
        if let Some(want) = &case.expect
            && p.text != want.trim()
        {
            print_field("want", want);
        }
        println!();
    }

    latencies.sort();
    let ms = |d: Duration| d.as_millis();
    println!(
        "{} cases, {} failed, {matched}/{expected} as expected",
        fixtures.cases.len(),
        failed
    );
    if let (Some(min), Some(max)) = (latencies.first(), latencies.last()) {
        let total: Duration = latencies.iter().sum();
        println!(
            "latency: min {} ms, median {} ms, mean {} ms, max {} ms",
            ms(*min),
            ms(latencies[latencies.len() / 2]),
            ms(total / latencies.len() as u32),
            ms(*max)
        );
    }
    Ok(if failed == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

fn outcome(p: &Processed) -> String {
    match (p.cleanup, &p.cleanup_error) {
        (CleanupStatus::Failed, Some(e)) => format!("FAILED ({e}), raw transcript kept"),
        (CleanupStatus::Cleaned, _) if p.raw.is_none() => "unchanged".into(),
        (CleanupStatus::Cleaned, _) => "cleaned".into(),
        (CleanupStatus::Skipped, _) => "skipped (cleanup.min_words)".into(),
        (status, _) => format!("{status:?}").to_lowercase(),
    }
}

fn print_field(name: &str, text: &str) {
    for (i, line) in text.trim().lines().enumerate() {
        let label = if i == 0 { name } else { "" };
        println!("  {label:<5} {line}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dev_fixtures_parse() {
        let f: Fixtures = toml::from_str(include_str!("../../dev/cleanup-fixtures.toml")).unwrap();
        assert!(f.cases.len() >= 10);
        assert!(f.cases.iter().all(|c| !c.input.trim().is_empty()));
    }
}

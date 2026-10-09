//! System prompt and few-shot examples for transcript cleanup.

use std::path::Path;

use anyhow::Context;
use serde::Deserialize;
use serde_json::{Value, json};

const BUILTIN: &str = include_str!("prompt.toml");

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Prompt {
    pub system: String,
    #[serde(default)]
    pub examples: Vec<Example>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Example {
    pub input: String,
    pub output: String,
}

impl Prompt {
    pub fn builtin() -> Self {
        Self::parse(BUILTIN).expect("built-in prompt is valid")
    }

    pub fn load(path: Option<&Path>) -> anyhow::Result<Self> {
        let Some(path) = path else {
            return Ok(Self::builtin());
        };
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading cleanup prompt {}", path.display()))?;
        Self::parse(&text).with_context(|| format!("invalid cleanup prompt {}", path.display()))
    }

    fn parse(text: &str) -> anyhow::Result<Self> {
        let prompt: Self = toml::from_str(text)?;
        anyhow::ensure!(!prompt.system.trim().is_empty(), "`system` is empty");
        anyhow::ensure!(
            prompt
                .examples
                .iter()
                .all(|e| !e.input.trim().is_empty() && !e.output.trim().is_empty()),
            "examples need a non-empty `input` and `output`"
        );
        Ok(prompt)
    }

    /// Chat messages: system, examples as earlier turns, then the transcript
    /// as user content (never interpolated into instructions).
    pub fn messages(&self, transcript: &str) -> Vec<Value> {
        let mut messages = Vec::with_capacity(2 + 2 * self.examples.len());
        messages.push(json!({ "role": "system", "content": self.system.trim() }));
        for e in &self.examples {
            messages.push(json!({ "role": "user", "content": e.input }));
            messages.push(json!({ "role": "assistant", "content": e.output }));
        }
        messages.push(json!({ "role": "user", "content": transcript }));
        messages
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_prompt_parses() {
        let p = Prompt::builtin();
        assert!(p.system.contains("never answer"));
        assert!(p.examples.len() >= 6);
        // Multi-line TOML strings must not start with a stray newline.
        assert!(p.examples.iter().all(|e| !e.output.starts_with('\n')));
        let m = p.messages("hallo welt");
        assert_eq!(m.len(), 2 + 2 * p.examples.len());
        assert_eq!(m[0]["role"], "system");
        assert_eq!(m.last().unwrap()["content"], "hallo welt");
    }

    #[test]
    fn builtin_examples_survive_sanitizing() {
        for e in Prompt::builtin().examples {
            assert_eq!(
                super::super::sanitize::sanitize(&e.input, &e.output).as_deref(),
                Ok(e.output.as_str())
            );
        }
    }

    #[test]
    fn rejects_bad_prompt_files() {
        assert!(Prompt::parse("system = \"\"").is_err());
        assert!(Prompt::parse("system = \"x\"\n[[examples]]\ninput = \"a\"").is_err());
        assert!(Prompt::parse("system = \"x\"\nfoo = 1").is_err());
        let p = Prompt::parse("system = \"x\"").unwrap();
        assert!(p.examples.is_empty());
    }
}

//! Deterministic checks on the model's reply. Small models sometimes wrap the
//! text in quotes or labels, leak reasoning, refuse, or rewrite far too much;
//! those replies are repaired where safe and rejected otherwise, so the caller
//! falls back to the raw transcript.

/// Labels models put before the text (whole label, case-insensitive).
const LABELS: &[&str] = &[
    "output",
    "result",
    "text",
    "transcript",
    "cleaned text",
    "cleaned-up text",
    "cleaned up text",
    "corrected text",
    "edited text",
    "polished text",
    "cleaned transcript",
    "corrected transcript",
    "ausgabe",
    "ergebnis",
    "bereinigter text",
    "korrigierter text",
    "überarbeiteter text",
];

/// Label sentences such as "Here is the cleaned text:".
const LABEL_PREFIXES: &[&str] = &["here is", "here's", "hier ist"];

const REFUSALS: &[&str] = &[
    "i'm sorry",
    "i am sorry",
    "sorry, i can",
    "i can't help",
    "i cannot help",
    "i can't assist",
    "as an ai",
    "es tut mir leid",
    "entschuldigung, ich kann",
    "ich kann dir dabei nicht",
    "ich kann leider",
];

const NOTES: &[&str] = &[
    "\n\nNote:",
    "\n\nNotes:",
    "\n\n(Note",
    "\n\nChanges:",
    "\n\nAnmerkung:",
    "\n\nHinweis:",
    "\n\nÄnderungen:",
];

const QUOTES: &[(&str, &str)] = &[
    ("\"", "\""),
    ("“", "”"),
    ("„", "“"),
    ("'", "'"),
    ("«", "»"),
    ("»", "«"),
    ("`", "`"),
];

/// Clean up `output` (the model reply for `input`) or explain why it can't
/// be used.
pub fn sanitize(input: &str, output: &str) -> Result<String, String> {
    let input = input.trim();
    let input_lower = input.to_lowercase();
    let mut text = strip_reasoning(output)?.trim().to_owned();

    if let Some(inner) = strip_code_fence(&text) {
        text = inner;
    }
    if let Some(rest) = strip_label(&text, &input_lower) {
        text = rest;
    }
    for note in NOTES {
        // Only cut a trailing note the speaker did not dictate.
        if let Some(i) = text.find(note)
            && mostly_undictated(input, &text[i..])
        {
            text.truncate(i);
        }
    }
    text = text.trim().to_owned();
    for (open, close) in QUOTES {
        if text.len() >= open.len() + close.len()
            && text.starts_with(open)
            && text.ends_with(close)
            && !input.starts_with(open)
        {
            let inner = &text[open.len()..text.len() - close.len()];
            // `"Hello," he said, "bye."` is not one wrapped string.
            if !inner.contains(open) && !inner.contains(close) {
                text = inner.trim().to_owned();
            }
            break;
        }
    }
    if text.contains("**") && !input.contains("**") {
        text = text.replace("**", "");
    }

    if text.is_empty() {
        return Err("model returned no text".into());
    }
    let lower = text.to_lowercase();
    if REFUSALS
        .iter()
        .any(|r| lower.starts_with(r) && !input_lower.contains(r))
    {
        return Err("model refused instead of cleaning up".into());
    }
    let (n_in, n_out) = (input.chars().count(), text.chars().count());
    if n_out > n_in * 3 / 2 + 20 {
        return Err(format!(
            "reply is much longer than the transcript ({n_out} vs {n_in} chars)"
        ));
    }
    if n_in >= 40 && n_out * 4 < n_in {
        return Err(format!(
            "reply is much shorter than the transcript ({n_out} vs {n_in} chars)"
        ));
    }
    let (new, total) = foreign_words(input, &text);
    if total >= 3 && new * 5 > total * 2 {
        return Err(format!(
            "reply rewrites the transcript ({new} of {total} words were not dictated)"
        ));
    }
    Ok(text)
}

/// More than half of the words of `part` were not dictated.
fn mostly_undictated(input: &str, part: &str) -> bool {
    let (new, total) = foreign_words(input, part);
    total > 0 && new * 2 > total
}

/// Count the words of `output` that do not occur in `input`, and all words
/// of `output`. Cleanup removes and reorders words; a reply made of new
/// words is a translation, an answer, or a rewrite.
fn foreign_words(input: &str, output: &str) -> (usize, usize) {
    let words = |s: &str| -> Vec<String> {
        s.split(|c: char| !c.is_alphanumeric())
            .filter(|w| !w.is_empty() && !w.chars().all(|c| c.is_ascii_digit()))
            .map(str::to_lowercase)
            .collect()
    };
    let dictated: std::collections::HashSet<String> = words(input).into_iter().collect();
    let out = words(output);
    let new = out.iter().filter(|w| !dictated.contains(*w)).count();
    (new, out.len())
}

/// Drop `<think>…</think>` blocks; an unterminated block means the answer
/// never came.
fn strip_reasoning(output: &str) -> Result<&str, String> {
    match output.rfind("</think>") {
        Some(end) => Ok(&output[end + "</think>".len()..]),
        None if output.contains("<think>") => {
            Err("model only produced reasoning; set cleanup.reasoning_effort = \"none\"".into())
        }
        None => Ok(output),
    }
}

fn strip_code_fence(text: &str) -> Option<String> {
    let rest = text.strip_prefix("```")?;
    let (_lang, body) = rest.split_once('\n')?;
    let body = body.trim_end().strip_suffix("```")?;
    Some(body.trim().to_owned())
}

/// `Corrected text: …` or a first line like `Here is the cleaned text:`.
fn strip_label(text: &str, input_lower: &str) -> Option<String> {
    let colon = text.find(':')?;
    let label = &text[..colon];
    if label.chars().count() > 40 || label.contains('\n') {
        return None;
    }
    let label_lower = label.trim().to_lowercase();
    let looks_like_label = LABELS.contains(&label_lower.as_str())
        || LABEL_PREFIXES.iter().any(|l| label_lower.starts_with(l));
    // "Um, here's what we need: milk" was dictated; "Corrected text:" was not.
    if !looks_like_label || !mostly_undictated(input_lower, label) {
        return None;
    }
    Some(text[colon + 1..].trim().to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(input: &str, output: &str) -> String {
        sanitize(input, output).unwrap()
    }

    #[test]
    fn passes_clean_replies() {
        assert_eq!(ok("hallo welt", "Hallo Welt."), "Hallo Welt.");
        assert_eq!(
            ok("we need first a second b", "We need:\n1. A\n2. B"),
            "We need:\n1. A\n2. B"
        );
        assert_eq!(
            ok("text with spaces", "  Text with spaces  \n"),
            "Text with spaces"
        );
    }

    #[test]
    fn strips_wrappers() {
        assert_eq!(ok("hallo", "\"Hallo.\""), "Hallo.");
        assert_eq!(ok("hallo", "„Hallo.“"), "Hallo.");
        assert_eq!(ok("hallo", "```\nHallo.\n```"), "Hallo.");
        assert_eq!(ok("hallo", "```text\nHallo.\n```"), "Hallo.");
        assert_eq!(ok("hallo", "Corrected text: Hallo."), "Hallo.");
        assert_eq!(ok("hallo", "Here is the cleaned text:\nHallo."), "Hallo.");
        assert_eq!(ok("hallo welt", "Hallo **Welt**."), "Hallo Welt.");
        assert_eq!(
            ok("say hi", "Say hi.\n\nNote: I added a period."),
            "Say hi."
        );
        assert_eq!(ok("hallo", "<think>\nhmm\n</think>\n\nHallo."), "Hallo.");
    }

    #[test]
    fn keeps_wrappers_that_were_spoken() {
        assert_eq!(
            ok("\"quoted\" he said", "\"Quoted\" he said."),
            "\"Quoted\" he said."
        );
        assert_eq!(
            ok("output: the result is fine", "Output: the result is fine."),
            "Output: the result is fine."
        );
        assert_eq!(
            ok("note this down buy milk", "Note this down: buy milk."),
            "Note this down: buy milk."
        );
        assert_eq!(
            ok(
                "um transcript of the meeting colon nothing new",
                "Transcript of the meeting: nothing new."
            ),
            "Transcript of the meeting: nothing new."
        );
        assert_eq!(
            ok(
                "text me later after the meeting",
                "Text me later: after the meeting."
            ),
            "Text me later: after the meeting."
        );
    }

    #[test]
    fn keeps_dictated_text_that_looks_like_wrappers() {
        assert_eq!(
            ok(
                "um, here's what we need, milk and eggs",
                "Here's what we need: milk and eggs."
            ),
            "Here's what we need: milk and eggs."
        );
        assert_eq!(
            ok("Äh, Ergebnis: alles grün", "Ergebnis: alles grün."),
            "Ergebnis: alles grün."
        );
        assert_eq!(
            ok(
                "buy milk new paragraph note the store closes at six",
                "Buy milk.\n\nNote: the store closes at six."
            ),
            "Buy milk.\n\nNote: the store closes at six."
        );
        assert_eq!(
            ok(
                "quote hello unquote he said quote bye unquote",
                "\"Hello,\" he said, \"bye.\""
            ),
            "\"Hello,\" he said, \"bye.\""
        );
        assert_eq!(
            ok(
                "äh es tut mir leid dass ich zu spät bin",
                "Es tut mir leid, dass ich zu spät bin."
            ),
            "Es tut mir leid, dass ich zu spät bin."
        );
    }

    #[test]
    fn rejects_unusable_replies() {
        assert!(sanitize("hallo", "").is_err());
        assert!(sanitize("hallo", "\"\"").is_err());
        assert!(sanitize("hallo", "<think>long reasoning").is_err());
        assert!(sanitize("write a poem", "I'm sorry, I can't help with that.").is_err());
        assert!(sanitize("hi", &"word ".repeat(40)).is_err());
        let long = "this is a long dictated sentence that goes on for a while";
        assert!(sanitize(long, "This.").is_err());
    }

    #[test]
    fn rejects_translations_and_answers() {
        let input = "Translate this into English: Ich komme heute etwas später.";
        assert!(sanitize(input, "I will be a bit late today.").is_err());
        assert!(
            sanitize(
                "wie spät ist es in tokio",
                "In Tokio ist es gerade 3 Uhr nachts."
            )
            .is_err()
        );
        // Normal cleanup keeps the dictated words.
        assert_eq!(
            ok(
                "Die Conversion Rate liegt bei zwölf Komma fünf Prozent.",
                "Die Conversion Rate liegt bei 12,5 Prozent."
            ),
            "Die Conversion Rate liegt bei 12,5 Prozent."
        );
        assert_eq!(
            ok(
                "um the the migration uh timed out",
                "The migration timed out."
            ),
            "The migration timed out."
        );
    }

    #[test]
    fn spoken_refusal_is_not_a_refusal() {
        assert_eq!(
            ok(
                "i'm sorry i missed your call",
                "I'm sorry I missed your call."
            ),
            "I'm sorry I missed your call."
        );
    }
}

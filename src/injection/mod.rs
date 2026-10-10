//! Text insertion: committing text as the seat's input method (default),
//! or the native data-control clipboard plus either direct typing or a paste
//! chord through a native virtual keyboard. No subprocess, X11, or uinput
//! fallbacks.

pub mod chord;
mod clipboard;
mod input_method;
pub mod typing;
mod virtual_keyboard;

use std::thread;
use std::time::Duration;

use crate::config::{InjectionConfig, InsertMethod};
use crate::controller::{Failure, FailureKind};
use chord::Chord;
use input_method::{ImError, InputMethod};
use typing::Newline;
use virtual_keyboard::{Batch, Timing};

#[derive(Debug)]
enum Method {
    Commit { im: InputMethod, newline: Newline },
    Type { newline: Newline, gap: Duration },
    Paste { chord: Chord, batch: Batch },
}

#[derive(Debug)]
pub struct Injector {
    method: Method,
    pre_insert_delay: Duration,
    key_delay: Duration,
}

impl Injector {
    pub fn new(config: &InjectionConfig) -> anyhow::Result<Self> {
        let method = match config.method {
            InsertMethod::InputMethod => Method::Commit {
                im: InputMethod::start()?,
                newline: config.newline,
            },
            InsertMethod::Type => {
                // Fail at startup rather than on the first insertion if the
                // typing keymap cannot be compiled.
                typing::plan("a", config.newline)?;
                Method::Type {
                    newline: config.newline,
                    gap: Duration::from_millis(config.type_delay_ms),
                }
            }
            InsertMethod::Paste => {
                let chord: Chord = config.paste_chord.parse()?;
                let batch = Batch::from(&chord::plan(&chord)?);
                Method::Paste { chord, batch }
            }
        };
        Ok(Self {
            method,
            pre_insert_delay: Duration::from_millis(config.pre_insert_delay_ms),
            key_delay: Duration::from_millis(config.key_delay_ms),
        })
    }

    /// Put `text` on the clipboard. Blocking; run off the GTK and
    /// controller threads.
    pub fn copy(&self, text: &str) -> Result<(), String> {
        clipboard::set_text(text)
    }

    /// Insert `text` into the focused application. Blocking; run off the
    /// GTK and controller threads.
    ///
    /// The input method commits directly and leaves the clipboard alone,
    /// unless no text field is focused: then the text is copied instead and
    /// the failure is [`FailureKind::NoTextField`]. Typing and pasting copy
    /// first, then send keys to whatever is focused.
    pub fn insert(&self, text: &str) -> Result<(), Failure> {
        match &self.method {
            Method::Commit { im, newline } => commit(im, &input_method_text(text, *newline)),
            Method::Type { newline, gap } => self.copy_then(text, |text| {
                let batches = typing::plan(text, *newline)
                    .map_err(|e| format!("copied to clipboard, but typing failed: {e}"))?;
                let timing = Timing {
                    settle: self.key_delay,
                    hold: Duration::ZERO,
                    gap: *gap,
                };
                virtual_keyboard::send(&batches, timing)
                    .map_err(|e| format!("copied to clipboard, but typing failed: {e}"))
            }),
            Method::Paste { chord, batch } => self.copy_then(text, |_| {
                let timing = Timing {
                    settle: self.key_delay,
                    hold: self.key_delay,
                    gap: Duration::ZERO,
                };
                virtual_keyboard::send(std::slice::from_ref(batch), timing)
                    .map_err(|e| format!("copied to clipboard, but sending {chord} failed: {e}"))
            }),
        }
    }

    /// Copy `text`, wait for the triggering keybinding to be released, then
    /// send keys.
    fn copy_then(
        &self,
        text: &str,
        send: impl FnOnce(&str) -> Result<(), String>,
    ) -> Result<(), Failure> {
        clipboard::set_text(text)?;
        thread::sleep(self.pre_insert_delay);
        send(text).map_err(Into::into)
    }
}

/// Commit through the input method. Without a text field to commit to, the
/// text goes to the clipboard and stays ready for another attempt.
fn commit(im: &InputMethod, text: &str) -> Result<(), Failure> {
    let e = match im.commit(text) {
        Ok(()) => return Ok(()),
        Err(e @ (ImError::NoTextField | ImError::Unavailable)) => e,
        Err(e) => return Err(e.to_string().into()),
    };
    let kind = match e {
        ImError::NoTextField => FailureKind::NoTextField,
        _ => FailureKind::Failed,
    };
    let message = match clipboard::set_text(text) {
        Ok(()) => format!("{e}; copied to the clipboard"),
        Err(c) => format!("{e}; {c}"),
    };
    Err(Failure {
        message,
        detail: None,
        kind,
    })
}

/// Text as committed by the input method. It arrives verbatim, so line
/// breaks cannot be typed as Shift+Return: they stay `\n` (a line break in
/// text fields, Return in terminals) unless `newline = "space"`. Other
/// control characters are dropped.
fn input_method_text(text: &str, newline: Newline) -> String {
    let text = text.replace("\r\n", "\n").replace('\r', "\n");
    text.chars()
        .filter_map(|c| match c {
            '\n' if newline == Newline::Space => Some(' '),
            '\n' | '\t' => Some(c),
            c if c.is_control() => None,
            c => Some(c),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn input_method_text_normalizes_line_breaks_and_controls() {
        let text = "a\r\nb\tc\u{7}d\re";
        assert_eq!(input_method_text(text, Newline::ShiftEnter), "a\nb\tcd\ne");
        assert_eq!(input_method_text(text, Newline::Enter), "a\nb\tcd\ne");
        assert_eq!(input_method_text(text, Newline::Space), "a b\tcd e");
        assert_eq!(input_method_text("Grüße 👋", Newline::Space), "Grüße 👋");
    }
}

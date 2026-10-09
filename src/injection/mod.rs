//! Text insertion: native data-control clipboard plus either direct typing
//! or a paste chord through a native virtual keyboard. No subprocess, X11,
//! or uinput fallbacks.

pub mod chord;
mod clipboard;
pub mod typing;
mod virtual_keyboard;

use std::thread;
use std::time::Duration;

use crate::config::{InjectionConfig, InsertMethod};
use chord::Chord;
use typing::Newline;
use virtual_keyboard::{Batch, Timing};

#[derive(Debug)]
enum Method {
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

    /// Copy `text`, then type or paste it into the focused application.
    /// Blocking; run off the GTK and controller threads.
    pub fn insert(&self, text: &str) -> Result<(), String> {
        clipboard::set_text(text)?;

        // Give the user a moment to release the physical keybinding that
        // triggered insertion.
        thread::sleep(self.pre_insert_delay);
        match &self.method {
            Method::Type { newline, gap } => {
                let batches = typing::plan(text, *newline)
                    .map_err(|e| format!("copied to clipboard, but typing failed: {e}"))?;
                let timing = Timing {
                    settle: self.key_delay,
                    hold: Duration::ZERO,
                    gap: *gap,
                };
                virtual_keyboard::send(&batches, timing)
                    .map_err(|e| format!("copied to clipboard, but typing failed: {e}"))
            }
            Method::Paste { chord, batch } => {
                let timing = Timing {
                    settle: self.key_delay,
                    hold: self.key_delay,
                    gap: Duration::ZERO,
                };
                virtual_keyboard::send(std::slice::from_ref(batch), timing)
                    .map_err(|e| format!("copied to clipboard, but sending {chord} failed: {e}"))
            }
        }
    }
}

//! Text insertion: native data-control clipboard + virtual-keyboard paste
//! chord. No subprocess, X11, or uinput fallbacks.

pub mod chord;
mod clipboard;
mod virtual_keyboard;

use std::thread;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::config::InjectionConfig;
use chord::{Chord, ChordPlan};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    /// Put the result on the clipboard; the user pastes.
    Copy,
    /// Put the result on the clipboard and send the paste chord.
    Insert,
}

#[derive(Debug)]
pub struct Injector {
    chord: Chord,
    plan: ChordPlan,
    pre_insert_delay: Duration,
    key_delay: Duration,
}

impl Injector {
    pub fn new(config: &InjectionConfig) -> anyhow::Result<Self> {
        let chord: Chord = config.paste_chord.parse()?;
        let plan = chord::plan(&chord)?;
        Ok(Self {
            chord,
            plan,
            pre_insert_delay: Duration::from_millis(config.pre_insert_delay_ms),
            key_delay: Duration::from_millis(config.key_delay_ms),
        })
    }

    /// Blocking; run off the GTK and controller threads.
    pub fn run(&self, action: Action, text: &str) -> Result<(), String> {
        clipboard::set_text(text)?;
        if action == Action::Copy {
            return Ok(());
        }

        // Give the user a moment to release the physical keybinding that
        // triggered insertion.
        thread::sleep(self.pre_insert_delay);
        virtual_keyboard::send_chord(&self.plan, self.key_delay).map_err(|e| {
            format!(
                "copied to clipboard, but sending {} failed: {e}",
                self.chord
            )
        })
    }
}

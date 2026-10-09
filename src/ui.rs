//! GTK4 layer-shell status indicator. All GTK objects stay on the main thread;
//! state arrives as owned [`View`] values over an async channel.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Instant;

use gtk4::prelude::*;
use gtk4::{gdk, glib};
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};

use crate::cleanup::CleanupStatus;
use crate::config::{Config, OverlayLayer};
use crate::controller::{SessionId, State};
use crate::injection::Action;

const APP_ID: &str = "de.makussu.WaylandWispr";
const PREVIEW_CHARS: usize = 60;

const CSS: &str = r#"
window.wispr {
  background: transparent;
}
.pill {
  background-color: rgba(20, 20, 24, 0.88);
  color: #f2f2f2;
  border-radius: 999px;
  padding: 8px 18px;
}
.dot {
  font-size: 14px;
}
.recording .dot { color: #ff4d4d; }
.processing .dot { color: #f5c542; }
.ready .dot { color: #4dd28a; }
.error .dot { color: #ff7a45; }
.status { font-weight: bold; }
levelbar.meter { min-width: 64px; }
levelbar.meter block.filled { background-color: #ff4d4d; }
levelbar.meter block.empty { background-color: rgba(255, 255, 255, 0.15); }
levelbar.meter trough { min-height: 6px; }
.detail { opacity: 0.75; }
"#;

/// Messages to the GTK thread. Owned and `Send`.
#[derive(Debug, Clone, PartialEq)]
pub enum UiEvent {
    State(View),
    /// Microphone input level; ignored unless that session is recording.
    Level {
        session: SessionId,
        dbfs: f32,
    },
    /// Processing stage label; ignored unless that session is processing.
    Stage {
        session: SessionId,
        label: &'static str,
    },
}

/// What the overlay should display. Owned and `Send`, built off the GTK thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum View {
    Hidden,
    Recording {
        session: SessionId,
    },
    Processing {
        session: SessionId,
    },
    Ready {
        preview: String,
        notice: Option<String>,
        /// Cleanup failed; the result is the raw transcript.
        uncleaned: bool,
    },
    Inserting {
        action: Action,
    },
    Error {
        message: String,
    },
}

impl From<&State> for View {
    fn from(state: &State) -> Self {
        match state {
            State::Idle => View::Hidden,
            State::Recording { session } => View::Recording { session: *session },
            State::Processing { session } => View::Processing { session: *session },
            State::Ready { result, notice, .. } => View::Ready {
                preview: preview(&result.text),
                notice: notice.clone(),
                uncleaned: result.cleanup == CleanupStatus::Failed,
            },
            State::Inserting { action, .. } => View::Inserting { action: *action },
            State::Error { message, .. } => View::Error {
                message: message.clone(),
            },
        }
    }
}

fn preview(text: &str) -> String {
    let flat: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= PREVIEW_CHARS {
        flat
    } else {
        let cut: String = flat.chars().take(PREVIEW_CHARS - 1).collect();
        format!("{cut}…")
    }
}

struct Overlay {
    window: gtk4::Window,
    pill: gtk4::Box,
    status: gtk4::Label,
    detail: gtk4::Label,
    meter: gtk4::LevelBar,
    recording_since: Cell<Option<Instant>>,
    recording_session: Cell<Option<SessionId>>,
    processing_session: Cell<Option<SessionId>>,
}

impl Overlay {
    fn new(app: &gtk4::Application, config: &Config) -> Self {
        let window = gtk4::Window::builder()
            .application(app)
            .title("wayland-wispr")
            .decorated(false)
            .resizable(false)
            .focusable(false)
            .css_classes(["wispr"])
            .build();

        window.init_layer_shell();
        window.set_namespace(Some("wayland-wispr"));
        window.set_layer(match config.overlay.layer {
            OverlayLayer::Overlay => Layer::Overlay,
            OverlayLayer::Top => Layer::Top,
        });
        window.set_anchor(Edge::Bottom, true);
        window.set_margin(Edge::Bottom, config.overlay.margin_bottom);
        // Never take keyboard focus from the application being dictated into,
        // and never reserve screen space.
        window.set_keyboard_mode(KeyboardMode::None);
        window.set_exclusive_zone(0);

        let pill = gtk4::Box::new(gtk4::Orientation::Horizontal, 10);
        pill.add_css_class("pill");
        let dot = gtk4::Label::new(Some("●"));
        dot.add_css_class("dot");
        let status = gtk4::Label::new(None);
        status.add_css_class("status");
        let detail = gtk4::Label::new(None);
        detail.add_css_class("detail");
        detail.set_ellipsize(gtk4::pango::EllipsizeMode::End);
        detail.set_max_width_chars(PREVIEW_CHARS as i32);
        let meter = gtk4::LevelBar::for_interval(0.0, 1.0);
        meter.add_css_class("meter");
        meter.set_valign(gtk4::Align::Center);
        // Drop the default low/high/full colour offsets; one colour is enough.
        for name in ["low", "high", "full"] {
            meter.remove_offset_value(Some(name));
        }
        pill.append(&dot);
        pill.append(&status);
        pill.append(&meter);
        pill.append(&detail);
        window.set_child(Some(&pill));

        Self {
            window,
            pill,
            status,
            detail,
            meter,
            recording_since: Cell::new(None),
            recording_session: Cell::new(None),
            processing_session: Cell::new(None),
        }
    }

    fn apply(&self, view: &View) {
        for class in ["recording", "processing", "ready", "error"] {
            self.pill.remove_css_class(class);
        }
        self.recording_since.set(None);
        self.recording_session.set(None);
        self.processing_session.set(None);
        self.meter.set_value(0.0);
        self.meter
            .set_visible(matches!(view, View::Recording { .. }));

        let (class, status, detail) = match view {
            View::Hidden => {
                self.window.set_visible(false);
                return;
            }
            View::Recording { session } => {
                self.recording_since.set(Some(Instant::now()));
                self.recording_session.set(Some(*session));
                ("recording", "Recording", "0:00".to_owned())
            }
            View::Processing { session } => {
                self.processing_session.set(Some(*session));
                ("processing", "Transcribing…", String::new())
            }
            View::Ready {
                notice: Some(notice),
                ..
            } => ("error", "Insert failed", notice.clone()),
            View::Ready {
                preview,
                uncleaned: true,
                ..
            } => ("ready", "Ready (not cleaned up)", preview.clone()),
            View::Ready { preview, .. } => ("ready", "Ready", preview.clone()),
            View::Inserting {
                action: Action::Copy,
            } => ("processing", "Copying…", String::new()),
            View::Inserting {
                action: Action::Insert,
            } => ("processing", "Pasting…", String::new()),
            View::Error { message } => ("error", "Error", message.clone()),
        };
        self.pill.add_css_class(class);
        self.status.set_text(status);
        self.detail.set_text(&detail);
        self.detail.set_visible(!detail.is_empty());
        self.window.set_visible(true);
    }

    fn level(&self, session: SessionId, dbfs: f32) {
        if self.recording_session.get() != Some(session) {
            return;
        }
        // Map -60..0 dBFS onto the bar, with a quick attack and slow decay.
        let target = f64::from((dbfs + 60.0) / 60.0).clamp(0.0, 1.0);
        let current = self.meter.value();
        let next = if target > current {
            target
        } else {
            (current - 0.05).max(target)
        };
        self.meter.set_value(next);
    }

    fn stage(&self, session: SessionId, label: &str) {
        if self.processing_session.get() == Some(session) {
            self.status.set_text(label);
        }
    }

    fn tick(&self) {
        if let Some(since) = self.recording_since.get() {
            let secs = since.elapsed().as_secs();
            self.detail
                .set_text(&format!("{}:{:02}", secs / 60, secs % 60));
        }
    }
}

/// Run the GTK main loop until the app quits. Returns `false` if the overlay
/// could not be shown (e.g. no layer-shell support).
pub fn run(config: Config, events: async_channel::Receiver<UiEvent>) -> bool {
    let app = gtk4::Application::builder()
        .application_id(APP_ID)
        // Single-instance handling is done by the control socket, not D-Bus.
        .flags(gtk4::gio::ApplicationFlags::NON_UNIQUE)
        .build();

    let failed = Rc::new(Cell::new(false));
    let hold = Rc::new(RefCell::new(None));
    let events = Rc::new(RefCell::new(Some(events)));

    app.connect_activate({
        let failed = failed.clone();
        move |app| {
            // Activation can be requested more than once; set up exactly once.
            let Some(events) = events.borrow_mut().take() else {
                return;
            };

            if !gtk4_layer_shell::is_supported() {
                tracing::error!(
                    "compositor does not support zwlr_layer_shell_v1; the overlay cannot be shown"
                );
                failed.set(true);
                app.quit();
                return;
            }

            install_css();
            *hold.borrow_mut() = Some(app.hold());

            let overlay = Rc::new(Overlay::new(app, &config));
            overlay.apply(&View::Hidden);

            glib::timeout_add_seconds_local(1, {
                let overlay = overlay.clone();
                move || {
                    overlay.tick();
                    glib::ControlFlow::Continue
                }
            });

            glib::spawn_future_local({
                let app = app.clone();
                async move {
                    while let Ok(event) = events.recv().await {
                        match event {
                            UiEvent::State(view) => overlay.apply(&view),
                            UiEvent::Level { session, dbfs } => overlay.level(session, dbfs),
                            UiEvent::Stage { session, label } => overlay.stage(session, label),
                        }
                    }
                    tracing::debug!("controller channel closed; quitting");
                    app.quit();
                }
            });
        }
    });

    for signal in [rustix::process::Signal::INT, rustix::process::Signal::TERM] {
        let app = app.clone();
        glib_unix::unix_signal_add_local(signal.as_raw(), move || {
            tracing::info!("signal received, shutting down");
            app.quit();
            glib::ControlFlow::Break
        });
    }

    // Do not let GTK interpret our CLI arguments.
    let code = app.run_with_args::<&str>(&[]);
    !failed.get() && code == glib::ExitCode::SUCCESS
}

fn install_css() {
    let provider = gtk4::CssProvider::new();
    provider.load_from_string(CSS);
    if let Some(display) = gdk::Display::default() {
        gtk4::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preview_flattens_and_truncates() {
        assert_eq!(preview("a\n  b\tc"), "a b c");
        let long = "x".repeat(200);
        let p = preview(&long);
        assert_eq!(p.chars().count(), PREVIEW_CHARS);
        assert!(p.ends_with('…'));
    }
}

//! GTK4 layer-shell status indicator. All GTK objects stay on the main thread;
//! state arrives as owned [`View`] values over an async channel.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Instant;

use gtk4::prelude::*;
use gtk4::{gdk, glib};
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};

use crate::config::{Config, OverlayLayer};
use crate::controller::State;

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
.detail { opacity: 0.75; }
"#;

/// What the overlay should display. Owned and `Send`, built off the GTK thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum View {
    Hidden,
    Recording,
    Processing,
    Ready { preview: String },
    Error { message: String },
}

impl From<&State> for View {
    fn from(state: &State) -> Self {
        match state {
            State::Idle => View::Hidden,
            State::Recording { .. } => View::Recording,
            State::Processing { .. } => View::Processing,
            State::Ready { text, .. } => View::Ready {
                preview: preview(text),
            },
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
    recording_since: Cell<Option<Instant>>,
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
        pill.append(&dot);
        pill.append(&status);
        pill.append(&detail);
        window.set_child(Some(&pill));

        Self {
            window,
            pill,
            status,
            detail,
            recording_since: Cell::new(None),
        }
    }

    fn apply(&self, view: &View) {
        for class in ["recording", "processing", "ready", "error"] {
            self.pill.remove_css_class(class);
        }
        self.recording_since.set(None);

        let (class, status, detail) = match view {
            View::Hidden => {
                self.window.set_visible(false);
                return;
            }
            View::Recording => {
                self.recording_since.set(Some(Instant::now()));
                ("recording", "Recording", "0:00".to_owned())
            }
            View::Processing => ("processing", "Transcribing…", String::new()),
            View::Ready { preview } => ("ready", "Ready", preview.clone()),
            View::Error { message } => ("error", "Error", message.clone()),
        };
        self.pill.add_css_class(class);
        self.status.set_text(status);
        self.detail.set_text(&detail);
        self.detail.set_visible(!detail.is_empty());
        self.window.set_visible(true);
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
pub fn run(config: Config, views: async_channel::Receiver<View>) -> bool {
    let app = gtk4::Application::builder()
        .application_id(APP_ID)
        // Single-instance handling is done by the control socket, not D-Bus.
        .flags(gtk4::gio::ApplicationFlags::NON_UNIQUE)
        .build();

    let failed = Rc::new(Cell::new(false));
    let hold = Rc::new(RefCell::new(None));
    let views = Rc::new(RefCell::new(Some(views)));

    app.connect_activate({
        let failed = failed.clone();
        move |app| {
            // Activation can be requested more than once; set up exactly once.
            let Some(views) = views.borrow_mut().take() else {
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
                    while let Ok(view) = views.recv().await {
                        overlay.apply(&view);
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

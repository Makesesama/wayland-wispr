//! GTK4 layer-shell status indicator. All GTK objects stay on the main thread;
//! state arrives as owned [`View`] values over an async channel.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::rc::{Rc, Weak};
use std::time::{Duration, Instant};

use gtk4::prelude::*;
use gtk4::{gdk, glib};
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};

use crate::cleanup::CleanupStatus;
use crate::config::{Config, InsertMethod, OnReady, OverlayConfig, OverlayLayer, Side};
use crate::controller::{Action, Notice, SessionId, State};
use crate::theme;

const APP_ID: &str = "de.makussu.WaylandWispr";
const PREVIEW_CHARS: usize = 60;

/// Most bars the waveform keeps; one per level report (every 50 ms). How many
/// are drawn depends on the width the waveform gets.
const WAVE_BARS: usize = 128;
/// Unscaled waveform geometry in logical pixels; multiplied by `overlay.scale`.
/// The waveform is at least `WAVE_WIDTH` wide and fills the pill's centre.
const WAVE_WIDTH: f64 = 170.0;
const WAVE_BAR_WIDTH: f64 = 3.0;
const WAVE_BAR_GAP: f64 = 2.0;
const WAVE_HEIGHT: f64 = 22.0;
/// Levels at or below this read as silence; 0 dBFS fills the bar.
const WAVE_FLOOR_DBFS: f32 = -60.0;
/// Fade in/out duration; skipped when GTK animations are disabled.
const FADE: Duration = Duration::from_millis(160);
/// How long "Inserted"/"Copied" stays up after a successful insert or copy.
const CONFIRM: Duration = Duration::from_millis(900);

/// Built-in stylesheet (the default theme) with all sizes multiplied by
/// `scale` and the pill background at `opacity`. A custom `overlay.theme` is
/// layered on top; `wayland-wispr default-theme` prints this as a template.
pub fn css(scale: f64, opacity: f64) -> String {
    let px = |v: f64| format!("{}px", (v * scale).round());
    let font = (scale * 100.0).round();
    format!(
        r#"
window.wispr {{
  background: transparent;
}}
.pill {{
  background-color: rgba(20, 20, 24, {opacity});
  color: #f2f2f2;
  border-radius: 999px;
  padding: {pad_y} {pad_x};
  font-size: {font}%;
  transition: background-color 200ms ease, color 200ms ease;
}}
.dot {{
  min-width: {dot};
  min-height: {dot};
  border-radius: 999px;
  background-color: #f2f2f2;
  transition: background-color 200ms ease;
}}
.recording .dot {{ background-color: #ff4d4d; }}
.processing .dot {{ background-color: #f5c542; }}
.ready .dot {{ background-color: #4dd28a; }}
.error .dot {{ background-color: #ff7a45; }}
/* `wayland-wispr note` sessions also carry `.note`. */
.note.recording .dot {{ background-color: #4da3ff; }}
/* Step indicator: one segment per step (transcribe, polish, insert/copy). */
.step {{
  min-width: {step_w};
  min-height: {step_h};
  border-radius: 999px;
  transition: background-color 200ms ease;
}}
.step.pending {{ background-color: rgba(255, 255, 255, 0.18); }}
.step.done {{ background-color: rgba(242, 242, 242, 0.85); }}
.step.active {{
  background-color: #f5c542;
  animation: step-pulse 700ms ease-in-out infinite alternate;
}}
.ready .step.done {{ background-color: #4dd28a; }}
@keyframes step-pulse {{
  from {{ opacity: 1; }}
  to {{ opacity: 0.35; }}
}}
/* Shown instead of the dot while transcribing, polishing, or inserting. */
spinner.busy {{ color: #f5c542; min-width: {spin}; min-height: {spin}; }}
.status {{ font-weight: bold; }}
/* Bars are drawn in the widget's foreground colour. */
.wave {{ color: #ff4d4d; }}
.note .wave {{ color: #4da3ff; }}
.time {{ font-feature-settings: "tnum"; opacity: 0.75; }}
.detail {{ opacity: 0.75; }}
"#,
        pad_y = px(8.0),
        pad_x = px(18.0),
        dot = px(10.0),
        step_w = px(12.0),
        step_h = px(4.0),
        spin = px(12.0),
        opacity = (opacity * 1000.0).round() / 1000.0,
    )
}

/// Logical-pixel spacing scaled by `overlay.scale`.
fn scaled(v: f64, scale: f64) -> i32 {
    (v * scale).round() as i32
}

/// Messages to the GTK thread. Owned and `Send`.
#[derive(Debug, Clone, PartialEq)]
pub enum UiEvent {
    State(View),
    /// Microphone input level; ignored unless that session is recording.
    Level {
        session: SessionId,
        dbfs: f32,
    },
    /// Processing stage; ignored unless that session is processing.
    Stage {
        session: SessionId,
        stage: Stage,
    },
}

/// Fine-grained processing stage reported by the worker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    LoadingModel,
    Transcribing,
    Polishing,
}

impl Stage {
    fn label(self) -> &'static str {
        match self {
            Stage::LoadingModel => "Loading model…",
            Stage::Transcribing => "Transcribing…",
            Stage::Polishing => "Polishing…",
        }
    }

    fn step(self) -> Step {
        match self {
            Stage::LoadingModel | Stage::Transcribing => Step::Transcribe,
            Stage::Polishing => Step::Polish,
        }
    }
}

/// One segment of the step indicator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    Transcribe,
    Polish,
    /// Automatic copy or insert after processing (`injection.on_ready`).
    Deliver,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StepState {
    Done,
    Active,
    Pending,
}

impl StepState {
    fn css_class(self) -> &'static str {
        match self {
            StepState::Done => "done",
            StepState::Active => "active",
            StepState::Pending => "pending",
        }
    }
}

/// Steps a session goes through after recording, given the config; notes
/// always end with the hook. Cleanup that turns out to be skipped (short
/// transcript) simply shows as done.
fn plan(config: &Config, note: bool) -> Vec<Step> {
    let mut steps = vec![Step::Transcribe];
    if config.cleanup.enabled {
        steps.push(Step::Polish);
    }
    if note || config.injection.on_ready != OnReady::None {
        steps.push(Step::Deliver);
    }
    steps
}

/// States of all planned steps with `current` active (`None`: all done).
fn step_states(plan: &[Step], current: Option<Step>) -> Vec<StepState> {
    let active = current.and_then(|step| plan.iter().position(|s| *s == step));
    (0..plan.len())
        .map(|i| match active {
            None => StepState::Done,
            Some(a) if i < a => StepState::Done,
            Some(a) if i == a => StepState::Active,
            Some(_) => StepState::Pending,
        })
        .collect()
}

/// What the overlay should display. Owned and `Send`, built off the GTK thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum View {
    Hidden,
    Recording {
        session: SessionId,
        /// Goes to the note hook.
        note: bool,
    },
    Processing {
        session: SessionId,
        note: bool,
    },
    Ready {
        preview: String,
        notice: Option<Notice>,
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
            State::Recording { session, on_ready } => View::Recording {
                session: *session,
                note: *on_ready == Some(Action::Note),
            },
            State::Processing { session, on_ready } => View::Processing {
                session: *session,
                note: *on_ready == Some(Action::Note),
            },
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

impl View {
    fn is_note(&self) -> bool {
        match self {
            View::Recording { note, .. } | View::Processing { note, .. } => *note,
            View::Inserting { action } => *action == Action::Note,
            _ => false,
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

/// Rolling history of input levels (0..=1), newest last.
#[derive(Debug, Default)]
struct Wave {
    levels: VecDeque<f64>,
}

impl Wave {
    fn clear(&mut self) {
        self.levels.clear();
    }

    fn push(&mut self, dbfs: f32) {
        if self.levels.len() == WAVE_BARS {
            self.levels.pop_front();
        }
        self.levels.push_back(normalize_level(dbfs));
    }
}

/// Map dBFS onto 0..=1 for drawing.
fn normalize_level(dbfs: f32) -> f64 {
    if !dbfs.is_finite() {
        return 0.0;
    }
    f64::from((dbfs - WAVE_FLOOR_DBFS) / -WAVE_FLOOR_DBFS).clamp(0.0, 1.0)
}

/// Elapsed recording time as `m:ss`, counting up.
fn format_elapsed(secs: u64) -> String {
    format!("{}:{:02}", secs / 60, secs % 60)
}

/// Bars that fit in `width` pixels.
fn bars_for_width(width: f64, bar: f64, gap: f64) -> usize {
    (((width + gap) / (bar + gap)).floor().max(0.0) as usize).min(WAVE_BARS)
}

fn draw_wave(
    area: &gtk4::DrawingArea,
    cr: &gtk4::cairo::Context,
    width: i32,
    height: i32,
    scale: f64,
    wave: &Wave,
) {
    let color = area.color();
    cr.set_source_rgba(
        f64::from(color.red()),
        f64::from(color.green()),
        f64::from(color.blue()),
        f64::from(color.alpha()),
    );
    let height = f64::from(height);
    let mid = height / 2.0;
    let (bar, gap) = (WAVE_BAR_WIDTH * scale, WAVE_BAR_GAP * scale);
    let radius = bar / 2.0;
    let count = bars_for_width(f64::from(width), bar, gap);
    // Centre the bars in the leftover sub-bar width.
    let used = count as f64 * (bar + gap) - gap;
    let offset = ((f64::from(width) - used) / 2.0).max(0.0);
    // The newest level is the rightmost bar; missing history (start of a
    // recording) shows as flat bars.
    let shown = wave.levels.len().min(count);
    let levels = std::iter::repeat_n(0.0, count - shown)
        .chain(wave.levels.iter().skip(wave.levels.len() - shown).copied());
    for (i, level) in levels.enumerate() {
        let x = offset + i as f64 * (bar + gap);
        let h = (level * height).max(bar);
        let top = mid - h / 2.0;
        // Pill-shaped bar: two semicircle caps joined by straight sides.
        cr.new_sub_path();
        cr.arc(x + radius, top + radius, radius, std::f64::consts::PI, 0.0);
        cr.arc(
            x + radius,
            top + h - radius,
            radius,
            0.0,
            std::f64::consts::PI,
        );
        cr.close_path();
    }
    let _ = cr.fill();
}

struct Overlay {
    this: Weak<Overlay>,
    window: gtk4::Window,
    pill: gtk4::CenterBox,
    /// Dot or spinner, in one fixed-size slot.
    icon: gtk4::Stack,
    spinner: gtk4::Spinner,
    /// Status text of the busy states, one page per possible text so it is
    /// as wide as the widest of them.
    status: gtk4::Stack,
    /// Centre of the pill: "recording" (waveform) or "busy" (status text).
    center: gtk4::Stack,
    /// Right end of the pill: "recording" (time) or "busy" (steps).
    end: gtk4::Stack,
    /// Ready/Error headline + detail, shown instead of `center`/`end`; may
    /// widen the pill.
    result: gtk4::Box,
    headline: gtk4::Label,
    /// Step indicator container and one segment per planned step.
    steps: gtk4::Box,
    plan: Vec<Step>,
    /// Steps of a note session; never shorter than `plan`.
    note_plan: Vec<Step>,
    /// The session shown is a note (kept through the "Note saved" confirmation).
    note: Cell<bool>,
    detail: gtk4::Label,
    wave_area: gtk4::DrawingArea,
    wave: Rc<RefCell<Wave>>,
    time: gtk4::Label,
    output: Option<String>,
    /// Last view applied, to tell a finished insert from a cancel.
    last: RefCell<View>,
    /// Running fade animation, if any.
    fade: RefCell<Option<gtk4::TickCallbackId>>,
    /// Pending timeout that ends the insert confirmation.
    confirm: RefCell<Option<glib::SourceId>>,
    recording_since: Cell<Option<Instant>>,
    recording_session: Cell<Option<SessionId>>,
    processing_session: Cell<Option<SessionId>>,
    insert_label: &'static str,
}

impl Overlay {
    fn new(app: &gtk4::Application, config: &Config) -> Rc<Self> {
        let window = gtk4::Window::builder()
            .application(app)
            .title("wayland-wispr")
            .decorated(false)
            .resizable(false)
            .focusable(false)
            .can_target(false)
            .css_classes(["wispr"])
            .build();

        window.init_layer_shell();
        window.set_namespace(Some("wayland-wispr"));
        window.set_layer(match config.overlay.layer {
            OverlayLayer::Overlay => Layer::Overlay,
            OverlayLayer::Top => Layer::Top,
        });
        place(&window, &config.overlay);
        // Never take keyboard focus from the application being dictated into,
        // and never reserve screen space.
        window.set_keyboard_mode(KeyboardMode::None);
        window.set_exclusive_zone(0);
        // Purely an indicator: pointer input passes through to what is below.
        window.connect_realize(click_through);

        let scale = config.overlay.scale;
        let insert_label = match config.injection.method {
            InsertMethod::Type => "Typing…",
            InsertMethod::Paste => "Pasting…",
        };

        let pill = gtk4::CenterBox::new();
        pill.add_css_class("pill");

        let dot = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
        dot.add_css_class("dot");
        dot.set_halign(gtk4::Align::Center);
        dot.set_valign(gtk4::Align::Center);
        let spinner = gtk4::Spinner::new();
        spinner.add_css_class("busy");
        spinner.set_halign(gtk4::Align::Center);
        spinner.set_valign(gtk4::Align::Center);
        let icon = fixed_stack();
        icon.add_named(&dot, Some("dot"));
        icon.add_named(&spinner, Some("spinner"));

        let status = fixed_stack();
        for text in [
            Stage::LoadingModel.label(),
            Stage::Transcribing.label(),
            Stage::Polishing.label(),
            insert_label,
            "Copying…",
            "Copied",
            "Inserted",
            "Saving note…",
            "Note saved",
        ] {
            add_status_page(&status, text);
        }
        let headline = gtk4::Label::new(None);
        headline.add_css_class("status");

        let wave = Rc::new(RefCell::new(Wave::default()));
        let wave_area = gtk4::DrawingArea::new();
        wave_area.add_css_class("wave");
        wave_area.set_valign(gtk4::Align::Center);
        wave_area.set_content_width((WAVE_WIDTH * scale).ceil() as i32);
        wave_area.set_content_height((WAVE_HEIGHT * scale).ceil() as i32);
        wave_area.set_draw_func({
            let wave = wave.clone();
            move |area, cr, width, height| draw_wave(area, cr, width, height, scale, &wave.borrow())
        });
        wave_area.set_hexpand(true);
        let time = gtk4::Label::new(None);
        time.add_css_class("time");
        time.set_xalign(1.0);
        // Room for the longest possible time, so the counter never resizes.
        time.set_width_chars(format_elapsed(config.recording.max_seconds).len() as i32);

        let note_plan = plan(config, true);
        let plan = plan(config, false);
        let steps = gtk4::Box::new(gtk4::Orientation::Horizontal, scaled(4.0, scale));
        steps.add_css_class("steps");
        steps.set_halign(gtk4::Align::End);
        steps.set_valign(gtk4::Align::Center);
        for _ in &note_plan {
            let segment = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
            segment.add_css_class("step");
            segment.set_valign(gtk4::Align::Center);
            steps.append(&segment);
        }

        let center = fixed_stack();
        center.add_named(&wave_area, Some("recording"));
        center.add_named(&status, Some("busy"));
        let end = fixed_stack();
        end.add_named(&time, Some("recording"));
        end.add_named(&steps, Some("busy"));

        let detail = gtk4::Label::new(None);
        detail.add_css_class("detail");
        detail.set_ellipsize(gtk4::pango::EllipsizeMode::End);
        detail.set_max_width_chars(PREVIEW_CHARS as i32);
        let result = gtk4::Box::new(gtk4::Orientation::Horizontal, scaled(10.0, scale));
        result.append(&headline);
        result.append(&detail);

        // Waveform/status text sit in the true centre of the pill, between
        // the dot/spinner and the time/steps.
        let middle = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
        middle.set_margin_start(scaled(12.0, scale));
        middle.set_margin_end(scaled(12.0, scale));
        middle.append(&center);
        middle.append(&result);
        icon.set_halign(gtk4::Align::Start);
        pill.set_start_widget(Some(&icon));
        pill.set_center_widget(Some(&middle));
        pill.set_end_widget(Some(&end));
        window.set_child(Some(&pill));

        // Fix the pill at the width of the recording/busy states (the wider
        // page of each stack). Ready/Error may grow beyond it for their detail.
        result.set_visible(false);
        let (_, width, _, _) = pill.measure(gtk4::Orientation::Horizontal, -1);
        pill.set_size_request(width, -1);

        Rc::new_cyclic(|this| Self {
            this: this.clone(),
            window,
            pill,
            icon,
            spinner,
            status,
            center,
            end,
            result,
            headline,
            steps,
            plan,
            note_plan,
            note: Cell::new(false),
            detail,
            wave_area,
            wave,
            time,
            output: config.overlay.output.clone(),
            last: RefCell::new(View::Hidden),
            fade: RefCell::new(None),
            confirm: RefCell::new(None),
            recording_since: Cell::new(None),
            recording_session: Cell::new(None),
            processing_session: Cell::new(None),
            insert_label,
        })
    }

    fn weak_self(&self) -> Weak<Self> {
        self.this.clone()
    }

    fn apply(&self, view: &View) {
        if let Some(id) = self.confirm.take() {
            id.remove();
        }
        let previous = self.last.replace(view.clone());
        for class in ["recording", "processing", "ready", "error", "note"] {
            self.pill.remove_css_class(class);
        }
        if view.is_note() {
            self.pill.add_css_class("note");
        }
        if *view != View::Hidden {
            self.note.set(view.is_note());
        }
        self.recording_since.set(None);
        self.recording_session.set(None);
        self.processing_session.set(None);
        self.wave.borrow_mut().clear();
        self.wave_area.queue_draw();
        let steps_shown = self.end.is_visible()
            && self.end.visible_child_name().as_deref() == Some("busy")
            && self.steps.is_visible();
        self.center.set_visible_child_name("busy");
        self.end.set_visible_child_name("busy");
        self.steps.set_visible(false);
        let busy = matches!(view, View::Processing { .. } | View::Inserting { .. });
        self.icon
            .set_visible_child_name(if busy { "spinner" } else { "dot" });
        self.spinner.set_spinning(busy);
        // Ready/Error use the free-width headline + detail; every other state
        // the fixed-width content.
        let result = matches!(view, View::Ready { .. } | View::Error { .. });
        self.center.set_visible(!result);
        self.end.set_visible(!result);
        self.result.set_visible(result);

        let (class, status, detail) = match view {
            View::Hidden => {
                // Inserting -> Idle means it succeeded (failure returns to
                // Ready with a notice); confirm briefly. Anything else, e.g. a
                // cancel, just goes away.
                let View::Inserting { action } = previous else {
                    self.fade_out();
                    return;
                };
                self.pill.add_css_class("ready");
                set_status(
                    &self.status,
                    match action {
                        Action::Copy => "Copied",
                        Action::Insert => "Inserted",
                        Action::Note => "Note saved",
                    },
                );
                self.detail.set_visible(false);
                if steps_shown {
                    self.show_steps(None);
                }
                let overlay = self.weak_self();
                *self.confirm.borrow_mut() =
                    Some(glib::timeout_add_local_once(CONFIRM, move || {
                        if let Some(overlay) = overlay.upgrade() {
                            overlay.confirm.take();
                            overlay.fade_out();
                        }
                    }));
                return;
            }
            View::Recording { session, .. } => {
                self.recording_since.set(Some(Instant::now()));
                self.recording_session.set(Some(*session));
                self.time.set_text(&format_elapsed(0));
                self.center.set_visible_child_name("recording");
                self.end.set_visible_child_name("recording");
                // No status text: dot, waveform, and time say enough.
                ("recording", "", String::new())
            }
            View::Processing { session, .. } => {
                self.processing_session.set(Some(*session));
                self.show_steps(Some(Step::Transcribe));
                ("processing", Stage::Transcribing.label(), String::new())
            }
            View::Ready {
                notice: Some(notice),
                ..
            } => {
                let headline = match notice.action {
                    Action::Copy => "Copy failed",
                    Action::Insert => "Insert failed",
                    Action::Note => "Note failed",
                };
                let failure = &notice.failure;
                let detail = match &failure.detail {
                    Some(detail) => format!("{}: {detail}", failure.message),
                    None => failure.message.clone(),
                };
                ("error", headline, detail)
            }
            View::Ready {
                preview,
                uncleaned: true,
                ..
            } => ("ready", "Ready (not cleaned up)", preview.clone()),
            View::Ready { preview, .. } => ("ready", "Ready", preview.clone()),
            View::Inserting { action } => {
                self.show_steps(Some(Step::Deliver));
                let label = match action {
                    Action::Copy => "Copying…",
                    Action::Insert => self.insert_label,
                    Action::Note => "Saving note…",
                };
                ("processing", label, String::new())
            }
            View::Error { message } => ("error", "Error", message.clone()),
        };
        self.pill.add_css_class(class);
        if result {
            self.headline.set_text(status);
        } else if !status.is_empty() {
            set_status(&self.status, status);
        }
        self.detail.set_text(&detail);
        self.detail.set_visible(!detail.is_empty());
        self.fade_in();
    }

    fn fade_in(&self) {
        if !self.window.is_visible() {
            // Only while unmapped: re-resolve the output (it may have been
            // plugged in since startup) before showing.
            self.select_output();
            self.window.set_opacity(0.0);
            self.window.set_visible(true);
        }
        self.fade_to(1.0);
    }

    fn fade_out(&self) {
        self.spinner.set_spinning(false);
        if self.window.is_visible() {
            self.fade_to(0.0);
        }
    }

    /// Animate the window opacity from its current value to `target`; at 0
    /// the window is unmapped.
    fn fade_to(&self, target: f64) {
        if let Some(id) = self.fade.take() {
            id.remove();
        }
        let animate = gtk4::Settings::default().is_none_or(|s| s.is_gtk_enable_animations());
        let from = self.window.opacity();
        if !animate || (from - target).abs() < f64::EPSILON {
            finish_fade(&self.window, target);
            return;
        }
        let duration = FADE.as_secs_f64() * (from - target).abs();
        let start = Cell::new(None);
        let id = self.window.add_tick_callback(move |window, clock| {
            let now = clock.frame_time() as f64 / 1e6;
            let t = ((now
                - start.get().unwrap_or_else(|| {
                    start.set(Some(now));
                    now
                }))
                / duration)
                .clamp(0.0, 1.0);
            // Ease out.
            let eased = 1.0 - (1.0 - t).powi(2);
            window.set_opacity(from + (target - from) * eased);
            if t >= 1.0 {
                finish_fade(window, target);
                glib::ControlFlow::Break
            } else {
                glib::ControlFlow::Continue
            }
        });
        *self.fade.borrow_mut() = Some(id);
    }

    fn select_output(&self) {
        let Some(name) = self.output.as_deref() else {
            return;
        };
        let monitor = gdk::Display::default().and_then(|display| {
            let monitors = display.monitors();
            (0..monitors.n_items())
                .filter_map(|i| monitors.item(i)?.downcast::<gdk::Monitor>().ok())
                .find(|m| m.connector().as_deref() == Some(name))
        });
        if monitor.is_none() {
            tracing::warn!(
                output = name,
                "configured overlay output not found; compositor chooses"
            );
        }
        self.window.set_monitor(monitor.as_ref());
    }

    fn level(&self, session: SessionId, dbfs: f32) {
        if self.recording_session.get() != Some(session) {
            return;
        }
        self.wave.borrow_mut().push(dbfs);
        self.wave_area.queue_draw();
    }

    fn stage(&self, session: SessionId, stage: Stage) {
        if self.processing_session.get() == Some(session) {
            set_status(&self.status, stage.label());
            self.show_steps(Some(stage.step()));
        }
    }

    /// Show the step indicator with `current` active (`None`: all done). A
    /// single step is not worth showing, nor a step that is not planned
    /// (e.g. a manual `insert` with `on_ready = "none"`).
    fn show_steps(&self, current: Option<Step>) {
        let plan = if self.note.get() {
            &self.note_plan
        } else {
            &self.plan
        };
        let planned = current.is_none_or(|step| plan.contains(&step));
        if plan.len() < 2 || !planned {
            self.steps.set_visible(false);
            return;
        }
        let mut states = step_states(plan, current).into_iter();
        let mut child = self.steps.first_child();
        while let Some(segment) = child {
            for class in ["done", "active", "pending"] {
                segment.remove_css_class(class);
            }
            // Segments beyond this plan (a note's extra step) are hidden.
            match states.next() {
                Some(state) => {
                    segment.add_css_class(state.css_class());
                    segment.set_visible(true);
                }
                None => segment.set_visible(false),
            }
            child = segment.next_sibling();
        }
        self.steps.set_visible(true);
    }

    fn tick(&self) {
        if let Some(since) = self.recording_since.get() {
            self.time
                .set_text(&format_elapsed(since.elapsed().as_secs()));
        }
    }
}

/// A stack sized to its largest page, switching without animation.
fn fixed_stack() -> gtk4::Stack {
    let stack = gtk4::Stack::new();
    stack.set_hhomogeneous(true);
    stack.set_vhomogeneous(true);
    stack.set_transition_type(gtk4::StackTransitionType::None);
    stack
}

fn add_status_page(stack: &gtk4::Stack, text: &str) {
    if stack.child_by_name(text).is_none() {
        let label = gtk4::Label::new(Some(text));
        label.add_css_class("status");
        label.set_xalign(0.5);
        stack.add_named(&label, Some(text));
    }
}

/// Show `text` in a status stack. Every text in use is added up front so the
/// width is fixed; an unexpected one is still shown, at the cost of a resize.
fn set_status(stack: &gtk4::Stack, text: &str) {
    if stack.child_by_name(text).is_none() {
        tracing::debug!(text, "status text without a pre-sized page");
        add_status_page(stack, text);
    }
    stack.set_visible_child_name(text);
}

fn finish_fade(window: &gtk4::Window, target: f64) {
    window.set_opacity(target);
    if target == 0.0 {
        window.set_visible(false);
    }
}

/// Anchor the window per the configured position. An axis with neither edge
/// anchored is centred by the compositor.
fn place(window: &gtk4::Window, overlay: &OverlayConfig) {
    let (horizontal, vertical) = overlay.position.sides();
    for (side, start, end, margin) in [
        (horizontal, Edge::Left, Edge::Right, overlay.margin_x()),
        (vertical, Edge::Top, Edge::Bottom, overlay.margin_y()),
    ] {
        window.set_anchor(start, side == Side::Start);
        window.set_anchor(end, side == Side::End);
        window.set_margin(start, margin);
        window.set_margin(end, margin);
    }
}

/// Empty input region: clicks and scrolling go to the window below.
fn click_through(window: &gtk4::Window) {
    if let Some(surface) = window.surface() {
        surface.set_input_region(Some(&gtk4::cairo::Region::create()));
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

            install_css(config.overlay.scale, config.overlay.opacity);
            // Before building the overlay: the pill's fixed width is measured
            // with the stylesheets active at that point.
            theme::install(config.overlay.theme.as_ref());
            *hold.borrow_mut() = Some(app.hold());

            let overlay = Overlay::new(app, &config);
            overlay.apply(&View::Hidden);

            // Sub-second tick so the counter turns over close to each full second.
            glib::timeout_add_local(std::time::Duration::from_millis(200), {
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
                            UiEvent::Stage { session, stage } => overlay.stage(session, stage),
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

fn install_css(scale: f64, opacity: f64) {
    let provider = gtk4::CssProvider::new();
    provider.load_from_string(&css(scale, opacity));
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

    #[test]
    fn plan_follows_cleanup_and_on_ready() {
        let mut config = Config::default();
        config.cleanup.enabled = true;
        config.injection.on_ready = OnReady::Insert;
        assert_eq!(
            plan(&config, false),
            [Step::Transcribe, Step::Polish, Step::Deliver]
        );
        config.cleanup.enabled = false;
        config.injection.on_ready = OnReady::Copy;
        assert_eq!(plan(&config, false), [Step::Transcribe, Step::Deliver]);
        config.injection.on_ready = OnReady::None;
        assert_eq!(plan(&config, false), [Step::Transcribe]);
        // Notes always run the hook.
        assert_eq!(plan(&config, true), [Step::Transcribe, Step::Deliver]);
    }

    #[test]
    fn step_states_mark_done_active_pending() {
        use StepState::*;
        let all = [Step::Transcribe, Step::Polish, Step::Deliver];
        assert_eq!(
            step_states(&all, Some(Step::Transcribe)),
            [Active, Pending, Pending]
        );
        assert_eq!(
            step_states(&all, Some(Step::Polish)),
            [Done, Active, Pending]
        );
        // Skipped cleanup: jumping to delivery marks polish done.
        assert_eq!(step_states(&all, Some(Step::Deliver)), [Done, Done, Active]);
        assert_eq!(step_states(&all, None), [Done, Done, Done]);
        assert_eq!(
            step_states(&[Step::Transcribe, Step::Polish], Some(Step::Polish)),
            [Done, Active]
        );
    }

    #[test]
    fn css_scales_sizes() {
        let normal = css(1.0, 0.88);
        assert!(normal.contains("rgba(20, 20, 24, 0.88)"));
        assert!(css(1.0, 0.0).contains("rgba(20, 20, 24, 0)"));
        assert!(normal.contains("padding: 8px 18px;"));
        assert!(normal.contains("font-size: 100%;"));
        let big = css(1.5, 1.0);
        assert!(big.contains("padding: 12px 27px;"));
        assert!(big.contains("font-size: 150%;"));
        assert_eq!(scaled(10.0, 1.5), 15);
    }

    #[test]
    fn elapsed_counts_up_as_minutes_and_seconds() {
        assert_eq!(format_elapsed(0), "0:00");
        assert_eq!(format_elapsed(59), "0:59");
        assert_eq!(format_elapsed(61), "1:01");
        assert_eq!(format_elapsed(3600), "60:00");
    }

    #[test]
    fn wave_keeps_recent_normalized_levels() {
        assert_eq!(normalize_level(0.0), 1.0);
        assert_eq!(normalize_level(-30.0), 0.5);
        assert_eq!(normalize_level(-90.0), 0.0);
        assert_eq!(normalize_level(f32::NEG_INFINITY), 0.0);
        let mut wave = Wave::default();
        for _ in 0..WAVE_BARS {
            wave.push(-60.0);
        }
        wave.push(0.0);
        assert_eq!(wave.levels.len(), WAVE_BARS);
        assert_eq!(wave.levels.back(), Some(&1.0));
        assert_eq!(bars_for_width(170.0, 3.0, 2.0), 34);
        assert_eq!(bars_for_width(2.0, 3.0, 2.0), 0);
        assert_eq!(bars_for_width(1e6, 3.0, 2.0), WAVE_BARS);
    }
}

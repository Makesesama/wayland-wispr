//! Custom overlay themes (`overlay.theme`): GTK CSS layered over the built-in
//! stylesheet. With `light`/`dark` variants the active one follows darkman's
//! D-Bus API (`nl.whynothugo.darkman`), falling back to the XDG desktop
//! portal's `org.freedesktop.appearance color-scheme`, then dark. Both are
//! watched live. Everything runs on the GTK main thread.

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use gtk4::{gdk, gio, glib};

use crate::config::ThemeConfig;

const DARKMAN: &str = "nl.whynothugo.darkman";
const DARKMAN_PATH: &str = "/nl/whynothugo/darkman";

const PORTAL: &str = "org.freedesktop.portal.Desktop";
const PORTAL_PATH: &str = "/org/freedesktop/portal/desktop";
const PORTAL_SETTINGS: &str = "org.freedesktop.portal.Settings";
const APPEARANCE_NAMESPACE: &str = "org.freedesktop.appearance";
const COLOR_SCHEME: &str = "color-scheme";

const CALL_TIMEOUT_MS: i32 = 5000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Appearance {
    Dark,
    Light,
}

/// Until darkman or the portal says otherwise.
const INITIAL: Appearance = Appearance::Dark;

/// Above the built-in stylesheet (`APPLICATION`), below the user's
/// `~/.config/gtk-4.0/gtk.css` (`USER`). The higher provider wins for every
/// property it sets, whatever the selector specificity.
const PRIORITY: u32 = gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION + 1;

/// Load the configured custom theme, if any, and keep the matching variant
/// active. The stylesheet is re-read on each switch, so edits show up at the
/// next mode change. A missing or invalid file is logged; whatever does not
/// apply falls back to the default theme.
pub fn install(theme: Option<&ThemeConfig>) {
    let Some(theme) = theme else { return };
    let Some(display) = gdk::Display::default() else {
        return;
    };
    let provider = gtk4::CssProvider::new();
    provider.connect_parsing_error(|_, section, error| {
        tracing::warn!(at = %section.to_str(), %error, "overlay theme");
    });
    gtk4::style_context_add_provider_for_display(&display, &provider, PRIORITY);
    match theme.clone() {
        ThemeConfig::Single(path) => load(&provider, Some(&path)),
        ThemeConfig::Variants(variants) => {
            let path_for = move |appearance| -> Option<PathBuf> {
                match appearance {
                    Appearance::Dark => variants.dark.clone(),
                    Appearance::Light => variants.light.clone(),
                }
            };
            load(&provider, path_for(INITIAL).as_deref());
            follow(move |appearance| load(&provider, path_for(appearance).as_deref()));
        }
    }
}

/// Load `path` into the theme provider; `None` clears it (default theme).
fn load(provider: &gtk4::CssProvider, path: Option<&Path>) {
    match path {
        Some(path) => {
            tracing::debug!(path = %path.display(), "loading overlay theme");
            // A missing file is reported through `parsing-error` and leaves
            // the provider empty.
            provider.load_from_path(path);
        }
        None => provider.load_from_string(""),
    }
}

impl Appearance {
    /// darkman's `Mode` property / `ModeChanged` argument.
    fn from_darkman(mode: &str) -> Option<Self> {
        match mode {
            "dark" => Some(Appearance::Dark),
            "light" => Some(Appearance::Light),
            _ => None,
        }
    }

    /// Portal `color-scheme`: 0 = no preference, 1 = prefer dark, 2 = prefer light.
    fn from_color_scheme(value: u32) -> Option<Self> {
        match value {
            1 => Some(Appearance::Dark),
            2 => Some(Appearance::Light),
            _ => None,
        }
    }
}

/// Last known preference of each source; darkman wins over the portal.
#[derive(Debug, Default)]
struct Sources {
    darkman: Option<Appearance>,
    portal: Option<Appearance>,
}

impl Sources {
    fn resolve(&self) -> Appearance {
        self.darkman.or(self.portal).unwrap_or(Appearance::Dark)
    }
}

struct Follower {
    sources: RefCell<Sources>,
    current: Cell<Appearance>,
    on_change: Box<dyn Fn(Appearance)>,
    /// Dropping a subscription unsubscribes; kept for the process lifetime.
    subscriptions: RefCell<Vec<gio::SignalSubscription>>,
}

impl Follower {
    fn update(&self, change: impl FnOnce(&mut Sources)) {
        change(&mut self.sources.borrow_mut());
        let next = self.sources.borrow().resolve();
        if next != self.current.replace(next) {
            tracing::info!(appearance = ?next, sources = ?self.sources.borrow(), "overlay theme changed");
            (self.on_change)(next);
        }
    }
}

/// Call `on_change` whenever the system appearance differs from [`INITIAL`]
/// or the last reported one. Subscriptions last for the rest of the process.
/// Without a session bus this logs a warning and keeps the initial appearance.
fn follow(on_change: impl Fn(Appearance) + 'static) {
    let follower = Rc::new(Follower {
        sources: RefCell::default(),
        current: Cell::new(INITIAL),
        on_change: Box::new(on_change),
        subscriptions: RefCell::default(),
    });
    glib::spawn_future_local(async move {
        let conn = match gio::bus_get_future(gio::BusType::Session).await {
            Ok(conn) => conn,
            Err(e) => {
                tracing::warn!(error = %e, "no D-Bus session bus; using the dark overlay theme");
                return;
            }
        };
        follow_darkman(&conn, &follower);
        follow_portal(&conn, &follower).await;
    });
}

// The callbacks own the follower, which owns the subscriptions; neither they
// nor the name watch are ever removed, so all of it lives as long as the
// process.

fn follow_darkman(conn: &gio::DBusConnection, follower: &Rc<Follower>) {
    let changed = follower.clone();
    let subscription = conn.subscribe_to_signal(
        Some(DARKMAN),
        Some(DARKMAN),
        Some("ModeChanged"),
        Some(DARKMAN_PATH),
        None,
        gio::DBusSignalFlags::NONE,
        move |signal| match signal.parameters.get::<(String,)>() {
            Some((mode,)) => {
                changed.update(|s| s.darkman = Appearance::from_darkman(&mode));
            }
            None => {
                tracing::debug!(params = %signal.parameters, "unexpected darkman ModeChanged")
            }
        },
    );
    follower.subscriptions.borrow_mut().push(subscription);

    // darkman may start after us or restart; read its mode whenever it
    // appears and fall back to the portal while it is gone. Not auto-started:
    // without a running darkman, `auto` simply uses the portal.
    let appeared = follower.clone();
    let vanished = follower.clone();
    let _watcher = gio::bus_watch_name_on_connection(
        conn,
        DARKMAN,
        gio::BusNameWatcherFlags::NONE,
        move |conn, _name, _owner| {
            let follower = appeared.clone();
            glib::spawn_future_local(async move {
                let mode = read_darkman_mode(&conn).await;
                follower.update(|s| s.darkman = mode);
            });
        },
        move |_conn, _name| vanished.update(|s| s.darkman = None),
    );
}

async fn read_darkman_mode(conn: &gio::DBusConnection) -> Option<Appearance> {
    let reply = conn
        .call_future(
            Some(DARKMAN),
            DARKMAN_PATH,
            "org.freedesktop.DBus.Properties",
            "Get",
            Some(&(DARKMAN, "Mode").into()),
            Some(glib::VariantTy::new("(v)").unwrap()),
            gio::DBusCallFlags::NO_AUTO_START,
            CALL_TIMEOUT_MS,
        )
        .await;
    match reply {
        Ok(reply) => {
            let mode = unbox(&reply.child_value(0))
                .and_then(|v| v.str().and_then(Appearance::from_darkman));
            if mode.is_none() {
                tracing::debug!(%reply, "unexpected darkman Mode");
            }
            mode
        }
        Err(e) => {
            tracing::debug!(error = %e, "reading darkman Mode failed");
            None
        }
    }
}

async fn follow_portal(conn: &gio::DBusConnection, follower: &Rc<Follower>) {
    let changed = follower.clone();
    let subscription = conn.subscribe_to_signal(
        Some(PORTAL),
        Some(PORTAL_SETTINGS),
        Some("SettingChanged"),
        Some(PORTAL_PATH),
        Some(APPEARANCE_NAMESPACE),
        gio::DBusSignalFlags::NONE,
        move |signal| {
            let params = signal.parameters;
            if params.n_children() != 3 || params.child_value(1).str() != Some(COLOR_SCHEME) {
                return;
            }
            let scheme = color_scheme(&params.child_value(2));
            changed.update(|s| s.portal = scheme);
        },
    );
    follower.subscriptions.borrow_mut().push(subscription);

    let scheme = read_color_scheme(conn).await;
    follower.update(|s| s.portal = scheme);
}

/// Current portal `color-scheme`; `None` without a portal or preference.
async fn read_color_scheme(conn: &gio::DBusConnection) -> Option<Appearance> {
    // ReadOne needs portal version 2; Read is the deprecated fallback.
    for method in ["ReadOne", "Read"] {
        let reply = conn
            .call_future(
                Some(PORTAL),
                PORTAL_PATH,
                PORTAL_SETTINGS,
                method,
                Some(&(APPEARANCE_NAMESPACE, COLOR_SCHEME).into()),
                Some(glib::VariantTy::new("(v)").unwrap()),
                gio::DBusCallFlags::NONE,
                CALL_TIMEOUT_MS,
            )
            .await;
        match reply {
            Ok(reply) => return color_scheme(&reply.child_value(0)),
            Err(e) => tracing::debug!(method, error = %e, "reading portal color-scheme failed"),
        }
    }
    None
}

fn color_scheme(value: &glib::Variant) -> Option<Appearance> {
    unbox(value)
        .and_then(|v| v.get::<u32>())
        .and_then(Appearance::from_color_scheme)
}

/// Strip `v` boxing; the portal's `Read` wraps the value twice.
fn unbox(value: &glib::Variant) -> Option<glib::Variant> {
    let mut value = value.clone();
    for _ in 0..4 {
        if !value.is_type(glib::VariantTy::VARIANT) {
            return Some(value);
        }
        value = value.as_variant()?;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use gtk4::glib::prelude::*;

    #[test]
    fn darkman_wins_over_portal_then_dark() {
        let mut s = Sources::default();
        assert_eq!(s.resolve(), Appearance::Dark);
        s.portal = Some(Appearance::Light);
        assert_eq!(s.resolve(), Appearance::Light);
        s.darkman = Some(Appearance::Dark);
        assert_eq!(s.resolve(), Appearance::Dark);
        s.darkman = None;
        assert_eq!(s.resolve(), Appearance::Light);
    }

    #[test]
    fn parses_darkman_modes_and_color_schemes() {
        assert_eq!(Appearance::from_darkman("light"), Some(Appearance::Light));
        assert_eq!(Appearance::from_darkman("dark"), Some(Appearance::Dark));
        assert_eq!(Appearance::from_darkman("null"), None);
        let boxed = |v: glib::Variant| glib::Variant::from_variant(&v);
        assert_eq!(
            color_scheme(&boxed(1u32.to_variant())),
            Some(Appearance::Dark)
        );
        assert_eq!(
            color_scheme(&boxed(boxed(2u32.to_variant()))),
            Some(Appearance::Light)
        );
        assert_eq!(color_scheme(&boxed(0u32.to_variant())), None);
        assert_eq!(color_scheme(&boxed("dark".to_variant())), None);
    }
}

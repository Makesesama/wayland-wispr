//! Pause MPRIS media players while recording.
//!
//! When recording starts, players that are playing get paused. Once the
//! microphone is closed, those players get resumed, and no others: a player
//! that was already paused or stopped stays that way.
//!
//! D-Bus calls run on a dedicated thread, so a slow or hung player can delay
//! neither the overlay nor the microphone. Requests run in order, so a resume
//! always comes after the pause it belongs to. Every failure (no session bus,
//! no players, a player quitting) is logged at debug level and otherwise
//! ignored.

use std::sync::mpsc;
use std::thread;

use gtk4::{gio, glib};

const MPRIS_PREFIX: &str = "org.mpris.MediaPlayer2.";
const MPRIS_PATH: &str = "/org/mpris/MediaPlayer2";
const PLAYER_IFACE: &str = "org.mpris.MediaPlayer2.Player";
/// playerctld mirrors whichever player was active last. Controlling it
/// would act on that player a second time, or on a different one.
const PROXIES: &[&str] = &["org.mpris.MediaPlayer2.playerctld"];
/// Keeps a hung player from holding up later requests for long.
const CALL_TIMEOUT_MS: i32 = 500;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Playback {
    Playing,
    Paused,
    Stopped,
}

/// The MPRIS operations used here. Players are identified by their unique
/// bus name (`:1.42`), so a restarted player counts as a different one.
trait Bus {
    /// Unique names of all MPRIS players, each listed once.
    fn players(&self) -> Result<Vec<Player>, String>;
    fn status(&self, unique: &str) -> Result<Playback, String>;
    fn pause(&self, unique: &str) -> Result<(), String>;
    fn play(&self, unique: &str) -> Result<(), String>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Player {
    /// Unique bus name; calls go here.
    unique: String,
    /// Well-known name (`org.mpris.MediaPlayer2.spotify`), for logs.
    name: String,
}

enum Request {
    Pause,
    Resume,
}

/// Handle to the media thread. Cheap to clone; does nothing if disabled.
#[derive(Clone)]
pub struct Media {
    tx: Option<mpsc::Sender<Request>>,
}

impl Media {
    /// Starts the media thread if `enabled`.
    pub fn new(enabled: bool) -> Self {
        if !enabled {
            return Self::disabled();
        }
        let (tx, rx) = mpsc::channel();
        let spawned = thread::Builder::new()
            .name("media".into())
            .spawn(move || serve(rx));
        match spawned {
            Ok(_) => Self { tx: Some(tx) },
            Err(e) => {
                tracing::warn!(error = %e, "media pausing unavailable");
                Self::disabled()
            }
        }
    }

    pub fn disabled() -> Self {
        Self { tx: None }
    }

    /// Pause the players that are playing now. Returns immediately.
    pub fn pause(&self) {
        self.send(Request::Pause);
    }

    /// Resume the players paused by [`Media::pause`]. Returns immediately.
    pub fn resume(&self) {
        self.send(Request::Resume);
    }

    fn send(&self, request: Request) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(request);
        }
    }
}

fn serve(rx: mpsc::Receiver<Request>) {
    let mut paused = PausedPlayers::default();
    // Connected on first use, and again after a failure: the session bus
    // may be unavailable at startup.
    let mut bus: Option<GioBus> = None;
    for request in rx {
        if bus.is_none() {
            match GioBus::connect() {
                Ok(b) => bus = Some(b),
                Err(e) => {
                    tracing::debug!(error = %e, "session bus unavailable; not pausing media");
                    // Nothing was paused, so there is nothing to resume.
                    paused = PausedPlayers::default();
                    continue;
                }
            }
        }
        let bus = bus.as_ref().expect("connected above");
        match request {
            Request::Pause => paused.pause_playing(bus),
            Request::Resume => paused.resume(bus),
        }
    }
}

/// The players this process paused and has not resumed yet.
#[derive(Debug, Default)]
struct PausedPlayers {
    players: Vec<Player>,
}

impl PausedPlayers {
    fn pause_playing(&mut self, bus: &impl Bus) {
        let players = match bus.players() {
            Ok(players) => players,
            Err(e) => {
                tracing::debug!(error = %e, "listing media players failed");
                return;
            }
        };
        for player in players {
            // Paused by an earlier request that was not resumed yet: its
            // status is ours, not the user's, so keep the record.
            if self.players.iter().any(|p| p.unique == player.unique) {
                continue;
            }
            match bus.status(&player.unique) {
                Ok(Playback::Playing) => {}
                Ok(_) => continue,
                Err(e) => {
                    tracing::debug!(player = %player.name, error = %e, "reading playback status failed");
                    continue;
                }
            }
            match bus.pause(&player.unique) {
                Ok(()) => {
                    tracing::info!(player = %player.name, "paused media");
                    self.players.push(player);
                }
                Err(e) => tracing::debug!(player = %player.name, error = %e, "pausing failed"),
            }
        }
    }

    fn resume(&mut self, bus: &impl Bus) {
        for player in self.players.drain(..) {
            // The user may have stopped it, or started it again themselves.
            match bus.status(&player.unique) {
                Ok(Playback::Paused) => {}
                Ok(_) => continue,
                // Most likely the player quit meanwhile.
                Err(e) => {
                    tracing::debug!(player = %player.name, error = %e, "not resuming media");
                    continue;
                }
            }
            match bus.play(&player.unique) {
                Ok(()) => tracing::info!(player = %player.name, "resumed media"),
                Err(e) => tracing::debug!(player = %player.name, error = %e, "resuming failed"),
            }
        }
    }
}

struct GioBus {
    conn: gio::DBusConnection,
}

impl GioBus {
    fn connect() -> Result<Self, String> {
        gio::bus_get_sync(gio::BusType::Session, gio::Cancellable::NONE)
            .map(|conn| Self { conn })
            .map_err(|e| e.to_string())
    }

    fn call(
        &self,
        dest: &str,
        path: &str,
        iface: &str,
        method: &str,
        args: Option<&glib::Variant>,
        reply: &str,
    ) -> Result<glib::Variant, String> {
        self.conn
            .call_sync(
                Some(dest),
                path,
                iface,
                method,
                args,
                Some(glib::VariantTy::new(reply).map_err(|e| e.to_string())?),
                gio::DBusCallFlags::NO_AUTO_START,
                CALL_TIMEOUT_MS,
                gio::Cancellable::NONE,
            )
            .map_err(|e| e.to_string())
    }

    fn dbus(
        &self,
        method: &str,
        args: Option<&glib::Variant>,
        reply: &str,
    ) -> Result<glib::Variant, String> {
        self.call(
            "org.freedesktop.DBus",
            "/org/freedesktop/DBus",
            "org.freedesktop.DBus",
            method,
            args,
            reply,
        )
    }
}

impl Bus for GioBus {
    fn players(&self) -> Result<Vec<Player>, String> {
        let (names,) = self
            .dbus("ListNames", None, "(as)")?
            .get::<(Vec<String>,)>()
            .ok_or("unexpected ListNames reply")?;
        let mut players: Vec<Player> = Vec::new();
        for name in names {
            if !name.starts_with(MPRIS_PREFIX) || PROXIES.contains(&name.as_str()) {
                continue;
            }
            // The owner may have quit since ListNames.
            let Some((unique,)) = self
                .dbus("GetNameOwner", Some(&(name.as_str(),).into()), "(s)")
                .ok()
                .and_then(|reply| reply.get::<(String,)>())
            else {
                continue;
            };
            if !players.iter().any(|p| p.unique == unique) {
                players.push(Player { unique, name });
            }
        }
        Ok(players)
    }

    fn status(&self, unique: &str) -> Result<Playback, String> {
        let reply = self.call(
            unique,
            MPRIS_PATH,
            "org.freedesktop.DBus.Properties",
            "Get",
            Some(&(PLAYER_IFACE, "PlaybackStatus").into()),
            "(v)",
        )?;
        let value = reply.child_value(0).as_variant();
        match value.as_ref().and_then(|v| v.str()) {
            Some("Playing") => Ok(Playback::Playing),
            Some("Paused") => Ok(Playback::Paused),
            Some("Stopped") => Ok(Playback::Stopped),
            _ => Err(format!("unexpected PlaybackStatus {reply}")),
        }
    }

    fn pause(&self, unique: &str) -> Result<(), String> {
        self.call(unique, MPRIS_PATH, PLAYER_IFACE, "Pause", None, "()")
            .map(drop)
    }

    fn play(&self, unique: &str) -> Result<(), String> {
        self.call(unique, MPRIS_PATH, PLAYER_IFACE, "Play", None, "()")
            .map(drop)
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::BTreeMap;

    use super::*;

    /// Players by unique name. A missing player fails every call, like one
    /// that quit.
    #[derive(Default)]
    struct FakeBus {
        players: RefCell<BTreeMap<String, Playback>>,
        reachable: bool,
    }

    impl FakeBus {
        fn new(players: &[(&str, Playback)]) -> Self {
            Self {
                players: RefCell::new(players.iter().map(|(n, s)| (n.to_string(), *s)).collect()),
                reachable: true,
            }
        }

        fn status_of(&self, unique: &str) -> Option<Playback> {
            self.players.borrow().get(unique).copied()
        }

        fn set(&self, unique: &str, status: Option<Playback>) {
            let mut players = self.players.borrow_mut();
            match status {
                Some(s) => players.insert(unique.into(), s),
                None => players.remove(unique),
            };
        }

        fn change(&self, unique: &str, to: Playback) -> Result<(), String> {
            match self.players.borrow_mut().get_mut(unique) {
                Some(s) => {
                    *s = to;
                    Ok(())
                }
                None => Err("no such player".into()),
            }
        }
    }

    impl Bus for FakeBus {
        fn players(&self) -> Result<Vec<Player>, String> {
            if !self.reachable {
                return Err("no bus".into());
            }
            Ok(self
                .players
                .borrow()
                .keys()
                .map(|u| Player {
                    unique: u.clone(),
                    name: format!("{MPRIS_PREFIX}{u}"),
                })
                .collect())
        }

        fn status(&self, unique: &str) -> Result<Playback, String> {
            self.status_of(unique)
                .ok_or_else(|| "no such player".into())
        }

        fn pause(&self, unique: &str) -> Result<(), String> {
            self.change(unique, Playback::Paused)
        }

        fn play(&self, unique: &str) -> Result<(), String> {
            self.change(unique, Playback::Playing)
        }
    }

    use Playback::*;

    #[test]
    fn only_players_that_were_playing_are_resumed() {
        let bus = FakeBus::new(&[(":1.1", Playing), (":1.2", Paused), (":1.3", Stopped)]);
        let mut paused = PausedPlayers::default();
        paused.pause_playing(&bus);
        assert_eq!(bus.status_of(":1.1"), Some(Paused));

        paused.resume(&bus);
        assert_eq!(bus.status_of(":1.1"), Some(Playing));
        assert_eq!(bus.status_of(":1.2"), Some(Paused));
        assert_eq!(bus.status_of(":1.3"), Some(Stopped));
    }

    #[test]
    fn multiple_players_are_tracked_individually() {
        let bus = FakeBus::new(&[(":1.1", Playing), (":1.2", Playing)]);
        let mut paused = PausedPlayers::default();
        paused.pause_playing(&bus);
        assert_eq!(bus.status_of(":1.1"), Some(Paused));
        assert_eq!(bus.status_of(":1.2"), Some(Paused));
        paused.resume(&bus);
        assert_eq!(bus.status_of(":1.1"), Some(Playing));
        assert_eq!(bus.status_of(":1.2"), Some(Playing));
    }

    #[test]
    fn resume_respects_changes_made_while_recording() {
        let bus = FakeBus::new(&[(":1.1", Playing), (":1.2", Playing), (":1.3", Playing)]);
        let mut paused = PausedPlayers::default();
        paused.pause_playing(&bus);
        bus.set(":1.1", Some(Stopped)); // user stopped it
        bus.set(":1.2", None); // player quit
        bus.set(":1.4", Some(Paused)); // new player appeared
        paused.resume(&bus);
        assert_eq!(bus.status_of(":1.1"), Some(Stopped));
        assert_eq!(bus.status_of(":1.3"), Some(Playing));
        assert_eq!(bus.status_of(":1.4"), Some(Paused));

        // Resuming twice does nothing the second time.
        bus.set(":1.3", Some(Paused));
        paused.resume(&bus);
        assert_eq!(bus.status_of(":1.3"), Some(Paused));
    }

    #[test]
    fn a_second_pause_keeps_the_first_record() {
        let bus = FakeBus::new(&[(":1.1", Playing)]);
        let mut paused = PausedPlayers::default();
        paused.pause_playing(&bus);
        // Now paused by us; it must not be forgotten as "already paused".
        paused.pause_playing(&bus);
        paused.resume(&bus);
        assert_eq!(bus.status_of(":1.1"), Some(Playing));
    }

    #[test]
    fn unreachable_bus_or_no_players_is_harmless() {
        let mut paused = PausedPlayers::default();
        paused.pause_playing(&FakeBus::default());
        paused.resume(&FakeBus::default());
        let bus = FakeBus::new(&[]);
        paused.pause_playing(&bus);
        paused.resume(&bus);
        assert!(paused.players.is_empty());
    }

    #[test]
    fn disabled_handle_does_nothing() {
        let media = Media::new(false);
        media.pause();
        media.resume();
    }
}

//! Native `zwp_virtual_keyboard_v1` chord emission on a private Wayland
//! connection (never GTK's).

use std::fs::File;
use std::io::Write;
use std::os::fd::{AsFd, OwnedFd};
use std::thread;
use std::time::Duration;

use rustix::fs::{MemfdFlags, SealFlags};
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::{wl_registry, wl_seat};
use wayland_client::{Connection, Dispatch, EventQueue, QueueHandle, delegate_noop};
use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::{
    zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1,
    zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1,
};
use xkbcommon::xkb;

use super::chord::ChordPlan;

const KEY_RELEASED: u32 = 0;
const KEY_PRESSED: u32 = 1;

struct State;

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

delegate_noop!(State: ignore wl_seat::WlSeat);
delegate_noop!(State: ZwpVirtualKeyboardManagerV1);
delegate_noop!(State: ZwpVirtualKeyboardV1);

fn err(context: &str, e: impl std::fmt::Display) -> String {
    format!("virtual keyboard: {context}: {e}")
}

/// Press and release `plan`'s chord once on a new virtual keyboard.
pub fn send_chord(plan: &ChordPlan, key_delay: Duration) -> Result<(), String> {
    let conn = Connection::connect_to_env().map_err(|e| err("connect", e))?;
    let (globals, mut queue) =
        registry_queue_init::<State>(&conn).map_err(|e| err("registry", e))?;
    let qh = queue.handle();

    let seats = globals
        .contents()
        .with_list(|l| l.iter().filter(|g| g.interface == "wl_seat").count());
    if seats > 1 {
        tracing::warn!(seats, "multiple seats; using the first advertised one");
    }
    let seat: wl_seat::WlSeat = globals
        .bind(&qh, 1..=1, ())
        .map_err(|e| err("wl_seat", e))?;
    let manager: ZwpVirtualKeyboardManagerV1 = globals
        .bind(&qh, 1..=1, ())
        .map_err(|e| err("zwp_virtual_keyboard_manager_v1", e))?;

    let keyboard = manager.create_virtual_keyboard(&seat, &qh, ());
    let keymap = keymap_fd(&plan.keymap).map_err(|e| err("keymap memfd", e))?;
    keyboard.keymap(
        xkb::KEYMAP_FORMAT_TEXT_V1,
        keymap.as_fd(),
        plan.keymap.len() as u32,
    );
    roundtrip(&mut queue, "keymap upload")?;
    thread::sleep(key_delay);

    let result = press_chord(&keyboard, &mut queue, plan, key_delay);

    // Always leave the virtual keyboard with nothing held, even if a step
    // above failed. Errors here are secondary to `result`.
    keyboard.key(timestamp(), plan.key, KEY_RELEASED);
    keyboard.modifiers(0, 0, 0, 0);
    let release = roundtrip(&mut queue, "release");
    thread::sleep(key_delay);
    keyboard.destroy();
    let _ = conn.flush();

    result.and(release)
}

fn press_chord(
    keyboard: &ZwpVirtualKeyboardV1,
    queue: &mut EventQueue<State>,
    plan: &ChordPlan,
    key_delay: Duration,
) -> Result<(), String> {
    keyboard.modifiers(plan.mods, 0, 0, 0);
    roundtrip(queue, "modifiers")?;
    keyboard.key(timestamp(), plan.key, KEY_PRESSED);
    roundtrip(queue, "key press")?;
    thread::sleep(key_delay);
    keyboard.key(timestamp(), plan.key, KEY_RELEASED);
    roundtrip(queue, "key release")?;
    Ok(())
}

fn roundtrip(queue: &mut EventQueue<State>, step: &str) -> Result<(), String> {
    queue
        .roundtrip(&mut State)
        .map(drop)
        .map_err(|e| err(step, e))
}

/// Sealed, anonymous, read-only-by-contract keymap file.
fn keymap_fd(keymap: &[u8]) -> std::io::Result<OwnedFd> {
    let fd = rustix::fs::memfd_create(
        c"wayland-wispr-keymap",
        MemfdFlags::CLOEXEC | MemfdFlags::ALLOW_SEALING,
    )?;
    let mut file = File::from(fd);
    file.write_all(keymap)?;
    let fd = OwnedFd::from(file);
    rustix::fs::fcntl_add_seals(
        &fd,
        SealFlags::SHRINK | SealFlags::GROW | SealFlags::WRITE | SealFlags::SEAL,
    )?;
    Ok(fd)
}

/// Millisecond CLOCK_MONOTONIC timestamp, as real input devices use.
fn timestamp() -> u32 {
    let t = rustix::time::clock_gettime(rustix::time::ClockId::Monotonic);
    (t.tv_sec as u64)
        .wrapping_mul(1000)
        .wrapping_add(t.tv_nsec as u64 / 1_000_000) as u32
}

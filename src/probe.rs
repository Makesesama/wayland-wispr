//! Native Wayland capability probe.
//!
//! Uses its own `wayland-client` connection (never GTK's) and only lists
//! advertised globals; it does not bind or exercise any protocol.

use serde::Serialize;
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::wl_registry;
use wayland_client::{Connection, Dispatch, QueueHandle};

pub const LAYER_SHELL: &str = "zwlr_layer_shell_v1";
pub const VIRTUAL_KEYBOARD: &str = "zwp_virtual_keyboard_manager_v1";
pub const INPUT_METHOD: &str = "zwp_input_method_manager_v2";
pub const EXT_DATA_CONTROL: &str = "ext_data_control_manager_v1";
pub const WLR_DATA_CONTROL: &str = "zwlr_data_control_manager_v1";
pub const SEAT: &str = "wl_seat";

#[derive(Debug, thiserror::Error)]
pub enum ProbeError {
    #[error("cannot connect to a Wayland compositor: {0}")]
    Connect(#[from] wayland_client::ConnectError),
    #[error("failed to read Wayland globals: {0}")]
    Globals(#[from] wayland_client::globals::GlobalError),
}

#[derive(Debug, Clone, Serialize)]
pub struct Capabilities {
    /// Advertised version of each protocol, or `None` if missing.
    pub layer_shell: Option<u32>,
    pub virtual_keyboard: Option<u32>,
    pub input_method: Option<u32>,
    pub ext_data_control: Option<u32>,
    pub wlr_data_control: Option<u32>,
    pub seats: usize,
}

impl Capabilities {
    pub fn data_control(&self) -> bool {
        self.ext_data_control.is_some() || self.wlr_data_control.is_some()
    }

    /// Text can be committed as the seat's input method.
    pub fn can_commit(&self) -> bool {
        self.input_method.is_some() && self.seats > 0
    }

    /// Native clipboard + virtual-keyboard paste is possible.
    pub fn can_insert(&self) -> bool {
        self.data_control() && self.virtual_keyboard.is_some() && self.seats > 0
    }

    pub fn rows(&self) -> [(&'static str, Option<u32>); 5] {
        [
            (LAYER_SHELL, self.layer_shell),
            (INPUT_METHOD, self.input_method),
            (VIRTUAL_KEYBOARD, self.virtual_keyboard),
            (EXT_DATA_CONTROL, self.ext_data_control),
            (WLR_DATA_CONTROL, self.wlr_data_control),
        ]
    }
}

struct Probe;

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for Probe {
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

pub fn probe() -> Result<Capabilities, ProbeError> {
    let conn = Connection::connect_to_env()?;
    let (globals, _queue) = registry_queue_init::<Probe>(&conn)?;

    Ok(globals.contents().with_list(|list| {
        let version = |name: &str| {
            list.iter()
                .filter(|g| g.interface == name)
                .map(|g| g.version)
                .max()
        };
        Capabilities {
            layer_shell: version(LAYER_SHELL),
            virtual_keyboard: version(VIRTUAL_KEYBOARD),
            input_method: version(INPUT_METHOD),
            ext_data_control: version(EXT_DATA_CONTROL),
            wlr_data_control: version(WLR_DATA_CONTROL),
            seats: list.iter().filter(|g| g.interface == SEAT).count(),
        }
    }))
}

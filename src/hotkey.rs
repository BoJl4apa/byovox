//! Hotkey events and the trait every hotkey backend implements.

use std::sync::mpsc::Sender;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HotkeyEvent {
    Pressed,
    Released,
    Toggle,
    Cancel,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HotkeyMode {
    Hold,
    Toggle,
}

impl HotkeyMode {
    pub fn parse(s: &str) -> Option<HotkeyMode> {
        match s {
            "hold" => Some(HotkeyMode::Hold),
            "toggle" => Some(HotkeyMode::Toggle),
            _ => None,
        }
    }
}

/// W3C UI Events `code` names byovox accepts. Backends map these to their own codes.
pub const KEY_NAMES: &[&str] = &[
    "ControlLeft",
    "ControlRight",
    "AltLeft",
    "AltRight",
    "ShiftLeft",
    "ShiftRight",
    "MetaLeft",
    "MetaRight",
    "CapsLock",
    "ScrollLock",
    "Pause",
    "Insert",
    "Escape",
    "F13",
    "F14",
    "F15",
    "F16",
    "F17",
    "F18",
    "F19",
    "F20",
    "F21",
    "F22",
    "F23",
    "F24",
];

pub fn validate_key_name(name: &str) -> Result<(), String> {
    if KEY_NAMES.contains(&name) {
        Ok(())
    } else {
        Err(format!(
            "unknown key `{name}`; accepted: {}",
            KEY_NAMES.join(", ")
        ))
    }
}

/// A backend runs on its own thread and pushes events until the sender drops.
pub trait Hotkey: Send {
    fn run(self: Box<Self>, tx: Sender<HotkeyEvent>);
}

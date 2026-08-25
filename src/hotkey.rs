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

#[cfg(test)]
mod tests {
    use super::KEY_NAMES;
    use crate::config::EXAMPLE;

    /// grok-2: `docs/config.example.toml` is the reference a user reads before setting
    /// `hotkey.key`, and its list had drifted — `Insert`, `Escape`, `AltLeft`, `MetaLeft` and
    /// `MetaRight` were accepted by `validate_key_name` but named nowhere a reader looks.
    /// `check` prints the full list too, but only after a wrong guess has been refused.
    #[test]
    fn every_accepted_key_name_is_documented_in_the_example() {
        let missing: Vec<&str> = KEY_NAMES
            .iter()
            .copied()
            .filter(|name| !EXAMPLE.contains(name))
            .collect();
        assert!(
            missing.is_empty(),
            "accepted by hotkey::KEY_NAMES, absent from docs/config.example.toml: {missing:?}"
        );
    }
}

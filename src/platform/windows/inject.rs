//! Text insertion on Windows: SendInput with KEYEVENTF_UNICODE (script-agnostic),
//! paste via clipboard + Ctrl+V with clipboard restore, and clipboard-only.
//! UIPI blocks SendInput into elevated windows: `type` and `paste` then return Err and
//! the pipeline falls to the next rung.

use std::time::Duration;

use windows::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP, KEYEVENTF_UNICODE, SendInput,
    VIRTUAL_KEY,
};

use super::hotkey::INJECT_MARKER;
use crate::inject::Inject;

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum KeyEvent {
    Unicode { unit: u16, up: bool },
    Vk { vk: u16, up: bool },
}

/// Pure planner: every UTF-16 unit as a unicode down/up; '\n' as VK_RETURN down/up.
pub fn key_events(text: &str) -> Vec<KeyEvent> {
    let mut out = Vec::with_capacity(text.len() * 2);
    for ch in text.chars() {
        if ch == '\n' {
            out.push(KeyEvent::Vk {
                vk: 0x0D,
                up: false,
            });
            out.push(KeyEvent::Vk { vk: 0x0D, up: true });
            continue;
        }
        if ch == '\r' {
            continue;
        }
        let mut units = [0u16; 2];
        for unit in ch.encode_utf16(&mut units) {
            out.push(KeyEvent::Unicode {
                unit: *unit,
                up: false,
            });
            out.push(KeyEvent::Unicode {
                unit: *unit,
                up: true,
            });
        }
    }
    out
}

fn to_input(ev: KeyEvent) -> INPUT {
    let ki = match ev {
        KeyEvent::Unicode { unit, up } => KEYBDINPUT {
            wVk: VIRTUAL_KEY(0),
            wScan: unit,
            dwFlags: if up {
                KEYEVENTF_UNICODE | KEYEVENTF_KEYUP
            } else {
                KEYEVENTF_UNICODE
            },
            time: 0,
            dwExtraInfo: INJECT_MARKER,
        },
        KeyEvent::Vk { vk, up } => KEYBDINPUT {
            wVk: VIRTUAL_KEY(vk),
            wScan: 0,
            dwFlags: if up {
                KEYEVENTF_KEYUP
            } else {
                Default::default()
            },
            time: 0,
            dwExtraInfo: INJECT_MARKER,
        },
    };
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 { ki },
    }
}

fn send(events: &[KeyEvent]) -> Result<(), String> {
    for chunk in events.chunks(64) {
        let inputs: Vec<INPUT> = chunk.iter().copied().map(to_input).collect();
        // SAFETY: inputs is a valid array of INPUT for the call's duration.
        let sent = unsafe { SendInput(&inputs, size_of::<INPUT>() as i32) };
        if sent as usize != inputs.len() {
            return Err(format!(
                "SendInput inserted {sent}/{} events (elevated window?)",
                inputs.len()
            ));
        }
    }
    Ok(())
}

pub struct TypeInject;
impl Inject for TypeInject {
    fn name(&self) -> &'static str {
        "type"
    }
    fn inject(&mut self, text: &str) -> Result<(), String> {
        send(&key_events(text))
    }
}

fn ctrl_v() -> Vec<KeyEvent> {
    vec![
        KeyEvent::Vk {
            vk: 0x11,
            up: false,
        },
        KeyEvent::Vk {
            vk: 0x56,
            up: false,
        },
        KeyEvent::Vk { vk: 0x56, up: true },
        KeyEvent::Vk { vk: 0x11, up: true },
    ]
}

pub struct PasteInject;
impl Inject for PasteInject {
    fn name(&self) -> &'static str {
        "paste"
    }
    fn inject(&mut self, text: &str) -> Result<(), String> {
        let mut cb = arboard::Clipboard::new().map_err(|e| format!("clipboard: {e}"))?;
        // Only text is saved: a clipboard holding an image is replaced and not restored.
        let previous = cb.get_text().ok();
        cb.set_text(text)
            .map_err(|e| format!("clipboard set: {e}"))?;
        let sent = send(&ctrl_v());
        // Let the target window read the clipboard before it changes back under it.
        std::thread::sleep(Duration::from_millis(150));
        if let Some(p) = previous
            && let Err(e) = cb.set_text(p)
        {
            tracing::warn!(error = %e, "could not restore the previous clipboard text");
        }
        sent
    }
}

pub struct ClipboardOnlyInject;
impl Inject for ClipboardOnlyInject {
    fn name(&self) -> &'static str {
        "clipboard-only"
    }
    fn inject(&mut self, text: &str) -> Result<(), String> {
        arboard::Clipboard::new()
            .and_then(|mut cb| cb.set_text(text))
            .map_err(|e| format!("clipboard: {e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plans_unicode_units_and_enter_for_newlines() {
        let ev = key_events("a\n");
        assert_eq!(
            ev,
            vec![
                KeyEvent::Unicode {
                    unit: 'a' as u16,
                    up: false
                },
                KeyEvent::Unicode {
                    unit: 'a' as u16,
                    up: true
                },
                KeyEvent::Vk {
                    vk: 0x0D,
                    up: false
                },
                KeyEvent::Vk { vk: 0x0D, up: true },
            ]
        );
    }

    #[test]
    fn non_bmp_becomes_surrogate_pairs() {
        let ev = key_events("😀");
        let units: Vec<u16> = ev
            .iter()
            .filter_map(|e| match e {
                KeyEvent::Unicode { unit, up: false } => Some(*unit),
                _ => None,
            })
            .collect();
        assert_eq!(units, vec![0xD83D, 0xDE00]);
    }

    #[test]
    fn hebrew_and_cyrillic_are_plain_units() {
        assert_eq!(key_events("א").len(), 2);
        assert_eq!(key_events("ю").len(), 2);
    }

    /// Every event we synthesise must be stamped so the hook can ignore it; otherwise the
    /// paste chord's Ctrl would read as a hotkey press.
    #[test]
    fn every_planned_input_carries_the_inject_marker() {
        let planned: Vec<KeyEvent> = key_events("a\n😀").into_iter().chain(ctrl_v()).collect();
        assert!(!planned.is_empty());
        for ev in planned {
            let input = to_input(ev);
            // SAFETY: `to_input` just wrote the `ki` arm of this union.
            let extra = unsafe { input.Anonymous.ki.dwExtraInfo };
            assert_eq!(extra, INJECT_MARKER, "{ev:?} lost the marker");
        }
    }

    /// `#[ignore]`d: it writes the **real** system clipboard. It restores whatever text was
    /// there, but a non-text clipboard (an image) cannot be saved and is lost. `type` and
    /// `paste` have no such test on purpose — they would send keys into whatever window has
    /// focus, i.e. the console running `cargo`. Run with `cargo test -- --ignored`.
    #[test]
    #[ignore]
    fn clipboard_only_sets_the_clipboard() {
        const PROBE: &str = "byovox clipboard probe שלום мир 😀";

        let mut cb = arboard::Clipboard::new().expect("clipboard");
        let previous = cb.get_text().ok();
        println!("previous clipboard text: {previous:?}");

        ClipboardOnlyInject.inject(PROBE).expect("inject");
        let got = cb.get_text().expect("read back");
        println!("read back: {got:?}");
        assert_eq!(got, PROBE);

        if let Some(p) = previous {
            cb.set_text(p).expect("restore");
            println!("restored: {:?}", cb.get_text().ok());
        }
    }
}

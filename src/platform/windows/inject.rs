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

/// Events per `SendInput` call. Must be at least 4 — the longest character — or `chunks`
/// could not find a boundary to end on.
const CHUNK: usize = 64;

/// True where a chunk may end: the start of a character's events. `key_events` emits a
/// character as a key-down first, and the only event that begins with a down while
/// *continuing* a character is a low surrogate's.
fn is_boundary(ev: KeyEvent) -> bool {
    match ev {
        KeyEvent::Unicode { unit, up } => !up && !(0xDC00..=0xDFFF).contains(&unit),
        KeyEvent::Vk { up, .. } => !up,
    }
}

/// Split the plan into `SendInput` batches. One call is atomic, but consecutive calls are
/// not: another process's keystroke can land between them. So a batch never ends inside one
/// character — never between a key-down and its up, and never between a high surrogate and
/// its low half, which would put a lone half-character in the target window.
///
/// Chunking on a fixed multiple of 4 does *not* give that: a character is 2 or 4 events, so
/// a surrogate pair can start at an offset of 2 (mod 4) — in "a😀" the pair occupies events
/// 2..6 and a boundary at 4 splits it. Ending only on `is_boundary` is what holds.
fn chunks(events: &[KeyEvent]) -> Vec<&[KeyEvent]> {
    let mut out = Vec::new();
    let mut start = 0;
    while start < events.len() {
        let mut end = (start + CHUNK).min(events.len());
        // At most 3 steps back, so `end` stays greater than `start`.
        while end < events.len() && !is_boundary(events[end]) {
            end -= 1;
        }
        out.push(&events[start..end]);
        start = end;
    }
    out
}

fn send(events: &[KeyEvent]) -> Result<(), String> {
    for chunk in chunks(events) {
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

    /// A batch must never end inside a character. "a" then 40 emoji is the case a fixed
    /// chunk size gets wrong: 162 events (not a multiple of 64), and every surrogate pair
    /// after the "a" sits at an offset of 2 (mod 4), so a boundary at 64 or 128 would fall
    /// between a high surrogate and its low half.
    #[test]
    fn a_batch_never_ends_inside_a_character() {
        let ev = key_events(&format!("a{}", "😀".repeat(40)));
        assert_eq!(ev.len(), 162);
        assert_ne!(ev.len() % CHUNK, 0);

        let batches = chunks(&ev);
        let flat: Vec<KeyEvent> = batches.iter().flat_map(|b| b.iter().copied()).collect();
        assert_eq!(flat, ev, "chunking must not lose or reorder events");

        let is_unit = |e: &&KeyEvent, range: std::ops::RangeInclusive<u16>| matches!(e, KeyEvent::Unicode { unit, .. } if range.contains(unit));
        for b in &batches {
            assert!(
                !b.is_empty() && b.len() <= CHUNK,
                "bad batch length {}",
                b.len()
            );
            assert!(is_boundary(b[0]), "batch starts mid-character: {:?}", b[0]);
            assert_eq!(b.len() % 2, 0, "batch splits a down/up pair");
            let highs = b.iter().filter(|e| is_unit(e, 0xD800..=0xDBFF)).count();
            let lows = b.iter().filter(|e| is_unit(e, 0xDC00..=0xDFFF)).count();
            assert_eq!(highs, lows, "a surrogate pair straddles a batch boundary");
        }
    }

    /// The paste chord must reach the target as one atomic call: a Ctrl-down whose V never
    /// arrives leaves the modifier stuck in the target window.
    #[test]
    fn the_paste_chord_is_one_batch() {
        assert_eq!(chunks(&ctrl_v()).len(), 1);
    }

    #[test]
    fn nothing_to_type_is_no_batches_at_all() {
        assert!(chunks(&key_events("")).is_empty());
        assert!(chunks(&key_events("\r")).is_empty());
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

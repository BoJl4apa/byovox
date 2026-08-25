//! WH_KEYBOARD_LL hook: press/release for any key, bare modifiers included, and for chords
//! (`ControlLeft+ShiftLeft+Z`) whose trigger the focused window never sees. The hook
//! procedure is a plain function, so the chord state and the event sender live in statics;
//! the message pump runs on the thread `run` is called on.
//!
//! The rules are `hotkey::ChordTracker`'s — this module only maps virtual keys onto its
//! `ChordKey`s and does what its `Step` says.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Mutex, OnceLock};

use windows::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP, SendInput,
    VIRTUAL_KEY,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, DispatchMessageW, GetMessageW, KBDLLHOOKSTRUCT, MSG, SetWindowsHookExW,
    TranslateMessage, WH_KEYBOARD_LL, WM_KEYDOWN, WM_KEYUP, WM_SYSKEYDOWN, WM_SYSKEYUP,
};

use crate::hotkey::{
    Chord, ChordKey, ChordTracker, Hotkey, HotkeyEvent, Step, parse_chord, validate_key_name,
};

static SENDER: OnceLock<Mutex<Option<Sender<HotkeyEvent>>>> = OnceLock::new();
static CHORD: OnceLock<Mutex<Option<ChordHook>>> = OnceLock::new();
static CANCEL_VK: AtomicU32 = AtomicU32::new(0);
/// The tray's Enable/Disable, mirrored where the hook procedure can read it: a daemon that
/// was told to stop listening must not swallow the keystroke it is not going to act on.
///
/// A mirror rather than a reference to `pipeline::Shared` because the backend is built
/// before that exists — `daemon::start` calls `platform::detect` well before `Pipeline::new`
/// — and because a hook procedure is a plain function whose state lives in statics anyway.
/// `Shared::enabled` has exactly one writer (the tray's Enable/Disable), which is the one
/// place this is written, so the two cannot disagree.
static ARMED: AtomicBool = AtomicBool::new(true);
/// True once a hook is live. Guards against a second `run`, whose chained hook would
/// double every event, and lets a caller wait for the hook to be installed.
///
/// Assumes one `run` per process — `main` calls it once, from the single thread it spawns,
/// with the backend moved into that closure — which is why the load-then-store is not a race.
/// It is never cleared, because the hook lives until process exit: nothing unhooks it.
static INSTALLED: AtomicBool = AtomicBool::new(false);

/// Stamped into `dwExtraInfo` of every event `platform::windows::inject` sends, so the hook
/// can tell our own synthetic keys from the user's. The hook cannot filter `LLKHF_INJECTED`
/// instead: its own probe injects the keys it tests with.
pub const INJECT_MARKER: usize = 0x00B7_0B0C;

/// An unassigned virtual key, tapped once when a chord fires so the shell sees "a key was
/// pressed with those modifiers".
///
/// Windows acts on a modifier combination *released without another key*: Ctrl+Left Shift
/// sets left-to-right and Ctrl+Right Shift right-to-left in RTL-aware editors, and
/// Alt+Shift / Ctrl+Shift is the layout-switch hotkey where it is enabled. The trigger is
/// swallowed, so without this tap the app would see exactly that bare combination and act on
/// it. `0xFF` is reserved and assigned to nothing, so an application ignores it — the same
/// trick AutoHotkey uses to keep Alt from opening a menu bar.
const DEFUSE_VK: u16 = 0xFF;

/// Arm or disarm the hotkey. Called from the tray beside its write to `Shared::enabled`;
/// while disarmed a chord's trigger types as usual and starts nothing, and a chord already
/// latched still finishes the swallow it began.
pub fn set_armed(on: bool) {
    ARMED.store(on, Ordering::Relaxed);
}

fn sender_slot() -> &'static Mutex<Option<Sender<HotkeyEvent>>> {
    SENDER.get_or_init(|| Mutex::new(None))
}

fn chord_slot() -> &'static Mutex<Option<ChordHook>> {
    CHORD.get_or_init(|| Mutex::new(None))
}

pub fn vk_for(name: &str) -> Option<u32> {
    Some(match name {
        "ControlLeft" => 0xA2,
        "ControlRight" => 0xA3,
        "AltLeft" => 0xA4,
        "AltRight" => 0xA5,
        "ShiftLeft" => 0xA0,
        "ShiftRight" => 0xA1,
        "MetaLeft" => 0x5B,
        "MetaRight" => 0x5C,
        "CapsLock" => 0x14,
        "ScrollLock" => 0x91,
        "Pause" => 0x13,
        "Insert" => 0x2D,
        "Escape" => 0x1B,
        // A chord's trigger: `A`-`Z` are VK 0x41-0x5A and `0`-`9` are 0x30-0x39, their own
        // ASCII codes. A virtual key is assigned by the *active layout*, not by the key's
        // position: a non-Latin layout (Cyrillic, Hebrew) keeps the US assignments for this
        // block, so `Z` is the same key there even though it types я or ז — while another
        // Latin layout follows its own printed letters, and on QWERTZ `Z` is the key QWERTY
        // labels Y. Before the `F` arm, so a bare `F` is the letter and `F13` the function key.
        c if c.len() == 1 => {
            let b = c.as_bytes()[0];
            if !b.is_ascii_uppercase() && !b.is_ascii_digit() {
                return None;
            }
            u32::from(b)
        }
        f if f.starts_with('F') => {
            // Exactly two ASCII digits: `parse` alone would accept `F+13` and `F0013`.
            let digits = &f[1..];
            if digits.len() != 2 || !digits.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            let n: u32 = digits.parse().ok()?;
            if (13..=24).contains(&n) {
                0x70 + n - 1
            } else {
                return None;
            }
        }
        _ => return None,
    })
}

pub struct HookHotkey {
    chord: Chord,
    /// The chord's virtual keys: its modifiers in order, then the trigger.
    vks: Vec<u32>,
    cancel_vk: u32,
}

impl HookHotkey {
    pub fn new(key: &str, cancel_key: &str) -> Result<HookHotkey, String> {
        let chord = parse_chord(key)?;
        // The cancel key takes no chord and no letter: it is one of the names a bare hotkey
        // may be, because it is pressed on its own while recording.
        validate_key_name(cancel_key)?;
        let mut vks = Vec::with_capacity(chord.modifiers.len() + 1);
        for name in chord
            .modifiers
            .iter()
            .chain(std::iter::once(&chord.trigger))
        {
            vks.push(vk_for(name).ok_or_else(|| format!("no virtual key for `{name}`"))?);
        }
        let cancel_vk =
            vk_for(cancel_key).ok_or_else(|| format!("no virtual key for `{cancel_key}`"))?;
        if vks.contains(&cancel_vk) {
            // The chord branch wins in `hook_proc`, so cancel could never fire.
            return Err(if chord.modifiers.is_empty() {
                format!("hotkey and cancel key are both `{key}`; they must differ")
            } else {
                format!(
                    "cancel key `{cancel_key}` is part of the hotkey `{chord}`; they must differ"
                )
            });
        }
        Ok(HookHotkey {
            chord,
            vks,
            cancel_vk,
        })
    }
}

/// The chord as the hook procedure needs it: the virtual keys to watch, and the state
/// machine that decides what each one means.
struct ChordHook {
    vks: Vec<u32>,
    tracker: ChordTracker,
}

impl ChordHook {
    /// Which member of the chord this virtual key is, or `None` for every other key on the
    /// keyboard. The trigger is the last entry, by construction in `HookHotkey::new`.
    fn key_for(&self, vk: u32) -> Option<ChordKey> {
        let i = self.vks.iter().position(|v| *v == vk)?;
        if i + 1 == self.vks.len() {
            Some(ChordKey::Trigger)
        } else {
            Some(ChordKey::Modifier(i))
        }
    }
}

fn emit(ev: HotkeyEvent) {
    // Recovering from poisoning rather than unwrapping: a panic here would unwind out of
    // an `extern "system"` callback, which aborts the process.
    if let Some(m) = SENDER.get()
        && let Some(tx) = m.lock().unwrap_or_else(|p| p.into_inner()).as_ref()
    {
        let _ = tx.send(ev);
    }
}

/// The two events the defuse tap sends. Pure, so a test can read the virtual key and the
/// marker off it — a defuse that lost the marker would read as a hotkey press of its own.
fn defuse_inputs() -> [INPUT; 2] {
    [false, true].map(|up| INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(DEFUSE_VK),
                wScan: 0,
                dwFlags: if up {
                    KEYEVENTF_KEYUP
                } else {
                    Default::default()
                },
                time: 0,
                dwExtraInfo: INJECT_MARKER,
            },
        },
    })
}

/// Tap `DEFUSE_VK` so the modifiers the app is still holding cannot read as a bare
/// combination. Sent from the hook procedure itself: two events, and `SendInput` only queues
/// them — the hook is re-entered for them after this call returns, and the marker makes that
/// re-entry a no-op.
///
/// The result is deliberately dropped, and this is the one place in the hook where a failure
/// is silent (there is no logging from inside a hook procedure to report it with). Both
/// refusal paths arrive with nothing left to defuse: `SendInput` is refused when the
/// foreground window is elevated — and then UIPI never showed the hook the chord either —
/// and while another thread holds `BlockInput`, which is blocking the very input this hook
/// would not have seen.
fn defuse_modifiers() {
    let inputs = defuse_inputs();
    // SAFETY: two well-formed INPUTs, size declared as the API requires.
    let _ = unsafe { SendInput(&inputs, size_of::<INPUT>() as i32) };
}

/// The tracker's verdict for one key event, or `None` when the key is no part of the chord.
/// Takes and releases the chord lock without emitting anything: `emit` takes a second lock,
/// and a hook procedure holding two of them is a deadlock waiting to happen.
fn step_for(vk: u32, down: bool) -> Option<Step> {
    let slot = CHORD.get()?;
    // Recovering from poisoning rather than unwrapping, as `emit` does: a panic in an
    // `extern "system"` callback aborts the process.
    let mut guard = slot.lock().unwrap_or_else(|p| p.into_inner());
    let hook = guard.as_mut()?;
    let key = hook.key_for(vk)?;
    // The tracker sees every event whatever the arming: a modifier released while the daemon
    // is disabled has to clear its flag, or it would be stuck down when the daemon is armed
    // again.
    let armed = ARMED.load(Ordering::Relaxed);
    if key == ChordKey::Trigger && down && hook.tracker.would_fire() {
        // The flags say the chord is held, and this keystroke is about to be swallowed on
        // the strength of them — so check them against the OS first. A low-level hook is
        // per-desktop: hold Ctrl+Shift into a UAC prompt (Ctrl+Shift+click *is* the "run as
        // administrator" gesture) or into Win+L and the key-ups are delivered to that
        // desktop, never to us, leaving both flags stuck down. The next plain `z` would then
        // satisfy the chord and be eaten, and every one after it.
        //
        // Two state reads on the one keystroke that could fire, none on any other key. It
        // also fixes the opposite staleness for free: a modifier held from before the daemon
        // started is discovered here rather than never.
        //
        // `vks` always ends with the trigger, so these indices are the modifiers. The
        // returned `Step`s are dropped, and nothing is lost with them: `would_fire` is false
        // while the chord is held, so nothing is latched here and a modifier fed now can
        // emit no event.
        for i in 0..hook.vks.len() - 1 {
            let vk = hook.vks[i];
            // SAFETY: a plain keyboard-state read, no arguments to get wrong.
            let held = unsafe { GetAsyncKeyState(vk as i32) } as u16 & 0x8000 != 0;
            hook.tracker.feed(ChordKey::Modifier(i), held, armed);
        }
    }
    Some(hook.tracker.feed(key, down, armed))
}

unsafe extern "system" fn hook_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code >= 0 {
        // SAFETY: for WH_KEYBOARD_LL, lparam points at a KBDLLHOOKSTRUCT for the call's duration.
        let kb = unsafe { &*(lparam.0 as *const KBDLLHOOKSTRUCT) };
        // Text we typed or pasted ourselves, and our own defuse tap: a paste chord's Ctrl
        // must not read as a hotkey press. The probe below stamps 0, so it still drives the
        // hook.
        if kb.dwExtraInfo == INJECT_MARKER {
            return unsafe { CallNextHookEx(None, code, wparam, lparam) };
        }
        let msg = wparam.0 as u32;
        let is_down = msg == WM_KEYDOWN || msg == WM_SYSKEYDOWN;
        let is_up = msg == WM_KEYUP || msg == WM_SYSKEYUP;
        if is_down || is_up {
            match step_for(kb.vkCode, is_down) {
                Some(step) => {
                    if let Some(ev) = step.event {
                        if ev == HotkeyEvent::Pressed && step.swallow {
                            // A chord fired and its trigger is being swallowed, so the app is
                            // left holding modifiers it never saw a key pressed with.
                            defuse_modifiers();
                        }
                        emit(ev);
                    }
                    if step.swallow {
                        // 1 ends the chain: the focused window never sees the trigger, which
                        // is the whole point of a chord whose trigger is a letter.
                        return LRESULT(1);
                    }
                }
                // The chord branch wins, as it always has: a cancel key that is part of the
                // chord could never fire, which is why `HookHotkey::new` refuses one.
                None if is_down && kb.vkCode == CANCEL_VK.load(Ordering::Relaxed) => {
                    emit(HotkeyEvent::Cancel);
                }
                None => {}
            }
        }
    }
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

impl Hotkey for HookHotkey {
    fn run(self: Box<Self>, tx: Sender<HotkeyEvent>) {
        if INSTALLED.load(Ordering::SeqCst) {
            // Checked before the sender is stored, so the running hook keeps its consumer.
            tracing::error!("a keyboard hook is already installed; refusing to install a second");
            return;
        }
        let HookHotkey {
            chord,
            vks,
            cancel_vk,
        } = *self;
        let tracker = ChordTracker::new(&chord);
        *chord_slot().lock().unwrap_or_else(|p| p.into_inner()) = Some(ChordHook { vks, tracker });
        CANCEL_VK.store(cancel_vk, Ordering::Relaxed);
        *sender_slot().lock().unwrap_or_else(|p| p.into_inner()) = Some(tx);
        // SAFETY: standard hook installation; the hook is removed when the process exits.
        let hook = unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, Some(hook_proc), None, 0) };
        if let Err(e) = hook {
            tracing::error!(error = %e, "SetWindowsHookExW failed");
            // Drop the sender so the consumer's receive ends instead of blocking forever.
            *sender_slot().lock().unwrap_or_else(|p| p.into_inner()) = None;
            return;
        }
        INSTALLED.store(true, Ordering::SeqCst);
        let mut msg = MSG::default();
        // SAFETY: plain message pump on this thread; GetMessageW returns 0 on WM_QUIT.
        unsafe {
            while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn w3c_names_map_to_virtual_keys() {
        assert_eq!(vk_for("ControlRight"), Some(0xA3));
        assert_eq!(vk_for("ControlLeft"), Some(0xA2));
        assert_eq!(vk_for("AltRight"), Some(0xA5));
        assert_eq!(vk_for("F13"), Some(0x7C));
        assert_eq!(vk_for("F24"), Some(0x87));
        assert_eq!(vk_for("Escape"), Some(0x1B));
        assert_eq!(vk_for("Nope"), None);
        assert_eq!(vk_for("F+13"), None);
        assert_eq!(vk_for("F0013"), None);
    }

    /// A chord's trigger. The VK is the *physical* key, which is what makes the chord work
    /// under a layout that types something else on it — and `F` is the letter, not a
    /// function key with its number missing.
    #[test]
    fn letters_and_digits_map_to_their_own_ascii_codes() {
        assert_eq!(vk_for("A"), Some(0x41));
        assert_eq!(vk_for("Z"), Some(0x5A));
        assert_eq!(vk_for("0"), Some(0x30));
        assert_eq!(vk_for("9"), Some(0x39));
        assert_eq!(vk_for("F"), Some(0x46));
        assert_eq!(vk_for("z"), None);
        assert_eq!(vk_for("+"), None);
        assert_eq!(vk_for(""), None);
    }

    #[test]
    fn every_accepted_key_name_has_a_vk() {
        for name in crate::hotkey::KEY_NAMES {
            assert!(vk_for(name).is_some(), "{name}");
        }
    }

    #[test]
    fn hotkey_and_cancel_key_must_differ() {
        match HookHotkey::new("ControlRight", "ControlRight") {
            Err(e) => assert!(e.contains("must differ"), "{e}"),
            Ok(_) => panic!("the same key for hotkey and cancel must be rejected"),
        }
        assert!(HookHotkey::new("ControlRight", "Escape").is_ok());
    }

    /// Same rule against a chord: the chord branch wins in `hook_proc`, so a cancel key that
    /// is any member of it — modifier or trigger — could never fire.
    #[test]
    fn a_cancel_key_that_is_part_of_the_chord_is_refused() {
        for cancel in ["ControlLeft", "ShiftLeft", "Insert"] {
            match HookHotkey::new("ControlLeft+ShiftLeft+Insert", cancel) {
                Err(e) => {
                    assert!(e.contains("must differ"), "{cancel}: {e}");
                    assert!(e.contains("ControlLeft+ShiftLeft+Insert"), "{cancel}: {e}");
                }
                Ok(_) => panic!("`{cancel}` is part of the chord and must be refused"),
            }
        }
        assert!(HookHotkey::new("ControlLeft+ShiftLeft+Z", "Escape").is_ok());
    }

    /// What the hook procedure does on every keystroke: turn a virtual key back into the
    /// chord member it is. The trigger is last, and every other key on the keyboard is
    /// `None` — including the cancel key, whose own branch is reached that way.
    #[test]
    fn the_chords_virtual_keys_map_back_to_its_members() {
        let h = HookHotkey::new("ControlLeft+ShiftLeft+Z", "Escape").unwrap();
        assert_eq!(h.vks, vec![0xA2, 0xA0, 0x5A]);
        let hook = ChordHook {
            tracker: ChordTracker::new(&h.chord),
            vks: h.vks,
        };
        assert_eq!(hook.key_for(0xA2), Some(ChordKey::Modifier(0)));
        assert_eq!(hook.key_for(0xA0), Some(ChordKey::Modifier(1)));
        assert_eq!(hook.key_for(0x5A), Some(ChordKey::Trigger));
        assert_eq!(hook.key_for(0x1B), None);
        assert_eq!(
            hook.key_for(0xA3),
            None,
            "the other Ctrl is not this chord's"
        );
    }

    /// The tap that keeps a swallowed trigger from leaving the app with a bare Ctrl+Shift —
    /// which flips text direction, or switches the layout. It has to be a key nothing is
    /// bound to, and it has to carry the marker, or the hook would read its own tap as a
    /// keystroke.
    #[test]
    fn the_defuse_tap_is_an_unassigned_key_stamped_with_the_marker() {
        for (i, input) in defuse_inputs().iter().enumerate() {
            assert_eq!(input.r#type, INPUT_KEYBOARD);
            // SAFETY: `defuse_inputs` builds keyboard inputs, so `ki` is the live union field.
            let ki = unsafe { input.Anonymous.ki };
            assert_eq!(
                ki.wVk,
                VIRTUAL_KEY(0xFF),
                "an assigned key would reach the app"
            );
            assert_eq!(ki.dwExtraInfo, INJECT_MARKER);
            assert_eq!(
                ki.dwFlags,
                if i == 1 {
                    KEYEVENTF_KEYUP
                } else {
                    Default::default()
                },
                "down then up"
            );
        }
    }

    /// Drives the real hook with synthetic keystrokes: `SendInput` events reach a
    /// `WH_KEYBOARD_LL` hook, so this is the one executable check that the backend
    /// latches auto-repeat and reports press/release/cancel.
    ///
    /// `#[ignore]`d, and never run in CI, because it is not a self-contained unit test:
    /// it installs a **global** hook that is never unhooked and leaves a message-pumping
    /// thread alive for the rest of the process, so it cannot run twice in one process
    /// (the `INSTALLED` guard refuses the second install); and it injects **real global
    /// keystrokes** — the Escape it sends goes to whatever window has focus, not to the
    /// test. Run it deliberately, on a desktop session, with `cargo test -- --ignored`.
    #[test]
    #[ignore]
    fn injected_keys_drive_the_hook() {
        use std::sync::mpsc::{RecvTimeoutError, channel};
        use std::thread::sleep;
        use std::time::{Duration, Instant};
        use windows::Win32::UI::Input::KeyboardAndMouse::{
            INPUT, INPUT_0, INPUT_KEYBOARD, KEYBD_EVENT_FLAGS, KEYBDINPUT, KEYEVENTF_EXTENDEDKEY,
            KEYEVENTF_KEYUP, SendInput, VIRTUAL_KEY,
        };

        fn key(vk: u16, flags: KEYBD_EVENT_FLAGS) {
            let input = INPUT {
                r#type: INPUT_KEYBOARD,
                Anonymous: INPUT_0 {
                    ki: KEYBDINPUT {
                        wVk: VIRTUAL_KEY(vk),
                        wScan: 0,
                        dwFlags: flags,
                        time: 0,
                        dwExtraInfo: 0,
                    },
                },
            };
            // SAFETY: one well-formed INPUT, size declared as the API requires.
            let sent = unsafe { SendInput(&[input], size_of::<INPUT>() as i32) };
            assert_eq!(sent, 1, "SendInput refused vk {vk:#04X}");
        }

        const RCONTROL: u16 = 0xA3;
        const ESCAPE: u16 = 0x1B;

        let (tx, rx) = channel();
        std::thread::spawn(move || {
            Box::new(HookHotkey::new("ControlRight", "Escape").unwrap()).run(tx)
        });

        // Wait for the install rather than guessing at a sleep: injecting before the hook
        // is live would drop the events and fail the assert for the wrong reason.
        let deadline = Instant::now() + Duration::from_secs(2);
        while !INSTALLED.load(Ordering::SeqCst) {
            assert!(Instant::now() < deadline, "hook not installed within 2 s");
            sleep(Duration::from_millis(10));
        }

        key(RCONTROL, KEYEVENTF_EXTENDEDKEY); // first down -> Pressed
        sleep(Duration::from_secs(2));
        key(RCONTROL, KEYEVENTF_EXTENDEDKEY); // stands in for auto-repeat -> nothing
        key(RCONTROL, KEYEVENTF_EXTENDEDKEY | KEYEVENTF_KEYUP); // up -> Released
        key(ESCAPE, KEYBD_EVENT_FLAGS(0)); // down -> Cancel
        key(ESCAPE, KEYEVENTF_KEYUP); // up -> nothing

        let mut got = Vec::new();
        loop {
            match rx.recv_timeout(Duration::from_secs(3)) {
                Ok(ev) => got.push(ev),
                Err(RecvTimeoutError::Timeout) => break,
                Err(RecvTimeoutError::Disconnected) => panic!("hook thread exited: {got:?}"),
            }
        }
        println!("observed: {got:?}");
        assert_eq!(
            got,
            vec![
                HotkeyEvent::Pressed,
                HotkeyEvent::Released,
                HotkeyEvent::Cancel
            ]
        );
    }
}

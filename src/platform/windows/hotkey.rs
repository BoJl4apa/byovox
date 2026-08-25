//! WH_KEYBOARD_LL hook: press/release for any key, bare modifiers included. The hook
//! procedure is a plain function, so the target key and the event sender live in
//! statics; the message pump runs on the thread `run` is called on.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Mutex, OnceLock};

use windows::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, DispatchMessageW, GetMessageW, KBDLLHOOKSTRUCT, MSG, SetWindowsHookExW,
    TranslateMessage, WH_KEYBOARD_LL, WM_KEYDOWN, WM_KEYUP, WM_SYSKEYDOWN, WM_SYSKEYUP,
};

use crate::hotkey::{Hotkey, HotkeyEvent};

static SENDER: OnceLock<Mutex<Option<Sender<HotkeyEvent>>>> = OnceLock::new();
static TARGET_VK: AtomicU32 = AtomicU32::new(0);
static CANCEL_VK: AtomicU32 = AtomicU32::new(0);
static DOWN: AtomicBool = AtomicBool::new(false);
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

fn sender_slot() -> &'static Mutex<Option<Sender<HotkeyEvent>>> {
    SENDER.get_or_init(|| Mutex::new(None))
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
    vk: u32,
    cancel_vk: u32,
}

impl HookHotkey {
    pub fn new(key: &str, cancel_key: &str) -> Result<HookHotkey, String> {
        let vk = vk_for(key).ok_or_else(|| format!("no virtual key for `{key}`"))?;
        let cancel_vk =
            vk_for(cancel_key).ok_or_else(|| format!("no virtual key for `{cancel_key}`"))?;
        if vk == cancel_vk {
            // The target branch wins in `hook_proc`, so cancel could never fire.
            return Err(format!(
                "hotkey and cancel key are both `{key}`; they must differ"
            ));
        }
        Ok(HookHotkey { vk, cancel_vk })
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

unsafe extern "system" fn hook_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code >= 0 {
        // SAFETY: for WH_KEYBOARD_LL, lparam points at a KBDLLHOOKSTRUCT for the call's duration.
        let kb = unsafe { &*(lparam.0 as *const KBDLLHOOKSTRUCT) };
        // Text we typed or pasted ourselves: a paste chord's Ctrl must not read as a hotkey
        // press. The probe below stamps 0, so it still drives the hook.
        if kb.dwExtraInfo == INJECT_MARKER {
            return unsafe { CallNextHookEx(None, code, wparam, lparam) };
        }
        let msg = wparam.0 as u32;
        let is_down = msg == WM_KEYDOWN || msg == WM_SYSKEYDOWN;
        let is_up = msg == WM_KEYUP || msg == WM_SYSKEYUP;
        if kb.vkCode == TARGET_VK.load(Ordering::Relaxed) {
            if is_down && !DOWN.swap(true, Ordering::Relaxed) {
                emit(HotkeyEvent::Pressed); // first down only; auto-repeat is ignored
            } else if is_up && DOWN.swap(false, Ordering::Relaxed) {
                // Only a latched press releases: a key already held when the daemon
                // started, a second keyboard's key-up, or a stuck-key clear from another
                // process must not produce a `Released` with no `Pressed` before it.
                emit(HotkeyEvent::Released);
            }
        } else if kb.vkCode == CANCEL_VK.load(Ordering::Relaxed) && is_down {
            emit(HotkeyEvent::Cancel);
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
        TARGET_VK.store(self.vk, Ordering::Relaxed);
        CANCEL_VK.store(self.cancel_vk, Ordering::Relaxed);
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

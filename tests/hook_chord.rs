#![cfg(windows)]
//! The chord end of the hook, driven by synthetic keystrokes: `ControlLeft+ShiftLeft+Z`
//! reports one press and one release, a plain `Z` reports nothing — and the chord's trigger
//! never reaches the window while the plain one does.
//!
//! The swallow is what needs a live hook to prove, and this proves it without asking the
//! keyboard state: the test installs a `WH_KEYBOARD_LL` hook of its own *first*, so byovox's
//! — installed after it, and therefore ahead of it in the chain — either passes a keystroke
//! down to it or ends the chain. A `Z` the observer never sees is a `Z` the focused window
//! never saw either.
//!
//! `#[ignore]`d, and never run in CI, for the same reasons as the probe in
//! `platform::windows::hotkey`: it installs **global** hooks that are never unhooked and
//! leaves two message-pumping threads alive for the rest of the process, so it cannot run
//! twice in one process, and it injects **real global keystrokes**. It lives in its own test
//! binary so it gets its own process and does not collide with that probe's `INSTALLED`
//! guard. Unlike the other two it deliberately types: the plain `Z` it sends lands in
//! whatever window has focus, and is *meant* to. Focus a scratch Notepad, then run
//! `cargo test --test hook_chord -- --ignored --nocapture`.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::channel;
use std::thread::sleep;
use std::time::Duration;

use byovox::hotkey::{Hotkey, HotkeyEvent};
use byovox::platform::windows::hotkey::HookHotkey;
use windows::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_0, INPUT_KEYBOARD, KEYBD_EVENT_FLAGS, KEYBDINPUT, KEYEVENTF_KEYUP, SendInput,
    VIRTUAL_KEY,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, DispatchMessageW, GetMessageW, KBDLLHOOKSTRUCT, MSG, SetWindowsHookExW,
    TranslateMessage, WH_KEYBOARD_LL, WM_KEYDOWN,
};

const LCONTROL: u16 = 0xA2;
const LSHIFT: u16 = 0xA0;
const Z: u16 = 0x5A;

/// `Z` key-downs that got past byovox's hook. The chord's must not.
static Z_REACHED_THE_WINDOW: AtomicU32 = AtomicU32::new(0);
static OBSERVING: AtomicBool = AtomicBool::new(false);

unsafe extern "system" fn observer(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code >= 0 {
        // SAFETY: for WH_KEYBOARD_LL, lparam points at a KBDLLHOOKSTRUCT for the call's duration.
        let kb = unsafe { &*(lparam.0 as *const KBDLLHOOKSTRUCT) };
        if kb.vkCode == u32::from(Z) && wparam.0 as u32 == WM_KEYDOWN {
            Z_REACHED_THE_WINDOW.fetch_add(1, Ordering::SeqCst);
        }
    }
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

/// One key event with `dwExtraInfo` 0 — a human at the keyboard, as far as the hook knows.
fn key(vk: u16, up: bool) {
    let input = INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(vk),
                wScan: 0,
                dwFlags: if up {
                    KEYEVENTF_KEYUP
                } else {
                    KEYBD_EVENT_FLAGS(0)
                },
                time: 0,
                dwExtraInfo: 0,
            },
        },
    };
    // SAFETY: one well-formed INPUT, size declared as the API requires.
    let sent = unsafe { SendInput(&[input], size_of::<INPUT>() as i32) };
    assert_eq!(sent, 1, "SendInput refused vk {vk:#04X}");
}

#[test]
#[ignore]
fn a_chord_reports_once_and_never_reaches_the_window() {
    std::thread::spawn(|| {
        // SAFETY: standard hook installation; the hook lives until the process exits.
        let hook = unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, Some(observer), None, 0) };
        hook.expect("the observer hook must install");
        OBSERVING.store(true, Ordering::SeqCst);
        let mut msg = MSG::default();
        // SAFETY: plain message pump on this thread.
        unsafe {
            while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    });
    while !OBSERVING.load(Ordering::SeqCst) {
        sleep(Duration::from_millis(10));
    }

    let (tx, rx) = channel();
    std::thread::spawn(move || {
        Box::new(HookHotkey::new("ControlLeft+ShiftLeft+Z", "Escape").unwrap()).run(tx)
    });
    // `INSTALLED` is private to the backend, so this cannot wait on it the way the in-crate
    // probe does. The chord below is the liveness check: it has to produce events before
    // anything the observer did *not* see means a thing.
    sleep(Duration::from_millis(700));

    key(LCONTROL, false);
    key(LSHIFT, false);
    key(Z, false);
    sleep(Duration::from_millis(200)); // a hold, not a tap
    key(Z, true);
    key(LSHIFT, true);
    key(LCONTROL, true);

    let mut got = Vec::new();
    while let Ok(ev) = rx.recv_timeout(Duration::from_millis(700)) {
        got.push(ev);
    }
    println!("ControlLeft+ShiftLeft+Z -> {got:?}");
    assert_eq!(
        got,
        vec![HotkeyEvent::Pressed, HotkeyEvent::Released],
        "the hook was not live, so this test proves nothing"
    );
    assert_eq!(
        Z_REACHED_THE_WINDOW.load(Ordering::SeqCst),
        0,
        "the chord's trigger reached the window"
    );

    key(Z, false);
    key(Z, true);
    let mut after = Vec::new();
    while let Ok(ev) = rx.recv_timeout(Duration::from_millis(700)) {
        after.push(ev);
    }
    println!(
        "plain Z -> {after:?}, seen past the hook: {}",
        Z_REACHED_THE_WINDOW.load(Ordering::SeqCst)
    );
    assert!(after.is_empty(), "a plain Z started a dictation");
    assert_eq!(
        Z_REACHED_THE_WINDOW.load(Ordering::SeqCst),
        1,
        "a plain Z must type"
    );
}

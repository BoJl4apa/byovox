#![cfg(windows)]
//! The other half of the inject marker: `platform::windows::inject` stamps `INJECT_MARKER`
//! into every event it sends, and this asserts the hook actually skips those — that is, that
//! `SendInput`'s `dwExtraInfo` reaches `KBDLLHOOKSTRUCT.dwExtraInfo`, which is the OS
//! guarantee the whole no-feedback design rests on. Without it a paste chord's Ctrl-down
//! would read as a hotkey press and every paste would look like a new recording.
//!
//! `#[ignore]`d, and never run in CI, for the same reasons as the hook probe in
//! `platform::windows::hotkey`: it installs a **global** hook that is never unhooked and
//! leaves a message-pumping thread alive for the rest of the process, so it cannot run twice
//! in one process, and it injects **real global keystrokes**. It lives in its own test
//! binary so it gets its own process and does not collide with that probe's `INSTALLED`
//! guard. The only key it injects is a bare Right Ctrl, which does nothing to the focused
//! window — deliberately not text and not the Ctrl+V chord, either of which would land in
//! whatever window has focus. Run it on a desktop session with
//! `cargo test --test hook_marker -- --ignored`.

use std::sync::mpsc::{RecvTimeoutError, channel};
use std::thread::sleep;
use std::time::Duration;

use byovox::hotkey::{Hotkey, HotkeyEvent};
use byovox::platform::windows::hotkey::{HookHotkey, INJECT_MARKER};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, SendInput,
    VIRTUAL_KEY,
};

const RCONTROL: u16 = 0xA3;

/// Right Ctrl down then up, stamped with `extra` — 0 stands in for a human at the keyboard.
fn tap_right_ctrl(extra: usize) {
    for flags in [
        KEYEVENTF_EXTENDEDKEY,
        KEYEVENTF_EXTENDEDKEY | KEYEVENTF_KEYUP,
    ] {
        let input = INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: VIRTUAL_KEY(RCONTROL),
                    wScan: 0,
                    dwFlags: flags,
                    time: 0,
                    dwExtraInfo: extra,
                },
            },
        };
        // SAFETY: one well-formed INPUT, size declared as the API requires.
        let sent = unsafe { SendInput(&[input], size_of::<INPUT>() as i32) };
        assert_eq!(sent, 1, "SendInput refused Right Ctrl");
    }
}

#[test]
#[ignore]
fn events_stamped_with_the_inject_marker_are_invisible_to_the_hotkey() {
    let (tx, rx) = channel();
    std::thread::spawn(move || {
        Box::new(HookHotkey::new("ControlRight", "Escape").unwrap()).run(tx)
    });
    // `INSTALLED` is private to the backend, so an integration test cannot wait on it the
    // way the in-crate probe does. The unmarked tap below is the real liveness check: it
    // has to produce events before the silence that follows means anything.
    sleep(Duration::from_millis(700));

    tap_right_ctrl(0);
    let mut got = Vec::new();
    while let Ok(ev) = rx.recv_timeout(Duration::from_millis(700)) {
        got.push(ev);
    }
    println!("unmarked Right Ctrl -> {got:?}");
    assert_eq!(
        got,
        vec![HotkeyEvent::Pressed, HotkeyEvent::Released],
        "the hook was not live, so this test proves nothing"
    );

    tap_right_ctrl(INJECT_MARKER);
    match rx.recv_timeout(Duration::from_millis(700)) {
        Err(RecvTimeoutError::Timeout) => println!("marked Right Ctrl -> no event"),
        Err(RecvTimeoutError::Disconnected) => panic!("the hook thread exited"),
        Ok(ev) => panic!("a marked event reached the hotkey as {ev:?}"),
    }
}

//! Keyboard layout of the foreground window via GetKeyboardLayout — per-window, which is
//! exactly what Windows' "let me set a different input method for each app window" sets.

use windows::Win32::UI::Input::KeyboardAndMouse::GetKeyboardLayout;
use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId};

use crate::lang::Lang;
use crate::layout::Layout;

pub struct WinLayout;

impl Layout for WinLayout {
    fn current(&self) -> Option<Lang> {
        // SAFETY: all three calls accept any state; a null foreground window yields tid 0,
        // for which GetKeyboardLayout returns the calling thread's layout.
        let langid = unsafe {
            let hwnd = GetForegroundWindow();
            let tid = GetWindowThreadProcessId(hwnd, None);
            let hkl = GetKeyboardLayout(tid);
            (hkl.0 as usize & 0xFFFF) as u16
        };
        lang_from_langid(langid)
    }
}

/// LANGID → ISO 639-1 by primary language id (low 10 bits).
pub fn lang_from_langid(langid: u16) -> Option<Lang> {
    let code = match langid & 0x3FF {
        0x01 => "ar",
        0x04 => "zh",
        0x05 => "cs",
        0x06 => "da",
        0x07 => "de",
        0x08 => "el",
        0x09 => "en",
        0x0A => "es",
        0x0B => "fi",
        0x0C => "fr",
        0x0D => "he",
        0x0E => "hu",
        0x10 => "it",
        0x11 => "ja",
        0x12 => "ko",
        0x13 => "nl",
        0x14 => "no",
        0x15 => "pl",
        0x16 => "pt",
        0x18 => "ro",
        0x19 => "ru",
        0x1A => "hr",
        0x1B => "sk",
        0x1D => "sv",
        0x1E => "th",
        0x1F => "tr",
        0x22 => "uk",
        0x25 => "et",
        0x26 => "lv",
        0x27 => "lt",
        0x2A => "vi",
        0x39 => "hi",
        0x3E => "ms",
        _ => return None,
    };
    Lang::parse(code)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn primary_language_ids_normalise() {
        let l = |s| Lang::parse(s).unwrap();
        assert_eq!(lang_from_langid(0x0409), Some(l("en"))); // en-US
        assert_eq!(lang_from_langid(0x0809), Some(l("en"))); // en-GB: same primary id
        assert_eq!(lang_from_langid(0x0419), Some(l("ru")));
        assert_eq!(lang_from_langid(0x040D), Some(l("he")));
        assert_eq!(lang_from_langid(0x0407), Some(l("de")));
        assert_eq!(lang_from_langid(0x0000), None);
    }

    /// The one executable check on the FFI half: it reads the layout of whatever window
    /// happens to be focused, so its result is the machine's state, not a fixed value —
    /// hence `#[ignore]`d and never asserted on. It also prints every installed layout's
    /// LANGID through the table, which is the closest thing to the manual "switch layouts
    /// in Notepad" probe that a session with nobody at the keyboard can run.
    #[test]
    #[ignore]
    fn reads_the_foreground_layout() {
        use windows::Win32::UI::Input::KeyboardAndMouse::{GetKeyboardLayoutList, HKL};

        let show = |l: Option<Lang>| l.map_or("None".to_string(), |l| l.to_string());
        println!("foreground layout: {}", show(WinLayout.current()));

        // SAFETY: the count call passes no buffer; the second passes a slice whose length
        // the wrapper hands to `nbuff`, so the API cannot write past it.
        let installed = unsafe {
            let n = GetKeyboardLayoutList(None);
            let mut list = vec![HKL::default(); n.max(0) as usize];
            let got = GetKeyboardLayoutList(Some(&mut list));
            list.truncate(got.max(0) as usize);
            list
        };
        for hkl in installed {
            let langid = (hkl.0 as usize & 0xFFFF) as u16;
            println!(
                "installed {langid:#06X} -> {}",
                show(lang_from_langid(langid))
            );
        }
    }
}

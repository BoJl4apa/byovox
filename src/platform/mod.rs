//! Platform backends and the runtime selection between them. One rung at a time:
//! `detect` records what it chose so `check` and the log can say so.

use anyhow::Result;

use crate::capture::Capture;
use crate::config::Config;
use crate::hotkey::{Hotkey, validate_key_name};
use crate::inject::{Inject, InjectMode};
use crate::layout::Layout;

#[cfg(windows)]
pub mod windows;

pub struct BackendNames {
    pub hotkey: &'static str,
    pub layout: &'static str,
    pub rungs: Vec<&'static str>,
}

pub struct Backends {
    pub hotkey: Box<dyn Hotkey>,
    pub capture: Box<dyn Capture>,
    pub layout: Box<dyn Layout>,
    pub rungs: Vec<Box<dyn Inject>>,
    pub names: BackendNames,
}

pub fn detect(cfg: &Config) -> Result<Backends> {
    validate_key_name(&cfg.hotkey.key).map_err(|e| anyhow::anyhow!("hotkey.key: {e}"))?;
    validate_key_name(&cfg.hotkey.cancel_key)
        .map_err(|e| anyhow::anyhow!("hotkey.cancel_key: {e}"))?;
    let mode = InjectMode::parse(&cfg.inject.mode).ok_or_else(|| {
        anyhow::anyhow!(
            "inject.mode `{}`: expected auto | type | paste | clipboard-only",
            cfg.inject.mode
        )
    })?;
    platform_detect(cfg, mode)
}

/// The rungs a mode selects, in the order the pipeline tries them. Owns no resource: all
/// three are unit structs, so this is testable without a device.
#[cfg(windows)]
fn rungs_for(mode: InjectMode) -> Vec<Box<dyn Inject>> {
    use windows::inject::{ClipboardOnlyInject, PasteInject, TypeInject};

    match mode {
        InjectMode::Auto => vec![
            Box::new(TypeInject),
            Box::new(PasteInject),
            Box::new(ClipboardOnlyInject),
        ],
        InjectMode::Type => vec![Box::new(TypeInject)],
        InjectMode::Paste => vec![Box::new(PasteInject)],
        InjectMode::ClipboardOnly => vec![Box::new(ClipboardOnlyInject)],
    }
}

#[cfg(windows)]
fn platform_detect(cfg: &Config, mode: InjectMode) -> Result<Backends> {
    use windows::hotkey::HookHotkey;
    use windows::layout::WinLayout;

    let hotkey =
        HookHotkey::new(&cfg.hotkey.key, &cfg.hotkey.cancel_key).map_err(anyhow::Error::msg)?;
    let capture = crate::capture::CpalCapture::open_default().map_err(anyhow::Error::msg)?;
    let rungs = rungs_for(mode);
    let names = BackendNames {
        hotkey: "hook",
        layout: "win32",
        rungs: rungs.iter().map(|r| r.name()).collect(),
    };
    Ok(Backends {
        hotkey: Box::new(hotkey),
        capture: Box::new(capture),
        layout: Box::new(WinLayout),
        rungs,
        names,
    })
}

#[cfg(not(windows))]
fn platform_detect(_cfg: &Config, _mode: InjectMode) -> Result<Backends> {
    anyhow::bail!(
        "no backends for this platform yet — Linux (KDE) lands in the next plan, macOS after"
    )
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use crate::config::Config;

    /// The rejection message. `Backends` holds trait objects and so has no `Debug` for
    /// `unwrap_err` to print; naming the rungs it wrongly chose says more anyway.
    fn rejection(cfg: &Config) -> String {
        match detect(cfg) {
            Err(e) => e.to_string(),
            Ok(b) => panic!("expected a rejection, got rungs {:?}", b.names.rungs),
        }
    }

    /// `#[ignore]`d because `detect` opens the default microphone: on a box with no input
    /// device (a CI runner) it fails at `CpalCapture::open_default`, not at anything this
    /// test is about. Run it on a desktop with `cargo test -- --ignored`.
    #[test]
    #[ignore]
    fn default_config_selects_hook_win32_and_all_rungs() {
        let b = detect(&Config::default()).unwrap();
        assert_eq!(b.names.hotkey, "hook");
        assert_eq!(b.names.layout, "win32");
        assert_eq!(b.names.rungs, vec!["type", "paste", "clipboard-only"]);
    }

    /// `#[ignore]`d for the same reason: pinning a mode still reaches the microphone.
    #[test]
    #[ignore]
    fn pinned_mode_selects_one_rung_and_bad_mode_errors() {
        let mut c = Config::default();
        c.inject.mode = "paste".into();
        assert_eq!(detect(&c).unwrap().names.rungs, vec!["paste"]);
        c.inject.mode = "telepathy".into();
        assert!(detect(&c).is_err());
        c.inject.mode = "auto".into();
        c.hotkey.key = "Nope".into();
        assert!(rejection(&c).contains("Nope"));
    }

    /// The selection itself, with no device in the way: the whole point of `rungs_for` being
    /// separate. Names come from `Inject::name()`, so a rung renamed in its own module shows
    /// up here rather than drifting from a literal list kept in parallel.
    #[test]
    fn every_mode_selects_its_rungs_in_order() {
        let names = |m: InjectMode| -> Vec<&'static str> {
            rungs_for(m).iter().map(|r| r.name()).collect()
        };
        assert_eq!(
            names(InjectMode::Auto),
            vec!["type", "paste", "clipboard-only"]
        );
        assert_eq!(names(InjectMode::Type), vec!["type"]);
        assert_eq!(names(InjectMode::Paste), vec!["paste"]);
        assert_eq!(names(InjectMode::ClipboardOnly), vec!["clipboard-only"]);
    }

    /// The rejections `detect` makes before it touches hardware — the same assertions the
    /// two tests above make, minus the microphone, so they still run on a CI runner. Each
    /// names the config field at fault, so the user knows which line to edit.
    #[test]
    fn bad_mode_or_key_is_rejected_before_any_device_is_opened() {
        let mut c = Config::default();
        c.inject.mode = "telepathy".into();
        let e = rejection(&c);
        assert!(e.starts_with("inject.mode `telepathy`"), "{e}");

        let mut c = Config::default();
        c.hotkey.key = "Nope".into();
        let e = rejection(&c);
        assert!(e.starts_with("hotkey.key: "), "{e}");
        assert!(e.contains("Nope"), "{e}");

        let mut c = Config::default();
        c.hotkey.cancel_key = "Nope".into();
        let e = rejection(&c);
        assert!(e.starts_with("hotkey.cancel_key: "), "{e}");
        assert!(e.contains("Nope"), "{e}");
    }

    /// The target branch of the hook wins, so a cancel key equal to the hotkey could never
    /// fire; `detect` must refuse at startup rather than ship a dead cancel.
    #[test]
    fn same_key_for_hotkey_and_cancel_is_rejected() {
        let mut c = Config::default();
        c.hotkey.key = "Escape".into();
        c.hotkey.cancel_key = "Escape".into();
        let e = rejection(&c);
        assert!(e.contains("Escape") && e.contains("differ"), "{e}");
    }
}

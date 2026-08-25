//! Per-user autostart via HKCU\...\Run — no elevation, no installer.

use std::path::Path;

use anyhow::{Context, Result};
use winreg::RegKey;
use winreg::enums::HKEY_CURRENT_USER;

const RUN: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";

/// `config` is carried into the registered command line. `--config` is a global flag, so a
/// user who registers autostart while pointing at a non-default file expects the autostarted
/// daemon to read that file rather than silently falling back to the platform default.
pub fn enable(config: Option<&Path>) -> Result<()> {
    let exe = std::env::current_exe().context("current exe")?;
    let command = match config {
        Some(c) => format!("\"{}\" --config \"{}\"", exe.display(), c.display()),
        None => format!("\"{}\"", exe.display()),
    };
    let (key, _) = RegKey::predef(HKEY_CURRENT_USER).create_subkey(RUN)?;
    key.set_value("byovox", &command)?;
    Ok(())
}

pub fn disable() -> Result<()> {
    let key =
        RegKey::predef(HKEY_CURRENT_USER).open_subkey_with_flags(RUN, winreg::enums::KEY_WRITE)?;
    match key.delete_value("byovox") {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `#[ignore]`d: it writes the **real** `HKCU\...\Run` key of the user running it. Any
    /// `byovox` value already there is read first and put back at the end, so a box with
    /// autostart configured is left as it was found. Run with `cargo test -- --ignored`.
    #[test]
    #[ignore]
    fn autostart_round_trips_the_run_key() {
        let (key, _) = RegKey::predef(HKEY_CURRENT_USER)
            .create_subkey(RUN)
            .expect("open Run");
        let original: Option<String> = key.get_value("byovox").ok();
        println!("original Run\\byovox: {original:?}");

        enable(None).expect("enable");
        let value: String = key.get_value("byovox").expect("byovox value");
        println!("after enable: {value}");
        let exe = std::env::current_exe().expect("current exe");
        assert_eq!(value, format!("\"{}\"", exe.display()));

        let cfg = Path::new(r"C:\somewhere\byovox.toml");
        enable(Some(cfg)).expect("enable with a config");
        let value: String = key.get_value("byovox").expect("byovox value");
        println!("after enable --config: {value}");
        assert_eq!(
            value,
            format!("\"{}\" --config \"{}\"", exe.display(), cfg.display())
        );

        disable().expect("disable");
        let after = key.get_value::<String, _>("byovox");
        println!("after disable: {after:?}");
        assert!(after.is_err(), "the value survived disable(): {after:?}");

        disable().expect("disable on a missing value is a no-op");
        println!("second disable: ok");

        if let Some(original) = original {
            key.set_value("byovox", &original).expect("restore");
            let back: String = key.get_value("byovox").expect("restored value");
            assert_eq!(back, original);
            println!("restored: {back}");
        }
    }
}

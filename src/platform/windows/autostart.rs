//! Per-user autostart via HKCU\...\Run — no elevation, no installer.

use anyhow::{Context, Result};
use winreg::RegKey;
use winreg::enums::HKEY_CURRENT_USER;

const RUN: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";

pub fn enable() -> Result<()> {
    let exe = std::env::current_exe().context("current exe")?;
    let (key, _) = RegKey::predef(HKEY_CURRENT_USER).create_subkey(RUN)?;
    key.set_value("byovox", &format!("\"{}\"", exe.display()))?;
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

    /// `#[ignore]`d: it writes the **real** `HKCU\...\Run` key of the user running it, and
    /// leaves the `byovox` value deleted (the disabled state) — including one that was
    /// already there. Run with `cargo test -- --ignored`.
    #[test]
    #[ignore]
    fn autostart_round_trips_the_run_key() {
        enable().expect("enable");
        let key = RegKey::predef(HKEY_CURRENT_USER)
            .open_subkey(RUN)
            .expect("open Run");
        let value: String = key.get_value("byovox").expect("byovox value");
        println!("Run\\byovox = {value}");
        let exe = std::env::current_exe().expect("current exe");
        assert_eq!(value, format!("\"{}\"", exe.display()));

        disable().expect("disable");
        let after = key.get_value::<String, _>("byovox");
        println!("after disable: {after:?}");
        assert!(after.is_err(), "the value survived disable(): {after:?}");

        disable().expect("disable on a missing value is a no-op");
        println!("second disable: ok");
    }
}

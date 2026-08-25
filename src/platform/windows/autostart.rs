//! Per-user autostart via HKCU\...\Run — no elevation, no installer.

use std::path::Path;

use anyhow::{Context, Result};
use winreg::RegKey;
use winreg::enums::{HKEY_CURRENT_USER, KEY_WRITE};

const RUN: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";

/// The command line to register, for the executable `exe` names.
///
/// `config` is carried through because `--config` is a global flag: a user who registers
/// autostart while pointing at a non-default file expects the autostarted daemon to read that
/// file, not to fall back to the platform default. It is absolutised first — a `Run` value is
/// executed with an unrelated working directory, so a relative path registered from a shell
/// would resolve somewhere else at login and start the daemon on a different config, or none.
fn command_line(exe: &Path, config: Option<&Path>) -> Result<String> {
    let Some(c) = config else {
        return Ok(format!("\"{}\"", exe.display()));
    };
    let absolute =
        std::path::absolute(c).with_context(|| format!("resolving --config {}", c.display()))?;
    Ok(format!(
        "\"{}\" --config \"{}\"",
        exe.display(),
        absolute.display()
    ))
}

/// What gets registered is `byovox-daemon`, never the CLI doing the registering: the CLI is
/// console-subsystem, so the Run key would flash a console window at every logon.
pub fn enable(config: Option<&Path>) -> Result<()> {
    let exe = std::env::current_exe().context("current exe")?;
    let daemon = crate::daemon::daemon_exe(&exe);
    // A Run value is only ever executed at the next logon, where a wrong path fails silently
    // and goes on failing. `byovox` refuses to spawn a daemon that is not beside it;
    // registering one refuses for the same reason, while someone is still reading the output.
    if !daemon.exists() {
        anyhow::bail!(
            "{} not found — autostart would fail at every logon",
            daemon.display()
        );
    }
    let command = command_line(&daemon, config)?;
    // Registry errors say only "The system cannot find the file specified", so every call
    // carries the key it was talking about.
    let (key, _) = RegKey::predef(HKEY_CURRENT_USER)
        .create_subkey(RUN)
        .with_context(|| format!(r"HKCU\{RUN}"))?;
    key.set_value("byovox", &command)
        .with_context(|| format!(r"HKCU\{RUN}\byovox"))?;
    Ok(())
}

pub fn disable() -> Result<()> {
    let key = match RegKey::predef(HKEY_CURRENT_USER).open_subkey_with_flags(RUN, KEY_WRITE) {
        Ok(key) => key,
        // No Run key at all is nothing to unregister — the same answer as no value under it.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e).with_context(|| format!(r"HKCU\{RUN}")),
    };
    match key.delete_value("byovox") {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!(r"HKCU\{RUN}\byovox")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `Run` value runs with an unrelated working directory, so a relative `--config` would
    /// resolve to a different file at login — or to none, and the daemon would come up on the
    /// defaults without a word. Pure: it never touches the registry.
    ///
    /// It also pins what gets registered. The CLI is console-subsystem: registering it would
    /// flash a console window at every logon, which is the whole reason the daemon is its own
    /// binary.
    #[test]
    fn a_relative_config_is_absolutised_before_it_is_registered() {
        let exe = crate::daemon::daemon_exe(Path::new(r"C:\bin\byovox.exe"));
        assert_eq!(
            command_line(&exe, None).unwrap(),
            r#""C:\bin\byovox-daemon.exe""#
        );

        let relative = command_line(&exe, Some(Path::new("byovox.toml"))).unwrap();
        assert!(
            !relative.contains(r#"--config "byovox.toml""#),
            "still relative: {relative}"
        );
        let resolved = std::path::absolute("byovox.toml").unwrap();
        assert!(
            relative.ends_with(&format!("--config \"{}\"", resolved.display())),
            "{relative}"
        );

        // An absolute path is already what it will mean at login, so it is passed through.
        let absolute = command_line(&exe, Some(Path::new(r"C:\somewhere\byovox.toml"))).unwrap();
        assert_eq!(
            absolute,
            r#""C:\bin\byovox-daemon.exe" --config "C:\somewhere\byovox.toml""#
        );
    }

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
        let exe = crate::daemon::daemon_exe(&std::env::current_exe().expect("current exe"));
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

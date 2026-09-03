//! `byovox hotkey`: show the push-to-talk binding, list the names it accepts, and change it.
//!
//! The binding has always been configurable — `hotkey.key` in the config file, and a question
//! in `byovox setup` — but both of those are all-or-nothing: the wizard rewrites the whole
//! file and the file needs an editor. This is the one-line path, and the only place that says
//! out loud whether anything else on the machine already answers to the key.
//!
//! Depends on `platform::hotkey_availability` for that answer and on `setup::set_or_add_key`
//! to edit the file in place, comments and all. Produces a config file and nothing else: a
//! running daemon read its `[hotkey]` at startup and has to be restarted to see this.

use std::path::Path;

use anyhow::{Context, Result, bail};

use crate::config::{self, Config};
use crate::hotkey::{HotkeyMode, KEY_NAMES, parse_chord};
use crate::platform::{self, Availability};
use crate::setup::{set_or_add_key, toml_string};

/// What one invocation asks to change. All `None` is `byovox hotkey` with no flags, which
/// reports and writes nothing.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Change {
    pub key: Option<String>,
    pub mode: Option<String>,
    pub cancel: Option<String>,
}

impl Change {
    pub fn is_empty(&self) -> bool {
        *self == Change::default()
    }
}

/// The file's text and the config it parses to, without the endpoint checks `config::load`
/// makes.
///
/// Deliberately not `config::load`: that one refuses a config whose `stt.base_url` is still
/// empty, and the binding is worth setting before there is a server to point it at — a fresh
/// `byovox config --init` file is exactly that case. A missing file starts from the documented
/// example, so the first `--set` writes a config with every other key explained in it.
fn read(path: &Path) -> Result<(String, Config)> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => config::EXAMPLE.to_string(),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let cfg = toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
    Ok((text, cfg))
}

/// Every name accepted in a binding, and the two shapes a binding can take.
pub fn list() {
    println!("Keys byovox can bind on their own, or use as a chord's modifiers:\n");
    for row in KEY_NAMES.chunks(4) {
        println!("  {}", row.join("  "));
    }
    println!("\nA chord may also end on A-Z or 0-9 — only as the trigger, never alone:");
    println!("  byovox hotkey --set ControlLeft+ShiftLeft+Z");
    println!("\nOne key on its own is the simplest binding, and the default:");
    println!("  byovox hotkey --set ControlRight");
    println!("\nF13-F24 are the keys nothing else uses: no physical keyboard sends them, so a");
    println!("macro key or a remapper set to one collides with nothing.");
}

/// The `hotkey.key` line as `byovox check` would print it, plus what the platform says about
/// the binding being free.
pub fn show(path: &Path) -> Result<()> {
    let (_, cfg) = read(path)?;
    let h = &cfg.hotkey;
    println!("config     {}", path.display());
    println!("key        {}", h.key);
    println!(
        "mode       {}{}",
        h.mode,
        match HotkeyMode::parse(&h.mode) {
            Some(HotkeyMode::Hold) => "  (hold to talk, release to send)",
            Some(HotkeyMode::Toggle) => "  (press to start, press again to send)",
            None => "  (not a mode: expected hold | toggle)",
        }
    );
    println!("min hold   {} ms", h.min_hold_ms);
    println!(
        "cancel     {}  (discards a dictation in progress)",
        h.cancel_key
    );
    match parse_chord(&h.key) {
        Ok(chord) => report(&chord),
        Err(e) => println!("\nhotkey.key is not valid: {e}"),
    }
    Ok(())
}

/// The availability line, in the one wording `show` and `set` both use.
fn report(chord: &crate::hotkey::Chord) {
    match platform::hotkey_availability(chord) {
        Availability::Free => println!("\nfree       no other application has registered {chord}"),
        Availability::Taken => {
            println!("\nTAKEN      another application has registered {chord} system-wide.");
            println!("           byovox would still fire — it reads the keyboard through a hook,");
            println!("           which runs first — but so would the other application, on every");
            println!("           press. Pick another key, or close whatever holds this one.");
        }
        Availability::Unknown(why) => println!("\n           not checked: {why}"),
    }
}

/// Whether the platform reports this chord as somebody else's, phrased as the refusal `set`
/// makes. `Ok(())` covers "free" and "could not tell": neither is a reason to stop.
fn refuse_if_taken(chord: &crate::hotkey::Chord) -> Result<()> {
    if let Availability::Taken = platform::hotkey_availability(chord) {
        bail!(
            "{chord} is already registered by another application — both it and byovox would \
             fire on every press. Choose another key, or pass --force to bind it anyway"
        );
    }
    Ok(())
}

/// Apply `change` to the file at `path`.
///
/// Every value is checked before anything is written, and checked the way the daemon will
/// check it: `platform::validate` is the function `byovox` itself refuses to start on, so a
/// binding accepted here is one the daemon accepts. That is also what catches the pairs a
/// single key cannot — a cancel key that is part of the chord, which is only wrong in
/// combination.
pub fn set(path: &Path, change: &Change, force: bool) -> Result<()> {
    if change.is_empty() {
        bail!("nothing to change — pass --set, --mode or --cancel, or `byovox hotkey` to show");
    }
    let (text, mut cfg) = read(path)?;
    let before = cfg.hotkey.clone();
    if let Some(key) = &change.key {
        cfg.hotkey.key = key.clone();
    }
    if let Some(mode) = &change.mode {
        if HotkeyMode::parse(mode).is_none() {
            bail!("hotkey.mode `{mode}`: expected hold | toggle");
        }
        cfg.hotkey.mode = mode.clone();
    }
    if let Some(cancel) = &change.cancel {
        cfg.hotkey.cancel_key = cancel.clone();
    }
    // The daemon's own refusals, made here on the console the change was typed into.
    platform::validate(&cfg)?;

    let chord = parse_chord(&cfg.hotkey.key).map_err(|e| anyhow::anyhow!("hotkey.key: {e}"))?;
    // Only when the key itself moved: re-running `--mode toggle` must not fail on a conflict
    // the user already accepted, and a binding byovox is currently listening on is not one
    // this probe can say anything useful about anyway.
    if change.key.is_some() && !force {
        refuse_if_taken(&chord)?;
    }

    let mut out = text;
    for (key, value) in [
        ("key", &cfg.hotkey.key),
        ("mode", &cfg.hotkey.mode),
        ("cancel_key", &cfg.hotkey.cancel_key),
    ] {
        out = set_or_add_key(&out, "hotkey", key, &toml_string(value));
    }
    // Parsed back before it lands, so a bad edit says so instead of leaving a file the daemon
    // will refuse at its next start.
    let round_trip: Config =
        toml::from_str(&out).context("the edit produced a file that is not valid TOML")?;
    if round_trip.hotkey != cfg.hotkey {
        bail!("the edit did not take — {} was left alone", path.display());
    }
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    std::fs::write(path, &out).with_context(|| format!("writing {}", path.display()))?;

    for (what, was, now) in [
        ("key", &before.key, &cfg.hotkey.key),
        ("mode", &before.mode, &cfg.hotkey.mode),
        ("cancel", &before.cancel_key, &cfg.hotkey.cancel_key),
    ] {
        if was != now {
            println!("{what}: {was} -> {now}");
        }
    }
    println!("wrote {}", path.display());
    if change.key.is_some() && force {
        report(&chord);
    }
    if crate::ipc::daemon_running(&crate::ipc::socket_name()) {
        println!("\nthe daemon is running and read its hotkey at startup:");
        println!("  byovox quit; byovox      # to pick this up");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::EXAMPLE;

    fn dir(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("byovox-hotkey-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// The point of the command: one line changes the binding, and the file is still the
    /// documented one afterwards.
    #[test]
    fn setting_the_key_edits_the_file_in_place_and_keeps_every_comment() {
        let path = dir("in-place").join("config.toml");
        std::fs::write(&path, EXAMPLE).unwrap();
        let change = Change {
            key: Some("F13".into()),
            ..Default::default()
        };
        set(&path, &change, true).unwrap();
        let out = std::fs::read_to_string(&path).unwrap();
        let cfg: Config = toml::from_str(&out).unwrap();
        assert_eq!(cfg.hotkey.key, "F13");
        assert_eq!(cfg.hotkey.mode, "hold", "an unasked key is untouched");
        assert!(out.contains("W3C UI Events"), "the paragraph survives");
        assert_eq!(
            EXAMPLE.lines().count(),
            out.lines().count(),
            "an in-place edit adds and removes no lines"
        );
    }

    /// A config with no `[hotkey]` paragraph at all is a legal one — every key is optional —
    /// so the command has to be able to add the section rather than refuse the file.
    #[test]
    fn a_file_without_the_section_gets_one() {
        let path = dir("no-section").join("config.toml");
        std::fs::write(&path, "[stt]\nbase_url = \"http://127.0.0.1:8770/v1\"\n").unwrap();
        let change = Change {
            key: Some("Insert".into()),
            mode: Some("toggle".into()),
            ..Default::default()
        };
        set(&path, &change, true).unwrap();
        let cfg: Config = toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(cfg.hotkey.key, "Insert");
        assert_eq!(cfg.hotkey.mode, "toggle");
        assert_eq!(cfg.stt.base_url, "http://127.0.0.1:8770/v1", "kept");
    }

    /// Every refusal happens before the write, so a rejected value cannot leave the file
    /// half-changed.
    #[test]
    fn a_binding_the_daemon_would_refuse_is_refused_here_and_nothing_is_written() {
        let path = dir("refusals").join("config.toml");
        std::fs::write(&path, EXAMPLE).unwrap();

        // A bare letter would swallow every `Z` typed on the desktop.
        let bad_key = Change {
            key: Some("Z".into()),
            ..Default::default()
        };
        assert!(set(&path, &bad_key, true).is_err());

        let bad_mode = Change {
            mode: Some("push".into()),
            ..Default::default()
        };
        let e = set(&path, &bad_mode, true).unwrap_err();
        assert!(e.to_string().contains("hold | toggle"), "{e}");

        // Only wrong in combination: `Escape` is a fine cancel key until the hotkey is it.
        let same = Change {
            key: Some("Escape".into()),
            ..Default::default()
        };
        assert!(set(&path, &same, true).is_err());

        assert!(
            set(&path, &Change::default(), true).is_err(),
            "nothing asked"
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            EXAMPLE,
            "untouched"
        );
    }

    /// A chord's trigger is swallowed, so the cancel key may not be inside it.
    #[test]
    fn a_cancel_key_inside_the_chord_is_refused() {
        let path = dir("cancel").join("config.toml");
        std::fs::write(&path, EXAMPLE).unwrap();
        let change = Change {
            key: Some("ControlLeft+ShiftLeft+Z".into()),
            cancel: Some("ShiftLeft".into()),
            ..Default::default()
        };
        assert!(set(&path, &change, true).is_err());
    }
}
